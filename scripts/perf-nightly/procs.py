"""Process accounting for one measured run (plan §6.4).

Every process the harness starts carries `PERF_NIGHTLY_RUN=<run id>` in its
environment, and children (the LSP relay's daemon, the MCP proxy's daemon)
inherit it. The run's tracked set is every process of this user whose
`/proc/<pid>/environ` holds that exact pair; RSS and CPU are sums over it, and
stopping a run signals exactly that set by PID — never a pattern match over
command lines, which would also hit the shell running the check.
"""

from __future__ import annotations

import os
import select
import signal
import threading

from clock import RealClock

MARKER = "PERF_NIGHTLY_RUN"
TERM_GRACE_S = 10.0
# SIGKILL cannot be ignored; this bound only guards against a stuck kernel.
KILL_WAIT_S = 30.0
CLK_TCK = os.sysconf("SC_CLK_TCK")


def child_env(run_id: str, xdg_root: dict | None = None, extra: dict | None = None,
              base: dict | None = None) -> dict:
    """Environment of a measured process: no `BSL_*` variable survives (they
    change cache roots, broker use, help lookup, logging and thread counts),
    the run marker is set, and the XDG roots point at the run's own dirs."""
    env = {k: v for k, v in (os.environ if base is None else base).items()
           if not k.startswith("BSL_")}
    env[MARKER] = run_id
    for name, path in (xdg_root or {}).items():
        env[name] = path
    env.update(extra or {})
    return env


def _read(path: str) -> bytes | None:
    try:
        with open(path, "rb") as fh:
            return fh.read()
    except OSError:
        return None


def _pids(proc_root: str, own_only: bool) -> list[int]:
    uid = os.getuid()
    pids = []
    for name in os.listdir(proc_root):
        if not name.isdigit():
            continue
        if own_only:
            try:
                if os.stat(os.path.join(proc_root, name)).st_uid != uid:
                    continue
            except OSError:
                continue
        pids.append(int(name))
    return sorted(pids)


def _has_marker(pid: int, run_id: str, proc_root: str) -> bool:
    environ = _read(os.path.join(proc_root, str(pid), "environ"))
    if not environ:
        return False
    return f"{MARKER}={run_id}".encode() in environ.split(b"\0")


def _argv0_base(pid: int, proc_root: str) -> str | None:
    cmdline = _read(os.path.join(proc_root, str(pid), "cmdline"))
    if not cmdline:
        return None
    argv0 = cmdline.split(b"\0", 1)[0].decode("utf-8", "replace")
    return os.path.basename(argv0)


def tracked(run_id: str, proc_root: str = "/proc") -> list[int]:
    return [pid for pid in _pids(proc_root, own_only=True)
            if _has_marker(pid, run_id, proc_root)]


def foreign(run_id: str | None = None, proc_root: str = "/proc",
            prefix: str = "bsl-analyzer") -> list[dict]:
    """Processes whose argv[0] base name starts with `prefix` and that do not
    carry the current run's marker (any marker when no run is active)."""
    found = []
    for pid in _pids(proc_root, own_only=False):
        if pid == os.getpid():
            continue
        name = _argv0_base(pid, proc_root)
        if name is None or not name.startswith(prefix):
            continue
        if run_id is not None and _has_marker(pid, run_id, proc_root):
            continue
        found.append({"pid": pid, "name": name})
    return found


def _status_kb(pid: int, field: str, proc_root: str) -> int | None:
    status = _read(os.path.join(proc_root, str(pid), "status"))
    if status is None:
        return None
    for line in status.decode("ascii", "replace").splitlines():
        if line.startswith(field + ":"):
            return int(line.split()[1])
    return None


def _cpu_s(pid: int, proc_root: str) -> float | None:
    stat = _read(os.path.join(proc_root, str(pid), "stat"))
    if stat is None:
        return None
    fields = stat.decode("ascii", "replace").rsplit(")", 1)[1].split()
    # fields[0] is field 3 (state); utime and stime are fields 14 and 15.
    return (int(fields[11]) + int(fields[12])) / CLK_TCK


