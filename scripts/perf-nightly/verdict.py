"""Verdict rules, noise calibration and the trend table (plan §8.5–§8.6).

A metric is one (corpus, scenario, temperature, metric) — for calls also the
call. Each side of a comparison is a series of N runs and gets a state:
`valid`, `invalid`, `failed`, `missing` or `n/a`; the pair of states decides
the outcome (the table of §8.6), and only a valid pair is compared by Δ
against the calibrated noise of that metric.
"""

from __future__ import annotations

import datetime
import glob
import os
import re
from dataclasses import dataclass

from common import HARNESS_SCHEMA, MISSING_REASONS, HarnessError, median, read_json

NOISE_SCHEMA = 1
NOISE_SERIES = 5
BLOCK_DELTA = 0.40
ISSUE_DELTA = 0.10
NOISY = 0.40
COMPLETENESS = 0.99
# Recorded with every run but left out of comparability: routine system updates
# change them, and both sides of a nightly verdict are measured with the same ones.
RECORDED_ONLY = ("kernel", "rustc", "mem_total")
# Outcomes that come from an actual comparison; a pair (scenario, corpus) of
# the set without any of them keeps the night from `ok` (И42).
COMPARED = ("ok", "improved", "issue", "block")
LABELS = {"block": ["perf", "regression", "release-blocker"], "issue": ["perf", "regression"]}
OUTCOME_TEXT = {
    "ok": "ok",
    "improved": "улучшение",
    "issue": "issue perf",
    "block": "блок релиза",
    "fixed": "исправлено, только отчёт",
    "both_invalid": "не сравнено: оба невалидны",
    "incomplete": "не сравнено: неполно",
    "noise_stale": "не сравнено: калибровка",
    "no_noise": "не сравнено: калибровка",
    "noisy": "не сравнено: шум",
    "help": "не сравнено: справка",
    "incomparable": "несравнимо",
    "n/a": "n/a",
}
CALLS_LSP_REPEAT = ("L1", "L2", "L3", "L4", "L6", "G1", "G3")
CALLS_MCP = ("M1", "M2", "M3", "M5", "M6", "M7")


@dataclass(frozen=True)
class Metric:
    """One metric of the verdict. `call` None means a run-level value from
    `metrics`; `kind` is `delta` (Δ against noise), `growth` (D8),
    `counter` (candidate above base is an issue, И38) or `completeness`."""

    key: str
    field: str
    call: str | None = None
    kind: str = "delta"
    lenient: bool = False


def _run(field: str, kind: str = "delta") -> Metric:
    return Metric(field, field, None, kind)


def _call(call: str, field: str, kind: str = "delta", lenient: bool = False) -> Metric:
    return Metric(f"{call}.{field}", field, call, kind, lenient)


VERDICT_METRICS = {
    "analyze": [_run("wall_s"), _run("max_rss_kb")],
    "lsp-session": [
        _run("init_s"), _run("ready_s"), _run("rss_ready_kb"),
        _call("L5", "publish_s"), _call("L5s", "publish_s"),
        *[_call(c, f) for c in CALLS_LSP_REPEAT for f in ("first_s", "repeat_s")],
        _call("G2", "first_s"), _call("G4", "first_s"),
        _run("rss_after_global_kb"),
    ],
    "lsp-workspace-diag": [
        _run("ready_s"), _call("wsdiag", "first_s"), _run("rss_peak_kb"),
        _call("wsdiag", "count", "completeness"),
    ],
    "mcp-start": [_run("init_s"), _call("tools", "first_s")],
    "mcp-memory": [
        _run("rss_start_kb"), _run("rss_ready_kb"), _run("rss_series_kb"),
        _run("rss_growth_kb", "growth"), _run("ready_s"),
        *[_call(c, f) for c in CALLS_MCP for f in ("first_s", "repeat_s")],
    ],
    # In mcp-load a failed call is counted in `errors`, not an invalid side (§8.3).
    "mcp-load": [
        _run("p50_s"), _run("p95_s"), _run("timeouts", "counter"),
        *[_call(c, f, lenient=True) for c in CALLS_MCP for f in ("p50_s", "p95_s")],
        *[_call(c, "errors", "counter", lenient=True) for c in CALLS_MCP],
    ],
}


def scenario_of(sdir: str) -> str:
    return "mcp-load" if sdir.startswith("mcp-load") else sdir


def metrics_of(sdir: str) -> list[Metric]:
    return VERDICT_METRICS.get(scenario_of(sdir), [])


