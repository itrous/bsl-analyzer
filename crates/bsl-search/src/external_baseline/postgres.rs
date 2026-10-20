use crate::domain::{
    BaselineRef, CorpusId, ExternalBaselineConfig, IndexedDocument, LexicalHit, SemanticHit,
    Snapshot, SnapshotPublishMetadata, SnapshotPublishStats, SourceSpan,
};
use crate::error::{reason, ReasonCode, SearchError};
use crate::external_baseline::{
    BaselineCollectionRecord, BaselineEmbeddingCoverageRecord, BaselineEmbeddingModelRecord,
    BaselineEmbeddingStats, BaselineFileObjectDetails, BaselineFileObjectRecord,
    BaselineFileObjectReference, BaselineGcReport, BaselineSemanticPublication,
    BaselineSnapshotDetails, BaselineSnapshotRecord, SemanticPublishPhase, SemanticPublishProgress,
};
use crate::ports::{
    BaselineLexicalSearch, BaselineSemanticSearch, SnapshotCatalog, SnapshotContentStore,
    SnapshotPublisher, WorkspaceBaselineManifestStore,
};
use postgres::{GenericClient, NoTls, Row, Transaction};
use r2d2_postgres::{r2d2::Pool, PostgresConnectionManager};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SCHEMA_METADATA_TABLE: &str = "_schema_metadata_";
const EMBEDDING_MODEL_SETTING: &str = "embedding_model";
const EMBEDDING_DIMENSION_SETTING: &str = "embedding_dimension";
const TOKEN_LAYOUT_VERSION_SETTING: &str = "token_layout_version";
const TOKEN_LAYOUT_CLAIM_SETTING: &str = "token_layout_claim";
const TOKEN_LAYOUT_PARTS_TABLE: &str = "file_object_parts";
/// Whether a carrier has to be there at all.
///
/// `serving_semantic` exists only where the `vector` extension does, and a database without it is
/// fully usable for everything else. That is the ONLY difference the two kinds make: an absent
/// optional carrier is not a fault, while an absent mandatory one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CarrierObligation {
    Required,
    Optional,
}

/// The carriers of file identity and the key each one must have.
///
/// One list, consulted by the migration that builds the keys and by the readiness check that
/// answers for them. Two lists would be two answers to the same question, and the one nobody
/// looked at would be the wrong one — so obligation is a FIELD here, not a second list.
const ROOTED_CARRIER_KEYS: [(&str, &[&str], CarrierObligation); 4] = [
    (
        "snapshot_files",
        &["snapshot_id", "collection", "root_id", "path"],
        CarrierObligation::Required,
    ),
    (
        "snapshot_deletions",
        &["snapshot_id", "collection", "root_id", "path"],
        CarrierObligation::Required,
    ),
    (
        "serving_lexical",
        &["snapshot_id", "collection", "root_id", "path", "ordinal"],
        CarrierObligation::Required,
    ),
    (
        "serving_semantic",
        &["snapshot_id", "model_id", "collection", "root_id", "path", "ordinal"],
        CarrierObligation::Optional,
    ),
];

const REQUIRED_STORAGE_TABLES: &[&str] = &[
    SCHEMA_METADATA_TABLE,
    "snapshots",
    "snapshot_files",
    "snapshot_deletions",
    "snapshot_heads",
    "content_objects",
    "semantic_embeddings",
    "file_objects",
    "file_object_items",
    "serving_lexical",
];

type PgPooledConnection = r2d2_postgres::r2d2::PooledConnection<PostgresConnectionManager<NoTls>>;

const CONTENT_OBJECT_BATCH_SIZE: usize = 5_000;
const FILE_OBJECT_ITEM_BATCH_SIZE: usize = 2_000;
const SNAPSHOT_FILE_BATCH_SIZE: usize = 2_000;
const SNAPSHOT_DELETION_BATCH_SIZE: usize = 5_000;
const SERVING_LEXICAL_BATCH_SIZE: usize = 2_000;
const SERVING_SEMANTIC_BATCH_SIZE: usize = 256;
const SEMANTIC_PUBLICATION_COMPLETE_PREFIX: &str = "semantic_publication_complete:";

/// Readiness re-verification period. A successful check is trusted for this long, then
/// re-run, so the typed `StorageNotInitialized` / `SchemaVersionMismatch` errors still
/// surface (with bounded delay) if the schema is dropped or migrated mid-process — callers
/// branch on those exact error kinds and never treat them as retryable.
const STORAGE_READINESS_TTL: Duration = Duration::from_secs(60);

/// How long the pool waits for a connection before a call fails.
///
/// Test builds give a pool aimed at port 1 50 ms: that is the endpoint the fixtures use for
/// "nothing listens", the pool retries its refusals for the whole budget, and the failure a
/// test waits for arrives after it either way. A pool aimed anywhere else — a fixture's own
/// listener included — keeps the production budget.
fn connection_timeout(config: &postgres::Config) -> std::time::Duration {
    #[cfg(any(test, feature = "test-seams"))]
    if config.get_ports().contains(&1) {
        return std::time::Duration::from_millis(50);
    }
    #[cfg(not(any(test, feature = "test-seams")))]
    let _ = config;
    std::time::Duration::from_secs(5)
}

#[derive(Debug, Clone)]
pub struct PostgresBaselineAdapter {
    config: ExternalBaselineConfig,
    schema: String,
    pool: Pool<PostgresConnectionManager<NoTls>>,
    storage_verified_at: Arc<Mutex<Option<Instant>>>,
    token_layout_claim: Option<(String, String)>,
}

impl PostgresBaselineAdapter {
    pub fn new(config: ExternalBaselineConfig) -> Result<Self, SearchError> {
        let schema = config.schema.clone().unwrap_or_else(|| "bsl_search".to_owned());
        validate_identifier(&schema)?;
        let pg_config = config.connection.parse().map_err(|err: postgres::Error| {
            SearchError::ExternalBaseline(format!("invalid connection string: {err}"))
        })?;
        let connection_timeout = connection_timeout(&pg_config);
        let manager = PostgresConnectionManager::new(pg_config, NoTls);
        let pool = Pool::builder()
            .max_size(4)
            .connection_timeout(connection_timeout)
            .build_unchecked(manager);
        Ok(Self {
            config,
            schema,
            pool,
            storage_verified_at: Arc::new(Mutex::new(None)),
            token_layout_claim: None,
        })
    }

    pub fn config(&self) -> &ExternalBaselineConfig {
        &self.config
    }

    pub fn with_token_layout_claim(
        mut self,
        layout_version: impl Into<String>,
        claim: impl Into<String>,
    ) -> Result<Self, SearchError> {
        let layout_version = layout_version.into();
        let claim = claim.into();
        if layout_version != crate::token_policy::SEGMENTATION_VERSION || claim.trim().is_empty() {
            return Err(crate::EmbeddingFailure::new(
                crate::EmbeddingFailureCode::EmbeddingInvalidConfig,
            )
            .into());
        }
        if self.token_layout_claim.as_ref().is_some_and(|(version, configured)| {
            version.as_str() != layout_version || configured.as_str() != claim
        }) {
            return Err(crate::EmbeddingFailure::new(
                crate::EmbeddingFailureCode::EmbeddingInvalidConfig,
            )
            .into());
        }
        self.token_layout_claim = Some((layout_version, claim));
        if let Ok(mut verified_at) = self.storage_verified_at.lock() {
            *verified_at = None;
        }
        Ok(self)
    }

