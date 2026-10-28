"""The pio job module over a fake `night` and a fake `gh` (plan §10.2; И21,
И26, И32, И33, И54). The module runs as pio runs it: a process reading one
request on stdin, in a `<DATA>/src` clone of a local origin whose develop holds
the fake `perf_nightly.py`."""

import calendar
import datetime
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from helpers import FakeGh, mock_env, run_git, set_mode
from test_night import CLOSE_SET, SMALL, NightCase

import night
import perf_nightly
import publish
from common import read_json, write_json

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.dirname(HERE)
MODULE = os.path.join(HARNESS, "pio_module.py")
JOB_FILE = os.path.join(os.path.dirname(os.path.dirname(HARNESS)), "docs", "plans", "perf-nightly-s4",
                        "perf-nightly.toml")
TZ = "Europe/Moscow"
NOTICE_KEY = r"\d{4}-\d{2}-\d{2}:(verdict|failed|unfinished|gh-error:[1-9]\d*)"
# 2026-10-21 22:30 UTC is already 2026-10-22 01:30 in Moscow: the night is keyed by the local date.
SLOT = calendar.timegm(datetime.datetime(2026, 10, 21, 22, 30).timetuple())
DATE = "2026-10-22"
DAY = 24 * 3600
ISSUE_ROW = {"corpus": "crm", "scenario": "lsp-session", "temp": "warm", "metric": "G2.first_s", "call": "G2",
             "kind": "delta", "outcome": "issue", "base": {"state": "valid", "value": 1.0},
             "candidate": {"state": "valid", "value": 1.5}, "delta": 0.5, "noise": 0.05}


def checked_answer(case: unittest.TestCase, result: subprocess.CompletedProcess) -> dict:
    """The module's answer as the pio `Response` takes it: one JSON line, no diff in
    this slice so no runs (И21), notices exactly `{key, text}` keyed `<date>:<kind>` (И33)."""
    case.assertEqual(result.returncode, 0, result.stderr)
    lines = result.stdout.splitlines()
    case.assertEqual(len(lines), 1, result.stdout)
    response = json.loads(lines[0])
    # The pio `Response` and `Notice` are `deny_unknown_fields`: exactly these fields.
    case.assertEqual(set(response), {"state", "runs", "notices"})
    case.assertEqual(set(response["state"]), {"last_night", "verdict"})
    case.assertEqual(response["runs"], [])
    for notice in response["notices"]:
        case.assertEqual(set(notice), {"key", "text"})
        case.assertRegex(notice["key"], f"^{NOTICE_KEY}$")
        case.assertTrue(notice["text"].startswith(f"perf-nightly {notice['key'][:10]}: "), notice)
    return response


def done_summary(date: str, verdict: str = "ok", **kw) -> dict:
    return {"state": "done", "kind": "night", "name": date, "date": date, "verdict": verdict, "reasons": [],
            "not_compared": 0, "skipped": [], **kw}


