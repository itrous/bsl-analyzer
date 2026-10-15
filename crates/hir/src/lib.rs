pub mod bare_root;
mod definition;
pub mod name_classify;
mod semantic_symbol;
pub mod type_facade;

pub use bsl_types::intern::TypeKernelDb;
pub use bsl_types::kind::{MetadataReferenceKind, TypeId, TypeKind};
pub use definition::{Definition, ReferenceScope};
pub use hir_ty::builtin::builtin_functions;
pub use hir_ty::compat_mode;
pub use hir_ty::infer::CandidateCallBinding;
pub use hir_ty::method_lookup::platform_type_key_id;
pub use hir_ty::min_platform;
pub use hir_ty::this_object::coerce_to_metadata_ref_id;
pub use hir_ty::TyLoweringContext;
pub use hir_ty::{
    bare_global_name_claim, manager_collection_env, resolve_platform_global_property_type,
    BareGlobalClaim, BodyShadowScope,
};
pub use hir_ty::{is_form_items_collection_ty, FORM_ITEMS_TYPE_EN, FORM_ITEMS_TYPE_RU};
pub use hir_ty::{PlatformMethodHandle, PlatformMethodOrigin};
pub use name_classify::{classify_token, NameClass};
pub use semantic_symbol::{
    FileSymbolCtx, MemberReceiver, SemanticSymbol, SemanticSymbolKey, SemanticSymbolKind,
    SymbolDeclaration,
};
pub use type_facade::{
    execution_environment_at, form_element_type, kernel_type_label, module_implicit_field_names,
    module_implicit_fields, platform_name_entries, root_metadata_object_type,
    root_metadata_ref_type, root_object_manager_type, Field, HirFieldOrigin, PlatformNameEntry,
    PlatformNameKind, Type,
};

pub use hir_def::execution_env;
pub use hir_def::sdbl_cache::DbSdblResolver;
pub use hir_def::{all_sdbl_in_file_query, sdbl_hir_for_file_query, SdblHirEntries, SdblInFile};
pub use hir_def::{
    BindingId, DefWithBodyId, ExprId, IdConversion, ModuleMetadata, Name, PathResolution, StmtId,
};
pub use hir_def::{
    CallHierarchyReverseIndex, MethodCallPair, MethodId, ModuleData, ModuleId, VariableId,
};
pub use hir_def::{ExecutionContext, QualifiedName};
pub use hir_def::{RedundantAccessKind, SdblExprId};

pub use hir_def::body::SourceMapAt;
pub use hir_def::body::{
    DeprecatedKind8312, ExistingBindingKind, ExternalRef, MagicNumberContext, ManagerType,
};
pub use hir_def::{
    Body, BodyDiagnostic, BodySourceMap, LocalOffset, LocalRange, LowerResult, LowerResultAt,
    MethodOffset, ModuleBodies,
};

pub use hir_def::hir::{
    BinaryOp, Expr, ExprIdx, HirPreBranch, HirPreBranchKind, IfStmt, Literal, Stmt, StmtIdx,
    UnaryOp,
};

pub use hir_def::item_tree::{Annotation, AnnotationKind, Function, ModItem, Param, Procedure};

pub use hir_def::is_bsl_source;
pub use hir_def::module_structure;
pub use hir_def::region_tree::{RegionIdx, RegionTree};
pub use hir_def::symbol_tree::{MethodSymbol, VariableSymbol};
pub use hir_def::{
    module_key_for_path, module_path_segment_modes, parse_form_module_path, ConditionalTree,
    FormKey, ItemTree, ModuleIndex, ModuleKey, SymbolTree, WorkspaceMembers,
};

pub use hir_def::resolver::{
    AssignmentResolution, GlobalExportCapabilities, GlobalExportDefinition, GlobalExportEntry,
    GlobalExportHost, GlobalExports, GlobalSurfaceGap, Resolver,
};
pub use hir_def::scope::{ExprScopes, ScopeDef};
pub use hir_def::DefDatabase;

pub use hir_def::call_graph;
pub use hir_def::call_graph::{
    CallSite, EdgeProvenance, ForeignHandlerBindings, GraphNode, ModuleCallSummary, NotifyTarget,
    ResolvedCallEdge, ResolvedModuleSummary, ResolvedTarget, WorkspaceCallEdge, WorkspaceCallGraph,
    CALL_SITE_NOT_RECORDED, NO_CALL_SITE,
};
pub use hir_def::docs::{
    doc_comment_tokens, is_dotted_type_reference, DocCommentToken, DocCommentTokenKind, MethodDocs,
    ParameterDoc, TypeDoc, VariableDocs,
};
pub use hir_def::graph_index;
pub use hir_def::graph_index::{call_hierarchy_method_digest, MethodCallDigest};
pub use hir_def::{
    method_outbound_facts, resolved_module_summary_query, workspace_call_graph_query, ManagerRef,
    MethodOutboundFacts,
};

pub use hir_def::catch_class;
pub use hir_def::metrics;

pub mod cfg {
    pub use ::cfg::{
        cyclomatic_complexity, BasicBlockVertex, CfgBuilder, CfgEdgeType, CfgVertex,
        ConditionalVertex, ControlFlowGraph, ForEachHeaderVertex, ForHeaderVertex, NodeIndex,
        PreprocConditionVertex, WhileHeaderVertex,
    };
}

