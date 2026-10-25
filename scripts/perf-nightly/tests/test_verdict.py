"""The verdict rule of plan §8.6 on synthetic series: Δ against noise, the
table of side states, counters, completeness, noise calibration and trend."""

import os
import tempfile
import unittest
from unittest.mock import patch

import helpers  # noqa: F401 — installs the harness import path

import verdict
from common import HARNESS_SCHEMA, write_json

FP = {"harness_schema": HARNESS_SCHEMA, "rustc": "rustc test", "snapshot_commit": "c" * 40}
HELP = {"source": "external", "sha256": "h" * 64}
W = ("crm", "analyze", "warm")


def rec(k, metrics=None, calls=(), held=True, reason=None, fp=FP, help_=HELP, runs=3, started="x"):
    return {"k": k, "runs": runs, "held": held, "reason": reason, "metrics": dict(metrics or {}),
            "calls": [dict(c) for c in calls], "env_fingerprint": fp, "help": help_, "started_at": started}


def series(values, **kw):
    """Three runs whose `metrics` are the given dicts (or one dict thrice)."""
    if isinstance(values, dict):
        values = [values] * 3
    return [rec(k + 1, v, **kw) for k, v in enumerate(values)]


def noise_for(entries: dict) -> dict:
    return {"harness_schema": HARNESS_SCHEMA, "fingerprints": {"crm": FP},
            "entries": {key: {"noise": value} for key, value in entries.items()}}


def outcome(rows, metric, scenario=None):
    return next(r["outcome"] for r in rows if r["metric"] == metric
                and (scenario is None or r["scenario"] == scenario))


class Delta(unittest.TestCase):
    def judge(self, cand_wall, noise):
        base = {W: series({"wall_s": 100.0, "max_rss_kb": 1000})}
        cand = {W: series({"wall_s": cand_wall, "max_rss_kb": 1000})}
        n = noise_for({"crm/analyze/warm/wall_s": noise, "crm/analyze/warm/max_rss_kb": noise})
        return outcome(verdict.compare(base, cand, n, None), "wall_s")

    def test_thresholds_against_noise(self):
        # И14: +5% ok, +10% issue, +15% issue, +45% block at noise 0.08; +15% ok at noise 0.20.
        self.assertEqual([self.judge(v, 0.08) for v in (105.0, 110.0, 115.0, 145.0)],
                         ["ok", "issue", "issue", "block"])
        self.assertEqual(self.judge(115.0, 0.20), "ok")

    def test_improvement_is_only_noted(self):
        self.assertEqual(self.judge(80.0, 0.08), "improved")

    def test_noisy_metric_is_out_of_the_verdict(self):
        # И16: noise above 40% keeps even a +45% change out of the verdict.
        self.assertEqual(self.judge(145.0, 0.45), "noisy")
        rows = [{"scenario": "analyze", "corpus": "crm", "outcome": "noisy"},
                {"scenario": "analyze", "corpus": "crm", "outcome": "ok"}]
        self.assertEqual(verdict.night_verdict(rows)["verdict"], "ok")
        self.assertEqual(verdict.not_compared(rows), 1)

    def test_cpu_never_enters_the_verdict(self):
        # И15: cpu_s +300% with everything else equal.
        base = {W: series({"wall_s": 100.0, "max_rss_kb": 1000, "cpu_s": 10.0})}
        cand = {W: series({"wall_s": 100.0, "max_rss_kb": 1000, "cpu_s": 40.0})}
        n = noise_for({"crm/analyze/warm/wall_s": 0.05, "crm/analyze/warm/max_rss_kb": 0.05})
        rows = verdict.compare(base, cand, n, None)
        self.assertEqual({r["metric"] for r in rows}, {"wall_s", "max_rss_kb"})
        self.assertEqual(verdict.night_verdict(rows)["verdict"], "ok")

    def test_median_of_three_is_compared(self):
        base = {W: series([{"wall_s": 100.0, "max_rss_kb": 1}, {"wall_s": 1.0, "max_rss_kb": 1},
                           {"wall_s": 900.0, "max_rss_kb": 1}])}
        cand = {W: series({"wall_s": 100.0, "max_rss_kb": 1})}
        n = noise_for({"crm/analyze/warm/wall_s": 0.05, "crm/analyze/warm/max_rss_kb": 0.05})
        row = next(r for r in verdict.compare(base, cand, n, None) if r["metric"] == "wall_s")
        self.assertEqual((row["base"]["value"], row["delta"], row["outcome"]), (100.0, 0.0, "ok"))

    def test_growth_is_normalised_by_ready_memory(self):
        # D8: growth 10 -> 500 KiB on 1000 KiB ready memory is +49%, a block.
        key = ("crm", "mcp-memory", "warm")
        base = {key: series({"rss_growth_kb": 10, "rss_ready_kb": 1000})}
        cand = {key: series({"rss_growth_kb": 500, "rss_ready_kb": 1000})}
        n = noise_for({"crm/mcp-memory/warm/rss_growth_kb": 0.01, "crm/mcp-memory/warm/rss_ready_kb": 0.01})
        row = next(r for r in verdict.compare(base, cand, n, None) if r["metric"] == "rss_growth_kb")
        self.assertAlmostEqual(row["delta"], 0.49)
        self.assertEqual(row["outcome"], "block")
        self.assertEqual(outcome(verdict.compare(base, cand, n, None), "rss_ready_kb"), "ok")


