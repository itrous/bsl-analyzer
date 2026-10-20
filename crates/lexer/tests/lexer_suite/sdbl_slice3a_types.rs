use lexer::sdbl::{tokenize_sdbl, SdblTokenKind};

fn single_kind(src: &str) -> SdblTokenKind {
    let toks: Vec<_> = tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .collect();
    assert_eq!(toks.len(), 1, "expected exactly one token for {src:?}, got {toks:#?}");
    toks[0].kind
}

fn significant_kinds(src: &str) -> Vec<SdblTokenKind> {
    tokenize_sdbl(src)
        .into_iter()
        .filter(|t| t.kind != SdblTokenKind::Whitespace && t.kind != SdblTokenKind::Newline)
        .map(|t| t.kind)
        .collect()
}

#[test]
fn type_boolean_russian_canonical() {
    assert_eq!(single_kind("БУЛЕВО"), SdblTokenKind::TypeBoolean);
}

#[test]
fn type_boolean_english_canonical() {
    assert_eq!(single_kind("BOOLEAN"), SdblTokenKind::TypeBoolean);
}

#[test]
fn type_number_russian_canonical() {
    assert_eq!(single_kind("ЧИСЛО"), SdblTokenKind::TypeNumber);
}

#[test]
fn type_number_english_canonical() {
    assert_eq!(single_kind("NUMBER"), SdblTokenKind::TypeNumber);
}

#[test]
fn type_string_russian_canonical() {
    assert_eq!(single_kind("СТРОКА"), SdblTokenKind::TypeString);
}

#[test]
fn type_string_english_canonical() {
    assert_eq!(single_kind("STRING"), SdblTokenKind::TypeString);
}

#[test]
fn type_date_russian_canonical() {
    assert_eq!(single_kind("ДАТА"), SdblTokenKind::TypeDate);
}

#[test]
fn type_date_english_canonical() {
    assert_eq!(single_kind("DATE"), SdblTokenKind::TypeDate);
}

#[test]
fn lit_undefined_russian_canonical() {
    assert_eq!(single_kind("НЕОПРЕДЕЛЕНО"), SdblTokenKind::LitUndefined);
}

#[test]
fn lit_undefined_english_canonical() {
    assert_eq!(single_kind("UNDEFINED"), SdblTokenKind::LitUndefined);
}

#[test]
fn period_tendays_russian_canonical() {
    assert_eq!(single_kind("ДЕКАДА"), SdblTokenKind::PeriodTenDays);
}

#[test]
fn period_tendays_english_canonical() {
    assert_eq!(single_kind("TENDAYS"), SdblTokenKind::PeriodTenDays);
}

#[test]
fn period_halfyear_russian_canonical() {
    assert_eq!(single_kind("ПОЛУГОДИЕ"), SdblTokenKind::PeriodHalfYear);
}

#[test]
fn period_halfyear_english_canonical() {
    assert_eq!(single_kind("HALFYEAR"), SdblTokenKind::PeriodHalfYear);
}

#[test]
fn case_insensitivity_sweep() {
    use SdblTokenKind::*;
    let cases: &[(&str, SdblTokenKind)] = &[
        ("Булево", TypeBoolean),
        ("boolean", TypeBoolean),
        ("BoOLeAn", TypeBoolean),
        ("Число", TypeNumber),
        ("number", TypeNumber),
        ("Строка", TypeString),
        ("string", TypeString),
        ("Дата", TypeDate),
        ("date", TypeDate),
        ("Неопределено", LitUndefined),
        ("undefined", LitUndefined),
        ("Декада", PeriodTenDays),
        ("tendays", PeriodTenDays),
        ("Полугодие", PeriodHalfYear),
        ("halfyear", PeriodHalfYear),
    ];
    for (src, expected) in cases {
        assert_eq!(single_kind(src), *expected, "case-insensitive mismatch on {src:?}");
    }
}

#[test]
fn cast_to_boolean_russian() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("ВЫРАЗИТЬ(X КАК БУЛЕВО)");
    assert_eq!(kinds, vec![KwCast, LParen, Ident, KwAs, TypeBoolean, RParen]);
}

#[test]
fn cast_to_number_with_precision_russian() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("ВЫРАЗИТЬ(X КАК ЧИСЛО(15, 2))");
    assert_eq!(
        kinds,
        vec![
            KwCast, LParen, Ident, KwAs, TypeNumber, LParen, Decimal, Comma, Decimal, RParen,
            RParen,
        ]
    );
}

#[test]
fn cast_to_string_with_length_russian() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("ВЫРАЗИТЬ(X КАК СТРОКА(50))");
    assert_eq!(
        kinds,
        vec![KwCast, LParen, Ident, KwAs, TypeString, LParen, Decimal, RParen, RParen]
    );
}

#[test]
fn cast_to_date_english() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("CAST(X AS DATE)");
    assert_eq!(kinds, vec![KwCast, LParen, Ident, KwAs, TypeDate, RParen]);
}

#[test]
fn type_function_with_primitive_argument_russian() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("ТИП(Число)");
    assert_eq!(kinds, vec![KwType, LParen, TypeNumber, RParen]);
}

#[test]
fn type_function_with_primitive_argument_english() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("TYPE(Boolean)");
    assert_eq!(kinds, vec![KwType, LParen, TypeBoolean, RParen]);
}

#[test]
fn undefined_predicate_position_english() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("WHERE X = UNDEFINED OR Y <> NULL");
    assert_eq!(kinds, vec![KwWhere, Ident, Eq, LitUndefined, OpOr, Ident, Neq, LitNull]);
}

#[test]
fn totals_by_periods_tendays_english() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("TOTALS SUM(X) BY PERIODS(TENDAYS, &S, &E)");
    assert_eq!(
        kinds,
        vec![
            KwTotals,
            FnSum,
            LParen,
            Ident,
            RParen,
            KwOnOrBy,
            KwPeriods,
            LParen,
            PeriodTenDays,
            Comma,
            Parameter,
            Comma,
            Parameter,
            RParen,
        ]
    );
}

#[test]
fn totals_by_periods_halfyear_english() {
    use SdblTokenKind::*;
    let kinds = significant_kinds("TOTALS SUM(X) BY PERIODS(HALFYEAR, &S, &E)");
    assert_eq!(
        kinds,
        vec![
            KwTotals,
            FnSum,
            LParen,
            Ident,
            RParen,
            KwOnOrBy,
            KwPeriods,
            LParen,
            PeriodHalfYear,
            Comma,
            Parameter,
            Comma,
            Parameter,
            RParen,
        ]
    );
}

#[test]
fn keyword_prefix_idents_lex_as_ident() {
    use SdblTokenKind::*;
    let cases: &[&str] = &[
        "БУЛЕВОТЕСТ",
        "numberOfItems",
        "СТРОКАТАБЛИЦЫ",
        "DATEStamp",
        "UNDEFINEDValue",
        "ДЕКАДАРАСЧЕТ",
        "HALFYEARS",
    ];
    for src in cases {
        assert_eq!(single_kind(src), Ident, "expected Ident for {src:?}");
    }
}
