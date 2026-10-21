"""The six scenarios of plan §8: `analyze`, `lsp-session`, `lsp-workspace-diag`,
`mcp-start`, `mcp-memory`, `mcp-load`.

Each scenario gets a `Ctx` and returns the run's outcome: `held`, `reason`,
`metrics`, `calls`, `errors`, `exit_code`. Every wait ends on an event or on a
timeout from plan D15; the clock is injected so tests decide timeouts.
"""

from __future__ import annotations

import json
import math
import os
import pathlib
import re
import subprocess
import sys
import threading
import tomllib
import urllib.parse
from dataclasses import dataclass, field

import procs
from common import HarnessError, median
from corpus import CONFIG_NAME
from mcp_client import McpClient

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lsp-bench"))
from lsp_client import LspClient  # noqa: E402

CALL_TIMEOUT_S = 120.0
PUBLISH_TIMEOUT_S = 60.0
READY_TIMEOUT_S = 1200.0
ANALYZE_TIMEOUT_S = 1800.0
SHUTDOWN_TIMEOUT_S = 10.0
LSP_POLL_S = 0.5
MCP_POLL_S = 1.0
MCP_MEMORY_ROUNDS = 10
MCP_LOAD_ROUNDS = 5
HELP_WARNINGS = (
    "platform help degraded",
    "platform help unavailable",
    "invalid platform_help configuration",
)
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


class NotHeld(Exception):
    def __init__(self, reason: str, detail: str = ""):
        super().__init__(reason)
        self.reason = reason
        self.detail = detail


@dataclass
class Ctx:
    binary: str
    root: str
    snap: str
    env: dict
    run_dir: str
    run_id: str
    clock: object
    points: dict
    snap_meta: dict
    clients: int = 1
    out: dict = field(default_factory=lambda: {"metrics": {}, "calls": [], "errors": []})

    def call(self, entry: dict) -> None:
        self.out["calls"].append(entry)

    def error(self, text: str) -> None:
        self.out["errors"].append(text)


def help_warnings(path: str) -> list[str]:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            text = ANSI_RE.sub("", fh.read())
    except OSError:
        return []
    return [w for w in HELP_WARNINGS if w in text]


def na(call_id: str, why: str) -> dict:
    return {"id": call_id, "ok": None, "na": True, "error": why}


def _pidfd(proc) -> dict:
    try:
        return {proc.pid: os.pidfd_open(proc.pid)}
    except ProcessLookupError:
        return {}


def _close_fds(fds: dict) -> None:
    for fd in fds.values():
        os.close(fd)


# -- analyze ------------------------------------------------------------------

TIME_FIELDS = {
    "Maximum resident set size (kbytes)": "max_rss_kb",
    "User time (seconds)": "user_s",
    "System time (seconds)": "sys_s",
    "Percent of CPU this job got": "cpu_pct",
    "Elapsed (wall clock) time (h:mm:ss or m:ss)": "wall_s",
    "Exit status": "exit_code",
}


