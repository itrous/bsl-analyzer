"""Test fixtures: a fake clock, a fake `bsl-analyzer-app`, a synthetic corpus."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import select
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import corpus  # noqa: E402
import perf_nightly  # noqa: E402
import procs  # noqa: E402
from common import Data, sha256_file, write_json  # noqa: E402

FAKE_APP = os.path.join(HERE, "fake_app.py")
# Real-process tests use a name the production foreign check (`bsl-analyzer*`)
# never matches, so a test run cannot disturb a measurement on the same host.
FOREIGN_PREFIX = "perf-nightly-test"
MODULE = "﻿Процедура Тест() Экспорт\n\tСообщить(\"т\");\nКонецПроцедуры\n"


class FakeClock:
    """Virtual time. A wait ends on its event, or — only when the scripted
    server announced silence for that wait — on a timeout that advances
    virtual time by exactly the timeout. Sleeps advance virtual time."""

    def __init__(self, stuck=lambda pid: False):
        self.t = 0.0
        self.lock = threading.Lock()
        self.waits: list = []
        self.sleeps: list = []
        self.stuck = stuck

    def now(self) -> float:
        with self.lock:
            return self.t

    def advance(self, seconds: float) -> None:
        with self.lock:
            self.t += seconds

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.advance(seconds)

    def wait(self, ready, timeout, cond, key=None) -> bool:
        with cond:
            while True:
                if ready():
                    self.waits.append((key, timeout, "event"))
                    return True
                if silenced(key):
                    self.advance(timeout)
                    self.waits.append((key, timeout, "timeout"))
                    return False
                cond.wait()

    def wait_exit(self, pidfds: dict, timeout: float) -> bool:
        stuck = [pid for pid in pidfds if self.stuck(pid)]
        others = {pid: fd for pid, fd in pidfds.items() if pid not in stuck}
        wait_for_exit(others)
        if stuck:
            self.advance(timeout)
            return False
        return True


def wait_for_exit(pidfds: dict) -> None:
    pending = set(pidfds.values())
    poller = select.poll()
    for fd in pending:
        poller.register(fd, select.POLLIN)
    while pending:
        for fd, _ in poller.poll():
            poller.unregister(fd)
            pending.remove(fd)


def event_driven_process_waits(case: unittest.TestCase) -> None:
    """Use exit events after SIGKILL and stop events instead of sampling timers."""
    case.enterContext(patch.object(procs, "RealClock", FakeClock))

    def sample_until_stopped(sampler):
        sampler.sample()
        sampler._stop.wait()

    case.enterContext(patch.object(procs.Sampler, "_loop", sample_until_stopped))


def silenced(key) -> bool:
    """True once the fake server announced that it will never answer `key`."""
    if not key:
        return False
    kind, client, ident = key
    notes = client.notifications_snapshot() if kind.startswith("lsp") else list(client.notifications)
    for note in notes:
        if note.get("method") in ("$/fake/silent", "fake/silent"):
            params = note.get("params") or {}
            if params.get("id") == ident or params.get("uri") == ident:
                return True
    return False


def run_git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


class Workspace(unittest.TestCase):
    """A data dir with a synthetic corpus, its snapshot, points and a fake binary."""

    config = '[analysis]\ndiff_base = "vendor"\n\n[features]\nchecked_environments = ["Сервер"]\n'

    def setUp(self):
        event_driven_process_waits(self)
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-nightly-test-")
        self.tmp = self._tmp.name
        self.log = os.path.join(self.tmp, "log")
        os.makedirs(self.log)
        self.env = mock_env(self, {"FAKE_LOG": self.log, "FAKE_MODE": ""})
        self.data_dir = os.path.join(self.tmp, "data")
        self.data = Data(self.data_dir)
        self.source = os.path.join(self.tmp, "source")
        self.make_source()
        help_path = os.path.join(self.tmp, "help.json")
        with open(help_path, "w") as fh:
            fh.write("{}")
        self.manifest_path = os.path.join(self.data_dir, "corpora.toml")
        os.makedirs(self.data_dir, exist_ok=True)
        commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.source, check=True,
                                capture_output=True, text=True).stdout.strip()
        with open(self.manifest_path, "w", encoding="utf-8") as fh:
            fh.write(f'[platform_help]\nsource = "external"\npath = "{help_path}"\n\n'
                     f'[[corpus]]\nid = "crm"\nlabel = "ЦРМ"\nsource_repo = "{self.source}"\n'
                     f'commit = "{commit}"\nmode = "full"\n')
        self.manifest = corpus.load_manifest(self.manifest_path)
        self.meta = corpus.make_snapshot(self.data, self.manifest, "crm")
        self.snap = self.data.snapshot("crm")
        corpus.prepare_snapshot(self.snap, self.meta, "help")
        self.write_points()
        self.binary = self.fake_binary()

    def tearDown(self):
        self._tmp.cleanup()

    def make_source(self):
        os.makedirs(os.path.join(self.source, "Модули"))
        run_git("init", "-q", "-b", "main", cwd=self.source)
        run_git("config", "user.email", "t@t", cwd=self.source)
        run_git("config", "user.name", "t", cwd=self.source)
        with open(os.path.join(self.source, "bsl-analyzer.toml"), "w", encoding="utf-8") as fh:
            fh.write(self.config)
        for name in ("Большой", "Малый"):
            with open(os.path.join(self.source, "Модули", f"{name}.bsl"), "w", encoding="utf-8") as fh:
                fh.write(MODULE)
        run_git("add", "-A", cwd=self.source)
        run_git("commit", "-q", "-m", "base", cwd=self.source)
        run_git("branch", "vendor", cwd=self.source)

    def write_points(self, drop=()):
        offset = MODULE.encode().index("Тест".encode())
        large, small = "Модули/Большой.bsl", "Модули/Малый.bsl"
        targets = [
            ("document_symbol/common_large/01", large, {"feature": "document_symbol"}),
            ("diagnostics_push/common_large/01", large, {"feature": "diagnostics_push"}),
            ("document_symbol/common_small/02", small, {"feature": "document_symbol"}),
            ("diagnostics_push/common_small/02", small, {"feature": "diagnostics_push"}),
            ("inlay_hints/common_large/01", large, {"feature": "inlay_hints", "range": None}),
            ("hover/common_large/01", large, {"feature": "hover", "offset": offset}),
            ("completion/common_large/01", large, {"feature": "completion", "offset": offset}),
            ("goto_definition/common_large/01", large, {"feature": "goto_definition", "offset": offset}),
            ("references/common_large/01", large, {"feature": "references", "offset": offset}),
            ("rename/common_large/01", large, {"feature": "rename", "offset": offset, "new_name": "Н"}),
            ("workspace_symbol/scan_common_large/01", large, {"feature": "workspace_symbol", "query": "Тес"}),
        ]
        discovered = {"targets": [{"id": i, "relative_path": p, "spec": s}
                                  for i, p, s in targets if i not in drop]}
        doc = corpus.build_points(self.snap, discovered, self.meta, {"label": "fake", "sha": "f" * 40})
        write_json(self.data.points("crm"), doc)
        return doc

    def fake_binary(self, has_help=True, label="fake"):
        bin_dir = os.path.join(self.data_dir, "bin", label)
        os.makedirs(bin_dir, exist_ok=True)
        path = os.path.join(bin_dir, "bsl-analyzer-app")
        with open(path, "w") as fh:
            fh.write(f'#!/bin/sh\nexec "{sys.executable}" "{FAKE_APP}" "$@"\n')
        os.chmod(path, 0o755)
        write_json(os.path.join(bin_dir, "build.json"), {
            "label": label, "sha": "a" * 40, "rustc": "rustc test", "has_platform_help": has_help,
            "sha256": sha256_file(path)})
        return path

    def args(self, **kw):
        base = {"data_dir": self.data_dir, "corpora": None, "corpus": "crm", "binary": self.binary,
                "series": "t", "runs": 3, "clients": None, "help_override": None}
        base.update(kw)
        return argparse.Namespace(**base)

    def helpcheck(self, **kw):
        with contextlib.redirect_stdout(io.StringIO()):
            return perf_nightly.cmd_helpcheck(self.args(**kw), FakeClock())

    def measure(self, clock=None, **kw) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            perf_nightly.Measure(self.args(**kw), clock or FakeClock(), self.data,
                                 foreign_prefix=FOREIGN_PREFIX).run()
        return out.getvalue()

    def series_dir(self, scenario, temp="warm", series="t"):
        return os.path.join(self.data.results({"label": "fake", "sha": "a" * 40}), "crm",
                            scenario, temp, series)

    def record(self, scenario, name, temp="warm"):
        with open(os.path.join(self.series_dir(scenario, temp), f"{name}.json"), encoding="utf-8") as fh:
            return json.load(fh)

    def launches(self) -> list:
        path = os.path.join(self.log, "launches.jsonl")
        if not os.path.exists(path):
            return []
        with open(path, encoding="utf-8") as fh:
            return [json.loads(line) for line in fh]


def mock_env(case: unittest.TestCase, values: dict) -> dict:
    saved = {k: os.environ.get(k) for k in values}
    os.environ.update(values)

    def restore():
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    case.addCleanup(restore)
    return values


def set_mode(*switches: str) -> None:
    os.environ["FAKE_MODE"] = ",".join(switches)
