"""The criteria of the gate against the previous release and in the probe (plan
docs/plans/gate-ok-criteria.md, table of checks): the CPU as an observation (И1), the
trade of the time of resolve and of the sweep for the wall (И2), the checks that the new
rules leave in force (И7), the wall of the illustration of 0.4.4 (И8) and the independence
of a feature (И10). Every check reads the verdict, the exit code and the report that the
owner reads. The clock is virtual and the machine scripted (`gate_fixtures.py`); nothing
here waits for time."""

import os
import unittest
from unittest.mock import patch

from gate_fixtures import CAND_LABEL
from helpers import set_mode
from ok_criteria_fixtures import TRADED, CriteriaCase, feature_signals

import gate
import verdict
from common import read_json

BAR = verdict.GATE_BAR
ROUND_ONE_RUNS = 9
PROBE_RUNS = 6
OBSERVED = "наблюдение; на вердикт не влияет"
README = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "README.md")


class CpuAgainstThePreviousRelease(CriteriaCase):
    """И1 in the release and in the probe: the CPU is measured, shown with its numbers and decides nothing."""

    MODES = {"release": (), "probe": ("--ready-only",)}

    def setUp(self):
        super().setUp()
        # A record that is narrow on the CPU, so that a CPU which decided anything would block or ask for a round.
        self.set_record("graph_ready_cpu_s", 40.0, 0.01)

    def cpu(self, factor):
        self.script.values["cpu"] = lambda run: 40.0 * factor if run.role == "c" else 40.0

    def test_a_cpu_twice_or_half_the_base_changes_neither_the_verdict_nor_the_rounds(self):
        for mode, args in self.MODES.items():
            for factor in (2.0, 0.5):
                with self.subTest(mode=mode, factor=factor):
                    self.cpu(factor)
                    label = f"{mode}-{factor}"
                    code, summary, _ = self.gate(*args, label=label)
                    self.assertEqual((code, summary["verdict"]), (0, "pass" if mode == "release" else "probe-ok"))
                    row = self.row(summary, "graph_ready_cpu_s")
                    self.assertEqual((row["kind"], row["outcome"], row["limit"], row["reason"]),
                                     ("observation", "observation", None, None))
                    self.assertEqual((row["base"]["value"], row["candidate"]["value"]), (40.0, 40.0 * factor))
                    self.assertAlmostEqual(row["delta"], factor - 1)
                    self.assertNotIn("basis", row)
                    self.assertEqual(self.decided(summary), {"ok"})
                    self.assertEqual(len(summary["runs"]), ROUND_ONE_RUNS if mode == "release" else PROBE_RUNS)
                    self.assertEqual(summary["record_confirmation"]["requested"], False)
                    self.assertEqual(summary["record_proposals"], [])
                    cpu = self.record_row(summary, "graph_ready_cpu_s")
                    self.assertEqual((cpu["outcome"], cpu["limit"], cpu["candidate"]["value"]),
                                     ("observation", None, 40.0 * factor))
                    report = self.report_text(label)
                    self.assertIn(f"| graph_ready_cpu_s | observation | 40.000 | {40.0 * factor:.3f} | {(factor - 1) * 100:+.1f}% "
                                  f"| 1.0% | - | - | {OBSERVED} |", report)
                    self.assertIn(OBSERVED, self.section(report, "Рекорд"))
                    self.assertNotIn("рост CPU", report)

    def test_a_cpu_without_noise_or_record_is_told_in_its_row_and_the_gate_passes(self):
        cases = {"no gate noise": lambda: self.break_gate_noise("graph_ready_cpu_s"),
                 "no record noise": lambda: self.edit_baseline(
                     lambda doc: doc["metrics"]["graph_ready_cpu_s"]["noise"].update(value=None)),
                 "no record noise object": lambda: self.edit_baseline(
                     lambda doc: doc["metrics"]["graph_ready_cpu_s"].pop("noise")),
                 "no record": lambda: self.edit_baseline(lambda doc: doc["metrics"].pop("graph_ready_cpu_s"))}
        for mode, args in self.MODES.items():
            for name, edit in cases.items():
                with self.subTest(mode=mode, case=name):
                    self.write_baseline()
                    self.write_noise()
                    edit()
                    self.cpu(2.0)
                    code, summary, _ = self.gate(*args, label=f"{mode}-{name}".replace(" ", "-"))
                    self.assertEqual((code, summary["verdict"], summary["reasons"]),
                                     (0, "pass" if mode == "release" else "probe-ok", []))
                    row = self.row(summary, "graph_ready_cpu_s")
                    self.assertEqual((row["outcome"], row["candidate"]["value"]), ("observation", 80.0))
                    if name == "no gate noise":
                        self.assertEqual((row["noise"], row["reason"]), (None, "no_noise"))
                    self.assertEqual(summary["record"]["verdict"], "pass" if mode == "release" else "probe-ok")

    def test_a_cpu_that_was_not_measured_is_told_in_its_row_and_nothing_else_is_missed(self):
        base = [{"k": k, "runs": 3, "held": True, "reason": None, "calls": [], "metrics": {
            "graph_ready_s": 100.0, "graph_resolve_s": 17.8, "diagnostics_workspace_s": 22.1}} for k in (1, 2, 3)]
        noise = {"entries": {f"erp/mcp-graph/cold/{key}": {"noise": 0.01} for key in gate.METRICS}}
        rows = gate.rule_rows(base, base, noise, False)
        cpu = next(r for r in rows if r["metric"] == "graph_ready_cpu_s")
        self.assertEqual((cpu["outcome"], cpu["delta"], cpu["base"]["state"], cpu["candidate"]["state"]),
                         ("observation", None, "invalid", "invalid"))
        self.assertIn("candidate: invalid", cpu["reason"])
        self.assertEqual({r["outcome"] for r in rows if r["kind"] != "observation"}, {"ok"})
        self.assertEqual(verdict.gate_verdict(rows, [])["verdict"], "pass")

    def test_a_calibration_without_a_noise_of_the_cpu_is_written_and_still_judges(self):
        os.remove(self.data.path("gate-noise.json"))
        original = verdict.build_noise
        key = verdict.noise_key("erp", gate.SCENARIO, gate.TEMP, "graph_ready_cpu_s")

        def without_cpu_noise(*args, **kwargs):
            found = original(*args, **kwargs)
            found[key]["noise"] = None
            return found

        with patch.object(verdict, "build_noise", without_cpu_noise):
            code, line = self.calibrate()
        self.assertEqual((code, "written=true" in line), (0, True), line)
        self.assertIsNone(read_json(self.data.path("gate-noise.json"))["entries"][key]["noise"])
        self.cpu(2.0)
        code, summary, _ = self.gate(label="calibrated")
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["reason"], "no_noise")

    def test_a_calibration_without_a_noise_of_a_decisive_metric_is_not_written(self):
        os.remove(self.data.path("gate-noise.json"))
        original = verdict.build_noise
        key = verdict.noise_key("erp", gate.SCENARIO, gate.TEMP, "graph_resolve_s")

        def without_noise(*args, **kwargs):
            found = original(*args, **kwargs)
            found[key]["noise"] = None
            return found

        with patch.object(verdict, "build_noise", without_noise):
            code, line = self.calibrate()
        self.assertEqual((code, "written=false" in line), (2, True), line)
        self.assertFalse(os.path.exists(self.data.path("gate-noise.json")))


