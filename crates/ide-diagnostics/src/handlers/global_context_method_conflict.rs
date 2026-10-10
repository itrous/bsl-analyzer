use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::GlobalContextMethodConflict;
    if ctx.is_disabled_with_metadata(code)
        || ctx.module_metadata().module_type != bsl_metadata::ModuleType::FormModule
    {
        return;
    }

    let Some(range) = ctx.method_name_range() else {
        return;
    };
    let name = ctx.text_of(range);
    let Some(function) = bsl_platform::PlatformDataInner::instance().get_global_function(&name)
    else {
        return;
    };

    let Some(decl) = ctx.decl() else {
        return;
    };

    // In a managed form, an explicit client method is compiled for thin/web
    // clients. A thick-client-only global (such as ПолучитьОбщуюФорму) is not
    // part of that method's context, and the platform accepts the declaration.
    // Keep the server-side check: native 8.3.27 reports the same name as a
    // collision for &НаСервере, while accepting &НаКлиенте in a managed form.
    if decl.directives.len() == 1
        && decl.directives[0] == hir::AnnotationKind::AtClient
        && ctx.module_metadata().form.as_ref().is_some_and(|form| {
            form.form_type() == bsl_metadata::FormType::Managed
                && function.context.is_some_and(|availability| {
                    !availability.thin_client
                        && !availability.web_client
                        && !availability.mobile_client
                })
        })
    {
        return;
    }

    // The 8312 collision is the precise account of this declaration: while that
    // check is enabled it already names the problem, and this report would be a
    // duplicate. Disabling it must not silence this one too — the two toggles
    // are independent (github#170), so ask the config, not the lowering alone.
    if !ctx.is_disabled_with_metadata(DiagnosticCode::GlobalContextMethodCollision8312)
        && ctx.lower().diagnostics.iter().any(|diag| {
            matches!(diag, hir::BodyDiagnostic::GlobalContextMethodCollision8312 { .. })
        })
    {
        return;
    }

    acc.push(Diagnostic {
        code,
        message: format!("Имя метода формы \"{name}\" конфликтует с глобальной функцией платформы"),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    });
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_metadata_diagnostic_with_config, make_non_common_module_metadata,
    };
    use crate::DiagnosticCode;
    use std::sync::Arc;

    fn conflicts(
        source: &str,
        form_type: Option<bsl_metadata::FormType>,
    ) -> Vec<crate::Diagnostic> {
        conflicts_with(source, form_type, &[])
    }

    /// `disabled` выключает коды в конфиге — так же, как их выключает
    /// пользователь, чтобы проверить независимость тумблеров (github#170).
    fn conflicts_with(
        source: &str,
        form_type: Option<bsl_metadata::FormType>,
        disabled: &[DiagnosticCode],
    ) -> Vec<crate::Diagnostic> {
        let mut metadata = make_non_common_module_metadata(bsl_metadata::ModuleType::FormModule);
        metadata.form = form_type.map(|form_type| {
            Arc::new(bsl_metadata::Form::new(
                "ТестоваяФорма".to_string(),
                form_type,
                uuid::Uuid::nil(),
            ))
        });
        let mut config = crate::DiagnosticsConfig::all_enabled();
        config.disabled.extend_from_slice(disabled);
        check_metadata_diagnostic_with_config(metadata, source, config, |_, ctx| {
            crate::diagnostics(ctx)
                .into_iter()
                .filter(|diag| diag.code == DiagnosticCode::GlobalContextMethodConflict)
                .collect()
        })
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn reports_server_context_collision_and_allows_client_only_global_in_managed_form() {
        let server =
            "&НаСервере\nФункция ПолучитьОбщуюФорму()\n    Возврат Неопределено;\nКонецФункции";
        let client =
            "&НаКлиенте\nФункция ПолучитьОбщуюФорму()\n    Возврат Неопределено;\nКонецФункции";

        assert_eq!(conflicts(server, Some(bsl_metadata::FormType::Managed)).len(), 1);
        assert!(conflicts(client, Some(bsl_metadata::FormType::Managed)).is_empty());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn preserves_unannotated_ba029_collision() {
        let source =
            "Функция ПредставлениеПериода(Начало, Конец)\n    Возврат Начало;\nКонецФункции";
        assert_eq!(conflicts(source, None).len(), 1);
    }

    /// Выключение `GlobalContextMethodCollision8312` не глушит этот конфликт:
    /// у проверок независимые тумблеры. Пока 8312 включена, её точный отчёт
    /// выигрывает и дубля нет; выключили — конфликт обязан ответить (github#170).
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn disabling_the_8312_collision_does_not_silence_the_conflict() {
        // `ПроверитьБит` лежит и в списке коллизий 8312, и в глобальных
        // функциях платформы — пересечение, на котором связка и была видна.
        let source = "Функция ПроверитьБит()\n    Возврат Ложь;\nКонецФункции";

        let both = conflicts(source, Some(bsl_metadata::FormType::Managed));
        assert!(both.is_empty(), "with 8312 enabled its precise report wins: {both:#?}");

        let conflict_only = conflicts_with(
            source,
            Some(bsl_metadata::FormType::Managed),
            &[DiagnosticCode::GlobalContextMethodCollision8312],
        );
        assert_eq!(
            conflict_only.len(),
            1,
            "disabling 8312 must not silence the conflict: {conflict_only:#?}"
        );
    }
}
