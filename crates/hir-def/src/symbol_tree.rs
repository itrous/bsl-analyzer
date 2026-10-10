//! Navigation view of a module's declarations: the position-free
//! [`ModuleInterface`] joined with the ranges of the [`ItemTree`].
//!
//! Inference never reads this tree — it reads the interface, which does not
//! change when text moves. Everything that needs to point at a declaration
//! (hover, go-to, document symbols, diagnostics on a signature) reads this.

use crate::item_tree::{Annotation, ItemTree, ModItem};
use crate::module_interface::{MethodDecl, ModuleInterface, VariableDecl};
use crate::{MethodId, ModuleId, Name, VariableId};
use intern::NormName;
use la_arena::{Arena, Idx};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::sync::Arc;
use stdx::case::CaseExt;
use syntax::{Parse, SyntaxKind, SyntaxNode};
use text_size::TextRange;

pub use crate::module_interface::ParamSymbol;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTree {
    methods: Arena<MethodSymbol>,

    variables: Arena<VariableSymbol>,

    /// Keyed like [`ModuleInterface`], so both resolve a name to the same method.
    methods_by_name: FxHashMap<NormName, Vec<Idx<MethodSymbol>>>,

    variables_by_name: FxHashMap<SmolStr, Vec<Idx<VariableSymbol>>>,
}

/// A method declaration together with where it sits in the file. The
/// declaration itself is reachable through `Deref`, so a reader that only
/// needs the name or the parameters is written the same way against either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSymbol {
    pub decl: MethodDecl,

    pub annotations: Vec<Annotation>,

    pub source_range: TextRange,

    pub name_range: TextRange,
}

impl std::ops::Deref for MethodSymbol {
    type Target = MethodDecl;

    fn deref(&self) -> &MethodDecl {
        &self.decl
    }
}

impl MethodSymbol {
    pub fn syntax_node(&self, parse: &Parse<SyntaxNode>) -> Option<SyntaxNode> {
        method_node_at(parse, self.source_range, self.is_function)
    }
}

