use crate::completion::sdbl::domain::{MetadataProvider, SdblCompletionItem};
use crate::completion::CompletionItemKind;
use bsl_metadata::MdoType;
use stdx::case::CaseExt;

pub struct CompleteNestedElementsUseCase;

impl CompleteNestedElementsUseCase {
    pub fn execute(
        metadata_provider: &impl MetadataProvider,
        mdo_type: MdoType,
        object_name: &str,
        prefix: &str,
    ) -> Vec<SdblCompletionItem> {
        tracing::info!(
            ?mdo_type,
            object_name = %object_name,
            prefix = %prefix,
            "CompleteNestedElementsUseCase: executing"
        );

        match mdo_type {
            MdoType::InformationRegister
            | MdoType::AccumulationRegister
            | MdoType::AccountingRegister
            | MdoType::CalculationRegister => {
                Self::complete_virtual_tables(metadata_provider, mdo_type, object_name, prefix)
            }
            _ => Self::complete_tabular_sections(metadata_provider, mdo_type, object_name, prefix),
        }
    }

    fn complete_tabular_sections(
        metadata_provider: &impl MetadataProvider,
        mdo_type: MdoType,
        object_name: &str,
        prefix: &str,
    ) -> Vec<SdblCompletionItem> {
        let Some(object) = metadata_provider.resolve_metadata_object(mdo_type, object_name) else {
            tracing::debug!(
                ?mdo_type,
                object_name = %object_name,
                "metadata object not found"
            );
            return Vec::new();
        };

        let prefix_lower = prefix.fold_lower();

        let items: Vec<SdblCompletionItem> = object
            .tabular_sections
            .iter()
            .filter(|ts| ts.name().fold_lower().starts_with(&prefix_lower))
            .map(|ts| {
                let detail = format!("{}.{}.{}", mdo_type.russian_name(), object_name, ts.name());
                let documentation =
                    ts.synonym().map(|s| s.to_string()).unwrap_or_else(|| ts.name().to_string());

                SdblCompletionItem::new(ts.name(), CompletionItemKind::Field)
                    .with_detail(detail)
                    .with_documentation(documentation)
            })
            .collect();

        tracing::debug!(
            count = items.len(),
            total = object.tabular_sections.len(),
            ?mdo_type,
            object_name = %object_name,
            "generated tabular section completions"
        );

        items
    }

    fn complete_virtual_tables(
        metadata_provider: &impl MetadataProvider,
        mdo_type: MdoType,
        register_name: &str,
        prefix: &str,
    ) -> Vec<SdblCompletionItem> {
        // `resolve_register` filters by kind, so a name that exists under a
        // different register kind resolves to `None` here (the old explicit
        // `mdo_type` mismatch guard is subsumed).
        let Some(register) = metadata_provider.resolve_register(mdo_type, register_name) else {
            tracing::debug!(
                ?mdo_type,
                register_name = %register_name,
                "register not found"
            );
            return Vec::new();
        };

        let virtual_tables = register.virtual_tables();

        let prefix_lower = prefix.fold_lower();

        let items: Vec<SdblCompletionItem> = virtual_tables
            .into_iter()
            .filter(|vt_name| vt_name.fold_lower().starts_with(&prefix_lower))
            .map(|vt_name| {
                let detail = format!("{}.{}.{}", mdo_type.russian_name(), register_name, vt_name);

                SdblCompletionItem::new(vt_name, CompletionItemKind::Field).with_detail(detail)
            })
            .collect();

        tracing::debug!(
            count = items.len(),
            ?mdo_type,
            register_name = %register_name,
            "generated virtual table completions"
        );

        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::{Configuration, MetadataObject, TabularSection};
    use std::sync::Arc;
    use uuid::Uuid;

    struct MockMetadataProvider {
        config: Arc<Configuration>,
    }

    impl MetadataProvider for MockMetadataProvider {
        fn get_configuration(&self) -> Option<Arc<Configuration>> {
            Some(self.config.clone())
        }
    }

    fn make_test_provider() -> MockMetadataProvider {
        let mut config = Configuration::new("TestConfig");

        let catalog = MetadataObject {
            mdo_type: MdoType::Catalog,
            name: "Номенклатура".to_string(),
            name_en: Some("Items".to_string()),
            attributes: vec![],
            tabular_sections: vec![
                TabularSection::new(Uuid::nil(), "Штрихкоды"),
                TabularSection::new(Uuid::nil(), "Изображения"),
            ],
            children: vec![],
            enum_values: vec![],
            predefined_items: vec![],
            check_unique: false,
            code_series: bsl_metadata::CodeSeries::default(),
            constant_type: None,
            register_records: vec![],
            uuid: None,
            object_belonging: bsl_metadata::ObjectBelonging::Own,
            extended_configuration_object: None,
            common_attributes: Vec::new(),
            common_attributes_open: false,
        };
        config.add_metadata_object(catalog);

        MockMetadataProvider { config: Arc::new(config) }
    }

    #[test]
    fn test_complete_tabular_sections_no_prefix() {
        let provider = make_test_provider();
        let items =
            CompleteNestedElementsUseCase::execute(&provider, MdoType::Catalog, "Номенклатура", "");

        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.label == "Штрихкоды"));
        assert!(items.iter().any(|i| i.label == "Изображения"));
    }

    #[test]
    fn test_complete_tabular_sections_with_prefix() {
        let provider = make_test_provider();
        let items = CompleteNestedElementsUseCase::execute(
            &provider,
            MdoType::Catalog,
            "Номенклатура",
            "Штри",
        );

        assert_eq!(items.len(), 1);
        assert!(items.iter().any(|i| i.label == "Штрихкоды"));
    }

    #[test]
    fn test_complete_tabular_sections_unknown_object() {
        let provider = make_test_provider();
        let items = CompleteNestedElementsUseCase::execute(
            &provider,
            MdoType::Catalog,
            "НесуществующийОбъект",
            "",
        );

        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_complete_tabular_sections_has_detail() {
        let provider = make_test_provider();
        let items = CompleteNestedElementsUseCase::execute(
            &provider,
            MdoType::Catalog,
            "Номенклатура",
            "Штри",
        );

        assert_eq!(items.len(), 1);
        let item = &items[0];

        assert!(item.detail.is_some());
        assert!(item.detail.as_ref().unwrap().contains("Справочник.Номенклатура.Штрихкоды"));
    }
}
