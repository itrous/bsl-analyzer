#!/usr/bin/env python3
"""Check the CI test wrappers preserve failures and reject filters that select nothing,
that every Windows job routes each `cargo test` through one, that the legs of the
`check` matrix together cover the workspace once, and that the test-only seams reach a
build through dev-dependencies alone."""

from pathlib import Path
import fnmatch
import json
import os
import re
import shlex
import signal
import string
import subprocess
import textwrap
import threading


root = Path(__file__).resolve().parents[1]
text = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
wrappers = re.findall(
    r"^          (?:run_tests|list_tests|run_nonempty)\(\) \{\n.*?^          \}",
    text,
    re.MULTILINE | re.DOTALL,
)
# Every wrapper must be checked: one whose spelling drifts from the pattern above
# would otherwise drop out of the check silently.
defined = re.findall(r"^\s*(?:function\s+)?(?:run|list)_\w+\s*\(\s*\)\s*\{", text, re.MULTILINE)
assert len(wrappers) == len(defined), f"checked {len(wrappers)} of {len(defined)} CI wrappers"


def name_of(block):
    return block.strip().split("(", 1)[0]


def blocks(name):
    return [block for block in wrappers if name_of(block) == name]


assert blocks("run_tests"), "no run_tests wrapper in the workflow"
assert blocks("list_tests"), "no list_tests wrapper in the workflow"


def call(block, cargo_body, invocation):
    return subprocess.run(
        ["bash", "-c", "set -euo pipefail\n"
         + f"cargo() {{ {cargo_body}; }}\n"
         + textwrap.dedent(block) + f"\n{invocation}"],
        capture_output=True, text=True, check=False,
    )


for block in blocks("run_nonempty"):
    for passed, status in [(1, 0), (0, 0), (1, 101)]:
        output = f"test result: ok. {passed} passed; diagnostic marker"
        result = call(block, f"echo '{output}' >&2; return {status}", "run_nonempty fixture test_name")
        assert (result.returncode == 0) == (passed > 0 and status == 0), result
        assert output in result.stdout, result

