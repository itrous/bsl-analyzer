use super::FusedChunkWriter;
use bsl_search::SearchEngine;
use ide::FusedChunkSink;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn capture(f: impl FnOnce()) -> Vec<Value> {
    use tracing_subscriber::prelude::*;
    struct Capture(Arc<Mutex<Vec<Value>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor<'a>(&'a mut Vec<Value>);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
                fn record_str(&mut self, field: &tracing::field::Field, text: &str) {
                    if field.name() == "record" {
                        self.0.push(serde_json::from_str(text).unwrap());
                    }
                }
            }
            if event.metadata().target() == bsl_search::lifecycle::TARGET {
                event.record(&mut Visitor(&mut self.0.lock().unwrap()));
            }
        }
    }
    let records = Arc::new(Mutex::new(Vec::new()));
    test_utils::with_subscriber(tracing_subscriber::registry().with(Capture(records.clone())), f);
    Arc::try_unwrap(records).unwrap().into_inner().unwrap()
}

#[test]
fn vector_lifecycle_capture_survives_first_callsite_on_unsubscribed_thread() {
    const CHILD: &str = "BSL_MCP_LIFECYCLE_CAPTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "graph::build::vector_lifecycle_tests::vector_lifecycle_capture_survives_first_callsite_on_unsubscribed_thread"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        // A drifted name would select no test and still exit successfully.
        assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}");
        return;
    }
    let probe = || {
        bsl_search::lifecycle::Record::new(
            Path::new("fixture.db"),
            "capture_probe",
            bsl_search::lifecycle::Reason::Startup,
        )
        .emit(true);
    };
    let records = capture(|| {
        std::thread::spawn(probe).join().unwrap();
        probe();
    });
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["kind"], "capture_probe");
}

fn emit(engine: &mut SearchEngine, source: &Path, rows: &[ide::ChunkRow]) {
    let mut writer = FusedChunkWriter::new(
        engine,
        source.to_owned(),
        crate::workspace_lease::WorkspaceLease::unmanaged(),
    );
    writer.emit_chunks(rows).unwrap();
    writer.finish(bsl_search::lifecycle::Outcome::Completed);
}

fn row(path: &Path, symbol: &str) -> ide::ChunkRow {
    ide::ChunkRow {
        path: path.to_string_lossy().replace('\\', "/"),
        symbol: symbol.into(),
        kind: bsl_search::ChunkKind::Procedure,
        is_export: false,
        annotations: Vec::new(),
        line_start: 1,
        line_end: 2,
        text: "canary source text must not enter lifecycle records".into(),
        graph_context: None,
    }
}

fn seed_with_roots(source: &Path, install_roots: bool) -> (SearchEngine, Vec<ide::ChunkRow>) {
    let file = source.join("Module.bsl");
    std::fs::write(&file, "Процедура Делать()\nКонецПроцедуры").unwrap();
    let file = file.canonicalize().unwrap();
    let rows = ["A", "B"].into_iter().map(|name| row(&file, name)).collect::<Vec<_>>();
    let mut engine = SearchEngine::fts_only(&source.join("search.db")).unwrap();
    if install_roots {
        let (roots, rejected) = bsl_search::WorkspaceRoots::build(source, source, &[]);
        assert!(rejected.is_empty());
        engine.set_workspace_roots(roots);
        assert!(engine.workspace_roots().is_some(), "the root-table branch must be exercised");
    }
    emit(&mut engine, source, &rows);
    let pending = engine.store().load_pending_embedding_documents("code").unwrap();
    assert_eq!(pending.len(), 2);
    engine.store().set_chunk_embedding(pending[0].0, &[0.1, 0.2]).unwrap();
    (engine, rows)
}

fn seed(source: &Path) -> (SearchEngine, Vec<ide::ChunkRow>) {
    seed_with_roots(source, false)
}

fn rootless_source(dir: &tempfile::TempDir) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let canonical = dir.path().join("canonical-cf");
        let declared = dir.path().join("declared-cf");
        std::fs::create_dir(&canonical).unwrap();
        std::os::unix::fs::symlink(&canonical, &declared).unwrap();
        assert_ne!(declared.canonicalize().unwrap(), declared);
        declared
    }
    #[cfg(not(unix))]
    {
        dir.path().to_path_buf()
    }
}

