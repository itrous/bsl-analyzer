//! Exact XML value spans for metadata references, without serializing a document again.

mod composition_expressions;
mod dump_info;
mod generated_types;
mod properties;

pub use dump_info::XmlDumpInfoEntry;

use std::ops::Range;

use sha2::{Digest, Sha256};

use crate::declaration_identity::{metadata_declarations, MetadataDeclaration};
use crate::xml_parser::helpers::find_child;
use crate::{AttributeType, FormAttribute, MdoType};

const MD: &str = "http://v8.1c.ru/8.3/MDClasses";
const FORM: &str = "http://v8.1c.ru/8.3/xcf/logform";
const CORE: &str = "http://v8.1c.ru/8.1/data/core";
const DCS: &str = "http://v8.1c.ru/8.1/data-composition-system/schema";
const EXTERNAL: &str = "http://v8.1c.ru/8.3/xcf/extrnprops";
const READABLE: &str = "http://v8.1c.ru/8.3/xcf/readable";
const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

pub fn metadata_source_fingerprint(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmlReferenceProblem {
    UnsupportedEncoding,
    MalformedXml,
    UnsupportedSurface,
    UnmappableValue,
    UnknownFormBinding,
    InheritedFormBindings,
    UnknownReferenceContract,
    DataSetOutputContract,
    GeneratedTypeContract,
}

/// Offsets always address the original bytes, including their original BOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlNameToken {
    pub text: String,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmlReferenceAnchor {
    Metadata { kind: MdoType, name: XmlNameToken },
    FormValue(FormAttribute),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlMetadataReference {
    pub anchor: XmlReferenceAnchor,
    pub members: Vec<XmlNameToken>,
    pub declared_member_kinds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDeclarationName {
    pub declaration: MetadataDeclaration,
    pub internal_object_ids: Vec<crate::Uuid>,
    pub name: XmlNameToken,
}

/// A configuration container is not a renameable adopted declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlContainerIdentity {
    pub uuid: crate::Uuid,
    pub name: XmlNameToken,
    pub internal_object_ids: Vec<crate::Uuid>,
}

/// A format-typed path can name kinds outside the semantic model's supported rename targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlPropertyReference {
    pub names: Vec<XmlNameToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDataSetField {
    pub query: usize,
    pub data_path: XmlQueryText,
    pub field: XmlQueryText,
}

/// A direct DCS field path needs the dataset's typed output before it denotes metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDataSetExpression {
    pub query: usize,
    pub path: XmlQueryText,
}

/// Its query is parsed by the normal SDBL reader; the XML mapper only translates ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlQueryText {
    pub text: String,
    spans: Vec<Range<usize>>,
}

