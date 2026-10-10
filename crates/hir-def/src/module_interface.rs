//! What a module declares, with no positions in it.
//!
//! Inference of one method reads the declarations of its own module and of the
//! modules it calls: names, kinds, export flags, parameters, directives, docs.
//! None of that moves when text is inserted elsewhere in the file — but the
//! ranges next to it in [`SymbolTree`](crate::SymbolTree) do, and a value that
//! changes on every edit invalidates every method that read it. The interface
//! is the position-free half of the symbol tree, so a per-method query can
//! depend on it and still be validated after an unrelated edit.
//!
//! Positions stay in [`ItemTree`] and in the symbol tree assembled from both.

use std::sync::Arc;

use la_arena::{Arena, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{Parse, SyntaxNode};
use text_size::TextRange;

use crate::conditional_tree::ConditionalTree;
use crate::docs::{compute_variable_docs_with_node, MethodDocs, VariableDocs};
use crate::execution_env::{conditional_env, EnvFlags};
use crate::item_tree::{AnnotationKind, ItemTree, ModItem, Param};
use crate::ty::doc_types::{parse_method_doc_types, MethodTypeHints};
use crate::type_ref::TypeRef;
use crate::{MethodId, ModuleId, Name, VariableId};
use intern::NormName;

/// Declarations are shared: the per-declaration projections below hand out
/// the same allocation the interface holds, so a memo per method costs a
/// pointer, not a copy.
type MethodSlot = Idx<Arc<MethodDecl>>;
type VariableSlot = Idx<Arc<VariableDecl>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInterface {
    module_id: ModuleId,
    methods: Arena<Arc<MethodDecl>>,
    variables: Arena<Arc<VariableDecl>>,
    /// Declarations of each folded name in source order, so a method's key
    /// — name and ordinal among namesakes — is a lookup and an index.
    methods_by_name: FxHashMap<NormName, Vec<MethodSlot>>,
    variables_by_name: FxHashMap<NormName, Vec<VariableSlot>>,
    /// Arena slot of each top-level item, indexed by the variable's positional
    /// `local_id`; `None` for a method's slot.
    variable_slots: Vec<Option<VariableSlot>>,
}

/// A method as its callers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDecl {
    pub id: MethodId,
    pub name: Name,
    pub is_function: bool,
    pub is_export: bool,
    pub params: Vec<ParamSymbol>,
    pub directives: Box<[AnnotationKind]>,
    /// Environments admitted by the module-level `#Если` regions around the
    /// method; [`EnvFlags::ALL`] outside any region.
    pub preproc_env: EnvFlags,
    pub docs: Option<Arc<MethodDocs>>,
    pub return_type_ref: Option<TypeRef>,
}

/// A module variable as its readers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDecl {
    pub id: VariableId,
    pub name: Name,
    pub is_export: bool,
    pub directives: Box<[AnnotationKind]>,
    pub docs: Option<Arc<VariableDocs>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSymbol {
    pub name: Name,

    pub is_val: bool,

    pub has_default: bool,

    pub type_ref: Option<TypeRef>,
}

impl From<&Param> for ParamSymbol {
    fn from(param: &Param) -> Self {
        ParamSymbol {
            name: param.name.clone(),
            is_val: param.is_val,
            has_default: param.has_default,
            type_ref: None,
        }
    }
}

impl ModuleInterface {
    /// Declarations of a parsed module. `conditionals` narrows each method's
    /// `preproc_env`; pass `None` when the file has no module-level `#Если`.
    pub fn from_item_tree(
        item_tree: &ItemTree,
        module_id: ModuleId,
        parse: &Parse<SyntaxNode>,
        source_text: &str,
        conditionals: Option<&ConditionalTree>,
    ) -> Self {
        let var_def_nodes: FxHashMap<TextRange, SyntaxNode> = parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == syntax::SyntaxKind::VAR_DEF)
            .map(|n| (n.text_range(), n))
            .collect();
        let comment_runs = syntax::comment_runs(&parse.syntax_node());

