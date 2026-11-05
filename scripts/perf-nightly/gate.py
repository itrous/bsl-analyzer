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

The second base is `baseline.json`: the best confirmed value of each metric,
judged by `C > R·(1+ν)` on the full profile and protocol of the record. Memory
without a measured ν is held to the bar instead, and never counts as improved. A
second round measures an improvement over it; neither the gate nor the night
writes the file. `record_night` measures develop against it with the same runs.

The CPU time is observed against both bases and decides nothing. New features
buy no time and no memory: the sweep and the CHANGELOG enter no judgement.
"""

from __future__ import annotations

import fcntl
import glob
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
    HARNESS_SCHEMA, HERE, MISSING_REASONS, Data, HarnessError, git, median, read_json, repo_root, sha256_bytes,
    write_json,
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
BASELINE_SCHEMA = 1
BASELINE_PATH = os.path.join(HERE, "baseline.json")
EVIDENCE_FIELDS = ("kind", "summary", "role", "series", "runs", "confirmation")
MEMORY_A_KEY = "hwm_A_kb"
HWM_METRIC = verdict.Metric("hwm_kb", "hwm_kb")
# Seconds per run of a record night with no earlier one on this profile: the
# longest held run of each series of the release gate of 2026-11-01.
RECORD_RUN_ESTIMATE_S = {"A": 319.58177912799874, "B": 401.89415255299537}
# The one binary of the noise a record lacks: a release that has the references tool.
RECORD_CALIBRATION_BASE = "v0.4.2"
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


class WindowExhausted(Exception):
    """The next run would not end inside the window of the night."""


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


def check_source(data: Data, manifest_path: str | None = None) -> dict:
    """The ERP clone of the manifest, at its commit, with no tracked file changed."""
    manifest = corpus.load_manifest(manifest_path or data.path("corpora.toml"))
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
    that did not count share one budget. `fits(series)`, when given, says
    whether a run of the series still ends inside a window; it is asked while
    the machine is waited for and after that, and a run that does not fit never
    starts."""

    def __init__(self, clock, machine, source: dict, print_: dict, budget_min: float, fits=None):
        self.clock = clock
        self.machine = machine
        self.source = source
        self.print = print_
        self.budget_s = budget_min * 60.0
        self.spent_s = 0.0
        self.launches: list[dict] = []
        self.fits = fits

    def check_window(self, series: str) -> None:
        if self.fits is not None and not self.fits(series):
            raise WindowExhausted()

    def wait_quiet(self, series: str) -> None:
        while not self.machine.quiet()["ok"]:
            if self.spent_s >= self.budget_s:
                raise MachineBusy()
            self.check_window(series)
            self.clock.sleep(PRECONDITION_POLL_S)
            self.spent_s += PRECONDITION_POLL_S
        self.check_window(series)

    def run(self, run_dir: str, binary: dict, role: str, series: str, k: int, ready_only: bool,
            controls: dict, record_path: str | None = None) -> dict:
        attempt = 0
        while True:
            self.wait_quiet(series)
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


# Work metrics whose growth over their limit is forgiven where the wall time fell by more.
TRADED_METRICS = ("graph_resolve_s", "diagnostics_workspace_s")


def repaid_outcome(delta: float, wall_delta: float | None) -> tuple[str, str]:
    """What becomes of a block of a work metric, whichever limit it passed. It
    stays unless the wall time of the same comparison fell by at least as much:
    the machine has idle cores, the user waits for the wall."""
    if wall_delta is not None and -wall_delta >= delta:
        return "ok", "growth_traded"
    return "block", "growth_not_repaid"


def traded_outcome(key: str, delta: float, wall_delta: float | None, noise: float | None) -> tuple[str, str | None]:
    """Outcome and the ground of the rule for a work metric of `TRADED_METRICS`
    against the previous release. The noise of the metric is held to the bar as
    for any work metric."""
    outcome = verdict.gate_outcome(key, delta, noise)
    return (outcome, None) if outcome != "block" else repaid_outcome(delta, wall_delta)


def observation_row(key: str, base: list, cand: list, nu: float | None) -> dict:
    """The numbers of an observed metric against the previous release. What is
    missing is told in the row and leaves the gate as it is."""
    b_state, b_value = side(base, METRICS[key])
    c_state, c_value = side(cand, METRICS[key])
    missing = [f"{name}: {state}" for name, state in (("base", b_state), ("candidate", c_state)) if state != "valid"]
    return {"metric": key, "kind": "observation", "base": {"state": b_state, "value": b_value},
            "candidate": {"state": c_state, "value": c_value},
            "delta": None if missing else _delta(b_value, c_value), "noise": nu, "limit": None,
            "outcome": "observation", "reason": "; ".join(missing + ([] if nu is not None else ["no_noise"])) or None}


def rule_rows(base: list, cand: list, noise: dict, ready_only: bool) -> list[dict]:
    keys = READY_ONLY_METRICS if ready_only else (*verdict.GATE_WALL, *verdict.GATE_OBSERVED, *verdict.GATE_DECISIVE)
    rows = []
    wall_delta = None
    for key in keys:
        if key in verdict.GATE_OBSERVED:
            rows.append(observation_row(key, base, cand, noise_of(noise, key)))
            continue
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
            if key in TRADED_METRICS:
                outcome, row["basis"] = traded_outcome(key, row["delta"], wall_delta, nu)
                row["wall_delta"] = wall_delta
            else:
                outcome = verdict.gate_outcome(key, row["delta"], nu)
        if wall:
            wall_delta = row["delta"]
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


def _worst_state(groups: list[list], metric) -> str:
    states = [side(records, metric)[0] for records in groups]
    return next((s for s in ("failed", "invalid", "missing", "n/a") if s in states), "valid")


