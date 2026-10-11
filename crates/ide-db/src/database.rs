use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stdx::case::CaseExt;

use dashmap::DashMap;
use rustc_hash::FxHasher;

use base_db::{FileIdInput, Files, RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use hir::{
    ConditionalTree, DefDatabase, ItemTree, ModuleBodies, ModuleData, ModuleId, RegionTree,
    SymbolTree,
};
use vfs::FileId;

use crate::features::FeaturesInput;
use crate::metadata::{GlobalConfigRevisionInput, MetadataDb};
use crate::queries::{
    line_index_query, method_cfg_query, module_metadata_query, reaching_definitions_query,
};
use crate::type_kernel::TypeKernelInner;
use crate::{metadata, queries, vfs_helpers, RootDatabase, SdblHirEntries};
use hir::{all_sdbl_in_file_query, sdbl_hir_for_file_query};
use intern::NormName;

/// Query families whose salsa key is exactly one `MethodIdInput` — the memos
/// that live per method. The hot-key decoder reads the key through this
/// list, and the incrementality stands count executions per family from it,
/// so a per-method query missing here is invisible to both. Check the
/// signature before adding a name.
pub const METHOD_KEYED_QUERY_FAMILIES: &[&str] = &[
    "infer_method_query",
    "interface_method_query",
    "method_syntax_query",
    "method_slab_query",
    "method_lower_query",
    "method_body_query",
    "method_sdbl_hir_query",
    "method_return_type_query",
    "proc_signature_query",
    "doc_see_signature_query",
    "structure_param_keys_query",
    "method_cfg_query",
    "reaching_definitions_query",
    "method_path_terminates_query",
    "method_security_state_query",
    "method_hir_metrics_query",
    "method_cyclomatic_query",
    "method_effect_summary_query",
    "method_arg_diagnostics_query",
];

/// The config roots visible to one file: the base configuration plus the
/// file's dependency-ordered extension chain. `chain` holds `(name, path)`
/// pairs — transitive dependencies in stable topological order, the file's
/// own extension last. Forward iteration is overlay-composition order;
/// reverse iteration is replacement/name-lookup precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleRoots {
    pub main: Option<PathBuf>,
    pub chain: Vec<(String, PathBuf)>,
}

/// One effective top-level member of a metadata object together with the root
/// whose overlay supplied the winning value. `None` denotes the base
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMetadataMember {
    pub member: EffectiveMetadataMemberValue,
    pub source_extension: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveMetadataMemberValue {
    Attribute(bsl_metadata::Attribute),
    TabularSection(bsl_metadata::TabularSection),
}

impl EffectiveMetadataMemberValue {
    pub fn name(&self) -> &str {
        match self {
            Self::Attribute(attribute) => &attribute.name,
            Self::TabularSection(section) => section.name(),
        }
    }
}

#[salsa::db]
pub struct RootDatabaseImpl {
    storage: salsa::Storage<Self>,

    files: Files,

    /// Per-config-root revision inputs, keyed by the canonical root path. Shared
    /// across cloned database handles (snapshots) so every handle reads and bumps
    /// the same Salsa input for a root. A config-dependent query reads the input
    /// for its file's root (recording a Salsa dependency); bumping that root's
    /// revision invalidates only those queries, not every configuration.
    config_revisions:
        Arc<DashMap<String, metadata::ConfigRevisionInput, BuildHasherDefault<FxHasher>>>,

    /// Per-config-root metadata *structure* inputs, keyed by the canonical root
    /// path. Shared across cloned handles like [`config_revisions`]. Each holds the
    /// list of MDOs discovered in that root; a structure change re-sets it, driving
    /// `config_index`/`resolve_metadata_object` re-derivation for that root only.
    metadata_listings:
        Arc<DashMap<String, metadata::MetadataListingInput, BuildHasherDefault<FxHasher>>>,

    type_kernel: Arc<TypeKernelInner>,

    /// Optional process-side cache of loaded configurations, keyed by the interned
    /// config-root path string (stable and consistent across this build's batch
    /// databases — not necessarily filesystem-canonical, which does not matter since
    /// every batch interns the same root the same way). Set only by the batched
    /// graph build, which opens a fresh
    /// database per batch: without it each batch would reload the whole
    /// configuration via [`metadata::load_configuration`] (re-running
    /// `bsl_metadata::load_from_directory` over every metadata file). A single
    /// `Arc` is shared across all of one build's batch databases (and their per-job
    /// clones), so the load happens once. `None` for the long-lived LSP database,
    /// which already memoises `load_configuration` per revision — there the field is
    /// absent and loading is unchanged.
    graph_config_cache: Option<Arc<GraphConfigCache>>,

    /// Opt-in salsa event counters (`BSL_SALSA_EVENTS=1`). `Some` installs an
    /// `event_callback` on the storage; the same `Arc` is shared across cloned
    /// handles so the counters are process-global for this database tree. `None`
    /// (the default) leaves the hot path at salsa's single `is_some` branch per
    /// event. See [`crate::salsa_events`].
    salsa_events: Option<Arc<crate::salsa_events::SalsaEventStats>>,
}

/// Build-scoped cache of loaded configurations by interned config-root path string.
/// A fresh instance per build keeps it a content snapshot — a later build (new
/// instance) never sees a stale entry — so no version key is needed.
pub type GraphConfigCache = dashmap::DashMap<PathBuf, Arc<bsl_metadata::Configuration>>;

/// Whether to install the salsa event-counter callback, gated by `BSL_SALSA_EVENTS=1`.
/// Off leaves the runtime hot path at salsa's single `is_some` branch per event.
fn salsa_events_enabled_by_env() -> bool {
    matches!(std::env::var("BSL_SALSA_EVENTS").as_deref(), Ok("1"))
}

/// Fold the same-named common modules of the visible roots, base first and the
/// file's own root last: an adopted copy overlays the fold so far, any other
/// same-named module is independent and replaces it.
fn overlay_common_modules(
    modules: impl Iterator<Item = Option<Arc<bsl_metadata::CommonModule>>>,
) -> Option<Arc<bsl_metadata::CommonModule>> {
    modules.flatten().reduce(|base, overlay| {
        if !overlay.adopts(&base) {
            return overlay;
        }
        let mut merged = Arc::unwrap_or_clone(base);
        merged.apply_extension_overlay(&overlay);
        Arc::new(merged)
    })
}

impl Default for RootDatabaseImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for RootDatabaseImpl {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            files: self.files.clone(),
            config_revisions: Arc::clone(&self.config_revisions),
            metadata_listings: Arc::clone(&self.metadata_listings),
            type_kernel: Arc::clone(&self.type_kernel),
            // Share the same cache across clones so a per-job db clone sees configs
            // loaded by its siblings.
            graph_config_cache: self.graph_config_cache.clone(),
            // Salsa shares one `event_callback` across all storage clones, so every
            // handle must point at the same stats to keep counts consistent.
            salsa_events: self.salsa_events.clone(),
        }
    }
}

impl RootDatabaseImpl {
    pub fn new() -> Self {
        Self::new_inner(salsa_events_enabled_by_env())
    }

    /// A database with the salsa event counters installed regardless of
    /// `BSL_SALSA_EVENTS` (process-global and racy under the test harness).
    /// For stands and tests that assert *what* a change recomputes; the env
    /// gate stays the production switch.
    pub fn new_with_salsa_events() -> Self {
        Self::new_inner(true)
    }

    fn new_inner(events_enabled: bool) -> Self {
        // Install the event callback (if any) before any input is created so it
        // observes the whole database lifetime, including the singleton inputs below.
        let salsa_events =
            events_enabled.then(|| Arc::new(crate::salsa_events::SalsaEventStats::default()));
        let storage = match &salsa_events {
            Some(stats) => {
                let stats = Arc::clone(stats);
                salsa::Storage::builder()
                    .event_callback(Box::new(move |event| stats.record(&event)))
                    .build()
            }
            None => salsa::Storage::default(),
        };
        let db = Self {
            storage,
            files: Files::new(),
            config_revisions: Arc::new(DashMap::default()),
            metadata_listings: Arc::new(DashMap::default()),
            type_kernel: Arc::new(TypeKernelInner::new()),
            graph_config_cache: None,
            salsa_events,
        };
        // The singleton inputs are created once here and retrieved via `::get()`
        // afterwards; a second `new()` against the same storage would panic inside
        // salsa, but every construction path goes through this function.
        //
        // The workspace/metadata inputs are MEDIUM durability: they change only on
        // config drift or reload — rare batches against the keystroke stream. A
        // `.bsl` edit is a LOW write, so every memo whose cone is MEDIUM-floored
        // (the metadata chain) shallow-verifies in O(1) instead of re-walking its
        // dependency edges on each keystroke.
        use salsa::Durability;
        let _ = metadata::WorkspaceConfigsInput::builder(Arc::new(
            metadata::WorkspaceConfigsSnapshot::default(),
        ))
        .durability(Durability::MEDIUM)
        .new(&db);
        let _ = metadata::WorkspaceLoadStateInput::builder(true)
            .durability(Durability::MEDIUM)
            .new(&db);
        let _ = GlobalConfigRevisionInput::builder(0).durability(Durability::MEDIUM).new(&db);
        let defaults = project_model::FeaturesConfig::default();
        let _ = FeaturesInput::builder(
            defaults.type_narrowing,
            hir::execution_env::EnvOptions::default(),
            None,
            None,
            None,
        )
        .durability(Durability::MEDIUM)
        .new(&db);
        db
    }

    pub(crate) fn type_kernel_inner(&self) -> &Arc<TypeKernelInner> {
        &self.type_kernel
    }

    /// Run salsa's LRU trim now, dropping memoized values beyond each query's
    /// configured `lru` cap and releasing their heap. salsa only trims
    /// automatically at a revision boundary, so a single-revision batch (seed
    /// inputs once, never mutate) must call this explicitly — e.g. between
    /// chunks of files — to bound resident memory. Requires exclusive `&mut`
    /// access and blocks on any live snapshot, so never call it mid-`par_iter`.
    pub fn enforce_lru(&mut self) {
        salsa::Database::trigger_lru_eviction(self);
    }

    /// Run one deep LRU trim: evict with the heaviest per-file caps (parse trees,
    /// lowered bodies) shrunk to a small sweep profile, then restore the interactive
    /// caps — restoring evicts nothing, the wide window simply repopulates on
    /// demand. For a whole-workspace sweep's final cleanup, where the swept files'
    /// memos are working set nothing will read again: a plain [`Self::enforce_lru`]
    /// would leave a full interactive window of them resident. The shrink/restore
    /// pair lives inside this method so no caller can strand the shrunk caps on the
    /// long-lived database. Same exclusivity contract as [`Self::enforce_lru`].
    pub fn enforce_lru_deep(&mut self) {
        self.set_sweep_lru(true);
        self.enforce_lru();
        self.set_sweep_lru(false);
    }

    /// Switch the heaviest per-file LRU caps between the interactive profile and the
    /// small sweep profile. The caps take effect at the next eviction; switching
    /// evicts nothing by itself. Like any salsa write this cancels in-flight
    /// snapshots — private on purpose, so the only user is the shrink/evict/restore
    /// sequence of [`Self::enforce_lru_deep`].
    fn set_sweep_lru(&mut self, sweep: bool) {
        base_db::set_parse_lru_sweep_mode(self, sweep);
        hir::set_lowering_lru_sweep_mode(self, sweep);
        queries::set_dataflow_lru_sweep_mode(self, sweep);
    }

    /// Per-ingredient memory snapshot for diagnostics tooling, via salsa's
    /// `memory_usage` introspection (the `salsa_unstable` feature). Each tuple is
    /// `(ingredient name, live entry count, salsa metadata bytes, field stack bytes,
    /// optional heap bytes)`. `heap` is `None` unless the ingredient implements a
    /// `heap_size` hook, so the strongest signal here is `count` (whether LRU is
    /// actually evicting) rather than absolute bytes.
    pub fn memory_report(&self) -> Vec<(&'static str, usize, usize, usize, Option<usize>)> {
        let db: &dyn salsa::Database = self;
        let info = db.memory_usage();
        info.structs
            .iter()
            .chain(info.queries.values())
            .map(|ing| {
                (
                    ing.debug_name(),
                    ing.count(),
                    ing.size_of_metadata(),
                    ing.size_of_fields(),
                    ing.heap_size_of_fields(),
                )
            })
            .collect()
    }

    /// Per-ingredient salsa event counters (executes / validates / discards /
    /// interns), sorted by descending execute count. `None` unless the database was
    /// built with events enabled (`BSL_SALSA_EVENTS=1`). Names are resolved here via
    /// [`salsa::Database::ingredient_debug_name`] — the callback that accumulates the
    /// counts cannot touch the database.
    pub fn salsa_event_report(&self) -> Option<Vec<crate::salsa_events::SalsaEventRow>> {
        use salsa::Database;
        let stats = self.salsa_events.as_ref()?;
        Some(stats.rows(|idx| self.ingredient_debug_name(idx).into_owned()))
    }

    /// Global (keyless) salsa event counters — cancellation checks/flags and
    /// accumulator discards. `None` unless events are enabled; see
    /// [`Self::salsa_event_report`].
    pub fn salsa_event_global(&self) -> Option<crate::salsa_events::GlobalCounts> {
        self.salsa_events.as_ref().map(|s| s.global_counts())
    }

    /// `(execute, validate)` of one query family in the current observation
    /// window, matched on the family's unqualified name (`infer_method_query`).
    /// A family that recorded no event in the window reads as `(0, 0)` — the
    /// value a churn assertion wants. `None` unless events are enabled.
    pub fn salsa_family_churn(&self, family: &str) -> Option<(u64, u64)> {
        let rows = self.salsa_event_report()?;
        Some(
            rows.iter()
                .find(|r| r.name.rsplit("::").next().unwrap_or(&r.name) == family)
                .map_or((0, 0), |r| (r.execute, r.validate)),
        )
    }

    /// The `k` hottest salsa *keys* (concrete file/method query instances) by
    /// re-execution count, resolved to readable names. This answers "which files
    /// or methods drove the churn", the per-key complement to
    /// [`Self::salsa_event_report`]'s per-query-type view. `None` unless events are
    /// enabled (`BSL_SALSA_EVENTS=1`).
    ///
    /// Resolution decodes the key's interned input (`FileIdInput` / `MethodIdInput`)
    /// back to a path, which is only sound *in the revision that produced the keys*:
    /// a later revision may have reused the interned slot (salsa ids carry a
    /// generation), so a stale decode could misname or fault. Callers must therefore
    /// invoke this at the end of a single-revision batch (the CLI `analyze` / smoke
    /// runs), not from an incremental LSP session. The [`salsa::attach`] scope makes
    /// the fallback for unlisted query families render as `query(Id(..))` rather than
    /// raw numeric indices; each interned read is panic-guarded so a decode fault
    /// degrades one row to that fallback instead of aborting the report.
    pub fn salsa_key_event_report(
        &self,
        k: usize,
    ) -> Option<Vec<crate::salsa_events::KeyEventRow>> {
        let stats = self.salsa_events.as_ref()?;
        let counts = stats.top_keys(k);
        Some(salsa::attach(self, || {
            counts
                .into_iter()
                .map(|c| crate::salsa_events::KeyEventRow {
                    name: self.resolve_hot_key_name(c.key),
                    execute: c.execute,
                    discard_stale: c.discard_stale,
                })
                .collect()
        }))
    }