# -- series ---------------------------------------------------------------------


def load_series(results_dir: str, series: str, corpora=None) -> dict:
    """{(corpus, sdir, temp): [r1..rN records]} of one series of one binary;
    the warm-up run is not part of a series."""
    groups: dict = {}
    for path in sorted(glob.glob(os.path.join(results_dir, "*", "*", "*", series, "r*.json"))):
        parts = path.split(os.sep)
        corpus_id, sdir, temp = parts[-5], parts[-4], parts[-3]
        if corpora is not None and corpus_id not in corpora:
            continue
        groups.setdefault((corpus_id, sdir, temp), []).append(read_json(path))
    for records in groups.values():
        records.sort(key=lambda r: r.get("k") or 0)
    return groups


def _number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _entry(record: dict, call: str):
    return next((c for c in record.get("calls") or [] if c.get("id") == call), None)


def side(records: list, metric: Metric) -> tuple[str, float | None]:
    """(state, value) of one side; value is the median of N runs, or the sum
    for counters (plan §8.3)."""
    if not records:
        return "missing", None
    if len(records) < (records[0].get("runs") or len(records)):
        return "missing", None
    if any(not r["held"] for r in records):
        if any(r.get("reason") in MISSING_REASONS for r in records):
            return "missing", None
        return "failed", None
    if metric.call is None:
        values = [r["metrics"].get(metric.field) for r in records]
    else:
        entries = [_entry(r, metric.call) for r in records]
        if any(e is None or e.get("na") for e in entries):
            return "n/a", None
        if not metric.lenient and not all(e.get("ok") for e in entries):
            return "invalid", None
        values = [e.get(metric.field) for e in entries]
    if not all(_number(v) for v in values):
        return "invalid", None
    return "valid", float(sum(values) if metric.kind == "counter" else median(values))


def _rss_ready(records: list) -> float | None:
    values = [r["metrics"].get("rss_ready_kb") for r in records]
    return float(median(values)) if values and all(_number(v) for v in values) else None


def comparable(fingerprint: dict | None) -> dict | None:
    """The part of `env_fingerprint` that decides whether two series compare."""
    if fingerprint is None:
        return None
    return {k: v for k, v in fingerprint.items() if k not in RECORDED_ONLY}


def series_meta(records: list) -> dict:
    first = records[0] if records else {}
    return {"fingerprint": comparable(first.get("env_fingerprint")), "help": first.get("help")}


# -- noise ----------------------------------------------------------------------


def noise_key(corpus_id: str, sdir: str, temp: str, key: str) -> str:
    return f"{corpus_id}/{sdir}/{temp}/{key}"


def build_noise(series_groups: list[dict]) -> dict:
    """{noise key: {medians, noise}} over calibration series (И45):
    (max − min) / median of the series medians; for memory growth the spread
    of growth over the median `rss_ready_kb` of the same series (D8). A metric
    that is not valid in every series gets `noise: null` and its reason."""
    entries = {}
    keys = sorted({k for groups in series_groups for k in groups})
    for corpus_id, sdir, temp in keys:
        for metric in metrics_of(sdir):
            if metric.kind not in ("delta", "growth"):
                continue
            medians, ready, states = [], [], []
            for groups in series_groups:
                records = groups.get((corpus_id, sdir, temp), [])
                state, value = side(records, metric)
                states.append(state)
                if state == "valid":
                    medians.append(value)
                    ready.append(_rss_ready(records))
            entry = {"medians": medians, "states": states, "noise": None}
            if len(medians) == len(series_groups) and medians:
                spread = max(medians) - min(medians)
                if metric.kind == "growth":
                    scale = median(ready) if all(v is not None for v in ready) else None
                else:
                    scale = median(medians)
                if scale:
                    entry["noise"] = spread / scale
                else:
                    entry["reason"] = "zero median"
            else:
                entry["reason"] = "not valid in every series"
            entries[noise_key(corpus_id, sdir, temp, metric.key)] = entry
    return entries


def noise_stale(noise: dict | None, fingerprints: dict) -> str | None:
    """Why `noise.json` cannot judge this night (И46), or None."""
    if noise is None:
        return "no noise.json"
    if noise.get("harness_schema") != HARNESS_SCHEMA:
        return "harness schema changed"
    for corpus_id, fingerprint in fingerprints.items():
        if comparable((noise.get("fingerprints") or {}).get(corpus_id)) != comparable(fingerprint):
            return f"environment fingerprint of {corpus_id} changed"
    return None


