pub mod batch_fixes;
mod call_hierarchy;
mod call_hierarchy_index;
mod completion;
pub mod config_finder;
mod declarations;
pub mod diagnostics_baseline;
pub mod diagnostics_catalog;
mod document_highlight;
mod document_symbols;
mod folding;
pub mod formatting;
mod goto_definition;
pub mod graph;
mod hover;
mod inlay_hints;
pub mod jsonl;
mod name_lookup;
pub mod partitioned_diagnostics_baseline;
mod reference_kind;
mod references;
mod rename;
mod selection_range;
mod signature_help;
pub mod symbol_info;
mod syntax_highlighting;
mod type_definition;

pub use call_hierarchy::{CallHierarchyCall, CallHierarchyItem};
pub use call_hierarchy_index::{
    build_call_hierarchy_index, reproject_call_hierarchy_index_modules, CallHierarchyBatchEvent,
    CallHierarchyBatchEventKind, CallHierarchyBatchPhase, CallHierarchyIndexBuildError,
    CallHierarchyIndexBuildRequest, CallHierarchyIndexBuildResult,
    CallHierarchyIndexModuleProjection, CallHierarchyRssSample,
};
pub use completion::{CompletionItem, CompletionItemKind};
pub use declarations::{
    classify_unreferenceable, graph_id_of_declaration, resolve_declarations, Declaration,
    DeclarationKind, UnsupportedCategory,
};
pub use diagnostics_catalog::{catalog_entry, diagnostic_catalog, CatalogEntry, SeverityBucket};
pub use document_highlight::{DocumentHighlight, DocumentHighlightKind};
pub use folding::{FoldingRange, FoldingRangeKind};
pub use formatting::{FormattingConfig, FormattingResult};
pub use graph::{
    build_workspace_graph_rows, call_site_absence_reason, classify_graph_id, confidence_label,
    folded_common_scope_for_path, form_key_for_path, method_graph_id, method_id_for_path,
    module_id_of_method, rank_resolve_candidates, reproject_changed_modules,
    reproject_metadata_owners, resolve_name_segment, scope_for_path, warm_batch_config_roots,
    BatchDbOpener, ChunkRow, Direction, EdgeRef, FusedChunkSink, GraphBuildSummary,
    GraphBuildTicker, GraphContext, GraphDetail, GraphError, GraphIdKind, GraphOverview,
    GraphRowSink, ModuleMethod, NeighborsParams, NeighborsResult, NodeRef, NodeResult,
    ReprojectedRows, ResolveCandidate, ResolveResult, SourceItem, SourceResult, StripRoot,
    CALL_SITE_NOT_RECORDED, MAX_DROPPED_SAMPLE, NO_CALL_SITE, NO_SOURCE_LOCATION,
    ROOTS_UNAVAILABLE, SOURCE_DRIFTED,
};
pub use hir::graph_index;
pub use hir::AnnotationKind;
pub use hir::ModuleId;
pub use hir::{call_hierarchy_method_digest, MethodCallDigest};
pub use ide_assists::{Assist, AssistId, SourceChange};
pub use ide_db::base_db::Locale;
pub use ide_db::metadata::{RootKind as WorkspaceRootKind, WorkspaceConfigsSnapshot};
pub use ide_db::query_resolver::AcrossRootsQueryResolver;
pub use ide_db::{
    CommonModuleBodies, GraphConfigCache, RootDatabase, RootDatabaseImpl, SymbolKind, TextRange,
};
pub use ide_diagnostics::{
    all_diagnostic_codes, apply_extension_merge, diagnostics as compute_diagnostics, docs,
    file_diagnostics, file_diagnostics_query, get_metadata, message_with_standards,
    slab_verify_mismatches, standard_url, standards, validate_query_text, CleanCodeAttribute,
    Diagnostic, DiagnosticCode, DiagnosticOutput, DiagnosticSeverityLevel, DiagnosticTag,
    DiagnosticType, DiagnosticsConfig, DiagnosticsContext, Fix, ImpactSeverity, MetadataTag,
    Severity, SoftwareQuality, TextEdit, METADATA_DEPENDENT_CODES,
};
pub use inlay_hints::{InlayHint, InlayHintKind};
pub use name_lookup::{
    line_text, line_with_context, lookup_names, match_tier, resolve_file_range, resolve_place,
    ExternalNameSource, NameCandidate, NameCategory, NameLookupResult, NameMatchTier, NamePlace,
    NameQuery, PlatformRef, ProviderHits, ProviderId, ProviderReport, ProviderState, ResolvedPlace,
    WORKSPACE_SYMBOL_LIMIT,
};
pub use reference_kind::{classify_reference_token, ReferenceKind};
pub use references::{
    find_references_by_name, AnchorSite, AnchorStaleReason, BodySource, FileIdSet, ReferenceAnchor,
    ReferenceArea, ReferenceHit, ReferencesOutcome, ReferencesRequest, ReferencesResult,
};
pub use rename::{prepare_rename, rename, RenameError, RenameTarget};
pub use signature_help::{ParameterInfo, SignatureHelp, SignatureInformation};
pub use symbol_info::{
    is_well_formed_symbol, symbol_info, DefinitionRole, SymbolContainer, SymbolDefinition,
    SymbolDefinitionSite, SymbolInfoCard, SymbolInfoRequest, SymbolInfoSections, SymbolMember,
    SymbolMemberAvailability, SymbolMemberContextStatus, SymbolMemberOrigin, SymbolMemberSignature,
    SymbolPosition, SymbolTypeVariant, SYMBOL_KINDS,
};
pub use syntax_highlighting::{highlight, HighlightResult, HlMod, HlRange, HlTag};

