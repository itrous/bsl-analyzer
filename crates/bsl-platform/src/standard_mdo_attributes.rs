#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MdoTemplateKind {
    Catalog,
    Document,
    BusinessProcess,
    Task,
    ChartOfAccounts,
    ChartOfCharacteristicTypes,
    ChartOfCalculationTypes,
    ExchangePlan,
    InformationRegister,
    AccumulationRegister,
    AccountingRegister,
    CalculationRegister,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectView {
    Object,
    Ref,
    RecordSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardKind {
    Code,
    Description,
    Ref,
    DeletionMark,
    IsFolder,
    Owner,
    Parent,
    Predefined,
    PredefinedDataName,
    Number,
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
    ActionPeriodIsBasic,
}

impl StandardKind {
    pub fn russian_name(self) -> &'static str {
        match self {
            Self::Code => "Код",
            Self::Description => "Наименование",
            Self::Ref => "Ссылка",
            Self::DeletionMark => "ПометкаУдаления",
            Self::IsFolder => "ЭтоГруппа",
            Self::Owner => "Владелец",
            Self::Parent => "Родитель",
            Self::Predefined => "Предопределенный",
            Self::PredefinedDataName => "ИмяПредопределенныхДанных",
            Self::Number => "Номер",
            Self::Date => "Дата",
            Self::Posted => "Проведен",
            Self::Started => "Стартован",
            Self::Completed => "Завершен",
            Self::HeadTask => "ВедущаяЗадача",
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
            Self::ActionPeriodIsBasic => "ПериодДействияБазовый",
        }
    }

    pub fn english_name(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Description => "Description",
            Self::Ref => "Ref",
            Self::DeletionMark => "DeletionMark",
            Self::IsFolder => "IsFolder",
            Self::Owner => "Owner",
            Self::Parent => "Parent",
            Self::Predefined => "Predefined",
            Self::PredefinedDataName => "PredefinedDataName",
            Self::Number => "Number",
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
            Self::ActionPeriodIsBasic => "ActionPeriodIsBasic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceCondition {
    Always,
    HasCode,
    HasDescription,
    HasNumber,
    Hierarchical,
    HasOwners,
    IsPeriodic,
    DependsOnCalculationTypes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrValueKind {
    Boolean,
    DateTime,
    StringCodeOrDescription,
    StringNumber,
    StringUnbounded,
    NumberLineNumber,
    SelfRef,
    OwnerRef,
    AnyDocumentRef,
    /// The `ОписаниеТипов` of a chart of characteristic types — its standard
    /// `ТипЗначения` attribute.
    TypeDescription,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct StandardAttrSpec {
    pub kind: StandardKind,
    pub value: AttrValueKind,
    pub condition: PresenceCondition,
    pub is_readonly: bool,
}

static CATALOG_BASE_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Code,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasCode,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Description,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasDescription,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::IsFolder,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Parent,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Owner,
        value: AttrValueKind::OwnerRef,
        condition: PresenceCondition::HasOwners,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Predefined,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::PredefinedDataName,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        // Writable on the object since 8.3.3: an element is bound to or released
        // from a predefined item by assigning it (an unknown name fails with
        // «Предопределенный элемент не существует», not as a read-only field).
        is_readonly: false,
    },
];

static CATALOG_OBJECT: &[StandardAttrSpec] = CATALOG_BASE_OBJECT;

static EXCHANGE_PLAN_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Code,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasCode,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Description,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasDescription,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::IsFolder,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Parent,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Owner,
        value: AttrValueKind::OwnerRef,
        condition: PresenceCondition::HasOwners,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Predefined,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::PredefinedDataName,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::ThisNode,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

static CHART_OF_CHARACTERISTIC_TYPES_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Code,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasCode,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Description,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasDescription,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::IsFolder,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Parent,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Hierarchical,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Owner,
        value: AttrValueKind::OwnerRef,
        condition: PresenceCondition::HasOwners,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Predefined,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::PredefinedDataName,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        // Writable on the object since 8.3.3: an element is bound to or released
        // from a predefined item by assigning it (an unknown name fails with
        // «Предопределенный элемент не существует», not as a read-only field).
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::ValueType,
        value: AttrValueKind::TypeDescription,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

// A chart of calculation types is flat (no hierarchy, no owners). Its
// `ПериодДействияБазовый` exists only when the chart depends on calculation types;
// the displacing/leading/base standard tabular sections, gated on the same
// dependency, are synthesized by the XML parser, not described here.
static CHART_OF_CALCULATION_TYPES_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Code,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasCode,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Description,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasDescription,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Predefined,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::PredefinedDataName,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::ActionPeriodIsBasic,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::DependsOnCalculationTypes,
        is_readonly: false,
    },
];

