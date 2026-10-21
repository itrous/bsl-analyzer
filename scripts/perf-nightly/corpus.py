"""Corpus manifest, snapshot clones, working-config variants and call points
(plan §6.2–§6.3). Snapshot and point files are private; refusals name only
the check that failed, never a path, object or module of the corpus."""

from __future__ import annotations

import json
import os
import re
import tomllib

from common import HarnessError, git, read_json, sha256_bytes, sha256_file, write_json

CONFIG_NAME = "bsl-analyzer.toml"
HELP_SOURCES = ("external", "installed")
MODES = ("full", "diff")
HEADER_RE = re.compile(r"^\s*\[\[?\s*([^\[\]]+?)\s*\]\]?\s*(#.*)?$")
KEY_PART_RE = re.compile(r'\s*("(?:[^"\\]|\\.)*"|\'[^\']*\'|[^."\'\s]+)\s*(?:\.|$)')
DIFF_BASE_RE = re.compile(r"^\s*diff_base\s*=")
STRING_RE = re.compile(r'"(?:[^"\\]|\\.)*"|\'[^\']*\'')
# Tables the snapshot config drops: the corpus's own help source, and the shared
# semantic search baseline, which needs credentials the stand does not have and
# whose search tools the nightly set does not measure.
DROPPED_TABLES = (("platform_help",), ("search", "baseline"))

# Plan §6.3: key -> (discover feature, file class).
POINT_KEYS = {
    "F": ("document_symbol", "common_large"),
    "F_small": ("document_symbol", "common_small"),
    "L5": ("diagnostics_push", "common_large"),
    "L5s": ("diagnostics_push", "common_small"),
    "L6": ("inlay_hints", "common_large"),
    "P_hover": ("hover", "common_large"),
    "P_compl": ("completion", "common_large"),
    "P_def": ("goto_definition", "common_large"),
    "P_refs": ("references", "common_large"),
    "P_ren": ("rename", "common_large"),
    "Q": ("workspace_symbol", "scan_common_large"),
}
F_ROT_MAX = 6


def load_manifest(path: str) -> dict:
    with open(path, "rb") as fh:
        raw = tomllib.load(fh)
    help_section = raw.get("platform_help") or {}
    if help_section.get("source") not in HELP_SOURCES or not help_section.get("path"):
        raise HarnessError("manifest_invalid",
                           "[platform_help] needs source = external|installed and a path")
    corpora = {}
    for corpus in raw.get("corpus", []):
        for key in ("id", "label", "source_repo", "commit", "mode"):
            if not corpus.get(key):
                raise HarnessError("manifest_invalid", f"corpus entry without `{key}`")
        if corpus["mode"] not in MODES:
            raise HarnessError("manifest_invalid", f"corpus {corpus['id']}: mode must be full|diff")
        if corpus["mode"] == "diff" and not (corpus.get("diff_base_ref") and corpus.get("diff_base_sha")):
            raise HarnessError("manifest_invalid",
                               f"corpus {corpus['id']}: diff mode needs diff_base_ref and diff_base_sha")
        if corpus["id"] in corpora:
            raise HarnessError("manifest_invalid", f"corpus id {corpus['id']} appears twice")
        corpora[corpus["id"]] = corpus
    return {"platform_help": help_section, "corpora": corpora}


def _toml_value(value) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    return json.dumps(str(value), ensure_ascii=False)


def table_name(raw: str) -> tuple:
    """The key path a table header means: `a."b".c` and `a . b . c` are
    `("a", "b", "c")`, while `a."b.c"` is `("a", "b.c")` — a quoted part is one key."""
    parts = []
    for match in KEY_PART_RE.finditer(raw):
        part = match.group(1)
        parts.append(part[1:-1] if part[:1] in "\"'" else part)
    return tuple(parts)