    /// Zero the salsa event counters, opening a fresh observation window.
    /// Returns `false` when events are disabled (no `BSL_SALSA_EVENTS=1`).
    /// See [`crate::salsa_events::SalsaEventStats::reset`] for the quiescence
    /// contract.
    pub fn salsa_events_reset(&self) -> bool {
        match self.salsa_events.as_ref() {
            Some(stats) => {
                stats.reset();
                true
            }
            None => false,
        }
    }

    /// The *complete* per-key churn of the current observation window — exact
    /// distinct-key count and every key resolved to a readable name — unlike
    /// the hottest-N sample of [`Self::salsa_key_event_report`].
    ///
    /// Same revision constraint as the top-K report, restated as a windowed
    /// protocol: open the window with [`Self::salsa_events_reset`], apply at
    /// most one revision-bumping change, run the queries under observation,
    /// and call this *before any further revision bump* — every recorded key
    /// then decodes in the revision that produced it. `None` unless events
    /// are enabled.
    pub fn salsa_key_event_window(&self) -> Option<crate::salsa_events::KeyEventWindow> {
        let stats = self.salsa_events.as_ref()?;
        let counts = stats.all_keys();
        let distinct_keys = counts.len();
        let rows = salsa::attach(self, || {
            counts
                .into_iter()
                .map(|c| crate::salsa_events::KeyEventRow {
                    name: self.resolve_hot_key_name(c.key),
                    execute: c.execute,
                    discard_stale: c.discard_stale,
                })
                .collect()
        });
        Some(crate::salsa_events::KeyEventWindow { distinct_keys, rows })
    }

    /// Name one hot salsa key: `query(<module path>[#<name>#<ordinal>])` for the curated
    /// per-file / per-method query families, else the attach-rendered
    /// `query(Id(..))` fallback. Must run inside a [`salsa::attach`] scope (for the
    /// fallback) and only within the key's originating revision (see
    /// [`Self::salsa_key_event_report`]).
    fn resolve_hot_key_name(&self, key: salsa::DatabaseKeyIndex) -> String {
        use salsa::Database;
        let query = self.ingredient_debug_name(key.ingredient_index());
        self.decode_hot_key(query.as_ref(), key.key_index()).unwrap_or_else(|| format!("{key:?}"))
    }

