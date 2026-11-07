"""Issues of the record night on a fake `gh` (plan
docs/plans/perf-ratchet-record.md, И09 and the part of И10 that is published):
one open issue per metric over the record, found among all issues of the
repository, a range of commits that names candidates and not a culprit, a
profile that does not compare as an issue of its own, and a `gh` error that is
seen and never turns into a duplicate. The night summaries are the ones the
record night writes (`gate.record_night`); nothing waits for time."""

import argparse
import os
import unittest

from gate_fixtures import CAND_SHA
from helpers import FakeGh, set_mode
from record_fixtures import RecordCase
from test_night import DATE

import gate
import night
import perf_nightly
import pio_module
import publish
from common import read_json

WORK = "perf-ratchet/graph_resolve_s"
WALL = "perf-ratchet/graph_ready_s"
PROFILE = "perf-ratchet/profile"
NEXT = "2026-10-23"
SHA_12 = CAND_SHA[:12]
RECORD_SHA = "a" * 40


def issue(number, title="t", body="", state="OPEN", reason=None, labels=tuple(publish.BLOCK_LABELS), comments=(),
          pull=False):
    return {"number": number, "state": state, "stateReason": reason, "title": title, "body": body,
            "labels": list(labels), "comments": list(comments), "pull": pull}


def keyed(key, extra=""):
    return f"текст\n{publish.KEY_PREFIX}{key}\n{extra}"


class RecordPublish(RecordCase):
    def setUp(self):
        super().setUp()
        self.gh = FakeGh(self, self.tmp)
        self.set_record("graph_resolve_s", 15.0, 0.01, sha=RECORD_SHA)   # the candidate's 17.8 is over it by 18.67 %
        self.ancestor = lambda sha, of: True

    def summary(self, date=DATE, machine=None) -> dict:
        """What the record night of `date` wrote."""
        cand = perf_nightly.load_binary(self.cand_path)
        args = argparse.Namespace(baseline=self.baseline_path, corpora=None)
        result = gate.record_night(args, self.clock, self.data, date, cand, night.Window(self.clock, None),
                                   machine or self.machine)
        return self.read_summary(result)

    def read_summary(self, result) -> dict:
        return read_json(self.data.path(*result["summary"].split("/")[1:]))

    def out(self, date=DATE) -> str:
        return os.path.join(self.tmp, "nights", date)

    def publish(self, saved, forbidden=(), ancestor=None, date=DATE) -> list[dict]:
        return publish.publish_record(self.out(date), saved, list(forbidden), ancestor or self.ancestor)

    def issues(self) -> list[dict]:
        return self.gh.state()["issues"]

    def writes(self) -> list[list[str]]:
        return [c["args"] for c in self.gh.calls() if c["args"][0] != "api"]

    def actions(self, entries) -> list[tuple]:
        return [(e["action"], e["issue"], e["key"]) for e in entries]

    def lines(self, text) -> list[str]:
        return [line.strip() for line in text.splitlines()]


