"""What the release job and a reader of the README get out of the record (plan
docs/plans/perf-ratchet-record.md, И11 and И12): an exit code and a summary
from which the steps 6 to 8 of the regulation follow without a second reading,
and a README whose commands are the ones that parse. The regulation itself is
a file of the job outside this repository; its text is not tested here. The
clock is virtual and the machine scripted (`gate_fixtures.py`)."""

import os
import shlex
import unittest

from gate_fixtures import CAND_LABEL, MACHINE, timed
from helpers import set_mode
from record_fixtures import RecordCase

import gate
import perf_nightly
import verdict

README = os.path.join(os.path.dirname(gate.__file__), "..", "README.md")
PARAGRAPH = (
    "Оптимизацию закрепляет рекорд, а не релиз. Выпуск версии не повышает baseline и не прощает накопленные "
    "небольшие ухудшения. Подтверждённое двумя раундами улучшение сверх шума переносит исполнитель оптимизации "
    "из record_proposals в baseline отдельным обычным коммитом с SHA и ссылкой на summary. Один удачный прогон "
    "рекорд не двигает; возврат памяти к 4 787 080 KB остаётся обязательным. Ночной develop проверяется заранее, "
    "но его успешный отчёт не заменяет релизный гейт по обеим базам.")


def regulation(code, summary) -> str:
    """Steps 6 to 8 of the regulation, from the exit code and the summary alone:
    `release` (step 8) only for a full `pass` with `record.verdict = pass`; `step7` for a block by any base,
    the memory, a ceiling or the answers; `stop` for everything else, which counts for nothing."""
    if summary is None:
        return "stop"
    if code == 0 and summary["verdict"] == "pass" and (summary.get("record") or {}).get("verdict") == "pass":
        return "release"
    return "step7" if code == 1 and summary["verdict"] == "block" else "stop"


class ReleaseDecision(RecordCase):
    """И11."""

    def scenario(self, name):
        if name == "record block":
            self.set_record("diagnostics_workspace_s", 10.0, 0.01)
        elif name == "previous release block":
            self.script.values["elapsed"] = timed("graph_resolve", lambda run: 19.58 if run.role == "c" else None)
        elif name == "record incomplete":
            self.set_record("graph_resolve_s", noise=None)
        elif name == "profile incomplete":
            self.machine.print["kernel"] = "7.0"
        elif name == "no baseline":
            os.remove(self.baseline_path)
        elif name == "peak over the target":
            self.set_record("hwm_kb", 4667316, 0.01)
            self.edit_baseline(lambda doc: doc["metrics"]["hwm_kb"]["evidence"].update(kind="owner_target"))
            self.script.values["hwm_kb"] = lambda run: 6917564 if run.series == "B" else 4739312
        elif name == "answers differ":
            set_mode(*self.modes.split(","), f"{CAND_LABEL}=resolve_other")
        elif name == "both bases fail":
            self.scenario("record block")
            self.scenario("previous release block")

    def run_scenario(self, name):
        self.write_baseline()
        self.write_noise()
        self.script.values.clear()
        set_mode(*self.modes.split(","))
        self.machine.print = dict(MACHINE)
        self.scenario(name)
        return self.gate(label=name.replace(" ", "-"))

    @staticmethod
    def cells(report, start):
        line = next(l for l in report.splitlines() if l.startswith(start))
        return [c.strip() for c in line.strip("|").split("|")]

    def test_the_steps_of_the_regulation_follow_from_the_exit_code_and_the_summary(self):
        expected = {
            "all pass": ("release", 0), "record block": ("step7", 1), "previous release block": ("step7", 1),
            "both bases fail": ("step7", 1), "peak over the target": ("step7", 1), "answers differ": ("step7", 1),
            "record incomplete": ("stop", 2), "profile incomplete": ("stop", 2), "no baseline": ("stop", 2)}
        for name, (decision, code) in expected.items():
            with self.subTest(scenario=name):
                got, summary, line = self.run_scenario(name)
                self.assertEqual((regulation(got, summary), got), (decision, code))
                self.assertTrue(line.startswith(f"gate={summary['verdict']} "), line)
                if got == 0:
                    self.assertEqual((summary["verdict"], summary["record"]["verdict"]), ("pass", "pass"))
                if summary["record"]["verdict"] != "pass":
                    self.assertNotEqual(got, 0, "no release while the record does not pass")

    def test_an_old_pass_that_the_record_blocks_is_a_block_that_shows_both_bases(self):
        code, summary, line = self.run_scenario("record block")
        self.assertEqual(self.decided(summary), {"ok"}, "the previous release: pass")
        self.assertEqual((code, summary["verdict"], summary["record"]["verdict"]), (1, "block", "block"))
        self.assertIn("block: record:diagnostics_workspace_s", summary["reasons"])
        report = self.report_text("record-block")
        old = self.cells(report, "| diagnostics_workspace_s | decisive |")
        record = self.cells(report, "| diagnostics_workspace_s | median_A |")
        self.assertEqual(old[2:4], ["22.100", "22.100"], "base and candidate of the previous release")
        # metric, aggregate, record, candidate, Δ, ν, limit, its basis, record SHA, source, outcome, reason
        self.assertEqual(record[2:], ["10.000", "22.100", "+121.0%", "1.00%", "10.100", "шум записи (ν)", "bbbbbbbbbbbb",
                                      "historical_import", "block", "-"])
        for part in ("## Прошлый релиз", "## Рекорд", "## Прогоны", "| A-c1 | candidate | A | true |"):
            self.assertIn(part, report)
        self.assertTrue(line.strip().endswith(f"out={self.data.path('gates', 'record-block')}"),
                        "the path of the report is in the last line of the output")
        self.assertTrue(os.path.isfile(os.path.join(self.data.path("gates", "record-block"), "report.md")))

    def test_a_failure_of_both_bases_is_one_block_with_every_reason_and_the_memory_target_is_named(self):
        code, summary, _ = self.run_scenario("both bases fail")
        self.assertEqual((code, summary["verdict"]), (1, "block"))
        self.assertIn("block: graph_resolve_s", summary["reasons"])
        self.assertIn("block: record:diagnostics_workspace_s", summary["reasons"])
        code, summary, _ = self.run_scenario("peak over the target")
        self.assertIn("| owner_target | block |", self.report_text("peak-over-the-target"))

    def test_an_incomplete_record_is_not_a_block_and_names_its_reason(self):
        for name, reason in (("record incomplete", "record_no_noise:graph_resolve_s"),
                             ("profile incomplete", "record_profile_mismatch:kernel"),
                             ("no baseline", "baseline_missing")):
            with self.subTest(scenario=name):
                code, summary, _ = self.run_scenario(name)
                self.assertEqual((code, summary["verdict"]), (2, "incomplete"))
                self.assertIn(reason, summary["reasons"])
                self.assertNotIn("block", self.decided(summary))

    def test_a_probe_and_a_summary_without_a_record_do_not_vouch_for_a_release(self):
        code, probe, _ = self.gate("--ready-only", label="probe")
        self.assertEqual((code, probe["verdict"], regulation(code, probe)), (0, "probe-ok", "stop"))
        _, full, _ = self.gate(label="full")
        self.assertEqual(regulation(0, full), "release")
        legacy = {k: v for k, v in full.items() if k not in ("record", "memory_base", "record_proposals", "record_confirmation")}
        self.assertEqual((legacy["verdict"], regulation(0, legacy)), ("pass", "stop"))
        self.assertEqual(regulation(2, None), "stop", "no summary.json: stop")

    def test_the_old_command_line_still_runs_and_reads_the_record_next_to_the_gate(self):
        args = perf_nightly.parser().parse_args(["gate", "--data-dir", "/d", "--base", "v0.4.1", "--candidate", "HEAD",
                                                 "--label", "x"])
        self.assertIsNone(args.baseline)
        self.assertEqual(gate.baseline_path(args), os.path.join(os.path.dirname(gate.__file__), "baseline.json"))
        self.assertTrue(os.path.isfile(gate.baseline_path(args)))
        night = perf_nightly.parser().parse_args(["night", "--data-dir", "/d"])
        self.assertEqual(gate.baseline_path(night), gate.baseline_path(args))


