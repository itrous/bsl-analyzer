//! End-to-end check of the optional-join-side diagnostic through a BSL module.
//!
//! The query lives in a BSL string literal; the file goes through the production path
//! (literal recognition, query extraction, lowering, the IDE adapter) and only the final
//! file diagnostics are inspected. Sources: ITS pubqlang «Внутреннее соединение», «Левое
//! внешнее соединение» (fields of the unmatched side are NULL) and metod8dev «Использование
//! функции ЕСТЬNULL()». The diagnostic range — the field reference projected into the BSL
//! file — is the local adapter contract.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, MetadataOverride, Severity};
use vfs::{FileId, FileSet, VfsPath};

const CODE: DiagnosticCode = DiagnosticCode::FieldsFromJoinsWithoutIsNull;

/// Every `FieldsFromJoinsWithoutIsNull` diagnostic of the module under `config`.
fn diagnostics(source: &str, config: &DiagnosticsConfig) -> Vec<Diagnostic> {
    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_file_text(file_id, source);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    ide_diagnostics::file_diagnostics(&db, file_id, config)
        .into_iter()
        .filter(|d| d.code == CODE)
        .collect()
}

fn text_under<'a>(source: &'a str, diagnostic: &Diagnostic) -> &'a str {
    &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
}

fn rule_enabled() -> DiagnosticsConfig {
    DiagnosticsConfig { enabled: vec![CODE], ..DiagnosticsConfig::default() }
}

/// The texts under every `FieldsFromJoinsWithoutIsNull` diagnostic of the module.
fn reported(source: &str) -> Vec<String> {
    diagnostics(source, &rule_enabled()).iter().map(|d| text_under(source, d).to_string()).collect()
}

/// The texts under the diagnostics, sorted: the contract is which fields are reported and how
/// often, not the order of the reports.
fn reported_sorted(source: &str) -> Vec<String> {
    let mut texts = reported(source);
    texts.sort();
    texts
}

fn module(query: &str) -> String {
    format!(
        "Процедура Остатки()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query}\";\n    Выборка = Запрос.Выполнить().Выбрать();\nКонецПроцедуры\n"
    )
}

/// Positive control: the same path reports a field of the optional side used as a value,
/// at the field reference inside the BSL literal.
#[test]
fn an_unprotected_field_of_the_left_joined_side_is_reported_at_its_reference() {
    let source = module(
        "ВЫБРАТЬ Товары.Наименование КАК Наименование, Цены.Цена КАК Цена ИЗ Справочник.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Товары.Ссылка",
    );
    assert_eq!(reported(&source), vec!["Цены.Цена"]);
}

/// A table joined with ВНУТРЕННЕЕ is never NULL, even when a left join hangs off it:
/// selecting its field is not reported. The left-joined side is used only through
/// ЕСТЬNULL.
#[test]
fn a_field_of_an_inner_joined_source_with_a_nested_left_join_is_not_reported() {
    let source = module(
        "ВЫБРАТЬ Заказ.Номер КАК Номер, Состав.Количество КАК Количество, ЕСТЬNULL(Цены.Цена, 0) КАК Цена ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар ПО Состав.Ссылка = Заказ.Ссылка",
    );
    assert_eq!(reported(&source), Vec::<String>::new());
}

/// The same nested form with the optional side used unprotected is still reported,
/// so the negative case above is not silent because the nesting hides the join.
#[test]
fn the_nested_left_join_itself_is_still_checked() {
    let source = module(
        "ВЫБРАТЬ Состав.Количество КАК Количество, Цены.Цена КАК Цена ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар ПО Состав.Ссылка = Заказ.Ссылка",
    );
    assert_eq!(reported(&source), vec!["Цены.Цена"]);
}

/// The wording of a `ДанныеГрафика` value without NULL handling, written out here and not
/// taken from the constant under test.
const SCHEDULE_DATA_MESSAGE: &str = "Поле данных графика может быть NULL, если график за период не заполнен: добавьте проверку через ЕСТЬ NULL или функцию ЕСТЬNULL";

/// The wording of an optional join side's field.
const JOIN_MESSAGE: &str = "Для полей из ЛЕВОГО СОЕДИНЕНИЯ добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ";

const SCHEDULE_FIELD: &str = "ДанныеГрафика.ЧасыФактическийПериодДействия";

const SCHEDULE_SOURCE: &str =
    "РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК ДанныеГрафика";

