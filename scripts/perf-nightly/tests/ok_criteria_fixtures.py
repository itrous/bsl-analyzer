"""Fixtures of the tests of the criteria of the gate (plan docs/plans/gate-ok-criteria.md,
table of checks И1–И10): the record world of `record_fixtures.py` with the knobs the
criteria turn, and the night against the record. Nothing waits for time: the clock is
virtual and the machine scripted; the numbers are synthetic inputs of the tests, not
limits of the product."""

from __future__ import annotations

import argparse
import os

from gate_fixtures import BASE_TAG, CAND_LABEL, CAND_SHA, timed
from helpers import set_mode
from record_fixtures import RecordCase

import gate
import night
import perf_nightly
import verdict
from common import read_json

CALL = {"graph_resolve_s": "graph_resolve", "diagnostics_workspace_s": "diagnostics_workspace"}
TRADED = tuple(CALL)
TARGET_KB = 4667316
PEAK_OVER_TARGET_KB = 6917216


class CriteriaCase(RecordCase):
    """A gate world in which the test sets what the criteria are about."""

    nights = 0

    def reset_world(self) -> None:
        """The world of `setUp` again, for a test that runs the gate for several inputs in turn."""
        self.write_baseline()
        self.script.values.clear()
        set_mode(*self.modes.split(","))
        changelog = os.path.join(self.repo, "CHANGELOG.md")
        if os.path.exists(changelog):
            os.remove(changelog)

    def work(self, metric: str, base, cand) -> None:
        """The seconds of the call of `metric` for the base runs and for the candidate runs."""
        self.script.values["elapsed"] = timed(CALL[metric], lambda run: cand if run.role == "c" else base)

    def wall(self, base, cand) -> None:
        """Seconds to graph readiness of the base and of the candidate in series A; either is a number
        or a function of the run (`run.n` is 1 to 3 in the first round and 4 to 6 in the second)."""
        def pick(value, run):
            return value(run) if callable(value) else value

        self.script.values["wall"] = lambda run: pick(cand if run.role == "c" and run.series == "A" else base, run)

    def peak(self, value) -> None:
        self.script.values["hwm_kb"] = value if callable(value) else (lambda run: value)

    def memory_record(self, noise, base_a_noise=1000.0, owner_target=True) -> None:
        """The record of memory as the owner's target; `noise` is its tolerance (None: none was measured)."""
        self.set_record("hwm_kb", TARGET_KB, noise, base_a_noise=base_a_noise)
        if owner_target:
            self.edit_baseline(lambda doc: doc["metrics"]["hwm_kb"]["evidence"].update(kind="owner_target"))

    def night(self) -> tuple[dict, dict]:
        """(result, saved summary) of a night of the candidate against the record, on a date of its own."""
        CriteriaCase.nights += 1
        date = f"2026-11-{CriteriaCase.nights:02d}"
        self.night_date = date
        cand = perf_nightly.load_binary(self.cand_path)
        args = argparse.Namespace(baseline=self.baseline_path, corpora=None)
        result = gate.record_night(args, self.clock, self.data, date, cand, night.Window(self.clock, None), self.machine)
        return result, read_json(self.data.path(*result["summary"].split("/")[1:]))

    def night_report(self) -> str:
        path = self.data.path("gates", f"record-night-{self.night_date}-{CAND_SHA[:12]}", "report.md")
        with open(path, encoding="utf-8") as fh:
            return fh.read()

    @staticmethod
    def cells(report: str, start: str) -> list[str]:
        """The cells of the line of a table of the report that starts with `start`."""
        line = next(l for l in report.splitlines() if l.startswith(start))
        return [c.strip() for c in line.strip("|").split("|")]

    @staticmethod
    def section(report: str, title: str) -> str:
        return report.split(f"## {title}", 1)[1].split("\n## ", 1)[0]

    def record_cells(self, report: str, metric: str) -> dict:
        """The line of `metric` in the table of the record, by the names of its columns."""
        names = ["metric", "aggregate", "record", "candidate", "delta", "noise", "limit", "basis", "sha", "source",
                 "outcome", "reason"]
        line = next(l for l in self.section(report, "Рекорд").splitlines() if l.startswith(f"| {metric} |"))
        return dict(zip(names, [c.strip() for c in line.strip("|").split("|")]))


def limit_of(record: float, margin: float) -> float:
    """The limit of the record as the harness computes it."""
    return verdict.record_limit(record, margin)


def feature_signals(case):
    """(name, how to give the signal) of a feature in the sweep or in the CHANGELOG."""
    def changelog(text):
        def give():
            with open(os.path.join(case.repo, "CHANGELOG.md"), "w", encoding="utf-8") as fh:
                fh.write(text)
        return give

    def mode(label, switch):
        return lambda: set_mode(*case.modes.split(","), f"{label}={switch}")

    return [("no signal", lambda: None),
            ("a new code", mode(CAND_LABEL, "sweep_extra_code")),
            ("a vanished code", mode(BASE_TAG, "sweep_extra_code")),
            ("a code that grew", mode(CAND_LABEL, "sweep_other_count")),
            ("a code that shrank", mode(BASE_TAG, "sweep_other_count")),
            ("a feature in the CHANGELOG",
             changelog("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- new-feature: a diagnostic of a new kind\n")),
            ("a CHANGELOG without it", changelog("# Changelog\n\n## [Unreleased]\n\n### Fixed\n\n- a typo\n"))]
