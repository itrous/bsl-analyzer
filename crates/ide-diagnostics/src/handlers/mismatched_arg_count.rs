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

    /// The rule's messages for `body` as the whole of the server common module `Проба`.
    fn in_server_module(body: &str) -> Vec<String> {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module("Проба", body);
        crate::test_utils::check_cfe_at_with_db_setup(
            "CommonModules/Проба/Ext/Module.bsl",
            body,
            builder.build(),
            &[],
            crate::DiagnosticsConfig::default(),
            |_| {},
            |_| {},
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::MismatchedArgCount)
        .map(|diag| diag.message)
        .collect()
    }

    /// The platform drops a `#Если` branch the module's environment rules out before
    /// compiling it, so the call there is never checked: `#Если Сервер И Не Сервер`
    /// (a typing hint for the editor) and a client branch of a server module compile
    /// nowhere. The same call in the plain body and in a branch the server does
    /// compile is still reported.
    #[test]
    fn a_branch_no_environment_compiles_is_not_judged() {
        let module = |call_site: &str| {
            in_server_module(&format!(
                "Процедура Сложение(Левый, Правый) Экспорт\nКонецПроцедуры\n\n\
                 Процедура Тест() Экспорт\n{call_site}КонецПроцедуры\n"
            ))
        };
        const CALL: &str = "    Сложение(1);\n";

        let plain = module(CALL);
        assert_eq!(plain.len(), 1, "the server compiles the plain call: {plain:?}");
        let compiled = module(&format!("#Если Сервер Тогда\n{CALL}#КонецЕсли\n"));
        assert_eq!(compiled.len(), 1, "the server compiles this branch: {compiled:?}");

        let contradiction = module(&format!("#Если Сервер И Не Сервер Тогда\n{CALL}#КонецЕсли\n"));
        assert!(contradiction.is_empty(), "no environment compiles this branch: {contradiction:?}");
        let client =
            module(&format!("#Если ТолстыйКлиентОбычноеПриложение Тогда\n{CALL}#КонецЕсли\n"));
        assert!(client.is_empty(), "a server module never compiles this branch: {client:?}");
    }

    /// A branch the condition cannot decide may be compiled, so its calls are
    /// judged: `Линукс` is no symbol of the help, and an `#Иначе` after such a
    /// branch is no more decided than the branch itself.
    #[test]
    fn an_undecided_branch_is_judged() {
        let module = |call_site: &str| {
            in_server_module(&format!(
                "Процедура Сложение(Левый, Правый) Экспорт\nКонецПроцедуры\n\n\
                 Процедура Тест() Экспорт\n{call_site}КонецПроцедуры\n"
            ))
        };
        const CALL: &str = "    Сложение(1);\n";

        let then_branch = module(&format!("#Если Линукс Тогда\n{CALL}#КонецЕсли\n"));
        assert_eq!(then_branch.len(), 1, "the condition is undecided: {then_branch:?}");
        let else_branch = module(&format!("#Если Линукс Тогда\n#Иначе\n{CALL}#КонецЕсли\n"));
        assert_eq!(else_branch.len(), 1, "the chain is undecided: {else_branch:?}");
        let after_taken = module(&format!("#Если Сервер Тогда\n#Иначе\n{CALL}#КонецЕсли\n"));
        assert!(after_taken.is_empty(), "the server takes the first branch: {after_taken:?}");
    }

    /// The session module runs on the server and in the external connection, where
    /// `Сервер` is undecided: a branch nested under `#Если Сервер` may still be
    /// compiled there, while a contradiction is false whatever `Сервер` holds.
    #[test]
    fn a_branch_is_dead_only_when_every_environment_rules_it_out() {
        let module = |call_site: &str| {
            let fixture = format!(
                "//- /Ext/SessionModule.bsl\n\
                 Процедура Сложение(Левый, Правый) Экспорт\nКонецПроцедуры\n\n\
                 Процедура Тест() Экспорт\n{call_site}КонецПроцедуры\n"
            );
            check_hir_diagnostic_with_fixtures(&fixture)
                .into_iter()
                .filter(|diag| diag.code == DiagnosticCode::MismatchedArgCount)
                .map(|diag| diag.message)
                .collect::<Vec<_>>()
        };
        const CALL: &str = "    Сложение(1);\n";

        let plain = module(CALL);
        assert_eq!(plain.len(), 1, "the session module compiles the plain call: {plain:?}");
        let nested = module(&format!(
            "#Если Сервер Тогда\n#Если Не Сервер Тогда\n{CALL}#КонецЕсли\n#КонецЕсли\n"
        ));
        assert_eq!(nested.len(), 1, "the external connection may compile it: {nested:?}");

        let contradiction = module(&format!("#Если Сервер И Не Сервер Тогда\n{CALL}#КонецЕсли\n"));
        assert!(contradiction.is_empty(), "no environment compiles this branch: {contradiction:?}");
        let client = module(&format!("#Если ТонкийКлиент Тогда\n{CALL}#КонецЕсли\n"));
        assert!(client.is_empty(), "neither environment is a thin client: {client:?}");
    }

    /// A module of unknown environment may compile any of its branches.
    #[test]
    fn a_branch_of_a_module_of_unknown_environment_is_judged() {
        let fixture = r#"
//- /test.bsl
Процедура Сложение(Левый, Правый) Экспорт
КонецПроцедуры

Процедура Тест()
#Если Сервер Тогда
    Сложение(1);
#КонецЕсли
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let mismatched: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(mismatched.len(), 1, "expected one MismatchedArgCount, got: {diags:?}");
    }

    /// A method declared in a module-level branch no environment compiles is
    /// dropped with the branch; one under an undecided region may be compiled.
    #[test]
    fn a_method_in_a_module_level_branch_no_environment_compiles_is_not_judged() {
        let module = |condition: &str| {
            in_server_module(&format!(
                "Процедура Сложение(Левый, Правый) Экспорт\nКонецПроцедуры\n\n\
                 #Если {condition} Тогда\n\
                 Процедура Тест() Экспорт\n    Сложение(1);\nКонецПроцедуры\n\
                 #КонецЕсли\n"
            ))
        };

        let dead = module("Сервер И Не Сервер");
        assert!(dead.is_empty(), "no environment compiles the method: {dead:?}");
        let client = module("ТолстыйКлиентОбычноеПриложение");
        assert!(client.is_empty(), "a server module never compiles the method: {client:?}");
        let compiled = module("Сервер");
        assert_eq!(compiled.len(), 1, "the server compiles the method: {compiled:?}");
        let undecided = module("Линукс");
        assert_eq!(undecided.len(), 1, "the region is undecided: {undecided:?}");
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
