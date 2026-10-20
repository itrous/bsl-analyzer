//! Acceptance suite for the SDBL query-function vocabulary.
//!
//! Every canonical-form test pins one spelling of one query function
//! against the 1C:Enterprise syntax assistant book «Синтаксис текста
//! запросов» — its function index, its bilingual keyword table, or the
//! bilingual headline of the function's own article.
//! Provenance and per-variant sources: `docs/legal/sdbl-clean-room-slice4.md`.

use std::collections::HashSet;

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

/// Functions whose canonical spelling exists in both languages.
const BILINGUAL: &[(&str, &str, SdblTokenKind)] = &[
    // Aggregate.
    ("СУММА", "SUM", SdblTokenKind::FnSum),
    ("СРЕДНЕЕ", "AVG", SdblTokenKind::FnAvg),
    ("МИНИМУМ", "MIN", SdblTokenKind::FnMin),
    ("МАКСИМУМ", "MAX", SdblTokenKind::FnMax),
    ("КОЛИЧЕСТВО", "COUNT", SdblTokenKind::FnCount),
    // Date.
    ("ГОД", "YEAR", SdblTokenKind::FnYear),
    ("КВАРТАЛ", "QUARTER", SdblTokenKind::FnQuarter),
    ("МЕСЯЦ", "MONTH", SdblTokenKind::FnMonth),
    ("ДЕНЬГОДА", "DAYOFYEAR", SdblTokenKind::FnDayOfYear),
    ("ДЕНЬ", "DAY", SdblTokenKind::FnDay),
    ("НЕДЕЛЯ", "WEEK", SdblTokenKind::FnWeek),
    ("ДЕНЬНЕДЕЛИ", "WEEKDAY", SdblTokenKind::FnWeekDay),
    ("ЧАС", "HOUR", SdblTokenKind::FnHour),
    ("МИНУТА", "MINUTE", SdblTokenKind::FnMinute),
    ("СЕКУНДА", "SECOND", SdblTokenKind::FnSecond),
    ("НАЧАЛОПЕРИОДА", "BEGINOFPERIOD", SdblTokenKind::FnBeginOfPeriod),
    ("КОНЕЦПЕРИОДА", "ENDOFPERIOD", SdblTokenKind::FnEndOfPeriod),
    ("ДОБАВИТЬКДАТЕ", "DATEADD", SdblTokenKind::FnDateAdd),
    ("РАЗНОСТЬДАТ", "DATEDIFF", SdblTokenKind::FnDateDiff),
    ("ДАТАВРЕМЯ", "DATETIME", SdblTokenKind::FnDateTime),
    // String.
    ("ПОДСТРОКА", "SUBSTRING", SdblTokenKind::FnSubstring),
    ("ДлинаСтроки", "StringLength", SdblTokenKind::FnStringLength),
    ("СтрНайти", "StrFind", SdblTokenKind::FnStrFind),
    ("СтрЗаменить", "StrReplace", SdblTokenKind::FnStrReplace),
    ("ВРег", "Upper", SdblTokenKind::FnUpper),
    ("НРег", "Lower", SdblTokenKind::FnLower),
    ("СокрЛП", "TrimAll", SdblTokenKind::FnTrimAll),
    ("СокрЛ", "TrimL", SdblTokenKind::FnTrimL),
    ("СокрП", "TrimR", SdblTokenKind::FnTrimR),
    // Mathematical.
    ("Окр", "Round", SdblTokenKind::FnRound),
    ("Цел", "Int", SdblTokenKind::FnInt),
    // Other.
    ("ТИПЗНАЧЕНИЯ", "VALUETYPE", SdblTokenKind::FnValueType),
    ("ПРЕДСТАВЛЕНИЕ", "PRESENTATION", SdblTokenKind::FnPresentation),
    ("ПРЕДСТАВЛЕНИЕССЫЛКИ", "REFPRESENTATION", SdblTokenKind::FnRefPresentation),
    ("ЕСТЬNULL", "ISNULL", SdblTokenKind::FnIsNull),
    ("АВТОНОМЕРЗАПИСИ", "RECORDAUTONUMBER", SdblTokenKind::FnRecordAutoNumber),
    ("СГРУППИРОВАНОПО", "GROUPEDBY", SdblTokenKind::FnGroupedBy),
    ("РАЗМЕРХРАНИМЫХДАННЫХ", "StoredDataSize", SdblTokenKind::FnStoredDataSize),
    ("ПУСТАЯТАБЛИЦА", "EMPTYTABLE", SdblTokenKind::FnEmptyTable),
    ("ПустаяСсылка", "EmptyRef", SdblTokenKind::FnEmptyRef),
    ("УНИКАЛЬНЫЙИДЕНТИФИКАТОР", "UUID", SdblTokenKind::FnUUID),
];

