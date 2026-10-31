"""Одна холодная сборка графа как в гейте, с разбивкой стены по фазам.

Запускает `<binary> mcp serve --profile workspace --source-dir <root>` с окружением гейта
(без BSL_*, свежий BSL_CACHE_DIR, BSL_MCP_DAEMON_LOG), находит отсоединённый процесс
`--mode daemon` по маркеру окружения, раз в секунду опрашивает `graph status`, при первом
`ready` снимает VmHWM демона и останавливает все процессы прогона. Печатает таблицу фаз по
heartbeat-строкам `graph build: …` daemon.log и пишет summary.json рядом.

    python3 scripts/graph_ready_probe.py \
        --binary <bsl-analyzer-app> --out <каталог на NVMe> [--perf]

`--perf` — плоское семплирование `perf record -e cpu-clock` (perf — родитель команды;
стеки на машине недоступны). Каталог `--out` должен лежать на NVMe (как data dir гейта),
иначе фазы записи SQLite и хеширования XML растягиваются в разы.
"""
import argparse, csv, datetime, json, os, re, signal, subprocess, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.join(HERE, "perf-nightly")
sys.path.insert(0, HARNESS)
from mcp_client import McpClient  # noqa: E402
from clock import RealClock  # noqa: E402

MARKER = "PERF_NIGHTLY_RUN"


def state_of(result):
    docs = []
    if isinstance(result, dict):
        if isinstance(result.get("structuredContent"), dict):
            docs.append(result["structuredContent"])
        for c in result.get("content") or []:
            try:
                d = json.loads(c.get("text", ""))
            except Exception:
                continue
            if isinstance(d, dict):
                docs.append(d)
    for d in docs:
        if isinstance(d.get("state"), str):
            return d["state"]
    return None


def marked(run_id, daemon_only=False):
    found = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/environ", "rb") as fh:
                environ = fh.read()
            with open(f"/proc/{name}/cmdline", "rb") as fh:
                cmdline = fh.read()
        except OSError:
            continue
        if f"{MARKER}={run_id}".encode() not in environ:
            continue
        if daemon_only and b"--mode\x00daemon" not in cmdline:
            continue
        found.append(int(name))
    return found


