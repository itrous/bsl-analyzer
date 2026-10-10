use crate::completion::sdbl::domain::{FieldFormatter, MetadataProvider, SdblCompletionItem};
use crate::completion::CompletionItemKind;
use bsl_metadata::MdoType;
use sdbl_hir::{FieldDef, MdoRef, Scope, SdblType};
use stdx::case::CaseExt;

pub struct CompleteCastFieldsUseCase;

impl CompleteCastFieldsUseCase {
    pub fn execute<M: MetadataProvider>(
        scope: Option<&Scope>,
        _metadata_provider: &M,
        mdo_type: MdoType,
        object_name: &str,
        field_chain: &[String],
        prefix: &str,
    ) -> Vec<SdblCompletionItem> {
        let prefix_lower = prefix.fold_lower();

        tracing::info!(
            ?mdo_type,
            object_name = %object_name,
            field_chain_len = field_chain.len(),
            prefix = %prefix,
            "CompleteCastFieldsUseCase: resolving CAST target type"
        );

        let base_mdo_ref = MdoRef { mdo_type, name: object_name.to_string() };

        let Some(scope) = scope else {
            tracing::warn!("no scope available for CAST field completion");
            return Vec::new();
        };

        let base_fields = scope.get_fields_for_ref(&base_mdo_ref);
        if base_fields.is_empty() {
            tracing::warn!("no fields found for CAST target type");
            return Vec::new();
        }

        let (final_type, final_fields) = if field_chain.is_empty() {
            (SdblType::Ref(base_mdo_ref), base_fields)
        } else {
            match Self::resolve_field_chain(scope, base_fields, field_chain) {
                Some((ty, fields)) => (ty, fields),
                None => {
                    tracing::warn!("failed to resolve CAST field chain");
                    return Vec::new();
                }
            }
        };

        tracing::info!(fields_count = final_fields.len(), "got fields from CAST target type");

        let table_name = Self::extract_table_name(&final_type);

        final_fields
            .into_iter()
            .filter(|field| {
                if prefix.is_empty() {
                    true
                } else {
                    field.name.fold_lower().starts_with(&prefix_lower)
                }
            })
            .map(|field| {
                let (detail, documentation) =
                    FieldFormatter::format_field_type(&field.ty, &table_name, field.is_standard);

                SdblCompletionItem::new(&field.name, CompletionItemKind::Field)
                    .with_detail(detail)
                    .with_documentation(documentation)
            })
            .collect()
    }

    fn resolve_field_chain(
        scope: &Scope,
        mut current_fields: Vec<FieldDef>,
        field_chain: &[String],
    ) -> Option<(SdblType, Vec<FieldDef>)> {
        let mut current_type = SdblType::Unknown;

        for field_name in field_chain {
            let field = current_fields.iter().find(|f| f.matches_name(field_name))?;

            current_type = field.ty.clone();

            current_fields = match &current_type {
                SdblType::Ref(mdo_ref) => scope.get_fields_for_ref(mdo_ref),
                SdblType::Composite { types } => scope.get_fields_for_composite(types),
                SdblType::DefinedType { name, underlying_type } => {
                    scope.get_fields_for_defined_type(name, underlying_type)
                }
                _ => {
                    return None;
                }
            };

            if current_fields.is_empty() {
                return None;
            }
        }

        Some((current_type, current_fields))
    }

