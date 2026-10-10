//! Common attributes (`ОбщиеРеквизиты`): fields the platform adds to many objects at once.
//!
//! A common attribute is declared once under `CommonAttributes/<Name>.xml` and then appears as
//! an ordinary field of every object in its composition (`Состав`) — in the object model
//! (`Ссылка.Организация`, `Объект.ОбластьДанныхОсновныеДанные`) and in query tables. The
//! objects' own XML does not mention it, so a field model built from that XML alone is short by
//! exactly these fields.
//!
//! Membership follows the platform rule (ITS, «Общие реквизиты»): an object listed in `Content`
//! with `Use` or `DontUse` is in or out regardless of anything else; an object not listed, or
//! listed with `Auto`, follows the attribute's `AutoUse`. Only the kinds that have fields are
//! ever reported as members; everything else (constants, scheduled jobs, ...) may sit in
//! `Content` but contributes no field this model could expose.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use stdx::case::CaseExt;

use crate::metadata_object::{AttributeType, MdoType};

/// One object's setting in a common attribute's `Content` (`Состав`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommonAttributeUse {
    Use,
    DontUse,
    Auto,
}

/// A parsed `CommonAttributes/<Name>.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonAttribute {
    pub name: String,
    pub attr_type: AttributeType,
    /// `AutoUse = Use`: objects without an explicit setting are members.
    pub auto_use: bool,
    /// `DataSeparation = Separate`: the attribute is a data separator.
    pub separator: bool,
    /// Explicit settings keyed by `(kind, folded object name)`.
    pub content: FxHashMap<(MdoType, String), CommonAttributeUse>,
    /// A `Content` entry this reader could not attach to an object while it could still have
    /// put one INTO the composition. With `AutoUse = DontUse` such an entry is the only way an
    /// object becomes a member, so membership is then unknown for every object; with
    /// `AutoUse = Use` an unreadable entry can only remove the field, and over-reporting a
    /// field silences a diagnostic rather than inventing one.
    pub unattributed_members: bool,
}

/// A field a common attribute contributes to one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommonAttributeField {
    pub name: String,
    pub attr_type: AttributeType,
    /// A data separator. In register tables it behaves like a dimension: it is part of the
    /// record key and a column of every virtual table.
    pub separator: bool,
}

impl CommonAttributeField {
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity() + self.attr_type.estimated_heap_size()
    }
}

/// The common attributes that apply to one object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectCommonAttributes {
    pub fields: Vec<CommonAttributeField>,
    /// Membership could not be decided for at least one common attribute, so the object may
    /// carry a field this list does not name.
    pub open: bool,
}

impl ObjectCommonAttributes {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && !self.open
    }
}

/// Whether the platform lets a common attribute add a field to an object of this kind.
///
/// The set is the ITS list of kinds a common attribute may include, narrowed to the kinds this
/// model gives fields to. Constants, scheduled jobs and the like may sit in `Content` (a
/// separator divides their data too) but expose no field to code or queries.
pub fn kind_takes_common_attributes(mdo_type: MdoType) -> bool {
    matches!(
        mdo_type,
        MdoType::Catalog
            | MdoType::Document
            | MdoType::ChartOfCharacteristicTypes
            | MdoType::ChartOfAccounts
            | MdoType::ChartOfCalculationTypes
            | MdoType::BusinessProcess
            | MdoType::Task
            | MdoType::ExchangePlan
            | MdoType::InformationRegister
            | MdoType::AccumulationRegister
            | MdoType::AccountingRegister
            | MdoType::CalculationRegister
    )
}

/// What a `Content` entry's `xr:Metadata` reference names.
pub(crate) enum ContentTarget {
    /// A top-level object of a kind this model represents.
    Object(MdoType, String),
    /// A valid reference to something that never carries a field here: a nested object
    /// (`CalculationRegister.X.Recalculation.Y`) or a kind without fields in this model.
    Ignored,
    /// A reference this reader does not understand.
    Unknown,
}

/// Kinds that legitimately appear in `Content` but are not represented by [`MdoType`].
const KNOWN_UNMODELED_KINDS: &[&str] = &[
    "DocumentJournal",
    "Sequence",
    "ScheduledJob",
    "SessionParameter",
    "SettingsStorage",
    "FilterCriterion",
    "ExternalDataSource",
];