class OneIssuePerMetric(RecordPublish):
    def test_a_new_excess_opens_one_issue_with_the_range_the_values_and_the_links(self):
        saved = self.summary()
        entries = self.publish(saved)
        self.assertEqual(self.actions(entries), [("create", 1, WORK)])
        [created] = self.issues()
        self.assertEqual(created["title"], "perf: рекорд превышен — graph_resolve_s")
        self.assertEqual(created["labels"], ["perf", "regression", "release-blocker"])
        lines = self.lines(created["body"])
        self.assertIn(f"{publish.KEY_PREFIX}{WORK}", lines)
        self.assertIn(f"{publish.NIGHT_PREFIX}record-{DATE}-{SHA_12}", lines)
        for part in ("Метрика: graph_resolve_s, s, агрегат median_A.", "Рекорд R = 15.0; кандидат C = 17.8.",
                     "Δ = +18.67%; ν = 1.00%; предел R·(1+ν) = 15.15; абсолютный потолок: -.",
                     f"SHA рекордсмена: {RECORD_SHA}; develop: {CAND_SHA}.",
                     f"`{RECORD_SHA}..{CAND_SHA}`",
                     f"https://github.com/itrous/bsl-analyzer/compare/{RECORD_SHA}...{CAND_SHA}",
                     "Кандидаты для поиска причины (не найденный виновник)",
                     "Baseline: baseline.json в коммите харнесса",
                     f"data/gates/record-night-{DATE}-{SHA_12}/summary.json",
                     f"data/gates/record-night-{DATE}-{SHA_12}/report.md",
                     "Профиль замера:", "Отличия от профиля рекорда: нет."):
            self.assertIn(part, created["body"])
        table = [line for line in lines if line.startswith("| A-c")]
        self.assertEqual(len(table), 3, "the held runs of the series A")
        self.assertTrue(all(line.endswith("| true | 17.8 |") for line in table))
        self.assertNotIn("B-c1", created["body"])
        self.assertEqual(os.path.isfile(os.path.join(self.out(), publish.LOG)), True)

    def test_nothing_private_reaches_the_issue(self):
        self.publish(self.summary())
        [created] = self.issues()
        for private in (self.tmp, self.erp, self.data_dir, self.baseline_path):
            self.assertNotIn(private, created["body"] + created["title"])

    def test_the_target_of_the_memory_is_told_where_it_came_from_and_only_for_the_memory(self):
        self.set_record("hwm_kb", 1000, 0.01)
        self.edit_baseline(lambda doc: doc["metrics"]["hwm_kb"]["evidence"].update(kind="owner_target"))
        self.publish(self.summary())
        memory = next(i for i in self.issues() if "hwm_kb" in i["title"])
        work = next(i for i in self.issues() if "graph_resolve_s" in i["title"])
        self.assertIn(publish.owner_target_text("hwm_kb", 1000), memory["body"])
        self.assertIn("Рекорд R = 1000;", memory["body"])
        self.assertIn("Цель 1 000 KB задана владельцем для пика серии A, без references", memory["body"],
                      "the target told is the record's own value")
        self.assertNotIn("4 787 080", memory["body"], "no number of the shipped file stands beside another record")
        self.assertIn("Пик серии B с references держит абсолютный потолок 7 340 032 KB", memory["body"])
        self.assertNotIn("общего пика", memory["body"])
        self.assertIn("исправление памяти — отдельная задача", memory["body"])
        self.assertNotIn("владельцем", work["body"], "the record of the work was not set by the owner")
        self.assertIn("абсолютный потолок: 7340032", memory["body"])

    def test_each_record_the_owner_set_tells_its_own_decision(self):
        self.set_record("hwm_kb", 1000, 0.01)
        self.set_record("diagnostics_workspace_s", 20.0, 0.01)   # the candidate's 22.1 is over it
        self.edit_baseline(lambda doc: [doc["metrics"][metric]["evidence"].update(kind="owner_target")
                                        for metric in ("hwm_kb", "diagnostics_workspace_s")])
        self.publish(self.summary())
        memory = next(i for i in self.issues() if "hwm_kb" in i["title"])
        sweep = next(i for i in self.issues() if "diagnostics_workspace_s" in i["title"])
        work = next(i for i in self.issues() if "graph_resolve_s" in i["title"])
        self.assertIn(publish.owner_target_text("diagnostics_workspace_s", 20.0), sweep["body"])
        self.assertIn("Рекорд 20.00 с задан владельцем на уровне develop", sweep["body"],
                      "the level told is the record's own value")
        self.assertIn("против v0.4.1 (21.93 с) узаконено", sweep["body"])
        self.assertNotIn("Цель 1 000 KB", sweep["body"], "the sweep does not tell the target of the memory")
        self.assertNotIn("узаконено", memory["body"])
        self.assertNotIn("владельцем", work["body"], "a record nobody set by decision tells no decision")

    def test_a_record_that_is_no_longer_the_owners_tells_no_decision_of_the_owner(self):
        self.set_record("hwm_kb", 1000, 0.01)
        self.set_record("diagnostics_workspace_s", 20.0, 0.01)
        self.edit_baseline(lambda doc: [doc["metrics"][metric]["evidence"].update(kind="historical_import")
                                        for metric in ("hwm_kb", "diagnostics_workspace_s")])
        self.publish(self.summary())
        for metric in ("hwm_kb", "diagnostics_workspace_s"):
            body = next(i for i in self.issues() if metric in i["title"])["body"]
            self.assertIn(f"Рекорд R = {'1000' if metric == 'hwm_kb' else '20.0'};", body, "the control: it is over the record")
            self.assertNotIn("владельцем", body, metric)

    def test_the_slowdown_the_owner_legalised_is_told_in_percent_of_the_record_it_replaced(self):
        slowed = publish.owner_target_text("diagnostics_workspace_s", 22.560848722991068)
        self.assertIn("Рекорд 22.56 с задан владельцем на уровне develop: замедление на 2.86 % против v0.4.1 "
                      "(21.93 с) узаконено", slowed)
        self.assertIn("Цель 4 787 080 KB задана владельцем", publish.owner_target_text("hwm_kb", 4787080))
        self.assertIsNone(publish.owner_target_text("graph_resolve_s", 17.4), "the owner set no other record")

    def memory_runs(self, saved) -> list[str]:
        """The runs the table of the memory issue lists, in its order."""
        self.publish(saved)
        memory = next(i for i in self.issues() if "hwm_kb" in i["title"])
        return [line.split("|")[1].strip() for line in self.lines(memory["body"])
                if line.startswith(("| A-", "| B-"))]

    def test_the_runs_listed_for_the_memory_are_the_runs_of_the_series_a_alone(self):
        self.set_record("hwm_kb", 1000, 0.01)
        saved = self.summary()
        self.assertEqual({r["series"] for r in saved["run_values"]}, {"A", "B"}, "the control: both series ran")
        self.assertEqual(self.memory_runs(saved), ["A-c1", "A-c2", "A-c3"])

    def test_a_summary_of_the_memory_held_by_both_series_lists_the_runs_of_both(self):
        self.set_record("hwm_kb", 1000, 0.01)
        saved = self.summary()
        [row] = [r for r in saved["record"]["rows"] if r["metric"] == "hwm_kb"]
        row["aggregate"] = "max_AB"
        self.assertEqual(self.memory_runs(saved), ["A-c1", "A-c2", "A-c3", "B-c1", "B-c2", "B-c3"])

    def test_a_record_that_is_not_an_ancestor_says_so_and_promises_no_linear_range(self):
        self.publish(self.summary(), ancestor=lambda sha, of: False)
        [created] = self.issues()
        self.assertIn(f"История разошлась: {RECORD_SHA} не предок {CAND_SHA}, диапазон не линеен.", created["body"])
        self.assertIn(f"`{RECORD_SHA}..{CAND_SHA}`", created["body"])

    def test_the_record_holder_itself_has_an_empty_range_and_sends_the_search_to_the_environment(self):
        self.set_record("graph_resolve_s", sha=CAND_SHA)
        self.publish(self.summary())
        [created] = self.issues()
        self.assertIn(f"Диапазон пуст: `{CAND_SHA}..{CAND_SHA}`", created["body"])
        self.assertIn("проверить окружение и калибровку", created["body"])
        self.assertNotIn("Кандидаты для поиска причины", created["body"])
        self.assertNotIn("/compare/", created["body"])
        self.assertEqual(created["labels"], ["perf", "regression", "release-blocker"], "the measured excess stands")

    def test_another_record_sha_gives_a_real_range_with_the_way_back_to_the_environment(self):
        self.publish(self.summary())
        [created] = self.issues()
        self.assertNotIn("Диапазон пуст", created["body"])
        self.assertIn("разбор идёт к окружению и калибровке, а не к этим коммитам", created["body"])

    def test_two_metrics_over_their_records_are_two_issues_and_each_is_found_by_its_own_key(self):
        self.set_record("graph_ready_s", 90.0, 0.01)             # the candidate's 100 is over it too
        saved = self.summary()
        entries = self.publish(saved)
        self.assertEqual(self.actions(entries), [("create", 1, WALL), ("create", 2, WORK)])
        again = self.publish(self.summary(NEXT), date=NEXT)
        self.assertEqual(self.actions(again), [("comment", 1, WALL), ("comment", 2, WORK)])
        self.assertEqual(len(self.issues()), 2)

    def test_a_night_with_nothing_over_the_record_says_nothing_to_github(self):
        self.set_record("graph_resolve_s", 100.0, 0.01)
        self.assertEqual(self.publish(self.summary()), [])
        self.assertEqual(self.gh.calls(), [])

    def test_a_run_that_did_not_hold_blocks_the_record_without_a_number_and_opens_no_issue(self):
        set_mode(*self.modes.split(","), "cand=cand_crash")
        saved = self.summary()
        row = next(r for r in saved["record"]["rows"] if r["metric"] == "graph_resolve_s")
        self.assertEqual((row["outcome"], row["delta"]), ("block", None))
        self.assertEqual(self.publish(saved), [])
        self.assertEqual(self.gh.calls(), [])


