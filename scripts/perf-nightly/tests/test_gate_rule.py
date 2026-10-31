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


class CpuTradedForWall(unittest.TestCase):
    """More CPU time is accepted where the wall time fell by at least as much;
    the machine has idle cores, the user waits for the wall."""

    NOISE = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=0.01, graph_resolve_s=0.01, diagnostics_workspace_s=0.01)

    def rows(self, wall, cpu, noise=None):
        base = [record(k + 1, graph_ready_s=100.0, graph_ready_cpu_s=40.0) for k in range(3)]
        cand = [record(k + 1, graph_ready_s=100.0 * (1 + wall), graph_ready_cpu_s=40.0 * (1 + cpu))
                for k in range(3)]
        rows = gate.rule_rows(base, cand, noise or self.NOISE, True)
        return {r["metric"]: r for r in rows}

    def test_a_faster_wall_pays_for_the_growth_of_cpu(self):
        rows = self.rows(-0.45, 0.24)
        cpu = rows["graph_ready_cpu_s"]
        self.assertEqual((cpu["outcome"], cpu["basis"]), ("ok", "growth_traded"))
        self.assertAlmostEqual(cpu["delta"], 0.24)
        self.assertAlmostEqual(cpu["wall_delta"], -0.45)
        self.assertEqual(rows["graph_ready_s"]["outcome"], "ok")
        self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "pass")

    def test_growth_without_a_faster_wall_blocks(self):
        for wall in (0.01, 0.0):
            with self.subTest(wall=wall):
                cpu = self.rows(wall, 0.24)["graph_ready_cpu_s"]
                self.assertEqual((cpu["outcome"], cpu["basis"]), ("block", "growth_not_repaid"))

    def test_growth_larger_than_the_gain_of_the_wall_blocks(self):
        rows = self.rows(-0.10, 0.30)
        self.assertEqual(rows["graph_ready_cpu_s"]["outcome"], "block")
        self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "block")

    def test_growth_within_the_bar_passes_without_a_note(self):
        cpu = self.rows(-0.45, 0.03)["graph_ready_cpu_s"]
        self.assertEqual((cpu["outcome"], cpu["basis"]), ("ok", None))
        self.assertEqual(self.rows(0.0, 0.05)["graph_ready_cpu_s"]["outcome"], "ok")

    def test_the_gain_of_the_wall_must_reach_the_growth(self):
        # Equal: traded; a hair less gain: not. The wall at -24 % and at -23.9 % is far from the wall's own limit.
        self.assertEqual(gate.cpu_outcome(0.24, -0.24, 0.01), ("ok", "growth_traded"))
        self.assertEqual(gate.cpu_outcome(0.24, -0.239, 0.01), ("block", "growth_not_repaid"))
        self.assertEqual(gate.cpu_outcome(0.0501, -0.0501, 0.01), ("ok", "growth_traded"))
        self.assertEqual(gate.cpu_outcome(0.0501, None, 0.01), ("block", "growth_not_repaid"))

    def test_a_noisy_cpu_time_never_passes_however_fast_the_wall(self):
        for cpu_noise, expected in ((0.06, "noisy"), (None, "no_noise")):
            with self.subTest(noise=cpu_noise):
                noise = noise_of(graph_ready_s=0.03, graph_ready_cpu_s=cpu_noise)
                for cpu_delta in (0.24, 0.03, 0.0):
                    rows = self.rows(-0.45, cpu_delta, noise)
                    self.assertEqual(rows["graph_ready_cpu_s"]["outcome"], expected)
                    self.assertEqual(verdict.gate_verdict(list(rows.values()), [])["verdict"], "incomplete")

    def test_only_the_cpu_of_readiness_is_traded(self):
        # Resolve and sweep time are held to the bar with no wall to pay for them.
        base, cand = side(40.0, 40.0, 40.0), side(49.6, 49.6, 49.6)
        for r in cand:
            r["metrics"]["graph_ready_s"] = 55.0
        for r in base:
            r["metrics"]["graph_ready_s"] = 100.0
        rows = {r["metric"]: r for r in gate.rule_rows(base, cand, self.NOISE, False)}
        self.assertEqual(rows["graph_ready_cpu_s"]["outcome"], "ok")
        self.assertEqual(rows["graph_resolve_s"]["outcome"], "block")
        self.assertEqual(rows["diagnostics_workspace_s"]["outcome"], "block")
        self.assertNotIn("basis", rows["graph_resolve_s"])


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
                rows = gate.rule_rows(base, cand, noise, True)
                self.assertEqual({r["outcome"] for r in rows}, {expected})


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
        rows = [{"metric": "graph_ready_cpu_s", "outcome": verdict.gate_outcome("graph_ready_cpu_s", cpu, noise_cpu)},
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


if __name__ == "__main__":
    unittest.main()
