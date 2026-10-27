"""The nightly set, its calibration and the night itself (plan §7.3–§7.4,
§8.6, §10.2 step 4, §11).

`night` builds develop (or a candidate tag) and the last release, checks their
help, measures the nightly set for both in one window and judges the
candidate against the release measured that night. With no current noise
calibration it measures calibration series of the release instead, two a
night, and writes `noise.json` after the fifth. Other `bsl-analyzer*` processes
on the machine do not stop or cancel anything: their load is part of what the
night and its calibration measure. Diff, triage and publication of issues are
later steps of the plan and are not run here.
"""

from __future__ import annotations

import argparse
import datetime
import fcntl
import glob
import os
import re
import subprocess

import corpus
import perf_nightly
import verdict
from common import HARNESS_SCHEMA, Data, HarnessError, git, median, read_json, write_json

RUNS = 3
CAL_PER_NIGHT = 2
CAL_RE = re.compile(r"cal-(\d+)")
DATE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")
# The nightly set (plan §8.4): which corpora each unit measures. `mcp` is the
# warm MCP group: mcp-memory (its first run warms the cache), mcp-start, mcp-load.
SUITE = (
    ("analyze", ("crm", "ut", "erp", "lombard")),
    ("lsp-session", ("crm", "ut")),
    ("lsp-close-memory", ("ut",)),
    ("lsp-workspace-diag", ("crm", "ut")),
    ("mcp", ("crm", "ut")),
)
MCP_GROUP = ("mcp-memory", "mcp-start", "mcp-load")
# Measured for the candidate only, in one run: an absolute limit needs no base,
# and three two-minute samplings would not fit into the window twice.
CANDIDATE_ONLY = ("lsp-close-memory",)
CANDIDATE_ONLY_RUNS = 1
# Corpora whose number of `analyze` diagnostics the report follows from night to night.
DIAG_COUNT_CORPORA = ("ut",)
DIAG_STATE_TEXT = {
    "missing": "нет удержанного прогона кандидата",
    "unstable": "нестабильно между прогонами",
    "no_previous": "нет прошлой записи",
    "changed": "**изменилось**",
    "same": "без изменений",
}
# Seconds per run before any night has been measured (plan §11.1).
UNIT_SECONDS = {
    ("analyze", "crm"): 29.4, ("analyze", "ut"): 93.2,
    ("analyze", "lombard"): 164.3, ("analyze", "erp"): 238.6,
    ("lsp-session", "crm"): 28.8, ("lsp-session", "ut"): 132.6,
    ("lsp-workspace-diag", "crm"): 46.9, ("lsp-workspace-diag", "ut"): 143.5,
    ("mcp-start", "crm"): 5.2, ("mcp-start", "ut"): 5.2,
    ("mcp-memory", "crm"): 74.9, ("mcp-memory", "ut"): 182.3,
    ("mcp-load-c2", "crm"): 74.9, ("mcp-load-c4", "crm"): 79.9, ("mcp-load-c4", "ut"): 193.0,
    # lsp-session up to its last call, three cycles of 120 s sampling and their opens.
    ("lsp-close-memory", "ut"): 520.0,
}
UNKNOWN_SECONDS = 300.0


def suite_units(corpora: list[str], role: str | None = None) -> list[tuple[str, str]]:
    """[(corpus, unit)] of the set over the manifest corpora, corpus-major; a
    `base` side has no candidate-only units."""
    return [(c, unit) for c in corpora for unit, members in SUITE
            if c in members and not (role == "base" and unit in CANDIDATE_ONLY)]


def unit_sdirs(corpus_id: str, unit: str) -> list[str]:
    if unit == "mcp":
        loads = [f"mcp-load-c{n}" for n in perf_nightly.DEFAULT_CLIENTS.get(corpus_id, [4])]
        return ["mcp-memory", "mcp-start", *loads]
    return [unit]


def expected_keys(corpora: list[str], role: str | None = None) -> list[tuple[str, str, str]]:
    return [(c, sdir, "warm") for c, unit in suite_units(corpora, role) for sdir in unit_sdirs(c, unit)]


def series_runs(sdir: str) -> int:
    return CANDIDATE_ONLY_RUNS if sdir in CANDIDATE_ONLY else RUNS


