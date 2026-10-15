use crate::define_metadata;
use crate::metadata::*;
use crate::utils::platform_event_handlers::{
    is_platform_event_handler, is_report_object_module_event_handler, ModuleOwner,
};
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::traits::MdObject;
use hir::{Expr, ModItem};
use ide_db::TextRange;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Os,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Design, MetadataTag::Unused],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const DEFAULT_ATTACHABLE_PREFIXES: &str = "подключаемый_,attachable_";

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::UnusedParameters;

    if ctx.is_disabled_with_metadata(code) {
        return vec![];
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

    let mut diagnostics = Vec::new();

    let module_bodies = ctx.module_bodies();
    let item_tree = ctx.item_tree();

    let metadata = ctx.module_metadata();
    let mut fixed_signature_handlers: FxHashSet<String> = FxHashSet::default();
    if let Some(ref form) = metadata.form {
        for handler in form.event_handlers() {
            fixed_signature_handlers.insert(handler.handler_name.fold_lower());
        }
        for handler in form.command_handlers() {
            fixed_signature_handlers.insert(handler.fold_lower());
        }
    }
    if let Some(ref http_service) = metadata.http_service {
        for (_, method) in http_service.all_methods() {
            if !method.is_handler_empty() {
                fixed_signature_handlers.insert(method.handler().fold_lower());
            }
        }
    }

    for subscription in ctx.visible_event_subscriptions() {
        if let Some(handler) = subscription.parse_handler() {
            if metadata.common_module.as_ref().is_some_and(|module| {
                handler.module_name.fold_lower() == module.name().fold_lower()
            }) && !handler.method_name.is_empty()
            {
                fixed_signature_handlers.insert(handler.method_name.fold_lower());
            }
        }
    }

    let module_id = hir::ModuleId::new(ctx.file_id);
    let summary = ctx.call_summary(module_id);
    for reg in &summary.notify_regs {
        // A callback handled in the current module (or whose receiver we cannot
        // classify) keeps the platform-fixed signature, so its parameters are not
        // "unused". A handler resolved to another module is checked there instead.
        if matches!(reg.target, hir::NotifyTarget::ThisObject | hir::NotifyTarget::Unsupported) {
            fixed_signature_handlers.insert(reg.callback_name.as_str().fold_lower());
        }
    }
    // A form-module procedure bound at runtime — `УстановитьДействие` directly, or by
    // name through a code-created command / a helper in another module — has a
    // platform-fixed signature the author cannot trim. `set_action_regs` is the typed,
    // receiver-checked fact; `name_literal_refs` is a deliberately broad name-match that
    // also covers indirect bindings, accepting that string data coinciding with a method
    // name exempts that method's parameters too.
    if metadata.module_type == bsl_metadata::ModuleType::FormModule {
        // An EXPORTED form-module method is an interface for code outside the form -
        // often outside the configuration altogether (an external processor calling
        // `Форма.Метод(...)` by name). The caller dictates the signature; a parameter the
        // body ignores is not the author's to drop.
        for method in summary.methods.iter().filter(|m| m.is_export) {
            fixed_signature_handlers.insert(method.name.as_str().fold_lower());
        }
        for reg in &summary.set_action_regs {
            fixed_signature_handlers.insert(reg.handler_name.as_str().fold_lower());
        }
        for local_id in &summary.name_literal_refs {
            if let Some(method) = summary.methods.iter().find(|m| m.local_id == *local_id) {
                fixed_signature_handlers.insert(method.name.as_str().fold_lower());
            }
        }
        // A handler bound by string from another module (a helper handed the form) is
        // called by the platform with its own fixed signature, like any other handler.
        let bindings = ctx.workspace_foreign_handler_bindings();
        if !bindings.is_empty() {
            for method in &summary.methods {
                if bindings.is_bound(method.name.as_str()) {
                    fixed_signature_handlers.insert(method.name.as_str().fold_lower());
                }
            }
        }
    }

    for (local_id, _) in module_bodies.iter_bodies() {
        if item_tree.method(local_id).is_some_and(|method| {
            method.annotations().iter().any(|annotation| annotation.kind.is_interception())
        }) {
            if let Some(name) = get_method_name(&item_tree, local_id) {
                fixed_signature_handlers.insert(name.fold_lower());
            }
        }
        if let Some(name) = get_method_name(&item_tree, local_id) {
            let lower = name.fold_lower();
            if attachable_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
                fixed_signature_handlers.insert(lower);
            }
        }
    }

    let exemptions = SignatureExemptions {
        fixed_handlers: &fixed_signature_handlers,
        owner: ModuleOwner {
            module_type: metadata.module_type,
            mdo_type: metadata.mdo.as_ref().map(|mdo| mdo.mdo_type),
        },
    };

    for (local_id, body) in module_bodies.iter_bodies() {
        let method_name = get_method_name(&item_tree, local_id);

        diagnostics.extend(check_method(
            local_id,
            body,
            method_name.as_deref(),
            &module_bodies,
            &exemptions,
            code,
            ctx,
        ));
    }

    diagnostics
}