def lookup_noise(noise: dict, corpus_id: str, sdir: str, temp: str, key: str) -> float | None:
    """noise(m); a (corpus, temperature) without calibration takes the largest
    noise of the metric over calibrated corpora of that temperature, but not
    less than this corpus's warm noise (D14)."""
    entries = noise.get("entries") or {}
    own = entries.get(noise_key(corpus_id, sdir, temp, key))
    if own is not None:
        return own.get("noise")
    calibrated = {k.split("/")[0] for k, v in entries.items() if k.split("/")[2] == temp}
    if corpus_id in calibrated:
        return None
    pattern = re.compile(rf"^[^/]+/{re.escape(sdir)}/{re.escape(temp)}/{re.escape(key)}$")
    others = [v.get("noise") for k, v in entries.items() if pattern.match(k)]
    warm = (entries.get(noise_key(corpus_id, sdir, "warm", key)) or {}).get("noise")
    candidates = [v for v in others if v is not None] + ([warm] if warm is not None else [])
    return max(candidates) if candidates else None


# -- comparison -----------------------------------------------------------------


def table_outcome(base: str, cand: str) -> str | None:
    """The table of §8.6 for every pair except valid/valid (None)."""
    if "n/a" in (base, cand):
        return "n/a"
    if "missing" in (base, cand):
        return "incomplete"
    if cand == "failed":
        return "both_invalid" if base == "failed" else "block"
    if cand == "invalid":
        return "issue" if base == "valid" else "both_invalid"
    if base in ("invalid", "failed"):
        return "fixed"
    return None


def delta_outcome(delta: float, noise: float) -> str:
    if delta > BLOCK_DELTA:
        return "block"
    if delta >= ISSUE_DELTA and delta > noise:
        return "issue"
    if delta <= -ISSUE_DELTA and abs(delta) > noise:
        return "improved"
    return "ok"


def compare_metric(metric: Metric, base_records: list, cand_records: list, noise: dict | None,
                   stale: str | None, where: tuple) -> dict:
    corpus_id, sdir, temp = where
    b_state, b_value = side(base_records, metric)
    c_state, c_value = side(cand_records, metric)
    row = {"corpus": corpus_id, "scenario": sdir, "temp": temp, "metric": metric.key,
           "call": metric.call or "run", "kind": metric.kind,
           "base": {"state": b_state, "value": b_value}, "candidate": {"state": c_state, "value": c_value},
           "delta": None, "noise": None}
    b_meta, c_meta = series_meta(base_records), series_meta(cand_records)
    outcome = None
    if base_records and cand_records:
        if b_meta["help"] != c_meta["help"]:
            outcome = "help"
        elif b_meta["fingerprint"] != c_meta["fingerprint"]:
            outcome = "incomparable"
    if outcome is None:
        outcome = table_outcome(b_state, c_state)
    if outcome is None and stale is not None:
        outcome = "noise_stale"
    if outcome is None:
        if metric.kind == "counter":
            outcome = "issue" if c_value > b_value else "ok"
        elif metric.kind == "completeness":
            outcome = "issue" if c_value < COMPLETENESS * b_value else "ok"
        else:
            if metric.kind == "growth":
                scale = _rss_ready(base_records)
            else:
                scale = b_value
            delta = (c_value - b_value) / scale if scale else (0.0 if c_value == b_value else float("inf"))
            row["delta"] = delta
            value = lookup_noise(noise, corpus_id, sdir, temp, metric.key)
            row["noise"] = value
            if value is None:
                outcome = "no_noise"
            elif value > NOISY:
                outcome = "noisy"
            else:
                outcome = delta_outcome(delta, value)
    row["outcome"] = outcome
    row["labels"] = LABELS.get(outcome, [])
    if outcome == "both_invalid":
        row["errors"] = {"base": _first_error(base_records, metric),
                         "candidate": _first_error(cand_records, metric)}
    return row


def _first_error(records: list, metric: Metric):
    for record in records:
        if not record["held"]:
            return record.get("reason")
        entry = _entry(record, metric.call) if metric.call else None
        if entry is not None and not entry.get("ok"):
            return entry.get("error")
    return None


def compare(base_groups: dict, cand_groups: dict, noise: dict | None, stale: str | None,
            units=None) -> list[dict]:
    """Rows of every verdict metric over the given (corpus, sdir, temp) units,
    by default the union of both sides."""
    keys = sorted(units if units is not None else set(base_groups) | set(cand_groups))
    rows = []
    for where in keys:
        for metric in metrics_of(where[1]):
            rows.append(compare_metric(metric, base_groups.get(where, []), cand_groups.get(where, []),
                                       noise, stale, where))
    return rows