def runs_of(sdir: str) -> int:
    """Launches a warm series may take: the warm-up run comes on top of N;
    scenarios on a cache another scenario warmed have none."""
    if sdir in CANDIDATE_ONLY or sdir.startswith(("mcp-start", "mcp-load")):
        return series_runs(sdir)
    return RUNS + 1


class Estimates:
    """Expected seconds of one run: the same run of the previous night, else
    the units of plan §11 (И31)."""

    def __init__(self, history: dict | None):
        self.history = history or {}

    def run(self, role: str, corpus_id: str, sdir: str) -> float:
        seen = self.history.get(f"{role}/{corpus_id}/{sdir}")
        if seen:
            return float(median(seen))
        return UNIT_SECONDS.get((sdir, corpus_id), UNKNOWN_SECONDS)

    def series(self, role: str, corpora: list[str]) -> float:
        return sum(self.run(role, c, sdir) * runs_of(sdir) for c, sdir, _ in expected_keys(corpora, role))


def _summaries_before(data: Data, before: str):
    """(date, summary) of the nights strictly before the date `before`, latest first."""
    dates = sorted(d for d in os.listdir(data.path("nights")) if DATE_RE.fullmatch(d) and d < before) \
        if os.path.isdir(data.path("nights")) else []
    for date in reversed(dates):
        path = data.path("nights", date, "summary.json")
        if os.path.isfile(path):
            yield date, read_json(path)


def previous_history(data: Data, before: str) -> dict | None:
    """Durations of the latest night strictly before the date `before`."""
    for _, summary in _summaries_before(data, before):
        if summary.get("durations"):
            return summary["durations"]
    return None


def previous_diag_counts(data: Data, before: str) -> dict:
    """{corpus: (count, date)}: the candidate's number of diagnostics from the
    latest night strictly before `before` that has one for that corpus of
    `DIAG_COUNT_CORPORA`; older nights are not read once every corpus has one."""
    found = {}
    for date, summary in _summaries_before(data, before):
        for corpus_id in DIAG_COUNT_CORPORA:
            item = (summary.get("diag_counts") or {}).get(corpus_id) or {}
            if corpus_id not in found and item.get("candidate") is not None:
                found[corpus_id] = (item["candidate"], date)
        if len(found) == len(DIAG_COUNT_CORPORA):
            break
    return found


def _diag_values(records: list) -> list:
    return sorted({r["metrics"].get("total_diagnostics") for r in records if r["held"]} - {None})


def diag_counts(base_groups: dict, cand_groups: dict, previous: dict) -> dict:
    """Per corpus of `DIAG_COUNT_CORPORA`: the candidate's number of `analyze`
    diagnostics against the previous night's; a note of the report, not of the
    verdict. The base is shown only: develop adds diagnostics against a release."""
    counts = {}
    for corpus_id in DIAG_COUNT_CORPORA:
        key = (corpus_id, "analyze", "warm")
        if key not in base_groups and key not in cand_groups:
            continue
        values = _diag_values(cand_groups.get(key, []))
        base = _diag_values(base_groups.get(key, []))
        before, night = previous.get(corpus_id, (None, None))
        candidate = values[0] if len(values) == 1 else None
        if not values:
            state = "missing"
        elif len(values) > 1:
            state = "unstable"
        elif before is None:
            state = "no_previous"
        else:
            state = "changed" if candidate != before else "same"
        counts[corpus_id] = {"candidate": candidate, "values": values,
                             "base": base[0] if len(base) == 1 else None,
                             "previous": before, "previous_night": night, "state": state}
    return counts


class Window:
    """Deadline as a point on the injected clock: a run starts only when its
    estimate ends by then (plan §7.4)."""

    def __init__(self, clock, deadline_s: float | None):
        self.clock = clock
        self.at = None if deadline_s is None else clock.now() + deadline_s

    def fits(self, seconds: float) -> bool:
        return self.at is None or self.clock.now() + seconds <= self.at


def seconds_until(hhmm: str, now: datetime.datetime) -> float:
    """Seconds from `now` to the next `HH:MM` local time."""
    match = re.fullmatch(r"(\d{2}):(\d{2})", hhmm)
    if not match or int(match.group(1)) > 23 or int(match.group(2)) > 59:
        raise HarnessError("deadline_invalid", f"{hhmm}: expected HH:MM, 00:00-23:59")
    hour, minute = int(match.group(1)), int(match.group(2))
    at = now.replace(hour=hour, minute=minute, second=0, microsecond=0)
    if at <= now:
        at += datetime.timedelta(days=1)
    return (at - now).total_seconds()


