#!/usr/bin/env python3
"""Host driver for the Windows native acceptance stand (libvirt domain win10).

  build [--rev REV]                        build bsl-analyzer-app + bsl-native-check in the guest
  run   [--rev REV] [--fixtures L,M] [--rows id,...]   run the matrix with those binaries

Data goes through the virtiofs share (guest Z:); QGA is used only to start one
PowerShell process per command. Results land in <share>/wa/runs/<run-id>/out.
"""
import argparse
import base64
import json
import pathlib
import shutil
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[1]
# Machine-specific paths live in stand.local.json (not committed); see stand.example.json.
STAND = json.loads((HERE / "stand.local.json").read_text(encoding="utf-8"))
DOMAIN = STAND["domain"]
SHARE = pathlib.Path(STAND["host_share"])
GUEST_PYTHON = STAND["guest_python"]
REGISTRY = STAND["registry"]
FIXTURE_ROOT = STAND["fixture_root"]
SENTINELS = STAND["sentinels"]
VCVARS = STAND["vcvars"]
RUSTUP_HOME = STAND["rustup_home"]
CARGO_HOME = STAND["cargo_home"]
TOOLCHAIN = STAND["toolchain"]
TARGET = "x86_64-pc-windows-msvc"


def agent(command, timeout=10):
    out = subprocess.run(["virsh", "-c", "qemu:///system", "qemu-agent-command", DOMAIN,
                          json.dumps(command), "--timeout", str(timeout)],
                         capture_output=True, text=True, check=True).stdout
    return json.loads(out)["return"]


def guest_powershell(script, deadline_s, progress=None):
    """Run a PowerShell script in the guest, wait for it, return (exit code, stdout, stderr)."""
    script = "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n" + script
    encoded = base64.b64encode(script.encode("utf-16-le")).decode()
    pid = agent({"execute": "guest-exec", "arguments": {
        "path": "powershell.exe",
        "arg": ["-NoProfile", "-NonInteractive", "-EncodedCommand", encoded],
        "capture-output": True}})["pid"]
    end = time.time() + deadline_s
    while time.time() < end:
        status = agent({"execute": "guest-exec-status", "arguments": {"pid": pid}})
        if status.get("exited"):
            decode = lambda key: base64.b64decode(status.get(key, "")).decode("utf-8", "replace")
            return status.get("exitcode"), decode("out-data"), decode("err-data")
        if progress:
            progress()
        time.sleep(5)
    raise TimeoutError(f"guest process {pid} still running after {deadline_s}s")


def read_log(path):
    """PowerShell `*>` redirection writes UTF-16 LE with a BOM."""
    data = path.read_bytes()
    return data.decode("utf-16") if data.startswith(b"\xff\xfe") else data.decode("utf-8", "replace")


def revision(rev):
    return subprocess.run(["git", "-C", str(REPO), "rev-parse", "--short=12", rev],
                          capture_output=True, text=True, check=True).stdout.strip()


def guest_bin(sha):
    return rf"C:\wa\bin-{sha}"


