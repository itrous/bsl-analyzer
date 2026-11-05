"""The record as the second base of the release gate (plan
docs/plans/perf-ratchet-record.md, И01–И05): its eight rows with their
provenance, the rule `C > R·(1+ν)`, what makes a record incomparable, the
aggregates it is judged by and the priority of the verdict. The clock is
virtual and the machine scripted (`gate_fixtures.py`); nothing here waits for
time."""

import math
import os
import tempfile
import unittest
from unittest.mock import patch

from gate_fixtures import BASE_TAG, CAND_LABEL, timed
from helpers import set_mode
from record_fixtures import MEASURED, RecordCase, full_round

import gate
import publish
import verdict
from common import read_json, sha256_file

EVIDENCE_FIELDS = ("kind", "summary", "role", "series", "runs", "confirmation")
PROFILE_KEYS = {"cpu_model", "nproc", "mem_total", "kernel", "rustc", "source_commit", "config_sha256",
                "harness_schema", "gate_schema"}
PROTOCOL_KEYS = {"corpus", "scenario", "runs", "symbols", "module", "extra_resolve", "extra_top",
                 "sweep_max_files", "pauses_s", "call_timeout_s", "temp", "build"}


def candidate_slower(factor):
    """Script value: the candidate takes `factor` times the base (100) in series A."""
    return lambda run: 100.0 * factor if run.role == "c" and run.series == "A" else 100.0


def changed(value):
    """A value other than `value` of the same kind."""
    if isinstance(value, bool):
        return not value
    if isinstance(value, (int, float)):
        return value + 1
    if isinstance(value, list):
        return [*value, "extra"]
    return f"{value}-other"


class Judged(RecordCase):
    """Judging by hand: the record file as it now is against rounds made in the test."""

    def section(self, rounds=None, print_="machine", keys=None, probe=False, path=None) -> dict:
        baseline = gate.Baseline(path or self.baseline_path)
        return gate.record_section(baseline, self.fingerprint() if print_ == "machine" else print_,
                                   [full_round()] if rounds is None else rounds, keys or tuple(verdict.GATE_RECORD),
                                   probe)

    def rows(self, section: dict) -> dict:
        return {r["metric"]: r for r in section["rows"]}

    def assert_not_compared(self, section: dict, reason: str, metrics=None) -> None:
        """Every row of `metrics` (default: all that judge) is incomplete for `reason`, with no number set
        against another; an observed row tells the same reason and judges nothing."""
        rows = self.rows(section)
        for metric in metrics or [m for m in rows if m not in verdict.GATE_OBSERVED]:
            row = rows[metric]
            self.assertEqual((row["outcome"], row["reason"], row["delta"], row["candidate"]["value"]),
                             ("incomplete", reason, None, None), metric)
        if metrics is None:
            for metric in verdict.GATE_OBSERVED:
                self.assertEqual((rows[metric]["outcome"], rows[metric]["delta"]), ("observation", None), metric)
                self.assertIn(reason, rows[metric]["reason"], metric)
        self.assertIn(reason, section["reasons"])
        self.assertEqual(section["verdict"], "incomplete")


