use std::sync::Arc;

use bsl_metadata::MdObject;

use crate::configs::ConfigsDatabase;
use crate::module_interface::ModuleInterface;
use crate::scope::{ExprScopes, ScopeId};
use crate::{DefDatabase, MethodId, ModuleId, Name, PathResolution, QualifiedName, VariableId};

pub struct Resolver {
    #[doc(hidden)]
    pub scopes: Vec<Scope>,
}

#[doc(hidden)]
pub enum Scope {
    /// The enclosing module. `local_symbols` overrides same-module method/variable
    /// resolution with an explicit symbol tree (the *effective* module of an
    /// `&ИзменениеИКонтроль` extension); `None` resolves through the module's
    /// declarations by name (`db.interface_method_named`) — the default for
    /// every ordinary module.
    ///
    /// `base_fallback` is the paired base module of a configuration-*extension* module
    /// (weaving, Phase 3): same-module method/variable lookups that miss in `module_id`'s
    /// own symbols retry against the base module's symbols, so an interceptor or new
    /// extension method that calls a base-module sibling resolves. The extension's own
    /// symbols are tried FIRST (extension shadows base); the fallback only fills misses, so
    /// it can never turn a resolved name unresolved. Mutually exclusive with `local_symbols`
    /// (the `&ИзменениеИКонтроль` effective module already contains the merged base body).
    ModuleScope {
        module_id: ModuleId,
        local_symbols: Option<Arc<ModuleInterface>>,
        base_fallback: Option<ModuleId>,
    },

    ExprScope {
        scopes: Arc<ExprScopes>,
        scope_id: ScopeId,
    },

    WorkspaceScope,

    Builtins,
}

impl Resolver {
    pub fn for_module(module_id: ModuleId) -> Self {
        Resolver {
            scopes: vec![Scope::ModuleScope {
                module_id,
                local_symbols: None,
                base_fallback: None,
            }],
        }
    }

    pub fn with_workspace_scope(module_id: ModuleId) -> Self {
        Resolver {
            scopes: vec![
                Scope::WorkspaceScope,
                Scope::ModuleScope { module_id, local_symbols: None, base_fallback: None },
            ],
        }
    }

    pub fn with_builtins_and_workspace(module_id: ModuleId) -> Self {
        Resolver {
            scopes: vec![
                Scope::Builtins,
                Scope::WorkspaceScope,
                Scope::ModuleScope { module_id, local_symbols: None, base_fallback: None },
            ],
        }
    }

    /// Like [`Self::with_builtins_and_workspace`], but same-module method/variable
    /// lookups resolve against `local_symbols` (the effective module's symbol tree)
    /// instead of the module's declarations by name. Cross-module / metadata resolution still
    /// keys on `module_id.file_id`, which is the base file — correct, because the
    /// effective module *is* the base module with the extension's edits applied.
    pub fn with_builtins_and_workspace_effective(
        module_id: ModuleId,
        local_symbols: Arc<ModuleInterface>,
    ) -> Self {
        Resolver {
            scopes: vec![
                Scope::Builtins,
                Scope::WorkspaceScope,
                Scope::ModuleScope {
                    module_id,
                    local_symbols: Some(local_symbols),
                    base_fallback: None,
                },
            ],
        }
    }

    /// Like [`Self::with_builtins_and_workspace`], but same-module method/variable lookups
    /// that miss in `module_id`'s own symbols retry against `base_module_id` — the paired
    /// base module of a configuration-extension module (weaving, Phase 3). `module_id` stays
    /// the extension file (so configuration / metadata / cross-module resolution keys on the
    /// extension's own `file_id`, which is correct); only the bare same-module sibling lookup
    /// gains the base fallback. The extension shadows the base (own symbols tried first).
    pub fn with_builtins_and_workspace_weaving(
        module_id: ModuleId,
        base_module_id: ModuleId,
    ) -> Self {
        Resolver {
            scopes: vec![
                Scope::Builtins,
                Scope::WorkspaceScope,
                Scope::ModuleScope {
                    module_id,
                    local_symbols: None,
                    base_fallback: Some(base_module_id),
                },
            ],
        }
    }

    fn has_builtins(&self) -> bool {
        self.scopes.iter().any(|s| matches!(s, Scope::Builtins))
    }

    fn resolve_builtin(&self, name: &Name) -> Option<Name> {
        if !self.has_builtins() {
            return None;
        }

        if bsl_platform::PlatformDataInner::instance().get_global_function(name.as_str()).is_some()
        {
            Some(name.clone())
        } else {
            None
        }
    }

    fn mdo_visible(
        db: &dyn ConfigsDatabase,
        file_id: vfs::FileId,
        mdo_type: bsl_metadata::MdoType,
        mdo_name: &Name,
    ) -> bool {
        let needle = mdo_name.as_str();
        db.resolve_metadata_object(file_id, mdo_type, needle).is_some()
            || db.resolve_register(file_id, mdo_type, needle).is_some()
    }

    pub fn push_expr_scope(mut self, scopes: Arc<ExprScopes>, scope_id: ScopeId) -> Self {
        self.scopes.push(Scope::ExprScope { scopes, scope_id });
        self
    }

