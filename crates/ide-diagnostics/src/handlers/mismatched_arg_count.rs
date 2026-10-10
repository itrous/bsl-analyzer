use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 3,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    required_count: usize,
    total_count: usize,
    found: usize,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = if required_count == total_count {
        format!("Неверное количество аргументов: ожидалось {required_count}, передано {found}")
    } else {
        format!(
            "Неверное количество аргументов: ожидалось от {required_count} до {total_count}, передано {found}"
        )
    };
    crate::simple_hir_diagnostic(DiagnosticCode::MismatchedArgCount, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_hir_diagnostic_with_fixtures;
    use crate::DiagnosticCode;

    #[test]
    fn emits_when_arg_count_differs_from_signature() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Процедура Сложение(Левый, Правый) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ОбщийМодуль.Сложение(1);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(mismatched.len(), 1, "expected one MismatchedArgCount, got: {diags:?}");
        assert!(mismatched[0].message.contains("2") && mismatched[0].message.contains("1"));
    }

    #[test]
    fn does_not_fire_when_optional_args_are_omitted() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция ПодставитьПараметры(Шаблон, П1, П2 = Неопределено, П3 = Неопределено,
    П4 = Неопределено, П5 = Неопределено, П6 = Неопределено,
    П7 = Неопределено, П8 = Неопределено, П9 = Неопределено) Экспорт
    Возврат Шаблон;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.ПодставитьПараметры("шаблон", 1, 2, 3);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert!(
            mismatched.is_empty(),
            "expected no MismatchedArgCount when call has 4 args within [2, 10] range, got: {diags:?}"
        );
    }

    #[test]
    fn emits_range_message_when_fewer_than_required() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция Метод(А, Б, В = Неопределено) Экспорт
    Возврат А;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.Метод(1);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(mismatched.len(), 1, "expected one MismatchedArgCount, got: {diags:?}");
        assert!(
            mismatched[0].message.contains("от 2 до 3")
                && mismatched[0].message.contains("передано 1"),
            "expected range form 'от 2 до 3, передано 1', got: {}",
            mismatched[0].message
        );
    }

    #[test]
    fn emits_when_more_than_total_args() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция Метод(А, Б = Неопределено) Экспорт
    Возврат А;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.Метод(1, 2, 3);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(mismatched.len(), 1, "expected one MismatchedArgCount, got: {diags:?}");
        assert!(
            mismatched[0].message.contains("от 1 до 2")
                && mismatched[0].message.contains("передано 3"),
            "expected 'от 1 до 2, передано 3', got: {}",
            mismatched[0].message
        );
    }

    #[test]
    fn non_standard_optional_in_middle_accepts_full_arity_call() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция Метод(А, Б = Неопределено, В) Экспорт
    Возврат А;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.Метод(1, 2, 3);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert!(
            mismatched.is_empty(),
            "non-standard order with all 3 args supplied must not fire MismatchedArgCount, got: {diags:?}"
        );
    }

    #[test]
    fn skipped_args_in_optional_slot_pass_arity_check() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция Метод(А, Б = Неопределено, В = Неопределено) Экспорт
    Возврат А;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.Метод(1, , 3);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert!(
            mismatched.is_empty(),
            "Foo(1,,3) against (А, Б = ..., В = ...) must not fire — Expr::Missing fills slot 2, args.len()=3=total. Got: {diags:?}"
        );
    }

    #[test]
    fn multi_overload_attach_addin_two_args_silent() {
        let fixture = r#"
//- /test.bsl
Процедура Тест(Местоположение, Идентификатор)
    Результат = ПодключитьВнешнююКомпоненту(Местоположение, Идентификатор + "SymbolicName");
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert!(
            mismatched.is_empty(),
            "ПодключитьВнешнююКомпоненту(Loc, Name) hits the 'По имени и местоположению' overload, must not fire MismatchedArgCount; got: {diags:?}"
        );
    }

    #[test]
    fn multi_overload_attach_addin_one_arg_silent() {
        let fixture = r#"
//- /test.bsl
Процедура Тест(ProgID)
    Результат = ПодключитьВнешнююКомпоненту(ProgID);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert!(
            mismatched.is_empty(),
            "ПодключитьВнешнююКомпоненту(ProgID) hits the COM 'По идентификатору' overload; got: {diags:?}"
        );
    }

    #[test]
    fn multi_overload_xml_reader_get_attribute_string_arg_silent() {
        let fixture = r#"
//- /test.bsl
Процедура Тест()
    Чтение = Новый ЧтениеXML;
    ИмяТаблицы = Чтение.ПолучитьАтрибут("Description");
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let blockers: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(d.code, DiagnosticCode::MismatchedArgCount | DiagnosticCode::TypeMismatch)
            })
            .collect();
        assert!(
            blockers.is_empty(),
            "ЧтениеXML.ПолучитьАтрибут(\"...\") hits 'По полному имени' overload — neither TypeMismatch nor MismatchedArgCount must fire; got: {diags:?}"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn multi_overload_attach_addin_zero_args_fires() {
        let fixture = r#"
//- /test.bsl
Процедура Тест()
    Результат = ПодключитьВнешнююКомпоненту();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(
            mismatched.len(),
            1,
            "0-arg call satisfies no overload; expected one MismatchedArgCount, got: {diags:?}"
        );
    }

    #[test]
    fn non_standard_optional_in_middle_requires_all_args() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Функция Метод(А, Б = Неопределено, В) Экспорт
    Возврат А;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщийМодуль.Метод(1, 2);
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(
            mismatched.len(),
            1,
            "non-standard optional-in-middle: 2 args must trigger diagnostic, got: {diags:?}"
        );
        assert!(
            mismatched[0].message.contains("ожидалось 3"),
            "expected single-number form 'ожидалось 3' (required==total), got: {}",
            mismatched[0].message
        );
    }

    /// An export of a GLOBAL common module shadows the platform global of the same
    /// name. So when such a module has a body nobody could read, the platform
    /// signature is not the one this call is measured against — the unknown body may
    /// declare its own `СтрДлина`, and measuring against the platform's is a guess
    /// charged to the caller.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn an_unread_global_body_bars_the_platform_signature_from_answering() {
        use crate::test_utils::check_with_cfe_unreadable;
        use test_fixture::CfeFixtureBuilder;

        let code = "Процедура Тест() Экспорт\nСтрДлина();\nКонецПроцедуры";
        let fixture = || {
            let mut builder = CfeFixtureBuilder::new("");
            builder
                .add_base_module_global("Глоб", "Процедура Иное() Экспорт КонецПроцедуры")
                .add_extension("Расш", "");
            builder.build()
        };

        // Control: with the global body readable the call really is measured against the
        // platform signature and IS accused — so the silence below is the barrier.
        let control = check_with_cfe_unreadable(code, fixture(), &[]);
        assert!(
            control.iter().any(|d| d.code == DiagnosticCode::MismatchedArgCount),
            "control: the platform arity check must fire, got {control:?}"
        );

        let unread =
            check_with_cfe_unreadable(code, fixture(), &["CommonModules/Глоб/Ext/Module.bsl"]);
        assert!(
            !unread.iter().any(|d| d.code == DiagnosticCode::MismatchedArgCount),
            "an unread global body may declare this very name, got {unread:?}"
        );
    }
}