def peak(groups: list[list]) -> tuple[str, float | None]:
    """(state, value) of the peak memory over the given series taken together."""
    state = _worst_state(groups, HWM_METRIC)
    return state, max(r["metrics"]["hwm_kb"] for records in groups for r in records) if state == "valid" else None


def _ceiling(key: str, groups: list[list], value: float | None, limit: float) -> dict:
    """`value` is what the held runs measured. Above the limit it blocks whatever
    the other runs did: a round that did not finish cannot erase a measured excess."""
    state = _worst_state(groups, verdict.Metric(key, key))
    if value is not None and value > limit:
        return {"metric": key, "kind": "ceiling", "limit": limit, "candidate": {"state": "valid", "value": value},
                "outcome": "block"}
    return {"metric": key, "kind": "ceiling", "limit": limit,
            "candidate": {"state": state, "value": value if state == "valid" else None},
            "outcome": side_outcome("valid", state) or "ok"}


def ceiling_rows(series_a: list, series_b: list, confirm: tuple[list, list] | None = None) -> list[dict]:
    """Peak memory over every run of the candidate; a reference over its worst
    round median, since the medians of two rounds do not mix."""
    groups_a = [series_a] + ([confirm[0]] if confirm else [])
    groups_b = [series_b] + ([confirm[1]] if confirm else [])
    groups = groups_a + groups_b
    peaks = [r["metrics"].get("hwm_kb") for records in groups for r in records if r["held"]]
    rows = [_ceiling("hwm_kb", groups, max(filter(verdict._number, peaks), default=None), verdict.GATE_HWM_LIMIT_KB)]
    for key, limit in verdict.GATE_REFS_LIMITS_S.items():
        medians = [side(b, verdict.Metric(key, key))[1] for b in groups_b]
        rows.append(_ceiling(key, groups_b, max((m for m in medians if m is not None), default=None), limit))
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


# -- the record ------------------------------------------------------------------------


def measured_protocol() -> dict:
    """The part of the protocol that decides whether a run compares with a record."""
    return {"corpus": CORPUS, "scenario": SCENARIO, "runs": RUNS, "symbols": [SYMBOL_1, SYMBOL_2],
            "module": MODULE_REL, "extra_resolve": list(EXTRA_RESOLVE), "extra_top": EXTRA_TOP,
            "sweep_max_files": SWEEP_MAX_FILES, "pauses_s": [PAUSE_READY_S, PAUSE_SETTLE_S, PAUSE_SWEEP_S],
            "call_timeout_s": CALL_TIMEOUT_S, "temp": TEMP, "build": "release"}


def _differing(expected: dict, measured: dict) -> list[str]:
    return sorted(k for k in expected.keys() | measured.keys() if expected.get(k) != measured.get(k))


def _text(value) -> bool:
    return isinstance(value, str) and bool(value)


def noise_value(noise) -> float | None:
    """The tolerance of a `noise` object; anything but a finite number ≥ 0 is none."""
    value = noise.get("value") if isinstance(noise, dict) else None
    return value if verdict._number(value) and math.isfinite(value) and value >= 0 else None


def noise_absent(noise) -> bool:
    """No tolerance was given at all: no object, no `value` or a null one. A given
    one that `noise_value` refuses is something else: a broken record."""
    return noise is None or (isinstance(noise, dict) and noise.get("value") is None)


class Baseline:
    """`baseline.json`, read once per run. What is wrong with it is a named
    reason of the record, never an exception: the old rule still gets judged."""

    def __init__(self, path: str):
        self.path = path
        self.sha256 = None
        self.profile: dict = {}
        self.protocol: dict = {}
        self.metrics: dict = {}
        self.reasons: list[str] = []
        try:
            with open(path, "rb") as fh:
                raw = fh.read()
        except OSError:
            self.reasons.append("baseline_missing")
            return
        self.sha256 = sha256_bytes(raw)
        try:
            doc = json.loads(raw)
        except ValueError:
            self.reasons.append("baseline_invalid:not_json")
            return
        # `True` and `1.0` equal 1 in Python, and neither is a known schema.
        if not isinstance(doc, dict) or type(doc.get("schema")) is not int or doc["schema"] != BASELINE_SCHEMA:
            self.reasons.append("baseline_invalid:schema")
            return
        for part in ("profile", "protocol", "metrics"):
            if not isinstance(doc.get(part), dict) or not doc[part]:
                self.reasons.append(f"baseline_invalid:{part}")
        if self.reasons:
            return
        self.profile, self.protocol, self.metrics = doc["profile"], doc["protocol"], doc["metrics"]
        self.reasons += [f"baseline_invalid:{key}" for key in self.metrics if key not in verdict.GATE_RECORD]

    def invalid(self, key: str) -> str | None:
        """Why this entry, whatever its tolerance, is not a record of its metric."""
        entry = self.metrics.get(key)
        unit, aggregate = verdict.GATE_RECORD[key]
        if not isinstance(entry, dict):
            return f"baseline_invalid:{key}"
        value = entry.get("value")
        evidence = entry.get("evidence")
        if not verdict._number(value) or not math.isfinite(value) or value <= 0 \
                or entry.get("unit") != unit or entry.get("aggregate") != aggregate \
                or not _text(entry.get("record_sha")) or not _text(entry.get("measured_at")) \
                or not isinstance(evidence, dict) or any(field not in evidence for field in EVIDENCE_FIELDS):
            return f"baseline_invalid:{key}"
        return None

    def problem(self, key: str) -> str | None:
        """Why this record cannot judge its metric. Memory without a measured
        tolerance still can: `record_row` holds it to the bar."""
        invalid = self.invalid(key)
        if invalid:
            return invalid
        noise = self.metrics[key].get("noise")
        if noise_value(noise) is not None or (key == HWM_METRIC.key and noise_absent(noise)):
            return None
        return f"record_no_noise:{key}"

    def incomparable(self, print_: dict | None) -> list[str]:
        """The reasons no number of this measurement compares with any record."""
        reasons = list(self.reasons)
        if self.profile and print_ is not None:
            diff = _differing(self.profile, print_)
            reasons += [f"record_profile_mismatch:{','.join(diff)}"] if diff else []
        if self.protocol:
            diff = _differing(self.protocol, measured_protocol())
            reasons += [f"record_protocol_mismatch:{','.join(diff)}"] if diff else []
        return reasons