pub mod dataflow {
    pub use ::dataflow::{DataflowResult, DataflowSolver, Direction, DEFAULT_MAX_ITERATIONS};

    pub mod path_terminates {
        pub use ::dataflow::path_terminates::{
            analyze_path_terminates, analyze_path_terminates_default, MayFallthrough,
            PathTerminatesConfig, PathTerminatesResult, PathTerminatesTransfer,
        };
    }

    pub mod reaching_defs {
        pub use ::dataflow::reaching_defs::{
            DefSite, Definition, DefinitionIndex, ReachingDefs, ReachingDefsResult,
            ReachingDefsTransfer,
        };
    }

    pub mod temp_resource {
        pub use ::dataflow::temp_resource::{
            analyze_open_resources, OpenResourcesResult, OpenSet, ResourceEvent, ResourceProvider,
        };
    }

    pub mod effect_summary {
        pub use ::dataflow::effect_summary::{analyze_method_effects, CalleeKey, EffectSummary};
    }

    pub mod security_state {
        pub use ::dataflow::security_state::{
            analyze, open_events, Category, OpenEvent, PrivilegeCounter, SaturatingCount,
            SecurityCounters, SecurityModeState, SecurityStateProvider, K_MAX,
        };
    }

    pub mod guard_predicates {
        pub use ::dataflow::guard_predicates::{
            default_registry, is_stmt_guarded, GuardPredicate, GuardRegistry, GuardSemantics,
        };
    }
}

pub use hir_def::compute_execution_context;
pub use hir_def::method_slab::{
    method_slab_query, module_slab_layout_query, slab_of, LeadingContext, MethodSlab,
    RemainderBlock, Rules, SlabLayout, SlabSpan,
};
pub use hir_def::method_syntax::{detach, method_syntax_query, MethodSyntax};
pub use hir_def::name_usage_index::{
    normalize_match_name, normalize_name as normalize_usage_name, FileNameOffsets, FileNameUsage,
    SourceRootNameUsage,
};
pub use hir_def::region_tree::lower_regions;
pub use hir_def::resolver::Resolution;
pub use hir_def::workspace_index::WorkspaceIndex;
pub use hir_def::{
    interface_method_named, interface_variable_named, module_declares_method_query,
    module_method_names_query, module_misses_read_name_set_query, module_variable_names_query,
    NAME_SET_METHOD_LIMIT,
};
pub use hir_def::{MethodDecl, MethodKey, ModuleInterface, VariableDecl};
pub use hir_def::{MethodIdInput, ModuleNameInput};

pub use hir_def::{
    conditional_tree_query, file_dependencies_query, file_external_refs_query,
    file_name_offsets_query, file_name_usage_query, interface_method_query,
    interface_variable_named_query, item_tree_query, method_body_query, method_lower_query,
    module_bodies_query, module_call_summary_query, module_code_lower_query, module_data_query,
    module_index_query, module_interface_query, module_members_query, region_tree_query,
    set_lowering_lru_sweep_mode, source_root_name_usage_query, symbol_tree_query,
    workspace_foreign_handler_bindings_query, workspace_index_query,
};

pub use bsl_config::VisibleConfig;
pub use bsl_types::facet::{FormBindingFacet, FormBindingTargetFacet, MdoRefFacet};
pub use hir_def::effective_module::{
    effective_module_text, item_tree_effective, module_bodies_effective,
    module_interface_effective, parse_effective, symbol_tree_effective, EffectiveModule,
    EffectiveModuleId,
};
pub use hir_def::extension_merge::{
    extract_change_and_validate, pair_base_module_path, unquote_bsl_string, Origin, Segment,
};
pub use hir_def::weaving::{
    interception_applicable, interception_effective, interceptor_target, signature_mismatch,
    Interception, InterceptionKind, SignatureMismatch, WeavingModuleId,
};
pub use hir_def::{
    ApplicationModuleKind, BodySearch, CommonModuleBodies, CommonModuleBody, ConfigsDatabase,
    MdoModuleRole,
};
pub use hir_ty::arg_diagnostics::{
    file_arg_diagnostics, method_arg_diagnostics_query, module_code_arg_diagnostics_query,
    set_arg_diagnostics_lru_capacity,
};
pub use hir_ty::call_resolution::{
    BuiltinCallableId, CallCandidateSet, CallParam, CallParamMode, CallResolution, CallSelection,
    CallSignature, CandidateId, CandidateOrigin, CandidateProvenance, PlatformSignatureSlot,
    UserMethodId,
};
pub use hir_ty::db::HirDatabase;
pub use hir_ty::doc_see::{doc_see_signature_query, DocSeeSignature};
pub use hir_ty::form_self::{
    is_form_self_property_name, managed_form_platform_type_names, FORM_TYPE_NAME,
};
pub use hir_ty::infer::{
    infer_effective, infer_module_code_query, infer_owner, infer_query, infer_weaving,
    type_of_expr_query,
};
pub use hir_ty::method_graph::{infer_method_query, method_return_type_query};
pub use hir_ty::method_resolution::{resolve_qualified_call, MethodResolution};
pub use hir_ty::narrow::{
    narrow_or_base, narrow_or_base_indexed, narrow_query, narrowed_type_at, NarrowExprIndex,
    NarrowState,
};
pub use hir_ty::proc_signature;
pub use hir_ty::{
    form_control_platform_type_chain, form_control_platform_type_name, form_element_kind_label,
    form_element_kind_sort_band, BareNameGap, BareNameOrigin, BareNameResolution, BareNameUse,
    BodyInferenceResult, CallArgBinding, EnvCalleeKind, EnvMemberKind, FormElementKind,
    InferOwnerResult, InferenceContext, InferenceDiagnostic, InferenceResult, MetadataKind,
    ModuleCodeInferenceResult, UnresolvedMethodKind,
};

