"""`night` on fake binaries: both sides in one window, the gate's own
directory, recalibration nights, the deadline, the load of foreign processes
and the choice of the base release (plan §7.3–§7.4, §8.6, §10.2)."""

import argparse
import contextlib
import datetime
import io
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from helpers import FOREIGN_PREFIX, FakeClock, Workspace, run_git, set_mode

import night
import perf_nightly
import procs
import verdict
from common import HARNESS_SCHEMA, HarnessError, read_json, write_json

SMALL = (("analyze", ("crm",)),)
DATE = "2026-10-22"


def commit(repo, name):
    with open(os.path.join(repo, "f"), "a") as fh:
        fh.write(name + "\n")
    run_git("add", "-A", cwd=repo)
    run_git("commit", "-q", "-m", name, cwd=repo)
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True,
                          text=True).stdout.strip()


def make_repo(path):
    os.makedirs(path)
    run_git("init", "-q", "-b", "main", cwd=path)
    run_git("config", "user.email", "t@t", cwd=path)
    run_git("config", "user.name", "t", cwd=path)


class AdvancingClock(FakeClock):
    """Every awaited process exit takes `step` virtual seconds."""

    def __init__(self, step):
        super().__init__()
        self.step = step

    def wait_exit(self, pidfds, timeout):
        done = super().wait_exit(pidfds, timeout)
        self.advance(self.step)
        return done


class NightCase(Workspace):
    def setUp(self):
        super().setUp()
        self.clone = self.data.path("build")
        make_repo(self.clone)
        self.sha = {"v0.3.2": commit(self.clone, "release")}
        run_git("tag", "v0.3.2", cwd=self.clone)
        self.sha["develop"] = commit(self.clone, "develop")
        self.sha["v0.3.3"] = commit(self.clone, "candidate")
        run_git("tag", "v0.3.3", cwd=self.clone)
        self.paths = {ref: self.fake_binary(label=ref, sha=sha) for ref, sha in self.sha.items()}
        self.built = []
        for path in self.paths.values():
            night.ensure_help(self.data, perf_nightly.load_binary(path), self.manifest["platform_help"],
                              FakeClock())

    def measured(self):
        return [l for l in self.launches() if "BSL_LOG" not in l["env"]]

    def builder(self, ref):
        self.built.append((ref, len(self.measured())))
        return self.paths[ref]

    def night(self, clock=None, now=None, **kw):
        values = {"data_dir": self.data_dir, "corpora": None, "deadline": None, "candidate": None,
                  "date": DATE, "remote": None}
        values.update(kw)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            summary = night.run_night(argparse.Namespace(**values), clock or FakeClock(),
                                      now=now or datetime.datetime(2026, 10, 22, 5, 0),
                                      builder=self.builder)
        return summary

    def base(self):
        return perf_nightly.load_binary(self.paths["v0.3.2"])

    def fresh_noise(self, value=0.2):
        corpora = ["crm"]
        entries = {verdict.noise_key(c, sdir, temp, m.key): {"noise": value}
                   for c, sdir, temp in night.expected_keys(corpora) for m in verdict.metrics_of(sdir)}
        write_json(self.data.path("noise.json"), {
            "harness_schema": HARNESS_SCHEMA, "entries": entries,
            "fingerprints": night.fingerprints(self.data, self.base(), corpora)})

    def results(self, ref):
        return self.data.results({"label": ref, "sha": self.sha[ref]})

    def series(self, ref, series, scenario="analyze"):
        path = os.path.join(self.results(ref), "crm", scenario, "warm", series)
        return sorted(os.listdir(path)) if os.path.isdir(path) else []


