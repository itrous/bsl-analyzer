use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::OnceLock;
use stdx::case::CaseExt;
use uuid::Uuid;

pub type Name = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MdoType {
    Catalog,
    Document,
    InformationRegister,
    AccumulationRegister,
    AccountingRegister,
    CalculationRegister,
    ChartOfCharacteristicTypes,
    ChartOfAccounts,
    ChartOfCalculationTypes,
    BusinessProcess,
    Task,
    Enum,
    ExchangePlan,
    ExternalDataSource,
    Constant,
    DataProcessor,
    Report,
    /// An external data processor (`ВнешняяОбработка`): an EPF export analyzed as
    /// its own source root. Like the reference-less kinds below it never surfaces
    /// in [`MdoType::all`] or as a manager collection — `ВнешниеОбработки` is the
    /// platform's own manager (`Создать`, `Подключить`), not a collection of
    /// named objects — but unlike them it is a data-bearing object with
    /// attributes, tabular sections, forms and an object type of its own.
    ExternalDataProcessor,
    /// An external report (`ВнешнийОтчет`): the ERF counterpart of
    /// [`MdoType::ExternalDataProcessor`].
    ExternalReport,
    CommonModule,
    /// An event subscription (`ПодпискаНаСобытие`). Unlike the data-bearing objects
    /// above it has no manager, ref type, or attributes — it only binds a platform
    /// event to a handler method. It is deliberately excluded from [`MdoType::all`]
    /// so it never surfaces in metadata-object enumeration/completion, and every
    /// type-facing mapping (manager prefix, `MetadataKind`) yields `None` for it.
    EventSubscription,
    /// A subsystem (`Подсистема`) — an organisational container listing member objects
    /// and child subsystems. Like [`MdoType::EventSubscription`] it is not a data-bearing,
    /// manager-backed type: it is excluded from [`MdoType::all`] and every type-facing
    /// mapping yields `None`. It exists so the call graph can carry subsystem-membership
    /// edges to a subsystem node.
    Subsystem,
    /// A role (`Роль`) — an access-rights container listing the metadata objects it grants
    /// rights on, optionally with row-level-security restriction conditions. Like
    /// [`MdoType::EventSubscription`] / [`MdoType::Subsystem`] it is not a data-bearing,
    /// manager-backed type: it is excluded from [`MdoType::all`] and every type-facing
    /// mapping yields `None`. It exists so the call graph can carry role → object reference
    /// edges to a role node.
    Role,
}

impl FromStr for MdoType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.fold_lower().as_str() {
            "справочник" | "catalog" => Ok(Self::Catalog),
            "документ" | "document" => Ok(Self::Document),
            "регистрсведений" | "informationregister" => {
                Ok(Self::InformationRegister)
            }
            "регистрнакопления" | "accumulationregister" => {
                Ok(Self::AccumulationRegister)
            }
            "регистрбухгалтерии" | "accountingregister" => {
                Ok(Self::AccountingRegister)
            }
            "регистррасчета" | "calculationregister" => Ok(Self::CalculationRegister),
            "планвидовхарактеристик" | "chartofcharacteristictypes" => {
                Ok(Self::ChartOfCharacteristicTypes)
            }
            "плансчетов" | "chartofaccounts" => Ok(Self::ChartOfAccounts),
            "планвидоврасчета" | "chartofcalculationtypes" => {
                Ok(Self::ChartOfCalculationTypes)
            }
            "бизнеспроцесс" | "businessprocess" => Ok(Self::BusinessProcess),
            "задача" | "task" => Ok(Self::Task),
            "перечисление" | "enum" => Ok(Self::Enum),
            "планобмена" | "exchangeplan" => Ok(Self::ExchangePlan),
            "внешнийисточникданных" | "externaldatasource" => {
                Ok(Self::ExternalDataSource)
            }
            "константа" | "constant" => Ok(Self::Constant),
            "обработка" | "dataprocessor" => Ok(Self::DataProcessor),
            "внешняяобработка" | "externaldataprocessor" => {
                Ok(Self::ExternalDataProcessor)
            }
            "внешнийотчет" | "внешнийотчёт" | "externalreport" => {
                Ok(Self::ExternalReport)
            }
            "отчет" | "report" => Ok(Self::Report),
            "общиймодуль" | "commonmodule" => Ok(Self::CommonModule),
            "подписканасобытие" | "eventsubscription" => {
                Ok(Self::EventSubscription)
            }
            "подсистема" | "subsystem" => Ok(Self::Subsystem),
            "роль" | "role" => Ok(Self::Role),
            _ => Err(format!("Unknown MDO type: {}", s)),
        }
    }
}

impl MdoType {
    /// Whether this kind is a register (its objects live in `Configuration`'s
    /// register list, resolved by `find_register_by_type_and_name`, not the
    /// metadata-object list).
    pub fn is_register(&self) -> bool {
        matches!(
            self,
            Self::InformationRegister
                | Self::AccumulationRegister
                | Self::AccountingRegister
                | Self::CalculationRegister
        )
    }

