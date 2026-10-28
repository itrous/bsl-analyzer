"""Publication of perf issues on a fake `gh` (plan §9.7, D10, D17; И27–И29,
И39, И41)."""

import os
import tempfile
import unittest
from unittest.mock import patch

from helpers import FakeGh, mock_env, set_mode
from test_night import CLOSE_SET, DATE, NightCase

import night
import publish
from common import HarnessError, read_json

LABELS = {"crm": "ЦРМ", "ut": "УТ"}
SUMMARY = {"kind": "night", "name": DATE,
           "candidate": {"label": "develop", "sha": "c" * 40}, "base": {"label": "v0.3.2", "sha": "b" * 40}}
G2 = "perf/lsp-session/G2/first_s"
L1 = "perf/lsp-session/L1/first_s"


def row(corpus_id, metric, outcome, call=None, scenario="lsp-session"):
    return {"corpus": corpus_id, "scenario": scenario, "temp": "warm", "metric": metric,
            "call": call or metric.split(".")[0], "kind": "delta", "outcome": outcome,
            "base": {"state": "valid", "value": 1.0}, "candidate": {"state": "valid", "value": 1.5},
            "delta": 0.5, "noise": 0.05}


def issue(number, key, state="OPEN", reason=None, labels=("perf", "regression"), comments=()):
    return {"number": number, "state": state, "stateReason": reason, "title": "t",
            "body": f"текст\n{publish.KEY_PREFIX}{key}\n", "labels": list(labels), "comments": list(comments)}


