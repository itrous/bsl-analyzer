"""`analyze` scenario: argv, jsonl validity, analysis scope, help (plan §8.1)."""

import os
import subprocess
import unittest

from helpers import FakeClock, Workspace, run_git, set_mode

import scenarios
from common import HARNESS_SCHEMA, median


class AnalyzeRuns(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def run_analyze(self, *switches):
        set_mode(*switches)
        self.measure(scenario="analyze", temp="warm", runs=1)
        return self.record("analyze", "r1")

    def test_argv_and_cwd_are_exactly_the_plan(self):
        record = self.run_analyze()
        self.assertTrue(record["held"], record["errors"])
        launch = [l for l in self.launches() if "BSL_LOG" not in l["env"]][0]
        self.assertEqual(launch["argv"], ["analyze", "--format=jsonl", "--quiet", "--source-dir", self.snap])
        self.assertEqual(os.path.realpath(launch["cwd"]), os.path.realpath(self.snap))
        for key in ("wall_s", "max_rss_kb", "cpu_s", "cpu_pct", "total_files"):
            self.assertIn(key, record["metrics"])
        self.assertEqual(record["exit_code"], 0)
        self.assertEqual(record["schema"], HARNESS_SCHEMA)
        self.assertEqual(record["env_fingerprint"]["harness_schema"], HARNESS_SCHEMA)

    def test_broken_streams_and_failures_are_not_held(self):
        for switch, reason in (("no_done", "invalid_jsonl"), ("total_mismatch", "invalid_jsonl"),
                               ("bad_line", "invalid_jsonl"), ("bad_utf8", "invalid_jsonl"),
                               ("zero_files", "empty_analysis"),
                               ("exit2", "analyze_exit"), ("help_warn", "help_degraded"),
                               ("only_first", "scope_mismatch")):
            with self.subTest(switch=switch):
                record = self.run_analyze(switch)
                self.assertEqual((record["held"], record["reason"]), (False, reason))
                os.rename(self.series_dir("analyze"), self.series_dir("analyze") + f"-{switch}")

    def test_invalid_utf8_does_not_abort_the_series(self):
        set_mode("bad_utf8")
        self.measure(scenario="analyze", temp="warm", runs=3)
        records = [self.record("analyze", f"r{k}") for k in (1, 2, 3)]
        self.assertEqual([(r["held"], r["reason"]) for r in records], [(False, "invalid_jsonl")] * 3)
        self.assertEqual(len([l for l in self.launches() if "BSL_LOG" not in l["env"]]), 3)


class JsonlValidity(unittest.TestCase):
    def test_counts_errors_and_rejects_each_structural_violation(self):
        import json
        import tempfile
        events = [{"type": "start", "total_files": 1},
                  {"type": "file", "path": "a.bsl", "diagnostics": [{}], "error": "x"},
                  {"type": "done", "total_files": 1, "total_diagnostics": 1, "failed_files": 1}]
        with tempfile.NamedTemporaryFile("w+", encoding="utf-8") as fh:
            def validate(value):
                fh.seek(0)
                fh.truncate()
                for event in value:
                    fh.write(json.dumps(event) + "\n")
                fh.flush()
                return scenarios.validate_jsonl(fh.name)

            self.assertEqual(validate(events), {"total_files": 1, "total_diagnostics": 1,
                                                "failed_files": 1, "files_with_error": 1, "paths": ["a.bsl"]})
            variants = [[], [[], *events], events[1:], events[:-1],
                        [events[0], events[1], {**events[2], "total_files": 2}],
                        [{**events[0], "total_files": 2}, *events[1:]],
                        [*events[:2], {**events[2], "total_diagnostics": 2}]]
            for number, value in enumerate(variants):
                with self.subTest(violation=number), self.assertRaisesRegex(scenarios.NotHeld, "invalid_jsonl"):
                    validate(value)


class Scope(unittest.TestCase):
    """Invariant И50 on a git corpus: base `vendor`, a commit changes two modules."""

    def setUp(self):
        import tempfile
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        run_git("init", "-q", "-b", "main", cwd=self.root)
        run_git("config", "user.email", "t@t", cwd=self.root)
        run_git("config", "user.name", "t", cwd=self.root)
        for name in ("a", "b", "c"):
            self.write(f"{name}.bsl", "Процедура П()\nКонецПроцедуры\n")
        self.write("bsl-analyzer.toml", '[analysis]\ndiff_base = "vendor"\n')
        run_git("add", "-A", cwd=self.root)
        run_git("commit", "-q", "-m", "base", cwd=self.root)
        self.base = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.root, check=True,
                                   capture_output=True, text=True).stdout.strip()
        for name in ("a", "b"):
            self.write(f"{name}.bsl", "Процедура П()\n\tВозврат;\nКонецПроцедуры\n")
        run_git("commit", "-q", "-am", "change", cwd=self.root)
        self.meta = {"mode": "diff", "diff_base_sha": self.base, "files_total": 300}

    def tearDown(self):
        self._tmp.cleanup()

    def write(self, name, text):
        with open(os.path.join(self.root, name), "w", encoding="utf-8") as fh:
            fh.write(text)

    def summary(self, *names):
        return {"total_files": len(names), "paths": [os.path.join(self.root, n) for n in names]}

    def test_diff_area_fully_analysed_passes(self):
        scenarios.check_scope(self.root, self.meta, self.summary("a.bsl", "b.bsl"))

    def test_one_of_two_area_files_is_a_mismatch(self):
        with self.assertRaisesRegex(scenarios.NotHeld, "scope_mismatch"):
            scenarios.check_scope(self.root, self.meta, self.summary("a.bsl"))

    def test_a_file_outside_the_area_is_a_mismatch(self):
        with self.assertRaisesRegex(scenarios.NotHeld, "scope_mismatch"):
            scenarios.check_scope(self.root, self.meta, self.summary("a.bsl", "b.bsl", "c.bsl"))

    def test_diff_covering_the_whole_snapshot_is_a_mismatch(self):
        with self.assertRaisesRegex(scenarios.NotHeld, "scope_mismatch"):
            scenarios.check_scope(self.root, {**self.meta, "files_total": 2}, self.summary("a.bsl", "b.bsl"))

    def test_full_mode_with_diff_base_in_the_config_is_a_mismatch(self):
        with self.assertRaisesRegex(scenarios.NotHeld, "scope_mismatch"):
            scenarios.check_scope(self.root, {"mode": "full", "files_total": 3},
                                  self.summary("a.bsl", "b.bsl", "c.bsl"))