    pub fn ensure_token_layout_claim(
        &self,
        layout_version: &str,
        claim: &str,
    ) -> Result<(), SearchError> {
        if layout_version != crate::token_policy::SEGMENTATION_VERSION || claim.trim().is_empty() {
            return Err(crate::EmbeddingFailure::new(
                crate::EmbeddingFailureCode::EmbeddingInvalidConfig,
            )
            .into());
        }
        if !self.token_layout_claim.as_ref().is_some_and(|(version, configured)| {
            version.as_str() == layout_version && configured.as_str() == claim
        }) {
            return Err(crate::EmbeddingFailure::new(
                crate::EmbeddingFailureCode::EmbeddingInvalidConfig,
            )
            .into());
        }
        let mut client = self.connect()?;
        let mut tx = client.transaction()?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtext($1), hashtext('bsl-token-layout-claim'))",
            &[&self.schema],
        )?;
        let existing_claim = tx.query(
            &format!(
                "SELECT setting, value FROM {} WHERE setting = ANY($1)",
                self.table(SCHEMA_METADATA_TABLE)
            ),
            &[&&[TOKEN_LAYOUT_VERSION_SETTING, TOKEN_LAYOUT_CLAIM_SETTING][..]],
        )?;
        if !existing_claim.is_empty() {
            let found = existing_claim
                .into_iter()
                .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                .collect::<HashMap<_, _>>();
            if found.get(TOKEN_LAYOUT_VERSION_SETTING).map(String::as_str) != Some(layout_version)
                || found.get(TOKEN_LAYOUT_CLAIM_SETTING).map(String::as_str) != Some(claim)
                || !self.storage_table_exists(&mut tx, TOKEN_LAYOUT_PARTS_TABLE)?
            {
                return Err(SearchError::ExternalBaseline(
                    "token_layout_claim_mismatch: schema claim or companion table differs"
                        .to_owned(),
                ));
            }
            self.check_layout_claim_rows(&mut tx, Some((layout_version, claim)))?;
            tx.commit()?;
            return Ok(());
        }
        if self.storage_table_exists(&mut tx, TOKEN_LAYOUT_PARTS_TABLE)? {
            return Err(SearchError::ExternalBaseline(
                "token_layout_claim_mismatch: companion table exists without a claim".to_owned(),
            ));
        }
        if !self.storage_table_exists(&mut tx, "snapshots")? {
            return Err(SearchError::StorageNotInitialized { schema: self.schema.clone() });
        }
        let rows: i64 = tx
            .query_one(
                &format!(
                    "SELECT
                       (SELECT COUNT(*) FROM {}) +
                       (SELECT COUNT(*) FROM {}) +
                       (SELECT COUNT(*) FROM {}) +
                       (SELECT COUNT(*) FROM {}) +
                       (SELECT COUNT(*) FROM {}) +
                       (SELECT COUNT(*) FROM {})",
                    self.table("snapshots"),
                    self.table("file_objects"),
                    self.table("file_object_items"),
                    self.table("serving_lexical"),
                    self.table("content_objects"),
                    self.table("semantic_embeddings"),
                ),
                &[],
            )?
            .get(0);
        if rows != 0 {
            return Err(SearchError::ExternalBaseline(format!(
                "token_layout_claim_required: schema '{}' contains legacy rows and cannot be claimed",
                self.schema
            )));
        }
        tx.batch_execute(&format!(
            "CREATE TABLE {} (
                file_object_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                parent_symbol TEXT NOT NULL,
                byte_start BIGINT NOT NULL,
                byte_end BIGINT NOT NULL,
                parent_byte_start BIGINT NOT NULL,
                parent_byte_end BIGINT NOT NULL,
                part_index BIGINT NOT NULL,
                part_count BIGINT NOT NULL,
                PRIMARY KEY (file_object_id, ordinal),
                CHECK (byte_start >= parent_byte_start AND byte_end <= parent_byte_end),
                CHECK (byte_start < byte_end AND parent_byte_start < parent_byte_end),
                CHECK (part_index > 0 AND part_count >= part_index)
            )",
            self.table(TOKEN_LAYOUT_PARTS_TABLE),
            self.table("file_objects"),
        ))?;
        let insert = format!(
            "INSERT INTO {} (setting, value) VALUES ($1, $2)",
            self.table(SCHEMA_METADATA_TABLE)
        );
        tx.execute(&insert, &[&TOKEN_LAYOUT_VERSION_SETTING, &layout_version])?;
        tx.execute(&insert, &[&TOKEN_LAYOUT_CLAIM_SETTING, &claim])?;
        tx.commit()?;
        if let Ok(mut verified_at) = self.storage_verified_at.lock() {
            *verified_at = None;
        }
        Ok(())
    }

    fn check_layout_claim_rows(
        &self,
        client: &mut impl GenericClient,
        expected: Option<(&str, &str)>,
    ) -> Result<(), SearchError> {
        let rows = client.query(
            &format!(
                "SELECT setting, value FROM {} WHERE setting = ANY($1)",
                self.table(SCHEMA_METADATA_TABLE)
            ),
            &[&&[TOKEN_LAYOUT_VERSION_SETTING, TOKEN_LAYOUT_CLAIM_SETTING][..]],
        )?;
        let found = rows
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect::<HashMap<_, _>>();
        if found.len() == 1 {
            return Err(SearchError::ExternalBaseline(
                "token_layout_claim_missing: schema has only part of the layout claim".to_owned(),
            ));
        }
        let actual =
            found.get(TOKEN_LAYOUT_VERSION_SETTING).zip(found.get(TOKEN_LAYOUT_CLAIM_SETTING));
        let expected = expected.or(self
            .token_layout_claim
            .as_ref()
            .map(|(version, claim)| (version.as_str(), claim.as_str())));
        match (expected, actual) {
            (None, None) => {
                if self.storage_table_exists(client, TOKEN_LAYOUT_PARTS_TABLE)? {
                    return Err(SearchError::ExternalBaseline(
                        "token_layout_claim_mismatch: unclaimed schema has a companion parts table".to_owned(),
                    ));
                }
            }
            (None, Some(_)) => return Err(SearchError::ExternalBaseline(
                "token_layout_claim_mismatch: token-bound schema requires a configured layout claim".to_owned(),
            )),
            (Some(_), None) => return Err(SearchError::ExternalBaseline(
                "token_layout_claim_missing: token-bound schema has no complete layout claim".to_owned(),
            )),
            (Some((version, claim)), Some((actual_version, actual_claim))) => {
                if version != actual_version || claim != actual_claim
                    || !self.storage_table_exists(client, TOKEN_LAYOUT_PARTS_TABLE)?
                {
                    return Err(SearchError::ExternalBaseline(
                        "token_layout_claim_mismatch: schema claim or companion table differs".to_owned(),
                    ));
                }
                let incomplete: i64 = client
                    .query_one(
                        &format!(
                            "SELECT COUNT(*)
                             FROM {} foi
                             JOIN {} fo ON fo.id = foi.file_object_id
                             LEFT JOIN {} part
                               ON part.file_object_id = foi.file_object_id
                              AND part.ordinal = foi.ordinal
                             WHERE fo.collection = 'code' AND part.file_object_id IS NULL",
                            self.table("file_object_items"),
                            self.table("file_objects"),
                            self.table(TOKEN_LAYOUT_PARTS_TABLE),
                        ),
                        &[],
                    )?
                    .get(0);
                if incomplete != 0 {
                    return Err(SearchError::ExternalBaseline(
                        "token_layout_provenance_missing: code documents lack part metadata".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn connect(&self) -> Result<PgPooledConnection, SearchError> {
        self.pool.get().map_err(|err| {
            SearchError::named(
                pool_connection_reason_code(&err.to_string()),
                format!("failed to get pooled connection: {err}"),
            )
        })
    }

    fn table(&self, table: &str) -> String {
        format!("{}.{}", self.schema, table)
    }

    fn check_embedding_claim_model(&self, model_id: &str) -> Result<(), SearchError> {
        if self.token_layout_claim.as_ref().is_some_and(|(_, claim)| claim.as_str() != model_id) {
            return Err(SearchError::ExternalBaseline(
                "token_layout_claim_mismatch: embedding identity differs from configured layout"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn storage_table_exists(
        &self,
        client: &mut impl GenericClient,
        table_name: &str,
    ) -> Result<bool, SearchError> {
        Ok(client
            .query_opt(
                "SELECT 1 FROM information_schema.tables
                 WHERE table_schema = $1 AND table_name = $2
                 LIMIT 1",
                &[&self.schema, &table_name],
            )?
            .is_some())
    }

    fn lexical_serving_rows_exist(
        &self,
        client: &mut impl GenericClient,
        snapshot_id: &str,
        collection: Option<&str>,
    ) -> Result<bool, SearchError> {
        let mut sql =
            format!("SELECT 1 FROM {} WHERE snapshot_id = $1", self.table("serving_lexical"));
        let snapshot_id = snapshot_id.to_owned();
        let collection = collection.map(ToOwned::to_owned);
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&snapshot_id];
        if let Some(collection) = collection.as_ref() {
            sql.push_str(&format!(" AND collection = ${}", params.len() + 1));
            params.push(collection);
        }
        sql.push_str(" LIMIT 1");
        Ok(client.query_opt(&sql, &params)?.is_some())
    }

    fn semantic_serving_rows_exist(
        &self,
        client: &mut impl GenericClient,
        snapshot_id: &str,
        model_id: &str,
        dimension: i32,
        collection: Option<&str>,
    ) -> Result<bool, SearchError> {
        let mut sql = format!(
            "SELECT 1 FROM {} WHERE snapshot_id = $1 AND model_id = $2 AND dimension = $3",
            self.table("serving_semantic")
        );
        let snapshot_id = snapshot_id.to_owned();
        let model_id = model_id.to_owned();
        let collection = collection.map(ToOwned::to_owned);
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            vec![&snapshot_id, &model_id, &dimension];
        if let Some(collection) = collection.as_ref() {
            sql.push_str(&format!(" AND collection = ${}", params.len() + 1));
            params.push(collection);
        }
        sql.push_str(" LIMIT 1");
        Ok(client.query_opt(&sql, &params)?.is_some())
    }

    pub fn check_storage_readiness(&self) -> Result<(), SearchError> {
        // Nearly every public method starts with this check; without the TTL cache each
        // call pays information_schema roundtrips on the shared 4-connection pool.
        // Only success is cached — failures re-check on every call so `migrate_storage`
        // heals a not-ready schema without waiting out the TTL.
        if let Ok(verified_at) = self.storage_verified_at.lock() {
            if verified_at.is_some_and(|at| at.elapsed() < STORAGE_READINESS_TTL) {
                let mut client = self.connect()?;
                let expected = self
                    .token_layout_claim
                    .as_ref()
                    .map(|(version, claim)| (version.as_str(), claim.as_str()));
                self.check_layout_claim_rows(&mut *client, expected)?;
                return Ok(());
            }
        }

        let mut client = self.connect()?;
        let present_tables: i64 = client
            .query_one(
                "SELECT COUNT(DISTINCT table_name) FROM information_schema.tables
                 WHERE table_schema = $1 AND table_name = ANY($2)",
                &[&self.schema, &REQUIRED_STORAGE_TABLES],
            )?
            .get(0);
        if present_tables as usize != REQUIRED_STORAGE_TABLES.len() {
            return Err(SearchError::StorageNotInitialized { schema: self.schema.clone() });
        }

        let version = {
            let row = client.query_opt(
                &format!(
                    "SELECT value::INTEGER FROM {} WHERE setting = 'schema_version' LIMIT 1",
                    self.table(SCHEMA_METADATA_TABLE)
                ),
                &[],
            )?;
            row.map(|r| r.get::<_, i32>(0))
        };
        if let Some(version) = version {
            if version != crate::error::SCHEMA_VERSION_CURRENT {
                return Err(SearchError::SchemaVersionMismatch {
                    expected: crate::error::SCHEMA_VERSION_CURRENT,
                    actual: Some(version),
                    schema: self.schema.clone(),
                });
            }
        } else {
            return Err(SearchError::StorageNotInitialized { schema: self.schema.clone() });
        }

        let expected = self
            .token_layout_claim
            .as_ref()
            .map(|(version, claim)| (version.as_str(), claim.as_str()));
        self.check_layout_claim_rows(&mut *client, expected)?;

        self.check_carriers_key_by_root(&mut client)?;

        if let Ok(mut verified_at) = self.storage_verified_at.lock() {
            *verified_at = Some(Instant::now());
        }
        Ok(())
    }

    /// Every mandatory carrier of file identity must have EXACTLY the key the migration builds.
    ///
    /// Membership of `root_id` alone would be too weak: a key of `(root_id)` contains it and
    /// still cannot tell two files apart, so the damage would surface as a duplicate-key error
    /// in the middle of a publish instead of as a named refusal before one. Asking for the exact
    /// composition costs the same query and answers the question the storage actually asks.
    ///
    /// Checking the presence of the COLUMN would be weaker still, and not in theory: after a
    /// failure between `DROP CONSTRAINT` and `ADD PRIMARY KEY` the column is there while the key
    /// is not. Migration through this build makes that unreachable, but readiness also answers
    /// for schemas brought to it by another hand.
    ///
    /// An OPTIONAL carrier is asked about only when it exists — and existence is asked directly.
    ///
    /// Not inferred from an empty key: the composition subquery yields NULL both for a table that
    /// is not there and for a table that is there with no primary key at all. Reading NULL as
    /// "absent" would wave through exactly the state described above, where a run died between
    /// `DROP CONSTRAINT` and `ADD PRIMARY KEY`.
    fn check_carriers_key_by_root(
        &self,
        client: &mut PgPooledConnection,
    ) -> Result<(), SearchError> {
        let carriers: Vec<&str> = ROOTED_CARRIER_KEYS.iter().map(|(table, _, _)| *table).collect();
        let rows = client.query(
            "SELECT carrier,
                    to_regclass($1 || '.' || carrier) IS NOT NULL,
                    (SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
                       FROM pg_constraint c
                       CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_attribute a
                         ON a.attrelid = c.conrelid AND a.attnum = k.attnum
                      WHERE c.contype = 'p'
                        AND c.conrelid = to_regclass($1 || '.' || carrier))
               FROM unnest($2::TEXT[]) AS carrier",
            &[&self.schema, &carriers.as_slice()],
        )?;

        let mut wrong = Vec::new();
        for row in rows {
            let carrier: String = row.get(0);
            let present: bool = row.get(1);
            let actual: Option<Vec<String>> = row.get(2);
            let Some((_, expected, obligation)) =
                ROOTED_CARRIER_KEYS.iter().find(|(table, _, _)| *table == carrier)
            else {
                continue;
            };
            if !present && *obligation == CarrierObligation::Optional {
                continue;
            }
            let matches = actual.as_ref().is_some_and(|columns| {
                columns.iter().map(String::as_str).eq(expected.iter().copied())
            });
            if !matches {
                wrong.push(format!(
                    "{carrier} keyed by {}",
                    actual
                        .map(|columns| columns.join(", "))
                        .unwrap_or_else(|| "nothing".to_owned())
                ));
            }
        }

        if wrong.is_empty() {
            return Ok(());
        }
        Err(SearchError::StorageNotInitialized {
            schema: format!("{}; wrong key on {}", self.schema, wrong.join("; ")),
        })
    }

    pub fn get_schema_version(&self) -> Result<Option<i32>, SearchError> {
        let mut client = self.connect()?;
        let row = client.query_opt(
            &format!(
                "SELECT value::INTEGER FROM {} WHERE setting = 'schema_version' LIMIT 1",
                self.table(SCHEMA_METADATA_TABLE)
            ),
            &[],
        )?;
        Ok(row.map(|r| r.get::<_, i32>(0)))
    }

    fn write_schema_version(&self, tx: &mut Transaction<'_>) -> Result<(), SearchError> {
        let schema_version = crate::error::SCHEMA_VERSION_CURRENT.to_string();
        tx.execute(
            &format!(
                "INSERT INTO {} (setting, value)
                 VALUES ('schema_version', $1::TEXT)
                 ON CONFLICT (setting) DO UPDATE SET value = EXCLUDED.value",
                self.table(SCHEMA_METADATA_TABLE),
            ),
            &[&schema_version],
        )?;
        Ok(())
    }

    pub fn read_embedding_identity(&self) -> Result<Option<(String, usize)>, SearchError> {
        let mut client = self.connect()?;
        let model = client
            .query_opt(
                &format!(
                    "SELECT value FROM {} WHERE setting = $1 LIMIT 1",
                    self.table(SCHEMA_METADATA_TABLE)
                ),
                &[&EMBEDDING_MODEL_SETTING],
            )?
            .map(|row| row.get::<_, String>(0));
        let dimension = client
            .query_opt(
                &format!(
                    "SELECT value::INTEGER FROM {} WHERE setting = $1 LIMIT 1",
                    self.table(SCHEMA_METADATA_TABLE)
                ),
                &[&EMBEDDING_DIMENSION_SETTING],
            )?
            .map(|row| row.get::<_, i32>(0));
        match (model, dimension) {
            (Some(model), Some(dimension)) => Ok(Some((model, dimension as usize))),
            _ => Ok(None),
        }
    }

    pub fn ensure_embedding_identity(
        &self,
        model_id: &str,
        dimension: usize,
    ) -> Result<(), SearchError> {
        self.check_embedding_claim_model(model_id)?;
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let mut tx = client.transaction()?;
        // Claim the identity if unset, atomically. `DO NOTHING` (not `DO UPDATE`) means
        // the first writer wins; a concurrent first publish blocks on the row lock and,
        // once the winner commits, its own insert becomes a no-op. The read-back below
        // therefore reflects the single committed identity for every writer, so two racing
        // first publishes with different models can't both succeed.
        let claim = format!(
            "INSERT INTO {} (setting, value)
             VALUES ($1, $2)
             ON CONFLICT (setting) DO NOTHING",
            self.table(SCHEMA_METADATA_TABLE)
        );
        tx.execute(&claim, &[&EMBEDDING_MODEL_SETTING, &model_id])?;
        tx.execute(&claim, &[&EMBEDDING_DIMENSION_SETTING, &dimension.to_string()])?;

        let recorded_model: String = tx
            .query_one(
                &format!(
                    "SELECT value FROM {} WHERE setting = $1",
                    self.table(SCHEMA_METADATA_TABLE)
                ),
                &[&EMBEDDING_MODEL_SETTING],
            )?
            .get(0);
        let recorded_dimension: i32 = tx
            .query_one(
                &format!(
                    "SELECT value::INTEGER FROM {} WHERE setting = $1",
                    self.table(SCHEMA_METADATA_TABLE)
                ),
                &[&EMBEDDING_DIMENSION_SETTING],
            )?
            .get(0);
        tx.commit()?;

        let recorded_dimension = recorded_dimension as usize;
        if recorded_model != model_id || recorded_dimension != dimension {
            let schema = &self.schema;
            return Err(SearchError::ExternalBaseline(format!(
                "embedding identity mismatch for schema '{schema}': baseline uses \
                 model '{recorded_model}' (dim {recorded_dimension}); refusing to publish with \
                 model '{model_id}' (dim {dimension}). A shared baseline must use one \
                 embedding model."
            )));
        }
        Ok(())
    }

    fn snapshot_row_to_model(row: Row) -> Snapshot {
        let corpus = CorpusId::from_storage(row.get::<_, String>("corpus"));
        let mut snapshot = Snapshot::new(row.get::<_, String>("id"), corpus);
        snapshot.fingerprint = row.get("fingerprint");
        snapshot.parent_id =
            row.get::<_, Option<String>>("parent_snapshot_id").map(crate::SnapshotId::new);
        snapshot
    }

    fn latest_snapshot_query(&self, with_filter: &str) -> String {
        format!(
            "SELECT id, corpus, fingerprint, parent_snapshot_id
             FROM {}
             WHERE {}
             ORDER BY created_at DESC
             LIMIT 1",
            self.table("snapshots"),
            with_filter
        )
    }

    /// Gives a carrier the root column on schemas published before roots existed.
    ///
    /// `CONFIGURATION_ROOT_ID` is the empty string precisely so this backfill is free: every
    /// pre-root row belongs to the configuration, so the default is already its true value and
    /// not one row is rewritten.
    fn add_root_id_column(&self, table: &str) -> String {
        format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS root_id TEXT NOT NULL DEFAULT ''",
            self.table(table)
        )
    }

    /// The same backfill for a carrier that may not be there at all.
    ///
    /// `IF NOT EXISTS` in the plain form speaks about the COLUMN; the statement still fails on a
    /// missing table, and it runs inside the mandatory migration's transaction — so on a database
    /// without the `vector` extension the unguarded form would roll the whole migration back and
    /// leave a perfectly usable schema permanently unready. Deliberately not folded into
    /// `add_root_id_column`: a mandatory carrier that has gone missing must not be skipped in
    /// silence.
    fn add_root_id_column_if_the_carrier_exists(&self, table: &str) -> String {
        let qualified = self.table(table);
        format!(
            "DO $$
             BEGIN
                 IF to_regclass('{qualified}') IS NULL THEN
                     RETURN;
                 END IF;
                 ALTER TABLE {qualified} ADD COLUMN IF NOT EXISTS root_id TEXT NOT NULL DEFAULT '';
             END $$"
        )
    }

    /// Brings every carrier of file identity to its rooted key, columns before keys.
    ///
    /// Driven from `ROOTED_CARRIER_KEYS` rather than written out, so a carrier added to the list
    /// is migrated by the same act that declares it. Order matters across the whole set, not
    /// within one carrier: the column has to exist before anything keys by it.
    fn give_every_carrier_the_root_key(&self) -> Vec<String> {
        let mut statements = Vec::with_capacity(ROOTED_CARRIER_KEYS.len() * 2);
        for (table, _, obligation) in ROOTED_CARRIER_KEYS {
            statements.push(match obligation {
                CarrierObligation::Required => self.add_root_id_column(table),
                CarrierObligation::Optional => self.add_root_id_column_if_the_carrier_exists(table),
            });
        }
        for (table, columns, _) in ROOTED_CARRIER_KEYS {
            statements.push(self.enforce_primary_key(table, columns));
        }
        statements
    }

    /// Brings a carrier's primary key to `columns`, deciding by the catalog rather than by name.
    ///
    /// The name of a constraint guarantees nothing about its columns — a schema created before
    /// roots carries `..._pkey` over a two-part key — so the composition is read from
    /// `pg_constraint` and the rebuild happens only when it actually differs. That is what makes
    /// re-running the migration a no-op instead of a fresh lock on the largest tables.
    fn enforce_primary_key(&self, table: &str, columns: &[&str]) -> String {
        let qualified = self.table(table);
        format!(
            "DO $$
             DECLARE
                 target_columns TEXT[] := {};
                 actual_columns TEXT[];
                 key_name TEXT;
                 relation OID := to_regclass('{qualified}');
             BEGIN
                 IF relation IS NULL THEN
                     RETURN;
                 END IF;
                 SELECT c.conname, array_agg(a.attname ORDER BY k.ord)
                   INTO key_name, actual_columns
                   FROM pg_constraint c
                   CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
                   JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
                  WHERE c.conrelid = relation AND c.contype = 'p'
                  GROUP BY c.conname;
                 IF actual_columns IS DISTINCT FROM target_columns THEN
                     IF key_name IS NOT NULL THEN
                         EXECUTE format('ALTER TABLE {qualified} DROP CONSTRAINT %I', key_name);
                     END IF;
                     EXECUTE 'ALTER TABLE {qualified} ADD PRIMARY KEY {}';
                 END IF;
             END $$",
            sql_text_array(columns),
            sql_column_list(columns),
        )
    }

    /// Same decision for a secondary index, and for the same reason made explicit here.
    ///
    /// These indexes are declared with `CREATE INDEX IF NOT EXISTS` under a fixed name, so on a
    /// live database a re-declaration with new columns is skipped BY NAME and the index silently
    /// keeps its old composition. Nothing about the answers changes when that happens — only the
    /// cost of the query — which is why it would otherwise never be noticed.
    fn enforce_index(&self, index: &str, table: &str, columns: &[&str]) -> String {
        let qualified = self.table(table);
        let index_name = format!("idx_{}_{index}", self.schema);
        let qualified_index = format!("{}.{index_name}", self.schema);
        format!(
            "DO $$
             DECLARE
                 target_columns TEXT[] := {};
                 actual_columns TEXT[];
                 relation OID := to_regclass('{qualified_index}');
             BEGIN
                 IF relation IS NOT NULL THEN
                     SELECT array_agg(a.attname ORDER BY k.ord)
                       INTO actual_columns
                       FROM pg_index i
                       CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                      WHERE i.indexrelid = relation
                        AND i.indrelid = to_regclass('{qualified}');
                 END IF;
                 IF actual_columns IS DISTINCT FROM target_columns THEN
                     EXECUTE 'DROP INDEX IF EXISTS {qualified_index}';
                     EXECUTE 'CREATE INDEX {index_name} ON {qualified} {}';
                 END IF;
             END $$",
            sql_text_array(columns),
            sql_column_list(columns),
        )
    }

    fn ensure_schema_statements(&self) -> Vec<String> {
        // One statement per carrier operation, never one script holding them all: the gate that
        // answers for a carrier picks its statements out by the carrier's own name, and a single
        // string naming all four satisfies every carrier's check with some other carrier's text.
        let mut statements = vec![
            format!("CREATE SCHEMA IF NOT EXISTS {}", self.schema),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    setting TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                )",
                self.table(SCHEMA_METADATA_TABLE)
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    id TEXT PRIMARY KEY,
                    corpus TEXT NOT NULL,
                    fingerprint TEXT NULL,
                    parent_snapshot_id TEXT NULL,
                    branch TEXT NULL,
                    commit_sha TEXT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                )",
                self.table("snapshots")
            ),
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS fingerprint TEXT NULL",
                self.table("snapshots")
            ),
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS parent_snapshot_id TEXT NULL",
                self.table("snapshots")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    content_hash TEXT PRIMARY KEY,
                    text TEXT NOT NULL
                )",
                self.table("content_objects")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    embedding_key TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    dimension INTEGER NOT NULL,
                    embedding BYTEA NOT NULL,
                    PRIMARY KEY (embedding_key, model_id, dimension)
                )",
                self.table("semantic_embeddings")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    id TEXT PRIMARY KEY,
                    collection TEXT NOT NULL,
                    file_fingerprint TEXT NOT NULL,
                    document_count INTEGER NOT NULL
                )",
                self.table("file_objects")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    file_object_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    ordinal INTEGER NOT NULL,
                    symbol_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    content_hash TEXT NOT NULL REFERENCES {}(content_hash) ON DELETE RESTRICT,
                    graph_context TEXT,
                    PRIMARY KEY (file_object_id, ordinal)
                )",
                self.table("file_object_items"),
                self.table("file_objects"),
                self.table("content_objects")
            ),
            // Idempotent column add for central databases created before
            // graph-enriched embeddings; pre-existing rows keep NULL (no context,
            // matching their pre-enrichment embeddings).
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS graph_context TEXT",
                self.table("file_object_items")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    snapshot_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    collection TEXT NOT NULL,
                    root_id TEXT NOT NULL DEFAULT '',
                    path TEXT NOT NULL,
                    file_fingerprint TEXT NOT NULL,
                    document_count INTEGER NOT NULL,
                    file_object_id TEXT NOT NULL REFERENCES {}(id) ON DELETE RESTRICT,
                    PRIMARY KEY (snapshot_id, collection, root_id, path)
                )",
                self.table("snapshot_files"),
                self.table("snapshots"),
                self.table("file_objects")
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    snapshot_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    collection TEXT NOT NULL,
                    root_id TEXT NOT NULL DEFAULT '',
                    path TEXT NOT NULL,
                    PRIMARY KEY (snapshot_id, collection, root_id, path)
                )",
                self.table("snapshot_deletions"),
                self.table("snapshots"),
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    snapshot_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    collection TEXT NOT NULL,
                    root_id TEXT NOT NULL DEFAULT '',
                    path TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    symbol_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    text TEXT NOT NULL,
                    tsv TSVECTOR NOT NULL,
                    PRIMARY KEY (snapshot_id, collection, root_id, path, ordinal)
                )",
                self.table("serving_lexical"),
                self.table("snapshots"),
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    corpus TEXT NOT NULL,
                    branch TEXT NOT NULL,
                    snapshot_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    PRIMARY KEY (corpus, branch)
                )",
                self.table("snapshot_heads"),
                self.table("snapshots"),
            ),
        ];
        // Между таблицами и индексами: индексы тоже ключуются корнем, поэтому колонка обязана
        // существовать раньше них, а не только раньше первичных ключей.
        statements.extend(self.give_every_carrier_the_root_key());
        statements.extend([
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_snapshots_corpus_created_at
                 ON {} (corpus, created_at DESC)",
                self.schema,
                self.table("snapshots")
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_snapshots_branch_commit
                 ON {} (corpus, branch, commit_sha, created_at DESC)",
                self.schema,
                self.table("snapshots")
            ),
            self.enforce_index(
                "snapshot_files_snapshot_path",
                "snapshot_files",
                &["snapshot_id", "collection", "root_id", "path"],
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_snapshot_files_object
                 ON {} (file_object_id)",
                self.schema,
                self.table("snapshot_files")
            ),
            self.enforce_index(
                "snapshot_deletions_snapshot_path",
                "snapshot_deletions",
                &["snapshot_id", "collection", "root_id", "path"],
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_serving_lexical_tsv
                 ON {} USING GIN (tsv)",
                self.schema,
                self.table("serving_lexical")
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_serving_lexical_snapshot_id
                 ON {} (snapshot_id)",
                self.schema,
                self.table("serving_lexical")
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_file_objects_fingerprint
                 ON {} (collection, file_fingerprint)",
                self.schema,
                self.table("file_objects")
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_file_object_items_object
                 ON {} (file_object_id, ordinal)",
                self.schema,
                self.table("file_object_items")
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_semantic_embeddings_model_dim
                 ON {} (model_id, dimension)",
                self.schema,
                self.table("semantic_embeddings")
            ),
        ]);
        statements
    }

    fn pgvector_schema_statements(&self) -> Vec<String> {
        vec![
            "CREATE EXTENSION IF NOT EXISTS vector".to_owned(),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    snapshot_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE,
                    collection TEXT NOT NULL,
                    root_id TEXT NOT NULL DEFAULT '',
                    path TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    symbol_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER NOT NULL,
                    model_id TEXT NOT NULL,
                    dimension INTEGER NOT NULL,
                    embedding vector NOT NULL,
                    PRIMARY KEY (snapshot_id, model_id, collection, root_id, path, ordinal)
                )",
                self.table("serving_semantic"),
                self.table("snapshots"),
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_serving_semantic_snapshot_model
                 ON {} (snapshot_id, model_id)",
                self.schema,
                self.table("serving_semantic")
            ),
        ]
    }

    pub fn list_snapshots(
        &self,
        corpus: Option<&str>,
        branch: Option<&str>,
        commit: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BaselineSnapshotRecord>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let limit = limit.clamp(1, 200) as i64;
        let corpus = corpus.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let branch = branch.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let commit = commit.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);

        let mut query = format!(
            "SELECT s.id,
                    s.corpus,
                    s.fingerprint,
                    s.parent_snapshot_id,
                    s.branch,
                    s.commit_sha,
                    s.created_at::TEXT AS created_at
             FROM {} s
             WHERE 1 = 1",
            self.table("snapshots"),
        );

        let mut params = Vec::<&(dyn postgres::types::ToSql + Sync)>::new();
        if let Some(corpus) = corpus.as_ref() {
            query.push_str(&format!(" AND s.corpus = ${}", params.len() + 1));
            params.push(corpus);
        }
        if let Some(branch) = branch.as_ref() {
            query.push_str(&format!(" AND s.branch = ${}", params.len() + 1));
            params.push(branch);
        }
        if let Some(commit) = commit.as_ref() {
            query.push_str(&format!(" AND s.commit_sha = ${}", params.len() + 1));
            params.push(commit);
        }
        query.push_str(" ORDER BY s.created_at DESC");
        query.push_str(&format!(" LIMIT ${}", params.len() + 1));
        params.push(&limit);

        let rows = client.query(&query, &params)?;
        let mut snapshots: Vec<BaselineSnapshotRecord> =
            rows.into_iter().map(snapshot_record_from_metadata_row).collect();

        // Effective file/document totals for every listed snapshot in one round-trip, rather than
        // walking each snapshot's ancestry and summarising it separately.
        let seed_ids: Vec<String> = snapshots.iter().map(|s| s.snapshot_id.clone()).collect();
        let totals = effective_snapshot_totals_batch(&mut *client, self, &seed_ids)?;
        for snapshot in &mut snapshots {
            let (files, documents) = totals.get(&snapshot.snapshot_id).copied().unwrap_or((0, 0));
            snapshot.files = files;
            snapshot.documents = documents;
        }
        Ok(snapshots)
    }

    pub fn snapshot_details(
        &self,
        snapshot_id: &str,
    ) -> Result<Option<BaselineSnapshotDetails>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;

        let summary_query = format!(
            "SELECT s.id,
                    s.corpus,
                    s.fingerprint,
                    s.parent_snapshot_id,
                    s.branch,
                    s.commit_sha,
                    s.created_at::TEXT AS created_at,
                    model.value AS semantic_model,
                    dimension.value AS semantic_dimension,
                    publication.value AS semantic_publication
             FROM {} s
             LEFT JOIN {metadata} model ON model.setting = '{EMBEDDING_MODEL_SETTING}'
             LEFT JOIN {metadata} dimension ON dimension.setting = '{EMBEDDING_DIMENSION_SETTING}'
             LEFT JOIN {metadata} publication ON publication.setting =
                 '{SEMANTIC_PUBLICATION_COMPLETE_PREFIX}' || s.id || ':' || model.value || ':' || dimension.value
             WHERE s.id = $1
             LIMIT 1",
            self.table("snapshots"),
            metadata = self.table(SCHEMA_METADATA_TABLE),
        );
        let Some(snapshot_row) = client.query_opt(&summary_query, &[&snapshot_id])? else {
            return Ok(None);
        };
        let model: Option<String> = snapshot_row.get("semantic_model");
        let dimension: Option<String> = snapshot_row.get("semantic_dimension");
        let publication: Option<String> = snapshot_row.get("semantic_publication");
        let semantic_publication =
            model.filter(|model| !model.trim().is_empty()).and_then(|model_id| {
                let raw_dimension = dimension?;
                let dimension = raw_dimension.parse::<usize>().ok()?;
                // The publisher uses canonical positive dimensions in marker keys.
                (dimension > 0 && dimension.to_string() == raw_dimension).then_some(
                    BaselineSemanticPublication {
                        model_id,
                        dimension,
                        complete: publication.as_deref() == Some("complete"),
                    },
                )
            });
        let mut snapshot = snapshot_record_from_metadata_row(snapshot_row);
        let summary = effective_snapshot_summary(&mut *client, self, snapshot_id)?;
        snapshot.files = summary.total_files;
        snapshot.documents = summary.total_documents;
        let collections = summary.collections;

        Ok(Some(BaselineSnapshotDetails { snapshot, collections, semantic_publication }))
    }

    pub fn list_file_objects(
        &self,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BaselineFileObjectRecord>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let limit = limit.clamp(1, 200) as i64;
        let collection =
            collection.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);

        let mut query = format!(
            "SELECT fo.id,
                    fo.collection,
                    fo.file_fingerprint,
                    fo.document_count,
                    COUNT(DISTINCT sf.snapshot_id) AS snapshots
             FROM {} fo
             LEFT JOIN {} sf ON sf.file_object_id = fo.id
             WHERE 1 = 1",
            self.table("file_objects"),
            self.table("snapshot_files"),
        );
        let mut params = Vec::<&(dyn postgres::types::ToSql + Sync)>::new();
        if let Some(collection) = collection.as_ref() {
            query.push_str(&format!(" AND fo.collection = ${}", params.len() + 1));
            params.push(collection);
        }
        query.push_str(
            " GROUP BY fo.id, fo.collection, fo.file_fingerprint, fo.document_count
              ORDER BY snapshots DESC, fo.collection, fo.id",
        );
        query.push_str(&format!(" LIMIT ${}", params.len() + 1));
        params.push(&limit);

        Ok(client.query(&query, &params)?.into_iter().map(file_object_record_from_row).collect())
    }

    pub fn file_object_details(
        &self,
        file_object_id: &str,
    ) -> Result<Option<BaselineFileObjectDetails>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let summary_query = format!(
            "SELECT fo.id,
                    fo.collection,
                    fo.file_fingerprint,
                    fo.document_count,
                    COUNT(DISTINCT sf.snapshot_id) AS snapshots
             FROM {} fo
             LEFT JOIN {} sf ON sf.file_object_id = fo.id
             WHERE fo.id = $1
             GROUP BY fo.id, fo.collection, fo.file_fingerprint, fo.document_count
             LIMIT 1",
            self.table("file_objects"),
            self.table("snapshot_files"),
        );
        let Some(file_object_row) = client.query_opt(&summary_query, &[&file_object_id])? else {
            return Ok(None);
        };

        let references_query = format!(
            "SELECT snapshot_id, root_id, path
             FROM {}
             WHERE file_object_id = $1
             ORDER BY snapshot_id, root_id, path",
            self.table("snapshot_files")
        );
        let references = client
            .query(&references_query, &[&file_object_id])?
            .into_iter()
            .map(|row| BaselineFileObjectReference {
                snapshot_id: row.get("snapshot_id"),
                root_id: row.get("root_id"),
                path: row.get("path"),
            })
            .collect();

        Ok(Some(BaselineFileObjectDetails {
            file_object: file_object_record_from_row(file_object_row),
            references,
        }))
    }

    pub fn store_embeddings(
        &self,
        model_id: &str,
        dimension: usize,
        embeddings: &[(String, Vec<f32>)],
    ) -> Result<BaselineEmbeddingStats, SearchError> {
        self.check_embedding_claim_model(model_id)?;
        self.check_storage_readiness()?;
        if embeddings.is_empty() {
            return Ok(BaselineEmbeddingStats { stored: 0, reused: 0 });
        }

        let mut client = self.connect()?;
        let mut tx = client.transaction()?;
        let insert = format!(
            "INSERT INTO {} (embedding_key, model_id, dimension, embedding)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (embedding_key, model_id, dimension) DO NOTHING",
            self.table("semantic_embeddings")
        );

        let mut stored = 0usize;
        let mut seen = HashSet::new();
        for (embedding_key, embedding) in embeddings {
            if !seen.insert(embedding_key.as_str()) {
                continue;
            }
            let blob: Vec<u8> = embedding.iter().flat_map(|value| value.to_le_bytes()).collect();
            let inserted =
                tx.execute(&insert, &[&embedding_key, &model_id, &(dimension as i32), &blob])?;
            stored += inserted as usize;
        }

        tx.commit()?;
        Ok(BaselineEmbeddingStats { stored, reused: seen.len().saturating_sub(stored) })
    }

    pub fn load_embeddings(
        &self,
        embedding_keys: &[String],
        model_id: &str,
        dimension: usize,
    ) -> Result<HashMap<String, Vec<f32>>, SearchError> {
        self.check_embedding_claim_model(model_id)?;
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        load_embeddings_from_client(&mut *client, self, embedding_keys, model_id, dimension)
    }

    pub fn list_embedding_models(
        &self,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Vec<BaselineEmbeddingModelRecord>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let model_id =
            model_id.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let dimension = dimension.map(|value| value as i32);

        let mut query = format!(
            "SELECT model_id, dimension, COUNT(*) AS embeddings
             FROM {}
             WHERE 1 = 1",
            self.table("semantic_embeddings")
        );
        let mut params = Vec::<&(dyn postgres::types::ToSql + Sync)>::new();
        if let Some(model_id) = model_id.as_ref() {
            query.push_str(&format!(" AND model_id = ${}", params.len() + 1));
            params.push(model_id);
        }
        if let Some(dimension) = dimension.as_ref() {
            query.push_str(&format!(" AND dimension = ${}", params.len() + 1));
            params.push(dimension);
        }
        query.push_str(" GROUP BY model_id, dimension ORDER BY model_id, dimension");

        Ok(client
            .query(&query, &params)?
            .into_iter()
            .map(|row| BaselineEmbeddingModelRecord {
                model_id: row.get("model_id"),
                dimension: row.get::<_, i32>("dimension") as usize,
                embeddings: row.get::<_, i64>("embeddings") as usize,
            })
            .collect())
    }

    pub fn embedding_coverage(
        &self,
        model_id: Option<&str>,
        dimension: Option<usize>,
    ) -> Result<Vec<BaselineEmbeddingCoverageRecord>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let active_keys = collect_active_embedding_keys(&mut *client, self)?;
        let active_payloads = active_keys.len();
        let models = self.list_embedding_models(model_id, dimension)?;
        if models.is_empty() {
            return Ok(Vec::new());
        }

        let model_id =
            model_id.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let dimension = dimension.map(|value| value as i32);
        let mut query = format!(
            "SELECT embedding_key, model_id, dimension
             FROM {}
             WHERE 1 = 1",
            self.table("semantic_embeddings")
        );
        let mut params = Vec::<&(dyn postgres::types::ToSql + Sync)>::new();
        if let Some(model_id) = model_id.as_ref() {
            query.push_str(&format!(" AND model_id = ${}", params.len() + 1));
            params.push(model_id);
        }
        if let Some(dimension) = dimension.as_ref() {
            query.push_str(&format!(" AND dimension = ${}", params.len() + 1));
            params.push(dimension);
        }

        let mut covered = HashMap::<(String, usize), HashSet<String>>::new();
        for row in client.query(&query, &params)? {
            let embedding_key: String = row.get("embedding_key");
            if !active_keys.contains(&embedding_key) {
                continue;
            }
            let model_id: String = row.get("model_id");
            let dimension = row.get::<_, i32>("dimension") as usize;
            covered.entry((model_id, dimension)).or_default().insert(embedding_key);
        }

        Ok(models
            .into_iter()
            .map(|model| {
                let covered_payloads = covered
                    .remove(&(model.model_id.clone(), model.dimension))
                    .map(|keys| keys.len())
                    .unwrap_or(0);
                BaselineEmbeddingCoverageRecord {
                    model_id: model.model_id,
                    dimension: model.dimension,
                    active_payloads,
                    covered_payloads,
                    embeddings: model.embeddings,
                }
            })
            .collect())
    }

    pub fn garbage_collect(&self, execute: bool) -> Result<BaselineGcReport, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let active_keys = collect_active_embedding_keys(&mut *client, self)?;

        let orphan_file_object_ids = query_string_column(
            &mut *client,
            &format!(
                "SELECT fo.id
                 FROM {} fo
                 LEFT JOIN {} sf ON sf.file_object_id = fo.id
                 WHERE sf.file_object_id IS NULL
                 ORDER BY fo.id",
                self.table("file_objects"),
                self.table("snapshot_files"),
            ),
            &[],
        )?;

        let orphan_file_object_item_rows = client.query(
            &format!(
                "SELECT foi.file_object_id
                 FROM {} foi
                 LEFT JOIN {} fo ON fo.id = foi.file_object_id
                 WHERE fo.id IS NULL
                 ORDER BY foi.file_object_id, foi.ordinal",
                self.table("file_object_items"),
                self.table("file_objects"),
            ),
            &[],
        )?;
        let orphan_file_object_items = orphan_file_object_item_rows.len();
        let orphan_file_object_item_ids = orphan_file_object_item_rows
            .into_iter()
            .map(|row| row.get::<_, String>("file_object_id"))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let orphan_semantic_embedding_rows = client.query(
            &format!(
                "SELECT embedding_key
                 FROM {}
                 ORDER BY embedding_key",
                self.table("semantic_embeddings")
            ),
            &[],
        )?;
        let orphan_semantic_embeddings = orphan_semantic_embedding_rows
            .iter()
            .filter(|row| !active_keys.contains(row.get::<_, String>("embedding_key").as_str()))
            .count();
        let orphan_semantic_embedding_keys = orphan_semantic_embedding_rows
            .into_iter()
            .map(|row| row.get::<_, String>("embedding_key"))
            .filter(|key| !active_keys.contains(key))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let mut report = BaselineGcReport {
            orphan_file_objects: orphan_file_object_ids.len(),
            orphan_file_object_items,
            orphan_semantic_embeddings,
            deleted_file_objects: 0,
            deleted_file_object_items: 0,
            deleted_semantic_embeddings: 0,
        };

        if !execute {
            return Ok(report);
        }

        let mut tx = client.transaction()?;
        if !orphan_file_object_item_ids.is_empty() {
            let delete = format!(
                "DELETE FROM {}
                 WHERE file_object_id = ANY($1)",
                self.table("file_object_items")
            );
            report.deleted_file_object_items =
                tx.execute(&delete, &[&orphan_file_object_item_ids])? as usize;
        }
        if !orphan_file_object_ids.is_empty() {
            let delete = format!(
                "DELETE FROM {}
                 WHERE id = ANY($1)",
                self.table("file_objects")
            );
            report.deleted_file_objects = tx.execute(&delete, &[&orphan_file_object_ids])? as usize;
        }
        if !orphan_semantic_embedding_keys.is_empty() {
            let delete = format!(
                "DELETE FROM {}
                 WHERE embedding_key = ANY($1)",
                self.table("semantic_embeddings")
            );
            report.deleted_semantic_embeddings =
                tx.execute(&delete, &[&orphan_semantic_embedding_keys])? as usize;
        }
        tx.commit()?;

        Ok(report)
    }

    pub fn populate_serving_semantic(
        &self,
        snapshot_id: &str,
        model_id: &str,
        dimension: usize,
    ) -> Result<usize, SearchError> {
        self.populate_serving_semantic_with_progress(snapshot_id, model_id, dimension, None)
    }

    pub fn populate_serving_semantic_with_progress(
        &self,
        snapshot_id: &str,
        model_id: &str,
        dimension: usize,
        progress: Option<&dyn Fn(SemanticPublishProgress)>,
    ) -> Result<usize, SearchError> {
        self.check_embedding_claim_model(model_id)?;
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        if !self.storage_table_exists(&mut *client, "serving_semantic")? {
            return Err(SearchError::ExternalBaseline(
                "serving_semantic_unavailable: semantic serving table is not initialized"
                    .to_owned(),
            ));
        }

        let planning_started = Instant::now();
        // Everything the plan is decided from is read in ONE snapshot of the data, explicitly at
        // REPEATABLE READ. Under the server default of READ COMMITTED each statement takes its
        // own snapshot, so "the same transaction" would guarantee nothing: a concurrent
        // republish of this very snapshot id — legal, `ON CONFLICT` on the id — could commit
        // between the parent lookup and the file rows, and the plan would describe a corpus that
        // never existed.
        let mut tx = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .start()?;
        #[cfg(test)]
        observe_planning_isolation(&mut tx)?;

        let parent_snapshot_id = snapshot_parent_id(&mut tx, self, snapshot_id)?;
        let current_snapshot_files = load_snapshot_file_rows(&mut tx, self, snapshot_id)?;
        let deleted_paths = load_snapshot_deletion_keys(&mut tx, self, snapshot_id)?;
        let parent_complete = match parent_snapshot_id.as_deref() {
            Some(parent_snapshot_id) => semantic_publication_complete(
                &mut tx,
                self,
                parent_snapshot_id,
                model_id,
                dimension,
            )?,
            None => false,
        };
        let plan = semantic_publish_plan(
            parent_snapshot_id.clone(),
            parent_complete,
            &current_snapshot_files,
            &deleted_paths,
        );
        // Which snapshots the plan is READ FROM, decided by the strategy: a full rebuild folds
        // the whole ancestry into its visible set, an incremental copy reads exactly one parent,
        // and a standalone publish reads nobody but itself. The final transaction locks this set
        // and re-checks it, so lock and check answer for the same snapshots by construction.
        let mut plan_dependencies = vec![snapshot_id.to_owned()];
        match &plan.strategy {
            SemanticPublishStrategy::FullRebuild => {
                plan_dependencies = snapshot_ancestry_ids(&mut tx, self, snapshot_id)?;
            }
            SemanticPublishStrategy::IncrementalFromParent { parent_snapshot_id } => {
                plan_dependencies.push(parent_snapshot_id.clone());
            }
            SemanticPublishStrategy::CurrentSnapshotOnly => {}
        }
        plan_dependencies.sort();
        plan_dependencies.dedup();

        // Materialized here, inside the same snapshot, and only for the strategy that needs it:
        // reading it later — after row preparation — would put it in a different snapshot than
        // the plan it belongs to, which is the whole reason this transaction exists.
        let materialization_started = Instant::now();
        let visible_files = match plan.strategy {
            SemanticPublishStrategy::FullRebuild => {
                Some(materialize_visible_snapshot_files(&mut tx, self, snapshot_id)?)
            }
            _ => None,
        };
        let ancestry_materialization = materialization_started.elapsed();
        // The version of every snapshot row the plan was read from, captured before the
        // transaction that read them ends and compared again under the final lock. A republish
        // that commits in between is invisible to that lock — it orders only republishes that
        // have not committed yet — and everything computed after this point describes a corpus
        // that no longer exists.
        //
        // A parent republished in that window is the WORSE case, not a lesser one: its own
        // invalidation clears its rows and never touches ours, so the copy silently brings
        // nothing and the gap is sealed under our completeness mark, with nothing left to
        // correct it later.
        let planned_versions = snapshot_row_versions(&mut tx, self, &plan_dependencies)?;
        tx.commit()?;
        let strategy = plan.strategy.clone();
        let phase_count = semantic_publish_phase_count(&strategy);
        if let Some(on_progress) = progress {
            on_progress(SemanticPublishProgress::Plan {
                strategy: strategy.label().to_owned(),
                changed_files: plan.changed_paths.len(),
                deleted_paths: plan.deleted_paths.len(),
                parent_snapshot_id: strategy.parent_snapshot_id().map(str::to_owned),
                phase_count,
            });
        }

        let mut timings = SemanticPublishPhaseTimings {
            ancestry_materialization: planning_started.elapsed(),
            ..SemanticPublishPhaseTimings::default()
        };

        let (prepared_rows, missing_embeddings) = match &strategy {
            SemanticPublishStrategy::CurrentSnapshotOnly => {
                let recompute_started = Instant::now();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseStarted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        detail: format!(
                            "Preparing semantic rows for {} changed files",
                            plan.changed_paths.len()
                        ),
                    });
                }
                let prepared = prepare_semantic_rows_for_files(
                    &mut *client,
                    self,
                    &current_snapshot_files,
                    model_id,
                    dimension,
                )?;
                timings.changed_recompute = recompute_started.elapsed();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseCompleted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        elapsed: timings.changed_recompute,
                        output_rows: prepared.rows.len(),
                    });
                }
                (prepared.rows, prepared.missing_embeddings)
            }
            SemanticPublishStrategy::IncrementalFromParent { .. } => {
                let recompute_started = Instant::now();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseStarted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        detail: format!(
                            "Preparing semantic rows for {} changed files",
                            plan.changed_paths.len()
                        ),
                    });
                }
                let prepared = prepare_semantic_rows_for_files(
                    &mut *client,
                    self,
                    &current_snapshot_files,
                    model_id,
                    dimension,
                )?;
                timings.changed_recompute = recompute_started.elapsed();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseCompleted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        elapsed: timings.changed_recompute,
                        output_rows: prepared.rows.len(),
                    });
                }
                (prepared.rows, prepared.missing_embeddings)
            }
            SemanticPublishStrategy::FullRebuild => {
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseStarted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        detail: "Recomputing semantic rows for the full visible snapshot"
                            .to_owned(),
                    });
                }
                let visible_files = visible_files
                    .expect("the full rebuild strategy materializes its files while planning");
                // Measured where the work happens now — in planning — instead of around the
                // `expect` that merely picks it up, which would report ~0 for the one strategy
                // whose materialization this field exists to diagnose.
                timings.ancestry_materialization = ancestry_materialization;

                let recompute_started = Instant::now();
                let prepared = prepare_semantic_rows_for_files(
                    &mut *client,
                    self,
                    &visible_files,
                    model_id,
                    dimension,
                )?;
                timings.changed_recompute = recompute_started.elapsed();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseCompleted {
                        phase: SemanticPublishPhase::PrepareRows,
                        phase_index: 1,
                        phase_count,
                        elapsed: timings.ancestry_materialization + timings.changed_recompute,
                        output_rows: prepared.rows.len(),
                    });
                }
                (prepared.rows, prepared.missing_embeddings)
            }
        };

        let changed_rows = prepared_rows.len();
        let changed_files = plan.changed_paths.len();
        let final_sync_started = Instant::now();
        let mut tx = client.transaction()?;
        // Serialized against a concurrent republish. `publish_snapshot` upserts a snapshot's row
        // as the first write of its own transaction and then clears that snapshot's semantics,
        // so holding these rows orders the two operations: a republish that has not committed
        // waits for us instead of clearing rows out from under the completeness mark we are
        // about to write, which would leave the corpus marked complete over semantics of a
        // version it no longer has.
        //
        // Exactly the snapshots the PLAN was read from. Always this one — its own file rows and
        // its own semantics are what everything here is built on — plus, for a full rebuild, the
        // ancestry it folded into its visible set, or, for an incremental copy, the one parent
        // whose rows are carried forward below.
        //
        // Not the ancestry as it stands NOW, which is a different set: the rows copied below
        // belong to the parent the plan chose, not to whoever this snapshot's parent has become
        // since planning.
        //
        // Locked in id order so two publications sharing a parent queue instead of deadlocking.
        tx.execute(
            &format!(
                "SELECT id FROM {} WHERE id = ANY($1) ORDER BY id FOR UPDATE",
                self.table("snapshots")
            ),
            &[&plan_dependencies],
        )?;
        // Asked again now that the rows are held: from here on nobody can republish any of them
        // until we commit, so versions equal to the planned ones mean the plan still describes
        // what is in the tables it was read from.
        let current_versions = snapshot_row_versions(&mut tx, self, &plan_dependencies)?;
        // Absence counts as movement, and it is reported as ITSELF. Two identical absences would
        // otherwise compare equal and let a plan read from a snapshot that was already gone sail
        // through; and an operator told "republished" about a snapshot that is simply not there
        // would go looking through the catalogue for a publication that never happened.
        let moved = plan_dependencies.iter().find_map(|id| {
            match (planned_versions.get(id), current_versions.get(id)) {
                (Some(planned), Some(current)) if planned == current => None,
                (Some(_), Some(_)) => Some((id, "was republished")),
                (Some(_), None) => Some((id, "was deleted")),
                (None, _) => Some((id, "was already gone when the plan was read")),
            }
        });
        if let Some((moved_snapshot, what_happened)) = moved {
            return Err(SearchError::named(
                reason::SNAPSHOT_REPUBLISHED_WHILE_PUBLISHING,
                format!(
                    "snapshot '{moved_snapshot}' {what_happened} while the semantics of \
                     '{snapshot_id}' were being computed; nothing was written, publish them again"
                ),
            ));
        }
        clear_semantic_publication_complete(&mut tx, self, snapshot_id, model_id, dimension)?;
        delete_serving_semantic_rows(&mut tx, self, snapshot_id, model_id, dimension)?;

        let copied_rows = match &strategy {
            SemanticPublishStrategy::IncrementalFromParent { parent_snapshot_id } => {
                let copy_started = Instant::now();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseStarted {
                        phase: SemanticPublishPhase::CopyParentRows,
                        phase_index: 2,
                        phase_count,
                        detail: format!(
                            "Copying unchanged rows from parent snapshot {}",
                            parent_snapshot_id
                        ),
                    });
                }
                let copied = copy_parent_serving_semantic_rows(
                    &mut tx,
                    self,
                    snapshot_id,
                    parent_snapshot_id,
                    model_id,
                    dimension,
                )?;
                timings.parent_copy = copy_started.elapsed();
                if let Some(on_progress) = progress {
                    on_progress(SemanticPublishProgress::PhaseCompleted {
                        phase: SemanticPublishPhase::CopyParentRows,
                        phase_index: 2,
                        phase_count,
                        elapsed: timings.parent_copy,
                        output_rows: copied,
                    });
                }
                copied
            }
            _ => 0,
        };
        if let Some(on_progress) = progress {
            on_progress(SemanticPublishProgress::PhaseStarted {
                phase: SemanticPublishPhase::WriteServingRows,
                phase_index: phase_count,
                phase_count,
                detail: format!(
                    "Writing {} changed rows into serving_semantic",
                    prepared_rows.len()
                ),
            });
        }
        let inserted_rows = insert_serving_semantic_rows(
            &mut tx,
            self,
            snapshot_id,
            model_id,
            dimension,
            &prepared_rows,
        )?;
        if missing_embeddings == 0 {
            mark_semantic_publication_complete(&mut tx, self, snapshot_id, model_id, dimension)?;
        }
        tx.commit()?;
        timings.final_sync = final_sync_started.elapsed();
        if let Some(on_progress) = progress {
            on_progress(SemanticPublishProgress::PhaseCompleted {
                phase: SemanticPublishPhase::WriteServingRows,
                phase_index: phase_count,
                phase_count,
                elapsed: timings.final_sync,
                output_rows: inserted_rows,
            });
            on_progress(SemanticPublishProgress::Completed {
                total_rows: copied_rows + inserted_rows,
                copied_rows,
                inserted_rows,
                missing_embeddings,
                total_elapsed: planning_started.elapsed(),
            });
        }

        log_semantic_publish_phase_timings(
            snapshot_id,
            model_id,
            dimension,
            &strategy,
            &timings,
            &SemanticPublishLogStats {
                copied_rows,
                inserted_rows,
                missing_embeddings,
                changed_files,
                changed_rows,
                deleted_paths: plan.deleted_paths.len(),
            },
        );
        Ok(copied_rows + inserted_rows)
    }
}