def parse_gnu_time(text: str) -> dict:
    values = {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("Command terminated by signal"):
            values["signal"] = int(line.rsplit(" ", 1)[1])
        name, sep, value = line.rpartition(": ")
        key = TIME_FIELDS.get(name)
        if not sep or key is None:
            continue
        if key == "wall_s":
            seconds = 0.0
            for part in value.split(":"):
                seconds = seconds * 60 + float(part)
            values[key] = seconds
        elif key == "cpu_pct":
            values[key] = int(value.rstrip("%")) if value.rstrip("%").isdigit() else None
        elif key in ("max_rss_kb", "exit_code"):
            values[key] = int(value)
        else:
            values[key] = float(value)
    return values


def validate_jsonl(path: str) -> dict:
    """Invariant И11: structure of the `analyze --format=jsonl` stream."""
    start = done = None
    files = diagnostics = with_error = lines = 0
    paths = []
    last_type = None
    with open(path, "rb") as fh:
        for number, raw in enumerate(fh, 1):
            lines = number
            try:
                line = raw.decode("utf-8")
            except UnicodeDecodeError:
                raise NotHeld("invalid_jsonl", f"line {number} is not UTF-8") from None
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                raise NotHeld("invalid_jsonl", f"line {number} is not JSON")
            if not isinstance(event, dict):
                raise NotHeld("invalid_jsonl", f"line {number} is not an object")
            last_type = event.get("type")
            if number == 1:
                if last_type != "start":
                    raise NotHeld("invalid_jsonl", "the first event is not `start`")
                start = event
            elif last_type == "file":
                files += 1
                diagnostics += len(event.get("diagnostics") or [])
                with_error += 1 if event.get("error") else 0
                paths.append(event.get("path"))
            elif last_type == "done":
                done = event
    if start is None or lines == 0:
        raise NotHeld("invalid_jsonl", "empty output")
    if last_type != "done" or done is None:
        raise NotHeld("invalid_jsonl", "the last event is not `done`")
    if not (files == done.get("total_files") == start.get("total_files")):
        raise NotHeld("invalid_jsonl", f"file events {files}, done.total_files {done.get('total_files')}, "
                                       f"start.total_files {start.get('total_files')}")
    if diagnostics != done.get("total_diagnostics"):
        raise NotHeld("invalid_jsonl", f"diagnostics {diagnostics} != done.total_diagnostics")
    return {"total_files": files, "total_diagnostics": diagnostics,
            "failed_files": done.get("failed_files"), "files_with_error": with_error,
            "paths": paths}


def _sources_in(names) -> list[str]:
    return [n for n in names if n.endswith((".bsl", ".os"))]


def check_scope(root: str, meta: dict, summary: dict) -> None:
    """Invariant И50: the analysed set matches the corpus mode."""
    with open(os.path.join(root, CONFIG_NAME), "rb") as fh:
        diff_base = (tomllib.load(fh).get("analysis") or {}).get("diff_base")
    total, files_total = summary["total_files"], meta["files_total"]
    if meta["mode"] == "full":
        if diff_base is not None:
            raise NotHeld("scope_mismatch", "full mode but the working config sets diff_base")
        if total < 0.99 * files_total:
            raise NotHeld("scope_mismatch", f"full mode analysed {total} of {files_total} files")
        return
    out = subprocess.run(["git", "diff", "--name-only", "-z", meta["diff_base_sha"]], cwd=root,
                         capture_output=True, check=True).stdout.decode("utf-8")
    changed = {p for p in out.split("\0") if p}
    real_root = os.path.realpath(root)
    for path in summary["paths"]:
        rel = os.path.relpath(path, real_root) if os.path.isabs(path) else path
        if rel.replace(os.sep, "/") not in changed:
            raise NotHeld("scope_mismatch", "a file event lies outside the diff_base area")
    in_area = [p for p in _sources_in(changed) if os.path.isfile(os.path.join(root, p))]
    if total < 0.99 * len(in_area):
        raise NotHeld("scope_mismatch", f"diff mode analysed {total} of {len(in_area)} area files")
    if total >= 0.99 * files_total:
        raise NotHeld("scope_mismatch", f"diff mode analysed {total} of {files_total} snapshot files")


def run_analyze(ctx: Ctx) -> dict:
    argv = [ctx.binary, "analyze", "--format=jsonl", "--quiet", "--source-dir", ctx.root]
    time_path = os.path.join(ctx.run_dir, "time.txt")
    jsonl_path = os.path.join(ctx.run_dir, "analyze.jsonl")
    stderr_path = os.path.join(ctx.run_dir, "stderr.txt")
    t0 = ctx.clock.now()
    with open(jsonl_path, "wb") as out, open(stderr_path, "wb") as err:
        proc = subprocess.Popen(["/usr/bin/time", "-v", "-o", time_path, *argv], cwd=ctx.root,
                                env=ctx.env, stdout=out, stderr=err)
        fds = _pidfd(proc)
        try:
            finished = ctx.clock.wait_exit(fds, ANALYZE_TIMEOUT_S)
        finally:
            _close_fds(fds)
        if not finished:
            procs.stop(ctx.run_id, ctx.clock)
        proc.wait()
    m = ctx.out["metrics"]
    m["run_wall_s"] = ctx.clock.now() - t0
    with open(time_path, encoding="utf-8", errors="replace") as fh:
        timing = parse_gnu_time(fh.read())
    ctx.out["exit_code"] = -timing["signal"] if "signal" in timing else timing.get("exit_code", proc.returncode)
    for key in ("wall_s", "max_rss_kb", "cpu_pct"):
        m[key] = timing.get(key)
    if "user_s" in timing and "sys_s" in timing:
        m["cpu_s"] = round(timing["user_s"] + timing["sys_s"], 3)
    if not finished:
        raise NotHeld("analyze_timeout", f"no exit within {ANALYZE_TIMEOUT_S:.0f} s")
    if ctx.out["exit_code"] != 0:
        raise NotHeld("analyze_exit", f"exit code {ctx.out['exit_code']}")
    summary = validate_jsonl(jsonl_path)
    for key in ("total_files", "total_diagnostics", "failed_files", "files_with_error"):
        m[key] = summary[key]
    if summary["total_files"] == 0:
        raise NotHeld("empty_analysis", "done.total_files = 0")
    check_scope(ctx.root, ctx.snap_meta, summary)
    if help_warnings(stderr_path):
        raise NotHeld("help_degraded", "stderr carries a platform help warning")
    return ctx.out


# -- LSP --------------------------------------------------------------------------


class HarnessLsp(LspClient):
    """`LspClient` on the injected clock: arrival times come from it, and
    responses wake the same condition the harness waits on."""

    def __init__(self, cmd, cwd, env, stderr, clock):
        self.clock = clock
        self.progress_end_at = None
        super().__init__(cmd, cwd=cwd, env=env, stderr=stderr)

    def _route_response(self, message, received_at):
        super()._route_response(message, self.clock.now())
        with self._notify_cond:
            self._notify_cond.notify_all()

    def _handle_notification(self, message):
        message["_received_at"] = self.clock.now()
        if self.progress_end_at is None and message.get("method") == "$/progress":
            if ((message.get("params") or {}).get("value") or {}).get("kind") == "end":
                self.progress_end_at = message["_received_at"]
        super()._handle_notification(message)

    @property
    def dead(self) -> bool:
        return self._closed.is_set()

    def call(self, method: str, params, timeout: float = CALL_TIMEOUT_S):
        """(elapsed_s, result, error); error "closed" means the server is gone."""
        sent_at = self.clock.now()
        try:
            request_id, waiter = self.request_async(method, params)
        except OSError:
            return None, None, "closed"
        arrived = self.clock.wait(lambda: waiter["event"].is_set(), timeout, self._notify_cond,
                                  key=("lsp", self, request_id))
        if not arrived:
            with self._pending_lock:
                self._pending.pop(request_id, None)
            try:
                self.cancel(request_id)
            except OSError:
                pass
            return None, None, "timeout"
        if waiter["error"] is not None:
            return None, None, "closed"
        message = waiter["message"]
        elapsed = waiter["received_at"] - sent_at
        if "error" in message:
            error = message["error"] or {}
            return elapsed, None, f"{error.get('code')}: {error.get('message')}"
        return elapsed, message.get("result"), None

    def wait_publish(self, uri: str, after: int, timeout: float):
        """First `publishDiagnostics` for `uri` among notifications past index
        `after`; URIs compare after percent-decoding (the server encodes them)."""
        target = urllib.parse.unquote(uri)
        found = []

        def ready():
            for message in self.notifications_snapshot()[after:]:
                params = message.get("params") or {}
                if message.get("method") == "textDocument/publishDiagnostics" \
                        and urllib.parse.unquote(params.get("uri", "")) == target:
                    found.append(message)
                    return True
            return self.dead

        self.clock.wait(ready, timeout, self._notify_cond, key=("lsp-publish", self, uri))
        return found[0] if found else None


def _count(result) -> int:
    if result is None:
        return 0
    if isinstance(result, list):
        return len(result)
    if isinstance(result, dict):
        if "items" in result:
            return len(result["items"] or [])
        if "contents" in result:
            contents = result["contents"]
            if isinstance(contents, dict):
                return 1 if contents.get("value") else 0
            return len(contents) if contents else 0
        if "changes" in result or "documentChanges" in result:
            return sum(len(v) for v in (result.get("changes") or {}).values()) \
                + len(result.get("documentChanges") or [])
        return 1
    return 1


def _prepare_rename_count(result) -> int:
    return 0 if result is None else 1


def capabilities(pull: bool) -> dict:
    caps = {
        "window": {"workDoneProgress": True},
        "textDocument": {"publishDiagnostics": {}},
        "workspace": {"configuration": True},
    }
    if pull:
        caps["textDocument"]["diagnostic"] = {"dynamicRegistration": False}
        caps["workspace"]["diagnostics"] = {"refreshSupport": True}
    return caps


class LspRun:
    def __init__(self, ctx: Ctx):
        self.ctx = ctx
        self.stderr_path = os.path.join(ctx.run_dir, "stderr.txt")
        self._stderr = open(self.stderr_path, "wb")
        self.spawned_at = ctx.clock.now()
        self.client = HarnessLsp([ctx.binary, "lsp"], ctx.root, ctx.env, self._stderr, ctx.clock)
        self.sampler = procs.Sampler(ctx.run_id)

    def uri(self, relative_path: str) -> str:
        return pathlib.Path(os.path.join(self.ctx.root, relative_path)).as_uri()

    def call(self, method, params, timeout=CALL_TIMEOUT_S):
        elapsed, result, error = self.client.call(method, params, timeout)
        if error == "closed":
            raise NotHeld("server_failed", f"connection closed during {method}")
        return elapsed, result, error

    def initialize(self, pull: bool) -> dict:
        elapsed, result, error = self.call("initialize", {
            "processId": os.getpid(),
            "rootUri": pathlib.Path(self.ctx.root).as_uri(),
            "capabilities": capabilities(pull),
            "workspaceFolders": [{"uri": pathlib.Path(self.ctx.root).as_uri(), "name": "perf"}],
        })
        if error is not None:
            raise NotHeld("server_failed", f"initialize: {error}")
        self.client.notify("initialized", {})
        self.ctx.out["metrics"]["init_s"] = self.ctx.clock.now() - self.spawned_at
        return result or {}

    def wait_ready(self, query: str) -> None:
        """S2 (plan D7): the first non-empty `workspace/symbol` answer."""
        clock, m = self.ctx.clock, self.ctx.out["metrics"]
        last = "no answer"
        while True:
            left = READY_TIMEOUT_S - (clock.now() - self.spawned_at)
            if left <= 0:
                raise NotHeld("ready_timeout", f"no symbols within {READY_TIMEOUT_S:.0f} s; last: {last}")
            _, result, error = self.call("workspace/symbol", {"query": query}, min(CALL_TIMEOUT_S, left))
            if isinstance(result, list) and result:
                break
            last = error or "empty result"
            clock.sleep(LSP_POLL_S)
        m["ready_s"] = clock.now() - self.spawned_at
        if self.client.progress_end_at is not None:
            m["progress_end_s"] = self.client.progress_end_at - self.spawned_at

    def measure(self, call_id: str, method: str, params, repeats: int, count=_count) -> None:
        entry = {"id": call_id, "first_s": None, "repeat_s": None, "count": None,
                 "ok": True, "error": None, "repeats": repeats}
        samples = []
        for _ in range(1 + repeats):
            elapsed, result, error = self.call(method, params)
            if error is None and count(result) == 0:
                error = "empty result"
            if error is not None:
                entry.update(ok=False, error=error)
                break
            if not samples:
                entry["count"] = count(result)
            samples.append(elapsed)
        if entry["ok"]:
            entry["first_s"] = samples[0]
            entry["repeat_s"] = median(samples[1:])
        self.ctx.call(entry)

    def publish(self, call_id: str, point: dict) -> None:
        text = read_point_text(self.ctx, point)
        uri = self.uri(point["relative_path"])
        after = len(self.client.notifications_snapshot())
        opened_at = self.ctx.clock.now()
        self.client.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "bsl", "version": 1, "text": text}})
        message = self.client.wait_publish(uri, after, PUBLISH_TIMEOUT_S)
        entry = {"id": call_id, "first_s": None, "repeat_s": None, "count": None,
                 "ok": False, "error": None, "repeats": 0}
        if message is None:
            if self.client.dead:
                raise NotHeld("server_failed", "connection closed waiting for publishDiagnostics")
            entry["error"] = "timeout"
        else:
            count = len((message.get("params") or {}).get("diagnostics") or [])
            entry.update(first_s=message["_received_at"] - opened_at, count=count,
                         publish_s=message["_received_at"] - opened_at,
                         ok=count > 0, error=None if count > 0 else "empty result")
        self.ctx.call(entry)

    def finish(self) -> None:
        """shutdown + exit; a server that does not exit within the timeout is
        taken down with the rest of the tracked set (plan §6.4)."""
        out, client = self.ctx.out, self.client
        if not client.dead:
            client.call("shutdown", None, SHUTDOWN_TIMEOUT_S)
            try:
                client.notify("exit", None)
            except OSError:
                pass
        fds = _pidfd(client.proc)
        try:
            exited = self.ctx.clock.wait_exit(fds, SHUTDOWN_TIMEOUT_S)
        finally:
            _close_fds(fds)
        out["shutdown_timeout"] = not exited
        self.sampler.sample()
        out["stop"] = procs.stop(self.ctx.run_id, self.ctx.clock)
        out["exit_code"] = client.proc.wait()
        client.close()
        self._stderr.close()
        sampled = self.sampler.stop()
        out["metrics"]["rss_peak_kb"] = sampled["rss_peak_kb"]
        out["metrics"]["cpu_s"] = sampled["cpu_s"]
        out["metrics"]["run_wall_s"] = self.ctx.clock.now() - self.spawned_at


