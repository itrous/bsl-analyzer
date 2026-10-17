pub mod body;
pub mod call_graph;
pub mod call_hierarchy_index;
pub mod catch_class;
pub mod common_module_ref;
pub mod conditional_tree;
pub mod configs;
pub mod docs;
pub mod effective_module;
pub mod execution_env;
pub mod extension_merge;
pub mod graph_index;
pub(crate) mod heap_estimate;
pub mod hir;
pub mod item_tree;
pub mod method_body;
pub mod method_slab;
pub mod method_syntax;
pub mod metrics;
pub mod module_index;
pub mod module_interface;
pub mod module_structure;
pub mod name;
pub mod name_usage_index;
pub mod path;
pub mod preproc_condition;
pub mod queries;
pub mod region_tree;
pub mod resolver;
pub mod scope;
pub mod sdbl_cache;
pub mod symbol_tree;
pub mod ty;
pub mod type_ref;
pub mod weaving;
pub mod workspace;
pub mod workspace_index;

use std::sync::Arc;
use stdx::case::CaseExt;

use base_db::SourceRootId;
use vfs::FileId;

pub use body::{
    lower_method, lower_module_code, Body, BodyDiagnostic, BodySourceMap, DeprecatedKind8312,
    ExistingBindingKind, ExternalRef, LowerResult, ManagerType, RedundantAccessKind,
};
pub use call_hierarchy_index::{CallHierarchyReverseIndex, MethodCallPair};
pub use hir::{BinaryOp, Binding, Expr, IfStmt, Literal, Stmt, UnaryOp};

pub use cfg_types::{
    BindingId, ExprId, IdConversion, LocalOffset, LocalRange, MethodOffset, StmtId,
};

pub use conditional_tree::{ConditionalData, ConditionalIdx, ConditionalKind, ConditionalTree};
pub use configs::{
    ApplicationModuleKind, BodySearch, CommonModuleBodies, CommonModuleBody, ConfigsDatabase,
    MdoModuleRole,
};
pub use item_tree::ItemTree;
pub use module_index::{
    module_key_for_path, module_path_segment_modes, parse_form_module_path, FormKey, ModuleIndex,
    ModuleKey,
};
pub use module_interface::{
    interface_method_named, interface_method_query, interface_variable_named,
    interface_variable_named_query, module_declares_method_query, module_interface_query,
    module_method_names_query, module_misses_read_name_set_query, module_variable_names_query,
    MethodDecl, ModuleInterface, VariableDecl, NAME_SET_METHOD_LIMIT,
};
pub use name::Name;
pub use name_usage_index::{
    file_name_offsets_query, file_name_usage_query, normalize_match_name, normalize_name,
    source_root_name_usage_query, FileNameOffsets, FileNameUsage, SourceRootNameUsage,
};
pub use path::{PathResolution, QualifiedName};
pub use region_tree::{RegionData, RegionIdx, RegionTree};
pub use sdbl_cache::{
    all_sdbl_in_file_query, method_sdbl_hir_query, sdbl_hir_for_file_query, sdbl_package_for,
    MethodSdblHir, SdblHirEntries, SdblInFile,
};
pub use symbol_tree::{MethodSymbol, ParamSymbol, SymbolTree, VariableSymbol};
pub use ty::FunctionSignature;
pub use type_ref::{BuiltinTypeRef, TypeRef};
pub use workspace::{is_bsl_source, ModuleMembers, WorkspaceMembers};
pub use workspace_index::{SymbolInfo, SymbolKind, WorkspaceIndex};

pub use method_body::{lower_detached_method, method_body_query, method_lower_query};
pub use method_syntax::{method_syntax_query, MethodSyntax};
pub use queries::{
    conditional_tree_query, file_dependencies_query, file_external_refs_query, item_tree_query,
    method_outbound_facts, module_bodies_query, module_call_summary_query, module_code_lower_query,
    module_data_query, module_index_query, module_members_query, region_tree_query,
    resolved_module_summary_query, set_lowering_lru_sweep_mode, symbol_tree_query,
    workspace_call_graph_query, workspace_index_query, ManagerRef, MethodOutboundFacts,
};

#[salsa::db]
pub trait DefDatabase: base_db::RootQueryDb {
    fn item_tree(&self, file_id: FileId) -> Arc<ItemTree>;

    /// Borrowed variant of [`item_tree`](Self::item_tree) for read-only paths:
    /// no `Arc` refcount traffic per read (`.clone()` the result if ownership
    /// is needed after all).
    fn item_tree_ref(&self, file_id: FileId) -> &Arc<ItemTree>;

    fn region_tree(&self, file_id: FileId) -> Arc<RegionTree>;

    fn conditional_tree(&self, file_id: FileId) -> Arc<ConditionalTree>;

    /// Borrowed variant of [`conditional_tree`](Self::conditional_tree); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn conditional_tree_ref(&self, file_id: FileId) -> &Arc<ConditionalTree>;