passed_line = "test fixture::name ... ok"
for block in blocks("run_tests"):
    def run_tests(output, status, invocation):
        return call(block, f"echo '{output}' >&2; return {status}", invocation)

    result = run_tests(passed_line, 0, "run_tests --exact -p x -- fixture::name")
    assert result.returncode == 0, result
    # A filter that matched nothing names itself, so a renamed test is found in the log.
    result = run_tests(passed_line, 0, "run_tests --exact -p x -- fixture::other")
    assert result.returncode != 0 and "::error::fixture::other ran no test" in result.stdout, result
    assert run_tests(passed_line, 0, "run_tests -p x -- name").returncode == 0
    assert run_tests(passed_line, 0, "run_tests --exact -p x -- name").returncode != 0
    result = run_tests("test fixture::name - should panic ... ok", 0, "run_tests --exact -p x -- fixture::name")
    assert result.returncode == 0, result
    result = run_tests(passed_line, 101, "run_tests --exact -p x -- fixture::name")
    assert result.returncode == 101 and passed_line in result.stdout, result
    assert run_tests("test result: ok. 0 passed;", 0, "run_tests -p x").returncode != 0
    assert run_tests("test a ... ok", 0, "run_tests -p x").returncode == 0
    # Whatever the outcome, the log names the command before its output and its exit after it.
    for status in (0, 101):
        lines = run_tests(passed_line, status, "run_tests --exact -p x -- fixture::name").stdout.splitlines()
        started = [i for i, line in enumerate(lines) if line.startswith(">>> cargo test -p x -- ")]
        finished = [i for i, line in enumerate(lines) if line == f"<<< cargo test -p x: exit {status}"]
        assert len(started) == 1 and len(finished) == 1, lines
        assert started[0] < lines.index(passed_line) < finished[0], lines
    # A step the runner cancels is killed before its command exits; what the command
    # already printed must have reached the log by then, or a hung step leaves no trace
    # of where it hung. The stub prints and then blocks on stdin, which stays open, so
    # the kill comes only once the line has been seen: a wrapper that holds the output
    # back until cargo exits leaves this check hanging, and the job timeout ends it.
    stalled = subprocess.Popen(
        ["bash", "-c", "set -euo pipefail\n"
         + f"cargo() {{ echo '{passed_line}'; read -r _; }}\n"
         + textwrap.dedent(block) + "\nrun_tests --exact -p x -- fixture::name"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, start_new_session=True,
    )
    # A wrapper that holds the output back never prints the line; the guard turns that
    # into a failure that says so instead of a lint job held to its timeout.
    guard = threading.Timer(600, os.killpg, (stalled.pid, signal.SIGKILL))
    guard.start()
    seen = []
    for line in stalled.stdout:
        seen.append(line.rstrip("\n"))
        if seen[-1] == passed_line:
            break
    guard.cancel()
    os.killpg(stalled.pid, signal.SIGKILL)
    seen += stalled.stdout.read().splitlines()
    assert stalled.wait() == -signal.SIGKILL, seen
    assert passed_line in seen, f"run_tests held cargo's output until cargo exited: {seen}"
    assert any(line.startswith(">>> cargo test -p x") for line in seen), seen
    assert not any(line.startswith("<<<") for line in seen), seen

# The listing stub answers 99 to anything but a listing: a gate that ran tests would
# show up as that code.
list_stub = (
    'case " $* " in *" --list "*) ;; *) return 99 ;; esac; '
    "echo '     Running unittests src/lib.rs (target/debug/deps/fixture-0123abcd)'; "
    "echo 'fixture::muted: test'; "
    'case " $* " in *" --ignored "*) return 0 ;; esac; '
    "echo 'fixture::kept: test'; "
    "echo '     Running unittests src/lib.rs (target/debug/deps/other-4567ef01)'; "
    "echo 'other::name: test'"
)
for block in blocks("list_tests"):
    def list_tests(invocation, stub=list_stub):
        result = call(block, stub, invocation)
        assert result.returncode != 99, result
        return result.returncode

    assert list_tests("list_tests --exact fixture --workspace --lib -- fixture::kept") == 0
    assert list_tests("list_tests --exact fixture --workspace --lib -- fixture::muted") != 0
    assert list_tests("list_tests --exact fixture --workspace --lib -- other::name") != 0
    assert list_tests("list_tests --exact fixture --workspace --lib -- fixture::gone") != 0
    assert list_tests("list_tests fixture --workspace --lib -- kept") == 0
    assert list_tests("list_tests fixture --workspace --lib") == 0
    assert list_tests("list_tests absent --workspace --lib") != 0
    assert list_tests("list_tests fixture --workspace --lib", "echo 'error: build failed'; return 101") == 101

shared_step = re.findall(
    r"^      - name: Shared LSP and MCP owner and editor lifecycle\n.*?(?=^      - |^  \S|\Z)",
    text,
    re.MULTILINE | re.DOTALL,
)
assert len(shared_step) == 1, "expected one Shared LSP and MCP owner and editor lifecycle step"
assert any(block in shared_step[0] for block in blocks("run_tests")), \
    "the Shared LSP and MCP owner and editor lifecycle step defines no run_tests"

jobs = dict(re.findall(
    r"^  ([a-zA-Z0-9_-]+):\n(.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
    text.split("\njobs:\n", 1)[1],
    re.MULTILINE | re.DOTALL,
))
# The Windows jobs are the only place the code runs under Windows, and their coverage
# is a hand-written list of names. A bare `cargo test` there can select nothing and
# stay green; every invocation must go through a checked wrapper.
# Options and a toolchain may stand between `cargo` and `test`.
CARGO_TEST = re.compile(
    r"""(?<![\w./-])cargo(?:\.exe)?["']?(?:\s+(?:\+\S+|-\S+))*\s+["']?test\b"""
)
# Inside a wrapper, cargo runs only on the selection the wrapper was called with: a
# fixed selection added to a body would be a bare `cargo test` that the wrapper check
# cannot see, since the body is the checked part.
WRAPPED_CARGO = re.compile(
    r"""cargo test (?:--color never )?(?:"\$\{selection\[@\]\}"|"\$@")(?:\s|\)|$)"""
)


def bare_cargo_tests(job, wrappers):
    found = []
    for block in wrappers:
        if block not in job:
            continue
        job = job.replace(block, "")
        for line in block.splitlines()[1:]:
            words = line.split()
            if not words or words[0] in ("#", "printf", "echo"):
                continue
            for cargo in re.finditer(r"(?<![\w./-])cargo(?:\.exe)?\b", line):
                if not WRAPPED_CARGO.match(line, cargo.start()):
                    found.append(line.strip())
    return found + [match.group(0) for match in CARGO_TEST.finditer(job)]


# The guard is only as good as the inputs it can be seen to reject.
for bypass in ["cargo --locked test -p x", "cargo +stable test -p x", "cargo.exe test -p x"]:
    assert bare_cargo_tests(f"      run: {bypass}\n", []), bypass
for injected in ["cargo test -p x --lib", "cargo test -p x --lib || true", "cargo build -p x"]:
    block = blocks("run_tests")[0]
    lines = block.splitlines()
    tampered = "\n".join(lines[:-1] + [f"            {injected}", lines[-1]])
    assert bare_cargo_tests(tampered, [tampered]), injected

windows = [name for name, body in jobs.items() if re.search(r"^    runs-on: windows", body, re.MULTILINE)]
assert len(windows) >= 4, f"expected at least four Windows jobs, found {windows}"
for name in windows:
    found = bare_cargo_tests(jobs[name], wrappers)
    assert not found, f"{name} runs cargo test outside a wrapper: {found}"

check = jobs["check"]
# Unquoted, the `::` of a filter followed by a space reads as a YAML mapping.
selects = re.findall(r"^\s+select:.*$", check, re.MULTILINE)
assert selects and all(re.fullmatch(r"\s+select: \"[^\"]*\"", line) for line in selects), selects
legs = dict(re.findall(r"^\s+- shard: (\S+)\n\s+select: \"([^\"]*)\"$", check, re.MULTILINE))
assert len(legs) == len(selects), legs
words = {shard: shlex.split(select) for shard, select in legs.items()}
skipped = {words["unit-a"][i + 1] for i, word in enumerate(words["unit-a"]) if word == "--skip"}
filtered = set(words["unit-b"][words["unit-b"].index("--") + 1:])
assert skipped and skipped == filtered, f"unit-a skips {skipped}, unit-b runs {filtered}"
assert not any("-p" in leg for leg in words.values()), "a check leg selects a package"
assert all(legs[shard].startswith("--lib --bins --") for shard in ("unit-a", "unit-b")), legs
assert sum("--examples" in leg for leg in words.values()) == 1, "examples must be built by one leg"
assert re.search(
    r"^      - if: \$\{\{ !cancelled\(\) && matrix\.shard == 'unit-b' \}\}\n"
    r"        run: cargo test --locked --workspace --no-fail-fast --doc$",
    check, re.MULTILINE,
), "no doc-test step on the unit-b leg"
assert re.search(
    r"^      - run: cargo test --locked --workspace --no-fail-fast \$\{\{ matrix\.select \}\}$",
    check, re.MULTILINE,
), "no leg command in the check job"
masks = [leg[i + 1] for leg in words.values() for i, word in enumerate(leg) if word == "--test"]
metadata = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1"],
    capture_output=True, text=True, check=True, cwd=root,
).stdout)
targets = {
    target["name"]
    for package in metadata["packages"]
    for target in package["targets"]
    if "test" in target["kind"]
}
alphabet = string.ascii_letters + string.digits + "_"
for name in sorted(targets | set(alphabet) | {c + "x" for c in alphabet}):
    matched = [mask for mask in masks if fnmatch.fnmatchcase(name, mask)]
    assert len(matched) == 1, f"integration target {name!r} falls under {matched}"

