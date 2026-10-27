"""`lsp-close-memory` on the fake server: three open-work-close cycles in one
session, memory sampled on the virtual clock after each close, and the growth
from cycle to cycle judged against the absolute limit."""

import json
import os
import threading
import unittest
from unittest.mock import patch

from helpers import FakeClock, Workspace, fake_latency, set_mode

import corpus
import procs
import scenarios
import verdict
from common import HarnessError

KEY = ("crm", "lsp-close-memory", "warm")


def growth(rows):
    return [r["outcome"] for r in rows if r["metric"] == "rss_growth_max_kb"]


def judged(record, metric):
    return next(r["outcome"] for r in verdict.compare({}, {KEY: [record]}, None, "no noise.json")
                if r["metric"] == metric)


def recorded_requests(case):
    """What the client waits for and asks of the server, in order: `("publish", uri)` once the
    wait for the diagnostics of `uri` has ended, `("call", method, params)` as a request goes out."""
    events = []
    wait_publish, call = scenarios.HarnessLsp.wait_publish, scenarios.HarnessLsp.call

    def published(self, uri, after, timeout):
        message = wait_publish(self, uri, after, timeout)
        events.append(("publish", uri))
        return message

    def requested(self, method, params, timeout=scenarios.CALL_TIMEOUT_S):
        events.append(("call", method, params))
        return call(self, method, params, timeout)

    case.enterContext(patch.object(scenarios.HarnessLsp, "wait_publish", published))
    case.enterContext(patch.object(scenarios.HarnessLsp, "call", requested))
    return events


