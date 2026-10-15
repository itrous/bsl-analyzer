use bsl_metadata::{FormType, ModuleType};
use hir::AnnotationKind;
use ide_db::TextRange;
use syntax::ast::{AstNode, FunctionDef, ProcedureDef};
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext, Fix, TextEdit};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Unpredictable, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::ServerSideExportFormMethod;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != ModuleType::FormModule {
        return Vec::new();
    }

    match &metadata.form {
        Some(form) if form.form_type == FormType::Managed => {}
        _ => return Vec::new(),
    }

    let item_tree = ctx.item_tree();
    let root = ctx.parse().syntax_node();
    let mut diagnostics = Vec::new();

    for (_, proc) in item_tree.procedures() {
        if proc.is_export && !has_client_annotation(&proc.annotations) {
            let fixes = remove_export_fix(&root, proc.source_range);
            diagnostics.push(make_diagnostic(proc.name_range, fixes, code, ctx));
        }
    }

    for (_, func) in item_tree.functions() {
        if func.is_export && !has_client_annotation(&func.annotations) {
            let fixes = remove_export_fix(&root, func.source_range);
            diagnostics.push(make_diagnostic(func.name_range, fixes, code, ctx));
        }
    }

    diagnostics
}

/// Drops `Экспорт` (and the blank run before it on the same line) from the method
/// header. Manual: a call through `ЭтаФорма.`/`ЭтотОбъект.` still compiles after the
/// edit but fails at run time, because the platform resolves a member call only
/// against exported methods.
fn remove_export_fix(root: &SyntaxNode, source_range: TextRange) -> Vec<Fix> {
    let Some(export) = export_keyword(root, source_range) else {
        return Vec::new();
    };
    let mut start = export.text_range().start();
    if let Some(prev) = export.prev_token() {
        if prev.kind() == SyntaxKind::WHITESPACE && !prev.text().contains(['\n', '\r']) {
            start = prev.text_range().start();
        }
    }
    vec![Fix::manual(
        "Убрать \"Экспорт\"",
        vec![TextEdit {
            range: TextRange::new(start, export.text_range().end()),
            new_text: String::new(),
        }],
    )]
}

fn export_keyword(root: &SyntaxNode, source_range: TextRange) -> Option<SyntaxToken> {
    root.descendants().filter(|node| node.text_range() == source_range).find_map(|node| {
        ProcedureDef::cast(node.clone())
            .and_then(|proc| proc.export_keyword())
            .or_else(|| FunctionDef::cast(node).and_then(|func| func.export_keyword()))
    })
}

/// A method reachable on the client is not a forbidden server-only export. Besides
/// `&НаКлиенте`, the context-free `&НаКлиентеНаСервереБезКонтекста` (and the
/// `&НаКлиентеНаСервере` pair) compile for the client too, so an export of that kind
/// is a legitimate client-callable form method, not a server export.
fn has_client_annotation(annotations: &[hir::Annotation]) -> bool {
    annotations.iter().any(|a| {
        matches!(
            a.kind,
            AnnotationKind::AtClient
                | AnnotationKind::AtClientAtServer
                | AnnotationKind::AtClientAtServerNoContext
        )
    })
}

fn make_diagnostic(
    range: TextRange,
    fixes: Vec<Fix>,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Diagnostic {
    Diagnostic {
        code,
        message: "Запрещено создавать серверные экспортные методы в форме".to_string(),
        range,
        severity: ctx.severity(code),
        tags: ctx.tags(code),
        fixes,
    }
}

#[cfg(test)]
mod tests {
    use crate::DiagnosticCode;
    use std::sync::Arc;

    fn managed_form_metadata() -> hir::ModuleMetadata {
        let form = bsl_metadata::xml_parser::parse_form_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"></Form>"#,
        )
        .unwrap();

        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: Some(Arc::new(form)),
            http_service: None,
            web_service: None,
            integration_service: None,
        }
    }

    fn flagged(code_text: &str) -> Vec<crate::Diagnostic> {
        let diagnostics = crate::test_utils::check_metadata_diagnostic(
            managed_form_metadata(),
            code_text,
            |_, ctx| super::check(ctx),
        );
        diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::ServerSideExportFormMethod)
            .collect()
    }

    #[test]
    fn client_server_no_context_export_not_flagged() {
        let code = r#"
&НаКлиентеНаСервереБезКонтекста
Процедура ОбщийМетод() Экспорт
КонецПроцедуры
"#;
        assert!(
            flagged(code).is_empty(),
            "&НаКлиентеНаСервереБезКонтекста export must not be flagged"
        );
    }

    #[test]
    fn client_export_not_flagged() {
        let code = r#"
&НаКлиенте
Процедура КлиентскийМетод() Экспорт
КонецПроцедуры
"#;
        assert!(flagged(code).is_empty(), "&НаКлиенте export must not be flagged");
    }

    #[test]
    fn server_export_still_flagged() {
        let code = r#"
&НаСервере
Процедура СерверныйМетод() Экспорт
КонецПроцедуры
"#;
        assert_eq!(flagged(code).len(), 1, "&НаСервере export must stay flagged");
    }
}