pub use bsl_types::builders::Builders;
pub use bsl_types::facet::FormDataFacet;
use syntax::{ast::AstNode, TextRange, TextSize};
use vfs::FileId;

#[derive(Debug, Clone, Copy)]
pub struct Module<'db, DB> {
    db: &'db DB,
    id: ModuleId,
}

impl<'db, DB: DefDatabase> Module<'db, DB> {
    pub(crate) fn new(db: &'db DB, id: ModuleId) -> Self {
        Self { db, id }
    }

    pub fn id(&self) -> ModuleId {
        self.id
    }

    pub fn procedures(&self) -> Vec<Method<'db, DB>> {
        let data = self.db.module_data(self.id);
        data.procedures.iter().map(|&id| Method::new(self.db, id)).collect()
    }

    pub fn functions(&self) -> Vec<Method<'db, DB>> {
        let data = self.db.module_data(self.id);
        data.functions.iter().map(|&id| Method::new(self.db, id)).collect()
    }

    pub fn variables(&self) -> Vec<Variable<'db, DB>> {
        let data = self.db.module_data(self.id);
        data.variables.iter().map(|&id| Variable::new(self.db, id)).collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Method<'db, DB> {
    db: &'db DB,
    id: MethodId,
}

pub(crate) struct MethodInfo {
    pub(crate) name: Name,
    pub(crate) is_export: bool,
    pub(crate) is_function: bool,
    pub(crate) source_range: TextRange,
    pub(crate) name_range: TextRange,
    pub(crate) sig_end: TextSize,
}

pub(crate) fn get_method_info(id: &MethodId, db: &dyn DefDatabase) -> Option<MethodInfo> {
    let tree = db.item_tree_ref(id.module.file_id);
    let method = tree.method(id.local_id)?;
    Some(MethodInfo {
        name: method.name().clone(),
        is_export: method.is_export(),
        is_function: method.is_function(),
        source_range: method.source_range(),
        name_range: method.name_range(),
        sig_end: method.sig_end(),
    })
}

impl<'db, DB: DefDatabase> Method<'db, DB> {
    pub fn new(db: &'db DB, id: MethodId) -> Self {
        Self { db, id }
    }

    pub fn id(&self) -> MethodId {
        self.id
    }

    fn method_info(&self) -> Option<MethodInfo> {
        get_method_info(&self.id, self.db)
    }

    pub fn name(&self) -> Name {
        self.method_info().map_or_else(Name::missing, |i| i.name)
    }

    pub fn is_export(&self) -> bool {
        self.method_info().is_some_and(|i| i.is_export)
    }

    pub fn is_function(&self) -> bool {
        self.method_info().is_some_and(|i| i.is_function)
    }

    pub fn source_range(&self) -> Option<TextRange> {
        self.method_info().map(|i| i.source_range)
    }

    pub fn name_range(&self) -> Option<TextRange> {
        self.method_info().map(|i| i.name_range)
    }

    /// End of the declaration header (closing `)` or export keyword) — the end of
    /// the full, possibly multi-line, signature that starts at [`name_range`](Self::name_range).
    pub fn sig_end(&self) -> Option<TextSize> {
        self.method_info().map(|i| i.sig_end)
    }

    pub fn docs(&self) -> Option<std::sync::Arc<hir_def::docs::MethodDocs>> {
        self.db.method_docs(self.id)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Variable<'db, DB> {
    db: &'db DB,
    id: VariableId,
}

pub(crate) struct VariableInfo {
    pub(crate) name: Name,
    pub(crate) is_export: bool,
    pub(crate) source_range: TextRange,
}

pub(crate) fn get_variable_info(id: &VariableId, db: &dyn DefDatabase) -> Option<VariableInfo> {
    let tree = db.item_tree_ref(id.module.file_id);
    let item = tree.top_level_items().get(id.local_id as usize)?;
    if let hir_def::item_tree::ModItem::Variable(var_idx) = item {
        let var = tree.variable(*var_idx);
        Some(VariableInfo {
            name: var.name.clone(),
            is_export: var.is_export,
            source_range: var.source_range,
        })
    } else {
        None
    }
}

impl<'db, DB: DefDatabase> Variable<'db, DB> {
    pub(crate) fn new(db: &'db DB, id: VariableId) -> Self {
        Self { db, id }
    }

    pub fn id(&self) -> VariableId {
        self.id
    }

    fn variable_info(&self) -> Option<VariableInfo> {
        get_variable_info(&self.id, self.db)
    }

    pub fn name(&self) -> Name {
        self.variable_info().map_or_else(Name::missing, |i| i.name)
    }

    pub fn is_export(&self) -> bool {
        self.variable_info().is_some_and(|i| i.is_export)
    }

    pub fn source_range(&self) -> Option<TextRange> {
        self.variable_info().map(|i| i.source_range)
    }
}

#[derive(Debug)]
pub struct Semantics<'db, DB> {
    db: &'db DB,
}