class Publish(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.out = os.path.join(self._tmp.name, "nights", DATE)
        self.gh = FakeGh(self, self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def publish(self, rows, summary=SUMMARY, forbidden=(), labels=LABELS, table=None, runs=lambda sdir: 3):
        return publish.publish_perf(self.out, summary, rows, table, labels, list(forbidden), runs)

    def lines(self, text):
        return [line.strip() for line in text.splitlines()]

    def writes(self):
        return [c["args"] for c in self.gh.calls() if c["args"][:2] != ["issue", "list"] and c["args"][0] != "api"]

    def test_one_issue_per_scenario_call_and_metric(self):
        # И39: G2 worse on two corpora is one issue; L1 worse on one is another.
        entries = self.publish([row("crm", "G2.first_s", "issue"), row("ut", "G2.first_s", "issue"),
                                row("crm", "L1.first_s", "issue"), row("ut", "L1.first_s", "ok"),
                                row("crm", "ready_s", "noisy", call="run")])
        self.assertEqual([(e["action"], e["key"]) for e in entries], [("create", G2), ("create", L1)])
        issues = {i["number"]: i for i in self.gh.state()["issues"]}
        g2 = issues[entries[0]["issue"]]
        self.assertIn(f"{publish.KEY_PREFIX}{G2}", g2["body"].splitlines())
        self.assertIn("| ЦРМ |", g2["body"])
        self.assertIn("| УТ |", g2["body"])
        self.assertEqual(g2["labels"], ["perf", "regression"])
        self.assertNotIn("| УТ |", issues[entries[1]["issue"]]["body"])

    def test_a_block_carries_the_release_blocker_label(self):
        self.publish([row("crm", "G2.first_s", "issue"), row("ut", "G2.first_s", "block")])
        [created] = self.gh.state()["issues"]
        self.assertEqual(created["labels"], ["perf", "regression", "release-blocker"])
        self.assertIn("блок релиза", created["body"])

    def test_an_open_issue_gets_one_comment_a_night(self):
        # И27: the next night comments; that night again comments no more.
        rows = [row("crm", "G2.first_s", "issue")]
        self.publish(rows)
        [created] = self.gh.state()["issues"]
        second = dict(SUMMARY, name="2026-10-23")
        entries = self.publish(rows, summary=second)
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("comment", created["number"])])
        [comment] = self.gh.state()["issues"][0]["comments"]
        self.assertIn(f"{publish.NIGHT_PREFIX}2026-10-23", comment.splitlines())
        again = self.publish(rows, summary=second)
        self.assertEqual([e["action"] for e in again], ["commented"])
        self.assertEqual(len(self.gh.state()["issues"]), 1)
        self.assertEqual(len(self.gh.state()["issues"][0]["comments"]), 1)

    def test_the_night_that_created_the_issue_does_not_comment_on_it(self):
        rows = [row("crm", "G2.first_s", "issue")]
        self.publish(rows)
        self.assertEqual([e["action"] for e in self.publish(rows)], ["commented"])
        self.assertEqual(self.gh.state()["issues"][0]["comments"], [])

    def test_an_issue_the_owner_closed_is_left_alone(self):
        for reason in publish.OWNER_CLOSED:
            with self.subTest(reason=reason):
                self.gh.set([issue(7, G2, "CLOSED", reason)], [])
                entries = self.publish([row("crm", "G2.first_s", "issue")])
                self.assertEqual([(e["action"], e["issue"]) for e in entries], [("closed", 7)])
                self.assertEqual(self.writes(), [])

    def test_a_completed_issue_that_came_back_is_a_new_issue(self):
        self.gh.set([issue(7, G2, "CLOSED", "COMPLETED")], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("create", 8)])
        self.assertIn("повтор #7", self.gh.state()["issues"][1]["body"].splitlines())

    def test_a_list_of_a_thousand_issues_publishes_nothing(self):
        self.gh.set([issue(n, f"perf/x/run/m{n}", "CLOSED", "COMPLETED") for n in range(1, 1001)], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([e["action"] for e in entries], ["gh-error"])
        self.assertEqual(self.writes(), [])
        self.assertEqual(publish.read_log(self.out), entries)

    def test_a_block_on_an_open_issue_adds_the_label_and_no_night_removes_it(self):
        # И41
        self.gh.set([issue(5, G2)], [])
        self.publish([row("crm", "G2.first_s", "block")])
        edits = [a for a in self.writes() if a[:2] == ["issue", "edit"]]
        self.assertEqual(edits, [["issue", "edit", "5", "-R", publish.REPO, "--add-label", "release-blocker"]])
        before = len(self.gh.calls())
        self.publish([row("crm", "G2.first_s", "ok")], summary=dict(SUMMARY, name="2026-10-23"))
        self.assertEqual(len(self.gh.calls()), before)
        self.assertFalse(any("--remove-label" in c["args"] for c in self.gh.calls()))

    def test_a_failed_comment_still_marks_a_block(self):
        for fail in ("api|comments", "issue comment"):
            with self.subTest(fail=fail):
                self.gh.set([issue(5, G2)], [fail])
                entries = self.publish([row("crm", "G2.first_s", "block")])
                self.assertEqual([e["action"] for e in entries], ["gh-error", "label"])
                self.assertIn("release-blocker", self.gh.state()["issues"][0]["labels"])

    def test_a_label_outside_the_set_is_refused(self):
        # И28
        with self.assertRaises(HarnessError):
            publish.check_labels(["perf", "bug"])

    def test_text_with_a_point_path_is_not_published(self):
        # И29: a corpus label that is a directory of a point leaks into the body.
        entries = self.publish([row("crm", "G2.first_s", "issue"), row("crm", "L1.first_s", "issue")],
                               forbidden=["Модули"], labels={"crm": "Модули"})
        self.assertEqual([e["action"] for e in entries], ["refused", "refused"])
        self.assertEqual(self.writes(), [])

    def test_a_directory_name_is_matched_as_a_whole_word(self):
        # One-letter and common directory names occur inside the words of every text.
        entries = self.publish([row("crm", "G2.first_s", "issue")], forbidden=["e", "Ext", "first"])
        self.assertEqual([e["action"] for e in entries], ["create"])
        self.assertEqual(publish.leaked("каталог Ext/Модуль.bsl", ["Ext", "Ext/Модуль.bsl"]),
                         ["Ext", "Ext/Модуль.bsl"])

    def test_a_gh_error_is_logged_and_the_other_groups_go_on(self):
        self.gh.set([], ["issue create|регрессия lsp-session G2 first_s"])
        entries = self.publish([row("crm", "G2.first_s", "issue"), row("crm", "L1.first_s", "issue")])
        self.assertEqual([(e["action"], e["key"]) for e in entries], [("gh-error", G2), ("create", L1)])
        self.assertIn("HTTP 502", entries[0]["detail"])
        self.assertEqual(publish.read_log(self.out), entries)

    def test_without_gh_or_with_its_garbage_the_night_goes_on(self):
        mock_env(self, {"PATH": os.path.join(self._tmp.name, "empty")})
        [missing] = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual(missing["action"], "gh-error")
        self.assertIn("No such file", missing["detail"])
        [garbage] = publish.publish_perf(self.out, SUMMARY, [row("crm", "G2.first_s", "issue")], None, LABELS,
                                         [], lambda sdir: 3, gh=lambda args, stdin=None: (0, "<html>", ""))
        self.assertEqual((garbage["action"], garbage["detail"]), ("gh-error", "issue list: output is not JSON"))

    def test_the_gate_marks_its_comment_with_its_tag(self):
        self.gh.set([issue(5, G2)], [])
        self.publish([row("crm", "G2.first_s", "issue")], summary=dict(SUMMARY, kind="gate", name="v0.3.3"))
        [comment] = self.gh.state()["issues"][0]["comments"]
        self.assertIn(f"{publish.NIGHT_PREFIX}gate-v0.3.3", comment.splitlines())

    def test_the_issue_list_is_one_call_a_night_with_the_query_of_the_plan(self):
        self.publish([row("crm", "G2.first_s", "issue"), row("ut", "G2.first_s", "issue"),
                      row("crm", "L1.first_s", "issue")])
        [args] = [c["args"] for c in self.gh.calls() if c["args"][:2] == ["issue", "list"]]
        option = {name: args[args.index(name) + 1] for name in ("-R", "--state", "--label", "-L", "--json")}
        self.assertEqual(option, {"-R": "itrous/bsl-analyzer", "--state": "all", "--label": "perf", "-L": "1000",
                                  "--json": "number,state,stateReason,body,labels"})

    def test_a_list_short_of_the_limit_is_published(self):
        self.gh.set([issue(n, f"perf/x/run/m{n}", "CLOSED", "COMPLETED") for n in range(1, 1000)], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("create", 1000)])

    def test_the_body_carries_the_versions_the_medians_the_trend_and_the_key(self):
        # Δ, noise, N, the temperature, the candidate and the base with their SHAs, the trend of the metric.
        trend = {"columns": ["v0.3.2-bbbbbbbbbbbb", "develop-cccccccccccc"],
                 "cells": {("crm", "lsp-session", "warm", "G2.first_s"): {
                     "v0.3.2-bbbbbbbbbbbb": {"value": 1.0, "state": "valid"},
                     "develop-cccccccccccc": {"value": 1.5, "state": "valid"}}}}
        self.publish([row("crm", "G2.first_s", "issue")], table=trend)
        [created] = self.gh.state()["issues"]
        body = self.lines(created["body"])
        for expected in ("Кандидат: develop cccccccccccc; база: v0.3.2 bbbbbbbbbbbb.",
                         "Температура: warm, N = 3.",
                         "| ЦРМ | 1.000 | 1.500 | +50.0% | 5.0% | issue perf |",
                         "Тренд:", "| конфигурация | v0.3.2-bbbbbbbbbbbb | develop-cccccccccccc |",
                         "| ЦРМ | 1.000 | 1.500 |",
                         f"{publish.KEY_PREFIX}{G2}", f"{publish.NIGHT_PREFIX}{DATE}"):
            self.assertIn(expected, body)

    def test_the_body_has_no_trend_without_a_table_and_n_and_temperature_are_of_the_group(self):
        runs = {"lsp-session": 3, "mcp-load-c4": 5}.__getitem__
        self.publish([row("crm", "G2.first_s", "issue"),
                      dict(row("crm", "M1.first_s", "issue", scenario="mcp-load-c4"), temp="cold")], runs=runs)
        first, second = (self.lines(i["body"]) for i in self.gh.state()["issues"])
        self.assertIn("Температура: warm, N = 3.", first)
        self.assertIn("Температура: cold, N = 5.", second)
        self.assertNotIn("Тренд:", first)

    def test_a_ceiling_row_shows_its_limit_and_a_side_without_a_value_its_state(self):
        ceiling = dict(row("crm", "G2.first_s", "block"), kind="ceiling", limit=51200, delta=None, noise=None,
                       candidate={"state": "valid", "value": 81928.0})
        broken = dict(row("ut", "G2.first_s", "issue"), candidate={"state": "missing", "value": None})
        self.publish([ceiling, broken])
        body = self.lines(self.gh.state()["issues"][0]["body"])
        self.assertIn("| ЦРМ | ≤ 51200 | 81928.0 | - | - | блок релиза |", body)
        self.assertIn("| УТ | 1.000 | missing | +50.0% | 5.0% | issue perf |", body)

    def test_the_comment_carries_the_facts_of_the_night(self):
        self.gh.set([issue(5, G2)], [])
        self.publish([row("crm", "G2.first_s", "issue")], summary=dict(SUMMARY, name="2026-10-23"))
        [comment] = self.gh.state()["issues"][0]["comments"]
        body = self.lines(comment)
        for expected in ("Кандидат: develop cccccccccccc; база: v0.3.2 bbbbbbbbbbbb.",
                         "| ЦРМ | 1.000 | 1.500 | +50.0% | 5.0% | issue perf |",
                         f"{publish.NIGHT_PREFIX}2026-10-23"):
            self.assertIn(expected, body)

    def test_a_gate_issue_names_the_gate_in_its_night_line(self):
        self.publish([row("crm", "G2.first_s", "issue")], summary=dict(SUMMARY, kind="gate", name="v0.3.3"))
        self.assertIn(f"{publish.NIGHT_PREFIX}gate-v0.3.3", self.lines(self.gh.state()["issues"][0]["body"]))

    def test_the_last_closed_issue_by_number_decides(self):
        # The list is not sorted by number: the later issue is first in it.
        self.gh.set([issue(7, G2, "CLOSED", "NOT_PLANNED"), issue(3, G2, "CLOSED", "COMPLETED")], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("closed", 7)])
        self.assertEqual(self.writes(), [])
        self.gh.set([issue(7, G2, "CLOSED", "COMPLETED"), issue(3, G2, "CLOSED", "NOT_PLANNED")], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("create", 8)])
        self.assertIn("повтор #7", self.lines(self.gh.state()["issues"][2]["body"]))

    def test_an_open_issue_wins_over_the_closed_ones_of_its_key(self):
        # The closed one has the higher number: the last issue of the key is not the open one.
        self.gh.set([issue(7, G2, "CLOSED", "NOT_PLANNED"), issue(5, G2)], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("comment", 5)])

    def test_an_issue_closed_as_not_planned_or_duplicate_is_left_alone(self):
        # И27: the two reasons of the plan, spelled out.
        for reason in ("NOT_PLANNED", "DUPLICATE"):
            with self.subTest(reason=reason):
                self.gh.set([issue(7, G2, "CLOSED", reason)], [])
                entries = self.publish([row("crm", "G2.first_s", "issue")])
                self.assertEqual([(e["action"], e["issue"], e["detail"]) for e in entries], [("closed", 7, reason)])
                self.assertEqual(self.writes(), [])

    def test_a_key_that_only_begins_like_the_key_is_another_issue(self):
        self.gh.set([issue(7, G2 + "_p95")], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("create", 8)])
        self.assertEqual(self.gh.state()["issues"][0]["comments"], [])

    def test_the_blocker_label_is_added_once_and_never_taken_away(self):
        # И41: a block on an issue that has the label edits nothing; a lesser verdict on it removes nothing.
        self.gh.set([issue(5, G2, labels=("perf", "regression", "release-blocker"))], [])
        self.publish([row("crm", "G2.first_s", "block")])
        self.assertEqual([a for a in self.writes() if a[:2] == ["issue", "edit"]], [])
        self.publish([row("crm", "G2.first_s", "issue")], summary=dict(SUMMARY, name="2026-10-23"))
        self.assertFalse(any("--remove-label" in c["args"] for c in self.gh.calls()))
        self.assertIn("release-blocker", self.gh.state()["issues"][0]["labels"])

    def test_an_issue_that_is_not_a_block_gets_no_blocker_label(self):
        self.gh.set([issue(5, G2)], [])
        self.publish([row("crm", "G2.first_s", "issue")])
        self.assertEqual(self.gh.state()["issues"][0]["labels"], ["perf", "regression"])
        self.assertEqual([a for a in self.writes() if a[:2] == ["issue", "edit"]], [])

    def test_only_the_labels_of_the_plan_reach_gh(self):
        # И28
        self.gh.set([issue(5, G2)], [])
        self.publish([row("crm", "G2.first_s", "block"), row("crm", "L1.first_s", "issue"),
                      row("ut", "L1.first_s", "block")])
        used = {args[i + 1] for call in self.gh.calls() for args in [call["args"]]
                for i, arg in enumerate(args) if arg in ("--label", "--add-label", "--remove-label")}
        self.assertEqual(used, {"perf", "regression", "release-blocker"})
        self.assertLessEqual(used, {"perf", "false-positive", "regression", "release-blocker"})

    def test_a_comment_with_a_forbidden_text_is_not_posted(self):
        # И29 on an open issue: the refusal names the issue and no comment is made.
        self.gh.set([issue(5, G2)], [])
        entries = self.publish([row("crm", "G2.first_s", "issue")], forbidden=["Модули"], labels={"crm": "Модули"})
        self.assertEqual([(e["action"], e["issue"]) for e in entries], [("refused", 5)])
        self.assertEqual(self.writes(), [])

    def test_a_path_in_the_text_refuses_the_group_without_a_write(self):
        # И29: the snapshot path and the path of a point are matched as they are, not as words.
        for leak in ("/data/snapshots/crm", "Модули/Большой.bsl"):
            with self.subTest(leak=leak):
                entries = self.publish([row("crm", "G2.first_s", "issue")], forbidden=[leak],
                                       labels={"crm": f"ЦРМ в {leak}"})
                self.assertEqual([e["action"] for e in entries], ["refused"])
                self.assertEqual(self.writes(), [])