def baseline_path(args) -> str:
    return getattr(args, "baseline", None) or BASELINE_PATH


def new_round() -> dict:
    """The runs of one round apart, so that no median mixes two rounds."""
    return {"base": [], "candidate": [], "B": [], "names": {"base": [], "candidate": [], "B": []}}


def _round_side(key: str, round_: dict) -> tuple[str, float | None]:
    aggregate = verdict.GATE_RECORD[key][1]
    if aggregate == "max_AB":
        return peak([round_["candidate"], round_["B"]])
    return side(round_["candidate" if aggregate == "median_A" else "B"], verdict.Metric(key, key))


def _worst_cell(cells: list[dict]) -> dict:
    rank = {"ok": 0, "incomplete": 1, "block": 2}
    return max(cells, key=lambda c: (rank[c["outcome"]], -math.inf if c["delta"] is None else c["delta"]))


def _record_wall_delta(baseline: Baseline, round_: dict) -> float | None:
    """The wall of this round against its own record; None where that is no comparison."""
    key = verdict.GATE_WALL[0]
    state, value = _round_side(key, round_)
    return _delta(baseline.metrics[key]["value"], value) if state == "valid" and not baseline.problem(key) else None


def _observed_record_row(row: dict, baseline: Baseline, incomparable: list[str], rounds: list[dict]) -> dict:
    """An observed metric against its record: what can be shown of it over the
    rounds, the largest value on top. Whatever is wrong with its entry or its noise
    is told in the row and leaves the record as it is."""
    key = row["metric"]
    invalid = "; ".join(incomparable) or baseline.invalid(key)
    for round_ in rounds:
        state, value = _round_side(key, round_)
        row["rounds"].append({"candidate": {"state": state, "value": value}, "improved": False, "outcome": "observation",
                              "delta": _delta(row["record"]["value"], value) if state == "valid" and not invalid else None})
    measured = [c for c in row["rounds"] if c["candidate"]["state"] == "valid"]
    top = max(measured, key=lambda c: c["candidate"]["value"], default=row["rounds"][0] if row["rounds"] else None)
    if top is not None:
        row.update(candidate=top["candidate"], delta=top["delta"])
    reasons = [invalid or baseline.problem(key), None if measured else f"record_missing:{key}"]
    row.update(outcome="observation", reason="; ".join(r for r in reasons if r) or None)
    return row


def record_row(key: str, baseline: Baseline, incomparable: list[str], rounds: list[dict]) -> dict:
    """One record metric over the rounds: each round is compared on its own and
    the row is the worst of them, so a later round cannot erase a block, and a wall
    that repaid the growth in one round repays nothing in another."""
    unit, aggregate = verdict.GATE_RECORD[key]
    entry = baseline.metrics.get(key)
    entry = entry if isinstance(entry, dict) else {}
    nu = noise_value(entry.get("noise"))
    row = {"metric": key, "kind": "record", "unit": unit, "aggregate": aggregate,
           "record": {"value": entry.get("value"), "sha": entry.get("record_sha"),
                      "measured_at": entry.get("measured_at"), "evidence": entry.get("evidence")},
           "candidate": {"state": None, "value": None}, "delta": None, "noise": nu, "limit": None, "limit_basis": None,
           "outcome": "incomplete", "reason": None, "rounds": []}
    if key in verdict.GATE_OBSERVED:
        return _observed_record_row(row, baseline, incomparable, rounds)
    blocked = "; ".join(incomparable) or baseline.problem(key)
    if blocked:
        row["reason"] = blocked
        return row
    record = entry["value"]
    # The bar is a margin, not a measured noise: `noise` stays null and nothing counts as improved.
    margin = verdict.GATE_BAR if nu is None else nu
    row.update(limit=verdict.record_limit(record, margin), limit_basis="bar_no_noise" if nu is None else "noise")
    for round_ in rounds:
        state, value = _round_side(key, round_)
        cell = {"candidate": {"state": state, "value": value}, "delta": None, "improved": False,
                "outcome": side_outcome("valid", state)}
        if cell["outcome"] is None:
            cell["delta"] = _delta(record, value)
            cell["outcome"] = verdict.record_outcome(value, record, margin)
            cell["improved"] = nu is not None and verdict.record_improved(value, record, nu)
            if key in TRADED_METRICS:
                cell["wall_delta"] = _record_wall_delta(baseline, round_)
                if cell["outcome"] == "block":
                    cell["outcome"], cell["basis"] = repaid_outcome(cell["delta"], cell["wall_delta"])
        row["rounds"].append(cell)
    if not row["rounds"]:
        row["reason"] = f"record_missing:{key}"
        return row
    worst = _worst_cell(row["rounds"])
    row.update(candidate=worst["candidate"], delta=worst["delta"], outcome=worst["outcome"],
               basis=worst.get("basis"), wall_delta=worst.get("wall_delta"))
    if row["outcome"] == "incomplete":
        row["reason"] = f"record_missing:{key}"
    return row


def _unique(items: list[str]) -> list[str]:
    return list(dict.fromkeys(items))


