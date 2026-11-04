use crate::common_module_helpers;
use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode};
use hir::ModuleMetadata;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::CommonModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Unpredictable, MetadataTag::Design],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

pub fn from_metadata(
    metadata: &ModuleMetadata,
    ctx: &crate::DiagnosticsContext,
) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CommonModuleInvalidType;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    if !matches!(metadata.module_type, bsl_metadata::ModuleType::CommonModule) {
        return Vec::new();
    }

    let module = match &metadata.common_module {
        Some(m) => m.as_ref(),
        None => return Vec::new(),
    };

    // std469 templates, with two concessions taken from the standard itself:
    //   * Клиентский (п. 2.3): a client module may target only one of the two clients
    //     (only managed or only ordinary application), so any non-empty client pair
    //     without server-side flags is valid;
    //   * Серверный / Клиент-серверный (п. 2.1): the standard asks for the extra flags
    //     "для корректной работы в режимах внешнего соединения, управляемого и обычного
    //     приложений", i.e. conditionally. `Клиент (обычное приложение)` exists only for
    //     the ordinary application, so it is required only when the configuration
    //     supports it; `ВнешнееСоединение` is optional - a module that is only `Сервер`
    //     is an ordinary server module of a working configuration.
    // Серверный для вызова с клиента stays exact: `ВызовСервера` is valid only with
    // `Сервер` and no other context (std469 п. 2.2, std679); a module with no context
    // is a deviation too.
    // `Глобальный`, `Привилегированный` and `ПовторноеИспользование` do not matter.
    let ordinary = ctx.config.ordinary_app_support;
    let is_client = !module.is_server_call()
        && !module.is_server()
        && !module.is_external_connection()
        && (module.is_client_ordinary_application() || module.is_client_managed_application());

    let is_valid = is_server_for_type_check(module, ordinary)
        || common_module_helpers::is_server_call(module)
        || is_client
        || is_client_server_for_type_check(module, ordinary);

    if is_valid {
        return Vec::new();
    }

    vec![Diagnostic {
        code,
        message:
            "Общий модуль должен быть одного из типов: Server, ServerCall, Client, ClientServer"
                .to_string(),
        severity: ctx.severity(code),
        range: syntax::MODULE_RANGE,
        tags: ctx.tags(code),
        fixes: vec![],
    }]
}

// Not the shared `common_module_helpers::is_server` / `is_client_server`: both still
// require `ВнешнееСоединение`. `is_client_server` is the predicate of the client-server
// naming check, so loosening it would change that rule. No naming check selects
// the server template. Here the flag is optional for the type check only.
fn is_server_for_type_check(module: &bsl_metadata::CommonModule, ordinary: bool) -> bool {
    !module.is_server_call()
        && module.is_server()
        && (module.is_client_ordinary_application() || !ordinary)
        && !module.is_client_managed_application()
}

