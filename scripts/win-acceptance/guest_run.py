#!/usr/bin/env python3
"""Run the Windows native-check matrix inside the guest (stdlib only).

For every row: snapshot the fixture source (path, size, SHA-256), call the MCP
`execute` tool, compare the answer with the expected outcome, snapshot the
source again and check that no Designer process, owned job or sentinel marker
is left behind. Results are appended to results.jsonl as they come.
"""
import argparse
import hashlib
import json
import os
import queue
import subprocess
import sys
import threading
import time

COMPATIBILITY = {"L": "Version8_2_16", "M": "Version8_3_27"}
PLATFORM_PROCESSES = ("1cv8.exe", "1cv8c.exe", "ibcmd.exe")


def snapshot(root):
    files = {}
    for base, _dirs, names in os.walk(root):
        for name in names:
            path = os.path.join(base, name)
            digest = hashlib.sha256()
            with open(path, "rb") as handle:
                for chunk in iter(lambda: handle.read(1 << 20), b""):
                    digest.update(chunk)
            files[os.path.relpath(path, root)] = [os.path.getsize(path), digest.hexdigest()]
    return files


def diff(before, after):
    changed = sorted(k for k in before.keys() & after.keys() if before[k] != after[k])
    return {"added": sorted(after.keys() - before.keys()),
            "removed": sorted(before.keys() - after.keys()), "changed": changed}


def platform_processes():
    out = subprocess.run(["tasklist", "/FO", "CSV", "/NH"], capture_output=True, text=True,
                         encoding="utf-8", errors="replace").stdout
    return sorted(line.split(",")[0].strip('"') for line in out.splitlines()
                  if line.split(",")[0].strip('"').lower() in PLATFORM_PROCESSES)


def owned_jobs(work_root):
    path = os.path.join(work_root, "owned-jobs")
    return sorted(os.listdir(path)) if os.path.isdir(path) else []


class Mcp:
    def __init__(self, argv, env, cwd):
        self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                     text=True, encoding="utf-8", bufsize=1)
        self.messages = queue.Queue()
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                self.messages.put(json.loads(line))
            except ValueError:
                pass
        self.messages.put(None)

    def request(self, method, params, timeout):
        self.next_id += 1
        request_id = self.next_id
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method,
                                          "params": params}, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            item = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            if item is None:
                raise RuntimeError("MCP process closed stdout")
            if item.get("id") == request_id:
                return item

    def notify(self, method, params=None):
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method,
                                          "params": params or {}}) + "\n")
        self.proc.stdin.flush()

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=15)
        except Exception:
            self.proc.kill()
            self.proc.wait()


def tool_payload(message):
    if "error" in message:
        return {"_jsonrpc_error": message["error"]}
    result = message.get("result", {})
    if result.get("structuredContent") is not None:
        return result["structuredContent"]
    for block in result.get("content", []):
        if block.get("type") == "text":
            try:
                return json.loads(block["text"])
            except ValueError:
                return {"_text": block["text"]}
    return {}


def evaluate(row, result):
    expected, problems = row["expected"], []
    status = result.get("status")
    allowed = expected.get("status_one_of") or [expected.get("status")]
    if status not in allowed:
        problems.append(f"status {status!r} not in expected {allowed!r}")
    if status in ("valid", "invalid"):
        if result.get("valid") is not (status == "valid"):
            problems.append(f"valid flag {result.get('valid')!r} disagrees with status")
        if result.get("cleanup_status") != "complete":
            problems.append(f"cleanup_status {result.get('cleanup_status')!r}")
        required = result.get("contexts_required") or []
        if not required or sorted(required) != sorted(result.get("contexts_checked") or []):
            problems.append("required contexts not fully checked")
        compatibility = (result.get("compiler") or {}).get("compatibility")
        if compatibility != COMPATIBILITY[row["fixture"]]:
            problems.append(f"compatibility {compatibility!r}")
    if "backend" in expected and result.get("backend") != expected["backend"]:
        problems.append(f"backend {result.get('backend')!r}")
    if "failure_code" in expected and (result.get("failure") or {}).get("code") != expected["failure_code"]:
        problems.append(f"failure code {(result.get('failure') or {}).get('code')!r}")
    if "position" in expected:
        coords = [[i.get("line"), i.get("column")] for i in result.get("issues", [])]
        if expected["position"] not in coords:
            problems.append(f"position {expected['position']} not in {coords}")
    return problems