    fn extract_table_name(ty: &SdblType) -> String {
        match ty {
            SdblType::Ref(mdo_ref) => {
                format!("{}.{}", mdo_ref.mdo_type.russian_name(), mdo_ref.name)
            }
            SdblType::Composite { .. } => "Составной тип".to_string(),
            SdblType::DefinedType { name, underlying_type } => {
                if let Some(underlying) = underlying_type {
                    Self::extract_table_name(underlying)
                } else {
                    format!("ОпределяемыйТип.{}", name)
                }
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::sdbl::domain::MetadataProvider;
    use bsl_metadata::Configuration;
    use sdbl_hir::{FieldDef, ResolvedTable, TableRef};
    use smol_str::SmolStr;
    use std::sync::Arc;

    struct TestMetadataProvider(Option<Arc<Configuration>>);

    impl MetadataProvider for TestMetadataProvider {
        fn get_configuration(&self) -> Option<Arc<Configuration>> {
            self.0.clone()
        }
    }

    fn make_test_scope_with_document() -> Scope<'static> {
        let mut config = Configuration::new("TestConfig");
        let document = bsl_metadata::MetadataObject {
            mdo_type: MdoType::Document,
            name: "НачислениеИСписаниеБонусныхБаллов".to_string(),
            name_en: Some("BonusPointsAccrualAndWriteOff".to_string()),
            attributes: vec![
                bsl_metadata::Attribute {
                    name: "ПричинаНачисленияИСписанияБонусныхБаллов".to_string(),
                    name_en: Some("BonusPointsAccrualAndWriteOffReason".to_string()),
                    attr_type: bsl_metadata::AttributeType::Ref {
                        mdo_type: MdoType::Catalog,
                        name: "ПричиныНачисленияИСписанияБонусныхБаллов".to_string(),
                    },
                },
                bsl_metadata::Attribute {
                    name: "Контрагент".to_string(),
                    name_en: Some("Counterparty".to_string()),
                    attr_type: bsl_metadata::AttributeType::Ref {
                        mdo_type: MdoType::Catalog,
                        name: "Контрагенты".to_string(),
                    },
                },
            ],
            tabular_sections: vec![],
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
        config.add_metadata_object(document);

        let metadata_arc = Arc::new(config);
        let mut scope = Scope::new_with_metadata(Some(metadata_arc));

        let table = TableRef {
            parts: vec![SmolStr::from("Справочник"), SmolStr::from("Валюты")],
            full_name: "Справочник.Валюты".to_string(),
            alias: Some(SmolStr::from("Т")),
            metadata: Some(ResolvedTable::Metadata {
                mdo_type: MdoType::Catalog,
                name: "Валюты".to_string(),
                fields: vec![
                    FieldDef::standard(
                        "Ссылка",
                        "Ref",
                        SdblType::reference(MdoType::Catalog, "Валюты"),
                    ),
                    FieldDef::new("Код", SdblType::string()),
                ],
                field_model_complete: false,
            }),
            is_virtual_table: false,
            virtual_table_params: Vec::new(),
            range: syntax::MODULE_RANGE,
            subquery: Vec::new(),
        };

        let _ = scope.add_table(table);
        scope
    }

    #[test]
    fn test_complete_cast_direct_fields() {
        let scope = make_test_scope_with_document();
        let provider = TestMetadataProvider(None);

        let items = CompleteCastFieldsUseCase::execute(
            Some(&scope),
            &provider,
            MdoType::Document,
            "НачислениеИСписаниеБонусныхБаллов",
            &[],
            "",
        );

        assert!(!items.is_empty(), "Expected fields from Document type");

        assert!(
            items.iter().any(|i| i.label == "ПричинаНачисленияИСписанияБонусныхБаллов"),
            "Expected ПричинаНачисленияИСписанияБонусныхБаллов field"
        );
    }

    #[test]
    fn test_complete_cast_with_prefix() {
        let scope = make_test_scope_with_document();
        let provider = TestMetadataProvider(None);

        let items = CompleteCastFieldsUseCase::execute(
            Some(&scope),
            &provider,
            MdoType::Document,
            "НачислениеИСписаниеБонусныхБаллов",
            &[],
            "При",
        );

        assert!(
            items.iter().all(|i| i.label.fold_lower().starts_with("при")),
            "Expected only fields starting with 'При'"
        );
    }

    #[test]
    fn test_complete_cast_no_scope() {
        let provider = TestMetadataProvider(None);

        let items = CompleteCastFieldsUseCase::execute(
            None,
            &provider,
            MdoType::Document,
            "НачислениеИСписаниеБонусныхБаллов",
            &[],
            "",
        );

        assert!(items.is_empty(), "Expected empty results without scope");
    }
}