    pub fn russian_name(&self) -> &'static str {
        match self {
            Self::Catalog => "Справочник",
            Self::Document => "Документ",
            Self::InformationRegister => "РегистрСведений",
            Self::AccumulationRegister => "РегистрНакопления",
            Self::AccountingRegister => "РегистрБухгалтерии",
            Self::CalculationRegister => "РегистрРасчета",
            Self::ChartOfCharacteristicTypes => "ПланВидовХарактеристик",
            Self::ChartOfAccounts => "ПланСчетов",
            Self::ChartOfCalculationTypes => "ПланВидовРасчета",
            Self::BusinessProcess => "БизнесПроцесс",
            Self::Task => "Задача",
            Self::Enum => "Перечисление",
            Self::ExchangePlan => "ПланОбмена",
            Self::ExternalDataSource => "ВнешнийИсточникДанных",
            Self::Constant => "Константа",
            Self::DataProcessor => "Обработка",
            Self::Report => "Отчет",
            Self::ExternalDataProcessor => "ВнешняяОбработка",
            Self::ExternalReport => "ВнешнийОтчет",
            Self::CommonModule => "ОбщийМодуль",
            Self::EventSubscription => "ПодпискаНаСобытие",
            Self::Subsystem => "Подсистема",
            Self::Role => "Роль",
        }
    }

    pub fn english_name(&self) -> &'static str {
        match self {
            Self::Catalog => "Catalog",
            Self::Document => "Document",
            Self::InformationRegister => "InformationRegister",
            Self::AccumulationRegister => "AccumulationRegister",
            Self::AccountingRegister => "AccountingRegister",
            Self::CalculationRegister => "CalculationRegister",
            Self::ChartOfCharacteristicTypes => "ChartOfCharacteristicTypes",
            Self::ChartOfAccounts => "ChartOfAccounts",
            Self::ChartOfCalculationTypes => "ChartOfCalculationTypes",
            Self::BusinessProcess => "BusinessProcess",
            Self::Task => "Task",
            Self::Enum => "Enum",
            Self::ExchangePlan => "ExchangePlan",
            Self::ExternalDataSource => "ExternalDataSource",
            Self::Constant => "Constant",
            Self::DataProcessor => "DataProcessor",
            Self::Report => "Report",
            Self::ExternalDataProcessor => "ExternalDataProcessor",
            Self::ExternalReport => "ExternalReport",
            Self::CommonModule => "CommonModule",
            Self::EventSubscription => "EventSubscription",
            Self::Subsystem => "Subsystem",
            Self::Role => "Role",
        }
    }

    /// The Russian collection name a manager-backed object is addressed through
    /// (`Справочники.Товары`). `None` for the kinds that own no manager module
    /// and therefore have no collection — they are named directly.
    ///
    /// The inverse of [`Self::from_plural`], which also accepts the English
    /// spellings; this one canonicalises to Russian, so a diagnostic naming a
    /// receiver reads the same however the source spelled it.
    pub fn russian_plural(&self) -> Option<&'static str> {
        Some(match self {
            Self::Catalog => "Справочники",
            Self::Document => "Документы",
            Self::InformationRegister => "РегистрыСведений",
            Self::AccumulationRegister => "РегистрыНакопления",
            Self::AccountingRegister => "РегистрыБухгалтерии",
            Self::CalculationRegister => "РегистрыРасчета",
            Self::ChartOfCharacteristicTypes => "ПланыВидовХарактеристик",
            Self::ChartOfAccounts => "ПланыСчетов",
            Self::ChartOfCalculationTypes => "ПланыВидовРасчета",
            Self::BusinessProcess => "БизнесПроцессы",
            Self::Task => "Задачи",
            Self::Enum => "Перечисления",
            Self::ExchangePlan => "ПланыОбмена",
            Self::ExternalDataSource => "ВнешниеИсточникиДанных",
            Self::Constant => "Константы",
            Self::DataProcessor => "Обработки",
            Self::Report => "Отчеты",
            Self::ExternalDataProcessor
            | Self::ExternalReport
            | Self::CommonModule
            | Self::EventSubscription
            | Self::Subsystem
            | Self::Role => return None,
        })
    }

    pub fn is_russian_keyword(&self, keyword: &str) -> bool {
        let keyword_lower = keyword.fold_lower();
        let russian_lower = self.russian_name().fold_lower();
        keyword_lower == russian_lower
    }

    pub fn all() -> &'static [MdoType] {
        &[
            Self::Catalog,
            Self::Document,
            Self::InformationRegister,
            Self::AccumulationRegister,
            Self::AccountingRegister,
            Self::CalculationRegister,
            Self::ChartOfCharacteristicTypes,
            Self::ChartOfAccounts,
            Self::ChartOfCalculationTypes,
            Self::BusinessProcess,
            Self::Task,
            Self::Enum,
            Self::ExchangePlan,
            Self::ExternalDataSource,
            Self::Constant,
            Self::DataProcessor,
            Self::Report,
            Self::CommonModule,
        ]
    }

    pub fn from_plural(s: &str) -> Option<Self> {
        match s.fold_lower().as_str() {
            "документы" | "documents" => Some(Self::Document),
            "справочники" | "catalogs" => Some(Self::Catalog),
            "регистрысведений" | "informationregisters" => {
                Some(Self::InformationRegister)
            }
            "регистрынакопления" | "accumulationregisters" => {
                Some(Self::AccumulationRegister)
            }
            "регистрыбухгалтерии" | "accountingregisters" => {
                Some(Self::AccountingRegister)
            }
            "регистрырасчета" | "calculationregisters" => {
                Some(Self::CalculationRegister)
            }
            "планывидовхарактеристик" | "chartsofcharacteristictypes" => {
                Some(Self::ChartOfCharacteristicTypes)
            }
            "планысчетов" | "chartsofaccounts" => Some(Self::ChartOfAccounts),
            "планывидоврасчета" | "chartsofcalculationtypes" => {
                Some(Self::ChartOfCalculationTypes)
            }
            "бизнеспроцессы" | "businessprocesses" => Some(Self::BusinessProcess),
            "задачи" | "tasks" => Some(Self::Task),
            "перечисления" | "enums" => Some(Self::Enum),
            "планыобмена" | "exchangeplans" => Some(Self::ExchangePlan),
            "внешниеисточникиданных" | "externaldatasources" => {
                Some(Self::ExternalDataSource)
            }
            "константы" | "constants" => Some(Self::Constant),
            "обработки" | "dataprocessors" => Some(Self::DataProcessor),
            "отчеты" | "reports" => Some(Self::Report),
            "общиемодули" | "commonmodules" => Some(Self::CommonModule),
            _ => None,
        }
    }

    pub fn hbk_global_property(self) -> Option<&'static bsl_platform::PlatformProperty> {
        static HBK_MAP: OnceLock<
            rustc_hash::FxHashMap<MdoType, &'static bsl_platform::PlatformProperty>,
        > = OnceLock::new();
        HBK_MAP
            .get_or_init(|| {
                let mut m: rustc_hash::FxHashMap<MdoType, &'static bsl_platform::PlatformProperty> =
                    rustc_hash::FxHashMap::default();
                let data: &'static bsl_platform::PlatformDataInner =
                    bsl_platform::PlatformDataInner::instance();
                for prop in data.all_global_properties() {
                    let Some(t) = MdoType::from_plural(prop.name.as_str())
                        .or_else(|| MdoType::from_plural(prop.english_name.as_str()))
                    else {
                        continue;
                    };
                    m.insert(t, prop);
                }
                m
            })
            .get(&self)
            .copied()
    }

    pub fn manager_type_prefix(&self) -> Option<&'static str> {
        match self {
            Self::Catalog => Some("CatalogManager"),
            Self::Document => Some("DocumentManager"),
            Self::InformationRegister => Some("InformationRegisterManager"),
            Self::AccumulationRegister => Some("AccumulationRegisterManager"),
            Self::AccountingRegister => Some("AccountingRegisterManager"),
            Self::CalculationRegister => Some("CalculationRegisterManager"),
            Self::ChartOfCharacteristicTypes => Some("ChartOfCharacteristicTypesManager"),
            Self::ChartOfAccounts => Some("ChartOfAccountsManager"),
            Self::ChartOfCalculationTypes => Some("ChartOfCalculationTypesManager"),
            Self::BusinessProcess => Some("BusinessProcessManager"),
            Self::Task => Some("TaskManager"),
            Self::Enum => Some("EnumManager"),
            Self::ExchangePlan => Some("ExchangePlanManager"),
            Self::ExternalDataSource => Some("ExternalDataSourceManager"),
            Self::Constant => Some("ConstantManager"),
            Self::DataProcessor => Some("DataProcessorManager"),
            Self::Report => Some("ReportManager"),
            Self::ExternalDataProcessor
            | Self::ExternalReport
            | Self::CommonModule
            | Self::EventSubscription
            | Self::Subsystem
            | Self::Role => None,
        }
    }

    pub fn manager_type_prefix_ru(&self) -> Option<&'static str> {
        match self {
            Self::Catalog => Some("СправочникМенеджер"),
            Self::Document => Some("ДокументМенеджер"),
            Self::InformationRegister => Some("РегистрСведенийМенеджер"),
            Self::AccumulationRegister => Some("РегистрНакопленияМенеджер"),
            Self::AccountingRegister => Some("РегистрБухгалтерииМенеджер"),
            Self::CalculationRegister => Some("РегистрРасчетаМенеджер"),
            Self::ChartOfCharacteristicTypes => Some("ПланВидовХарактеристикМенеджер"),
            Self::ChartOfAccounts => Some("ПланСчетовМенеджер"),
            Self::ChartOfCalculationTypes => Some("ПланВидовРасчетаМенеджер"),
            Self::BusinessProcess => Some("БизнесПроцессМенеджер"),
            Self::Task => Some("ЗадачаМенеджер"),
            Self::Enum => Some("ПеречислениеМенеджер"),
            Self::ExchangePlan => Some("ПланОбменаМенеджер"),
            Self::ExternalDataSource => Some("ВнешнийИсточникДанныхМенеджер"),
            Self::Constant => Some("КонстантаМенеджер"),
            Self::DataProcessor => Some("ОбработкаМенеджер"),
            Self::Report => Some("ОтчетМенеджер"),
            Self::ExternalDataProcessor
            | Self::ExternalReport
            | Self::CommonModule
            | Self::EventSubscription
            | Self::Subsystem
            | Self::Role => None,
        }
    }

    pub fn is_plural_form(s: &str) -> bool {
        static PLURAL_FORMS: OnceLock<FxHashSet<String>> = OnceLock::new();

        let set = PLURAL_FORMS.get_or_init(|| {
            let mut set = FxHashSet::default();
            set.insert("документы".to_string());
            set.insert("documents".to_string());
            set.insert("справочники".to_string());
            set.insert("catalogs".to_string());
            set.insert("регистрысведений".to_string());
            set.insert("informationregisters".to_string());
            set.insert("регистрынакопления".to_string());
            set.insert("accumulationregisters".to_string());
            set.insert("регистрыбухгалтерии".to_string());
            set.insert("accountingregisters".to_string());
            set.insert("регистрырасчета".to_string());
            set.insert("calculationregisters".to_string());
            set.insert("планывидовхарактеристик".to_string());
            set.insert("chartsofcharacteristictypes".to_string());
            set.insert("планысчетов".to_string());
            set.insert("chartsofaccounts".to_string());
            set.insert("планывидоврасчета".to_string());
            set.insert("chartsofcalculationtypes".to_string());
            set.insert("бизнеспроцессы".to_string());
            set.insert("businessprocesses".to_string());
            set.insert("задачи".to_string());
            set.insert("tasks".to_string());
            set.insert("перечисления".to_string());
            set.insert("enums".to_string());
            set.insert("планыобмена".to_string());
            set.insert("exchangeplans".to_string());
            set.insert("внешниеисточникиданных".to_string());
            set.insert("externaldatasources".to_string());
            set.insert("константы".to_string());
            set.insert("constants".to_string());
            set.insert("обработки".to_string());
            set.insert("dataprocessors".to_string());
            set.insert("отчеты".to_string());
            set.insert("отчёты".to_string());
            set.insert("reports".to_string());
            set.insert("общиемодули".to_string());
            set.insert("commonmodules".to_string());
            set
        });

        set.contains(&s.fold_lower())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumValue {
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
    pub uuid: String,
}

impl EnumValue {
    /// Heap bytes owned by this value: its name strings plus the `uuid` text
    /// (stored as a plain `String`, unlike the parsed [`uuid::Uuid`] elsewhere).
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.name_en.as_ref().map_or(0, String::capacity)
            + self.uuid.capacity()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredefinedItem {
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
    pub uuid: String,
}

impl PredefinedItem {
    /// Heap bytes owned by this item: its name strings plus the `uuid` text.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.name_en.as_ref().map_or(0, String::capacity)
            + self.uuid.capacity()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataObject {
    pub mdo_type: MdoType,
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
    #[serde(default)]
    pub attributes: Vec<Attribute>,
    #[serde(default)]
    pub tabular_sections: Vec<crate::tabular_section::TabularSection>,
    #[serde(default)]
    pub children: Vec<MetadataObject>,
    #[serde(default)]
    pub enum_values: Vec<EnumValue>,
    #[serde(default)]
    pub predefined_items: Vec<PredefinedItem>,
    #[serde(default)]
    pub check_unique: bool,
    #[serde(default)]
    pub code_series: crate::enums::CodeSeries,
    #[serde(default)]
    pub constant_type: Option<AttributeType>,
    #[serde(default)]
    pub register_records: Vec<(MdoType, Name)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<Uuid>,
    #[serde(default)]
    pub object_belonging: crate::ObjectBelonging,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extended_configuration_object: Option<Uuid>,
    /// Fields contributed by common attributes whose composition includes this object. Kept
    /// apart from `attributes` because they are not declared by the object: checks of the
    /// object's own structure must not report them as its members.
    #[serde(default)]
    pub common_attributes: Vec<crate::common_attribute::CommonAttributeField>,
    /// Whether a common attribute may apply to this object without being listed in
    /// `common_attributes` (an unreadable `CommonAttributes/*.xml` or `Content` entry). The
    /// object's field set is then not known to be exhaustive.
    #[serde(default)]
    pub common_attributes_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
    pub attr_type: AttributeType,
}

impl Attribute {
    /// Heap bytes owned by this attribute: its name strings plus its type's own
    /// owned payload.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.name_en.as_ref().map_or(0, String::capacity)
            + self.attr_type.estimated_heap_size()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandardAttributeKind {
    Code { length: u32 },
    Description { length: u32 },
    Ref,
    DeletionMark,
    IsFolder,
    Owner,
    Parent,
    Predefined,
    PredefinedDataName,

    Number { length: u32 },
    Date,
    Posted,
    Started,
    Completed,
    HeadTask,
    Executed,
    TaskBusinessProcess,
    RoutePoint,

    ThisNode,

    ValueType,

    Order,

    Active,
    LineNumber,
    Recorder,
    Period,
}

impl StandardAttributeKind {
    /// Every standard-attribute kind. The single enumeration the name helpers below
    /// derive from, so they cannot drift from the variant set (a missing entry fails
    /// `standard_attribute_names_cover_every_kind`). Variant payloads are placeholders
    /// — only the discriminant matters for naming.
    pub const ALL: &'static [StandardAttributeKind] = &[
        Self::Code { length: 0 },
        Self::Description { length: 0 },
        Self::Ref,
        Self::DeletionMark,
        Self::IsFolder,
        Self::Owner,
        Self::Parent,
        Self::Predefined,
        Self::PredefinedDataName,
        Self::Number { length: 0 },
        Self::Date,
        Self::Posted,
        Self::Started,
        Self::Completed,
        Self::HeadTask,
        Self::Executed,
        Self::TaskBusinessProcess,
        Self::RoutePoint,
        Self::ThisNode,
        Self::ValueType,
        Self::Order,
        Self::Active,
        Self::LineNumber,
        Self::Recorder,
        Self::Period,
    ];

    pub fn attribute_type(&self, mdo_type: MdoType, object_name: &str) -> AttributeType {
        match self {
            Self::Code { length } => AttributeType::String { length: Some(*length) },
            Self::Description { length } => AttributeType::String { length: Some(*length) },
            Self::Ref => AttributeType::Ref { mdo_type, name: object_name.to_string() },
            Self::DeletionMark => AttributeType::Boolean,
            Self::IsFolder => AttributeType::Boolean,
            Self::Owner => AttributeType::Unknown,
            Self::Parent => AttributeType::Ref { mdo_type, name: object_name.to_string() },
            Self::Predefined => AttributeType::Boolean,
            Self::PredefinedDataName => AttributeType::String { length: None },
            Self::Number { length } => AttributeType::String { length: Some(*length) },
            Self::Date => AttributeType::DateTime,
            Self::Posted => AttributeType::Boolean,
            Self::Started => AttributeType::Boolean,
            Self::Completed => AttributeType::Boolean,
            Self::HeadTask => AttributeType::Unknown,
            Self::Executed => AttributeType::Boolean,
            Self::TaskBusinessProcess => AttributeType::Unknown,
            Self::RoutePoint => AttributeType::Unknown,
            Self::ThisNode => AttributeType::Boolean,
            Self::ValueType => AttributeType::Unknown,
            Self::Order => AttributeType::String { length: None },
            Self::Active => AttributeType::Boolean,
            Self::LineNumber => AttributeType::Number { precision: 10, scale: 0 },
            Self::Recorder => AttributeType::AnyObjectRef { mdo_type: MdoType::Document },
            Self::Period => AttributeType::DateTime,
        }
    }

    pub fn russian_name(&self) -> &'static str {
        match self {
            Self::Code { .. } => "Код",
            Self::Description { .. } => "Наименование",
            Self::Ref => "Ссылка",
            Self::DeletionMark => "ПометкаУдаления",
            Self::IsFolder => "ЭтоГруппа",
            Self::Owner => "Владелец",
            Self::Parent => "Родитель",
            Self::Predefined => "Предопределенный",
            Self::PredefinedDataName => "ИмяПредопределенныхДанных",
            Self::Number { .. } => "Номер",
            Self::Date => "Дата",
            Self::Posted => "Проведен",
            Self::Started => "Стартован",
            Self::Completed => "Завершен",
            Self::HeadTask => "ГлавнаяЗадача",
            Self::Executed => "Выполнена",
            Self::TaskBusinessProcess => "БизнесПроцесс",
            Self::RoutePoint => "ТочкаМаршрута",
            Self::ThisNode => "ЭтотУзел",
            Self::ValueType => "ТипЗначения",
            Self::Order => "Порядок",
            Self::Active => "Активность",
            Self::LineNumber => "НомерСтроки",
            Self::Recorder => "Регистратор",
            Self::Period => "Период",
        }
    }

    pub fn english_name(&self) -> &'static str {
        match self {
            Self::Code { .. } => "Code",
            Self::Description { .. } => "Description",
            Self::Ref => "Ref",
            Self::DeletionMark => "DeletionMark",
            Self::IsFolder => "IsFolder",
            Self::Owner => "Owner",
            Self::Parent => "Parent",
            Self::Predefined => "Predefined",
            Self::PredefinedDataName => "PredefinedDataName",
            Self::Number { .. } => "Number",
            Self::Date => "Date",
            Self::Posted => "Posted",
            Self::Started => "Started",
            Self::Completed => "Completed",
            Self::HeadTask => "HeadTask",
            Self::Executed => "Executed",
            Self::TaskBusinessProcess => "BusinessProcess",
            Self::RoutePoint => "RoutePoint",
            Self::ThisNode => "ThisNode",
            Self::ValueType => "ValueType",
            Self::Order => "Order",
            Self::Active => "Active",
            Self::LineNumber => "LineNumber",
            Self::Recorder => "Recorder",
            Self::Period => "Period",
        }
    }
}

/// Whether `name` is the name (Russian or English) of a platform standard attribute —
/// one of the fields `add_*_standard_attributes` synthesises onto every object during
/// configuration load. Derived from [`StandardAttributeKind::ALL`], so it is the single
/// source of truth: a consumer that wants only user-declared attributes (the call-graph
/// catalog pass, the SDBL scope builder) filters against this. Case-insensitive.
///
/// Name-based by necessity — the synthesised attributes are merged into the same
/// `attributes` list as user ones with no surviving marker — so a user attribute that
/// happens to be named exactly like a standard one is treated as standard.
pub fn is_standard_attribute_name(name: &str) -> bool {
    // Unicode-aware fold: `eq_ignore_ascii_case` does not fold Cyrillic (С ≠ с), so the
    // Russian names need full lowercasing. The kind names are constant, so fold them
    // once into a set rather than on every call (this is a hot resolution check).
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        StandardAttributeKind::ALL
            .iter()
            .flat_map(|kind| [kind.russian_name().fold_lower(), kind.english_name().fold_lower()])
            .collect()
    });
    names.contains(&name.fold_lower())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttributeType {
    String {
        length: Option<u32>,
    },
    Number {
        precision: u8,
        scale: u8,
    },
    Boolean,
    Date,
    DateTime,
    Ref {
        mdo_type: MdoType,
        name: String,
    },
    /// `cfg:InformationRegisterRecordManager.<Имя>` — the type of the main attribute
    /// `Запись` of an information register record form. Only information registers
    /// have a record manager, so the register kind is implied.
    InformationRegisterRecordManager {
        name: String,
    },
    AnyRef,
    AnyObjectRef {
        mdo_type: MdoType,
    },
    Uuid,
    ValueStorage,
    DefinedType {
        name: String,
    },
    Composite {
        types: Vec<AttributeType>,
    },
    Platform(PlatformValueType),
    /// A platform type identified by its canonical Russian name, resolved from
    /// the platform catalogue by stripping the XML namespace prefix
    /// (`v8ui:`, `mxl:`, `d5p1:`, …) and looking the local name up there.
    PlatformNamed(String),
    /// A type name preserved from XML when this build cannot resolve it. Keeping the
    /// source token lets metadata consumers report an honest partial type description.
    UnknownNamed(String),
    Unknown,
}