def start_mcp(args, fixture, work, timeout_ms=120000):
    os.makedirs(os.path.join(work, "empty-source"), exist_ok=True)
    config = {"connections": {fixture: {"check_executor": {
        "program": args.executor,
        "args": ["--config", args.registry, "--profile", fixture],
        "timeout_ms": timeout_ms, "max_code_bytes": 2097152, "max_response_bytes": 262144,
        "max_stderr_bytes": 65536, "stop_grace_ms": 2000, "reap_timeout_ms": 1000,
        "max_concurrent": 1, "handles_snippet": True}}}}
    config_path = os.path.join(work, f"connections-{fixture}.json")
    with open(config_path, "w", encoding="utf-8") as handle:
        json.dump(config, handle, indent=1)
    env = dict(os.environ)
    env["BSL_ONEC_CONNECTIONS_FILE"] = config_path
    env[f"EXECUTOR_ACCEPTANCE_{fixture}_SOURCE"] = os.path.join(args.fixture_root, fixture, "ib")
    mcp = Mcp([args.app, "mcp", "serve", "--profile", "workspace",
               "--source-dir", os.path.join(work, "empty-source"),
               "--cache-dir", os.path.join(work, "cache"), "--mode", "stdio"], env, work)
    init = mcp.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                      "clientInfo": {"name": "win-acceptance", "version": "1"}}, 60)
    if "result" not in init:
        raise RuntimeError(f"MCP initialize failed: {init}")
    mcp.notify("notifications/initialized")
    return mcp


def process_ids(image):
    out = subprocess.run(["tasklist", "/FO", "CSV", "/NH", "/FI", f"IMAGENAME eq {image}"],
                         capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    return {int(line.split(",")[1].strip('"')) for line in out.splitlines() if line.startswith('"')}


def set_suspended(pid, suspended):
    import ctypes
    ntdll, kernel32 = ctypes.WinDLL("ntdll"), ctypes.WinDLL("kernel32")
    handle = kernel32.OpenProcess(0x0800, False, pid)  # PROCESS_SUSPEND_RESUME
    if not handle:
        raise RuntimeError(f"cannot open Designer {pid}")
    try:
        status = (ntdll.NtSuspendProcess if suspended else ntdll.NtResumeProcess)(handle)
        if status != 0:
            raise RuntimeError(f"NtSuspend/ResumeProcess({pid}) status {status:#x}")
    finally:
        kernel32.CloseHandle(handle)


class Pending:
    """One tools/call sent without waiting, so a stimulus can be applied meanwhile."""

    def __init__(self, mcp, arguments):
        mcp.next_id += 1
        self.mcp, self.id = mcp, mcp.next_id
        mcp.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": "tools/call",
                                         "params": {"name": "execute", "arguments": arguments}},
                                        ensure_ascii=False) + "\n")
        mcp.proc.stdin.flush()

    def cancel(self):
        self.mcp.notify("notifications/cancelled", {"requestId": self.id, "reason": "acceptance"})


def collect(mcp, timeout):
    """Read all responses that arrive within timeout, keyed by request id."""
    responses, deadline = {}, time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            item = mcp.messages.get(timeout=max(0.01, deadline - time.monotonic()))
        except queue.Empty:
            break
        if item is None:
            raise RuntimeError("MCP process closed stdout")
        if "id" in item:
            responses[item["id"]] = item
    return responses