/// Functions the lexer accepts in Russian only, because their English
/// spelling is byte-identical to a join keyword.
const RUSSIAN_ONLY: &[(&str, SdblTokenKind)] =
    &[("Лев", SdblTokenKind::FnLeft), ("Прав", SdblTokenKind::FnRight)];

/// Mathematical functions the source writes in Latin letters in both
/// language builds.
const LATIN_ONLY: &[(&str, SdblTokenKind)] = &[
    ("Exp", SdblTokenKind::FnExp),
    ("Log10", SdblTokenKind::FnLog10),
    ("Log", SdblTokenKind::FnLog),
    ("Pow", SdblTokenKind::FnPow),
    ("Sqrt", SdblTokenKind::FnSqrt),
    ("ACos", SdblTokenKind::FnACos),
    ("ASin", SdblTokenKind::FnASin),
    ("ATan", SdblTokenKind::FnATan),
    ("Cos", SdblTokenKind::FnCos),
    ("Sin", SdblTokenKind::FnSin),
    ("Tan", SdblTokenKind::FnTan),
];

// --- Canonical forms --------------------------------------------------

#[test]
fn bilingual_functions_russian_canonical() {
    for (ru, _, kind) in BILINGUAL {
        assert_eq!(single_kind(ru), *kind, "russian spelling {ru:?}");
    }
}

#[test]
fn bilingual_functions_english_canonical() {
    for (_, en, kind) in BILINGUAL {
        assert_eq!(single_kind(en), *kind, "english spelling {en:?}");
    }
}

#[test]
fn russian_only_functions_canonical() {
    for (ru, kind) in RUSSIAN_ONLY {
        assert_eq!(single_kind(ru), *kind, "russian spelling {ru:?}");
    }
}

#[test]
fn latin_only_functions_canonical() {
    for (name, kind) in LATIN_ONLY {
        assert_eq!(single_kind(name), *kind, "spelling {name:?}");
    }
}

#[test]
fn functions_are_case_insensitive() {
    for (ru, en, kind) in BILINGUAL {
        assert_eq!(single_kind(&ru.to_lowercase()), *kind, "lowercase {ru:?}");
        assert_eq!(single_kind(&ru.to_uppercase()), *kind, "uppercase {ru:?}");
        assert_eq!(single_kind(&en.to_lowercase()), *kind, "lowercase {en:?}");
        assert_eq!(single_kind(&en.to_uppercase()), *kind, "uppercase {en:?}");
    }
    for (name, kind) in RUSSIAN_ONLY.iter().chain(LATIN_ONLY) {
        assert_eq!(single_kind(&name.to_lowercase()), *kind, "lowercase {name:?}");
        assert_eq!(single_kind(&name.to_uppercase()), *kind, "uppercase {name:?}");
    }
}

#[test]
fn every_function_spelling_maps_to_its_own_kind() {
    let mut kinds = HashSet::new();
    for (_, _, kind) in BILINGUAL {
        kinds.insert(*kind);
    }
    for (_, kind) in RUSSIAN_ONLY.iter().chain(LATIN_ONLY) {
        kinds.insert(*kind);
    }
    assert_eq!(kinds.len(), 54, "no two spellings may collapse onto one kind");
}

// --- Source conflict on the English name of РАЗНОСТЬДАТ ---------------

