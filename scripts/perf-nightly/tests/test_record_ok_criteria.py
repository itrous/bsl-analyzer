"""The criteria of the gate against the record and in the night (plan
docs/plans/gate-ok-criteria.md, table of checks): the trade of the time of resolve and
of the sweep for the wall against the record, round by round (И2, И3), the memory
against its target without a measured noise (И4–И6), the CPU as an observation (И1),
the independence of a feature (И10) and the record file left as it is (И9). Every
check reads the verdict, the exit code and the report that the owner reads. The clock
is virtual and the machine scripted (`gate_fixtures.py`); nothing here waits for
time."""

import hashlib
import math
import os
import unittest

from gate_fixtures import BASE_TAG, CAND_LABEL
from ok_criteria_fixtures import PEAK_OVER_TARGET_KB, TARGET_KB, TRADED, CriteriaCase, feature_signals, limit_of
from record_fixtures import full_round
from test_record_publish import NEXT, RECORD_SHA, RecordPublish

import gate
import publish
import verdict

BAR = verdict.GATE_BAR
LEGACY_TEXT = "больше 5 % не окуплен"
ROUND_ONE_RUNS = 9


def drop(path):
    def edit(entry):
        node = entry
        for key in path[:-1]:
            node = node[key]
        node.pop(path[-1], None)
    return edit


class RecordTrade(CriteriaCase):
    """И2 against the record: the growth over the limit of the record is repaid by the wall
    against the record, in the same round, and by nothing else."""

    def world(self, metric, work, wall):
        """The record of `metric` is 100 s with ν = 1 %; the base and the candidate both take `work`
        and `wall`, so the previous release sees no change and only the record is in question."""
        self.set_record(metric, 100.0, 0.01)
        self.work(metric, work, work)
        self.wall(wall, wall)

    def test_an_equal_wall_repays_the_growth_and_a_smaller_gain_leaves_the_block(self):
        for metric in TRADED:
            for work, wall, outcome, basis in ((108.0, 92.0, "ok", "growth_traded"),
                                               (108.0, 92.1, "block", "growth_not_repaid"),
                                               (108.0, 100.0, "block", "growth_not_repaid"),
                                               (102.0, 100.0, "block", "growth_not_repaid")):
                with self.subTest(metric=metric, work=work, wall=wall):
                    self.world(metric, work, wall)
                    code, summary, _ = self.gate(label=f"{metric}-{work}-{wall}")
                    row = self.record_row(summary, metric)
                    self.assertEqual((row["outcome"], row["basis"]), (outcome, basis))
                    self.assertAlmostEqual(row["limit"], 101.0)
                    self.assertAlmostEqual(row["delta"], (work - 100.0) / 100.0)
                    self.assertAlmostEqual(row["wall_delta"], (wall - 100.0) / 100.0)
                    self.assertEqual(self.decided(summary), {"ok"}, "the previous release sees no change")
                    self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]),
                                     (0, "pass", "pass") if outcome == "ok" else (1, "block", "block"))
                    self.assertEqual("block: record:" + metric in summary["reasons"], outcome == "block")

    def test_a_wall_the_record_cannot_judge_repays_nothing(self):
        for metric in TRADED:
            with self.subTest(metric=metric):
                self.world(metric, 108.0, 70.0)
                self.set_record("graph_ready_s", noise=None)
                _, summary, _ = self.gate(label=metric)
                row = self.record_row(summary, metric)
                self.assertEqual((row["outcome"], row["basis"], row["wall_delta"]), ("block", "growth_not_repaid", None))
                self.assertIn("record_no_noise:graph_ready_s", summary["reasons"])
                self.assertEqual(summary["verdict"], "block")

    def test_the_report_names_the_limit_of_the_record_and_never_the_bar_of_the_release(self):
        for number, (work, wall, phrase) in enumerate(((108.0, 92.0, "принят как размен на ускорение"),
                                                       (102.0, 100.0, "не окуплен сокращением стены против рекорда"))):
            with self.subTest(work=work, wall=wall):
                self.world("graph_resolve_s", work, wall)
                _, summary, _ = self.gate(label=f"r{number}")
                report = self.report_text(f"r{number}")
                [line] = [l for l in report.splitlines() if l.startswith("- graph_resolve_s: ")]
                self.assertIn("выше предела рекорда 101.0", line)
                self.assertIn(phrase, line)
                self.assertNotIn("5 %", line, "the record has a limit of its own")
                self.assertNotIn(LEGACY_TEXT, report)
                self.assertIn(f"Δ стены {wall - 100.0:+.1f}%, Δ graph_resolve_s {work - 100.0:+.1f}%", line)
                row = self.record_cells(report, "graph_resolve_s")
                self.assertEqual((row["record"], row["candidate"], row["delta"], row["noise"], row["limit"], row["basis"]),
                                 ("100.0", f"{work:.1f}", f"{work - 100.0:+.1f}%", "1.00%", "101.0", "шум записи (ν)"))