impl<'db, DB: ConfigsDatabase + base_db::RootQueryDb> Semantics<'db, DB> {
    pub fn new(db: &'db DB) -> Self {
        Self { db }
    }

    pub fn form(&self, file_id: FileId) -> Option<std::sync::Arc<bsl_metadata::Form>> {
        let module_id = ModuleId::new(file_id);
        self.db.module_metadata(module_id).form.clone()
    }

    pub fn classify_at(&self, file_id: FileId, offset: syntax::TextSize) -> NameClass {
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        match root.token_at_offset(offset).right_biased() {
            Some(token) => classify_token(&token),
            None => NameClass::Other,
        }
    }

    pub fn module_from_file(&self, file_id: vfs::FileId) -> Module<'db, DB> {
        let module_id = ModuleId::new(file_id);
        Module::new(self.db, module_id)
    }

    pub fn find_method(&self, file_id: vfs::FileId, name: &str) -> Option<Method<'db, DB>> {
        let module = self.module_from_file(file_id);
        let search_name = Name::new(name);

        module
            .procedures()
            .into_iter()
            .chain(module.functions())
            .find(|method| method.name().eq_ignore_case(&search_name))
    }

    pub(crate) fn try_resolve_qualified_name_for_token(
        &self,
        file_id: FileId,
        token: &syntax::SyntaxToken,
    ) -> Option<crate::definition::Definition> {
        use crate::definition::Definition;

        let parent = token.parent()?;

        for ancestor in parent.ancestors() {
            if let Some(field_expr) = syntax::ast::FieldExpr::cast(ancestor.clone()) {
                let qualified_name = match self.extract_qualified_name_from_field_expr(field_expr) {
                    Some(qn) => qn,
                    None => continue,
                };
                tracing::debug!(?qualified_name, "extracted qualified name from field expr");

                let module_id = ModuleId::new(file_id);
                let resolver = hir_def::resolver::Resolver::with_workspace_scope(module_id);
                let resolution = resolver.resolve_path(self.db, &qualified_name);

                tracing::debug!(?resolution, "resolved path");

                return match resolution {
                    PathResolution::Method(method_id) => Some(Definition::Method(method_id)),
                    PathResolution::Variable(var_id) => Some(Definition::Variable(var_id)),
                    PathResolution::Builtin(name) => Some(Definition::BuiltinFunction(name)),
                    PathResolution::Unresolved(_) => None,
                };
            }

            match ancestor.kind() {
                syntax::SyntaxKind::STMT_LIST
                | syntax::SyntaxKind::SOURCE_FILE
                | syntax::SyntaxKind::PROCEDURE_DEF
                | syntax::SyntaxKind::FUNCTION_DEF => break,
                _ => {}
            }
        }

        None
    }

    pub(crate) fn try_resolve_builtin(&self, name: &str) -> Option<crate::definition::Definition> {
        if bsl_platform::PlatformDataInner::instance().get_global_function(name).is_some() {
            return Some(Definition::BuiltinFunction(Name::new(name)));
        }

        None
    }

    fn extract_qualified_name_from_field_expr(
        &self,
        field_expr: syntax::ast::FieldExpr,
    ) -> Option<QualifiedName> {
        let mut segments = Vec::new();

        let field_token = syntax::ast_utils::field_tail_name_token(field_expr.syntax())?;
        segments.push(Name::new(field_token.text()));

        let base = field_expr.syntax().children().next()?;
        extract_segments_from_expr(&base, &mut segments)?;

        segments.reverse();

        Some(QualifiedName::from_segments(segments))
    }
}

impl<'db, DB: HirDatabase + base_db::RootQueryDb> Semantics<'db, DB> {
    pub fn type_of_expr(&self, file_id: FileId, node: &syntax::SyntaxNode) -> TypeId {
        let module_id = ModuleId::new(file_id);
        let module_bodies = self.db.module_bodies_ref(module_id);
        let range = node.text_range();

        if let Some(result) = module_bodies.module_code_result() {
            if let Some(expr_id) = result.source_map().expr_at_range(range) {
                let owner = DefWithBodyId::ModuleCode;
                let routed = infer_owner(self.db, file_id, owner);
                let base_id = routed.type_id_of_expr(expr_id).unwrap_or_else(|| self.db.unknown());
                return narrow_or_base(self.db, file_id, owner, result.body(), expr_id, base_id);
            }
        }

        for (local_id, body, source_map) in module_bodies.method_bodies() {
            if let Some(expr_id) = source_map.expr_at_range(range) {
                let owner = DefWithBodyId::Method(local_id);
                let routed = infer_owner(self.db, file_id, owner);
                let base_id = routed.type_id_of_expr(expr_id).unwrap_or_else(|| self.db.unknown());
                return narrow_or_base(self.db, file_id, owner, body, expr_id, base_id);
            }
        }

        self.db.unknown()
    }

    pub fn type_of_binding_at(&self, file_id: FileId, range: TextRange) -> Option<TypeId> {
        let module_id = ModuleId::new(file_id);
        let module_bodies = self.db.module_bodies_ref(module_id);

        if let Some(result) = module_bodies.module_code_result() {
            if let Some(binding_id) = result.source_map().binding_at_range(range) {
                let routed = infer_owner(self.db, file_id, DefWithBodyId::ModuleCode);
                return routed.type_id_of_binding(binding_id);
            }
        }

        for (local_id, _body, source_map) in module_bodies.method_bodies() {
            if let Some(binding_id) = source_map.binding_at_range(range) {
                let routed = infer_owner(self.db, file_id, DefWithBodyId::Method(local_id));
                return routed.type_id_of_binding(binding_id);
            }
        }

        None
    }