pub(crate) fn classify_content_reference(reference: &str) -> ContentTarget {
    let mut segments = reference.split('.');
    let (Some(kind), Some(name)) = (segments.next(), segments.next()) else {
        return ContentTarget::Unknown;
    };
    if kind.is_empty() || name.is_empty() {
        return ContentTarget::Unknown;
    }
    let nested = segments.next().is_some();
    if KNOWN_UNMODELED_KINDS.iter().any(|known| known.eq_ignore_ascii_case(kind)) {
        return ContentTarget::Ignored;
    }
    match kind.parse::<MdoType>() {
        Ok(_) if nested => ContentTarget::Ignored,
        Ok(mdo_type) => ContentTarget::Object(mdo_type, name.to_string()),
        Err(_) => ContentTarget::Unknown,
    }
}

impl CommonAttribute {
    /// `Some(true/false)` when the platform rule decides membership, `None` when it cannot be
    /// decided from what was read.
    pub fn includes(&self, mdo_type: MdoType, object_name: &str) -> Option<bool> {
        if !kind_takes_common_attributes(mdo_type) {
            return Some(false);
        }
        let explicit = self.content.get(&(mdo_type, object_name.fold_lower())).copied();
        match explicit {
            Some(CommonAttributeUse::Use) => Some(true),
            Some(CommonAttributeUse::DontUse) => Some(false),
            Some(CommonAttributeUse::Auto) | None if self.auto_use => Some(true),
            Some(CommonAttributeUse::Auto) | None if self.unattributed_members => None,
            Some(CommonAttributeUse::Auto) | None => Some(false),
        }
    }
}

/// All common attributes of one configuration root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommonAttributeSet {
    pub attributes: Vec<CommonAttribute>,
    /// A `CommonAttributes/*.xml` file exists but could not be read: some field of unknown
    /// name may sit on any object.
    pub unreadable: bool,
}

impl CommonAttributeSet {
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty() && !self.unreadable
    }

    /// Add the result of parsing one `CommonAttributes/*.xml`; `None` marks the set unreadable.
    pub fn push_parsed(&mut self, parsed: Option<CommonAttribute>) {
        match parsed {
            Some(attribute) => self.attributes.push(attribute),
            None => self.unreadable = true,
        }
    }

    /// The common-attribute fields of one object, and whether that list may be short.
    pub fn for_object(&self, mdo_type: MdoType, object_name: &str) -> ObjectCommonAttributes {
        if !kind_takes_common_attributes(mdo_type) {
            return ObjectCommonAttributes::default();
        }
        let mut out = ObjectCommonAttributes { fields: Vec::new(), open: self.unreadable };
        for attribute in &self.attributes {
            match attribute.includes(mdo_type, object_name) {
                Some(true) => out.fields.push(CommonAttributeField {
                    name: attribute.name.clone(),
                    attr_type: attribute.attr_type.clone(),
                    separator: attribute.separator,
                }),
                Some(false) => {}
                None => out.open = true,
            }
        }
        out
    }
}

