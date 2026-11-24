use super::*;

const APP: &str = "http://v8.1c.ru/8.2/managed-application/core";
const XS: &str = "http://www.w3.org/2001/XMLSchema";

#[derive(Clone, Copy)]
enum Contract {
    Text,
    LocalText,
    Boolean,
    Number,
    Enumeration,
    Type,
    Reference(&'static str),
    FieldList,
    ReferenceList,
    RoleList,
    StandardAttributes,
    Primitive,
    FillValue,
    ChoiceParameters,
    Empty,
    UsePurposes,
    MobileFunctionalities,
}

// Scalar kinds come from platform metadata help; compound shapes come from the readable XDTO schema.
fn contract(owner: &str, name: &str) -> Option<Contract> {
    use Contract::*;
    if owner.parse::<MdoType>().is_err()
        && !matches!(
            owner,
            "Attribute"
                | "TabularSection"
                | "Dimension"
                | "Resource"
                | "EnumValue"
                | "Form"
                | "Template"
                | "Command"
                | "Configuration"
                | "Language"
                | "SessionParameter"
                | "CommonAttribute"
                | "CommonForm"
                | "CommonTemplate"
                | "StandardAttribute"
        )
    {
        return None;
    }
    let common = match name {
        "Name" | "Comment" => Some(Text),
        "Synonym" => Some(LocalText),
        "ObjectBelonging" => Some(Enumeration),
        "ExtendedConfigurationObject" => Some(Text),
        _ => None,
    };
    if common.is_some() {
        return common;
    }
    let attribute = matches!(
        owner,
        "Attribute" | "Dimension" | "Resource" | "CommonAttribute" | "StandardAttribute"
    );
    if attribute {
        let value = match name {
            "Type" => Some(Type),
            "Format" | "EditFormat" | "ToolTip" => Some(LocalText),
            "Mask" => Some(Text),
            "PasswordMode"
            | "MarkNegatives"
            | "MultiLine"
            | "ExtendedEdit"
            | "FillFromFillingValue" => Some(Boolean),
            "FillChecking"
            | "ChoiceFoldersAndItems"
            | "QuickChoice"
            | "CreateOnInput"
            | "ChoiceHistoryOnInput"
            | "Indexing"
            | "FullTextSearch"
            | "DataHistory"
            | "TypeReductionMode" => Some(Enumeration),
            "MinValue" | "MaxValue" => Some(Primitive),
            "FillValue" => Some(FillValue),
            "ChoiceForm" => Some(Reference("Form")),
            "ChoiceParameters" => Some(ChoiceParameters),
            "ChoiceParameterLinks" | "LinkByType" => Some(Empty),
            _ => None,
        };
        if value.is_some() {
            return value;
        }
    }
    match (owner, name) {
        (
            "CommonModule",
            "Global"
            | "ClientManagedApplication"
            | "Server"
            | "ExternalConnection"
            | "ClientOrdinaryApplication"
            | "ServerCall"
            | "Privileged",
        ) => Some(Boolean),
        ("CommonModule", "ReturnValuesReuse") => Some(Enumeration),
        ("CommonForm", "IncludeHelpInContents" | "UseStandardCommands") => Some(Boolean),
        ("CommonForm", "UsePurposes") => Some(UsePurposes),
        ("CommonForm", "ExtendedPresentation" | "Explanation") => Some(LocalText),
        ("TabularSection", "ToolTip") => Some(LocalText),
        ("TabularSection", "FillChecking") | ("Attribute" | "TabularSection", "Use") => {
            Some(Enumeration)
        }
        ("TabularSection", "LineNumberLength") => Some(Number),
        ("TabularSection" | "Document" | "ExchangePlan", "StandardAttributes") => {
            Some(StandardAttributes)
        }
        (
            "Document",
            "UseStandardCommands"
            | "CheckUnique"
            | "Autonumbering"
            | "PostInPrivilegedMode"
            | "UnpostInPrivilegedMode"
            | "IncludeHelpInContents"
            | "UpdateDataHistoryImmediatelyAfterWrite"
            | "ExecuteAfterWriteDataHistoryVersionProcessing",
        ) => Some(Boolean),
        ("Document", "NumberLength") => Some(Number),
        (
            "Document",
            "NumberType"
            | "NumberAllowedLength"
            | "NumberPeriodicity"
            | "CreateOnInput"
            | "SearchStringModeOnInputByString"
            | "FullTextSearchOnInputByString"
            | "ChoiceDataGetModeOnInputByString"
            | "Posting"
            | "RealTimePosting"
            | "RegisterRecordsDeletion"
            | "RegisterRecordsWritingOnPost"
            | "SequenceFilling"
            | "DataLockControlMode"
            | "FullTextSearch"
            | "ChoiceHistoryOnInput"
            | "DataHistory",
        ) => Some(Enumeration),
        ("Document", "Numerator" | "Characteristics") => Some(Empty),
        ("Document" | "ExchangePlan", "BasedOn") | ("Document", "RegisterRecords") => {
            Some(ReferenceList)
        }
        ("Document" | "ExchangePlan", "InputByString" | "DataLockFields") => Some(FieldList),
        (
            "Document" | "ExchangePlan",
            "DefaultObjectForm"
            | "DefaultListForm"
            | "DefaultChoiceForm"
            | "AuxiliaryObjectForm"
            | "AuxiliaryListForm"
            | "AuxiliaryChoiceForm",
        ) => Some(Reference("Form")),
        (
            "Document" | "ExchangePlan",
            "ObjectPresentation"
            | "ExtendedObjectPresentation"
            | "ListPresentation"
            | "ExtendedListPresentation"
            | "Explanation",
        ) => Some(LocalText),
        (
            "ExchangePlan",
            "UseStandardCommands"
            | "QuickChoice"
            | "DistributedInfoBase"
            | "IncludeConfigurationExtensions"
            | "IncludeHelpInContents"
            | "UpdateDataHistoryImmediatelyAfterWrite"
            | "ExecuteAfterWriteDataHistoryVersionProcessing",
        ) => Some(Boolean),
        ("ExchangePlan", "CodeLength" | "DescriptionLength") => Some(Number),
        (
            "ExchangePlan",
            "CodeAllowedLength"
            | "DefaultPresentation"
            | "EditType"
            | "ChoiceMode"
            | "SearchStringModeOnInputByString"
            | "FullTextSearchOnInputByString"
            | "ChoiceDataGetModeOnInputByString"
            | "CreateOnInput"
            | "ChoiceHistoryOnInput"
            | "DataLockControlMode"
            | "FullTextSearch"
            | "DataHistory",
        ) => Some(Enumeration),
        ("ExchangePlan", "Characteristics") => Some(Empty),
        ("Report", "UseStandardCommands" | "IncludeHelpInContents") => Some(Boolean),
        (
            "Report",
            "DefaultForm"
            | "AuxiliaryForm"
            | "DefaultSettingsForm"
            | "AuxiliarySettingsForm"
            | "DefaultVariantForm",
        ) => Some(Reference("Form")),
        ("Report", "MainDataCompositionSchema") => Some(Reference("Template")),
        ("Report", "VariantsStorage" | "SettingsStorage") => Some(Reference("SettingsStorage")),
        ("Report", "ExtendedPresentation" | "Explanation") => Some(LocalText),
        ("Form" | "CommonForm", "FormType") | ("Template" | "CommonTemplate", "TemplateType") => {
            Some(Enumeration)
        }
        (
            "Catalog",
            "Hierarchical"
            | "LimitLevelCount"
            | "FoldersOnTop"
            | "UseStandardCommands"
            | "CheckUnique"
            | "Autonumbering"
            | "QuickChoice"
            | "IncludeHelpInContents"
            | "UpdateDataHistoryImmediatelyAfterWrite"
            | "ExecuteAfterWriteDataHistoryVersionProcessing",
        ) => Some(Boolean),
        ("Catalog", "LevelCount" | "CodeLength" | "DescriptionLength") => Some(Number),
        (
            "Catalog",
            "HierarchyType"
            | "SubordinationUse"
            | "CodeType"
            | "CodeAllowedLength"
            | "CodeSeries"
            | "DefaultPresentation"
            | "PredefinedDataUpdate"
            | "EditType"
            | "ChoiceMode"
            | "SearchStringModeOnInputByString"
            | "FullTextSearchOnInputByString"
            | "ChoiceDataGetModeOnInputByString"
            | "DataLockControlMode"
            | "FullTextSearch"
            | "CreateOnInput"
            | "ChoiceHistoryOnInput"
            | "DataHistory",
        ) => Some(Enumeration),
        (
            "Catalog",
            "ObjectPresentation"
            | "ExtendedObjectPresentation"
            | "ListPresentation"
            | "ExtendedListPresentation"
            | "Explanation",
        ) => Some(LocalText),
        (
            "Catalog",
            "DefaultObjectForm"
            | "DefaultFolderForm"
            | "DefaultListForm"
            | "DefaultChoiceForm"
            | "DefaultFolderChoiceForm"
            | "AuxiliaryObjectForm"
            | "AuxiliaryFolderForm"
            | "AuxiliaryListForm"
            | "AuxiliaryChoiceForm"
            | "AuxiliaryFolderChoiceForm",
        ) => Some(Reference("Form")),
        ("Catalog", "InputByString" | "DataLockFields") => Some(FieldList),
        ("Catalog", "Owners" | "BasedOn" | "Characteristics") => Some(Empty),
        (
            "CommonAttribute",
            "AutoUse"
            | "DataSeparation"
            | "SeparatedDataUse"
            | "UsersSeparation"
            | "AuthenticationSeparation"
            | "ConfigurationExtensionsSeparation",
        ) => Some(Enumeration),
        ("CommonAttribute", "DataSeparationValue" | "DataSeparationUse") => {
            Some(Reference("SessionParameter"))
        }
        ("CommonAttribute", "ConditionalSeparation") => Some(Reference("Constant")),
        ("CommonAttribute", "Content") => Some(Empty),
        ("Dimension", "Master" | "MainFilter" | "DenyIncompleteValues") => Some(Boolean),
        (
            "InformationRegister",
            "UseStandardCommands"
            | "MainFilterOnPeriod"
            | "IncludeHelpInContents"
            | "EnableTotalsSliceFirst"
            | "EnableTotalsSliceLast"
            | "UpdateDataHistoryImmediatelyAfterWrite"
            | "ExecuteAfterWriteDataHistoryVersionProcessing",
        ) => Some(Boolean),
        (
            "InformationRegister",
            "EditType"
            | "InformationRegisterPeriodicity"
            | "WriteMode"
            | "DataLockControlMode"
            | "FullTextSearch"
            | "DataHistory",
        ) => Some(Enumeration),
        (
            "InformationRegister",
            "RecordPresentation"
            | "ExtendedRecordPresentation"
            | "ListPresentation"
            | "ExtendedListPresentation"
            | "Explanation",
        ) => Some(LocalText),
        (
            "InformationRegister",
            "DefaultRecordForm" | "DefaultListForm" | "AuxiliaryRecordForm" | "AuxiliaryListForm",
        ) => Some(Reference("Form")),
        ("InformationRegister", "StandardAttributes") => Some(StandardAttributes),
        ("SessionParameter", "Type") => Some(Type),
        ("Language", "LanguageCode") => Some(Text),
        (
            "Configuration",
            "Vendor"
            | "Version"
            | "UpdateCatalogAddress"
            | "Copyright"
            | "VendorInformationAddress"
            | "ConfigurationInformationAddress",
        ) => Some(Text),
        ("Configuration", "BriefInformation" | "DetailedInformation") => Some(LocalText),
        (
            "Configuration",
            "IncludeHelpInContents"
            | "KeepMappingToExtendedConfigurationObjectsByIDs"
            | "UseManagedFormInOrdinaryApplication"
            | "UseOrdinaryFormInManagedApplication",
        ) => Some(Boolean),
        (
            "Configuration",
            "ConfigurationExtensionCompatibilityMode"
            | "ConfigurationExtensionPurpose"
            | "DefaultRunMode"
            | "ScriptVariant"
            | "MainClientApplicationWindowMode"
            | "DataLockControlMode"
            | "ObjectAutonumerationMode"
            | "ModalityUseMode"
            | "SynchronousPlatformExtensionAndAddInCallUseMode"
            | "InterfaceCompatibilityMode"
            | "DatabaseTablespacesUseMode"
            | "CompatibilityMode",
        ) => Some(Enumeration),
        ("Configuration", "DefaultLanguage") => Some(Reference("Language")),
        ("Configuration", "DefaultStyle") => Some(Reference("Style")),
        ("Configuration", "DefaultInterface") => Some(Reference("Interface")),
        (
            "Configuration",
            "DefaultReportForm"
            | "DefaultReportVariantForm"
            | "DefaultReportSettingsForm"
            | "DefaultDynamicListSettingsForm"
            | "DefaultSearchForm"
            | "DefaultDataHistoryChangeHistoryForm"
            | "DefaultDataHistoryVersionDataForm"
            | "DefaultDataHistoryVersionDifferencesForm"
            | "DefaultCollaborationSystemUsersChoiceForm",
        ) => Some(Reference("Form")),
        ("Configuration", "DefaultReportAppearanceTemplate") => Some(Reference("Template")),
        ("Configuration", "DefaultRoles") => Some(RoleList),
        (
            "Configuration",
            "NamePrefix"
            | "AdditionalFullTextSearchDictionaries"
            | "CommonSettingsStorage"
            | "ReportsUserSettingsStorage"
            | "ReportsVariantsStorage"
            | "FormDataSettingsStorage"
            | "DynamicListsUserSettingsStorage"
            | "URLExternalDataStorage"
            | "Content"
            | "RequiredMobileApplicationPermissions"
            | "StandaloneConfigurationRestrictionRoles"
            | "MobileApplicationURLs"
            | "AllowedIncomingShareRequestTypes"
            | "DefaultConstantsForm",
        ) => Some(Empty),
        ("Configuration", "UsePurposes") => Some(UsePurposes),
        ("Configuration", "UsedMobileApplicationFunctionalities") => Some(MobileFunctionalities),
        _ => None,
    }
}

fn elements<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children().filter(|node| node.is_element())
}