    fn module_data(&self, module_id: ModuleId) -> Arc<ModuleData>;

    /// Position-free declarations of a module: the only view of a module's
    /// items that a method-keyed query may read.
    fn module_interface(&self, module_id: ModuleId) -> Arc<ModuleInterface>;

    /// Borrowed variant of [`module_interface`](Self::module_interface); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn module_interface_ref(&self, module_id: ModuleId) -> &Arc<ModuleInterface>;

    /// One declaration of the interface, by key: what a method-keyed query
    /// reads instead of the whole interface, so that an edit of another
    /// declaration in the file leaves its memo valid.
    fn interface_method(&self, method_id: MethodId) -> Option<Arc<MethodDecl>>;

    /// The first declaration of `name` in the module — the one a bare call
    /// resolves to; `None` when the module declares no such method.
    fn interface_method_named(&self, module_id: ModuleId, name: &Name) -> Option<Arc<MethodDecl>>;

    /// The first module variable named `name`.
    fn interface_variable_named(
        &self,
        module_id: ModuleId,
        name: &Name,
    ) -> Option<Arc<VariableDecl>>;

    fn symbol_tree(&self, module_id: ModuleId) -> Arc<SymbolTree>;

    /// Borrowed variant of [`symbol_tree`](Self::symbol_tree); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn symbol_tree_ref(&self, module_id: ModuleId) -> &Arc<SymbolTree>;

    fn module_bodies(&self, module_id: ModuleId) -> Arc<ModuleBodies>;

    /// Borrowed variant of [`module_bodies`](Self::module_bodies); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn module_bodies_ref(&self, module_id: ModuleId) -> &Arc<ModuleBodies>;

    fn method_body(&self, method: MethodIdInput<'_>) -> Arc<body::Body>;

    /// Borrowed variant of [`method_body`](Self::method_body); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn method_body_ref<'db>(&'db self, method: MethodIdInput<'db>) -> &'db Arc<body::Body>;

    /// The full lowering of one method — body, source map, diagnostics — in
    /// method-relative positions; `None` when the id names no method.
    fn method_lower(&self, method: MethodIdInput<'_>) -> Option<Arc<body::LowerResult>>;

    fn module_metadata(&self, module_id: ModuleId) -> Arc<ModuleMetadata>;

    fn module_call_summary(&self, module_id: ModuleId) -> Arc<call_graph::ModuleCallSummary>;

    fn method_docs(&self, method: MethodId) -> Option<Arc<crate::docs::MethodDocs>>;

    fn variable_docs(&self, variable: VariableId) -> Option<Arc<crate::docs::VariableDocs>>;

    fn module_members(&self, source_root_id: SourceRootId) -> Arc<WorkspaceMembers>;

    fn workspace_index(
        &self,
        source_root_id: SourceRootId,
    ) -> Arc<crate::workspace_index::WorkspaceIndex>;

    fn name_usage_index(
        &self,
        source_root_id: SourceRootId,
    ) -> Arc<crate::name_usage_index::SourceRootNameUsage>;

    fn file_name_offsets(&self, file_id: FileId) -> Arc<crate::name_usage_index::FileNameOffsets>;

    /// Borrowed variant of [`file_name_offsets`](Self::file_name_offsets); see
    /// [`item_tree_ref`](Self::item_tree_ref).
    fn file_name_offsets_ref(
        &self,
        file_id: FileId,
    ) -> &Arc<crate::name_usage_index::FileNameOffsets>;

    fn file_external_refs(&self, module_id: ModuleId) -> Arc<Vec<ExternalRef>>;

    fn module_index(&self, source_root_id: SourceRootId) -> Arc<ModuleIndex>;

    fn file_dependencies(&self, module_id: ModuleId) -> Arc<Vec<FileId>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleId {
    pub file_id: FileId,
}

impl ModuleId {
    pub fn new(file_id: FileId) -> Self {
        Self { file_id }
    }
}

/// Identity of a method within its module that survives edits elsewhere in
/// the file: the case-folded name and the ordinal among same-named
/// declarations in source order. A method added or removed above shifts
/// nobody's key, so the memos keyed by it stay valid; only a namesake added
/// above moves the ordinal of the namesakes below it. The item tree assigns
/// keys ([`ItemTree::methods`]) and every other numbering is a projection.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodKey {
    pub name: intern::NormName,
    /// Position among the module's declarations of this name, from zero.
    pub ordinal: u32,
}

impl MethodKey {
    /// The first — normally the only — declaration of `name`.
    pub fn first(name: &str) -> Self {
        Self::nth(name, 0)
    }

    pub fn nth(name: &str, ordinal: u32) -> Self {
        Self { name: intern::NormName::intern(name), ordinal }
    }
}

impl std::fmt::Debug for MethodKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.name.as_str(), self.ordinal)
    }
}

