use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::AttributeType;

/// All translations in the source; `None` means unread, an empty map means empty.
pub type LocalizedString = BTreeMap<String, String>;

/// A native assertion binding a configured technical name to an infobase reference.
/// The reference UUID is independent from configured PredefinedData Item IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePredefinedIdentity {
    pub metadata_type: String,
    pub metadata_name: String,
    pub predefined_name: String,
    pub runtime_ref_uuid: String,
}

/// The declared value, including its XML type and an explicit null marker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataValue {
    pub type_name: Option<String>,
    pub value: Option<String>,
    pub nil: bool,
    pub fields: Vec<(String, MetadataValue)>,
    /// Native evidence is absent from source values. Box the sparse proof instead
    /// of adding four string handles to every declared value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_predefined_identity: Option<Box<RuntimePredefinedIdentity>>,
}

impl MetadataValue {
    /// Whether a live CatalogRef needs a name binding; primitive, enumeration,
    /// source DesignTimeRef and confirmed empty reference values do not.
    pub fn is_nonempty_runtime_catalog_reference(&self) -> bool {
        !self.nil
            && self.fields.is_empty()
            && self.type_name.as_deref().is_some_and(|name| {
                name.strip_prefix("{}CatalogRef.").is_some_and(|name| !name.is_empty())
            })
            && self
                .value
                .as_deref()
                .and_then(|value| crate::Uuid::parse_str(value).ok())
                .is_some_and(|uuid| !uuid.is_nil())
    }

    /// Check the native assertion against the XML reference type and UUID.
    /// This preserves the asserted name; it does not infer a name from a UUID.
    pub fn runtime_predefined_identity_matches_xml(&self) -> bool {
        let Some(identity) = &self.runtime_predefined_identity else { return false };
        let component = |name: &str| {
            !name.is_empty()
                && !name.chars().any(|character| character == '.' || character.is_whitespace())
        };
        self.is_nonempty_runtime_catalog_reference()
            && identity.metadata_type == "Catalog"
            && component(&identity.metadata_name)
            && component(&identity.predefined_name)
            && self.type_name.as_deref()
                == Some(format!("{{}}CatalogRef.{}", identity.metadata_name).as_str())
            && crate::Uuid::parse_str(&identity.runtime_ref_uuid).ok()
                == self.value.as_deref().and_then(|value| crate::Uuid::parse_str(value).ok())
    }

    pub fn estimated_heap_size(&self) -> usize {
        self.type_name.as_ref().map_or(0, String::capacity)
            + self.value.as_ref().map_or(0, String::capacity)
            + stdx::heap::vec_bytes::<(String, Self)>(self.fields.len())
            + self
                .fields
                .iter()
                .map(|(name, value)| name.capacity() + value.estimated_heap_size())
                .sum::<usize>()
            + self.runtime_predefined_identity.as_ref().map_or(0, |identity| {
                std::mem::size_of::<RuntimePredefinedIdentity>()
                    + identity.metadata_type.capacity()
                    + identity.metadata_name.capacity()
                    + identity.predefined_name.capacity()
                    + identity.runtime_ref_uuid.capacity()
            })
    }
}

/// Presentation and use constraints of a declared member, addressed by kind/name pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberReadProperties {
    pub path: Vec<String>,
    pub synonym: Option<LocalizedString>,
    pub synonym_complete: Option<bool>,
    pub use_mode: Option<String>,
    pub fill_value: Option<MetadataValue>,
    pub choice: crate::xml_parser::form_inspection::ChoiceProperties,
}

/// Source facts which cannot be reconstructed from the object's field types.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectReadProperties {
    pub synonym: Option<LocalizedString>,
    pub synonym_complete: Option<bool>,
    pub members: Vec<MemberReadProperties>,
    pub hierarchical: Option<bool>,
    pub hierarchy_type: Option<String>,
    pub code_type: Option<String>,
    pub code_length: Option<u32>,
    pub code_allowed_length: Option<String>,
    pub periodicity: Option<String>,
    pub write_mode: Option<String>,
    pub characteristic_value_type: Option<AttributeType>,
    pub exchange_plan_content: Option<Vec<crate::source_content::ExchangePlanItem>>,
    pub exchange_plan_content_complete: bool,
    pub predefined_accounts: Option<Vec<crate::source_content::PredefinedAccount>>,
}

