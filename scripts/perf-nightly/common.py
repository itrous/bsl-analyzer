"""Shared helpers: data-dir layout, hashing, JSON files, medians."""

from __future__ import annotations

import hashlib
import json
import os
import statistics
import subprocess

# Grows whenever the way a metric is measured changes (plan §8.5).
HARNESS_SCHEMA = 1
HERE = os.path.dirname(os.path.abspath(__file__))


class HarnessError(Exception):
    """A refusal with a machine-readable reason (`snapshot_mismatch`, …)."""

    def __init__(self, reason: str, detail: str = ""):
        super().__init__(f"{reason}: {detail}" if detail else reason)
        self.reason = reason
        self.detail = detail


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_tree(path: str) -> str:
    """sha256 of a file, or of a directory as sorted relative paths and bytes."""
    if os.path.isfile(path):
        return sha256_file(path)
    digest = hashlib.sha256()
    for dirpath, dirnames, filenames in os.walk(path):
        dirnames.sort()
        for name in sorted(filenames):
            full = os.path.join(dirpath, name)
            digest.update(os.path.relpath(full, path).encode() + b"\0")
            digest.update(sha256_file(full).encode() + b"\0")
    return digest.hexdigest()


def read_json(path: str):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path: str, value) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(value, fh, ensure_ascii=False, indent=2, sort_keys=True)
        fh.write("\n")
    os.replace(tmp, path)


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def git(*args, cwd: str | None = None, check: bool = True) -> str:
    result = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise HarnessError("git_failed", f"git {args[0]}: {result.stderr.strip()}")
    # Only trailing whitespace: porcelain status lines start with a space.
    return result.stdout.rstrip()


def repo_root() -> str:
    return git("rev-parse", "--show-toplevel", cwd=HERE)


def _common_dir(path: str) -> str | None:
    result = subprocess.run(["git", "-C", path, "rev-parse", "--path-format=absolute",
                             "--git-common-dir"], capture_output=True, text=True)
    return os.path.realpath(result.stdout.strip()) if result.returncode == 0 else None


class Data:
    """Layout of the data directory (plan §6.1); never inside this repository."""

    def __init__(self, root: str):
        self.root = os.path.abspath(root)
        probe = self.root
        while not os.path.isdir(probe):
            probe = os.path.dirname(probe)
        harness = _common_dir(HERE)
        if harness is not None and _common_dir(probe) == harness:
            raise HarnessError("data_dir_in_repo",
                               "the data dir lies inside a work tree of the harness repository")

    def path(self, *parts: str) -> str:
        return os.path.join(self.root, *parts)

    def snapshot(self, corpus_id: str) -> str:
        return self.path("snapshots", corpus_id)

    def snapshot_meta(self, corpus_id: str) -> str:
        return self.path("snapshots", f"{corpus_id}.json")

    def points(self, corpus_id: str) -> str:
        return self.path("points", f"{corpus_id}.json")

    def results(self, binary: dict) -> str:
        return self.path("results", f"{binary['label']}-{binary['sha'][:12]}")
