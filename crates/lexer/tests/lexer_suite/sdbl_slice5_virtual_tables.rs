//! Acceptance suite for the SDBL virtual-table vocabulary.
//!
//! Every canonical-form test pins one spelling of one virtual-table
//! suffix against the bilingual headline of its article in the
//! 1C:Enterprise syntax assistant, section «Работа с запросами →
//! Таблицы запросов».
//! Provenance and per-variant sources: `docs/legal/sdbl-clean-room-slice5.md`.

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

/// The external-source root and every suffix whose canonical spelling
/// exists in both languages.
const BILINGUAL: &[(&str, &str, SdblTokenKind)] = &[
    ("ВнешнийИсточникДанных", "ExternalDataSource", SdblTokenKind::MdoExternalDataSource),
    ("СрезПервых", "SliceFirst", SdblTokenKind::VtSliceFirst),
    ("СрезПоследних", "SliceLast", SdblTokenKind::VtSliceLast),
    ("Остатки", "Balance", SdblTokenKind::VtBalance),
    ("Обороты", "Turnovers", SdblTokenKind::VtTurnovers),
    ("ОстаткиИОбороты", "BalanceAndTurnovers", SdblTokenKind::VtBalanceAndTurnovers),
    ("ОборотыДтКт", "DrCrTurnovers", SdblTokenKind::VtDrCrTurnovers),
    ("Субконто", "ExtDimensions", SdblTokenKind::VtExtDimensions),
    ("ДвиженияССубконто", "RecordsWithExtDimensions", SdblTokenKind::VtRecordsWithExtDimensions),
    ("ДанныеГрафика", "ScheduleData", SdblTokenKind::VtScheduleData),
    (
        "ФактическийПериодДействия",
        "AdjustedEffectivePeriod",
        SdblTokenKind::VtAdjustedEffectivePeriod,
    ),
    ("Границы", "Boundaries", SdblTokenKind::VtBoundaries),
    ("Точки", "Points", SdblTokenKind::VtPoints),
    ("ЗадачиПоИсполнителю", "TasksByPerformer", SdblTokenKind::VtTasksByPerformer),
    ("Таблица", "Table", SdblTokenKind::VtTable),
    ("Куб", "Cube", SdblTokenKind::VtCube),
    ("ТаблицаИзмерения", "DimensionTable", SdblTokenKind::VtDimensionTable),
];

// --- Canonical forms --------------------------------------------------

#[test]
fn bilingual_suffixes_russian_canonical() {
    for (ru, _, kind) in BILINGUAL {
        assert_eq!(single_kind(ru), *kind, "russian spelling {ru:?}");
    }
}

#[test]
fn bilingual_suffixes_english_canonical() {
    for (_, en, kind) in BILINGUAL {
        assert_eq!(single_kind(en), *kind, "english spelling {en:?}");
    }
}

#[test]
fn change_registration_suffix_is_english_only() {
    assert_eq!(single_kind("Changes"), SdblTokenKind::VtChanges);
}

#[test]
fn suffixes_are_case_insensitive() {
    for (ru, en, kind) in BILINGUAL {
        assert_eq!(single_kind(&ru.to_lowercase()), *kind, "lowercase {ru:?}");
        assert_eq!(single_kind(&ru.to_uppercase()), *kind, "uppercase {ru:?}");
        assert_eq!(single_kind(&en.to_lowercase()), *kind, "lowercase {en:?}");
        assert_eq!(single_kind(&en.to_uppercase()), *kind, "uppercase {en:?}");
    }
    assert_eq!(single_kind("CHANGES"), SdblTokenKind::VtChanges);
    assert_eq!(single_kind("changes"), SdblTokenKind::VtChanges);
}

#[test]
fn every_suffix_spelling_maps_to_its_own_kind() {
    let mut kinds: HashSet<_> = BILINGUAL.iter().map(|(_, _, k)| *k).collect();
    kinds.insert(SdblTokenKind::VtChanges);
    assert_eq!(kinds.len(), 18, "no two spellings may collapse onto one kind");
}

// --- Longest-match guards ---------------------------------------------

#[test]
fn balance_and_turnovers_is_not_shadowed_by_balance() {
    assert_eq!(single_kind("ОстаткиИОбороты"), SdblTokenKind::VtBalanceAndTurnovers);
    assert_eq!(single_kind("BalanceAndTurnovers"), SdblTokenKind::VtBalanceAndTurnovers);
    assert_eq!(single_kind("Остатки"), SdblTokenKind::VtBalance);
    assert_eq!(single_kind("Balance"), SdblTokenKind::VtBalance);
}

#[test]
fn dr_cr_turnovers_is_not_shadowed_by_turnovers() {
    assert_eq!(single_kind("ОборотыДтКт"), SdblTokenKind::VtDrCrTurnovers);
    assert_eq!(single_kind("Обороты"), SdblTokenKind::VtTurnovers);
}

#[test]
fn dimension_table_is_not_shadowed_by_table() {
    assert_eq!(single_kind("ТаблицаИзмерения"), SdblTokenKind::VtDimensionTable);
    assert_eq!(single_kind("DimensionTable"), SdblTokenKind::VtDimensionTable);
    assert_eq!(single_kind("Таблица"), SdblTokenKind::VtTable);
    assert_eq!(single_kind("Table"), SdblTokenKind::VtTable);
}

#[test]
fn the_empty_table_keyword_is_not_disturbed_by_the_table_suffix() {
    assert_eq!(single_kind("ПУСТАЯТАБЛИЦА"), SdblTokenKind::FnEmptyTable);
    assert_eq!(single_kind("EMPTYTABLE"), SdblTokenKind::FnEmptyTable);
}