#[test]
fn date_diff_accepts_both_documented_english_spellings() {
    assert_eq!(single_kind("DATEDIFF"), SdblTokenKind::FnDateDiff);
    assert_eq!(single_kind("DATEDIFFERENCE"), SdblTokenKind::FnDateDiff);
}

// --- Longest-match guards ---------------------------------------------

#[test]
fn day_of_year_is_not_shadowed_by_day() {
    assert_eq!(single_kind("ДЕНЬГОДА"), SdblTokenKind::FnDayOfYear);
    assert_eq!(single_kind("DAYOFYEAR"), SdblTokenKind::FnDayOfYear);
    assert_eq!(single_kind("ДЕНЬ"), SdblTokenKind::FnDay);
    assert_eq!(single_kind("DAY"), SdblTokenKind::FnDay);
}

#[test]
fn trim_all_is_not_shadowed_by_trim_l() {
    assert_eq!(single_kind("СокрЛП"), SdblTokenKind::FnTrimAll);
    assert_eq!(single_kind("СокрЛ"), SdblTokenKind::FnTrimL);
    assert_eq!(single_kind("СокрП"), SdblTokenKind::FnTrimR);
}

#[test]
fn ref_presentation_is_not_shadowed_by_presentation() {
    assert_eq!(single_kind("ПРЕДСТАВЛЕНИЕССЫЛКИ"), SdblTokenKind::FnRefPresentation);
    assert_eq!(single_kind("ПРЕДСТАВЛЕНИЕ"), SdblTokenKind::FnPresentation);
}

#[test]
fn log10_is_not_shadowed_by_log() {
    assert_eq!(single_kind("Log10"), SdblTokenKind::FnLog10);
    assert_eq!(single_kind("Log"), SdblTokenKind::FnLog);
}

#[test]
fn grouped_by_is_not_shadowed_by_the_group_keyword() {
    assert_eq!(single_kind("GROUPEDBY"), SdblTokenKind::FnGroupedBy);
    assert_eq!(single_kind("GROUP"), SdblTokenKind::KwGroup);
    assert_eq!(single_kind("СГРУППИРОВАНОПО"), SdblTokenKind::FnGroupedBy);
    assert_eq!(single_kind("СГРУППИРОВАТЬ"), SdblTokenKind::KwGroup);
}

#[test]
fn arc_functions_are_not_shadowed_by_their_bases() {
    for (arc, base, arc_kind, base_kind) in [
        ("ACos", "Cos", SdblTokenKind::FnACos, SdblTokenKind::FnCos),
        ("ASin", "Sin", SdblTokenKind::FnASin, SdblTokenKind::FnSin),
        ("ATan", "Tan", SdblTokenKind::FnATan, SdblTokenKind::FnTan),
    ] {
        assert_eq!(single_kind(arc), arc_kind, "{arc:?}");
        assert_eq!(single_kind(base), base_kind, "{base:?}");
    }
}

// --- Spellings shared with other vocabularies -------------------------

#[test]
fn left_and_right_functions_do_not_disturb_the_join_keywords() {
    assert_eq!(single_kind("ЛЕВ"), SdblTokenKind::FnLeft);
    assert_eq!(single_kind("ПРАВ"), SdblTokenKind::FnRight);
    assert_eq!(single_kind("ЛЕВОЕ"), SdblTokenKind::KwLeft);
    assert_eq!(single_kind("ПРАВОЕ"), SdblTokenKind::KwRight);
}

#[test]
fn english_left_and_right_stay_join_keywords() {
    assert_eq!(single_kind("LEFT"), SdblTokenKind::KwLeft);
    assert_eq!(single_kind("RIGHT"), SdblTokenKind::KwRight);
}

#[test]
fn date_is_a_type_name_not_a_function() {
    assert_eq!(single_kind("ДАТА"), SdblTokenKind::TypeDate);
    assert_eq!(single_kind("Дата"), SdblTokenKind::TypeDate);
    assert_eq!(single_kind("DATE"), SdblTokenKind::TypeDate);
}

#[test]
fn string_function_shares_the_type_name_spelling() {
    assert_eq!(single_kind("СТРОКА"), SdblTokenKind::TypeString);
    assert_eq!(single_kind("STRING"), SdblTokenKind::TypeString);
}

