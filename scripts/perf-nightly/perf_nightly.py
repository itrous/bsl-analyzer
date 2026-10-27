#!/usr/bin/env python3
"""Nightly performance harness for bsl-analyzer (plan docs/plans/perf-nightly.md).

Drives any bsl-analyzer-app build as a black box over its CLI and stdio:
`analyze`, the LSP server and the MCP server, on private corpus snapshots kept
in a data directory outside this repository. Standard library only.
"""

from __future__ import annotations

import argparse
import datetime
import fcntl
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import corpus  # noqa: E402
import procs  # noqa: E402
import scenarios  # noqa: E402
from clock import RealClock  # noqa: E402
from common import (  # noqa: E402
    HARNESS_SCHEMA, MISSING_REASONS, Data, HarnessError, git, median, read_json, repo_root, sha256_bytes,
    sha256_file, sha256_tree, write_json,
)

TAG_RE = re.compile(r"v\d+\.\d+\.\d+")


def build_clone(data: Data, remote: str | None = None) -> str:
    """The build clone (plan §6.1), cloned on first use and fetched with tags."""
    clone = data.path("build")
    if not os.path.isdir(os.path.join(clone, ".git")):
        origin = remote or git("remote", "get-url", "origin", cwd=repo_root())
        git("clone", "--no-checkout", repo_root(), clone)
        git("remote", "set-url", "origin", origin, cwd=clone)
    git("fetch", "--tags", "origin", cwd=clone)
    return clone


