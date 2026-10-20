//! Acceptance suite for the SDBL metadata-object table vocabulary.
//!
//! Every canonical-form test pins one spelling of one query table root
//! against the bilingual headline of its article in the 1C:Enterprise
//! syntax assistant, section «Работа с запросами → Таблицы запросов».
//! Provenance and per-variant sources: `docs/legal/sdbl-clean-room-slice3b.md`.

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

// --- Reference-type roots, Russian canonical spellings ----------------

#[test]
fn catalog_russian_canonical() {
    assert_eq!(single_kind("Справочник"), SdblTokenKind::MdoCatalog);
}

#[test]
fn document_russian_canonical() {
    assert_eq!(single_kind("Документ"), SdblTokenKind::MdoDocument);
}

#[test]
fn enum_russian_canonical() {
    assert_eq!(single_kind("Перечисление"), SdblTokenKind::MdoEnum);
}

#[test]
fn chart_of_characteristic_types_russian_canonical() {
    assert_eq!(single_kind("ПланВидовХарактеристик"), SdblTokenKind::MdoChartOfCharacteristicTypes);
}

#[test]
fn chart_of_accounts_russian_canonical() {
    assert_eq!(single_kind("ПланСчетов"), SdblTokenKind::MdoChartOfAccounts);
}

#[test]
fn chart_of_calculation_types_russian_canonical() {
    assert_eq!(single_kind("ПланВидовРасчета"), SdblTokenKind::MdoChartOfCalculationTypes);
}

#[test]
fn exchange_plan_russian_canonical() {
    assert_eq!(single_kind("ПланОбмена"), SdblTokenKind::MdoExchangePlan);
}

#[test]
fn business_process_russian_canonical() {
    assert_eq!(single_kind("БизнесПроцесс"), SdblTokenKind::MdoBusinessProcess);
}

#[test]
fn task_russian_canonical() {
    assert_eq!(single_kind("Задача"), SdblTokenKind::MdoTask);
}

// --- Reference-type roots, English canonical spellings ----------------

#[test]
fn catalog_english_canonical() {
    assert_eq!(single_kind("Catalog"), SdblTokenKind::MdoCatalog);
}

#[test]
fn document_english_canonical() {
    assert_eq!(single_kind("Document"), SdblTokenKind::MdoDocument);
}

#[test]
fn enum_english_canonical() {
    assert_eq!(single_kind("Enum"), SdblTokenKind::MdoEnum);
}

#[test]
fn chart_of_characteristic_types_english_canonical() {
    assert_eq!(
        single_kind("ChartOfCharacteristicTypes"),
        SdblTokenKind::MdoChartOfCharacteristicTypes
    );
}

#[test]
fn chart_of_accounts_english_canonical() {
    assert_eq!(single_kind("ChartOfAccounts"), SdblTokenKind::MdoChartOfAccounts);
}

#[test]
fn chart_of_calculation_types_english_canonical() {
    assert_eq!(single_kind("ChartOfCalculationTypes"), SdblTokenKind::MdoChartOfCalculationTypes);
}

#[test]
fn exchange_plan_english_canonical() {
    assert_eq!(single_kind("ExchangePlan"), SdblTokenKind::MdoExchangePlan);
}

#[test]
fn business_process_english_canonical() {
    assert_eq!(single_kind("BusinessProcess"), SdblTokenKind::MdoBusinessProcess);
}

#[test]
fn task_english_canonical() {
    assert_eq!(single_kind("Task"), SdblTokenKind::MdoTask);
}

// --- Register roots, both languages -----------------------------------

#[test]
fn information_register_russian_canonical() {
    assert_eq!(single_kind("РегистрСведений"), SdblTokenKind::MdoInformationRegister);
}

#[test]
fn information_register_english_canonical() {
    assert_eq!(single_kind("InformationRegister"), SdblTokenKind::MdoInformationRegister);
}

#[test]
fn accumulation_register_russian_canonical() {
    assert_eq!(single_kind("РегистрНакопления"), SdblTokenKind::MdoAccumulationRegister);
}

#[test]
fn accumulation_register_english_canonical() {
    assert_eq!(single_kind("AccumulationRegister"), SdblTokenKind::MdoAccumulationRegister);
}

#[test]
fn accounting_register_russian_canonical() {
    assert_eq!(single_kind("РегистрБухгалтерии"), SdblTokenKind::MdoAccountingRegister);
}

#[test]
fn accounting_register_english_canonical() {
    assert_eq!(single_kind("AccountingRegister"), SdblTokenKind::MdoAccountingRegister);
}

#[test]
fn calculation_register_russian_canonical() {
    assert_eq!(single_kind("РегистрРасчета"), SdblTokenKind::MdoCalculationRegister);
}