def edit_config(text: str, help_section: dict | None, mode: str, diff_base_ref: str | None) -> str:
    """The working config: the corpus's own text with only `[platform_help]`,
    `[analysis].diff_base` and `[search.baseline]` changed, nothing else
    reformatted."""
    nl = "\r\n" if "\r\n" in text else "\n"
    lines = text.splitlines(keepends=True)
    kept, section, depth = [], None, 0
    for line in lines:
        # Inside a multi-line array a line like `["a"]` is a value, not a header.
        header = HEADER_RE.match(line) if depth == 0 else None
        if header:
            section = table_name(header.group(1))
        else:
            bare = STRING_RE.sub("", line).split("#", 1)[0]
            depth = max(0, depth + bare.count("[") - bare.count("]"))
        dropped = section is not None and any(section[:len(t)] == t for t in DROPPED_TABLES)
        # Comments and blank lines stay: they may belong to the next section.
        if dropped and line.strip() and not line.lstrip().startswith("#"):
            continue
        if section == ("analysis",) and DIFF_BASE_RE.match(line):
            continue
        kept.append(line)
    if kept and not kept[-1].endswith("\n"):
        kept[-1] += nl
    if mode == "diff":
        entry = f"diff_base = {_toml_value(diff_base_ref)}{nl}"
        for index, line in enumerate(kept):
            header = HEADER_RE.match(line)
            if header and table_name(header.group(1)) == ("analysis",) and not line.lstrip().startswith("[["):
                kept.insert(index + 1, entry)
                break
        else:
            kept.append(f"{nl}[analysis]{nl}{entry}")
    if help_section is not None:
        kept.append(f"{nl}[platform_help]{nl}")
        kept.extend(f"{key} = {_toml_value(value)}{nl}" for key, value in help_section.items())
    return "".join(kept)


def _strip_managed(parsed: dict) -> dict:
    parsed = dict(parsed)
    parsed.pop("platform_help", None)
    search = dict(parsed.get("search") or {})
    search.pop("baseline", None)
    if search:
        parsed["search"] = search
    else:
        parsed.pop("search", None)
    analysis = dict(parsed.get("analysis") or {})
    analysis.pop("diff_base", None)
    if analysis:
        parsed["analysis"] = analysis
    else:
        parsed.pop("analysis", None)
    return parsed


def check_variant(original: str, variant: str, help_section: dict | None, mode: str,
                  diff_base_ref: str | None) -> None:
    try:
        parsed = tomllib.loads(variant)
        source = tomllib.loads(original)
    except tomllib.TOMLDecodeError as exc:
        raise HarnessError("config_invalid", f"working config does not parse: {exc}") from exc
    if parsed.get("platform_help") != help_section:
        raise HarnessError("config_invalid", "[platform_help] differs from the manifest")
    if "baseline" in (parsed.get("search") or {}):
        raise HarnessError("config_invalid", "[search.baseline] survived the edit")
    diff_base = (parsed.get("analysis") or {}).get("diff_base")
    if diff_base != (diff_base_ref if mode == "diff" else None):
        raise HarnessError("config_invalid", f"analysis.diff_base does not match mode {mode}")
    if _strip_managed(parsed) != _strip_managed(source):
        raise HarnessError("config_invalid", "the edit changed keys other than the managed ones")


def count_sources(root: str) -> int:
    """`*.bsl` and `*.os` files, pruning `.build` and `.git` (plan §4.6)."""
    total = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in (".build", ".git")]
        total += sum(1 for n in filenames if n.endswith((".bsl", ".os")))
    return total