class RoundsOfTheRecord(CriteriaCase):
    """И3: each round is judged on its own against its own wall; a wall above its limit is not repaid
    by a trade of another comparison."""

    def setUp(self):
        super().setUp()
        # An improvement of a reference asks for the second round without touching the metrics under test.
        self.set_record("references_1_s", 60.0, 0.01)
        self.set_record("graph_resolve_s", 100.0, 0.01)
        self.work("graph_resolve_s", 108.0, 108.0)

    def rounds(self, summary):
        row = self.record_row(summary, "graph_resolve_s")
        return [(c["outcome"], c.get("basis"), round(c["wall_delta"], 6)) for c in row["rounds"]]

    def test_a_trade_of_one_round_covers_nothing_in_another_round(self):
        traded, unpaid = ("ok", "growth_traded", -0.2), ("block", "growth_not_repaid", 0.0)
        for name, first, second, expected, rounds in (
                ("the trade is in the first round", 80.0, 100.0, "block", [traded, unpaid]),
                ("the trade is in the second round", 100.0, 80.0, "block", [unpaid, traded]),
                ("both rounds repay", 80.0, 80.0, "pass", [traded, traded])):
            with self.subTest(name):
                self.wall(lambda run, a=first, b=second: a if run.n <= 3 else b,
                          lambda run, a=first, b=second: a if run.n <= 3 else b)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual(len([r for r in summary["runs"] if r["series"] == "A" and r["role"] == "candidate"]), 6)
                self.assertEqual(self.rounds(summary), rounds)
                row = self.record_row(summary, "graph_resolve_s")
                self.assertEqual(row["outcome"], "ok" if expected == "pass" else "block")
                self.assertEqual((code, summary["verdict"]), (0, "pass") if expected == "pass" else (1, "block"))
                if expected == "block":
                    self.assertIn("block: record:graph_resolve_s", summary["reasons"])
                    self.assertEqual((row["basis"], row["wall_delta"]), ("growth_not_repaid", 0.0),
                                     "the row shows the round that holds the block, not a mean of the rounds")

    def test_a_wall_over_its_limit_stays_a_block_whatever_another_comparison_traded(self):
        self.write_noise(wall=0.01)
        # The candidate is 6 % slower than the base: over max(5 %, ν) = 5 % of the previous release;
        # against a record of 120 s it is 11.7 % faster and repays the growth of resolve there.
        self.set_record("graph_ready_s", 120.0)
        self.wall(100.0, 106.0)
        code, summary, _ = self.gate(label="release-wall")
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "block")
        resolve = self.record_row(summary, "graph_resolve_s")
        self.assertEqual((resolve["outcome"], resolve["basis"]), ("ok", "growth_traded"))
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (1, "block", "pass"))
        self.assertIn("block: graph_ready_s", summary["reasons"])

    def test_a_wall_over_the_limit_of_the_record_stays_a_block_whatever_the_release_traded(self):
        self.write_noise(wall=0.01)
        # Against the base of 120 s the candidate is 11.7 % faster and repays the growth of resolve there;
        # against a record of 100 s with ν = 1 % it is over the limit of 101 s.
        self.set_record("graph_ready_s", 100.0, 0.01)
        self.set_record("graph_resolve_s", 108.0)
        self.work("graph_resolve_s", 100.0, 108.0)
        self.wall(120.0, 106.0)
        code, summary, _ = self.gate(label="record-wall")
        resolve = self.row(summary, "graph_resolve_s")
        self.assertEqual((resolve["outcome"], resolve["basis"]), ("ok", "growth_traded"))
        self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "ok")
        wall = self.record_row(summary, "graph_ready_s")
        self.assertEqual(wall["outcome"], "block")
        self.assertAlmostEqual(wall["limit"], 101.0)
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: record:graph_ready_s", summary["reasons"])