/// Ordered by the folded spelling, then the ordinal. `NormName` ids follow
/// intern order and are deliberately not `Ord`; sorts and tie-breaks over
/// methods need an order that is the same in every run, and the spelling
/// gives one. Consistent with `Eq`: equal ids are equal spellings.
impl Ord for MethodKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.name.as_str(), self.ordinal).cmp(&(other.name.as_str(), other.ordinal))
    }
}

impl PartialOrd for MethodKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodId {
    pub module: ModuleId,
    /// The method's identity local to its module — not a position.
    pub local_id: MethodKey,
}

/// A folded name looked up in one module: the key of the by-name
/// projections of the module interface.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct ModuleNameInput {
    #[returns(copy)]
    pub file_id: FileId,
    #[returns(copy)]
    pub name: intern::NormName,
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct MethodIdInput {
    #[returns(copy)]
    pub method_id: MethodId,
}

/// A module variable, addressed by its position among the module's top-level
/// items (`Перем А, Б` takes two). Variables have no per-item memo, so the
/// position is not an incrementality concern the way a method's is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VariableId {
    pub module: ModuleId,
    pub local_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBodyId {
    Method(MethodKey),
    ModuleCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SdblExprId {
    pub owner: DefWithBodyId,
    pub expr_id: ExprId,
}

impl SdblExprId {
    pub fn from_method(local_id: MethodKey, expr_id: ExprId) -> Self {
        Self { owner: DefWithBodyId::Method(local_id), expr_id }
    }

    pub fn from_module_code(expr_id: ExprId) -> Self {
        Self { owner: DefWithBodyId::ModuleCode, expr_id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleData {
    pub file_id: FileId,
    pub name: Option<Name>,
    pub procedures: Vec<MethodId>,
    pub functions: Vec<MethodId>,
    pub variables: Vec<VariableId>,
}

impl ModuleData {
    pub fn from_item_tree(module_id: ModuleId, tree: Arc<ItemTree>) -> Self {
        let mut procedures = Vec::new();
        let mut functions = Vec::new();
        let mut variables = Vec::new();

        for (idx, item) in tree.top_level_items().iter().enumerate() {
            match item {
                item_tree::ModItem::Procedure(p) => {
                    procedures
                        .push(MethodId { module: module_id, local_id: tree.procedure(*p).key });
                }
                item_tree::ModItem::Function(f) => {
                    functions.push(MethodId { module: module_id, local_id: tree.function(*f).key });
                }
                item_tree::ModItem::Variable(_) => {
                    variables.push(VariableId { module: module_id, local_id: idx as u32 });
                }
            }
        }

        ModuleData { file_id: module_id.file_id, name: None, procedures, functions, variables }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleVarDecl {
    pub name: String,
    pub range: text_size::TextRange,
    pub is_export: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionContext {
    Server,
    ServerCall,
    Client,
    ClientServer,
    ExternalConnection,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMetadata {
    pub module_type: bsl_metadata::ModuleType,
    pub execution_context: Option<ExecutionContext>,
    pub common_module: Option<Arc<bsl_metadata::CommonModule>>,
    pub mdo: Option<Arc<bsl_metadata::MetadataObject>>,
    pub register: Option<Arc<bsl_metadata::Register>>,
    pub form: Option<Arc<bsl_metadata::Form>>,
    pub http_service: Option<Arc<bsl_metadata::HTTPService>>,
    pub web_service: Option<Arc<bsl_metadata::WebService>>,
    pub integration_service: Option<Arc<bsl_metadata::IntegrationService>>,
}

impl ModuleMetadata {
    /// A common module whose flags select no environment at all: the platform accepts
    /// it and compiles its body nowhere, so nothing in it can fail (checked with
    /// `/CheckModules` for every environment on 8.3.17 and 8.3.27). Such modules occur
    /// in real configurations: a developer-tools library ships one. An empty
    /// execution environment cannot carry this: it also means "environment unknown".
    pub fn compiles_nowhere(&self) -> bool {
        if self.module_type != bsl_metadata::ModuleType::CommonModule {
            return false;
        }
        self.common_module.as_deref().is_some_and(|module| {
            !(module.is_server()
                || module.is_server_call()
                || module.is_external_connection()
                || module.is_client_managed_application()
                || module.is_client_ordinary_application())
        })
    }

    /// Whether this is the module of an ORDINARY form (a `Form.bin` dialog): code of
    /// a thick client only, with no compilation directives.
    /// Managed-form rules (directive defaults, `ЭтотОбъект`, `ОткрытьФорму`, no modal
    /// dialogs) do not apply there.
    pub fn is_ordinary_form_module(&self) -> bool {
        self.module_type == bsl_metadata::ModuleType::FormModule
            && self.form.as_ref().is_some_and(|form| form.is_ordinary())
    }

    pub fn unknown(module_type: bsl_metadata::ModuleType) -> Self {
        Self {
            module_type,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: None,
        }
    }

    /// Heap bytes owned by this metadata snapshot, memoised by `ide-db`'s
    /// `module_metadata_query` for Salsa's `heap_size` hook: each populated
    /// `Arc<T>` payload counted as `size_of::<T>()` (the heap allocation the
    /// `Arc` points at) plus `T`'s own `estimated_heap_size`. `module_type`/
    /// `execution_context` are `Copy` enums and own no heap. New heap-owning
    /// fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        let mut bytes = 0usize;
        if let Some(cm) = &self.common_module {
            bytes += std::mem::size_of::<bsl_metadata::CommonModule>() + cm.estimated_heap_size();
        }
        if let Some(mdo) = &self.mdo {
            bytes +=
                std::mem::size_of::<bsl_metadata::MetadataObject>() + mdo.estimated_heap_size();
        }
        if let Some(register) = &self.register {
            bytes += std::mem::size_of::<bsl_metadata::Register>() + register.estimated_heap_size();
        }
        if let Some(form) = &self.form {
            bytes += std::mem::size_of::<bsl_metadata::Form>() + form.estimated_heap_size();
        }
        if let Some(http_service) = &self.http_service {
            bytes += std::mem::size_of::<bsl_metadata::HTTPService>()
                + http_service.estimated_heap_size();
        }
        if let Some(web_service) = &self.web_service {
            bytes +=
                std::mem::size_of::<bsl_metadata::WebService>() + web_service.estimated_heap_size();
        }
        if let Some(integration_service) = &self.integration_service {
            bytes += std::mem::size_of::<bsl_metadata::IntegrationService>()
                + integration_service.estimated_heap_size();
        }
        bytes
    }
}

#[cfg(test)]
mod module_metadata_heap_tests {
    use super::*;

    #[test]
    fn module_metadata_heap_counts_populated_common_module() {
        let empty = ModuleMetadata::unknown(bsl_metadata::ModuleType::CommonModule);
        assert_eq!(empty.estimated_heap_size(), 0);

        let long_name = "ОбщийМодульСДлиннымИменемДляТеста".to_string();
        let name_capacity = long_name.capacity();
        let common_module = bsl_metadata::CommonModule::builder().name(long_name).build();
        let mut with_module = empty;
        with_module.common_module = Some(Arc::new(common_module));

        let bytes = with_module.estimated_heap_size();
        // At least the owned name string plus the boxed struct's own fields;
        // well under a kilobyte for a single common module.
        assert!(bytes > name_capacity);
        assert!(bytes < 1024);
    }
}

/// One method's lowering placed in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MethodEntry {
    lower: Arc<body::LowerResult>,
    base: MethodOffset,
}

/// A lowered body placed in its file: the lowering's own positions are
/// relative to its root, and this pairs them with where that root sits.
#[derive(Debug, Clone, Copy)]
pub struct LowerResultAt<'a> {
    pub result: &'a body::LowerResult,
    pub base: MethodOffset,
}

impl<'a> LowerResultAt<'a> {
    pub fn body(&self) -> &'a Body {
        &self.result.body
    }

    pub fn source_map(&self) -> body::SourceMapAt<'a> {
        body::SourceMapAt::new(&self.result.source_map, self.base)
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = BodyDiagnostic> + 'a {
        let base = self.base;
        self.result.diagnostics.iter().map(move |d| d.clone().lift(base))
    }

    pub fn external_refs(&self) -> impl Iterator<Item = body::ExternalRef> + 'a {
        let base = self.base;
        self.result.external_refs.iter().map(move |r| r.clone().lift(base))
    }

    pub fn referenced_externals(&self) -> &'a rustc_hash::FxHashSet<intern::NormName> {
        &self.result.referenced_externals
    }

    pub fn size_lines(&self) -> u32 {
        self.result.size_lines
    }
}

/// Every lowered body of a file, in item order, with the file positions the
/// per-method lowerings do not know. Built as a fold over `method_lower_query`
/// on the database path and from detached nodes on the pure path, so both
/// produce the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBodies {
    /// In item order, so iteration follows the file.
    bodies: indexmap::IndexMap<MethodKey, MethodEntry>,
    /// Lifted, in method order; module code last. `diagnostic_spans` slices it.
    all_diagnostics: Vec<(DefWithBodyId, BodyDiagnostic)>,
    diagnostic_spans: rustc_hash::FxHashMap<DefWithBodyId, (usize, usize)>,
    module_vars: Vec<ModuleVarDecl>,
    module_code: Option<Arc<body::LowerResult>>,
}