class Module(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.tmp = self._tmp.name
        self.data = os.path.join(self.tmp, "data")
        self.origin = os.path.join(self.tmp, "origin")
        self.night_log = os.path.join(self.tmp, "night.jsonl")
        self.rows_path = os.path.join(self.tmp, "rows.json")
        self.rows([ISSUE_ROW])
        self.gh = FakeGh(self, self.tmp)
        mock_env(self, {"FAKE_HARNESS": HARNESS, "FAKE_NIGHT_LOG": self.night_log,
                        "FAKE_NIGHT_ROWS": self.rows_path, "FAKE_NIGHT_VERDICT": "issue",
                        "FAKE_NIGHT_EXIT": "", "TZ": TZ})
        os.makedirs(os.path.join(self.origin, "scripts", "perf-nightly"))
        run_git("init", "-q", "-b", "develop", cwd=self.origin)
        run_git("config", "user.email", "t@t", cwd=self.origin)
        run_git("config", "user.name", "t", cwd=self.origin)
        self.develop_commit("night")
        run_git("clone", "-q", self.origin, os.path.join(self.data, "src"), cwd=self.tmp)
        self.develop_commit("night, a commit later")

    def tearDown(self):
        self._tmp.cleanup()

    def develop_commit(self, message):
        target = os.path.join(self.origin, "scripts", "perf-nightly", "perf_nightly.py")
        shutil.copy(os.path.join(HERE, "fixtures", "fake_night.py"), target)
        with open(target, "a") as fh:
            fh.write(f"# {message}\n")
        run_git("add", "-A", cwd=self.origin)
        run_git("commit", "-q", "-m", message, cwd=self.origin)

    def rows(self, rows):
        with open(self.rows_path, "w", encoding="utf-8") as fh:
            json.dump(rows, fh)

    def request(self, slot=SLOT, **kw):
        return {"protocol": 1, "job": "perf-nightly", "params": {"corpora": "corpora.toml", "deadline": "08:45"},
                "state": None, "slot": slot, "now": (slot or SLOT) + 60, "data_dir": self.data, **kw}

    def run_module(self, request=None, command=None, **kw) -> subprocess.CompletedProcess:
        return subprocess.run(command or [sys.executable, MODULE], input=json.dumps(request or self.request(**kw)),
                              capture_output=True, text=True)

    def answer(self, **kw) -> dict:
        return checked_answer(self, self.run_module(**kw))

    def seed(self, date, summary=None, log=()):
        """A night the module did not run: its summary, and the lines of its `publish.log`."""
        night_dir = self.night_dir(date)
        write_json(os.path.join(night_dir, "summary.json"), summary or done_summary(date))
        for entry in log:
            publish.Log(night_dir).add(*entry)

    def nights(self) -> list:
        if not os.path.exists(self.night_log):
            return []
        with open(self.night_log) as fh:
            return [json.loads(line) for line in fh]

    def night_dir(self, date=DATE):
        return os.path.join(self.data, "nights", date)

    def files(self, root):
        found = {}
        for dirpath, _, names in os.walk(root):
            for name in names:
                with open(os.path.join(dirpath, name), "rb") as fh:
                    found[os.path.relpath(os.path.join(dirpath, name), root)] = fh.read()
        return found

    def test_a_night_answers_one_json_with_its_verdict(self):
        # И33, И21: one JSON; the night's verdict under `<date>:verdict`; no diff, so no runs.
        response = self.answer()
        self.assertEqual(response["runs"], [])
        self.assertEqual(response["state"], {"last_night": DATE, "verdict": "issue"})
        [notice] = response["notices"]
        self.assertEqual(notice["key"], f"{DATE}:verdict")
        [created] = self.gh.state()["issues"]
        self.assertEqual(notice["text"], f"perf-nightly {DATE}: issue, issue: #{created['number']}, diff: -, "
                                         f"не сравнено: 2, пропущено: 1")
        self.assertEqual(self.nights(), [["night", "--data-dir", self.data, "--corpora",
                                          os.path.join(self.data, "corpora.toml"), "--deadline", "08:45",
                                          "--date", DATE, "--publish"]])
        src = os.path.join(self.data, "src")
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, capture_output=True, text=True).stdout
        tip = subprocess.run(["git", "rev-parse", "develop"], cwd=self.origin, capture_output=True, text=True).stdout
        self.assertEqual(head, tip, "the night runs the harness of origin/develop")

    def test_the_verdict_names_the_reasons_of_an_incomplete_night(self):
        self.rows([])
        mock_env(self, {"FAKE_NIGHT_VERDICT": "incomplete"})
        response = self.answer()
        self.assertEqual(response["notices"][0]["text"],
                         f"perf-nightly {DATE}: incomplete, issue: нет, diff: -, не сравнено: 2, пропущено: 1")
        self.assertEqual(self.gh.calls(), [], "no flagged row, no gh")

    def test_a_repeated_night_measures_and_publishes_nothing(self):
        # И32
        first = self.answer()
        calls = len(self.gh.calls())
        self.assertGreater(calls, 0)
        again = self.answer()
        self.assertEqual(again, first)
        self.assertEqual(len(self.nights()), 1)
        self.assertEqual(len(self.gh.calls()), calls)

    def test_a_failed_night_is_reported_and_runs_again(self):
        # И26: `night` exits 1 — `failed` with its last stderr line, and no verdict.
        mock_env(self, {"FAKE_NIGHT_EXIT": "1"})
        response = self.answer()
        reason = "perf_nightly: refused: build_failed: cargo build exited 101"
        self.assertEqual(response["notices"], [{"key": f"{DATE}:failed", "text": f"perf-nightly {DATE}: сбой — {reason}"}])
        self.assertEqual(response["state"], {"last_night": DATE, "verdict": "failed"})
        self.assertEqual(read_json(os.path.join(self.night_dir(), "summary.json")),
                         {"state": "failed", "date": DATE, "reason": reason})
        with open(os.path.join(self.night_dir(), "night-stderr.txt")) as fh:
            self.assertIn("building develop", fh.read())
        mock_env(self, {"FAKE_NIGHT_EXIT": ""})
        again = self.answer()
        self.assertEqual(len(self.nights()), 2, "a failed night is not done")
        self.assertEqual([n["key"] for n in again["notices"]], [f"{DATE}:verdict"])

    def test_a_failed_checkout_fails_the_night_before_it_runs(self):
        run_git("remote", "set-url", "origin", os.path.join(self.tmp, "gone"), cwd=os.path.join(self.data, "src"))
        response = self.answer()
        [notice] = response["notices"]
        self.assertEqual(notice["key"], f"{DATE}:failed")
        self.assertEqual(self.nights(), [])

    def test_without_git_the_night_fails_with_the_reason(self):
        mock_env(self, {"PATH": os.path.join(self.tmp, "empty")})
        response = self.answer()
        [notice] = response["notices"]
        self.assertEqual(notice["key"], f"{DATE}:failed")
        self.assertIn("git fetch: [Errno 2]", notice["text"])
        self.assertEqual(read_json(os.path.join(self.night_dir(), "summary.json"))["state"], "failed")

    def test_a_night_killed_before_its_summary_is_reported_the_next_night(self):
        # И26: the job dies with its night; the next night reports it as unfinished.
        mock_env(self, {"FAKE_NIGHT_EXIT": "kill"})
        self.assertNotEqual(self.run_module().returncode, 0)
        self.assertEqual(read_json(os.path.join(self.night_dir(), "summary.json")),
                         {"state": "running", "date": DATE})
        mock_env(self, {"FAKE_NIGHT_EXIT": ""})
        response = self.answer(slot=SLOT + DAY)
        self.assertEqual([n["key"] for n in response["notices"]], [f"{DATE}:unfinished", "2026-10-23:verdict"])
        self.assertEqual(response["notices"][0]["text"], f"perf-nightly {DATE}: ночь не завершена")

    def test_two_killed_nights_are_both_reported(self):
        self.answer(slot=SLOT - DAY)
        mock_env(self, {"FAKE_NIGHT_EXIT": "kill"})
        self.run_module()
        self.run_module(slot=SLOT + DAY)
        mock_env(self, {"FAKE_NIGHT_EXIT": ""})
        response = self.answer(slot=SLOT + 2 * DAY)
        self.assertEqual([n["key"] for n in response["notices"]],
                         [f"{DATE}:unfinished", "2026-10-23:unfinished", "2026-10-24:verdict"])

    def test_a_killed_retry_of_a_failed_night_is_unfinished(self):
        mock_env(self, {"FAKE_NIGHT_EXIT": "1"})
        self.answer()
        mock_env(self, {"FAKE_NIGHT_EXIT": "kill"})
        self.assertNotEqual(self.run_module().returncode, 0)
        mock_env(self, {"FAKE_NIGHT_EXIT": ""})
        response = self.answer(slot=SLOT + DAY)
        self.assertEqual([n["key"] for n in response["notices"]], [f"{DATE}:unfinished", "2026-10-23:verdict"])

    def test_a_finished_previous_night_leaves_nothing_behind(self):
        self.answer()
        response = self.answer(slot=SLOT + DAY)
        self.assertEqual([n["key"] for n in response["notices"]], ["2026-10-23:verdict"])
        mock_env(self, {"FAKE_NIGHT_EXIT": "1"})
        failed = self.answer(slot=SLOT + 2 * DAY)
        mock_env(self, {"FAKE_NIGHT_EXIT": ""})
        after_failed = self.answer(slot=SLOT + 3 * DAY)
        self.assertEqual([n["key"] for n in failed["notices"]], ["2026-10-24:failed"])
        self.assertEqual([n["key"] for n in after_failed["notices"]], ["2026-10-25:verdict"],
                         "a failed night reported itself and is not unfinished")

    def test_a_gh_error_is_a_notice_of_its_night_and_of_the_next(self):
        # И26: a `gh` error in the night's publication, again in the next night under the same key.
        self.gh.set([], ["issue create"])
        response = self.answer()
        error = {"key": f"{DATE}:gh-error:1",
                 "text": f"perf-nightly {DATE}: ошибка gh — issue create: HTTP 502: Bad Gateway"}
        self.assertEqual(response["notices"][1:], [error])
        self.assertIn("issue: нет", response["notices"][0]["text"])
        self.gh.set([], [])
        self.rows([])
        next_night = self.answer(slot=SLOT + DAY)
        self.assertEqual(next_night["notices"], [error, next_night["notices"][1]])
        self.assertEqual(next_night["notices"][1]["key"], "2026-10-23:verdict")

    def test_a_gate_of_the_same_date_leaves_the_night_answer(self):
        # И54: a gate writes `gates/<tag>/` only; the night's files and answer stay.
        first = self.answer()
        before = self.files(self.night_dir())
        gate = os.path.join(self.data, "gates", "v0.3.3")
        write_json(os.path.join(gate, "summary.json"), {"state": "done", "kind": "gate", "name": "v0.3.3",
                                                        "date": DATE, "verdict": "block"})
        publish.Log(gate).add("create", 99, "perf/x/run/m")
        self.assertEqual(self.answer(), first)
        self.assertEqual(self.files(self.night_dir()), before)

    def test_without_a_slot_the_night_is_the_date_of_now(self):
        response = self.answer(slot=None, now=SLOT + DAY)
        self.assertEqual(response["state"]["last_night"], "2026-10-23")

    def test_another_protocol_is_refused(self):
        result = self.run_module(protocol=2)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("протокол 2", result.stderr)
        self.assertEqual(self.nights(), [])

    def test_the_night_is_keyed_by_the_slot_not_by_a_later_now(self):
        # `now` is the start of the run: a slot at 01:30 local that starts a day later is still its own night.
        response = self.answer(slot=SLOT, now=SLOT + DAY)
        self.assertEqual(response["state"]["last_night"], DATE)
        self.assertEqual(self.nights()[0][self.nights()[0].index("--date") + 1], DATE)

    def test_the_night_date_is_the_local_date_of_the_machine(self):
        # The same instant is another date in another zone: the key is not UTC and not Moscow.
        for date in ("2026-10-21", DATE):
            self.seed(date)
        expected = {"Europe/Moscow": DATE, "Asia/Tokyo": DATE, "UTC": "2026-10-21", "America/Los_Angeles": "2026-10-21"}
        for zone, date in expected.items():
            with self.subTest(zone=zone):
                mock_env(self, {"TZ": zone})
                self.assertEqual(self.answer()["state"]["last_night"], date)

    def test_the_params_of_the_job_reach_night(self):
        # The defaults equal the request of the other tests: other values tell a read param from a default.
        self.answer(params={"corpora": "other.toml", "deadline": "07:30"})
        [argv] = self.nights()
        self.assertEqual(argv[argv.index("--corpora") + 1], os.path.join(self.data, "other.toml"))
        self.assertEqual(argv[argv.index("--deadline") + 1], "07:30")

    def test_the_argv_of_night_is_the_one_the_real_parser_takes(self):
        # The fake `night` takes whatever the module passes; the real one is the contract.
        self.answer()
        [argv] = self.nights()
        parsed = perf_nightly.parser().parse_args(argv)
        self.assertEqual((parsed.command, parsed.data_dir, parsed.corpora, parsed.deadline, parsed.date,
                          parsed.candidate, parsed.publish),
                         ("night", self.data, os.path.join(self.data, "corpora.toml"), "08:45", DATE, None, True))
        self.assertFalse(perf_nightly.parser().parse_args(["night", "--data-dir", self.data]).publish,
                         "a hand-run night publishes nothing")

    def test_pio_starts_the_module_through_the_symlink_of_its_modules_dir(self):
        link = os.path.join(self.tmp, "modules", "perf-nightly")
        os.makedirs(os.path.dirname(link))
        os.symlink(MODULE, link)
        response = checked_answer(self, self.run_module(command=[link]))
        self.assertEqual(response["state"], {"last_night": DATE, "verdict": "issue"})

    def test_a_failed_checkout_after_a_good_fetch_fails_the_night(self):
        mock_env(self, {"LC_ALL": "C"})
        run_git("branch", "-m", "develop", "trunk", cwd=self.origin)
        run_git("update-ref", "-d", "refs/remotes/origin/develop", cwd=os.path.join(self.data, "src"))
        response = self.answer()
        self.assertEqual([n["key"] for n in response["notices"]], [f"{DATE}:failed"])
        self.assertIn("origin/develop", response["notices"][0]["text"])
        self.assertEqual(self.nights(), [], "night does not run on an unchecked-out harness")
        self.assertEqual(read_json(os.path.join(self.night_dir(), "summary.json"))["state"], "failed")

    def test_a_done_night_runs_neither_git_nor_night(self):
        # И32: git is not even started — with no git the answer of the done night is the same.
        first = self.answer()
        mock_env(self, {"PATH": os.path.join(self.tmp, "empty")})
        self.assertEqual(self.answer(), first)
        self.assertEqual(len(self.nights()), 1)

    def test_a_previous_night_without_a_summary_is_unfinished(self):
        # И26: the night directory is there, `summary.json` is not.
        os.makedirs(self.night_dir("2026-10-21"))
        with open(os.path.join(self.night_dir("2026-10-21"), "report.md"), "w") as fh:
            fh.write("# half\n")
        response = self.answer()
        self.assertEqual([n["key"] for n in response["notices"]], ["2026-10-21:unfinished", f"{DATE}:verdict"])

    def test_unfinished_nights_before_the_last_done_night_are_left_alone(self):
        self.seed("2026-10-19", {"state": "running", "date": "2026-10-19"})
        self.seed("2026-10-20")
        self.seed("2026-10-21", {"state": "running", "date": "2026-10-21"})
        response = self.answer()
        self.assertEqual([n["key"] for n in response["notices"]], ["2026-10-21:unfinished", f"{DATE}:verdict"],
                         "2026-10-19 lies before the last done night, 2026-10-20")

    def test_every_gh_error_of_a_night_has_a_key_of_its_own(self):
        # Two errors, one text: the keys tell them apart, and a repeat gives the same keys.
        self.rows([ISSUE_ROW, dict(ISSUE_ROW, metric="G2.ready_s")])
        self.gh.set([], ["issue create"])
        response = self.answer()
        errors = [n for n in response["notices"] if ":gh-error:" in n["key"]]
        self.assertEqual([n["key"] for n in errors], [f"{DATE}:gh-error:1", f"{DATE}:gh-error:2"])
        self.assertEqual(self.answer(slot=SLOT + DAY)["notices"][:2], errors)

    def test_the_verdict_names_its_reason_and_the_issues_of_the_night(self):
        self.seed(DATE, done_summary(DATE, "incomplete", reasons=["noise_stale"], not_compared=5,
                                     skipped=[{"role": "base"}, {"role": "candidate"}]),
                  [("create", 12, "perf/a/run/m"), ("comment", 9, "perf/b/run/m"), ("commented", 9, "perf/b/run/m"),
                   ("gh-error", None, "perf/e/run/m", "issue create: boom")])
        response = self.answer()
        verdict, error = response["notices"]
        self.assertEqual(verdict["text"], f"perf-nightly {DATE}: incomplete (noise_stale), issue: #9, #12, "
                                          f"diff: -, не сравнено: 5, пропущено: 2")
        self.assertEqual(error, {"key": f"{DATE}:gh-error:1", "text": f"perf-nightly {DATE}: ошибка gh — issue create: boom"})
        self.assertEqual(self.nights(), [])

    def test_the_job_file_is_the_one_of_the_plan(self):
        with open(JOB_FILE, "rb") as fh:
            job = tomllib.load(fh)
        self.assertEqual({k: job[k] for k in ("id", "cron", "timeout", "params", "limits")},
                         {"id": "perf-nightly", "cron": "0 5 * * *", "timeout": "4h",
                          "params": {"corpora": "corpora.toml", "deadline": "08:45"},
                          "limits": {"concurrent": 1, "runs_per_day": 1}})
        self.assertEqual(len(job["command"]), 1)
        self.assertNotIn("tasks", job, "the triage template comes with slice 3")
        self.assertEqual(set(job), {"id", "command", "cron", "timeout", "params", "limits"})


