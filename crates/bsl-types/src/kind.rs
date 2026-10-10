use std::sync::Arc;

use bsl_metadata::{MdoType, Name};

use crate::facet::{
    ArrayFacet, CommonModuleFacet, DateFacet, FormBindingFacet, FormDataFacet, FormElementFacet,
    FunctionFacet, ManagerFacet, MapFacet, MdoRefFacet, MetaObjFacet, MetaRefFacet, NumberFacet,
    PlatformObjectFacet, ProjectionFacet, StringFacet, StructureFacet, TableFacet,
};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct TypeId(pub(crate) u64);

impl TypeId {
    pub fn raw(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

pub use bsl_config::ConfigId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetadataKind {
    CatalogRef,
    DocumentRef,
    DocumentObject,
    CatalogObject,
    InformationRegisterRecordManager,
    InformationRegisterRecordSet,
    AccumulationRegisterRecordSet,
    AccountingRegisterRecordSet,
    CalculationRegisterRecordSet,
    InformationRegisterRecord,
    AccumulationRegisterRecord,
    AccountingRegisterRecord,
    CalculationRegisterRecord,
    EnumRef,
    TaskRef,
    TaskObject,
    BusinessProcessRef,
    BusinessProcessObject,
    DataProcessorObject,
    ReportObject,
    /// `ВнешняяОбработкаОбъект.<Имя>`: the object of an EPF export. Its members
    /// come from the export's own metadata; its platform methods and properties
    /// from the bare platform type `ВнешняяОбработка`, not from a manager prefix.
    ExternalDataProcessorObject,
    /// `ВнешнийОтчетОбъект.<Имя>`: the ERF counterpart.
    ExternalReportObject,
    ExchangePlanRef,
    ExchangePlanObject,
    ChartOfAccountsRef,
    ChartOfAccountsObject,
    ChartOfCharacteristicTypesRef,
    ChartOfCharacteristicTypesObject,
    ChartOfCalculationTypesRef,
    ChartOfCalculationTypesObject,
    InformationRegisterRef,
    AccumulationRegisterRef,
    AccountingRegisterRef,
    CalculationRegisterRef,
    RegisterDimension {
        parent: MdoType,
    },
    RegisterResource {
        parent: MdoType,
    },
    RegisterAttribute {
        parent: MdoType,
    },
    RegisterFilter {
        parent: MdoType,
    },
    TabularSection {
        parent: MdoType,
    },
    TabularSectionRow {
        parent: MdoType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetadataReferenceKind {
    Role,
    EventSubscription,
    ScheduledJob,
    HttpService,
    WebService,
    Subsystem,
}

impl MetadataReferenceKind {
    pub const ALL: &'static [Self] = &[
        Self::Role,
        Self::EventSubscription,
        Self::ScheduledJob,
        Self::HttpService,
        Self::WebService,
        Self::Subsystem,
    ];

    pub fn from_plural(s: &str) -> Option<Self> {
        let lower = s.to_lowercase().replace('ё', "е");
        match lower.as_str() {
            "роли" | "roles" => Some(Self::Role),
            "подпискинасобытия" | "eventsubscriptions" => {
                Some(Self::EventSubscription)
            }
            "регламентныезадания" | "scheduledjobs" => Some(Self::ScheduledJob),
            "httpсервисы" | "httpservices" => Some(Self::HttpService),
            "webсервисы" | "webservices" => Some(Self::WebService),
            "подсистемы" | "subsystems" => Some(Self::Subsystem),
            _ => None,
        }
    }

    pub const fn russian_singular(self) -> &'static str {
        match self {
            Self::Role => "Роль",
            Self::EventSubscription => "ПодпискаНаСобытие",
            Self::ScheduledJob => "РегламентноеЗадание",
            Self::HttpService => "HTTPСервис",
            Self::WebService => "WebСервис",
            Self::Subsystem => "Подсистема",
        }
    }

    pub const fn english_singular(self) -> &'static str {
        match self {
            Self::Role => "Role",
            Self::EventSubscription => "EventSubscription",
            Self::ScheduledJob => "ScheduledJob",
            Self::HttpService => "HTTPService",
            Self::WebService => "WebService",
            Self::Subsystem => "Subsystem",
        }
    }

    pub const fn russian_plural(self) -> &'static str {
        match self {
            Self::Role => "Роли",
            Self::EventSubscription => "ПодпискиНаСобытия",
            Self::ScheduledJob => "РегламентныеЗадания",
            Self::HttpService => "HTTPСервисы",
            Self::WebService => "WebСервисы",
            Self::Subsystem => "Подсистемы",
        }
    }

