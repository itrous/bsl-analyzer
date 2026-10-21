"""MCP scenarios on the fake server (plan §8.3)."""

import gc
import json
import os
import unittest
import warnings

from helpers import FakeClock, Workspace, set_mode

from common import HarnessError

CALLS = ["M1", "M2", "M3", "M5", "M6", "M7"]


class McpMemory(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def run_memory(self, *switches, clock=None):
        set_mode(*switches)
        self.measure(clock=clock, scenario="mcp-memory", temp="warm", runs=1)
        record = self.record("mcp-memory", "r1")
        return record, {c["id"]: c for c in record["calls"]}

    def test_full_server_has_no_na_and_every_call_ok(self):
        record, calls = self.run_memory()
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(list(calls), CALLS)
        self.assertTrue(all(c["ok"] for c in calls.values()), calls)
        self.assertEqual(calls["M1"]["repeats"], 9)
        launches = [l for l in self.launches() if l["argv"][:2] == ["mcp", "serve"]]
        self.assertEqual(launches[0]["argv"], ["mcp", "serve", "--profile", "workspace", "--source-dir", self.snap])
        with open(os.path.join(self.log, "tools.jsonl"), encoding="utf-8") as fh:
            rotation = [json.loads(line)["arguments"]["path"] for line in fh
                        if json.loads(line)["arguments"].get("action") == "file"]
        self.assertEqual(rotation, ["Модули/Большой.bsl", "Модули/Малый.bsl"] * 5)
        for key in ("rss_start_kb", "rss_ready_kb", "rss_series_kb", "rss_growth_kb", "ready_s"):
            self.assertIn(key, record["metrics"])

    def test_a_missing_tool_is_na_not_an_error(self):
        record, calls = self.run_memory("no_graph")
        self.assertTrue(record["held"])
        self.assertTrue(calls["M5"]["na"])
        self.assertIsNone(calls["M5"]["ok"])
        self.assertTrue(all(calls[c]["ok"] for c in CALLS if c != "M5"))

    def test_error_and_not_ready_answers_fail_the_call_only(self):
        record, calls = self.run_memory("iserror_m1", "loading_m6", "status_loading_m5")
        self.assertTrue(record["held"])
        for call_id in ("M1", "M5", "M6"):
            self.assertFalse(calls[call_id]["ok"], call_id)
            self.assertIsNone(calls[call_id]["first_s"], call_id)
        self.assertTrue(calls["M2"]["ok"])

    def test_failed_diagnostics_backend_ends_the_run_without_waiting(self):
        clock = FakeClock()
        record, _ = self.run_memory("diag_failed", clock=clock)
        self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))
        self.assertEqual(clock.sleeps, [])

    def test_a_failed_or_disabled_graph_ends_the_run_without_waiting(self):
        for switch in ("graph_failed", "graph_disabled"):
            with self.subTest(switch=switch):
                clock = FakeClock()
                record, _ = self.run_memory(switch, clock=clock)
                self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))
                self.assertIn(switch.split("_")[1], " ".join(record["errors"]))
                self.assertEqual(clock.sleeps, [])
                os.rename(self.series_dir("mcp-memory"), self.series_dir("mcp-memory") + f"-{switch}")

    def test_a_busy_graph_is_awaited_not_failed(self):
        clock = FakeClock()
        record, calls = self.run_memory("graph_busy", clock=clock)
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(clock.sleeps, [1.0, 1.0])
        self.assertTrue(calls["M5"]["ok"])

    def test_busy_without_a_graph_target_is_a_failure(self):
        clock = FakeClock()
        record, _ = self.run_memory("graph_busy_no_target", clock=clock)
        self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))
        self.assertEqual(clock.sleeps, [])

    def test_busy_with_a_graph_reason_code_is_a_failure(self):
        clock = FakeClock()
        record, _ = self.run_memory("graph_busy_failure", clock=clock)
        self.assertEqual((record["held"], record["reason"]), (False, "backend_failed"))
        self.assertEqual(clock.sleeps, [])

    def test_the_client_pipes_are_closed_after_the_run(self):
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always", ResourceWarning)
            record, _ = self.run_memory()
            gc.collect()
        self.assertTrue(record["held"])
        self.assertEqual([str(w.message) for w in caught if issubclass(w.category, ResourceWarning)], [])

    def test_a_field_name_containing_a_marker_is_not_a_refusal(self):
        record, calls = self.run_memory("field_unavailable_m1")
        self.assertTrue(calls["M1"]["ok"], calls["M1"])

    def test_help_warning_from_the_mcp_server_fails_the_run(self):
        record, _ = self.run_memory("help_warn_mcp")
        self.assertEqual((record["held"], record["reason"]), (False, "help_degraded"))

    def test_search_is_neither_awaited_nor_measured(self):
        record, calls = self.run_memory("search_failed")
        self.assertTrue(record["held"], record["errors"])
        self.assertNotIn("M4", calls)
        with open(os.path.join(self.log, "tools.jsonl"), encoding="utf-8") as fh:
            names = {json.loads(line)["name"] for line in fh}
        self.assertNotIn("search", names)

    def test_never_ready_is_ready_timeout(self):
        clock = FakeClock()
        record, _ = self.run_memory("never_ready", clock=clock)
        self.assertEqual((record["held"], record["reason"]), (False, "ready_timeout"))
        self.assertGreaterEqual(clock.now(), 1200)


