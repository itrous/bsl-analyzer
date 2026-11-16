use std::sync::Arc;

use base_db::{RootQueryDb, SourceDatabase};
use hir::DefDatabase;
use vfs::FileId;

use crate::SdblHirEntries;

#[salsa::db]
pub trait RootDatabase:
    SourceDatabase + RootQueryDb + DefDatabase + hir::HirDatabase + crate::metadata::MetadataDb
{
    fn get_configuration(&self, file_id: FileId) -> Option<Arc<bsl_metadata::Configuration>>;

    /// The configurations VISIBLE to `file_id` as separate entries (no merge):
    /// the base first, then the file's dependency chain in order (own extension
    /// last). A base-config file sees only the base; an extension file sees the
    /// base, its declared transitive dependencies and itself — never an
    /// unrelated sibling.
    fn get_all_configurations(
        &self,
        file_id: FileId,
    ) -> Vec<(Option<String>, Arc<bsl_metadata::Configuration>)>;

    /// EVERY registered root — the base, all extensions, then the external
    /// objects — regardless of any file's visibility: the inventory view for
    /// index/graph builders that need the whole workspace, keyed by kind and
    /// name. Semantic per-file resolution must use
    /// [`Self::get_all_configurations`] instead: this union deliberately ignores
    /// dependency-scoped visibility.
    fn all_configurations_inventory(
        &self,
    ) -> Vec<(Option<String>, Arc<bsl_metadata::Configuration>)>;

    /// Every registered root: the base, the extensions and the external objects.
    /// What scanning, watching and the metadata substrate enumerate.
    fn all_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)>;

    /// The external root owning `path`, when the file lies in an EPF/ERF export:
    /// the root's configured and canonical spellings and the export's kind.
    fn external_root_of_path(
        &self,
        path: &std::path::Path,
    ) -> Option<(std::path::PathBuf, std::path::PathBuf, bsl_metadata::ExternalObjectKind)>;

    /// The base and its extensions only, in topological order: the designer's
    /// view of the project. External objects are leaves outside it: code sees
    /// them from their own files alone, so module pairing, override order and
    /// the semantic inventory of configurations leave them out. A consumer
    /// keyed by `(kind, name)` — the name dictionary, navigation to an object's
    /// XML — reads [`Self::all_config_paths`] instead: an external kind never
    /// names a designer object, so its roots add entries without shadowing any.
    fn designer_config_paths(&self) -> Vec<(Option<String>, std::path::PathBuf)>;

    /// Topological rank and extension label of the root owning `file_id`, using
    /// the same configured/canonical longest-prefix rule as visibility.
    fn config_root_rank_and_label(&self, file_id: FileId) -> Option<(usize, Option<String>)>;

    /// The vendor support map, when `file_id` lies in the main configuration and
    /// the map is loaded.
    fn vendor_support_for_file(&self, file_id: FileId) -> Option<Arc<bsl_metadata::VendorSupport>>;

    /// Topological ranks of roots visible to `file_id`, or `None` when the
    /// workspace has no configured root visibility.
    fn visible_config_root_ranks(&self, file_id: FileId) -> Option<Vec<usize>>;

    /// The common module that owns the `Ext/Module.bsl` whose id is `module_file_id`
    /// (typically the file being analysed), or `None` if it is not a common module's
    /// body. Per-common-module Salsa granularity when the substrate is populated.
    fn common_module_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::CommonModule>>;

    /// EventSubscription names owned by the main configuration only.
    fn main_event_subscription_names_for_file(&self, file_id: FileId) -> Vec<String>;

    fn http_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::HTTPService>>;

    fn web_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::WebService>>;

    fn integration_service_for_file_id(
        &self,
        module_file_id: FileId,
    ) -> Option<Arc<bsl_metadata::IntegrationService>>;

    /// The `Ext/Module.bsl` bodies of the common module `name` visible to `file_id`
    /// (base + the file's own extension), each carrying whether its bytes could be
    /// read. For method/parameter validation that must read the module body, scoped
    /// extension-private like the metadata.
    ///
    /// The composition rather than a plain list of files, because a consumer that
    /// cannot see an unread body decides "the module has no such method" from bodies
    /// that were never entitled to answer.
    ///
    /// This is the MERGED surface, ordered extension-first — not priority order. Walk
    /// it with [`hir::CommonModuleBodies::search_merged_surface`]; `search` would stop
    /// at "the first" unread body, and in this order the unread one can be last.
    fn resolve_common_module_files(&self, file_id: FileId, name: &str) -> hir::CommonModuleBodies;

    /// Uncached application-host path lookup used only behind the tracked
    /// `application_module_files_query` aggregation.
    fn resolve_application_module_files_uncached(
        &self,
        file_id: FileId,
        kind: hir::ApplicationModuleKind,
    ) -> Option<hir::CommonModuleBodies>;

    fn all_sdbl_in_file(
        &self,
        file_id: FileId,
    ) -> Arc<Vec<(hir::SdblExprId, syntax::SdblQueryInfo)>>;

    fn sdbl_hir_in_file(&self, file_id: FileId) -> SdblHirEntries;

    fn reaching_definitions(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>>;

    fn method_path_terminates(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::path_terminates::PathTerminatesResult>>;

    fn method_security_state(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>;

    fn module_code_security_state(
        &self,
        file_id: FileId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>;

    fn module_level_cfg(&self, module_id: hir::ModuleId) -> Arc<hir::cfg::ControlFlowGraph>;

    fn line_index(&self, file_id_input: base_db::FileIdInput) -> Arc<line_index::LineIndex>;

    fn as_any(&self) -> &dyn std::any::Any;

    /// The Salsa-tracked config revision token for the root owning `path` (see
    /// [`RootDatabaseImpl::config_root_revision_for_path`](crate::RootDatabaseImpl::config_root_revision_for_path)).
    fn config_root_revision_for_path(&self, path: &std::path::Path) -> u32;

    /// The config roots visible to `file_id`: the base plus the file's
    /// dependency-ordered extension chain (own extension last). `None` when no
    /// roots are registered or the file's path is unknown.
    fn visible_roots_for_file(&self, file_id: FileId) -> Option<crate::VisibleRoots>;
}
