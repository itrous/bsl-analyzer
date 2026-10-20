//! A keyword of the query language used as an alias.
//!
//! The two lists are the platform's verdicts, taken word by word on 8.3.17 and 8.3.27 as a field
//! alias and as a table alias, with the same answer in both positions on both versions: a refused
//! word fails with «Ожидается имя» pointing at the alias, an accepted one parses. The subquery
//! form below reaches the same alias rule behind a parenthesised source. Words outside the lists
//! were not checked.

use parser::parse_sdbl;
use parser_error::ParseError;
use syntax::sdbl_query::collect_query_parse_errors;
use syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

const REFUSED: &[&str] = &[
    "В",
    "ИЗ",
    "ГДЕ",
    "И",
    "ИЛИ",
    "НЕ",
    "КАК",
    "ПО",
    "ВЫБОР",
    "КОГДА",
    "ТОГДА",
    "ИНАЧЕ",
    "ЕСТЬ",
    "NULL",
    "ИСТИНА",
    "ЛОЖЬ",
    "ПОДОБНО",
    "СПЕЦСИМВОЛ",
    "МЕЖДУ",
    "ПЕРВЫЕ",
    "РАЗЛИЧНЫЕ",
    "РАЗРЕШЕННЫЕ",
    "ВНУТРЕННЕЕ",
    "ДЛЯ",
    "ПОМЕСТИТЬ",
    "ВОЗР",
    "УБЫВ",
    "АВТОУПОРЯДОЧИВАНИЕ",
    "ОБЩИЕ",
    "ТОЛЬКО",
    "ПЕРИОДАМИ",
    "НЕОПРЕДЕЛЕНО",
    "ВЫРАЗИТЬ",
    "IN",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "AS",
    "BY",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "IS",
    "TRUE",
    "FALSE",
    "LIKE",
    "BETWEEN",
    "TOP",
    "DISTINCT",
    "ALLOWED",
    "INNER",
    "FOR",
    "INTO",
    "ASC",
    "DESC",
    "ONLY",
    "OVERALL",
    "CAST",
    "UNDEFINED",
];

const ACCEPTED: &[&str] = &[
    "КОНЕЦ",
    "ИЕРАРХИИ",
    "ВЫБРАТЬ",
    "СГРУППИРОВАТЬ",
    "ИМЕЮЩИЕ",
    "УПОРЯДОЧИТЬ",
    "ИТОГИ",
    "ОБЪЕДИНИТЬ",
    "ВСЕ",
    "СОЕДИНЕНИЕ",
    "ЛЕВОЕ",
    "ПРАВОЕ",
    "ПОЛНОЕ",
    "ВНЕШНЕЕ",
    "ИЗМЕНЕНИЯ",
    "УНИЧТОЖИТЬ",
    "ИНДЕКСИРОВАТЬ",
    "ЗНАЧЕНИЕ",
    "ТИП",
    "ССЫЛКА",
    "СУММА",
    "КОЛИЧЕСТВО",
    "МАКСИМУМ",
    "МИНИМУМ",
    "СРЕДНЕЕ",
    "ПОДСТРОКА",
    "ГОД",
    "МЕСЯЦ",
    "ДАТАВРЕМЯ",
    "ЕСТЬNULL",
    "ПРЕДСТАВЛЕНИЕ",
    "СТРОКА",
    "ЧИСЛО",
    "ДАТА",
    "БУЛЕВО",
    "END",
    "SELECT",
    "GROUP",
    "HAVING",
    "ORDER",
    "TOTALS",
    "UNION",
    "ALL",
    "JOIN",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "UPDATE",
    "DROP",
    "INDEX",
    "HIERARCHY",
    "VALUE",
    "TYPE",
    "REFS",
    "SUM",
    "COUNT",
];

const EXPLICIT: &str =
    "ожидается имя: ключевое слово языка запросов нельзя использовать как псевдоним";
const IMPLICIT: &str =
    "синтаксическая ошибка: ключевое слово языка запросов нельзя использовать как псевдоним";

/// The alias positions, with the alias as the last word.
const POSITIONS: &[(&str, &str)] = &[
    ("field alias", "ВЫБРАТЬ 1 КАК "),
    ("table alias", "ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК "),
    ("subquery alias", "ВЫБРАТЬ 1 ИЗ (ВЫБРАТЬ 1 КАК Б) КАК "),
];

/// Every parse error a query route sees, as (range, message).
fn errors(query: &str) -> Vec<(TextRange, &'static str)> {
    collect_query_parse_errors(&parse_sdbl(query))
        .into_iter()
        .map(|(range, error)| (range, custom_message(&error)))
        .collect()
}