class TradeAgainstThePreviousRelease(CriteriaCase):
    """И2 against the previous release: the growth over the bar is repaid by an equal fall of the wall in the
    same comparison, and by nothing less."""

    def world(self, metric, wall):
        self.work(metric, 100.0, 108.0)
        self.wall(100.0, wall)

    def test_an_equal_wall_repays_the_growth_and_a_smaller_gain_leaves_the_block(self):
        for metric in TRADED:
            for wall, outcome, basis in ((92.0, "ok", "growth_traded"), (92.1, "block", "growth_not_repaid"),
                                         (100.0, "block", "growth_not_repaid")):
                with self.subTest(metric=metric, wall=wall):
                    self.world(metric, wall)
                    label = f"{metric}-{wall}"
                    code, summary, _ = self.gate(label=label)
                    row = self.row(summary, metric)
                    self.assertEqual((row["outcome"], row["basis"]), (outcome, basis))
                    self.assertAlmostEqual(row["delta"], 0.08)
                    self.assertAlmostEqual(row["wall_delta"], (wall - 100.0) / 100.0)
                    self.assertEqual((code, summary["verdict"]), (0, "pass") if outcome == "ok" else (1, "block"))
                    self.assertEqual(f"block: {metric}" in summary["reasons"], outcome == "block")
                    report = self.report_text(label)
                    [line] = [l for l in report.splitlines() if l.startswith(f"- {metric}: ")]
                    self.assertIn("принят как размен на ускорение" if outcome == "ok" else "блок: рост", line)
                    self.assertIn(f"Δ стены {wall - 100.0:+.1f}%, Δ {metric} +8.0%", line)

    def test_a_wall_over_its_limit_is_a_block_beside_the_growth_it_cannot_repay(self):
        self.write_noise(wall=0.01)
        self.work("graph_resolve_s", 100.0, 108.0)
        self.wall(100.0, 106.0)
        code, summary, _ = self.gate(label="slower")
        wall = self.row(summary, "graph_ready_s")
        self.assertEqual((wall["outcome"], wall["limit"]), ("block", BAR))
        resolve = self.row(summary, "graph_resolve_s")
        self.assertEqual((resolve["outcome"], resolve["basis"]), ("block", "growth_not_repaid"))
        self.assertAlmostEqual(resolve["wall_delta"], 0.06)
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: graph_ready_s", summary["reasons"])
        self.assertIn("block: graph_resolve_s", summary["reasons"])

    def test_a_wall_that_is_not_there_repays_nothing(self):
        # The wall of the comparison cannot be missing where its work metric is not: the runs are the same.
        # The rule is therefore shown on the rows, with a side that holds no wall at all.
        steady = {"graph_resolve_s": 17.8, "diagnostics_workspace_s": 22.1, "graph_ready_cpu_s": 40.0}
        base = [{"k": k, "runs": 3, "held": True, "reason": None, "calls": [],
                 "metrics": {"graph_ready_s": 100.0, **steady}} for k in (1, 2, 3)]
        cand = [{"k": k, "runs": 3, "held": True, "reason": None, "calls": [],
                 "metrics": {**steady, "graph_resolve_s": 17.8 * 1.08}} for k in (1, 2, 3)]
        noise = {"entries": {f"erp/mcp-graph/cold/{key}": {"noise": 0.01} for key in gate.METRICS}}
        rows = {r["metric"]: r for r in gate.rule_rows(base, cand, noise, False)}
        self.assertIsNone(rows["graph_ready_s"]["delta"])
        self.assertEqual((rows["graph_resolve_s"]["outcome"], rows["graph_resolve_s"]["basis"]),
                         ("block", "growth_not_repaid"))
        self.assertIsNone(rows["graph_resolve_s"]["wall_delta"])


