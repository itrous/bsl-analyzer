//! On-disk SQLite store for the workspace call graph.
//!
//! The whole-config in-memory graph does not fit in RAM on large configurations
//! (a 25k-file ERP needs tens of GB). The graph is therefore built in bounded
//! batches and streamed into a SQLite file, which is then queried to serve `graph`
//! tool calls without ever materialising the full node/edge set in memory.
//!
//! The database is a **derived cache**: every row is reconstructable from the
//! sources, so the writer favours bulk-insert throughput (in-memory journal, no
//! per-row fsync) over crash durability. A truncated or corrupt file is detected
//! on open and rebuilt rather than trusted.
//!
//! Durable node ids ([`NodeRow::id`]) are produced by the build-time encoder in
//! [`hir::graph_index`], byte-identical to the ids the in-memory serving path
//! emits, so ids an agent holds survive the in-memory → SQLite switch.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ide::graph_index::{EdgeRow, NodeRow};
use ide::{GraphBuildSummary, GraphBuildTicker, MethodCallDigest, ModuleId, RootDatabaseImpl};
use rusqlite::{params, Connection, OptionalExtension};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use stdx::batch::BatchBudget;
use vfs::FileId;

#[cfg(test)]
use crate::graph::input::enumerate_bsl_files;
use crate::graph::input::{build_source_root, db_for_files};

/// Bumped whenever the table layout OR the persisted edge/node content changes so a
/// stale on-disk cache from an older binary is rejected (via the `meta` row) and
/// rebuilt. Version 5 adds the `notify_ref`/`idle_handler` callback edges; version 6
/// adds the `event_subscription` handler edges; version 7 changes persisted edge
/// content again — literal manager dispatch now stores `resolved` provenance, a
/// `Новый ОписаниеОповещения` error handler becomes a second `notify_ref` edge, and
/// `Движения.<Регистр>.<метод>()` movements become `register_movement` edges. Version 8
/// resolves idle handlers to a unique global common module (new cross-module edges).
/// Version 9 adds `subsystem_membership` edges (subsystem → member object / child subsystem).
/// Version 10 adds `role_reference` edges (role → object it grants rights on, plus RLS
/// condition objects). Version 11 adds `register_records` edges (document → register it
/// declares it posts, from the document's `RegisterRecords` metadata). Version 12 adds
/// `register_record_set` edges (code → register reached through a literal record-set creator
/// `РегистрыНакопления.<X>.СоздатьНаборЗаписей()`) and resolves locally-literal dynamic
/// `Движения[…]` indices to `register_movement` edges. Version 13 persists resolved
/// constant-manager method calls as method-to-method `call` edges. Version 14 builds
/// edges under dependency-aware extension visibility (`dependsOn`), so graphs built by
/// a pre-dependency binary must be rejected and rebuilt. Version 15 records the
/// extension-topology fingerprint (`topology_fp`) in the freshness meta, so a cached
/// graph without it can never be mistaken for topology-fresh.
// 16: `meta` gained `unread_paths` — the modules whose bytes could not be read when
// the artefact was built. An older artefact has no such key, and reading its absence
// as "nothing was unread" would certify a graph built partly blind, so the bump
// routes it to a rebuild through the existing mismatch path.
// 17: an unread body now bars callers from resolving into any body behind it, so a
// version-16 artefact holds edges — and `unresolved_calls` rows — this binary would
// never produce. Nothing about the workspace changed, so no fingerprint moves and
// nothing else would ever force the rebuild; only the version says the projection
// itself is of another generation.
// 18: `mdo` rows carry the file the object is defined by. A version-17 artefact
// holds them with a null file, and nothing about the workspace changed, so no
// fingerprint moves: a stale artefact would keep answering about metadata objects
// with a durable id and no place, which is the split this version closes.
// 19: `edges` rows carry the call site — the byte range of the call in the `from` node's
// file, or the code saying why there is none. A version-18 artefact has neither, and its
// edges are otherwise indistinguishable from this binary's, so serving call sites from it
// would answer "no place" for every edge that has one.
// 20: the row encoder now strips its rel against the RESOLVED workspace root, so a workspace
// declared through a link mints `method/file/<rel>::<name>` where a version-19 build minted
// `method/file/<basename>::<name>` and called it unaddressable. Same tree, same bytes: no
// fingerprint moves and no patch would ever rewrite a module nobody edited, so a reused
// artefact would answer `not_found` for the ids this binary now hands out — and an
// incremental patch would leave one database holding both spellings for nodes of one kind.
// 21: graph file addresses are persisted as root_id/path pairs and the files
// table carries full BLAKE3 content hashes, so absolute paths and stat-only
// identity cannot survive as the current format.
// 22: the files table records the stat identity each content hash was taken under, so a
// later process reuses the hash of an unchanged file instead of reading it again.
// 23: every publication records a `publication_id`, so the result of a write is established
// from the database itself; an older database has none and is rebuilt, never stamped.
// 24: XML semantic fingerprints now preserve tree boundaries/namespaces and exported method
// signatures include parameter composition. Existing version-23 rows cannot prove either
// property, so they are invalidated through the normal cache-format gate.
pub(crate) const SCHEMA_VERSION: u32 = 24;

/// One file's persisted identity in the `files` table: its complete content hash,
/// a compact projection retained for the existing drift API, and (for `.bsl`) its
/// resolution-signature hash. Persisting these per key lets a reload classify drift
/// granularly instead of only knowing the whole-workspace fingerprint moved.
pub(crate) struct FileFingerprint {
    /// Root-relative durable identity. The pair is never flattened into a
    /// single string in the SQLite key space.
    pub root_id: String,
    pub path: String,
    /// Full BLAKE3 digest of the file bytes.
    pub content_hash: [u8; 32],
    /// Compact projection retained for the existing drift API.
    pub fingerprint: u64,
    /// Resolution-signature hash, `None` for `.xml` (filled in by the body-only fast
    /// path; currently always `None`).
    pub sig_hash: Option<u64>,
    /// The stat identity `content_hash` was read under; `None` for a file whose bytes were
    /// not read, so its unreadable marker is never taken for a content hash.
    pub observation: Option<crate::graph::content_hash::Observation>,
}

/// The workspace identity a graph build reflects, as two independent components.
/// `files` folds every graph-relevant file's `(root_id, path, content_hash)`; `topology`
/// identifies the extension dependency graph (declared roots + `dependsOn`
/// closures). Kept structured — not XOR-folded into one word — so a change in one
/// component can never algebraically cancel a change in the other, and so a
/// consumer can tell a topology-triggered rebuild from a plain file edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphFp {
    /// Stable BLAKE3 fold of the durable file keys and complete content hashes.
    pub files: u64,
    /// Stable hash of the extension-topology fingerprint.
    pub topology: u64,
}

/// Build-level metadata recorded in the `meta` table, used on reopen to decide
/// whether a cached database still matches the current sources and binary. Node
/// and edge counts are derived from the bulk data at finalize time, not supplied.
pub struct GraphMeta {
    /// The [`GraphState`](crate::graph) generation this build reflects.
    pub revision: u64,
    /// Workspace identity (portable file contents + extension topology) at build time.
    pub fingerprint: GraphFp,
    /// Number of `.bsl` files indexed.
    pub files: usize,
    /// RFC 3339 build timestamp.
    pub built_at: String,
    /// This publication's identity: the publishing owner's token and its monotonic
    /// publication number. It differs between two publications even when their fingerprints
    /// are equal, and a moved database keeps the one it was published with.
    pub publication_id: String,
}

/// Full replacements of the graph database written by this process. A point patch writes into
/// the published file in one transaction and copies nothing, so it never appears here: an audit
/// sets these figures against the process's disk writes and the size of the database.
static CANDIDATES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CANDIDATE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What [`copy_audit`] has counted so far in this process.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyAudit {
    pub(crate) candidates: u64,
    pub(crate) candidate_bytes: u64,
}

#[cfg(test)]
pub(crate) fn copy_audit() -> CopyAudit {
    use std::sync::atomic::Ordering::SeqCst;
    CopyAudit { candidates: CANDIDATES.load(SeqCst), candidate_bytes: CANDIDATE_BYTES.load(SeqCst) }
}

/// Count a full replacement database written for installation. It is new content, not a copy
/// of the published one.
pub(crate) fn record_candidate(bytes: u64) {
    use std::sync::atomic::Ordering::SeqCst;
    let candidates = CANDIDATES.fetch_add(1, SeqCst) + 1;
    let total = CANDIDATE_BYTES.fetch_add(bytes, SeqCst) + bytes;
    tracing::info!(bytes, candidates, total, "full graph replacement written");
}

/// Read the canonical method-call digest from an existing bounded-build SQLite graph.
///
/// `SetAction` registrations are intentionally excluded: the call hierarchy only
/// represents direct, notification, and idle-handler method calls.
pub fn read_sqlite_method_call_digest(path: &Path) -> anyhow::Result<MethodCallDigest> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening graph database at {}", path.display()))?;
    let mut statement = conn.prepare(
        "SELECT edge.to_id, edge.from_id \
         FROM edges AS edge \
         JOIN nodes AS target ON target.id = edge.to_id \
         JOIN nodes AS caller ON caller.id = edge.from_id \
         WHERE target.kind = 'method' \
           AND caller.kind = 'method' \
           AND edge.kind IN ('call', 'notify_ref', 'idle_handler') \
         ORDER BY edge.to_id, edge.from_id",
    )?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()
        .context("reading method-to-method call edges from graph database")?;
    Ok(MethodCallDigest::from_rows(rows))
}

/// Read the method-call digest whose caller and target both belong to one source root.
///
/// Callers obtain `source_root_files` by resolving the anchor file's `SourceRootId` and
/// enumerating that root's file set. Requiring both endpoints matches the compact index,
/// which retains only modules from that same source root.
pub fn read_source_root_scoped_sqlite_method_call_digest<I>(
    path: &Path,
    source_root_files: I,
) -> anyhow::Result<MethodCallDigest>
where
    I: IntoIterator<Item = bsl_search::FileKey>,
{
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening graph database at {}", path.display()))?;
    let tx = conn.transaction().context("starting source-root scope transaction")?;
    tx.execute_batch(
        "CREATE TEMP TABLE source_root_files (
             root_id TEXT NOT NULL,
             path TEXT NOT NULL,
             PRIMARY KEY (root_id, path)
         ) WITHOUT ROWID;",
    )
    .context("creating source-root file scope")?;
    {
        let mut insert = tx
            .prepare("INSERT OR IGNORE INTO source_root_files (root_id, path) VALUES (?1, ?2)")
            .context("preparing source-root file scope insert")?;
        for file in source_root_files {
            insert
                .execute(params![file.root_id, file.path])
                .context("adding file to source-root scope")?;
        }
    }

    let mut statement = tx.prepare(
        "SELECT edge.to_id, edge.from_id \
         FROM edges AS edge \
         JOIN nodes AS target ON target.id = edge.to_id \
         JOIN nodes AS caller ON caller.id = edge.from_id \
         JOIN source_root_files AS target_file ON target_file.root_id = target.file_root_id \
             AND target_file.path = target.file_path \
         JOIN source_root_files AS caller_file ON caller_file.root_id = caller.file_root_id \
             AND caller_file.path = caller.file_path \
         WHERE target.kind = 'method' \
           AND caller.kind = 'method' \
           AND edge.kind IN ('call', 'notify_ref', 'idle_handler') \
         ORDER BY edge.to_id, edge.from_id",
    )?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()
        .context("reading source-root-scoped method-to-method call edges from graph database")?;
    Ok(MethodCallDigest::from_rows(rows))
}

/// Streams graph rows into a fresh SQLite file. Created once per build; nodes and
/// edges are appended in batches, then [`finalize`](Self::finalize) builds the
/// secondary indexes and the in-degree table in one pass over the bulk data.
pub(crate) struct GraphDbWriter {
    conn: Connection,
    roots: Option<bsl_search::WorkspaceRoots>,
    /// Canonical and walked spellings from the one scan that produced this graph.
    /// A row may carry either spelling; both must resolve to the same durable key
    /// when a symlink target lies outside the canonical root table.
    file_key_aliases: FxHashMap<String, bsl_search::FileKey>,
}

impl GraphDbWriter {
    /// Open `path` as a fresh database, discarding any prior file at that path so
    /// a stale schema can never leak into the new build. Sets bulk-load pragmas.
    pub(crate) fn create(path: &Path) -> anyhow::Result<Self> {
        for suffix in ["", "-wal", "-shm"] {
            let sibling = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("bsl-graph.db")
            ));
            let _ = std::fs::remove_file(&sibling);
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening graph database at {}", path.display()))?;
        // A rebuildable cache: trade durability for bulk-insert throughput.
        conn.execute_batch(
            "
            PRAGMA journal_mode = MEMORY;
            PRAGMA synchronous = OFF;
            PRAGMA temp_store = MEMORY;
            PRAGMA cache_size = -65536;

            CREATE TABLE nodes (
                id          TEXT PRIMARY KEY,
                kind        TEXT NOT NULL,
                name        TEXT NOT NULL,
                qualified   TEXT NOT NULL,
                module      TEXT,
                file_root_id TEXT,
                file_path   TEXT,
                name_offset INTEGER,
                sig_end     INTEGER,
                src_start   INTEGER,
                src_end     INTEGER,
                dispatch    TEXT,
                is_export   INTEGER,
                addressable INTEGER NOT NULL
            ) WITHOUT ROWID;

            CREATE TABLE edges (
                from_id    TEXT NOT NULL,
                to_id      TEXT NOT NULL,
                kind       TEXT NOT NULL,
                provenance TEXT NOT NULL,
                call_start INTEGER,
                call_end   INTEGER,
                call_absent TEXT,
                crosses    INTEGER NOT NULL
            );

            CREATE TABLE in_degree (
                id     TEXT PRIMARY KEY,
                degree INTEGER NOT NULL
            ) WITHOUT ROWID;

            CREATE TABLE meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE files (
                root_id      TEXT NOT NULL,
                path         TEXT NOT NULL,
                content_hash BLOB NOT NULL,
                fingerprint  INTEGER NOT NULL,
                sig_hash     INTEGER,
                stat_len         INTEGER,
                stat_mtime_ns    INTEGER,
                stat_ctime_ns    INTEGER,
                stat_ino         INTEGER,
                stat_dev         INTEGER,
                stat_observed_ns INTEGER,
                PRIMARY KEY (root_id, path)
            ) WITHOUT ROWID;

            CREATE TABLE unresolved_calls (
                target_scope TEXT NOT NULL,
                method_lower TEXT NOT NULL,
                caller_root_id TEXT NOT NULL,
                caller_path    TEXT NOT NULL,
                PRIMARY KEY (target_scope, method_lower, caller_root_id, caller_path)
            ) WITHOUT ROWID;
            ",
        )
        .context("initialising graph schema")?;