    /// Decode a hot key's interned id to a readable location for the curated query
    /// families. Returns `None` for unlisted queries so the caller falls back to the
    /// raw id — an unknown family is never guessed, which would risk decoding the id
    /// against the wrong interned table.
    fn decode_hot_key(&self, query: &str, id: salsa::Id) -> Option<String> {
        use salsa::plumbing::FromId;
        // A qualified debug name (`crate::module::parse_query`) still matches on its
        // final segment.
        let name = query.rsplit("::").next().unwrap_or(query);

        // Only queries whose salsa key is a *single* interned input can be decoded
        // this way: the key id then IS that input's id. Queries with a composite key
        // (e.g. `file_diagnostics_query(FileIdInput, DiagnosticsConfigId)`) intern the
        // whole tuple, so their id is not a `FileIdInput` and must not be listed —
        // it would decode against the wrong table. An unlisted query degrades to the
        // raw-id fallback rather than risking a wrong-table decode.
        const FILE_KEYED: &[&str] = &[
            "parse_query",
            "file_text_query",
            "item_tree_query",
            "symbol_tree_query",
            "infer_query",
            "infer_module_code_query",
            "module_bodies_query",
            "module_code_reaching_definitions_query",
            "module_code_security_state_query",
            "module_code_arg_diagnostics_query",
        ];
        const METHOD_KEYED: &[&str] = METHOD_KEYED_QUERY_FAMILIES;

        if FILE_KEYED.contains(&name) {
            let file_id = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                FileIdInput::from_id(id).file_id(self)
            }))
            .ok()?;
            let path = vfs_helpers::get_file_path(self, file_id)?;
            Some(format!("{query}({})", path.display()))
        } else if METHOD_KEYED.contains(&name) {
            let method_id = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hir::MethodIdInput::from_id(id).method_id(self)
            }))
            .ok()?;
            let path = vfs_helpers::get_file_path(self, method_id.module.file_id)?;
            Some(format!("{query}({}#{:?})", path.display(), method_id.local_id))
        } else {
            None
        }
    }

    /// Attach a build-scoped configuration cache shared across this build's batch
    /// databases. See [`GraphConfigCache`] and the `graph_config_cache` field.
    pub fn set_graph_config_cache(&mut self, cache: Arc<GraphConfigCache>) {
        self.graph_config_cache = Some(cache);
    }

    /// Warm body inference for every method of a large module in parallel, before
    /// the sequential `infer_query` fold reads them from cache. Each method infers
    /// its own body and pulls callee return types via `method_return_type`, which
    /// projects `infer_method` — so a method's inference can recurse into another
    /// `infer_method`. Both queries carry salsa Fixpoint recovery, so recursive and
    /// mutually recursive call SCCs converge rather than panic, and the warming
    /// stays cycle safe under cross-thread demand. The payoff is when this module is
    /// the straggler tail of its batch chunk: the inner `par_iter` reclaims the
    /// cores left idle by the finished files. Returns the number of methods warmed,
    /// or 0 if the module has fewer than `min_methods` (priming a small module is
    /// pure overhead).
    pub fn prime_module_inference(&self, file_id: FileId, min_methods: usize) -> usize {
        use hir::HirDatabase;
        use rayon::prelude::*;

        let module_id = ModuleId { file_id };
        let method_ids: Vec<hir::MethodId> = self
            .module_bodies_ref(module_id)
            .iter_bodies()
            .map(|(local_id, _)| hir::MethodId { module: module_id, local_id })
            .collect();
        if method_ids.len() < min_methods {
            return 0;
        }
        let n = method_ids.len();
        method_ids.par_iter().for_each_with(self.clone(), |db, &method_id| {
            let method_input = hir::MethodIdInput::new(&*db, method_id);
            let _ = db.infer_method_ref(method_input);
        });
        n
    }

    fn workspace_configs(&self) -> metadata::WorkspaceConfigsInput {
        metadata::WorkspaceConfigsInput::try_get(self)
            .expect("WorkspaceConfigsInput is created in RootDatabaseImpl::new")
    }

    fn workspace_load_state(&self) -> metadata::WorkspaceLoadStateInput {
        metadata::WorkspaceLoadStateInput::try_get(self)
            .expect("WorkspaceLoadStateInput is created in RootDatabaseImpl::new")
    }

    /// Whether the host's initial workspace load has completed; see
    /// [`metadata::WorkspaceLoadStateInput`]. Reading it inside a tracked query
    /// records a dependency, so the finalize flip recomputes whatever resolved
    /// against the boot-window stub.
    pub fn workspace_load_complete(&self) -> bool {
        self.workspace_load_state().complete(self)
    }

    pub fn set_workspace_load_complete(&mut self, complete: bool) {
        use salsa::Setter;
        let input = self.workspace_load_state();
        input.set_complete(self).to(complete);
    }

    fn global_config_revision_input(&self) -> GlobalConfigRevisionInput {
        GlobalConfigRevisionInput::try_get(self)
            .expect("GlobalConfigRevisionInput is created in RootDatabaseImpl::new")
    }

    /// The current revision for a registered config root, as recorded in its
    /// Salsa [`ConfigRevisionInput`](metadata::ConfigRevisionInput). Reading the
    /// input field here records a dependency on that specific root for the
    /// enclosing tracked query, so a later [`bump_config_revision`] invalidates
    /// only the queries that touched this root. Unregistered roots fall back to
    /// the global revision input, so they still record a dependency (coarse).
    pub fn config_revision(&self, root: &str) -> u32 {
        let key = metadata::canonicalize_configuration_path(root);
        match self.config_revisions.get(&key).map(|e| *e.value()) {
            Some(input) => input.revision(self),
            None => self.global_config_revision_input().revision(self),
        }
    }

    /// The longest registered config root that is a prefix of `path` (the same
    /// matching rule used by both reads and bumps, so their revision keys always
    /// agree). Reading the workspace config paths records a dependency, so a
    /// workspace reload invalidates every config-dependent query.
    fn longest_config_root_for_path(&self, path: &Path) -> Option<PathBuf> {
        // Both spellings match, as in `visible_roots_for_file`: a file enumerated
        // canonically under a root configured through a symlink still belongs to
        // it. The CONFIGURED spelling is what comes back, so the revision key a
        // reader interns is the one a bump on that root increments.
        let snapshot = self.workspace_configs_snapshot();
        snapshot
            .paths
            .iter()
            .zip(&snapshot.canonical_paths)
            .filter_map(|((_, configured), canonical)| {
                [configured, canonical]
                    .into_iter()
                    .filter(|root| path.starts_with(root))
                    .map(|root| root.as_os_str().len())
                    .max()
                    .map(|len| (len, configured))
            })
            .max_by_key(|&(len, _)| len)
            .map(|(_, configured)| configured.clone())
    }

    /// The revision token to fold into a config's interned key for any reader
    /// concerned with `path` (a source file or a config root). Derives the root
    /// by [`longest_config_root_for_path`] so file readers and per-root config
    /// readers share one key; unmatched paths use the global fallback revision.
    pub fn config_root_revision_for_path(&self, path: &Path) -> u32 {
        match self.longest_config_root_for_path(path) {
            Some(root) => self.config_revision(&root.to_string_lossy()),
            None => self.global_config_revision_input().revision(self),
        }
    }

    pub fn try_file_text(&self, file_id: vfs::FileId) -> Option<base_db::FileTextInput> {
        self.files.try_file_text(file_id)
    }

    /// Create the [`ConfigRevisionInput`](metadata::ConfigRevisionInput) for a
    /// root if it does not exist yet. Must be called outside any tracked query
    /// (Salsa forbids creating inputs during query execution). Idempotent: an
    /// existing root keeps its accumulated revision so previously recorded
    /// dependencies stay valid across workspace reloads.
    pub fn ensure_config_revision_input(&mut self, root: &str) {
        let key = metadata::canonicalize_configuration_path(root);
        if self.config_revisions.contains_key(&key) {
            return;
        }
        let input = metadata::ConfigRevisionInput::builder(0)
            .durability(salsa::Durability::MEDIUM)
            .new(self);
        self.config_revisions.insert(key, input);
    }

    /// Bump one config root's revision, invalidating only the queries that read
    /// that root's configuration. Creates the input first if needed.
    pub fn bump_config_revision(&mut self, root: &str) {
        use salsa::Setter;
        self.ensure_config_revision_input(root);
        let key = metadata::canonicalize_configuration_path(root);
        let input = match self.config_revisions.get(&key).map(|e| *e.value()) {
            Some(input) => input,
            None => return,
        };
        let current = input.revision(self);
        input.set_revision(self).to(current.wrapping_add(1));
    }

    /// Bump the revision for the config root that owns `path`, matched the same
    /// way reads derive their revision key. A path under no registered root bumps
    /// the global fallback instead.
    pub fn bump_config_for_path(&mut self, path: &Path) {
        use salsa::Setter;
        match self.longest_config_root_for_path(path) {
            Some(root) => self.bump_config_revision(&root.to_string_lossy()),
            None => {
                let input = self.global_config_revision_input();
                let current = input.revision(self);
                input.set_revision(self).to(current.wrapping_add(1));
            }
        }
    }

    /// Bump the revision for every config root that owns one of `paths`, each
    /// root at most once (so an N-file batch under one root is a single revision
    /// write, not N). Paths under no registered root bump the global fallback once.
    pub fn bump_config_for_paths<'a, I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = &'a Path>,
    {
        use salsa::Setter;
        let mut roots: Vec<String> = Vec::new();
        let mut bump_global = false;
        for path in paths {
            match self.longest_config_root_for_path(path) {
                Some(root) => {
                    let key = metadata::canonicalize_configuration_path(&root.to_string_lossy());
                    if !roots.contains(&key) {
                        roots.push(key);
                    }
                }
                None => bump_global = true,
            }
        }
        for key in &roots {
            self.bump_config_revision(key);
        }
        if bump_global {
            let input = self.global_config_revision_input();
            let current = input.revision(self);
            input.set_revision(self).to(current.wrapping_add(1));
        }
    }

    /// Bump every config revision (all registered roots plus the global
    /// fallback). Used when the change is not attributable to a single root
    /// (e.g. metadata watch registration completing after the bootstrap load).
    pub fn bump_all_config_revisions(&mut self) {
        use salsa::Setter;
        let inputs: Vec<metadata::ConfigRevisionInput> =
            self.config_revisions.iter().map(|e| *e.value()).collect();
        for input in inputs {
            let current = input.revision(self);
            input.set_revision(self).to(current.wrapping_add(1));
        }
        let global = self.global_config_revision_input();
        let current = global.revision(self);
        global.set_revision(self).to(current.wrapping_add(1));
    }

    /// Set the workspace config roots without dependency topology (every
    /// extension independent — the pre-`dependsOn` semantics). At most ONE entry
    /// may carry the `None` label (the base configuration); every other entry is
    /// an extension with a `Some(name)` label. Two `None` entries would silently be
    /// treated as two bases.
    ///
    /// Zero is legal and means an extension-only project, which has no base at all.
    /// Consumers that split base from extensions must handle it: such a project is
    /// bootstrapped once its own roots are listed, and the readers that take a
    /// single side read the file's visibility chain instead of a base — see
    /// [`Self::substrate_listings`].
    pub fn set_all_config_paths(&mut self, paths: Vec<(Option<String>, std::path::PathBuf)>) {
        self.set_workspace_configs_snapshot(metadata::WorkspaceConfigsSnapshot::from_paths(paths));
    }

    /// Set the full workspace-configs snapshot (roots + dependency closures +
    /// topology fingerprint) as ONE input write, so a project reload is atomic:
    /// no query can observe new paths with old closures or vice versa.
    pub fn set_workspace_configs_snapshot(&mut self, snapshot: metadata::WorkspaceConfigsSnapshot) {
        use salsa::Setter;
        assert_eq!(
            snapshot.paths.len(),
            snapshot.canonical_paths.len(),
            "workspace-configs snapshot: canonical_paths must parallel paths"
        );
        assert_eq!(
            snapshot.paths.len(),
            snapshot.closures.len(),
            "workspace-configs snapshot: closures must parallel paths"
        );
        assert_eq!(
            snapshot.paths.len(),
            snapshot.topological_order.len(),
            "workspace-configs snapshot: topological_order must cover every path"
        );
        assert_eq!(
            snapshot.paths.len(),
            snapshot.kinds.len(),
            "workspace-configs snapshot: kinds must parallel paths"
        );
        assert!(
            snapshot.closures.iter().flatten().all(|&dep| dep < snapshot.paths.len()),
            "workspace-configs snapshot: closure indices must point into paths"
        );
        assert!(
            snapshot.topological_order.iter().all(|&idx| idx < snapshot.paths.len()),
            "workspace-configs snapshot: topological_order indices must point into paths"
        );
        let mut unique_order = snapshot.topological_order.clone();
        unique_order.sort_unstable();
        unique_order.dedup();
        assert_eq!(
            unique_order.len(),
            snapshot.paths.len(),
            "workspace-configs snapshot: topological_order must not repeat paths"
        );
        for (_, path) in &snapshot.paths {
            self.ensure_config_revision_input(&path.to_string_lossy());
        }
        let input = self.workspace_configs();
        input.set_snapshot(self).to(Arc::new(snapshot));
    }

    pub fn workspace_configs_snapshot(&self) -> Arc<metadata::WorkspaceConfigsSnapshot> {
        self.workspace_configs().snapshot(self)
    }

    pub fn all_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)> {
        self.workspace_configs().snapshot(self).paths.clone()
    }

    /// The base and its extensions in topological order — the designer's view,
    /// with external objects left out. See [`metadata::WorkspaceConfigsSnapshot::designer_order`].
    pub fn designer_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)> {
        let snapshot = self.workspace_configs_snapshot();
        snapshot.designer_order().map(|idx| snapshot.paths[idx].clone()).collect()
    }

    pub fn config_root_rank_and_label(&self, file_id: FileId) -> Option<(usize, Option<String>)> {
        let file_path = vfs_helpers::get_file_path(self, file_id)?;
        self.config_root_rank_for_path(&file_path)
    }

    /// The same ranking, addressed by PATH.
    ///
    /// A caller that already holds the path must not go back through the file id:
    /// resolving a path from an id needs the global file→root mapping, which
    /// PANICS for a file the database was never told about — and the graph builder
    /// runs against per-batch databases that legitimately know only their own
    /// slice. See `CommonModuleCandidates::reflagged` for the same asymmetry.
    pub fn config_root_rank_for_path(
        &self,
        file_path: &std::path::Path,
    ) -> Option<(usize, Option<String>)> {
        let snapshot = self.workspace_configs_snapshot();
        snapshot
            .topological_order
            .iter()
            .enumerate()
            .filter_map(|(rank, &idx)| {
                let configured = &snapshot.paths[idx].1;
                let canonical = &snapshot.canonical_paths[idx];
                let matched_len = [configured, canonical]
                    .into_iter()
                    .filter(|root| file_path.starts_with(root))
                    .map(|root| root.components().count())
                    .max()?;
                Some((matched_len, rank, snapshot.paths[idx].0.clone()))
            })
            .max_by_key(|(matched_len, _, _)| *matched_len)
            .map(|(_, rank, label)| (rank, label))
    }

    pub fn visible_config_root_ranks(&self, file_id: FileId) -> Option<Vec<usize>> {
        let roots = self.visible_roots_for_file(file_id)?;
        let snapshot = self.workspace_configs_snapshot();
        let visible_paths = roots.main.iter().chain(roots.chain.iter().map(|(_, path)| path));
        let visible_paths: HashSet<&PathBuf> = visible_paths.collect();
        Some(
            snapshot
                .topological_order
                .iter()
                .enumerate()
                .filter_map(|(rank, &idx)| {
                    visible_paths.contains(&snapshot.paths[idx].1).then_some(rank)
                })
                .collect(),
        )
    }

    /// Set (or update) one config root's metadata structure listing. Must run
    /// outside any tracked query. On reload it updates the existing input in place
    /// so previously recorded `config_index`/`resolve_metadata_object` dependencies
    /// stay valid and re-derive from the new structure.
    pub fn set_metadata_listing(&mut self, root: &str, listing: metadata::MetadataListingData) {
        use salsa::Setter;
        let key = metadata::canonicalize_configuration_path(root);
        let metadata::MetadataListingData {
            entries,
            defined_types,
            common_modules,
            event_subscriptions,
            scheduled_jobs,
            roles,
            http_services,
            web_services,
            integration_services,
            subsystems,
            common_attributes,
        } = listing;
        let entries = Arc::new(entries);
        let defined_types = Arc::new(defined_types);
        let common_modules = Arc::new(common_modules);
        let event_subscriptions = Arc::new(event_subscriptions);
        let scheduled_jobs = Arc::new(scheduled_jobs);
        let roles = Arc::new(roles);
        let http_services = Arc::new(http_services);
        let web_services = Arc::new(web_services);
        let integration_services = Arc::new(integration_services);
        let subsystems = Arc::new(subsystems);
        let common_attributes = Arc::new(common_attributes);
        match self.metadata_listings.get(&key).map(|e| *e.value()) {
            Some(input) => {
                input.set_entries(self).to(entries);
                input.set_defined_types(self).to(defined_types);
                input.set_common_modules(self).to(common_modules);
                input.set_event_subscriptions(self).to(event_subscriptions);
                input.set_scheduled_jobs(self).to(scheduled_jobs);
                input.set_roles(self).to(roles);
                input.set_http_services(self).to(http_services);
                input.set_web_services(self).to(web_services);
                input.set_integration_services(self).to(integration_services);
                input.set_subsystems(self).to(subsystems);
                input.set_common_attributes(self).to(common_attributes);
            }
            None => {
                let input = metadata::MetadataListingInput::builder(
                    entries,
                    defined_types,
                    common_modules,
                    event_subscriptions,
                    scheduled_jobs,
                    roles,
                    http_services,
                    web_services,
                    integration_services,
                    subsystems,
                    common_attributes,
                )
                .durability(salsa::Durability::MEDIUM)
                .new(self);
                self.metadata_listings.insert(key, input);
            }
        }
    }

    /// The metadata structure-listing input for a config root, if one was set.
    /// Resolution consumers fold this through `resolve_metadata_object`.
    pub fn metadata_listing(&self, root: &str) -> Option<metadata::MetadataListingInput> {
        let key = metadata::canonicalize_configuration_path(root);
        self.metadata_listings.get(&key).map(|e| *e.value())
    }

    /// The ONE per-file visibility resolver: the config roots a file may see.
    /// `chain` lists the file's transitive extension dependencies in stable
    /// topological order with the file's own extension LAST — so forward overlay
    /// composition applies dependencies before dependents, and reverse iteration
    /// gives replacement/name-lookup precedence (own first). A base-config file
    /// gets an empty chain; without declared dependencies the chain is exactly
    /// `[own]`, the pre-dependency semantics. `None` when no config roots are
    /// registered or the file's path is unknown.
    pub fn visible_roots_for_file(&self, file_id: FileId) -> Option<VisibleRoots> {
        let file_path = vfs_helpers::get_file_path(self, file_id)?;
        let snapshot = RootDatabaseImpl::workspace_configs_snapshot(self);
        if snapshot.paths.is_empty() {
            return None;
        }

        let main =
            snapshot.paths.iter().find_map(|(label, path)| label.is_none().then(|| path.clone()));
        // Longest-prefix match against BOTH the configured and the canonical
        // root spelling: enumerated file paths may arrive canonicalized (MCP)
        // while the configured root is symlinked, or vice versa. Ranking uses
        // the length of the spelling that actually MATCHED — ranking by the
        // configured spelling when only the canonical one matched could pick a
        // less-specific root for nested symlinked extensions.
        let own = snapshot
            .paths
            .iter()
            .enumerate()
            .filter_map(|(idx, (label, path))| {
                label.as_ref()?;
                let mut matched_len: Option<usize> = None;
                if file_path.starts_with(path) {
                    matched_len = Some(path.as_os_str().len());
                }
                let canonical = &snapshot.canonical_paths[idx];
                if file_path.starts_with(canonical) {
                    let len = canonical.as_os_str().len();
                    matched_len = Some(matched_len.map_or(len, |best| best.max(len)));
                }
                matched_len.map(|len| (idx, len))
            })
            .max_by_key(|&(_, len)| len)
            .map(|(idx, _)| idx);

        let chain = own
            .map(|own_idx| {
                let entry = |idx: usize| {
                    let (label, path) = &snapshot.paths[idx];
                    (label.clone().unwrap_or_default(), path.clone())
                };
                let mut chain: Vec<(String, PathBuf)> =
                    snapshot.closures[own_idx].iter().map(|&dep| entry(dep)).collect();
                chain.push(entry(own_idx));
                chain
            })
            .unwrap_or_default();

        Some(VisibleRoots { main, chain })
    }

    /// For `file_id`: the structure listing of the main config root and the
    /// listings of the file's visibility chain (dependencies first, own extension
    /// last), plus whether the per-MDO substrate is populated (the bootstrap ran
    /// for the roots this resolution touches). `None` if the file has no config
    /// root. Shared by the per-file resolvers so they pick the same roots and
    /// make the same bootstrapped-vs-fallback decision.
    fn metadata_listings_for_file(
        &self,
        file_id: FileId,
    ) -> Option<(
        Option<metadata::MetadataListingInput>,
        Vec<Option<metadata::MetadataListingInput>>,
        bool,
    )> {
        let roots = match self.visible_roots_for_file(file_id) {
            Some(roots) => roots,
            // No registered config roots (single-file / batch mode): report "not
            // bootstrapped" with empty listings so callers reach their
            // whole-config fallback, exactly as before the chain resolver.
            // `None` overall remains reserved for a file with no known path.
            None => {
                vfs_helpers::get_file_path(self, file_id)?;
                VisibleRoots { main: None, chain: Vec::new() }
            }
        };

        let main_listing = roots.main.as_ref().map(|p| self.metadata_listing(&p.to_string_lossy()));
        let chain_listings: Vec<Option<metadata::MetadataListingInput>> =
            roots.chain.iter().map(|(_, p)| self.metadata_listing(&p.to_string_lossy())).collect();
        // "Bootstrapped" means every listing this resolution needs is present.
        // Which listings those are depends on the project: one with a base needs the
        // base listed, while an extension-only project has no base to list and is
        // served entirely by the chain. Demanding a base listing from a project that
        // has none would pin it to the whole-config fallback forever.
        //
        // With no config roots registered at all (the batch / CLI path that never
        // calls `set_workspace_configs`) the chain is empty and nothing is
        // bootstrapped, so callers reach their whole-config fallback. A root present
        // but without a listing (batch/graph/tests) is likewise not bootstrapped —
        // EVERY chain root must be listed, or the per-MDO path would silently drop a
        // dependency's objects.
        let base_ready = match roots.main {
            Some(_) => matches!(main_listing, Some(Some(_))),
            None => !roots.chain.is_empty(),
        };
        let bootstrapped = base_ready && chain_listings.iter().all(|l| l.is_some());

        Some((main_listing.flatten(), chain_listings, bootstrapped))
    }

    /// The listings a per-file lookup reads once the substrate is populated: the
    /// base when the project has one, otherwise the file's own visibility chain.
    ///
    /// Lookups that compose a base with an overlay take `main` and the chain apart
    /// themselves; this is for the ones that read a single side. Reading `main`
    /// alone would answer "not found" for an extension-only project, which has no
    /// base — while the whole-config fallback answers correctly from the extension.
    fn substrate_listings(
        main_listing: Option<metadata::MetadataListingInput>,
        chain_listings: &[Option<metadata::MetadataListingInput>],
    ) -> Vec<metadata::MetadataListingInput> {
        match main_listing {
            Some(listing) => vec![listing],
            // The chain is stored dependencies-first with the file's own extension
            // last, but replacement runs the other way: the extension that owns the
            // file wins over the one it borrows from. Reading it forward would hand
            // back a dependency's object and quietly disagree with the whole-config
            // path, which composes the overlay.
            None => chain_listings.iter().rev().flatten().copied().collect(),
        }
    }

    /// Event subscriptions do not overlay another object's metadata: declarations
    /// in the base and every visible extension remain independently active. Keep
    /// the base first so a duplicate name retains the established base-first lookup.
    fn visible_event_subscription_listings(
        main_listing: Option<metadata::MetadataListingInput>,
        chain_listings: &[Option<metadata::MetadataListingInput>],
    ) -> Vec<metadata::MetadataListingInput> {
        main_listing.into_iter().chain(chain_listings.iter().flatten().copied()).collect()
    }

    /// Names collected from several listings, deduplicated the way
    /// [`Self::subsystem_names_for_file`] does it: one chain can declare the same
    /// object in a dependency and in the extension that borrows it, and a list shown
    /// to the user must carry it once. Case-insensitive, because BSL names are.
    fn dedup_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut seen = HashSet::new();
        names.into_iter().filter(|name| seen.insert(name.fold_lower())).collect()
    }

    /// Resolve a single metadata object visible to `file_id` at per-MDO Salsa
    /// granularity, composing the main config with the file's applicable extension
    /// via [`MetadataObject::apply_extension_overlay`] (or whichever side alone
    /// exists) — exactly as `merged_visible_configuration` does per object.
    ///
    /// Replaces a `merged_visible_configuration().find_metadata_object` lookup for
    /// the migrated consumers. When the per-MDO substrate is populated (the LSP
    /// bootstrap ran), it resolves through the per-MDO queries so the caller depends
    /// on only that MDO — editing an unrelated MDO does not invalidate it. When the
    /// substrate is absent (batch analysis, the graph build's per-batch DBs, tests),
    /// it falls back to the whole-config merged lookup: identical result, no
    /// narrowing. Returns `None` when the file has no registered config root.
    pub fn resolve_metadata_object_for_file(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_metadata_object(mdo_type, name)
                .cloned()
                .map(Arc::new);
        }

        let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
            listing.and_then(|l| {
                metadata::resolve_metadata_object(self, l, mdo_type, name.to_string())
            })
        };
        // Forward overlay composition: the base result first, then each chain
        // root's overlay in dependency order, the file's own extension last.
        let hits = std::iter::once(resolve_in(main_listing))
            .chain(chain_listings.into_iter().map(resolve_in))
            .flatten();
        let mut merged: Option<bsl_metadata::MetadataObject> = None;
        for overlay in hits {
            match &mut merged {
                Some(base) if overlay.adopts(base) => base.apply_extension_overlay(&overlay),
                Some(base) => *base = (*overlay).clone(),
                None => merged = Some((*overlay).clone()),
            }
        }
        merged.map(Arc::new)
    }

    /// The register counterpart of [`resolve_metadata_object_for_file`]: resolve a
    /// single register visible to `file_id`, composing main + the file's extension
    /// via [`bsl_metadata::Register::apply_extension_overlay`]. Per-MDO when the
    /// substrate is populated, falling back to
    /// `merged_visible_configuration().find_register_by_type_and_name` otherwise.
    pub fn resolve_register_for_file(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_register_by_type_and_name(mdo_type, name)
                .cloned()
                .map(Arc::new);
        }

        let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
            listing.and_then(|l| metadata::resolve_register(self, l, mdo_type, name.to_string()))
        };
        let mut hits = std::iter::once(resolve_in(main_listing))
            .chain(chain_listings.into_iter().map(resolve_in))
            .flatten();
        let first = hits.next()?;
        let mut merged: Option<bsl_metadata::Register> = None;
        for overlay in hits {
            merged.get_or_insert_with(|| (*first).clone()).apply_extension_overlay(&overlay);
        }
        Some(merged.map(Arc::new).unwrap_or(first))
    }

    /// Resolve a register visible to `file_id` by NAME alone (its kind unknown to
    /// the caller). The by-name counterpart of [`resolve_register_for_file`], with
    /// the same main + file's-extension overlay and per-MDO-or-fallback split.
    pub fn resolve_register_by_name_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_register(name)
                .cloned()
                .map(Arc::new);
        }

        let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
            listing.and_then(|l| metadata::resolve_register_by_name(self, l, name.to_string()))
        };
        // Same name along the chain: the WINNING kind is the last (most
        // dependent) hit's kind — a same-name register of a different kind is
        // not a borrow, so the later root's replaces outright, mirroring
        // `merge_extension_overlay`. All hits OF the winning kind then merge in
        // chain order, so a base register still contributes its fields when an
        // intervening dependency declared an unrelated kind under the name.
        let hits: Vec<Arc<bsl_metadata::Register>> = std::iter::once(resolve_in(main_listing))
            .chain(chain_listings.into_iter().map(resolve_in))
            .flatten()
            .collect();
        let winning_kind = hits.last()?.mdo_type();
        let mut same_kind = hits.iter().filter(|hit| hit.mdo_type() == winning_kind);
        let first = same_kind.next()?.clone();
        let mut merged: Option<bsl_metadata::Register> = None;
        for overlay in same_kind {
            merged.get_or_insert_with(|| (*first).clone()).apply_extension_overlay(overlay);
        }
        Some(merged.map(Arc::new).unwrap_or(first))
    }

    /// The defined-type counterpart of [`resolve_metadata_object_for_file`]:
    /// resolve a defined type's underlying type visible to `file_id`. A defined
    /// type's overlay replaces the underlying type wholesale, so the file's
    /// applicable extension wins outright over the base (no field merge). Per-
    /// defined-type when the substrate is populated, falling back to
    /// `merged_visible_configuration().resolve_defined_type` otherwise.
    pub fn resolve_defined_type_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<bsl_metadata::AttributeType> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use bsl_metadata::MetadataResolver;
            use hir::ConfigsDatabase;
            return self.merged_visible_configuration(file_id)?.resolve_defined_type(name);
        }

        let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
            listing.and_then(|l| metadata::resolve_defined_type(self, l, name.to_string()))
        };
        // Replacement semantics: own extension first, then dependencies from the
        // closest to the base, the base last.
        chain_listings
            .into_iter()
            .rev()
            .find_map(resolve_in)
            .or_else(|| resolve_in(main_listing))
            .map(|underlying| (*underlying).clone())
    }

    /// The common-module counterpart of [`resolve_metadata_object_for_file`]:
    /// resolve a common module's metadata by name visible to `file_id` — the base
    /// config overlaid by the file's visibility chain in order, the own extension
    /// last ([`bsl_metadata::CommonModule::apply_extension_overlay`]: a property
    /// absent in a borrowed module is inherited). A main-config common module is
    /// visible everywhere; an extension's common module is visible within that
    /// extension and its dependents (an unrelated extension's modules are not),
    /// the same scoping as metadata objects.
    /// Per-common-module when the substrate is populated, falling back to a
    /// per-config scan otherwise — `merge_extension_overlay` does not fold common
    /// modules into the merged configuration, so the fallback cannot go through
    /// `merged_visible_configuration`.
    pub fn resolve_common_module_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if bootstrapped {
            let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
                listing.and_then(|l| metadata::resolve_common_module(self, l, name.to_string()))
            };
            return overlay_common_modules(
                std::iter::once(main_listing).chain(chain_listings).map(resolve_in),
            );
        }

        let find_in = |root: &std::path::Path| -> Option<Arc<bsl_metadata::CommonModule>> {
            let path_input = metadata::intern_configuration_path(
                self,
                &root.to_string_lossy(),
                self.config_root_revision_for_path(root),
            );
            self.load_configuration(path_input).find_common_module(name).cloned().map(Arc::new)
        };

        let Some(roots) = self.visible_roots_for_file(file_id) else {
            let file_path = vfs_helpers::get_file_path(self, file_id)?;
            let config_root = vfs_helpers::find_configuration_root(self, &file_path)?;
            return find_in(&config_root);
        };

        overlay_common_modules(
            roots
                .main
                .iter()
                .map(|p| find_in(p))
                .chain(roots.chain.iter().map(|(_, p)| find_in(p))),
        )
    }

    pub fn resolve_http_service_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::HTTPService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_http_service(name)
                .cloned()
                .map(Arc::new);
        }

        Self::substrate_listings(main_listing, &chain_listings)
            .into_iter()
            .find_map(|listing| metadata::resolve_http_service(self, listing, name.to_string()))
    }

    pub fn resolve_web_service_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::WebService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_web_service(name)
                .cloned()
                .map(Arc::new);
        }

        Self::substrate_listings(main_listing, &chain_listings)
            .into_iter()
            .find_map(|listing| metadata::resolve_web_service(self, listing, name.to_string()))
    }

    pub fn http_service_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)
                .map(|config| {
                    config
                        .http_services()
                        .iter()
                        .map(|service| service.name().to_string())
                        .collect()
                })
                .unwrap_or_default();
        }

        Self::dedup_names(
            Self::substrate_listings(main_listing, &chain_listings).into_iter().flat_map(
                |listing| {
                    listing
                        .http_services(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                },
            ),
        )
    }

    pub fn web_service_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)
                .map(|config| {
                    config.web_services().iter().map(|service| service.name().to_string()).collect()
                })
                .unwrap_or_default();
        }

        Self::dedup_names(
            Self::substrate_listings(main_listing, &chain_listings).into_iter().flat_map(
                |listing| {
                    listing
                        .web_services(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                },
            ),
        )
    }

    pub fn resolve_integration_service_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::IntegrationService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_integration_service(name)
                .cloned()
                .map(Arc::new);
        }

        Self::substrate_listings(main_listing, &chain_listings).into_iter().find_map(|listing| {
            metadata::resolve_integration_service(self, listing, name.to_string())
        })
    }

    /// The event-subscription counterpart of [`resolve_common_module_for_file`]:
    /// resolve a subscription by name visible to `file_id`. Event subscriptions are
    /// flat one-file metadata. `Configuration::merge_extension_overlay` does not
    /// merge subscriptions today, so the bootstrapped path intentionally resolves
    /// from the base listing, or from the file's own visibility chain when the
    /// project has no base, to match the merged whole-config lookup.
    pub fn resolve_event_subscription_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::EventSubscription>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            return self
                .get_all_configurations(file_id)
                .into_iter()
                .find_map(|(_, config)| config.find_event_subscription(name).cloned())
                .map(Arc::new);
        }

        Self::visible_event_subscription_listings(main_listing, &chain_listings)
            .into_iter()
            .find_map(|listing| {
                metadata::resolve_event_subscription(self, listing, name.to_string())
            })
    }

    pub fn event_subscription_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            return Self::dedup_names(self.get_all_configurations(file_id).into_iter().flat_map(
                |(_, config)| {
                    config
                        .event_subscriptions()
                        .iter()
                        .map(|subscription| subscription.name().to_string())
                        .collect::<Vec<_>>()
                },
            ));
        }

        Self::dedup_names(
            Self::visible_event_subscription_listings(main_listing, &chain_listings)
                .into_iter()
                .flat_map(|listing| {
                    listing
                        .event_subscriptions(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                }),
        )
    }

    /// EventSubscription names declared by the main configuration only. Kept
    /// separate from the visible enumeration because older diagnostics explicitly
    /// inspect main-owned declarations while UnusedParameters needs the full chain.
    ///
    /// A project whose only configuration is an extension has no main to inspect:
    /// there the file's own visibility chain stands in for the main scope, or the
    /// subscription diagnostics of that project would see nothing at all
    /// (github#172). A registered main root whose substrate is absent (batch/CLI)
    /// is not that case: its whole-config lookup below stays main-only.
    pub fn main_event_subscription_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, _, bootstrapped)) = self.metadata_listings_for_file(file_id) else {
            return Vec::new();
        };

        // `main_listing` is None both when the project has no main root at all
        // (the extension-only case) and when a main root is registered but the
        // substrate was not bootstrapped. Only the former stands the chain in
        // for the missing main; the latter keeps the main-only lookup below.
        if main_listing.is_none()
            && self.visible_roots_for_file(file_id).is_some_and(|roots| roots.main.is_none())
        {
            return self.event_subscription_names_for_file(file_id);
        }

        if bootstrapped {
            return main_listing
                .map(|listing| {
                    listing
                        .event_subscriptions(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect()
                })
                .unwrap_or_default();
        }

        self.get_all_configurations(file_id)
            .into_iter()
            .find(|(name, _)| name.is_none())
            .map(|(_, config)| {
                config.event_subscriptions().iter().map(|entry| entry.name().to_string()).collect()
            })
            .unwrap_or_default()
    }

    pub fn enumerate_event_subscriptions_for_file(
        &self,
        file_id: FileId,
    ) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        let paths = RootDatabaseImpl::all_config_paths(self);
        if !paths.is_empty() {
            let mut listings = Vec::with_capacity(paths.len());
            for (_, path) in &paths {
                let Some(listing) = self.metadata_listing(&path.to_string_lossy()) else {
                    listings.clear();
                    break;
                };
                listings.push(listing);
            }
            if !listings.is_empty() {
                let mut out = Vec::new();
                for listing in listings {
                    let names: Vec<String> = listing
                        .event_subscriptions(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect();
                    for name in names {
                        if let Some(subscription) =
                            metadata::resolve_event_subscription(self, listing, name)
                        {
                            out.push(subscription);
                        }
                    }
                }
                return out;
            }
        }

        // With no configured roots (single-file mode) the inventory is empty;
        // fall back to the file's own discovered configuration like the per-file
        // resolvers do.
        let inventory = RootDatabase::all_configurations_inventory(self);
        let configs: Vec<Arc<bsl_metadata::Configuration>> = if inventory.is_empty() {
            self.get_configuration(file_id).into_iter().collect()
        } else {
            inventory.into_iter().map(|(_, config)| config).collect()
        };
        configs
            .into_iter()
            .flat_map(|config| {
                config.event_subscriptions().iter().cloned().map(Arc::new).collect::<Vec<_>>()
            })
            .collect()
    }

    /// Resolve the scheduled job `name` visible to `file_id` at per-scheduled-job
    /// granularity. Scheduled jobs are flat one-file metadata. The scheduled-job
    /// counterpart of [`resolve_event_subscription_for_file`]; the bootstrapped
    /// path reads the base listing, or the file's own visibility chain when the
    /// project has no base, to match the merged whole-config lookup.
    pub fn resolve_scheduled_job_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::ScheduledJob>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_scheduled_job(name)
                .cloned()
                .map(Arc::new);
        }

        Self::substrate_listings(main_listing, &chain_listings)
            .into_iter()
            .find_map(|listing| metadata::resolve_scheduled_job(self, listing, name.to_string()))
    }

    pub fn scheduled_job_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)
                .map(|config| {
                    config.scheduled_jobs().iter().map(|job| job.name().to_string()).collect()
                })
                .unwrap_or_default();
        }

        Self::dedup_names(
            Self::substrate_listings(main_listing, &chain_listings).into_iter().flat_map(
                |listing| {
                    listing
                        .scheduled_jobs(self)
                        .iter()
                        .map(|entry| entry.name.clone())
                        .collect::<Vec<_>>()
                },
            ),
        )
    }

    pub fn resolve_role_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Role>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .find_role(name)
                .cloned()
                .map(Arc::new);
        }

        Self::substrate_listings(main_listing, &chain_listings)
            .into_iter()
            .find_map(|listing| metadata::resolve_role(self, listing, name.to_string()))
    }

    pub fn role_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)
                .map(|config| config.roles().iter().map(|role| role.name().to_string()).collect())
                .unwrap_or_default();
        }

        Self::dedup_names(
            Self::substrate_listings(main_listing, &chain_listings).into_iter().flat_map(
                |listing| {
                    listing.roles(self).iter().map(|entry| entry.name.clone()).collect::<Vec<_>>()
                },
            ),
        )
    }

    pub fn enumerate_roles_for_file(&self, file_id: FileId) -> Vec<Arc<bsl_metadata::Role>> {
        let paths = RootDatabaseImpl::all_config_paths(self);
        if !paths.is_empty() {
            let mut listings = Vec::with_capacity(paths.len());
            for (_, path) in &paths {
                let Some(listing) = self.metadata_listing(&path.to_string_lossy()) else {
                    listings.clear();
                    break;
                };
                listings.push(listing);
            }
            if !listings.is_empty() {
                let mut out = Vec::new();
                for listing in listings {
                    let names: Vec<String> =
                        listing.roles(self).iter().map(|entry| entry.name.clone()).collect();
                    for name in names {
                        if let Some(role) = metadata::resolve_role(self, listing, name) {
                            out.push(role);
                        }
                    }
                }
                return out;
            }
        }

        // With no configured roots (single-file mode) the inventory is empty;
        // fall back to the file's own discovered configuration like the per-file
        // resolvers do.
        let inventory = RootDatabase::all_configurations_inventory(self);
        let configs: Vec<Arc<bsl_metadata::Configuration>> = if inventory.is_empty() {
            self.get_configuration(file_id).into_iter().collect()
        } else {
            inventory.into_iter().map(|(_, config)| config).collect()
        };
        configs
            .into_iter()
            .flat_map(|config| config.roles().iter().cloned().map(Arc::new).collect::<Vec<_>>())
            .collect()
    }

    /// Resolve the subsystem `name` visible to `file_id` at per-subsystem Salsa
    /// granularity. The subsystem counterpart of
    /// [`resolve_scheduled_job_for_file`]; the bootstrapped path composes the main
    /// listing with the file's own extension listing, merging a same-name
    /// extension into the base via [`bsl_metadata::Subsystem::merge_from`] and
    /// resolving an extension-only subsystem from the extension listing. When the
    /// substrate is not bootstrapped, falls back to the merged whole-config
    /// subsystem lookup.
    pub fn resolve_subsystem_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Subsystem>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(file_id)?;

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)?
                .subsystems()
                .iter()
                .find(|s| s.name().fold_lower() == name.fold_lower())
                .cloned()
                .map(Arc::new);
        }

        let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
            listing.and_then(|l| metadata::resolve_subsystem(self, l, name.to_string()))
        };
        let mut hits = std::iter::once(resolve_in(main_listing))
            .chain(chain_listings.into_iter().map(resolve_in))
            .flatten();
        let first = hits.next()?;
        let mut merged: Option<bsl_metadata::Subsystem> = None;
        for overlay in hits {
            merged.get_or_insert_with(|| (*first).clone()).merge_from(&overlay);
        }
        Some(merged.map(Arc::new).unwrap_or(first))
    }

    pub fn subsystem_names_for_file(&self, file_id: FileId) -> Vec<String> {
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return Vec::new();
        };

        if !bootstrapped {
            use hir::ConfigsDatabase;
            return self
                .merged_visible_configuration(file_id)
                .map(|config| {
                    config.subsystems().iter().map(|sub| sub.name().to_string()).collect()
                })
                .unwrap_or_default();
        }

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for listing in std::iter::once(main_listing).chain(chain_listings).flatten() {
            for entry in listing.subsystems(self).iter() {
                let name = entry.name.clone();
                if seen.insert(name.fold_lower()) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Explicit project/config enumeration for graph-style consumers that need
    /// every listed subsystem, not just a single hot lookup. The subsystem
    /// counterpart of [`enumerate_roles_for_file`]: prefers all configured
    /// listings when all are bootstrapped, merging same-name subsystems
    /// deterministically (base first, then extension order); falls back to
    /// [`RootDatabase::all_configurations_inventory`] only behind this enumeration API.
    pub fn enumerate_subsystems_for_file(
        &self,
        file_id: FileId,
    ) -> Vec<Arc<bsl_metadata::Subsystem>> {
        let paths = RootDatabaseImpl::all_config_paths(self);
        if !paths.is_empty() {
            let mut listings = Vec::with_capacity(paths.len());
            for (_, path) in &paths {
                let Some(listing) = self.metadata_listing(&path.to_string_lossy()) else {
                    listings.clear();
                    break;
                };
                listings.push(listing);
            }
            if !listings.is_empty() {
                let mut acc: std::collections::HashMap<String, Arc<bsl_metadata::Subsystem>> =
                    std::collections::HashMap::new();
                let mut order: Vec<String> = Vec::new();
                for listing in listings {
                    let names: Vec<String> =
                        listing.subsystems(self).iter().map(|entry| entry.name.clone()).collect();
                    for name in names {
                        if let Some(sub) = metadata::resolve_subsystem(self, listing, name.clone())
                        {
                            let key = name.fold_lower();
                            match acc.get_mut(&key) {
                                Some(existing) => {
                                    let mut merged = (**existing).clone();
                                    merged.merge_from(&sub);
                                    *existing = Arc::new(merged);
                                }
                                None => {
                                    order.push(key.clone());
                                    acc.insert(key, sub);
                                }
                            }
                        }
                    }
                }
                return order.into_iter().filter_map(|key| acc.remove(&key)).collect();
            }
        }

        // With no configured roots (single-file mode) the inventory is empty;
        // fall back to the file's own discovered configuration like the per-file
        // resolvers do.
        let inventory = RootDatabase::all_configurations_inventory(self);
        let configs: Vec<Arc<bsl_metadata::Configuration>> = if inventory.is_empty() {
            self.get_configuration(file_id).into_iter().collect()
        } else {
            inventory.into_iter().map(|(_, config)| config).collect()
        };
        configs
            .into_iter()
            .flat_map(|config| {
                config.subsystems().iter().cloned().map(Arc::new).collect::<Vec<_>>()
            })
            .collect()
    }

    /// Config-root path strings in visibility precedence: the base configuration first,
    /// then each extension, then every external object. Order is load-bearing for the
    /// `*_across_roots` resolvers — the base is an object's authoritative definition,
    /// extensions overlay it, and an external root holds only its own kind.
    fn ordered_config_roots(&self) -> Vec<String> {
        let snapshot = self.workspace_configs_snapshot();
        let paths = &snapshot.paths;
        debug_assert!(
            paths.iter().filter(|(label, _)| label.is_none()).count() <= 1,
            "all_config_paths must carry at most one None-labelled base root",
        );
        snapshot.inventory_order().map(|idx| paths[idx].1.to_string_lossy().into_owned()).collect()
    }

    /// [`Self::ordered_config_roots`] without the external objects: the designer's
    /// view, for a resolver whose language names none of them.
    fn designer_config_roots(&self) -> Vec<String> {
        let snapshot = self.workspace_configs_snapshot();
        snapshot
            .designer_order()
            .map(|idx| snapshot.paths[idx].1.to_string_lossy().into_owned())
            .collect()
    }

    fn effective_metadata_members_from_roots(
        &self,
        roots: impl IntoIterator<Item = (Option<String>, std::path::PathBuf)>,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<Vec<EffectiveMetadataMember>>> {
        let mut found_object = false;
        let mut members: Vec<EffectiveMetadataMember> = Vec::new();

        for (source_extension, root) in roots {
            let Some(listing) = self.metadata_listing(&root.to_string_lossy()) else { continue };
            let Some(object) =
                metadata::resolve_metadata_object(self, listing, mdo_type, name.to_string())
            else {
                continue;
            };
            found_object = true;

            let overlay = object
                .attributes
                .iter()
                .cloned()
                .map(EffectiveMetadataMemberValue::Attribute)
                .chain(
                    object
                        .tabular_sections
                        .iter()
                        .cloned()
                        .map(EffectiveMetadataMemberValue::TabularSection),
                );
            for member in overlay {
                members.retain(|existing| {
                    existing.member.name().fold_lower() != member.name().fold_lower()
                });
                members.push(EffectiveMetadataMember {
                    member,
                    source_extension: source_extension.clone(),
                });
            }
        }

        found_object.then(|| Arc::new(members))
    }

    /// Effective top-level object members in the workspace-wide view. The base
    /// is composed first, followed by every loaded extension in the workspace's
    /// stable topological order, then the external objects, which only ever
    /// answer for their own kinds. An unread or absent root contributes nothing.
    pub fn effective_metadata_members_across_roots(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<Vec<EffectiveMetadataMember>>> {
        let snapshot = self.workspace_configs_snapshot();
        let roots = snapshot.inventory_order().map(|idx| snapshot.paths[idx].clone());
        self.effective_metadata_members_from_roots(roots, mdo_type, name)
    }

    /// Effective top-level object members visible from one file: base,
    /// transitive dependencies, then the file's own extension.
    pub fn effective_metadata_members_for_file(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<Vec<EffectiveMetadataMember>>> {
        let roots = self.visible_roots_for_file(file_id)?;
        let ordered = roots
            .main
            .into_iter()
            .map(|path| (None, path))
            .chain(roots.chain.into_iter().map(|(label, path)| (Some(label), path)));
        self.effective_metadata_members_from_roots(ordered, mdo_type, name)
    }

    /// Resolve a metadata object visible anywhere in the configuration — base plus every
    /// extension — composing base with each extension's overlay via
    /// [`bsl_metadata::MetadataObject::apply_extension_overlay`]. The root-scoped
    /// counterpart of [`Self::resolve_metadata_object_for_file`] for a consumer that has
    /// NO file anchor (the MCP `metadata object` tool): visibility is the whole
    /// workspace, not one file's root. Deliberately wider than a base-only read —
    /// an object defined only in an extension is found, and an external object is
    /// found under its own kind.
    pub fn resolve_metadata_object_across_roots(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        self.resolve_metadata_object_over(self.ordered_config_roots(), mdo_type, name)
    }

    /// [`Self::resolve_metadata_object_across_roots`] over the designer's view alone —
    /// what a query resolver composes over, since SDBL names no external object.
    pub fn resolve_metadata_object_across_designer_roots(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        self.resolve_metadata_object_over(self.designer_config_roots(), mdo_type, name)
    }

    fn resolve_metadata_object_over(
        &self,
        roots: Vec<String>,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        let mut acc: Option<bsl_metadata::MetadataObject> = None;
        for root in roots {
            let Some(listing) = self.metadata_listing(&root) else { continue };
            if let Some(found) =
                metadata::resolve_metadata_object(self, listing, mdo_type, name.to_string())
            {
                match &mut acc {
                    None => acc = Some((*found).clone()),
                    Some(base) if found.adopts(base) => base.apply_extension_overlay(&found),
                    Some(base) => *base = (*found).clone(),
                }
            }
        }
        acc.map(Arc::new)
    }

    /// The register counterpart of [`Self::resolve_metadata_object_across_roots`]:
    /// resolve a register by kind + name across base and every extension, folding each
    /// extension's overlay via [`bsl_metadata::Register::apply_extension_overlay`].
    pub fn resolve_register_across_roots(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        self.resolve_register_over(self.ordered_config_roots(), mdo_type, name)
    }

    /// [`Self::resolve_register_across_roots`] over the designer's view alone.
    pub fn resolve_register_across_designer_roots(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        self.resolve_register_over(self.designer_config_roots(), mdo_type, name)
    }

    fn resolve_register_over(
        &self,
        roots: Vec<String>,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        let mut acc: Option<bsl_metadata::Register> = None;
        for root in roots {
            let Some(listing) = self.metadata_listing(&root) else { continue };
            if let Some(found) =
                metadata::resolve_register(self, listing, mdo_type, name.to_string())
            {
                match &mut acc {
                    None => acc = Some((*found).clone()),
                    Some(base) => base.apply_extension_overlay(&found),
                }
            }
        }
        acc.map(Arc::new)
    }

    /// The defined-type counterpart of [`Self::resolve_metadata_object_across_roots`], for a
    /// consumer with no file anchor.
    ///
    /// The composition differs from its two neighbours and cannot be copied from them: a
    /// metadata object folds each extension's overlay into the base, whereas an extension
    /// **replaces** a defined type's underlying type wholesale (see
    /// [`metadata::resolve_defined_type`]). So the last root that defines the name wins
    /// outright, and the base is consulted only when no extension defines it — the same
    /// replacement semantics [`Self::resolve_defined_type_for_file`] applies along a file's
    /// visibility chain, widened here to the whole configuration.
    pub fn resolve_defined_type_across_roots(
        &self,
        name: &str,
    ) -> Option<bsl_metadata::AttributeType> {
        self.resolve_defined_type_over(self.ordered_config_roots(), name)
    }

    /// [`Self::resolve_defined_type_across_roots`] over the designer's view alone.
    pub fn resolve_defined_type_across_designer_roots(
        &self,
        name: &str,
    ) -> Option<bsl_metadata::AttributeType> {
        self.resolve_defined_type_over(self.designer_config_roots(), name)
    }

    fn resolve_defined_type_over(
        &self,
        roots: Vec<String>,
        name: &str,
    ) -> Option<bsl_metadata::AttributeType> {
        let mut found = None;
        for root in roots {
            let Some(listing) = self.metadata_listing(&root) else { continue };
            if let Some(underlying) =
                metadata::resolve_defined_type(self, listing, name.to_string())
            {
                found = Some((*underlying).clone());
            }
        }
        found
    }

    /// Resolve an event subscription by name across base + every extension. Event
    /// subscriptions carry no extension-overlay merge (mirroring
    /// [`Self::resolve_event_subscription_for_file`], which reads a single side),
    /// so this returns the first match in precedence order — base wins, an
    /// extension-only subscription is still surfaced.
    pub fn resolve_event_subscription_across_roots(
        &self,
        name: &str,
    ) -> Option<Arc<bsl_metadata::EventSubscription>> {
        self.ordered_config_roots().iter().find_map(|root| {
            let listing = self.metadata_listing(root)?;
            metadata::resolve_event_subscription(self, listing, name.to_string())
        })
    }

    /// Resolve an HTTP service by name across base + every extension (first match in
    /// precedence order; services carry no overlay merge, matching
    /// [`Self::resolve_http_service_for_file`]).
    pub fn resolve_http_service_across_roots(
        &self,
        name: &str,
    ) -> Option<Arc<bsl_metadata::HTTPService>> {
        self.ordered_config_roots().iter().find_map(|root| {
            let listing = self.metadata_listing(root)?;
            metadata::resolve_http_service(self, listing, name.to_string())
        })
    }

    /// Resolve a Web service by name across base + every extension (first match; no
    /// overlay merge, matching [`Self::resolve_web_service_for_file`]).
    pub fn resolve_web_service_across_roots(
        &self,
        name: &str,
    ) -> Option<Arc<bsl_metadata::WebService>> {
        self.ordered_config_roots().iter().find_map(|root| {
            let listing = self.metadata_listing(root)?;
            metadata::resolve_web_service(self, listing, name.to_string())
        })
    }

    /// Resolve an integration service by name across base + every extension (first
    /// match; no overlay merge, matching [`Self::resolve_integration_service_for_file`]).
    pub fn resolve_integration_service_across_roots(
        &self,
        name: &str,
    ) -> Option<Arc<bsl_metadata::IntegrationService>> {
        self.ordered_config_roots().iter().find_map(|root| {
            let listing = self.metadata_listing(root)?;
            metadata::resolve_integration_service(self, listing, name.to_string())
        })
    }

    /// The whole [`bsl_metadata::Configuration`] for one config root, via the cached
    /// `load_configuration` Salsa query (Channel-2). This IS a full `load_from_directory`
    /// when it recomputes, so it is for the rare configuration-header reads
    /// (`name`/`uuid`, extension names/counts) the per-MDO listing does not carry — never
    /// the hot `object` point-lookup. Invalidated by `bump_config_for_paths`, so a
    /// metadata drift re-parses it lazily on the next header read.
    pub fn configuration_for_root(&self, root: &Path) -> Arc<bsl_metadata::Configuration> {
        let path_input = metadata::intern_configuration_path(
            self,
            &root.to_string_lossy(),
            self.config_root_revision_for_path(root),
        );
        self.load_configuration(path_input)
    }

    /// Resolve the common module that owns the `Ext/Module.bsl` whose id is
    /// `module_file_id` (typically the file currently being analysed). Answers "is
    /// this `.bsl` a common module's source, and if so which?" via the per-root
    /// reverse index when the substrate is populated, falling back to a
    /// root-relative URI scan over the visible roots' common modules otherwise;
    /// the owner's metadata is then composed by name over the roots visible to the
    /// file, as [`Self::resolve_common_module_for_file`] does.
    pub fn common_module_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        use bsl_metadata::traits::MdObject;
        let owner = self.common_module_owner_of_file(module_file_id)?;
        self.resolve_common_module_for_file(module_file_id, owner.name()).or(Some(owner))
    }

    fn common_module_owner_of_file(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(module_file_id)?;

        if bootstrapped {
            let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
                listing
                    .and_then(|l| metadata::resolve_common_module_by_file(self, l, module_file_id))
            };
            return chain_listings
                .into_iter()
                .rev()
                .find_map(resolve_in)
                .or_else(|| resolve_in(main_listing));
        }

        let file_path = vfs_helpers::get_file_path(self, module_file_id)?;
        let file_path_lower = file_path.to_string_lossy().fold_lower();
        let paths = RootDatabaseImpl::all_config_paths(self);

        let load_at = |path: &std::path::Path| -> Arc<bsl_metadata::Configuration> {
            let path_input = metadata::intern_configuration_path(
                self,
                &path.to_string_lossy(),
                self.config_root_revision_for_path(path),
            );
            self.load_configuration(path_input)
        };

        let find_in = |root: &std::path::Path| -> Option<Arc<bsl_metadata::CommonModule>> {
            let config = load_at(root);
            // `root.join(uri).fold_lower() == file_path_lower` reduces to a relative
            // lookup: strip the (lowercased) root prefix and match the module's folded
            // root-relative URI, so the O(all-modules) per-call Cyrillic re-fold is gone.
            // The separator between root and remainder is mandatory so a sibling whose
            // name merely starts with the root (`/cfg` vs `/cfgX`) is not a false match.
            let root_lower = root.to_string_lossy().fold_lower();
            let root_lower = root_lower.strip_suffix(['/', '\\']).unwrap_or(&root_lower);
            let rel = file_path_lower.strip_prefix(root_lower)?.strip_prefix(['/', '\\'])?;
            config.find_common_module_by_uri_lower(rel).cloned().map(Arc::new)
        };

        if paths.is_empty() {
            let config_root = vfs_helpers::find_configuration_root(self, &file_path)?;
            return find_in(&config_root);
        }

        let roots = self.visible_roots_for_file(module_file_id)?;
        roots
            .main
            .as_ref()
            .and_then(|p| find_in(p))
            .or_else(|| roots.chain.iter().rev().find_map(|(_, p)| find_in(p)))
    }

    pub fn http_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::HTTPService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(module_file_id)?;

        if bootstrapped {
            let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
                listing
                    .and_then(|l| metadata::resolve_http_service_by_file(self, l, module_file_id))
            };
            return chain_listings
                .into_iter()
                .rev()
                .find_map(resolve_in)
                .or_else(|| resolve_in(main_listing));
        }

        use hir::ConfigsDatabase;
        let file_path = vfs_helpers::get_file_path(self, module_file_id)?;
        self.merged_visible_configuration(module_file_id)
            .and_then(|config| metadata::find_http_service_by_path(&config, &file_path))
    }

    pub fn web_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::WebService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(module_file_id)?;

        if bootstrapped {
            let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
                listing.and_then(|l| metadata::resolve_web_service_by_file(self, l, module_file_id))
            };
            return chain_listings
                .into_iter()
                .rev()
                .find_map(resolve_in)
                .or_else(|| resolve_in(main_listing));
        }

        use hir::ConfigsDatabase;
        let file_path = vfs_helpers::get_file_path(self, module_file_id)?;
        self.merged_visible_configuration(module_file_id)
            .and_then(|config| metadata::find_web_service_by_path(&config, &file_path))
    }

    pub fn integration_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::IntegrationService>> {
        let (main_listing, chain_listings, bootstrapped) =
            self.metadata_listings_for_file(module_file_id)?;

        if bootstrapped {
            let resolve_in = |listing: Option<metadata::MetadataListingInput>| {
                listing.and_then(|l| {
                    metadata::resolve_integration_service_by_file(self, l, module_file_id)
                })
            };
            return chain_listings
                .into_iter()
                .rev()
                .find_map(resolve_in)
                .or_else(|| resolve_in(main_listing));
        }

        use hir::ConfigsDatabase;
        let file_path = vfs_helpers::get_file_path(self, module_file_id)?;
        self.merged_visible_configuration(module_file_id)
            .and_then(|config| metadata::find_integration_service_by_path(&config, &file_path))
    }

    /// The `Ext/Module.bsl` body file id(s) of the common module `name` visible to
    /// `file_id` — base + the file's own extension. A borrowed module has a base
    /// body and an extension body; both are returned so method/parameter validation
    /// sees the merged surface. The scoped counterpart of the former all-config
    /// `find_common_module_files_anywhere`: body ids come straight from the substrate
    /// when bootstrapped, otherwise from a scoped root-relative URI scan.
    /// Bodies of a metadata object's module that `file_id` may resolve against,
    /// ordered by config-root rank so the base declaration wins.
    ///
    /// The path-derived index answers by PATH order and knows nothing of root
    /// topology, so on its own it hands a caller the body of an extension the
    /// caller never declared a dependency on. Filtering happens here rather than
    /// in `hir-def` because the ranks live here.
    ///
    /// `None` — the file has no configured visibility at all, and the caller keeps
    /// the path index. `Some(empty)` — no VISIBLE root holds such a body, which is
    /// a real absence; see the trait doc for why this differs from the common
    /// module.
    pub fn resolve_mdo_module_files_for_file(
        &self,
        file_id: FileId,
        role: hir::MdoModuleRole,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<hir::CommonModuleBodies> {
        let visible_ranks = self.visible_config_root_ranks(file_id)?;

        let source_root_id = self.file_source_root_input(file_id).source_root_id(self);
        let index = <Self as hir::DefDatabase>::module_index(self, source_root_id);
        let name = hir::Name::new(name);

        let candidates: Vec<FileId> = match role {
            hir::MdoModuleRole::Manager => hir::ManagerType::from_mdo_type(mdo_type)
                .map(|manager_type| index.manager_candidates(manager_type, &name).to_vec())
                .unwrap_or_default(),
            hir::MdoModuleRole::Object => index.object_module_candidates(mdo_type, &name).to_vec(),
            hir::MdoModuleRole::RecordSet => index.record_set_candidates(mdo_type, &name).to_vec(),
        };

        // Paths come from the source root's own file set, never from the global
        // file→root mapping: that mapping panics for a file the database has not
        // been told about, and the graph builder resolves against per-batch
        // databases holding only their own slice.
        let source_root = self.source_root_input(source_root_id).root(self);
        let file_set = source_root.file_set();
        let rank_of = |file: FileId| -> Option<usize> {
            let path = file_set.path_for_file(&file)?.as_path().to_path_buf();
            self.config_root_rank_for_path(&path).map(|(rank, _)| rank)
        };

        // Only a body whose root is KNOWN and not visible is dropped. A body that
        // sits outside every configured root has no rank, and topology says
        // nothing about it — the path index hands it over today, and removing it
        // here would be a second, unasked-for change of behaviour.
        // `effective_module_exports_query` drops those instead, and can afford to:
        // it composes a surface, while this decides an answer.
        let mut files: Vec<(FileId, usize)> = candidates
            .into_iter()
            .filter_map(|file| match rank_of(file) {
                Some(rank) if !visible_ranks.contains(&rank) => None,
                Some(rank) => Some((file, rank)),
                None => Some((file, usize::MAX)),
            })
            .collect();
        files.sort_by_key(|&(_, rank)| rank);

        let mut out = hir::CommonModuleBodies::default();
        for (file, _) in files {
            out.push(file, self.file_is_unread(file));
        }
        Some(out)
    }

    pub fn resolve_common_module_files_for_file(
        &self,
        file_id: FileId,
        name: &str,
    ) -> hir::CommonModuleBodies {
        let mut out = hir::CommonModuleBodies::default();
        let Some((main_listing, chain_listings, bootstrapped)) =
            self.metadata_listings_for_file(file_id)
        else {
            return out;
        };

        if bootstrapped {
            for listing in
                chain_listings.iter().rev().cloned().chain(std::iter::once(main_listing)).flatten()
            {
                let index = metadata::common_module_index(self, listing);
                let readable = index.lookup_module_file(name);
                let unread = index.lookup_unread_module_file(name);
                if let Some(fid) = readable {
                    out.push(fid, false);
                }
                if let Some(fid) = unread {
                    out.push(fid, true);
                }
                if index.lookup(name).is_some() && readable.is_none() && unread.is_none() {
                    out.mark_missing_expected_body();
                }
            }
            return out;
        }

        use bsl_metadata::traits::Module;

        let Some(file_path) = vfs_helpers::get_file_path(self, file_id) else {
            return out;
        };
        let paths = RootDatabaseImpl::all_config_paths(self);

        let body_in = |root: &std::path::Path| -> (bool, Option<FileId>) {
            let path_input = metadata::intern_configuration_path(
                self,
                &root.to_string_lossy(),
                self.config_root_revision_for_path(root),
            );
            let config = self.load_configuration(path_input);
            let Some(module) = config.find_common_module(name) else {
                return (false, None);
            };
            let body = module.uri().and_then(|uri| {
                let vfs_path = vfs::VfsPath::new(root.join(uri).to_string_lossy().into_owned());
                self.resolve_vfs_path(SourceRootId(0), &vfs_path)
            });
            (true, body)
        };

        // The scan resolves a URI to an id without ever reading it, so readability is
        // asked here — the same question the substrate branch answers from its index.
        let admit = |out: &mut hir::CommonModuleBodies, fid: FileId| {
            out.push(fid, self.file_is_unread(fid))
        };

        if paths.is_empty() {
            if let Some(root) = vfs_helpers::find_configuration_root(self, &file_path) {
                let (declared, body) = body_in(&root);
                if let Some(fid) = body {
                    admit(&mut out, fid);
                } else if declared {
                    out.mark_missing_expected_body();
                }
            }
            return out;
        }

        let Some(roots) = self.visible_roots_for_file(file_id) else {
            return out;
        };
        for root in roots.chain.iter().rev().map(|(_, p)| p).chain(roots.main.as_ref()) {
            let (declared, body) = body_in(root);
            if let Some(fid) = body {
                admit(&mut out, fid);
            } else if declared {
                out.mark_missing_expected_body();
            }
        }
        out
    }

    /// Fixed application-module body files visible to `file_id`, in overlay
    /// composition order (base first). An empty result is complete: every visible
    /// root was examined and none contained that fixed path.
    pub fn resolve_application_module_files_for_file(
        &self,
        file_id: FileId,
        kind: hir::ApplicationModuleKind,
    ) -> Option<hir::CommonModuleBodies> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        queries::application_module_files_query(self, file_id_input, kind)
    }

    fn resolve_application_module_files_uncached_impl(
        &self,
        file_id: FileId,
        kind: hir::ApplicationModuleKind,
    ) -> Option<hir::CommonModuleBodies> {
        use base_db::SourceDatabase;

        let file_path = vfs_helpers::get_file_path(self, file_id)?;
        let roots = if RootDatabaseImpl::all_config_paths(self).is_empty() {
            vec![vfs_helpers::find_configuration_root(self, &file_path)?]
        } else {
            let visible = self.visible_roots_for_file(file_id)?;
            visible
                .main
                .into_iter()
                .chain(visible.chain.into_iter().map(|(_, path)| path))
                .collect()
        };
        let source_root_id = self.file_source_root_input(file_id).source_root_id(self);
        let source_root = self.source_root_input(source_root_id);
        let relative_path = kind.relative_path();
        let modes =
            hir::module_path_segment_modes(&relative_path.to_string_lossy()).unwrap_or_default();
        let mut bodies = hir::CommonModuleBodies::default();
        for root in roots {
            let candidate = root.join(&relative_path).to_string_lossy().into_owned();
            if let Some(found) =
                base_db::resolve_vfs_path_ci_query(self, source_root, candidate, &modes)
            {
                bodies.push(found, self.file_is_unread(found));
            }
        }
        Some(bodies)
    }

    fn features(&self) -> FeaturesInput {
        FeaturesInput::try_get(self).expect("FeaturesInput is created in RootDatabaseImpl::new")
    }

    pub fn type_narrowing_enabled(&self) -> bool {
        self.features().type_narrowing(self)
    }

    pub fn set_type_narrowing_enabled(&mut self, enabled: bool) {
        use salsa::Setter;
        let input = self.features();
        input.set_type_narrowing(self).to(enabled);
    }

    pub fn env_options(&self) -> hir::execution_env::EnvOptions {
        self.features().env_options(self)
    }

    pub fn set_env_options(&mut self, options: hir::execution_env::EnvOptions) {
        use salsa::Setter;
        let input = self.features();
        input.set_env_options(self).to(options);
    }

    pub fn target_platform_version(&self) -> Option<Arc<str>> {
        self.features().target_platform_version(self)
    }

    pub fn set_target_platform_version(&mut self, version: Option<Arc<str>>) {
        use salsa::Setter;
        let input = self.features();
        input.set_target_platform_version(self).to(version);
    }

    pub fn min_platform_version(&self) -> Option<Arc<str>> {
        self.features().min_platform_version(self)
    }

    pub fn set_min_platform_version(&mut self, version: Option<Arc<str>>) {
        use salsa::Setter;
        let input = self.features();
        input.set_min_platform_version(self).to(version);
    }

    pub fn compatibility_mode(&self) -> Option<Arc<str>> {
        self.features().compatibility_mode(self)
    }

    pub fn set_compatibility_mode(&mut self, mode: Option<Arc<str>>) {
        use salsa::Setter;
        let input = self.features();
        input.set_compatibility_mode(self).to(mode);
    }

    pub(crate) fn get_file_path(&self, file_id: FileId) -> Option<PathBuf> {
        let source_root_input = self.file_source_root_input(file_id);
        let source_root_id = source_root_input.source_root_id(self);
        let source_root_input = self.source_root_input(source_root_id);
        let source_root = source_root_input.root(self);
        let file_set = source_root.file_set();
        let vfs_path = file_set.path_for_file(&file_id)?;
        Some(PathBuf::from(vfs_path.as_path()))
    }

    /// The whole-config load key for `file_path`: the root attributed from disk,
    /// carried at the revision of the declared root the file sits under.
    ///
    /// Shared by the lazy read and the pre-pool warm-up on purpose. The two parts
    /// come from different places — the path from the filesystem walk, the
    /// revision from the declared-root prefix — so a warm-up building the key its
    /// own way would memoise under a key the readers never ask for, and warm
    /// nothing.
    pub(crate) fn configuration_input_for_path<'db>(
        &'db self,
        file_path: &Path,
    ) -> Option<metadata::ConfigurationPathInput<'db>> {
        // Two answers. The registered root is the identity every key is built on,
        // so it wins whenever both name the same directory — in either spelling:
        // the walk finds the canonical one through a symlinked root, and taking
        // that spelling would intern a second key for one configuration. The walk
        // wins only when it stops strictly INSIDE the registered root: a nested
        // configuration nothing declared still owns its files by its own markers.
        // An external object's root carries no marker, so the walk passes it by
        // and the registered root is the only answer.
        let snapshot = self.workspace_configs_snapshot();
        let registered = snapshot
            .owner_of_path(file_path)
            .map(|idx| (snapshot.paths[idx].1.clone(), snapshot.canonical_paths[idx].clone()));
        let walked = self.find_configuration_root(file_path);
        let config_root = match (registered, walked) {
            (Some((configured, canonical)), Some(walked)) => {
                let strictly_inside = |root: &Path| walked.starts_with(root) && walked != root;
                if strictly_inside(&configured) || strictly_inside(&canonical) {
                    walked
                } else {
                    configured
                }
            }
            (Some((configured, _)), None) => configured,
            (None, walked) => walked?,
        };
        Some(metadata::intern_configuration_path(
            self,
            &config_root.to_string_lossy(),
            self.config_root_revision_for_path(file_path),
        ))
    }

    pub(crate) fn find_configuration_root(&self, file_path: &Path) -> Option<PathBuf> {
        let mut current = file_path.parent()?;

        loop {
            let common_modules = current.join("CommonModules");
            if common_modules.is_dir() {
                tracing::debug!(?current, "Found configuration root via CommonModules/");
                return Some(current.to_path_buf());
            }

            let config_xml = bsl_conventions::find_child_ci(
                current,
                bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
            )
            .filter(|p| p.is_file());
            if config_xml.is_some() {
                tracing::debug!(?current, "Found configuration root via Configuration.xml");
                return Some(current.to_path_buf());
            }

            current = match current.parent() {
                Some(parent) if parent != current => parent,
                _ => return None,
            };
        }
    }
}