    pub fn module_id(&self) -> Option<ModuleId> {
        for scope in &self.scopes {
            if let Scope::ModuleScope { module_id, .. } = scope {
                return Some(*module_id);
            }
        }
        None
    }

    /// The effective module's symbol tree, when this resolver was built with one
    /// ([`Self::with_builtins_and_workspace_effective`]). `None` for every ordinary
    /// module, in which case same-module lookups go to the declarations by name.
    fn module_local_symbols(&self) -> Option<&Arc<ModuleInterface>> {
        for scope in &self.scopes {
            if let Scope::ModuleScope { local_symbols, .. } = scope {
                return local_symbols.as_ref();
            }
        }
        None
    }

    /// The paired base module to retry same-module sibling lookups against, when this
    /// resolver was built for a configuration-extension module
    /// ([`Self::with_builtins_and_workspace_weaving`]). `None` for every other module.
    fn module_base_fallback(&self) -> Option<ModuleId> {
        for scope in &self.scopes {
            if let Scope::ModuleScope { base_fallback, .. } = scope {
                return *base_fallback;
            }
        }
        None
    }

    pub fn resolve_local(&self, name: &Name) -> Option<ResolvedLocal> {
        for scope in self.scopes.iter().rev() {
            if let Scope::ExprScope { scopes, scope_id } = scope {
                if let Some(def) = scopes.resolve_name(*scope_id, name) {
                    return Some(ResolvedLocal { def });
                }
            }
        }

        None
    }

    pub fn resolve_module_method(&self, db: &dyn DefDatabase, name: &Name) -> Option<MethodId> {
        if let Some(symbols) = self.module_local_symbols() {
            return symbols.find_method(name).map(|m| m.id);
        }
        let module_id = self.module_id()?;
        if let Some(m) = db.interface_method_named(module_id, name) {
            return Some(m.id);
        }
        // Weaving: an extension method calling a base-module sibling resolves against the
        // paired base module. The returned `MethodId` carries the base module, so downstream
        // type inference uses the base method's real signature.
        let base = self.module_base_fallback()?;
        db.interface_method_named(base, name).map(|m| m.id)
    }

    pub fn resolve_module_variable(&self, db: &dyn DefDatabase, name: &Name) -> Option<VariableId> {
        if let Some(symbols) = self.module_local_symbols() {
            return symbols.find_variable(name).map(|v| v.id);
        }
        let module_id = self.module_id()?;
        if let Some(v) = db.interface_variable_named(module_id, name) {
            return Some(v.id);
        }
        let base = self.module_base_fallback()?;
        db.interface_variable_named(base, name).map(|v| v.id)
    }

    pub fn user_common_module_exists(&self, db: &dyn ConfigsDatabase, module_name: &Name) -> bool {
        if !self.scopes.iter().any(|s| matches!(s, Scope::WorkspaceScope)) {
            return false;
        }
        let Some(module_id) = self.module_id() else { return false };
        let file_id = module_id.file_id;

        if db.has_config_root(file_id)
            && db.resolve_common_module(file_id, module_name.as_str()).is_none()
        {
            return false;
        }

        let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
        db.module_index(source_root_id).resolve_common_module(module_name).is_some()
    }

    /// Resolve a bare register name (from a `Движения.<Регистр>` movement touch) to its
    /// metadata type and canonical name. The movement syntax carries only the register
    /// name; the configuration's register index supplies the type (Accumulation /
    /// Information / Accounting / Calculation). Returns `None` when no register of that
    /// name is visible, so an unresolved movement is surfaced honestly rather than guessed.
    pub fn resolve_register_by_name(
        &self,
        db: &dyn ConfigsDatabase,
        register_name: &Name,
    ) -> Option<(bsl_metadata::MdoType, Name)> {
        if !self.scopes.iter().any(|s| matches!(s, Scope::WorkspaceScope)) {
            return None;
        }
        let module_id = self.module_id()?;
        db.resolve_register_by_name(module_id.file_id, register_name.as_str())
            .map(|reg| (reg.mdo_type(), Name::new(reg.name())))
    }

    /// Names of the configuration's GLOBAL common modules — those whose exported
    /// procedures are callable unqualified. These are the candidate modules for a
    /// global-context `ПодключитьОбработчикОжидания` handler, which names the procedure
    /// without a module qualifier. Deduplicated across visible configs (extension overlays).
    ///
    /// Two deliberate scope choices, both in service of impact analysis:
    /// - Modules are NOT filtered by client/server dispatch. The help requires an idle
    ///   handler to be client-side, but the graph models the *named reference* — if such a
    ///   handler names a server-only method, renaming that method still breaks the
    ///   registration, so the edge must exist. Dispatch validity is a diagnostic concern.
    /// - The application module (`МодульПриложения`) — the help's other global-context host
    ///   — is not yet enumerated here; only global common modules are. A handler living in
    ///   the application module is a known, currently-unmodelled gap.
    pub fn global_common_module_names(&self, db: &dyn ConfigsDatabase) -> Vec<Name> {
        if !self.scopes.iter().any(|s| matches!(s, Scope::WorkspaceScope)) {
            return Vec::new();
        }
        let Some(module_id) = self.module_id() else { return Vec::new() };
        let mut names = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        for cfg in db.configurations(module_id.file_id).iter() {
            for cm in cfg.configuration.common_modules() {
                if cm.is_global() && seen.insert(intern::NormName::intern(cm.name())) {
                    names.push(Name::new(cm.name()));
                }
            }
        }
        names
    }

