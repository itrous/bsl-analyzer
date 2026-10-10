pub mod lower;

use smol_str::SmolStr;

use crate::{MethodKey, Name};
use base_db::RootQueryDb;
use intern::NormName;
use la_arena::{Arena, Idx};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use text_size::{TextRange, TextSize};
use vfs::FileId;

pub fn lower_file(db: &dyn RootQueryDb, file_id: FileId) -> Arc<ItemTree> {
    lower::Ctx::lower_file(db, file_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTree {
    pub(crate) top_level: Vec<ModItem>,

    pub(crate) procedures: Arena<Procedure>,

    pub(crate) functions: Arena<Function>,

    pub(crate) variables: Arena<Variable>,

    /// Each method's item under its key — the one place a key is assigned
    /// (see [`MethodKey`]) and the lookup every keyed reader goes through.
    pub(crate) methods_by_key: FxHashMap<MethodKey, ModItem>,

    /// The file has a `#Если` region outside any method — only then can an
    /// item-level preprocessor condition narrow a method's environments, so
    /// consumers may skip the conditional tree entirely when this is false.
    pub(crate) has_module_preproc: bool,
}

impl Default for ItemTree {
    fn default() -> Self {
        Self {
            top_level: Vec::new(),
            procedures: Arena::new(),
            functions: Arena::new(),
            variables: Arena::new(),
            methods_by_key: FxHashMap::default(),
            has_module_preproc: false,
        }
    }
}

/// A procedure or function of the item tree, read uniformly: the two item
/// kinds carry the same declaration facts.
#[derive(Debug, Clone, Copy)]
pub enum MethodItem<'a> {
    Procedure(&'a Procedure),
    Function(&'a Function),
}

impl<'a> MethodItem<'a> {
    pub fn key(self) -> MethodKey {
        match self {
            Self::Procedure(p) => p.key,
            Self::Function(f) => f.key,
        }
    }

    pub fn is_function(self) -> bool {
        matches!(self, Self::Function(_))
    }

    pub fn name(self) -> &'a Name {
        match self {
            Self::Procedure(p) => &p.name,
            Self::Function(f) => &f.name,
        }
    }

    pub fn is_export(self) -> bool {
        match self {
            Self::Procedure(p) => p.is_export,
            Self::Function(f) => f.is_export,
        }
    }

    pub fn params(self) -> &'a [Param] {
        match self {
            Self::Procedure(p) => &p.params,
            Self::Function(f) => &f.params,
        }
    }

    pub fn annotations(self) -> &'a [Annotation] {
        match self {
            Self::Procedure(p) => &p.annotations,
            Self::Function(f) => &f.annotations,
        }
    }

    pub fn source_range(self) -> TextRange {
        match self {
            Self::Procedure(p) => p.source_range,
            Self::Function(f) => f.source_range,
        }
    }

    pub fn name_range(self) -> TextRange {
        match self {
            Self::Procedure(p) => p.name_range,
            Self::Function(f) => f.name_range,
        }
    }

    pub fn param_list_range(self) -> Option<TextRange> {
        match self {
            Self::Procedure(p) => p.param_list_range,
            Self::Function(f) => f.param_list_range,
        }
    }

    pub fn sig_end(self) -> TextSize {
        match self {
            Self::Procedure(p) => p.sig_end,
            Self::Function(f) => f.sig_end,
        }
    }
}

/// Assigns each method its key while the tree is lowered: the ordinal is
/// the count of earlier declarations of the same folded name.
#[derive(Default)]
pub(crate) struct MethodKeys {
    seen: FxHashMap<NormName, u32>,
}

impl MethodKeys {
    pub(crate) fn next(&mut self, name: &Name) -> MethodKey {
        let name = NormName::intern(name.as_str());
        let ordinal = self.seen.entry(name).or_insert(0);
        let key = MethodKey { name, ordinal: *ordinal };
        *ordinal += 1;
        key
    }
}

impl ItemTree {
    pub fn from_parse(parse: &syntax::Parse<syntax::SyntaxNode>) -> Self {
        use syntax::ast::{self, AstNode};

        let file = match ast::SourceFile::cast(parse.syntax_node()) {
            Some(f) => f,
            None => return ItemTree::default(),
        };

        let mut tree = ItemTree::default();
        lower::lower_module_items_into(&file, &mut tree);
        tree
    }