#[salsa::db]
impl salsa::Database for RootDatabaseImpl {}

#[salsa::db]
impl SourceDatabase for RootDatabaseImpl {
    fn file_text_input(&self, file_id: FileId) -> base_db::FileTextInput {
        self.files.file_text(file_id)
    }

    fn try_file_text_input(&self, file_id: FileId) -> Option<base_db::FileTextInput> {
        self.files.try_file_text(file_id)
    }

    fn file_revision_input(&self, file_id: FileId) -> base_db::FileRevisionInput {
        self.files.file_revision(file_id)
    }

    fn try_file_revision_input(&self, file_id: FileId) -> Option<base_db::FileRevisionInput> {
        self.files.try_file_revision(file_id)
    }

    fn file_text(&self, file_id: FileId) -> std::sync::Arc<str> {
        self.file_text_ref(file_id).clone()
    }

    fn file_text_ref(&self, file_id: FileId) -> &std::sync::Arc<str> {
        let input = base_db::FileIdInput::new(self, file_id);
        base_db::file_text_query(self, input)
    }

    fn set_file_revision_from_disk(&mut self, file_id: FileId, revision: u64) {
        let files = self.files.clone();
        files.set_file_revision_from_disk(self, file_id, revision);
    }

    fn source_root_input(&self, source_root_id: SourceRootId) -> base_db::SourceRootInput {
        self.files.source_root(source_root_id)
    }