class RulesThatStayInForce(CriteriaCase):
    """И7: with every new rule at work, each old check that is broken in turn still blocks or leaves
    the gate incomplete."""

    def setUp(self):
        super().setUp()
        # The world in which every new rule is at work and the gate passes: the CPU doubled, the growth of
        # resolve repaid by the wall, the memory at the limit of the bar of a target without a noise.
        self.memory_record(None)
        self.peak(4900000)
        self.script.values["cpu"] = lambda run: 80.0 if run.role == "c" else 40.0
        self.wall(100.0, 92.0)
        self.times(graph_resolve=(100.0, 108.0))
        self.args_extra = ()

    def times(self, **calls):
        """Seconds of calls by name: a number for both sides or (base, candidate)."""
        def elapsed(run, name):
            value = calls.get(name)
            if value is None:
                return None
            return value[1 if run.role == "c" else 0] if isinstance(value, tuple) else value

        self.script.values["elapsed"] = elapsed

    def judged(self, label):
        return self.gate(*self.args_extra, label=label)

    def test_the_world_of_the_new_rules_passes(self):
        code, summary, _ = self.judged("control")
        self.assertEqual((code, summary["verdict"], summary["reasons"]), (0, "pass", []))
        self.assertEqual(self.row(summary, "graph_resolve_s")["basis"], "growth_traded")
        self.assertEqual(self.row(summary, "graph_ready_cpu_s")["outcome"], "observation")
        self.assertEqual(self.record_row(summary, "hwm_kb")["limit_basis"], "bar_no_noise")

    def test_a_reference_over_its_ceiling_blocks(self):
        self.times(graph_resolve=(100.0, 108.0), references_1=70.0)
        code, summary, _ = self.judged("references")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: references_1_s", summary["reasons"])

    def test_answers_that_differ_block(self):
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=resolve_other")
        code, summary, _ = self.judged("answers")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("answers_differ", summary["reasons"])

    def test_a_candidate_that_does_not_hold_blocks(self):
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=cand_crash")
        code, summary, _ = self.judged("crash")
        self.assertEqual((code, summary["verdict"]), (1, "block"))

    def test_a_profile_that_does_not_compare_leaves_the_gate_incomplete(self):
        self.machine.print["kernel"] = "7.0"
        code, summary, _ = self.judged("profile")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertIn("record_profile_mismatch:kernel", summary["reasons"])

    def test_a_foreign_load_leaves_the_gate_incomplete(self):
        self.script.values["foreign"] = 8.0
        self.args_extra = ("--busy-budget-min", "0")
        code, summary, _ = self.judged("foreign")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual(summary["reasons"][0], "machine_busy")

    def test_a_decisive_metric_without_a_calibration_leaves_the_gate_incomplete(self):
        for metric in TRADED:
            with self.subTest(metric=metric):
                self.write_noise()
                self.break_gate_noise(metric)
                code, summary, _ = self.judged(metric)
                self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
                self.assertEqual(self.row(summary, metric)["outcome"], "no_noise")

    def test_the_noise_of_memory_that_is_missing_does_not_make_the_rest_of_the_record_complete(self):
        # The shipped record has no noise of memory of A against A either.
        self.memory_record(None, base_a_noise=None)
        code, summary, _ = self.judged("memory-a-a")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertIn("record_no_noise:hwm_A_kb", summary["reasons"])
        self.assertEqual(self.record_row(summary, "hwm_kb")["outcome"], "ok")
        # And a work metric of the record without its noise stays incomplete beside a memory without one.
        self.write_baseline()
        self.memory_record(None)
        self.set_record("graph_resolve_s", noise=None)
        code, summary, _ = self.judged("work-noise")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertIn("record_no_noise:graph_resolve_s", summary["reasons"])
        self.assertNotIn("record_no_noise:hwm_kb", summary["reasons"])


