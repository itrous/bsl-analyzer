"""Working config, snapshot verification (И7) and call points (И8)."""

import os
import subprocess
import tomllib
import unittest

from helpers import MODULE, Workspace, run_git

import corpus
from common import HarnessError

HELP = {"source": "external", "path": "/data/help/pkg"}


class EditConfig(unittest.TestCase):
    original = ('# comment kept\r\n[source]\r\nroot = "src"\r\n\r\n[analysis]\r\ndiff_base = "vendor"\r\n'
                'other = 1\r\n\r\n[platform_help]\r\nsource = "auto"\r\n\r\n[features]\r\nx = "y"\r\n')

    def test_full_drops_diff_base_and_replaces_help_keeping_the_rest_verbatim(self):
        text = corpus.edit_config(self.original, HELP, "full", None)
        corpus.check_variant(self.original, text, HELP, "full", None)
        self.assertNotIn("diff_base", text)
        self.assertNotIn('source = "auto"', text)
        self.assertTrue(text.startswith('# comment kept\r\n[source]\r\nroot = "src"\r\n'))
        self.assertIn('[features]\r\nx = "y"\r\n', text)
        self.assertNotIn("\n", text.replace("\r\n", ""), "line endings stay CRLF")
        self.assertEqual(tomllib.loads(text)["platform_help"], HELP)

    def test_diff_sets_the_ref_in_an_existing_or_a_new_analysis_section(self):
        text = corpus.edit_config(self.original.replace('"vendor"', '"main"'), HELP, "diff", "vendor")
        self.assertEqual(tomllib.loads(text)["analysis"], {"diff_base": "vendor", "other": 1})
        bare = '[source]\nroot = "src"\n'
        text = corpus.edit_config(bare, HELP, "diff", "vendor")
        self.assertEqual(tomllib.loads(text)["analysis"], {"diff_base": "vendor"})
        self.assertTrue(text.startswith(bare))

    def test_builtin_variant_has_no_help_section(self):
        text = corpus.edit_config(self.original, None, "full", None)
        self.assertNotIn("platform_help", tomllib.loads(text))

    def test_a_nested_array_line_is_not_a_section_header(self):
        text = '[analysis]\nexclude = [\n  ["src"],\n  ["x"]\n]\ndiff_base = "vendor"\n'
        out = corpus.edit_config(text, None, "full", None)
        corpus.check_variant(text, out, None, "full", None)
        self.assertNotIn("diff_base", out)

    def test_comments_after_the_help_section_are_kept(self):
        text = '[platform_help]\nsource = "none"\n# Анализ\n[analysis]\nx = 1\n'
        out = corpus.edit_config(text, HELP, "full", None)
        self.assertIn("# Анализ\n[analysis]\n", out)
        self.assertNotIn('"none"', out)

    def test_the_search_baseline_is_dropped_with_its_subtables(self):
        text = ('[search]\nmode = "x"\n\n[search.baseline]\nbackend = "postgres"\n\n'
                '[search.baseline.postgres.credential_helper]\nargs = [\n  "vault",\n]\n\n'
                '[[search.baseline.workspace_code.policy.branches]]\nmatch = "vendor"\n\n'
                '# kept\n[features]\nx = 1\n')
        out = corpus.edit_config(text, HELP, "full", None)
        corpus.check_variant(text, out, HELP, "full", None)
        parsed = tomllib.loads(out)
        self.assertEqual(parsed["search"], {"mode": "x"})
        self.assertEqual(parsed["features"], {"x": 1})
        self.assertIn("# kept\n[features]", out)

    def test_quoted_and_spaced_table_names_are_recognised(self):
        text = ('[search."baseline"]\nbackend = "postgres"\n\n[ search . \'baseline\' . postgres ]\nhost = "h"\n\n'
                '["analysis"]\ndiff_base = "vendor"\nx = 1\n')
        out = corpus.edit_config(text, HELP, "full", None)
        corpus.check_variant(text, out, HELP, "full", None)
        self.assertNotIn("search", tomllib.loads(out))
        self.assertEqual(tomllib.loads(out)["analysis"], {"x": 1})

    def test_a_quoted_part_with_a_dot_is_one_key(self):
        text = '[search."baseline.x"]\nmode = "y"\n\n[search.baseline]\nbackend = "postgres"\n'
        out = corpus.edit_config(text, HELP, "full", None)
        corpus.check_variant(text, out, HELP, "full", None)
        self.assertEqual(tomllib.loads(out)["search"], {"baseline.x": {"mode": "y"}})

    def test_check_variant_refuses_a_surviving_search_baseline(self):
        text = '[search.baseline]\nbackend = "postgres"\n'
        with self.assertRaisesRegex(HarnessError, "search.baseline"):
            corpus.check_variant(text, text + '\n[platform_help]\nsource = "external"\npath = "/data/help/pkg"\n',
                                 HELP, "full", None)

    def test_check_variant_refuses_an_edit_that_touched_other_keys(self):
        text = corpus.edit_config(self.original, HELP, "full", None).replace('x = "y"', 'x = "z"')
        with self.assertRaises(HarnessError):
            corpus.check_variant(self.original, text, HELP, "full", None)