impl SnapshotCatalog for PostgresBaselineAdapter {
    fn resolve_baseline(&self, baseline: &BaselineRef) -> Result<Option<Snapshot>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let corpus = baseline.corpus.as_str();

        let row = if let Some(snapshot_id) = &baseline.snapshot_id {
            let query = format!(
                "SELECT id, corpus, fingerprint, parent_snapshot_id
                 FROM {}
                 WHERE id = $1 AND corpus = $2
                 LIMIT 1",
                self.table("snapshots")
            );
            client.query_opt(&query, &[&snapshot_id.0, &corpus])?
        } else if let (Some(branch), Some(commit)) = (&baseline.branch, &baseline.commit) {
            let query =
                self.latest_snapshot_query("corpus = $1 AND branch = $2 AND commit_sha = $3");
            client.query_opt(&query, &[&corpus, branch, commit])?
        } else if let Some(branch) = &baseline.branch {
            let head_query = format!(
                "SELECT s.id, s.corpus, s.fingerprint, s.parent_snapshot_id
                 FROM {} h
                 JOIN {} s ON s.id = h.snapshot_id
                 WHERE h.corpus = $1 AND h.branch = $2
                 LIMIT 1",
                self.table("snapshot_heads"),
                self.table("snapshots"),
            );
            let row = client.query_opt(&head_query, &[&corpus, branch]).ok().flatten();
            if row.is_some() {
                row
            } else {
                let fallback = self.latest_snapshot_query("corpus = $1 AND branch = $2");
                client.query_opt(&fallback, &[&corpus, branch])?
            }
        } else if let Some(commit) = &baseline.commit {
            let query = self.latest_snapshot_query("corpus = $1 AND commit_sha = $2");
            client.query_opt(&query, &[&corpus, commit])?
        } else {
            let query = self.latest_snapshot_query("corpus = $1");
            client.query_opt(&query, &[&corpus])?
        };

        Ok(row.map(Self::snapshot_row_to_model))
    }
}

impl SnapshotContentStore for PostgresBaselineAdapter {
    fn load_snapshot_documents(
        &self,
        snapshot: &Snapshot,
    ) -> Result<Vec<IndexedDocument>, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;
        let visible_files = materialize_visible_snapshot_files(&mut *client, self, &snapshot.id.0)?;
        if visible_files.is_empty() {
            return Ok(Vec::new());
        }

        let file_object_ids =
            visible_files.iter().map(|file| file.file_object_id.clone()).collect::<Vec<_>>();
        let items_query = if self.token_layout_claim.is_some() {
            format!(
            "SELECT foi.file_object_id,
                    foi.ordinal,
                    foi.symbol_name,
                    foi.kind,
                    foi.line_start,
                    foi.line_end,
                    co.text,
                    foi.content_hash,
                    foi.graph_context,
                    part.parent_symbol,
                    part.byte_start,
                    part.byte_end,
                    part.parent_byte_start,
                    part.parent_byte_end,
                    part.part_index,
                    part.part_count
             FROM {} foi
             JOIN {} co ON co.content_hash = foi.content_hash
             LEFT JOIN {} part ON part.file_object_id = foi.file_object_id AND part.ordinal = foi.ordinal
             WHERE foi.file_object_id = ANY($1)
             ORDER BY foi.file_object_id, foi.ordinal",
            self.table("file_object_items"),
            self.table("content_objects"),
            self.table(TOKEN_LAYOUT_PARTS_TABLE)
            )
        } else {
            format!(
                "SELECT foi.file_object_id,
                    foi.ordinal,
                    foi.symbol_name,
                    foi.kind,
                    foi.line_start,
                    foi.line_end,
                    co.text,
                    foi.content_hash,
                    foi.graph_context
             FROM {} foi
             JOIN {} co ON co.content_hash = foi.content_hash
             WHERE foi.file_object_id = ANY($1)
             ORDER BY foi.file_object_id, foi.ordinal",
                self.table("file_object_items"),
                self.table("content_objects")
            )
        };
        // Publication requires a span only for code documents; other collections keep theirs
        // when one was published and otherwise read back without provenance.
        let code_file_objects = visible_files
            .iter()
            .filter(|file| file.collection == "code")
            .map(|file| file.file_object_id.as_str())
            .collect::<HashSet<_>>();
        let mut items_by_file_object = HashMap::<String, Vec<FileObjectItem>>::new();
        for row in client.query(&items_query, &[&file_object_ids])? {
            let file_object_id: String = row.get("file_object_id");
            let source_span = if self.token_layout_claim.is_none() {
                None
            } else if code_file_objects.contains(file_object_id.as_str())
                || row.try_get::<_, Option<String>>("parent_symbol")?.is_some()
            {
                Some(source_span_from_row(&row)?)
            } else {
                None
            };
            items_by_file_object.entry(file_object_id).or_default().push(FileObjectItem {
                symbol_name: row.get("symbol_name"),
                kind: row.get("kind"),
                line_start: row.get::<_, i32>("line_start") as u32,
                line_end: row.get::<_, i32>("line_end") as u32,
                text: row.get("text"),
                content_hash: row.get("content_hash"),
                graph_context: row.get("graph_context"),
                source_span,
            });
        }

        let mut documents = Vec::new();
        for file in visible_files {
            if let Some(items) = items_by_file_object.get(&file.file_object_id) {
                for item in items {
                    documents.push(IndexedDocument {
                        collection: file.collection.clone(),
                        root_id: file.root_id.clone(),
                        path: file.path.clone(),
                        symbol_name: item.symbol_name.clone(),
                        kind: item.kind.clone(),
                        line_start: item.line_start,
                        line_end: item.line_end,
                        text: item.text.clone(),
                        content_hash: item.content_hash.clone(),
                        graph_context: item.graph_context.clone(),
                        source_span: item.source_span.clone(),
                    });
                }
            }
        }
        documents.sort_by(|lhs, rhs| {
            (
                lhs.collection.as_str(),
                lhs.path.as_str(),
                lhs.line_start,
                lhs.line_end,
                lhs.symbol_name.as_str(),
            )
                .cmp(&(
                    rhs.collection.as_str(),
                    rhs.path.as_str(),
                    rhs.line_start,
                    rhs.line_end,
                    rhs.symbol_name.as_str(),
                ))
        });
        if self.token_layout_claim.is_some() {
            crate::publish::validate_source_span_layout(&documents, true)?;
        }
        Ok(documents)
    }
}

type ServingRowKey = (String, String, String, i32);

fn serving_row_key(row: &Row) -> ServingRowKey {
    (row.get("collection"), row.get("root_id"), row.get("path"), row.get("ordinal"))
}

fn source_spans_for_serving_rows(
    adapter: &PostgresBaselineAdapter,
    client: &mut impl GenericClient,
    snapshot_id: &str,
    rows: &[Row],
) -> Result<HashMap<ServingRowKey, SourceSpan>, SearchError> {
    if adapter.token_layout_claim.is_none() || rows.is_empty() {
        return Ok(HashMap::new());
    }
    let visible = materialize_visible_snapshot_file_map(client, adapter, snapshot_id)?;
    let query = format!(
        "SELECT parent_symbol, byte_start, byte_end, parent_byte_start,
                parent_byte_end, part_index, part_count
         FROM {} WHERE file_object_id = $1 AND ordinal = $2",
        adapter.table(TOKEN_LAYOUT_PARTS_TABLE)
    );
    let mut spans = HashMap::new();
    for row in rows {
        let key = serving_row_key(row);
        if key.0 != "code" {
            continue;
        }
        let file =
            visible.get(&(key.0.clone(), key.1.clone(), key.2.clone())).ok_or_else(|| {
                SearchError::ExternalBaseline(
                    "token_layout_provenance_missing: serving row has no visible file object"
                        .to_owned(),
                )
            })?;
        let part = client.query_opt(&query, &[&file.file_object_id, &key.3])?.ok_or_else(|| {
            SearchError::ExternalBaseline(
                "token_layout_provenance_missing: serving document has no part metadata".to_owned(),
            )
        })?;
        spans.insert(key, source_span_from_row(&part)?);
    }
    Ok(spans)
}

impl BaselineLexicalSearch for PostgresBaselineAdapter {
    fn lexical_search_baseline(
        &self,
        snapshot_id: &str,
        query: &str,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LexicalHit>, SearchError> {
        // OR the per-term `plainto_tsquery`s instead of passing the whole query to a single
        // `plainto_tsquery` (which ANDs every lexeme): a multi-word query must surface a chunk
        // that contains any term, with `ts_rank` lifting chunks that contain more of them. Each
        // term is a bound parameter, so no tsquery operator can be injected.
        let terms: Vec<String> =
            crate::lexical::query_terms(query).into_iter().map(str::to_owned).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        self.check_storage_readiness()?;
        let collection =
            collection.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let limit = limit.clamp(1, 200) as i64;
        let mut client = self.connect()?;

        let snapshot_id = snapshot_id.to_owned();
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&snapshot_id];
        let tsquery_expr = format!(
            "({})",
            terms
                .iter()
                .map(|term| {
                    params.push(term);
                    format!("plainto_tsquery('simple', ${})", params.len())
                })
                .collect::<Vec<_>>()
                .join(" || ")
        );
        let mut sql = format!(
            "SELECT collection,
                    root_id,
                    path,
                    ordinal,
                    symbol_name,
                    kind,
                    line_start,
                    line_end,
                    text,
                    ts_rank(tsv, {tsquery_expr}, 32) AS rank
             FROM {}
             WHERE snapshot_id = $1
               AND tsv @@ {tsquery_expr}",
            self.table("serving_lexical")
        );

        if let Some(collection) = collection.as_ref() {
            sql.push_str(&format!(" AND collection = ${}", params.len() + 1));
            params.push(collection);
        }
        sql.push_str(" ORDER BY rank DESC, collection, root_id, path, ordinal");
        sql.push_str(&format!(" LIMIT ${}", params.len() + 1));
        params.push(&limit);

        let rows = client.query(&sql, &params)?;
        if rows.is_empty()
            && !self.lexical_serving_rows_exist(
                &mut *client,
                &snapshot_id,
                collection.as_deref(),
            )?
        {
            return Err(SearchError::ExternalBaseline(
                "serving_lexical_unavailable: snapshot has no serving rows for lexical search"
                    .to_owned(),
            ));
        }

        let source_spans = source_spans_for_serving_rows(self, &mut *client, &snapshot_id, &rows)?;
        Ok(rows
            .into_iter()
            .map(|row| LexicalHit {
                source_span: source_spans.get(&serving_row_key(&row)).cloned(),
                collection: row.get("collection"),
                root_id: row.get("root_id"),
                path: row.get("path"),
                symbol_name: row.get("symbol_name"),
                kind: row.get("kind"),
                line_start: row.get::<_, i32>("line_start") as u32,
                line_end: row.get::<_, i32>("line_end") as u32,
                text: row.get("text"),
                rank: row.get::<_, f32>("rank"),
            })
            .collect())
    }
}

impl BaselineSemanticSearch for PostgresBaselineAdapter {
    fn semantic_search_baseline(
        &self,
        snapshot_id: &str,
        query_embedding: &[f32],
        model_id: &str,
        dimension: usize,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SemanticHit>, SearchError> {
        if query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        self.check_embedding_claim_model(model_id)?;
        self.check_storage_readiness()?;
        let collection =
            collection.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned);
        let limit = limit.clamp(1, 200) as i64;
        let dimension = dimension as i32;
        let snapshot_id = snapshot_id.to_owned();
        let model_id = model_id.to_owned();
        let vector_text = format!(
            "[{}]",
            query_embedding.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
        );

        let mut sql = format!(
            "SELECT collection,
                    root_id,
                    path,
                    ordinal,
                    symbol_name,
                    kind,
                    line_start,
                    line_end,
                    1.0 - (embedding <=> $1::text::vector) AS score
             FROM {}
             WHERE snapshot_id = $2 AND model_id = $3 AND dimension = $4",
            self.table("serving_semantic")
        );

        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            vec![&vector_text, &snapshot_id, &model_id, &dimension];
        if let Some(collection) = collection.as_ref() {
            sql.push_str(&format!(" AND collection = ${}", params.len() + 1));
            params.push(collection);
        }
        sql.push_str(&format!(
            " ORDER BY embedding <=> $1::text::vector LIMIT ${}",
            params.len() + 1
        ));
        params.push(&limit);

        let mut client = self.connect()?;
        if !self.storage_table_exists(&mut *client, "serving_semantic")? {
            return Err(SearchError::ExternalBaseline(
                "serving_semantic_unavailable: semantic serving table is not initialized"
                    .to_owned(),
            ));
        }

        let rows = client.query(&sql, &params)?;
        if rows.is_empty()
            && !self.semantic_serving_rows_exist(
                &mut *client,
                &snapshot_id,
                &model_id,
                dimension,
                collection.as_deref(),
            )?
        {
            return Err(SearchError::ExternalBaseline(
                "serving_semantic_unavailable: snapshot has no semantic serving rows".to_owned(),
            ));
        }

        let source_spans = source_spans_for_serving_rows(self, &mut *client, &snapshot_id, &rows)?;
        Ok(rows
            .into_iter()
            .map(|row| SemanticHit {
                source_span: source_spans.get(&serving_row_key(&row)).cloned(),
                collection: row.get("collection"),
                root_id: row.get("root_id"),
                path: row.get("path"),
                symbol_name: row.get("symbol_name"),
                kind: row.get("kind"),
                line_start: row.get::<_, i32>("line_start") as u32,
                line_end: row.get::<_, i32>("line_end") as u32,
                score: row.get::<_, f64>("score") as f32,
            })
            .collect())
    }
}

impl WorkspaceBaselineManifestStore for PostgresBaselineAdapter {
    fn load_baseline_manifest(
        &self,
        snapshot_id: &str,
    ) -> Result<crate::ports::WorkspaceBaselineManifest, SearchError> {
        self.check_storage_readiness()?;
        let mut client = self.connect()?;

        let meta_query = format!(
            "SELECT id, fingerprint FROM {} WHERE id = $1 LIMIT 1",
            self.table("snapshots")
        );
        let Some(meta_row) = client.query_opt(&meta_query, &[&snapshot_id])? else {
            return Err(SearchError::ExternalBaseline(format!(
                "snapshot '{}' not found for manifest loading",
                snapshot_id
            )));
        };
        let snapshot_fingerprint: Option<String> = meta_row.get("fingerprint");

        let visible_files = materialize_visible_snapshot_files(&mut *client, self, snapshot_id)?;

        let files = visible_files
            .into_iter()
            .filter(|f| f.collection == "code")
            .map(|f| crate::ports::BaselineManifestFile {
                collection: f.collection,
                root_id: f.root_id,
                path: f.path,
                file_fingerprint: f.file_fingerprint,
                document_count: f.document_count,
                file_object_id: f.file_object_id,
            })
            .collect();

        Ok(crate::ports::WorkspaceBaselineManifest {
            snapshot_id: snapshot_id.to_owned(),
            snapshot_fingerprint,
            files,
        })
    }
}

impl PostgresBaselineAdapter {
    pub fn migrate_storage(&self) -> Result<(), SearchError> {
        self.ensure_index_names_survive_the_identifier_limit()?;

        let mut client = self.connect()?;

        let statements = self.ensure_schema_statements();
        self.migrate_structure(&mut client, &statements)?;

        // The optional half stays outside and stays tolerant: a database without the `vector`
        // extension is fully usable for everything but semantic serving, which refuses by name.
        for statement in self.pgvector_schema_statements() {
            if let Err(e) = client.batch_execute(&statement) {
                let concurrent_extension_install = statement
                    == "CREATE EXTENSION IF NOT EXISTS vector"
                    && e.as_db_error().is_some_and(|error| {
                        error.code().code() == "23505"
                            && error.constraint() == Some("pg_extension_name_index")
                    });
                if concurrent_extension_install {
                    // Two independent workspace schemas can migrate concurrently in one
                    // database. `IF NOT EXISTS` can still lose the unique-index race while
                    // both sessions install this database-wide extension. The winner has
                    // installed it; continue with this schema's serving table and index.
                    continue;
                }
                tracing::warn!("pgvector DDL skipped (semantic serving will be unavailable): {e}");
                break;
            }
        }

        Ok(())
    }

    /// Forward-only, enforced inside the migration's own transaction and under the row lock.
    ///
    /// An older build's migrator would otherwise stamp its own version over a newer schema AND
    /// rebuild the keys to its own shape, undoing the barrier that keeps consumers off a schema
    /// they cannot read.
    ///
    /// Read inside the transaction, after the advisory lock that serializes migrators: outside
    /// it this is check-then-act — two migrators of different versions both read the old number,
    /// and the older one commits last and wins.
    ///
    /// Asked only where there is somewhere to ask: on an empty schema this very migration is
    /// what creates the metadata table.
    fn refuse_to_move_a_newer_schema_backwards(
        &self,
        tx: &mut Transaction<'_>,
    ) -> Result<(), SearchError> {
        if !self.storage_table_exists(tx, SCHEMA_METADATA_TABLE)? {
            return Ok(());
        }
        let row = tx.query_opt(
            &format!(
                "SELECT value::INTEGER FROM {} WHERE setting = 'schema_version'",
                self.table(SCHEMA_METADATA_TABLE)
            ),
            &[],
        )?;
        let Some(version) = row.map(|row| row.get::<_, i32>(0)) else {
            return Ok(());
        };
        if version <= crate::error::SCHEMA_VERSION_CURRENT {
            return Ok(());
        }
        Err(SearchError::SchemaVersionMismatch {
            expected: crate::error::SCHEMA_VERSION_CURRENT,
            actual: Some(version),
            schema: self.schema.clone(),
        })
    }

    /// Refuses a schema name only when two generated index names actually COLLIDE after
    /// truncation.
    ///
    /// Truncation by itself is harmless: PostgreSQL cuts identifiers at 63 bytes symmetrically,
    /// in the declaration and in the lookup alike, which is why long names work. What breaks is
    /// a collision — when the cut eats the part that tells two names apart, one index is dropped
    /// in place of another and the migration still records success.
    ///
    /// Judged by comparing the truncated names, not by their length: the earlier version of this
    /// guard refused as soon as the longest name stopped fitting, which is 15 bytes of schema
    /// name before any collision is possible, and it told the operator that names "would
    /// collide" when none did. That refusal stood first in `migrate_storage`, so a deployment
    /// with a merely long schema name could neither migrate nor pass the version check.
    fn ensure_index_names_survive_the_identifier_limit(&self) -> Result<(), SearchError> {
        const IDENTIFIER_LIMIT: usize = 63;
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for name in self.generated_index_names() {
            let truncated = truncate_identifier(&name, IDENTIFIER_LIMIT);
            if let Some(other) = seen.insert(truncated.clone(), name.clone()) {
                if other != name {
                    return Err(SearchError::ExternalBaseline(format!(
                        "schema_name_too_long: index names '{other}' and '{name}' both truncate \
                         to '{truncated}' at PostgreSQL's {IDENTIFIER_LIMIT}-byte identifier \
                         limit, so one of the two indexes would silently replace the other. Use \
                         a shorter schema name."
                    )));
                }
            }
        }
        Ok(())
    }

    /// Every index name this schema's DDL declares, gathered from the DDL itself.
    ///
    /// Read out of the generated statements rather than listed by hand, so an index added later
    /// is covered without anyone remembering to add it here — the failure this guards against is
    /// precisely one that nobody notices.
    fn generated_index_names(&self) -> Vec<String> {
        let prefix = format!("idx_{}_", self.schema);
        let mut names = Vec::new();
        for statement in
            self.ensure_schema_statements().iter().chain(self.pgvector_schema_statements().iter())
        {
            let mut rest = statement.as_str();
            while let Some(at) = rest.find(&prefix) {
                let tail = &rest[at..];
                let end =
                    tail.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(tail.len());
                let name = tail[..end].to_owned();
                if !names.contains(&name) {
                    names.push(name);
                }
                rest = &tail[end..];
            }
        }
        names
    }

    /// Applies the mandatory half of the migration as one transaction, version included.
    ///
    /// Changing a primary key is not idempotent in the middle: between `DROP CONSTRAINT` and
    /// `ADD PRIMARY KEY` the table has no key at all, and an interrupted run would leave a state
    /// a retry cannot repair. PostgreSQL keeps DDL transactional, so one transaction removes
    /// that state entirely.
    ///
    /// The version is stamped inside the same transaction rather than after it. Stamped after,
    /// a rolled-back structural change would still be followed by version 2 on an unmigrated
    /// schema, `Ok` from this function, and "storage is ready" printed at the operator — a false
    /// ready, and a baseline switched off for every consumer.
    fn migrate_structure(
        &self,
        client: &mut PgPooledConnection,
        statements: &[String],
    ) -> Result<(), SearchError> {
        let mut tx = client.transaction()?;
        // Serializes migrators of this schema against each other for the life of the
        // transaction. The version row cannot do it: on a fresh or repaired schema there is no
        // row to lock, so two migrators of different versions would both read "no version",
        // both proceed, and the older one would commit last and win.
        //
        // Binds only builds that take this lock, which means builds that know this version and
        // later ones. A build old enough to predate the lock stamps its own, lower version over
        // this one and thereby unblocks its own readiness check — nothing here can reach it.
        // Two builds of different versions must not share a schema; the barrier orders upgrades,
        // it does not survive a downgrade.
        tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&self.schema])?;
        self.refuse_to_move_a_newer_schema_backwards(&mut tx)?;
        for statement in statements {
            tx.batch_execute(statement)?;
        }
        self.write_schema_version(&mut tx)?;
        tx.commit()?;
        Ok(())
    }
}

impl SnapshotPublisher for PostgresBaselineAdapter {
    fn publish_snapshot(
        &self,
        snapshot: &Snapshot,
        metadata: &SnapshotPublishMetadata,
        documents: &[IndexedDocument],
    ) -> Result<SnapshotPublishStats, SearchError> {
        if snapshot.parent_id.as_ref().is_some_and(|parent| parent.0 == snapshot.id.0) {
            return Err(SearchError::ExternalBaseline(
                "snapshot cannot reference itself as parent".to_owned(),
            ));
        }

        // Before the readiness check, which connects: an identity this baseline cannot share is
        // refused without touching the database at all.
        ensure_the_roots_mean_the_same_elsewhere(documents)?;
        crate::publish::validate_source_span_layout(documents, self.token_layout_claim.is_some())?;
        if let Some((version, claim)) = &self.token_layout_claim {
            self.ensure_token_layout_claim(version, claim)?;
        }

        self.check_storage_readiness()?;

        let mut phase_timings = SnapshotPublishPhaseTimings::default();
        let grouping_started = Instant::now();
        let file_groups = group_documents_by_file(documents);
        phase_timings.grouping = grouping_started.elapsed();

        let mut client = self.connect()?;
        let mut tx = client.transaction()?;

        let upsert_snapshot = format!(
            "INSERT INTO {} (id, corpus, fingerprint, parent_snapshot_id, branch, commit_sha)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
                corpus = EXCLUDED.corpus,
                fingerprint = EXCLUDED.fingerprint,
                parent_snapshot_id = EXCLUDED.parent_snapshot_id,
                branch = EXCLUDED.branch,
                commit_sha = EXCLUDED.commit_sha",
            self.table("snapshots")
        );
        tx.execute(
            &upsert_snapshot,
            &[
                &snapshot.id.0,
                &snapshot.corpus.as_str(),
                &snapshot.fingerprint,
                &snapshot.parent_id.as_ref().map(|value| value.0.as_str()),
                &metadata.branch.as_deref(),
                &metadata.commit.as_deref(),
            ],
        )?;

        let delete_snapshot_files =
            format!("DELETE FROM {} WHERE snapshot_id = $1", self.table("snapshot_files"));
        tx.execute(&delete_snapshot_files, &[&snapshot.id.0])?;
        let delete_snapshot_deletions =
            format!("DELETE FROM {} WHERE snapshot_id = $1", self.table("snapshot_deletions"));
        tx.execute(&delete_snapshot_deletions, &[&snapshot.id.0])?;
        invalidate_semantic_publication_for_snapshot(&mut tx, self, &snapshot.id.0)?;

        let parent_files = if let Some(parent_id) = snapshot.parent_id.as_ref() {
            materialize_visible_snapshot_file_map(&mut tx, self, &parent_id.0)?
        } else {
            BTreeMap::new()
        };

        let mut stats = SnapshotPublishStats::default();
        let mut remaining_parent_files = parent_files;
        let mut snapshot_file_rows = Vec::new();
        let mut snapshot_deletion_rows = Vec::new();
        let normalized_content_started = Instant::now();
        for file_group in &file_groups {
            let file_key = (
                file_group.collection.clone(),
                file_group.root_id.clone(),
                file_group.path.clone(),
            );
            let parent_entry = remaining_parent_files.remove(&file_key);
            if parent_entry
                .as_ref()
                .is_some_and(|parent| parent.file_fingerprint == file_group.file_fingerprint)
            {
                stats.reused_files += 1;
                stats.reused_documents += file_group.documents.len();
                continue;
            }

            let file_object_id =
                file_object_id_for(&file_group.collection, &file_group.file_fingerprint);
            let inserted = try_insert_file_object(&mut tx, self, &file_object_id, file_group)?;
            snapshot_file_rows.push(SnapshotFileRow {
                snapshot_id: snapshot.id.0.clone(),
                collection: file_group.collection.clone(),
                root_id: file_group.root_id.clone(),
                path: file_group.path.clone(),
                file_fingerprint: file_group.file_fingerprint.clone(),
                document_count: file_group.documents.len() as i32,
                file_object_id,
            });
            stats.written_files += 1;
            stats.written_documents += file_group.documents.len();
            if !inserted {
                tracing::trace!(
                    collection = %file_group.collection,
                    path = %file_group.path,
                    documents = file_group.documents.len(),
                    "reused existing file object during snapshot publish"
                );
            }
        }

        for ((collection, root_id, path), _) in remaining_parent_files {
            snapshot_deletion_rows.push(SnapshotDeletionRow {
                snapshot_id: snapshot.id.0.clone(),
                collection,
                root_id,
                path,
            });
            stats.deleted_files += 1;
        }

        insert_snapshot_file_rows(&mut tx, self, &snapshot_file_rows)?;
        insert_snapshot_deletion_rows(&mut tx, self, &snapshot_deletion_rows)?;
        phase_timings.normalized_content = normalized_content_started.elapsed();

        let lexical_started = Instant::now();
        replace_serving_lexical_rows(&mut tx, self, &snapshot.id.0, &file_groups)?;
        phase_timings.lexical_rebuild = lexical_started.elapsed();

        let final_activation_started = Instant::now();
        if let Some(branch) = metadata.branch.as_deref() {
            let upsert_head = format!(
                "INSERT INTO {heads} (corpus, branch, snapshot_id, updated_at)
                 VALUES ($1, $2, $3, NOW())
                 ON CONFLICT (corpus, branch) DO UPDATE SET
                    snapshot_id = EXCLUDED.snapshot_id,
                    updated_at = NOW()
                 WHERE (SELECT created_at FROM {snaps} WHERE id = {heads}.snapshot_id)
                       < (SELECT created_at FROM {snaps} WHERE id = EXCLUDED.snapshot_id)",
                heads = self.table("snapshot_heads"),
                snaps = self.table("snapshots"),
            );
            tx.execute(&upsert_head, &[&snapshot.corpus.as_str(), &branch, &snapshot.id.0])?;
        }

        tx.commit()?;
        phase_timings.final_activation = final_activation_started.elapsed();
        log_snapshot_publish_phase_timings(snapshot, &phase_timings, &stats, documents.len());
        Ok(stats)
    }
}

#[derive(Debug, Clone, Default)]
struct SnapshotPublishPhaseTimings {
    grouping: Duration,
    normalized_content: Duration,
    lexical_rebuild: Duration,
    final_activation: Duration,
}

#[derive(Debug, Clone, Default)]
struct SemanticPublishPhaseTimings {
    ancestry_materialization: Duration,
    parent_copy: Duration,
    changed_recompute: Duration,
    final_sync: Duration,
}

