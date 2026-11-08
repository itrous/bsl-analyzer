"""The opt-in resource series of a gate run (`--trace-resources`): its shape
over a fake /proc, its time bounds from calls and daemon log marks, and that it
never becomes an input of the verdict. Clocks are virtual; nothing waits."""

import builtins
import errno
import json
import os
import shutil
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

from helpers import FakeClock, event_driven_process_waits, set_mode
from test_procs import fake_proc

import gate
import perf_nightly
import procs
import scenarios
from common import read_json
from gate_fixtures import MODES, GateCase

T0 = 1_800_000_000.0
UNIX_TIME_FLOOR = 1_000_000_000

PROCESS_KEYS = {"pid", "cpu_s", "rss_kb", "hwm_kb", "read_bytes", "write_bytes"}
LINE_KEYS = {
    "header": {"run_id", "binary_sha", "role", "series", "attempt", "interval_s"},
    "sample": {"t_s", "processes"},
    "event": {"name", "t_s"},
    "phase": {"name", "elapsed_ms", "source"},
    "footer": {"status", "errors"},
}
EVENT_NAMES = {"graph_ready", "call_start", "call_end"}


def own_proc(root, pid, run_id, cpu_s, rss_kb, hwm_kb, io=None):
    shutil.rmtree(os.path.join(root, str(pid)), ignore_errors=True)
    fake_proc(root, pid, ["bsl-analyzer-app", "mcp"], [f"{procs.MARKER}={run_id}"])
    with open(os.path.join(root, str(pid), "status"), "w") as fh:
        fh.write(f"Name:\tx\nVmHWM:\t{hwm_kb} kB\nVmRSS:\t{rss_kb} kB\n")
    ticks = int(cpu_s * procs.CLK_TCK)
    with open(os.path.join(root, str(pid), "stat"), "w") as fh:
        fh.write(f"{pid} (x) S 1 1 1 0 -1 0 0 0 0 0 {ticks - ticks // 3} {ticks // 3} 0 0\n")
    if io is not None:
        with open(os.path.join(root, str(pid), "io"), "w") as fh:
            fh.write(f"rchar: 1\nwchar: 2\nread_bytes: {io[0]}\nwrite_bytes: {io[1]}\ncancelled_write_bytes: 0\n")


def lines(path):
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh]


def utc(*stamp):
    return datetime(*stamp, tzinfo=timezone.utc).timestamp()


class WriteFails:
    """A text file whose writes fail with ENOSPC after `ok` good ones, and that
    remembers how often it was written to and whether it was closed."""

    def __init__(self, fh, ok):
        self.fh, self.ok, self.attempts, self.closed = fh, ok, 0, False

    def write(self, text):
        self.attempts += 1
        if self.attempts > self.ok:
            raise OSError(errno.ENOSPC, "No space left on device")
        return self.fh.write(text)

    def flush(self):
        self.fh.flush()

    def close(self):
        self.closed = True
        self.fh.close()


def opening_trace_as(wrap):
    """An `open` that hands the trace file to `wrap(file)` and everything else
    through untouched."""
    real_open = builtins.open

    def opener(path, *args, **kwargs):
        fh = real_open(path, *args, **kwargs)
        return wrap(fh) if str(path).endswith(procs.TRACE_NAME) else fh

    return opener


def check_shape(case, g):
    """One line of a series, to the letter of its kind."""
    case.assertEqual(g["schema"], 1, g)
    kind = g["kind"]
    case.assertIn(kind, LINE_KEYS, g)
    keys = set(LINE_KEYS[kind])
    if kind == "event":
        case.assertIn(g["name"], EVENT_NAMES, g)
        if g["name"] != "graph_ready":
            keys.add("call_id")
        if g["name"] == "call_end":
            keys.add("ok")
    case.assertEqual(set(g) - {"schema", "kind", "wall_s"}, keys, g)
    if kind == "phase":
        case.assertIn(g["wall_s"].__class__, (float, type(None)), g)
    else:
        case.assertIsInstance(g["wall_s"], float, g)
    if kind in ("sample", "event"):
        case.assertIsInstance(g["t_s"], float, g)
    if kind == "sample":
        for proc in g["processes"]:
            case.assertEqual(set(proc), PROCESS_KEYS, g)
            case.assertIsInstance(proc["pid"], int, g)
            for name in PROCESS_KEYS - {"pid"}:
                case.assertTrue(proc[name] is None or isinstance(proc[name], (int, float)), g)
    if kind == "phase":
        case.assertIn(g["name"], gate.PHASES, g)
        case.assertIsInstance(g["elapsed_ms"], int, g)
        case.assertEqual(g["source"], "daemon.log", g)
    if kind == "footer":
        case.assertIn(g["status"], ("complete", "partial"), g)
        case.assertTrue(all(isinstance(e, str) for e in g["errors"]), g)
        case.assertEqual(g["status"], "partial" if g["errors"] else "complete", g)


HEADER = {"run_id": "run-t", "binary_sha": "c" * 40, "role": "candidate", "series": "A", "attempt": 1}