def record_section(baseline: Baseline, print_: dict | None, rounds: list[dict], keys, probe: bool) -> dict:
    """`summary.record`: its rows and its own verdict. A probe does not vouch for anything."""
    incomparable = baseline.incomparable(print_)
    rows = [record_row(key, baseline, incomparable, rounds) for key in keys]
    reasons = _unique([part for r in rows if r["outcome"] == "incomplete" for part in r["reason"].split("; ")])
    result = verdict.gate_verdict([r for r in rows if r["outcome"] == "block"], _unique(incomparable + reasons))
    found = "probe-ok" if probe and result["verdict"] == "pass" else result["verdict"]
    return {"baseline_path": baseline.path, "baseline_sha256": baseline.sha256, "profile": print_,
            "baseline_profile": baseline.profile, "protocol": baseline.protocol, "entries": baseline.metrics,
            "rows": rows, "verdict": found, "reasons": result["reasons"]}


def memory_base_row(baseline: Baseline, print_: dict | None, rounds: list[dict]) -> dict:
    """Peak of series A of the candidate against peak of series A of the base,
    each round on its own: B has references, which the base does not measure."""
    entry = baseline.metrics.get("hwm_kb")
    noise = entry.get("noise") if isinstance(entry, dict) else None
    nu = noise_value(noise.get("base_A_noise")) if isinstance(noise, dict) else None
    row = {"metric": MEMORY_A_KEY, "kind": "memory_base", "base": {"state": None, "value": None},
           "candidate": {"state": None, "value": None}, "delta": None, "noise": nu, "limit": None,
           "outcome": "incomplete", "reason": None, "rounds": []}
    blocked = "; ".join(baseline.incomparable(print_)) or (None if nu is not None else f"record_no_noise:{MEMORY_A_KEY}")
    if blocked:
        row["reason"] = blocked
        return row
    for round_ in rounds:
        b_state, b_value = peak([round_["base"]])
        c_state, c_value = peak([round_["candidate"]])
        cell = {"base": {"state": b_state, "value": b_value}, "candidate": {"state": c_state, "value": c_value},
                "delta": None, "limit": None, "outcome": side_outcome(b_state, c_state)}
        if cell["outcome"] is None:
            cell["delta"] = _delta(b_value, c_value)
            cell["limit"] = verdict.record_limit(b_value, nu)
            cell["outcome"] = verdict.record_outcome(c_value, b_value, nu)
        row["rounds"].append(cell)
    worst = _worst_cell(row["rounds"])
    row.update(base=worst["base"], candidate=worst["candidate"], delta=worst["delta"], limit=worst["limit"],
               outcome=worst["outcome"])
    if row["outcome"] == "incomplete":
        row["reason"] = f"memory_base_missing:{MEMORY_A_KEY}"
    return row


def second_base_judgement(section: dict, memory: dict | None) -> tuple[list[dict], list[str]]:
    """Rows and reasons the second base adds to the verdict of the whole gate;
    `record:` tells its block of a metric from the block of the old rule."""
    judged = [{"metric": f"record:{r['metric']}", "outcome": "block"} for r in section["rows"]
              if r["outcome"] == "block"]
    reasons = [r for r in section["reasons"] if not r.startswith("block: ")]
    if memory is not None and memory["outcome"] == "block":
        judged.append({"metric": "memory_base", "outcome": "block"})
    elif memory is not None and memory["outcome"] == "incomplete":
        reasons += memory["reason"].split("; ")
    return judged, _unique(reasons)


def _improved_metrics(section: dict) -> list[str]:
    return [r["metric"] for r in section["rows"] if any(c["improved"] for c in r["rounds"])]


def _names(round_: dict, aggregate: str) -> list[str]:
    names = round_["names"]
    if aggregate == "max_AB":
        return names["candidate"] + names["B"]
    return names["candidate" if aggregate == "median_A" else "B"]


def _round_held(round_: dict) -> bool:
    return all(len(round_[part]) == RUNS and all(r["held"] for r in round_[part]) for part in ("base", "candidate", "B"))


def record_proposals(section: dict, baseline: Baseline, rounds: list[dict], answers: dict | None, label: str,
                     sha: str, print_: dict) -> list[dict]:
    """The metrics that beat their record by more than the noise in both full
    rounds, each at the worse of the two. `baseline.json` is moved by a commit
    of the person who reads these."""
    if len(rounds) != 2 or not all(_round_held(r) for r in rounds) or (answers or {}).get("outcome") != "ok":
        return []
    found = []
    for row in section["rows"]:
        if len(row["rounds"]) != 2 or not all(c["improved"] for c in row["rounds"]):
            continue
        values = [c["candidate"]["value"] for c in row["rounds"]]
        aggregate = row["aggregate"]
        found.append({
            "metric": row["metric"], "unit": row["unit"], "aggregate": aggregate, "value": max(values),
            "rounds": values, "previous_value": row["record"]["value"], "record_sha": sha,
            "measured_at": verdict.now_iso(), "noise": baseline.metrics[row["metric"]]["noise"], "profile": print_,
            "evidence": {"kind": "confirmed", "summary": f"data/gates/{label}/summary.json", "role": "candidate",
                         "series": {"median_A": "A", "median_B": "B", "max_AB": "A+B"}[aggregate],
                         "runs": _names(rounds[0], aggregate), "confirmation": _names(rounds[1], aggregate)}})
    return found


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
                   "record": None, "memory_base": None, "record_proposals": [], "record_confirmation": None,
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
    measured = {k: v for k, v in measured_protocol().items() if k not in ("temp", "build")}
    return {"gate_schema": GATE_SCHEMA, **measured, "ready_only": bool(getattr(args, "ready_only", False)),
            "bar": verdict.GATE_BAR, "foreign_cores_limit": verdict.GATE_FOREIGN_CORES,
            "hwm_limit_kb": verdict.GATE_HWM_LIMIT_KB, "refs_limits_s": verdict.GATE_REFS_LIMITS_S,
            "busy_budget_min": _budget(args)}