def cmd_build(args) -> int:
    data = Data(args.data_dir)
    # Two builds share one clone: a checkout under a running build would let it
    # ship another tree under its own label and SHA.
    os.makedirs(data.root, exist_ok=True)
    with open(data.path("build.lock"), "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise HarnessError("data_busy", "another build holds this data dir") from None
        build_clone(data, args.remote)
        binary, cached = build_ref(data, args.ref, args.label)
        meta = read_json(os.path.join(os.path.dirname(binary), "build.json"))
        print(f"build={meta['label']} sha={meta['sha'][:12]} cached={str(cached).lower()}")
        print(binary)
        return 0


def build_ref(data: Data, ref_name: str, label: str | None = None) -> tuple[str, bool]:
    """`cargo build --locked --release` of a ref in the fetched build clone,
    reused when a binary of the same SHA and `rustc -V` exists (plan D4)."""
    clone = data.path("build")
    ref = "origin/develop" if ref_name == "develop" else ref_name
    sha = git("rev-parse", "--verify", f"{ref}^{{commit}}", cwd=clone)
    if label is None:
        if TAG_RE.fullmatch(ref_name):
            label = ref_name
        elif ref_name in ("develop", "origin/develop"):
            label = "develop"
        else:
            raise HarnessError("label_required", "pass --label for a ref that is neither a tag nor develop")
    rustc = subprocess.run(["rustc", "-V"], cwd=clone, capture_output=True, text=True,
                           check=True).stdout.strip()
    bin_dir = data.path("bin", f"{label}-{sha[:12]}-{sha256_bytes(rustc.encode())[:12]}")
    binary = os.path.join(bin_dir, "bsl-analyzer-app")
    if os.path.isfile(os.path.join(bin_dir, "build.json")):
        return binary, True
    git("checkout", "--force", "--detach", sha, cwd=clone)
    # The build must land in the clone's own target/, whatever the caller exported.
    env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
    log_path = bin_dir + ".build.log"
    os.makedirs(os.path.dirname(log_path), exist_ok=True)
    with open(log_path, "wb") as log:
        result = subprocess.run(
            ["cargo", "build", "--locked", "--release", "--bin", "bsl-analyzer-app"],
            cwd=clone, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode != 0:
        raise HarnessError("build_failed", f"cargo exited {result.returncode}, log: {log_path}")
    os.makedirs(bin_dir, exist_ok=True)
    shutil.copy2(os.path.join(clone, "target", "release", "bsl-analyzer-app"), binary)
    write_json(os.path.join(bin_dir, "build.json"), {
        "label": label,
        "sha": sha,
        "rustc": rustc,
        "has_platform_help": bool(git("ls-tree", sha, "crates/platform-help", cwd=clone)),
        "sha256": sha256_file(binary),
    })
    return binary, False


MCP_SCENARIOS = ("mcp-start", "mcp-memory", "mcp-load")
# Scenarios measured on the warm cache that `lsp-session` of the same series left.
LSP_FOLLOWERS = ("lsp-close-memory",)
DEFAULT_CLIENTS = {"crm": [2, 4], "ut": [4]}
HELPCHECK_TIMEOUT_S = 300.0
# Discovery probes every feature in-process; CRM takes about 30 s.
DISCOVER_TIMEOUT_S = 3600.0
LOAD_ERROR_CLASSES = ("timeout", "empty result", "closed", "point_missing", "tool_missing")


def corpora_path(args, data: Data) -> str:
    return args.corpora or data.path("corpora.toml")


def load_binary(path: str) -> dict:
    path = os.path.abspath(path)
    meta_path = os.path.join(os.path.dirname(path), "build.json")
    if not os.path.isfile(meta_path):
        raise HarnessError("binary_unknown", "the binary dir has no build.json; build it with `build`")
    meta = read_json(meta_path)
    if meta.get("sha256") != sha256_file(path):
        raise HarnessError("binary_changed", "the binary differs from its build.json")
    return {**meta, "path": path}


def help_identity(binary: dict, help_section: dict) -> dict:
    """`help` of a run (plan §8.5): the configured file, or `builtin` for a
    binary without the platform-help crate."""
    if not binary.get("has_platform_help"):
        return {"source": "builtin"}
    path = help_section.get("path")
    return {"source": help_section.get("source"),
            "sha256": sha256_tree(path) if path and os.path.exists(path) else None}


def mem_total_gib(value: str | None) -> str | None:
    """`MemTotal` in whole GiB: the kernel's reservations move the exact figure
    by a few kB between boots of the same machine, and a reboot must not make
    the calibration stale."""
    if not value or not value.endswith(" kB"):
        return value
    return f"{round(int(value.split()[0]) / (1 << 20))} GiB"


def env_fingerprint(binary: dict, meta: dict, points_sha: str) -> dict:
    def proc_field(path, name):
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                if line.startswith(name):
                    return line.split(":", 1)[1].strip()
        return None

    return {
        "cpu_model": proc_field("/proc/cpuinfo", "model name"),
        "nproc": os.cpu_count(),
        "mem_total": mem_total_gib(proc_field("/proc/meminfo", "MemTotal")),
        "kernel": os.uname().release,
        "rustc": binary.get("rustc"),
        "harness_schema": HARNESS_SCHEMA,
        "snapshot_commit": meta["commit"],
        "mode": meta["mode"],
        "points_sha256": points_sha,
    }


def xdg_dirs(base: str, cache: str) -> dict:
    dirs = {"XDG_CACHE_HOME": cache, "XDG_STATE_HOME": os.path.join(base, "state"),
            "XDG_DATA_HOME": os.path.join(base, "data")}
    for path in dirs.values():
        os.makedirs(path, exist_ok=True)
    return dirs


def wait_child(proc, clock, timeout: float) -> bool:
    try:
        fds = {proc.pid: os.pidfd_open(proc.pid)}
    except ProcessLookupError:
        return True
    try:
        return clock.wait_exit(fds, timeout)
    finally:
        os.close(fds[proc.pid])


# -- snapshot / points / helpcheck ----------------------------------------------


def cmd_snapshot(args) -> int:
    data = Data(args.data_dir)
    manifest = corpus.load_manifest(corpora_path(args, data))
    meta = corpus.make_snapshot(data, manifest, args.corpus)
    corpus.prepare_snapshot(data.snapshot(args.corpus), meta, "help")
    print(f"snapshot corpus={args.corpus} commit={meta['commit'][:12]} mode={meta['mode']} "
          f"files={meta['files_total']}")
    return 0


def cmd_points(args, clock=None) -> int:
    clock = clock or RealClock()
    data = Data(args.data_dir)
    binary = load_binary(args.binary)
    meta = corpus.load_snapshot(data, args.corpus)
    snap = data.snapshot(args.corpus)
    corpus.prepare_snapshot(snap, meta, "help" if binary.get("has_platform_help") else "builtin")
    work = data.path("xdg", "points", args.corpus)
    shutil.rmtree(work, ignore_errors=True)
    run_id = f"points-{uuid.uuid4().hex[:12]}"
    env = procs.child_env(run_id, xdg_dirs(work, os.path.join(work, "cache")))
    manifest_path = os.path.join(work, "discover.json")
    with open(os.path.join(work, "stderr.txt"), "wb") as err:
        proc = subprocess.Popen([binary["path"], "bench", "discover", "--source-dir", snap,
                                 "--output", manifest_path], cwd=snap, env=env,
                                stdout=subprocess.DEVNULL, stderr=err)
        finished = wait_child(proc, clock, DISCOVER_TIMEOUT_S)
        procs.stop(run_id, clock)
        proc.wait()
    if not finished:
        raise HarnessError("discover_timeout", f"bench discover did not exit within {DISCOVER_TIMEOUT_S:.0f} s")
    if proc.returncode != 0:
        raise HarnessError("discover_failed", f"bench discover exited {proc.returncode}")
    doc = corpus.build_points(snap, read_json(manifest_path), meta, binary)
    write_json(data.points(args.corpus), doc)
    shutil.rmtree(work, ignore_errors=True)
    chosen = corpus.select_points(doc)
    keys = " ".join(f"{key}={'yes' if key in chosen else 'n/a'}" for key in corpus.POINT_KEYS)
    print(f"points corpus={args.corpus} total={len(doc['points'])} "
          f"sha256={sha256_file(data.points(args.corpus))[:12]}")
    print(f"keys {keys} F_rot={len(chosen['F_rot'])}")
    return 0


HELP_SOURCE_RE = re.compile(r"\bsource=(.*?)\s+origin=")


def run_helpcheck(data: Data, binary: dict, help_section: dict, clock) -> dict:
    """`analyze` of a one-module project at `BSL_LOG=info` (plan §8): passed
    only with `platform help loaded` from the configured source and no help
    warning."""
    work = data.path("helpcheck", f"{binary['label']}-{binary['sha'][:12]}")
    shutil.rmtree(work, ignore_errors=True)
    project = os.path.join(work, "project")
    os.makedirs(project)
    with open(os.path.join(project, corpus.CONFIG_NAME), "w", encoding="utf-8") as fh:
        fh.write("[platform_help]\n")
        fh.writelines(f"{key} = {corpus._toml_value(value)}\n" for key, value in help_section.items())
    with open(os.path.join(project, "Module.bsl"), "w", encoding="utf-8") as fh:
        fh.write("Процедура Проба() Экспорт\n\tСообщить(\"проба\");\nКонецПроцедуры\n")
    run_id = f"helpcheck-{uuid.uuid4().hex[:12]}"
    env = procs.child_env(run_id, xdg_dirs(work, os.path.join(work, "cache")), {"BSL_LOG": "info"})
    stderr_path = os.path.join(work, "stderr.txt")
    with open(stderr_path, "wb") as err:
        proc = subprocess.Popen([binary["path"], "analyze", "--format=jsonl", "--quiet",
                                 "--source-dir", project], cwd=project, env=env,
                                stdout=subprocess.DEVNULL, stderr=err)
        finished = wait_child(proc, clock, HELPCHECK_TIMEOUT_S)
        procs.stop(run_id, clock)
        proc.wait()
    with open(stderr_path, encoding="utf-8", errors="replace") as fh:
        text = scenarios.ANSI_RE.sub("", fh.read())
    loaded = [line for line in text.splitlines() if "platform help loaded" in line]
    seen = None
    if loaded:
        match = HELP_SOURCE_RE.search(loaded[0])
        seen = match.group(1) if match else None
    kind, path = help_section.get("source"), help_section.get("path")
    expected = f"{kind} ({path})" if path else kind
    warnings = scenarios.help_warnings(stderr_path)
    if not finished:
        reason = "timeout"
    elif not loaded:
        reason = "no `platform help loaded` line"
    elif seen != expected:
        reason = "loaded from another source"
    elif warnings:
        reason = "help warning in stderr"
    else:
        reason = None
    return {"passed": reason is None, "reason": reason, "source_seen": seen,
            "warnings": warnings, "exit_code": proc.returncode}


def cmd_helpcheck(args, clock=None) -> int:
    clock = clock or RealClock()
    data = Data(args.data_dir)
    binary = load_binary(args.binary)
    if args.help_override:
        with open(args.help_override, "rb") as fh:
            help_section = tomllib.load(fh).get("platform_help") or {}
    else:
        help_section = corpus.load_manifest(corpora_path(args, data))["platform_help"]
    identity = help_identity(binary, help_section)
    if binary.get("has_platform_help"):
        record = run_helpcheck(data, binary, help_section, clock)
    else:
        record = {"passed": True, "reason": None, "builtin": True}
    record.update(help=identity, binary={"label": binary["label"], "sha": binary["sha"]},
                  override=bool(args.help_override))
    write_json(os.path.join(data.results(binary), "helpcheck.json"), record)
    print(f"helpcheck binary={binary['label']} source={identity['source']} "
          f"passed={str(record['passed']).lower()} reason={record['reason'] or '-'}")
    return 0 if record["passed"] else 1


# -- measure --------------------------------------------------------------------


def cache_leaves(cache_root: str) -> list[str]:
    """Analyzer cache paths under an XDG cache root, cut at the leaf the code
    creates: `bsl-analyzer/workspaces/<v>/<digest>`, `bsl-analyzer/platform-help/<kind>`,
    else the first level below `bsl-analyzer/`. Other entries are not the
    analyzer's and never enter the map."""
    base = os.path.join(cache_root, "bsl-analyzer")
    if not os.path.isdir(base):
        return []
    leaves = []
    for entry in sorted(os.listdir(base)):
        path = os.path.join(base, entry)
        if entry == "workspaces" and os.path.isdir(path):
            for version in sorted(os.listdir(path)):
                inner = os.path.join(path, version)
                names = sorted(os.listdir(inner)) if os.path.isdir(inner) else []
                leaves.extend(f"bsl-analyzer/workspaces/{version}/{d}" for d in names)
                if not names:
                    leaves.append(f"bsl-analyzer/workspaces/{version}")
        elif entry == "platform-help" and os.path.isdir(path):
            leaves.extend(f"bsl-analyzer/platform-help/{d}" for d in sorted(os.listdir(path)))
        else:
            leaves.append(f"bsl-analyzer/{entry}")
    return leaves


def workspace_untracked(root: str) -> set[str]:
    out = subprocess.run(["git", "status", "--porcelain=v1", "-z", "--ignored",
                          "--untracked-files=normal"], cwd=root, capture_output=True, check=True).stdout
    return {entry[3:].decode("utf-8").rstrip("/") for entry in out.split(b"\0")
            if entry[:3] in (b"?? ", b"!! ")}


def _remove(path: str) -> None:
    if os.path.isdir(path) and not os.path.islink(path):
        shutil.rmtree(path)
    elif os.path.lexists(path):
        os.remove(path)


def lsp_warm(results: str, corpus_id: str, series: str) -> bool:
    """A warm series of lsp-session with a held run: the cache its followers measure on."""
    series_dir = os.path.join(results, corpus_id, "lsp-session", "warm", series)
    return any(read_json(path).get("held") for path in glob.glob(os.path.join(series_dir, "*.json")))


class Measure:
    """One `measure` invocation: one scenario, temperature and series of
    one binary on one corpus (plan §7)."""

    def __init__(self, args, clock, data: Data, foreign_prefix: str | None = "bsl-analyzer", window=None,
                 survivors: dict | None = None):
        self.args, self.clock, self.data = args, clock, data
        # None: other processes of that name are part of the measured load, not a reason to cancel.
        self.foreign_prefix = foreign_prefix
        # {pid: run id} of processes a run could not stop; shared by the invocations of one night.
        self.survivors = {} if survivors is None else survivors
        # window(measure, series) -> True when the next run fits before the deadline.
        self.window = window
        self.skipped = 0
        manifest = corpus.load_manifest(corpora_path(args, data))
        if args.corpus not in manifest["corpora"]:
            raise HarnessError("unknown_corpus", args.corpus)
        self.binary = load_binary(args.binary)
        self.meta = corpus.load_snapshot(data, args.corpus)
        if self.meta["commit"] != manifest["corpora"][args.corpus]["commit"]:
            raise HarnessError("snapshot_mismatch", "the manifest commit changed; re-run `snapshot`")
        self.snap = data.snapshot(args.corpus)
        self.variant = "help" if self.binary.get("has_platform_help") else "builtin"
        self.help = help_identity(self.binary, manifest["platform_help"])
        self.help_ok = True
        if self.binary.get("has_platform_help"):
            path = os.path.join(data.results(self.binary), "helpcheck.json")
            if not os.path.isfile(path):
                raise HarnessError("helpcheck_missing", "run `helpcheck` for this binary first")
            check = read_json(path)
            if check["passed"] and check["help"] != self.help:
                raise HarnessError("helpcheck_stale", "the help file changed since `helpcheck`")
            self.help_ok = check["passed"]
        points_doc, self.points_sha = corpus.load_points(data, args.corpus)
        self.points = corpus.select_points(points_doc)
        self.fingerprint = env_fingerprint(self.binary, self.meta, self.points_sha)
        self.scenario = args.scenario
        self.wsdiag = self.scenario == "lsp-workspace-diag"
        self.root = data.path("shadow", args.corpus) if self.wsdiag else self.snap
        self.results = data.results(self.binary)
        self.cancelled: list | None = None

    # -- series ------------------------------------------------------------------

    def run(self) -> None:
        clients = [None]
        if self.scenario == "mcp-load":
            clients = self.args.clients or DEFAULT_CLIENTS.get(self.args.corpus, [4])
        # Two invocations on one data dir would delete each other's caches mid-run.
        os.makedirs(self.data.root, exist_ok=True)
        with open(self.data.path("measure.lock"), "w") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise HarnessError("data_busy", "another measure holds this data dir") from None
            for count in clients:
                self.series(count)

    def series(self, clients) -> None:
        a = self.args
        sdir = self.scenario if clients is None else f"{self.scenario}-c{clients}"
        series_dir = os.path.join(self.results, a.corpus, sdir, a.temp, a.series)
        if glob.glob(os.path.join(series_dir, "*.json")):
            raise HarnessError("series_exists", f"{sdir}/{a.temp}/{a.series} already has results")
        is_mcp = self.scenario in MCP_SCENARIOS
        follower = self.scenario in LSP_FOLLOWERS
        if follower and a.temp != "warm":
            raise HarnessError("temp_unsupported", f"{self.scenario} is measured warm only")
        map_name = "mcp-memory" if is_mcp else self.scenario
        if is_mcp and a.temp == "warm":
            group = f"mcp-{a.temp}-{a.series}"
        elif follower:
            group = f"lsp-session-{a.temp}-{a.series}"
        else:
            group = f"{sdir}-{a.temp}-{a.series}"
        xdg_base = self.data.path("xdg", f"{self.binary['label']}-{self.binary['sha'][:12]}",
                                  a.corpus, group)
        cache_root = os.path.join(xdg_base, "cache")
        cachemap_path = os.path.join(self.results, "cachemap.json")
        cachemap = read_json(cachemap_path) if os.path.isfile(cachemap_path) else {}
        known = cachemap.get(a.corpus, {}).get(map_name)
        s = {"sdir": sdir, "dir": series_dir, "xdg": xdg_base, "cache": cache_root, "clients": clients}

        if not self.help_ok:
            return self.unlaunched_all(s, "help_unavailable", "helpcheck did not pass")
        needed = ("F",) if self.scenario in MCP_SCENARIOS else ("F", "Q")
        if self.scenario != "analyze" and any(key not in self.points for key in needed):
            return self.unlaunched_all(s, "precondition_missing", f"point {' or '.join(needed)} is missing")

        if a.temp == "warm":
            if self.scenario in ("mcp-start", "mcp-load", *LSP_FOLLOWERS):
                # The reset creates the cache root before the first run, so a follower
                # needs a held lsp-session run, not just the directory.
                if follower and not lsp_warm(self.results, a.corpus, a.series):
                    raise HarnessError("group_missing", "measure warm lsp-session in this series first")
                if not os.path.isdir(cache_root):
                    raise HarnessError("group_missing", "measure warm mcp-memory in this series first")
                for k in range(1, a.runs + 1):
                    self.finish(s, f"r{k}", self.run_one(s, f"r{k}"), k)
                return
            self.reset(s)
            first = self.run_one(s, "first")
            created = first[1]
            if created is None:
                for k in range(1, a.runs + 1):
                    self.finish(s, f"r{k}", first if k == 1 else self.run_one(s, f"r{k}"), k)
                return
            warmup = False
            if first[0]["held"]:
                cachemap.setdefault(a.corpus, {})[map_name] = {**created, "series": a.series}
                write_json(cachemap_path, cachemap)
                warmup = bool(created["cache"] or created["snapshot"])
            else:
                # Only a run that held proves what this version creates; cold stays refused.
                first[0]["errors"].append("cachemap_not_taken: the first run did not hold")
            if warmup:
                self.finish(s, "warmup", first, None)
                start = 1
            else:
                self.finish(s, "r1", first, 1)
                start = 2
            for k in range(start, a.runs + 1):
                self.finish(s, f"r{k}", self.run_one(s, f"r{k}"), k)
            return

        if known is None:
            raise HarnessError("cachemap_missing", "measure warm first: cold needs the cache map")
        if not known["cache"] and not known["snapshot"]:
            print(f"scenario={sdir} corpus={a.corpus} temp=cold run=- held=- calls_ok=- reason=no_cache")
            return
        self.reset(s)
        first = self.run_one(s, "first")
        created = first[1]
        if created is None:
            for k in range(1, a.runs + 1):
                self.finish(s, f"r{k}", first if k == 1 else self.run_one(s, f"r{k}"), k)
            return
        check = {
            "missing": sorted(set(known["cache"]) - set(created["cache"]))
            + sorted(set(known["snapshot"]) - set(created["snapshot"])),
            "extra": sorted(set(created["cache"]) - set(known["cache"]))
            + sorted(set(created["snapshot"]) - set(known["snapshot"])),
        }
        first[0]["cachemap_check"] = check
        self.finish(s, "r1", first, 1)
        for k in range(2, a.runs + 1):
            if not self.foreign_now():
                paths = [os.path.join(cache_root, rel) for rel in known["cache"]] \
                    + [os.path.join(self.root, rel) for rel in known["snapshot"]]
                for path in paths:
                    _remove(path)
                if any(os.path.lexists(path) for path in paths):
                    raise HarnessError("cold_reset_failed", "a cache map path survived its removal")
            self.finish(s, f"r{k}", self.run_one(s, f"r{k}"), k)

    def foreign_now(self) -> bool:
        """Cancel the rest of the invocation once a foreign process shows up;
        checked before anything is deleted, not only before a launch."""
        if self.cancelled is None and self.foreign_prefix is not None:
            found = procs.foreign(prefix=self.foreign_prefix)
            if found:
                self.cancelled = found
        return self.cancelled is not None

    def reset(self, s: dict) -> None:
        """Start of a warm group or cold series: empty cache, no `.build`."""
        if self.foreign_now():
            return
        shutil.rmtree(s["xdg"], ignore_errors=True)
        os.makedirs(s["cache"], exist_ok=True)
        if self.wsdiag:
            corpus.prepare_shadow(self.snap, self.root, self.meta, self.variant)
        for root in {self.snap, self.root}:
            _remove(os.path.join(root, ".build"))
        # The warm and cold methods both rest on this start state; check it as cold
        # checks its map paths, so a failed removal cannot pass for a fresh cache.
        if os.listdir(s["cache"]) or any(os.path.lexists(os.path.join(root, ".build"))
                                         for root in {self.snap, self.root}):
            raise HarnessError("reset_failed", "the cache root or .build survived the reset")

    def unlaunched_all(self, s: dict, reason: str, detail: str) -> None:
        for k in range(1, self.args.runs + 1):
            record = self.record(s, None, {"held": False, "reason": reason}, [f"{reason}: {detail}"])
            self.finish(s, f"r{k}", (record, None), k)

    # -- one run ---------------------------------------------------------------------

    def run_one(self, s: dict, name: str):
        if self.foreign_now():
            return self.cancelled_record(s), None
        left = self.surviving()
        if left:
            # They hold the group's cache and CPU, so a run beside them is not measured.
            return self.record(s, None, {"held": False, "reason": "survivor_running"},
                               [f"survivor_running: {', '.join(map(str, left))} of an earlier run"]), None
        if self.window is not None and not self.window(self, s):
            self.skipped += 1
            return self.record(s, None, {"held": False, "reason": "window_exhausted"},
                               ["window_exhausted: the run would end after the deadline"]), None
        began = self.clock.now()
        if self.scenario != "analyze":
            corpus.verify_points(self.snap, self.points)
        corpus.prepare_snapshot(self.snap, self.meta, self.variant)
        if self.wsdiag:
            corpus.prepare_shadow(self.snap, self.root, self.meta, self.variant)
        run_id = f"{self.binary['label']}-{self.binary['sha'][:12]}-{self.args.corpus}-{s['sdir']}-" \
                 f"{self.args.temp}-{self.args.series}-{name}-{uuid.uuid4().hex[:8]}"
        run_dir = os.path.join(s["dir"], name)
        shutil.rmtree(run_dir, ignore_errors=True)
        os.makedirs(run_dir)
        own = os.path.join(s["xdg"], "runs", run_id)
        env = procs.child_env(run_id, xdg_dirs(own, s["cache"]))
        a = self.args
        ballasts, after_ready, relay = [], None, None
        if getattr(a, "ballast_mb", None):
            ballasts.append(start_ballast(env, a.ballast_mb))
        if getattr(a, "ballast_after_ready_mb", None):
            def after_ready():
                ballasts.append(start_ballast(env, a.ballast_after_ready_mb))
        if getattr(a, "inject_latency_ms", None):
            relay = [sys.executable, RELAY, "--delay-ms", str(a.inject_latency_ms)]
        before = workspace_untracked(self.root)
        load_before = procs.machine_load()
        started_at = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        ctx = scenarios.Ctx(binary=self.binary["path"], root=self.root, snap=self.snap, env=env,
                            run_dir=run_dir, run_id=run_id, clock=self.clock, points=self.points,
                            snap_meta=self.meta, clients=s["clients"] or 1, relay=relay,
                            after_ready=after_ready)
        out = scenarios.run_scenario(self.scenario, ctx)
        final = procs.stop(run_id, self.clock)
        for proc in ballasts:
            proc.wait()
            proc.stdout.close()
        record = self.record(s, run_id, out, out["errors"])
        record.update(started_at=started_at, load_before=load_before, load_after=procs.machine_load(),
                      stop={"first": out.get("stop"), "final": final},
                      controls=controls(a, ctx.applied | ({"ballast_mb"} if getattr(a, "ballast_mb", None) else set())),
                      duration_s=self.clock.now() - began)
        if final["left"]:
            self.survivors.update(dict.fromkeys(final["left"], run_id))
            record.update(held=False, reason="stop_incomplete")
            record["errors"].append(f"stop_incomplete: {len(final['left'])} tracked process(es) survive")
        found = procs.foreign(prefix=self.foreign_prefix) if self.foreign_prefix is not None else []
        if found:
            self.cancelled = found
            record.update(held=False, reason="foreign_process")
            record["errors"].append("foreign_process: " + ", ".join(f"{p['pid']} {p['name']}" for p in found))
        shutil.rmtree(own, ignore_errors=True)
        created = {"cache": cache_leaves(s["cache"]),
                   "snapshot": sorted(workspace_untracked(self.root) - before)}
        return record, created

    def surviving(self) -> list[int]:
        for pid, run_id in list(self.survivors.items()):
            if pid not in procs.tracked(run_id):
                del self.survivors[pid]
        return sorted(self.survivors)

    def cancelled_record(self, s: dict) -> dict:
        pids = ", ".join(f"{p['pid']} {p['name']}" for p in self.cancelled)
        return self.record(s, None, {"held": False, "reason": "foreign_process"},
                           [f"foreign_process: {pids}"])

    def record(self, s: dict, run_id, out: dict, errors: list) -> dict:
        config_sha = self.meta["variants"][self.variant]["config_sha256"]
        return {
            "schema": HARNESS_SCHEMA,
            "run_id": run_id,
            "scenario": self.scenario,
            "clients": s["clients"],
            "series": self.args.series,
            "runs": self.args.runs,
            "binary": {"label": self.binary["label"], "sha": self.binary["sha"],
                       "sha256": self.binary["sha256"]},
            "corpus": {"id": self.args.corpus, "commit": self.meta["commit"]},
            "points_sha256": self.points_sha,
            "temp": self.args.temp,
            "started_at": None,
            "metrics": out.get("metrics", {}),
            "calls": out.get("calls", []),
            "errors": list(errors),
            "env_fingerprint": self.fingerprint,
            "help": self.help,
            "config_sha256": config_sha,
            "held": out["held"],
            "reason": out["reason"],
            "exit_code": out.get("exit_code"),
            "shutdown_timeout": out.get("shutdown_timeout"),
        }

    def finish(self, s: dict, name: str, result, k) -> None:
        record, _ = result
        record["k"] = k
        record["warmup"] = name == "warmup"
        first_dir = os.path.join(s["dir"], "first")
        if os.path.isdir(first_dir) and name != "first":
            shutil.rmtree(os.path.join(s["dir"], name), ignore_errors=True)
            os.replace(first_dir, os.path.join(s["dir"], name))
        write_json(os.path.join(s["dir"], f"{name}.json"), record)
        measured = [c for c in record["calls"] if not c.get("na")]
        ok = sum(1 for c in measured if c.get("ok"))
        print(f"scenario={s['sdir']} corpus={self.args.corpus} temp={self.args.temp} "
              f"run={k if k is not None else 'warmup'} held={str(record['held']).lower()} "
              f"calls_ok={ok}/{len(measured)} reason={record['reason'] or '-'}", flush=True)
        for cycle in record["metrics"].get("cycles") or []:
            print(f"  cycle={cycle['cycle']} open_s={_fmt(cycle['open_s'])} "
                  f"global_s={_fmt(cycle['global_s'])} rss_closed_kb={cycle['rss_closed_kb']} growth_kb={cycle['growth_kb']}", flush=True)


RELAY = os.path.join(os.path.dirname(os.path.abspath(__file__)), "relay.py")
# Allocates and touches the given MiB, says so, then lives until the stop.
BALLAST_CODE = (
    "import signal, sys\n"
    "size = int(sys.argv[1]) << 20\n"
    "block = bytearray(size)\n"
    "for i in range(0, size, 4096):\n"
    "    block[i] = 1\n"
    "print(flush=True)\n"
    "signal.pause()\n"
)


def start_ballast(env: dict, mib: int):
    """A process of the run (it carries the run marker) holding `mib` MiB
    resident: the positive control of the memory metrics (plan §13, slice 2)."""
    proc = subprocess.Popen([sys.executable, "-c", BALLAST_CODE, str(mib)], env=env,
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
    if not proc.stdout.readline():
        raise HarnessError("ballast_failed", f"the ballast exited with {proc.wait()}")
    return proc


def controls(args, applied: set) -> dict | None:
    """The positive controls that took effect in a run, not merely requested."""
    found = {name: getattr(args, name) for name in sorted(applied)}
    return found or None


def cmd_measure(args, clock=None) -> int:
    data = Data(args.data_dir)
    Measure(args, clock or RealClock(), data).run()
    return 0


# -- report ---------------------------------------------------------------------


def error_class(text) -> str:
    """A short, corpus-free label of a call error for stdout."""
    if text is None:
        return "-"
    text = str(text)
    if text in LOAD_ERROR_CLASSES:
        return text
    if text.startswith("timeout"):
        return "timeout"
    for marker in ("still building", "not ready", "unavailable"):
        if marker in text:
            return marker
    match = re.search(r'"(state|status)"\s*:\s*"(\w+)"', text)
    if match:
        return f"{match.group(1)}={match.group(2)}"
    match = re.match(r"(-?\d+): ", text)
    if match:
        return f"rpc {match.group(1)}"
    return "isError"


def side(records: list, call_id: str | None):
    """valid / invalid / failed / n/a of one metric over a series (plan §8.6)."""
    if not records:
        return "missing"
    if any(not r["held"] for r in records):
        if any(r["reason"] in MISSING_REASONS for r in records):
            return "missing"
        return "failed"
    if call_id is None:
        return "valid"
    calls = [next((c for c in r["calls"] if c["id"] == call_id), None) for r in records]
    if any(c is None or c.get("na") for c in calls):
        return "n/a"
    return "valid" if all(c.get("ok") for c in calls) else "invalid"


def _number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _fmt(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.3f}" if value < 100 else f"{value:.1f}"
    return str(value)


def cmd_report(args) -> int:
    data = Data(args.data_dir)
    pattern = os.path.join(data.path("results"), "*", args.corpus, "*", "*", args.series or "*", "r*.json")
    groups: dict = {}
    binaries = []
    for path in sorted(glob.glob(pattern)):
        record = read_json(path)
        parts = path.split(os.sep)
        sdir, temp = parts[-4], parts[-3]
        label = f"{record['binary']['label']}-{record['binary']['sha'][:12]}"
        if label not in binaries:
            binaries.append(label)
        groups.setdefault((sdir, temp), {}).setdefault(label, []).append(record)

    rows, both_invalid, na_count = [], [], {b: 0 for b in binaries}
    for (sdir, temp), by_binary in sorted(groups.items()):
        run_metrics, call_ids = [], []
        for records in by_binary.values():
            for record in records:
                for key, value in record["metrics"].items():
                    if _number(value) and key not in run_metrics:
                        run_metrics.append(key)
                for call in record["calls"]:
                    if call["id"] not in call_ids:
                        call_ids.append(call["id"])
        for key in run_metrics:
            cells = []
            for b in binaries:
                records = by_binary.get(b, [])
                state = side(records, None)
                held = [r["metrics"].get(key) for r in records if r["held"]]
                cells.append(_fmt(median(held)) if state == "valid" else state)
            rows.append((sdir, temp, key, cells))
        for call_id in call_ids:
            states = {b: side(by_binary.get(b, []), call_id) for b in binaries}
            for b, state in states.items():
                if state == "n/a":
                    na_count[b] += 1
            fields = [f for f in ("first_s", "repeat_s", "p50_s", "p95_s", "count", "errors", "timeouts")
                      if any(_number(c.get(f)) for rs in by_binary.values() for r in rs
                             for c in r["calls"] if c["id"] == call_id)] or ["first_s"]
            for f in fields:
                cells = []
                for b in binaries:
                    state = states[b]
                    if state != "valid" and not (f in ("errors", "timeouts", "p50_s", "p95_s")
                                                 and state == "invalid"):
                        cells.append(state)
                        continue
                    values = [c.get(f) for r in by_binary.get(b, []) for c in r["calls"]
                              if c["id"] == call_id]
                    cells.append(_fmt(sum(values) if f in ("errors", "timeouts") else median(values)))
                rows.append((sdir, temp, f"{call_id}.{f}", cells))
            if len(binaries) > 1 and all(states[b] in ("invalid", "failed") for b in binaries):
                errors = {}
                for b in binaries:
                    texts = [c.get("error") for r in by_binary.get(b, []) for c in r["calls"]
                             if c["id"] == call_id and not c.get("ok")]
                    errors[b] = texts[0] if texts else next(
                        (r["reason"] for r in by_binary.get(b, []) if not r["held"]), None)
                both_invalid.append((sdir, temp, call_id, errors))

    header = "| scenario | temp | metric | " + " | ".join(binaries) + " |"
    lines = [header, "|" + "---|" * (3 + len(binaries))]
    lines += [f"| {sdir} | {temp} | {key} | " + " | ".join(cells) + " |" for sdir, temp, key, cells in rows]
    na_line = "n/a: " + ", ".join(f"{b}={na_count[b]}" for b in binaries)
    print("\n".join(lines))
    print(na_line)
    print("both invalid: " + ("; ".join(
        f"{sdir}/{temp}/{call}: " + ", ".join(f"{b}={error_class(e)}" for b, e in errs.items())
        for sdir, temp, call, errs in both_invalid) or "none"))
    for b in binaries:
        path = os.path.join(data.path("results"), b, "cachemap.json")
        cmap = read_json(path).get(args.corpus, {}) if os.path.isfile(path) else {}
        for scenario, entry in sorted(cmap.items()):
            shown = [re.sub(r"(workspaces/[^/]+/)[^/]+$", r"\1<digest>", p) for p in entry["cache"]]
            print(f"cachemap {b} {scenario}: cache=[{', '.join(shown)}] snapshot=[{', '.join(entry['snapshot'])}]")

    report = [f"# Срез замеров: {args.corpus}", "", *lines, "", na_line, "", "## Оба невалидны", ""]
    report += [f"- {sdir} / {temp} / {call}: " + "; ".join(f"{b}: {e}" for b, e in errs.items())
               for sdir, temp, call, errs in both_invalid] or ["- нет"]
    out_path = data.path("reports", f"{args.corpus}.md")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(report) + "\n")
    return 0


# -- nightly set, calibration, night (night.py) ------------------------------------


def _session(args, clock, data: Data):
    import night
    manifest = corpus.load_manifest(corpora_path(args, data))
    corpora = [c for c in args.corpus or manifest["corpora"]
               if any(c in members for _, members in night.SUITE)]
    window = night.Window(clock, night.seconds_until(args.deadline, datetime.datetime.now())
                          if args.deadline else None)
    estimates = night.Estimates(night.previous_history(data, datetime.date.today().isoformat()))
    session = night.Session(args, clock, data, corpora, window, estimates)
    binary = load_binary(args.binary)
    night.ensure_help(data, binary, manifest["platform_help"], clock)
    return night, session, binary, corpora


def cmd_suite(args, clock=None) -> int:
    """The nightly set of one binary as one series (bases of past releases)."""
    data = Data(args.data_dir)
    night, session, binary, _ = _session(args, clock or RealClock(), data)
    session.suite([("base", binary["path"])], args.series)
    print(f"suite binary={binary['label']} series={args.series} skipped={len(session.skipped)} "
          f"refused={len(session.refused)}")
    return 0


def cmd_calibrate(args, clock=None) -> int:
    data = Data(args.data_dir)
    if args.temp != "warm":
        raise HarnessError("calibration_temp", "the nightly set and its calibration are warm")
    if args.corpus:
        raise HarnessError("calibration_scope", "noise.json covers the whole set; calibrate without --corpus")
    night, session, binary, corpora = _session(args, clock or RealClock(), data)
    prints = night.fingerprints(data, binary, corpora)
    result = night.calibrate(session, binary, corpora, prints, args.series)
    print(f"calibrate binary={binary['label']} measured={','.join(result['measured']) or '-'} "
          f"complete={','.join(result['complete']) or '-'} noise_written={str(result['noise_written']).lower()}")
    if result["noise_written"]:
        check = read_json(data.path("noise.json"))["self_check"]
        print(f"noise self_check checked={check['checked']} violations={len(check['violations'])}")
    return 0


def cmd_night(args, clock=None) -> int:
    import night
    summary = night.run_night(args, clock or RealClock())
    print(night.summary_line(summary))
    return 0


def cmd_compare(args) -> int:
    import verdict
    data = Data(args.data_dir)
    root = data.path("results")
    base_dir, base_series = verdict.resolve_side(root, args.base)
    cand_dir, cand_series = verdict.resolve_side(root, args.candidate)
    corpora = set(args.corpus) if args.corpus else None
    base_groups = verdict.load_series(base_dir, base_series, corpora)
    cand_groups = verdict.load_series(cand_dir, cand_series, corpora)
    noise_path = data.path("noise.json")
    noise = read_json(noise_path) if os.path.isfile(noise_path) else None
    prints = {key[0]: records[0].get("env_fingerprint") for key, records in base_groups.items() if records}
    stale = verdict.noise_stale(noise, prints)
    rows = verdict.compare(base_groups, cand_groups, noise, stale)
    result = verdict.night_verdict(rows, stale=stale)
    shown = rows if args.all else [r for r in rows if r["outcome"] not in ("ok", "n/a")]
    print("\n".join(verdict.ROW_HEADER + [verdict.row_line(r) for r in shown]))
    print(f"compare base={args.base} candidate={args.candidate} verdict={result['verdict']} "
          f"reasons={'; '.join(result['reasons']) or '-'} counts=" +
          ",".join(f"{k}:{v}" for k, v in result["counts"].items()))
    if args.trend:
        extra = [] if verdict.version_key(os.path.basename(cand_dir).rsplit("-", 1)[0]) else \
            [(os.path.basename(cand_dir), cand_dir, cand_series)]
        print("\n".join(verdict.trend_lines(verdict.trend(root, cand_groups, extra))))
    if args.out:
        write_json(args.out, {"rows": rows, **result})
    return 0


def parser() -> argparse.ArgumentParser:
    top = argparse.ArgumentParser(prog="perf_nightly.py")
    sub = top.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", help="build bsl-analyzer-app at a ref into <DATA>/bin")
    build.add_argument("--data-dir", required=True)
    build.add_argument("--ref", required=True)
    build.add_argument("--label")
    build.add_argument("--remote", help="URL the build clone fetches from (default: origin)")
    build.set_defaults(func=cmd_build)

    def common(p, binary=False, corpus_arg=False):
        p.add_argument("--data-dir", required=True)
        p.add_argument("--corpora", help="corpus manifest (default: <DATA>/corpora.toml)")
        if binary:
            p.add_argument("--binary", required=True)
        if corpus_arg:
            p.add_argument("--corpus", required=True)

    snapshot = sub.add_parser("snapshot", help="clone a corpus at its manifest commit")
    common(snapshot, corpus_arg=True)
    snapshot.set_defaults(func=cmd_snapshot)

    points = sub.add_parser("points", help="discover call points on a snapshot")
    common(points, binary=True, corpus_arg=True)
    points.set_defaults(func=cmd_points)

    helpcheck = sub.add_parser("helpcheck", help="check that a binary loads the configured help")
    common(helpcheck, binary=True)
    helpcheck.add_argument("--help-override", help="TOML file whose [platform_help] replaces the manifest's")
    helpcheck.set_defaults(func=cmd_helpcheck)

    measure = sub.add_parser("measure", help="measure one scenario series")
    common(measure, binary=True, corpus_arg=True)
    measure.add_argument("--scenario", required=True, choices=sorted(scenarios.SCENARIOS))
    measure.add_argument("--temp", required=True, choices=("cold", "warm"))
    measure.add_argument("--series", required=True)
    measure.add_argument("--runs", type=int, default=3)
    measure.add_argument("--clients", type=lambda v: [int(x) for x in v.split(",")],
                         help="mcp-load client counts, e.g. 2,4")
    measure.add_argument("--ballast-mb", type=int, help="control: a run process holding this many MiB")
    measure.add_argument("--ballast-after-ready-mb", type=int,
                         help="control: the same ballast, allocated once the server is ready")
    measure.add_argument("--inject-latency-ms", type=int,
                         help="control: a relay delays every server message by this many ms")
    measure.set_defaults(func=cmd_measure)

    def windowed(p):
        p.add_argument("--deadline", help="HH:MM local time the last run must end by")
        p.add_argument("--corpus", action="append", help="limit the set to these corpora")

    suite = sub.add_parser("suite", help="measure the nightly set of one binary as one series")
    common(suite, binary=True)
    suite.add_argument("--series", required=True)
    windowed(suite)
    suite.set_defaults(func=cmd_suite)

    calibrate = sub.add_parser("calibrate", help="measure calibration series and write noise.json")
    common(calibrate, binary=True)
    calibrate.add_argument("--temp", default="warm", choices=("warm",))
    calibrate.add_argument("--series", type=int, default=5, help="how many new cal-<i> series to measure")
    windowed(calibrate)
    calibrate.set_defaults(func=cmd_calibrate)

    night = sub.add_parser("night", help="the nightly cycle: build, measure, judge")
    common(night)
    night.add_argument("--deadline", default="08:45", help="HH:MM local time the last run must end by")
    night.add_argument("--candidate", help="a release candidate tag; results go to gates/<tag>/")
    night.add_argument("--date", help="night key YYYY-MM-DD (default: today)")
    night.add_argument("--remote", help="URL the build clone fetches from (default: origin)")
    night.set_defaults(func=cmd_night)

    compare = sub.add_parser("compare", help="verdict of a candidate series against a base series")
    compare.add_argument("--data-dir", required=True)
    compare.add_argument("--base", required=True, help="<label>[-<sha12>]/<series>")
    compare.add_argument("--candidate", required=True, help="<label>[-<sha12>]/<series>")
    compare.add_argument("--corpus", action="append")
    compare.add_argument("--all", action="store_true", help="print every metric, not only flagged ones")
    compare.add_argument("--trend", action="store_true", help="print the trend table too")
    compare.add_argument("--out", help="write the rows and the verdict as JSON")
    compare.set_defaults(func=cmd_compare)

    report = sub.add_parser("report", help="median table of a corpus over all measured binaries")
    report.add_argument("--data-dir", required=True)
    report.add_argument("--corpus", required=True)
    report.add_argument("--series")
    report.set_defaults(func=cmd_report)
    return top


def main(argv=None) -> int:
    args = parser().parse_args(argv)
    try:
        return args.func(args)
    except HarnessError as exc:
        print(f"perf_nightly: refused: {exc}", file=sys.stderr)
        return 2
    except (tomllib.TOMLDecodeError, json.JSONDecodeError, OSError) as exc:
        print(f"perf_nightly: refused: input_invalid: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