        let mut interface = ModuleInterface::empty(module_id);
        let items = item_tree.top_level_items();
        interface.variable_slots = vec![None; items.len()];

        for (idx, item) in items.iter().enumerate() {
            let local_id = idx as u32;
            match item {
                ModItem::Procedure(_) | ModItem::Function(_) => {
                    let method = item_tree.method_item(item).expect("a method item");
                    let method_id = MethodId { module: module_id, local_id: method.key() };
                    let docs = crate::docs::compute_method_docs(
                        &comment_runs,
                        item_tree,
                        method_id,
                        source_text,
                    );
                    let decl = Self::method_decl(
                        method_id,
                        method.name(),
                        method.is_function(),
                        method.is_export(),
                        method.params(),
                        method.annotations().iter().map(|a| a.kind).collect(),
                        preproc_env(conditionals, method.source_range()),
                        docs,
                    );
                    interface.push_method(decl);
                }
                ModItem::Variable(var_idx) => {
                    let var = item_tree.variable(*var_idx);
                    let variable_id = VariableId { module: module_id, local_id };
                    let docs = match var_def_nodes.get(&var.source_range) {
                        Some(node) => {
                            compute_variable_docs_with_node(node, var, source_text, &comment_runs)
                        }
                        None => crate::docs::compute_variable_docs(
                            parse,
                            item_tree,
                            variable_id,
                            source_text,
                            &comment_runs,
                        ),
                    };
                    let decl = VariableDecl {
                        id: variable_id,
                        name: var.name.clone(),
                        is_export: var.is_export,
                        directives: var.annotations.iter().map(|a| a.kind).collect(),
                        docs,
                    };
                    interface.push_variable(local_id, decl);
                }
            }
        }