use ide_db::base_db::DiagnosticsConfigInput;
use std::path::PathBuf;
use std::sync::Arc;
use syntax::TextSize;
use vfs::FileId;

/// One deep eviction pass on the small sweep caps of every per-method
/// chain — lowering, dataflow and diagnostics — then back to the interactive
/// caps. The diagnostics memos live above `ide_db`, so their cap is switched
/// here, around the database's own switch.
pub fn sweep_lru_deep(db: &mut RootDatabaseImpl) {
    ide_diagnostics::set_diagnostics_lru_sweep_mode(db, true);
    db.enforce_lru_deep();
    ide_diagnostics::set_diagnostics_lru_sweep_mode(db, false);
}

pub struct Analysis {
    db: RootDatabaseImpl,
}

impl Analysis {
    pub fn new() -> Self {
        Self { db: RootDatabaseImpl::default() }
    }

    pub fn from_database(db: RootDatabaseImpl) -> Self {
        Self { db }
    }

    pub fn database(&self) -> &RootDatabaseImpl {
        &self.db
    }

    /// Run `f` with this handle attached to the current thread for the whole call,
    /// so a cancellation of the handle's token survives until a checkpoint sees it.
    ///
    /// Salsa keeps a handle's local cancellation token only for the OUTERMOST attach
    /// scope on a thread and resets it when that scope exits. Every tracked-fn body is
    /// an attach scope, so a query called from plain code is its own outermost one: a
    /// cancel that lands while it runs is wiped on its way out unless a nested checkpoint
    /// saw it first, and the next `unwind_if_revision_cancelled` reads a clear token.
    /// Held around the whole unit of cancellation — an LSP request, an MCP call — this
    /// scope makes every query inside a nested one, and the cancel keeps from the moment
    /// it arrives until a checkpoint unwinds with `Cancelled::Local`.
    ///
    /// Inside `f` the thread may query only THIS handle: a second database handle on
    /// the same thread makes salsa panic with «Cannot change database mid-query».
    /// Nesting on the same handle is free.
    pub fn attached<T>(&self, f: impl FnOnce(&Analysis) -> T) -> T {
        salsa::attach(&self.db, || f(self))
    }

    pub fn diagnostics(&self, file_id: FileId, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
        ide_diagnostics::file_diagnostics(&self.db, file_id, config)
    }

    pub fn goto_definition(&self, file_id: FileId, offset: u32) -> Option<NavigationTarget> {
        let offset = TextSize::from(offset);
        goto_definition::goto_definition(&self.db, file_id, offset)
    }

    pub fn find_references(&self, file_id: FileId, offset: u32) -> Vec<Location> {
        let offset = TextSize::from(offset);
        references::find_references(&self.db, file_id, offset)
    }