def night_verdict(rows: list[dict], *, stale: str | None = None,
                  diff_missing: list | None = None) -> dict:
    """block > issue > incomplete > ok (И14, И42, И46, И47)."""
    outcomes = [r["outcome"] for r in rows]
    reasons = []
    if any(o == "incomplete" for o in outcomes):
        reasons.append("missing")
    pairs: dict = {}
    for row in rows:
        pairs.setdefault((row["scenario"], row["corpus"]), []).append(row["outcome"])
    uncompared = sorted(f"{scenario}/{corpus_id}" for (scenario, corpus_id), got in pairs.items()
                        if not any(o in COMPARED for o in got))
    if uncompared:
        reasons.append("uncompared: " + ", ".join(uncompared))
    if stale:
        reasons.append("noise_stale")
    if diff_missing:
        reasons.append("diff_not_built: " + ", ".join(diff_missing))
    if "block" in outcomes:
        verdict = "block"
    elif "issue" in outcomes:
        verdict = "issue"
    elif reasons:
        verdict = "incomplete"
    else:
        verdict = "ok"
    return {"verdict": verdict, "reasons": reasons, "uncompared": uncompared,
            "counts": {o: outcomes.count(o) for o in sorted(set(outcomes))}}


def not_compared(rows: list[dict]) -> int:
    return sum(1 for r in rows if r["outcome"] not in COMPARED + ("fixed", "n/a"))


# -- noise self-check -----------------------------------------------------------


