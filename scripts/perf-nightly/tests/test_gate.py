"""The release gate on a fake server (plan docs/plans/release-gate-graph-ready.md §7, §10, §11):
the protocol of one run, the order of the runs and what is done about a run
under foreign load, the answers and ceilings, the refusals, the controls, the
calibration and the report. The clock is virtual and the machine scripted
(`gate_fixtures.py`); nothing here waits for time."""

import contextlib
import fcntl
import io
import json
import os
import sys
import tempfile
import unittest
from unittest.mock import patch

from helpers import FakeClock, mock_env, run_git, set_mode

import common
import gate
import perf_nightly
import procs
import scenarios
import verdict
from common import HarnessError, read_json, write_json
from gate_fixtures import BASE_SHA, BASE_TAG, CAND_LABEL, GateCase, MACHINE, ScriptedMachine

# The release protocol as the ad-hoc driver of the measurement of 2026-10-29 ran it
# (/home/itrous/src/.temp/bsl304-plan-bench/driver3.py:28-30, :195-236).
S1 = "ОбщегоНазначения.ОбщийМодуль"
S2 = "ОбщегоНазначения.ЗначениеРеквизитаОбъекта"
MODULE = "src/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl"
EXTRA = ("ЗначениеРеквизитаОбъекта", "ЗначениеРеквизит", "ОбщегоНазначения")
STATUS = [("diagnostics", {"action": "status"}), ("graph", {"action": "status"})]
RESOLVE = [("symbol_info", {"symbol": S1}), ("graph", {"action": "resolve", "query": S1}),
           *[("graph", {"action": "resolve", "query": q, "top": 50}) for q in EXTRA]]
ANSWERS_A = ["c-graph-resolve", "c-graph-resolve-extra-0", "c-graph-resolve-extra-1", "c-graph-resolve-extra-2",
             "d-graph-resolve-3", "f-diagnostics-workspace"]
LIMIT_KB = 7168 * 1024


def read_bytes(path):
    with open(path, "rb") as fh:
        return fh.read()


def candidate_slower(factor, field="wall"):
    """Script value: the candidate takes `factor` times the base (100) in series A, rounds one and two apart."""
    return lambda run: 100.0 * factor if run.role == "c" and run.series == "A" else 100.0


class Protocol(GateCase):
    """One run of the gate repeats the calls and the pauses of the release protocol."""

    def test_series_a_follows_the_release_protocol(self):
        mock_env(self, {"BSL_MCP_BROKER": "1", "BSL_LOG": "debug"})
        record = self.one_run("A")
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual([(t["name"], t["arguments"]) for t in self.tools()], [
            *STATUS, *RESOLVE, ("diagnostics", {"action": "file", "path": MODULE}),
            ("symbol_info", {"symbol": S1}), ("symbol_info", {"symbol": S2}),
            ("graph", {"action": "resolve", "query": S1}), ("graph", {"action": "resolve", "query": S2}),
            ("diagnostics", {"action": "workspace", "max_files": 5000})])
        self.assertEqual(self.clock.sleeps, [20.0, 60.0, 30.0])
        [launch] = self.launches()
        self.assertEqual(launch["argv"], ["mcp", "serve", "--profile", "workspace", "--source-dir", self.erp])
        self.assertEqual(launch["cwd"], self.erp)
        self.assertNotIn("BSL_MCP_BROKER", launch["env"])
        self.assertNotIn("BSL_LOG", launch["env"])
        self.assertEqual(launch["env"]["BSL_CACHE_DIR"], os.path.join(self.run_dir, "cache"))
        self.assertEqual(launch["env"][procs.MARKER], record["run_id"])
        self.assertEqual(sorted(os.listdir(os.path.join(self.run_dir, "answers"))),
                         sorted(f"{name}.json" for name in ANSWERS_A))
        with open(os.path.join(self.run_dir, "answers", "c-graph-resolve.json"), "rb") as fh:
            canonical = fh.read()
        self.assertTrue(json.loads(canonical))
        self.assertEqual(canonical, json.dumps(json.loads(canonical), sort_keys=True, ensure_ascii=False).encode())
        saved = read_json(os.path.join(self.run_dir, "r.json"))
        self.assertEqual(saved, record)
        metrics = record["metrics"]
        self.assertEqual({k: metrics[k] for k in ("graph_ready_s", "graph_ready_cpu_s", "foreign_cores",
                                                   "graph_resolve_s", "diagnostics_workspace_s", "hwm_kb")},
                         {"graph_ready_s": 100.0, "graph_ready_cpu_s": 40.0, "foreign_cores": 0.0,
                          "graph_resolve_s": 17.8, "diagnostics_workspace_s": 22.1, "hwm_kb": 4 << 20})
        for key in ("schema", "run_id", "binary", "errors", "held", "reason", "env_fingerprint", "k", "runs",
                    "controls", "load_before", "load_after"):
            self.assertIn(key, record)
        self.assertEqual((record["k"], record["runs"], record["controls"], record["reason"]), (1, 3, None, None))
        self.assertEqual(record["stop"]["final"]["left"], [])

    def test_series_b_adds_the_references_tool_and_leaves_the_sweep(self):
        record = self.one_run("B", "candidate")
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual([(t["name"], t["arguments"]) for t in self.tools()], [
            *STATUS, *RESOLVE, ("references", {"symbol": S1}), ("diagnostics", {"action": "file", "path": MODULE}),
            ("symbol_info", {"symbol": S1}), ("symbol_info", {"symbol": S2}),
            ("references", {"symbol": S1}), ("references", {"symbol": S2}),
            ("graph", {"action": "resolve", "query": S1}), ("graph", {"action": "resolve", "query": S2})])
        [launch] = self.launches()
        self.assertEqual(launch["argv"], ["mcp", "serve", "--profile", "workspace", "--source-dir", self.erp,
                                          "--enable-tool", "references"])
        self.assertEqual(self.clock.sleeps, [20.0, 60.0])
        metrics = record["metrics"]
        self.assertEqual((metrics["references_1_s"], metrics["references_2_same_s"], metrics["references_3_other_s"]),
                         (56.5, 24.9, 50.3))
        self.assertNotIn("diagnostics_workspace_s", metrics)
        self.assertEqual(sorted(os.listdir(os.path.join(self.run_dir, "answers"))),
                         sorted(f"{name}.json" for name in ANSWERS_A[:-1]))

    def test_a_series_b_server_without_the_references_tool_is_not_held(self):
        set_mode("graph_answers", "sweep_answers")
        record = self.one_run("B", "candidate")
        self.assertEqual((record["held"], record["reason"]), (False, "tool_missing"))

    def test_an_answer_without_its_list_fails_the_run(self):
        set_mode("sweep_answers", "refs_tool")
        record = self.one_run("A")
        self.assertEqual((record["held"], record["reason"]), (False, "call_failed"))
        self.assertTrue(any("graph_resolve" in e and "candidates" in e for e in record["errors"]), record["errors"])
        set_mode("graph_answers", "refs_tool")
        record = self.one_run("A")
        self.assertEqual((record["held"], record["reason"]), (False, "call_failed"))
        self.assertTrue(any("diagnostics_workspace" in e and "aggregates" in e for e in record["errors"]),
                        record["errors"])

    def test_a_server_that_never_reports_the_graph_ready_fails_the_run(self):
        set_mode("graph_failed")
        record = self.one_run("A")
        self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))
        set_mode("no_graph")
        record = self.one_run("A")
        self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))


class WaitReady(GateCase):
    """`McpRun.wait_ready(on_ready)`: once per backend, at its first `ready`."""

    def test_on_ready_runs_once_per_backend_at_its_first_ready(self):
        set_mode("graph_busy")
        run_id = "gate-x-aaaaaaaaaaaa-A-b1-deadbeef"
        ctx = scenarios.Ctx(binary=self.base_path, root=self.erp, snap=self.erp, env=procs.child_env(run_id),
                            run_dir=os.path.join(self.tmp, "wait"), run_id=run_id, clock=self.clock, points={},
                            snap_meta={})
        os.makedirs(ctx.run_dir)
        run = scenarios.McpRun(ctx, extra_args=("--enable-tool", "references"))
        seen = []
        try:
            run.start()
            run.wait_ready(seen.append)
        finally:
            run.close()
            procs.stop(run_id, self.clock)
            run.reap(True)
        # The graph answered `busy` twice: the diagnostics were ready from the first poll and reported once.
        self.assertEqual(seen, ["diagnostics", "graph"])
        self.assertEqual(self.clock.sleeps, [1.0, 1.0])
        [launch] = self.launches()
        self.assertEqual(launch["argv"][-2:], ["--enable-tool", "references"])