class TempDir(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = self._tmp.name
        self.clock = FakeClock()
        self.path = os.path.join(self.tmp, procs.TRACE_NAME)

    def trace(self, header=None):
        return procs.ResourceTrace(self.path, header or HEADER, self.clock, wall=lambda: T0 + self.clock.now())


class FakeProc(TempDir):
    """A /proc of the test's own, sampled only when the test says so."""

    def setUp(self):
        super().setUp()
        event_driven_process_waits(self)
        self.root = os.path.join(self.tmp, "proc")
        os.makedirs(self.root)
        # Only the samples the test takes: the background loop would race the fake /proc edits.
        self.enterContext(patch.object(procs.Sampler, "_loop", lambda sampler: None))


class SeriesShape(FakeProc):
    """A sample per PID of the run, its unknown values null, foreign PIDs absent."""

    def test_own_pid_series_with_null_io_and_an_empty_list_after_exit(self):
        own_proc(self.root, 400, "run-t", 12.0, 100, 140, io=(4096, 8192))
        own_proc(self.root, 401, "run-other", 999.0, 99999, 99999, io=(1 << 30, 1 << 30))
        trace = self.trace()
        sampler = procs.Sampler("run-t", proc_root=self.root, sink=trace)
        sampler.sample()
        self.clock.advance(1.0)
        os.remove(os.path.join(self.root, "400", "io"))
        sampler.sample()
        self.clock.advance(1.0)
        shutil.rmtree(os.path.join(self.root, "400"))
        self.assertEqual(sampler.stop(), {"rss_peak_kb": 100, "cpu_s": 12.0, "hwm_kb": 140})
        self.assertEqual(trace.finish(), {"path": procs.TRACE_NAME, "status": "partial", "errors": [
            "pid 400: read_bytes unreadable", "pid 400: write_bytes unreadable"]})

        got = lines(self.path)
        self.assertEqual([g["kind"] for g in got], ["header", "sample", "sample", "sample", "footer"])
        for g in got:
            check_shape(self, g)
        self.assertEqual(got[0], {"schema": 1, "kind": "header", "wall_s": T0, **HEADER, "interval_s": 1.0})
        self.assertEqual([(g["t_s"], g["wall_s"]) for g in got[1:4]], [(0.0, T0), (1.0, T0 + 1), (2.0, T0 + 2)])
        known = {"pid": 400, "cpu_s": 12.0, "rss_kb": 100, "hwm_kb": 140}
        self.assertEqual(got[1]["processes"], [{**known, "read_bytes": 4096, "write_bytes": 8192}])
        self.assertEqual(got[2]["processes"], [{**known, "read_bytes": None, "write_bytes": None}])
        self.assertEqual(got[3]["processes"], [])
        self.assertEqual(got[4]["status"], "partial")
        self.assertEqual(got[4]["errors"], ["pid 400: read_bytes unreadable", "pid 400: write_bytes unreadable"])

    def test_every_pid_of_the_run_and_nothing_that_only_looks_like_it(self):
        own_proc(self.root, 400, "run-t", 1.0, 10, 20, io=(1, 2))
        own_proc(self.root, 402, "run-t", 2.0, 11, 21, io=(3, 4))
        # The marker of another run that begins with this run's id, and a process with no marker at all.
        own_proc(self.root, 403, "run-t2", 9.0, 99, 99, io=(9, 9))
        fake_proc(self.root, 404, ["bsl-analyzer-app", "mcp"])
        trace = self.trace()
        sampler = procs.Sampler("run-t", proc_root=self.root, sink=trace)
        sampler.sample()
        shutil.rmtree(os.path.join(self.root, "402"))
        sampler.sample()
        trace.finish()

        samples = [g["processes"] for g in lines(self.path) if g["kind"] == "sample"]
        self.assertEqual([[p["pid"] for p in procs_] for procs_ in samples], [[400, 402], [400]])
        self.assertEqual(samples[0][1], {"pid": 402, "cpu_s": 2.0, "rss_kb": 11, "hwm_kb": 21,
                                         "read_bytes": 3, "write_bytes": 4})

    def test_unknown_values_are_null_the_known_ones_stay_and_each_loss_is_named(self):
        # 410 has no VmHWM and one io value that is not a number; 411 is found by its marker
        # and gone by the time its files are read.
        own_proc(self.root, 410, "run-t", 3.0, 30, 40, io=(7, 8))
        with open(os.path.join(self.root, "410", "status"), "w") as fh:
            fh.write("Name:\tx\nVmRSS:\t30 kB\n")
        with open(os.path.join(self.root, "410", "io"), "w") as fh:
            fh.write("read_bytes: 7\nwrite_bytes: lots\n")
        own_proc(self.root, 411, "run-t", 1.0, 1, 1)
        for name in ("status", "stat"):
            os.remove(os.path.join(self.root, "411", name))
        trace = self.trace()
        procs.Sampler("run-t", proc_root=self.root, sink=trace).sample()
        traced = trace.finish()

        sample = next(g for g in lines(self.path) if g["kind"] == "sample")
        self.assertEqual(sample["processes"], [
            {"pid": 410, "cpu_s": 3.0, "rss_kb": 30, "hwm_kb": None, "read_bytes": 7, "write_bytes": None},
            {"pid": 411, "cpu_s": None, "rss_kb": None, "hwm_kb": None, "read_bytes": None, "write_bytes": None}])
        self.assertEqual(traced["status"], "partial")
        self.assertEqual(traced["errors"], [
            "pid 410: hwm_kb unreadable", "pid 410: write_bytes unreadable",
            "pid 411: cpu_s unreadable", "pid 411: rss_kb unreadable", "pid 411: hwm_kb unreadable",
            "pid 411: read_bytes unreadable", "pid 411: write_bytes unreadable"])

    def test_a_loss_that_repeats_is_listed_once(self):
        own_proc(self.root, 400, "run-t", 1.0, 10, 20)
        trace = self.trace()
        sampler = procs.Sampler("run-t", proc_root=self.root, sink=trace)
        for _ in range(30):
            sampler.sample()
            self.clock.advance(1.0)
        traced = trace.finish()
        self.assertEqual(traced["errors"], ["pid 400: read_bytes unreadable", "pid 400: write_bytes unreadable"])
        self.assertEqual(len(lines(self.path)), 1 + 30 + 1)

    def test_a_complete_trace_says_so(self):
        own_proc(self.root, 400, "run-t", 1.0, 10, 20, io=(0, 0))
        trace = self.trace()
        sampler = procs.Sampler("run-t", proc_root=self.root, sink=trace)
        sampler.stop()
        self.assertEqual(trace.finish()["status"], "complete")
        self.assertEqual(lines(self.path)[-1]["errors"], [])

    def test_without_a_sink_io_is_never_read_and_nothing_is_written(self):
        own_proc(self.root, 400, "run-t", 1.0, 10, 20, io=(1, 2))
        with patch.object(procs, "_io_bytes", side_effect=AssertionError("io read without a trace")) as io:
            sampler = procs.Sampler("run-t", proc_root=self.root)
            sampler.sample()
            self.assertEqual(sampler.stop()["hwm_kb"], 20)
        io.assert_not_called()
        self.assertEqual(os.listdir(self.tmp), ["proc"])


class SeriesFile(TempDir):
    """Lines reach the file as they are written, on the time bases they claim."""

    def test_each_line_is_on_disk_when_it_is_written(self):
        trace = self.trace()
        self.assertEqual([g["kind"] for g in lines(self.path)], ["header"])
        trace.sample([])
        trace.event("graph_ready")
        trace.phase("done", 1, T0)
        self.assertEqual([g["kind"] for g in lines(self.path)], ["header", "sample", "event", "phase"])
        trace.finish()

    def test_t_s_counts_from_the_start_of_the_trace_on_the_run_clock(self):
        self.clock.t = 50.0
        trace = self.trace()
        self.clock.t = 55.0
        trace.sample([])
        self.clock.t = 58.5
        trace.event("graph_ready")
        trace.event("call_end", 62.0, call_id="x", ok=True)
        trace.finish()
        got = lines(self.path)
        self.assertEqual([g["t_s"] for g in got if "t_s" in g], [5.0, 8.5, 12.0])
        # The Unix time of a line is the wall clock's, which the run clock never stands in for.
        self.assertEqual([g["wall_s"] for g in got[:2]], [T0 + 50.0, T0 + 55.0])

    def test_wall_s_is_unix_time_unless_given(self):
        with patch.object(procs.time, "time", return_value=T0):
            trace = procs.ResourceTrace(self.path, {"run_id": "run-t"}, self.clock)
        trace.sample([])
        trace.finish()
        self.assertEqual({g["wall_s"] for g in lines(self.path)}, {T0})

    def test_a_failed_write_ends_only_the_trace(self):
        written = []

        def wrap(fh):
            written.append(WriteFails(fh, ok=1))
            return written[-1]

        with patch("procs.open", opening_trace_as(wrap), create=True):
            trace = self.trace()
        trace.sample([])
        failing = written[0]
        # The handle is closed at once, and the lines after the failure are not tried.
        self.assertTrue(failing.closed)
        self.assertEqual(failing.attempts, 2)
        trace.event("graph_ready")
        trace.sample([])
        self.assertEqual(failing.attempts, 2)
        traced = trace.finish()
        self.assertEqual(traced["status"], "partial")
        self.assertEqual(len(traced["errors"]), 1)
        self.assertTrue(traced["errors"][0].startswith("write: "), traced)
        # What was written before the failure stays, and no footer follows it.
        self.assertEqual([g["kind"] for g in lines(self.path)], ["header"])

    def test_a_failed_close_is_in_the_footer(self):
        trace = self.trace()
        trace.sample([])
        real = trace._fh

        class BrokenClose:
            def write(self, text):
                return real.write(text)

            def flush(self):
                real.flush()

            def close(self):
                real.close()
                raise OSError(5, "Input/output error")

        trace._fh = BrokenClose()
        traced = trace.finish()
        got = lines(self.path)
        self.assertEqual([g["kind"] for g in got], ["header", "sample", "footer"])
        self.assertEqual(traced["status"], "partial")
        self.assertEqual((got[-1]["status"], got[-1]["errors"]), (traced["status"], traced["errors"]))
        self.assertTrue(traced["errors"][0].startswith("close: "), traced)

    def test_a_trace_that_cannot_be_opened_is_a_partial_one_without_a_file(self):
        self.path = os.path.join(self.tmp, "absent", procs.TRACE_NAME)
        trace = self.trace()
        trace.sample([])
        traced = trace.finish()
        self.assertEqual(traced["status"], "partial")
        self.assertEqual(len(traced["errors"]), 1)
        self.assertTrue(traced["errors"][0].startswith("open: "), traced)
        self.assertFalse(os.path.exists(self.path))


class SamplerWithoutInfluence(FakeProc):
    """What the sampler reports of the same /proc does not depend on the trace
    it feeds: not when the trace is there, nor when it loses I/O, writes or its file."""

    def scripted(self, name, sink):
        """Four samples over one fake /proc (the last one is `stop`'s own): the
        snapshots and the aggregates the sampler reports."""
        root = os.path.join(self.tmp, f"proc-{name}")
        os.makedirs(root)
        sampler = procs.Sampler("run-t", proc_root=root, sink=sink)
        own_proc(root, 400, "run-t", 12.0, 100, 140, io=(4096, 8192))
        own_proc(root, 401, "run-t", 5.0, 50, 60, io=(1, 2))
        snaps = [sampler.sample()]
        own_proc(root, 400, "run-t", 20.0, 300, 400, io=(5000, 9000))
        snaps.append(sampler.sample())
        own_proc(root, 400, "run-t", 21.0, 80, 400, io=(6000, 9500))
        shutil.rmtree(os.path.join(root, "401"))
        snaps.append(sampler.sample())
        return snaps, sampler.stop()

    def new_trace(self, name, path=None):
        trace = procs.ResourceTrace(path or os.path.join(self.tmp, f"{name}.jsonl"), HEADER, self.clock)
        self.addCleanup(trace.finish)
        return trace

    def test_the_aggregates_and_snapshots_are_the_same_with_any_trace(self):
        want = self.scripted("none", None)
        # Peak RSS of the two PIDs together (300 + 50), CPU of each PID at its last (21 + 5),
        # the highest VmHWM of either.
        self.assertEqual(want[1], {"rss_peak_kb": 350, "cpu_s": 26.0, "hwm_kb": 400})
        real_read = procs._read

        def io_denied(path):
            return None if path.endswith("/io") else real_read(path)

        self.assertEqual(self.scripted("plain", self.new_trace("plain")), want)
        with patch.object(procs, "_read", io_denied):
            self.assertEqual(self.scripted("no-io", self.new_trace("no-io")), want)
        with patch("procs.open", opening_trace_as(lambda fh: WriteFails(fh, ok=1)), create=True):
            self.assertEqual(self.scripted("write", self.new_trace("write")), want)
        gone = self.new_trace("open", os.path.join(self.tmp, "absent", "r.jsonl"))
        self.assertEqual(self.scripted("open", gone), want)


def log_line(wall_s, name, elapsed_ms, hours=3):
    zone = timezone(timedelta(hours=hours))
    stamp = datetime.fromtimestamp(wall_s, zone).isoformat(timespec="microseconds")
    return f"{stamp}  INFO graph build: {name} elapsed_ms={elapsed_ms}\n"


def first_rises(got):
    """(wall_s, t_s) of each sample whose HWM goes above every earlier one."""
    peak, rises = None, []
    for g in got:
        if g["kind"] != "sample":
            continue
        top = max((p["hwm_kb"] for p in g["processes"] if p["hwm_kb"] is not None), default=None)
        if top is not None and peak is not None and top > peak:
            rises.append((g["wall_s"], g["t_s"]))
        peak = top if peak is None else max(peak, top or 0)
    return rises


def phase_interval(got, wall_s):
    """The two adjacent known phase marks around `wall_s` (None where unbounded)."""
    marks = sorted((g["wall_s"], g["name"]) for g in got if g["kind"] == "phase" and g["wall_s"] is not None)
    before = [name for when, name in marks if when <= wall_s]
    after = [name for when, name in marks if when > wall_s]
    return (before[-1] if before else None, after[0] if after else None)


def call_at(got, t_s):
    starts = {g["call_id"]: g["t_s"] for g in got if g["kind"] == "event" and g["name"] == "call_start"}
    ends = {g["call_id"]: g["t_s"] for g in got if g["kind"] == "event" and g["name"] == "call_end"}
    return [call for call, start in starts.items() if call in ends and start <= t_s <= ends[call]]


class PhaseBounds(TempDir):
    """Growth is placed between daemon log marks by wall time and inside calls by
    monotonic time; a missing mark stays missing."""

    def write_log(self, marks):
        path = os.path.join(self.tmp, "daemon.log")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("2026-11-08T10:25:46.246818537+03:00  INFO log session started pid=1\n")
            for wall_s, name, elapsed_ms in marks:
                fh.write(log_line(wall_s, name, elapsed_ms))
        return path

    def series(self, rise_at, sweep_rise_at=210.0, marks=None):
        """Samples every 5 s from 95 to 135 and 195 to 225 (t_s = wall_s - T0),
        HWM up at `rise_at` and again at `sweep_rise_at`; the sweep runs [200, 220]."""
        trace = self.trace()
        for t in [*range(95, 140, 5), *range(195, 230, 5)]:
            self.clock.t = float(t)
            if t == 200:
                trace.event("call_start", call_id="diagnostics_workspace")
            hwm = 100 + (100 if t >= rise_at else 0) + (100 if t >= sweep_rise_at else 0)
            trace.sample([{"pid": 7, "cpu_s": 1.0, "rss_kb": 50, "hwm_kb": hwm, "read_bytes": 0, "write_bytes": 0}])
            if t == 220:
                trace.event("call_end", call_id="diagnostics_workspace", ok=True)
        marks = marks if marks is not None else [(T0 + 100, "done", 100000), (T0 + 110, "validation", 110000),
                                                 (T0 + 130, "published", 130000)]
        gate.trace_phases(trace, self.write_log(marks))
        return trace.finish(), lines(self.path)

    def test_growth_in_the_publication_tail_and_in_the_sweep(self):
        traced, got = self.series(120.0)
        self.assertEqual(traced["status"], "complete")
        for g in got:
            check_shape(self, g)
        phases = [g for g in got if g["kind"] == "phase"]
        self.assertEqual([(g["name"], g["elapsed_ms"], g["wall_s"], g["source"]) for g in phases], [
            ("done", 100000, T0 + 100, "daemon.log"), ("validation", 110000, T0 + 110, "daemon.log"),
            ("published", 130000, T0 + 130, "daemon.log")])
        self.assertTrue(all("t_s" not in g for g in phases))
        (first_wall, first_t), (second_wall, second_t) = first_rises(got)
        self.assertEqual(phase_interval(got, first_wall), ("validation", "published"))
        self.assertEqual(call_at(got, first_t), [])
        self.assertEqual(phase_interval(got, second_wall), ("published", None))
        self.assertEqual(call_at(got, second_t), ["diagnostics_workspace"])

    def test_growth_before_validation_is_not_in_the_tail(self):
        _, got = self.series(105.0)
        self.assertEqual(phase_interval(got, first_rises(got)[0][0]), ("done", "validation"))

    def test_a_missing_mark_is_a_loss_not_a_stand_in(self):
        traced, got = self.series(120.0, marks=[(T0 + 100, "done", 100000), (T0 + 130, "published", 130000)])
        self.assertEqual(traced, {"path": procs.TRACE_NAME, "status": "partial",
                                  "errors": ["daemon.log: no `validation` mark"]})
        self.assertEqual([g["name"] for g in got if g["kind"] == "phase"], ["done", "published"])
        self.assertEqual(got[-1]["status"], "partial")
        # The tail cannot be told from the projection: the bound is the outer pair.
        self.assertEqual(phase_interval(got, first_rises(got)[0][0]), ("done", "published"))

    def test_repeated_builds_keep_every_mark(self):
        marks = [(T0 + 100, "done", 100000), (T0 + 110, "validation", 110000), (T0 + 130, "published", 130000),
                 (T0 + 300, "done", 5000), (T0 + 301, "validation", 6000), (T0 + 302, "published", 7000)]
        _, got = self.series(120.0, marks=marks)
        self.assertEqual([(g["name"], g["elapsed_ms"]) for g in got if g["kind"] == "phase"],
                         [(name, ms) for _, name, ms in marks])

    def test_the_marks_as_the_daemon_writes_them_are_read(self):
        # Nanoseconds and a UTC offset, between lines of the same log that are no marks.
        path = os.path.join(self.tmp, "daemon.log")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("2026-11-08T10:19:22.585160019+03:00  INFO graph build: index batch 1/71 (first: /a/b)\n"
                     "2026-11-08T10:27:21.345590215+03:00  INFO graph build: done elapsed_ms=90717\n"
                     "2026-11-08T10:28:08.815278827+03:00  INFO graph build: validation elapsed_ms=138186\n"
                     "2026-11-08T10:28:13.403670511+03:00  INFO graph build: published elapsed_ms=142775\n"
                     "2026-11-08T10:28:14.000000000+03:00  INFO graph build finished\n")
        trace = self.trace()
        gate.trace_phases(trace, path)
        self.assertEqual(trace.finish()["status"], "complete")
        self.assertEqual([(g["name"], g["elapsed_ms"], g["wall_s"]) for g in lines(self.path) if g["kind"] == "phase"], [
            ("done", 90717, utc(2026, 11, 8, 7, 27, 21, 345590)),
            ("validation", 138186, utc(2026, 11, 8, 7, 28, 8, 815278)),
            ("published", 142775, utc(2026, 11, 8, 7, 28, 13, 403670))])

    def test_an_unreadable_time_or_log_is_partial(self):
        trace = self.trace()
        path = os.path.join(self.tmp, "daemon.log")
        with open(path, "w") as fh:
            fh.write("garbage  INFO graph build: done elapsed_ms=1\n"
                     "2026-11-08T10:27:21.345590215  INFO graph build: validation elapsed_ms=2\n"
                     + log_line(T0, "published", 3))
        gate.trace_phases(trace, path)
        traced = trace.finish()
        self.assertEqual(traced["status"], "partial")
        self.assertEqual([(g["name"], g["wall_s"]) for g in lines(self.path) if g["kind"] == "phase"],
                         [("done", None), ("validation", None), ("published", T0)])
        missing = self.trace()
        gate.trace_phases(missing, os.path.join(self.tmp, "absent.log"))
        self.assertEqual(missing.finish()["status"], "partial")


class TracedGate(GateCase):
    """The machinery of the gate-level cases: the same scripted measurements, run
    with the trace off and on."""

    wall_noise = 0.045009872812704146
    replay = None

    def script_values(self):
        if self.replay is None:
            return {}
        return {"wall": lambda run: self.replay.get((run.role, run.n), 100.0) if run.series == "A" else 100.0}

    def setUp(self):
        super().setUp()
        scripted_stop = procs.Sampler.stop

        # The fake server's own RSS and CPU are real and differ between two gates.
        def stop(sampler):
            return {**scripted_stop(sampler), "rss_peak_kb": 1 << 20, "cpu_s": 50.0}

        self.enterContext(patch.object(procs.Sampler, "stop", stop))
        # What the harness opens of a process's I/O or of the series: nothing, without the flag.
        self.opened = []
        self.opener = builtins.open

        def opening(path, *args, **kwargs):
            if str(path).endswith(("/io", procs.TRACE_NAME)):
                self.opened.append(str(path))
            return self.opener(path, *args, **kwargs)

        self.enterContext(patch("procs.open", opening, create=True))

    # Comparing field by field: what a run records of the real machine and of time.
    # The fake server ends on closed stdin or on SIGTERM, whichever comes first: 0 or -15.
    VOLATILE_RECORD = ("run_id", "load_before", "load_after", "started_at", "stop", "exit_code", "resource_trace")
    VOLATILE_SUMMARY = ("started_at", "finished_at", "runs")

    def comparable(self, summary):
        records = []
        for launch in summary["runs"]:
            record = read_json(os.path.join(launch["dir"], "r.json"))
            records.append({k: v for k, v in record.items() if k not in self.VOLATILE_RECORD})
        rest = {k: v for k, v in summary.items() if k not in self.VOLATILE_SUMMARY}
        launches = [(r["run"], r["role"], r["series"], r["held"], r["reason"]) for r in summary["runs"]]
        # The run dirs hold the same files, the series aside.
        files = [sorted(set(os.listdir(r["dir"])) - {procs.TRACE_NAME}) for r in summary["runs"]]
        return records, rest, launches, files

    def trace_files(self, summary):
        return [os.path.join(r["dir"], procs.TRACE_NAME) for r in summary["runs"]]

    def off_and_on(self, inject=None):
        """The gate without the flag, then with it (after `inject()` has made the
        trace lose something): the same exit code, records and summary."""
        code_off, off, _ = self.gate(label="off")
        self.assertEqual(self.opened, [], "I/O or the series opened without the flag")
        self.assertFalse([p for p in self.trace_files(off) if os.path.exists(p)])
        self.assertTrue(all("resource_trace" not in read_json(os.path.join(r["dir"], "r.json")) for r in off["runs"]))
        if inject is not None:
            inject()
        code_on, on, _ = self.gate("--trace-resources", label="on")
        self.assertEqual(code_on, code_off)
        self.assertEqual(self.comparable(on), self.comparable(off))
        return off, on

    def traced_run(self, role="base", series="A"):
        """One run of the gate with the trace on: its record and its series."""
        runner = gate.Runner(self.clock, self.machine, gate.check_source(self.data), self.fingerprint(), 60.0,
                             trace=True)
        binary = perf_nightly.load_binary(self.base_path if role == "base" else self.cand_path)
        run_dir = os.path.join(self.tmp, "traced", f"{series}-{role[0]}1")
        record = runner.run(run_dir, binary, role, series, 1, False, {})
        return record, lines(os.path.join(run_dir, procs.TRACE_NAME))


class NonInterference(TracedGate):
    """The trace on gives the same records and verdict as the trace off — also
    when it loses its I/O or its file — and the flag is what turns it on."""

    def test_the_flag_is_off_unless_given(self):
        self.assertFalse(self.args().trace_resources)
        self.assertTrue(self.args("gate", "g1", "HEAD", "--trace-resources").trace_resources)

    def test_trace_on_and_off_give_the_same_records_and_verdict(self):
        _, on = self.off_and_on()
        self.assertEqual(on["verdict"], "pass")
        # The positive control of "nothing opened without the flag": with it the same record sees both.
        self.assertTrue([p for p in self.opened if p.endswith("/io")])
        self.assertTrue([p for p in self.opened if p.endswith(procs.TRACE_NAME)])
        for launch, path in zip(on["runs"], self.trace_files(on)):
            got = lines(path)
            for g in got:
                check_shape(self, g)
                if g["kind"] != "phase":
                    self.assertGreater(g["wall_s"], UNIX_TIME_FLOOR)
            self.assertEqual(got[0]["kind"], "header")
            self.assertEqual(got[-1]["kind"], "footer")
            record = read_json(os.path.join(launch["dir"], "r.json"))
            self.assertEqual(got[0]["run_id"], record["run_id"])
            self.assertEqual((got[0]["role"], got[0]["series"], got[0]["attempt"]),
                             (record["role"], record["series"], record["attempt"]))
            self.assertEqual(got[0]["binary_sha"], record["binary"]["sha"])
            # The fake server writes no daemon log: every mark is a named loss, and none is made up.
            self.assertEqual(set(record["resource_trace"]), {"path", "status", "errors"})
            self.assertEqual(record["resource_trace"]["path"], procs.TRACE_NAME)
            self.assertEqual(record["resource_trace"]["status"], "partial")
            for name in gate.PHASES:
                self.assertIn(f"daemon.log: no `{name}` mark", record["resource_trace"]["errors"])
            self.assertEqual([g for g in got if g["kind"] == "phase"], [])
            self.assertEqual(got[-1]["errors"], record["resource_trace"]["errors"])
            events = [g for g in got if g["kind"] == "event"]
            self.assertEqual(events[0]["name"], "graph_ready")
            calls = [g["call_id"] for g in events if g["name"] == "call_start"]
            self.assertEqual(calls, [c["id"] for c in record["calls"]])
            self.assertEqual([g["call_id"] for g in events if g["name"] == "call_end"], calls)
            self.assertEqual([g["ok"] for g in events if g["name"] == "call_end"], [c["ok"] for c in record["calls"]])
            own = {p["pid"] for g in got if g["kind"] == "sample" for p in g["processes"]}
            self.assertTrue(own)
            self.assertTrue(own.isdisjoint({os.getpid()}))

    def test_a_failed_call_ends_with_ok_false_and_the_same_reason(self):
        # Without `graph_answers` the first resolve has no candidates list and fails the run.
        set_mode("sweep_answers", "refs_tool")
        binary = perf_nightly.load_binary(self.base_path)
        records = {}
        for trace in (False, True):
            runner = gate.Runner(self.clock, self.machine, gate.check_source(self.data), self.fingerprint(), 60.0,
                                 trace=trace)
            run_dir = os.path.join(self.tmp, f"trace-{trace}", "A-b1")
            records[trace] = runner.run(run_dir, binary, "base", "A", 1, False, {})
        for record in records.values():
            self.assertEqual((record["held"], record["reason"]), (False, "call_failed"))
        self.assertEqual(records[True]["calls"], records[False]["calls"])
        got = lines(os.path.join(self.tmp, "trace-True", "A-b1", procs.TRACE_NAME))
        ends = [(g["call_id"], g["ok"]) for g in got if g["kind"] == "event" and g["name"] == "call_end"]
        self.assertEqual(ends, [("symbol_info_1", True), ("graph_resolve", False)])
        self.assertEqual(got[-1]["kind"], "footer")

    def test_a_run_redone_under_foreign_load_and_series_b_have_their_own_trace(self):
        self.script.values["foreign"] = lambda run: 8.0 if (run.role, run.series, run.n, run.attempt) == \
            ("c", "A", 1, 1) else 0.0
        _, on = self.off_and_on()
        self.assertEqual([r["run"] for r in on["runs"][:3]], ["A-b1", "A-c1", "A-c1.2"])
        self.assertEqual(on["runs"][1]["reason"], "foreign_load")
        self.assertTrue(any(r["series"] == "B" for r in on["runs"]))
        for launch in on["runs"]:
            record = read_json(os.path.join(launch["dir"], "r.json"))
            got = lines(os.path.join(launch["dir"], procs.TRACE_NAME))
            self.assertEqual((got[0]["kind"], got[-1]["kind"]), ("header", "footer"), launch["run"])
            self.assertEqual((got[0]["series"], got[0]["attempt"]), (launch["series"], record["attempt"]))
            self.assertEqual([g["call_id"] for g in got if g["kind"] == "event" and g["name"] == "call_start"],
                             [c["id"] for c in record["calls"]])
        self.assertEqual([lines(os.path.join(r["dir"], procs.TRACE_NAME))[0]["attempt"] for r in on["runs"][1:3]],
                         [1, 2])

    def test_lost_io_does_not_change_the_outcome(self):
        real_read = procs._read

        def io_denied(path):
            return None if path.endswith("/io") else real_read(path)

        _, on = self.off_and_on(lambda: self.enterContext(patch.object(procs, "_read", io_denied)))
        record = read_json(os.path.join(on["runs"][0]["dir"], "r.json"))
        self.assertEqual(record["resource_trace"]["status"], "partial")
        self.assertTrue(any("read_bytes unreadable" in e for e in record["resource_trace"]["errors"]))
        seen = [p for g in lines(self.trace_files(on)[0]) if g["kind"] == "sample" for p in g["processes"]]
        self.assertTrue(seen)
        self.assertEqual({(p["read_bytes"], p["write_bytes"]) for p in seen}, {(None, None)})
        # What /proc did give stays.
        self.assertTrue(any(p["rss_kb"] is not None and p["cpu_s"] is not None for p in seen))

    def test_a_trace_that_cannot_be_opened_does_not_change_the_outcome(self):
        def refuse():
            def opener(path, *args, **kwargs):
                if str(path).endswith(procs.TRACE_NAME):
                    raise OSError(errno.ENOSPC, "No space left on device")
                return builtins.open(path, *args, **kwargs)

            self.opener = opener

        _, on = self.off_and_on(refuse)
        record = read_json(os.path.join(on["runs"][0]["dir"], "r.json"))
        self.assertEqual(record["resource_trace"]["status"], "partial")
        self.assertTrue(record["resource_trace"]["errors"][0].startswith("open: "))
        self.assertFalse(any(os.path.exists(p) for p in self.trace_files(on)))

    def test_a_trace_that_fails_while_the_run_goes_does_not_change_the_outcome(self):
        def fail_after_the_header():
            self.opener = opening_trace_as(lambda fh: WriteFails(fh, ok=1))

        _, on = self.off_and_on(fail_after_the_header)
        for launch, path in zip(on["runs"], self.trace_files(on)):
            record = read_json(os.path.join(launch["dir"], "r.json"))
            self.assertEqual(record["resource_trace"]["status"], "partial", launch["run"])
            self.assertTrue(any(e.startswith("write: ") for e in record["resource_trace"]["errors"]), launch["run"])
            # The header is all that was written; the run carried on to its calls.
            self.assertEqual([g["kind"] for g in lines(path)], ["header"])
            self.assertTrue(record["calls"] or not record["held"])


class TraceWiring(TracedGate):
    """What the gate hands its trace: where calls begin and end, and what the
    daemon log says."""

    def test_each_call_is_bracketed_by_its_events_and_a_sample_in_it_lies_between(self):
        samplers = []
        sampler_init = procs.Sampler.__init__
        tool = scenarios.McpRun.tool

        def init(sampler, *args, **kwargs):
            sampler_init(sampler, *args, **kwargs)
            samplers.append(sampler)

        # A scenario call takes 7 s of the run clock, and a sample is taken 4 s into it.
        def slow_tool(mcp_run, name, arguments, timeout=scenarios.CALL_TIMEOUT_S):
            if arguments.get("action") != "status":
                self.clock.advance(4.0)
                samplers[-1].sample()
                self.clock.advance(3.0)
            return tool(mcp_run, name, arguments, timeout)

        # Every line then comes from this thread, in the order of its time.
        for target, attribute, replacement in ((procs.Sampler, "_loop", lambda sampler: None),
                                               (procs.Sampler, "__init__", init),
                                               (scenarios.McpRun, "tool", slow_tool)):
            self.enterContext(patch.object(target, attribute, replacement))
        record, got = self.traced_run()

        self.assertTrue(record["held"], record["errors"])
        stamps = [g["t_s"] for g in got if "t_s" in g]
        self.assertEqual(stamps, sorted(stamps))
        events = [g for g in got if g["kind"] == "event"]
        self.assertEqual(events[0]["name"], "graph_ready")
        self.assertEqual(events[0]["t_s"], record["metrics"]["graph_ready_s"])
        starts = [g for g in events if g["name"] == "call_start"]
        ends = [g for g in events if g["name"] == "call_end"]
        calls = [c["id"] for c in record["calls"]]
        self.assertGreater(len(calls), 5)
        self.assertEqual([g["call_id"] for g in starts], calls)
        self.assertEqual([g["call_id"] for g in ends], calls)
        self.assertLess(events[0]["t_s"], starts[0]["t_s"])
        # The series holds a sample of the very instant of readiness, after the event that marks it.
        at_ready = [g for g in got if g["kind"] == "sample" and g["t_s"] == events[0]["t_s"]]
        self.assertEqual(len(at_ready), 1)
        self.assertLess(got.index(events[0]), got.index(at_ready[0]))
        for start, end in zip(starts, ends):
            self.assertAlmostEqual(end["t_s"] - start["t_s"], 7.0, msg=start["call_id"])
            inside = [g for g in got if g["kind"] == "sample" and start["t_s"] < g["t_s"] < end["t_s"]]
            self.assertEqual(len(inside), 1, start["call_id"])
            self.assertAlmostEqual(inside[0]["t_s"] - start["t_s"], 4.0)
            self.assertLess(got.index(start), got.index(inside[0]))
            self.assertLess(got.index(inside[0]), got.index(end))
            self.assertTrue(inside[0]["processes"])

    def test_the_marks_of_the_servers_log_are_phases_at_their_own_time_after_the_samples(self):
        set_mode(*MODES.split(","), "daemon_log")
        record, got = self.traced_run()

        phases = [g for g in got if g["kind"] == "phase"]
        self.assertEqual([(g["name"], g["elapsed_ms"], g["wall_s"]) for g in phases], [
            ("done", 90717, utc(2026, 11, 8, 7, 27, 21, 345590)),
            ("validation", 138186, utc(2026, 11, 8, 7, 28, 8, 815278)),
            ("published", 142775, utc(2026, 11, 8, 7, 28, 13, 403670))])
        self.assertEqual([e for e in record["resource_trace"]["errors"] if e.startswith("daemon.log")], [])
        last_sample = max(i for i, g in enumerate(got) if g["kind"] == "sample")
        self.assertGreater(got.index(phases[0]), last_sample)
        self.assertEqual(got[-1]["kind"], "footer")

    def test_a_mark_the_servers_log_lacks_is_a_loss_and_not_made_up(self):
        set_mode(*MODES.split(","), "daemon_log_no_validation")
        record, got = self.traced_run()

        self.assertEqual([g["name"] for g in got if g["kind"] == "phase"], ["done", "published"])
        self.assertEqual([e for e in record["resource_trace"]["errors"] if e.startswith("daemon.log")],
                         ["daemon.log: no `validation` mark"])
        self.assertEqual(record["resource_trace"]["status"], "partial")
        self.assertEqual(got[-1]["status"], "partial")


class ReplayBlockedWall(TracedGate):
    """The held runs of series A of the gate 0.4.4-2026-11-03, seconds to graph
    readiness per (role, number): Δ = +6.336 %, the wall regression that was
    blocked and must stay blocked, trace or not."""

    replay = {("b", 1): 147.49084236798808, ("c", 1): 151.44668673496926,
              ("b", 2): 142.42263735499, ("c", 2): 161.41684043896385,
              ("b", 3): 139.4382696429966, ("c", 3): 150.45520327298436}

    def test_the_wall_blocks_with_the_trace_on_and_off(self):
        off, on = self.off_and_on()
        for summary in (off, on):
            row = self.row(summary, "graph_ready_s")
            self.assertAlmostEqual(row["delta"], 0.06336106076653192, places=12)
            self.assertEqual(row["outcome"], "block")
            self.assertEqual(summary["verdict"], "block")


class ReplayFasterWall(TracedGate):
    """The same for 0.4.4-2026-11-03-own: Δ = −14.298 %, the wall is ok."""

    replay = {("b", 1): 205.55179182799475, ("c", 1): 145.40348594600073,
              ("b", 2): 143.43251362199953, ("c", 2): 145.4803426990038,
              ("b", 3): 169.66238021699974, ("c", 3): 134.39783272700151}

    def test_the_wall_is_ok_with_the_trace_on_and_off(self):
        off, on = self.off_and_on()
        for summary in (off, on):
            row = self.row(summary, "graph_ready_s")
            self.assertAlmostEqual(row["delta"], -0.142983342800989, places=12)
            self.assertEqual(row["outcome"], "ok")


if __name__ == "__main__":
    unittest.main()
