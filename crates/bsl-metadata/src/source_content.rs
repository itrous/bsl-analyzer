use serde::{Deserialize, Serialize};

use crate::xml_parser::helpers::{child_text, find_child};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangePlanItem {
    pub metadata: String,
    pub auto_record: Option<String>,
}

/// Read the exchange composition sidecar; an empty list is a confirmed empty plan.
pub fn parse_exchange_plan_content(xml: &str) -> Option<Vec<ExchangePlanItem>> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "ExchangePlanContent" {
        return None;
    }
    root.children()
        .filter(|node| node.is_element())
        .map(|node| {
            (node.tag_name().name() == "Item").then_some(())?;
            Some(ExchangePlanItem {
                metadata: child_text(node, "Metadata")?.to_string(),
                auto_record: child_text(node, "AutoRecord").map(str::to_owned),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountingFlag {
    pub reference: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredefinedExtDimension {
    pub name: Option<String>,
    pub turnover: Option<bool>,
    pub accounting_flags: Option<Vec<AccountingFlag>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredefinedAccount {
    pub name: String,
    pub uuid: String,
    pub parent: Option<String>,
    pub code: Option<String>,
    pub description: Option<String>,
    pub account_type: Option<String>,
    pub off_balance: Option<bool>,
    pub order: Option<String>,
    pub extension_state: Option<String>,
    pub accounting_flags: Option<Vec<AccountingFlag>>,
    pub ext_dimension_types: Option<Vec<PredefinedExtDimension>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_origins: Vec<String>,
}

fn optional_bool(raw: Option<&str>) -> Option<bool> {
    match raw {
        Some("true" | "1") => Some(true),
        Some("false" | "0") => Some(false),
        _ => None,
    }
}

fn flags(node: roxmltree::Node<'_, '_>) -> Option<Vec<AccountingFlag>> {
    Some(
        find_child(node, "AccountingFlags")?
            .children()
            .filter(|node| node.is_element())
            .map(|node| AccountingFlag {
                reference: node.attribute("ref").map(str::to_owned),
                enabled: optional_bool(node.text()),
            })
            .collect(),
    )
}

pub fn parse_predefined_accounts(xml: &str) -> Option<Vec<PredefinedAccount>> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "PredefinedData" {
        return None;
    }
    let mut values = Vec::new();
    collect_accounts(root, None, &mut values)?;
    Some(values)
}

fn collect_accounts(
    node: roxmltree::Node<'_, '_>,
    parent: Option<&str>,
    values: &mut Vec<PredefinedAccount>,
) -> Option<()> {
    for item in node.children().filter(|node| node.is_element() && node.tag_name().name() == "Item")
    {
        let uuid = item.attribute("id")?.to_string();
        let name = child_text(item, "Name")?.to_string();
        let text =
            |field| find_child(item, field).map(|node| node.text().unwrap_or_default().to_string());
        let ext_dimension_types = find_child(item, "ExtDimensionTypes").map(|node| {
            node.children()
                .filter(|node| node.is_element())
                .map(|node| PredefinedExtDimension {
                    name: node.attribute("name").map(str::to_owned),
                    turnover: optional_bool(child_text(node, "Turnover")),
                    accounting_flags: flags(node),
                })
                .collect()
        });
        values.push(PredefinedAccount {
            name,
            uuid: uuid.clone(),
            parent: parent.map(str::to_owned),
            code: text("Code"),
            description: text("Description"),
            account_type: text("AccountType"),
            off_balance: optional_bool(child_text(item, "OffBalance")),
            order: text("Order"),
            extension_state: text("ExtensionState"),
            accounting_flags: flags(item),
            ext_dimension_types,
            source_origins: Vec::new(),
        });
        if let Some(children) = find_child(item, "ChildItems") {
            collect_accounts(children, Some(&uuid), values)?;
        }
    }
    Some(())
}

impl PredefinedAccount {
    pub fn apply_overlay(&mut self, overlay: &Self) {
        macro_rules! replace_present {($($field:ident),*) => {$(if overlay.$field.is_some() {self.$field.clone_from(&overlay.$field);})*};}
        replace_present!(
            code,
            description,
            account_type,
            off_balance,
            order,
            extension_state,
            accounting_flags,
            ext_dimension_types
        );
        self.parent.clone_from(&overlay.parent);
        for origin in &overlay.source_origins {
            if !self.source_origins.contains(origin) {
                self.source_origins.push(origin.clone());
            }
        }
    }

    pub fn estimated_heap_size(&self) -> usize {
        fn flags_size(flags: &Option<Vec<AccountingFlag>>) -> usize {
            flags.as_ref().map_or(0, |values| {
                stdx::heap::vec_bytes::<AccountingFlag>(values.len())
                    + values
                        .iter()
                        .map(|flag| flag.reference.as_ref().map_or(0, String::capacity))
                        .sum::<usize>()
            })
        }
        self.name.capacity()
            + self.uuid.capacity()
            + [
                &self.parent,
                &self.code,
                &self.description,
                &self.account_type,
                &self.order,
                &self.extension_state,
            ]
            .iter()
            .map(|s| s.as_ref().map_or(0, String::capacity))
            .sum::<usize>()
            + stdx::heap::vec_bytes::<String>(self.source_origins.capacity())
            + self.source_origins.iter().map(String::capacity).sum::<usize>()
            + flags_size(&self.accounting_flags)
            + self.ext_dimension_types.as_ref().map_or(0, |values| {
                stdx::heap::vec_bytes::<PredefinedExtDimension>(values.len())
                    + values
                        .iter()
                        .map(|value| {
                            value.name.as_ref().map_or(0, String::capacity)
                                + flags_size(&value.accounting_flags)
                        })
                        .sum::<usize>()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_exchange_content_distinguishes_empty_unknown_and_auto_record_modes() {
        let values = parse_exchange_plan_content("<ExchangePlanContent><Item><Metadata>Catalog.Products</Metadata><AutoRecord>Allow</AutoRecord></Item><Item><Metadata>Document.Sale</Metadata><AutoRecord>Deny</AutoRecord></Item></ExchangePlanContent>").unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].metadata, "Catalog.Products");
        assert_eq!(values[0].auto_record.as_deref(), Some("Allow"));
        assert_eq!(values[1].auto_record.as_deref(), Some("Deny"));
        assert_eq!(parse_exchange_plan_content("<ExchangePlanContent/>"), Some(vec![]));
        assert_eq!(parse_exchange_plan_content("<Other/>"), None);
        assert_eq!(
            parse_exchange_plan_content("<ExchangePlanContent><Item/></ExchangePlanContent>"),
            None
        );
    }

    #[test]
    fn source_predefined_accounts_keep_parent_code_and_ordered_analytics() {
        let xml = r#"<PredefinedData><Item id="parent"><Name>Assets</Name><Code>01</Code><AccountType>Active</AccountType><OffBalance>false</OffBalance><AccountingFlags><Flag ref="ChartOfAccounts.Accounts.AccountingFlag.Quantity">true</Flag></AccountingFlags><ExtDimensionTypes><ExtDimensionType name="ChartOfCharacteristicTypes.Analytics.Products"><Turnover>false</Turnover><AccountingFlags/></ExtDimensionType><ExtDimensionType name="ChartOfCharacteristicTypes.Analytics.Partners"><Turnover>true</Turnover><AccountingFlags/></ExtDimensionType></ExtDimensionTypes><ChildItems><Item id="child"><Name>Cash</Name><Code>01.1</Code><ExtDimensionTypes/></Item></ChildItems></Item></PredefinedData>"#;
        let values = parse_predefined_accounts(xml).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].parent, None);
        assert_eq!(values[1].parent.as_deref(), Some("parent"));
        assert_eq!(values[1].code.as_deref(), Some("01.1"));
        assert!(values[1].ext_dimension_types.as_ref().unwrap().is_empty());
        let analytics = values[0].ext_dimension_types.as_ref().unwrap();
        assert_eq!(
            analytics[0].name.as_deref(),
            Some("ChartOfCharacteristicTypes.Analytics.Products")
        );
        assert_eq!(analytics[1].turnover, Some(true));
        assert_eq!(values[0].accounting_flags.as_ref().unwrap()[0].enabled, Some(true));
    }

    #[test]
    fn source_predefined_overlay_can_clear_analytics_and_preserves_missing_flags() {
        let mut base = parse_predefined_accounts(r#"<PredefinedData><Item id="one"><Name>Cash</Name><Code>50</Code><AccountingFlags><Flag ref="Quantity">false</Flag></AccountingFlags><ExtDimensionTypes><ExtDimensionType name="Kind"/></ExtDimensionTypes></Item></PredefinedData>"#).unwrap().remove(0);
        let overlay = parse_predefined_accounts(r#"<PredefinedData><Item id="one"><Name>Cash</Name><Code>51</Code><ExtDimensionTypes/></Item></PredefinedData>"#).unwrap().remove(0);
        base.apply_overlay(&overlay);
        assert_eq!(base.code.as_deref(), Some("51"));
        assert!(base.ext_dimension_types.as_ref().unwrap().is_empty());
        assert_eq!(base.accounting_flags.as_ref().unwrap()[0].enabled, Some(false));
    }
}
