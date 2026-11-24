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

    /// `Балансовый` of an accounting-register resource; `None` for every other register kind.
    /// A non-balance resource of a correspondence register is named by its sides (`<Ресурс>Дт`)
    /// and is the only kind that has the correspondent-turnover fields (`<Ресурс>КорОборот`).
    #[serde(default)]
    balance: Option<bool>,

    #[serde(skip)]
    attr_type: Option<crate::metadata_object::AttributeType>,
}

impl RegisterResource {
    pub fn new(uuid: Uuid, name: impl Into<String>) -> Self {
        Self {
            uuid,
            name: name.into(),
            name_en: None,
            type_str: String::new(),
            balance: None,
            attr_type: None,
        }
    }

    pub fn balance(&self) -> Option<bool> {
        self.balance
    }

    pub fn set_balance(&mut self, balance: Option<bool>) {
        self.balance = balance;
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
    #[serde(default)]
    read_properties: crate::read_properties::ObjectReadProperties,
    #[serde(rename = "uuid")]
    uuid: Uuid,

    #[serde(rename = "objectBelonging", default)]
    object_belonging: ObjectBelonging,

    #[serde(rename = "extendedConfigurationObject", default)]
    extended_configuration_object: Option<Uuid>,

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

    /// `ПоддержкаКорреспонденции` of an accounting register; `None` for other kinds. It decides
    /// whether the records carry one account (`Счет`, `ВидДвижения`) or two (`СчетДт`, `СчетКт`).
    #[serde(rename = "correspondence", default)]
    correspondence: Option<bool>,

    /// Name of the chart of accounts an accounting register is kept on, without the
    /// `ChartOfAccounts.` prefix. Its `МаксКоличествоСубконто` fixes how many `Субконто<N>`
    /// fields the register's query tables have.
    #[serde(rename = "chartOfAccounts", default)]
    chart_of_accounts: Option<String>,
}

impl Register {
    pub fn read_properties(&self) -> &crate::read_properties::ObjectReadProperties {
        &self.read_properties
    }

