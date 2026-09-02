use crate::document::Document;
use crate::error::SearchError;
use crate::workspace_roots::{FileKey, CONFIGURATION_ROOT_ID};
use code_chunk::Chunk;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub struct Store {
    conn: Connection,
    path: PathBuf,
    /// Monotonic sequence stamped on every `context_dirty` mark. A graph build captures this
    /// value at build start; its post-publish refresh then consumes ONLY marks whose `seq` is
    /// at or below that captured bound, so a drift landing after the build started is never
    /// cleared against the pre-drift graph. Shared as an `Arc` so the graph layer reads the
    /// same value the store maintains.
    ///
    /// Seqs are allocated by the DATABASE (the `mark_seq` row of `meta`, bumped in the same
    /// transaction as the mark itself), so any number of stores — reopened, standalone, or
    /// belonging to two daemon generations racing over one workspace — draw from one
    /// sequence and can never duplicate or lower a stamp. This atomic is a local mirror of
    /// the highest seq THIS store has observed: it is what the graph reads at build start to
    /// bound the marks its publish may consume. It only ever rises, and it lags the
    /// database whenever another writer allocates — the safe direction, since a bound below
    /// a mark leaves that mark pending for a later publish instead of clearing it against a
    /// graph that predates it.
    mark_seq: Arc<AtomicI64>,
}

pub(crate) struct CollectionReplaceOutcome {
    pub(crate) committed_fingerprint: String,
    pub(crate) written: bool,
}

/// One chunk's `(id, symbol_name, kind, graph_context)` as read for a context re-render.
pub type ChunkContextRow = (i64, String, String, Option<String>);

/// One already-rendered context refresh write. The engine prepares these without a lease;
/// the store commits at most one bounded slice per fenced transaction.
pub(crate) enum ContextRefreshMutation {
    Mark { key: FileKey, seq: i64 },
    Update { chunk_id: i64, graph_context: Option<String> },
    Clear { key: FileKey, seq_bound: i64 },
}

pub(crate) struct WorkspaceDriftStoreOutcome {
    pub(crate) removed_chunk_ids: Vec<i64>,
    pub(crate) context_mark_seq: Option<i64>,
}

/// The embeddings the vector index is built from (`(chunk_id, vector)` rows) paired with the
/// `embedding_generation` they were read at, as one consistent snapshot.
pub type EmbeddingsSnapshot = (i64, Vec<(i64, Vec<f32>)>);
type OverlayEmbeddingPublication<'a> = (&'a str, usize, &'a HashMap<String, Vec<f32>>);

/// One file prepared off-lock for an atomic workspace-root transition.
pub(crate) struct WorkspaceTransitionFile {
    pub(crate) key: FileKey,
    pub(crate) hash: Vec<u8>,
    pub(crate) chunks: Vec<Chunk>,
    pub(crate) graph_contexts: Vec<Option<String>>,
}

/// Persistent mutation set for one workspace-root transition.
pub(crate) struct WorkspaceStoreTransition<'a> {
    /// Entire keyspaces whose binding changed. Cleaning by id also reaches negative state and
    /// persisted overlay rows that are deliberately absent from the positive carrier snapshot.
    pub(crate) changed_root_ids: &'a HashSet<String>,
    pub(crate) cleanup: &'a HashSet<FileKey>,
    pub(crate) tombstones: &'a HashSet<FileKey>,
    pub(crate) upserts: &'a [WorkspaceTransitionFile],
}

#[cfg(test)]
thread_local! {
    /// Fail after the transition has mutated its transaction but before the vector candidate is
    /// built, proving that dropping the transaction restores every persistent carrier.
    pub(crate) static FORCE_WORKSPACE_TRANSITION_VECTOR_ERROR: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };

    /// How many further opens must lose the bootstrap race. Each consumed attempt decrements it,
    /// so a test states the number of retries it wants rather than racing a real peer for them.
    pub(crate) static FORCE_BOOTSTRAP_RETRIES: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

/// Bumped whenever the embedding text composed by
/// `document::semantic_text_for_indexed_document` changes shape. Stored in the SQLite
/// `user_version` pragma; on mismatch the store clears file hashes so the next index
/// re-embeds everything, rather than mixing old- and new-format vectors in one space
/// (file-hash gating would otherwise keep stale-format embeddings indefinitely).
pub(crate) const EMBED_TEXT_VERSION: i64 = 1;

/// The structural version of the SQLite schema, recorded in the `meta` table — the
/// search-index counterpart to the call graph's `graph_db::SCHEMA_VERSION`. Bump this
/// whenever a table's shape changes in a way the additive `ALTER TABLE` migrations in
/// [`Store::init_schema`] cannot reconcile; on mismatch the derived cache is wiped and
/// rebuilt. Distinct from [`EMBED_TEXT_VERSION`], which only forces a soft re-embed and
/// leaves the schema intact. A pre-versioning database (no `meta` row) is treated as
/// already current — the additive migrations keep it compatible — so upgrading does not
/// trigger a needless full re-index.
const SCHEMA_VERSION: i64 = 2;
const SQLITE_IOERR_FSTAT: i32 = 1802;

/// How long an unfenced open lets SQLite wait out a contended write before giving up.
pub(crate) const DEFAULT_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The same budget for an open that runs under an ownership fence: long enough to ride out the
/// ordinary WAL handover, short enough that one admission is not a stall. The total wait is
/// unchanged — the caller's deadline spans many admissions, and it waits between them with the
/// fence released.
pub(crate) const FENCED_OPEN_BUSY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(100);

pub(crate) fn sqlite_bootstrap_retryable(error: &SearchError) -> bool {
    matches!(
        error,
        SearchError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                // A peer creating the WAL files can briefly invalidate SQLite's file stat.
                || code.extended_code == SQLITE_IOERR_FSTAT
    )
}

/// The tables whose row identity is the pair `(root_id, path)` rather than the
/// path alone, and the body each is created with.
///
/// One definition serves both the create path and the migration below, so the
/// shape a fresh store is born with and the shape an upgraded one is rebuilt
/// into cannot drift apart.
struct RootKeyedTable {
    name: &'static str,
    body: &'static str,
    suffix: &'static str,
}

const ROOT_KEYED_TABLES: &[RootKeyedTable] = &[
    RootKeyedTable {
        name: "files",
        body: "
            id         INTEGER PRIMARY KEY,
            root_id    TEXT    NOT NULL DEFAULT '',
            path       TEXT    NOT NULL,
            hash       BLOB    NOT NULL,
            indexed_at INTEGER NOT NULL,
            collection TEXT    NOT NULL DEFAULT 'code',
            UNIQUE (root_id, path)
        ",
        suffix: "",
    },
    RootKeyedTable {
        name: "baseline_manifest_files",
        body: "
            root_id          TEXT    NOT NULL DEFAULT '',
            collection       TEXT    NOT NULL DEFAULT 'code',
            path             TEXT    NOT NULL,
            file_fingerprint TEXT    NOT NULL,
            PRIMARY KEY (collection, root_id, path)
        ",
        suffix: "",
    },
    RootKeyedTable {
        name: "overlay_tombstones",
        body: "
            root_id    TEXT    NOT NULL DEFAULT '',
            path       TEXT    NOT NULL,
            collection TEXT    NOT NULL DEFAULT 'code',
            deleted_at TEXT    NOT NULL,
            UNIQUE (root_id, path)
        ",
        suffix: "",
    },
    RootKeyedTable {
        name: "overlay_files",
        body: "
            id         INTEGER PRIMARY KEY,
            root_id    TEXT    NOT NULL DEFAULT '',
            path       TEXT    NOT NULL,
            hash       BLOB    NOT NULL,
            indexed_at INTEGER NOT NULL,
            collection TEXT    NOT NULL DEFAULT 'code',
            UNIQUE (root_id, path)
        ",
        suffix: "",
    },
    RootKeyedTable {
        name: "overlay_fingerprint_cache",
        body: "
            root_id              TEXT    NOT NULL DEFAULT '',
            path                 TEXT    NOT NULL,
            collection           TEXT    NOT NULL DEFAULT 'code',
            file_size            INTEGER NOT NULL,
            file_mtime_secs      INTEGER NOT NULL,
            file_mtime_nanos     INTEGER NOT NULL,
            content_fingerprint  TEXT    NOT NULL,
            manifest_snapshot_id TEXT    NOT NULL,
            canonical            TEXT    NOT NULL DEFAULT '',
            PRIMARY KEY (root_id, path)
        ",
        suffix: "",
    },
    RootKeyedTable {
        name: "context_dirty",
        body: "
            root_id    TEXT    NOT NULL DEFAULT '',
            path       TEXT    NOT NULL,
            collection TEXT    NOT NULL DEFAULT 'code',
            marked_at  INTEGER NOT NULL,
            seq        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (root_id, path, collection)
        ",
        suffix: " WITHOUT ROWID",
    },
];

impl RootKeyedTable {
    fn create_as(&self, name: &str) -> String {
        format!("CREATE TABLE IF NOT EXISTS {name} ({}){};", self.body, self.suffix)
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, SearchError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match Self::open_once(path) {
                Err(error)
                    if sqlite_bootstrap_retryable(&error)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    fn open_once(path: &Path) -> Result<Self, SearchError> {
        let store = Self::prepare_open(path)?;
        let mut checkpoint = || ControlFlow::Continue(());
        match store.finish_open_checkpointed(&mut checkpoint)? {
            ControlFlow::Continue(()) => Ok(store),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    /// Open the connection and apply connection-local pragmas without changing schema rows.
    pub(crate) fn prepare_open(path: &Path) -> Result<Self, SearchError> {
        Self::prepare_open_with_busy_timeout(path, DEFAULT_BUSY_TIMEOUT)
    }

    /// The same open with an explicit contention budget.
    ///
    /// A caller that opens under an ownership fence must pass a SHORT one. The budget set here
    /// governs the whole connection, and [`Self::finish_open_checkpointed`] goes on to take a
    /// writer reservation in [`Self::migrate_structural_schema`] — a wait SQLite serves inside
    /// that call. Under the fence such a wait is an interprocess lock and the lease's lifecycle
    /// mutex held for its whole length, which is how a shutdown ends up queued behind a peer's
    /// bootstrap. A fenced caller keeps its own deadline across attempts and waits between them
    /// with the fence released, so the budget belongs to the retry and not to one admission.
    pub(crate) fn prepare_open_with_busy_timeout(
        path: &Path,
        busy_timeout: std::time::Duration,
    ) -> Result<Self, SearchError> {
        #[cfg(test)]
        if FORCE_BOOTSTRAP_RETRIES.with(|left| {
            let remaining = left.get();
            left.set(remaining.saturating_sub(1));
            remaining > 0
        }) {
            return Err(SearchError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(SQLITE_IOERR_FSTAT),
                Some("forced bootstrap race".to_owned()),
            )));
        }
        let conn = Connection::open(path)?;
        // Set this before `journal_mode=WAL`: on a brand-new shared cache, changing journal
        // mode is itself the first contended write and must wait for the peer bootstrap.
        conn.busy_timeout(busy_timeout)?;
        let store = Self { conn, path: path.to_path_buf(), mark_seq: Arc::new(AtomicI64::new(0)) };
        store.apply_pragmas()?;
        Ok(store)
    }

    /// Apply every schema/open mutation as atomic groups under a cooperative fence.
    /// Put the connection back on the operational contention budget.
    ///
    /// The short budget a fenced open runs under belongs to that open alone: it exists so one
    /// admission cannot sit under the ownership fence waiting for a peer's bootstrap. The store
    /// that open produces then lives for the whole daemon and shares its database with the
    /// background embedding pass, which holds the WAL writer for far longer than the admission
    /// budget — a live write that gave up after it would return `SQLITE_BUSY` where it used to
    /// wait and succeed.
    pub(crate) fn restore_operational_busy_timeout(&self) -> Result<(), SearchError> {
        self.conn.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
        Ok(())
    }

    pub(crate) fn finish_open_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if self.migrate_root_keyed_tables_checkpointed(checkpoint)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        self.migrate_structural_schema()?;
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        if self.migrate_embed_text_version_checkpointed(checkpoint)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        self.seed_mark_seq()?;
        Ok(ControlFlow::Continue(()))
    }

    /// The database file this store was opened from — the anchor for the sibling persisted
    /// vector-index files (see [`crate::vector_persist`]).
    pub fn db_path(&self) -> &Path {
        &self.path
    }

    /// Connection-level pragmas. Set outside any transaction — `journal_mode` is a no-op
    /// inside one — so the WAL mode that makes [`Self::migrate_structural_schema`]
    /// crash-atomic is actually in force before that transaction runs.
    ///
    /// `busy_timeout` matters once two connections write the same database: the
    /// background embedding pass opens its own connection (WAL: many readers, one
    /// writer) while the overlay watcher keeps writing through the live engine. Without
    /// a timeout a writer that finds the WAL lock held returns `SQLITE_BUSY`
    /// immediately; with it SQLite retries internally for the configured window.
    /// Open an EXISTING store without migrating anything: pragmas only, schema version
    /// validated, mismatch is an error. For a standalone pass that may run while another
    /// daemon owns the workspace — the migrating [`Self::open`] could wipe and recreate the
    /// owner's tables on a version mismatch, and a pass has no business doing either.
    /// `seed_mark_seq` still runs: it only raises the in-memory counter floor.
    pub fn open_existing(path: &Path) -> Result<Self, SearchError> {
        // No CREATE flag: the default open would materialize an empty file for a missing
        // path — a side effect the "existing" contract (and the ownership discipline of the
        // standalone pass) forbids.
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        let store = Self { conn, path: path.to_path_buf(), mark_seq: Arc::new(AtomicI64::new(0)) };
        store.apply_pragmas()?;
        let stored: Option<String> = store
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| row.get(0))
            .optional()?;
        match stored.and_then(|value| value.parse::<i64>().ok()) {
            Some(version) if version == SCHEMA_VERSION => {}
            other => {
                return Err(SearchError::Index(format!(
                    "store schema mismatch: found {other:?}, need {SCHEMA_VERSION}; \
                     a migrating open must do this"
                )));
            }
        }
        store.seed_mark_seq()?;
        Ok(store)
    }

