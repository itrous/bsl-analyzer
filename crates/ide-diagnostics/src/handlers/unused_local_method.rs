use crate::define_metadata;
use crate::metadata::*;
use crate::utils::platform_event_handlers as peh;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use ide_db::TextRange;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[bsl_metadata::ModuleType::CommonModule, bsl_metadata::ModuleType::ObjectModule, bsl_metadata::ModuleType::HTTPServiceModule, bsl_metadata::ModuleType::WebServiceModule],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Suspicious, MetadataTag::Unused],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const DEFAULT_ATTACHABLE_PREFIXES: &str = "подключаемый_,attachable_";

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::UnusedLocalMethod;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let attachable_prefixes_str = ctx
        .config
        .get_string(code, "attachableMethodPrefixes")
        .unwrap_or(DEFAULT_ATTACHABLE_PREFIXES);

    let attachable_prefixes: Vec<String> = attachable_prefixes_str
        .split(',')
        .map(|s| s.trim().fold_lower())
        .filter(|s| !s.is_empty())
        .collect();

    let check_object_module = ctx.config.get_bool(code, "checkObjectModule").unwrap_or(false);

    let metadata = ctx.module_metadata();

    if !check_object_module && metadata.module_type == bsl_metadata::ModuleType::ObjectModule {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();

    let module_id = hir::ModuleId::new(ctx.file_id);
    let summary = ctx.call_summary(module_id);

    let mut called_methods: FxHashSet<String> = FxHashSet::default();

    for edge in &summary.call_edges {
        match &edge.target {
            hir::call_graph::CallTarget::Local { callee_local_id } => {
                if let Some(method) =
                    summary.methods.iter().find(|m| m.local_id == *callee_local_id)
                {
                    called_methods.insert(method.name.as_str().fold_lower());
                }
            }
            // The method segment of a member call counts as a use regardless of
            // the receiver. The BSP "Свойства" subsystem re-arms a form's local
            // callback by name through a common-module wrapper —
            // `УправлениеСвойствамиКлиент.ОбновитьЗависимостиДополнительныхРеквизитов(Форма)`
            // ultimately runs `Форма.ПодключитьОбработчикОжидания("ОбновитьЗависимости…")`
            // — so the local procedure sharing that name is reachable even
            // though the module contains no bare call to it. Matching by name is
            // the only signal available without cross-module flow.
            hir::call_graph::CallTarget::QualifiedModule { method_name, .. }
            | hir::call_graph::CallTarget::ThisObjectMethod { method_name } => {
                called_methods.insert(method_name.as_str().fold_lower());
            }
            hir::call_graph::CallTarget::ManagerAccess {
                method_name: Some(method_name), ..
            } => {
                called_methods.insert(method_name.as_str().fold_lower());
            }
            hir::call_graph::CallTarget::ManagerAccess { method_name: None, .. }
            | hir::call_graph::CallTarget::RegisterMovement { .. }
            | hir::call_graph::CallTarget::Unresolved => {}
        }
    }

    // The platform invokes these by the registered string name:
    // ПодключитьОбработчикОжидания and Новый ОписаниеОповещения (both handler slots).
    for reg in &summary.notify_regs {
        called_methods.insert(reg.callback_name.as_str().fold_lower());
    }
    for reg in &summary.idle_handler_regs {
        called_methods.insert(reg.handler_name.as_str().fold_lower());
    }
    // `Элементы.X.УстановитьДействие("Событие", "Обработчик")` binds a form element's
    // event to a module procedure by name at runtime — the procedure has no other call
    // site, so without this it reads as unused. Scoped to form modules: that is the only
    // kind where `УстановитьДействие` targets a local handler, so consuming it elsewhere
    // could mask a genuinely unused local that shares a name with some object's action.
    if metadata.module_type == bsl_metadata::ModuleType::FormModule {
        for reg in &summary.set_action_regs {
            called_methods.insert(reg.handler_name.as_str().fold_lower());
        }
        // Handler names also reach `УстановитьДействие`/`Действие` indirectly — through a
        // command created in code or a parameter structure handed to a helper in another
        // module — where the only same-module trace is the name literal itself. Any
        // identifier-shaped literal naming a local method counts as a use; scoped to form
        // modules, where dynamic binding is idiomatic and worth the missed-dead-code risk.
        for local_id in &summary.name_literal_refs {
            if let Some(method) = summary.methods.iter().find(|m| m.local_id == *local_id) {
                called_methods.insert(method.name.as_str().fold_lower());
            }
        }
        // The binding may sit in ANOTHER module: a helper handed the form runs
        // `Форма.ПодключитьОбработчикОжидания("Имя", …)`, and the only trace of the name is
        // that literal, in that helper.
        let bindings = ctx.workspace_foreign_handler_bindings();
        if !bindings.is_empty() {
            for method in &summary.methods {
                if bindings.is_bound(method.name.as_str()) {
                    called_methods.insert(method.name.as_str().fold_lower());
                }
            }
        }
    }

    if let Some(ref form) = metadata.form {
        for handler in form.event_handlers() {
            called_methods.insert(handler.handler_name.fold_lower());
        }
        for handler in form.command_handlers() {
            called_methods.insert(handler.fold_lower());
        }
    }

    if let Some(ref http_service) = metadata.http_service {
        for (_template, method) in http_service.all_methods() {
            if !method.is_handler_empty() {
                called_methods.insert(method.handler().fold_lower());
            }
        }
    }

    if let Some(ref web_service) = metadata.web_service {
        for operation in web_service.operations() {
            if !operation.is_handler_empty() {
                called_methods.insert(operation.procedure_name().fold_lower());
            }
        }
    }

    if let Some(ref integration_service) = metadata.integration_service {
        for handler in integration_service.receive_handlers() {
            called_methods.insert(handler.fold_lower());
        }
    }

    let mut diagnostics = Vec::new();

    let owner = peh::ModuleOwner {
        module_type: metadata.module_type,
        mdo_type: metadata.mdo.as_ref().map(|mdo| mdo.mdo_type),
    };

    for (_, proc) in item_tree.procedures() {
        if let Some(diag) = check_method_unused(
            &proc.name,
            proc.name_range,
            proc.is_export,
            &proc.annotations,
            &attachable_prefixes,
            &called_methods,
            owner,
            code,
            ctx,
        ) {
            diagnostics.push(diag);
        }
    }

    for (_, func) in item_tree.functions() {
        if let Some(diag) = check_method_unused(
            &func.name,
            func.name_range,
            func.is_export,
            &func.annotations,
            &attachable_prefixes,
            &called_methods,
            owner,
            code,
            ctx,
        ) {
            diagnostics.push(diag);
        }
    }

    diagnostics
}

