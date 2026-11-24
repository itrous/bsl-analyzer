use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use stdx::case::CaseExt;
use syntax::ast::{AstNode, FunctionDef, PreRegionDir, ProcedureDef};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::CommonModule],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Brainoverload, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CommonModuleMissingAPI;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();
    check_for_module_type(ctx, metadata.module_type)
}

fn check_for_module_type(
    ctx: &DiagnosticsContext,
    module_type: bsl_metadata::ModuleType,
) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CommonModuleMissingAPI;

    if module_type != bsl_metadata::ModuleType::CommonModule {
        return Vec::new();
    }

    let parse = ctx.parse();
    let root = parse.syntax_node();

    let has_methods = has_any_methods(&root);
    if !has_methods {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();
    let methods = item_tree.methods().collect::<Vec<_>>();
    if !methods.is_empty()
        && methods.iter().all(|method| {
            method.annotations().iter().any(|annotation| annotation.kind.is_interception())
        })
    {
        return Vec::new();
    }

    let has_export = has_export_methods(&root);
    let has_api_region = has_api_regions(&root);

    if !has_export || !has_api_region {
        vec![Diagnostic {
            code,
            message: "Общий модуль должен содержать экспортные методы и области API".to_string(),
            severity: ctx.severity(code),
            range: syntax::MODULE_RANGE,
            tags: ctx.tags(code),
            fixes: vec![],
        }]
    } else {
        Vec::new()
    }
}

fn has_any_methods(root: &syntax::SyntaxNode) -> bool {
    root.descendants()
        .any(|node| ProcedureDef::cast(node.clone()).is_some() || FunctionDef::cast(node).is_some())
}

fn has_export_methods(root: &syntax::SyntaxNode) -> bool {
    root.descendants().any(|node| {
        if let Some(proc) = ProcedureDef::cast(node.clone()) {
            proc.export_keyword().is_some()
        } else if let Some(func) = FunctionDef::cast(node) {
            func.export_keyword().is_some()
        } else {
            false
        }
    })
}

fn has_api_regions(root: &syntax::SyntaxNode) -> bool {
    const API_REGIONS: &[&str] =
        &["программныйинтерфейс", "public", "служебныйпрограммныйинтерфейс", "internal"];

    root.descendants()
        .filter_map(PreRegionDir::cast)
        .filter_map(|r| r.name())
        .any(|name| API_REGIONS.contains(&name.fold_lower().as_str()))
}

#[cfg(test)]
mod tests {
    use super::check_for_module_type;
    use crate::test_utils::check_ast_diagnostic;
    use bsl_metadata::ModuleType;

    fn check_as(
        module_type: ModuleType,
    ) -> impl Fn(&crate::DiagnosticsContext) -> Vec<crate::Diagnostic> {
        move |ctx| check_for_module_type(ctx, module_type)
    }

    #[test]
    fn test_with_export_and_api() {
        let code = r#"
#Область ПрограммныйИнтерфейс
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(diagnostics.len(), 0);
    }

    #[test]
    fn test_without_export() {
        let code = r#"
#Область ПрограммныйИнтерфейс
Процедура Тест()
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn test_without_api_region() {
        let code = r#"
#Область Служебный
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn interception_only_module_does_not_need_an_exported_api() {
        let code = r#"&После("ОбработкаЗаполнения")
Процедура Перехват()
КонецПроцедуры
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn change_and_validate_only_module_does_not_need_an_exported_api() {
        let code = r#"&ИзменениеИКонтроль("ОбработкаЗаполнения")
Процедура Перехват()
КонецПроцедуры
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn non_interception_method_still_needs_an_exported_api() {
        let code = r#"Процедура СлужебныйМетод()
КонецПроцедуры
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn test_no_methods() {
        let code = r#"
#Область ПрограммныйИнтерфейс
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(diagnostics.len(), 0);
    }

    #[test]
    fn test_manager_module_without_api_region() {
        let code = r#"
#Область Служебный
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::ManagerModule));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn manager_interception_and_helper_do_not_need_an_artificial_export() {
        let code = include_str!("fixtures/common_module_missing_api/manager_interception.bsl");
        for module_type in [
            ModuleType::ManagerModule,
            ModuleType::ObjectModule,
            ModuleType::FormModule,
            ModuleType::Unknown,
        ] {
            let diagnostics = check_ast_diagnostic(code, check_as(module_type));
            assert!(diagnostics.is_empty(), "{module_type:?}: {diagnostics:?}");
        }
        let common = check_ast_diagnostic(code, check_as(ModuleType::CommonModule));
        assert_eq!(common.len(), 1, "{common:?}");
    }

    #[test]
    fn test_form_module_is_ignored() {
        let code = r#"
#Область Служебный
Процедура Тест()
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::FormModule));
        assert_eq!(diagnostics.len(), 0);
    }

    #[test]
    fn test_object_module_is_ignored() {
        let code = r#"
#Область Служебный
Процедура Тест()
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::ObjectModule));
        assert_eq!(diagnostics.len(), 0);
    }

    #[test]
    fn test_unknown_module_is_ignored() {
        let code = r#"
#Область Служебный
Процедура Тест()
КонецПроцедуры
#КонецОбласти
"#;
        let diagnostics = check_ast_diagnostic(code, check_as(ModuleType::Unknown));
        assert_eq!(diagnostics.len(), 0);
    }
}
