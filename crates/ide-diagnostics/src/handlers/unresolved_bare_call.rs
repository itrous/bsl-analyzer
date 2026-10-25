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
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    name: &Name,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    // The Russian wording is the platform's own runtime text, so a reader who has
    // seen the module fail to compile recognises the same sentence here.
    let message = match ctx.locale() {
        base_db::Locale::Ru => {
            format!("Процедура или функция с указанным именем не определена: '{}'", name.as_str())
        }
        base_db::Locale::En => {
            format!("Procedure or function '{}' is not defined", name.as_str())
        }
    };
    crate::simple_hir_diagnostic(DiagnosticCode::UnresolvedBareCall, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_with_cfe_config, check_with_cfe_unreadable_config};
    use crate::{DiagnosticCode, DiagnosticsConfig};

    /// The rule defers to `UnresolvedName` wherever that one is on, so every
    /// behavioural stand runs on the default profile, where it is off.
    fn bare_calls(source: &str, fixture: test_fixture::CfeFixture) -> Vec<String> {
        check_with_cfe_config(source, fixture, DiagnosticsConfig::default())
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedBareCall)
            .map(|diag| diag.message)
            .collect()
    }

    #[test]
    fn absent_bare_call_is_reported() {
        let source = r#"
Процедура Тест()
    ДекодироватьСтроку("x");
КонецПроцедуры
"#;
        let reported = bare_calls(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert_eq!(reported.len(), 1, "a name owned by nothing must be reported: {reported:?}");
        assert!(reported[0].contains("ДекодироватьСтроку"), "{reported:?}");
    }

    #[test]
    fn absent_same_module_call_is_reported() {
        let source = r#"
Процедура Тест()
    ЛокальнаяКоторойНет();
КонецПроцедуры
"#;
        let reported = bare_calls(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert_eq!(reported.len(), 1, "a missing sibling method must be reported: {reported:?}");
        assert!(reported[0].contains("ЛокальнаяКоторойНет"), "{reported:?}");
    }

    #[test]
    fn same_module_method_platform_global_and_local_are_silent() {
        let source = r#"
Процедура Сосед()
КонецПроцедуры

Функция СоседняяФункция()
    Возврат 1;
КонецФункции

Процедура Тест(Параметр)
    Сосед();
    Результат = СоседняяФункция();
    Результат = СтрДлина("x");
    Результат = РаскодироватьСтроку("x", СпособКодированияСтроки.КодировкаURL);
    Локальная = Новый Массив;
    Локальная.Добавить(Результат);
    Параметр.Метод();
КонецПроцедуры
"#;
        let reported = bare_calls(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert!(reported.is_empty(), "resolved bare calls must stay silent: {reported:?}");
    }

    /// 8.2-era globals the platform still compiles but no catalog lists (checked live
    /// on 8.3.17 and 8.3.27); a name the platform does not compile next to them is
    /// still reported.
    #[test]
    fn undocumented_platform_globals_are_silent() {
        let source = r#"
Процедура Тест()
    Код = КодЛокализации();
    Код = localecode();
    УстановитьЗаголовокПриложения(ПолучитьЗаголовокПриложения());
    SetApplicationCaption(GetApplicationCaption());
    УстановитьЗаголовокСистемы(ПолучитьЗаголовокСистемы());
    Код = ОпределитьЭтаИнформационнаяБазаФайловая();
КонецПроцедуры
"#;
        let reported = bare_calls(source, test_fixture::CfeFixtureBuilder::new("").build());
        assert_eq!(reported.len(), 1, "only the name the platform lacks: {reported:?}");
        assert!(reported[0].contains("ОпределитьЭтаИнформационнаяБазаФайловая"), "{reported:?}");
    }

    /// A common module with every environment flag off is compiled nowhere, so a call
    /// in it cannot fail; the same body in a server module is reported.
    #[test]
    fn a_common_module_compiled_nowhere_stays_silent() {
        const BODY: &str = "Процедура Проба() Экспорт\n    НетТакойПроцедуры();\nКонецПроцедуры\n";
        fn run(compiled_nowhere: bool) -> Vec<String> {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module("Проба", BODY);
            crate::test_utils::check_cfe_at_with_unreadable_config_and_setup(
                "CommonModules/Проба/Ext/Module.bsl",
                BODY,
                builder.build(),
                &[],
                DiagnosticsConfig::default(),
                |fixture| {
                    if !compiled_nowhere {
                        return;
                    }
                    let path = fixture.root().join("CommonModules/Проба.xml");
                    let xml = std::fs::read_to_string(&path).expect("read module metadata");
                    std::fs::write(
                        &path,
                        xml.replace("<Server>true</Server>", "<Server>false</Server>"),
                    )
                    .expect("write module metadata");
                },
                crate::diagnostics,
            )
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedBareCall)
            .map(|diag| diag.message)
            .collect()
        }
        assert_eq!(run(false).len(), 1, "a server module compiles the call: {:?}", run(false));
        assert!(run(true).is_empty(), "a module compiled nowhere cannot fail: {:?}", run(true));
    }

    #[test]
    fn global_common_module_export_is_silent() {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(
            "Глобальный",
            "Процедура ЭкспортнаяПроцедура() Экспорт КонецПроцедуры",
        );
        let source = r#"
Процедура Тест()
    ЭкспортнаяПроцедура();
КонецПроцедуры
"#;
        let reported = bare_calls(source, builder.build());
        assert!(
            reported.is_empty(),
            "an export of a global common module is callable bare: {reported:?}"
        );
    }

    /// The effective body of a `&ИзменениеИКонтроль` method is inferred without a known
    /// environment, so nothing proves a `#Если` branch in it compiled; the call outside
    /// the branch is still reported.
    #[test]
    fn a_branch_of_a_change_and_validate_body_stays_silent() {
        fn run(insert: &str) -> Vec<String> {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module("Сервер", "Процедура Цель() Экспорт\nКонецПроцедуры\n");
            let ext = format!(
                "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель()\n#Вставка\n{insert}#КонецВставки\nКонецПроцедуры\n"
            );
            builder.add_extension("Расш", "");
            crate::test_utils::check_cfe_at_with_db(
                "CommonModules/Сервер/Ext/Module.bsl",
                &ext,
                builder.build(),
                &[],
                DiagnosticsConfig::default(),
                |_| {},
                |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
            )
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedBareCall)
            .map(|diag| diag.message)
            .collect()
        }
        let outside = run("\tНетТакойПроцедуры();\n");
        assert_eq!(outside.len(), 1, "the inserted call is compiled: {outside:?}");
        let branch = run("#Если Клиент Тогда\n\tНетТакойПроцедуры();\n#КонецЕсли\n");
        assert!(branch.is_empty(), "an unproven branch cannot fail the module: {branch:?}");
    }

    /// `#Вставка` code runs in the extension module, so a call to a method the extension
    /// declares itself resolves there; the base's own methods stay callable, and a name
    /// neither module declares is still reported (the effective pass is alive).
    #[test]
    fn a_change_and_validate_insertion_sees_the_extension_methods() {
        fn run(insert: &str) -> Vec<String> {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module(
                "Сервер",
                "Процедура Цель() Экспорт\nКонецПроцедуры\n\nПроцедура Проверить()\nКонецПроцедуры\n",
            );
            let ext = format!(
                "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель()\n#Вставка\n{insert}#КонецВставки\nКонецПроцедуры\n\nПроцедура Расш_Дополнить()\nКонецПроцедуры\n"
            );
            builder.add_extension("Расш", "");
            crate::test_utils::check_cfe_at_with_db(
                "CommonModules/Сервер/Ext/Module.bsl",
                &ext,
                builder.build(),
                &[],
                DiagnosticsConfig::default(),
                |_| {},
                |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
            )
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedBareCall)
            .map(|diag| diag.message)
            .collect()
        }
        let own = run("\tРасш_Дополнить();\n");
        assert!(own.is_empty(), "a method of the extension is callable: {own:?}");
        let base = run("\tПроверить();\n");
        assert!(base.is_empty(), "a method of the base is callable: {base:?}");
        let both = run("\tРасш_Дополнить();\n\tНетТакойПроцедуры();\n");
        assert_eq!(both.len(), 1, "only the unknown name is reported: {both:?}");
    }

    /// An export variable of the application module is a value, not a method: calling
    /// it by name is still a call of nothing.
    #[test]
    fn a_name_only_a_global_export_variable_holds_is_still_absent() {
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        std::fs::write(&app_path, "Перем ГлобальнаяПеременная Экспорт;\n").unwrap();
        let source = r#"
Процедура Тест()
    ГлобальнаяПеременная();
КонецПроцедуры
"#;
        let reported = bare_calls(source, fixture);
        assert_eq!(reported.len(), 1, "a global export variable owns no call: {reported:?}");
        assert!(reported[0].contains("ГлобальнаяПеременная"), "{reported:?}");
    }

    /// One `BareNameGap` is enough to withhold the verdict: an application module
    /// that exists but could not be read may export this very name.
    #[test]
    fn unread_global_surface_keeps_the_call_undiagnosed() {
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        std::fs::write(&app_path, "Процедура МожетБытьЗдесь() Экспорт КонецПроцедуры").unwrap();
        let source = r#"
Процедура Тест()
    СовершенноНеизвестныйВызов();
КонецПроцедуры
"#;
        let diagnostics = check_with_cfe_unreadable_config(
            source,
            fixture,
            &["Ext/ManagedApplicationModule.bsl"],
            DiagnosticsConfig::default(),
        );
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedBareCall),
            "an unread global surface cannot prove absence: {diagnostics:?}"
        );
    }

    /// `UnresolvedName` covers the same token with the same verdict, so the two
    /// must never land on one range — the precedent `ReceiverNameAbsent` set.
    #[test]
    fn defers_to_the_broader_rule_when_that_one_is_enabled() {
        let source = r#"
Процедура Тест()
    ДекодироватьСтроку("x");
КонецПроцедуры
"#;
        let diagnostics = crate::test_utils::check_with_cfe(
            source,
            test_fixture::CfeFixtureBuilder::new("").build(),
        );
        assert!(
            diagnostics.iter().all(|diag| diag.code != DiagnosticCode::UnresolvedBareCall),
            "UnresolvedName already owns the token: {diagnostics:?}"
        );
        assert_eq!(
            diagnostics.iter().filter(|diag| diag.code == DiagnosticCode::UnresolvedName).count(),
            1,
            "the broader rule must still report it: {diagnostics:?}"
        );
    }

    #[test]
    fn metadata_matches_the_qualified_call_rule() {
        let bare = crate::handlers::get_metadata(DiagnosticCode::UnresolvedBareCall).unwrap();
        let qualified =
            crate::handlers::get_metadata(DiagnosticCode::UnresolvedMethodCall).unwrap();
        assert_eq!(bare.severity, qualified.severity);
        assert_eq!(bare.diagnostic_type, qualified.diagnostic_type);
        assert!(bare.activated_by_default);
        assert!(!DiagnosticsConfig::default().is_disabled(DiagnosticCode::UnresolvedBareCall));
    }
}