def self_check(series_groups: list[dict], noise: dict) -> dict:
    """For every metric with noise, the series whose value is the median taken
    as base against each of the others: no `issue` and no `block`, since by
    construction |Δ| ≤ noise(m) — an outcome here means the threshold of
    noise was not applied."""
    bad, checked = [], 0
    for key, entry in sorted((noise.get("entries") or {}).items()):
        if entry.get("noise") is None or entry["noise"] > NOISY:
            continue
        corpus_id, sdir, temp, metric_key = key.split("/", 3)
        metric = next(m for m in metrics_of(sdir) if m.key == metric_key)
        values = entry["medians"]
        middle = values.index(sorted(values)[len(values) // 2])
        base = series_groups[middle].get((corpus_id, sdir, temp), [])
        for index, groups in enumerate(series_groups):
            if index == middle:
                continue
            row = compare_metric(metric, base, groups.get((corpus_id, sdir, temp), []), noise, None,
                                 (corpus_id, sdir, temp))
            checked += 1
            if row["outcome"] in ("issue", "block"):
                bad.append({"metric": key, "series": index, "outcome": row["outcome"], "delta": row["delta"]})
    return {"checked": checked, "violations": bad}


# -- trend ----------------------------------------------------------------------

TAG_RE = re.compile(r"v(\d+)\.(\d+)\.(\d+)")


def version_key(tag: str):
    match = TAG_RE.fullmatch(tag)
    return tuple(int(x) for x in match.groups()) if match else None


def release_dirs(results_root: str) -> list[tuple[str, str]]:
    """[(tag, results dir)] of measured releases, oldest first."""
    found = []
    for name in os.listdir(results_root) if os.path.isdir(results_root) else []:
        tag = name.rsplit("-", 1)[0]
        if version_key(tag) is not None:
            found.append((tag, os.path.join(results_root, name)))
    return sorted(found, key=lambda item: version_key(item[0]))


def latest_series(results_dir: str, where: tuple, fingerprint: dict | None) -> tuple[str | None, bool]:
    """(series name, any series seen) of the most recent nightly, gate or
    calibration series of a release with this fingerprint at (corpus, sdir,
    temp); controls never feed the trend."""
    best, best_at, any_seen = None, "", False
    for path in glob.glob(os.path.join(results_dir, *where, "*", "r1.json")):
        series = path.split(os.sep)[-2]
        if not series.startswith(("night-", "gate-", "cal-")):
            continue
        record = read_json(path)
        any_seen = True
        if comparable(record.get("env_fingerprint")) != comparable(fingerprint):
            continue
        started = record.get("started_at") or ""
        if started >= best_at:
            best, best_at = series, started
    return best, any_seen


def trend(results_root: str, cand_groups: dict, extra: list[tuple[str, str, str]] = ()) -> dict:
    """Columns: every measured release, then the given (name, dir, series)
    sides. Cells hold the median of a valid side, its state, or
    `несравнимо` for a series with another fingerprint (И17, И36);
    a column whose help differs from the candidate's is marked (И52)."""
    columns, cells = [], {}
    for where in sorted(cand_groups):
        corpus_id, sdir, temp = where
        cand = cand_groups[where]
        meta = series_meta(cand)
        for tag, rdir in release_dirs(results_root):
            name = os.path.basename(rdir)
            if name not in columns:
                columns.append(name)
            series, seen = latest_series(rdir, where, meta["fingerprint"])
            records = load_series(rdir, series, {corpus_id}).get(where, []) if series else []
            for metric in metrics_of(sdir):
                key = (corpus_id, sdir, temp, metric.key)
                if series is None:
                    cells.setdefault(key, {})[name] = {"text": "несравнимо" if seen else "-"}
                    continue
                state, value = side(records, metric)
                help_mark = None
                other = series_meta(records)["help"] if records else None
                if other and meta["help"] and other != meta["help"]:
                    help_mark = "справка встроенная" if other.get("source") == "builtin" else "справка другая"
                cells.setdefault(key, {})[name] = {"value": value, "state": state, "series": series,
                                                   "help": help_mark}
        for name, rdir, series in extra:
            if name not in columns:
                columns.append(name)
            records = load_series(rdir, series, {corpus_id}).get(where, [])
            for metric in metrics_of(sdir):
                state, value = side(records, metric)
                cells.setdefault((corpus_id, sdir, temp, metric.key), {})[name] = {
                    "value": value, "state": state, "series": series}
    return {"columns": columns, "cells": cells}


def fmt(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            return str(value)
        return f"{value:.3f}" if abs(value) < 100 else f"{value:.1f}"
    return str(value)


def trend_cell(cell: dict | None) -> str:
    if not cell:
        return "-"
    if "text" in cell:
        return cell["text"]
    text = fmt(cell["value"]) if cell["state"] == "valid" else cell["state"]
    return f"{text} ({cell['help']})" if cell.get("help") else text


def trend_lines(table: dict) -> list[str]:
    columns = table["columns"]
    marks = {}
    for cell_map in table["cells"].values():
        for name, cell in cell_map.items():
            if cell.get("help"):
                marks[name] = cell["help"]
    header = [f"{c} ({marks[c]})" if c in marks else c for c in columns]
    lines = ["| corpus | scenario | temp | metric | " + " | ".join(header) + " |",
             "|" + "---|" * (4 + len(columns))]
    for (corpus_id, sdir, temp, key), cell_map in sorted(table["cells"].items()):
        values = [trend_cell(cell_map.get(c)) for c in columns]
        lines.append(f"| {corpus_id} | {sdir} | {temp} | {key} | " + " | ".join(values) + " |")
    return lines


def row_line(row: dict) -> str:
    delta = "-" if row["delta"] is None else f"{row['delta'] * 100:+.1f}%"
    noise = "-" if row["noise"] is None else f"{row['noise'] * 100:.1f}%"
    b, c = row["base"], row["candidate"]
    b_text = fmt(b["value"]) if b["state"] == "valid" else b["state"]
    c_text = fmt(c["value"]) if c["state"] == "valid" else c["state"]
    return (f"| {row['corpus']} | {row['scenario']} | {row['temp']} | {row['metric']} | {b_text} | "
            f"{c_text} | {delta} | {noise} | {OUTCOME_TEXT[row['outcome']]} |")


ROW_HEADER = ["| corpus | scenario | temp | metric | база | кандидат | Δ | шум | исход |",
              "|---|---|---|---|---|---|---|---|---|"]


def now_iso() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def resolve_side(results_root: str, spec: str) -> tuple[str, str]:
    """`<label>[-<sha12>]/<series>` -> (results dir, series); a bare label must
    name exactly one measured binary."""
    if "/" not in spec:
        raise HarnessError("side_invalid", f"{spec}: expected <label>/<series>")
    name, series = spec.rsplit("/", 1)
    exact = os.path.join(results_root, name)
    if os.path.isdir(exact):
        return exact, series
    matches = sorted(glob.glob(os.path.join(results_root, glob.escape(name) + "-" + "[0-9a-f]" * 12)))
    if len(matches) != 1:
        raise HarnessError("side_invalid", f"{name}: {len(matches)} measured binaries match; pass <label>-<sha12>")
    return matches[0], series
