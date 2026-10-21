"""Scripted stand-in for `bsl-analyzer-app`: `analyze`, `lsp`, `mcp serve`.

Behaviour is chosen by environment variables (the harness strips only `BSL_*`):
  FAKE_LOG     directory for logs (argv, cwd, environment, launch counter)
  FAKE_MODE    comma-separated switches, see below
Every launch appends `{"argv", "cwd", "env", "cache"}` to FAKE_LOG/launches.jsonl.

Switches:
  cache          create $XDG_CACHE_HOME/bsl-analyzer/workspaces/v1/x and $XDG_CACHE_HOME/keep
  build          create <cwd>/.build/db
  stubborn       spawn a child that ignores SIGTERM and lives on (a daemon)
  daemon         spawn a child that lives on after the client leaves (handles SIGTERM)
  foreign2       on the 2nd measured launch spawn a detached `perf-nightly-test-x` without the run marker
  analyze: no_done, total_mismatch, bad_line, bad_utf8, zero_files, exit2, help_warn, only_first
  helpcheck (BSL_LOG=info): help_noline, help_auto, help_degraded
  lsp: hover_null, never_ready, foreign_publish, noexit, no_pull, help_warn_lsp
  mcp: no_graph, iserror_m1, loading_m6, status_loading_m5, silent_m1,
       never_ready, diag_failed, search_failed, graph_failed, graph_disabled, graph_busy,
       graph_busy_no_target, graph_busy_failure, help_warn_mcp, field_unavailable_m1
Every MCP tool call is appended as `{"name", "arguments"}` to FAKE_LOG/tools.jsonl.
"""

import json
import os
import re
import signal
import subprocess
import sys
import tomllib
import urllib.parse

MODE = set(filter(None, os.environ.get("FAKE_MODE", "").split(",")))
LOG = os.environ.get("FAKE_LOG")


def launch_number() -> int:
    path = os.path.join(LOG, "launches.jsonl")
    cache = os.environ.get("XDG_CACHE_HOME") or ""
    had = {name: os.path.exists(os.path.join(cache, *name.split("/")))
           for name in ("bsl-analyzer/workspaces/v1/x", "keep")}
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"argv": sys.argv[1:], "cwd": os.getcwd(), "env": dict(os.environ),
                             "cache": cache, "pid": os.getpid(), "had": had,
                             "had_build": os.path.exists(os.path.join(os.getcwd(), ".build"))},
                            ensure_ascii=False) + "\n")
    with open(path, encoding="utf-8") as fh:
        # Launches of measured runs; the help check (BSL_LOG=info) does not count.
        return sum(1 for line in fh if "BSL_LOG" not in json.loads(line)["env"])


