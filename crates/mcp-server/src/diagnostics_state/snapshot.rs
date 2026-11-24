use std::path::Path;
use std::sync::Arc;

use super::lifecycle::{lock_inner, DiagnosticsState};
use super::types::DiagnosticsStatus;

/// The outcome of one [`DiagnosticsState::try_snapshot_once`] pass.
enum SnapshotAttempt {
    /// The resident served text + shared parse.
    Fetched((Arc<str>, syntax::Parse<syntax::SyntaxNode>)),
    /// Definitively unserveable this call — no retry. Covers no resident / a non-resident path,
    /// and an UNEXPECTED read panic (already logged at error): a genuine invariant bug will not
    /// clear on a retry, so it degrades straight to the caller's disk read.
    Unavailable,
    /// A read unwound on an EXPECTED drift race (a `salsa::Cancelled`, which a disk read that
    /// drifted from its revision raises too, or a `file_text` disk-read panic). The caller
    /// retries once on a fresh snapshot.
    Unwound,
}

/// Classify a caught unwind from an unlocked resident snapshot read. Two payloads are EXPECTED on
/// the drift hot path and logged at debug: a `salsa::Cancelled` (a concurrent `set_file_text`
/// cancelled the cloned handle's in-flight query, or the file's bytes changed between the recorded
/// revision and this disk re-read — `file_text_query` unwinds that as `PendingWrite`) and
/// `file_text_query`'s own disk-read panic (the file was deleted or became unreadable). ANY OTHER
/// payload is a real invariant bug, not a drift race, so it is logged at error with its message —
/// never masked as a race — while the read still degrades to `Unavailable` rather than taking down
/// the query thread.
///
/// The disk-read panic also fires the process-global panic hook, printing a backtrace to stderr.
/// Swapping the hook (`set_hook`/`update_hook`) is process-global and would race other threads'
/// panics, so it is deliberately NOT done here: the stderr backtrace is accepted as expected noise
/// on that race. The revision drift is unwound without the hook.
fn classify_snapshot_unwind(
    path: &Path,
    payload: Box<dyn std::any::Any + Send>,
) -> SnapshotAttempt {
    if payload.is::<salsa::Cancelled>() {
        tracing::debug!(?path, "resident snapshot read cancelled (drift race); retrying once");
        return SnapshotAttempt::Unwound;
    }
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied());
    match message {
        Some(msg) if msg.starts_with("file_text") => {
            tracing::debug!(
                ?path,
                %msg,
                "resident snapshot read unwound (drift race); retrying once"
            );
            SnapshotAttempt::Unwound
        }
        Some(msg) => {
            tracing::error!(
                ?path,
                %msg,
                "resident snapshot read panicked unexpectedly; treating as unavailable"
            );
            SnapshotAttempt::Unavailable
        }
        None => {
            tracing::error!(
                ?path,
                "resident snapshot read panicked with a non-string payload; treating as unavailable"
            );
            SnapshotAttempt::Unavailable
        }
    }
}

/// Adapts the resident [`DiagnosticsState`] to the search index's
/// [`bsl_search::ModuleSnapshotSource`] port, so the overlay's incremental reindex chunks the
/// shared resident parse instead of reading and re-parsing the file itself. Cheap to clone (the
/// state is `Arc`-backed). Its `text_and_parse` takes the resident lock only briefly and never
/// while the search engine lock is held (the caller prefetches off-lock).
#[derive(Clone)]
pub(crate) struct ResidentModuleSnapshotSource {
    diagnostics: DiagnosticsState,
}

impl ResidentModuleSnapshotSource {
    pub(crate) fn new(diagnostics: DiagnosticsState) -> Self {
        Self { diagnostics }
    }
}

impl bsl_search::ModuleSnapshotSource for ResidentModuleSnapshotSource {
    fn text_and_parse(&self, path: &str) -> bsl_search::SnapshotFetch {
        match self.diagnostics.snapshot_text_and_parse(Path::new(path)) {
            Some((text, parse)) => bsl_search::SnapshotFetch::Fetched(bsl_search::ModuleSnapshot {
                text,
                root: parse.syntax_node(),
            }),
            None => bsl_search::SnapshotFetch::Unavailable,
        }
    }