def read_point_text(ctx: Ctx, point: dict) -> str:
    with open(os.path.join(ctx.snap, point["relative_path"]), "rb") as fh:
        data = fh.read()
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        raise NotHeld("point_unreadable", f"point {point['id']}: file is not UTF-8") from None


def _end_position(text: str) -> dict:
    lines = text.split("\n")
    return {"line": len(lines) - 1, "character": len(lines[-1].encode("utf-16-le")) // 2}


def run_lsp_session(ctx: Ctx) -> dict:
    p = ctx.points
    if "F" not in p or "Q" not in p:
        raise NotHeld("precondition_missing", "point F or Q is missing")
    run = LspRun(ctx)
    try:
        run.initialize(pull=False)
        run.wait_ready(p["Q"]["query"])
        snap = procs.snapshot(ctx.run_id)
        m = ctx.out["metrics"]
        m["rss_ready_kb"] = snap["rss_kb"]
        m["rss_ready_by_pid"] = snap["rss_by_pid"]
        m["cpu_start_s"] = round(snap["cpu_s"], 3)
        for call_id, push_key, file_key in (("L5", "L5", "F"), ("L5s", "L5s", "F_small")):
            if push_key in p and file_key in p:
                run.publish(call_id, p[file_key])
            else:
                ctx.call(na(call_id, "point_missing"))

        def at(key):
            point = p[key]
            return {"textDocument": {"uri": run.uri(point["relative_path"])},
                    "position": {"line": point["line"], "character": point["character"]}}

        f_doc = {"textDocument": {"uri": run.uri(p["F"]["relative_path"])}}
        for call_id, key, method, repeats in (
                ("L1", "P_compl", "textDocument/completion", 4),
                ("L2", "P_hover", "textDocument/hover", 4),
                ("L3", "P_def", "textDocument/definition", 4)):
            if key in p:
                run.measure(call_id, method, at(key), repeats)
            else:
                ctx.call(na(call_id, "point_missing"))
        run.measure("L4", "textDocument/documentSymbol", f_doc, 4)
        if "L6" in p:
            end = _end_position(read_point_text(ctx, p["F"]))
            run.measure("L6", "textDocument/inlayHint",
                        {**f_doc, "range": {"start": {"line": 0, "character": 0}, "end": end}}, 4)
        else:
            ctx.call(na("L6", "point_missing"))
        run.measure("G1", "workspace/symbol", {"query": p["Q"]["query"]}, 2)
        if "P_refs" in p:
            run.measure("G2", "textDocument/references",
                        {**at("P_refs"), "context": {"includeDeclaration": True}}, 0)
        else:
            ctx.call(na("G2", "point_missing"))
        if "P_ren" in p:
            run.measure("G3", "textDocument/prepareRename", at("P_ren"), 2, _prepare_rename_count)
            run.measure("G4", "textDocument/rename",
                        {**at("P_ren"), "newName": p["P_ren"]["new_name"]}, 0)
        else:
            ctx.call(na("G3", "point_missing"))
            ctx.call(na("G4", "point_missing"))
        m["rss_after_global_kb"] = procs.snapshot(ctx.run_id)["rss_kb"]
    finally:
        run.finish()
    if help_warnings(run.stderr_path):
        raise NotHeld("help_degraded", "stderr carries a platform help warning")
    return ctx.out


def run_lsp_wsdiag(ctx: Ctx) -> dict:
    p = ctx.points
    if "F" not in p or "Q" not in p:
        raise NotHeld("precondition_missing", "point F or Q is missing")
    run = LspRun(ctx)
    try:
        init = run.initialize(pull=True)
        run.wait_ready(p["Q"]["query"])
        entry = {"id": "wsdiag", "first_s": None, "repeat_s": None, "count": None,
                 "ok": False, "error": None, "repeats": 0}
        if (init.get("capabilities") or {}).get("diagnosticProvider") is None:
            entry["error"] = "server did not declare diagnosticProvider"
        else:
            token = f"perf-{ctx.run_id}"
            elapsed, result, error = run.call(
                "workspace/diagnostic", {"previousResultIds": [], "partialResultToken": token})
            uris = set()
            for message in run.client.notifications_snapshot():
                params = message.get("params") or {}
                if message.get("method") == "$/progress" and params.get("token") == token:
                    uris.update(i.get("uri") for i in (params.get("value") or {}).get("items") or [])
            uris.update(i.get("uri") for i in (result or {}).get("items") or [])
            uris.discard(None)
            entry.update(first_s=elapsed, count=len(uris), error=error)
            entry["ok"] = error is None and len(uris) > 0
            if error is None and not uris:
                entry["error"] = "empty result"
            ctx.out["metrics"]["wsdiag_s"] = elapsed
            ctx.out["metrics"]["wsdiag_items"] = len(uris)
        ctx.call(entry)
    finally:
        run.finish()
    if help_warnings(run.stderr_path):
        raise NotHeld("help_degraded", "stderr carries a platform help warning")
    return ctx.out


# -- MCP --------------------------------------------------------------------------

# Whole words only: a field name such as `location_unavailable` is not a refusal.
BAD_TEXT_RE = re.compile(r"(?<!\w)(still building|not ready|unavailable)(?!\w)")
BAD_STATES = ("loading", "building", "failed")
GRAPH_PENDING = ("waiting", "running", "loading", "unknown")


def _texts(result: dict) -> str:
    return "\n".join(c.get("text", "") for c in result.get("content") or []
                     if isinstance(c, dict) and c.get("type") == "text")


def _documents(result: dict) -> list[dict]:
    docs = []
    if isinstance(result.get("structuredContent"), dict):
        docs.append(result["structuredContent"])
    for content in result.get("content") or []:
        try:
            doc = json.loads(content.get("text", ""))
        except (json.JSONDecodeError, AttributeError, TypeError):
            continue
        if isinstance(doc, dict):
            docs.append(doc)
    return docs


def _state(result: dict) -> str | None:
    for doc in _documents(result):
        if isinstance(doc.get("state"), str):
            return doc["state"]
    return None


def graph_busy(result) -> bool:
    """`failed` with no reason code and a "graph busy" error: the cache is still
    leased to a previous owner (its heartbeat ages out within a minute), so the
    graph is not broken yet — readiness keeps waiting (owner decision 9466)."""
    for doc in _documents(result or {}):
        if doc.get("state") != "failed" or not str(doc.get("error", "")).startswith("graph busy"):
            continue
        targets = (doc.get("indexing") or {}).get("targets") or []
        graph = [t for t in targets if isinstance(t, dict) and t.get("kind") == "graph"]
        if graph and all(t.get("reason_code") is None for t in graph):
            return True
    return False


def check_tool_result(result) -> str | None:
    """None when the answer is a real result (invariant И9), else why not."""
    if not isinstance(result, dict):
        return "no result"
    text = _texts(result)
    if result.get("isError"):
        return text or "isError"
    if not text.strip() and not result.get("structuredContent"):
        return "empty result"
    if BAD_TEXT_RE.search(text):
        return text
    for doc in _documents(result):
        if doc.get("state") in BAD_STATES or doc.get("status") == "loading":
            return text or json.dumps(doc, ensure_ascii=False)
    return None


def mcp_calls(points: dict, round_index: int) -> list:
    """The calls of one round (plan §8.3); M3 rotates over F_rot. The search
    tool (M4) is outside the nightly set, so the ids keep their numbers."""
    f, hover, rot = points.get("F"), points.get("P_hover"), points.get("F_rot") or []
    calls = [
        ("M1", "outline", None, {"path": f["relative_path"]}),
        ("M2", "symbol_info", None, {"path": hover["relative_path"], "line": hover["line"],
                                     "column": hover["character"]} if hover else None),
        ("M3", "diagnostics", "file", {"action": "file",
                                      "path": rot[round_index % len(rot)]["relative_path"]} if rot else None),
        ("M5", "graph", "overview", {"action": "overview"}),
        ("M6", "metadata", "info", {"action": "info"}),
        ("M7", "syntax_help", None, {"name": "СтрНайти"}),
    ]
    return calls


def tool_actions(tools: list) -> dict:
    """{name: set of actions or None when the tool takes no `action`}."""
    found = {}
    for tool in tools:
        action = ((tool.get("inputSchema") or {}).get("properties") or {}).get("action") or {}
        enum = action.get("enum")
        found[tool.get("name")] = set(enum) if isinstance(enum, list) else None
    return found


def call_available(actions: dict, tool: str, action: str | None) -> bool:
    if tool not in actions:
        return False
    return action is None or actions[tool] is None or action in actions[tool]


class McpRun:
    def __init__(self, ctx: Ctx, index: int = 0):
        self.ctx = ctx
        self.stderr_path = os.path.join(ctx.run_dir, f"stderr-{index}.txt")
        self._stderr = open(self.stderr_path, "wb")
        self.spawned_at = ctx.clock.now()
        self.client = McpClient(
            [ctx.binary, "mcp", "serve", "--profile", "workspace", "--source-dir", ctx.root],
            ctx.root, ctx.env, self._stderr, ctx.clock)
        self.actions: dict = {}

    def request(self, method, params, timeout=CALL_TIMEOUT_S):
        elapsed, result, error = self.client.request(method, params, timeout)
        if error == "closed":
            raise NotHeld("server_failed", f"connection closed during {method}")
        return elapsed, result, error

    def tool(self, name, arguments, timeout=CALL_TIMEOUT_S):
        return self.request("tools/call", {"name": name, "arguments": arguments}, timeout)

    def status(self, name: str):
        left = READY_TIMEOUT_S - (self.ctx.clock.now() - self.spawned_at)
        if left <= 0:
            raise NotHeld("ready_timeout", f"{name} status not final within {READY_TIMEOUT_S:.0f} s")
        return self.tool(name, {"action": "status"}, min(CALL_TIMEOUT_S, left))

    def start(self) -> tuple[float, float]:
        elapsed, _, error = self.client.initialize(CALL_TIMEOUT_S)
        if error is not None:
            raise NotHeld("server_failed", f"initialize: {error}")
        init_s = self.ctx.clock.now() - self.spawned_at
        tools_s, result, error = self.request("tools/list", {})
        if error is not None:
            raise NotHeld("server_failed", f"tools/list: {error}")
        self.actions = tool_actions((result or {}).get("tools") or [])
        return init_s, tools_s

    def wait_ready(self) -> float:
        """Poll the diagnostics and graph statuses once a second until both are
        final; a backend that ends `failed` or `disabled` fails the run."""
        clock = self.ctx.clock
        while True:
            pending = []
            if "diagnostics" in self.actions:
                _, result, error = self.status("diagnostics")
                state = _state(result) if error is None else None
                if state in ("failed", "disabled"):
                    raise NotHeld("backend_failed", f"diagnostics status {state}")
                if state != "ready":
                    pending.append("diagnostics")
            if "graph" in self.actions:
                _, result, error = self.status("graph")
                state = _state(result) if error is None else None
                if state == "failed" and graph_busy(result):
                    pending.append("graph")
                elif state in ("failed", "disabled"):
                    raise NotHeld("backend_failed", f"graph status {state}: {_texts(result or {})}")
                elif state is None or state in GRAPH_PENDING:
                    pending.append("graph")
            if not pending:
                return clock.now() - self.spawned_at
            if clock.now() - self.spawned_at >= READY_TIMEOUT_S:
                raise NotHeld("ready_timeout", f"still pending: {', '.join(pending)}")
            clock.sleep(MCP_POLL_S)

    def round(self, round_index: int) -> list[dict]:
        """One round of calls: [{id, elapsed, ok, error, timeout}] or n/a."""
        samples = []
        for call_id, tool, action, arguments in mcp_calls(self.ctx.points, round_index):
            if arguments is None:
                samples.append({"id": call_id, "na": True, "error": "point_missing"})
                continue
            if not call_available(self.actions, tool, action):
                samples.append({"id": call_id, "na": True, "error": "tool_missing"})
                continue
            elapsed, result, error = self.tool(tool, arguments)
            if error is None:
                error = check_tool_result(result)
            samples.append({"id": call_id, "elapsed": elapsed, "ok": error is None,
                            "error": error, "timeout": (error or "").startswith("timeout")})
        return samples

    def close(self) -> int | None:
        self.client.close_stdin()
        return None

    def reap(self, writers_gone: bool) -> int | None:
        code = self.client.proc.wait()
        self._stderr.close()
        # A survivor of the stop may still hold the pipe; its run is stop_incomplete anyway.
        if writers_gone:
            self.client.release()
        return code


def stop_mcp(ctx: Ctx, runs: list, sampler) -> None:
    """Close the clients' stdin, then take down the tracked set — the proxy's
    daemon outlives its clients by the idle TTL (plan §6.4)."""
    for run in runs:
        run.close()
    sampler.sample()
    ctx.out["stop"] = procs.stop(ctx.run_id, ctx.clock)
    codes = [run.reap(not ctx.out["stop"]["left"]) for run in runs]
    ctx.out["exit_code"] = codes[0] if len(codes) == 1 else codes


def check_mcp_help(runs: list) -> None:
    if any(help_warnings(run.stderr_path) for run in runs):
        raise NotHeld("help_degraded", "stderr carries a platform help warning")


def _mcp_precondition(ctx: Ctx) -> None:
    if "F" not in ctx.points:
        raise NotHeld("precondition_missing", "point F is missing")


def run_mcp_start(ctx: Ctx) -> dict:
    _mcp_precondition(ctx)
    sampler = procs.Sampler(ctx.run_id)
    run = McpRun(ctx)
    try:
        init_s, tools_s = run.start()
        m = ctx.out["metrics"]
        m["init_s"], m["tools_s"] = init_s, tools_s
        ctx.call({"id": "tools", "first_s": tools_s, "repeat_s": None, "count": len(run.actions),
                  "ok": "diagnostics" in run.actions, "repeats": 0,
                  "error": None if "diagnostics" in run.actions else "no diagnostics tool"})
    finally:
        stop_mcp(ctx, [run], sampler)
        _finish_sampler(ctx, sampler, run.spawned_at)
    check_mcp_help([run])
    return ctx.out


def _finish_sampler(ctx: Ctx, sampler, started_at: float) -> None:
    sampled = sampler.stop()
    ctx.out["metrics"]["rss_peak_kb"] = sampled["rss_peak_kb"]
    ctx.out["metrics"]["cpu_s"] = sampled["cpu_s"]
    ctx.out["metrics"]["run_wall_s"] = ctx.clock.now() - started_at


def _aggregate(call_id: str, samples: list[dict]) -> dict:
    """first_s from round 1, repeat_s = median of later rounds; one failed
    round makes the call `ok=false` (its latency stays out of medians)."""
    if samples and samples[0].get("na"):
        return na(call_id, samples[0]["error"])
    failed = [s for s in samples if not s["ok"]]
    entry = {"id": call_id, "first_s": None, "repeat_s": None, "count": len(samples),
             "ok": not failed, "error": failed[0]["error"] if failed else None,
             "repeats": len(samples) - 1, "timeouts": sum(s["timeout"] for s in samples)}
    if not failed:
        entry["first_s"] = samples[0]["elapsed"]
        entry["repeat_s"] = median([s["elapsed"] for s in samples[1:]])
    return entry


def run_mcp_memory(ctx: Ctx) -> dict:
    _mcp_precondition(ctx)
    sampler = procs.Sampler(ctx.run_id)
    run = McpRun(ctx)
    m = ctx.out["metrics"]
    try:
        m["init_s"], m["tools_s"] = run.start()
        m["rss_start_kb"] = procs.snapshot(ctx.run_id)["rss_kb"]
        m["ready_s"] = run.wait_ready()
        m["rss_ready_kb"] = procs.snapshot(ctx.run_id)["rss_kb"]
        by_call: dict[str, list] = {}
        for index in range(MCP_MEMORY_ROUNDS):
            for sample in run.round(index):
                by_call.setdefault(sample["id"], []).append(sample)
        m["rss_series_kb"] = procs.snapshot(ctx.run_id)["rss_kb"]
        m["rss_growth_kb"] = m["rss_series_kb"] - m["rss_ready_kb"]
        for call_id, samples in by_call.items():
            ctx.call(_aggregate(call_id, samples))
    finally:
        stop_mcp(ctx, [run], sampler)
        _finish_sampler(ctx, sampler, run.spawned_at)
    check_mcp_help([run])
    return ctx.out


def percentile(values: list[float], fraction: float) -> float | None:
    """Nearest-rank percentile."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def run_mcp_load(ctx: Ctx) -> dict:
    _mcp_precondition(ctx)
    sampler = procs.Sampler(ctx.run_id)
    runs: list[McpRun] = []
    m = ctx.out["metrics"]
    started_at = ctx.clock.now()
    failures: list[NotHeld] = []
    samples: list[dict] = []
    lock = threading.Lock()
    try:
        runs = [McpRun(ctx, index) for index in range(ctx.clients)]
        barrier = threading.Barrier(ctx.clients + 1)

        def client(run: McpRun) -> None:
            try:
                run.start()
                run.wait_ready()
            except NotHeld as exc:
                failures.append(exc)
                barrier.abort()
                return
            except Exception as exc:  # a dead thread would leave the barrier waiting forever
                failures.append(NotHeld("server_failed", f"{type(exc).__name__}: {exc}"))
                barrier.abort()
                return
            try:
                barrier.wait()
            except threading.BrokenBarrierError:
                return
            try:
                for index in range(MCP_LOAD_ROUNDS):
                    got = run.round(index)
                    with lock:
                        samples.extend(got)
            except NotHeld as exc:
                failures.append(exc)
            except Exception as exc:
                failures.append(NotHeld("server_failed", f"{type(exc).__name__}: {exc}"))

        threads = [threading.Thread(target=client, args=(run,)) for run in runs]
        for thread in threads:
            thread.start()
        try:
            barrier.wait()
            load_from = ctx.clock.now()
            sampler.sample()
            cpu_from = sampler.cpu_s()
        except threading.BrokenBarrierError:
            load_from = None
        for thread in threads:
            thread.join()
        if failures:
            raise failures[0]
        window = ctx.clock.now() - load_from
        sampler.sample()
        m["cpu_s_load"] = round(sampler.cpu_s() - cpu_from, 3)
        m["cpu_pct_load"] = round(100.0 * m["cpu_s_load"] / window, 1) if window > 0 else None
        m["load_window_s"] = window
        measured = [s for s in samples if not s.get("na")]
        ok = [s["elapsed"] for s in measured if s["ok"]]
        m["p50_s"], m["p95_s"] = percentile(ok, 0.50), percentile(ok, 0.95)
        m["timeouts"] = sum(s["timeout"] for s in measured)
        for call_id in ("M1", "M2", "M3", "M5", "M6", "M7"):
            mine = [s for s in samples if s["id"] == call_id]
            if not mine or mine[0].get("na"):
                ctx.call(na(call_id, mine[0]["error"] if mine else "not run"))
                continue
            good = [s["elapsed"] for s in mine if s["ok"]]
            bad = [s for s in mine if not s["ok"]]
            ctx.call({"id": call_id, "p50_s": percentile(good, 0.50), "p95_s": percentile(good, 0.95),
                      "count": len(mine), "errors": len(bad), "timeouts": sum(s["timeout"] for s in mine),
                      "ok": not bad, "error": bad[0]["error"] if bad else None})
    finally:
        if runs:
            stop_mcp(ctx, runs, sampler)
        _finish_sampler(ctx, sampler, started_at)
    check_mcp_help(runs)
    return ctx.out


SCENARIOS = {
    "analyze": run_analyze,
    "lsp-session": run_lsp_session,
    "lsp-workspace-diag": run_lsp_wsdiag,
    "mcp-start": run_mcp_start,
    "mcp-memory": run_mcp_memory,
    "mcp-load": run_mcp_load,
}


def run_scenario(name: str, ctx: Ctx) -> dict:
    out = ctx.out
    out.setdefault("exit_code", None)
    try:
        SCENARIOS[name](ctx)
        out["held"], out["reason"] = True, None
    except NotHeld as exc:
        out["held"], out["reason"] = False, exc.reason
        if exc.detail:
            ctx.error(f"{exc.reason}: {exc.detail}")
    except (OSError, HarnessError) as exc:
        out["held"], out["reason"] = False, "server_failed"
        ctx.error(f"harness: {type(exc).__name__}: {exc}")
    return out