#[derive(Debug, Clone, Default)]
struct SemanticPublishLogStats {
    copied_rows: usize,
    inserted_rows: usize,
    missing_embeddings: usize,
    changed_files: usize,
    changed_rows: usize,
    deleted_paths: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticPublishPlan {
    strategy: SemanticPublishStrategy,
    changed_paths: BTreeSet<VisibleFileKey>,
    deleted_paths: BTreeSet<VisibleFileKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemanticPublishStrategy {
    CurrentSnapshotOnly,
    IncrementalFromParent { parent_snapshot_id: String },
    FullRebuild,
}

impl SemanticPublishStrategy {
    fn label(&self) -> &'static str {
        match self {
            Self::CurrentSnapshotOnly => "current_snapshot_only",
            Self::IncrementalFromParent { .. } => "incremental_copy_forward",
            Self::FullRebuild => "full_rebuild",
        }
    }

    fn parent_snapshot_id(&self) -> Option<&str> {
        match self {
            Self::IncrementalFromParent { parent_snapshot_id } => Some(parent_snapshot_id.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct PublishedFileGroup {
    collection: String,
    root_id: String,
    path: String,
    file_fingerprint: String,
    documents: Vec<IndexedDocument>,
}

#[derive(Debug, Clone)]
struct VisibleSnapshotFile {
    collection: String,
    /// The source root this file belongs to, read from the column the schema now keys by.
    ///
    /// Half of the file's identity, not a label on it: the same relative path under two roots
    /// is two different files, so a row that loses this field silently becomes the other one.
    root_id: String,
    path: String,
    file_fingerprint: String,
    document_count: usize,
    file_object_id: String,
}

/// What makes one visible file distinct from another within a snapshot's ancestry:
/// collection, the source root, and the path relative to that root.
///
/// The root is part of it because an extension repeats the configuration's directory layout,
/// so the same relative path names a different file under each root. Without it a deletion
/// recorded for one root shadows the live file of another.
type VisibleFileKey = (String, String, String);

impl VisibleSnapshotFile {
    fn key(&self) -> VisibleFileKey {
        (self.collection.clone(), self.root_id.clone(), self.path.clone())
    }
}

#[derive(Debug, Clone)]
struct FileObjectItem {
    symbol_name: String,
    kind: String,
    line_start: u32,
    line_end: u32,
    text: String,
    content_hash: String,
    graph_context: Option<String>,
    source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone)]
struct ContentObjectRow {
    content_hash: String,
    text: String,
}

#[derive(Debug, Clone)]
struct FileObjectItemRow {
    file_object_id: String,
    ordinal: i32,
    symbol_name: String,
    kind: String,
    line_start: i32,
    line_end: i32,
    content_hash: String,
    graph_context: Option<String>,
    source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone)]
struct FileObjectPartRow {
    file_object_id: String,
    ordinal: i32,
    source_span: SourceSpan,
}

fn source_span_from_row(row: &Row) -> Result<SourceSpan, SearchError> {
    let missing = || {
        SearchError::ExternalBaseline(
            "token_layout_provenance_missing: file object part row is incomplete".to_owned(),
        )
    };
    let parent_symbol = row.try_get::<_, Option<String>>("parent_symbol")?.ok_or_else(missing)?;
    let to_u32 = |column: &str| -> Result<u32, SearchError> {
        let value = row.try_get::<_, Option<i64>>(column)?.ok_or_else(missing)?;
        u32::try_from(value).map_err(|_| missing())
    };
    Ok(SourceSpan {
        parent_symbol,
        byte_start: to_u32("byte_start")?,
        byte_end: to_u32("byte_end")?,
        parent_byte_start: to_u32("parent_byte_start")?,
        parent_byte_end: to_u32("parent_byte_end")?,
        part_index: to_u32("part_index")?,
        part_count: to_u32("part_count")?,
    })
}

#[derive(Debug, Clone)]
struct SnapshotFileRow {
    snapshot_id: String,
    collection: String,
    root_id: String,
    path: String,
    file_fingerprint: String,
    document_count: i32,
    file_object_id: String,
}

#[derive(Debug, Clone)]
struct SnapshotDeletionRow {
    snapshot_id: String,
    collection: String,
    root_id: String,
    path: String,
}

#[derive(Debug, Clone)]
struct ServingLexicalRow {
    snapshot_id: String,
    collection: String,
    root_id: String,
    path: String,
    ordinal: i32,
    symbol_name: String,
    kind: String,
    line_start: i32,
    line_end: i32,
    text: String,
}

#[derive(Debug, Clone)]
struct PendingSemanticRow {
    collection: String,
    root_id: String,
    path: String,
    ordinal: i32,
    symbol_name: String,
    kind: String,
    line_start: i32,
    line_end: i32,
    embedding_key: String,
}

#[derive(Debug, Clone)]
struct ServingSemanticRow {
    collection: String,
    root_id: String,
    path: String,
    ordinal: i32,
    symbol_name: String,
    kind: String,
    line_start: i32,
    line_end: i32,
    vector_text: String,
}

#[derive(Debug, Clone, Default)]
struct PreparedSemanticRows {
    rows: Vec<ServingSemanticRow>,
    missing_embeddings: usize,
}

#[derive(Debug, Clone)]
struct EffectiveSnapshotSummary {
    total_files: usize,
    total_documents: usize,
    collections: Vec<BaselineCollectionRecord>,
}

fn load_embeddings_from_client(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    embedding_keys: &[String],
    model_id: &str,
    dimension: usize,
) -> Result<HashMap<String, Vec<f32>>, SearchError> {
    if embedding_keys.is_empty() {
        return Ok(HashMap::new());
    }

    let query = format!(
        "SELECT embedding_key, embedding
         FROM {}
         WHERE model_id = $1 AND dimension = $2 AND embedding_key = ANY($3)",
        adapter.table("semantic_embeddings")
    );
    let rows = client.query(&query, &[&model_id, &(dimension as i32), &embedding_keys])?;

    let mut result = HashMap::new();
    for row in rows {
        let key: String = row.get("embedding_key");
        let blob: Vec<u8> = row.get("embedding");
        if blob.len() != dimension * 4 {
            continue;
        }
        let embedding =
            blob.as_chunks::<4>().0.iter().copied().map(f32::from_le_bytes).collect::<Vec<_>>();
        result.insert(key, embedding);
    }
    Ok(result)
}

fn semantic_publication_setting(snapshot_id: &str, model_id: &str, dimension: usize) -> String {
    format!("{SEMANTIC_PUBLICATION_COMPLETE_PREFIX}{snapshot_id}:{model_id}:{dimension}")
}

fn semantic_publication_complete(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
) -> Result<bool, SearchError> {
    let setting = semantic_publication_setting(snapshot_id, model_id, dimension);
    Ok(client
        .query_opt(
            &format!(
                "SELECT 1 FROM {} WHERE setting = $1 AND value = 'complete' LIMIT 1",
                adapter.table(SCHEMA_METADATA_TABLE)
            ),
            &[&setting],
        )?
        .is_some())
}

fn clear_semantic_publication_complete(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
) -> Result<(), SearchError> {
    let setting = semantic_publication_setting(snapshot_id, model_id, dimension);
    tx.execute(
        &format!("DELETE FROM {} WHERE setting = $1", adapter.table(SCHEMA_METADATA_TABLE)),
        &[&setting],
    )?;
    Ok(())
}

fn mark_semantic_publication_complete(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
) -> Result<(), SearchError> {
    let setting = semantic_publication_setting(snapshot_id, model_id, dimension);
    let value = "complete".to_owned();
    tx.execute(
        &format!(
            "INSERT INTO {} (setting, value)
             VALUES ($1, $2)
             ON CONFLICT (setting) DO UPDATE SET value = EXCLUDED.value",
            adapter.table(SCHEMA_METADATA_TABLE)
        ),
        &[&setting, &value],
    )?;
    Ok(())
}

fn invalidate_semantic_publication_for_snapshot(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<(), SearchError> {
    let settings_prefix = format!("{SEMANTIC_PUBLICATION_COMPLETE_PREFIX}{snapshot_id}:%");
    tx.execute(
        &format!("DELETE FROM {} WHERE setting LIKE $1", adapter.table(SCHEMA_METADATA_TABLE)),
        &[&settings_prefix],
    )?;
    if adapter.storage_table_exists(tx, "serving_semantic")? {
        tx.execute(
            &format!("DELETE FROM {} WHERE snapshot_id = $1", adapter.table("serving_semantic")),
            &[&snapshot_id],
        )?;
    }
    Ok(())
}

/// How many times each snapshot's own row has been written, as PostgreSQL counts it.
///
/// `publish_snapshot` upserts a snapshot's row as the first write of its own transaction, so the
/// value changes on every republish of that id — whether the corpus changed its bytes, its roots
/// or nothing at all. That is stricter than comparing file sets and deliberately so: the cost of
/// a false alarm is one loud, repeatable refusal, while the cost of a missed one is a stale
/// corpus wearing a completeness mark.
///
/// A snapshot that is not there is reported by its ABSENCE from the map rather than as an error.
/// The caller treats absence on either side as movement, which it has to: two identical absences
/// would otherwise compare equal, and a plan read from a snapshot that was already gone — the
/// completeness mark of a parent outlives the parent, since it lives in the metadata table — is
/// exactly as stale as one read from a snapshot since replaced.
fn snapshot_row_versions(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_ids: &[String],
) -> Result<HashMap<String, String>, SearchError> {
    let query =
        format!("SELECT id, xmin::TEXT FROM {} WHERE id = ANY($1)", adapter.table("snapshots"));
    Ok(client
        .query(&query, &[&snapshot_ids])?
        .into_iter()
        .map(|row| (row.get("id"), row.get("xmin")))
        .collect())
}

fn snapshot_parent_id(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<Option<String>, SearchError> {
    let query = format!(
        "SELECT parent_snapshot_id FROM {} WHERE id = $1 LIMIT 1",
        adapter.table("snapshots")
    );
    let Some(row) = client.query_opt(&query, &[&snapshot_id])? else {
        return Err(SearchError::ExternalBaseline(format!(
            "snapshot '{}' was not found",
            snapshot_id
        )));
    };
    Ok(row.get("parent_snapshot_id"))
}

fn semantic_publish_phase_count(strategy: &SemanticPublishStrategy) -> usize {
    match strategy {
        SemanticPublishStrategy::IncrementalFromParent { .. } => 3,
        SemanticPublishStrategy::CurrentSnapshotOnly | SemanticPublishStrategy::FullRebuild => 2,
    }
}

fn semantic_publish_strategy(
    parent_snapshot_id: Option<String>,
    parent_complete: bool,
) -> SemanticPublishStrategy {
    match parent_snapshot_id {
        Some(parent_snapshot_id) if parent_complete => {
            SemanticPublishStrategy::IncrementalFromParent { parent_snapshot_id }
        }
        Some(_) => SemanticPublishStrategy::FullRebuild,
        None => SemanticPublishStrategy::CurrentSnapshotOnly,
    }
}

fn semantic_publish_plan(
    parent_snapshot_id: Option<String>,
    parent_complete: bool,
    current_snapshot_files: &[VisibleSnapshotFile],
    deleted_paths: &[VisibleFileKey],
) -> SemanticPublishPlan {
    SemanticPublishPlan {
        strategy: semantic_publish_strategy(parent_snapshot_id, parent_complete),
        changed_paths: current_snapshot_files.iter().map(VisibleSnapshotFile::key).collect(),
        deleted_paths: deleted_paths.iter().cloned().collect(),
    }
}

fn load_snapshot_file_rows(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<Vec<VisibleSnapshotFile>, SearchError> {
    let query = format!(
        "SELECT collection, root_id, path, file_fingerprint, document_count, file_object_id
         FROM {}
         WHERE snapshot_id = $1
         ORDER BY collection, root_id, path",
        adapter.table("snapshot_files")
    );
    Ok(client
        .query(&query, &[&snapshot_id])?
        .into_iter()
        .map(|row| VisibleSnapshotFile {
            collection: row.get("collection"),
            root_id: row.get("root_id"),
            path: row.get("path"),
            file_fingerprint: row.get("file_fingerprint"),
            document_count: row.get::<_, i32>("document_count") as usize,
            file_object_id: row.get("file_object_id"),
        })
        .collect())
}

/// Refuses a corpus whose root identifiers cannot mean the same thing on another machine.
///
/// A root inside the project directory is identified by its path relative to that directory, so
/// two checkouts agree on it. A root OUTSIDE gets its absolute canonical path
/// (`workspace_roots.rs:319-338`), which names a place on one machine and nothing anywhere else.
///
/// In a shared baseline that is not a cosmetic defect. The consumer keys files by
/// `(root_id, path)`, so every file of such a root misses its manifest entry and is re-indexed
/// whole as an overlay — the baseline silently stops being reusable, and the symptom reads as
/// slowness rather than as a wrong identity. Refusing to store it is the honest answer, and the
/// name of the extension cannot stand in: several entries are allowed to share one.
pub fn ensure_the_roots_mean_the_same_elsewhere(
    documents: &[IndexedDocument],
) -> Result<(), SearchError> {
    let unportable: BTreeSet<&str> = documents
        .iter()
        .map(|document| document.root_id.as_str())
        .filter(|root_id| std::path::Path::new(root_id).is_absolute())
        .collect();
    if unportable.is_empty() {
        return Ok(());
    }
    Err(SearchError::ExternalBaseline(format!(
        "root_id_not_portable: these source roots lie outside the project directory, so they are \
         identified by an absolute path that names nothing on any other machine: {}. A shared \
         baseline keyed by such an id cannot be reused by anyone else. Move the extension inside \
         the project directory, or publish the configuration alone.",
        unportable.into_iter().collect::<Vec<_>>().join(", ")
    )))
}

#[cfg(test)]
thread_local! {
    /// The isolation level actually in force inside the planning transaction.
    ///
    /// Recorded from production's own transaction rather than asserted about the source, because
    /// the property that matters — one snapshot of the data for every read the plan is built
    /// from — is a property of the connection at runtime. A structural check ("the same
    /// `&mut Transaction`") is green for a READ COMMITTED transaction, which is exactly the case
    /// this guards against.
    static OBSERVED_PLANNING_ISOLATION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn observe_planning_isolation(client: &mut impl GenericClient) -> Result<(), SearchError> {
    let level: String = client.query_one("SHOW transaction_isolation", &[])?.get(0);
    OBSERVED_PLANNING_ISOLATION.with(|cell| *cell.borrow_mut() = Some(level));
    Ok(())
}

/// An identifier as PostgreSQL will store it: cut to `limit` BYTES, never mid-character.
fn truncate_identifier(name: &str, limit: usize) -> String {
    if name.len() <= limit {
        return name.to_owned();
    }
    let mut end = limit;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// `['a', 'b']` as a PostgreSQL text array, for comparing against a catalog composition.
fn sql_text_array(columns: &[&str]) -> String {
    let items: Vec<String> = columns.iter().map(|column| format!("'{column}'")).collect();
    format!("ARRAY[{}]", items.join(", "))
}

/// `['a', 'b']` as the parenthesised column list of a key or index declaration.
fn sql_column_list(columns: &[&str]) -> String {
    format!("({})", columns.join(", "))
}

fn load_snapshot_deletion_keys(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<Vec<VisibleFileKey>, SearchError> {
    let query = format!(
        "SELECT collection, root_id, path FROM {} WHERE snapshot_id = $1
          ORDER BY collection, root_id, path",
        adapter.table("snapshot_deletions")
    );
    Ok(client
        .query(&query, &[&snapshot_id])?
        .into_iter()
        .map(|row| (row.get("collection"), row.get("root_id"), row.get("path")))
        .collect())
}

fn prepare_semantic_rows_for_files(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    visible_files: &[VisibleSnapshotFile],
    model_id: &str,
    dimension: usize,
) -> Result<PreparedSemanticRows, SearchError> {
    if visible_files.is_empty() {
        return Ok(PreparedSemanticRows::default());
    }

    // Deduplicated: two files with identical content under two roots share ONE file object, and
    // that is deliberate — the object describes content, so they also share one embedding.
    let file_object_ids = visible_files
        .iter()
        .map(|file| file.file_object_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let items_query = format!(
        "SELECT foi.file_object_id, foi.ordinal, foi.symbol_name, foi.kind,
                foi.line_start, foi.line_end, foi.graph_context, co.text
         FROM {} foi
         JOIN {} co ON co.content_hash = foi.content_hash
         WHERE foi.file_object_id = ANY($1)
         ORDER BY foi.file_object_id, foi.ordinal",
        adapter.table("file_object_items"),
        adapter.table("content_objects"),
    );
    let item_rows = client.query(&items_query, &[&file_object_ids])?;

    let mut items_by_object: HashMap<String, Vec<postgres::Row>> = HashMap::new();
    for row in item_rows {
        items_by_object.entry(row.get("file_object_id")).or_default().push(row);
    }

    // Walked from the FILES, not from the objects. `file_object_id` is derived from content, so
    // it stops being one-to-one the moment two roots hold the same bytes at the same relative
    // path — and rebuilding identity backwards from it drops one of those files entirely rather
    // than merely mislabelling it.
    let mut pending_rows = Vec::new();
    let mut embedding_keys = Vec::new();
    let mut seen_keys = HashSet::new();
    for file in visible_files {
        let Some(items) = items_by_object.get(&file.file_object_id) else {
            continue;
        };
        for row in items {
            let graph_context: Option<String> = row.get("graph_context");
            let embedding_key = crate::document::semantic_key_from_parts(
                &file.path,
                row.get("kind"),
                row.get("symbol_name"),
                graph_context.as_deref().unwrap_or(""),
                row.get("text"),
            );
            if seen_keys.insert(embedding_key.clone()) {
                embedding_keys.push(embedding_key.clone());
            }
            pending_rows.push(PendingSemanticRow {
                collection: file.collection.clone(),
                root_id: file.root_id.clone(),
                path: file.path.clone(),
                ordinal: row.get("ordinal"),
                symbol_name: row.get("symbol_name"),
                kind: row.get("kind"),
                line_start: row.get("line_start"),
                line_end: row.get("line_end"),
                embedding_key,
            });
        }
    }

    let embeddings =
        load_embeddings_from_client(client, adapter, &embedding_keys, model_id, dimension)?;
    let mut prepared_rows = PreparedSemanticRows::default();
    prepared_rows.rows.reserve(pending_rows.len());
    for pending_row in pending_rows {
        if let Some(embedding) = embeddings.get(&pending_row.embedding_key) {
            prepared_rows.rows.push(ServingSemanticRow {
                collection: pending_row.collection,
                root_id: pending_row.root_id,
                path: pending_row.path,
                ordinal: pending_row.ordinal,
                symbol_name: pending_row.symbol_name,
                kind: pending_row.kind,
                line_start: pending_row.line_start,
                line_end: pending_row.line_end,
                vector_text: format_pgvector_text(embedding),
            });
        } else {
            prepared_rows.missing_embeddings += 1;
        }
    }
    Ok(prepared_rows)
}

fn delete_serving_semantic_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
) -> Result<(), SearchError> {
    tx.execute(
        &format!(
            "DELETE FROM {} WHERE snapshot_id = $1 AND model_id = $2 AND dimension = $3",
            adapter.table("serving_semantic")
        ),
        &[&snapshot_id, &model_id, &(dimension as i32)],
    )?;
    Ok(())
}

fn copy_parent_serving_semantic_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    parent_snapshot_id: &str,
    model_id: &str,
    dimension: usize,
) -> Result<usize, SearchError> {
    let query = format!(
        "INSERT INTO {} (
            snapshot_id, collection, root_id, path, ordinal, symbol_name, kind,
            line_start, line_end, model_id, dimension, embedding
         )
         SELECT $1, parent.collection, parent.root_id, parent.path, parent.ordinal,
                parent.symbol_name, parent.kind, parent.line_start, parent.line_end,
                parent.model_id, parent.dimension, parent.embedding
         FROM {} parent
         WHERE parent.snapshot_id = $2
           AND parent.model_id = $3
           AND parent.dimension = $4
           AND NOT EXISTS (
               SELECT 1 FROM {} sf
               WHERE sf.snapshot_id = $1
                 AND sf.collection = parent.collection
                 AND sf.root_id = parent.root_id
                 AND sf.path = parent.path
           )
           AND NOT EXISTS (
               SELECT 1 FROM {} sd
               WHERE sd.snapshot_id = $1
                 AND sd.collection = parent.collection
                 AND sd.root_id = parent.root_id
                 AND sd.path = parent.path
           )",
        adapter.table("serving_semantic"),
        adapter.table("serving_semantic"),
        adapter.table("snapshot_files"),
        adapter.table("snapshot_deletions"),
    );
    Ok(tx.execute(&query, &[&snapshot_id, &parent_snapshot_id, &model_id, &(dimension as i32)])?
        as usize)
}

fn insert_serving_semantic_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
    rows: &[ServingSemanticRow],
) -> Result<usize, SearchError> {
    if rows.is_empty() {
        return Ok(0);
    }

    let dimension = dimension as i32;
    let mut inserted = 0usize;
    for batch in rows.chunks(SERVING_SEMANTIC_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 12);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 12;
            values.push(format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}::text::vector)",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
                base + 11,
                base + 12
            ));
            params.push(&snapshot_id);
            params.push(&row.collection);
            params.push(&row.root_id);
            params.push(&row.path);
            params.push(&row.ordinal);
            params.push(&row.symbol_name);
            params.push(&row.kind);
            params.push(&row.line_start);
            params.push(&row.line_end);
            params.push(&model_id);
            params.push(&dimension);
            params.push(&row.vector_text);
        }
        let query = format!(
            "INSERT INTO {} (
                snapshot_id, collection, root_id, path, ordinal, symbol_name, kind,
                line_start, line_end, model_id, dimension, embedding
             ) VALUES {}",
            adapter.table("serving_semantic"),
            values.join(", ")
        );
        inserted += tx.execute(&query, &params)? as usize;
    }
    Ok(inserted)
}

fn format_pgvector_text(embedding: &[f32]) -> String {
    format!("[{}]", embedding.iter().map(|value| value.to_string()).collect::<Vec<_>>().join(","))
}

fn group_documents_by_file(documents: &[IndexedDocument]) -> Vec<PublishedFileGroup> {
    let mut grouped = BTreeMap::<VisibleFileKey, Vec<IndexedDocument>>::new();
    for document in documents {
        grouped
            .entry((document.collection.clone(), document.root_id.clone(), document.path.clone()))
            .or_default()
            .push(document.clone());
    }

    grouped
        .into_iter()
        .map(|((collection, root_id, path), mut documents)| {
            documents.sort_by(|lhs, rhs| {
                (
                    lhs.line_start,
                    lhs.line_end,
                    lhs.symbol_name.as_str(),
                    lhs.kind.as_str(),
                    lhs.content_hash.as_str(),
                )
                    .cmp(&(
                        rhs.line_start,
                        rhs.line_end,
                        rhs.symbol_name.as_str(),
                        rhs.kind.as_str(),
                        rhs.content_hash.as_str(),
                    ))
            });
            let file_fingerprint = fingerprint_file_documents(&documents);
            PublishedFileGroup { collection, root_id, path, file_fingerprint, documents }
        })
        .collect()
}

fn fingerprint_file_documents(documents: &[IndexedDocument]) -> String {
    let mut hasher = blake3::Hasher::new();
    for document in documents {
        hasher.update(document.collection.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.path.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.symbol_name.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.kind.as_bytes());
        hasher.update(&document.line_start.to_le_bytes());
        hasher.update(&document.line_end.to_le_bytes());
        hasher.update(document.content_hash.as_bytes());
        hasher.update(&[0]);
        hasher.update(document.text.as_bytes());
        hasher.update(&[0]);
        // graph_context is folded into the embedding text (and thus the embedding
        // key), so a context-only change must invalidate file-object reuse — else
        // the reused row keeps stale context and its recomputed key no longer
        // matches the freshly stored embedding.
        match document.graph_context.as_deref() {
            Some(context) => {
                hasher.update(&[1]);
                hasher.update(context.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        if let Some(span) = &document.source_span {
            hasher.update(&[1]);
            hasher.update(span.parent_symbol.as_bytes());
            hasher.update(&span.byte_start.to_le_bytes());
            hasher.update(&span.byte_end.to_le_bytes());
            hasher.update(&span.parent_byte_start.to_le_bytes());
            hasher.update(&span.parent_byte_end.to_le_bytes());
            hasher.update(&span.part_index.to_le_bytes());
            hasher.update(&span.part_count.to_le_bytes());
        }
        hasher.update(&[0xff]);
    }
    hasher.finalize().to_hex().to_string()
}

fn file_object_id_for(collection: &str, file_fingerprint: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(collection.as_bytes());
    hasher.update(&[0]);
    hasher.update(file_fingerprint.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn try_insert_file_object(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    file_object_id: &str,
    file_group: &PublishedFileGroup,
) -> Result<bool, SearchError> {
    let insert_file_object = format!(
        "INSERT INTO {} (id, collection, file_fingerprint, document_count)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (id) DO NOTHING",
        adapter.table("file_objects")
    );
    let inserted = tx.execute(
        &insert_file_object,
        &[
            &file_object_id,
            &file_group.collection,
            &file_group.file_fingerprint,
            &(file_group.documents.len() as i32),
        ],
    )?;
    if inserted == 0 {
        return Ok(false);
    }

    let mut content_rows = Vec::with_capacity(file_group.documents.len());
    let mut item_rows = Vec::with_capacity(file_group.documents.len());
    for (ordinal, document) in file_group.documents.iter().enumerate() {
        content_rows.push(ContentObjectRow {
            content_hash: document.content_hash.clone(),
            text: document.text.clone(),
        });
        item_rows.push(FileObjectItemRow {
            file_object_id: file_object_id.to_owned(),
            ordinal: ordinal as i32,
            symbol_name: document.symbol_name.clone(),
            kind: document.kind.clone(),
            line_start: document.line_start as i32,
            line_end: document.line_end as i32,
            content_hash: document.content_hash.clone(),
            graph_context: document.graph_context.clone(),
            source_span: document.source_span.clone(),
        });
    }

    upsert_content_objects(tx, adapter, &content_rows)?;
    insert_file_object_items(tx, adapter, &item_rows)?;
    if adapter.token_layout_claim.is_some() {
        let part_rows = item_rows
            .iter()
            .enumerate()
            .filter_map(|(ordinal, row)| {
                row.source_span.clone().map(|source_span| FileObjectPartRow {
                    file_object_id: file_object_id.to_owned(),
                    ordinal: ordinal as i32,
                    source_span,
                })
            })
            .collect::<Vec<_>>();
        insert_file_object_parts(tx, adapter, &part_rows)?;
    }
    Ok(true)
}

fn insert_file_object_parts(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    rows: &[FileObjectPartRow],
) -> Result<(), SearchError> {
    for batch in rows.chunks(FILE_OBJECT_ITEM_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 9);
        let numeric = batch
            .iter()
            .map(|row| {
                [
                    row.source_span.byte_start as i64,
                    row.source_span.byte_end as i64,
                    row.source_span.parent_byte_start as i64,
                    row.source_span.parent_byte_end as i64,
                    row.source_span.part_index as i64,
                    row.source_span.part_count as i64,
                ]
            })
            .collect::<Vec<_>>();
        for (index, row) in batch.iter().enumerate() {
            let base = index * 9;
            values.push(format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9
            ));
            params.push(&row.file_object_id);
            params.push(&row.ordinal);
            params.push(&row.source_span.parent_symbol);
            params.push(&numeric[index][0]);
            params.push(&numeric[index][1]);
            params.push(&numeric[index][2]);
            params.push(&numeric[index][3]);
            params.push(&numeric[index][4]);
            params.push(&numeric[index][5]);
        }
        let query = format!(
            "INSERT INTO {} (
                file_object_id, ordinal, parent_symbol, byte_start, byte_end,
                parent_byte_start, parent_byte_end, part_index, part_count
             ) VALUES {}",
            adapter.table(TOKEN_LAYOUT_PARTS_TABLE),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }
    Ok(())
}

fn unique_content_object_rows(
    rows: &[ContentObjectRow],
) -> Result<Vec<&ContentObjectRow>, SearchError> {
    let mut seen = HashMap::with_capacity(rows.len());
    let mut unique_rows = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(existing) = seen.get(row.content_hash.as_str()) {
            if existing != &row.text {
                return Err(SearchError::ExternalBaseline(format!(
                    "content hash collision within publish batch: hash {} maps to multiple texts",
                    row.content_hash
                )));
            }
            continue;
        }

        seen.insert(row.content_hash.as_str(), row.text.as_str());
        unique_rows.push(row);
    }
    Ok(unique_rows)
}

fn upsert_content_objects(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    rows: &[ContentObjectRow],
) -> Result<(), SearchError> {
    if rows.is_empty() {
        return Ok(());
    }

    let unique_rows = unique_content_object_rows(rows)?;
    for batch in unique_rows.chunks(CONTENT_OBJECT_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 2);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 2;
            values.push(format!("(${}, ${})", base + 1, base + 2));
            params.push(&row.content_hash);
            params.push(&row.text);
        }
        let query = format!(
            "INSERT INTO {} (content_hash, text) VALUES {} \
             ON CONFLICT (content_hash) DO UPDATE SET text = EXCLUDED.text",
            adapter.table("content_objects"),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }

    Ok(())
}

fn insert_file_object_items(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    rows: &[FileObjectItemRow],
) -> Result<(), SearchError> {
    if rows.is_empty() {
        return Ok(());
    }

    for batch in rows.chunks(FILE_OBJECT_ITEM_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 8);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 8;
            values.push(format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8
            ));
            params.push(&row.file_object_id);
            params.push(&row.ordinal);
            params.push(&row.symbol_name);
            params.push(&row.kind);
            params.push(&row.line_start);
            params.push(&row.line_end);
            params.push(&row.content_hash);
            params.push(&row.graph_context);
        }
        let query = format!(
            "INSERT INTO {} (
                file_object_id, ordinal, symbol_name, kind, line_start, line_end, content_hash,
                graph_context
             ) VALUES {}",
            adapter.table("file_object_items"),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }

    Ok(())
}

fn insert_snapshot_file_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    rows: &[SnapshotFileRow],
) -> Result<(), SearchError> {
    if rows.is_empty() {
        return Ok(());
    }

    for batch in rows.chunks(SNAPSHOT_FILE_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 7);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 7;
            values.push(format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7
            ));
            params.push(&row.snapshot_id);
            params.push(&row.collection);
            params.push(&row.root_id);
            params.push(&row.path);
            params.push(&row.file_fingerprint);
            params.push(&row.document_count);
            params.push(&row.file_object_id);
        }
        let query = format!(
            "INSERT INTO {} (
                snapshot_id, collection, root_id, path,
                file_fingerprint, document_count, file_object_id
             ) VALUES {}",
            adapter.table("snapshot_files"),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }

    Ok(())
}

fn insert_snapshot_deletion_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    rows: &[SnapshotDeletionRow],
) -> Result<(), SearchError> {
    if rows.is_empty() {
        return Ok(());
    }

    for batch in rows.chunks(SNAPSHOT_DELETION_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 4);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 4;
            values.push(format!("(${}, ${}, ${}, ${})", base + 1, base + 2, base + 3, base + 4));
            params.push(&row.snapshot_id);
            params.push(&row.collection);
            params.push(&row.root_id);
            params.push(&row.path);
        }
        let query = format!(
            "INSERT INTO {} (snapshot_id, collection, root_id, path) VALUES {}",
            adapter.table("snapshot_deletions"),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }

    Ok(())
}

fn replace_serving_lexical_rows(
    tx: &mut Transaction<'_>,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
    file_groups: &[PublishedFileGroup],
) -> Result<(), SearchError> {
    let delete = format!("DELETE FROM {} WHERE snapshot_id = $1", adapter.table("serving_lexical"));
    tx.execute(&delete, &[&snapshot_id])?;

    let mut rows = Vec::new();
    for file_group in file_groups {
        for (ordinal, document) in file_group.documents.iter().enumerate() {
            rows.push(ServingLexicalRow {
                snapshot_id: snapshot_id.to_owned(),
                collection: file_group.collection.clone(),
                root_id: file_group.root_id.clone(),
                path: file_group.path.clone(),
                ordinal: ordinal as i32,
                symbol_name: document.symbol_name.clone(),
                kind: document.kind.clone(),
                line_start: document.line_start as i32,
                line_end: document.line_end as i32,
                text: document.text.clone(),
            });
        }
    }

    for batch in rows.chunks(SERVING_LEXICAL_BATCH_SIZE) {
        let mut values = Vec::with_capacity(batch.len());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            Vec::with_capacity(batch.len() * 10);
        for (index, row) in batch.iter().enumerate() {
            let base = index * 10;
            values.push(format!(
                "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, to_tsvector('simple', ${}))",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
                base + 10
            ));
            params.push(&row.snapshot_id);
            params.push(&row.collection);
            params.push(&row.root_id);
            params.push(&row.path);
            params.push(&row.ordinal);
            params.push(&row.symbol_name);
            params.push(&row.kind);
            params.push(&row.line_start);
            params.push(&row.line_end);
            params.push(&row.text);
        }
        let query = format!(
            "INSERT INTO {} (
                snapshot_id, collection, root_id, path, ordinal, symbol_name, kind,
                line_start, line_end, text, tsv
             ) VALUES {}",
            adapter.table("serving_lexical"),
            values.join(", ")
        );
        tx.execute(&query, &params)?;
    }
    Ok(())
}