def side_effects(number: int) -> None:
    cache = os.environ.get("XDG_CACHE_HOME")
    if "cache" in MODE and cache:
        os.makedirs(os.path.join(cache, "bsl-analyzer", "workspaces", "v1", "x"), exist_ok=True)
        open(os.path.join(cache, "keep"), "a").close()
    if "build" in MODE:
        os.makedirs(os.path.join(os.getcwd(), ".build"), exist_ok=True)
        open(os.path.join(os.getcwd(), ".build", "db"), "a").close()
    if "stubborn" in MODE or "daemon" in MODE:
        ignore = "signal.signal(signal.SIGTERM, signal.SIG_IGN)" if "stubborn" in MODE else "pass"
        child = subprocess.Popen(
            [sys.executable, "-c", f"import signal, sys\n{ignore}\nprint(flush=True)\nwhile True: signal.pause()"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, start_new_session=True)
        child.stdout.readline()  # the handler is installed before we go on
        with open(os.path.join(LOG, "daemon.pid"), "w") as fh:
            fh.write(str(child.pid))
    if "foreign2" in MODE and number == 2:
        env = {k: v for k, v in os.environ.items() if k != "PERF_NIGHTLY_RUN"}
        child = subprocess.Popen(["perf-nightly-test-x", "-c", "import signal\nprint(flush=True)\nsignal.pause()"],
                                 executable=sys.executable, env=env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, start_new_session=True)
        child.stdout.readline()
        with open(os.path.join(LOG, "foreign.pid"), "w") as fh:
            fh.write(str(child.pid))


# -- analyze -----------------------------------------------------------------------


def analyze(argv) -> int:
    source = argv[argv.index("--source-dir") + 1]
    if os.environ.get("BSL_LOG") == "info":
        return helpcheck(source)
    if "help_warn" in MODE:
        sys.stderr.write("WARN platform help degraded to the built-in interface facts; descriptions are empty\n")
    if "exit2" in MODE:
        return 2
    files = sorted(os.path.join(dp, f) for dp, _, fs in os.walk(source) for f in fs
                   if f.endswith(".bsl") and "/.git/" not in dp + "/")
    if "only_first" in MODE:
        files = files[:1]
    if "zero_files" in MODE:
        files = []
    out = sys.stdout
    total = len(files) + (1 if "total_mismatch" in MODE else 0)
    out.write(json.dumps({"type": "start", "total_files": total, "version": "fake"}) + "\n")
    if "bad_line" in MODE:
        out.write("{broken\n")
    if "bad_utf8" in MODE:
        out.flush()
        sys.stdout.buffer.write(b'{"type": "file", "path": "\xff"}\n')
        sys.stdout.buffer.flush()
    for path in files:
        out.write(json.dumps({"type": "file", "path": path, "diagnostics": [{"code": "X"}]}) + "\n")
    if "no_done" not in MODE:
        out.write(json.dumps({"type": "done", "elapsed_secs": 0.1, "total_files": total,
                              "total_diagnostics": len(files), "failed_files": 0}) + "\n")
    return 0


def helpcheck(source) -> int:
    with open(os.path.join(source, "bsl-analyzer.toml"), "rb") as fh:
        section = tomllib.load(fh).get("platform_help") or {}
    shown = f"{section.get('source')} ({section['path']})" if section.get("path") else section.get("source")
    if "help_auto" in MODE:
        shown = "auto"
    if "help_noline" not in MODE:
        sys.stderr.write(f"INFO platform_help_bootstrap: platform help loaded source={shown} origin=\"x\"\n")
    if "help_degraded" in MODE:
        sys.stderr.write("WARN platform help degraded to the built-in interface facts; descriptions are empty\n")
    return 0


# -- LSP ----------------------------------------------------------------------------


class Wire:
    def __init__(self):
        self.stdin, self.stdout = sys.stdin.buffer, sys.stdout.buffer

    def read(self):
        headers = {}
        line = self.stdin.readline()
        if not line:
            return None
        while line not in (b"\r\n", b"\n"):
            key, value = line.decode("ascii").split(":", 1)
            headers[key.strip().lower()] = value.strip()
            line = self.stdin.readline()
        return json.loads(self.stdin.read(int(headers["content-length"])))

    def send(self, message):
        body = json.dumps(message, ensure_ascii=False).encode("utf-8")
        self.stdout.write(b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
        self.stdout.flush()

    def reply(self, request, result):
        self.send({"jsonrpc": "2.0", "id": request["id"], "result": result})


LOC = {"uri": "file:///x.bsl", "range": {"start": {"line": 0, "character": 0},
                                         "end": {"line": 0, "character": 1}}}


def lsp() -> int:
    wire = Wire()
    polls = 0
    if "help_warn_lsp" in MODE:
        sys.stderr.write("WARN platform help unavailable; platform lookups and docs are empty\n")
        sys.stderr.flush()
    while True:
        message = wire.read()
        if message is None:
            return 0
        method = message.get("method")
        if method == "initialize":
            caps = {} if "no_pull" in MODE else {"diagnosticProvider": {"workspaceDiagnostics": True}}
            wire.reply(message, {"capabilities": caps})
            wire.send({"jsonrpc": "2.0", "method": "$/progress",
                       "params": {"token": "t", "value": {"kind": "end"}}})
        elif method == "workspace/symbol":
            polls += 1
            ready = polls > 2 and "never_ready" not in MODE
            wire.reply(message, [{"name": "s", "kind": 12, "location": LOC}] if ready else [])
        elif method == "textDocument/didOpen":
            uri = message["params"]["textDocument"]["uri"]
            if "foreign_publish" in MODE:
                wire.send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                           "params": {"uri": "file:///elsewhere.bsl", "diagnostics": [{}]}})
                wire.send({"jsonrpc": "2.0", "method": "$/fake/silent", "params": {"uri": uri}})
            else:
                # Lower-case escapes: equal to the client's URI only after decoding.
                encoded = re.sub(r"%[0-9A-F]{2}", lambda m: m.group(0).lower(),
                                 "file://" + urllib.parse.quote(urllib.parse.unquote(uri[len("file://"):])))
                wire.send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                           "params": {"uri": encoded, "diagnostics": [{"message": "d"}]}})
        elif method == "textDocument/completion":
            wire.reply(message, {"isIncomplete": False, "items": [{"label": "a"}]})
        elif method == "textDocument/hover":
            wire.reply(message, None if "hover_null" in MODE else {"contents": {"kind": "markdown", "value": "h"}})
        elif method in ("textDocument/definition", "textDocument/references"):
            wire.reply(message, [LOC])
        elif method == "textDocument/documentSymbol":
            wire.reply(message, [{"name": "s", "kind": 12, "range": LOC["range"], "selectionRange": LOC["range"]}])
        elif method == "textDocument/inlayHint":
            wire.reply(message, [{"position": {"line": 0, "character": 0}, "label": "h"}])
        elif method == "textDocument/prepareRename":
            wire.reply(message, LOC["range"])
        elif method == "textDocument/rename":
            wire.reply(message, {"changes": {LOC["uri"]: [{"range": LOC["range"], "newText": "n"}]}})
        elif method == "workspace/diagnostic":
            token = message["params"].get("partialResultToken")
            wire.send({"jsonrpc": "2.0", "method": "$/progress", "params": {
                "token": token, "value": {"items": [{"uri": "file:///a.bsl", "kind": "full", "items": []}]}}})
            wire.reply(message, {"items": [{"uri": "file:///b.bsl", "kind": "full", "items": []}]})
        elif method == "shutdown":
            wire.reply(message, None)
        elif method == "exit":
            if "noexit" in MODE:
                continue
            return 0


