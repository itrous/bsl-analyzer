#!/usr/bin/env python3
"""The pio job module of the nightly measurement (plan §10).

Reads one pio job request from stdin and answers one JSON `{state, runs,
notices}` on stdout. A night is keyed by the local date of its slot: the module
updates `<data_dir>/src` to origin/develop and runs `perf_nightly.py night
--publish` there, unless that night is already `done`; then it answers from the
night's files again, so a repeated run measures and publishes nothing and
returns the same notices (И32). Notices cover the night's verdict or failure,
its `gh` errors and the outcomes earlier nights left behind (И26).
"""

from __future__ import annotations

import datetime
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))

import publish  # noqa: E402
from common import read_json, write_json  # noqa: E402

PROTOCOL = 1
DEFAULT_CORPORA = "corpora.toml"
DEFAULT_DEADLINE = "08:45"
DATE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")
PUBLISHED = ("create", "comment", "commented", "label")


class RequestError(Exception):
    pass


def night_date(request: dict) -> str:
    """The local date of the slot, or of `now` for a run outside the schedule."""
    moment = request["slot"] if request.get("slot") is not None else request["now"]
    return datetime.datetime.fromtimestamp(moment).strftime("%Y-%m-%d")


def nights_behind(nights: str, date: str) -> list[str]:
    """Nights before `date` from the latest `done` one on: a run killed with
    its night answered nothing, so every night after that `done` may still
    owe its outcomes."""
    if not os.path.isdir(nights):
        return []
    earlier = sorted(d for d in os.listdir(nights) if DATE_RE.fullmatch(d) and d < date)
    done = [d for d in earlier if (read_summary(os.path.join(nights, d)) or {}).get("state") == "done"]
    return [d for d in earlier if not done or d >= done[-1]]


def read_summary(night_dir: str) -> dict | None:
    path = os.path.join(night_dir, "summary.json")
    return read_json(path) if os.path.isfile(path) else None


def last_line(text: str) -> str | None:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else None


def run_night(data_dir: str, night_dir: str, date: str, params: dict) -> str | None:
    """Updates the harness and runs the night; the reason of a failure, else None."""
    src = os.path.join(data_dir, "src")
    corpora = os.path.join(data_dir, params.get("corpora") or DEFAULT_CORPORA)
    steps = [
        ("git fetch", ["git", "-C", src, "fetch", "origin"]),
        ("git checkout", ["git", "-C", src, "checkout", "--detach", "origin/develop"]),
        ("night", [sys.executable, os.path.join(src, "scripts", "perf-nightly", "perf_nightly.py"), "night",
                   "--data-dir", data_dir, "--corpora", corpora,
                   "--deadline", params.get("deadline") or DEFAULT_DEADLINE, "--date", date, "--publish"]),
    ]
    for name, command in steps:
        try:
            # A foreign command may print bytes that are not UTF-8; its failure
            # must still reach the night as a reason, not crash the module.
            result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                    errors="replace")
        except OSError as exc:
            return f"{name}: {exc}"
        if name == "night":
            with open(os.path.join(night_dir, "night-stderr.txt"), "w", encoding="utf-8") as fh:
                fh.write(result.stderr)
        if result.returncode != 0:
            return last_line(result.stderr) or f"{name}: exit {result.returncode}"
    return None


def gh_errors(date: str, night_dir: str) -> list[dict]:
    errors = [e for e in publish.read_log(night_dir) if e["action"] == "gh-error"]
    return [{"key": f"{date}:gh-error:{n}", "text": f"perf-nightly {date}: ошибка gh — {e['detail']}"}
            for n, e in enumerate(errors, 1)]


def verdict_text(date: str, summary: dict, night_dir: str) -> str:
    reasons = summary.get("reasons") or []
    numbers = sorted({e["issue"] for e in publish.read_log(night_dir)
                      if e["action"] in PUBLISHED and e["issue"] is not None})
    issues = ", ".join(f"#{n}" for n in numbers) or "нет"
    return (f"perf-nightly {date}: {summary['verdict']}" + (f" ({'; '.join(reasons)})" if reasons else "") +
            f", issue: {issues}, diff: -, не сравнено: {summary.get('not_compared', 0)}, "
            f"пропущено: {len(summary.get('skipped') or [])}")


def night_notices(date: str, night_dir: str, summary: dict) -> list[dict]:
    if summary["state"] == "failed":
        own = [{"key": f"{date}:failed", "text": f"perf-nightly {date}: сбой — {summary['reason']}"}]
    else:
        own = [{"key": f"{date}:verdict", "text": verdict_text(date, summary, night_dir)}]
    return own + gh_errors(date, night_dir)


def left_behind(date: str, night_dir: str) -> list[dict]:
    """Outcomes of an earlier night its own answer could not carry: a night
    killed before its summary (none, or still `running`), and its `gh` errors."""
    found = []
    summary = read_summary(night_dir)
    if summary is None or summary.get("state") not in ("done", "failed"):
        found.append({"key": f"{date}:unfinished", "text": f"perf-nightly {date}: ночь не завершена"})
    return found + gh_errors(date, night_dir)


def handle(request: dict) -> dict:
    if request.get("protocol") != PROTOCOL:
        raise RequestError(f"протокол {request.get('protocol')} не поддерживается, модуль понимает {PROTOCOL}")
    data_dir = request["data_dir"]
    params = request.get("params") or {}
    date = night_date(request)
    nights = os.path.join(data_dir, "nights")
    night_dir = os.path.join(nights, date)
    notices = []
    for before in nights_behind(nights, date):
        notices += left_behind(before, os.path.join(nights, before))
    summary = read_summary(night_dir)
    if summary is None or summary.get("state") != "done":
        # A night killed by the timeout leaves `running`, also over the summary
        # of an earlier failed attempt, and the next night reports it.
        write_json(os.path.join(night_dir, "summary.json"), {"state": "running", "date": date})
        reason = run_night(data_dir, night_dir, date, params)
        summary = read_summary(night_dir)
        if reason is None and (summary is None or summary.get("state") != "done"):
            reason = "night wrote no done summary.json"
        if reason is not None:
            summary = {"state": "failed", "date": date, "reason": reason}
            write_json(os.path.join(night_dir, "summary.json"), summary)
    notices += night_notices(date, night_dir, summary)
    verdict = "failed" if summary["state"] == "failed" else summary["verdict"]
    return {"state": {"last_night": date, "verdict": verdict}, "runs": [], "notices": notices}


def main() -> int:
    try:
        request = json.loads(sys.stdin.read())
        response = handle(request)
    except (RequestError, KeyError, ValueError, OSError) as exc:
        print(f"perf-nightly module: {exc}", file=sys.stderr)
        return 1
    sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
