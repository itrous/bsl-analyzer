use hir::Semantics;
use ide_db::RootDatabase;
use syntax::{NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};
use vfs::FileId;

use crate::reference_kind::{classify_reference_token, ReferenceKind};
use crate::references;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentHighlight {
    pub range: TextRange,
    pub kind: DocumentHighlightKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentHighlightKind {
    Text,
    Read,
    Write,
}

/// Highlights for one file.
///
/// Answered without the reference-scope gate on purpose: this consumer is read-only and
/// local, while the gate protects walks that leave the file and feeds rename. A typed member
/// carries a real identity — its receiver and its field name — so the ungated walk lights
/// every read of the member instead of the occurrence alone.
///
/// On a structural keyword (`Если`, `Цикл`, `Попытка`, `Процедура`, `#Если`, ...) or a region
/// directive the answer is the keywords of that one construct, the way an editor matches a
/// bracket: there is no symbol under the cursor to highlight there.
pub fn document_highlights<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Vec<DocumentHighlight> {
    let _span = tracing::info_span!("document_highlights", ?file_id).entered();

    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    if let Some(marker) = root.token_at_offset(offset).find(|t| is_region_marker(t.kind())) {
        return region_pair_highlights(&root, &marker);
    }

    if let Some(construct) = root.token_at_offset(offset).find_map(|t| keyword_construct(&t)) {
        return construct_keyword_highlights(&construct);
    }

    let token = match root.token_at_offset(offset).right_biased() {
        Some(t) if t.kind().is_name_token() => t,
        _ => return Vec::new(),
    };

    let sema = Semantics::new(db);
    let symbol = match sema.symbol_for_token(file_id, &token) {
        Some(symbol) => symbol,
        None => return Vec::new(),
    };

    references::find_references_in_file(db, file_id, &symbol)
        .into_iter()
        .filter_map(|loc| {
            let token = token_for_range(&root, loc.range)?;
            let kind = classify_highlight_token(&token);
            Some(DocumentHighlight { range: loc.range, kind })
        })
        .collect()
}

/// The structural keywords of a block construct; the last one closes it.
fn construct_keywords(kind: SyntaxKind) -> &'static [SyntaxKind] {
    use SyntaxKind::*;
    match kind {
        PROCEDURE_DEF => &[KW_PROCEDURE, KW_END_PROCEDURE],
        FUNCTION_DEF => &[KW_FUNCTION, KW_END_FUNCTION],
        IF_STMT => &[KW_IF, KW_THEN, KW_ELSIF, KW_ELSE, KW_END_IF],
        WHILE_STMT => &[KW_WHILE, KW_DO, KW_END_DO],
        FOR_STMT => &[KW_FOR, KW_DO, KW_END_DO],
        FOR_EACH_STMT => &[KW_FOR, KW_EACH, KW_DO, KW_END_DO],
        TRY_STMT => &[KW_TRY, KW_EXCEPT, KW_END_TRY],
        PRE_IF_DIR => &[PRE_IF, KW_THEN, PRE_ELSIF, PRE_ELSE, PRE_END_IF],
        _ => &[],
    }
}

/// Branch nodes whose leading keyword belongs to the enclosing construct.
fn is_construct_clause(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::ELSIF_CLAUSE
            | SyntaxKind::ELSE_CLAUSE
            | SyntaxKind::EXCEPT_CLAUSE
            | SyntaxKind::PRE_ELSIF_CLAUSE
            | SyntaxKind::PRE_ELSE_CLAUSE
    )
}

/// The construct a keyword token opens, divides or closes.
///
/// Only a keyword standing directly in the construct or in one of its branch nodes counts:
/// the same word elsewhere — inside an expression, in a recovered error node — is not
/// structural, and the cursor there keeps the ordinary symbol highlight.
fn keyword_construct(token: &SyntaxToken) -> Option<SyntaxNode> {
    let parent = token.parent()?;
    let construct = if is_construct_clause(parent.kind()) { parent.parent()? } else { parent };
    construct_keywords(construct.kind()).contains(&token.kind()).then_some(construct)
}