impl ModuleBodies {
    pub fn new() -> Self {
        Self {
            bodies: indexmap::IndexMap::new(),
            all_diagnostics: Vec::new(),
            diagnostic_spans: rustc_hash::FxHashMap::default(),
            module_vars: Vec::new(),
            module_code: None,
        }
    }

    pub fn from_parse(parse: &syntax::Parse<syntax::SyntaxNode>) -> Self {
        let root = parse.syntax_node();
        Self::lower_from_root(&root, &ItemTree::from_parse(parse), None)
    }

    /// Like [`Self::from_parse`] but with a line index built from `source_text`, so
    /// line-dependent lowering of the module code matches the disk-backed
    /// [`lower_module_bodies`] path. Used when the parse comes from assembled
    /// text that has no `file_id` to read through `db.file_text`.
    pub fn from_parse_with_text(
        parse: &syntax::Parse<syntax::SyntaxNode>,
        source_text: &str,
    ) -> Self {
        let root = parse.syntax_node();
        let line_index = std::sync::Arc::new(line_index::LineIndex::new(source_text));
        Self::lower_from_root(&root, &ItemTree::from_parse(parse), Some(line_index))
    }

    /// The item tree names the methods and their keys; the syntax walk finds
    /// their nodes. Both enumerate the file's methods in document order — a
    /// method never nests in another — so they are joined by position, and
    /// the key is never derived a second time.
    fn lower_from_root(
        root: &syntax::SyntaxNode,
        item_tree: &ItemTree,
        line_index: Option<std::sync::Arc<line_index::LineIndex>>,
    ) -> Self {
        use syntax::SyntaxKind;

        let module_vars = collect_module_vars_of_root(root);
        let method_nodes = root
            .descendants()
            .filter(|node| {
                matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
            })
            .collect::<Vec<_>>();
        let items = item_tree.methods().collect::<Vec<_>>();
        assert_eq!(
            method_nodes.len(),
            items.len(),
            "the item tree and the syntax tree enumerate the same methods"
        );

        let methods = items.into_iter().zip(method_nodes).map(|(item, node)| {
            debug_assert_eq!(node.text_range(), item.source_range());
            let base = MethodOffset::new(node.text_range().start());
            let detached = crate::method_syntax::detach(&node);
            let lower = crate::method_body::lower_detached_method(&detached, item.is_function());
            (item.key(), Arc::new(lower), base)
        });
        let module_code = Arc::new(body::lower_module_code(root, line_index));
        Self::assemble(methods, module_vars, Some(module_code))
    }

