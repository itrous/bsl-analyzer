//! Acceptance suite for the SDBL query-extension brace pair.
//!
//! The braces are not part of the query language: they come from the
//! data-composition extension of it, attested by the 1C:Enterprise
//! syntax assistant article «Расширение языка запросов для системы
//! компоновки данных».
//! Provenance: `docs/legal/sdbl-clean-room-slice1-addendum.md`.

use lexer::sdbl::{tokenize_sdbl, SdblToken, SdblTokenKind};

fn significant(src: &str) -> Vec<SdblToken> {
    tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .collect()
}

fn kinds(src: &str) -> Vec<SdblTokenKind> {
    significant(src).into_iter().map(|t| t.kind).collect()
}

/// Every keyword of the `ХАРАКТЕРИСТИКИ` construct, in both languages.
/// The extension defines them; this lexer deliberately does not.
const EXTENSION_KEYWORDS: &[(&str, &str)] = &[
    ("ХАРАКТЕРИСТИКИ", "CHARACTERISTICS"),
    ("ВИДЫХАРАКТЕРИСТИК", "CHARACTERISTICTYPES"),
    ("ПОЛЕКЛЮЧА", "KEYFIELD"),
    ("ПОЛЕИМЕНИ", "NAMEFIELD"),
    ("ПОЛЕТИПАЗНАЧЕНИЯ", "VALUETYPEFIELD"),
    ("ЗНАЧЕНИЯХАРАКТЕРИСТИК", "CHARACTERISTICVALUES"),
    ("ПОЛЕОБЪЕКТА", "OBJECTFIELD"),
    ("ПОЛЕВИДА", "TYPEFIELD"),
    ("ПОЛЕЗНАЧЕНИЯ", "VALUEFIELD"),
];

// --- The tokens exist at all ------------------------------------------

#[test]
fn braces_are_tokens() {
    assert_eq!(kinds("{"), vec![SdblTokenKind::LBrace]);
    assert_eq!(kinds("}"), vec![SdblTokenKind::RBrace]);
    assert_eq!(kinds("{}"), vec![SdblTokenKind::LBrace, SdblTokenKind::RBrace]);
}

#[test]
fn braces_are_not_error_tokens() {
    // The contrast that gives the slice its point: a byte no rule matches
    // becomes `Error`, and the braces used to be in that position.
    assert_eq!(kinds("@"), vec![SdblTokenKind::Error]);
    assert!(!kinds("{ГДЕ Т.Поле}").contains(&SdblTokenKind::Error));
}

#[test]
fn braces_carry_their_own_spans() {
    let toks = significant("ВЫБРАТЬ 1 {}");
    let open = toks.iter().find(|t| t.kind == SdblTokenKind::LBrace).expect("no LBrace");
    let close = toks.iter().find(|t| t.kind == SdblTokenKind::RBrace).expect("no RBrace");
    assert_eq!(open.text.as_str(), "{");
    assert_eq!(close.text.as_str(), "}");
    assert_eq!(close.offset, open.offset + 1);
}

// --- The documented elements ------------------------------------------

#[test]
fn selection_element_bilingual() {
    assert_eq!(
        kinds("{ВЫБРАТЬ Номенклатура, Склад}"),
        vec![
            SdblTokenKind::LBrace,
            SdblTokenKind::KwSelect,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
        ]
    );
    assert_eq!(
        kinds("{SELECT Products, Warehouse}"),
        vec![
            SdblTokenKind::LBrace,
            SdblTokenKind::KwSelect,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
        ]
    );
}

#[test]
fn filter_element_with_child_field_marker() {
    assert_eq!(
        kinds("{ГДЕ Номенклатура.*, Склад}"),
        vec![
            SdblTokenKind::LBrace,
            SdblTokenKind::KwWhere,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Star,
            SdblTokenKind::Comma,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
        ],
        "the `.*` child-field marker needs no token of its own"
    );
}

#[test]
fn characteristics_element_opens_and_closes() {
    let k = kinds("{ХАРАКТЕРИСТИКИ ТИП(Справочник.Номенклатура) ПОЛЕКЛЮЧА Ссылка}");
    assert_eq!(k.first().copied(), Some(SdblTokenKind::LBrace));
    assert_eq!(k.last().copied(), Some(SdblTokenKind::RBrace));
    assert_eq!(k[1], SdblTokenKind::Ident, "ХАРАКТЕРИСТИКИ is not a keyword here");
    assert_eq!(k[2], SdblTokenKind::KwType, "ТИП is, and belongs to slice 2-addendum");
}