class Aggregation(unittest.TestCase):
    def test_first_and_repeat_are_kept_apart(self):
        samples = [{"elapsed": e, "ok": True, "error": None, "timeout": False} for e in (5.0, 1.1, 1.2, 1.3)]
        entry = scenarios._aggregate("M1", samples)
        self.assertEqual((entry["first_s"], entry["repeat_s"]), (5.0, 1.2))
        self.assertEqual(median([3.0, None, 1.0, 2.0]), 2.0)

    def test_gnu_time_report_is_parsed(self):
        report = ("\tCommand being timed: \"x\"\n\tUser time (seconds): 200.10\n\tSystem time (seconds): 40.20\n"
                  "\tPercent of CPU this job got: 839%\n\tElapsed (wall clock) time (h:mm:ss or m:ss): 1:02.50\n"
                  "\tMaximum resident set size (kbytes): 1468000\n\tExit status: 0\n")
        self.assertEqual(scenarios.parse_gnu_time(report), {
            "user_s": 200.1, "sys_s": 40.2, "cpu_pct": 839, "wall_s": 62.5, "max_rss_kb": 1468000,
            "exit_code": 0})


class HelpCheck(Workspace):
    def test_loaded_from_the_configured_source_passes(self):
        self.assertEqual(self.helpcheck(), 0)

    def test_missing_line_other_source_or_warning_fail(self):
        for switch in ("help_noline", "help_auto", "help_degraded"):
            with self.subTest(switch=switch):
                set_mode(switch)
                self.assertEqual(self.helpcheck(), 1)

    def test_a_failed_check_is_recorded_for_measure(self):
        set_mode("help_degraded")
        self.helpcheck()
        set_mode()
        self.measure(clock=FakeClock(), scenario="lsp-session", temp="warm", runs=1)
        self.assertEqual(self.record("lsp-session", "r1")["reason"], "help_unavailable")


if __name__ == "__main__":
    unittest.main()