class Session:
    """Runs series of the nightly set for given binaries on one clock and
    window, and remembers what happened."""

    def __init__(self, args, clock, data: Data, corpora: list[str], window: Window,
                 estimates: Estimates):
        self.args, self.clock, self.data = args, clock, data
        self.corpora = corpora
        self.window, self.estimates = window, estimates
        self.survivors: dict = {}
        self.skipped: list[dict] = []
        self.refused: dict = {}
        self.durations: dict = {}

    def measure(self, role: str, binary: str, corpus_id: str, scenario: str, series: str) -> str:
        """One `measure` invocation: `done`, `skipped` (the window cut a run)
        or `refused` (the corpus is refused for this binary)."""
        a = argparse.Namespace(data_dir=self.data.root, corpora=self.args.corpora, corpus=corpus_id,
                               binary=binary, scenario=scenario, temp="warm", series=series,
                               runs=series_runs(scenario), clients=None)

        def fits(m, s) -> bool:
            seconds = self.estimates.run(role, corpus_id, s["sdir"])
            if self.window.fits(seconds):
                return True
            self.skipped.append({"role": role, "corpus": corpus_id, "scenario": s["sdir"],
                                 "estimate_s": seconds})
            return False

        m = perf_nightly.Measure(a, self.clock, self.data, foreign_prefix=None, window=fits,
                                 survivors=self.survivors)
        try:
            m.run()
        except HarnessError as exc:
            self.refused[(role, corpus_id)] = str(exc)
            return "refused"
        finally:
            self._collect(role, corpus_id, m, series)
        return "skipped" if m.skipped else "done"

    def _collect(self, role: str, corpus_id: str, m, series: str) -> None:
        for path in glob.glob(os.path.join(m.results, corpus_id, f"{m.scenario}*", "warm", series, "*.json")):
            record = read_json(path)
            if isinstance(record.get("duration_s"), (int, float)):
                sdir = path.split(os.sep)[-4]
                self.durations.setdefault(f"{role}/{corpus_id}/{sdir}", []).append(record["duration_s"])

    def suite(self, sides: list[tuple[str, str]], series: str) -> None:
        """The set for each (role, binary) side, interleaved per unit so a
        window that runs out cuts both sides alike."""
        lsp_done: set = set()
        for corpus_id, unit in suite_units(self.corpora):
            for role, binary in sides:
                if (role, corpus_id) in self.refused:
                    continue
                if unit in CANDIDATE_ONLY:
                    if role == "base":
                        continue
                    if (role, corpus_id) not in lsp_done:
                        # It runs on the cache a held lsp-session warmed; without one there is none.
                        self.skipped.append({"role": role, "corpus": corpus_id, "scenario": unit,
                                             "estimate_s": None})
                        continue
                for scenario in (MCP_GROUP if unit == "mcp" else (unit,)):
                    status = self.measure(role, binary, corpus_id, scenario, series)
                    if status == "refused":
                        break
                    if status == "done" and scenario == "lsp-session" and perf_nightly.lsp_warm(
                            self.data.results(perf_nightly.load_binary(binary)), corpus_id, series):
                        lsp_done.add((role, corpus_id))
                    if status == "skipped" and scenario == "mcp-memory":
                        # Without its warm-up the MCP group has no warm cache to measure on.
                        self.skipped.append({"role": role, "corpus": corpus_id,
                                             "scenario": "mcp-start, mcp-load", "estimate_s": None})
                        break


# -- calibration ------------------------------------------------------------------


def fingerprints(data: Data, binary: dict, corpora: list[str]) -> dict:
    found = {}
    for corpus_id in corpora:
        meta = corpus.load_snapshot(data, corpus_id)
        _, points_sha = corpus.load_points(data, corpus_id)
        found[corpus_id] = perf_nightly.env_fingerprint(binary, meta, points_sha)
    return found


def cal_series(results_dir: str) -> list[int]:
    numbers = set()
    for path in glob.glob(os.path.join(results_dir, "*", "*", "*", "cal-*")):
        match = CAL_RE.fullmatch(os.path.basename(path))
        if match:
            numbers.add(int(match.group(1)))
    return sorted(numbers)