class NightPublishes(NightCase):
    """`night --publish` publishes its perf issues; `night` alone never calls `gh`."""

    def setUp(self):
        super().setUp()
        self.gh = FakeGh(self, self.tmp)
        set_mode("develop=leak")
        self.fresh_noise()

    def test_without_the_flag_no_gh(self):
        with patch.object(night, "SUITE", CLOSE_SET):
            summary = self.night()
        self.assertEqual(summary["verdict"], "block")
        self.assertEqual(self.gh.calls(), [])
        self.assertNotIn("publish", summary)

    def test_with_the_flag_the_issue_is_published_before_the_summary(self):
        with patch.object(night, "SUITE", CLOSE_SET):
            summary = self.night(publish=True)
        self.assertEqual(summary["verdict"], "block")
        rows = read_json(self.data.path("nights", DATE, "compare.json"))["rows"]
        flagged = {publish.perf_key(r) for r in rows if r["outcome"] in ("issue", "block")}
        self.assertIn("perf/lsp-close-memory/run/rss_growth_max_kb", flagged)
        self.assertEqual([(e["action"], e["key"]) for e in summary["publish"]],
                         [("create", key) for key in sorted(flagged)])
        self.assertEqual(publish.read_log(self.data.path("nights", DATE)), summary["publish"])
        body = self.gh.state()["issues"][0]["body"]
        self.assertIn("ЦРМ", body)
        for leak in publish.forbidden_strings(self.data, ["crm"]):
            self.assertNotIn(leak, body)
        with open(self.data.path("nights", DATE, "report.md"), encoding="utf-8") as fh:
            self.assertIn("## Публикация", fh.read())

    def test_the_summary_on_disk_carries_the_publication(self):
        # The publication precedes the summary: what was published is in `summary.json`, not only in the answer.
        with patch.object(night, "SUITE", CLOSE_SET):
            summary = self.night(publish=True)
        self.assertTrue(summary["publish"])
        self.assertEqual(read_json(self.data.path("nights", DATE, "summary.json"))["publish"], summary["publish"])

    def test_the_forbidden_strings_are_the_snapshot_and_the_paths_of_the_points(self):
        # И29: what a text must not hold — the snapshot path, each `relative_path` and its directories.
        self.assertEqual(publish.forbidden_strings(self.data, ["crm"]),
                         sorted([self.snap, "Модули", "Модули/Большой.bsl", "Модули/Малый.bsl"]))

    def test_a_corpus_label_that_is_a_point_directory_is_refused_by_the_night(self):
        # И29 through `night --publish`: the wiring passes the strings of the data dir to the publication.
        with open(self.manifest_path, encoding="utf-8") as fh:
            manifest = fh.read()
        with open(self.manifest_path, "w", encoding="utf-8") as fh:
            fh.write(manifest.replace('label = "ЦРМ"', 'label = "Модули"'))
        with patch.object(night, "SUITE", CLOSE_SET):
            summary = self.night(publish=True)
        self.assertEqual({e["action"] for e in summary["publish"]}, {"refused"})
        self.assertEqual(self.gh.state()["issues"], [])


if __name__ == "__main__":
    unittest.main()