    /// Exported methods of the configuration's GLOBAL common modules — those callable
    /// without a module qualifier because a global common module extends the global
    /// context. Each entry is `(module_name, method_name, method_id)`.
    ///
    /// Both the global flag and the module body are taken from the module *effective* for
    /// this file: candidate names come from [`Self::global_common_module_names`], but each
    /// is confirmed global via [`ConfigsDatabase::resolve_common_module`] (base composed
    /// with the file's own extension, extension winning) before its bodies are located
    /// through [`Self::locate_common_module_candidates`] — the same visibility-correct path
    /// a qualified `Модуль.Метод()` call takes. So a bare call resolves to exactly the
    /// target the qualified form would, and an extension that turns a base-global module
    /// non-global (or vice versa) is honoured rather than leaking the base flag.
    ///
    /// Order is deterministic: global modules in configuration-iteration order, methods in
    /// symbol-tree order. Callers that need a single winner for a name keep the FIRST
    /// occurrence (a name exported by two global modules is a configuration-level ambiguity
    /// 1C itself forbids; we pick deterministically rather than guess).
    pub fn global_common_module_exports(&self, db: &dyn ConfigsDatabase) -> GlobalExports {
        let Some(self_module) = self.module_id() else {
            return GlobalExports::default();
        };
        let mut exports = Vec::new();
        let mut gaps = Vec::new();
        for module_name in self.global_common_module_names(db) {
            let effective_is_global = db
                .resolve_common_module(self_module.file_id, module_name.as_str())
                .is_some_and(|cm| cm.is_global());
            if !effective_is_global {
                continue;
            }
            let Ok(candidates) = self.locate_common_module_candidates(db, &module_name) else {
                gaps.push(GlobalSurfaceGap::MissingBody { module: module_name });
                continue;
            };
            // Base first, then the caller's own extension body: a name declared
            // in both keeps the base declaration, an extension-added export is
            // appended after it. The probe never answers, so the walk collects from
            // every readable body and stops where a qualified call would — at the
            // first unread one, whose exports are unknown and whom a body behind it
            // must not extend the global context in place of.
            let missing_body = candidates.missing_body();
            if missing_body {
                gaps.push(GlobalSurfaceGap::MissingBody { module: module_name.clone() });
            }
            let mut seen_methods = rustc_hash::FxHashSet::default();
            let walk: crate::configs::BodySearch<()> = candidates.search(|module_id| {
                let interface = db.module_interface_ref(module_id);
                for method in interface.exported_methods() {
                    if seen_methods.insert(intern::NormName::intern(method.name.as_str())) {
                        exports.push(GlobalExportEntry {
                            module: module_name.clone(),
                            host: GlobalExportHost::CommonModule,
                            name: method.name.clone(),
                            definition: GlobalExportDefinition::Method(method.id),
                            capabilities: GlobalExportCapabilities {
                                readable_as_value: Some(false),
                                callable: Some(true),
                                assignable: Some(false),
                            },
                        });
                    }
                }
                // Common modules cannot legally contain module variables; EDT
                // reports such a body as invalid. Parser-recovery variables must
                // therefore not extend the user global surface.
                None
            });
            if !missing_body && matches!(walk, crate::configs::BodySearch::Unread) {
                gaps.push(GlobalSurfaceGap::UnreadBody { module: module_name });
            }
        }
        GlobalExports { entries: exports, gaps }
    }

    /// Exported methods and variables of application-context host modules visible
    /// to this caller. Unlike global common modules, application and external-
    /// connection module variables are legal and extend their respective global
    /// contexts.
    pub fn application_module_exports(&self, db: &dyn ConfigsDatabase) -> GlobalExports {
        let Some(self_module) = self.module_id() else {
            return GlobalExports::default();
        };
        let mut exports = Vec::new();
        let mut gaps = Vec::new();
        for kind in crate::ApplicationModuleKind::ALL {
            let Some(bodies) =
                db.resolve_application_module_file_candidates(self_module.file_id, kind)
            else {
                gaps.push(GlobalSurfaceGap::UnindexedApplicationModules { kind });
                continue;
            };
            let mut seen_methods = rustc_hash::FxHashSet::default();
            let mut seen_variables = rustc_hash::FxHashSet::default();
            let walk: crate::BodySearch<()> = bodies.search_merged_surface(|module_id| {
                let module_id = ModuleId::new(module_id);
                let interface = db.module_interface_ref(module_id);
                for method in interface.exported_methods() {
                    if seen_methods.insert(intern::NormName::intern(method.name.as_str())) {
                        exports.push(GlobalExportEntry {
                            module: application_module_name(kind),
                            host: GlobalExportHost::ApplicationModule(kind),
                            name: method.name.clone(),
                            definition: GlobalExportDefinition::Method(method.id),
                            capabilities: GlobalExportCapabilities {
                                readable_as_value: Some(false),
                                callable: Some(true),
                                assignable: Some(false),
                            },
                        });
                    }
                }
                for variable in interface.exported_variables() {
                    if seen_variables.insert(intern::NormName::intern(variable.name.as_str())) {
                        exports.push(GlobalExportEntry {
                            module: application_module_name(kind),
                            host: GlobalExportHost::ApplicationModule(kind),
                            name: variable.name.clone(),
                            definition: GlobalExportDefinition::Variable(variable.id),
                            capabilities: GlobalExportCapabilities {
                                readable_as_value: Some(true),
                                callable: Some(false),
                                assignable: None,
                            },
                        });
                    }
                }
                None
            });
            if matches!(walk, crate::BodySearch::Unread) {
                gaps.push(GlobalSurfaceGap::UnreadApplicationModule { kind });
            }
        }
        GlobalExports { entries: exports, gaps }
    }