    fn try_source_root_input(
        &self,
        source_root_id: SourceRootId,
    ) -> Option<base_db::SourceRootInput> {
        self.files.try_source_root(source_root_id)
    }

    fn file_source_root_input(&self, file_id: FileId) -> base_db::FileSourceRootInput {
        self.files.file_source_root(file_id)
    }

    fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let files = self.files.clone();
        files.set_file_text_smart(self, file_id, text);
    }

    fn set_file_unreadable(&mut self, file_id: FileId) {
        let files = self.files.clone();
        files.set_file_unreadable(self, file_id);
    }

    fn file_is_unread(&self, file_id: FileId) -> bool {
        self.files.file_is_unread(self, file_id)
    }

    fn parse_snapshot(&self, file_id: FileId) -> Option<base_db::ParseSnapshot> {
        self.files.parse_snapshot(file_id)
    }

    fn store_parse_snapshot(&self, file_id: FileId, snapshot: base_db::ParseSnapshot) {
        self.files.store_parse_snapshot(file_id, snapshot);
    }

    fn count_parse(&self, outcome: base_db::ParseOutcome) {
        self.files.count_parse(outcome);
    }

    fn parse_stats(&self) -> base_db::ParseStats {
        self.files.parse_stats()
    }

    fn set_file_source_root(&mut self, file_id: FileId, source_root_id: SourceRootId) {
        let files = self.files.clone();
        files.set_file_source_root(self, file_id, source_root_id);
    }

    fn set_source_root(&mut self, source_root_id: SourceRootId, source_root: SourceRoot) {
        let files = self.files.clone();
        files.set_source_root(self, source_root_id, source_root);
    }

    fn resolve_vfs_path(
        &self,
        source_root_id: SourceRootId,
        vfs_path: &vfs::VfsPath,
    ) -> Option<FileId> {
        let source_root_input = self.source_root_input(source_root_id);
        let vfs_path_str = vfs_path.as_path().to_string_lossy().to_string();
        base_db::resolve_vfs_path_query(self, source_root_input, vfs_path_str)
    }
}