        Ok(Self { conn, roots: None, file_key_aliases: FxHashMap::default() })
    }

    pub(crate) fn set_workspace_roots(&mut self, roots: Option<&bsl_search::WorkspaceRoots>) {
        self.roots = roots.cloned();
    }

    pub(crate) fn set_file_key_aliases(
        &mut self,
        aliases: &FxHashMap<String, bsl_search::FileKey>,
    ) {
        self.file_key_aliases = aliases.clone();
    }

    /// Append a batch of nodes. A node id may be projected more than once across
    /// batches (the same MDO/method reached from several callers); the first
    /// spelling wins and later duplicates are ignored, matching the in-memory
    /// graph's first-seen node identity.
    pub(crate) fn write_nodes(&mut self, rows: &[NodeRow]) -> anyhow::Result<()> {
        let roots = self.roots.clone();
        let aliases = &self.file_key_aliases;
        let tx = self.conn.transaction().context("begin node batch")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO nodes \
                 (id, kind, name, qualified, module, file_root_id, file_path, name_offset, sig_end, src_start, \
                  src_end, dispatch, is_export, addressable) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for row in rows {
                let dispatch =
                    if row.dispatch.is_empty() { None } else { Some(row.dispatch.join(",")) };
                let (file_root_id, file_path) =
                    durable_file_key_with_aliases(roots.as_ref(), row.file.as_deref(), aliases);
                if row.file.is_some()
                    && roots.is_some()
                    && (file_root_id.is_none() || file_path.is_none())
                {
                    anyhow::bail!(
                        "graph node source is outside registered workspace roots: {}",
                        row.file.as_deref().unwrap_or_default()
                    );
                }
                stmt.execute(params![
                    row.id,
                    row.kind,
                    row.name,
                    row.qualified,
                    row.module,
                    file_root_id,
                    file_path,
                    row.name_offset,
                    row.sig_end,
                    row.src_start,
                    row.src_end,
                    dispatch,
                    row.is_export.map(|b| b as i64),
                    row.addressable as i64,
                ])?;
            }
        }
        tx.commit().context("commit node batch")?;
        Ok(())
    }

    /// Append a batch of edges verbatim. Edge multiplicity is preserved as given;
    /// de-duplication, if any, is the build orchestrator's policy.
    pub(crate) fn write_edges(&mut self, rows: &[EdgeRow]) -> anyhow::Result<()> {
        let tx = self.conn.transaction().context("begin edge batch")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO edges \
                 (from_id, to_id, kind, provenance, call_start, call_end, call_absent, crosses) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for row in rows {
                stmt.execute(params![
                    row.from_id,
                    row.to_id,
                    row.kind,
                    row.provenance,
                    row.call_start,
                    row.call_end,
                    row.call_site_absent,
                    row.crosses as i64,
                ])?;
            }
        }
        tx.commit().context("commit edge batch")?;
        Ok(())
    }

    /// Persist the per-file fingerprints into the `files` table. Used on reload to
    /// classify which files drifted instead of only knowing the workspace-wide
    /// fingerprint moved. `INSERT OR REPLACE` so a re-run at the same path is
    /// idempotent.
    pub(crate) fn write_files(&mut self, rows: &[FileFingerprint]) -> anyhow::Result<()> {
        let tx = self.conn.transaction().context("begin files batch")?;
        {
            let mut stmt = tx.prepare_cached(FILES_INSERT_SQL)?;
            for row in rows {
                let [len, mtime, ctime, ino, dev, observed] = observation_columns(row.observation);
                stmt.execute(params![
                    row.root_id,
                    row.path,
                    row.content_hash.as_slice(),
                    row.fingerprint as i64,
                    row.sig_hash.map(|h| h as i64),
                    len,
                    mtime,
                    ctime,
                    ino,
                    dev,
                    observed,
                ])?;
            }
        }
        tx.commit().context("commit files batch")?;
        Ok(())
    }

    /// Persist the set of inconsistently-cased objects into a single `meta` row
    /// (`casing_variants`, newline-joined). The incremental fast path reads it to
    /// refuse a body-only update that touches such an object. Empty for the common,
    /// consistently-cased configuration.
    pub(crate) fn write_casing_variants(&mut self, keys: &[String]) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('casing_variants', ?1)",
                params![keys.join("\n")],
            )
            .context("writing casing variants")?;
        Ok(())
    }

    /// Persist the module-located-but-unresolved qualified/manager call sites into the
    /// `unresolved_calls` reverse index. The PK dedups repeated call sites, so the
    /// content is order-independent. Used by the incremental fast path to find callers
    /// that would newly resolve when a target module gains/exports a method.
    pub(crate) fn write_unresolved_calls(
        &mut self,
        rows: &[(String, String, String)],
    ) -> anyhow::Result<()> {
        let roots = self.roots.clone();
        let aliases = &self.file_key_aliases;
        let tx = self.conn.transaction().context("begin unresolved_calls batch")?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO unresolved_calls \
                 (target_scope, method_lower, caller_root_id, caller_path) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (target_scope, method_lower, caller_file) in rows {
                let (root_id, path) =
                    durable_file_key_with_aliases(roots.as_ref(), Some(caller_file), aliases);
                let (Some(root_id), Some(path)) = (root_id, path) else {
                    anyhow::bail!(
                        "unresolved call caller is outside registered workspace roots: {caller_file}"
                    );
                };
                stmt.execute(params![target_scope, method_lower, root_id, path])?;
            }
        }
        tx.commit().context("commit unresolved_calls batch")?;
        Ok(())
    }

    /// Build secondary indexes, materialise the in-degree table from the bulk
    /// edges, and record build metadata (including derived node/edge counts).
    /// Consumes the writer — no further rows may be appended.
    pub(crate) fn finalize(mut self, meta: &GraphMeta) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                "
                CREATE INDEX edges_from ON edges(from_id);
                CREATE INDEX edges_to ON edges(to_id);
                CREATE INDEX nodes_kind ON nodes(kind);

                INSERT INTO in_degree (id, degree)
                    SELECT to_id, COUNT(*) FROM edges GROUP BY to_id;
                ",
            )
            .context("finalising graph indexes")?;

        let nodes: i64 = self.conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let edges: i64 = self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;

        let rows: [(&str, String); 9] = [
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("publication_id", meta.publication_id.clone()),
            ("revision", meta.revision.to_string()),
            ("fingerprint", meta.fingerprint.files.to_string()),
            ("topology_fp", meta.fingerprint.topology.to_string()),
            ("files", meta.files.to_string()),
            ("built_at", meta.built_at.clone()),
            ("nodes", nodes.to_string()),
            ("edges", edges.to_string()),
        ];
        let tx = self.conn.transaction().context("begin meta write")?;
        {
            let mut stmt = tx.prepare_cached("INSERT INTO meta (key, value) VALUES (?1, ?2)")?;
            for (key, value) in &rows {
                stmt.execute(params![key, value])?;
            }
        }
        tx.commit().context("commit meta write")?;
        self.conn.execute_batch("ANALYZE;").context("analyse graph database")?;
        Ok(())
    }
}

const FILES_INSERT_SQL: &str = "INSERT OR REPLACE INTO files \
     (root_id, path, content_hash, fingerprint, sig_hash, \
      stat_len, stat_mtime_ns, stat_ctime_ns, stat_ino, stat_dev, stat_observed_ns) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

/// The stat columns of a `files` row. SQLite integers are signed 64-bit: nanosecond times fit
/// until 2262, and inode/device numbers are stored bit for bit.
fn observation_columns(
    observation: Option<crate::graph::content_hash::Observation>,
) -> [Option<i64>; 6] {
    let Some(observation) = observation else { return [None; 6] };
    let stat = observation.stat;
    let change = stat.change;
    [
        Some(stat.len as i64),
        Some(stat.mtime_ns as i64),
        change.map(|change| change.ctime_ns as i64),
        change.map(|change| change.ino as i64),
        change.map(|change| change.dev as i64),
        Some(observation.observed_at_ns as i64),
    ]
}

/// The content hash stored for every indexed file, by durable key. Empty when the rows will
/// not read: the body-only fast path then rebuilds in full.
pub(crate) fn stored_fingerprints_in(
    conn: &Connection,
) -> std::collections::HashMap<bsl_search::FileKey, [u8; 32]> {
    let mut map = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT root_id, path, content_hash FROM files") else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?))
    }) else {
        return map;
    };
    for row in rows {
        let Ok((root_id, path, bytes)) = row else { return std::collections::HashMap::new() };
        let bytes: [u8; 32] = match bytes.as_slice().try_into() {
            Ok(bytes) => bytes,
            Err(_) => return std::collections::HashMap::new(),
        };
        map.insert(bsl_search::FileKey::new(root_id, path), bytes);
    }
    map
}

/// Read the stored per-file signature hashes (`None` for `.xml`, and for `.bsl` built
/// before signature persistence). A query failure yields an empty map → the body-only
/// fast path treats every module as ineligible (full rebuild). Keep the durable key:
/// resolving it to a declared path can differ from the canonical path used by the
/// current scan, especially on Windows.
pub(crate) fn stored_sig_hashes_in(
    conn: &Connection,
) -> std::collections::HashMap<bsl_search::FileKey, Option<u64>> {
    let mut map = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT root_id, path, sig_hash FROM files") else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            bsl_search::FileKey::new(r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        ))
    }) else {
        return map;
    };
    map.extend(rows.flatten());
    map
}

/// [`read_stored_observations_in`] over a file opened by path, for a test inspecting a
/// database it built by hand.
#[cfg(test)]
pub(crate) fn read_stored_observations(
    db_path: &Path,
    roots: &bsl_search::WorkspaceRoots,
) -> Vec<(PathBuf, crate::graph::content_hash::Observation)> {
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return Vec::new();
    };
    read_stored_observations_in(&conn, roots)
}

/// The content hashes the graph recorded together with the stat identity they were read
/// under, addressed through the current roots. A row whose file was not read, or whose stat
/// columns are incomplete, carries nothing to reuse.
pub(crate) fn read_stored_observations_in(
    conn: &Connection,
    roots: &bsl_search::WorkspaceRoots,
) -> Vec<(PathBuf, crate::graph::content_hash::Observation)> {
    use crate::graph::content_hash::{ChangeIdentity, Observation, StatIdentity};
    let Ok(mut stmt) = conn.prepare(
        "SELECT root_id, path, content_hash, stat_len, stat_mtime_ns, stat_ctime_ns, stat_ino, \
                stat_dev, stat_observed_ns \
         FROM files WHERE stat_len IS NOT NULL AND stat_mtime_ns IS NOT NULL \
                      AND stat_observed_ns IS NOT NULL",
    ) else {
        return Vec::new();
    };
    type Row = (String, String, Vec<u8>, i64, i64, Option<i64>, Option<i64>, Option<i64>, i64);
    let Ok(rows) = stmt.query_map([], |row| -> rusqlite::Result<Row> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(root_id, path, hash, len, mtime, ctime, ino, dev, observed)| {
            let hash: [u8; 32] = hash.try_into().ok()?;
            let change = match (ctime, ino, dev) {
                (Some(ctime), Some(ino), Some(dev)) => Some(ChangeIdentity {
                    ctime_ns: i128::from(ctime),
                    ino: ino as u64,
                    dev: dev as u64,
                }),
                (None, None, None) => None,
                _ => return None,
            };
            let file = roots.resolve_walked(&bsl_search::FileKey::new(root_id, path))?;
            Some((
                file,
                Observation {
                    stat: StatIdentity { len: len as u64, mtime_ns: mtime as u128, change },
                    hash,
                    observed_at_ns: observed as u128,
                },
            ))
        })
        .collect()
}

fn durable_file_key(
    roots: Option<&bsl_search::WorkspaceRoots>,
    file: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(file) = file else { return (None, None) };
    if let Some(roots) = roots {
        if let Some(key) = roots.key_of_path(Path::new(file)) {
            return (Some(key.root_id), Some(key.path));
        }
        // A source row outside the registered roots is deliberately not made
        // readable through a guessed absolute fallback.
        return (None, None);
    }
    // Unit fixtures that construct a writer without a project root use
    // already-relative paths. Keep the legacy column populated only for those
    // fixtures; production writers always install WorkspaceRoots.
    (Some(String::new()), Some(file.replace('\\', "/")))
}

fn durable_file_key_with_aliases(
    roots: Option<&bsl_search::WorkspaceRoots>,
    file: Option<&str>,
    aliases: &FxHashMap<String, bsl_search::FileKey>,
) -> (Option<String>, Option<String>) {
    if let Some(file) = file {
        if let Some(key) = aliases.get(file).or_else(|| aliases.get(&file.replace('\\', "/"))) {
            return (Some(key.root_id.clone()), Some(key.path.clone()));
        }
    }
    durable_file_key(roots, file)
}

fn install_changed_file_keys(
    conn: &Connection,
    changed_files: &[String],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<()> {
    // A pooled read handle outlives one plan; the table from the last plan on it is dropped.
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.changed_file_keys;
         CREATE TEMP TABLE changed_file_keys (
             root_id TEXT NOT NULL,
             path TEXT NOT NULL,
             PRIMARY KEY (root_id, path)
         ) WITHOUT ROWID;",
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO changed_file_keys (root_id, path) VALUES (?1, ?2)",
    )?;
    for file in changed_files {
        let (Some(root_id), Some(path)) = durable_file_key(roots, Some(file)) else {
            anyhow::bail!("incremental update: changed file is outside registered roots: {file}");
        };
        insert.execute(params![root_id, path])?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFileKey {
    root_id: String,
    path: String,
}

fn write_unread_keys(
    conn: &rusqlite::Connection,
    unread: &std::collections::BTreeSet<bsl_search::FileKey>,
) -> anyhow::Result<()> {
    let list: Vec<StoredFileKey> = unread
        .iter()
        .map(|key| StoredFileKey { root_id: key.root_id.clone(), path: key.path.clone() })
        .collect();
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('unread_paths', ?1)",
        rusqlite::params![serde_json::to_string(&list)?],
    )?;
    Ok(())
}

fn unread_keys_from_paths(
    unread: &BTreeSet<PathBuf>,
    roots: Option<&bsl_search::WorkspaceRoots>,
    universe: Option<&crate::graph::universe::ScannedUniverse>,
) -> anyhow::Result<BTreeSet<bsl_search::FileKey>> {
    unread
        .iter()
        .map(|path| {
            if let Some(roots) = roots {
                universe
                    .and_then(|universe| universe.key_for_path(roots, path))
                    .or_else(|| roots.key_of_path(path))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "unread file is outside registered roots: {}",
                            path.display()
                        )
                    })
            } else {
                Ok(bsl_search::FileKey::configuration(path.to_string_lossy().into_owned()))
            }
        })
        .collect()
}

/// The same modules, read STRICTLY: every writer of the current schema records the key, even
/// as an empty list, so an absent key, a query that fails or a payload that will not decode is
/// a failure to LOOK, not an empty answer. The structured key is retained
/// all the way to recovery; it must never be flattened with a separator that can collide with a
/// root or path.
pub(crate) fn read_unread_keys_strict(
    conn: &rusqlite::Connection,
) -> anyhow::Result<Vec<bsl_search::FileKey>> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'unread_paths'", [], |r| r.get(0))
        .optional()
        .context("reading unread_paths")?;
    let Some(raw) = raw else {
        anyhow::bail!("missing unread_paths metadata");
    };
    let keys = serde_json::from_str::<Vec<StoredFileKey>>(&raw)
        .context("decoding structured unread_paths")?;
    Ok(keys.into_iter().map(|key| bsl_search::FileKey::new(key.root_id, key.path)).collect())
}

/// The modules an artefact recorded as unreadable when it was built or last patched. This
/// lenient view is used for counts and diagnostics; it keeps the structured key even when a
/// caller elects to ignore a metadata read error, and is deliberately not an authority for
/// retiring recovery obligations.
pub(crate) fn read_unread_paths(conn: &rusqlite::Connection) -> Vec<bsl_search::FileKey> {
    read_unread_keys_strict(conn).unwrap_or_default()
}

/// Build the whole-workspace call graph straight into a fresh SQLite file at
/// `out_path`, in RAM-bounded batches. The in-memory graph does not fit on large
/// configurations (a 25k-module ERP blows past 8 GB in a single database), so this
/// is the path that makes a whole-config graph available at all.
///
/// The file universe arrives ALREADY SCANNED (`universe`): the id↔path map, the
/// persisted `files` rows and the caller's fingerprint bracket all project one walk,
/// so no pass of the operation can see a tree another pass did not. Each batch's
/// texts are loaded into a throwaway database (dropped before the next), with
/// cross-batch call targets resolved through the resident compact method index —
/// never another batch's database. Peak memory is therefore bounded by the batch
/// size plus that index, not by the whole config.
///
/// Cut `modules` into the batches one streaming pass loads together, weighing each
/// module by the byte length the universe's scan recorded for it (a module the scan
/// did not stat weighs nothing and is bounded by the file cap alone).
fn plan_batches<'a>(
    universe: &crate::graph::universe::ScannedUniverse,
    modules: &'a [ModuleId],
    file_paths: &FxHashMap<FileId, PathBuf>,
    budget: BatchBudget,
) -> Vec<&'a [ModuleId]> {
    let bytes_of: FxHashMap<&Path, u64> =
        universe.stats.iter().map(|stat| (stat.canonical.as_path(), stat.len)).collect();
    stdx::batch::chunks_by_budget(
        modules,
        |module| {
            file_paths
                .get(&module.file_id)
                .and_then(|path| bytes_of.get(path.as_path()))
                .copied()
                .unwrap_or(0)
        },
        budget,
    )
}

