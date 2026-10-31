//! An ORDINARY form whose existing sidecar was rejected: the dialog lives in the binary
//! `Ext/Form.bin`, and `Ext/Form/Form.json` is stale, unreadable or malformed. Its
//! button bindings are then unknown, so checks that depend on them (`UnusedLocalMethod`)
//! are dropped for the module - this diagnostic says why. A missing sidecar is a normal,
//! documented state and stays silent.

use super::missing_form_event_handler::first_line_range;
use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode};
use hir::ModuleMetadata;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_metadata(
    metadata: &ModuleMetadata,
    ctx: &crate::DiagnosticsContext,
) -> Vec<Diagnostic> {
    let code = DiagnosticCode::OrdinaryFormDialogUnread;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    if metadata.module_type != bsl_metadata::ModuleType::FormModule {
        return Vec::new();
    }

    let Some(form) = metadata.form.as_deref() else {
        return Vec::new();
    };

    if !form.is_ordinary() || form.ordinary_dialog_known() {
        return Vec::new();
    }

    let Some(rejection) = form.ordinary_dialog_rejection() else {
        return Vec::new();
    };

    let message = format!(
        "Диалог обычной формы не прочитан ({reason}): привязки обработчиков диалога неизвестны, \
         поэтому поиск неиспользуемых локальных методов в этом модуле отключён. \
         Обновите Ext/Form/Form.json тем инструментом, который его создаёт, - \
         описание файла в docs/configuration/ordinary-form-sidecar.md",
        reason = rejection.reason_ru()
    );

    vec![Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range: first_line_range(&ctx.file_text()),
        tags: ctx.tags(code),
        fixes: vec![],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::{Form, FormType, OrdinaryDialogRejection};
    use std::sync::Arc;

    const MODULE: &str = "Процедура КнопкаОКНажатие(Кнопка)\nКонецПроцедуры\n";

    fn check(form: Form) -> Vec<Diagnostic> {
        let metadata = ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: Some(Arc::new(form)),
        };
        crate::test_utils::check_metadata_diagnostic(metadata, MODULE, from_metadata)
    }

    fn ordinary(rejection: Option<OrdinaryDialogRejection>) -> Form {
        let mut form = Form::ordinary_without_dialog("Форма");
        form.ordinary_dialog_rejection = rejection;
        form
    }

    #[test]
    fn stale_sidecar_names_the_reason() {
        let diagnostics = check(ordinary(Some(OrdinaryDialogRejection::Stale)));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("старше Ext/Form.bin"), "{diagnostics:?}");
        assert!(!diagnostics[0].message.contains('\n'));
    }

    #[test]
    fn message_is_the_documented_single_line() {
        let diagnostics = check(ordinary(Some(OrdinaryDialogRejection::Malformed)));
        assert_eq!(
            diagnostics[0].message,
            "Диалог обычной формы не прочитан (сайдкар Ext/Form/Form.json не разбирается): \
             привязки обработчиков диалога неизвестны, поэтому поиск неиспользуемых локальных \
             методов в этом модуле отключён. Обновите Ext/Form/Form.json тем инструментом, \
             который его создаёт, - описание файла в docs/configuration/ordinary-form-sidecar.md"
        );
    }

    #[test]
    fn absent_sidecar_stays_silent() {
        assert_eq!(check(ordinary(None)), vec![]);
    }

    #[test]
    fn every_rejection_reports_once() {
        for rejection in [
            OrdinaryDialogRejection::Stale,
            OrdinaryDialogRejection::Unreadable,
            OrdinaryDialogRejection::Malformed,
            OrdinaryDialogRejection::InvalidSchema,
        ] {
            assert_eq!(check(ordinary(Some(rejection))).len(), 1, "{rejection:?}");
        }
    }

    #[test]
    fn known_dialog_is_silent() {
        let mut form = ordinary(None);
        form.ordinary_dialog_known = true;
        assert_eq!(check(form), vec![]);
    }

    #[test]
    fn managed_form_is_silent() {
        let form = Form::new("Форма".to_string(), FormType::Managed, uuid::Uuid::nil());
        assert_eq!(check(form), vec![]);
    }
}
