use bsl_metadata::MdoType;
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;
use bsl_types::testing::RootConfigCtx;
use hir_def::ty::MetadataKind;
use hir_def::Name;

pub(crate) fn map_generic_metadata_return_type_typeid(
    db: &dyn TypeKernelDb,
    raw: &str,
    mdo_type: MdoType,
    mdo_name: &Name,
) -> Option<TypeId> {
    let kind = map_generic_metadata_return_type(raw, mdo_type)?;
    Some(db.metadata_ref(kind, mdo_name.as_str().to_string(), &RootConfigCtx))
}

pub(crate) fn metadata_kind_to_prefix_and_mdo(
    kind: MetadataKind,
) -> Option<(&'static str, MdoType)> {
    let prefix = kind.platform_prefix()?;
    let parent_mdo = match kind {
        MetadataKind::CatalogObject | MetadataKind::CatalogRef => MdoType::Catalog,
        MetadataKind::DocumentObject | MetadataKind::DocumentRef => MdoType::Document,
        MetadataKind::EnumRef => MdoType::Enum,
        MetadataKind::TaskRef | MetadataKind::TaskObject => MdoType::Task,
        MetadataKind::BusinessProcessRef | MetadataKind::BusinessProcessObject => {
            MdoType::BusinessProcess
        }
        MetadataKind::DataProcessorObject => MdoType::DataProcessor,
        MetadataKind::ReportObject => MdoType::Report,
        MetadataKind::ExchangePlanRef | MetadataKind::ExchangePlanObject => MdoType::ExchangePlan,
        MetadataKind::ChartOfAccountsRef | MetadataKind::ChartOfAccountsObject => {
            MdoType::ChartOfAccounts
        }
        MetadataKind::ChartOfCharacteristicTypesRef
        | MetadataKind::ChartOfCharacteristicTypesObject => MdoType::ChartOfCharacteristicTypes,
        MetadataKind::ChartOfCalculationTypesRef | MetadataKind::ChartOfCalculationTypesObject => {
            MdoType::ChartOfCalculationTypes
        }
        MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecordSet
        | MetadataKind::InformationRegisterRecord => MdoType::InformationRegister,
        MetadataKind::AccumulationRegisterRecordSet | MetadataKind::AccumulationRegisterRecord => {
            MdoType::AccumulationRegister
        }
        MetadataKind::AccountingRegisterRecordSet | MetadataKind::AccountingRegisterRecord => {
            MdoType::AccountingRegister
        }
        MetadataKind::CalculationRegisterRecordSet | MetadataKind::CalculationRegisterRecord => {
            MdoType::CalculationRegister
        }
        MetadataKind::ConstantValueManager => MdoType::Constant,
        // The external kinds have no prefix; `?` above already answered for them.
        MetadataKind::ExternalDataProcessorObject
        | MetadataKind::ExternalReportObject
        | MetadataKind::InformationRegisterRef
        | MetadataKind::AccumulationRegisterRef
        | MetadataKind::AccountingRegisterRef
        | MetadataKind::CalculationRegisterRef
        | MetadataKind::RegisterDimension { .. }
        | MetadataKind::RegisterResource { .. }
        | MetadataKind::RegisterAttribute { .. }
        | MetadataKind::RegisterFilter { .. }
        | MetadataKind::TabularSection { .. }
        | MetadataKind::TabularSectionRow { .. } => return None,
    };
    Some((prefix, parent_mdo))
}