class GateBesideNight(NightCase):
    """И54 on the real `night` over fake binaries: a gate of the date of a develop night
    leaves that night's files and the module's answer for the date as they were."""

    def setUp(self):
        super().setUp()
        self.gh = FakeGh(self, self.tmp)
        mock_env(self, {"TZ": TZ})
        set_mode("develop=leak", "v0.3.3=leak")
        self.fresh_noise()

    def ask(self) -> dict:
        request = {"protocol": 1, "job": "perf-nightly", "params": {}, "state": None, "slot": SLOT,
                   "now": SLOT + 60, "data_dir": self.data_dir}
        return checked_answer(self, subprocess.run([sys.executable, MODULE], input=json.dumps(request),
                                                   capture_output=True, text=True))

    def tree(self, root):
        found = {}
        for dirpath, _, names in os.walk(root):
            for name in names:
                with open(os.path.join(dirpath, name), "rb") as fh:
                    found[os.path.relpath(os.path.join(dirpath, name), root)] = fh.read()
        return found

    def test_a_gate_leaves_the_files_and_the_answer_of_the_night_of_its_date(self):
        with patch.object(night, "SUITE", CLOSE_SET):
            self.night(publish=True)
            first = self.ask()
            self.assertEqual([n["key"] for n in first["notices"]], [f"{DATE}:verdict"])
            self.assertEqual(first["state"]["last_night"], DATE)
            before = self.tree(self.data.path("nights", DATE))
            self.assertIn("publish.log", before, "the night published, so a gate that writes into it would show")
            self.gh.set(self.gh.state()["issues"], ["issue comment"])
            gate = self.night(candidate="v0.3.3", publish=True)
        self.assertEqual((gate["kind"], gate["name"]), ("gate", "v0.3.3"))
        self.assertTrue(os.path.isfile(self.data.path("gates", "v0.3.3", "summary.json")))
        gate_log = publish.read_log(self.data.path("gates", "v0.3.3"))
        self.assertIn("gh-error", [e["action"] for e in gate_log], "the gate published, and its gh failed")
        self.assertEqual(self.tree(self.data.path("nights", DATE)), before)
        self.assertEqual(self.ask(), first, "no notice of the gate, none of its gh errors")