    pub fn type_definition(&self, file_id: FileId, offset: u32) -> Option<NavigationTarget> {
        let offset = TextSize::from(offset);
        type_definition::type_definition(&self.db, file_id, offset)
    }

    pub fn prepare_call_hierarchy(
        &self,
        file_id: FileId,
        offset: u32,
    ) -> Option<CallHierarchyItem> {
        let offset = TextSize::from(offset);
        call_hierarchy::prepare_call_hierarchy(&self.db, file_id, offset)
    }

    pub fn call_hierarchy_incoming_from_index(
        &self,
        file_id: FileId,
        offset: u32,
        index: Arc<hir::CallHierarchyReverseIndex>,
    ) -> Option<Vec<CallHierarchyCall>> {
        let offset = TextSize::from(offset);
        call_hierarchy::incoming_calls(&self.db, file_id, offset, &index)
    }

    pub fn call_hierarchy_outgoing(&self, file_id: FileId, offset: u32) -> Vec<CallHierarchyCall> {
        let offset = TextSize::from(offset);
        call_hierarchy::outgoing_calls(&self.db, file_id, offset)
    }

    pub fn prepare_rename(&self, file_id: FileId, offset: u32) -> Option<RenameTarget> {
        let offset = TextSize::from(offset);
        rename::prepare_rename(&self.db, file_id, offset)
    }

    pub fn rename(
        &self,
        file_id: FileId,
        offset: u32,
        new_name: &str,
    ) -> Result<Vec<Location>, RenameError> {
        let offset = TextSize::from(offset);
        rename::rename(&self.db, file_id, offset, new_name)
    }

    pub fn document_highlights(&self, file_id: FileId, offset: u32) -> Vec<DocumentHighlight> {
        let offset = TextSize::from(offset);
        document_highlight::document_highlights(&self.db, file_id, offset)
    }

    pub fn folding_ranges(&self, file_id: FileId) -> Vec<FoldingRange> {
        folding::folding_ranges(&self.db, file_id)
    }

    pub fn inlay_hints(&self, file_id: FileId, range: TextRange) -> Vec<InlayHint> {
        inlay_hints::inlay_hints(&self.db, file_id, range)
    }

    /// Symbols for `workspace/symbol`: dictionary candidates that have a place
    /// in a file.
    ///
    /// The narrowing is part of the QUESTION — an editor offers what it can jump
    /// to — so it happens where the answer is assembled, not as a filter in the
    /// handler that would drop rows without saying so.
    pub fn workspace_symbols(&self, query: &str) -> NameLookupResult {
        let query = NameQuery::new(query, name_lookup::WORKSPACE_SYMBOL_LIMIT).requiring_location();
        name_lookup::lookup_names(&self.db, &query, &[])
    }

    pub fn selection_ranges(&self, file_id: FileId, offsets: &[TextSize]) -> Vec<Vec<TextRange>> {
        selection_range::selection_ranges(&self.db, file_id, offsets)
    }

    pub fn completions(
        &self,
        file_id: FileId,
        offset: u32,
        workspace_root: Option<PathBuf>,
        locale: Locale,
    ) -> Vec<CompletionItem> {
        let offset = TextSize::from(offset);
        let position = completion::CompletionPosition { file_id, offset, workspace_root, locale };
        completion::completions(&self.db, position)
    }

    pub fn hover(&self, file_id: FileId, offset: u32, locale: Locale) -> Option<HoverResult> {
        let offset = TextSize::from(offset);
        hover::hover(&self.db, file_id, offset, locale)
    }

    pub fn document_symbols(&self, file_id: FileId) -> Vec<DocumentSymbol> {
        document_symbols::document_symbols(&self.db, file_id)
    }

    /// The file's map, whole or narrowed to its regions.
    ///
    /// [`OutlineMode::RegionsOnly`] answers "how is this module laid out" for a module too
    /// big to read method by method. It is a narrower QUESTION, not a trimmed answer: the
    /// caller asked for less, so nothing about the result is incomplete.
    pub fn file_outline(&self, file_id: FileId, mode: OutlineMode) -> Vec<DocumentSymbol> {
        document_symbols::file_outline(&self.db, file_id, mode)
    }

    pub fn code_actions(&self, _file_id: FileId, _range: TextRange) -> Vec<Assist> {
        Vec::new()
    }

