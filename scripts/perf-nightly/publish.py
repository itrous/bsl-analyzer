"""Publication of perf issues (plan §9.7, D10, D12, D17).

One issue per (scenario, call, metric) of the night's `issue` and `block` rows;
its corpora are listed in the body. A key line in the body finds the issue
again: an open one gets one comment per night, a closed one is reopened by a
new issue only when it was completed. Every action and every `gh` error is a
line of `publish.log` in the night's directory.
"""

from __future__ import annotations

import json
import os
import re
import subprocess

import corpus
import verdict
from common import HarnessError

REPO = "itrous/bsl-analyzer"
ALLOWED_LABELS = ("perf", "false-positive", "regression", "release-blocker")
LIST_LIMIT = 1000
KEY_PREFIX = "perf-nightly-key: "
NIGHT_PREFIX = "perf-nightly-night: "
OWNER_CLOSED = ("NOT_PLANNED", "DUPLICATE")
LOG = "publish.log"
WORD_RE = re.compile(r"[\w.-]+")


def run_gh(args: list[str], stdin: str | None = None) -> tuple[int, str, str]:
    try:
        result = subprocess.run(["gh", *args], input=stdin, capture_output=True, text=True)
    except OSError as exc:
        return 127, "", f"gh: {exc}"
    return result.returncode, result.stdout, result.stderr


def perf_key(row: dict) -> str:
    return f"perf/{row['scenario']}/{row['call']}/{row['metric'].split('.', 1)[-1]}"


def perf_groups(rows: list[dict]) -> list[dict]:
    """[{key, rows, block}] of the rows with outcome `issue` or `block`, by key."""
    groups: dict = {}
    for row in rows:
        if row["outcome"] in ("issue", "block"):
            groups.setdefault(perf_key(row), []).append(row)
    return [{"key": key, "rows": found, "block": any(r["outcome"] == "block" for r in found)}
            for key, found in sorted(groups.items())]


def labels_of(group: dict) -> list[str]:
    labels = ["perf", "regression"] + (["release-blocker"] if group["block"] else [])
    check_labels(labels)
    return labels


def check_labels(labels: list[str]) -> None:
    """Only the labels of the plan ever reach an issue (И28)."""
    wrong = [label for label in labels if label not in ALLOWED_LABELS]
    if wrong:
        raise HarnessError("publish_label", f"labels outside the allowed set: {', '.join(wrong)}")


def forbidden_strings(data, corpora: list[str]) -> list[str]:
    """Snapshot paths, point paths and their directories: none may be published (И29)."""
    found = set()
    for corpus_id in corpora:
        found.add(data.snapshot(corpus_id))
        doc, _ = corpus.load_points(data, corpus_id)
        for point in (doc or {}).get("points", []):
            path = point["relative_path"]
            found.add(path)
            found.update(part for part in os.path.dirname(path).split("/") if part)
    return sorted(found)


def leaked(text: str, forbidden: list[str]) -> list[str]:
    """Paths are looked for anywhere in the text; a single directory name only
    as a whole word, or a one-letter directory would match every text."""
    words = set(WORD_RE.findall(text))
    return [s for s in forbidden if (s in text if "/" in s else s in words)]


def _value(side: dict) -> str:
    return verdict.fmt(side["value"]) if side["state"] == "valid" else side["state"]


def night_name(summary: dict) -> str:
    return f"gate-{summary['name']}" if summary["kind"] == "gate" else summary["name"]


