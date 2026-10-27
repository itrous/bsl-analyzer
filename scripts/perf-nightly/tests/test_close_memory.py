"""`lsp-close-memory` on the fake server: three open-work-close cycles in one
session, memory sampled on the virtual clock after each close, and the growth
from cycle to cycle judged against the absolute limit."""

import json
import os
import threading
import unittest
from unittest.mock import patch

from helpers import FakeClock, Workspace, set_mode

import procs
import scenarios
import verdict
from common import HarnessError

KEY = ("crm", "lsp-close-memory", "warm")


class CloseMemory(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def run_cycles(self, *switches, clock=None):
        set_mode(*switches)
        self.measure(scenario="lsp-session", temp="warm", runs=1)
        clock = clock or FakeClock()
        out = self.measure(clock=clock, scenario="lsp-close-memory", temp="warm", runs=1)
        return self.record("lsp-close-memory", "r1"), out

    def documents(self):
        with open(os.path.join(self.log, "documents.jsonl"), encoding="utf-8") as fh:
            return [json.loads(line) for line in fh]

    def test_three_cycles_open_and_close_every_module_and_sample_two_minutes(self):
        clock = FakeClock()
        record, out = self.run_cycles(clock=clock)
        self.assertTrue(record["held"], record["errors"])
        cycles = record["metrics"]["cycles"]
        self.assertEqual([c["cycle"] for c in cycles], [1, 2, 3])
        self.assertTrue(all(len(c["rss_closed_samples_kb"]) == 5 for c in cycles))
        self.assertTrue(all(c["rss_closed_kb"] == c["rss_closed_samples_kb"][-1] for c in cycles))
        self.assertTrue(all(c["open_s"] is not None for c in cycles))
        self.assertEqual([s for s in clock.sleeps if s != scenarios.LSP_POLL_S], [30.0] * 12)
        self.assertEqual(record["metrics"]["documents"], 2)
        # The lsp-session run before it opened F and F_small once each.
        methods = [d["method"] for d in self.documents()][2:]
        self.assertEqual(methods, (["textDocument/didOpen"] * 2 + ["textDocument/didClose"] * 2) * 3)
        self.assertEqual([c["id"] for c in record["calls"]], ["C1", "C2", "C3"])
        self.assertTrue(all(c["ok"] for c in record["calls"]), record["calls"])
        self.assertEqual(record["metrics"]["rss_growth_max_kb"],
                         max(c["growth_kb"] for c in cycles[1:]))
        for cycle in cycles:
            self.assertIn(f"cycle={cycle['cycle']} open_s=", out)
            self.assertIn(f"rss_closed_kb={cycle['rss_closed_kb']} growth_kb={cycle['growth_kb']}", out)

    def test_memory_kept_after_every_close_is_an_issue_and_none_is_ok(self):
        # Positive control: the leaking server keeps 40 MiB per opened module, 80 MiB a cycle.
        leaking, _ = self.run_cycles("leak")
        self.assertGreater(leaking["metrics"]["rss_growth_max_kb"], verdict.CLOSE_GROWTH_LIMIT_KB)
        rows = verdict.compare({}, {KEY: [leaking]}, None, "no noise.json")
        self.assertEqual([r["outcome"] for r in rows], ["issue"])
        self.assertEqual(verdict.night_verdict(rows)["verdict"], "issue")

    def test_a_server_that_returns_its_memory_is_ok(self):
        record, _ = self.run_cycles()
        self.assertLess(record["metrics"]["rss_growth_max_kb"], verdict.CLOSE_GROWTH_LIMIT_KB)
        rows = verdict.compare({}, {KEY: [record]}, None, "no noise.json")
        self.assertEqual([r["outcome"] for r in rows], ["ok"])

    def test_a_failed_request_makes_the_growth_invalid(self):
        record, _ = self.run_cycles("hover_null")
        self.assertTrue(record["held"])
        self.assertIsNone(record["metrics"]["rss_growth_max_kb"])
        self.assertEqual(record["calls"][0]["error"], "textDocument/hover: empty result")

    def test_it_needs_the_warm_cache_of_lsp_session(self):
        with self.assertRaisesRegex(HarnessError, "group_missing"):
            self.measure(scenario="lsp-close-memory", temp="warm", runs=1)
        with self.assertRaisesRegex(HarnessError, "temp_unsupported"):
            self.measure(scenario="lsp-close-memory", temp="cold", runs=1)

    def test_a_server_gone_in_the_memory_window_does_not_hold(self):
        # From the second sample after the third close on, the run's tracked set is empty.
        real, calls = procs.snapshot, []

        def snapshot(run_id, proc_root="/proc"):
            found = real(run_id, proc_root)
            if threading.current_thread() is threading.main_thread():
                calls.append(1)
                # ready, then per cycle the open memory and five samples
                if len(calls) >= 1 + 6 + 6 + 3:
                    return {**found, "pids": [], "rss_kb": 0, "rss_by_pid": {}}
            return found

        self.measure(scenario="lsp-session", temp="warm", runs=1)
        with patch.object(procs, "snapshot", snapshot):
            self.measure(scenario="lsp-close-memory", temp="warm", runs=1)
        record = self.record("lsp-close-memory", "r1")
        self.assertEqual((record["held"], record["reason"]), (False, "server_failed"))
        self.assertIn("memory window of cycle 3", " ".join(record["errors"]))

    def test_a_lsp_session_without_a_held_run_leaves_no_warm_cache(self):
        set_mode("help_warn_lsp")
        self.measure(scenario="lsp-session", temp="warm", runs=1)
        launches = len(self.launches())
        with self.assertRaisesRegex(HarnessError, "group_missing"):
            self.measure(scenario="lsp-close-memory", temp="warm", runs=1)
        self.assertEqual(len(self.launches()), launches)

    def test_it_shares_the_cache_group_of_lsp_session(self):
        self.run_cycles()
        caches = {l["cache"] for l in self.launches() if l["argv"][:1] == ["lsp"]}
        self.assertEqual(len(caches), 1, caches)


class Cycles(unittest.TestCase):
    def test_the_first_cycle_grows_over_ready_memory_the_others_over_the_previous_close(self):
        self.assertEqual(scenarios.cycle_growth(1000, [5000, 5100, 4900]), [4000, 100, -200])

    def test_the_set_holds_every_file_once(self):
        a, b, c = ({"relative_path": n} for n in ("a", "b", "c"))
        points = {"F_rot": [a, b], "F": a, "F_small": c}
        self.assertEqual(scenarios.close_set(points), [a, b, c])
        self.assertEqual(scenarios.close_set({"F": b}), [b])


if __name__ == "__main__":
    unittest.main()