    /// Fold the per-body lowerings into the file view, lifting diagnostics once.
    fn assemble(
        methods: impl Iterator<Item = (MethodKey, Arc<body::LowerResult>, MethodOffset)>,
        module_vars: Vec<ModuleVarDecl>,
        module_code: Option<Arc<body::LowerResult>>,
    ) -> Self {
        let mut result = ModuleBodies::new();
        for (key, lower, base) in methods {
            let owner = DefWithBodyId::Method(key);
            let start = result.all_diagnostics.len();
            result
                .all_diagnostics
                .extend(lower.diagnostics.iter().map(|d| (owner, d.clone().lift(base))));
            result.diagnostic_spans.insert(owner, (start, result.all_diagnostics.len()));
            result.bodies.insert(key, MethodEntry { lower, base });
        }

        {
            let mut seen_names: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
            result.module_vars = module_vars;
            result.module_vars.retain(|var| {
                let key = var.name.fold_lower();
                seen_names.insert(key)
            });
        }

        if let Some(module_code) = module_code {
            let owner = DefWithBodyId::ModuleCode;
            let start = result.all_diagnostics.len();
            result.all_diagnostics.extend(
                module_code.diagnostics.iter().map(|d| (owner, d.clone().lift(MethodOffset::ZERO))),
            );
            result.diagnostic_spans.insert(owner, (start, result.all_diagnostics.len()));
            result.module_code = Some(module_code);
        }

        result
    }

    pub fn body(&self, key: MethodKey) -> Option<&Body> {
        self.bodies.get(&key).map(|e| &*e.lower.body)
    }

    /// Where the method's lowering root sits in the file.
    pub fn method_offset(&self, key: MethodKey) -> Option<MethodOffset> {
        self.bodies.get(&key).map(|e| e.base)
    }