LSP = ("crm", "lsp-session", "warm")


def lsp_calls(**over):
    """lsp-session calls; `over` maps a call id to its `ok` (or "na")."""
    calls = []
    for call_id in ("L5", "L5s", "L1", "L2", "L3", "L4", "L6", "G1", "G2", "G3", "G4"):
        ok = over.get(call_id, True)
        if ok == "na":
            calls.append({"id": call_id, "na": True, "ok": None})
            continue
        calls.append({"id": call_id, "ok": ok, "first_s": 1.0 if ok else None,
                      "repeat_s": 0.5 if ok else None, "publish_s": 1.0 if ok else None,
                      "error": None if ok else "empty result"})
    return calls


LSP_METRICS = {"init_s": 1.0, "ready_s": 5.0, "rss_ready_kb": 1000, "rss_after_global_kb": 2000}


class Table(unittest.TestCase):
    def test_invalid_candidate_is_an_issue_and_both_invalid_are_not_compared(self):
        # И18: G2 ok 3/3 at base, ok=false in 1 of 3 at the candidate; L5 invalid on both; L2 by Δ.
        base = {LSP: [rec(1, LSP_METRICS, lsp_calls()), rec(2, LSP_METRICS, lsp_calls(L5=False)),
                      rec(3, LSP_METRICS, lsp_calls())]}
        cand = {LSP: [rec(1, LSP_METRICS, lsp_calls(G2=False, L5=False)),
                      rec(2, LSP_METRICS, lsp_calls(L5=False)), rec(3, LSP_METRICS, lsp_calls(L5=False))]}
        noise = noise_for({f"crm/lsp-session/warm/{m.key}": 0.05 for m in verdict.metrics_of("lsp-session")})
        rows = verdict.compare(base, cand, noise, None)
        g2 = next(r for r in rows if r["metric"] == "G2.first_s")
        self.assertEqual((g2["outcome"], g2["labels"]), ("issue", ["perf", "regression"]))
        l5 = next(r for r in rows if r["metric"] == "L5.publish_s")
        self.assertEqual(l5["outcome"], "both_invalid")
        self.assertEqual(l5["errors"], {"base": "empty result", "candidate": "empty result"})
        self.assertEqual(outcome(rows, "L2.repeat_s"), "ok")

    def test_failed_candidate_is_a_release_block(self):
        # И55: the candidate's lsp-session never got ready while the base held.
        base = {LSP: series(LSP_METRICS, calls=lsp_calls())}
        cand = {LSP: series({}, held=False, reason="ready_timeout")}
        rows = verdict.compare(base, cand, noise_for({}), None)
        self.assertEqual({r["outcome"] for r in rows}, {"block"})
        self.assertIn("release-blocker", rows[0]["labels"])
        self.assertEqual(verdict.night_verdict(rows)["verdict"], "block")

    def test_one_failed_analyze_run_blocks_its_metrics(self):
        base = {W: series({"wall_s": 100.0, "max_rss_kb": 1})}
        cand = {W: [rec(1, {"wall_s": 100.0, "max_rss_kb": 1}), rec(2, {}, held=False, reason="scope_mismatch"),
                    rec(3, {"wall_s": 100.0, "max_rss_kb": 1})]}
        self.assertEqual(outcome(verdict.compare(base, cand, noise_for({}), None), "wall_s"), "block")

    def test_failures_on_both_sides_are_not_compared(self):
        both = verdict.compare({LSP: series({}, held=False, reason="ready_timeout")},
                               {LSP: series({}, held=False, reason="ready_timeout")}, noise_for({}), None)
        self.assertEqual({r["outcome"] for r in both}, {"both_invalid"})
        mixed = verdict.compare({LSP: series({}, held=False, reason="backend_failed")},
                                {LSP: series(LSP_METRICS, calls=lsp_calls(G2=False))}, noise_for({}), None)
        self.assertEqual(outcome(mixed, "G2.first_s"), "both_invalid")
        self.assertEqual(outcome(mixed, "init_s"), "fixed")

    def test_missing_and_na(self):
        base = {LSP: series(LSP_METRICS, calls=lsp_calls(L6="na"))}
        cand = {LSP: [rec(1, LSP_METRICS, lsp_calls(L6="na")),
                      rec(2, {}, held=False, reason="window_exhausted"), rec(3, {}, held=False,
                                                                           reason="window_exhausted")]}
        rows = verdict.compare(base, cand, noise_for({}), None)
        self.assertEqual(outcome(rows, "init_s"), "incomplete")
        self.assertEqual(outcome(rows, "L6.first_s"), "n/a", "a call the version lacks stays out")
        ok = verdict.compare(base, base, noise_for({f"crm/lsp-session/warm/{m.key}": 0.05
                                                    for m in verdict.metrics_of("lsp-session")}), None)
        self.assertEqual(outcome(ok, "L6.first_s"), "n/a")

    def test_runs_left_unstarted_by_a_surviving_process_are_missing_not_a_failure(self):
        # The harness's own survivor must not read as the version's failure: `ready_timeout`
        # on the same series is a release blocker, `survivor_running` only an incomplete night.
        base = {LSP: series(LSP_METRICS, calls=lsp_calls())}
        for reason, expected in (("survivor_running", "incomplete"), ("ready_timeout", "block")):
            with self.subTest(reason=reason):
                rows = verdict.compare(base, {LSP: series({}, held=False, reason=reason)}, noise_for({}), None)
                self.assertEqual({r["outcome"] for r in rows}, {expected})
                self.assertEqual(verdict.night_verdict(rows)["verdict"], expected)