#[test]
fn identifier_starting_with_a_function_stays_ident() {
    for name in [
        "Годовой",
        "Дневник",
        "ЛЕВЫЙ",
        "ПРАВЫЙ",
        "Sinus",
        "Cosinus",
        "Exponent",
        "Counter",
        "Intel",
        "Powered",
        "Rounded",
    ] {
        assert_eq!(single_kind(name), SdblTokenKind::Ident, "{name:?}");
    }
}

// --- Structural integration -------------------------------------------

#[test]
fn aggregates_in_a_russian_selection_list() {
    let kinds = significant_kinds("ВЫБРАТЬ СУММА(Цена), КОЛИЧЕСТВО(*) ИЗ Продажи");
    assert_eq!(
        kinds,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::FnSum,
            SdblTokenKind::LParen,
            SdblTokenKind::Ident,
            SdblTokenKind::RParen,
            SdblTokenKind::Comma,
            SdblTokenKind::FnCount,
            SdblTokenKind::LParen,
            SdblTokenKind::Star,
            SdblTokenKind::RParen,
            SdblTokenKind::KwFrom,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn a_date_function_doubles_as_a_period_specifier() {
    let kinds = significant_kinds("НАЧАЛОПЕРИОДА(Момент, МЕСЯЦ)");
    assert_eq!(
        kinds,
        vec![
            SdblTokenKind::FnBeginOfPeriod,
            SdblTokenKind::LParen,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::FnMonth,
            SdblTokenKind::RParen,
        ],
        "the period specifier is the same token as the extraction function"
    );
}

#[test]
fn empty_table_keyword_in_the_selection_list() {
    let kinds = significant_kinds("ВЫБРАТЬ ПУСТАЯТАБЛИЦА.(Ном, Тов)");
    assert_eq!(
        kinds,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::FnEmptyTable,
            SdblTokenKind::Dot,
            SdblTokenKind::LParen,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::Ident,
            SdblTokenKind::RParen,
        ]
    );
}

#[test]
fn empty_ref_selector_inside_a_value_literal() {
    let kinds = significant_kinds("ЗНАЧЕНИЕ(Справочник.Города.ПустаяСсылка)");
    assert_eq!(
        kinds,
        vec![
            SdblTokenKind::KwValue,
            SdblTokenKind::LParen,
            SdblTokenKind::MdoCatalog,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::FnEmptyRef,
            SdblTokenKind::RParen,
        ]
    );
}

#[test]
fn record_autonumber_in_a_temporary_table_query() {
    let kinds = significant_kinds("ВЫБРАТЬ АВТОНОМЕРЗАПИСИ() КАК Ключ ПОМЕСТИТЬ Оплаты");
    assert_eq!(
        kinds,
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::FnRecordAutoNumber,
            SdblTokenKind::LParen,
            SdblTokenKind::RParen,
            SdblTokenKind::KwAs,
            SdblTokenKind::Ident,
            SdblTokenKind::KwInto,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn the_left_function_and_the_left_join_in_one_query() {
    let kinds = significant_kinds(
        "ВЫБРАТЬ ЛЕВ(Тов.Имя, 3) ИЗ Справочник.Товары КАК Тов \
         ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Цены КАК Цен ПО Тов.Код = Цен.Код",
    );
    assert_eq!(kinds[1], SdblTokenKind::FnLeft, "the function opens the selection list");
    assert!(kinds.contains(&SdblTokenKind::KwLeft), "the join keyword survives in the same query");
    assert!(!kinds.contains(&SdblTokenKind::KwRight));
}

#[test]
fn the_date_literal_nests_inside_a_date_function() {
    let kinds = significant_kinds("РАЗНОСТЬДАТ(Момент, ДАТАВРЕМЯ(2024, 1, 1), ЧАС)");
    assert_eq!(kinds[0], SdblTokenKind::FnDateDiff);
    assert_eq!(kinds[4], SdblTokenKind::FnDateTime);
    assert_eq!(kinds[kinds.len() - 2], SdblTokenKind::FnHour);
}
