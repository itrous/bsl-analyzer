"""The decision rule of the release gate (plan docs/plans/release-gate-graph-ready.md §5.3):
Δ against the bar and the calibrated noise, the machine-load measure, the
verdict and its exit code, and the operating characteristics of the rule."""

import argparse
import contextlib
import io
import math
import os
import random
import statistics
import tempfile
import unittest
from unittest.mock import patch

import helpers  # noqa: F401 — installs the harness import path

import gate
import perf_nightly
import procs
import verdict

DECISIVE = verdict.GATE_DECISIVE
WALL = "graph_ready_s"


def outcome(key, delta, noise, confirm=None):
    return verdict.gate_outcome(key, delta, noise, confirm)


def record(k, **metrics):
    return {"k": k, "runs": 3, "held": True, "reason": None, "metrics": dict(metrics), "calls": []}


def side(*values):
    """Three runs whose four gate metrics are the given values."""
    return [record(k + 1, graph_ready_s=v, graph_ready_cpu_s=v, graph_resolve_s=v, diagnostics_workspace_s=v)
            for k, v in enumerate(values)]


def noise_of(**values):
    return {"entries": {f"erp/mcp-graph/cold/{key}": {"noise": value} for key, value in values.items()}}


class DecisiveMetrics(unittest.TestCase):
    def test_decisive_bar_is_five_percent(self):
        # With a noise of 1 % a rule "Δ above the noise" would block both: 5 % is the bar itself.
        for key in DECISIVE:
            with self.subTest(key=key):
                self.assertEqual(outcome(key, 0.0501, 0.01), "block")
                self.assertEqual(outcome(key, 0.05, 0.01), "ok")
                self.assertEqual(outcome(key, -0.20, 0.01), "ok")

    def test_the_cpu_time_is_not_among_them(self):
        self.assertEqual(DECISIVE, ("graph_resolve_s", "diagnostics_workspace_s"))
        self.assertEqual(verdict.GATE_OBSERVED, ("graph_ready_cpu_s",))
        self.assertNotIn("graph_ready_cpu_s", gate.TRADED_METRICS)
        self.assertIn("graph_ready_cpu_s", gate.METRICS, "it is still measured and calibrated")

    def test_noisy_or_missing_noise_never_passes(self):
        for key in DECISIVE:
            with self.subTest(key=key):
                # A noise above the bar stops the verdict even where "max(5 %, ν)" would say ok.
                self.assertEqual(outcome(key, 0.055, 0.06), "noisy")
                self.assertEqual(outcome(key, 0.30, 0.06), "noisy")
                self.assertEqual(outcome(key, 0.0, 0.06), "noisy")
                self.assertEqual(outcome(key, 0.30, None), "no_noise")
                self.assertEqual(outcome(key, 0.0, None), "no_noise")
                self.assertEqual(outcome(key, 0.05, 0.05), "ok")
        self.assertEqual(outcome(WALL, 0.0, None), "no_noise")
        self.assertEqual(outcome(WALL, 0.30, 0.41), "noisy")
        self.assertEqual(outcome(WALL, 0.30, 0.40), "noise")
        for key, delta, noise in ((DECISIVE[0], 0.055, 0.06), (DECISIVE[1], 0.0, None), (WALL, 0.30, 0.41),
                                  (WALL, 0.0, None)):
            rows = [{"metric": m, "outcome": "ok"} for m in (*DECISIVE, WALL) if m != key]
            rows.append({"metric": key, "outcome": outcome(key, delta, noise)})
            result = verdict.gate_verdict(rows, [])
            self.assertEqual(result["verdict"], "incomplete", (key, delta, noise))
            self.assertIn(rows[-1]["outcome"], result["reasons"][0])