def _budget(args) -> float:
    value = getattr(args, "busy_budget_min", None)
    return BUSY_BUDGET_MIN if value is None else value


def _gate(args, clock, builder, machine, data: Data, out_dir: str, summary: dict) -> None:
    ready_only = bool(args.ready_only)
    controls = _controls(args)
    probe = ready_only or bool(controls)
    keys = READY_ONLY_METRICS if ready_only else tuple(verdict.GATE_RECORD)
    baseline = Baseline(baseline_path(args))
    summary["record"] = record_section(baseline, None, [], keys, probe)
    source = check_source(data)
    print_ = fingerprint(machine, source)
    summary["record"] = record_section(baseline, print_, [], keys, probe)
    noise = read_noise(data)
    stale = noise_stale(noise, print_)
    summary["noise"] = {"status": stale or "current",
                        "values": {key: noise_of(noise, key) for key in METRICS} if noise else None}
    if stale is not None:
        summary.update(verdict="incomplete", reasons=[f"noise_stale: {stale}", *baseline.incomparable(print_)])
        return
    base, cand = build_pair(data, args.base, args.candidate, builder)
    summary["base"] = {"label": base["label"], "sha": base["sha"]}
    summary["candidate"] = {"label": cand["label"], "sha": cand["sha"]}
    if "control_burn_cpu_s" in controls:
        controls["burners"] = burn_count(controls["control_burn_cpu_s"], noise)
    spin = {k: v for k, v in controls.items() if k == "control_foreign_burn"}
    runner = Runner(clock, machine, source, print_, _budget(args))
    runs = os.path.join(out_dir, "runs")
    reasons: list[str] = []
    rounds: list[dict] = []
    pairs: list = []

    def measure_round() -> None:
        found = new_round()
        rounds.append(found)

        def run(part: str, name: str, binary: dict, role: str, series: str, k: int, run_controls: dict) -> dict:
            found[part].append(runner.run(os.path.join(runs, name), binary, role, series, k, ready_only,
                                          run_controls))
            launch = runner.launches[-1]
            found["names"][part].append(launch["run"])
            return launch

        first = (len(rounds) - 1) * RUNS
        for k in range(1, RUNS + 1):
            b_launch = run("base", f"A-b{first + k}", base, "base", "A", k, spin)
            pairs.append((b_launch, run("candidate", f"A-c{first + k}", cand, "candidate", "A", k, controls)))
        if not ready_only:
            for k in range(1, RUNS + 1):
                run("B", f"B-c{first + k}", cand, "candidate", "B", k, controls)

    rows: list[dict] = []
    ceilings: list[dict] = []
    confirmation = {"requested": False, "triggers": [], "complete": False}
    try:
        measure_round()
        rows = rule_rows(rounds[0]["base"], rounds[0]["candidate"], noise, ready_only)
        wall = next(r for r in rows if r["metric"] in verdict.GATE_WALL)
        confirm_wall = wall["outcome"] == "confirm"
        improved = [] if probe else _improved_metrics(record_section(baseline, print_, rounds, keys, probe))
        if confirm_wall or improved:
            confirmation.update(requested=True, triggers=(["wall"] if confirm_wall else []) + improved)
            measure_round()
            confirmation["complete"] = all(_round_held(r) for r in rounds)
            if confirm_wall:
                confirm_row(wall, rounds[1]["base"], rounds[1]["candidate"])
    except MachineBusy:
        reasons.append("machine_busy")
        if not rows:
            rows = rule_rows(rounds[0]["base"], rounds[0]["candidate"], noise, ready_only)
    answers = None
    if not ready_only:
        ceilings = ceiling_rows(rounds[0]["candidate"], rounds[0]["B"],
                                (rounds[1]["candidate"], rounds[1]["B"]) if len(rounds) > 1 else None)
        answers = compare_answers(pairs)
        if answers["outcome"] == "block":
            reasons.append("answers_differ")
        elif answers["control"] is False:
            reasons.append("answers_control")
    section = record_section(baseline, print_, rounds, keys, probe)
    memory = None if ready_only else memory_base_row(baseline, print_, rounds)
    second_rows, second_reasons = second_base_judgement(section, memory)
    judged = rows + ceilings + ([{"metric": "answers", "outcome": answers["outcome"]}] if answers else []) + second_rows
    result = verdict.gate_verdict(judged, reasons + second_reasons)
    if probe and result["verdict"] == "pass":
        result["verdict"] = "probe-ok"
    proposals = [] if probe else record_proposals(section, baseline, rounds, answers, os.path.basename(out_dir),
                                                  cand["sha"], print_)
    summary.update(verdict=result["verdict"], reasons=result["reasons"], counts=result["counts"], rows=rows,
                   ceilings=ceilings, answers=answers, runs=runner.launches, busy_spent_s=runner.spent_s,
                   record=section, memory_base=memory, record_proposals=proposals, record_confirmation=confirmation)


# -- the night against the record -------------------------------------------------------


def _record_attempt_dir(data: Data, name: str) -> tuple[str, bool]:
    """(directory, it holds a finished measurement). A directory left without a
    summary is not a measurement; it stays, and the attempt gets a suffix."""
    attempt = 1
    while True:
        path = data.path("gates", name if attempt == 1 else f"{name}-{attempt}")
        if not os.path.exists(path):
            return path, False
        if os.path.isfile(os.path.join(path, "summary.json")):
            return path, True
        attempt += 1