/// The module of the issue: the schedule value is multiplied without ЕСТЬNULL.
const UNPROTECTED_PROCEDURE: &str = r#"Процедура ЗначенияГрафикаБезЕстьNull()
	Запрос = Новый Запрос;
	Запрос.Текст =
	"ВЫБРАТЬ
	|	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	|	ДанныеГрафика.ЧасыФактическийПериодДействия * 100 КАК Сумма
	|ИЗ
	|	РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК ДанныеГрафика";
	Запрос.УстановитьПараметр("Регистратор", Ссылка);
	Запрос.Выполнить();
КонецПроцедуры
"#;

/// The module of the issue: the same value inside ЕСТЬNULL.
const PROTECTED_PROCEDURE: &str = r#"Процедура ДанныеГрафикаКакУЭкзаменатора()
	Запрос = Новый Запрос;
	Запрос.Текст =
	"ВЫБРАТЬ
	|	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	|	ЕСТЬNULL(ДанныеГрафика.ЧасыФактическийПериодДействия, 0) КАК Часы
	|ИЗ
	|	РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК ДанныеГрафика";
	Запрос.УстановитьПараметр("Регистратор", Ссылка);
	Запрос.Выполнить();
КонецПроцедуры
"#;

/// A schedule value read without ЕСТЬNULL is reported at the field alone: not at the other
/// fields of the schedule table, not at the surrounding expression, and with the schedule
/// wording, which offers no inner join. One `enabled` entry is all it takes.
#[test]
fn schedule_data_value_without_isnull_is_reported_on_the_field() {
    let found = diagnostics(UNPROTECTED_PROCEDURE, &rule_enabled());

    assert_eq!(found.len(), 1, "{found:?}");
    let found = &found[0];
    assert_eq!(text_under(UNPROTECTED_PROCEDURE, found), SCHEDULE_FIELD);
    assert_eq!(found.message, SCHEDULE_DATA_MESSAGE);
    assert_eq!(found.message, sdbl_hir::SCHEDULE_DATA_NULL_MESSAGE);
    assert_eq!(found.severity, Severity::Warning);
    assert!(found.fixes.is_empty(), "{:?}", found.fixes);
}

/// The value inside ЕСТЬNULL is protected. The control is the same module with the wrapper
/// taken off: it is reported, so the silence comes from the wrapper.
#[test]
fn schedule_data_isnull_wrapped_value_is_not_reported() {
    assert_eq!(reported(PROTECTED_PROCEDURE), Vec::<String>::new());

    let unwrapped = PROTECTED_PROCEDURE
        .replace("ЕСТЬNULL(ДанныеГрафика.ЧасыФактическийПериодДействия, 0)", SCHEDULE_FIELD);
    assert_ne!(unwrapped, PROTECTED_PROCEDURE, "the control must change the module");
    assert_eq!(reported(&unwrapped), vec![SCHEDULE_FIELD]);
}

/// The period fields of the register itself are not schedule values: without a resource name
/// in front, nothing is reported. The control is the same query with one prefixed value.
#[test]
fn schedule_data_register_period_fields_are_not_reported() {
    let query = |extra: &str| {
        module(&format!(
            "ВЫБРАТЬ ДанныеГрафика.ПериодРегистрации КАК А, ДанныеГрафика.ПериодДействия КАК Б, ДанныеГрафика.ПериодДействияНачало КАК В, ДанныеГрафика.ПериодДействияКонец КАК Г, ДанныеГрафика.БазовыйПериодНачало КАК Д, ДанныеГрафика.БазовыйПериодКонец КАК Е{extra} ИЗ {SCHEDULE_SOURCE}"
        ))
    };

    assert_eq!(reported(&query("")), Vec::<String>::new());
    assert_eq!(
        reported(&query(", ДанныеГрафика.ЧасыПериодДействия КАК Ж")),
        vec!["ДанныеГрафика.ЧасыПериодДействия"]
    );
}

/// `ФактическийПериодДействия` is another virtual table of the register, not the schedule
/// data: its source named `ДанныеГрафика` is not classified by alias, by path tail or by the
/// field suffix. The control is the same query over the schedule table.
#[test]
fn adjusted_effective_period_source_is_not_treated_as_schedule_data() {
    let query = |source: &str| module(&format!("ВЫБРАТЬ {SCHEDULE_FIELD} КАК Часы ИЗ {source}"));
    let adjusted =
        "РегистрРасчета.Начисления.ФактическийПериодДействия(Регистратор = &Регистратор) КАК ДанныеГрафика";

    assert_eq!(reported(&query(adjusted)), Vec::<String>::new());
    assert_eq!(reported(&query(SCHEDULE_SOURCE)), vec![SCHEDULE_FIELD]);
}