/// What keeps a method's signature out of the author's hands: names bound at runtime
/// or declared in metadata, plus the module kind that decides which platform events
/// this module can receive at all.
struct SignatureExemptions<'a> {
    fixed_handlers: &'a FxHashSet<String>,
    owner: ModuleOwner,
}

fn get_method_name(item_tree: &hir::ItemTree, local_id: hir::MethodKey) -> Option<String> {
    let item = item_tree.item_of(local_id)?;
    match item {
        ModItem::Procedure(idx) => Some(item_tree.procedure(*idx).name.as_str().to_string()),
        ModItem::Function(idx) => Some(item_tree.function(*idx).name.as_str().to_string()),
        ModItem::Variable(_) => None,
    }
}

fn check_method(
    local_id: hir::MethodKey,
    body: &hir::Body,
    method_name: Option<&str>,
    module_bodies: &hir::ModuleBodies,
    exemptions: &SignatureExemptions<'_>,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    if body.params().next().is_none() {
        return diagnostics;
    }

    if is_empty_body(body) {
        return diagnostics;
    }

    if method_name.is_some_and(is_platform_event_handler) {
        return diagnostics;
    }

    if method_name.is_some_and(|name| is_report_object_module_event_handler(name, exemptions.owner))
    {
        return diagnostics;
    }

    if method_name.is_some_and(|name| exemptions.fixed_handlers.contains(&name.fold_lower())) {
        return diagnostics;
    }

    let source_map = match module_bodies.source_map(local_id) {
        Some(sm) => sm,
        None => return diagnostics,
    };

    let used_names = collect_used_identifiers(body);

    for param_id in body.params() {
        let binding = body.binding(param_id);
        let param_name = binding.name.as_str();
        let param_name_lower = param_name.fold_lower();

        if !used_names.contains(&param_name_lower) {
            if let Some(range) = source_map.binding_range(param_id) {
                diagnostics.push(create_diagnostic(param_name, range, code, ctx));
            }
        }
    }

    diagnostics
}

fn collect_used_identifiers(body: &hir::Body) -> FxHashSet<String> {
    let mut used = FxHashSet::default();

    for (_, expr) in body.exprs_iter() {
        if let Expr::Path(name) = expr {
            used.insert(name.as_str().fold_lower());
        }
    }

    used
}

fn is_empty_body(body: &hir::Body) -> bool {
    body.body_stmts().next().is_none()
}

