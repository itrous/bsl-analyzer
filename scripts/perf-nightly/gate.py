"""The release gate (plan docs/plans/release-gate-graph-ready.md): a base tag
against a candidate commit on the ERP clone of the manifest, measured as the
release protocol measured it, and its noise calibration.

`gate` measures series A (pairs base/candidate in turn: graph readiness, the
resolve and sweep calls) and series B (the candidate with the references
tool), judges every metric by its Δ against `gate-noise.json`, checks the
absolute ceilings and the byte equality of the answers. A run during which the
machine spent more than two cores on work outside the run does not count and is
measured again. `gate-calibrate` measures the base alone and writes
`gate-noise.json`, separate from the nightly `noise.json`.
"""

from __future__ import annotations

import fcntl
import json
import math
import os
import re
import shutil
import subprocess
import sys
import uuid

import corpus
import perf_nightly
import procs
import scenarios
import verdict
from clock import RealClock
from common import (
    HARNESS_SCHEMA, MISSING_REASONS, Data, HarnessError, git, median, read_json, repo_root, sha256_bytes, write_json,
)

GATE_SCHEMA = 1
CORPUS = "erp"
SCENARIO = "mcp-graph"
TEMP = "cold"
# The calls and the pauses of the release protocol, one to one, so that its
# absolute limits keep the setting they were taken in.
MODULE_REL = "src/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl"
SYMBOL_1 = "ОбщегоНазначения.ОбщийМодуль"
SYMBOL_2 = "ОбщегоНазначения.ЗначениеРеквизитаОбъекта"
EXTRA_RESOLVE = ("ЗначениеРеквизитаОбъекта", "ЗначениеРеквизит", "ОбщегоНазначения")
EXTRA_TOP = 50
CALL_TIMEOUT_S = 1800.0
SWEEP_MAX_FILES = 5000
PAUSE_READY_S = 20.0
PAUSE_SETTLE_S = 60.0
PAUSE_SWEEP_S = 30.0
RUNS = 3
PRECONDITION_MEM_KB = 13 * 1024 * 1024
PRECONDITION_LOAD1 = 2.0
PRECONDITION_POLL_S = 30.0
BUSY_BUDGET_MIN = 60.0
RESOLVE_ANSWERS = ("c-graph-resolve", *[f"c-graph-resolve-extra-{i}" for i in range(len(EXTRA_RESOLVE))],
                   "d-graph-resolve-3")
SWEEP_ANSWER = "f-diagnostics-workspace"
LABEL_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
METRICS = {m.key: m for m in verdict.GATE_METRICS}
READY_ONLY_METRICS = ("graph_ready_s", "graph_ready_cpu_s")
REFS_METRICS = tuple(verdict.GATE_REFS_LIMITS_S)
# Burns its share of CPU seconds of its own and exits.
BURN_CODE = (
    "import sys, time\n"
    "end = float(sys.argv[1])\n"
    "while time.process_time() < end:\n"
    "    pass\n"
)
# A busy loop until killed: load outside the run.
SPIN_CODE = "while True:\n    pass\n"
# Sleeps, then becomes the server: the PID and the environment stay the same.
DELAY_CODE = (
    "import os, sys, time\n"
    "time.sleep(float(sys.argv[1]))\n"
    "os.execv(sys.argv[2], sys.argv[2:])\n"
)


class MachineBusy(Exception):
    """The budget for waiting on a quiet machine is spent."""


class Machine:
    """What the gate reads of the machine it measures on."""

    def __init__(self, proc_root: str = "/proc"):
        self.proc_root = proc_root

    def busy_s(self) -> float:
        return procs.machine_busy_s(self.proc_root)

    def quiet(self) -> dict:
        """The precondition of a run: enough memory and no load on start."""
        mem = _proc_field(os.path.join(self.proc_root, "meminfo"), "MemAvailable")
        mem_kb = int(mem.split()[0]) if mem else None
        load1 = procs.machine_load(self.proc_root)["load1"]
        ok = mem_kb is not None and mem_kb >= PRECONDITION_MEM_KB and load1 is not None \
            and load1 <= PRECONDITION_LOAD1
        return {"ok": ok, "mem_available_kb": mem_kb, "load1": load1}

    def fingerprint(self) -> dict:
        rustc = subprocess.run(["rustc", "-V"], capture_output=True, text=True, check=True).stdout.strip()
        return {
            "cpu_model": _proc_field(os.path.join(self.proc_root, "cpuinfo"), "model name"),
            # As `nproc`: the CPUs this process and the server it starts may run on.
            "nproc": len(os.sched_getaffinity(0)),
            "mem_total": perf_nightly.mem_total_gib(_proc_field(os.path.join(self.proc_root, "meminfo"),
                                                                 "MemTotal")),
            "kernel": os.uname().release,
            "rustc": rustc,
        }


def _proc_field(path: str, name: str) -> str | None:
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            if line.startswith(name):
                return line.split(":", 1)[1].strip()
    return None


def foreign_cores(busy_s: float, own_cpu_s: float, wall_s: float) -> float:
    """Cores the machine spent outside the run over its readiness window."""
    excess = busy_s - own_cpu_s
    if wall_s > 0:
        return excess / wall_s
    return math.inf if excess > 0 else 0.0


# -- source and noise ----------------------------------------------------------------