static CHART_OF_ACCOUNTS_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Code,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasCode,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Description,
        value: AttrValueKind::StringCodeOrDescription,
        condition: PresenceCondition::HasDescription,
        is_readonly: false,
    },
    // A chart of accounts is always hierarchical (accounts/sub-accounts) and its
    // XML carries no `Hierarchical` property, so `Родитель` is unconditional.
    // There is no `ЭтоГруппа` field at all: the hierarchy has no folder rows.
    StandardAttrSpec {
        kind: StandardKind::Parent,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Owner,
        value: AttrValueKind::OwnerRef,
        condition: PresenceCondition::HasOwners,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Predefined,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::PredefinedDataName,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Order,
        value: AttrValueKind::StringUnbounded,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

static DOCUMENT_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Number,
        value: AttrValueKind::StringNumber,
        condition: PresenceCondition::HasNumber,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Date,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Posted,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

static BUSINESS_PROCESS_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Number,
        value: AttrValueKind::StringNumber,
        condition: PresenceCondition::HasNumber,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Date,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Started,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Completed,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::HeadTask,
        value: AttrValueKind::Unknown,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

static TASK_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Ref,
        value: AttrValueKind::SelfRef,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::DeletionMark,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Number,
        value: AttrValueKind::StringNumber,
        condition: PresenceCondition::HasNumber,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Date,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Executed,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::TaskBusinessProcess,
        value: AttrValueKind::Unknown,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::RoutePoint,
        value: AttrValueKind::Unknown,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

static INFORMATION_REGISTER_OBJECT: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Active,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::LineNumber,
        value: AttrValueKind::NumberLineNumber,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::Recorder,
        value: AttrValueKind::AnyDocumentRef,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Period,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::IsPeriodic,
        is_readonly: false,
    },
];

static INFORMATION_REGISTER_RECORD_SET: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Active,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::LineNumber,
        value: AttrValueKind::NumberLineNumber,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::Recorder,
        value: AttrValueKind::AnyDocumentRef,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Period,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::IsPeriodic,
        is_readonly: false,
    },
];