class RecordRows(RecordCase):
    """И01."""

    def test_a_full_baseline_gives_a_row_per_metric_with_its_provenance(self):
        code, summary, _ = self.gate()
        record, doc = summary["record"], self.baseline_doc()
        self.assertEqual([r["metric"] for r in record["rows"]], list(verdict.GATE_RECORD))
        self.assertEqual(len(record["rows"]), 8)
        for row in record["rows"]:
            metric = row["metric"]
            unit, aggregate = verdict.GATE_RECORD[metric]
            entry = doc["metrics"][metric]
            self.assertEqual((row["kind"], row["unit"], row["aggregate"]), ("record", unit, aggregate), metric)
            self.assertEqual(row["record"], {"value": entry["value"], "sha": entry["record_sha"],
                                             "measured_at": entry["measured_at"], "evidence": entry["evidence"]})
            self.assertEqual(row["candidate"], {"state": "valid", "value": MEASURED[metric]}, metric)
            observed = metric in verdict.GATE_OBSERVED
            self.assertEqual((row["delta"], row["noise"], row["outcome"]),
                             (0.0, entry["noise"]["value"], "observation" if observed else "ok"))
            if observed:
                self.assertIsNone(row["limit"])
            else:
                self.assertAlmostEqual(row["limit"], entry["value"] * (1 + entry["noise"]["value"]))
        self.assertEqual((code, summary["verdict"], record["verdict"]), (0, "pass", "pass"))
        self.assertEqual(record["baseline_path"], self.baseline_path)
        self.assertEqual(record["baseline_sha256"], sha256_file(self.baseline_path))
        self.assertEqual(record["entries"], doc["metrics"])
        self.assertEqual(record["profile"], self.fingerprint())
        for key in ("memory_base", "record_proposals", "record_confirmation"):
            self.assertIn(key, summary)

    def test_a_record_that_changes_after_the_start_does_not_change_the_gate(self):
        # The file is read once per run: the summary names the bytes it judged by.
        digest = sha256_file(self.baseline_path)
        original = gate.Baseline

        def read_then_rewrite(path):
            found = original(path)
            self.set_record("graph_resolve_s", 1.0, 0.0)
            return found

        with patch.object(gate, "Baseline", read_then_rewrite):
            _, summary, _ = self.gate()
        self.assertEqual(summary["record"]["baseline_sha256"], digest)
        self.assertEqual(self.record_row(summary, "graph_resolve_s")["record"]["value"], MEASURED["graph_resolve_s"])
        self.assertEqual(summary["record"]["verdict"], "pass")

    def test_a_summary_without_a_record_reads_as_not_measured_and_not_as_passed(self):
        _, summary, _ = self.gate()
        legacy = {k: v for k, v in summary.items()
                  if k not in ("record", "memory_base", "record_proposals", "record_confirmation")}
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as out:
            gate.write_report(out, legacy)
            with open(os.path.join(out, "report.md"), encoding="utf-8") as fh:
                report = fh.read()
        self.assertIn("- вердикт: **pass**", report, "the old verdict is still shown as it was")
        section = report.split("## Рекорд", 1)[1].split("\n## ", 1)[0]
        self.assertIn("ратчет не измерялся", section)
        self.assertNotIn("итог:", section)
        self.assertNotIn("pass", section)
        proposals = report.split("## Предложения рекорда", 1)[1].split("\n## ", 1)[0]
        self.assertIn("ратчет не измерялся", proposals)
        self.assertEqual(publish.ratchet_groups(legacy), [])

    def test_a_baseline_without_one_metric_is_not_seven_rows_passed_as_a_full_result(self):
        self.edit_baseline(lambda doc: doc["metrics"].pop("references_2_same_s"))
        code, summary, _ = self.gate()
        rows = summary["record"]["rows"]
        self.assertEqual(len(rows), 8)
        missing = self.record_row(summary, "references_2_same_s")
        self.assertEqual((missing["outcome"], missing["reason"], missing["delta"]),
                         ("incomplete", "baseline_invalid:references_2_same_s", None))
        seven = [r for r in rows if r is not missing]
        self.assertEqual({r["outcome"] for r in seven if r["metric"] not in verdict.GATE_OBSERVED}, {"ok"})
        self.assertTrue(all(r["delta"] is not None for r in seven), "the other seven are compared")
        self.assertEqual(summary["record"]["verdict"], "incomplete")
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertIn("baseline_invalid:references_2_same_s", summary["reasons"])

    def test_a_metric_beyond_the_eight_makes_the_whole_record_invalid(self):
        self.edit_baseline(lambda doc: doc["metrics"].update(
            hwm_A_kb=dict(doc["metrics"]["hwm_kb"])))
        code, summary, _ = self.gate()
        for row in summary["record"]["rows"]:
            outcome = "observation" if row["metric"] in verdict.GATE_OBSERVED else "incomplete"
            self.assertEqual((row["outcome"], row["reason"], row["delta"]),
                             (outcome, "baseline_invalid:hwm_A_kb", None), row["metric"])
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))


class ShippedBaseline(unittest.TestCase):
    """И01: the file that ships with the harness."""

    def setUp(self):
        self.path = gate.BASELINE_PATH
        self.doc = read_json(self.path)

    def test_it_holds_exactly_the_eight_metrics_with_their_provenance(self):
        self.assertEqual(self.doc["schema"], gate.BASELINE_SCHEMA)
        self.assertEqual(list(self.doc["metrics"]), list(verdict.GATE_RECORD))
        for metric, entry in self.doc["metrics"].items():
            unit, aggregate = verdict.GATE_RECORD[metric]
            self.assertEqual((entry["unit"], entry["aggregate"]), (unit, aggregate), metric)
            self.assertTrue(entry["value"] > 0 and math.isfinite(entry["value"]), metric)
            self.assertRegex(entry["record_sha"], r"^[0-9a-f]{40}$")
            self.assertTrue(entry["measured_at"])
            evidence = entry["evidence"]
            self.assertIn(evidence["kind"], ("historical_import", "owner_target", "confirmed"), metric)
            self.assertEqual(sorted(evidence), sorted([*EVIDENCE_FIELDS, *(["note"] if "note" in evidence else [])]))
            self.assertRegex(evidence["summary"], r"^data/gates/[^/]+/summary\.json$", "a path from the job, not from git")
            if evidence["kind"] != "confirmed":
                self.assertIsNone(evidence["confirmation"], "an import is not a second round")

    def test_the_memory_target_is_the_owners_and_is_not_dressed_up_as_a_measurement(self):
        entry = self.doc["metrics"]["hwm_kb"]
        if entry["evidence"]["kind"] != "owner_target":
            self.skipTest("the target was replaced by a confirmed record")
        self.assertEqual(entry["value"], 4667316)
        self.assertEqual((entry["aggregate"], entry["evidence"]["series"], entry["evidence"]["role"]),
                         ("max_AB", "A", "base"))
        self.assertTrue(entry["evidence"]["note"], "the difference between the target and the measured A is written down")
        self.assertIn("base_A_noise", entry["noise"], "A against A has its own tolerance")

    def test_the_historical_values_are_the_extracted_ones(self):
        expected = {"graph_ready_s": 145.43540564802242, "graph_ready_cpu_s": 1245.29,
                    "graph_resolve_s": 17.442752545000985, "diagnostics_workspace_s": 21.9344880010176,
                    "hwm_kb": 4667316, "references_1_s": 56.56816865399014,
                    "references_2_same_s": 24.998699392017443, "references_3_other_s": 49.895538646000205}
        for metric, entry in self.doc["metrics"].items():
            if entry["evidence"]["kind"] != "confirmed":
                self.assertEqual(entry["value"], expected[metric], metric)

    def test_a_missing_tolerance_is_named_and_never_replaced_by_a_number(self):
        baseline = gate.Baseline(self.path)
        self.assertEqual(baseline.reasons, [])
        for metric, entry in self.doc["metrics"].items():
            value = entry["noise"]["value"]
            if metric == "hwm_kb":
                self.assertIsNone(value, "the bar stands in for it and is not written down as one")
                self.assertIsNone(baseline.problem(metric))
            elif value is None:
                self.assertEqual(baseline.problem(metric), f"record_no_noise:{metric}")
            else:
                self.assertIsNone(baseline.problem(metric))
                self.assertGreaterEqual(value, 0.0)

    def test_it_describes_the_profile_and_the_protocol_the_harness_measures(self):
        self.assertEqual(set(self.doc["profile"]), PROFILE_KEYS)
        self.assertEqual(self.doc["protocol"], gate.measured_protocol())
        self.assertEqual(gate.Baseline(self.path).incomparable(None), [])


