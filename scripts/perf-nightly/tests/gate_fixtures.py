"""Fixtures of the release gate tests: a data dir with an ERP corpus, fake
binaries, a machine whose load the test scripts, and the values that a fake
server cannot give on a virtual clock (CPU of a run, readiness and call times,
peak memory). Nothing here waits for time: the clock is virtual and every wait
of the harness ends on the fake server's answer or on a process exit."""

from __future__ import annotations

import argparse
import collections
import contextlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from helpers import FAKE_APP, FakeClock, event_driven_process_waits, mock_env, run_git

import gate
import perf_nightly
import procs
import scenarios
import verdict
from common import HARNESS_SCHEMA, Data, read_json, sha256_file, write_json

BASE_TAG = "v0.4.1"
BASE_SHA = "b" * 40
CAND_SHA = "c" * 40
CAND_LABEL = "cand"
MODES = "graph_answers,sweep_answers,refs_tool"
MACHINE = {"cpu_model": "Test CPU", "nproc": 32, "mem_total": "62 GiB", "kernel": "6.1", "rustc": "rustc test"}
CONFIG = '[analysis]\ndiff_base = "vendor"\n'
RUN_ID_RE = re.compile(r"-(?:(?P<series>[AB])-(?P<role>[bc])(?P<n>\d+)|(?P<cal>r)(?P<k>\d+))"
                       r"(?:\.(?P<attempt>\d+))?-[0-9a-f]{8}$")

Run = collections.namedtuple("Run", "series role n attempt")


def parse_run(run_id: str) -> Run:
    """Where in the gate a run is, from the run id the gate gives it: series A or
    B, `b`(ase) or `c`(andidate), its number (4 to 6 in the confirming round)
    and the attempt (2 and up after a run under foreign load)."""
    found = RUN_ID_RE.search(run_id)
    assert found, run_id
    if found["cal"]:
        return Run("A", "b", int(found["k"]), int(found["attempt"] or 1))
    return Run(found["series"], found["role"], int(found["n"]), int(found["attempt"] or 1))


class Script:
    """What the fake server cannot give deterministically. Each value is a
    constant or a function of the run: `wall` (seconds to graph readiness),
    `cpu` (CPU seconds of the run at readiness), `foreign` (cores of load
    outside the run), `hwm_kb` (peak of the run) and `elapsed` (seconds of a
    call that carries a metric, by its name: `graph_resolve`,
    `diagnostics_workspace`, `references_1`, `references_2_same`,
    `references_3_other`; `None` keeps `ELAPSED`)."""

    DEFAULTS = {"wall": 100.0, "cpu": 40.0, "foreign": 0.0, "hwm_kb": 4 << 20}
    ELAPSED = {"graph_resolve": 17.8, "diagnostics_workspace": 22.1, "references_1": 56.5,
               "references_2_same": 24.9, "references_3_other": 50.3}

    def __init__(self, clock: FakeClock, **values):
        self.clock = clock
        self.values = values
        self.stage = "idle"
        self.started_at = 0.0
        self.current: Run | None = None
        self.runs: list[Run] = []
        self.run_ids: list[str] = []

    def get(self, name: str, run: Run, *args):
        value = self.values.get(name, self.DEFAULTS.get(name))
        return value(run, *args) if callable(value) else value

    def call_name(self, mcp_run, tool: str, arguments: dict) -> str | None:
        seen = mcp_run.__dict__.setdefault("_seen", collections.Counter())
        if tool == "graph" and arguments.get("action") == "resolve" and "top" not in arguments:
            key = ("graph", arguments["query"])
            seen[key] += 1
            if arguments["query"] == gate.SYMBOL_1:
                return "graph_resolve" if seen[key] == 1 else "graph_resolve_2_same"
            return "graph_resolve_3_other"
        if tool == "references":
            key = ("references", arguments["symbol"])
            seen[key] += 1
            if arguments["symbol"] == gate.SYMBOL_1:
                return "references_1" if seen[key] == 1 else "references_2_same"
            return "references_3_other"
        if tool == "diagnostics" and arguments.get("action") == "workspace":
            return "diagnostics_workspace"
        return None

    def install(self, case: unittest.TestCase) -> None:
        script = self
        original = {name: getattr(procs.Sampler, name) for name in ("__init__", "stop")}
        original_status, original_tool = scenarios.McpRun.status, scenarios.McpRun.tool

        def init(sampler, run_id, *args, **kwargs):
            original["__init__"](sampler, run_id, *args, **kwargs)
            script.current = parse_run(run_id)
            script.runs.append(script.current)
            script.run_ids.append(run_id)
            script.stage, script.started_at = "start", script.clock.now()

        def cpu_s(sampler, exclude=None):
            # Only the readiness snapshot passes `exclude`: it is what the machine reads next.
            if exclude is not None:
                script.stage = "ready"
            return script.get("cpu", parse_run(sampler.run_id))

        def stop(sampler):
            sampled = original["stop"](sampler)
            script.stage = "idle"
            return {**sampled, "hwm_kb": script.get("hwm_kb", parse_run(sampler.run_id))}

        def status(mcp_run, name):
            if name == "graph" and not mcp_run.__dict__.get("_waited"):
                mcp_run._waited = True
                script.clock.advance(script.get("wall", parse_run(mcp_run.ctx.run_id)))
            return original_status(mcp_run, name)

        def tool(mcp_run, name, arguments, timeout=scenarios.CALL_TIMEOUT_S):
            elapsed, result, error = original_tool(mcp_run, name, arguments, timeout)
            call = script.call_name(mcp_run, name, arguments)
            override = script.get("elapsed", parse_run(mcp_run.ctx.run_id), call) if call else None
            if override is None and call:
                override = script.ELAPSED[call] if call in script.ELAPSED else None
            return (elapsed if override is None else override), result, error

        for target, attribute, replacement in ((procs.Sampler, "__init__", init), (procs.Sampler, "cpu_s", cpu_s),
                                               (procs.Sampler, "stop", stop), (scenarios.McpRun, "status", status),
                                               (scenarios.McpRun, "tool", tool)):
            case.enterContext(patch.object(target, attribute, replacement))


