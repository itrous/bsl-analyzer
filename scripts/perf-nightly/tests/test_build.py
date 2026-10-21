"""Local build invocation, metadata and reuse of the same SHA/toolchain."""

import contextlib
import io
import os
import subprocess
from unittest.mock import patch

from helpers import Workspace

import perf_nightly
from common import read_json, sha256_file


class Build(Workspace):
    def test_build_uses_locked_release_and_reuses_the_artifact(self):
        real_run = subprocess.run
        builds = []

        def run(argv, **kwargs):
            if argv == ["cargo", "build", "--locked", "--release", "--bin", "bsl-analyzer-app"]:
                builds.append(kwargs)
                target = os.path.join(kwargs["cwd"], "target", "release")
                os.makedirs(target, exist_ok=True)
                with open(os.path.join(target, "bsl-analyzer-app"), "wb") as fh:
                    fh.write(b"synthetic binary")
                return subprocess.CompletedProcess(argv, 0)
            if argv == ["rustc", "-V"]:
                return subprocess.CompletedProcess(argv, 0, "rustc test\n", "")
            return real_run(argv, **kwargs)

        args = self.args(remote=self.source, ref="main", label="test")
        with patch.object(perf_nightly, "repo_root", return_value=self.source), \
                patch.object(perf_nightly.subprocess, "run", side_effect=run), \
                patch.dict(os.environ, {"CARGO_TARGET_DIR": "/must-not-be-used"}), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(perf_nightly.cmd_build(args), 0)
            self.assertEqual(perf_nightly.cmd_build(args), 0)
        self.assertEqual(len(builds), 1)
        self.assertNotIn("CARGO_TARGET_DIR", builds[0]["env"])
        self.assertEqual(builds[0]["cwd"], self.data.path("build"))
        bin_root = self.data.path("bin")
        built = [os.path.join(bin_root, name) for name in os.listdir(bin_root)
                 if name.startswith("test-") and os.path.isdir(os.path.join(bin_root, name))]
        self.assertEqual(len(built), 1)
        meta = read_json(os.path.join(built[0], "build.json"))
        self.assertEqual(meta["sha"], self.meta["commit"])
        self.assertEqual(meta["rustc"], "rustc test")
        self.assertEqual(meta["sha256"], sha256_file(os.path.join(built[0], "bsl-analyzer-app")))