    /// Expected type(s) at a completion position, used to boost candidates whose
    /// type matches. Resolves the most specific context: a call argument, then an
    /// assignment right-hand side, then a `Возврат` value. `Unknown` types are
    /// dropped so a candidate is never boosted on a bogus expectation.
    ///
    /// When the cursor is syntactically inside a call argument list, only the
    /// call-argument expectation is used (possibly empty) — assignment/return are
    /// not consulted, so an unresolved callee never leaks the outer context into
    /// its argument slot.
    pub fn expected_types_at(&self, file_id: FileId, offset: TextSize) -> Vec<TypeId> {
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();

        // Any argument list (call OR constructor) blocks the assignment/return
        // fallthrough — an unresolved callee/constructor must not leak the outer
        // expected type into its argument slot. `expected_arg_types_at` yields the
        // real parameter types for resolved calls and empty for constructors.
        let raw = if in_arg_list(&root, offset) {
            self.expected_arg_types_at(file_id, offset)
        } else if let Some(ty) = self.expected_assignment_type_at(file_id, offset) {
            vec![ty]
        } else if let Some(ty) = self.expected_return_type_at(file_id, offset) {
            vec![ty]
        } else {
            Vec::new()
        };

        raw.into_iter()
            .filter(|ty| !matches!(self.db.lookup_type(*ty), TypeKind::Unknown))
            .collect()
    }

    /// Expected type of an assignment right-hand side = the type of the assignment
    /// target. `None` unless the cursor sits past the `=` of an `ASSIGN_STMT`.
    fn expected_assignment_type_at(&self, file_id: FileId, offset: TextSize) -> Option<TypeId> {
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        let token = root.token_at_offset(offset).left_biased()?;
        let assign =
            token.parent_ancestors().find(|n| n.kind() == syntax::SyntaxKind::ASSIGN_STMT)?;
        let eq = assign.children_with_tokens().find(|c| c.kind() == syntax::SyntaxKind::EQ)?;
        if offset < eq.text_range().end() {
            return None;
        }
        let target = assign.children().next()?;
        Some(self.type_of_expr(file_id, &target))
    }

    /// Expected type of a `Возврат` value = the enclosing function's inferred
    /// return type. `None` outside a `RETURN_STMT` or inside a procedure.
    fn expected_return_type_at(&self, file_id: FileId, offset: TextSize) -> Option<TypeId> {
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        let token = root.token_at_offset(offset).left_biased()?;
        token.parent_ancestors().find(|n| n.kind() == syntax::SyntaxKind::RETURN_STMT)?;

        let method_id = self.enclosing_method_id(file_id, offset)?;
        if !Method::new(self.db, method_id).is_function() {
            return None;
        }
        Some(method_return_type(self.db, method_id))
    }

    /// The `MethodId` of the procedure/function whose source range contains
    /// `offset` (BSL has no nested methods, so at most one matches).
    fn enclosing_method_id(&self, file_id: FileId, offset: TextSize) -> Option<MethodId> {
        let tree = self.db.item_tree_ref(file_id);
        let module_id = ModuleId::new(file_id);
        tree.methods()
            .find(|method| method.source_range().contains(offset))
            .map(|method| MethodId { module: module_id, local_id: method.key() })
    }

    /// Resolves the call whose argument list contains `offset`.
    pub fn call_resolution_at(&self, file_id: FileId, offset: TextSize) -> Option<CallResolution> {
        self.call_binding_at(file_id, offset).map(|binding| binding.resolution)
    }

    /// Returns the stored candidate binding for the call whose argument list contains `offset`.
    pub fn call_binding_at(
        &self,
        file_id: FileId,
        offset: TextSize,
    ) -> Option<CandidateCallBinding> {
        let parse = self.db.parse(file_id);
        let (callee, _) = call_arg_at_offset(&parse.syntax_node(), offset)?;
        let callee_range = callee.text_range();
        let call_range = callee
            .ancestors()
            .find(|node| {
                matches!(node.kind(), syntax::SyntaxKind::CALL_EXPR | syntax::SyntaxKind::NEW_EXPR)
            })
            .map(|call| call.text_range());

        let module_id = ModuleId::new(file_id);
        let module_bodies = self.db.module_bodies(module_id);

        if let Some(result) = module_bodies.module_code_result() {
            let routed = infer_owner(self.db, file_id, DefWithBodyId::ModuleCode);
            if let Some(binding) = routed.call_arg_bindings().iter().find(|binding| {
                result.source_map().expr_range(binding.call_expr).is_some_and(|range| {
                    [Some(callee_range), call_range]
                        .into_iter()
                        .flatten()
                        .any(|target| target == range)
                })
            }) {
                return Some(binding.candidate.clone());
            }
        }
        for (local_id, _body, source_map) in module_bodies.method_bodies() {
            let routed = infer_owner(self.db, file_id, DefWithBodyId::Method(local_id));
            if let Some(binding) = routed.call_arg_bindings().iter().find(|binding| {
                source_map.expr_range(binding.call_expr).is_some_and(|range| {
                    [Some(callee_range), call_range]
                        .into_iter()
                        .flatten()
                        .any(|target| target == range)
                })
            }) {
                return Some(binding.candidate.clone());
            }
        }
        None
    }

