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