def complete(groups: dict, keys: list, prints: dict) -> bool:
    """Every unit of the set has N runs, none skipped or cancelled, all with
    the current fingerprint of its corpus."""
    for key in keys:
        records = groups.get(key, [])
        if len(records) < RUNS:
            return False
        for record in records:
            if record.get("reason") in verdict.MISSING_REASONS:
                return False
            if verdict.comparable(record.get("env_fingerprint")) != verdict.comparable(prints.get(key[0])):
                return False
    return True


def current_cal(results_dir: str, corpora: list[str], prints: dict) -> list[str]:
    keys = expected_keys(corpora, "base")
    return [f"cal-{n}" for n in cal_series(results_dir)
            if complete(verdict.load_series(results_dir, f"cal-{n}", set(corpora)), keys, prints)]


def write_noise(data: Data, binary: dict, corpora: list[str], prints: dict) -> dict | None:
    """`noise.json` from the latest five complete calibration series with the
    current fingerprint (§8.6, И45, И46); None until there are five."""
    results_dir = data.results(binary)
    names = current_cal(results_dir, corpora, prints)[-verdict.NOISE_SERIES:]
    if len(names) < verdict.NOISE_SERIES:
        return None
    groups = [verdict.load_series(results_dir, name, set(corpora)) for name in names]
    noise = {
        "schema": verdict.NOISE_SCHEMA,
        "harness_schema": HARNESS_SCHEMA,
        "binary": {"label": binary["label"], "sha": binary["sha"]},
        "temp": "warm",
        "series": names,
        "fingerprints": prints,
        "created_at": verdict.now_iso(),
        "entries": verdict.build_noise(groups),
    }
    noise["self_check"] = verdict.self_check(groups, noise)
    write_json(data.path("noise.json"), noise)
    return noise


def read_noise(data: Data) -> dict | None:
    path = data.path("noise.json")
    return read_json(path) if os.path.isfile(path) else None


def calibrate(session: Session, binary: dict, corpora: list[str], prints: dict, count: int) -> dict:
    """Up to `count` new calibration series of the release, each started only
    when its whole estimate fits the window; `noise.json` once five complete
    series with the current fingerprint exist."""
    results_dir = session.data.results(binary)
    measured = []
    noise = write_noise(session.data, binary, corpora, prints)
    for _ in range(count if noise is None else 0):
        estimate = session.estimates.series("base", corpora)
        if not session.window.fits(estimate):
            session.skipped.append({"role": "base", "corpus": "*", "scenario": "calibration series",
                                    "estimate_s": estimate})
            break
        name = f"cal-{max(cal_series(results_dir), default=0) + 1}"
        session.suite([("base", binary["path"])], name)
        measured.append(name)
        noise = write_noise(session.data, binary, corpora, prints)
        if noise is not None:
            break
    return {"measured": measured, "noise_written": noise is not None,
            "complete": current_cal(results_dir, corpora, prints)}


# -- base selection and builds ----------------------------------------------------


def select_base(clone: str, cand_sha: str) -> str:
    """The highest `vX.Y.Z` tag that is an ancestor of the candidate and not
    the candidate itself (И35)."""
    tags = [t for t in git("tag", "--list", "v*", cwd=clone).split() if verdict.version_key(t)]
    for tag in sorted(tags, key=verdict.version_key, reverse=True):
        sha = git("rev-parse", f"{tag}^{{commit}}", cwd=clone)
        if sha == cand_sha:
            continue
        ancestor = subprocess.run(["git", "merge-base", "--is-ancestor", sha, cand_sha], cwd=clone,
                                  capture_output=True)
        if ancestor.returncode == 0:
            return tag
    raise HarnessError("no_base", "no release tag is an ancestor of the candidate")


def ensure_help(data: Data, binary: dict, help_section: dict, clock) -> None:
    """`helpcheck` for a binary without a current `helpcheck.json`."""
    path = os.path.join(data.results(binary), "helpcheck.json")
    identity = perf_nightly.help_identity(binary, help_section)
    if os.path.isfile(path):
        check = read_json(path)
        if check.get("help") == identity and not check.get("override"):
            return
    if binary.get("has_platform_help"):
        record = perf_nightly.run_helpcheck(data, binary, help_section, clock)
    else:
        record = {"passed": True, "reason": None, "builtin": True}
    record.update(help=identity, binary={"label": binary["label"], "sha": binary["sha"]}, override=False)
    write_json(path, record)