LOAD = ("crm", "mcp-load-c4", "warm")


def load_run(k, errors=0, timeouts=0, p95=1.0):
    calls = [{"id": c, "p50_s": 0.5, "p95_s": p95, "errors": errors if c == "M1" else 0,
              "timeouts": 0, "ok": not (errors and c == "M1"), "error": "boom" if errors and c == "M1" else None}
             for c in verdict.CALLS_MCP]
    return rec(k, {"p50_s": 0.5, "p95_s": p95, "timeouts": timeouts}, calls)


class Counters(unittest.TestCase):
    def judge(self, base_runs, cand_runs, metric):
        noise = noise_for({f"crm/mcp-load-c4/warm/{m.key}": 0.05 for m in verdict.metrics_of("mcp-load")})
        return outcome(verdict.compare({LOAD: base_runs}, {LOAD: cand_runs}, noise, None), metric)

    def test_timeouts_and_errors_of_the_candidate_above_the_base(self):
        # И38: timeouts 0 -> 1; errors M1 0 -> 5 at a smaller p95; errors M1 1 -> 5.
        clean = [load_run(k) for k in (1, 2, 3)]
        self.assertEqual(self.judge(clean, [load_run(1, timeouts=1), load_run(2), load_run(3)], "timeouts"), "issue")
        fast_errors = [load_run(1, errors=5, p95=0.1), load_run(2, p95=0.1), load_run(3, p95=0.1)]
        self.assertEqual(self.judge(clean, fast_errors, "M1.errors"), "issue")
        self.assertEqual(self.judge(clean, fast_errors, "M1.p95_s"), "improved")
        one = [load_run(1, errors=1), load_run(2), load_run(3)]
        five = [load_run(1, errors=5), load_run(2), load_run(3)]
        self.assertEqual(self.judge(one, five, "M1.errors"), "issue")
        self.assertEqual(self.judge(five, one, "M1.errors"), "ok")

    def test_a_stale_calibration_compares_no_counter_either(self):
        # И46: with a stale calibration nothing is compared, counters and completeness included.
        clean = [load_run(k) for k in (1, 2, 3)]
        worse = [load_run(1, timeouts=1, errors=5), load_run(2), load_run(3)]
        old = {**noise_for({}), "harness_schema": HARNESS_SCHEMA - 1}
        rows = verdict.compare({LOAD: clean}, {LOAD: worse}, old, "harness schema changed")
        self.assertEqual({r["outcome"] for r in rows}, {"noise_stale"})
        self.assertEqual(verdict.night_verdict(rows, stale="harness schema changed")["verdict"], "incomplete")

    def test_workspace_diagnostics_completeness(self):
        # И38: 4 980 -> 12 files at a faster answer is an issue; 4 960 is within 99%.
        key = ("crm", "lsp-workspace-diag", "warm")

        def wsdiag(items, seconds):
            return series({"ready_s": 1.0, "rss_peak_kb": 1000},
                          calls=[{"id": "wsdiag", "ok": True, "first_s": seconds, "count": items}])

        noise = noise_for({f"crm/lsp-workspace-diag/warm/{m.key}": 0.05
                           for m in verdict.metrics_of("lsp-workspace-diag")})
        rows = verdict.compare({key: wsdiag(4980, 10.0)}, {key: wsdiag(12, 1.0)}, noise, None)
        self.assertEqual(outcome(rows, "wsdiag.count"), "issue")
        self.assertEqual(outcome(rows, "wsdiag.first_s"), "improved")
        rows = verdict.compare({key: wsdiag(4980, 10.0)}, {key: wsdiag(4960, 10.0)}, noise, None)
        self.assertEqual(outcome(rows, "wsdiag.count"), "ok")


