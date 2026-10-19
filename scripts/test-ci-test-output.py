#!/usr/bin/env python3
"""Check the CI test wrappers preserve failures and reject empty selections,
and that the Windows job routes every `cargo test` through one."""

from pathlib import Path
import re
import subprocess
import textwrap


workflow = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
wrappers = re.findall(
    r"^          run_(?:exact|nonempty)\(\) \{\n.*?^          \}",
    workflow.read_text(),
    re.MULTILINE | re.DOTALL,
)
assert wrappers, "no CI test wrappers found"
# Every wrapper must be checked: one whose spelling drifts from the pattern above
# would otherwise drop out of the check silently.
defined = re.findall(
    r"^\s*(?:function\s+)?run_\w+\s*\(\s*\)\s*\{", workflow.read_text(), re.MULTILINE
)
assert len(wrappers) == len(defined), f"checked {len(wrappers)} of {len(defined)} CI wrappers"
for block in wrappers:
    name = block.strip().split("(", 1)[0]
    for passed, status in [(1, 0), (0, 0), (1, 101)]:
        output = f"test result: ok. {passed} passed; diagnostic marker"
        result = subprocess.run(
            ["bash", "-c", "set -euo pipefail\n"
             + f"cargo() {{ echo '{output}' >&2; return {status}; }}\n"
             + textwrap.dedent(block) + f"\n{name} fixture test_name"],
            capture_output=True, text=True, check=False,
        )
        assert (result.returncode == 0) == (passed > 0 and status == 0), result
        assert output in result.stdout, result
# A step the runner cancels is killed before its command exits; what the command already
# printed must have reached the log by then, or a hung step leaves no trace of where it hung.
shared_step = re.search(
    r"^      - name: Shared LSP and MCP owner and editor lifecycle\n.*?(?=^      - name:)",
    workflow.read_text(),
    re.MULTILINE | re.DOTALL,
)
assert shared_step, "no Shared LSP and MCP owner and editor lifecycle step in the workflow"
shared_wrapper = next((block for block in wrappers if block in shared_step.group(0)), None)
assert shared_wrapper, "no CI test wrapper in the Shared LSP and MCP owner and editor lifecycle step"
shared_name = shared_wrapper.strip().split("(", 1)[0]


def run_shared(cargo_body):
    return subprocess.run(
        ["bash", "-c", "set -euo pipefail\n"
         + f"cargo() {{ {cargo_body}; }}\n"
         + textwrap.dedent(shared_wrapper) + f"\n{shared_name} -p x"],
        capture_output=True, text=True, check=False,
    )


result = run_shared("echo 'test result: ok. 1 passed; streamed marker'; kill -KILL $$")
assert result.returncode == -9, result
assert "streamed marker" in result.stdout, result
# A command that never ends is the last start line without an end line.
assert ">>> cargo test -p x" in result.stdout.splitlines(), result
assert "<<<" not in result.stdout, result
# Whatever the outcome, the log names the command before its output and its exit after it.
for passed, status in [(1, 0), (0, 0), (1, 101)]:
    output = f"test result: ok. {passed} passed; diagnostic marker"
    result = run_shared(f"echo '{output}' >&2; return {status}")
    lines = result.stdout.splitlines()
    started, finished = ">>> cargo test -p x", f"<<< cargo test -p x: exit {status}"
    assert lines.count(started) == 1 and lines.count(finished) == 1, result
    assert lines.index(started) < lines.index(output) < lines.index(finished), result
# The windows-mcp job is the only place the code runs under Windows, and its coverage
# is a hand-written list of names. A bare `cargo test` there can select nothing and
# stay green; every invocation in the job must go through a checked wrapper.
job = re.search(
    r"^  windows-mcp:$\n.*?(?=^  [a-zA-Z0-9_-]+:$\n|\Z)",
    workflow.read_text(),
    re.MULTILINE | re.DOTALL,
)
assert job, "no windows-mcp job in the workflow"
unwrapped = job.group(0)
for block in wrappers:
    unwrapped = unwrapped.replace(block, "")
bare_test = re.search(r'cargo\s+["\']?\s*test\b', unwrapped)
assert not bare_test, "windows-mcp runs cargo test outside a wrapper"
print(f"PASS: {len(wrappers)} CI wrappers, success/empty/failure output; shared LSP step streams output; windows-mcp wraps every cargo test")