/// Each of the four period kinds is a schedule value, whatever the resource name in front;
/// the protected use of one of them is not counted and does not hide the others.
#[test]
fn each_schedule_data_value_suffix_is_reported() {
    let source = module(&format!(
        "ВЫБРАТЬ ДанныеГрафика.ЧасыПериодДействия КАК А, ДанныеГрафика.ЧасыФактическийПериодДействия КАК Б, ДанныеГрафика.ЧасыПериодРегистрации КАК В, ДанныеГрафика.ЧасыБазовыйПериод КАК Г, ЕСТЬNULL(ДанныеГрафика.ЧасыПериодДействия, 0) КАК Защищено ИЗ {SCHEDULE_SOURCE}"
    ));
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 4, "{found:?}");
    assert!(found.iter().all(|d| d.message == SCHEDULE_DATA_MESSAGE), "{found:?}");
    assert_eq!(
        reported_sorted(&source),
        vec![
            "ДанныеГрафика.ЧасыБазовыйПериод",
            "ДанныеГрафика.ЧасыПериодДействия",
            "ДанныеГрафика.ЧасыПериодРегистрации",
            "ДанныеГрафика.ЧасыФактическийПериодДействия",
        ]
    );
}

/// The English table, alias and suffixes work as the Russian ones; the register's own
/// `RegistrationPeriod` and `ActionPeriod` are not schedule values. The message stays Russian.
#[test]
fn each_english_schedule_data_value_suffix_is_reported() {
    let source = module(
        "SELECT ScheduleData.HoursActionPeriod AS A, ScheduleData.HoursActualActionPeriod AS B, ScheduleData.HoursRegistrationPeriod AS C, ScheduleData.HoursBasePeriod AS D, ScheduleData.RegistrationPeriod AS E, ScheduleData.ActionPeriod AS F FROM CalculationRegister.Accruals.ScheduleData(Recorder = &Recorder) AS ScheduleData",
    );
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 4, "{found:?}");
    assert!(found.iter().all(|d| d.message == SCHEDULE_DATA_MESSAGE), "{found:?}");
    assert_eq!(
        reported_sorted(&source),
        vec![
            "ScheduleData.HoursActionPeriod",
            "ScheduleData.HoursActualActionPeriod",
            "ScheduleData.HoursBasePeriod",
            "ScheduleData.HoursRegistrationPeriod",
        ]
    );
}

/// The suffix is matched without regard to case, in the lower-case resource name as well as
/// in a field written entirely in capitals.
#[test]
fn schedule_data_value_suffix_match_ignores_field_case() {
    for field in [
        "ДанныеГрафика.часыФактическийПериодДействия",
        "ДанныеГрафика.ЧАСЫФАКТИЧЕСКИЙПЕРИОДДЕЙСТВИЯ",
    ] {
        let source = module(&format!("ВЫБРАТЬ {field} КАК Часы ИЗ {SCHEDULE_SOURCE}"));
        assert_eq!(reported(&source), vec![field]);
    }
}

/// The schedule table on the optional side of a left join is already reported by the join
/// check: one message per use, in the join's wording.
#[test]
fn schedule_data_field_on_an_optional_join_side_is_reported_once_as_a_join() {
    let source = module(&format!(
        "ВЫБРАТЬ {SCHEDULE_FIELD} ИЗ Документ.Движение КАК Движение ЛЕВОЕ СОЕДИНЕНИЕ {SCHEDULE_SOURCE} ПО Движение.Ссылка = ДанныеГрафика.Регистратор"
    ));
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(&source, &found[0]), SCHEDULE_FIELD);
    assert_eq!(found[0].message, JOIN_MESSAGE);
}

/// With an inner join the schedule table is not an optional side, so the join check is
/// silent and the schedule check alone reports the value, in the schedule wording.
#[test]
fn schedule_data_field_of_an_inner_joined_source_is_reported_as_schedule_data() {
    let source = module(&format!(
        "ВЫБРАТЬ {SCHEDULE_FIELD} ИЗ Документ.Движение КАК Движение ВНУТРЕННЕЕ СОЕДИНЕНИЕ {SCHEDULE_SOURCE} ПО Движение.Ссылка = ДанныеГрафика.Регистратор"
    ));
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(&source, &found[0]), SCHEDULE_FIELD);
    assert_eq!(found[0].message, SCHEDULE_DATA_MESSAGE);
}