    pub fn file_dependencies(&self, file_id: FileId) -> Arc<Vec<FileId>> {
        use hir::{DefDatabase, ModuleId};
        let module_id = ModuleId::new(file_id);
        self.db.file_dependencies(module_id)
    }

    pub fn file_text(&self, file_id: FileId) -> String {
        use ide_db::base_db::SourceDatabase;
        self.db.file_text(file_id).to_string()
    }

    /// A file's source text as the shared `Arc<str>` the database holds, without a `String`
    /// copy. Reads the disk-backed text under the same LRU/revision contract as any query.
    pub fn file_text_arc(&self, file_id: FileId) -> Arc<str> {
        use ide_db::base_db::SourceDatabase;
        self.db.file_text(file_id)
    }

    /// The file's parsed syntax tree, memoized in the database. Shares the one parse the rest
    /// of the analysis rides, so a consumer can chunk it without re-parsing the source.
    pub fn parse(&self, file_id: FileId) -> syntax::Parse<syntax::SyntaxNode> {
        use ide_db::base_db::RootQueryDb;
        self.db.parse(file_id)
    }

    pub fn file_diagnostics_cached(
        &self,
        file_id: FileId,
        config: DiagnosticsConfigInput,
    ) -> Arc<Vec<Diagnostic>> {
        use ide_db::base_db::{DiagnosticsConfigId, FileIdInput};
        let file_id_input = FileIdInput::new(&self.db, file_id);
        let config_id = DiagnosticsConfigId::new(&self.db, config);
        ide_diagnostics::file_diagnostics_query(&self.db, file_id_input, config_id)
    }

