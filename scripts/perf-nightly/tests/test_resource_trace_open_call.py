"""A call that the server cuts short leaves a boundary of the resource series
missing: the series has to say so. Clocks are virtual; nothing waits."""

import unittest
from unittest.mock import patch

from helpers import set_mode
from test_resource_trace import MODES, TracedGate

import scenarios


class CallCutShort(TracedGate):
    def test_a_call_without_its_end_is_not_left_open_in_a_complete_trace(self):
        # The daemon log is whole, so the only thing the series can lose is the end of the call.
        set_mode(*MODES.split(","), "daemon_log")
        tool = scenarios.McpRun.tool

        def closed_mid_call(mcp_run, name, arguments, timeout=scenarios.CALL_TIMEOUT_S):
            if name == "diagnostics" and arguments.get("action") == "file":
                raise scenarios.NotHeld("server_failed", "connection closed during tools/call")
            return tool(mcp_run, name, arguments, timeout)

        self.enterContext(patch.object(scenarios.McpRun, "tool", closed_mid_call))
        record, got = self.traced_run()

        self.assertEqual((record["held"], record["reason"]), (False, "server_failed"))
        events = [g for g in got if g["kind"] == "event"]
        starts = [g["call_id"] for g in events if g["name"] == "call_start"]
        self.assertEqual(starts[-1], "diagnostics_file")
        ends = [g for g in events if g["name"] == "call_end" and g["call_id"] == "diagnostics_file"]
        # Either the call is ended, as a failed one, or the series is partial and names the call it lost.
        if ends:
            self.assertEqual([g["ok"] for g in ends], [False])
        else:
            self.assertTrue(any("diagnostics_file" in e for e in record["resource_trace"]["errors"]),
                            record["resource_trace"])
            self.assertEqual(record["resource_trace"]["status"], "partial")
            self.assertEqual(got[-1]["status"], "partial")


if __name__ == "__main__":
    unittest.main()