class Order(GateCase):
    def test_the_gate_passes_a_candidate_equal_to_the_base(self):
        code, summary, line = self.gate()
        self.assertEqual((code, summary["verdict"], summary["reasons"]), (0, "pass", []))
        self.assertEqual(line, f"gate=pass reasons=- out={self.data.path('gates', 'g1')}\n")
        self.assertEqual(self.roles(summary), "bcbcbcccc")
        self.assertEqual([r["series"] for r in summary["runs"]], list("AAAAAA") + list("BBB"))
        self.assertEqual([r["run"] for r in summary["runs"]],
                         ["A-b1", "A-c1", "A-b2", "A-c2", "A-b3", "A-c3", "B-c1", "B-c2", "B-c3"])
        self.assertTrue(all(r["held"] and r["foreign_cores"] == 0.0 for r in summary["runs"]))
        self.assertEqual(self.builds, [(BASE_TAG, BASE_TAG), (self.rev(self.repo), "rc")])
        self.assertEqual(summary["base"], {"label": BASE_TAG, "sha": BASE_SHA})
        self.assertEqual(summary["candidate"]["label"], CAND_LABEL)
        self.assertEqual([r["metric"] for r in summary["rows"]],
                         ["graph_ready_s", "graph_ready_cpu_s", "graph_resolve_s", "diagnostics_workspace_s"])
        self.assertEqual({r["outcome"] for r in summary["rows"]}, {"ok"})
        self.assertEqual([(r["metric"], r["outcome"]) for r in summary["ceilings"]],
                         [("hwm_kb", "ok"), ("references_1_s", "ok"), ("references_2_same_s", "ok"),
                          ("references_3_other_s", "ok")])
        self.assertEqual((summary["answers"]["outcome"], summary["answers"]["control"]), ("ok", True))
        self.assertEqual(summary["noise"]["status"], "current")
        self.assertIsNone(summary["controls"])
        self.assertEqual(summary["protocol"]["pauses_s"], [20.0, 60.0, 30.0])
        for key in ("verdict", "reasons", "rows", "answers", "ceilings", "runs", "noise", "controls", "protocol",
                    "base", "candidate", "started_at", "finished_at"):
            self.assertIn(key, summary)
        self.assertTrue(os.path.isfile(self.data.path("gates", "g1", "report.md")))

    def test_foreign_run_is_redone_in_place(self):
        # The first candidate run has 8 cores of foreign load over its readiness.
        self.script.values["foreign"] = lambda run: 8.0 if (run.role, run.series, run.n, run.attempt) == \
            ("c", "A", 1, 1) else 0.0
        code, summary, _ = self.gate()
        self.assertEqual(self.roles(summary), "bccbcbcccc")
        self.assertEqual([r["run"] for r in summary["runs"][:3]], ["A-b1", "A-c1", "A-c1.2"])
        first = summary["runs"][1]
        self.assertEqual((first["held"], first["reason"], first["foreign_cores"]), (False, "foreign_load", 8.0))
        self.assertTrue(all(r["held"] for r in summary["runs"] if r["run"] != "A-c1"))
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertEqual(len(self.launches()), 10)
        self.assertGreater(summary["busy_spent_s"], 0)
        # Redone in the place of the run: the three candidate runs of the round are k = 1, 2, 3.
        self.assertEqual([r["k"] for r in summary["runs"] if r["role"] == "candidate" and r["series"] == "A"],
                         [1, 1, 2, 3])

    def test_two_cores_of_foreign_load_count_and_a_bit_more_does_not(self):
        self.script.values["foreign"] = lambda run: 2.0 if run.n == 1 and run.attempt == 1 else 0.0
        _, summary, _ = self.gate("--ready-only", label="two")
        self.assertEqual(self.roles(summary), "bcbcbc")
        self.assertEqual(summary["runs"][0]["foreign_cores"], 2.0)
        self.script.values["foreign"] = lambda run: 2.01 if run.n == 1 and run.attempt == 1 and run.role == "b" \
            else 0.0
        _, summary, _ = self.gate("--ready-only", label="more")
        self.assertEqual(self.roles(summary), "bbcbcbc")
        self.assertEqual(summary["runs"][0]["reason"], "foreign_load")

    def test_busy_budget_gives_machine_busy(self):
        self.script.values["foreign"] = lambda run: 8.0
        code, summary, _ = self.gate("--busy-budget-min", "0")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual(summary["reasons"][0], "machine_busy")
        self.assertEqual([(r["run"], r["reason"]) for r in summary["runs"]], [("A-b1", "foreign_load")])
        self.assertEqual(len(self.launches()), 1, "no run after the budget is spent")

    def test_foreign_runs_spend_the_budget_by_their_own_duration(self):
        # Each run under load takes 100 s of virtual time: 3 minutes allow one redo and stop at the second.
        self.script.values["foreign"] = lambda run: 8.0
        code, summary, _ = self.gate("--busy-budget-min", "3")
        self.assertEqual(summary["reasons"][0], "machine_busy")
        self.assertEqual([r["run"] for r in summary["runs"]], ["A-b1", "A-b1.2"])
        self.assertEqual(summary["busy_spent_s"], 200.0)

    def test_a_budget_spent_to_the_last_second_allows_no_second_attempt(self):
        # "No longer than the budget": a run that took 60 s leaves nothing of a one minute budget.
        self.script.values["wall"] = 60.0
        self.script.values["foreign"] = lambda run: 8.0 if (run.role, run.n, run.attempt) == ("b", 1, 1) else 0.0
        code, summary, _ = self.gate("--ready-only", "--busy-budget-min", "1")
        self.assertEqual((code, summary["reasons"][0]), (2, "machine_busy"))
        self.assertEqual((summary["busy_spent_s"], [r["run"] for r in summary["runs"]]), (60.0, ["A-b1"]))
        code, summary, _ = self.gate("--ready-only", "--busy-budget-min", "2", label="g2")
        self.assertEqual((code, summary["verdict"], summary["busy_spent_s"]), (0, "probe-ok", 60.0))
        self.assertEqual(self.roles(summary), "bbcbcbc")

    def test_a_zero_budget_does_not_stop_a_quiet_machine(self):
        code, summary, _ = self.gate("--ready-only", "--busy-budget-min", "0")
        self.assertEqual((code, summary["verdict"], summary["busy_spent_s"]), (0, "probe-ok", 0.0))

    def test_a_block_stands_when_the_machine_gets_busy_later(self):
        # The work regression is found in the first round; series B then meets foreign load and a spent budget.
        self.script.values["cpu"] = lambda run: 44.0 if run.role == "c" else 40.0
        self.script.values["foreign"] = lambda run: 8.0 if run.series == "B" else 0.0
        code, summary, _ = self.gate("--busy-budget-min", "1")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(summary["reasons"][0], "machine_busy")
        self.assertIn("block: graph_ready_cpu_s", summary["reasons"])
        self.assertEqual([r["run"] for r in summary["runs"]][-1], "B-c1")

    def test_waiting_for_a_quiet_machine_spends_the_budget(self):
        self.machine.quiet_of = lambda n: n > 3
        code, summary, _ = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"]), (0, "probe-ok"))
        self.assertEqual(self.clock.sleeps[:3], [30.0, 30.0, 30.0])
        self.assertEqual(summary["busy_spent_s"], 90.0)
        self.assertEqual(self.roles(summary), "bcbcbc")

    def test_a_machine_that_never_quiets_ends_the_gate_without_a_run(self):
        self.machine.quiet_of = lambda n: False
        code, summary, _ = self.gate("--busy-budget-min", "1")
        self.assertEqual((code, summary["verdict"], summary["reasons"][0]), (2, "incomplete", "machine_busy"))
        self.assertEqual(self.clock.sleeps, [30.0, 30.0])
        self.assertEqual((summary["runs"], self.launches()), ([], []))

    def test_a_work_regression_blocks(self):
        self.script.values["cpu"] = lambda run: 44.0 if run.role == "c" else 40.0
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["outcome"], "block")
        self.assertAlmostEqual(self.row(summary, "graph_ready_cpu_s")["delta"], 0.1)
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "ok")
        self.assertIn("block: graph_ready_cpu_s", summary["reasons"])
        self.script.values["cpu"] = lambda run: 41.9 if run.role == "c" else 40.0
        code, summary, _ = self.gate(label="g2")
        self.assertEqual((code, summary["verdict"]), (0, "pass"))

    def test_more_work_for_a_shorter_wait_passes_and_the_report_says_why(self):
        self.script.values["wall"] = candidate_slower(0.55)
        self.script.values["cpu"] = lambda run: 49.6 if run.role == "c" else 40.0
        code, summary, _ = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"]), (0, "probe-ok"))
        cpu = self.row(summary, "graph_ready_cpu_s")
        self.assertEqual((cpu["outcome"], cpu["basis"]), ("ok", "growth_traded"))
        self.assertAlmostEqual(cpu["delta"], 0.24)
        self.assertAlmostEqual(cpu["wall_delta"], -0.45)
        with open(self.data.path("gates", "g1", "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("рост CPU принят как размен на ускорение", report)
        self.assertIn("Δ стены -45.0%, Δ CPU +24.0%", report)

    def test_more_work_without_a_shorter_wait_blocks_and_the_report_says_why(self):
        self.script.values["cpu"] = lambda run: 49.6 if run.role == "c" else 40.0
        code, summary, _ = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["basis"], "growth_not_repaid")
        with open(self.data.path("gates", "g1", "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("блок: рост CPU больше 5 % не окуплен сокращением стены", report)
        self.assertIn("Δ стены +0.0%, Δ CPU +24.0%", report)

    def test_a_wait_regression_above_a_small_noise_blocks_without_a_second_round(self):
        self.script.values["wall"] = candidate_slower(1.5)
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        wall = self.row(summary, "graph_ready_s")
        self.assertEqual(wall["outcome"], "block")
        self.assertNotIn("confirm", wall)
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["outcome"], "ok")
        self.assertEqual(len([r for r in summary["runs"] if r["series"] == "A"]), 6)

    def test_a_regression_of_resolve_or_sweep_blocks(self):
        for call in ("graph_resolve", "diagnostics_workspace"):
            with self.subTest(call=call):
                self.script.values["elapsed"] = lambda run, name, call=call: \
                    1.2 * self.script.ELAPSED[name] if name == call and run.role == "c" else None
                code, summary, _ = self.gate(label=f"g-{call}")
                self.assertEqual((code, summary["verdict"]), (1, "block"))
                self.assertEqual(self.row(summary, f"{call}_s")["outcome"], "block")


class Confirmation(GateCase):
    wall_noise = 0.18

    def wall(self, first, second):
        return lambda run: 100.0 if run.role == "b" else (first if run.n <= 3 else second) if run.series == "A" \
            else 100.0

    def test_a_shift_within_the_noise_is_noise_and_needs_no_second_round(self):
        # The measurement of 2026-10-29: Δ = 17.6 % at a noise of 18 %.
        self.script.values["wall"] = self.wall(117.6, 117.6)
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "noise")
        self.assertEqual(self.roles(summary), "bcbcbcccc")

    def test_a_shift_above_the_noise_is_confirmed_by_three_more_pairs(self):
        self.script.values["wall"] = self.wall(120.0, 119.0)
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual([r["run"] for r in summary["runs"]],
                         ["A-b1", "A-c1", "A-b2", "A-c2", "A-b3", "A-c3",
                          "A-b4", "A-c4", "A-b5", "A-c5", "A-b6", "A-c6", "B-c1", "B-c2", "B-c3"])
        wall = self.row(summary, "graph_ready_s")
        self.assertEqual(wall["outcome"], "block")
        self.assertAlmostEqual(wall["delta"], 0.20)
        self.assertAlmostEqual(wall["confirm"]["delta"], 0.19)
        self.assertEqual(wall["confirm"]["base"]["value"], 100.0)

    def test_a_shift_the_second_round_does_not_repeat_is_noise(self):
        self.script.values["wall"] = self.wall(120.0, 110.0)
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        wall = self.row(summary, "graph_ready_s")
        self.assertEqual(wall["outcome"], "noise")
        self.assertAlmostEqual(wall["confirm"]["delta"], 0.10)

    def test_a_busy_machine_in_the_confirming_round_leaves_the_gate_incomplete(self):
        self.script.values["wall"] = self.wall(120.0, 120.0)
        self.script.values["foreign"] = lambda run: 8.0 if (run.role, run.n, run.attempt) == ("b", 4, 1) else 0.0
        code, summary, _ = self.gate("--busy-budget-min", "1")
        self.assertEqual((code, summary["verdict"], summary["reasons"][0]), (2, "incomplete", "machine_busy"))
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "confirm")
        self.assertEqual(summary["runs"][-1]["run"], "A-b4")
        self.assertNotIn("B", {r["series"] for r in summary["runs"]})

    def test_the_confirming_round_does_not_decide_the_work_metrics(self):
        # CPU +10 % is a block of its own; the wall confirmation only keeps the wall row.
        self.script.values["wall"] = self.wall(120.0, 100.0)
        self.script.values["cpu"] = lambda run: 44.0 if run.role == "c" else 40.0
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["outcome"], "block")
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "noise")