class RecordRule(RecordCase):
    """И02."""

    def work(self, record, candidate, base=None, noise=0.01):
        self.set_record("graph_resolve_s", record, noise)
        self.script.values["elapsed"] = timed("graph_resolve", lambda run: candidate if run.role == "c" or base is None
                                              else base)

    def test_the_record_blocks_above_its_limit_and_at_no_other_value(self):
        for number, (candidate, outcome) in enumerate(((100.0, "ok"), (99.5, "ok"), (100.5, "ok"), (101.0, "ok"),
                                                       (101.001, "block"), (130.0, "block"))):
            with self.subTest(candidate=candidate):
                self.work(100.0, candidate)
                code, summary, _ = self.gate(label=f"c{number}")
                row = self.record_row(summary, "graph_resolve_s")
                self.assertEqual(row["outcome"], outcome)
                self.assertAlmostEqual(row["limit"], 101.0)
                self.assertAlmostEqual(row["delta"], (candidate - 100.0) / 100.0)
                self.assertEqual(self.decided(summary), {"ok"}, "the old rule sees no change")
                self.assertEqual((code, summary["verdict"]), (1, "block") if outcome == "block" else (0, "pass"))
                self.assertEqual(summary["record"]["verdict"], "block" if outcome == "block" else "pass")

    def test_a_loss_the_old_bar_forgives_is_a_loss_of_the_record(self):
        self.work(100.0, 108.16, base=104.0)
        before = self.baseline_bytes()
        code, summary, _ = self.gate()
        self.assertEqual(self.decided(summary), {"ok"}, "the previous release: pass")
        self.assertAlmostEqual(self.row(summary, "graph_resolve_s")["delta"], 0.04)
        row = self.record_row(summary, "graph_resolve_s")
        self.assertEqual((row["outcome"], row["record"]["value"]), ("block", 100.0))
        self.assertAlmostEqual(row["delta"], 0.0816)
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (1, "block", "block"))
        self.assertIn("block: record:graph_resolve_s", summary["reasons"])
        self.assertEqual(self.baseline_bytes(), before, "the gate does not move the record")

    def test_a_shorter_wait_buys_work_over_the_record_as_it_does_over_the_release(self):
        for number, (candidate, traded) in enumerate(((110.0, True), (102.0, False))):
            with self.subTest(candidate_work=candidate):
                self.work(100.0, candidate, base=100.0)
                self.script.values["wall"] = candidate_slower(0.5)
                code, summary, _ = self.gate(label=f"w{number}")
                old = self.row(summary, "graph_resolve_s")
                self.assertEqual(old["outcome"], "ok")
                self.assertEqual(old.get("basis"), "growth_traded" if traded else None)
                self.assertEqual(self.row(summary, "graph_ready_s")["outcome"], "ok")
                record = self.record_row(summary, "graph_resolve_s")
                self.assertEqual((record["outcome"], record["basis"]), ("ok", "growth_traded"),
                                 "over its own limit, which has no 5 % on top of ν")
                self.assertAlmostEqual(record["delta"], (candidate - 100.0) / 100.0)
                self.assertAlmostEqual(record["wall_delta"], -0.5)
                self.assertEqual((code, summary["verdict"]), (0, "pass"))

    def test_a_series_of_small_losses_does_not_lift_the_record(self):
        self.set_record("graph_resolve_s", 100.0, 0.01)
        before = self.baseline_bytes()
        previous = 100.0
        for number, current in enumerate((103.0, 106.0, 109.0)):
            self.script.values["elapsed"] = timed("graph_resolve", lambda run, p=previous, c=current: c if run.role == "c" else p)
            code, summary, _ = self.gate(label=f"s{number}")
            self.assertEqual(self.decided(summary), {"ok"}, "every release passes the one before")
            row = self.record_row(summary, "graph_resolve_s")
            self.assertEqual((row["outcome"], row["record"]["value"]), ("block", 100.0))
            self.assertEqual(code, 1)
            self.assertEqual(self.baseline_bytes(), before)
            previous = current

    def test_a_round_cannot_erase_the_block_of_another_round_and_can_add_one(self):
        # A second round runs where a metric improved; the record row is the worst of the rounds.
        self.set_record("diagnostics_workspace_s", 100.0, 0.01)
        self.set_record("graph_resolve_s", 20.0, 0.01)      # 17.8 is an improvement: the trigger
        self.script.values["elapsed"] = timed("diagnostics_workspace", lambda run: 105.0 if run.n <= 3 else 98.0)
        code, summary, _ = self.gate(label="first")
        row = self.record_row(summary, "diagnostics_workspace_s")
        self.assertEqual([c["outcome"] for c in row["rounds"]], ["block", "ok"])
        self.assertEqual((row["outcome"], round(row["delta"], 6)), ("block", 0.05))
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.script.values["elapsed"] = timed("diagnostics_workspace", lambda run: 98.0 if run.n <= 3 else 105.0)
        code, summary, _ = self.gate(label="second")
        row = self.record_row(summary, "diagnostics_workspace_s")
        self.assertEqual([c["outcome"] for c in row["rounds"]], ["ok", "block"])
        self.assertEqual((row["outcome"], round(row["delta"], 6)), ("block", 0.05))
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual([p["metric"] for p in summary["record_proposals"]], ["graph_resolve_s"],
                         "the block of one metric neither proposes it nor keeps another from being confirmed")

    def test_the_peak_of_the_second_round_counts_for_the_ceiling(self):
        self.set_record("graph_resolve_s", 100.0, 0.01)
        self.script.values["elapsed"] = timed("graph_resolve", 98.0)   # improved in round one: a second round
        self.script.values["hwm_kb"] = lambda run: 8 << 20 if (run.series, run.n) == ("B", 5) else 4 << 20
        code, summary, _ = self.gate()
        self.assertEqual([r["run"] for r in summary["runs"]][-3:], ["B-c4", "B-c5", "B-c6"])
        ceiling = self.row(summary, "hwm_kb")
        self.assertEqual((ceiling["kind"], ceiling["outcome"], ceiling["candidate"]["value"]),
                         ("ceiling", "block", 8 << 20))
        self.assertEqual((code, summary["verdict"]), (1, "block"))