class CpuIsObserved(unittest.TestCase):
    """The CPU time is measured and shown, and decides nothing: neither a growth
    nor a missing noise changes the verdict, with or without a faster wall."""

    NOISE = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=0.01, graph_resolve_s=0.01, diagnostics_workspace_s=0.01)

    def rows(self, wall, cpu, noise=None):
        base = [record(k + 1, graph_ready_s=100.0, graph_ready_cpu_s=40.0) for k in range(3)]
        cand = [record(k + 1, graph_ready_s=100.0 * (1 + wall), graph_ready_cpu_s=40.0 * (1 + cpu))
                for k in range(3)]
        rows = gate.rule_rows(base, cand, noise or self.NOISE, True)
        return {r["metric"]: r for r in rows}

    def test_a_growth_is_shown_with_its_numbers_and_with_no_trade(self):
        for wall, growth in ((-0.45, 0.24), (0.01, 0.24), (0.0, 0.24), (-0.10, 0.30), (-0.45, 0.03), (0.0, -0.5)):
            with self.subTest(wall=wall, growth=growth):
                rows = self.rows(wall, growth)
                cpu = rows["graph_ready_cpu_s"]
                self.assertEqual((cpu["kind"], cpu["outcome"], cpu["limit"], cpu["reason"]),
                                 ("observation", "observation", None, None))
                self.assertAlmostEqual(cpu["delta"], growth)
                self.assertAlmostEqual(cpu["noise"], 0.01)
                self.assertNotIn("basis", cpu)
                self.assertNotIn("wall_delta", cpu)
                self.assertEqual(rows["graph_ready_s"]["outcome"], "ok" if wall <= 0.05 else "block")
                self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "pass")

    def test_a_noise_that_is_missing_or_wide_leaves_the_verdict_as_it_is(self):
        for cpu_noise, reason in ((0.06, None), (None, "no_noise")):
            with self.subTest(noise=cpu_noise):
                noise = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=cpu_noise)
                for cpu_delta in (0.24, 0.03, 0.0):
                    rows = self.rows(-0.45, cpu_delta, noise)
                    self.assertEqual((rows["graph_ready_cpu_s"]["outcome"], rows["graph_ready_cpu_s"]["reason"]),
                                     ("observation", reason))
                    self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "pass")

    def test_the_trade_is_the_same_rule_for_every_work_metric(self):
        # Resolve and sweep: +24 % against a wall at -45 % is paid for in each; the CPU has no trade to be paid for.
        base, cand = side(40.0, 40.0, 40.0), side(49.6, 49.6, 49.6)
        for r in cand:
            r["metrics"]["graph_ready_s"] = 55.0
        for r in base:
            r["metrics"]["graph_ready_s"] = 100.0
        rows = {r["metric"]: r for r in gate.rule_rows(base, cand, self.NOISE, False)}
        for key in DECISIVE:
            with self.subTest(key=key):
                self.assertEqual((rows[key]["outcome"], rows[key]["basis"]), ("ok", "growth_traded"))
                self.assertAlmostEqual(rows[key]["wall_delta"], -0.45)
        self.assertNotIn("basis", rows["graph_ready_s"])
        self.assertNotIn("basis", rows["graph_ready_cpu_s"])


