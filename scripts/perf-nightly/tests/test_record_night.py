"""The night against the record (plan docs/plans/perf-ratchet-record.md, И08
and the part of И10 that the night measures): six cold runs of develop inside
the window of the night, whatever the warm calibration is, a result that is
kept for the date and that the verdict of the night cannot lose. The clock is
virtual, the machine scripted (`gate_fixtures.py`) and `gh` the fake of
`fake_gh.py`."""

import argparse
import contextlib
import datetime
import fcntl
import io
import os
import types
import unittest
from unittest.mock import patch

from gate_fixtures import CAND_SHA, CONFIG, MODES, GateCase, Script, ScriptedMachine
from helpers import FakeClock, FakeGh
from record_fixtures import RecordCase, edit_json, set_record_in
from test_night import DATE, SMALL, NightCase

import gate
import night
import perf_nightly
import pio_module
import publish
import verdict
from common import HARNESS_SCHEMA, read_json, write_json

REAL_RECORD_NIGHT = gate.record_night
A_ESTIMATE, B_ESTIMATE = gate.RECORD_RUN_ESTIMATE_S["A"], gate.RECORD_RUN_ESTIMATE_S["B"]
# What the scripted runs take on the virtual clock: readiness 100 s and the pauses of the protocol.
A_RUN_S, B_RUN_S = 210.0, 180.0
NAMES = ["A-c1", "A-c2", "A-c3", "B-c1", "B-c2", "B-c3"]