def build(args):
    sha = revision(args.rev)
    src = SHARE / "wa" / f"src-{sha}"
    if not src.exists():
        src.mkdir(parents=True)
        archive = subprocess.Popen(["git", "-C", str(REPO), "archive", sha], stdout=subprocess.PIPE)
        subprocess.run(["tar", "-x", "-C", str(src)], stdin=archive.stdout, check=True)
        if archive.wait() != 0:
            raise SystemExit("git archive failed")
    log = rf"Z:\wa\build-{sha}.log"
    script = rf"""
$src = 'C:\wa\src-{sha}'
if (-not (Test-Path $src)) {{ robocopy 'Z:\wa\src-{sha}' $src /E /NFL /NDL /NJH /NJS /NP | Out-Null }}
foreach ($line in (cmd /c ('call "{VCVARS}" x64 >nul && set'))) {{
  if ($line -match '^([^=]+)=(.*)$') {{ Set-Item -Path ("env:" + $Matches[1]) -Value $Matches[2] }}
}}
$env:RUSTUP_HOME = '{RUSTUP_HOME}'
$env:CARGO_HOME = '{CARGO_HOME}'
$env:RUSTUP_TOOLCHAIN = '{TOOLCHAIN}'
$env:CARGO_TARGET_DIR = 'Z:\wa\target'
$env:PATH = '{STAND['cargo_bin']};' + $env:PATH
Set-Location $src
$ErrorActionPreference = 'Continue'
& cargo build --locked --target {TARGET} -p bsl-analyzer --bin bsl-analyzer-app -p bsl-check-executor --bin bsl-native-check *> '{log}'
$code = $LASTEXITCODE
if ($code -ne 0) {{ Get-Content '{log}' -Tail 40; exit $code }}
# Cargo does not reliably refresh the uplifted debug\*.exe on virtiofs; take the linker output.
$deps = 'Z:\wa\target\{TARGET}\debug\deps'
$bin = '{guest_bin(sha)}'
New-Item -ItemType Directory -Force $bin | Out-Null
foreach ($name in 'bsl-analyzer-app','bsl-native-check') {{
  $built = Get-Item "$deps\$($name.Replace('-','_')).exe"
  Copy-Item $built.FullName "$bin\$name.exe" -Force
}}
Get-FileHash '{guest_bin(sha)}\*.exe' -Algorithm SHA256 | % {{ "$($_.Hash) $($_.Path)" }}
"""
    log_path = SHARE / "wa" / f"build-{sha}.log"
    seen = [0]

    def progress():
        if log_path.exists():
            lines = read_log(log_path).splitlines()
            compiling = [l for l in lines if "Compiling" in l]
            if len(compiling) != seen[0]:
                seen[0] = len(compiling)
                print(f"  {len(compiling)} crates compiled; last: {compiling[-1].strip() if compiling else ''}",
                      flush=True)

    code, out, err = guest_powershell(script, args.deadline, progress)
    print(out)
    if code != 0:
        print(err[-3000:], file=sys.stderr)
    return code


def run(args):
    sha = revision(args.rev)
    run_id = time.strftime("%Y%m%d-%H%M%S") + f"-{sha}"
    host_run = SHARE / "wa" / "runs" / run_id
    host_run.mkdir(parents=True)
    shutil.copy(HERE / "guest_run.py", host_run)
    shutil.copy(HERE / "windows-matrix.json", host_run)
    guest_run = rf"C:\wa\runs\{run_id}"
    rows = f"'--rows', '{args.rows}'," if args.rows else ""
    rows += f"'--lifecycle', '{args.lifecycle}'," if args.lifecycle else ""
    script = rf"""
New-Item -ItemType Directory -Force '{guest_run}' | Out-Null
Copy-Item 'Z:\wa\runs\{run_id}\guest_run.py','Z:\wa\runs\{run_id}\windows-matrix.json' '{guest_run}'
$ErrorActionPreference = 'Continue'
$argv = @('-I', '-B', '{guest_run}\guest_run.py',
  '--app', '{guest_bin(sha)}\bsl-analyzer-app.exe', '--executor', '{guest_bin(sha)}\bsl-native-check.exe',
  '--registry', '{REGISTRY}', '--fixture-root', '{FIXTURE_ROOT}', '--matrix', '{guest_run}\windows-matrix.json',
  '--out', 'Z:\wa\runs\{run_id}\out', '--work', '{guest_run}\work', '--sentinel-dir', '{SENTINELS}',
  {rows} '--fixtures', '{args.fixtures}')
& '{GUEST_PYTHON}' @argv *> 'Z:\wa\runs\{run_id}\console.log'
exit $LASTEXITCODE
"""
    results = host_run / "out" / "results.jsonl"
    shown = [0]

    def progress():
        if results.exists():
            lines = results.read_text(encoding="utf-8").splitlines()
            for line in lines[shown[0]:]:
                row = json.loads(line)
                print(f"  {row['verdict']} {row['id']} {row['seconds']}s {'; '.join(row['problems'])[:300]}",
                      flush=True)
            shown[0] = len(lines)

    print(f"run {run_id}", flush=True)
    code, out, err = guest_powershell(script, args.deadline, progress)
    progress()
    console = host_run / "console.log"
    if console.exists():
        print(read_log(console)[-2000:])
    print(f"results: {host_run / 'out'}")
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    b = sub.add_parser("build")
    b.add_argument("--rev", default="HEAD")
    b.add_argument("--deadline", type=int, default=4 * 3600)
    r = sub.add_parser("run")
    r.add_argument("--rev", default="HEAD")
    r.add_argument("--fixtures", default="L,M")
    r.add_argument("--rows")
    r.add_argument("--lifecycle", default="", help="active-cancel,active-timeout,queue-cancel")
    r.add_argument("--deadline", type=int, default=6 * 3600)
    args = parser.parse_args()
    return build(args) if args.command == "build" else run(args)


if __name__ == "__main__":
    sys.exit(main())