#[salsa::db]
impl RootQueryDb for RootDatabaseImpl {
    fn parse(&self, file_id: FileId) -> syntax::Parse<syntax::SyntaxNode> {
        self.parse_ref(file_id).clone()
    }

    fn parse_ref(&self, file_id: FileId) -> &syntax::Parse<syntax::SyntaxNode> {
        let input = base_db::FileIdInput::new(self, file_id);
        base_db::parse_query(self, input)
    }

    fn method_regions(
        &self,
        file_id: FileId,
    ) -> Arc<std::collections::HashMap<syntax::TextRange, String>> {
        let input = base_db::FileIdInput::new(self, file_id);
        base_db::method_regions_query(self, input)
    }
}

#[salsa::db]
impl DefDatabase for RootDatabaseImpl {
    fn item_tree(&self, file_id: FileId) -> Arc<ItemTree> {
        self.item_tree_ref(file_id).clone()
    }

    fn item_tree_ref(&self, file_id: FileId) -> &Arc<ItemTree> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        hir::item_tree_query(self, file_id_input)
    }

    fn region_tree(&self, file_id: FileId) -> Arc<RegionTree> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        hir::region_tree_query(self, file_id_input).clone()
    }

    fn conditional_tree(&self, file_id: FileId) -> Arc<ConditionalTree> {
        self.conditional_tree_ref(file_id).clone()
    }

    fn conditional_tree_ref(&self, file_id: FileId) -> &Arc<ConditionalTree> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        hir::conditional_tree_query(self, file_id_input)
    }

    fn module_data(&self, module_id: ModuleId) -> Arc<ModuleData> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::module_data_query(self, file_id_input).clone()
    }

    fn module_interface(&self, module_id: ModuleId) -> Arc<hir::ModuleInterface> {
        self.module_interface_ref(module_id).clone()
    }

    fn module_interface_ref(&self, module_id: ModuleId) -> &Arc<hir::ModuleInterface> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::module_interface_query(self, file_id_input)
    }

    fn interface_method(&self, method_id: hir::MethodId) -> Option<Arc<hir::MethodDecl>> {
        hir::interface_method_query(self, hir::MethodIdInput::new(self, method_id))
    }

    fn interface_method_named(
        &self,
        module_id: ModuleId,
        name: &hir::Name,
    ) -> Option<Arc<hir::MethodDecl>> {
        hir::interface_method_named(self, module_id, NormName::intern(name.as_str()))
    }

    fn interface_variable_named(
        &self,
        module_id: ModuleId,
        name: &hir::Name,
    ) -> Option<Arc<hir::VariableDecl>> {
        hir::interface_variable_named(self, module_id, NormName::intern(name.as_str()))
    }

    fn symbol_tree(&self, module_id: ModuleId) -> Arc<SymbolTree> {
        self.symbol_tree_ref(module_id).clone()
    }

    fn symbol_tree_ref(&self, module_id: ModuleId) -> &Arc<SymbolTree> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::symbol_tree_query(self, file_id_input)
    }

    fn module_bodies(&self, module_id: ModuleId) -> Arc<ModuleBodies> {
        self.module_bodies_ref(module_id).clone()
    }

    fn module_bodies_ref(&self, module_id: ModuleId) -> &Arc<ModuleBodies> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::module_bodies_query(self, file_id_input)
    }

    fn method_body(&self, method: hir::MethodIdInput<'_>) -> Arc<hir::Body> {
        self.method_body_ref(method).clone()
    }

    fn method_body_ref<'db>(&'db self, method: hir::MethodIdInput<'db>) -> &'db Arc<hir::Body> {
        hir::method_body_query(self, method)
    }

    fn method_lower(&self, method: hir::MethodIdInput<'_>) -> Option<Arc<hir::LowerResult>> {
        hir::method_lower_query(self, method).clone()
    }

    fn module_metadata(&self, module_id: ModuleId) -> Arc<hir::ModuleMetadata> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        module_metadata_query(self, file_id_input)
    }

    fn module_call_summary(&self, module_id: ModuleId) -> Arc<hir::ModuleCallSummary> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::module_call_summary_query(self, file_id_input)
    }

    // Docs come off the method's own declaration, so a reader keyed by the
    // method depends on neither the position nor the rest of the file.
    fn method_docs(&self, method: hir::MethodId) -> Option<Arc<hir::MethodDocs>> {
        self.interface_method(method)?.docs.clone()
    }

    fn variable_docs(&self, variable: hir::VariableId) -> Option<Arc<hir::VariableDocs>> {
        let interface = self.module_interface_ref(variable.module);
        let variable_symbol = interface.find_variable_by_id(variable)?;
        variable_symbol.docs.clone()
    }

    fn module_members(&self, source_root_id: base_db::SourceRootId) -> Arc<hir::WorkspaceMembers> {
        let source_root_input = self.source_root_input(source_root_id);
        hir::module_members_query(self, source_root_input)
    }

    fn workspace_index(&self, source_root_id: base_db::SourceRootId) -> Arc<hir::WorkspaceIndex> {
        let source_root_input = self.source_root_input(source_root_id);
        hir::workspace_index_query(self, source_root_input)
    }

    fn name_usage_index(
        &self,
        source_root_id: base_db::SourceRootId,
    ) -> Arc<hir::SourceRootNameUsage> {
        let source_root_input = self.source_root_input(source_root_id);
        hir::source_root_name_usage_query(self, source_root_input)
    }

    fn file_name_offsets(&self, file_id: FileId) -> Arc<hir::FileNameOffsets> {
        self.file_name_offsets_ref(file_id).clone()
    }

    fn file_name_offsets_ref(&self, file_id: FileId) -> &Arc<hir::FileNameOffsets> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        hir::file_name_offsets_query(self, file_id_input)
    }

    fn file_external_refs(&self, module_id: ModuleId) -> Arc<Vec<hir::ExternalRef>> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::file_external_refs_query(self, file_id_input)
    }

    fn module_index(&self, source_root_id: base_db::SourceRootId) -> Arc<hir::ModuleIndex> {
        let source_root_input = self.source_root_input(source_root_id);
        hir::module_index_query(self, source_root_input)
    }

    fn file_dependencies(&self, module_id: ModuleId) -> Arc<Vec<FileId>> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::file_dependencies_query(self, file_id_input)
    }
}