class NightOverTheSummariesOfTheModule(NightCase):
    """The module writes `running` into the summary of the night before it starts `night`,
    and `failed` after a failed one: the real `night` is not to take either for a done night,
    and reads past them on later nights."""

    def setUp(self):
        super().setUp()
        self.fresh_noise()

    def seed(self, date, summary):
        write_json(self.data.path("nights", date, "summary.json"), summary)

    def test_a_night_runs_over_the_running_summary_the_module_left(self):
        self.seed("2026-10-22", {"state": "running", "date": "2026-10-22"})
        with patch.object(night, "SUITE", SMALL):
            summary = self.night()
        self.assertEqual((summary["state"], summary["measured"]), ("done", True))
        self.assertEqual(read_json(self.data.path("nights", "2026-10-22", "summary.json"))["state"], "done")
        self.assertTrue(self.series("develop", "night-2026-10-22"), "the night measured")

    def test_a_night_reads_past_the_failed_and_running_summaries_of_earlier_nights(self):
        self.seed("2026-10-20", {"state": "failed", "date": "2026-10-20", "reason": "build_failed"})
        self.seed("2026-10-21", {"state": "running", "date": "2026-10-21"})
        with patch.object(night, "SUITE", SMALL):
            summary = self.night()
        self.assertEqual(summary["state"], "done")
        self.assertTrue(self.series("develop", "night-2026-10-22"), "the night measured")


if __name__ == "__main__":
    unittest.main()