impl XmlQueryText {
    pub fn original_range(&self, range: Range<usize>) -> Option<Range<usize>> {
        if range.is_empty()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return None;
        }
        Some(self.spans.get(range.start)?.start..self.spans.get(range.end - 1)?.end)
    }

    pub fn tokens(&self) -> Option<Vec<XmlNameToken>> {
        let mut start = 0;
        self.text
            .split('.')
            .map(|part| {
                let end = start + part.len();
                let token = XmlNameToken {
                    text: part.to_string(),
                    range: self.original_range(start..end)?,
                };
                start = end + 1;
                Some(token)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmlMetadataReferences {
    pub container_identities: Vec<XmlContainerIdentity>,
    pub dump_info: Vec<XmlDumpInfoEntry>,
    pub property_references: Vec<XmlPropertyReference>,
    pub declarations: Vec<XmlDeclarationName>,
    pub references: Vec<XmlMetadataReference>,
    pub queries: Vec<XmlQueryText>,
    pub data_set_fields: Vec<XmlDataSetField>,
    pub data_set_expressions: Vec<XmlDataSetExpression>,
    pub problems: Vec<XmlReferenceProblem>,
    pub form_bindings_required: bool,
}

/// The decoded text is immutable; every replacement is encoded into the original encoding.
#[derive(Debug, Clone)]
pub struct MetadataXmlSource {
    text: String,
    raw_offsets: Vec<usize>,
    pub encoding: XmlEncoding,
    pub sha256: [u8; 32],
}

impl MetadataXmlSource {
    pub fn decode(bytes: &[u8]) -> Result<Self, XmlReferenceProblem> {
        let (encoding, skip) = if bytes.starts_with(&[0xff, 0xfe]) {
            (XmlEncoding::Utf16Le, 2)
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            (XmlEncoding::Utf16Be, 2)
        } else {
            (XmlEncoding::Utf8, usize::from(bytes.starts_with(&[0xef, 0xbb, 0xbf])) * 3)
        };
        let text = match encoding {
            XmlEncoding::Utf8 => std::str::from_utf8(&bytes[skip..])
                .map_err(|_| XmlReferenceProblem::UnsupportedEncoding)?
                .to_string(),
            XmlEncoding::Utf16Le | XmlEncoding::Utf16Be => {
                let (pairs, remainder) = bytes[skip..].as_chunks::<2>();
                if !remainder.is_empty() {
                    return Err(XmlReferenceProblem::UnsupportedEncoding);
                }
                let units = pairs
                    .iter()
                    .map(|pair| {
                        if encoding == XmlEncoding::Utf16Le {
                            u16::from_le_bytes(*pair)
                        } else {
                            u16::from_be_bytes(*pair)
                        }
                    })
                    .collect::<Vec<_>>();
                String::from_utf16(&units).map_err(|_| XmlReferenceProblem::UnsupportedEncoding)?
            }
        };
        if let Some(declaration) = text.strip_prefix("<?xml") {
            let end = declaration.find("?>").ok_or(XmlReferenceProblem::MalformedXml)?;
            let wrapper = format!("<declaration{}/>", &declaration[..end]);
            let document = roxmltree::Document::parse(&wrapper)
                .map_err(|_| XmlReferenceProblem::MalformedXml)?;
            if let Some(declared) = document.root_element().attribute("encoding") {
                let supported = match encoding {
                    XmlEncoding::Utf8 => declared.eq_ignore_ascii_case("UTF-8"),
                    XmlEncoding::Utf16Le => {
                        declared.eq_ignore_ascii_case("UTF-16")
                            || declared.eq_ignore_ascii_case("UTF-16LE")
                    }
                    XmlEncoding::Utf16Be => {
                        declared.eq_ignore_ascii_case("UTF-16")
                            || declared.eq_ignore_ascii_case("UTF-16BE")
                    }
                };
                if !supported {
                    return Err(XmlReferenceProblem::UnsupportedEncoding);
                }
            }
        }
        let mut raw_offsets = Vec::with_capacity(text.len() + 1);
        let mut raw = skip;
        for character in text.chars() {
            raw_offsets.extend(std::iter::repeat_n(raw, character.len_utf8()));
            raw += match encoding {
                XmlEncoding::Utf8 => character.len_utf8(),
                XmlEncoding::Utf16Le | XmlEncoding::Utf16Be => character.len_utf16() * 2,
            };
        }
        raw_offsets.push(raw);
        Ok(Self { text, raw_offsets, encoding, sha256: metadata_source_fingerprint(bytes) })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_well_formed(&self) -> bool {
        roxmltree::Document::parse(&self.text).is_ok()
    }

    pub fn encode_name(&self, name: &str) -> Vec<u8> {
        match self.encoding {
            XmlEncoding::Utf8 => name.as_bytes().to_vec(),
            XmlEncoding::Utf16Le => name.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            XmlEncoding::Utf16Be => name.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        }
    }

    pub fn inspect(
        &self,
        effective_form_attributes: Option<&[FormAttribute]>,
    ) -> XmlMetadataReferences {
        let Ok(document) = roxmltree::Document::parse(&self.text) else {
            return XmlMetadataReferences {
                problems: vec![XmlReferenceProblem::MalformedXml],
                ..Default::default()
            };
        };
        let root = document.root_element();
        let mut result = XmlMetadataReferences::default();
        match (root.tag_name().namespace(), root.tag_name().name()) {
            (Some(MD), "MetaDataObject") => self.declarations(root, &mut result),
            (Some(dump_info::NAMESPACE), "ConfigDumpInfo") => self.dump_info(root, &mut result),
            (Some(FORM), "Form") => self.form(root, effective_form_attributes, &mut result),
            (Some(DCS), "DataCompositionSchema") => self.composition(root, &mut result),
            (None | Some(EXTERNAL), "ExchangePlanContent") => {
                self.exchange_content(root, &mut result)
            }
            (Some(EXTERNAL), "CommandInterface") => self.commands(root, &mut result),
            _ => result.problems.push(XmlReferenceProblem::UnsupportedSurface),
        }
        self.generated_types(root, &mut result);
        self.types(root, &mut result);
        self.typed_references(root, &mut result);
        result
    }

    pub fn raw_range(&self, range: Range<usize>) -> Option<Range<usize>> {
        if !self.text.is_char_boundary(range.start) || !self.text.is_char_boundary(range.end) {
            return None;
        }
        Some(*self.raw_offsets.get(range.start)?..*self.raw_offsets.get(range.end)?)
    }

    fn value(&self, node: roxmltree::Node<'_, '_>) -> Option<XmlQueryText> {
        let mut children = node.children().filter(|child| child.is_text());
        let child = children.next()?;
        if children.next().is_some() || node.children().any(|child| child.is_element()) {
            return None;
        }
        self.value_at(child.range(), child.text()?, false)
    }

    fn value_at(
        &self,
        mut range: Range<usize>,
        expected: &str,
        attribute: bool,
    ) -> Option<XmlQueryText> {
        let mut cdata = false;
        if self.text.get(range.clone())?.starts_with("<![CDATA[") {
            if !self.text.get(range.clone())?.ends_with("]]>") {
                return None;
            }
            range.start += 9;
            range.end -= 3;
            cdata = true;
        }
        let mut output = XmlQueryText { text: String::new(), spans: Vec::new() };
        let mut cursor = range.start;
        while cursor < range.end {
            let start = cursor;
            let tail = self.text.get(cursor..range.end)?;
            let (decoded, consumed) = if !cdata && tail.starts_with('&') {
                let end = tail.find(';')?;
                let entity = &tail[1..end];
                let character = match entity {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    _ => char::from_u32(if let Some(hex) = entity.strip_prefix("#x") {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        entity.strip_prefix('#')?.parse().ok()?
                    })?,
                };
                (character, end + 1)
            } else if tail.starts_with("\r\n") {
                (if attribute { ' ' } else { '\n' }, 2)
            } else {
                let character = tail.chars().next()?;
                (
                    match character {
                        '\r' => {
                            if attribute {
                                ' '
                            } else {
                                '\n'
                            }
                        }
                        '\n' | '\t' if attribute => ' ',
                        other => other,
                    },
                    character.len_utf8(),
                )
            };
            cursor += consumed;
            let raw = self.raw_range(start..cursor)?;
            output.text.push(decoded);
            output.spans.extend(std::iter::repeat_n(raw, decoded.len_utf8()));
        }
        (output.text == expected).then_some(output)
    }

    fn declarations(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        self.properties(root, result);
        let containers = root.children().filter(|node| node.is_element()).collect::<Vec<_>>();
        if let [configuration] = containers.as_slice() {
            if configuration.has_tag_name((MD, "Configuration")) {
                let mut sections = std::collections::BTreeSet::new();
                if configuration
                    .children()
                    .filter(|node| node.is_text())
                    .any(|node| node.text().is_some_and(|text| !text.trim().is_empty()))
                {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                }
                let shape = root.attributes().all(|attribute| {
                    attribute.namespace().is_none()
                        && attribute.name() == "version"
                        && attribute.value() == "2.20"
                }) && configuration
                    .attributes()
                    .all(|attribute| attribute.namespace().is_none() && attribute.name() == "uuid")
                    && configuration.children().filter(|node| node.is_element()).all(|node| {
                        node.tag_name().namespace() == Some(MD)
                            && matches!(
                                node.tag_name().name(),
                                "Properties" | "InternalInfo" | "ChildObjects"
                            )
                            && sections.insert(node.tag_name().name())
                    });
                if !shape {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                }
                if let Some(internal) = find_child(*configuration, "InternalInfo") {
                    if internal
                        .children()
                        .filter(|node| node.is_text())
                        .any(|node| node.text().is_some_and(|text| !text.trim().is_empty()))
                        || internal.attributes().len() != 0
                        || internal
                            .children()
                            .filter(|node| node.is_element())
                            .any(|node| !node.has_tag_name((READABLE, "ContainedObject")))
                    {
                        result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    }
                }
                if let Some(children) = find_child(*configuration, "ChildObjects") {
                    if children
                        .children()
                        .filter(|node| node.is_text())
                        .any(|node| node.text().is_some_and(|text| !text.trim().is_empty()))
                        || children.attributes().len() != 0
                        || children.children().filter(|node| node.is_element()).any(|node| {
                            node.tag_name().namespace() != Some(MD)
                                || node.attributes().len() != 0
                                || node.children().any(|child| child.is_element())
                                || !(node.tag_name().name().parse::<MdoType>().is_ok()
                                    || matches!(
                                        node.tag_name().name(),
                                        "CommonAttribute"
                                            | "CommonForm"
                                            | "SessionParameter"
                                            | "Language"
                                    ))
                                || !node.text().is_some_and(properties::identifier)
                        })
                    {
                        result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    }
                }
                let name = find_child(*configuration, "Properties")
                    .filter(|properties| properties.has_tag_name((MD, "Properties")))
                    .and_then(|properties| find_child(properties, "Name"))
                    .filter(|name| name.has_tag_name((MD, "Name")))
                    .and_then(|name| self.value(name));
                let uuid = configuration
                    .attribute("uuid")
                    .and_then(|id| id.parse::<crate::Uuid>().ok())
                    .filter(|id| !id.is_nil());
                if let Some((name, uuid)) =
                    name.filter(|name| properties::identifier(&name.text)).zip(uuid)
                {
                    if let Some(range) = name.original_range(0..name.text.len()) {
                        let internal_object_ids = self.internal_object_ids(*configuration, result);
                        result.container_identities.push(XmlContainerIdentity {
                            uuid,
                            name: XmlNameToken { text: name.text, range },
                            internal_object_ids,
                        });
                    } else {
                        result.problems.push(XmlReferenceProblem::UnmappableValue);
                    }
                } else {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                }
                self.configuration_children(root, result);
                return;
            }
        }
        let Ok(declarations) = metadata_declarations(&self.text) else {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        };
        for declaration in declarations {
            let Some(node) = root.descendants().find(|node| {
                node.attribute("uuid").and_then(|uuid| uuid.parse::<crate::Uuid>().ok())
                    == Some(declaration.uuid)
            }) else {
                continue;
            };
            let name = find_child(node, "Properties").and_then(|props| find_child(props, "Name"));
            let Some(value) = name.and_then(|name| self.value(name)) else {
                result.problems.push(XmlReferenceProblem::UnmappableValue);
                continue;
            };
            let Some(range) = value.original_range(0..value.text.len()) else {
                continue;
            };
            let internal_object_ids = self.internal_object_ids(node, result);
            result.declarations.push(XmlDeclarationName {
                declaration,
                internal_object_ids,
                name: XmlNameToken { text: value.text, range },
            });
        }
        self.configuration_children(root, result);
    }

    fn internal_object_ids(
        &self,
        node: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) -> Vec<crate::Uuid> {
        let mut internal_object_ids = Vec::new();
        if let Some(internal) = find_child(node, "InternalInfo") {
            for object in internal
                .children()
                .filter(|child| child.has_tag_name((READABLE, "ContainedObject")))
            {
                if object.attributes().len() != 0
                    || object.children().filter(|child| child.is_element()).count() != 2
                {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    continue;
                }
                let id = object
                    .children()
                    .find(|child| child.has_tag_name((READABLE, "ObjectId")))
                    .filter(|id| {
                        id.attributes().len() == 0 && !id.children().any(|child| child.is_element())
                    })
                    .and_then(|id| id.text())
                    .and_then(|id| id.parse::<crate::Uuid>().ok());
                let class = object
                    .children()
                    .find(|child| child.has_tag_name((READABLE, "ClassId")))
                    .filter(|id| {
                        id.attributes().len() == 0 && !id.children().any(|child| child.is_element())
                    })
                    .and_then(|id| id.text())
                    .and_then(|id| id.parse::<crate::Uuid>().ok());
                match id.zip(class).filter(|(id, class)| !id.is_nil() && !class.is_nil()) {
                    Some((id, _)) if !internal_object_ids.contains(&id) => {
                        internal_object_ids.push(id)
                    }
                    _ => result.problems.push(XmlReferenceProblem::UnknownReferenceContract),
                }
            }
        }
        internal_object_ids
    }

    fn configuration_children(
        &self,
        root: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) {
        for list in root.descendants().filter(|node| {
            node.is_element()
                && node.tag_name().namespace() == Some(MD)
                && node.tag_name().name() == "Configuration"
        }) {
            if let Some(children) = find_child(list, "ChildObjects") {
                for child in children.children().filter(|node| node.is_element()) {
                    let Ok(kind) = child.tag_name().name().parse::<MdoType>() else {
                        continue;
                    };
                    let Some(value) = self.value(child) else {
                        result.problems.push(XmlReferenceProblem::UnmappableValue);
                        continue;
                    };
                    let Some(range) = value.original_range(0..value.text.len()) else {
                        continue;
                    };
                    result.references.push(XmlMetadataReference {
                        anchor: XmlReferenceAnchor::Metadata {
                            kind,
                            name: XmlNameToken { text: value.text, range },
                        },
                        members: Vec::new(),
                        declared_member_kinds: Vec::new(),
                    });
                }
            }
        }
    }

    fn explicit_metadata(&self, node: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        match self.value(node).and_then(|value| explicit_reference(&value)) {
            Some(reference) => result.references.push(reference),
            None => result.problems.push(XmlReferenceProblem::UnknownReferenceContract),
        }
    }

    fn form(
        &self,
        root: roxmltree::Node<'_, '_>,
        attributes: Option<&[FormAttribute]>,
        result: &mut XmlMetadataReferences,
    ) {
        result.form_bindings_required = true;
        let parsed = crate::xml_parser::parse_form_xml(&self.text).ok();
        let inherited = find_child(root, "BaseForm").is_some();
        if inherited && attributes.is_none() {
            result.problems.push(XmlReferenceProblem::InheritedFormBindings);
        }
        let Some(attributes) = attributes.or_else(|| parsed.as_ref().map(|form| form.attributes()))
        else {
            result.problems.push(XmlReferenceProblem::UnknownFormBinding);
            return;
        };
        for node in root.descendants().filter(|node| {
            node.is_element()
                && node.tag_name().namespace() == Some(FORM)
                && node.tag_name().name() == "DataPath"
        }) {
            let Some(tokens) = self.value(node).and_then(|value| value.tokens()) else {
                result.problems.push(XmlReferenceProblem::UnmappableValue);
                continue;
            };
            let Some(head) = tokens.first() else { continue };
            let matches = attributes
                .iter()
                .filter(|attribute| stdx::case::eq_ignore_case(&attribute.name, &head.text))
                .collect::<Vec<_>>();
            let [attribute] = matches.as_slice() else {
                result.problems.push(XmlReferenceProblem::UnknownFormBinding);
                continue;
            };
            result.references.push(XmlMetadataReference {
                anchor: XmlReferenceAnchor::FormValue((*attribute).clone()),
                members: tokens[1..].to_vec(),
                declared_member_kinds: Vec::new(),
            });
        }
    }

    fn composition(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        if crate::xml_parser::content_inspection::parse_composition_inspection(&self.text, "")
            .is_err()
        {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        }
        for node in root
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().namespace() == Some(DCS))
        {
            if node.tag_name().name() == "query" {
                if let Some(query) = self.value(node) {
                    let local = node
                        .parent_element()
                        .and_then(|dataset| find_child(dataset, "dataSource"))
                        .and_then(|source| source.text())
                        .is_some_and(|source| {
                            root.children()
                                .filter(|child| {
                                    child.is_element() && child.tag_name().name() == "dataSource"
                                })
                                .any(|candidate| {
                                    find_child(candidate, "name").and_then(|name| name.text())
                                        == Some(source)
                                        && find_child(candidate, "dataSourceType")
                                            .and_then(|kind| kind.text())
                                            == Some("Local")
                                })
                        });
                    if !local {
                        result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                        continue;
                    }
                    let index = result.queries.len();
                    result.queries.push(query);
                    if let Some(dataset) = node.parent_element() {
                        for field in dataset.children().filter(|child| {
                            child.is_element() && child.tag_name().name() == "field"
                        }) {
                            let values = find_child(field, "dataPath")
                                .and_then(|value| self.value(value))
                                .zip(
                                    find_child(field, "field").and_then(|value| self.value(value)),
                                );
                            if let Some((data_path, field)) = values {
                                result.data_set_fields.push(XmlDataSetField {
                                    query: index,
                                    data_path,
                                    field,
                                });
                                result.problems.push(XmlReferenceProblem::DataSetOutputContract);
                            } else {
                                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                            }
                        }
                    }
                } else {
                    result.problems.push(XmlReferenceProblem::UnmappableValue);
                }
            }
        }
        self.composition_expressions(root, result);
    }

    fn exchange_content(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        if !external_root_attributes(root) || !element_content(root) {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        }
        for item in root.children().filter(|node| node.is_element()) {
            let mut fields = std::collections::BTreeSet::new();
            let shape = item.tag_name().namespace() == root.tag_name().namespace()
                && item.tag_name().name() == "Item"
                && item.attributes().len() == 0
                && element_content(item)
                && item.children().filter(|node| node.is_element()).all(|field| {
                    field.tag_name().namespace() == root.tag_name().namespace()
                        && fields.insert(field.tag_name().name())
                        && scalar_content(field)
                        && match field.tag_name().name() {
                            "Metadata" => true,
                            "AutoRecord" => matches!(field.text(), Some("Allow" | "Deny")),
                            _ => false,
                        }
                })
                && fields.contains("Metadata");
            if !shape {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            if let Some(metadata) = find_child(item, "Metadata") {
                self.explicit_metadata(metadata, result);
            }
        }
    }

    fn commands(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        if !external_root_attributes(root) || !element_content(root) {
            result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            return;
        }
        for section in root.children().filter(|node| node.is_element()) {
            if section.tag_name().namespace() != root.tag_name().namespace()
                || !matches!(section.tag_name().name(), "CommandsPlacement" | "CommandsOrder")
                || section.attributes().len() != 0
                || !element_content(section)
            {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                continue;
            }
            for command in section.children().filter(|node| node.is_element()) {
                let mut fields = std::collections::BTreeSet::new();
                let shape = command.tag_name().namespace() == root.tag_name().namespace()
                    && command.tag_name().name() == "Command"
                    && command.attributes().len() == 1
                    && element_content(command)
                    && command.children().filter(|node| node.is_element()).all(|field| {
                        field.tag_name().namespace() == root.tag_name().namespace()
                            && fields.insert(field.tag_name().name())
                            && scalar_content(field)
                            && match field.tag_name().name() {
                                "CommandGroup" => field.text().is_some_and(|text| {
                                    let mut parts = text.split('.');
                                    let Some(first) = parts.next() else { return false };
                                    if first == "CommandGroup" {
                                        parts.next().is_some_and(properties::identifier)
                                            && parts.next().is_none()
                                    } else {
                                        properties::identifier(first) && parts.next().is_none()
                                    }
                                }),
                                "Placement" => field.text().is_some_and(properties::identifier),
                                _ => false,
                            }
                    });
                if !shape {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    continue;
                }
                let reference = command
                    .attributes()
                    .find(|attribute| attribute.namespace().is_none() && attribute.name() == "name")
                    .and_then(|attribute| {
                        self.value_at(attribute.range_value(), attribute.value(), true)
                    })
                    .and_then(|value| explicit_reference(&value));
                match reference {
                    Some(reference) => result.references.push(reference),
                    None => result.problems.push(XmlReferenceProblem::UnknownReferenceContract),
                }
            }
        }
    }

    fn types(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        for node in
            root.descendants().filter(|node| node.is_element() && node.tag_name().name() == "Type")
        {
            let Some(value) = self.value(node) else {
                continue;
            };
            if node.tag_name().namespace() != Some(CORE) {
                continue;
            }
            let Ok(ty) = crate::xml_parser::parse_xml_type_token(node) else {
                continue;
            };
            let owner = match ty {
                AttributeType::Ref { mdo_type, name } => Some((mdo_type, name)),
                AttributeType::InformationRegisterRecordManager { name } => {
                    Some((MdoType::InformationRegister, name))
                }
                AttributeType::UnknownNamed(name) if name.starts_with("cfg:") => {
                    result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
                    None
                }
                _ => None,
            };
            let Some((kind, name)) = owner else { continue };
            let Some((_, token)) = value.text.rsplit_once('.') else {
                continue;
            };
            if token != name {
                continue;
            }
            let start = value.text.len() - name.len();
            if let Some(range) = value.original_range(start..value.text.len()) {
                result.references.push(XmlMetadataReference {
                    anchor: XmlReferenceAnchor::Metadata {
                        kind,
                        name: XmlNameToken { text: name, range },
                    },
                    members: Vec::new(),
                    declared_member_kinds: Vec::new(),
                });
            }
        }
    }

    fn typed_references(&self, root: roxmltree::Node<'_, '_>, result: &mut XmlMetadataReferences) {
        for node in root.descendants().filter(|node| node.is_element()) {
            let Some(type_name) = node.attribute((XSI, "type")) else {
                continue;
            };
            let Some((prefix, local)) = type_name.split_once(':') else {
                continue;
            };
            if node.lookup_namespace_uri(Some(prefix)) != Some(READABLE) || local != "MDObjectRef" {
                continue;
            }
            let reference = self.value(node).and_then(|value| {
                let tokens = value.tokens()?;
                let kind = tokens.first()?.text.parse::<MdoType>().ok()?;
                let name = tokens.get(1)?.clone();
                let tail = tokens.get(2..)?;
                if tail.len() % 2 != 0 {
                    return None;
                }
                let mut members = Vec::new();
                let mut kinds = Vec::new();
                for pair in tail.as_chunks::<2>().0 {
                    if !matches!(
                        pair[0].text.as_str(),
                        "Attribute" | "TabularSection" | "Dimension" | "Resource" | "EnumValue"
                    ) {
                        return None;
                    }
                    kinds.push(pair[0].text.clone());
                    members.push(pair[1].clone());
                }
                Some(XmlMetadataReference {
                    anchor: XmlReferenceAnchor::Metadata { kind, name },
                    members,
                    declared_member_kinds: kinds,
                })
            });
            match reference {
                Some(reference) => result.references.push(reference),
                None => result.problems.push(XmlReferenceProblem::UnknownReferenceContract),
            }
        }
    }
}

fn element_content(node: roxmltree::Node<'_, '_>) -> bool {
    node.children()
        .filter(|child| child.is_text())
        .all(|child| child.text().is_none_or(|text| text.trim().is_empty()))
}

fn external_root_attributes(root: roxmltree::Node<'_, '_>) -> bool {
    root.attributes().len() == 0
        || (root.tag_name().namespace() == Some(EXTERNAL)
            && root.attributes().len() == 1
            && root.attributes().all(|attribute| {
                attribute.namespace().is_none()
                    && attribute.name() == "version"
                    && attribute.value() == "2.20"
            }))
}

fn scalar_content(node: roxmltree::Node<'_, '_>) -> bool {
    node.attributes().len() == 0 && !node.children().any(|child| child.is_element())
}

fn explicit_reference(value: &XmlQueryText) -> Option<XmlMetadataReference> {
    let tokens = value.tokens()?;
    let kind = tokens.first()?.text.parse::<MdoType>().ok()?;
    let name = tokens.get(1)?.clone();
    let tail = tokens.get(2..)?;
    if !tail.is_empty()
        && !(tail.len() == 2 && matches!(tail[0].text.as_str(), "StandardCommand" | "Command"))
    {
        return None;
    }
    Some(XmlMetadataReference {
        anchor: XmlReferenceAnchor::Metadata { kind, name },
        members: Vec::new(),
        declared_member_kinds: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_xml_in_the_complete_historical_carrier_has_a_known_reader_contract() {
        let sources: &[(&str, &[u8])] = &[
            ("base/Catalogs/BA023Catalog.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Catalogs/BA023Catalog.xml").as_slice()),
            ("base/Catalogs/BA252Other.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Catalogs/BA252Other.xml").as_slice()),
            ("base/Catalogs/BA252Partners/Templates/BA252Layout/Ext/Template.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Catalogs/BA252Partners/Templates/BA252Layout/Ext/Template.xml").as_slice()),
            ("base/Catalogs/BA252Partners/Templates/BA252Layout.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Catalogs/BA252Partners/Templates/BA252Layout.xml").as_slice()),
            ("base/Catalogs/BA252Partners.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Catalogs/BA252Partners.xml").as_slice()),
            ("base/CommonForms/BA023Managed/Ext/Form.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonForms/BA023Managed/Ext/Form.xml").as_slice()),
            ("base/CommonForms/BA023Managed.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonForms/BA023Managed.xml").as_slice()),
            ("base/CommonForms/BA023Ordinary.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonForms/BA023Ordinary.xml").as_slice()),
            ("base/CommonForms/BA252Card/Ext/Form.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonForms/BA252Card/Ext/Form.xml").as_slice()),
            ("base/CommonForms/BA252Card.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonForms/BA252Card.xml").as_slice()),
            ("base/CommonModules/BA023Common.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonModules/BA023Common.xml").as_slice()),
            ("base/CommonModules/BA252References.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/CommonModules/BA252References.xml").as_slice()),
            ("base/ConfigDumpInfo.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/ConfigDumpInfo.xml").as_slice()),
            ("base/Configuration.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Configuration.xml").as_slice()),
            ("base/Documents/BA095Document.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Documents/BA095Document.xml").as_slice()),
            ("base/ExchangePlans/BA252Exchange/Ext/Content.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/ExchangePlans/BA252Exchange/Ext/Content.xml").as_slice()),
            ("base/ExchangePlans/BA252Exchange.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/ExchangePlans/BA252Exchange.xml").as_slice()),
            ("base/Ext/CommandInterface.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Ext/CommandInterface.xml").as_slice()),
            ("base/Languages/English.xml", include_bytes!("../fixtures/rename/carrier-corpus/base/Languages/English.xml").as_slice()),
            ("extension/Catalogs/BA023Catalog.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/Catalogs/BA023Catalog.xml").as_slice()),
            ("extension/Catalogs/BA252Partners.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/Catalogs/BA252Partners.xml").as_slice()),
            ("extension/CommonForms/BA023_ExtManaged/Ext/Form.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonForms/BA023_ExtManaged/Ext/Form.xml").as_slice()),
            ("extension/CommonForms/BA023_ExtManaged.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonForms/BA023_ExtManaged.xml").as_slice()),
            ("extension/CommonForms/BA023_ExtOrdinary.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonForms/BA023_ExtOrdinary.xml").as_slice()),
            ("extension/CommonForms/BA252OverlayCard/Ext/Form.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonForms/BA252OverlayCard/Ext/Form.xml").as_slice()),
            ("extension/CommonForms/BA252OverlayCard.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonForms/BA252OverlayCard.xml").as_slice()),
            ("extension/CommonModules/BA023_Extra.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonModules/BA023_Extra.xml").as_slice()),
            ("extension/CommonModules/BA252OverlayReferences.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/CommonModules/BA252OverlayReferences.xml").as_slice()),
            ("extension/ConfigDumpInfo.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/ConfigDumpInfo.xml").as_slice()),
            ("extension/Configuration.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/Configuration.xml").as_slice()),
            ("extension/Documents/BA095Document.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/Documents/BA095Document.xml").as_slice()),
            ("extension/Roles/ОсновнаяРоль.xml", include_bytes!("../fixtures/rename/carrier-corpus/extension/Roles/ОсновнаяРоль.xml").as_slice()),
        ];
        assert_eq!(sources.len(), 32);
        let mut blocking = Vec::new();
        let mut findings = Vec::new();
        let mut contextual = 0;
        let mut contextual_fields = Vec::new();
        for (path, bytes) in sources {
            let facts = MetadataXmlSource::decode(bytes).unwrap().inspect(None);
            findings.push((*path, facts.problems.clone()));
            if !facts.data_set_fields.is_empty() {
                contextual_fields
                    .extend(facts.data_set_fields.iter().map(|field| field.data_path.text.clone()));
            }
            let reasons = facts
                .problems
                .into_iter()
                .filter(|reason| {
                    if *reason == XmlReferenceProblem::DataSetOutputContract {
                        assert_eq!(
                            *path,
                            "base/Catalogs/BA252Partners/Templates/BA252Layout/Ext/Template.xml"
                        );
                        contextual += 1;
                        false
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>();
            if !reasons.is_empty() {
                blocking.push((*path, reasons));
            }
        }
        assert!(blocking.is_empty(), "blocking={blocking:#?}; all_XML_findings={findings:#?}");
        let expected_contextual_fields = ["StableCaption", "Partner.Caption", "Other.Caption"];
        assert_eq!(contextual_fields, expected_contextual_fields, "blocking={blocking:?}");
        assert_eq!(contextual, expected_contextual_fields.len(), "blocking={blocking:?}");
    }

    #[test]
    fn native_external_root_versions_keep_owned_reference_spans() {
        for (xml, expected) in [
            (include_str!("../fixtures/rename/full-corpus/base-additions/Ext/CommandInterface.xml"), 4),
            (include_str!("../fixtures/rename/full-corpus/base-additions/ExchangePlans/BA252Exchange/Ext/Content.xml"), 2),
        ] {
            for source in [xml.to_string(), xml.replacen(" version=\"2.20\"", "", 1)] {
                let facts = MetadataXmlSource::decode(source.as_bytes()).unwrap().inspect(None);
                assert!(facts.problems.is_empty(), "{:?}", facts.problems);
                assert_eq!(facts.references.len(), expected);
                for reference in facts.references {
                    let XmlReferenceAnchor::Metadata { kind, name } = reference.anchor else { panic!("metadata owner required") };
                    assert_eq!(kind, MdoType::Catalog);
                    assert_eq!(&source.as_bytes()[name.range], name.text.as_bytes());
                }
            }
            for invalid in [
                xml.replacen("version=\"2.20\"", "version=\"2.21\"", 1),
                xml.replacen("version=\"2.20\"", "xmlns:f=\"urn:foreign\" f:version=\"2.20\"", 1),
                xml.replacen("version=\"2.20\"", "version=\"2.20\" extra=\"unknown\"", 1),
                xml.replacen("http://v8.1c.ru/8.3/xcf/extrnprops", "urn:foreign", 1),
            ] {
                assert!(!MetadataXmlSource::decode(invalid.as_bytes()).unwrap().inspect(None).problems.is_empty());
            }
        }
    }

    #[test]
    fn native_configuration_common_form_child_is_known_without_admitting_unknown_classes() {
        let xml = include_str!("../fixtures/rename/full-corpus/extension/Configuration.xml");
        assert!(MetadataXmlSource::decode(xml.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .is_empty());
        for invalid in [
            xml.replace(
                "<CommonForm>BA252OverlayCard</CommonForm>",
                "<UnknownClass>BA252OverlayCard</UnknownClass>",
            ),
            xml.replace("<CommonForm>", "<CommonForm xmlns=\"urn:foreign\">"),
            xml.replace("<CommonForm>BA252OverlayCard", "<CommonForm><Nested/>BA252OverlayCard"),
        ] {
            assert!(MetadataXmlSource::decode(invalid.as_bytes())
                .unwrap()
                .inspect(None)
                .problems
                .contains(&XmlReferenceProblem::UnknownReferenceContract));
        }
    }

    fn replace(bytes: &[u8], range: Range<usize>, replacement: &[u8]) -> Vec<u8> {
        [bytes[..range.start].to_vec(), replacement.to_vec(), bytes[range.end..].to_vec()].concat()
    }

    #[test]
    fn native_export_properties_have_complete_known_contracts() {
        let files: &[(&str, &[u8])] = &[
            (
                "Catalogs/BaseCatalog.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml")
                    .as_slice(),
            ),
            (
                "CommonAttributes/AreaNoAuto.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/CommonAttributes/AreaNoAuto.xml")
                    .as_slice(),
            ),
            (
                "CommonAttributes/AreaUse.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/CommonAttributes/AreaUse.xml")
                    .as_slice(),
            ),
            (
                "CommonAttributes/OrdinaryAuto.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/CommonAttributes/OrdinaryAuto.xml"
                )
                .as_slice(),
            ),
            (
                "CommonAttributes/OtherNoAuto.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/CommonAttributes/OtherNoAuto.xml")
                    .as_slice(),
            ),
            (
                "CommonAttributes/OtherUse.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/CommonAttributes/OtherUse.xml")
                    .as_slice(),
            ),
            (
                "ConfigDumpInfo.xml",
                include_bytes!("../fixtures/rename/native-config-dump-info.xml").as_slice(),
            ),
            (
                "Configuration.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/Configuration.xml").as_slice(),
            ),
            (
                "InformationRegisters/BaseRegister.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/InformationRegisters/BaseRegister.xml"
                )
                .as_slice(),
            ),
            (
                "Languages/English.xml",
                include_bytes!("../fixtures/extension_common_attributes/base/Languages/English.xml").as_slice(),
            ),
            (
                "SessionParameters/AreaNoAutoValue.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/SessionParameters/AreaNoAutoValue.xml"
                )
                .as_slice(),
            ),
            (
                "SessionParameters/AreaUseValue.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/SessionParameters/AreaUseValue.xml"
                )
                .as_slice(),
            ),
            (
                "SessionParameters/OtherNoAutoValue.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/SessionParameters/OtherNoAutoValue.xml"
                )
                .as_slice(),
            ),
            (
                "SessionParameters/OtherUseValue.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/SessionParameters/OtherUseValue.xml"
                )
                .as_slice(),
            ),
            (
                "SessionParameters/UseSeparation.xml",
                include_bytes!(
                    "../fixtures/extension_common_attributes/base/SessionParameters/UseSeparation.xml"
                )
                .as_slice(),
            ),
        ];
        let provenance: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/rename/native-common-provenance.json"))
                .unwrap();
        for (path, bytes) in files {
            let hash = metadata_source_fingerprint(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(hash, provenance["files"][path]["sha256"].as_str().unwrap(), "{path}");
            let facts = MetadataXmlSource::decode(bytes).unwrap().inspect(None);
            assert!(facts.problems.is_empty(), "{path}: {:#?}", facts.problems);
        }
    }

    #[test]
    fn xml_declaration_replacement_preserves_uuid_bom_crlf_and_unrelated_bytes() {
        let xml = include_str!("../fixtures/rename/adopted.xml").replace('\n', "\r\n");
        for encoding in [XmlEncoding::Utf8, XmlEncoding::Utf16Le, XmlEncoding::Utf16Be] {
            let bytes = match encoding {
                XmlEncoding::Utf8 => [vec![0xef, 0xbb, 0xbf], xml.as_bytes().to_vec()].concat(),
                XmlEncoding::Utf16Le => {
                    [vec![0xff, 0xfe], xml.encode_utf16().flat_map(u16::to_le_bytes).collect()]
                        .concat()
                }
                XmlEncoding::Utf16Be => {
                    [vec![0xfe, 0xff], xml.encode_utf16().flat_map(u16::to_be_bytes).collect()]
                        .concat()
                }
            };
            let source = MetadataXmlSource::decode(&bytes).unwrap();
            let facts = source.inspect(None);
            assert!(facts.problems.is_empty(), "{facts:#?}");
            let name =
                &facts.declarations.iter().find(|item| item.name.text == "Caption").unwrap().name;
            let changed = replace(&bytes, name.range.clone(), &source.encode_name("Заголовок"));
            let decoded = MetadataXmlSource::decode(&changed).unwrap();
            assert_eq!(
                decoded.text(),
                xml.replace("<Name>Caption</Name>", "<Name>Заголовок</Name>")
            );
            assert_eq!(source.encoding, decoded.encoding);
            assert_eq!(&changed[..name.range.start], &bytes[..name.range.start]);
            assert!(changed.ends_with(&bytes[name.range.end..]));
            assert_eq!(
                source.inspect(None).declarations[0].declaration.uuid,
                decoded.inspect(None).declarations[0].declaration.uuid
            );
        }
    }

    #[test]
    fn managed_paths_keep_the_typed_anchor_and_ignore_ui_names() {
        let bytes = include_bytes!("../fixtures/rename/managed.xml");
        let facts = MetadataXmlSource::decode(bytes).unwrap().inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        let paths = facts
            .references
            .iter()
            .filter(|reference| !reference.members.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 3);
        assert_eq!(
            paths[1].members.iter().map(|member| member.text.as_str()).collect::<Vec<_>>(),
            ["Rows", "Caption"]
        );
        assert!(
            matches!(&paths[1].anchor, XmlReferenceAnchor::FormValue(attribute) if matches!(&attribute.attr_type, AttributeType::Ref { name, .. } if name == "Partners"))
        );
        assert!(
            matches!(&paths[2].anchor, XmlReferenceAnchor::FormValue(attribute) if matches!(&attribute.attr_type, AttributeType::Ref { name, .. } if name == "Customers"))
        );
        assert_eq!(&bytes[paths[1].members[1].range.clone()], b"Caption");
        assert_eq!(
            facts
                .references
                .iter()
                .filter(|reference| matches!(reference.anchor, XmlReferenceAnchor::Metadata { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn composition_query_maps_decoded_entities_back_to_original_bytes() {
        let bytes = include_bytes!("../fixtures/rename/composition.xml");
        let source = MetadataXmlSource::decode(bytes).unwrap();
        let facts = source.inspect(None);
        assert_eq!(facts.problems, [XmlReferenceProblem::DataSetOutputContract]);
        assert_eq!(facts.data_set_fields[0].field.text, "Stable");
        let query = &facts.queries[0];
        let start = query.text.find("Caption").unwrap();
        let range = query.original_range(start..start + "Caption".len()).unwrap();
        assert_eq!(&bytes[range.clone()], b"Cap&#116;ion");
        let changed = replace(bytes, range, &source.encode_name("Heading"));
        assert!(std::str::from_utf8(&changed).unwrap().contains("<title>Caption</title>"));
        assert!(std::str::from_utf8(&changed).unwrap().contains("P.Heading AS Stable"));
    }

    #[test]
    fn command_and_exchange_contracts_reference_objects_without_editing_captions() {
        let source =
            MetadataXmlSource::decode(include_bytes!("../fixtures/rename/commands.xml")).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.references.len(), 2);
        let XmlReferenceAnchor::Metadata { name, .. } = &facts.references[0].anchor else {
            panic!("metadata anchor")
        };
        assert_eq!(name.text, "Partners");
        let xml = "<ExchangePlanContent xmlns='http://v8.1c.ru/8.3/xcf/extrnprops'><Item><Metadata>Catalog.Partners</Metadata><AutoRecord>Allow</AutoRecord></Item></ExchangePlanContent>";
        let source = MetadataXmlSource::decode(xml.as_bytes()).unwrap();
        let facts = source.inspect(None);
        assert!(facts.problems.is_empty(), "{facts:#?}");
        assert_eq!(facts.references.len(), 1);
    }

    #[test]
    fn unsupported_xml_and_unproven_form_bindings_block_completeness() {
        let source = MetadataXmlSource::decode(b"<Unknown><Name>Caption</Name></Unknown>").unwrap();
        assert_eq!(source.inspect(None).problems, [XmlReferenceProblem::UnsupportedSurface]);
        let xml = include_str!("../fixtures/rename/managed.xml")
            .replace("Object.Rows.Caption", "Missing.Caption");
        assert!(MetadataXmlSource::decode(xml.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .contains(&XmlReferenceProblem::UnknownFormBinding));
    }

    #[test]
    fn command_and_exchange_children_require_the_surface_namespace() {
        let commands = include_str!("../fixtures/rename/commands.xml");
        let exchange = "<ExchangePlanContent xmlns='http://v8.1c.ru/8.3/xcf/extrnprops'><Item><Metadata>Catalog.Partners</Metadata><AutoRecord>Allow</AutoRecord></Item></ExchangePlanContent>";
        for xml in [
            commands.replacen("<CommandsPlacement>", "<CommandsPlacement xmlns='urn:foreign'>", 1),
            commands.replacen("<Command name=", "<Command xmlns='urn:foreign' name=", 1),
            exchange.replacen("<Item>", "<Item xmlns='urn:foreign'>", 1),
            exchange.replacen("<Metadata>", "<Metadata xmlns='urn:foreign'>", 1),
        ] {
            assert!(
                MetadataXmlSource::decode(xml.as_bytes())
                    .unwrap()
                    .inspect(None)
                    .problems
                    .contains(&XmlReferenceProblem::UnknownReferenceContract),
                "{xml}"
            );
        }
        for xml in [commands, exchange] {
            assert!(MetadataXmlSource::decode(xml.as_bytes())
                .unwrap()
                .inspect(None)
                .problems
                .is_empty());
        }
    }

    #[test]
    fn command_and_exchange_shapes_refuse_unknown_fields_attributes_and_duplicates() {
        let exchange = "<ExchangePlanContent xmlns='http://v8.1c.ru/8.3/xcf/extrnprops'><Item><Metadata>Catalog.Customers</Metadata><AutoRecord>Allow</AutoRecord></Item></ExchangePlanContent>";
        let commands = include_str!("../fixtures/rename/commands.xml");
        for xml in [
            exchange.replacen(
                "</Item>",
                "<UnknownReference>Catalog.Partners</UnknownReference></Item>",
                1,
            ),
            exchange.replacen("</Metadata>", "</Metadata><Metadata>Catalog.Partners</Metadata>", 1),
            exchange.replacen("</Item>", "<AutoRecord>Deny</AutoRecord></Item>", 1),
            exchange.replacen("<Item>", "<Item unknown='Catalog.Partners'>", 1),
            exchange.replacen("<Metadata>", "<Metadata unknown='Catalog.Partners'>", 1),
            exchange.replacen(
                "<AutoRecord>Allow</AutoRecord>",
                "<AutoRecord><Unknown>Allow</Unknown></AutoRecord>",
                1,
            ),
            commands.replacen(
                "</Command>",
                "<UnknownReference>Catalog.Partners</UnknownReference></Command>",
                1,
            ),
            commands.replacen("<Command name=", "<Command unknown='Catalog.Partners' name=", 1),
            commands.replacen(
                "<CommandsPlacement>",
                "<CommandsPlacement unknown='Catalog.Partners'>",
                1,
            ),
            commands.replacen(
                "</CommandGroup>",
                "</CommandGroup><CommandGroup>Other</CommandGroup>",
                1,
            ),
            commands.replacen(
                "<Placement>Auto</Placement>",
                "<Placement><Unknown>Auto</Unknown></Placement>",
                1,
            ),
        ] {
            assert!(
                MetadataXmlSource::decode(xml.as_bytes())
                    .unwrap()
                    .inspect(None)
                    .problems
                    .contains(&XmlReferenceProblem::UnknownReferenceContract),
                "{xml}"
            );
        }
        for xml in [commands, exchange] {
            assert!(MetadataXmlSource::decode(xml.as_bytes())
                .unwrap()
                .inspect(None)
                .problems
                .is_empty());
        }
        let minimal = "<ExchangePlanContent><Item><Metadata>Catalog.Partners</Metadata></Item></ExchangePlanContent>";
        assert!(MetadataXmlSource::decode(minimal.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .is_empty());
    }

    #[test]
    fn command_and_exchange_duplicate_metadata_values_are_incomplete() {
        let xml = "<ExchangePlanContent xmlns='http://v8.1c.ru/8.3/xcf/extrnprops'><Item><Metadata>Catalog.Customers</Metadata><Metadata>Catalog.Partners</Metadata><AutoRecord>Allow</AutoRecord></Item></ExchangePlanContent>";
        assert!(MetadataXmlSource::decode(xml.as_bytes())
            .unwrap()
            .inspect(None)
            .problems
            .contains(&XmlReferenceProblem::UnknownReferenceContract));
    }
}