    pub fn resolve_assignment_target(
        &self,
        db: &dyn ConfigsDatabase,
        name: &Name,
    ) -> AssignmentResolution {
        if let Some(local) = self.resolve_local(name) {
            return match local.def {
                crate::scope::ScopeDef::LocalVariable => AssignmentResolution::Local,
                crate::scope::ScopeDef::Parameter => AssignmentResolution::Param,
            };
        }
        if let Some(var_id) = self.resolve_module_variable(db, name) {
            return AssignmentResolution::ModuleVariable(var_id);
        }
        if let Some(module_id) = self.module_id() {
            if db.resolve_common_module(module_id.file_id, name.as_str()).is_some() {
                return AssignmentResolution::CommonModule(name.clone());
            }
        }
        AssignmentResolution::Unknown
    }

    pub fn resolve_name(&self, db: &dyn DefDatabase, name: &Name) -> Option<Resolution> {
        if let Some(builtin_name) = self.resolve_builtin(name) {
            return Some(Resolution::Builtin(builtin_name));
        }

        if let Some(local) = self.resolve_local(name) {
            return Some(Resolution::Local(local));
        }

        if let Some(method_id) = self.resolve_module_method(db, name) {
            return Some(Resolution::Method(method_id));
        }

        if let Some(variable_id) = self.resolve_module_variable(db, name) {
            return Some(Resolution::Variable(variable_id));
        }

        None
    }

    pub fn resolve_path(&self, db: &dyn ConfigsDatabase, path: &QualifiedName) -> PathResolution {
        let segments = path.segments();

        match segments.len() {
            0 => PathResolution::Unresolved(path.clone()),

            1 => {
                if let Some(resolution) = self.resolve_name(db, &segments[0]) {
                    return match resolution {
                        Resolution::Builtin(name) => PathResolution::Builtin(name),
                        Resolution::Method(id) => PathResolution::Method(id),
                        Resolution::Variable(id) => PathResolution::Variable(id),
                        Resolution::Local(_) => PathResolution::Unresolved(path.clone()),
                    };
                }
                PathResolution::Unresolved(path.clone())
            }

            2 => self.resolve_two_level(db, &segments[0], &segments[1]),

            3 => self.resolve_three_level(db, &segments[0], &segments[1], &segments[2]),

            _ => PathResolution::Unresolved(path.clone()),
        }
    }

    fn resolve_two_level(
        &self,
        db: &dyn ConfigsDatabase,
        module_name: &Name,
        method_name: &Name,
    ) -> PathResolution {
        for scope in &self.scopes {
            if let Scope::WorkspaceScope = scope {
                return self.resolve_cross_module(db, module_name, method_name);
            }
        }

        PathResolution::Unresolved(QualifiedName::from_segments([
            module_name.clone(),
            method_name.clone(),
        ]))
    }

