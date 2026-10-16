#!/usr/bin/env python3
"""Render and parse synthetic native-check XML templates without 1C."""

from __future__ import annotations

import re
import uuid
from pathlib import Path
from xml.etree import ElementTree


ROOT = Path(__file__).resolve().parent
TOKEN = re.compile(r"\{\{([A-Z][A-Z0-9_]*)\}\}")
NAME_TOKENS = {"MODULE_NAME", "CATALOG_NAME", "FORM_NAME"}
UUID_TOKENS = {
    "MODULE_UUID",
    "CATALOG_UUID",
    "FORM_UUID",
    "TYPE_ID_OBJECT",
    "VALUE_ID_OBJECT",
    "TYPE_ID_REF",
    "VALUE_ID_REF",
    "TYPE_ID_SELECTION",
    "VALUE_ID_SELECTION",
    "TYPE_ID_LIST",
    "VALUE_ID_LIST",
    "TYPE_ID_MANAGER",
    "VALUE_ID_MANAGER",
}
NAME_PATTERN = re.compile(r"^[A-Za-z][A-Za-z0-9_]{0,63}$")


def render_all() -> dict[str, ElementTree.Element]:
    names = {token: f"NativeCheck{index}" for index, token in enumerate(sorted(NAME_TOKENS), 1)}
    if any(not NAME_PATTERN.fullmatch(value) for value in names.values()):
        raise ValueError("sample name is not a safe ASCII metadata name")
    ids = {token: str(uuid.uuid4()) for token in sorted(UUID_TOKENS)}
    if len(set(ids.values())) != len(ids):
        raise ValueError("sample UUIDs must be unique")
    values = names | ids
    result: dict[str, ElementTree.Element] = {}

    for filename in (
        "common_module.xml",
        "catalog.xml",
        "managed_form.xml",
        "ordinary_form.xml",
        "managed_form_body.xml",
    ):
        source = (ROOT / filename).read_text(encoding="utf-8")
        tokens = set(TOKEN.findall(source))
        if not tokens <= values.keys():
            unknown = ", ".join(sorted(tokens - values.keys()))
            raise ValueError(f"{filename}: unknown placeholder(s): {unknown}")
        rendered = TOKEN.sub(lambda match: values[match.group(1)], source)
        if "{{" in rendered or "}}" in rendered:
            raise ValueError(f"{filename}: unresolved placeholder")
        result[filename] = ElementTree.fromstring(rendered)

    return result


if __name__ == "__main__":
    parsed = render_all()
    print(f"rendered and parsed {len(parsed)} XML templates")