#[test]
fn parameter_form_inside_virtual_table_arguments() {
    assert_eq!(
        kinds("Обороты({&ДатаНачала}, {&ДатаКонца}, , {Номенклатура.*})"),
        vec![
            SdblTokenKind::VtTurnovers,
            SdblTokenKind::LParen,
            SdblTokenKind::LBrace,
            SdblTokenKind::Parameter,
            SdblTokenKind::RBrace,
            SdblTokenKind::Comma,
            SdblTokenKind::LBrace,
            SdblTokenKind::Parameter,
            SdblTokenKind::RBrace,
            SdblTokenKind::Comma,
            SdblTokenKind::Comma,
            SdblTokenKind::LBrace,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Star,
            SdblTokenKind::RBrace,
            SdblTokenKind::RParen,
        ]
    );
}

// --- The refusal ------------------------------------------------------

#[test]
fn extension_keywords_stay_identifiers() {
    for (ru, en) in EXTENSION_KEYWORDS {
        assert_eq!(kinds(ru), vec![SdblTokenKind::Ident], "{ru:?}");
        assert_eq!(kinds(en), vec![SdblTokenKind::Ident], "{en:?}");
    }
}

#[test]
fn extension_keywords_stay_identifiers_inside_a_brace_region_too() {
    // The lexer has no state, so being inside `{…}` changes nothing. This
    // is the property that makes a `CHARACTERISTICS` variant meaningless
    // rather than merely unused.
    let outside = kinds("ХАРАКТЕРИСТИКИ");
    let inside = kinds("{ХАРАКТЕРИСТИКИ}");
    assert_eq!(outside, vec![SdblTokenKind::Ident]);
    assert_eq!(inside, vec![SdblTokenKind::LBrace, SdblTokenKind::Ident, SdblTokenKind::RBrace]);
}

// --- Shapes the documentation does not describe -----------------------

#[test]
fn braces_nest() {
    assert_eq!(
        kinds("{{}}"),
        vec![
            SdblTokenKind::LBrace,
            SdblTokenKind::LBrace,
            SdblTokenKind::RBrace,
            SdblTokenKind::RBrace,
        ]
    );
    assert_eq!(
        kinds("{ГДЕ {Вложенный} Т.Поле}"),
        vec![
            SdblTokenKind::LBrace,
            SdblTokenKind::KwWhere,
            SdblTokenKind::LBrace,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
        ]
    );
}

#[test]
fn an_unbalanced_brace_tokenises_on_either_side() {
    assert_eq!(
        kinds("ВЫБРАТЬ 1 {ВЫБРАТЬ Т"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::LBrace,
            SdblTokenKind::KwSelect,
            SdblTokenKind::Ident,
        ],
        "a literal that ends mid-extension is ordinary input"
    );
    assert_eq!(
        kinds("ВЫБРАТЬ 1 } ИЗ Т"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::RBrace,
            SdblTokenKind::KwFrom,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn braces_inside_a_string_stay_in_the_string() {
    let toks = significant("ВЫБРАТЬ \"{ВЫБРАТЬ Поле}\" КАК Ф");
    assert!(
        !toks.iter().any(|t| matches!(t.kind, SdblTokenKind::LBrace | SdblTokenKind::RBrace)),
        "quoted braces must not become extension delimiters: {toks:#?}"
    );
    assert!(toks
        .iter()
        .any(|t| t.kind == SdblTokenKind::String && t.text.as_str() == "{ВЫБРАТЬ Поле}"));
}

// --- Structural integration -------------------------------------------

#[test]
fn an_extension_follows_a_complete_query() {
    let k = kinds("ВЫБРАТЬ Т.Код ИЗ Справочник.Товары КАК Т {ГДЕ Т.Код}");
    assert_eq!(k.first().copied(), Some(SdblTokenKind::KwSelect));
    assert_eq!(
        &k[k.len() - 6..],
        &[
            SdblTokenKind::LBrace,
            SdblTokenKind::KwWhere,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::RBrace,
        ],
        "the extension is a complete trailing block, not a clause of the query"
    );
}

#[test]
fn several_extensions_may_follow_one_another() {
    let k = kinds("{ВЫБРАТЬ Поле1} {ГДЕ Поле2} {УПОРЯДОЧИТЬ ПО Поле3}");
    assert_eq!(k.iter().filter(|x| **x == SdblTokenKind::LBrace).count(), 3);
    assert_eq!(k.iter().filter(|x| **x == SdblTokenKind::RBrace).count(), 3);
    assert!(
        k.contains(&SdblTokenKind::KwOrder) && k.contains(&SdblTokenKind::KwOnOrBy),
        "the braced ordering form is undocumented but occurs; its inner \
         clause is ordinary slice 2 vocabulary"
    );
}
