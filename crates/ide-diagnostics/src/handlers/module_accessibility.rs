use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::execution_env::EnvFlags;
use hir::LocalRange;
use hir::{EnvCalleeKind, Name};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

pub fn from_hir(
    name: &Name,
    callee_kind: EnvCalleeKind,
    missing: EnvFlags,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let kind_ru = match callee_kind {
        EnvCalleeKind::CommonModule => "Модуль",
        EnvCalleeKind::LocalMethod => "Метод",
    };
    let envs: Vec<&str> = missing.iter().map(|flag| flag.name_ru()).collect();
    let message = format!("{} '{}' недоступен [{}]", kind_ru, name.as_str(), envs.join(", "));
    crate::simple_hir_diagnostic(DiagnosticCode::ModuleAccessibility, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_form_with_common_modules, check_hir_diagnostic_with_fixtures,
        PREPROC_CROSSED_ELSE_IF, PREPROC_TORN_CONDITION,
    };
    use crate::DiagnosticCode;

    fn access_diags(fixture: &str) -> Vec<String> {
        check_hir_diagnostic_with_fixtures(fixture)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::ModuleAccessibility)
            .map(|d| d.message)
            .collect()
    }

    fn form_access_diags(form_source: &str, modules: &[(&str, &str, &str)]) -> Vec<String> {
        check_form_with_common_modules(form_source, modules)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::ModuleAccessibility)
            .map(|d| d.message)
            .collect()
    }

    const EXPORT_WRITE: &str = "Процедура Записать() Экспорт\nКонецПроцедуры\n";
    const EXPORT_SHOW: &str = "Процедура Показать() Экспорт\nКонецПроцедуры\n";

    const SERVER_MODULE_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000101">
        <Properties>
            <Name>Серверный</Name>
            <Server>true</Server>
            <ClientManagedApplication>false</ClientManagedApplication>
            <ServerCall>false</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    const SERVER_CALL_MODULE_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000102">
        <Properties>
            <Name>СерверныйВызовСервера</Name>
            <Server>true</Server>
            <ClientManagedApplication>false</ClientManagedApplication>
            <ServerCall>true</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    const CLIENT_MODULE_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000103">
        <Properties>
            <Name>Клиентский</Name>
            <Server>false</Server>
            <ClientManagedApplication>true</ClientManagedApplication>
            <ServerCall>false</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    #[test]
    fn server_module_called_from_client_form_method() {
        let diags = form_access_diags(
            "&НаКлиенте\nПроцедура Сохранить()\n    Серверный.Записать();\nКонецПроцедуры\n",
            &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
        );
        assert_eq!(diags.len(), 1, "server module has no client contexts, got: {diags:?}");
        assert!(
            diags[0].starts_with("Модуль 'Серверный' недоступен")
                && diags[0].contains("Тонкий клиент")
                && diags[0].contains("Веб-клиент"),
            "message must name the module and the client environments: {}",
            diags[0]
        );
        let qualifier = diags[0].split('[').nth(1).unwrap_or("");
        assert!(
            !qualifier.contains("Сервер"),
            "the server side is fine and must not be reported: {}",
            diags[0]
        );
    }

    #[test]
    fn server_call_module_reachable_from_client() {
        let diags = form_access_diags(
            "&НаКлиенте\nПроцедура Сохранить()\n    СерверныйВызовСервера.Записать();\nКонецПроцедуры\n",
            &[("СерверныйВызовСервера", EXPORT_WRITE, SERVER_CALL_MODULE_XML)],
        );
        assert!(diags.is_empty(), "ВызовСервера is the client's remote call path, got: {diags:?}");
    }

    #[test]
    fn client_module_called_from_server_method() {
        let diags = form_access_diags(
            "&НаСервере\nПроцедура Обработать()\n    Клиентский.Показать();\nКонецПроцедуры\n",
            &[("Клиентский", EXPORT_SHOW, CLIENT_MODULE_XML)],
        );
        assert_eq!(diags.len(), 1, "client module does not exist on the server, got: {diags:?}");
        assert!(
            diags[0].starts_with("Модуль 'Клиентский' недоступен") && diags[0].contains("Сервер"),
            "message must report the server side: {}",
            diags[0]
        );
    }

    /// A module-level region around whole methods; `preceding` is a method
    /// inside it whose own directive the parser does not read as a plain block.
    fn region_source(region_condition: &str, preceding: &str, caller_inside: bool) -> String {
        let caller = "&НаКлиентеНаСервереБезКонтекста
Процедура Сохранить()
    Серверный.Записать();
КонецПроцедуры
";
        let (inside, outside) = if caller_inside { (caller, "") } else { ("", caller) };
        format!(
            "#Если {region_condition} Тогда

{preceding}
{inside}
#КонецЕсли

{outside}"
        )
    }

    #[test]
    fn module_region_narrows_caller_after_torn_condition() {
        let diags = form_access_diags(
            &region_source("Сервер", PREPROC_TORN_CONDITION, true),
            &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
        );
        assert!(diags.is_empty(), "the region leaves only the server, got: {diags:?}");
    }

    #[test]
    fn module_region_narrows_caller_after_crossed_else_if() {
        let diags = form_access_diags(
            &region_source("Сервер", PREPROC_CROSSED_ELSE_IF, true),
            &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
        );
        assert!(diags.is_empty(), "the region leaves only the server, got: {diags:?}");
    }

    #[test]
    fn caller_after_module_region_is_not_narrowed() {
        for preceding in [PREPROC_TORN_CONDITION, PREPROC_CROSSED_ELSE_IF] {
            let diags = form_access_diags(
                &region_source("Сервер", preceding, false),
                &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
            );
            assert_eq!(diags.len(), 1, "outside the region the clients remain, got: {diags:?}");
        }
    }

    #[test]
    fn module_region_keeping_clients_still_checks_them() {
        for preceding in [PREPROC_TORN_CONDITION, PREPROC_CROSSED_ELSE_IF] {
            let diags = form_access_diags(
                &region_source("Не МобильныйКлиент", preceding, true),
                &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
            );
            assert_eq!(diags.len(), 1, "the region keeps the clients, got: {diags:?}");
        }
    }

    #[test]
    fn server_module_called_from_server_method_is_fine() {
        let diags = form_access_diags(
            "&НаСервере\nПроцедура Обработать()\n    Серверный.Записать();\nКонецПроцедуры\n",
            &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
        );
        assert!(diags.is_empty(), "server-to-server call is legal, got: {diags:?}");
    }

    #[test]
    fn preprocessor_guard_narrows_caller_before_module_check() {
        let diags = form_access_diags(
            "&НаКлиентеНаСервере\nПроцедура Сохранить()\n    #Если Сервер Тогда\n    Серверный.Записать();\n    #КонецЕсли\nКонецПроцедуры\n",
            &[("Серверный", EXPORT_WRITE, SERVER_MODULE_XML)],
        );
        assert!(diags.is_empty(), "the branch narrows the caller to the server, got: {diags:?}");
    }

    #[test]
    fn server_form_method_calling_client_method_flagged() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Показать()
