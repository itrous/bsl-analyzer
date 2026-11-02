"""Fixtures of the record tests: the gate world of `gate_fixtures.py` with a
record whose numbers a test sets, and rounds of runs made by hand for the
functions that judge them. Nothing waits for time: the clock is virtual, the
machine scripted and `gh` the fake of `fake_gh.py`."""

from __future__ import annotations

from gate_fixtures import GateCase

import gate
import verdict
from common import read_json, write_json

# The numbers the scripted server of `gate_fixtures.py` gives when a test does not set them.
MEASURED = {"graph_ready_s": 100.0, "graph_ready_cpu_s": 40.0, "graph_resolve_s": 17.8,
            "diagnostics_workspace_s": 22.1, "hwm_kb": 4 << 20, "references_1_s": 56.5,
            "references_2_same_s": 24.9, "references_3_other_s": 50.3}


def edit_json(path: str, edit) -> None:
    doc = read_json(path)
    edit(doc)
    write_json(path, doc)


def set_record_in(path: str, metric: str, value=None, noise="keep", base_a_noise="keep", sha=None) -> None:
    """The record of `metric` in the file at `path`: its value, its noise and, for memory, the noise of A against A."""
    def edit(doc):
        entry = doc["metrics"][metric]
        if value is not None:
            entry["value"] = value
        if noise != "keep":
            entry["noise"]["value"] = noise
        if base_a_noise != "keep":
            entry["noise"]["base_A_noise"]["value"] = base_a_noise
        if sha is not None:
            entry["record_sha"] = sha
    edit_json(path, edit)


class RecordCase(GateCase):
    """A gate whose record holds exactly the numbers of the world (`MEASURED`)
    with a noise so wide that nothing counts as a loss or as an improvement;
    a test narrows the metric it is about."""

    def edit_baseline(self, edit) -> None:
        edit_json(self.baseline_path, edit)

    def set_record(self, metric: str, value=None, noise="keep", base_a_noise="keep", sha=None) -> None:
        set_record_in(self.baseline_path, metric, value, noise, base_a_noise, sha)

    def baseline_doc(self) -> dict:
        return read_json(self.baseline_path)

    def baseline_bytes(self) -> bytes:
        with open(self.baseline_path, "rb") as fh:
            return fh.read()

    def record_row(self, summary: dict, metric: str) -> dict:
        return next(r for r in summary["record"]["rows"] if r["metric"] == metric)

    def report_text(self, label: str) -> str:
        with open(self.data.path("gates", label, "report.md"), encoding="utf-8") as fh:
            return fh.read()

    def break_gate_noise(self, metric: str) -> None:
        """The calibration of the old rule loses the noise of `metric`: that row has no decision."""
        def edit(noise):
            noise["entries"][verdict.noise_key("erp", gate.SCENARIO, gate.TEMP, metric)]["noise"] = None
        edit_json(self.data.path("gate-noise.json"), edit)


def held(count: int = 3, **metrics) -> list[dict]:
    """`count` runs of the gate that held, each with the same numbers."""
    return [{"k": k, "runs": gate.RUNS, "held": True, "reason": None, "metrics": dict(metrics), "calls": []}
            for k in range(1, count + 1)]


def round_of(candidate=(), B=(), base=()) -> dict:
    """One round of the gate as `gate._gate` collects it."""
    found = gate.new_round()
    found.update(candidate=list(candidate), B=list(B), base=list(base))
    return found


def full_round(**overrides) -> dict:
    """A round whose every record metric has its number from `MEASURED`, or from `overrides`."""
    values = {**MEASURED, **overrides}
    a_keys = [k for k, (_, aggregate) in verdict.GATE_RECORD.items() if aggregate == "median_A"]
    b_keys = [k for k, (_, aggregate) in verdict.GATE_RECORD.items() if aggregate == "median_B"]
    return round_of(candidate=held(**{k: values[k] for k in a_keys}, hwm_kb=values["hwm_kb"]),
                    B=held(**{k: values[k] for k in b_keys}, hwm_kb=values["hwm_kb"]))