fn custom_message(error: &ParseError) -> &'static str {
    match error {
        ParseError::Custom { message, .. } => message,
        _ => "<not a custom error>",
    }
}

fn range_at(start: usize, text: &str) -> TextRange {
    TextRange::at(TextSize::try_from(start).unwrap(), TextSize::of(text))
}

/// The last word of every alias node, keyword kinds included.
fn alias_words(root: &SyntaxNode) -> Vec<String> {
    root.descendants()
        .filter(|node| node.kind() == SyntaxKind::SDBL_ALIAS)
        .filter_map(|node| {
            node.descendants_with_tokens()
                .filter_map(|it| it.into_token())
                .filter(|token| !token.kind().is_trivia())
                .last()
                .map(|token| token.text().to_string())
        })
        .collect()
}

fn has_kind(root: &SyntaxNode, kind: SyntaxKind) -> bool {
    root.descendants().any(|node| node.kind() == kind)
}

#[test]
fn a_refused_word_is_the_one_finding_at_the_alias_in_every_position() {
    for (position, prefix) in POSITIONS {
        for word in REFUSED {
            let query = format!("{prefix}{word}");
            assert_eq!(
                errors(&query),
                vec![(range_at(prefix.len(), word), EXPLICIT)],
                "{position}: `{query}`"
            );
        }
    }
}

/// A clause word followed by a list separator or by the end opens no clause, so it is the alias
/// the text meant, and the list goes on behind it.
#[test]
fn a_refused_clause_word_before_a_separator_is_taken_as_the_alias() {
    for (query, words) in [
        ("ВЫБРАТЬ 1 КАК ГДЕ, 2 КАК КОГДА", &["ГДЕ", "КОГДА"][..]),
        ("ВЫБРАТЬ Т.А КАК А ИЗ (ВЫБРАТЬ 1 КАК ИЗ) КАК Т", &["ИЗ"][..]),
        ("ВЫБРАТЬ 1 КАК ПО;\nВЫБРАТЬ 2 КАК Б", &["ПО"][..]),
        ("ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Валюты КАК ДЛЯ, Справочник.Валюты КАК Т", &["ДЛЯ"][..]),
    ] {
        let expected: Vec<_> = words
            .iter()
            .map(|word| {
                let start = query.find(&format!("КАК {word}")).unwrap() + "КАК ".len();
                (range_at(start, word), EXPLICIT)
            })
            .collect();
        assert_eq!(errors(query), expected, "`{query}`");
    }
}

#[test]
fn an_accepted_word_is_a_clean_alias_in_every_position() {
    for (position, prefix) in POSITIONS {
        for word in ACCEPTED {
            let query = format!("{prefix}{word}");
            let root = parse_sdbl(&query).syntax_node();
            assert_eq!(errors(&query), Vec::new(), "{position}: `{query}`");
            // Silence alone would also hold if the word were dropped from the tree; it has to
            // be the alias.
            assert_eq!(
                alias_words(&root).last().map(String::as_str),
                Some(*word),
                "{position}: `{query}`"
            );
            let queries =
                root.descendants().filter(|node| node.kind() == SyntaxKind::SDBL_SELECT_QUERY);
            assert_eq!(queries.count(), 1, "{position}: `{query}` must stay one query");
        }
    }
}

#[test]
fn the_word_is_matched_regardless_of_case() {
    for (query, word) in [
        ("ВЫБРАТЬ 1 КАК в", "в"),
        ("ВЫБРАТЬ 1 как Когда", "Когда"),
        ("SELECT 1 AS null", "null"),
        ("SELECT 1 FROM Catalog.Currencies as Distinct", "Distinct"),
    ] {
        let start = query.rfind(word).unwrap();
        assert_eq!(errors(query), vec![(range_at(start, word), EXPLICIT)], "`{query}`");
    }
    assert_eq!(errors("ВЫБРАТЬ 1 КАК Упорядочить"), Vec::new());
}

/// Without `КАК` the platform answers with a syntax error at the word rather than «Ожидается
/// имя»; the position is still the word.
#[test]
fn a_refused_word_after_a_field_or_a_source_without_as_is_reported_at_itself() {
    for (query, word) in [
        ("ВЫБРАТЬ 1 КОГДА", "КОГДА"),
        ("ВЫБРАТЬ 1 ИЗ Справочник.Валюты КОГДА", "КОГДА"),
        ("ВЫБРАТЬ 1 ИЗ Справочник.Валюты ВЫРАЗИТЬ", "ВЫРАЗИТЬ"),
        ("SELECT 1 FROM Catalog.Currencies BY", "BY"),
        ("SELECT 1 BY", "BY"),
    ] {
        let start = query.rfind(word).unwrap();
        assert_eq!(errors(query), vec![(range_at(start, word), IMPLICIT)], "`{query}`");
    }
}