def record_estimates(data: Data, date: str, print_: dict) -> dict:
    """Seconds per run of each series: the longest run of its held three on the
    latest earlier record night of this profile, or `RECORD_RUN_ESTIMATE_S`."""
    found = dict(RECORD_RUN_ESTIMATE_S)
    pending = set(found)
    for path in sorted(glob.glob(os.path.join(glob.escape(data.path("gates")), "record-night-*", "summary.json")),
                       reverse=True):
        summary = read_json(path)
        if summary.get("date", date) >= date or (summary.get("record") or {}).get("profile") != print_:
            continue
        for series in sorted(pending):
            durations = (summary.get("durations") or {}).get(series)
            if durations:
                found[series] = max(durations)
                pending.discard(series)
    return found


def _baseline_reference(path: str) -> dict:
    """The record file as an issue may name it: the repository's own path and
    commit; a path outside the repository is not published."""
    try:
        root = repo_root()
    except HarnessError:
        return {"path": os.path.basename(path), "commit": None}
    relative = os.path.relpath(os.path.abspath(path), root)
    return {"path": os.path.basename(path) if relative.startswith("..") else relative,
            "commit": git("rev-parse", "HEAD", cwd=root, check=False) or None}


def _record_result(summary_path: str | None, summary: dict | None, reasons: list[str], reused: bool = False) -> dict:
    return {"verdict": summary["verdict"] if summary else "incomplete",
            "record_verdict": (summary.get("record") or {}).get("verdict") if summary else None,
            "reasons": summary["reasons"] if summary else reasons, "summary": summary_path,
            "report": f"{os.path.dirname(summary_path)}/report.md" if summary_path else None, "reused": reused}


def _public_summary(out_dir: str) -> str:
    return f"data/gates/{os.path.basename(out_dir)}/summary.json"


