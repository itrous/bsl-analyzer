pub mod capability;
mod db;
pub mod deprecation;
mod global_catalog;
pub mod help;
mod parameter_series;
pub mod security;
mod snapshot;
pub mod standard_mdo_attributes;
mod types;

mod overlays;

#[allow(warnings)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/generated.rs"));

    use super::types::*;
}

pub use standard_mdo_attributes::{
    standard_attributes_for, AttrValueKind, MdoTemplateKind, ObjectView, PresenceCondition,
    StandardAttrSpec, StandardKind,
};

pub use db::{
    find_prefixed_method, find_prefixed_methods, global_function_query, global_member_method_query,
    global_property_query, manager_methods_query, method_display_names,
    platform_constructors_query, platform_method_query, platform_property_query,
    platform_type_query, prefixed_method_query, type_methods_query, type_properties_query,
    MethodLookupInput, PlatformData, PlatformDataInner, PrefixedMethodLookupInput, TypeNameInput,
    GLOBAL_CONTEXT_OWNER, LEGACY_GLOBAL_FUNCTION_EN_ALIASES,
};
pub use global_catalog::{
    PlatformCatalogStatus, PlatformGlobalCatalog, PlatformGlobalCatalogMetadata,
    PlatformGlobalKind, PlatformGlobalSymbol, PlatformSymbolCapabilities, PlatformVersion,
    PlatformVersionParseError,
};
pub use help::{
    active_platform_help_request, install_platform_help, InstallOutcome, PlatformHelp,
    PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpSourceKind, PlatformHelpStatus,
    RestartRequired, BUNDLED_PLATFORM_VERSION, CORPUS_ENV,
};
pub use parameter_series::ParameterSeries;
pub use snapshot::{PlatformSnapshot, SnapshotDecodeError};
pub use types::*;

pub fn split_type_alternatives(raw: &str) -> Vec<&str> {
    raw.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_data_loads() {
        let data = PlatformData::instance();

        let types = data.all_types();
        let methods = data.all_methods();

        println!("Loaded {} types and {} methods", types.len(), methods.len());
    }

    #[test]
    fn split_type_alternatives_handles_comma_semicolon_and_trailing_garbage() {
        assert_eq!(split_type_alternatives("Число"), vec!["Число"]);
        assert_eq!(split_type_alternatives("Число, Строка"), vec!["Число", "Строка"]);
        assert_eq!(split_type_alternatives("Форма ; Элемент"), vec!["Форма", "Элемент"]);
        assert_eq!(split_type_alternatives("Метаданные, Массив ;"), vec!["Метаданные", "Массив"],);
        assert!(split_type_alternatives(", ,").is_empty());
        assert!(split_type_alternatives("").is_empty());
    }

    #[test]
    fn test_bilingual_lookup() {
        let data = PlatformData::instance();

        if data.all_types().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        for ty in data.all_types() {
            assert!(
                data.get_type(&ty.name).is_some(),
                "Failed to find type by Russian name: {}",
                ty.name
            );
            assert!(
                data.get_type(&ty.english_name).is_some(),
                "Failed to find type by English name: {}",
                ty.english_name
            );
        }
    }
}