class RecordNight(RecordCase):
    """`gate.record_night` on its own."""

    def measure(self, date=DATE, seconds=None, machine=None, clock=None):
        cand = perf_nightly.load_binary(self.cand_path)
        args = argparse.Namespace(baseline=self.baseline_path, corpora=None)
        clock = clock or self.clock
        return gate.record_night(args, clock, self.data, date, cand, night.Window(clock, seconds),
                                 machine or self.machine)

    def saved(self, result) -> dict:
        return read_json(self.data.path(*result["summary"].split("/")[1:]))

    def mcp_launches(self, label=None):
        return [launch for launch in self.launches() if launch["argv"][:2] == ["mcp", "serve"]
                and label in (None, launch["env"]["FAKE_LABEL"])]

    def test_a_night_runs_a_and_then_b_of_the_candidate_alone_into_its_own_directory(self):
        result = self.measure()
        self.assertEqual((result["verdict"], result["record_verdict"], result["reasons"], result["reused"]),
                         ("ok", "pass", [], False))
        name = f"record-night-{DATE}-{CAND_SHA[:12]}"
        self.assertEqual((result["summary"], result["report"]),
                         (f"data/gates/{name}/summary.json", f"data/gates/{name}/report.md"))
        saved = self.saved(result)
        self.assertEqual(saved["mode"], "record-night")
        self.assertEqual([r["run"] for r in saved["runs"]], NAMES)
        self.assertEqual([(r["series"], r["role"], r["held"]) for r in saved["runs"]],
                         [("A", "candidate", True)] * 3 + [("B", "candidate", True)] * 3)
        self.assertEqual(len(self.mcp_launches()), 6)
        self.assertEqual({launch["env"]["FAKE_LABEL"] for launch in self.launches()}, {"cand"}, "no base, no build")
        self.assertEqual(self.builds, [])
        self.assertEqual(sorted(r.series for r in self.script.runs), ["A"] * 3 + ["B"] * 3)
        for absent in ("base", "answers", "record_proposals", "record_confirmation", "memory_base"):
            self.assertNotIn(absent, saved, "a night compares no release and proposes nothing")
        self.assertEqual(saved["verdict"], "ok", "a night's top verdict never says `pass`")
        self.assertEqual([r["metric"] for r in saved["record"]["rows"]], list(verdict.GATE_RECORD))
        self.assertEqual([r["metric"] for r in saved["ceilings"]],
                         ["hwm_kb", "references_1_s", "references_2_same_s", "references_3_other_s"])
        self.assertEqual(saved["candidate"], {"label": "cand", "sha": CAND_SHA})
        self.assertEqual(saved["artifacts"], {"summary": result["summary"], "report": result["report"]})
        self.assertEqual({k: saved["durations"][k] for k in ("A", "B")}, {"A": [A_RUN_S] * 3, "B": [B_RUN_S] * 3})
        report = self.read_report(name)
        self.assertIn("режим: ночная проверка рекорда, не гейт релиза", report)
        self.assertIn("## Рекорд", report)
        self.assertNotIn("## Прошлый релиз", report)
        # The lock of the gate is released for the release gate.
        with open(self.data.path("gate.lock"), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def read_report(self, name):
        with open(self.data.path("gates", name, "report.md"), encoding="utf-8") as fh:
            return fh.read()

    def test_a_night_neither_needs_the_calibration_of_the_gate_nor_writes_the_record(self):
        os.remove(self.data.path("gate-noise.json"))
        before = self.baseline_bytes()
        result = self.measure()
        self.assertEqual((result["verdict"], result["record_verdict"]), ("ok", "pass"))
        self.assertEqual(self.baseline_bytes(), before)

    def test_a_night_over_the_record_is_a_block_that_names_the_excess(self):
        self.set_record("graph_ready_cpu_s", 30.0, 0.01)
        result = self.measure()
        self.assertEqual((result["verdict"], result["record_verdict"]), ("block", "block"))
        self.assertIn("block: record:graph_ready_cpu_s", result["reasons"])
        row = next(r for r in self.saved(result)["record"]["rows"] if r["metric"] == "graph_ready_cpu_s")
        self.assertEqual((row["outcome"], row["candidate"]["value"]), ("block", 40.0))
        self.assertAlmostEqual(row["delta"], 1 / 3)

    def test_a_window_that_ends_before_the_series_b_leaves_the_record_incomplete(self):
        # Three runs of A (each estimated at 319.58 s) end by 630 s on the clock; B would end at 1031.89 s.
        result = self.measure(seconds=900.0)
        self.assertEqual((result["verdict"], result["record_verdict"]), ("incomplete", "incomplete"))
        self.assertIn("window_exhausted", result["reasons"])
        saved = self.saved(result)
        self.assertEqual([r["run"] for r in saved["runs"]], NAMES[:3])
        self.assertEqual(set(saved["durations"]), {"A"}, "a series of fewer than three held runs sets no estimate")
        rows = {r["metric"]: r for r in saved["record"]["rows"]}
        self.assertEqual(rows["graph_ready_cpu_s"]["outcome"], "ok", "what was measured is compared")
        for metric in ("references_1_s", "references_2_same_s", "references_3_other_s", "hwm_kb"):
            self.assertEqual((rows[metric]["outcome"], rows[metric]["reason"]),
                             ("incomplete", f"record_missing:{metric}"), metric)
        self.assertEqual(saved["record"]["verdict"], "incomplete")

    def test_the_estimate_of_the_next_night_is_the_longest_held_run_of_the_night_before(self):
        # The runs of the first night take 210, 230 and 220 s in series A and 180, 170 and 185 s in series B.
        self.script.values["wall"] = lambda run: ((100.0, 120.0, 110.0) if run.series == "A" else (100.0, 90.0, 105.0))[run.n - 1]
        first = self.measure(date="2026-10-21")
        self.assertEqual(self.saved(first)["durations"], {"A": [210.0, 230.0, 220.0], "B": [180.0, 170.0, 185.0]})
        print_ = self.fingerprint()
        self.assertEqual(gate.record_estimates(self.data, "2026-10-22", print_), {"A": 230.0, "B": 185.0})
        self.assertEqual(gate.record_estimates(self.data, "2026-10-21", print_), gate.RECORD_RUN_ESTIMATE_S,
                         "the night itself and the later ones are no history")
        self.assertEqual(gate.record_estimates(self.data, "2026-10-22", {**print_, "kernel": "7.0"}),
                         gate.RECORD_RUN_ESTIMATE_S, "another profile has no history either")
        # A window of 990 s, the runs of this night taking 210 and 180 s: by the longest runs of the night before
        # (230 and 185 s) the first B ends by 810 s and the second would end at 995 s; by the shortest (210 and
        # 170 s) it would fit, by the defaults not even the first B would.
        self.script.values["wall"] = 100.0
        second = self.measure(date="2026-10-22", seconds=990.0)
        self.assertEqual([r["run"] for r in self.saved(second)["runs"]], NAMES[:4])
        self.assertIn("window_exhausted", second["reasons"])
        third = self.measure(date="2026-10-23", seconds=990.0, machine=self.other_profile())
        self.assertEqual([r["run"] for r in self.saved(third)["runs"]], NAMES[:3],
                         "the defaults of the first night on this profile: B would end after the window")

    def other_profile(self):
        machine = ScriptedMachine(self.script, self.clock, print_={**self.machine.print, "kernel": "7.0"})
        return machine

    def test_a_series_is_not_started_when_its_estimate_does_not_fit_and_a_wait_for_quiet_is_not_unbounded(self):
        # The machine is quiet only from the fifth question. Each wait costs 30 s of the window: after three of
        # them the first run (319.58 s) no longer fits, so the fourth wait is never made.
        self.machine.quiet_of = lambda n: n > 4
        result = self.measure(seconds=400.0)
        self.assertEqual(result["reasons"][:1], ["window_exhausted"])
        self.assertEqual(self.clock.sleeps, [30.0, 30.0, 30.0])
        self.assertEqual((self.launches(), self.saved(result)["runs"]), ([], []))

    def test_a_window_that_fits_nothing_starts_nothing(self):
        result = self.measure(seconds=100.0)
        self.assertEqual((result["verdict"], result["reasons"][:1]), ("incomplete", ["window_exhausted"]))
        self.assertEqual(self.launches(), [])

    def test_a_machine_that_is_never_quiet_is_busy_not_green(self):
        self.machine.quiet_of = lambda n: False
        result = self.measure()
        self.assertEqual((result["verdict"], result["reasons"][:1]), ("incomplete", ["machine_busy"]))
        self.assertEqual(self.launches(), [])

    def test_a_release_gate_in_progress_is_not_disturbed(self):
        with open(self.data.path("gate.lock"), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.measure()
        self.assertEqual(result, {"verdict": "incomplete", "record_verdict": None, "reasons": ["gate_busy"],
                                  "summary": None, "report": None, "reused": False})
        self.assertEqual(self.launches(), [])
        self.assertFalse(os.path.exists(self.data.path("gates", f"record-night-{DATE}-{CAND_SHA[:12]}")))

    def test_an_unusable_source_is_the_reason_of_the_record(self):
        with open(os.path.join(self.erp, "f.txt"), "w") as fh:
            fh.write("changed\n")
        result = self.measure()
        self.assertEqual(result["verdict"], "incomplete")
        self.assertTrue(result["reasons"][0].startswith("source_mismatch"), result["reasons"])
        self.assertEqual((result["summary"], self.launches()), (None, []))

    def test_a_finished_date_is_reused_and_an_unfinished_directory_is_left_where_it_is(self):
        name = f"record-night-{DATE}-{CAND_SHA[:12]}"
        stray = self.data.path("gates", name, "runs", "A-c1", "r.json")
        write_json(stray, {"held": True})
        first = self.measure()
        self.assertEqual(first["summary"], f"data/gates/{name}-2/summary.json", "a new attempt, a suffixed directory")
        self.assertTrue(os.path.isfile(stray), "what the unfinished attempt left is kept")
        self.assertFalse(os.path.isfile(self.data.path("gates", name, "summary.json")))
        launches = len(self.launches())
        self.assertEqual(launches, 6)
        again = self.measure()
        self.assertEqual((again["reused"], again["summary"], again["verdict"]), (True, first["summary"], "ok"))
        self.assertEqual(len(self.launches()), launches, "the finished date is not measured again")

    def test_another_profile_is_measured_and_incomplete_with_the_difference_named(self):
        before = self.baseline_bytes()
        result = self.measure(machine=self.other_profile())
        self.assertEqual((result["verdict"], result["record_verdict"]), ("incomplete", "incomplete"))
        self.assertIn("record_profile_mismatch:kernel", result["reasons"])
        saved = self.saved(result)
        self.assertTrue(all(r["delta"] is None for r in saved["record"]["rows"]), "no number is set against the record")
        self.assertEqual(saved["record"]["profile"]["kernel"], "7.0")
        self.assertEqual(self.baseline_bytes(), before)

    def test_the_baseline_is_named_without_a_path_of_this_machine(self):
        saved = self.saved(self.measure())
        self.assertEqual(saved["baseline"], {"path": "baseline.json", "commit": self.rev(self.repo)})
        inside = os.path.join(self.repo, "scripts", "perf-nightly", "baseline.json")
        os.makedirs(os.path.dirname(inside))
        write_json(inside, read_json(self.baseline_path))
        self.baseline_path = inside
        saved = self.saved(self.measure(date="2026-10-23"))
        self.assertEqual(saved["baseline"]["path"], "scripts/perf-nightly/baseline.json")


class WithRecord(unittest.TestCase):
    """The verdict of the night with its record: block, issue, incomplete, ok."""

    def test_the_worse_of_the_warm_set_and_the_record_stands_and_no_reason_is_lost(self):
        order = ("ok", "incomplete", "issue", "block")
        record = {"reasons": ["window_exhausted", "block: record:hwm_kb"]}
        for warm in order:
            for found in ("ok", "incomplete", "block"):
                with self.subTest(warm=warm, record=found):
                    result = night.with_record({"verdict": warm, "reasons": ["w"], "counts": {}},
                                               {**record, "verdict": found})
                    expected = "block" if "block" in (warm, found) else "issue" if warm == "issue" else \
                        "incomplete" if "incomplete" in (warm, found) else "ok"
                    self.assertEqual(result["verdict"], expected)
                    self.assertEqual(result["reasons"], ["w", "record incomplete: window_exhausted",
                                                         "block: record:hwm_kb"])

    def test_a_night_without_a_record_is_what_it_was(self):
        result = {"verdict": "ok", "reasons": []}
        self.assertEqual(night.with_record(result, None), result)

    def test_a_finished_night_is_exit_zero_whatever_it_found(self):
        # pio_module would otherwise record a content block as a failed run.
        blocked = {"kind": "night", "name": DATE, "verdict": "block", "reasons": ["block: record:hwm_kb"],
                   "skipped": [], "not_compared": 0}
        args = perf_nightly.parser().parse_args(["night", "--data-dir", "/data", "--baseline", "/b.json", "--publish"])
        self.assertEqual((args.baseline, args.publish), ("/b.json", True))
        with patch.object(night, "run_night", lambda *a, **k: blocked), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(perf_nightly.cmd_night(args), 0)


class NightRecord(NightCase):
    """`night` with the record measured for real, on the ERP clone next to the crm of the warm set."""

    def setUp(self):
        super().setUp()
        self.erp = GateCase.make_repo(os.path.join(self.tmp, "erp"), "erp", CONFIG)
        with open(self.manifest_path, "a", encoding="utf-8") as fh:
            fh.write(f'\n[[corpus]]\nid = "erp"\nlabel = "ERP"\nsource_repo = "{self.erp}"\n'
                     f'commit = "{GateCase.rev(self.erp)}"\nmode = "full"\n')
        self.clock = FakeClock()
        self.script = Script(self.clock)
        self.machine = ScriptedMachine(self.script, self.clock)
        self.baseline_path = os.path.join(self.tmp, "baseline.json")
        GateCase.write_baseline(self)
        self.enterContext(patch.object(night.release_gate, "record_night", self.scripted_record_night))
        self.enterContext(patch.object(night, "SUITE", SMALL))

    def fingerprint(self) -> dict:
        return gate.fingerprint(self.machine, gate.check_source(self.data))

    def scripted_record_night(self, *args):
        """The record of the night on the scripted server; the warm set, which has no scripted values, runs on its own."""
        with contextlib.ExitStack() as stack:
            self.script.install(types.SimpleNamespace(enterContext=stack.enter_context))
            stack.enter_context(patch.dict(os.environ, {"FAKE_MODE": MODES}))
            return REAL_RECORD_NIGHT(*args)

    def run_night(self, deadline=None, date=DATE, publish=False):
        values = {"data_dir": self.data_dir, "corpora": None, "deadline": deadline, "candidate": None, "date": date,
                  "remote": None, "baseline": self.baseline_path, "publish": publish}
        with contextlib.redirect_stdout(io.StringIO()):
            return night.run_night(argparse.Namespace(**values), self.clock,
                                   now=datetime.datetime(2026, 10, 22, 5, 0), builder=self.builder,
                                   machine=self.machine)

    def record_launches(self):
        return [launch for launch in self.launches() if launch["argv"][:2] == ["mcp", "serve"]]

    def test_a_stale_warm_calibration_still_gets_six_cold_runs_and_a_record(self):
        write_json(self.data.path("noise.json"), {"harness_schema": HARNESS_SCHEMA - 1, "entries": {}})
        summary = self.run_night()
        self.assertEqual(summary["kind"], "calibration", "the warm set went to its calibration")
        record = summary["record"]
        self.assertEqual((record["record_verdict"], record["verdict"], record["reused"]), ("pass", "ok", False))
        launches = self.record_launches()
        self.assertEqual(len(launches), 6)
        self.assertEqual({launch["env"]["FAKE_LABEL"] for launch in launches}, {"develop"})
        saved = read_json(self.data.path(*record["summary"].split("/")[1:]))
        self.assertEqual([r["run"] for r in saved["runs"]], NAMES)
        self.assertEqual(saved["candidate"]["sha"], self.sha["develop"])
        self.assertEqual(summary["verdict"], "incomplete", "a stale calibration does not turn the night green")
        self.assertFalse(os.path.exists(self.data.path("gate-noise.json")))

    def test_a_record_excess_blocks_the_night_whatever_the_warm_set_says(self):
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01)
        summary = self.run_night()
        self.assertEqual(summary["kind"], "night")
        self.assertEqual((summary["verdict"], summary["record"]["verdict"]), ("block", "block"))
        self.assertIn("block: record:graph_ready_cpu_s", summary["reasons"])
        with open(self.data.path("nights", DATE, "report.md"), encoding="utf-8") as fh:
            report = fh.read()
        self.assertIn("## Рекорд", report)
        self.assertIn("- итог: block", report)

    def test_a_night_whose_record_did_not_compare_is_not_ok_whatever_the_warm_set_says(self):
        self.fresh_noise()
        self.assertEqual(self.run_night(date="2026-10-22")["record"]["record_verdict"], "pass")
        edit_json(self.baseline_path, lambda doc: doc["profile"].update(kernel="other"))
        missing = self.run_night(date="2026-10-23")
        self.assertEqual(missing["record"]["record_verdict"], "incomplete")
        self.assertNotEqual(missing["verdict"], "ok")
        self.assertIn("record incomplete: record_profile_mismatch:kernel", missing["reasons"])

    def test_the_record_shares_the_window_with_the_warm_set_and_is_incomplete_when_it_runs_out(self):
        self.fresh_noise()
        # 1200 s from 05:00: A×3 end by 630 s, the first B by 810 s, the second would end at 1211.89 s.
        summary = self.run_night(deadline="05:20")
        record = summary["record"]
        self.assertEqual((record["verdict"], record["record_verdict"]), ("incomplete", "incomplete"))
        self.assertIn("window_exhausted", record["reasons"])
        self.assertEqual(len(self.record_launches()), 4)
        self.assertNotEqual(summary["verdict"], "ok")
        self.assertIn("record incomplete: window_exhausted", summary["reasons"])
        kept = read_json(self.data.path("nights", DATE, "summary.json"))["record"]
        self.assertEqual(kept, record, "the warm set that follows does not overwrite the record")

    def test_a_done_night_is_not_measured_again_and_a_finished_record_is_not_either(self):
        self.fresh_noise()
        first = self.run_night()
        launches = len(self.launches())
        self.assertEqual(len(self.record_launches()), 6)
        self.assertEqual(self.run_night(), first)
        self.assertEqual(len(self.launches()), launches)
        # The night's summary is gone but the record's directory is finished: the record is read, not measured.
        os.remove(self.data.path("nights", DATE, "summary.json"))
        again = self.run_night()
        self.assertEqual((again["record"]["reused"], again["record"]["summary"]), (True, first["record"]["summary"]))
        self.assertEqual(len(self.record_launches()), 6)

    def test_a_record_that_cannot_be_measured_does_not_cancel_the_warm_set(self):
        self.fresh_noise()
        # A release gate holds the lock of the gate: the record is incomplete and the night goes on.
        with open(self.data.path("gate.lock"), "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy = self.run_night(date="2026-10-22")
        self.assertEqual((busy["record"]["reasons"], busy["record"]["summary"]), (["gate_busy"], None))
        self.assertEqual(busy["kind"], "night")
        self.assertTrue(os.path.isfile(self.data.path("nights", "2026-10-22", "compare.json")), "the warm set was compared")
        self.assertIn("record incomplete: gate_busy", busy["reasons"])
        self.assertNotEqual(busy["verdict"], "ok")
        # The clone of the source is not what the manifest names: the record says why, the warm set is not touched.
        with open(os.path.join(self.erp, "f.txt"), "w") as fh:
            fh.write("changed\n")
        changed = self.run_night(date="2026-10-23")
        self.assertTrue(changed["record"]["reasons"][0].startswith("source_mismatch"), changed["record"]["reasons"])
        self.assertTrue(os.path.isfile(self.data.path("nights", "2026-10-23", "compare.json")))
        self.assertEqual(self.record_launches(), [], "nothing of the record was measured on either night")

    def record_issues(self, gh) -> list[dict]:
        """The issues of the record: the warm set has its own, and which of them a night opens depends on the
        wall time that its real fake processes take."""
        return [i for i in gh.state()["issues"] if i["title"].startswith("perf: рекорд")]

    def test_a_published_night_opens_the_issue_with_the_range_and_the_notice_of_the_job_names_it(self):
        gh = FakeGh(self, self.tmp)
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01, sha=self.sha["v0.3.2"])
        summary = self.run_night(publish=True)
        self.assertEqual(summary["verdict"], "block")
        [created] = self.record_issues(gh)
        self.assertEqual(created["title"], "perf: рекорд превышен — graph_ready_cpu_s")
        self.assertIn(f"`{self.sha['v0.3.2']}..{self.sha['develop']}`", created["body"])
        self.assertNotIn("История разошлась", created["body"], "a release tag is an ancestor of develop")
        record = [e for e in summary["publish"] if e["key"].startswith("perf-ratchet/")]
        self.assertEqual([(e["action"], e["issue"], e["key"]) for e in record],
                         [("create", created["number"], "perf-ratchet/graph_ready_cpu_s")])
        [notice, *_] = pio_module.night_notices(DATE, self.data.path("nights", DATE), summary)
        self.assertIn("block", notice["text"])
        self.assertIn("record:graph_ready_cpu_s", notice["text"])
        self.assertIn(f"#{created['number']}", notice["text"])
        # The next night of the same excess comments on the same issue.
        self.run_night(date="2026-10-23", publish=True)
        [same] = self.record_issues(gh)
        self.assertEqual((same["number"], len(same["comments"])), (created["number"], 1))

    def test_a_record_that_is_not_an_ancestor_of_develop_is_told_so_in_the_issue(self):
        gh = FakeGh(self, self.tmp)
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01, sha="f" * 40)
        self.run_night(publish=True)
        [created] = self.record_issues(gh)
        self.assertIn(f"История разошлась: {'f' * 40} не предок {self.sha['develop']}", created["body"])

    def test_a_gh_error_of_the_record_reaches_the_notices_of_the_job(self):
        FakeGh(self, self.tmp, fail=["issues?state=all"])
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01)
        summary = self.run_night(publish=True)
        self.assertEqual(summary["verdict"], "block", "the measured excess stands whatever gh does")
        texts = [n["text"] for n in pio_module.night_notices(DATE, self.data.path("nights", DATE), summary)]
        self.assertEqual(len(texts), 2)
        self.assertIn(f"perf-nightly {DATE}: ошибка gh — ", texts[1])

    def test_a_night_run_again_over_a_finished_record_does_not_comment_twice(self):
        gh = FakeGh(self, self.tmp)
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01)
        self.run_night(publish=True)
        os.remove(self.data.path("nights", DATE, "summary.json"))       # the night was killed before its summary
        again = self.run_night(publish=True)
        self.assertTrue(again["record"]["reused"])
        record = [e for e in again["publish"] if e["key"].startswith("perf-ratchet/")]
        self.assertEqual([e["action"] for e in record], ["commented"])
        logged = [e for e in publish.read_log(self.data.path("nights", DATE)) if e["key"].startswith("perf-ratchet/")]
        self.assertEqual([e["action"] for e in logged], ["create", "commented"], "the log of the night keeps both")
        [issue] = self.record_issues(gh)
        self.assertEqual(issue["comments"], [])

    def test_a_night_that_is_not_published_does_not_call_gh(self):
        gh = FakeGh(self, self.tmp)
        self.fresh_noise()
        set_record_in(self.baseline_path, "graph_ready_cpu_s", 10.0, 0.01)
        self.run_night(publish=False)
        self.assertEqual(gh.calls(), [])


if __name__ == "__main__":
    unittest.main()