def facts(group: dict, summary: dict, table: dict | None, labels: dict, runs_of) -> list[str]:
    """The night's data on one key: versions, the row of each corpus, the trend."""
    cand, base = summary["candidate"], summary["base"]
    first = group["rows"][0]
    lines = [f"Кандидат: {cand['label']} {cand['sha'][:12]}; база: {base['label']} {base['sha'][:12]}.",
             f"Температура: {first['temp']}, N = {runs_of(first['scenario'])}.", "",
             "| конфигурация | база | кандидат | Δ | шум | исход |", "|---|---|---|---|---|---|"]
    for row in group["rows"]:
        base_text = f"≤ {verdict.fmt(row['limit'])}" if row["kind"] == "ceiling" else _value(row["base"])
        delta = "-" if row["delta"] is None else f"{row['delta'] * 100:+.1f}%"
        noise = "-" if row["noise"] is None else f"{row['noise'] * 100:.1f}%"
        lines.append(f"| {labels.get(row['corpus'], row['corpus'])} | {base_text} | {_value(row['candidate'])} | "
                     f"{delta} | {noise} | {verdict.OUTCOME_TEXT[row['outcome']]} |")
    cells = (table or {}).get("cells", {})
    columns = (table or {}).get("columns", [])
    trend = [(row, cells.get((row["corpus"], row["scenario"], row["temp"], row["metric"]))) for row in group["rows"]]
    if columns and any(cell_map for _, cell_map in trend):
        lines += ["", "Тренд:", "", "| конфигурация | " + " | ".join(columns) + " |",
                  "|" + "---|" * (1 + len(columns))]
        for row, cell_map in trend:
            values = [verdict.trend_cell((cell_map or {}).get(c)) for c in columns]
            lines.append(f"| {labels.get(row['corpus'], row['corpus'])} | " + " | ".join(values) + " |")
    return lines


def issue_text(group: dict, summary: dict, table, labels: dict, runs_of, repeat_of: int | None) -> tuple[str, str]:
    first = group["rows"][0]
    metric = first["metric"].split(".", 1)[-1]
    what = f"{first['scenario']} {metric}" if first["call"] == "run" else f"{first['scenario']} {first['call']} {metric}"
    title = f"perf: регрессия {what}"
    outcome = verdict.OUTCOME_TEXT["block" if group["block"] else "issue"]
    lines = [f"Регрессия производительности: {what} ({outcome}), ночь {night_name(summary)}."]
    if repeat_of is not None:
        lines.append(f"повтор #{repeat_of}")
    lines += [""] + facts(group, summary, table, labels, runs_of)
    lines += ["", f"{KEY_PREFIX}{group['key']}", f"{NIGHT_PREFIX}{night_name(summary)}"]
    return title, "\n".join(lines) + "\n"


def comment_text(group: dict, summary: dict, table, labels: dict, runs_of) -> str:
    outcome = verdict.OUTCOME_TEXT["block" if group["block"] else "issue"]
    lines = [f"Ночь {night_name(summary)}: {outcome}.", ""] + facts(group, summary, table, labels, runs_of)
    return "\n".join(lines + ["", f"{NIGHT_PREFIX}{night_name(summary)}"]) + "\n"


def has_line(text: str, line: str) -> bool:
    return any(l.strip() == line for l in (text or "").splitlines())


class Log:
    """`publish.log`: one tab-separated line per action — action, issue, key, detail."""

    def __init__(self, out_dir: str):
        self.path = os.path.join(out_dir, LOG)
        self.entries: list[dict] = []

    def add(self, action: str, number, key: str, detail: str = "") -> None:
        entry = {"action": action, "issue": number, "key": key, "detail": " ".join(detail.split())}
        self.entries.append(entry)
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        with open(self.path, "a", encoding="utf-8") as fh:
            fh.write("\t".join([action, "-" if number is None else str(number), key, entry["detail"]]) + "\n")


def read_log(out_dir: str) -> list[dict]:
    path = os.path.join(out_dir, LOG)
    if not os.path.isfile(path):
        return []
    entries = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            action, number, key, detail = (line.rstrip("\n").split("\t") + ["", "", ""])[:4]
            entries.append({"action": action, "issue": None if number in ("", "-") else int(number),
                            "key": key, "detail": detail})
    return entries


def _error(code: int, out: str, err: str) -> str:
    return (err.strip() or out.strip() or f"exit {code}").splitlines()[-1]


