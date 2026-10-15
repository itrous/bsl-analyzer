use crate::DiagnosticsConfig;
use std::sync::Arc;
use vfs::FileId;

/// What a check may know about the file without reading its positions: the
/// configuration, metadata, cross-file resolution, docs, the module's
/// declarations and the per-method dataflow. A body check runs on this alone
/// (see [`crate::BodyContext`]), so nothing it reads changes when the method
/// it checks moves within the file. Cross-file lookups (`symbol_tree_for`,
/// `module_bodies_for`) do carry positions of OTHER files; that is the same
/// trade inference makes.
pub struct AnalysisContext<'a> {
    pub config: &'a DiagnosticsConfig,
    pub file_id: FileId,
    provider: &'a dyn ide_db::AnalysisProvider,
}

/// The whole-file view: [`AnalysisContext`] plus the file's positional
/// state (its parse, text, item tree, symbol tree, bodies, call summary).
/// Only file-level checks and the assembly of a file's diagnostics see it.
pub struct DiagnosticsContext<'a> {
    analysis: AnalysisContext<'a>,
}

impl<'a> std::ops::Deref for DiagnosticsContext<'a> {
    type Target = AnalysisContext<'a>;

    fn deref(&self) -> &Self::Target {
        &self.analysis
    }
}

impl<'a> DiagnosticsContext<'a> {
    pub fn new(
        config: &'a DiagnosticsConfig,
        file_id: FileId,
        provider: &'a dyn ide_db::AnalysisProvider,
    ) -> Self {
        Self { analysis: AnalysisContext::new(config, file_id, provider) }
    }

    pub fn analysis(&self) -> &AnalysisContext<'a> {
        &self.analysis
    }

    fn query<T>(&self, f: impl FnOnce(&dyn ide_db::AnalysisProvider) -> T) -> T {
        f(self.analysis.provider)
    }
    pub fn parse(&self) -> syntax::Parse<syntax::SyntaxNode> {
        self.query(|p| p.parse(self.file_id))
    }

    pub fn module_bodies(&self) -> Arc<hir::ModuleBodies> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.module_bodies(module_id))
    }

    pub fn infer(&self) -> Arc<hir::InferenceResult> {
        self.query(|p| p.infer(self.file_id))
    }

    pub fn symbol_tree(&self) -> Arc<hir::SymbolTree> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.symbol_tree_for(module_id)
    }

    pub fn call_summary(&self, module_id: hir::ModuleId) -> Arc<hir::ModuleCallSummary> {
        self.query(|p| p.call_summary(module_id))
    }

    pub fn item_tree(&self) -> Arc<hir::ItemTree> {
        self.query(|p| p.item_tree(self.file_id))
    }

    pub fn file_text(&self) -> Arc<String> {
        Arc::new(self.query(|p| p.file_text(self.file_id)))
    }

    pub fn line_index(&self) -> Arc<line_index::LineIndex> {
        self.query(|p| p.line_index(self.file_id))
    }

    pub fn region_tree(&self) -> Arc<hir::RegionTree> {
        self.query(|p| p.region_tree(self.file_id))
    }

    pub fn sdbl_hir_in_file(&self) -> ide_db::SdblHirEntries {
        self.query(|p| p.sdbl_hir_in_file(self.file_id))
    }

    pub fn all_sdbl_in_file(&self) -> Arc<Vec<(hir::SdblExprId, syntax::SdblQueryInfo)>> {
        self.query(|p| p.all_sdbl_in_file(self.file_id))
    }

    pub fn file_external_refs(&self) -> std::sync::Arc<Vec<hir::ExternalRef>> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.file_external_refs(module_id))
    }

    pub fn module_bodies_for(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleBodies> {
        self.query(|p| p.module_bodies(module_id))
    }
}

impl<'a> AnalysisContext<'a> {
    pub fn new(
        config: &'a DiagnosticsConfig,
        file_id: FileId,
        provider: &'a dyn ide_db::AnalysisProvider,
    ) -> Self {
        Self { config, file_id, provider }
    }