#[salsa::db]
impl hir::ConfigsDatabase for RootDatabaseImpl {
    fn configurations(&self, file_id: FileId) -> Vec<hir::VisibleConfig> {
        RootDatabase::get_all_configurations(self, file_id)
            .into_iter()
            .map(|(name, configuration)| hir::VisibleConfig { name, configuration })
            .collect()
    }

    fn configurations_inventory(&self) -> Vec<hir::VisibleConfig> {
        RootDatabase::all_configurations_inventory(self)
            .into_iter()
            .map(|(name, configuration)| hir::VisibleConfig { name, configuration })
            .collect()
    }

    fn warm_config_roots(&self, modules: &[ModuleId]) {
        // Load each root through its own key rather than through
        // `module_metadata`: that query returns early for service modules whose
        // service resolves out of the DECLARED roots, so a representative can
        // report success without its attributed root ever being read — and the
        // dedup below would then skip every remaining module of that root.
        //
        // Deduped on the interned key, not the root path: two files of one root
        // under different declared roots carry different revisions, and it is
        // the key that the loader memoises.
        let mut warmed = rustc_hash::FxHashSet::default();
        for module in modules {
            let Some(path) = vfs_helpers::get_file_path(self, module.file_id) else {
                continue;
            };
            let Some(path_input) = self.configuration_input_for_path(&path) else {
                continue;
            };
            if warmed.insert(path_input) {
                let _ = metadata::MetadataDb::load_configuration(self, path_input);
                // The per-file memos a batch also parks on (the extension merge
                // above all) hang off `module_metadata`, so one representative
                // still pays for them here rather than inside the pool.
                let _ = hir::DefDatabase::module_metadata(self, *module);
            }
        }
    }

    fn resolve_metadata_object(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        RootDatabaseImpl::resolve_metadata_object_for_file(self, file_id, mdo_type, name)
    }

    fn resolve_register(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        RootDatabaseImpl::resolve_register_for_file(self, file_id, mdo_type, name)
    }