class ResolveAndSweepTradedForWall(unittest.TestCase):
    """More time of `graph_resolve_s` and `diagnostics_workspace_s` is accepted where the wall
    time fell by at least as much; the machine has idle cores, the user waits for the wall."""

    NOISE = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=0.01, graph_resolve_s=0.013, diagnostics_workspace_s=0.01)

    def rows(self, key, wall, growth, noise=None):
        steady = {"graph_ready_cpu_s": 40.0, "graph_resolve_s": 17.7, "diagnostics_workspace_s": 22.1}
        base = [record(k + 1, graph_ready_s=250.0, **steady) for k in range(3)]
        cand = [record(k + 1, graph_ready_s=250.0 * (1 + wall), **{**steady, key: steady[key] * (1 + growth)})
                for k in range(3)]
        noise = noise or self.NOISE
        return {r["metric"]: r for r in gate.rule_rows(base, cand, noise, False)}

    def test_growth_just_over_the_bar_with_a_much_faster_wall_passes_with_a_note(self):
        row = self.rows("graph_resolve_s", -0.45, 0.0509)["graph_resolve_s"]
        self.assertEqual((row["outcome"], row["basis"]), ("ok", "growth_traded"))
        self.assertAlmostEqual(row["delta"], 0.0509)
        self.assertAlmostEqual(row["wall_delta"], -0.45)

    def test_growth_with_a_flat_wall_blocks(self):
        row = self.rows("graph_resolve_s", 0.01, 0.10)["graph_resolve_s"]
        self.assertEqual((row["outcome"], row["basis"]), ("block", "growth_not_repaid"))

    def test_growth_within_the_bar_passes_without_a_note(self):
        row = self.rows("graph_resolve_s", 0.0, 0.03)["graph_resolve_s"]
        self.assertEqual((row["outcome"], row["basis"]), ("ok", None))

    def test_the_sweep_trades_growth_for_a_faster_wall(self):
        row = self.rows("diagnostics_workspace_s", -0.20, 0.08)["diagnostics_workspace_s"]
        self.assertEqual((row["outcome"], row["basis"]), ("ok", "growth_traded"))
        self.assertAlmostEqual(row["wall_delta"], -0.20)

    def test_the_gain_of_the_wall_must_reach_the_growth(self):
        for key in ("graph_resolve_s", "diagnostics_workspace_s"):
            with self.subTest(key=key):
                self.assertEqual(gate.traded_outcome(key, 0.08, -0.08, 0.01), ("ok", "growth_traded"))
                self.assertEqual(gate.traded_outcome(key, 0.08, -0.079, 0.01), ("block", "growth_not_repaid"))
                self.assertEqual(gate.traded_outcome(key, 0.08, None, 0.01), ("block", "growth_not_repaid"))

    def test_a_noisy_metric_never_passes_however_fast_the_wall(self):
        for key in ("graph_resolve_s", "diagnostics_workspace_s"):
            for noise_value, expected in ((0.06, "noisy"), (None, "no_noise")):
                for growth in (0.08, 0.03, 0.0):
                    with self.subTest(key=key, noise=noise_value, growth=growth):
                        rows = self.rows(key, -0.45, growth, noise_of(**{
                            "graph_ready_s": 0.03, "graph_ready_cpu_s": 0.01, "graph_resolve_s": 0.01,
                            "diagnostics_workspace_s": 0.01, key: noise_value}))
                        self.assertEqual(rows[key]["outcome"], expected)
                        self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "incomplete")