    pub(crate) fn provider(&self) -> &'a dyn ide_db::AnalysisProvider {
        self.provider
    }

    /// One declaration of this file's module, by key.
    pub fn interface_method(&self, method_id: hir::MethodId) -> Option<Arc<hir::MethodDecl>> {
        self.query(|p| p.interface_method(method_id))
    }

    /// The first method of this file's module named `name`.
    pub fn interface_method_named(&self, name: &hir::Name) -> Option<Arc<hir::MethodDecl>> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.interface_method_named(module_id, name))
    }

    /// The first module variable of this file's module named `name`.
    pub fn interface_variable_named(&self, name: &hir::Name) -> Option<Arc<hir::VariableDecl>> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.interface_variable_named(module_id, name))
    }

    pub fn infer_owner(&self, owner: hir::DefWithBodyId) -> hir::InferOwnerResult {
        self.query(|p| p.infer_owner(self.file_id, owner))
    }

    pub fn arg_diagnostics_of(
        &self,
        owner: hir::DefWithBodyId,
    ) -> Arc<Vec<hir::InferenceDiagnostic>> {
        self.query(|p| p.arg_diagnostics_of(self.file_id, owner))
    }

    pub fn module_recursive_methods(&self) -> Arc<rustc_hash::FxHashSet<hir::MethodKey>> {
        self.query(|p| p.module_recursive_methods(self.file_id))
    }

    pub fn module_level_cfg(&self) -> Arc<hir::cfg::ControlFlowGraph> {
        self.query(|p| p.module_level_cfg(self.file_id))
    }

    pub fn module_code_reaching_definitions(
        &self,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        self.query(|p| p.module_code_reaching_definitions(self.file_id))
    }

    /// Metrics of the module-level code, if it has any statements.
    pub fn module_code_hir_metrics(&self) -> Option<Arc<hir::metrics::HirMethodMetrics>> {
        self.query(|p| p.module_hir_metrics(self.file_id)).module_code()
    }

    pub fn module_code_security_state(
        &self,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        self.query(|p| p.module_code_security_state(self.file_id))
    }

    pub fn locale(&self) -> base_db::Locale {
        self.config.locale
    }

    pub fn kernel_type_display(&self, id: hir::TypeId, locale: base_db::Locale) -> String {
        self.provider.kernel_type_display(id, locale)
    }

    /// The project's minimum platform release, parsed; `None` when unset or not a
    /// version.
    pub fn min_platform_version(&self) -> Option<bsl_platform::PlatformVersion> {
        let value = self.provider.min_platform_version()?;
        bsl_platform::PlatformVersion::parse_release(&value)
    }

    /// The compatibility mode the code compiles under; `None` when it hides
    /// nothing (unknown, `DontUse`, or not a mode).
    pub fn compatibility_mode(&self) -> Option<bsl_platform::PlatformVersion> {
        hir::compat_mode::effective_mode(self.provider.compatibility_mode().as_deref())
    }

    /// Project settings for which environments a call is compiled into.
    pub fn env_options(&self) -> hir::execution_env::EnvOptions {
        self.provider.env_options()
    }

    pub fn main_configuration(&self) -> Option<Arc<bsl_metadata::Configuration>> {
        self.visible_configurations()
            .into_iter()
            .find(|vc| vc.config.name.is_none())
            .map(|vc| vc.config.configuration)
    }

    pub fn visible_configurations(&self) -> Vec<ide_db::provider::VisibleConfigWithRoot> {
        self.provider.visible_configurations(self.file_id)
    }

    /// The `Ext/Module.bsl` bodies of the common module `name` visible to this file —
    /// base + its own extension, each carrying whether it could be read. For
    /// diagnostics that read the module body (handler method existence/export,
    /// required parameters). Scoped extension-private through the per-common-module
    /// substrate.
    ///
    /// Look things up with [`hir::CommonModuleBodies::search_merged_surface`]: this is
    /// the merged surface, not priority order, and a diagnostic that concludes "the
    /// module has no such method" must know that every body was actually readable, or
    /// it accuses the caller of an absence nobody established.
    pub fn common_module_bodies(&self, name: &str) -> hir::CommonModuleBodies {
        self.provider.resolve_common_module_files(self.file_id, name)
    }

    /// The common module `name` visible to this file (base + its own extension),
    /// at per-common-module Salsa granularity. Replaces the former all-configs
    /// `find_common_module_anywhere`/`is_common_module_anywhere` scans, which were
    /// over-permissive (a sibling extension's module was visible) and depended on
    /// the whole configuration.
    pub fn resolve_common_module(&self, name: &str) -> Option<Arc<bsl_metadata::CommonModule>> {
        self.provider.resolve_common_module(self.file_id, name)
    }

    /// Resolve one EventSubscription visible to this file through the provider's
    /// single-name API. Salsa providers route this through the typed substrate.
    pub fn resolve_event_subscription(
        &self,
        name: &str,
    ) -> Option<Arc<bsl_metadata::EventSubscription>> {
        self.provider.resolve_event_subscription(self.file_id, name)
    }

    /// Main-configuration EventSubscription enumeration for diagnostics that need
    /// to scan declared subscriptions while preserving previous main-only behavior.
    pub fn main_event_subscriptions(&self) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        self.provider.main_event_subscriptions(self.file_id)
    }

    /// All event subscriptions visible to this file through its base and
    /// extension dependency chain.
    pub fn visible_event_subscriptions(&self) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        self.provider.visible_event_subscriptions(self.file_id)
    }

    /// Main-configuration Role enumeration for diagnostics that need to scan
    /// declared roles while preserving previous main-only behavior.
    pub fn main_roles(&self) -> Vec<Arc<bsl_metadata::Role>> {
        self.provider.main_roles(self.file_id)
    }

    /// Main-configuration ScheduledJob enumeration for diagnostics that need to
    /// scan declared jobs while preserving previous main-only behavior.
    pub fn main_scheduled_jobs(&self) -> Vec<Arc<bsl_metadata::ScheduledJob>> {
        self.provider.main_scheduled_jobs(self.file_id)
    }

    pub fn is_common_module_anywhere(&self, name: &str) -> bool {
        self.resolve_common_module(name).is_some()
    }

    pub fn assignment_target_kind(&self, name: &str) -> hir::AssignmentResolution {
        self.provider.assignment_target_kind(self.file_id, name)
    }

    pub fn file_path(&self) -> Option<String> {
        self.provider.file_path(self.file_id)
    }

    /// The common module this file is the body of (its `Ext/Module.bsl`), if any.
    /// Routes through the per-common-module reverse index when the substrate is
    /// populated, so it depends on just that module rather than the whole config.
    pub fn common_module_for_file(&self) -> Option<Arc<bsl_metadata::CommonModule>> {
        self.provider.common_module_for_file(self.file_id)
    }

    /// The metadata object `(mdo_type, name)` visible to this file, resolved
    /// per-MDO (base + the file's own extension) so it depends on just that object.
    pub fn resolve_metadata_object(
        &self,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        self.provider.resolve_metadata_object(self.file_id, mdo_type, name)
    }

    fn query<T>(&self, f: impl FnOnce(&dyn ide_db::AnalysisProvider) -> T) -> T {
        f(self.provider)
    }

    /// Whether this file is the module of an ORDINARY form (a `Form.bin` dialog):
    /// thick-client code, where `ЭтаФорма`,
    /// `ПолучитьФорму`, modal dialogs and synchronous calls are the platform's own
    /// means rather than smells to report.
    pub fn is_ordinary_form_module(&self) -> bool {
        self.module_metadata().is_ordinary_form_module()
    }

    pub fn module_metadata(&self) -> Arc<hir::ModuleMetadata> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.module_metadata(module_id))
    }

    pub fn module_implicit_field_names(&self) -> Vec<String> {
        self.query(|p| p.module_implicit_field_names(self.file_id))
    }

    pub fn symbol_tree_for(&self, module_id: hir::ModuleId) -> Arc<hir::SymbolTree> {
        self.query(|p| p.symbol_tree(module_id))
    }

    pub fn source_root_id(&self) -> base_db::SourceRootId {
        self.query(|p| p.file_source_root_id(self.file_id))
    }

    pub fn module_index(&self) -> Arc<hir::ModuleIndex> {
        let source_root_id = self.source_root_id();
        self.query(|p| p.module_index(source_root_id))
    }

    pub fn method_cfg(&self, method_id: hir::MethodId) -> Arc<hir::cfg::ControlFlowGraph> {
        self.query(|p| p.method_cfg(method_id))
    }

    pub fn method_path_terminates(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::path_terminates::PathTerminatesResult>> {
        self.query(|p| p.method_path_terminates(method_id))
    }

    pub fn method_security_state(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        self.query(|p| p.method_security_state(method_id))
    }

    pub fn method_effect_summary(
        &self,
        method: hir::MethodId,
    ) -> std::sync::Arc<hir::dataflow::effect_summary::EffectSummary> {
        self.query(|p| p.method_effect_summary(method))
    }

    pub fn method_hir_metrics(
        &self,
        method_id: hir::MethodId,
    ) -> Arc<hir::metrics::HirMethodMetrics> {
        self.query(|p| p.method_hir_metrics(method_id))
    }

    /// Metrics of the module-level code, if it has any statements.
    pub fn method_cyclomatic(&self, method_id: hir::MethodId) -> u32 {
        self.query(|p| p.method_cyclomatic(method_id))
    }

    pub fn module_data(&self) -> Arc<hir::ModuleData> {
        let module_id = hir::ModuleId::new(self.file_id);
        self.query(|p| p.module_data(module_id))
    }

    pub fn method_docs(&self, method_id: hir::MethodId) -> Option<Arc<hir::MethodDocs>> {
        self.query(|p| p.method_docs(method_id))
    }

    pub fn variable_docs(&self, variable_id: hir::VariableId) -> Option<Arc<hir::VariableDocs>> {
        self.query(|p| p.variable_docs(variable_id))
    }

    pub fn reaching_definitions(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        self.query(|p| p.reaching_definitions(method_id))
    }

    pub fn resolve_vfs_path(
        &self,
        source_root_id: base_db::SourceRootId,
        vfs_path: &vfs::VfsPath,
    ) -> Option<vfs::FileId> {
        self.query(|p| p.resolve_vfs_path(source_root_id, vfs_path))
    }

    pub fn resolve_vfs_path_ci(
        &self,
        source_root_id: base_db::SourceRootId,
        candidate: &std::path::Path,
        tail_modes: &[bsl_conventions::SegmentMatch],
    ) -> Option<vfs::FileId> {
        self.query(|p| p.resolve_vfs_path_ci(source_root_id, candidate, tail_modes))
    }

    pub fn resolve_qualified_path(
        &self,
        module_name: &hir::Name,
        method_name: &hir::Name,
    ) -> hir::PathResolution {
        let module_index = self.module_index();

        let target_file_id = match module_index.resolve_common_module(module_name) {
            Some(id) => id,
            None => {
                return hir::PathResolution::Unresolved(hir::QualifiedName::from_segments([
                    module_name.clone(),
                    method_name.clone(),
                ]));
            }
        };

        let target_module_id = hir::ModuleId::new(target_file_id);
        let symbol_tree = self.symbol_tree_for(target_module_id);

        if let Some(method_symbol) = symbol_tree.find_method(method_name) {
            if method_symbol.is_export {
                return hir::PathResolution::Method(method_symbol.id);
            }
        }

        hir::PathResolution::Unresolved(hir::QualifiedName::from_segments([
            module_name.clone(),
            method_name.clone(),
        ]))
    }

    pub fn resolve_platform_global_member(
        &self,
        module_name: &hir::Name,
        method_name: &hir::Name,
    ) -> Option<bsl_platform::PlatformMethod> {
        let data = bsl_platform::PlatformDataInner::instance();
        data.resolve_global_member(module_name.as_str(), method_name.as_str()).cloned()
    }

    pub fn find_common_module_file(
        &self,
        common_module: &bsl_metadata::CommonModule,
    ) -> Option<vfs::FileId> {
        use bsl_metadata::traits::{MdObject, Module};

        let uri = common_module.uri()?;
        let file_id = self.resolve_module_file(uri);

        if file_id.is_none() {
            tracing::warn!(
                module = %common_module.name(),
                uri = %uri,
                "CommonModule file not found in VFS"
            );
        }

        file_id
    }

    pub fn resolve_module_file(&self, relative_uri: &str) -> Option<vfs::FileId> {
        self.query(|p| p.resolve_module_file(relative_uri))
    }

    pub fn config_int(&self, code: crate::DiagnosticCode, param: &str, default: i64) -> i64 {
        self.config.get_int(code, param).unwrap_or(default)
    }

    pub fn config_bool(&self, code: crate::DiagnosticCode, param: &str, default: bool) -> bool {
        self.config.get_bool(code, param).unwrap_or(default)
    }

    pub fn config_string(&self, code: crate::DiagnosticCode, param: &str, default: &str) -> String {
        self.config.get_string(code, param).unwrap_or(default).to_string()
    }

    pub fn severity(&self, code: crate::DiagnosticCode) -> crate::Severity {
        self.config.severity(code)
    }

    pub fn tags(&self, code: crate::DiagnosticCode) -> Vec<crate::DiagnosticTag> {
        self.config.tags(code)
    }

    pub fn is_disabled_with_metadata(&self, code: crate::DiagnosticCode) -> bool {
        self.config.is_disabled(code)
    }
}