class Incomparable(Judged):
    """И03."""

    def test_a_tolerance_that_is_not_a_finite_number_from_zero_up_leaves_no_comparison(self):
        def drop(entry):
            entry.pop("noise")

        def drop_value(entry):
            entry["noise"].pop("value")

        def put(value):
            def edit(entry):
                entry["noise"]["value"] = value
            return edit

        cases = {"absent": drop, "no value": drop_value, "null": put(None), "NaN": put(math.nan),
                 "infinite": put(math.inf), "negative": put(-0.01), "text": put("0.01"), "boolean": put(True)}
        for name, edit in cases.items():
            with self.subTest(noise=name):
                self.write_baseline()
                self.edit_baseline(lambda doc: edit(doc["metrics"]["graph_resolve_s"]))
                section = self.section()
                self.assert_not_compared(section, "record_no_noise:graph_resolve_s", ["graph_resolve_s"])
                others = [r for r in section["rows"] if r["metric"] != "graph_resolve_s"]
                self.assertEqual({r["outcome"] for r in others if r["metric"] not in verdict.GATE_OBSERVED}, {"ok"})
                self.assertTrue(all(r["delta"] == 0.0 for r in others), "an independent metric is still compared")

    def test_a_zero_tolerance_is_a_tolerance(self):
        self.set_record("graph_resolve_s", noise=0.0)
        self.assertEqual(self.rows(self.section())["graph_resolve_s"]["outcome"], "ok")
        worse = full_round(graph_resolve_s=17.8 + 0.001)
        row = self.rows(self.section([worse]))["graph_resolve_s"]
        self.assertEqual((row["outcome"], row["noise"]), ("block", 0.0))

    def test_a_value_that_is_not_a_positive_finite_number_is_invalid(self):
        for name, value in {"zero": 0, "negative": -5.0, "NaN": math.nan, "infinite": math.inf, "text": "56.5",
                            "null": None, "boolean": True}.items():
            with self.subTest(value=name):
                self.write_baseline()
                self.edit_baseline(lambda doc: doc["metrics"]["references_1_s"].update(value=value))
                self.assert_not_compared(self.section(), "baseline_invalid:references_1_s", ["references_1_s"])
        self.write_baseline()
        self.edit_baseline(lambda doc: doc["metrics"]["references_1_s"].pop("record_sha"))
        self.assert_not_compared(self.section(), "baseline_invalid:references_1_s", ["references_1_s"])
        self.write_baseline()
        self.edit_baseline(lambda doc: doc["metrics"].update(references_1_s=None))
        self.assert_not_compared(self.section(), "baseline_invalid:references_1_s", ["references_1_s"])

    def test_a_file_the_harness_cannot_use_leaves_every_row_incomplete(self):
        def write_text(text):
            def edit():
                with open(self.baseline_path, "w", encoding="utf-8") as fh:
                    fh.write(text)
            return edit

        def mutate(edit):
            return lambda: self.edit_baseline(edit)

        cases = {
            "missing file": (lambda: os.remove(self.baseline_path), "baseline_missing"),
            "not JSON": (write_text("{not json"), "baseline_invalid:not_json"),
            "a list": (write_text("[]"), "baseline_invalid:schema"),
            "unknown schema": (mutate(lambda doc: doc.update(schema=2)), "baseline_invalid:schema"),
            "text schema": (mutate(lambda doc: doc.update(schema="1")), "baseline_invalid:schema"),
            "no schema": (mutate(lambda doc: doc.pop("schema")), "baseline_invalid:schema"),
            "empty profile": (mutate(lambda doc: doc.update(profile={})), "baseline_invalid:profile"),
            "no protocol": (mutate(lambda doc: doc.pop("protocol")), "baseline_invalid:protocol"),
            "no metrics": (mutate(lambda doc: doc.update(metrics={})), "baseline_invalid:metrics"),
        }
        for name, (break_it, reason) in cases.items():
            with self.subTest(file=name):
                self.write_baseline()
                break_it()
                self.assert_not_compared(self.section(), reason)

    def test_any_field_of_the_profile_that_differs_names_itself(self):
        self.assertEqual(set(self.baseline_doc()["profile"]), PROFILE_KEYS)
        self.assertEqual(set(self.fingerprint()), PROFILE_KEYS)
        for key in sorted(PROFILE_KEYS):
            with self.subTest(field=key):
                self.write_baseline()
                self.edit_baseline(lambda doc: doc["profile"].update({key: changed(doc["profile"][key])}))
                self.assert_not_compared(self.section(), f"record_profile_mismatch:{key}")
        self.write_baseline()
        self.edit_baseline(lambda doc: doc["profile"].update(nproc=64, kernel="other"))
        self.assert_not_compared(self.section(), "record_profile_mismatch:kernel,nproc")

    def test_any_field_of_the_protocol_that_differs_names_itself(self):
        self.assertEqual(set(self.baseline_doc()["protocol"]), PROTOCOL_KEYS)
        for key in sorted(PROTOCOL_KEYS):
            with self.subTest(field=key):
                self.write_baseline()
                self.edit_baseline(lambda doc: doc["protocol"].update({key: changed(doc["protocol"][key])}))
                self.assert_not_compared(self.section(), f"record_protocol_mismatch:{key}")
        self.write_baseline()
        self.edit_baseline(lambda doc: (doc["profile"].update(rustc="rustc other"), doc["protocol"].update(runs=5)))
        section = self.section()
        self.assertIn("record_profile_mismatch:rustc", section["reasons"])
        self.assertIn("record_protocol_mismatch:runs", section["reasons"])
        self.assertEqual(section["verdict"], "incomplete")

    def test_the_same_full_profile_is_compared(self):
        section = self.section()
        self.assertEqual((section["verdict"], section["reasons"]), ("pass", []))
        self.assertTrue(all(r["delta"] == 0.0 for r in section["rows"]))

    def test_only_the_source_commit_differing_makes_a_whole_gate_incomplete(self):
        code, summary, _ = self.gate(label="same")
        self.assertEqual((code, summary["record"]["verdict"]), (0, "pass"))
        self.assertTrue(all(r["delta"] is not None for r in summary["record"]["rows"]))
        self.edit_baseline(lambda doc: doc["profile"].update(source_commit="0" * 40))
        code, summary, _ = self.gate(label="commit")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (2, "incomplete", "incomplete"))
        self.assertIn("record_profile_mismatch:source_commit", summary["reasons"])
        self.assertTrue(all(r["delta"] is None and r["reason"] == "record_profile_mismatch:source_commit"
                            for r in summary["record"]["rows"]))
        self.assertEqual(self.decided(summary), {"ok"}, "the old rule is judged all the same")

    def test_a_kernel_changed_on_the_machine_compares_nothing_and_cannot_be_cured_by_raising_the_record(self):
        before = self.baseline_bytes()
        self.machine.print["kernel"] = "7.0"
        self.script.values["elapsed"] = timed("graph_resolve", lambda run: 40.0 if run.role == "c" else 20.0)   # a loss the old rule blocks
        code, summary, _ = self.gate()
        self.assertEqual(summary["noise"]["status"], "current", "the kernel is not what stales the calibration")
        self.assertEqual(summary["record"]["profile"]["kernel"], "7.0")
        self.assertEqual(summary["record"]["baseline_profile"]["kernel"], "6.1")
        self.assertTrue(all(r["delta"] is None and r["reason"] == "record_profile_mismatch:kernel"
                            for r in summary["record"]["rows"]))
        self.assertEqual(self.row(summary, "graph_resolve_s")["outcome"], "block")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (1, "block", "incomplete"))
        self.assertIn("record_profile_mismatch:kernel", summary["reasons"])
        self.assertEqual(self.baseline_bytes(), before)

    def test_a_block_of_another_part_stays_beside_a_missing_tolerance(self):
        self.set_record("graph_resolve_s", noise=None)
        self.script.values["hwm_kb"] = lambda run: 8 << 20 if run.series == "B" else 4 << 20   # over the ceiling
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: hwm_kb", summary["reasons"])
        self.assertIn("record_no_noise:graph_resolve_s", summary["reasons"], "the incomplete is not hidden")
        self.assertEqual(summary["record"]["verdict"], "incomplete")

    def test_a_baseline_that_is_missing_is_incomplete_and_the_old_rule_still_judges(self):
        os.remove(self.baseline_path)
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual(summary["reasons"].count("baseline_missing"), 1)
        self.assertEqual(self.decided(summary), {"ok"})
        self.assertEqual(summary["record"]["baseline_sha256"], None)
        self.assertTrue(all(r["delta"] is None for r in summary["record"]["rows"]))