class MemoryWithoutNoise(CriteriaCase):
    """И4: the memory of the owner's target has no measured noise; the bar of 5 % stands in for it
    and only for the comparison."""

    LIMIT = TARGET_KB * (1 + BAR)

    def run_peak(self, peak, label, noise=None, **extra):
        self.memory_record(noise, **extra)
        self.peak(peak)
        code, summary, _ = self.gate(label=label)
        return code, summary, self.record_row(summary, "hwm_kb")

    def test_the_target_with_the_bar_blocks_above_the_limit_and_not_at_it(self):
        self.assertAlmostEqual(self.LIMIT, 4900681.8)
        for number, (peak, outcome) in enumerate(((limit_of(TARGET_KB, BAR), "ok"), (4900681, "ok"),
                                                  (4900682, "block"), (TARGET_KB, "ok"), (4000000, "ok"))):
            with self.subTest(peak=peak):
                code, summary, row = self.run_peak(peak, f"p{number}")
                self.assertEqual(row["outcome"], outcome)
                self.assertAlmostEqual(row["limit"], 4900681.8, places=6)
                self.assertEqual(row["limit_basis"], "bar_no_noise")
                self.assertIsNone(row["noise"], "no tolerance is invented")
                self.assertEqual(row["record"]["evidence"]["kind"], "owner_target", "the target stays the target")
                self.assertEqual(self.ceilings(summary)["hwm_kb"], "ok")
                self.assertEqual((code, summary["verdict"]), (1, "block") if outcome == "block" else (0, "pass"))
                self.assertEqual("block: record:hwm_kb" in summary["reasons"], outcome == "block")

    def ceilings(self, summary):
        return {c["metric"]: c["outcome"] for c in summary["ceilings"]}

    def test_the_noise_may_be_left_out_in_either_way(self):
        for name, edit in (("null", None), ("no value", drop(["noise", "value"]))):
            with self.subTest(noise=name):
                self.memory_record(None)
                if edit is not None:
                    self.edit_baseline(lambda doc, edit=edit: edit(doc["metrics"]["hwm_kb"]))
                self.peak(4900682)
                _, summary, _ = self.gate(label=name.replace(" ", "-"))
                row = self.record_row(summary, "hwm_kb")
                self.assertEqual((row["outcome"], row["limit_basis"], row["noise"]), ("block", "bar_no_noise", None))

    def test_without_a_measured_noise_an_improvement_is_neither_confirmed_nor_proposed(self):
        _, summary, row = self.run_peak(4000000, "better")
        self.assertEqual([c["improved"] for c in row["rounds"]], [False])
        self.assertEqual(len(summary["runs"]), ROUND_ONE_RUNS)
        self.assertEqual(summary["record_confirmation"]["requested"], False)
        self.assertEqual(summary["record_proposals"], [])

    def test_a_measured_noise_keeps_the_limit_of_the_noise_and_the_confirmation(self):
        limit = TARGET_KB * 1.01
        for number, (peak, outcome) in enumerate(((4713989, "ok"), (4713990, "block"))):
            with self.subTest(peak=peak):
                _, summary, row = self.run_peak(peak, f"n{number}", noise=0.01)
                self.assertEqual((row["outcome"], row["limit_basis"], row["noise"]), (outcome, "noise", 0.01))
                self.assertAlmostEqual(row["limit"], limit, places=6)
        _, summary, row = self.run_peak(4000000, "improved", noise=0.01)
        self.assertEqual([c["improved"] for c in row["rounds"]], [True, True])
        self.assertEqual(summary["record_confirmation"]["requested"], True)
        self.assertEqual([p["metric"] for p in summary["record_proposals"]], ["hwm_kb"])

    def test_a_noise_that_is_given_and_wrong_is_an_incomplete_record_and_not_the_bar(self):
        for noise in (-0.1, "x", math.inf, True):
            with self.subTest(noise=noise):
                code, summary, row = self.run_peak(PEAK_OVER_TARGET_KB, f"w{noise}", noise=noise)
                self.assertEqual((row["outcome"], row["reason"], row["limit"]), ("incomplete", "record_no_noise:hwm_kb", None))
                self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
                self.assertNotIn("block: record:hwm_kb", summary["reasons"])