def record_night(args, clock, data: Data, date: str, cand: dict, window, machine=None) -> dict:
    """Three runs of series A and three of series B of an already built develop
    against the numbers of the record: no base, no second round, no proposals.
    Whatever stands in the way is the reason of an incomplete record, and the
    warm set of the night goes on regardless."""
    machine = machine or Machine()
    os.makedirs(data.root, exist_ok=True)
    with open(data.path("gate.lock"), "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return _record_result(None, None, ["gate_busy"])
        try:
            out_dir, finished = _record_attempt_dir(data, f"record-night-{date}-{cand['sha'][:12]}")
            if finished:
                return _record_result(_public_summary(out_dir), read_json(os.path.join(out_dir, "summary.json")), [],
                                      reused=True)
            return _record_night(args, clock, data, date, cand, window, machine, out_dir)
        except (HarnessError, OSError, subprocess.CalledProcessError, ValueError) as exc:
            reason = exc.reason if isinstance(exc, HarnessError) else "harness_error"
            detail = exc.detail if isinstance(exc, HarnessError) else f"{type(exc).__name__}: {exc}"
            return _record_result(None, None, [f"{reason}: {detail}" if detail else reason])


def _record_night(args, clock, data: Data, date: str, cand: dict, window, machine, out_dir: str) -> dict:
    source = check_source(data, getattr(args, "corpora", None))
    print_ = fingerprint(machine, source)
    path = baseline_path(args)
    baseline = Baseline(path)
    estimates = record_estimates(data, date, print_)
    runner = Runner(clock, machine, source, print_, BUSY_BUDGET_MIN, lambda series: window.fits(estimates[series]))
    os.makedirs(out_dir)
    started_at = verdict.now_iso()
    found = new_round()
    reasons: list[str] = []
    try:
        for series, part in (("A", "candidate"), ("B", "B")):
            for k in range(1, RUNS + 1):
                found[part].append(runner.run(os.path.join(out_dir, "runs", f"{series}-c{k}"), cand, "candidate",
                                              series, k, False, {}))
                found["names"][part].append(runner.launches[-1]["run"])
    except MachineBusy:
        reasons.append("machine_busy")
    except WindowExhausted:
        reasons.append("window_exhausted")
    section = record_section(baseline, print_, [found], tuple(verdict.GATE_RECORD), False)
    ceilings = ceiling_rows(found["candidate"], found["B"])
    second_rows, second_reasons = second_base_judgement(section, None)
    result = verdict.gate_verdict(ceilings + second_rows, reasons + second_reasons)
    durations = {series: [r["duration_s"] for r in records]
                 for series, records in (("A", found["candidate"]), ("B", found["B"]))
                 if len(records) == RUNS and all(r["held"] for r in records)}
    values = [{"run": name, "series": series, "held": record["held"],
               "metrics": {key: record["metrics"].get(key) for key in verdict.GATE_RECORD}}
              for series, part in (("A", "candidate"), ("B", "B"))
              for name, record in zip(found["names"][part], found[part])]
    summary = {"mode": "record-night", "date": date, "candidate": {"label": cand["label"], "sha": cand["sha"]},
               # A night compares no release: its top verdict never says `pass`.
               "verdict": "ok" if result["verdict"] == "pass" else result["verdict"], "reasons": result["reasons"],
               "counts": result["counts"], "record": section, "ceilings": ceilings, "runs": runner.launches,
               "run_values": values, "durations": durations, "baseline": _baseline_reference(path),
               "artifacts": {"summary": _public_summary(out_dir),
                             "report": f"{os.path.dirname(_public_summary(out_dir))}/report.md"},
               "protocol": protocol(None), "controls": None, "noise": None, "busy_spent_s": runner.spent_s,
               "started_at": started_at, "finished_at": verdict.now_iso()}
    write_report(out_dir, summary)
    write_json(os.path.join(out_dir, "summary.json"), summary)
    return _record_result(_public_summary(out_dir), summary, [])


# -- report ---------------------------------------------------------------------------


GROWTH_BASIS = {
    "growth_traded": "рост {what} принят как размен на ускорение (сокращение стены не меньше роста {what})",
    "growth_not_repaid": "блок: рост {what} больше 5 % не окуплен сокращением стены",
}
# The record has a limit of its own for each metric: no bar is named here.
RECORD_GROWTH_BASIS = {
    "growth_traded": "рост {what} выше предела рекорда {limit} принят как размен на ускорение "
                     "(сокращение стены против рекорда не меньше роста {what})",
    "growth_not_repaid": "блок: рост {what} выше предела рекорда {limit} не окуплен сокращением стены против рекорда",
}
LIMIT_BASIS = {"noise": "шум записи (ν)", "bar_no_noise": "бар 5 %, шум отсутствует"}
OBSERVATION_TEXT = "наблюдение; на вердикт не влияет"


def _pct(value) -> str:
    return "-" if value is None else f"{value * 100:+.1f}%"


def _share(value) -> str:
    return "-" if value is None else f"{value * 100:.2f}%"


def _side_text(side: dict) -> str:
    return verdict.fmt(side["value"]) if side["state"] == "valid" else side["state"]


def _aggregate_text(aggregate: dict) -> str:
    return f"{aggregate['count']}/{aggregate['files_affected']}/{aggregate['severity']}"


def _outcome_text(outcome: str) -> str:
    return OBSERVATION_TEXT if outcome == "observation" else outcome


def _record_lines(summary: dict) -> list[str]:
    """The sections of the second base; a summary older than it has none, which
    says nothing about passing it."""
    record = summary.get("record")
    night = summary.get("mode") == "record-night"
    lines = ["## Рекорд", ""]
    if record is None:
        lines += ["- ратчет не измерялся", ""]
    else:
        lines += [f"- итог: **{record['verdict']}**", f"- причины: {'; '.join(record['reasons']) or '-'}",
                  f"- baseline: {record['baseline_path']} (sha256 {record['baseline_sha256'] or '-'})", "",
                  "| метрика | агрегат | рекорд | кандидат | Δ | ν | предел | основание предела | SHA рекорда | "
                  "источник | исход | причина |",
                  "|---|---|---|---|---|---|---|---|---|---|---|---|"]
        for row in record["rows"]:
            evidence = row["record"]["evidence"]
            candidate = row["candidate"]
            shown = verdict.fmt(candidate["value"]) if candidate["state"] == "valid" else candidate["state"] or "-"
            lines.append(f"| {row['metric']} | {row['aggregate']} | {verdict.fmt(row['record']['value'])} | {shown} | "
                         f"{_pct(row['delta'])} | {_share(row['noise'])} | {verdict.fmt(row['limit'])} | "
                         f"{LIMIT_BASIS.get(row.get('limit_basis'), '-')} | "
                         f"{str(row['record']['sha'] or '-')[:12]} | "
                         f"{evidence.get('kind', '-') if isinstance(evidence, dict) else '-'} | "
                         f"{_outcome_text(row['outcome'])} | {row['reason'] or '-'} |")
        lines.append("")
        for row in record["rows"]:
            basis = RECORD_GROWTH_BASIS.get(row.get("basis"))
            if basis:
                what = row["metric"]
                lines += [f"- {what}: {basis.format(what=what, limit=verdict.fmt(row['limit']))}; "
                          f"Δ стены {_pct(row.get('wall_delta'))}, Δ {what} {_pct(row['delta'])}.", ""]
    if night:
        return lines
    lines += ["## Память A/A", ""]
    memory = summary.get("memory_base")
    if memory is None:
        lines += ["- не измерялась", ""]
    else:
        lines += [f"- пик A базы: {_side_text(memory['base']) if memory['base']['state'] else '-'}; "
                  f"пик A кандидата: {_side_text(memory['candidate']) if memory['candidate']['state'] else '-'}; "
                  f"Δ {_pct(memory['delta'])}; ν {_share(memory['noise'])}; предел {verdict.fmt(memory['limit'])}; "
                  f"исход {memory['outcome']}" + (f" ({memory['reason']})" if memory["reason"] else ""), ""]
    lines += ["## Предложения рекорда", ""]
    if record is None:
        lines += ["- ратчет не измерялся", ""]
        return lines
    confirmation = summary.get("record_confirmation") or {}
    if confirmation.get("requested"):
        lines += [f"- второй раунд: {', '.join(confirmation['triggers'])}; "
                  f"{'полный' if confirmation['complete'] else 'неполный'}"]
    proposals = summary.get("record_proposals") or []
    lines += [f"- {p['metric']}: {verdict.fmt(p['previous_value'])} → {verdict.fmt(p['value'])} {p['unit']}, "
              f"раунды {', '.join(verdict.fmt(v) for v in p['rounds'])}, SHA {p['record_sha']}" for p in proposals] or \
        ["- нет"]
    return lines + [""]


def write_report(out_dir: str, summary: dict) -> None:
    lines = [f"# Гейт {os.path.basename(out_dir)}", "",
             f"- вердикт: **{summary['verdict']}**",
             f"- причины: {'; '.join(summary['reasons']) or '-'}"]
    if summary.get("mode") == "record-night":
        lines.append("- режим: ночная проверка рекорда, не гейт релиза")
    else:
        lines.append(f"- база: {summary['base']['label']} {summary['base']['sha'] or '-'}")
    lines += [f"- кандидат: {summary['candidate']['label']} {summary['candidate']['sha'] or '-'}",
              f"- калибровка: {(summary.get('noise') or {}).get('status', '-')}",
              f"- контроли: {json.dumps(summary['controls'], ensure_ascii=False) if summary['controls'] else '-'}",
              ""]
    rows = summary.get("rows") or []
    if summary.get("mode") != "record-night":
        lines += ["## Прошлый релиз", ""]
    if rows:
        lines += ["| метрика | род | база | кандидат | Δ | ν | предел | Δ′ | исход |",
                  "|---|---|---|---|---|---|---|---|---|"]
        for row in rows:
            noise = "-" if row["noise"] is None else f"{row['noise'] * 100:.1f}%"
            confirm = _pct((row.get("confirm") or {}).get("delta"))
            limit = "-" if row["limit"] is None else f"{row['limit'] * 100:.1f}%"
            missing = f" ({row['reason']})" if row.get("reason") else ""
            lines.append(f"| {row['metric']} | {row['kind']} | {_side_text(row['base'])} | "
                         f"{_side_text(row['candidate'])} | {_pct(row['delta'])} | {noise} | "
                         f"{limit} | {confirm} | {_outcome_text(row['outcome'])}{missing} |")
        wall = next((r for r in rows if r["kind"] == "wall"), None)
        if wall is not None and wall["noise"] is not None:
            lines += ["", f"Стеночная метрика решает от Δ > max(5 %, ν) = {wall['limit'] * 100:.1f} % "
                          f"(ν = {wall['noise'] * 100:.1f} %); меньший сдвиг стены от шума не отличим.", ""]
        for row in rows:
            basis = GROWTH_BASIS.get(row.get("basis"))
            if basis:
                what = row["metric"]
                lines += [f"- {row['metric']}: {basis.format(what=what)}; Δ стены {_pct(row.get('wall_delta'))}, "
                          f"Δ {what} {_pct(row['delta'])}.", ""]
    elif summary.get("mode") != "record-night":
        lines += ["- не измерялся", ""]
    lines += _record_lines(summary)
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
                      "CHANGELOG «Изменения поведения», гейт судит время и память. Изменения функционала "
                      "не расширяют временные пределы.", ""]
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
    if args.record_metrics_only and args.base != RECORD_CALIBRATION_BASE:
        raise HarnessError("base_invalid", f"--record-metrics-only measures {RECORD_CALIBRATION_BASE}, not {args.base}")
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
        if args.record_metrics_only:
            return _calibrate_record(clock, machine, data, source, print_, base, root, count)
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
        # An observed metric without a noise is told in its row; it keeps no calibration from the gate.
        written = bool(values) and all(v is not None for k, v in values.items() if k not in verdict.GATE_OBSERVED)
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