/// Every structural keyword of `construct`. Nested constructs own their keywords and are
/// not entered: the keywords sit only in the construct node itself and its branch nodes.
///
/// A construct the parser left without its closing keyword answers nothing: half a pair
/// would point at a block end that is not there.
fn construct_keyword_highlights(construct: &SyntaxNode) -> Vec<DocumentHighlight> {
    let keywords = construct_keywords(construct.kind());
    let Some(&closing) = keywords.last() else {
        return Vec::new();
    };

    let tokens: Vec<SyntaxToken> = construct
        .children_with_tokens()
        .flat_map(|element| match element {
            NodeOrToken::Token(token) => vec![token],
            NodeOrToken::Node(node) if is_construct_clause(node.kind()) => {
                node.children_with_tokens().filter_map(|it| it.into_token()).collect()
            }
            NodeOrToken::Node(_) => Vec::new(),
        })
        .filter(|token| keywords.contains(&token.kind()))
        .collect();

    if !tokens.iter().any(|token| token.kind() == closing) {
        return Vec::new();
    }

    tokens
        .into_iter()
        .map(|token| DocumentHighlight {
            range: token.text_range(),
            kind: DocumentHighlightKind::Text,
        })
        .collect()
}

fn is_region_marker(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::PRE_REGION | SyntaxKind::PRE_END_REGION)
}

/// A region directive and its balanced partner.
///
/// Region directives are flat markers, not containers, so the pairs are rebuilt in source
/// order with a stack, the way the region tree builds them. A marker left without a partner
/// answers nothing.
fn region_pair_highlights(root: &SyntaxNode, marker: &SyntaxToken) -> Vec<DocumentHighlight> {
    let mut open: Vec<SyntaxToken> = Vec::new();
    let mut partner = None;
    for token in root
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|token| is_region_marker(token.kind()))
    {
        if token.kind() == SyntaxKind::PRE_REGION {
            open.push(token);
            continue;
        }
        let Some(start) = open.pop() else { continue };
        if &start == marker {
            partner = Some(token);
            break;
        }
        if &token == marker {
            partner = Some(start);
            break;
        }
    }

    let Some(partner) = partner else {
        return Vec::new();
    };
    let mut pair = [marker.text_range(), partner.text_range()];
    pair.sort_by_key(|range| range.start());
    pair.into_iter()
        .map(|range| DocumentHighlight { range, kind: DocumentHighlightKind::Text })
        .collect()
}

fn token_for_range(root: &SyntaxNode, range: TextRange) -> Option<SyntaxToken> {
    root.descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .find(|token| token.text_range() == range)
}