    /// Every recorded call-argument binding in the file, keyed by the source
    /// range its binding expression was lowered from.
    ///
    /// One pass over the module's bodies, for callers that walk a whole range:
    /// [`call_binding_at`](Self::call_binding_at) rescans every body per
    /// position, which turns such a walk quadratic. The key is the callee's
    /// range for ordinary calls and the whole call's range for the shapes
    /// lowered as a qualified path, so look up both — same as `call_binding_at`
    /// matches either.
    pub fn call_bindings(&self, file_id: FileId) -> Vec<(syntax::TextRange, CandidateCallBinding)> {
        let module_id = ModuleId::new(file_id);
        let module_bodies = self.db.module_bodies(module_id);
        let mut bindings = Vec::new();

        if let Some(result) = module_bodies.module_code_result() {
            let routed = infer_owner(self.db, file_id, DefWithBodyId::ModuleCode);
            for binding in routed.call_arg_bindings() {
                if let Some(range) = result.source_map().expr_range(binding.call_expr) {
                    bindings.push((range, binding.candidate.clone()));
                }
            }
        }
        for (local_id, _body, source_map) in module_bodies.method_bodies() {
            let routed = infer_owner(self.db, file_id, DefWithBodyId::Method(local_id));
            for binding in routed.call_arg_bindings() {
                if let Some(range) = source_map.expr_range(binding.call_expr) {
                    bindings.push((range, binding.candidate.clone()));
                }
            }
        }
        bindings
    }

    /// Expected parameter type(s) at a call-argument completion position.
    ///
    /// Locates the innermost call argument list around `offset` and the active
    /// argument index by counting commas (robust to the empty/trailing slot the
    /// cursor sits in while typing, which has no lowered expression of its own),
    /// maps the call to its recorded argument bindings, and returns the parameter
    /// type(s) at that index. Several types are returned for overloaded calls — a
    /// candidate is a match if assignable to any of them. Empty when the cursor is
    /// not inside a resolvable call argument.
    fn expected_arg_types_at(&self, file_id: FileId, offset: TextSize) -> Vec<TypeId> {
        let parse = self.db.parse(file_id);
        let Some((callee, active)) = call_arg_at_offset(&parse.syntax_node(), offset) else {
            return Vec::new();
        };
        let callee_range = callee.text_range();

        let module_id = ModuleId::new(file_id);
        let module_bodies = self.db.module_bodies_ref(module_id);

        let mut found: Option<(DefWithBodyId, ExprId)> = None;
        if let Some(result) = module_bodies.module_code_result() {
            if let Some(expr_id) = result.source_map().expr_at_range(callee_range) {
                found = Some((DefWithBodyId::ModuleCode, expr_id));
            }
        }
        if found.is_none() {
            for (local_id, _body, source_map) in module_bodies.method_bodies() {
                if let Some(expr_id) = source_map.expr_at_range(callee_range) {
                    found = Some((DefWithBodyId::Method(local_id), expr_id));
                    break;
                }
            }
        }
        let Some((owner, callee_id)) = found else {
            return Vec::new();
        };

        let routed = infer_owner(self.db, file_id, owner);
        let Some(binding) = routed.call_arg_bindings().iter().find(|b| b.call_expr == callee_id)
        else {
            return Vec::new();
        };

        binding
            .candidate
            .candidates
            .as_slice()
            .iter()
            .filter_map(|candidate| candidate.params.get(active).map(|param| param.ty))
            .collect()
    }

    pub fn resolve_method_call_to_definition(
        &self,
        file_id: FileId,
        token: &syntax::SyntaxToken,
    ) -> Option<crate::definition::Definition> {
        crate::FileSymbolCtx::new(self.db, file_id).resolve_method_call_to_definition(token)
    }

    pub fn resolve_name_to_definition(
        &self,
        file_id: FileId,
        token: &syntax::SyntaxToken,
    ) -> Option<crate::definition::Definition> {
        crate::FileSymbolCtx::new(self.db, file_id).resolve_name_to_definition(token)
    }
}

/// True if a value of type `from` may be passed where `to` is expected.
pub fn type_assignable<DB: HirDatabase>(db: &DB, from: TypeId, to: TypeId) -> bool {
    hir_ty::is_assignable(db, from, to)
}

/// The inferred return type of a method (union of its `Возврат` expression types).
pub fn method_return_type<DB: HirDatabase>(db: &DB, method_id: MethodId) -> TypeId {
    method_return_type_query(db, MethodIdInput::new(db, method_id))
}

/// Whether `offset` sits inside any argument list — a call's or a constructor's
/// (`NEW_EXPR`). Used to stop the assignment/return fallthrough so an unresolved
/// callee never inherits the outer context's expected type in its argument slot.
fn in_arg_list(root: &syntax::SyntaxNode, offset: TextSize) -> bool {
    use syntax::{SyntaxKind, TokenAtOffset};

    let token = match root.token_at_offset(offset) {
        TokenAtOffset::None => return false,
        TokenAtOffset::Single(t) => t,
        TokenAtOffset::Between(left, right) => {
            if left.parent_ancestors().any(|n| n.kind() == SyntaxKind::ARG_LIST) {
                left
            } else {
                right
            }
        }
    };
    token.parent_ancestors().any(|n| n.kind() == SyntaxKind::ARG_LIST)
}