/// Returns the build tally; node/edge counts in the database are recorded in its
/// `meta` table by [`GraphDbWriter::finalize`], and the paths whose bytes could not be
/// read go beside them under `unread_paths` — the artefact carries its own gaps, so no
/// caller has to thread them through.
#[cfg(test)]
pub(crate) fn build_graph_database(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    out_path: &Path,
    budget: BatchBudget,
    meta: &GraphMeta,
) -> anyhow::Result<GraphBuildSummary> {
    build_graph_database_inner(project, universe, out_path, budget, meta, None, None)
}

/// As [`build_graph_database`], but also streams the search index's code chunks (with
/// graph context) from the same parse pass into `chunk_sink` — the compute half of the
/// graph/search fusion. The graph rows written are byte-identical to the plain build.
#[cfg(test)]
pub(crate) fn build_graph_database_fused(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    out_path: &Path,
    budget: BatchBudget,
    meta: &GraphMeta,
    chunk_sink: &mut dyn ide::FusedChunkSink,
) -> anyhow::Result<GraphBuildSummary> {
    build_graph_database_inner(project, universe, out_path, budget, meta, Some(chunk_sink), None)
}

/// Default seconds without build progress before the watchdog reports a stall.
const GRAPH_STALL_REPORT_SECS: u64 = 600;

/// Monitor thread for a running graph build. A deadlock in the build's parallel
/// region freezes the process silently — alive, zero CPU, zero disk growth — so
/// the watchdog turns that into an actionable `error!` record: how long the build
/// has been stuck, at which phase/batch, and every thread's kernel state. It
/// re-reports once per stall interval while the stall lasts.
///
/// `BSL_GRAPH_STALL_SECS` overrides the reporting threshold;
/// `BSL_GRAPH_STALL_ABORT=1` additionally aborts the process on the first report
/// (for supervised deployments where a restart beats a wedged daemon). The
/// watchdog only observes — by default a stalled build is left running so it can
/// still be inspected with a debugger.
struct BuildWatchdog {
    stop: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for BuildWatchdog {
    fn drop(&mut self) {
        let (lock, signal) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        signal.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Append one stall episode to the report file next to the graph database. The
/// daemon's file logging is opt-in, so this one-shot artifact is what survives a
/// wedged build in a default deployment: nothing is written in healthy runs,
/// and each episode appends a timestamped position + thread-state block.
fn write_stall_report(dir: &Path, stalled_secs: u64, position: &str, threads: &str) {
    use std::io::Write;
    let path = dir.join(crate::cache::STALL_REPORT_FILE);
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = format!(
        "[epoch {epoch_secs}] graph build stalled for {stalled_secs}s\n\
         position: {position}\nthreads: {threads}\n\n"
    );
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));
    if let Err(e) = written {
        tracing::warn!(path = %path.display(), "could not write stall report: {e}");
    }
}

fn spawn_build_watchdog(
    ticker: Arc<GraphBuildTicker>,
    report_dir: Option<PathBuf>,
) -> BuildWatchdog {
    let stop = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let stop_pair = Arc::clone(&stop);
    // Misconfiguration must be loud: an operator reproducing a wedged build relies
    // on these knobs actually being in effect, and a silent fallback costs them a
    // multi-hour cold-build cycle.
    let threshold_secs = match std::env::var("BSL_GRAPH_STALL_SECS") {
        Ok(value) => match value.parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => {
                tracing::warn!(
                    value = %value,
                    default_secs = GRAPH_STALL_REPORT_SECS,
                    "invalid BSL_GRAPH_STALL_SECS (want a positive integer); using the default"
                );
                GRAPH_STALL_REPORT_SECS
            }
        },
        Err(_) => GRAPH_STALL_REPORT_SECS,
    };
    let abort_on_stall = match std::env::var("BSL_GRAPH_STALL_ABORT") {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "" | "0" | "false" | "no" | "off" => false,
            _ => {
                tracing::warn!(
                    value = %value,
                    "unrecognized BSL_GRAPH_STALL_ABORT (want 1/true/yes/on); abort disabled"
                );
                false
            }
        },
        Err(_) => false,
    };
    tracing::info!(
        target: "bsl_graph",
        threshold_secs,
        abort_on_stall,
        "graph build watchdog armed"
    );
    let spawned =
        std::thread::Builder::new().name("bsl-graph-watchdog".to_owned()).spawn(move || {
            let (lock, signal) = &*stop_pair;
            let mut reported_episodes = 0;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !*stopped {
                // Condvar wait instead of a plain sleep so dropping the watchdog
                // (every build teardown, including tests) returns immediately
                // rather than after the current poll tick.
                let (guard, _) = signal
                    .wait_timeout(stopped, Duration::from_secs(1))
                    .unwrap_or_else(|e| e.into_inner());
                stopped = guard;
                if *stopped {
                    break;
                }
                let stalled_ms = ticker.ms_since_progress();
                let episodes = stalled_ms / (threshold_secs * 1000);
                if episodes == 0 {
                    reported_episodes = 0;
                } else if episodes > reported_episodes {
                    reported_episodes = episodes;
                    let position = ticker.position();
                    let threads = thread_state_summary();
                    tracing::error!(
                        stalled_secs = stalled_ms / 1000,
                        position = %position,
                        threads = %threads,
                        "graph build has made no progress; its parallel region may be deadlocked"
                    );
                    if let Some(dir) = &report_dir {
                        write_stall_report(dir, stalled_ms / 1000, &position, &threads);
                    }
                    if abort_on_stall {
                        tracing::error!("BSL_GRAPH_STALL_ABORT=1: aborting the stalled process");
                        std::process::abort();
                    }
                }
            }
        });
    let handle = match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::warn!("could not spawn graph build watchdog: {e}");
            None
        }
    };
    BuildWatchdog { stop, handle }
}

/// One compact `name:state:wchan` entry per OS thread of this process — the same
/// facts a by-hand `/proc` inspection collects, captured at the moment of a stall.
#[cfg(target_os = "linux")]
fn thread_state_summary() -> String {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return "unavailable".to_owned();
    };
    let mut entries: Vec<String> = Vec::new();
    for task in tasks.flatten() {
        let read = |name: &str| {
            std::fs::read_to_string(task.path().join(name)).unwrap_or_default().trim().to_owned()
        };
        let comm = read("comm");
        let wchan = read("wchan");
        // The state field follows the parenthesised comm, which may itself
        // contain spaces — split after the closing paren, not on raw whitespace.
        let stat = read("stat");
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?")
            .to_owned();
        entries.push(format!("{comm}:{state}:{wchan}"));
    }
    entries.join(" ")
}

#[cfg(not(target_os = "linux"))]
fn thread_state_summary() -> String {
    "unavailable on this platform".to_owned()
}