def check_source(data: Data) -> dict:
    """The ERP clone of the manifest, at its commit, with no tracked file changed."""
    manifest = corpus.load_manifest(data.path("corpora.toml"))
    entry = manifest["corpora"].get(CORPUS)
    if entry is None:
        raise HarnessError("manifest_invalid", f"no `{CORPUS}` corpus in the manifest")
    root = entry["source_repo"]
    if not os.path.isdir(root):
        raise HarnessError("source_mismatch", "the source clone is missing")
    if git("rev-parse", "HEAD", cwd=root, check=False) != entry["commit"]:
        raise HarnessError("source_mismatch", "the source clone is not at the manifest commit")
    if git("status", "--porcelain", "--untracked-files=no", cwd=root, check=False):
        raise HarnessError("source_mismatch", "tracked files of the source clone are changed")
    config = subprocess.run(["git", "show", f"HEAD:{corpus.CONFIG_NAME}"], cwd=root, capture_output=True)
    if config.returncode != 0:
        raise HarnessError("source_mismatch", f"the source clone has no committed {corpus.CONFIG_NAME}")
    return {"root": root, "commit": entry["commit"], "config_sha256": sha256_bytes(config.stdout)}


def fingerprint(machine, source: dict) -> dict:
    return {**machine.fingerprint(), "harness_schema": HARNESS_SCHEMA, "gate_schema": GATE_SCHEMA,
            "source_commit": source["commit"], "config_sha256": source["config_sha256"]}


def read_noise(data: Data) -> dict | None:
    path = data.path("gate-noise.json")
    return read_json(path) if os.path.isfile(path) else None


def noise_stale(noise: dict | None, print_: dict) -> str | None:
    if noise is None:
        return "no gate-noise.json"
    if noise.get("gate_schema") != GATE_SCHEMA:
        return "gate schema changed"
    return verdict.noise_stale(noise, {CORPUS: print_})


def noise_of(noise: dict, key: str) -> float | None:
    return verdict.lookup_noise(noise, CORPUS, SCENARIO, TEMP, key)


def calibrated_median(noise: dict, key: str) -> float | None:
    entry = (noise.get("entries") or {}).get(verdict.noise_key(CORPUS, SCENARIO, TEMP, key)) or {}
    return median(entry.get("medians") or [])


# -- builds -------------------------------------------------------------------------


def resolve_candidate(data: Data, spec: str) -> str:
    """The full SHA of a commit-ish of this repository, fetched into the build
    clone when the clone does not know it (a commit that was never pushed)."""
    sha = git("rev-parse", "--verify", f"{spec}^{{commit}}", cwd=repo_root())
    clone = data.path("build")
    # A substituted builder brings its own binaries and no clone.
    if os.path.isdir(os.path.join(clone, ".git")) and \
            not git("rev-parse", "--verify", "-q", f"{sha}^{{commit}}", cwd=clone, check=False):
        git("fetch", repo_root(), sha, cwd=clone)
    return sha


def build_pair(data: Data, base_tag: str, candidate: str | None, builder) -> tuple[dict, dict | None]:
    """(base, candidate) binaries; `builder(ref, label)` returns a binary path."""
    if not perf_nightly.TAG_RE.fullmatch(base_tag):
        raise HarnessError("base_invalid", f"{base_tag}: the base is a vX.Y.Z tag")
    cand_label = candidate if candidate and perf_nightly.TAG_RE.fullmatch(candidate) else "rc"
    if builder is not None:
        sha = resolve_candidate(data, candidate) if candidate else None
        base = perf_nightly.load_binary(builder(base_tag, base_tag))
        cand = perf_nightly.load_binary(builder(sha, cand_label)) if sha else None
        return base, cand
    os.makedirs(data.root, exist_ok=True)
    with open(data.path("build.lock"), "w") as lock:
        # Builds of other commands share the clone; wait for them, never check out under them.
        fcntl.flock(lock, fcntl.LOCK_EX)
        perf_nightly.build_clone(data)
        sha = resolve_candidate(data, candidate) if candidate else None
        base = perf_nightly.load_binary(perf_nightly.build_ref(data, base_tag)[0])
        cand = perf_nightly.load_binary(perf_nightly.build_ref(data, sha, cand_label)[0]) if sha else None
    return base, cand


# -- one run --------------------------------------------------------------------------


def _answer(result, key: str):
    structured = (result or {}).get("structuredContent") or {}
    inner = structured.get("result")
    if isinstance(inner, dict) and key in inner:
        return inner[key]
    return structured.get(key)


