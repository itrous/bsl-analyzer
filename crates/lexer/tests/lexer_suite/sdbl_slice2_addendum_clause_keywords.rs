use lexer::sdbl::{tokenize_sdbl, SdblTokenKind};

fn single_kind(src: &str) -> SdblTokenKind {
    let toks: Vec<_> = tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .collect();
    assert_eq!(toks.len(), 1, "expected exactly one token for {src:?}, got {toks:#?}");
    toks[0].kind
}

#[test]
fn kw_periods_canonical_russian_periodami() {
    assert_eq!(single_kind("ПЕРИОДАМИ"), SdblTokenKind::KwPeriods);
}

#[test]
fn kw_periods_english_unchanged() {
    assert_eq!(single_kind("PERIODS"), SdblTokenKind::KwPeriods);
}

#[test]
fn kw_periods_legacy_misspelling_now_ident() {
    assert_eq!(single_kind("ПЕРИОДЫ"), SdblTokenKind::Ident);
}

#[test]
fn kw_drop_bilingual() {
    assert_eq!(single_kind("УНИЧТОЖИТЬ"), SdblTokenKind::KwDrop);
    assert_eq!(single_kind("DROP"), SdblTokenKind::KwDrop);
}

#[test]
fn kw_autoorder_bilingual() {
    assert_eq!(single_kind("АВТОУПОРЯДОЧИВАНИЕ"), SdblTokenKind::KwAutoOrder);
    assert_eq!(single_kind("AUTOORDER"), SdblTokenKind::KwAutoOrder);
}

#[test]
fn kw_asc_bilingual() {
    assert_eq!(single_kind("ВОЗР"), SdblTokenKind::KwAsc);
    assert_eq!(single_kind("ASC"), SdblTokenKind::KwAsc);
}

#[test]
fn kw_desc_bilingual() {
    assert_eq!(single_kind("УБЫВ"), SdblTokenKind::KwDesc);
    assert_eq!(single_kind("DESC"), SdblTokenKind::KwDesc);
}

#[test]
fn kw_hierarchy_bilingual() {
    assert_eq!(single_kind("ИЕРАРХИЯ"), SdblTokenKind::KwHierarchy);
    assert_eq!(single_kind("HIERARCHY"), SdblTokenKind::KwHierarchy);
}

#[test]
fn kw_allowed_bilingual() {
    assert_eq!(single_kind("РАЗРЕШЕННЫЕ"), SdblTokenKind::KwAllowed);
    assert_eq!(single_kind("ALLOWED"), SdblTokenKind::KwAllowed);
}

#[test]
fn kw_for_bilingual() {
    assert_eq!(single_kind("ДЛЯ"), SdblTokenKind::KwFor);
    assert_eq!(single_kind("FOR"), SdblTokenKind::KwFor);
}

#[test]
fn kw_update_bilingual() {
    assert_eq!(single_kind("ИЗМЕНЕНИЯ"), SdblTokenKind::KwUpdate);
    assert_eq!(single_kind("UPDATE"), SdblTokenKind::KwUpdate);
}

#[test]
fn kw_index_bilingual() {
    assert_eq!(single_kind("ИНДЕКСИРОВАТЬ"), SdblTokenKind::KwIndex);
    assert_eq!(single_kind("INDEX"), SdblTokenKind::KwIndex);
}

#[test]
fn kw_only_bilingual() {
    assert_eq!(single_kind("ТОЛЬКО"), SdblTokenKind::KwOnly);
    assert_eq!(single_kind("ONLY"), SdblTokenKind::KwOnly);
}

#[test]
fn kw_overall_bilingual() {
    assert_eq!(single_kind("ОБЩИЕ"), SdblTokenKind::KwOverall);
    assert_eq!(single_kind("OVERALL"), SdblTokenKind::KwOverall);
}

#[test]
fn kw_escape_bilingual() {
    assert_eq!(single_kind("СПЕЦСИМВОЛ"), SdblTokenKind::KwEscape);
    assert_eq!(single_kind("ESCAPE"), SdblTokenKind::KwEscape);
}

#[test]
fn kw_refs_bilingual() {
    assert_eq!(single_kind("ССЫЛКА"), SdblTokenKind::KwRefs);
    assert_eq!(single_kind("REFS"), SdblTokenKind::KwRefs);
}

#[test]
fn kw_cast_bilingual() {
    assert_eq!(single_kind("ВЫРАЗИТЬ"), SdblTokenKind::KwCast);
    assert_eq!(single_kind("CAST"), SdblTokenKind::KwCast);
}

#[test]
fn kw_type_bilingual() {
    assert_eq!(single_kind("ТИП"), SdblTokenKind::KwType);
    assert_eq!(single_kind("TYPE"), SdblTokenKind::KwType);
}

#[test]
fn kw_value_bilingual() {
    assert_eq!(single_kind("ЗНАЧЕНИЕ"), SdblTokenKind::KwValue);
    assert_eq!(single_kind("VALUE"), SdblTokenKind::KwValue);
}