    fn apply_pragmas(&self) -> Result<(), SearchError> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(())
    }

    /// Give the path-keyed tables their `root_id` column without losing a row.
    ///
    /// The identity of a row moved from `path` to `(root_id, path)`, and SQLite
    /// cannot alter a `UNIQUE`/`PRIMARY KEY` clause in place: each table has to
    /// be rebuilt. The ordinary route for a shape change — bump the version and
    /// let the store be wiped — is exactly what must not happen here, because a
    /// wipe costs a full re-index and re-embedding of a corpus that has not
    /// changed at all. Every existing row is the configuration's, which is what
    /// the reserved empty id means, so the default value alone carries them over.
    ///
    /// The entry condition is the shape of the table, not the recorded version:
    /// a store written before versioning existed reports no version at all and
    /// would sail past a version check into the new code with its old
    /// constraints intact.
    ///
    /// One consequence is deliberate. A daemon of the previous release keeps a
    /// connection to this file open while it drains, and the lease does not gate
    /// the lexical side of the index, so its upserts start failing against the
    /// new constraint the moment this runs. That process keeps serving the index
    /// it already holds and exits with its last session; the store itself stays
    /// consistent and belongs to the newer daemon. Splitting the file per schema
    /// would remove the window at the price of copying the whole store on every
    /// upgrade.
    fn migrate_root_keyed_tables_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        let pending: Vec<&RootKeyedTable> = ROOT_KEYED_TABLES
            .iter()
            .filter(|table| Self::table_awaits_root_id(&self.conn, table.name))
            .collect();
        if pending.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        // Dropping `files` with foreign keys enforced would cascade its chunks
        // away — the embeddings this migration exists to preserve. The pragma is
        // a no-op inside a transaction, so the window is opened here and closed
        // whatever happens, including on a failed rebuild.
        self.conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let rebuilt = self.rebuild_root_keyed_tables(&pending, checkpoint);
        let restored = self.conn.execute_batch("PRAGMA foreign_keys = ON;");
        let rebuilt = rebuilt?;
        restored?;
        Ok(rebuilt)
    }

    /// Whether the table exists and still lacks `root_id`. A missing table is
    /// not pending: it will be created in the current shape.
    fn table_awaits_root_id(conn: &Connection, table: &str) -> bool {
        let columns = Self::column_names(conn, table);
        !columns.is_empty() && !columns.iter().any(|column| column == "root_id")
    }

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info(?1)") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(params![table], |row| row.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok).collect()
    }

    fn rebuild_root_keyed_tables(
        &self,
        pending: &[&RootKeyedTable],
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        let tx = self.conn.unchecked_transaction()?;
        for table in pending {
            let staging = format!("{}_root_id_migration", table.name);
            tx.execute_batch(&table.create_as(&staging))?;
            // The columns to carry over are read off both tables rather than
            // written out here: a store old enough to predate one of the
            // additive column adds would otherwise fail on a column that its
            // copy of the table never had.
            let carried: Vec<String> = Self::column_names(&tx, table.name)
                .into_iter()
                .filter(|column| Self::column_names(&tx, &staging).contains(column))
                .collect();
            let carried = carried.join(", ");
            let mut offset = 0usize;
            loop {
                let copied = tx.execute(
                    &format!(
                        "INSERT INTO {staging} ({carried}) SELECT {carried} FROM {} LIMIT ?1 OFFSET ?2",
                        table.name
                    ),
                    params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64, offset as i64],
                )?;
                offset += copied;
                if copied == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                    return Ok(ControlFlow::Break(()));
                }
                if copied < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                    break;
                }
            }
            tx.execute_batch(&format!(
                "DROP TABLE {};
                 ALTER TABLE {staging} RENAME TO {};",
                table.name, table.name
            ))?;
        }

        // The rebuild leaves `chunks` pointing at a table that was dropped and
        // recreated under the same name; this is where a mistake in that dance
        // shows up, while the transaction can still be rolled back.
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| row.get(0))?;
        if violations != 0 {
            return Err(SearchError::Index(format!(
                "root_id migration left {violations} dangling foreign key reference(s)"
            )));
        }

        // Stamped forward here, inside the same transaction, so the structural
        // migration that runs next sees a current store and leaves it alone
        // instead of wiping the rows just carried over. A store with no `meta`
        // table at all is stamped by that migration instead.
        if !Self::column_names(&tx, "meta").is_empty() {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    /// Reconcile the structural schema in a single transaction: wipe a stale cache,
    /// (re)create the current tables, and stamp the version atomically. Under WAL a
    /// crash mid-reconcile rolls back to the prior consistent state, so the next open
    /// never sees a half-wiped database it would mistake for a pre-versioning one (whose
    /// data must be kept). Distinct from [`Self::migrate_embed_text_version`], a soft
    /// re-embed that leaves the schema intact.
    fn migrate_structural_schema(&self) -> Result<(), SearchError> {
        // Two processes may bootstrap the same derived cache. Taking the writer reservation
        // before reading the version prevents both from observing the same pre-migration state
        // and then racing a deferred read transaction's upgrade to writer.
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(stored) = Self::stored_schema_version(&tx)? {
            if stored != SCHEMA_VERSION {
                tracing::info!(
                    from = stored,
                    to = SCHEMA_VERSION,
                    "search index schema changed; wiping derived cache to rebuild"
                );
                Self::wipe_all_tables(&tx)?;
            }
        }
        Self::create_schema(&tx)?;
        // Additive columns are added in place, NOT via a SCHEMA_VERSION bump: a version
        // mismatch wipes every derived table, which is exactly what an additive change
        // exists to avoid. `create_schema` only creates missing tables, so an existing
        // database needs the column grafted onto its live table, data intact.
        Self::ensure_column(
            &tx,
            "overlay_fingerprint_cache",
            "canonical",
            "TEXT NOT NULL DEFAULT ''",
        )?;

        Self::ensure_embedding_generation(&tx, &self.path)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Add `column` to `table` when it is missing — the in-place half of an additive schema
    /// change. A no-op on a fresh database, whose `CREATE TABLE` already carries the column.
    fn ensure_column(
        tx: &Connection,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), SearchError> {
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|name| name.ok())
            .any(|name| name == column);
        if !exists {
            tx.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"), [])?;
        }
        Ok(())
    }

    /// Guarantee the `embedding_generation` counter exists, and invalidate stale vector artifacts
    /// whenever it has to be (re)created. The counter is absent in exactly three cases — a fresh
    /// database, one just wiped by [`Self::wipe_all_tables`] above, or a corrupt one that lost the
    /// row — and in all of them a persisted index/sidecar cannot be trusted against the
    /// freshly-reset counter (the wipe drops the row via `DROP TABLE`, firing no trigger, so a
    /// surviving generation-0 sidecar would otherwise false-accept). So when the row is missing we
    /// remove the artifacts FIRST, fallibly: a sidecar that cannot be deleted aborts the migration
    /// before `tx.commit()` (the transaction rolls back) rather than leave an emptied/reset database
    /// next to a loadable sidecar. Seeding only after a successful removal keeps the counter and the
    /// on-disk artifacts consistent. A normal open (row present) skips all of this.
    fn ensure_embedding_generation(tx: &Connection, db_path: &Path) -> Result<(), SearchError> {
        if Self::read_embedding_generation(tx)? != Self::MISSING_GENERATION {
            return Ok(());
        }
        crate::vector_persist::remove_artifacts(db_path)?;
        tx.execute("INSERT INTO meta (key, value) VALUES ('embedding_generation', '0')", [])?;
        Ok(())
    }

    /// The structural schema version recorded in `meta`, or `None` for a fresh or
    /// pre-versioning database (no `meta` table, or no `schema_version` row).
    fn stored_schema_version(conn: &Connection) -> Result<Option<i64>, SearchError> {
        let has_meta = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_meta {
            return Ok(None);
        }
        let version = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok());
        Ok(version)
    }

    /// Drop every table so [`Self::create_schema`] can recreate the current structure.
    /// FTS5 virtual tables are dropped first so their shadow tables vanish before the
    /// generic enumeration runs (dropping a shadow table directly is an error). The
    /// `embedding_generation` triggers are dropped before any table: dropping the parent
    /// `files` table runs its FK `ON DELETE CASCADE` onto `chunks` (foreign keys are ON and
    /// the pragma cannot be toggled inside this transaction), which would otherwise fire
    /// `chunks_gen_del` against an already-dropped `meta` table and abort the wipe.
    fn wipe_all_tables(conn: &Connection) -> Result<(), SearchError> {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS chunks_gen_ins;
             DROP TRIGGER IF EXISTS chunks_gen_upd;
             DROP TRIGGER IF EXISTS chunks_gen_del;
             DROP TRIGGER IF EXISTS files_gen_del;
             DROP TABLE IF EXISTS chunks_fts; DROP TABLE IF EXISTS overlay_chunks_fts;",
        )?;
        let names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for name in names {
            conn.execute(&format!("DROP TABLE IF EXISTS \"{name}\""), [])?;
        }
        Ok(())
    }

    /// Force a full re-embed when the embedding-text format has changed since this
    /// database was built (see [`EMBED_TEXT_VERSION`]). A fresh database just records
    /// the current version.
    fn migrate_embed_text_version_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != EMBED_TEXT_VERSION {
            let tx = self.conn.unchecked_transaction()?;
            let mut cleared = 0usize;
            loop {
                let batch = tx.execute(
                    "UPDATE files SET hash = zeroblob(0)
                     WHERE id IN (SELECT id FROM files WHERE length(hash) > 0 LIMIT ?1)",
                    params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64],
                )?;
                cleared += batch;
                if batch == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                    return Ok(ControlFlow::Break(()));
                }
                if batch < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                    break;
                }
            }
            tx.pragma_update(None, "user_version", EMBED_TEXT_VERSION)?;
            if checkpoint().is_break() {
                return Ok(ControlFlow::Break(()));
            }
            tx.commit()?;
            if cleared > 0 {
                tracing::info!(
                    cleared,
                    from = version,
                    to = EMBED_TEXT_VERSION,
                    "embed-text format changed; cleared file hashes to force re-embed"
                );
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self, SearchError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        let store =
            Self { conn, path: PathBuf::from(":memory:"), mark_seq: Arc::new(AtomicI64::new(0)) };
        store.apply_pragmas()?;
        let mut checkpoint = || ControlFlow::Continue(());
        assert!(store.finish_open_checkpointed(&mut checkpoint)?.is_continue());
        Ok(store)
    }

    fn create_schema(conn: &Connection) -> Result<(), SearchError> {
        // Created first, and from the shared definitions: `chunks` and
        // `overlay_chunks` reference them, and their shape must be the one the
        // migration rebuilds into.
        for table in ROOT_KEYED_TABLES {
            conn.execute_batch(&table.create_as(table.name))?;
        }

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id          INTEGER PRIMARY KEY,
                file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                kind        TEXT    NOT NULL,
                symbol_name TEXT    NOT NULL,
                is_export   INTEGER NOT NULL DEFAULT 0,
                annotations TEXT,
                line_start  INTEGER NOT NULL,
                line_end    INTEGER NOT NULL,
                text        TEXT    NOT NULL,
                embedding   BLOB,
                graph_context TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_file
                ON chunks(file_id);

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                symbol_name,
                text,
                tokenize='unicode61'
            );
            ",
        )?;

        let _ = conn
            .execute("ALTER TABLE files ADD COLUMN collection TEXT NOT NULL DEFAULT 'code'", []);
        // Idempotent column add for databases created before graph-enriched embeddings;
        // the error when it already exists is intentionally ignored.
        let _ = conn.execute("ALTER TABLE chunks ADD COLUMN graph_context TEXT", []);

        conn.execute_batch(
            "
            -- Baseline manifest metadata for workspace code.
            -- Stores the selected snapshot identity and manifest snapshot.
            CREATE TABLE IF NOT EXISTS baseline_manifest (
                id              INTEGER PRIMARY KEY CHECK (id = 1),
                snapshot_id     TEXT    NOT NULL,
                fingerprint     TEXT,
                manifest_files  INTEGER NOT NULL DEFAULT 0,
                fetched_at      TEXT    NOT NULL
            );

            -- Tombstones for deleted baseline files.
            -- When a baseline file is deleted locally, its path is recorded here
            -- so the merge layer can hide the baseline hit.
            -- Overlay files: files that are locally modified or new relative to
            -- the baseline manifest. These are separate from the main `files`
            -- table so baseline rows never appear in local storage.
            -- Overlay chunks: lexical chunks belonging to overlay files.
            CREATE TABLE IF NOT EXISTS overlay_chunks (
                id          INTEGER PRIMARY KEY,
                file_id     INTEGER NOT NULL REFERENCES overlay_files(id) ON DELETE CASCADE,
                kind        TEXT    NOT NULL,
                symbol_name TEXT    NOT NULL,
                is_export   INTEGER NOT NULL DEFAULT 0,
                annotations TEXT,
                line_start  INTEGER NOT NULL,
                line_end    INTEGER NOT NULL,
                text        TEXT    NOT NULL,
                embedding   BLOB
            );

            CREATE INDEX IF NOT EXISTS idx_overlay_chunks_file
                ON overlay_chunks(file_id);

            -- FTS index for overlay chunks.
            CREATE VIRTUAL TABLE IF NOT EXISTS overlay_chunks_fts USING fts5(
                symbol_name,
                text,
                tokenize='unicode61'
            );

            -- Persisted overlay fingerprint cache: avoids re-reading and
            -- re-hashing unchanged files on MCP server restart.
            -- Persisted overlay embedding cache: avoids re-embedding
            -- unchanged overlay chunks on MCP server restart. Keyed by the
            -- embedding key (hash of the embedded text), not raw chunk text.
            CREATE TABLE IF NOT EXISTS overlay_embedding_cache (
                embedding_key TEXT NOT NULL PRIMARY KEY,
                model_id      TEXT NOT NULL,
                dimension     INTEGER NOT NULL,
                embedding     BLOB NOT NULL
            );

            -- Context-dirty registry: workspace files whose stored graph_context (and
            -- hence embedding) is stale because a metadata `.xml` they own or read
            -- changed. Kept as a side table, NOT a `chunks` column, so marking never
            -- fires the embedding-generation triggers and invalidates the vector
            -- sidecar. A later reindex/embed pass re-renders the context and clears the
            -- entry; a lost row (a schema wipe) is harmless — the next drift re-marks it.
            -- `seq` is a monotonic mark stamp drawn from the `mark_seq` row of `meta` (see
            -- `Store::next_mark_seq`), so every process writing this file shares one
            -- sequence: a graph build's
            -- post-publish refresh consumes only rows at or below the build's captured
            -- start-seq, so a drift that lands after the build started is never cleared
            -- against the pre-drift graph. Re-marking a row bumps its `seq`, so a clear
            -- bounded by an older start-seq skips a row a fresher drift just re-stamped.
            ",
        )?;

        Self::migrate_overlay_embedding_cache_key(conn)?;

        Self::create_embedding_generation_triggers(conn)?;

        Ok(())
    }

    /// Drop and recreate `overlay_embedding_cache` when it still has the legacy `content_hash`
    /// column. The cache used to be keyed by the hash of the raw chunk text; it is now keyed by the
    /// embedding key (the hash of the actual embedded text, which folds in module / symbol / kind /
    /// graph context). The two keys never collide, so old rows could never be reused under the new
    /// keying anyway. Dropping them is safe: the cache is rebuilt on miss by re-embedding, so this
    /// only costs the next warmup a re-embed of the affected chunks.
    fn migrate_overlay_embedding_cache_key(conn: &Connection) -> Result<(), SearchError> {
        let has_legacy_column = conn
            .prepare("PRAGMA table_info(overlay_embedding_cache)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|column| column == "content_hash");

        if has_legacy_column {
            conn.execute_batch(
                "
                DROP TABLE overlay_embedding_cache;
                CREATE TABLE overlay_embedding_cache (
                    embedding_key TEXT NOT NULL PRIMARY KEY,
                    model_id      TEXT NOT NULL,
                    dimension     INTEGER NOT NULL,
                    embedding     BLOB NOT NULL
                );
                ",
            )?;
        }

        Ok(())
    }

    /// A monotonic counter bumped by triggers on every write that can change the set of
    /// `(chunks.id, chunks.embedding)` rows the vector index is built from. The persisted index's
    /// sidecar records the generation it was built at, so [`crate::vector_persist::try_load`] can
    /// confirm "nothing changed since" with a single-row read instead of scanning every embedding
    /// BLOB (see `embedding_generation` / `load_all_embeddings_with_generation`).
    ///
    /// Coverage (auditable contract): `insert_chunk` and all `reindex_*` inserts fire
    /// `chunks_gen_ins`; the reindex delete-phases and `delete_chunks_for_file` fire `chunks_gen_del`;
    /// `set_chunk_embedding` fires `chunks_gen_upd`; `remove_file` / `clear_collection` delete `files`
    /// rows (and cascade to `chunks`) and fire `files_gen_del`. The `files_gen_del` trigger makes the
    /// counter advance on a file removal regardless of the `recursive_triggers` pragma, so we never
    /// depend on whether an FK cascade fires the `chunks` delete trigger. `upsert_file`,
    /// `clear_file_hashes`, and `migrate_embed_text_version` touch only `files` metadata, not the
    /// indexed embedding set, and intentionally do not bump. Over-bumping is safe (only forces a
    /// rebuild); under-bumping would serve a stale index, so the triggers err toward bumping. A
    /// destructive `wipe_all_tables` resets the counter (DROP TABLE fires no trigger); the counter
    /// row itself is (re)seeded by [`Self::ensure_embedding_generation`], which deletes any stale
    /// persisted artifacts whenever it has to recreate the row so the reset can never match a
    /// pre-wipe sidecar. These triggers reference the `meta` row but do not create it.
    fn create_embedding_generation_triggers(conn: &Connection) -> Result<(), SearchError> {
        conn.execute_batch(
            "
            CREATE TRIGGER IF NOT EXISTS chunks_gen_ins AFTER INSERT ON chunks BEGIN
                UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
                WHERE key = 'embedding_generation';
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_gen_upd AFTER UPDATE OF embedding ON chunks BEGIN
                UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
                WHERE key = 'embedding_generation';
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_gen_del AFTER DELETE ON chunks BEGIN
                UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
                WHERE key = 'embedding_generation';
            END;
            CREATE TRIGGER IF NOT EXISTS files_gen_del AFTER DELETE ON files BEGIN
                UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
                WHERE key = 'embedding_generation';
            END;
            ",
        )?;
        Ok(())
    }

    pub fn file_hash(&self, root_id: &str, path: &str) -> Result<Option<Vec<u8>>, SearchError> {
        let hash = self
            .conn
            .query_row(
                "SELECT hash FROM files WHERE root_id = ?1 AND path = ?2",
                params![root_id, path],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(hash)
    }

    pub fn upsert_file(
        &self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        collection: &str,
    ) -> Result<i64, SearchError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT INTO files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4, collection = ?5",
            params![root_id, path, hash, now, collection],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Remove one file (its `files` row, cascaded `chunks`, and the matching FTS rows)
    /// from `collection`, atomically. The FTS delete and the `files` delete run in one
    /// transaction so a failure between them cannot leave an orphaned FTS row or a
    /// `files` row without its FTS. The delete is scoped to `collection` so a same-named
    /// path in another collection is never touched, and to `root_id` so the same
    /// relative path under another source root — which a `cfe` extension repeats
    /// wholesale — keeps its own row.
    pub fn remove_file(
        &self,
        root_id: &str,
        path: &str,
        collection: &str,
    ) -> Result<(), SearchError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (
                 SELECT c.id FROM chunks c
                 JOIN files f ON f.id = c.file_id
                 WHERE f.root_id = ?1 AND f.path = ?2 AND f.collection = ?3
             )",
            params![root_id, path, collection],
        )?;
        tx.execute(
            "DELETE FROM files WHERE root_id = ?1 AND path = ?2 AND collection = ?3",
            params![root_id, path, collection],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The row ids of every chunk owned by `path` in `collection`. Collected before a
    /// [`Self::remove_file`] so the caller can evict exactly those vectors from the live
    /// index incrementally, instead of rebuilding it from scratch.
    pub fn chunk_ids_for_file(
        &self,
        collection: &str,
        root_id: &str,
        path: &str,
    ) -> Result<Vec<i64>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE f.root_id = ?1 AND f.path = ?2 AND f.collection = ?3",
        )?;
        let ids = stmt
            .query_map(params![root_id, path, collection], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    pub fn delete_chunks_for_file(&self, file_id: i64) -> Result<(), SearchError> {
        self.conn.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// A handle to the observed mark-seq high-water (see [`Self::mark_seq`]). The graph
    /// layer captures its value at build start to bound the marks its publish may consume.
    pub fn mark_seq_handle(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.mark_seq)
    }

    /// Reserve the next mark sequence from the persisted counter, on `conn` (the caller's
    /// transaction, so the stamp and the row it lands on commit together).
    ///
    /// The counter lives in the database rather than in this process, so the sequence stays
    /// strictly increasing across every store that ever opens the file — including two
    /// daemon generations overlapping on one workspace. SQLite serializes the bumping write
    /// against every other connection, so two allocations can neither collide nor rewind;
    /// clearing rows never touches the counter, so a value a build captured earlier stays
    /// below every mark stamped afterward.
    fn next_mark_seq(conn: &Connection) -> Result<i64, SearchError> {
        let seq = conn.query_row(
            "INSERT INTO meta (key, value) VALUES ('mark_seq', '1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
             RETURNING CAST(value AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        Ok(seq)
    }

    /// Raise the persisted counter above every surviving mark, and mirror it into the
    /// in-memory high-water.
    ///
    /// The counter row is absent on a pre-counter database and dropped by a schema wipe,
    /// while `context_dirty` rows can survive both; seeding from the larger of the two keeps
    /// the next allocation above every stamp still on disk instead of re-issuing seqs a
    /// surviving row already holds.
    fn seed_mark_seq(&self) -> Result<(), SearchError> {
        let rows_max: i64 =
            self.conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM context_dirty", [], |row| {
                row.get(0)
            })?;
        let seeded: i64 = self.conn.query_row(
            "INSERT INTO meta (key, value) VALUES ('mark_seq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = CAST(MAX(CAST(value AS INTEGER), ?1) AS TEXT)
             RETURNING CAST(value AS INTEGER)",
            params![rows_max],
            |row| row.get(0),
        )?;
        self.observe_mark_seq(seeded);
        Ok(())
    }

    /// Raise the local high-water to `seq` (never lower it): concurrent marks through this
    /// same store may commit out of order, and another process's allocations are invisible
    /// here until one of ours returns a seq above them.
    fn observe_mark_seq(&self, seq: i64) {
        self.mark_seq.fetch_max(seq, Ordering::SeqCst);
    }

    pub(crate) fn observe_committed_mark_seq(&self, seq: i64) {
        self.observe_mark_seq(seq);
    }

    /// The highest mark seq the database at `path` has issued to ANYONE.
    ///
    /// A store's own high-water only tracks the stamps it allocated, so it sits below anything
    /// another process wrote. That is safe for a bound but not sufficient: a mark stamped by a
    /// second daemon would stay below every bound this one ever captures, and a mark no bound
    /// covers is a mark no publish ever consumes — the file's context would stay stale for
    /// good. A build captures its bound from HERE instead, which by construction sits above
    /// every mark stamped before the build began and below every one stamped after.
    ///
    /// Read-only and connectionless: it is called once per build, from a process that may not
    /// hold the engine. A database that does not exist yet, or predates the counter, reports
    /// what its surviving rows imply.
    pub fn persisted_mark_seq(path: &Path) -> Result<i64, SearchError> {
        // A database that was never created has issued nothing — the ordinary state of a cold
        // workspace, not a failure worth reporting to a caller that would only log it.
        if !path.exists() {
            return Ok(0);
        }
        // Read-only and NOT URI-interpreted: this takes a filesystem path, and a workspace whose
        // path happens to contain `?` must not have it read as a URI query.
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Only "no such row" reads as zero. A busy database, a schema that is not there yet, or
        // any other failure is REPORTED: swallowing it as zero would silently hand the caller a
        // bound below another writer's marks, and a mark no bound covers is never consumed.
        let counter: i64 = conn
            .query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'mark_seq'", [], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or(0);
        let rows_max: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM context_dirty", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(counter.max(rows_max))
    }

    /// Record that `path`'s stored `graph_context` is stale (a metadata `.xml` it owns
    /// or reads changed), so a later reindex/embed pass re-renders it. A hint only: it
    /// carries no foreign key, and re-marking an already-dirty path is a cheap upsert that
    /// bumps the row's monotonic `seq`.
    pub fn mark_context_dirty(
        &self,
        collection: &str,
        root_id: &str,
        path: &str,
    ) -> Result<(), SearchError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Allocation and row land in ONE transaction: a seq that committed while its row
        // did not would let a build capture a bound above a mark that only becomes visible
        // afterwards, and that mark would then be cleared against a graph predating it.
        let tx = self.conn.unchecked_transaction()?;
        let seq = Self::next_mark_seq(&tx)?;
        tx.execute(
            "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path, collection) DO UPDATE SET marked_at = ?4, seq = ?5",
            params![root_id, path, collection, now, seq],
        )?;
        tx.commit()?;
        self.observe_mark_seq(seq);
        Ok(())
    }

    /// Mark every indexed file in `collection` context-dirty (a configuration-root `.xml`
    /// or the extension topology changed: conservatively assume any module's context
    /// could shift). Bounded by the file count, one upsert per path, all stamped with a
    /// single mark `seq` (the whole batch is one drift event). Returns the number of
    /// files marked and that shared `seq`, so a caller that immediately consumes the
    /// batch can bound its clear to exactly these marks and nothing stamped later.
    pub fn mark_collection_context_dirty(
        &self,
        collection: &str,
    ) -> Result<(usize, i64), SearchError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let tx = self.conn.unchecked_transaction()?;
        let seq = Self::next_mark_seq(&tx)?;
        let count = tx.execute(
            "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
             SELECT root_id, path, collection, ?2, ?3 FROM files WHERE collection = ?1
             ON CONFLICT(root_id, path, collection) DO UPDATE SET marked_at = ?2, seq = ?3",
            params![collection, now, seq],
        )?;
        tx.commit()?;
        self.observe_mark_seq(seq);
        Ok((count, seq))
    }

    /// Mark every indexed file in `collection` context-dirty AT `stamp_seq`, leaving alone any
    /// row that already carries a fresher stamp. Returns the number of rows written.
    ///
    /// This is the topology variant of [`Self::mark_collection_context_dirty`], and the
    /// difference is the whole point. A topology re-render is performed against a build that
    /// reflects the workspace as of `stamp_seq`, so that is the stamp its rows deserve — and a
    /// row stamped ABOVE it belongs to a drift that build does NOT reflect. Stamping the batch
    /// with a fresh (higher) seq would overwrite such a row and then sweep it into the same
    /// bounded clear, re-rendering that file against a graph predating its drift and losing the
    /// mark that would have fixed it later.
    pub fn mark_collection_context_dirty_at(
        &self,
        collection: &str,
        stamp_seq: i64,
    ) -> Result<usize, SearchError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let count = self.conn.execute(
            "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
             SELECT root_id, path, collection, ?2, ?3 FROM files WHERE collection = ?1
             ON CONFLICT(root_id, path, collection) DO UPDATE SET marked_at = ?2, seq = ?3
             WHERE context_dirty.seq <= ?3",
            params![collection, now, stamp_seq],
        )?;
        Ok(count)
    }

    /// The set of paths currently marked context-dirty in `collection`, regardless of
    /// mark `seq`. Used for status/assertions; the consuming refresh uses the bounded
    /// [`Self::context_dirty_paths_bounded`] variant.
    pub fn context_dirty_paths(&self, collection: &str) -> Result<HashSet<FileKey>, SearchError> {
        let mut stmt =
            self.conn.prepare("SELECT root_id, path FROM context_dirty WHERE collection = ?1")?;
        let rows = stmt
            .query_map(params![collection], file_key_row)?
            .collect::<Result<HashSet<FileKey>, _>>()?;
        Ok(rows)
    }

    /// The paths marked context-dirty in `collection` at or below `seq_bound` — the marks a
    /// graph build that captured `seq_bound` at its start is allowed to consume. Marks
    /// stamped after the build started (`seq > seq_bound`) are excluded and left for a later
    /// build's publish.
    pub fn context_dirty_paths_bounded(
        &self,
        collection: &str,
        seq_bound: i64,
    ) -> Result<HashSet<FileKey>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT root_id, path FROM context_dirty WHERE collection = ?1 AND seq <= ?2",
        )?;
        let rows = stmt
            .query_map(params![collection, seq_bound], file_key_row)?
            .collect::<Result<HashSet<FileKey>, _>>()?;
        Ok(rows)
    }

    /// Clear one path's context-dirty mark (a reindex/embed pass re-rendered it).
    pub fn clear_context_dirty(
        &self,
        collection: &str,
        root_id: &str,
        path: &str,
    ) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM context_dirty WHERE collection = ?1 AND root_id = ?2 AND path = ?3",
            params![collection, root_id, path],
        )?;
        Ok(())
    }

    /// Clear one path's context-dirty mark ONLY when it still sits at or below `seq_bound`.
    /// If a fresher drift re-stamped the row after the build captured `seq_bound`, its `seq`
    /// now exceeds the bound and the row survives — the newer mark is not lost to this
    /// build's clear.
    pub fn clear_context_dirty_bounded(
        &self,
        collection: &str,
        root_id: &str,
        path: &str,
        seq_bound: i64,
    ) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM context_dirty
             WHERE collection = ?1 AND root_id = ?2 AND path = ?3 AND seq <= ?4",
            params![collection, root_id, path, seq_bound],
        )?;
        Ok(())
    }

    /// Commit one bounded, already-rendered context-refresh slice atomically. The caller keeps
    /// slices at `WORKSPACE_APPLY_BATCH_ROWS`; the two checkpoints refresh the lease heartbeat
    /// and turn a release before commit into a full rollback of this slice.
    pub(crate) fn apply_context_refresh_batch(
        &self,
        mutations: &[ContextRefreshMutation],
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> ControlFlow<(), Result<(usize, usize, usize), SearchError>> {
        if checkpoint().is_break() {
            return ControlFlow::Break(());
        }
        let tx = match self.conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(error) => return ControlFlow::Continue(Err(error.into())),
        };
        let marked_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let mut marked = 0;
        let mut updated = 0;
        let mut cleared = 0;
        for mutation in mutations {
            let changed = match mutation {
                ContextRefreshMutation::Mark { key, seq } => tx.execute(
                    "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
                     VALUES (?1, ?2, 'code', ?3, ?4)
                     ON CONFLICT(root_id, path, collection) DO UPDATE
                     SET marked_at = ?3, seq = ?4 WHERE context_dirty.seq <= ?4",
                    params![key.root_id, key.path, marked_at, seq],
                ),
                ContextRefreshMutation::Update { chunk_id, graph_context } => tx.execute(
                    "UPDATE chunks SET graph_context = ?2, embedding = NULL WHERE id = ?1",
                    params![chunk_id, graph_context],
                ),
                ContextRefreshMutation::Clear { key, seq_bound } => tx.execute(
                    "DELETE FROM context_dirty
                     WHERE collection = 'code' AND root_id = ?1 AND path = ?2 AND seq <= ?3",
                    params![key.root_id, key.path, seq_bound],
                ),
            };
            let changed = match changed {
                Ok(changed) => changed,
                Err(error) => return ControlFlow::Continue(Err(error.into())),
            };
            match mutation {
                ContextRefreshMutation::Mark { .. } => marked += changed,
                ContextRefreshMutation::Update { .. } => updated += changed,
                ContextRefreshMutation::Clear { .. } => cleared += changed,
            }
        }
        if checkpoint().is_break() {
            return ControlFlow::Break(());
        }
        match tx.commit() {
            Ok(()) => ControlFlow::Continue(Ok((marked, updated, cleared))),
            Err(error) => ControlFlow::Continue(Err(error.into())),
        }
    }

    pub(crate) fn apply_workspace_drift_batch(
        &self,
        removed: &[FileKey],
        context: &[FileKey],
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<(), WorkspaceDriftStoreOutcome>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut removed_chunk_ids = Vec::new();
        let deleted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        for key in removed {
            let mut stmt = tx.prepare(
                "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                 WHERE f.collection = 'code' AND f.root_id = ?1 AND f.path = ?2",
            )?;
            removed_chunk_ids.extend(
                stmt.query_map(params![key.root_id, key.path], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            tx.execute(
                "INSERT INTO overlay_tombstones (root_id, path, collection, deleted_at)
                 VALUES (?1, ?2, 'code', ?3)
                 ON CONFLICT(root_id, path) DO UPDATE SET collection = 'code', deleted_at = ?3",
                params![key.root_id, key.path, deleted_at],
            )?;
            tx.execute(
                "DELETE FROM overlay_fingerprint_cache WHERE root_id = ?1 AND path = ?2",
                params![key.root_id, key.path],
            )?;
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN (
                     SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                     WHERE f.collection = 'code' AND f.root_id = ?1 AND f.path = ?2
                 )",
                params![key.root_id, key.path],
            )?;
            tx.execute(
                "DELETE FROM files WHERE collection = 'code' AND root_id = ?1 AND path = ?2",
                params![key.root_id, key.path],
            )?;
        }
        let context_mark_seq = if context.is_empty() {
            None
        } else {
            let seq = Self::next_mark_seq(&tx)?;
            let marked_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0);
            let mut stmt = tx.prepare(
                "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
                 VALUES (?1, ?2, 'code', ?3, ?4)
                 ON CONFLICT(root_id, path, collection) DO UPDATE SET marked_at = ?3, seq = ?4",
            )?;
            for key in context {
                stmt.execute(params![key.root_id, key.path, marked_at, seq])?;
            }
            Some(seq)
        };
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(WorkspaceDriftStoreOutcome {
            removed_chunk_ids,
            context_mark_seq,
        }))
    }

    pub fn insert_chunk(
        &self,
        file_id: i64,
        chunk: &Chunk,
        embedding: Option<&[f32]>,
    ) -> Result<i64, SearchError> {
        let kind_str = match chunk.kind {
            code_chunk::ChunkKind::ModuleHeader => "header",
            code_chunk::ChunkKind::Procedure => "procedure",
            code_chunk::ChunkKind::Function => "function",
        };
        let annotations =
            if chunk.annotations.is_empty() { None } else { Some(chunk.annotations.join(",")) };
        let embedding_blob: Option<Vec<u8>> =
            embedding.map(|e| e.iter().flat_map(|f| f.to_le_bytes()).collect());

        self.conn.execute(
            "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                 line_start, line_end, text, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                file_id,
                kind_str,
                chunk.name,
                chunk.is_export as i32,
                annotations,
                chunk.line_start,
                chunk.line_end,
                chunk.text,
                embedding_blob,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn reindex_file(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
    ) -> Result<i64, SearchError> {
        self.reindex_file_in_collection(root_id, path, hash, "code", chunks, embeddings, None)
    }

    /// Apply every persistent part of a workspace-root transition in one SQLite transaction.
    ///
    /// The external baseline manifest and value-keyed embedding cache are deliberately not
    /// touched: neither belongs to the mutable workspace keyspace. The caller prepares its cache
    /// and vector candidates before entering the fence, then swaps them only after this commits.
    pub(crate) fn apply_workspace_roots_transition(
        &mut self,
        change: WorkspaceStoreTransition<'_>,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.transaction()?;
        let mut rows = 0usize;
        let mut tick = || {
            rows += 1;
            rows.is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                && checkpoint().is_break()
        };

        for root_id in change.changed_root_ids {
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN (
                     SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                     WHERE f.collection = 'code' AND f.root_id = ?1
                 )",
                params![root_id],
            )?;
            tx.execute(
                "DELETE FROM files WHERE collection = 'code' AND root_id = ?1",
                params![root_id],
            )?;
            tx.execute(
                "DELETE FROM overlay_chunks_fts WHERE rowid IN (
                     SELECT c.id FROM overlay_chunks c
                     JOIN overlay_files f ON f.id = c.file_id
                     WHERE f.collection = 'code' AND f.root_id = ?1
                 )",
                params![root_id],
            )?;
            tx.execute(
                "DELETE FROM overlay_files WHERE collection = 'code' AND root_id = ?1",
                params![root_id],
            )?;
            for table in ["overlay_fingerprint_cache", "context_dirty", "overlay_tombstones"] {
                let sql = format!("DELETE FROM {table} WHERE root_id = ?1");
                tx.execute(&sql, params![root_id])?;
            }
            if tick() {
                return Ok(ControlFlow::Break(()));
            }
        }

        for key in change.cleanup {
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN (
                     SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                     WHERE f.collection = 'code' AND f.root_id = ?1 AND f.path = ?2
                 )",
                params![key.root_id, key.path],
            )?;
            tx.execute(
                "DELETE FROM files WHERE collection = 'code' AND root_id = ?1 AND path = ?2",
                params![key.root_id, key.path],
            )?;
            tx.execute(
                "DELETE FROM overlay_chunks_fts WHERE rowid IN (
                     SELECT c.id FROM overlay_chunks c
                     JOIN overlay_files f ON f.id = c.file_id
                     WHERE f.collection = 'code' AND f.root_id = ?1 AND f.path = ?2
                 )",
                params![key.root_id, key.path],
            )?;
            tx.execute(
                "DELETE FROM overlay_files WHERE collection = 'code' AND root_id = ?1 AND path = ?2",
                params![key.root_id, key.path],
            )?;
            for table in ["overlay_fingerprint_cache", "context_dirty", "overlay_tombstones"] {
                let sql = format!("DELETE FROM {table} WHERE root_id = ?1 AND path = ?2");
                tx.execute(&sql, params![key.root_id, key.path])?;
            }
            if tick() {
                return Ok(ControlFlow::Break(()));
            }
        }

        let deleted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        for key in change.tombstones {
            tx.execute(
                "INSERT INTO overlay_tombstones (root_id, path, collection, deleted_at)
                 VALUES (?1, ?2, 'code', ?3)
                ON CONFLICT(root_id, path) DO UPDATE SET collection = 'code', deleted_at = ?3",
                params![key.root_id, key.path, deleted_at],
            )?;
            if tick() {
                return Ok(ControlFlow::Break(()));
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        for file in change.upserts {
            if file.chunks.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO files (root_id, path, hash, indexed_at, collection)
                 VALUES (?1, ?2, ?3, ?4, 'code')
                 ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4,
                                                        collection = 'code'",
                params![file.key.root_id, file.key.path, file.hash, now],
            )?;
            let file_id: i64 = tx.query_row(
                "SELECT id FROM files WHERE root_id = ?1 AND path = ?2",
                params![file.key.root_id, file.key.path],
                |row| row.get(0),
            )?;
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
                params![file_id],
            )?;
            tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;

            let mut chunk_stmt = tx.prepare(
                "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                     line_start, line_end, text, embedding, graph_context)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
            )?;
            let mut fts_stmt =
                tx.prepare("INSERT INTO chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)")?;
            for (index, chunk) in file.chunks.iter().enumerate() {
                let kind = match chunk.kind {
                    code_chunk::ChunkKind::ModuleHeader => "header",
                    code_chunk::ChunkKind::Procedure => "procedure",
                    code_chunk::ChunkKind::Function => "function",
                };
                let annotations =
                    (!chunk.annotations.is_empty()).then(|| chunk.annotations.join(","));
                let graph_context =
                    file.graph_contexts.get(index).and_then(|context| context.as_deref());
                chunk_stmt.execute(params![
                    file_id,
                    kind,
                    chunk.name,
                    chunk.is_export as i32,
                    annotations,
                    chunk.line_start,
                    chunk.line_end,
                    chunk.text,
                    graph_context,
                ])?;
                let chunk_id = tx.last_insert_rowid();
                fts_stmt.execute(params![chunk_id, chunk.name, chunk.text])?;
                if tick() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    /// As [`Self::reindex_file`], but persists each chunk's graph context (parallel to
    /// `chunks`) so a later reconstruction re-embeds with the same enriched text.
    pub fn reindex_file_with_context(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
        graph_contexts: Option<&[Option<String>]>,
    ) -> Result<i64, SearchError> {
        self.reindex_file_in_collection(
            root_id,
            path,
            hash,
            "code",
            chunks,
            embeddings,
            graph_contexts,
        )
    }

    pub(crate) fn reindex_file_with_context_checkpointed(
        &mut self,
        key: &FileKey,
        hash: &[u8],
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
        graph_contexts: Option<&[Option<String>]>,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<(), i64>, SearchError> {
        self.reindex_file_in_collection_checkpointed(
            &key.root_id,
            &key.path,
            hash,
            "code",
            chunks,
            embeddings,
            graph_contexts,
            checkpoint,
        )
    }

    // Every argument is a distinct input of one write — which row, what it now
    // holds, and the optional vectors and rendered contexts that go with it.
    // Bundling them into a context struct would only rename the same fields, so
    // the one-over-limit arity is accepted here.
    #[allow(clippy::too_many_arguments)]
    pub fn reindex_file_in_collection(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        collection: &str,
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
        graph_contexts: Option<&[Option<String>]>,
    ) -> Result<i64, SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.reindex_file_in_collection_checkpointed(
            root_id,
            path,
            hash,
            collection,
            chunks,
            embeddings,
            graph_contexts,
            &mut checkpoint,
        )? {
            ControlFlow::Continue(file_id) => Ok(file_id),
            ControlFlow::Break(()) => unreachable!("the permit-all checkpoint cannot cancel"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reindex_file_in_collection_checkpointed(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        collection: &str,
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
        graph_contexts: Option<&[Option<String>]>,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<(), i64>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.transaction()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        tx.execute(
            "INSERT INTO files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4, collection = ?5",
            params![root_id, path, hash, now, collection],
        )?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE root_id = ?1 AND path = ?2",
            params![root_id, path],
            |row| row.get(0),
        )?;

        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
            params![file_id],
        )?;

        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                     line_start, line_end, text, embedding, graph_context)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut fts_stmt =
                tx.prepare("INSERT INTO chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)")?;

            for (i, chunk) in chunks.iter().enumerate() {
                let kind_str = match chunk.kind {
                    code_chunk::ChunkKind::ModuleHeader => "header",
                    code_chunk::ChunkKind::Procedure => "procedure",
                    code_chunk::ChunkKind::Function => "function",
                };
                let annotations = if chunk.annotations.is_empty() {
                    None
                } else {
                    Some(chunk.annotations.join(","))
                };
                let embedding_blob: Option<Vec<u8>> = embeddings
                    .and_then(|embs| embs.get(i))
                    .map(|e| e.iter().flat_map(|f| f.to_le_bytes()).collect());
                let graph_context: Option<&str> =
                    graph_contexts.and_then(|gc| gc.get(i)).and_then(|g| g.as_deref());

                stmt.execute(params![
                    file_id,
                    kind_str,
                    chunk.name,
                    chunk.is_export as i32,
                    annotations,
                    chunk.line_start,
                    chunk.line_end,
                    chunk.text,
                    embedding_blob,
                    graph_context,
                ])?;

                let chunk_id = tx.last_insert_rowid();
                fts_stmt.execute(params![chunk_id, chunk.name, chunk.text])?;
                if (i + 1).is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                    && checkpoint().is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }

        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(file_id))
    }

    pub fn reindex_documents(
        &mut self,
        collection: &str,
        virtual_path: &str,
        hash: &[u8],
        documents: &[Document],
        embeddings: Option<&[Vec<f32>]>,
    ) -> Result<i64, SearchError> {
        let tx = self.conn.transaction()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        tx.execute(
            "INSERT INTO files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4, collection = ?5",
            params![CONFIGURATION_ROOT_ID, virtual_path, hash, now, collection],
        )?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE root_id = ?1 AND path = ?2",
            params![CONFIGURATION_ROOT_ID, virtual_path],
            |row| row.get(0),
        )?;

        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
            params![file_id],
        )?;
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                     line_start, line_end, text, embedding)
                 VALUES (?1, ?2, ?3, 0, NULL, 0, 0, ?4, ?5)",
            )?;
            let mut fts_stmt =
                tx.prepare("INSERT INTO chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)")?;

            for (i, doc) in documents.iter().enumerate() {
                let embedding_blob: Option<Vec<u8>> = embeddings
                    .and_then(|embs| embs.get(i))
                    .map(|e| e.iter().flat_map(|f| f.to_le_bytes()).collect());

                stmt.execute(params![file_id, doc.kind, doc.title, doc.body, embedding_blob])?;

                let chunk_id = tx.last_insert_rowid();
                fts_stmt.execute(params![chunk_id, doc.title, doc.body])?;
            }
        }

        tx.commit()?;
        Ok(file_id)
    }

    pub(crate) fn replace_reference_collection_if_stale(
        &mut self,
        collection: &str,
        virtual_path: &str,
        fingerprint: &str,
        documents: &[Document],
        embeddings: Option<&[Vec<f32>]>,
    ) -> Result<CollectionReplaceOutcome, SearchError> {
        let key = format!("reference_collection_fingerprint:{collection}");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let committed = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if committed.as_deref() == Some(fingerprint) {
            tx.commit()?;
            return Ok(CollectionReplaceOutcome {
                committed_fingerprint: fingerprint.to_owned(),
                written: false,
            });
        }

        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (
                 SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                 WHERE f.collection = ?1
             )",
            [collection],
        )?;
        tx.execute("DELETE FROM files WHERE collection = ?1", [collection])?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        tx.execute(
            "INSERT INTO files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![CONFIGURATION_ROOT_ID, virtual_path, fingerprint.as_bytes(), now, collection],
        )?;
        let file_id = tx.last_insert_rowid();
        {
            let mut chunks = tx.prepare(
                "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                     line_start, line_end, text, embedding)
                 VALUES (?1, ?2, ?3, 0, NULL, 0, 0, ?4, ?5)",
            )?;
            let mut fts =
                tx.prepare("INSERT INTO chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)")?;
            for (index, document) in documents.iter().enumerate() {
                let embedding: Option<Vec<u8>> = embeddings
                    .and_then(|items| items.get(index))
                    .map(|values| values.iter().flat_map(|value| value.to_le_bytes()).collect());
                chunks.execute(params![
                    file_id,
                    document.kind,
                    document.title,
                    document.body,
                    embedding,
                ])?;
                fts.execute(params![tx.last_insert_rowid(), document.title, document.body])?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, fingerprint],
        )?;
        tx.commit()?;
        Ok(CollectionReplaceOutcome {
            committed_fingerprint: fingerprint.to_owned(),
            written: true,
        })
    }

    pub(crate) fn clear_reference_collection_fingerprint(
        &self,
        collection: &str,
    ) -> Result<(), SearchError> {
        let key = format!("reference_collection_fingerprint:{collection}");
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    pub(crate) fn reference_collection_fingerprint(
        &self,
        collection: &str,
    ) -> Result<Option<String>, SearchError> {
        let key = format!("reference_collection_fingerprint:{collection}");
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()?)
    }

    pub fn reindex_indexed_documents_in_collection(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        collection: &str,
        documents: &[crate::IndexedDocument],
        embeddings: Option<&[Vec<f32>]>,
    ) -> Result<i64, SearchError> {
        let tx = self.conn.transaction()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        tx.execute(
            "INSERT INTO files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4, collection = ?5",
            params![root_id, path, hash, now, collection],
        )?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE root_id = ?1 AND path = ?2",
            params![root_id, path],
            |row| row.get(0),
        )?;

        tx.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
            params![file_id],
        )?;
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks (file_id, kind, symbol_name, is_export, annotations,
                                     line_start, line_end, text, embedding)
                 VALUES (?1, ?2, ?3, 0, NULL, ?4, ?5, ?6, ?7)",
            )?;
            let mut fts_stmt =
                tx.prepare("INSERT INTO chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)")?;

            for (idx, document) in documents.iter().enumerate() {
                let embedding_blob: Option<Vec<u8>> = embeddings
                    .and_then(|embs| embs.get(idx))
                    .map(|embedding| embedding.iter().flat_map(|f| f.to_le_bytes()).collect());

                stmt.execute(params![
                    file_id,
                    document.kind,
                    document.symbol_name,
                    document.line_start,
                    document.line_end,
                    document.text,
                    embedding_blob,
                ])?;

                let chunk_id = tx.last_insert_rowid();
                fts_stmt.execute(params![chunk_id, document.symbol_name, document.text])?;
            }
        }

        tx.commit()?;
        Ok(file_id)
    }

    pub fn load_all_embeddings(&self, dim: usize) -> Result<Vec<(i64, Vec<f32>)>, SearchError> {
        Self::read_all_embeddings(&self.conn, dim)
    }

    /// The embeddings the vector index is built from, paired with the `embedding_generation` they
    /// were read at — both captured in one read transaction so the generation exactly describes
    /// this snapshot of the data. The persisted index records this generation; a later cold start
    /// that sees the same generation can trust the index without re-reading every BLOB (a concurrent
    /// writer that bumps the generation during the long HNSW build only makes a later load rebuild).
    pub fn load_all_embeddings_with_generation(
        &self,
        dim: usize,
    ) -> Result<EmbeddingsSnapshot, SearchError> {
        let tx = self.conn.unchecked_transaction()?;
        let generation = Self::read_embedding_generation(&tx)?;
        let data = Self::read_all_embeddings(&tx, dim)?;
        // Read-only: drop the transaction without committing.
        Ok((generation, data))
    }

    /// The current `embedding_generation` counter (O(1) single-row read). `Store::open` always
    /// seeds the row, so a missing row means corrupt/foreign state; it maps to `-1`, a sentinel
    /// that can never equal a real generation (which is `>= 0`, since a fresh build can stamp 0),
    /// so a stale gen-0 sidecar cannot validate against a database whose counter has gone missing.
    pub fn embedding_generation(&self) -> Result<i64, SearchError> {
        Self::read_embedding_generation(&self.conn)
    }

    /// Missing-row sentinel (see [`Self::embedding_generation`]): distinct from every persisted
    /// generation so it never produces a false-accept.
    const MISSING_GENERATION: i64 = -1;

    fn read_embedding_generation(conn: &Connection) -> Result<i64, SearchError> {
        let generation = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'embedding_generation'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(Self::MISSING_GENERATION);
        Ok(generation)
    }

    fn read_all_embeddings(
        conn: &Connection,
        dim: usize,
    ) -> Result<Vec<(i64, Vec<f32>)>, SearchError> {
        let mut stmt =
            conn.prepare("SELECT id, embedding FROM chunks WHERE embedding IS NOT NULL")?;

        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        let mut result = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            if blob.len() == dim * 4 {
                let embedding: Vec<f32> =
                    blob.as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect();
                result.push((id, embedding));
            }
        }
        Ok(result)
    }

    pub fn chunk_by_id(&self, chunk_id: i64) -> Result<Option<ChunkInfo>, SearchError> {
        let info = self
            .conn
            .query_row(
                "SELECT c.kind, c.symbol_name, c.line_start, c.line_end, c.text,
                        c.annotations, c.is_export, f.path, f.collection, f.root_id
                 FROM chunks c
                 JOIN files f ON f.id = c.file_id
                 WHERE c.id = ?1",
                params![chunk_id],
                |row| {
                    Ok(ChunkInfo {
                        root_id: row.get(9)?,
                        file_path: row.get(7)?,
                        collection: row.get(8)?,
                        kind: row.get(0)?,
                        symbol_name: row.get(1)?,
                        line_start: row.get(2)?,
                        line_end: row.get(3)?,
                        text: row.get(4)?,
                        annotations: row.get::<_, Option<String>>(5)?,
                        is_export: row.get::<_, i32>(6)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(info)
    }

    /// Fetch metadata for many chunks in one round-trip, keyed by id.
    ///
    /// The per-result `chunk_by_id` loop after a vector/FTS search issues one SELECT per hit; on a
    /// wide fetch window that N+1 dominates the query latency. Batching collapses it to a single
    /// `IN (...)` query. SQLite caps bound parameters (`SQLITE_MAX_VARIABLE_NUMBER`, 999 on older
    /// builds), so the id list is chunked under that cap and the partial maps merged.
    pub fn chunks_by_ids(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, ChunkInfo>, SearchError> {
        const MAX_VARS: usize = 900;
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(out);
        }
        for batch in ids.chunks(MAX_VARS) {
            let placeholders = std::iter::repeat_n("?", batch.len()).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT c.id, c.kind, c.symbol_name, c.line_start, c.line_end, c.text,
                        c.annotations, c.is_export, f.path, f.collection, f.root_id
                 FROM chunks c
                 JOIN files f ON f.id = c.file_id
                 WHERE c.id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(batch.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    ChunkInfo {
                        root_id: row.get(10)?,
                        file_path: row.get(8)?,
                        collection: row.get(9)?,
                        kind: row.get(1)?,
                        symbol_name: row.get(2)?,
                        line_start: row.get(3)?,
                        line_end: row.get(4)?,
                        text: row.get(5)?,
                        annotations: row.get::<_, Option<String>>(6)?,
                        is_export: row.get::<_, i32>(7)? != 0,
                    },
                ))
            })?;
            for row in rows {
                let (id, info) = row?;
                out.insert(id, info);
            }
        }
        Ok(out)
    }

    pub fn all_files(&self) -> Result<Vec<(FileKey, Vec<u8>)>, SearchError> {
        let mut stmt = self.conn.prepare("SELECT root_id, path, hash FROM files")?;
        let rows = stmt.query_map([], |row| Ok((file_key_row(row)?, row.get::<_, Vec<u8>>(2)?)))?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn all_files_in_collection(
        &self,
        collection: &str,
    ) -> Result<Vec<(FileKey, Vec<u8>)>, SearchError> {
        let mut stmt =
            self.conn.prepare("SELECT root_id, path, hash FROM files WHERE collection = ?1")?;
        let rows = stmt.query_map(params![collection], |row| {
            Ok((file_key_row(row)?, row.get::<_, Vec<u8>>(2)?))
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn clear_collection(&self, collection: &str) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (
                 SELECT c.id FROM chunks c
                 JOIN files f ON f.id = c.file_id
                 WHERE f.collection = ?1
             )",
            params![collection],
        )?;
        self.conn.execute("DELETE FROM files WHERE collection = ?1", params![collection])?;
        Ok(())
    }

    pub fn chunk_count(&self) -> Result<usize, SearchError> {
        let count: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn load_indexed_documents(
        &self,
        collection: Option<&str>,
    ) -> Result<Vec<crate::IndexedDocument>, SearchError> {
        let query = if collection.is_some() {
            "SELECT f.collection, f.path, c.symbol_name, c.kind, c.line_start, c.line_end, c.text,
                    c.graph_context, f.root_id
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE f.collection = ?1
             ORDER BY f.collection, f.root_id, f.path, c.line_start, c.line_end, c.symbol_name"
        } else {
            "SELECT f.collection, f.path, c.symbol_name, c.kind, c.line_start, c.line_end, c.text,
                    c.graph_context, f.root_id
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             ORDER BY f.collection, f.root_id, f.path, c.line_start, c.line_end, c.symbol_name"
        };

        let mut stmt = self.conn.prepare(query)?;
        let rows = if let Some(collection) = collection {
            stmt.query_map(params![collection], |row| {
                let text: String = row.get(6)?;
                Ok(crate::IndexedDocument {
                    collection: row.get(0)?,
                    root_id: row.get(8)?,
                    path: row.get(1)?,
                    symbol_name: row.get(2)?,
                    kind: row.get(3)?,
                    line_start: row.get::<_, i64>(4)? as u32,
                    line_end: row.get::<_, i64>(5)? as u32,
                    content_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
                    text,
                    graph_context: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], |row| {
                let text: String = row.get(6)?;
                Ok(crate::IndexedDocument {
                    collection: row.get(0)?,
                    root_id: row.get(8)?,
                    path: row.get(1)?,
                    symbol_name: row.get(2)?,
                    kind: row.get(3)?,
                    line_start: row.get::<_, i64>(4)? as u32,
                    line_end: row.get::<_, i64>(5)? as u32,
                    content_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
                    text,
                    graph_context: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        Ok(rows)
    }

    /// Chunks in `collection` whose embedding has not been computed yet, each paired
    /// with its row id. Powers the fused cold-build's separate embedding phase: the
    /// graph pass writes chunk text + FTS + graph context with a NULL embedding, then
    /// this lists exactly what still needs a vector — so embedding stays decoupled
    /// from the graph build's lifecycle.
    pub fn load_pending_embedding_documents(
        &self,
        collection: &str,
    ) -> Result<Vec<(i64, crate::IndexedDocument)>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, f.collection, f.path, c.symbol_name, c.kind, c.line_start, c.line_end,
                    c.text, c.graph_context, f.root_id
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE f.collection = ?1 AND c.embedding IS NULL
             ORDER BY f.root_id, f.path, c.line_start, c.line_end, c.symbol_name",
        )?;
        let rows = stmt
            .query_map(params![collection], |row| {
                let id: i64 = row.get(0)?;
                let text: String = row.get(7)?;
                Ok((
                    id,
                    crate::IndexedDocument {
                        collection: row.get(1)?,
                        root_id: row.get(9)?,
                        path: row.get(2)?,
                        symbol_name: row.get(3)?,
                        kind: row.get(4)?,
                        line_start: row.get::<_, i64>(5)? as u32,
                        line_end: row.get::<_, i64>(6)? as u32,
                        content_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
                        text,
                        graph_context: row.get(8)?,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every chunk owned by `path` in `collection`, as `(id, symbol_name, kind,
    /// graph_context)`. Powers the context re-render: the caller re-derives each chunk's
    /// graph context from the freshly published graph and compares it against the stored
    /// value here.
    pub fn chunks_with_context_for_file(
        &self,
        collection: &str,
        root_id: &str,
        path: &str,
    ) -> Result<Vec<ChunkContextRow>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.symbol_name, c.kind, c.graph_context
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE f.root_id = ?1 AND f.path = ?2 AND f.collection = ?3",
        )?;
        let rows = stmt
            .query_map(params![root_id, path, collection], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Overwrite one chunk's stored `graph_context` by row id, leaving its embedding
    /// untouched. This deliberately does NOT touch the `embedding` column, so the
    /// `chunks_gen_upd` trigger (which fires only `AFTER UPDATE OF embedding`) does not
    /// bump `embedding_generation` and the persisted vector sidecar stays valid — a
    /// context re-render that produced the SAME string must invalidate nothing.
    pub fn set_chunk_graph_context(
        &self,
        chunk_id: i64,
        graph_context: Option<&str>,
    ) -> Result<(), SearchError> {
        self.conn.execute(
            "UPDATE chunks SET graph_context = ?2 WHERE id = ?1",
            params![chunk_id, graph_context],
        )?;
        Ok(())
    }

    /// Clear one chunk's embedding by row id (set it NULL), so the existing
    /// NULL-embedding embed machinery re-embeds it. This DOES fire `chunks_gen_upd` and
    /// bump `embedding_generation`, correctly invalidating the persisted vector sidecar
    /// because the chunk's vector must be recomputed.
    pub fn clear_chunk_embedding(&self, chunk_id: i64) -> Result<(), SearchError> {
        self.conn.execute("UPDATE chunks SET embedding = NULL WHERE id = ?1", params![chunk_id])?;
        Ok(())
    }

    /// Set one chunk's embedding by row id, leaving its text/FTS/context untouched —
    /// the write half of the fused build's embedding phase.
    pub fn set_chunk_embedding(&self, chunk_id: i64, embedding: &[f32]) -> Result<(), SearchError> {
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.conn
            .execute("UPDATE chunks SET embedding = ?2 WHERE id = ?1", params![chunk_id, blob])?;
        Ok(())
    }

    /// Commit one prepared embedding batch atomically.
    pub fn set_chunk_embeddings(&self, embeddings: &[(i64, Vec<f32>)]) -> Result<(), SearchError> {
        let tx = self.conn.unchecked_transaction()?;
        for (chunk_id, embedding) in embeddings {
            let blob: Vec<u8> = embedding.iter().flat_map(|value| value.to_le_bytes()).collect();
            tx.execute("UPDATE chunks SET embedding = ?2 WHERE id = ?1", params![chunk_id, blob])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn text_search(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<TextSearchResult>, SearchError> {
        let Some(match_query) = crate::lexical::fts5_match_query(query) else {
            return Ok(Vec::new());
        };
        let results = if let Some(coll) = collection {
            let mut stmt = self.conn.prepare(
                "SELECT chunks_fts.rowid, chunks_fts.rank
                 FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 JOIN files f ON f.id = c.file_id
                 WHERE chunks_fts MATCH ?1 AND f.collection = ?2
                 ORDER BY chunks_fts.rank
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![match_query, coll, limit as i64], |row| {
                Ok(TextSearchResult { chunk_id: row.get(0)?, rank: row.get(1)? })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT rowid, rank
                 FROM chunks_fts
                 WHERE chunks_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![match_query, limit as i64], |row| {
                Ok(TextSearchResult { chunk_id: row.get(0)?, rank: row.get(1)? })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        Ok(results)
    }

    pub fn rebuild_fts(&self) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.rebuild_fts_checkpointed(&mut checkpoint)? {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub(crate) fn rebuild_fts_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM chunks_fts", [])?;
        let mut offset = 0usize;
        loop {
            let inserted = tx.execute(
                "INSERT INTO chunks_fts(rowid, symbol_name, text)
                 SELECT id, symbol_name, text FROM chunks ORDER BY id LIMIT ?1 OFFSET ?2",
                params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64, offset as i64],
            )?;
            offset += inserted;
            if inserted == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                return Ok(ControlFlow::Break(()));
            }
            if inserted < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                break;
            }
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    pub fn fts_count(&self) -> Result<usize, SearchError> {
        let count: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM chunks_fts", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn file_count(&self) -> Result<usize, SearchError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn embedding_count_by_collection(&self, collection: &str) -> Result<usize, SearchError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE c.embedding IS NOT NULL AND f.collection = ?1",
            params![collection],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn clear_file_hashes(&self, collection: &str) -> Result<usize, SearchError> {
        let count = self.conn.execute(
            "UPDATE files SET hash = zeroblob(0) WHERE collection = ?1",
            params![collection],
        )?;
        Ok(count)
    }

    pub fn clear_file_hashes_without_embeddings(
        &self,
        collection: &str,
    ) -> Result<usize, SearchError> {
        // Clear the skip hash for any file that has even one un-embedded chunk, not
        // only files with zero embeddings. A partially embedded file (some chunks
        // vectored, some still NULL — e.g. a build interrupted mid-corpus, or the fused
        // cold-build's embedding phase failing after the chunks were written) must be
        // re-indexed in full on the next run; the previous `NOT IN (… IS NOT NULL)`
        // predicate kept such a file's hash and skipped it forever.
        let count = self.conn.execute(
            "UPDATE files SET hash = zeroblob(0)
             WHERE collection = ?1
               AND id IN (
                   SELECT DISTINCT file_id FROM chunks WHERE embedding IS NULL
               )",
            params![collection],
        )?;
        Ok(count)
    }

    pub fn upsert_baseline_manifest(
        &self,
        snapshot_id: &str,
        fingerprint: Option<&str>,
        manifest_files: usize,
    ) -> Result<(), SearchError> {
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn.execute(
            "INSERT INTO baseline_manifest (id, snapshot_id, fingerprint, manifest_files, fetched_at)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 snapshot_id = ?1,
                 fingerprint = ?2,
                 manifest_files = ?3,
                 fetched_at = ?4",
            params![snapshot_id, fingerprint, manifest_files as i64, fetched_at.to_string()],
        )?;
        Ok(())
    }

    pub fn save_baseline_manifest(
        &self,
        manifest: &crate::WorkspaceBaselineManifest,
    ) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.save_baseline_manifest_checkpointed(manifest, &mut checkpoint)? {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub fn save_baseline_manifest_checkpointed(
        &self,
        manifest: &crate::WorkspaceBaselineManifest,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO baseline_manifest (id, snapshot_id, fingerprint, manifest_files, fetched_at)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 snapshot_id = ?1,
                 fingerprint = ?2,
                 manifest_files = ?3,
                 fetched_at = ?4",
            params![
                manifest.snapshot_id,
                manifest.snapshot_fingerprint,
                manifest.files.len() as i64,
                fetched_at.to_string()
            ],
        )?;
        loop {
            let deleted = tx.execute(
                "DELETE FROM baseline_manifest_files WHERE rowid IN (
                     SELECT rowid FROM baseline_manifest_files LIMIT ?1
                 )",
                params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64],
            )?;
            if deleted == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                return Ok(ControlFlow::Break(()));
            }
            if deleted < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                break;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO baseline_manifest_files
                 (root_id, collection, path, file_fingerprint)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (index, file) in manifest.files.iter().enumerate() {
                stmt.execute(params![
                    file.root_id,
                    file.collection,
                    file.path,
                    file.file_fingerprint
                ])?;
                if (index + 1).is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                    && checkpoint().is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    pub fn load_baseline_manifest(&self) -> Result<Option<BaselineManifestRecord>, SearchError> {
        let record = self
            .conn
            .query_row(
                "SELECT snapshot_id, fingerprint, manifest_files, fetched_at
             FROM baseline_manifest WHERE id = 1",
                [],
                |row| {
                    Ok(BaselineManifestRecord {
                        snapshot_id: row.get(0)?,
                        fingerprint: row.get(1)?,
                        manifest_files: row.get::<_, i64>(2)? as usize,
                        fetched_at: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn load_baseline_manifest_fingerprints(
        &self,
        collection: &str,
    ) -> Result<Option<HashMap<FileKey, String>>, SearchError> {
        let has_manifest = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM baseline_manifest WHERE id = 1)",
            [],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !has_manifest {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT root_id, path, file_fingerprint
             FROM baseline_manifest_files
             WHERE collection = ?1",
        )?;
        let rows = stmt.query_map(params![collection], |row| {
            Ok((file_key_row(row)?, row.get::<_, String>(2)?))
        })?;
        let mut fingerprints = HashMap::new();
        for row in rows {
            let (key, fingerprint) = row?;
            fingerprints.insert(key, fingerprint);
        }
        Ok(Some(fingerprints))
    }

    /// Readers key manifest validity on the header row alone
    /// (`load_baseline_manifest_fingerprints`), so the header and the file rows must
    /// never be observable half-deleted: a surviving header over an emptied files table
    /// would read back as a valid-but-empty manifest. One transaction removes both.
    pub fn clear_baseline_manifest(&self) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.clear_baseline_manifest_checkpointed(&mut checkpoint)? {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub fn clear_baseline_manifest_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        loop {
            let deleted = tx.execute(
                "DELETE FROM baseline_manifest_files WHERE rowid IN (
                     SELECT rowid FROM baseline_manifest_files LIMIT ?1
                 )",
                params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64],
            )?;
            if deleted == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                return Ok(ControlFlow::Break(()));
            }
            if deleted < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                break;
            }
        }
        tx.execute("DELETE FROM baseline_manifest WHERE id = 1", [])?;
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    /// The persisted manifest header, but only when the `baseline_manifest_files` rows
    /// agree with the count recorded in it. The header is what downstream readers trust,
    /// while the file rows are what overlay diffing actually consumes — a database where
    /// the two disagree (a half-clear written by an older binary, manual surgery) must
    /// not be reused as a warm-boot cache. A mismatch reads as "no manifest".
    pub fn load_coherent_baseline_manifest(
        &self,
    ) -> Result<Option<BaselineManifestRecord>, SearchError> {
        let Some(record) = self.load_baseline_manifest()? else {
            return Ok(None);
        };
        let file_rows: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM baseline_manifest_files", [], |row| row.get(0))?;
        if file_rows as usize != record.manifest_files {
            return Ok(None);
        }
        Ok(Some(record))
    }

    pub fn insert_overlay_tombstone(
        &self,
        root_id: &str,
        path: &str,
        collection: &str,
    ) -> Result<(), SearchError> {
        let deleted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn.execute(
            "INSERT INTO overlay_tombstones (root_id, path, collection, deleted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root_id, path) DO UPDATE SET collection = ?3, deleted_at = ?4",
            params![root_id, path, collection, deleted_at.to_string()],
        )?;
        Ok(())
    }

    pub fn remove_overlay_tombstone(&self, root_id: &str, path: &str) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM overlay_tombstones WHERE root_id = ?1 AND path = ?2",
            params![root_id, path],
        )?;
        Ok(())
    }

    pub fn overlay_tombstone_paths(
        &self,
        collection: &str,
    ) -> Result<HashSet<FileKey>, SearchError> {
        let mut stmt = self
            .conn
            .prepare("SELECT root_id, path FROM overlay_tombstones WHERE collection = ?1")?;
        let rows = stmt.query_map(params![collection], file_key_row)?;
        let mut keys = HashSet::new();
        for row in rows {
            keys.insert(row?);
        }
        Ok(keys)
    }

    pub fn clear_overlay_tombstones(&self, collection: &str) -> Result<(), SearchError> {
        self.conn
            .execute("DELETE FROM overlay_tombstones WHERE collection = ?1", params![collection])?;
        Ok(())
    }

    pub fn upsert_overlay_file_with_chunks(
        &mut self,
        root_id: &str,
        path: &str,
        hash: &[u8],
        collection: &str,
        chunks: &[Chunk],
        embeddings: Option<&[Vec<f32>]>,
    ) -> Result<i64, SearchError> {
        let tx = self.conn.transaction()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        tx.execute(
            "INSERT INTO overlay_files (root_id, path, hash, indexed_at, collection)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_id, path) DO UPDATE SET hash = ?3, indexed_at = ?4, collection = ?5",
            params![root_id, path, hash, now, collection],
        )?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM overlay_files WHERE root_id = ?1 AND path = ?2",
            params![root_id, path],
            |row| row.get(0),
        )?;

        tx.execute(
            "DELETE FROM overlay_chunks_fts WHERE rowid IN (SELECT id FROM overlay_chunks WHERE file_id = ?1)",
            params![file_id],
        )?;
        tx.execute("DELETE FROM overlay_chunks WHERE file_id = ?1", params![file_id])?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO overlay_chunks (file_id, kind, symbol_name, is_export, annotations,
                     line_start, line_end, text, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            let mut fts_stmt = tx.prepare(
                "INSERT INTO overlay_chunks_fts(rowid, symbol_name, text) VALUES (?1, ?2, ?3)",
            )?;

            for (i, chunk) in chunks.iter().enumerate() {
                let kind_str = match chunk.kind {
                    code_chunk::ChunkKind::ModuleHeader => "header",
                    code_chunk::ChunkKind::Procedure => "procedure",
                    code_chunk::ChunkKind::Function => "function",
                };
                let annotations = if chunk.annotations.is_empty() {
                    None
                } else {
                    Some(chunk.annotations.join(","))
                };
                let embedding_blob: Option<Vec<u8>> = embeddings
                    .and_then(|embs| embs.get(i))
                    .map(|e| e.iter().flat_map(|f| f.to_le_bytes()).collect());

                stmt.execute(params![
                    file_id,
                    kind_str,
                    chunk.name,
                    chunk.is_export as i32,
                    annotations,
                    chunk.line_start,
                    chunk.line_end,
                    chunk.text,
                    embedding_blob,
                ])?;

                let chunk_id = tx.last_insert_rowid();
                fts_stmt.execute(params![chunk_id, chunk.name, chunk.text])?;
            }
        }

        tx.commit()?;
        Ok(file_id)
    }

    pub fn remove_overlay_file(&self, root_id: &str, path: &str) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM overlay_chunks_fts WHERE rowid IN (
                 SELECT c.id FROM overlay_chunks c
                 JOIN overlay_files f ON f.id = c.file_id
                 WHERE f.root_id = ?1 AND f.path = ?2
             )",
            params![root_id, path],
        )?;
        self.conn.execute(
            "DELETE FROM overlay_files WHERE root_id = ?1 AND path = ?2",
            params![root_id, path],
        )?;
        Ok(())
    }

    pub fn overlay_text_search(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<TextSearchResult>, SearchError> {
        let Some(match_query) = crate::lexical::fts5_match_query(query) else {
            return Ok(Vec::new());
        };
        let results = if let Some(coll) = collection {
            let mut stmt = self.conn.prepare(
                "SELECT overlay_chunks_fts.rowid, overlay_chunks_fts.rank
                 FROM overlay_chunks_fts
                 JOIN overlay_chunks c ON c.id = overlay_chunks_fts.rowid
                 JOIN overlay_files f ON f.id = c.file_id
                 WHERE overlay_chunks_fts MATCH ?1 AND f.collection = ?2
                 ORDER BY overlay_chunks_fts.rank
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![match_query, coll, limit as i64], |row| {
                Ok(TextSearchResult { chunk_id: row.get(0)?, rank: row.get(1)? })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT rowid, rank
                 FROM overlay_chunks_fts
                 WHERE overlay_chunks_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![match_query, limit as i64], |row| {
                Ok(TextSearchResult { chunk_id: row.get(0)?, rank: row.get(1)? })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        Ok(results)
    }

    pub fn overlay_chunks_by_ids(&self, chunk_ids: &[i64]) -> Result<Vec<ChunkInfo>, SearchError> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT c.kind, c.symbol_name, c.line_start, c.line_end, c.text,
                    c.annotations, c.is_export, f.path, f.collection, f.root_id
             FROM overlay_chunks c
             JOIN overlay_files f ON f.id = c.file_id
             WHERE c.id IN ({})",
            placeholders
        );
        let mut stmt = self.conn.prepare(&query)?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            Ok(ChunkInfo {
                root_id: row.get(9)?,
                file_path: row.get(7)?,
                collection: row.get(8)?,
                kind: row.get(0)?,
                symbol_name: row.get(1)?,
                line_start: row.get(2)?,
                line_end: row.get(3)?,
                text: row.get(4)?,
                annotations: row.get::<_, Option<String>>(5)?,
                is_export: row.get::<_, i32>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_overlay_embeddings(&self, dim: usize) -> Result<Vec<(i64, Vec<f32>)>, SearchError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding FROM overlay_chunks WHERE embedding IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            if blob.len() == dim * 4 {
                let embedding: Vec<f32> =
                    blob.as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect();
                result.push((id, embedding));
            }
        }
        Ok(result)
    }

    pub fn overlay_file_count(&self, collection: &str) -> Result<usize, SearchError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM overlay_files WHERE collection = ?1",
            params![collection],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn overlay_chunk_count(&self, collection: &str) -> Result<usize, SearchError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM overlay_chunks c
             JOIN overlay_files f ON f.id = c.file_id
             WHERE f.collection = ?1",
            params![collection],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn overlay_tombstone_count(&self, collection: &str) -> Result<usize, SearchError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM overlay_tombstones WHERE collection = ?1",
            params![collection],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn load_overlay_fingerprint_cache(
        &self,
        manifest_snapshot_id: &str,
    ) -> Result<Option<HashMap<FileKey, PersistedFingerprint>>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT root_id, path, file_size, file_mtime_secs, file_mtime_nanos,
                    content_fingerprint, canonical
             FROM overlay_fingerprint_cache
             WHERE manifest_snapshot_id = ?1",
        )?;
        let rows = stmt.query_map(params![manifest_snapshot_id], |row| {
            Ok((
                file_key_row(row)?,
                PersistedFingerprint {
                    file_size: row.get::<_, i64>(2)? as u64,
                    file_mtime_secs: row.get::<_, i64>(3)?,
                    file_mtime_nanos: row.get::<_, u32>(4)?,
                    content_fingerprint: row.get::<_, String>(5)?,
                    canonical: row.get::<_, String>(6)?,
                },
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (key, entry) = row?;
            map.insert(key, entry);
        }
        if map.is_empty() {
            let any_rows: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM overlay_fingerprint_cache LIMIT 1)",
                [],
                |row| row.get(0),
            )?;
            if any_rows {
                self.clear_overlay_fingerprint_cache()?;
            }
            return Ok(None);
        }
        Ok(Some(map))
    }

    pub fn save_overlay_fingerprint_cache(
        &self,
        manifest_snapshot_id: &str,
        entries: &HashMap<FileKey, PersistedFingerprint>,
    ) -> Result<(), SearchError> {
        // One transaction end to end: a failed INSERT rolls the DELETE back too, so `Err`
        // means "the table is exactly as it was" — a committed half-replacement would leave
        // survivors telling a story no pass ever proved.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM overlay_fingerprint_cache", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO overlay_fingerprint_cache
                 (root_id, path, file_size, file_mtime_secs, file_mtime_nanos, content_fingerprint,
                  manifest_snapshot_id, canonical)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (key, entry) in entries {
                stmt.execute(params![
                    key.root_id,
                    key.path,
                    entry.file_size as i64,
                    entry.file_mtime_secs,
                    entry.file_mtime_nanos,
                    entry.content_fingerprint,
                    manifest_snapshot_id,
                    entry.canonical,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist the two shared Phase-C caches in one cancellable transaction. A break drops the
    /// transaction, allowing the caller to retain and retry the same in-memory publication.
    pub(crate) fn apply_overlay_publication(
        &self,
        fingerprints: Option<(&str, &HashMap<FileKey, PersistedFingerprint>)>,
        embeddings: Option<OverlayEmbeddingPublication<'_>>,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut rows = 0usize;
        let mut tick = || {
            rows += 1;
            rows.is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                && checkpoint().is_break()
        };
        if let Some((snapshot_id, entries)) = fingerprints {
            tx.execute("DELETE FROM overlay_fingerprint_cache", [])?;
            let mut stmt = tx.prepare(
                "INSERT INTO overlay_fingerprint_cache
                 (root_id, path, file_size, file_mtime_secs, file_mtime_nanos, content_fingerprint,
                  manifest_snapshot_id, canonical)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (key, entry) in entries {
                stmt.execute(params![
                    key.root_id,
                    key.path,
                    entry.file_size as i64,
                    entry.file_mtime_secs,
                    entry.file_mtime_nanos,
                    entry.content_fingerprint,
                    snapshot_id,
                    entry.canonical,
                ])?;
                if tick() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        if let Some((model_id, dimension, entries)) = embeddings {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO overlay_embedding_cache
                 (embedding_key, model_id, dimension, embedding)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (embedding_key, embedding) in entries {
                let blob: Vec<u8> =
                    embedding.iter().flat_map(|value| value.to_le_bytes()).collect();
                stmt.execute(params![embedding_key, model_id, dimension as i64, blob])?;
                if tick() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    /// Every key the fingerprint cache holds, whatever snapshot it was written for.
    ///
    /// Deliberately not [`Self::load_overlay_fingerprint_cache`]: that one takes a snapshot id
    /// and CLEARS the whole table when the rows belong to another one, which is right for a
    /// refresh reading its own snapshot but destructive for a caller that only wants to know
    /// which keys still have a row. It also needs no manifest header, so a reconcile of a
    /// local index does not depend on a carrier that mode does not serve.
    pub fn overlay_fingerprint_keys(&self) -> Result<HashSet<FileKey>, SearchError> {
        let mut stmt = self.conn.prepare("SELECT root_id, path FROM overlay_fingerprint_cache")?;
        let rows = stmt.query_map([], file_key_row)?.collect::<Result<HashSet<FileKey>, _>>()?;
        Ok(rows)
    }

    pub fn clear_overlay_fingerprint_cache(&self) -> Result<(), SearchError> {
        let mut checkpoint = || ControlFlow::Continue(());
        match self.clear_overlay_fingerprint_cache_checkpointed(&mut checkpoint)? {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => unreachable!("permit-all checkpoint cannot cancel"),
        }
    }

    pub fn clear_overlay_fingerprint_cache_checkpointed(
        &self,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        loop {
            let deleted = tx.execute(
                "DELETE FROM overlay_fingerprint_cache WHERE rowid IN (
                     SELECT rowid FROM overlay_fingerprint_cache LIMIT ?1
                 )",
                params![crate::engine::WORKSPACE_APPLY_BATCH_ROWS as i64],
            )?;
            if deleted == crate::engine::WORKSPACE_APPLY_BATCH_ROWS && checkpoint().is_break() {
                return Ok(ControlFlow::Break(()));
            }
            if deleted < crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
                break;
            }
        }
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }

    /// Drop exactly these keys' fingerprint rows, leaving every other row alone. A row asserts
    /// "this file was verified against the manifest", so the caller that failed to stat or read a
    /// file must retract the claim for THAT file without wiping the verified neighbours — a
    /// table-wide delete here would cost a full re-read of the workspace on the next plan.
    pub fn delete_overlay_fingerprint_entries(&self, keys: &[FileKey]) -> Result<(), SearchError> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM overlay_fingerprint_cache WHERE root_id = ?1 AND path = ?2")?;
        for key in keys {
            stmt.execute(params![key.root_id, key.path])?;
        }
        Ok(())
    }

    pub fn load_overlay_embedding_cache(
        &self,
        model_id: &str,
        dimension: usize,
    ) -> Result<HashMap<String, Vec<f32>>, SearchError> {
        let dimension = dimension as i64;
        let mut stmt = self.conn.prepare(
            "SELECT embedding_key, embedding
             FROM overlay_embedding_cache
             WHERE model_id = ?1 AND dimension = ?2",
        )?;
        let rows = stmt.query_map(params![model_id, dimension], |row| {
            let hash: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((hash, blob))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (hash, blob) = row?;
            if blob.len() % 4 == 0 {
                let embedding: Vec<f32> =
                    blob.as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect();
                map.insert(hash, embedding);
            }
        }
        Ok(map)
    }

    pub fn save_overlay_embedding_cache(
        &self,
        model_id: &str,
        dimension: usize,
        entries: &HashMap<String, Vec<f32>>,
    ) -> Result<(), SearchError> {
        let dimension = dimension as i64;
        let mut stmt = self.conn.prepare(
            "INSERT OR REPLACE INTO overlay_embedding_cache
             (embedding_key, model_id, dimension, embedding)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (embedding_key, embedding) in entries {
            let blob: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
            stmt.execute(params![embedding_key, model_id, dimension, blob])?;
        }
        Ok(())
    }

    pub fn clear_overlay_embedding_cache(&self) -> Result<(), SearchError> {
        self.conn.execute("DELETE FROM overlay_embedding_cache", [])?;
        Ok(())
    }

    pub fn clear_overlay_state(&self, collection: &str) -> Result<(), SearchError> {
        self.conn.execute(
            "DELETE FROM overlay_chunks_fts WHERE rowid IN (
                 SELECT c.id FROM overlay_chunks c
                 JOIN overlay_files f ON f.id = c.file_id
                 WHERE f.collection = ?1
             )",
            params![collection],
        )?;
        self.conn.execute(
            "DELETE FROM overlay_chunks WHERE file_id IN (
                 SELECT id FROM overlay_files WHERE collection = ?1
             )",
            params![collection],
        )?;
        self.conn
            .execute("DELETE FROM overlay_files WHERE collection = ?1", params![collection])?;
        self.clear_overlay_tombstones(collection)?;
        Ok(())
    }

    /// Atomically clear the local collection and its overlay carriers during workspace bootstrap.
    pub fn clear_workspace_overlay_checkpointed(
        &self,
        collection: &str,
        checkpoint: &mut dyn FnMut() -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, SearchError> {
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let tx = self.conn.unchecked_transaction()?;
        let file_ids = {
            let mut stmt = tx.prepare("SELECT id FROM files WHERE collection = ?1 ORDER BY id")?;
            let ids = stmt
                .query_map(params![collection], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        let overlay_ids = {
            let mut stmt =
                tx.prepare("SELECT id FROM overlay_files WHERE collection = ?1 ORDER BY id")?;
            let ids = stmt
                .query_map(params![collection], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        let mut rows = 0usize;
        for file_id in file_ids {
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
                params![file_id],
            )?;
            tx.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
            rows += 1;
            if rows.is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                && checkpoint().is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
        }
        for file_id in overlay_ids {
            tx.execute(
                "DELETE FROM overlay_chunks_fts
                 WHERE rowid IN (SELECT id FROM overlay_chunks WHERE file_id = ?1)",
                params![file_id],
            )?;
            tx.execute("DELETE FROM overlay_files WHERE id = ?1", params![file_id])?;
            rows += 1;
            if rows.is_multiple_of(crate::engine::WORKSPACE_APPLY_BATCH_ROWS)
                && checkpoint().is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
        }
        tx.execute("DELETE FROM overlay_tombstones WHERE collection = ?1", params![collection])?;
        if checkpoint().is_break() {
            return Ok(ControlFlow::Break(()));
        }
        tx.commit()?;
        Ok(ControlFlow::Continue(()))
    }
}

#[derive(Debug, Clone)]
pub struct TextSearchResult {
    pub chunk_id: i64,
    pub rank: f64,
}

/// The identity of a row whose first two selected columns are `root_id, path`.
/// Every listing query selects them in that order so the key is read the same
/// way everywhere.
fn file_key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileKey> {
    Ok(FileKey::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?))
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub root_id: String,
    pub file_path: String,
    pub collection: String,
    pub kind: String,
    pub symbol_name: String,
    pub line_start: u32,
    pub line_end: u32,
    pub text: String,
    pub annotations: Option<String>,
    pub is_export: bool,
}

#[derive(Debug, Clone)]
pub struct BaselineManifestRecord {
    pub snapshot_id: String,
    pub fingerprint: Option<String>,
    pub manifest_files: usize,
    pub fetched_at: String,
}

#[derive(Debug, Clone)]
pub struct PersistedFingerprint {
    pub file_size: u64,
    pub file_mtime_secs: i64,
    pub file_mtime_nanos: u32,
    pub content_fingerprint: String,
    /// The physical spelling of the file the row was verified against. An empty string is a row
    /// from before the column existed: it must NOT count as a match — the gate re-reads and the
    /// re-save writes the spelling, so old rows heal themselves.
    pub canonical: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_roots::{FileKey, CONFIGURATION_ROOT_ID};
    use code_chunk::{Chunk, ChunkKind};

    /// The `canonical` column lands via an in-place ALTER, not via the version-bump wipe: rows
    /// written before the column keep living (in the altered table AND in its neighbours), and
    /// the grafted column arrives empty. Dropping the column emulates a database of the release
    /// before it — "column present and working" alone would not tell the ALTER from a wipe that
    /// recreated everything.
    #[test]
    fn the_canonical_column_is_added_in_place_without_wiping() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        {
            let store = Store::open(&db_path).unwrap();
            store.upsert_file(CONFIGURATION_ROOT_ID, "Neighbour.bsl", b"hash", "code").unwrap();
            store
                .save_overlay_fingerprint_cache(
                    "snap",
                    &std::collections::HashMap::from([(
                        FileKey::configuration("Cached.bsl"),
                        PersistedFingerprint {
                            file_size: 7,
                            file_mtime_secs: 1,
                            file_mtime_nanos: 2,
                            content_fingerprint: "fp".to_owned(),
                            canonical: "/spelled".to_owned(),
                        },
                    )]),
                )
                .unwrap();
        }
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute("ALTER TABLE overlay_fingerprint_cache DROP COLUMN canonical", [])
                .unwrap();
        }

        let store = Store::open(&db_path).unwrap();
        let rows = store.load_overlay_fingerprint_cache("snap").unwrap().unwrap_or_default();
        let row = rows.get(&FileKey::configuration("Cached.bsl")).expect("the row survived");
        assert_eq!(
            (row.file_size, row.canonical.as_str()),
            (7, ""),
            "data intact, the grafted column arrives empty"
        );
        assert_eq!(
            store.all_files_in_collection("code").unwrap().len(),
            1,
            "the neighbouring table survived too"
        );
    }

    /// The replace-save is one transaction: an INSERT that fails mid-way must leave the table
    /// in its ORIGINAL state — a committed DELETE with partial inserts would mean `Err` no
    /// longer implies "nothing changed on disk", and the survivors would tell a half-story.
    #[test]
    fn a_failed_replace_save_leaves_the_table_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");
        let store = Store::open(&db_path).unwrap();
        let row = |fp: &str| PersistedFingerprint {
            file_size: 1,
            file_mtime_secs: 1,
            file_mtime_nanos: 0,
            content_fingerprint: fp.to_owned(),
            canonical: String::new(),
        };
        store
            .save_overlay_fingerprint_cache(
                "snap",
                &std::collections::HashMap::from([(FileKey::configuration("Old.bsl"), row("old"))]),
            )
            .unwrap();

        let saboteur = Connection::open(&db_path).unwrap();
        saboteur
            .execute_batch(
                "CREATE TRIGGER deny_b_insert BEFORE INSERT ON overlay_fingerprint_cache \
                 WHEN NEW.path = 'B.bsl' BEGIN SELECT RAISE(FAIL, 'deny'); END;",
            )
            .unwrap();
        let result = store.save_overlay_fingerprint_cache(
            "snap",
            &std::collections::HashMap::from([
                (FileKey::configuration("A.bsl"), row("a")),
                (FileKey::configuration("B.bsl"), row("b")),
            ]),
        );
        assert!(result.is_err(), "the denied insert surfaces as an error");
        let rows = store.load_overlay_fingerprint_cache("snap").unwrap().unwrap_or_default();
        assert_eq!(rows.len(), 1, "the original table survived intact");
        assert!(rows.contains_key(&FileKey::configuration("Old.bsl")));
    }

    /// `open_existing` opens EXISTING stores only: a missing path is an error and no empty
    /// file is materialized — the standalone pass has no business creating shared state.
    #[test]
    fn open_existing_does_not_create_a_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(Store::open_existing(&path).is_err());
        assert!(!path.exists(), "the refusal must leave no file behind");
    }

    /// A store in the shape the release before composite keys wrote, built with
    /// raw SQL on purpose.
    ///
    /// A fixture assembled by today's `create_schema` would stop modelling the
    /// old state the moment the schema changes, and would then keep passing
    /// without the migration ever running. This one is frozen: it is what is
    /// actually on disk in front of an upgrading user.
    ///
    /// Ids are deliberately sparse and out of order — a rebuild that renumbers
    /// rows would still satisfy a row count.
    fn write_pre_root_id_store(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT INTO meta (key, value) VALUES ('embedding_generation', '17');

            CREATE TABLE files (
                id         INTEGER PRIMARY KEY,
                path       TEXT    NOT NULL UNIQUE,
                hash       BLOB    NOT NULL,
                indexed_at INTEGER NOT NULL,
                collection TEXT    NOT NULL DEFAULT 'code'
            );
            CREATE TABLE chunks (
                id          INTEGER PRIMARY KEY,
                file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                kind        TEXT    NOT NULL,
                symbol_name TEXT    NOT NULL,
                is_export   INTEGER NOT NULL DEFAULT 0,
                annotations TEXT,
                line_start  INTEGER NOT NULL,
                line_end    INTEGER NOT NULL,
                text        TEXT    NOT NULL,
                embedding   BLOB,
                graph_context TEXT
            );
            CREATE INDEX idx_chunks_file ON chunks(file_id);
            CREATE VIRTUAL TABLE chunks_fts USING fts5(symbol_name, text, tokenize='unicode61');

            CREATE TABLE baseline_manifest (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                snapshot_id TEXT NOT NULL,
                fingerprint TEXT,
                manifest_files INTEGER NOT NULL DEFAULT 0,
                fetched_at TEXT NOT NULL
            );
            CREATE TABLE baseline_manifest_files (
                collection       TEXT NOT NULL DEFAULT 'code',
                path             TEXT NOT NULL,
                file_fingerprint TEXT NOT NULL,
                PRIMARY KEY (collection, path)
            );
            CREATE TABLE overlay_tombstones (
                path       TEXT NOT NULL UNIQUE,
                collection TEXT NOT NULL DEFAULT 'code',
                deleted_at TEXT NOT NULL
            );
            CREATE TABLE overlay_files (
                id         INTEGER PRIMARY KEY,
                path       TEXT    NOT NULL UNIQUE,
                hash       BLOB    NOT NULL,
                indexed_at INTEGER NOT NULL,
                collection TEXT    NOT NULL DEFAULT 'code'
            );
            CREATE TABLE overlay_chunks (
                id          INTEGER PRIMARY KEY,
                file_id     INTEGER NOT NULL REFERENCES overlay_files(id) ON DELETE CASCADE,
                kind        TEXT    NOT NULL,
                symbol_name TEXT    NOT NULL,
                is_export   INTEGER NOT NULL DEFAULT 0,
                annotations TEXT,
                line_start  INTEGER NOT NULL,
                line_end    INTEGER NOT NULL,
                text        TEXT    NOT NULL,
                embedding   BLOB
            );
            CREATE INDEX idx_overlay_chunks_file ON overlay_chunks(file_id);
            CREATE VIRTUAL TABLE overlay_chunks_fts USING fts5(symbol_name, text, tokenize='unicode61');

            CREATE TABLE overlay_fingerprint_cache (
                path                 TEXT NOT NULL PRIMARY KEY,
                collection           TEXT NOT NULL DEFAULT 'code',
                file_size            INTEGER NOT NULL,
                file_mtime_secs      INTEGER NOT NULL,
                file_mtime_nanos     INTEGER NOT NULL,
                content_fingerprint  TEXT NOT NULL,
                manifest_snapshot_id TEXT NOT NULL
            );
            CREATE TABLE overlay_embedding_cache (
                embedding_key TEXT NOT NULL PRIMARY KEY,
                model_id      TEXT NOT NULL,
                dimension     INTEGER NOT NULL,
                embedding     BLOB NOT NULL
            );
            CREATE TABLE context_dirty (
                path       TEXT    NOT NULL,
                collection TEXT    NOT NULL DEFAULT 'code',
                marked_at  INTEGER NOT NULL,
                seq        INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (path, collection)
            ) WITHOUT ROWID;

            INSERT INTO files (id, path, hash, indexed_at, collection)
                VALUES (7, 'CommonModules/A/Ext/Module.bsl', x'0a', 100, 'code'),
                       (9, 'CommonModules/B/Ext/Module.bsl', x'0b', 100, 'code');
            INSERT INTO chunks (id, file_id, kind, symbol_name, line_start, line_end, text, embedding)
                VALUES (21, 7, 'procedure', 'ПерваяПроцедура', 1, 2, 'Процедура ПерваяПроцедура()', x'cafe'),
                       (23, 9, 'procedure', 'ВтораяПроцедура', 1, 2, 'Процедура ВтораяПроцедура()', x'beef');
            INSERT INTO chunks_fts (rowid, symbol_name, text)
                VALUES (21, 'ПерваяПроцедура', 'Процедура ПерваяПроцедура()'),
                       (23, 'ВтораяПроцедура', 'Процедура ВтораяПроцедура()');

            INSERT INTO baseline_manifest_files (collection, path, file_fingerprint)
                VALUES ('code', 'CommonModules/A/Ext/Module.bsl', 'fp-a');
            INSERT INTO overlay_tombstones (path, collection, deleted_at)
                VALUES ('CommonModules/C/Ext/Module.bsl', 'code', '2026-01-01');
            INSERT INTO overlay_files (id, path, hash, indexed_at, collection)
                VALUES (31, 'CommonModules/D/Ext/Module.bsl', x'0d', 100, 'code');
            INSERT INTO overlay_chunks (id, file_id, kind, symbol_name, line_start, line_end, text)
                VALUES (41, 31, 'procedure', 'ЧетвёртаяПроцедура', 1, 2, 'Процедура ЧетвёртаяПроцедура()');
            INSERT INTO overlay_fingerprint_cache
                    (path, collection, file_size, file_mtime_secs, file_mtime_nanos,
                     content_fingerprint, manifest_snapshot_id)
                VALUES ('CommonModules/A/Ext/Module.bsl', 'code', 10, 1, 2, 'fp-a', 'snap-1');
            INSERT INTO context_dirty (path, collection, marked_at, seq)
                VALUES ('CommonModules/B/Ext/Module.bsl', 'code', 5, 3);
            ",
        )
        .unwrap();
    }

    /// Rows of a table, as `(root_id, path)` pairs, ordered.
    fn keys_of(store: &Store, table: &str) -> Vec<(String, String)> {
        let mut stmt = store
            .conn
            .prepare(&format!("SELECT root_id, path FROM {table} ORDER BY path"))
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    #[test]
    fn a_store_written_before_root_ids_keeps_every_row_and_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);

        let store = Store::open(&path).unwrap();

        // Ids, not counts: a rebuild that renumbered rows would orphan every
        // child row while leaving the counts untouched.
        assert_eq!(
            keys_of(&store, "files"),
            vec![
                (String::new(), "CommonModules/A/Ext/Module.bsl".to_owned()),
                (String::new(), "CommonModules/B/Ext/Module.bsl".to_owned()),
            ],
            "configuration rows keep their meaning under the reserved empty root id"
        );
        let joined: Vec<(i64, String)> = store
            .conn
            .prepare(
                "SELECT c.id, f.path FROM chunks c JOIN files f ON f.id = c.file_id ORDER BY c.id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            joined,
            vec![
                (21, "CommonModules/A/Ext/Module.bsl".to_owned()),
                (23, "CommonModules/B/Ext/Module.bsl".to_owned()),
            ],
            "every chunk still finds its file"
        );
        let overlay_joined: Vec<(i64, String)> = store
            .conn
            .prepare(
                "SELECT c.id, f.path FROM overlay_chunks c
                 JOIN overlay_files f ON f.id = c.file_id ORDER BY c.id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            overlay_joined,
            vec![(41, "CommonModules/D/Ext/Module.bsl".to_owned())],
            "overlay chunks still find their file too — the overlay parent is rebuilt as well"
        );

        assert_eq!(store.embedding_generation().unwrap(), 17, "the vector generation is preserved");
    }

    #[test]
    fn checkpointed_root_migration_rolls_back_at_64_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);
        let conn = Connection::open(&path).unwrap();
        for id in 100..165 {
            conn.execute(
                "INSERT INTO files (id, path, hash, indexed_at, collection)
                 VALUES (?1, ?2, x'01', 100, 'code')",
                params![id, format!("Bulk/{id}.bsl")],
            )
            .unwrap();
        }
        drop(conn);

        let store = Store::prepare_open(&path).unwrap();
        assert!(store.finish_open_checkpointed(&mut || ControlFlow::Break(())).unwrap().is_break());
        assert!(Store::column_names(&store.conn, "files").iter().all(|name| name != "root_id"));
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            67
        );

        assert!(store
            .finish_open_checkpointed(&mut || ControlFlow::Continue(()))
            .unwrap()
            .is_continue());
        assert!(Store::column_names(&store.conn, "files").iter().any(|name| name == "root_id"));
    }

    #[test]
    fn checkpointed_fts_rebuild_rolls_back_and_retries() {
        let mut store = Store::in_memory().unwrap();
        for index in 0..=crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    &format!("M{index}.bsl"),
                    b"hash",
                    &[sample_chunk(&format!("P{index}"))],
                    None,
                )
                .unwrap();
        }
        store.conn.execute("DELETE FROM chunks_fts", []).unwrap();
        let mut checkpoints = 0;
        assert!(store
            .rebuild_fts_checkpointed(&mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap()
            .is_break());
        assert_eq!(store.fts_count().unwrap(), 0);
        assert!(store
            .rebuild_fts_checkpointed(&mut || ControlFlow::Continue(()))
            .unwrap()
            .is_continue());
        assert_eq!(store.fts_count().unwrap(), crate::engine::WORKSPACE_APPLY_BATCH_ROWS + 1);
    }

    #[test]
    fn checkpointed_workspace_clear_is_atomic() {
        let mut store = Store::in_memory().unwrap();
        for index in 0..=crate::engine::WORKSPACE_APPLY_BATCH_ROWS {
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    &format!("M{index}.bsl"),
                    b"hash",
                    &[sample_chunk(&format!("P{index}"))],
                    None,
                )
                .unwrap();
        }
        let mut checkpoints = 0;
        assert!(store
            .clear_workspace_overlay_checkpointed("code", &mut || {
                checkpoints += 1;
                if checkpoints == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap()
            .is_break());
        assert_eq!(store.file_count().unwrap(), crate::engine::WORKSPACE_APPLY_BATCH_ROWS + 1);
        assert!(store
            .clear_workspace_overlay_checkpointed("code", &mut || ControlFlow::Continue(()))
            .unwrap()
            .is_continue());
        assert_eq!(store.file_count().unwrap(), 0);
        assert_eq!(store.fts_count().unwrap(), 0);
    }

    /// Rebuilding a parent table drops the foreign keys and triggers that hang
    /// off it. Row counts cannot see that: the damage only shows on the next
    /// delete, when orphans are left behind and the vector sidecar is never
    /// invalidated.
    #[test]
    fn the_migrated_store_keeps_its_cascades_and_generation_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);

        let store = Store::open(&path).unwrap();
        let generation_before = store.embedding_generation().unwrap();

        store.conn.execute("DELETE FROM files WHERE id = 7", []).unwrap();

        let orphans: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks WHERE file_id = 7", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "deleting a file must still cascade onto its chunks");
        assert!(
            store.embedding_generation().unwrap() > generation_before,
            "deleting a file must still bump the vector generation"
        );

        store.conn.execute("DELETE FROM overlay_files WHERE id = 31", []).unwrap();
        let overlay_orphans: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM overlay_chunks WHERE file_id = 31", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(overlay_orphans, 0, "the overlay cascade survives the rebuild too");
    }

    /// Files and chunks surviving is not enough: search answers out of the FTS
    /// projection, and its auto-rebuild only fires on a completely empty index.
    /// A partially lost projection would leave every other invariant green and
    /// silently shrink the warm store's results.
    #[test]
    fn documents_indexed_before_the_migration_are_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);

        let store = Store::open(&path).unwrap();

        let hits = store.text_search("ПерваяПроцедура", 10, Some("code")).unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(),
            vec![21],
            "a document stored before the migration is still searchable"
        );
    }

    /// The point of the whole migration: the same relative path may now exist
    /// under several roots. Checked on every rebuilt table, because a leftover
    /// old constraint on any one of them blocks exactly one feature each.
    #[test]
    fn the_same_path_may_now_exist_under_two_roots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);
        let store = Store::open(&path).unwrap();
        let taken = "CommonModules/A/Ext/Module.bsl";

        store
            .conn
            .execute(
                "INSERT INTO files (root_id, path, hash, indexed_at, collection)
                 VALUES ('cfe/one', ?1, x'0e', 100, 'code')",
                params![taken],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO baseline_manifest_files (root_id, collection, path, file_fingerprint)
                 VALUES ('cfe/one', 'code', ?1, 'fp')",
                params![taken],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO overlay_tombstones (root_id, path, collection, deleted_at)
                 VALUES ('cfe/one', 'CommonModules/C/Ext/Module.bsl', 'code', '2026-01-01')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO overlay_files (root_id, path, hash, indexed_at, collection)
                 VALUES ('cfe/one', 'CommonModules/D/Ext/Module.bsl', x'0f', 100, 'code')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO overlay_fingerprint_cache
                        (root_id, path, collection, file_size, file_mtime_secs, file_mtime_nanos,
                         content_fingerprint, manifest_snapshot_id)
                 VALUES ('cfe/one', ?1, 'code', 10, 1, 2, 'fp', 'snap-1')",
                params![taken],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO context_dirty (root_id, path, collection, marked_at, seq)
                 VALUES ('cfe/one', 'CommonModules/B/Ext/Module.bsl', 'code', 5, 3)",
                [],
            )
            .unwrap();

        // Counted per path, not per table: the fixture does not hold the same
        // number of rows everywhere, and a total would pass on a table that
        // silently replaced its row instead of adding one.
        for (table, duplicated) in [
            ("files", taken),
            ("baseline_manifest_files", taken),
            ("overlay_tombstones", "CommonModules/C/Ext/Module.bsl"),
            ("overlay_files", "CommonModules/D/Ext/Module.bsl"),
            ("overlay_fingerprint_cache", taken),
            ("context_dirty", "CommonModules/B/Ext/Module.bsl"),
        ] {
            let rows: i64 = store
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE path = ?1"),
                    params![duplicated],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(rows, 2, "{table} must hold one row per root for the same path");
        }
    }

    /// A fresh database takes the new shape through the ordinary create path,
    /// which the migration never touches: "the column is missing" is true of an
    /// absent table too, so the two cases have to be checked apart.
    #[test]
    fn a_fresh_store_is_created_with_root_ids() {
        let store = Store::in_memory().unwrap();

        for table in ROOT_KEYED_TABLES.iter().map(|table| table.name) {
            let has_root_id: bool = store
                .conn
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = 'root_id'"
                ))
                .unwrap()
                .query_row([], |_| Ok(true))
                .optional()
                .unwrap()
                .unwrap_or(false);
            assert!(has_root_id, "{table} must be created with a root_id");
        }
    }

    /// The upgrade must not go through the wipe path: on a real workspace that
    /// is hours of re-indexing and re-embedding, and the whole point of the
    /// default value is that rows are kept.
    #[test]
    fn the_upgrade_never_wipes_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.db");
        write_pre_root_id_store(&path);

        let store = Store::open(&path).unwrap();

        assert_eq!(store.file_count().unwrap(), 2);
        assert_eq!(store.chunk_count().unwrap(), 2);
        // Stamped forward, so a binary of the previous release rebuilds its
        // derived cache instead of failing on an upsert whose conflict target
        // no longer exists.
        assert_eq!(Store::stored_schema_version(&store.conn).unwrap(), Some(SCHEMA_VERSION));
    }

    fn sample_chunk(name: &str) -> Chunk {
        Chunk {
            kind: ChunkKind::Procedure,
            name: name.to_owned(),
            is_export: true,
            annotations: vec!["НаСервере".to_owned()],
            line_start: 0,
            line_end: 5,
            text: format!("Процедура {name}()\nКонецПроцедуры"),
        }
    }

    #[test]
    fn create_and_query() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test content");

        let file_id = store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("Тест")],
                None,
            )
            .unwrap();

        assert!(file_id > 0);
        assert_eq!(store.file_count().unwrap(), 1);
        assert_eq!(store.chunk_count().unwrap(), 1);
    }

    #[test]
    fn context_dirty_marks_are_recorded_cleared_and_do_not_touch_the_vector_generation() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"body");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "Owned.bsl",
                hash.as_bytes(),
                &[sample_chunk("П")],
                None,
            )
            .unwrap();
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "Other.bsl",
                hash.as_bytes(),
                &[sample_chunk("Д")],
                None,
            )
            .unwrap();

        let generation_before = store.embedding_generation().unwrap();

        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();
        assert_eq!(
            store.context_dirty_paths("code").unwrap(),
            HashSet::from([FileKey::configuration("Owned.bsl")]),
        );
        // The side table must not fire the chunk triggers, or every metadata edit would
        // invalidate the persisted vector index for a mark that changes no embedding.
        assert_eq!(
            store.embedding_generation().unwrap(),
            generation_before,
            "marking context-dirty leaves the vector generation untouched",
        );

        // A configuration-root edit marks every indexed file, all under ONE seq.
        let (marked, batch_seq) = store.mark_collection_context_dirty("code").unwrap();
        assert_eq!(marked, 2);
        assert!(batch_seq > 0, "the batch reports the shared mark seq it stamped");
        assert_eq!(store.context_dirty_paths("code").unwrap().len(), 2);

        store.clear_context_dirty("code", CONFIGURATION_ROOT_ID, "Owned.bsl").unwrap();
        assert_eq!(
            store.context_dirty_paths("code").unwrap(),
            HashSet::from([FileKey::configuration("Other.bsl")]),
        );
    }

    /// A topology re-render runs against a build that reflects the workspace up to its bound, so
    /// its batch is stamped AT that bound — and a file whose own drift landed later keeps the
    /// higher stamp it already carries. Stamping the batch above everything (a fresh seq) would
    /// overwrite that file's mark and then sweep it into the same bounded clear: the file would
    /// be re-rendered against a graph predating its change, and the mark that would have fixed
    /// it later would be gone.
    #[test]
    fn a_topology_batch_leaves_a_fresher_drift_mark_alone() {
        let mut store = Store::in_memory().unwrap();
        for path in ["Stable.bsl", "Drifted.bsl"] {
            store
                .reindex_file(CONFIGURATION_ROOT_ID, path, b"h", &[sample_chunk("П")], None)
                .unwrap();
        }

        // A build captured its bound here; then one file drifted, stamped ABOVE it.
        let build_start_seq = store.mark_seq_handle().load(Ordering::SeqCst);
        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Drifted.bsl").unwrap();

        let marked = store.mark_collection_context_dirty_at("code", build_start_seq).unwrap();
        assert_eq!(marked, 1, "only the file with no fresher mark is stamped by the batch");

        let batch = store.context_dirty_paths_bounded("code", build_start_seq).unwrap();
        assert!(
            batch.contains(&FileKey::configuration("Stable.bsl")),
            "the batch re-renders what the build reflects"
        );
        assert!(
            !batch.contains(&FileKey::configuration("Drifted.bsl")),
            "a drift the build does not reflect is not re-rendered against it",
        );

        store
            .clear_context_dirty_bounded(
                "code",
                CONFIGURATION_ROOT_ID,
                "Stable.bsl",
                build_start_seq,
            )
            .unwrap();
        store
            .clear_context_dirty_bounded(
                "code",
                CONFIGURATION_ROOT_ID,
                "Drifted.bsl",
                build_start_seq,
            )
            .unwrap();
        assert_eq!(
            store.context_dirty_paths("code").unwrap(),
            HashSet::from([FileKey::configuration("Drifted.bsl")]),
            "and its mark survives for the publish that will reflect it",
        );
    }

    /// The bounded consume is stamped per mark: a re-mark that lands after a build captured
    /// its start-seq bumps the row's `seq` above the bound, so the build's bounded clear
    /// skips it and the newer mark survives (a lost update at row granularity is prevented).
    /// Reverting the `seq <= ?` predicate on the clear (an unconditional delete) removes the
    /// re-stamped row and this fails.
    #[test]
    fn a_remark_above_the_build_start_seq_survives_the_bounded_clear() {
        let store = Store::in_memory().unwrap();

        // Mark P (seq 1), then a build captures the current start-seq (1) and reads the set.
        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "P.bsl").unwrap();
        let build_start_seq = store.mark_seq_handle().load(Ordering::SeqCst);
        assert_eq!(build_start_seq, 1);
        let read_set = store.context_dirty_paths_bounded("code", build_start_seq).unwrap();
        assert!(
            read_set.contains(&FileKey::configuration("P.bsl")),
            "P is in the build's read set"
        );

        // A fresher drift re-marks P (seq 2) while the build is processing its read set.
        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "P.bsl").unwrap();
        assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 2);

        // The build clears P bounded by ITS start-seq (1); P's row now sits at seq 2, so the
        // newer mark is not lost.
        store
            .clear_context_dirty_bounded("code", CONFIGURATION_ROOT_ID, "P.bsl", build_start_seq)
            .unwrap();
        assert!(
            store.context_dirty_paths("code").unwrap().contains(&FileKey::configuration("P.bsl")),
            "the re-mark stamped after the build started survives the bounded clear",
        );

        // The next build (start-seq 2) does consume it.
        store.clear_context_dirty_bounded("code", CONFIGURATION_ROOT_ID, "P.bsl", 2).unwrap();
        assert!(
            !store.context_dirty_paths("code").unwrap().contains(&FileKey::configuration("P.bsl")),
            "a build whose start-seq covers the re-mark clears it",
        );
    }

    /// The mark-seq counter is monotonic and survives a clear: after a marked-then-cleared
    /// row the next mark still gets a strictly higher seq (an atomic counter, not `MAX+1`
    /// over live rows), and a reopen seeds the counter above any persisted row. Without the
    /// non-resetting counter, a cleared table would recycle low seqs and an old build's
    /// bound could consume a brand-new mark.
    #[test]
    fn mark_seq_is_monotonic_across_clears_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("search.db");
        {
            let store = Store::open(&db).unwrap();
            store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "A.bsl").unwrap(); // seq 1
            store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "B.bsl").unwrap(); // seq 2
            assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 2);
            // Clear A: the live MAX(seq) drops to 1, but the counter must not rewind.
            store.clear_context_dirty("code", CONFIGURATION_ROOT_ID, "A.bsl").unwrap();
            store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "C.bsl").unwrap(); // seq 3, never reused
            assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 3);
        }
        // Reopen: B (seq 2) and C (seq 3) persist; the counter seeds above the max.
        let store = Store::open(&db).unwrap();
        assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 3);
        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "D.bsl").unwrap(); // seq 4
        assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 4);
    }

    /// Two stores over ONE database file — the shape two daemon generations take while they
    /// overlap on a workspace — draw from the same sequence: every stamp is unique and each
    /// store's next allocation lands above whatever the other just wrote. With a per-process
    /// counter both stores seed from the same `MAX(seq)` and re-issue the same numbers, and
    /// a bounded clear then consumes the other writer's mark against a graph predating it.
    #[test]
    fn two_stores_on_one_file_never_duplicate_or_lower_a_mark_seq() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("search.db");
        let first = Store::open(&db).unwrap();
        let second = Store::open(&db).unwrap();

        let mut seqs = Vec::new();
        for round in 0..3 {
            first
                .mark_context_dirty("code", CONFIGURATION_ROOT_ID, &format!("A{round}.bsl"))
                .unwrap();
            seqs.push(first.mark_seq_handle().load(Ordering::SeqCst));
            second
                .mark_context_dirty("code", CONFIGURATION_ROOT_ID, &format!("B{round}.bsl"))
                .unwrap();
            seqs.push(second.mark_seq_handle().load(Ordering::SeqCst));
        }

        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6], "the two stores share one rising sequence");
        let persisted: Vec<i64> = {
            let mut stmt =
                first.conn.prepare("SELECT seq FROM context_dirty ORDER BY seq").unwrap();
            stmt.query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(persisted, vec![1, 2, 3, 4, 5, 6], "no stamp is duplicated on disk");
    }

    /// A store's own high-water only tracks what IT allocated, so a bound taken from it would
    /// sit below another writer's marks forever — and a mark no bound ever covers is a mark no
    /// publish ever consumes. The persisted counter is what a build reads instead, and it sees
    /// every writer's stamps.
    #[test]
    fn the_persisted_mark_seq_sees_marks_this_store_did_not_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("search.db");
        let mine = Store::open(&db).unwrap();
        let other = Store::open(&db).unwrap();

        mine.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Mine.bsl").unwrap(); // seq 1
        other.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "Theirs.bsl").unwrap(); // seq 2

        assert_eq!(
            mine.mark_seq_handle().load(Ordering::SeqCst),
            1,
            "the local mirror never saw the other writer's stamp",
        );
        assert_eq!(
            Store::persisted_mark_seq(&db).unwrap(),
            2,
            "but the persisted counter covers it, so a build's bound consumes that mark",
        );
    }

    /// A database written before the counter existed (rows carrying seqs, no `mark_seq` row)
    /// must not restart the sequence at 1 and re-issue stamps its surviving rows already
    /// hold — the open seeds the counter from those rows.
    #[test]
    fn opening_a_pre_counter_database_seeds_above_surviving_marks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("search.db");
        {
            let store = Store::open(&db).unwrap();
            store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "A.bsl").unwrap(); // seq 1
            store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "B.bsl").unwrap(); // seq 2
            store.conn.execute("DELETE FROM meta WHERE key = 'mark_seq'", []).unwrap();
        }

        let store = Store::open(&db).unwrap();
        assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 2);
        store.mark_context_dirty("code", CONFIGURATION_ROOT_ID, "C.bsl").unwrap();
        assert_eq!(store.mark_seq_handle().load(Ordering::SeqCst), 3);
    }

    #[test]
    fn context_write_does_not_bump_generation_but_clearing_embedding_does() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"body");
        let vec = vec![1.0f32, 0.0, 0.0];
        store
            .reindex_file_with_context(
                CONFIGURATION_ROOT_ID,
                "F.bsl",
                hash.as_bytes(),
                &[sample_chunk("П")],
                Some(std::slice::from_ref(&vec)),
                Some(&[Some("старый контекст".to_owned())]),
            )
            .unwrap();
        let chunk_id = store.chunk_ids_for_file("code", CONFIGURATION_ROOT_ID, "F.bsl").unwrap()[0];

        let gen0 = store.embedding_generation().unwrap();
        // Rewriting only graph_context must NOT bump the vector generation — the
        // `chunks_gen_upd` trigger fires only `AFTER UPDATE OF embedding`.
        store.set_chunk_graph_context(chunk_id, Some("новый контекст")).unwrap();
        assert_eq!(
            store.embedding_generation().unwrap(),
            gen0,
            "a graph_context-only write leaves the vector generation untouched",
        );
        // Clearing the embedding DOES bump it — the persisted vector sidecar must
        // invalidate when a vector is dropped for re-embed.
        store.clear_chunk_embedding(chunk_id).unwrap();
        assert!(
            store.embedding_generation().unwrap() > gen0,
            "clearing an embedding bumps the vector generation",
        );
    }

    #[test]
    fn remove_file_is_atomic_all_or_nothing() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"body");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "t.bsl",
                hash.as_bytes(),
                &[sample_chunk("П")],
                None,
            )
            .unwrap();
        assert_eq!(store.fts_count().unwrap(), 1);

        // Force the second statement (the `files` delete) to abort, so the whole
        // transaction must roll back. Without the transaction the first statement (the
        // FTS delete) would have committed on its own and left an orphaned state.
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER block_files_delete BEFORE DELETE ON files
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();

        assert!(
            store.remove_file(CONFIGURATION_ROOT_ID, "t.bsl", "code").is_err(),
            "the aborted delete surfaces an error"
        );
        // The FTS delete rolled back with the aborted files delete — nothing was lost.
        assert_eq!(store.fts_count().unwrap(), 1, "the FTS rows survive a rolled-back removal");
        assert_eq!(store.file_count().unwrap(), 1, "the files row survives too");

        store.conn.execute_batch("DROP TRIGGER block_files_delete;").unwrap();
        // With the block lifted the removal now succeeds and clears both together.
        store.remove_file(CONFIGURATION_ROOT_ID, "t.bsl", "code").unwrap();
        assert_eq!(store.fts_count().unwrap(), 0);
        assert_eq!(store.file_count().unwrap(), 0);
    }

    #[test]
    fn remove_file_is_scoped_to_the_callers_collection() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"body");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "only.bsl",
                hash.as_bytes(),
                &[sample_chunk("П")],
                None,
            )
            .unwrap();
        assert_eq!(store.file_count().unwrap(), 1);

        // A removal scoped to a different collection must not touch this file.
        store.remove_file(CONFIGURATION_ROOT_ID, "only.bsl", "platform").unwrap();
        assert_eq!(store.file_count().unwrap(), 1, "a mismatched collection removes nothing");

        // The correctly-scoped removal clears it.
        store.remove_file(CONFIGURATION_ROOT_ID, "only.bsl", "code").unwrap();
        assert_eq!(store.file_count().unwrap(), 0);
    }

    #[test]
    fn embedding_generation_advances_on_indexed_set_changes() {
        let mut store = Store::in_memory().unwrap();
        // Pin the conservative pragma: the cascade bump must hold even with recursive triggers off.
        store.conn.execute_batch("PRAGMA recursive_triggers = OFF;").unwrap();

        let g0 = store.embedding_generation().unwrap();

        // Insert two chunks -> two INSERT trigger firings.
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "m.bsl",
                b"h0",
                &[sample_chunk("Один"), sample_chunk("Два")],
                None,
            )
            .unwrap();
        let g_after_insert = store.embedding_generation().unwrap();
        assert!(g_after_insert > g0, "insert must advance the generation");

        // In-place embedding update -> UPDATE OF embedding trigger.
        let id: i64 =
            store.conn.query_row("SELECT id FROM chunks LIMIT 1", [], |r| r.get(0)).unwrap();
        store.set_chunk_embedding(id, &[0.1_f32, 0.2, 0.3]).unwrap();
        let g_after_update = store.embedding_generation().unwrap();
        assert!(g_after_update > g_after_insert, "embedding update must advance the generation");

        // A non-embedding column update must NOT advance it (the index is unaffected).
        store
            .conn
            .execute("UPDATE chunks SET line_end = line_end + 1 WHERE id = ?1", params![id])
            .unwrap();
        assert_eq!(
            store.embedding_generation().unwrap(),
            g_after_update,
            "a non-embedding update must not advance the generation"
        );

        // File removal cascades to chunks; `files_gen_del` guarantees an advance regardless of
        // whether the cascade fires the chunk delete trigger.
        store.remove_file(CONFIGURATION_ROOT_ID, "m.bsl", "code").unwrap();
        assert!(
            store.embedding_generation().unwrap() > g_after_update,
            "file removal (cascade delete) must advance the generation"
        );
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn structural_wipe_removes_persisted_vector_artifacts() {
        use crate::index::VectorIndex;

        const DIM: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");

        // Seed one embedded chunk and persist a vector index + sidecar beside the DB.
        {
            let mut store = Store::open(&db_path).unwrap();
            let emb = vec![0.1_f32, 0.2, 0.3, 0.4];
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "f.bsl",
                    b"h0",
                    &[sample_chunk("П")],
                    Some(&[emb]),
                )
                .unwrap();
            let (generation, data) = store.load_all_embeddings_with_generation(DIM).unwrap();
            let index = VectorIndex::build(DIM, &data).unwrap();
            let key = crate::vector_persist::PersistKey {
                db_path: store.db_path(),
                model_id: "test-model",
                dim: DIM,
            };
            crate::vector_persist::persist(&index, &key, generation).unwrap();

            // Simulate a future structural-schema change: stamp a different version so the next
            // open wipes the derived cache (which drops `meta` and resets the generation counter).
            store
                .conn
                .execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'", [])
                .unwrap();
        }

        let usearch = dir.path().join("search.db.usearch");
        let sidecar = dir.path().join("search.db.usearch.json");
        assert!(usearch.exists() && sidecar.exists(), "artifacts persisted before the wipe");

        // Reopening sees the version mismatch, wipes the tables, and must delete the stale
        // artifacts so the reset generation (0) can never match the old sidecar.
        let store = Store::open(&db_path).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0, "wipe emptied the chunks");
        assert!(!usearch.exists(), "stale index file removed by the wipe");
        assert!(!sidecar.exists(), "stale sidecar removed by the wipe");
        let key = crate::vector_persist::PersistKey {
            db_path: store.db_path(),
            model_id: "test-model",
            dim: DIM,
        };
        assert!(
            crate::vector_persist::try_load(&store, &key).is_none(),
            "no stale index is served over the emptied database"
        );
    }

    #[test]
    fn structural_wipe_aborts_when_stale_sidecar_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");

        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "f.bsl",
                    b"h0",
                    &[sample_chunk("П")],
                    Some(&[vec![0.1, 0.2, 0.3, 0.4]]),
                )
                .unwrap();
            store
                .conn
                .execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'", [])
                .unwrap();
        }

        // Make the sidecar path un-removable as a plain file by turning it into a (non-empty)
        // directory, so `fs::remove_file` fails with a non-`NotFound` error. The wipe must abort
        // rather than empty the DB while a loadable sidecar survives.
        let sidecar = dir.path().join("search.db.usearch.json");
        std::fs::create_dir_all(sidecar.join("blocker")).unwrap();

        assert!(
            Store::open(&db_path).is_err(),
            "a structural wipe must fail closed when the stale sidecar cannot be removed"
        );

        // Once the obstruction is gone, the wipe proceeds and the DB is reconciled.
        std::fs::remove_dir_all(&sidecar).unwrap();
        let store = Store::open(&db_path).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn missing_generation_row_on_current_schema_invalidates_artifacts() {
        use crate::index::VectorIndex;

        const DIM: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("search.db");

        {
            let mut store = Store::open(&db_path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "f.bsl",
                    b"h0",
                    &[sample_chunk("П")],
                    Some(&[vec![0.1, 0.2, 0.3, 0.4]]),
                )
                .unwrap();
            let (generation, data) = store.load_all_embeddings_with_generation(DIM).unwrap();
            let index = VectorIndex::build(DIM, &data).unwrap();
            let key = crate::vector_persist::PersistKey {
                db_path: store.db_path(),
                model_id: "test-model",
                dim: DIM,
            };
            crate::vector_persist::persist(&index, &key, generation).unwrap();
            assert!(crate::vector_persist::try_load(&store, &key).is_some());

            // Corruption: the counter row vanishes while the schema version stays current, so no
            // structural wipe runs. The reset counter must not silently come back as 0 and validate
            // the old sidecar (which would serve a possibly-stale index).
            store.conn.execute("DELETE FROM meta WHERE key = 'embedding_generation'", []).unwrap();
        }

        let sidecar = dir.path().join("search.db.usearch.json");
        assert!(sidecar.exists(), "sidecar present before the corrupt reopen");

        let store = Store::open(&db_path).unwrap();
        assert_eq!(store.embedding_generation().unwrap(), 0, "counter reseeded");
        assert!(!sidecar.exists(), "stale sidecar removed when the counter had to be recreated");
        let key = crate::vector_persist::PersistKey {
            db_path: store.db_path(),
            model_id: "test-model",
            dim: DIM,
        };
        assert!(
            crate::vector_persist::try_load(&store, &key).is_none(),
            "no stale index is served after the counter was recreated"
        );
    }

    #[test]
    fn reindex_replaces_chunks() {
        let mut store = Store::in_memory().unwrap();
        let hash1 = blake3::hash(b"v1");
        let hash2 = blake3::hash(b"v2");

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "mod.bsl",
                hash1.as_bytes(),
                &[sample_chunk("Первая"), sample_chunk("Вторая")],
                None,
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "mod.bsl",
                hash2.as_bytes(),
                &[sample_chunk("Новая")],
                None,
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
        assert_eq!(store.file_count().unwrap(), 1);
    }

    #[test]
    fn file_hash_lookup() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"content");

        assert!(store.file_hash(CONFIGURATION_ROOT_ID, "test.bsl").unwrap().is_none());

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("Тест")],
                None,
            )
            .unwrap();

        let stored = store.file_hash(CONFIGURATION_ROOT_ID, "test.bsl").unwrap().unwrap();
        assert_eq!(stored, hash.as_bytes());
    }

    #[test]
    fn embeddings_roundtrip() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        let embedding = vec![0.1f32, 0.2, 0.3, 0.4];

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("Тест")],
                Some(std::slice::from_ref(&embedding)),
            )
            .unwrap();

        let loaded = store.load_all_embeddings(4).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, embedding);
    }

    #[test]
    fn chunk_by_id_returns_metadata() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "path/to/module.bsl",
                hash.as_bytes(),
                &[sample_chunk("Метод")],
                None,
            )
            .unwrap();

        assert_eq!(store.chunk_count().unwrap(), 1);

        let chunk_id: i64 =
            store.conn.query_row("SELECT id FROM chunks LIMIT 1", [], |r| r.get(0)).unwrap();

        let info = store.chunk_by_id(chunk_id).unwrap().unwrap();
        assert_eq!(info.file_path, "path/to/module.bsl");
        assert_eq!(info.symbol_name, "Метод");
        assert_eq!(info.kind, "procedure");
        assert!(info.is_export);
        assert_eq!(info.annotations.as_deref(), Some("НаСервере"));
    }

    #[test]
    fn chunks_by_ids_matches_individual_lookups() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "path/to/module.bsl",
                hash.as_bytes(),
                &[sample_chunk("Альфа"), sample_chunk("Бета"), sample_chunk("Гамма")],
                None,
            )
            .unwrap();

        let all_ids: Vec<i64> = {
            let mut stmt = store.conn.prepare("SELECT id FROM chunks ORDER BY id").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(all_ids.len(), 3);

        // Empty ids → empty map, no query.
        assert!(store.chunks_by_ids(&[]).unwrap().is_empty());

        // A subset, requested out of order, must equal the per-id lookups.
        let subset = vec![all_ids[2], all_ids[0]];
        let batch = store.chunks_by_ids(&subset).unwrap();
        assert_eq!(batch.len(), 2);
        for id in &subset {
            let one = store.chunk_by_id(*id).unwrap().unwrap();
            let many = batch.get(id).unwrap();
            assert_eq!(many.file_path, one.file_path);
            assert_eq!(many.symbol_name, one.symbol_name);
            assert_eq!(many.kind, one.kind);
            assert_eq!(many.collection, one.collection);
            assert_eq!(many.line_start, one.line_start);
            assert_eq!(many.line_end, one.line_end);
            assert_eq!(many.text, one.text);
            assert_eq!(many.annotations, one.annotations);
            assert_eq!(many.is_export, one.is_export);
        }
        // The id not requested is absent.
        assert!(!batch.contains_key(&all_ids[1]));

        // A missing id is simply absent from the map (no error).
        let mut missing = all_ids.clone();
        missing.push(999_999);
        let full = store.chunks_by_ids(&missing).unwrap();
        assert_eq!(full.len(), 3);
        assert!(!full.contains_key(&999_999));
    }

    #[test]
    fn remove_file_cascades() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("А"), sample_chunk("Б")],
                None,
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);

        store.remove_file(CONFIGURATION_ROOT_ID, "test.bsl", "code").unwrap();
        assert_eq!(store.file_count().unwrap(), 0);
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn fts_search_by_symbol_name() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("ОбработкаПроведения"), sample_chunk("ПриСозданииНаСервере")],
                None,
            )
            .unwrap();

        let results = store.text_search("ОбработкаПроведения", 10, None).unwrap();
        assert_eq!(results.len(), 1);

        let info = store.chunk_by_id(results[0].chunk_id).unwrap().unwrap();
        assert_eq!(info.symbol_name, "ОбработкаПроведения");
    }

    #[test]
    fn fts_search_by_text_content() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");

        let mut chunk = sample_chunk("Тест");
        chunk.text =
            "Процедура Тест()\n    СообщитьПользователю(\"Привет\");\nКонецПроцедуры".to_owned();

        store
            .reindex_file(CONFIGURATION_ROOT_ID, "test.bsl", hash.as_bytes(), &[chunk], None)
            .unwrap();

        let results = store.text_search("СообщитьПользователю", 10, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fts_multi_term_query_matches_any_term() {
        let mut store = Store::in_memory().unwrap();

        // One chunk carries only the identifier; the other carries the identifier and the extra
        // words. A multi-word query used to be wrapped as one phrase, matching neither; with the
        // OR fix both surface.
        let mut only_id = sample_chunk("Прочее");
        only_id.text = "Процедура Прочее()\n    ВызватьHTTПМетод();\nКонецПроцедуры".to_owned();
        let mut full = sample_chunk("Отправщик");
        full.text =
            "Процедура Отправщик()\n    ВызватьHTTПМетод(); // отправка запроса\nКонецПроцедуры"
                .to_owned();

        store.reindex_file(CONFIGURATION_ROOT_ID, "a.bsl", b"h0", &[only_id], None).unwrap();
        store.reindex_file(CONFIGURATION_ROOT_ID, "b.bsl", b"h1", &[full], None).unwrap();

        let results = store.text_search("ВызватьHTTПМетод отправка запроса", 10, None).unwrap();
        assert_eq!(results.len(), 2, "OR semantics must surface a chunk matching any term");
    }

    #[test]
    fn fts_dotted_call_term_matches_indexed_code() {
        let mut store = Store::in_memory().unwrap();
        let mut chunk = sample_chunk("Отправщик");
        chunk.text =
            "Процедура Отправщик()\n    КоннекторHTTP.ВызватьМетод();\nКонецПроцедуры".to_owned();
        store.reindex_file(CONFIGURATION_ROOT_ID, "a.bsl", b"h0", &[chunk], None).unwrap();

        // The dotted call is one quoted token; unicode61 makes it an adjacency phrase that still
        // matches the same dotted call in the body.
        let results = store.text_search("КоннекторHTTP.ВызватьМетод()", 10, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn fts_punctuation_only_query_is_empty_not_error() {
        let mut store = Store::in_memory().unwrap();
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "a.bsl", b"h0", &[sample_chunk("Метод")], None)
            .unwrap();
        // No usable term -> empty result, never an FTS5 syntax error.
        assert!(store.text_search("()", 10, None).unwrap().is_empty());
        assert!(store.text_search("   ", 10, None).unwrap().is_empty());
    }

    #[test]
    fn fts_reindex_updates_index() {
        let mut store = Store::in_memory().unwrap();
        let hash1 = blake3::hash(b"v1");
        let hash2 = blake3::hash(b"v2");

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash1.as_bytes(),
                &[sample_chunk("Старая")],
                None,
            )
            .unwrap();
        assert_eq!(store.text_search("Старая", 10, None).unwrap().len(), 1);

        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash2.as_bytes(),
                &[sample_chunk("Новая")],
                None,
            )
            .unwrap();

        assert_eq!(store.text_search("Старая", 10, None).unwrap().len(), 0);
        assert_eq!(store.text_search("Новая", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn fts_remove_file_cleans_index() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"test");
        store
            .reindex_file(
                CONFIGURATION_ROOT_ID,
                "test.bsl",
                hash.as_bytes(),
                &[sample_chunk("Удаляемая")],
                None,
            )
            .unwrap();
        assert_eq!(store.text_search("Удаляемая", 10, None).unwrap().len(), 1);

        store.remove_file(CONFIGURATION_ROOT_ID, "test.bsl", "code").unwrap();
        assert_eq!(store.text_search("Удаляемая", 10, None).unwrap().len(), 0);
    }

    #[test]
    fn load_indexed_documents_filters_by_collection() {
        let mut store = Store::in_memory().unwrap();
        let code = crate::Chunker::chunk("Процедура Код()\nКонецПроцедуры");
        store.reindex_file(CONFIGURATION_ROOT_ID, "A.bsl", b"hash-a", &code, None).unwrap();
        store
            .reindex_documents(
                "platform",
                "platform://docs",
                b"hash-docs",
                &[crate::Document {
                    title: "Строка".to_owned(),
                    body: "Описание".to_owned(),
                    kind: "type".to_owned(),
                }],
                None,
            )
            .unwrap();

        let code_docs = store.load_indexed_documents(Some("code")).unwrap();
        let platform_docs = store.load_indexed_documents(Some("platform")).unwrap();

        assert_eq!(code_docs.len(), 1);
        assert_eq!(code_docs[0].collection, "code");
        assert_eq!(platform_docs.len(), 1);
        assert_eq!(platform_docs[0].collection, "platform");
    }

    #[test]
    fn baseline_manifest_roundtrip() {
        let store = Store::in_memory().unwrap();
        assert!(store.load_baseline_manifest().unwrap().is_none());
        assert!(store.load_baseline_manifest_fingerprints("code").unwrap().is_none());

        let manifest = crate::WorkspaceBaselineManifest {
            snapshot_id: "snap-123".to_owned(),
            snapshot_fingerprint: Some("fp-abc".to_owned()),
            files: vec![
                crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "src/A.bsl".to_owned(),
                    file_fingerprint: "fp-a".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-a".to_owned(),
                },
                crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "src/B.bsl".to_owned(),
                    file_fingerprint: "fp-b".to_owned(),
                    document_count: 2,
                    file_object_id: "obj-b".to_owned(),
                },
            ],
        };
        store.save_baseline_manifest(&manifest).unwrap();

        let record = store.load_baseline_manifest().unwrap().unwrap();
        assert_eq!(record.snapshot_id, "snap-123");
        assert_eq!(record.fingerprint, Some("fp-abc".to_owned()));
        assert_eq!(record.manifest_files, 2);

        let fingerprints = store.load_baseline_manifest_fingerprints("code").unwrap().unwrap();
        assert_eq!(fingerprints.len(), 2);
        assert_eq!(
            fingerprints.get(&FileKey::configuration("src/A.bsl")).map(String::as_str),
            Some("fp-a")
        );
        assert_eq!(
            fingerprints.get(&FileKey::configuration("src/B.bsl")).map(String::as_str),
            Some("fp-b")
        );

        store.clear_baseline_manifest().unwrap();
        assert!(store.load_baseline_manifest().unwrap().is_none());
        assert!(store.load_baseline_manifest_fingerprints("code").unwrap().is_none());
    }

    #[test]
    fn coherent_baseline_manifest_rejects_header_without_matching_file_rows() {
        let store = Store::in_memory().unwrap();
        assert!(store.load_coherent_baseline_manifest().unwrap().is_none());

        let manifest = crate::WorkspaceBaselineManifest {
            snapshot_id: "snap-123".to_owned(),
            snapshot_fingerprint: Some("fp-abc".to_owned()),
            files: vec![
                crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "src/A.bsl".to_owned(),
                    file_fingerprint: "fp-a".to_owned(),
                    document_count: 1,
                    file_object_id: "obj-a".to_owned(),
                },
                crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: "code".to_owned(),
                    path: "src/B.bsl".to_owned(),
                    file_fingerprint: "fp-b".to_owned(),
                    document_count: 2,
                    file_object_id: "obj-b".to_owned(),
                },
            ],
        };
        store.save_baseline_manifest(&manifest).unwrap();

        let record = store.load_coherent_baseline_manifest().unwrap().unwrap();
        assert_eq!(record.snapshot_id, "snap-123");
        assert_eq!(record.manifest_files, 2);

        // A file row lost underneath an intact header (older binaries cleared the two
        // tables non-transactionally) must disqualify the record even though the plain
        // header read still succeeds.
        store
            .conn
            .execute("DELETE FROM baseline_manifest_files WHERE path = 'src/B.bsl'", [])
            .unwrap();
        assert!(store.load_baseline_manifest().unwrap().is_some());
        assert!(store.load_coherent_baseline_manifest().unwrap().is_none());
    }

    #[test]
    fn overlay_tombstone_persistence() {
        let store = Store::in_memory().unwrap();
        assert!(store.overlay_tombstone_paths("code").unwrap().is_empty());

        store.insert_overlay_tombstone(CONFIGURATION_ROOT_ID, "src/A.bsl", "code").unwrap();
        store.insert_overlay_tombstone(CONFIGURATION_ROOT_ID, "src/B.bsl", "code").unwrap();

        let paths = store.overlay_tombstone_paths("code").unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&FileKey::configuration("src/A.bsl")));
        assert!(paths.contains(&FileKey::configuration("src/B.bsl")));

        store.remove_overlay_tombstone(CONFIGURATION_ROOT_ID, "src/A.bsl").unwrap();
        let paths = store.overlay_tombstone_paths("code").unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(&FileKey::configuration("src/B.bsl")));

        store.clear_overlay_tombstones("code").unwrap();
        assert!(store.overlay_tombstone_paths("code").unwrap().is_empty());
    }

    #[test]
    fn overlay_file_with_chunks_roundtrip() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"overlay content");
        let chunks = vec![sample_chunk("OverlayProc")];

        store
            .upsert_overlay_file_with_chunks(
                CONFIGURATION_ROOT_ID,
                "src/Overlay.bsl",
                hash.as_bytes(),
                "code",
                &chunks,
                None,
            )
            .unwrap();

        assert_eq!(store.overlay_file_count("code").unwrap(), 1);
        assert_eq!(store.overlay_chunk_count("code").unwrap(), 1);

        let results = store.overlay_text_search("OverlayProc", 10, Some("code")).unwrap();
        assert_eq!(results.len(), 1);

        store.remove_overlay_file(CONFIGURATION_ROOT_ID, "src/Overlay.bsl").unwrap();
        assert_eq!(store.overlay_file_count("code").unwrap(), 0);
        assert_eq!(store.overlay_chunk_count("code").unwrap(), 0);
        assert_eq!(store.overlay_text_search("OverlayProc", 10, Some("code")).unwrap().len(), 0);
    }

    #[test]
    fn clear_overlay_state_removes_all() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"overlay");
        store
            .upsert_overlay_file_with_chunks(
                CONFIGURATION_ROOT_ID,
                "src/A.bsl",
                hash.as_bytes(),
                "code",
                &[sample_chunk("ProcA")],
                None,
            )
            .unwrap();
        store.insert_overlay_tombstone(CONFIGURATION_ROOT_ID, "src/B.bsl", "code").unwrap();

        store.clear_overlay_state("code").unwrap();
        assert_eq!(store.overlay_file_count("code").unwrap(), 0);
        assert_eq!(store.overlay_chunk_count("code").unwrap(), 0);
        assert_eq!(store.overlay_tombstone_count("code").unwrap(), 0);
    }

    #[test]
    fn overlay_embeddings_roundtrip() {
        let mut store = Store::in_memory().unwrap();
        let hash = blake3::hash(b"overlay");
        let embedding = vec![0.1f32, 0.2, 0.3, 0.4];
        store
            .upsert_overlay_file_with_chunks(
                CONFIGURATION_ROOT_ID,
                "src/Emb.bsl",
                hash.as_bytes(),
                "code",
                &[sample_chunk("EmbProc")],
                Some(std::slice::from_ref(&embedding)),
            )
            .unwrap();

        let loaded = store.load_overlay_embeddings(4).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, embedding);
    }

    #[test]
    fn reindex_persists_and_loads_graph_context() {
        let mut store = Store::in_memory().unwrap();
        store
            .reindex_file_with_context(
                CONFIGURATION_ROOT_ID,
                "A.bsl",
                b"h",
                &[sample_chunk("Делать")],
                None,
                Some(&[Some("Dispatch: server | сервер\nCalls: Иная\n".to_owned())]),
            )
            .unwrap();
        let docs = store.load_indexed_documents(Some("code")).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].graph_context.as_deref(),
            Some("Dispatch: server | сервер\nCalls: Иная\n")
        );

        // A chunk indexed without context round-trips as `None`.
        store
            .reindex_file(CONFIGURATION_ROOT_ID, "B.bsl", b"h2", &[sample_chunk("Плейн")], None)
            .unwrap();
        let b = store
            .load_indexed_documents(Some("code"))
            .unwrap()
            .into_iter()
            .find(|d| d.path == "B.bsl")
            .unwrap();
        assert_eq!(b.graph_context, None);
    }

    #[test]
    fn embed_text_version_bump_clears_file_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"realhash",
                    &[sample_chunk("Делать")],
                    None,
                )
                .unwrap();
            assert_eq!(
                store.file_hash(CONFIGURATION_ROOT_ID, "A.bsl").unwrap().unwrap(),
                b"realhash"
            );
            // Simulate a database written by an older embed-text format.
            store.conn.pragma_update(None, "user_version", 0i64).unwrap();
        }
        // Reopening with a version mismatch clears the hash, so the next index
        // re-embeds the file under the current format instead of keeping a stale vector.
        let store = Store::open(&path).unwrap();
        assert!(
            store.file_hash(CONFIGURATION_ROOT_ID, "A.bsl").unwrap().unwrap().is_empty(),
            "file hash cleared to force re-embed"
        );

        // A second open at the same version is a no-op (does not re-clear).
        let store = Store::open(&path).unwrap();
        assert!(store.file_hash(CONFIGURATION_ROOT_ID, "A.bsl").unwrap().unwrap().is_empty());
    }

    #[test]
    fn open_stamps_current_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let store = Store::open(&path).unwrap();
        let stored = Store::stored_schema_version(&store.conn).unwrap();
        assert_eq!(stored, Some(SCHEMA_VERSION));
    }

    #[test]
    fn schema_version_bump_wipes_derived_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"realhash",
                    &[sample_chunk("Делать")],
                    None,
                )
                .unwrap();
            assert_eq!(store.file_count().unwrap(), 1);
            assert_eq!(store.chunk_count().unwrap(), 1);
            // Simulate a database written under an older structural schema.
            store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![(SCHEMA_VERSION - 1).to_string()],
                )
                .unwrap();
        }
        // Reopening with a structural-version mismatch wipes the cache and rebuilds the
        // current schema; the rows are gone and the version is stamped current.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.file_count().unwrap(), 0);
        assert_eq!(store.chunk_count().unwrap(), 0);
        assert_eq!(Store::stored_schema_version(&store.conn).unwrap(), Some(SCHEMA_VERSION));
    }

    #[test]
    fn a_root_transition_retracts_every_persistent_key_carrier() {
        let mut store = Store::in_memory().unwrap();
        let key = FileKey::new("removed-root", "Module.bsl");
        store.upsert_file(&key.root_id, &key.path, b"hash", "code").unwrap();
        store
            .upsert_overlay_file_with_chunks(
                &key.root_id,
                &key.path,
                b"overlay",
                "code",
                &[sample_chunk("Overlay")],
                None,
            )
            .unwrap();
        store.insert_overlay_tombstone(&key.root_id, &key.path, "code").unwrap();
        store.mark_context_dirty("code", &key.root_id, &key.path).unwrap();
        store
            .save_overlay_fingerprint_cache(
                "snapshot",
                &HashMap::from([(
                    key.clone(),
                    PersistedFingerprint {
                        file_size: 1,
                        file_mtime_secs: 2,
                        file_mtime_nanos: 3,
                        content_fingerprint: "fingerprint".to_owned(),
                        canonical: "/removed/Module.bsl".to_owned(),
                    },
                )]),
            )
            .unwrap();

        let changed_root_ids = HashSet::from([key.root_id.clone()]);
        assert!(store
            .apply_workspace_roots_transition(
                WorkspaceStoreTransition {
                    changed_root_ids: &changed_root_ids,
                    cleanup: &HashSet::new(),
                    tombstones: &HashSet::new(),
                    upserts: &[],
                },
                &mut || ControlFlow::Continue(()),
            )
            .unwrap()
            .is_continue());

        assert!(store.file_hash(&key.root_id, &key.path).unwrap().is_none());
        assert_eq!(store.overlay_file_count("code").unwrap(), 0);
        assert!(!store.overlay_tombstone_paths("code").unwrap().contains(&key));
        assert!(!store.context_dirty_paths("code").unwrap().contains(&key));
        assert!(!store.overlay_fingerprint_keys().unwrap().contains(&key));
    }

    #[test]
    fn pre_versioning_database_is_kept_and_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .reindex_file(
                    CONFIGURATION_ROOT_ID,
                    "A.bsl",
                    b"realhash",
                    &[sample_chunk("Делать")],
                    None,
                )
                .unwrap();
            // Simulate a database created before schema versioning existed.
            store.conn.execute("DELETE FROM meta WHERE key = 'schema_version'", []).unwrap();
        }
        // A missing version row is treated as already-current: the data survives and the
        // version is stamped, so existing workspaces are not force-reindexed on upgrade.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.file_count().unwrap(), 1);
        assert_eq!(Store::stored_schema_version(&store.conn).unwrap(), Some(SCHEMA_VERSION));
    }
}