class Answers(GateCase):
    def test_answers_equal_differ_and_control(self):
        # Equal.
        _, summary, _ = self.gate(label="equal")
        self.assertEqual((summary["answers"]["outcome"], summary["answers"]["new_codes"]), ("ok", []))
        self.assertTrue(all(not p["differ"] for p in summary["answers"]["pairs"]))
        self.assertEqual(len(summary["answers"]["pairs"]), 3)
        # One more code on the candidate: listed as new, the verdict stays.
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=sweep_extra_code")
        code, summary, _ = self.gate(label="extra")
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertNotIn("answers_differ", summary["reasons"])
        self.assertEqual(summary["answers"]["outcome"], "ok")
        self.assertEqual(summary["answers"]["new_codes"], [{"code": "X", "count": 1, "files_affected": 1}])
        self.assertEqual([p["differ"] for p in summary["answers"]["pairs"]], [[]] * 3)
        report = self.report("extra")
        self.assertIn("## Новые коды", report)
        self.assertIn("| X | 1 | 1 |", report)
        # The same codes, one count apart: listed as changed, the verdict stays.
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=sweep_other_count")
        code, summary, _ = self.gate(label="count")
        self.assertEqual((code, summary["verdict"], summary["answers"]["outcome"]), (0, "pass", "ok"))
        self.assertEqual([c["code"] for c in summary["answers"]["changed_codes"]], ["BSL001"])
        self.assertIn("| BSL001 |", self.report("count"))
        # A resolve answer that differs byte for byte still blocks.
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=resolve_other")
        code, summary, _ = self.gate(label="resolve")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("answers_differ", summary["reasons"])
        self.assertEqual(summary["answers"]["outcome"], "block")
        # Answers that cannot differ: the control has nothing to tell them apart with.
        set_mode("graph_answers_same", "sweep_answers", "refs_tool")
        code, summary, _ = self.gate(label="control")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertIn("answers_control", summary["reasons"])
        self.assertEqual((summary["answers"]["outcome"], summary["answers"]["control"]), ("incomplete", False))

    def report(self, label):
        with open(self.data.path("gates", label, "report.md"), encoding="utf-8") as fh:
            return fh.read()