/// The rule is on by default: the schedule value is reported without the code in `enabled`.
#[test]
fn schedule_data_value_is_reported_under_default_config() {
    let config = DiagnosticsConfig::default();
    assert!(!config.enabled.contains(&CODE), "the default config must not list the rule");
    let found = diagnostics(UNPROTECTED_PROCEDURE, &config);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(UNPROTECTED_PROCEDURE, &found[0]), SCHEDULE_FIELD);
    assert_eq!(found[0].message, SCHEDULE_DATA_MESSAGE);
}

/// The default also covers the field of an outer-join side, which the rule did not report
/// unless enabled.
#[test]
fn join_field_is_reported_under_default_config() {
    let config = DiagnosticsConfig::default();
    assert!(!config.enabled.contains(&CODE), "the default config must not list the rule");
    let source = module(
        "ВЫБРАТЬ Товары.Наименование КАК Наименование, Цены.Цена КАК Цена ИЗ Справочник.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Товары.Ссылка",
    );
    let found = diagnostics(&source, &config);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(&source, &found[0]), "Цены.Цена");
    assert_eq!(found[0].message, JOIN_MESSAGE);
}

/// The module of issue #768, kept as the handler's fixture.
const ISSUE_768_MODULE: &str =
    include_str!("../../../ide-diagnostics/src/handlers/fixtures/fields_from_joins_issue_768.bsl");

/// Under the default config only the product of case 4 is reported: the owner of a tabular
/// section, the branch under a comparison of the same side and the field used only in a
/// condition are silent, while the same factor inside ЕСТЬNULL in case 5 is protected.
#[test]
fn issue_768_module_reports_only_the_unprotected_product() {
    let found = diagnostics(ISSUE_768_MODULE, &DiagnosticsConfig::default());

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(ISSUE_768_MODULE, &found[0]), "Ставки.Ставка");
    assert_eq!(found[0].message, JOIN_MESSAGE);
    let case_4 = ISSUE_768_MODULE.find("Функция СуммаСНалогом()").unwrap();
    let case_5 = ISSUE_768_MODULE.find("Функция СуммаСНалогомЕстьNull()").unwrap();
    assert!((case_4..case_5).contains(&usize::from(found[0].range.start())), "{found:?}");
}

const RATES: &str = "ИЗ Документ.Заказ.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки ПО Товары.Номенклатура = Ставки.Номенклатура";

/// A field of the optional side used only in a condition does not reach the result. Selected
/// next to a condition on another source it is still reported, since that condition does not
/// exclude the row without a pair; next to a condition on the side itself it is hidden.
#[test]
fn a_field_used_only_in_a_condition_is_not_reported() {
    assert_eq!(
        reported(&module(&format!(
            "ВЫБРАТЬ Товары.Номенклатура {RATES} ГДЕ Ставки.Ставка = &Ставка"
        ))),
        Vec::<String>::new()
    );
    assert_eq!(
        reported(&module(&format!(
            "ВЫБРАТЬ Ставки.Код {RATES} ГДЕ Товары.Номенклатура = &Номенклатура"
        ))),
        vec!["Ставки.Код"]
    );
    assert_eq!(
        reported(&module(&format!("ВЫБРАТЬ Ставки.Код {RATES} ГДЕ Ставки.Ставка = &Ставка"))),
        Vec::<String>::new()
    );
}

/// The raw uses of the field are reported each, the one inside ЕСТЬNULL is not.
#[test]
fn each_raw_use_in_the_result_is_reported() {
    let source = module(&format!(
        "ВЫБРАТЬ Ставки.Ставка КАК А, Товары.Сумма * Ставки.Ставка КАК Б, ЕСТЬNULL(Ставки.Ставка, 0) КАК В {RATES}"
    ));
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 2, "{found:?}");
    let protected = source.find("ЕСТЬNULL(Ставки.Ставка").unwrap();
    assert!(found.iter().all(|d| usize::from(d.range.start()) < protected), "{found:?}");
    assert!(found.iter().all(|d| text_under(&source, d) == "Ставки.Ставка"), "{found:?}");
}

