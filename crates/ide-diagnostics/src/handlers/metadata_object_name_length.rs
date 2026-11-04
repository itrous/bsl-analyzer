use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::traits::MdObject;
use hir::ModuleMetadata;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: true,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
    clean_code_attribute: CleanCodeAttribute::Consistent,
};

const DEFAULT_MAX_LENGTH: usize = 80;

pub fn from_metadata(metadata: &ModuleMetadata, ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::MetadataObjectNameLength;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let max_length = ctx
        .config
        .get_int(DiagnosticCode::MetadataObjectNameLength, "maxMetadataObjectNameLength")
        .unwrap_or(DEFAULT_MAX_LENGTH as i64) as usize;

    let mut diagnostics = Vec::new();

    if let Some(ref common_module) = metadata.common_module {
        check_common_module(common_module, max_length, code, ctx, &mut diagnostics);
    }

    if let Some(ref mdo) = metadata.mdo {
        check_metadata_object(mdo, max_length, code, ctx, &mut diagnostics);
    }

    if let Some(ref register) = metadata.register {
        check_register(register, max_length, code, ctx, &mut diagnostics);
    }

    diagnostics
}

pub fn check_session_module(
    configuration: &bsl_metadata::Configuration,
    ctx: &DiagnosticsContext,
) -> Vec<Diagnostic> {
    let code = DiagnosticCode::MetadataObjectNameLength;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let max_length = ctx
        .config
        .get_int(DiagnosticCode::MetadataObjectNameLength, "maxMetadataObjectNameLength")
        .unwrap_or(DEFAULT_MAX_LENGTH as i64) as usize;

    let mut diagnostics = Vec::new();

    for mdo in configuration.metadata_objects() {
        if !has_modules(&mdo.mdo_type) {
            let name_length = mdo.name.chars().count();
            if name_length > max_length {
                diagnostics.push(Diagnostic {
                    code,
                    message: format!(
                        "Rename the metadata object `{}` so that the name length is less than {}",
                        mdo.name, max_length
                    ),
                    severity: ctx.severity(code),
                    range: syntax::MODULE_RANGE,
                    tags: ctx.tags(code),
                    fixes: vec![],
                });
            }
        }
    }

    diagnostics
}

fn has_modules(mdo_type: &bsl_metadata::MdoType) -> bool {
    use bsl_metadata::MdoType;
    matches!(
        mdo_type,
        MdoType::Catalog
            | MdoType::Document
            | MdoType::BusinessProcess
            | MdoType::Task
            | MdoType::ChartOfAccounts
            | MdoType::ChartOfCalculationTypes
            | MdoType::ChartOfCharacteristicTypes
    )
}