КонецПроцедуры

&НаСервере
Процедура Обработать()
    Показать();
КонецПроцедуры
"#;
        let diags = access_diags(fixture);
        assert_eq!(diags.len(), 1, "server code cannot reach a client-only method, got: {diags:?}");
        assert!(
            diags[0].starts_with("Метод 'Показать' недоступен") && diags[0].contains("Сервер"),
            "message must report the server side: {}",
            diags[0]
        );
    }

    #[test]
    fn client_form_method_calling_server_method_is_remote_call() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Записать()
КонецПроцедуры

&НаКлиенте
Процедура Сохранить()
    Записать();
КонецПроцедуры
"#;
        let diags = access_diags(fixture);
        assert!(diags.is_empty(), "client-to-server is the form's remote call, got: {diags:?}");
    }

    #[test]
    fn server_form_method_calling_client_method_through_this_object_flagged() {
        let diags = form_access_diags(
            "&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаСервере\nПроцедура Обработать()\n    ЭтотОбъект.Показать();\nКонецПроцедуры\n",
            &[],
        );
        assert_eq!(
            diags.len(),
            1,
            "a self-qualified call is the same call as the bare one, got: {diags:?}"
        );
        assert!(
            diags[0].starts_with("Метод 'Показать' недоступен") && diags[0].contains("Сервер"),
            "message must report the server side: {}",
            diags[0]
        );
    }

    #[test]
    fn this_form_alias_judged_like_this_object() {
        let diags = form_access_diags(
            "&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаСервере\nПроцедура Обработать()\n    ЭтаФорма.Показать();\nКонецПроцедуры\n",
            &[],
        );
        assert_eq!(
            diags.len(),
            1,
            "the deprecated self spelling is still the form itself, got: {diags:?}"
        );
    }

    #[test]
    fn client_form_method_calling_server_method_through_this_object_is_remote_call() {
        let diags = form_access_diags(
            "&НаСервере\nПроцедура Записать()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Сохранить()\n    ЭтотОбъект.Записать();\nКонецПроцедуры\n",
            &[],
        );
        assert!(diags.is_empty(), "client-to-server is the form's remote call, got: {diags:?}");
    }

    #[test]
    fn client_at_server_no_context_caller_flagged_for_server_half() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Показать()
КонецПроцедуры

&НаКлиентеНаСервереБезКонтекста
Процедура Обработать()
    Показать();
КонецПроцедуры
"#;
        let diags = access_diags(fixture);
        assert_eq!(
            diags.len(),
            1,
            "the server half cannot reach the client method, got: {diags:?}"
        );
        assert!(diags[0].contains("Сервер"), "only the server side violates: {}", diags[0]);
    }

    #[test]
    fn module_variable_named_this_object_is_not_form_self() {
        let diags = form_access_diags(
            "Перем ЭтотОбъект;\n\n&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаСервере\nПроцедура Обработать()\n    ЭтотОбъект.Показать();\nКонецПроцедуры\n",
            &[],
        );
        assert!(
            diags.is_empty(),
            "a module variable only borrows the spelling — it is not the form, got: {diags:?}"
        );
    }

    #[test]
    fn module_variable_named_this_form_is_not_form_self() {
        let diags = form_access_diags(
            "Перем ЭтаФорма;\n\n&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаСервере\nПроцедура Обработать()\n    ЭтаФорма.Показать();\nКонецПроцедуры\n",
            &[],
        );
        assert!(
            diags.is_empty(),
            "a module variable only borrows the spelling — it is not the form, got: {diags:?}"
        );
    }
}