class MemoryAgainstTheTarget(CriteriaCase):
    """И5 and И6: the peak over the target blocks though the ceiling passes, in the release and in the night,
    and the report of the record tells the numbers."""

    def release(self, label):
        self.memory_record(None)
        self.peak(PEAK_OVER_TARGET_KB)
        return self.gate(label=label)

    def test_a_peak_over_the_target_blocks_the_release_and_the_control_does_not(self):
        code, summary, _ = self.release("over")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: record:hwm_kb", summary["reasons"])
        ceiling = self.row(summary, "hwm_kb")
        self.assertEqual((ceiling["outcome"], ceiling["limit"]), ("ok", verdict.GATE_HWM_LIMIT_KB))
        self.assertLess(PEAK_OVER_TARGET_KB, verdict.GATE_HWM_LIMIT_KB)
        self.assertEqual(self.record_row(summary, "hwm_kb")["outcome"], "block")
        self.peak(TARGET_KB)
        code, summary, _ = self.gate(label="control")
        self.assertEqual((code, summary["verdict"], self.record_row(summary, "hwm_kb")["outcome"]), (0, "pass", "ok"))

    def test_the_block_stands_beside_the_incompleteness_of_the_rest_of_the_record(self):
        # The shipped record has no noise of memory at all, not even of A against A.
        self.memory_record(None, base_a_noise=None)
        self.peak(PEAK_OVER_TARGET_KB)
        code, summary, _ = self.gate(label="shipped")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("record_no_noise:hwm_A_kb", summary["reasons"])
        self.assertIn("block: record:hwm_kb", summary["reasons"])

    def test_a_night_over_the_target_is_a_block_and_a_night_at_it_is_not(self):
        self.memory_record(None)
        self.peak(PEAK_OVER_TARGET_KB)
        result, saved = self.night()
        self.assertEqual((result["verdict"], result["record_verdict"]), ("block", "block"))
        self.assertIn("block: record:hwm_kb", result["reasons"])
        self.assertEqual(next(c for c in saved["ceilings"] if c["metric"] == "hwm_kb")["outcome"], "ok")
        self.peak(TARGET_KB)
        result, _ = self.night()
        self.assertEqual((result["verdict"], result["record_verdict"]), ("ok", "pass"))

    def test_the_report_tells_the_record_the_candidate_the_delta_the_limit_and_its_basis(self):
        self.release("report")
        row = self.record_cells(self.report_text("report"), "hwm_kb")
        self.assertEqual((row["record"], row["delta"], row["noise"], row["limit"], row["basis"], row["source"],
                          row["outcome"]),
                         ("4667316", "+48.2%", "-", "4900681.8", "бар 5 %, шум отсутствует", "owner_target", "block"))
        self.assertTrue(row["candidate"].startswith("6917216"), row["candidate"])
        ceilings = self.section(self.report_text("report"), "Потолки")
        self.assertIn("| hwm_kb |", ceilings, "the ceiling is a table of its own")
        self.assertNotIn("бар 5 %", ceilings)

    def test_the_night_report_tells_the_same(self):
        self.memory_record(None)
        self.peak(PEAK_OVER_TARGET_KB)
        self.night()
        row = self.record_cells(self.night_report(), "hwm_kb")
        self.assertEqual((row["record"], row["delta"], row["limit"], row["basis"], row["outcome"]),
                         ("4667316", "+48.2%", "4900681.8", "бар 5 %, шум отсутствует", "block"))

    def test_a_measured_noise_is_told_as_the_basis_of_the_limit(self):
        self.memory_record(0.01)
        self.peak(PEAK_OVER_TARGET_KB)
        self.gate(label="noise")
        row = self.record_cells(self.report_text("noise"), "hwm_kb")
        self.assertEqual((row["noise"], row["basis"], row["limit"]), ("1.00%", "шум записи (ν)", "4713989.2"))