impl ObjectReadProperties {
    pub fn set_source_origin(&mut self, origin: &str) {
        for member in &mut self.members {
            for value in member.choice.property_origins.values_mut() {
                *value = origin.into();
            }
        }
        if !origin.is_empty() {
            for account in self.predefined_accounts.iter_mut().flatten() {
                account.source_origins = vec![origin.into()];
            }
        }
    }
    /// Missing extension properties leave the corresponding base fact intact.
    /// Merge extension properties and report an unresolved predefined account check.
    pub fn apply_overlay(&mut self, overlay: &Self) -> bool {
        macro_rules! replace_present {
            ($($field:ident),* $(,)?) => {$(
                if overlay.$field.is_some() { self.$field.clone_from(&overlay.$field); }
            )*};
        }
        replace_present!(
            synonym,
            hierarchical,
            hierarchy_type,
            code_type,
            code_length,
            code_allowed_length,
            periodicity,
            write_mode,
            characteristic_value_type
        );
        if overlay.synonym.is_some() {
            self.synonym_complete = overlay.synonym_complete;
        }
        if let Some(items) = &overlay.exchange_plan_content {
            let existing = self.exchange_plan_content.get_or_insert_default();
            for item in items {
                if let Some(base) = existing
                    .iter_mut()
                    .find(|base| stdx::case::eq_ignore_case(&base.metadata, &item.metadata))
                {
                    if item.auto_record.is_some() {
                        base.auto_record.clone_from(&item.auto_record);
                    }
                } else {
                    existing.push(item.clone());
                }
            }
        }
        self.exchange_plan_content_complete &= overlay.exchange_plan_content_complete;
        let mut unresolved_account_check = false;
        if let Some(items) = &overlay.predefined_accounts {
            let existing = self.predefined_accounts.get_or_insert_default();
            let is_check = |item: &crate::source_content::PredefinedAccount| {
                item.extension_state.as_deref() == Some("AdoptedCheck")
            };
            // A borrowed check only anchors children in the extension's XML.
            let mut parent_aliases = BTreeMap::new();
            for check in items.iter().filter(|item| is_check(item)) {
                let mut matches = existing
                    .iter()
                    .filter(|base| stdx::case::eq_ignore_case(&base.name, &check.name));
                match (matches.next(), matches.next()) {
                    (Some(base), None) => {
                        parent_aliases.insert(check.uuid.as_str(), base.uuid.clone());
                    }
                    _ => unresolved_account_check = true,
                }
            }
            for item in items.iter().filter(|item| !is_check(item)) {
                let mut item = item.clone();
                if let Some(parent) = item.parent.as_deref().and_then(|id| parent_aliases.get(id)) {
                    item.parent = Some(parent.clone());
                }
                if let Some(base) = existing.iter_mut().find(|base| base.uuid == item.uuid) {
                    base.apply_overlay(&item);
                } else {
                    existing.push(item);
                }
            }
        }
        for member in &overlay.members {
            if let Some(base) = self.members.iter_mut().find(|base| {
                base.path.len() == member.path.len()
                    && base
                        .path
                        .iter()
                        .zip(&member.path)
                        .all(|(a, b)| stdx::case::eq_ignore_case(a, b))
            }) {
                if member.synonym.is_some() {
                    base.synonym.clone_from(&member.synonym);
                    base.synonym_complete = member.synonym_complete;
                }
                if member.use_mode.is_some() {
                    base.use_mode.clone_from(&member.use_mode);
                }
                if member.fill_value.is_some() {
                    base.fill_value.clone_from(&member.fill_value);
                }
                base.choice.apply_overlay(&member.choice);
            } else {
                self.members.push(member.clone());
            }
        }
        unresolved_account_check
    }