def make_snapshot(data, manifest: dict, corpus_id: str) -> dict:
    corpus = manifest["corpora"].get(corpus_id)
    if corpus is None:
        raise HarnessError("unknown_corpus", corpus_id)
    snap = data.snapshot(corpus_id)
    if os.path.isdir(snap):
        if git("rev-parse", "HEAD", cwd=snap) != corpus["commit"]:
            raise HarnessError("snapshot_mismatch", "an existing snapshot sits at another commit")
    else:
        os.makedirs(os.path.dirname(snap), exist_ok=True)
        git("clone", "--no-checkout", corpus["source_repo"], snap)
        git("checkout", "--detach", corpus["commit"], cwd=snap)
    ref = corpus.get("diff_base_ref") if corpus["mode"] == "diff" else None
    if ref:
        if not git("rev-parse", "--verify", "--quiet", f"{corpus['diff_base_sha']}^{{commit}}",
                   cwd=snap, check=False):
            raise HarnessError("snapshot_mismatch", "diff_base_sha is not a commit of the clone")
        git("branch", "-f", ref, corpus["diff_base_sha"], cwd=snap)
    if not os.path.isfile(os.path.join(snap, CONFIG_NAME)):
        raise HarnessError("snapshot_mismatch", f"{CONFIG_NAME} is missing in the snapshot root")
    # The checked-out text, not the blob: checkout may convert line endings.
    git("checkout", "--", CONFIG_NAME, cwd=snap)
    with open(os.path.join(snap, CONFIG_NAME), "rb") as fh:
        original = fh.read().decode("utf-8")
    variants = {}
    for name, help_section in (("help", manifest["platform_help"]), ("builtin", None)):
        text = edit_config(original, help_section, corpus["mode"], ref)
        check_variant(original, text, help_section, corpus["mode"], ref)
        variants[name] = {"text": text, "config_sha256": sha256_bytes(text.encode())}
    meta = {
        "id": corpus_id,
        "label": corpus["label"],
        "commit": corpus["commit"],
        "mode": corpus["mode"],
        "diff_base_ref": ref,
        "diff_base_sha": corpus.get("diff_base_sha") if ref else None,
        "variants": variants,
        "files_total": count_sources(snap),
    }
    write_json(data.snapshot_meta(corpus_id), meta)
    return meta


def load_snapshot(data, corpus_id: str) -> dict:
    path = data.snapshot_meta(corpus_id)
    if not os.path.isfile(path):
        raise HarnessError("snapshot_missing", f"run `snapshot --corpus {corpus_id}` first")
    return read_json(path)


def verify_snapshot(snap: str, meta: dict, variant: str, config_sha256: str | None = None) -> None:
    """Invariant И7: refuse before any binary runs on a drifted snapshot."""
    if git("rev-parse", "HEAD", cwd=snap) != meta["commit"]:
        raise HarnessError("snapshot_mismatch", "HEAD differs from the manifest commit")
    if meta.get("diff_base_ref"):
        moved = git("rev-parse", "--verify", "--quiet", f"refs/heads/{meta['diff_base_ref']}",
                    cwd=snap, check=False)
        if moved != meta["diff_base_sha"]:
            raise HarnessError("snapshot_mismatch", "the diff_base ref does not point at diff_base_sha")
    status = git("status", "--porcelain", "--untracked-files=no", cwd=snap).splitlines()
    extra = [line for line in status if line != f" M {CONFIG_NAME}"]
    if extra:
        raise HarnessError("snapshot_mismatch", f"{len(extra)} tracked file(s) changed besides the config")
    expected = config_sha256 or meta["variants"][variant]["config_sha256"]
    if sha256_file(os.path.join(snap, CONFIG_NAME)) != expected:
        raise HarnessError("snapshot_mismatch", f"the config is not the `{variant}` variant")


def _write_config(root: str, text: str) -> None:
    path = os.path.join(root, CONFIG_NAME)
    with open(path, "rb") as fh:
        current = fh.read()
    if current != text.encode():
        with open(path, "wb") as fh:
            fh.write(text.encode())


def prepare_snapshot(snap: str, meta: dict, variant: str) -> None:
    _write_config(snap, meta["variants"][variant]["text"])
    verify_snapshot(snap, meta, variant)


def shadow_config(text: str) -> str:
    """The variant with pull diagnostics forced on for the whole project."""
    lines = [line for line in text.splitlines(keepends=True)
             if not re.match(r"^\s*(workspace_diagnostics|workspaceDiagnostics)\s*=", line)]
    entry = 'workspace_diagnostics = "all"\n'
    for index, line in enumerate(lines):
        header = HEADER_RE.match(line)
        if header and table_name(header.group(1)) == ("features",):
            lines.insert(index + 1, entry)
            break
    else:
        lines.append(f"\n[features]\n{entry}")
    result = "".join(lines)
    if (tomllib.loads(result).get("features") or {}).get("workspace_diagnostics") != "all":
        raise HarnessError("config_invalid", "the shadow config does not enable workspace_diagnostics")
    return result


