import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("ordinary_export", Path(__file__).with_name("export-ordinary-form-sidecar.py"))
export = importlib.util.module_from_spec(spec)
spec.loader.exec_module(export)


class OrdinaryExportTests(unittest.TestCase):
    def reader(self):
        return {"obj_version": "13", "Версия элементов формы": "0-27"}, {
            "tree": [{"name": "Panel", "type": "Panel", "page": "Main", "child": [
                {"name": "Partner", "type": "Field", "page": "Details"}]}],
            "props": [{"name": "Document"}],
            "data": {"-pages-": ["Main"], "Main/Panel": {"id": "2"},
                     "Main/Panel/Details/Partner": {"id": "3", "prop": "Document", "raw": ["uninterpreted"]}},
        }

    def test_keeps_pages_order_source_identity_and_confirmed_property(self):
        header, elements = self.reader()
        result = export.from_reader(header, elements, "Dialog", "a" * 64, "1.2.6")
        self.assertEqual(result["schema"], 3)
        self.assertEqual(result["source"]["sha256"], "a" * 64)
        self.assertEqual(result["form"]["pages"], ["Main"])
        panel = result["controls"][0]
        self.assertEqual(panel["kind"], "panel")
        self.assertEqual(panel["children"][0]["page"], "Details")
        self.assertEqual(panel["children"][0]["dataPath"], "Document")
        self.assertEqual(panel["children"][0]["dataPathCoverage"], "reader_property")
        self.assertNotIn("handlers", result["form"])
        self.assertIn("event_bindings_unread", result["inspectionLimitations"])

    def test_rejects_unknown_version_missing_data_and_duplicate_identity(self):
        header, elements = self.reader()
        with self.assertRaisesRegex(ValueError, "version"):
            export.from_reader(header, elements, "Dialog", "a" * 64, "2.0")
        elements["data"]["Main/Panel/Details/Partner"]["id"] = "2"
        with self.assertRaisesRegex(ValueError, "duplicate"):
            export.from_reader(header, elements, "Dialog", "a" * 64, "1.2.6")
        del elements["data"]["Main/Panel"]
        with self.assertRaisesRegex(ValueError, "missing"):
            export.from_reader(header, elements, "Dialog", "a" * 64, "1.2.6")

    def test_output_is_atomically_replaced_without_temporary_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "Form.json"
            output.write_text("old")
            export.write_atomic(output, {"schema": 3})
            self.assertEqual(json.loads(output.read_text()), {"schema": 3})
            self.assertEqual([p.name for p in output.parent.iterdir()], ["Form.json"])


if __name__ == "__main__":
    unittest.main()