class FindingTheOpenIssue(RecordPublish):
    def test_the_same_night_comments_once_and_the_next_night_comments_again(self):
        saved = self.summary()
        self.publish(saved)
        again = self.publish(saved)
        self.assertEqual([e["action"] for e in again], ["commented"])
        self.assertEqual(self.issues()[0]["comments"], [], "the night that opened the issue is its first report")
        night_two = self.summary(NEXT)
        entries = self.publish(night_two, date=NEXT)
        self.assertEqual(self.actions(entries), [("comment", 1, WORK)])
        [comment] = self.issues()[0]["comments"]
        self.assertIn(f"{publish.NIGHT_PREFIX}record-{NEXT}-{SHA_12}", self.lines(comment))
        self.assertIn("Δ = +18.67%", comment)
        twice = self.publish(night_two, date=NEXT)
        self.assertEqual([e["action"] for e in twice], ["commented"])
        self.assertEqual((len(self.issues()), len(self.issues()[0]["comments"])), (1, 1))

    def test_an_open_issue_without_the_perf_label_is_found_by_its_marker(self):
        self.gh.set([issue(4, "об этом", keyed(WORK), labels=())], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 4, WORK)] + [("label", 4, WORK)] * 3)
        self.assertEqual(len(self.issues()), 1)
        added = [a for a in self.writes() if a[:2] == ["issue", "edit"]]
        self.assertEqual([a[-1] for a in added], ["perf", "regression", "release-blocker"], "its labels are completed")

    def test_the_old_release_title_without_any_marker_is_found_and_gets_the_marker_in_a_comment(self):
        self.gh.set([issue(9, "Ночной релиз 0.4.3: гейт провален по graph_resolve", "ничего про ключ", labels=())], [])
        entries = self.publish(self.summary())
        self.assertEqual([e["action"] for e in entries][:1], ["comment"])
        self.assertEqual({e["issue"] for e in entries}, {9})
        [comment] = self.issues()[0]["comments"]
        self.assertIn(f"{publish.KEY_PREFIX}{WORK}", self.lines(comment), "so that the next search finds it by the key")
        self.assertEqual(len(self.issues()), 1)

    def test_a_title_names_one_metric_and_not_the_ones_that_share_its_letters(self):
        self.set_record("graph_ready_s", 90.0, 0.01)
        self.gh.set([issue(3, "Ночной релиз 0.4.3: гейт провален по graph_ready", "x")], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 3, WALL), ("create", 4, WORK)],
                         "`graph_ready` is the wall; the work metric has its own issue")
        self.gh.set([issue(3, "Ночной релиз 0.4.3: гейт провален по graph_ready_cpu", "x")], [])
        entries = self.publish(self.summary(NEXT), date=NEXT)
        self.assertEqual(self.actions(entries), [("create", 4, WALL), ("create", 5, WORK)],
                         "`graph_ready_cpu` names no metric that judges; the wall has its own issue")

    def test_a_marker_that_lives_only_in_a_comment_is_found(self):
        self.gh.set([issue(6, "обычный заголовок", "тело без ключа", comments=[f"ранее\n{publish.KEY_PREFIX}{WORK}\n"])], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 6, WORK)])
        self.assertEqual(len(self.issues()), 1)

    def test_a_match_on_the_second_page_is_not_missed(self):
        others = [issue(n, "другое", f"тело {n}") for n in range(1, 5)]
        self.gh.set([*others, issue(5, "найди меня", keyed(WORK))], [], page_size=2)
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 5, WORK)])
        pages = [c for c in self.gh.calls() if c["args"][:2] == ["api", "--paginate"]]
        self.assertEqual(pages[0]["args"][2], "repos/itrous/bsl-analyzer/issues?state=all&per_page=100")
        self.assertEqual(len(self.issues()), 5)

    def test_among_several_open_issues_the_lowest_number_gets_the_comment(self):
        self.gh.set([issue(8, "второй", keyed(WORK)), issue(3, "первый", keyed(WORK))], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 3, WORK)])

    def test_an_open_issue_beats_a_closed_one_and_a_pull_request_is_no_issue(self):
        self.gh.set([issue(2, "закрыт", keyed(WORK), state="CLOSED", reason="COMPLETED"),
                     issue(5, "pr", keyed(WORK), pull=True), issue(7, "открыт", keyed(WORK))], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("comment", 7, WORK)])
        self.gh.set([issue(5, "pr", keyed(WORK), pull=True)], [])
        entries = self.publish(self.summary(NEXT), date=NEXT)
        self.assertEqual(self.actions(entries), [("create", 6, WORK)], "the pull request does not stand for the issue")

    def test_an_issue_of_another_metric_is_not_merged_into_this_one(self):
        self.gh.set([issue(2, "другая метрика", keyed("perf-ratchet/diagnostics_workspace_s"))], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("create", 3, WORK)])
        self.assertEqual(self.issues()[0]["comments"], [])

    def test_an_issue_the_owner_closed_stays_closed_and_the_block_stays_in_the_report(self):
        for reason in publish.OWNER_CLOSED:
            with self.subTest(reason=reason):
                self.gh.set([issue(7, "закрыт владельцем", keyed(WORK), state="CLOSED", reason=reason)], [])
                saved = self.summary(f"2026-11-0{publish.OWNER_CLOSED.index(reason) + 1}")
                self.assertEqual(saved["record"]["verdict"], "block")
                entries = self.publish(saved)
                self.assertEqual(self.actions(entries), [("closed", 7, WORK)])
                self.assertEqual(self.writes(), [])

    def test_a_completed_issue_that_came_back_is_a_new_issue_that_names_the_old_one(self):
        self.gh.set([issue(7, "решён", keyed(WORK), state="CLOSED", reason="COMPLETED")], [])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("create", 8, WORK)])
        self.assertIn("повтор #7", self.lines(self.issues()[1]["body"]))