def _write_answer(run_dir: str, name: str, value) -> None:
    path = os.path.join(run_dir, "answers", f"{name}.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as fh:
        fh.write(json.dumps(value, sort_keys=True, ensure_ascii=False).encode("utf-8"))


def _spawn(code: str, args: list, env: dict):
    return subprocess.Popen([sys.executable, "-c", code, *map(str, args)], env=env,
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def burn_count(burn_s: float, noise: dict) -> int:
    """Burners for X CPU seconds: each one's share fits into half the
    calibrated readiness, so it is done before the graph is."""
    ready = calibrated_median(noise, "graph_ready_s") or 0.0
    return max(4, math.ceil(burn_s / (0.5 * ready))) if ready > 0 else 4


class _Live:
    """What one run started: the server, CPU burners of the run (they carry its
    marker) and spinners outside it (they do not)."""

    def __init__(self):
        self.server = None
        self.burners: list = []
        self.spinners: list = []

    def reap_burners(self, clock) -> float:
        """Wait for every burner to exit; their CPU from the kernel's own account."""
        fds = {}
        try:
            for proc in self.burners:
                fds.update(scenarios._pidfd(proc))
            if not clock.wait_exit(fds, scenarios.READY_TIMEOUT_S):
                raise scenarios.NotHeld("control_failed", "a CPU burner outlived readiness")
        finally:
            for fd in fds.values():
                os.close(fd)
        total = 0.0
        for proc in self.burners:
            _, status, usage = os.wait4(proc.pid, 0)
            proc.returncode = os.waitstatus_to_exitcode(status)
            total += usage.ru_utime + usage.ru_stime
        self.burners = []
        return total

    def stop_spinners(self) -> None:
        self.spinners = _kill(self.spinners)

    def close(self) -> None:
        self.spinners = _kill(self.spinners)
        self.burners = _kill(self.burners)


def _kill(processes: list) -> list:
    for proc in processes:
        if proc.poll() is None:
            proc.kill()
        proc.wait()
    return []


def _measure(ctx, series: str, ready_only: bool, controls: dict, machine, sampler, live: _Live) -> None:
    m = ctx.out["metrics"]
    run_dir = ctx.run_dir
    extra = ("--enable-tool", "references") if series == "B" else ()
    prefix = ()
    if controls.get("control_start_delay_s"):
        prefix = (sys.executable, "-c", DELAY_CODE, str(controls["control_start_delay_s"]))
        ctx.applied.add("control_start_delay_s")
    busy_from = machine.busy_s()
    run = live.server = scenarios.McpRun(ctx, extra_args=extra, argv_prefix=prefix)
    if controls.get("control_burn_cpu_s"):
        k = controls["burners"]
        share = controls["control_burn_cpu_s"] / k
        live.burners = [_spawn(BURN_CODE, [share], ctx.env) for _ in range(k)]
        ctx.applied.add("control_burn_cpu_s")
    if controls.get("control_foreign_burn"):
        env = {key: value for key, value in ctx.env.items() if key != procs.MARKER}
        live.spinners = [_spawn(SPIN_CODE, [], env) for _ in range(controls["control_foreign_burn"])]
        ctx.applied.add("control_foreign_burn")
    run.start()
    if "graph" not in run.actions:
        raise scenarios.NotHeld("backend_failed", "the server has no graph tool")
    if series == "B" and not ready_only and "references" not in run.actions:
        raise scenarios.NotHeld("tool_missing", "the server has no references tool")

    def on_ready(backend: str) -> None:
        if backend != "graph":
            return
        m["graph_ready_s"] = ctx.clock.now() - run.spawned_at
        burned = 0.0
        exclude = [proc.pid for proc in live.burners]
        if live.burners:
            burned = live.reap_burners(ctx.clock)
        sampler.sample()
        m["graph_ready_cpu_s"] = round(sampler.cpu_s(exclude) + burned, 3)
        m["foreign_cores"] = foreign_cores(machine.busy_s() - busy_from, m["graph_ready_cpu_s"],
                                           m["graph_ready_s"])
        live.stop_spinners()
        if m["foreign_cores"] > verdict.GATE_FOREIGN_CORES:
            raise scenarios.NotHeld("foreign_load", f"{m['foreign_cores']:.2f} cores of foreign load")

    m["ready_s"] = run.wait_ready(on_ready)
    if "graph_ready_s" not in m:
        raise scenarios.NotHeld("backend_failed", "the graph never reported ready")
    if ready_only:
        return

    def call(name: str, tool: str, arguments: dict, answer: tuple | None = None, metric: str | None = None):
        elapsed, result, error = run.tool(tool, arguments, CALL_TIMEOUT_S)
        if error is None:
            error = scenarios.check_tool_result(result)
        value = _answer(result, answer[1]) if answer is not None and error is None else None
        # An answer without its list would make two missing answers compare equal.
        if answer is not None and error is None and not isinstance(value, list):
            error = f"no `{answer[1]}` list in the answer"
        ctx.call({"id": name, "elapsed": elapsed, "ok": error is None, "error": error})
        if error is not None:
            raise scenarios.NotHeld("call_failed", f"{name}: {error}")
        if metric is not None:
            m[metric] = elapsed
        if answer is not None:
            _write_answer(run_dir, answer[0], value)

    ctx.clock.sleep(PAUSE_READY_S)
    call("symbol_info_1", "symbol_info", {"symbol": SYMBOL_1})
    call("graph_resolve", "graph", {"action": "resolve", "query": SYMBOL_1},
         ("c-graph-resolve", "candidates"), "graph_resolve_s")
    for index, query in enumerate(EXTRA_RESOLVE):
        call(f"graph_resolve_extra_{index}", "graph", {"action": "resolve", "query": query, "top": EXTRA_TOP},
             (f"c-graph-resolve-extra-{index}", "candidates"))
    if series == "B":
        call("references_1", "references", {"symbol": SYMBOL_1}, metric="references_1_s")
    call("diagnostics_file", "diagnostics", {"action": "file", "path": MODULE_REL})
    ctx.clock.sleep(PAUSE_SETTLE_S)
    call("symbol_info_2_same", "symbol_info", {"symbol": SYMBOL_1})
    call("symbol_info_3_other", "symbol_info", {"symbol": SYMBOL_2})
    if series == "B":
        call("references_2_same", "references", {"symbol": SYMBOL_1}, metric="references_2_same_s")
        call("references_3_other", "references", {"symbol": SYMBOL_2}, metric="references_3_other_s")
    call("graph_resolve_2_same", "graph", {"action": "resolve", "query": SYMBOL_1})
    call("graph_resolve_3_other", "graph", {"action": "resolve", "query": SYMBOL_2},
         ("d-graph-resolve-3", "candidates"))
    if series == "A":
        call("diagnostics_workspace", "diagnostics", {"action": "workspace", "max_files": SWEEP_MAX_FILES},
             (SWEEP_ANSWER, "aggregates"), "diagnostics_workspace_s")
        ctx.clock.sleep(PAUSE_SWEEP_S)


def run_pass(ctx, series: str, ready_only: bool, *, record: dict, controls: dict | None = None,
             machine=None) -> dict:
    """One run of the gate on `ctx` (root = the ERP clone): series A or B,
    up to readiness only with `ready_only`. Writes and returns the run's
    record (`<run dir>/r.json`); `record` carries its identity (binary,
    fingerprint, k, runs)."""
    machine = machine or Machine()
    controls = controls or {}
    out = ctx.out
    out.setdefault("exit_code", None)
    load_before = procs.machine_load()
    started_at = verdict.now_iso()
    began = ctx.clock.now()
    sampler = procs.Sampler(ctx.run_id)
    live = _Live()
    try:
        _measure(ctx, series, ready_only, controls, machine, sampler, live)
        out["held"], out["reason"] = True, None
    except scenarios.NotHeld as exc:
        out["held"], out["reason"] = False, exc.reason
        if exc.detail:
            ctx.error(f"{exc.reason}: {exc.detail}")
    except (OSError, HarnessError) as exc:
        out["held"], out["reason"] = False, "server_failed"
        ctx.error(f"harness: {type(exc).__name__}: {exc}")
    finally:
        live.close()
        if live.server is not None:
            scenarios.stop_mcp(ctx, [live.server], sampler)
        sampled = sampler.stop()
        out["metrics"].update(hwm_kb=sampled["hwm_kb"], rss_peak_kb=sampled["rss_peak_kb"],
                              cpu_s=sampled["cpu_s"])
    final = procs.stop(ctx.run_id, ctx.clock)
    found = {**record, "schema": HARNESS_SCHEMA, "run_id": ctx.run_id, "series": series,
             "metrics": out["metrics"], "calls": out["calls"], "errors": list(out["errors"]),
             "held": out["held"], "reason": out["reason"], "exit_code": out.get("exit_code"),
             "controls": {name: controls[name] for name in sorted(ctx.applied)} or None,
             "load_before": load_before, "load_after": procs.machine_load(), "started_at": started_at,
             "duration_s": ctx.clock.now() - began, "stop": {"first": out.get("stop"), "final": final}}
    if final["left"]:
        found.update(held=False, reason="stop_incomplete")
        found["errors"].append(f"stop_incomplete: {len(final['left'])} tracked process(es) survive")
    # The graph cache of a cold run is gigabytes and is never read again.
    shutil.rmtree(os.path.join(ctx.run_dir, "cache"), ignore_errors=True)
    write_json(os.path.join(ctx.run_dir, "r.json"), found)
    return found


# -- a sequence of runs ---------------------------------------------------------------


class Runner:
    """Runs in their order, each after the quiet-machine precondition; a run
    with foreign load is measured again in its place. Waiting and the runs
    that did not count share one budget."""

    def __init__(self, clock, machine, source: dict, print_: dict, budget_min: float):
        self.clock = clock
        self.machine = machine
        self.source = source
        self.print = print_
        self.budget_s = budget_min * 60.0
        self.spent_s = 0.0
        self.launches: list[dict] = []

    def wait_quiet(self) -> None:
        while not self.machine.quiet()["ok"]:
            if self.spent_s >= self.budget_s:
                raise MachineBusy()
            self.clock.sleep(PRECONDITION_POLL_S)
            self.spent_s += PRECONDITION_POLL_S

    def run(self, run_dir: str, binary: dict, role: str, series: str, k: int, ready_only: bool,
            controls: dict, record_path: str | None = None) -> dict:
        attempt = 0
        while True:
            self.wait_quiet()
            attempt += 1
            this_dir = run_dir if attempt == 1 else f"{run_dir}.{attempt}"
            shutil.rmtree(this_dir, ignore_errors=True)
            os.makedirs(this_dir)
            run_id = f"gate-{binary['label']}-{binary['sha'][:12]}-{os.path.basename(this_dir)}-" \
                     f"{uuid.uuid4().hex[:8]}"
            env = procs.child_env(run_id, None, {"BSL_CACHE_DIR": os.path.join(this_dir, "cache"),
                                                 "BSL_MCP_DAEMON_LOG": os.path.join(this_dir, "daemon.log")})
            ctx = scenarios.Ctx(binary=binary["path"], root=self.source["root"], snap=self.source["root"],
                                env=env, run_dir=this_dir, run_id=run_id, clock=self.clock, points={},
                                snap_meta={})
            identity = {"binary": {"label": binary["label"], "sha": binary["sha"], "sha256": binary["sha256"]},
                        "role": role, "k": k, "runs": RUNS, "attempt": attempt, "env_fingerprint": self.print,
                        "corpus": {"id": CORPUS, "commit": self.source["commit"]}, "temp": TEMP}
            record = run_pass(ctx, series, ready_only, record=identity, controls=controls, machine=self.machine)
            self.launches.append({"run": os.path.basename(this_dir), "dir": this_dir, "role": role,
                                  "series": series, "k": k, "held": record["held"], "reason": record["reason"],
                                  "foreign_cores": record["metrics"].get("foreign_cores")})
            if record["reason"] != "foreign_load":
                if record_path is not None:
                    write_json(record_path, record)
                return record
            self.spent_s += record["duration_s"]
            if self.spent_s >= self.budget_s:
                raise MachineBusy()


# -- judging --------------------------------------------------------------------------


def side(records: list, metric) -> tuple[str, float | None]:
    """`verdict.side`, except that a side cut short (`machine_busy`) which
    already holds a failed run is `failed`, not `missing`."""
    state, value = verdict.side(records, metric)
    if state == "missing" and any(not r["held"] and r.get("reason") not in MISSING_REASONS for r in records):
        return "failed", None
    return state, value


def side_outcome(base: str, cand: str) -> str | None:
    """The states of both sides, mapped by the gate itself: a failed or invalid
    candidate blocks; any other side that is not valid leaves no decision."""
    if cand in ("failed", "invalid"):
        return "block"
    if base != "valid" or cand != "valid":
        return "incomplete"
    return None


def _delta(base: float, cand: float) -> float:
    if base:
        # Not `cand / base - 1`: 42 / 40 - 1 rounds above 0.05 and an exact +5 % would block.
        return (cand - base) / base
    return 0.0 if cand == base else math.inf


def rule_rows(base: list, cand: list, noise: dict, ready_only: bool) -> list[dict]:
    keys = READY_ONLY_METRICS if ready_only else (*verdict.GATE_WALL, *verdict.GATE_DECISIVE)
    rows = []
    for key in keys:
        metric = METRICS[key]
        b_state, b_value = side(base, metric)
        c_state, c_value = side(cand, metric)
        nu = noise_of(noise, key)
        wall = key in verdict.GATE_WALL
        row = {"metric": key, "kind": "wall" if wall else "decisive",
               "base": {"state": b_state, "value": b_value}, "candidate": {"state": c_state, "value": c_value},
               "delta": None, "noise": nu,
               "limit": max(verdict.GATE_BAR, nu) if wall and nu is not None else verdict.GATE_BAR}
        outcome = side_outcome(b_state, c_state)
        if outcome is None:
            row["delta"] = _delta(b_value, c_value)
            outcome = verdict.gate_outcome(key, row["delta"], nu)
        row["outcome"] = outcome
        rows.append(row)
    return rows


def confirm_row(row: dict, base: list, cand: list) -> None:
    metric = METRICS[row["metric"]]
    b_state, b_value = side(base, metric)
    c_state, c_value = side(cand, metric)
    row["confirm"] = {"base": {"state": b_state, "value": b_value},
                      "candidate": {"state": c_state, "value": c_value}, "delta": None}
    outcome = side_outcome(b_state, c_state)
    if outcome is None:
        row["confirm"]["delta"] = _delta(b_value, c_value)
        outcome = verdict.gate_outcome(row["metric"], row["delta"], row["noise"], row["confirm"]["delta"])
    row["outcome"] = outcome


def _ceiling(key: str, groups: list[list], value, limit: float) -> dict:
    metric = verdict.Metric(key, key)
    states = [side(records, metric)[0] for records in groups]
    worst = next((s for s in ("failed", "invalid", "missing", "n/a") if s in states), "valid")
    row = {"metric": key, "kind": "ceiling", "limit": limit, "candidate": {"state": worst, "value": None}}
    outcome = side_outcome("valid", row["candidate"]["state"])
    if outcome is None:
        row["candidate"]["value"] = value()
        outcome = "block" if row["candidate"]["value"] > limit else "ok"
    row["outcome"] = outcome
    return row


def ceiling_rows(series_a: list, series_b: list) -> list[dict]:
    rows = [_ceiling("hwm_kb", [series_a, series_b],
                     lambda: max(r["metrics"]["hwm_kb"] for r in series_a + series_b),
                     verdict.GATE_HWM_LIMIT_KB)]
    for key, limit in verdict.GATE_REFS_LIMITS_S.items():
        rows.append(_ceiling(key, [series_b], lambda key=key: side(series_b, verdict.Metric(key, key))[1], limit))
    return rows


def _read_answer(run_dir: str, name: str) -> bytes | None:
    path = os.path.join(run_dir, "answers", f"{name}.json")
    if not os.path.isfile(path):
        return None
    with open(path, "rb") as fh:
        return fh.read()


def _aggregates(data: bytes | None) -> dict:
    items = json.loads(data) if data else None
    return {str(a.get("code")): {"count": a.get("count"), "files_affected": a.get("files_affected"),
                                 "severity": a.get("severity")}
            for a in items or [] if isinstance(a, dict)}


def _sweep_delta(base: bytes | None, cand: bytes | None) -> dict:
    """The sweep of the candidate against the base, by diagnostic code. Only
    reported: the correctness of diagnostics is the project's tests and the
    CHANGELOG, the gate judges time and memory."""
    left, right = _aggregates(base), _aggregates(cand)
    return {
        "new": [{"code": code, "count": right[code]["count"], "files_affected": right[code]["files_affected"]}
                for code in sorted(right.keys() - left.keys())],
        "vanished": [{"code": code, "count": left[code]["count"], "files_affected": left[code]["files_affected"]}
                     for code in sorted(left.keys() - right.keys())],
        "changed": [{"code": code, "base": left[code], "candidate": right[code]}
                    for code in sorted(left.keys() & right.keys()) if left[code] != right[code]],
    }


def compare_answers(pairs: list[tuple[dict, dict]]) -> dict:
    """Byte equality of every resolve answer of each pair (base, candidate) of
    launches that held, and the control that the answers can differ at all: the
    first candidate run has two different non-empty extra resolves (`control`
    None: no candidate run held, the control was not made). The sweep
    aggregates do not decide: they are listed as new, vanished and changed
    codes."""
    compared, sweep = [], {"new": [], "vanished": [], "changed": []}
    for base, cand in pairs:
        if not (base["held"] and cand["held"]):
            continue
        differ = [name for name in RESOLVE_ANSWERS
                  if _read_answer(base["dir"], name) != _read_answer(cand["dir"], name)]
        delta = _sweep_delta(_read_answer(base["dir"], SWEEP_ANSWER), _read_answer(cand["dir"], SWEEP_ANSWER))
        for kind, items in delta.items():
            # A code whose values differ between pairs keeps a row per value: the spread is not hidden.
            sweep[kind] += [i for i in items if i not in sweep[kind]]
        compared.append({"pair": f"{base['run']}/{cand['run']}", "differ": differ,
                         **{kind: [i["code"] for i in items] for kind, items in delta.items()}})
    first = next((cand for _, cand in pairs if cand["held"]), None)
    control = None
    if first is not None:
        a, b = (json.loads(_read_answer(first["dir"], f"c-graph-resolve-extra-{i}") or b"null") for i in (0, 1))
        control = bool(a) and bool(b) and a != b
    if any(p["differ"] for p in compared):
        outcome = "block"
    elif not compared or not control:
        outcome = "incomplete"
    else:
        outcome = "ok"
    return {"pairs": compared, "control": control, "outcome": outcome,
            **{f"{kind}_codes": sorted(sweep[kind], key=lambda i: i["code"]) for kind in sweep}}


# -- the gate ---------------------------------------------------------------------------


def _controls(args) -> dict:
    return {name: getattr(args, name) for name in ("control_burn_cpu_s", "control_start_delay_s",
                                                   "control_foreign_burn") if getattr(args, name, None)}


def cmd_gate(args, clock=None, builder=None, machine=None) -> int:
    clock = clock or RealClock()
    machine = machine or Machine()
    data = Data(args.data_dir)
    if not LABEL_RE.fullmatch(args.label or ""):
        raise HarnessError("label_invalid", f"{args.label}: letters, digits, `.`, `_`, `-`")
    budget = _budget(args)
    if not math.isfinite(budget) or budget < 0:
        raise HarnessError("budget_invalid", f"--busy-budget-min {budget}: a finite number of minutes, ≥ 0")
    os.makedirs(data.root, exist_ok=True)
    with open(data.path("gate.lock"), "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise HarnessError("gate_busy", "another gate holds this data dir") from None
        out_dir = data.path("gates", args.label)
        if os.path.exists(out_dir):
            raise HarnessError("gate_exists", f"gates/{args.label} exists; pass a new --label")
        os.makedirs(out_dir)
        summary = {"verdict": None, "reasons": [], "rows": [], "answers": None, "ceilings": [], "runs": [],
                   "noise": None, "controls": _controls(args) or None, "protocol": protocol(args),
                   "base": {"label": args.base, "sha": None},
                   "candidate": {"label": args.candidate, "sha": None},
                   "started_at": verdict.now_iso(), "finished_at": None}
        try:
            _gate(args, clock, builder, machine, data, out_dir, summary)
        except (HarnessError, OSError, subprocess.CalledProcessError, ValueError) as exc:
            reason = exc.reason if isinstance(exc, HarnessError) else "harness_error"
            detail = exc.detail if isinstance(exc, HarnessError) else f"{type(exc).__name__}: {exc}"
            summary.update(verdict="incomplete", reasons=[f"{reason}: {detail}" if detail else reason])
        summary["finished_at"] = verdict.now_iso()
        write_json(os.path.join(out_dir, "summary.json"), summary)
        write_report(out_dir, summary)
        print(f"gate={summary['verdict']} reasons={'; '.join(summary['reasons']) or '-'} out={out_dir}")
        return {"pass": 0, "probe-ok": 0, "block": 1}.get(summary["verdict"], 2)


def protocol(args) -> dict:
    return {"gate_schema": GATE_SCHEMA, "corpus": CORPUS, "scenario": SCENARIO, "runs": RUNS,
            "ready_only": bool(getattr(args, "ready_only", False)),
            "symbols": [SYMBOL_1, SYMBOL_2], "module": MODULE_REL, "extra_resolve": list(EXTRA_RESOLVE),
            "extra_top": EXTRA_TOP, "sweep_max_files": SWEEP_MAX_FILES,
            "pauses_s": [PAUSE_READY_S, PAUSE_SETTLE_S, PAUSE_SWEEP_S], "call_timeout_s": CALL_TIMEOUT_S,
            "bar": verdict.GATE_BAR, "foreign_cores_limit": verdict.GATE_FOREIGN_CORES,
            "hwm_limit_kb": verdict.GATE_HWM_LIMIT_KB, "refs_limits_s": verdict.GATE_REFS_LIMITS_S,
            "busy_budget_min": _budget(args)}


def _budget(args) -> float:
    value = getattr(args, "busy_budget_min", None)
    return BUSY_BUDGET_MIN if value is None else value


def _gate(args, clock, builder, machine, data: Data, out_dir: str, summary: dict) -> None:
    source = check_source(data)
    print_ = fingerprint(machine, source)
    noise = read_noise(data)
    stale = noise_stale(noise, print_)
    summary["noise"] = {"status": stale or "current",
                        "values": {key: noise_of(noise, key) for key in METRICS} if noise else None}
    if stale is not None:
        summary.update(verdict="incomplete", reasons=[f"noise_stale: {stale}"])
        return
    base, cand = build_pair(data, args.base, args.candidate, builder)
    summary["base"] = {"label": base["label"], "sha": base["sha"]}
    summary["candidate"] = {"label": cand["label"], "sha": cand["sha"]}
    ready_only = bool(args.ready_only)
    controls = _controls(args)
    if "control_burn_cpu_s" in controls:
        controls["burners"] = burn_count(controls["control_burn_cpu_s"], noise)
    spin = {k: v for k, v in controls.items() if k == "control_foreign_burn"}
    runner = Runner(clock, machine, source, print_, _budget(args))
    runs = os.path.join(out_dir, "runs")
    reasons: list[str] = []
    series: dict[str, list] = {"base": [], "candidate": [], "confirm_base": [], "confirm_candidate": [], "B": []}
    pairs: list = []

    def round_(first: int, base_key: str, cand_key: str) -> None:
        for k in range(1, RUNS + 1):
            index = first + k - 1
            series[base_key].append(runner.run(os.path.join(runs, f"A-b{index}"), base, "base", "A", k,
                                               ready_only, spin))
            b_launch = runner.launches[-1]
            series[cand_key].append(runner.run(os.path.join(runs, f"A-c{index}"), cand, "candidate", "A", k,
                                               ready_only, controls))
            pairs.append((b_launch, runner.launches[-1]))

    rows: list[dict] = []
    ceilings: list[dict] = []
    try:
        round_(1, "base", "candidate")
        rows = rule_rows(series["base"], series["candidate"], noise, ready_only)
        wall = next(r for r in rows if r["metric"] in verdict.GATE_WALL)
        if wall["outcome"] == "confirm":
            round_(RUNS + 1, "confirm_base", "confirm_candidate")
            confirm_row(wall, series["confirm_base"], series["confirm_candidate"])
        if not ready_only:
            for k in range(1, RUNS + 1):
                series["B"].append(runner.run(os.path.join(runs, f"B-c{k}"), cand, "candidate", "B", k,
                                              False, controls))
    except MachineBusy:
        reasons.append("machine_busy")
        if not rows:
            rows = rule_rows(series["base"], series["candidate"], noise, ready_only)
    answers = None
    if not ready_only:
        ceilings = ceiling_rows(series["candidate"] + series["confirm_candidate"], series["B"])
        answers = compare_answers(pairs)
        if answers["outcome"] == "block":
            reasons.append("answers_differ")
        elif answers["control"] is False:
            reasons.append("answers_control")
    judged = rows + ceilings + ([{"metric": "answers", "outcome": answers["outcome"]}] if answers else [])
    result = verdict.gate_verdict(judged, reasons)
    if ready_only and result["verdict"] == "pass":
        result["verdict"] = "probe-ok"
    summary.update(verdict=result["verdict"], reasons=result["reasons"], counts=result["counts"], rows=rows,
                   ceilings=ceilings, answers=answers, runs=runner.launches,
                   busy_spent_s=runner.spent_s)


# -- report ---------------------------------------------------------------------------


def _pct(value) -> str:
    return "-" if value is None else f"{value * 100:+.1f}%"


def _side_text(side: dict) -> str:
    return verdict.fmt(side["value"]) if side["state"] == "valid" else side["state"]


def _aggregate_text(aggregate: dict) -> str:
    return f"{aggregate['count']}/{aggregate['files_affected']}/{aggregate['severity']}"


def write_report(out_dir: str, summary: dict) -> None:
    lines = [f"# Гейт {os.path.basename(out_dir)}", "",
             f"- вердикт: **{summary['verdict']}**",
             f"- причины: {'; '.join(summary['reasons']) or '-'}",
             f"- база: {summary['base']['label']} {summary['base']['sha'] or '-'}",
             f"- кандидат: {summary['candidate']['label']} {summary['candidate']['sha'] or '-'}",
             f"- калибровка: {(summary.get('noise') or {}).get('status', '-')}",
             f"- контроли: {json.dumps(summary['controls'], ensure_ascii=False) if summary['controls'] else '-'}",
             ""]
    rows = summary.get("rows") or []
    if rows:
        lines += ["## Правило", "", "| метрика | род | база | кандидат | Δ | ν | предел | Δ′ | исход |",
                  "|---|---|---|---|---|---|---|---|---|"]
        for row in rows:
            noise = "-" if row["noise"] is None else f"{row['noise'] * 100:.1f}%"
            confirm = _pct((row.get("confirm") or {}).get("delta"))
            lines.append(f"| {row['metric']} | {row['kind']} | {_side_text(row['base'])} | "
                         f"{_side_text(row['candidate'])} | {_pct(row['delta'])} | {noise} | "
                         f"{row['limit'] * 100:.1f}% | {confirm} | {row['outcome']} |")
        wall = next((r for r in rows if r["kind"] == "wall"), None)
        if wall is not None and wall["noise"] is not None:
            lines += ["", f"Стеночная метрика решает от Δ > max(5 %, ν) = {wall['limit'] * 100:.1f} % "
                          f"(ν = {wall['noise'] * 100:.1f} %); меньший сдвиг стены от шума не отличим.", ""]
    ceilings = summary.get("ceilings") or []
    if ceilings:
        lines += ["## Потолки", "", "| метрика | кандидат | предел | исход |", "|---|---|---|---|"]
        lines += [f"| {r['metric']} | {_side_text(r['candidate'])} | {verdict.fmt(r['limit'])} | {r['outcome']} |"
                  for r in ceilings]
        lines.append("")
    answers = summary.get("answers")
    if answers:
        lines += ["## Ответы", "", f"- исход: {answers['outcome']}; контроль: {answers['control']}"]
        lines += [f"- {p['pair']}: {', '.join(p['differ']) or 'равны'}" for p in answers["pairs"]]
        lines += ["", "Агрегаты свипа вердикт не меняют: корректность диагностик судят тесты проекта и "
                      "CHANGELOG «Изменения поведения», гейт судит время и память.", ""]
        for title, key, head, cells in (
                ("Новые коды", "new_codes", "| код | count | files_affected |",
                 lambda i: f"| {i['code']} | {i['count']} | {i['files_affected']} |"),
                ("Исчезнувшие коды", "vanished_codes", "| код | count | files_affected |",
                 lambda i: f"| {i['code']} | {i['count']} | {i['files_affected']} |"),
                ("Изменённые коды", "changed_codes", "| код | было (count/files_affected/severity) | стало |",
                 lambda i: f"| {i['code']} | {_aggregate_text(i['base'])} | {_aggregate_text(i['candidate'])} |")):
            lines += [f"## {title}", ""]
            items = answers.get(key) or []
            if items:
                lines += [head, "|---|---|---|", *map(cells, items)]
            else:
                lines.append("-")
            lines.append("")
    runs = summary.get("runs") or []
    if runs:
        lines += ["## Прогоны", "", "| прогон | сторона | серия | засчитан | причина | foreign_cores |",
                  "|---|---|---|---|---|---|"]
        lines += [f"| {r['run']} | {r['role']} | {r['series']} | {str(r['held']).lower()} | {r['reason'] or '-'} | "
                  f"{verdict.fmt(r['foreign_cores'])} |" for r in runs]
        lines.append("")
    with open(os.path.join(out_dir, "report.md"), "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


# -- calibration ------------------------------------------------------------------------


def cmd_gate_calibrate(args, clock=None, builder=None, machine=None) -> int:
    clock = clock or RealClock()
    machine = machine or Machine()
    data = Data(args.data_dir)
    count = verdict.NOISE_SERIES if args.series is None else args.series
    if count < verdict.NOISE_SERIES:
        # Fewer series medians make the spread a poor, down to zero, estimate of noise.
        raise HarnessError("series_invalid", f"--series {count}: the noise needs {verdict.NOISE_SERIES} series")
    os.makedirs(data.root, exist_ok=True)
    with open(data.path("gate.lock"), "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise HarnessError("gate_busy", "another gate holds this data dir") from None
        source = check_source(data)
        print_ = fingerprint(machine, source)
        base, _ = build_pair(data, args.base, None, builder)
        root = data.path("gate-cal", f"{base['label']}-{base['sha'][:12]}")
        groups, busy = [], []
        for index in range(1, count + 1):
            series_dir = os.path.join(root, f"s{index}")
            shutil.rmtree(series_dir, ignore_errors=True)
            runner = Runner(clock, machine, source, print_, BUSY_BUDGET_MIN)
            records = []
            try:
                for k in range(1, RUNS + 1):
                    records.append(runner.run(os.path.join(series_dir, f"r{k}"), base, "base", "A", k, False, {},
                                              record_path=os.path.join(series_dir, f"r{k}.json")))
            except MachineBusy:
                busy.append(index)
            groups.append({(CORPUS, SCENARIO, TEMP): records})
        entries = verdict.build_noise(groups, lambda sdir: verdict.GATE_METRICS)
        values = {key.rsplit("/", 1)[1]: entry["noise"] for key, entry in entries.items()}
        written = bool(values) and all(v is not None for v in values.values())
        if written:
            write_json(data.path("gate-noise.json"), {
                "schema": verdict.NOISE_SCHEMA, "harness_schema": HARNESS_SCHEMA, "gate_schema": GATE_SCHEMA,
                "fingerprints": {CORPUS: print_},
                "binary": {"label": base["label"], "sha": base["sha"], "sha256": base["sha256"]},
                "created_at": verdict.now_iso(), "entries": entries})
        print(f"gate-calibrate binary={base['label']} series={count} "
              f"busy={','.join(f's{i}' for i in busy) or '-'} written={str(written).lower()} " +
              " ".join(f"nu.{key}={verdict.fmt(value)}" for key, value in sorted(values.items())))
        return 0 if written else 2