/// The words that reach the parser with kinds of their own are refused without `КАК` as well,
/// and the clause behind the source is still parsed. Behind a field only the literals reach the
/// alias: `И`, `ИЛИ`, `В` and `НЕ` continue the expression.
#[test]
fn a_refused_keyword_kind_without_as_is_reported_at_itself() {
    let mut queries = Vec::new();
    for word in [
        "В",
        "И",
        "ИЛИ",
        "НЕ",
        "ИСТИНА",
        "ЛОЖЬ",
        "НЕОПРЕДЕЛЕНО",
        "IN",
        "AND",
        "OR",
        "NOT",
        "TRUE",
        "FALSE",
        "UNDEFINED",
    ] {
        queries.push((format!("ВЫБРАТЬ 1 ИЗ Справочник.Валюты {word}"), word));
    }
    for word in ["ИСТИНА", "ЛОЖЬ", "НЕОПРЕДЕЛЕНО", "TRUE", "FALSE", "UNDEFINED"]
    {
        queries.push((format!("ВЫБРАТЬ 1 {word}"), word));
    }
    for (query, word) in &queries {
        let start = query.rfind(word).unwrap();
        assert_eq!(errors(query), vec![(range_at(start, word), IMPLICIT)], "`{query}`");
    }

    let query = "ВЫБРАТЬ 1 ИЗ Справочник.Валюты В ГДЕ ИСТИНА";
    let start = query.find(" В ").unwrap() + 1;
    assert_eq!(errors(query), vec![(range_at(start, "В"), IMPLICIT)], "`{query}`");
    let root = parse_sdbl(query).syntax_node();
    assert!(has_kind(&root, SyntaxKind::SDBL_WHERE_CLAUSE), "`{query}`: {root:#?}");
}

/// A join, or the `ПО` of one, cannot follow a field, so after a field's `КАК` such a word is the
/// alias it was meant to be, and the source clause behind it is still parsed. A clause word is
/// taken as a recovery span, which runs up to the next word.
#[test]
fn a_join_word_as_a_field_alias_leaves_the_source_clause_intact() {
    for (query, word, span) in [
        ("SELECT 1 AS INNER FROM Catalog.Currencies", "INNER", "INNER "),
        ("ВЫБРАТЬ 1 КАК ВНУТРЕННЕЕ ИЗ Справочник.Валюты", "ВНУТРЕННЕЕ", "ВНУТРЕННЕЕ "),
        ("ВЫБРАТЬ 1 КАК ПО ИЗ Справочник.Валюты", "ПО", "ПО "),
        ("SELECT 1 AS BY FROM Catalog.Currencies", "BY", "BY"),
    ] {
        let start = query.find(&format!(" {word} ")).unwrap() + 1;
        let span = range_at(start, span);
        assert_eq!(errors(query), vec![(span, EXPLICIT)], "`{query}`");
        let root = parse_sdbl(query).syntax_node();
        assert!(has_kind(&root, SyntaxKind::SDBL_FROM_CLAUSE), "`{query}`: {root:#?}");
    }
}

/// A source spelled with a refused word is referenced afterwards; the reference reads as a chain
/// so the alias stays the one finding, in the select list and in a filter alike.
#[test]
fn a_reference_to_a_refused_alias_adds_no_finding_of_its_own() {
    for word in [
        "В",
        "И",
        "ИЛИ",
        "НЕ",
        "ИСТИНА",
        "ЛОЖЬ",
        "НЕОПРЕДЕЛЕНО",
        "ВЫБОР",
        "NULL",
        "КОГДА",
        "ЕСТЬ",
        "ВЫРАЗИТЬ",
        "IN",
        "AND",
        "OR",
        "NOT",
        "TRUE",
        "FALSE",
        "UNDEFINED",
        "CASE",
    ] {
        for query in [
            format!("ВЫБРАТЬ {word}.Ссылка КАК Ссылка ИЗ Справочник.Валюты КАК {word}"),
            format!("ВЫБРАТЬ 1 КАК Б, {word}.Ссылка КАК Ссылка ИЗ Справочник.Валюты КАК {word}"),
            format!("ВЫБРАТЬ 1 КАК Б ИЗ Справочник.Валюты КАК {word} ГДЕ {word}.Ссылка = 1"),
            format!(
                "ВЫБРАТЬ 1 КАК Б ИЗ Справочник.Валюты КАК {word} ГДЕ 1 = 1 И НЕ {word}.Ссылка = 1"
            ),
            format!("ВЫБРАТЬ -{word}.Ссылка КАК Б ИЗ Справочник.Валюты КАК {word}"),
        ] {
            let alias = query.find(&format!("КАК {word}")).unwrap() + "КАК ".len();
            assert_eq!(errors(&query), vec![(range_at(alias, word), EXPLICIT)], "`{query}`");
        }
    }
}

