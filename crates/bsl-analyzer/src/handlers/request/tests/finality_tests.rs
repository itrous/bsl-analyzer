//! The pull surfaces under a baseline and a scope: the identity of every finding is fixed
//! on the file's whole set, and the scope cuts what is published only afterwards.

use lsp_types::{
    DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    PreviousResultId, TextDocumentIdentifier, WorkspaceDiagnosticParams,
    WorkspaceDiagnosticReportResult, WorkspaceDocumentDiagnosticReport,
};

use super::*;
use crate::diagnostics_baseline::finality_fixture::{published_self_assign_lines, Form, Workspace};

/// A pulled report: its result id, and its items unless the server answered `Unchanged`.
type Pulled = (String, Option<Vec<lsp_types::Diagnostic>>);

fn pull_single(state: &GlobalState, uri: &lsp_types::Url, previous: Option<&str>) -> Pulled {
    let report = handle_document_diagnostic(
        &latency_ctx(state),
        DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            identifier: None,
            previous_result_id: previous.map(str::to_owned),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    )
    .unwrap();
    match report {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => (
            full.full_document_diagnostic_report.result_id.unwrap(),
            Some(full.full_document_diagnostic_report.items),
        ),
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(unchanged)) => {
            (unchanged.unchanged_document_diagnostic_report.result_id, None)
        }
        other => panic!("unexpected report: {other:?}"),
    }
}