class CloseMemory(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def run_cycles(self, *switches, clock=None, series="t"):
        set_mode(*switches)
        self.measure(scenario="lsp-session", temp="warm", runs=1, series=series)
        clock = clock or FakeClock()
        out = self.measure(clock=clock, scenario="lsp-close-memory", temp="warm", runs=1, series=series)
        with open(os.path.join(self.series_dir("lsp-close-memory", series=series), "r1.json"), encoding="utf-8") as fh:
            return json.load(fh), out

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
        self.assertEqual(growth(rows), ["issue"])
        self.assertEqual(verdict.night_verdict(rows)["verdict"], "issue")

    def test_a_server_that_returns_its_memory_is_ok(self):
        record, _ = self.run_cycles()
        self.assertLess(record["metrics"]["rss_growth_max_kb"], verdict.CLOSE_GROWTH_LIMIT_KB)
        rows = verdict.compare({}, {KEY: [record]}, None, "no noise.json")
        self.assertEqual(growth(rows), ["ok"])

    def test_a_failed_request_makes_the_growth_invalid(self):
        record, _ = self.run_cycles("hover_null")
        self.assertTrue(record["held"])
        self.assertIsNone(record["metrics"]["rss_growth_max_kb"])
        self.assertEqual(record["calls"][0]["error"], "textDocument/hover: empty result")

    def test_memory_over_ready_counts_the_first_cycle_too(self):
        # Two modules a cycle, 40 MiB kept per open: six opens are 240 MiB over ready memory,
        # while the largest growth of one cycle is 80 MiB.
        leaking, _ = self.run_cycles("leak")
        over = leaking["metrics"]["rss_over_ready_max_kb"]
        self.assertGreater(over, 200 * 1024)
        cycles = leaking["metrics"]["cycles"]
        self.assertEqual(over, max(c["rss_closed_kb"] for c in cycles) - leaking["metrics"]["rss_ready_kb"])
        clean, _ = self.run_cycles(series="clean")
        self.assertLess(clean["metrics"]["rss_over_ready_max_kb"], 40 * 1024)
        self.assertEqual(judged(clean, "rss_over_ready_max_kb"), "ok")

    def test_the_global_query_after_reopening_pays_for_what_the_close_evicted(self):
        # The fake server answers the first request after a reopen 0.5 s late: only cycles
        # after a close see it, and only the request sent first in the work phase.
        fake_latency(self)
        evicting, out = self.run_cycles("evict")
        self.assertEqual([c["global_s"] for c in evicting["metrics"]["cycles"]], [0.0, 0.5, 0.5])
        self.assertEqual(evicting["metrics"]["global_reopen_s"], 0.5)
        self.assertEqual(judged(evicting, "global_reopen_s"), "issue")
        self.assertIn("cycle=2 open_s=", out)
        self.assertIn("global_s=0.500 rss_closed_kb=", out)
        clean, _ = self.run_cycles(series="clean")
        self.assertEqual([c["global_s"] for c in clean["metrics"]["cycles"]], [0.0, 0.0, 0.0])
        self.assertEqual(judged(clean, "global_reopen_s"), "ok")

    def test_the_global_query_is_part_of_the_work_time_of_the_cycle(self):
        # Nothing else in the work phase takes virtual time: the slow query is all of it.
        fake_latency(self)
        evicting, _ = self.run_cycles("evict")
        self.assertEqual([c["work_s"] for c in evicting["metrics"]["cycles"]], [0.0, 0.5, 0.5])

    def test_a_failed_cycle_leaves_both_new_values_invalid(self):
        for mode in ("hover_null", "global_empty"):
            with self.subTest(mode=mode):
                record, _ = self.run_cycles(mode, series=mode)
                self.assertTrue(record["held"])
                self.assertIsNone(record["metrics"]["rss_over_ready_max_kb"])
                self.assertIsNone(record["metrics"]["global_reopen_s"])
                self.assertEqual(judged(record, "global_reopen_s"), "incomplete")
        cycles = record["metrics"]["cycles"]
        self.assertEqual([c["global_s"] is None for c in cycles], [False, True, True])
        self.assertEqual([c["error"] for c in record["calls"]],
                         [None, "workspace/symbol: empty result", "workspace/symbol: empty result"])

    def test_the_global_query_is_the_first_request_after_the_diagnostics_of_every_cycle(self):
        events = recorded_requests(self)
        self.run_cycles()
        # lsp-close-memory is the run that initializes last; its cycles start at the first wait
        # for diagnostics, after the readiness polls.
        events = events[[e[:2] for e in events].index(("call", "initialize"), 1):]
        events = events[next(i for i, e in enumerate(events) if e[0] == "publish"):]
        cycles, current = [], []
        for event in events:
            if event[0] == "publish" and current and current[-1][0] == "call":
                cycles.append(current)
                current = []
            current.append(event)
        cycles.append(current)
        points_doc, _ = corpus.load_points(self.data, "crm")
        query = corpus.select_points(points_doc)["Q"]["query"]
        self.assertEqual(len(cycles), 3)
        for number, cycle in enumerate(cycles, 1):
            with self.subTest(cycle=number):
                self.assertEqual([e[0] for e in cycle[:3]], ["publish", "publish", "call"])
                self.assertEqual(cycle[2][1:], ("workspace/symbol", {"query": query}))
                calls = [e[1] for e in cycle if e[0] == "call"]
                self.assertEqual(calls.count("workspace/symbol"), 1)

    def scripted(self, *cycles, series):
        """Run the scenario with cycles that report `(ok, global_s, over_ready_kb)` as given."""
        def close_cycle(ctx, run, docs, number):
            ok, global_s, over_ready_kb = cycles[number - 1]
            closed = ctx.out["metrics"]["rss_ready_kb"] + over_ready_kb
            return {"cycle": number, "ok": ok, "open_s": 1.0, "global_s": global_s, "work_s": 0.0,
                    "rss_open_kb": closed, "rss_closed_samples_kb": [closed], "rss_closed_kb": closed}

        with patch.object(scenarios, "close_cycle", close_cycle):
            record, _ = self.run_cycles(series=series)
        return record["metrics"]

    def test_the_limits_judge_the_worst_cycle_and_the_global_query_only_after_a_close(self):
        mib = 1024
        # Only cycle 1 is far over ready memory, and only its global query is slow: it has no close before it.
        metrics = self.scripted((True, 9.0, 500 * mib), (True, 0.3, 100 * mib), (True, 0.1, 120 * mib), series="a")
        self.assertEqual(metrics["rss_over_ready_max_kb"], 500 * mib)
        self.assertEqual(metrics["global_reopen_s"], 0.3)
        self.assertEqual(metrics["rss_growth_max_kb"], 20 * mib)
        # The worst global query is the first reopen's here, the last one's there.
        metrics = self.scripted((True, 0.0, 10 * mib), (True, 0.1, 10 * mib), (True, 0.3, 10 * mib), series="b")
        self.assertEqual(metrics["global_reopen_s"], 0.3)
        metrics = self.scripted((True, 0.0, 10 * mib), (True, 0.3, 10 * mib), (True, 0.1, 10 * mib), series="c")
        self.assertEqual(metrics["global_reopen_s"], 0.3)

    def test_a_failed_cycle_invalidates_the_values_that_read_it(self):
        mib = 1024
        # Cycle 1 failed: memory over ready reads it, the reopening values of cycles 2 and 3 do not.
        metrics = self.scripted((False, None, 10 * mib), (True, 0.3, 10 * mib), (True, 0.1, 10 * mib), series="first")
        self.assertIsNone(metrics["rss_over_ready_max_kb"])
        self.assertIsNone(metrics["rss_growth_max_kb"])
        self.assertEqual(metrics["global_reopen_s"], 0.3)
        # Cycle 3 failed: nothing that reads it is left valid.
        metrics = self.scripted((True, 0.0, 10 * mib), (True, 0.3, 10 * mib), (False, None, 10 * mib), series="last")
        self.assertIsNone(metrics["rss_over_ready_max_kb"])
        self.assertIsNone(metrics["global_reopen_s"])

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