impl AttributeType {
    /// Heap bytes owned by this type: the `name`/`types` payload of the
    /// name-carrying and composite variants; every other variant is a plain enum
    /// discriminant (plus `Copy` payloads) and owns no heap.
    pub fn estimated_heap_size(&self) -> usize {
        match self {
            Self::Ref { name, .. }
            | Self::InformationRegisterRecordManager { name }
            | Self::DefinedType { name }
            | Self::PlatformNamed(name) => name.capacity(),
            Self::Composite { types } => {
                stdx::heap::vec_bytes::<AttributeType>(types.len())
                    + types.iter().map(AttributeType::estimated_heap_size).sum::<usize>()
            }
            Self::UnknownNamed(name) => name.capacity(),
            Self::String { .. }
            | Self::Number { .. }
            | Self::Boolean
            | Self::Date
            | Self::DateTime
            | Self::AnyRef
            | Self::AnyObjectRef { .. }
            | Self::Uuid
            | Self::ValueStorage
            | Self::Platform(_)
            | Self::Unknown => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformValueType {
    ValueList,
    ValueTable,
    ValueTree,
    StandardPeriod,
    StandardBeginningDate,
    TypeDescription,
    FixedStructure,
    FixedArray,
    FixedMap,
    Type,
    Null,
    FormattedString,
    SpreadsheetDocument,
    FormattedDocument,
    Picture,
    Color,
    Font,
    Chart,
    GanttChart,
    SettingsComposer,
    DataCompositionFilter,
    DynamicList,
    ConstantsSet,
    ReportBuilder,
}

impl PlatformValueType {
    pub fn russian_name(self) -> &'static str {
        match self {
            Self::ValueList => "СписокЗначений",
            Self::ValueTable => "ТаблицаЗначений",
            Self::ValueTree => "ДеревоЗначений",
            Self::StandardPeriod => "СтандартныйПериод",
            Self::StandardBeginningDate => "СтандартнаяДатаНачала",
            Self::TypeDescription => "ОписаниеТипов",
            Self::FixedStructure => "ФиксированнаяСтруктура",
            Self::FixedArray => "ФиксированныйМассив",
            Self::FixedMap => "ФиксированноеСоответствие",
            Self::Type => "Тип",
            Self::Null => "Null",
            Self::FormattedString => "ФорматированнаяСтрока",
            Self::SpreadsheetDocument => "ТабличныйДокумент",
            Self::FormattedDocument => "ФорматированныйДокумент",
            Self::Picture => "Картинка",
            Self::Color => "Цвет",
            Self::Font => "Шрифт",
            Self::Chart => "Диаграмма",
            Self::GanttChart => "ДиаграммаГанта",
            Self::SettingsComposer => "КомпоновщикНастроекКомпоновкиДанных",
            Self::DataCompositionFilter => "ОтборКомпоновкиДанных",
            Self::DynamicList => "ДинамическийСписок",
            Self::ConstantsSet => "КонстантыНабор",
            Self::ReportBuilder => "ПостроительОтчета",
        }
    }
}

impl MetadataObject {
    pub fn new(mdo_type: MdoType, name: impl Into<String>) -> Self {
        Self {
            mdo_type,
            name: name.into(),
            name_en: None,
            attributes: Vec::new(),
            tabular_sections: Vec::new(),
            children: Vec::new(),
            enum_values: Vec::new(),
            predefined_items: Vec::new(),
            check_unique: false,
            code_series: crate::enums::CodeSeries::default(),
            constant_type: None,
            register_records: Vec::new(),
            uuid: None,
            object_belonging: crate::ObjectBelonging::Own,
            extended_configuration_object: None,
            common_attributes: Vec::new(),
            common_attributes_open: false,
        }
    }

    pub fn with_details(
        mdo_type: MdoType,
        name: impl Into<String>,
        name_en: Option<String>,
        attributes: Vec<Attribute>,
    ) -> Self {
        Self {
            mdo_type,
            name: name.into(),
            name_en,
            attributes,
            tabular_sections: Vec::new(),
            children: Vec::new(),
            enum_values: Vec::new(),
            predefined_items: Vec::new(),
            check_unique: false,
            code_series: crate::enums::CodeSeries::default(),
            constant_type: None,
            register_records: Vec::new(),
            uuid: None,
            object_belonging: crate::ObjectBelonging::Own,
            extended_configuration_object: None,
            common_attributes: Vec::new(),
            common_attributes_open: false,
        }
    }

    pub fn register_records(&self) -> &[(MdoType, Name)] {
        &self.register_records
    }

    pub fn set_register_records(&mut self, records: Vec<(MdoType, Name)>) {
        self.register_records = records;
    }

    /// Apply an extension overlay onto this object, replacing same-named members,
    /// merging same-named tabular sections and recursing into children. Names
    /// match case-insensitively, Cyrillic included. This is the single source of
    /// truth for per-object extension merge semantics, shared by whole-configuration merge
    /// ([`crate::Configuration::merge_extension_overlay`]) and per-object
    /// resolution.
    pub fn apply_extension_overlay(&mut self, overlay: &MetadataObject) {
        if overlay.name_en.is_some() {
            self.name_en = overlay.name_en.clone();
        }
        if overlay.constant_type.is_some() {
            self.constant_type = overlay.constant_type.clone();
        }

        for attr in &overlay.attributes {
            self.attributes
                .retain(|existing| !stdx::case::eq_ignore_case(&existing.name, &attr.name));
            self.attributes.push(attr.clone());
        }

        for tabular_section in &overlay.tabular_sections {
            if let Some(base_section) = self.tabular_sections.iter_mut().find(|existing| {
                stdx::case::eq_ignore_case(existing.name(), tabular_section.name())
            }) {
                base_section.apply_extension_overlay(tabular_section);
            } else {
                self.tabular_sections.push(tabular_section.clone());
            }
        }

        for child in &overlay.children {
            if let Some(base_child) = self.children.iter_mut().find(|existing| {
                existing.mdo_type == child.mdo_type
                    && stdx::case::eq_ignore_case(&existing.name, &child.name)
            }) {
                base_child.apply_extension_overlay(child);
            } else {
                self.children.push(child.clone());
            }
        }

        for enum_value in &overlay.enum_values {
            self.enum_values
                .retain(|existing| !stdx::case::eq_ignore_case(&existing.name, &enum_value.name));
            self.enum_values.push(enum_value.clone());
        }

        for predefined_item in &overlay.predefined_items {
            self.predefined_items.retain(|existing| {
                !stdx::case::eq_ignore_case(&existing.name, &predefined_item.name)
            });
            self.predefined_items.push(predefined_item.clone());
        }

        if !overlay.register_records().is_empty() {
            self.set_register_records(overlay.register_records().to_vec());
        }

        crate::common_attribute::merge_common_attribute_fields(
            &mut self.common_attributes,
            &overlay.common_attributes,
        );
        self.common_attributes_open |= overlay.common_attributes_open;
    }

    /// Record which common attributes apply to this object.
    pub fn set_common_attributes(
        &mut self,
        applied: crate::common_attribute::ObjectCommonAttributes,
    ) {
        self.common_attributes = applied.fields;
        self.common_attributes_open = applied.open;
    }

    pub fn uuid(&self) -> Option<&Uuid> {
        self.uuid.as_ref()
    }

    pub fn object_belonging(&self) -> crate::ObjectBelonging {
        self.object_belonging
    }

    pub fn extends_uuid(&self) -> Option<&Uuid> {
        self.extended_configuration_object.as_ref()
    }

    pub fn set_object_belonging(&mut self, value: crate::ObjectBelonging) {
        self.object_belonging = value;
    }

    pub fn set_extends_uuid(&mut self, value: Uuid) {
        self.extended_configuration_object = Some(value);
    }

    pub fn adopts(&self, base: &MetadataObject) -> bool {
        self.object_belonging == crate::ObjectBelonging::Adopted
            && self.mdo_type == base.mdo_type
            && stdx::case::eq_ignore_case(&self.name, &base.name)
            && base.uuid.as_ref().is_some_and(|uuid| Some(uuid) == self.extends_uuid())
    }

    pub fn set_uuid(&mut self, uuid: Uuid) {
        self.uuid = Some(uuid);
    }

    pub fn set_constant_type(&mut self, value: AttributeType) {
        self.constant_type = Some(value);
    }

    pub fn set_check_unique(&mut self, value: bool) {
        self.check_unique = value;
    }

    pub fn set_code_series(&mut self, value: crate::enums::CodeSeries) {
        self.code_series = value;
    }

    pub fn is_find_by_code_safe(&self) -> bool {
        match self.mdo_type {
            MdoType::Catalog | MdoType::ChartOfCharacteristicTypes | MdoType::ChartOfAccounts => {
                self.check_unique && self.code_series.is_whole()
            }
            // A chart of calculation types is flat (no hierarchy/owner) and has no
            // code-series scoping, so enabled uniqueness control already makes codes
            // unique across the whole chart — НайтиПоКоду is then unambiguous.
            MdoType::ChartOfCalculationTypes => self.check_unique,
            _ => true,
        }
    }

    pub fn add_child(&mut self, child: MetadataObject) {
        self.children.push(child);
    }

    pub fn add_attribute(&mut self, attribute: Attribute) {
        self.attributes.push(attribute);
    }

    pub fn add_tabular_section(&mut self, tabular_section: crate::tabular_section::TabularSection) {
        self.tabular_sections.push(tabular_section);
    }

    pub fn find_child(&self, name: &str) -> Option<&MetadataObject> {
        self.children.iter().find(|child| stdx::case::eq_ignore_case(&child.name, name))
    }

    pub fn find_attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attr| stdx::case::eq_ignore_case(&attr.name, name))
    }

