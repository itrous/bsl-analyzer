use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use stdx::case::eq_ignore_case;
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Sql, MetadataTag::Standard, MetadataTag::Performance],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

/// Narrows the rule to modules that write the register, which keeps report modules quiet; on
/// unless set to `false`. A bare query text has no module, so the narrowing applies only where
/// there is a file.
pub(crate) const ONLY_IN_MODULES_WRITING_REGISTER: &str = "onlyInModulesWritingRegister";

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::CalculationRegisterVirtualTableWithoutRecorder {
        table_name,
        range,
        ..
    } = diag
    {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
            &format!("В условии виртуальной таблицы \"{table_name}\" нет отбора по регистратору"),
            *range,
            mapper,
            query_text,
            diagnostics,
        );
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
        dispatch,
    )
}

/// Whether the module writes the register: `Движения.<Регистр>` or `RegisterRecords.<Name>`,
/// on any receiver. Read from the tree as written — a branch the preprocessor would drop still
/// counts, and `Движения["<Регистр>"]` does not.
pub(crate) fn writes_register(root: &SyntaxNode, register_name: &str) -> bool {
    root.descendants().filter(|node| node.kind() == SyntaxKind::FIELD_EXPR).any(|field| {
        last_ident(&field).is_some_and(|name| eq_ignore_case(name.text(), register_name))
            && field.first_child().and_then(|receiver| last_ident(&receiver)).is_some_and(|name| {
                eq_ignore_case(name.text(), "Движения")
                    || eq_ignore_case(name.text(), "RegisterRecords")
            })
    })
}

fn last_ident(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::IDENT)
        .last()
}