def prepare_shadow(snap: str, shadow: str, meta: dict, variant: str) -> None:
    """A git worktree of the snapshot at the same commit whose config turns
    on workspace diagnostics (plan §8.2); its files are the snapshot's bytes,
    so the diff_base area and the points stay valid in it."""
    if not os.path.isdir(shadow):
        os.makedirs(os.path.dirname(shadow), exist_ok=True)
        git("worktree", "add", "--force", "--detach", shadow, meta["commit"], cwd=snap)
    text = shadow_config(meta["variants"][variant]["text"])
    _write_config(shadow, text)
    verify_snapshot(shadow, meta, variant, sha256_bytes(text.encode()))


def is_ident_char(ch: str) -> bool:
    return ch.isalnum() or ch == "_"


def lsp_position(data: bytes, offset: int) -> tuple[int, int]:
    """Line and UTF-16 column of a byte offset in the raw text, BOM included
    (the server keeps the BOM verbatim)."""
    prefix = data[:offset].decode("utf-8")
    line = prefix.count("\n")
    last = prefix[prefix.rfind("\n") + 1:]
    return line, len(last.encode("utf-16-le")) // 2


def check_ident_start(data: bytes, offset: int) -> bool:
    try:
        prefix = data[:offset].decode("utf-8")
    except UnicodeDecodeError:
        return False
    head = data[offset:offset + 4].decode("utf-8", "ignore")[:1]
    if not head or not (head.isalpha() or head == "_"):
        return False
    return not prefix or not is_ident_char(prefix[-1])


def build_points(snap: str, discovered: dict, meta: dict, binary: dict) -> dict:
    points = []
    for target in discovered.get("targets", []):
        spec = target["spec"]
        with open(os.path.join(snap, target["relative_path"]), "rb") as fh:
            data = fh.read()
        point = {
            "id": target["id"],
            "relative_path": target["relative_path"],
            "file_sha256": sha256_bytes(data),
        }
        if "offset" in spec:
            offset = spec["offset"]
            if not check_ident_start(data, offset):
                raise HarnessError("points_invalid", f"point {target['id']}: offset is not an identifier start")
            point["offset"] = offset
            point["line"], point["character"] = lsp_position(data, offset)
        if "query" in spec:
            point["query"] = spec["query"]
        if "new_name" in spec:
            point["new_name"] = spec["new_name"]
        points.append(point)
    return {
        "snapshot_commit": meta["commit"],
        "discovered_by": {"label": binary["label"], "sha": binary["sha"]},
        "points": points,
    }


def select_points(points_doc: dict | None) -> dict:
    """Plan §6.3 keys -> point. `discover` numbers points per feature across
    the whole manifest, so a class's first point is the lowest id of that
    (feature, class), not necessarily `/01`."""
    chosen = {}
    points = (points_doc or {}).get("points", [])
    for key, (feature, cls) in POINT_KEYS.items():
        matching = [p for p in points if p["id"].split("/")[:2] == [feature, cls]]
        if matching:
            chosen[key] = min(matching, key=lambda p: p["id"])
    rot, seen = [], set()
    for point in sorted(points, key=lambda p: p["id"]):
        if point["id"].startswith("document_symbol/") and point["relative_path"] not in seen:
            seen.add(point["relative_path"])
            rot.append(point)
    chosen["F_rot"] = rot[:F_ROT_MAX]
    return chosen


def verify_points(snap: str, chosen: dict) -> None:
    """Invariant И8: every used file is byte-identical to discovery time and
    every offset still starts an identifier."""
    used = [p for key, p in chosen.items() if key != "F_rot"] + chosen.get("F_rot", [])
    for point in used:
        with open(os.path.join(snap, point["relative_path"]), "rb") as fh:
            data = fh.read()
        if sha256_bytes(data) != point["file_sha256"]:
            raise HarnessError("points_mismatch", f"point {point['id']}: file hash differs")
        if "offset" in point and not check_ident_start(data, point["offset"]):
            raise HarnessError("points_mismatch", f"point {point['id']}: offset is not an identifier start")


def load_points(data, corpus_id: str) -> tuple[dict | None, str]:
    path = data.points(corpus_id)
    if not os.path.isfile(path):
        return None, "-"
    return read_json(path), sha256_file(path)
