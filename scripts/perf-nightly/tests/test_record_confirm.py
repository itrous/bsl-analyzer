"""The second round and the proposals of the record (plan
docs/plans/perf-ratchet-record.md, И06 and the part of И12 that the gate
writes): a record moves down only after two full rounds beyond its noise, and
only a person moves it. The clock is virtual and the machine scripted
(`gate_fixtures.py`)."""

import unittest

from gate_fixtures import CAND_SHA, CAND_LABEL, timed
from helpers import set_mode
from record_fixtures import RecordCase

import gate

ROUND_ONE = ["A-b1", "A-c1", "A-b2", "A-c2", "A-b3", "A-c3", "B-c1", "B-c2", "B-c3"]
ROUND_TWO = ["A-b4", "A-c4", "A-b5", "A-c5", "A-b6", "A-c6", "B-c4", "B-c5", "B-c6"]


def per_run(*values):
    """Script value: the number of the n-th run (1 to 3 in the first round, 4 to 6 in the second)."""
    return lambda run: values[run.n - 1]


class Proposals(RecordCase):
    """И06."""

    def work_record(self, record=100.0, noise=0.01):
        self.set_record("graph_resolve_s", record, noise)

    def work(self, value):
        self.script.values["elapsed"] = timed("graph_resolve", value)

    def proposed(self, summary) -> list[str]:
        return [p["metric"] for p in summary["record_proposals"]]

    def test_two_full_rounds_beyond_the_noise_propose_the_worse_of_the_two(self):
        self.work_record()
        # Medians of the rounds 98 and 98.5; the six runs together would give 98.35.
        self.work(per_run(97.0, 98.0, 99.0, 98.2, 98.5, 99.0))
        before = self.baseline_bytes()
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (0, "pass"))
        self.assertEqual([r["run"] for r in summary["runs"]], ROUND_ONE + ROUND_TWO)
        self.assertEqual(summary["record_confirmation"],
                         {"requested": True, "triggers": ["graph_resolve_s"], "complete": True})
        [proposal] = summary["record_proposals"]
        self.assertEqual((proposal["metric"], proposal["unit"], proposal["aggregate"]),
                         ("graph_resolve_s", "s", "median_A"))
        self.assertEqual((proposal["value"], proposal["previous_value"], proposal["rounds"]), (98.5, 100.0, [98.0, 98.5]))
        self.assertEqual(proposal["record_sha"], CAND_SHA)
        self.assertEqual(proposal["noise"], self.baseline_doc()["metrics"]["graph_resolve_s"]["noise"])
        self.assertEqual(proposal["profile"], self.fingerprint())
        self.assertTrue(proposal["measured_at"])
        self.assertEqual(proposal["evidence"], {
            "kind": "confirmed", "summary": "data/gates/g1/summary.json", "role": "candidate", "series": "A",
            "runs": ["A-c1", "A-c2", "A-c3"], "confirmation": ["A-c4", "A-c5", "A-c6"]})
        self.assertEqual(self.baseline_bytes(), before, "the gate proposes and does not write")
        row = self.record_row(summary, "graph_resolve_s")
        self.assertEqual([c["candidate"]["value"] for c in row["rounds"]], [98.0, 98.5])

    def test_memory_is_proposed_at_the_worse_peak_of_a_and_b_together(self):
        self.set_record("hwm_kb", 100, 0.01, base_a_noise=1000.0)

        def hwm(run):
            return 90 if run.series == "A" else (95, 96, 97, 96, 98, 95)[run.n - 1]

        self.script.values["hwm_kb"] = hwm
        _, summary, _ = self.gate()
        [proposal] = summary["record_proposals"]
        self.assertEqual((proposal["metric"], proposal["value"], proposal["rounds"]), ("hwm_kb", 98.0, [97.0, 98.0]),
                         "the peak of A+B of each round, not of A alone")
        self.assertEqual((proposal["aggregate"], proposal["evidence"]["series"]), ("max_AB", "A+B"))
        self.assertEqual(proposal["evidence"]["runs"], ["A-c1", "A-c2", "A-c3", "B-c1", "B-c2", "B-c3"])
        self.assertEqual(proposal["evidence"]["confirmation"], ["A-c4", "A-c5", "A-c6", "B-c4", "B-c5", "B-c6"])

    def test_a_reference_is_proposed_by_the_median_of_b_of_each_round(self):
        self.set_record("references_1_s", 60.0, 0.01)
        series = {"references_1": (50.0, 51.0, 70.0, 52.0, 53.0, 80.0)}
        self.script.values["elapsed"] = lambda run, call: series[call][run.n - 1] \
            if call in series and run.series == "B" else None
        _, summary, _ = self.gate()
        [proposal] = summary["record_proposals"]
        self.assertEqual((proposal["metric"], proposal["value"], proposal["rounds"]),
                         ("references_1_s", 53.0, [51.0, 53.0]))
        self.assertEqual((proposal["evidence"]["series"], proposal["evidence"]["runs"],
                          proposal["evidence"]["confirmation"]), ("B", ["B-c1", "B-c2", "B-c3"], ["B-c4", "B-c5", "B-c6"]))

    def test_a_round_that_does_not_repeat_the_improvement_proposes_nothing(self):
        cases = {"the second round is back at the record": (98.0, 100.0),
                 "the second round improves the mean but not beyond the noise": (98.0, 99.5),
                 "the second round is worse than the record": (98.0, 105.0),
                 "the first round is the one that is not beyond the noise": (99.5, 98.0),
                 "exactly at the edge of the noise": (99.0, 99.0)}
        for number, (name, (first, second)) in enumerate(cases.items()):
            with self.subTest(name):
                self.work_record()
                self.work(lambda run, a=first, b=second: a if run.n <= 3 else b)
                code, summary, _ = self.gate(label=f"r{number}")
                self.assertEqual(summary["record_proposals"], [])
                self.assertEqual(code, 1 if second == 105.0 else 0)

    def test_an_improvement_inside_the_noise_asks_for_no_second_round(self):
        self.work_record()
        self.work(99.0)
        _, summary, _ = self.gate()
        self.assertEqual([r["run"] for r in summary["runs"]], ROUND_ONE)
        self.assertEqual((summary["record_proposals"], summary["record_confirmation"]["requested"]), ([], False))

    def test_a_second_round_the_machine_cut_short_proposes_nothing_and_leaves_the_gate_incomplete(self):
        self.work_record()
        self.work(98.0)
        # Quiet for the nine runs of the first round and never again.
        self.machine.quiet_of = lambda n: len(self.script.runs) < 9
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
        self.assertEqual(summary["reasons"][0], "machine_busy")
        self.assertEqual(summary["record_confirmation"], {"requested": True, "triggers": ["graph_resolve_s"],
                                                         "complete": False})
        self.assertEqual(summary["record_proposals"], [])
        self.assertEqual([r["run"] for r in summary["runs"]], ROUND_ONE)

    def test_a_second_round_that_lost_its_series_b_proposes_nothing_even_for_a_metric_of_a(self):
        # The work metric is judged by the series A, which both rounds hold in full; the round as a whole is not.
        self.work_record()
        self.work(98.0)
        self.machine.quiet_of = lambda n: len(self.script.runs) < 15
        code, summary, _ = self.gate()
        self.assertEqual([r["run"] for r in summary["runs"]], ROUND_ONE + ROUND_TWO[:6])
        row = self.record_row(summary, "graph_resolve_s")
        self.assertEqual([c["improved"] for c in row["rounds"]], [True, True])
        self.assertEqual(summary["record_proposals"], [])
        self.assertEqual(summary["record_confirmation"]["complete"], False)
        self.assertEqual((code, summary["verdict"], summary["reasons"][0]), (2, "incomplete", "machine_busy"))

    def test_a_round_one_improvement_whose_answers_differ_proposes_nothing(self):
        self.work_record()
        self.work(98.0)
        set_mode(*self.modes.split(","), f"{CAND_LABEL}=resolve_other")
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(summary["record_proposals"], [])

    def test_a_block_of_another_metric_does_not_forbid_confirming_an_independent_improvement(self):
        self.work_record()
        self.set_record("diagnostics_workspace_s", 15.0, 0.01)   # 22.1 against 15: a block
        self.work(per_run(98.0, 98.0, 98.0, 98.5, 98.5, 98.5))
        code, summary, _ = self.gate()
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertEqual(self.proposed(summary), ["graph_resolve_s"])

    def test_a_record_that_is_already_lower_rejects_the_older_proposal(self):
        # The same two rounds against a record of 97: 98 and 98.5 are no improvement over it.
        self.work_record(record=97.0)
        self.work(per_run(98.0, 98.0, 98.0, 98.5, 98.5, 98.5))
        code, summary, _ = self.gate()
        self.assertEqual(summary["record_proposals"], [])
        self.assertEqual(summary["record_confirmation"]["requested"], False)
        self.assertEqual(code, 1, "and 98 over 97·1.01 is a loss against that record")

    def test_a_proposal_is_an_entry_the_harness_accepts_and_the_next_gate_holds_to_it(self):
        self.work_record()
        self.work(per_run(98.0, 98.0, 98.0, 98.5, 98.5, 98.5))
        _, summary, _ = self.gate(label="first")
        [proposal] = summary["record_proposals"]
        self.assertEqual(proposal["profile"], self.baseline_doc()["profile"], "one profile")

        def take(doc):
            doc["metrics"][proposal["metric"]] = {key: proposal[key] for key in (
                "value", "unit", "aggregate", "record_sha", "measured_at", "evidence", "noise")}
        self.edit_baseline(take)
        self.assertIsNone(gate.Baseline(self.baseline_path).problem("graph_resolve_s"))
        for number, (candidate, outcome) in enumerate(((98.5, "ok"), (99.4, "ok"), (99.5, "block"))):
            with self.subTest(candidate=candidate):
                self.work(candidate)
                code, summary, _ = self.gate(label=f"after{number}")
                row = self.record_row(summary, "graph_resolve_s")
                self.assertEqual((row["record"]["value"], row["outcome"], summary["record_proposals"]),
                                 (98.5, outcome, []))

    def test_a_release_that_beats_nothing_leaves_the_record_where_it_was(self):
        self.work_record(100.0, 0.01)
        for number, value in enumerate((100.0, 100.9, 101.5)):
            self.work(value)
            before = self.baseline_bytes()
            _, summary, _ = self.gate(label=f"bump{number}")
            self.assertEqual(summary["record_proposals"], [])
            self.assertEqual(self.baseline_bytes(), before)
        self.assertEqual(self.baseline_doc()["metrics"]["graph_resolve_s"]["value"], 100.0)


if __name__ == "__main__":
    unittest.main()