fn create_diagnostic(
    name: &str,
    range: TextRange,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Diagnostic {
    Diagnostic {
        code,
        message: format!("Уберите неиспользуемый параметр \"{}\"", name),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_unused_parameter() {
        let code = r#"Процедура ВсеПлохо(А1, Знач Б1 = Ложь)
    ВызовМетода(А1);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 1:29..1:31
              message: Уберите неиспользуемый параметр "Б1"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_unused_parameter_export() {
        let code = r#"Процедура ВсеПлохоИЭкспорт(А2, Знач Б2 = Ложь) Экспорт
    Вызов(А2);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 1:37..1:39
              message: Уберите неиспользуемый параметр "Б2"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_all_parameters_used() {
        let code = r#"Процедура ВсеХорошо(А3, Б3)
    Б3 = А3 + 1;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_empty_body_no_diagnostic() {
        let code = r#"Процедура Просто(А) Экспорт
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_oncreate_handler_no_diagnostic() {
        let code = r#"Процедура ПриСозданииОбъекта(Отказ)
    Если ЧтоТо Тогда
    КонецЕсли;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_platform_event_handler_no_diagnostic() {
        let code = r#"Процедура ПриЗаписи(Отказ)
    Если ЧтоТо Тогда
    КонецЕсли;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    fn object_module_of(mdo_type: bsl_metadata::MdoType, name: &str) -> hir::ModuleMetadata {
        let mut metadata = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::ObjectModule,
        );
        metadata.mdo = Some(std::sync::Arc::new(bsl_metadata::MetadataObject::new(mdo_type, name)));
        metadata
    }

    fn check_object_module_snapshot(
        metadata: hir::ModuleMetadata,
        code: &str,
        expected: expect_test::Expect,
    ) {
        use crate::test_utils::{check_metadata_diagnostic, format_diags};

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        expected.assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_report_compose_result_handler_no_diagnostic() {
        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, ДанныеРасшифровки, СтандартнаяОбработка)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        check_object_module_snapshot(
            object_module_of(bsl_metadata::MdoType::Report, "ИсторияРеквизитов"),
            code,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_report_compose_result_handler_english_no_diagnostic() {
        let code = r#"Процедура OnComposeResult(ДокументРезультат, ДанныеРасшифровки, СтандартнаяОбработка)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        check_object_module_snapshot(
            object_module_of(bsl_metadata::MdoType::Report, "ИсторияРеквизитов"),
            code,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_report_compose_result_handler_external_report_no_diagnostic() {
        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, ДанныеРасшифровки, СтандартнаяОбработка)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        check_object_module_snapshot(
            object_module_of(bsl_metadata::MdoType::ExternalReport, "ВнешнийОтчет"),
            code,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_compose_result_name_in_catalog_object_module_is_flagged() {
        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, Лишний)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        check_object_module_snapshot(
            object_module_of(bsl_metadata::MdoType::Catalog, "Товары"),
            code,
            expect![[r#"
                UnusedParameters @ 1:54..1:60
                  message: Уберите неиспользуемый параметр "Лишний"
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_compose_result_name_outside_object_module_is_flagged() {
        let code = r#"Процедура ПриКомпоновкеРезультата(ДокументРезультат, Лишний)
    ДокументРезультат.Очистить();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 1:54..1:60
              message: Уберите неиспользуемый параметр "Лишний"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_parameter_used_in_field_access() {
        let code = r#"Процедура ВсеХорошо(Объект, Объект2, Объект3)
    Объект.Поле = 1;
    Объект2.Поле.Метод(2);
    Чтото[Объект3];
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_detects_unused_parameters_in_fixture() {
        let code = r#"Процедура ВсеПлохо(А1, Знач Б1 = Ложь) // Параметр Б
    ВызовМетода(А1);
КонецПроцедуры

Процедура ВсеПлохоИЭкспорт(А2, Знач Б2 = Ложь) Экспорт
    Вызов(А2);
КонецПроцедуры

Процедура ВсеХорошо(А3, Б3)
    //Если А3 Тогда
    Б3 = А3 + 1;
    //КонецЕсли;
КонецПроцедуры

Процедура Просто(А) Экспорт
КонецПроцедуры

Процедура ПриСозданииОбъекта(Отказ)
    Если ЧтоТо Тогда
    КонецЕсли;
КонецПроцедуры

Процедура ВсеХорошо(Объект, Объект2, Объект3)
    Объект.Поле = 1;
    Объект2.Поле.Метод(2);
    Чтото[Объект3];
КонецПроцедуры

Процедура нпе( , знач "")
    Объект.Поле = 1;
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 1:29..1:31
              message: Уберите неиспользуемый параметр "Б1"
              severity: Warning
            UnusedParameters @ 5:37..5:39
              message: Уберите неиспользуемый параметр "Б2"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_form_element_event_handler_no_diagnostic() {
        use std::sync::Arc;

        let code = r#"&НаКлиенте
Процедура СписокПриАктивизацииСтроки(Элемент)
    Если ЧтоТо Тогда
    КонецЕсли;
КонецПроцедуры
"#;

        let form_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20">
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
        let unused: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedParameters).collect();

        assert_eq!(unused.len(), 0);
    }

    fn form_module_metadata() -> hir::ModuleMetadata {
        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
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

    #[test]
    fn test_set_action_handler_params_no_diagnostic() {
        // `УстановитьДействие` binds the procedure as a platform event handler; its
        // signature is fixed by the platform, so an untouched `Элемент` is not unused.
        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Элементы.Валюта.УстановитьДействие("ПриИзменении", "ВалютаПриИзменении");
КонецПроцедуры

&НаКлиенте
Процедура ВалютаПриИзменении(Элемент)
    Сообщить("х");
КонецПроцедуры
"#;
        let diagnostics = crate::test_utils::check_metadata_diagnostic(
            form_module_metadata(),
            code,
            |_metadata, ctx| super::check(ctx),
        );
        let unused: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnusedParameters)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            !unused.iter().any(|m| m.contains("Элемент")),
            "SetAction-bound handler keeps its fixed signature: {unused:?}"
        );
    }

    #[test]
    fn test_name_literal_handler_params_no_diagnostic() {
        // The handler is bound inside a helper module fed a parameter structure; the
        // name literal in this module fixes the signature. A regular method's unused
        // parameter must stay flagged.
        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Параметры = Новый Структура("ИмяСобытия, ИмяПроцедурыОбработчика", "ПриИзменении", "ВесБруттоПриИзменении");
    Помощники.ДобавитьПолеФормы(ЭтаФорма, Параметры);
КонецПроцедуры

&НаКлиенте
Процедура ВесБруттоПриИзменении(Элемент)
    Сообщить("х");
КонецПроцедуры

Процедура Обычная(Лишний)
    Сообщить("х");
КонецПроцедуры
"#;
        let diagnostics = crate::test_utils::check_metadata_diagnostic(
            form_module_metadata(),
            code,
            |_metadata, ctx| super::check(ctx),
        );
        let unused: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnusedParameters)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            !unused.iter().any(|m| m.contains("Элемент")),
            "name-literal-bound handler keeps its fixed signature: {unused:?}"
        );
        assert!(
            unused.iter().any(|m| m.contains("Лишний")),
            "an ordinary unused parameter must stay flagged: {unused:?}"
        );
    }

    #[test]
    fn test_name_literal_params_flagged_outside_form_module() {
        let code = r#"Процедура Настроить() Экспорт
    Имя = "Обработчик";
КонецПроцедуры

Процедура Обработчик(Параметр)
    Сообщить("х");
КонецПроцедуры
"#;
        let metadata = hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::CommonModule,
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
            .filter(|d| d.code == DiagnosticCode::UnusedParameters)
            .map(|d| d.message.clone())
            .collect();
        assert!(
            unused.iter().any(|m| m.contains("Параметр")),
            "name literals fix signatures only in form modules: {unused:?}"
        );
    }

    #[test]
    fn test_http_service_handler_no_diagnostic() {
        use std::sync::Arc;

        let code = r#"Функция ЗапросPOST(Запрос)
    Возврат Новый HTTPСервисОтвет(200);
КонецФункции
"#;

        let http_service = bsl_metadata::HTTPServiceBuilder::new()
            .name("TestService")
            .root_url("test")
            .add_url_template(
                bsl_metadata::HTTPServiceURLTemplateBuilder::new()
                    .name("Запрос")
                    .template("/query")
                    .add_method(
                        bsl_metadata::HTTPServiceMethodBuilder::new()
                            .name("POST")
                            .http_method("POST")
                            .handler("ЗапросPOST")
                            .build(),
                    )
                    .build(),
            )
            .build();

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
        let unused: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedParameters).collect();

        assert_eq!(unused.len(), 0);
    }

    #[test]
    fn test_platform_handler_after_var_def() {
        let code = r#"Перем МояПеременная;

Процедура ПриЗаписи(Отказ)
    Если ЧтоТо Тогда
    КонецЕсли;
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_attachable_method_no_diagnostic() {
        let code = r#"&НаКлиенте
Процедура Подключаемый_ПродолжитьВыполнениеКомандыНаСервере(ПараметрыВыполнения, ДополнительныеПараметры) Экспорт
    ВыполнитьКомандуНаСервере(ПараметрыВыполнения);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_attachable_method_english_no_diagnostic() {
        let code = r#"&AtClient
Procedure Attachable_ContinueCommandExecutionAtServer(ExecutionParameters, AdditionalParameters) Export
    ExecuteCommandAtServer(ExecutionParameters);
EndProcedure
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_non_attachable_still_flags_unused() {
        let code = r#"Процедура ОбычнаяПроцедура(Параметр1, Параметр2) Экспорт
    Вызов(Параметр1);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 1:39..1:48
              message: Уберите неиспользуемый параметр "Параметр2"
              severity: Warning"#]],
        );
    }

    #[test]
    fn extension_interception_keeps_the_platform_signature() {
        let code = r#"&После("ОбработкаЗаполнения")
Процедура ПослеЗаполнения(ДанныеЗаполнения, СтандартнаяОбработка)
    Вызов(ДанныеЗаполнения);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn change_and_validate_interception_keeps_the_original_signature() {
        let code = r#"&ИзменениеИКонтроль("ЗаполнитьТовары")
Процедура Расш1_ЗаполнитьТовары(Параметр, Отказ)
    Вызов(Параметр);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn event_subscription_handler_keeps_its_declared_signature() {
        assert_event_subscription_handler_keeps_its_signature("CommonModule.Сервер.Обработчик");
    }

    #[test]
    fn event_subscription_handler_module_name_ignores_cyrillic_case() {
        assert_event_subscription_handler_keeps_its_signature("CommonModule.сервер.Обработчик");
    }

    fn assert_event_subscription_handler_keeps_its_signature(handler: &str) {
        use crate::test_utils::check_cfe_at_with_unreadable;
        use test_fixture::CfeFixtureBuilder;

        let mut builder = CfeFixtureBuilder::new("");
        builder
            .add_extension("Расширение", "")
            .add_extension_module(
                "Расширение",
                "Сервер",
                "Процедура Обработчик(Источник, Отказ, Замещение) Экспорт\n    Если Отказ Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры",
            );
        let fixture = builder.build();
        let subscriptions = fixture.extensions()[0].root().join("EventSubscriptions");
        std::fs::create_dir_all(&subscriptions).expect("create EventSubscriptions directory");
        std::fs::write(
            subscriptions.join("Подписка.xml"),
            format!(
                r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
<EventSubscription uuid="aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"><Properties>
<Name>Подписка</Name><Source>Document.Документ1</Source><Event>BeforeWrite</Event>
<Handler>{handler}</Handler>
</Properties></EventSubscription></MetaDataObject>"#
            ),
        )
        .expect("write event subscription fixture");

        let code = "Процедура Обработчик(Источник, Отказ, Замещение) Экспорт\n    Если Отказ Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры";
        let diagnostics = check_cfe_at_with_unreadable(
            "CommonModules/Сервер/Ext/Module.bsl",
            code,
            fixture,
            &[],
            super::check,
        );
        let unused = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::UnusedParameters)
            .collect::<Vec<_>>();
        assert!(unused.is_empty(), "event subscription signature is fixed by metadata: {unused:?}");
    }

    #[test]
    fn test_notify_description_callback_no_diagnostic() {
        let code = r#"&НаКлиенте
Процедура СоздатьНаОснованииАктУПД()
    ОписаниеОповещения = Новый ОписаниеОповещения("ПослеВыбораОснования", ЭтотОбъект);
    ТекущиеДанные.ПоказатьВыборЭлемента(ОписаниеОповещения, "Выбор");
КонецПроцедуры

&НаКлиенте
Процедура ПослеВыбораОснования(ВыбранныйЭлемент, Параметры) Экспорт
    Если ВыбранныйЭлемент <> Неопределено Тогда
        Вызов(ВыбранныйЭлемент.Значение);
    КонецЕсли;
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_notify_description_english_no_diagnostic() {
        let code = r#"&AtClient
Procedure DoSomething()
    Handler = New NotifyDescription("AfterSelection", ThisObject);
EndProcedure

&AtClient
Procedure AfterSelection(SelectedItem, AdditionalParameters) Export
    If SelectedItem <> Undefined Then
        Call(SelectedItem.Value);
    EndIf;
EndProcedure
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_notify_description_cross_module_still_flags() {
        let code = r#"&НаКлиенте
Процедура Вызов()
    Описание = Новый ОписаниеОповещения("Callback", КлиентскийМодуль);
КонецПроцедуры

&НаКлиенте
Процедура Callback(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 7:31..7:43
              message: Уберите неиспользуемый параметр "ДопПараметры"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_notify_description_variable_arg_still_flags() {
        let code = r#"&НаКлиенте
Процедура Вызов()
    ИмяМетода = "Callback";
    Описание = Новый ОписаниеОповещения(ИмяМетода, ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура Callback(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedParameters,
            expect![[r#"
            UnusedParameters @ 8:31..8:43
              message: Уберите неиспользуемый параметр "ДопПараметры"
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_notify_description_dynamic_constructor() {
        let code = r#"&НаКлиенте
Процедура Вызов()
    Описание = Новый("ОписаниеОповещения", "ПослеВыбора", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ПослеВыбора(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_notify_description_in_module_level_code() {
        let code = r#"Описание = Новый ОписаниеОповещения("ОбработчикЗавершения", ЭтотОбъект);

&НаКлиенте
Процедура ОбработчикЗавершения(Результат, ДопПараметры) Экспорт
    Вызов(Результат);
КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedParameters, expect![[r#""#]]);
    }

    #[test]
    fn test_custom_attachable_prefixes() {
        let code = r#"Процедура Обр_СобытиеФормы(Параметр1, Параметр2) Экспорт
    Вызов(Параметр1);
КонецПроцедуры
"#;

        let mut config = crate::DiagnosticsConfig::default();
        let mut params = serde_json::Map::new();
        params.insert(
            "attachableMethodPrefixes".to_string(),
            serde_json::Value::String("обр_,handler_".to_string()),
        );
        config
            .parameters
            .insert(DiagnosticCode::UnusedParameters, serde_json::Value::Object(params));

        let diagnostics =
            crate::test_utils::check_hir_diagnostic_with_config(code, config, crate::diagnostics);
        let unused: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedParameters).collect();

        assert_eq!(unused.len(), 0);
    }

    /// An exported form-module method is called by code the form cannot see (often
    /// an external processor), so its signature is the caller's; the same body in a
    /// non-exported method still reports.
    #[test]
    fn exported_form_module_method_keeps_its_contract_parameters() {
        use crate::test_utils::check_hir_diagnostic_with_fixtures;
        let fixture = r#"//- /Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl
Функция ВнешнийОбработчик(Первый, Второй, Третий) Экспорт
    Возврат Ложь;
КонецФункции

Функция ЛокальнаяФункция(Первый, Второй, Третий)
    Возврат Ложь;
КонецФункции

Процедура ПриОткрытии()
    ЛокальнаяФункция(1, 2, 3);
КонецПроцедуры
"#;
        let messages: Vec<String> = check_hir_diagnostic_with_fixtures(fixture)
            .into_iter()
            .filter(|d| d.code == crate::DiagnosticCode::UnusedParameters)
            .map(|d| d.message)
            .collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
    }
}