class NightVerdict(unittest.TestCase):
    def rows(self, *outcomes, scenario="analyze"):
        return [{"scenario": scenario, "corpus": "crm", "outcome": o} for o in outcomes]

    def test_order_block_issue_incomplete_ok(self):
        # И14: a night with one incomplete and one issue is an issue; with a block, a block.
        self.assertEqual(verdict.night_verdict(self.rows("incomplete", "issue", "ok"))["verdict"], "issue")
        self.assertEqual(verdict.night_verdict(self.rows("incomplete", "issue", "block"))["verdict"], "block")
        self.assertEqual(verdict.night_verdict(self.rows("ok", "improved"))["verdict"], "ok")

    def test_incomplete_without_compared_metrics(self):
        # И42: one skipped run; separately a pair whose metrics are all "both invalid".
        self.assertEqual(verdict.night_verdict(self.rows("ok", "incomplete"))["verdict"], "incomplete")
        rows = self.rows("ok") + self.rows("both_invalid", "both_invalid", scenario="mcp-memory")
        result = verdict.night_verdict(rows)
        self.assertEqual(result["verdict"], "incomplete")
        self.assertEqual(result["uncompared"], ["mcp-memory/crm"])
        fixed_only = self.rows("ok") + self.rows("fixed", scenario="mcp-memory")
        self.assertEqual(verdict.night_verdict(fixed_only)["verdict"], "incomplete")

    def test_stale_and_missing_diff_are_incomplete(self):
        ok = self.rows("ok")
        self.assertEqual(verdict.night_verdict(ok, stale="harness schema changed")["reasons"], ["noise_stale"])
        self.assertEqual(verdict.night_verdict(ok, diff_missing=["crm"])["verdict"], "incomplete")


