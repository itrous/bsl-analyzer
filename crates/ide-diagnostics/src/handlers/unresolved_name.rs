use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::Name;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: false,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    name: &Name,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = match ctx.locale() {
        base_db::Locale::Ru => format!("Имя '{}' не определено", name.as_str()),
        base_db::Locale::En => format!("Name '{}' is not defined", name.as_str()),
    };
    crate::simple_hir_diagnostic(DiagnosticCode::UnresolvedName, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_surfaces_report_issue_185_roots_once() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Процедура Тест()
    Результат = ГоризонтальноеПоложениеТабличногоДокумента.Право;
    Результат = РасположениеПолейКомпоновкиДанных.Вместе;
    Результат = ГоризонтальноеПоложение.Право;
    Результат = РасположениеПоляКомпоновкиДанных.Вместе;
    Локальная = СовершенноНеизвестноеИмя;
КонецПроцедуры
"#;
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        let diagnostics = check_with_cfe(source, fixture);
        let unresolved = diagnostics
            .iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
            .collect::<Vec<_>>();
        assert_eq!(
            unresolved.len(),
            3,
            "two issue roots and the unknown RHS must be reported exactly once: {diagnostics:?}"
        );
        for name in [
            "ГоризонтальноеПоложениеТабличногоДокумента",
            "РасположениеПолейКомпоновкиДанных",
            "СовершенноНеизвестноеИмя",
        ] {
            assert!(
                unresolved.iter().any(|diag| diag.message.contains(name)),
                "missing diagnostic for {name}: {unresolved:?}"
            );
        }
        assert!(
            unresolved.iter().all(|diag| {
                let text = &source[usize::from(diag.range.start())..usize::from(diag.range.end())];
                !text.contains('.')
            }),
            "diagnostics must cover only root tokens: {unresolved:?}"
        );
    }

    #[test]
    fn module_code_loop_backedge_declares_implicit_local() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Контроль = СовсемНеизвестноеИмя;
Пока Истина Цикл
    Чтение = ПоздняяВЦикле;
    ПоздняяВЦикле = 1;
КонецЦикла;
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        let unresolved = diagnostics
            .iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(unresolved.len(), 1, "only the unrelated control miss remains: {diagnostics:?}");
        assert!(unresolved[0].contains("СовсемНеизвестноеИмя"));
    }

    #[test]
    fn unknown_typed_assignment_still_declares_an_implicit_local() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Процедура Тест(Параметр)
    Локальная = Параметр;
    Результат = Локальная;
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName),
            "an Unknown type is still a resolved flow-local binding: {diagnostics:?}"
        );
    }

    #[test]
    fn module_variable_shadows_same_named_platform_function_on_read() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Перем Формат;
Процедура Тест()
    Результат = Формат;
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName),
            "a declared module variable must win over a same-named platform function: {diagnostics:?}"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn same_named_module_method_is_not_a_value_of_platform_function() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Процедура Формат()