        interface
    }

    #[allow(clippy::too_many_arguments, reason = "one call per item kind, all fields named")]
    fn method_decl(
        id: MethodId,
        name: &Name,
        is_function: bool,
        is_export: bool,
        params: &[Param],
        directives: Box<[AnnotationKind]>,
        preproc_env: EnvFlags,
        docs: Option<Arc<MethodDocs>>,
    ) -> MethodDecl {
        let hints = docs.as_deref().and_then(|d| parse_method_doc_types(&d.raw));
        MethodDecl {
            id,
            name: name.clone(),
            is_function,
            is_export,
            params: params_with_hints(params, hints.as_ref()),
            directives,
            preproc_env,
            return_type_ref: hints.as_ref().map(|h| h.ret.clone()),
            docs,
        }
    }

    /// Declarations arrive in source order, so the position under the name
    /// is the key's ordinal — the same count the item tree assigned.
    fn push_method(&mut self, decl: MethodDecl) {
        let key = decl.id.local_id;
        let idx = self.methods.alloc(Arc::new(decl));
        let namesakes = self.methods_by_name.entry(key.name).or_default();
        debug_assert_eq!(namesakes.len() as u32, key.ordinal, "declarations arrive in key order");
        namesakes.push(idx);
    }

    /// Only the first declaration of a name is reachable by name: a duplicate
    /// module variable is a diagnostic, not a second symbol.
    fn push_variable(&mut self, local_id: u32, decl: VariableDecl) {
        let key = NormName::intern(decl.name.as_str());
        let idx = self.variables.alloc(Arc::new(decl));
        let entry = self.variables_by_name.entry(key).or_default();
        if entry.is_empty() {
            entry.push(idx);
        }
        self.variable_slots[local_id as usize] = Some(idx);
    }

    pub fn find_method(&self, name: &Name) -> Option<&MethodDecl> {
        self.find_method_shared(NormName::intern(name.as_str())).map(|m| &**m)
    }

    /// The first declaration of a folded name, as the interface holds it.
    pub fn find_method_shared(&self, name: NormName) -> Option<&Arc<MethodDecl>> {
        let indices = self.methods_by_name.get(&name)?;
        indices.first().map(|&idx| &self.methods[idx])
    }

    pub fn find_methods(&self, name: &Name) -> Vec<&MethodDecl> {
        self.methods_by_name
            .get(&NormName::intern(name.as_str()))
            .map(|indices| indices.iter().map(|&idx| &*self.methods[idx]).collect())
            .unwrap_or_default()
    }

    pub fn find_variable(&self, name: &Name) -> Option<&VariableDecl> {
        self.find_variable_shared(NormName::intern(name.as_str())).map(|v| &**v)
    }

    /// The first declaration of a folded variable name, as the interface holds it.
    pub fn find_variable_shared(&self, name: NormName) -> Option<&Arc<VariableDecl>> {
        let indices = self.variables_by_name.get(&name)?;
        indices.first().map(|&idx| &self.variables[idx])
    }

    pub fn methods(&self) -> impl Iterator<Item = &MethodDecl> {
        self.methods.iter().map(|(_, m)| &**m)
    }

    pub fn exported_methods(&self) -> impl Iterator<Item = &MethodDecl> {
        self.methods().filter(|m| m.is_export)
    }

    pub fn variables(&self) -> impl Iterator<Item = &VariableDecl> {
        self.variables.iter().map(|(_, v)| &**v)
    }

    pub fn exported_variables(&self) -> impl Iterator<Item = &VariableDecl> {
        self.variables().filter(|v| v.is_export)
    }

    pub fn find_method_by_id(&self, method_id: MethodId) -> Option<&MethodDecl> {
        self.find_method_by_id_shared(method_id).map(|m| &**m)
    }

    /// The declaration under a key, as the interface holds it.
    pub fn find_method_by_id_shared(&self, method_id: MethodId) -> Option<&Arc<MethodDecl>> {
        let key = method_id.local_id;
        let idx = *self.methods_by_name.get(&key.name)?.get(key.ordinal as usize)?;
        let decl = &self.methods[idx];
        (decl.id == method_id).then_some(decl)
    }

    pub fn find_variable_by_id(&self, variable_id: VariableId) -> Option<&VariableDecl> {
        let idx = (*self.variable_slots.get(variable_id.local_id as usize)?)?;
        let decl = &self.variables[idx];
        (decl.id == variable_id).then_some(decl)
    }

    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    pub fn module_id(&self) -> ModuleId {
        self.module_id
    }

    /// Declarations without documentation, for tests that build an item tree
    /// by hand and have no text to read comments from.
    #[cfg(test)]
    pub(crate) fn from_item_tree_no_docs(item_tree: &ItemTree, module_id: ModuleId) -> Self {
        let mut interface = ModuleInterface::empty(module_id);
        let items = item_tree.top_level_items();
        interface.variable_slots = vec![None; items.len()];
        for (idx, item) in items.iter().enumerate() {
            let local_id = idx as u32;
            match item {
                ModItem::Procedure(_) | ModItem::Function(_) => {
                    let method = item_tree.method_item(item).expect("a method item");
                    let decl = Self::method_decl(
                        MethodId { module: module_id, local_id: method.key() },
                        method.name(),
                        method.is_function(),
                        method.is_export(),
                        method.params(),
                        method.annotations().iter().map(|a| a.kind).collect(),
                        EnvFlags::ALL,
                        None,
                    );
                    interface.push_method(decl);
                }
                ModItem::Variable(var_idx) => {
                    let var = item_tree.variable(*var_idx);
                    let decl = VariableDecl {
                        id: VariableId { module: module_id, local_id },
                        name: var.name.clone(),
                        is_export: var.is_export,
                        directives: var.annotations.iter().map(|a| a.kind).collect(),
                        docs: None,
                    };
                    interface.push_variable(local_id, decl);
                }
            }
        }
        interface
    }
}

impl ModuleInterface {
    pub fn empty(module_id: ModuleId) -> Self {
        Self {
            module_id,
            methods: Arena::new(),
            variables: Arena::new(),
            methods_by_name: FxHashMap::default(),
            variables_by_name: FxHashMap::default(),
            variable_slots: Vec::new(),
        }
    }
}

