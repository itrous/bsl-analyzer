use bsl_metadata::declaration_identity::DeclarationSegment;
use bsl_metadata::MdoType;
use text_size::TextRange;

/// A query token bound by its own scope, before source-set identity is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataReference {
    pub range: TextRange,
    pub owner_kind: MdoType,
    pub owner_name: String,
    pub members: Vec<DeclarationSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataReferenceIssue {
    pub range: TextRange,
    pub reason: MetadataReferenceIssueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataReferenceIssueKind {
    Unresolved,
    Ambiguous,
    OutputAlias,
    DerivedField,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration() -> std::sync::Arc<bsl_metadata::Configuration> {
        let mut configuration = bsl_metadata::Configuration::new("Preview");
        for (name, uuid) in [("Partners", 1), ("Customers", 2)] {
            let xml=format!("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'><Catalog uuid='00000000-0000-0000-0000-{uuid:012}'><Properties><Name>{name}</Name></Properties><ChildObjects><Attribute uuid='00000000-0000-0000-0001-{uuid:012}'><Properties><Name>Caption</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>");
            configuration
                .add_metadata_object(bsl_metadata::xml_parser::parse_catalog_xml(&xml).unwrap());
        }
        std::sync::Arc::new(configuration)
    }

    #[test]
    fn query_metadata_references_keep_alias_owner_and_identifier_ranges() {
        let query =
            "SELECT P.Caption, C.Caption FROM Catalog.Partners AS P, Catalog.Customers AS C";
        let lowered = crate::lower_sdbl_to_hir(&parser::parse_sdbl(query), Some(configuration()));
        let fields = lowered
            .source_map
            .metadata_references()
            .iter()
            .filter(|reference| !reference.members.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].owner_name, "Partners");
        assert_eq!(fields[1].owner_name, "Customers");
        assert!(fields.iter().all(|reference| &query[reference.range] == "Caption"));
    }

    #[test]
    fn ambiguous_unqualified_field_is_not_a_confirmed_metadata_reference() {
        let query = "SELECT Caption FROM Catalog.Partners AS P, Catalog.Customers AS C";
        let lowered = crate::lower_sdbl_to_hir(&parser::parse_sdbl(query), Some(configuration()));
        assert!(lowered
            .source_map
            .metadata_references()
            .iter()
            .all(|reference| reference.members.is_empty()));
        assert!(lowered
            .source_map
            .metadata_reference_issues()
            .iter()
            .any(|issue| issue.reason == MetadataReferenceIssueKind::Ambiguous));
    }

    #[test]
    fn query_metadata_references_accept_bilingual_table_roots_and_aliases() {
        for query in [
            "SELECT P.Caption FROM Catalog.Partners AS P",
            "ВЫБРАТЬ П.Caption ИЗ Справочник.Partners КАК П",
        ] {
            let lowered =
                crate::lower_sdbl_to_hir(&parser::parse_sdbl(query), Some(configuration()));
            let fields = lowered
                .source_map
                .metadata_references()
                .iter()
                .filter(|reference| !reference.members.is_empty())
                .collect::<Vec<_>>();
            assert_eq!(fields.len(), 1, "{query}");
            assert_eq!(fields[0].owner_name, "Partners");
            assert_eq!(&query[fields[0].range], "Caption");
        }
    }
    #[test]
    fn implicit_physical_table_alias_resolves_select_and_where() {
        let query = "SELECT Partners.Caption FROM Catalog.Partners WHERE partners.Caption = \"x\"";
        let lowered = crate::lower_sdbl_to_hir(&parser::parse_sdbl(query), Some(configuration()));
        assert!(
            lowered.queries().iter().all(|query| query
                .hir
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.is_error())),
            "{lowered:#?}"
        );
        let fields = lowered
            .source_map
            .metadata_references()
            .iter()
            .filter(|reference| !reference.members.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 2, "{lowered:#?}");
        assert!(fields.iter().all(|reference| reference.owner_name == "Partners"));
    }

    #[test]
    fn explicit_physical_alias_does_not_bind_an_unrelated_owner() {
        let query = "SELECT P.Caption, Customers.Caption FROM Catalog.Partners AS P, Catalog.Customers WHERE Customers.Caption = \"x\"";
        let lowered = crate::lower_sdbl_to_hir(&parser::parse_sdbl(query), Some(configuration()));
        let fields = lowered
            .source_map
            .metadata_references()
            .iter()
            .filter(|reference| !reference.members.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            fields.iter().map(|reference| reference.owner_name.as_str()).collect::<Vec<_>>(),
            ["Partners", "Customers", "Customers"]
        );
    }
}