    /// Every body file the common module `module_name` resolves to for this
    /// resolver's file (config visibility + body lookup), base first, without
    /// looking at any method. Shared by [`Self::resolve_qualified_method`] and
    /// the graph-index build, so both agree on which bodies a qualified call
    /// targets; only the method-lookup step (symbol tree vs. the resident graph
    /// index) differs.
    ///
    /// A module adopted by the caller's own
    /// extension has two bodies — the base one and the extension one that adds
    /// methods on top of it — and a method lookup that sees only one of them
    /// misreports the other half of the surface. Callers outside any extension
    /// get exactly the base body, so an extension-added method stays invisible
    /// to base-configuration code (calling it from there is a genuine error:
    /// the extension can be detached at any time).
    ///
    /// The returned vector is never empty.
    pub(crate) fn locate_common_module_candidates(
        &self,
        db: &dyn ConfigsDatabase,
        module_name: &Name,
    ) -> Result<CommonModuleCandidates, QualifiedMethodError> {
        if !self.scopes.iter().any(|s| matches!(s, Scope::WorkspaceScope)) {
            tracing::warn!(
                "locate_common_module_candidates called without workspace scope; refusing"
            );
            return Err(QualifiedMethodError::NotFound);
        }

        let module_id = self.module_id().ok_or_else(|| {
            tracing::warn!("locate_common_module_candidates called without module scope");
            QualifiedMethodError::NotFound
        })?;

        let file_id = module_id.file_id;

        if db.has_config_root(file_id) {
            if db.resolve_common_module(file_id, module_name.as_str()).is_none() {
                return Err(QualifiedMethodError::NotVisibleInConfigs);
            }

            if let Some(bodies) =
                db.resolve_common_module_file_candidates(file_id, module_name.as_str())
            {
                if !bodies.is_empty() {
                    // Composition, not a verdict: whether a body without the method is
                    // an answer depends on the method name, which this function does
                    // not have — that decision belongs to `search` one level up.
                    // Reporting the outcome here would also hide the unread ids from
                    // the graph index, which needs them to reproject on healing.
                    return Ok(CommonModuleCandidates::new(
                        bodies.iter().map(|b| (crate::ModuleId::new(b.file), b.unread)).collect(),
                        bodies.has_missing_expected_body(),
                    ));
                }
                // Configs see the module but no body file mapped (metadata-URI
                // drift): degrade to the path index below instead of reporting
                // a module the visibility gate just confirmed as missing.
            }
        }

        let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
        let module_index = db.module_index(source_root_id);

        let target_file_id = module_index
            .resolve_common_module(module_name)
            .ok_or(QualifiedMethodError::NotFound)?;

        // The path index is built from source-root paths, which hold an unread body
        // exactly like any other file — its entry cannot be dropped, because the first
        // query through a dropped id panics resolving its path. So the sorting happens
        // here, on the way out.
        Ok(CommonModuleCandidates::new(
            vec![(crate::ModuleId::new(target_file_id), db.file_is_unread(target_file_id))],
            false,
        ))
    }

    pub fn resolve_qualified_method(
        &self,
        db: &dyn ConfigsDatabase,
        module_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let _span =
            tracing::info_span!("resolve_qualified_method", %module_name, %method_name).entered();

        let candidates = self.locate_common_module_candidates(db, module_name)?;
        let found = candidates.search(|target_module_id| {
            db.interface_method_named(target_module_id, method_name).map(|m| (m.id, m.is_export))
        });
        match found {
            crate::configs::BodySearch::Found((method_id, is_export)) => {
                Ok(QualifiedMethodResolution { method_id, is_export })
            }
            crate::configs::BodySearch::Absent => Err(QualifiedMethodError::NotFound),
            crate::configs::BodySearch::Unread => Err(QualifiedMethodError::BodyUnread),
        }
    }

    fn resolve_cross_module(
        &self,
        db: &dyn ConfigsDatabase,
        module_name: &Name,
        method_name: &Name,
    ) -> PathResolution {
        let unresolved = || {
            PathResolution::Unresolved(QualifiedName::from_segments([
                module_name.clone(),
                method_name.clone(),
            ]))
        };
        match self.resolve_qualified_method(db, module_name, method_name) {
            Ok(r) if r.is_export => PathResolution::Method(r.method_id),
            Ok(_) | Err(_) => unresolved(),
        }
    }

    pub fn resolve_three_level_method(
        &self,
        db: &dyn ConfigsDatabase,
        mdo_type_plural: &Name,
        mdo_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let mdo_type =
            bsl_metadata::MdoType::from_plural(mdo_type_plural.as_str()).ok_or_else(|| {
                tracing::debug!("Unknown MDO type plural: {}", mdo_type_plural);
                QualifiedMethodError::NotFound
            })?;

        let manager_type = crate::body::ManagerType::from_mdo_type(mdo_type).ok_or_else(|| {
            tracing::debug!("MdoType {:?} does not have manager module", mdo_type);
            QualifiedMethodError::NotFound
        })?;

        self.resolve_manager_method(db, manager_type, mdo_name, method_name)
    }

    /// Resolve a user-defined method on the manager module of a metadata object,
    /// addressed by an already-parsed [`ManagerType`] (e.g.
    /// `Справочники.Контрагенты.Метод`). Platform manager methods (e.g.
    /// `СоздатьЭлемент`) are not user methods and resolve to `NotFound`.
    /// Locate the manager module of `manager_type`/`mdo_name` (config visibility +
    /// path index), without looking at any method. Shared by
    /// [`Self::resolve_manager_method`] and the graph-index build.
    /// The first body that declares `method_name`, with the unread barrier: a body
    /// nobody could read stops the walk instead of letting a lower-priority body
    /// answer in its place.
    fn first_method_in(
        db: &dyn ConfigsDatabase,
        candidates: &CommonModuleCandidates,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        match candidates.search(|module| {
            db.interface_method_named(module, method_name).map(|m| (m.id, m.is_export))
        }) {
            crate::configs::BodySearch::Found((method_id, is_export)) => {
                Ok(QualifiedMethodResolution { method_id, is_export })
            }
            crate::configs::BodySearch::Absent => Err(QualifiedMethodError::NotFound),
            crate::configs::BodySearch::Unread => Err(QualifiedMethodError::BodyUnread),
        }
    }