    pub const fn english_plural(self) -> &'static str {
        match self {
            Self::Role => "Roles",
            Self::EventSubscription => "EventSubscriptions",
            Self::ScheduledJob => "ScheduledJobs",
            Self::HttpService => "HTTPServices",
            Self::WebService => "WebServices",
            Self::Subsystem => "Subsystems",
        }
    }
}

impl MetadataKind {
    pub fn object_kind_for(mdo_type: MdoType) -> Option<Self> {
        match mdo_type {
            MdoType::Catalog => Some(MetadataKind::CatalogObject),
            MdoType::Document => Some(MetadataKind::DocumentObject),
            MdoType::ExchangePlan => Some(MetadataKind::ExchangePlanObject),
            MdoType::ChartOfAccounts => Some(MetadataKind::ChartOfAccountsObject),
            MdoType::Task => Some(MetadataKind::TaskObject),
            MdoType::BusinessProcess => Some(MetadataKind::BusinessProcessObject),
            MdoType::DataProcessor => Some(MetadataKind::DataProcessorObject),
            MdoType::Report => Some(MetadataKind::ReportObject),
            MdoType::ExternalDataProcessor => Some(MetadataKind::ExternalDataProcessorObject),
            MdoType::ExternalReport => Some(MetadataKind::ExternalReportObject),
            MdoType::ChartOfCharacteristicTypes => {
                Some(MetadataKind::ChartOfCharacteristicTypesObject)
            }
            MdoType::ChartOfCalculationTypes => Some(MetadataKind::ChartOfCalculationTypesObject),
            _ => None,
        }
    }

    pub fn ref_kind_for(mdo_type: MdoType) -> Option<Self> {
        match mdo_type {
            MdoType::Catalog => Some(MetadataKind::CatalogRef),
            MdoType::Document => Some(MetadataKind::DocumentRef),
            MdoType::Enum => Some(MetadataKind::EnumRef),
            MdoType::Task => Some(MetadataKind::TaskRef),
            MdoType::BusinessProcess => Some(MetadataKind::BusinessProcessRef),
            MdoType::ExchangePlan => Some(MetadataKind::ExchangePlanRef),
            MdoType::ChartOfAccounts => Some(MetadataKind::ChartOfAccountsRef),
            MdoType::ChartOfCharacteristicTypes => {
                Some(MetadataKind::ChartOfCharacteristicTypesRef)
            }
            MdoType::ChartOfCalculationTypes => Some(MetadataKind::ChartOfCalculationTypesRef),
            MdoType::InformationRegister => Some(MetadataKind::InformationRegisterRef),
            MdoType::AccumulationRegister => Some(MetadataKind::AccumulationRegisterRef),
            MdoType::AccountingRegister => Some(MetadataKind::AccountingRegisterRef),
            MdoType::CalculationRegister => Some(MetadataKind::CalculationRegisterRef),
            _ => None,
        }
    }

    pub fn record_set_kind_for(mdo_type: MdoType) -> Option<Self> {
        match mdo_type {
            MdoType::InformationRegister => Some(MetadataKind::InformationRegisterRecordSet),
            MdoType::AccumulationRegister => Some(MetadataKind::AccumulationRegisterRecordSet),
            MdoType::AccountingRegister => Some(MetadataKind::AccountingRegisterRecordSet),
            MdoType::CalculationRegister => Some(MetadataKind::CalculationRegisterRecordSet),
            _ => None,
        }
    }