/// The words that read as a chain in front of a dot keep their meaning everywhere else.
#[test]
fn operators_and_literals_keep_their_meaning_without_a_dot() {
    for query in [
        "ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Валюты КАК Т ГДЕ Т.А В (1, 2)",
        "ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Валюты КАК Т ГДЕ НЕ Т.А = 1 И ИСТИНА ИЛИ ЛОЖЬ",
        "ВЫБРАТЬ ВЫБОР КОГДА Т.А ЕСТЬ NULL ТОГДА НЕОПРЕДЕЛЕНО ИНАЧЕ Т.А КОНЕЦ КАК А \
         ИЗ Справочник.Валюты КАК Т",
        "ВЫБРАТЬ NULL КАК А, -Т.А КАК Б, НЕ ИСТИНА КАК В1 ИЗ Справочник.Валюты КАК Т",
        "ВЫБРАТЬ Т.В КАК А, Т.И КАК Б, Т.НЕ КАК В1 ИЗ Справочник.Валюты КАК Т",
    ] {
        assert_eq!(errors(query), Vec::new(), "`{query}`");
    }
}

/// An accepted clause word as an alias stays one alias: the clause it names is still parsed
/// where it really begins, and no query of the package is split off.
#[test]
fn an_accepted_clause_word_as_an_alias_leaves_the_real_clause_intact() {
    for (query, clause) in [
        ("ВЫБРАТЬ Т.А КАК Выбрать ИЗ Справочник.Валюты КАК Т", SyntaxKind::SDBL_FROM_CLAUSE),
        (
            "ВЫБРАТЬ Т.А КАК Упорядочить ИЗ Справочник.Валюты КАК Т УПОРЯДОЧИТЬ ПО Упорядочить",
            SyntaxKind::SDBL_ORDER_CLAUSE,
        ),
        (
            "ВЫБРАТЬ Т.А КАК Сгруппировать ИЗ Справочник.Валюты КАК Т СГРУППИРОВАТЬ ПО Т.А",
            SyntaxKind::SDBL_GROUP_CLAUSE,
        ),
        (
            "ВЫБРАТЬ Т.А КАК А ИЗ Справочник.Валюты КАК Объединить \
             ОБЪЕДИНИТЬ ВЫБРАТЬ 1 ИЗ Справочник.Валюты КАК Т",
            SyntaxKind::SDBL_UNION_CLAUSE,
        ),
    ] {
        assert_eq!(errors(query), Vec::new(), "`{query}`");
        let root = parse_sdbl(query).syntax_node();
        assert!(has_kind(&root, clause), "`{query}`: {root:#?}");
    }
}

/// After an explicit `КАК` a refused word that opens a clause is treated as an omitted alias:
/// reported where it stands, and the clause behind it is still parsed.
#[test]
fn a_refused_clause_word_after_as_is_left_to_its_clause() {
    let query = "ВЫБРАТЬ Т.А КАК ИЗ Справочник.Валюты КАК ГДЕ Т.А = 1";
    let from = query.find("ИЗ").unwrap();
    let r#where = query.find("ГДЕ").unwrap();
    assert_eq!(
        errors(query),
        vec![(range_at(from, ""), EXPLICIT), (range_at(r#where, ""), EXPLICIT)],
        "`{query}`"
    );
    let root = parse_sdbl(query).syntax_node();
    for clause in [SyntaxKind::SDBL_FROM_CLAUSE, SyntaxKind::SDBL_WHERE_CLAUSE] {
        assert!(has_kind(&root, clause), "{clause:?}: {root:#?}");
    }
}

/// A word already refused as a table name, or as the part of a name after a dot, is not reported
/// a second time as a bad alias.
#[test]
fn a_word_refused_as_a_name_is_not_reported_again_as_an_alias() {
    for query in ["SELECT A FROM BY", "SELECT A FROM Catalog.BY", "SELECT A REFS Catalog.BY FROM T"]
    {
        let found = errors(query);
        assert_eq!(found.len(), 1, "`{query}`: {found:?}");
        assert!(found.iter().all(|(_, message)| *message != IMPLICIT), "`{query}`: {found:?}");
    }
}