fn preproc_env(conditionals: Option<&ConditionalTree>, range: TextRange) -> EnvFlags {
    match conditionals {
        Some(tree) if !tree.is_empty() => conditional_env(tree, range),
        _ => EnvFlags::ALL,
    }
}

pub(crate) fn params_with_hints(
    params: &[Param],
    hints: Option<&MethodTypeHints>,
) -> Vec<ParamSymbol> {
    params
        .iter()
        .map(|p| {
            let mut sym = ParamSymbol::from(p);
            if let Some(hints) = hints {
                sym.type_ref = hints
                    .params
                    .iter()
                    .find(|(n, _)| p.name.eq_ignore_case(n))
                    .map(|(_, t)| t.clone());
            }
            sym
        })
        .collect()
}

/// Rough live bytes for Salsa's `memory_usage` report: both arenas with each
/// declaration's name, params and directives, plus the name indexes and the
/// two slot tables. Shared `docs` are counted as their inline `Arc` only.
pub(crate) fn module_interface_heap(v: &Arc<ModuleInterface>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes, vec_bytes};

    let i = &**v;
    let mut bytes = std::mem::size_of::<ModuleInterface>();
    bytes += vec_bytes::<Arc<MethodDecl>>(i.methods.len());
    for method in i.methods.values() {
        bytes += std::mem::size_of::<MethodDecl>();
        bytes += name_bytes(&method.name);
        bytes += vec_bytes::<ParamSymbol>(method.params.len());
        for param in &method.params {
            bytes += name_bytes(&param.name);
        }
        bytes += vec_bytes::<AnnotationKind>(method.directives.len());
    }
    bytes += vec_bytes::<Arc<VariableDecl>>(i.variables.len());
    for variable in i.variables.values() {
        bytes += std::mem::size_of::<VariableDecl>();
        bytes += name_bytes(&variable.name);
        bytes += vec_bytes::<AnnotationKind>(variable.directives.len());
    }
    bytes += map_table_bytes::<NormName, Vec<MethodSlot>>(i.methods_by_name.len());
    for idxs in i.methods_by_name.values() {
        bytes += vec_bytes::<MethodSlot>(idxs.len());
    }
    bytes += map_table_bytes::<NormName, Vec<VariableSlot>>(i.variables_by_name.len());
    for idxs in i.variables_by_name.values() {
        bytes += vec_bytes::<VariableSlot>(idxs.len());
    }
    bytes += vec_bytes::<Option<VariableSlot>>(i.variable_slots.len());
    bytes
}

// One declaration per memo, shared with the interface that owns it, so a
// method-keyed reader depends on the declaration it reads and not on the
// file. The memo keeps the declaration — parameters, docs and all — alive
// after the interface it came from is evicted, so it needs a cap of its own:
// without one a batch sweep pins every declaration of the workspace. The cap
// is what the interface's cap used to pin (2048 modules of a few dozen
// methods), and the eviction list's mutex is touched a few times per body,
// not once per name — the by-name misses below never reach it.

/// A declaration by key — the memo a method-keyed query reads for its own
/// declaration and, once resolved, for a callee's.
#[salsa::tracked(lru = 65536, heap_size = method_decl_heap, returns(clone))]
pub fn interface_method_query<'db>(
    db: &'db dyn crate::DefDatabase,
    method: crate::MethodIdInput<'db>,
) -> Option<Arc<MethodDecl>> {
    let method_id = method.method_id(db);
    db.module_interface_ref(method_id.module).find_method_by_id_shared(method_id).cloned()
}