КонецПроцедуры
Процедура Тест()
    Результат = Формат;
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        let unresolved = diagnostics
            .iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
            .collect::<Vec<_>>();
        assert_eq!(
            unresolved.len(),
            1,
            "only the non-first-class method read is unresolved: {diagnostics:?}"
        );
        assert!(unresolved[0].message.contains("Формат"));
    }

    #[test]
    fn indexed_assignment_declares_its_base_implicit_local() {
        use crate::test_utils::check_with_cfe;

        let source = "Таблица[0] = 1;\nРезультат = Таблица;";
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        let unresolved = diagnostics
            .iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
            .collect::<Vec<_>>();
        assert_eq!(
            unresolved.len(),
            1,
            "the indexed target keeps its pre-existing target-base diagnostic, but the later read must resolve: {diagnostics:?}"
        );
        assert_eq!(
            unresolved[0].range.start(),
            0.into(),
            "only the indexed assignment target may remain diagnosed"
        );
    }

    #[test]
    fn catalog_property_without_hbk_type_is_still_a_known_value() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Процедура Тест()
    Результат = СредстваПередачиДанныхНаУстройстве;
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName),
            "exact catalog membership must not depend on an HBK value type: {diagnostics:?}"
        );
    }

    #[test]
    fn qualified_call_has_one_root_owner() {
        use crate::test_utils::check_with_cfe;

        let source = r#"
Процедура Тест()
    НесуществующийКорень.Метод();
    НесуществующийВызов();
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert_eq!(
            diagnostics.iter().filter(|diag| diag.code == DiagnosticCode::UnresolvedName).count(),
            2,
            "each absent root is owned once: {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedMethodCall),
            "ReceiverNotResolved must not duplicate UnresolvedName: {diagnostics:?}"
        );
    }

    #[test]
    fn default_off_rule_keeps_the_legacy_qualified_call_fallback() {
        use crate::test_utils::check_with_cfe_config;

        let source = r#"
Процедура Тест()
    НесуществующийКорень.Метод();
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe_config(
            source,
            test_fixture::CfeFixtureBuilder::new("").build(),
            crate::DiagnosticsConfig::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diag| diag.code == DiagnosticCode::UnresolvedMethodCall)
                .count(),
            1,
            "default-off UnresolvedName must not create a diagnostic hole: {diagnostics:?}"
        );
        assert!(diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName));
    }

    #[test]
    fn call_and_value_capabilities_match_the_edt_probe() {
        use crate::test_utils::check_with_cfe;

        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(
            "Глобальный",
            "Процедура ЭкспортнаяПроцедура() Экспорт КонецПроцедуры",
        );
        let fixture = builder.build();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        std::fs::write(
            &app_path,
            "Перем ПеременнаяПриложения Экспорт;\n\
             Процедура ПроцедураПриложения() Экспорт КонецПроцедуры",
        )
        .unwrap();
        let source = r#"
Процедура Тест()
    Результат = СтрДлина;
    Результат = СтрДлина("x");
    ЭкспортнаяПроцедура();
    Результат = ЭкспортнаяПроцедура;
    ПроцедураПриложения();
    Результат = ПроцедураПриложения;
    Результат = ПеременнаяПриложения;
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe(source, fixture);
        let unresolved = diagnostics
            .iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
            .collect::<Vec<_>>();
        assert_eq!(unresolved.len(), 3, "only callable-as-value forms fail: {diagnostics:?}");
        for name in ["СтрДлина", "ЭкспортнаяПроцедура", "ПроцедураПриложения"]
        {
            assert!(unresolved.iter().any(|diag| diag.message.contains(name)));
        }
        assert!(!unresolved.iter().any(|diag| diag.message.contains("ПеременнаяПриложения")));
    }

    #[test]
    fn duplicate_application_host_exports_do_not_use_one_hosts_signature() {
        use crate::test_utils::check_with_cfe;

        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        for (file, body) in [
            (
                "ManagedApplicationModule.bsl",
                "Процедура ОбщаяПроцедура(Обязательный) Экспорт КонецПроцедуры",
            ),
            ("OrdinaryApplicationModule.bsl", "Процедура ОбщаяПроцедура() Экспорт КонецПроцедуры"),
        ] {
            let path = fixture.root().join("Ext").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let source = "Процедура Тест()\nОбщаяПроцедура();\nКонецПроцедуры";
        let diagnostics = check_with_cfe(source, fixture);
        assert!(
            diagnostics.iter().all(|diag| {
                !matches!(
                    diag.code,
                    DiagnosticCode::MissedRequiredParameter
                        | DiagnosticCode::MismatchedArgCount
                        | DiagnosticCode::UnresolvedName
                )
            }),
            "a call available through multiple application hosts must not inherit one host's incompatible signature: {diagnostics:?}"
        );
    }

    #[test]
    fn unread_application_module_keeps_unknown_name_indeterminate() {
        use crate::test_utils::check_with_cfe_unreadable;

        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        std::fs::write(&app_path, "Процедура МожетБытьЗдесь() Экспорт КонецПроцедуры").unwrap();
        let source = r#"
Процедура Тест()
    Результат = СовершенноНеизвестноеИмя;
КонецПроцедуры
"#;
        let diagnostics =
            check_with_cfe_unreadable(source, fixture, &["Ext/ManagedApplicationModule.bsl"]);
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName),
            "an unread application global surface cannot prove absence: {diagnostics:?}"
        );
    }

    /// A method of the implicit object, as the call channel already recognises
    /// it, must not be `UnresolvedName` when read as a bare callee. A value
    /// read of the same spelling, a name that kind does not own, and the same
    /// call in a module with no such receiver stay diagnosed.
    struct ObjectKind {
        collection: &'static str,
        element: &'static str,
        methods: &'static [&'static str],
        absent_calls: &'static [&'static str],
        attributes: &'static [&'static str],
    }

    fn object_kinds() -> Vec<ObjectKind> {
        vec![
            ObjectKind {
                collection: "Documents",
                element: "Document",
                methods: &[
                    "ЭтоНовый",
                    "IsNew",
                    "УстановитьНовыйНомер",
                    "ПроверитьЗаполнение",
                    "ПолучитьМакет",
                    "Записать",
                    "Заполнить",
                    "Модифицированность",
                    "УстановитьСсылкуНового",
                    "ПолучитьСсылкуНового",
                    "МоментВремени",
                ],
                absent_calls: &["УстановитьНовыйКод", "ЭтоГруппа", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "Catalogs",
                element: "Catalog",
                methods: &[
                    "ЭтоНовый",
                    "УстановитьНовыйКод",
                    "ПроверитьЗаполнение",
                    "ПолучитьМакет",
                    "Записать",
                    "Заполнить",
                    "Модифицированность",
                    "УстановитьСсылкуНового",
                    "ПолучитьСсылкуНового",
                ],
                absent_calls: &[
                    "УстановитьНовыйНомер",
                    "МоментВремени",
                    "ЭтоГруппа",
                    "СовсемНеизвестныйВызов",
                ],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "ExchangePlans",
                element: "ExchangePlan",
                methods: &["ЭтоНовый", "УстановитьНовыйКод", "ПроверитьЗаполнение", "Записать"],
                absent_calls: &["УстановитьНовыйНомер", "МоментВремени", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "ChartsOfAccounts",
                element: "ChartOfAccounts",
                methods: &["ЭтоНовый", "ПроверитьЗаполнение", "ПолучитьМакет", "Записать"],
                absent_calls: &[
                    "УстановитьНовыйНомер",
                    "УстановитьНовыйКод",
                    "МоментВремени",
                    "СовсемНеизвестныйВызов",
                ],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "Tasks",
                element: "Task",
                methods: &[
                    "ЭтоНовый",
                    "УстановитьНовыйНомер",
                    "ПроверитьЗаполнение",
                    "Модифицированность",
                ],
                absent_calls: &["УстановитьНовыйКод", "МоментВремени", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "BusinessProcesses",
                element: "BusinessProcess",
                methods: &["ЭтоНовый", "УстановитьНовыйНомер", "ПроверитьЗаполнение", "Записать"],
                absent_calls: &["УстановитьНовыйКод", "МоментВремени", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "ChartsOfCharacteristicTypes",
                element: "ChartOfCharacteristicTypes",
                methods: &["ЭтоНовый", "УстановитьНовыйКод", "ПроверитьЗаполнение", "Записать"],
                absent_calls: &[
                    "ПолучитьМакет",
                    "МоментВремени",
                    "ЭтоГруппа",
                    "СовсемНеизвестныйВызов",
                ],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "ChartsOfCalculationTypes",
                element: "ChartOfCalculationTypes",
                methods: &["ЭтоНовый", "ПроверитьЗаполнение", "ПолучитьМакет", "Записать"],
                absent_calls: &[
                    "УстановитьНовыйНомер",
                    "УстановитьНовыйКод",
                    "МоментВремени",
                    "СовсемНеизвестныйВызов",
                ],
                attributes: &["Сумма", "Ссылка", "ОбменДанными"],
            },
            ObjectKind {
                collection: "DataProcessors",
                element: "DataProcessor",
                methods: &["ПроверитьЗаполнение", "ПолучитьМакет"],
                absent_calls: &["ЭтоНовый", "Записать", "МоментВремени", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма"],
            },
            ObjectKind {
                collection: "Reports",
                element: "Report",
                methods: &["ПроверитьЗаполнение", "ПолучитьМакет"],
                absent_calls: &["ЭтоНовый", "Записать", "МоментВремени", "СовсемНеизвестныйВызов"],
                attributes: &["Сумма"],
            },
        ]
    }

    fn object_module_source(kind: &ObjectKind) -> String {
        let mut body = String::from("Процедура Тест()\n");
        for method in kind.methods {
            body.push_str(&format!("    {method}();\n"));
        }
        if kind.methods.contains(&"ЭтоНовый") {
            body.push_str("    ЭтотОбъект.ЭтоНовый();\n");
        }
        for attribute in kind.attributes {
            body.push_str(&format!("    Значение = {attribute};\n"));
        }
        // A method is not a value: the same spelling read, not called, stays absent.
        body.push_str("    Значение = ЭтоНовый;\n");
        for call in kind.absent_calls {
            body.push_str(&format!("    {call}();\n"));
        }
        body.push_str("КонецПроцедуры\n");
        body
    }

    fn write_object_xml(root: &std::path::Path, kind: &ObjectKind, name: &str) {
        let dir = root.join(kind.collection);
        std::fs::create_dir_all(&dir).expect("create collection directory");
        let xml = format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<{element} uuid="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"><Properties><Name>{name}</Name></Properties><ChildObjects>
<Attribute uuid="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeef"><Properties><Name>Сумма</Name><Type/></Properties></Attribute>
</ChildObjects></{element}></MetaDataObject>"#,
            element = kind.element,
        );
        std::fs::write(dir.join(format!("{name}.xml")), xml).expect("write object metadata");
    }

    fn tokens(
        source: &str,
        diagnostics: &[crate::Diagnostic],
        code: DiagnosticCode,
    ) -> Vec<String> {
        diagnostics
            .iter()
            .filter(|diag| diag.code == code)
            .map(|diag| {
                source[usize::from(diag.range.start())..usize::from(diag.range.end())].to_string()
            })
            .collect()
    }

    fn diagnosed_token<'a>(source: &'a str, diag: &crate::Diagnostic) -> (&'a str, &'a str) {
        let end = usize::from(diag.range.end());
        (&source[usize::from(diag.range.start())..end], source[end..].trim_start())
    }

    fn is_call_callee(
        source: &str,
        diagnostics: &[crate::Diagnostic],
        code: DiagnosticCode,
        name: &str,
    ) -> bool {
        diagnostics.iter().filter(|diag| diag.code == code).any(|diag| {
            let (token, after) = diagnosed_token(source, diag);
            token == name && after.starts_with('(')
        })
    }

    fn value_read_is_unresolved(
        source: &str,
        diagnostics: &[crate::Diagnostic],
        name: &str,
    ) -> bool {
        diagnostics.iter().filter(|diag| diag.code == DiagnosticCode::UnresolvedName).any(|diag| {
            let (token, after) = diagnosed_token(source, diag);
            token == name && !after.starts_with('(')
        })
    }

    #[test]
    fn object_module_bare_method_is_not_an_unresolved_name() {
        for kind in object_kinds() {
            let name = "Объект";
            let source = object_module_source(&kind);
            let fixture = test_fixture::CfeFixtureBuilder::new("").build();
            write_object_xml(fixture.root(), &kind, name);
            let relative = format!("{}/{name}/Ext/ObjectModule.bsl", kind.collection);
            let diagnostics = crate::test_utils::check_cfe_at_with_unreadable(
                &relative,
                &source,
                fixture,
                &[],
                crate::diagnostics,
            );
            let unresolved = tokens(&source, &diagnostics, DiagnosticCode::UnresolvedName);
            let bare = tokens(&source, &diagnostics, DiagnosticCode::UnresolvedBareCall);
            assert!(
                bare.is_empty(),
                "{}: UnresolvedName owns the token when it is on: bare={bare:?} unresolved={unresolved:?}",
                kind.collection
            );
            for method in kind.methods {
                assert!(
                    !is_call_callee(&source, &diagnostics, DiagnosticCode::UnresolvedName, method),
                    "{}: bare {method}() is a method of the implicit object: {unresolved:?}",
                    kind.collection
                );
            }
            for attribute in kind.attributes {
                assert!(
                    !unresolved.iter().any(|token| token == attribute),
                    "{}: attribute {attribute} must stay resolved: {unresolved:?}",
                    kind.collection
                );
            }
            for call in kind.absent_calls {
                assert!(
                    is_call_callee(&source, &diagnostics, DiagnosticCode::UnresolvedName, call),
                    "{}: {call}() is not a method of this object: {unresolved:?}",
                    kind.collection
                );
            }
            assert!(
                value_read_is_unresolved(&source, &diagnostics, "ЭтоНовый"),
                "{}: a value read of a method is still not a value: {unresolved:?}",
                kind.collection
            );
            if kind.methods.contains(&"ЭтоНовый") {
                assert!(
                    !is_call_callee(
                        &source,
                        &diagnostics,
                        DiagnosticCode::UnresolvedName,
                        "ЭтоНовый"
                    ),
                    "{}: the value-read diagnostic must not land on the call: {unresolved:?}",
                    kind.collection
                );
            }
        }
    }

    #[test]
    fn a_local_of_the_method_name_still_binds_the_value() {
        let source = r#"
Процедура Тест(ЭтоНовый)
    Значение = ЭтоНовый;
    ЭтоНовый();
    СовсемНеизвестныйВызов();
КонецПроцедуры
"#;
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        write_object_xml(
            fixture.root(),
            &ObjectKind {
                collection: "Documents",
                element: "Document",
                methods: &[],
                absent_calls: &[],
                attributes: &[],
            },
            "Объект",
        );
        let diagnostics = crate::test_utils::check_cfe_at_with_unreadable(
            "Documents/Объект/Ext/ObjectModule.bsl",
            source,
            fixture,
            &[],
            crate::diagnostics,
        );
        let unresolved = tokens(source, &diagnostics, DiagnosticCode::UnresolvedName);
        assert_eq!(
            unresolved,
            vec!["СовсемНеизвестныйВызов".to_string()],
            "the parameter owns the value read, the call is the object method: {diagnostics:?}"
        );
        assert!(diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedBareCall));
    }

    /// An export root has no collection directory. The call channel still
    /// looks up methods of `ExternalDataProcessor` / `ExternalReport` once the
    /// owner is attached; the callee read must use that same recognition.
    #[test]
    fn external_object_module_bare_method_is_not_an_unresolved_name() {
        for (element, kind) in [
            ("ExternalDataProcessor", bsl_metadata::ExternalObjectKind::DataProcessor),
            ("ExternalReport", bsl_metadata::ExternalObjectKind::Report),
        ] {
            let diagnostics = external_object_diagnostics(element, kind);
            let source = external_object_source();
            let unresolved = tokens(&source, &diagnostics, DiagnosticCode::UnresolvedName);
            assert!(
                diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedBareCall),
                "{element}: UnresolvedBareCall must defer: {diagnostics:?}"
            );
            for method in ["ПроверитьЗаполнение", "ПолучитьМакет"] {
                assert!(
                    !is_call_callee(&source, &diagnostics, DiagnosticCode::UnresolvedName, method),
                    "{element}: bare {method}() is a method of the export: {unresolved:?}"
                );
            }
            assert!(
                !unresolved.iter().any(|token| token == "Сумма"),
                "{element}: own attribute must stay resolved: {unresolved:?}"
            );
            for call in ["ЭтоНовый", "СовсемНеизвестныйВызов"] {
                assert!(
                    is_call_callee(&source, &diagnostics, DiagnosticCode::UnresolvedName, call),
                    "{element}: {call}() is not a method of this export: {unresolved:?}"
                );
            }
        }
    }

    fn external_object_source() -> String {
        "\
Процедура Тест()
    ПроверитьЗаполнение();
    ПолучитьМакет();
    Значение = Сумма;
    ЭтоНовый();
    СовсемНеизвестныйВызов();
КонецПроцедуры
"
        .to_string()
    }

    fn external_object_diagnostics(
        element: &str,
        kind: bsl_metadata::ExternalObjectKind,
    ) -> Vec<crate::Diagnostic> {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::metadata::{RootKind, WorkspaceConfigsSnapshot};
        use ide_db::RootDatabaseImpl;
        use vfs::{FileId, FileSet, VfsPath};

        let root = std::env::temp_dir().join(format!(
            "bsl_external_object_bare_{}_{}",
            std::process::id(),
            element
        ));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("cf");
        let export = root.join("export");
        std::fs::create_dir_all(base.join("CommonModules")).expect("base");
        std::fs::write(base.join("Configuration.xml"), "<MetaDataObject/>").expect("base xml");
        let name = "Внешняя";
        std::fs::create_dir_all(export.join(name).join("Ext")).expect("export module dir");
        let xml = format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<{element} uuid="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"><Properties><Name>{name}</Name></Properties><ChildObjects>
<Attribute uuid="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeef"><Properties><Name>Сумма</Name><Type/></Properties></Attribute>
</ChildObjects></{element}></MetaDataObject>"#
        );
        std::fs::write(export.join(format!("{name}.xml")), xml).expect("export xml");

        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let module_path = export.join(name).join("Ext/ObjectModule.bsl");
        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new(module_path.to_string_lossy().into_owned()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, &external_object_source());

        let paths = vec![(None, base.clone()), (Some("EXPORT".to_string()), export.clone())];
        db.set_workspace_configs_snapshot(WorkspaceConfigsSnapshot {
            extension_scopes: vec![None, None],
            canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
            kinds: vec![RootKind::Base, RootKind::External(kind)],
            paths,
            closures: vec![vec![], vec![]],
            topological_order: vec![0, 1],
            fingerprint: Some(element.to_string()),
            source_exclusions: Default::default(),
        });

        let config_path = export.to_string_lossy();
        let config_path_input = ide_db::metadata::intern_configuration_path(
            &db,
            config_path.as_ref(),
            db.config_root_revision_for_path(std::path::Path::new(config_path.as_ref())),
        );
        let config = crate::DiagnosticsConfig::all_enabled();
        let provider = ide_db::SalsaProvider::new(&db, Some(config_path_input));
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);
        let diagnostics = crate::diagnostics(&ctx);
        let _ = std::fs::remove_dir_all(&root);
        diagnostics
    }

    /// With `UnresolvedName` off, the call channel still owns the token: a
    /// method of the implicit object stays silent, a name it does not own is
    /// `UnresolvedBareCall`.
    #[test]
    fn default_profile_keeps_the_call_channel_for_an_object_module() {
        let source = "\
Процедура Тест()
    ЭтоНовый();
    СовсемНеизвестныйВызов();
КонецПроцедуры
";
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        write_object_xml(
            fixture.root(),
            &ObjectKind {
                collection: "Documents",
                element: "Document",
                methods: &[],
                absent_calls: &[],
                attributes: &[],
            },
            "Объект",
        );
        let diagnostics = crate::test_utils::check_cfe_at_with_unreadable_config_and_setup(
            "Documents/Объект/Ext/ObjectModule.bsl",
            source,
            fixture,
            &[],
            crate::DiagnosticsConfig::default(),
            |_| {},
            crate::diagnostics,
        );
        assert!(
            !is_call_callee(source, &diagnostics, DiagnosticCode::UnresolvedBareCall, "ЭтоНовый"),
            "the call channel already recognises ЭтоНовый(): {diagnostics:?}"
        );
        assert!(
            is_call_callee(
                source,
                &diagnostics,
                DiagnosticCode::UnresolvedBareCall,
                "СовсемНеизвестныйВызов",
            ),
            "an unknown bare call stays on the call channel: {diagnostics:?}"
        );
        assert!(diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedName));
    }

    #[test]
    fn a_common_module_has_no_implicit_object_method() {
        let source = r#"
Процедура Тест()
    ЭтоНовый();
КонецПроцедуры
"#;
        let diagnostics = crate::test_utils::check_with_cfe(
            source,
            test_fixture::CfeFixtureBuilder::new("").build(),
        );
        assert!(
            is_call_callee(source, &diagnostics, DiagnosticCode::UnresolvedName, "ЭтоНовый"),
            "a common module has no implicit object: {diagnostics:?}"
        );
        assert!(diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedBareCall));
    }

    #[test]
    fn metadata_keeps_the_rule_default_off_until_corpus_gates_pass() {
        let metadata = crate::handlers::get_metadata(DiagnosticCode::UnresolvedName).unwrap();
        assert_eq!(metadata.diagnostic_type, DiagnosticType::Error);
        assert_eq!(metadata.severity, DiagnosticSeverityLevel::Major);
        assert!(!metadata.activated_by_default);
        assert!(crate::DiagnosticsConfig::default().is_disabled(DiagnosticCode::UnresolvedName));
        assert!(
            !crate::DiagnosticsConfig::all_enabled().is_disabled(DiagnosticCode::UnresolvedName)
        );
    }
}