fn is_client_server_for_type_check(module: &bsl_metadata::CommonModule, ordinary: bool) -> bool {
    !module.is_server_call()
        && module.is_server()
        && (module.is_client_ordinary_application() || !ordinary)
        && module.is_client_managed_application()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiagnosticsConfig;
    use bsl_metadata::ReturnValueReuse;

    fn metadata_for(module: Option<bsl_metadata::CommonModule>) -> hir::ModuleMetadata {
        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::CommonModule,
            execution_context: None,
            common_module: module.map(std::sync::Arc::new),
            mdo: None,
            register: None,
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    fn check(module: bsl_metadata::CommonModule, ordinary: bool) -> Vec<Diagnostic> {
        let mut config = DiagnosticsConfig::all_enabled();
        config.ordinary_app_support = ordinary;
        crate::test_utils::check_metadata_diagnostic_with_config(
            metadata_for(Some(module)),
            "",
            config,
            from_metadata,
        )
    }

    fn fires(module: bsl_metadata::CommonModule, ordinary: bool) -> bool {
        !check(module, ordinary).is_empty()
    }

    fn named(name: &str) -> bsl_metadata::CommonModuleBuilder {
        bsl_metadata::CommonModule::builder().name(name)
    }

    /// `bits` lists ВызовСервера, Сервер, ВнешнееСоединение, Клиент (обычное),
    /// Клиент (управляемое) in this order, '1' = set.
    fn module_from_bits(bits: &str) -> bsl_metadata::CommonModule {
        let b: Vec<bool> = bits.chars().map(|c| c == '1').collect();
        named("ОбщийМодуль1")
            .server_call(b[0])
            .server(b[1])
            .external_connection(b[2])
            .client_ordinary_application(b[3])
            .client_managed_application(b[4])
            .build()
    }

    fn assert_valid_set(ordinary: bool, valid: &[&str]) {
        for n in 0u8..32 {
            let bits: String =
                (0..5).map(|i| if n & (1 << (4 - i)) != 0 { '1' } else { '0' }).collect();
            let expected_valid = valid.contains(&bits.as_str());
            assert_eq!(
                fires(module_from_bits(&bits), ordinary),
                !expected_valid,
                "sc,s,ec,coa,cma = {bits}, ordinaryAppSupport = {ordinary}"
            );
        }
    }

    #[test]
    fn exhaustive_flag_table_without_ordinary_app_support() {
        assert_valid_set(
            false,
            &[
                // Серверный: ВнешнееСоединение and Клиент (обычное) optional.
                "01000", "01010", "01100", "01110", //
                // Серверный для вызова с клиента.
                "11000", //
                // Клиентский: any non-empty client pair.
                "00010", "00001", "00011", //
                // Клиент-серверный: ВнешнееСоединение and Клиент (обычное) optional.
                "01001", "01011", "01101", "01111",
            ],
        );
    }

    #[test]
    fn exhaustive_flag_table_with_ordinary_app_support() {
        assert_valid_set(
            true,
            &[
                // Серверный: ВнешнееСоединение optional, Клиент (обычное) required.
                "01010", "01110", //
                // Серверный для вызова с клиента.
                "11000", //
                // Клиентский: any non-empty client pair.
                "00010", "00001", "00011", //
                // Клиент-серверный.
                "01011", "01111",
            ],
        );
    }

    #[test]
    fn module_without_context_fires_with_message() {
        let diagnostics = check(named("ОбщийМодуль1").build(), false);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagnosticCode::CommonModuleInvalidType);
        assert_eq!(
            diagnostics[0].message,
            "Общий модуль должен быть одного из типов: Server, ServerCall, Client, ClientServer"
        );
    }

    #[test]
    fn single_client_modules_are_valid_in_both_modes() {
        // std469 п. 2.3: a client module may target only one of the two clients.
        for ordinary in [false, true] {
            let managed_only = named("ОбщийМодуль1Клиент").client_managed_application(true).build();
            assert!(!fires(managed_only, ordinary));
            let ordinary_only =
                named("ОбщийМодуль2Клиент").client_ordinary_application(true).build();
            assert!(!fires(ordinary_only, ordinary));
            let both = named("ОбщийМодуль3Клиент")
                .client_ordinary_application(true)
                .client_managed_application(true)
                .build();
            assert!(!fires(both, ordinary));
        }
    }

    #[test]
    fn ordinary_client_flag_is_required_only_with_ordinary_app_support() {
        let server = || named("ОбщийМодуль4").server(true).external_connection(true).build();
        assert!(!fires(server(), false));
        assert!(fires(server(), true));

        let client_server = || {
            named("ОбщийМодуль5КлиентСервер")
                .server(true)
                .external_connection(true)
                .client_managed_application(true)
                .build()
        };
        assert!(!fires(client_server(), false));
        assert!(fires(client_server(), true));

        for ordinary in [false, true] {
            let server_with_flag = named("ОбщийМодуль6")
                .server(true)
                .external_connection(true)
                .client_ordinary_application(true)
                .build();
            assert!(!fires(server_with_flag, ordinary));
            let client_server_with_flag = named("ОбщийМодуль7КлиентСервер")
                .server(true)
                .external_connection(true)
                .client_ordinary_application(true)
                .client_managed_application(true)
                .build();
            assert!(!fires(client_server_with_flag, ordinary));
        }
    }

    #[test]
    fn server_module_without_external_connection_is_valid() {
        // Only `Сервер`: valid without ordinaryAppSupport; with it the rule still
        // requires `Клиент (обычное приложение)`, so the bare module fires.
        let bare = || named("ОбщийМодуль24").server(true).build();
        assert!(!fires(bare(), false));
        assert!(fires(bare(), true));
        // With `Клиент (обычное приложение)` it is valid in both modes.
        for ordinary in [false, true] {
            let with_ordinary =
                named("ОбщийМодуль25").server(true).client_ordinary_application(true).build();
            assert!(!fires(with_ordinary, ordinary));
            let client_server = named("ОбщийМодуль26КлиентСервер")
                .server(true)
                .client_ordinary_application(true)
                .client_managed_application(true)
                .build();
            assert!(!fires(client_server, ordinary));
        }
    }

    #[test]
    fn server_call_template_is_valid() {
        for ordinary in [false, true] {
            let module = named("ОбщийМодуль8ВызовСервера").server_call(true).server(true).build();
            assert!(!fires(module, ordinary));
        }
    }

    #[test]
    fn server_call_mixes_fire() {
        for ordinary in [false, true] {
            // Серверный + ВызовСервера.
            let server_plus_call = named("ОбщийМодуль9")
                .server_call(true)
                .server(true)
                .external_connection(true)
                .client_ordinary_application(true)
                .build();
            assert!(fires(server_plus_call, ordinary));
            let without_external = named("ОбщийМодуль10")
                .server_call(true)
                .server(true)
                .client_ordinary_application(true)
                .build();
            assert!(fires(without_external, ordinary));
            let with_external = named("ОбщийМодуль11")
                .server_call(true)
                .server(true)
                .external_connection(true)
                .build();
            assert!(fires(with_external, ordinary));
            // ВызовСервера + every context: the Designer itself never produces it.
            let everything = named("ОбщийМодуль12")
                .server_call(true)
                .server(true)
                .external_connection(true)
                .client_ordinary_application(true)
                .client_managed_application(true)
                .build();
            assert!(fires(everything, ordinary));
            let call_with_client =
                named("ОбщийМодуль13").server_call(true).client_managed_application(true).build();
            assert!(fires(call_with_client, ordinary));
            assert!(fires(named("ОбщийМодуль14").server_call(true).build(), ordinary));
        }
    }

    #[test]
    fn incomplete_server_contexts_fire() {
        for ordinary in [false, true] {
            assert!(fires(named("ОбщийМодуль15").build(), ordinary));
            assert!(fires(named("ОбщийМодуль17").external_connection(true).build(), ordinary));
            // ВызовСервера mixes with a context-less or external-only module stay errors.
            let call_external =
                named("ОбщийМодуль18").server_call(true).external_connection(true).build();
            assert!(fires(call_external, ordinary));
        }
    }

    #[test]
    fn overlay_flags_do_not_affect_validity() {
        for ordinary in [false, true] {
            let global_client = named("ОбщийМодуль19Глобальный")
                .global(true)
                .client_managed_application(true)
                .build();
            assert!(!fires(global_client, ordinary));
            let privileged_server = named("ОбщийМодуль20Полный")
                .privileged(true)
                .server(true)
                .external_connection(true)
                .client_ordinary_application(true)
                .build();
            assert!(!fires(privileged_server, ordinary));
            let cached_client = named("ОбщийМодуль21КлиентПовтИсп")
                .return_values_reuse(ReturnValueReuse::DuringSession)
                .client_ordinary_application(true)
                .build();
            assert!(!fires(cached_client, ordinary));
            let cached_mix = named("ОбщийМодуль22ПовтИсп")
                .return_values_reuse(ReturnValueReuse::DuringRequest)
                .server_call(true)
                .server(true)
                .client_ordinary_application(true)
                .build();
            assert!(fires(cached_mix, ordinary));
            assert!(fires(named("ОбщийМодуль23Глобальный").global(true).build(), ordinary));
        }
    }

    #[test]
    fn test_from_metadata_non_common_module() {
        let mut metadata = metadata_for(None);
        metadata.module_type = bsl_metadata::ModuleType::ManagerModule;
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);
        assert_eq!(diagnostics.len(), 0);
    }
}