/// Bytes of one projected declaration: what the memo pins once its interface
/// is gone. Counted here as well as in the interface while both are alive —
/// an overcount by the memo's share beats an untracked pin.
fn method_decl_heap(v: &Option<Arc<MethodDecl>>) -> usize {
    use crate::heap_estimate::{name_bytes, vec_bytes};
    let Some(method) = v else { return 0 };
    let mut bytes = std::mem::size_of::<MethodDecl>() + name_bytes(&method.name);
    bytes += vec_bytes::<ParamSymbol>(method.params.len());
    for param in &method.params {
        bytes += name_bytes(&param.name);
    }
    bytes + vec_bytes::<AnnotationKind>(method.directives.len())
}

/// The first module variable of a name.
#[salsa::tracked(heap_size = stdx::heap::zero, returns(clone))]
pub fn interface_variable_named_query<'db>(
    db: &'db dyn crate::DefDatabase,
    key: crate::ModuleNameInput<'db>,
) -> Option<Arc<VariableDecl>> {
    db.module_interface_ref(ModuleId::new(key.file_id(db)))
        .find_variable_shared(key.name(db))
        .cloned()
}

// A miss is the common case of a by-name lookup: every global function is
// asked of the module first, and so is every assigned local and parameter,
// whether it is a module method or variable. A memo per missed name
// outweighed the module's own memos (1.9 million method-name and 1.4 million
// variable-name keys for 579 380 methods), so a miss reads a set of the
// module's names and costs no memo of its own — the whole set for a module
// small enough that re-inferring it after a method is added or removed costs
// what a body edit does today, one memo per name above that, where names are
// many and a whole-file re-inference is what this projection exists to avoid
// (one workspace in a hundred modules is that big, and they hold an eighth of
// the names looked up). A hit needs no memo
// by name either: the first declaration of a name is the key `{name, 0}`,
// and the memo by key is the one the method's own readers already hold.

/// The most methods a module may have for its by-name misses to read the
/// whole set of its method names instead of a memo per name.
pub const NAME_SET_METHOD_LIMIT: usize = 256;

/// Whether the module's by-name misses read the whole set of its method
/// names; moves only when the module crosses [`NAME_SET_METHOD_LIMIT`].
#[salsa::tracked(heap_size = stdx::heap::zero, returns(copy))]
pub fn module_misses_read_name_set_query<'db>(
    db: &'db dyn crate::DefDatabase,
    file_id_input: base_db::FileIdInput<'db>,
) -> bool {
    let module_id = ModuleId::new(file_id_input.file_id(db));
    db.module_interface_ref(module_id).methods.len() <= NAME_SET_METHOD_LIMIT
}

/// The folded names of a module's methods.
#[salsa::tracked(heap_size = name_set_heap, returns(clone))]
pub fn module_method_names_query<'db>(
    db: &'db dyn crate::DefDatabase,
    file_id_input: base_db::FileIdInput<'db>,
) -> Arc<FxHashSet<NormName>> {
    let module_id = ModuleId::new(file_id_input.file_id(db));
    Arc::new(db.module_interface_ref(module_id).methods_by_name.keys().copied().collect())
}

/// Whether a module declares a method of a name: the miss memo of a module
/// too big for its readers to share one set of names.
#[salsa::tracked(heap_size = stdx::heap::zero, returns(copy))]
pub fn module_declares_method_query<'db>(
    db: &'db dyn crate::DefDatabase,
    key: crate::ModuleNameInput<'db>,
) -> bool {
    db.module_interface_ref(ModuleId::new(key.file_id(db)))
        .methods_by_name
        .contains_key(&key.name(db))
}

/// The folded names of a module's variables.
#[salsa::tracked(heap_size = name_set_heap, returns(clone))]
pub fn module_variable_names_query<'db>(
    db: &'db dyn crate::DefDatabase,
    file_id_input: base_db::FileIdInput<'db>,
) -> Arc<FxHashSet<NormName>> {
    let module_id = ModuleId::new(file_id_input.file_id(db));
    Arc::new(db.module_interface_ref(module_id).variables_by_name.keys().copied().collect())
}

fn name_set_heap(v: &Arc<FxHashSet<NormName>>) -> usize {
    crate::heap_estimate::map_table_bytes::<NormName, ()>(v.len())
}