pub(crate) fn build_graph_database_inner(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    out_path: &Path,
    budget: BatchBudget,
    meta: &GraphMeta,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
    ticker: Option<Arc<GraphBuildTicker>>,
) -> anyhow::Result<GraphBuildSummary> {
    if !project.validated || project.search_roots.is_none() {
        anyhow::bail!("cannot persist a portable graph without validated workspace roots");
    }
    let files = &universe.files;
    let modules: Vec<ModuleId> = files.iter().map(|(f, _)| ModuleId::new(*f)).collect();
    let paths: FxHashMap<FileId, String> = files
        .iter()
        .map(|(f, p)| {
            let walked = universe.walked_path_for(p).unwrap_or(p);
            (*f, walked.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    let file_paths: FxHashMap<FileId, PathBuf> =
        files.iter().map(|(f, p)| (*f, p.clone())).collect();
    let batches = plan_batches(universe, &modules, &file_paths, budget);

    // Where each metadata object is defined, read off the universe this build
    // already scanned rather than a fresh walk of the disk.
    let mdo_files = crate::graph::mdo_files::mdo_files(
        &project.configs,
        &bsl_conventions::PathSetTree::from_files(
            universe.stats.iter().map(|stat| PathBuf::from(&stat.path)),
        ),
    );

    // The whole-workspace source root, built once and shared (cheap `Arc` clone)
    // into every per-batch database, so the 25k-path file set is assembled a single
    // time for the build rather than re-cloned per batch.
    let source_root = build_source_root(files);

    let mut writer = GraphDbWriter::create(out_path)?;
    writer.set_workspace_roots(project.search_roots.as_ref());
    let roots = project.search_roots.as_ref().expect("validated roots");
    let aliases = universe
        .file_key_aliases(roots)
        .ok_or_else(|| anyhow::anyhow!("graph scan file is outside registered workspace roots"))?;
    writer.set_file_key_aliases(&aliases);

    // One configuration cache shared across every batch database (and their per-job
    // clones), so the whole-config metadata load runs once for this build instead of
    // once per fresh batch database. A fresh cache per build keeps it a content
    // snapshot — see `ide_db`'s `GraphConfigCache`.
    let config_cache = std::sync::Arc::new(ide::GraphConfigCache::default());

    // Heartbeat + stall watchdog for the whole build (index, edge passes, fused
    // chunking): kept alive until after `finalize`, so a wedge anywhere in the
    // pipeline gets reported rather than freezing silently.
    let ticker = ticker.unwrap_or_else(|| Arc::new(GraphBuildTicker::default()));
    ticker.set_eta_total_intervals(if batches.is_empty() {
        0
    } else {
        batches.len().saturating_mul(4).saturating_add(8)
    });
    let _watchdog =
        spawn_build_watchdog(Arc::clone(&ticker), out_path.parent().map(Path::to_path_buf));

    // A SET, not a counter: `open_batch` is called once per batch per pass, and the
    // index pass alone re-opens every module, so a file that cannot be read would be
    // counted many times over.
    let mut unread: BTreeSet<PathBuf> = BTreeSet::new();

    // Scope the closures so their borrows end before `finalize`. `open_batch`
    // loads only the batch's texts (sharing the resident source root + config);
    // `sink` persists the freshly-encoded rows (the sole `&mut writer` borrow).
    let summary = {
        let mut open_batch = |batch: &[ModuleId]| -> RootDatabaseImpl {
            let batch_files: Vec<(FileId, PathBuf)> =
                batch.iter().map(|m| (m.file_id, file_paths[&m.file_id].clone())).collect();
            let loaded =
                db_for_files(&source_root, &batch_files, &project.configs, Some(&config_cache));
            unread.extend(loaded.unread);
            loaded.db
        };
        let mut sink = |nodes: &[NodeRow],
                        edges: &[EdgeRow]|
         -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            writer.write_nodes(nodes)?;
            writer.write_edges(edges)?;
            Ok(())
        };
        ide::build_workspace_graph_rows(
            &modules,
            &paths,
            Some(&project.workspace_root),
            &mdo_files,
            &batches,
            &mut open_batch,
            &mut sink,
            chunk_sink,
            Some(&ticker),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?
    };

    // Persist a per-file fingerprint for every graph-relevant file (`.bsl` + `.xml`),
    // from the SAME scanned universe the build lowered — not a fresh walk, which
    // could see a tree the built modules do not. For `.bsl` modules also persist the
    // body-free signature hash from the build, so a body-only edit (sig unchanged)
    // is distinguishable from a resolution-affecting one. `.xml` rows keep NULL sig.
    //
    // `file_paths` holds each module's canonical path verbatim; the stats projection
    // stringifies the same canonical path, so keying by that string lines the two up.
    let sig_by_path: FxHashMap<String, u64> = summary
        .module_sig_hashes
        .iter()
        .filter_map(|(m, &h)| {
            file_paths.get(&m.file_id).map(|p| (p.to_string_lossy().into_owned(), h))
        })
        .collect();
    let file_rows: anyhow::Result<Vec<FileFingerprint>> = universe
        .stats
        .iter()
        .map(|s| {
            let Some(key) = s.key(roots) else {
                return Err(anyhow::anyhow!(
                    "graph scan file is outside registered workspace roots: {}",
                    s.path
                ));
            };
            let content_hash = s.persisted_content_hash();
            let sig_hash =
                if bsl_conventions::str_has_extension(&s.path, bsl_conventions::XML_EXTENSION) {
                    crate::graph::scan::xml_semantic_hash_file(&s.canonical).map(|h| {
                        u64::from_le_bytes(h[..8].try_into().expect("blake3 hash >= 8 bytes"))
                    })
                } else {
                    sig_by_path.get(&s.path).copied()
                };
            Ok(FileFingerprint {
                root_id: key.root_id,
                path: key.path,
                content_hash,
                fingerprint: s.fingerprint(),
                sig_hash,
                observation: s.persisted_observation(),
            })
        })
        .collect();
    let file_rows = file_rows?;
    writer.write_files(&file_rows)?;
    writer.write_casing_variants(&summary.casing_variant_objects)?;
    writer.write_unresolved_calls(&summary.unresolved_calls)?;

    writer.finalize(meta)?;
    // Written by the BUILDER, not by whoever calls it. `finalize` stamps
    // `schema_version`, and the contract that an absent key means "nothing was
    // unread" rests on that version gating out older artefacts — so any caller who
    // forgot to add the key would produce a current-version artefact certifying a
    // graph it built blind. The set is born in this function; it is recorded here.
    {
        let conn = rusqlite::Connection::open(out_path)
            .with_context(|| format!("reopening {} to record unread paths", out_path.display()))?;
        let unread_keys =
            unread_keys_from_paths(&unread, project.search_roots.as_ref(), Some(universe))?;
        write_unread_keys(&conn, &unread_keys)?;
    }
    ticker.note("validation", 0, 0, "");
    Ok(summary)
}

/// Canonicalise a freshly-projected aux (`mdo`/`attribute`) node/edge id against the
/// object spellings already in the store. The durable id embeds the source-written
/// object casing, but a full rebuild fixes it to the global first-seen owner; an
/// incremental reprojection of a subset only knows the subset's casing, so for an
/// object an unchanged module already owns we must reuse the stored spelling.
///
/// `existing_mdo` maps each stored `mdo/<Type>/<obj>` id, lowercased (Unicode-aware,
/// since SQLite's `lower()` folds ASCII only and object names are Cyrillic), to its
/// actual spelling. A `method`/`module` id, or an object the store does not yet know
/// (genuinely new — owned by the changed set in both paths), is returned unchanged.
fn canonicalize_aux_id(
    existing_mdo: &std::collections::HashMap<String, String>,
    id: &str,
) -> String {
    if id.starts_with("mdo/") {
        return existing_mdo.get(&id.to_lowercase()).cloned().unwrap_or_else(|| id.to_string());
    }
    if let Some(rest) = id.strip_prefix("attribute/") {
        // rest = <Type>/<object>/<attr>; only the object segment needs canonicalising
        // (Type is the stable english name, attr is the metadata-stable field name).
        let mut seg = rest.splitn(3, '/');
        if let (Some(etype), Some(_obj), Some(attr)) = (seg.next(), seg.next(), seg.next()) {
            let mdo_key = format!("mdo/{etype}/{_obj}").to_lowercase();
            if let Some(canon_mdo) = existing_mdo.get(&mdo_key) {
                if let Some(canon_obj) =
                    canon_mdo.strip_prefix("mdo/").and_then(|r| r.split_once('/')).map(|(_, o)| o)
                {
                    return format!("attribute/{etype}/{canon_obj}/{attr}");
                }
            }
        }
    }
    id.to_string()
}

/// Split an aux durable id into its `(EnglishType, object)` segments. `None` for a
/// `method`/`module` id. Object/type segments never contain `/` (BSL identifiers and
/// english type names exclude it), so the split is unambiguous.
fn aux_object(id: &str) -> Option<(&str, &str)> {
    if let Some(rest) = id.strip_prefix("mdo/") {
        return rest.split_once('/');
    }
    if let Some(rest) = id.strip_prefix("attribute/") {
        let mut seg = rest.splitn(3, '/');
        if let (Some(etype), Some(obj), Some(_attr)) = (seg.next(), seg.next(), seg.next()) {
            return Some((etype, obj));
        }
    }
    None
}

/// Refuse the body-only fast path for the two aux-spelling cases its DB-pinned
/// canonicalisation cannot reproduce byte-identically — both require cross-module
/// casing inconsistency, so a normal (consistent-casing) edit is unaffected:
///
/// - **(A) casing change of a referenced object** — a changed module references an
///   existing object with a different exact spelling. If that module is the object's
///   first-seen owner, a full rebuild would adopt the new spelling, but the fast path
///   pins to the stored one.
/// - **(B) ownership shift on drop** — a changed module drops its last reference to an
///   object that survives via another module; a full rebuild would re-derive the
///   canonical spelling from the surviving (possibly different-cased) owner.
///
/// In both cases we fall back to a full rebuild, which is always correct.
fn incremental_safety_check(
    conn: &Connection,
    changed_files: &[String],
    rows: &ide::ReprojectedRows,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<()> {
    use std::collections::{HashMap, HashSet};

    install_changed_file_keys(conn, changed_files, roots)?;

    // (C) Objects the full build saw with inconsistent casing across modules. Their
    // cross-module first-seen ordering is not reconstructable from the canonicalised
    // store, so the fast path must not touch them. Recorded as lowercased
    // `englishtype/object` keys in the `casing_variants` meta row.
    let variant_keys: HashSet<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'casing_variants'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .into_iter()
        .flat_map(|v| v.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|s| !s.is_empty())
        .collect();
    let touches_variant = |id: &str| -> bool {
        aux_object(id).is_some_and(|(etype, obj)| {
            variant_keys.contains(&format!("{}/{}", etype.to_lowercase(), obj.to_lowercase()))
        })
    };

    // (A) Stored object spelling per (type, object), case-folded.
    let mut stored_obj: HashMap<(String, String), String> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM nodes WHERE kind IN ('mdo', 'attribute')")?;
        let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for id in ids.flatten() {
            if let Some((etype, obj)) = aux_object(&id) {
                stored_obj
                    .entry((etype.to_lowercase(), obj.to_lowercase()))
                    .or_insert_with(|| obj.to_string());
            }
        }
    }
    let reprojected_aux = rows
        .nodes
        .iter()
        .filter(|n| n.kind == "mdo" || n.kind == "attribute")
        .map(|n| n.id.as_str())
        .chain(rows.edges.iter().map(|e| e.to_id.as_str()));
    for id in reprojected_aux {
        if touches_variant(id) {
            anyhow::bail!("incremental update: touches casing-variant object {id}; full rebuild");
        }
        if let Some((etype, obj)) = aux_object(id) {
            if let Some(stored) = stored_obj.get(&(etype.to_lowercase(), obj.to_lowercase())) {
                if stored != obj {
                    anyhow::bail!(
                        "incremental update: aux object casing drift ({obj} vs stored {stored}); full rebuild"
                    );
                }
            }
        }
    }

    // (B) Aux objects the changed modules referenced before the edit.
    let old_sql = "SELECT DISTINCT e.to_id FROM edges e JOIN nodes n ON e.from_id = n.id \
         JOIN changed_file_keys changed ON changed.root_id = n.file_root_id \
             AND changed.path = n.file_path \
         WHERE e.to_id LIKE 'mdo/%' OR e.to_id LIKE 'attribute/%'";
    let old_aux: HashSet<String> = {
        let mut stmt = conn.prepare(old_sql)?;
        let it = stmt.query_map([], |r| r.get::<_, String>(0))?;
        it.filter_map(|r| r.ok()).collect()
    };
    for id in &old_aux {
        if touches_variant(id) {
            anyhow::bail!(
                "incremental update: drops/keeps a casing-variant object {id}; full rebuild"
            );
        }
    }
    let new_aux: HashSet<&str> = rows
        .edges
        .iter()
        .map(|e| e.to_id.as_str())
        .filter(|t| t.starts_with("mdo/") || t.starts_with("attribute/"))
        .collect();
    // A surviving reference is one from BSL source outside the changed set. The
    // kind says so; `file` alone no longer does, now that an object carries one.
    let survivors_sql = "SELECT COUNT(*) FROM edges e JOIN nodes n ON e.from_id = n.id \
         WHERE e.to_id = ?1 AND n.kind IN ('method','module') \
           AND NOT EXISTS (SELECT 1 FROM changed_file_keys changed \
                           WHERE changed.root_id = n.file_root_id \
                             AND changed.path = n.file_path)";
    for dropped in old_aux.iter().filter(|x| !new_aux.contains(x.as_str())) {
        let survivors: i64 = conn.query_row(survivors_sql, params![dropped], |r| r.get(0))?;
        if survivors > 0 {
            anyhow::bail!(
                "incremental update: dropped aux ref {dropped} still referenced by an unchanged module; full rebuild"
            );
        }
    }
    Ok(())
}

/// Insert one node row, overriding only its `id` (for aux-id canonicalisation).
/// `INSERT OR IGNORE` keeps the first-seen spelling, exactly like the bulk writer.
fn insert_node_row(
    tx: &Connection,
    row: &NodeRow,
    id: &str,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<()> {
    let dispatch = if row.dispatch.is_empty() { None } else { Some(row.dispatch.join(",")) };
    let (file_root_id, file_path) = durable_file_key(roots, row.file.as_deref());
    if row.file.is_some() && roots.is_some() && (file_root_id.is_none() || file_path.is_none()) {
        anyhow::bail!(
            "incremental graph node source is outside registered workspace roots: {}",
            row.file.as_deref().unwrap_or_default()
        );
    }
    tx.prepare_cached(
        "INSERT OR IGNORE INTO nodes \
         (id, kind, name, qualified, module, file_root_id, file_path, name_offset, sig_end, src_start, \
          src_end, dispatch, is_export, addressable) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?
    .execute(params![
        id,
        row.kind,
        row.name,
        row.qualified,
        row.module,
        file_root_id,
        file_path,
        row.name_offset,
        row.sig_end,
        row.src_start,
        row.src_end,
        dispatch,
        row.is_export.map(|b| b as i64),
        row.addressable as i64,
    ])?;
    Ok(())
}

/// What a point patch has worked out before it writes anything: the reprojected rows of the
/// modules at `changed_paths`. Computed from the sources and the model without a write
/// transaction open, so the file is locked for the writing alone.
pub(crate) struct BodyPatch {
    rows: ide::ReprojectedRows,
    changed_modules: Vec<ModuleId>,
    changed_paths: Vec<PathBuf>,
    metadata_node_prefixes: Vec<String>,
    clear_graph: bool,
    xml_sig_hashes: FxHashMap<String, Option<u64>>,
    file_paths: FxHashMap<FileId, PathBuf>,
    unread: BTreeSet<PathBuf>,
    modules: usize,
}

/// The `Form.xml` descriptor beside a managed form's `Ext/Form/Module.bsl`, spelled with
/// `/` separators like the descriptor paths it is compared with.
fn form_xml_for_module(module: &Path) -> Option<String> {
    use bsl_conventions::{conventional_of, ConventionalName};
    let form_dir = module.parent()?;
    let is_module = module
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| conventional_of(name) == Some(ConventionalName::Module));
    let is_form_dir = form_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| conventional_of(name) == Some(ConventionalName::Form));
    if !is_module || !is_form_dir {
        return None;
    }
    let xml = form_dir.parent()?.join(ConventionalName::FormXml.canonical());
    Some(xml.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
impl BodyPatch {
    pub(crate) fn rows_for_test(&self) -> &ide::ReprojectedRows {
        &self.rows
    }
}

/// Identify only metadata XML files whose projection is local to one persisted MDO owner
/// or one form. `None` is the deliberately narrow fallback for global/unsupported XML.
pub(crate) fn local_metadata_delta(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    db_path: &Path,
    xml_paths: &[PathBuf],
) -> anyhow::Result<Option<(Vec<String>, Vec<PathBuf>)>> {
    if xml_paths.is_empty() {
        return Ok(Some((Vec::new(), Vec::new())));
    }
    let roots = project.search_roots.as_ref();
    let mdo_files = crate::graph::mdo_files::mdo_files(
        &project.configs,
        &bsl_conventions::PathSetTree::from_files(
            universe.stats.iter().map(|stat| PathBuf::from(&stat.path)),
        ),
    );
    let current_mdos: std::collections::HashMap<String, String> = mdo_files
        .iter()
        .map(|((kind, folded_name), path)| {
            let object_name =
                Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or(folded_name);
            (path.clone(), format!("mdo/{}/{}", kind.english_name(), object_name))
        })
        .collect();
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut owners = Vec::new();
    let mut forms = Vec::new();

    for xml_path in xml_paths {
        let xml = xml_path.to_string_lossy().replace('\\', "/");
        let mut found_owner = false;
        let current_owner = current_mdos.get(&xml);
        let mut previous_mdo_owners = Vec::new();
        if let Some(owner) = current_owner {
            owners.push(owner.clone());
            found_owner = true;
        }
        if let (Some(root_id), Some(path)) = durable_file_key(roots, Some(&xml)) {
            let mut stmt = conn.prepare(
                "SELECT id FROM nodes WHERE kind = 'mdo' AND file_root_id = ?1 AND file_path = ?2",
            )?;
            let rows = stmt.query_map(params![root_id, path], |row| row.get::<_, String>(0))?;
            for row in rows {
                let owner = row?;
                previous_mdo_owners.push(owner.clone());
                owners.push(owner);
                found_owner = true;
            }
        }
        if current_owner.is_some() || !previous_mdo_owners.is_empty() {
            // The local MDO path proves a stable owner identity. Adding, deleting,
            // or renaming the object can introduce consumers that have no persisted
            // incoming edge yet, so keep those topology edits on the full-build path.
            if previous_mdo_owners.len() != 1
                || current_owner
                    .is_none_or(|current| !previous_mdo_owners[0].eq_ignore_ascii_case(current))
            {
                return Ok(None);
            }
        }
        // An MDO's forms can bind directly to its attributes. Reproject every form
        // module owned by that object together with the catalog rows so data_binding
        // edges are rebuilt from the same metadata generation.
        let mdo_owners: Vec<_> =
            owners.iter().filter(|owner| owner.starts_with("mdo/")).cloned().collect();
        for (_file_id, module_path) in &universe.files {
            let text = module_path.to_string_lossy().replace('\\', "/");
            let Some((Some((kind, object)), form_name)) = ide::form_key_for_path(&text) else {
                continue;
            };
            let owner_id = format!("mdo/{}/{}", kind.english_name(), object);
            if !mdo_owners.iter().any(|owner| owner.eq_ignore_ascii_case(&owner_id)) {
                continue;
            }
            owners.push(format!("form/{}/{form_name}", owner_id.trim_start_matches("mdo/")));
            forms.push(module_path.clone());
            found_owner = true;
        }
        for (_file_id, module_path) in &universe.files {
            let Some(xml_candidate) = form_xml_for_module(module_path) else { continue };
            if !xml.eq_ignore_ascii_case(&xml_candidate) {
                continue;
            }
            let text = module_path.to_string_lossy().replace('\\', "/");
            let Some((owner, form_name)) = ide::form_key_for_path(&text) else { continue };
            let scope = owner.as_ref().map_or_else(
                || "common".to_owned(),
                |(kind, object)| format!("{}/{}", kind.english_name(), object),
            );
            owners.push(format!("form/{scope}/{form_name}"));
            forms.push(module_path.clone());
            found_owner = true;
        }
        if !found_owner {
            return Ok(None);
        }
    }
    owners.sort();
    owners.dedup();
    forms.sort();
    forms.dedup();
    Ok(Some((owners, forms)))
}

impl BodyPatch {
    /// How many modules the graph holds once the patch is applied.
    pub(crate) fn modules(&self) -> usize {
        self.modules
    }

    pub(crate) fn reprojected_modules(&self) -> usize {
        self.changed_modules.len()
    }
}

/// Reproject ONLY the modules at `changed_paths` against the graph database at `src_path`.
/// `changed_paths` is the FULL reprojection set the caller proved sufficient — either the edited
/// body-only modules (signature unchanged), or, for a signature change, the changed modules PLUS
/// their resolved callers (the caller-delta set). Once [`begin_body_patch`] applies it, the
/// database holds what a full rebuild of the edited tree would. Eligibility is not re-validated
/// here: the caller (`try_incremental_reload`) owns the sig/caller-delta-safety gates.
#[cfg(test)]
pub(crate) fn compute_body_patch(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    src_path: &Path,
    changed_paths: &[PathBuf],
    budget: BatchBudget,
) -> anyhow::Result<BodyPatch> {
    compute_body_patch_with_metadata(
        project,
        universe,
        src_path,
        changed_paths,
        &[],
        &[],
        &[],
        &[],
        budget,
    )
}

/// As [`compute_body_patch`], with a proved local XML owner delta. Metadata rows and
/// their old owner prefixes are carried to the same SQL transaction as BSL rows.
#[allow(
    clippy::too_many_arguments,
    reason = "the production caller passes distinct inputs for one atomic BSL and metadata projection"
)]
pub(crate) fn compute_body_patch_with_metadata(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    src_path: &Path,
    changed_paths: &[PathBuf],
    metadata_paths: &[PathBuf],
    owner_ids: &[String],
    form_paths: &[PathBuf],
    xml_observations: &[(PathBuf, Option<u64>)],
    budget: BatchBudget,
) -> anyhow::Result<BodyPatch> {
    if !project.validated || project.search_roots.is_none() {
        anyhow::bail!("cannot patch a portable graph without validated workspace roots");
    }
    let files = &universe.files;
    let all_modules: Vec<ModuleId> = files.iter().map(|(f, _)| ModuleId::new(*f)).collect();
    let paths: FxHashMap<FileId, String> =
        files.iter().map(|(f, p)| (*f, p.to_string_lossy().replace('\\', "/"))).collect();
    let file_paths: FxHashMap<FileId, PathBuf> =
        files.iter().map(|(f, p)| (*f, p.clone())).collect();
    let batches = plan_batches(universe, &all_modules, &file_paths, budget);

    // Where each metadata object is defined, read off the universe this build
    // already scanned rather than a fresh walk of the disk.
    let mdo_files = crate::graph::mdo_files::mdo_files(
        &project.configs,
        &bsl_conventions::PathSetTree::from_files(
            universe.stats.iter().map(|stat| PathBuf::from(&stat.path)),
        ),
    );

    // Map changed canonical paths → ModuleIds, preserving file-id order so a new aux
    // object's first-seen spelling matches a full build's.
    let mut effective_changed_paths = changed_paths.to_vec();
    let changed_mdo_owners: Vec<_> =
        owner_ids.iter().filter(|owner| owner.starts_with("mdo/")).collect();
    if !changed_mdo_owners.is_empty() {
        let conn =
            Connection::open_with_flags(src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.file_root_id, n.file_path FROM edges e \
             JOIN nodes n ON n.id = e.from_id WHERE n.kind IN ('method','module') \
             AND (e.to_id = ?1 OR substr(e.to_id, 1, length(?1) + 1) = ?1 || '/')",
        )?;
        for owner in changed_mdo_owners {
            let rows = stmt.query_map(params![owner], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (Some(root_id), Some(path)) = row? else { continue };
                let Some(roots) = project.search_roots.as_ref() else { continue };
                let Some(path) = roots.resolve_walked(&bsl_search::FileKey::new(root_id, path))
                else {
                    continue;
                };
                if files.iter().any(|(_, scanned)| scanned == &path)
                    && !effective_changed_paths.contains(&path)
                {
                    effective_changed_paths.push(path);
                }
            }
        }
    }
    let changed_set: std::collections::HashSet<&Path> =
        effective_changed_paths.iter().map(|p| p.as_path()).collect();
    let changed_modules: Vec<ModuleId> = files
        .iter()
        .filter(|(_, p)| changed_set.contains(p.as_path()))
        .map(|(f, _)| ModuleId::new(*f))
        .collect();
    let scanned_changed = effective_changed_paths
        .iter()
        .filter(|path| files.iter().any(|(_, scanned)| scanned == *path))
        .count();
    if changed_modules.len() != scanned_changed {
        anyhow::bail!("incremental update: changed paths do not match scanned BSL modules");
    }

    if changed_paths.is_empty() && metadata_paths.is_empty() && xml_observations.is_empty() {
        return Ok(BodyPatch {
            rows: ide::ReprojectedRows {
                nodes: Vec::new(),
                edges: Vec::new(),
                sig_hashes: FxHashMap::default(),
                casing_variant_objects: Vec::new(),
                unresolved_calls: Vec::new(),
            },
            changed_modules: Vec::new(),
            changed_paths: Vec::new(),
            metadata_node_prefixes: Vec::new(),
            clear_graph: false,
            xml_sig_hashes: FxHashMap::default(),
            file_paths,
            unread: BTreeSet::new(),
            modules: all_modules.len(),
        });
    }

    let source_root = build_source_root(files);
    let config_cache = std::sync::Arc::new(ide::GraphConfigCache::default());
    // The patch's own report covers the WHOLE universe, not just `changed`: the
    // index pass opens `all_modules` through this same closure.
    let mut unread: BTreeSet<PathBuf> = BTreeSet::new();
    let mut open_batch = |batch: &[ModuleId]| -> RootDatabaseImpl {
        let batch_files: Vec<(FileId, PathBuf)> =
            batch.iter().map(|m| (m.file_id, file_paths[&m.file_id].clone())).collect();
        let loaded =
            db_for_files(&source_root, &batch_files, &project.configs, Some(&config_cache));
        unread.extend(loaded.unread);
        loaded.db
    };

    // The reprojection's index pass runs the same guarded batch runners as a full
    // build, so it gets the same heartbeat + stall watchdog.
    let ticker = Arc::new(GraphBuildTicker::default());
    let _watchdog =
        spawn_build_watchdog(Arc::clone(&ticker), src_path.parent().map(Path::to_path_buf));

    let source_projection_empty = if all_modules.len() == 1 && changed_modules.len() == 1 {
        let conn =
            Connection::open_with_flags(src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))? == 0
    } else {
        false
    };

    let mut rows = if source_projection_empty {
        // With no prior graph rows and exactly one current module there are no
        // unchanged BSL consumers. Reuse the canonical whole-workspace projector to
        // restore that module plus all metadata owners in one ordinary patch.
        let mut projected_nodes = Vec::new();
        let mut projected_edges = Vec::new();
        let mut sink = |nodes: &[ide::graph_index::NodeRow],
                        edges: &[ide::graph_index::EdgeRow]| {
            projected_nodes.extend_from_slice(nodes);
            projected_edges.extend_from_slice(edges);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        };
        let summary = ide::build_workspace_graph_rows(
            &all_modules,
            &paths,
            Some(&project.workspace_root),
            &mdo_files,
            &batches,
            &mut open_batch,
            &mut sink,
            None,
            Some(&ticker),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        ide::ReprojectedRows {
            nodes: projected_nodes,
            edges: projected_edges,
            sig_hashes: summary.module_sig_hashes,
            casing_variant_objects: summary.casing_variant_objects,
            unresolved_calls: summary.unresolved_calls,
        }
    } else if changed_modules.is_empty() {
        ide::ReprojectedRows {
            nodes: Vec::new(),
            edges: Vec::new(),
            sig_hashes: FxHashMap::default(),
            casing_variant_objects: Vec::new(),
            unresolved_calls: Vec::new(),
        }
    } else {
        ide::reproject_changed_modules(
            &all_modules,
            &changed_modules,
            &paths,
            Some(&project.workspace_root),
            &mdo_files,
            &batches,
            &mut open_batch,
            Some(&ticker),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?
    };

    // A form-module BSL add/remove changes whether the cold graph projects that
    // form's XML structure. Treat it as an owner delta too: additions reproject
    // the existing Form.xml owner; deletions clear the prior owner prefix.
    let mut metadata_owner_ids = owner_ids.to_vec();
    let mut metadata_form_paths = form_paths.to_vec();
    for path in &effective_changed_paths {
        let text = path.to_string_lossy().replace('\\', "/");
        let Some((owner, form)) = ide::form_key_for_path(&text) else { continue };
        let scope = owner.map_or_else(
            || "common".to_owned(),
            |(kind, name)| format!("{}/{}", kind.english_name(), name),
        );
        metadata_owner_ids.push(format!("form/{scope}/{form}"));
        if let Some(xml) = form_xml_for_module(path).map(PathBuf::from) {
            if !metadata_form_paths.contains(&xml) {
                metadata_form_paths.push(xml);
            }
        }
    }
    if owner_ids.iter().any(|owner| owner.starts_with("mdo/")) {
        // Newly-added attributes have no old binding edge to use as a reverse index.
        // Reproject every form module so unresolved data paths and Ref fields can
        // become bindings in this same metadata publication. Their structural owner
        // prefixes join the same transaction, replacing rather than duplicating the
        // existing form→item/attribute and mdo→form rows.
        for (_, path) in files {
            let Some((owner, form)) = ide::form_key_for_path(&path.to_string_lossy()) else {
                continue;
            };
            let scope = owner.map_or_else(
                || "common".to_owned(),
                |(kind, name)| format!("{}/{}", kind.english_name(), name),
            );
            metadata_owner_ids.push(format!("form/{scope}/{form}"));
            metadata_form_paths.push(path.clone());
        }
    }
    metadata_owner_ids.sort();
    metadata_owner_ids.dedup();

    let mut form_modules: Vec<ModuleId> = metadata_form_paths
        .iter()
        .filter_map(|path| {
            files.iter().find(|(_, scanned)| scanned == path).map(|(file, _)| ModuleId::new(*file))
        })
        .collect();
    // The changed Form/Module.bsl path itself is already a proof of the owner even
    // when path normalization differs from the accompanying Form.xml path.
    // Include it directly so adding the module projects the existing form owner.
    form_modules.extend(effective_changed_paths.iter().filter_map(|changed| {
        let text = changed.to_string_lossy();
        ide::form_key_for_path(&text).and_then(|_| {
            files
                .iter()
                .find(|(_, scanned)| scanned == changed)
                .map(|(file, _)| ModuleId::new(*file))
        })
    }));
    form_modules.sort_by_key(|module| module.file_id);
    form_modules.dedup();
    if !source_projection_empty && (!metadata_owner_ids.is_empty() || !form_modules.is_empty()) {
        if let Some(representative) = all_modules.first().copied() {
            let mut projection_ids = vec![representative];
            projection_ids.extend(form_modules.iter().copied().filter(|m| *m != representative));
            let projection_files: Vec<_> = projection_ids
                .iter()
                .map(|module| (module.file_id, file_paths[&module.file_id].clone()))
                .collect();
            let loaded = db_for_files(
                &source_root,
                &projection_files,
                &project.configs,
                Some(&config_cache),
            );
            unread.extend(loaded.unread);
            let (nodes, edges) = ide::reproject_metadata_owners(
                &loaded.db,
                representative.file_id,
                &form_modules,
                &paths,
                Some(&project.workspace_root),
                &mdo_files,
                &metadata_owner_ids.iter().cloned().collect(),
            );
            rows.nodes.extend(nodes);
            rows.edges.extend(edges);
        } else {
            // A cold graph with no BSL modules has no derived nodes or edges, no
            // matter which metadata owner changed. `clear_graph` below writes that
            // exact empty projection in the same transaction.
        }
    }

    let mut metadata_node_prefixes = Vec::new();
    for owner in metadata_owner_ids {
        if let Some(rest) = owner.strip_prefix("mdo/") {
            metadata_node_prefixes.extend([
                owner.clone(),
                format!("attribute/{rest}"),
                format!("tabular_section/{rest}"),
                format!("ts_attr/{rest}"),
                format!("form/{rest}"),
                format!("form_item/{rest}"),
                format!("form_attr/{rest}"),
            ]);
        } else if let Some(rest) = owner.strip_prefix("form/") {
            metadata_node_prefixes.extend([
                owner.clone(),
                format!("form_item/{rest}"),
                format!("form_attr/{rest}"),
            ]);
        }
    }
    metadata_node_prefixes.sort();
    metadata_node_prefixes.dedup();

    // Normalised `(file_root_id, file_path)` keys for the changed modules — used both to gate the
    // fast path and to scope the per-module deletes below.
    let changed_files: Vec<String> =
        changed_modules.iter().map(|m| paths[&m.file_id].clone()).collect();

    // Bail to a full rebuild for the aux-casing cases the DB-pinned canonicalisation
    // cannot reproduce (a no-op for normal, consistent-casing edits).
    {
        let src = Connection::open_with_flags(src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening graph db {} read-only", src_path.display()))?;
        incremental_safety_check(&src, &changed_files, &rows, project.search_roots.as_ref())?;
    }

    let mut all_changed_paths = changed_paths.to_vec();
    all_changed_paths.extend(metadata_paths.iter().cloned());
    all_changed_paths.extend(xml_observations.iter().map(|(path, _)| path.clone()));
    all_changed_paths.extend(effective_changed_paths.iter().cloned());
    all_changed_paths.sort();
    all_changed_paths.dedup();
    Ok(BodyPatch {
        rows,
        changed_modules,
        changed_paths: all_changed_paths,
        metadata_node_prefixes,
        clear_graph: all_modules.is_empty(),
        xml_sig_hashes: xml_observations
            .iter()
            .map(|(path, hash)| (path.to_string_lossy().into_owned(), *hash))
            .collect(),
        file_paths,
        unread,
        modules: all_modules.len(),
    })
}

/// Why a patch transaction was not written.
#[derive(Debug)]
pub(crate) enum PatchError {
    /// Another process holds the file's write lock past the wait: nothing was written.
    Busy,
    /// Applying the SQL outlasted its budget; it was interrupted and rolled back.
    Budget,
    Failed(anyhow::Error),
}

/// How long applying a patch's SQL may take, and how long the write lock is waited for.
pub(crate) const PATCH_SQL_BUDGET: Duration = Duration::from_secs(5);
const PATCH_LOCK_WAIT: Duration = Duration::from_millis(250);

/// A patch written to the graph database and not yet committed. Dropping it rolls the
/// transaction back before the connection closes.
pub(crate) struct PatchTransaction {
    conn: Option<Connection>,
    summary: Option<GraphBuildSummary>,
}

impl PatchTransaction {
    /// Make the patch durable. On failure the transaction is rolled back where it can be; the
    /// caller settles what the file holds by its `publication_id`.
    pub(crate) fn commit(mut self) -> rusqlite::Result<GraphBuildSummary> {
        let conn = self.conn.take().expect("a patch transaction commits once");
        let summary = self.summary.take().expect("a patch transaction holds its summary");
        match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(summary),
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

impl Drop for PatchTransaction {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
}

/// Write `patch` into the published graph database `db_path` inside one transaction, without
/// committing it. Nothing else has the file open in this process (the caller holds reads back).
/// The journal is kept between transactions and truncated to nothing, so a crash leaves a
/// journal the next open rolls back: the file holds the old publication or the new one, never
/// a mix. Data, derived counts, unread files, `publication_id`, revision, fingerprints and the
/// final `force_stale` are all in this transaction; nothing is written after it.
pub(crate) fn begin_body_patch(
    db_path: &Path,
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    patch: &BodyPatch,
    meta: &GraphMeta,
    force_stale: bool,
    budget: Duration,
) -> Result<PatchTransaction, PatchError> {
    let failed = |error: rusqlite::Error| PatchError::Failed(error.into());
    let conn = Connection::open(db_path).map_err(failed)?;
    conn.busy_timeout(PATCH_LOCK_WAIT).map_err(failed)?;
    // The settings that make a commit durable are read back: a connection that silently kept
    // another journal mode or `synchronous` would weaken the file it writes into.
    let mode: String =
        conn.query_row("PRAGMA journal_mode = PERSIST", [], |r| r.get(0)).map_err(failed)?;
    conn.execute_batch("PRAGMA journal_size_limit = 0; PRAGMA synchronous = FULL;")
        .map_err(failed)?;
    let synchronous: i64 =
        conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).map_err(failed)?;
    if !mode.eq_ignore_ascii_case("persist") || synchronous != 2 {
        return Err(PatchError::Failed(anyhow::anyhow!(
            "graph database refused durable settings: journal_mode={mode}, synchronous={synchronous}"
        )));
    }
    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => {}
        Err(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseBusy =>
        {
            return Err(PatchError::Busy);
        }
        Err(error) => return Err(failed(error)),
    }
    let interrupt = conn.get_interrupt_handle();
    let transaction = PatchTransaction { conn: Some(conn), summary: None };

    // The budget interrupts the statement in flight; the rollback follows when the
    // transaction is dropped.
    let (finished, timeout) = std::sync::mpsc::channel::<()>();
    let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let expired = Arc::clone(&expired);
        std::thread::spawn(move || {
            if timeout.recv_timeout(budget) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                expired.store(true, std::sync::atomic::Ordering::SeqCst);
                interrupt.interrupt();
            }
        })
    };
    let conn = transaction.conn.as_ref().expect("the connection is open until commit");
    let started = std::time::Instant::now();
    let written = write_body_patch(conn, project, universe, patch, meta, force_stale);
    drop(finished);
    let _ = watchdog.join();
    // Finishing late is an overrun too: the budget bounds how long the file is held.
    let overran = expired.load(std::sync::atomic::Ordering::SeqCst) || started.elapsed() > budget;
    match written {
        Ok(_) | Err(_) if overran => Err(PatchError::Budget),
        Ok(summary) => {
            let mut transaction = transaction;
            transaction.summary = Some(summary);
            Ok(transaction)
        }
        Err(error) => Err(PatchError::Failed(error)),
    }
}

/// A patch applied to a COPY of `src_path` written to `out_path`: the shape the equivalence
/// tests compare against a full rebuild.
#[cfg(test)]
pub(crate) fn update_graph_database_bodies(
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    src_path: &Path,
    out_path: &Path,
    changed_paths: &[PathBuf],
    budget: BatchBudget,
    meta: &GraphMeta,
) -> anyhow::Result<GraphBuildSummary> {
    let patch = compute_body_patch(project, universe, src_path, changed_paths, budget)?;
    std::fs::copy(src_path, out_path)?;
    let transaction =
        begin_body_patch(out_path, project, universe, &patch, meta, false, PATCH_SQL_BUDGET)
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    Ok(transaction.commit()?)
}

fn write_body_patch(
    conn: &Connection,
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    patch: &BodyPatch,
    meta: &GraphMeta,
    force_stale: bool,
) -> anyhow::Result<GraphBuildSummary> {
    let BodyPatch {
        rows,
        changed_modules,
        changed_paths,
        metadata_node_prefixes,
        clear_graph,
        xml_sig_hashes,
        file_paths,
        unread,
        modules,
    } = patch;
    let stat_by_path: FxHashMap<String, &crate::graph::scan::FileStat> =
        universe.stats.iter().map(|s| (s.path.clone(), s)).collect();

    let changed_path_strings: Vec<String> =
        changed_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    install_changed_file_keys(conn, &changed_path_strings, project.search_roots.as_ref())?;
    {
        let tx = conn;

        // The first-seen object spellings the store already owns (Unicode-lowercased
        // key → actual id), loaded before inserting so new objects keep their casing.
        let existing_mdo: std::collections::HashMap<String, String> = {
            let mut stmt = tx.prepare("SELECT id FROM nodes WHERE kind = 'mdo'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).map(|id| (id.to_lowercase(), id)).collect()
        };

        // Drop each changed module's outgoing edges, method nodes, AND module-code
        // node. The module-code node is re-emitted by the reprojection only if the
        // module still has a module-level edge — matching a full rebuild, which emits
        // it solely as an edge endpoint. Deleting it (rather than INSERT OR IGNORE)
        // is what lets a module that lost its last module-level edge shed the node.
        // Only the module's body-derived outgoing edges (from its method/module-code
        // nodes) are reprojected, so only those are deleted. `contains` edges have
        // `mdo`/`form` from-endpoints — never method/module — and the form pass is
        // full-build-only, so the kind filter keeps reprojection from dropping form
        // structure it cannot re-emit.
        tx.execute(
            "DELETE FROM edges WHERE from_id IN \
             (SELECT n.id FROM nodes n JOIN changed_file_keys changed \
              ON changed.root_id = n.file_root_id AND changed.path = n.file_path \
              WHERE n.kind IN ('method', 'module'))",
            [],
        )?;
        tx.execute(
            "DELETE FROM nodes WHERE id IN \
             (SELECT n.id FROM nodes n JOIN changed_file_keys changed \
              ON changed.root_id = n.file_root_id AND changed.path = n.file_path \
              WHERE n.kind IN ('method', 'module'))",
            [],
        )?;

        // A cold workspace with no BSL modules emits no graph projection at all.
        // Dropping all derived rows here makes the last-module transition exact and
        // lets a later first-module patch repopulate the complete projection.
        if *clear_graph {
            tx.execute_batch("DELETE FROM edges; DELETE FROM nodes;")?;
        }

        if !metadata_node_prefixes.is_empty() {
            tx.execute_batch(
                "DROP TABLE IF EXISTS temp.metadata_node_prefixes;
                 CREATE TEMP TABLE metadata_node_prefixes (prefix TEXT PRIMARY KEY) WITHOUT ROWID;
                 DROP TABLE IF EXISTS temp.metadata_current_nodes;
                 CREATE TEMP TABLE metadata_current_nodes (id TEXT PRIMARY KEY) WITHOUT ROWID;",
            )?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO metadata_node_prefixes (prefix) VALUES (?1)",
                )?;
                for prefix in metadata_node_prefixes {
                    stmt.execute(params![prefix])?;
                }
            }
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO metadata_current_nodes (id) VALUES (?1)",
                )?;
                for row in &rows.nodes {
                    let id = match row.kind {
                        "mdo" | "attribute" => canonicalize_aux_id(&existing_mdo, &row.id),
                        _ => row.id.clone(),
                    };
                    stmt.execute(params![id])?;
                }
            }
            // Replacing an owner must replace its own outgoing structural edges. Keep
            // references from unchanged BSL and neighboring forms when their target node
            // still exists in the new projection; those consumers are outside this patch
            // and cannot be reconstructed by the metadata-only projector. Drop incoming
            // edges only when the referenced owner node disappeared.
            tx.execute(
                "DELETE FROM edges WHERE EXISTS (
                    SELECT 1 FROM metadata_node_prefixes p
                    WHERE edges.from_id = p.prefix OR substr(edges.from_id, 1, length(p.prefix) + 1) = p.prefix || '/'
                 ) OR EXISTS (
                    SELECT 1 FROM metadata_node_prefixes p
                    WHERE (edges.to_id = p.prefix OR substr(edges.to_id, 1, length(p.prefix) + 1) = p.prefix || '/')
                      AND (
                        NOT EXISTS (SELECT 1 FROM metadata_current_nodes n WHERE n.id = edges.to_id)
                        OR edges.kind IN ('contains','query_ref','manager_access','manager_creates','data_binding')
                      )
                 )",
                [],
            )?;
            tx.execute(
                "DELETE FROM nodes WHERE EXISTS (
                    SELECT 1 FROM metadata_node_prefixes p
                    WHERE nodes.id = p.prefix OR substr(nodes.id, 1, length(p.prefix) + 1) = p.prefix || '/'
                 )",
                [],
            )?;
        }

        // Re-insert the reprojected nodes, canonicalising aux ids against the store.
        for row in &rows.nodes {
            match row.kind {
                "mdo" | "attribute" => {
                    let id = canonicalize_aux_id(&existing_mdo, &row.id);
                    insert_node_row(tx, row, &id, project.search_roots.as_ref())?;
                }
                _ => insert_node_row(tx, row, &row.id, project.search_roots.as_ref())?,
            }
        }
        // Re-insert the edges, canonicalising aux `to_id`s the same way.
        for edge in &rows.edges {
            let to_id = canonicalize_aux_id(&existing_mdo, &edge.to_id);
            tx.prepare_cached(
                "INSERT INTO edges \
                 (from_id, to_id, kind, provenance, call_start, call_end, call_absent, crosses) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                edge.from_id,
                to_id,
                edge.kind,
                edge.provenance,
                edge.call_start,
                edge.call_end,
                edge.call_site_absent,
                edge.crosses as i64
            ])?;
        }

        // GC aux nodes that lost their last reference. Restricted to pure-sink kinds:
        // module-code nodes are `from_id`-only sources, so a `to_id`-absence sweep
        // would wrongly delete a live caller. An `mdo` may also be a `from_id` — the
        // parent of a `contains` edge to a form — so it must survive on either role:
        // a full rebuild keeps such an object (materialised as the contains-from
        // endpoint) even with no inbound call/query edge. (The `from_id` clause is a
        // no-op when no form nodes exist: `mdo`/`attribute` are never edge sources then.)
        tx.execute(
            "DELETE FROM nodes WHERE kind IN ('mdo', 'attribute') \
             AND id NOT IN (SELECT to_id FROM edges) \
             AND id NOT IN (SELECT from_id FROM edges)",
            [],
        )?;

        // Recompute the whole in-degree table — a delta that forgot the deleted edges'
        // old targets would leave stale degrees.
        tx.execute("DELETE FROM in_degree", [])?;
        tx.execute(
            "INSERT INTO in_degree (id, degree) SELECT to_id, COUNT(*) FROM edges GROUP BY to_id",
            [],
        )?;

        // Merge any casing variants the reprojection observed AMONG the changed
        // modules into the persisted set, so a future reload still refuses the fast
        // path for a newly-inconsistent object a multi-file edit introduced.
        if !rows.casing_variant_objects.is_empty() {
            let existing: String = tx
                .query_row("SELECT value FROM meta WHERE key = 'casing_variants'", [], |r| r.get(0))
                .optional()?
                .unwrap_or_default();
            let mut set: std::collections::BTreeSet<String> =
                existing.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
            set.extend(rows.casing_variant_objects.iter().cloned());
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('casing_variants', ?1)",
                params![set.into_iter().collect::<Vec<_>>().join("\n")],
            )?;
        }

        // Refresh the changed modules' persisted fingerprint + signature hash.
        for module in changed_modules {
            let canonical = file_paths[&module.file_id].to_string_lossy().into_owned();
            let Some(stat) = stat_by_path.get(&canonical).copied() else {
                anyhow::bail!(
                    "incremental update: changed file disappeared from scan: {canonical}"
                );
            };
            let (Some(root_id), Some(path)) =
                durable_file_key(project.search_roots.as_ref(), Some(&canonical))
            else {
                anyhow::bail!(
                    "incremental update: changed file is outside registered roots: {canonical}"
                );
            };
            let content_hash = stat.persisted_content_hash();
            let sig = rows.sig_hashes.get(module).copied();
            let [len, mtime, ctime, ino, dev, observed] =
                observation_columns(stat.persisted_observation());
            tx.execute(
                FILES_INSERT_SQL,
                params![
                    root_id,
                    path,
                    content_hash.as_slice(),
                    stat.fingerprint() as i64,
                    sig.map(|h| h as i64),
                    len,
                    mtime,
                    ctime,
                    ino,
                    dev,
                    observed,
                ],
            )?;
        }

        for changed_path in changed_paths {
            let canonical = changed_path.to_string_lossy().into_owned();
            if stat_by_path.contains_key(&canonical) {
                continue;
            }
            let (Some(root_id), Some(path)) =
                durable_file_key(project.search_roots.as_ref(), Some(&canonical))
            else {
                anyhow::bail!(
                    "incremental update: removed file is outside registered roots: {canonical}"
                );
            };
            tx.execute(
                "DELETE FROM files WHERE root_id = ?1 AND path = ?2",
                params![root_id, path],
            )?;
        }

        // Refresh only XML files whose bytes changed. The watcher already hashed each
        // diff XML once for semantic classification; unchanged XML reuses its persisted
        // signature without a second workspace-wide parse.
        if let Some(roots) = project.search_roots.as_ref() {
            for changed_path in changed_paths.iter().filter(|path| {
                bsl_conventions::str_has_extension(
                    &path.to_string_lossy(),
                    bsl_conventions::XML_EXTENSION,
                )
            }) {
                let canonical = changed_path.to_string_lossy().into_owned();
                let Some(stat) = stat_by_path.get(&canonical).copied() else { continue };
                let Some(key) = stat.key(roots) else { continue };
                let content_hash = stat.persisted_content_hash();
                let sig = xml_sig_hashes.get(&canonical).copied().flatten().or_else(|| {
                    crate::graph::scan::xml_semantic_hash_file(&stat.canonical).map(|h| {
                        u64::from_le_bytes(h[..8].try_into().expect("blake3 hash >= 8 bytes"))
                    })
                });
                let [len, mtime, ctime, ino, dev, observed] =
                    observation_columns(stat.persisted_observation());
                tx.execute(
                    FILES_INSERT_SQL,
                    params![
                        key.root_id,
                        key.path,
                        content_hash.as_slice(),
                        stat.fingerprint() as i64,
                        sig.map(|h| h as i64),
                        len,
                        mtime,
                        ctime,
                        ino,
                        dev,
                        observed,
                    ],
                )?;
            }
        }

        // The artefact's hole set changes ONLY for the modules this patch rewrote —
        // the filter is needed on both sides, and for the same reason. A module's rows
        // are restored, or dropped, solely by the pass that lowered it: an inherited
        // hole that merely became readable still has no rows here, and a module that
        // went dark while nobody edited it still has the rows the build left. Recording
        // the latter would claim its rows are absent when they are only stale, and
        // nothing could ever take it back out — the subtraction releases a path only
        // when a later patch rewrites it, and a module nobody edits is never rewritten.
        // (That such a module is even a candidate is not an accident of `changed`: the
        // reprojection's index pass opens the WHOLE universe through the same loader,
        // so `unread` reports far more than this patch touched.)
        //
        // Keyed by the canonical spelling `changed_paths` carries, NOT by the
        // '/'-normalised `(file_root_id, file_path)` spelling: the unread paths are raw `PathBuf`s,
        // and on Windows the two differ.
        {
            let rewritten: std::collections::BTreeSet<bsl_search::FileKey> = changed_paths
                .iter()
                .filter_map(|path| {
                    let path_text = path.to_string_lossy();
                    let (root_id, key_path) =
                        durable_file_key(project.search_roots.as_ref(), Some(path_text.as_ref()));
                    root_id.zip(key_path).map(|(root, key)| bsl_search::FileKey::new(root, key))
                })
                .collect();
            let mut carried: BTreeSet<bsl_search::FileKey> = read_unread_keys_strict(tx)?
                .into_iter()
                .filter(|key| !rewritten.contains(key))
                .collect();
            let unread_keys =
                unread_keys_from_paths(unread, project.search_roots.as_ref(), Some(universe))?;
            carried.extend(unread_keys.into_iter().filter(|key| rewritten.contains(key)));
            write_unread_keys(tx, &carried)?;
        }

        // Refresh the reverse index of unresolved calls for the reprojected modules:
        // drop their old rows, insert their fresh ones. Unchanged modules' rows stay.
        tx.execute(
            "DELETE FROM unresolved_calls WHERE EXISTS (
                 SELECT 1 FROM changed_file_keys changed
                 WHERE changed.root_id = unresolved_calls.caller_root_id
                   AND changed.path = unresolved_calls.caller_path
             )",
            [],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO unresolved_calls \
                 (target_scope, method_lower, caller_root_id, caller_path) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (target_scope, method_lower, caller_file) in &rows.unresolved_calls {
                let (Some(root_id), Some(path)) =
                    durable_file_key(project.search_roots.as_ref(), Some(caller_file))
                else {
                    anyhow::bail!(
                        "incremental unresolved call caller is outside registered workspace roots: {caller_file}"
                    );
                };
                stmt.execute(params![target_scope, method_lower, root_id, path])?;
            }
        }

        // Refresh build metadata + derived counts; a clean incremental snapshot is
        // never force-stale.
        let node_count: i64 = tx.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let edge_count: i64 = tx.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        let meta_rows: [(&str, String); 9] = [
            ("publication_id", meta.publication_id.clone()),
            ("revision", meta.revision.to_string()),
            ("fingerprint", meta.fingerprint.files.to_string()),
            ("topology_fp", meta.fingerprint.topology.to_string()),
            ("files", modules.to_string()),
            ("built_at", meta.built_at.clone()),
            ("nodes", node_count.to_string()),
            ("edges", edge_count.to_string()),
            ("force_stale", (if force_stale { "1" } else { "0" }).to_string()),
        ];
        for (key, value) in &meta_rows {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
    }

    let node_rows = rows.nodes.len();
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
    Ok(GraphBuildSummary {
        modules: *modules,
        node_rows,
        edges: edges as usize,
        module_sig_hashes: rows.sig_hashes.clone(),
        // Variants observed among the changed modules (merged into the persisted set
        // above); pre-existing variants for untouched objects remain in the copied db.
        casing_variant_objects: rows.casing_variant_objects.clone(),
        // The reprojected modules' unresolved refs were refreshed in the patch above.
        unresolved_calls: rows.unresolved_calls.clone(),
    })
}