# -- night ------------------------------------------------------------------------


def diff_missing(base_groups: dict, cand_groups: dict, corpora: list[str]) -> list[str]:
    """Corpora whose r1 `analyze` did not hold on either side: the diagnostics
    diff of that corpus cannot be built (И47)."""
    missing = []
    for corpus_id in corpora:
        key = (corpus_id, "analyze", "warm")
        if key not in expected_keys(corpora):
            continue
        sides = [next((r for r in groups.get(key, []) if r.get("k") == 1), None)
                 for groups in (base_groups, cand_groups)]
        if any(r is None or not r["held"] for r in sides):
            missing.append(corpus_id)
    return missing


def write_report(out_dir: str, summary: dict, rows: list, table: dict | None) -> None:
    lines = [f"# perf-nightly {summary['name']}", "",
             f"- вердикт: **{summary['verdict']}**" + (f" ({'; '.join(summary['reasons'])})"
                                                      if summary["reasons"] else ""),
             f"- вид: {summary['kind']}",
             f"- кандидат: {summary['candidate']['label']} {summary['candidate']['sha'][:12]}",
             f"- база: {summary['base']['label']} {summary['base']['sha'][:12]}",
             f"- серия: {summary['series']}",
             f"- начало: {summary['started_at']}, конец: {summary['finished_at']}, "
             f"длительность замеров: {summary['elapsed_s'] / 60:.1f} мин",
             f"- дедлайн: {summary['deadline'] or '-'}",
             f"- шум: {summary['noise']['status']}",
             f"- не сравнено: {summary['not_compared']}, пропущено: {len(summary['skipped'])}", ""]
    if summary.get("calibration"):
        cal = summary["calibration"]
        lines += ["## Калибровка", "",
                  f"- серии этой ночи: {', '.join(cal['measured']) or 'нет'}",
                  f"- полные серии с текущим отпечатком: {', '.join(cal['complete']) or 'нет'}",
                  f"- noise.json записан: {'да' if cal['noise_written'] else 'нет'}", ""]
    if summary["skipped"]:
        lines += ["## Пропущено по окну", ""]
        lines += [f"- {s['role']} {s['corpus']} {s['scenario']} (оценка "
                  f"{verdict.fmt(s['estimate_s'])} с)" for s in summary["skipped"]]
        lines.append("")
    if summary.get("close_memory"):
        growth = verdict.CLOSE_GROWTH_LIMIT_KB / 1024
        over = verdict.CLOSE_OVER_READY_LIMIT_KB / 1024
        lines += ["## Память после закрытия документов", "",
                  f"Issue: прирост цикла 2 и далее больше {growth:.0f} МБ (цикл 1 считается от памяти "
                  f"готовности и в это правило не идёт); RSS после закрытия любого цикла над памятью "
                  f"готовности больше {over:.0f} МБ; глобальный запрос после повторного открытия "
                  f"(циклы 2 и 3) дольше {verdict.GLOBAL_REOPEN_LIMIT_S} с.", ""]
        for item in summary["close_memory"]:
            spent = "-" if item["duration_s"] is None else f"{item['duration_s'] / 60:.1f}"
            lines += [f"### {item['label']}", "",
                      f"- время сценария: {spent} мин (оценка {item['estimate_s'] / 60:.1f} мин)",
                      f"- исход: {verdict.OUTCOME_TEXT.get(item['outcome'], item['outcome'])}", ""]
            if item["cycles"]:
                lines += ["| цикл | открытие, с | глобальный запрос, с | RSS после закрытия, МБ | "
                          "над готовностью, МБ | прирост, МБ |", "|---|---|---|---|---|---|"]
                for c in item["cycles"]:
                    ready = item["rss_ready_kb"]
                    over_ready = "-" if ready is None else f"{(c['rss_closed_kb'] - ready) / 1024:+.1f}"
                    lines.append(f"| {c['cycle']} | {verdict.fmt(c['open_s'])} | {verdict.fmt(c.get('global_s'))} | "
                                 f"{c['rss_closed_kb'] / 1024:.1f} | {over_ready} | {c['growth_kb'] / 1024:+.1f} |")
            else:
                lines.append(f"- прогон не состоялся: {item['reason'] or 'нет записи'}")
            lines.append("")
    if summary.get("diag_counts"):
        lines += ["## Число диагностик", "",
                  "`analyze` кандидата против прошлой ночи; в вердикт не идёт.", "",
                  "| корпус | кандидат | прогоны | база | прошлая ночь | состояние |", "|---|---|---|---|---|---|"]
        for corpus_id, item in summary["diag_counts"].items():
            before = "-" if item["previous"] is None else f"{item['previous']} ({item['previous_night']})"
            lines.append(f"| {corpus_id} | {verdict.fmt(item['candidate'])} | "
                         f"{', '.join(str(v) for v in item['values']) or '-'} | {verdict.fmt(item['base'])} | "
                         f"{before} | {DIAG_STATE_TEXT[item['state']]} |")
        lines.append("")
    if summary["refused"]:
        lines += ["## Отказы корпусов", ""] + [f"- {k}: {v}" for k, v in summary["refused"].items()] + [""]
    if rows:
        flagged = [r for r in rows if r["outcome"] not in ("ok", "n/a")]
        lines += ["## Метрики с исходом не ok", ""] + verdict.ROW_HEADER
        lines += [verdict.row_line(r) for r in flagged] or ["| - | - | - | - | - | - | - | - | - |"]
        lines += ["", "## Оба невалидны", ""]
        both = [r for r in rows if r["outcome"] == "both_invalid"]
        lines += [f"- {r['corpus']} / {r['scenario']} / {r['metric']}: база: {r['errors']['base']}; "
                  f"кандидат: {r['errors']['candidate']}" for r in both] or ["- нет"]
        lines += ["", "## Все метрики", ""] + verdict.ROW_HEADER + [verdict.row_line(r) for r in rows]
    if table is not None:
        lines += ["", "## Тренд", ""] + verdict.trend_lines(table)
    with open(os.path.join(out_dir, "report.md"), "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


def run_night(args, clock, *, now: datetime.datetime | None = None, builder=None) -> dict:
    data = Data(args.data_dir)
    now = now or datetime.datetime.now()
    date = args.date or now.strftime("%Y-%m-%d")
    gate = args.candidate
    if gate is not None and verdict.version_key(gate) is None:
        raise HarnessError("candidate_invalid", f"{gate}: a candidate is a vX.Y.Z tag")
    out_dir = data.path("gates", gate) if gate else data.path("nights", date)
    summary_path = os.path.join(out_dir, "summary.json")
    if os.path.isfile(summary_path):
        done = read_json(summary_path)
        # A night that measured nothing (a gate waiting for calibration) has no
        # results to protect and runs again.
        if done.get("state") == "done" and done.get("measured", True):
            return done
    os.makedirs(data.root, exist_ok=True)
    lock = open(data.path("night.lock"), "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        raise HarnessError("night_busy", "another night holds this data dir") from None
    try:
        return _night(args, clock, data, now, date, gate, out_dir, builder)
    finally:
        lock.close()


def _night(args, clock, data, now, date, gate, out_dir, builder) -> dict:
    manifest = corpus.load_manifest(args.corpora or data.path("corpora.toml"))
    corpora = [c for c in manifest["corpora"] if any(c in members for _, members in SUITE)]
    series = f"gate-{gate}" if gate else f"night-{date}"
    started_at, began = verdict.now_iso(), clock.now()
    window = Window(clock, seconds_until(args.deadline, now) if args.deadline else None)
    summary = {"state": "done", "kind": "gate" if gate else "night", "name": gate or date, "date": date,
               "series": series, "started_at": started_at, "deadline": args.deadline,
               "skipped": [], "refused": {}, "durations": {}, "reasons": [], "not_compared": 0,
               "candidate": {"label": gate or "develop", "sha": ""}, "base": {"label": "", "sha": ""},
               "noise": {"status": "-"}, "measured": False}

    def finish(rows, table, result):
        summary.update(finished_at=verdict.now_iso(), elapsed_s=clock.now() - began,
                       verdict=result["verdict"], reasons=result["reasons"],
                       counts=result.get("counts", {}), not_compared=verdict.not_compared(rows))
        os.makedirs(out_dir, exist_ok=True)
        if rows:
            write_json(os.path.join(out_dir, "compare.json"), {"rows": rows})
        write_report(out_dir, summary, rows, table)
        write_json(os.path.join(out_dir, "summary.json"), summary)
        return summary

    clone = data.path("build")
    if builder is None:
        perf_nightly.build_clone(data, getattr(args, "remote", None))

        def builder(ref):
            return perf_nightly.build_ref(data, ref)[0]
    cand_ref = gate or "develop"
    cand_path = builder(cand_ref)
    cand = perf_nightly.load_binary(cand_path)
    base_tag = select_base(clone, cand["sha"])
    base = perf_nightly.load_binary(builder(base_tag))
    summary["candidate"] = {"label": cand["label"], "sha": cand["sha"]}
    summary["base"] = {"label": base["label"], "sha": base["sha"]}
    for binary in (cand, base):
        ensure_help(data, binary, manifest["platform_help"], clock)

    estimates = Estimates(previous_history(data, date))
    session = Session(args, clock, data, corpora, window, estimates)
    prints = fingerprints(data, base, corpora)
    stale = verdict.noise_stale(read_noise(data), prints)
    summary["noise"] = {"status": stale or "current"}
    if stale is not None:
        if gate:
            # A gate is judged against calibrated noise only; it waits for the recalibration.
            return finish([], None, verdict.night_verdict([], stale=stale))
        summary["kind"] = "calibration"
        summary["series"] = "cal-*"
        summary["measured"] = True
        summary["calibration"] = calibrate(session, base, corpora, prints, CAL_PER_NIGHT)
        if summary["calibration"]["noise_written"]:
            summary["noise"] = {"status": "written"}
        summary.update(skipped=session.skipped, refused=_refused(session), durations=session.durations)
        return finish([], None, verdict.night_verdict([], stale=stale))

    summary["measured"] = True
    session.suite([("base", base["path"]), ("candidate", cand["path"])], series)
    summary.update(skipped=session.skipped, refused=_refused(session), durations=session.durations)
    base_groups = verdict.load_series(data.results(base), series, set(corpora))
    cand_groups = verdict.load_series(data.results(cand), series, set(corpora))
    keys = expected_keys(corpora)
    rows = verdict.compare(base_groups, cand_groups, read_noise(data), None, units=keys)
    summary["close_memory"] = close_memory(manifest, corpora, cand_groups, rows, session)
    summary["diag_counts"] = diag_counts(base_groups, cand_groups, previous_diag_counts(data, date))
    summary["diff"] = {"not_built": diff_missing(base_groups, cand_groups, corpora)}
    result = verdict.night_verdict(rows, diff_missing=summary["diff"]["not_built"])
    extra = [] if gate else [(os.path.basename(data.results(cand)), data.results(cand), series)]
    table = verdict.trend(data.path("results"), cand_groups, extra)
    return finish(rows, table, result)


def close_memory(manifest: dict, corpora: list[str], cand_groups: dict, rows: list,
                 session: Session) -> list[dict]:
    """Per corpus of `lsp-close-memory`: its cycles, outcome, spent and expected time."""
    found = []
    for corpus_id, unit in suite_units(corpora):
        if unit not in CANDIDATE_ONLY:
            continue
        records = cand_groups.get((corpus_id, unit, "warm"), [])
        record = records[0] if records else {}
        metrics = record.get("metrics") or {}
        spent = session.durations.get(f"candidate/{corpus_id}/{unit}")
        own = [r for r in rows if r["corpus"] == corpus_id and r["scenario"] == unit]
        found.append({
            "corpus": corpus_id, "label": manifest["corpora"][corpus_id]["label"],
            "cycles": metrics.get("cycles") or [],
            "rss_ready_kb": metrics.get("rss_ready_kb"),
            "reason": record.get("reason"),
            "outcome": verdict.night_verdict(own)["verdict"] if own else "incomplete",
            "duration_s": sum(spent) if spent else None,
            "estimate_s": session.estimates.run("candidate", corpus_id, unit) * runs_of(unit),
        })
    return found


def _refused(session: Session) -> dict:
    return {f"{role}/{corpus_id}": reason for (role, corpus_id), reason in session.refused.items()}


def summary_line(summary: dict) -> str:
    return (f"{summary['kind']}={summary['name']} verdict={summary['verdict']} "
            f"reasons={'; '.join(summary['reasons']) or '-'} skipped={len(summary['skipped'])} "
            f"not_compared={summary['not_compared']}")