#[allow(clippy::too_many_arguments)]
fn check_method_unused(
    name: &hir::Name,
    name_range: TextRange,
    is_export: bool,
    annotations: &[hir::Annotation],
    attachable_prefixes: &[String],
    called_methods: &FxHashSet<String>,
    owner: peh::ModuleOwner,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Option<Diagnostic> {
    if is_export {
        return None;
    }

    if has_extension_annotation(annotations) {
        return None;
    }

    let name_lower = name.as_str().fold_lower();

    if is_attachable_method(&name_lower, attachable_prefixes) {
        return None;
    }

    if is_handler_method(&name_lower, owner) {
        return None;
    }

    if called_methods.contains(&name_lower) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!("Неиспользуемый локальный метод \"{}\"", name.as_str()),
        severity: ctx.severity(code),
        range: name_range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn has_extension_annotation(annotations: &[hir::Annotation]) -> bool {
    annotations.iter().any(|ann| ann.kind.is_interception())
}

fn is_attachable_method(name_lower: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| name_lower.starts_with(prefix))
}

fn is_handler_method(name_lower: &str, owner: peh::ModuleOwner) -> bool {
    use bsl_metadata::ModuleType;

    if peh::is_platform_event_handler(name_lower) {
        return true;
    }

    if peh::is_report_object_module_event_handler(name_lower, owner) {
        return true;
    }

    match owner.module_type {
        ModuleType::ManagedApplicationModule => {
            peh::is_managed_application_module_event_handler(name_lower)
        }
        // The generic `ApplicationModule` is the pre-split single application module;
        // treat it as the ordinary application module (lifecycle + external event),
        // never the managed-only UI handlers.
        ModuleType::OrdinaryApplicationModule | ModuleType::ApplicationModule => {
            peh::is_ordinary_application_module_event_handler(name_lower)
        }
        ModuleType::ExternalConnectionModule => {
            peh::is_external_connection_module_event_handler(name_lower)
        }
        ModuleType::SessionModule => peh::is_session_module_event_handler(name_lower),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_diagnostics_snapshot_for, check_hir_diagnostic, check_hir_diagnostic_with_config,
    };
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;

    #[test]
    fn test_call_inside_dot_function_condition_is_counted() {
        let code = r#"
Процедура ВывестиКолонкуФункция()
КонецПроцедуры

Процедура Главная(ПараметрыПечати)
    Если ПараметрыПечати.Функция Тогда
        ВывестиКолонкуФункция();
    КонецЕсли;
КонецПроцедуры

Главная(Неопределено);
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_detects_unused_local_methods() {
        let code = r#"
Процедура НеИспользуется() // Тут
КонецПроцедуры



&Вместо("ИспользуетсяВРасширенииВместо")
Функция Расш_ИспользуетсяВРасширенииВместо()
КонецФункции

&Перед("ИспользуетсяВРасширенииПеред")
Функция Расш_ИспользуетсяВРасширенииПеред()
КонецФункции

&После("ИспользуетсяВРасширенииПосле")
Функция Расш_ИспользуетсяВРасширенииПосле()
КонецФункции

&ИзменениеИКонтроль("ИспользуетсяВРасширенииИзменениеИКонтроль")
Функция Расш_ИспользуетсяВРасширенииИзменениеИКонтроль()
КонецФункции

Процедура НеИспользуетсяЭкспорт() Экспорт
КонецПроцедуры

Процедура ИспользуетсяВОсновномТеле()
КонецПроцедуры

Процедура ИспользуетсяВМетоде()
КонецПроцедуры

Процедура ИспользуетсяВУсловии()
КонецПроцедуры

Функция ИспользуетсяВПрисвоении()
КонецФункции

Функция ИспользуетсяВПарметре()
КонецФункции

Функция ИспользуетсяВПарметреПриПрисвоении()
КонецФункции

Функция СВызовами(Параметры)

	ИспользуетсяВМетоде();
	B = ИспользуетсяВПрисвоении();

КонецФункции

Функция СВызовами2()

	Если ИспользуетсяВУсловии() Тогда
	КонецЕсли;

	ГлобальныйМетод(ИспользуетсяВПарметре());
	А = СВызовами(ИспользуетсяВПарметреПриПрисвоении());

КонецФункции

Процедура Подключаемый_КакойтоОбработчик()
КонецПроцедуры

Процедура Attachable__КакойтоОбработчик()
КонецПроцедуры

Процедура ПриСозданииОбъекта(Параметр1)

КонецПроцедуры

Процедура ПодключаемаяМоя_НужнаяПроцедура()
КонецПроцедуры

ИспользуетсяВОсновномТеле();
СВызовами();
СВызовами2();
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
            UnusedLocalMethod @ 2:11..2:25
              message: Неиспользуемый локальный метод "НеИспользуется"
              severity: Warning
            UnusedLocalMethod @ 71:11..71:42
              message: Неиспользуемый локальный метод "ПодключаемаяМоя_НужнаяПроцедура"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_configure_prefixes() {
        let code = r#"
Процедура НеИспользуется() // Тут
КонецПроцедуры



&Вместо("ИспользуетсяВРасширенииВместо")
Функция Расш_ИспользуетсяВРасширенииВместо()
КонецФункции

&Перед("ИспользуетсяВРасширенииПеред")
Функция Расш_ИспользуетсяВРасширенииПеред()
КонецФункции

&После("ИспользуетсяВРасширенииПосле")
Функция Расш_ИспользуетсяВРасширенииПосле()
КонецФункции

&ИзменениеИКонтроль("ИспользуетсяВРасширенииИзменениеИКонтроль")
Функция Расш_ИспользуетсяВРасширенииИзменениеИКонтроль()
КонецФункции

Процедура НеИспользуетсяЭкспорт() Экспорт
КонецПроцедуры

Процедура ИспользуетсяВОсновномТеле()
КонецПроцедуры

Процедура ИспользуетсяВМетоде()
КонецПроцедуры

Процедура ИспользуетсяВУсловии()
КонецПроцедуры

Функция ИспользуетсяВПрисвоении()
КонецФункции

Функция ИспользуетсяВПарметре()
КонецФункции

Функция ИспользуетсяВПарметреПриПрисвоении()
КонецФункции

Функция СВызовами(Параметры)

	ИспользуетсяВМетоде();
	B = ИспользуетсяВПрисвоении();

КонецФункции

Функция СВызовами2()

	Если ИспользуетсяВУсловии() Тогда
	КонецЕсли;

	ГлобальныйМетод(ИспользуетсяВПарметре());
	А = СВызовами(ИспользуетсяВПарметреПриПрисвоении());

КонецФункции

Процедура Подключаемый_КакойтоОбработчик()
КонецПроцедуры

Процедура Attachable__КакойтоОбработчик()
КонецПроцедуры

Процедура ПриСозданииОбъекта(Параметр1)

КонецПроцедуры

Процедура ПодключаемаяМоя_НужнаяПроцедура()
КонецПроцедуры

ИспользуетсяВОсновномТеле();
СВызовами();
СВызовами2();
"#;

        let mut config = DiagnosticsConfig::default();
        let mut params = serde_json::Map::new();
        params.insert(
            "attachableMethodPrefixes".to_string(),
            serde_json::Value::String("ПодключаемаяМоя_".to_string()),
        );
        config
            .parameters
            .insert(DiagnosticCode::UnusedLocalMethod, serde_json::Value::Object(params));

        let diagnostics = check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        assert_eq!(
            unused_diags.len(),
            3,
            "Expected 3 diagnostics with custom prefixes, got {}",
            unused_diags.len()
        );

        crate::test_utils::assert_diagnostic_range(code, unused_diags[0], 1, 10, 24);
        crate::test_utils::assert_diagnostic_range(code, unused_diags[1], 60, 10, 40);
        crate::test_utils::assert_diagnostic_range(code, unused_diags[2], 63, 10, 39);
    }

    #[test]
    fn test_report_compose_result_not_flagged_in_object_module() {
        use crate::test_utils::{
            check_metadata_diagnostic_with_config, make_non_common_module_metadata,
        };

        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, ДанныеРасшифровки, СтандартнаяОбработка)
    ДокументРезультат.Очистить();
КонецПроцедуры

Процедура НикемНеВызывается()
    Возврат;
КонецПроцедуры"#;

        let mut config = DiagnosticsConfig::default();
        let mut params = serde_json::Map::new();
        params.insert("checkObjectModule".to_string(), serde_json::Value::Bool(true));
        config
            .parameters
            .insert(DiagnosticCode::UnusedLocalMethod, serde_json::Value::Object(params));

        let mut metadata = make_non_common_module_metadata(bsl_metadata::ModuleType::ObjectModule);
        metadata.mdo = Some(std::sync::Arc::new(bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Report,
            "ИсторияРеквизитов",
        )));
        let diagnostics =
            check_metadata_diagnostic_with_config(metadata, code, config, |_meta, ctx| {
                super::check(ctx)
            });
        let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();

        assert_eq!(messages, vec!["Неиспользуемый локальный метод \"НикемНеВызывается\""]);
    }

    #[test]
    fn test_report_compose_result_gated_by_external_object_kind() {
        use crate::test_utils::{
            check_metadata_diagnostic_with_config, make_non_common_module_metadata,
        };

        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, ДанныеРасшифровки, СтандартнаяОбработка)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        let flagged_for = |mdo_type| {
            let mut config = DiagnosticsConfig::default();
            let mut params = serde_json::Map::new();
            params.insert("checkObjectModule".to_string(), serde_json::Value::Bool(true));
            config
                .parameters
                .insert(DiagnosticCode::UnusedLocalMethod, serde_json::Value::Object(params));

            let mut metadata =
                make_non_common_module_metadata(bsl_metadata::ModuleType::ObjectModule);
            metadata.mdo =
                Some(std::sync::Arc::new(bsl_metadata::MetadataObject::new(mdo_type, "Внешний")));
            check_metadata_diagnostic_with_config(metadata, code, config, |_meta, ctx| {
                super::check(ctx)
            })
            .into_iter()
            .map(|d| d.message)
            .collect::<Vec<_>>()
        };

        assert!(flagged_for(bsl_metadata::MdoType::ExternalReport).is_empty());
        assert_eq!(
            flagged_for(bsl_metadata::MdoType::ExternalDataProcessor),
            vec!["Неиспользуемый локальный метод \"ПриКомпоновкеРезультата\"".to_string()]
        );
    }

    #[test]
    fn test_exported_method_not_flagged() {
        let code = r#"
Процедура ПубличнаяПроцедура() Экспорт
КонецПроцедуры

Процедура ЛокальнаяПроцедура()
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic(code);
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
            UnusedLocalMethod @ 5:11..5:29
              message: Неиспользуемый локальный метод "ЛокальнаяПроцедура"
              severity: Warning"#]],
        );
        assert!(unused_diags[0].message.contains("ЛокальнаяПроцедура"));
    }

    #[test]
    fn test_used_method_not_flagged() {
        let code = r#"
Процедура ИспользуемаяПроцедура()
КонецПроцедуры

Процедура Главная()
    ИспользуемаяПроцедура();
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic(code);
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
            UnusedLocalMethod @ 5:11..5:18
              message: Неиспользуемый локальный метод "Главная"
              severity: Warning"#]],
        );
        assert!(unused_diags[0].message.contains("Главная"));
    }

    #[test]
    fn test_callback_used_via_qualified_call_not_flagged() {
        // BSP "Свойства" pattern: the form's local no-arg callback is re-armed by
        // name from a common-module wrapper that shares its name. No bare call to
        // the local procedure exists, only the qualified `Модуль.Имя(...)` call, so
        // the method segment of a member call must count as a use.
        let code = r#"
&НаКлиенте
Процедура ОбновитьЗависимостиДополнительныхРеквизитов()
    УправлениеСвойствамиКлиент.ОбновитьЗависимостиДополнительныхРеквизитов(ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура НеИспользуемая()
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
                UnusedLocalMethod @ 8:11..8:25
                  message: Неиспользуемый локальный метод "НеИспользуемая"
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_set_action_handler_not_flagged() {
        // `Элементы.X.УстановитьДействие("Событие", "Обработчик")` binds the handler by
        // name at runtime; the procedure has no other call site and must not be flagged
        // in a form module. The genuinely unused one must stay flagged.
        let code = r#"
&НаКлиенте
Процедура УстановитьОбработчики()
    Элементы.Валюта.УстановитьДействие("ПриИзменении", "ВалютаПриИзменении");
КонецПроцедуры

&НаКлиенте
Процедура ВалютаПриИзменении(Элемент)
КонецПроцедуры

&НаКлиенте
Процедура НеИспользуемая()
КонецПроцедуры
"#;

        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: None,
        };

        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let unused: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnusedLocalMethod)
            .map(|d| d.message.clone())
            .collect();

        assert!(
            !unused.iter().any(|m| m.contains("ВалютаПриИзменении")),
            "SetAction-bound handler must not be flagged: {unused:?}"
        );
        assert!(
            unused.iter().any(|m| m.contains("НеИспользуемая")),
            "genuinely unused method must stay flagged: {unused:?}"
        );
        assert!(
            unused.iter().any(|m| m.contains("УстановитьОбработчики")),
            "the registering method itself is uncalled and must stay flagged: {unused:?}"
        );
    }

    #[test]
    fn test_set_action_handler_flagged_outside_form_module() {
        // Outside a form module `УстановитьДействие` is some object's method, not a form
        // event binding, so it must not exempt a same-named local in (e.g.) a common module.
        let code = r#"
Процедура Настроить()
    Объект.УстановитьДействие("Событие", "Обработчик");
КонецПроцедуры

Процедура Обработчик()
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::CommonModule, code);
        let names: Vec<_> = diags.iter().map(|d| d.message.clone()).collect();
        assert!(
            names.iter().any(|m| m.contains("Обработчик")),
            "non-form SetAction must not exempt a local: {names:?}"
        );
    }

    #[test]
    fn test_name_literal_collision_suppresses_local_in_form() {
        // Documents an intentional precision tradeoff. In a form module, any
        // identifier-shaped string literal naming a local method counts as a use —
        // dynamic handler binding (code-created commands, helper modules fed a
        // parameter structure) leaves the literal as its only same-module trace. The
        // price: string *data* that happens to name a dead local exempts it, as with
        // this manager-method call whose second argument is not a handler.
        let code = r#"
&НаСервере
Процедура Настроить()
    Справочники.Номенклатура.УстановитьДействие("Опция", "НеИспользуемая");
КонецПроцедуры

&НаКлиенте
Процедура НеИспользуемая()
КонецПроцедуры
"#;
        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: None,
        };
        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let names: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnusedLocalMethod)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            !names.iter().any(|m| m.contains("НеИспользуемая")),
            "a name literal matching a local method exempts it in a form module: {names:?}"
        );
    }

    #[test]
    fn test_name_literal_bound_handler_not_flagged_in_form() {
        // The handler name reaches `УстановитьДействие` only inside a helper from
        // another module — via a parameter structure or a code-created command — so
        // the same-module name literal is what marks the handler as reachable.
        let code = r#"
&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Параметры = Новый Структура("ИмяСобытия, ИмяПроцедурыОбработчика", "ПриИзменении", "ВесБруттоПриИзменении");
    Помощники.ДобавитьПолеФормы(ЭтаФорма, Параметры);
    Помощники.СоздатьКоманду(ЭтаФорма, "Дозагруз", "Дозагруз", "ДозагрузКоманда");
КонецПроцедуры

&НаКлиенте
Процедура ВесБруттоПриИзменении(Элемент)
КонецПроцедуры

&НаКлиенте
Процедура ДозагрузКоманда(Команда)
КонецПроцедуры

&НаКлиенте
Процедура ЛишняяПроцедура()
КонецПроцедуры
"#;
        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: None,
        };
        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let names: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnusedLocalMethod)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            !names.iter().any(|m| m.contains("ВесБруттоПриИзменении")),
            "structure-bound handler must not be flagged: {names:?}"
        );
        assert!(
            !names.iter().any(|m| m.contains("ДозагрузКоманда")),
            "helper-created command action must not be flagged: {names:?}"
        );
        assert!(
            names.iter().any(|m| m.contains("ЛишняяПроцедура")),
            "genuinely unused method must stay flagged: {names:?}"
        );
    }

    #[test]
    fn test_name_literal_does_not_exempt_outside_form_module() {
        let code = r#"
Процедура Настроить() Экспорт
    Имя = "Обработчик";
КонецПроцедуры

Процедура Обработчик()
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::CommonModule, code);
        let names: Vec<_> = diags.iter().map(|d| d.message.clone()).collect();
        assert!(
            names.iter().any(|m| m.contains("Обработчик")),
            "name literals exempt locals only in form modules: {names:?}"
        );
    }

    #[test]
    fn test_member_call_name_collision_suppresses_local() {
        // Documents an intentional precision tradeoff. The method segment of a
        // member call is counted receiver-blind, so a dead local `Очистить()`
        // is treated as used once any `Получатель.Очистить()` appears. This
        // mirrors bsl-language-server's name-based reachability and is the price
        // of catching the BSP callback pattern without cross-module flow.
        let code = r#"
Процедура Очистить()
КонецПроцедуры

Процедура Главная(Массив)
    Массив.Очистить();
КонецПроцедуры

Главная(Неопределено);
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_extension_annotations_not_flagged() {
        let code = r#"
&Вместо("ОригинальныйМетод")
Процедура Расш_ОригинальныйМетод()
КонецПроцедуры

&Перед("ДругойМетод")
Процедура Расш_ДругойМетод()
КонецПроцедуры

&После("ТретийМетод")
Процедура Расш_ТретийМетод()
КонецПроцедуры

&ИзменениеИКонтроль("ЧетвертыйМетод")
Процедура Расш_ЧетвертыйМетод()
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_attachable_methods_not_flagged() {
        let code = r#"
Процедура Подключаемый_ОбработчикСобытия()
КонецПроцедуры

Процедура Attachable_EventHandler()
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_handler_methods_not_flagged() {
        let code = r#"
Процедура ПриСозданииОбъекта(Параметр)
КонецПроцедуры

Процедура OnObjectCreate(Parameter)
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_platform_event_handlers_not_flagged() {
        let code = r#"
Процедура ПередЗаписью(Отказ)
КонецПроцедуры

Процедура ПриЗаписи(Отказ)
КонецПроцедуры

Процедура ПередУдалением(Отказ)
КонецПроцедуры

Процедура ОбработкаЗаполнения(ДанныеЗаполнения)
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_method_called_in_module_code() {
        let code = r#"
Процедура ВызываемаяПроцедура()
КонецПроцедуры

ВызываемаяПроцедура();
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalMethod, expect![[r#""#]]);
    }

    #[test]
    fn test_http_service_handler_not_flagged() {
        use std::sync::Arc;

        let code = r#"
Функция createPOST(Запрос)
    Возврат Запрос;
КонецФункции

Функция НеИспользуемая()
    Возврат 1;
КонецФункции
"#;

        let method = bsl_metadata::HTTPServiceMethodBuilder::new()
            .name("POST")
            .http_method("POST")
            .handler("createPOST")
            .build();
        let template = bsl_metadata::HTTPServiceURLTemplateBuilder::new()
            .name("create")
            .template("/client/create")
            .add_method(method)
            .build();
        let http_service =
            bsl_metadata::HTTPServiceBuilder::new().name("lk").add_url_template(template).build();

        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::HTTPServiceModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: Some(Arc::new(http_service)),
            web_service: None,
            integration_service: None,
        };

        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        assert_eq!(unused_diags.len(), 1);
        assert!(unused_diags[0].message.contains("НеИспользуемая"));
    }

    #[test]
    fn test_idle_handler_callback_not_flagged() {
        let code = r#"
Процедура ПриОткрытии() Экспорт
    ПодключитьОбработчикОжидания("ОкончаниеПостроенияФормы", 0.1, Истина);
КонецПроцедуры

Процедура ОкончаниеПостроенияФормы()
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
                UnusedLocalMethod @ 9:11..9:25
                  message: Неиспользуемый локальный метод "НеИспользуемая"
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_notify_description_callback_not_flagged() {
        let code = r#"
Процедура ОткрытьВыбор() Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеВыбораФайла", ЭтотОбъект);
КонецПроцедуры

Процедура ПослеВыбораФайла(Результат, ДополнительныеПараметры) Экспорт
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
                UnusedLocalMethod @ 9:11..9:25
                  message: Неиспользуемый локальный метод "НеИспользуемая"
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_web_service_operation_handler_not_flagged() {
        use std::sync::Arc;

        let code = r#"
Функция GetJobsCount(СтрокаПараметров)
    Возврат 0;
КонецФункции

Функция НеИспользуемая()
    Возврат 1;
КонецФункции
"#;

        let operation = bsl_metadata::WebServiceOperationBuilder::new()
            .name("GetJobsCount")
            .procedure_name("GetJobsCount")
            .build();
        let web_service = bsl_metadata::WebServiceBuilder::new()
            .name("WMSMobileClientExchange")
            .add_operation(operation)
            .build();

        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::WebServiceModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: Some(Arc::new(web_service)),
            integration_service: None,
        };

        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "operation-bound method must be exempt, unbound one must stay flagged: {unused_diags:?}"
        );
        assert!(unused_diags[0].message.contains("НеИспользуемая"));
    }

    #[test]
    fn test_form_element_event_handler_not_flagged() {
        use std::sync::Arc;

        let code = r#"
&НаКлиенте
Процедура СписокПриАктивизацииСтроки(Элемент)
КонецПроцедуры

&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;

        let form_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
    <Events>
        <Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
    </Events>
    <ChildItems>
        <Table name="Список" id="1">
            <Events>
                <Event name="OnActivateRow">СписокПриАктивизацииСтроки</Event>
            </Events>
        </Table>
    </ChildItems>
</Form>"#;

        let form = bsl_metadata::xml_parser::parse_form_xml(form_xml).unwrap();

        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: Some(Arc::new(form)),
            http_service: None,
            web_service: None,
            integration_service: None,
        };

        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "Expected 1 diagnostic, got {}. Diagnostics: {:?}",
            unused_diags.len(),
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert!(unused_diags[0].message.contains("НеИспользуемая"));
    }

    #[test]
    fn test_integration_service_channel_handler_not_flagged() {
        use std::sync::Arc;

        let code = r#"
Процедура ОбработатьСообщениеОбычныйПриоритет(Сообщение, Отказ)
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;

        let channel = bsl_metadata::IntegrationServiceChannelBuilder::new()
            .name("input_from_SM_normal_priority")
            .receive_message_processing("ОбработатьСообщениеОбычныйПриоритет")
            .build();
        let service = bsl_metadata::IntegrationServiceBuilder::new()
            .name("ОбменСообщениями")
            .add_channel(channel)
            .build();

        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::IntegrationServiceModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: Some(Arc::new(service)),
        };

        let diagnostics =
            crate::test_utils::check_metadata_diagnostic(metadata, code, |_metadata, ctx| {
                super::check(ctx)
            });
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "channel-bound handler must be exempt, unbound one must stay flagged: {unused_diags:?}"
        );
        assert!(unused_diags[0].message.contains("НеИспользуемая"));
    }

    fn module_metadata(module_type: bsl_metadata::ModuleType) -> hir::ModuleMetadata {
        hir::ModuleMetadata {
            module_type,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            form: None,
            http_service: None,
            web_service: None,
            integration_service: None,
        }
    }

    fn unused_in_module(
        module_type: bsl_metadata::ModuleType,
        code_text: &str,
    ) -> Vec<crate::Diagnostic> {
        let diagnostics = crate::test_utils::check_metadata_diagnostic(
            module_metadata(module_type),
            code_text,
            |_metadata, ctx| super::check(ctx),
        );
        diagnostics.into_iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect()
    }

    #[test]
    fn test_application_module_event_handlers_not_flagged() {
        let code = r#"
Процедура ПередНачаломРаботыСистемы()
КонецПроцедуры

Процедура ОбработкаВнешнегоСобытия(Источник, Событие, Данные)
КонецПроцедуры

Процедура ПриГлобальномПоиске(СтрокаПоиска, ПланПоиска)
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::ManagedApplicationModule, code);
        assert_eq!(
            diags.len(),
            1,
            "only the genuinely unused method must stay flagged: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert!(diags[0].message.contains("НеИспользуемая"));
    }

    #[test]
    fn test_session_module_handler_not_flagged() {
        let code = r#"
Процедура УстановкаПараметровСеанса(ИменаПараметровСеанса)
КонецПроцедуры

Процедура НеИспользуемая()
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::SessionModule, code);
        assert_eq!(diags.len(), 1, "{:?}", diags.iter().map(|d| &d.message).collect::<Vec<_>>());
        assert!(diags[0].message.contains("НеИспользуемая"));
    }

    #[test]
    fn test_application_handler_name_in_common_module_still_flagged() {
        // Strict module-kind gating: an application-module handler NAME living in a
        // common module is not a platform entry point there and must stay flagged.
        let code = r#"
Процедура ПередНачаломРаботыСистемы()
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::CommonModule, code);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("ПередНачаломРаботыСистемы"));
    }

    #[test]
    fn test_external_connection_module_only_run_lifecycle_exempt() {
        // The non-interactive external connection invokes start/exit only. A
        // managed-only UI handler and the interactive "before" hook are NOT entry
        // points here and must stay flagged.
        let code = r#"
Процедура ПриНачалеРаботыСистемы()
КонецПроцедуры

Процедура ПередНачаломРаботыСистемы()
КонецПроцедуры

Процедура ПриГлобальномПоиске(СтрокаПоиска, ПланПоиска)
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::ExternalConnectionModule, code);
        let names: Vec<_> = diags.iter().map(|d| d.message.clone()).collect();
        assert_eq!(diags.len(), 2, "{names:?}");
        assert!(names.iter().any(|m| m.contains("ПередНачаломРаботыСистемы")));
        assert!(names.iter().any(|m| m.contains("ПриГлобальномПоиске")));
    }

    #[test]
    fn test_ordinary_application_module_no_managed_ui_handlers() {
        // Ordinary application exposes lifecycle + external event, but the
        // managed-only global-search handler must stay flagged.
        let code = r#"
Процедура ПередНачаломРаботыСистемы()
КонецПроцедуры

Процедура ПриГлобальномПоиске(СтрокаПоиска, ПланПоиска)
КонецПроцедуры
"#;
        let diags = unused_in_module(bsl_metadata::ModuleType::OrdinaryApplicationModule, code);
        assert_eq!(diags.len(), 1, "{:?}", diags.iter().map(|d| &d.message).collect::<Vec<_>>());
        assert!(diags[0].message.contains("ПриГлобальномПоиске"));
    }

    #[test]
    fn test_case_insensitive_call() {
        let code = r#"
Процедура МояПроцедура()
КонецПроцедуры

Процедура Главная()
    МОЯПРОЦЕДУРА();
КонецПроцедуры
"#;

        let diagnostics = check_hir_diagnostic(code);
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalMethod).collect();

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalMethod,
            expect![[r#"
            UnusedLocalMethod @ 5:11..5:18
              message: Неиспользуемый локальный метод "Главная"
              severity: Warning"#]],
        );
        assert!(unused_diags[0].message.contains("Главная"));
    }
}