class Aggregates(RecordCase):
    """И04."""

    def peaks(self, a_candidate=(4, 4, 5), a_base=(4, 4, 4), b=(6, 7, 9)):
        def hwm(run):
            if run.series == "B":
                return b[run.n - 1]
            return (a_base if run.role == "b" else a_candidate)[run.n - 1]
        self.script.values["hwm_kb"] = hwm

    def test_memory_is_the_peak_of_both_series_against_the_record_and_a_against_a_to_the_base(self):
        self.set_record("hwm_kb", 8, 0.0, base_a_noise=0.0)
        self.peaks()
        code, summary, _ = self.gate()
        row = self.record_row(summary, "hwm_kb")
        self.assertEqual((row["candidate"]["value"], row["outcome"], row["limit"]), (9.0, "block", 8.0),
                         "the median of B (7) and the peak of A (5) would not block")
        self.assertAlmostEqual(row["delta"], 0.125)
        memory = summary["memory_base"]
        self.assertEqual((memory["base"]["value"], memory["candidate"]["value"], memory["outcome"]), (4.0, 5.0, "block"))
        self.assertAlmostEqual(memory["delta"], 0.25)
        self.assertEqual(memory["limit"], 4.0)
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: record:hwm_kb", summary["reasons"])
        self.assertIn("block: memory_base", summary["reasons"])
        self.assertEqual(self.row(summary, "hwm_kb")["outcome"], "ok", "the absolute ceiling is still there, and above these")

    def test_each_part_of_the_memory_rule_decides_on_its_own(self):
        self.peaks()
        for label, record, a_noise, record_outcome, base_outcome in (
                ("both", 8, 0.0, "block", "block"), ("record only", 8, 0.25, "block", "ok"),
                ("A against A only", 9, 0.0, "ok", "block"), ("neither", 9, 0.25, "ok", "ok")):
            with self.subTest(case=label):
                self.set_record("hwm_kb", record, 0.0, base_a_noise=a_noise)
                _, summary, _ = self.gate(label=label.replace(" ", "-"))
                self.assertEqual((self.record_row(summary, "hwm_kb")["outcome"], summary["memory_base"]["outcome"]),
                                 (record_outcome, base_outcome))

    def test_the_tolerance_of_a_against_a_is_inclusive_at_its_limit(self):
        self.peaks()
        self.set_record("hwm_kb", 100, 1000.0, base_a_noise=0.25)
        _, summary, _ = self.gate(label="at")
        self.assertEqual((summary["memory_base"]["limit"], summary["memory_base"]["outcome"]), (5.0, "ok"))
        self.set_record("hwm_kb", base_a_noise=0.2499)
        _, summary, _ = self.gate(label="under")
        self.assertEqual(summary["memory_base"]["outcome"], "block")

    def test_references_are_judged_by_their_median_of_b_and_the_base_runs_no_series_b(self):
        medians = {"references_1": (9, 10, 100), "references_2_same": (9, 10, 100), "references_3_other": (12, 13, 100)}
        for metric, record in (("references_1_s", 10.0), ("references_2_same_s", 10.0), ("references_3_other_s", 11.0)):
            self.set_record(metric, record, 0.0)
        self.script.values["elapsed"] = lambda run, call: medians[call][(run.n - 1) % 3] if call in medians else None
        code, summary, _ = self.gate()
        rows = {r["metric"]: r for r in summary["record"]["rows"]}
        for metric, value, outcome in (("references_1_s", 10.0, "ok"), ("references_2_same_s", 10.0, "ok"),
                                       ("references_3_other_s", 13.0, "block")):
            self.assertEqual((rows[metric]["candidate"]["value"], rows[metric]["outcome"]), (value, outcome), metric)
        self.assertAlmostEqual(rows["references_3_other_s"]["delta"], 2 / 11)
        ceilings = {r["metric"]: r for r in summary["ceilings"]}
        self.assertEqual([ceilings[m]["limit"] for m in ("references_1_s", "references_2_same_s", "references_3_other_s")],
                         [67.7, 32.3, 59.5], "the absolute ceilings stay as they were")
        self.assertEqual({ceilings[m]["outcome"] for m in ceilings if m.startswith("references")}, {"ok"})
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        # No run of the base is a run with the references tool: the base has no series B at all.
        base_runs = [r for r in summary["runs"] if r["role"] == "base"]
        self.assertEqual([(r["series"], r["held"]) for r in base_runs], [("A", True)] * 3)
        base_launches = [launch for launch in self.launches() if launch["env"]["FAKE_LABEL"] == BASE_TAG]
        self.assertEqual(len(base_launches), 3)
        self.assertFalse(any("references" in launch["argv"] for launch in base_launches))
        self.assertEqual(self.roles(summary), "bcbcbcccc")

    def test_the_time_metrics_of_a_are_judged_by_the_median_of_the_three_runs(self):
        self.set_record("graph_resolve_s", 17.8, 0.0)
        self.script.values["elapsed"] = lambda run, call: (17.0, 17.5, 40.0)[(run.n - 1) % 3] \
            if call == "graph_resolve" and run.series == "A" and run.role == "c" else None
        _, summary, _ = self.gate()
        row = self.record_row(summary, "graph_resolve_s")
        self.assertEqual((row["candidate"]["value"], row["outcome"]), (17.5, "ok"), "a slow run among three is not the median")

    def test_the_old_summary_keeps_its_forms(self):
        _, summary, _ = self.gate()
        self.assertEqual([r["metric"] for r in summary["rows"]],
                         ["graph_ready_s", "graph_ready_cpu_s", "graph_resolve_s", "diagnostics_workspace_s"])
        self.assertEqual([r["metric"] for r in summary["ceilings"]],
                         ["hwm_kb", "references_1_s", "references_2_same_s", "references_3_other_s"])
        self.assertEqual(summary["ceilings"][0]["limit"], verdict.GATE_HWM_LIMIT_KB)
        self.assertEqual(summary["answers"]["outcome"], "ok")