class WhenGhFails(RecordPublish):
    def notices(self, date=DATE) -> list[str]:
        return [n["text"] for n in pio_module.gh_errors(date, self.out(date))]

    def test_a_list_that_cannot_be_read_creates_nothing_and_is_seen(self):
        self.gh.set([], ["issues?state=all"])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("gh-error", None, "*")])
        self.assertEqual(self.writes(), [])
        [text] = self.notices()
        self.assertIn(f"perf-nightly {DATE}: ошибка gh — ", text)
        self.assertIn("issues", text)

    def test_comments_that_cannot_be_read_do_not_end_in_a_duplicate(self):
        self.gh.set([issue(2, "другое", "тело")], ["issues/comments"])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("gh-error", None, WORK)])
        self.assertEqual(self.writes(), [])
        self.assertEqual(len(self.issues()), 1)
        self.assertIn("comments", self.notices()[0])

    def test_a_list_that_is_not_json_creates_nothing(self):
        entries = publish.publish_record(self.out(), self.summary(), [], self.ancestor,
                                         gh=lambda args, stdin=None: (0, "<html>", ""))
        self.assertEqual([e["action"] for e in entries], ["gh-error"])
        self.assertEqual(self.gh.calls(), [])

    def test_a_create_and_a_comment_that_fail_are_errors_and_not_publications(self):
        self.gh.set([], ["issue create"])
        entries = self.publish(self.summary())
        self.assertEqual(self.actions(entries), [("gh-error", None, WORK)])
        self.assertEqual(self.issues(), [])
        self.assertEqual(len(self.notices()), 1)
        self.gh.set([issue(4, "открыт", keyed(WORK), labels=publish.BLOCK_LABELS)], ["issue comment"])
        entries = self.publish(self.summary(NEXT), date=NEXT)
        self.assertEqual(self.actions(entries), [("gh-error", 4, WORK)])
        self.assertEqual(self.issues()[0]["comments"], [])
        self.assertEqual(len(self.notices(NEXT)), 1)

    def test_a_text_with_a_path_of_the_private_corpus_is_refused_in_an_issue_and_in_a_comment(self):
        forbidden = ["data/gates/record-night"]
        entries = self.publish(self.summary(), forbidden)
        self.assertEqual(self.actions(entries), [("refused", None, WORK)])
        self.assertEqual(self.issues(), [])
        self.gh.set([issue(4, "открыт", keyed(WORK), labels=publish.BLOCK_LABELS)], [])
        entries = self.publish(self.summary(NEXT), forbidden, date=NEXT)
        self.assertEqual(self.actions(entries), [("refused", 4, WORK)])
        self.assertEqual(self.issues()[0]["comments"], [])


