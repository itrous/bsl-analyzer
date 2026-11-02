"""`gate-calibrate --record-metrics-only` (plan docs/plans/perf-ratchet-record.md,
И07): the tolerances of peak memory and of the references that the record
lacks, measured on a fixed binary and joined to the calibration that exists
without touching it. The clock is virtual and the machine scripted
(`gate_fixtures.py`)."""

import contextlib
import io
import os
import unittest

from gate_fixtures import BASE_TAG
from record_fixtures import RecordCase

import gate
import perf_nightly
import verdict
from common import HarnessError, read_json

CALIBRATION_BASE = "v0.4.2"
NEW = ("hwm_A_kb", "hwm_kb", "references_1_s", "references_2_same_s", "references_3_other_s")
OLD = ("graph_ready_s", "graph_ready_cpu_s", "graph_resolve_s", "diagnostics_workspace_s")
SERIES = 5
RUNS_PER_SERIES = 6


def key(metric: str) -> str:
    return verdict.noise_key("erp", gate.SCENARIO, gate.TEMP, metric)


class CalibrateRecord(RecordCase):
    def setUp(self):
        super().setUp()
        self.base_path = self.fake_binary(CALIBRATION_BASE, "d" * 40)
        self.root = self.data.path("gate-cal", f"{CALIBRATION_BASE}-{'d' * 12}")

    def series_of(self) -> int:
        """The series (0 to 4) of the run that is running: six runs make one."""
        return (len(self.script.runs) - 1) // RUNS_PER_SERIES

    def builder(self, ref, label):
        self.builds.append((ref, label))
        return self.base_path

    def measure(self, *extra, base=CALIBRATION_BASE, machine=None):
        args = perf_nightly.parser().parse_args(["gate-calibrate", "--data-dir", self.data_dir, "--base", base,
                                                 "--record-metrics-only", *extra])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_gate_calibrate(args, self.clock, self.builder, machine or self.machine)
        return code, out.getvalue()

    def script_series(self, memory_b=(100, 101, 102, 103, 104), memory_a=(50, 51, 52, 53, 54), peak=None,
                      refs=None):
        """Aggregates of the five series: the peak of B and of A, and the median of B per reference."""
        refs = refs or {"references_1": (10, 11, 12, 13, 14), "references_2_same": (5, 5, 5, 5, 5),
                        "references_3_other": (30, 30, 30, 30, 36)}

        def hwm(run):
            if run.series == "A" and peak is not None and (self.series_of(), run.n) == peak[:2]:
                return peak[2]
            return (memory_a if run.series == "A" else memory_b)[self.series_of()]

        self.script.values["hwm_kb"] = hwm
        self.script.values["elapsed"] = lambda run, call: float(refs[call][self.series_of()]) \
            if call in refs and run.series == "B" else None

    def noise_file(self) -> dict:
        return read_json(self.data.path("gate-noise.json"))

    def test_five_full_series_give_each_aggregate_its_own_spread(self):
        self.script_series()
        old = self.noise_file()
        code, line = self.measure()
        self.assertEqual(code, 0, line)
        self.assertIn("merged=true", line)
        entries = self.noise_file()["entries"]
        # hwm_kb is the peak of A+B (B here), hwm_A_kb the peak of A: (max − min) / median of the five series.
        self.assertAlmostEqual(entries[key("hwm_kb")]["noise"], 4 / 102)
        self.assertAlmostEqual(entries[key("hwm_A_kb")]["noise"], 4 / 52)
        self.assertAlmostEqual(entries[key("references_1_s")]["noise"], 4 / 12)
        self.assertEqual(entries[key("references_2_same_s")]["noise"], 0.0)
        self.assertAlmostEqual(entries[key("references_3_other_s")]["noise"], 6 / 30)
        self.assertEqual(entries[key("hwm_kb")]["aggregates"], [100.0, 101.0, 102.0, 103.0, 104.0])
        self.assertEqual(entries[key("hwm_A_kb")]["aggregates"], [50.0, 51.0, 52.0, 53.0, 54.0])
        for metric in NEW:
            entry = entries[key(metric)]
            self.assertEqual(entry["states"], ["valid"] * SERIES)
            self.assertEqual(entry["binary"], {"label": CALIBRATION_BASE, "sha": "d" * 40,
                                               "sha256": perf_nightly.load_binary(self.base_path)["sha256"]})
            self.assertEqual(entry["fingerprint"], self.fingerprint())
            self.assertTrue(entry["created_at"])
            self.assertNotEqual(entry["created_at"], old["created_at"], "the date of the new entry is its own")
        for metric in OLD:
            self.assertEqual(entries[key(metric)], old["entries"][key(metric)], "the old entries stay as they were")
        for field in ("schema", "harness_schema", "gate_schema", "fingerprints", "binary", "created_at"):
            self.assertEqual(self.noise_file()[field], old[field], "the root of the calibration is not rewritten")
        self.assertIn("nu.hwm_kb=", line)
        # 5 series of A×3 + B×3, the base only.
        self.assertEqual(len(self.launches()), SERIES * RUNS_PER_SERIES)
        self.assertEqual(sorted((r.series, r.role) for r in self.script.runs),
                         sorted([("A", "b")] * 15 + [("B", "b")] * 15))
        self.assertEqual({launch["env"]["FAKE_LABEL"] for launch in self.launches()}, {CALIBRATION_BASE})
        self.assertEqual(self.builds, [(CALIBRATION_BASE, CALIBRATION_BASE)])
        saved = read_json(os.path.join(self.root, "record-noise.json"))
        self.assertEqual(saved["differs_from_calibration"], [])
        self.assertEqual(sorted(saved["entries"]), sorted(key(m) for m in NEW))
        self.assertIsNone(gate.noise_stale(self.noise_file(), self.fingerprint()), "the gate still takes it as current")

    def test_one_peak_inside_a_moves_the_spread_though_the_median_of_a_does_not(self):
        self.script_series(peak=(2, 2, 200))                 # the third series: one run of A reaches 200
        code, line = self.measure()
        self.assertEqual(code, 0, line)
        entries = self.noise_file()["entries"]
        self.assertEqual(entries[key("hwm_A_kb")]["aggregates"], [50.0, 51.0, 200.0, 53.0, 54.0])
        self.assertAlmostEqual(entries[key("hwm_A_kb")]["noise"], (200 - 50) / 53)
        self.assertEqual(entries[key("hwm_kb")]["aggregates"], [100.0, 101.0, 200.0, 103.0, 104.0])
        self.assertAlmostEqual(entries[key("hwm_kb")]["noise"], (200 - 100) / 103)

    def test_a_series_short_of_a_run_of_b_makes_no_number_and_no_zero(self):
        self.script_series()
        before = read_bytes(self.data.path("gate-noise.json"))
        # The twelfth run is the last of the second series; from it the machine stays busy for the whole
        # budget (60 min in polls of 30 s, and the check that spends it), then it is quiet again.
        self.machine.quiet_of = lambda n: not 12 <= n <= 12 + 120
        code, line = self.measure()
        self.assertEqual(code, 2, line)
        self.assertIn("merged=false", line)
        self.assertIn("busy=s2", line)
        self.assertEqual(read_bytes(self.data.path("gate-noise.json")), before)
        saved = read_json(os.path.join(self.root, "record-noise.json"))
        for metric in NEW:
            entry = saved["entries"][key(metric)]
            self.assertIsNone(entry["noise"], metric)
            own = "another aggregate is not valid in every series" if metric == "hwm_A_kb" else "not valid in every series"
            self.assertEqual(entry["reason"], own, metric)
        for metric in ("hwm_kb", "references_1_s", "references_2_same_s", "references_3_other_s"):
            states = saved["entries"][key(metric)]["states"]
            self.assertEqual((states[1], states.count("valid")), ("missing", SERIES - 1), metric)
        shown = [part.split("=") for part in line.split() if part.startswith("nu.")]
        self.assertEqual(sorted(name for name, _ in shown), sorted(f"nu.{m}" for m in NEW))
        self.assertEqual({value for _, value in shown}, {"-"}, "no tolerance is printed, least of all a zero")

    def test_a_calibration_of_another_profile_is_kept_apart(self):
        self.script_series()
        before = read_bytes(self.data.path("gate-noise.json"))
        self.machine.print["kernel"] = "7.0"                 # not what stales the old calibration, but the record's profile
        code, line = self.measure()
        self.assertEqual(code, 2, line)
        self.assertIn("merged=false", line)
        self.assertIn("differs=kernel", line)
        self.assertEqual(read_bytes(self.data.path("gate-noise.json")), before, "the old calibration is not declared current")
        saved = read_json(os.path.join(self.root, "record-noise.json"))
        self.assertEqual(saved["differs_from_calibration"], ["kernel"])
        self.assertAlmostEqual(saved["entries"][key("hwm_kb")]["noise"], 4 / 102, msg="the measurement itself is kept")

    def test_without_any_calibration_the_numbers_are_kept_apart_too(self):
        self.script_series()
        os.remove(self.data.path("gate-noise.json"))
        code, line = self.measure()
        self.assertEqual(code, 2, line)
        self.assertFalse(os.path.exists(self.data.path("gate-noise.json")))
        self.assertEqual(read_json(os.path.join(self.root, "record-noise.json"))["differs_from_calibration"],
                         ["gate-noise.json"])

    def test_only_the_release_that_has_the_references_tool_is_measured(self):
        with self.assertRaisesRegex(HarnessError, "base_invalid"):
            self.measure(base=BASE_TAG)
        self.assertEqual((self.builds, self.launches()), ([], []))

    def test_fewer_than_five_series_are_refused(self):
        with self.assertRaisesRegex(HarnessError, "series_invalid"):
            self.measure("--series", "4")
        self.assertEqual((self.builds, self.launches()), ([], []))


def read_bytes(path):
    with open(path, "rb") as fh:
        return fh.read()


if __name__ == "__main__":
    unittest.main()
