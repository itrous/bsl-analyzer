#!/usr/bin/env python3
"""Check the licensing registry, Cargo metadata and provenance disclosures.

Run from any directory with Python 3.11+: python3 scripts/test-tier-registry.py.
Historical checks require the repository's full Git history; no CI policy is added.
"""

from collections import Counter
import json
from pathlib import Path
import re
import subprocess
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
BASE = "51ac6d89545902345142cc748b8c820ff3f3f314"
AUDIT_BASE = "046ee5d38b74"
PURPOSES = {
    "bsl-config": ("configurations", "ConfigId"),
    "bsl-types": ("Type kernel",),
    "code-chunk": ("Splitting sources", "fragments"),
    "ide-host-core": ("Shared analysis host",),
    "parser-error": ("Parse error types",),
    "vcs": ("Git diff", "analysis scoping"),
}
ADDITIONS = set(PURPOSES)
MOVED_TO_A = {"sdbl-hir", "parser", "lexer"}
VERIFIED = "d50f2fd8"
DIFF_BASE = "c6d140ee"
RECOVERY_301 = "crates/parser/src/grammar/sdbl/expressions.rs"
GUARDED_PREFIXES = ("crates/parser/", "crates/lexer/")
MOVED_MANIFESTS = ("crates/parser/Cargo.toml", "crates/lexer/Cargo.toml")
HANDLERS = "crates/ide-diagnostics/src/handlers/"
# Handler files the attestation touched, by exact path: test material replaced (None) or one
# user-facing message rewritten by the owner's decision (the base line and its replacement).
# Everything before the first `#[cfg(test)]` must otherwise equal DIFF_BASE byte for byte.
HANDLER_EDITS = {
    HANDLERS + "assign_alias_fields_in_query.rs": None,
    HANDLERS + "using_like_in_query.rs": None,
    HANDLERS + "full_outer_join_query.rs": None,
    HANDLERS + "join_with_sub_query.rs": None,
    HANDLERS + "join_with_virtual_table.rs": None,
    HANDLERS + "multiline_string_in_query.rs": None,
    HANDLERS + "query_nested_fields_by_dot.rs": None,
    HANDLERS + "query_parse_error.rs": None,
    HANDLERS + "select_top_without_order_by.rs": None,
    HANDLERS + "virtual_table_call_without_parameters.rs": None,
    HANDLERS + "logical_or_in_join_query_section.rs": (
        """            "Обнаружен оператор 'ИЛИ' в условии соединения",\n""",
        """            "ИЛИ в условии соединения мешает СУБД использовать индекс, если не сводится к В; разбивать запрос на части через ОБЪЕДИНИТЬ ВСЕ можно, только если результат не изменится",\n""",
    ),
    HANDLERS + "query_to_missing_metadata.rs": (
        """                "Исправьте обращение к несуществующему метаданному \\"{}\\" в запросе",\n""",
        """                "Источник запроса \\"{}\\" не разрешается в таблицу метаданных конфигурации",\n""",
    ),
}
DIFF_ALLOWLIST = (
    "crates/sdbl-hir/**",
    RECOVERY_301,
    "crates/*/tests/**",
    "docs/legal/**",
    "docs/plans/**",
    "LICENSING.md",
    "NOTICE",
    "scripts/test-tier-registry.py",
    # The contribution terms and the README licence section, by the owner's decision on
    # the contributions finding: they stated LGPL only, against the published policy.
    # Only the named section may differ from DIFF_BASE, see SECTION_EDITS.
    "CONTRIBUTING.md",
    "README.md",
    "Cargo.toml",
    "crates/*/Cargo.toml",
    *HANDLER_EDITS,
)


def path_matches(path, pattern):
    """Segment-exact match: `*` is exactly one segment, a trailing `**` is one or more."""
    parts, pats = path.split("/"), pattern.split("/")
    if pats[-1] == "**":
        pats = pats[:-1]
        if len(parts) <= len(pats):
            return False
        parts = parts[: len(pats)]
    if len(parts) != len(pats):
        return False
    return all(pat == "*" or pat == part for pat, part in zip(pats, parts))


def name_status_paths(output):
    """Every path named by `git diff --name-status`, both sides of a rename or copy."""
    paths = []
    for line in output.splitlines():
        fields = line.split("\t")
        paths.extend(fields[1:])
    return paths


SECTION_EDITS = {"README.md": "## Лицензия", "CONTRIBUTING.md": "## Вопросы и лицензия"}


