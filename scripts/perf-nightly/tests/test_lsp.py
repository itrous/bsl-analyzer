"""`lsp-session` and `lsp-workspace-diag` on the fake server (plan §8.2)."""

import os
import unittest

from helpers import FakeClock, Workspace, set_mode

CALLS = ["L5", "L5s", "L1", "L2", "L3", "L4", "L6", "G1", "G2", "G3", "G4"]


class LspSession(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def run_session(self, *switches, clock=None):
        set_mode(*switches)
        self.measure(clock=clock, scenario="lsp-session", temp="warm", runs=1)
        record = self.record("lsp-session", "r1")
        return record, {c["id"]: c for c in record["calls"]}

    def test_every_call_is_measured_and_the_encoded_uri_counts(self):
        record, calls = self.run_session()
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(list(calls), CALLS)
        self.assertTrue(all(c["ok"] for c in calls.values()), calls)
        self.assertEqual(calls["L5"]["count"], 1)
        self.assertEqual(calls["L5"]["publish_s"], calls["L5"]["first_s"])
        self.assertEqual((calls["L1"]["repeats"], calls["G1"]["repeats"], calls["G2"]["repeats"]), (4, 2, 0))
        for key in ("init_s", "ready_s", "rss_ready_kb", "cpu_start_s", "rss_after_global_kb"):
            self.assertIn(key, record["metrics"])
        self.assertEqual(record["exit_code"], 0)
        self.assertFalse(record["shutdown_timeout"])
        self.assertEqual(record["stop"]["final"]["left"], [])

    def test_a_missing_optional_point_is_na_in_a_held_run(self):
        self.write_points(drop=("hover/common_large/01",))
        record, calls = self.run_session()
        self.assertTrue(record["held"], record["errors"])
        self.assertTrue(calls["L2"]["na"])
        self.assertIsNone(calls["L2"]["ok"])
        self.assertTrue(calls["L3"]["ok"])

    def test_empty_hover_is_a_failed_call_in_a_held_run(self):
        record, calls = self.run_session("hover_null")
        self.assertTrue(record["held"])
        self.assertEqual((calls["L2"]["ok"], calls["L2"]["error"], calls["L2"]["first_s"]),
                         (False, "empty result", None))
        self.assertTrue(calls["L3"]["ok"])

    def test_no_symbols_within_the_limit_is_ready_timeout(self):
        clock = FakeClock()
        record, _ = self.run_session("never_ready", clock=clock)
        self.assertEqual((record["held"], record["reason"]), (False, "ready_timeout"))
        self.assertGreaterEqual(clock.now(), 1200)
        self.assertEqual(set(clock.sleeps), {0.5})

    def test_publish_for_another_uri_times_out_after_exactly_sixty_seconds(self):
        clock = FakeClock()
        record, calls = self.run_session("foreign_publish", clock=clock)
        self.assertTrue(record["held"])
        self.assertEqual((calls["L5"]["ok"], calls["L5"]["error"]), (False, "timeout"))
        timeouts = [(key[0], timeout) for key, timeout, how in clock.waits if how == "timeout"]
        self.assertEqual(timeouts, [("lsp-publish", 60.0), ("lsp-publish", 60.0)])

    def test_a_server_that_ignores_exit_is_taken_down(self):
        def stuck(pid):
            return any(l["pid"] == pid and l["argv"][:1] == ["lsp"] for l in self.launches())

        record, _ = self.run_session("noexit", clock=FakeClock(stuck=stuck))
        self.assertTrue(record["shutdown_timeout"])
        self.assertTrue(record["stop"]["first"]["terminated"])
        self.assertEqual(record["stop"]["final"]["left"], [])

    def test_a_term_ignoring_daemon_is_killed(self):
        def stuck(pid):
            path = os.path.join(self.log, "daemon.pid")
            if not os.path.exists(path):
                return False
            with open(path) as fh:
                return int(fh.read()) == pid

        record, _ = self.run_session("stubborn", clock=FakeClock(stuck=stuck))
        with open(os.path.join(self.log, "daemon.pid")) as fh:
            daemon = int(fh.read())
        self.assertEqual(record["stop"]["first"]["killed"], [daemon])
        self.assertEqual(record["stop"]["final"]["left"], [])

    def test_help_warning_in_stderr_fails_the_run(self):
        record, _ = self.run_session("help_warn_lsp")
        self.assertEqual(record["reason"], "help_degraded")


class PointText(Workspace):
    def test_a_point_file_that_is_not_utf8_does_not_hold(self):
        import scenarios
        path = os.path.join(self.tmp, "cp1251.bsl")
        with open(path, "wb") as fh:
            fh.write("Процедура".encode("cp1251"))
        ctx = scenarios.Ctx(binary="", root=self.tmp, snap=self.tmp, env={}, run_dir=self.tmp, run_id="x",
                            clock=FakeClock(), points={}, snap_meta={})
        with self.assertRaisesRegex(scenarios.NotHeld, "point_unreadable"):
            scenarios.read_point_text(ctx, {"id": "p", "relative_path": "cp1251.bsl"})


class WorkspaceDiag(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def test_items_are_counted_over_partial_and_final_reports(self):
        self.measure(scenario="lsp-workspace-diag", temp="warm", runs=1)
        record = self.record("lsp-workspace-diag", "r1")
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(record["metrics"]["wsdiag_items"], 2)
        launch = [l for l in self.launches() if l["argv"][:1] == ["lsp"]][0]
        self.assertEqual(launch["argv"], ["lsp"])
        self.assertEqual(launch["cwd"], self.data.path("shadow", "crm"))

    def test_without_a_diagnostic_provider_the_call_fails(self):
        set_mode("no_pull")
        self.measure(scenario="lsp-workspace-diag", temp="warm", runs=1)
        record = self.record("lsp-workspace-diag", "r1")
        self.assertEqual(record["calls"][0]["ok"], False)


if __name__ == "__main__":
    unittest.main()