class ScriptedMachine:
    """The machine of a gate under test: its busy time grows by the run's CPU
    plus `foreign` cores over the readiness window, and the precondition
    answers as scripted (`quiet(n)` for the n-th question)."""

    def __init__(self, script: Script, clock: FakeClock, quiet=None, print_=None):
        self.script, self.clock, self.quiet_of = script, clock, quiet
        self.print = dict(print_ or MACHINE)
        self.total = 1000.0
        self.quiet_calls = 0

    def fingerprint(self) -> dict:
        return dict(self.print)

    def quiet(self) -> dict:
        self.quiet_calls += 1
        ok = True if self.quiet_of is None else self.quiet_of(self.quiet_calls)
        return {"ok": ok, "mem_available_kb": 64 << 20, "load1": 0.0}

    def busy_s(self) -> float:
        script = self.script
        if script.stage == "ready":
            script.stage = "after"
            wall = self.clock.now() - script.started_at
            self.total += script.get("cpu", script.current) + script.get("foreign", script.current) * wall
        return self.total


class GateCase(unittest.TestCase):
    """A data dir with the ERP corpus of the manifest, two fake binaries, a
    scripted machine and a calibration that is fresh."""

    modes = MODES
    wall_noise = 0.03
    decisive_noise = 0.01

    def setUp(self):
        event_driven_process_waits(self)
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.addCleanup(self._tmp.cleanup)
        self.tmp = self._tmp.name
        self.log = os.path.join(self.tmp, "log")
        os.makedirs(self.log)
        mock_env(self, {"FAKE_LOG": self.log, "FAKE_MODE": self.modes})
        self.data_dir = os.path.join(self.tmp, "data")
        os.makedirs(self.data_dir)
        self.data = Data(self.data_dir)
        self.repo = self.make_repo(os.path.join(self.tmp, "repo"), "head")
        self.enterContext(patch.object(gate, "repo_root", lambda: self.repo))
        self.erp = self.make_repo(os.path.join(self.tmp, "erp"), "erp", CONFIG)
        self.erp_commit = self.rev(self.erp)
        self.write_manifest()
        self.clock = FakeClock()
        self.script = Script(self.clock, **self.script_values())
        self.script.install(self)
        self.machine = ScriptedMachine(self.script, self.clock)
        self.base_path = self.fake_binary(BASE_TAG, BASE_SHA)
        self.cand_path = self.fake_binary(CAND_LABEL, CAND_SHA)
        self.builds: list = []
        self.write_noise()

    def script_values(self) -> dict:
        return {}

    # -- the world --------------------------------------------------------------------

    @staticmethod
    def make_repo(path: str, name: str, config: str | None = None) -> str:
        os.makedirs(path)
        run_git("init", "-q", "-b", "main", cwd=path)
        run_git("config", "user.email", "t@t", cwd=path)
        run_git("config", "user.name", "t", cwd=path)
        with open(os.path.join(path, "f.txt"), "w") as fh:
            fh.write(name + "\n")
        if config is not None:
            with open(os.path.join(path, "bsl-analyzer.toml"), "w") as fh:
                fh.write(config)
        run_git("add", "-A", cwd=path)
        run_git("commit", "-q", "-m", name, cwd=path)
        return path

    @staticmethod
    def rev(path: str, ref: str = "HEAD") -> str:
        return subprocess.run(["git", "rev-parse", ref], cwd=path, check=True, capture_output=True,
                              text=True).stdout.strip()

    def write_manifest(self, commit: str | None = None, corpus_id: str = "erp") -> None:
        help_path = os.path.join(self.tmp, "help.json")
        with open(help_path, "w") as fh:
            fh.write("{}")
        with open(self.data.path("corpora.toml"), "w", encoding="utf-8") as fh:
            fh.write(f'[platform_help]\nsource = "external"\npath = "{help_path}"\n\n'
                     f'[[corpus]]\nid = "{corpus_id}"\nlabel = "ERP"\nsource_repo = "{self.erp}"\n'
                     f'commit = "{commit or self.erp_commit}"\nmode = "full"\n')

    def fake_binary(self, label: str, sha: str) -> str:
        bin_dir = os.path.join(self.data_dir, "bin", f"{label}-{sha[:12]}")
        os.makedirs(bin_dir, exist_ok=True)
        path = os.path.join(bin_dir, "bsl-analyzer-app")
        with open(path, "w") as fh:
            fh.write(f'#!/bin/sh\nFAKE_LABEL=\'{label}\' exec "{sys.executable}" "{FAKE_APP}" "$@"\n')
        os.chmod(path, 0o755)
        write_json(os.path.join(bin_dir, "build.json"), {
            "label": label, "sha": sha, "rustc": "rustc test", "has_platform_help": True,
            "sha256": sha256_file(path)})
        return path

    def builder(self, ref: str, label: str) -> str:
        self.builds.append((ref, label))
        return self.base_path if ref == BASE_TAG else self.cand_path

    def fingerprint(self) -> dict:
        return gate.fingerprint(self.machine, gate.check_source(self.data))

    def write_noise(self, wall=None, decisive=None, **fields) -> dict:
        wall = self.wall_noise if wall is None else wall
        decisive = self.decisive_noise if decisive is None else decisive
        entries = {verdict.noise_key("erp", gate.SCENARIO, gate.TEMP, key):
                   {"medians": [100.0] * 5, "states": ["valid"] * 5, "noise": wall if key == "graph_ready_s" else decisive}
                   for key in gate.METRICS}
        noise = {"schema": verdict.NOISE_SCHEMA, "harness_schema": HARNESS_SCHEMA, "gate_schema": gate.GATE_SCHEMA,
                 "fingerprints": {"erp": self.fingerprint()}, "binary": {"label": BASE_TAG, "sha": BASE_SHA},
                 "created_at": "2026-10-29T00:00:00+00:00", "entries": entries}
        noise.update(fields)
        write_json(self.data.path("gate-noise.json"), noise)
        return noise

    # -- running ----------------------------------------------------------------------

    def args(self, command="gate", label="g1", candidate="HEAD", *extra) -> argparse.Namespace:
        argv = [command, "--data-dir", self.data_dir, "--base", BASE_TAG]
        if command == "gate":
            argv += ["--candidate", candidate, "--label", label]
        return perf_nightly.parser().parse_args([*argv, *extra])

    def gate(self, *extra, label="g1", candidate="HEAD", builder=None, machine=None) -> tuple[int, dict, str]:
        """(exit code, summary.json, printed line) of `gate` with the scripted world."""
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_gate(self.args("gate", label, candidate, *extra), self.clock, builder or self.builder,
                                 machine or self.machine)
        return code, read_json(self.data.path("gates", label, "summary.json")), out.getvalue()

    def calibrate(self, *extra, machine=None) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_gate_calibrate(self.args("gate-calibrate", None, None, *extra), self.clock,
                                           self.builder, machine or self.machine)
        return code, out.getvalue()

    def runner(self, budget_min=60.0) -> gate.Runner:
        source = gate.check_source(self.data)
        return gate.Runner(self.clock, self.machine, source, self.fingerprint(), budget_min)

    def one_run(self, series="A", role="base", budget_min=60.0, ready_only=False, controls=None) -> dict:
        """One run of the gate on its own, in a fresh run dir."""
        binary = perf_nightly.load_binary(self.base_path if role == "base" else self.cand_path)
        self.single_runs = getattr(self, "single_runs", 0) + 1
        self.run_dir = os.path.join(self.tmp, "one", f"{series}-{'b' if role == 'base' else 'c'}{self.single_runs}")
        return self.runner(budget_min).run(self.run_dir, binary, role, series, 1, ready_only, controls or {})

    # -- what the fake server saw -----------------------------------------------------

    def launches(self) -> list[dict]:
        path = os.path.join(self.log, "launches.jsonl")
        if not os.path.exists(path):
            return []
        with open(path, encoding="utf-8") as fh:
            return [json.loads(line) for line in fh]

    def tools(self) -> list[dict]:
        path = os.path.join(self.log, "tools.jsonl")
        if not os.path.exists(path):
            return []
        with open(path, encoding="utf-8") as fh:
            return [json.loads(line) for line in fh]

    def roles(self, summary: dict) -> str:
        """The order of the launches of a gate, one letter each: `b` base, `c` candidate."""
        return "".join(r["role"][0] for r in summary["runs"])

    def row(self, summary: dict, metric: str) -> dict:
        return next(r for r in summary["rows"] + summary["ceilings"] if r["metric"] == metric)