def without_section(text, heading):
    """The text with one level-2 section — heading to the next level-2 heading — cut out."""
    lines = text.split("\n")
    start = lines.index(heading)
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    return "\n".join(lines[:start] + lines[end:])


def product_prefix(text):
    """The part of a handler file before its test module."""
    return text.split("#[cfg(test)]", 1)[0]


def handler_edit_violations(read_base, read_current, edits=None):
    """Handler files whose product part differs from the base beyond their one named edit."""
    bad = []
    for path, edit in (HANDLER_EDITS if edits is None else edits).items():
        before, after = product_prefix(read_base(path)), product_prefix(read_current(path))
        if edit is not None:
            old, new = edit
            if before.count(old) != 1:
                bad.append(f"{path}: the base line of the edit is not unique")
                continue
            before = before.replace(old, new)
        if before != after:
            bad.append(path)
    return bad


def diff_violations(changed):
    """Paths outside the allowlist; the guarded crates admit only the #301 recovery file and the moved manifests."""
    bad = []
    for path in changed:
        if path.startswith(GUARDED_PREFIXES):
            if path != RECOVERY_301 and path not in MOVED_MANIFESTS:
                bad.append(path)
        elif not any(path_matches(path, pattern) for pattern in DIFF_ALLOWLIST):
            bad.append(path)
    return bad
AUDIT_PATH = "docs/legal/tier-a-provenance-audit.md"
STANDARD = "crates/hir-def/src/module_structure/standard.rs"
OLD_STANDARD = "crates/ide-diagnostics/src/utils/standard_regions.rs"
ATTRIBUTION = {
    "syntax": ("architecture",),
    "base-db": ("concurrent map", "Salsa"),
    "stdx": ("quality of service", "worker pool"),
    "paths": ("absolute", "relative", "wrapper"),
    "cfg": ("HIR", "separate notice"),
    "dataflow": ("MIR dataflow", "type-inference lattice"),
    "hir-def": ("documentation", "DefWithBodyId", "path resolution"),
    "hir-ty": ("type inference", "diagnostics"),
    "hir": ("representation of definitions",),
    "ide-db": ("LineIndex", "Salsa"),
}


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True)


def historical(path, revision=BASE):
    return command("git", "show", f"{revision}:{path}")


def section(text, start, end=None):
    result = text.split(start, 1)[1]
    return result.split(end, 1)[0] if end else result


def rows(text, tier):
    end = "## Tier B" if tier == "A" else "## Clean-room"
    block = section(text, f"## Tier {tier}", end)
    return [line.split("|")[1:-1] for line in block.splitlines() if line.startswith("| `")]


def names(text, tier):
    return [name for row in rows(text, tier) for name in re.findall(r"`([^`]+)`", row[0])]


def dependency_marks(text):
    result = {}
    for row in rows(text, "A"):
        members = re.findall(r"`([^`]+)`", row[0])
        cell = row[2].strip()
        if cell.startswith("all three:"):
            result.update({name: set(re.findall(r"`([^`]+)`", cell)) for name in members})
        elif ":" in cell:
            for clause in cell.split(";"):
                owners, targets = clause.split(":", 1)
                targets = set(re.findall(r"`([^`]+)`", targets))
                if "also" in clause:
                    targets |= next(iter(result[name] for name in members if name in result))
                for name in re.findall(r"`([^`]+)`", owners):
                    result[name] = targets
        else:
            result.update({name: set(re.findall(r"`([^`]+)`", cell)) for name in members})
        assert set(members) <= result.keys(), f"unparsed dependency marks: {row}"
    return result


def closure(graph, name):
    reached = set()
    pending = list(graph[name])
    while pending:
        current = pending.pop()
        if current not in reached:
            reached.add(current)
            pending.extend(graph[current])
    return reached


def qualified_permission(text):
    intro = " ".join(section(text, "## Tier A", "| Crate |").split())
    return all(term in intro for term in (
        "crate's own sources, in isolation", "permission does not extend to a build",
        "depends on the tiers", "dependency tree", "cfg", "bsl-metadata", "under review",
    ))


def notice_entries(text):
    heritage = section(text, "Architecture heritage", "Language heritage")
    entries = re.findall(r"^  \* crates/([\w-]+) — (.*?)(?=^  \*|\n\n)", heritage, re.M | re.S)
    return {name: " ".join(body.split()) for name, body in entries}