# -- MCP ----------------------------------------------------------------------------

TOOLS = [
    ("outline", None), ("symbol_info", None), ("diagnostics", ["status", "file"]),
    ("search", ["status", "search_code"]), ("graph", ["status", "overview"]),
    ("metadata", ["info"]), ("syntax_help", None),
]


def text(value, error=False, structured=None):
    result = {"content": [{"type": "text", "text": value}], "isError": error}
    if structured is not None:
        result["structuredContent"] = structured
    return result


GRAPH_STATUS_CALLS = []


def tool_result(name, args, call_index):
    action = args.get("action")
    if name == "diagnostics" and action == "status":
        state = "failed" if "diag_failed" in MODE else "loading" if "never_ready" in MODE else "ready"
        return text(json.dumps({"state": state}), structured={"state": state})
    if name == "search" and action == "status":
        if "search_failed" in MODE:
            return text("Summary:\nlexical: unavailable - search index initialization failed; waiting will not publish it.")
        return text("Summary:\nlexical: reflects the current working tree.")
    if name == "graph" and action == "status":
        target = {"kind": "graph", "reason_code": None, "state": "ready"}
        GRAPH_STATUS_CALLS.append(1)
        if "graph_busy_no_target" in MODE:
            return text(json.dumps({"state": "failed", "error": "graph busy: lease held"}))
        if "graph_busy_failure" in MODE:
            return text(json.dumps({"state": "failed", "error": "graph busy: native failure",
                                    "indexing": {"targets": [{**target, "state": "failed",
                                                              "reason_code": "native_failure"}]}}))
        if "graph_busy" in MODE and len(GRAPH_STATUS_CALLS) <= 2:
            target["state"] = "failed"
            return text(json.dumps({"state": "failed", "error": "graph busy: the cache directory /c is used by "
                                    "another live process (pid 1, version x)", "indexing": {"targets": [target]}}))
        if "graph_failed" in MODE:
            return text(json.dumps({"state": "failed", "error": "builder panicked", "indexing": {"targets": [
                {**target, "state": "failed", "reason_code": "native_failure"}]}}))
        state = "disabled" if "graph_disabled" in MODE else "ready"
        return text(json.dumps({"state": state}))
    if name == "outline" and "field_unavailable_m1" in MODE:
        return text(json.dumps({"nodes": [{"name": "x", "location_unavailable": "no_source_location"}]}))
    if name == "outline" and "iserror_m1" in MODE:
        return text("boom", error=True)
    if name == "metadata" and "loading_m6" in MODE:
        return text(json.dumps({"state": "loading"}))
    if name == "graph" and "status_loading_m5" in MODE:
        return text(json.dumps({"status": "loading"}))
    return text(f"{name} ok")


def silence_once() -> bool:
    try:
        os.close(os.open(os.path.join(LOG, "silenced"), os.O_CREAT | os.O_EXCL))
        return True
    except FileExistsError:
        return False


def mcp() -> int:
    out = sys.stdout
    calls = 0
    if "help_warn_mcp" in MODE:
        sys.stderr.write("WARN platform help degraded to the built-in interface facts; descriptions are empty\n")
        sys.stderr.flush()

    def send(message):
        out.write(json.dumps(message, ensure_ascii=False) + "\n")
        out.flush()

    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if "id" not in message:
            continue
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {
                "protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake"}}})
        elif method == "tools/list":
            tools = []
            for name, actions in TOOLS:
                if name == "graph" and "no_graph" in MODE:
                    continue
                props = {"action": {"type": "string", "enum": actions}} if actions else {}
                tools.append({"name": name, "inputSchema": {"type": "object", "properties": props}})
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"tools": tools}})
        elif method == "tools/call":
            calls += 1
            name, args = message["params"]["name"], message["params"].get("arguments") or {}
            with open(os.path.join(LOG, "tools.jsonl"), "a", encoding="utf-8") as fh:
                fh.write(json.dumps({"name": name, "arguments": args}, ensure_ascii=False) + "\n")
            if name == "outline" and "silent_m1" in MODE and silence_once():
                send({"jsonrpc": "2.0", "method": "fake/silent", "params": {"id": message["id"]}})
                continue
            send({"jsonrpc": "2.0", "id": message["id"], "result": tool_result(name, args, calls)})
    return 0


def main() -> int:
    argv = sys.argv[1:]
    side_effects(launch_number())
    if argv[:1] == ["analyze"]:
        return analyze(argv)
    if argv[:1] == ["lsp"]:
        return lsp()
    if argv[:2] == ["mcp", "serve"]:
        return mcp()
    return 64


if __name__ == "__main__":
    sys.exit(main())