/// A changed module's recomputed body-free profile: its signature hash plus the
/// resolvable-name surface a caller-delta eligibility check needs — the lowercased
/// names of its exported methods, and whether any two methods fold to the same name
/// (a collision makes "exported name" ≠ "resolvable name", since resolution is
/// first-wins).
pub struct ModuleProfile {
    pub sig_hash: u64,
    pub exported_lower: std::collections::BTreeSet<String>,
    pub has_collision: bool,
    /// The module's bytes could not be read. Not a property of its declarations —
    /// which is exactly why the caller-delta cannot work with it, see
    /// [`caller_delta_plan`].
    pub unread: bool,
}

impl ModuleProfile {
    pub(crate) fn removed() -> Self {
        Self {
            sig_hash: 0,
            exported_lower: std::collections::BTreeSet::new(),
            has_collision: false,
            unread: false,
        }
    }
}

/// Recompute each module at `changed_paths`'s profile, for the incremental
/// eligibility checks (sig drift, and the caller-delta resolvable-name surface).
/// Builds a tiny resident index over only those modules — these reads are a module's
/// own item-tree + dispatch, no cross-module data — so it stays cheap. Keyed by
/// canonical path.
///
/// `files` is the operation's ALREADY-SCANNED enumeration: profiling must judge the
/// same universe the eligibility diff saw, not a fresh walk that may already differ.
pub fn recompute_module_profiles(
    project: &crate::graph::ProjectSnapshot,
    files: &[(FileId, PathBuf)],
    changed_paths: &[PathBuf],
) -> anyhow::Result<FxHashMap<String, ModuleProfile>> {
    use ide::graph_index::GraphIndex;

    let source_root = crate::graph::build_source_root(files);

    let changed_set: std::collections::HashSet<&Path> =
        changed_paths.iter().map(|p| p.as_path()).collect();
    let changed: Vec<(ModuleId, PathBuf)> = files
        .iter()
        .filter(|(_, p)| changed_set.contains(p.as_path()))
        .map(|(f, p)| (ModuleId::new(*f), p.clone()))
        .collect();

    let batch_files: Vec<(FileId, PathBuf)> =
        changed.iter().map(|(m, p)| (m.file_id, p.clone())).collect();
    // Profiling only compares signature hashes; the unread report belongs to the
    // passes that publish rows, not here.
    let db = db_for_files(&source_root, &batch_files, &project.configs, None).db;
    let modules: Vec<ModuleId> = changed.iter().map(|(m, _)| *m).collect();
    let index = GraphIndex::build(&db, &modules);

    let mut out = FxHashMap::default();
    for (module, path) in &changed {
        let Some(sig_hash) = index.module_sig_hash(*module) else {
            continue;
        };
        let methods = index.module_methods(*module).unwrap_or_default();
        let lowers: Vec<String> = methods.iter().map(|(n, _)| n.to_lowercase()).collect();
        let has_collision =
            lowers.iter().collect::<std::collections::HashSet<_>>().len() != lowers.len();
        let exported_lower: std::collections::BTreeSet<String> =
            methods.iter().filter(|(_, exp)| *exp).map(|(n, _)| n.to_lowercase()).collect();
        out.insert(
            path.to_string_lossy().into_owned(),
            ModuleProfile {
                sig_hash,
                exported_lower,
                has_collision,
                unread: index.is_unread(*module).unwrap_or(false),
            },
        );
    }

    Ok(out)
}