class Readme(unittest.TestCase):
    """И12."""

    def setUp(self):
        with open(README, encoding="utf-8") as fh:
            self.text = fh.read()
        block = self.text.split("## perf-nightly/", 1)[1].split("```bash", 1)[1].split("```", 1)[0]
        self.commands = [line for line in block.splitlines() if line.startswith("python3 ")]

    def parse(self, line):
        words = shlex.split(line.split("  #", 1)[0])
        substitute = {"$D": "/data", "$R": "/record.json", "<T>": "v0.4.1", "<SHA>": "HEAD",
                      "<уникальная-метка>": "label-1"}
        return perf_nightly.parser().parse_args([substitute.get(w, w) for w in words[2:]])

    def test_the_commands_of_the_three_measurements_are_there_and_parse(self):
        found = {}
        for line in self.commands:
            if "--baseline" in line or "--record-metrics-only" in line:
                args = self.parse(line)
                found[args.func.__name__] = args
        self.assertEqual(sorted(found), ["cmd_gate", "cmd_gate_calibrate", "cmd_night"])
        night = found["cmd_night"]
        self.assertEqual((night.baseline, night.publish, night.deadline), ("/record.json", True, "08:45"),
                         "the warm night, which also measures the record cold")
        calibrate = found["cmd_gate_calibrate"]
        self.assertEqual((calibrate.record_metrics_only, calibrate.base), (True, gate.RECORD_CALIBRATION_BASE))
        release = found["cmd_gate"]
        self.assertEqual((release.baseline, release.base, release.label), ("/record.json", "v0.4.1", "label-1"),
                         "the release gate against two bases")
        self.assertIn("python3 -B -m unittest discover -s scripts/perf-nightly/tests", self.commands)

    def test_the_reader_is_told_what_each_of_them_measures(self):
        flat = " ".join(self.text.split())
        for part in ("record-night-<дата>-<sha12>", "Она не обновляет рекорд и не заменяет релизный гейт",
                     "scripts/perf-nightly/baseline.json", "C > R·(1+ν)", "record_proposals",
                     "chore(perf): ratchet <metric> <old> -> <new>", "chore(perf): calibrate record tolerances",
                     "record_no_noise", "record_profile_mismatch", "ратчет не измерялся"):
            self.assertIn(part, flat)
        for metric in verdict.GATE_RECORD:
            self.assertIn(f"`{metric}`", self.text, "the table of the record names every metric")
        for aggregate in ("медиана A", "| `hwm_kb` | максимум A |", "медиана B"):
            self.assertIn(aggregate, self.text)
        self.assertNotIn("максимум A+B", self.text, "the peak of both series is compared with no record")

    def test_the_paragraph_that_ties_the_record_to_the_optimisation_is_there_word_for_word(self):
        self.assertIn(PARAGRAPH, self.text)

    def test_the_commit_of_a_record_is_the_only_way_down_that_the_readme_names(self):
        flat = " ".join(self.text.split())
        self.assertIn("Ни `gate`, ни `night` файл рекорда не пишут", flat)
        self.assertIn("и только вниз", flat)


if __name__ == "__main__":
    unittest.main()