#[test]
fn calculation_register_english_canonical() {
    assert_eq!(single_kind("CalculationRegister"), SdblTokenKind::MdoCalculationRegister);
}

// --- Other real-table roots, both languages ---------------------------

#[test]
fn document_journal_russian_canonical() {
    assert_eq!(single_kind("ЖурналДокументов"), SdblTokenKind::MdoDocumentJournal);
}

#[test]
fn document_journal_english_canonical() {
    assert_eq!(single_kind("DocumentJournal"), SdblTokenKind::MdoDocumentJournal);
}

#[test]
fn constant_russian_canonical() {
    assert_eq!(single_kind("Константа"), SdblTokenKind::MdoConstant);
}

#[test]
fn constant_english_canonical() {
    assert_eq!(single_kind("Constant"), SdblTokenKind::MdoConstant);
}

#[test]
fn constants_russian_canonical() {
    assert_eq!(single_kind("Константы"), SdblTokenKind::MdoConstants);
}

#[test]
fn constants_english_canonical() {
    assert_eq!(single_kind("Constants"), SdblTokenKind::MdoConstants);
}

#[test]
fn sequence_russian_canonical() {
    assert_eq!(single_kind("Последовательность"), SdblTokenKind::MdoSequence);
}

#[test]
fn sequence_english_canonical() {
    assert_eq!(single_kind("Sequence"), SdblTokenKind::MdoSequence);
}

#[test]
fn filter_criterion_russian_canonical() {
    assert_eq!(single_kind("КритерийОтбора"), SdblTokenKind::MdoFilterCriterion);
}

#[test]
fn filter_criterion_english_canonical() {
    assert_eq!(single_kind("FilterCriterion"), SdblTokenKind::MdoFilterCriterion);
}

// --- Case insensitivity ------------------------------------------------

/// Глава 8 §8.4.5: «Регистр букв (строчные или заглавные) при написании
/// не имеет значения».
#[test]
fn roots_are_case_insensitive() {
    for (src, expected) in [
        ("СПРАВОЧНИК", SdblTokenKind::MdoCatalog),
        ("справочник", SdblTokenKind::MdoCatalog),
        ("СпРаВоЧнИк", SdblTokenKind::MdoCatalog),
        ("CATALOG", SdblTokenKind::MdoCatalog),
        ("catalog", SdblTokenKind::MdoCatalog),
        ("cAtAlOg", SdblTokenKind::MdoCatalog),
        ("журналдокументов", SdblTokenKind::MdoDocumentJournal),
        ("DOCUMENTJOURNAL", SdblTokenKind::MdoDocumentJournal),
        ("КОНСТАНТЫ", SdblTokenKind::MdoConstants),
        ("constants", SdblTokenKind::MdoConstants),
        ("критерийотбора", SdblTokenKind::MdoFilterCriterion),
        ("FILTERCRITERION", SdblTokenKind::MdoFilterCriterion),
    ] {
        assert_eq!(single_kind(src), expected, "case folding failed for {src:?}");
    }
}

// --- Longest-match guards ----------------------------------------------