/// Find the innermost call argument list around `offset` and return the call's
/// callee expression node plus the active (zero-based) argument index. The index
/// is derived by counting commas before the cursor, so it is correct even when
/// the cursor sits in an empty or trailing argument slot that has no expression.
/// The callee node (not the whole call) is returned because argument bindings are
/// keyed by the callee expression.
fn call_arg_at_offset(
    root: &syntax::SyntaxNode,
    offset: TextSize,
) -> Option<(syntax::SyntaxNode, usize)> {
    use syntax::{SyntaxKind, TokenAtOffset};

    let token = match root.token_at_offset(offset) {
        TokenAtOffset::None => return None,
        TokenAtOffset::Single(t) => t,
        TokenAtOffset::Between(left, right) => {
            if left.parent_ancestors().any(|n| n.kind() == SyntaxKind::ARG_LIST) {
                left
            } else {
                right
            }
        }
    };

    let arg_list = token.parent_ancestors().find(|node| node.kind() == SyntaxKind::ARG_LIST)?;
    let call_expr = arg_list
        .parent()
        .filter(|p| p.kind() == SyntaxKind::CALL_EXPR || p.kind() == SyntaxKind::NEW_EXPR)?;
    let callee = match call_expr.kind() {
        SyntaxKind::CALL_EXPR => call_expr.children().next()?,
        SyntaxKind::NEW_EXPR => call_expr.clone(),
        _ => unreachable!(),
    };

    let mut active = 0usize;
    for child in arg_list.children_with_tokens() {
        if child.text_range().start() >= offset {
            break;
        }
        if child.kind() == SyntaxKind::COMMA {
            active += 1;
        }
    }

    Some((callee, active))
}

fn field_name_receiver(token: &syntax::SyntaxToken) -> Option<syntax::SyntaxNode> {
    use syntax::SyntaxKind;

    let parent = token.parent()?;
    if parent.kind() != SyntaxKind::FIELD_EXPR {
        return None;
    }
    let receiver = parent.children().next()?;
    if receiver.text_range().contains_range(token.text_range()) {
        return None;
    }
    Some(receiver)
}