/// Plan the caller-delta for a set of signature-changed modules (the body-only fast
/// path is not eligible because their signature moved). Returns:
/// - `Ok(Some(caller_files))` — reprojecting the changed modules PLUS the returned
///   callers reproduces a full rebuild. Callers are the union of: modules with a
///   stored edge INTO a changed module (covers removal/unexport/dispatch/case-rename),
///   and modules whose previously-unresolved `B.<name>()` would newly resolve when B
///   gains a resolvable `name` (looked up in the `unresolved_calls` reverse index).
///   Excludes the changed modules themselves.
/// - `Ok(None)` — not eligible: a first-wins name collision (invalid-BSL shadowing,
///   old or new), or an added resolvable name on a module whose scope is not
///   name-keyed (so its callers cannot be found). The caller must do a full rebuild.
///
/// `sig_changed` pairs each changed module's normalised `(file_root_id, file_path)` key with its
/// freshly-recomputed [`ModuleProfile`].
pub(crate) fn caller_delta_plan_in(
    conn: &Connection,
    sig_changed: &[(&str, &ModuleProfile)],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let plan = plan_caller_delta(conn, sig_changed, roots);
    // The handle goes back to its pool; the keys of this plan must not ride along.
    let dropped = conn.execute_batch("DROP TABLE IF EXISTS temp.changed_file_keys;");
    let plan = plan?;
    dropped?;
    Ok(plan)
}