class Priority(RecordCase):
    """И05."""

    ORDER = ("pass", "incomplete", "block")
    EXIT = {"pass": 0, "incomplete": 2, "block": 1}

    def reset(self):
        self.write_baseline()
        self.write_noise()
        self.script.values.clear()
        set_mode(*self.modes.split(","))

    def old_rule(self, kind):
        if kind == "block":
            self.script.values["elapsed"] = timed("graph_resolve", lambda run: 19.58 if run.role == "c" else None)
        elif kind == "incomplete":
            self.break_gate_noise("graph_resolve_s")

    def record(self, kind):
        if kind == "block":
            self.set_record("diagnostics_workspace_s", 10.0, 0.01)
        elif kind == "incomplete":
            self.set_record("graph_resolve_s", noise=None)

    def test_block_outranks_incomplete_and_incomplete_outranks_pass_for_both_bases(self):
        for old in self.ORDER:
            for record in self.ORDER:
                with self.subTest(previous_release=old, record=record):
                    self.reset()
                    self.old_rule(old)
                    self.record(record)
                    code, summary, _ = self.gate(label=f"{old}-{record}")
                    expected = max(old, record, key=self.ORDER.index)
                    self.assertEqual((summary["verdict"], code), (expected, self.EXIT[expected]))
                    self.assertEqual(summary["record"]["verdict"], record, "the record is judged on its rows alone")
                    reasons = " | ".join(summary["reasons"])
                    if old == "block":
                        self.assertIn("block: graph_resolve_s", reasons)
                    if old == "incomplete":
                        self.assertIn("no_noise: graph_resolve_s", reasons)
                    if record == "block":
                        self.assertIn("block: record:diagnostics_workspace_s", reasons)
                    if record == "incomplete":
                        self.assertIn("record_no_noise:graph_resolve_s", reasons)

    def test_the_ceiling_the_answers_and_the_memory_against_the_base_enter_the_same_order(self):
        def ceiling():
            self.script.values["hwm_kb"] = lambda run: 8 << 20 if run.series == "B" else 4 << 20

        def differ():
            set_mode(*self.modes.split(","), f"{CAND_LABEL}=resolve_other")

        def no_control():
            set_mode("graph_answers_same", "sweep_answers", "refs_tool")

        def memory_block():
            self.set_record("hwm_kb", 100, 1000.0, base_a_noise=0.0)
            self.script.values["hwm_kb"] = lambda run: 5 if (run.role, run.series) == ("c", "A") else 4

        def memory_incomplete():
            self.set_record("hwm_kb", base_a_noise=None)

        for name, setup, expected, reason in (
                ("ceiling", ceiling, "block", "block: hwm_kb"), ("answers", differ, "block", "answers_differ"),
                ("memory against the base", memory_block, "block", "block: memory_base"),
                ("answers control", no_control, "incomplete", "answers_control"),
                ("tolerance of A against A", memory_incomplete, "incomplete", "record_no_noise:hwm_A_kb")):
            opposite = "incomplete" if expected == "block" else "block"
            for record in ("pass", opposite):
                with self.subTest(part=name, record=record):
                    self.reset()
                    setup()
                    self.record(record)
                    code, summary, _ = self.gate(label=f"{name}-{record}".replace(" ", "-"))
                    final = max(expected, record, key=self.ORDER.index)
                    self.assertEqual((summary["verdict"], code), (final, self.EXIT[final]))
                    self.assertIn(reason, summary["reasons"])

    def test_a_legacy_pass_does_not_make_a_pass_without_the_record(self):
        # The exit code and the verdict never say pass while the record does not.
        for record in ("block", "incomplete"):
            with self.subTest(record=record):
                self.reset()
                self.record(record)
                code, summary, _ = self.gate(label=record)
                self.assertEqual(self.decided(summary), {"ok"})
                self.assertNotEqual((code, summary["verdict"]), (0, "pass"))
                self.assertEqual(summary["record"]["verdict"], record)

    def test_the_known_peak_over_the_target_blocks_by_memory_though_the_previous_release_passes(self):
        # 6 917 564 KB against the target 4 667 316 KB: +48.2128915 %.
        self.set_record("hwm_kb", 4667316, 0.01, base_a_noise=1000.0)
        self.script.values["hwm_kb"] = lambda run: 6917564 if run.series == "B" else 4739312
        code, summary, _ = self.gate()
        row = self.record_row(summary, "hwm_kb")
        self.assertEqual((row["candidate"]["value"], row["outcome"]), (6917564.0, "block"))
        self.assertAlmostEqual(row["delta"], 0.48212891520522716, places=12)
        self.assertEqual(self.decided(summary), {"ok"}, "the previous release: pass")
        self.assertEqual(self.row(summary, "hwm_kb")["outcome"], "ok", "under the old ceiling of 7 GiB")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: record:hwm_kb", summary["reasons"])
        for nu, outcome in ((0.4821, "block"), (0.4822, "ok")):
            with self.subTest(noise=nu):
                self.set_record("hwm_kb", noise=nu)
                _, summary, _ = self.gate(label=f"nu-{nu}")
                self.assertEqual(self.record_row(summary, "hwm_kb")["outcome"], outcome)

    def test_a_measured_excess_stands_when_the_second_round_is_cut_short(self):
        # Round one improves the work metric, which asks for a second round, and takes memory over its ceiling;
        # the machine is busy from the tenth run on. An incomplete round must not erase what was measured.
        self.set_record("graph_resolve_s", 100.0, 0.01)
        self.script.values["elapsed"] = timed("graph_resolve", 98.0)
        self.script.values["hwm_kb"] = lambda run: 8 << 20 if run.series == "B" else 4 << 20
        self.machine.quiet_of = lambda n: len(self.script.runs) < 9
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(summary["reasons"][0], "machine_busy")
        self.assertIn("block: hwm_kb", summary["reasons"])
        self.assertEqual(summary["record_confirmation"]["complete"], False)
        self.assertEqual(summary["record_proposals"], [])

    def test_a_probe_never_vouches_for_a_release_and_never_proposes_a_record(self):
        # An improvement beyond the noise: a full gate would run a second round and propose it.
        self.set_record("graph_ready_s", 120.0, 0.01)
        code, summary, _ = self.gate("--ready-only", label="probe")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (0, "probe-ok", "probe-ok"))
        self.assertEqual([r["metric"] for r in summary["record"]["rows"]], ["graph_ready_s", "graph_ready_cpu_s"])
        self.assertEqual((summary["record_proposals"], summary["record_confirmation"]["requested"]), ([], False))
        self.assertEqual(self.roles(summary), "bcbcbc")
        code, summary, _ = self.gate("--control-foreign-burn", "1", label="injected")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (0, "probe-ok", "probe-ok"))
        self.assertEqual((summary["record_proposals"], summary["record_confirmation"]["requested"]), ([], False))
        self.assertEqual(self.roles(summary), "bcbcbcccc", "no second round for an injected run")
        # The full gate of the same world proposes: the probe's silence is the probe's own.
        code, summary, _ = self.gate(label="full")
        self.assertEqual((code, summary["verdict"], [p["metric"] for p in summary["record_proposals"]]),
                         (0, "pass", ["graph_ready_s"]))

    def test_a_probe_can_still_block_by_the_record(self):
        self.set_record("graph_ready_s", 90.0, 0.01)
        code, summary, _ = self.gate("--ready-only")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (1, "block", "block"))


if __name__ == "__main__":
    unittest.main()
