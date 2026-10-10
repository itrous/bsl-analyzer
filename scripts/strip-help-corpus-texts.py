#!/usr/bin/env python3
"""Derive the built-in interface-facts corpus from a full platform help corpus.

The full corpus (the `html-parser` output, `platform_data.json`) carries 1C's
descriptive texts: descriptions, parameter documentation, examples, notes and
"see also" lists. Those are 1C's copyrighted expression and never enter the
repository. This script keeps only the structured interface facts — names,
parameters, types, versions, execution contexts and the signature line — and
writes them as `crates/bsl-platform/data/platform_facts.json`, the corpus the
analyzer serves when no richer source is available.

Usage:

  scripts/strip-help-corpus-texts.py \
    --corpus "$HOME/.cache/bsl-analyzer/platform-help-corpus/platform_data.json" \
    --output crates/bsl-platform/data/platform_facts.json

The source is checked structurally before anything is removed: every key of
every record must be either a fact to keep or one of the text fields named
below, so a new field in the extractor output — text or fact — fails the run
instead of being silently dropped or slipping into the tree. The output is
checked again against the facts-only allow-list, and the Rust test
`crates/bsl-platform/tests/bundled_facts.rs` repeats that check on the
committed file with its own, independent allow-list.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

# Sections of the corpus and the keys a record may keep. The `keywords`
# section is dropped entirely: it holds only language documentation texts.
RECORD_KEYS = {
    "types": {"name", "english_name", "min_version", "context", "iter_element_types", "xdto_name"},
    "methods": {
        "id",
        "type_name",
        "name",
        "english_name",
        "return_type",
        "parameters",
        "variants",
        "min_version",
        "context",
        "documentation",
    },
    "global_functions": {
        "id",
        "name",
        "english_name",
        "return_type",
        "parameters",
        "variants",
        "min_version",
        "context",
        "documentation",
    },
    "constructors": {
        "id",
        "type_name",
        "variant_name",
        "parameters",
        "min_version",
        "context",
        "documentation",
    },
    "properties": {
        "id",
        "type_name",
        "name",
        "english_name",
        "property_types",
        "is_readonly",
        "min_version",
        "context",
        "documentation",
    },
}
# The signature line is the only documentation field that is a fact rather
# than prose: `method_display_names` reads the Russian name of template manager
# methods (`СправочникМенеджер.<Имя>`) from it.
DOCUMENTATION_KEYS = {"syntax"}
# The texts the script exists to remove. A documentation key outside both sets
# is unknown and stops the run.
TEXT_KEYS = {"description", "param_descriptions", "examples", "notes", "see_also"}
# The one section removed whole: it holds only language documentation texts.
TEXT_SECTIONS = {"keywords"}
PARAMETER_KEYS = {"name", "param_type", "is_optional", "is_variadic"}
VARIANT_KEYS = {"variant_name", "parameters"}
CONTEXT_KEYS = {
    "thick_client",
    "thin_client",
    "web_client",
    "server",
    "mobile_client",
    "external_connection",
}


def strip_record(record: dict, allowed: set[str]) -> dict:
    out = {key: value for key, value in record.items() if key in allowed}
    documentation = record.get("documentation")
    if isinstance(documentation, dict) and documentation.get("syntax") is not None:
        out["documentation"] = {"syntax": documentation["syntax"]}
    else:
        out.pop("documentation", None)
    return out


def strip(corpus: dict) -> dict:
    out: dict[str, list] = {}
    for section, allowed in RECORD_KEYS.items():
        out[section] = [strip_record(record, allowed) for record in corpus.get(section, [])]
    return out


def check_keys(where: str, value: object, allowed: set[str], errors: list[str]) -> None:
    if not isinstance(value, dict):
        errors.append(f"{where} must be an object")
        return
    for key in value:
        if key not in allowed:
            errors.append(f"{where}.{key} is not an interface fact")


def check(corpus: dict, documentation_keys: set[str], sections: set[str]) -> list[str]:
    """Problems with `corpus`: a key anywhere outside the given allow-lists.

    With the facts-only lists it checks the output; with the text keys and the
    text sections added it checks the source, where every text field is known
    by name and anything else is a change of the extractor's format.
    """
    errors: list[str] = []
    for section in corpus:
        if section not in sections:
            errors.append(f"unexpected section {section}")
    for section, allowed in RECORD_KEYS.items():
        for index, record in enumerate(corpus.get(section, [])):
            where = f"{section}[{index}]"
            check_keys(where, record, allowed, errors)
            if not isinstance(record, dict):
                continue
            documentation = record.get("documentation")
            if documentation is not None:
                check_keys(f"{where}.documentation", documentation, documentation_keys, errors)
                if isinstance(documentation, dict) and "syntax" in documentation:
                    if not isinstance(documentation["syntax"], str):
                        errors.append(f"{where}.documentation.syntax must be a string")
            if "context" in record and record["context"] is not None:
                check_keys(f"{where}.context", record["context"], CONTEXT_KEYS, errors)
            for parameter_index, parameter in enumerate(record.get("parameters") or []):
                check_keys(f"{where}.parameters[{parameter_index}]", parameter, PARAMETER_KEYS, errors)
            for variant_index, variant in enumerate(record.get("variants") or []):
                variant_where = f"{where}.variants[{variant_index}]"
                check_keys(variant_where, variant, VARIANT_KEYS, errors)
                if isinstance(variant, dict):
                    for parameter_index, parameter in enumerate(variant.get("parameters") or []):
                        check_keys(
                            f"{variant_where}.parameters[{parameter_index}]",
                            parameter,
                            PARAMETER_KEYS,
                            errors,
                        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--corpus", required=True, type=Path, help="full help corpus JSON (html-parser output)")
    parser.add_argument("--output", required=True, type=Path, help="where to write the interface-facts corpus")
    args = parser.parse_args()

    raw = args.corpus.read_bytes()
    corpus = json.loads(raw)
    if not isinstance(corpus, dict):
        print("the corpus root must be an object", file=sys.stderr)
        return 1

    source_errors = check(corpus, DOCUMENTATION_KEYS | TEXT_KEYS, set(RECORD_KEYS) | TEXT_SECTIONS)
    facts = strip(corpus)
    errors = source_errors or check(facts, DOCUMENTATION_KEYS, set(RECORD_KEYS))
    if errors:
        for error in errors[:20]:
            print(error, file=sys.stderr)
        print(f"{len(errors)} problem(s); nothing written", file=sys.stderr)
        return 1

    text = json.dumps(facts, ensure_ascii=False, indent=1) + "\n"
    args.output.write_bytes(text.encode("utf-8"))

    print(f"source  {args.corpus} ({len(raw)} bytes, SHA-256 {hashlib.sha256(raw).hexdigest()})")
    print(f"written {args.output} ({len(text.encode('utf-8'))} bytes)")
    for section in RECORD_KEYS:
        print(f"  {section}: {len(facts[section])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