def wait_for_designer(known, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        new = process_ids("1cv8.exe") - known
        if new:
            return sorted(new)
        time.sleep(0.2)
    raise RuntimeError("no Designer process appeared")


def run_lifecycle(args, scenario, rows, matrix_rows):
    """Apply one lifecycle stimulus on fixture L; return the list of problems."""
    by_id = {r["id"]: r for r in rows}
    followups = {r["id"]: r for r in matrix_rows}
    problems = []
    work = os.path.join(args.work, "lifecycle-" + scenario)
    os.makedirs(work, exist_ok=True)
    source = os.path.join(args.fixture_root, "L", "ib")
    work_root = json.load(open(args.registry, encoding="utf-8"))["profiles"]["L"]["work_root"]
    before, jobs_before = snapshot(source), owned_jobs(work_root)
    timeout_ms = 30000 if scenario in ("active-cancel", "active-timeout") else 120000
    mcp = start_mcp(args, "L", work, timeout_ms)
    designers = []
    try:
        if scenario in ("active-cancel", "active-timeout"):
            row = by_id["windows-L-" + scenario]
            call = Pending(mcp, row["arguments"])
            designers = wait_for_designer(process_ids("1cv8.exe"))
            for pid in designers:
                set_suspended(pid, True)
            if scenario == "active-cancel":
                call.cancel()
                answered = collect(mcp, 20)
                if call.id in answered:
                    problems.append(f"response after cancellation: {tool_payload(answered[call.id])}")
            else:
                answered = collect(mcp, timeout_ms / 1000 + 30)
                result = tool_payload(answered[call.id]) if call.id in answered else None
                code = ((result or {}).get("failure") or {}).get("code")
                if result is None or result.get("status") != "error" or \
                        code not in row["expected"]["failure_code_one_of"]:
                    problems.append(f"timeout answer: {result}")
            followup = followups[row["stimulus"]["followup_existing_request"]]
        else:  # queue-cancel
            active, peer = by_id["windows-L-queue-cancel-active"], by_id["windows-L-queue-cancel-peer"]
            call = Pending(mcp, active["arguments"])
            designers = wait_for_designer(process_ids("1cv8.exe"))
            for pid in designers:
                set_suspended(pid, True)
            executors = process_ids("bsl-native-check.exe")
            queued = Pending(mcp, peer["arguments"])
            time.sleep(3)
            if process_ids("bsl-native-check.exe") - executors:
                problems.append("queued peer started an executor")
            queued.cancel()
            answered = collect(mcp, 5)
            if queued.id in answered:
                problems.append(f"response for cancelled peer: {tool_payload(answered[queued.id])}")
            for pid in designers:
                set_suspended(pid, False)
            designers = []
            answered.update(collect(mcp, 120))
            result = tool_payload(answered[call.id]) if call.id in answered else None
            if result is None or result.get("status") != "valid":
                problems.append(f"resumed active answer: {result}")
            if queued.id in answered:
                problems.append("cancelled peer answered after resume")
            followup = followups[active["stimulus"]["followup_existing_request"]]
        for _ in range(60):
            if not platform_processes() and not set(owned_jobs(work_root)) - set(jobs_before):
                break
            time.sleep(0.5)
        if platform_processes():
            problems.append(f"platform processes left: {platform_processes()}")
        if set(owned_jobs(work_root)) - set(jobs_before):
            problems.append(f"owned jobs left: {sorted(set(owned_jobs(work_root)) - set(jobs_before))}")
    finally:
        for pid in designers:
            try:
                set_suspended(pid, False)
            except RuntimeError:
                pass
        mcp.close()
    # The follow-up proves that a normal request works again, so it uses the regular deadline.
    mcp = start_mcp(args, "L", work)
    try:
        result = tool_payload(mcp.request("tools/call", {"name": "execute", "arguments": followup["arguments"]}, 150))
    finally:
        mcp.close()
    problems += [f"followup {followup['id']}: {p} (failure {result.get('failure')})"
                 for p in evaluate(followup, result)]
    source_delta = diff(before, snapshot(source))
    if any(source_delta.values()):
        problems.append(f"source changed: {source_delta}")
    return problems


def main():
    parser = argparse.ArgumentParser()
    for flag in ("app", "executor", "registry", "fixture-root", "matrix", "out", "work", "sentinel-dir"):
        parser.add_argument("--" + flag, required=True)
    parser.add_argument("--fixtures", default="L,M")
    parser.add_argument("--rows", help="comma-separated row ids; default all rows of the fixtures")
    parser.add_argument("--lifecycle", default="",
                        help="comma-separated: active-cancel,active-timeout,queue-cancel (fixture L)")
    args = parser.parse_args()

    registry = json.load(open(args.registry, encoding="utf-8"))["profiles"]
    with open(args.matrix, encoding="utf-8") as handle:
        text = handle.read().replace("{SENTINELS}", json.dumps(args.sentinel_dir)[1:-1])
    loaded = json.loads(text)
    matrix, lifecycle_rows = loaded["rows"], loaded["lifecycle"]
    wanted = set(args.rows.split(",")) if args.rows else None
    fixtures = args.fixtures.split(",")
    os.makedirs(args.out, exist_ok=True)
    results_path = os.path.join(args.out, "results.jsonl")
    if not os.path.isdir(args.sentinel_dir):
        raise SystemExit(f"sentinel dir {args.sentinel_dir} is missing; marker absence would prove nothing")

    summary = {"started": time.strftime("%Y-%m-%dT%H:%M:%S"), "app_sha256": None, "executor_sha256": None,
               "pass": 0, "fail": 0, "rows": []}
    for key, path in (("app_sha256", args.app), ("executor_sha256", args.executor)):
        summary[key] = hashlib.sha256(open(path, "rb").read()).hexdigest()

    for fixture in fixtures:
        rows = [r for r in matrix if r["fixture"] == fixture and (wanted is None or r["id"] in wanted)]
        if not rows:
            continue
        source = os.path.join(args.fixture_root, fixture, "ib")
        work_root = registry[fixture]["work_root"]
        work = os.path.join(args.work, fixture)
        os.makedirs(work, exist_ok=True)
        mcp = None
        for row in rows:
            record = {"id": row["id"], "fixture": fixture, "problems": []}
            started = time.monotonic()
            try:
                if platform_processes():
                    raise RuntimeError(f"platform processes before row: {platform_processes()}")
                markers_before = sorted(os.listdir(args.sentinel_dir))
                jobs_before = owned_jobs(work_root)
                before = snapshot(source)
                if mcp is None:
                    mcp = start_mcp(args, fixture, work)
                message = mcp.request("tools/call", {"name": "execute", "arguments": row["arguments"]}, 150)
                result = tool_payload(message)
                record["result"] = result
                record["problems"] += evaluate(row, result)
                for _ in range(20):
                    if not platform_processes():
                        break
                    time.sleep(0.5)
                if platform_processes():
                    record["problems"].append(f"platform processes left: {platform_processes()}")
                source_delta = diff(before, snapshot(source))
                if any(source_delta.values()):
                    record["problems"].append(f"source changed: {source_delta}")
                new_jobs = sorted(set(owned_jobs(work_root)) - set(jobs_before))
                if new_jobs:
                    record["problems"].append(f"owned jobs left: {new_jobs}")
                new_markers = sorted(set(os.listdir(args.sentinel_dir)) - set(markers_before))
                if new_markers:
                    record["problems"].append(f"execution markers created: {new_markers}")
            except queue.Empty:
                record["problems"].append("MCP request timed out")
                mcp.close()
                mcp = None
            except Exception as error:  # a broken row must not stop the matrix
                record["problems"].append(f"{type(error).__name__}: {error}")
                if mcp is not None:
                    mcp.close()
                    mcp = None
            record["seconds"] = round(time.monotonic() - started, 2)
            record["verdict"] = "PASS" if not record["problems"] else "FAIL"
            summary["pass" if record["verdict"] == "PASS" else "fail"] += 1
            summary["rows"].append({"id": row["id"], "verdict": record["verdict"]})
            with open(results_path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, ensure_ascii=False) + "\n")
            print(f"{record['verdict']} {row['id']} {record['seconds']}s {record['problems'][:1]}", flush=True)
        if mcp is not None:
            mcp.close()

    for scenario in filter(None, args.lifecycle.split(",")):
        started = time.monotonic()
        try:
            problems = run_lifecycle(args, scenario, lifecycle_rows, matrix)
        except Exception as error:
            problems = [f"{type(error).__name__}: {error}"]
        record = {"id": "lifecycle-" + scenario, "fixture": "L", "problems": problems,
                  "seconds": round(time.monotonic() - started, 2),
                  "verdict": "PASS" if not problems else "FAIL"}
        summary["pass" if record["verdict"] == "PASS" else "fail"] += 1
        summary["rows"].append({"id": record["id"], "verdict": record["verdict"]})
        with open(results_path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")
        print(f"{record['verdict']} {record['id']} {record['seconds']}s {problems[:1]}", flush=True)

    summary["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as handle:
        json.dump(summary, handle, ensure_ascii=False, indent=1)
    print(f"PASS {summary['pass']} FAIL {summary['fail']}")
    return 0 if summary["fail"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