class AnswerFiles(unittest.TestCase):
    """`compare_answers` on answer files: what counts as a difference."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.addCleanup(self._tmp.cleanup)
        self.count = 0

    def launch(self, held=True, **override):
        self.count += 1
        answers = {name: [{"id": f"{name}-{i}"} for i in range(2)] for name in gate.RESOLVE_ANSWERS}
        answers["c-graph-resolve-extra-0"] = [{"id": "x0"}]
        answers["c-graph-resolve-extra-1"] = [{"id": "x1"}]
        answers[gate.SWEEP_ANSWER] = [{"code": "A", "count": 1, "files_affected": 1, "severity": "warning"},
                                      {"code": "B", "count": 2, "files_affected": 2, "severity": "error"}]
        answers.update(override)
        run_dir = os.path.join(self._tmp.name, f"run{self.count}")
        for name, value in answers.items():
            if value is not None:
                os.makedirs(os.path.join(run_dir, "answers"), exist_ok=True)
                with open(os.path.join(run_dir, "answers", f"{name}.json"), "wb") as fh:
                    fh.write(json.dumps(value, sort_keys=True, ensure_ascii=False).encode("utf-8"))
        return {"run": f"run{self.count}", "dir": run_dir, "held": held}

    def compare(self, base=None, cand=None):
        return gate.compare_answers([(self.launch(**(base or {})), self.launch(**(cand or {})))])

    def test_equal_answers_with_a_working_control_are_ok(self):
        result = self.compare()
        self.assertEqual((result["outcome"], result["control"], result["new_codes"], result["vanished_codes"],
                          result["changed_codes"]), ("ok", True, [], [], []))

    SWEEP = [{"code": "A", "count": 1, "files_affected": 1, "severity": "warning"},
             {"code": "B", "count": 2, "files_affected": 2, "severity": "error"}]

    @staticmethod
    def fields(aggregate):
        return {k: v for k, v in aggregate.items() if k != "code"}

    def sweep_of(self, cand):
        result = self.compare(cand={gate.SWEEP_ANSWER: cand})
        self.assertEqual((result["outcome"], result["pairs"][0]["differ"]), ("ok", []))
        return result

    def test_a_new_code_is_listed_and_does_not_block(self):
        added = [{"code": "C", "count": 160, "files_affected": 92, "severity": "hint"},
                 {"code": "D", "count": 3, "files_affected": 1, "severity": "error"}]
        result = self.sweep_of([*self.SWEEP, *added])
        self.assertEqual(result["new_codes"], [{"code": "C", "count": 160, "files_affected": 92},
                                               {"code": "D", "count": 3, "files_affected": 1}])
        self.assertEqual((result["vanished_codes"], result["changed_codes"]), ([], []))
        self.assertEqual(result["pairs"][0]["new"], ["C", "D"])

    def test_a_vanished_code_is_listed_and_does_not_block(self):
        result = self.sweep_of([self.SWEEP[0]])
        self.assertEqual(result["vanished_codes"], [{"code": "B", "count": 2, "files_affected": 2}])
        self.assertEqual((result["new_codes"], result["changed_codes"]), ([], []))

    def test_a_changed_code_is_listed_with_both_sides_and_does_not_block(self):
        variants = {"count": {**self.SWEEP[0], "count": 2}, "files_affected": {**self.SWEEP[0], "files_affected": 5},
                    "severity": {**self.SWEEP[0], "severity": "hint"}}
        for name, changed in variants.items():
            with self.subTest(name=name):
                result = self.sweep_of([changed, self.SWEEP[1]])
                self.assertEqual(result["changed_codes"], [{"code": "A", "base": self.fields(self.SWEEP[0]),
                                                            "candidate": self.fields(changed)}])
                self.assertEqual((result["new_codes"], result["vanished_codes"]), ([], []))

    def test_a_code_changing_differently_in_two_pairs_keeps_both_rows(self):
        pairs = [(self.launch(), self.launch(**{gate.SWEEP_ANSWER: [{**self.SWEEP[0], "count": count}, self.SWEEP[1]]}))
                 for count in (2, 3)]
        result = gate.compare_answers(pairs)
        self.assertEqual((result["outcome"], [c["candidate"]["count"] for c in result["changed_codes"]]),
                         ("ok", [2, 3]))

    def test_new_vanished_and_changed_codes_together_leave_the_outcome_ok(self):
        result = self.sweep_of([{**self.SWEEP[0], "count": 9},
                                {"code": "C", "count": 1, "files_affected": 1, "severity": "hint"}])
        self.assertEqual(([c["code"] for c in result["new_codes"]], [c["code"] for c in result["vanished_codes"]],
                          [c["code"] for c in result["changed_codes"]]), (["C"], ["B"], ["A"]))

    def test_a_resolve_answer_that_differs_byte_for_byte_blocks(self):
        for name in gate.RESOLVE_ANSWERS:
            with self.subTest(name=name):
                result = self.compare(cand={name: [{"id": "other"}] if name != "c-graph-resolve-extra-0" else
                                            [{"id": "x0"}, {"id": "more"}]})
                self.assertEqual(result["outcome"], "block")
                self.assertEqual(result["pairs"][0]["differ"], [name])

    def test_an_answer_one_side_lacks_differs(self):
        result = self.compare(cand={"d-graph-resolve-3": None})
        self.assertEqual((result["outcome"], result["pairs"][0]["differ"]), ("block", ["d-graph-resolve-3"]))

    def test_the_control_wants_two_different_non_empty_extra_answers(self):
        # Both sides give the same answers, so only the control can tell.
        for name, extra in (("equal", {"c-graph-resolve-extra-1": [{"id": "x0"}]}),
                            ("empty second", {"c-graph-resolve-extra-1": []}),
                            ("empty first", {"c-graph-resolve-extra-0": []}),
                            ("both empty", {"c-graph-resolve-extra-0": [], "c-graph-resolve-extra-1": []}),
                            ("missing", {"c-graph-resolve-extra-1": None})):
            with self.subTest(name=name):
                result = self.compare(base=extra, cand=extra)
                self.assertEqual((result["outcome"], result["control"], result["pairs"][0]["differ"]),
                                 ("incomplete", False, []))

    def test_a_difference_outranks_a_missing_control(self):
        result = self.compare(cand={"c-graph-resolve-extra-1": [{"id": "x0"}], "d-graph-resolve-3": []})
        self.assertEqual(result["outcome"], "block")

    def test_a_pair_with_a_run_that_did_not_hold_is_not_compared(self):
        bad_base = self.launch(held=False, **{gate.SWEEP_ANSWER: []})
        good = (self.launch(), self.launch())
        result = gate.compare_answers([(bad_base, self.launch()), good])
        self.assertEqual((result["outcome"], len(result["pairs"])), ("ok", 1))
        self.assertEqual(gate.compare_answers([(bad_base, self.launch())])["pairs"], [])
        self.assertEqual(gate.compare_answers([])["outcome"], "incomplete")

    def test_the_control_reads_the_first_candidate_run_that_held(self):
        same = {"c-graph-resolve-extra-1": [{"id": "x0"}]}
        control_ok = (self.launch(), self.launch())
        no_control = (self.launch(**same), self.launch(**same))
        self.assertEqual(gate.compare_answers([(self.launch(), self.launch(held=False)), control_ok])["outcome"],
                         "ok")
        self.assertEqual(gate.compare_answers([no_control, control_ok])["outcome"], "incomplete")


def run_record(metrics, held=True, reason=None):
    return {"k": 1, "runs": 3, "held": held, "reason": reason, "metrics": metrics, "calls": []}


class Ceilings(GateCase):
    def series_a(self, *peaks):
        return [run_record({"hwm_kb": p}) for p in peaks]

    def series_b(self, peaks=(1, 1, 1), refs=(60.0, 30.0, 50.0)):
        return [run_record({"hwm_kb": p, "references_1_s": refs[0], "references_2_same_s": refs[1],
                            "references_3_other_s": refs[2]}) for p in peaks]

    def outcomes(self, a, b):
        return {r["metric"]: r["outcome"] for r in gate.ceiling_rows(a, b)}

    def test_ceilings(self):
        ok = self.series_a(1, 1, 1)
        self.assertEqual(set(self.outcomes(ok, self.series_b()).values()), {"ok"})
        # The peak: any run of any series, strictly above 7168 MiB.
        self.assertEqual(self.outcomes(self.series_a(1, LIMIT_KB, 1), self.series_b())["hwm_kb"], "ok")
        self.assertEqual(self.outcomes(self.series_a(1, LIMIT_KB + 1, 1), self.series_b())["hwm_kb"], "block")
        self.assertEqual(self.outcomes(ok, self.series_b((1, 1, LIMIT_KB + 1)))["hwm_kb"], "block")
        self.assertEqual(self.outcomes(ok, self.series_b((LIMIT_KB, 1, 1)))["hwm_kb"], "ok")
        # The medians of the three references calls: 67.7, 32.3 and 59.5 s are still ok.
        for index, (key, limit) in enumerate((("references_1_s", 67.7), ("references_2_same_s", 32.3),
                                              ("references_3_other_s", 59.5))):
            refs = [60.0, 30.0, 50.0]
            with self.subTest(key=key):
                refs[index] = limit
                self.assertEqual(self.outcomes(ok, self.series_b(refs=refs))[key], "ok")
                refs[index] = round(limit + 0.1, 1)
                self.assertEqual(self.outcomes(ok, self.series_b(refs=refs))[key], "block")
                self.assertEqual({k: v for k, v in self.outcomes(ok, self.series_b(refs=refs)).items()
                                  if k != key}.values().__len__(), 3)
        # A median, not a worst run: one slow run of three does not block.
        b = [run_record({"hwm_kb": 1, "references_1_s": v, "references_2_same_s": 1.0, "references_3_other_s": 1.0})
             for v in (67.7, 67.7, 99.0)]
        self.assertEqual(self.outcomes(ok, b)["references_1_s"], "ok")
        b = [run_record({"hwm_kb": 1, "references_1_s": v, "references_2_same_s": 1.0, "references_3_other_s": 1.0})
             for v in (67.8, 67.8, 1.0)]
        self.assertEqual(self.outcomes(ok, b)["references_1_s"], "block")

    def test_a_series_that_did_not_hold_leaves_no_ceiling_decision(self):
        failed = self.series_b()
        failed[1].update(held=False, reason="call_failed")
        self.assertEqual(set(self.outcomes(self.series_a(1, 1, 1), failed).values()), {"block"})
        self.assertEqual(set(self.outcomes(self.series_a(1, 1, 1), []).values()), {"incomplete"})
        self.assertEqual(self.outcomes(self.series_a(1, 1, 1), self.series_b())["hwm_kb"], "ok")

    def test_the_ceilings_decide_the_gate(self):
        self.script.values["hwm_kb"] = lambda run: LIMIT_KB + 1 if run.series == "B" else 1 << 20
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "hwm_kb")["outcome"], "block")
        self.assertEqual(self.row(summary, "hwm_kb")["candidate"]["value"], LIMIT_KB + 1)
        self.script.values["hwm_kb"] = LIMIT_KB
        self.script.values["elapsed"] = lambda run, call: 67.8 if call == "references_1" else None
        code, summary, _ = self.gate(label="g2")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "hwm_kb")["outcome"], "ok")
        self.assertEqual(self.row(summary, "references_1_s")["outcome"], "block")
        self.assertEqual(self.row(summary, "references_2_same_s")["outcome"], "ok")


class FailedSides(GateCase):
    def test_failed_side_outcomes(self):
        # The candidate's server exits after `initialize`: the gate blocks.
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=cand_crash")
        code, summary, _ = self.gate(label="cand")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual({r["outcome"] for r in summary["rows"]}, {"block"})
        self.assertEqual({r["reason"] for r in summary["runs"] if r["role"] == "candidate"}, {"server_failed"})
        self.assertTrue(all(r["held"] for r in summary["runs"] if r["role"] == "base"))
        # The base's server does: no decision, never `pass` (the nightly table says `fixed` for it).
        set_mode(*self.modes.split(","), f"{BASE_TAG}=cand_crash")
        code, summary, _ = self.gate(label="base")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual({r["outcome"] for r in summary["rows"]}, {"incomplete"})
        self.assertNotIn("block", {r["outcome"] for r in summary["rows"] + summary["ceilings"]})
        # Both: the candidate failed.
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=cand_crash", f"{BASE_TAG}=cand_crash")
        code, summary, _ = self.gate(label="both")
        self.assertEqual((code, summary["verdict"]), (1, "block"))


class Refusals(GateCase):
    def test_existing_label_refused(self):
        out_dir = self.data.path("gates", "g1")
        os.makedirs(out_dir)
        old = os.path.join(out_dir, "summary.json")
        write_json(old, {"verdict": "pass"})
        before = (os.stat(old).st_mtime_ns, read_bytes(old), os.listdir(out_dir))
        with self.assertRaisesRegex(HarnessError, "gate_exists"):
            gate.cmd_gate(self.args(label="g1"), self.clock, self.builder, self.machine)
        with contextlib.redirect_stderr(io.StringIO()) as err:
            code = perf_nightly.main(["gate", "--data-dir", self.data_dir, "--base", BASE_TAG, "--candidate", "HEAD",
                                      "--label", "g1"])
        self.assertEqual(code, 2)
        self.assertIn("gate_exists", err.getvalue())
        self.assertEqual((os.stat(old).st_mtime_ns, read_bytes(old), os.listdir(out_dir)), before)
        self.assertEqual((self.builds, self.launches()), ([], []))

    def test_a_gate_in_progress_refuses_a_second_one(self):
        with open(self.data.path("gate.lock"), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with self.assertRaisesRegex(HarnessError, "gate_busy"):
                gate.cmd_gate(self.args(label="g1"), self.clock, self.builder, self.machine)
            with self.assertRaisesRegex(HarnessError, "gate_busy"):
                gate.cmd_gate_calibrate(self.args("gate-calibrate"), self.clock, self.builder, self.machine)
        self.assertFalse(os.path.exists(self.data.path("gates", "g1")))

    def test_a_label_and_a_budget_are_checked_before_anything_is_made(self):
        for label in ("../x", "", "a b", "-x", "a/b"):
            with self.subTest(label=label):
                args = self.args(label="g1")
                args.label = label
                with self.assertRaisesRegex(HarnessError, "label_invalid"):
                    gate.cmd_gate(args, self.clock, self.builder, self.machine)
        for budget in ("-1", "nan", "inf"):
            with self.subTest(budget=budget):
                with self.assertRaisesRegex(HarnessError, "budget_invalid"):
                    gate.cmd_gate(self.args("gate", "g1", "HEAD", "--busy-budget-min", budget), self.clock,
                                  self.builder, self.machine)
        self.assertFalse(os.path.exists(self.data.path("gates")))

    def test_source_mismatch_refused(self):
        with open(os.path.join(self.erp, "f.txt"), "w") as fh:
            fh.write("changed\n")
        code, summary, _ = self.gate(label="dirty")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertTrue(summary["reasons"][0].startswith("source_mismatch"), summary["reasons"])
        self.assertEqual((self.builds, self.launches(), summary["runs"]), ([], [], []))
        run_git("checkout", "--", "f.txt", cwd=self.erp)
        # Not at the commit of the manifest.
        with open(os.path.join(self.erp, "f.txt"), "w") as fh:
            fh.write("next\n")
        run_git("commit", "-q", "-am", "next", cwd=self.erp)
        code, summary, _ = self.gate(label="moved")
        self.assertEqual((code, summary["reasons"][0].split(":")[0]), (2, "source_mismatch"))
        self.assertEqual((self.builds, self.launches()), ([], []))

    def test_source_mismatch_is_not_raised_for_what_the_gate_does_not_read(self):
        with open(os.path.join(self.erp, "untracked.txt"), "w") as fh:
            fh.write("x\n")
        self.assertEqual(gate.check_source(self.data)["commit"], self.erp_commit)

    def test_a_source_without_its_config_or_corpus_is_refused(self):
        run_git("rm", "-q", "bsl-analyzer.toml", cwd=self.erp)
        run_git("commit", "-q", "-m", "drop the config", cwd=self.erp)
        self.write_manifest(commit=self.rev(self.erp))
        with self.assertRaisesRegex(HarnessError, "source_mismatch.*bsl-analyzer.toml"):
            gate.check_source(self.data)
        self.write_manifest(corpus_id="crm")
        with self.assertRaisesRegex(HarnessError, "manifest_invalid"):
            gate.check_source(self.data)

    def test_a_base_that_is_not_a_release_tag_is_incomplete(self):
        args = self.args("gate", "g1", "HEAD")
        args.base = "develop"
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_gate(args, self.clock, self.builder, self.machine)
        summary = read_json(self.data.path("gates", "g1", "summary.json"))
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertTrue(summary["reasons"][0].startswith("base_invalid"), summary["reasons"])
        self.assertEqual(self.builds, [])

    def test_a_failed_build_or_an_unknown_candidate_is_incomplete_with_a_summary(self):
        def failing(ref, label):
            raise HarnessError("build_failed", "cargo exited 101, log: /x")

        code, summary, _ = self.gate(label="build", builder=failing)
        self.assertEqual((code, summary["verdict"], summary["reasons"]), (2, "incomplete",
                                                                           ["build_failed: cargo exited 101, log: /x"]))
        code, summary, _ = self.gate(label="unknown", candidate="no-such-commit")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertTrue(summary["reasons"][0].startswith("git_failed"), summary["reasons"])
        self.assertTrue(os.path.isfile(self.data.path("gates", "unknown", "report.md")))
        self.assertEqual(self.launches(), [])


class Candidate(GateCase):
    def test_candidate_commit_fetched_from_local_repo(self):
        # The build clone knows the repository as it was; the candidate is a later, never pushed commit.
        clone = self.data.path("build")
        run_git("clone", "-q", "--no-checkout", self.repo, clone, cwd=self.tmp)
        with open(os.path.join(self.repo, "f.txt"), "a") as fh:
            fh.write("later\n")
        run_git("commit", "-q", "-am", "later", cwd=self.repo)
        later = self.rev(self.repo)
        self.assertEqual(common.git("rev-parse", "--verify", "-q", f"{later}^{{commit}}", cwd=clone, check=False), "")
        built = []

        def builder(ref, label):
            built.append((ref, label))
            if ref == BASE_TAG:
                return self.base_path
            # What `build_ref` does: it can only build what its clone has.
            common.git("rev-parse", "--verify", f"{ref}^{{commit}}", cwd=clone)
            return self.fake_binary("rc", ref)

        code, summary, _ = self.gate("--ready-only", candidate=later, builder=builder)
        self.assertEqual((code, summary["verdict"]), (0, "probe-ok"), summary["reasons"])
        self.assertEqual(summary["candidate"], {"label": "rc", "sha": later})
        self.assertEqual(built, [(BASE_TAG, BASE_TAG), (later, "rc")])
        self.assertEqual(common.git("rev-parse", "--verify", f"{later}^{{commit}}", cwd=clone), later)

    def test_a_commit_the_clone_does_not_know_is_not_built_without_the_fetch(self):
        clone = self.data.path("build")
        run_git("clone", "-q", "--no-checkout", self.repo, clone, cwd=self.tmp)
        with open(os.path.join(self.repo, "f.txt"), "a") as fh:
            fh.write("later\n")
        run_git("commit", "-q", "-am", "later", cwd=self.repo)

        def builder(ref, label):
            if ref != BASE_TAG:
                common.git("rev-parse", "--verify", f"{ref}^{{commit}}", cwd=clone)
            return self.base_path

        with patch.object(gate, "git", lambda *a, **k: common.git(*a, **k) if a[0] != "fetch" else ""):
            code, summary, _ = self.gate("--ready-only", label="nofetch", candidate="HEAD", builder=builder)
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertTrue(summary["reasons"][0].startswith("git_failed"), summary["reasons"])

    def test_a_tag_candidate_keeps_its_name_as_the_label_and_a_commit_gets_rc(self):
        run_git("tag", "v0.4.1", cwd=self.repo)
        self.gate("--ready-only", candidate="v0.4.1", label="tag")
        self.gate("--ready-only", candidate=self.rev(self.repo)[:12], label="short")
        self.assertEqual(self.builds, [(BASE_TAG, BASE_TAG), (self.rev(self.repo), "v0.4.1"),
                                       (BASE_TAG, BASE_TAG), (self.rev(self.repo), "rc")])


    def test_the_default_builder_builds_both_refs_under_the_build_lock(self):
        calls = []

        def lock_held():
            with open(self.data.path("build.lock"), "w") as probe:
                try:
                    fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return True
            return False

        def clone(data, remote=None):
            calls.append(("clone", lock_held()))

        def build_ref(data, ref, label=None):
            calls.append((ref, label, lock_held()))
            return (self.base_path if ref == BASE_TAG else self.cand_path), False

        with patch.object(perf_nightly, "build_clone", clone), patch.object(perf_nightly, "build_ref", build_ref):
            base, cand = gate.build_pair(self.data, BASE_TAG, "HEAD", None)
            calls.append(("after", lock_held()))
            self.assertEqual(gate.build_pair(self.data, BASE_TAG, None, None)[1], None)
            with self.assertRaisesRegex(HarnessError, "base_invalid"):
                gate.build_pair(self.data, "v0.4", "HEAD", None)
        head = self.rev(self.repo)
        self.assertEqual(calls[:5], [("clone", True), (BASE_TAG, None, True), (head, "rc", True), ("after", False),
                                     ("clone", True)])
        self.assertEqual((base["label"], cand["label"]), (BASE_TAG, CAND_LABEL))
        self.assertEqual(calls[5:], [(BASE_TAG, None, True)], "a calibration builds the base alone; a refused base nothing")


class StaleNoise(GateCase):
    def test_stale_noise_refuses_before_build(self):
        fingerprint = self.fingerprint()

        def changed(key, value="other"):
            return {"fingerprints": {"erp": {**fingerprint, key: value}}}

        cases = {
            "no gate-noise.json": None,
            "cpu_model": changed("cpu_model"),
            "nproc": changed("nproc", 1),
            "gate_schema in the fingerprint": changed("gate_schema", 2),
            "gate_schema": {"gate_schema": 2},
            "harness_schema": {"harness_schema": 2},
            "source_commit": changed("source_commit", "0" * 40),
            "config_sha256": changed("config_sha256", "0" * 64),
            "no fingerprint": {"fingerprints": {}},
        }
        for index, (name, fields) in enumerate(cases.items()):
            with self.subTest(name=name):
                if fields is None:
                    os.remove(self.data.path("gate-noise.json"))
                else:
                    self.write_noise(**fields)
                code, summary, _ = self.gate(label=f"stale{index}")
                self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
                self.assertTrue(summary["reasons"][0].startswith("noise_stale"), (name, summary["reasons"]))
                self.assertNotEqual(summary["noise"]["status"], "current")
                self.assertEqual((self.builds, self.launches(), summary["runs"]), ([], [], []), name)

    def test_what_the_nightly_calibration_does_not_compare_is_not_stale(self):
        fingerprint = self.fingerprint()
        for key in ("kernel", "rustc", "mem_total"):
            with self.subTest(key=key):
                noise = self.write_noise(fingerprints={"erp": {**fingerprint, key: "another"}})
                self.assertNotEqual(fingerprint[key], noise["fingerprints"]["erp"][key])
                self.assertIsNone(gate.noise_stale(noise, fingerprint))
        self.assertIsNone(gate.noise_stale(self.write_noise(), fingerprint))

    def test_a_noise_the_gate_cannot_use_is_not_a_calibration(self):
        # A metric without a noise stops the verdict: `no_noise` in the rows, `incomplete` as the verdict.
        noise = self.write_noise()
        del noise["entries"]["erp/mcp-graph/cold/graph_resolve_s"]
        write_json(self.data.path("gate-noise.json"), noise)
        code, summary, _ = self.gate("--busy-budget-min", "60")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual(self.row(summary, "graph_resolve_s")["outcome"], "no_noise")
        self.assertIn("no_noise: graph_resolve_s", summary["reasons"])


class ReadyOnly(GateCase):
    def test_ready_only_never_passes(self):
        code, summary, line = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"]), (0, "probe-ok"))
        self.assertTrue(line.startswith("gate=probe-ok "), line)
        self.assertEqual(self.roles(summary), "bcbcbc")
        self.assertEqual([r["metric"] for r in summary["rows"]], ["graph_ready_s", "graph_ready_cpu_s"])
        self.assertEqual((summary["answers"], summary["ceilings"]), (None, []))
        self.assertTrue(summary["protocol"]["ready_only"])
        calls = [(t["name"], t["arguments"].get("action")) for t in self.tools()]
        self.assertEqual(set(calls), {("diagnostics", "status"), ("graph", "status")})
        self.assertEqual(self.clock.sleeps, [], "no pause of the protocol after readiness")
        for run_dir in os.listdir(self.data.path("gates", "g1", "runs")):
            record = read_json(self.data.path("gates", "g1", "runs", run_dir, "r.json"))
            self.assertNotIn("graph_resolve_s", record["metrics"])
            self.assertNotIn("diagnostics_workspace_s", record["metrics"])
            self.assertFalse(os.path.exists(self.data.path("gates", "g1", "runs", run_dir, "answers")))

    def test_a_probe_can_still_block(self):
        self.script.values["cpu"] = lambda run: 44.0 if run.role == "c" else 40.0
        code, summary, _ = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"]), (1, "block"))


class Controls(GateCase):
    def setUp(self):
        super().setUp()
        self.spawned, self.clients = [], []
        original_spawn = gate._spawn

        def spy_spawn(code, args, env):
            proc = original_spawn(code, args, env)
            self.spawned.append({"code": code, "args": list(args), "env": env, "proc": proc})
            return proc

        class SpyClient(scenarios.McpClient):
            def __init__(client, cmd, cwd, env, stderr, clock):
                super().__init__(cmd, cwd, env, stderr, clock)
                owner.clients.append({"cmd": list(cmd), "pid": client.proc.pid, "marker": env[procs.MARKER]})

        owner = self
        self.enterContext(patch.object(gate, "_spawn", spy_spawn))
        self.enterContext(patch.object(scenarios, "McpClient", SpyClient))

    def test_controls_take_effect(self):
        self.burn()
        self.delay()
        self.foreign()

    def burn(self):
        # The server itself is idle here, so what the candidate shows is what the burners burned.
        self.script.values["cpu"] = 0.0
        code, summary, _ = self.gate("--ready-only", "--control-burn-cpu-s", "0.3", label="burn")
        runs = {r["run"]: read_json(self.data.path("gates", "burn", "runs", r["run"], "r.json"))
                for r in summary["runs"]}
        for name, record in runs.items():
            if name.startswith("A-c"):
                self.assertGreaterEqual(record["metrics"]["graph_ready_cpu_s"], 0.3, name)
                self.assertEqual(record["controls"], {"control_burn_cpu_s": 0.3})
            else:
                self.assertEqual(record["metrics"]["graph_ready_cpu_s"], 0.0, name)
                self.assertIsNone(record["controls"], name)
        self.assertEqual(summary["controls"], {"control_burn_cpu_s": 0.3})
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["outcome"], "block")
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "ok")
        burners = [s for s in self.spawned if s["code"] == gate.BURN_CODE]
        # max(4, ⌈0.3 / (0.5 · 100)⌉) burners per candidate run, each burning an equal share, carrying the run's marker.
        self.assertEqual(len(burners), 12)
        self.assertEqual({s["args"][0] for s in burners}, {0.3 / 4})
        markers = {c["marker"] for c in self.clients if c["cmd"][0] == self.cand_path}
        self.assertEqual({s["env"][procs.MARKER] for s in burners}, markers)
        self.assertTrue(all(s["proc"].poll() is not None for s in burners))
        self.assertEqual(gate.burn_count(0.3, {"entries": {}}), 4)
        noise = self.write_noise()
        self.assertEqual(gate.burn_count(100.0, noise), 4)
        self.assertEqual(gate.burn_count(1000.0, noise), 20)

    def delay(self):
        self.spawned.clear(), self.clients.clear()
        self.launches_before = len(self.launches())
        code, summary, _ = self.gate("--ready-only", "--control-start-delay-s", "1e-06", label="delay")
        self.assertEqual(summary["controls"], {"control_start_delay_s": 1e-06})
        launches = self.launches()[self.launches_before:]
        by_pid = {launch["pid"]: launch for launch in launches}
        self.assertEqual((len(launches), len(by_pid), len(self.clients)), (6, 6, 6),
                         "the wrapper becomes the server: one process per run")
        wrapped = [c for c in self.clients if c["cmd"][0] != self.base_path]
        self.assertEqual(len(wrapped), 3)
        for client in self.clients:
            if client in wrapped:
                self.assertEqual(client["cmd"][:5], [sys.executable, "-c", gate.DELAY_CODE, "1e-06", self.cand_path])
            else:
                self.assertEqual(client["cmd"][0], self.base_path)
            self.assertEqual(by_pid[client["pid"]]["argv"][:2], ["mcp", "serve"])
            self.assertEqual(by_pid[client["pid"]]["env"][procs.MARKER], client["marker"])
        for name in ("A-c1", "A-c2", "A-c3"):
            record = read_json(self.data.path("gates", "delay", "runs", name, "r.json"))
            self.assertEqual(record["controls"], {"control_start_delay_s": 1e-06})
        record = read_json(self.data.path("gates", "delay", "runs", "A-b1", "r.json"))
        self.assertIsNone(record["controls"])

    def foreign(self):
        self.spawned.clear(), self.clients.clear()
        alive = []
        machine = self.machine

        def busy_s():
            if self.script.stage == "ready":
                spinners = [s for s in self.spawned if s["code"] == gate.SPIN_CODE][-2:]
                tracked = procs.tracked(self.script.run_ids[-1])
                alive.append({"alive": [s["proc"].poll() is None for s in spinners],
                              "tracked": [s["proc"].pid in tracked for s in spinners], "server": len(tracked)})
            return ScriptedMachine.busy_s(machine)

        machine.busy_s = busy_s
        code, summary, _ = self.gate("--ready-only", "--control-foreign-burn", "2", label="foreign")
        self.assertEqual((code, summary["verdict"]), (0, "probe-ok"))
        self.assertEqual(summary["controls"], {"control_foreign_burn": 2})
        spinners = [s for s in self.spawned if s["code"] == gate.SPIN_CODE]
        self.assertEqual(len(spinners), 12, "two in every run, the base's too")
        self.assertTrue(all(procs.MARKER not in s["env"] for s in spinners))
        self.assertEqual(len(alive), 6)
        for window in alive:
            self.assertEqual(window["alive"], [True, True], "the load lives through the readiness window")
            self.assertEqual(window["tracked"], [False, False], "and is not in the tracked set of the run")
            self.assertGreaterEqual(window["server"], 1)
        self.assertTrue(all(s["proc"].poll() is not None for s in spinners), "none outlives its run")
        for name in ("A-b1", "A-c1"):
            record = read_json(self.data.path("gates", "foreign", "runs", name, "r.json"))
            self.assertEqual(record["controls"], {"control_foreign_burn": 2})


class Calibration(GateCase):
    def setUp(self):
        super().setUp()
        os.remove(self.data.path("gate-noise.json"))

    def test_calibrate_writes_gate_noise(self):
        # Five series whose medians of the readiness are 10, 11, 12, 13 and 14 s.
        self.script.values["wall"] = lambda run: [10.0, 11.0, 12.0, 13.0, 14.0][(len(self.script.runs) - 1) // 3]
        code, line = self.calibrate("--series", "5")
        self.assertEqual(code, 0, line)
        noise = read_json(self.data.path("gate-noise.json"))
        entries = noise["entries"]
        self.assertEqual(sorted(entries), sorted(f"erp/mcp-graph/cold/{k}" for k in
                                                 ("graph_ready_s", "graph_ready_cpu_s", "graph_resolve_s",
                                                  "diagnostics_workspace_s")))
        wall = entries["erp/mcp-graph/cold/graph_ready_s"]
        self.assertEqual(wall["medians"], [10.0, 11.0, 12.0, 13.0, 14.0])
        self.assertAlmostEqual(wall["noise"], (14 - 10) / 12)
        self.assertAlmostEqual(wall["noise"], 0.333, places=3)
        for key in ("graph_ready_cpu_s", "graph_resolve_s", "diagnostics_workspace_s"):
            self.assertEqual(entries[f"erp/mcp-graph/cold/{key}"]["noise"], 0.0)
        self.assertEqual((noise["schema"], noise["harness_schema"], noise["gate_schema"]),
                         (verdict.NOISE_SCHEMA, common.HARNESS_SCHEMA, gate.GATE_SCHEMA))
        self.assertEqual(noise["fingerprints"], {"erp": self.fingerprint()})
        self.assertEqual(noise["binary"], {"label": BASE_TAG, "sha": BASE_SHA,
                                           "sha256": perf_nightly.load_binary(self.base_path)["sha256"]})
        self.assertIn("nu.graph_ready_s=0.333", line)
        self.assertIn("written=true", line)
        self.assertFalse(os.path.exists(self.data.path("noise.json")), "the nightly calibration is not touched")
        root = self.data.path("gate-cal", f"{BASE_TAG}-{BASE_SHA[:12]}")
        for series in range(1, 6):
            for k in range(1, 4):
                record = read_json(os.path.join(root, f"s{series}", f"r{k}.json"))
                self.assertTrue(record["held"])
                self.assertEqual(record["metrics"]["graph_ready_s"], 9.0 + series)
        self.assertEqual(self.builds, [(BASE_TAG, BASE_TAG)])
        # A calibration this gate accepts as current.
        self.assertIsNone(gate.noise_stale(noise, self.fingerprint()))

    def test_fewer_than_five_series_are_refused(self):
        for series in ("0", "1", "4", "-3"):
            with self.subTest(series=series):
                with self.assertRaisesRegex(HarnessError, "series_invalid"):
                    gate.cmd_gate_calibrate(self.args("gate-calibrate", None, None, "--series", series), self.clock,
                                            self.builder, self.machine)
        self.assertEqual((self.builds, self.launches()), ([], []))
        self.assertFalse(os.path.exists(self.data.path("gate-noise.json")))

    def test_a_calibration_that_did_not_hold_is_not_written(self):
        stale = self.write_noise()
        before = read_bytes(self.data.path("gate-noise.json"))
        set_mode(*self.modes.split(","), f"{BASE_TAG}=cand_crash")
        code, line = self.calibrate()
        self.assertEqual(code, 2, line)
        self.assertIn("written=false", line)
        self.assertEqual(read_bytes(self.data.path("gate-noise.json")), before)
        self.assertEqual(stale["entries"], read_json(self.data.path("gate-noise.json"))["entries"])

    def test_a_series_cut_short_by_a_busy_machine_is_named(self):
        self.machine.quiet_of = lambda n: False
        code, line = self.calibrate()
        self.assertEqual(code, 2)
        self.assertIn("busy=s1,s2,s3,s4,s5", line)
        self.assertIn("written=false", line)
        self.assertFalse(os.path.exists(self.data.path("gate-noise.json")))

    def test_a_calibration_needs_the_source_of_the_manifest(self):
        with open(os.path.join(self.erp, "f.txt"), "w") as fh:
            fh.write("changed\n")
        with self.assertRaisesRegex(HarnessError, "source_mismatch"):
            self.calibrate()
        self.assertEqual(self.builds, [])


class Report(GateCase):
    wall_noise = 0.18

    def read(self, label):
        with open(self.data.path("gates", label, "report.md"), encoding="utf-8") as fh:
            return fh.read()

    def test_report_shows_wall_resolution(self):
        self.script.values["wall"] = candidate_slower(1.176)
        code, summary, _ = self.gate("--ready-only")
        report = self.read("g1")
        self.assertIn("решает от Δ > max(5 %, ν) = 18.0 % (ν = 18.0 %)", report)
        self.assertIn("| graph_ready_s | wall | 100.0 | 117.6 | +17.6% | 18.0% | 18.0% | - | noise |", report)
        self.assertIn("- вердикт: **probe-ok**", report)

    def test_the_limit_is_the_bar_when_the_noise_is_smaller(self):
        summary = {"verdict": "pass", "reasons": [], "base": {"label": "v", "sha": "s"},
                   "candidate": {"label": "c", "sha": "t"}, "controls": None, "noise": {"status": "current"},
                   "rows": [{"metric": "graph_ready_s", "kind": "wall", "base": {"state": "valid", "value": 1.0},
                             "candidate": {"state": "valid", "value": 1.0}, "delta": 0.0, "noise": 0.03,
                             "limit": 0.05, "outcome": "ok"}]}
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as tmp:
            gate.write_report(tmp, summary)
            with open(os.path.join(tmp, "report.md"), encoding="utf-8") as fh:
                report = fh.read()
        self.assertIn("решает от Δ > max(5 %, ν) = 5.0 % (ν = 3.0 %)", report)

    def test_the_report_of_a_full_gate_lists_every_table(self):
        self.script.values["wall"] = candidate_slower(1.5)
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=sweep_extra_code")
        code, summary, _ = self.gate()
        report = self.read("g1")
        for heading in ("## Правило", "## Потолки", "## Ответы", "## Новые коды", "## Исчезнувшие коды",
                        "## Изменённые коды", "## Прогоны"):
            self.assertIn(heading, report)
        self.assertIn("- вердикт: **block**", report)
        self.assertNotIn("answers_differ", report)
        self.assertIn("| A-c1 | candidate | A | true | - | 0.000 |", report)


class Machine(unittest.TestCase):
    def test_the_fingerprint_names_what_decides_comparability(self):
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as root:
            with open(os.path.join(root, "cpuinfo"), "w") as fh:
                fh.write("processor\t: 0\nmodel name\t: Test CPU 9000\n")
            with open(os.path.join(root, "meminfo"), "w") as fh:
                fh.write(f"MemTotal:       {63 << 20} kB\nMemAvailable:   1 kB\n")
            completed = type("Done", (), {"stdout": "rustc 1.99.0\n"})()
            with patch.object(gate.subprocess, "run", return_value=completed):
                print_ = gate.Machine(root).fingerprint()
        self.assertEqual(print_, {"cpu_model": "Test CPU 9000", "nproc": len(os.sched_getaffinity(0)),
                                  "mem_total": "63 GiB", "kernel": os.uname().release, "rustc": "rustc 1.99.0"})
        self.assertEqual(sorted(MACHINE), sorted(print_))


class DataDir(unittest.TestCase):
    def test_data_dir_in_repo_refused(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as tmp:
            repo = os.path.join(tmp, "harness-repo")
            harness = os.path.join(repo, "scripts", "perf-nightly")
            os.makedirs(harness)
            inside = os.path.join(harness, "no-such-data")
            with patch.dict(os.environ, env, clear=True):
                run_git("init", "-q", cwd=repo)
                with patch.object(common, "HERE", harness), contextlib.redirect_stderr(io.StringIO()) as err:
                    gate_code = perf_nightly.main(["gate", "--data-dir", inside, "--base", BASE_TAG, "--candidate",
                                                   "HEAD", "--label", "g1"])
                    calibrate_code = perf_nightly.main(["gate-calibrate", "--data-dir", inside, "--base", BASE_TAG])
            self.assertEqual((gate_code, calibrate_code), (2, 2))
            self.assertEqual(err.getvalue().count("data_dir_in_repo"), 2)
            self.assertFalse(os.path.exists(inside), "nothing was made in the work tree")


class CommandLine(unittest.TestCase):
    def test_the_commands_take_the_flags_of_the_plan(self):
        args = perf_nightly.parser().parse_args(
            ["gate", "--data-dir", "/d", "--base", "v0.4.1", "--candidate", "abc", "--label", "x", "--ready-only",
             "--busy-budget-min", "5", "--control-burn-cpu-s", "1.5", "--control-start-delay-s", "7",
             "--control-foreign-burn", "8"])
        self.assertEqual((args.ready_only, args.busy_budget_min, args.control_burn_cpu_s,
                          args.control_start_delay_s, args.control_foreign_burn),
                         (True, 5.0, 1.5, 7.0, 8))
        args = perf_nightly.parser().parse_args(["gate", "--data-dir", "/d", "--base", "v0.4.1", "--candidate", "a",
                                                 "--label", "x"])
        self.assertEqual((args.ready_only, args.busy_budget_min, args.control_burn_cpu_s,
                          args.control_start_delay_s, args.control_foreign_burn), (False, None, None, None, None))
        args = perf_nightly.parser().parse_args(["gate-calibrate", "--data-dir", "/d", "--base", "v0.4.1"])
        self.assertIsNone(args.series)
        args = perf_nightly.parser().parse_args(["gate-calibrate", "--data-dir", "/d", "--base", "v0.4.1",
                                                 "--series", "6"])
        self.assertEqual(args.series, 6)

    def test_the_commands_are_dispatched_to_the_gate(self):
        clock = FakeClock()
        calls = []
        with patch.object(gate, "cmd_gate", lambda *a: calls.append(("gate", a)) or 0), \
                patch.object(gate, "cmd_gate_calibrate", lambda *a: calls.append(("calibrate", a)) or 0):
            self.assertEqual(perf_nightly.cmd_gate("a", clock), 0)
            self.assertEqual(perf_nightly.cmd_gate_calibrate("b", clock), 0)
        self.assertEqual(calls, [("gate", ("a", clock)), ("calibrate", ("b", clock))])


if __name__ == "__main__":
    unittest.main()
