"""Medians, failed calls and private error details in the slice report."""

import argparse
import contextlib
import io
import os
import tempfile
import unittest

import helpers  # noqa: F401 — installs the harness import path

import perf_nightly
from common import Data, HARNESS_SCHEMA, write_json


class Report(unittest.TestCase):
    def setUp(self):
        self.tmp = self.enterContext(tempfile.TemporaryDirectory(prefix="perf-nightly-report-"))
        self.data = Data(self.tmp)

    def record(self, label, k, metric, calls=(), held=True, series="t", schema=HARNESS_SCHEMA):
        binary = {"label": label, "sha": "a" * 40}
        path = os.path.join(self.data.results(binary), "crm", "mcp-memory", "warm", series, f"r{k}.json")
        write_json(path, {"binary": binary, "held": held, "reason": None if held else "backend_failed",
                          "metrics": {"rss_ready_kb": metric}, "calls": list(calls),
                          "env_fingerprint": {"harness_schema": schema}, "schema": schema})

    def report(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(perf_nightly.cmd_report(argparse.Namespace(
                data_dir=self.tmp, corpus="crm", series="t")), 0)
        with open(self.data.path("reports", "crm.md"), encoding="utf-8") as fh:
            return out.getvalue(), fh.read()

    def test_three_run_medians_keep_first_and_repeat_separate(self):
        for k, value, first, repeat in ((1, 90, 5.0, 1.1), (2, 10, 9.0, 1.3), (3, 20, 7.0, 1.2)):
            self.record("fake", k, value, [{"id": "M1", "ok": True, "first_s": first, "repeat_s": repeat}])
        out, saved = self.report()
        for row in ("| rss_ready_kb | 20 |", "| M1.first_s | 7.000 |", "| M1.repeat_s | 1.200 |"):
            self.assertIn(row, out)
            self.assertIn(row, saved)

    def test_invalid_calls_are_not_reported_as_latency_and_errors_are_named(self):
        for label in ("base", "candidate"):
            for k in (1, 2, 3):
                self.record(label, k, 100, [{"id": "M1", "ok": False, "first_s": 999,
                                             "error": "unavailable: private detail"},
                                            {"id": "M5", "ok": None, "na": True}])
        out, saved = self.report()
        self.assertIn("| M1.first_s | invalid | invalid |", out)
        self.assertNotIn("999", out)
        self.assertIn("base-aaaaaaaaaaaa=1", out)
        self.assertIn("candidate-aaaaaaaaaaaa=1", out)
        self.assertIn("mcp-memory/warm/M1", out)
        self.assertNotIn("private detail", out)
        self.assertIn("unavailable: private detail", saved)

    def test_failed_runs_do_not_produce_a_median(self):
        self.record("fake", 1, 100)
        self.record("fake", 2, 1000, held=False)
        self.record("fake", 3, 10)
        out, _ = self.report()
        self.assertIn("| rss_ready_kb | failed |", out)

    def test_series_filter_does_not_mix_a_previous_harness_series(self):
        for k in (1, 2, 3):
            self.record("fake", k, 20)
            self.record("fake", k, 999, series="old", schema=HARNESS_SCHEMA - 1)
        out, _ = self.report()
        self.assertIn("| rss_ready_kb | 20 |", out)
        self.assertNotIn("999", out)

    def test_measure_records_the_harness_schema_in_its_fingerprint(self):
        fingerprint = perf_nightly.env_fingerprint({"rustc": "rustc test"},
                                                   {"commit": "b" * 40, "mode": "full"}, "points")
        self.assertEqual(fingerprint["harness_schema"], HARNESS_SCHEMA)
        self.assertEqual(fingerprint["snapshot_commit"], "b" * 40)
        self.assertEqual(fingerprint["points_sha256"], "points")