    pub fn ref_mdo_type(self) -> Option<MdoType> {
        match self {
            Self::CatalogRef => Some(MdoType::Catalog),
            Self::DocumentRef => Some(MdoType::Document),
            Self::EnumRef => Some(MdoType::Enum),
            Self::TaskRef => Some(MdoType::Task),
            Self::BusinessProcessRef => Some(MdoType::BusinessProcess),
            Self::ExchangePlanRef => Some(MdoType::ExchangePlan),
            Self::ChartOfAccountsRef => Some(MdoType::ChartOfAccounts),
            Self::ChartOfCharacteristicTypesRef => Some(MdoType::ChartOfCharacteristicTypes),
            Self::ChartOfCalculationTypesRef => Some(MdoType::ChartOfCalculationTypes),
            Self::InformationRegisterRef => Some(MdoType::InformationRegister),
            Self::AccumulationRegisterRef => Some(MdoType::AccumulationRegister),
            Self::AccountingRegisterRef => Some(MdoType::AccountingRegister),
            Self::CalculationRegisterRef => Some(MdoType::CalculationRegister),
            _ => None,
        }
    }

    pub fn platform_prefix(self) -> Option<&'static str> {
        match self {
            Self::CatalogObject => Some("CatalogObject"),
            Self::CatalogRef => Some("CatalogRef"),
            Self::DocumentObject => Some("DocumentObject"),
            Self::DocumentRef => Some("DocumentRef"),
            Self::EnumRef => Some("EnumRef"),
            Self::TaskRef => Some("TaskRef"),
            Self::TaskObject => Some("TaskObject"),
            Self::BusinessProcessRef => Some("BusinessProcessRef"),
            Self::BusinessProcessObject => Some("BusinessProcessObject"),
            Self::DataProcessorObject => Some("DataProcessorObject"),
            Self::ReportObject => Some("ReportObject"),
            Self::ExchangePlanRef => Some("ExchangePlanRef"),
            Self::ExchangePlanObject => Some("ExchangePlanObject"),
            Self::ChartOfAccountsRef => Some("ChartOfAccountsRef"),
            Self::ChartOfAccountsObject => Some("ChartOfAccountsObject"),
            Self::ChartOfCharacteristicTypesRef => Some("ChartOfCharacteristicTypesRef"),
            Self::ChartOfCharacteristicTypesObject => Some("ChartOfCharacteristicTypesObject"),
            Self::ChartOfCalculationTypesRef => Some("ChartOfCalculationTypesRef"),
            Self::ChartOfCalculationTypesObject => Some("ChartOfCalculationTypesObject"),
            Self::InformationRegisterRecordManager => Some("InformationRegisterRecordManager"),
            Self::InformationRegisterRecordSet => Some("InformationRegisterRecordSet"),
            Self::AccumulationRegisterRecordSet => Some("AccumulationRegisterRecordSet"),
            Self::AccountingRegisterRecordSet => Some("AccountingRegisterRecordSet"),
            Self::CalculationRegisterRecordSet => Some("CalculationRegisterRecordSet"),
            Self::InformationRegisterRecord => Some("InformationRegisterRecord"),
            Self::AccumulationRegisterRecord => Some("AccumulationRegisterRecord"),
            Self::AccountingRegisterRecord => Some("AccountingRegisterRecord"),
            Self::CalculationRegisterRecord => Some("CalculationRegisterRecord"),
            // The platform corpus keys an external object's members under the bare
            // type, not under a dotted `<Kind>.<Name>` prefix: see `bare_platform_type`.
            Self::ExternalDataProcessorObject
            | Self::ExternalReportObject
            | Self::InformationRegisterRef
            | Self::AccumulationRegisterRef
            | Self::AccountingRegisterRef
            | Self::CalculationRegisterRef
            | Self::RegisterDimension { .. }
            | Self::RegisterResource { .. }
            | Self::RegisterAttribute { .. }
            | Self::RegisterFilter { .. }
            | Self::TabularSection { .. }
            | Self::TabularSectionRow { .. } => None,
        }
    }

    /// The platform type whose methods and properties this kind carries when it
    /// has no manager prefix. An external object's corpus entries have a bare
    /// `type_name` (`ExternalDataProcessor`), so the prefix index never lists them.
    pub fn bare_platform_type(self) -> Option<&'static str> {
        self.bare_platform_type_names().map(|(_, en)| en)
    }

    /// Both spellings of [`Self::bare_platform_type`]: what the corpus writes as
    /// the type of that type's own self-returning members (`ЭтотОбъект`).
    pub fn bare_platform_type_names(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::ExternalDataProcessorObject => {
                Some(("ВнешняяОбработка", "ExternalDataProcessor"))
            }
            Self::ExternalReportObject => Some(("ВнешнийОтчет", "ExternalReport")),
            _ => None,
        }
    }

    pub fn scalar_platform_key(self) -> Option<&'static str> {
        match self {
            Self::RegisterFilter { .. } => Some("Filter"),
            _ => None,
        }
    }

    pub fn display_label(self, locale: impl std::fmt::Debug) -> &'static str {
        let is_en = format!("{locale:?}") == "En";
        match (self, is_en) {
            (Self::CatalogRef, false) => "СправочникСсылка",
            (Self::CatalogRef, true) => "CatalogRef",
            (Self::CatalogObject, false) => "СправочникОбъект",
            (Self::CatalogObject, true) => "CatalogObject",
            (Self::DocumentRef, false) => "ДокументСсылка",
            (Self::DocumentRef, true) => "DocumentRef",
            (Self::DocumentObject, false) => "ДокументОбъект",
            (Self::DocumentObject, true) => "DocumentObject",
            (Self::EnumRef, false) => "ПеречислениеСсылка",
            (Self::EnumRef, true) => "EnumRef",
            (Self::TaskRef, false) => "ЗадачаСсылка",
            (Self::TaskRef, true) => "TaskRef",
            (Self::TaskObject, false) => "ЗадачаОбъект",
            (Self::TaskObject, true) => "TaskObject",
            (Self::BusinessProcessRef, false) => "БизнесПроцессСсылка",
            (Self::BusinessProcessRef, true) => "BusinessProcessRef",
            (Self::BusinessProcessObject, false) => "БизнесПроцессОбъект",
            (Self::BusinessProcessObject, true) => "BusinessProcessObject",
            (Self::DataProcessorObject, false) => "ОбработкаОбъект",
            (Self::DataProcessorObject, true) => "DataProcessorObject",
            (Self::ReportObject, false) => "ОтчётОбъект",
            (Self::ReportObject, true) => "ReportObject",
            (Self::ExternalDataProcessorObject, false) => "ВнешняяОбработкаОбъект",
            (Self::ExternalDataProcessorObject, true) => "ExternalDataProcessorObject",
            (Self::ExternalReportObject, false) => "ВнешнийОтчётОбъект",
            (Self::ExternalReportObject, true) => "ExternalReportObject",
            (Self::ExchangePlanRef, false) => "ПланОбменаСсылка",
            (Self::ExchangePlanRef, true) => "ExchangePlanRef",
            (Self::ExchangePlanObject, false) => "ПланОбменаОбъект",
            (Self::ExchangePlanObject, true) => "ExchangePlanObject",
            (Self::ChartOfAccountsRef, false) => "ПланСчетовСсылка",
            (Self::ChartOfAccountsRef, true) => "ChartOfAccountsRef",
            (Self::ChartOfAccountsObject, false) => "ПланСчетовОбъект",
            (Self::ChartOfAccountsObject, true) => "ChartOfAccountsObject",
            (Self::ChartOfCharacteristicTypesRef, false) => "ПланВидовХарактеристикСсылка",
            (Self::ChartOfCharacteristicTypesRef, true) => "ChartOfCharacteristicTypesRef",
            (Self::ChartOfCharacteristicTypesObject, false) => "ПланВидовХарактеристикОбъект",
            (Self::ChartOfCharacteristicTypesObject, true) => "ChartOfCharacteristicTypesObject",
            (Self::ChartOfCalculationTypesRef, false) => "ПланВидовРасчетаСсылка",
            (Self::ChartOfCalculationTypesRef, true) => "ChartOfCalculationTypesRef",
            (Self::ChartOfCalculationTypesObject, false) => "ПланВидовРасчетаОбъект",
            (Self::ChartOfCalculationTypesObject, true) => "ChartOfCalculationTypesObject",
            (Self::InformationRegisterRef, false) => "РегистрСведенийКлючЗаписи",
            (Self::InformationRegisterRef, true) => "InformationRegisterRef",
            (Self::InformationRegisterRecordManager, false) => "РегистрСведенийМенеджерЗаписи",
            (Self::InformationRegisterRecordManager, true) => "InformationRegisterRecordManager",
            (Self::InformationRegisterRecordSet, false) => "РегистрСведенийНаборЗаписей",
            (Self::InformationRegisterRecordSet, true) => "InformationRegisterRecordSet",
            (Self::InformationRegisterRecord, false) => "РегистрСведенийЗапись",
            (Self::InformationRegisterRecord, true) => "InformationRegisterRecord",
            (Self::AccumulationRegisterRef, false) => "РегистрНакопленияКлючЗаписи",
            (Self::AccumulationRegisterRef, true) => "AccumulationRegisterRef",
            (Self::AccumulationRegisterRecordSet, false) => "РегистрНакопленияНаборЗаписей",
            (Self::AccumulationRegisterRecordSet, true) => "AccumulationRegisterRecordSet",
            (Self::AccumulationRegisterRecord, false) => "РегистрНакопленияЗапись",
            (Self::AccumulationRegisterRecord, true) => "AccumulationRegisterRecord",
            (Self::AccountingRegisterRef, false) => "РегистрБухгалтерииКлючЗаписи",
            (Self::AccountingRegisterRef, true) => "AccountingRegisterRef",
            (Self::AccountingRegisterRecordSet, false) => "РегистрБухгалтерииНаборЗаписей",
            (Self::AccountingRegisterRecordSet, true) => "AccountingRegisterRecordSet",
            (Self::AccountingRegisterRecord, false) => "РегистрБухгалтерииЗапись",
            (Self::AccountingRegisterRecord, true) => "AccountingRegisterRecord",
            (Self::CalculationRegisterRef, false) => "РегистрРасчётаКлючЗаписи",
            (Self::CalculationRegisterRef, true) => "CalculationRegisterRef",
            (Self::CalculationRegisterRecordSet, false) => "РегистрРасчетаНаборЗаписей",
            (Self::CalculationRegisterRecordSet, true) => "CalculationRegisterRecordSet",
            (Self::CalculationRegisterRecord, false) => "РегистрРасчетаЗапись",
            (Self::CalculationRegisterRecord, true) => "CalculationRegisterRecord",
            (Self::RegisterDimension { .. }, false) => "Измерение",
            (Self::RegisterDimension { .. }, true) => "Dimension",
            (Self::RegisterResource { .. }, false) => "Ресурс",
            (Self::RegisterResource { .. }, true) => "Resource",
            (Self::RegisterAttribute { .. }, false) => "Реквизит",
            (Self::RegisterAttribute { .. }, true) => "Attribute",
            (Self::RegisterFilter { .. }, false) => "Отбор",
            (Self::RegisterFilter { .. }, true) => "Filter",
            (Self::TabularSection { .. }, false) => "ТабличнаяЧасть",
            (Self::TabularSection { .. }, true) => "TabularSection",
            (Self::TabularSectionRow { .. }, false) => "СтрокаТабличнойЧасти",
            (Self::TabularSectionRow { .. }, true) => "TabularSectionRow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LiteralValue {
    Number(Box<str>),
    String(Box<str>),
    Boolean(bool),
    Undefined,
    Null,
    Date(Box<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExprRef(pub(crate) u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Projection {
    pub fields: Arc<[ProjectionField]>,
    pub origin: ProjectionOrigin,
    pub raw_sdbl_types: Option<Arc<[crate::facet::SdblTypeShadowFacet]>>,
}

impl Projection {
    pub fn new(
        fields: Arc<[ProjectionField]>,
        origin: ProjectionOrigin,
        raw_sdbl_types: Option<Arc<[crate::facet::SdblTypeShadowFacet]>>,
    ) -> Self {
        Self { fields, origin, raw_sdbl_types }
    }
}

impl ProjectionField {
    pub fn new(name: Name, ty: TypeId, source: ProjectionFieldSource) -> Self {
        Self { name, ty, source }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ProjectionField {
    pub name: Name,
    pub ty: TypeId,
    pub source: ProjectionFieldSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ProjectionOrigin {
    SdblQuery,
    FormAttribute,
    ValueTableLiteral,
    StructureLiteral,
    DocComment,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ProjectionFieldSource {
    Column,
    Cast,
    Aggregate,
    Derived,
    StructureLiteral,
    DocComment,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeOrigin {
    BslLiteral,
    SdblCast,
    DocComment,
    FormAttribute,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeKind {
    #[default]
    Unknown,
    Never,
    Any,

    Number(NumberFacet),
    String(StringFacet),
    Date(DateFacet),
    Boolean,
    Null,
    Undefined,
    Uuid,

    Array(ArrayFacet),
    Map(MapFacet),
    Structure(StructureFacet),
    ValueList(Option<TypeId>),
    ValueTable(TableFacet),
    ValueTableRow(TableFacet),

    MetadataRef(MetaRefFacet),
    AnyMetadataRef {
        mdo_type: MdoType,
    },
    MetadataReferenceCollection(MetadataReferenceKind),
    MetadataReference {
        kind: MetadataReferenceKind,
        name: Name,
    },
    /// A manager-backed collection of the configuration metadata root
    /// (`Метаданные.Справочники`): the platform's `КоллекцияОбъектовМетаданных`,
    /// remembering which kind its elements are. The bare platform type forgets it,
    /// and with it every check below `Метаданные.Справочники.X`.
    MetadataObjectCollection(MdoType),
    AnyRef,
    MetadataObject(MetaObjFacet),
    TabularSection {
        parent: MetaRefFacet,
        name: Name,
    },
    TabularSectionRow {
        parent: MetaRefFacet,
        name: Name,
    },

    RegisterDimension {
        parent: MetaRefFacet,
        name: Name,
    },
    RegisterResource {
        parent: MetaRefFacet,
        name: Name,
    },
    RegisterAttribute {
        parent: MetaRefFacet,
        name: Name,
    },
    RegisterFilter {
        parent: MetaRefFacet,
    },
    Attribute {
        parent: MetaRefFacet,
        name: Name,
    },

    FormData {
        kind: FormDataFacet,
        underlying: Option<MdoRefFacet>,
    },
    FormControl {
        kind: FormElementFacet,
        binding: Option<FormBindingFacet>,
    },
    ThisObject {
        config_id: ConfigId,
        owner: MdoRefFacet,
    },
    ThisManager {
        config_id: ConfigId,
        owner: MdoRefFacet,
    },

    CommonModule(CommonModuleFacet),

    PlatformObject(PlatformObjectFacet),
    ValueStorage,
    TypeDescriptor,

    Union(Arc<[TypeId]>),

    ManagerCollection(MdoType),
    ObjectManager(ManagerFacet),

    Function(FunctionFacet),

    QueryResult(ProjectionFacet),
    QueryResultSelection(ProjectionFacet),
    QueryBatchResult {
        per_query: Arc<[Option<Arc<Projection>>]>,
    },
    Query {
        projections: Arc<[Option<Arc<Projection>>]>,
    },
}
