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
    lsp_severity_override: "",
};

/// Narrows the rule to modules that write the register, which keeps report modules quiet. A
/// bare query text has no module, so the narrowing applies only where there is a file.
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
    use crate::test_utils::{
        check_diagnostics_for_with_config, check_diagnostics_snapshot_for, check_hir_diagnostic,
        format_diags,
    };
    use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, Severity};
    use expect_test::{expect, Expect};

    const CODE: DiagnosticCode = DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder;

    const SCHEDULE: &str = "РегистрРасчета.Начисления.ДанныеГрафика";
    const PERIOD: &str = "РегистрРасчета.Начисления.ФактическийПериодДействия";
    const BASE: &str = "РегистрРасчета.Начисления.БазаНачисления";

    fn check(source: &str, expected: Expect) {
        check_diagnostics_snapshot_for(source, CODE, expected);
    }

    fn findings(source: &str) -> Vec<Diagnostic> {
        check_hir_diagnostic(source).into_iter().filter(|diag| diag.code == CODE).collect()
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
        assert_eq!(found[0].severity, Severity::Major, "{table}");
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
                CalculationRegisterVirtualTableWithoutRecorder @ 8:4..8:43
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major
                CalculationRegisterVirtualTableWithoutRecorder @ 19:4..19:69
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]],
        );
    }

    #[test]
    fn a_table_without_arguments_or_with_empty_ones_is_reported() {
        assert_fires(SCHEDULE);
        assert_fires(&format!("{SCHEDULE}()"));
        assert_fires(&format!("{SCHEDULE}(, )"));
        assert_silent(&schedule("Регистратор = &Регистратор"));
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
            CalculationRegisterVirtualTableWithoutRecorder @ 4:9..4:48
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major
            CalculationRegisterVirtualTableWithoutRecorder @ 11:9..11:74
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
        );
    }

    #[test]
    fn a_recorder_in_list_is_a_filter() {
        assert_silent(&schedule("Регистратор В (&Список)"));
        assert_fires(&schedule("ВидРасчета В (&Список)"));
    }

    #[test]
    fn a_recorder_inequality_is_not_a_filter() {
        assert_fires(&schedule("Регистратор <> &Регистратор"));
        assert_silent(&schedule("Регистратор = &Регистратор"));
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
        assert_fires("CalculationRegister.Accruals.ScheduleData");
        assert_silent("CalculationRegister.Accruals.ScheduleData(Recorder = &Recorder)");
        assert_fires(
            "CalculationRegister.Accruals.ScheduleData(CalculationType = &CalculationType)",
        );
    }

    #[test]
    fn the_actual_action_period_table_is_judged() {
        assert_fires(PERIOD);
        assert_fires(&format!("{PERIOD}(ВидРасчета = &ВидРасчета)"));
        assert_silent(&format!("{PERIOD}(Регистратор = &Регистратор)"));
    }

    #[test]
    fn the_english_adjusted_effective_period_table_is_judged() {
        assert_fires("CalculationRegister.Accruals.AdjustedEffectivePeriod");
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
        assert_fires(SCHEDULE);
        assert_silent("РегистрРасчета.Начисления.Изменения");
        assert_silent("РегистрРасчета.Начисления");
        assert_silent("РегистрРасчета.Начисления.Перерасчет");
        assert_silent("РегистрРасчета.Начисления.База");
    }

    #[test]
    fn a_virtual_table_of_another_register_kind_is_not_judged() {
        assert_fires(SCHEDULE);
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
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика КАК Т";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
            CalculationRegisterVirtualTableWithoutRecorder @ 20:9..20:48
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
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
        assert_fires(&schedule(""));
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
        assert_fires(&schedule(""));
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
    |ИЗ РегистрРасчета.Начисления.ДанныеГрафика() КАК Т
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрРасчета.Начисления.ФактическийПериодДействия(" + Условие + ") КАК Д
    |ПО ИСТИНА";
    Возврат Запрос;
КонецФункции
"#;
        check(
            source,
            expect![[r#"
            CalculationRegisterVirtualTableWithoutRecorder @ 4:9..4:50
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
        );
    }

    #[test]
    fn a_data_composition_extension_in_the_condition_slot_is_not_judged() {
        assert_silent(&schedule("{Регистратор = &Регистратор}"));
        assert_silent(&schedule("{(Регистратор = &Регистратор)}"));
        assert_silent(&format!("{BASE}(&Измерения, &Измерения, , {{Регистратор = &Регистратор}})"));
        assert_fires(&schedule(""));
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
    |ЛЕВОЕ СОЕДИНЕНИЕ РегистрРасчета.Начисления.ДанныеГрафика КАК Д
    |ПО Т.Ссылка = Д.Сотрудник";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
            CalculationRegisterVirtualTableWithoutRecorder @ 5:23..5:62
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
        );
    }

    #[test]
    fn a_virtual_table_in_a_nested_query_is_judged() {
        let source = r#"Процедура Вложенный()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле КАК Поле
    |ИЗ (ВЫБРАТЬ Д.Поле КАК Поле
    |    ИЗ РегистрРасчета.Начисления.ДанныеГрафика КАК Д) КАК Т";
КонецПроцедуры
"#;
        check(
            source,
            expect![[r#"
            CalculationRegisterVirtualTableWithoutRecorder @ 5:13..5:52
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
        );
    }

    #[test]
    fn the_call_without_parameters_rule_still_reports_the_same_tables() {
        let source = module_of(&[SCHEDULE, PERIOD, &schedule("ВидРасчета = &ВидРасчета")]);
        check_diagnostics_snapshot_for(
            &source,
            DiagnosticCode::VirtualTableCallWithoutParameters,
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
            CalculationRegisterVirtualTableWithoutRecorder @ 4:9..4:48
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major
            CalculationRegisterVirtualTableWithoutRecorder @ 9:9..9:60
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ФактическийПериодДействия" нет отбора по регистратору
              severity: Major
            CalculationRegisterVirtualTableWithoutRecorder @ 14:9..14:74
              message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
              severity: Major"#]],
        );
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
    |    РегистрРасчета.Начисления.ДанныеГрафика КАК ДанныеГрафика";
    Запрос.Выполнить();
КонецПроцедуры
"#;

        fn narrowed(value: bool) -> DiagnosticsConfig {
            let mut config = DiagnosticsConfig::all_enabled();
            config
                .parameters
                .insert(CODE, serde_json::json!({ "onlyInModulesWritingRegister": value }));
            config
        }

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
                CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:49
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]],
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
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:49
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Major"#]],
            );
            check_narrowed(
                &with_statement("движения.начисления.Записать();"),
                expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:49
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]],
            );
            let english = "Procedure Write()\n    RegisterRecords.Accruals.Write();\nEndProcedure\n\nProcedure Query()\n    Query = New Query;\n    Query.Text = \"SELECT T.Field AS Field\n    |FROM CalculationRegister.Accruals.ScheduleData AS T\";\nEndProcedure\n";
            check_narrowed(
                english,
                expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 8:11..8:52
                  message: В условии виртуальной таблицы "CalculationRegister.Accruals.ScheduleData" нет отбора по регистратору
                  severity: Major"#]],
            );
        }

        #[test]
        fn a_bare_query_text_is_not_narrowed() {
            let query = "ВЫБРАТЬ Т.Поле КАК Поле ИЗ РегистрРасчета.Начисления.ДанныеГрафика КАК Т";
            let found: Vec<_> = crate::validate_query_text(&narrowed(true), None, query)
                .into_iter()
                .filter(|diag| diag.code == CODE)
                .collect();
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 1:28..1:67
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]].assert_eq(&format_diags(query, &found));
        }

        #[test]
        fn a_member_named_like_the_register_on_another_receiver_is_not_a_write() {
            check_narrowed(&with_statement("ЭтотОбъект.Начисления.Очистить();"), expect![[""]]);
            check_narrowed(
                &with_statement("Движения.Начисления.Добавить();"),
                expect![[r#"
                    CalculationRegisterVirtualTableWithoutRecorder @ 11:10..11:49
                      message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                      severity: Major"#]],
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
                CalculationRegisterVirtualTableWithoutRecorder @ 13:10..13:49
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]],
            );
        }

        #[test]
        fn without_the_parameter_or_with_it_false_the_module_does_not_matter() {
            let off = check_diagnostics_for_with_config(FILE_QUERY, narrowed(false), CODE);
            expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 7:10..7:49
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]].assert_eq(&format_diags(FILE_QUERY, &off));
            check(
                FILE_QUERY,
                expect![[r#"
                CalculationRegisterVirtualTableWithoutRecorder @ 7:10..7:49
                  message: В условии виртуальной таблицы "РегистрРасчета.Начисления.ДанныеГрафика" нет отбора по регистратору
                  severity: Major"#]],
            );
        }
    }
}
