use std::sync::Arc;

use base_db::{FileIdInput, SourceRootId};
use bsl_metadata::Configuration;
use hir::{
    AssignmentResolution, DefWithBodyId, HirDatabase, InferenceDiagnostic, InferenceResult,
    ItemTree, MethodId, ModuleBodies, ModuleId, ModuleIndex, ModuleMetadata, Name, Resolver,
    SymbolTree,
};
use syntax::{Parse, SyntaxNode};
use vfs::FileId;

use crate::{
    metadata::{intern_configuration_path, ConfigurationPathInput},
    provider::{AnalysisProvider, VisibleConfigWithRoot},
    RootDatabase,
};

/// Routes the seven local per-module queries (`parse` / `file_text` / `item_tree` /
/// `symbol_tree` / `module_bodies` / `infer` / `line_index`) of the extension file to the
/// spliced `&ИзменениеИКонтроль` effective module, so the diagnostics inference pass sees a
/// coherent merged source map. Everything else stays on the base/extension context.
#[derive(Clone, Copy)]
struct EffectiveRoute<'db> {
    eid: hir::EffectiveModuleId<'db>,
    ext_file: FileId,
}

/// Routes the extension file's `infer` query to weaving inference (base module as
/// same-module sibling fallback), so the diagnostics pass over a `&Вместо` / `&Перед` /
/// `&После` module sees the base methods it targets. Unlike [`EffectiveRoute`] there is
/// no text splice, so only `infer` is routed; parse / symbol-tree stay the ext file's.
#[derive(Clone, Copy)]
struct WeavingRoute<'db> {
    wid: hir::WeavingModuleId<'db>,
    ext_file: FileId,
}

pub struct SalsaProvider<'db> {
    db: &'db dyn RootDatabase,
    configuration_path_input: Option<ConfigurationPathInput<'db>>,
    file_set: Option<&'db vfs::file_set::FileSet>,
    effective: Option<EffectiveRoute<'db>>,
    weaving: Option<WeavingRoute<'db>>,
}

impl<'db> SalsaProvider<'db> {
    pub fn new(
        db: &'db dyn RootDatabase,
        configuration_path_input: Option<ConfigurationPathInput<'db>>,
    ) -> Self {
        Self { db, configuration_path_input, file_set: None, effective: None, weaving: None }
    }

    pub fn with_file_set(
        db: &'db dyn RootDatabase,
        configuration_path_input: Option<ConfigurationPathInput<'db>>,
        file_set: Option<&'db vfs::file_set::FileSet>,
    ) -> Self {
        Self { db, configuration_path_input, file_set, effective: None, weaving: None }
    }

    /// Route the extension file's local queries to its effective `&ИзменениеИКонтроль`
    /// module. Used only by the diagnostics inference pass; all other consumers keep the
    /// default (no-op) provider so behaviour is byte-identical for ordinary modules.
    pub fn with_effective(mut self, eid: hir::EffectiveModuleId<'db>, ext_file: FileId) -> Self {
        self.effective = Some(EffectiveRoute { eid, ext_file });
        self
    }

    /// Route the extension file's `infer` query to its weaving inference. Used only by the
    /// diagnostics inference pass; all other consumers keep the default provider so
    /// behaviour is byte-identical for ordinary modules.
    pub fn with_weaving(mut self, wid: hir::WeavingModuleId<'db>, ext_file: FileId) -> Self {
        self.weaving = Some(WeavingRoute { wid, ext_file });
        self
    }

    pub fn db(&self) -> &'db dyn RootDatabase {
        self.db
    }

    /// The effective id when `file_id` is the routed extension file.
    fn effective_for(&self, file_id: FileId) -> Option<hir::EffectiveModuleId<'db>> {
        self.effective.filter(|r| r.ext_file == file_id).map(|r| r.eid)
    }

    /// The weaving id when `file_id` is the routed extension file.
    fn weaving_for(&self, file_id: FileId) -> Option<hir::WeavingModuleId<'db>> {
        self.weaving.filter(|r| r.ext_file == file_id).map(|r| r.wid)
    }
}