def status_kb(pid, key):
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith(key + ":"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


class Sampler(threading.Thread):
    """Раз в 0,5 с: CPU процесса-демона (включая завершившиеся потоки), число потоков в R, RSS."""

    def __init__(self, pid, path):
        super().__init__(daemon=True)
        self.pid, self.path, self.stop = pid, path, threading.Event()
        self.rows = []

    def run(self):
        tick = os.sysconf("SC_CLK_TCK")
        while not self.stop.is_set():
            try:
                with open(f"/proc/{self.pid}/stat") as fh:
                    f = fh.read().rpartition(")")[2].split()
                cpu = (int(f[11]) + int(f[12])) / tick
                running = 0
                for tid in os.listdir(f"/proc/{self.pid}/task"):
                    try:
                        with open(f"/proc/{self.pid}/task/{tid}/stat") as fh:
                            if fh.read().rpartition(")")[2].split()[0] == "R":
                                running += 1
                    except OSError:
                        pass
            except OSError:
                break
            self.rows.append((time.monotonic(), datetime.datetime.now().astimezone().isoformat(timespec="milliseconds"),
                              cpu, running, status_kb(self.pid, "VmRSS") or 0))
            self.stop.wait(0.5)
        with open(self.path, "w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["mono_s", "wall", "cpu_s", "running", "rss_kb"])
            w.writerows(self.rows)


def phase_table(out, spawned_mono, ready_s, rows):
    if not rows:
        return []
    w0 = datetime.datetime.fromisoformat(rows[0][1])
    m0 = rows[0][0]
    mono_of = lambda iso: m0 + (datetime.datetime.fromisoformat(iso) - w0).total_seconds()
    phases, prev = [], None
    with open(os.path.join(out, "daemon.log"), errors="replace") as log:
        for line in log:
            m = re.match(r"(\S+)\s+\w+\s+graph build: (\w+)", line)
            if not m or m.group(2) == prev:
                continue
            if phases:
                phases[-1][2] = mono_of(m.group(1))
            phases.append([m.group(2), mono_of(m.group(1)), None])
            prev = m.group(2)
    if not phases:
        return []
    ready = spawned_mono + ready_s
    phases.insert(0, ["pre-build", spawned_mono, phases[0][1]])
    phases[-1][2] = ready
    phases[-1][0] += "→ready"

    def cpu_at(mono):
        best = rows[0][2]
        for r in rows:
            if r[0] <= mono:
                best = r[2]
            else:
                break
        return best

    table = []
    for name, a, b in phases:
        inwin = [r for r in rows if a <= r[0] < b]
        wall = b - a
        cpu = cpu_at(b) - cpu_at(a)
        table.append({"phase": name, "start_s": round(a - spawned_mono, 1), "wall_s": round(wall, 1),
                      "wall_pct": round(100 * wall / ready_s, 1), "cpu_s": round(cpu, 1),
                      "cores": round(cpu / wall, 2) if wall > 0 else None,
                      "serial_share": round(sum(1 for r in inwin if r[3] <= 2) / len(inwin), 2) if inwin else None,
                      "rss_max_mib": max((r[4] for r in inwin), default=0) // 1024})
    return table


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--root", default="/home/itrous/src/pt/erp-at-9469321")
    ap.add_argument("--out", required=True)
    ap.add_argument("--perf", action="store_true")
    ap.add_argument("--timeout", type=float, default=1500.0)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith("BSL_")}
    env["BSL_CACHE_DIR"] = os.path.join(args.out, "cache")
    env["BSL_MCP_DAEMON_LOG"] = os.path.join(args.out, "daemon.log")
    run_id = f"probe-{os.getpid()}-{int(time.time())}"
    env[MARKER] = run_id
    binary = os.path.abspath(args.binary)
    argv = [binary, "mcp", "serve", "--profile", "workspace", "--source-dir", args.root]
    if args.perf:
        argv = ["perf", "record", "-e", "cpu-clock", "-F", "199", "-k", "CLOCK_MONOTONIC",
                "-o", os.path.join(args.out, "perf.data"), "--", *argv]
    stderr = open(os.path.join(args.out, "stderr.txt"), "wb")
    spawned_mono = time.monotonic()
    client = McpClient(argv, args.root, env, stderr, RealClock())
    pid = None
    deadline = time.monotonic() + 60
    while pid is None and time.monotonic() < deadline:
        daemons = marked(run_id, daemon_only=True)
        pid = daemons[0] if daemons else None
        if pid is None:
            time.sleep(0.2)
    if pid is None:
        sys.exit("daemon process not found")
    sampler = Sampler(pid, os.path.join(args.out, "samples.csv"))
    sampler.start()
    _, _, err = client.initialize(120)
    summary = {"pid": pid, "argv": argv, "initialize_error": err, "graph_ready_s": None,
               "diagnostics_ready_s": None}
    while summary["graph_ready_s"] is None and time.monotonic() - spawned_mono < args.timeout:
        for tool in ("diagnostics", "graph"):
            if summary[f"{tool}_ready_s"] is not None:
                continue
            _, result, err = client.call_tool(tool, {"action": "status"}, 120)
            st = state_of(result) if err is None else None
            if st == "ready":
                summary[f"{tool}_ready_s"] = round(time.monotonic() - spawned_mono, 2)
                if tool == "graph":
                    summary["daemon_vmhwm_kb_at_ready"] = status_kb(pid, "VmHWM")
                    summary["daemon_threads_at_ready"] = len(os.listdir(f"/proc/{pid}/task"))
            elif tool == "graph" and st in ("failed", "disabled"):
                summary["graph_error"] = result
                summary["graph_ready_s"] = -1
        time.sleep(1.0)
    sampler.stop.set()
    sampler.join()
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for p in marked(run_id):
            try:
                os.kill(p, sig)
            except ProcessLookupError:
                pass
        for _ in range(150):
            if not marked(run_id):
                break
            time.sleep(0.2)
    ready = summary["graph_ready_s"] or 0
    summary["phases"] = phase_table(args.out, spawned_mono, ready, sampler.rows) if ready > 0 else []
    summary["cpu_s_at_ready"] = sampler.rows[-1][2] if sampler.rows else None
    with open(os.path.join(args.out, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=1, ensure_ascii=False)
    print(f"graph_ready_s={summary['graph_ready_s']} cpu_s={summary['cpu_s_at_ready']} "
          f"daemon_vmhwm_mib={(summary.get('daemon_vmhwm_kb_at_ready') or 0) // 1024}")
    print(f"{'phase':28} {'start':>7} {'wall':>7} {'%':>5} {'cpu':>7} {'cores':>6} {'serial':>6} {'rssMiB':>7}")
    for t in summary["phases"]:
        print(f"{t['phase']:28} {t['start_s']:7.1f} {t['wall_s']:7.1f} {t['wall_pct']:5.1f} {t['cpu_s']:7.1f} "
              f"{t['cores'] or 0:6.2f} {t['serial_share'] if t['serial_share'] is not None else 0:6.2f} {t['rss_max_mib']:7d}")


if __name__ == "__main__":
    main()