    /// Bodies of the `role` module of `mdo_type`/`mdo_name` that this resolver's
    /// file may resolve against, in priority order — base first.
    ///
    /// TWO different visibility questions meet here, and conflating them is the
    /// defect this exists to close. `mdo_visible` asks whether the caller can see
    /// the OBJECT, and a catalog adopted by the base configuration is visible to
    /// every extension — so that check passes even when the body about to answer
    /// lives in a root the caller never declared a dependency on. The second
    /// question, whose BODY may answer, is what the candidate lookup answers.
    fn locate_mdo_module(
        &self,
        db: &dyn ConfigsDatabase,
        role: crate::configs::MdoModuleRole,
        mdo_type: bsl_metadata::MdoType,
        mdo_name: &Name,
    ) -> Result<CommonModuleCandidates, QualifiedMethodError> {
        let current_module_id = self.module_id().ok_or_else(|| {
            tracing::warn!("locate_mdo_module called without module scope");
            QualifiedMethodError::NotFound
        })?;
        let current_file_id = current_module_id.file_id;

        if db.file_has_visible_config(current_file_id)
            && !Self::mdo_visible(db, current_file_id, mdo_type, mdo_name)
        {
            return Err(QualifiedMethodError::NotVisibleInConfigs);
        }

        if let Some(bodies) = db.resolve_mdo_module_file_candidates(
            current_file_id,
            role,
            mdo_type,
            mdo_name.as_str(),
        ) {
            // Empty means no VISIBLE root holds such a body, and that is an answer.
            // The common-module route degrades to the path index here instead,
            // because its listing can disagree with the file set; doing that here
            // would hand back the very body the filter just removed.
            if bodies.is_empty() {
                return Err(QualifiedMethodError::NotFound);
            }
            return Ok(CommonModuleCandidates::new(
                bodies.iter().map(|b| (crate::ModuleId::new(b.file), b.unread)).collect(),
                false,
            ));
        }

        // No configured visibility at all: the path index, exactly as before.
        let source_root_id = db.file_source_root_input(current_file_id).source_root_id(db);
        let module_index = db.module_index(source_root_id);
        let target_file_id = match role {
            crate::configs::MdoModuleRole::Manager => {
                crate::body::ManagerType::from_mdo_type(mdo_type)
                    .and_then(|manager_type| module_index.resolve_manager(manager_type, mdo_name))
            }
            crate::configs::MdoModuleRole::Object => {
                module_index.resolve_object_module(mdo_type, mdo_name)
            }
            crate::configs::MdoModuleRole::RecordSet => {
                module_index.resolve_record_set_module(mdo_type, mdo_name)
            }
        }
        .ok_or(QualifiedMethodError::NotFound)?;

        Ok(CommonModuleCandidates::new(
            vec![(crate::ModuleId::new(target_file_id), db.file_is_unread(target_file_id))],
            false,
        ))
    }

    /// Locate the manager module of `manager_type`/`mdo_name`, without looking at
    /// any method. Shared by [`Self::resolve_manager_method`] and the graph-index
    /// build, so both agree on which bodies a manager call targets.
    pub(crate) fn locate_manager_module(
        &self,
        db: &dyn ConfigsDatabase,
        manager_type: crate::body::ManagerType,
        mdo_name: &Name,
    ) -> Result<CommonModuleCandidates, QualifiedMethodError> {
        self.locate_mdo_module(
            db,
            crate::configs::MdoModuleRole::Manager,
            manager_type.to_mdo_type(),
            mdo_name,
        )
    }

    pub fn resolve_manager_method(
        &self,
        db: &dyn ConfigsDatabase,
        manager_type: crate::body::ManagerType,
        mdo_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let _span = tracing::info_span!(
            "resolve_manager_method",
            ?manager_type,
            mdo_name = %mdo_name,
            method = %method_name
        )
        .entered();

        let candidates = self.locate_manager_module(db, manager_type, mdo_name)?;
        Self::first_method_in(db, &candidates, method_name)
    }

    pub fn resolve_aliased_manager_method(
        &self,
        db: &dyn ConfigsDatabase,
        mdo_type: bsl_metadata::MdoType,
        mdo_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let _span = tracing::info_span!(
            "resolve_aliased_manager_method",
            ?mdo_type,
            mdo_name = %mdo_name,
            method = %method_name
        )
        .entered();

        let manager_type = crate::body::ManagerType::from_mdo_type(mdo_type).ok_or_else(|| {
            tracing::debug!("MdoType {:?} does not have manager module", mdo_type);
            QualifiedMethodError::NotFound
        })?;

        self.resolve_manager_method(db, manager_type, mdo_name, method_name)
    }

    pub fn resolve_object_module_method(
        &self,
        db: &dyn ConfigsDatabase,
        mdo_type: bsl_metadata::MdoType,
        mdo_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let _span = tracing::info_span!(
            "resolve_object_module_method",
            ?mdo_type,
            mdo_name = %mdo_name,
            method = %method_name
        )
        .entered();

        let candidates =
            self.locate_mdo_module(db, crate::configs::MdoModuleRole::Object, mdo_type, mdo_name)?;
        Self::first_method_in(db, &candidates, method_name)
    }