fn log_snapshot_publish_phase_timings(
    snapshot: &Snapshot,
    timings: &SnapshotPublishPhaseTimings,
    stats: &SnapshotPublishStats,
    total_documents: usize,
) {
    tracing::info!(
        snapshot_id = %snapshot.id.0,
        corpus = %snapshot.corpus.as_str(),
        total_documents,
        grouping_ms = timings.grouping.as_millis() as u64,
        normalized_content_ms = timings.normalized_content.as_millis() as u64,
        lexical_rebuild_ms = timings.lexical_rebuild.as_millis() as u64,
        final_activation_ms = timings.final_activation.as_millis() as u64,
        written_files = stats.written_files,
        reused_files = stats.reused_files,
        deleted_files = stats.deleted_files,
        written_documents = stats.written_documents,
        reused_documents = stats.reused_documents,
        "postgres snapshot publish phase timings"
    );
}

fn log_semantic_publish_phase_timings(
    snapshot_id: &str,
    model_id: &str,
    dimension: usize,
    strategy: &SemanticPublishStrategy,
    timings: &SemanticPublishPhaseTimings,
    stats: &SemanticPublishLogStats,
) {
    let parent_snapshot_id = match strategy {
        SemanticPublishStrategy::IncrementalFromParent { parent_snapshot_id } => {
            parent_snapshot_id.as_str()
        }
        _ => "-",
    };
    tracing::info!(
        snapshot_id,
        model_id,
        dimension,
        strategy = strategy.label(),
        parent_snapshot_id,
        ancestry_materialization_ms = timings.ancestry_materialization.as_millis() as u64,
        parent_copy_ms = timings.parent_copy.as_millis() as u64,
        changed_recompute_ms = timings.changed_recompute.as_millis() as u64,
        final_sync_ms = timings.final_sync.as_millis() as u64,
        copied_rows = stats.copied_rows,
        inserted_rows = stats.inserted_rows,
        missing_embeddings = stats.missing_embeddings,
        changed_files = stats.changed_files,
        changed_rows = stats.changed_rows,
        deleted_paths = stats.deleted_paths,
        "postgres semantic publish phase timings"
    );
    if stats.missing_embeddings > 0 {
        tracing::warn!(
            snapshot_id,
            model_id,
            dimension,
            missing_embeddings = stats.missing_embeddings,
            strategy = strategy.label(),
            "semantic publish completed without readiness marker because embeddings were missing"
        );
    }
}

fn pool_connection_reason_code(message: &str) -> ReasonCode {
    let message = message.to_ascii_lowercase();
    if message.contains("password authentication failed")
        || message.contains("authentication failed")
        || message.contains("invalid password")
        || message.contains("saslauth")
    {
        reason::POSTGRES_AUTH_FAILED
    } else {
        reason::POSTGRES_CONNECT_FAILED
    }
}

fn validate_identifier(identifier: &str) -> Result<(), SearchError> {
    if identifier.is_empty()
        || !identifier.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(SearchError::ExternalBaseline(format!(
            "invalid postgres schema identifier: {identifier}"
        )));
    }
    Ok(())
}

fn snapshot_record_from_metadata_row(row: Row) -> BaselineSnapshotRecord {
    BaselineSnapshotRecord {
        snapshot_id: row.get("id"),
        corpus: row.get("corpus"),
        fingerprint: row.get("fingerprint"),
        parent_snapshot_id: row.get("parent_snapshot_id"),
        branch: row.get("branch"),
        commit: row.get("commit_sha"),
        created_at: row.get("created_at"),
        files: 0,
        documents: 0,
    }
}

fn file_object_record_from_row(row: Row) -> BaselineFileObjectRecord {
    BaselineFileObjectRecord {
        file_object_id: row.get("id"),
        collection: row.get("collection"),
        fingerprint: row.get("file_fingerprint"),
        documents: row.get::<_, i32>("document_count") as usize,
        snapshots: row.get::<_, i64>("snapshots") as usize,
    }
}

fn collect_active_embedding_keys(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
) -> Result<HashSet<String>, SearchError> {
    let shared_query = format!(
        "SELECT sf.path,
                foi.kind,
                foi.symbol_name,
                foi.graph_context,
                co.text
         FROM {} sf
         JOIN {} foi ON foi.file_object_id = sf.file_object_id
         JOIN {} co ON co.content_hash = foi.content_hash
         WHERE sf.file_object_id IS NOT NULL",
        adapter.table("snapshot_files"),
        adapter.table("file_object_items"),
        adapter.table("content_objects")
    );

    let mut active_keys = HashSet::new();
    for row in client.query(&shared_query, &[])? {
        active_keys.insert(semantic_key_for_semantic_row(row));
    }
    Ok(active_keys)
}

/// Counts visible files per collection with one server-side aggregate instead of
/// materializing every visible `snapshot_files` row over the wire (25k+ rows on large
/// corpora) just to count them in Rust. The aggregate mirrors the visibility rules of
/// `materialize_visible_snapshot_file_map`: the first occurrence of a
/// `(collection, root_id, path)` key in child-first ancestry order wins, and on a position tie
/// a deletion shadows a file published by the same snapshot (`is_deletion DESC`).
fn effective_snapshot_summary(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<EffectiveSnapshotSummary, SearchError> {
    let ancestry = snapshot_ancestry_ids(client, adapter, snapshot_id)?;
    let query = format!(
        "WITH entries AS (
             SELECT collection, root_id, path, document_count, FALSE AS is_deletion,
                    array_position($1::TEXT[], snapshot_id) AS ancestry_position
             FROM {files}
             WHERE snapshot_id = ANY($1)
             UNION ALL
             SELECT collection, root_id, path, 0 AS document_count, TRUE AS is_deletion,
                    array_position($1::TEXT[], snapshot_id) AS ancestry_position
             FROM {deletions}
             WHERE snapshot_id = ANY($1)
         ),
         winners AS (
             SELECT DISTINCT ON (collection, root_id, path) collection, document_count, is_deletion
             FROM entries
             ORDER BY collection, root_id, path, ancestry_position, is_deletion DESC
         )
         SELECT collection,
                COUNT(*)::BIGINT AS files,
                COALESCE(SUM(document_count), 0)::BIGINT AS documents
         FROM winners
         WHERE NOT is_deletion
         GROUP BY collection",
        files = adapter.table("snapshot_files"),
        deletions = adapter.table("snapshot_deletions"),
    );

    let mut total_files = 0usize;
    let mut total_documents = 0usize;
    let mut collections = Vec::new();
    for row in client.query(&query, &[&ancestry])? {
        let files = row.get::<_, i64>("files").max(0) as usize;
        let documents = row.get::<_, i64>("documents").max(0) as usize;
        total_files += files;
        total_documents += documents;
        collections.push(BaselineCollectionRecord {
            collection: row.get("collection"),
            files,
            documents,
        });
    }
    // Byte-order sort in Rust, not SQL ORDER BY: the materialized path grouped through a
    // BTreeMap and SQL collation order can disagree with it on non-ASCII names.
    collections.sort_by(|left, right| left.collection.cmp(&right.collection));

    Ok(EffectiveSnapshotSummary { total_files, total_documents, collections })
}

/// Per-seed effective (visible) file/document totals for a batch of snapshots, computed in one
/// round-trip instead of walking each snapshot's ancestry and summarising it separately (which
/// `effective_snapshot_summary` does per call — `list_snapshots` over N snapshots of depth D would
/// otherwise issue `1 + N*(D+1)` queries). A `RECURSIVE` CTE expands every seed's ancestry keyed by
/// the seed it belongs to, so the child-first visibility rule
/// (`DISTINCT ON (seed, collection, root_id, path)`
/// by ancestry depth, a same-position deletion shadowing its file) matches the single-snapshot path.
/// Only totals are returned: `list_snapshots` does not use the per-collection breakdown.
///
/// The delicate error semantics of `snapshot_ancestry_ids` are preserved: a parent that points at a
/// missing snapshot and a parent chain that cycles are both surfaced as the same errors, emitted as
/// discriminated rows in the same result so a corrupt chain still fails the listing instead of
/// silently under-counting. Seeds with no visible files simply have no aggregate row (totals 0/0).
fn effective_snapshot_totals_batch(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    seed_ids: &[String],
) -> Result<HashMap<String, (usize, usize)>, SearchError> {
    if seed_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let query = format!(
        "WITH RECURSIVE chain(seed_id, snapshot_id, next_parent, depth, missing) AS (
             SELECT id, id, parent_snapshot_id, 0, FALSE
             FROM {snapshots}
             WHERE id = ANY($1)
             UNION ALL
             SELECT c.seed_id, c.next_parent, s.parent_snapshot_id, c.depth + 1, s.id IS NULL
             FROM chain c
             LEFT JOIN {snapshots} s ON s.id = c.next_parent
             WHERE c.next_parent IS NOT NULL
         ) CYCLE snapshot_id SET is_cycle USING cycle_path,
         entries AS (
             SELECT c.seed_id, f.collection, f.root_id, f.path, f.document_count,
                    FALSE AS is_deletion, c.depth AS ancestry_position
             FROM chain c
             JOIN {files} f ON f.snapshot_id = c.snapshot_id
             WHERE NOT c.is_cycle AND NOT c.missing
             UNION ALL
             SELECT c.seed_id, d.collection, d.root_id, d.path, 0,
                    TRUE AS is_deletion, c.depth AS ancestry_position
             FROM chain c
             JOIN {deletions} d ON d.snapshot_id = c.snapshot_id
             WHERE NOT c.is_cycle AND NOT c.missing
         ),
         winners AS (
             SELECT DISTINCT ON (seed_id, collection, root_id, path)
                    seed_id, document_count, is_deletion
             FROM entries
             ORDER BY seed_id, collection, root_id, path, ancestry_position, is_deletion DESC
         ),
         totals AS (
             SELECT seed_id,
                    COUNT(*)::BIGINT AS files,
                    COALESCE(SUM(document_count), 0)::BIGINT AS documents
             FROM winners
             WHERE NOT is_deletion
             GROUP BY seed_id
         ),
         failures AS (
             SELECT DISTINCT seed_id, snapshot_id AS offending, 'cycle' AS reason
             FROM chain WHERE is_cycle
             UNION
             SELECT DISTINCT seed_id, snapshot_id AS offending, 'missing' AS reason
             FROM chain WHERE missing
         )
         SELECT 'total'::TEXT AS row_kind, seed_id, files, documents,
                NULL::TEXT AS offending, NULL::TEXT AS reason
         FROM totals
         UNION ALL
         SELECT 'failure'::TEXT AS row_kind, seed_id, NULL::BIGINT, NULL::BIGINT, offending, reason
         FROM failures",
        snapshots = adapter.table("snapshots"),
        files = adapter.table("snapshot_files"),
        deletions = adapter.table("snapshot_deletions"),
    );

    let rows = client.query(&query, &[&seed_ids])?;
    let mut totals = HashMap::with_capacity(seed_ids.len());
    let mut failures: HashMap<String, (String, String)> = HashMap::new();
    for row in rows {
        let row_kind: String = row.get("row_kind");
        if row_kind == "failure" {
            failures.insert(row.get("seed_id"), (row.get("offending"), row.get("reason")));
            continue;
        }
        let files = row.get::<_, i64>("files").max(0) as usize;
        let documents = row.get::<_, i64>("documents").max(0) as usize;
        totals.insert(row.get::<_, String>("seed_id"), (files, documents));
    }

    // A corrupt ancestry chain fails the whole listing, as the per-snapshot path did. Report the
    // first seed in listing order that is corrupt so the error is deterministic when several seeds
    // are corrupt at once (the batch result set has no inherent order).
    for seed_id in seed_ids {
        if let Some((offending, reason)) = failures.get(seed_id) {
            return Err(SearchError::ExternalBaseline(match reason.as_str() {
                "cycle" => format!("snapshot parent chain contains cycle at '{offending}'"),
                _ => format!("snapshot '{offending}' was not found"),
            }));
        }
    }
    Ok(totals)
}