class WallMetric(unittest.TestCase):
    def test_wall_within_noise_needs_confirmation(self):
        # The measurement of 2026-10-29: base 332.4 / 412.4 / 364.4, candidate 428.5 / 444.4 / 358.4.
        base = side(332.4, 412.4, 364.4)
        cand = side(428.5, 444.4, 358.4)
        self.assertGreater(428.5, 364.4 * 1.05, "the old rule 'median × 1.05' failed this run")
        rows = gate.rule_rows(base, cand, noise_of(graph_ready_s=0.18, graph_ready_cpu_s=0.01,
                                                   graph_resolve_s=0.01, diagnostics_workspace_s=0.01), False)
        wall = next(r for r in rows if r["metric"] == WALL)
        self.assertAlmostEqual(wall["delta"], 428.5 / 364.4 - 1)
        self.assertAlmostEqual(wall["delta"], 0.176, places=3)
        self.assertEqual(wall["outcome"], "noise")
        self.assertEqual(outcome(WALL, 0.176, 0.18), "noise")
        self.assertEqual(outcome(WALL, 0.20, 0.18), "confirm")
        self.assertEqual(outcome(WALL, 0.20, 0.18, 0.19), "block")
        self.assertEqual(outcome(WALL, 0.20, 0.18, 0.10), "noise")

    def test_wall_limits_are_the_larger_of_the_bar_and_the_noise(self):
        # Δ = noise stays inside it, a hair above needs the confirming round; Δ′ likewise.
        self.assertEqual(outcome(WALL, 0.18, 0.18), "noise")
        self.assertEqual(outcome(WALL, 0.1801, 0.18), "confirm")
        self.assertEqual(outcome(WALL, 0.30, 0.18, 0.18), "noise")
        self.assertEqual(outcome(WALL, 0.30, 0.18, 0.1801), "block")
        self.assertEqual(outcome(WALL, 0.05, 0.18), "ok")
        self.assertEqual(outcome(WALL, 0.0501, 0.18), "noise")
        self.assertEqual(outcome(WALL, 0.0501, 0.01), "block")
        self.assertEqual(outcome(WALL, 0.05, 0.01), "ok")

    def test_wall_decisive_blocks_without_confirmation(self):
        self.assertEqual(outcome(WALL, 0.06, 0.03), "block")
        self.assertEqual(outcome(WALL, 0.06, 0.05), "block")
        # Just above the bar the noise is a noise: the confirming round is back.
        self.assertEqual(outcome(WALL, 0.06, 0.0501), "confirm")

    def test_a_side_without_a_decision_blocks_the_candidate_only(self):
        good, empty = side(1.0, 1.0, 1.0), []
        bad = side(1.0, 1.0, 1.0)
        bad[1].update(held=False, reason="server_failed")
        noise = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=0.01, graph_resolve_s=0.01,
                         diagnostics_workspace_s=0.01)
        for base, cand, expected in ((good, bad, "block"), (bad, good, "incomplete"), (empty, good, "incomplete"),
                                     (good, empty, "incomplete"), (bad, bad, "block"), (empty, empty, "incomplete")):
            with self.subTest(base=len(base), cand=len(cand)):
                rows = gate.rule_rows(base, cand, noise, False)
                self.assertEqual({r["outcome"] for r in rows if r["kind"] != "observation"}, {expected})
                cpu = next(r for r in rows if r["kind"] == "observation")
                self.assertEqual((cpu["metric"], cpu["outcome"], cpu["delta"], cpu["limit"]),
                                 ("graph_ready_cpu_s", "observation", None, None))
                self.assertTrue(cpu["reason"], "the row says why it has no numbers")


    def test_a_series_cut_short_keeps_the_failure_it_already_holds(self):
        # The machine stayed busy after the second run: two of three launches exist.
        failed = side(1.0, 1.0)
        failed[1].update(held=False, reason="server_failed")
        foreign = side(1.0, 1.0)
        foreign[1].update(held=False, reason="foreign_process")
        metric = gate.METRICS["graph_ready_s"]
        self.assertEqual(gate.side(failed, metric), ("failed", None))
        self.assertEqual(gate.side(foreign, metric), ("missing", None))
        self.assertEqual(gate.side(side(1.0, 1.0), metric), ("missing", None))
        self.assertEqual(gate.side(side(1.0, 1.0, 1.0), metric), ("valid", 1.0))
        self.assertEqual(gate.side_outcome("valid", "failed"), "block")
        self.assertEqual(gate.side_outcome("valid", "invalid"), "block")
        self.assertEqual(gate.side_outcome("failed", "valid"), "incomplete")
        self.assertEqual(gate.side_outcome("invalid", "valid"), "incomplete")
        self.assertEqual(gate.side_outcome("valid", "missing"), "incomplete")
        self.assertEqual(gate.side_outcome("valid", "n/a"), "incomplete")
        self.assertIsNone(gate.side_outcome("valid", "valid"))


    def test_the_gate_maps_the_states_of_the_sides_itself(self):
        # What the nightly table says of the same pairs would let a broken base through as `fixed`.
        for base, cand, nightly, gated in (("failed", "valid", "fixed", "incomplete"),
                                           ("invalid", "valid", "fixed", "incomplete"),
                                           ("failed", "failed", "both_invalid", "block"),
                                           ("valid", "invalid", "issue", "block"),
                                           ("invalid", "invalid", "both_invalid", "block"),
                                           ("valid", "failed", "block", "block")):
            with self.subTest(base=base, cand=cand):
                self.assertEqual(verdict.table_outcome(base, cand), nightly)
                self.assertEqual(gate.side_outcome(base, cand), gated)


