"""Publication of perf issues (plan §9.7, D10, D12, D17).

One issue per (scenario, call, metric) of the night's `issue` and `block` rows;
its corpora are listed in the body. A key line in the body finds the issue
again: an open one gets one comment per night, a closed one is reopened by a
new issue only when it was completed. Every action and every `gh` error is a
line of `publish.log` in the night's directory.

The record of the night (`publish_record`) has a group of its own: one open
issue per metric over the record, one per incomparable profile, found among
all issues of the repository, not only the labelled ones.
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
RATCHET_KEY = "perf-ratchet/"
PROFILE_KEY = RATCHET_KEY + "profile"
BLOCK_LABELS = ["perf", "regression", "release-blocker"]
INCOMPARABLE = ("baseline_missing", "baseline_invalid", "record_no_noise", "record_profile_mismatch",
                "record_protocol_mismatch")
# Older issues of the release job name the failed metric only in their title.
RELEASE_TITLE_RE = re.compile(r"Ночной релиз .+: гейт (?:\w+ )?провален по (.+)")
OWNER_TARGET_TEXT = ("Цель 4 667 316 KB задана для общего пика; исходное число — серия A v0.4.2. Общий пик кандидата "
                     "включает references B. Следующий релиз заблокирован до возврата с допуском ν; исправление "
                     "памяти — отдельная задача.")


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


# -- the record of the night ----------------------------------------------------------


def _pages(text: str) -> list:
    """The items of `gh api --paginate`: one array, or one array per page."""
    decoder = json.JSONDecoder()
    items, position = [], 0
    text = text.strip()
    while position < len(text):
        page, position = decoder.raw_decode(text, position)
        if not isinstance(page, list):
            raise ValueError("a page is not an array")
        items += page
        while position < len(text) and text[position].isspace():
            position += 1
    return items


def _read_issues(gh) -> list[dict]:
    """Every issue of the repository, in the states and reasons of `gh issue list`."""
    code, out, err = gh(["api", "--paginate", f"repos/{REPO}/issues?state=all&per_page=100"])
    if code != 0:
        raise HarnessError("gh_error", f"issues: {_error(code, out, err)}")
    try:
        items = _pages(out)
    except ValueError:
        raise HarnessError("gh_error", "issues: output is not JSON") from None
    return [{"number": i["number"], "state": i["state"].upper(), "stateReason": (i.get("state_reason") or "").upper(),
             "title": i.get("title") or "", "body": i.get("body") or "", "labels": i.get("labels") or []}
            for i in items if "pull_request" not in i]


def _read_comments(gh) -> dict[int, list[str]]:
    """{issue number: its comments} of the whole repository."""
    code, out, err = gh(["api", "--paginate", f"repos/{REPO}/issues/comments?per_page=100"])
    if code != 0:
        raise HarnessError("gh_error", f"comments: {_error(code, out, err)}")
    found: dict = {}
    try:
        for item in _pages(out):
            found.setdefault(int(item["issue_url"].rsplit("/", 1)[-1]), []).append(item.get("body") or "")
    except (ValueError, KeyError, TypeError):
        raise HarnessError("gh_error", "comments: output is not JSON") from None
    return found


def _title_names(title: str, metric: str) -> bool:
    found = RELEASE_TITLE_RE.fullmatch(title.strip())
    if not found:
        return False
    # The release job writes `graph_ready`, without the unit of the metric.
    words = set(WORD_RE.findall(found[1]))
    return metric in words or metric.removesuffix("_s") in words


def ratchet_groups(saved: dict) -> list[dict]:
    """What a record night has to say on GitHub. Only a comparison that was made
    claims an excess; a failed run has no number, and an incomparable profile
    says nothing about the product."""
    record = saved.get("record") or {}
    groups = [{"key": f"{RATCHET_KEY}{row['metric']}", "metric": row["metric"], "row": row, "labels": BLOCK_LABELS,
               "title": f"perf: рекорд превышен — {row['metric']}"}
              for row in record.get("rows") or [] if row["outcome"] == "block" and row["delta"] is not None]
    reasons = [r for r in record.get("reasons") or [] if r.startswith(INCOMPARABLE)]
    if reasons:
        groups.append({"key": PROFILE_KEY, "metric": None, "reasons": reasons, "labels": ["perf"],
                       "title": f"perf: рекорд несопоставим — {', '.join(reasons)}"})
    return groups


def _exact(value) -> str:
    return "-" if value is None else str(value)


def _candidates_text(record_sha: str, sha: str, ancestor) -> list[str]:
    """The range to look through. It names candidates, never a culprit."""
    uri = f"https://github.com/{REPO}/compare/{record_sha}...{sha}"
    notation = f"`{record_sha}..{sha}`"
    if record_sha == sha:
        return [f"Диапазон пуст: {notation}, кандидат — сам рекордсмен. Превышение воспроизводится на рекордсмене: "
                "проверить окружение и калибровку, последующих коммитов здесь нет."]
    lines = [f"Кандидаты для поиска причины (не найденный виновник): {notation}", uri]
    if not ancestor(record_sha, sha):
        lines.append(f"История разошлась: {record_sha} не предок {sha}, диапазон не линеен.")
    return lines + ["Если тот же холодный A/B на SHA рекордсмена при том же профиле тоже выше предела, разбор идёт к "
                    "окружению и калибровке, а не к этим коммитам."]


def _run_table(saved: dict, row: dict) -> list[str]:
    series = {"median_A": ("A",), "median_B": ("B",), "max_AB": ("A", "B")}[row["aggregate"]]
    lines = ["| запуск | серия | удержан | значение |", "|---|---|---|---|"]
    return lines + [f"| {r['run']} | {r['series']} | {str(r['held']).lower()} | {_exact(r['metrics'].get(row['metric']))} |"
                    for r in saved.get("run_values") or [] if r["series"] in series]


def _artifacts(saved: dict) -> list[str]:
    baseline, artifacts = saved.get("baseline") or {}, saved.get("artifacts") or {}
    return [f"Baseline: {baseline.get('path', '-')} в коммите харнесса {baseline.get('commit') or '-'}.",
            f"Отчёт: {artifacts.get('summary', '-')}, {artifacts.get('report', '-')} (каталог задания perf-nightly)."]


def _profile_lines(saved: dict) -> list[str]:
    record = saved.get("record") or {}
    measured, expected = record.get("profile") or {}, record.get("baseline_profile") or {}
    differ = sorted(k for k in measured.keys() | expected.keys() if measured.get(k) != expected.get(k))
    return [f"Профиль замера: {json.dumps(measured, ensure_ascii=False, sort_keys=True)}.",
            f"Отличия от профиля рекорда: {', '.join(differ) or 'нет'}."]


def ratchet_facts(group: dict, saved: dict, ancestor) -> list[str]:
    sha = saved["candidate"]["sha"]
    if group["metric"] is None:
        return [f"Рекорд не сравнивался с develop {sha}: {'; '.join(group['reasons'])}.",
                "Это несопоставимость замера, а не регрессия продукта. Значения рекорда не повышать: вернуть прежний "
                "профиль, заполнить шум измерением или вынести миграцию профиля отдельным решением."] + \
            _profile_lines(saved) + _artifacts(saved)
    row = group["row"]
    record, candidate = row["record"], row["candidate"]
    ceiling = next((c["limit"] for c in saved.get("ceilings") or [] if c["metric"] == row["metric"]), None)
    lines = [f"Метрика: {row['metric']}, {row['unit']}, агрегат {row['aggregate']}.",
             f"Рекорд R = {_exact(record['value'])}; кандидат C = {_exact(candidate['value'])}.",
             f"Δ = {row['delta'] * 100:+.2f}%; ν = {row['noise'] * 100:.2f}%; предел R·(1+ν) = {_exact(row['limit'])}; "
             f"абсолютный потолок: {_exact(ceiling)}.",
             f"SHA рекордсмена: {record['sha']}; develop: {sha}."]
    if (record.get("evidence") or {}).get("kind") == "owner_target":
        lines.append(OWNER_TARGET_TEXT)
    return lines + _candidates_text(record["sha"], sha, ancestor) + _profile_lines(saved) + \
        _artifacts(saved) + ["", *_run_table(saved, row)]


def _ensure_labels(log, gh, number: int, key: str, have: set, wanted: list[str]) -> None:
    for label in wanted:
        if label in have:
            continue
        check_labels([label])
        code, out, err = gh(["issue", "edit", str(number), "-R", REPO, "--add-label", label])
        if code != 0:
            log.add("gh-error", number, key, f"issue edit: {_error(code, out, err)}")
            return
        log.add("label", number, key, label)


def publish_record(out_dir: str, saved: dict, forbidden: list[str], ancestor, gh=run_gh) -> list[dict]:
    """Publishes one record night. A list that could not be read in full creates nothing."""
    log = Log(out_dir)
    groups = ratchet_groups(saved)
    if not groups:
        return log.entries
    try:
        issues = _read_issues(gh)
    except HarnessError as exc:
        log.add("gh-error", None, "*", exc.detail)
        return log.entries
    comments = None
    event = f"record-{saved['date']}-{saved['candidate']['sha'][:12]}"
    for group in groups:
        key, line = group["key"], KEY_PREFIX + group["key"]
        marker = NIGHT_PREFIX + event
        same = [i for i in issues if has_line(i["body"], line) or (
            group["metric"] is not None and _title_names(i["title"], group["metric"]))]
        found = [i for i in same if i["state"] == "OPEN"]
        keyed = []
        if not found:
            # A marker that lives only in a comment counts as well, here and for a decision of the owner.
            if comments is None:
                try:
                    comments = _read_comments(gh)
                except HarnessError as exc:
                    log.add("gh-error", None, key, exc.detail)
                    continue
            keyed = sorted((i for i in issues if i in same or
                            any(has_line(c, line) for c in comments.get(i["number"], []))), key=lambda i: i["number"])
            found = [i for i in keyed if i["state"] == "OPEN"]
        facts_lines = ratchet_facts(group, saved, ancestor)
        if found:
            target = min(found, key=lambda i: i["number"])
            adopt = [line] if not has_line(target["body"], line) else []
            body = "\n".join([f"Ночь {event}: {group['title']}.", "", *facts_lines, "", *adopt, marker]) + "\n"
            _comment(log, gh, target["number"], key, marker, target, body, forbidden)
            _ensure_labels(log, gh, target["number"], key, {label["name"] for label in target["labels"]},
                           group["labels"])
            continue
        if keyed and keyed[-1]["stateReason"] in OWNER_CLOSED:
            log.add("closed", keyed[-1]["number"], key, keyed[-1]["stateReason"])
            continue
        repeat = [f"повтор #{keyed[-1]['number']}"] if keyed else []
        body = "\n".join([f"{group['title']}, ночь {event}.", *repeat, "", *facts_lines, "", line, marker]) + "\n"
        if leaked(group["title"] + body, forbidden):
            log.add("refused", None, key, "forbidden text")
            continue
        check_labels(group["labels"])
        args = ["issue", "create", "-R", REPO, "--title", group["title"], "--body-file", "-"]
        for label in group["labels"]:
            args += ["--label", label]
        code, out, err = gh(args, body)
        if code != 0:
            log.add("gh-error", None, key, f"issue create: {_error(code, out, err)}")
            continue
        tail = out.strip().rstrip("/").rsplit("/", 1)[-1]
        log.add("create", int(tail) if tail.isdigit() else None, key,
                f"repeat of #{keyed[-1]['number']}" if keyed else "")
    return log.entries