class ToolControls(unittest.TestCase):
    def test_busy_with_another_target_kind_is_not_pending_graph(self):
        from scenarios import graph_busy
        result = {"structuredContent": {"state": "failed", "error": "graph busy: lease",
                                        "indexing": {"targets": [{"kind": "diagnostics", "reason_code": None}]}}}
        self.assertFalse(graph_busy(result))

    def test_an_unavailable_action_is_not_called(self):
        from scenarios import call_available, tool_actions
        actions = tool_actions([{"name": "graph", "inputSchema": {"properties": {
            "action": {"enum": ["status"]}}}}])
        self.assertTrue(call_available(actions, "graph", "status"))
        self.assertFalse(call_available(actions, "graph", "overview"))

    def test_refusal_words_and_structured_states_fail_but_fields_do_not(self):
        from scenarios import check_tool_result
        for text in ("still building", "not ready", "unavailable"):
            with self.subTest(text=text):
                self.assertEqual(check_tool_result({"content": [{"type": "text", "text": text}]}), text)
        for state in ("loading", "building", "failed"):
            with self.subTest(state=state):
                self.assertIsNotNone(check_tool_result({"structuredContent": {"state": state}}))
        self.assertIsNotNone(check_tool_result({"content": []}))
        self.assertIsNone(check_tool_result({"content": [{"type": "text", "text": "location_unavailable"}]}))


class LateAnswer(unittest.TestCase):
    def test_an_answer_after_the_deadline_is_a_timeout(self):
        import sys
        from mcp_client import McpClient

        class LateClock:
            def now(self):
                return 0.0

            def wait(self, ready, timeout, cond, key=None):
                with cond:
                    client.responses[1] = ({"jsonrpc": "2.0", "id": 1, "result": {}}, 121.0)
                return False

        client = McpClient([sys.executable, "-c", "import sys; sys.stdin.read()"], None, None, None, LateClock())
        try:
            self.assertEqual(client.request("tools/list", {}, 120.0), (None, None, "timeout"))
        finally:
            client.close_stdin()
            client.proc.wait()
            client.proc.stdout.close()


class ProtocolError(unittest.TestCase):
    def test_a_malformed_line_is_named_in_the_timeout(self):
        import sys
        from mcp_client import McpClient

        class UntilNotified:
            def now(self):
                return 0.0

            def wait(self, ready, timeout, cond, key=None):
                with cond:
                    cond.wait_for(lambda: client.notifications)
                return False

        script = ('import sys\nprint("{broken", flush=True)\n'
                  'print(\'{"jsonrpc": "2.0", "method": "x"}\', flush=True)\nsys.stdin.read()')
        client = McpClient([sys.executable, "-c", script], None, None, None, UntilNotified())
        try:
            _, _, error = client.request("tools/list", {}, 120.0)
        finally:
            client.close_stdin()
            client.proc.wait()
            client.proc.stdout.close()
        self.assertTrue(error.startswith("timeout; protocol error: malformed JSON line"), error)


class McpStartAndLoad(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def test_start_and_load_need_the_warm_group_of_mcp_memory(self):
        with self.assertRaisesRegex(HarnessError, "group_missing"):
            self.measure(scenario="mcp-start", temp="warm", runs=1)

    def test_start_runs_in_a_group_whose_memory_runs_failed(self):
        set_mode("diag_failed")
        self.measure(scenario="mcp-memory", temp="warm", runs=1)
        self.assertEqual(self.record("mcp-memory", "r1")["reason"], "backend_failed")
        self.measure(scenario="mcp-start", temp="warm", runs=1)
        self.assertTrue(self.record("mcp-start", "r1")["held"])

    def test_start_measures_initialize_and_tools(self):
        self.measure(scenario="mcp-memory", temp="warm", runs=1)
        self.measure(scenario="mcp-start", temp="warm", runs=1)
        record = self.record("mcp-start", "r1")
        self.assertTrue(record["held"])
        self.assertIn("init_s", record["metrics"])
        self.assertIn("tools_s", record["metrics"])
        self.assertTrue(record["calls"][0]["ok"])

    def test_an_unanswered_call_counts_as_one_timeout_and_one_error(self):
        self.measure(scenario="mcp-memory", temp="warm", runs=1)
        set_mode("silent_m1")
        clock = FakeClock()
        self.measure(clock=clock, scenario="mcp-load", temp="warm", runs=1, clients=[2])
        record = self.record("mcp-load-c2", "r1")
        self.assertTrue(record["held"], record["errors"])
        calls = {c["id"]: c for c in record["calls"]}
        self.assertEqual(record["metrics"]["timeouts"], 1)
        self.assertEqual((calls["M1"]["errors"], calls["M1"]["timeouts"], calls["M1"]["ok"]), (1, 1, False))
        self.assertEqual(calls["M1"]["count"], 10)
        self.assertTrue(calls["M2"]["ok"])
        self.assertEqual([t for _, t, how in clock.waits if how == "timeout"], [120.0])


if __name__ == "__main__":
    unittest.main()