class Night(NightCase):
    def test_a_night_judges_develop_against_the_release_of_the_same_night(self):
        self.fresh_noise()
        summary = self.night()
        self.assertEqual((summary["kind"], summary["series"]), ("night", f"night-{DATE}"))
        self.assertEqual(summary["base"]["label"], "v0.3.2")
        self.assertEqual(summary["candidate"]["label"], "develop")
        self.assertEqual([ref for ref, _ in self.built], ["develop", "v0.3.2"])
        self.assertEqual({launches for _, launches in self.built}, {0}, "both builds precede the first run")
        for ref in ("develop", "v0.3.2"):
            self.assertEqual(self.series(ref, f"night-{DATE}"), ["r1", "r1.json", "r2", "r2.json", "r3",
                                                                 "r3.json"])
        rows = read_json(self.data.path("nights", DATE, "compare.json"))["rows"]
        self.assertEqual({(r["corpus"], r["scenario"]) for r in rows},
                         {(c, s) for c, s, _ in night.expected_keys(["crm"])})
        self.assertIn(summary["verdict"], ("ok", "issue", "block", "incomplete"))
        self.assertEqual(summary["state"], "done")
        self.assertIn("candidate/crm/mcp-load-c4", summary["durations"])
        with open(self.data.path("nights", DATE, "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("## Тренд", report)
        self.assertIn(f"вердикт: **{summary['verdict']}**", report)

    def test_a_done_night_is_not_measured_again(self):
        self.fresh_noise()
        with patch.object(night, "SUITE", SMALL):
            first = self.night()
            launches = len(self.measured())
            again = self.night()
        self.assertEqual(again, first)
        self.assertEqual(len(self.measured()), launches)

    def test_the_gate_writes_its_own_directory_only(self):
        # И54: a gate on the date of a develop night leaves that night byte for byte.
        self.fresh_noise()
        with patch.object(night, "SUITE", SMALL):
            self.night()
            night_dir = self.data.path("nights", DATE)
            before = {name: open(os.path.join(night_dir, name), "rb").read() for name in os.listdir(night_dir)}
            gate = self.night(candidate="v0.3.3")
        self.assertEqual((gate["kind"], gate["series"]), ("gate", "gate-v0.3.3"))
        self.assertEqual((gate["candidate"]["label"], gate["base"]["label"]), ("v0.3.3", "v0.3.2"))
        self.assertTrue(os.path.isfile(self.data.path("gates", "v0.3.3", "summary.json")))
        after = {name: open(os.path.join(night_dir, name), "rb").read() for name in os.listdir(night_dir)}
        self.assertEqual(after, before)
        self.assertEqual(len(self.series("v0.3.3", "gate-v0.3.3")), 6)

    def test_an_analyze_base_failure_leaves_the_diff_unbuilt(self):
        # И47: the base's analyze fails in r1; the night cannot be ok.
        self.fresh_noise()
        set_mode("v0.3.2=exit2")
        with patch.object(night, "SUITE", SMALL):
            summary = self.night()
        self.assertEqual(summary["diff"]["not_built"], ["crm"])
        self.assertNotEqual(summary["verdict"], "ok")
        self.assertIn("diff_not_built: crm", summary["reasons"])


class Calibration(NightCase):
    def test_a_stale_calibration_takes_three_nights_of_calibration_series(self):
        # И46: noise.json of an older harness schema; two series a night, the fifth writes noise.json.
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        with patch.object(night, "SUITE", SMALL):
            nights = []
            for day in (22, 23):
                nights.append(self.night(date=f"2026-10-{day}"))
            gate = self.night(candidate="v0.3.3")
            nights.append(self.night(date="2026-10-24"))
            judged = self.night(date="2026-10-25")
        self.assertEqual([n["calibration"]["measured"] for n in nights],
                         [["cal-1", "cal-2"], ["cal-3", "cal-4"], ["cal-5"]])
        self.assertEqual([n["verdict"] for n in nights], ["incomplete"] * 3)
        self.assertEqual([n["reasons"] for n in nights], [["noise_stale"]] * 3)
        self.assertEqual([n["calibration"]["noise_written"] for n in nights], [False, False, True])
        self.assertEqual((gate["verdict"], gate["reasons"]), ("incomplete", ["noise_stale"]))
        self.assertEqual(self.series("v0.3.3", "gate-v0.3.3"), [], "no gate runs while calibrating")
        for day in (22, 23, 24):
            self.assertEqual(self.series("develop", f"night-2026-10-{day}"), [], "no develop set")
        noise = read_json(self.data.path("noise.json"))
        self.assertEqual(noise["harness_schema"], HARNESS_SCHEMA)
        self.assertEqual(noise["series"], ["cal-1", "cal-2", "cal-3", "cal-4", "cal-5"])
        self.assertEqual(noise["fingerprints"], night.fingerprints(self.data, self.base(), ["crm"]))
        self.assertIn("crm/analyze/warm/wall_s", noise["entries"])
        self.assertEqual(noise["self_check"]["violations"], [])
        self.assertEqual((judged["kind"], judged["noise"]["status"]), ("night", "current"))

    def test_a_gate_refused_for_stale_calibration_runs_once_it_is_current(self):
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        with patch.object(night, "SUITE", SMALL):
            refused = self.night(candidate="v0.3.3")
            self.fresh_noise()
            judged = self.night(candidate="v0.3.3")
        self.assertEqual((refused["reasons"], refused["measured"]), (["noise_stale"], False))
        self.assertEqual((judged["noise"]["status"], judged["measured"]), ("current", True))
        self.assertEqual(len(self.series("v0.3.3", "gate-v0.3.3")), 6)

    def test_calibration_covers_the_whole_set(self):
        # noise.json is one file for every corpus of the set; a corpus-scoped run would rewrite it.
        args = argparse.Namespace(data_dir=self.data_dir, corpora=None, binary=self.paths["v0.3.2"],
                                  temp="warm", series=1, deadline=None, corpus=["crm"])
        with self.assertRaisesRegex(HarnessError, "calibration_scope"):
            perf_nightly.cmd_calibrate(args, FakeClock())
        self.assertEqual(self.measured(), [])

    def test_a_series_cut_by_the_window_does_not_count(self):
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        set_mode("v0.3.2=exit2")
        with patch.object(night, "SUITE", SMALL):
            for k in range(1, 4):
                write_json(os.path.join(self.results("v0.3.2"), "crm", "analyze", "warm", "cal-1",
                                        f"r{k}.json"), {"k": k, "held": False, "reason": "window_exhausted"})
            prints = night.fingerprints(self.data, self.base(), ["crm"])
            self.assertEqual(night.current_cal(self.results("v0.3.2"), ["crm"], prints), [])
            summary = self.night()
        self.assertEqual(summary["calibration"]["measured"], ["cal-2", "cal-3"])
        self.assertEqual(summary["calibration"]["complete"], ["cal-2", "cal-3"],
                         "failed runs of the version count; skipped ones do not")


    def test_system_updates_keep_the_calibration(self):
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        with patch.object(night, "SUITE", SMALL):
            self.night()
            prints = night.fingerprints(self.data, self.base(), ["crm"])
            self.assertEqual(night.current_cal(self.results("v0.3.2"), ["crm"], prints), ["cal-1", "cal-2"])
            rewritten = {"cal-1": {**prints["crm"], "kernel": "other", "rustc": "rustc other", "mem_total": "63 GiB"},
                         "cal-2": {**prints["crm"], "cpu_model": "other cpu"}}
            for name, fingerprint in rewritten.items():
                for k in range(1, 4):
                    path = os.path.join(self.results("v0.3.2"), "crm", "analyze", "warm", name, f"r{k}.json")
                    write_json(path, {**read_json(path), "env_fingerprint": fingerprint})
            self.assertEqual(night.current_cal(self.results("v0.3.2"), ["crm"], prints), ["cal-1"],
                             "an update of the kernel, compiler or memory keeps a series; another CPU does not")


class Window(NightCase):
    def test_a_run_that_would_end_after_the_deadline_is_skipped(self):
        # И31: no history, so analyze on crm is estimated at 29.4 s; each run takes 25 s; 60 s left.
        self.fresh_noise()
        clock = AdvancingClock(25.0)
        with patch.object(night, "SUITE", SMALL):
            summary = self.night(clock=clock, deadline="05:01", now=datetime.datetime(2026, 10, 22, 5, 0))
        base = os.path.join(self.results("v0.3.2"), "crm", "analyze", "warm", f"night-{DATE}")
        reasons = [read_json(os.path.join(base, f"r{k}.json"))["reason"] for k in (1, 2, 3)]
        self.assertEqual(reasons, [None, None, "window_exhausted"])
        cand = os.path.join(self.results("develop"), "crm", "analyze", "warm", f"night-{DATE}")
        self.assertEqual({read_json(os.path.join(cand, f"r{k}.json"))["reason"] for k in (1, 2, 3)},
                         {"window_exhausted"})
        self.assertEqual(len(summary["skipped"]), 4)
        self.assertEqual(summary["skipped"][0]["estimate_s"], 29.4)
        self.assertEqual(summary["verdict"], "incomplete")
        self.assertIn("missing", summary["reasons"])

    def test_the_previous_night_sets_the_estimate(self):
        write_json(self.data.path("nights", "2026-10-21", "summary.json"),
                   {"durations": {"base/crm/analyze": [5.0, 7.0, 6.0]}})
        estimates = night.Estimates(night.previous_history(self.data, DATE))
        self.assertEqual(estimates.run("base", "crm", "analyze"), 6.0)
        self.assertEqual(estimates.run("candidate", "crm", "analyze"), 29.4)
        self.assertIsNone(night.previous_history(self.data, "2026-10-21"))

    def test_deadline_is_the_next_local_time(self):
        now = datetime.datetime(2026, 10, 22, 5, 0)
        self.assertEqual(night.seconds_until("08:45", now), 3.75 * 3600)
        self.assertEqual(night.seconds_until("04:00", now), 23 * 3600)
        for bad in ("24:00", "08:60", "99:99", "8:45"):
            with self.assertRaisesRegex(HarnessError, "deadline_invalid"):
                night.seconds_until(bad, now)


class Foreign(NightCase):
    """Other `bsl-analyzer*` processes are part of the measured load: the
    night and its calibration neither wait for them nor cancel a run."""

    FOREIGN = [{"pid": 4242, "name": "bsl-analyzer-app"}]

    def held(self, ref, series):
        base = os.path.join(self.results(ref), "crm", "analyze", "warm", series)
        return [(r["held"], r["reason"]) for r in
                (read_json(os.path.join(base, f"r{k}.json")) for k in (1, 2, 3))]

    def test_a_night_with_foreign_processes_measures_everything(self):
        # Input: a foreign process is reported before the night and around every run.
        self.fresh_noise()
        with patch.object(night, "SUITE", SMALL), \
                patch.object(procs, "foreign", return_value=self.FOREIGN) as foreign:
            summary = self.night()
        self.assertEqual(foreign.call_count, 0, "the night never looks for foreign processes")
        for ref in ("develop", "v0.3.2"):
            self.assertEqual(self.held(ref, f"night-{DATE}"), [(True, None)] * 3)
        self.assertNotIn("aborted", summary)
        self.assertFalse([r for r in summary["reasons"] if "foreign" in r or "aborted" in r])
        self.assertTrue(summary["measured"])

    def test_a_calibration_night_with_foreign_processes_measures_both_series(self):
        with patch.object(night, "SUITE", SMALL), \
                patch.object(procs, "foreign", return_value=self.FOREIGN) as foreign:
            summary = self.night()
        self.assertEqual(foreign.call_count, 0)
        self.assertEqual(summary["calibration"]["measured"], ["cal-1", "cal-2"])
        for name in ("cal-1", "cal-2"):
            self.assertEqual(self.held("v0.3.2", name), [(True, None)] * 3)

    def test_a_suite_with_foreign_processes_measures_everything(self):
        args = perf_nightly.parser().parse_args(["suite", "--data-dir", self.data_dir,
                                                 "--binary", self.paths["v0.3.2"], "--series", "base-s"])
        with patch.object(night, "SUITE", SMALL), \
                patch.object(procs, "foreign", return_value=self.FOREIGN) as foreign, \
                contextlib.redirect_stdout(io.StringIO()):
            code = perf_nightly.cmd_suite(args, FakeClock())
        self.assertEqual((code, foreign.call_count), (0, 0))
        self.assertEqual(self.held("v0.3.2", "base-s"), [(True, None)] * 3)

    def test_a_real_foreign_process_during_a_run_cancels_nothing(self):
        # The fake app spawns a detached `perf-nightly-test-x` without the run marker on its 2nd launch.
        # Any foreign check, whatever prefix it asks for, looks for that name, so a check would see it.
        self.fresh_noise()
        set_mode("foreign2")
        real = procs.foreign

        def any_prefix(run_id=None, proc_root="/proc", prefix=None):
            return real(run_id, proc_root, prefix=FOREIGN_PREFIX)

        try:
            with patch.object(night, "SUITE", SMALL), patch.object(procs, "foreign", side_effect=any_prefix):
                summary = self.night()
        finally:
            pid_file = os.path.join(self.log, "foreign.pid")
            if os.path.exists(pid_file):
                os.kill(int(open(pid_file).read()), signal.SIGKILL)
        self.assertTrue(os.path.exists(pid_file), "the foreign process was spawned")
        self.assertEqual(self.held("v0.3.2", f"night-{DATE}"), [(True, None)] * 3)
        self.assertEqual(self.held("develop", f"night-{DATE}"), [(True, None)] * 3)
        self.assertNotIn("aborted", summary)

    def test_a_survivor_of_a_run_stops_the_runs_after_it(self):
        # Input: the stop of the first run leaves a live process with that run's marker.
        self.fresh_noise()
        real, survivor = procs.stop, []

        def leaves_one(run_id, clock=None, **kw):
            report = real(run_id, clock, **kw)
            if not survivor:
                survivor.append(subprocess.Popen([sys.executable, "-c", "import signal\nsignal.pause()"],
                                                 env={**os.environ, procs.MARKER: run_id}))
                report["left"] = [survivor[0].pid]
            return report

        try:
            with patch.object(night, "SUITE", SMALL), patch.object(procs, "stop", side_effect=leaves_one):
                summary = self.night()
        finally:
            for proc in survivor:
                proc.kill()
                proc.wait()
        self.assertEqual(self.held("v0.3.2", f"night-{DATE}"),
                         [(False, "stop_incomplete")] + [(False, "survivor_running")] * 2)
        self.assertEqual(self.held("develop", f"night-{DATE}"), [(False, "survivor_running")] * 3)
        self.assertEqual(summary["verdict"], "incomplete")


class BaseSelection(Workspace):
    def test_the_highest_release_ancestor_that_is_not_the_candidate(self):
        # И35: v0.3.2 and v0.3.10 are ancestors, v0.4.0 is not; a candidate tag is not its own base.
        repo = os.path.join(self.tmp, "repo")
        make_repo(repo)
        commit(repo, "a")
        run_git("tag", "v0.3.2", cwd=repo)
        commit(repo, "b")
        run_git("tag", "v0.3.10", cwd=repo)
        run_git("checkout", "-q", "-b", "side", cwd=repo)
        commit(repo, "side")
        run_git("tag", "v0.4.0", cwd=repo)
        run_git("checkout", "-q", "main", cwd=repo)
        candidate = commit(repo, "c")
        self.assertEqual(night.select_base(repo, candidate), "v0.3.10")
        run_git("tag", "v0.3.11", cwd=repo)
        self.assertEqual(night.select_base(repo, candidate), "v0.3.10")
        run_git("tag", "v0.3.11-rc1", "HEAD~1", cwd=repo)
        self.assertEqual(night.select_base(repo, candidate), "v0.3.10", "only vX.Y.Z tags are releases")


CLOSE_SET = (("lsp-session", ("crm",)), ("lsp-close-memory", ("crm",)))
ALL = ["crm", "ut", "erp", "lombard"]


class CloseMemory(NightCase):
    """Memory after closing documents: the candidate alone, one run, on the
    cache lsp-session warmed; reported with its time."""

    def test_the_candidate_alone_measures_it_once_and_the_report_shows_it(self):
        with patch.object(night, "SUITE", CLOSE_SET):
            self.fresh_noise()
            summary = self.night()
        self.assertEqual(self.series("develop", f"night-{DATE}", "lsp-close-memory"), ["r1", "r1.json"])
        self.assertEqual(self.series("v0.3.2", f"night-{DATE}", "lsp-close-memory"), [])
        rows = read_json(self.data.path("nights", DATE, "compare.json"))["rows"]
        row = next(r for r in rows if r["scenario"] == "lsp-close-memory")
        self.assertEqual((row["metric"], row["base"]["state"], row["outcome"]),
                         ("rss_growth_max_kb", "missing", "ok"))
        [item] = summary["close_memory"]
        self.assertEqual((item["label"], item["outcome"], len(item["cycles"])), ("ЦРМ", "ok", 3))
        self.assertEqual(item["duration_s"], sum(summary["durations"]["candidate/crm/lsp-close-memory"]))
        with open(self.data.path("nights", DATE, "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("## Память после закрытия документов", report)
        self.assertIn("### ЦРМ", report)
        self.assertIn("- время сценария: ", report)
        self.assertIn("| 3 | ", report)
        self.assertIn("больше 50 МБ", report)
        self.assertIn("больше 300 МБ", report)
        self.assertIn("дольше 0.2 с", report)
        self.assertIn("| цикл | открытие, с | глобальный запрос, с | RSS после закрытия, МБ | "
                      "над готовностью, МБ | прирост, МБ |", report)
        cycle = item["cycles"][2]
        over = (cycle["rss_closed_kb"] - item["rss_ready_kb"]) / 1024
        self.assertIn(f"| 3 | {verdict.fmt(cycle['open_s'])} | 0.000 | {cycle['rss_closed_kb'] / 1024:.1f} | "
                      f"{over:+.1f} | ", report)

    def test_the_table_shows_the_global_query_and_the_memory_over_ready(self):
        mib = 1024
        cycles = [{"cycle": n, "open_s": 1.0, "global_s": g, "rss_closed_kb": closed * mib, "growth_kb": grown * mib}
                  for n, g, closed, grown in ((1, 0.0, 400, 300), (2, 0.5, 410, 10), (3, None, 420, 10))]
        summary = {"name": DATE, "verdict": "issue", "reasons": [], "kind": "night",
                   "candidate": {"label": "develop", "sha": "d" * 40}, "base": {"label": "v0.3.2", "sha": "b" * 40},
                   "series": f"night-{DATE}", "started_at": "s", "finished_at": "f", "elapsed_s": 60.0,
                   "deadline": None, "noise": {"status": "current"}, "not_compared": 0, "skipped": [],
                   "refused": {}, "close_memory": [{"label": "УТ", "duration_s": 60.0, "estimate_s": 520.0,
                                                   "outcome": "issue", "cycles": cycles, "rss_ready_kb": 100 * mib,
                                                   "reason": None}]}
        out = self.enterContext(tempfile.TemporaryDirectory(prefix="perf-nightly-report-"))
        night.write_report(out, summary, [], None)
        with open(os.path.join(out, "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("| 1 | 1.000 | 0.000 | 400.0 | +300.0 | +300.0 |", report)
        self.assertIn("| 2 | 1.000 | 0.500 | 410.0 | +310.0 | +10.0 |", report)
        self.assertIn("| 3 | 1.000 | - | 420.0 | +320.0 | +10.0 |", report)
        self.assertIn("- исход: issue perf", report)

    def test_the_outcome_of_the_section_is_the_worst_row_of_the_scenario(self):
        # The first row of the scenario is ok, a later limit is exceeded.
        session = SimpleNamespace(durations={}, estimates=night.Estimates(None))
        manifest = {"corpora": {"ut": {"label": "УТ"}}}

        def rows(*outcomes):
            metrics = ("rss_growth_max_kb", "rss_over_ready_max_kb", "global_reopen_s")
            return [{"corpus": "ut", "scenario": "lsp-close-memory", "metric": m, "outcome": o}
                    for m, o in zip(metrics, outcomes)] + [
                {"corpus": "ut", "scenario": "analyze", "metric": "wall_s", "outcome": "block"}]

        def judged(found_rows):
            [item] = night.close_memory(manifest, ["ut"], {}, found_rows, session)
            return item["outcome"]

        self.assertEqual(judged(rows("ok", "issue", "ok")), "issue")
        self.assertEqual(judged(rows("ok", "ok", "issue")), "issue")
        self.assertEqual(judged(rows("ok", "ok", "ok")), "ok")
        self.assertEqual(judged(rows("issue", "incomplete", "incomplete")), "issue")
        self.assertEqual(judged([]), "incomplete")

    def test_without_its_warm_lsp_session_it_is_skipped(self):
        clock = AdvancingClock(0.0)
        session = night.Session(argparse.Namespace(corpora=None), clock, self.data, ["crm"],
                                night.Window(clock, 0.0), night.Estimates(None))
        with patch.object(night, "SUITE", CLOSE_SET):
            session.suite([("base", self.paths["v0.3.2"]), ("candidate", self.paths["develop"])], "s")
        self.assertEqual([(s["role"], s["scenario"]) for s in session.skipped][-1],
                         ("candidate", "lsp-close-memory"))
        self.assertNotIn(("base", "lsp-close-memory"), [(s["role"], s["scenario"]) for s in session.skipped])
        self.assertEqual(self.measured(), [])

    def test_a_failed_lsp_session_skips_it_and_keeps_the_rest_of_the_corpus(self):
        set_mode("develop=help_warn_lsp")
        clock = FakeClock()
        session = night.Session(argparse.Namespace(corpora=None), clock, self.data, ["crm"],
                                night.Window(clock, None), night.Estimates(None))
        units = (*CLOSE_SET, ("analyze", ("crm",)))
        with patch.object(night, "SUITE", units):
            session.suite([("candidate", self.paths["develop"])], "s")
        self.assertEqual(session.refused, {})
        self.assertEqual([s["scenario"] for s in session.skipped], ["lsp-close-memory"])
        self.assertEqual(self.series("develop", "s", "lsp-close-memory"), [])
        self.assertEqual(len([n for n in self.series("develop", "s") if n.endswith(".json")]), 3)

    def test_calibration_neither_measures_nor_needs_it(self):
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        with patch.object(night, "SUITE", CLOSE_SET):
            summary = self.night()
        self.assertEqual(summary["calibration"]["complete"], ["cal-1", "cal-2"])
        self.assertEqual(self.series("v0.3.2", "cal-1", "lsp-close-memory"), [])

    def test_the_nightly_set_fits_the_window_and_the_scenario_adds_at_most_ten_minutes(self):
        estimates = night.Estimates(None)
        both = estimates.series("base", ALL) + estimates.series("candidate", ALL)
        window = night.seconds_until("08:45", datetime.datetime(2026, 10, 22, 5, 0))
        self.assertLessEqual(both, window)
        with patch.object(night, "SUITE", tuple(u for u in night.SUITE if u[0] != "lsp-close-memory")):
            without = estimates.series("base", ALL) + estimates.series("candidate", ALL)
        self.assertEqual(estimates.series("base", ALL) * 2, without)
        self.assertLessEqual(both - without, 600)
        self.assertEqual(both - without, night.UNIT_SECONDS[("lsp-close-memory", "ut")])

    def test_the_scenario_still_costs_the_night_520_seconds(self):
        # The global query adds no run and no estimate: before any night is measured the
        # scenario adds the same 520 s as before.
        estimates = night.Estimates(None)
        with patch.object(night, "SUITE", tuple(u for u in night.SUITE if u[0] != "lsp-close-memory")):
            without = estimates.series("base", ALL) + estimates.series("candidate", ALL)
        self.assertEqual(estimates.series("base", ALL) + estimates.series("candidate", ALL) - without, 520.0)


def analyze_runs(*counts, held=True):
    return [{"held": held, "metrics": {"total_diagnostics": n}} for n in counts]


UT = ("ut", "analyze", "warm")


class DiagCounts(unittest.TestCase):
    """The number of `analyze` diagnostics on UT against the previous night: a note, not a verdict."""

    def test_the_candidate_is_compared_with_the_previous_night_not_with_the_base(self):
        base = {UT: analyze_runs(5, 5, 5)}
        cand = {UT: analyze_runs(2, 2, 2)}
        changed = night.diag_counts(base, cand, {"ut": (3, "2026-10-21")})["ut"]
        self.assertEqual(changed, {"candidate": 2, "values": [2], "base": 5, "previous": 3,
                                   "previous_night": "2026-10-21", "state": "changed"})
        same = night.diag_counts(base, cand, {"ut": (2, "2026-10-21")})["ut"]
        self.assertEqual(same["state"], "same")

    def test_unstable_runs_no_previous_record_and_no_held_run(self):
        base = {UT: analyze_runs(5, 5, 5)}
        unstable = night.diag_counts(base, {UT: analyze_runs(2, 3, 2)}, {"ut": (2, "2026-10-21")})["ut"]
        self.assertEqual((unstable["state"], unstable["values"], unstable["candidate"]), ("unstable", [2, 3], None))
        fresh = night.diag_counts(base, {UT: analyze_runs(2, 2, 2)}, {})["ut"]
        self.assertEqual((fresh["state"], fresh["previous"], fresh["previous_night"]), ("no_previous", None, None))
        failed = night.diag_counts(base, {UT: analyze_runs(2, 2, 2, held=False)}, {"ut": (2, "2026-10-21")})["ut"]
        self.assertEqual((failed["state"], failed["candidate"]), ("missing", None))
        self.assertEqual(night.diag_counts({}, {}, {}), {})

    def test_only_the_ut_count_is_followed(self):
        other = {("crm", "analyze", "warm"): analyze_runs(2, 2, 2), ("erp", "analyze", "warm"): analyze_runs(2, 2, 2)}
        self.assertEqual(night.diag_counts(other, other, {"crm": (3, "2026-10-21")}), {})

    def test_the_report_marks_a_changed_count_and_no_other_state(self):
        states = {"changed": (2, "2026-10-21", "**изменилось**"), "same": (2, "2026-10-21", "без изменений"),
                  "unstable": (None, None, "нестабильно между прогонами"),
                  "no_previous": (None, None, "нет прошлой записи"),
                  "missing": (None, None, "нет удержанного прогона кандидата")}
        counts = {state: {"candidate": 2 if state in ("changed", "same") else None, "values": [2], "base": 5,
                          "previous": before, "previous_night": night_date, "state": state}
                  for state, (before, night_date, _) in states.items()}
        summary = {"name": DATE, "verdict": "ok", "reasons": [], "kind": "night",
                   "candidate": {"label": "develop", "sha": "d" * 40}, "base": {"label": "v0.3.2", "sha": "b" * 40},
                   "series": f"night-{DATE}", "started_at": "s", "finished_at": "f", "elapsed_s": 60.0,
                   "deadline": None, "noise": {"status": "current"}, "not_compared": 0, "skipped": [],
                   "refused": {}, "diag_counts": counts}
        out = self.enterContext(tempfile.TemporaryDirectory(prefix="perf-nightly-report-"))
        night.write_report(out, summary, [], None)
        with open(os.path.join(out, "report.md"), encoding="utf-8") as fh:
            lines = {line.split(" | ")[0].lstrip("| "): line for line in fh.read().splitlines() if line.startswith("| ")}
        for state, (_, _, text) in states.items():
            with self.subTest(state=state):
                self.assertTrue(lines[state].endswith(f" | {text} |"), lines[state])
        self.assertEqual([state for state, line in lines.items() if "**" in line], ["changed"])


class DiagCountsHistory(NightCase):
    def write_night(self, date, **summary):
        write_json(self.data.path("nights", date, "summary.json"), {"state": "done", **summary})

    def test_nights_without_a_count_are_passed_over(self):
        self.write_night("2026-10-18", diag_counts={"crm": {"candidate": 9}})
        self.write_night("2026-10-19", diag_counts={"crm": {"candidate": 3}})
        self.write_night("2026-10-20", diag_counts={"crm": {"candidate": None}})
        self.write_night("2026-10-21")
        self.write_night("2026-10-21-rerun", diag_counts={"crm": {"candidate": 7}})
        self.write_night(DATE, diag_counts={"crm": {"candidate": 7}})
        with patch.object(night, "DIAG_COUNT_CORPORA", ("crm",)):
            self.assertEqual(night.previous_diag_counts(self.data, DATE), {"crm": (3, "2026-10-19")})

    def test_the_search_stops_at_the_first_night_with_a_count(self):
        # An older summary that no longer reads must not cost the night, as it does not for durations.
        os.makedirs(self.data.path("nights", "2026-10-01"))
        with open(self.data.path("nights", "2026-10-01", "summary.json"), "w") as fh:
            fh.write("{")
        self.write_night("2026-10-21", durations={"x": [1.0]}, diag_counts={"crm": {"candidate": 3}})
        with patch.object(night, "DIAG_COUNT_CORPORA", ("crm",)):
            self.assertEqual(night.previous_diag_counts(self.data, DATE), {"crm": (3, "2026-10-21")})
        self.assertEqual(night.previous_history(self.data, DATE), {"x": [1.0]})

    def test_a_night_reports_its_count_against_the_last_night_that_has_one(self):
        # D−2 recorded 3, D−1 has no count, this night measures 2 diagnostics (one per module).
        self.write_night("2026-10-20", diag_counts={"crm": {"candidate": 3}})
        self.write_night("2026-10-21", durations={})
        self.fresh_noise()
        with patch.object(night, "SUITE", (*CLOSE_SET, ("analyze", ("crm",)))), \
                patch.object(night, "DIAG_COUNT_CORPORA", ("crm",)):
            summary = self.night()
        self.assertEqual(summary["diag_counts"], {"crm": {
            "candidate": 2, "values": [2], "base": 2, "previous": 3, "previous_night": "2026-10-20",
            "state": "changed"}})
        # The verdict comes from the rows alone: a changed count adds no outcome and no reason.
        rows = read_json(self.data.path("nights", DATE, "compare.json"))["rows"]
        result = verdict.night_verdict(rows, diff_missing=summary["diff"]["not_built"])
        self.assertEqual((summary["verdict"], summary["reasons"]), (result["verdict"], result["reasons"]))
        with open(self.data.path("nights", DATE, "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("## Память после закрытия документов", report)
        self.assertIn("## Число диагностик", report)
        self.assertIn("| crm | 2 | 2 | 2 | 3 (2026-10-20) | **изменилось** |", report)
        self.assertEqual(read_json(self.data.path("nights", DATE, "summary.json"))["diag_counts"],
                         summary["diag_counts"])


class Suite(NightCase):
    def test_the_mcp_group_needs_its_warm_up(self):
        # A window that cuts the warm-up of mcp-memory leaves no warm cache: start and load are not run.
        self.fresh_noise()
        clock = AdvancingClock(0.0)
        window = night.Window(clock, 0.0)
        session = night.Session(argparse.Namespace(corpora=None), clock, self.data, ["crm"], window,
                                night.Estimates(None))
        with patch.object(night, "SUITE", (("mcp", ("crm",)),)):
            session.suite([("base", self.paths["v0.3.2"])], "s")
        self.assertEqual([s["scenario"] for s in session.skipped][-1], "mcp-start, mcp-load")
        self.assertEqual(self.series("v0.3.2", "s", "mcp-start"), [])
        self.assertEqual(self.measured(), [])

    def test_the_set_follows_the_plan(self):
        units = night.suite_units(["crm", "ut", "erp", "lombard"])
        self.assertEqual(units[:4], [("crm", "analyze"), ("crm", "lsp-session"),
                                     ("crm", "lsp-workspace-diag"), ("crm", "mcp")])
        self.assertEqual([u for c, u in units if c == "ut"],
                         ["analyze", "lsp-session", "lsp-close-memory", "lsp-workspace-diag", "mcp"])
        self.assertNotIn("lsp-close-memory", [u for _, u in night.suite_units(["ut"], "base")])
        self.assertEqual([u for c, u in units if c in ("erp", "lombard")], ["analyze", "analyze"])
        self.assertEqual(night.unit_sdirs("crm", "mcp"), ["mcp-memory", "mcp-start", "mcp-load-c2",
                                                          "mcp-load-c4"])
        self.assertEqual(night.unit_sdirs("ut", "mcp"), ["mcp-memory", "mcp-start", "mcp-load-c4"])