fn assert_read_error_preserves_rows_and_vectors(
    source: &Path,
    mut engine: SearchEngine,
    rows: &[ide::ChunkRow],
) {
    let before_rows = engine.store().load_indexed_documents(Some("code")).unwrap();
    let before_files = engine.store().all_files_in_collection("code").unwrap();
    let before_vectors = engine.store().load_all_embeddings(2).unwrap();
    let generation = engine.store().embedding_generation().unwrap();
    std::fs::remove_file(source.join("Module.bsl")).unwrap();

    let records = capture(|| emit(&mut engine, source, rows));
    let decision = records
        .iter()
        .find(|record| record["kind"] == "decision")
        .unwrap_or_else(|| panic!("missing decision in lifecycle records: {records:#?}"));
    assert_eq!(decision["reason"], "read_error", "lifecycle records: {records:#?}");
    assert_eq!(decision["examples"], serde_json::json!([":Module.bsl"]));
    assert!(!records.iter().any(|record| record["kind"] == "mutation"));
    assert_eq!(engine.store().load_indexed_documents(Some("code")).unwrap(), before_rows);
    assert_eq!(engine.store().all_files_in_collection("code").unwrap(), before_files);
    assert_eq!(engine.store().load_all_embeddings(2).unwrap(), before_vectors);
    assert_eq!(engine.store().embedding_generation().unwrap(), generation);
    assert_eq!(engine.store().chunk_count().unwrap(), 2);
}

fn assert_replacement(records: &[Value], reason: &str) {
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], reason);
    let mutations = records
        .iter()
        .filter(|r| r["kind"] == "mutation" && r["outcome"] == "committed")
        .collect::<Vec<_>>();
    assert_eq!(mutations.len(), 1);
    let mutation = mutations[0];
    assert_eq!(mutation["reason"], reason);
    assert_eq!(mutation["counts"]["sqlite_vectors_removed"], 1);
    assert_eq!(mutation["count_quality"], "exact");
    assert_eq!(mutation["process_id"], decision["process_id"]);
    assert_eq!(mutation["parent_operation_id"], decision["parent_operation_id"]);
    assert!(mutation["parent_operation_id"].as_u64().is_some());
    let terminal = records
        .iter()
        .find(|r| r["kind"] == "mutation_summary" && r["outcome"] == "completed")
        .unwrap();
    assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 1);
    assert!(!serde_json::to_string(records).unwrap().contains("canary source"));
}

#[test]
fn vector_lifecycle_unchanged_warm_reopen_preserves_mixed_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path();
    let (engine, rows) = seed(source);
    let before = engine.store().load_all_embeddings(2).unwrap();
    let generation = engine.store().embedding_generation().unwrap();
    drop(engine);
    let records = capture(|| {
        let mut engine = SearchEngine::fts_only(&source.join("search.db")).unwrap();
        emit(&mut engine, source, &rows);
        assert_eq!(engine.store().load_all_embeddings(2).unwrap(), before);
        assert_eq!(engine.store().embedding_generation().unwrap(), generation);
        assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 1);
    });
    let snapshot = records.iter().find(|r| r["kind"] == "startup_snapshot").unwrap();
    assert_eq!(snapshot["snapshot"]["state"], "observed");
    assert_eq!(snapshot["snapshot"]["files"], 1);
    assert_eq!(snapshot["snapshot"]["chunks"], 2);
    assert_eq!(snapshot["snapshot"]["vectors"], 1);
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], "unchanged");
    assert!(records.iter().all(|r| r["counts"]["sqlite_vectors_removed"] == 0));
}

#[test]
fn vector_lifecycle_changed_file_attributes_actual_non_null_loss() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    std::fs::write(dir.path().join("Module.bsl"), "Процедура Другая()\nКонецПроцедуры").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_changed");
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
}

#[test]
fn vector_lifecycle_hash_lookup_error_reindexes_without_claiming_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    let connection = rusqlite::Connection::open(engine.store().db_path()).unwrap();
    connection.execute("UPDATE files SET hash = 17 WHERE path = 'Module.bsl'", []).unwrap();
    assert!(engine.store().file_hash("", "Module.bsl").is_err());
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_lookup_error");
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
    assert!(engine.store().file_hash("", "Module.bsl").unwrap().is_some());
}

#[test]
fn vector_lifecycle_cleared_hash_and_missing_record_keep_distinct_causes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut engine, rows) = seed(dir.path());
    engine.store().clear_file_hashes("code").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    assert_replacement(&records, "hash_cleared");
    // No historical initiating actor is inferred from an empty stored hash.
    assert!(!records.iter().any(|r| r["reason"] == "hash_changed"));

    engine.store().remove_file("", "Module.bsl", "code").unwrap();
    let records = capture(|| emit(&mut engine, dir.path(), &rows));
    let decision = records.iter().find(|r| r["kind"] == "decision").unwrap();
    assert_eq!(decision["reason"], "missing_record");
    let mutation =
        records.iter().find(|r| r["kind"] == "mutation" && r["outcome"] == "committed").unwrap();
    assert_eq!(mutation["reason"], "missing_record");
    assert_eq!(mutation["counts"]["sqlite_vectors_removed"], 0);
    assert_eq!(decision["parent_operation_id"], mutation["parent_operation_id"]);
    assert_eq!(engine.store().load_pending_embedding_documents("code").unwrap().len(), 2);
}

