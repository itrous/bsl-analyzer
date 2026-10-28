"""Stand-in for `perf_nightly.py` in the module tests' `<DATA>/src` clone.

`night` appends its argv to `$FAKE_NIGHT_LOG`, then by `$FAKE_NIGHT_EXIT`
prints two stderr lines and exits with that code, or, with `kill`, kills the
module that started it. Otherwise it writes a `done` summary with verdict
`$FAKE_NIGHT_VERDICT` and, with `--publish`, publishes the rows of
`$FAKE_NIGHT_ROWS` through the real `publish.py` of `$FAKE_HARNESS`.
"""

import argparse
import json
import os
import signal
import sys

sys.path.insert(0, os.environ["FAKE_HARNESS"])

import publish  # noqa: E402
from common import write_json  # noqa: E402


def main() -> int:
    with open(os.environ["FAKE_NIGHT_LOG"], "a", encoding="utf-8") as fh:
        fh.write(json.dumps(sys.argv[1:]) + "\n")
    top = argparse.ArgumentParser()
    top.add_argument("command")
    for name in ("--data-dir", "--corpora", "--deadline", "--date"):
        top.add_argument(name)
    top.add_argument("--publish", action="store_true")
    args = top.parse_args()
    exit_code = os.environ.get("FAKE_NIGHT_EXIT")
    if exit_code == "kill":
        os.kill(os.getppid(), signal.SIGKILL)
        return 0
    if exit_code:
        print("building develop", file=sys.stderr)
        print("perf_nightly: refused: build_failed: cargo build exited 101", file=sys.stderr)
        return int(exit_code)
    out_dir = os.path.join(args.data_dir, "nights", args.date)
    with open(os.environ["FAKE_NIGHT_ROWS"], encoding="utf-8") as fh:
        rows = json.load(fh)
    summary = {"state": "done", "kind": "night", "name": args.date, "date": args.date,
               "verdict": os.environ["FAKE_NIGHT_VERDICT"], "reasons": [], "not_compared": 2,
               "skipped": [{"role": "base"}], "candidate": {"label": "develop", "sha": "c" * 40},
               "base": {"label": "v0.3.2", "sha": "b" * 40}}
    if args.publish:
        summary["publish"] = publish.publish_perf(out_dir, summary, rows, None, {"crm": "ЦРМ"}, [],
                                                  lambda sdir: 3)
    with open(os.path.join(out_dir, "report.md"), "w", encoding="utf-8") as fh:
        fh.write(f"# perf-nightly {args.date}\n")
    write_json(os.path.join(out_dir, "summary.json"), summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