def snapshot(run_id: str, proc_root: str = "/proc") -> dict:
    """One pass over /proc: the tracked set, its summed VmRSS and CPU."""
    pids = tracked(run_id, proc_root)
    rss = {pid: _status_kb(pid, "VmRSS", proc_root) for pid in pids}
    cpu = {pid: _cpu_s(pid, proc_root) for pid in pids}
    return {
        "pids": pids,
        "rss_kb": sum(v for v in rss.values() if v is not None),
        "rss_by_pid": {str(p): v for p, v in rss.items() if v is not None},
        "cpu_s": sum(v for v in cpu.values() if v is not None),
        "cpu_by_pid": {str(p): v for p, v in cpu.items() if v is not None},
    }


def machine_load(proc_root: str = "/proc") -> dict:
    loadavg = (_read(os.path.join(proc_root, "loadavg")) or b"").decode().split()
    running = total = None
    if len(loadavg) >= 4 and "/" in loadavg[3]:
        running, total = (int(x) for x in loadavg[3].split("/"))
    procs_running = None
    for line in (_read(os.path.join(proc_root, "stat")) or b"").decode().splitlines():
        if line.startswith("procs_running "):
            procs_running = int(line.split()[1])
    return {
        "load1": float(loadavg[0]) if loadavg else None,
        "load5": float(loadavg[1]) if len(loadavg) > 1 else None,
        "load15": float(loadavg[2]) if len(loadavg) > 2 else None,
        "runnable": running,
        "threads": total,
        "procs_running": procs_running,
    }


def _exited(fd: int) -> bool:
    poller = select.poll()
    poller.register(fd, select.POLLIN)
    return bool(poller.poll(0))


def stop(run_id: str, clock=None, proc_root: str = "/proc", grace: float = TERM_GRACE_S) -> dict:
    """SIGTERM the tracked set by PID, SIGKILL whatever outlives `grace`, and
    report what is left (must be nothing). A proxy can spawn a child while the
    set is being read, so the set is re-read until it comes back empty."""
    clock = clock or RealClock()
    report = {"terminated": [], "killed": [], "left": []}
    for _ in range(3):
        pids = tracked(run_id, proc_root)
        if not pids:
            break
        fds = {}
        for pid in pids:
            try:
                fds[pid] = os.pidfd_open(pid)
                os.kill(pid, signal.SIGTERM)
                report["terminated"].append(pid)
            except ProcessLookupError:
                continue
        try:
            if not clock.wait_exit(fds, grace):
                alive = [pid for pid, fd in fds.items() if not _exited(fd)]
                for pid in alive:
                    try:
                        os.kill(pid, signal.SIGKILL)
                        report["killed"].append(pid)
                    except ProcessLookupError:
                        pass
                RealClock().wait_exit({pid: fds[pid] for pid in alive}, KILL_WAIT_S)
        finally:
            for fd in fds.values():
                os.close(fd)
    report["left"] = tracked(run_id, proc_root)
    return report


class Sampler:
    """1 Hz sampler of the tracked set: peak of summed RSS and the last CPU
    seen per PID (so processes that exit before the end still count)."""

    def __init__(self, run_id: str, interval: float = 1.0, proc_root: str = "/proc"):
        self.run_id = run_id
        self.interval = interval
        self.proc_root = proc_root
        self.peak_rss_kb = 0
        self._cpu: dict[str, float] = {}
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def sample(self) -> dict:
        snap = snapshot(self.run_id, self.proc_root)
        with self._lock:
            self.peak_rss_kb = max(self.peak_rss_kb, snap["rss_kb"])
            for pid, cpu in snap["cpu_by_pid"].items():
                self._cpu[pid] = max(cpu, self._cpu.get(pid, 0.0))
        return snap

    def cpu_s(self) -> float:
        with self._lock:
            return sum(self._cpu.values())

    def _loop(self) -> None:
        while not self._stop.is_set():
            self.sample()
            self._stop.wait(self.interval)

    def stop(self) -> dict:
        if not self._stop.is_set():
            self._stop.set()
            self._thread.join()
            self.sample()
        return {"rss_peak_kb": self.peak_rss_kb, "cpu_s": round(self.cpu_s(), 3)}
