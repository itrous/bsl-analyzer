"""Analysis modes end to end on a real `bsl-analyzer-app` (plan §5, И50).

Needs a binary built by `perf_nightly.py build` (its directory holds
`build.json`) and a platform help package:
    PERF_NIGHTLY_E2E_BINARY=<DATA>/bin/<dir>/bsl-analyzer-app
    PERF_NIGHTLY_E2E_HELP=<help package or HBK directory>
Without them the tests are skipped. Only file sets and reasons are checked.
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest

from helpers import FakeClock, event_driven_process_waits, mock_env, run_git

import perf_nightly
from common import Data, read_json, sha256_bytes, write_json

BINARY = os.environ.get("PERF_NIGHTLY_E2E_BINARY")
HELP = os.environ.get("PERF_NIGHTLY_E2E_HELP")
FIXTURE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures", "e2e-corpus")


@unittest.skipUnless(BINARY and HELP, "set PERF_NIGHTLY_E2E_BINARY and PERF_NIGHTLY_E2E_HELP")
class Modes(unittest.TestCase):
    def setUp(self):
        event_driven_process_waits(self)
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-e2e-")
        self.tmp = self._tmp.name
        mock_env(self, {"FAKE_MODE": ""})
        self.source = os.path.join(self.tmp, "source")
        shutil.copytree(os.path.join(FIXTURE, "base"), self.source)
        run_git("init", "-q", "-b", "main", cwd=self.source)
        run_git("config", "user.email", "t@t", cwd=self.source)
        run_git("config", "user.name", "t", cwd=self.source)
        run_git("add", "-A", cwd=self.source)
        run_git("commit", "-q", "-m", "vendor", cwd=self.source)
        run_git("branch", "vendor", cwd=self.source)
        self.base = self.rev("HEAD")
        shutil.copytree(os.path.join(FIXTURE, "change"), self.source, dirs_exist_ok=True)
        run_git("commit", "-q", "-am", "change", cwd=self.source)
        self.head = self.rev("HEAD")
        self.data_dir = os.path.join(self.tmp, "data")
        self.data = Data(self.data_dir)
        bin_dir = os.path.join(self.data_dir, "bin", "e2e")
        os.makedirs(bin_dir)
        self.binary = os.path.join(bin_dir, "bsl-analyzer-app")
        shutil.copy2(BINARY, self.binary)
        shutil.copy2(os.path.join(os.path.dirname(BINARY), "build.json"), bin_dir)

    def tearDown(self):
        self._tmp.cleanup()

    def rev(self, ref):
        return subprocess.run(["git", "rev-parse", ref], cwd=self.source, check=True,
                              capture_output=True, text=True).stdout.strip()

    def corpus(self, corpus_id, mode, base=None):
        os.makedirs(self.data_dir, exist_ok=True)
        entry = (f'[[corpus]]\nid = "{corpus_id}"\nlabel = "x"\nsource_repo = "{self.source}"\n'
                 f'commit = "{self.head}"\nmode = "{mode}"\n')
        if mode == "diff":
            entry += f'diff_base_ref = "vendor"\ndiff_base_sha = "{base}"\n'
        path = os.path.join(self.data_dir, "corpora.toml")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(f'[platform_help]\nsource = "external"\npath = "{HELP}"\n\n{entry}')
        args = perf_nightly.parser().parse_args(["snapshot", "--data-dir", self.data_dir, "--corpus", corpus_id])
        perf_nightly.cmd_snapshot(args)
        args = perf_nightly.parser().parse_args(["helpcheck", "--data-dir", self.data_dir, "--binary", self.binary])
        self.assertEqual(perf_nightly.cmd_helpcheck(args, FakeClock()), 0)

    def analyze(self, corpus_id):
        args = perf_nightly.parser().parse_args([
            "measure", "--data-dir", self.data_dir, "--corpus", corpus_id, "--binary", self.binary,
            "--scenario", "analyze", "--temp", "warm", "--series", "e2e", "--runs", "1"])
        perf_nightly.Measure(args, FakeClock(), self.data, foreign_prefix="perf-nightly-test").run()
        series = os.path.join(self.data.results(perf_nightly.load_binary(self.binary)), corpus_id,
                              "analyze", "warm", "e2e")
        record = read_json(os.path.join(series, "r1.json"))
        files = []
        jsonl = os.path.join(series, "r1", "analyze.jsonl")
        if os.path.exists(jsonl):
            with open(jsonl, encoding="utf-8") as fh:
                files = sorted(os.path.basename(e["path"]) for e in map(json.loads, fh) if e["type"] == "file")
        return record, files

    def test_diff_mode_analyses_only_the_changed_module(self):
        self.corpus("d", "diff", self.base)
        record, files = self.analyze("d")
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(files, ["Второй.bsl"])

    def test_full_mode_analyses_both_modules(self):
        self.corpus("f", "full")
        record, files = self.analyze("f")
        self.assertTrue(record["held"], record["errors"])
        self.assertEqual(files, ["Второй.bsl", "Первый.bsl"])

    def test_full_config_with_diff_base_put_back_is_a_scope_mismatch(self):
        self.corpus("f", "full")
        meta_path = self.data.snapshot_meta("f")
        meta = read_json(meta_path)
        text = meta["variants"]["help"]["text"].replace("[analysis]\n", '[analysis]\ndiff_base = "vendor"\n', 1)
        self.assertIn("diff_base", text)
        meta["variants"]["help"] = {"text": text, "config_sha256": sha256_bytes(text.encode())}
        write_json(meta_path, meta)
        run_git("branch", "vendor", self.base, cwd=self.data.snapshot("f"))
        record, files = self.analyze("f")
        self.assertEqual(files, ["Второй.bsl"])
        self.assertEqual((record["held"], record["reason"]), (False, "scope_mismatch"))

    def test_diff_without_changes_is_not_held(self):
        self.corpus("n", "diff", self.head)
        record, _ = self.analyze("n")
        self.assertFalse(record["held"])
        self.assertEqual(record["reason"], "empty_analysis")


if __name__ == "__main__":
    unittest.main()