class WallOfTheIllustration(CriteriaCase):
    """И8: the data of the illustration of 0.4.4 — a wall of 142.4 s against 151.4 s at a noise of 4.5 % —
    block whatever the CPU and a feature do; a wall without growth does not."""

    def world(self, cand_wall):
        self.write_noise(wall=0.045)
        self.wall(142.4, cand_wall)

    def test_the_wall_of_the_illustration_blocks_whatever_the_cpu_does(self):
        for factor in (0.5, 1.0, 2.0):
            with self.subTest(cpu=factor):
                self.world(151.4)
                self.script.values["cpu"] = lambda run, f=factor: 40.0 * f if run.role == "c" else 40.0
                code, summary, _ = self.gate(label=f"cpu{factor}")
                wall = self.row(summary, "graph_ready_s")
                self.assertEqual((wall["outcome"], wall["kind"]), ("block", "wall"))
                self.assertAlmostEqual(wall["delta"], 9.0 / 142.4)
                self.assertAlmostEqual(wall["limit"], BAR)
                self.assertNotIn("confirm", wall)
                self.assertEqual((code, summary["verdict"]), (1, "block"), "the release is not declared allowed")
                self.assertIn("block: graph_ready_s", summary["reasons"])
                self.assertEqual(len(summary["runs"]), ROUND_ONE_RUNS)

    def test_the_wall_of_the_illustration_blocks_with_every_signal_of_a_feature(self):
        for name in dict(feature_signals(self)):
            with self.subTest(signal=name):
                self.reset_world()
                dict(feature_signals(self))[name]()
                self.world(151.4)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "block")
                self.assertEqual((code, summary["verdict"]), (1, "block"))

    def test_the_same_wall_without_growth_is_not_blocked_by_the_wall(self):
        self.world(142.4)
        code, summary, _ = self.gate(label="control")
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "ok")
        self.assertEqual((code, summary["verdict"]), (0, "pass"))


