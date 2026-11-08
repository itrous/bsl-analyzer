use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::ModuleMetadata;
use ide_db::TextRange;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[
        bsl_metadata::ModuleType::ManagerModule,
        bsl_metadata::ModuleType::ObjectModule,
        bsl_metadata::ModuleType::SessionModule,
    ],
    minutes_to_fix: 30,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Design],
    can_locate_on_project: true,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
    clean_code_attribute: CleanCodeAttribute::Consistent,
};

const SUPPORTED_MODULE_TYPES: &[bsl_metadata::ModuleType] =
    &[bsl_metadata::ModuleType::ManagerModule, bsl_metadata::ModuleType::ObjectModule];

fn is_supported_module_type(module_type: bsl_metadata::ModuleType) -> bool {
    SUPPORTED_MODULE_TYPES.contains(&module_type)
}

pub fn from_metadata(metadata: &ModuleMetadata, ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::SameMetadataObjectAndChildNames;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    if !is_supported_module_type(metadata.module_type) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    if let Some(ref mdo) = metadata.mdo {
        collect_mdo_diagnostics(&mdo.name, mdo.as_ref(), code, ctx, &mut diagnostics);
    }

    if let Some(ref register) = metadata.register {
        collect_register_diagnostics(
            register.name(),
            register.as_ref(),
            code,
            ctx,
            &mut diagnostics,
        );
    }

    diagnostics
}

fn collect_mdo_diagnostics(
    parent_name: &str,
    mdo: &bsl_metadata::MetadataObject,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let parent_name_lower = parent_name.fold_lower();

    for attr in &mdo.attributes {
        if attr.name.fold_lower() == parent_name_lower {
            diagnostics.push(create_diagnostic(&attr.name, parent_name, code, ctx));
        }
    }

    for ts in &mdo.tabular_sections {
        let ts_name = ts.name();
        let ts_name_lower = ts_name.fold_lower();

        if ts_name_lower == parent_name_lower {
            diagnostics.push(create_diagnostic(ts_name, parent_name, code, ctx));
        }

        for ts_attr in ts.attributes() {
            if ts_attr.name().fold_lower() == ts_name_lower {
                diagnostics.push(create_diagnostic(ts_attr.name(), ts_name, code, ctx));
            }
        }
    }
}

fn collect_register_diagnostics(
    parent_name: &str,
    register: &bsl_metadata::Register,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let parent_name_lower = parent_name.fold_lower();

    for dim in register.dimensions() {
        if dim.name().fold_lower() == parent_name_lower {
            diagnostics.push(create_diagnostic(dim.name(), parent_name, code, ctx));
        }
    }

    for resource in register.resources() {
        if resource.name().fold_lower() == parent_name_lower {
            diagnostics.push(create_diagnostic(resource.name(), parent_name, code, ctx));
        }
    }

    for attr in register.attributes() {
        if attr.name().fold_lower() == parent_name_lower {
            diagnostics.push(create_diagnostic(attr.name(), parent_name, code, ctx));
        }
    }
}

fn create_diagnostic(
    child_name: &str,
    parent_name: &str,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Diagnostic {
    Diagnostic {
        code,
        message: format!(
            "Измените имя '{}', чтобы оно не совпадало с родительским '{}'",
            child_name, parent_name
        ),
        range: TextRange::default(),
        severity: ctx.severity(code),
        tags: ctx.tags(code),
        fixes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uuid::Uuid;
    fn make_metadata_with_mdo(mdo: bsl_metadata::MetadataObject) -> ModuleMetadata {
        ModuleMetadata {
            module_type: bsl_metadata::ModuleType::ManagerModule,
            execution_context: None,
            common_module: None,
            mdo: Some(Arc::new(mdo)),
            register: None,
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    fn make_metadata_with_register(register: bsl_metadata::Register) -> ModuleMetadata {
        ModuleMetadata {
            module_type: bsl_metadata::ModuleType::ManagerModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: Some(Arc::new(register)),
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    #[test]
    fn test_catalog_with_matching_attribute() {
        let mut catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");
        catalog.add_attribute(bsl_metadata::Attribute {
            name: "Номенклатура".to_string(),
            name_en: None,
            attr_type: bsl_metadata::AttributeType::String { length: Some(100) },
        });

        let metadata = make_metadata_with_mdo(catalog);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Номенклатура"));
    }

    #[test]
    fn test_catalog_with_non_matching_attribute() {
        let mut catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");
        catalog.add_attribute(bsl_metadata::Attribute {
            name: "Наименование".to_string(),
            name_en: None,
            attr_type: bsl_metadata::AttributeType::String { length: Some(100) },
        });

        let metadata = make_metadata_with_mdo(catalog);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_catalog_with_matching_tabular_section() {
        let mut catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");

        let ts = bsl_metadata::TabularSection::new(Uuid::new_v4(), "Номенклатура");
        catalog.add_tabular_section(ts);

        let metadata = make_metadata_with_mdo(catalog);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Номенклатура"));
    }

    #[test]
    fn test_tabular_section_with_matching_attribute() {
        let mut catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");

        let mut ts = bsl_metadata::TabularSection::new(Uuid::new_v4(), "Штрихкоды");
        ts.set_attributes(vec![bsl_metadata::TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Штрихкоды",
            bsl_metadata::AttributeType::String { length: Some(13) },
        )]);
        catalog.add_tabular_section(ts);

        let metadata = make_metadata_with_mdo(catalog);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Штрихкоды"));
    }

    #[test]
    fn test_register_with_matching_dimension() {
        let register = bsl_metadata::Register::builder()
            .name("Товары")
            .mdo_type(bsl_metadata::MdoType::InformationRegister)
            .add_dimension(bsl_metadata::Dimension::builder().name("Товары").build())
            .build();

        let metadata = make_metadata_with_register(register);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Товары"));
    }

    #[test]
    fn test_register_with_matching_resource() {
        let register = bsl_metadata::Register::builder()
            .name("Остатки")
            .mdo_type(bsl_metadata::MdoType::AccumulationRegister)
            .add_resource(bsl_metadata::RegisterResource::new(Uuid::new_v4(), "Остатки"))
            .build();

        let metadata = make_metadata_with_register(register);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Остатки"));
    }

    #[test]
    fn test_register_with_non_matching_children() {
        let register = bsl_metadata::Register::builder()
            .name("РегистрСведений")
            .mdo_type(bsl_metadata::MdoType::InformationRegister)
            .add_dimension(bsl_metadata::Dimension::builder().name("Период").build())
            .add_attribute(bsl_metadata::RegisterAttribute::new(Uuid::new_v4(), "Значение"))
            .build();

        let metadata = make_metadata_with_register(register);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert!(diagnostics.is_empty());
    }

    #[test]
    fn test_case_insensitive_matching() {
        let mut catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");
        catalog.add_attribute(bsl_metadata::Attribute {
            name: "НОМЕНКЛАТУРА".to_string(),
            name_en: None,
            attr_type: bsl_metadata::AttributeType::String { length: Some(100) },
        });

        let metadata = make_metadata_with_mdo(catalog);
        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn test_unsupported_module_type() {
        let catalog =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Номенклатура");

        let metadata = ModuleMetadata {
            module_type: bsl_metadata::ModuleType::CommonModule,
            execution_context: None,
            common_module: None,
            mdo: Some(Arc::new(catalog)),
            register: None,
            command: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        };

        let diagnostics = crate::test_utils::check_metadata_diagnostic(metadata, "", from_metadata);

        assert!(diagnostics.is_empty());
    }
}