/// Fold `overlay` into `base` the way an extension overlay folds members: a same-named field is
/// replaced, a new one added, and openness accumulates.
pub(crate) fn merge_common_attribute_fields(
    base: &mut Vec<CommonAttributeField>,
    overlay: &[CommonAttributeField],
) {
    for field in overlay {
        base.retain(|existing| existing.name.fold_lower() != field.name.fold_lower());
        base.push(field.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(
        auto_use: bool,
        content: &[(MdoType, &str, CommonAttributeUse)],
    ) -> CommonAttribute {
        CommonAttribute {
            name: "Организация".to_string(),
            attr_type: AttributeType::String { length: Some(10) },
            auto_use,
            separator: false,
            content: content
                .iter()
                .map(|(kind, name, usage)| ((*kind, name.fold_lower()), *usage))
                .collect(),
            unattributed_members: false,
        }
    }

    #[test]
    fn explicit_setting_wins_over_auto_use() {
        let attr = attribute(
            true,
            &[
                (MdoType::Catalog, "Исключенный", CommonAttributeUse::DontUse),
                (MdoType::Catalog, "Явный", CommonAttributeUse::Use),
            ],
        );
        assert_eq!(attr.includes(MdoType::Catalog, "Исключенный"), Some(false));
        assert_eq!(attr.includes(MdoType::Catalog, "явный"), Some(true));
        assert_eq!(attr.includes(MdoType::Catalog, "НеУпомянутый"), Some(true));

        let attr = attribute(false, &[(MdoType::Document, "Явный", CommonAttributeUse::Use)]);
        assert_eq!(attr.includes(MdoType::Document, "Явный"), Some(true));
        assert_eq!(attr.includes(MdoType::Document, "НеУпомянутый"), Some(false));
    }

    #[test]
    fn auto_entry_follows_auto_use() {
        let on = attribute(true, &[(MdoType::Catalog, "А", CommonAttributeUse::Auto)]);
        let off = attribute(false, &[(MdoType::Catalog, "А", CommonAttributeUse::Auto)]);
        assert_eq!(on.includes(MdoType::Catalog, "А"), Some(true));
        assert_eq!(off.includes(MdoType::Catalog, "А"), Some(false));
    }

    #[test]
    fn kinds_without_fields_never_take_the_attribute() {
        let attr = attribute(true, &[(MdoType::Constant, "К", CommonAttributeUse::Use)]);
        assert_eq!(attr.includes(MdoType::Constant, "К"), Some(false));
        assert_eq!(attr.includes(MdoType::Enum, "П"), Some(false));
    }

    #[test]
    fn unattributed_entry_leaves_membership_open_only_without_auto_use() {
        let mut attr = attribute(false, &[]);
        attr.unattributed_members = true;
        assert_eq!(attr.includes(MdoType::Catalog, "А"), None);
        attr.auto_use = true;
        assert_eq!(attr.includes(MdoType::Catalog, "А"), Some(true));
    }

    #[test]
    fn content_references_are_classified() {
        assert!(matches!(
            classify_content_reference("Catalog.Товары"),
            ContentTarget::Object(MdoType::Catalog, name) if name == "Товары"
        ));
        assert!(matches!(classify_content_reference("ScheduledJob.Х"), ContentTarget::Ignored));
        assert!(matches!(
            classify_content_reference("CalculationRegister.Р.Recalculation.П"),
            ContentTarget::Ignored
        ));
        assert!(matches!(classify_content_reference("Nonsense.Х"), ContentTarget::Unknown));
        assert!(matches!(classify_content_reference("Catalog"), ContentTarget::Unknown));
    }

    fn names(fields: &[CommonAttributeField]) -> Vec<(&str, bool)> {
        let mut out: Vec<_> = fields.iter().map(|f| (f.name.as_str(), f.separator)).collect();
        out.sort();
        out
    }

    #[test]
    fn a_loaded_configuration_attaches_fields_by_composition() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/common_attributes");
        let config = crate::load_from_directory(root).expect("the fixture loads");

        let catalog = config.find_metadata_object(MdoType::Catalog, "Справочник1").unwrap();
        assert_eq!(
            names(&catalog.common_attributes),
            [
                ("ОбластьДанныхОсновныеДанные", true),
                ("ОбщийКомментарий", false),
                ("Организация", false)
            ]
        );
        assert!(!catalog.common_attributes_open);
        // Own attributes stay what the object declares.
        assert!(catalog.attributes.iter().all(|a| a.name != "Организация"));

        let excluded =
            config.find_metadata_object(MdoType::Catalog, "СправочникБезРазделения").unwrap();
        assert_eq!(names(&excluded.common_attributes), [("ОбщийКомментарий", false)]);

        let document = config.find_metadata_object(MdoType::Document, "Документ1").unwrap();
        assert_eq!(
            names(&document.common_attributes),
            [("ОбластьДанныхОсновныеДанные", true), ("Организация", false)]
        );

        let info = config
            .find_register_by_type_and_name(MdoType::InformationRegister, "РегистрСведений1")
            .unwrap();
        assert_eq!(
            names(info.common_attributes()),
            [
                ("ОбластьДанныхОсновныеДанные", true),
                ("ОбщийКомментарий", false),
                ("Организация", false)
            ]
        );
        let accumulation = config
            .find_register_by_type_and_name(MdoType::AccumulationRegister, "РегистрНакопления1")
            .unwrap();
        assert_eq!(
            names(accumulation.common_attributes()),
            [("ОбластьДанныхОсновныеДанные", true), ("ОбщийКомментарий", false)]
        );
    }

    #[test]
    fn unreadable_file_opens_every_object_that_takes_fields() {
        let set = CommonAttributeSet { attributes: Vec::new(), unreadable: true };
        assert!(set.for_object(MdoType::Catalog, "А").open);
        assert!(!set.for_object(MdoType::Enum, "П").open);
    }
}
