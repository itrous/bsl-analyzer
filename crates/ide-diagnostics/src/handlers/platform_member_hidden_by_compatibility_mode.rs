use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::PlatformVersion;
use hir::LocalRange;
use hir::Name;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

/// Modes are shown as the platform names them (`8.2.13`), never with a build.
fn mode_text(version: PlatformVersion) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

fn message(
    name: &str,
    mode: PlatformVersion,
    visible_from: PlatformVersion,
    ctx: &AnalysisContext,
) -> String {
    let (mode, visible_from) = (mode_text(mode), mode_text(visible_from));
    match ctx.locale() {
        base_db::Locale::Ru => format!(
            "'{name}' недоступен в режиме совместимости {mode}: виден с режима {visible_from}"
        ),
        base_db::Locale::En => format!(
            "'{name}' is not available in compatibility mode {mode}: it is visible from mode {visible_from}"
        ),
    }
}

/// A platform name resolved by inference that the configuration's compatibility
/// mode hides ([`hir::compat_mode`]).
pub fn from_hir(
    name: &Name,
    mode: PlatformVersion,
    visible_from: PlatformVersion,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    crate::simple_hir_diagnostic(
        DiagnosticCode::PlatformMemberHiddenByCompatibilityMode,
        message(name.as_str(), mode, visible_from, ctx),
        range,
        ctx,
    )
}

#[cfg(test)]
mod tests {
    use crate::metadata::DiagnosticSeverityLevel;
    use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};

    fn run(
        source: &str,
        mode: Option<&str>,
        builder: test_fixture::CfeFixtureBuilder,
    ) -> Vec<Diagnostic> {
        let mode = mode.map(std::sync::Arc::<str>::from);
        crate::test_utils::check_cfe_at_with_db_setup(
            "CommonModules/Caller/Ext/Module.bsl",
            source,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |_| {},
            |db| db.set_compatibility_mode(mode),
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::PlatformMemberHiddenByCompatibilityMode)
        .collect()
    }

    fn hidden(source: &str, mode: Option<&str>) -> Vec<String> {
        run(source, mode, test_fixture::CfeFixtureBuilder::new(""))
            .into_iter()
            .map(|diag| diag.message)
            .collect()
    }

    const STR_SPLIT: &str = r#"
Процедура Тест()
    Части = СтрРазделить("a,b", ",");
КонецПроцедуры
"#;

    #[test]
    fn a_global_hidden_by_an_8_2_mode_is_reported() {
        let reported = hidden(STR_SPLIT, Some("Version8_2_13"));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(
            reported[0],
            "'СтрРазделить' недоступен в режиме совместимости 8.2.13: виден с режима 8.3.6"
        );
    }

    #[test]
    fn the_same_global_in_a_later_mode_is_silent() {
        assert!(hidden(STR_SPLIT, Some("Version8_3_17")).is_empty());
        assert!(hidden(STR_SPLIT, Some("8.3.6")).is_empty(), "8.3.6 is the threshold itself");
    }

    #[test]
    fn bitwise_functions_need_mode_8_3_11() {
        let source = r#"
Процедура Тест()
    Маска = ПобитовоеИ(6, 3);
    Позиция = СтрНайти("abc", "b");
КонецПроцедуры
"#;
        let reported = hidden(source, Some("8.3.10"));
        assert_eq!(reported.len(), 1, "СтрНайти is visible from 8.3.6: {reported:?}");
        assert!(reported[0].contains("'ПобитовоеИ'") && reported[0].contains("8.3.11"));
    }

    #[test]
    fn a_regex_function_in_mode_8_3_17_is_not_this_rule() {
        let source = r#"
Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
КонецПроцедуры
"#;
        assert!(hidden(source, Some("Version8_3_17")).is_empty());
        let in_8_3_8 = hidden(source, Some("Version8_3_8"));
        assert_eq!(in_8_3_8.len(), 1, "{in_8_3_8:?}");
    }

    #[test]
    fn dont_use_unknown_and_garbage_modes_are_silent() {
        assert!(hidden(STR_SPLIT, Some("DontUse")).is_empty());
        assert!(hidden(STR_SPLIT, None).is_empty());
        assert!(hidden(STR_SPLIT, Some("not a mode")).is_empty());
    }

    #[test]
    fn english_aliases_are_hidden_too() {
        let source = r#"
Procedure Test()
    Parts = StrSplit("a,b", ",");
    Storage = DatabaseCopies;
EndProcedure
"#;
        let reported = hidden(source, Some("Version8_2_13"));
        assert_eq!(reported.len(), 2, "{reported:?}");
        assert!(reported.iter().any(|m| m.contains("'StrSplit'")), "{reported:?}");
        assert!(
            reported.iter().any(|m| m.contains("'DatabaseCopies'") && m.contains("8.3.14")),
            "{reported:?}"
        );
    }

    #[test]
    fn a_local_procedure_of_the_same_name_shadows_the_global() {
        let source = r#"
Функция СтрРазделить(Строка, Разделитель)
    Возврат Новый Массив;
КонецФункции

Процедура Тест()
    Части = СтрРазделить("a,b", ",");
КонецПроцедуры
"#;
        let reported = hidden(source, Some("Version8_2_13"));
        assert!(reported.is_empty(), "the call reaches the module's own function: {reported:?}");
    }

    #[test]
    fn a_global_common_module_export_of_the_same_name_shadows_the_global() {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(
            "Глобальный",
            "Функция СтрРазделить(А, Б) Экспорт Возврат Новый Массив; КонецФункции",
        );
        let reported: Vec<_> =
            run(STR_SPLIT, Some("Version8_2_13"), builder).into_iter().map(|d| d.message).collect();
        assert!(reported.is_empty(), "{reported:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn query_required_data_relevance_needs_mode_8_3_14() {
        let source = r#"
Процедура Тест()
    Запрос = Новый Запрос("ВЫБРАТЬ 1");
    Актуальность = Запрос.ТребуемаяАктуальностьДанных;
    Текст = Запрос.Текст;
КонецПроцедуры
"#;
        let reported = hidden(source, Some("Version8_3_13"));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].contains("'ТребуемаяАктуальностьДанных'"), "{reported:?}");
        assert!(reported[0].contains("8.3.14"), "{reported:?}");
        assert!(hidden(source, Some("Version8_3_14")).is_empty());
    }

    #[test]
    fn metadata_is_an_active_major_error() {
        let metadata =
            crate::handlers::get_metadata(DiagnosticCode::PlatformMemberHiddenByCompatibilityMode)
                .unwrap();
        assert_eq!(metadata.severity, DiagnosticSeverityLevel::Major);
        assert!(metadata.activated_by_default);
        assert!(!DiagnosticsConfig::default()
            .is_disabled(DiagnosticCode::PlatformMemberHiddenByCompatibilityMode));
    }
}
