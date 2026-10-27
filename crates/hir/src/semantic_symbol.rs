use std::cell::{OnceCell, RefCell};
use std::sync::Arc;

use crate::{Definition, Name, NameClass, ReferenceScope, Semantics};
use bsl_types::builders::Builders;
use bsl_types::kind::{TypeId, TypeKind};
use hir_def::scope::{ExprScopes, ScopeDef};
use hir_def::{
    BindingId, DefDatabase, DefWithBodyId, ExprId, LowerResultAt, MethodId, MethodIdInput,
    MethodKey, MethodOffset, ModuleId, VariableId,
};
use hir_ty::narrow::{NarrowExprIndex, NarrowState};
use hir_ty::{db::HirDatabase, ImplicitLocalInfo};
use rustc_hash::FxHashMap;
use stdx::case::{fold_lower_per_char, CaseExt};
use syntax::ast::{self, AstNode};
use syntax::{SyntaxKind, TextRange, TextSize};
use vfs::FileId;

/// What makes two occurrences the SAME symbol.
///
/// A key holds what a symbol IS, never which occurrence was asked about: the reference
/// walk compares keys, so a key that varied per occurrence — the occurrence's own range,
/// say — would split one symbol into as many slices as it has occurrences, and each slice
/// would then be reported as a complete answer.
///
/// A variable a body never declares is identified exactly like a declared one — by its
/// owner and its folded name. A member of a typed receiver has neither declaration nor
/// definition; it is identified by the RECEIVER it is read from and the folded field name.
/// Which assignment an occurrence reads its declaration and type from is a per-occurrence
/// choice, and it lives outside the key, on the symbol.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SemanticSymbolKey {
    Definition(Definition),
    BodyLocal { file_id: FileId, owner: DefWithBodyId, name_lower: String },
    ImplicitLocal { file_id: FileId, owner: DefWithBodyId, name_lower: String },
    TypedMember { receiver: MemberReceiver, name_lower: String },
}