fn materialize_visible_snapshot_file_map(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<BTreeMap<VisibleFileKey, VisibleSnapshotFile>, SearchError> {
    let ancestry = snapshot_ancestry_ids(client, adapter, snapshot_id)?;
    let file_query = format!(
        "SELECT snapshot_id, collection, root_id, path,
                file_fingerprint, document_count, file_object_id
         FROM {}
         WHERE snapshot_id = ANY($1)",
        adapter.table("snapshot_files")
    );
    let deletion_query = format!(
        "SELECT snapshot_id, collection, root_id, path
         FROM {}
         WHERE snapshot_id = ANY($1)",
        adapter.table("snapshot_deletions")
    );

    let mut files_by_snapshot = HashMap::<String, Vec<VisibleSnapshotFile>>::new();
    for row in client.query(&file_query, &[&ancestry])? {
        let snapshot_id: String = row.get("snapshot_id");
        files_by_snapshot.entry(snapshot_id).or_default().push(VisibleSnapshotFile {
            collection: row.get("collection"),
            root_id: row.get("root_id"),
            path: row.get("path"),
            file_fingerprint: row.get("file_fingerprint"),
            document_count: row.get::<_, i32>("document_count") as usize,
            file_object_id: row.get("file_object_id"),
        });
    }
    let mut deletions_by_snapshot = HashMap::<String, Vec<VisibleFileKey>>::new();
    for row in client.query(&deletion_query, &[&ancestry])? {
        let snapshot_id: String = row.get("snapshot_id");
        deletions_by_snapshot.entry(snapshot_id).or_default().push((
            row.get("collection"),
            row.get("root_id"),
            row.get("path"),
        ));
    }

    Ok(fold_visible_files(&ancestry, &mut files_by_snapshot, &mut deletions_by_snapshot))
}

/// Which file of an ancestry is the one a snapshot serves, walking child-first.
///
/// A pure function over rows the caller already read, because this is where the whole
/// visibility rule lives: the first occurrence of a key wins, and a deletion recorded closer
/// to the child shadows every ancestor's file under that same key. Reachable only through a
/// live database in its caller, it would be observable nowhere else — and a rule this central
/// must be answerable without one.
fn fold_visible_files(
    ancestry: &[String],
    files_by_snapshot: &mut HashMap<String, Vec<VisibleSnapshotFile>>,
    deletions_by_snapshot: &mut HashMap<String, Vec<VisibleFileKey>>,
) -> BTreeMap<VisibleFileKey, VisibleSnapshotFile> {
    let mut seen_paths = HashSet::<VisibleFileKey>::new();
    let mut visible_files = BTreeMap::<VisibleFileKey, VisibleSnapshotFile>::new();
    for snapshot_id in ancestry {
        if let Some(deletions) = deletions_by_snapshot.remove(snapshot_id) {
            for key in deletions {
                seen_paths.insert(key);
            }
        }
        if let Some(files) = files_by_snapshot.remove(snapshot_id) {
            for file in files {
                let key = file.key();
                if seen_paths.insert(key.clone()) {
                    visible_files.insert(key, file);
                }
            }
        }
    }
    visible_files
}

fn materialize_visible_snapshot_files(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<Vec<VisibleSnapshotFile>, SearchError> {
    Ok(materialize_visible_snapshot_file_map(client, adapter, snapshot_id)?.into_values().collect())
}

fn snapshot_ancestry_ids(
    client: &mut impl GenericClient,
    adapter: &PostgresBaselineAdapter,
    snapshot_id: &str,
) -> Result<Vec<String>, SearchError> {
    let query = format!(
        "SELECT id, parent_snapshot_id
         FROM {}
         WHERE id = $1
         LIMIT 1",
        adapter.table("snapshots")
    );

    let mut ancestry = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(snapshot_id.to_owned());
    while let Some(snapshot_id) = current.take() {
        if !seen.insert(snapshot_id.clone()) {
            return Err(SearchError::ExternalBaseline(format!(
                "snapshot parent chain contains cycle at '{}'",
                snapshot_id
            )));
        }
        let Some(row) = client.query_opt(&query, &[&snapshot_id])? else {
            return Err(SearchError::ExternalBaseline(format!(
                "snapshot '{}' was not found",
                snapshot_id
            )));
        };
        ancestry.push(row.get::<_, String>("id"));
        current = row.get("parent_snapshot_id");
    }

    Ok(ancestry)
}

fn semantic_key_for_semantic_row(row: Row) -> String {
    let path: String = row.get("path");
    let kind: String = row.get("kind");
    let symbol_name: String = row.get("symbol_name");
    let graph_context: Option<String> = row.get("graph_context");
    let text: String = row.get("text");
    crate::document::semantic_key_from_parts(
        &path,
        &kind,
        &symbol_name,
        graph_context.as_deref().unwrap_or(""),
        &text,
    )
}

fn query_string_column(
    client: &mut impl GenericClient,
    query: &str,
    params: &[&(dyn postgres::types::ToSql + Sync)],
) -> Result<Vec<String>, SearchError> {
    Ok(client.query(query, params)?.into_iter().map(|row| row.get(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::{
        effective_snapshot_totals_batch, file_object_id_for, fingerprint_file_documents,
        fold_visible_files, group_documents_by_file, materialize_visible_snapshot_files,
        semantic_publication_complete, semantic_publish_phase_count, semantic_publish_plan,
        semantic_publish_strategy, unique_content_object_rows, CarrierObligation, ContentObjectRow,
        EffectiveSnapshotSummary, PostgresBaselineAdapter, SemanticPublishPlan,
        SemanticPublishStrategy, VisibleFileKey, VisibleSnapshotFile, ROOTED_CARRIER_KEYS,
        TOKEN_LAYOUT_PARTS_TABLE,
    };
    use crate::domain::{
        CorpusId, ExternalBaselineConfig, Snapshot, SnapshotPublishMetadata, SourceSpan,
    };
    use crate::external_baseline::BaselineCollectionRecord;
    use crate::ports::{
        BaselineLexicalSearch, BaselineSemanticSearch, SnapshotCatalog, SnapshotContentStore,
        SnapshotPublisher, WorkspaceBaselineManifestStore,
    };
    use crate::workspace_roots::CONFIGURATION_ROOT_ID;
    use crate::{BaselineRef, IndexedDocument};
    use std::collections::HashMap;

    /// Two facts hold this file together, and neither is visible from where the other lives.
    ///
    /// The garbage collector rebuilds the set of live embedding keys from `snapshot_files.path`
    /// with no root at all (`collect_active_embedding_keys`), and that is CORRECT only because
    /// the embedding key ignores the root by construction. Fold the root into the key recipe
    /// and the collector starts calling live vectors orphans — silently, and the payment is
    /// their deletion.
    ///
    /// So the recipe is pinned here too, next to the storage that depends on it, and not only
    /// in `document.rs` where it is written.
    #[test]
    fn the_serving_side_key_recipe_ignores_the_root() {
        let source = include_str!("postgres.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(source);
        let recipe = production
            .split_once("fn semantic_key_for_semantic_row")
            .expect("the serving-side key recomputation moved; this gate scans what it can find")
            .1;
        let recipe = recipe.split_once("\nfn ").map(|(body, _)| body).unwrap_or(recipe);

        assert!(
            !recipe.contains("root_id"),
            "recomputing a stored row's embedding key must not read the root: the collector \
             that decides which vectors are live cannot see it, and would delete what it \
             cannot match"
        );
    }

    /// A root identified by an absolute path is refused before the adapter reaches the database.
    ///
    /// Observed against an address nothing listens on: an implementation that stored first and
    /// refused afterwards would have to connect, and there is no connection to be had. The
    /// positive control is the same call with a workspace-relative root, which must get past the
    /// refusal and fail on the network — without it the assertion holds under any error at all.
    #[test]
    fn a_root_named_by_an_absolute_path_is_refused_before_the_connection() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();
        let snapshot = Snapshot::new("snap-1", CorpusId::WorkspaceCode);
        let metadata = SnapshotPublishMetadata::default();
        let portable = IndexedDocument {
            root_id: "src/cfe/Расш".to_owned(),
            ..indexed_document("code", "CommonModules/Общий/Ext/Module.bsl", "Общий", 1, "h", "t")
        };
        let outside =
            IndexedDocument { root_id: "/srv/extensions/Расш".to_owned(), ..portable.clone() };

        let refused = adapter
            .publish_snapshot(&snapshot, &metadata, std::slice::from_ref(&outside))
            .unwrap_err();
        assert_eq!(
            refused.reason_code(),
            Some("root_id_not_portable"),
            "the refusal must be named, and named before any connection: {refused}"
        );
        assert!(
            !refused.to_string().contains("connection"),
            "refusing after connecting means the identity already reached the wire: {refused}"
        );

        let reached_the_network = adapter
            .publish_snapshot(&snapshot, &metadata, std::slice::from_ref(&portable))
            .unwrap_err()
            .to_string();
        assert!(
            reached_the_network.contains("connection"),
            "positive control: a root inside the project must get past the refusal: \
             {reached_the_network}"
        );
    }

    /// A file is a `(collection, root_id, path)`, so the same relative path under two roots is
    /// two files with two fingerprints — not one file whose chunks got merged.
    #[test]
    fn two_roots_sharing_a_relative_path_are_two_published_files() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let configuration =
            indexed_document("code", PATH, "Общий", 1, "hash-конфигурации", "текст");
        let extension = IndexedDocument {
            root_id: "Расш".to_owned(),
            content_hash: "hash-расширения".to_owned(),
            ..configuration.clone()
        };

        let groups = group_documents_by_file(&[configuration, extension]);

        assert_eq!(
            groups.iter().map(|group| group.root_id.as_str()).collect::<Vec<_>>(),
            vec![CONFIGURATION_ROOT_ID, "Расш"],
            "merging them would publish one row holding the chunks of two different files"
        );
    }

    fn deletion_key(root_id: &str, path: &str) -> VisibleFileKey {
        visible_file(root_id, path).key()
    }

    fn visible_file(root_id: &str, path: &str) -> VisibleSnapshotFile {
        VisibleSnapshotFile {
            collection: "code".to_owned(),
            root_id: root_id.to_owned(),
            path: path.to_owned(),
            file_fingerprint: format!("fp-{root_id}-{path}"),
            document_count: 1,
            file_object_id: format!("obj-{root_id}-{path}"),
        }
    }

    /// The whole visibility rule in one place: a deletion recorded by a child shadows the
    /// ancestor's file under the SAME key — and an extension's file is not the
    /// configuration's, however identical their relative paths look.
    #[test]
    fn a_deletion_shadows_only_its_own_root() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let ancestry = vec!["child".to_owned(), "parent".to_owned()];
        let mut files = HashMap::from([(
            "parent".to_owned(),
            vec![visible_file(CONFIGURATION_ROOT_ID, PATH), visible_file("Расш", PATH)],
        )]);
        let mut deletions = HashMap::from([("child".to_owned(), vec![deletion_key("Расш", PATH)])]);

        let visible = fold_visible_files(&ancestry, &mut files, &mut deletions);

        assert_eq!(
            visible.values().map(|file| file.root_id.as_str()).collect::<Vec<_>>(),
            vec![CONFIGURATION_ROOT_ID],
            "deleting the extension's file must leave the configuration's alone: they are two \
             different files that merely share a relative path"
        );
    }

    /// The positive control for the rule above: without it, an implementation that shadows
    /// nothing at all passes just as well.
    #[test]
    fn a_deletion_shadows_the_file_of_its_own_root() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let ancestry = vec!["child".to_owned(), "parent".to_owned()];
        let mut files = HashMap::from([(
            "parent".to_owned(),
            vec![visible_file(CONFIGURATION_ROOT_ID, PATH), visible_file("Расш", PATH)],
        )]);
        let mut deletions =
            HashMap::from([("child".to_owned(), vec![deletion_key(CONFIGURATION_ROOT_ID, PATH)])]);

        let visible = fold_visible_files(&ancestry, &mut files, &mut deletions);

        assert_eq!(
            visible.values().map(|file| file.root_id.as_str()).collect::<Vec<_>>(),
            vec!["Расш"],
            "a deletion must still shadow the file it names"
        );
    }

    /// The other half of the rule, and the half no deletion takes part in: with the same key
    /// published twice, the row closer to the child wins. An ancestor's row is an older
    /// publication of that same file, so serving it would hand out a fingerprint and a file
    /// object the corpus has already replaced.
    #[test]
    fn the_file_closer_to_the_child_wins_over_its_ancestors() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let ancestry = vec!["child".to_owned(), "parent".to_owned()];
        let republished = VisibleSnapshotFile {
            file_fingerprint: "fp-republished".to_owned(),
            ..visible_file(CONFIGURATION_ROOT_ID, PATH)
        };
        let mut files = HashMap::from([
            ("child".to_owned(), vec![republished]),
            ("parent".to_owned(), vec![visible_file(CONFIGURATION_ROOT_ID, PATH)]),
        ]);
        let mut deletions = HashMap::new();

        let visible = fold_visible_files(&ancestry, &mut files, &mut deletions);

        assert_eq!(
            visible.values().map(|file| file.file_fingerprint.as_str()).collect::<Vec<_>>(),
            vec!["fp-republished"],
            "the ancestor's row must not overwrite the republished file: ancestry is walked \
             child-first precisely so the newest publication of a key is the visible one"
        );
    }

    /// Every carrier of file identity keys it by the root, and none still declares the key
    /// without it.
    ///
    /// Both the carriers and the expected keys come from `ROOTED_CARRIER_KEYS`, so a carrier
    /// added there is covered by the act of declaring it. A hand-written copy of the list here
    /// would be the second answer the constant's own doc warns about — and the one nobody looks
    /// at is the one that stays wrong.
    ///
    /// The optional carrier's `CREATE TABLE` lives in the tolerant half of the migration, so both
    /// halves are searched; what must be in the mandatory half for every carrier is the backfill
    /// and the key, because those are what an ALREADY created schema needs.
    #[test]
    fn every_carrier_keys_the_file_by_its_root() {
        let adapter =
            PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres("postgres://example"))
                .unwrap();
        let mandatory = adapter.ensure_schema_statements();
        let mut statements = mandatory.clone();
        statements.extend(adapter.pgvector_schema_statements());

        for (table, columns, _) in ROOTED_CARRIER_KEYS {
            let qualified = adapter.table(table);
            let owned: Vec<&String> =
                statements.iter().filter(|statement| statement.contains(&qualified)).collect();
            assert!(!owned.is_empty(), "no statement mentions {qualified}");

            let expected_key = format!("PRIMARY KEY ({})", columns.join(", "));
            assert!(
                owned.iter().any(|statement| statement.contains(&expected_key)),
                "{table} must declare {expected_key}"
            );

            let rootless: Vec<&str> =
                columns.iter().copied().filter(|column| *column != "root_id").collect();
            let rootless_key = format!("PRIMARY KEY ({})", rootless.join(", "));
            assert!(
                !owned.iter().any(|statement| statement.contains(&rootless_key)),
                "{table} still declares {rootless_key}, so two roots collapse into one row"
            );

            let backfill = mandatory
                .iter()
                .position(|statement| {
                    statement.contains(&qualified)
                        && statement.contains("ADD COLUMN IF NOT EXISTS root_id")
                })
                .unwrap_or_else(|| {
                    panic!("{table} must gain root_id on schemas that predate roots")
                });

            // And it must come before everything that KEYS by the root without creating it —
            // the primary key and the indexes. Those fail outright on a schema where the column
            // does not exist yet, taking the whole migration down with them; presence alone is
            // green for a statement list in any order. `CREATE TABLE` is exempt because it
            // declares the column in the same breath.
            // The backfill must belong to THIS carrier alone. Merge the carriers back into one
            // script and every check above is satisfied by some other carrier's text — the exact
            // blindness this gate was rewritten to remove, and nothing else would notice it.
            for other in ROOTED_CARRIER_KEYS
                .iter()
                .map(|(other, _, _)| adapter.table(other))
                .filter(|other| *other != qualified)
            {
                assert!(
                    !mandatory[backfill].contains(&other),
                    "{table}'s backfill also names {other}: one script for several carriers makes \
                     every carrier's check pass on another's text"
                );
            }

            let keyed_by_root = mandatory.iter().enumerate().filter(|(position, statement)| {
                *position != backfill
                    && statement.contains(&qualified)
                    && statement.contains("root_id")
                    && !statement.contains("CREATE TABLE")
            });
            for (position, statement) in keyed_by_root {
                assert!(
                    position > backfill,
                    "{table} is keyed by root_id before the column is added, at statement \
                     {position} against a backfill at {backfill}: {statement}"
                );
            }
        }
    }

    #[test]
    fn defaults_to_bsl_search_schema() {
        let adapter =
            PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres("postgres://example"))
                .unwrap();

        assert_eq!(adapter.config().schema, None);
        assert_eq!(adapter.table("snapshots"), "bsl_search.snapshots");
        assert!(adapter.token_layout_claim.is_none());
    }

    #[test]
    fn companion_layout_is_opt_in_and_not_part_of_legacy_migration() {
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres("postgres://example").with_schema("layout_test"),
        )
        .unwrap();
        assert!(adapter
            .ensure_schema_statements()
            .iter()
            .all(|statement| !statement.contains(TOKEN_LAYOUT_PARTS_TABLE)));

        let configured = adapter
            .with_token_layout_claim(crate::token_policy::SEGMENTATION_VERSION, "frozen-claim")
            .unwrap();
        assert_eq!(
            configured
                .token_layout_claim
                .as_ref()
                .map(|(version, claim)| (version.as_str(), claim.as_str())),
            Some((crate::token_policy::SEGMENTATION_VERSION, "frozen-claim"))
        );
        assert!(configured.with_token_layout_claim("unknown-layout", "frozen-claim").is_err());
    }

    #[test]
    fn schema_validation_rejects_invalid_identifier() {
        let error = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres("postgres://example").with_schema("bad-schema"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid postgres schema"));
    }

    #[test]
    fn connection_errors_surface_from_runtime_queries() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();
        let baseline = BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "snapshot-1");

        let error = adapter.resolve_baseline(&baseline).unwrap_err();
        assert!(error.to_string().contains("connection"));
    }

    #[test]
    fn connection_errors_surface_from_storage_migration() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();

        let error = adapter.migrate_storage().unwrap_err();
        assert!(error.to_string().contains("connection"));
    }

    #[test]
    fn connection_errors_surface_from_baseline_lexical_search() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();

        let error = adapter.lexical_search_baseline("snapshot-1", "Найти", None, 10).unwrap_err();
        assert!(error.to_string().contains("connection"));
    }

    #[test]
    fn lexical_search_returns_empty_for_blank_query() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();

        let result = adapter.lexical_search_baseline("snapshot-1", "", None, 10).unwrap();
        assert!(result.is_empty());

        let result = adapter.lexical_search_baseline("snapshot-1", "   ", None, 10).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn semantic_search_returns_empty_for_empty_embedding() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();

        let result =
            adapter.semantic_search_baseline("snapshot-1", &[], "model-1", 1024, None, 10).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn connection_errors_surface_from_semantic_search() {
        let adapter = PostgresBaselineAdapter::new(ExternalBaselineConfig::postgres(
            "postgres://127.0.0.1:1",
        ))
        .unwrap();

        let error = adapter
            .semantic_search_baseline("snapshot-1", &[0.1, 0.2, 0.3], "model-1", 3, None, 10)
            .unwrap_err();
        assert!(error.to_string().contains("connection"));
    }

    #[test]
    fn semantic_publish_phase_count_matches_strategy() {
        assert_eq!(semantic_publish_phase_count(&SemanticPublishStrategy::CurrentSnapshotOnly), 2);
        assert_eq!(semantic_publish_phase_count(&SemanticPublishStrategy::FullRebuild), 2);
        assert_eq!(
            semantic_publish_phase_count(&SemanticPublishStrategy::IncrementalFromParent {
                parent_snapshot_id: "parent-1".to_owned()
            }),
            3
        );
    }

    #[test]
    fn semantic_publish_strategy_exposes_parent_snapshot_id_only_for_incremental_reuse() {
        assert_eq!(SemanticPublishStrategy::CurrentSnapshotOnly.parent_snapshot_id(), None);
        assert_eq!(SemanticPublishStrategy::FullRebuild.parent_snapshot_id(), None);
        assert_eq!(
            SemanticPublishStrategy::IncrementalFromParent {
                parent_snapshot_id: "parent-1".to_owned()
            }
            .parent_snapshot_id(),
            Some("parent-1")
        );
    }

    #[test]
    fn semantic_publish_requires_completion_marker_for_parent_reuse() {
        assert_eq!(
            semantic_publish_strategy(Some("parent-1".to_owned()), false),
            SemanticPublishStrategy::FullRebuild
        );
        assert_eq!(
            semantic_publish_strategy(Some("parent-1".to_owned()), true),
            SemanticPublishStrategy::IncrementalFromParent {
                parent_snapshot_id: "parent-1".to_owned()
            }
        );
    }

    #[test]
    fn semantic_publish_plan_tracks_changed_and_deleted_paths_for_incremental_reuse() {
        let plan = semantic_publish_plan(
            Some("parent-1".to_owned()),
            true,
            &[
                visible_snapshot_file("code", "src/New.bsl"),
                visible_snapshot_file("code", "src/Renamed.bsl"),
            ],
            &[("code".to_owned(), CONFIGURATION_ROOT_ID.to_owned(), "src/Old.bsl".to_owned())],
        );

        assert_eq!(
            plan,
            SemanticPublishPlan {
                strategy: SemanticPublishStrategy::IncrementalFromParent {
                    parent_snapshot_id: "parent-1".to_owned()
                },
                changed_paths: [
                    ("code".to_owned(), CONFIGURATION_ROOT_ID.to_owned(), "src/New.bsl".to_owned()),
                    (
                        "code".to_owned(),
                        CONFIGURATION_ROOT_ID.to_owned(),
                        "src/Renamed.bsl".to_owned()
                    )
                ]
                .into_iter()
                .collect(),
                deleted_paths: [(
                    "code".to_owned(),
                    CONFIGURATION_ROOT_ID.to_owned(),
                    "src/Old.bsl".to_owned()
                )]
                .into_iter()
                .collect(),
            }
        );
    }

    /// One relative path under two roots is TWO files on both sides of the plan.
    ///
    /// An extension repeats the configuration's layout, so this is the ordinary shape of a
    /// project with extensions rather than an edge case. Both halves are asserted: each is
    /// reported to the operator separately, and a key that keeps the root on the changed side
    /// while losing it on the deleted side is green in every other test here.
    #[test]
    fn one_path_under_two_roots_counts_as_two_files_on_both_sides_of_the_plan() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let plan = semantic_publish_plan(
            Some("parent-1".to_owned()),
            true,
            &[
                rooted_visible_snapshot_file("code", CONFIGURATION_ROOT_ID, PATH),
                rooted_visible_snapshot_file("code", "Расш", PATH),
            ],
            &[
                ("code".to_owned(), CONFIGURATION_ROOT_ID.to_owned(), "src/Gone.bsl".to_owned()),
                ("code".to_owned(), "Расш".to_owned(), "src/Gone.bsl".to_owned()),
            ],
        );

        assert_eq!(
            plan.changed_paths.len(),
            2,
            "two roots sharing one relative path are two changed files: {:?}",
            plan.changed_paths
        );
        assert_eq!(
            plan.deleted_paths.len(),
            2,
            "and two deletions, which the operator is told about separately: {:?}",
            plan.deleted_paths
        );
    }

    #[test]
    fn semantic_publish_plan_falls_back_to_full_rebuild_without_parent_marker() {
        let plan = semantic_publish_plan(
            Some("parent-1".to_owned()),
            false,
            &[visible_snapshot_file("code", "src/Changed.bsl")],
            &[("code".to_owned(), CONFIGURATION_ROOT_ID.to_owned(), "src/Deleted.bsl".to_owned())],
        );

        assert_eq!(plan.strategy, SemanticPublishStrategy::FullRebuild);
        assert_eq!(plan.changed_paths.len(), 1);
        assert_eq!(plan.deleted_paths.len(), 1);
    }

    #[test]
    fn semantic_publish_uses_current_snapshot_only_without_parent() {
        assert_eq!(
            semantic_publish_strategy(None, false),
            SemanticPublishStrategy::CurrentSnapshotOnly
        );
    }

    #[test]
    fn unique_content_object_rows_deduplicates_repeated_hashes() {
        let rows = vec![
            ContentObjectRow { content_hash: "hash-a".to_owned(), text: "text-a".to_owned() },
            ContentObjectRow { content_hash: "hash-a".to_owned(), text: "text-a".to_owned() },
            ContentObjectRow { content_hash: "hash-b".to_owned(), text: "text-b".to_owned() },
        ];

        let unique_rows = unique_content_object_rows(&rows).unwrap();

        assert_eq!(unique_rows.len(), 2);
        assert_eq!(unique_rows[0].content_hash, "hash-a");
        assert_eq!(unique_rows[1].content_hash, "hash-b");
    }

    #[test]
    fn unique_content_object_rows_rejects_conflicting_text_for_same_hash() {
        let rows = vec![
            ContentObjectRow { content_hash: "hash-a".to_owned(), text: "text-a".to_owned() },
            ContentObjectRow { content_hash: "hash-a".to_owned(), text: "text-b".to_owned() },
        ];

        let error = unique_content_object_rows(&rows).unwrap_err();

        assert!(error.to_string().contains("content hash collision within publish batch"));
    }

    #[test]
    fn group_documents_by_file_merges_same_path_and_sorts_chunks() {
        let documents = vec![
            indexed_document("code", "src/A.bsl", "B", 20, "hash-b", "text-b"),
            indexed_document("code", "src/A.bsl", "A", 10, "hash-a", "text-a"),
            indexed_document("code", "src/B.bsl", "C", 5, "hash-c", "text-c"),
        ];

        let groups = group_documents_by_file(&documents);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].collection, "code");
        assert_eq!(groups[0].path, "src/A.bsl");
        assert_eq!(groups[0].documents.len(), 2);
        assert_eq!(groups[0].documents[0].symbol_name, "A");
        assert_eq!(groups[0].documents[1].symbol_name, "B");
    }

    #[test]
    fn file_fingerprint_changes_when_chunk_content_changes() {
        let documents = vec![indexed_document("code", "src/A.bsl", "A", 10, "hash-a", "text-a")];
        let changed =
            vec![indexed_document("code", "src/A.bsl", "A", 10, "hash-a", "changed-text")];

        assert_ne!(fingerprint_file_documents(&documents), fingerprint_file_documents(&changed));
    }

    #[test]
    fn file_fingerprint_changes_when_source_part_provenance_changes() {
        let mut document = indexed_document("code", "src/A.bsl", "A", 10, "hash-a", "text-a");
        document.source_span = Some(SourceSpan {
            parent_symbol: "A".to_owned(),
            byte_start: 10,
            byte_end: 20,
            parent_byte_start: 10,
            parent_byte_end: 20,
            part_index: 1,
            part_count: 1,
        });
        let original = fingerprint_file_documents(std::slice::from_ref(&document));
        document.source_span.as_mut().unwrap().parent_symbol = "renamed".to_owned();
        assert_ne!(original, fingerprint_file_documents(&[document]));
    }

    #[test]
    fn file_fingerprint_preserves_legacy_graph_context_recipe() {
        const NO_CONTEXT: &str = "de50ce142750232ee9a57e75f9fe9bb483b596deec09dedea9a0c29c69007b01";
        const WITH_CONTEXT: &str =
            "75f07ad2e7b8dd93e20110ceeb47e9e75cf387943d361f0d674c3c61893025f1";
        let document = indexed_document("code", "src/A.bsl", "A", 10, "hash-a", "text-a");
        let no_context = fingerprint_file_documents(std::slice::from_ref(&document));
        let mut with_context = document;
        with_context.graph_context = Some("Calls: B".to_owned());
        let with_context = fingerprint_file_documents(&[with_context]);
        assert_eq!(no_context, NO_CONTEXT);
        assert_eq!(with_context, WITH_CONTEXT);
    }

    /// The manifest carries the fingerprint computed here, and the working-tree
    /// side recomputes its own to decide whether a file differs from the
    /// baseline. Disagreeing recipes make every file read as locally changed,
    /// so the whole corpus lives as an overlay delta.
    ///
    /// The expected value is pinned from THIS side. Published manifests were
    /// computed with this recipe, so it is the one that cannot move; a check
    /// that merely compared the two live functions would be satisfied just as
    /// well by changing the publisher, which would leave every published
    /// manifest mismatched and this check green.
    #[test]
    fn the_working_tree_recipe_reproduces_the_published_fingerprint() {
        const PUBLISHED: &str = "4e5ca8d990e5af2764397a6700dc0e46f66141881870dcf946a0829a334e3804";
        let rel_path = "CommonModules/Модуль/Ext/Module.bsl";
        let content = "Процедура Первая() Экспорт\nКонецПроцедуры\n\n\
                       Функция Вторая() Экспорт\n\tВозврат 1;\nКонецФункции\n";
        // Built through the shared chunk→document builder the overlay uses, so
        // the two recipes are fed the very same documents and only the recipes
        // themselves are under test.
        let documents: Vec<IndexedDocument> = code_chunk::Chunker::chunk(content)
            .iter()
            .map(|chunk| {
                crate::document::indexed_document_for_chunk(
                    &crate::FileKey::configuration(rel_path),
                    chunk,
                    content,
                    None,
                )
                .0
            })
            .collect();
        assert!(documents.len() > 1, "the fixture must exercise more than one chunk");

        assert_eq!(
            fingerprint_file_documents(&documents),
            PUBLISHED,
            "the published recipe must not move: manifests already carry it"
        );
        assert_eq!(
            crate::workspace_overlay::fingerprint_overlay_documents(&documents, rel_path),
            PUBLISHED,
            "the overlay's document recipe must reproduce the published fingerprint"
        );
        assert_eq!(
            crate::workspace_overlay::fingerprint_content(content, rel_path),
            PUBLISHED,
            "the from-disk recipe must reproduce the published fingerprint"
        );
    }

    /// The behaviour the byte-level pin above exists for: a working tree that
    /// *is* the published snapshot must report nothing local at all.
    ///
    /// The manifest is produced by the publisher's own pipeline — index the
    /// directory, load the documents, group them by file — instead of by
    /// calling the working-tree recipe and comparing it with itself. That is
    /// what makes this able to fail: with the two recipes apart, an untouched
    /// checkout reports every one of its files as a local change.
    #[test]
    fn a_working_tree_equal_to_the_snapshot_has_no_overlay() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let publisher_dir = tempfile::tempdir().unwrap();
        let workspace = workspace_dir.path();
        let file = workspace.join("CommonModule.bsl");
        std::fs::write(&file, "Процедура Базовая() Экспорт\nКонецПроцедуры\n").unwrap();
        // Two methods on one physical line: their chunks share a line span, so
        // the publisher's sort puts them in a different order than the chunker
        // returns them. Ordinary code cannot show that — its line numbers rise
        // with position, and both orders coincide.
        std::fs::write(
            workspace.join("OneLine.bsl"),
            "Процедура Б() Экспорт КонецПроцедуры Процедура А() Экспорт КонецПроцедуры\n",
        )
        .unwrap();

        let mut publisher =
            crate::SearchEngine::fts_only(&publisher_dir.path().join("baseline.db")).unwrap();
        publisher.index_directory_fts(workspace).unwrap();
        let published = publisher.load_indexed_documents(Some("code")).unwrap();
        let groups = group_documents_by_file(&published);
        assert_eq!(groups.len(), 2, "the fixture publishes both files");

        let manifest = crate::WorkspaceBaselineManifest {
            snapshot_id: "snap-1".to_owned(),
            snapshot_fingerprint: Some("fp-1".to_owned()),
            files: groups
                .iter()
                .map(|group| crate::BaselineManifestFile {
                    root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
                    collection: group.collection.clone(),
                    path: group.path.clone(),
                    file_fingerprint: group.file_fingerprint.clone(),
                    document_count: group.documents.len(),
                    file_object_id: file_object_id_for(&group.collection, &group.file_fingerprint),
                })
                .collect(),
        };

        // A fresh engine per measurement, primed explicitly. `stats` alone never
        // walks the tree — it is the read-only status path — so on its own it
        // reports "no local changes" for an edited file just as happily as for
        // an untouched one.
        let overlay_files = |db: &str| {
            let mut engine = crate::SearchEngine::fts_only(&publisher_dir.path().join(db)).unwrap();
            engine.set_workspace_root(workspace);
            engine.set_serves_external_baseline(true).unwrap();
            engine.store().save_baseline_manifest(&manifest).unwrap();
            engine.prime_workspace_overlay().unwrap();
            engine.workspace_overlay_stats().unwrap().unwrap().overlay_files
        };

        assert_eq!(overlay_files("clean.db"), 0, "an untouched checkout has no local changes");

        // Zero above means nothing on its own — an overlay that never scanned
        // reports the same. Editing the very file the manifest describes must
        // move it.
        std::fs::write(&file, "Процедура Базовая() Экспорт\n\tВозврат;\nКонецПроцедуры\n").unwrap();
        assert_eq!(overlay_files("edited.db"), 1, "an edited file is a local change");
    }

    #[test]
    fn file_object_id_is_stable_for_same_collection_and_fingerprint() {
        let id_a = file_object_id_for("code", "abc");
        let id_b = file_object_id_for("code", "abc");
        let id_c = file_object_id_for("platform", "abc");

        assert_eq!(id_a, id_b);
        assert_ne!(id_a, id_c);
    }

    fn indexed_document(
        collection: &str,
        path: &str,
        symbol_name: &str,
        line_start: u32,
        content_hash: &str,
        text: &str,
    ) -> IndexedDocument {
        IndexedDocument {
            collection: collection.to_owned(),
            root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
            path: path.to_owned(),
            symbol_name: symbol_name.to_owned(),
            kind: "procedure".to_owned(),
            line_start,
            line_end: line_start + 1,
            text: text.to_owned(),
            content_hash: content_hash.to_owned(),
            graph_context: None,
            source_span: None,
        }
    }

    fn visible_snapshot_file(collection: &str, path: &str) -> VisibleSnapshotFile {
        rooted_visible_snapshot_file(collection, CONFIGURATION_ROOT_ID, path)
    }

    fn rooted_visible_snapshot_file(
        collection: &str,
        root_id: &str,
        path: &str,
    ) -> VisibleSnapshotFile {
        VisibleSnapshotFile {
            collection: collection.to_owned(),
            root_id: root_id.to_owned(),
            path: path.to_owned(),
            file_fingerprint: format!("fp:{collection}:{root_id}:{path}"),
            document_count: 1,
            file_object_id: format!("fo:{collection}:{root_id}:{path}"),
        }
    }

    /// Raises the pre-root shape of the schema with raw SQL.
    ///
    /// It cannot be built from the node's own steps: after this node the only producer of DDL
    /// always emits the root column, so a legacy schema has to be written by hand or the
    /// migration is never observed doing anything.
    fn raise_pre_root_schema(adapter: &PostgresBaselineAdapter, schema: &str) {
        let mut client = adapter.connect().unwrap();
        client
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {schema};
                 CREATE TABLE {schema}._schema_metadata_ (
                     setting TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 INSERT INTO {schema}._schema_metadata_ VALUES ('schema_version', '1');
                 CREATE TABLE {schema}.snapshots (
                     id TEXT PRIMARY KEY,
                     corpus TEXT NOT NULL,
                     fingerprint TEXT NULL,
                     parent_snapshot_id TEXT NULL,
                     branch TEXT NULL,
                     commit_sha TEXT NULL,
                     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                 );
                 CREATE TABLE {schema}.file_objects (
                     id TEXT PRIMARY KEY,
                     collection TEXT NOT NULL,
                     file_fingerprint TEXT NOT NULL,
                     document_count INTEGER NOT NULL
                 );
                 CREATE TABLE {schema}.snapshot_files (
                     snapshot_id TEXT NOT NULL REFERENCES {schema}.snapshots(id) ON DELETE CASCADE,
                     collection TEXT NOT NULL,
                     path TEXT NOT NULL,
                     file_fingerprint TEXT NOT NULL,
                     document_count INTEGER NOT NULL,
                     file_object_id TEXT NOT NULL REFERENCES {schema}.file_objects(id),
                     PRIMARY KEY (snapshot_id, collection, path)
                 );
                 CREATE TABLE {schema}.snapshot_deletions (
                     snapshot_id TEXT NOT NULL REFERENCES {schema}.snapshots(id) ON DELETE CASCADE,
                     collection TEXT NOT NULL,
                     path TEXT NOT NULL,
                     PRIMARY KEY (snapshot_id, collection, path)
                 );
                 CREATE TABLE {schema}.serving_lexical (
                     snapshot_id TEXT NOT NULL REFERENCES {schema}.snapshots(id) ON DELETE CASCADE,
                     collection TEXT NOT NULL,
                     path TEXT NOT NULL,
                     ordinal INTEGER NOT NULL,
                     symbol_name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     line_start INTEGER NOT NULL,
                     line_end INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     tsv TSVECTOR NOT NULL,
                     PRIMARY KEY (snapshot_id, collection, path, ordinal)
                 );
                 CREATE INDEX idx_{schema}_snapshot_files_snapshot_path
                     ON {schema}.snapshot_files (snapshot_id, collection, path);
                 CREATE INDEX idx_{schema}_snapshot_deletions_snapshot_path
                     ON {schema}.snapshot_deletions (snapshot_id, collection, path);
                 CREATE TABLE {schema}.content_objects (
                     content_hash TEXT PRIMARY KEY,
                     text TEXT NOT NULL
                 );
                 CREATE TABLE {schema}.semantic_embeddings (
                     embedding_key TEXT NOT NULL,
                     model_id TEXT NOT NULL,
                     dimension INTEGER NOT NULL,
                     embedding BYTEA NOT NULL,
                     PRIMARY KEY (embedding_key, model_id, dimension)
                 );
                 CREATE TABLE {schema}.file_object_items (
                     file_object_id TEXT NOT NULL
                         REFERENCES {schema}.file_objects(id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL,
                     symbol_name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     line_start INTEGER NOT NULL,
                     line_end INTEGER NOT NULL,
                     content_hash TEXT NOT NULL
                         REFERENCES {schema}.content_objects(content_hash) ON DELETE RESTRICT,
                     PRIMARY KEY (file_object_id, ordinal)
                 );
                 CREATE TABLE {schema}.snapshot_heads (
                     corpus TEXT NOT NULL,
                     branch TEXT NOT NULL,
                     snapshot_id TEXT NOT NULL
                         REFERENCES {schema}.snapshots(id) ON DELETE CASCADE,
                     updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                     PRIMARY KEY (corpus, branch)
                 );"
            ))
            .unwrap();
    }

    fn primary_key_columns(adapter: &PostgresBaselineAdapter, table: &str) -> Vec<String> {
        let mut client = adapter.connect().unwrap();
        client
            .query_one(
                "SELECT array_agg(a.attname::text ORDER BY k.ord)
                   FROM pg_constraint c
                   CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
                   JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
                  WHERE c.conrelid = to_regclass($1) AND c.contype = 'p'",
                &[&table],
            )
            .unwrap()
            .get::<_, Option<Vec<String>>>(0)
            .unwrap_or_default()
    }

    fn index_columns(adapter: &PostgresBaselineAdapter, index: &str) -> Vec<String> {
        let mut client = adapter.connect().unwrap();
        client
            .query_one(
                "SELECT array_agg(a.attname::text ORDER BY k.ord)
                   FROM pg_index i
                   CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord)
                   JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                  WHERE i.indexrelid = to_regclass($1)",
                &[&index],
            )
            .unwrap()
            .get::<_, Option<Vec<String>>>(0)
            .unwrap_or_default()
    }

    fn relation_exists(adapter: &PostgresBaselineAdapter, relation: &str) -> bool {
        let mut client = adapter.connect().unwrap();
        client.query_one("SELECT to_regclass($1) IS NOT NULL", &[&relation]).unwrap().get(0)
    }

    fn relation_oid(adapter: &PostgresBaselineAdapter, relation: &str) -> u32 {
        let mut client = adapter.connect().unwrap();
        client
            .query_one("SELECT to_regclass($1)::oid::int8", &[&relation])
            .unwrap()
            .get::<_, i64>(0) as u32
    }

    fn constraint_oid(adapter: &PostgresBaselineAdapter, table: &str) -> u32 {
        let mut client = adapter.connect().unwrap();
        client
            .query_one(
                "SELECT c.oid::int8 FROM pg_constraint c
                  WHERE c.conrelid = to_regclass($1) AND c.contype = 'p'",
                &[&table],
            )
            .unwrap()
            .get::<_, i64>(0) as u32
    }

    /// A throwaway schema name short enough that the generated index names still fit an
    /// identifier: PostgreSQL truncates at 63 bytes, and the longest generated name adds 37 to
    /// whatever is written here.
    fn unique_schema(prefix: &str) -> String {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
                as u64;
        format!("bsl_{prefix}_{:02x}{:08x}", std::process::id() % 256, nanos as u32)
    }

    #[test]
    #[ignore = "requires an isolated live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn token_layout_claim_provenance_is_atomic_and_fails_closed() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to an isolated live Postgres");
        let layout = crate::token_policy::SEGMENTATION_VERSION;
        let claim = "profile-v1:runtime-fixture";

        let schema = unique_schema("tokenlayout");
        let legacy_schema = unique_schema("tokenlegacy");
        let legacy = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url.clone()).with_schema(&legacy_schema),
        )
        .unwrap();
        let _legacy_guard =
            TestSchemaGuard { adapter: legacy.clone(), schema: legacy_schema.clone() };
        legacy.migrate_storage().unwrap();
        legacy
            .publish_snapshot(
                &Snapshot::new("legacy", CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &[indexed_document("code", "src/Legacy.bsl", "Legacy", 1, "legacy-hash", "body")],
            )
            .unwrap();
        let legacy_claimed = legacy.clone().with_token_layout_claim(layout, claim).unwrap();
        let legacy_error = legacy_claimed.ensure_token_layout_claim(layout, claim).unwrap_err();
        assert!(legacy_error.to_string().contains("token_layout_claim_required"));
        assert_eq!(legacy.list_snapshots(None, None, None, 10).unwrap().len(), 1);
        let mut legacy_client = legacy.connect().unwrap();
        let legacy_row = legacy_client
            .query_one(
                &format!(
                    "SELECT (SELECT COUNT(*) FROM {legacy_schema}.snapshots),
                            (SELECT COUNT(*) FROM {legacy_schema}.file_objects),
                            (SELECT COUNT(*) FROM {legacy_schema}.file_object_items)"
                ),
                &[],
            )
            .unwrap();
        let legacy_rows: (i64, i64, i64) =
            (legacy_row.get(0), legacy_row.get(1), legacy_row.get(2));
        assert_eq!(legacy_rows, (1, 1, 1), "legacy rows survive claim refusal");
        let claim_rows: i64 = legacy_client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {legacy_schema}._schema_metadata_
                     WHERE setting IN ('token_layout_version', 'token_layout_claim')"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(claim_rows, 0, "legacy refusal must not write a partial claim");
        assert!(!legacy
            .storage_table_exists(&mut *legacy_client, TOKEN_LAYOUT_PARTS_TABLE)
            .unwrap());

        let base = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _guard = TestSchemaGuard { adapter: base.clone(), schema: schema.clone() };
        base.migrate_storage().unwrap();
        let adapter = base.clone().with_token_layout_claim(layout, claim).unwrap();
        adapter.ensure_token_layout_claim(layout, claim).unwrap();

        let parts = || {
            let mut first =
                indexed_document("code", "src/A.bsl", "A (часть 1)", 1, "part-1", "abc");
            first.source_span = Some(SourceSpan {
                parent_symbol: "A".to_owned(),
                byte_start: 0,
                byte_end: 3,
                parent_byte_start: 0,
                parent_byte_end: 6,
                part_index: 1,
                part_count: 2,
            });
            let mut second =
                indexed_document("code", "src/A.bsl", "A (часть 2)", 2, "part-2", "def");
            second.source_span = Some(SourceSpan {
                parent_symbol: "A".to_owned(),
                byte_start: 3,
                byte_end: 6,
                parent_byte_start: 0,
                parent_byte_end: 6,
                part_index: 2,
                part_count: 2,
            });
            [first, second]
        };

        let mut client = adapter.connect().unwrap();
        client
            .batch_execute(&format!(
                "CREATE FUNCTION {schema}.reject_lexical_insert() RETURNS trigger
                   LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected lexical failure'; END $$;
                 CREATE TRIGGER reject_lexical_insert BEFORE INSERT ON {schema}.serving_lexical
                   FOR EACH ROW EXECUTE FUNCTION {schema}.reject_lexical_insert();"
            ))
            .unwrap();
        assert!(adapter
            .publish_snapshot(
                &Snapshot::new("partial", CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &parts(),
            )
            .is_err());
        client
            .batch_execute(&format!(
                "DROP TRIGGER reject_lexical_insert ON {schema}.serving_lexical;
                 DROP FUNCTION {schema}.reject_lexical_insert();"
            ))
            .unwrap();
        for table in ["snapshots", "file_objects", "file_object_items", TOKEN_LAYOUT_PARTS_TABLE] {
            let count: i64 = client
                .query_one(&format!("SELECT COUNT(*) FROM {schema}.{table}"), &[])
                .unwrap()
                .get(0);
            assert_eq!(count, 0, "failed publish must roll back {table}");
        }
        assert!(adapter.snapshot_details("partial").unwrap().is_none());
        let marker_count: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {schema}._schema_metadata_
                     WHERE setting LIKE 'semantic_publication_complete:%'"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(marker_count, 0, "a partial snapshot cannot become semantically complete");

        adapter
            .publish_snapshot(
                &Snapshot::new("complete", CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &parts(),
            )
            .unwrap();
        let loaded = adapter
            .load_snapshot_documents(&Snapshot::new("complete", CorpusId::WorkspaceCode))
            .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].source_span.as_ref().unwrap().parent_symbol, "A");
        assert_eq!(loaded[0].source_span.as_ref().unwrap().part_index, 1);
        assert_eq!(loaded[1].source_span.as_ref().unwrap().part_index, 2);
        assert!(adapter
            .snapshot_details("complete")
            .unwrap()
            .unwrap()
            .semantic_publication
            .is_none());

        let mut counts = || -> (i64, i64, i64, i64) {
            let row = client
                .query_one(
                    &format!(
                        "SELECT (SELECT COUNT(*) FROM {schema}.snapshots),
                                (SELECT COUNT(*) FROM {schema}.file_objects),
                                (SELECT COUNT(*) FROM {schema}.file_object_items),
                                (SELECT COUNT(*) FROM {schema}.file_object_parts)"
                    ),
                    &[],
                )
                .unwrap();
            (row.get(0), row.get(1), row.get(2), row.get(3))
        };
        let before = counts();
        let unconfigured =
            base.load_snapshot_documents(&Snapshot::new("complete", CorpusId::WorkspaceCode));
        assert!(unconfigured.is_err());
        let mismatched = base
            .with_token_layout_claim(layout, "profile-v1:other")
            .unwrap()
            .load_snapshot_documents(&Snapshot::new("complete", CorpusId::WorkspaceCode));
        assert!(mismatched.is_err());
        let after = counts();
        assert_eq!(before, after, "foreign and mismatched claims must preserve published rows");
    }

    /// Only code documents carry a source span. A claimed schema also stores documents of
    /// other collections (the reference corpus), and reading them back must not demand
    /// provenance rows the publication never had to write.
    #[test]
    #[ignore = "requires an isolated live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn token_layout_claim_reads_back_documents_without_source_spans() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to an isolated live Postgres");
        let layout = crate::token_policy::SEGMENTATION_VERSION;
        let claim = "profile-v1:reference-fixture";
        let schema = unique_schema("tokenref");
        let base = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _guard = TestSchemaGuard { adapter: base.clone(), schema: schema.clone() };
        base.migrate_storage().unwrap();
        let adapter = base.with_token_layout_claim(layout, claim).unwrap();
        adapter.ensure_token_layout_claim(layout, claim).unwrap();

        let platform = indexed_document("platform", "docs/A.md", "A", 1, "platform-a", "body");
        let mut code = indexed_document("code", "src/A.bsl", "A", 1, "code-a", "abc");
        code.source_span = Some(SourceSpan {
            parent_symbol: "A".to_owned(),
            byte_start: 0,
            byte_end: 3,
            parent_byte_start: 0,
            parent_byte_end: 3,
            part_index: 1,
            part_count: 1,
        });
        let snapshot = Snapshot::new("mixed", CorpusId::Reference);
        adapter
            .publish_snapshot(
                &snapshot,
                &SnapshotPublishMetadata::default(),
                &[platform.clone(), code.clone()],
            )
            .unwrap();

        let mut documents = adapter.load_snapshot_documents(&snapshot).unwrap();
        documents.sort_by(|left, right| left.collection.cmp(&right.collection));
        assert_eq!(documents, vec![code, platform]);
    }

    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn indexing_remote_publication_reader() {
        let url = std::env::var("BSL_TEST_PG_URL").expect("isolated Postgres URL required");
        let schema = unique_schema("indexing");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        let mut client = adapter.connect().unwrap();
        client
            .batch_execute(&format!(
                "INSERT INTO {schema}.snapshots (id, corpus, fingerprint)
             VALUES ('empty', 'code', 'fingerprint');"
            ))
            .unwrap();
        assert!(adapter.snapshot_details("absent").unwrap().is_none());
        let read = || adapter.snapshot_details("empty").unwrap().unwrap();
        assert!(read().semantic_publication.is_none());
        let set = |client: &mut postgres::Client, key: &str, value: &str| {
            client
                .execute(
                    &format!(
                        "INSERT INTO {schema}._schema_metadata_ (setting, value) VALUES ($1, $2)
                 ON CONFLICT (setting) DO UPDATE SET value = excluded.value"
                    ),
                    &[&key, &value],
                )
                .unwrap();
        };
        set(&mut client, "embedding_model", "model");
        set(&mut client, "embedding_dimension", "4");
        assert!(!read().semantic_publication.unwrap().complete);
        // Other identities/snapshots must not qualify this details row.
        for key in [
            "semantic_publication_complete:other:model:4",
            "semantic_publication_complete:empty:other:4",
            "semantic_publication_complete:empty:model:8",
        ] {
            set(&mut client, key, "complete");
            assert!(!read().semantic_publication.unwrap().complete);
        }
        let marker = "semantic_publication_complete:empty:model:4";
        set(&mut client, marker, "complete");
        let details = read();
        assert_eq!(details.snapshot.snapshot_id, "empty");
        assert_eq!(details.snapshot.fingerprint.as_deref(), Some("fingerprint"));
        assert_eq!((details.snapshot.files, details.snapshot.documents), (0, 0));
        let evidence = details.semantic_publication.unwrap();
        assert_eq!(
            (evidence.model_id.as_str(), evidence.dimension, evidence.complete),
            ("model", 4, true)
        );
        set(&mut client, marker, "invalidated");
        assert!(!read().semantic_publication.unwrap().complete);
        for dimension in ["bad", "0", "-1", "04", "184467440737095516160"] {
            set(&mut client, "embedding_dimension", dimension);
            assert!(read().semantic_publication.is_none(), "{dimension}");
        }
        set(&mut client, "embedding_dimension", "4");
        set(&mut client, "embedding_model", " ");
        assert!(read().semantic_publication.is_none());
    }

    /// The catalog after migration agrees with the generated SQL — for the keys AND for the two
    /// secondary indexes.
    ///
    /// The indexes are not padding. They are declared `CREATE INDEX IF NOT EXISTS` under a fixed
    /// name, so on a live database a re-declaration is skipped by name and the index keeps its
    /// old composition; nothing about the answers changes, only their cost, so that failure is
    /// invisible forever unless it is caught here.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migrating_a_pre_root_schema_keys_every_carrier_and_index_by_root() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("preroot");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);

        assert_eq!(
            primary_key_columns(&adapter, &format!("{schema}.snapshot_files")),
            vec!["snapshot_id", "collection", "path"],
            "the fixture must start from the pre-root shape, or the migration is never observed"
        );

        adapter.migrate_storage().unwrap();

        // Из константы, и только обязательные: фикстура поднимает схему БЕЗ pgvector, поэтому
        // необязательного носителя в ней нет вовсе, а его миграция закрыта своим тестом.
        for (table, expected, _) in ROOTED_CARRIER_KEYS
            .iter()
            .filter(|(_, _, obligation)| *obligation == CarrierObligation::Required)
        {
            assert_eq!(
                primary_key_columns(&adapter, &format!("{schema}.{table}")),
                *expected,
                "{table} did not reach the rooted key"
            );
        }
        for table in ["snapshot_files", "snapshot_deletions"] {
            assert_eq!(
                index_columns(&adapter, &format!("{schema}.idx_{schema}_{table}_snapshot_path")),
                vec!["snapshot_id", "collection", "root_id", "path"],
                "the secondary index of {table} kept its pre-root composition"
            );
        }
    }

    /// An interrupted migration leaves the original key, not a half-migrated schema.
    ///
    /// What this actually gates is atomicity: applying the statements outside the transaction
    /// turns it red, because the earlier ones have already keyed the table by the time the last
    /// one fails. The version assertion below is a consistency check and NOT a second gate — an
    /// abort inside the statement loop never reaches the version write in any arrangement, so it
    /// would hold even if the stamp were moved back outside the transaction.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn an_interrupted_migration_leaves_the_original_key() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("interrupted");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);

        let mut statements = adapter.ensure_schema_statements();
        statements.push("SELECT 1 / 0".to_owned());
        let mut client = adapter.connect().unwrap();
        let error = adapter.migrate_structure(&mut client, &statements).unwrap_err();

        // Identified by SQLSTATE, not by message text: `SearchError::Postgres` renders as a bare
        // "db error" and keeps the server's own message in its source — where it arrives in the
        // server's language, so matching on English would pass or fail by locale.
        let code = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<postgres::Error>())
            .and_then(|pg| pg.code())
            .cloned();
        assert_eq!(
            code,
            Some(postgres::error::SqlState::DIVISION_BY_ZERO),
            "the interruption must surface, not be swallowed: {error}"
        );
        assert_eq!(
            primary_key_columns(&adapter, &format!("{schema}.snapshot_files")),
            vec!["snapshot_id", "collection", "path"],
            "a rolled-back migration must leave the original key"
        );
        assert_eq!(
            adapter.get_schema_version().unwrap(),
            Some(1),
            "a rolled-back migration must not claim the new version"
        );
    }

    /// A parent carrying one relative path under two roots, and a child deleting only the
    /// extension's copy. Written with raw SQL because the publisher does not carry roots yet.
    fn seed_two_rooted_files(adapter: &PostgresBaselineAdapter, schema: &str) {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let mut client = adapter.connect().unwrap();
        client
            .batch_execute(&format!(
                "INSERT INTO {schema}.snapshots (id, corpus) VALUES ('parent', 'code');
                 INSERT INTO {schema}.snapshots (id, corpus, parent_snapshot_id)
                     VALUES ('child', 'code', 'parent');
                 INSERT INTO {schema}.file_objects VALUES
                     ('obj-cfg', 'code', 'fp-cfg', 2),
                     ('obj-ext', 'code', 'fp-ext', 1);
                 INSERT INTO {schema}.snapshot_files
                     (snapshot_id, collection, root_id, path,
                      file_fingerprint, document_count, file_object_id)
                     VALUES
                     ('parent', 'code', '', '{PATH}', 'fp-cfg', 2, 'obj-cfg'),
                     ('parent', 'code', 'Расш', '{PATH}', 'fp-ext', 1, 'obj-ext');
                 INSERT INTO {schema}.snapshot_deletions (snapshot_id, collection, root_id, path)
                     VALUES ('child', 'code', 'Расш', '{PATH}');"
            ))
            .unwrap();
    }

    /// Visibility distinguishes the roots THROUGH the adapter, not only in the pure fold.
    ///
    /// The fold is already gated offline, and it would stay green while the SQL projection fails
    /// to select the column or fills it wrongly — the central rule of this node broken in the
    /// plumbing, with every offline gate passing.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_deletion_shadows_only_its_own_root_through_the_adapter() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("visible");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        seed_two_rooted_files(&adapter, &schema);

        let mut client = adapter.connect().unwrap();
        let mut parent_roots: Vec<String> =
            materialize_visible_snapshot_files(&mut *client, &adapter, "parent")
                .unwrap()
                .into_iter()
                .map(|file| file.root_id)
                .collect();
        parent_roots.sort();
        let child_roots: Vec<String> =
            materialize_visible_snapshot_files(&mut *client, &adapter, "child")
                .unwrap()
                .into_iter()
                .map(|file| file.root_id)
                .collect();

        assert_eq!(
            parent_roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "one relative path under two roots is two files"
        );
        assert_eq!(
            child_roots,
            vec![CONFIGURATION_ROOT_ID.to_owned()],
            "deleting the extension's file must leave the configuration's alone"
        );
    }

    /// The server-side summaries count two roots as two files AND shadow a deletion by root.
    ///
    /// Two inputs, because each CTE has two independent branches. An implementation that added
    /// the root to the file branch and left the deletion branch rootless passes the first input
    /// — two live roots, two counted — and then shadows the wrong root on the second, which only
    /// an operator reading the summary would ever see.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn summaries_count_two_roots_and_shadow_the_deleted_one() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("summary");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        seed_two_rooted_files(&adapter, &schema);

        let parent = adapter.snapshot_details("parent").unwrap().unwrap();
        assert_eq!(parent.snapshot.files, 2, "two roots are two files");
        assert_eq!(parent.snapshot.documents, 3, "and their documents are counted apart");

        let child = adapter.snapshot_details("child").unwrap().unwrap();
        assert_eq!(child.snapshot.files, 1, "the deletion removes exactly one of the two");
        assert_eq!(
            child.snapshot.documents, 2,
            "the surviving file is the configuration's, so its two documents remain"
        );

        let listed = adapter.list_snapshots(None, None, None, 10).unwrap();
        let child_row = listed.iter().find(|record| record.snapshot_id == "child").unwrap();
        assert_eq!(
            (child_row.files, child_row.documents),
            (1, 2),
            "the batch totals must agree with the single-snapshot summary"
        );
    }

    /// The publisher writes two files where one relative path lives under two roots, and the
    /// content of the two is IDENTICAL.
    ///
    /// Identical content is not a detail of the stand. With different content the two files get
    /// different `file_object_id`s and the input stops being the one where a shared file object
    /// arises at all — and a shared file object is exactly what has to be shown not to fold two
    /// files into one row of `snapshot_files`.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn publishing_one_path_under_two_roots_writes_two_files_sharing_one_object() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("publish");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-один", "текст");
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        let snapshot = Snapshot::new("snap-roots".to_owned(), CorpusId::WorkspaceCode);

        let stats = adapter
            .publish_snapshot(
                &snapshot,
                &SnapshotPublishMetadata::default(),
                &[configuration, extension],
            )
            .expect("a rooted corpus is publishable now that the schema keys files by root");

        assert_eq!(stats.written_files, 2, "two roots are two files, not one merged row");

        let mut client = adapter.connect().unwrap();
        let rows = client
            .query(
                &format!(
                    "SELECT root_id, file_object_id FROM {schema}.snapshot_files
                      WHERE snapshot_id = 'snap-roots' ORDER BY root_id"
                ),
                &[],
            )
            .unwrap();
        let roots: Vec<String> = rows.iter().map(|row| row.get::<_, String>(0)).collect();
        let objects: Vec<String> = rows.iter().map(|row| row.get::<_, String>(1)).collect();
        assert_eq!(roots, vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()]);
        assert_eq!(
            objects[0], objects[1],
            "identical content legitimately shares one file object; identity lives in \
             snapshot_files, and it is the ROW that must be two"
        );

        let lexical_roots: Vec<String> = client
            .query(
                &format!(
                    "SELECT DISTINCT root_id FROM {schema}.serving_lexical
                      WHERE snapshot_id = 'snap-roots' ORDER BY root_id"
                ),
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();
        assert_eq!(
            lexical_roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "the lexical serving rows carry the root too, or search answers for the wrong file"
        );
    }

    /// What was published under a root comes back under that root, by both lexical routes.
    ///
    /// Reading is where the root stops being bookkeeping: a hit that names the configuration's
    /// file when the match is an extension's sends the caller to the wrong file on disk.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn the_published_root_comes_back_from_both_lexical_routes() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("read");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let configuration = indexed_document(
            "code",
            PATH,
            "Общий",
            1,
            "hash-один",
            "Процедура Общий КонецПроцедуры",
        );
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        let snapshot = Snapshot::new("snap-read".to_owned(), CorpusId::WorkspaceCode);
        adapter
            .publish_snapshot(
                &snapshot,
                &SnapshotPublishMetadata::default(),
                &[configuration, extension],
            )
            .unwrap();

        let mut documents_roots: Vec<String> = adapter
            .load_snapshot_documents(&snapshot)
            .unwrap()
            .into_iter()
            .map(|document| document.root_id)
            .collect();
        documents_roots.sort();
        documents_roots.dedup();
        assert_eq!(
            documents_roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "the corpus reader must return the identity the corpus was published under"
        );

        let mut hit_roots: Vec<String> = adapter
            .lexical_search_baseline("snap-read", "Общий", Some("code"), 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.root_id)
            .collect();
        hit_roots.sort();
        hit_roots.dedup();
        assert_eq!(
            hit_roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "a hit naming the wrong root sends the caller to the wrong file on disk"
        );
    }

    /// Inspection tells apart two files that share one file object.
    ///
    /// Sharing is legitimate — identical content under two roots — so the reference list is the
    /// only place an operator can see which file a row came from, and without the root the two
    /// rows are indistinguishable from a duplicate.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn inspection_tells_apart_two_files_sharing_one_object() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("inspect");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        seed_two_rooted_files(&adapter, &schema);
        {
            let mut client = adapter.connect().unwrap();
            client
                .batch_execute(&format!(
                    "UPDATE {schema}.snapshot_files SET file_object_id = 'obj-cfg'
                      WHERE snapshot_id = 'parent';"
                ))
                .unwrap();
        }

        let details = adapter.file_object_details("obj-cfg").unwrap().unwrap();
        let mut roots: Vec<String> =
            details.references.into_iter().map(|reference| reference.root_id).collect();
        roots.sort();

        assert_eq!(
            roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "two references to one object must say which file each of them is"
        );
    }

    /// The root travels the whole way: PostgreSQL manifest, local store, keys read back.
    ///
    /// Started at the production producer rather than at a manifest assembled in the test,
    /// because a circle that begins with a hand-built manifest cannot tell a correct producer
    /// from one that still stamps the configuration on every file.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn the_manifest_carries_the_root_to_the_local_store() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("manifest");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        seed_two_rooted_files(&adapter, &schema);

        let manifest = adapter.load_baseline_manifest("parent").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("bsl-search.db")).unwrap();
        store.save_baseline_manifest(&manifest).unwrap();

        let keys = store.load_baseline_manifest_fingerprints("code").unwrap().unwrap();
        let mut roots: Vec<String> = keys.keys().map(|key| key.root_id.clone()).collect();
        roots.sort();

        assert_eq!(
            roots,
            vec![CONFIGURATION_ROOT_ID.to_owned(), "Расш".to_owned()],
            "a manifest that flattens both files onto the configuration makes the consumer \
             compare an extension's file against the configuration's fingerprint"
        );
    }

    /// The collector does not mistake a live vector for an orphan on a rooted corpus.
    ///
    /// This is the other half of a connection that is invisible where either half is written:
    /// the collector rebuilds live keys from `sf.path` with NO root, which is correct only
    /// because the embedding key ignores the root. Whoever adds the root to the recipe breaks
    /// the collector in silence, and the price is deleting vectors that are in use — expensive
    /// to recompute and, until recomputed, semantic search answers nothing for those files.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn the_collector_keeps_the_vectors_of_a_rooted_corpus() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("gc");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        const TEXT: &str = "Процедура Общий() КонецПроцедуры";
        const OWN_PATH: &str = "CommonModules/ТолькоРасширение/Ext/Module.bsl";
        const OWN_TEXT: &str = "Процедура ТолькоРасширение() КонецПроцедуры";
        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-один", TEXT);
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        // A file living ONLY under a non-empty root. Without it this test cannot see the root
        // leaking into the key at all: the configuration's root is the empty string, so any
        // recipe that mixes it in leaves that file's key byte-for-byte unchanged, and a
        // fixture of shared files alone stays green for a collector that reads the root.
        let extension_only = IndexedDocument {
            root_id: "Расш".to_owned(),
            path: OWN_PATH.to_owned(),
            symbol_name: "ТолькоРасширение".to_owned(),
            content_hash: "hash-два".to_owned(),
            text: OWN_TEXT.to_owned(),
            ..configuration.clone()
        };
        adapter
            .publish_snapshot(
                &Snapshot::new("snap-gc".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &[configuration.clone(), extension, extension_only.clone()],
            )
            .unwrap();

        let live_key = crate::document::semantic_key_from_parts(
            PATH,
            &configuration.kind,
            &configuration.symbol_name,
            "",
            TEXT,
        );
        let extension_only_key = crate::document::semantic_key_from_parts(
            OWN_PATH,
            &extension_only.kind,
            &extension_only.symbol_name,
            "",
            OWN_TEXT,
        );
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (live_key.clone(), vec![0.1, 0.2, 0.3, 0.4]),
                    (extension_only_key.clone(), vec![0.5, 0.6, 0.7, 0.8]),
                ],
            )
            .unwrap();
        // Positive control: without a key that MUST be collected, this test is green for a
        // collector that deletes nothing at all.
        adapter
            .store_embeddings("model", 4, &[("ключ-сироты".to_owned(), vec![0.4, 0.3, 0.2, 0.1])])
            .unwrap();

        let report = adapter.garbage_collect(true).unwrap();

        assert_eq!(report.deleted_semantic_embeddings, 1, "exactly the orphan must go: {report:?}");
        let survivors: Vec<String> = adapter
            .connect()
            .unwrap()
            .query(
                &format!("SELECT embedding_key FROM {schema}.semantic_embeddings ORDER BY 1"),
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();
        let mut expected = vec![live_key, extension_only_key];
        expected.sort();
        assert_eq!(
            survivors, expected,
            "both vectors are live: one for a file published under two roots, one for a file \
             that exists only under the extension"
        );
    }

    /// A republish attempted DURING the final write is serialized behind it.
    ///
    /// `publish_snapshot` clears the snapshot's semantics as part of its own transaction, so the
    /// two operations must not interleave: rows computed from one version of the corpus, written
    /// after another version replaced it, would sit under a completeness mark that says they
    /// describe it.
    ///
    /// The rendezvous is a flag rather than a sleep: the republish waits until the write window
    /// is actually open, so the input hits the window on every run instead of when the timing
    /// happens to work out. Driven from another thread, because a synchronous republish would
    /// block on the lock our own transaction holds and the test would hang — which is what the
    /// first version of it did.
    ///
    /// The observable is the END STATE. With the lock the republish lands after our commit and
    /// its own invalidation clears the rows; without it, the republish commits BEFORE our insert,
    /// so its invalidation clears nothing and our rows survive the corpus they described.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_republish_during_the_final_write_waits_for_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("race");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-один", "текст");
        adapter
            .publish_snapshot(
                &Snapshot::new("race".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&configuration),
            )
            .unwrap();

        // Without a stored embedding the write phase produces no rows at all, and "no rows at
        // the end" would then hold for any implementation — the assertion below would be green
        // whether or not the republish slipped in.
        adapter
            .store_embeddings(
                "model",
                4,
                &[(
                    crate::document::semantic_key_from_parts(
                        PATH,
                        &configuration.kind,
                        &configuration.symbol_name,
                        "",
                        &configuration.text,
                    ),
                    vec![0.1, 0.2, 0.3, 0.4],
                )],
            )
            .unwrap();

        let window_open = Arc::new(AtomicBool::new(false));
        let republisher = PostgresBaselineAdapter::new(config).unwrap();
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        let handle = {
            let window_open = Arc::clone(&window_open);
            let configuration = configuration.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if window_open.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                republisher
                    .publish_snapshot(
                        &Snapshot::new("race".to_owned(), CorpusId::WorkspaceCode),
                        &SnapshotPublishMetadata::default(),
                        &[configuration, extension],
                    )
                    .unwrap();
            })
        };

        let open_the_window = |event: crate::external_baseline::SemanticPublishProgress| {
            if matches!(
                event,
                crate::external_baseline::SemanticPublishProgress::PhaseStarted {
                    phase: crate::external_baseline::SemanticPublishPhase::WriteServingRows,
                    ..
                }
            ) {
                window_open.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(3000));
            }
        };
        adapter
            .populate_serving_semantic_with_progress("race", "model", 4, Some(&open_the_window))
            .expect("the semantic write commits before the republish is allowed through");
        handle.join().unwrap();

        let rows: i64 = adapter
            .connect()
            .unwrap()
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {schema}.serving_semantic WHERE snapshot_id = 'race'"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(
            rows, 0,
            "the republish must be ordered after the write, so its invalidation clears what the \
             write produced; rows surviving mean it slipped in between the check and the insert"
        );
    }

    /// A republish that COMMITS between planning and the final write is refused, not served.
    ///
    /// The lock taken before the write orders republishes that have not committed yet; one that
    /// already committed is simply invisible to it, and everything computed since — the file set,
    /// the strategy, the prepared rows — belongs to a corpus that no longer exists. Writing it
    /// under a completeness mark would claim the current snapshot is served by semantics of the
    /// previous one.
    ///
    /// The corpus here gains a ROOT rather than changing bytes, which is the case a byte-level
    /// notion of staleness misses entirely: the file's content is untouched and its identity is
    /// new. The republish runs synchronously from the `Plan` callback, which fires after the
    /// planning transaction has committed, so it cannot deadlock against a lock we still hold.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_snapshot_republished_between_planning_and_the_write_is_refused() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("restale");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-cfg", "текст");
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        adapter
            .store_embeddings(
                "model",
                4,
                &[(
                    crate::document::semantic_key_from_parts(
                        PATH,
                        &configuration.kind,
                        &configuration.symbol_name,
                        "",
                        &configuration.text,
                    ),
                    vec![0.1, 0.2, 0.3, 0.4],
                )],
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("mid".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&configuration),
            )
            .unwrap();

        let republisher = PostgresBaselineAdapter::new(config).unwrap();
        let republish_once = std::cell::Cell::new(true);
        let republish = |event: crate::external_baseline::SemanticPublishProgress| {
            if matches!(event, crate::external_baseline::SemanticPublishProgress::Plan { .. })
                && republish_once.replace(false)
            {
                republisher
                    .publish_snapshot(
                        &Snapshot::new("mid".to_owned(), CorpusId::WorkspaceCode),
                        &SnapshotPublishMetadata::default(),
                        &[configuration.clone(), extension.clone()],
                    )
                    .unwrap();
            }
        };

        let error = adapter
            .populate_serving_semantic_with_progress("mid", "model", 4, Some(&republish))
            .expect_err(
                "a plan built for a corpus that has since been replaced must not be served",
            );
        assert_eq!(
            error.reason_code(),
            Some("snapshot_republished_while_publishing"),
            "the refusal must be the named one: {error}"
        );

        let mut client = adapter.connect().unwrap();
        let rows: i64 = client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM {schema}.serving_semantic WHERE snapshot_id = 'mid'"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(rows, 0, "the refusal must roll back with its transaction");
        assert!(
            !semantic_publication_complete(&mut *client, &adapter, "mid", "model", 4).unwrap(),
            "and it must leave no completeness mark behind"
        );
    }

    /// A plan read from a snapshot that is already GONE is refused, not silently served.
    ///
    /// The parent's completeness mark lives in the metadata table, which no foreign key ties to
    /// `snapshots`, so it outlives the parent row. Planning then picks the incremental strategy
    /// on the strength of a mark whose snapshot is not there, the copy brings nothing, and the
    /// child would be marked complete over a corpus it never read. Two identical absences compare
    /// equal, which is why absence on either side counts as movement rather than as a match.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_plan_read_from_a_vanished_parent_is_refused() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("gone");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let parent_file = indexed_document("code", "src/A.bsl", "А", 1, "hash-а", "текст-а");
        let child_file = indexed_document("code", "src/B.bsl", "Б", 1, "hash-б", "текст-б");
        let key_of = |document: &IndexedDocument| {
            crate::document::semantic_key_from_parts(
                &document.path,
                &document.kind,
                &document.symbol_name,
                "",
                &document.text,
            )
        };
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (key_of(&parent_file), vec![1.0, 0.0, 0.0, 0.0]),
                    (key_of(&child_file), vec![0.0, 1.0, 0.0, 0.0]),
                ],
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&parent_file),
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("child".to_owned(), CorpusId::WorkspaceCode).with_parent("parent"),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&child_file),
            )
            .unwrap();
        adapter.populate_serving_semantic("parent", "model", 4).unwrap();
        // Raw SQL because nothing in the crate deletes a snapshot: the state is reachable only
        // from outside, which is why this is a guard rather than a scenario.
        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!("DELETE FROM {schema}.snapshots WHERE id = 'parent'"))
            .unwrap();

        let error = adapter
            .populate_serving_semantic("child", "model", 4)
            .expect_err("a plan whose parent is gone must not be served");
        assert_eq!(
            error.reason_code(),
            Some("snapshot_republished_while_publishing"),
            "the refusal must be the named one: {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("was already gone"),
            "a snapshot that is not there must not be reported as republished, or the operator \
             hunts the catalogue for a publication that never happened: {message}"
        );
        // The refusal is the one text this node shows an operator, and a line continuation that
        // loses its backslash pulls the indentation into the sentence. Neither rustfmt nor clippy
        // reads string contents, so the only thing that can notice is this.
        assert!(
            !message.contains("  "),
            "the message carries the source's indentation into the text: {message}"
        );

        let mut client = adapter.connect().unwrap();
        assert!(
            !semantic_publication_complete(&mut *client, &adapter, "child", "model", 4).unwrap(),
            "and it must leave no completeness mark behind"
        );
    }

    /// A PARENT republished between planning and the write is refused just as the snapshot is.
    ///
    /// The worse half of the same class, and the one a check on our own row alone misses: the
    /// parent's republish clears ITS rows and its completeness mark and never touches ours, so
    /// the copy-forward silently brings nothing, our own rows go in, and the gap is sealed under
    /// our completeness mark with nothing left to correct it later. Our own row's version does
    /// not move at all, which is exactly why the check answers for every snapshot the plan was
    /// read from rather than for the one being published.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_parent_republished_between_planning_and_the_write_is_refused() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("parstale");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-cfg", "текст");
        let child_file = indexed_document("code", "src/B.bsl", "Б", 1, "hash-б", "текст-б");
        let key_of = |document: &IndexedDocument| {
            crate::document::semantic_key_from_parts(
                &document.path,
                &document.kind,
                &document.symbol_name,
                "",
                &document.text,
            )
        };
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (key_of(&configuration), vec![1.0, 0.0, 0.0, 0.0]),
                    (key_of(&child_file), vec![0.0, 1.0, 0.0, 0.0]),
                ],
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&configuration),
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("child".to_owned(), CorpusId::WorkspaceCode).with_parent("parent"),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&child_file),
            )
            .unwrap();
        // Only a parent with a completeness mark makes the plan incremental, and only an
        // incremental plan reads the parent at all.
        adapter.populate_serving_semantic("parent", "model", 4).unwrap();

        let republisher = PostgresBaselineAdapter::new(config).unwrap();
        let republish_once = std::cell::Cell::new(true);
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        let republish_parent = |event: crate::external_baseline::SemanticPublishProgress| {
            if matches!(event, crate::external_baseline::SemanticPublishProgress::Plan { .. })
                && republish_once.replace(false)
            {
                republisher
                    .publish_snapshot(
                        &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                        &SnapshotPublishMetadata::default(),
                        &[configuration.clone(), extension.clone()],
                    )
                    .unwrap();
            }
        };

        let error = adapter
            .populate_serving_semantic_with_progress("child", "model", 4, Some(&republish_parent))
            .expect_err("a plan that copies from a parent replaced since must not be served");
        assert_eq!(
            error.reason_code(),
            Some("snapshot_republished_while_publishing"),
            "the refusal must be the named one: {error}"
        );
        assert!(
            error.to_string().contains("'parent'"),
            "and it must name the snapshot that moved, not the one being published: {error}"
        );

        let mut client = adapter.connect().unwrap();
        assert!(
            !semantic_publication_complete(&mut *client, &adapter, "child", "model", 4).unwrap(),
            "an incomplete corpus must not be left wearing a completeness mark"
        );
    }

    /// The PARENT the plan copies from is serialized behind the final write too.
    ///
    /// An incremental publish READS the parent's serving rows while writing its own, so a lock on
    /// the snapshot's own row is narrower than what the transaction touches: republishing the
    /// parent clears those rows without ever touching the child's row, and the copy takes a
    /// mixture of before and after.
    ///
    /// The parent's semantics are published FIRST on purpose. Without a completeness mark on the
    /// parent the strategy is `FullRebuild`, which reads nothing from the parent — the copy never
    /// happens, there is nothing to order, and the assertion below would fail against a correct
    /// implementation rather than a defective one.
    ///
    /// Observed by ORDER here, unlike its sibling. A republish of the snapshot ITSELF clears the
    /// snapshot's semantic rows, so there the end state tells the two designs apart. A republish
    /// of a PARENT clears only the parent's own rows — nothing invalidates a descendant — so the
    /// end state is identical either way and only the ordering differs.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_parent_republished_during_the_final_write_waits_for_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("ancest");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let configuration = indexed_document("code", PATH, "Общий", 1, "hash-один", "текст");
        let child_file = indexed_document("code", "src/B.bsl", "Б", 1, "hash-б", "текст-б");
        adapter
            .publish_snapshot(
                &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&configuration),
            )
            .unwrap();
        adapter
            .publish_snapshot(
                &Snapshot::new("child".to_owned(), CorpusId::WorkspaceCode).with_parent("parent"),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&child_file),
            )
            .unwrap();
        // Without a stored embedding the write phase produces no rows, and the assertion below
        // would hold for any implementation.
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (
                        crate::document::semantic_key_from_parts(
                            "src/B.bsl",
                            &child_file.kind,
                            &child_file.symbol_name,
                            "",
                            &child_file.text,
                        ),
                        vec![0.1, 0.2, 0.3, 0.4],
                    ),
                    (
                        crate::document::semantic_key_from_parts(
                            PATH,
                            &configuration.kind,
                            &configuration.symbol_name,
                            "",
                            &configuration.text,
                        ),
                        vec![0.5, 0.6, 0.7, 0.8],
                    ),
                ],
            )
            .unwrap();
        // The completeness mark is what makes the child's plan incremental, and only an
        // incremental plan reads the parent at all.
        adapter
            .populate_serving_semantic_with_progress("parent", "model", 4, None)
            .expect("the parent's own semantics publish without a race");

        let window_open = Arc::new(AtomicBool::new(false));
        let write_finished = Arc::new(AtomicBool::new(false));
        let republish_waited = Arc::new(AtomicBool::new(false));
        let republisher = PostgresBaselineAdapter::new(config).unwrap();
        let extension = IndexedDocument { root_id: "Расш".to_owned(), ..configuration.clone() };
        let handle = {
            let window_open = Arc::clone(&window_open);
            let write_finished = Arc::clone(&write_finished);
            let waited = Arc::clone(&republish_waited);
            let configuration = configuration.clone();
            std::thread::spawn(move || {
                for _ in 0..500 {
                    if window_open.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                republisher
                    .publish_snapshot(
                        &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                        &SnapshotPublishMetadata::default(),
                        &[configuration, extension],
                    )
                    .unwrap();
                waited.store(write_finished.load(Ordering::SeqCst), Ordering::SeqCst);
            })
        };

        let open_the_window = |event: crate::external_baseline::SemanticPublishProgress| {
            if matches!(
                event,
                crate::external_baseline::SemanticPublishProgress::PhaseStarted {
                    phase: crate::external_baseline::SemanticPublishPhase::WriteServingRows,
                    ..
                }
            ) {
                window_open.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(3000));
            }
        };
        adapter
            .populate_serving_semantic_with_progress("child", "model", 4, Some(&open_the_window))
            .expect("the child's incremental publish copies the parent's rows and commits");
        write_finished.store(true, Ordering::SeqCst);
        handle.join().unwrap();

        let roots: Vec<String> = adapter
            .connect()
            .unwrap()
            .query(
                &format!(
                    "SELECT DISTINCT root_id FROM {schema}.snapshot_files
                      WHERE snapshot_id = 'parent' ORDER BY 1"
                ),
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();
        assert!(
            roots.iter().any(|root| root == "Расш"),
            "the republish must have happened, or this test proves nothing: {roots:?}"
        );
        assert!(
            republish_waited.load(Ordering::SeqCst),
            "the parent's republish committed while the semantic write was still running: the \
             lock is narrower than what the write reads, so the rows copied forward were a \
             mixture of the parent before and after"
        );
    }

    /// A pre-root `serving_semantic`, as a schema published before this node has it.
    fn raise_pre_root_serving_semantic(adapter: &PostgresBaselineAdapter, schema: &str) {
        let mut client = adapter.connect().unwrap();
        client
            .batch_execute(&format!(
                "CREATE EXTENSION IF NOT EXISTS vector;
                 CREATE TABLE {schema}.serving_semantic (
                     snapshot_id TEXT NOT NULL REFERENCES {schema}.snapshots(id) ON DELETE CASCADE,
                     collection TEXT NOT NULL,
                     path TEXT NOT NULL,
                     ordinal INTEGER NOT NULL,
                     symbol_name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     line_start INTEGER NOT NULL,
                     line_end INTEGER NOT NULL,
                     model_id TEXT NOT NULL,
                     dimension INTEGER NOT NULL,
                     embedding vector NOT NULL,
                     PRIMARY KEY (snapshot_id, model_id, collection, path, ordinal)
                 );"
            ))
            .unwrap();
    }

    /// Migrating an EXISTING semantic carrier rebuilds its key and leaves its rows alone.
    ///
    /// The key is asserted here and nowhere else. A fresh database gets the right key from
    /// `CREATE TABLE`, and readiness only proves it can REFUSE a wrong key — neither says the
    /// migration fixed an already published schema. Without this, every gate in this file is
    /// green while an upgraded database carries version 3 over the old key and fails with a
    /// duplicate key on the first pair of files that share a path.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migrating_an_existing_semantic_carrier_keys_it_by_root_and_keeps_its_rows() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("semkey");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);
        raise_pre_root_serving_semantic(&adapter, &schema);
        {
            let mut client = adapter.connect().unwrap();
            client
                .batch_execute(&format!(
                    "INSERT INTO {schema}.snapshots (id, corpus) VALUES ('snap-1', 'code');
                     INSERT INTO {schema}.serving_semantic VALUES
                         ('snap-1', 'code', 'src/A.bsl', 0, 'A', 'procedure', 0, 1,
                          'model', 4, '[1,0,0,0]');"
                ))
                .unwrap();
        }
        let row_version = |adapter: &PostgresBaselineAdapter| -> String {
            let mut client = adapter.connect().unwrap();
            client
                .query_one(&format!("SELECT xmin::text FROM {schema}.serving_semantic"), &[])
                .unwrap()
                .get(0)
        };
        let before = row_version(&adapter);

        adapter.migrate_storage().unwrap();

        assert_eq!(
            primary_key_columns(&adapter, &format!("{schema}.serving_semantic")),
            vec!["snapshot_id", "model_id", "collection", "root_id", "path", "ordinal"],
            "the migration must rebuild the key of a carrier that already existed"
        );
        let mut client = adapter.connect().unwrap();
        let (path, root_id): (String, String) = {
            let row = client
                .query_one(&format!("SELECT path, root_id FROM {schema}.serving_semantic"), &[])
                .unwrap();
            (row.get(0), row.get(1))
        };
        assert_eq!(path, "src/A.bsl", "the row must survive the migration");
        assert_eq!(root_id, CONFIGURATION_ROOT_ID, "a pre-root row belongs to the configuration");
        assert_eq!(before, row_version(&adapter), "the backfill rewrote the row");
        adapter.check_storage_readiness().expect("the migrated schema must be ready");
    }

    /// A schema WITHOUT the optional carrier migrates like any other.
    ///
    /// The backfill for `serving_semantic` runs inside the mandatory migration's transaction,
    /// where a plain `ALTER TABLE` on a missing table would roll the whole thing back and leave
    /// a database that never had the `vector` extension permanently unready. The table is dropped
    /// here rather than the extension removed, because the state that matters is the same one and
    /// this stand cannot be stripped of pgvector.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migrating_a_schema_without_the_semantic_carrier_is_not_a_failure() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("nosem");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!("DROP TABLE {schema}.serving_semantic"))
            .unwrap();

        adapter
            .migrate_storage()
            .expect("a schema without the optional carrier must migrate, not roll back");

        adapter
            .check_storage_readiness()
            .expect("and it must be ready afterwards, optional carrier or not");
        assert_eq!(
            adapter.get_schema_version().unwrap(),
            Some(crate::error::SCHEMA_VERSION_CURRENT),
            "the version must be stamped, since the carrier that is missing is optional"
        );
    }

    /// Readiness answers about the optional carrier in THREE ways, not two.
    ///
    /// The third input is the one a two-way rule gets wrong: a table that exists with no primary
    /// key at all yields exactly the same NULL composition as a table that is not there. Reading
    /// that NULL as "absent" waves through a schema whose migration died between
    /// `DROP CONSTRAINT` and `ADD PRIMARY KEY` — the state readiness exists for.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn readiness_answers_three_ways_for_the_optional_carrier() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("optready");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        let forget_readiness = |adapter: &PostgresBaselineAdapter| {
            *adapter.storage_verified_at.lock().unwrap() = None;
        };

        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!("DROP TABLE {schema}.serving_semantic"))
            .unwrap();
        forget_readiness(&adapter);
        adapter
            .check_storage_readiness()
            .expect("an absent optional carrier is not a fault: pgvector may simply be missing");

        raise_pre_root_serving_semantic(&adapter, &schema);
        forget_readiness(&adapter);
        let error = adapter
            .check_storage_readiness()
            .expect_err("an optional carrier that EXISTS must carry the right key");
        assert!(
            error.to_string().contains("serving_semantic"),
            "the refusal must name the carrier: {error}"
        );

        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!(
                "ALTER TABLE {schema}.serving_semantic DROP CONSTRAINT serving_semantic_pkey"
            ))
            .unwrap();
        forget_readiness(&adapter);
        let error = adapter.check_storage_readiness().expect_err(
            "a carrier left with no key at all reads as NULL just like an absent one, and must \
             not be mistaken for it",
        );
        assert!(
            error.to_string().contains("serving_semantic"),
            "the refusal must name the carrier: {error}"
        );
    }

    /// A delta inherits the other root's semantics and rewrites only its own.
    ///
    /// Shadowing blind to the root would take the child's rooted file as covering the
    /// configuration's file of the same path, so the inherited row would never be copied.
    ///
    /// The parent's semantics are published first because only a parent with a completeness mark
    /// makes the plan incremental — without it the strategy rebuilds everything and copies
    /// nothing, and this test would pass without exercising the copy at all.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_delta_inherits_the_other_roots_semantics_and_rewrites_only_its_own() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("delta");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let configuration =
            indexed_document("code", PATH, "Общий", 1, "hash-cfg", "текст-конфигурации");
        let mut extension =
            indexed_document("code", PATH, "Общий", 1, "hash-ext", "текст-расширения");
        extension.root_id = "Расш".to_owned();
        let mut extension_v2 =
            indexed_document("code", PATH, "Общий", 1, "hash-ext2", "текст-расширения-2");
        extension_v2.root_id = "Расш".to_owned();

        let key_of = |document: &IndexedDocument| {
            crate::document::semantic_key_from_parts(
                &document.path,
                &document.kind,
                &document.symbol_name,
                "",
                &document.text,
            )
        };
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (key_of(&configuration), vec![1.0, 0.0, 0.0, 0.0]),
                    (key_of(&extension), vec![0.0, 1.0, 0.0, 0.0]),
                    (key_of(&extension_v2), vec![0.0, 0.0, 1.0, 0.0]),
                ],
            )
            .unwrap();

        adapter
            .publish_snapshot(
                &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &[configuration.clone(), extension],
            )
            .unwrap();
        adapter.populate_serving_semantic("parent", "model", 4).unwrap();

        adapter
            .publish_snapshot(
                &Snapshot::new("child".to_owned(), CorpusId::WorkspaceCode).with_parent("parent"),
                &SnapshotPublishMetadata::default(),
                &[configuration, extension_v2],
            )
            .unwrap();
        adapter.populate_serving_semantic("child", "model", 4).unwrap();

        let mut client = adapter.connect().unwrap();
        let rows: Vec<(String, String)> = client
            .query(
                &format!(
                    "SELECT root_id, embedding::text FROM {schema}.serving_semantic
                      WHERE snapshot_id = 'child' ORDER BY root_id"
                ),
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();

        assert_eq!(
            rows,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), "[1,0,0,0]".to_owned()),
                ("Расш".to_owned(), "[0,0,1,0]".to_owned()),
            ],
            "the configuration's row is inherited unchanged and only the extension's is rebuilt"
        );
    }

    /// Deleting a file under ONE root leaves the other root's semantics standing.
    ///
    /// The mirror of the inheritance case, and the one that loses data rather than merely missing
    /// it: shadowing by `(collection, path)` lets a deletion under the extension shadow the
    /// configuration's file of the same relative path, so the surviving file is served by nothing.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_deletion_under_one_root_leaves_the_other_roots_semantics_standing() {
        const PATH: &str = "CommonModules/Общий/Ext/Module.bsl";
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("delroot");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let configuration =
            indexed_document("code", PATH, "Общий", 1, "hash-cfg", "текст-конфигурации");
        let mut extension =
            indexed_document("code", PATH, "Общий", 1, "hash-ext", "текст-расширения");
        extension.root_id = "Расш".to_owned();
        let key_of = |document: &IndexedDocument| {
            crate::document::semantic_key_from_parts(
                &document.path,
                &document.kind,
                &document.symbol_name,
                "",
                &document.text,
            )
        };
        adapter
            .store_embeddings(
                "model",
                4,
                &[
                    (key_of(&configuration), vec![1.0, 0.0, 0.0, 0.0]),
                    (key_of(&extension), vec![0.0, 1.0, 0.0, 0.0]),
                ],
            )
            .unwrap();

        adapter
            .publish_snapshot(
                &Snapshot::new("parent".to_owned(), CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &[configuration.clone(), extension],
            )
            .unwrap();
        adapter.populate_serving_semantic("parent", "model", 4).unwrap();

        // The extension is gone from the tree, so publishing without it records a deletion for
        // its root alone.
        adapter
            .publish_snapshot(
                &Snapshot::new("child".to_owned(), CorpusId::WorkspaceCode).with_parent("parent"),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&configuration),
            )
            .unwrap();
        adapter.populate_serving_semantic("child", "model", 4).unwrap();

        let mut client = adapter.connect().unwrap();
        let rows: Vec<(String, String)> = client
            .query(
                &format!(
                    "SELECT root_id, embedding::text FROM {schema}.serving_semantic
                      WHERE snapshot_id = 'child' ORDER BY root_id"
                ),
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();

        assert_eq!(
            rows,
            vec![(CONFIGURATION_ROOT_ID.to_owned(), "[1,0,0,0]".to_owned())],
            "the deletion belongs to the extension's root and must not take the configuration \
             file with it"
        );
    }

    /// Every read the plan is built from sees ONE snapshot of the data.
    ///
    /// Observed as the value in force inside production's own transaction, not as a property of
    /// the source: a structural check ("the reads share the same `&mut Transaction`") is green
    /// for a READ COMMITTED transaction, where every statement takes a fresh snapshot and a
    /// concurrent republish can commit between the parent lookup and the file rows.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn the_planning_transaction_holds_one_snapshot_of_the_data() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("isolation");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        seed_two_rooted_files(&adapter, &schema);
        super::OBSERVED_PLANNING_ISOLATION.with(|cell| *cell.borrow_mut() = None);

        let _ = adapter.populate_serving_semantic_with_progress("parent", "model", 8, None);

        assert_eq!(
            super::OBSERVED_PLANNING_ISOLATION.with(|cell| cell.borrow().clone()),
            Some("repeatable read".to_owned()),
            "planning must run at REPEATABLE READ, or the reads it combines may disagree"
        );
    }

    /// A build that knows roots refuses a pre-root schema by name, and stops refusing once the
    /// schema is migrated.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_pre_root_schema_is_refused_by_name_until_it_is_migrated() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("version");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);

        let error = adapter.check_storage_readiness().unwrap_err();
        assert!(
            matches!(error, crate::SearchError::SchemaVersionMismatch { actual: Some(1), .. }),
            "a pre-root schema must be named, not met with a raw SQL error: {error}"
        );

        adapter.migrate_storage().unwrap();

        adapter.check_storage_readiness().expect("a migrated schema must be accepted");
    }

    /// Readiness refuses a carrier whose key lost the root, even though the column is still
    /// there and the table still has a key.
    ///
    /// That exact state is what makes this a control rather than a formality: an implementation
    /// checking "is the column present" and one checking "is there any key at all" both refuse a
    /// dropped column and a keyless table, so those inputs would be green for the very
    /// implementations this test exists to tell apart.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn readiness_refuses_a_carrier_whose_key_lost_the_root() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("keyless");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);
        adapter.migrate_storage().unwrap();

        {
            let mut client = adapter.connect().unwrap();
            client
                .batch_execute(&format!(
                    "ALTER TABLE {schema}.snapshot_files
                         DROP CONSTRAINT snapshot_files_pkey;
                     ALTER TABLE {schema}.snapshot_files
                         ADD PRIMARY KEY (snapshot_id, collection, path);"
                ))
                .unwrap();
        }
        assert!(
            primary_key_columns(&adapter, &format!("{schema}.snapshot_files"))
                .contains(&"snapshot_id".to_owned()),
            "the tampered table must still have a key, or weak implementations pass too"
        );

        // A fresh adapter: a successful check is cached for a minute, and the one above warmed it.
        let cold = PostgresBaselineAdapter::new(config).unwrap();
        let error = cold.check_storage_readiness().unwrap_err();

        assert!(
            error.to_string().contains("snapshot_files"),
            "the refusal must name the carrier that lost the root: {error}"
        );
        assert!(!error.is_retryable(), "a schema this build cannot serve is terminal: {error}");
    }

    /// A migrator refuses a schema newer than the one it knows, and leaves it untouched.
    ///
    /// Forward-only is a property of the migration, not of the operator's memory: running an
    /// older build's `admin migrate` against a migrated schema would otherwise stamp its own
    /// version back over the newer one and rebuild the keys to its own shape — undoing the
    /// version barrier that protects every other consumer.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migrating_a_newer_schema_is_refused_and_changes_nothing() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("newer");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!(
                "UPDATE {schema}._schema_metadata_ SET value = '4' WHERE setting = 'schema_version'"
            ))
            .unwrap();

        let error = adapter.migrate_storage().unwrap_err();

        assert!(
            matches!(
                error,
                crate::SearchError::SchemaVersionMismatch { actual: Some(4), expected: 3, .. }
            ),
            "a newer schema must be named, not quietly downgraded: {error}"
        );
        assert_eq!(
            adapter.get_schema_version().unwrap(),
            Some(4),
            "the refusal must leave the newer version in place"
        );
    }

    /// Readiness refuses a key that CONTAINS the root but is not the key the storage needs.
    ///
    /// The weaker rule — "root_id appears somewhere in the primary key" — accepts a key of
    /// `(root_id)`, which cannot tell two files apart at all. Its damage would then surface as a
    /// duplicate-key error in the middle of a publish, instead of as a named refusal before one.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn readiness_refuses_a_key_that_merely_contains_the_root() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("partialkey");
        let config = ExternalBaselineConfig::postgres(url).with_schema(&schema);
        let adapter = PostgresBaselineAdapter::new(config.clone()).unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();
        adapter
            .connect()
            .unwrap()
            .batch_execute(&format!(
                "ALTER TABLE {schema}.snapshot_files DROP CONSTRAINT snapshot_files_pkey;
                 ALTER TABLE {schema}.snapshot_files ADD PRIMARY KEY (root_id, path);"
            ))
            .unwrap();
        assert!(
            primary_key_columns(&adapter, &format!("{schema}.snapshot_files"))
                .contains(&"root_id".to_owned()),
            "the tampered key must still CONTAIN the root, or the weak rule refuses it too and \
             this control proves nothing"
        );

        let cold = PostgresBaselineAdapter::new(config).unwrap();
        let error = cold.check_storage_readiness().unwrap_err();

        assert!(
            error.to_string().contains("snapshot_files"),
            "the refusal must name the carrier: {error}"
        );
    }

    /// A schema name is refused exactly when two index names truly collide — and not before.
    ///
    /// Both directions are checked because the earlier guard was wrong in the tolerant one: it
    /// refused as soon as the longest name stopped fitting, which is well before any collision,
    /// and that refusal blocks migration outright for a deployment whose only sin is a longish
    /// schema name.
    #[test]
    fn a_schema_name_is_refused_only_when_index_names_actually_collide() {
        let refusal_for = |schema: &str| {
            PostgresBaselineAdapter::new(
                ExternalBaselineConfig::postgres("postgres://127.0.0.1:1").with_schema(schema),
            )
            .unwrap()
            .migrate_storage()
            .unwrap_err()
        };

        // Long enough that names are truncated, short enough that they stay distinct.
        let tolerated = refusal_for(&"a".repeat(30));
        assert_eq!(
            tolerated.reason_code(),
            Some("postgres_connect_failed"),
            "truncation alone is harmless — PostgreSQL cuts declaration and lookup alike: \
             {tolerated}"
        );

        // Long enough that two names become one after the cut.
        let refused = refusal_for(&"a".repeat(50));
        assert_eq!(
            refused.reason_code(),
            Some("schema_name_too_long"),
            "a real collision must be named before the database is touched: {refused}"
        );
    }

    /// The guard reads the index names out of the DDL, so one added later is covered without
    /// anyone remembering this list.
    #[test]
    fn the_guard_sees_every_index_the_ddl_declares() {
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres("postgres://example").with_schema("bsl_search"),
        )
        .unwrap();

        let names = adapter.generated_index_names();

        assert!(
            names.iter().any(|name| name.ends_with("_serving_semantic_snapshot_model")),
            "the optional half of the schema declares indexes too: {names:?}"
        );
        assert!(
            names.len() >= 10,
            "the DDL declares more indexes than this; the scan is missing some: {names:?}"
        );
    }

    /// Re-running the migration is a genuine no-op, proved by object identity.
    ///
    /// Composition alone cannot prove it: an implementation that unconditionally drops and
    /// recreates already-correct keys shows the same final composition — while taking the heavy
    /// lock on the largest tables of the schema on every `admin migrate`.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migrating_an_already_rooted_schema_rebuilds_nothing() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("noop");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);
        adapter.migrate_storage().unwrap();

        // Из константы, а не своим списком: носитель, добавленный туда, попадает под проверку
        // тем же действием, которым объявлен. `serving_semantic` здесь и самый дорогой —
        // повторная перестройка её ключа берёт ACCESS EXCLUSIVE на таблицу векторов.
        // Из константы, а не своим списком, но только по СУЩЕСТВУЮЩИМ таблицам: без pgvector
        // необязательного носителя в схеме нет, и спрашивать его oid значило бы превратить
        // состояние, которое миграция терпит намеренно, в панику каталога.
        let carriers: Vec<&str> = ROOTED_CARRIER_KEYS
            .iter()
            .map(|(table, _, _)| *table)
            .filter(|table| relation_exists(&adapter, &format!("{schema}.{table}")))
            .collect();
        let indexes = ["snapshot_files", "snapshot_deletions"];
        let keys_before: Vec<u32> = carriers
            .iter()
            .map(|table| constraint_oid(&adapter, &format!("{schema}.{table}")))
            .collect();
        let indexes_before: Vec<u32> = indexes
            .iter()
            .map(|table| {
                relation_oid(&adapter, &format!("{schema}.idx_{schema}_{table}_snapshot_path"))
            })
            .collect();

        adapter.migrate_storage().unwrap();

        let keys_after: Vec<u32> = carriers
            .iter()
            .map(|table| constraint_oid(&adapter, &format!("{schema}.{table}")))
            .collect();
        let indexes_after: Vec<u32> = indexes
            .iter()
            .map(|table| {
                relation_oid(&adapter, &format!("{schema}.idx_{schema}_{table}_snapshot_path"))
            })
            .collect();

        assert_eq!(keys_before, keys_after, "a primary key was rebuilt on an unchanged schema");
        assert_eq!(
            indexes_before, indexes_after,
            "a secondary index was rebuilt on an unchanged schema"
        );
    }

    /// Existing rows survive the migration without being rewritten.
    ///
    /// The third assertion is the one the invariant exists for: the first two are green for an
    /// implementation that ran `UPDATE … SET root_id = root_id` over every row, and on a corpus
    /// the size of ERP that is the difference between a cheap migration and a full rewrite.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn migration_backfills_the_root_without_rewriting_a_row() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("backfill");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        raise_pre_root_schema(&adapter, &schema);
        {
            let mut client = adapter.connect().unwrap();
            client
                .batch_execute(&format!(
                    "INSERT INTO {schema}.snapshots (id, corpus) VALUES ('snap-1', 'code');
                     INSERT INTO {schema}.file_objects VALUES ('obj-1', 'code', 'fp-1', 1);
                     INSERT INTO {schema}.snapshot_files
                         VALUES ('snap-1', 'code', 'src/A.bsl', 'fp-1', 1, 'obj-1');"
                ))
                .unwrap();
        }
        let row_version = |adapter: &PostgresBaselineAdapter| -> String {
            let mut client = adapter.connect().unwrap();
            client
                .query_one(&format!("SELECT xmin::text FROM {schema}.snapshot_files"), &[])
                .unwrap()
                .get(0)
        };
        let before = row_version(&adapter);

        adapter.migrate_storage().unwrap();

        let mut client = adapter.connect().unwrap();
        let (path, root_id): (String, String) = {
            let row = client
                .query_one(&format!("SELECT path, root_id FROM {schema}.snapshot_files"), &[])
                .unwrap();
            (row.get(0), row.get(1))
        };
        assert_eq!(path, "src/A.bsl", "the row must survive the migration");
        assert_eq!(root_id, CONFIGURATION_ROOT_ID, "a pre-root row belongs to the configuration");
        assert_eq!(before, row_version(&adapter), "the migration rewrote the row");

        client
            .batch_execute(&format!("UPDATE {schema}.snapshot_files SET document_count = 2"))
            .unwrap();
        assert_ne!(
            before,
            row_version(&adapter),
            "positive control: a real rewrite must move the row version, or this gate is blind"
        );
    }

    /// Drops the throwaway test schema even when an assertion panics mid-test.
    struct TestSchemaGuard {
        adapter: PostgresBaselineAdapter,
        schema: String,
    }

    impl Drop for TestSchemaGuard {
        fn drop(&mut self) {
            if let Ok(mut client) = self.adapter.connect() {
                let _ =
                    client.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema));
            }
        }
    }

    fn materialized_summary(
        adapter: &PostgresBaselineAdapter,
        snapshot_id: &str,
    ) -> EffectiveSnapshotSummary {
        let mut client = adapter.connect().unwrap();
        let visible = materialize_visible_snapshot_files(&mut *client, adapter, snapshot_id)
            .unwrap_or_else(|error| panic!("materialize {snapshot_id}: {error}"));
        let mut documents = 0usize;
        let mut by_collection = std::collections::BTreeMap::<String, (usize, usize)>::new();
        for file in &visible {
            documents += file.document_count;
            let entry = by_collection.entry(file.collection.clone()).or_default();
            entry.0 += 1;
            entry.1 += file.document_count;
        }
        let collections = by_collection
            .into_iter()
            .map(|(collection, (files, documents))| BaselineCollectionRecord {
                collection,
                files,
                documents,
            })
            .collect();
        EffectiveSnapshotSummary {
            total_files: visible.len(),
            total_documents: documents,
            collections,
        }
    }

    /// Parity oracle for the server-side summary aggregate: every snapshot's counts must
    /// equal a recount over the materialized visible-file map, which stays the semantic
    /// reference for resolve and publish paths.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn snapshot_summary_aggregate_matches_materialized_visibility_on_live_postgres() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("parity");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        let corpus = CorpusId::WorkspaceCode;
        let metadata = SnapshotPublishMetadata::default();
        let changed_a = [
            indexed_document("code", "src/A.bsl", "A1", 10, "hash-a1-v2", "text-a1-v2"),
            indexed_document("code", "src/A.bsl", "A2", 20, "hash-a2-v2", "text-a2-v2"),
            indexed_document("code", "src/A.bsl", "A3", 30, "hash-a3", "text-a3"),
        ];

        adapter
            .publish_snapshot(
                &Snapshot::new("snap-1", corpus.clone()),
                &metadata,
                &[
                    indexed_document("code", "src/A.bsl", "A1", 10, "hash-a1", "text-a1"),
                    indexed_document("code", "src/A.bsl", "A2", 20, "hash-a2", "text-a2"),
                    indexed_document("code", "src/B.bsl", "B", 10, "hash-b", "text-b"),
                    indexed_document("platform", "std/P.bsl", "P", 10, "hash-p", "text-p"),
                ],
            )
            .unwrap();
        // B and P are not republished: the publish path records them as deletions.
        adapter
            .publish_snapshot(
                &Snapshot::new("snap-2", corpus.clone()).with_parent("snap-1"),
                &metadata,
                &[
                    changed_a[0].clone(),
                    changed_a[1].clone(),
                    changed_a[2].clone(),
                    indexed_document("code", "src/C.bsl", "C", 10, "hash-c", "text-c"),
                ],
            )
            .unwrap();
        // A is byte-identical to snap-2, so it is reused (visible via the parent row only);
        // P returns after an ancestor deletion; C drops out.
        adapter
            .publish_snapshot(
                &Snapshot::new("snap-3", corpus.clone()).with_parent("snap-2"),
                &metadata,
                &[
                    changed_a[0].clone(),
                    changed_a[1].clone(),
                    changed_a[2].clone(),
                    indexed_document("code", "src/D.bsl", "D1", 10, "hash-d1", "text-d1"),
                    indexed_document("code", "src/D.bsl", "D2", 20, "hash-d2", "text-d2"),
                    indexed_document("platform", "std/P.bsl", "P", 10, "hash-p", "text-p"),
                ],
            )
            .unwrap();
        // An empty publish over a non-empty parent turns every visible file into a deletion.
        adapter
            .publish_snapshot(
                &Snapshot::new("snap-4", corpus.clone()).with_parent("snap-3"),
                &metadata,
                &[],
            )
            .unwrap();
        adapter
            .publish_snapshot(&Snapshot::new("root-empty", corpus.clone()), &metadata, &[])
            .unwrap();

        // A file row and a deletion row for the same key in the same snapshot cannot come
        // out of the publish path; the deletion must win on the ancestry-position tie.
        {
            let mut client = adapter.connect().unwrap();
            client
                .execute(
                    &format!(
                        "INSERT INTO {} (id, collection, file_fingerprint, document_count)
                         VALUES ($1, $2, $3, $4)",
                        adapter.table("file_objects")
                    ),
                    &[&"adversarial-fo", &"code", &"adversarial-fp", &7i32],
                )
                .unwrap();
            client
                .execute(
                    &format!(
                        "INSERT INTO {} (snapshot_id, collection, path, file_fingerprint,
                                         document_count, file_object_id)
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        adapter.table("snapshot_files")
                    ),
                    &[
                        &"snap-3",
                        &"code",
                        &"src/Shadowed.bsl",
                        &"adversarial-fp",
                        &7i32,
                        &"adversarial-fo",
                    ],
                )
                .unwrap();
            client
                .execute(
                    &format!(
                        "INSERT INTO {} (snapshot_id, collection, path) VALUES ($1, $2, $3)",
                        adapter.table("snapshot_deletions")
                    ),
                    &[&"snap-3", &"code", &"src/Shadowed.bsl"],
                )
                .unwrap();
        }

        struct SnapshotExpectation {
            snapshot_id: &'static str,
            files: usize,
            documents: usize,
            collections: &'static [(&'static str, usize, usize)],
        }
        let expectations = [
            SnapshotExpectation {
                snapshot_id: "snap-1",
                files: 3,
                documents: 4,
                collections: &[("code", 2, 3), ("platform", 1, 1)],
            },
            SnapshotExpectation {
                snapshot_id: "snap-2",
                files: 2,
                documents: 4,
                collections: &[("code", 2, 4)],
            },
            SnapshotExpectation {
                snapshot_id: "snap-3",
                files: 3,
                documents: 6,
                collections: &[("code", 2, 5), ("platform", 1, 1)],
            },
            SnapshotExpectation { snapshot_id: "snap-4", files: 0, documents: 0, collections: &[] },
            SnapshotExpectation {
                snapshot_id: "root-empty",
                files: 0,
                documents: 0,
                collections: &[],
            },
        ];
        for expectation in &expectations {
            let snapshot_id = expectation.snapshot_id;
            let details = adapter
                .snapshot_details(snapshot_id)
                .unwrap()
                .unwrap_or_else(|| panic!("snapshot {snapshot_id} must exist"));
            let materialized = materialized_summary(&adapter, snapshot_id);
            assert_eq!(
                materialized.total_files, expectation.files,
                "{snapshot_id}: materialized files"
            );
            assert_eq!(
                materialized.total_documents, expectation.documents,
                "{snapshot_id}: materialized documents"
            );
            assert_eq!(
                details.snapshot.files, materialized.total_files,
                "{snapshot_id}: aggregate files diverge from materialization"
            );
            assert_eq!(
                details.snapshot.documents, materialized.total_documents,
                "{snapshot_id}: aggregate documents diverge from materialization"
            );
            assert_eq!(
                details.collections, materialized.collections,
                "{snapshot_id}: aggregate collections diverge from materialization"
            );
            let expected_collections: Vec<BaselineCollectionRecord> = expectation
                .collections
                .iter()
                .map(|(collection, files, documents)| BaselineCollectionRecord {
                    collection: (*collection).to_owned(),
                    files: *files,
                    documents: *documents,
                })
                .collect();
            assert_eq!(details.collections, expected_collections, "{snapshot_id}: collections");
        }

        let listed = adapter.list_snapshots(None, None, None, 10).unwrap();
        assert_eq!(listed.len(), expectations.len(), "listing must cover every snapshot");
        for record in &listed {
            let materialized = materialized_summary(&adapter, &record.snapshot_id);
            assert_eq!(
                record.files, materialized.total_files,
                "{}: listed files diverge from materialization",
                record.snapshot_id
            );
            assert_eq!(
                record.documents, materialized.total_documents,
                "{}: listed documents diverge from materialization",
                record.snapshot_id
            );
        }
    }

    /// The batched totals path reimplements ancestry walking in SQL; it must surface the same
    /// cycle / missing-parent errors as the iterative `snapshot_ancestry_ids`, and pick the error
    /// deterministically (first corrupt seed in listing order) when several seeds are corrupt.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn snapshot_totals_batch_surfaces_corrupt_ancestry_on_live_postgres() {
        let url = std::env::var("BSL_TEST_PG_URL")
            .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
        let schema = unique_schema("corrupt");
        let adapter = PostgresBaselineAdapter::new(
            ExternalBaselineConfig::postgres(url).with_schema(&schema),
        )
        .unwrap();
        let _schema_guard = TestSchemaGuard { adapter: adapter.clone(), schema: schema.clone() };
        adapter.migrate_storage().unwrap();

        // A healthy snapshot, plus two corrupt ones inserted directly: the publish path cannot
        // produce a self-cycle or a parent pointing at a non-existent snapshot.
        adapter
            .publish_snapshot(
                &Snapshot::new("good", CorpusId::WorkspaceCode),
                &SnapshotPublishMetadata::default(),
                &[indexed_document("code", "src/A.bsl", "A", 5, "hash-a", "text-a")],
            )
            .unwrap();
        {
            let mut client = adapter.connect().unwrap();
            let insert = format!(
                "INSERT INTO {} (id, corpus, parent_snapshot_id) VALUES ($1, $2, $3)",
                adapter.table("snapshots")
            );
            client.execute(&insert, &[&"cyc", &"workspace-code", &"cyc"]).unwrap();
            client.execute(&insert, &[&"dang", &"workspace-code", &"ghost"]).unwrap();
        }

        let mut client = adapter.connect().unwrap();
        let seed = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<String>>();

        let healthy =
            effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["good"])).unwrap();
        let reference = materialized_summary(&adapter, "good");
        assert_eq!(
            healthy.get("good").copied(),
            Some((reference.total_files, reference.total_documents)),
            "healthy seed totals must match the materialized reference",
        );

        let cycle = effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["cyc"]))
            .expect_err("self-cycle must error");
        assert!(cycle.to_string().contains("cycle at 'cyc'"), "unexpected cycle error: {cycle}");

        let missing = effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["dang"]))
            .expect_err("dangling parent must error");
        assert!(
            missing.to_string().contains("'ghost' was not found"),
            "unexpected missing error: {missing}",
        );

        // The first corrupt seed in listing order decides the error, regardless of result-set order.
        let cyc_first =
            effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["cyc", "dang"]))
                .expect_err("mixed corrupt batch must error");
        assert!(cyc_first.to_string().contains("cycle at 'cyc'"), "cyc-first: {cyc_first}");
        let dang_first =
            effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["dang", "cyc"]))
                .expect_err("mixed corrupt batch must error");
        assert!(
            dang_first.to_string().contains("'ghost' was not found"),
            "dang-first: {dang_first}",
        );

        // A healthy seed alongside a corrupt one still fails the whole batch.
        let mixed =
            effective_snapshot_totals_batch(&mut *client, &adapter, &seed(&["good", "dang"]))
                .expect_err("healthy + corrupt must error");
        assert!(mixed.to_string().contains("'ghost' was not found"), "good+dang: {mixed}");
    }
}