static ACCUMULATION_REGISTER_RECORD_SET: &[StandardAttrSpec] = &[
    StandardAttrSpec {
        kind: StandardKind::Active,
        value: AttrValueKind::Boolean,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::LineNumber,
        value: AttrValueKind::NumberLineNumber,
        condition: PresenceCondition::Always,
        is_readonly: true,
    },
    StandardAttrSpec {
        kind: StandardKind::Recorder,
        value: AttrValueKind::AnyDocumentRef,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
    StandardAttrSpec {
        kind: StandardKind::Period,
        value: AttrValueKind::DateTime,
        condition: PresenceCondition::Always,
        is_readonly: false,
    },
];

pub fn standard_attributes_for(
    kind: MdoTemplateKind,
    view: ObjectView,
) -> &'static [StandardAttrSpec] {
    match (kind, view) {
        (MdoTemplateKind::Catalog, ObjectView::Object) => CATALOG_OBJECT,
        (MdoTemplateKind::ExchangePlan, ObjectView::Object) => EXCHANGE_PLAN_OBJECT,
        (MdoTemplateKind::ChartOfCharacteristicTypes, ObjectView::Object) => {
            CHART_OF_CHARACTERISTIC_TYPES_OBJECT
        }
        (MdoTemplateKind::ChartOfAccounts, ObjectView::Object) => CHART_OF_ACCOUNTS_OBJECT,
        (MdoTemplateKind::ChartOfCalculationTypes, ObjectView::Object) => {
            CHART_OF_CALCULATION_TYPES_OBJECT
        }
        (MdoTemplateKind::Document, ObjectView::Object) => DOCUMENT_OBJECT,
        (MdoTemplateKind::BusinessProcess, ObjectView::Object) => BUSINESS_PROCESS_OBJECT,
        (MdoTemplateKind::Task, ObjectView::Object) => TASK_OBJECT,
        (MdoTemplateKind::InformationRegister, ObjectView::Object) => INFORMATION_REGISTER_OBJECT,
        (MdoTemplateKind::InformationRegister, ObjectView::RecordSet) => {
            INFORMATION_REGISTER_RECORD_SET
        }
        (MdoTemplateKind::AccumulationRegister, ObjectView::RecordSet) => {
            ACCUMULATION_REGISTER_RECORD_SET
        }
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_name_and_english_name_match_help() {
        let all = [
            StandardKind::Code,
            StandardKind::Description,
            StandardKind::Ref,
            StandardKind::DeletionMark,
            StandardKind::IsFolder,
            StandardKind::Owner,
            StandardKind::Parent,
            StandardKind::Predefined,
            StandardKind::PredefinedDataName,
            StandardKind::Number,
            StandardKind::Date,
            StandardKind::Posted,
            StandardKind::Started,
            StandardKind::Completed,
            StandardKind::HeadTask,
            StandardKind::Executed,
            StandardKind::TaskBusinessProcess,
            StandardKind::RoutePoint,
            StandardKind::ThisNode,
            StandardKind::ValueType,
            StandardKind::Order,
            StandardKind::Active,
            StandardKind::LineNumber,
            StandardKind::Recorder,
            StandardKind::Period,
            StandardKind::ActionPeriodIsBasic,
        ];
        for kind in all {
            assert!(!kind.russian_name().is_empty(), "{kind:?} has empty russian_name");
            assert!(!kind.english_name().is_empty(), "{kind:?} has empty english_name");
        }
    }

    #[test]
    fn catalog_object_view_contains_ref_deletion_predefined() {
        let specs = standard_attributes_for(MdoTemplateKind::Catalog, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Ref));
        assert!(kinds.contains(&StandardKind::DeletionMark));
        assert!(kinds.contains(&StandardKind::Predefined));
        assert!(kinds.contains(&StandardKind::PredefinedDataName));
    }

    #[test]
    fn document_object_view_contains_ref_date_posted_number() {
        let specs = standard_attributes_for(MdoTemplateKind::Document, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Ref));
        assert!(kinds.contains(&StandardKind::Date));
        assert!(kinds.contains(&StandardKind::Posted));
        assert!(kinds.contains(&StandardKind::Number));
    }

    #[test]
    fn business_process_object_view_contains_started_completed_headtask() {
        let specs = standard_attributes_for(MdoTemplateKind::BusinessProcess, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Started));
        assert!(kinds.contains(&StandardKind::Completed));
        assert!(kinds.contains(&StandardKind::HeadTask));
    }

    #[test]
    fn task_object_view_contains_executed_taskbp_routepoint() {
        let specs = standard_attributes_for(MdoTemplateKind::Task, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Executed));
        assert!(kinds.contains(&StandardKind::TaskBusinessProcess));
        assert!(kinds.contains(&StandardKind::RoutePoint));
    }

    #[test]
    fn exchange_plan_object_view_contains_thisnode_after_catalog_set() {
        let specs = standard_attributes_for(MdoTemplateKind::ExchangePlan, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Ref));
        assert!(kinds.contains(&StandardKind::DeletionMark));
        assert!(kinds.contains(&StandardKind::ThisNode));
    }

    #[test]
    fn chart_of_characteristic_types_object_view_contains_value_type() {
        let specs = standard_attributes_for(
            MdoTemplateKind::ChartOfCharacteristicTypes,
            ObjectView::Object,
        );
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::ValueType));
        assert!(kinds.contains(&StandardKind::Ref));
    }

    #[test]
    fn chart_of_calculation_types_object_view_contains_action_period_and_no_hierarchy() {
        let specs =
            standard_attributes_for(MdoTemplateKind::ChartOfCalculationTypes, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Ref));
        assert!(kinds.contains(&StandardKind::Predefined));
        let basic = specs.iter().find(|s| s.kind == StandardKind::ActionPeriodIsBasic).unwrap();
        assert_eq!(basic.condition, PresenceCondition::DependsOnCalculationTypes);
        assert!(
            !kinds.contains(&StandardKind::IsFolder) && !kinds.contains(&StandardKind::Parent),
            "charts of calculation types are flat: {kinds:?}"
        );
    }

    #[test]
    fn chart_of_accounts_object_view_contains_order() {
        let specs = standard_attributes_for(MdoTemplateKind::ChartOfAccounts, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Order));
        assert!(kinds.contains(&StandardKind::Ref));
    }

    #[test]
    fn chart_of_accounts_parent_is_unconditional_and_no_folder_flag() {
        let specs = standard_attributes_for(MdoTemplateKind::ChartOfAccounts, ObjectView::Object);
        let parent = specs.iter().find(|s| s.kind == StandardKind::Parent).unwrap();
        assert_eq!(
            parent.condition,
            PresenceCondition::Always,
            "chart-of-accounts XML has no Hierarchical property; Родитель must not be gated on it"
        );
        assert!(
            !specs.iter().any(|s| s.kind == StandardKind::IsFolder),
            "chart-of-accounts tables have no ЭтоГруппа field"
        );
    }

    #[test]
    fn information_register_object_view_contains_active_line_number_recorder() {
        let specs =
            standard_attributes_for(MdoTemplateKind::InformationRegister, ObjectView::Object);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Active));
        assert!(kinds.contains(&StandardKind::LineNumber));
        assert!(kinds.contains(&StandardKind::Recorder));
        let period_spec = specs.iter().find(|s| s.kind == StandardKind::Period);
        assert!(
            period_spec.map(|s| s.condition) == Some(PresenceCondition::IsPeriodic),
            "Period must be IsPeriodic for InformationRegister"
        );
    }

    #[test]
    fn accumulation_register_object_view_contains_period_always() {
        let specs =
            standard_attributes_for(MdoTemplateKind::AccumulationRegister, ObjectView::RecordSet);
        let kinds: Vec<StandardKind> = specs.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StandardKind::Period));
        let period_spec = specs.iter().find(|s| s.kind == StandardKind::Period).unwrap();
        assert_eq!(period_spec.condition, PresenceCondition::Always);
    }
}
