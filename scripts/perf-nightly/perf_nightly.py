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
    HARNESS_SCHEMA, Data, HarnessError, git, median, read_json, repo_root, sha256_bytes,
    sha256_file, sha256_tree, write_json,
)

TAG_RE = re.compile(r"v\d+\.\d+\.\d+")


def cmd_build(args) -> int:
    data = Data(args.data_dir)
    clone = data.path("build")
    if not os.path.isdir(os.path.join(clone, ".git")):
        origin = args.remote or git("remote", "get-url", "origin", cwd=repo_root())
        git("clone", "--no-checkout", repo_root(), clone)
        git("remote", "set-url", "origin", origin, cwd=clone)
    git("fetch", "--tags", "origin", cwd=clone)
    ref = "origin/develop" if args.ref == "develop" else args.ref
    sha = git("rev-parse", "--verify", f"{ref}^{{commit}}", cwd=clone)
    if args.label:
        label = args.label
    elif TAG_RE.fullmatch(args.ref):
        label = args.ref
    elif args.ref in ("develop", "origin/develop"):
        label = "develop"
    else:
        raise HarnessError("label_required", "pass --label for a ref that is neither a tag nor develop")
    rustc = subprocess.run(["rustc", "-V"], cwd=clone, capture_output=True, text=True,
                           check=True).stdout.strip()
    bin_dir = data.path("bin", f"{label}-{sha[:12]}-{sha256_bytes(rustc.encode())[:12]}")
    binary = os.path.join(bin_dir, "bsl-analyzer-app")
    if os.path.isfile(os.path.join(bin_dir, "build.json")):
        print(f"build={label} sha={sha[:12]} cached=true")
        print(binary)
        return 0
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
    print(f"build={label} sha={sha[:12]} cached=false")
    print(binary)
    return 0


MCP_SCENARIOS = ("mcp-start", "mcp-memory", "mcp-load")
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
        "mem_total": proc_field("/proc/meminfo", "MemTotal"),
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


class Measure:
    """One `measure` invocation: one scenario, temperature and series of
    one binary on one corpus (plan §7)."""

    def __init__(self, args, clock, data: Data, foreign_prefix: str = "bsl-analyzer"):
        self.args, self.clock, self.data = args, clock, data
        self.foreign_prefix = foreign_prefix
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
        map_name = "mcp-memory" if is_mcp else self.scenario
        group = f"mcp-{a.temp}-{a.series}" if is_mcp and a.temp == "warm" else f"{sdir}-{a.temp}-{a.series}"
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
            if self.scenario in ("mcp-start", "mcp-load"):
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
        if self.cancelled is None:
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
        before = workspace_untracked(self.root)
        load_before = procs.machine_load()
        started_at = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        ctx = scenarios.Ctx(binary=self.binary["path"], root=self.root, snap=self.snap, env=env,
                            run_dir=run_dir, run_id=run_id, clock=self.clock, points=self.points,
                            snap_meta=self.meta, clients=s["clients"] or 1)
        out = scenarios.run_scenario(self.scenario, ctx)
        final = procs.stop(run_id, self.clock)
        record = self.record(s, run_id, out, out["errors"])
        record.update(started_at=started_at, load_before=load_before, load_after=procs.machine_load(),
                      stop={"first": out.get("stop"), "final": final})
        if final["left"]:
            record.update(held=False, reason="stop_incomplete")
            record["errors"].append(f"stop_incomplete: {len(final['left'])} tracked process(es) survive")
        found = procs.foreign(prefix=self.foreign_prefix)
        if found:
            self.cancelled = found
            record.update(held=False, reason="foreign_process")
            record["errors"].append("foreign_process: " + ", ".join(f"{p['pid']} {p['name']}" for p in found))
        shutil.rmtree(own, ignore_errors=True)
        created = {"cache": cache_leaves(s["cache"]),
                   "snapshot": sorted(workspace_untracked(self.root) - before)}
        return record, created

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
        if any(r["reason"] in ("foreign_process", "window_exhausted") for r in records):
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
    measure.set_defaults(func=cmd_measure)

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