class CpuAgainstTheRecord(CriteriaCase):
    """И1 in the record and in the night: the CPU is shown against the record and decides nothing."""

    def setUp(self):
        super().setUp()
        self.set_record("graph_ready_cpu_s", 40.0, 0.01)

    def cpu(self, factor):
        self.script.values["cpu"] = lambda run: 40.0 * factor if run.role == "c" else 40.0

    def seen(self, summary):
        return self.record_row(summary, "graph_ready_cpu_s")

    def assert_cpu_in_the_report(self, report, value):
        self.assertIn(f"| graph_ready_cpu_s | median_A | 40.000 | {value:.3f} |", report)
        self.assertIn("наблюдение; на вердикт не влияет", self.section(report, "Рекорд"))

    def test_a_cpu_twice_or_half_the_record_changes_neither_the_verdict_nor_the_rounds(self):
        for factor in (2.0, 0.5):
            with self.subTest(factor=factor):
                self.cpu(factor)
                code, summary, _ = self.gate(label=f"cpu{factor}")
                self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (0, "pass", "pass"))
                row = self.seen(summary)
                self.assertEqual((row["outcome"], row["limit"], row["candidate"]["value"]), ("observation", None, 40.0 * factor))
                self.assertAlmostEqual(row["delta"], factor - 1)
                self.assertEqual(len(summary["runs"]), ROUND_ONE_RUNS)
                self.assertEqual(summary["record_confirmation"]["requested"], False)
                self.assertEqual(summary["record_proposals"], [])
                self.assert_cpu_in_the_report(self.report_text(f"cpu{factor}"), 40.0 * factor)

    def test_what_is_missing_of_the_cpu_is_told_and_leaves_the_verdict_as_it_is(self):
        cases = {"no noise": (lambda doc: doc["metrics"]["graph_ready_cpu_s"]["noise"].update(value=None),
                              "record_no_noise:graph_ready_cpu_s"),
                 "no noise object": (lambda doc: doc["metrics"]["graph_ready_cpu_s"].pop("noise"),
                                     "record_no_noise:graph_ready_cpu_s"),
                 "no record": (lambda doc: doc["metrics"].pop("graph_ready_cpu_s"), "baseline_invalid:graph_ready_cpu_s")}
        for name, (edit, reason) in cases.items():
            with self.subTest(name):
                self.write_baseline()
                self.edit_baseline(edit)
                self.cpu(2.0)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                row = self.seen(summary)
                self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (0, "pass", "pass"))
                self.assertEqual((row["outcome"], row["reason"]), ("observation", reason))
                self.assertEqual(summary["reasons"], [])
                self.assertEqual(row["candidate"]["value"], 80.0, "the number of the candidate is still shown")

    def test_a_cpu_that_was_not_measured_is_told_in_its_row_and_judges_nothing(self):
        # The fake server always gives a number, so the round is made by hand: the runs hold no CPU at all.
        round_ = full_round()
        for run in round_["candidate"]:
            del run["metrics"]["graph_ready_cpu_s"]
        section = gate.record_section(gate.Baseline(self.baseline_path), self.fingerprint(), [round_],
                                      tuple(verdict.GATE_RECORD), False)
        rows = {r["metric"]: r for r in section["rows"]}
        cpu = rows["graph_ready_cpu_s"]
        self.assertEqual((cpu["outcome"], cpu["delta"], cpu["reason"]),
                         ("observation", None, "record_missing:graph_ready_cpu_s"))
        self.assertEqual({r["outcome"] for metric, r in rows.items() if metric != "graph_ready_cpu_s"}, {"ok"})
        self.assertEqual((section["verdict"], section["reasons"]), ("pass", []))
        self.assertEqual(gate.second_base_judgement(section, None), ([], []))

    def test_a_night_shows_the_cpu_and_is_not_changed_by_it(self):
        for factor in (2.0, 0.5):
            with self.subTest(factor=factor):
                self.cpu(factor)
                result, saved = self.night()
                self.assertEqual((result["verdict"], result["record_verdict"], result["reasons"]), ("ok", "pass", []))
                row = self.seen(saved)
                self.assertEqual((row["outcome"], row["limit"], row["candidate"]["value"]), ("observation", None, 40.0 * factor))
                self.assertNotIn("record_proposals", saved)
                self.assert_cpu_in_the_report(self.night_report(), 40.0 * factor)

    def test_a_night_whose_cpu_has_no_noise_or_no_record_is_the_same_night(self):
        for name, edit in (("no noise", lambda doc: doc["metrics"]["graph_ready_cpu_s"]["noise"].update(value=None)),
                           ("no record", lambda doc: doc["metrics"].pop("graph_ready_cpu_s"))):
            with self.subTest(name):
                self.write_baseline()
                self.edit_baseline(edit)
                self.cpu(2.0)
                result, saved = self.night()
                self.assertEqual((result["verdict"], result["record_verdict"], result["reasons"]), ("ok", "pass", []))
                self.assertEqual(self.seen(saved)["outcome"], "observation")