#[test]
fn case_insensitivity_addendum() {
    for s in ["УНИЧТОЖИТЬ", "Уничтожить", "уничтожить"] {
        assert_eq!(single_kind(s), SdblTokenKind::KwDrop);
    }
    for s in ["DROP", "Drop", "drop"] {
        assert_eq!(single_kind(s), SdblTokenKind::KwDrop);
    }
    for s in ["ВЫРАЗИТЬ", "Выразить", "выразить"] {
        assert_eq!(single_kind(s), SdblTokenKind::KwCast);
    }
    for s in ["ИНДЕКСИРОВАТЬ", "Индексировать", "индексировать"]
    {
        assert_eq!(single_kind(s), SdblTokenKind::KwIndex);
    }
    for s in ["ПЕРИОДАМИ", "Периодами", "периодами"] {
        assert_eq!(single_kind(s), SdblTokenKind::KwPeriods);
    }
}

fn significant_kinds(src: &str) -> Vec<SdblTokenKind> {
    tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .map(|t| t.kind)
        .collect()
}

#[test]
fn structural_drop_in_batch() {
    let kinds = significant_kinds("ВЫБРАТЬ 1 ПОМЕСТИТЬ #T; УНИЧТОЖИТЬ #T");
    assert!(kinds.contains(&SdblTokenKind::KwDrop));
    assert!(kinds.contains(&SdblTokenKind::Semicolon));
}

#[test]
fn structural_order_by_with_modifiers() {
    let kinds = significant_kinds("УПОРЯДОЧИТЬ ПО Имя ВОЗР, Цена УБЫВ ИЕРАРХИЯ");
    assert!(kinds.contains(&SdblTokenKind::KwOrder));
    assert!(kinds.contains(&SdblTokenKind::KwAsc));
    assert!(kinds.contains(&SdblTokenKind::KwDesc));
    assert!(kinds.contains(&SdblTokenKind::KwHierarchy));
}

#[test]
fn structural_totals_periods_canonical() {
    let kinds = significant_kinds(
        "ИТОГИ СУММА(X) ПО ОБЩИЕ, Период ПЕРИОДАМИ(МИНУТА, ДАТАВРЕМЯ(2024, 1, 1), &End)",
    );
    assert!(kinds.contains(&SdblTokenKind::KwTotals));
    assert!(kinds.contains(&SdblTokenKind::KwOverall));
    assert!(kinds.contains(&SdblTokenKind::KwPeriods));
}

#[test]
fn structural_for_update_two_word_form() {
    let kinds = significant_kinds("ВЫБРАТЬ 1 ИЗ T ДЛЯ ИЗМЕНЕНИЯ");
    assert_eq!(
        kinds
            .iter()
            .filter(|k| matches!(k, SdblTokenKind::KwFor | SdblTokenKind::KwUpdate))
            .count(),
        2
    );
}

#[test]
fn structural_index_by_field() {
    let kinds = significant_kinds("ИНДЕКСИРОВАТЬ ПО T.F");
    assert!(kinds.contains(&SdblTokenKind::KwIndex));
    assert!(kinds.contains(&SdblTokenKind::KwOnOrBy));
}

#[test]
fn structural_like_escape_canonical() {
    let kinds = significant_kinds("Н ПОДОБНО \"%X%\" СПЕЦСИМВОЛ \"!\"");
    assert!(kinds.contains(&SdblTokenKind::KwLike));
    assert!(kinds.contains(&SdblTokenKind::KwEscape));
}

#[test]
fn structural_refs_canonical() {
    let kinds = significant_kinds("ГДЕ Р ССЫЛКА Документ.ПриходнаяНакладная");
    assert!(kinds.contains(&SdblTokenKind::KwWhere));
    assert!(kinds.contains(&SdblTokenKind::KwRefs));
}

#[test]
fn structural_cast_with_as_target() {
    let kinds = significant_kinds("ВЫРАЗИТЬ(П КАК ЧИСЛО(15, 2))");
    assert!(kinds.contains(&SdblTokenKind::KwCast));
    assert!(kinds.contains(&SdblTokenKind::KwAs));
}

#[test]
fn structural_value_canonical() {
    let kinds = significant_kinds("ЗНАЧЕНИЕ(Справочник.Товары.ПустаяСсылка)");
    assert!(kinds.contains(&SdblTokenKind::KwValue));
    assert!(kinds.contains(&SdblTokenKind::LParen));
    assert!(kinds.contains(&SdblTokenKind::RParen));
}

#[test]
fn addendum_keyword_prefix_identifiers_lex_as_ident() {
    for s in [
        "DROPPED",
        "ALLOWED_FLAG",
        "INDEXING",
        "CASTABLE",
        "ВЫРАЗИТЬNESS",
        "УНИЧТОЖИТЬ_ALL",
        "ИНДЕКСИРОВАТЬ_ВСЁ",
    ] {
        let toks = tokenize_sdbl(s);
        assert_eq!(toks.len(), 1, "expected one token for {s:?}");
        assert_eq!(toks[0].kind, SdblTokenKind::Ident, "{s:?} should lex as Ident");
        assert_eq!(toks[0].text.as_str(), s);
    }
}
