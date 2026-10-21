"""MCP stdio client: one JSON message per line, responses matched by id.

A reader thread stamps every response with the injected clock, so latency is
`received_at - sent_at` on the same clock the tests control. A call that
outlives its timeout is abandoned with `notifications/cancelled`.
"""

from __future__ import annotations

import itertools
import json
import subprocess
import threading

PROTOCOL_VERSION = "2025-06-18"


class McpClient:
    def __init__(self, cmd, cwd, env, stderr, clock):
        self.clock = clock
        self.cond = threading.Condition()
        self.responses: dict = {}
        self.notifications: list = []
        self.protocol_errors: list = []
        self.closed = False
        self._ids = itertools.count(1)
        self._send_lock = threading.Lock()
        self.proc = subprocess.Popen(cmd, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=stderr)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self) -> None:
        try:
            for line in self.proc.stdout:
                if not line.strip():
                    continue
                try:
                    message = json.loads(line)
                except json.JSONDecodeError as exc:
                    self.protocol_errors.append(f"malformed JSON line: {exc}")
                    continue
                received_at = self.clock.now()
                if "id" in message and "method" in message:
                    self._answer_server_request(message)
                    continue
                with self.cond:
                    if "id" in message:
                        self.responses[message["id"]] = (message, received_at)
                    else:
                        self.notifications.append(message)
                    self.cond.notify_all()
        finally:
            with self.cond:
                self.closed = True
                self.cond.notify_all()

    def _answer_server_request(self, message: dict) -> None:
        if message["method"] == "ping":
            self._send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
        else:
            self._send({"jsonrpc": "2.0", "id": message["id"],
                        "error": {"code": -32601, "message": "unsupported by the harness"}})

    def _send(self, message: dict) -> None:
        data = (json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8")
        with self._send_lock:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()

    def notify(self, method: str, params=None) -> None:
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._send(message)

    def request(self, method: str, params, timeout: float):
        """(elapsed_s, result, error). `error` is "timeout", "closed" or the
        JSON-RPC error text; on "closed" the server is gone."""
        request_id = next(self._ids)
        sent_at = self.clock.now()
        try:
            self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        except OSError:
            return None, None, "closed"
        arrived = self.clock.wait(
            lambda: request_id in self.responses or self.closed, timeout, self.cond,
            key=("mcp", self, request_id))
        with self.cond:
            entry = self.responses.pop(request_id, None)
        if not arrived:
            # An answer that slipped in after the deadline is still a timeout.
            try:
                self.notify("notifications/cancelled", {"requestId": request_id, "reason": "timeout"})
            except OSError:
                pass
            # A malformed line may be the answer that never arrived; say so.
            if self.protocol_errors:
                return None, None, f"timeout; protocol error: {self.protocol_errors[-1]}"
            return None, None, "timeout"
        if entry is None:
            return None, None, "closed"
        message, received_at = entry
        elapsed = received_at - sent_at
        if "error" in message:
            error = message["error"] or {}
            return elapsed, None, f"{error.get('code')}: {error.get('message')}"
        return elapsed, message.get("result"), None

    def initialize(self, timeout: float):
        response = self.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "perf-nightly", "version": "1"},
        }, timeout)
        if response[2] is None:
            self.notify("notifications/initialized")
        return response

    def call_tool(self, name: str, arguments: dict, timeout: float):
        return self.request("tools/call", {"name": name, "arguments": arguments}, timeout)

    def close_stdin(self) -> None:
        try:
            self.proc.stdin.close()
        except OSError:
            pass

    def release(self) -> None:
        """Close stdout once every writer is gone: the reader stops on EOF first,
        so the pipe is never closed under a read in flight."""
        self._reader.join()
        self.proc.stdout.close()