fn container(node: roxmltree::Node<'_, '_>) -> bool {
    node.attributes().len() == 0
        && node
            .children()
            .filter(|child| child.is_text())
            .all(|child| child.text().is_none_or(|text| text.trim().is_empty()))
}

fn empty(node: roxmltree::Node<'_, '_>) -> bool {
    node.attributes().len() == 0
        && elements(node).next().is_none()
        && node.text().is_none_or(|text| text.trim().is_empty())
}

fn leaf(node: roxmltree::Node<'_, '_>) -> bool {
    node.attributes().len() == 0 && elements(node).next().is_none()
}

pub(super) fn identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|ch| ch.is_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

fn enumeration(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

impl MetadataXmlSource {
    pub(super) fn properties(
        &self,
        root: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) {
        for properties in root
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "Properties")
        {
            let Some(owner) = properties.parent_element() else {
                continue;
            };
            if properties.tag_name().namespace() != Some(MD)
                || owner.tag_name().namespace() != Some(MD)
            {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            let mut names = std::collections::BTreeSet::new();
            if properties.attributes().len() != 0 {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            }
            for property in elements(properties) {
                if !names.insert(property.tag_name().name())
                    || property.tag_name().namespace() != Some(MD)
                    || !self.property(owner.tag_name().name(), property, result)
                {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                }
            }
        }
    }

    fn property(
        &self,
        owner: &str,
        node: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) -> bool {
        match contract(owner, node.tag_name().name()) {
            Some(Contract::Text) => leaf(node),
            Some(Contract::LocalText) => Self::local_text(node),
            Some(Contract::Boolean) => leaf(node) && matches!(node.text(), Some("true" | "false")),
            Some(Contract::Number) => {
                leaf(node) && node.text().is_some_and(|text| text.parse::<u64>().is_ok())
            }
            Some(Contract::Enumeration) => leaf(node) && node.text().is_some_and(enumeration),
            Some(Contract::Type) => Self::type_description(node),
            Some(Contract::Reference(kind)) => self.property_reference(node, Some(kind), result),
            Some(Contract::FieldList) => {
                container(node)
                    && elements(node).all(|field| {
                        field.has_tag_name((READABLE, "Field"))
                            && self.property_reference(field, None, result)
                    })
            }
            Some(Contract::ReferenceList) => {
                container(node)
                    && elements(node).all(|item| {
                        item.has_tag_name((READABLE, "Item"))
                            && item.attributes().len() == 1
                            && qname_is(
                                item,
                                item.attribute((XSI, "type")),
                                READABLE,
                                "MDObjectRef",
                            )
                            && self
                                .value(item)
                                .and_then(|value| explicit_reference(&value))
                                .is_some()
                    })
            }
            Some(Contract::RoleList) => {
                container(node) && elements(node).all(|item| {
                    item.has_tag_name((READABLE, "Item"))
                        && item.attributes().len() == 1
                        && qname_is(item, item.attribute((XSI, "type")), READABLE, "MDObjectRef")
                        && self.value(item).and_then(|value| value.tokens()).is_some_and(|tokens| {
                            matches!(tokens.as_slice(), [kind, name] if kind.text == "Role" && identifier(&name.text))
                        })
                })
            }
            Some(Contract::Empty) => empty(node),
            Some(Contract::Primitive) => Self::primitive(node),
            Some(Contract::FillValue) => {
                Self::primitive(node) || self.design_time_reference(node, result)
            }
            Some(Contract::ChoiceParameters) => {
                let mut names = std::collections::BTreeSet::new();
                container(node)
                    && elements(node).all(|item| {
                        let children = elements(item).collect::<Vec<_>>();
                        item.has_tag_name((APP, "item"))
                            && item.attributes().len() == 1
                            && item.attribute("name").is_some_and(|name| {
                                name.split('.').all(identifier) && names.insert(name)
                            })
                            && element_content(item)
                            && children.len() == 1
                            && children[0].has_tag_name((APP, "value"))
                            && self.design_time_reference(children[0], result)
                    })
            }
            Some(Contract::StandardAttributes) => {
                container(node)
                    && elements(node).all(|attribute| {
                        attribute.has_tag_name((READABLE, "StandardAttribute"))
                            && attribute.attributes().len() == 1
                            && attribute.attribute("name").is_some_and(enumeration)
                            && elements(attribute).all(|property| {
                                property.tag_name().namespace() == Some(READABLE)
                                    && self.property("StandardAttribute", property, result)
                            })
                    })
            }
            Some(Contract::UsePurposes) => {
                container(node)
                    && elements(node).all(|value| {
                        value.has_tag_name((CORE, "Value"))
                            && elements(value).next().is_none()
                            && value.attributes().len() == 1
                            && qname_is(
                                value,
                                value.attribute((XSI, "type")),
                                APP,
                                "ApplicationUsePurpose",
                            )
                            && value.text().is_some_and(enumeration)
                    })
            }
            Some(Contract::MobileFunctionalities) => {
                container(node)
                    && elements(node).all(|item| {
                        let children = elements(item).collect::<Vec<_>>();
                        item.has_tag_name((APP, "functionality"))
                            && item.attributes().len() == 0
                            && children.len() == 2
                            && children[0].has_tag_name((APP, "functionality"))
                            && leaf(children[0])
                            && children[0].text().is_some_and(enumeration)
                            && children[1].has_tag_name((APP, "use"))
                            && leaf(children[1])
                            && matches!(children[1].text(), Some("true" | "false"))
                    })
            }
            None => false,
        }
    }

    fn local_text(node: roxmltree::Node<'_, '_>) -> bool {
        if leaf(node) {
            return true;
        }
        node.attributes().len() == 0
            && elements(node).all(|item| {
                let children = elements(item).collect::<Vec<_>>();
                item.has_tag_name((CORE, "item"))
                    && item.attributes().len() == 0
                    && children.len() == 2
                    && children[0].has_tag_name((CORE, "lang"))
                    && leaf(children[0])
                    && children[1].has_tag_name((CORE, "content"))
                    && leaf(children[1])
            })
    }

    fn primitive(node: roxmltree::Node<'_, '_>) -> bool {
        if elements(node).next().is_some() {
            return false;
        }
        if node.attributes().any(|attribute| {
            attribute.namespace() != Some(XSI) || !matches!(attribute.name(), "nil" | "type")
        }) {
            return false;
        }
        if let Some(nil) = node.attribute((XSI, "nil")) {
            if !matches!(nil, "true" | "false") {
                return false;
            }
        }
        if let Some(ty) = node.attribute((XSI, "type")) {
            return ["string", "decimal", "boolean", "dateTime"]
                .iter()
                .any(|kind| qname_is(node, Some(ty), XS, kind));
        }
        node.attribute((XSI, "nil")) == Some("true") && node.text().is_none()
    }

    fn design_time_reference(
        &self,
        node: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) -> bool {
        if node.attributes().len() != 1
            || !qname_is(node, node.attribute((XSI, "type")), READABLE, "DesignTimeRef")
        {
            return false;
        }
        let Some(tokens) = self.value(node).and_then(|value| value.tokens()) else { return false };
        if !tokens.iter().all(|token| identifier(&token.text)) {
            return false;
        }
        let reference = match tokens.as_slice() {
            [kind, name, empty] if kind.text == "Catalog" && empty.text == "EmptyRef" => {
                XmlMetadataReference {
                    anchor: XmlReferenceAnchor::Metadata {
                        kind: MdoType::Catalog,
                        name: name.clone(),
                    },
                    members: Vec::new(),
                    declared_member_kinds: Vec::new(),
                }
            }
            [kind, name, member_kind, value]
                if kind.text == "Enum" && member_kind.text == "EnumValue" =>
            {
                XmlMetadataReference {
                    anchor: XmlReferenceAnchor::Metadata {
                        kind: MdoType::Enum,
                        name: name.clone(),
                    },
                    members: vec![value.clone()],
                    declared_member_kinds: vec!["EnumValue".into()],
                }
            }
            _ => return false,
        };
        result.references.push(reference);
        true
    }

    fn type_description(node: roxmltree::Node<'_, '_>) -> bool {
        container(node)
            && elements(node).all(|child| {
                if !child.tag_name().namespace().is_some_and(|namespace| namespace == CORE)
                    || child.attributes().len() != 0
                {
                    return false;
                }
                match child.tag_name().name() {
                    "Type" => {
                        elements(child).next().is_none()
                            && crate::xml_parser::parse_xml_type_token(child).is_ok_and(|ty| {
                                !matches!(
                                    ty,
                                    AttributeType::UnknownNamed(_) | AttributeType::Unknown
                                )
                            })
                    }
                    "StringQualifiers"
                    | "NumberQualifiers"
                    | "DateQualifiers"
                    | "BinaryDataQualifiers" => elements(child).all(|qualifier| {
                        qualifier.tag_name().namespace() == Some(CORE)
                            && leaf(qualifier)
                            && matches!(
                                qualifier.tag_name().name(),
                                "Length"
                                    | "AllowedLength"
                                    | "Digits"
                                    | "FractionDigits"
                                    | "AllowedSign"
                                    | "DateFractions"
                            )
                    }),
                    _ => false,
                }
            })
    }

    fn property_reference(
        &self,
        node: roxmltree::Node<'_, '_>,
        expected: Option<&str>,
        result: &mut XmlMetadataReferences,
    ) -> bool {
        if empty(node) {
            return true;
        }
        if !leaf(node) {
            return false;
        }
        let Some(names) = self.value(node).and_then(|value| value.tokens()) else {
            return false;
        };
        if names.len() < 2
            || names.len() % 2 != 0
            || names.iter().any(|name| !identifier(&name.text))
        {
            return false;
        }
        let owner = &names[0].text;
        if owner.parse::<MdoType>().is_err()
            && !matches!(
                owner.as_str(),
                "Language"
                    | "SessionParameter"
                    | "CommonAttribute"
                    | "CommonForm"
                    | "CommonTemplate"
                    | "Style"
                    | "Interface"
                    | "SettingsStorage"
            )
        {
            return false;
        }
        if !names.iter().skip(2).step_by(2).all(|kind| {
            matches!(
                kind.text.as_str(),
                "Attribute"
                    | "TabularSection"
                    | "Dimension"
                    | "Resource"
                    | "EnumValue"
                    | "Form"
                    | "Template"
                    | "Command"
                    | "StandardAttribute"
                    | "StandardCommand"
            )
        }) {
            return false;
        }
        if let Some(kind) = expected {
            let actual = if names.len() == 2 {
                owner.as_str()
            } else {
                names[names.len() - 2].text.as_str()
            };
            if actual != kind
                && !matches!(
                    (actual, kind),
                    ("CommonForm", "Form") | ("CommonTemplate", "Template")
                )
            {
                return false;
            }
        }
        result.property_references.push(XmlPropertyReference { names });
        true
    }
}

fn qname_is(
    node: roxmltree::Node<'_, '_>,
    value: Option<&str>,
    namespace: &str,
    name: &str,
) -> bool {
    value.and_then(|value| value.split_once(':')).is_some_and(|(prefix, local)| {
        local == name && node.lookup_namespace_uri(Some(prefix)) == Some(namespace)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_default_roles_are_role_only_and_keep_the_existing_mapped_name_span() {
        let xml = include_str!("../../fixtures/rename/native-default-roles-configuration.xml");
        let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        let roles = facts
            .references
            .iter()
            .filter_map(|reference| match &reference.anchor {
                XmlReferenceAnchor::Metadata { kind: MdoType::Role, name }
                    if xml.as_bytes()[..name.range.start].ends_with(b"Role.") =>
                {
                    Some(name)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(roles.len(), 1);
        let name = roles[0];
        assert_eq!(&xml.as_bytes()[name.range.clone()], "ОсновнаяРоль".as_bytes());
        let mut after = xml.as_bytes().to_vec();
        after.splice(name.range.clone(), source.encode_name("ДругаяРоль"));
        assert_eq!(after, xml.replacen("Role.ОсновнаяРоль", "Role.ДругаяРоль", 1).as_bytes());
        let empty = include_str!("../../fixtures/rename/carrier-corpus/base/Configuration.xml");
        assert!(MetadataXmlSource::decode(empty.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .is_empty());
    }

    #[test]
    fn native_default_roles_refuse_other_kinds_untyped_values_and_malformed_items() {
        let xml = include_str!("../../fixtures/rename/native-default-roles-configuration.xml");
        for invalid in [
            xml.replace("Role.ОсновнаяРоль", "Catalog.ОсновнаяРоль"),
            xml.replace("Role.ОсновнаяРоль", "Role.ОсновнаяРоль.Attribute.Extra"),
            xml.replace("Role.ОсновнаяРоль", "arbitrary text"),
            xml.replace("Role.ОсновнаяРоль", "Role."),
            xml.replacen("xsi:type=\"xr:MDObjectRef\"", "xsi:type=\"xs:MDObjectRef\"", 1),
            xml.replacen("<xr:Item xsi:type=\"xr:MDObjectRef\">", "<xr:Item>", 1),
            xml.replacen(
                "<xr:Item xsi:type=\"xr:MDObjectRef\">",
                "<xr:Item xmlns:xr=\"urn:foreign\" xsi:type=\"xr:MDObjectRef\">",
                1,
            ),
            xml.replacen(
                "<xr:Item xsi:type=\"xr:MDObjectRef\">",
                "<xr:Item extra=\"unknown\" xsi:type=\"xr:MDObjectRef\">",
                1,
            ),
            xml.replacen(
                "<xr:Item xsi:type=\"xr:MDObjectRef\">",
                "<xr:Item xsi:type=\"xr:MDObjectRef\"><Nested/>",
                1,
            ),
        ] {
            let facts = MetadataXmlSource::decode(invalid.as_bytes()).unwrap().inspect(None);
            assert!(facts.problems.contains(&XmlReferenceProblem::UnknownReferenceContract));
        }
    }

    #[test]
    fn full_native_common_module_and_form_properties_are_complete() {
        for xml in [
            include_str!("../../fixtures/rename/common-contracts/module-server.xml"),
            include_str!("../../fixtures/rename/common-contracts/module-client.xml"),
            include_str!("../../fixtures/rename/common-contracts/form-call.xml"),
            include_str!("../../fixtures/rename/common-contracts/form-settings.xml"),
        ] {
            let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
            let facts = source.inspect(None);
            assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        }
    }

    #[test]
    fn common_descriptor_properties_do_not_accept_unknown_or_malformed_shapes() {
        let module = include_str!("../../fixtures/rename/common-contracts/module-server.xml");
        let form = include_str!("../../fixtures/rename/common-contracts/form-call.xml");
        for xml in [
            module.replacen("<Server>true</Server>", "<Server>perhaps</Server>", 1),
            module.replacen("<Global>false</Global>", "<Global><Nested/></Global>", 1),
            module.replacen(
                "<Server>true</Server>",
                "<Server xmlns=\"urn:foreign\">true</Server>",
                1,
            ),
            module.replacen(
                "<Privileged>false</Privileged>",
                "<Privileged>false</Privileged><UnknownProperty/>",
                1,
            ),
            form.replacen(
                "<IncludeHelpInContents>false</IncludeHelpInContents>",
                "<IncludeHelpInContents>unknown</IncludeHelpInContents>",
                1,
            ),
            form.replacen(
                "xsi:type=\"app:ApplicationUsePurpose\"",
                "xsi:type=\"xs:ApplicationUsePurpose\"",
                1,
            ),
            form.replacen("<UsePurposes>", "<UsePurposes><Unknown/>", 1),
            form.replacen(
                "<ExtendedPresentation/>",
                "<ExtendedPresentation><Unknown/></ExtendedPresentation>",
                1,
            ),
        ] {
            let facts = MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None);
            assert!(facts.problems.contains(&XmlReferenceProblem::UnknownReferenceContract));
        }
    }

    const NATIVE_DOCUMENT_TABLES: &str =
        include_str!("../../fixtures/rename/generated-types/document-tables.xml");

    #[test]
    fn native_design_time_references_have_separate_metadata_and_enum_value_spans() {
        let xml = NATIVE_DOCUMENT_TABLES
            .replace("Catalog.Контрагенты", "Catalog.Partners")
            .replace("Enum.ВидыТоваров.EnumValue.Товар", "Enum.NativeEnum.EnumValue.Item");
        let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        let mut ranges = Vec::new();
        let mut enum_values = 0;
        for reference in &facts.references {
            let XmlReferenceAnchor::Metadata { kind, name } = &reference.anchor else { continue };
            if *kind == MdoType::Catalog && name.text == "Partners" {
                ranges.push((name.range.clone(), "Buyers"));
            }
            if *kind == MdoType::Enum && name.text == "NativeEnum" {
                assert_eq!(reference.declared_member_kinds, ["EnumValue"]);
                assert_eq!(reference.members.len(), 1);
                assert_eq!(&xml.as_bytes()[reference.members[0].range.clone()], b"Item");
                ranges.push((reference.members[0].range.clone(), "RenamedItem"));
                enum_values += 1;
            }
        }
        assert_eq!(enum_values, 1);
        assert_eq!(ranges.len(), 3);
        ranges.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut after = xml.as_bytes().to_vec();
        for (range, name) in ranges {
            after.splice(range, source.encode_name(name));
        }
        let expected = xml
            .replace("Catalog.Partners", "Catalog.Buyers")
            .replace("Enum.NativeEnum.EnumValue.Item", "Enum.NativeEnum.EnumValue.RenamedItem");
        assert_eq!(after, expected.as_bytes());
    }

    #[test]
    fn native_design_time_values_and_choice_parameters_refuse_unknown_shapes() {
        for xml in [
            NATIVE_DOCUMENT_TABLES.replacen(
                "Catalog.Контрагенты.EmptyRef",
                "Document.Контрагенты.EmptyRef",
                1,
            ),
            NATIVE_DOCUMENT_TABLES.replacen(
                "Catalog.Контрагенты.EmptyRef",
                "Catalog.Контрагенты.Arbitrary",
                1,
            ),
            NATIVE_DOCUMENT_TABLES.replacen("Catalog.Контрагенты.EmptyRef", "arbitrary text", 1),
            NATIVE_DOCUMENT_TABLES.replacen(
                "xsi:type=\"xr:DesignTimeRef\"",
                "xsi:type=\"xs:DesignTimeRef\"",
                1,
            ),
            NATIVE_DOCUMENT_TABLES.replacen(
                "<FillValue xsi:type=\"xr:DesignTimeRef\">",
                "<FillValue xsi:type=\"xr:DesignTimeRef\" extra=\"unproved\">",
                1,
            ),
            NATIVE_DOCUMENT_TABLES
                .replace("Enum.ВидыТоваров.EnumValue.Товар", "Enum.ВидыТоваров.Attribute.Товар"),
            NATIVE_DOCUMENT_TABLES
                .replace("Enum.ВидыТоваров.EnumValue.Товар", "Catalog.ВидыТоваров.EnumValue.Товар"),
            NATIVE_DOCUMENT_TABLES.replace(
                "<app:item name=\"Отбор.Вид\">",
                "<app:item xmlns:app=\"urn:foreign\" name=\"Отбор.Вид\">",
            ),
            NATIVE_DOCUMENT_TABLES.replace(
                "<app:item name=\"Отбор.Вид\">",
                "<app:item name=\"Отбор.Вид\" extra=\"unproved\">",
            ),
            NATIVE_DOCUMENT_TABLES.replace(
                "<app:value xsi:type=\"xr:DesignTimeRef\">",
                "<app:value xsi:type=\"xr:DesignTimeRef\"><Nested/>",
            ),
        ] {
            let facts = MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None);
            assert!(facts.problems.contains(&XmlReferenceProblem::UnknownReferenceContract));
        }
    }

    #[test]
    fn native_document_exchange_report_and_table_property_shapes_are_attested() {
        let mut unknown = Vec::new();
        for xml in [
            include_str!("../../fixtures/rename/generated-types/document.xml"),
            include_str!("../../fixtures/rename/generated-types/document-tables.xml"),
            include_str!("../../fixtures/rename/generated-types/exchange-plan.xml"),
            include_str!("../../fixtures/rename/generated-types/report.xml"),
            include_str!("../../fixtures/rename/generated-types/catalog-tables.xml"),
        ] {
            let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
            let document = roxmltree::Document::parse(source.text()).unwrap();
            let mut facts = XmlMetadataReferences::default();
            for properties in
                document.descendants().filter(|node| node.has_tag_name((MD, "Properties")))
            {
                let owner = properties.parent_element().unwrap().tag_name().name();
                for property in elements(properties) {
                    if !source.property(owner, property, &mut facts) {
                        unknown.push(format!("{owner}.{}", property.tag_name().name()));
                    }
                }
            }
        }
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    const CATALOG: &str =
        include_str!("../../fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml");
    const COMMON: &str = include_str!(
        "../../fixtures/extension_common_attributes/base/CommonAttributes/AreaUse.xml"
    );

    fn inspect(xml: &str) -> XmlMetadataReferences {
        MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None)
    }

    #[test]
    fn native_reference_paths_keep_raw_spans_and_standard_field_kinds() {
        let facts = inspect(CATALOG);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.property_references.len(), 2);
        let reference = &facts.property_references[0];
        assert_eq!(
            reference.names.iter().map(|part| part.text.as_str()).collect::<Vec<_>>(),
            ["Catalog", "BaseCatalog", "StandardAttribute", "Description"]
        );
        assert_eq!(&CATALOG.as_bytes()[reference.names[1].range.clone()], b"BaseCatalog");
        let common = inspect(COMMON);
        assert!(common.problems.is_empty(), "{common:#?}");
        assert!(common
            .property_references
            .iter()
            .any(|reference| reference.names[0].text == "SessionParameter"));
    }

    #[test]
    fn unknown_properties_wrong_namespaces_and_unproven_filled_shapes_stay_incomplete() {
        for xml in [
            CATALOG.replacen(
                "<Comment/>",
                "<UnknownReference>Catalog.BaseCatalog</UnknownReference>",
                1,
            ),
            CATALOG.replacen(
                "<HierarchyType>HierarchyFoldersAndItems</HierarchyType>",
                "<HierarchyType>Catalog.BaseCatalog</HierarchyType>",
                1,
            ),
            CATALOG.replacen(
                "<Owners/>",
                "<Owners><Unknown>Catalog.BaseCatalog</Unknown></Owners>",
                1,
            ),
            COMMON.replacen("<DataSeparationUse>", "<DataSeparationUse xmlns='urn:unknown'>", 1),
            COMMON.replacen(
                "<ChoiceParameterLinks/>",
                "<ChoiceParameterLinks><Link>Catalog.BaseCatalog</Link></ChoiceParameterLinks>",
                1,
            ),
        ] {
            assert!(
                inspect(&xml).problems.contains(&XmlReferenceProblem::UnknownReferenceContract),
                "{xml}"
            );
        }
    }

    #[test]
    fn authoritative_nonreference_text_is_not_promoted_by_matching_a_metadata_name() {
        let xml = CATALOG.replacen("<Comment/>", "<Comment>Catalog.BaseCatalog</Comment>", 1)
            .replacen("<Synonym/>", "<Synonym><v8:item><v8:lang>en</v8:lang><v8:content>Catalog.BaseCatalog</v8:content></v8:item></Synonym>", 1);
        let before = inspect(CATALOG);
        let facts = inspect(&xml);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.property_references.len(), before.property_references.len());
    }
}

#[cfg(test)]
mod native_extension_tests {
    use super::*;

    #[test]
    fn native_extension_container_does_not_relax_user_declaration_adoption() {
        let xml = include_str!(
            "../../fixtures/extension_common_attributes/extensions/no-common/Configuration.xml"
        );
        assert!(metadata_declarations(xml).is_err());
        let facts = MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert!(facts.declarations.is_empty());
        assert_eq!(facts.container_identities.len(), 1);
        for bytes in [
            include_bytes!("../../fixtures/extension_common_attributes/extensions/no-common/Catalogs/BaseCatalog.xml").as_slice(),
            include_bytes!("../../fixtures/extension_common_attributes/extensions/no-common/Catalogs/NativeCatalog.xml").as_slice(),
            include_bytes!("../../fixtures/extension_common_attributes/extensions/no-common/InformationRegisters/BaseRegister.xml").as_slice(),
            include_bytes!("../../fixtures/extension_common_attributes/extensions/no-common/InformationRegisters/NativeRegister.xml").as_slice(),
        ] {
            let facts = MetadataXmlSource::decode(bytes).unwrap().inspect(None);
            assert!(facts.problems.is_empty(), "{facts:#?}");
        }
        let invalid = "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><Catalog uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Partners</Name><ObjectBelonging>Adopted</ObjectBelonging></Properties></Catalog></MetaDataObject>";
        assert!(metadata_declarations(invalid).is_err());
        assert!(!MetadataXmlSource::decode(invalid.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .is_empty());
    }

    #[test]
    fn native_configuration_container_rejects_unproven_namespaces_and_shapes() {
        let original = include_str!(
            "../../fixtures/extension_common_attributes/extensions/no-common/Configuration.xml"
        );
        for xml in [
            original.replacen("<Properties>", "<Properties xmlns='urn:foreign'>", 1),
            original.replacen("<InternalInfo>", "<InternalInfo unexpected='1'>", 1),
            original.replacen("<Properties>", "<UnknownSection/><Properties>", 1),
            original.replacen(
                "<Name>Issue379Extension</Name>",
                "<Name>Issue379Extension</Name><Name>Other</Name>",
                1,
            ),
        ] {
            let facts = MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None);
            assert!(
                facts.problems.contains(&XmlReferenceProblem::UnknownReferenceContract),
                "{facts:#?}"
            );
        }
    }
}