    pub fn source_map(&self, key: MethodKey) -> Option<body::SourceMapAt<'_>> {
        self.bodies.get(&key).map(|e| body::SourceMapAt::new(&e.lower.source_map, e.base))
    }

    /// The body's diagnostics in file positions.
    pub fn diagnostics(
        &self,
        owner: DefWithBodyId,
    ) -> Option<impl Iterator<Item = &BodyDiagnostic>> {
        let &(start, end) = self.diagnostic_spans.get(&owner)?;
        Some(self.all_diagnostics[start..end].iter().map(|(_, d)| d))
    }

    pub fn all_diagnostics(&self) -> &[(DefWithBodyId, BodyDiagnostic)] {
        &self.all_diagnostics
    }

    pub fn lower_result(&self, key: MethodKey) -> Option<LowerResultAt<'_>> {
        self.bodies.get(&key).map(|e| LowerResultAt { result: &e.lower, base: e.base })
    }

    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    pub fn iter_bodies(&self) -> impl Iterator<Item = (MethodKey, &Body)> {
        self.bodies.iter().map(|(key, e)| (*key, &*e.lower.body))
    }

    /// The bodies as the shared handles the lowering memos hold, for a result
    /// that keeps a body alongside its own data without copying it.
    pub fn iter_body_arcs(&self) -> impl Iterator<Item = (MethodKey, &Arc<Body>)> {
        self.bodies.iter().map(|(key, e)| (*key, &e.lower.body))
    }

    pub fn module_code_arc(&self) -> Option<&Arc<Body>> {
        self.module_code.as_ref().map(|r| &r.body)
    }

    pub fn method_bodies(&self) -> impl Iterator<Item = (MethodKey, &Body, body::SourceMapAt<'_>)> {
        self.bodies.iter().map(|(key, e)| {
            (*key, &*e.lower.body, body::SourceMapAt::new(&e.lower.source_map, e.base))
        })
    }

    pub fn iter_lower_results(&self) -> impl Iterator<Item = (MethodKey, LowerResultAt<'_>)> {
        self.bodies.iter().map(|(key, e)| (*key, LowerResultAt { result: &e.lower, base: e.base }))
    }

    pub fn module_code(&self) -> Option<&Body> {
        self.module_code.as_ref().map(|r| &*r.body)
    }

    /// Module-level code is lowered from the file root, so it sits at offset zero.
    pub fn module_code_result(&self) -> Option<LowerResultAt<'_>> {
        self.module_code.as_ref().map(|r| LowerResultAt { result: r, base: MethodOffset::ZERO })
    }

    pub fn module_vars(&self) -> &[ModuleVarDecl] {
        &self.module_vars
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report. The
    /// per-body lowerings are shared with their own memos and counted here
    /// again, so the file view reports what it keeps alive, not what it owns.
    pub fn estimated_heap(&self) -> usize {
        use crate::heap_estimate::{map_table_bytes, vec_bytes};

        let mut bytes = vec_bytes::<(MethodKey, MethodEntry)>(self.bodies.len());
        bytes += map_table_bytes::<MethodKey, usize>(self.bodies.len());
        for e in self.bodies.values() {
            bytes += crate::body::lower_result_heap(&Some(Arc::clone(&e.lower)));
        }

        bytes += vec_bytes::<(DefWithBodyId, BodyDiagnostic)>(self.all_diagnostics.len());
        bytes += map_table_bytes::<DefWithBodyId, (usize, usize)>(self.diagnostic_spans.len());
        bytes += vec_bytes::<ModuleVarDecl>(self.module_vars.len());
        for var in &self.module_vars {
            bytes += var.name.capacity();
        }
        if let Some(module_code) = &self.module_code {
            bytes += crate::body::lower_result_heap(&Some(Arc::clone(module_code)));
        }

        bytes
    }
}

/// Heap bytes owned by an [`body::ExternalRef`]'s `Name` fields (their `SmolStr`
/// payloads when not inlined).
fn external_ref_name_heap(ext: &body::ExternalRef) -> usize {
    use crate::heap_estimate::name_bytes;
    match ext {
        body::ExternalRef::QualifiedCall { receiver, method, .. } => {
            name_bytes(receiver) + name_bytes(method)
        }
        body::ExternalRef::ManagerAccess { object_name, method, .. } => {
            name_bytes(object_name) + method.as_ref().map_or(0, name_bytes)
        }
    }
}