/// The first method named `name` in a module: a miss depends on the module's
/// names, a hit on the declaration under the key `{name, 0}`.
pub fn interface_method_named(
    db: &dyn crate::DefDatabase,
    module_id: ModuleId,
    name: NormName,
) -> Option<Arc<MethodDecl>> {
    let file_id_input = base_db::FileIdInput::new(db, module_id.file_id);
    let declared = if module_misses_read_name_set_query(db, file_id_input) {
        module_method_names_query(db, file_id_input).contains(&name)
    } else {
        module_declares_method_query(db, crate::ModuleNameInput::new(db, module_id.file_id, name))
    };
    if !declared {
        return None;
    }
    let first = MethodId { module: module_id, local_id: crate::MethodKey { name, ordinal: 0 } };
    interface_method_query(db, crate::MethodIdInput::new(db, first))
}

/// The first module variable named `name`: a miss depends on the module's
/// variable names, a hit on the declaration.
pub fn interface_variable_named(
    db: &dyn crate::DefDatabase,
    module_id: ModuleId,
    name: NormName,
) -> Option<Arc<VariableDecl>> {
    let file_id_input = base_db::FileIdInput::new(db, module_id.file_id);
    if !module_variable_names_query(db, file_id_input).contains(&name) {
        return None;
    }
    interface_variable_named_query(db, crate::ModuleNameInput::new(db, module_id.file_id, name))
}

