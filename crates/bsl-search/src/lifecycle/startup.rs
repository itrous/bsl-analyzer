use std::cell::RefCell;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::{bounded, Outcome, Reason, Record, Snapshot};

thread_local! {
    static ROOTS: RefCell<Option<(String, Vec<PathBuf>)>> = const { RefCell::new(None) };
}

/// Host-supplied, already-resolved roots. The scope must not cross an await.
pub fn with_startup_roots<T>(mode: &str, roots: Vec<PathBuf>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(String, Vec<PathBuf>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ROOTS.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ROOTS.with(|slot| slot.replace(Some((bounded(mode, 64), roots)))));
    f()
}

pub fn startup_snapshot(path: &Path, mode: &str) {
    if !tracing::enabled!(target: "bsl_vector_lifecycle", tracing::Level::INFO) {
        return;
    }
    let mut record = observe(path, mode);
    record.emit(false);
}

fn observe(path: &Path, mode: &str) -> Record {
    let mut record = Record::new(path, "startup_snapshot", Reason::Startup);
    record.outcome = Outcome::Completed;
    let mut snapshot = Snapshot {
        state: "unavailable",
        mode: Some(bounded(mode, 64)),
        build_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        build_sha: build_info::commit().map(str::to_owned),
        ..Snapshot::default()
    };
    ROOTS.with(|slot| {
        if let Some((mode, roots)) = slot.borrow().as_ref() {
            snapshot.mode = Some(mode.clone());
            snapshot.roots_count = roots.len();
            let mut digest = blake3::Hasher::new();
            for root in roots {
                let bytes = root.as_os_str().as_encoded_bytes();
                digest.update(&(bytes.len() as u64).to_le_bytes());
                digest.update(bytes);
                if snapshot.roots.len() < super::MAX_EXAMPLES {
                    let text = root.to_string_lossy();
                    record.truncated |= text.len() > 256;
                    snapshot.roots.push(bounded(&text, 256));
                }
            }
            record.truncated |= roots.len() > snapshot.roots.len();
            snapshot.roots_digest = Some(digest.finalize().to_hex().to_string());
        }
    });
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            snapshot.state = "absent";
            snapshot.files = Some(0);
            snapshot.chunks = Some(0);
            snapshot.vectors = Some(0);
            snapshot.overlay_vectors = Some(0);
            snapshot.overlay_cache_entries = Some(0);
        }
        Ok(before) => {
            snapshot.file_identity = file_identity(&before);
            if let Ok(connection) = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                // A diagnostic snapshot must never wait for a writer, create the DB or migrate it.
                let _ = connection.busy_timeout(std::time::Duration::ZERO);
                if let Ok(tx) = connection.unchecked_transaction() {
                    snapshot.files = table_count(&tx, "files", false);
                    snapshot.chunks = table_count(&tx, "chunks", false);
                    snapshot.vectors = table_count(&tx, "chunks", true);
                    snapshot.overlay_vectors = table_count(&tx, "overlay_chunks", true);
                    snapshot.overlay_cache_entries =
                        table_count(&tx, "overlay_embedding_cache", false);
                    snapshot.schema_version = meta(&tx, "schema_version");
                    snapshot.generation = meta(&tx, "embedding_generation");
                    snapshot.text_version =
                        tx.query_row("PRAGMA user_version", [], |row| row.get(0)).ok();
                    snapshot.state = if [
                        snapshot.files,
                        snapshot.chunks,
                        snapshot.vectors,
                        snapshot.overlay_vectors,
                        snapshot.overlay_cache_entries,
                    ]
                    .iter()
                    .all(Option::is_some)
                    {
                        "observed"
                    } else {
                        "unavailable"
                    };
                }
            }
            let after = std::fs::metadata(path).ok();
            finish_identity(&mut snapshot, after.as_ref());
        }
        Err(_) => {}
    }
    record.snapshot = Some(snapshot);
    record
}

/// Compare the identity bracketing the read snapshot; unavailable identity cannot prove stability.
fn finish_identity(snapshot: &mut Snapshot, after: Option<&std::fs::Metadata>) {
    snapshot.identity_stable =
        after.is_some_and(|after| match (&snapshot.file_identity, file_identity(after)) {
            (Some(before), Some(after)) => before == &after,
            _ => false,
        });
    if snapshot.file_identity.is_some() && !snapshot.identity_stable {
        snapshot.state = "unstable";
    }
}