    /// Diagnostics for a whole set of files (the LSP `workspace/diagnostic` sweep).
    ///
    /// A thin loop over the per-file query: each file rides the same Salsa-memoized
    /// `file_diagnostics_query` as push and single-document pull, so results are
    /// identical and already-computed files are free. Peak memory is bounded by the
    /// queries' own LRU caps (which evict at revision boundaries) — the caller must not
    /// force LRU eviction from a background worker, which would contend with the live
    /// database. Cancellation is automatic: a concurrent edit bumps the revision and the
    /// in-flight query unwinds, so the caller's `salsa::Cancelled::catch` aborts the sweep.
    pub fn workspace_diagnostics(
        &self,
        file_ids: &[FileId],
        config: DiagnosticsConfigInput,
    ) -> Vec<(FileId, Arc<Vec<Diagnostic>>)> {
        file_ids
            .iter()
            .filter_map(|&file_id| {
                // One file must not sink the whole sweep. A file racing a disk delete/rewrite can
                // make `file_text_query` panic on a revision mismatch; catch it and skip that file.
                // A `salsa::Cancelled` is a genuine revision-bump abort, not a per-file fault — it
                // must keep unwinding so the caller's `Cancelled::catch` aborts the request.
                let computed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.file_diagnostics_cached(file_id, config.clone())
                }));
                match computed {
                    Ok(diagnostics) => Some((file_id, diagnostics)),
                    Err(payload) if payload.is::<salsa::Cancelled>() => {
                        std::panic::resume_unwind(payload)
                    }
                    Err(_) => {
                        tracing::warn!(
                            file_id = file_id.0,
                            "workspace diagnostics: skipping file after a compute panic"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Load every configuration root ON THIS THREAD, before a parallel region opens.
    ///
    /// The configuration loader fans out over its own rayon scope. Warmed here, the
    /// parallel jobs find it memoised and never open that nested scope — where a stolen
    /// sibling job carrying a different `db` clone would attach a second database to a
    /// thread mid-query, which salsa forbids.
    ///
    /// The INVENTORY is what closes the window: it loads every root (base plus all
    /// extensions), while a per-file `configurations` would warm only that file's visible
    /// chain and still let an unrelated root first-load inside a worker. Call it per
    /// chunk — a prior chunk's between-chunk LRU trim can leave it cold again.
    pub fn warm_configuration_inventory(&self) {
        use hir::ConfigsDatabase;
        let _ = self.db.configurations_inventory();
    }

    /// Memoise the per-file halves of the workspace-wide name indexes for `files`:
    /// the symbol tree `module_members` folds and the name set `name_usage_index`
    /// folds. Each aggregate is one query over every file, so a cold one builds with
    /// every file's syntax tree live until it answers; warmed in chunks with a trim
    /// between them, the aggregates assemble from memos and no tree outlives its
    /// chunk. The accessors are the ones the aggregates read, so the memos are theirs.
    pub fn warm_name_indexes(&self, files: &[FileId]) {
        use hir::{DefDatabase, ModuleId};
        use ide_db::base_db::FileIdInput;
        for &file_id in files {
            let _ = self.db.symbol_tree_ref(ModuleId::new(file_id));
            let _ = hir::file_name_usage_query(&self.db, FileIdInput::new(&self.db, file_id));
        }
    }

    /// Parallel variant of [`Self::workspace_diagnostics`] for the deferred whole-project
    /// batch. Each file's Salsa-memoised `file_diagnostics_query` runs on the caller's
    /// bounded `pool`, each rayon worker on its own `db` snapshot (`db.clone()` shares the
    /// memo tables, so already-computed files stay free and interactive stays warm). The
    /// pool is the caller's — sized below the core count — so the batch never saturates the
    /// cores interactive requests need. Results are identical to the serial sweep.
    ///
    /// Cancellation and per-file panic handling match [`Self::workspace_diagnostics`]: a
    /// `salsa::Cancelled` unwinds out of the pool to abort the chunk, a per-file compute
    /// panic skips just that file.
    pub fn workspace_diagnostics_parallel(
        &self,
        file_ids: &[FileId],
        config: DiagnosticsConfigInput,
        pool: &rayon::ThreadPool,
    ) -> Vec<(FileId, Arc<Vec<Diagnostic>>)> {
        use ide_db::base_db::{DiagnosticsConfigId, FileIdInput};
        use rayon::prelude::*;

        if !file_ids.is_empty() {
            self.warm_configuration_inventory();
        }

        // Move an owned db clone into the pool (Salsa handles are `Send` but `&Analysis`
        // is not, so `install` cannot borrow `self`); each worker gets its own clone from it.
        let seed = self.db.clone();
        pool.install(move || {
            file_ids
                .par_iter()
                .map_with(seed, |db, &file_id| {
                    // Belt-and-suspenders: if a diagnostic still reaches an internally
                    // parallel query despite the warm-up, this makes it run serially rather
                    // than steal a sibling job onto this pool and attach a second database.
                    let _guard = stdx::par_guard::enter_no_nested_parallelism();
                    let computed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let file_id_input = FileIdInput::new(&*db, file_id);
                        let config_id = DiagnosticsConfigId::new(&*db, config.clone());
                        ide_diagnostics::file_diagnostics_query(&*db, file_id_input, config_id)
                    }));
                    match computed {
                        Ok(diagnostics) => Some((file_id, diagnostics)),
                        Err(payload) if payload.is::<salsa::Cancelled>() => {
                            std::panic::resume_unwind(payload)
                        }
                        Err(_) => {
                            tracing::warn!(
                                file_id = file_id.0,
                                "workspace diagnostics: skipping file after a compute panic"
                            );
                            None
                        }
                    }
                })
                .filter_map(|result| result)
                .collect()
        })
    }

    pub fn warm_caches_task(&self, file_ids: &[FileId]) -> WarmCachesTask {
        WarmCachesTask { db: self.db.clone(), file_ids: file_ids.to_vec() }
    }

    pub fn highlight(&self, file_id: FileId) -> HighlightResult {
        syntax_highlighting::highlight(&self.db, file_id)
    }

    pub fn signature_help(&self, file_id: FileId, offset: u32) -> Option<SignatureHelp> {
        let offset = TextSize::from(offset);
        signature_help::signature_help(&self.db, file_id, offset)
    }

    pub fn format_file(&self, file_id: FileId, config: &FormattingConfig) -> FormattingResult {
        use ide_db::base_db::RootQueryDb;
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        formatting::format_file(&root, config)
    }

    pub fn format_range(
        &self,
        file_id: FileId,
        range: TextRange,
        config: &FormattingConfig,
    ) -> FormattingResult {
        use ide_db::base_db::RootQueryDb;
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        formatting::format_range(&root, range, config)
    }