/// The LSP vocabulary has no slot for a call, so a call is a read — exactly what
/// the classifier answered before it learned to name calls apart.
fn classify_highlight_token(token: &SyntaxToken) -> DocumentHighlightKind {
    match classify_reference_token(token) {
        ReferenceKind::Declaration => DocumentHighlightKind::Text,
        ReferenceKind::Write => DocumentHighlightKind::Write,
        ReferenceKind::Call | ReferenceKind::Read => DocumentHighlightKind::Read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, VfsPath};

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

    fn highlight_kinds(
        db: &RootDatabaseImpl,
        file_id: FileId,
        source: &str,
        name: &str,
    ) -> Vec<(String, DocumentHighlightKind)> {
        let offset = TextSize::from(source.find(name).unwrap() as u32);
        let mut highlights = document_highlights(db, file_id, offset);
        highlights.sort_by_key(|highlight| highlight.range.start());

        highlights
            .into_iter()
            .map(|highlight| {
                let start: u32 = highlight.range.start().into();
                let end: u32 = highlight.range.end().into();
                (source[start as usize..end as usize].to_string(), highlight.kind)
            })
            .collect()
    }

    fn classify_first_token(
        db: &RootDatabaseImpl,
        file_id: FileId,
        text: &str,
    ) -> DocumentHighlightKind {
        let root = db.parse(file_id).syntax_node();
        let token = root
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|token| token.text() == text)
            .unwrap();

        classify_highlight_token(&token)
    }

    #[test]
    fn local_variable_highlights_text_write_and_read() {
        let source = r#"
Процедура Тест()
    Перем МояПеременная;

    МояПеременная = 10;
    Сообщить(МояПеременная);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let kinds = highlight_kinds(&db, file_id, source, "МояПеременная");

        assert_eq!(
            kinds,
            vec![
                ("МояПеременная".to_string(), DocumentHighlightKind::Text),
                ("МояПеременная".to_string(), DocumentHighlightKind::Write),
                ("МояПеременная".to_string(), DocumentHighlightKind::Read),
            ]
        );
    }

    #[test]
    fn parameter_highlights_text_write_and_read() {
        let source = r#"
Процедура Тест(МойПараметр)
    МойПараметр = 10;
    Сообщить(МойПараметр);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let kinds = highlight_kinds(&db, file_id, source, "МойПараметр");

        assert_eq!(
            kinds,
            vec![
                ("МойПараметр".to_string(), DocumentHighlightKind::Text),
                ("МойПараметр".to_string(), DocumentHighlightKind::Write),
                ("МойПараметр".to_string(), DocumentHighlightKind::Read),
            ]
        );
    }

    #[test]
    fn implicit_local_highlights_write_and_read() {
        let source = r#"
Процедура Тест()
    НаборЗаписей = 10;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let kinds = highlight_kinds(&db, file_id, source, "НаборЗаписей");

        assert_eq!(
            kinds,
            vec![
                ("НаборЗаписей".to_string(), DocumentHighlightKind::Write),
                ("НаборЗаписей".to_string(), DocumentHighlightKind::Read),
            ]
        );
    }

    /// Highlighting a variable is its reference walk restricted to one file, so an
    /// identity split into slices highlights only the slice the cursor stands in.
    /// Every occurrence must light up from every occurrence, writes and reads alike.
    #[test]
    fn implicit_local_highlights_span_every_assignment() {
        let source = r#"
Процедура Тест()
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = 2;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let expected = vec![
            ("НаборЗаписей".to_string(), DocumentHighlightKind::Write),
            ("НаборЗаписей".to_string(), DocumentHighlightKind::Read),
            ("НаборЗаписей".to_string(), DocumentHighlightKind::Write),
            ("НаборЗаписей".to_string(), DocumentHighlightKind::Read),
        ];

        let mut from = 0usize;
        while let Some(at) = source[from..].find("НаборЗаписей") {
            let at = from + at;
            from = at + "НаборЗаписей".len();

            let mut highlights = document_highlights(&db, file_id, TextSize::from(at as u32));
            highlights.sort_by_key(|highlight| highlight.range.start());
            let seen: Vec<(String, DocumentHighlightKind)> = highlights
                .into_iter()
                .map(|highlight| {
                    let start: u32 = highlight.range.start().into();
                    let end: u32 = highlight.range.end().into();
                    (source[start as usize..end as usize].to_string(), highlight.kind)
                })
                .collect();

            assert_eq!(seen, expected, "the cursor at {at} lit only its own slice");
        }
    }

    #[test]
    fn indexed_assignment_does_not_mark_index_operands_as_write() {
        let source = r#"
Процедура Тест()
    Перем Массив, Индекс;

    Массив[Индекс] = 10;
    Сообщить(Индекс);
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let kinds = highlight_kinds(&db, file_id, source, "Индекс");

        assert_eq!(
            kinds,
            vec![
                ("Индекс".to_string(), DocumentHighlightKind::Text),
                ("Индекс".to_string(), DocumentHighlightKind::Read),
                ("Индекс".to_string(), DocumentHighlightKind::Read),
            ]
        );
    }

    #[test]
    fn field_assignment_marks_tail_as_write_and_receiver_as_read() {
        let source = r#"
Процедура Тест()
    Объект.Поле = 10;
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        assert_eq!(classify_first_token(&db, file_id, "Объект"), DocumentHighlightKind::Read);
        assert_eq!(classify_first_token(&db, file_id, "Поле"), DocumentHighlightKind::Write);
    }

    #[test]
    fn call_projects_onto_read() {
        let source = r#"
Процедура Тест()
    Помощник();
КонецПроцедуры

Процедура Помощник()
КонецПроцедуры
"#;
        let (db, file_id) = create_db_with_file(source);

        let root = db.parse(file_id).syntax_node();
        let call_offset = TextSize::from(source.find("Помощник();").unwrap() as u32);
        let call_token = root.token_at_offset(call_offset).right_biased().unwrap();
        assert_eq!(classify_reference_token(&call_token), ReferenceKind::Call);
        assert_eq!(classify_highlight_token(&call_token), DocumentHighlightKind::Read);
    }

    /// A member of a local reached through a typed receiver is identified by the RECEIVER it
    /// is read from and the field name. Two readings of one field are one member; a highlight
    /// that trusted the occurrence's own range would light only the one under the cursor.
    #[test]
    fn typed_member_highlights_every_read_of_one_receiver() {
        let source = "Процедура Тест() Экспорт\n    С = Новый Структура(\"Поле\", 1);\n    Х = С.Поле;\n    У = С.Поле;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let at = source.find("С.Поле").unwrap() + "С.".len();
        let mut highlights = document_highlights(&db, file_id, TextSize::from(at as u32));
        highlights.sort_by_key(|highlight| highlight.range.start());

        let lines: Vec<usize> = highlights
            .iter()
            .map(|highlight| source[..highlight.range.start().into()].matches('\n').count())
            .collect();
        assert_eq!(lines, vec![2, 3], "only the cursor's own read lit: {highlights:?}");
        assert!(
            highlights.iter().all(|highlight| highlight.kind == DocumentHighlightKind::Read),
            "{highlights:?}"
        );
    }

    /// Gate — the other direction: equal-shaped fields of two DIFFERENT locals are two
    /// members. A key of name and field type would collapse all three reads into one answer:
    /// the scalar field type is deliberately soft (`unknown`), so it separates nothing.
    #[test]
    fn typed_member_highlight_does_not_cross_receivers() {
        let source = "Процедура Тест() Экспорт\n    С = Новый Структура(\"Поле\", 1);\n    Д = Новый Структура(\"Поле\", 2);\n    Х = С.Поле;\n    У = С.Поле;\n    Т = Д.Поле;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let lines_from = |needle: &str| {
            let at = source.find(needle).unwrap() + needle.find('.').unwrap() + 1;
            let mut highlights = document_highlights(&db, file_id, TextSize::from(at as u32));
            highlights.sort_by_key(|highlight| highlight.range.start());
            highlights
                .iter()
                .map(|highlight| source[..highlight.range.start().into()].matches('\n').count())
                .collect::<Vec<_>>()
        };

        assert_eq!(lines_from("С.Поле"), vec![3, 4], "С's reads are not one member");
        assert_eq!(lines_from("Д.Поле"), vec![5], "Д's read belongs to its own member");
    }

    /// The receiver's value type is a property of a program point, not of the member's
    /// identity: reassigning the local to a structure whose key carries another value type
    /// does not make a second member. The mirror of the implicit-local doctrine gate.
    #[test]
    fn typed_member_highlights_ignore_the_type_of_the_reassignment() {
        let source = "Процедура Тест() Экспорт\n    С = Новый Структура(\"Поле\", 1);\n    Х = С.Поле;\n    С = Новый Структура(\"Поле\", \"текст\");\n    У = С.Поле;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let at = source.find("С.Поле").unwrap() + "С.".len();
        let mut highlights = document_highlights(&db, file_id, TextSize::from(at as u32));
        highlights.sort_by_key(|highlight| highlight.range.start());

        let lines: Vec<usize> = highlights
            .iter()
            .map(|highlight| source[..highlight.range.start().into()].matches('\n').count())
            .collect();
        assert_eq!(lines, vec![2, 4], "the reassignment split the member: {highlights:?}");
    }

    /// A receiver that is itself a member (`С.Внутр.Поле`) is identified by its own key,
    /// recursively: both reads of the nested member light, another outer local's nested
    /// member stays out.
    #[test]
    fn typed_member_highlights_nested_receiver_chain() {
        let source = "Процедура Тест() Экспорт\n    С = Новый Структура(\"Внутр\", Новый Структура(\"Поле\", 1));\n    Д = Новый Структура(\"Внутр\", Новый Структура(\"Поле\", 2));\n    Х = С.Внутр.Поле;\n    У = С.Внутр.Поле;\n    Т = Д.Внутр.Поле;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let lines_from = |needle: &str| {
            let at = source.find(needle).unwrap() + needle.rfind('.').unwrap() + 1;
            let mut highlights = document_highlights(&db, file_id, TextSize::from(at as u32));
            highlights.sort_by_key(|highlight| highlight.range.start());
            highlights
                .iter()
                .map(|highlight| source[..highlight.range.start().into()].matches('\n').count())
                .collect::<Vec<_>>()
        };

        assert_eq!(lines_from("С.Внутр.Поле"), vec![3, 4], "С's nested member split");
        assert_eq!(lines_from("Д.Внутр.Поле"), vec![5], "Д's nested member leaked");
    }

    /// A receiver that names nothing the rule can take (`Получить().Поле` — the result of a
    /// call) has no identity beyond its spelling: each read answers for itself alone. The
    /// call's type flows here from the returned typed local, so the member resolves and the
    /// spelling branch is live, not defensive.
    #[test]
    fn typed_member_highlights_of_a_call_receiver_stay_one_site() {
        let source = "Функция Получить() Экспорт\n    С = Новый Структура(\"Поле\", 1);\n    Возврат С;\nКонецФункции\n\nПроцедура Тест() Экспорт\n    Х = Получить().Поле;\n    У = Получить().Поле;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let at = |nth: usize| {
            source
                .match_indices("Получить().Поле")
                .nth(nth)
                .map(|(at, _)| TextSize::from((at + "Получить().".len()) as u32))
                .expect("the input must carry two call reads")
        };

        for (nth, own_line) in [(0usize, 6usize), (1usize, 7usize)] {
            let mut highlights = document_highlights(&db, file_id, at(nth));
            highlights.sort_by_key(|highlight| highlight.range.start());
            let lines: Vec<usize> = highlights
                .iter()
                .map(|highlight| source[..highlight.range.start().into()].matches('\n').count())
                .collect();
            assert_eq!(lines, vec![own_line], "call read #{nth}: {highlights:?}");
        }
    }

    #[test]
    fn no_symbol_returns_empty_highlights() {
        let source = "Процедура Тест() КонецПроцедуры";
        let (db, file_id) = create_db_with_file(source);

        let offset = TextSize::from(source.find('(').unwrap() as u32);
        let highlights = document_highlights(&db, file_id, offset);

        assert!(highlights.is_empty());
    }

    fn create_db_with_two_files(
        source_a: &str,
        path_a: &str,
        source_b: &str,
        path_b: &str,
    ) -> (RootDatabaseImpl, FileId, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_a = FileId(0);
        let file_b = FileId(1);

        let mut file_set = FileSet::new();
        file_set.insert(file_a, VfsPath::new(path_a));
        file_set.insert(file_b, VfsPath::new(path_b));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_a, SourceRootId(0));
        db.set_file_source_root(file_b, SourceRootId(0));
        db.set_file_text(file_a, source_a);
        db.set_file_text(file_b, source_b);

        (db, file_a, file_b)
    }

    #[test]
    fn document_highlights_do_not_cross_file_boundaries() {
        let source_a = r#"
Процедура ОбщийМетод() Экспорт
    ОбщийМетод();
КонецПроцедуры
"#;
        let source_b = r#"
Процедура ОбщийМетод() Экспорт
    ОбщийМетод();
КонецПроцедуры
"#;
        let (db, file_a, _file_b) =
            create_db_with_two_files(source_a, "/a.bsl", source_b, "/b.bsl");

        let offset = TextSize::from(source_a.find("ОбщийМетод").unwrap() as u32);
        let highlights = document_highlights(&db, file_a, offset);

        assert_eq!(
            highlights.len(),
            2,
            "expected definition + 1 call in file A, got {} highlights",
            highlights.len()
        );

        let source_len = source_a.len() as u32;
        for highlight in &highlights {
            let start: u32 = highlight.range.start().into();
            let end: u32 = highlight.range.end().into();
            assert!(
                end <= source_len,
                "highlight range {start}..{end} exceeds file A source length {source_len} — \
                 file-B range leaked into result"
            );
            assert_eq!(&source_a[start as usize..end as usize], "ОбщийМетод");
        }
    }

    #[test]
    fn document_highlights_on_call_site_stay_file_local() {
        let source_a = r#"
Процедура Вызов() Экспорт
    Помощник();
    Помощник();
КонецПроцедуры

Процедура Помощник() Экспорт
КонецПроцедуры
"#;
        let source_b = r#"
Процедура Помощник() Экспорт
    Помощник();
КонецПроцедуры
"#;
        let (db, file_a, _file_b) =
            create_db_with_two_files(source_a, "/caller.bsl", source_b, "/other.bsl");

        let first_call = source_a.find("Помощник();").unwrap();
        let offset = TextSize::from(first_call as u32);
        let highlights = document_highlights(&db, file_a, offset);

        assert_eq!(
            highlights.len(),
            3,
            "expected 2 calls + 1 definition in file A, got {} highlights",
            highlights.len()
        );

        let source_len = source_a.len() as u32;
        for highlight in &highlights {
            let start: u32 = highlight.range.start().into();
            let end: u32 = highlight.range.end().into();
            assert!(
                end <= source_len,
                "highlight range {start}..{end} exceeds file A source length {source_len}"
            );
            assert_eq!(&source_a[start as usize..end as usize], "Помощник");
        }
    }
}