#[test]
fn vector_lifecycle_read_error_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = rootless_source(&dir);
    let (engine, rows) = seed(&source);
    assert_read_error_preserves_rows_and_vectors(&source, engine, &rows);
}

#[test]
fn vector_lifecycle_read_error_with_root_table_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, rows) = seed_with_roots(dir.path(), true);
    assert_read_error_preserves_rows_and_vectors(dir.path(), engine, &rows);
}

#[cfg(unix)]
#[test]
fn vector_lifecycle_read_error_through_symlinked_root_with_root_table_preserves_rows_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let source = rootless_source(&dir);
    let (engine, rows) = seed_with_roots(&source, true);
    assert_read_error_preserves_rows_and_vectors(&source, engine, &rows);
}

#[test]
fn vector_lifecycle_missing_prefix_neighbor_stays_outside_with_or_without_root_table() {
    for install_roots in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cf");
        let outsider_dir = dir.path().join("cf_ext");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&outsider_dir).unwrap();
        let (mut engine, rows) = seed_with_roots(&source, install_roots);
        let before_rows = engine.store().load_indexed_documents(Some("code")).unwrap();
        let outsider = row(&outsider_dir.join("Module.bsl"), "Outsider");

        let records = capture(|| emit(&mut engine, &source, &[rows[0].clone(), outsider]));
        let decisions =
            records.iter().filter(|record| record["kind"] == "decision").collect::<Vec<_>>();
        assert_eq!(decisions.len(), 1, "lifecycle records: {records:#?}");
        assert_eq!(decisions[0]["reason"], "unchanged");
        assert_eq!(decisions[0]["examples"], serde_json::json!([":Module.bsl"]));
        assert_eq!(engine.store().load_indexed_documents(Some("code")).unwrap(), before_rows);
        assert_eq!(engine.store().chunk_count().unwrap(), 2);
    }
}

#[test]
fn vector_lifecycle_fused_writer_retains_totals_across_emit_batches_and_takeover() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path();
    let (mut engine, rows) = seed(source);
    let cache = crate::cache::WorkspaceCacheLayout::for_workspace(source);
    cache.ensure().unwrap();
    let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
    assert!(lease.owns_caches_now());
    let newer = Arc::new(Mutex::new(None));
    let newer_hook = newer.clone();
    super::FUSED_FILE_COMMITTED_HOOK.with(|hook| {
        hook.replace(Some(Box::new(move || {
            let claim = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
            assert!(claim.owns_caches_now());
            *newer_hook.lock().unwrap() = Some(claim);
        })))
    });
    let records = capture(|| {
        let mut writer = FusedChunkWriter::new(&mut engine, source.to_owned(), lease.clone());
        std::fs::write(source.join("Module.bsl"), "first change").unwrap();
        writer.emit_chunks(&rows).unwrap();
        std::fs::write(source.join("Module.bsl"), "second change").unwrap();
        assert!(writer.emit_chunks(&rows).is_err());
        let outcome = writer.failure.as_ref().unwrap().lifecycle_outcome();
        writer.finish(outcome);
    });
    assert!(lease.is_superseded());
    newer.lock().unwrap().take().unwrap().release();
    let summaries: Vec<_> = records.iter().filter(|r| r["kind"] == "mutation_summary").collect();
    // A one-second timer decides whether a flush happens, so the count of progress summaries
    // varies; each one opens a fresh interval with its own started intent.
    let started = summaries.iter().filter(|r| r["outcome"] == "started").count();
    let progress = summaries.iter().filter(|r| r["outcome"] == "progress").count();
    assert_eq!(started, 1 + progress, "lifecycle summaries: {summaries:#?}");
    let terminal = summaries.last().unwrap();
    assert_eq!(terminal["outcome"], "interrupted");
    assert_eq!(terminal["committed_totals"]["sqlite_vectors_removed"], 1);
    let decisions: Vec<_> = records.iter().filter(|r| r["kind"] == "decision").collect();
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0]["parent_operation_id"], decisions[1]["parent_operation_id"]);
    assert_eq!(engine.store().chunk_count().unwrap(), 2);
}