    pub fn on_type_formatting(
        &self,
        file_id: FileId,
        offset: u32,
        char_typed: char,
        config: &FormattingConfig,
    ) -> Option<Vec<formatting::TextEdit>> {
        use ide_db::base_db::RootQueryDb;
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        let offset = TextSize::from(offset);
        formatting::on_char_typed(&root, offset, char_typed, config).map(|r| r.edits)
    }
}

impl Default for Analysis {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WarmCachesTask {
    db: RootDatabaseImpl,
    file_ids: Vec<FileId>,
}

impl WarmCachesTask {
    pub fn cancellation_token(&self) -> salsa::CancellationToken {
        salsa::Database::cancellation_token(&self.db)
    }

    pub fn run(self) -> usize {
        use hir::{DefDatabase, ModuleId};

        for file_id in &self.file_ids {
            let module_id = ModuleId::new(*file_id);
            let _ = self.db.symbol_tree(module_id);
            let _ = self.db.module_bodies(module_id);
        }

        self.file_ids.len()
    }
}

#[derive(Debug, Clone)]
pub struct NavigationTarget {
    pub file_id: FileId,
    pub range: TextRange,
    pub name: String,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone)]
pub struct Location {
    pub file_id: FileId,
    pub range: TextRange,
}

#[derive(Debug, Clone)]
pub struct HoverResult {
    pub markup: String,
    pub range: Option<TextRange>,
}

/// How much of a file's map to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineMode {
    /// Every declaration the module makes.
    Full,
    /// Only the region skeleton, without the methods and variables inside it.
    RegionsOnly,
}

/// One node of a file's map: what it is called, where it is, and what it is.
///
/// The kind is not a field of its own — it is read off [`SymbolDetail`] through
/// [`DocumentSymbol::kind`]. A separate field would be a second source of truth for the
/// same fact, free to say `Procedure` beside a `Variable`'s details.
#[derive(Debug, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub range: TextRange,
    pub selection_range: TextRange,
    pub detail: SymbolDetail,
    pub children: Vec<DocumentSymbol>,
}

impl DocumentSymbol {
    pub fn kind(&self) -> SymbolKind {
        self.detail.kind()
    }
}

/// What a map node is, together with everything the parsed item already knows about it.
///
/// This is where a file map stops being a list of names: a consumer that has to open the
/// file again to learn whether a method is exported, which compilation directives it
/// carries and what its parameters are would be better off reading the file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolDetail {
    Procedure(MethodDetail),
    Function(MethodDetail),
    Variable(VariableDetail),
    Region,
}

impl SymbolDetail {
    pub fn kind(&self) -> SymbolKind {
        match self {
            Self::Procedure(_) => SymbolKind::Procedure,
            Self::Function(_) => SymbolKind::Function,
            Self::Variable(_) => SymbolKind::Variable,
            Self::Region => SymbolKind::Region,
        }
    }
}

/// A method's declaration, minus its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDetail {
    pub is_export: bool,
    /// Compilation directives in declaration order (`&НаКлиенте`, `&Вместо`, …).
    pub directives: Vec<AnnotationKind>,
    pub params: Vec<ParamDetail>,
}

/// A module variable's declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDetail {
    pub is_export: bool,
    pub directives: Vec<AnnotationKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDetail {
    pub name: String,
    /// `Знач` / `Val`: the parameter is passed by value.
    pub by_value: bool,
    pub default: ParamDefault,
}

/// Whether a parameter has a default value, and whether its text could be named.
///
/// Three states rather than an `Option<String>`, because two of them are NOT the same
/// answer to "is this parameter optional": a declaration whose default expression cannot
/// be read is still optional, and reporting it as required changes the arity a consumer
/// derives from the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamDefault {
    /// No `=` in the declaration: the caller must pass this argument.
    Required,
    /// Optional, and the default expression's text was recovered.
    Value(String),
    /// Optional — there is an `=` — but the expression's text could not be named.
    Unknown,
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Analysis>();
    assert_send::<WarmCachesTask>();
};

/// In-code suppression directives must be honoured by the two entry points the LSP server and
/// the MCP server use — both route through `ide_diagnostics::file_diagnostics`, one directly and
/// one through the salsa-tracked `file_diagnostics_query`.
#[cfg(test)]
mod attached_scope_tests {
    use super::Analysis;
    use salsa::Database as _;
    use std::panic::AssertUnwindSafe;