class IncomparableProfile(RecordPublish):
    def mismatch(self, date=DATE) -> dict:
        self.machine.print["kernel"] = "7.0"
        return self.summary(date)

    def test_a_profile_that_does_not_compare_is_one_issue_of_its_own_and_no_claim_of_a_regression(self):
        saved = self.mismatch()
        self.assertTrue(all(r["delta"] is None for r in saved["record"]["rows"]))
        entries = self.publish(saved)
        self.assertEqual(self.actions(entries), [("create", 1, PROFILE)])
        [created] = self.issues()
        self.assertEqual(created["title"], "perf: рекорд несопоставим — record_profile_mismatch:kernel")
        self.assertEqual(created["labels"], ["perf"])
        self.assertIn(f"{publish.KEY_PREFIX}{PROFILE}", self.lines(created["body"]))
        self.assertIn("Это несопоставимость замера, а не регрессия продукта.", created["body"])
        self.assertIn("Значения рекорда не повышать", created["body"])
        self.assertIn("Отличия от профиля рекорда: kernel.", created["body"])
        self.assertNotIn("Δ =", created["body"])
        self.assertNotIn(RECORD_SHA, created["body"], "no range of commits is named for a profile")
        self.assertNotIn("/compare/", created["body"])
        self.assertNotIn(WORK, created["body"], "with another kernel no metric issue claims an excess")

    def test_a_changed_kernel_under_the_very_sha_of_the_record_is_a_profile_and_not_an_empty_range(self):
        self.set_record("graph_resolve_s", sha=CAND_SHA)
        before = self.baseline_bytes()
        entries = self.publish(self.mismatch())
        self.assertEqual(self.actions(entries), [("create", 1, PROFILE)])
        [created] = self.issues()
        self.assertEqual(created["labels"], ["perf"], "no claim of a regression")
        self.assertNotIn("Диапазон пуст", created["body"])
        self.assertEqual(self.baseline_bytes(), before, "and the record is not raised to fit the new kernel")

    def test_the_same_profile_issue_gets_a_comment_the_next_night_and_not_a_second_issue(self):
        self.publish(self.mismatch())
        entries = self.publish(self.summary(NEXT), date=NEXT)
        self.assertEqual(self.actions(entries), [("comment", 1, PROFILE)])
        self.assertEqual(len(self.issues()), 1)

    def test_a_tolerance_that_is_missing_is_the_same_kind_of_issue(self):
        self.set_record("diagnostics_workspace_s", noise=None)
        entries = self.publish(self.summary())
        self.assertEqual([e["key"] for e in entries], [WORK, PROFILE])
        profile = next(i for i in self.issues() if i["labels"] == ["perf"])
        self.assertEqual(profile["title"], "perf: рекорд несопоставим — record_no_noise:diagnostics_workspace_s")
        self.assertIn("record_no_noise:diagnostics_workspace_s", profile["body"])


if __name__ == "__main__":
    unittest.main()