    pub fn resolve_record_set_module_method(
        &self,
        db: &dyn ConfigsDatabase,
        mdo_type: bsl_metadata::MdoType,
        mdo_name: &Name,
        method_name: &Name,
    ) -> Result<QualifiedMethodResolution, QualifiedMethodError> {
        let _span = tracing::info_span!(
            "resolve_record_set_module_method",
            ?mdo_type,
            mdo_name = %mdo_name,
            method = %method_name
        )
        .entered();

        let candidates = self.locate_mdo_module(
            db,
            crate::configs::MdoModuleRole::RecordSet,
            mdo_type,
            mdo_name,
        )?;
        Self::first_method_in(db, &candidates, method_name)
    }

    fn resolve_three_level(
        &self,
        db: &dyn ConfigsDatabase,
        mdo_type_plural: &Name,
        mdo_name: &Name,
        method_name: &Name,
    ) -> PathResolution {
        let unresolved = || {
            PathResolution::Unresolved(QualifiedName::from_segments([
                mdo_type_plural.clone(),
                mdo_name.clone(),
                method_name.clone(),
            ]))
        };
        match self.resolve_three_level_method(db, mdo_type_plural, mdo_name, method_name) {
            Ok(r) if r.is_export => PathResolution::Method(r.method_id),
            Ok(_) | Err(_) => unresolved(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLocal {
    pub def: crate::scope::ScopeDef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Builtin(Name),

    Local(ResolvedLocal),

    Method(MethodId),

    Variable(VariableId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentResolution {
    Local,
    Param,
    ModuleVariable(VariableId),
    CommonModule(Name),
    Unknown,
}

fn application_module_name(kind: crate::ApplicationModuleKind) -> Name {
    Name::new(match kind {
        crate::ApplicationModuleKind::Managed => "МодульУправляемогоПриложения",
        crate::ApplicationModuleKind::Ordinary => "МодульОбычногоПриложения",
        crate::ApplicationModuleKind::Generic => "МодульПриложения",
        crate::ApplicationModuleKind::ExternalConnection => "МодульВнешнегоСоединения",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualifiedMethodResolution {
    pub method_id: MethodId,
    pub is_export: bool,
}

/// A symbol exported into the bare global context by a global common module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalExportEntry {
    pub module: Name,
    pub host: GlobalExportHost,
    pub name: Name,
    pub definition: GlobalExportDefinition,
    pub capabilities: GlobalExportCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalExportDefinition {
    Method(MethodId),
    Variable(VariableId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalExportHost {
    CommonModule,
    ApplicationModule(crate::ApplicationModuleKind),
}

/// Use capabilities measured by the language layer, independent of `hir-ty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalExportCapabilities {
    /// `None` means the Stage-0 runtime probes have not established the verdict.
    pub readable_as_value: Option<bool>,
    pub callable: Option<bool>,
    pub assignable: Option<bool>,
}

/// A reason the visible user-global surface cannot prove absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalSurfaceGap {
    MissingBody { module: Name },
    UnreadBody { module: Name },
    UnindexedApplicationModules { kind: crate::ApplicationModuleKind },
    UnreadApplicationModule { kind: crate::ApplicationModuleKind },
}

/// The exported surface of visible global common modules, including exact gap
/// reasons. Entries are inventory only; `hir-ty` decides which capabilities are
/// active for each bare-name use.
#[derive(Debug, Default)]
pub struct GlobalExports {
    pub entries: Vec<GlobalExportEntry>,
    pub gaps: Vec<GlobalSurfaceGap>,
}

impl GlobalExports {
    pub fn is_complete(&self) -> bool {
        self.gaps.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualifiedMethodError {
    NotVisibleInConfigs,
    NotFound,
    /// The module exists, but a body of it could not be read, so nothing can be
    /// concluded about the call — least of all against the file making it.
    BodyUnread,
}

/// The bodies of a common module that name resolution may look a method up in,
/// plus whether looking there is the whole story.
pub(crate) struct CommonModuleCandidates {
    /// Every body in PRIORITY ORDER with its readability. Order is semantic — the
    /// base declaration wins over an extension's — so a hit in a later body is the
    /// answer only when every earlier body was readable and did not have it.
    ///
    /// Private for the same reason as its file-level twin
    /// [`crate::configs::CommonModuleBodies`]: a readable field is a walk past the
    /// barrier that looks like an ordinary loop. All three walks live in this crate,
    /// so leaving it open here would be leaving it open exactly where it matters.
    bodies: Vec<(ModuleId, bool)>,
    missing_body: bool,
}

impl CommonModuleCandidates {
    pub(crate) fn new(bodies: Vec<(ModuleId, bool)>, missing_body: bool) -> Self {
        Self { bodies, missing_body }
    }

    pub(crate) fn missing_body(&self) -> bool {
        self.missing_body
    }

    /// The module-level twin of [`crate::configs::CommonModuleBodies::search`], with the
    /// same barrier and for the same reason.
    pub(crate) fn search<T>(
        &self,
        mut probe: impl FnMut(ModuleId) -> Option<T>,
    ) -> crate::configs::BodySearch<T> {
        for &(module, unread) in &self.bodies {
            if unread {
                return crate::configs::BodySearch::Unread;
            }
            if let Some(found) = probe(module) {
                return crate::configs::BodySearch::Found(found);
            }
        }
        crate::configs::BodySearch::Absent
    }

    /// The same candidates with each body's readability replaced wherever `flag` knows
    /// it. For a caller whose database cannot answer the question for every body — the
    /// graph's per-batch databases — the authority is the shared index, not the db.
    pub(crate) fn reflagged(mut self, flag: impl Fn(ModuleId) -> Option<bool>) -> Self {
        for (module, unread) in &mut self.bodies {
            if let Some(known) = flag(*module) {
                *unread = known;
            }
        }
        self
    }

    /// Every body, readable or not — for recording a relation to the module, never for
    /// reading anything out of it. See
    /// [`crate::configs::CommonModuleBodies::all_for_reference`].
    pub(crate) fn all_for_reference(&self) -> impl Iterator<Item = ModuleId> + '_ {
        self.bodies.iter().map(|(m, _)| *m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::{ExprScopes, ScopeDef};
    use crate::ModuleId;
    use vfs::FileId;

    #[test]
    fn test_module_resolver() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);
        let resolver = Resolver::for_module(module_id);

        assert_eq!(resolver.module_id(), Some(module_id));
    }

    #[test]
    fn test_local_resolution() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let mut scopes = ExprScopes::new();
        scopes.add_parameter(Name::new("Параметр"));
        scopes.add_local_variable(scopes.root_scope(), Name::new("Переменная"));

        let root_scope = scopes.root_scope();
        let resolver =
            Resolver::for_module(module_id).push_expr_scope(Arc::new(scopes), root_scope);

        let resolved = resolver.resolve_local(&Name::new("Параметр"));
        assert_eq!(resolved.map(|r| r.def), Some(ScopeDef::Parameter));

        let resolved = resolver.resolve_local(&Name::new("Переменная"));
        assert_eq!(resolved.map(|r| r.def), Some(ScopeDef::LocalVariable));

        let resolved = resolver.resolve_local(&Name::new("НеСуществует"));
        assert_eq!(resolved, None);
    }

    #[test]
    fn test_case_insensitive_resolution() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let mut scopes = ExprScopes::new();
        scopes.add_parameter(Name::new("МойПараметр"));

        let root_scope = scopes.root_scope();
        let resolver =
            Resolver::for_module(module_id).push_expr_scope(Arc::new(scopes), root_scope);

        let resolved = resolver.resolve_local(&Name::new("мойпараметр"));
        assert_eq!(resolved.map(|r| r.def), Some(ScopeDef::Parameter));

        let resolved = resolver.resolve_local(&Name::new("МОЙПАРАМЕТР"));
        assert_eq!(resolved.map(|r| r.def), Some(ScopeDef::Parameter));
    }

    #[test]
    fn test_resolve_path_single_segment() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);
        let _resolver = Resolver::for_module(module_id);

        let _path = QualifiedName::from_segments([Name::new("Переменная")]);
    }

    #[test]
    fn test_resolve_path_two_segments() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);
        let _resolver = Resolver::with_workspace_scope(module_id);

        let _path = QualifiedName::from_segments([Name::new("ОбщийМодуль"), Name::new("Метод")]);
    }

    #[test]
    fn test_resolve_path_three_segments() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);
        let _resolver = Resolver::with_workspace_scope(module_id);

        let _path = QualifiedName::from_segments([
            Name::new("Документы"),
            Name::new("ПКО"),
            Name::new("Создать"),
        ]);
    }

    #[test]
    fn test_builtins_scope_guard_is_opt_in() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let plain = Resolver::for_module(module_id);
        assert!(plain.resolve_builtin(&Name::new("Сообщить")).is_none());

        let with_workspace = Resolver::with_workspace_scope(module_id);
        assert!(with_workspace.resolve_builtin(&Name::new("Сообщить")).is_none());

        let with_all = Resolver::with_builtins_and_workspace(module_id);
        assert!(with_all.has_builtins());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtins_scope_resolves_platform_global() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let resolver = Resolver::with_builtins_and_workspace(module_id);

        assert!(
            bsl_platform::PlatformDataInner::instance().get_global_function("Сообщить").is_some(),
            "bundled platform data must include `Сообщить`; missing data would mean the \
             loader regressed — guard this assumption loudly"
        );

        let resolved = resolver.resolve_builtin(&Name::new("сообщить"));
        assert_eq!(
            resolved.as_ref().map(|n| n.as_str()),
            Some("сообщить"),
            "case-insensitive platform global lookup must succeed"
        );

        assert!(resolver.resolve_builtin(&Name::new("НетТакогоBuiltin")).is_none());
    }
}