    /// Salsa's own contract, stated so the door's test below is known to measure the
    /// door and not a changed salsa: a cancel that lands inside an OUTERMOST attach
    /// scope — which is what every tracked-fn body called from plain code is — is
    /// wiped when that scope exits.
    #[test]
    fn salsa_wipes_a_cancel_on_the_way_out_of_an_outermost_scope() {
        let analysis = Analysis::new();
        let token = analysis.database().cancellation_token();
        analysis.database().attach(|_| token.cancel());
        assert!(!token.is_cancelled(), "a cancel survived the exit of an outermost scope");
    }

    /// The same cancel, landing inside a query body that returns normally, is still
    /// there for the next checkpoint when the whole call is one attach scope.
    #[test]
    fn a_cancel_landing_inside_a_query_survives_to_the_next_checkpoint_under_the_door() {
        let analysis = Analysis::new();
        let token = analysis.database().cancellation_token();
        let outcome = salsa::Cancelled::catch(AssertUnwindSafe(|| {
            analysis.attached(|analysis| {
                let db = analysis.database();
                // The notification lands while a query body is on the stack.
                db.attach(|_| token.cancel());
                db.unwind_if_revision_cancelled();
                "walked to the end"
            })
        }));
        assert!(
            matches!(outcome, Err(salsa::Cancelled::Local)),
            "the cancel was wiped before the checkpoint: {outcome:?}"
        );
    }

    #[test]
    fn the_door_nests_on_the_same_handle() {
        let analysis = Analysis::new();
        assert_eq!(analysis.attached(|outer| outer.attached(|_| 7)), 7);
    }

    /// The contract the door imposes on its body: one database per thread.
    #[test]
    #[should_panic(expected = "Cannot change database mid-query")]
    fn a_second_handle_inside_the_door_is_refused() {
        let analysis = Analysis::new();
        let other = Analysis::new();
        analysis.attached(|_| other.attached(|_| ()));
    }
}

#[cfg(test)]
mod suppression_surface_tests {
    use super::*;
    use ide_db::base_db::{
        DiagnosticsConfigInput, Locale, SourceDatabase, SourceRoot, SourceRootId,
    };
    use ide_db::vfs::{file_set::FileSet, VfsPath};
    use ide_db::RootDatabaseImpl;

    const PLAIN: &str = "Процедура Тест()\n    А = А;\nКонецПроцедуры\n";
    const SUPPRESSED: &str =
        "Процедура Тест()\n    // bsl-analyzer:off SelfAssign\n    А = А;\nКонецПроцедуры\n";

    fn analysis_for(code: &str) -> (Analysis, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, code);
        (Analysis::from_database(db), file_id)
    }

    fn has_self_assign(diags: &[Diagnostic]) -> bool {
        diags.iter().any(|d| d.code == ide_diagnostics::DiagnosticCode::SelfAssign)
    }

    #[test]
    fn mcp_diagnostics_honours_suppression() {
        let config = DiagnosticsConfig::all_enabled();
        let (plain, fid) = analysis_for(PLAIN);
        assert!(has_self_assign(&plain.diagnostics(fid, &config)), "baseline must fire");
        let (supp, fid) = analysis_for(SUPPRESSED);
        assert!(!has_self_assign(&supp.diagnostics(fid, &config)), "directive must suppress");
    }

    #[test]
    fn lsp_cached_diagnostics_honour_suppression() {
        let input = DiagnosticsConfigInput::from_raw(
            Vec::<String>::new(),
            Vec::<String>::new(),
            Vec::<(String, String)>::new(),
            false,
            hir::dataflow::DEFAULT_MAX_ITERATIONS,
            Locale::default(),
            false,
        );
        let (plain, fid) = analysis_for(PLAIN);
        assert!(
            has_self_assign(&plain.file_diagnostics_cached(fid, input.clone())),
            "baseline must fire"
        );
        let (supp, fid) = analysis_for(SUPPRESSED);
        assert!(
            !has_self_assign(&supp.file_diagnostics_cached(fid, input)),
            "directive must suppress through the salsa-tracked query"
        );
    }
}