// Position-free declarations on the per-method inference path. The cap holds a
// whole workspace for the same reason as `item_tree`: a request resolving a call into
// this module must not re-derive it from a re-parse after a trim.
#[salsa::tracked(lru = 65536, heap_size = module_interface_heap, returns(ref))]
pub fn module_interface_query<'db>(
    db: &'db dyn crate::DefDatabase,
    file_id_input: base_db::FileIdInput<'db>,
) -> Arc<ModuleInterface> {
    let _span = tracing::info_span!("module_interface", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let item_tree = db.item_tree_ref(file_id);
    let parse = db.parse_ref(file_id);
    let source_text = db.file_text_ref(file_id);
    // The conditional tree is a dependency only for files that have module-level
    // regions; every other file's interface stays valid across `#Если` edits.
    let conditionals = item_tree.has_module_preproc().then(|| db.conditional_tree_ref(file_id));
    let module_id = ModuleId::new(file_id);
    Arc::new(ModuleInterface::from_item_tree(
        item_tree,
        module_id,
        parse,
        source_text,
        conditionals.map(|c| &**c),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MethodKey;
    use expect_test::expect;
    use vfs::FileId;

    fn interface(code: &str) -> ModuleInterface {
        let parse = parser::parse_with_shared_cache(code);
        let item_tree = ItemTree::from_parse(&parse);
        let conditionals = crate::conditional_tree::lower_conditionals(&parse.syntax_node());
        ModuleInterface::from_item_tree(
            &item_tree,
            ModuleId::new(FileId(0)),
            &parse,
            code,
            Some(&conditionals),
        )
    }

    #[test]
    fn comment_run_documentation_matches_the_module_interface_contract() {
        let i = interface(include_str!("fixtures/issue53_comment_runs.bsl"));

        expect![[r#"
            Some(
                MethodDocs {
                    raw: "\nВычисляет длину значения.\n\nПараметры:\n  Значение - Строка - исходное значение.\n\nВозвращаемое значение:\n  Число - длина значения.\n\nПример:\n  Посчитать(\"текст\")\n",
                    purpose: Some(
                        "Вычисляет длину значения.",
                    ),
                    parameters: [
                        ParameterDoc {
                            name: "Значение",
                            types: [
                                TypeDoc {
                                    name: "Строка",
                                    description: Some(
                                        "исходное значение.",
                                    ),
                                    parameters: [],
                                    is_hyperlink: false,
                                },
                            ],
                        },
                    ],
                    returned_value: [
                        TypeDoc {
                            name: "Число",
                            description: Some(
                                "длина значения.",
                            ),
                            parameters: [],
                            is_hyperlink: false,
                        },
                    ],
                    examples: [
                        "Посчитать(\"текст\")",
                    ],
                    call_options: [],
                    deprecation: None,
                    link: None,
                },
            )
        "#]]
        .assert_debug_eq(
            &i.find_method(&Name::new("Посчитать")).expect("documented method").docs.as_deref(),
        );
        expect![[r#"
            None
        "#]]
        .assert_debug_eq(
            &i.find_method(&Name::new("БезДокументации"))
                .expect("undocumented method")
                .docs
                .as_deref(),
        );
        expect![[r#"
            Some(
                VariableDocs {
                    raw: [
                        "Назначение переменной.",
                        "Дополнение назначения.",
                        "Заключительная строка.",
                    ],
                    purpose: Some(
                        "Назначение переменной.\nДополнение назначения.\nЗаключительная строка.",
                    ),
                    types: [],
                    deprecation: None,
                    link: None,
                },
            )
        "#]]
        .assert_debug_eq(
            &i.find_variable(&Name::new("Данные")).expect("documented variable").docs.as_deref(),
        );
        assert_eq!(
            &*i.find_variable(&Name::new("Данные")).expect("documented variable").directives,
            &[AnnotationKind::AtClient, AnnotationKind::AtServer]
        );
        expect![[r#"
            None
        "#]]
        .assert_debug_eq(
            &i.find_variable(&Name::new("БезОписания"))
                .expect("undocumented variable")
                .docs
                .as_deref(),
        );
        expect![[r#"
            Some(
                MethodDocs {
                    raw: "Поздняя документация.",
                    purpose: Some(
                        "Поздняя документация.",
                    ),
                    parameters: [],
                    returned_value: [],
                    examples: [],
                    call_options: [],
                    deprecation: None,
                    link: None,
                },
            )
        "#]]
        .assert_debug_eq(
            &i.find_method(&Name::new("Поздняя")).expect("later documented method").docs.as_deref(),
        );
    }

    #[test]
    fn module_interface_preserves_comment_boundaries_at_item_tree_offsets() {
        for (label, code, expected) in [
            (
                "bom",
                "\u{feff}// first\n// near\nПроцедура П()\nКонецПроцедуры",
                Some("first\nnear"),
            ),
            ("unicode indentation", "\u{2003}// near\nПроцедура П()\nКонецПроцедуры", Some("near")),
            ("crlf", "// near\r\nПроцедура П()\r\nКонецПроцедуры", Some("near")),
            ("tail barrier", "// far\nX = 1; // tail\nПроцедура П()\nКонецПроцедуры", None),
        ] {
            let i = interface(code);
            let raw = i
                .find_method(&Name::new("П"))
                .expect("method")
                .docs
                .as_deref()
                .map(|docs| docs.raw.as_str());
            assert_eq!(raw, expected, "{label}");
        }
    }

    #[test]
    fn lookup_by_id_and_by_name_agree_and_skip_variables() {
        let i = interface(
            "Перем А Экспорт;\nПерем Б;\n\n&НаСервере\nПроцедура П(Х, Знач У = 1) Экспорт\nКонецПроцедуры\n\nФункция Ф()\nКонецФункции\n",
        );
        let module = ModuleId::new(FileId(0));
        let p = i.find_method(&Name::new("п")).expect("procedure by folded name");
        assert_eq!(p.id, MethodId { module, local_id: MethodKey::first("П") });
        assert!(p.is_export && !p.is_function);
        assert_eq!(p.params.len(), 2);
        assert!(p.params[1].is_val && p.params[1].has_default);
        assert_eq!(&*p.directives, &[AnnotationKind::AtServer]);
        assert_eq!(p.preproc_env, EnvFlags::ALL);
        assert!(std::ptr::eq(i.find_method_by_id(p.id).unwrap(), p));
        assert!(i
            .find_method_by_id(MethodId { module, local_id: MethodKey::first("А") })
            .is_none());
        assert!(i
            .find_method_by_id(MethodId { module, local_id: MethodKey::nth("П", 1) })
            .is_none());

        let f = i
            .find_method_by_id(MethodId { module, local_id: MethodKey::first("ф") })
            .expect("function");
        assert!(f.is_function && !f.is_export);
        assert_eq!(i.exported_methods().count(), 1);

        let a = i.find_variable(&Name::new("А")).expect("variable");
        assert_eq!(a.id, VariableId { module, local_id: 0 });
        assert!(a.is_export);
        assert!(i.find_variable_by_id(VariableId { module, local_id: 1 }).is_some());
        assert!(i.find_variable_by_id(VariableId { module, local_id: 2 }).is_none());
        assert_eq!(i.exported_variables().count(), 1);
    }

    /// Variables are keyed by the same per-character fold as methods, so the
    /// two spellings of a Greek final sigma name one variable — the contextual
    /// fold would tell them apart.
    #[test]
    fn variable_lookup_folds_names_like_the_method_lookup() {
        let i = interface("Перем ΟΔΟΣ;\nПерем οδοσ Экспорт;\n");
        let module = ModuleId::new(FileId(0));
        for spelling in ["ΟΔΟΣ", "οδοσ", "Οδοσ"] {
            let v = i.find_variable(&Name::new(spelling)).expect("variable by folded name");
            assert_eq!(v.id, VariableId { module, local_id: 0 }, "{spelling}: first declaration");
            assert!(std::ptr::eq(
                &**i.find_variable_shared(NormName::intern(spelling)).unwrap(),
                v
            ));
        }
        assert_eq!(i.exported_variables().count(), 1);
    }

    /// Two declarations of one name are two methods: the first is what the
    /// name resolves to, the second is reachable by its ordinal, and a
    /// namesake above is the only edit that renumbers the one below.
    #[test]
    fn namesakes_are_distinct_methods_in_source_order() {
        let i = interface("Процедура П()\nКонецПроцедуры\nФункция п() Экспорт\nКонецФункции\n");
        let module = ModuleId::new(FileId(0));
        let both = i.find_methods(&Name::new("П"));
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].id.local_id, MethodKey::first("П"));
        assert_eq!(both[1].id.local_id, MethodKey::nth("П", 1));
        assert!(!both[0].is_function && both[1].is_function);
        assert!(std::ptr::eq(i.find_method(&Name::new("п")).unwrap(), both[0]));
        let second = i.find_method_by_id(MethodId { module, local_id: MethodKey::nth("п", 1) });
        assert!(second.is_some_and(|m| m.is_export));

        let shifted = interface(
            "Процедура Другая()\nКонецПроцедуры\nПроцедура П()\nКонецПроцедуры\nФункция п() Экспорт\nКонецФункции\n",
        );
        for m in i.methods() {
            assert_eq!(shifted.find_method_by_id(m.id).map(|s| s.is_function), Some(m.is_function));
        }
    }

    #[test]
    fn module_level_region_narrows_the_method_env() {
        let i = interface(
            "#Если Сервер Тогда\nПроцедура НаСервере()\nКонецПроцедуры\n#КонецЕсли\nПроцедура Везде()\nКонецПроцедуры\n",
        );
        let inside = i.find_method(&Name::new("НаСервере")).unwrap();
        let outside = i.find_method(&Name::new("Везде")).unwrap();
        assert_ne!(inside.preproc_env, EnvFlags::ALL);
        assert_eq!(outside.preproc_env, EnvFlags::ALL);
    }

    #[test]
    fn declarations_ignore_where_the_text_sits() {
        let module = "Процедура П()\nКонецПроцедуры\n";
        let shifted = format!("// комментарий\n\n\n{module}");
        // A doc comment right above the method is part of the declaration, so the
        // shift is made with blank lines between the comment and the method.
        assert_eq!(interface(module), interface(&shifted));
    }
}