class Comparability(unittest.TestCase):
    def test_another_fingerprint_is_not_compared(self):
        # И17: a series measured on another CPU is not compared against this one.
        other = {**FP, "cpu_model": "other cpu"}
        rows = verdict.compare({W: series({"wall_s": 1.0, "max_rss_kb": 1}, fp=other)},
                               {W: series({"wall_s": 9.0, "max_rss_kb": 1})}, noise_for({}), None)
        self.assertEqual({r["outcome"] for r in rows}, {"incomparable"})

    def test_system_updates_keep_series_comparable(self):
        # Kernel, compiler and memory size change with routine updates; they are recorded only.
        updated = {**FP, "kernel": "7.2.11", "rustc": "rustc other", "mem_total": "63 GiB"}
        rows = verdict.compare({W: series({"wall_s": 1.0, "max_rss_kb": 1}, fp=updated)},
                               {W: series({"wall_s": 1.0, "max_rss_kb": 1})},
                               noise_for({"crm/analyze/warm/wall_s": 0.05, "crm/analyze/warm/max_rss_kb": 0.05}), None)
        self.assertEqual({r["outcome"] for r in rows}, {"ok"})
        self.assertIsNone(verdict.noise_stale(noise_for({}), {"crm": updated}))

    def test_a_night_without_its_base_series_is_incomplete(self):
        # И17: the night's comparison takes only that night's series, not a stored one.
        rows = verdict.compare({}, {W: series({"wall_s": 1.0, "max_rss_kb": 1})}, noise_for({}), None, units=[W])
        self.assertEqual({r["outcome"] for r in rows}, {"incomplete"})

    def test_another_help_is_not_compared(self):
        # И52: base with built-in help, candidate with the external snapshot.
        rows = verdict.compare({W: series({"wall_s": 1.0, "max_rss_kb": 1}, help_={"source": "builtin"})},
                               {W: series({"wall_s": 9.0, "max_rss_kb": 1})}, noise_for({}), None)
        self.assertEqual({r["outcome"] for r in rows}, {"help"})

    def test_stale_calibration_compares_nothing(self):
        # И46: noise.json of another harness schema.
        old = {**noise_for({"crm/analyze/warm/wall_s": 0.05}), "harness_schema": HARNESS_SCHEMA - 1}
        stale = verdict.noise_stale(old, {"crm": FP})
        self.assertEqual(stale, "harness schema changed")
        self.assertEqual(verdict.noise_stale(noise_for({}), {"crm": {**FP, "cpu_model": "other"}}),
                         "environment fingerprint of crm changed")
        self.assertIsNone(verdict.noise_stale(noise_for({}), {"crm": FP}))
        rows = verdict.compare({W: series({"wall_s": 1.0, "max_rss_kb": 1})},
                               {W: series({"wall_s": 1.0, "max_rss_kb": 1})}, old, stale)
        self.assertEqual({r["outcome"] for r in rows}, {"noise_stale"})
        result = verdict.night_verdict(rows, stale=stale)
        self.assertEqual((result["verdict"], result["reasons"][-1]), ("incomplete", "noise_stale"))


