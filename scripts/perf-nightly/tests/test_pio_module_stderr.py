"""A `night` whose stderr is not UTF-8 is a failed night, not a crashed module (plan §10.2 п. 5:
`night` exits non-zero — `summary.json` `failed` with the last stderr line, and the `failed` notice)."""

import os
import unittest

import test_pio_module as base
from common import read_json
from helpers import run_git

NIGHT = '''import sys
sys.stderr.buffer.write(b"night: refused: a path with a bad byte \\xff\\n")
sys.exit(1)
'''


class UndecodableStderr(unittest.TestCase):
    setUp = base.Module.setUp
    tearDown = base.Module.tearDown
    rows = base.Module.rows
    request = base.Module.request
    run_module = base.Module.run_module
    night_dir = base.Module.night_dir

    def develop_commit(self, message):
        target = os.path.join(self.origin, "scripts", "perf-nightly", "perf_nightly.py")
        with open(target, "w", encoding="utf-8") as fh:
            fh.write(NIGHT + f"# {message}\n")
        run_git("add", "-A", cwd=self.origin)
        run_git("commit", "-q", "-m", message, cwd=self.origin)

    def test_a_night_whose_stderr_is_not_utf8_is_reported_as_failed(self):
        response = base.checked_answer(self, self.run_module())
        self.assertEqual([n["key"] for n in response["notices"]], [f"{base.DATE}:failed"])
        self.assertEqual(read_json(os.path.join(self.night_dir(), "summary.json"))["state"], "failed")


if __name__ == "__main__":
    unittest.main()