    pub fn estimated_heap_size(&self) -> usize {
        fn localized(value: &Option<LocalizedString>) -> usize {
            value.as_ref().map_or(0, |items| {
                items
                    .iter()
                    .map(|(key, value)| {
                        std::mem::size_of::<(String, String)>() + key.capacity() + value.capacity()
                    })
                    .sum()
            })
        }
        localized(&self.synonym)
            + self.exchange_plan_content.as_ref().map_or(0, |items| {
                stdx::heap::vec_bytes::<crate::source_content::ExchangePlanItem>(items.len())
                    + items
                        .iter()
                        .map(|item| {
                            item.metadata.capacity()
                                + item.auto_record.as_ref().map_or(0, String::capacity)
                        })
                        .sum::<usize>()
            })
            + self.predefined_accounts.as_ref().map_or(0, |items| {
                stdx::heap::vec_bytes::<crate::source_content::PredefinedAccount>(items.len())
                    + items
                        .iter()
                        .map(crate::source_content::PredefinedAccount::estimated_heap_size)
                        .sum::<usize>()
            })
            + [
                &self.hierarchy_type,
                &self.code_type,
                &self.code_allowed_length,
                &self.periodicity,
                &self.write_mode,
            ]
            .iter()
            .map(|value| value.as_ref().map_or(0, String::capacity))
            .sum::<usize>()
            + self.characteristic_value_type.as_ref().map_or(0, AttributeType::estimated_heap_size)
            + stdx::heap::vec_bytes::<MemberReadProperties>(self.members.len())
            + self
                .members
                .iter()
                .map(|member| {
                    stdx::heap::vec_bytes::<String>(member.path.len())
                        + member.path.iter().map(String::capacity).sum::<usize>()
                        + localized(&member.synonym)
                        + member.use_mode.as_ref().map_or(0, String::capacity)
                        + member.fill_value.as_ref().map_or(0, MetadataValue::estimated_heap_size)
                        + member.choice.estimated_heap_size()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopted_account_check_anchors_native_children_without_replacing_base() {
        let accounts = |xml| crate::source_content::parse_predefined_accounts(xml);
        let mut base = ObjectReadProperties {
            predefined_accounts: accounts(
                r#"<PredefinedData><Item id="base"><Name>Cash</Name><Code>50</Code></Item></PredefinedData>"#,
            ),
            ..Default::default()
        };
        let mut overlay = ObjectReadProperties {
            predefined_accounts: accounts(
                r#"<PredefinedData><Item id="check"><Name>Cash</Name><Code>51</Code>
                <ExtensionState>AdoptedCheck</ExtensionState>
                <ChildItems><Item id="own"><Name>CashDesk</Name><Code>50.01</Code>
                <ExtensionState>Native</ExtensionState></Item></ChildItems>
                </Item></PredefinedData>"#,
            ),
            ..Default::default()
        };
        base.set_source_origin(":ChartsOfAccounts/Accounts.xml");
        overlay.set_source_origin("Extra:ChartsOfAccounts/Accounts.xml");
        assert!(!base.apply_overlay(&overlay));
        let effective = base.predefined_accounts.unwrap();
        assert_eq!(effective.len(), 2);
        assert_eq!(effective[0].uuid, "base");
        assert_eq!(effective[0].code.as_deref(), Some("50"));
        assert_eq!(effective[1].uuid, "own");
        assert_eq!(effective[1].parent.as_deref(), Some("base"));
        assert_eq!(
            serde_json::to_value(&effective[0]).unwrap()["source_origins"],
            serde_json::json!([":ChartsOfAccounts/Accounts.xml"])
        );
        assert_eq!(
            serde_json::to_value(&effective[1]).unwrap()["source_origins"],
            serde_json::json!(["Extra:ChartsOfAccounts/Accounts.xml"])
        );

        let mut missing = ObjectReadProperties::default();
        assert!(missing.apply_overlay(&overlay));
        assert_eq!(missing.predefined_accounts.unwrap().len(), 1);
    }

    #[test]
    fn fill_value_keeps_legacy_shape_without_native_evidence() {
        let raw = serde_json::json!({"type_name":"{http://www.w3.org/2001/XMLSchema}string",
            "value":"","nil":false,"fields":[]});
        let value: MetadataValue = serde_json::from_value(raw.clone()).unwrap();
        assert!(value.runtime_predefined_identity.is_none());
        assert_eq!(serde_json::to_value(value).unwrap(), raw);
    }

    #[test]
    fn fill_value_accounts_for_sparse_runtime_identity_allocations() {
        let mut value: MetadataValue = serde_json::from_value(serde_json::json!({
            "type_name":"{}CatalogRef.OwnCountries", "value":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "nil":false, "fields":[]
        }))
        .unwrap();
        let before = value.estimated_heap_size();
        let identity = RuntimePredefinedIdentity {
            metadata_type: "Catalog".into(),
            metadata_name: "OwnCountries".into(),
            predefined_name: "OwnCountry".into(),
            runtime_ref_uuid: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        };
        let content = identity.metadata_type.len()
            + identity.metadata_name.len()
            + identity.predefined_name.len()
            + identity.runtime_ref_uuid.len();
        value.runtime_predefined_identity = Some(Box::new(identity));
        assert!(value.runtime_predefined_identity_matches_xml());
        assert!(
            value.estimated_heap_size()
                >= before + std::mem::size_of::<RuntimePredefinedIdentity>() + content
        );
        let wire = serde_json::to_value(&value).unwrap();
        assert_eq!(serde_json::from_value::<MetadataValue>(wire).unwrap(), value);
    }
}