/// A schedule value is NULL in a present row, so a condition on it is still a use and a
/// comparison of the row's other field does not protect it.
#[test]
fn schedule_data_value_keeps_its_conditions_and_guards() {
    let value = "ДанныеГрафика.ЧасыПериодДействия";
    for query in [
        format!("ВЫБРАТЬ ДанныеГрафика.НомерСтроки КАК Н ИЗ {SCHEDULE_SOURCE} ГДЕ {value} > 0"),
        format!("ВЫБРАТЬ ВЫБОР КОГДА {value} > 0 ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК Н ИЗ {SCHEDULE_SOURCE}"),
        format!("ВЫБРАТЬ ВЫБОР КОГДА ДанныеГрафика.НомерСтроки <> 0 ТОГДА {value} ИНАЧЕ 0 КОНЕЦ КАК Н ИЗ {SCHEDULE_SOURCE}"),
    ] {
        let source = module(&query);
        let found = diagnostics(&source, &rule_enabled());
        assert_eq!(found.len(), 1, "{query}: {found:?}");
        assert_eq!(text_under(&source, &found[0]), value, "{query}");
        assert_eq!(found[0].message, SCHEDULE_DATA_MESSAGE, "{query}");
    }
}

/// On the optional side of a left join the comparison of the row's field proves the row, not
/// the schedule value: the join is silent and the value is reported once as schedule data.
#[test]
fn schedule_data_value_under_a_row_comparison_on_a_join_side_is_reported_once() {
    let source = module(&format!(
        "ВЫБРАТЬ ВЫБОР КОГДА ДанныеГрафика.НомерСтроки <> 0 ТОГДА {SCHEDULE_FIELD} ИНАЧЕ 0 КОНЕЦ КАК Н ИЗ Документ.Движение КАК Движение ЛЕВОЕ СОЕДИНЕНИЕ {SCHEDULE_SOURCE} ПО Движение.Ссылка = ДанныеГрафика.Регистратор"
    ));
    let found = diagnostics(&source, &rule_enabled());

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(&source, &found[0]), SCHEDULE_FIELD);
    assert_eq!(found[0].message, SCHEDULE_DATA_MESSAGE);
}

/// Disabling the code silences both the join and the schedule reports that the default
/// config produces.
#[test]
fn disabled_rule_reports_nothing() {
    let config = DiagnosticsConfig { disabled: vec![CODE], ..DiagnosticsConfig::default() };
    let join = module(
        "ВЫБРАТЬ Товары.Наименование КАК Наименование, Цены.Цена КАК Цена ИЗ Справочник.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Товары.Ссылка",
    );

    assert_eq!(diagnostics(&join, &DiagnosticsConfig::default()).len(), 1);
    assert_eq!(diagnostics(&join, &config), Vec::<Diagnostic>::new());
    assert_eq!(diagnostics(UNPROTECTED_PROCEDURE, &DiagnosticsConfig::default()).len(), 1);
    assert_eq!(diagnostics(UNPROTECTED_PROCEDURE, &config), Vec::<Diagnostic>::new());
}

/// What remains of the rule is what it always was: the wording, the level the configuration
/// gives it, no tags and no fixes. A configured level is followed and disabling the code
/// silences the report. The same field behind a `ГДЕ` that excludes the missing row is not
/// reported, so the report above is not silent for another reason.
#[test]
fn a_remaining_report_keeps_its_message_level_tags_and_fixes() {
    let field = module(&format!("ВЫБРАТЬ Ставки.Код {RATES}"));
    let filtered = module(&format!("ВЫБРАТЬ Ставки.Код {RATES} ГДЕ Ставки.Ставка > 0"));
    let level = |level: &str| DiagnosticsConfig {
        metadata_overrides: std::collections::HashMap::from([(
            CODE,
            MetadataOverride { lsp_severity: Some(level.to_string()), ..Default::default() },
        )]),
        ..rule_enabled()
    };

    let found = diagnostics(&field, &rule_enabled());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(text_under(&field, &found[0]), "Ставки.Код");
    assert_eq!(found[0].message, JOIN_MESSAGE);
    assert_eq!(found[0].severity, Severity::Warning);
    assert!(found[0].tags.is_empty(), "{:?}", found[0].tags);
    assert!(found[0].fixes.is_empty(), "{:?}", found[0].fixes);

    let found = diagnostics(&field, &level("Error"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].message, JOIN_MESSAGE);
    assert!(found[0].fixes.is_empty());

    assert_eq!(diagnostics(&filtered, &level("Error")), Vec::<Diagnostic>::new());

    let disabled = DiagnosticsConfig { disabled: vec![CODE], ..DiagnosticsConfig::default() };
    assert_eq!(diagnostics(&field, &disabled), Vec::<Diagnostic>::new());
}

