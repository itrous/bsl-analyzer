#!/usr/bin/env python3
"""Oracle for the platform-type attestation programme.

A type may be closed (`type_member_surface_attestations`) only when the bundled
facts hold its WHOLE documented member surface. The facts are derived from one
pinned platform build, and that build's own help archive can omit a member that
both an older and a newer build document -- closing such a type reports a member
that really exists. So the oracle reads several installed builds' archives and
answers per type whether the facts carry a hole.

Inputs are corpora produced by the repository's own extractor, one per build:

    cargo build --release -p html-parser --bin html-parser
    html-parser hbk "C:\\Program Files\\1cv8\\8.3.17.1549\\bin" old.json
    html-parser hbk "C:\\Program Files\\1cv8\\8.5.1.1343\\bin"  new.json

Usage:

    python attestation-oracle.py --facts crates/bsl-platform/data/platform_facts.json \\
        --pinned-version 8.3.27 --corpus old.json --corpus new.json [TYPE ...]

With type names (Russian or English) it prints their surfaces side by side;
without, it lists every type whose facts carry a hole.

A member is REQUIRED in the facts when some corpus documents it with a
`min_version` no newer than the pinned build AND the newest given corpus still
documents it. Anything else is a legitimate absence: a newer platform's addition,
or a member the platform has since removed.

Types are matched by their RUSSIAN name: the platform renames English names
between builds (NotifyDescription -> CallbackDescription), so matching on the
English name invents both missing types and missing members.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys
from pathlib import Path

SURFACES = ("methods", "properties")


def parse_version(text: str | None) -> tuple[int, ...]:
    """1C writes versions as '8.3.16' but also as '8.3.16 (в режиме ...)'."""
    return tuple(int(part) for part in re.findall(r"\d+", text or "0"))


class Corpus:
    def __init__(self, path: Path) -> None:
        data = json.loads(path.read_text(encoding="utf-8"))
        self.path = path
        english_to_russian = {t["english_name"]: t["name"] for t in data["types"]}
        self.types = set(english_to_russian.values())
        self.surface: dict[str, dict[str, dict[str, str | None]]] = {
            s: collections.defaultdict(dict) for s in SURFACES
        }
        for surface in SURFACES:
            for record in data[surface]:
                owner = english_to_russian.get(record["type_name"], record["type_name"])
                self.surface[surface][owner][record["name"]] = record.get("min_version")

    def members(self, surface: str, type_name: str) -> dict[str, str | None]:
        return self.surface[surface].get(type_name, {})


def holes(facts: Corpus, corpora: list[Corpus], pinned: tuple[int, ...]):
    newest = corpora[-1]
    found: dict[str, list[tuple[str, str, str | None]]] = collections.defaultdict(list)
    for surface in SURFACES:
        owners: set[str] = set()
        for corpus in corpora:
            owners |= set(corpus.surface[surface])
        for owner in owners:
            carried = facts.members(surface, owner)
            for corpus in corpora:
                for name, min_version in corpus.members(surface, owner).items():
                    if name in carried:
                        continue
                    if parse_version(min_version) > pinned:
                        continue  # newer than the pinned build
                    if name not in newest.members(surface, owner):
                        continue  # removed by the platform since
                    entry = (surface, name, min_version)
                    if entry not in found[owner]:
                        found[owner].append(entry)
    return found


def resolve(name: str, corpora: list[Corpus], facts: Corpus) -> str:
    """Accept an English name too, by looking it up in any corpus."""
    for corpus in [facts, *corpora]:
        if name in corpus.types:
            return name
    for corpus in [facts, *corpora]:
        data = json.loads(corpus.path.read_text(encoding="utf-8"))
        for entry in data["types"]:
            if entry["english_name"] == name:
                return entry["name"]
    return name


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--facts", required=True, type=Path)
    parser.add_argument("--corpus", required=True, action="append", type=Path,
                        help="a build's corpus; give them oldest first")
    parser.add_argument("--pinned-version", required=True,
                        help="the build the facts were extracted from, e.g. 8.3.27")
    parser.add_argument("types", nargs="*")
    args = parser.parse_args()

    facts = Corpus(args.facts)
    corpora = [Corpus(p) for p in args.corpus]
    pinned = parse_version(args.pinned_version)

    if args.types:
        found = holes(facts, corpora, pinned)
        for raw in args.types:
            name = resolve(raw, corpora, facts)
            print(f"=== {name}")
            for surface in SURFACES:
                in_facts = set(facts.members(surface, name))
                print(f"  {surface:10} facts={len(in_facts):4}", end="")
                for corpus in corpora:
                    theirs = set(corpus.members(surface, name))
                    print(f"  {corpus.path.stem}={len(theirs):4}", end="")
                print()
            verdict = found.get(name)
            if verdict:
                print("  ОТКАЗ: закреплённая сборка не несёт",
                      ", ".join(f"{s}:{n}({v})" for s, n, v in verdict))
            else:
                print("  состав полон, тип можно аттестовать")
        return 1 if any(resolve(r, corpora, facts) in found for r in args.types) else 0

    found = holes(facts, corpora, pinned)
    print(f"types carrying a hole: {len(found)}")
    for name in sorted(found):
        members = ", ".join(f"{s}:{n}({v})" for s, n, v in found[name])
        print(f"  {name}: {members}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