    fn resolve_register_by_name(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Register>> {
        RootDatabaseImpl::resolve_register_by_name_for_file(self, file_id, name)
    }

    fn has_effective_module_variable(
        &self,
        file_id: FileId,
        module_type: bsl_metadata::ModuleType,
        mdo_type: bsl_metadata::MdoType,
        object_name: &str,
        variable_name: &str,
    ) -> bool {
        let role = match module_type {
            bsl_metadata::ModuleType::ObjectModule => crate::EffectiveModuleRole::Object,
            bsl_metadata::ModuleType::ManagerModule => crate::EffectiveModuleRole::Manager,
            _ => return false,
        };
        let source_root_id = self.file_source_root_input(file_id).source_root_id(self);
        crate::effective_module_exports_query(
            self,
            source_root_id,
            Some(file_id),
            role,
            mdo_type,
            object_name.to_string(),
            None,
        )
        .variables
        .iter()
        .any(|item| stdx::case::eq_ignore_case(item.variable.name.as_str(), variable_name))
    }

    fn resolve_defined_type(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<bsl_metadata::AttributeType> {
        RootDatabaseImpl::resolve_defined_type_for_file(self, file_id, name)
    }

    fn resolve_common_module(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        RootDatabaseImpl::resolve_common_module_for_file(self, file_id, name)
    }

    fn resolve_mdo_module_file_candidates(
        &self,
        file_id: FileId,
        role: hir::MdoModuleRole,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<hir::CommonModuleBodies> {
        self.resolve_mdo_module_files_for_file(file_id, role, mdo_type, name)
    }

    fn resolve_common_module_file_candidates(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<hir::CommonModuleBodies> {
        // The helper orders extension-first for merged-surface validation;
        // qualified resolution wants the base declaration to win, so reverse.
        let mut bodies = self.resolve_common_module_files_for_file(file_id, name);
        bodies.reverse_priority();
        Some(bodies)
    }

    fn resolve_application_module_file_candidates(
        &self,
        file_id: FileId,
        kind: hir::ApplicationModuleKind,
    ) -> Option<hir::CommonModuleBodies> {
        self.resolve_application_module_files_for_file(file_id, kind)
    }

    fn resolve_event_subscription(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::EventSubscription>> {
        RootDatabaseImpl::resolve_event_subscription_for_file(self, file_id, name)
    }

    fn resolve_role(&self, file_id: FileId, name: &str) -> Option<Arc<bsl_metadata::Role>> {
        RootDatabaseImpl::resolve_role_for_file(self, file_id, name)
    }

    fn role_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::role_names_for_file(self, file_id)
    }

    fn enumerate_roles(&self, file_id: FileId) -> Vec<Arc<bsl_metadata::Role>> {
        RootDatabaseImpl::enumerate_roles_for_file(self, file_id)
    }

    fn event_subscription_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::event_subscription_names_for_file(self, file_id)
    }

    fn enumerate_event_subscriptions(
        &self,
        file_id: FileId,
    ) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        RootDatabaseImpl::enumerate_event_subscriptions_for_file(self, file_id)
    }

    fn resolve_scheduled_job(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::ScheduledJob>> {
        RootDatabaseImpl::resolve_scheduled_job_for_file(self, file_id, name)
    }

    fn scheduled_job_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::scheduled_job_names_for_file(self, file_id)
    }

    fn resolve_http_service(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::HTTPService>> {
        RootDatabaseImpl::resolve_http_service_for_file(self, file_id, name)
    }

    fn http_service_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::http_service_names_for_file(self, file_id)
    }

    fn resolve_web_service(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::WebService>> {
        RootDatabaseImpl::resolve_web_service_for_file(self, file_id, name)
    }

    fn web_service_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::web_service_names_for_file(self, file_id)
    }

    fn resolve_subsystem(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::Subsystem>> {
        RootDatabaseImpl::resolve_subsystem_for_file(self, file_id, name)
    }

    fn subsystem_names(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::subsystem_names_for_file(self, file_id)
    }

    fn enumerate_subsystems(&self, file_id: FileId) -> Vec<Arc<bsl_metadata::Subsystem>> {
        RootDatabaseImpl::enumerate_subsystems_for_file(self, file_id)
    }

    fn has_config_root(&self, file_id: FileId) -> bool {
        !RootDatabaseImpl::all_config_paths(self).is_empty()
            || RootDatabase::get_configuration(self, file_id).is_some()
    }

    fn file_has_visible_config(&self, file_id: FileId) -> bool {
        let Some(file_path) = vfs_helpers::get_file_path(self, file_id) else {
            return false;
        };

        match self.visible_roots_for_file(file_id) {
            Some(roots) => roots.main.is_some() || !roots.chain.is_empty(),
            None => vfs_helpers::find_configuration_root(self, &file_path).is_some(),
        }
    }

    fn recorders_for_register(
        &self,
        file_id: FileId,
        parent: bsl_metadata::MdoType,
        register_name: &str,
    ) -> Vec<String> {
        // A reverse relation (all documents writing to the register) cannot narrow
        // to one MDO, so it reads the whole merged visible configuration for now; a
        // per-document reverse index is a follow-up.
        self.merged_visible_configuration(file_id)
            .map(|config| {
                config
                    .recorders_for_register(parent, register_name)
                    .iter()
                    .map(|n| n.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn merged_visible_configuration(
        &self,
        file_id: FileId,
    ) -> Option<Arc<bsl_metadata::Configuration>> {
        let input_for = |path: &std::path::Path| -> metadata::ConfigurationPathInput<'_> {
            metadata::intern_configuration_path(
                self,
                &path.to_string_lossy(),
                self.config_root_revision_for_path(path),
            )
        };

        let Some(roots) = self.visible_roots_for_file(file_id) else {
            let file_path = vfs_helpers::get_file_path(self, file_id)?;
            let config_root = vfs_helpers::find_configuration_root(self, &file_path)?;
            return Some(self.load_configuration(input_for(&config_root)));
        };

        // Forward composition through the memoised chain query: the base, then
        // each dependency's overlay in order, the file's own extension last. The
        // deep clone of the accumulated configuration runs once per unique chain
        // prefix, not on every metadata lookup.
        let chain_inputs: Vec<metadata::ConfigurationPathInput<'_>> = roots
            .main
            .iter()
            .map(|p| input_for(p))
            .chain(roots.chain.iter().map(|(_, p)| input_for(p)))
            .collect();
        match chain_inputs.as_slice() {
            [] => None,
            [only] => Some(self.load_configuration(*only)),
            _ => {
                // Warm every prefix bottom-up so the recursive step inside
                // `chain_configuration` always finds its sub-chain memoised:
                // the recursion depth stays 1 regardless of chain length.
                for prefix_len in 2..chain_inputs.len() {
                    let prefix =
                        metadata::ConfigChainInput::new(self, chain_inputs[..prefix_len].to_vec());
                    let _ = metadata::chain_configuration(self, prefix);
                }
                let chain = metadata::ConfigChainInput::new(self, chain_inputs);
                Some(metadata::chain_configuration(self, chain))
            }
        }
    }

    fn resolved_module_summary(
        &self,
        module_id: ModuleId,
    ) -> Arc<hir::call_graph::ResolvedModuleSummary> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        hir::resolved_module_summary_query(self, file_id_input)
    }

    fn workspace_call_graph(
        &self,
        source_root_id: base_db::SourceRootId,
    ) -> Arc<hir::call_graph::WorkspaceCallGraph> {
        let source_root_input = self.source_root_input(source_root_id);
        hir::workspace_call_graph_query(self, source_root_input)
    }
}

#[salsa::db]
impl hir::HirDatabase for RootDatabaseImpl {
    fn infer(&self, file_id: FileId) -> Arc<hir::InferenceResult> {
        let file_id_input = FileIdInput::new(self, file_id);
        hir::infer_query(self, file_id_input)
    }

    fn type_of_expr(
        &self,
        file_id: FileId,
        owner: hir::DefWithBodyId,
        expr: hir::ExprId,
    ) -> hir::TypeId {
        hir::type_of_expr_query(self, file_id, owner, expr)
    }

    fn narrow(
        &self,
        file_id: FileId,
        owner: hir::DefWithBodyId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::NarrowState>>> {
        hir::narrow_query(self, file_id, owner)
    }

    fn method_arg_diagnostics(
        &self,
        method: hir::MethodIdInput<'_>,
    ) -> Arc<Vec<hir::InferenceDiagnostic>> {
        hir::method_arg_diagnostics_query(self, method)
    }

    fn module_code_arg_diagnostics(&self, file_id: FileId) -> Arc<Vec<hir::InferenceDiagnostic>> {
        let file_id_input = FileIdInput::new(self, file_id);
        hir::module_code_arg_diagnostics_query(self, file_id_input)
    }

    fn arg_diagnostics(
        &self,
        file_id: FileId,
    ) -> Arc<Vec<(hir::DefWithBodyId, hir::InferenceDiagnostic)>> {
        Arc::new(hir::file_arg_diagnostics(self, file_id))
    }

    fn type_narrowing_enabled(&self) -> bool {
        RootDatabaseImpl::type_narrowing_enabled(self)
    }

    fn env_options(&self) -> hir::execution_env::EnvOptions {
        RootDatabaseImpl::env_options(self)
    }

    fn target_platform_version(&self) -> Option<Arc<str>> {
        RootDatabaseImpl::target_platform_version(self)
    }

    fn min_platform_version(&self) -> Option<Arc<str>> {
        RootDatabaseImpl::min_platform_version(self)
    }

    fn compatibility_mode(&self) -> Option<Arc<str>> {
        RootDatabaseImpl::compatibility_mode(self)
    }

    fn workspace_load_complete(&self) -> bool {
        RootDatabaseImpl::workspace_load_complete(self)
    }

    fn proc_signature(
        &self,
        method_input: hir::MethodIdInput<'_>,
    ) -> Arc<hir::proc_signature::ProcSignature> {
        hir::proc_signature::proc_signature_query(self, method_input).clone()
    }

    fn infer_method(&self, method: hir::MethodIdInput<'_>) -> Arc<hir::BodyInferenceResult> {
        self.infer_method_ref(method).clone()
    }

    fn infer_method_ref<'db>(
        &'db self,
        method: hir::MethodIdInput<'db>,
    ) -> &'db Arc<hir::BodyInferenceResult> {
        hir::infer_method_query(self, method)
    }

    fn infer_module_code(&self, file_id: FileId) -> Arc<hir::ModuleCodeInferenceResult> {
        self.infer_module_code_ref(file_id).clone()
    }

    fn infer_module_code_ref(&self, file_id: FileId) -> &Arc<hir::ModuleCodeInferenceResult> {
        let file_id_input = FileIdInput::new(self, file_id);
        hir::infer_module_code_query(self, file_id_input)
    }

    fn module_code_reaching_definitions(
        &self,
        file_id: FileId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        let file_id_input = FileIdInput::new(self, file_id);
        queries::module_code_reaching_definitions_query(self, file_id_input)
    }

    fn method_reaching_definitions(
        &self,
        method: hir::MethodIdInput<'_>,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        queries::reaching_definitions_query(self, method)
    }
}

#[salsa::db]
impl RootDatabase for RootDatabaseImpl {
    fn get_configuration(&self, file_id: FileId) -> Option<Arc<bsl_metadata::Configuration>> {
        let file_path = vfs_helpers::get_file_path(self, file_id)?;
        let path_input = self.configuration_input_for_path(&file_path)?;
        Some(self.load_configuration(path_input))
    }

    fn get_all_configurations(
        &self,
        file_id: FileId,
    ) -> Vec<(Option<String>, Arc<bsl_metadata::Configuration>)> {
        let load = |path: &std::path::Path| {
            let path_input = metadata::intern_configuration_path(
                self,
                &path.to_string_lossy(),
                self.config_root_revision_for_path(path),
            );
            self.load_configuration(path_input)
        };

        let Some(roots) = self.visible_roots_for_file(file_id) else {
            return self.get_configuration(file_id).into_iter().map(|c| (None, c)).collect();
        };

        roots
            .main
            .iter()
            .map(|p| (None, load(p)))
            .chain(roots.chain.iter().map(|(name, p)| (Some(name.clone()), load(p))))
            .collect()
    }

    fn all_configurations_inventory(
        &self,
    ) -> Vec<(Option<String>, Arc<bsl_metadata::Configuration>)> {
        let snapshot = self.workspace_configs_snapshot();
        snapshot
            .inventory_order()
            .map(|idx| snapshot.paths[idx].clone())
            .map(|(name, path)| {
                let path_input = metadata::intern_configuration_path(
                    self,
                    &path.to_string_lossy(),
                    self.config_root_revision_for_path(&path),
                );
                let config = self.load_configuration(path_input);
                (name, config)
            })
            .collect()
    }

    fn all_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)> {
        RootDatabaseImpl::all_config_paths(self)
    }

    fn designer_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)> {
        RootDatabaseImpl::designer_config_paths(self)
    }

    fn external_root_of_path(
        &self,
        path: &std::path::Path,
    ) -> Option<(std::path::PathBuf, std::path::PathBuf, bsl_metadata::ExternalObjectKind)> {
        RootDatabaseImpl::workspace_configs_snapshot(self).external_root_of_path(path)
    }

    fn config_root_rank_and_label(&self, file_id: FileId) -> Option<(usize, Option<String>)> {
        RootDatabaseImpl::config_root_rank_and_label(self, file_id)
    }

    fn visible_config_root_ranks(&self, file_id: FileId) -> Option<Vec<usize>> {
        RootDatabaseImpl::visible_config_root_ranks(self, file_id)
    }

    fn common_module_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        RootDatabaseImpl::common_module_for_file_id(self, module_file_id)
    }

    fn main_event_subscription_names_for_file(&self, file_id: FileId) -> Vec<String> {
        RootDatabaseImpl::main_event_subscription_names_for_file(self, file_id)
    }

    fn http_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::HTTPService>> {
        RootDatabaseImpl::http_service_for_file_id(self, module_file_id)
    }

    fn web_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::WebService>> {
        RootDatabaseImpl::web_service_for_file_id(self, module_file_id)
    }

    fn integration_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::IntegrationService>> {
        RootDatabaseImpl::integration_service_for_file_id(self, module_file_id)
    }

    fn resolve_common_module_files(&self, file_id: FileId, name: &str) -> hir::CommonModuleBodies {
        RootDatabaseImpl::resolve_common_module_files_for_file(self, file_id, name)
    }

    fn resolve_application_module_files_uncached(
        &self,
        file_id: FileId,
        kind: hir::ApplicationModuleKind,
    ) -> Option<hir::CommonModuleBodies> {
        self.resolve_application_module_files_uncached_impl(file_id, kind)
    }

    fn all_sdbl_in_file(
        &self,
        file_id: FileId,
    ) -> Arc<Vec<(hir::SdblExprId, syntax::SdblQueryInfo)>> {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        all_sdbl_in_file_query(self, file_id_input)
    }

    fn sdbl_hir_in_file(&self, file_id: FileId) -> SdblHirEntries {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        sdbl_hir_for_file_query(self, file_id_input)
    }

    fn reaching_definitions(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        let method_id_input = hir::MethodIdInput::new(self, method_id);
        reaching_definitions_query(self, method_id_input)
    }

    fn method_path_terminates(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::path_terminates::PathTerminatesResult>> {
        let method_id_input = hir::MethodIdInput::new(self, method_id);
        queries::method_path_terminates_query(self, method_id_input)
    }

    fn method_security_state(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        let method_id_input = hir::MethodIdInput::new(self, method_id);
        crate::effects::method_security_state_query(self, method_id_input)
    }

    fn module_code_security_state(
        &self,
        file_id: FileId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        let file_id_input = base_db::FileIdInput::new(self, file_id);
        crate::effects::module_code_security_state_query(self, file_id_input)
    }

    fn method_cfg(&self, method_id: hir::MethodId) -> Arc<hir::cfg::ControlFlowGraph> {
        let method_id_input = hir::MethodIdInput::new(self, method_id);
        method_cfg_query(self, method_id_input)
    }

    fn module_level_cfg(&self, module_id: hir::ModuleId) -> Arc<hir::cfg::ControlFlowGraph> {
        let file_id_input = base_db::FileIdInput::new(self, module_id.file_id);
        queries::module_level_cfg_query(self, file_id_input)
    }

    fn line_index(&self, file_id_input: base_db::FileIdInput) -> Arc<line_index::LineIndex> {
        line_index_query(self, file_id_input)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn visible_roots_for_file(&self, file_id: FileId) -> Option<VisibleRoots> {
        RootDatabaseImpl::visible_roots_for_file(self, file_id)
    }

    fn config_root_revision_for_path(&self, path: &Path) -> u32 {
        RootDatabaseImpl::config_root_revision_for_path(self, path)
    }
}

#[salsa::db]
impl metadata::MetadataDb for RootDatabaseImpl {
    /// Override the default loader to consult the build-scoped cache when one is
    /// attached, so the whole-config metadata load runs once per config root per
    /// build instead of once per fresh batch database. This is the single chokepoint
    /// every config read funnels through (the resolver's `find_*`, `module_metadata`,
    /// `configurations`/`merged_visible_configuration`), so caching here covers them
    /// all. With no cache attached (the LSP database) it is the plain salsa query.
    fn load_configuration<'db>(
        &'db self,
        path_input: metadata::ConfigurationPathInput<'db>,
    ) -> Arc<bsl_metadata::Configuration> {
        // Boot gate: while the initial workspace load streams in, the full-config
        // XML parse must not run — it is minutes of non-cancellable work inside a
        // single query, against metadata the VFS has not finished delivering. An
        // empty configuration resolves nothing, which is the correct boot-window
        // answer; the input read records a dependency on the calling query, so
        // the finalize flip recomputes everything resolved against this stub.
        if !self.workspace_load_complete() {
            tracing::debug!("workspace load incomplete; whole-config load gated to empty");
            return Arc::new(bsl_metadata::Configuration::new("Configuration"));
        }

        let Some(cache) = &self.graph_config_cache else {
            return metadata::load_configuration(self, path_input);
        };
        let key = PathBuf::from(path_input.path(self));
        if let Some(config) = cache.get(&key) {
            return Arc::clone(&config);
        }
        // Miss: load (the build warms each root sequentially before its parallel
        // region, so concurrent first-loads of the same root do not occur; a rare
        // duplicate load would only repeat pure work, never corrupt the result).
        // A miss under an exclusive-pool job means the warm-up failed to cover this
        // root and the internally-parallel loader is about to run on the build pool
        // — record which root slipped through before the loader's own check fires.
        if stdx::par_guard::no_nested_parallelism() {
            tracing::error!(
                path = %key.display(),
                "graph config cache miss inside a no-nested-parallelism job; \
                 the pre-pool warm-up did not cover this config root"
            );
        }
        let config = metadata::load_configuration(self, path_input);
        cache.insert(key, Arc::clone(&config));
        config
    }
}

#[cfg(test)]
#[path = "database_impl_tests.rs"]
mod database_impl_tests;
