#!/usr/bin/env python3
"""Build windows-matrix.json from the prepared native request manifest.

Each row keeps the exact MCP `execute` arguments and the expected outcome.
Rows that need a lifecycle stimulus (cancel, timeout, crash) are listed
separately because a plain sequential call cannot reproduce them.
"""
import argparse
import json
import pathlib

# The guest runner replaces this token with its --sentinel-dir.
SENTINELS = "{SENTINELS}"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True, help="proposed-requests.json")
    parser.add_argument("--repo-root", required=True, help="root the manifest paths are relative to")
    parser.add_argument("--out", required=True)
    parser.add_argument("--sentinel-dir", required=True,
                        help="guest sentinel directory used in the manifest; stored as {SENTINELS}")
    parser.add_argument("--corrections", default=str(pathlib.Path(__file__).with_name("oracle-corrections.json")))
    args = parser.parse_args()
    if not args.sentinel_dir:
        parser.error("--sentinel-dir must not be empty")

    root = pathlib.Path(args.repo_root)
    manifest = json.loads(pathlib.Path(args.manifest).read_text(encoding="utf-8"))
    corrections = {key: value for key, value in
                   json.loads(pathlib.Path(args.corrections).read_text(encoding="utf-8")).items()
                   if not key.startswith("_")}
    rows, lifecycle = [], []
    for request in manifest["requests"]:
        if request["platform"] != "windows" or request["source_binding"] not in ("L", "M"):
            continue
        arguments = json.loads((root / request["mcp_arguments"]["path"]).read_text(encoding="utf-8"))
        expected = {key: value for key, value in request["expected"].items() if key != "position_unit"}
        if request["id"] in corrections:
            marker = expected.get("execution_marker_must_not_exist")
            expected = dict(corrections.pop(request["id"])["expected"])
            if marker:
                expected["execution_marker_must_not_exist"] = marker
        if "stimulus" in request:
            lifecycle.append({"id": request["id"], "arguments": arguments, "expected": expected,
                              "stimulus": request["stimulus"]})
            continue
        rows.append({
            "id": request["id"],
            "fixture": request["source_binding"],
            "scope": request["scope"],
            "arguments": arguments,
            "expected": expected,
        })
    if corrections:
        raise SystemExit(f"corrections for unknown rows: {sorted(corrections)}")
    out = {"rows": rows, "lifecycle": lifecycle}
    text = json.dumps(out, ensure_ascii=False, indent=1)
    text = text.replace(json.dumps(args.sentinel_dir, ensure_ascii=False)[1:-1], SENTINELS)
    pathlib.Path(args.out).write_text(text + "\n", encoding="utf-8")
    print(f"{len(rows)} rows, {len(lifecycle)} lifecycle rows listed separately")


if __name__ == "__main__":
    main()