def disclosure_complete(text):
    try:
        one = section(text, "## 1.", "## 2.")
        three = section(text, "## 3.", "## 4.")
        four = section(text, "## 4.", "## 5.")
        five = section(text, "## 5.", "## 6.")
        six = section(text, "## 6.", "## 7.")
        transfer = section(text, "### 8.3.", "### 8.4.")
    except IndexError:
        return False
    requirements = (
        (one, (AUDIT_BASE, "31", "нынешняя таблица")),
        (three, ("разобран в #154", "основание Tier A", "repository.workspace = true")),
        (four, ("Не закрывает `cfg` и `bsl-metadata`", "§ 8.3")),
        (five, ("15 файлов", "Architecture heritage", "в исходники", "не возвращались")),
        (six, ("Шесть крейтов", "не покрыты", "не сборку")),
        (transfer, (
            STANDARD.removeprefix("crates/"), OLD_STANDARD.removeprefix("crates/"),
            "05dc4f98", "65f1278a", "843b00ab", "effab845", "R084", "Regions.java",
            "Keywords.java", "#455", "https://its.1c.ru/db/v8std#content:455",
            "https://v8std.ru/std/455/", "§ 1.4", "§ 1.5", "§ 1.6", "§ 1.7",
            "Расхождения со стандартом", "регистронезависимость", "пустой суффикс",
            "Независимого clean-room-переписывания словаря не было",
            "словарь состоит из имён", "обвязка сопоставления и тесты при файле местные",
            "Основанием не служат", "--diff-filter=R -M", "cb6e7ac1", "**D** + **A**",
            "bsl-clean-room-slice-b2.md", "копирования, разделения и пересказы без R",
            "Тир `cfg` и `bsl-metadata` этим не",
        )),
    )
    return all(term in block for block, terms in requirements for term in terms)


class TierRegistryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.licensing = (ROOT / "LICENSING.md").read_text()
        cls.audit = (ROOT / AUDIT_PATH).read_text()
        cls.notice = (ROOT / "NOTICE").read_text()
        cls.baseline = historical("LICENSING.md")
        metadata = json.loads(command("cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"))
        cls.packages = {p["name"]: p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
        cls.graph = {
            name: {d["name"] for d in p["dependencies"] if d["kind"] in (None, "build") and d["name"] in cls.packages}
            for name, p in cls.packages.items()
        }

    def test_I1_permission_is_for_sources_not_the_build(self):
        self.assertTrue(qualified_permission(self.licensing))
        self.assertFalse(qualified_permission(self.baseline))
        self.assertIn("parser", self.graph["base-db"])
        cfg_users = {n for n, p in self.packages.items() if any(d["name"] == "cfg" and d["kind"] is None for d in p["dependencies"])}
        self.assertEqual(cfg_users, {"dataflow", "hir", "hir-ty"})
        metadata_users = sorted(n for n, p in self.packages.items() if any(d["name"] == "bsl-metadata" and d["kind"] is None for d in p["dependencies"]))
        self.assertEqual(len(metadata_users), 14)
        self.assertIn("fourteen crates depend on", self.licensing)
        print("E1 bsl-metadata direct normal users:", metadata_users)

    def test_I1_dependency_column_matches_normal_build_closure(self):
        marks = dependency_marks(self.licensing)
        tier_b = set(names(self.licensing, "B"))
        for name in names(self.licensing, "A"):
            with self.subTest(crate=name):
                self.assertEqual(marks[name], closure(self.graph, name) & tier_b)
        self.assertEqual(marks["syntax"], set())
        self.assertEqual(marks["ide"], {"ide-diagnostics"})
        self.assertNotIn("parser", closure(self.graph, "syntax"), "syntax's dev dependency must not enter the closure")
        print("E1 Tier B dependency column checked for", len(marks), "crates")

    def test_I2_six_rows_license_inheritance_and_existing_tiers(self):
        a, b = names(self.licensing, "A"), names(self.licensing, "B")
        self.assertEqual(Counter(a), Counter(names(self.baseline, "A")) + Counter(ADDITIONS) + Counter(MOVED_TO_A))
        self.assertEqual(Counter(b), Counter(names(self.baseline, "B")) - Counter(MOVED_TO_A))
        purposes = {row[0].strip().strip("`"): row[1] for row in rows(self.licensing, "A")}
        for name in ADDITIONS:
            with self.subTest(crate=name):
                for term in PURPOSES[name]:
                    self.assertIn(term, purposes[name])
                self.assertEqual(a.count(name), 1)
                self.assertNotIn(name, b)
                manifest = tomllib.loads(Path(self.packages[name]["manifest_path"]).read_text())
                self.assertIs(manifest["package"]["license"]["workspace"], True)
                self.assertEqual(self.packages[name]["license"], "MIT OR Apache-2.0")
                self.assertNotIn(name, names(self.baseline, "A"))
        for name in MOVED_TO_A:
            with self.subTest(moved=name):
                self.assertEqual(a.count(name), 1)
                self.assertNotIn(name, b)
                self.assertIn(name, names(self.baseline, "B"))
                manifest = tomllib.loads(Path(self.packages[name]["manifest_path"]).read_text())
                self.assertIs(manifest["package"]["license"]["workspace"], True)
                self.assertEqual(self.packages[name]["license"], "MIT OR Apache-2.0")
        print("E1 packages/A/B/missing:", len(self.packages), len(a), len(b), sorted(set(self.packages) - set(a + b)))

    def test_I3_I5_audit_sections_and_historical_boundary(self):
        self.assertTrue(disclosure_complete(self.audit))
        self.assertFalse(disclosure_complete(historical(AUDIT_PATH)))
        for term in (
            AUDIT_BASE, "разобран в #154", "Не закрывает `cfg` и `bsl-metadata`",
            "Architecture heritage", "не покрыты", "словарь состоит из имён",
            "--diff-filter=R -M", "копирования, разделения и пересказы без R",
        ):
            with self.subTest(omitted_disclosure=term):
                self.assertFalse(disclosure_complete(self.audit.replace(term, "")))
        ledger = section(self.audit, "Ведомость удалённых строк", "## 6.")
        counts = {}
        files = set()
        for line in ledger.splitlines():
            if line.startswith("| `"):
                cells = line.split("|")
                crate = cells[1].strip().strip("`")
                self.assertNotIn(crate, counts)
                counts[crate] = int(cells[3])
                files.update(f"crates/{crate}/{p}" for p in re.findall(r"`([^`]+)`", cells[2]))
        boundary = names(historical("LICENSING.md", AUDIT_BASE), "A")
        patch = command("git", "show", "843b00ab", "--", *[f"crates/{n}" for n in boundary])
        hits = []
        path = ""
        for line in patch.splitlines():
            if line.startswith("diff --git"):
                path = line.split()[2][2:]
            if line.startswith("-") and not line.startswith("---") and "rust-analyzer" in line:
                hits.append((path, line))
        self.assertEqual({p for p, _ in hits}, files)
        self.assertEqual(Counter(p.split("/")[1] for p, _ in hits), counts)
        self.assertEqual(set(counts), set(ATTRIBUTION))
        self.assertEqual((len(hits), len(counts), len({p for p, _ in hits})), (17, 10, 15))
        self.assertIn(("crates/syntax/src/lib.rs", "-//! Based on rust-analyzer's syntax crate architecture."), hits)
        print("E3 boundary:", len(boundary), "crates; calibration: 17 lines / 10 crates / 15 files")
        for path, line in hits:
            print("E3", path, line)

    def test_I3_rename_search_both_directions_and_nonrename_limit(self):
        a, b = set(names(self.licensing, "A")), set(names(self.licensing, "B"))
        log = command("git", "log", "HEAD", "--format=COMMIT %H", "--name-status", "--diff-filter=R", "-M", "--", "crates", "xtask")
        found = []
        commit = ""
        for line in log.splitlines():
            if line.startswith("COMMIT "):
                commit = line.split()[1]
            if line.startswith("R"):
                score, old, new = line.split("\t")
                x, y = (p.split("/")[1] if p.startswith("crates/") else "xtask" for p in (old, new))
                if (x in a and y in b) or (x in b and y in a):
                    found.append((commit, score, old, new, "A→B" if x in a else "B→A"))
        transfer_commit = command("git", "rev-parse", "effab845^{commit}").strip()
        self.assertIn((transfer_commit, "R084", OLD_STANDARD, STANDARD, "B→A"), found)
        count_a = sum(r[-1] == "A→B" for r in found)
        count_b = sum(r[-1] == "B→A" for r in found)
        self.assertIn(f"**A→B {count_a}, B→A {count_b}**", self.audit)
        limit = command("git", "show", "--format=", "--name-status", "-M", "cb6e7ac1", "--", "crates")
        self.assertIn("D\tcrates/ide-diagnostics/src/utils/preprocessor_symbols.rs", limit)
        self.assertIn("A\tcrates/syntax/src/preproc_symbols.rs", limit)
        print("E2", *found, sep="\n")
        print("E2 A→B / B→A:", count_a, count_b, "; D/A positive limit confirmed")

    def test_I4_hir_ty_repository_inherits_our_workspace(self):
        manifest = tomllib.loads((ROOT / "crates/hir-ty/Cargo.toml").read_text())
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]
        self.assertEqual(manifest["package"]["repository"], {"workspace": True})
        self.assertEqual(self.packages["hir-ty"]["repository"], workspace["repository"])
        self.assertEqual(workspace["repository"], "https://github.com/itrous/bsl-analyzer")
        self.assertNotEqual(tomllib.loads(historical("crates/hir-ty/Cargo.toml"))["package"]["repository"], workspace["repository"])
        print("E1 hir-ty repository:", self.packages["hir-ty"]["repository"])

    def test_I5_notice_has_each_specific_attribution(self):
        entries = notice_entries(self.notice)
        self.assertEqual(set(entries), set(ATTRIBUTION))
        architecture = section(self.notice, "Architecture heritage", "Language heritage")
        self.assertEqual(len(re.findall(r"^  \* crates/", architecture, re.M)), len(ATTRIBUTION))
        self.assertEqual(notice_entries(historical("NOTICE")), {})
        for name, terms in ATTRIBUTION.items():
            with self.subTest(crate=name):
                for term in terms:
                    self.assertIn(term, entries[name])
        architecture = section(self.notice, "Architecture heritage", "Language heritage")
        self.assertIn("https://github.com/rust-analyzer/rust-analyzer", architecture)
        self.assertIn("MIT OR Apache-2.0", architecture)
        self.assertIn("no more of the crate", architecture)
        self.assertIn(AUDIT_PATH, architecture)
        print("I5 NOTICE:", sorted(entries))

    def test_I6a_historical_154_only_repository_changed_in_product_and_ci_files(self):
        paths = ["crates", "xtask", "Cargo.toml", "Cargo.lock", ".github", ".gitlab-ci.yml"]
        changed = command("git", "diff", "--name-only", BASE, VERIFIED, "--", *paths).splitlines()
        untracked = command("git", "ls-files", "--others", "--exclude-standard", "--", *paths).splitlines()
        self.assertEqual(untracked, [], "untracked product/CI files must not escape the comparison")
        self.assertEqual(changed, ["crates/hir-ty/Cargo.toml"])
        before = historical("crates/hir-ty/Cargo.toml")
        after = (ROOT / "crates/hir-ty/Cargo.toml").read_text()
        self.assertEqual(after, before.replace('repository = "https://github.com/1c-syntax/bsl-analyzer"', "repository.workspace = true"))
        self.assertEqual((ROOT / STANDARD).read_text(), historical(STANDARD))
        print("I6 product/CI diff:", changed, "; only repository; standard.rs byte-identical")

    def test_I6_current_diff_against_develop_stays_inside_allowlist(self):
        command("git", "merge-base", "--is-ancestor", DIFF_BASE, "origin/develop")
        command("git", "merge-base", "--is-ancestor", DIFF_BASE, "HEAD")
        status = command("git", "diff", "--name-status", "-M", "-C", DIFF_BASE, "--")
        changed = name_status_paths(status)
        changed += command("git", "ls-files", "--others", "--exclude-standard").splitlines()
        self.assertEqual(diff_violations(sorted(set(changed))), [], "changes outside the allowlist")
        print("I6 diff vs", DIFF_BASE, ":", len(set(changed)), "paths, all allowlisted")

    def test_I6_handler_product_bytes_equal_the_base_but_for_named_messages(self):
        base = lambda path: historical(path, DIFF_BASE)
        current = lambda path: (ROOT / path).read_text()
        self.assertEqual(handler_edit_violations(base, current), [])
        for path, edit in HANDLER_EDITS.items():
            with self.subTest(edited=path):
                changed = product_prefix(current(path)) != product_prefix(base(path))
                self.assertEqual(changed, edit is not None, "a listed message edit must be present, a test-only file must keep its product part")
        print("I6 handler edits:", len(HANDLER_EDITS), "files; product parts equal the base but for",
              sum(edit is not None for edit in HANDLER_EDITS.values()), "named messages")

    def test_I6_handler_comparison_detects_a_changed_byte(self):
        path = HANDLERS + "using_like_in_query.rs"
        base = "fn a() {}\n#[cfg(test)]\nmod tests {}\n"
        same_product = "fn a() {}\n#[cfg(test)]\nmod tests { new }\n"
        changed_product = "fn b() {}\n#[cfg(test)]\nmod tests {}\n"
        edits = {path: None}
        self.assertEqual(handler_edit_violations(lambda _: base, lambda _: same_product, edits), [])
        self.assertEqual(handler_edit_violations(lambda _: base, lambda _: changed_product, edits), [path])
        message = {path: ('"old",\n', '"new",\n')}
        old, new = 'x("old",\n);\n', 'x("new",\n);\n'
        self.assertEqual(handler_edit_violations(lambda _: old, lambda _: new, message), [])
        self.assertEqual(handler_edit_violations(lambda _: old, lambda _: 'x("other",\n);\n', message), [path])
        self.assertEqual(handler_edit_violations(lambda _: new, lambda _: new, message), [f"{path}: the base line of the edit is not unique"])

    def test_I6_docs_change_only_their_licence_sections(self):
        for path, heading in SECTION_EDITS.items():
            with self.subTest(document=path):
                before, after = historical(path, DIFF_BASE), (ROOT / path).read_text()
                self.assertEqual(without_section(after, heading), without_section(before, heading),
                                 f"{path} differs from the base outside {heading!r}")
        sample = "# T\n\n## A\n\na\n\n## Лицензия\n\nold\n\n## B\n\nb\n"
        self.assertEqual(without_section(sample.replace("old", "new"), "## Лицензия"),
                         without_section(sample, "## Лицензия"))
        self.assertNotEqual(without_section(sample.replace("b\n", "c\n"), "## Лицензия"),
                            without_section(sample, "## Лицензия"))

    def test_I6_rename_names_both_paths(self):
        paths = name_status_paths("R100\tcrates/lexer/src/lib.rs\tcrates/sdbl-hir/src/x.rs\nM\tNOTICE\nC075\tcrates/parser/src/a.rs\tdocs/legal/a.md")
        self.assertEqual(paths, [
            "crates/lexer/src/lib.rs", "crates/sdbl-hir/src/x.rs", "NOTICE",
            "crates/parser/src/a.rs", "docs/legal/a.md",
        ])
        self.assertEqual(diff_violations(paths), ["crates/lexer/src/lib.rs", "crates/parser/src/a.rs"])

    def test_I6_allowlist_rejects_and_accepts_known_cases(self):
        self.assertEqual(diff_violations([
            "crates/sdbl-hir/src/lib.rs", "crates/sdbl-hir/Cargo.toml", "crates/ide/tests/x.rs",
            "docs/legal/a.md", "docs/plans/p.md", "LICENSING.md", "NOTICE", "Cargo.toml",
            "crates/hir-ty/Cargo.toml", RECOVERY_301, "crates/ide/tests/a/b/c.rs", "crates/sdbl-hir/tests/f.rs",
            "crates/parser/Cargo.toml", "crates/lexer/Cargo.toml",
            "crates/ide-diagnostics/Cargo.toml", "crates/ide-diagnostics/tests/handler_attestation.rs",
            "CONTRIBUTING.md", "README.md",
            HANDLERS + "using_like_in_query.rs",
        ]), [])
        rejected = [
            "crates/lexer/src/lib.rs", "crates/ide-diagnostics/src/lib.rs", "crates/parser/src/lib.rs",
            HANDLERS + "union_all.rs", "crates/ide-diagnostics/src/handlers.rs", HANDLERS + "fixtures/x.bsl",
            "crates/ide-diagnostics/src/handlers/using_like_in_query.rs/x",
            "crates/parser/tests/t.rs", "crates/parser/src/Cargo.toml", "crates/hir-ty/src/lib.rs",
            ".gitlab-ci.yml", "Cargo.lock", "docs/other.md",
            "crates/hir-ty/src/Cargo.toml", "crates/hir-ty/Cargo.toml/x", "crates/a/b/Cargo.toml",
            "crates/lexer/tests/a/b.rs", "crates/parser/tests/a/b/c.rs", "crates/ide/tests", "crates/ide/src/tests/x.rs",
            "docs/legal", "docs/legalx/a.md", "crates/sdbl-hir", "crates/sdbl-hirx/a.rs", "Cargo.toml/x",
            "docs/README.md", "README.md/x",
        ]
        self.assertEqual(diff_violations(rejected), rejected)

    def test_I7_current_manifests_change_only_allowlisted_license_and_repository_fields(self):
        default_license, default_repository = "MIT OR Apache-2.0", "https://github.com/itrous/bsl-analyzer"
        allowed = {
            ("crates/hir-ty/Cargo.toml", "package.repository"): ("https://github.com/1c-syntax/bsl-analyzer", {"workspace": True}),
            ("crates/sdbl-hir/Cargo.toml", "package.license"): ("LGPL-3.0-or-later", {"workspace": True}),
            ("crates/parser/Cargo.toml", "package.license"): ("LGPL-3.0-or-later", {"workspace": True}),
            ("crates/lexer/Cargo.toml", "package.license"): ("LGPL-3.0-or-later", {"workspace": True}),
        }

        def fields(text):
            if text is None:
                return None
            data = tomllib.loads(text)
            result = {}
            for prefix, node in (("", data), ("workspace.", data.get("workspace", {}))):
                for section_name in ("dependencies", "dev-dependencies", "build-dependencies"):
                    if section_name in node:
                        result[f"{prefix}{section_name}"] = node[section_name]
            for target, node in data.get("target", {}).items():
                for section_name in ("dependencies", "dev-dependencies", "build-dependencies"):
                    if section_name in node:
                        result[f"target.{target}.{section_name}"] = node[section_name]
            for table in ("package", "workspace.package"):
                node = data
                for part in table.split("."):
                    node = node.get(part, {})
                for field in ("license", "repository"):
                    if field in node:
                        result[f"{table}.{field}"] = node[field]
            return result

        def at_base(path):
            try:
                return historical(path)
            except subprocess.CalledProcessError:
                return None

        current = set(command("git", "ls-files", "--", "Cargo.toml", "*/Cargo.toml").split())
        current |= set(command("git", "ls-files", "--others", "--exclude-standard", "--", "Cargo.toml", "*/Cargo.toml").split())
        base = set(command("git", "ls-tree", "-r", "--name-only", BASE).split())
        base = {path for path in base if path == "Cargo.toml" or path.endswith("/Cargo.toml")}
        seen = set()
        for path in sorted(current | base):
            before = fields(at_base(path))
            after = fields((ROOT / path).read_text()) if path in current else None
            with self.subTest(manifest=path):
                if after is None:
                    self.fail("manifest present at base was removed")
                if before is None:
                    for field, value in after.items():
                        if not field.endswith(("license", "repository")):
                            self.fail(f"new manifest {path} declares {field}: its tier cannot be verified")
                        self.assertIn(
                            value,
                            ({"workspace": True}, default_license if field.endswith("license") else default_repository),
                            f"new manifest {path}: {field}",
                        )
                    self.assertTrue(
                        after.get("package.license") == {"workspace": True}
                        or after.get("package.license") == default_license,
                        f"new manifest {path} declares no default-tier license",
                    )
                    continue
                for field in sorted(set(before) | set(after)):
                    if before.get(field) == after.get(field):
                        continue
                    key = (path, field)
                    self.assertIn(key, allowed, f"unlisted change of {field} (dependencies must equal the base)")
                    self.assertEqual((before.get(field), after.get(field)), allowed[key])
                    seen.add(key)
        self.assertEqual(seen, set(allowed), "an allowlisted change is not present")
        for moved in ("parser", "lexer"):
            path = f"crates/{moved}/Cargo.toml"
            before, after = tomllib.loads(historical(path)), tomllib.loads((ROOT / path).read_text())
            self.assertEqual(before["package"].pop("license"), "LGPL-3.0-or-later")
            self.assertEqual(after["package"].pop("license"), {"workspace": True})
            self.assertEqual(after, before, f"{path} may differ from the base only in package.license")
        for moved in ("sdbl-hir", "parser", "lexer"):
            self.assertEqual(fields((ROOT / f"crates/{moved}/Cargo.toml").read_text())["package.license"], {"workspace": True})
        self.assertEqual(fields((ROOT / "Cargo.toml").read_text())["workspace.package.license"], default_license)
        print("I7 manifest license/repository changes:", sorted(seen))


if __name__ == "__main__":
    unittest.main(verbosity=2)
