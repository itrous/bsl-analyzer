"""Process accounting (plan §6.4): marker, foreign check, RSS sum, stop."""

import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

from helpers import FOREIGN_PREFIX, FakeClock, event_driven_process_waits

import procs


def fake_proc(root, pid, argv, env=(), rss_kb=None):
    path = os.path.join(root, str(pid))
    os.makedirs(path)
    with open(os.path.join(path, "cmdline"), "wb") as fh:
        fh.write(b"\0".join(a.encode() for a in argv) + b"\0")
    with open(os.path.join(path, "environ"), "wb") as fh:
        fh.write(b"\0".join(e.encode() for e in env) + b"\0")
    with open(os.path.join(path, "status"), "w") as fh:
        fh.write(f"Name:\tx\nVmRSS:\t{rss_kb} kB\n" if rss_kb is not None else "Name:\tx\n")
    with open(os.path.join(path, "stat"), "w") as fh:
        fh.write(f"{pid} (x) S 1 1 1 0 -1 0 0 0 0 0 100 50 0 0\n")


class FakeProcTree(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name

    def tearDown(self):
        self._tmp.cleanup()

    def test_foreign_reads_argv0_not_command_line_substrings(self):
        # A shell whose command line merely mentions the name is not foreign.
        fake_proc(self.root, 100, ["/opt/bin/bsl-analyzer-app", "lsp"])
        fake_proc(self.root, 101, ["sh", "-c", "echo bsl-analyzer"])
        fake_proc(self.root, 102, ["bsl-analyzer-app", "mcp", "serve"], [f"{procs.MARKER}=run-x"])
        self.assertEqual([p["pid"] for p in procs.foreign(None, self.root)], [100, 102])
        self.assertEqual(procs.foreign("run-x", self.root), [{"pid": 100, "name": "bsl-analyzer-app"}])

    def test_run_rss_is_the_sum_over_the_tracked_set_in_one_pass(self):
        marker = f"{procs.MARKER}=run-r"
        fake_proc(self.root, 200, ["bsl-analyzer-app", "lsp"], [marker], rss_kb=37172)
        fake_proc(self.root, 201, ["bsl-analyzer-app", "mcp"], ["A=1", marker], rss_kb=371396)
        fake_proc(self.root, 202, ["bsl-analyzer-app", "lsp"], [f"{procs.MARKER}=run-rr"], rss_kb=999)
        snap = procs.snapshot("run-r", self.root)
        self.assertEqual(snap["pids"], [200, 201])
        self.assertEqual(snap["rss_kb"], 408568)

    def test_child_env_drops_every_bsl_variable(self):
        env = procs.child_env("run-e", {"XDG_CACHE_HOME": "/c"}, base={
            "BSL_CACHE_DIR": "/x", "BSL_MCP_BROKER": "0", "BSL_UNKNOWN_X": "1", "PATH": "/bin"})
        self.assertEqual(env, {"PATH": "/bin", procs.MARKER: "run-e", "XDG_CACHE_HOME": "/c"})

    def test_child_env_drops_every_embedding_variable(self):
        # `EMBED_KEEP` shares letters with the family and not its prefix; the `BSL_*` of
        # `extra` is what the harness itself hands the run and has to survive the filter.
        env = procs.child_env("run-e", extra={"BSL_CACHE_DIR": "/run/cache"}, base={
            "BSL_X": "1", "EMBEDDING_URL": "http://127.0.0.1:9", "EMBEDDING_MODEL": "m",
            "EMBEDDING_API_KEY": "k", "EMBEDDING_UNKNOWN_X": "1", "EMBED_KEEP": "1", "PATH": "/bin"})
        self.assertEqual(env, {"EMBED_KEEP": "1", "PATH": "/bin", procs.MARKER: "run-e",
                               "BSL_CACHE_DIR": "/run/cache"})


def fake_run_proc(root, pid, run_id, hwm_kb, rss_kb=1000, cpu_ticks=(100, 50)):
    """A process of a run with a VmHWM (utime and stime in clock ticks); a second
    call for the same PID is the same process a moment later."""
    shutil.rmtree(os.path.join(root, str(pid)), ignore_errors=True)
    fake_proc(root, pid, ["bsl-analyzer-app", "mcp"], [f"{procs.MARKER}={run_id}"])
    with open(os.path.join(root, str(pid), "status"), "w") as fh:
        fh.write(f"Name:\tx\nVmHWM:\t{hwm_kb} kB\nVmRSS:\t{rss_kb} kB\n")
    with open(os.path.join(root, str(pid), "stat"), "w") as fh:
        fh.write(f"{pid} (x) S 1 1 1 0 -1 0 0 0 0 0 {cpu_ticks[0]} {cpu_ticks[1]} 0 0\n")


class SamplerPeaks(unittest.TestCase):
    """What the sampler keeps of the processes of a run: the peak `VmHWM` of
    each process (also of one that exits), the largest of them, and the CPU."""

    def setUp(self):
        event_driven_process_waits(self)
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = self._tmp.name

    def sampler(self):
        sampler = procs.Sampler("run-h", proc_root=self.root)
        self.addCleanup(sampler.stop)
        return sampler

    def test_hwm_is_the_largest_peak_of_the_run_and_the_other_fields_stay(self):
        fake_run_proc(self.root, 300, "run-h", 5000, rss_kb=1000)
        fake_run_proc(self.root, 301, "run-h", 8000, rss_kb=2500, cpu_ticks=(200, 100))
        fake_run_proc(self.root, 302, "run-other", 99999, rss_kb=99999)
        sampler = self.sampler()
        sampler.sample()
        self.assertEqual(sampler.stop(), {"rss_peak_kb": 3500, "cpu_s": 4.5, "hwm_kb": 8000})

    def test_a_process_that_exits_keeps_its_peak_and_its_cpu(self):
        fake_run_proc(self.root, 300, "run-h", 5000)
        fake_run_proc(self.root, 301, "run-h", 8000, cpu_ticks=(200, 100))
        sampler = self.sampler()
        sampler.sample()
        shutil.rmtree(os.path.join(self.root, "301"))
        fake_run_proc(self.root, 300, "run-h", 6500, cpu_ticks=(300, 100))
        sampler.sample()
        stopped = sampler.stop()
        self.assertEqual((stopped["hwm_kb"], stopped["cpu_s"]), (8000, 7.0))

    def test_stop_takes_one_last_sample(self):
        fake_run_proc(self.root, 300, "run-h", 5000)
        sampler = self.sampler()
        sampler.sample()
        fake_run_proc(self.root, 300, "run-h", 9000)
        self.assertEqual(sampler.stop()["hwm_kb"], 9000)

    def test_a_peak_never_goes_down(self):
        fake_run_proc(self.root, 300, "run-h", 7000)
        sampler = self.sampler()
        sampler.sample()
        fake_run_proc(self.root, 300, "run-h", 4000)
        sampler.sample()
        self.assertEqual(sampler.stop()["hwm_kb"], 7000)

    def test_a_run_without_processes_has_no_peak(self):
        self.assertEqual(self.sampler().stop(), {"rss_peak_kb": 0, "cpu_s": 0.0, "hwm_kb": 0})

    def test_cpu_without_the_excluded_pids(self):
        fake_run_proc(self.root, 300, "run-h", 1)
        fake_run_proc(self.root, 301, "run-h", 1, cpu_ticks=(200, 100))
        sampler = self.sampler()
        sampler.sample()
        self.assertEqual((sampler.cpu_s(), sampler.cpu_s(()), sampler.cpu_s([301]), sampler.cpu_s([300, 301])),
                         (4.5, 4.5, 1.5, 0.0))
        self.assertEqual(sampler.cpu_s(exclude=[301, 999]), 1.5)


def spawn(argv0, env, ignore_term=False):
    code = ("import signal\n"
            + ("signal.signal(signal.SIGTERM, signal.SIG_IGN)\n" if ignore_term else "")
            + "print(flush=True)\nwhile True: signal.pause()\n")
    proc = subprocess.Popen([argv0, "-c", code], executable=sys.executable, env=env,
                            stdout=subprocess.PIPE, stdin=subprocess.DEVNULL)
    proc.stdout.readline()
    return proc


class RealProcesses(unittest.TestCase):
    def setUp(self):
        event_driven_process_waits(self)
        self.procs = []

    def tearDown(self):
        for proc in self.procs:
            if proc.poll() is None:
                proc.kill()
            proc.wait()
            proc.stdout.close()

    def keep(self, proc):
        self.procs.append(proc)
        return proc

    def test_stop_kills_a_term_ignoring_daemon_and_spares_unmarked_neighbours(self):
        run_id = "run-stop-test"
        stubborn = self.keep(spawn(f"{FOREIGN_PREFIX}app", procs.child_env(run_id), ignore_term=True))
        polite = self.keep(spawn(f"{FOREIGN_PREFIX}app", procs.child_env(run_id)))
        neighbour = self.keep(spawn(f"{FOREIGN_PREFIX}app", dict(os.environ)))
        report = procs.stop(run_id, FakeClock(stuck=lambda pid: pid == stubborn.pid))
        self.assertEqual(sorted(report["terminated"]), sorted([stubborn.pid, polite.pid]))
        self.assertEqual(report["killed"], [stubborn.pid])
        self.assertEqual(report["left"], [])
        self.assertEqual(stubborn.wait(), -9)
        self.assertEqual(polite.wait(), -15)
        self.assertIsNone(neighbour.poll(), "a process without the run marker must stay alive")

    def test_foreign_on_real_proc_matches_only_the_named_process(self):
        named = self.keep(spawn(f"{FOREIGN_PREFIX}app", dict(os.environ)))
        shell = subprocess.Popen(["sh", "-c", f"read x; echo {FOREIGN_PREFIX}"], stdin=subprocess.PIPE,
                                 stdout=subprocess.DEVNULL)
        try:
            found = procs.foreign(prefix=FOREIGN_PREFIX)
        finally:
            shell.communicate(b"\n")
        self.assertEqual(found, [{"pid": named.pid, "name": f"{FOREIGN_PREFIX}app"}])


class LoadSnapshots(unittest.TestCase):
    """Consistent CPU snapshots (И6): own CPU, a reaped burner, a raced probe, a read error."""

    def setUp(self):
        event_driven_process_waits(self)
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = self._tmp.name

    def sampler(self, busy):
        sampler = procs.Sampler("run-h", proc_root=self.root)
        sampler.bind_load(FakeClock(), busy)
        self.addCleanup(sampler.stop)
        return sampler

    def test_an_exited_pid_keeps_its_cpu_in_the_next_probe(self):
        fake_run_proc(self.root, 300, "run-h", 1000, cpu_ticks=(200, 100))
        sampler = self.sampler(lambda: 1.0)
        first = sampler.probe()
        shutil.rmtree(os.path.join(self.root, "300"))
        second = sampler.probe()
        self.assertGreater(first["own_cpu_s"], 0)
        self.assertGreaterEqual(second["own_cpu_s"], first["own_cpu_s"])

    def test_a_reaped_burner_is_credited_once(self):
        fake_run_proc(self.root, 300, "run-h", 1000, cpu_ticks=(100, 0))
        sampler = self.sampler(lambda: 1.0)
        sampler.sample()
        sampled = sampler.cpu_s()
        sampler.credit_reaped(300, sampled + 5.0)
        self.assertAlmostEqual(sampler.probe()["own_cpu_s"], sampled + 5.0)
        sampler.credit_reaped(300, sampled + 5.0)
        self.assertAlmostEqual(sampler.probe()["own_cpu_s"], sampled + 5.0)

    def test_an_unmarked_process_is_not_own_cpu(self):
        fake_run_proc(self.root, 300, "run-h", 1000, cpu_ticks=(200, 100))
        fake_run_proc(self.root, 301, "run-other", 1000, cpu_ticks=(400, 100))
        sampler = self.sampler(lambda: 10.0)
        own = sampler.probe()["own_cpu_s"]
        self.assertAlmostEqual(own, sampler.cpu_s())
        self.assertLess(own, 8.0)

    def test_a_read_error_is_not_a_zero_load(self):
        fake_run_proc(self.root, 300, "run-h", 1000)

        def busy():
            raise OSError("stat unreadable")

        sampler = self.sampler(busy)
        with self.assertRaises(OSError):
            sampler.probe()
        probe = sampler._probes[-1]
        self.assertIsNone(probe["busy_s"])
        self.assertNotEqual(probe["busy_s"], 0)
        self.assertIn("stat unreadable", probe["error"])

    def test_a_boundary_probe_waits_for_the_background_probe(self):
        fake_run_proc(self.root, 300, "run-h", 1000)
        inside = 0
        peak = 0
        guard = threading.Lock()
        busy_entered = threading.Event()
        release_busy = threading.Event()
        attempt_events = {f"probe-{i}": threading.Event() for i in range(4)}
        lock = threading.RLock()

        class ObservedRLock:
            def acquire(self, *args, **kwargs):
                event = attempt_events.get(threading.current_thread().name)
                if event is not None:
                    event.set()
                return lock.acquire(*args, **kwargs)

            def release(self):
                lock.release()

            def __enter__(self):
                self.acquire()
                return self

            def __exit__(self, exc_type, exc_value, traceback):
                self.release()

        def busy():
            nonlocal inside, peak
            with guard:
                inside += 1
                peak = max(peak, inside)
                busy_entered.set()
            release_busy.wait()
            with guard:
                inside -= 1
            return 1.0

        sampler = self.sampler(busy)
        sampler._lock = ObservedRLock()
        threads = [threading.Thread(target=sampler.probe, name=f"probe-{i}") for i in range(4)]
        for thread in threads:
            thread.start()
        busy_entered.wait()
        for event in attempt_events.values():
            event.wait()
        release_busy.set()
        for thread in threads:
            thread.join()
        self.assertEqual(peak, 1)
        self.assertEqual(len(sampler._probes), 4)


if __name__ == "__main__":
    unittest.main()
