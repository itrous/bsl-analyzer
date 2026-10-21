"""`measure` orchestration on the fake binary: temperature, cache map,
foreign processes, help check and refusals (plan §7, §8)."""

import fcntl
import os
import signal
import subprocess
import sys
import unittest

from helpers import FOREIGN_PREFIX, FakeClock, Workspace, set_mode

import corpus
import perf_nightly
import procs
from common import HarnessError, repo_root


class Temperature(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def analyze_launches(self):
        return [l for l in self.launches() if l["argv"][:1] == ["analyze"] and "BSL_LOG" not in l["env"]]

    def test_warm_with_a_cache_has_a_warmup_and_three_runs_on_one_root(self):
        set_mode("cache", "build")
        self.measure(scenario="analyze", temp="warm")
        launches = self.analyze_launches()
        self.assertEqual(len(launches), 4)
        self.assertEqual([l["had_build"] for l in launches], [False, True, True, True])
        self.assertEqual(len({l["cache"] for l in launches}), 1)
        self.assertTrue(os.path.exists(os.path.join(self.series_dir("analyze"), "warmup.json")))
        self.assertTrue(self.record("analyze", "warmup")["warmup"])
        self.assertEqual([self.record("analyze", f"r{k}")["k"] for k in (1, 2, 3)], [1, 2, 3])

    def test_warm_without_a_cache_counts_the_first_run(self):
        self.measure(scenario="analyze", temp="warm")
        self.assertEqual(len(self.analyze_launches()), 3)
        self.assertFalse(os.path.exists(os.path.join(self.series_dir("analyze"), "warmup.json")))
        self.assertTrue(self.record("analyze", "r1")["held"])
        cmap = perf_nightly.read_json(os.path.join(self.data.results({"label": "fake", "sha": "a" * 40}), "cachemap.json"))
        self.assertEqual((cmap["crm"]["analyze"]["cache"], cmap["crm"]["analyze"]["snapshot"]), ([], []))
        out = self.measure(scenario="analyze", temp="cold")
        self.assertIn("reason=no_cache", out)
        self.assertEqual(len(self.analyze_launches()), 3, "an empty map means no cold runs")

    def test_cache_map_holds_created_cache_leaves_and_snapshot_paths(self):
        set_mode("cache", "build")
        self.measure(scenario="analyze", temp="warm")
        cmap = perf_nightly.read_json(os.path.join(os.path.dirname(os.path.dirname(
            os.path.dirname(os.path.dirname(self.series_dir("analyze"))))), "cachemap.json"))
        entry = cmap["crm"]["analyze"]
        self.assertEqual(entry["cache"], ["bsl-analyzer/workspaces/v1/x"])
        self.assertEqual(entry["snapshot"], [".build"])

    def test_cold_removes_exactly_the_map_and_strips_bsl_variables(self):
        set_mode("cache", "build")
        self.measure(scenario="analyze", temp="warm")
        for name in ("BSL_CACHE_DIR", "BSL_MCP_BROKER", "BSL_PLATFORM_HELP_CACHE_DIR", "BSL_PLATFORM_PATH",
                     "BSL_LOG", "BSL_MCP_TASKS", "BSL_LSP_WORKERS", "BSL_UNKNOWN_X"):
            os.environ[name] = "1"
            self.addCleanup(os.environ.pop, name, None)
        self.measure(scenario="analyze", temp="cold")
        cold = [l for l in self.launches() if l["argv"][:1] == ["analyze"]][-3:]
        self.assertEqual(len({l["cache"] for l in cold}), 1)
        self.assertEqual([l["had"]["bsl-analyzer/workspaces/v1/x"] for l in cold], [False, False, False])
        self.assertEqual([l["had"]["keep"] for l in cold], [False, True, True], "paths outside the map stay")
        self.assertEqual([l["had_build"] for l in cold], [False, False, False])
        for launch in cold:
            self.assertEqual([k for k in launch["env"] if k.startswith("BSL_")], [])
        r1 = self.record("analyze", "r1", temp="cold")
        self.assertEqual(r1["cachemap_check"], {"missing": [], "extra": []})

    def test_a_first_run_that_did_not_hold_takes_no_map(self):
        set_mode("cache", "exit2")
        self.measure(scenario="analyze", temp="warm")
        self.assertIn("cachemap_not_taken", " ".join(self.record("analyze", "r1")["errors"]))
        with self.assertRaisesRegex(HarnessError, "cachemap_missing"):
            self.measure(scenario="analyze", temp="cold")

    def test_a_cache_that_survives_the_reset_is_refused(self):
        xdg = os.path.join(self.data_dir, "xdg", "fake-aaaaaaaaaaaa", "crm", "analyze-warm-t")
        locked = os.path.join(xdg, "cache", "bsl-analyzer")
        os.makedirs(locked)
        open(os.path.join(locked, "db"), "w").close()
        os.chmod(locked, 0o500)
        try:
            with self.assertRaisesRegex(HarnessError, "reset_failed"):
                self.measure(scenario="analyze", temp="warm")
        finally:
            os.chmod(locked, 0o700)
        self.assertEqual(len(self.analyze_launches()), 0)

    def test_a_build_directory_that_survives_reset_is_refused(self):
        from unittest.mock import patch
        os.makedirs(os.path.join(self.snap, ".build"))
        with patch.object(perf_nightly, "_remove"), self.assertRaisesRegex(HarnessError, "reset_failed"):
            self.measure(scenario="analyze", temp="warm")
        self.assertEqual(self.analyze_launches(), [])

    def test_cold_needs_the_warm_map_first(self):
        with self.assertRaises(HarnessError):
            self.measure(scenario="analyze", temp="cold")


class Foreign(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def test_a_foreign_process_before_the_first_run_cancels_everything(self):
        foreign = subprocess.Popen([f"{FOREIGN_PREFIX}-app", "-c", "import signal\nprint(flush=True)\nsignal.pause()"],
                                   executable=sys.executable, stdout=subprocess.PIPE)
        foreign.stdout.readline()
        try:
            self.measure(scenario="analyze", temp="warm")
        finally:
            foreign.kill()
            foreign.wait()
            foreign.stdout.close()
        for k in (1, 2, 3):
            record = self.record("analyze", f"r{k}")
            self.assertEqual((record["held"], record["reason"]), (False, "foreign_process"))
            self.assertIn(str(foreign.pid), record["errors"][0])
        self.assertEqual([l for l in self.launches() if "BSL_LOG" not in l["env"]], [])

    def test_nothing_is_deleted_while_a_foreign_process_runs(self):
        os.makedirs(os.path.join(self.snap, ".build"))
        foreign = subprocess.Popen([f"{FOREIGN_PREFIX}-app", "-c", "import signal\nprint(flush=True)\nsignal.pause()"],
                                   executable=sys.executable, stdout=subprocess.PIPE)
        foreign.stdout.readline()
        try:
            self.measure(scenario="analyze", temp="warm")
        finally:
            foreign.kill()
            foreign.wait()
            foreign.stdout.close()
        self.assertTrue(os.path.isdir(os.path.join(self.snap, ".build")))

    def test_a_foreign_process_before_run_two_preserves_run_one(self):
        from unittest.mock import patch
        with patch.object(procs, "foreign", side_effect=[[], [], [], [{"pid": 999999, "name": "bsl-analyzer-app"}]]) as foreign:
            self.measure(scenario="analyze", temp="warm")
        self.assertEqual(foreign.call_count, 4)
        self.assertEqual([self.record("analyze", f"r{k}")["reason"] for k in (1, 2, 3)],
                         [None, "foreign_process", "foreign_process"])
        self.assertEqual(len([l for l in self.launches() if "BSL_LOG" not in l["env"]]), 1)

    def test_a_foreign_process_seen_after_run_two_cancels_it_and_the_rest(self):
        set_mode("foreign2")
        try:
            self.measure(scenario="analyze", temp="warm")
        finally:
            with open(os.path.join(self.log, "foreign.pid")) as fh:
                os.kill(int(fh.read()), signal.SIGKILL)
        reasons = [self.record("analyze", f"r{k}")["reason"] for k in (1, 2, 3)]
        self.assertEqual(reasons, [None, "foreign_process", "foreign_process"])
        self.assertEqual(len([l for l in self.launches() if l["argv"][:1] == ["analyze"]
                              and "BSL_LOG" not in l["env"]]), 2)


class Refusals(Workspace):
    def test_failed_helpcheck_marks_every_run_without_launching(self):
        set_mode("help_noline")
        self.assertEqual(self.helpcheck(), 1)
        set_mode()
        self.measure(scenario="analyze", temp="warm")
        self.assertEqual([self.record("analyze", f"r{k}")["reason"] for k in (1, 2, 3)],
                         ["help_unavailable"] * 3)
        self.assertEqual(len(self.launches()), 1, "only the help check ran the binary")

    def test_missing_helpcheck_is_a_refusal(self):
        with self.assertRaisesRegex(HarnessError, "helpcheck"):
            self.measure(scenario="analyze", temp="warm")

    def test_a_missing_q_point_fails_lsp_only(self):
        self.helpcheck()
        self.write_points(drop=("workspace_symbol/scan_common_large/01",))
        for scenario in ("lsp-session", "lsp-workspace-diag"):
            self.measure(scenario=scenario, temp="warm", runs=1)
            self.assertEqual(self.record(scenario, "r1")["reason"], "precondition_missing")
        self.assertEqual(len(self.launches()), 1)
        self.measure(scenario="mcp-memory", temp="warm", runs=1)
        self.assertTrue(self.record("mcp-memory", "r1")["held"], "MCP no longer needs Q")

    def test_a_missing_f_point_fails_mcp_without_launching(self):
        self.helpcheck()
        self.write_points(drop=("document_symbol/common_large/01",))
        for scenario in ("mcp-memory", "mcp-start", "mcp-load"):
            self.measure(scenario=scenario, temp="warm", runs=1, clients=[2])
            directory = "mcp-load-c2" if scenario == "mcp-load" else scenario
            self.assertEqual(self.record(directory, "r1")["reason"], "precondition_missing")
        self.assertEqual(len(self.launches()), 1)

    def test_a_distorted_point_file_is_refused_before_launch(self):
        self.helpcheck()
        chosen = corpus.select_points(corpus.load_points(self.data, "crm")[0])
        with open(os.path.join(self.snap, chosen["F"]["relative_path"]), "ab") as fh:
            fh.write(b" ")
        with self.assertRaisesRegex(HarnessError, "points_mismatch"):
            self.measure(scenario="lsp-session", temp="warm")
        with self.assertRaisesRegex(HarnessError, "snapshot_mismatch"):
            self.measure(scenario="analyze", temp="warm")
        self.assertEqual(len(self.launches()), 1)

    def test_a_second_measure_on_the_same_data_dir_is_refused(self):
        self.helpcheck()
        with open(self.data.path("measure.lock"), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with self.assertRaisesRegex(HarnessError, "data_busy"):
                self.measure(scenario="analyze", temp="warm")

    def test_discover_that_never_exits_is_refused(self):
        with self.assertRaisesRegex(HarnessError, "discover_timeout"):
            perf_nightly.cmd_points(self.args(), FakeClock(stuck=lambda pid: True))

    def test_a_broken_manifest_is_a_refusal_not_a_traceback(self):
        with open(self.manifest_path, "a") as fh:
            fh.write("[platform_help\n")
        code = perf_nightly.main(["snapshot", "--data-dir", self.data_dir, "--corpus", "crm"])
        self.assertEqual(code, 2)

    def test_data_dir_inside_the_repository_is_refused(self):
        inside = os.path.join(repo_root(), "scripts", "perf-nightly", "no-such-data")
        code = perf_nightly.main(["report", "--data-dir", inside, "--corpus", "crm"])
        self.assertEqual(code, 2)
        self.assertFalse(os.path.exists(inside))


class TrackedSetIsEmptyAfterEachRun(Workspace):
    def test_daemons_left_by_the_server_are_gone_after_the_run(self):
        self.helpcheck()
        set_mode("daemon")
        self.measure(scenario="mcp-memory", temp="warm", runs=1)
        record = self.record("mcp-memory", "r1")
        with open(os.path.join(self.log, "daemon.pid")) as fh:
            daemon = int(fh.read())
        self.assertIn(daemon, record["stop"]["first"]["terminated"])
        self.assertEqual(record["stop"]["final"]["left"], [])
        self.assertFalse(os.path.exists(f"/proc/{daemon}") and procs.tracked(record["run_id"]))


if __name__ == "__main__":
    unittest.main()
