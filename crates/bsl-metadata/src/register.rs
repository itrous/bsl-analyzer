use crate::dimension::Dimension;
use crate::enums::ObjectBelonging;
use crate::metadata_object::MdoType;
use crate::traits::MdObject;
use serde::{Deserialize, Serialize};
use std::any::Any;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResource {
    uuid: Uuid,

    name: String,

    #[serde(default)]
    name_en: Option<String>,

    #[serde(default)]
    type_str: String,

    #[serde(skip)]
    attr_type: Option<crate::metadata_object::AttributeType>,
}

impl RegisterResource {
    pub fn new(uuid: Uuid, name: impl Into<String>) -> Self {
        Self { uuid, name: name.into(), name_en: None, type_str: String::new(), attr_type: None }
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn name_en(&self) -> Option<&str> {
        self.name_en.as_deref()
    }

    pub fn type_str(&self) -> &str {
        &self.type_str
    }

    pub fn set_type_str(&mut self, type_str: String) {
        self.type_str = type_str;
    }

    pub fn attr_type(&self) -> Option<&crate::metadata_object::AttributeType> {
        self.attr_type.as_ref()
    }

    pub fn set_attr_type(&mut self, attr_type: crate::metadata_object::AttributeType) {
        self.attr_type = Some(attr_type);
    }

    /// Heap bytes owned by this resource: its name/type-text strings plus its
    /// resolved type's own owned payload.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.name_en.as_ref().map_or(0, String::capacity)
            + self.type_str.capacity()
            + self
                .attr_type
                .as_ref()
                .map_or(0, crate::metadata_object::AttributeType::estimated_heap_size)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterAttribute {
    uuid: Uuid,

    name: String,

    #[serde(default)]
    name_en: Option<String>,

    #[serde(default)]
    type_str: String,

    #[serde(skip)]
    attr_type: Option<crate::metadata_object::AttributeType>,
}

impl RegisterAttribute {
    pub fn new(uuid: Uuid, name: impl Into<String>) -> Self {
        Self { uuid, name: name.into(), name_en: None, type_str: String::new(), attr_type: None }
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn name_en(&self) -> Option<&str> {
        self.name_en.as_deref()
    }

    /// The English spelling of a synthesised standard attribute. A user attribute parses its
    /// own names from XML; the synthesiser is the only writer here.
    pub fn set_name_en(&mut self, name_en: impl Into<String>) {
        self.name_en = Some(name_en.into());
    }

    pub fn type_str(&self) -> &str {
        &self.type_str
    }

    pub fn set_type_str(&mut self, type_str: String) {
        self.type_str = type_str;
    }

    pub fn attr_type(&self) -> Option<&crate::metadata_object::AttributeType> {
        self.attr_type.as_ref()
    }

    pub fn set_attr_type(&mut self, attr_type: crate::metadata_object::AttributeType) {
        self.attr_type = Some(attr_type);
    }

    /// Heap bytes owned by this attribute: its name/type-text strings plus its
    /// resolved type's own owned payload.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.name_en.as_ref().map_or(0, String::capacity)
            + self.type_str.capacity()
            + self
                .attr_type
                .as_ref()
                .map_or(0, crate::metadata_object::AttributeType::estimated_heap_size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterPeriodicity {
    Nonperiodical,
    Second,
    Day,
    Month,
    RecorderPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccumulationRegisterType {
    Balance,
    Turnovers,
    BalanceAndTurnovers,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Register {
    #[serde(rename = "uuid")]
    uuid: Uuid,

    #[serde(rename = "name")]
    name: String,

    #[serde(rename = "mdoType")]
    mdo_type: MdoType,

    #[serde(rename = "dimensions", default)]
    dimensions: Vec<Dimension>,

    #[serde(rename = "resources", default)]
    resources: Vec<RegisterResource>,

    #[serde(rename = "attributes", default)]
    attributes: Vec<RegisterAttribute>,

    #[serde(rename = "periodicity", default)]
    periodicity: Option<RegisterPeriodicity>,

    #[serde(rename = "recorderSubordinate", default)]
    recorder_subordinate: bool,

    #[serde(rename = "registerType", default)]
    register_type: Option<AccumulationRegisterType>,

    #[serde(rename = "enableTotalsSliceFirst", default)]
    enable_totals_slice_first: bool,

    #[serde(rename = "enableTotalsSliceLast", default)]
    enable_totals_slice_last: bool,

    /// Fields contributed by common attributes whose composition includes this register; a
    /// separator among them acts as a dimension. Kept apart from the declared members for the
    /// reason given on [`crate::MetadataObject::common_attributes`].
    #[serde(rename = "commonAttributes", default)]
    common_attributes: Vec<crate::common_attribute::CommonAttributeField>,

    #[serde(rename = "commonAttributesOpen", default)]
    common_attributes_open: bool,
}

impl Register {
    pub fn builder() -> RegisterBuilder {
        RegisterBuilder::default()
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mdo_type(&self) -> MdoType {
        self.mdo_type
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn resources(&self) -> &[RegisterResource] {
        &self.resources
    }

    pub fn attributes(&self) -> &[RegisterAttribute] {
        &self.attributes
    }

    pub fn is_information_register(&self) -> bool {
        self.mdo_type == MdoType::InformationRegister
    }

    pub fn is_accumulation_register(&self) -> bool {
        self.mdo_type == MdoType::AccumulationRegister
    }

    pub fn is_accounting_register(&self) -> bool {
        self.mdo_type == MdoType::AccountingRegister
    }

    pub fn is_calculation_register(&self) -> bool {
        self.mdo_type == MdoType::CalculationRegister
    }

    pub fn periodicity(&self) -> Option<RegisterPeriodicity> {
        self.periodicity
    }

    pub fn is_recorder_subordinate(&self) -> bool {
        self.recorder_subordinate
    }

    pub fn register_type(&self) -> Option<AccumulationRegisterType> {
        self.register_type
    }

    pub fn enable_totals_slice_first(&self) -> bool {
        self.enable_totals_slice_first
    }

    pub fn enable_totals_slice_last(&self) -> bool {
        self.enable_totals_slice_last
    }

    /// Fields contributed by common attributes (see [`Self::set_common_attributes`]).
    pub fn common_attributes(&self) -> &[crate::common_attribute::CommonAttributeField] {
        &self.common_attributes
    }

    /// Whether a common attribute may apply to this register without being listed in
    /// [`Self::common_attributes`], so its field set is not known to be exhaustive.
    pub fn common_attributes_open(&self) -> bool {
        self.common_attributes_open
    }

    /// Record which common attributes apply to this register.
    pub fn set_common_attributes(
        &mut self,
        applied: crate::common_attribute::ObjectCommonAttributes,
    ) {
        self.common_attributes = applied.fields;
        self.common_attributes_open = applied.open;
    }

    /// Overlay an extension's adopted copy of this register onto the base: merge
    /// dimensions, resources, and attributes by name (1C lets an extension add new
    /// measurements/resources/attributes to a borrowed register, or replace a
    /// same-named one), and take the extension's scalar settings where it specifies
    /// them. Mirrors [`MetadataObject::apply_extension_overlay`](crate::MetadataObject::apply_extension_overlay)
    /// so the whole-config merge and the per-register resolver share one rule.
    pub fn apply_extension_overlay(&mut self, overlay: &Register) {
        for dimension in &overlay.dimensions {
            self.dimensions
                .retain(|existing| !stdx::case::eq_ignore_case(existing.name(), dimension.name()));
            self.dimensions.push(dimension.clone());
        }
        for resource in &overlay.resources {
            self.resources
                .retain(|existing| !stdx::case::eq_ignore_case(existing.name(), resource.name()));
            self.resources.push(resource.clone());
        }
        for attribute in &overlay.attributes {
            self.attributes
                .retain(|existing| !stdx::case::eq_ignore_case(existing.name(), attribute.name()));
            self.attributes.push(attribute.clone());
        }
        if overlay.periodicity.is_some() {
            self.periodicity = overlay.periodicity;
        }
        if overlay.register_type.is_some() {
            self.register_type = overlay.register_type;
        }
        crate::common_attribute::merge_common_attribute_fields(
            &mut self.common_attributes,
            &overlay.common_attributes,
        );
        self.common_attributes_open |= overlay.common_attributes_open;
    }

    pub fn virtual_tables(&self) -> Vec<&'static str> {
        match self.mdo_type {
            MdoType::InformationRegister => self.info_register_virtual_tables(),
            MdoType::AccumulationRegister => self.accum_register_virtual_tables(),
            MdoType::AccountingRegister => vec![],
            MdoType::CalculationRegister => vec![],
            _ => vec![],
        }
    }

    fn info_register_virtual_tables(&self) -> Vec<&'static str> {
        let mut tables = Vec::new();

        if let Some(periodicity) = self.periodicity {
            if periodicity != RegisterPeriodicity::Nonperiodical {
                tables.push("СрезПервых");
                tables.push("СрезПоследних");
            }
        }

        tables
    }

    fn accum_register_virtual_tables(&self) -> Vec<&'static str> {
        match self.register_type {
            Some(AccumulationRegisterType::Balance) => vec!["Остатки"],
            Some(AccumulationRegisterType::Turnovers) => vec!["Обороты"],
            Some(AccumulationRegisterType::BalanceAndTurnovers) => vec!["Остатки", "Обороты"],
            None => vec![],
        }
    }

    /// Heap bytes owned by this register, memoised by `ide-db`'s
    /// `parse_register_query` for Salsa's `heap_size` hook: its name plus the
    /// dimension/resource/attribute vecs and each entry's own owned payload. New
    /// heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + stdx::heap::vec_bytes::<Dimension>(self.dimensions.len())
            + self.dimensions.iter().map(Dimension::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<RegisterResource>(self.resources.len())
            + self.resources.iter().map(RegisterResource::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<RegisterAttribute>(self.attributes.len())
            + self.attributes.iter().map(RegisterAttribute::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<crate::common_attribute::CommonAttributeField>(
                self.common_attributes.len(),
            )
            + self
                .common_attributes
                .iter()
                .map(crate::common_attribute::CommonAttributeField::estimated_heap_size)
                .sum::<usize>()
    }
}

impl MdObject for Register {
    fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn comment(&self) -> Option<&str> {
        None
    }

    fn object_belonging(&self) -> ObjectBelonging {
        ObjectBelonging::Own
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Default)]
pub struct RegisterBuilder {
    uuid: Option<Uuid>,
    name: Option<String>,
    mdo_type: Option<MdoType>,
    dimensions: Vec<Dimension>,
    resources: Vec<RegisterResource>,
    attributes: Vec<RegisterAttribute>,
    periodicity: Option<RegisterPeriodicity>,
    register_type: Option<AccumulationRegisterType>,
    recorder_subordinate: bool,
    enable_totals_slice_first: bool,
    enable_totals_slice_last: bool,
}

impl RegisterBuilder {
    pub fn uuid(mut self, uuid: Uuid) -> Self {
        self.uuid = Some(uuid);
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn mdo_type(mut self, mdo_type: MdoType) -> Self {
        self.mdo_type = Some(mdo_type);
        self
    }

    pub fn add_dimension(mut self, dimension: Dimension) -> Self {
        self.dimensions.push(dimension);
        self
    }

    pub fn dimensions(mut self, dimensions: Vec<Dimension>) -> Self {
        self.dimensions = dimensions;
        self
    }

    pub fn add_resource(mut self, resource: RegisterResource) -> Self {
        self.resources.push(resource);
        self
    }

    pub fn resources(mut self, resources: Vec<RegisterResource>) -> Self {
        self.resources = resources;
        self
    }

    pub fn add_attribute(mut self, attribute: RegisterAttribute) -> Self {
        self.attributes.push(attribute);
        self
    }

    pub fn attributes(mut self, attributes: Vec<RegisterAttribute>) -> Self {
        self.attributes = attributes;
        self
    }

    pub fn periodicity(mut self, periodicity: Option<RegisterPeriodicity>) -> Self {
        self.periodicity = periodicity;
        self
    }

    pub fn register_type(mut self, register_type: Option<AccumulationRegisterType>) -> Self {
        self.register_type = register_type;
        self
    }

    pub fn recorder_subordinate(mut self, value: bool) -> Self {
        self.recorder_subordinate = value;
        self
    }

    pub fn enable_totals_slice_first(mut self, value: bool) -> Self {
        self.enable_totals_slice_first = value;
        self
    }

    pub fn enable_totals_slice_last(mut self, value: bool) -> Self {
        self.enable_totals_slice_last = value;
        self
    }

    pub fn build(self) -> Register {
        Register {
            uuid: self.uuid.unwrap_or_else(Uuid::new_v4),
            name: self.name.unwrap_or_default(),
            mdo_type: self.mdo_type.unwrap_or(MdoType::InformationRegister),
            dimensions: self.dimensions,
            resources: self.resources,
            attributes: self.attributes,
            periodicity: self.periodicity,
            register_type: self.register_type,
            recorder_subordinate: self.recorder_subordinate,
            enable_totals_slice_first: self.enable_totals_slice_first,
            enable_totals_slice_last: self.enable_totals_slice_last,
            common_attributes: Vec::new(),
            common_attributes_open: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimension::Dimension;

    #[test]
    fn test_register_builder_information() {
        let register = Register::builder()
            .name("РегистрСведений1")
            .mdo_type(MdoType::InformationRegister)
            .build();

        assert_eq!(register.name(), "РегистрСведений1");
        assert!(register.is_information_register());
        assert!(!register.is_accumulation_register());
        assert_eq!(register.dimensions().len(), 0);
    }

    #[test]
    fn test_register_with_dimensions() {
        let dim1 = Dimension::builder().name("Справочник1").deny_incomplete_values(false).build();

        let dim2 = Dimension::builder().name("Справочник2").deny_incomplete_values(true).build();

        let register = Register::builder()
            .name("TestRegister")
            .mdo_type(MdoType::AccumulationRegister)
            .add_dimension(dim1)
            .add_dimension(dim2)
            .build();

        assert_eq!(register.name(), "TestRegister");
        assert!(register.is_accumulation_register());
        assert_eq!(register.dimensions().len(), 2);
        assert_eq!(register.dimensions()[0].name(), "Справочник1");
        assert!(!register.dimensions()[0].is_deny_incomplete_values());
        assert_eq!(register.dimensions()[1].name(), "Справочник2");
        assert!(register.dimensions()[1].is_deny_incomplete_values());
    }

    #[test]
    fn test_register_types() {
        let info_reg = Register::builder().mdo_type(MdoType::InformationRegister).build();
        assert!(info_reg.is_information_register());

        let accum_reg = Register::builder().mdo_type(MdoType::AccumulationRegister).build();
        assert!(accum_reg.is_accumulation_register());

        let accounting_reg = Register::builder().mdo_type(MdoType::AccountingRegister).build();
        assert!(accounting_reg.is_accounting_register());

        let calc_reg = Register::builder().mdo_type(MdoType::CalculationRegister).build();
        assert!(calc_reg.is_calculation_register());
    }

    #[test]
    fn test_register_partial_eq() {
        let uuid = Uuid::new_v4();

        let reg1 = Register::builder()
            .uuid(uuid)
            .name("Test")
            .mdo_type(MdoType::InformationRegister)
            .build();

        let reg2 = Register::builder()
            .uuid(uuid)
            .name("Test")
            .mdo_type(MdoType::InformationRegister)
            .build();

        assert_eq!(reg1, reg2);
    }

    #[test]
    fn test_register_mdo_object_trait() {
        let register =
            Register::builder().name("TestRegister").mdo_type(MdoType::InformationRegister).build();

        let mdo: &dyn MdObject = &register;
        assert_eq!(mdo.name(), "TestRegister");

        let concrete = mdo.as_any().downcast_ref::<Register>();
        assert!(concrete.is_some());
        assert_eq!(concrete.unwrap().mdo_type(), MdoType::InformationRegister);
    }
}