    pub fn set_read_properties(&mut self, value: crate::read_properties::ObjectReadProperties) {
        self.read_properties = value;
    }
    pub fn builder() -> RegisterBuilder {
        RegisterBuilder::default()
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    /// Whether this declaration adopts the identified register.
    pub fn adopts(&self, base: &Register) -> bool {
        self.adopts_by_kind_and_name(base) && self.extended_configuration_object == Some(base.uuid)
    }

    /// Whether this declaration can be treated as a partial overlay when the target UUID is
    /// missing or stale. The caller still has to mark the resulting model open.
    pub fn adopts_by_kind_and_name(&self, base: &Register) -> bool {
        self.object_belonging == ObjectBelonging::Adopted
            && self.mdo_type == base.mdo_type
            && stdx::case::eq_ignore_case(&self.name, &base.name)
    }

    /// The ownership declared by the metadata export.
    pub fn object_belonging(&self) -> ObjectBelonging {
        self.object_belonging
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

    /// Narrow the standard recorder to the document movements visible to this caller.
    pub fn resolve_recorder_type(&mut self, documents: impl FnOnce() -> Vec<String>) {
        use crate::metadata_object::AttributeType;

        let Some(recorder) =
            self.attributes.iter_mut().find(|field| field.name_en() == Some("Recorder"))
        else {
            return;
        };
        let mut types: Vec<_> = documents()
            .into_iter()
            .map(|name| AttributeType::Ref { mdo_type: MdoType::Document, name })
            .collect();
        match types.len() {
            0 => recorder.set_attr_type(AttributeType::UnknownNamed(
                AttributeType::AnyObjectRef { mdo_type: MdoType::Document }.to_string(),
            )),
            1 => recorder.set_attr_type(types.pop().expect("one recorder type")),
            _ => recorder.set_attr_type(AttributeType::Composite { types }),
        }
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

    /// Known recorder subordination; an unread or unfamiliar write mode stays unknown.
    pub fn recorder_subordination(&self) -> Option<bool> {
        if self.mdo_type != MdoType::InformationRegister {
            return Some(true);
        }
        match self.read_properties.write_mode.as_deref() {
            Some("Independent") => Some(false),
            Some("RecorderSubordinate") => Some(true),
            _ => None,
        }
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

    pub fn correspondence(&self) -> Option<bool> {
        self.correspondence
    }

    pub fn chart_of_accounts(&self) -> Option<&str> {
        self.chart_of_accounts.as_deref()
    }

    /// Overlay an extension's adopted copy of this register onto the base: merge
    /// dimensions, resources, and attributes by name (1C lets an extension add new
    /// measurements/resources/attributes to a borrowed register, or replace a
    /// same-named one), and take the extension's scalar settings where it specifies
    /// them. Mirrors [`MetadataObject::apply_extension_overlay`](crate::MetadataObject::apply_extension_overlay)
    /// so the whole-config merge and the per-register resolver share one rule.
    pub fn apply_extension_overlay(&mut self, overlay: &Register) {
        self.read_properties.apply_overlay(&overlay.read_properties);
        for dimension in &overlay.dimensions {
            let mut dimension = dimension.clone();
            // An omitted Balance is "the XML did not say", not "this member is no longer
            // balance". Replacing a known flag with None makes every spelling provisional.
            if dimension.balance().is_none() {
                if let Some(existing) = self
                    .dimensions
                    .iter()
                    .find(|existing| stdx::case::eq_ignore_case(existing.name(), dimension.name()))
                {
                    dimension.set_balance(existing.balance());
                }
            }
            self.dimensions
                .retain(|existing| !stdx::case::eq_ignore_case(existing.name(), dimension.name()));
            self.dimensions.push(dimension);
        }
        for resource in &overlay.resources {
            let mut resource = resource.clone();
            if resource.balance().is_none() {
                if let Some(existing) = self
                    .resources
                    .iter()
                    .find(|existing| stdx::case::eq_ignore_case(existing.name(), resource.name()))
                {
                    resource.set_balance(existing.balance());
                }
            }
            self.resources
                .retain(|existing| !stdx::case::eq_ignore_case(existing.name(), resource.name()));
            self.resources.push(resource);
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
        if overlay.correspondence.is_some() {
            self.correspondence = overlay.correspondence;
        }
        if overlay.chart_of_accounts.is_some() {
            self.chart_of_accounts.clone_from(&overlay.chart_of_accounts);
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
            + self.read_properties.estimated_heap_size()
            + self.chart_of_accounts.as_ref().map_or(0, String::capacity)
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
        self.object_belonging
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Default)]
pub struct RegisterBuilder {
    uuid: Option<Uuid>,
    object_belonging: ObjectBelonging,
    extended_configuration_object: Option<Uuid>,
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
    correspondence: Option<bool>,
    chart_of_accounts: Option<String>,
}

impl RegisterBuilder {
    /// Set the ownership and confirmed target of an extension declaration.
    pub fn extension_ownership(mut self, belonging: ObjectBelonging, target: Option<Uuid>) -> Self {
        self.object_belonging = belonging;
        self.extended_configuration_object = target;
        self
    }

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

    pub fn correspondence(mut self, correspondence: Option<bool>) -> Self {
        self.correspondence = correspondence;
        self
    }

    pub fn chart_of_accounts(mut self, chart_of_accounts: Option<String>) -> Self {
        self.chart_of_accounts = chart_of_accounts;
        self
    }

    pub fn build(self) -> Register {
        Register {
            read_properties: Default::default(),
            uuid: self.uuid.unwrap_or_else(Uuid::new_v4),
            object_belonging: self.object_belonging,
            extended_configuration_object: self.extended_configuration_object,
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
            correspondence: self.correspondence,
            chart_of_accounts: self.chart_of_accounts,
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
    fn strict_adoption_still_requires_the_extended_configuration_uuid() {
        let target = Uuid::new_v4();
        let base = Register::builder()
            .uuid(target)
            .name("BaseRegister")
            .mdo_type(MdoType::InformationRegister)
            .build();
        let stale_overlay = Register::builder()
            .uuid(Uuid::new_v4())
            .extension_ownership(ObjectBelonging::Adopted, Some(Uuid::new_v4()))
            .name("baseregister")
            .mdo_type(MdoType::InformationRegister)
            .build();
        assert!(stale_overlay.adopts_by_kind_and_name(&base));
        assert!(!stale_overlay.adopts(&base));
        let confirmed_overlay = Register::builder()
            .uuid(Uuid::new_v4())
            .extension_ownership(ObjectBelonging::Adopted, Some(target))
            .name("BaseRegister")
            .mdo_type(MdoType::InformationRegister)
            .build();
        assert!(confirmed_overlay.adopts(&base));
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
    fn extension_overlay_keeps_balance_when_the_replacement_omits_it() {
        let mut base = Register::builder()
            .name("Бухгалтерия1")
            .mdo_type(MdoType::AccountingRegister)
            .dimensions(vec![Dimension::builder().name("Измерение1").balance(Some(false)).build()])
            .resources(vec![{
                let mut resource = RegisterResource::new(Uuid::new_v4(), "Ресурс1");
                resource.set_balance(Some(false));
                resource
            }])
            .build();
        let overlay = Register::builder()
            .name("Бухгалтерия1")
            .mdo_type(MdoType::AccountingRegister)
            .dimensions(vec![Dimension::builder().name("Измерение1").build()])
            .resources(vec![RegisterResource::new(Uuid::new_v4(), "Ресурс1")])
            .build();
        base.apply_extension_overlay(&overlay);
        assert_eq!(base.dimensions()[0].balance(), Some(false));
        assert_eq!(base.resources()[0].balance(), Some(false));

        let explicit = Register::builder()
            .name("Бухгалтерия1")
            .mdo_type(MdoType::AccountingRegister)
            .dimensions(vec![Dimension::builder().name("Измерение1").balance(Some(true)).build()])
            .build();
        base.apply_extension_overlay(&explicit);
        assert_eq!(base.dimensions()[0].balance(), Some(true));
    }

    #[test]
    fn extension_overlay_takes_correspondence_and_chart_when_specified() {
        let mut base = Register::builder()
            .name("Бухгалтерия1")
            .mdo_type(MdoType::AccountingRegister)
            .correspondence(Some(false))
            .chart_of_accounts(Some("План1".to_string()))
            .build();
        let overlay = Register::builder()
            .name("Бухгалтерия1")
            .mdo_type(MdoType::AccountingRegister)
            .correspondence(Some(true))
            .chart_of_accounts(Some("План2".to_string()))
            .build();
        base.apply_extension_overlay(&overlay);
        assert_eq!(base.correspondence(), Some(true));
        assert_eq!(base.chart_of_accounts(), Some("План2"));

        let silent =
            Register::builder().name("Бухгалтерия1").mdo_type(MdoType::AccountingRegister).build();
        base.apply_extension_overlay(&silent);
        assert_eq!(base.correspondence(), Some(true), "an omitted scalar stays with the base");
        assert_eq!(base.chart_of_accounts(), Some("План2"));
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
