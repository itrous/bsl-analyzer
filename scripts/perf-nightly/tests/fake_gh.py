#!/usr/bin/env python3
"""A fake `gh` for the publication tests: issues live in `$FAKE_GH_STATE`
(JSON), every call is a line of `$FAKE_GH_LOG`. A call whose joined arguments
contain every `|`-separated part of an entry of the state's `fail` list exits 1."""

import json
import os
import sys


def main() -> int:
    args = sys.argv[1:]
    stdin = sys.stdin.read() if "--body-file" in args else None
    with open(os.environ["FAKE_GH_LOG"], "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"args": args, "stdin": stdin}, ensure_ascii=False) + "\n")
    path = os.environ["FAKE_GH_STATE"]
    with open(path, encoding="utf-8") as fh:
        state = json.load(fh)
    joined = " ".join(args)
    if any(all(part in joined for part in fail.split("|")) for fail in state.get("fail", [])):
        print("HTTP 502: Bad Gateway", file=sys.stderr)
        return 1
    issues = state.setdefault("issues", [])

    def option(name):
        return args[args.index(name) + 1]

    def labels():
        return [args[i + 1] for i, a in enumerate(args) if a == "--label"]

    if args[:2] == ["issue", "list"]:
        wanted = option("--label")
        found = [{k: i[k] for k in ("number", "state", "stateReason", "body")} |
                 {"labels": [{"name": n} for n in i["labels"]]} for i in issues if wanted in i["labels"]]
        print(json.dumps(found[:int(option("-L"))]))
    elif args[:2] == ["issue", "create"]:
        number = max([i["number"] for i in issues], default=0) + 1
        issues.append({"number": number, "state": "OPEN", "stateReason": None, "title": option("--title"),
                       "body": stdin, "labels": labels(), "comments": []})
        print(f"https://github.com/itrous/bsl-analyzer/issues/{number}")
    elif args[:2] == ["issue", "comment"]:
        next(i for i in issues if i["number"] == int(args[2]))["comments"].append(stdin)
    elif args[:2] == ["issue", "edit"]:
        issue = next(i for i in issues if i["number"] == int(args[2]))
        issue["labels"].append(option("--add-label"))
    elif args[0] == "api":
        number = int(args[1].split("/")[-2])
        for body in next(i for i in issues if i["number"] == number)["comments"]:
            print(body)
    else:
        print(f"fake gh: unsupported {args}", file=sys.stderr)
        return 2
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(state, fh, ensure_ascii=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