    pub fn find_tabular_section(
        &self,
        name: &str,
    ) -> Option<&crate::tabular_section::TabularSection> {
        self.tabular_sections.iter().find(|ts| {
            stdx::case::eq_ignore_case(ts.name(), name)
                || ts.name_en().is_some_and(|en| stdx::case::eq_ignore_case(en, name))
        })
    }

    pub fn find_enum_value(&self, name: &str) -> Option<&EnumValue> {
        self.enum_values.iter().find(|ev| {
            stdx::case::eq_ignore_case(&ev.name, name)
                || ev.name_en.as_ref().is_some_and(|en| stdx::case::eq_ignore_case(en, name))
        })
    }

    pub fn find_predefined_item(&self, name: &str) -> Option<&PredefinedItem> {
        self.predefined_items.iter().find(|pi| {
            stdx::case::eq_ignore_case(&pi.name, name)
                || pi.name_en.as_ref().is_some_and(|en| stdx::case::eq_ignore_case(en, name))
        })
    }

    /// Heap bytes owned by this object, memoised by `ide-db`'s `parse_mdo_query`
    /// for Salsa's `heap_size` hook: its own name strings plus every owned
    /// collection, recursing into nested attributes, tabular sections, and
    /// children. New heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        let mut bytes = self.name.capacity();
        bytes += self.name_en.as_ref().map_or(0, String::capacity);
        bytes += stdx::heap::vec_bytes::<Attribute>(self.attributes.len())
            + self.attributes.iter().map(Attribute::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<crate::tabular_section::TabularSection>(
            self.tabular_sections.len(),
        ) + self
            .tabular_sections
            .iter()
            .map(crate::tabular_section::TabularSection::estimated_heap_size)
            .sum::<usize>();
        bytes += stdx::heap::vec_bytes::<MetadataObject>(self.children.len())
            + self.children.iter().map(MetadataObject::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<EnumValue>(self.enum_values.len())
            + self.enum_values.iter().map(EnumValue::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<PredefinedItem>(self.predefined_items.len())
            + self.predefined_items.iter().map(PredefinedItem::estimated_heap_size).sum::<usize>();
        bytes += self.constant_type.as_ref().map_or(0, AttributeType::estimated_heap_size);
        bytes += stdx::heap::vec_bytes::<(MdoType, Name)>(self.register_records.len())
            + self.register_records.iter().map(|(_, name)| name.capacity()).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<crate::common_attribute::CommonAttributeField>(
            self.common_attributes.len(),
        ) + self
            .common_attributes
            .iter()
            .map(crate::common_attribute::CommonAttributeField::estimated_heap_size)
            .sum::<usize>();
        bytes
    }
}

impl std::fmt::Display for AttributeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String { length } => {
                write!(f, "Строка")?;
                if let Some(len) = length {
                    write!(f, "({})", len)?;
                }
                Ok(())
            }
            Self::Number { precision, scale } => {
                write!(f, "Число({}, {})", precision, scale)
            }
            Self::Boolean => write!(f, "Булево"),
            Self::Date => write!(f, "Дата"),
            Self::DateTime => write!(f, "ДатаВремя"),
            Self::Ref { mdo_type, name } => {
                write!(f, "{}.{}", mdo_type.russian_name(), name)
            }
            Self::InformationRegisterRecordManager { name } => {
                write!(f, "РегистрСведенийМенеджерЗаписи.{}", name)
            }
            Self::AnyRef => write!(f, "ЛюбаяСсылка"),
            Self::AnyObjectRef { mdo_type } => {
                write!(f, "{}", mdo_type.russian_name())
            }
            Self::Uuid => write!(f, "УникальныйИдентификатор"),
            Self::ValueStorage => write!(f, "ХранилищеЗначения"),
            Self::DefinedType { name } => {
                write!(f, "ОпределяемыйТип.{}", name)
            }
            Self::Composite { types } => {
                if types.is_empty() {
                    write!(f, "Составной тип (пусто)")
                } else if types.len() == 1 {
                    write!(f, "{}", types[0])
                } else {
                    write!(f, "Составной тип:")
                }
            }
            Self::Platform(pvt) => write!(f, "{}", pvt.russian_name()),
            Self::PlatformNamed(name) => write!(f, "{}", name),
            Self::UnknownNamed(name) => write!(f, "{}", name),
            Self::Unknown => write!(f, "Неизвестно"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plural_covers_exactly_the_manager_backed_kinds() {
        for &mdo in MdoType::all() {
            assert_eq!(
                mdo.russian_plural().is_some(),
                mdo.manager_type_prefix().is_some(),
                "{mdo:?}: a collection name exists iff a manager module does"
            );
        }
    }

    #[test]
    fn russian_plural_round_trips_through_from_plural() {
        for &mdo in MdoType::all() {
            let Some(plural) = mdo.russian_plural() else {
                continue;
            };
            assert_eq!(
                MdoType::from_plural(plural),
                Some(mdo),
                "{mdo:?}: '{plural}' must resolve back to the same kind"
            );
        }
    }

    #[test]
    fn russian_plural_is_spelled_in_russian() {
        // The round-trip alone does not pin the language: `from_plural` accepts the
        // English spellings too, so "Constants" would satisfy it while breaking the
        // canonicalisation every diagnostic message relies on.
        let expected = [
            (MdoType::Catalog, "Справочники"),
            (MdoType::Document, "Документы"),
            (MdoType::InformationRegister, "РегистрыСведений"),
            (MdoType::AccumulationRegister, "РегистрыНакопления"),
            (MdoType::AccountingRegister, "РегистрыБухгалтерии"),
            (MdoType::CalculationRegister, "РегистрыРасчета"),
            (MdoType::ChartOfCharacteristicTypes, "ПланыВидовХарактеристик"),
            (MdoType::ChartOfAccounts, "ПланыСчетов"),
            (MdoType::ChartOfCalculationTypes, "ПланыВидовРасчета"),
            (MdoType::BusinessProcess, "БизнесПроцессы"),
            (MdoType::Task, "Задачи"),
            (MdoType::Enum, "Перечисления"),
            (MdoType::ExchangePlan, "ПланыОбмена"),
            (MdoType::ExternalDataSource, "ВнешниеИсточникиДанных"),
            (MdoType::Constant, "Константы"),
            (MdoType::DataProcessor, "Обработки"),
            (MdoType::Report, "Отчеты"),
        ];
        for (mdo, plural) in expected {
            assert_eq!(mdo.russian_plural(), Some(plural), "{mdo:?}");
        }
        assert_eq!(
            expected.len(),
            MdoType::all().iter().filter(|mdo| mdo.manager_type_prefix().is_some()).count(),
            "every manager-backed kind must be pinned by name here"
        );
    }

    #[test]
    fn is_standard_attribute_name_matches_every_kind() {
        // Both name forms of every kind must be recognised, and a clearly
        // non-standard name must not be.
        for kind in StandardAttributeKind::ALL {
            assert!(
                is_standard_attribute_name(kind.russian_name()),
                "{} (ru) must be standard",
                kind.russian_name()
            );
            assert!(
                is_standard_attribute_name(kind.english_name()),
                "{} (en) must be standard",
                kind.english_name()
            );
        }
        assert!(is_standard_attribute_name("ссылка"), "case-insensitive");
        assert!(!is_standard_attribute_name("ИНН"));
    }

    #[test]
    fn test_mdo_type_from_str_russian() {
        assert_eq!("Справочник".parse::<MdoType>().ok(), Some(MdoType::Catalog));
        assert_eq!("РегистрСведений".parse::<MdoType>().ok(), Some(MdoType::InformationRegister));
        assert_eq!("Документ".parse::<MdoType>().ok(), Some(MdoType::Document));
    }

    #[test]
    fn test_mdo_type_from_str_english() {
        assert_eq!("Catalog".parse::<MdoType>().ok(), Some(MdoType::Catalog));
        assert_eq!(
            "InformationRegister".parse::<MdoType>().ok(),
            Some(MdoType::InformationRegister)
        );
        assert_eq!("Document".parse::<MdoType>().ok(), Some(MdoType::Document));
    }

    #[test]
    fn test_mdo_type_case_insensitive() {
        assert_eq!("справочник".parse::<MdoType>().ok(), Some(MdoType::Catalog));
        assert_eq!("CATALOG".parse::<MdoType>().ok(), Some(MdoType::Catalog));
        assert_eq!("CaTaLoG".parse::<MdoType>().ok(), Some(MdoType::Catalog));
    }

    #[test]
    fn test_mdo_type_names() {
        assert_eq!(MdoType::Catalog.russian_name(), "Справочник");
        assert_eq!(MdoType::Catalog.english_name(), "Catalog");
    }

    #[test]
    fn test_event_subscription_parses_and_names_but_is_typeless() {
        assert_eq!("ПодпискаНаСобытие".parse::<MdoType>().ok(), Some(MdoType::EventSubscription));
        assert_eq!("EventSubscription".parse::<MdoType>().ok(), Some(MdoType::EventSubscription));
        assert_eq!(MdoType::EventSubscription.russian_name(), "ПодпискаНаСобытие");
        assert_eq!(MdoType::EventSubscription.english_name(), "EventSubscription");
        // Not a data-bearing object: no manager and no ref/object MetadataKind.
        assert_eq!(MdoType::EventSubscription.manager_type_prefix(), None);
        // Excluded from the enumerated set so it never surfaces in MDO completion.
        assert!(!MdoType::all().contains(&MdoType::EventSubscription));
    }

    #[test]
    fn test_mdo_type_from_plural() {
        assert_eq!(MdoType::from_plural("Документы"), Some(MdoType::Document));
        assert_eq!(MdoType::from_plural("Справочники"), Some(MdoType::Catalog));
        assert_eq!(MdoType::from_plural("РегистрыСведений"), Some(MdoType::InformationRegister));
        assert_eq!(MdoType::from_plural("РегистрыНакопления"), Some(MdoType::AccumulationRegister));
        assert_eq!(MdoType::from_plural("ПланыСчетов"), Some(MdoType::ChartOfAccounts));

        assert_eq!(MdoType::from_plural("documents"), Some(MdoType::Document));
        assert_eq!(MdoType::from_plural("catalogs"), Some(MdoType::Catalog));
        assert_eq!(
            MdoType::from_plural("InformationRegisters"),
            Some(MdoType::InformationRegister)
        );

        assert_eq!(MdoType::from_plural("ДОКУМЕНТЫ"), Some(MdoType::Document));
        assert_eq!(MdoType::from_plural("CaTaLoGs"), Some(MdoType::Catalog));

        assert_eq!(MdoType::from_plural("InvalidType"), None);
        assert_eq!(MdoType::from_plural(""), None);
    }

    #[test]
    fn test_metadata_object_with_attributes() {
        let mut obj = MetadataObject::new(MdoType::Catalog, "Валюты");

        assert_eq!(obj.name, "Валюты");
        assert_eq!(obj.name_en, None);
        assert!(obj.attributes.is_empty());

        obj.add_attribute(Attribute {
            name: "Код".to_string(),
            name_en: Some("Code".to_string()),
            attr_type: AttributeType::String { length: Some(10) },
        });

        obj.add_attribute(Attribute {
            name: "Курс".to_string(),
            name_en: Some("Rate".to_string()),
            attr_type: AttributeType::Number { precision: 15, scale: 4 },
        });

        assert_eq!(obj.attributes.len(), 2);

        let code_attr = obj.find_attribute("Код").unwrap();
        assert_eq!(code_attr.name, "Код");
        assert_eq!(code_attr.name_en, Some("Code".to_string()));
        assert_eq!(code_attr.attr_type, AttributeType::String { length: Some(10) });

        let rate_attr = obj.find_attribute("курс").unwrap();
        assert_eq!(rate_attr.name, "Курс");

        assert!(obj.find_attribute("НесуществующееПоле").is_none());
    }

    #[test]
    fn test_metadata_object_with_details() {
        let attributes = vec![
            Attribute {
                name: "Активен".to_string(),
                name_en: Some("Active".to_string()),
                attr_type: AttributeType::Boolean,
            },
            Attribute {
                name: "Дата".to_string(),
                name_en: Some("Date".to_string()),
                attr_type: AttributeType::Date,
            },
        ];

        let obj = MetadataObject::with_details(
            MdoType::Document,
            "ПриходнаяНакладная",
            Some("GoodsReceipt".to_string()),
            attributes,
        );

        assert_eq!(obj.name, "ПриходнаяНакладная");
        assert_eq!(obj.name_en, Some("GoodsReceipt".to_string()));
        assert_eq!(obj.attributes.len(), 2);
    }

    #[test]
    fn test_attribute_types() {
        let attr_string = AttributeType::String { length: Some(100) };
        assert_eq!(attr_string, AttributeType::String { length: Some(100) });

        let attr_number = AttributeType::Number { precision: 10, scale: 2 };
        assert_eq!(attr_number, AttributeType::Number { precision: 10, scale: 2 });

        let attr_ref =
            AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".to_string() };
        assert_eq!(
            attr_ref,
            AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Валюты".to_string() }
        );

        let attr_boolean = AttributeType::Boolean;
        assert_eq!(attr_boolean, AttributeType::Boolean);

        let attr_date = AttributeType::Date;
        assert_eq!(attr_date, AttributeType::Date);

        let attr_datetime = AttributeType::DateTime;
        assert_eq!(attr_datetime, AttributeType::DateTime);

        let attr_unknown = AttributeType::Unknown;
        assert_eq!(attr_unknown, AttributeType::Unknown);
    }

    #[test]
    fn test_composite_type_display() {
        let single = AttributeType::Composite {
            types: vec![AttributeType::Ref {
                mdo_type: MdoType::Enum,
                name: "ВидДействия".to_string(),
            }],
        };
        assert_eq!(single.to_string(), "Перечисление.ВидДействия");

        let composite = AttributeType::Composite {
            types: vec![
                AttributeType::Ref {
                    mdo_type: MdoType::Enum,
                    name: "ВидыУсловийОтбораИсходящихПисем".to_string(),
                },
                AttributeType::Ref {
                    mdo_type: MdoType::Enum,
                    name: "ВидыДействийПриОбработкеИсходящихПисем".to_string(),
                },
                AttributeType::Ref {
                    mdo_type: MdoType::Enum,
                    name: "ВидыУсловийОтбораВходящихПисем".to_string(),
                },
                AttributeType::Ref {
                    mdo_type: MdoType::Enum,
                    name: "ВидыДействийПриОбработкеВходящихПисем".to_string(),
                },
            ],
        };

        let display = composite.to_string();
        assert_eq!(display, "Составной тип:");
    }

    #[test]
    fn test_any_object_ref_display() {
        let catalog_ref = AttributeType::AnyObjectRef { mdo_type: MdoType::Catalog };
        assert_eq!(catalog_ref.to_string(), "Справочник");

        let document_ref = AttributeType::AnyObjectRef { mdo_type: MdoType::Document };
        assert_eq!(document_ref.to_string(), "Документ");

        let bp_ref = AttributeType::AnyObjectRef { mdo_type: MdoType::BusinessProcess };
        assert_eq!(bp_ref.to_string(), "БизнесПроцесс");

        let enum_ref = AttributeType::AnyObjectRef { mdo_type: MdoType::Enum };
        assert_eq!(enum_ref.to_string(), "Перечисление");
    }

    #[test]
    fn metadata_object_heap_counts_attributes_and_children() {
        let mut obj = MetadataObject::new(MdoType::Catalog, "Номенклатура");
        let attr = Attribute {
            name: "Артикул".to_string(),
            name_en: Some("Code".to_string()),
            attr_type: AttributeType::String { length: Some(25) },
        };
        let owned_strings =
            attr.name.capacity() + attr.name_en.as_ref().unwrap().capacity() + obj.name.capacity();
        obj.add_attribute(attr);
        obj.add_child(MetadataObject::new(MdoType::Catalog, "ВложеннаяГруппа"));

        let bytes = obj.estimated_heap_size();
        // At least the owned name/attribute strings plus the child's own name;
        // well under a kilobyte for two small entries.
        assert!(bytes > owned_strings);
        assert!(bytes < 1024);
    }

    #[test]
    fn test_is_plural_form() {
        assert!(MdoType::is_plural_form("Документы"));
        assert!(MdoType::is_plural_form("Справочники"));
        assert!(MdoType::is_plural_form("РегистрыСведений"));

        assert!(MdoType::is_plural_form("Documents"));
        assert!(MdoType::is_plural_form("Catalogs"));
        assert!(MdoType::is_plural_form("InformationRegisters"));

        assert!(MdoType::is_plural_form("ДОКУМЕНТЫ"));
        assert!(MdoType::is_plural_form("документы"));
        assert!(MdoType::is_plural_form("ДоКуМеНтЫ"));
        assert!(MdoType::is_plural_form("CATALOGS"));
        assert!(MdoType::is_plural_form("catalogs"));

        assert!(!MdoType::is_plural_form("Документ"));
        assert!(!MdoType::is_plural_form("Document"));
        assert!(!MdoType::is_plural_form("Справочник"));

        assert!(!MdoType::is_plural_form(""));
        assert!(!MdoType::is_plural_form("InvalidType"));
    }
}