def record_noise_entries(groups: list[tuple[list, list]], binary: dict, print_: dict) -> dict:
    """{noise key: entry} of the aggregates a record judges that the old
    calibration does not hold, over series of (A, B). Every entry names its own
    binary, profile and date: they are not those of the root of `gate-noise.json`.
    A number needs every series with both of its threes: a failed run never
    turns into a zero spread."""
    def aggregates(key: str, series_a: list, series_b: list) -> tuple[str, float | None]:
        if key == MEMORY_A_KEY:
            return peak([series_a])
        return peak([series_a, series_b]) if key == "hwm_kb" else side(series_b, verdict.Metric(key, key))

    identity = {"binary": {"label": binary["label"], "sha": binary["sha"], "sha256": binary["sha256"]},
                "fingerprint": print_, "created_at": verdict.now_iso()}
    keys = (MEMORY_A_KEY, "hwm_kb", *REFS_METRICS)
    found = {key: [aggregates(key, a, b) for a, b in groups] for key in keys}
    full = all(state == "valid" for sides in found.values() for state, _ in sides)
    entries = {}
    for key, sides in found.items():
        values = [value for state, value in sides if state == "valid"]
        entry = {**identity, "aggregates": values, "states": [state for state, _ in sides],
                 "noise": verdict.spread_noise(values) if full else None}
        if entry["noise"] is None:
            entry["reason"] = ("zero median" if full else "not valid in every series" if len(values) < len(sides)
                               else "another aggregate is not valid in every series")
        entries[verdict.noise_key(CORPUS, SCENARIO, TEMP, key)] = entry
    return entries


def _calibrate_record(clock, machine, data: Data, source: dict, print_: dict, base: dict, root: str,
                      count: int) -> int:
    """`gate-calibrate --record-metrics-only`: its own series of A×3+B×3 of a
    fixed binary. The numbers join `gate-noise.json` only when the profile is
    the one of its calibration; its entries are never remeasured or replaced."""
    groups, busy = [], []
    for index in range(1, count + 1):
        series_dir = os.path.join(root, f"record-s{index}")
        shutil.rmtree(series_dir, ignore_errors=True)
        runner = Runner(clock, machine, source, print_, BUSY_BUDGET_MIN)
        series_a, series_b = [], []
        try:
            for name, series, records in (("A-b", "A", series_a), ("B-b", "B", series_b)):
                for k in range(1, RUNS + 1):
                    records.append(runner.run(os.path.join(series_dir, f"{name}{k}"), base, "base", series, k, False, {}))
        except MachineBusy:
            busy.append(index)
        groups.append((series_a, series_b))
    entries = record_noise_entries(groups, base, print_)
    values = {key.rsplit("/", 1)[1]: entry["noise"] for key, entry in entries.items()}
    measured = all(value is not None for value in values.values())
    noise = read_noise(data)
    calibrated = ((noise or {}).get("fingerprints") or {}).get(CORPUS)
    differs = ["gate-noise.json"] if calibrated is None else _differing(calibrated, print_)
    write_json(os.path.join(root, "record-noise.json"),
               {"schema": verdict.NOISE_SCHEMA, "gate_schema": GATE_SCHEMA, "differs_from_calibration": differs,
                "entries": entries})
    merged = measured and not differs
    if merged:
        write_json(data.path("gate-noise.json"), {**noise, "entries": {**noise["entries"], **entries}})
    print(f"gate-calibrate record-metrics binary={base['label']} series={count} "
          f"busy={','.join(f's{i}' for i in busy) or '-'} merged={str(merged).lower()} "
          f"differs={','.join(differs) or '-'} " +
          " ".join(f"nu.{key}={verdict.fmt(value)}" for key, value in sorted(values.items())))
    return 0 if merged else 2