class Noise(unittest.TestCase):
    def cal(self, wall, growth):
        return {("crm", "mcp-memory", "warm"): series({"rss_growth_kb": growth, "rss_ready_kb": 1000,
                                                       "ready_s": wall}),
                W: series({"wall_s": wall, "max_rss_kb": 1000})}

    def test_spread_over_median_and_growth_over_ready_memory(self):
        # И45: medians 100/102/98/105/95 -> 0.10; growth 10/12/8/15/5 at 1000 KiB -> 0.01.
        groups = [self.cal(w, g) for w, g in zip((100, 102, 98, 105, 95), (10, 12, 8, 15, 5))]
        entries = verdict.build_noise(groups)
        self.assertAlmostEqual(entries["crm/analyze/warm/wall_s"]["noise"], 0.10)
        self.assertAlmostEqual(entries["crm/mcp-memory/warm/rss_growth_kb"]["noise"], 0.01)
        self.assertEqual(entries["crm/analyze/warm/max_rss_kb"]["noise"], 0.0)

    def test_a_metric_not_valid_in_every_series_has_no_noise(self):
        groups = [self.cal(100, 10) for _ in range(4)]
        groups.append({W: series({}, held=False, reason="analyze_exit")})
        entry = verdict.build_noise(groups)["crm/analyze/warm/wall_s"]
        self.assertIsNone(entry["noise"])
        self.assertEqual(entry["states"][-1], "failed")

    def test_self_check_catches_a_comparison_that_ignores_noise(self):
        groups = [self.cal(w, g) for w, g in zip((100, 102, 98, 112, 95), (10, 12, 8, 15, 5))]
        noise = {"entries": verdict.build_noise(groups)}
        self.assertEqual(verdict.self_check(groups, noise)["violations"], [])
        # Positive control: with noise not applied the 112 series becomes an issue.
        with patch.object(verdict, "delta_outcome",
                          side_effect=lambda d, n: "issue" if d >= verdict.ISSUE_DELTA else "ok"):
            self.assertNotEqual(verdict.self_check(groups, noise)["violations"], [])

    def test_uncalibrated_corpus_and_temperature(self):
        # D14: cold of an uncalibrated pair takes the largest cold noise of the
        # metric over other corpora, but not less than this corpus's warm noise.
        noise = {"entries": {"ut/analyze/cold/wall_s": {"noise": 0.05},
                             "erp/analyze/cold/wall_s": {"noise": 0.07},
                             "crm/analyze/warm/wall_s": {"noise": 0.09}}}
        self.assertEqual(verdict.lookup_noise(noise, "crm", "analyze", "cold", "wall_s"), 0.09)
        noise["entries"]["crm/analyze/warm/wall_s"]["noise"] = 0.01
        self.assertEqual(verdict.lookup_noise(noise, "crm", "analyze", "cold", "wall_s"), 0.07)
        self.assertEqual(verdict.lookup_noise(noise, "ut", "analyze", "cold", "max_rss_kb"), None)


class Trend(unittest.TestCase):
    def setUp(self):
        self.tmp = self.enterContext(tempfile.TemporaryDirectory(prefix="perf-nightly-trend-"))

    def write(self, name, series_name, wall, fp=FP, help_=HELP, started="2026-10-21T01:00:00"):
        for record in series({"wall_s": wall, "max_rss_kb": 1}, fp=fp, help_=help_, started=started):
            write_json(os.path.join(self.tmp, name, "crm", "analyze", "warm", series_name,
                                    f"r{record['k']}.json"), record)

    def test_every_measured_release_is_a_column(self):
        # И36: three releases; И52: the built-in help column is marked; И17: another CPU is incomparable.
        self.write("v0.2.86-aaaaaaaaaaaa", "night-2026-10-20", 30.0, help_={"source": "builtin"})
        self.write("v0.3.0-bbbbbbbbbbbb", "night-2026-10-20", 20.0, fp={**FP, "cpu_model": "old cpu"})
        self.write("v0.3.2-cccccccccccc", "cal-1", 11.0, started="2026-10-19T01:00:00")
        self.write("v0.3.2-cccccccccccc", "night-2026-10-21", 10.0, fp={**FP, "kernel": "7.2.9"})
        self.write("v0.3.2-cccccccccccc", "ctl-ballast", 99.0, started="2026-10-22T01:00:00")
        self.write("develop-dddddddddddd", "night-2026-10-21", 9.0)
        cand = verdict.load_series(os.path.join(self.tmp, "develop-dddddddddddd"), "night-2026-10-21")
        table = verdict.trend(self.tmp, cand, [("develop-dddddddddddd", os.path.join(
            self.tmp, "develop-dddddddddddd"), "night-2026-10-21")])
        self.assertEqual(table["columns"], ["v0.2.86-aaaaaaaaaaaa", "v0.3.0-bbbbbbbbbbbb",
                                            "v0.3.2-cccccccccccc", "develop-dddddddddddd"])
        lines = verdict.trend_lines(table)
        self.assertIn("v0.2.86-aaaaaaaaaaaa (справка встроенная)", lines[0])
        row = next(line for line in lines if "| wall_s |" in line)
        self.assertEqual(row, "| crm | analyze | warm | wall_s | 30.000 (справка встроенная) | несравнимо "
                              "| 10.000 | 9.000 |")


if __name__ == "__main__":
    unittest.main()