/// A schedule value is reported once, wherever it stands, with the wording it always had: the
/// schedule one when its row is the schedule source itself or a comparison of the row number
/// proves the row, the join one when it stands on an optional join side that nothing proves.
/// Wrapped in ЕСТЬNULL it is silent in each of them.
#[test]
fn a_schedule_value_is_reported_once_whatever_proves_its_row() {
    let left = format!(
        "Документ.Движение КАК Движение ЛЕВОЕ СОЕДИНЕНИЕ {SCHEDULE_SOURCE} ПО Движение.Ссылка = ДанныеГрафика.Регистратор"
    );
    let queries = |value: &str| {
        [
            (format!("ВЫБРАТЬ {value} КАК Ч ИЗ {SCHEDULE_SOURCE}"), SCHEDULE_DATA_MESSAGE),
            (format!("ВЫБРАТЬ {value} КАК Ч ИЗ {left}"), JOIN_MESSAGE),
            (
                format!("ВЫБРАТЬ ВЫБОР КОГДА ДанныеГрафика.НомерСтроки <> 0 ТОГДА {value} ИНАЧЕ 0 КОНЕЦ КАК Ч ИЗ {SCHEDULE_SOURCE}"),
                SCHEDULE_DATA_MESSAGE,
            ),
            (
                format!("ВЫБРАТЬ ВЫБОР КОГДА ДанныеГрафика.НомерСтроки <> 0 ТОГДА {value} ИНАЧЕ 0 КОНЕЦ КАК Ч ИЗ {left}"),
                SCHEDULE_DATA_MESSAGE,
            ),
            (
                format!("ВЫБРАТЬ {value} КАК Ч ИЗ {SCHEDULE_SOURCE} ГДЕ ДанныеГрафика.НомерСтроки <> 0"),
                SCHEDULE_DATA_MESSAGE,
            ),
            (
                format!("ВЫБРАТЬ {value} КАК Ч ИЗ {left} ГДЕ ДанныеГрафика.НомерСтроки <> 0"),
                JOIN_MESSAGE,
            ),
        ]
    };
    let wrapped = format!("ЕСТЬNULL({SCHEDULE_FIELD}, 0)");
    for ((query, message), (protected, _)) in
        queries(SCHEDULE_FIELD).into_iter().zip(queries(&wrapped))
    {
        let source = module(&query);
        let found = diagnostics(&source, &rule_enabled());
        assert_eq!(found.len(), 1, "{query}: {found:?}");
        assert_eq!(text_under(&source, &found[0]), SCHEDULE_FIELD, "{query}");
        assert_eq!(found[0].message, message, "{query}");

        assert_eq!(
            diagnostics(&module(&protected), &rule_enabled()),
            Vec::<Diagnostic>::new(),
            "{protected}"
        );
    }
}

/// The proof that hides an ordinary field of an optional side does not hide a schedule value:
/// in one query a `ГДЕ` on both sources leaves only the schedule value reported, and without
/// the condition on the ordinary source its field is reported as well.
#[test]
fn a_proof_of_the_row_hides_an_ordinary_field_but_not_a_schedule_value() {
    let query = |condition: &str| {
        module(&format!(
            "ВЫБРАТЬ {SCHEDULE_FIELD} КАК Ч, Ставки.Код КАК Код ИЗ Документ.Движение КАК Движение ЛЕВОЕ СОЕДИНЕНИЕ {SCHEDULE_SOURCE} ПО Движение.Ссылка = ДанныеГрафика.Регистратор ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки ПО Движение.Ссылка = Ставки.Регистратор ГДЕ {condition}"
        ))
    };
    let texts = |source: &str| {
        let mut found = diagnostics(source, &rule_enabled());
        found.sort_by_key(|d| d.range.start());
        found.iter().map(|d| text_under(source, d).to_string()).collect::<Vec<_>>()
    };

    assert_eq!(
        texts(&query("ДанныеГрафика.НомерСтроки <> 0 И Ставки.Ставка <> 0")),
        [SCHEDULE_FIELD]
    );
    assert_eq!(texts(&query("ДанныеГрафика.НомерСтроки <> 0")), [SCHEDULE_FIELD, "Ставки.Код"]);
}