impl AnalysisProvider for SalsaProvider<'_> {
    fn configuration(&self) -> Option<Arc<Configuration>> {
        let path_input = self.configuration_path_input?;
        Some(self.db.load_configuration(path_input))
    }

    fn visible_configurations(&self, file_id: FileId) -> Vec<VisibleConfigWithRoot> {
        let load = |name: Option<String>, path: std::path::PathBuf| {
            let path_input = intern_configuration_path(
                self.db,
                &path.to_string_lossy(),
                self.db.config_root_revision_for_path(&path),
            );
            let configuration = self.db.load_configuration(path_input);
            VisibleConfigWithRoot {
                config: bsl_config::VisibleConfig { name, configuration },
                root: path,
            }
        };

        // Only the roots the FILE may see: base + its dependency chain. An
        // unrelated sibling extension must not leak into per-file diagnostics
        // even though it is registered in the workspace.
        match self.db.visible_roots_for_file(file_id) {
            Some(roots) => roots
                .main
                .into_iter()
                .map(|path| load(None, path))
                .chain(roots.chain.into_iter().map(|(name, path)| load(Some(name), path)))
                .collect(),
            None => match self.configuration_path_input {
                Some(path_input) => {
                    let root = std::path::PathBuf::from(path_input.path(self.db));
                    vec![VisibleConfigWithRoot {
                        config: bsl_config::VisibleConfig {
                            name: None,
                            configuration: self.db.load_configuration(path_input),
                        },
                        root,
                    }]
                }
                None => Vec::new(),
            },
        }
    }

    fn common_module_for_file(&self, file_id: FileId) -> Option<Arc<bsl_metadata::CommonModule>> {
        self.db.common_module_for_file_id(file_id)
    }

    fn resolve_metadata_object(
        &self,
        file_id: FileId,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<Arc<bsl_metadata::MetadataObject>> {
        self.db.resolve_metadata_object(file_id, mdo_type, name)
    }

    fn resolve_common_module(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::CommonModule>> {
        self.db.resolve_common_module(file_id, name)
    }

    fn resolve_event_subscription(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::EventSubscription>> {
        self.db.resolve_event_subscription(file_id, name)
    }

    fn resolve_role(&self, file_id: FileId, name: &str) -> Option<Arc<bsl_metadata::Role>> {
        self.db.resolve_role(file_id, name)
    }

    fn main_event_subscriptions(
        &self,
        file_id: FileId,
    ) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        self.db
            .main_event_subscription_names_for_file(file_id)
            .into_iter()
            .filter_map(|name| self.db.resolve_event_subscription(file_id, &name))
            .collect()
    }

    fn visible_event_subscriptions(
        &self,
        file_id: FileId,
    ) -> Vec<Arc<bsl_metadata::EventSubscription>> {
        self.db
            .event_subscription_names(file_id)
            .into_iter()
            .filter_map(|name| self.db.resolve_event_subscription(file_id, &name))
            .collect()
    }

    fn main_roles(&self, file_id: FileId) -> Vec<Arc<bsl_metadata::Role>> {
        if self.db.all_config_paths().is_empty() {
            return self
                .visible_configurations(file_id)
                .into_iter()
                .find(|visible| visible.config.name.is_none())
                .map(|visible| {
                    visible.config.configuration.roles().iter().cloned().map(Arc::new).collect()
                })
                .unwrap_or_default();
        }

        self.db
            .role_names(file_id)
            .into_iter()
            .filter_map(|name| self.db.resolve_role(file_id, &name))
            .collect()
    }

    fn resolve_scheduled_job(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::ScheduledJob>> {
        self.db.resolve_scheduled_job(file_id, name)
    }

    fn main_scheduled_jobs(&self, file_id: FileId) -> Vec<Arc<bsl_metadata::ScheduledJob>> {
        self.db
            .scheduled_job_names(file_id)
            .into_iter()
            .filter_map(|name| self.db.resolve_scheduled_job(file_id, &name))
            .collect()
    }

    fn resolve_common_module_files(&self, file_id: FileId, name: &str) -> hir::CommonModuleBodies {
        self.db.resolve_common_module_files(file_id, name)
    }

    fn module_members(&self, source_root_id: SourceRootId) -> Arc<hir::WorkspaceMembers> {
        self.db.module_members(source_root_id)
    }

    fn workspace_foreign_handler_bindings(
        &self,
        source_root_id: SourceRootId,
    ) -> Arc<hir::ForeignHandlerBindings> {
        self.db.workspace_foreign_handler_bindings(source_root_id)
    }

    fn module_index(&self, source_root_id: SourceRootId) -> Arc<ModuleIndex> {
        self.db.module_index(source_root_id)
    }

    fn assignment_target_kind(&self, file_id: FileId, name: &str) -> AssignmentResolution {
        let module_id = ModuleId::new(file_id);
        let resolver = Resolver::for_module(module_id);
        resolver.resolve_assignment_target(self.db, &Name::new(name))
    }

    fn kernel_type_display(&self, id: bsl_types::kind::TypeId, locale: base_db::Locale) -> String {
        hir::kernel_type_label(self.db, id, locale, false)
    }

    fn min_platform_version(&self) -> Option<Arc<str>> {
        HirDatabase::min_platform_version(self.db)
    }

    fn module_implicit_field_names(&self, file_id: FileId) -> Vec<String> {
        hir::module_implicit_field_names(self.db, file_id)
    }

    fn parse(&self, file_id: FileId) -> Parse<SyntaxNode> {
        match self.effective_for(file_id) {
            Some(eid) => hir::parse_effective(self.db, eid),
            None => self.db.parse(file_id),
        }
    }

    fn file_text(&self, file_id: FileId) -> String {
        match self.effective_for(file_id).and_then(|eid| hir::effective_module_text(self.db, eid)) {
            Some(em) => em.text.to_string(),
            None => self.db.file_text(file_id).to_string(),
        }
    }

    fn item_tree(&self, file_id: FileId) -> Arc<ItemTree> {
        match self.effective_for(file_id) {
            Some(eid) => hir::item_tree_effective(self.db, eid),
            None => self.db.item_tree(file_id),
        }
    }

    fn symbol_tree(&self, module_id: ModuleId) -> Arc<SymbolTree> {
        match self.effective_for(module_id.file_id) {
            Some(eid) => hir::symbol_tree_effective(self.db, eid),
            None => self.db.symbol_tree(module_id),
        }
    }

    fn module_bodies(&self, module_id: ModuleId) -> Arc<ModuleBodies> {
        match self.effective_for(module_id.file_id) {
            Some(eid) => hir::module_bodies_effective(self.db, eid),
            None => self.db.module_bodies(module_id),
        }
    }

    fn infer(&self, file_id: FileId) -> Arc<InferenceResult> {
        // Effective `&ИзменениеИКонтроль` routing takes precedence over weaving when both
        // somehow match (they are set on separate provider instances, so this ordering is
        // only defensive).
        if let Some(eid) = self.effective_for(file_id) {
            return hir::infer_effective(self.db, eid);
        }
        if let Some(wid) = self.weaving_for(file_id) {
            return hir::infer_weaving(self.db, wid);
        }
        HirDatabase::infer(self.db, file_id)
    }

    fn module_interface(&self, module_id: ModuleId) -> Arc<hir::ModuleInterface> {
        match self.effective_for(module_id.file_id) {
            Some(eid) => hir::module_interface_effective(self.db, eid),
            None => self.db.module_interface(module_id),
        }
    }

    // An effective module's interface is one value, so its declarations are
    // read off it whole; a plain module's come one memo per declaration.
    fn interface_method(&self, method_id: MethodId) -> Option<Arc<hir::MethodDecl>> {
        match self.effective_for(method_id.module.file_id) {
            Some(eid) => hir::module_interface_effective(self.db, eid)
                .find_method_by_id_shared(method_id)
                .cloned(),
            None => self.db.interface_method(method_id),
        }
    }

    fn interface_method_named(
        &self,
        module_id: ModuleId,
        name: &hir::Name,
    ) -> Option<Arc<hir::MethodDecl>> {
        match self.effective_for(module_id.file_id) {
            Some(eid) => hir::module_interface_effective(self.db, eid)
                .find_method_shared(intern::NormName::intern(name.as_str()))
                .cloned(),
            None => self.db.interface_method_named(module_id, name),
        }
    }

    fn interface_variable_named(
        &self,
        module_id: ModuleId,
        name: &hir::Name,
    ) -> Option<Arc<hir::VariableDecl>> {
        match self.effective_for(module_id.file_id) {
            Some(eid) => hir::module_interface_effective(self.db, eid)
                .find_variable_shared(intern::NormName::intern(name.as_str()))
                .cloned(),
            None => self.db.interface_variable_named(module_id, name),
        }
    }

    fn method_syntax(&self, method_id: MethodId) -> Option<SyntaxNode> {
        let input = hir::MethodIdInput::new(self.db, method_id);
        hir::method_syntax_query(self.db, input).as_ref().map(|syntax| syntax.detached_root())
    }

    fn infer_owner(&self, file_id: FileId, owner: DefWithBodyId) -> hir::InferOwnerResult {
        hir::infer_owner(self.db, file_id, owner)
    }

    fn arg_diagnostics_of(
        &self,
        file_id: FileId,
        owner: DefWithBodyId,
    ) -> Arc<Vec<InferenceDiagnostic>> {
        match owner {
            DefWithBodyId::Method(local_id) => {
                let method_id = MethodId { module: ModuleId::new(file_id), local_id };
                let input = hir::MethodIdInput::new(self.db, method_id);
                HirDatabase::method_arg_diagnostics(self.db, input)
            }
            DefWithBodyId::ModuleCode => HirDatabase::module_code_arg_diagnostics(self.db, file_id),
        }
    }

    fn module_level_cfg(&self, file_id: FileId) -> Arc<hir::cfg::ControlFlowGraph> {
        self.db.module_level_cfg(ModuleId::new(file_id))
    }

    fn module_recursive_methods(
        &self,
        file_id: FileId,
    ) -> Arc<rustc_hash::FxHashSet<hir::MethodKey>> {
        let input = FileIdInput::new(self.db, file_id);
        crate::queries::module_recursive_methods_query(self.db, input)
    }

    fn module_code_reaching_definitions(
        &self,
        file_id: FileId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        HirDatabase::module_code_reaching_definitions(self.db, file_id)
    }

    fn module_metadata(&self, module_id: ModuleId) -> Arc<ModuleMetadata> {
        self.db.module_metadata(module_id)
    }

    fn call_summary(&self, module_id: ModuleId) -> Arc<hir::ModuleCallSummary> {
        self.db.module_call_summary(module_id)
    }

    fn line_index(&self, file_id: FileId) -> Arc<line_index::LineIndex> {
        if let Some(em) =
            self.effective_for(file_id).and_then(|eid| hir::effective_module_text(self.db, eid))
        {
            return Arc::new(line_index::LineIndex::new(&em.text));
        }
        let input = FileIdInput::new(self.db, file_id);
        self.db.line_index(input)
    }

    fn file_source_root_id(&self, file_id: FileId) -> SourceRootId {
        self.db.file_source_root_input(file_id).source_root_id(self.db)
    }

    fn method_cfg(&self, method_id: hir::MethodId) -> Arc<hir::cfg::ControlFlowGraph> {
        self.db.method_cfg(method_id)
    }

    fn method_path_terminates(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::path_terminates::PathTerminatesResult>> {
        self.db.method_path_terminates(method_id)
    }

    fn region_tree(&self, file_id: FileId) -> Arc<hir::RegionTree> {
        self.db.region_tree(file_id)
    }

    fn sdbl_hir_in_file(&self, file_id: FileId) -> crate::SdblHirEntries {
        self.db.sdbl_hir_in_file(file_id)
    }

    fn all_sdbl_in_file(
        &self,
        file_id: FileId,
    ) -> Arc<Vec<(hir::SdblExprId, syntax::SdblQueryInfo)>> {
        self.db.all_sdbl_in_file(file_id)
    }

    fn module_data(&self, module_id: ModuleId) -> Arc<hir::ModuleData> {
        self.db.module_data(module_id)
    }

    fn method_docs(&self, method_id: hir::MethodId) -> Option<Arc<hir::MethodDocs>> {
        self.db.method_docs(method_id)
    }

    fn variable_docs(&self, variable_id: hir::VariableId) -> Option<Arc<hir::VariableDocs>> {
        self.db.variable_docs(variable_id)
    }

    fn reaching_definitions(
        &self,
        method_id: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        self.db.reaching_definitions(method_id)
    }

    fn file_external_refs(&self, module_id: ModuleId) -> std::sync::Arc<Vec<hir::ExternalRef>> {
        self.db.file_external_refs(module_id)
    }

    fn resolve_vfs_path(
        &self,
        source_root_id: base_db::SourceRootId,
        vfs_path: &vfs::VfsPath,
    ) -> Option<FileId> {
        self.db.resolve_vfs_path(source_root_id, vfs_path)
    }

    fn resolve_vfs_path_ci(
        &self,
        source_root_id: base_db::SourceRootId,
        candidate: &std::path::Path,
        tail_modes: &[bsl_conventions::SegmentMatch],
    ) -> Option<FileId> {
        base_db::resolve_vfs_path_ci_query(
            self.db,
            self.db.source_root_input(source_root_id),
            candidate.to_string_lossy().into_owned(),
            tail_modes,
        )
    }

    fn resolve_module_file(&self, relative_uri: &str) -> Option<FileId> {
        let config_path_input = self.configuration_path_input?;
        let config_root = config_path_input.path(self.db);
        let full_path = std::path::PathBuf::from(&config_root).join(relative_uri);
        let vfs_path = vfs::VfsPath::new(full_path.to_string_lossy().into_owned());

        if let Some(file_set) = self.file_set {
            file_set.file_for_path(&vfs_path).copied()
        } else {
            self.db.resolve_vfs_path(SourceRootId(0), &vfs_path)
        }
    }

    fn file_path(&self, file_id: FileId) -> Option<String> {
        if let Some(file_set) = self.file_set {
            let vfs_path = file_set.path_for_file(&file_id)?;
            return Some(vfs_path.as_path().to_string_lossy().to_string());
        }
        crate::vfs_helpers::get_file_path(self.db, file_id).map(|p| p.to_string_lossy().to_string())
    }

    fn method_effect_summary(
        &self,
        method: hir::MethodId,
    ) -> Arc<hir::dataflow::effect_summary::EffectSummary> {
        let method_input = hir::MethodIdInput::new(self.db, method);
        crate::effects::method_effect_summary_query(self.db, method_input)
    }

    fn method_security_state(
        &self,
        method: hir::MethodId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        self.db.method_security_state(method)
    }

    fn module_code_security_state(
        &self,
        file_id: FileId,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        self.db.module_code_security_state(file_id)
    }

    fn method_hir_metrics(&self, method: hir::MethodId) -> Arc<hir::metrics::HirMethodMetrics> {
        let input = hir::MethodIdInput::new(self.db, method);
        crate::queries::method_hir_metrics_query(self.db, input)
    }

    fn module_hir_metrics(&self, file_id: FileId) -> Arc<crate::queries::ModuleHirMetrics> {
        let input = FileIdInput::new(self.db, file_id);
        crate::queries::module_hir_metrics_query(self.db, input)
    }

    fn method_cyclomatic(&self, method: hir::MethodId) -> u32 {
        let input = hir::MethodIdInput::new(self.db, method);
        crate::queries::method_cyclomatic_query(self.db, input)
    }

    fn module_cyclomatic(&self, file_id: FileId) -> Arc<crate::queries::ModuleCyclomatic> {
        let input = FileIdInput::new(self.db, file_id);
        crate::queries::module_cyclomatic_query(self.db, input)
    }
}