pub(crate) fn map_generic_metadata_return_type(
    raw: &str,
    mdo_type: MdoType,
) -> Option<MetadataKind> {
    let kind = match (raw, mdo_type) {
        ("СправочникОбъект" | "CatalogObject", MdoType::Catalog) => {
            MetadataKind::CatalogObject
        }
        ("СправочникСсылка" | "CatalogRef", MdoType::Catalog) => {
            MetadataKind::CatalogRef
        }
        ("ДокументОбъект" | "DocumentObject", MdoType::Document) => {
            MetadataKind::DocumentObject
        }
        ("ДокументСсылка" | "DocumentRef", MdoType::Document) => {
            MetadataKind::DocumentRef
        }
        ("ПеречислениеСсылка" | "EnumRef", MdoType::Enum) => {
            MetadataKind::EnumRef
        }
        ("ЗадачаСсылка" | "TaskRef", MdoType::Task) => MetadataKind::TaskRef,
        ("БизнесПроцессСсылка" | "BusinessProcessRef", MdoType::BusinessProcess) => {
            MetadataKind::BusinessProcessRef
        }
        ("ПланОбменаСсылка" | "ExchangePlanRef", MdoType::ExchangePlan) => {
            MetadataKind::ExchangePlanRef
        }
        ("ПланОбменаОбъект" | "ExchangePlanObject", MdoType::ExchangePlan) => {
            MetadataKind::ExchangePlanObject
        }
        ("ПланСчетовСсылка" | "ChartOfAccountsRef", MdoType::ChartOfAccounts) => {
            MetadataKind::ChartOfAccountsRef
        }
        ("ПланСчетовОбъект" | "ChartOfAccountsObject", MdoType::ChartOfAccounts) => {
            MetadataKind::ChartOfAccountsObject
        }
        (
            "ПланВидовХарактеристикСсылка" | "ChartOfCharacteristicTypesRef",
            MdoType::ChartOfCharacteristicTypes,
        ) => MetadataKind::ChartOfCharacteristicTypesRef,
        (
            "ПланВидовХарактеристикОбъект" | "ChartOfCharacteristicTypesObject",
            MdoType::ChartOfCharacteristicTypes,
        ) => MetadataKind::ChartOfCharacteristicTypesObject,
        (
            "ПланВидовРасчетаСсылка" | "ChartOfCalculationTypesRef",
            MdoType::ChartOfCalculationTypes,
        ) => MetadataKind::ChartOfCalculationTypesRef,
        (
            "ПланВидовРасчетаОбъект" | "ChartOfCalculationTypesObject",
            MdoType::ChartOfCalculationTypes,
        ) => MetadataKind::ChartOfCalculationTypesObject,
        ("ЗадачаОбъект" | "TaskObject", MdoType::Task) => MetadataKind::TaskObject,
        ("БизнесПроцессОбъект" | "BusinessProcessObject", MdoType::BusinessProcess) => {
            MetadataKind::BusinessProcessObject
        }
        ("ОбработкаОбъект" | "DataProcessorObject", MdoType::DataProcessor) => {
            MetadataKind::DataProcessorObject
        }
        ("ОтчётОбъект" | "ОтчетОбъект" | "ReportObject", MdoType::Report) => {
            MetadataKind::ReportObject
        }
        (
            "РегистрСведенийМенеджерЗаписи" | "InformationRegisterRecordManager",
            MdoType::InformationRegister,
        ) => MetadataKind::InformationRegisterRecordManager,
        (
            "РегистрСведенийНаборЗаписей" | "InformationRegisterRecordSet",
            MdoType::InformationRegister,
        ) => MetadataKind::InformationRegisterRecordSet,
        (
            "РегистрНакопленияНаборЗаписей" | "AccumulationRegisterRecordSet",
            MdoType::AccumulationRegister,
        ) => MetadataKind::AccumulationRegisterRecordSet,
        (
            "РегистрБухгалтерииНаборЗаписей" | "AccountingRegisterRecordSet",
            MdoType::AccountingRegister,
        ) => MetadataKind::AccountingRegisterRecordSet,
        (
            "РегистрРасчетаНаборЗаписей" | "CalculationRegisterRecordSet",
            MdoType::CalculationRegister,
        ) => MetadataKind::CalculationRegisterRecordSet,
        ("РегистрСведенийЗапись" | "InformationRegisterRecord", MdoType::InformationRegister) => {
            MetadataKind::InformationRegisterRecord
        }
        (
            "РегистрНакопленияЗапись" | "AccumulationRegisterRecord",
            MdoType::AccumulationRegister,
        ) => MetadataKind::AccumulationRegisterRecord,
        ("РегистрБухгалтерииЗапись" | "AccountingRegisterRecord", MdoType::AccountingRegister) => {
            MetadataKind::AccountingRegisterRecord
        }
        ("РегистрРасчетаЗапись" | "CalculationRegisterRecord", MdoType::CalculationRegister) => {
            MetadataKind::CalculationRegisterRecord
        }
        // `Константы.<Имя>.СоздатьМенеджерЗначения()`.
        ("КонстантаМенеджерЗначения" | "ConstantValueManager", MdoType::Constant) => {
            MetadataKind::ConstantValueManager
        }
        _ => return None,
    };
    Some(kind)
}