fn table_count(connection: &Connection, table: &str, vectors: bool) -> Option<u64> {
    let present: Option<i64> = connection
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [table], |row| {
            row.get(0)
        })
        .optional()
        .ok()?;
    if present.is_none() {
        return Some(0);
    }
    // Names are fixed callsite constants, not caller-controlled SQL identifiers.
    let sql = format!(
        "SELECT COUNT(*) FROM {table}{}",
        if vectors { " WHERE embedding IS NOT NULL" } else { "" }
    );
    connection.query_row(&sql, [], |row| row.get::<_, i64>(0)).ok().and_then(|n| n.try_into().ok())
}

fn meta(connection: &Connection, key: &str) -> Option<i64> {
    connection
        .query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1", [key], |row| row.get(0))
        .ok()
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_: &std::fs::Metadata) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_snapshot_is_read_only_and_distinguishes_absent_unavailable_and_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        let absent = observe(&path, "fts").snapshot.unwrap();
        assert_eq!(absent.state, "absent");
        assert_eq!(absent.vectors, Some(0));
        assert!(!path.exists());
        std::fs::write(&path, b"not sqlite").unwrap();
        let unavailable = observe(&path, "fts").snapshot.unwrap();
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(unavailable.vectors, None);
        assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
        let path = temp.path().join("legacy.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE files(id INTEGER); CREATE TABLE chunks(embedding BLOB); INSERT INTO files VALUES(1); INSERT INTO chunks VALUES(NULL),(x'00000000'); PRAGMA user_version=3;").unwrap();
        let snapshot =
            with_startup_roots("local", vec![PathBuf::from("source")], || observe(&path, "fts"))
                .snapshot
                .unwrap();
        assert_eq!(snapshot.files, Some(1));
        assert_eq!(snapshot.chunks, Some(2));
        assert_eq!(snapshot.vectors, Some(1));
        assert_eq!(snapshot.overlay_vectors, Some(0));
        assert_eq!(snapshot.text_version, Some(3));
        assert_eq!(snapshot.schema_version, None);
        assert_eq!(snapshot.mode.as_deref(), Some("local"));
        assert_eq!(snapshot.roots, vec!["source"]);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn startup_snapshot_counts_all_legacy_cache_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE overlay_embedding_cache(legacy_value BLOB); INSERT INTO overlay_embedding_cache VALUES(NULL),(x'00');").unwrap();
        let snapshot = observe(&path, "fts").snapshot.unwrap();
        assert_eq!(snapshot.overlay_cache_entries, Some(2));
        assert_eq!(snapshot.vectors, Some(0));
        assert_eq!(snapshot.state, "observed");
        connection.execute_batch("CREATE TABLE chunks(legacy_value BLOB);").unwrap();
        let snapshot = observe(&path, "fts").snapshot.unwrap();
        assert_eq!(snapshot.chunks, Some(0));
        assert_eq!(
            snapshot.vectors, None,
            "an unreadable legacy vector column is not an empty index"
        );
        assert_eq!(snapshot.state, "unavailable");
    }

    #[test]
    fn startup_snapshot_distinguishes_replaced_file_from_stable_store_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.db");
        let replacement = temp.path().join("replacement.db");
        for (path, rows) in [(&path, 1), (&replacement, 3)] {
            let connection = Connection::open(path).unwrap();
            connection.execute_batch("CREATE TABLE chunks(embedding BLOB);").unwrap();
            for _ in 0..rows {
                connection.execute("INSERT INTO chunks VALUES(x'00')", []).unwrap();
            }
        }
        let before = observe(&path, "fts");
        std::fs::rename(&path, temp.path().join("previous.db")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let after = observe(&path, "fts");
        assert_eq!(before.store_id, after.store_id);
        let mut before = before.snapshot.unwrap();
        let after = after.snapshot.unwrap();
        assert_eq!(before.vectors, Some(1));
        assert_eq!(after.vectors, Some(3));
        #[cfg(unix)]
        {
            assert_ne!(before.file_identity, after.file_identity);
            assert!(before.identity_stable && after.identity_stable);
            // A swap between the two metadata reads invalidates the old snapshot's identity.
            finish_identity(&mut before, Some(&std::fs::metadata(&path).unwrap()));
            assert!(!before.identity_stable);
            assert_eq!(before.state, "unstable");
        }
        #[cfg(not(unix))]
        {
            assert!(before.file_identity.is_none() && after.file_identity.is_none());
            finish_identity(&mut before, Some(&std::fs::metadata(&path).unwrap()));
            assert!(!before.identity_stable);
            assert_eq!(
                before.state, "observed",
                "unsupported identity is not a proven replacement"
            );
        }
    }
}
