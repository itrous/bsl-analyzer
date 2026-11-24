use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::DuplicateMethodDeclaration;
    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }
    ctx.method_declaration_conflicts()
        .methods
        .iter()
        .map(|method| Diagnostic {
            code,
            message: format!("Метод «{}» объявлен несколько раз в одном модуле", method.name),
            severity: ctx.severity(code),
            range: method.range,
            tags: ctx.tags(code),
            fixes: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_ast_diagnostic, check_hir_diagnostic_with_fixtures};

    #[test]
    fn arity_and_method_kind_do_not_create_overloads() {
        let source = include_str!("fixtures/method_declaration_conflicts/duplicate_methods.bsl");
        let diagnostics = check_ast_diagnostic(source, check);
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        let names: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
            })
            .collect();
        assert_eq!(names, ["Применить", "ПРИМЕНИТЬ", "Shared", "shared"]);
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == crate::Severity::Blocker));
    }

    #[test]
    fn mutually_exclusive_guards_and_unknown_conditions_are_not_conflicts() {
        let source =
            include_str!("fixtures/method_declaration_conflicts/exclusive_declarations.bsl");
        assert!(check_ast_diagnostic(source, check).is_empty());
    }

    #[test]
    fn known_overlapping_guards_still_conflict() {
        let source = "#Если Сервер Тогда\nПроцедура Test()\nКонецПроцедуры\n#КонецЕсли\n#If Server Or Client Then\nProcedure TEST(Value)\nEndProcedure\n#EndIf";
        assert_eq!(check_ast_diagnostic(source, check).len(), 2);
    }

    #[test]
    fn module_variables_are_a_separate_namespace() {
        let source = "Перем Shared;\nПроцедура Shared()\nКонецПроцедуры";
        assert!(check_ast_diagnostic(source, check).is_empty());
    }

    #[test]
    fn declarations_in_different_modules_do_not_conflict() {
        let source = "Procedure Shared()\nEndProcedure";
        let mut fixture = test_fixture::CfeFixtureBuilder::new("");
        fixture.add_base_module("Other", source);
        let diagnostics = crate::test_utils::check_with_cfe(source, fixture.build());
        assert!(!diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration));
    }

    #[test]
    fn weaving_and_changed_body_are_not_duplicate_physical_declarations() {
        let base = "Функция Compute() Экспорт\n    Возврат 1;\nКонецФункции";
        for (source, weaving) in [
            ("&Вместо(\"Compute\")\nФункция Compute()\n    Возврат ПродолжитьВызов();\nКонецФункции", true),
            ("&ИзменениеИКонтроль(\"Compute\")\nФункция Compute()\n#Удаление\n    Возврат 1;\n#КонецУдаления\n#Вставка\n    Возврат 2;\n#КонецВставки\nКонецФункции", false),
        ] {
            let mut fixture = test_fixture::CfeFixtureBuilder::new("");
            fixture.add_base_module("Caller", base).add_extension("Own", "");
            let diagnostics = crate::test_utils::check_cfe_at_with_db(
                "CommonModules/Caller/Ext/Module.bsl", source, fixture.build(), &[],
                crate::DiagnosticsConfig::all_enabled(), |_| {},
                |db, ctx| {
                    if weaving {
                        assert!(ide_db::weaving_target(db, ctx.file_id).is_some());
                    } else {
                        assert!(ide_db::effective_target(db, ctx.file_id).is_some());
                    }
                    crate::file_diagnostics(db, ctx.file_id, ctx.config)
                },
            );
            assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration), "{diagnostics:?}");
        }
    }

    #[test]
    fn full_external_result_contains_a_separate_blocker() {
        for path in [
            "ExternalDataProcessors/Example/Ext/ObjectModule.bsl",
            "ExternalReports/Example/Ext/ObjectModule.bsl",
        ] {
            let source = format!(
                "//- /{path}\n{}",
                include_str!("fixtures/method_declaration_conflicts/duplicate_methods.bsl")
            );
            let diagnostics = check_hir_diagnostic_with_fixtures(&source);
            assert_eq!(
                diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.code
                        == DiagnosticCode::DuplicateMethodDeclaration
                        && diagnostic.severity == crate::Severity::Blocker)
                    .count(),
                4,
                "{diagnostics:?}"
            );
            assert!(!diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == DiagnosticCode::ParseError));
        }
    }

    #[test]
    fn extension_conflicts_keep_physical_ranges_through_weaving_and_cav() {
        let base = "Function Compute() Export\n    Return 1;\nEndFunction";
        for prefix in [
            "&Instead(\"Compute\")\nFunction Compute()\n    Return ProceedWithCall();\nEndFunction\n",
            "&ИзменениеИКонтроль(\"Compute\")\nFunction Compute()\n#Удаление\n    Return 1;\n#КонецУдаления\n#Вставка\n    Return 2;\n#КонецВставки\nEndFunction\n",
        ] {
            let source = format!("{prefix}\nProcedure Shared(Value)\nEndProcedure\n\nProcedure SHARED()\nEndProcedure\n");
            let mut fixture = test_fixture::CfeFixtureBuilder::new("");
            fixture.add_base_module("Caller", base).add_extension("Own", "");
            let diagnostics = crate::test_utils::check_cfe_at_with_db(
                "CommonModules/Caller/Ext/Module.bsl", &source, fixture.build(), &[],
                crate::DiagnosticsConfig::default(), |_| {},
                |db, ctx| {
                    assert!(ide_db::weaving_target(db, ctx.file_id).is_some() || ide_db::effective_target(db, ctx.file_id).is_some());
                    crate::file_diagnostics(db, ctx.file_id, ctx.config)
                },
            );
            let conflicts: Vec<_> = diagnostics.iter().filter(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration).collect();
            assert_eq!(conflicts.len(), 2, "{diagnostics:?}");
            let names: Vec<_> = conflicts.iter().map(|diagnostic| &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]).collect();
            assert_eq!(names, ["Shared", "SHARED"]);
        }
    }
}