/// What makes two typed-receiver expressions the same place a member can be read from.
///
/// The receiver's TYPE is not that identity: two locals built as `Новый Структура("Поле", …)`
/// share one structural `TypeId` — equal keys, equal soft value types — so keying by the type
/// would merge fields of two different locals into one symbol. What the receiver MEANS
/// separates them, and that is the symbol the receiver names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemberReceiver {
    /// The receiver names a place — a local (`С.Поле`), a definition, or another member
    /// (`А.Б.Поле`); the identity is that key, recursively.
    Symbol(Box<SemanticSymbolKey>),
    /// The receiver names nothing the identity rule can take (`Получить().Поле`): there is
    /// no identity beyond the spelling, and the member answers for its own occurrence alone.
    Spelling { file_id: FileId, range: TextRange },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticSymbolKind {
    Function,
    Method,
    Parameter,
    Variable,
    Property,
    Type,
    Namespace,
    Class,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSymbol {
    pub key: SemanticSymbolKey,
    pub name: Name,
    pub kind: SemanticSymbolKind,
    pub definition: Option<Definition>,
    pub declaration: Option<SymbolDeclaration>,
    pub ty: Option<TypeId>,
}

impl SemanticSymbol {
    pub fn reference_scope(&self, db: &dyn DefDatabase) -> ReferenceScope {
        if let Some(def) = self.definition.as_ref() {
            return def.reference_scope(db);
        }
        match &self.key {
            SemanticSymbolKey::BodyLocal { .. } | SemanticSymbolKey::ImplicitLocal { .. } => {
                ReferenceScope::FileLocal
            }
            SemanticSymbolKey::TypedMember { .. } | SemanticSymbolKey::Definition(_) => {
                ReferenceScope::Unknown
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDeclaration {
    pub file_id: FileId,
    pub range: TextRange,
    pub name: Name,
    pub kind: SemanticSymbolKind,
}

impl<'db, DB: HirDatabase + base_db::RootQueryDb> Semantics<'db, DB> {
    pub fn symbol_at(&self, file_id: FileId, offset: TextSize) -> Option<SemanticSymbol> {
        let parse = self.db.parse(file_id);
        let root = parse.syntax_node();
        let token = root.token_at_offset(offset).right_biased()?;
        self.symbol_for_token(file_id, &token)
    }

    pub fn symbol_for_token(
        &self,
        file_id: FileId,
        token: &syntax::SyntaxToken,
    ) -> Option<SemanticSymbol> {
        FileSymbolCtx::new(self.db, file_id).symbol_for_token(token)
    }
}

/// Per-file symbol resolution that shares lookup state across tokens.
///
/// `Semantics::symbol_for_token` re-derives everything from the database on
/// every call, which is fine for a single hover but quadratic when a caller
/// resolves every name token of a file (semantic highlighting): each token
/// re-scanned all method bodies (deep-cloning the matched one), re-built
/// `ExprScopes`, and re-collected the global common-module exports. This
/// context is built once per file and reuses those structures across tokens.
/// The one-shot `Semantics` entry points delegate here, so both paths resolve
/// identically.
pub struct FileSymbolCtx<'db, DB: HirDatabase + base_db::RootQueryDb> {
    sema: Semantics<'db, DB>,
    file_id: FileId,
    module_id: ModuleId,
    /// Method-def syntax range → the method's key, under which the lowered
    /// body is filed.
    method_ranges: FxHashMap<TextRange, MethodKey>,
    /// When false, explicit binding symbols skip type inference: highlighting
    /// never reads `SemanticSymbol::ty`, and the binding type is the only
    /// symbol field that forces inference for otherwise-syntactic tokens.
    binding_types: bool,
    /// Per-owner binding buckets keyed by `fold_lower_per_char` — the key
    /// equality that matches `eq_ignore_case` — keeping the first binding in
    /// iteration order, like the linear `find` it replaces.
    owner_bindings: RefCell<FxHashMap<DefWithBodyId, Arc<FxHashMap<String, BindingId>>>>,
    expr_scopes: RefCell<FxHashMap<MethodKey, Arc<ExprScopes>>>,
    /// Memoized scope resolutions keyed by the token text as written, so a
    /// cached `Definition` always embeds the requested casing.
    local_defs: RefCell<FxHashMap<(MethodKey, String), Option<Definition>>>,
    module_methods: RefCell<FxHashMap<String, Option<MethodId>>>,
    module_vars: RefCell<FxHashMap<String, Option<VariableId>>>,
    global_exports: OnceCell<FxHashMap<String, Definition>>,
    /// Narrowing dataflow per owner. `narrow_or_base` re-runs the whole
    /// dataflow solve on every call (`db.narrow` is not a tracked query), so
    /// resolving it per path token is quadratic in the body size; the paired
    /// index replaces `containing_vertex`'s per-lookup CFG scan.
    narrow_cache: RefCell<FxHashMap<DefWithBodyId, NarrowEntry>>,
}

type NarrowEntry = Option<(Arc<dataflow::DataflowResult<NarrowState>>, Arc<NarrowExprIndex>)>;

/// The token that names the receiver, when the receiver is name-shaped.
///
/// A bare identifier names itself, and a chain names what its tail names (`А.Б.Поле` reads
/// the member of what `А.Б` is). A call result, a parenthesized expression and an inline
/// constructor name nothing.
fn receiver_name_token(receiver: &syntax::SyntaxNode) -> Option<syntax::SyntaxToken> {
    match receiver.kind() {
        SyntaxKind::IDENT => receiver.first_token(),
        SyntaxKind::FIELD_EXPR => syntax::ast_utils::field_tail_name_token(receiver),
        _ => None,
    }
}

impl<'db, DB: HirDatabase + base_db::RootQueryDb> FileSymbolCtx<'db, DB> {
    pub fn new(db: &'db DB, file_id: FileId) -> Self {
        let module_id = ModuleId::new(file_id);
        let tree = db.item_tree(file_id);
        let mut method_ranges = FxHashMap::default();
        for method in tree.methods() {
            method_ranges.entry(method.source_range()).or_insert(method.key());
        }
        Self {
            sema: Semantics::new(db),
            file_id,
            module_id,
            method_ranges,
            binding_types: true,
            owner_bindings: RefCell::new(FxHashMap::default()),
            expr_scopes: RefCell::new(FxHashMap::default()),
            local_defs: RefCell::new(FxHashMap::default()),
            module_methods: RefCell::new(FxHashMap::default()),
            module_vars: RefCell::new(FxHashMap::default()),
            global_exports: OnceCell::new(),
            narrow_cache: RefCell::new(FxHashMap::default()),
        }
    }

    /// Skip inferring explicit binding types (`SemanticSymbol::ty` stays
    /// `None` for them). For callers that never read the type.
    pub fn without_binding_types(mut self) -> Self {
        self.binding_types = false;
        self
    }

    fn db(&self) -> &'db DB {
        self.sema.db
    }

    pub fn symbol_for_token(&self, token: &syntax::SyntaxToken) -> Option<SemanticSymbol> {
        match crate::classify_token(token) {
            NameClass::FreeName { token, is_call } => self.symbol_for_free_name(&token, is_call),
            NameClass::FieldName { receiver, token, is_call } => {
                self.symbol_for_field_name(&receiver, &token, is_call)
            }
            NameClass::TypeRef { token } => self.symbol_for_type_ref(&token),
            NameClass::Literal { .. } | NameClass::Keyword { .. } | NameClass::Other => None,
        }
    }

    fn symbol_for_free_name(
        &self,
        token: &syntax::SyntaxToken,
        is_call: bool,
    ) -> Option<SemanticSymbol> {
        if is_call {
            if let Some(definition) = self.resolve_bare_call_to_definition(token) {
                return Some(symbol_from_definition(self.db(), definition, None));
            }
        }

        // A global common module export shadows a same-named platform global, but a local or
        // same-module symbol wins — the helper gates on those. Checked before the builtin
        // short-circuit so Local → Module → Global-CM → Platform holds for goto/hover/refs too.
        if let Some(definition) = self.global_export_definition(token) {
            return Some(symbol_from_definition(self.db(), definition, None));
        }

        if let Some(symbol) = self.symbol_for_body_local(token) {
            return Some(symbol);
        }

        let definition = self.resolve_name_to_definition(token)?;
        Some(symbol_from_definition(self.db(), definition, None))
    }

    fn symbol_for_field_name(
        &self,
        receiver: &syntax::SyntaxNode,
        token: &syntax::SyntaxToken,
        is_call: bool,
    ) -> Option<SemanticSymbol> {
        let method = || {
            self.resolve_method_call_to_definition(token)
                .map(|definition| symbol_from_definition(self.db(), definition, None))
        };
        let property = || self.symbol_for_typed_property(receiver, token);

        if is_call {
            method().or_else(property).or_else(|| {
                self.resolve_name_to_definition(token)
                    .map(|definition| symbol_from_definition(self.db(), definition, None))
            })
        } else {
            property().or_else(method).or_else(|| {
                self.resolve_name_to_definition(token)
                    .map(|definition| symbol_from_definition(self.db(), definition, None))
            })
        }
    }

    fn symbol_for_typed_property(
        &self,
        receiver: &syntax::SyntaxNode,
        token: &syntax::SyntaxToken,
    ) -> Option<SemanticSymbol> {
        let receiver_id = self.type_of_expr(receiver);
        if matches!(self.db().lookup_type(receiver_id), TypeKind::Unknown) {
            return None;
        }

        let obj_resolver = hir_ty::DbObjectResolver::new(self.db(), self.file_id);
        let name = Name::new(token.text());
        let field = hir_ty::lookup_field(self.db(), &obj_resolver, receiver_id, &name)?;
        Some(SemanticSymbol {
            key: SemanticSymbolKey::TypedMember {
                receiver: self.member_receiver(receiver),
                name_lower: name.as_str().fold_lower(),
            },
            name: field.name,
            kind: SemanticSymbolKind::Property,
            definition: None,
            declaration: None,
            ty: Some(field.ty),
        })
    }

    /// What the receiver expression names, for [`SemanticSymbolKey::TypedMember`].
    ///
    /// Only a name-shaped receiver has an identity: an identifier (`С.Поле`), or a chain
    /// ending in one (`А.Б.Поле` reads the member of what `А.Б` means). Anything else — a
    /// parenthesized expression, a call result, an inline constructor — names nothing, and
    /// its member keeps to the spelling of its own occurrence. A definition the receiver names
    /// is stored folded: the key is compared by derived equality, and BSL does not tell
    /// `Справочник1` from `СПРАВОЧНИК1`.
    fn member_receiver(&self, receiver: &syntax::SyntaxNode) -> MemberReceiver {
        if let Some(name_token) = receiver_name_token(receiver) {
            if let Some(symbol) = self.symbol_for_token(&name_token) {
                let key = match symbol.key {
                    SemanticSymbolKey::Definition(definition) => {
                        SemanticSymbolKey::Definition(definition.folded())
                    }
                    key => key,
                };
                return MemberReceiver::Symbol(Box::new(key));
            }
        }
        MemberReceiver::Spelling { file_id: self.file_id, range: receiver.text_range() }
    }

    fn symbol_for_type_ref(&self, token: &syntax::SyntaxToken) -> Option<SemanticSymbol> {
        let definition = self.resolve_name_to_definition(token);
        definition.map(|definition| symbol_from_definition(self.db(), definition, None))
    }

    pub fn resolve_method_call_to_definition(
        &self,
        token: &syntax::SyntaxToken,
    ) -> Option<Definition> {
        if !token.kind().is_name_token() {
            return None;
        }

        let receiver_node = crate::field_name_receiver(token)?;
        let receiver_id = self.type_of_expr(&receiver_node);
        if matches!(self.db().lookup_type(receiver_id), TypeKind::Unknown) {
            return None;
        }

        let method_name = Name::new(token.text());

        // The USER method is asked for first, and the platform surface only after
        // it declines. Inference resolves the same call in that order, and a
        // navigation that reversed it would answer the platform for every user
        // method spelled like one — `Записать` on a catalog object, `НайтиПоКоду`
        // on its manager — leaving the call out of that method's references.
        let resolver = hir_def::resolver::Resolver::with_workspace_scope(self.module_id);
        if let hir_ty::method_resolution::UserCallTarget::Method(hit) =
            hir_ty::method_resolution::resolve_user_call(
                self.db(),
                receiver_id,
                &method_name,
                &resolver,
            )
        {
            // The export boundary is a CROSS-module rule, as the name of
            // `Resolver::resolve_cross_module` says: calling a non-exported method
            // from another module is an error, and naming its declaration here
            // would contradict the answer the qualified route gives for the very
            // same call. Inside its own module a private method is a legitimate
            // call, and inference binds it, so navigation names it too.
            if hit.resolution.is_export || hit.resolution.method_id.module == self.module_id {
                return Some(Definition::Method(hit.resolution.method_id));
            }
            // Found and barred — NOT a reason to consult the platform. Falling
            // through would answer a platform member for a call inference has
            // already bound to this user method, which is the exact disagreement
            // this shared entry exists to make impossible.
            return None;
        }

        let resolution = hir_ty::resolve_method(self.db(), receiver_id, &method_name)?;

        Some(Definition::BuiltinMethodHandle { handle: resolution.handle, method_name })
    }

    pub fn resolve_name_to_definition(&self, token: &syntax::SyntaxToken) -> Option<Definition> {
        let _span = tracing::info_span!("resolve_name_to_definition").entered();

        if token.kind() != SyntaxKind::IDENT && crate::field_name_receiver(token).is_none() {
            return None;
        }

        let token_text = token.text();
        let name = Name::new(token_text);

        if let Some(def) = self.sema.try_resolve_qualified_name_for_token(self.file_id, token) {
            tracing::debug!(?def, "resolved as qualified name");
            return Some(def);
        }

        if crate::field_name_receiver(token).is_some() {
            tracing::debug!("skipping free-name resolution: token is field-name in FIELD_EXPR");
            return None;
        }

        if crate::name_classify::is_bare_call_callee(token) {
            if let Some(def) = self.resolve_bare_call_to_definition(token) {
                tracing::debug!(?def, "resolved as bare call target");
                return Some(def);
            }
        }

        // A global common module export extends the global context and so shadows a
        // same-named platform global. Resolved before builtins to keep Local → Module →
        // Global-CM → Platform consistent with name inference and signature help; the helper
        // gates on nearer scopes (local/parameter, same-module method/variable) missing.
        if let Some(def) = self.global_export_definition(token) {
            tracing::debug!(?def, "resolved as global common module export");
            return Some(def);
        }

        if let Some(def) = self.resolve_local_to_definition(token) {
            tracing::debug!(?def, "resolved as local symbol");
            return Some(def);
        }

        // A call took its method above, so what reaches here is a value read, an assignment
        // target or a method's own declaration name. A value is never a method: a module
        // `Перем` of the name owns every such use but the declaration.
        let module_variable = self.module_variable(&name);
        if let Some(var_id) =
            module_variable.filter(|_| !crate::name_classify::is_method_declaration_name(token))
        {
            tracing::debug!(?var_id, "resolved as module variable");
            return Some(Definition::Variable(var_id));
        }

        if let Some(method_id) = self.module_method(&name) {
            tracing::debug!(?method_id, "resolved as module method");
            return Some(Definition::Method(method_id));
        }

        if let Some(var_id) = module_variable {
            tracing::debug!(?var_id, "resolved as module variable");
            return Some(Definition::Variable(var_id));
        }

        // Platform scopes come last, and for one reason: a module that declares
        // `Сообщить` means its own procedure everywhere in itself. Asking the platform
        // first made such a declaration unreachable from its own uses — the reference
        // walk then answered "no walk exists" about a method it had just found.
        if let Some(def) = self.sema.try_resolve_builtin(token_text) {
            tracing::debug!(?def, "resolved as builtin");
            return Some(def);
        }

        // Same rule, same order: a module method or variable named like a metadata
        // plural holds the name.
        if bsl_metadata::MdoType::is_plural_form(token_text) {
            if let Some(mdo_type) = bsl_metadata::MdoType::from_plural(token_text) {
                tracing::debug!(?mdo_type, "resolved as MDO collection");
                return Some(Definition::MdoCollectionType(mdo_type));
            }
        }

        tracing::debug!("unresolved identifier: {}", token_text);
        None
    }

    /// The method a bare call `Имя(...)` calls: one of this module, else an export of a global
    /// common or application module, else a platform function — the order inference calls
    /// them in. Variables are not asked: a call looks among methods only, so a parameter,
    /// `Перем` or implicit local of the name leaves the call to the method (checked live on
    /// 8.3.17 and 8.3.27). `None` when no method owns the name; the caller then keeps its
    /// value-name reading, and inference reports the call as unresolved.
    fn resolve_bare_call_to_definition(&self, token: &syntax::SyntaxToken) -> Option<Definition> {
        let name = Name::new(token.text());
        if let Some(method_id) = self.module_method(&name) {
            return Some(Definition::Method(method_id));
        }
        if let Some(export @ Definition::Method(_)) =
            self.global_exports().get(&fold_lower_per_char(token.text()))
        {
            return Some(export.clone());
        }
        self.sema.try_resolve_builtin(token.text())
    }

    /// See `Semantics::global_export_definition`'s shadow contract: a nearer
    /// scope (local/parameter, same-module method or variable) wins over the
    /// global common-module export.
    /// Whether this file's own scopes already answer the name — a binding of the body, or
    /// a method or variable of the module. Everything workspace-wide loses to them.
    fn shadowed_by_local_or_module(&self, token: &syntax::SyntaxToken) -> bool {
        let name = Name::new(token.text());
        self.resolve_local_to_definition(token).is_some()
            || self.module_method(&name).is_some()
            || self.module_variable(&name).is_some()
    }

    fn global_export_definition(&self, token: &syntax::SyntaxToken) -> Option<Definition> {
        if self.shadowed_by_local_or_module(token) {
            return None;
        }

        self.global_exports().get(&fold_lower_per_char(token.text())).cloned()
    }

    fn global_exports(&self) -> &FxHashMap<String, Definition> {
        self.global_exports.get_or_init(|| {
            let mut map = FxHashMap::default();
            let resolver = hir_def::resolver::Resolver::with_workspace_scope(self.module_id);
            let common = resolver.global_common_module_exports(self.db());
            let application = resolver.application_module_exports(self.db());
            // First occurrence wins, matching inference precedence: global common
            // modules first, then application-context hosts.
            for entry in common.entries.into_iter().chain(application.entries) {
                let definition = match entry.definition {
                    hir_def::resolver::GlobalExportDefinition::Method(method_id)
                        if entry.capabilities.callable == Some(true) =>
                    {
                        Definition::Method(method_id)
                    }
                    hir_def::resolver::GlobalExportDefinition::Variable(variable_id)
                        if entry.capabilities.readable_as_value == Some(true) =>
                    {
                        Definition::Variable(variable_id)
                    }
                    _ => continue,
                };
                map.entry(fold_lower_per_char(entry.name.as_str())).or_insert(definition);
            }
            map
        })
    }

    fn module_method(&self, name: &Name) -> Option<MethodId> {
        if let Some(hit) = self.module_methods.borrow().get(name.as_str()) {
            return *hit;
        }
        let resolver = hir_def::resolver::Resolver::for_module(self.module_id);
        let result = resolver.resolve_module_method(self.db(), name);
        self.module_methods.borrow_mut().insert(name.as_str().to_string(), result);
        result
    }

    fn module_variable(&self, name: &Name) -> Option<VariableId> {
        if let Some(hit) = self.module_vars.borrow().get(name.as_str()) {
            return *hit;
        }
        let resolver = hir_def::resolver::Resolver::for_module(self.module_id);
        let result = resolver.resolve_module_variable(self.db(), name);
        self.module_vars.borrow_mut().insert(name.as_str().to_string(), result);
        result
    }

    fn resolve_local_to_definition(&self, token: &syntax::SyntaxToken) -> Option<Definition> {
        let (method_node, local_id) = self.enclosing_method(token)?;
        let key = (local_id, token.text().to_string());
        if let Some(hit) = self.local_defs.borrow().get(&key) {
            return hit.clone();
        }
        let result = self.resolve_local_uncached(&method_node, local_id, &Name::new(token.text()));
        self.local_defs.borrow_mut().insert(key, result.clone());
        result
    }

    fn resolve_local_uncached(
        &self,
        method_node: &syntax::SyntaxNode,
        local_id: MethodKey,
        name: &Name,
    ) -> Option<Definition> {
        let scopes = self.scopes_for(method_node, local_id)?;
        let scope_def = scopes.resolve_name(scopes.root_scope(), name)?;

        let tree = self.db().item_tree(self.file_id);
        let params = tree.method(local_id)?.params();
        let method_id = MethodId { module: self.module_id, local_id };
        Some(match scope_def {
            ScopeDef::Parameter => {
                let param_index =
                    params.iter().position(|p| p.name.eq_ignore_case(name)).unwrap_or(0) as u32;
                Definition::Parameter { method_id, param_name: name.clone(), param_index }
            }
            ScopeDef::LocalVariable => Definition::Local { method_id, var_name: name.clone() },
        })
    }

    fn scopes_for(
        &self,
        method_node: &syntax::SyntaxNode,
        local_id: MethodKey,
    ) -> Option<Arc<ExprScopes>> {
        if let Some(scopes) = self.expr_scopes.borrow().get(&local_id) {
            return Some(scopes.clone());
        }
        let scopes = if let Some(proc_def) = ast::ProcedureDef::cast(method_node.clone()) {
            ExprScopes::from_procedure(&proc_def)
        } else {
            let func_def = ast::FunctionDef::cast(method_node.clone())?;
            ExprScopes::from_function(&func_def)
        };
        let scopes = Arc::new(scopes);
        self.expr_scopes.borrow_mut().insert(local_id, scopes.clone());
        Some(scopes)
    }

    /// The nearest enclosing method definition, if the item tree knows it.
    /// Outer definitions are not consulted: a name scoped to an unknown inner
    /// definition must not resolve against an enclosing one.
    fn enclosing_method(
        &self,
        token: &syntax::SyntaxToken,
    ) -> Option<(syntax::SyntaxNode, MethodKey)> {
        let mut node = token.parent()?;
        loop {
            if matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF) {
                let local_id = *self.method_ranges.get(&node.text_range())?;
                return Some((node, local_id));
            }
            node = node.parent()?;
        }
    }

    fn symbol_for_body_local(&self, token: &syntax::SyntaxToken) -> Option<SemanticSymbol> {
        let file_id = self.file_id;
        let (owner, body, source_map) = self.body_for_token(token)?;
        let name = Name::new(token.text());
        let name_lower = name.as_str().fold_lower();

        if let Some(binding_id) =
            self.owner_bindings(owner, body).get(&fold_lower_per_char(token.text())).copied()
        {
            let binding = body.binding(binding_id);
            let range = source_map.binding_range(binding_id)?;
            let is_param = body.params().any(|param_id| param_id == binding_id);
            let kind =
                if is_param { SemanticSymbolKind::Parameter } else { SemanticSymbolKind::Variable };
            let ty = if self.binding_types {
                crate::infer_owner(self.db(), file_id, owner).type_id_of_binding(binding_id)
            } else {
                None
            };
            return Some(SemanticSymbol {
                key: SemanticSymbolKey::BodyLocal { file_id, owner, name_lower },
                name: binding.name.clone(),
                kind,
                definition: None,
                declaration: Some(SymbolDeclaration {
                    file_id,
                    range,
                    name: binding.name.clone(),
                    kind,
                }),
                ty,
            });
        }

        let occurrence_expr = source_map.expr_at_range(token.text_range())?;
        let routed = crate::infer_owner(self.db(), file_id, owner);
        let implicit = routed.implicit_locals().get(&name_lower)?;
        let unknown = self.db().unknown();
        let occurrence_ty = routed.type_id_of_expr(occurrence_expr).unwrap_or(unknown);
        let (range, ty) = select_implicit_local_declaration(
            source_map,
            implicit,
            token.text_range(),
            occurrence_ty,
            unknown,
        )?;
        Some(SemanticSymbol {
            key: SemanticSymbolKey::ImplicitLocal { file_id, owner, name_lower },
            name: implicit.name.clone(),
            kind: SemanticSymbolKind::Variable,
            definition: None,
            declaration: Some(SymbolDeclaration {
                file_id,
                range,
                name: implicit.name.clone(),
                kind: SemanticSymbolKind::Variable,
            }),
            ty: Some(ty),
        })
    }

    fn owner_bindings(
        &self,
        owner: DefWithBodyId,
        body: &hir_def::Body,
    ) -> Arc<FxHashMap<String, BindingId>> {
        if let Some(bindings) = self.owner_bindings.borrow().get(&owner) {
            return bindings.clone();
        }
        let mut map = FxHashMap::default();
        for (binding_id, binding) in body.bindings_iter() {
            map.entry(fold_lower_per_char(binding.name.as_str())).or_insert(binding_id);
        }
        let map = Arc::new(map);
        self.owner_bindings.borrow_mut().insert(owner, map.clone());
        map
    }

    fn type_of_expr(&self, node: &syntax::SyntaxNode) -> TypeId {
        let range = node.text_range();
        if let Some((owner, body, source_map)) = self.body_for_node(node.clone()) {
            if let Some(expr_id) = source_map.expr_at_range(range) {
                let routed = crate::infer_owner(self.db(), self.file_id, owner);
                let base_id =
                    routed.type_id_of_expr(expr_id).unwrap_or_else(|| self.db().unknown());
                return self.narrow_or_base_cached(owner, body, expr_id, base_id);
            }
        }
        self.db().unknown()
    }

    /// `narrow_or_base` through the per-owner cache: one dataflow solve and
    /// one expression index per body, however many tokens resolve against it.
    fn narrow_or_base_cached(
        &self,
        owner: DefWithBodyId,
        body: &hir_def::Body,
        expr_id: ExprId,
        base: TypeId,
    ) -> TypeId {
        if !self.db().type_narrowing_enabled() {
            return base;
        }
        if !matches!(body.expr(expr_id), hir_def::hir::Expr::Path(_)) {
            return base;
        }
        let Some((result, index)) = self.narrow_for(owner, body) else {
            return base;
        };
        crate::narrow_or_base_indexed(self.db(), body, &result, &index, expr_id, base)
    }

    fn narrow_for(&self, owner: DefWithBodyId, body: &hir_def::Body) -> NarrowEntry {
        if let Some(hit) = self.narrow_cache.borrow().get(&owner) {
            return hit.clone();
        }
        let entry = self.db().narrow(self.file_id, owner).map(|result| {
            let index = Arc::new(NarrowExprIndex::build(body, result.cfg()));
            (result, index)
        });
        self.narrow_cache.borrow_mut().insert(owner, entry.clone());
        entry
    }

    fn body_for_token(
        &self,
        token: &syntax::SyntaxToken,
    ) -> Option<(DefWithBodyId, &hir_def::Body, hir_def::body::SourceMapAt<'_>)> {
        self.body_for(token.text_range(), token.parent()?)
    }

    fn body_for_node(
        &self,
        node: syntax::SyntaxNode,
    ) -> Option<(DefWithBodyId, &hir_def::Body, hir_def::body::SourceMapAt<'_>)> {
        self.body_for(node.text_range(), node)
    }

    /// The lowered body a source range participates in.
    ///
    /// Module code is checked first, matching the legacy all-bodies scan
    /// order; the remaining candidates are the syntactically enclosing method
    /// definitions (method ranges are disjoint, so no other body can contain
    /// the range). A range that participates in no body — a callee name, a
    /// token in a dead preprocessor branch — resolves to `None`, as it did
    /// when every body was scanned.
    ///
    /// The module code (one memo per file) is read first for every range;
    /// beyond it, only the method the range falls into is lowered, straight
    /// from the per-method memos. The whole-file fold (`module_bodies`) is not
    /// read here, or resolving one token would lower every method of the file.
    fn body_for(
        &self,
        range: TextRange,
        start: syntax::SyntaxNode,
    ) -> Option<(DefWithBodyId, &hir_def::Body, hir_def::body::SourceMapAt<'_>)> {
        let db: &'db dyn DefDatabase = self.db();
        let module_code =
            hir_def::module_code_lower_query(db, base_db::FileIdInput::new(db, self.file_id));
        let result = LowerResultAt { result: module_code, base: MethodOffset::ZERO };
        let source_map = result.source_map();
        if source_map.expr_at_range(range).is_some() || source_map.binding_at_range(range).is_some()
        {
            return Some((DefWithBodyId::ModuleCode, result.body(), source_map));
        }

        let mut node = Some(start);
        while let Some(current) = node {
            if matches!(current.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF) {
                if let Some(local_id) = self.method_ranges.get(&current.text_range()) {
                    let method = MethodId { module: self.module_id, local_id: *local_id };
                    if let Some(lower) =
                        hir_def::method_lower_query(db, MethodIdInput::new(db, method))
                    {
                        let base = MethodOffset::new(current.text_range().start());
                        let result = LowerResultAt { result: lower, base };
                        let source_map = result.source_map();
                        if source_map.expr_at_range(range).is_some()
                            || source_map.binding_at_range(range).is_some()
                        {
                            return Some((
                                DefWithBodyId::Method(*local_id),
                                result.body(),
                                source_map,
                            ));
                        }
                    }
                }
            }
            node = current.parent();
        }

        None
    }
}

/// Where an occurrence reads its declaration and its type from: the nearest preceding
/// assignment, preferring one whose type matches the occurrence's own.
///
/// This is a projection of the occurrence, not the variable's identity — navigation from a
/// later read lands on the write that produced what it reads, while the variable those
/// occurrences belong to stays one.
fn select_implicit_local_declaration(
    source_map: hir_def::body::SourceMapAt<'_>,
    implicit: &ImplicitLocalInfo,
    occurrence_range: TextRange,
    occurrence_ty: TypeId,
    unknown: TypeId,
) -> Option<(TextRange, TypeId)> {
    if occurrence_ty != unknown {
        let typed_preceding = implicit
            .assignments
            .iter()
            .filter_map(|assignment| {
                if assignment.ty != occurrence_ty {
                    return None;
                }
                let range = source_map.expr_range(assignment.target)?;
                (range.start() <= occurrence_range.start()).then_some((assignment, range))
            })
            .next_back();

        if let Some((assignment, range)) = typed_preceding {
            return Some((range, assignment.ty));
        }
    }

    let preceding = implicit
        .assignments
        .iter()
        .filter_map(|assignment| {
            let range = source_map.expr_range(assignment.target)?;
            (range.start() <= occurrence_range.start()).then_some((assignment, range))
        })
        .next_back();

    if let Some((assignment, range)) = preceding {
        return Some((range, assignment.ty));
    }

    let range = source_map.expr_range(implicit.first_assignment)?;
    Some((range, implicit.ty))
}

fn symbol_from_definition(
    db: &dyn hir_def::DefDatabase,
    definition: Definition,
    ty: Option<TypeId>,
) -> SemanticSymbol {
    let name = definition.name(db).unwrap_or_else(Name::missing);
    let kind = kind_for_definition(&definition);
    let declaration = declaration_for_definition(db, &definition, kind);
    SemanticSymbol {
        key: SemanticSymbolKey::Definition(definition.clone()),
        name,
        kind,
        definition: Some(definition),
        declaration,
        ty,
    }
}

fn kind_for_definition(definition: &Definition) -> SemanticSymbolKind {
    match definition {
        Definition::Method(_) | Definition::BuiltinFunction(_) => SemanticSymbolKind::Function,
        Definition::BuiltinMethodHandle { .. } => SemanticSymbolKind::Method,
        Definition::Variable(_) | Definition::Local { .. } => SemanticSymbolKind::Variable,
        Definition::Parameter { .. } => SemanticSymbolKind::Parameter,
        Definition::VirtualTableField { .. } => SemanticSymbolKind::Property,
        Definition::MdoCollectionType(_) => SemanticSymbolKind::Class,
        Definition::MdoObject { .. } => SemanticSymbolKind::Type,
        Definition::MdoManagerModule { .. } | Definition::Module(_) => {
            SemanticSymbolKind::Namespace
        }
        Definition::Unresolved => SemanticSymbolKind::Variable,
    }
}

fn declaration_for_definition(
    db: &dyn hir_def::DefDatabase,
    definition: &Definition,
    kind: SemanticSymbolKind,
) -> Option<SymbolDeclaration> {
    match definition {
        Definition::Method(method_id) => declaration_for_method(db, *method_id, kind),
        Definition::Variable(var_id) => Some(SymbolDeclaration {
            file_id: var_id.module.file_id,
            range: definition.source_range(db)?,
            name: definition.name(db)?,
            kind,
        }),
        _ => None,
    }
}

fn declaration_for_method(
    db: &dyn hir_def::DefDatabase,
    method_id: MethodId,
    kind: SemanticSymbolKind,
) -> Option<SymbolDeclaration> {
    Some(SymbolDeclaration {
        file_id: method_id.module.file_id,
        range: definition_source_range(db, method_id)?,
        name: Definition::Method(method_id).name(db)?,
        kind,
    })
}

fn definition_source_range(
    db: &dyn hir_def::DefDatabase,
    method_id: MethodId,
) -> Option<TextRange> {
    Definition::Method(method_id).source_range(db)
}

#[cfg(test)]
mod tests {
    use super::receiver_name_token;
    use syntax::SyntaxKind;

    fn receivers_of(code: &str) -> Vec<(SyntaxKind, Option<String>)> {
        let parse = parser::parse(code);
        parse
            .syntax_node()
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::FIELD_EXPR)
            .filter_map(|field| {
                let receiver = field.children().next()?;
                let named = receiver_name_token(&receiver).map(|token| token.text().to_string());
                Some((receiver.kind(), named))
            })
            .collect()
    }

    /// Only a bare identifier and a chain are name-shaped; the shapes that name nothing are
    /// pinned so their member keys to its own spelling.
    #[test]
    fn only_name_shaped_receivers_have_a_name_token() {
        let seen = receivers_of(
            "А = С.Поле;\nБ = С.Внутр.Поле;\nВ = Получить().Поле;\nГ = (С).Поле;\n\
             Д = Новый Структура(\"Поле\", 1).Поле;\n",
        );

        assert!(
            seen.contains(&(SyntaxKind::IDENT, Some("С".to_string()))),
            "a bare identifier must be named: {seen:?}"
        );
        assert!(
            seen.contains(&(SyntaxKind::FIELD_EXPR, Some("Внутр".to_string()))),
            "a chain must be named by its tail: {seen:?}"
        );
        for kind in [SyntaxKind::CALL_EXPR, SyntaxKind::PAREN_EXPR, SyntaxKind::NEW_EXPR] {
            assert!(
                seen.contains(&(kind, None)),
                "a `{kind:?}` receiver must stay unnamed: {seen:?}"
            );
        }
    }
}