class Verdict(unittest.TestCase):
    def run_cmd(self, final):
        """The exit code `cmd_gate` gives to a gate that ended with the verdict `final`."""
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as tmp:
            def fake_gate(args, clock, builder, machine, data, out_dir, summary):
                summary.update(verdict=final, reasons=[])

            args = argparse.Namespace(data_dir=tmp, base="v0.4.1", candidate="HEAD", label="g1",
                                      ready_only=False, busy_budget_min=None, control_burn_cpu_s=None,
                                      control_start_delay_s=None, control_foreign_burn=None)
            with patch.object(gate, "_gate", fake_gate), contextlib.redirect_stdout(io.StringIO()) as out:
                code = gate.cmd_gate(args, object(), None, object())
            return code, out.getvalue()

    def test_verdict_precedence_and_exit_codes(self):
        block, noisy, ok = ({"metric": "m", "outcome": o} for o in ("block", "noisy", "ok"))
        self.assertEqual(verdict.gate_verdict([block, noisy], ["machine_busy"])["verdict"], "block")
        self.assertEqual(verdict.gate_verdict([ok, noisy], [])["verdict"], "incomplete")
        self.assertEqual(verdict.gate_verdict([ok, ok], ["machine_busy"])["verdict"], "incomplete")
        self.assertEqual(verdict.gate_verdict([ok, ok], [])["verdict"], "pass")
        self.assertEqual(verdict.gate_verdict([ok, {"metric": "w", "outcome": "noise"}], [])["verdict"], "pass")
        for state in ("no_noise", "noisy", "confirm", "incomplete"):
            self.assertEqual(verdict.gate_verdict([ok, {"metric": "m", "outcome": state}], [])["verdict"],
                             "incomplete", state)
        result = verdict.gate_verdict([block, noisy, ok], ["answers_differ"])
        self.assertEqual(result["counts"], {"block": 1, "noisy": 1, "ok": 1})
        self.assertEqual(result["reasons"], ["answers_differ", "block: m", "noisy: m"])
        for final, expected in (("pass", 0), ("probe-ok", 0), ("block", 1), ("incomplete", 2)):
            code, out = self.run_cmd(final)
            self.assertEqual(code, expected, final)
            self.assertIn(f"gate={final} ", out)

    def test_a_refusal_is_exit_code_two(self):
        with tempfile.TemporaryDirectory(prefix="perf-nightly-test-") as tmp:
            os.makedirs(os.path.join(tmp, "gates", "g1"))
            args = ["gate", "--data-dir", tmp, "--base", "v0.4.1", "--candidate", "HEAD", "--label", "g1"]
            with contextlib.redirect_stderr(io.StringIO()) as err:
                code = perf_nightly.main(args)
        self.assertEqual(code, 2)
        self.assertIn("gate_exists", err.getvalue())