#[cfg(test)]
mod tests {
    use crate::config::MetadataOverride;
    use crate::test_utils::{
        check_diagnostics_for_with_config, check_diagnostics_snapshot_for,
        check_file_diagnostics_with_config, format_diags,
    };
    use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, Severity};
    use expect_test::{expect, Expect};

    const CODE: DiagnosticCode = DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder;
    const WITHOUT_PARAMETERS: DiagnosticCode = DiagnosticCode::VirtualTableCallWithoutParameters;

    const SCHEDULE: &str = "РегистрРасчета.Начисления.ДанныеГрафика";
    const PERIOD: &str = "РегистрРасчета.Начисления.ФактическийПериодДействия";
    const BASE: &str = "РегистрРасчета.Начисления.БазаНачисления";
    /// A condition that passes the table a parameter without taking a stand on the recorder, so
    /// the rule has something to judge and the neighbouring rule has nothing to say.
    const CONDITION: &str = "ВидРасчета = &ВидРасчета";

    fn narrowed(value: bool) -> DiagnosticsConfig {
        let mut config = DiagnosticsConfig::all_enabled();
        config
            .parameters
            .insert(CODE, serde_json::json!({ "onlyInModulesWritingRegister": value }));
        config
    }

    fn check(source: &str, expected: Expect) {
        expected.assert_eq(&format_diags(source, &findings(source)));
    }

    /// The query text alone decides: the narrowing has its own tests.
    fn findings(source: &str) -> Vec<Diagnostic> {
        check_diagnostics_for_with_config(source, narrowed(false), CODE)
    }

    /// How many findings the rule and the neighbouring rule give, in that order.
    fn pair(source: &str, config: &DiagnosticsConfig) -> (usize, usize) {
        let count = |code| check_diagnostics_for_with_config(source, config.clone(), code).len();
        (count(CODE), count(WITHOUT_PARAMETERS))
    }

    /// The same count for a query text with no module around it.
    fn bare_pair(query: &str, config: &DiagnosticsConfig) -> (usize, usize) {
        let found = crate::validate_query_text(config, None, query);
        let count = |code| found.iter().filter(|diag| diag.code == code).count();
        (count(CODE), count(WITHOUT_PARAMETERS))
    }

    /// The source text of each table the rule reports.
    fn reported_tables(source: &str) -> Vec<&str> {
        findings(source)
            .iter()
            .map(|diag| &source[usize::from(diag.range.start())..usize::from(diag.range.end())])
            .collect()
    }

    fn query_module(query: &str) -> String {
        format!("Процедура Проверка()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query}\";\nКонецПроцедуры\n")
    }

    /// One procedure per table reference, each reading from it in its own query, so a case
    /// stays one line of the source and the table sits at line `4 + 5 * index`, column 9.
    fn module_of(tables: &[&str]) -> String {
        tables
            .iter()
            .enumerate()
            .map(|(index, table)| {
                format!(
                    "Процедура Проверка{index}()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ Т.Поле КАК Поле\n    |ИЗ {table} КАК Т\";\nКонецПроцедуры\n"
                )
            })
            .collect()
    }

    /// The rule reports the table once, names its path without the argument list, and does it
    /// at the severity the registration promises.
    fn assert_fires(table: &str) {
        let found = findings(&module_of(&[table]));
        let path = table.split('(').next().unwrap_or(table).trim();
        assert_eq!(found.len(), 1, "{table}: the rule must fire once, got {found:?}");
        assert_eq!(
            found[0].message,
            format!("В условии виртуальной таблицы \"{path}\" нет отбора по регистратору"),
            "{table}"
        );
        assert_eq!(found[0].severity, Severity::Warning, "{table}");
    }

    fn assert_silent(table: &str) {
        let found = findings(&module_of(&[table]));
        assert!(found.is_empty(), "{table}: the rule must stay silent, got {found:?}");
    }

    fn schedule(condition: &str) -> String {
        format!("{SCHEDULE}({condition})")
    }

    #[test]
    fn issue_examples_are_reported_where_the_issue_expects() {
        check(
            r#"Процедура ДанныеГрафикаБезОтбора()
	Запрос = Новый Запрос;
	Запрос.Текст =
	"ВЫБРАТЬ
	|	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	|	ЕСТЬNULL(ДанныеГрафика.ЧасыФактическийПериодДействия, 0) КАК Часы
	|ИЗ
	|	РегистрРасчета.Начисления.ДанныеГрафика КАК ДанныеГрафика";
	Запрос.Выполнить();
КонецПроцедуры

Процедура ДанныеГрафикаБезРегистратора()
	Запрос = Новый Запрос;
	Запрос.Текст =
	"ВЫБРАТЬ
	|	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	|	ЕСТЬNULL(ДанныеГрафика.ЧасыФактическийПериодДействия, 0) КАК Часы
	|ИЗ
	|	РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК ДанныеГрафика";
	Запрос.УстановитьПараметр("ВидРасчета", ПланыВидовРасчета.ВидыРасчета.ПустаяСсылка());
	Запрос.Выполнить();
КонецПроцедуры

Процедура ДанныеГрафикаКакУЭкзаменатора()
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
"#,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 19:4..19:69
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn a_call_without_parameters_is_left_to_the_neighbouring_rule() {
        let tables = [
            SCHEDULE.to_string(),
            format!("{SCHEDULE}()"),
            format!("{SCHEDULE}(, )"),
            PERIOD.to_string(),
            format!("{PERIOD}()"),
            format!("{PERIOD}(, )"),
            "CalculationRegister.Accruals.ScheduleData".to_string(),
            "CalculationRegister.Accruals.AdjustedEffectivePeriod()".to_string(),
        ];
        for table in &tables {
            assert_eq!(pair(&module_of(&[table]), &narrowed(false)), (0, 1), "{table}");
        }
        assert_eq!(pair(&module_of(&[&schedule(CONDITION)]), &narrowed(false)), (1, 0));
    }

    #[test]
    fn a_call_without_parameters_stays_with_the_neighbouring_rule_whatever_the_where_says() {
        let source = |table: &str| {
            query_module(&format!(
                "ВЫБРАТЬ Т.Поле КАК Поле\n    |ИЗ {table} КАК Т\n    |ГДЕ Т.Регистратор <> &Ссылка"
            ))
        };
        assert_eq!(pair(&source(PERIOD), &narrowed(false)), (0, 1));
        assert_eq!(pair(&source(&format!("{PERIOD}({CONDITION})")), &narrowed(false)), (1, 0));
    }

    #[test]
    fn switching_the_neighbouring_rule_off_does_not_bring_the_recorder_report_back() {
        let mut config = narrowed(false);
        config.disabled.push(WITHOUT_PARAMETERS);
        assert_eq!(pair(&module_of(&[SCHEDULE]), &config), (0, 0));
        assert_eq!(
            bare_pair(&format!("ВЫБРАТЬ Т.Поле КАК Поле ИЗ {SCHEDULE} КАК Т"), &config),
            (0, 0)
        );
        assert_eq!(pair(&module_of(&[&schedule(CONDITION)]), &config), (1, 0));
    }

    #[test]
    fn a_call_without_parameters_does_not_shield_other_tables_of_the_query() {
        let judged = schedule(CONDITION);
        let joined = |first: &str, second: &str| {
            query_module(&format!(
                "ВЫБРАТЬ Т.Поле КАК Поле\n    |ИЗ {first} КАК Т\n    |ЛЕВОЕ СОЕДИНЕНИЕ {second} КАК Д\n    |ПО ИСТИНА"
            ))
        };
        for source in [joined(SCHEDULE, &judged), joined(&judged, SCHEDULE)] {
            assert_eq!(reported_tables(&source), [judged.as_str()], "{source}");
            assert_eq!(pair(&source, &narrowed(false)), (1, 1), "{source}");
        }

        let nested = |outer: &str, inner: &str| {
            query_module(&format!(
                "ВЫБРАТЬ Т.Поле КАК Поле\n    |ИЗ {outer} КАК Т\n    |ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ В.Поле КАК Поле\n    |    ИЗ {inner} КАК В) КАК Д\n    |ПО ИСТИНА"
            ))
        };
        for source in [nested(SCHEDULE, &judged), nested(&judged, SCHEDULE)] {
            assert_eq!(reported_tables(&source), [judged.as_str()], "{source}");
            assert_eq!(pair(&source, &narrowed(false)), (1, 1), "{source}");
        }
    }

    #[test]
    fn a_base_table_is_not_shielded_by_the_neighbouring_rule() {
        for table in [
            BASE.to_string(),
            format!("{BASE}()"),
            format!("{BASE}(&Измерения)"),
            format!("{BASE}(&Измерения, &Измерения)"),
            format!("{BASE}(&Измерения, &Измерения, , )"),
            format!("{BASE}(Регистратор = &Регистратор, &Измерения, , )"),
        ] {
            assert_eq!(pair(&module_of(&[&table]), &narrowed(false)), (1, 0), "{table}");
        }
        let filtered = format!("{BASE}(&Измерения, &Измерения, , Регистратор = &Регистратор)");
        assert_eq!(pair(&module_of(&[&filtered]), &narrowed(false)), (0, 0));
    }

    #[test]
    fn a_where_clause_does_not_stand_in_for_the_parameter() {
        let source = r#"Процедура БезПараметра()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика КАК Т
    |ГДЕ Регистратор = &Регистратор";
КонецПроцедуры

Процедура ДругойОтбор()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК Т
    |ГДЕ Т.Регистратор = &Регистратор";
КонецПроцедуры

Процедура ОтборВПараметре()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК Т
    |ГДЕ Т.Регистратор = &Регистратор";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 11:9..11:74
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn a_recorder_in_list_is_a_filter() {
        assert_silent(&schedule("Регистратор В (&Список)"));
        assert_fires(&schedule("ВидРасчета В (&Список)"));
    }

    #[test]
    fn a_recorder_inequality_is_a_deliberate_exclusion() {
        for condition in [
            "Регистратор <> &Ссылка",
            "&Ссылка <> Регистратор",
            "(Регистратор <> &Ссылка)",
            "Сотрудник = &Сотрудник И Регистратор <> &Ссылка",
            "(Сотрудник = &Сотрудник) И (&Ссылка <> Регистратор)",
            "регистратор <> &Ссылка",
            "РЕГИСТРАТОР <> &Ссылка",
        ] {
            assert_silent(&schedule(condition));
            assert_silent(&format!("{PERIOD}({condition})"));
            assert_silent(&format!("{BASE}(&Измерения, &Измерения, , {condition})"));
        }
        for condition in
            ["Recorder <> &Ref", "&Ref <> recorder", "Employee = &E AND Recorder <> &Ref"]
        {
            assert_silent(&format!("CalculationRegister.Accruals.ScheduleData({condition})"));
            assert_silent(&format!(
                "CalculationRegister.Accruals.AdjustedEffectivePeriod({condition})"
            ));
            assert_silent(&format!(
                "CalculationRegister.Accruals.BaseMainAccruals(&D, &D, , {condition})"
            ));
        }
        assert_fires(&schedule("Сотрудник = &Сотрудник"));
    }

    #[test]
    fn a_recorder_inequality_outside_the_condition_of_a_base_table_does_not_count() {
        assert_fires(&format!("{BASE}(Регистратор <> &Ссылка, &Измерения, , )"));
        assert_fires(&format!("{BASE}(Регистратор <> &Ссылка, &Измерения, , {CONDITION})"));
        assert_silent(&format!("{BASE}(&Измерения, &Измерения, , Регистратор <> &Ссылка)"));
    }

    #[test]
    fn only_a_direct_recorder_inequality_counts_as_an_exclusion() {
        for condition in [
            "Сотрудник <> &Сотрудник",
            "&Сотрудник <> Сотрудник",
            "Регистратор.Номер <> &Номер",
            "Т.Регистратор <> &Ссылка",
            "Регистратор <> &Ссылка ИЛИ Сотрудник = &Сотрудник",
            "Регистратор <> &А ИЛИ Регистратор <> &Б",
            "НЕ Регистратор <> &Ссылка",
            "НЕ (Регистратор <> &Ссылка)",
            "Регистратор < &Ссылка",
            "Регистратор > &Ссылка",
            "Регистратор <= &Ссылка",
            "Регистратор >= &Ссылка",
        ] {
            assert_fires(&schedule(condition));
        }
        assert_silent(&schedule("Сотрудник = &Сотрудник И Регистратор <> &Ссылка"));
    }

    #[test]
    fn a_recorder_inequality_in_the_where_clause_does_not_stand_in_for_the_parameter() {
        let source = |condition: &str| {
            query_module(&format!(
                "ВЫБРАТЬ Т.Поле КАК Поле\n    |ИЗ {SCHEDULE}({condition}) КАК Т\n    |ГДЕ Т.Регистратор <> &Ссылка"
            ))
        };
        assert_eq!(findings(&source(CONDITION)).len(), 1);
        assert_eq!(findings(&source("Регистратор <> &Ссылка")).len(), 0);
    }

    #[test]
    fn a_negated_recorder_comparison_is_not_a_filter() {
        assert_fires(&schedule("НЕ Регистратор = &Регистратор"));
    }

    #[test]
    fn a_negated_recorder_in_list_is_not_a_filter() {
        assert_fires(&schedule("НЕ Регистратор В (&Список)"));
        assert_silent(&schedule("Регистратор В (&Список)"));
    }

    #[test]
    fn a_recorder_outside_the_list_is_not_a_filter() {
        assert_fires(&schedule("Регистратор НЕ В (&Список)"));
        assert_silent(&schedule("Регистратор В (&Список)"));
    }

    #[test]
    fn only_the_left_operand_of_in_is_taken_for_the_recorder() {
        assert_fires(&schedule("ВидРасчета В (Регистратор)"));
        assert_silent(&schedule("Регистратор В (ВидРасчета)"));
    }

    #[test]
    fn a_recorder_in_hierarchy_is_not_a_filter() {
        assert_fires(&schedule("Регистратор В ИЕРАРХИИ (&Группа)"));
        assert_silent(&schedule("Регистратор В (&Группа)"));
    }

    #[test]
    fn a_recorder_comparison_under_or_is_not_a_filter() {
        assert_fires(&schedule("Регистратор = &А ИЛИ ВидРасчета = &В"));
        assert_silent(&schedule("Регистратор = &А И ВидРасчета = &В"));
    }

    #[test]
    fn a_recorder_comparison_in_a_conjunction_is_a_filter() {
        assert_silent(&schedule("Регистратор = &Р И ВидРасчета = &В"));
        assert_silent(&schedule("ВидРасчета = &В И Регистратор = &Р"));
        assert_fires(&schedule("ВидРасчета = &В И Сотрудник = &Р"));
    }

    #[test]
    fn the_recorder_may_stand_on_either_side_of_the_comparison() {
        assert_silent(&schedule("&Регистратор = Регистратор"));
        assert_fires(&schedule("&ВидРасчета = ВидРасчета"));
    }

    #[test]
    fn the_recorder_field_is_matched_ignoring_case() {
        assert_silent(&schedule("регистратор = &Регистратор"));
        assert_silent(&schedule("РЕГИСТРАТОР = &Регистратор"));
        assert_silent(&schedule("рЕгИсТрАтОр В (&Список)"));
        assert_fires(&schedule("ВидРасчета = &Регистратор"));
    }

    #[test]
    fn a_field_of_the_recorder_is_not_a_filter() {
        assert_fires(&schedule("Регистратор.Номер = &Номер"));
        assert_silent(&schedule("Регистратор = &Номер"));
    }

    #[test]
    fn english_schedule_data_is_judged_like_the_russian_one() {
        assert_silent("CalculationRegister.Accruals.ScheduleData(Recorder = &Recorder)");
        assert_fires(
            "CalculationRegister.Accruals.ScheduleData(CalculationType = &CalculationType)",
        );
    }

    #[test]
    fn the_actual_action_period_table_is_judged() {
        assert_fires(&format!("{PERIOD}(ВидРасчета = &ВидРасчета)"));
        assert_silent(&format!("{PERIOD}(Регистратор = &Регистратор)"));
    }

    #[test]
    fn the_english_adjusted_effective_period_table_is_judged() {
        assert_fires(
            "CalculationRegister.Accruals.AdjustedEffectivePeriod(CalculationType = &CalculationType)",
        );
        assert_silent("CalculationRegister.Accruals.AdjustedEffectivePeriod(Recorder = &Recorder)");
    }

    #[test]
    fn a_base_table_without_arguments_is_reported() {
        assert_fires(BASE);
        assert_fires(&format!("{BASE}()"));
    }

    #[test]
    fn a_base_table_is_filtered_by_its_fourth_argument() {
        assert_silent(&format!("{BASE}(&Измерения, &Измерения, , Регистратор = &Регистратор)"));
        assert_fires(&format!("{BASE}(&Измерения, &Измерения, , ВидРасчета = &ВидРасчета)"));
    }

    #[test]
    fn a_recorder_in_the_first_argument_of_a_base_table_is_not_its_condition() {
        assert_fires(&format!("{BASE}(Регистратор = &Регистратор)"));
        assert_fires(&format!("{BASE}(Регистратор = &Регистратор, &Измерения, , )"));
    }

    #[test]
    fn the_english_base_table_is_judged_by_its_fourth_argument() {
        assert_fires("CalculationRegister.Accruals.BaseMainAccruals");
        assert_silent(
            "CalculationRegister.Accruals.BaseMainAccruals(&Dimensions, &Dimensions, , Recorder = &Recorder)",
        );
        assert_fires(
            "CalculationRegister.Accruals.BaseMainAccruals(&Dimensions, &Dimensions, , CalculationType = &CalculationType)",
        );
        assert_fires("CalculationRegister.Accruals.baseMainAccruals");
    }

    #[test]
    fn other_tables_of_the_calculation_register_are_not_judged() {
        assert_fires(&schedule(CONDITION));
        assert_silent("РегистрРасчета.Начисления.Изменения");
        assert_silent("РегистрРасчета.Начисления");
        assert_silent("РегистрРасчета.Начисления.Перерасчет");
        assert_silent("РегистрРасчета.Начисления.База");
        assert_silent("CalculationRegister.Accruals.Changes");
        assert_silent("CalculationRegister.Accruals");
        assert_silent("CalculationRegister.Accruals.Base");
    }

    #[test]
    fn a_virtual_table_of_another_register_kind_is_not_judged() {
        assert_fires(&schedule(CONDITION));
        assert_silent("РегистрНакопления.Запасы.Остатки");
        assert_silent("РегистрНакопления.Запасы.Остатки(, Склад = &Склад)");
        assert_silent("РегистрНакопления.Запасы.БазаОстатков");
        assert_silent("Справочник.Товары.БазаДанных");
    }

    #[test]
    fn a_temporary_table_or_a_document_path_named_like_the_table_is_not_judged() {
        let source = r#"Процедура ВременнаяТаблица()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ПОМЕСТИТЬ ДанныеГрафика
    |ИЗ Справочник.Сотрудники КАК Т
    |;
    |ВЫБРАТЬ ДанныеГрафика.Поле КАК Поле
    |ИЗ ДанныеГрафика КАК ДанныеГрафика";
КонецПроцедуры

Процедура ПутьДокумента()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ Документ.Движение.ДанныеГрафика КАК Т";
КонецПроцедуры

Процедура Регистр()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК Т";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 20:9..20:74
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn arguments_concatenated_at_run_time_are_not_judged() {
        let source = r#"Функция Отчёт(Условие, Отбор) Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(" + Условие + ") КАК Т";
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета " + Отбор + ") КАК Т";
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.БазаНачисления(&Измерения, &Измерения, , " + Отбор + ") КАК Т";
    Возврат Запрос;
КонецФункции
"#;
        check(source, expect![[""]]);
        assert_fires(&schedule(CONDITION));
    }

    #[test]
    fn arguments_substituted_into_a_template_are_not_judged() {
        let source = r#"Функция Отчёт(Условие) Экспорт
    Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(//УСЛОВИЕ) КАК Т";
    Текст = СтрЗаменить(Текст, "//УСЛОВИЕ", Условие);

    Шаблон = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(%1) КАК Т";
    Возврат СтрШаблон(Шаблон, Условие) + Текст;
КонецФункции
"#;
        check(source, expect![[""]]);
        assert_fires(&schedule(CONDITION));
    }

    #[test]
    fn a_base_table_with_fewer_arguments_than_its_condition_slot_is_reported() {
        assert_fires(&format!("{BASE}(&Измерения)"));
        assert_fires(&format!("{BASE}(&Измерения, &Измерения, )"));
    }

    #[test]
    fn a_complete_call_next_to_a_run_time_one_is_still_judged() {
        let source = r#"Функция Отчёт(Условие) Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК Т
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрРасчета.Начисления.ФактическийПериодДействия(" + Условие + ") КАК Д
    |ПО ИСТИНА";
    Возврат Запрос;
КонецФункции
"#;
        check(
            source,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 4:9..4:74
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn a_data_composition_extension_in_the_condition_slot_is_not_judged() {
        assert_silent(&schedule("{Регистратор = &Регистратор}"));
        assert_silent(&schedule("{(Регистратор = &Регистратор)}"));
        assert_silent(&format!("{BASE}(&Измерения, &Измерения, , {{Регистратор = &Регистратор}})"));
        assert_fires(&schedule(CONDITION));
    }

    #[test]
    fn a_data_composition_extension_outside_the_condition_slot_does_not_silence_the_rule() {
        assert_fires(&format!("{BASE}({{&Измерения}}, &Измерения, , )"));
    }

    #[test]
    fn a_joined_virtual_table_is_judged() {
        let source = r#"Процедура Соединение()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ Справочник.Сотрудники КАК Т
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК Д
    |ПО Т.Ссылка = Д.Сотрудник";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 5:23..5:88
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn a_virtual_table_in_a_nested_query_is_judged() {
        let source = r#"Процедура Вложенный()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ (ВЫБРАТЬ Д.Поле КАК Поле
    |    ИЗ РегистрРасчета.Начисления.ДанныеГрафика(ВидРасчета = &ВидРасчета) КАК Д) КАК Т";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 5:13..5:78
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]],
        );
    }

    #[test]
    fn the_call_without_parameters_rule_reports_alone_on_the_tables_it_reports() {
        let source = module_of(&[SCHEDULE, PERIOD, &schedule("ВидРасчета = &ВидРасчета")]);
        check_diagnostics_snapshot_for(
            &source,
            WITHOUT_PARAMETERS,
            expect![[r#"
                VirtualTableCallWithoutParameters @ 4:9..4:48
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major
                VirtualTableCallWithoutParameters @ 9:9..9:60
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]],
        );
        check(
            &source,
            expect![[r#"
            CalculationRegisterVirtualTableWithoutRecorder @ 14:9..14:74
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Warning"#]],
        );
    }

    #[test]
    fn a_base_table_is_filtered_by_either_form_in_its_fourth_argument_only() {
        for condition in [
            "&Регистратор = Регистратор",
            "Регистратор В (&Список)",
            "(Регистратор = &Регистратор) И (ВидРасчета = &ВидРасчета)",
        ] {
            assert_silent(&format!("{BASE}(&Измерения, &Измерения, , {condition})"));
            assert_fires(&format!("{BASE}({condition}, &Измерения, , )"));
        }
    }

    #[test]
    fn the_finding_is_a_warning_on_both_inputs_unless_the_user_raises_it() {
        let table = schedule(CONDITION);
        let raised = {
            let mut config = narrowed(false);
            config.metadata_overrides.insert(
                CODE,
                MetadataOverride { lsp_severity: Some("Error".to_owned()), ..Default::default() },
            );
            config
        };
        let module = module_of(&[&table]);
        let query = format!("ВЫБРАТЬ Т.Поле КАК Поле ИЗ {table} КАК Т");
        let in_query = |config: &DiagnosticsConfig| -> Vec<Diagnostic> {
            crate::validate_query_text(config, None, &query)
                .into_iter()
                .filter(|diag| diag.code == CODE)
                .collect()
        };
        let table_start = query.find(&table).unwrap();

        for found in [findings(&module), in_query(&narrowed(false))] {
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].severity, Severity::Warning);
            assert_eq!(
                found[0].message,
                format!("В условии виртуальной таблицы \"{SCHEDULE}\" нет отбора по регистратору")
            );
        }
        let standalone = in_query(&narrowed(false));
        assert_eq!(usize::from(standalone[0].range.start()), table_start);
        assert_eq!(usize::from(standalone[0].range.end()), table_start + table.len());
        assert_eq!(
            &module[usize::from(findings(&module)[0].range.start())..][..table.len()],
            table
        );

        let in_module = check_diagnostics_for_with_config(&module, raised.clone(), CODE);
        assert_eq!(in_module[0].severity, Severity::Error);
        assert_eq!(in_query(&raised)[0].severity, Severity::Error);
    }

    #[test]
    fn a_bare_query_text_follows_the_same_conditions_as_a_module() {
        let config = narrowed(true);
        let query = |table: &str| format!("ВЫБРАТЬ Т.Поле КАК Поле ИЗ {table} КАК Т");
        assert_eq!(bare_pair(&query(SCHEDULE), &config), (0, 1));
        assert_eq!(bare_pair(&query(&format!("{SCHEDULE}()")), &config), (0, 1));
        assert_eq!(bare_pair(&query(&schedule("Регистратор <> &Ссылка")), &config), (0, 0));
        assert_eq!(bare_pair(&query(&schedule(CONDITION)), &config), (1, 0));
    }

    #[test]
    fn a_disjunction_of_recorder_comparisons_is_not_unfolded() {
        assert_fires(&schedule("Регистратор = &А ИЛИ Регистратор = &Б"));
    }

    #[test]
    fn a_parenthesised_conjunction_with_the_recorder_is_a_filter() {
        assert_silent(&schedule("(Регистратор = &Регистратор) И (ВидРасчета = &ВидРасчета)"));
        assert_fires(&schedule("(ВидРасчета = &ВидРасчета) И (Сотрудник = &Сотрудник)"));
    }

    #[test]
    fn a_broken_expression_inside_the_condition_is_still_judged_as_a_condition() {
        assert_fires(&schedule("&Список В Регистратор"));
    }

    #[test]
    fn the_virtual_table_parameter_check_is_not_widened_to_the_calculation_register() {
        let source = module_of(&[
            &schedule("Регистратор = &Регистратор, 1, 2"),
            &format!("{BASE}(&Измерения, &Измерения, , Регистратор = &Регистратор, 1)"),
            "РегистрНакопления.Запасы.Остатки(&Дата, Склад = &Склад, 1)",
        ]);
        check_diagnostics_snapshot_for(
            &source,
            DiagnosticCode::InvalidVirtualTableParameters,
            expect![[r#"
                InvalidVirtualTableParameters @ 14:65..14:66
                  message: Неверные параметры виртуальной таблицы "РегистрНакопления.Запасы.Остатки": передано 3, а у таблицы 2 (Период, Условие). Условия внутри одного параметра соединяются через И, а не запятой
                  severity: Blocker"#]],
        );
    }

    mod only_in_modules_writing_register {
        use super::*;

        const FILE_QUERY: &str = r#"Процедура ДанныеГрафикаБезОтбора()
    Запрос = Новый Запрос;
    Запрос.Текст =
    "ВЫБРАТЬ
    |    ДанныеГрафика.НомерСтроки КАК НомерСтроки
    |ИЗ
    |    РегистрРасчета.Начисления.ДанныеГрафика(Сотрудник = &Сотрудник) КАК ДанныеГрафика";
    Запрос.Выполнить();
КонецПроцедуры
"#;

        fn check_narrowed(source: &str, expected: Expect) {
            let found = check_diagnostics_for_with_config(source, narrowed(true), CODE);
            expected.assert_eq(&format_diags(source, &found));
        }

        /// Silent under the narrowing and reported without it, so the silence is the narrowing's
        /// doing and not a query the rule could not read.
        fn check_silenced(source: &str) {
            check_narrowed(source, expect![[""]]);
            let unnarrowed = check_diagnostics_for_with_config(source, narrowed(false), CODE);
            assert_eq!(
                unnarrowed.len(),
                1,
                "the module must be judged without the narrowing: {unnarrowed:?}"
            );
        }

        fn with_statement(statement: &str) -> String {
            format!("Процедура Запись()\n    {statement}\nКонецПроцедуры\n\n{FILE_QUERY}")
        }

        #[test]
        fn a_module_that_does_not_write_the_register_is_not_judged() {
            check_silenced(FILE_QUERY);
        }

        #[test]
        fn a_module_that_writes_the_register_is_judged() {
            check_narrowed(
                &with_statement("Движения.Начисления.Добавить();"),
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:73
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Warning"#]],
            );
        }

        #[test]
        fn a_module_that_writes_another_register_is_not_judged() {
            check_silenced(&with_statement("Движения.Удержания.Добавить();"));
        }

        #[test]
        fn the_register_is_found_through_any_receiver_in_either_language_and_case() {
            check_narrowed(
                &with_statement("ЭтотОбъект.Движения.Начисления.Записать();"),
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:73
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Warning"#]],
            );
            check_narrowed(
                &with_statement("движения.начисления.Записать();"),
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:73
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Warning"#]],
            );
            let english = "Procedure Write()\n    RegisterRecords.Accruals.Write();\nEndProcedure\n\nProcedure Query()\n    Query = New Query;\n    Query.Text = \"SELECT T.Field AS Field\n    |FROM CalculationRegister.Accruals.ScheduleData(Employee = &Employee) AS T\";\nEndProcedure\n";
            check_narrowed(
                english,
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 8:11..8:74
                      message: В условии виртуальной таблицы "CalculationRegister.Accruals.ScheduleData" нет отбора по регистратору
                      severity: Warning"#]],
            );
        }

        #[test]
        fn a_bare_query_text_is_not_narrowed() {
            let query = "ВЫБРАТЬ Т.Поле КАК Поле ИЗ РегистрРасчета.Начисления.ДанныеГрафика(Сотрудник = &Сотрудник) КАК Т";
            let found: Vec<_> = crate::validate_query_text(&narrowed(true), None, query)
                .into_iter()
                .filter(|diag| diag.code == CODE)
                .collect();
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 1:28..1:91
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Warning"#]].assert_eq(&format_diags(query, &found));
        }

        #[test]
        fn a_member_named_like_the_register_on_another_receiver_is_not_a_write() {
            check_narrowed(&with_statement("ЭтотОбъект.Начисления.Очистить();"), expect![[""]]);
            check_narrowed(
                &with_statement("Движения.Начисления.Добавить();"),
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:73
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Warning"#]],
            );
        }

        #[test]
        fn a_comment_that_mentions_a_write_is_not_a_write() {
            check_silenced(&with_statement("// Движения.Начисления.Добавить()"));
        }

        #[test]
        fn an_index_access_to_the_register_is_not_a_write() {
            check_silenced(&with_statement("Движения[\"Начисления\"].Добавить();"));
        }

        #[test]
        fn a_write_in_a_branch_the_preprocessor_drops_still_counts() {
            let source = format!(
                "Процедура Запись()\n#Если Ложь Тогда\n    Движения.Начисления.Добавить();\n#КонецЕсли\nКонецПроцедуры\n\n{FILE_QUERY}"
            );
            check_narrowed(
                &source,
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 13:10..13:73
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Warning"#]],
            );
        }

        #[test]
        fn with_the_parameter_false_the_module_does_not_matter() {
            for source in
                [FILE_QUERY.to_string(), with_statement("Движения.Начисления.Добавить();")]
            {
                assert_eq!(
                    check_diagnostics_for_with_config(&source, narrowed(false), CODE).len(),
                    1,
                    "{source}"
                );
            }
        }

        #[test]
        fn without_the_parameter_the_module_is_narrowed() {
            let unset = DiagnosticsConfig::all_enabled;
            assert_eq!(check_diagnostics_for_with_config(FILE_QUERY, unset(), CODE).len(), 0);
            let writing = with_statement("Движения.Начисления.Добавить();");
            assert_eq!(check_diagnostics_for_with_config(&writing, unset(), CODE).len(), 1);
            let bare = "ВЫБРАТЬ Т.Поле КАК Поле ИЗ РегистрРасчета.Начисления.ДанныеГрафика(Сотрудник = &Сотрудник) КАК Т";
            assert_eq!(bare_pair(bare, &unset()), (1, 0));
        }

        #[test]
        fn a_deliberate_exclusion_is_silent_in_a_module_the_narrowing_keeps() {
            let writing = |condition: &str| {
                FILE_QUERY.replace("Сотрудник = &Сотрудник", condition)
                    + "\nПроцедура Запись()\n    Движения.Начисления.Добавить();\nКонецПроцедуры\n"
            };
            let count = |source: &str| {
                check_diagnostics_for_with_config(source, narrowed(true), CODE).len()
            };
            assert_eq!(count(&writing("Сотрудник = &Сотрудник")), 1);
            assert_eq!(count(&writing("Сотрудник = &Сотрудник И Регистратор <> &Ссылка")), 0);
        }
    }

    mod issue_766 {
        use super::*;

        const MODULE: &str = include_str!(
            "fixtures/calculation_register_virtual_table_without_recorder_issue766.bsl"
        );

        fn writing_module() -> String {
            format!(
                "Процедура Запись()\n\tДвижения.Начисления.Записать();\nКонецПроцедуры\n\n{MODULE}"
            )
        }

        fn both_rules(source: &str, config: DiagnosticsConfig) -> String {
            let found: Vec<_> = check_file_diagnostics_with_config(source, config)
                .into_iter()
                .filter(|diag| diag.code == CODE || diag.code == WITHOUT_PARAMETERS)
                .collect();
            format_diags(source, &found)
        }

        /// The query texts of the module's literals, in source order.
        fn bare_queries() -> Vec<String> {
            MODULE
                .split("Запрос.Текст =")
                .skip(1)
                .map(|tail| {
                    let (_, literal) = tail.split_once('"').unwrap();
                    let (literal, _) = literal.split_once('"').unwrap();
                    literal
                        .lines()
                        .map(|line| line.trim_start().trim_start_matches('|'))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect()
        }

        #[test]
        fn a_module_that_does_not_write_the_register_keeps_only_the_parameter_report() {
            expect![[r#"
                VirtualTableCallWithoutParameters @ 28:4..28:55
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]]
            .assert_eq(&both_rules(MODULE, narrowed(true)));
        }

        #[test]
        fn the_default_gives_what_the_narrowing_gives() {
            assert_eq!(
                both_rules(MODULE, DiagnosticsConfig::all_enabled()),
                both_rules(MODULE, narrowed(true))
            );
            assert_eq!(
                both_rules(&writing_module(), DiagnosticsConfig::all_enabled()),
                both_rules(&writing_module(), narrowed(true))
            );
        }

        #[test]
        fn a_module_that_writes_the_register_gets_a_warning_for_the_history_only() {
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 13:4..16:61
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ФактическийПериодДействия" нет отбора по регистратору
                  severity: Warning
                VirtualTableCallWithoutParameters @ 32:4..32:55
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]].assert_eq(&both_rules(&writing_module(), narrowed(true)));
        }

        #[test]
        fn without_the_narrowing_the_history_is_reported_in_any_module() {
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 9:4..12:61
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ФактическийПериодДействия" нет отбора по регистратору
                  severity: Warning
                VirtualTableCallWithoutParameters @ 28:4..28:55
                  message: Не следует использовать виртуальные таблицы без параметров
                  severity: Major"#]].assert_eq(&both_rules(MODULE, narrowed(false)));
        }

        #[test]
        fn the_bare_queries_are_judged_as_in_a_module_that_writes_the_register() {
            let counts: Vec<_> =
                bare_queries().iter().map(|query| bare_pair(query, &narrowed(true))).collect();
            assert_eq!(counts, [(1, 0), (0, 1), (0, 0), (0, 0)]);
        }
    }
}