def publish_perf(out_dir: str, summary: dict, rows: list[dict], table, labels: dict, forbidden: list[str],
                 runs_of, gh=run_gh) -> list[dict]:
    """Publishes the night's perf groups; returns the `publish.log` entries it wrote."""
    log = Log(out_dir)
    groups = perf_groups(rows)
    if not groups:
        return log.entries
    code, out, err = gh(["issue", "list", "-R", REPO, "--state", "all", "--label", "perf",
                         "-L", str(LIST_LIMIT), "--json", "number,state,stateReason,body,labels"])
    if code != 0:
        log.add("gh-error", None, "*", f"issue list: {_error(code, out, err)}")
        return log.entries
    try:
        issues = json.loads(out or "[]")
    except ValueError:
        log.add("gh-error", None, "*", "issue list: output is not JSON")
        return log.entries
    if len(issues) >= LIST_LIMIT:
        log.add("gh-error", None, "*", f"issue list returned {len(issues)} issues: the list may be cut")
        return log.entries
    for group in groups:
        _publish_group(log, gh, group, issues, summary, table, labels, forbidden, runs_of)
    return log.entries


def _publish_group(log, gh, group, issues, summary, table, labels, forbidden, runs_of) -> None:
    key = group["key"]
    keyed = sorted((i for i in issues if has_line(i.get("body"), KEY_PREFIX + key)), key=lambda i: i["number"])
    open_issue = next((i for i in keyed if i["state"] == "OPEN"), None)
    if open_issue is None and keyed and keyed[-1].get("stateReason") in OWNER_CLOSED:
        log.add("closed", keyed[-1]["number"], key, keyed[-1]["stateReason"])
        return
    if open_issue is None:
        repeat_of = keyed[-1]["number"] if keyed else None
        title, body = issue_text(group, summary, table, labels, runs_of, repeat_of)
        if leaked(title + body, forbidden):
            log.add("refused", None, key, "forbidden text")
            return
        args = ["issue", "create", "-R", REPO, "--title", title, "--body-file", "-"]
        for label in labels_of(group):
            args += ["--label", label]
        code, out, err = gh(args, body)
        if code != 0:
            log.add("gh-error", None, key, f"issue create: {_error(code, out, err)}")
            return
        tail = out.strip().rstrip("/").rsplit("/", 1)[-1]
        log.add("create", int(tail) if tail.isdigit() else None, key,
                "" if repeat_of is None else f"repeat of #{repeat_of}")
        return
    number = open_issue["number"]
    marker = NIGHT_PREFIX + night_name(summary)
    _comment(log, gh, number, key, marker, open_issue, comment_text(group, summary, table, labels, runs_of),
             forbidden)
    # The label does not depend on the comment: a failed comment leaves a block unmarked otherwise.
    have = {label["name"] for label in open_issue.get("labels") or []}
    if group["block"] and "release-blocker" not in have:
        check_labels(["release-blocker"])
        code, out, err = gh(["issue", "edit", str(number), "-R", REPO, "--add-label", "release-blocker"])
        if code != 0:
            log.add("gh-error", number, key, f"issue edit: {_error(code, out, err)}")
            return
        log.add("label", number, key, "release-blocker")


def _comment(log, gh, number: int, key: str, marker: str, open_issue: dict, body: str, forbidden) -> None:
    """One comment of the night on an open issue that has none yet."""
    if has_line(open_issue.get("body"), marker):
        log.add("commented", number, key, "this night already")
        return
    code, out, err = gh(["api", f"repos/{REPO}/issues/{number}/comments", "--paginate", "--jq", ".[].body"])
    if code != 0:
        log.add("gh-error", number, key, f"comments: {_error(code, out, err)}")
    elif has_line(out, marker):
        log.add("commented", number, key, "this night already")
    elif leaked(body, forbidden):
        log.add("refused", number, key, "forbidden text")
    else:
        code, out, err = gh(["issue", "comment", str(number), "-R", REPO, "--body-file", "-"], body)
        if code != 0:
            log.add("gh-error", number, key, f"issue comment: {_error(code, out, err)}")
        else:
            log.add("comment", number, key)
