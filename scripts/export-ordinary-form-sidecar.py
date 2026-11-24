#!/usr/bin/env python3
"""Export an ordinary Form.bin through v8unpack into the analyzer's Form.json.

Python/v8unpack are preparation tools, not server dependencies. The output is
explicitly selected; the source container and configuration are never modified.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile


KINDS = {
    "Field": "input", "CheckBox": "checkbox", "RadioBtn": "radio",
    "SelectField": "choice", "CommandPanel": "commandPanel", "Button": "button",
    "Image": "picture", "Group": "group", "Table": "list", "TableField": "list",
    "Panel": "panel", "Label": "label", "ListField": "listbox", "Separator": "separator",
    "FieldHtml": "html", "Indicator": "indicator", "CalendarBox": "calendar",
    "TextDocumentField": "textDoc", "GraphicalSchemaField": "graphScheme", "Chart": "chart",
}


def from_reader(header: dict, elements: dict, name: str, fingerprint: str, version: str) -> dict:
    """Adapt documented reader fields; never index its undecoded raw payload."""
    if version != "1.2.6":
        raise ValueError(f"unsupported v8unpack version {version}; expected 1.2.6")
    if (str(header.get("obj_version")), header.get("Версия элементов формы")) not in {
        ("9", "0-26"), ("13", "0-27")
    }:
        raise ValueError("unsupported ordinary form representation")
    if not isinstance(elements.get("tree"), list) or not isinstance(elements.get("data"), dict):
        raise ValueError("reader did not return a tree/data representation")
    if not isinstance(elements.get("props"), list):
        raise ValueError("reader did not return properties")
    data = elements["data"]
    pages = data.get("-pages-")
    if pages is not None and (not isinstance(pages, list) or not all(isinstance(p, str) for p in pages)):
        raise ValueError("unsupported page representation")
    controls = []
    used_ids = set()

    def walk(items, parent_path="", output=None):
        if output is None:
            output = controls
        for order, item in enumerate(items):
            if not isinstance(item, dict) or not isinstance(item.get("name"), str) or not isinstance(item.get("type"), str):
                raise ValueError("unsupported tree item")
            page = item.get("page")
            if page is not None and not isinstance(page, str):
                raise ValueError("unsupported item page")
            key = "/".join(part for part in (parent_path, page, item["name"]) if part)
            record = data.get(key)
            if not isinstance(record, dict):
                raise ValueError(f"reader data missing for {key}")
            identifier = record.get("id")
            if isinstance(identifier, bool) or not str(identifier).isdigit() or not 0 <= int(identifier) <= 0xFFFFFFFF:
                raise ValueError(f"invalid dialog id for {key}")
            identifier = int(identifier)
            if identifier in used_ids:
                raise ValueError(f"duplicate dialog id {identifier}")
            used_ids.add(identifier)
            control = {"kind": KINDS.get(item["type"], item["type"]), "name": item["name"],
                       "id": identifier, "page": page, "order": order,
                       "sourceIdentity": key, "sourceKind": item["type"]}
            binding = record.get("prop")
            if binding is not None:
                if not isinstance(binding, str):
                    raise ValueError(f"unsupported binding for {key}")
                control["dataPath"] = binding
                control["dataPathCoverage"] = "reader_property"
            children = item.get("child")
            if children is not None:
                if not isinstance(children, list):
                    raise ValueError(f"unsupported children for {key}")
                control["children"] = []
                walk(children, key, control["children"])
            output.append(control)

    walk(elements["tree"])
    attributes = []
    for item in elements["props"]:
        if not isinstance(item, dict) or not isinstance(item.get("name"), str):
            raise ValueError("unsupported form property")
        attributes.append({"name": item["name"]})
    return {
        "schema": 3,
        "source": {"file": "Form.bin", "sha256": fingerprint},
        "producer": {"name": "v8unpack", "version": version, "formSchema": header["Версия элементов формы"]},
        "form": {"name": name, "pages": pages},
        "attributes": attributes,
        "controls": controls,
        "inspectionLimitations": ["event_bindings_unread", "attribute_types_unread", "nested_data_paths_unread"],
    }


def decode(binary: Path, name: str) -> dict:
    try:
        from v8unpack import helper
        from v8unpack.container_reader import extract
        from v8unpack.MetaDataObject.Form import Form0
        from v8unpack.MetaDataObject.Form.Form9 import Form9
        from v8unpack.version import __version__
    except ImportError as error:
        raise ValueError("v8unpack 1.2.6 is required in the selected Python environment; no packages were installed") from error
    if __version__ != "1.2.6":
        raise ValueError(f"unsupported v8unpack version {__version__}; expected 1.2.6")
    fingerprint = hashlib.sha256(binary.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix="ordinary-form-") as temporary:
        extract(str(binary), temporary, False, False)
        raw = Path(temporary) / "0"
        if not raw.is_dir() or {path.name for path in raw.iterdir()} != {"form", "module"}:
            raise ValueError("unsupported Form.bin container: expected form and module")
        form = Form9(meta_obj_class=Form0, obj_version="13", options={})
        form.header = {"name": name, "Тип формы": "0"}
        form.form = [helper.brace_file_read(str(raw), "form")]
        decoder = form.get_form_element_decoder()
        if decoder is None or form.header.get("Версия элементов формы") not in {"0-26", "0-27"}:
            raise ValueError("unsupported ordinary form schema")
        decoder(form).decode(str(raw), str(raw), "", form.form[0][0])
        form.header["obj_version"] = "9" if form.header["Версия элементов формы"] == "0-26" else "13"
        if hashlib.sha256(binary.read_bytes()).hexdigest() != fingerprint:
            raise ValueError("Form.bin changed while the reader was running")
        return from_reader(form.header, {"tree": form.elements_tree, "data": form.elements_data, "props": form.props}, name, fingerprint, __version__)


def write_atomic(output: Path, sidecar: dict) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=output.parent, prefix=".form-", delete=False) as temporary:
        temporary_path = Path(temporary.name)
        try:
            json.dump(sidecar, temporary, ensure_ascii=False, indent=2)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise
    try:
        os.replace(temporary_path, output)
    finally:
        temporary_path.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-form-bin", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--form-name", required=True)
    args = parser.parse_args()
    try:
        if args.input_form_bin.resolve() == args.output.resolve():
            raise ValueError("output must differ from Form.bin")
        write_atomic(args.output, decode(args.input_form_bin, args.form_name))
    except (ValueError, OSError) as error:
        parser.exit(2, f"ordinary form export: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
