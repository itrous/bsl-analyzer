mod docs;
mod sdbl;
#[cfg(test)]
mod token_stream_tests;

use ide_db::{RootDatabase, TextRange};
use syntax::{
    ast::{self, AstNode},
    SyntaxKind, SyntaxNode, SyntaxToken,
};
use vfs::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlTag {
    Keyword,
    Function,
    Procedure,
    Parameter,
    Variable,
    StringLiteral,
    NumberLiteral,
    BooleanLiteral,
    Comment,
    Preprocessor,
    Annotation,
    Property,
    Operator,
    UnresolvedReference,
    BuiltinFunction,
    Type,
    EnumMember,
    Namespace,
    Class,
}

impl HlTag {
    pub fn as_str(&self) -> &'static str {
        match self {
            HlTag::Keyword => "keyword",
            HlTag::Function => "function",
            HlTag::Procedure => "function",
            HlTag::Parameter => "parameter",
            HlTag::Variable => "variable",
            HlTag::StringLiteral => "string",
            HlTag::NumberLiteral => "number",
            HlTag::BooleanLiteral => "keyword",
            HlTag::Comment => "comment",
            HlTag::Preprocessor => "macro",
            HlTag::Annotation => "decorator",
            HlTag::Property => "property",
            HlTag::Operator => "operator",
            HlTag::UnresolvedReference => "unresolvedReference",
            HlTag::BuiltinFunction => "function",
            HlTag::Type => "type",
            HlTag::EnumMember => "enumMember",
            HlTag::Namespace => "namespace",
            HlTag::Class => "class",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HlMod(u32);

impl HlMod {
    pub const EXPORT: HlMod = HlMod(1 << 0);
    pub const DEPRECATED: HlMod = HlMod(1 << 1);
    pub const ASYNC: HlMod = HlMod(1 << 2);
    pub const DECLARATION: HlMod = HlMod(1 << 3);
    pub const DEFINITION: HlMod = HlMod(1 << 4);
    /// Occurrences inside structured method documentation.
    pub const DOCUMENTATION: HlMod = HlMod(1 << 5);

    pub const fn new() -> Self {
        HlMod(0)
    }

    pub const fn with(mut self, modifier: HlMod) -> Self {
        self.0 |= modifier.0;
        self
    }

    pub const fn contains(self, modifier: HlMod) -> bool {
        (self.0 & modifier.0) != 0
    }

    pub fn as_strings(&self) -> Vec<&'static str> {
        let mut result = Vec::new();
        if self.contains(HlMod::EXPORT) {
            result.push("defaultLibrary");
        }
        if self.contains(HlMod::DEPRECATED) {
            result.push("deprecated");
        }
        if self.contains(HlMod::ASYNC) {
            result.push("async");
        }
        if self.contains(HlMod::DECLARATION) {
            result.push("declaration");
        }
        if self.contains(HlMod::DEFINITION) {
            result.push("definition");
        }
        if self.contains(HlMod::DOCUMENTATION) {
            result.push("documentation");
        }
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlRange {
    pub range: TextRange,
    pub tag: HlTag,
    pub modifiers: HlMod,
}

#[derive(Debug, Clone)]
pub struct HighlightResult {
    pub highlights: Vec<HlRange>,
    pub resolved_external_files: Vec<FileId>,
}

pub(crate) struct HighlightContext<'db, DB: RootDatabase> {
    pub(crate) db: &'db DB,
    pub(crate) file_id: FileId,

    /// Shared per-file resolution state: binding indexes, scope caches and
    /// module-level lookup tables built once instead of per name token.
    /// Binding types are skipped — highlighting never reads `SemanticSymbol::ty`.
    symbol_ctx: hir::FileSymbolCtx<'db, DB>,

    pub(crate) line_index: Option<Vec<usize>>,

    pub(crate) sdbl_literal_ranges: rustc_hash::FxHashSet<TextRange>,

    pub(crate) resolved_external_files: rustc_hash::FxHashSet<FileId>,
}

impl<'db, DB: RootDatabase> HighlightContext<'db, DB> {
    fn new(db: &'db DB, file_id: FileId, line_index: Option<Vec<usize>>) -> Self {
        Self {
            db,
            file_id,
            symbol_ctx: hir::FileSymbolCtx::new(db, file_id).without_binding_types(),
            line_index,
            sdbl_literal_ranges: rustc_hash::FxHashSet::default(),
            resolved_external_files: rustc_hash::FxHashSet::default(),
        }
    }
}
pub fn highlight<DB: RootDatabase>(db: &DB, file_id: FileId) -> HighlightResult {
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let bsl_source = db.file_text(file_id);
    let line_index = ide_diagnostics::sdbl_utils::build_line_index_shared(&bsl_source);

    let mut ctx = HighlightContext::new(db, file_id, Some(line_index));
    let mut highlights = Vec::new();

    traverse_node(&mut ctx, &root, &bsl_source, &mut highlights);

    let highlights = normalize_highlights(highlights);

    HighlightResult {
        highlights,
        resolved_external_files: ctx.resolved_external_files.into_iter().collect(),
    }
}

fn traverse_node<DB: RootDatabase>(
    ctx: &mut HighlightContext<DB>,
    node: &SyntaxNode,
    source: &str,
    highlights: &mut Vec<HlRange>,
) {
    if matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF) {
        docs::highlight_leading_comments(node, source, highlights);
    }
    for token in node.children_with_tokens() {
        match token {
            syntax::NodeOrToken::Token(token) => {
                if let Some(hl) = highlight_def_site_token(&token) {
                    highlights.push(hl);
                    continue;
                }

                if token.kind().is_name_token() && !is_region_directive_name(&token) {
                    if let Some(hl) = highlight_name_semantic(ctx, &token) {
                        highlights.push(hl);
                        continue;
                    }
                }

                if let Some(hl) = highlight_token(&token, ctx) {
                    highlights.push(hl);
                }
            }
            syntax::NodeOrToken::Node(node) => {
                if node.kind() == SyntaxKind::LITERAL {
                    if let Some(sdbl_highlights) = sdbl::highlight_sdbl_in_literal(ctx, &node) {
                        ctx.sdbl_literal_ranges.insert(node.text_range());
                        highlights.extend(sdbl_highlights);
                        continue;
                    }
                }

                traverse_node(ctx, &node, source, highlights);
            }
        }
    }
}

fn highlight_name_semantic<DB: RootDatabase>(
    ctx: &mut HighlightContext<DB>,
    token: &SyntaxToken,
) -> Option<HlRange> {
    let range = token.text_range();

    tracing::debug!("highlight_name_semantic: processing token={}", token.text());

    let symbol = ctx.symbol_ctx.symbol_for_token(token)?;

    tracing::debug!("highlight_name_semantic: {} resolved to {:?}", token.text(), symbol);

    let tag = match &symbol.definition {
        Some(hir::Definition::BuiltinFunction(_)) => HlTag::BuiltinFunction,
        _ => match symbol.kind {
            hir::SemanticSymbolKind::Function => HlTag::Function,
            hir::SemanticSymbolKind::Method => HlTag::Function,
            hir::SemanticSymbolKind::Parameter => HlTag::Parameter,
            hir::SemanticSymbolKind::Variable => HlTag::Variable,
            hir::SemanticSymbolKind::Property => HlTag::Property,
            hir::SemanticSymbolKind::Type => HlTag::Type,
            hir::SemanticSymbolKind::Namespace => HlTag::Namespace,
            hir::SemanticSymbolKind::Class => HlTag::Class,
        },
    };

    if let Some(definition) = &symbol.definition {
        match definition {
            hir::Definition::MdoManagerModule { file_id, .. } if *file_id != ctx.file_id => {
                ctx.resolved_external_files.insert(*file_id);
            }
            hir::Definition::Module(module_id) if module_id.file_id != ctx.file_id => {
                ctx.resolved_external_files.insert(module_id.file_id);
            }
            hir::Definition::Method(method_id) if method_id.module.file_id != ctx.file_id => {
                ctx.resolved_external_files.insert(method_id.module.file_id);
            }
            _ => {}
        }
    }

    let mut modifiers = HlMod::new();

    if let Some(definition) = &symbol.definition {
        if definition.is_export(ctx.db) {
            modifiers = modifiers.with(HlMod::EXPORT);
        }
    }

    if matches!(
        symbol.definition,
        Some(hir::Definition::BuiltinFunction(_))
            | Some(hir::Definition::BuiltinMethodHandle { .. })
    ) {
        modifiers = modifiers.with(HlMod::EXPORT);
    }

    if symbol.declaration.as_ref().is_some_and(|declaration| declaration.range == range) {
        modifiers = modifiers.with(HlMod::DECLARATION);
    }

    Some(HlRange { range, tag, modifiers })
}

fn highlight_token<DB: RootDatabase>(
    token: &SyntaxToken,
    ctx: &HighlightContext<DB>,
) -> Option<HlRange> {
    let kind = token.kind();
    let range = token.text_range();

    if kind.is_string_literal() {
        if let Some(parent) = token.parent() {
            if ctx.sdbl_literal_ranges.contains(&parent.text_range()) {
                return None;
            }
        }
    }

    let tag = if kind.is_boolean_literal() {
        HlTag::BooleanLiteral
    } else if kind.is_string_literal() {
        HlTag::StringLiteral
    } else if kind.is_number_literal() {
        HlTag::NumberLiteral
    } else if kind.is_keyword() {
        HlTag::Keyword
    } else if kind == SyntaxKind::COMMENT {
        HlTag::Comment
    } else if kind.is_preprocessor() {
        HlTag::Preprocessor
    } else if kind.is_annotation() {
        HlTag::Annotation
    } else if kind.is_operator() {
        HlTag::Operator
    } else {
        return None;
    };

    Some(HlRange { range, tag, modifiers: HlMod::new() })
}

/// The name in a region directive (`#Область БыстрыйЗапуск`) is the label of a
/// flat folding marker, not an identifier. It must never be resolved
/// semantically: when the label happens to match a procedure name, resolving it
/// would emit a `Function` semantic token that overrides the editor's region
/// coloring, so that one region reads as a method call while every other region
/// name looks normal. Leaving it untagged lets the textmate region coloring show
/// through, consistent with all other region names.
fn is_region_directive_name(token: &SyntaxToken) -> bool {
    token.parent().is_some_and(|p| p.kind() == SyntaxKind::PRE_REGION_DIR)
}

fn highlight_def_site_token(token: &SyntaxToken) -> Option<HlRange> {
    let parent = token.parent()?;
    let range = token.text_range();
    match parent.kind() {
        SyntaxKind::PROCEDURE_DEF => {
            let proc = ast::ProcedureDef::cast(parent)?;
            if proc.name_or_keyword()?.text_range() != range {
                return None;
            }
            let mut modifiers = HlMod::new().with(HlMod::DEFINITION);
            if proc.export_keyword().is_some() {
                modifiers = modifiers.with(HlMod::EXPORT);
            }
            Some(HlRange { range, tag: HlTag::Procedure, modifiers })
        }
        SyntaxKind::FUNCTION_DEF => {
            let func = ast::FunctionDef::cast(parent)?;
            if func.name_or_keyword()?.text_range() != range {
                return None;
            }
            let mut modifiers = HlMod::new().with(HlMod::DEFINITION);
            if func.export_keyword().is_some() {
                modifiers = modifiers.with(HlMod::EXPORT);
            }
            Some(HlRange { range, tag: HlTag::Function, modifiers })
        }
        SyntaxKind::PARAM => {
            let param = ast::Param::cast(parent)?;
            if param.name()?.text_range() != range {
                return None;
            }
            Some(HlRange {
                range,
                tag: HlTag::Parameter,
                modifiers: HlMod::new().with(HlMod::DECLARATION),
            })
        }
        SyntaxKind::VAR_DEF => {
            let var_def = ast::VarDef::cast(parent)?;
            if !var_def.names().any(|n| n.text_range() == range) {
                return None;
            }
            let mut modifiers = HlMod::new().with(HlMod::DECLARATION);
            if var_def.export_keyword().is_some() {
                modifiers = modifiers.with(HlMod::EXPORT);
            }
            Some(HlRange { range, tag: HlTag::Variable, modifiers })
        }
        _ => None,
    }
}

fn normalize_highlights(highlights: Vec<HlRange>) -> Vec<HlRange> {
    if highlights.len() < 2 {
        return highlights;
    }

    let mut sorted = highlights;
    sorted.sort_by(|a, b| {
        a.range
            .start()
            .cmp(&b.range.start())
            .then_with(|| b.range.end().cmp(&a.range.end()))
            .then_with(|| highlight_priority(b).cmp(&highlight_priority(a)))
    });

    let mut result: Vec<HlRange> = Vec::with_capacity(sorted.len());
    for hl in sorted {
        if let Some(last) = result.last() {
            if hl.range.start() < last.range.end() {
                tracing::debug!(
                    target: "ide::syntax_highlighting",
                    dropped = ?hl,
                    kept = ?last,
                    "normalize_highlights: dropping overlapping highlight"
                );
                continue;
            }
        }
        result.push(hl);
    }
    result
}

fn highlight_priority(hl: &HlRange) -> u8 {
    if hl.modifiers.contains(HlMod::DEFINITION) {
        4
    } else if hl.modifiers.contains(HlMod::DECLARATION) {
        3
    } else if matches!(
        hl.tag,
        HlTag::Function
            | HlTag::Procedure
            | HlTag::BuiltinFunction
            | HlTag::Variable
            | HlTag::Parameter
            | HlTag::Type
            | HlTag::Class
            | HlTag::Namespace
            | HlTag::EnumMember
            | HlTag::Property
            | HlTag::UnresolvedReference
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::path::PathBuf;
    use vfs::{FileId, FileSet, VfsPath};

    fn designer_fixture_path() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
    }

    /// Shared isolated database for semantic highlighting regression tests.
    pub(super) fn create_db_with_file(source: &str) -> (RootDatabaseImpl, FileId) {
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
    fn test_region_name_matching_proc_is_not_a_method() {
        // A region whose name matches a procedure name must NOT be highlighted
        // as a function/method: it is a folding-marker label, not a call.
        let code =
            "#Область БыстрыйЗапуск\nПроцедура БыстрыйЗапуск()\nКонецПроцедуры\n#КонецОбласти\n";
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        // The `#Область` label range (byte offsets of "БыстрыйЗапуск" on line 1).
        let label_start = "#Область ".len();
        let label_end = label_start + "БыстрыйЗапуск".len();

        let miscolored = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Function
                && usize::from(hl.range.start()) >= label_start
                && usize::from(hl.range.end()) <= label_end
        });
        assert!(!miscolored, "region name must not be tagged as a function/method");

        // The procedure definition on line 2 must still be highlighted normally.
        let has_proc_def = highlights
            .highlights
            .iter()
            .any(|hl| hl.tag == HlTag::Procedure && usize::from(hl.range.start()) > label_end);
        assert!(has_proc_def, "the procedure definition must still be highlighted");
    }

