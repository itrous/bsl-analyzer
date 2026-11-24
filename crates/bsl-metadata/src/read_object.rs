use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::read_properties::LocalizedString;
use crate::xml_parser::{helpers::*, read_properties::parse_localized};
use crate::{AttributeType, ObjectBelonging};

/// Metadata containers which own no values, references or manager collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReadObjectKind {
    FunctionalOption,
    CommonCommand,
    CommonPicture,
}

impl ReadObjectKind {
    pub const ALL: [Self; 3] = [Self::FunctionalOption, Self::CommonCommand, Self::CommonPicture];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_lowercase().as_str() {
            "functionaloption"
            | "functionaloptions"
            | "функциональнаяопция"
            | "функциональныеопции" => Some(Self::FunctionalOption),
            "commoncommand" | "commoncommands" | "общаякоманда" | "общиекоманды" => {
                Some(Self::CommonCommand)
            }
            "commonpicture" | "commonpictures" | "общаякартинка" | "общиекартинки" => {
                Some(Self::CommonPicture)
            }
            _ => None,
        }
    }

    pub fn english_name(self) -> &'static str {
        match self {
            Self::FunctionalOption => "FunctionalOption",
            Self::CommonCommand => "CommonCommand",
            Self::CommonPicture => "CommonPicture",
        }
    }

    pub fn collection(self) -> &'static str {
        match self {
            Self::FunctionalOption => "FunctionalOptions",
            Self::CommonCommand => "CommonCommands",
            Self::CommonPicture => "CommonPictures",
        }
    }

    pub fn russian_name(self) -> &'static str {
        match self {
            Self::FunctionalOption => "ФункциональнаяОпция",
            Self::CommonCommand => "ОбщаяКоманда",
            Self::CommonPicture => "ОбщаяКартинка",
        }
    }
}

/// An inspectable metadata declaration; optional fields retain unavailable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadObject {
    pub kind: ReadObjectKind,
    pub name: String,
    pub uuid: Uuid,
    pub object_belonging: ObjectBelonging,
    pub extended_configuration_object: Option<Uuid>,
    pub synonym: Option<LocalizedString>,
    pub location: Option<String>,
    pub privileged_get_mode: Option<bool>,
    pub content: Option<Vec<String>>,
    pub group: Option<String>,
    pub picture: Option<String>,
    pub parameter_type: Option<AttributeType>,
    pub modifies_data: Option<bool>,
    /// Selected declarations could not be composed into one proved effective object.
    #[serde(default)]
    pub resolution_incomplete: bool,
}

impl ReadObject {
    pub fn from_xml(xml: &str, kind: ReadObjectKind) -> crate::Result<Self> {
        let doc = parse_xml(xml)?;
        let mdo = find_mdo_element(&doc)
            .filter(|node| node.tag_name().name() == kind.english_name())
            .ok_or_else(|| {
                crate::MetadataError::InvalidFormat(format!("Expected {}", kind.english_name()))
            })?;
        let props = find_child(mdo, "Properties")
            .ok_or_else(|| crate::MetadataError::InvalidFormat("Missing Properties".into()))?;
        let (object_belonging, extended_configuration_object) = parse_extension_ownership(props);
        let optional_string = |field| {
            find_child(props, field).map(|node| node.text().unwrap_or_default().to_string())
        };
        let optional_bool = |field| match child_text(props, field) {
            Some("true" | "1") => Some(true),
            Some("false" | "0") => Some(false),
            _ => None,
        };
        Ok(Self {
            kind,
            name: child_text(props, "Name").unwrap_or_default().into(),
            uuid: mdo_uuid(mdo),
            object_belonging,
            extended_configuration_object,
            synonym: parse_localized(props),
            location: optional_string("Location"),
            privileged_get_mode: optional_bool("PrivilegedGetMode"),
            content: find_child(props, "Content").and_then(|node| {
                node.children()
                    .filter(|child| child.is_element())
                    .map(|child| {
                        (child.tag_name().name() == "Object")
                            .then(|| child.text().map(str::to_owned))
                            .flatten()
                    })
                    .collect()
            }),
            group: optional_string("Group"),
            picture: optional_string("Picture"),
            parameter_type: find_child(props, "CommandParameterType")
                .map(crate::xml_parser::parse_type_description)
                .transpose()?,
            modifies_data: optional_bool("ModifiesData"),
            resolution_incomplete: false,
        })
    }

    pub fn adopts(&self, base: &Self) -> bool {
        self.object_belonging == ObjectBelonging::Adopted
            && !base.uuid.is_nil()
            && self.extended_configuration_object == Some(base.uuid)
            && self.kind == base.kind
    }

    pub fn apply_overlay(&mut self, overlay: &Self) {
        self.resolution_incomplete |= overlay.resolution_incomplete;
        macro_rules! replace_present { ($($field:ident),*) => {$(if overlay.$field.is_some() {self.$field.clone_from(&overlay.$field);})*}; }
        replace_present!(
            synonym,
            location,
            privileged_get_mode,
            content,
            group,
            picture,
            parameter_type,
            modifies_data
        );
    }

    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.synonym.as_ref().map_or(0, |values| {
                values
                    .iter()
                    .map(|(key, value)| {
                        std::mem::size_of::<(String, String)>() + key.capacity() + value.capacity()
                    })
                    .sum::<usize>()
            })
            + [&self.location, &self.group, &self.picture]
                .iter()
                .map(|v| v.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self.content.as_ref().map_or(0, |values| {
                stdx::heap::vec_bytes::<String>(values.len())
                    + values.iter().map(String::capacity).sum::<usize>()
            })
            + self.parameter_type.as_ref().map_or(0, AttributeType::estimated_heap_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_functional_option_keeps_storage_privileged_mode_and_content() {
        let option = ReadObject::from_xml(r#"<MetaDataObject xmlns:xr="urn:readable"><FunctionalOption><Properties><Name>UseFeature</Name><Synonym/><Location>Constant.Feature</Location><PrivilegedGetMode>false</PrivilegedGetMode><Content><xr:Object>CommonCommand.Run</xr:Object><xr:Object>Catalog.Products</xr:Object></Content></Properties></FunctionalOption></MetaDataObject>"#,ReadObjectKind::FunctionalOption).unwrap();
        assert_eq!(option.location.as_deref(), Some("Constant.Feature"));
        assert_eq!(option.privileged_get_mode, Some(false));
        assert_eq!(
            option.content,
            Some(vec!["CommonCommand.Run".into(), "Catalog.Products".into()])
        );
        assert!(option.synonym.unwrap().is_empty());
        let unknown = ReadObject::from_xml("<MetaDataObject><FunctionalOption><Properties><Name>X</Name></Properties></FunctionalOption></MetaDataObject>",ReadObjectKind::FunctionalOption).unwrap();
        assert!(unknown.content.is_none());
        assert_eq!(
            ReadObjectKind::parse("ФункциональныеОпции"),
            Some(ReadObjectKind::FunctionalOption)
        );
        assert!(ReadObject::from_xml("<MetaDataObject><Catalog><Properties><Name>X</Name></Properties></Catalog></MetaDataObject>",ReadObjectKind::FunctionalOption).is_err());
    }
}