#[test]
fn ext_dimensions_and_records_with_ext_dimensions_are_distinct() {
    assert_eq!(single_kind("Субконто"), SdblTokenKind::VtExtDimensions);
    assert_eq!(single_kind("ДвиженияССубконто"), SdblTokenKind::VtRecordsWithExtDimensions);
}

// --- Spellings shared with other vocabularies -------------------------

#[test]
fn the_russian_change_spelling_stays_the_update_keyword() {
    assert_eq!(single_kind("Изменения"), SdblTokenKind::KwUpdate);
    assert_eq!(single_kind("ИЗМЕНЕНИЯ"), SdblTokenKind::KwUpdate);
    assert_eq!(single_kind("UPDATE"), SdblTokenKind::KwUpdate);
}

#[test]
fn route_point_stays_an_identifier() {
    assert_eq!(single_kind("ТочкаМаршрута"), SdblTokenKind::Ident);
    assert_eq!(single_kind("RoutePoint"), SdblTokenKind::Ident);
}

#[test]
fn the_base_register_prefix_is_not_a_token() {
    assert_eq!(
        significant_kinds("РегистрРасчета.Начисления.БазаОсновныеНачисления"),
        vec![
            SdblTokenKind::MdoCalculationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ],
        "the glued base-register name stays one identifier"
    );
    assert_eq!(single_kind("БазаОсновныеНачисления"), SdblTokenKind::Ident);
    assert_eq!(single_kind("BaseMainAccruals"), SdblTokenKind::Ident);
}

#[test]
fn identifier_starting_with_a_suffix_stays_ident() {
    for name in [
        "КубическийМетр",
        "ТаблицаТоваров",
        "ГраницыДиапазона",
        "ТочкиРоста",
        "ОстаткиТоваров",
        "Changeset",
        "Tableau",
        "Cubed",
        "Pointer",
    ] {
        assert_eq!(single_kind(name), SdblTokenKind::Ident, "{name:?}");
    }
}

// --- The lexer is not dot-sensitive -----------------------------------

#[test]
fn a_suffix_carries_the_same_token_wherever_it_stands() {
    assert_eq!(
        single_kind("Остатки"),
        SdblTokenKind::VtBalance,
        "standing alone, outside any dotted path"
    );
    let in_path = significant_kinds("РегистрНакопления.Товары.Остатки");
    assert_eq!(
        in_path.last().copied(),
        Some(SdblTokenKind::VtBalance),
        "the lexer keeps no record of the preceding dots, so both readings \
         reach the parser as the same kind"
    );
}

// --- Structural integration -------------------------------------------

#[test]
fn information_register_slice_path() {
    assert_eq!(
        significant_kinds("РегистрСведений.Цены.СрезПоследних"),
        vec![
            SdblTokenKind::MdoInformationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtSliceLast,
        ]
    );
}

#[test]
fn accounting_register_ext_dimensions_path() {
    assert_eq!(
        significant_kinds("РегистрБухгалтерии.Хозрасчетный.ДвиженияССубконто"),
        vec![
            SdblTokenKind::MdoAccountingRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtRecordsWithExtDimensions,
        ]
    );
}

#[test]
fn calculation_register_schedule_data_path() {
    assert_eq!(
        significant_kinds("CalculationRegister.Accruals.ScheduleData"),
        vec![
            SdblTokenKind::MdoCalculationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtScheduleData,
        ]
    );
}

#[test]
fn external_source_table_path() {
    assert_eq!(
        significant_kinds("ВнешнийИсточникДанных.Склад.Таблица.Позиции"),
        vec![
            SdblTokenKind::MdoExternalDataSource,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtTable,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn external_source_cube_dimension_table_path() {
    assert_eq!(
        significant_kinds("ExternalDataSource.Depot.Cube.Sales.DimensionTable.Intervals"),
        vec![
            SdblTokenKind::MdoExternalDataSource,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtCube,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtDimensionTable,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
        ]
    );
}

#[test]
fn change_registration_path_reads_differently_per_language() {
    assert_eq!(
        significant_kinds("Catalog.Goods.Changes"),
        vec![
            SdblTokenKind::MdoCatalog,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtChanges,
        ]
    );
    assert_eq!(
        significant_kinds("Справочник.Товары.Изменения"),
        vec![
            SdblTokenKind::MdoCatalog,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::KwUpdate,
        ],
        "the russian spelling stays with the FOR UPDATE keyword"
    );
}

#[test]
fn the_for_update_clause_survives_the_changes_suffix() {
    assert_eq!(
        significant_kinds("ВЫБРАТЬ 1 ИЗ Т ДЛЯ ИЗМЕНЕНИЯ"),
        vec![
            SdblTokenKind::KwSelect,
            SdblTokenKind::Decimal,
            SdblTokenKind::KwFrom,
            SdblTokenKind::Ident,
            SdblTokenKind::KwFor,
            SdblTokenKind::KwUpdate,
        ]
    );
}

#[test]
fn balance_virtual_table_with_arguments() {
    assert_eq!(
        significant_kinds("РегистрНакопления.Товары.Остатки(&Дата, Склад = &Склад)"),
        vec![
            SdblTokenKind::MdoAccumulationRegister,
            SdblTokenKind::Dot,
            SdblTokenKind::Ident,
            SdblTokenKind::Dot,
            SdblTokenKind::VtBalance,
            SdblTokenKind::LParen,
            SdblTokenKind::Parameter,
            SdblTokenKind::Comma,
            SdblTokenKind::Ident,
            SdblTokenKind::Eq,
            SdblTokenKind::Parameter,
            SdblTokenKind::RParen,
        ]
    );
}