    pub fn top_level_items(&self) -> &[ModItem] {
        &self.top_level
    }

    /// The methods in declaration order.
    pub fn methods(&self) -> impl Iterator<Item = MethodItem<'_>> + '_ {
        self.top_level.iter().filter_map(|item| self.method_item(item))
    }

    /// The method under `key`; `None` when the module declares no such
    /// method (or fewer namesakes than the ordinal asks for).
    pub fn method(&self, key: MethodKey) -> Option<MethodItem<'_>> {
        self.method_item(self.methods_by_key.get(&key)?)
    }

    /// The top-level item under `key`, for readers that match on the item
    /// kind themselves.
    pub fn item_of(&self, key: MethodKey) -> Option<&ModItem> {
        self.methods_by_key.get(&key)
    }

    /// Full range and kind of the method under `key`.
    pub fn method_at(&self, key: MethodKey) -> Option<(TextRange, bool)> {
        let item = self.method(key)?;
        Some((item.source_range(), item.is_function()))
    }

    /// The method view of a top-level item; `None` for a variable.
    pub fn method_item(&self, item: &ModItem) -> Option<MethodItem<'_>> {
        match item {
            ModItem::Procedure(idx) => Some(MethodItem::Procedure(self.procedure(*idx))),
            ModItem::Function(idx) => Some(MethodItem::Function(self.function(*idx))),
            ModItem::Variable(_) => None,
        }
    }

    pub fn has_module_preproc(&self) -> bool {
        self.has_module_preproc
    }

    pub fn procedure(&self, idx: Idx<Procedure>) -> &Procedure {
        &self.procedures[idx]
    }

    pub fn function(&self, idx: Idx<Function>) -> &Function {
        &self.functions[idx]
    }

    pub fn variable(&self, idx: Idx<Variable>) -> &Variable {
        &self.variables[idx]
    }

    pub fn procedures(&self) -> impl Iterator<Item = (Idx<Procedure>, &Procedure)> {
        self.procedures.iter()
    }

    pub fn functions(&self) -> impl Iterator<Item = (Idx<Function>, &Function)> {
        self.functions.iter()
    }

    pub fn variables(&self) -> impl Iterator<Item = (Idx<Variable>, &Variable)> {
        self.variables.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModItem {
    Procedure(Idx<Procedure>),
    Function(Idx<Function>),
    Variable(Idx<Variable>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Procedure {
    pub key: MethodKey,
    pub name: Name,
    pub is_export: bool,
    pub params: Box<[Param]>,
    pub annotations: Box<[Annotation]>,
    pub source_range: TextRange,
    pub name_range: TextRange,
    pub param_list_range: Option<TextRange>,
    /// End of the declaration header — the closing `)` of the parameter list, or
    /// the export keyword when present. Anchors the full (possibly multi-line)
    /// signature slice, distinct from `name_range` which is the name token alone.
    pub sig_end: TextSize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub key: MethodKey,
    pub name: Name,
    pub is_export: bool,
    pub params: Box<[Param]>,
    pub annotations: Box<[Annotation]>,
    pub source_range: TextRange,
    pub name_range: TextRange,
    pub param_list_range: Option<TextRange>,
    /// End of the declaration header — the closing `)` of the parameter list, or
    /// the export keyword when present. Anchors the full (possibly multi-line)
    /// signature slice, distinct from `name_range` which is the name token alone.
    pub sig_end: TextSize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: Name,
    pub is_export: bool,
    pub annotations: Box<[Annotation]>,
    pub source_range: TextRange,
    pub name_range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: Name,
    pub is_val: bool,
    pub has_default: bool,
    /// Source text of the default-value expression (`= <expr>`), whitespace-collapsed, when the
    /// parameter is optional — so a declaration can be rendered faithfully as `Имя = Неопределено`.
    ///
    /// `None` for a REQUIRED parameter. An optional parameter whose expression could not be
    /// read yields `Some("")`, not `None`: the parser builds an expression node after every
    /// `=`, so the node is there and empty. A reader therefore cannot tell "optional" from
    /// "required" by this field alone — `has_default` answers that — and must treat an empty
    /// text as an unknown default rather than as no default, or it will render `Имя = ` with
    /// a dangling equals sign.
    pub default_value: Option<SmolStr>,
    pub name_range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Annotation {
    pub kind: AnnotationKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnnotationKind {
    AtClient,
    AtServer,
    AtClientAtServer,
    AtClientAtServerNoContext,
    AtServerNoContext,
    Before,
    After,
    Instead,
    ChangeAndValidate,
}

impl AnnotationKind {
    /// Extension method interception (`&Перед`, `&После`, `&Вместо`,
    /// `&ИзменениеИКонтроль`): the method's signature is fixed by the intercepted one.
    pub fn is_interception(self) -> bool {
        matches!(self, Self::Before | Self::After | Self::Instead | Self::ChangeAndValidate)
    }

    /// The canonical wire spelling of a compilation directive.
    ///
    /// Each directive has a localized and an English form (`&НаКлиенте` / `&AtClient`), so
    /// the source text cannot be the name a consumer matches on. `snake_case` to match the
    /// key style of the agent-facing contracts this travels in.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AtClient => "at_client",
            Self::AtServer => "at_server",
            Self::AtClientAtServer => "at_client_at_server",
            Self::AtClientAtServerNoContext => "at_client_at_server_no_context",
            Self::AtServerNoContext => "at_server_no_context",
            Self::Before => "before",
            Self::After => "after",
            Self::Instead => "instead",
            Self::ChangeAndValidate => "change_and_validate",
        }
    }
}

/// Rough live bytes of an [`ItemTree`] for Salsa's `memory_usage` introspection:
/// the three item arenas plus each item's per-name `SmolStr` payload and its
/// boxed param/annotation slices. `Annotation`/`ModItem` are `Copy` and carry no
/// further heap, so the arena/slice element counts capture them fully.
pub(crate) fn item_tree_heap(v: &Arc<ItemTree>) -> usize {
    use crate::heap_estimate::{name_bytes, vec_bytes};

    let tree = &**v;
    let mut bytes = std::mem::size_of::<ItemTree>();
    bytes += vec_bytes::<ModItem>(tree.top_level.len());
    bytes += crate::heap_estimate::map_table_bytes::<MethodKey, ModItem>(tree.methods_by_key.len());

    bytes += vec_bytes::<Procedure>(tree.procedures.len());
    for proc in tree.procedures.values() {
        bytes += name_bytes(&proc.name);
        bytes += vec_bytes::<Param>(proc.params.len());
        for param in proc.params.iter() {
            bytes += name_bytes(&param.name);
            bytes += param.default_value.as_ref().map_or(0, |s| s.len());
        }
        bytes += vec_bytes::<Annotation>(proc.annotations.len());
    }

    bytes += vec_bytes::<Function>(tree.functions.len());
    for func in tree.functions.values() {
        bytes += name_bytes(&func.name);
        bytes += vec_bytes::<Param>(func.params.len());
        for param in func.params.iter() {
            bytes += name_bytes(&param.name);
            bytes += param.default_value.as_ref().map_or(0, |s| s.len());
        }
        bytes += vec_bytes::<Annotation>(func.annotations.len());
    }

    bytes += vec_bytes::<Variable>(tree.variables.len());
    for var in tree.variables.values() {
        bytes += name_bytes(&var.name);
        bytes += vec_bytes::<Annotation>(var.annotations.len());
    }

    bytes
}

// Condensed module item index (no green-tree pin): feeds symbol/name resolution
// across modules. The cap holds a whole workspace: a resident that trims after every
// request, or a sweep that trims between chunks, must not re-lower a callee's index
// from a re-parse each time a request reaches into it.
#[salsa::tracked(lru = 65536, heap_size = crate::item_tree::item_tree_heap, returns(ref))]
pub fn item_tree_query<'db>(
    db: &'db dyn base_db::RootQueryDb,
    file_id_input: base_db::FileIdInput<'db>,
) -> Arc<ItemTree> {
    let _span = tracing::info_span!("item_tree", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    lower_file(db, file_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_tree_default() {
        let tree = ItemTree::default();
        assert_eq!(tree.top_level_items().len(), 0);
    }

    /// Every directive names itself, and no two share a spelling. The match has no `_`
    /// arm, so a new directive fails the build rather than shipping as an empty string —
    /// and a consumer matching on the closed set never meets a name it was not told about.
    #[test]
    fn every_directive_has_its_own_canonical_spelling() {
        let all = [
            AnnotationKind::AtClient,
            AnnotationKind::AtServer,
            AnnotationKind::AtClientAtServer,
            AnnotationKind::AtClientAtServerNoContext,
            AnnotationKind::AtServerNoContext,
            AnnotationKind::Before,
            AnnotationKind::After,
            AnnotationKind::Instead,
            AnnotationKind::ChangeAndValidate,
        ];

        let spellings: Vec<&str> = all.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(
            spellings,
            [
                "at_client",
                "at_server",
                "at_client_at_server",
                "at_client_at_server_no_context",
                "at_server_no_context",
                "before",
                "after",
                "instead",
                "change_and_validate",
            ]
        );

        let unique: std::collections::BTreeSet<&str> = spellings.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "two directives share a spelling: {spellings:?}");
    }

    #[test]
    fn test_procedure_creation() {
        let proc = Procedure {
            key: crate::MethodKey::first("ТестоваяПроцедура"),
            name: Name::new("ТестоваяПроцедура"),
            is_export: true,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: TextRange::new(0.into(), 10.into()),
            name_range: TextRange::new(0.into(), 10.into()),
            param_list_range: None,
            sig_end: 10.into(),
        };

        assert_eq!(proc.name.as_str(), "ТестоваяПроцедура");
        assert!(proc.is_export);
        assert_eq!(proc.params.len(), 0);
    }

    #[test]
    fn test_function_with_params() {
        let func = Function {
            key: crate::MethodKey::first("ТестоваяФункция"),
            name: Name::new("ТестоваяФункция"),
            is_export: false,
            params: Box::new([
                Param {
                    name: Name::new("Параметр1"),
                    is_val: true,
                    has_default: false,
                    default_value: None,
                    name_range: TextRange::new(0.into(), 10.into()),
                },
                Param {
                    name: Name::new("Параметр2"),
                    is_val: false,
                    has_default: true,
                    default_value: Some(SmolStr::new_static("Неопределено")),
                    name_range: TextRange::new(12.into(), 22.into()),
                },
            ]),
            annotations: Box::new([]),
            source_range: TextRange::new(0.into(), 10.into()),
            name_range: TextRange::new(0.into(), 10.into()),
            param_list_range: Some(TextRange::new(0.into(), 10.into())),
            sig_end: 10.into(),
        };

        assert_eq!(func.params.len(), 2);
        assert!(func.params[0].is_val);
        assert!(!func.params[0].has_default);
        assert!(!func.params[1].is_val);
        assert!(func.params[1].has_default);
    }

    #[test]
    fn test_annotation_kinds() {
        let ann1 = Annotation {
            kind: AnnotationKind::AtClient,
            range: TextRange::new(0.into(), 10.into()),
        };
        let ann2 = Annotation {
            kind: AnnotationKind::AtServer,
            range: TextRange::new(0.into(), 10.into()),
        };

        assert_ne!(ann1.kind, ann2.kind);
    }

    #[test]
    fn test_item_tree_procedures() {
        let mut tree = ItemTree::default();

        let proc_idx = tree.procedures.alloc(Procedure {
            key: crate::MethodKey::first("Процедура1"),
            name: Name::new("Процедура1"),
            is_export: false,
            params: Box::new([]),
            annotations: Box::new([]),
            source_range: TextRange::new(0.into(), 10.into()),
            name_range: TextRange::new(0.into(), 10.into()),
            param_list_range: None,
            sig_end: 10.into(),
        });

        tree.top_level.push(ModItem::Procedure(proc_idx));

        assert_eq!(tree.top_level_items().len(), 1);
        assert!(matches!(tree.top_level_items()[0], ModItem::Procedure(_)));

        let proc = tree.procedure(proc_idx);
        assert_eq!(proc.name.as_str(), "Процедура1");
    }
}
