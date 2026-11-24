use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::AnnotationKind;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::DuplicateInterception;
    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }
    ctx.method_declaration_conflicts()
        .interceptions
        .iter()
        .map(|interception| Diagnostic {
            code,
            message: format!(
                "Перехват «{}(\"{}\")» объявлен несколько раз в одном модуле",
                annotation(interception.kind),
                interception.target
            ),
            severity: ctx.severity(code),
            range: interception.method.range,
            tags: ctx.tags(code),
            fixes: Vec::new(),
        })
        .collect()
}

fn annotation(kind: AnnotationKind) -> &'static str {
    match kind {
        AnnotationKind::Before => "&Перед",
        AnnotationKind::After => "&После",
        AnnotationKind::Instead => "&Вместо",
        AnnotationKind::ChangeAndValidate => "&ИзменениеИКонтроль",
        _ => unreachable!("only interception annotations produce conflicts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::check_ast_diagnostic;

    #[test]
    fn equal_target_and_kind_report_both_declarations() {
        let source =
            include_str!("fixtures/method_declaration_conflicts/duplicate_interceptions.bsl");
        let diagnostics = check_ast_diagnostic(source, check);
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        let names: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
            })
            .collect();
        assert_eq!(names, ["Первый", "Second", "BeforeOne", "BeforeTwo"]);
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == crate::Severity::Warning));
    }

    #[test]
    fn different_targets_kinds_and_exclusive_guards_are_not_duplicates() {
        let source =
            include_str!("fixtures/method_declaration_conflicts/exclusive_interceptions.bsl");
        assert!(check_ast_diagnostic(source, check).is_empty());
    }

    #[test]
    fn malformed_annotation_does_not_invent_a_target() {
        let source = "&После()\nПроцедура First()\nКонецПроцедуры\n&After(Unknown)\nProcedure Second()\nEndProcedure";
        assert!(check_ast_diagnostic(source, check).is_empty());
    }

    #[test]
    fn same_interception_in_another_module_does_not_conflict() {
        let source = "&После(\"ПередЗаписью\")\nПроцедура Intercept()\nКонецПроцедуры";
        let mut fixture = test_fixture::CfeFixtureBuilder::new("");
        fixture.add_base_module("Other", source);
        let diagnostics = crate::test_utils::check_with_cfe(source, fixture.build());
        assert!(!diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateInterception));
    }

    #[test]
    fn all_interception_kinds_share_the_same_duplicate_rule() {
        for kind in ["Перед", "После", "Вместо", "ИзменениеИКонтроль"]
        {
            let source = format!("&{kind}(\"Target\")\nПроцедура First()\nКонецПроцедуры\n&{kind}(\"TARGET\")\nПроцедура Second()\nКонецПроцедуры");
            assert_eq!(check_ast_diagnostic(&source, check).len(), 2, "{kind}");
        }
    }

    #[test]
    fn default_public_extension_analysis_reports_both_physical_interceptors() {
        let source="&After(\"Target\")\nProcedure First()\nEndProcedure\n&После(\"TARGET\")\nProcedure Second()\nEndProcedure";
        let mut fixture = test_fixture::CfeFixtureBuilder::new("");
        fixture
            .add_base_module("Caller", "Procedure Target() Export\nEndProcedure")
            .add_extension("Own", "");
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            "CommonModules/Caller/Ext/Module.bsl",
            source,
            fixture.build(),
            &[],
            crate::DiagnosticsConfig::default(),
            |_| {},
            |db, ctx| {
                assert!(ide_db::weaving_target(db, ctx.file_id).is_some());
                crate::file_diagnostics(db, ctx.file_id, ctx.config)
            },
        );
        let conflicts: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateInterception)
            .collect();
        assert_eq!(conflicts.len(), 2, "{diagnostics:?}");
        let names: Vec<_> = conflicts
            .iter()
            .map(|diagnostic| {
                &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
            })
            .collect();
        assert_eq!(names, ["First", "Second"]);
    }
}