/// The method node covering exactly `range` in the file tree.
///
/// Descends by range containment instead of scanning every descendant: the
/// covering element of a method's full range is the method node itself,
/// possibly below same-range wrapper nodes, which are climbed back off here.
pub fn method_node_at(
    parse: &Parse<SyntaxNode>,
    range: TextRange,
    is_function: bool,
) -> Option<SyntaxNode> {
    let expected_kind =
        if is_function { SyntaxKind::FUNCTION_DEF } else { SyntaxKind::PROCEDURE_DEF };
    let root = parse.syntax_node();
    if !root.text_range().contains_range(range) {
        return None;
    }
    let mut node = match root.covering_element(range) {
        syntax::NodeOrToken::Node(n) => n,
        syntax::NodeOrToken::Token(t) => t.parent()?,
    };
    let mut found = None;
    loop {
        if node.text_range() == range && node.kind() == expected_kind {
            found = Some(node.clone());
        }
        match node.parent() {
            Some(parent) if parent.text_range() == range => node = parent,
            _ => break,
        }
    }
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSymbol {
    pub decl: VariableDecl,

    pub annotations: Vec<Annotation>,

    pub source_range: TextRange,

    pub name_range: TextRange,
}

impl std::ops::Deref for VariableSymbol {
    type Target = VariableDecl;

    fn deref(&self) -> &VariableDecl {
        &self.decl
    }
}

impl SymbolTree {
    /// Symbols of a parsed module without a database: the interface is built
    /// here from the same inputs the query path uses.
    pub fn from_item_tree(
        item_tree: &ItemTree,
        module_id: ModuleId,
        parse: &syntax::Parse<syntax::SyntaxNode>,
        source_text: &str,
    ) -> Self {
        let conditionals = item_tree
            .has_module_preproc()
            .then(|| crate::conditional_tree::lower_conditionals(&parse.syntax_node()));
        let interface = ModuleInterface::from_item_tree(
            item_tree,
            module_id,
            parse,
            source_text,
            conditionals.as_ref(),
        );
        Self::assemble(&interface, item_tree)
    }

    /// Join declarations with their ranges. Both come from the same item tree,
    /// so every declaration finds its item by key.
    pub fn assemble(interface: &ModuleInterface, item_tree: &ItemTree) -> Self {
        let mut methods = Arena::new();
        let mut variables = Arena::new();
        let mut methods_by_name: FxHashMap<NormName, Vec<Idx<MethodSymbol>>> = FxHashMap::default();
        let mut variables_by_name: FxHashMap<SmolStr, Vec<Idx<VariableSymbol>>> =
            FxHashMap::default();
        let module = interface.module_id();

        for (idx, item) in item_tree.top_level_items().iter().enumerate() {
            let local_id = idx as u32;
            match item {
                ModItem::Procedure(_) | ModItem::Function(_) => {
                    let method = item_tree.method_item(item).expect("a method item");
                    let id = MethodId { module, local_id: method.key() };
                    let Some(decl) = interface.find_method_by_id(id).cloned() else {
                        continue;
                    };
                    let symbol = MethodSymbol {
                        decl,
                        annotations: method.annotations().to_vec(),
                        source_range: method.source_range(),
                        name_range: method.name_range(),
                    };
                    let key = id.local_id.name;
                    let idx = methods.alloc(symbol);
                    methods_by_name.entry(key).or_default().push(idx);
                }
                ModItem::Variable(var_idx) => {
                    let var = item_tree.variable(*var_idx);
                    let Some(decl) =
                        interface.find_variable_by_id(VariableId { module, local_id }).cloned()
                    else {
                        continue;
                    };
                    let symbol = VariableSymbol {
                        decl,
                        annotations: var.annotations.to_vec(),
                        source_range: var.source_range,
                        name_range: var.name_range,
                    };
                    let key: SmolStr = symbol.name.as_str().fold_lower().into();
                    let idx = variables.alloc(symbol);
                    let entry = variables_by_name.entry(key).or_default();
                    if entry.is_empty() {
                        entry.push(idx);
                    }
                }
            }
        }

        SymbolTree { methods, variables, methods_by_name, variables_by_name }
    }

    #[cfg(test)]
    pub fn from_item_tree_no_docs(item_tree: &ItemTree, module_id: ModuleId) -> Self {
        let interface = ModuleInterface::from_item_tree_no_docs(item_tree, module_id);
        Self::assemble(&interface, item_tree)
    }

    pub fn find_method(&self, name: &Name) -> Option<&MethodSymbol> {
        let indices = self.methods_by_name.get(&NormName::intern(name.as_str()))?;
        indices.first().map(|&idx| &self.methods[idx])
    }

    pub fn find_methods(&self, name: &Name) -> Vec<&MethodSymbol> {
        self.methods_by_name
            .get(&NormName::intern(name.as_str()))
            .map(|indices| indices.iter().map(|&idx| &self.methods[idx]).collect())
            .unwrap_or_default()
    }

    pub fn find_variable(&self, name: &Name) -> Option<&VariableSymbol> {
        let key = name.as_str().fold_lower();
        let indices = self.variables_by_name.get(key.as_str())?;
        indices.first().map(|&idx| &self.variables[idx])
    }

    pub fn methods(&self) -> impl Iterator<Item = &MethodSymbol> {
        self.methods.iter().map(|(_, m)| m)
    }

    pub fn exported_methods(&self) -> impl Iterator<Item = &MethodSymbol> {
        self.methods().filter(|m| m.is_export)
    }

    pub fn variables(&self) -> impl Iterator<Item = &VariableSymbol> {
        self.variables.iter().map(|(_, v)| v)
    }

    pub fn exported_variables(&self) -> impl Iterator<Item = &VariableSymbol> {
        self.variables().filter(|v| v.is_export)
    }

    pub fn find_method_by_id(&self, method_id: MethodId) -> Option<&MethodSymbol> {
        self.methods().find(|m| m.id == method_id)
    }

    pub fn find_variable_by_id(&self, variable_id: VariableId) -> Option<&VariableSymbol> {
        self.variables().find(|v| v.id == variable_id)
    }
}

/// Rough live bytes of a [`SymbolTree`] for Salsa's `memory_usage` introspection:
/// the method/variable arenas with each symbol's name, param-symbol vec and
/// annotation vec, plus the two name-index maps and their `SmolStr` keys.
/// `docs` (`Option<Arc<…Docs>>`) counts only as the inline `Arc` pointer already
/// in the arena element — the pointee is shared, so its heap is not re-attributed
/// here. Nested `TypeRef` payloads inside params are likewise left at their
/// inline size; both keep the estimate simple at the cost of a mild undercount.
pub(crate) fn symbol_tree_heap(v: &Arc<SymbolTree>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes, smol_str_bytes, vec_bytes};

    let tree = &**v;
    let mut bytes = std::mem::size_of::<SymbolTree>();

    bytes += vec_bytes::<MethodSymbol>(tree.methods.len());
    for method in tree.methods.values() {
        bytes += name_bytes(&method.name);
        bytes += vec_bytes::<ParamSymbol>(method.params.len());
        for param in &method.params {
            bytes += name_bytes(&param.name);
        }
        bytes += vec_bytes::<Annotation>(method.annotations.len());
        bytes += vec_bytes::<crate::item_tree::AnnotationKind>(method.directives.len());
    }

    bytes += vec_bytes::<VariableSymbol>(tree.variables.len());
    for variable in tree.variables.values() {
        bytes += name_bytes(&variable.name);
        bytes += vec_bytes::<Annotation>(variable.annotations.len());
        bytes += vec_bytes::<crate::item_tree::AnnotationKind>(variable.directives.len());
    }

    bytes += map_table_bytes::<NormName, Vec<Idx<MethodSymbol>>>(tree.methods_by_name.len());
    for idxs in tree.methods_by_name.values() {
        bytes += vec_bytes::<Idx<MethodSymbol>>(idxs.len());
    }

    bytes += map_table_bytes::<SmolStr, Vec<Idx<VariableSymbol>>>(tree.variables_by_name.len());
    for (key, idxs) in &tree.variables_by_name {
        bytes += smol_str_bytes(key.len()) + vec_bytes::<Idx<VariableSymbol>>(idxs.len());
    }

    bytes
}