class FeaturesAgainstThePreviousRelease(CriteriaCase):
    """И10 against the previous release: a feature neither lifts a block of time nor makes one."""

    EXPECTED = {"a new code": ("## Новые коды", "| X | 1 | 1 |"),
                "a vanished code": ("## Исчезнувшие коды", "| X | 1 | 1 |"),
                "a code that grew": ("## Изменённые коды", "| BSL001 | 7/3/warning | 8/3/warning |"),
                "a code that shrank": ("## Изменённые коды", "| BSL001 | 8/3/warning | 7/3/warning |")}

    def test_a_time_over_the_bar_blocks_with_every_signal_of_a_feature(self):
        for metric in TRADED:
            for name in dict(feature_signals(self)):
                with self.subTest(metric=metric, signal=name):
                    self.reset_world()
                    dict(feature_signals(self))[name]()
                    self.work(metric, 100.0, 108.0)
                    label = f"{metric}-{name}".replace(" ", "-")
                    code, summary, _ = self.gate(label=label)
                    row = self.row(summary, metric)
                    self.assertEqual((row["outcome"], row["basis"]), ("block", "growth_not_repaid"))
                    self.assertEqual((code, summary["verdict"]), (1, "block"))
                    self.assertIn(f"block: {metric}", summary["reasons"])
                    self.assertEqual(summary["answers"]["outcome"], "ok", "the sweep decides nothing")
                    if name in self.EXPECTED:
                        title, line = self.EXPECTED[name]
                        report = self.report_text(label)
                        self.assertIn(line, self.section(report, title.removeprefix("## ")))
                        self.assertIn("Изменения функционала не расширяют временные пределы.", report)

    def test_every_decisive_check_passed_passes_with_every_signal_of_a_feature(self):
        for name in dict(feature_signals(self)):
            with self.subTest(signal=name):
                self.reset_world()
                dict(feature_signals(self))[name]()
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual((code, summary["verdict"], summary["reasons"]), (0, "pass", []))

    def test_a_memory_over_its_ceiling_blocks_with_every_signal_of_a_feature(self):
        for name in dict(feature_signals(self)):
            with self.subTest(signal=name):
                self.reset_world()
                dict(feature_signals(self))[name]()
                self.peak(8 << 20)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual((code, summary["verdict"]), (1, "block"))
                self.assertIn("block: hwm_kb", summary["reasons"])


class ReadmeOfTheCriteria(unittest.TestCase):
    """The document the owner reads tells the rules of the code."""

    def setUp(self):
        with open(README, encoding="utf-8") as fh:
            self.flat = " ".join(fh.read().split())

    def test_the_cpu_is_an_observation_and_the_trade_is_the_same_for_the_record(self):
        self.assertNotIn("и размена работы на стену здесь нет", self.flat)
        self.assertNotIn("`graph_ready_cpu_s`, `graph_resolve_s`, `diagnostics_workspace_s` | ν нет", self.flat)
        for part in ("`graph_ready_cpu_s` | наблюдение", "`graph_ready_cpu_s` — наблюдение и на вердикт рекорда не влияет",
                     "стена `graph_ready_s` против рекорда в том же раунде сократилась не меньше",
                     "`limit_basis: bar_no_noise`", "бар 5 %, шум отсутствует", "`record_no_noise:hwm_kb`",
                     "пределы времени и памяти не расширяют"):
            self.assertTrue(part in self.flat, f"scripts/README.md does not say: {part}")


if __name__ == "__main__":
    unittest.main()