    fn catch_up(&self) {
        self.diagnostics.poll_pending_drift();
    }
}

impl DiagnosticsState {
    /// A lock-free snapshot of a resident file's text and shared parse, for the search index's
    /// incremental reindex.
    ///
    /// Resolves the path and clones the db handle under a BRIEF lock, then reads `file_text`
    /// and `parse` OUTSIDE the lock. Those reads run on a cloned Salsa handle that shares the
    /// resident storage, so a concurrent drift `set_file_text` on another thread can cancel
    /// them (`salsa::Cancelled`) or — for a disk-backed file whose bytes changed between the
    /// recorded revision and this read — panic inside `file_text_query`'s revision assert. Both
    /// unwind. Catching them here is sound: the cloned handle is discarded on unwind, a read
    /// never mutates the resident master db, and no half-built state escapes — which is why the
    /// `AssertUnwindSafe` wrapper is justified. A caught unwind retries ONCE on a fresh
    /// snapshot, then returns `None`. `None` also covers a resident that is absent / loading /
    /// evicted or a path that is not resident (the caller then disk-reads instead). Never
    /// forces a resident build and never touches drift state.
    pub(crate) fn snapshot_text_and_parse(
        &self,
        path: &Path,
    ) -> Option<(Arc<str>, syntax::Parse<syntax::SyntaxNode>)> {
        let _disk_read = self.disk_gate.try_read().ok()?;
        match self.try_snapshot_once(path) {
            SnapshotAttempt::Fetched(pair) => Some(pair),
            SnapshotAttempt::Unavailable => None,
            SnapshotAttempt::Unwound => match self.try_snapshot_once(path) {
                SnapshotAttempt::Fetched(pair) => Some(pair),
                SnapshotAttempt::Unavailable | SnapshotAttempt::Unwound => None,
            },
        }
    }

