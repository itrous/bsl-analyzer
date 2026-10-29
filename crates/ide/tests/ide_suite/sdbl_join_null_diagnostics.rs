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
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, Severity};
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
