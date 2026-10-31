"""Таблица фаз пробы: стены фаз по heartbeat-строкам daemon.log и сэмплам."""

import os
import tempfile
import unittest

import graph_ready_probe as probe


class PhaseTableTests(unittest.TestCase):
    def test_phases_are_cut_at_heartbeat_boundaries_and_end_at_ready(self):
        with tempfile.TemporaryDirectory() as out:
            with open(os.path.join(out, "daemon.log"), "w") as fh:
                fh.write("2026-10-30T10:00:05.000+03:00 INFO graph build: index 0/2 first\n")
                fh.write("2026-10-30T10:00:05.500+03:00 INFO graph build: index 1/2 second\n")
                fh.write("2026-10-30T10:00:15.000+03:00 INFO graph build: method_nodes\n")
                fh.write("2026-10-30T10:00:18.000+03:00 INFO graph build: done\n")
                fh.write("2026-10-30T10:00:28.000+03:00 INFO graph build: validation\n")
            rows = [
                (1000.0 + i, f"2026-10-30T10:00:{i:02d}.000+03:00", float(i) * 2, 2 if i < 10 else 1, 1024 * i)
                for i in range(0, 35)
            ]
            table = probe.phase_table(out, 1000.0, 30.0, rows)
        names = [t["phase"] for t in table]
        self.assertEqual(names, ["pre-build", "index", "method_nodes", "done", "validation→ready"])
        walls = {t["phase"]: t["wall_s"] for t in table}
        self.assertEqual(walls["pre-build"], 5.0)
        self.assertEqual(walls["index"], 10.0)
        self.assertEqual(walls["method_nodes"], 3.0)
        self.assertEqual(walls["done"], 10.0)
        self.assertEqual(walls["validation→ready"], 2.0)
        self.assertEqual(sum(walls.values()), 30.0)
        index = next(t for t in table if t["phase"] == "index")
        self.assertEqual(index["cpu_s"], 20.0)
        self.assertEqual(index["rss_max_mib"], 14)

    def test_no_heartbeat_means_no_table(self):
        with tempfile.TemporaryDirectory() as out:
            open(os.path.join(out, "daemon.log"), "w").close()
            self.assertEqual(probe.phase_table(out, 0.0, 10.0, [(0.0, "2026-10-30T10:00:00.000+03:00", 0.0, 0, 0)]), [])


if __name__ == "__main__":
    unittest.main()