fn extract_segments_from_expr(
    expr_node: &syntax::SyntaxNode,
    segments: &mut Vec<Name>,
) -> Option<()> {
    use syntax::SyntaxKind;

    match expr_node.kind() {
        SyntaxKind::FIELD_EXPR => {
            let field_expr = syntax::ast::FieldExpr::cast(expr_node.clone())?;
            let field_token = syntax::ast_utils::field_tail_name_token(field_expr.syntax())?;
            segments.push(Name::new(field_token.text()));

            let base = field_expr.syntax().children().next()?;
            extract_segments_from_expr(&base, segments)
        }
        SyntaxKind::IDENT | SyntaxKind::EXPR => {
            let name_token = expr_node
                .children_with_tokens()
                .filter_map(|it| it.into_token())
                .find(|it| it.kind().is_name_token())?;
            segments.push(Name::new(name_token.text()));
            Some(())
        }
        _ => {
            let name_token = expr_node
                .children_with_tokens()
                .filter_map(|it| it.into_token())
                .find(|it| it.kind().is_name_token())?;
            segments.push(Name::new(name_token.text()));
            Some(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, FileId, VfsPath};

    fn create_db_with_file(source: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        (db, file_id)
    }

    #[test]
    fn test_find_method_by_name() {
        let source = r#"
Процедура ПерваяПроцедура()
КонецПроцедуры

Функция ВтораяФункция() Экспорт
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);

        let method = sema.find_method(file_id, "ПерваяПроцедура");
        assert!(method.is_some());
        let method = method.unwrap();
        assert_eq!(method.name().as_str(), "ПерваяПроцедура");
        assert!(!method.is_function());
        assert!(!method.is_export());

        let method = sema.find_method(file_id, "ВтораяФункция");
        assert!(method.is_some());
        let method = method.unwrap();
        assert_eq!(method.name().as_str(), "ВтораяФункция");
        assert!(method.is_function());
        assert!(method.is_export());

        let method = sema.find_method(file_id, "НесуществующаяФункция");
        assert!(method.is_none());
    }

    #[test]
    fn test_case_insensitive_search() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);

        assert!(sema.find_method(file_id, "мояпроцедура").is_some());
        assert!(sema.find_method(file_id, "МОЯПРОЦЕДУРА").is_some());
        assert!(sema.find_method(file_id, "МоЯпРоЦеДуРа").is_some());
    }

    #[test]
    fn test_list_all_procedures() {
        let source = r#"
Процедура Первая()
КонецПроцедуры

Процедура Вторая() Экспорт
КонецПроцедуры

Функция Третья()
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);
        let module = sema.module_from_file(file_id);

        let procedures = module.procedures();
        assert_eq!(procedures.len(), 2);

        let functions = module.functions();
        assert_eq!(functions.len(), 1);

        assert_eq!(procedures[0].name().as_str(), "Первая");
        assert!(!procedures[0].is_export());

        assert_eq!(procedures[1].name().as_str(), "Вторая");
        assert!(procedures[1].is_export());

        assert_eq!(functions[0].name().as_str(), "Третья");
        assert!(!functions[0].is_export());
    }

    #[test]
    fn test_module_variables() {
        let source = r#"
Перем ПерваяПеременная;
Перем ВтораяПеременная Экспорт;

Процедура Тест()
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);
        let module = sema.module_from_file(file_id);

        let variables = module.variables();
        assert_eq!(variables.len(), 2);

        assert_eq!(variables[0].name().as_str(), "ПерваяПеременная");
        assert!(!variables[0].is_export());

        assert_eq!(variables[1].name().as_str(), "ВтораяПеременная");
        assert!(variables[1].is_export());
    }

    #[test]
    fn test_empty_module() {
        let source = "// Пустой модуль\n";

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);
        let module = sema.module_from_file(file_id);

        assert_eq!(module.procedures().len(), 0);
        assert_eq!(module.functions().len(), 0);
        assert_eq!(module.variables().len(), 0);
    }

    #[test]
    fn reference_scope_distinguishes_export_methods() {
        let source = r#"
Процедура НеЭкспорт()
КонецПроцедуры

Процедура Экспортная() Экспорт
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);

        let private = sema.find_method(file_id, "НеЭкспорт").unwrap();
        let exported = sema.find_method(file_id, "Экспортная").unwrap();

        assert_eq!(
            Definition::Method(private.id()).reference_scope(&db),
            ReferenceScope::FileLocal,
            "non-export procedure must stay file-local"
        );
        assert_eq!(
            Definition::Method(exported.id()).reference_scope(&db),
            ReferenceScope::ModuleSymbolWorkspace,
            "Экспорт procedure must reach the whole source root"
        );
    }

    #[test]
    fn reference_scope_distinguishes_export_variables() {
        let source = r#"
Перем НеЭкспорт;
Перем Экспортная Экспорт;
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);
        let module = sema.module_from_file(file_id);
        let variables = module.variables();

        let private = variables.iter().find(|v| v.name().as_str() == "НеЭкспорт").unwrap();
        let exported = variables.iter().find(|v| v.name().as_str() == "Экспортная").unwrap();

        assert_eq!(
            Definition::Variable(private.id()).reference_scope(&db),
            ReferenceScope::FileLocal,
        );
        assert_eq!(
            Definition::Variable(exported.id()).reference_scope(&db),
            ReferenceScope::ModuleSymbolWorkspace,
        );
    }

    #[test]
    fn reference_scope_keeps_parameters_and_locals_file_local() {
        let source = r#"
Процедура Контейнер()
КонецПроцедуры
        "#;
        let (db, file_id) = create_db_with_file(source);
        let sema = Semantics::new(&db);
        let method = sema.find_method(file_id, "Контейнер").unwrap();

        let parameter = Definition::Parameter {
            method_id: method.id(),
            param_name: Name::new("Параметр"),
            param_index: 0,
        };
        let local =
            Definition::Local { method_id: method.id(), var_name: Name::new("Локальная") };

        assert_eq!(parameter.reference_scope(&db), ReferenceScope::FileLocal);
        assert_eq!(local.reference_scope(&db), ReferenceScope::FileLocal);
    }

    #[test]
    fn reference_scope_returns_unknown_for_non_source_definitions() {
        let (db, _file_id) = create_db_with_file("");

        let builtin = Definition::BuiltinFunction(Name::new("Сообщить"));
        let mdo = Definition::MdoCollectionType(bsl_metadata::MdoType::Catalog);
        let virt = Definition::VirtualTableField {
            table_name: Name::new("Документ"),
            field_name: Name::new("Поле"),
        };

        assert_eq!(builtin.reference_scope(&db), ReferenceScope::Unknown);
        assert_eq!(mdo.reference_scope(&db), ReferenceScope::Unknown);
        assert_eq!(virt.reference_scope(&db), ReferenceScope::Unknown);
        assert_eq!(Definition::Unresolved.reference_scope(&db), ReferenceScope::Unknown);
    }

    #[test]
    fn name_usage_index_aggregates_files_and_normalizes_case() {
        use crate::DefDatabase;
        use base_db::SourceRootId;

        let mut db = RootDatabaseImpl::default();
        let file_a = FileId(0);
        let file_b = FileId(1);
        let file_c = FileId(2);

        let mut file_set = vfs::file_set::FileSet::new();
        file_set.insert(file_a, vfs::VfsPath::new("/a.bsl"));
        file_set.insert(file_b, vfs::VfsPath::new("/b.bsl"));
        file_set.insert(file_c, vfs::VfsPath::new("/c.bsl"));
        let source_root = base_db::SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_a, SourceRootId(0));
        db.set_file_source_root(file_b, SourceRootId(0));
        db.set_file_source_root(file_c, SourceRootId(0));
        db.set_file_text(
            file_a,
            "Процедура ОбщийМетод() Экспорт\n    ОбщийМетод();\nКонецПроцедуры\n",
        );
        db.set_file_text(file_b, "Процедура B_метод()\n    общийметод();\nКонецПроцедуры\n");
        db.set_file_text(file_c, "Процедура C_метод()\nКонецПроцедуры\n");

        let aggregator = db.name_usage_index(SourceRootId(0));
        let lookup = normalize_usage_name(&Name::new("ОбщийМетод"));

        let mut files = aggregator.files_with(&lookup).to_vec();
        files.sort_unstable();
        assert_eq!(files, vec![file_a, file_b], "file C must be excluded from the bucket");

        assert!(aggregator
            .files_with(&normalize_usage_name(&Name::new("НеУпоминается")))
            .is_empty());
    }
}