class Manifest(unittest.TestCase):
    def test_a_duplicate_corpus_id_is_refused(self):
        import tempfile
        entry = '[[corpus]]\nid = "ut"\nlabel = "x"\nsource_repo = "/r"\ncommit = "{c}"\nmode = "full"\n'
        with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as fh:
            fh.write('[platform_help]\nsource = "external"\npath = "/h"\n'
                     + entry.format(c="a" * 40) + entry.format(c="b" * 40))
        self.addCleanup(os.remove, fh.name)
        with self.assertRaisesRegex(HarnessError, "twice"):
            corpus.load_manifest(fh.name)


class Positions(unittest.TestCase):
    def test_utf16_column_counts_the_bom_on_the_first_line_only(self):
        data = "﻿А Тест\r\nБв Тест".encode()
        first = data.index("Тест".encode())
        second = data.rindex("Тест".encode())
        self.assertEqual(corpus.lsp_position(data, first), (0, 3))
        self.assertEqual(corpus.lsp_position(data, second), (1, 3))

    def test_identifier_start_check(self):
        data = MODULE.encode()
        start = data.index("Тест".encode())
        self.assertTrue(corpus.check_ident_start(data, start))
        self.assertFalse(corpus.check_ident_start(data, start + len("Т".encode())))
        self.assertFalse(corpus.check_ident_start(data, start + 1), "inside a UTF-8 sequence")

    def test_select_points_takes_the_lowest_id_of_a_feature_and_class(self):
        doc = {"points": [{"id": "document_symbol/common_small/02", "relative_path": "b"},
                          {"id": "document_symbol/common_large/01", "relative_path": "a"},
                          {"id": "document_symbol/form_large/03", "relative_path": "c"}]}
        chosen = corpus.select_points(doc)
        self.assertEqual(chosen["F_small"]["relative_path"], "b")
        self.assertEqual([p["relative_path"] for p in chosen["F_rot"]], ["a", "b", "c"])
        self.assertNotIn("Q", chosen)


class SnapshotVerify(Workspace):
    def diff_snapshot(self):
        vendor = subprocess.run(["git", "rev-parse", "vendor"], cwd=self.source, check=True,
                                capture_output=True, text=True).stdout.strip()
        manifest = {"platform_help": self.manifest["platform_help"], "corpora": {"ut": {
            **self.manifest["corpora"]["crm"], "id": "ut", "mode": "diff",
            "diff_base_ref": "vendor", "diff_base_sha": vendor}}}
        meta = corpus.make_snapshot(self.data, manifest, "ut")
        snap = self.data.snapshot("ut")
        corpus.prepare_snapshot(snap, meta, "help")
        return snap, meta

    def test_moved_diff_base_ref_is_refused(self):
        snap, meta = self.diff_snapshot()
        run_git("branch", "-f", "vendor", "HEAD", cwd=snap)
        run_git("commit", "-q", "--allow-empty", "-m", "x", cwd=snap)
        run_git("branch", "-f", "vendor", "HEAD", cwd=snap)
        run_git("checkout", "-q", "--detach", meta["commit"], cwd=snap)
        with self.assertRaisesRegex(HarnessError, "diff_base"):
            corpus.verify_snapshot(snap, meta, "help")

    def test_an_extra_tracked_change_is_refused(self):
        with open(os.path.join(self.snap, "Модули", "Малый.bsl"), "a", encoding="utf-8") as fh:
            fh.write("// x\n")
        with self.assertRaisesRegex(HarnessError, "changed besides the config"):
            corpus.verify_snapshot(self.snap, self.meta, "help")

    def test_the_config_of_another_variant_is_refused(self):
        corpus.prepare_snapshot(self.snap, self.meta, "builtin")
        with self.assertRaisesRegex(HarnessError, "`help` variant"):
            corpus.verify_snapshot(self.snap, self.meta, "help")

    def test_head_off_the_manifest_commit_is_refused(self):
        run_git("commit", "-q", "--allow-empty", "-m", "x", cwd=self.snap)
        with self.assertRaisesRegex(HarnessError, "HEAD"):
            corpus.verify_snapshot(self.snap, self.meta, "help")


class PointsVerify(Workspace):
    def test_a_changed_byte_in_a_point_file_is_refused(self):
        chosen = corpus.select_points(corpus.load_points(self.data, "crm")[0])
        corpus.verify_points(self.snap, chosen)
        path = os.path.join(self.snap, chosen["F"]["relative_path"])
        with open(path, "r+b") as fh:
            fh.seek(len(MODULE.encode()) - 2)
            fh.write(b"X")
        with self.assertRaisesRegex(HarnessError, "file hash differs"):
            corpus.verify_points(self.snap, chosen)

    def test_an_offset_inside_a_word_is_refused(self):
        chosen = corpus.select_points(corpus.load_points(self.data, "crm")[0])
        chosen["P_hover"] = {**chosen["P_hover"], "offset": chosen["P_hover"]["offset"] + 2}
        with self.assertRaisesRegex(HarnessError, "identifier start"):
            corpus.verify_points(self.snap, chosen)


if __name__ == "__main__":
    unittest.main()
