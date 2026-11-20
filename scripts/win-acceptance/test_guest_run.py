"""Host-side tests for the row oracle of guest_run.py (python3 -m pytest scripts/win-acceptance)."""
import importlib.util
import pathlib

SPEC = importlib.util.spec_from_file_location("guest_run", pathlib.Path(__file__).with_name("guest_run.py"))
guest_run = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guest_run)

VALID = {"status": "valid", "valid": True, "cleanup_status": "complete",
         "contexts_required": ["server"], "contexts_checked": ["server"],
         "compiler": {"compatibility": "Version8_3_27"}, "issues": []}


def row(expected, fixture="M"):
    return {"id": "r", "fixture": fixture, "expected": expected}


def test_valid_row_passes():
    assert guest_run.evaluate(row({"status": "valid"}), VALID) == []


def test_status_mismatch_is_reported():
    assert guest_run.evaluate(row({"status": "invalid"}), VALID)


def test_unchecked_context_fails():
    result = dict(VALID, contexts_checked=[])
    assert any("contexts" in p for p in guest_run.evaluate(row({"status": "valid"}), result))


def test_incomplete_cleanup_fails():
    result = dict(VALID, cleanup_status="partial")
    assert any("cleanup" in p for p in guest_run.evaluate(row({"status": "valid"}), result))


def test_wrong_compatibility_fails():
    assert any("compatibility" in p for p in guest_run.evaluate(row({"status": "valid"}, "L"), VALID))


def test_position_must_match_an_issue():
    result = dict(VALID, status="invalid", valid=False, issues=[{"line": 2, "column": 8}])
    assert guest_run.evaluate(row({"status": "invalid", "position": [2, 8]}), result) == []
    assert guest_run.evaluate(row({"status": "invalid", "position": [1, 4]}), result)


def test_status_one_of():
    assert guest_run.evaluate(row({"status_one_of": ["valid", "invalid"]}), VALID) == []


def test_context_required_needs_no_compiler_fields():
    assert guest_run.evaluate(row({"status": "context_required"}), {"status": "context_required"}) == []


def test_snapshot_diff_detects_transient_file(tmp_path):
    (tmp_path / "1Cv8.1CD").write_bytes(b"db")
    before = guest_run.snapshot(tmp_path)
    (tmp_path / "1Cv8.1CD.cfl").write_bytes(b"")
    delta = guest_run.diff(before, guest_run.snapshot(tmp_path))
    assert delta["added"] == ["1Cv8.1CD.cfl"] and not delta["changed"]
    (tmp_path / "1Cv8.1CD").write_bytes(b"dB")
    assert guest_run.diff(before, guest_run.snapshot(tmp_path))["changed"] == ["1Cv8.1CD"]


def test_caller_refusal_checks_backend_and_failure_code():
    expected = {"status": "context_required", "failure_code": "context_required", "backend": "caller/1"}
    refused = {"status": "context_required", "backend": "caller/1", "failure": {"code": "context_required"}}
    assert guest_run.evaluate(row(expected), refused) == []
    assert guest_run.evaluate(row(expected), dict(refused, backend="bsl-native-check/0.2.85"))