    /// One resolve+read attempt for [`Self::snapshot_text_and_parse`]. `Unavailable` is
    /// definitive (no resident / not a resident file) and must not retry; `Unwound` is a
    /// transient cancellation/revision race the caller retries once.
    fn try_snapshot_once(&self, path: &Path) -> SnapshotAttempt {
        let (analysis, file_id) = {
            let inner = lock_inner(&self.inner);
            if !matches!(inner.status, DiagnosticsStatus::Ready { .. }) {
                return SnapshotAttempt::Unavailable;
            }
            let Some(resident) = inner.resident.as_ref() else {
                return SnapshotAttempt::Unavailable;
            };
            let Some(file_id) = resident.file_id_for(path) else {
                return SnapshotAttempt::Unavailable;
            };
            (resident.analysis(), file_id)
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let text = analysis.file_text_arc(file_id);
            let parse = analysis.parse(file_id);
            (text, parse)
        })) {
            Ok(pair) => SnapshotAttempt::Fetched(pair),
            Err(payload) => classify_snapshot_unwind(path, payload),
        }
    }

    /// Reconcile pending workspace drift for the search prefetch: the cheap event-driven drain
    /// (or, with no/degraded hub, the throttled scan) that a diagnostics read runs before it
    /// serves, so a snapshot read taken right after a file edit sees fresh resident text instead
    /// of the stale pre-edit content. Takes only the resident lock (respecting the invariant that
    /// it is never nested under the search engine lock); a full rebuild in flight is skipped by
    /// the drain path, so this never blocks a query on a rebuild.
    pub(crate) fn poll_pending_drift(&self) {
        let Ok(_disk_read) = self.disk_gate.try_read() else { return };
        self.poll_drift();
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{module_path, sample_workspace, wait_ready};
    use super::*;

    /// A resident write racing an unlocked snapshot read: the file changes on disk while the
    /// resident still records the OLD content revision (no drift poll re-keyed it), so the
    /// cloned `file_text` read trips `verify_disk_revision` and unwinds. `snapshot_text_and_parse`
    /// must catch it, retry once on a fresh snapshot, and return `None` — never propagate the
    /// panic. Reverting the `catch_unwind` in `try_snapshot_once` makes this test panic.
    #[test]
    fn snapshot_returns_none_on_revision_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let path = module_path(root, "Сервер");
        std::fs::write(&path, "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции")
            .unwrap();

        assert!(
            state.snapshot_text_and_parse(&path).is_none(),
            "a revision-drift read must degrade to None, not panic"
        );
    }

    /// The panic classifier separates an EXPECTED drift race (a pending write or a `file_text`
    /// disk-read panic, which the caller retries once) from a genuine invariant bug (any other
    /// payload, which degrades straight to unavailable and is logged at error, never masked as a
    /// race).
    #[test]
    fn snapshot_unwind_classification_separates_drift_from_bugs() {
        let path = Path::new("/ws/M.bsl");

        let drift: Box<dyn std::any::Any + Send> = Box::new(salsa::Cancelled::PendingWrite);
        assert!(
            matches!(classify_snapshot_unwind(path, drift), SnapshotAttempt::Unwound),
            "a disk read that drifted from its revision is an expected drift race → retry"
        );

        let unreadable: Box<dyn std::any::Any + Send> = Box::new(
            "file_text: cannot read \"/ws/M.bsl\" for FileId(1): No such file or directory"
                .to_owned(),
        );
        assert!(
            matches!(classify_snapshot_unwind(path, unreadable), SnapshotAttempt::Unwound),
            "a file_text disk-read panic is an expected drift race → retry"
        );

        let bug: Box<dyn std::any::Any + Send> = Box::new("index out of bounds: len 0".to_owned());
        assert!(
            matches!(classify_snapshot_unwind(path, bug), SnapshotAttempt::Unavailable),
            "an unrelated string panic is a real bug → unavailable, not a retried race"
        );

        let opaque: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert!(
            matches!(classify_snapshot_unwind(path, opaque), SnapshotAttempt::Unavailable),
            "a non-string payload is treated as unavailable, never as a drift race"
        );
    }

    /// An unbuilt (`Idle`) or `Disabled` resident yields `None` immediately and never forces a
    /// build, so a single-file reindex degrades to the caller's own disk read.
    #[test]
    fn snapshot_none_when_resident_unbuilt_or_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let path = module_path(root, "Сервер");

        let idle = DiagnosticsState::for_workspace(root.to_path_buf());
        assert!(idle.snapshot_text_and_parse(&path).is_none());
        assert!(
            matches!(idle.status(), DiagnosticsStatus::Idle),
            "a snapshot read must not kick a resident build"
        );

        let disabled = DiagnosticsState::disabled();
        assert!(disabled.snapshot_text_and_parse(&path).is_none());
    }

    /// The happy path: a Ready resident serves verbatim text and a parse whose chunk output
    /// matches a plain disk read+parse, so the overlay can safely chunk the shared tree.
    #[test]
    fn snapshot_serves_text_and_parse_matching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.ensure_loading();
        wait_ready(&state);

        let path = module_path(root, "Сервер");
        let (text, parse) =
            state.snapshot_text_and_parse(&path).expect("Ready resident must serve the file");

        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.as_ref(), disk, "resident text must be byte-verbatim");

        let via_shared = bsl_search::Chunker::chunk_parsed(&parse.syntax_node(), &text);
        let via_disk = bsl_search::Chunker::chunk(&disk);
        assert_eq!(via_shared.len(), via_disk.len());
        for (a, b) in via_shared.iter().zip(&via_disk) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.is_export, b.is_export);
            assert_eq!(a.annotations, b.annotations);
            assert_eq!(a.line_start, b.line_start);
            assert_eq!(a.line_end, b.line_end);
            assert_eq!(a.text, b.text);
        }
    }
}