fn check_common_module(
    module: &bsl_metadata::CommonModule,
    max_length: usize,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let name_length = module.name().chars().count();
    if name_length > max_length {
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Rename the metadata object `{}` so that the name length is less than {}",
                module.name(),
                max_length
            ),
            severity: ctx.severity(code),
            range: syntax::MODULE_RANGE,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn check_metadata_object(
    mdo: &bsl_metadata::MetadataObject,
    max_length: usize,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let name_length = mdo.name.chars().count();
    if name_length > max_length {
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Rename the metadata object `{}` so that the name length is less than {}",
                mdo.name, max_length
            ),
            severity: ctx.severity(code),
            range: syntax::MODULE_RANGE,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn check_register(
    register: &bsl_metadata::Register,
    max_length: usize,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let name_length = register.name().chars().count();
    if name_length > max_length {
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Rename the metadata object `{}` so that the name length is less than {}",
                register.name(),
                max_length
            ),
            severity: ctx.severity(code),
            range: syntax::MODULE_RANGE,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_metadata_diagnostic, check_metadata_diagnostic_with_config};
    use crate::DiagnosticsConfig;
    use std::sync::Arc;

    /// A metadata name of exactly `chars` characters.
    fn name_of(chars: usize) -> String {
        let stem = "ЖурналПриемкиТоваровНаРаспределительныйЦентр";
        let stem_chars = stem.chars().count();
        assert!(chars >= stem_chars);
        format!("{stem}{}", "Я".repeat(chars - stem_chars))
    }

    fn at_threshold() -> String {
        name_of(DEFAULT_MAX_LENGTH)
    }

    fn over_threshold() -> String {
        name_of(DEFAULT_MAX_LENGTH + 1)
    }

    fn expected_message(name: &str, max: usize) -> String {
        format!("Rename the metadata object `{name}` so that the name length is less than {max}")
    }

    fn base_metadata(module_type: bsl_metadata::ModuleType) -> ModuleMetadata {
        ModuleMetadata {
            module_type,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    fn common_module(name: &str) -> ModuleMetadata {
        let module = bsl_metadata::CommonModule::builder().name(name).build();
        ModuleMetadata {
            common_module: Some(Arc::new(module)),
            ..base_metadata(bsl_metadata::ModuleType::CommonModule)
        }
    }

    fn document(name: &str) -> ModuleMetadata {
        let mdo = bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Document, name);
        ModuleMetadata {
            mdo: Some(Arc::new(mdo)),
            ..base_metadata(bsl_metadata::ModuleType::ObjectModule)
        }
    }

    fn accumulation_register(name: &str) -> ModuleMetadata {
        let register = bsl_metadata::Register::builder()
            .name(name)
            .mdo_type(bsl_metadata::MdoType::AccumulationRegister)
            .build();
        ModuleMetadata {
            register: Some(Arc::new(register)),
            command: None,
            ..base_metadata(bsl_metadata::ModuleType::ManagerModule)
        }
    }

    fn messages(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics.iter().map(|d| d.message.clone()).collect()
    }

    #[test]
    fn test_names_are_built_to_length() {
        assert_eq!(at_threshold().chars().count(), 80);
        assert_eq!(over_threshold().chars().count(), 81);
    }

    #[test]
    fn test_common_module_short_name() {
        let diagnostics =
            check_metadata_diagnostic(common_module(&at_threshold()), "", from_metadata);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_common_module_long_name() {
        let name = over_threshold();
        let diagnostics = check_metadata_diagnostic(common_module(&name), "", from_metadata);
        assert_eq!(messages(&diagnostics), [expected_message(&name, 80)]);
        assert_eq!(diagnostics[0].range, syntax::MODULE_RANGE);
    }

    #[test]
    fn test_metadata_object_short_name() {
        let diagnostics = check_metadata_diagnostic(document(&at_threshold()), "", from_metadata);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_metadata_object_long_name() {
        let name = over_threshold();
        let diagnostics = check_metadata_diagnostic(document(&name), "", from_metadata);
        assert_eq!(messages(&diagnostics), [expected_message(&name, 80)]);
    }

    #[test]
    fn test_register_short_name() {
        let diagnostics =
            check_metadata_diagnostic(accumulation_register(&at_threshold()), "", from_metadata);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_register_long_name() {
        let name = over_threshold();
        let diagnostics =
            check_metadata_diagnostic(accumulation_register(&name), "", from_metadata);
        assert_eq!(messages(&diagnostics), [expected_message(&name, 80)]);
    }

    #[test]
    fn test_disabled_diagnostic() {
        let mut config = DiagnosticsConfig::default();
        config.disabled.push(DiagnosticCode::MetadataObjectNameLength);
        let diagnostics = check_metadata_diagnostic_with_config(
            common_module(&over_threshold()),
            "",
            config,
            from_metadata,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_custom_max_length() {
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MetadataObjectNameLength,
            serde_json::json!({"maxMetadataObjectNameLength": 7}),
        );
        let diagnostics = check_metadata_diagnostic_with_config(
            common_module("Приемка1"),
            "",
            config,
            from_metadata,
        );
        assert_eq!(messages(&diagnostics), [expected_message("Приемка1", 7)]);
    }

    #[test]
    fn test_has_modules_classification() {
        use bsl_metadata::MdoType;

        assert!(has_modules(&MdoType::Catalog));
        assert!(has_modules(&MdoType::Document));
        assert!(has_modules(&MdoType::BusinessProcess));
        assert!(has_modules(&MdoType::Task));
        assert!(has_modules(&MdoType::ChartOfAccounts));
        assert!(has_modules(&MdoType::ChartOfCharacteristicTypes));

        assert!(!has_modules(&MdoType::Constant));
        assert!(!has_modules(&MdoType::Enum));
        assert!(!has_modules(&MdoType::AccumulationRegister));
        assert!(!has_modules(&MdoType::InformationRegister));
    }

    #[test]
    fn test_session_module_checks_no_module_objects() {
        use ide_db::RootDatabaseImpl;
        let mut configuration = bsl_metadata::Configuration::new("Склад");

        let with_modules = over_threshold();
        configuration.add_metadata_object(bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Document,
            &with_modules,
        ));
        let without_modules = format!("{}Ц", at_threshold());
        configuration.add_metadata_object(bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Enum,
            &without_modules,
        ));
        configuration.add_metadata_object(bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Constant,
            at_threshold(),
        ));

        let db = RootDatabaseImpl::new();
        let config = DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(&db, None);
        let ctx = crate::DiagnosticsContext::new(&config, vfs::FileId(0), &provider);

        let diagnostics = check_session_module(&configuration, &ctx);
        assert_eq!(messages(&diagnostics), [expected_message(&without_modules, 80)]);
    }
}