// Navigation view; its ranges move on every edit, so nothing on the inference
// path reads it (that path reads `module_interface`). High cap keeps it across
// chunk-boundary LRU trims for the IDE surfaces that do.
// The cap holds a whole workspace: `module_members` folds every file's tree, and a
// resident that warms them in chunks with trims between must find them still memoised.
#[salsa::tracked(lru = 65536, heap_size = crate::symbol_tree::symbol_tree_heap, returns(ref))]
pub fn symbol_tree_query<'db>(
    db: &'db dyn crate::DefDatabase,
    file_id_input: base_db::FileIdInput<'db>,
) -> std::sync::Arc<SymbolTree> {
    let _span = tracing::info_span!("symbol_tree", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let item_tree = db.item_tree_ref(file_id);
    let interface = db.module_interface_ref(crate::ModuleId::new(file_id));
    std::sync::Arc::new(SymbolTree::assemble(interface, item_tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_tree::{Function, Param, Procedure, Variable};
    use crate::ModuleId;
    use syntax::{Parse, SyntaxNode};
    use text_size::TextSize;
    use vfs::FileId;

    fn make_text_range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    fn parse(code: &str) -> Parse<SyntaxNode> {
        parser::parse_with_shared_cache(code)
    }

    #[test]
    fn test_symbol_tree_basic() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("Первая"),
            name: Name::new("Первая"),
            is_export: false,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc_idx));

        let func_idx = item_tree.functions.alloc(Function {
            key: crate::MethodKey::first("Вторая"),
            name: Name::new("Вторая"),
            is_export: true,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(20, 30),
            name_range: make_text_range(20, 30),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Function(func_idx));

        let var_idx = item_tree.variables.alloc(Variable {
            name: Name::new("МодульнаяПеременная"),
            is_export: true,
            annotations: Box::new([]),
            source_range: make_text_range(40, 50),
            name_range: make_text_range(40, 50),
        });
        item_tree.top_level.push(ModItem::Variable(var_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert_eq!(symbol_tree.methods().count(), 2);

        let first = symbol_tree.find_method(&Name::new("Первая")).unwrap();
        assert_eq!(first.name.as_str(), "Первая");
        assert!(!first.is_function);
        assert!(!first.is_export);

        let second = symbol_tree.find_method(&Name::new("Вторая")).unwrap();
        assert_eq!(second.name.as_str(), "Вторая");
        assert!(second.is_function);
        assert!(second.is_export);

        assert_eq!(symbol_tree.variables().count(), 1);

        let var = symbol_tree.find_variable(&Name::new("МодульнаяПеременная")).unwrap();
        assert_eq!(var.name.as_str(), "МодульнаяПеременная");
        assert!(var.is_export);
    }

    #[test]
    fn test_case_insensitive_lookup() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("МояПроцедура"),
            name: Name::new("МояПроцедура"),
            is_export: false,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert!(symbol_tree.find_method(&Name::new("МояПроцедура")).is_some());
        assert!(symbol_tree.find_method(&Name::new("мояпроцедура")).is_some());
        assert!(symbol_tree.find_method(&Name::new("МОЯПРОЦЕДУРА")).is_some());
        assert!(symbol_tree.find_method(&Name::new("МоЯпРоЦеДуРа")).is_some());

        let m1 = symbol_tree.find_method(&Name::new("МояПроцедура")).unwrap();
        let m2 = symbol_tree.find_method(&Name::new("мояпроцедура")).unwrap();
        assert_eq!(m1.id, m2.id);
    }

    #[test]
    fn test_exported_methods_filter() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc1_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("Приватная"),
            name: Name::new("Приватная"),
            is_export: false,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc1_idx));

        let proc2_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("Публичная"),
            name: Name::new("Публичная"),
            is_export: true,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(20, 30),
            name_range: make_text_range(20, 30),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc2_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert_eq!(symbol_tree.methods().count(), 2);

        let exported: Vec<_> = symbol_tree.exported_methods().collect();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].name.as_str(), "Публичная");
    }

    #[test]
    fn test_method_with_parameters() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("СПараметрами"),
            name: Name::new("СПараметрами"),
            is_export: false,
            params: Box::new([
                Param {
                    name: Name::new("Параметр1"),
                    is_val: true,
                    has_default: false,
                    default_value: None,
                    name_range: make_text_range(0, 10),
                },
                Param {
                    name: Name::new("Параметр2"),
                    is_val: false,
                    has_default: true,
                    default_value: None,
                    name_range: make_text_range(12, 22),
                },
            ]),
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: Some(make_text_range(0, 10)),
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        let method = symbol_tree.find_method(&Name::new("СПараметрами")).unwrap();
        assert_eq!(method.params.len(), 2);
        assert_eq!(method.params[0].name.as_str(), "Параметр1");
        assert!(method.params[0].is_val);
        assert!(!method.params[0].has_default);
        assert_eq!(method.params[1].name.as_str(), "Параметр2");
        assert!(!method.params[1].is_val);
        assert!(method.params[1].has_default);
    }

    #[test]
    fn test_method_with_annotations() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("НаКлиенте"),
            name: Name::new("НаКлиенте"),
            is_export: true,
            params: Box::new([]),
            annotations: Box::new([Annotation {
                kind: crate::item_tree::AnnotationKind::AtClient,
                range: make_text_range(0, 10),
            }]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        let method = symbol_tree.find_method(&Name::new("НаКлиенте")).unwrap();
        assert_eq!(method.annotations.len(), 1);
        assert_eq!(method.annotations[0].kind, crate::item_tree::AnnotationKind::AtClient);
    }

    #[test]
    fn test_not_found() {
        let item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert!(symbol_tree.find_method(&Name::new("НесуществующаяПроцедура")).is_none());
        assert!(symbol_tree.find_variable(&Name::new("НесуществующаяПеременная")).is_none());
    }

    #[test]
    fn test_empty_symbol_tree() {
        let item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert_eq!(symbol_tree.methods().count(), 0);
        assert_eq!(symbol_tree.variables().count(), 0);
        assert_eq!(symbol_tree.exported_methods().count(), 0);
        assert_eq!(symbol_tree.exported_variables().count(), 0);
    }

    #[test]
    fn test_variable_case_insensitive() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let var_idx = item_tree.variables.alloc(Variable {
            name: Name::new("МояПеременная"),
            is_export: false,
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
        });
        item_tree.top_level.push(ModItem::Variable(var_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert!(symbol_tree.find_variable(&Name::new("МояПеременная")).is_some());
        assert!(symbol_tree.find_variable(&Name::new("мояпеременная")).is_some());
        assert!(symbol_tree.find_variable(&Name::new("МОЯПЕРЕМЕННАЯ")).is_some());
    }

    /// The symbol tree is assembled from the interface, so both must fold a
    /// name the same way; `NormName` folds per character, and a contextual
    /// fold (Greek final sigma) would land the same spelling in another bucket.
    #[test]
    fn method_lookup_folds_names_like_the_interface() {
        let fixture = "Функция ΟΔΟΣ() Экспорт\n\tВозврат 1;\nКонецФункции\nФункция οδοσ() Экспорт\n\tВозврат 2;\nКонецФункции\n";
        let item_tree = ItemTree::from_parse(&parse(fixture));
        let module_id = ModuleId::new(FileId(0));
        let interface = ModuleInterface::from_item_tree_no_docs(&item_tree, module_id);
        let symbol_tree = SymbolTree::assemble(&interface, &item_tree);

        for spelling in ["ΟΔΟΣ", "οδοσ"] {
            let name = Name::new(spelling);
            assert_eq!(
                symbol_tree.find_method(&name).map(|m| m.id),
                interface.find_method(&name).map(|m| m.id),
                "{spelling}"
            );
            assert_eq!(symbol_tree.find_methods(&name).len(), 2, "{spelling}");
        }
    }

    #[test]
    fn test_variable_comma_decl_lookup() {
        let fixture = r#"
Перем A, B;
Перем b;
        "#;
        let item_tree = ItemTree::from_parse(&parse(fixture));
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        let variables: Vec<_> = symbol_tree.variables().collect();
        assert_eq!(
            variables.iter().map(|var| var.name.as_str()).collect::<Vec<_>>(),
            ["A", "B", "b"]
        );

        let first_a = symbol_tree.find_variable(&Name::new("a")).unwrap();
        let first_b = symbol_tree.find_variable(&Name::new("b")).unwrap();
        let upper_b = symbol_tree.find_variable(&Name::new("B")).unwrap();
        let redeclared_b = variables.iter().find(|var| var.name.as_str() == "b").unwrap();

        assert_eq!(first_a.name.as_str(), "A");
        assert_eq!(first_b.name.as_str(), "B");
        assert_eq!(upper_b.name.as_str(), "B");
        // The redeclared `b` must not replace the first entry in the case-insensitive index.
        assert_ne!(first_b.source_range, redeclared_b.source_range);
        assert_eq!(first_b.source_range, upper_b.source_range);
    }

    #[test]
    fn test_exported_variables_filter() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let var1_idx = item_tree.variables.alloc(Variable {
            name: Name::new("Приватная"),
            is_export: false,
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
        });
        item_tree.top_level.push(ModItem::Variable(var1_idx));

        let var2_idx = item_tree.variables.alloc(Variable {
            name: Name::new("Публичная"),
            is_export: true,
            annotations: Box::new([]),
            source_range: make_text_range(20, 30),
            name_range: make_text_range(20, 30),
        });
        item_tree.top_level.push(ModItem::Variable(var2_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        assert_eq!(symbol_tree.variables().count(), 2);

        let exported: Vec<_> = symbol_tree.exported_variables().collect();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].name.as_str(), "Публичная");
    }

    #[test]
    fn test_find_methods_multiple() {
        let mut item_tree = ItemTree::default();
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let proc_idx = item_tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("Метод"),
            name: Name::new("Метод"),
            is_export: false,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: make_text_range(0, 10),
            name_range: make_text_range(0, 10),
            param_list_range: None,
            sig_end: make_text_range(0, 10).end(),
        });
        item_tree.top_level.push(ModItem::Procedure(proc_idx));

        let symbol_tree = SymbolTree::from_item_tree_no_docs(&item_tree, module_id);

        let methods = symbol_tree.find_methods(&Name::new("Метод"));
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name.as_str(), "Метод");

        let not_found = symbol_tree.find_methods(&Name::new("НеСуществует"));
        assert_eq!(not_found.len(), 0);
    }
}