class FeaturesAgainstTheRecord(CriteriaCase):
    """И10 in the record and in the night: a feature neither lifts a block of time or of memory nor makes
    one; the signals of a feature are the sweep of the codes and the CHANGELOG."""

    def give(self, name):
        self.reset_world()
        dict(feature_signals(self))[name]()

    def test_a_time_over_the_record_blocks_with_every_signal_of_a_feature(self):
        for metric in TRADED:
            for name in dict(feature_signals(self)):
                with self.subTest(metric=metric, signal=name):
                    self.give(name)
                    self.set_record(metric, 100.0, 0.01)
                    self.work(metric, 108.0, 108.0)
                    label = f"{metric}-{name}".replace(" ", "-")
                    code, summary, _ = self.gate(label=label)
                    row = self.record_row(summary, metric)
                    self.assertEqual((row["outcome"], row["basis"]), ("block", "growth_not_repaid"))
                    self.assertEqual((code, summary["verdict"]), (1, "block"))
                    self.assertEqual(self.decided(summary), {"ok"})

    def test_the_same_signals_with_every_decisive_check_passed_pass(self):
        for name in dict(feature_signals(self)):
            with self.subTest(signal=name):
                self.give(name)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (0, "pass", "pass"))

    def test_a_peak_over_the_target_blocks_with_every_signal_of_a_feature(self):
        for name in dict(feature_signals(self)):
            with self.subTest(signal=name):
                self.give(name)
                self.memory_record(None)
                self.peak(PEAK_OVER_TARGET_KB)
                code, summary, _ = self.gate(label=name.replace(" ", "-"))
                self.assertEqual((code, summary["verdict"]), (1, "block"))
                self.assertIn("block: record:hwm_kb", summary["reasons"])

    def test_a_night_neither_asks_for_a_sweep_nor_reads_a_feature(self):
        for name in ("no signal", "a feature in the CHANGELOG", "a CHANGELOG without it"):
            with self.subTest(signal=name):
                self.give(name)
                self.set_record("graph_resolve_s", 100.0, 0.01)
                self.work("graph_resolve_s", 108.0, 108.0)
                result, saved = self.night()
                self.assertEqual((result["verdict"], result["record_verdict"]), ("block", "block"))
                self.assertIn("block: record:graph_resolve_s", result["reasons"])
                self.assertNotIn("answers", saved)
        self.assertEqual(self.builds, [], "no base is built for a night")
        self.assertEqual({launch["env"]["FAKE_LABEL"] for launch in self.launches()}, {CAND_LABEL})