/// Both spellings of the document root are proper prefixes of the
/// document-journal root, in both languages. Longest match must win, or
/// `ЖурналДокументов` would be unreachable in English and the English
/// journal root would split into `Document` + `Journal`.
#[test]
fn document_journal_is_not_shadowed_by_document() {
    assert_eq!(single_kind("DocumentJournal"), SdblTokenKind::MdoDocumentJournal);
    assert_eq!(
        significant_kinds("Document.D DocumentJournal.J"),
        vec![
            SdblTokenKind::MdoDocument,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::MdoDocumentJournal,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

/// The aggregate constants table and the single-constant table differ by
/// their final letter only, in both languages.
#[test]
fn constants_and_constant_are_distinct_roots() {
    assert_eq!(
        significant_kinds("Константа.Организация Константы"),
        vec![
            SdblTokenKind::MdoConstant,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::MdoConstants,
        ]
    );
    assert_eq!(
        significant_kinds("Constant.Org Constants"),
        vec![
            SdblTokenKind::MdoConstant,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::MdoConstants,
        ]
    );
}

/// A root spelling that merely starts an identifier stays an identifier:
/// `Ident` matches longer, and a longer match wins outright.
#[test]
fn identifier_starting_with_a_root_stays_ident() {
    for src in [
        "СправочникТоваров",
        "CatalogOfThings",
        "ДокументооборотСсылка",
        "DocumentJournalEntry",
        "КонстантыОрганизации",
        "ConstantsRegistry",
    ] {
        assert_eq!(single_kind(src), SdblTokenKind::Ident, "{src:?} should be Ident");
    }
}

/// Every claimed root maps to its own token kind — no two share one.
#[test]
fn all_eighteen_roots_are_distinct_kinds() {
    let roots = [
        "Справочник",
        "Документ",
        "ЖурналДокументов",
        "РегистрСведений",
        "РегистрНакопления",
        "РегистрБухгалтерии",
        "РегистрРасчета",
        "ПланСчетов",
        "ПланВидовРасчета",
        "ПланВидовХарактеристик",
        "ПланОбмена",
        "Перечисление",
        "БизнесПроцесс",
        "Задача",
        "Константа",
        "Константы",
        "Последовательность",
        "КритерийОтбора",
    ];
    let mut kinds: Vec<_> = roots.iter().map(|r| single_kind(r)).collect();
    assert_eq!(kinds.len(), 18);
    kinds.sort_by_key(|k| format!("{k:?}"));
    kinds.dedup();
    assert_eq!(kinds.len(), 18, "two roots collapsed to the same token kind");
}

// --- Structural integration --------------------------------------------

#[test]
fn root_in_from_clause_russian() {
    assert_eq!(
        significant_kinds("ВЫБРАТЬ 1 ИЗ Справочник.Товары"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwFrom,
            SdblTokenKind::MdoCatalog,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

/// The aggregate constants table is a complete source reference on its
/// own — no dot, no name part.
#[test]
fn constants_needs_no_name_part() {
    assert_eq!(
        significant_kinds("ВЫБРАТЬ 1 ИЗ Константы"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwFrom,
            SdblTokenKind::MdoConstants,
        ]
    );
}

#[test]
fn register_family_in_one_source_list() {
    assert_eq!(
        significant_kinds(
            "ИЗ РегистрСведений.Цены, РегистрНакопления.Товары, \
             РегистрБухгалтерии.Хозрасчетный, РегистрРасчета.Начисления"
        ),
        vec![
            SdblTokenKind::KwFrom,
            SdblTokenKind::MdoInformationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoAccumulationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoAccountingRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoCalculationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

/// Глава 8 §8.4.4 gives this exact predefined-value form.
#[test]
fn enum_root_inside_value_literal() {
    assert_eq!(
        significant_kinds("ЗНАЧЕНИЕ(Перечисление.ВидыТоваров.Услуга)"),
        vec![
            SdblTokenKind::KwValue,
            SdblTokenKind::LParen,
            SdblTokenKind::MdoEnum,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::RParen,
        ]
    );
}

/// The four roots added by this slice, each in a source position, in
/// both languages.
#[test]
fn added_roots_in_source_position_bilingual() {
    assert_eq!(
        significant_kinds("ИЗ ЖурналДокументов.Ж, ПланОбмена.П, КритерийОтбора.К"),
        vec![
            SdblTokenKind::KwFrom,
            SdblTokenKind::MdoDocumentJournal,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoExchangePlan,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoFilterCriterion,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
    assert_eq!(
        significant_kinds("FROM DocumentJournal.J, ExchangePlan.E, FilterCriterion.F"),
        vec![
            SdblTokenKind::KwFrom,
            SdblTokenKind::MdoDocumentJournal,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoExchangePlan,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Comma,
            SdblTokenKind::MdoFilterCriterion,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

// --- Error fallback -----------------------------------------------------

#[test]
fn unmatched_byte_becomes_error_token() {
    assert_eq!(single_kind("@"), SdblTokenKind::Error);
}

/// The offending span survives tokenisation at its own offset, and the
/// tokens around it are unaffected — that is the whole point of emitting
/// it instead of dropping it.
#[test]
fn error_token_preserves_span_and_neighbours() {
    let toks = tokenize_sdbl("ВЫБРАТЬ @ ИЗ Т");
    let err: Vec<_> = toks.iter().filter(|t| t.kind == SdblTokenKind::Error).collect();
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].text.as_str(), "@");
    assert_eq!(err[0].offset, "ВЫБРАТЬ ".len());

    assert_eq!(
        significant_kinds("ВЫБРАТЬ @ ИЗ Т"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Error,
            SdblTokenKind::KwFrom,
            SdblTokenKind::Ident,
        ]
    );
}

/// Tokenisation covers the whole input: concatenating every token's text
/// reproduces the source byte for byte, unmatched bytes included.
#[test]
fn token_stream_covers_the_whole_input() {
    for src in ["ВЫБРАТЬ @ ИЗ Т", "@@@", "Справочник.Т @ ?", ""] {
        let joined: String = tokenize_sdbl(src).iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, src, "token stream lost bytes for {src:?}");
    }
}