    #[test]
    fn test_hl_tag_as_str() {
        assert_eq!(HlTag::Keyword.as_str(), "keyword");
        assert_eq!(HlTag::Function.as_str(), "function");
        assert_eq!(HlTag::StringLiteral.as_str(), "string");
    }

    #[test]
    fn test_hl_mod() {
        let mods = HlMod::new().with(HlMod::EXPORT).with(HlMod::DEPRECATED);

        assert!(mods.contains(HlMod::EXPORT));
        assert!(mods.contains(HlMod::DEPRECATED));
        assert!(!mods.contains(HlMod::ASYNC));

        let strings = mods.as_strings();
        assert!(strings.contains(&"defaultLibrary"));
        assert!(strings.contains(&"deprecated"));
    }

    #[test]
    fn test_highlight_parameter() {
        let code = r#"
Функция Тест(Параметр1, Параметр2)
    Результат = Параметр1 + Параметр2;
    Возврат Результат;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let param_highlights: Vec<_> =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::Parameter).collect();

        assert!(
            param_highlights.len() >= 4,
            "Expected at least 4 parameter highlights, got {}",
            param_highlights.len()
        );
    }

    #[test]
    fn test_highlight_local_variable() {
        let code = r#"
Процедура Тест()
    Перем ЛокальнаяПеременная;
    ЛокальнаяПеременная = 42;
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let var_highlights: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable
                    && (hl.modifiers.contains(HlMod::DECLARATION)
                        || !hl.modifiers.contains(HlMod::DEFINITION))
            })
            .collect();

        assert!(
            var_highlights.len() >= 2,
            "Expected at least 2 variable highlights, got {}",
            var_highlights.len()
        );
    }

    #[test]
    fn test_highlight_implicit_local_variable() {
        let code = r#"
Процедура Тест()
    НаборЗаписей = 42;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let ranges: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable
                    && code[hl.range.start().into()..hl.range.end().into()] == *"НаборЗаписей"
            })
            .collect();

        assert_eq!(ranges.len(), 2, "implicit local should be highlighted at declaration and use");
        assert!(
            ranges.iter().any(|hl| hl.modifiers.contains(HlMod::DECLARATION)),
            "first assignment should be marked as declaration"
        );
    }

    #[test]
    fn test_highlight_parameter_vs_local_variable() {
        let code = r#"
Функция Тест(Параметр)
    Перем ЛокальнаяПеременная;
    ЛокальнаяПеременная = СокрЛП(Параметр);
    Возврат ЛокальнаяПеременная;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let param_count =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::Parameter).count();

        let local_var_count = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable
                    && (hl.modifiers.contains(HlMod::DECLARATION)
                        || !hl.modifiers.contains(HlMod::DEFINITION))
            })
            .count();

        assert!(param_count >= 2, "Expected at least 2 parameter highlights");
        assert!(local_var_count >= 3, "Expected at least 3 local variable highlights");
    }

    #[test]
    fn test_highlight_case_insensitive() {
        let code = r#"
Функция Тест(Параметр)
    Результат = параметр + ПАРАМЕТР;
    Возврат результат;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let param_count =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::Parameter).count();

        assert!(
            param_count >= 3,
            "Expected at least 3 parameter highlights (case-insensitive), got {}",
            param_count
        );
    }

    #[test]
    fn test_highlight_module_variable_vs_local() {
        let code = r#"
Перем МодульнаяПеременная;

Функция Тест()
    Перем ЛокальнаяПеременная;
    ЛокальнаяПеременная = МодульнаяПеременная;
    Возврат ЛокальнаяПеременная;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let var_highlights: Vec<_> =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::Variable).collect();

        assert!(
            var_highlights.len() >= 5,
            "Expected at least 5 variable highlights (module + local), got {}",
            var_highlights.len()
        );
    }

    #[test]
    fn test_highlight_multiple_methods() {
        let code = r#"
Функция Функция1(Параметр1)
    Перем Локальная1;
    Локальная1 = Параметр1;
    Возврат Локальная1;
КонецФункции

Функция Функция2(Параметр2)
    Перем Локальная2;
    Локальная2 = Параметр2;
    Возврат Локальная2;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let param_count =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::Parameter).count();

        let var_count = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable
                    && (hl.modifiers.contains(HlMod::DECLARATION)
                        || !hl.modifiers.contains(HlMod::DEFINITION))
            })
            .count();

        assert!(param_count >= 4, "Expected at least 4 parameter highlights");
        assert!(var_count >= 6, "Expected at least 6 local variable highlights");
    }

    #[test]
    fn test_sdbl_keyword_highlighting() {
        let code = r#"
Функция Тест()
    Запрос = "SELECT Код FROM Справочник.Валюты";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let select_kw = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Keyword
                && code[hl.range.start().into()..hl.range.end().into()] == *"SELECT"
        });

        assert!(select_kw.is_some(), "SELECT should be highlighted as Keyword");

        let from_kw = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Keyword
                && code[hl.range.start().into()..hl.range.end().into()] == *"FROM"
        });

        assert!(from_kw.is_some(), "FROM should be highlighted as Keyword");
    }

    #[test]
    fn test_sdbl_multiline_highlighting() {
        let code = r#"
Функция Тест()
    Запрос = "ВЫБРАТЬ
             |    Код
             |ИЗ
             |    Справочник.Валюты";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let select_kw = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Keyword && {
                let text = &code[hl.range.start().into()..hl.range.end().into()];
                text.contains("ВЫБРАТЬ")
            }
        });

        assert!(select_kw.is_some(), "ВЫБРАТЬ should be highlighted as Keyword");

        let from_kw = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Keyword && {
                let text = &code[hl.range.start().into()..hl.range.end().into()];
                text.contains("ИЗ")
            }
        });

        assert!(from_kw.is_some(), "ИЗ should be highlighted as Keyword");
    }

    #[test]
    fn test_sdbl_totals_by_only_hierarchy_highlighting() {
        let code = r#"
Функция Тест()
    Запрос = "ВЫБРАТЬ
             |    Группа КАК Группа
             |ИЗ
             |    Товары
             |ИТОГИ ПО
             |    Группа ТОЛЬКО ИЕРАРХИЯ";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        for keyword in ["ИТОГИ", "ПО", "ТОЛЬКО", "ИЕРАРХИЯ"] {
            let found = highlights.highlights.iter().any(|hl| {
                hl.tag == HlTag::Keyword
                    && &code[hl.range.start().into()..hl.range.end().into()] == keyword
            });

            assert!(found, "`{keyword}` should be highlighted as Keyword");
        }
    }

    #[test]
    fn test_sdbl_aggregate_functions() {
        let code = r#"
Функция Тест()
    Запрос = "SELECT SUM(Сумма) FROM Документ.Продажи";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let sum_fn = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Function
                && code[hl.range.start().into()..hl.range.end().into()] == *"SUM"
        });

        assert!(sum_fn.is_some(), "SUM should be highlighted as Function");
    }

    #[test]
    fn test_sdbl_operators() {
        let code = r#"
Функция Тест()
    Запрос = "SELECT * FROM Таблица WHERE А = Б AND В <> Г";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let eq_op = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Operator
                    && code[hl.range.start().into()..hl.range.end().into()] == *"="
            })
            .count();

        assert!(eq_op >= 1, "= should be highlighted as Operator");

        let and_op = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Operator
                && code[hl.range.start().into()..hl.range.end().into()] == *"AND"
        });

        assert!(and_op.is_some(), "AND should be highlighted as Operator");
    }

    #[test]
    fn test_no_sdbl_highlight_for_short_strings() {
        let code = r#"
Функция Тест()
    Строка = "SELECT";
    Возврат Строка;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let string_highlights: Vec<_> =
            highlights.highlights.iter().filter(|hl| hl.tag == HlTag::StringLiteral).collect();

        assert!(!string_highlights.is_empty(), "Short strings should remain as StringLiteral");
    }

    #[test]
    fn test_sdbl_as_keyword_highlighting() {
        let code = r#"
Функция Тест()
    Запрос = "ВЫБРАТЬ
             |    Очередь.ОтметкаВремени КАК ОтметкаВремени,
             |    Очередь.Попыток КАК Попыток
             |ИЗ
             |    РегистрСведений.ОчередьОбновленияКэширующихДанных КАК Очередь";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let as_keywords: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Keyword && {
                    let text = &code[hl.range.start().into()..hl.range.end().into()];
                    text.contains("КАК")
                }
            })
            .collect();

        assert_eq!(as_keywords.len(), 3, "Expected 3 КАК keywords, got {}", as_keywords.len());
    }

    #[test]
    fn test_sdbl_field_alias_highlighting() {
        let code = r#"
Функция Тест()
    Запрос = "ВЫБРАТЬ
             |    Валюты.Наименование КАК СимвольныйКод,
             |    Валюты.Код КАК КодВалюты
             |ИЗ
             |    Справочник.Валюты КАК Валюты";
    Возврат Запрос;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let field_aliases: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::EnumMember && {
                    let text = &code[hl.range.start().into()..hl.range.end().into()];
                    text == "СимвольныйКод" || text == "КодВалюты"
                }
            })
            .collect();

        assert_eq!(
            field_aliases.len(),
            2,
            "Expected 2 field aliases (СимвольныйКод, КодВалюты) highlighted as EnumMember, got {}",
            field_aliases.len()
        );

        let table_aliases: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Namespace && {
                    let text = &code[hl.range.start().into()..hl.range.end().into()];
                    text == "Валюты"
                }
            })
            .collect();

        assert!(
            !table_aliases.is_empty(),
            "Expected table alias 'Валюты' highlighted as Namespace"
        );

        let table_names: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Type && {
                    let text = &code[hl.range.start().into()..hl.range.end().into()];
                    text == "Справочник" || text == "Валюты"
                }
            })
            .collect();

        assert!(
            !table_names.is_empty(),
            "Expected table names (Справочник, Валюты) highlighted as Type"
        );
    }

    #[test]
    fn test_sdbl_user_example_highlighting() {
        let code = r#"
Функция Тест()
    ТекстЗапроса =
    "ВЫБРАТЬ
    |    Валюты.Наименование КАК СимвольныйКод
    |ИЗ
    |    Справочник.Валюты КАК Валюты";
    Возврат ТекстЗапроса;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        println!("\n=== Highlights for user example ===");
        for hl in &highlights.highlights {
            let text = &code[hl.range.start().into()..hl.range.end().into()];
            if text.contains("Валюты")
                || text.contains("Наименование")
                || text.contains("СимвольныйКод")
                || text.contains("Справочник")
            {
                println!("{:?}: '{}'", hl.tag, text);
            }
        }

        let mut has_type = false;
        let mut has_namespace = false;
        let mut has_enum_member = false;
        let mut has_property_or_unresolved = false;

        for hl in &highlights.highlights {
            let text = &code[hl.range.start().into()..hl.range.end().into()];
            match hl.tag {
                HlTag::Type if text.contains("Справочник") || text.contains("Валюты") =>
                {
                    has_type = true;
                }
                HlTag::Namespace if text == "Валюты" => {
                    has_namespace = true;
                }
                HlTag::EnumMember if text == "СимвольныйКод" => {
                    has_enum_member = true;
                }
                HlTag::Property | HlTag::UnresolvedReference if text == "Наименование" =>
                {
                    has_property_or_unresolved = true;
                }
                _ => {}
            }
        }

        assert!(has_type, "Table names should be highlighted as Type");
        assert!(has_namespace, "Table alias should be highlighted as Namespace");
        assert!(has_enum_member, "Field alias should be highlighted as EnumMember");
        assert!(
            has_property_or_unresolved,
            "Field name should be highlighted as Property or UnresolvedReference"
        );
    }

    #[test]
    fn test_sdbl_as_keyword_english() {
        let code = r#"
Функция Тест()
    Query = "SELECT Name AS ProductName FROM Products AS P";
    Возврат Query;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let as_keywords: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Keyword
                    && code[hl.range.start().into()..hl.range.end().into()] == *"AS"
            })
            .collect();

        assert_eq!(as_keywords.len(), 2, "Expected 2 AS keywords, got {}", as_keywords.len());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtin_function_highlighting() {
        let code = r#"
Функция Тест()
    НачатьТранзакцию();
    Сообщить("Привет");
    Результат = Формат(123, "ЧГ=0");
    Возврат Результат;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        eprintln!("\n=== All highlights ===");
        for hl in highlights.highlights.iter() {
            let text = &code[hl.range.start().into()..hl.range.end().into()];
            if !text.trim().is_empty() && text.chars().all(|c| c.is_alphabetic() || c == '_') {
                eprintln!("{:?}: {:?} {:?}", text, hl.tag, hl.modifiers);
            }
        }
        eprintln!("======================\n");

        let begin_trans = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::BuiltinFunction
                && code[hl.range.start().into()..hl.range.end().into()] == *"НачатьТранзакцию"
        });

        assert!(begin_trans.is_some(), "НачатьТранзакцию should be highlighted as BuiltinFunction");
        assert!(
            begin_trans.unwrap().modifiers.contains(HlMod::EXPORT),
            "BuiltinFunction should have EXPORT modifier (defaultLibrary)"
        );

        let message_fn = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::BuiltinFunction
                && code[hl.range.start().into()..hl.range.end().into()] == *"Сообщить"
        });

        assert!(message_fn.is_some(), "Сообщить should be highlighted as BuiltinFunction");

        let format_fn = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::BuiltinFunction
                && code[hl.range.start().into()..hl.range.end().into()] == *"Формат"
        });

        assert!(format_fn.is_some(), "Формат should be highlighted as BuiltinFunction");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtin_vs_user_function() {
        let code = r#"
Функция МояФункция()
    Возврат 42;
КонецФункции

Функция Тест()
    // User-defined function call - should be Function (not BuiltinFunction)
    Результат1 = МояФункция();

    // Built-in platform function - should be BuiltinFunction
    Результат2 = НачатьТранзакцию();

    Возврат Результат1 + Результат2;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let my_func_call = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Function
                && code[hl.range.start().into()..hl.range.end().into()] == *"МояФункция"
                && !hl.modifiers.contains(HlMod::EXPORT)
        });

        assert!(
            my_func_call.is_some(),
            "МояФункция should be highlighted as Function (not builtin)"
        );

        let builtin_call = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::BuiltinFunction
                && code[hl.range.start().into()..hl.range.end().into()] == *"НачатьТранзакцию"
        });

        assert!(
            builtin_call.is_some(),
            "НачатьТранзакцию should be highlighted as BuiltinFunction"
        );
    }

    #[test]
    fn test_highlight_mdo_plural_forms_russian() {
        let code = r#"
Функция ПолучитьДокумент()
    Ссылка = Документы.ПКО.НайтиПоНомеру("001");
    Возврат Ссылка;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let documents_highlight = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Class
                && code[hl.range.start().into()..hl.range.end().into()] == *"Документы"
        });

        assert!(documents_highlight.is_some(), "Документы should be highlighted as Class");
    }

    #[test]
    fn test_highlight_mdo_plural_forms_english() {
        let code = r#"
Function GetCatalog()
    Ref = Catalogs.Currencies.FindByCode("USD");
    Return Ref;
EndFunction
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let catalogs_highlight = highlights.highlights.iter().find(|hl| {
            hl.tag == HlTag::Class
                && code[hl.range.start().into()..hl.range.end().into()] == *"Catalogs"
        });

        assert!(catalogs_highlight.is_some(), "Catalogs should be highlighted as Class");
    }

    #[test]
    fn test_highlight_mdo_plural_case_insensitive() {
        let code = r#"
Функция Тест()
    Результат1 = ДОКУМЕНТЫ.ПКО.Создать();
    Результат2 = документы.ПКО.Создать();
    Результат3 = Документы.ПКО.Создать();
    Возврат Результат1;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let documents_highlights: Vec<_> = highlights
            .highlights
            .iter()
            .filter(|hl| {
                if hl.tag != HlTag::Class {
                    return false;
                }
                let text = &code[hl.range.start().into()..hl.range.end().into()];
                text == "ДОКУМЕНТЫ" || text == "документы" || text == "Документы"
            })
            .collect();

        assert_eq!(
            documents_highlights.len(),
            3,
            "Expected 3 'Документы' highlights as Class (case-insensitive)"
        );
    }

    #[test]
    fn test_highlight_all_mdo_plural_forms() {
        let code = r#"
Функция ТестВсехТипов()
    А = Документы.ПКО.Создать();
    Б = Справочники.Валюты.НайтиПоКоду("USD");
    В = РегистрыСведений.КурсыВалют.СоздатьНаборЗаписей();
    Г = РегистрыНакопления.Продажи.СоздатьНаборЗаписей();
    Д = Перечисления.ВидыДвижений.Приход;
    Е = Обработки.ЗагрузкаДанных.Создать();
    Ж = Отчеты.ОстаткиТоваров.Создать();
    Возврат А;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let expected_classes = vec![
            "Документы",
            "Справочники",
            "РегистрыСведений",
            "РегистрыНакопления",
            "Перечисления",
            "Обработки",
            "Отчеты",
        ];

        for expected in expected_classes {
            let found = highlights.highlights.iter().any(|hl| {
                hl.tag == HlTag::Class
                    && code[hl.range.start().into()..hl.range.end().into()] == *expected
            });

            assert!(found, "{} should be highlighted as Class (MDO plural)", expected);
        }
    }

    #[test]
    fn test_mdo_plural_shadowed_by_local_variable() {
        let code = r#"
Функция Тест()
    Перем Документы;
    Документы = 42;
    Возврат Документы;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let documents_as_variable = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable && {
                    let text = &code[hl.range.start().into()..hl.range.end().into()];
                    text == "Документы"
                }
            })
            .count();

        assert!(
            documents_as_variable >= 3,
            "Local variable 'Документы' should shadow global MDO plural (expected >=3 Variable highlights)"
        );

        let documents_as_class = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Class && {
                let text = &code[hl.range.start().into()..hl.range.end().into()];
                text == "Документы"
            }
        });

        assert!(
            !documents_as_class,
            "Local variable should prevent 'Документы' from being highlighted as Class"
        );
    }

    #[test]
    #[ignore = "Track 3 Phase G dep: needs Definition/PathResolution support for config-backed MDO object names"]
    fn test_highlight_metadata_object_name_with_config() {
        let code = r#"
Функция Тест()
    Ссылка = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Возврат Ссылка;
КонецФункции
"#;

        let (mut db, file_id) = create_db_with_file(code);
        db.set_all_config_paths(vec![(None, designer_fixture_path())]);
        let highlights = highlight(&db, file_id);

        let plural_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Class
                && code[hl.range.start().into()..hl.range.end().into()] == *"РегистрыСведений"
        });
        assert!(plural_highlight, "РегистрыСведений should be highlighted as Class");

        let metadata_name_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Type
                && code[hl.range.start().into()..hl.range.end().into()] == *"РегистрСведений1"
        });
        assert!(
            metadata_name_highlight,
            "РегистрСведений1 should be highlighted as Type with configuration loaded"
        );
    }

    #[test]
    fn test_highlight_metadata_object_without_config() {
        let code = r#"
Функция Тест()
    Ссылка = Документы.ПКО.НайтиПоНомеру("001");
    Возврат Ссылка;
КонецФункции
"#;

        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let metadata_name_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Type && code[hl.range.start().into()..hl.range.end().into()] == *"ПКО"
        });

        assert!(
            !metadata_name_highlight,
            "ПКО should not be highlighted as Type (no configuration loaded)"
        );

        let plural_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Class
                && code[hl.range.start().into()..hl.range.end().into()] == *"Документы"
        });

        assert!(plural_highlight, "Документы should still be highlighted as Class");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_highlight_record_set_chain_uses_inferred_receiver_types() {
        let code = r#"
Процедура Тест(Значение)
    НаборЗаписей = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НаборЗаписей.Отбор.Справочник1.Установить(Значение);
    НаборЗаписей.Загрузить(Новый ТаблицаЗначений);
    НаборЗаписей.Записать();
КонецПроцедуры
"#;

        let (mut db, file_id) = create_db_with_file(code);
        db.set_all_config_paths(vec![(None, designer_fixture_path())]);
        let highlights = highlight(&db, file_id);

        let local_count = highlights
            .highlights
            .iter()
            .filter(|hl| {
                hl.tag == HlTag::Variable
                    && code[hl.range.start().into()..hl.range.end().into()] == *"НаборЗаписей"
            })
            .count();
        assert!(local_count >= 4, "НаборЗаписей should be an inferred implicit local");

        for expected in ["Отбор", "Справочник1"] {
            let found = highlights.highlights.iter().any(|hl| {
                hl.tag == HlTag::Property
                    && code[hl.range.start().into()..hl.range.end().into()] == *expected
            });
            assert!(found, "{expected} should be highlighted as typed property");
        }

        for expected in ["Установить", "Загрузить", "Записать"] {
            let found = highlights.highlights.iter().any(|hl| {
                hl.tag == HlTag::Function
                    && code[hl.range.start().into()..hl.range.end().into()] == *expected
            });
            assert!(found, "{expected} should be highlighted as typed method");
        }
    }

    #[test]
    #[ignore = "Track 3 Phase G dep: requires CfeFixtureBuilder (§8 CFE harness)"]
    fn test_highlight_manager_module_method() {
        let code = r#"
Процедура Тест()
    РегистрыСведений.ОчередьЗапросовERP.ДобавитьВОчередь();
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(code);
        let highlights = highlight(&db, file_id);

        let plural_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Class
                && code[hl.range.start().into()..hl.range.end().into()] == *"РегистрыСведений"
        });
        assert!(plural_highlight, "РегистрыСведений should be highlighted as Class");

        let method_highlight = highlights.highlights.iter().any(|hl| {
            hl.tag == HlTag::Function
                && code[hl.range.start().into()..hl.range.end().into()] == *"ДобавитьВОчередь"
        });
        assert!(
            method_highlight,
            "ДобавитьВОчередь should be highlighted as Function (manager method)"
        );
    }

    #[test]
    fn test_bonus_query_highlighting() {
        let code = r#"Процедура Тест()
    Запрос = "ВЫБРАТЬ
    |    НачислениеИСписаниеБонусныхБаллов.Ссылка КАК Ссылка
    |ИЗ
    |    Документ.НачислениеИСписаниеБонусныхБаллов КАК НачислениеИСписаниеБонусныхБаллов
    |ГДЕ
    |    НачислениеИСписаниеБонусныхБаллов.ПричинаНачисленияИСписанияБонусныхБаллов = ЗНАЧЕНИЕ(Справочник.ПричиныНачисленияИСписанияБонусныхБаллов.БонусЗаПодтверждениеЭлектроннойПочты)
    |    И НачислениеИСписаниеБонусныхБаллов.Начисление.Партнер = &Партнер
    |    И НачислениеИСписаниеБонусныхБаллов.Проведен
    |    И НЕ НачислениеИСписаниеБонусныхБаллов.ПометкаУдаления";
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(code);

        let highlights = highlight(&db, file_id);

        eprintln!("\n=== Bonus Query Test ===");
        eprintln!("Total highlights: {}", highlights.highlights.len());

        eprintln!("\n=== All highlights ===");
        for hl in &highlights.highlights {
            let text = &code[hl.range.start().into()..hl.range.end().into()];
            eprintln!("Range {:?}, Tag {:?}, Text: '{}'", hl.range, hl.tag, text);
        }

        let tokens = ["ЗНАЧЕНИЕ", "Партнер", "Проведен", "ПометкаУдаления", "НЕ"];
        for token in tokens {
            let found = highlights.highlights.iter().any(|hl| {
                let text = &code[hl.range.start().into()..hl.range.end().into()];
                text == token
            });
            eprintln!("{}: {}", token, if found { "✓" } else { "✗" });
        }

        let last_keyword_pos = highlights
            .highlights
            .iter()
            .filter(|hl| hl.tag == HlTag::Keyword)
            .map(|hl| hl.range.end())
            .max();

        if let Some(pos) = last_keyword_pos {
            let pos_usize: usize = pos.into();
            eprintln!("\nLast keyword ends at: {}", pos_usize);
            if pos_usize < code.len() {
                let remaining = &code[pos_usize..];
                eprintln!(
                    "Remaining text (first 100 chars): {:?}",
                    &remaining[..remaining.len().min(100)]
                );
            }
        }

        let sdbl_hirs = db.sdbl_hir_in_file(file_id);
        let sdbl_queries = db.all_sdbl_in_file(file_id);
        eprintln!("\nSDBL HIR entries: {}", sdbl_hirs.len());
        eprintln!("SDBL queries: {}", sdbl_queries.len());

        for ((_expr_id, sdbl_pkg), (_query_id, query_info)) in
            sdbl_hirs.iter().zip(sdbl_queries.iter())
        {
            eprintln!("\n=== SDBL Query Text ===");
            eprintln!("{}", query_info.query_text);

            eprintln!("\n=== Source Map All Tokens ===");
            let all_tokens: Vec<_> = sdbl_pkg.source_map.all_tokens().collect();
            eprintln!("Total tokens in source map: {}", all_tokens.len());
            for (token, category) in all_tokens.iter().take(50) {
                eprintln!("  {:?} at {:?}: '{}'", category, token.range, token.text);
            }
            if all_tokens.len() > 50 {
                eprintln!("  ... and {} more tokens", all_tokens.len() - 50);
            }

            eprintln!("\n=== Quote Corrections ===");
            eprintln!("Total: {}", query_info.quote_corrections.len());
            for (pos, chars) in &query_info.quote_corrections {
                eprintln!("  At offset {}: +{} chars", pos, chars);
            }

            if let Some(ref ast) = query_info.query_ast {
                if ast.has_errors() {
                    eprintln!("\n=== SDBL Parser Errors ===");
                    for err in ast.errors() {
                        eprintln!("  - {:?}", err);
                    }
                } else {
                    eprintln!("\n✓ SDBL parsed successfully, no errors");
                }
            } else {
                eprintln!("\n✗ SDBL AST is None");
            }
        }
    }

    fn highlights_for(highlights: &[HlRange], code: &str, needle: &str) -> Vec<HlRange> {
        highlights
            .iter()
            .filter(|hl| {
                let s: usize = hl.range.start().into();
                let e: usize = hl.range.end().into();
                &code[s..e] == needle
            })
            .cloned()
            .collect()
    }

    fn assert_sorted_non_overlapping(highlights: &[HlRange]) {
        for window in highlights.windows(2) {
            assert!(
                window[0].range.start() <= window[1].range.start(),
                "highlights must be sorted by start; got {:?} then {:?}",
                window[0],
                window[1]
            );
            assert!(
                window[0].range.end() <= window[1].range.start(),
                "highlights must be pairwise non-overlapping; got {:?} overlapping with {:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_highlight_procedure_def_single_definition() {
        let code = "Процедура Тест()\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "Тест");
        assert_eq!(name_hls.len(), 1, "expected exactly one HlRange on procedure name");
        let hl = &name_hls[0];
        assert_eq!(hl.tag, HlTag::Procedure);
        assert!(hl.modifiers.contains(HlMod::DEFINITION));
        assert!(!hl.modifiers.contains(HlMod::EXPORT));
    }

    #[test]
    fn test_highlight_function_export_definition_export() {
        let code = "Функция Тест() Экспорт\n    Возврат 1;\nКонецФункции\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "Тест");
        assert_eq!(name_hls.len(), 1);
        assert_eq!(name_hls[0].tag, HlTag::Function);
        assert!(name_hls[0].modifiers.contains(HlMod::DEFINITION));
        assert!(name_hls[0].modifiers.contains(HlMod::EXPORT));
    }

    #[test]
    fn test_highlight_param_def_emits_declaration() {
        let code = "Процедура Х(А, Б)\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        for needle in ["А", "Б"] {
            let hls = highlights_for(&result.highlights, code, needle);
            assert_eq!(hls.len(), 1, "param {needle}: expected one HlRange, got {hls:?}");
            assert_eq!(hls[0].tag, HlTag::Parameter);
            assert!(hls[0].modifiers.contains(HlMod::DECLARATION));
        }
    }

    #[test]
    fn test_highlight_multi_var_decl_module_export() {
        let code = "Перем A, B Экспорт;\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        for needle in ["A", "B"] {
            let hls = highlights_for(&result.highlights, code, needle);
            assert_eq!(hls.len(), 1);
            assert_eq!(hls[0].tag, HlTag::Variable);
            assert!(hls[0].modifiers.contains(HlMod::DECLARATION));
            assert!(
                hls[0].modifiers.contains(HlMod::EXPORT),
                "module-level VarDef with `Экспорт` keyword must propagate EXPORT to every name"
            );
        }
    }

    #[test]
    fn test_highlight_local_var_decl_no_export() {
        let code = "Процедура Х()\n    Перем Л1, Л2;\n    Л1 = 1;\n    Л2 = Л1;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let l1_decl = highlights_for(&result.highlights, code, "Л1")
            .into_iter()
            .find(|hl| hl.modifiers.contains(HlMod::DECLARATION))
            .expect("expected DECLARATION on Л1");
        assert_eq!(l1_decl.tag, HlTag::Variable);
        assert!(!l1_decl.modifiers.contains(HlMod::EXPORT));
    }

    #[test]
    fn test_highlight_def_site_unresolved_falls_back_to_ast() {
        let code = "Процедура Тест(\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "Тест");
        assert_eq!(name_hls.len(), 1, "AST classifier must still highlight name on broken code");
        assert_eq!(name_hls[0].tag, HlTag::Procedure);
        assert!(name_hls[0].modifiers.contains(HlMod::DEFINITION));
    }

    #[test]
    fn test_highlight_annotated_procedure_no_overlap() {
        let code = "&НаКлиенте\nПроцедура Тест()\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "Тест");
        assert_eq!(name_hls.len(), 1);
        assert_eq!(name_hls[0].tag, HlTag::Procedure);
        assert!(name_hls[0].modifiers.contains(HlMod::DEFINITION));
    }

    #[test]
    fn test_highlight_no_overlap_corpus() {
        let code = r#"
Перем МодульнаяПеременная Экспорт;

&НаКлиенте
Процедура Обработать(Параметр1, Параметр2 = 0)
    Перем Локальная;
    Локальная = Параметр1 + Параметр2 + МодульнаяПеременная;
    Запрос = "ВЫБРАТЬ Код ИЗ Справочник.Валюты";
    Возврат Локальная;
КонецПроцедуры

Функция Тест() Экспорт
    Возврат 42;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        for needle in
            ["Обработать", "Тест", "МодульнаяПеременная", "Локальная", "Параметр1", "Параметр2"]
        {
            let hls = highlights_for(&result.highlights, code, needle);
            let decl_hls: Vec<_> = hls
                .iter()
                .filter(|hl| {
                    hl.modifiers.contains(HlMod::DEFINITION)
                        || hl.modifiers.contains(HlMod::DECLARATION)
                })
                .collect();
            assert_eq!(
                decl_hls.len(),
                1,
                "{needle}: expected exactly one declaration highlight, got {decl_hls:?}"
            );
        }
    }

    #[test]
    fn test_highlight_async_procedure_no_overlap() {
        let code = "Асинх Процедура Х()\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "Х");
        assert_eq!(name_hls.len(), 1, "expected one HlRange on async proc name");
        assert_eq!(name_hls[0].tag, HlTag::Procedure);
        assert!(name_hls[0].modifiers.contains(HlMod::DEFINITION));
    }

    #[test]
    fn test_highlight_extension_annotation_no_overlap() {
        let code = "&Перед(\"ОригинальныйМетод\")\nПроцедура НовыйМетод()\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(code);
        let result = highlight(&db, file_id);
        assert_sorted_non_overlapping(&result.highlights);

        let name_hls = highlights_for(&result.highlights, code, "НовыйМетод");
        assert_eq!(name_hls.len(), 1);
        assert_eq!(name_hls[0].tag, HlTag::Procedure);
        assert!(name_hls[0].modifiers.contains(HlMod::DEFINITION));
    }

    #[test]
    fn test_normalize_highlights_dedupes_exact_overlap() {
        let r = TextRange::new(0.into(), 4.into());
        let input = vec![
            HlRange { range: r, tag: HlTag::Function, modifiers: HlMod::new() },
            HlRange {
                range: r,
                tag: HlTag::Procedure,
                modifiers: HlMod::new().with(HlMod::DEFINITION),
            },
        ];
        let out = normalize_highlights(input);
        assert_eq!(out.len(), 1);
        assert!(out[0].modifiers.contains(HlMod::DEFINITION));
        assert_eq!(out[0].tag, HlTag::Procedure);
    }

    /// Инструкции расширений подсвечиваются наравне с остальными.
    ///
    /// Эти четыре инструкции законны только в расширениях конфигурации,
    /// поэтому в основных конфигурациях их отсутствие в подсветке никак не
    /// проявлялось. Обе языковые формы взяты потому, что подсветка идёт от
    /// вида лексемы, а вид у русского и английского написания один.
    #[test]
    fn test_highlight_extension_directives() {
        for code in [
            "#Вставка\nПроцедура П() КонецПроцедуры\n#КонецВставки\n#Удаление\nПроцедура У() КонецПроцедуры\n#КонецУдаления\n",
            "#Insert\nПроцедура П() КонецПроцедуры\n#EndInsert\n#Delete\nПроцедура У() КонецПроцедуры\n#EndDelete\n",
        ] {
            let (db, file_id) = create_db_with_file(code);
            let highlights = highlight(&db, file_id);

            let directives: Vec<&str> = highlights
                .highlights
                .iter()
                .filter(|hl| hl.tag == HlTag::Preprocessor)
                .map(|hl| {
                    let start: usize = hl.range.start().into();
                    let end: usize = hl.range.end().into();
                    &code[start..end]
                })
                .collect();

            let expected: Vec<&str> =
                code.lines().filter(|line| line.starts_with('#')).collect();
            assert_eq!(directives, expected, "не все инструкции получили подсветку");
        }
    }

    #[test]
    fn test_normalize_highlights_drops_partial_overlap() {
        let outer = TextRange::new(0.into(), 6.into());
        let inner = TextRange::new(3.into(), 9.into());
        let input = vec![
            HlRange { range: outer, tag: HlTag::Variable, modifiers: HlMod::new() },
            HlRange { range: inner, tag: HlTag::Function, modifiers: HlMod::new() },
        ];
        let out = normalize_highlights(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].range, outer);
    }

    #[test]
    fn test_normalize_highlights_keeps_disjoint() {
        let a = TextRange::new(0.into(), 3.into());
        let b = TextRange::new(4.into(), 7.into());
        let input = vec![
            HlRange { range: a, tag: HlTag::Variable, modifiers: HlMod::new() },
            HlRange { range: b, tag: HlTag::Function, modifiers: HlMod::new() },
        ];
        let out = normalize_highlights(input);
        assert_eq!(out.len(), 2);
    }
}