def seam_leaks(packages):
    """Every way a `test-seams` feature can reach a build other than through a
    dev-dependency: a normal or build dependency that turns one on, a feature of any
    package that lists one, or a `default` that includes one. The seams shorten waits
    of the product, so a build that carries them is not the product."""
    leaks = []
    for package in packages:
        for dependency in package["dependencies"]:
            if any(feature.endswith("test-seams") for feature in dependency["features"]):
                if dependency["kind"] != "dev":
                    leaks.append(f"{package['name']} -> {dependency['name']}: {dependency['kind'] or 'normal'} dependency")
        for feature, enables in package["features"].items():
            if any(item.endswith("test-seams") for item in enables):
                leaks.append(f"{package['name']}: feature {feature!r} enables {enables}")
    return leaks


synthetic = [
    {"name": "user", "features": {}, "dependencies": [
        {"name": "lib", "kind": None, "features": ["test-seams"]},
        {"name": "lib", "kind": "build", "features": ["test-seams"]},
        {"name": "lib", "kind": "dev", "features": ["test-seams"]},
    ]},
    {"name": "lib", "dependencies": [], "features": {"test-seams": [], "default": ["test-seams"], "full": ["other/test-seams"]}},
]
assert len(seam_leaks(synthetic)) == 4, seam_leaks(synthetic)
assert not seam_leaks([{"name": "ok", "features": {"test-seams": []}, "dependencies": [
    {"name": "lib", "kind": "dev", "features": ["test-seams"]}]}])
assert not seam_leaks(metadata["packages"]), seam_leaks(metadata["packages"])
assert any(
    dependency["kind"] == "dev" and "test-seams" in dependency["features"]
    for package in metadata["packages"] for dependency in package["dependencies"]
), "no dev-dependency uses a test seam: the check above looked at nothing"


print(
    f"PASS: {len(wrappers)} CI wrappers (run_nonempty success/empty/failure; run_tests filters, "
    "exactness, failures and streamed output; list_tests runnable names without running); "
    f"shared LSP step wraps its tests; {len(windows)} Windows jobs wrap every cargo test; "
    f"{len(legs)} check legs split the unit filters and {len(targets)} integration targets once; "
    "test seams only in dev-dependencies"
)
