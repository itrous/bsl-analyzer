"""Positive controls of slice 2 on the fake binary: memory ballast before and
after readiness, and the latency relay in front of the server."""

import json
import os

from helpers import Workspace

import procs

MIB = 1024
BALLAST = 64


class Controls(Workspace):
    def setUp(self):
        super().setUp()
        self.helpcheck()

    def runs(self, scenario, series):
        return [self.record_of(scenario, series, k) for k in (1, 2, 3)]

    def record_of(self, scenario, series, k):
        with open(os.path.join(self.series_dir(scenario, series=series), f"r{k}.json"), encoding="utf-8") as fh:
            return json.load(fh)

    def test_ballast_counts_in_the_ready_memory_of_the_run(self):
        self.measure(scenario="lsp-session", temp="warm", series="plain")
        self.measure(scenario="lsp-session", temp="warm", series="ballast", ballast_mb=BALLAST)
        plain = [r["metrics"]["rss_ready_kb"] for r in self.runs("lsp-session", "plain")]
        loaded = [r["metrics"]["rss_ready_kb"] for r in self.runs("lsp-session", "ballast")]
        self.assertTrue(all(r["held"] for r in self.runs("lsp-session", "ballast")))
        self.assertGreater(min(loaded) - max(plain), (BALLAST - 4) * MIB)
        self.assertEqual(self.record_of("lsp-session", "ballast", 1)["controls"], {"ballast_mb": BALLAST})
        self.assertEqual(procs.tracked(self.record_of("lsp-session", "ballast", 3)["run_id"]), [])

    def test_ballast_after_ready_is_growth_not_ready_memory(self):
        self.measure(scenario="mcp-memory", temp="warm", series="growth", ballast_after_ready_mb=BALLAST)
        for record in self.runs("mcp-memory", "growth"):
            self.assertTrue(record["held"])
            self.assertLess(record["metrics"]["rss_ready_kb"], (BALLAST - 4) * MIB)
            self.assertGreater(record["metrics"]["rss_growth_kb"], (BALLAST - 4) * MIB)

    def test_the_latency_relay_keeps_both_protocols_intact(self):
        self.measure(scenario="lsp-session", temp="warm", series="relay", inject_latency_ms=1)
        self.measure(scenario="mcp-memory", temp="warm", series="relay", inject_latency_ms=1)
        for scenario in ("lsp-session", "mcp-memory"):
            for record in self.runs(scenario, "relay"):
                self.assertTrue(record["held"], record["errors"])
                self.assertTrue(all(c["ok"] for c in record["calls"] if not c.get("na")))
                self.assertEqual(record["controls"], {"inject_latency_ms": 1})
        servers = [l for l in self.launches() if l["argv"][:1] == ["lsp"]]
        self.assertTrue(servers)

    def test_only_controls_that_took_effect_are_recorded(self):
        # analyze has no server to relay, lsp-workspace-diag no ready hook for the ballast.
        self.measure(scenario="analyze", temp="warm", series="relay", inject_latency_ms=1)
        self.measure(scenario="lsp-workspace-diag", temp="warm", series="late", ballast_after_ready_mb=BALLAST)
        self.assertIsNone(self.record_of("analyze", "relay", 1)["controls"])
        self.assertIsNone(self.record_of("lsp-workspace-diag", "late", 1)["controls"])