/// [`caller_delta_plan_in`] over a file opened by path, for a test planning against a
/// database it built by hand.
#[cfg(test)]
pub fn caller_delta_plan(
    db_path: &Path,
    sig_changed: &[(&str, &ModuleProfile)],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    caller_delta_plan_in(&conn, sig_changed, roots)
}

fn plan_caller_delta(
    conn: &Connection,
    sig_changed: &[(&str, &ModuleProfile)],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let changed_file_names: Vec<String> =
        sig_changed.iter().map(|(file, _)| (*file).to_owned()).collect();
    install_changed_file_keys(conn, &changed_file_names, roots)?;

    // What the stored artefact recorded as unreadable. Compared verbatim: both this and
    // the keys of `sig_changed` are the raw canonical spelling of the same scanned
    // path — `unread_paths` from the batch loader's `PathBuf`s, the keys from
    // `files.path`. Normalising either side would break the match on the one platform
    // where the two spellings could differ at all.
    let was_unread: std::collections::HashSet<bsl_search::FileKey> =
        read_unread_keys_strict(conn)?.into_iter().collect();

    let mut index_callers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (file, profile) in sig_changed {
        let (root_id, path) = durable_file_key(roots, Some(file));
        let Some(file_key) =
            root_id.zip(path).map(|(root_id, path)| bsl_search::FileKey::new(root_id, path))
        else {
            return Ok(None);
        };
        if profile.has_collision {
            return Ok(None); // first-wins shadowing — exported set ≠ resolvable set
        }
        // A body that has just become unreadable bars callers from resolving into any
        // body BEHIND it — a sibling body of the same common module, in another file
        // entirely. Those callers hold a RESOLVED edge into the sibling's node and
        // nothing at all pointing here, so neither fan-out below can reach them and the
        // only correct answer is a full rebuild.
        if profile.unread {
            return Ok(None);
        }
        // The mirror image: this body has just become readable, and the calls its
        // barrier used to bar now resolve — into that same sibling body. What they were
        // barred from is this module's SCOPE, not any name this file declares, so the
        // by-name lookup below cannot find them (the disputed method may well be
        // declared only next door). Take every caller the artefact recorded as blocked
        // on this scope, whatever the name.
        if was_unread.contains(&file_key) {
            let Some(scope) = ide::scope_for_path(file) else {
                return Ok(None); // not name-keyed → its callers aren't indexable
            };
            let folded_scope =
                ide::folded_common_scope_for_path(file).unwrap_or_else(|| scope.clone());
            let mut stmt = conn.prepare(
                "SELECT caller_root_id, caller_path FROM unresolved_calls \
                 WHERE target_scope = ?1 OR target_scope = ?2",
            )?;
            let rows = stmt.query_map(params![scope, folded_scope], |r| {
                Ok(bsl_search::FileKey::new(r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let key = row?;
                let Some(path) = roots.and_then(|roots| roots.resolve_walked(&key)) else {
                    return Ok(None);
                };
                index_callers.insert(path.to_string_lossy().into_owned());
            }
        }
        // OLD resolvable surface from the stored method nodes.
        let mut stmt = conn.prepare(
            "SELECT name, is_export FROM nodes \
             WHERE file_root_id = ?1 AND file_path = ?2 AND kind = 'method'",
        )?;
        let rows = stmt.query_map(params![file_key.root_id, file_key.path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0) != 0))
        })?;
        let mut old_lowers: Vec<String> = Vec::new();
        let mut old_exported: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for row in rows {
            let (name, exported) = row?;
            let lower = name.to_lowercase();
            if exported {
                old_exported.insert(lower.clone());
            }
            old_lowers.push(lower);
        }
        let old_collision =
            old_lowers.iter().collect::<std::collections::HashSet<_>>().len() != old_lowers.len();
        if old_collision {
            return Ok(None);
        }
        // Newly-resolvable names: callers that called them were previously unresolved
        // (dropped from `edges`), so find them through the reverse index by scope+name.
        let added: Vec<&String> =
            profile.exported_lower.iter().filter(|n| !old_exported.contains(*n)).collect();
        if !added.is_empty() {
            let Some(scope) = ide::scope_for_path(file) else {
                return Ok(None); // not name-keyed → its callers aren't indexable
            };
            let folded_scope =
                ide::folded_common_scope_for_path(file).unwrap_or_else(|| scope.clone());
            let mut stmt = conn.prepare(
                "SELECT caller_root_id, caller_path FROM unresolved_calls \
                     WHERE (target_scope = ?1 OR target_scope = ?2) AND method_lower = ?3",
            )?;
            for name in added {
                let rows = stmt.query_map(params![scope, folded_scope, name], |r| {
                    Ok(bsl_search::FileKey::new(r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let key = row?;
                    let Some(path) = roots.and_then(|roots| roots.resolve_walked(&key)) else {
                        return Ok(None);
                    };
                    index_callers.insert(path.to_string_lossy().into_owned());
                }
            }
        }
    }

    // Resolved callers: modules with a stored edge into a changed module's method node.
    let changed_files: std::collections::BTreeSet<&str> =
        sig_changed.iter().map(|(f, _)| *f).collect();

    // A signature change in an event-subscription handler module can invalidate its
    // config-level `mdo -> method` subscription edge — but that edge's source is an
    // `mdo` node, which the resolved-caller fan-out below never selects (it asks for
    // BSL source by kind), and the body-only reproject never re-derives Phase F.
    // Bail to a full rebuild so a removed/unexported/renamed handler cannot leave a
    // dangling subscription edge.
    {
        let mut stmt = conn.prepare(
            "SELECT 1 FROM edges e JOIN nodes n1 ON e.to_id = n1.id \
             JOIN changed_file_keys changed ON changed.root_id = n1.file_root_id \
                 AND changed.path = n1.file_path \
             WHERE n1.kind = 'method' AND e.kind = 'event_subscription' LIMIT 1",
        )?;
        if stmt.exists([])? {
            return Ok(None);
        }
    }

    let mut stmt = conn.prepare(
        "SELECT DISTINCT n2.file_root_id, n2.file_path FROM edges e \
         JOIN nodes n1 ON e.to_id = n1.id \
         JOIN nodes n2 ON e.from_id = n2.id \
         JOIN changed_file_keys changed ON changed.root_id = n1.file_root_id \
             AND changed.path = n1.file_path \
         WHERE n1.kind = 'method' \
           AND n2.kind IN ('method','module') AND n2.file_path IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(bsl_search::FileKey::new(r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut callers: std::collections::BTreeSet<String> = index_callers;
    for row in rows {
        let key = row?;
        let Some(path) = roots.and_then(|roots| roots.resolve_walked(&key)) else {
            return Ok(None);
        };
        callers.insert(path.to_string_lossy().into_owned());
    }
    Ok(Some(
        callers
            .into_iter()
            .filter(|f| !changed_files.contains(f.as_str()))
            .map(PathBuf::from)
            .collect(),
    ))
}

#[cfg(test)]
mod stall_report_tests {
    use super::write_stall_report;

    #[test]
    fn episodes_append_to_one_report_file() {
        let dir = tempfile::tempdir().unwrap();
        write_stall_report(dir.path(), 600, "call_edges batch 52/59", "t1:S:futex");
        write_stall_report(dir.path(), 1200, "call_edges batch 52/59", "t1:S:futex");
        let report =
            std::fs::read_to_string(dir.path().join("bsl-graph-stall-report.txt")).unwrap();
        assert!(report.contains("stalled for 600s"));
        assert!(report.contains("stalled for 1200s"));
    }

    #[test]
    fn missing_directory_is_a_warning_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        write_stall_report(&dir.path().join("gone"), 600, "index batch 1/2", "t1:S:futex");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_node(id: &str, name: &str) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            kind: "method",
            name: name.to_string(),
            qualified: format!("ОбщийМодуль.X.{name}"),
            module: Some("ОбщийМодуль.X".to_string()),
            file: Some("CommonModules/X/Ext/Module.bsl".to_string()),
            name_offset: Some(10),
            sig_end: Some(20),
            src_start: Some(0),
            src_end: Some(40),
            dispatch: vec!["server"],
            is_export: Some(true),
            addressable: true,
        }
    }

    fn edge(from: &str, to: &str) -> EdgeRow {
        EdgeRow {
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: "call",
            provenance: "resolved",
            call_start: None,
            call_end: None,
            call_site_absent: Some(ide::NO_CALL_SITE),
            crosses: false,
        }
    }

    fn open(path: &Path) -> Connection {
        Connection::open(path).unwrap()
    }

    #[test]
    fn writes_and_reads_back_nodes_and_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_nodes(&[
            method_node("method/common/X/A", "A"),
            method_node("method/common/X/B", "B"),
        ])
        .unwrap();
        w.write_edges(&[edge("method/common/X/A", "method/common/X/B")]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp { files: 42, topology: 7 },
            files: 1,
            built_at: "2026-06-01T00:00:00Z".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let conn = open(&path);
        let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0)).unwrap();
        assert_eq!((nodes, edges), (2, 1));

        // finalize derives the node/edge counts and records them in `meta`.
        let meta_nodes: String =
            conn.query_row("SELECT value FROM meta WHERE key = 'nodes'", [], |r| r.get(0)).unwrap();
        let meta_edges: String =
            conn.query_row("SELECT value FROM meta WHERE key = 'edges'", [], |r| r.get(0)).unwrap();
        assert_eq!((meta_nodes.as_str(), meta_edges.as_str()), ("2", "1"));

        let (name, dispatch, is_export, addressable): (String, String, i64, i64) = conn
            .query_row(
                "SELECT name, dispatch, is_export, addressable FROM nodes WHERE id = ?1",
                params!["method/common/X/A"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (name.as_str(), dispatch.as_str(), is_export, addressable),
            ("A", "server", 1, 1)
        );

        let schema: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(schema, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn first_node_spelling_wins_on_duplicate_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let mut first = method_node("method/common/X/A", "A");
        first.qualified = "first".to_string();
        let mut second = method_node("method/common/X/A", "A");
        second.qualified = "second".to_string();

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_nodes(&[first]).unwrap();
        w.write_nodes(&[second]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let conn = open(&path);
        let qualified: String = conn
            .query_row(
                "SELECT qualified FROM nodes WHERE id = ?1",
                params!["method/common/X/A"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(qualified, "first", "INSERT OR IGNORE keeps the first-seen spelling");
    }

    /// Only a row whose bytes were read carries a reusable hash: an unreadable file's marker
    /// must never come back as its content.
    #[test]
    fn only_read_rows_are_offered_for_reuse() {
        use crate::graph::content_hash::{Observation, StatIdentity};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        let observation = Observation {
            stat: StatIdentity { len: 3, mtime_ns: 5, change: None },
            hash: [1; 32],
            observed_at_ns: 7,
        };
        let row = |path: &str, observation| FileFingerprint {
            root_id: "".to_string(),
            path: path.to_string(),
            content_hash: [1; 32],
            fingerprint: 1,
            sig_hash: None,
            observation,
        };
        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_files(&[row("Read.bsl", Some(observation)), row("Unread.bsl", None)]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let roots = bsl_search::WorkspaceRoots::build(dir.path(), dir.path(), &[]).0;
        let offered = read_stored_observations(&path, &roots);
        assert_eq!(offered.len(), 1, "{offered:?}");
        assert!(offered[0].0.ends_with("Read.bsl"));
        assert_eq!(offered[0].1, observation);
    }

    #[test]
    fn write_files_round_trips_fingerprints_and_null_sig_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_files(&[
            FileFingerprint {
                root_id: "".to_string(),
                path: "cfg/A.bsl".to_string(),
                content_hash: [1; 32],
                fingerprint: 111,
                sig_hash: None,
                observation: None,
            },
            FileFingerprint {
                root_id: "".to_string(),
                path: "cfg/A.xml".to_string(),
                content_hash: [2; 32],
                fingerprint: 222,
                sig_hash: None,
                observation: None,
            },
        ])
        .unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let conn = open(&path);
        let (fp, sig): (i64, Option<i64>) = conn
            .query_row(
                "SELECT fingerprint, sig_hash FROM files WHERE root_id = '' AND path = 'cfg/A.bsl'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp as u64, 111);
        assert_eq!(sig, None, "sig_hash is NULL until the body-only fast path fills it");

        let xml_fp: i64 = conn
            .query_row(
                "SELECT fingerprint FROM files WHERE root_id = '' AND path = 'cfg/A.xml'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(xml_fp as u64, 222);
    }

    #[test]
    fn in_degree_counts_incoming_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_nodes(&[method_node("a", "A"), method_node("b", "B"), method_node("hub", "Hub")])
            .unwrap();
        w.write_edges(&[edge("a", "hub"), edge("b", "hub"), edge("a", "b")]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let conn = open(&path);
        let hub: i64 = conn
            .query_row("SELECT degree FROM in_degree WHERE id = 'hub'", [], |r| r.get(0))
            .unwrap();
        let b: i64 = conn
            .query_row("SELECT degree FROM in_degree WHERE id = 'b'", [], |r| r.get(0))
            .unwrap();
        assert_eq!((hub, b), (2, 1));
        // A source-only node has no in_degree row.
        let a_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM in_degree WHERE id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a_rows, 0);
    }

    #[test]
    fn create_truncates_a_prior_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_nodes(&[method_node("stale", "Stale")]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        // A second build at the same path must not see the prior row.
        let w2 = GraphDbWriter::create(&path).unwrap();
        w2.finalize(&GraphMeta {
            revision: 2,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();

        let conn = open(&path);
        let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(nodes, 0, "create() discards the prior file");
    }

    #[test]
    fn caller_delta_bails_to_full_rebuild_for_subscription_handler() {
        // A signature change in a module that handles an event subscription must NOT take
        // the body-only caller-delta path: the subscription's `mdo -> method` edge has a
        // fileless source the delta never revisits, so it would go stale. The planner must
        // return None (force a full rebuild).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");

        let handler = method_node("method/common/X/Обработчик", "Обработчик");
        let mut subscription = method_node("mdo/EventSubscription/ПриЗаписи", "ПриЗаписи");
        subscription.kind = "mdo";
        subscription.module = None;
        subscription.file = None; // config-level node: no owning file
        subscription.is_export = None;

        let mut w = GraphDbWriter::create(&path).unwrap();
        w.write_nodes(&[handler, subscription]).unwrap();
        let mut sub_edge = edge("mdo/EventSubscription/ПриЗаписи", "method/common/X/Обработчик");
        sub_edge.kind = "event_subscription";
        sub_edge.provenance = "string_resolved";
        w.write_edges(&[sub_edge]).unwrap();
        w.finalize(&GraphMeta {
            revision: 1,
            fingerprint: GraphFp { files: 1, topology: 0 },
            files: 1,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        })
        .unwrap();
        open(&path)
            .execute("INSERT INTO meta (key, value) VALUES ('unread_paths', '[]')", [])
            .unwrap();

        let profile = ModuleProfile {
            sig_hash: 999,
            exported_lower: std::collections::BTreeSet::new(),
            has_collision: false,
            unread: false,
        };
        let plan = caller_delta_plan(&path, &[("CommonModules/X/Ext/Module.bsl", &profile)], None)
            .unwrap();
        assert!(
            plan.is_none(),
            "a signature change to a subscription handler module must force a full rebuild"
        );
    }

    /// Everything a build needs to see one catalog and one module.
    fn stage_catalog_workspace(root: &Path, module_body: &str) {
        std::fs::create_dir_all(root.join("CommonModules/Модуль/Ext")).unwrap();
        std::fs::create_dir_all(root.join("Catalogs")).unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            root.join("CommonModules/Модуль.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Модуль</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(root.join("CommonModules/Модуль/Ext/Module.bsl"), module_body).unwrap();
    }

    fn write_catalog(root: &Path) {
        std::fs::write(
            root.join("Catalogs/Товары.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties><Name>Товары</Name></Properties>
    </Catalog>
</MetaDataObject>"#,
        )
        .unwrap();
    }

    fn build_meta() -> GraphMeta {
        GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        }
    }

    fn scanned_project(
        root: &Path,
    ) -> (crate::graph::ProjectSnapshot, crate::graph::universe::ScannedUniverse) {
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        (project, universe)
    }

    /// `None` covers both "no such row" and "a row with no file": neither is a
    /// placed object, and the tests below care only about that. A placed object is
    /// asserted by its durable `(root_id, path)` pair, never by a physical address.
    fn stored_file(db: &Path, id: &str) -> Option<(String, String)> {
        use rusqlite::OptionalExtension;
        Connection::open(db)
            .unwrap()
            .query_row("SELECT file_root_id, file_path FROM nodes WHERE id = ?1", [id], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .optional()
            .unwrap()
            .and_then(|(root_id, path)| root_id.zip(path))
    }

    /// The path the build is expected to store, taken from the tree rather than
    /// spelled out: a hand-built string would agree with a row written in any
    /// shape, and the shape is the whole point — the dictionary resolves this
    /// against the scanned universe.
    fn expected_file(_root: &Path, relative: &str) -> (String, String) {
        (String::new(), relative.replace('\\', "/"))
    }

    /// The full build is the path that places an object: the catalog, role and
    /// subsystem passes run there and nowhere else, and the incremental pass
    /// never overwrites a row it finds.
    #[test]
    fn a_full_build_stores_the_file_an_object_is_defined_by() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        stage_catalog_workspace(
            root,
            "&НаСервере\nПроцедура Выполнить() Экспорт\nСправочники.Товары.СоздатьЭлемент();\nКонецПроцедуры",
        );
        write_catalog(root);

        let db = root.join(".build/graph.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let (project, universe) = scanned_project(root);
        build_graph_database(
            &project,
            &universe,
            &db,
            stdx::batch::BatchBudget::files(1),
            &build_meta(),
        )
        .unwrap();

        assert_eq!(
            stored_file(&db, "mdo/Catalog/Товары"),
            Some(expected_file(root, "Catalogs/Товары.xml")),
        );
    }

    /// The incremental path emits objects as the endpoints of the edges it
    /// reprojects. It cannot overwrite a row already stored, so the only case in
    /// which its map is observable is an object that did not exist at the last
    /// full build — which is exactly the case a relaxed eligibility gate would
    /// start delivering here.
    #[test]
    fn the_incremental_path_places_an_object_that_appeared_after_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        stage_catalog_workspace(root, "&НаСервере\nПроцедура Выполнить() Экспорт\nКонецПроцедуры");

        let db_pre = root.join(".build/pre.db");
        std::fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        let (project, universe) = scanned_project(root);
        build_graph_database(
            &project,
            &universe,
            &db_pre,
            stdx::batch::BatchBudget::files(1),
            &build_meta(),
        )
        .unwrap();
        assert_eq!(
            stored_file(&db_pre, "mdo/Catalog/Товары"),
            None,
            "the object must be unplaced before the edit, or the incremental write is invisible",
        );

        write_catalog(root);
        let module = root.join("CommonModules/Модуль/Ext/Module.bsl");
        std::fs::write(
            &module,
            "&НаСервере\nПроцедура Выполнить() Экспорт\nСправочники.Товары.СоздатьЭлемент();\nКонецПроцедуры",
        )
        .unwrap();

        let (edited_project, edited_universe) = scanned_project(root);
        let db_incremental = root.join(".build/incremental.db");
        update_graph_database_bodies(
            &edited_project,
            &edited_universe,
            &db_pre,
            &db_incremental,
            &[module.canonicalize().unwrap()],
            stdx::batch::BatchBudget::files(1),
            &build_meta(),
        )
        .unwrap();

        assert_eq!(
            stored_file(&db_incremental, "mdo/Catalog/Товары"),
            Some(expected_file(root, "Catalogs/Товары.xml")),
        );
    }

    #[test]
    fn variable_insertion_preserves_durable_body_only_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let module_path = root.join("CommonModules/Модуль/Ext/Module.bsl");
        std::fs::create_dir_all(module_path.parent().expect("module path has a parent")).unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            root.join("CommonModules/Модуль.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Модуль</Name>
            <Global>false</Global>
            <ClientManagedApplication>false</ClientManagedApplication>
            <Server>true</Server>
            <ExternalConnection>false</ExternalConnection>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(&module_path, "&НаСервере\nПроцедура Выполнить() Экспорт\nКонецПроцедуры")
            .unwrap();

        let meta = || GraphMeta {
            revision: 1,
            fingerprint: GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let scanned = |root: &Path| {
            let project = crate::graph::ProjectSnapshot::load(root);
            let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
            (project, universe)
        };
        let db_pre = root.join(".build/pre.db");
        std::fs::create_dir_all(db_pre.parent().expect("database path has a parent")).unwrap();
        let (project, universe) = scanned(root);
        build_graph_database(
            &project,
            &universe,
            &db_pre,
            stdx::batch::BatchBudget::files(1),
            &meta(),
        )
        .expect("initial build succeeds");

        let changed = vec![module_path.canonicalize().expect("module file exists")];
        let path_key = changed[0].to_string_lossy().into_owned();
        let stored_sig: i64 = Connection::open(&db_pre)
            .unwrap()
            .query_row("SELECT sig_hash FROM files WHERE path LIKE '%Module.bsl'", [], |row| {
                row.get(0)
            })
            .unwrap();

        // When: a top-level variable shifts method local ids but preserves its signature.
        std::fs::write(
            &module_path,
            "Перем Состояние;\n&НаСервере\nПроцедура Выполнить() Экспорт\nКонецПроцедуры",
        )
        .unwrap();
        let (edited_project, edited_universe) = scanned(root);
        let profiles = recompute_module_profiles(&edited_project, &edited_universe.files, &changed)
            .expect("profile recomputation succeeds");
        let profile = profiles.get(&path_key).expect("changed module has a profile");
        assert_eq!(
            profile.sig_hash,
            u64::from_ne_bytes(stored_sig.to_ne_bytes()),
            "the durable signature gate must retain the body-only path"
        );

        let db_incremental = root.join(".build/incremental.db");
        update_graph_database_bodies(
            &edited_project,
            &edited_universe,
            &db_pre,
            &db_incremental,
            &changed,
            stdx::batch::BatchBudget::files(1),
            &meta(),
        )
        .expect("body-only incremental update succeeds");
        let db_full = root.join(".build/full.db");
        let (project, universe) = scanned(root);
        build_graph_database(
            &project,
            &universe,
            &db_full,
            stdx::batch::BatchBudget::files(1),
            &meta(),
        )
        .expect("full rebuild succeeds");

        let dump = |path: &Path| {
            let conn = Connection::open(path).unwrap();
            let mut output = Vec::new();
            for (label, query, columns) in [
                (
                    "nodes",
                    "SELECT id, kind, name, qualified, module, file_root_id, file_path, name_offset, \
                     sig_end, src_start, src_end, dispatch, is_export, addressable FROM nodes ORDER BY id",
                    14,
                ),
                (
                    "edges",
                    "SELECT from_id, to_id, kind, provenance, crosses FROM edges \
                     ORDER BY from_id, to_id, kind, provenance, crosses",
                    5,
                ),
                ("in_degree", "SELECT id, degree FROM in_degree ORDER BY id", 2),
                (
                    "unresolved_calls",
                    "SELECT target_scope, method_lower, caller_root_id, caller_path \
                     FROM unresolved_calls ORDER BY target_scope, method_lower, caller_root_id, caller_path",
                    4,
                ),
                (
                    "files",
                    "SELECT root_id, path, content_hash, fingerprint, sig_hash \
                     FROM files ORDER BY root_id, path",
                    5,
                ),
            ] {
                let mut statement = conn.prepare(query).unwrap();
                let rows = statement
                    .query_map([], |row| {
                        let mut values = Vec::with_capacity(columns);
                        for column in 0..columns {
                            values.push(
                                row.get::<_, rusqlite::types::Value>(column)
                                    .map(|value| format!("{value:?}"))?,
                            );
                        }
                        Ok(values.join("|"))
                    })
                    .unwrap();
                output.extend(rows.map(|row| format!("{label}:{}", row.unwrap())));
            }
            output
        };

        // Then: the durable body-only update exactly matches a full rebuild.
        assert_eq!(dump(&db_incremental), dump(&db_full));
    }

    #[test]
    fn constant_manager_call_persists_method_to_method_call_edge() {
        // Given: a constant manager exporting `Цель` and a common-module caller.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let manager_module = root.join("Constants/Тест/Ext/ManagerModule.bsl");
        let caller_module = root.join("CommonModules/Тест/Ext/Module.bsl");
        std::fs::create_dir_all(manager_module.parent().expect("manager module has a parent"))
            .unwrap();
        std::fs::create_dir_all(caller_module.parent().expect("caller module has a parent"))
            .unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            root.join("Constants/Тест.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <Constant uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Тест</Name></Properties>
    </Constant>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(
            root.join("CommonModules/Тест.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Тест</Name>
            <Global>false</Global>
            <ClientManagedApplication>false</ClientManagedApplication>
            <Server>true</Server>
            <ExternalConnection>false</ExternalConnection>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(&manager_module, "Процедура Цель() Экспорт\nКонецПроцедуры").unwrap();
        std::fs::write(
            &caller_module,
            "Процедура Источник() Экспорт\nКонстанты.Тест.Цель();\nКонецПроцедуры",
        )
        .unwrap();
        let path = root.join(".build/bsl-graph.db");
        std::fs::create_dir_all(path.parent().expect("graph database has a parent")).unwrap();

        // When: the workspace graph is persisted through the production builder.
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        build_graph_database(
            &project,
            &universe,
            &path,
            stdx::batch::BatchBudget::files(1),
            &GraphMeta {
                revision: 1,
                fingerprint: GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .unwrap();

        // Then: the resolved manager target is a durable method-to-method call, not only MDO access.
        let conn = open(&path);
        let call_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_id = ?1 AND to_id = ?2 AND kind = 'call'",
                params!["method/common/Тест/Источник", "method/manager/Constant/Тест/Цель",],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(call_count, 1, "constant-manager method call must persist as a call edge");
    }
    #[test]
    fn call_hierarchy_sqlite_method_digest() {
        // Given: persisted method calls alongside metadata and SetAction rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        let mut subscription = method_node("mdo/EventSubscription/ПриЗаписи", "ПриЗаписи");
        subscription.kind = "mdo";
        subscription.module = None;
        subscription.file = None;
        subscription.is_export = None;

        let mut writer = GraphDbWriter::create(&path).unwrap();
        writer
            .write_nodes(&[
                method_node("method/common/Caller/Прямой", "Прямой"),
                method_node("method/common/Caller/Оповещение", "Оповещение"),
                method_node("method/common/Caller/Ожидание", "Ожидание"),
                method_node("method/common/Target/Цель", "Цель"),
                subscription,
            ])
            .unwrap();
        writer
            .write_edges(&[
                edge("method/common/Caller/Прямой", "method/common/Target/Цель"),
                EdgeRow {
                    from_id: "method/common/Caller/Оповещение".to_string(),
                    to_id: "method/common/Target/Цель".to_string(),
                    kind: "notify_ref",
                    provenance: "string_resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
                EdgeRow {
                    from_id: "method/common/Caller/Ожидание".to_string(),
                    to_id: "method/common/Target/Цель".to_string(),
                    kind: "idle_handler",
                    provenance: "string_resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
                EdgeRow {
                    from_id: "mdo/EventSubscription/ПриЗаписи".to_string(),
                    to_id: "method/common/Target/Цель".to_string(),
                    kind: "event_subscription",
                    provenance: "string_resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
                EdgeRow {
                    from_id: "method/common/Caller/Прямой".to_string(),
                    to_id: "method/common/Target/Цель".to_string(),
                    kind: "set_action",
                    provenance: "string_resolved",
                    call_start: None,
                    call_end: None,
                    call_site_absent: Some(ide::NO_CALL_SITE),
                    crosses: false,
                },
            ])
            .unwrap();
        writer
            .finalize(&GraphMeta {
                revision: 1,
                fingerprint: GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            })
            .unwrap();

        // When: the read-only SQLite oracle projects method-to-method calls.
        let digest = read_sqlite_method_call_digest(&path).unwrap();

        // Then: direct, notify, and idle handlers remain; metadata and SetAction do not.
        assert_eq!(
            digest.rows(),
            &[
                (
                    "method/common/Target/Цель".to_string(),
                    "method/common/Caller/Ожидание".to_string(),
                ),
                (
                    "method/common/Target/Цель".to_string(),
                    "method/common/Caller/Оповещение".to_string(),
                ),
                (
                    "method/common/Target/Цель".to_string(),
                    "method/common/Caller/Прямой".to_string(),
                ),
            ]
        );
        assert_eq!(digest.len(), 3);
    }

    #[test]
    fn source_root_scoped_method_digest_keeps_only_internal_pairs() {
        // Given: direct method calls within two distinct source roots and across their boundary.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db");
        let root_a = dir.path().join("root-a");
        let root_b = dir.path().join("root-b");
        let path_a = root_a.join("Caller.bsl");
        let path_b = root_b.join("Caller.bsl");
        let mut a_caller = method_node("method/a/Caller", "Caller");
        a_caller.file = Some(path_a.to_string_lossy().into_owned());
        let mut a_target = method_node("method/a/Target", "Target");
        a_target.file = Some(root_a.join("Target.bsl").to_string_lossy().into_owned());
        let mut b_caller = method_node("method/b/Caller", "Caller");
        b_caller.file = Some(path_b.to_string_lossy().into_owned());
        let mut b_target = method_node("method/b/Target", "Target");
        b_target.file = Some(root_b.join("Target.bsl").to_string_lossy().into_owned());

        let mut writer = GraphDbWriter::create(&path).unwrap();
        writer.write_nodes(&[a_caller, a_target, b_caller, b_target]).unwrap();
        writer
            .write_edges(&[
                edge("method/a/Caller", "method/a/Target"),
                edge("method/a/Caller", "method/b/Target"),
                edge("method/b/Caller", "method/a/Target"),
                edge("method/b/Caller", "method/b/Target"),
            ])
            .unwrap();
        writer
            .finalize(&GraphMeta {
                revision: 1,
                fingerprint: GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            })
            .unwrap();

        // When: source-root membership is derived from the anchor root's two module files.
        let digest = read_source_root_scoped_sqlite_method_call_digest(
            &path,
            [
                bsl_search::FileKey::new("", path_a.to_string_lossy().into_owned()),
                bsl_search::FileKey::new(
                    "",
                    root_a.join("Target.bsl").to_string_lossy().into_owned(),
                ),
                bsl_search::FileKey::new(
                    "",
                    root_a.join("Target.bsl").to_string_lossy().into_owned(),
                ),
            ],
        )
        .unwrap();

        // Then: only the pair with both method endpoints in the anchor root remains.
        assert_eq!(
            digest.rows(),
            &[("method/a/Target".to_string(), "method/a/Caller".to_string())]
        );
        assert_eq!(digest.len(), 1);
    }

    #[test]
    #[ignore = "requires BSL_GRAPH_DB and BSL_SOURCE_ROOT"]
    fn source_root_scoped_method_digest_from_environment() {
        let graph_db = std::env::var_os("BSL_GRAPH_DB").expect("BSL_GRAPH_DB is required");
        let source_root = std::env::var_os("BSL_SOURCE_ROOT").expect("BSL_SOURCE_ROOT is required");
        let graph_db = PathBuf::from(graph_db);
        let source_root = PathBuf::from(source_root);
        let project = crate::graph::ProjectSnapshot::load(&source_root);
        let files = enumerate_bsl_files(&project);

        // Given: the persisted graph and the exact BSL files in the anchor's source root.
        let digest = read_source_root_scoped_sqlite_method_call_digest(
            &graph_db,
            files.iter().filter_map(|(_, path)| {
                project.search_roots.as_ref().and_then(|roots| roots.key_of_path(path))
            }),
        )
        .unwrap();

        // When: durable target/caller rows are hashed using the parity-oracle byte contract.
        let mut hasher = blake3::Hasher::new();
        for (index, (target, caller)) in digest.rows().iter().enumerate() {
            if index > 0 {
                hasher.update(b"\n");
            }
            hasher.update(target.as_bytes());
            hasher.update(b"\t");
            hasher.update(caller.as_bytes());
        }

        // Then: the report is machine-readable and can be captured with --nocapture.
        println!(
            "{}",
            serde_json::json!({
                "source_root": source_root,
                "source_root_bsl_files": files.len(),
                "row_count": digest.len(),
                "digest": hasher.finalize().to_hex().to_string(),
            })
        );
    }
}