impl Default for ModuleBodies {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify a common module's effective client/server dispatch capability from its
/// metadata flags. This is the *dispatch* model: the result is consumed only through
/// [`call_graph::MethodDispatch::from_execution_context`], which collapses it to
/// can-run-on-client / can-run-on-server.
///
/// Deliberately distinct from the richer naming-rule predicates in
/// `ide-diagnostics::common_module_helpers` (`is_client`/`is_server`/…): those take a
/// configuration-level `ordinary_app_support` flag and gate on the legacy
/// `ClientOrdinaryApplication`, because they answer "what should this module be *named*".
/// Here we answer "where does its code *run*", for which the managed-application boundary
/// (`ClientManagedApplication` for client, `Server`/`ExternalConnection` for server) is
/// what matters. The two never decide the same thing for the same module. Threading
/// `ordinary_app_support` to fully unify them is a worthwhile follow-up but out of scope.
pub fn compute_execution_context(common_module: &bsl_metadata::CommonModule) -> ExecutionContext {
    // `ServerCall` (вызов сервера) executes on the server regardless of which client
    // contexts may also be set, so it short-circuits. Real 1C never emits `ServerCall`
    // alongside client flags; for malformed XML this stays on the safe (server) side.
    if common_module.is_server_call() {
        return ExecutionContext::ServerCall;
    }

    // Capability model, not mutually-exclusive buckets: a module compiles into
    // every context whose flag is set, and `ExternalConnection` is a *server-side*
    // (non-interactive) context — not a third axis. `ClientOrdinaryApplication` (the
    // legacy thick client) is intentionally NOT treated as client-capable: server
    // modules like `ОбщегоНазначения` carry `ClientOrdinaryApplication=true`, so
    // honouring it would wrongly make them client-capable. The previous `!is_external`
    // guards collapsed the common `Server + ExternalConnection` server module to
    // `Unknown`, which then fell back to the client-only annotation default.
    let is_server = common_module.is_server();
    let is_external = common_module.is_external_connection();
    let runs_on_client = common_module.is_client_managed_application();
    let runs_on_server = is_server || is_external;

    match (runs_on_client, runs_on_server) {
        (true, true) => ExecutionContext::ClientServer,
        // A non-interactive server-side module: `Server` when the explicit server
        // flag is set, otherwise external-connection-only.
        (false, true) if is_server => ExecutionContext::Server,
        (false, true) => ExecutionContext::ExternalConnection,
        (true, false) => ExecutionContext::Client,
        (false, false) => ExecutionContext::Unknown,
    }
}

/// The file view over the per-method memos: each method is lowered once, by
/// `method_lower_query`, and an edit in one method leaves the others' memos
/// standing.
pub fn lower_module_bodies(db: &dyn DefDatabase, module_id: ModuleId) -> ModuleBodies {
    let file_id = module_id.file_id;
    let item_tree = db.item_tree_ref(file_id);
    let methods = item_tree.methods().filter_map(|item| {
        let input = MethodIdInput::new(db, MethodId { module: module_id, local_id: item.key() });
        let lower = crate::method_body::method_lower_query(db, input).as_ref()?;
        Some((item.key(), Arc::clone(lower), MethodOffset::new(item.source_range().start())))
    });

    let parse = db.parse_ref(file_id);
    let module_vars = collect_module_vars_of_root(&parse.syntax_node());
    let module_code =
        crate::queries::module_code_lower_query(db, base_db::FileIdInput::new(db, file_id));

    ModuleBodies::assemble(methods, module_vars, Some(Arc::clone(module_code)))
}

fn is_inside_method(node: &syntax::SyntaxNode) -> bool {
    use syntax::SyntaxKind;
    node.ancestors()
        .any(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
}

/// Every module-level `Перем` of the file, in source order. A declaration
/// under a module-level `#Если` is a descendant of the directive node, not a
/// child of the root, so the walk is over descendants — the same walk the item
/// tree lowers by. Both builders of `ModuleBodies` use this one.
fn collect_module_vars_of_root(root: &syntax::SyntaxNode) -> Vec<ModuleVarDecl> {
    let mut vars = Vec::new();
    for node in root.descendants().filter(|n| n.kind() == syntax::SyntaxKind::VAR_DEF) {
        if !is_inside_method(&node) {
            collect_module_vars(&node, &mut vars);
        }
    }
    vars
}

fn collect_module_vars(var_def: &syntax::SyntaxNode, vars: &mut Vec<ModuleVarDecl>) {
    use syntax::ast::AstNode;

    let Some(var_def) = syntax::ast::VarDef::cast(var_def.clone()) else {
        return;
    };
    // `Экспорт` exports only the name it follows (`Перем А Экспорт, Б;`).
    for (token, is_export) in var_def.names_with_export() {
        vars.push(ModuleVarDecl {
            name: token.text().to_string(),
            range: token.text_range(),
            is_export,
        });
    }
}

#[cfg(test)]
mod module_bodies_order_tests {
    use super::*;

    fn lower(code: &str) -> ModuleBodies {
        ModuleBodies::from_parse(&parser::parse(code))
    }

    fn keys(bodies: &ModuleBodies) -> Vec<MethodKey> {
        bodies.iter_bodies().map(|(key, _)| key).collect()
    }

    #[test]
    fn iter_bodies_follows_item_tree_order() {
        let code = "\
Процедура Первая() КонецПроцедуры
Функция Вторая() КонецФункции
Процедура Третья() КонецПроцедуры
";
        let bodies = lower(code);
        assert_eq!(
            keys(&bodies),
            [MethodKey::first("Первая"), MethodKey::first("Вторая"), MethodKey::first("Третья")]
        );
    }

    #[test]
    fn iter_bodies_stable_across_repeated_calls() {
        let code = "\
Процедура Альфа() КонецПроцедуры
Процедура Бета() КонецПроцедуры
Процедура Гамма() КонецПроцедуры
Процедура Дельта() КонецПроцедуры
";
        let bodies = lower(code);
        let first = keys(&bodies);
        for _ in 0..5 {
            assert_eq!(first, keys(&bodies), "iteration order must be stable across calls");
        }
    }

    #[test]
    fn method_bodies_and_lower_results_share_order() {
        let code = "\
Процедура А() КонецПроцедуры
Перем М;
Функция Б() КонецФункции
Процедура В() КонецПроцедуры
";
        let bodies = lower(code);
        let from_iter = keys(&bodies);
        let from_method_bodies: Vec<_> = bodies.method_bodies().map(|(id, _, _)| id).collect();
        let from_lower_results: Vec<_> = bodies.iter_lower_results().map(|(id, _)| id).collect();
        assert_eq!(from_iter, from_method_bodies);
        assert_eq!(from_iter, from_lower_results);
    }

    /// A variable above the method is not part of its key, and a second
    /// declaration of the same name is the second key of that name.
    #[test]
    fn keys_ignore_variables_and_count_namesakes() {
        let bodies = lower("Перем A, B; Процедура P() КонецПроцедуры Процедура P() КонецПроцедуры");

        assert_eq!(keys(&bodies), [MethodKey::first("P"), MethodKey::nth("P", 1)]);
        assert!(bodies.body(MethodKey::first("p")).is_some(), "the key folds case");
        assert!(bodies.body(MethodKey::nth("P", 2)).is_none());
        assert!(bodies.body(MethodKey::first("A")).is_none());
    }
}

#[cfg(test)]
mod execution_context_tests {
    use super::*;
    use crate::call_graph::MethodDispatch;

    /// Flags from the named real-world shape; everything not mentioned is `false`.
    fn cm(
        server: bool,
        external: bool,
        client_managed: bool,
        client_ordinary: bool,
        server_call: bool,
    ) -> bsl_metadata::CommonModule {
        bsl_metadata::CommonModule::builder()
            .name("Модуль")
            .server(server)
            .external_connection(external)
            .client_managed_application(client_managed)
            .client_ordinary_application(client_ordinary)
            .server_call(server_call)
            .build()
    }

    #[test]
    fn server_module_with_external_connection_is_server() {
        // The `ОбщегоНазначения` shape: Server + ExternalConnection + ClientOrdinary,
        // no managed client. Must be server-only — the `!is_external` guard used to
        // collapse this to `Unknown` → client-only dispatch.
        let ctx = compute_execution_context(&cm(true, true, false, true, false));
        assert_eq!(ctx, ExecutionContext::Server);
        let d = MethodDispatch::from_execution_context(ctx).unwrap();
        assert!(d.is_server_only(), "server common module must dispatch server-only");
    }

    #[test]
    fn server_without_external_is_still_server() {
        assert_eq!(
            compute_execution_context(&cm(true, false, false, false, false)),
            ExecutionContext::Server
        );
    }

    #[test]
    fn managed_client_and_server_is_client_server() {
        assert_eq!(
            compute_execution_context(&cm(true, true, true, true, false)),
            ExecutionContext::ClientServer
        );
    }

    #[test]
    fn managed_client_plus_external_runs_on_both() {
        // ExternalConnection is a server-side context, so a managed client that also
        // compiles for external connection is client+server, not client-only.
        assert_eq!(
            compute_execution_context(&cm(false, true, true, false, false)),
            ExecutionContext::ClientServer
        );
    }

    #[test]
    fn managed_client_only_is_client() {
        assert_eq!(
            compute_execution_context(&cm(false, false, true, true, false)),
            ExecutionContext::Client
        );
    }

    #[test]
    fn external_connection_only_is_external() {
        let ctx = compute_execution_context(&cm(false, true, false, false, false));
        assert_eq!(ctx, ExecutionContext::ExternalConnection);
        assert!(MethodDispatch::from_execution_context(ctx).unwrap().is_server_only());
    }

    #[test]
    fn server_call_takes_precedence() {
        assert_eq!(
            compute_execution_context(&cm(true, false, false, false, true)),
            ExecutionContext::ServerCall
        );
    }

    #[test]
    fn server_call_wins_even_with_client_flags() {
        // Malformed/mixed shape: ServerCall must still short-circuit to server-side.
        assert_eq!(
            compute_execution_context(&cm(true, true, true, true, true)),
            ExecutionContext::ServerCall
        );
    }

    #[test]
    fn ordinary_client_only_is_unknown_not_client() {
        // Legacy thick-client flag alone is intentionally not client-capable in the
        // dispatch model — it must not be classified as `Client` (which would force
        // module-level client-only dispatch); it stays `Unknown` and defers to the
        // per-method annotation default.
        assert_eq!(
            compute_execution_context(&cm(false, false, false, true, false)),
            ExecutionContext::Unknown
        );
    }

    #[test]
    fn all_flags_false_is_unknown() {
        let ctx = compute_execution_context(&cm(false, false, false, false, false));
        assert_eq!(ctx, ExecutionContext::Unknown);
        assert!(MethodDispatch::from_execution_context(ctx).is_none());
    }
}
