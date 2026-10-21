"""Process accounting (plan §6.4): marker, foreign check, RSS sum, stop."""

import os
import subprocess
import sys
import tempfile
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
        stubborn = self.keep(spawn(f"{FOREIGN_PREFIX}-app", procs.child_env(run_id), ignore_term=True))
        polite = self.keep(spawn(f"{FOREIGN_PREFIX}-app", procs.child_env(run_id)))
        neighbour = self.keep(spawn(f"{FOREIGN_PREFIX}-app", dict(os.environ)))
        report = procs.stop(run_id, FakeClock(stuck=lambda pid: pid == stubborn.pid))
        self.assertEqual(sorted(report["terminated"]), sorted([stubborn.pid, polite.pid]))
        self.assertEqual(report["killed"], [stubborn.pid])
        self.assertEqual(report["left"], [])
        self.assertEqual(stubborn.wait(), -9)
        self.assertEqual(polite.wait(), -15)
        self.assertIsNone(neighbour.poll(), "a process without the run marker must stay alive")

    def test_foreign_on_real_proc_matches_only_the_named_process(self):
        named = self.keep(spawn(f"{FOREIGN_PREFIX}-app", dict(os.environ)))
        shell = subprocess.Popen(["sh", "-c", f"read x; echo {FOREIGN_PREFIX}"], stdin=subprocess.PIPE,
                                 stdout=subprocess.DEVNULL)
        try:
            found = procs.foreign(prefix=FOREIGN_PREFIX)
        finally:
            shell.communicate(b"\n")
        self.assertEqual(found, [{"pid": named.pid, "name": f"{FOREIGN_PREFIX}-app"}])


if __name__ == "__main__":
    unittest.main()