fn pull_workspace(state: &GlobalState, uri: &lsp_types::Url, previous: Option<&str>) -> Pulled {
    let report = handle_workspace_diagnostic(
        &latency_ctx(state),
        WorkspaceDiagnosticParams {
            identifier: None,
            previous_result_ids: previous
                .map(|value| PreviousResultId { uri: uri.clone(), value: value.to_owned() })
                .into_iter()
                .collect(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    )
    .unwrap();
    let WorkspaceDiagnosticReportResult::Report(report) = report else {
        panic!("a sweep without a partial token answers in the response")
    };
    let mut items = report.items.into_iter().filter_map(|item| match item {
        WorkspaceDocumentDiagnosticReport::Full(full) if &full.uri == uri => Some((
            full.full_document_diagnostic_report.result_id.unwrap(),
            Some(full.full_document_diagnostic_report.items),
        )),
        WorkspaceDocumentDiagnosticReport::Unchanged(unchanged) if &unchanged.uri == uri => {
            Some((unchanged.unchanged_document_diagnostic_report.result_id, None))
        }
        _ => None,
    });
    let pulled = items.next().expect("the sweep reports the module");
    assert!(items.next().is_none(), "the module is reported once");
    pulled
}

fn lines(pulled: &Pulled) -> Vec<u32> {
    published_self_assign_lines(pulled.1.as_deref().expect("a full report"))
}

/// c is the third self-assignment: 0-based line 3, after four spaces of indentation.
fn assert_is_c(pulled: &Pulled) {
    let items = pulled.1.as_deref().expect("a full report");
    let c: Vec<_> = items
        .iter()
        .filter(|d| d.code == Some(lsp_types::NumberOrString::String("SelfAssign".to_owned())))
        .collect();
    assert_eq!(c.len(), 1, "{items:#?}");
    assert_eq!((c[0].range.start.line, c[0].range.start.character), (3, 4), "{c:#?}");
}

/// The counterexample on both pull surfaces, for the v1 baseline and both partition
/// policies: recorded a and b stay hidden, c is shown with its own range, and an
/// `Unsuppressed` partition shows b and c. Without recorded entries both b and c show,
/// so an empty answer cannot pass for the hidden one.
#[test]
fn finality_scoped_baseline() {
    for form in [Form::V1, Form::Set, Form::Unsuppressed] {
        let workspace = Workspace::new(form);
        let uri = workspace.uri();
        let scope = Some(workspace.scope(Some(vec![[3, 4]])));
        let state = workspace.state(workspace.snapshot(2), scope.clone(), false);

        let single = pull_single(&state, &uri, None);
        let swept = pull_workspace(&state, &uri, None);
        if form == Form::Unsuppressed {
            assert_eq!(lines(&single), vec![3, 4], "{form:?}: single pull");
            assert_eq!(lines(&swept), vec![3, 4], "{form:?}: workspace pull");
        } else {
            assert_eq!(lines(&single), vec![4], "{form:?}: single pull");
            assert_eq!(lines(&swept), vec![4], "{form:?}: workspace pull");
            assert_is_c(&single);
            assert_is_c(&swept);
        }

        let unrecorded = workspace.state(workspace.snapshot(0), scope, false);
        assert_eq!(lines(&pull_single(&unrecorded, &uri, None)), vec![3, 4], "{form:?}");
        assert_eq!(lines(&pull_workspace(&unrecorded, &uri, None)), vec![3, 4], "{form:?}");
    }
}

/// A clean buffer is cut to the disk-derived hunks, a dirty one is analysed whole — and
/// both still under the baseline — on the single and the workspace pull alike. With a
/// recorded and only line 4 in the scope, the clean report shows c and the dirty one b
/// and c; a dirty item projected through the shared scope would lose b.
#[test]
fn finality_file_and_overlay_scope() {
    let workspace = Workspace::new(Form::V1);
    let uri = workspace.uri();
    let mut state =
        workspace.state(workspace.snapshot(1), Some(workspace.scope(Some(vec![[4, 4]]))), true);

    assert_eq!(lines(&pull_single(&state, &uri, None)), vec![4], "clean single pull");
    assert_eq!(lines(&pull_workspace(&state, &uri, None)), vec![4], "clean workspace pull");

    state.scope_dirty_docs.insert(uri.clone());
    assert_eq!(lines(&pull_single(&state, &uri, None)), vec![3, 4], "dirty single pull");
    assert_eq!(lines(&pull_workspace(&state, &uri, None)), vec![3, 4], "dirty workspace pull");
}

/// The result id is taken from the published set, after the baseline and the scope: a
/// change that only touches a hidden finding leaves the report unchanged, while a scope
/// that starts to cover c, or a snapshot that stops recording the visible b, yields a new
/// id and a full report. The first pull of each pair is a real full report whose id the
/// second one presents.
#[test]
fn finality_result_id_after_projection() {
    type Pull = fn(&GlobalState, &lsp_types::Url, Option<&str>) -> Pulled;
    for (surface, pull) in [("single", pull_single as Pull), ("workspace", pull_workspace as Pull)]
    {
        let workspace = Workspace::new(Form::V1);
        let uri = workspace.uri();
        let state = |recorded: u32, hunks: Vec<[u32; 2]>| {
            workspace.state(workspace.snapshot(recorded), Some(workspace.scope(Some(hunks))), false)
        };

        let (shown_c, items) = pull(&state(2, vec![[3, 4]]), &uri, None);
        assert_eq!(published_self_assign_lines(&items.unwrap()), vec![4], "{surface}");

        // a enters the scope, but it is recorded: nothing visible changes.
        let (same, unchanged) = pull(&state(2, vec![[2, 4]]), &uri, Some(&shown_c));
        assert_eq!(same, shown_c, "{surface}: a hidden change must keep the id");
        assert!(unchanged.is_none(), "{surface}: and answer Unchanged");

        // The snapshot stops recording b, which is in the scope: b becomes visible.
        let (changed, full) = pull(&state(1, vec![[3, 4]]), &uri, Some(&shown_c));
        assert_ne!(changed, shown_c, "{surface}: a visible change must change the id");
        assert_eq!(published_self_assign_lines(&full.unwrap()), vec![3, 4], "{surface}");

        // The scope starts to cover c.
        let (nothing, items) = pull(&state(2, vec![[2, 3]]), &uri, None);
        assert!(published_self_assign_lines(&items.unwrap()).is_empty(), "{surface}");
        let (covered, full) = pull(&state(2, vec![[2, 4]]), &uri, Some(&nothing));
        assert_ne!(covered, nothing, "{surface}: the scope reaching c must change the id");
        assert_eq!(published_self_assign_lines(&full.unwrap()), vec![4], "{surface}");
    }
}

/// Without a project root there is no baseline to apply, but the computation still held
/// the whole file, so both pulls cut it to the scope: b and c, never a. Without a scope the
/// same state shows all three.
#[test]
fn finality_missing_root_still_projects_scope() {
    let workspace = Workspace::new(Form::V1);
    let uri = workspace.uri();
    let mut state =
        workspace.state(workspace.snapshot(2), Some(workspace.scope(Some(vec![[3, 4]]))), false);
    state.workspace_root = None;
    assert_eq!(lines(&pull_single(&state, &uri, None)), vec![3, 4], "single pull");
    assert_eq!(lines(&pull_workspace(&state, &uri, None)), vec![3, 4], "workspace pull");

    let mut unscoped = workspace.state(workspace.snapshot(2), None, false);
    unscoped.workspace_root = None;
    assert_eq!(lines(&pull_single(&unscoped, &uri, None)), vec![2, 3, 4], "control");
    assert_eq!(lines(&pull_workspace(&unscoped, &uri, None)), vec![2, 3, 4], "control");
}