class MachineLoad(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.addCleanup(self._tmp.cleanup)
        self.root = self._tmp.name

    def stat(self, line):
        with open(os.path.join(self.root, "stat"), "w") as fh:
            fh.write(f"{line}\ncpu0 1 1 1 1 1 1 1 1 0 0\nprocs_running 3\n")
        return procs.machine_busy_s(self.root)

    def test_foreign_cores_from_proc_stat(self):
        self.assertEqual(procs.CLK_TCK, 100, "the plan's ticks are a hundredth of a second")
        before = self.stat("cpu  100 0 50 1000 20 0 0 0")
        after = self.stat("cpu  400 0 150 1100 20 0 0 0")
        # 400 ticks = 4.0 s busy; idle (100 ticks) is not work.
        self.assertAlmostEqual(after - before, 4.0)
        self.assertAlmostEqual(gate.foreign_cores(after - before, 2.0, 1.0), 2.0)
        self.assertLessEqual(gate.foreign_cores(after - before, 2.0, 1.0), verdict.GATE_FOREIGN_CORES)
        over = self.stat("cpu  401 0 150 1100 20 0 0 0")
        self.assertAlmostEqual(over - after, 0.01)
        self.assertAlmostEqual(gate.foreign_cores(over - before, 2.0, 1.0), 2.01)
        self.assertGreater(gate.foreign_cores(over - before, 2.0, 1.0), verdict.GATE_FOREIGN_CORES)
        self.assertEqual(gate.Machine(self.root).busy_s(), over)

    def test_every_busy_field_counts_and_idle_and_iowait_do_not(self):
        base = self.stat("cpu  0 0 0 0 0 0 0 0 0 0")
        # user nice system idle iowait irq softirq steal guest guest_nice
        for index, (name, counts) in enumerate((("user", 1), ("nice", 1), ("system", 1), ("idle", 0),
                                                ("iowait", 0), ("irq", 1), ("softirq", 1), ("steal", 1),
                                                ("guest", 0), ("guest_nice", 0))):
            fields = [0] * 10
            fields[index] = 100
            with self.subTest(field=name):
                self.assertAlmostEqual(self.stat("cpu  " + " ".join(map(str, fields))) - base, float(counts))

    def test_a_stat_without_the_cpu_line_is_an_error(self):
        with open(os.path.join(self.root, "stat"), "w") as fh:
            fh.write("procs_running 3\n")
        with self.assertRaises(OSError):
            procs.machine_busy_s(self.root)

    def test_no_wall_time_is_all_the_foreign_load_or_none(self):
        self.assertEqual(gate.foreign_cores(1.0, 0.5, 0.0), math.inf)
        self.assertEqual(gate.foreign_cores(0.5, 0.5, 0.0), 0.0)

    def test_the_precondition_wants_memory_and_a_quiet_load(self):
        with open(os.path.join(self.root, "meminfo"), "w") as fh:
            fh.write(f"MemTotal: {64 << 20} kB\nMemAvailable: {13 << 20} kB\n")
        loadavg = os.path.join(self.root, "loadavg")

        def quiet(load1, available_kb=13 << 20):
            with open(os.path.join(self.root, "meminfo"), "w") as fh:
                fh.write(f"MemTotal: {64 << 20} kB\nMemAvailable: {available_kb} kB\n")
            with open(loadavg, "w") as fh:
                fh.write(f"{load1} 0.50 0.50 1/100 123\n")
            return gate.Machine(self.root).quiet()

        self.assertTrue(quiet("2.00")["ok"])
        self.assertFalse(quiet("2.01")["ok"])
        self.assertFalse(quiet("0.10", (13 << 20) - 1)["ok"])
        self.assertTrue(quiet("0.10", 13 << 20)["ok"])


def _median3(rng, sigma, shift=0.0):
    return statistics.median(math.exp(rng.gauss(math.log1p(shift), sigma)) for _ in range(3))


def _noise(rng, sigma):
    medians = [_median3(rng, sigma) for _ in range(5)]
    return (max(medians) - min(medians)) / statistics.median(medians)


class OperatingCharacteristics(unittest.TestCase):
    """Plan appendix A on the rule that the harness runs: lognormal runs, the
    noise calibrated as the harness does (five medians of three), a seed."""

    N = 4000

    def new_rule(self, rng, sigma_wall, sigma_cpu, work, wait):
        noise_cpu, noise_wall = _noise(rng, sigma_cpu), _noise(rng, sigma_wall)
        cpu = _median3(rng, sigma_cpu, work) / _median3(rng, sigma_cpu) - 1
        total = (1 + work) * (1 + wait) - 1
        wall = _median3(rng, sigma_wall, total) / _median3(rng, sigma_wall) - 1
        wall_outcome = verdict.gate_outcome(WALL, wall, noise_wall)
        if wall_outcome == "confirm":
            again = _median3(rng, sigma_wall, total) / _median3(rng, sigma_wall) - 1
            wall_outcome = verdict.gate_outcome(WALL, wall, noise_wall, again)
        rows = [{"metric": "graph_resolve_s", "outcome": verdict.gate_outcome("graph_resolve_s", cpu, noise_cpu)},
                {"metric": WALL, "outcome": wall_outcome}]
        return verdict.gate_verdict(rows, [])["verdict"]

    def old_rule(self, rng, sigma_wall, work):
        return _median3(rng, sigma_wall, work) / _median3(rng, sigma_wall) > 1.05

    def rate(self, fn, *args):
        rng = random.Random(745)
        return sum(fn(rng, *args) for _ in range(self.N)) / self.N

    def test_operating_characteristics(self):
        quiet_block = self.rate(lambda rng, *a: self.new_rule(rng, *a) == "block", 0.11, 0.01, 0.0, 0.0)
        work_block = self.rate(lambda rng, *a: self.new_rule(rng, *a) == "block", 0.11, 0.01, 0.078, 0.0)
        old_fail = self.rate(self.old_rule, 0.11, 0.0)
        self.assertLessEqual(quiet_block, 0.04, "noise must not stop a release")
        self.assertGreaterEqual(work_block, 0.97, "a +7.8 % shift of the work must not pass")
        self.assertGreaterEqual(old_fail, 0.25, "the control: the old rule fails a quiet candidate")

    def test_the_rule_reads_its_bar_from_the_verdict_module(self):
        # The same draws under a bar of 40 %: the work shift passes, which is what the test above is for.
        with patch.object(verdict, "GATE_BAR", 0.40):
            work_block = self.rate(lambda rng, *a: self.new_rule(rng, *a) == "block", 0.11, 0.01, 0.078, 0.0)
        self.assertLess(work_block, 0.5)


def peaks(*values):
    """Three runs whose peak memory is each of the given values."""
    return [record(k + 1, hwm_kb=v) for k, v in enumerate(values)]


class MemoryAgainstTheTargetAndTheCeiling(unittest.TestCase):
    """The target of memory holds the peak of series A, which runs no references,
    with its noise; the peak of series B answers to the absolute ceiling alone, and
    the peak of A and B together is compared with nothing."""

    TARGET = 4667316
    CEILING = 7340032

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def baseline(self, noise):
        path = os.path.join(self.tmp.name, f"baseline-{noise}.json")
        tolerance = {"value": noise}
        entry = {"value": self.TARGET, "unit": "KB", "aggregate": verdict.GATE_RECORD["hwm_kb"][1],
                 "record_sha": "0" * 40, "measured_at": "2026-11-06T19:08:19+00:00",
                 "evidence": {field: None for field in gate.EVIDENCE_FIELDS},
                 "noise": {**tolerance, "base_A_noise": tolerance}}
        gate.write_json(path, {"schema": gate.BASELINE_SCHEMA, "profile": {"cpu_model": "x"},
                               "protocol": {"runs": 3}, "metrics": {"hwm_kb": entry}})
        return gate.Baseline(path)

    def round_(self, a, b):
        round_ = gate.new_round()
        round_["candidate"], round_["B"] = peaks(*a), peaks(*b)
        return round_

    def record(self, noise, a, b):
        return gate.record_row("hwm_kb", self.baseline(noise), [], [self.round_(a, b)])

    def test_the_target_holds_the_peak_of_a_with_its_noise(self):
        # 4 667 316 · 1.02 = 4 760 662.32; B stays far over the target and under the ceiling in every case.
        for peak_a, outcome in ((4760662, "ok"), (4760663, "block")):
            with self.subTest(peak_a=peak_a):
                row = self.record(0.02, (4600000, peak_a, 4650000), (7000000, 7100000, 6900000))
                self.assertEqual((row["aggregate"], row["candidate"]["value"], row["outcome"], row["limit_basis"]),
                                 ("max_A", peak_a, outcome, "noise"))
                self.assertAlmostEqual(row["limit"], self.TARGET * 1.02)

    def test_the_peak_of_b_answers_to_the_absolute_ceiling_alone(self):
        self.assertEqual(verdict.GATE_HWM_LIMIT_KB, self.CEILING)
        series_a = peaks(4600000, 4610000, 4620000)
        for peak_b, outcome in ((self.CEILING, "ok"), (self.CEILING + 1, "block")):
            with self.subTest(peak_b=peak_b):
                series_b = peaks(6900000, peak_b, 7000000)
                [ceiling] = [r for r in gate.ceiling_rows(series_a, series_b) if r["metric"] == "hwm_kb"]
                self.assertEqual((ceiling["candidate"]["value"], ceiling["limit"], ceiling["outcome"]),
                                 (peak_b, self.CEILING, outcome))
                row = self.record(0.0, (4600000, 4610000, 4620000), (6900000, peak_b, 7000000))
                self.assertEqual((row["candidate"]["value"], row["outcome"]), (4620000, "ok"),
                                 "B over the target is no loss of the record")

    def test_the_peak_of_a_and_b_together_is_compared_with_nothing(self):
        self.assertNotIn("max_AB", {aggregate for _, aggregate in verdict.GATE_RECORD.values()})
        # A at the target exactly and B over it: the peak of both would block at any noise under 57 %.
        round_ = self.round_((4600000, self.TARGET, 4650000), (7300000, 7340032, 7200000))
        self.assertEqual(gate._round_side("hwm_kb", round_), ("valid", self.TARGET))
        self.assertEqual(gate._names({**round_, "names": {"candidate": ["A-c1"], "B": ["B-c1"]}}, "max_A"), ["A-c1"])
        row = gate.record_row("hwm_kb", self.baseline(0.0), [], [round_])
        self.assertEqual((row["candidate"]["value"], row["delta"], row["outcome"]), (self.TARGET, 0.0, "ok"))


if __name__ == "__main__":
    unittest.main()