class IssueOfTheNight(RecordPublish, CriteriaCase):
    """И6 and И2 in the issue that the night opens: the owner reads the basis of the limit and the trade there."""

    def memory_issue(self):
        return next(i for i in self.issues() if "hwm_kb" in i["title"])

    def test_a_memory_over_its_target_tells_the_bar_and_claims_no_noise(self):
        self.memory_record(None)
        self.peak(PEAK_OVER_TARGET_KB)
        self.publish(self.summary())
        body = self.memory_issue()["body"]
        self.assertIn("Рекорд R = 4667316; кандидат C = 6917216", body)
        self.assertIn("Δ = +48.21%; предел по бару 5 %, шум отсутствует: 4900681.8; абсолютный потолок: 7340032", body)
        self.assertNotIn("ν = ", body, "no noise is invented for the memory")
        self.assertNotIn("R·(1+ν)", body)
        self.assertIn(publish.OWNER_TARGET_TEXT, body)

    def test_a_memory_with_a_noise_tells_the_noise_as_before(self):
        self.memory_record(0.01)
        self.peak(PEAK_OVER_TARGET_KB)
        self.publish(self.summary())
        self.assertIn("ν = 1.00%; предел R·(1+ν) = 4713989.16", self.memory_issue()["body"])

    def test_a_growth_the_wall_did_not_repay_says_so_and_a_growth_it_repaid_opens_nothing(self):
        self.publish(self.summary())
        [created] = self.issues()
        self.assertIn("Размен не выполнен: Δ стены против рекорда +0.00% в том же раунде не окупает Δ метрики.",
                      created["body"])
        self.write_baseline()
        self.set_record("graph_resolve_s", 15.0, 0.01, sha=RECORD_SHA)
        self.wall(100.0, 70.0)
        calls = len(self.gh.calls())
        entries = self.publish(self.summary(date=NEXT), date=NEXT)
        self.assertEqual(entries, [])
        self.assertEqual(len(self.gh.calls()), calls, "a night with the growth repaid says nothing to GitHub")

    def test_a_wall_the_record_cannot_judge_is_told_as_missing(self):
        self.set_record("graph_ready_s", noise=None)
        self.publish(self.summary())
        resolve = next(i for i in self.issues() if "graph_resolve_s" in i["title"])
        self.assertIn("Размен не выполнен: Δ стены против рекорда недоступна в том же раунде", resolve["body"])


class RecordFileStaysAsItIs(CriteriaCase):
    """И9: no evaluation moves the record and none measures the corpus of the owner."""

    SHIPPED = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "baseline.json")

    @staticmethod
    def digest(path):
        with open(path, "rb") as fh:
            return hashlib.sha256(fh.read()).hexdigest()

    def test_a_confirmed_improvement_is_proposed_and_the_file_is_not_written(self):
        shipped = self.digest(self.SHIPPED)
        self.set_record("graph_resolve_s", 100.0, 0.01)
        self.work("graph_resolve_s", 98.0, 98.0)
        self.set_record("hwm_kb", 100, 0.01, base_a_noise=1000.0)
        self.peak(90)
        before = self.baseline_bytes()
        code, summary, _ = self.gate(label="improved")
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertEqual(sorted(p["metric"] for p in summary["record_proposals"]), ["graph_resolve_s", "hwm_kb"])
        self.assertEqual(self.baseline_bytes(), before, "the gate proposes and does not write")
        self.night()
        self.assertEqual(self.baseline_bytes(), before, "the night does not write either")
        self.assertEqual(self.digest(self.SHIPPED), shipped, "the file of the repository was not the record in use")
        self.assertNotEqual(os.path.abspath(summary["record"]["baseline_path"]), os.path.abspath(self.SHIPPED))

    def test_only_the_fake_binaries_on_the_synthetic_corpus_were_launched(self):
        self.gate(label="synthetic")
        self.night()
        launches = self.launches()
        self.assertTrue(launches)
        self.assertEqual({launch["env"]["FAKE_LABEL"] for launch in launches}, {CAND_LABEL, BASE_TAG})
        self.assertTrue(all(launch["cwd"].startswith(self.tmp) for launch in launches))


if __name__ == "__main__":
    unittest.main()
