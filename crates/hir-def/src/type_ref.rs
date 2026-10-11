use bsl_metadata::{AttributeType, MdoType, PlatformValueType};
use stdx::case::CaseExt;

use crate::{path::QualifiedName, Name};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeRef {
    Builtin(BuiltinTypeRef),

    Name(QualifiedName),

    Array(Option<Box<TypeRef>>),

    Map(Option<(Box<TypeRef>, Box<TypeRef>)>),

    AnyRef,

    AnyRefOf(MdoType),

    Union(Vec<TypeRef>),

    /// An explicitly documented «Произвольный»: unconstrained by contract.
    /// Distinct from [`TypeRef::Unknown`] (no information): enrichment may
    /// replace an Unknown return with body inference, but a declared
    /// Произвольный must stay sticky.
    Any,

    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinTypeRef {
    Number,
    String,
    Boolean,
    Date,
    Undefined,
    Null,
    Structure,
    ValueTable,
    ValueList,
    Type,
}

impl BuiltinTypeRef {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.fold_lower().as_str() {
            "число" | "number" => Some(Self::Number),
            "строка" | "string" => Some(Self::String),
            "булево" | "boolean" => Some(Self::Boolean),
            "дата" | "date" => Some(Self::Date),
            "неопределено" | "undefined" => Some(Self::Undefined),
            "null" => Some(Self::Null),
            "структура" | "structure" => Some(Self::Structure),
            "таблицазначений" | "valuetable" => Some(Self::ValueTable),
            "списокзначений" | "valuelist" => Some(Self::ValueList),
            "тип" | "type" => Some(Self::Type),
            _ => None,
        }
    }
}

impl TypeRef {
    pub fn from_bare_name(name: &str) -> Option<Self> {
        match name.fold_lower().as_str() {
            "массив" | "array" => Some(TypeRef::Array(None)),
            "соответствие" | "map" => Some(TypeRef::Map(None)),
            "любаяссылка" | "anyref" => Some(TypeRef::AnyRef),
            other => BuiltinTypeRef::from_name(other).map(TypeRef::Builtin),
        }
    }

    pub fn from_attribute_type(attr: &AttributeType) -> Self {
        match attr {
            AttributeType::String { .. } => TypeRef::Builtin(BuiltinTypeRef::String),
            AttributeType::Number { .. } => TypeRef::Builtin(BuiltinTypeRef::Number),
            AttributeType::Boolean => TypeRef::Builtin(BuiltinTypeRef::Boolean),
            AttributeType::Date | AttributeType::DateTime => TypeRef::Builtin(BuiltinTypeRef::Date),
            AttributeType::Ref { mdo_type, name } => match mdo_ref_prefix(*mdo_type) {
                Some(prefix) => TypeRef::Name(QualifiedName::from_segments([
                    Name::new(prefix),
                    Name::new(name),
                ])),
                None => TypeRef::Unknown,
            },
            AttributeType::InformationRegisterRecordManager { name } => {
                TypeRef::Name(QualifiedName::from_segments([
                    Name::new("InformationRegisterRecordManager"),
                    Name::new(name),
                ]))
            }
            AttributeType::AnyRef => TypeRef::AnyRef,
            AttributeType::AnyObjectRef { mdo_type } => TypeRef::AnyRefOf(*mdo_type),
            AttributeType::Uuid => {
                TypeRef::Name(QualifiedName::from_segments([Name::new("УникальныйИдентификатор")]))
            }
            AttributeType::ValueStorage => {
                TypeRef::Name(QualifiedName::from_segments([Name::new("ХранилищеЗначения")]))
            }
            AttributeType::DefinedType { name } => TypeRef::Name(QualifiedName::from_segments([
                Name::new("ОпределяемыйТип"),
                Name::new(name),
            ])),
            AttributeType::Composite { types } => {
                let members: Vec<TypeRef> = types.iter().map(Self::from_attribute_type).collect();
                TypeRef::Union(members)
            }
            AttributeType::Platform(pvt) => platform_value_type_to_ref(*pvt),
            AttributeType::PlatformNamed(name) => {
                TypeRef::Name(QualifiedName::from_segments([Name::new(name)]))
            }
            AttributeType::UnknownNamed(_) => TypeRef::Unknown,
            AttributeType::Unknown => TypeRef::Unknown,
        }
    }
}

fn platform_value_type_to_ref(pvt: PlatformValueType) -> TypeRef {
    match pvt {
        PlatformValueType::ValueList => TypeRef::Builtin(BuiltinTypeRef::ValueList),
        PlatformValueType::ValueTable => TypeRef::Builtin(BuiltinTypeRef::ValueTable),
        PlatformValueType::Type => TypeRef::Builtin(BuiltinTypeRef::Type),
        PlatformValueType::Null => TypeRef::Builtin(BuiltinTypeRef::Null),
        PlatformValueType::ValueTree
        | PlatformValueType::StandardPeriod
        | PlatformValueType::StandardBeginningDate
        | PlatformValueType::TypeDescription
        | PlatformValueType::FixedStructure
        | PlatformValueType::FixedArray
        | PlatformValueType::FixedMap
        | PlatformValueType::FormattedString
        | PlatformValueType::SpreadsheetDocument
        | PlatformValueType::FormattedDocument
        | PlatformValueType::Picture
        | PlatformValueType::Color
        | PlatformValueType::Font
        | PlatformValueType::Chart
        | PlatformValueType::GanttChart
        | PlatformValueType::SettingsComposer
        | PlatformValueType::DataCompositionFilter
        | PlatformValueType::DynamicList
        | PlatformValueType::ConstantsSet
        | PlatformValueType::ReportBuilder => {
            TypeRef::Name(QualifiedName::from_segments([Name::new(pvt.russian_name())]))
        }
    }
}

fn mdo_ref_prefix(mdo: MdoType) -> Option<&'static str> {
    match mdo {
        MdoType::Catalog => Some("CatalogRef"),
        MdoType::Document => Some("DocumentRef"),
        MdoType::InformationRegister => Some("InformationRegisterRef"),
        MdoType::AccumulationRegister => Some("AccumulationRegisterRef"),
        MdoType::AccountingRegister => Some("AccountingRegisterRef"),
        MdoType::CalculationRegister => Some("CalculationRegisterRef"),
        MdoType::ChartOfCharacteristicTypes => Some("ChartOfCharacteristicTypesRef"),
        MdoType::ChartOfAccounts => Some("ChartOfAccountsRef"),
        MdoType::ChartOfCalculationTypes => Some("ChartOfCalculationTypesRef"),
        MdoType::BusinessProcess => Some("BusinessProcessRef"),
        MdoType::Task => Some("TaskRef"),
        MdoType::Enum => Some("EnumRef"),
        MdoType::ExchangePlan => Some("ExchangePlanRef"),
        MdoType::Constant => Some("ConstantValueManager"),
        MdoType::DataProcessor => Some("DataProcessorObject"),
        MdoType::Report => Some("ReportObject"),
        MdoType::ExternalDataProcessor => Some("ExternalDataProcessorObject"),
        MdoType::ExternalReport => Some("ExternalReportObject"),
        MdoType::ExternalDataSource
        | MdoType::CommonModule
        | MdoType::EventSubscription
        | MdoType::Subsystem
        | MdoType::Role => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_from_name_russian_and_english_case_insensitive() {
        assert_eq!(BuiltinTypeRef::from_name("Число"), Some(BuiltinTypeRef::Number));
        assert_eq!(BuiltinTypeRef::from_name("ЧИСЛО"), Some(BuiltinTypeRef::Number));
        assert_eq!(BuiltinTypeRef::from_name("Number"), Some(BuiltinTypeRef::Number));
        assert_eq!(BuiltinTypeRef::from_name("NUMBER"), Some(BuiltinTypeRef::Number));
        assert_eq!(BuiltinTypeRef::from_name("таблицаЗначений"), Some(BuiltinTypeRef::ValueTable));
    }

    #[test]
    fn builtin_from_name_rejects_non_primitives() {
        assert_eq!(BuiltinTypeRef::from_name("Массив"), None);
        assert_eq!(BuiltinTypeRef::from_name("Соответствие"), None);
        assert_eq!(BuiltinTypeRef::from_name("СправочникСсылка"), None);
        assert_eq!(BuiltinTypeRef::from_name(""), None);
    }

    #[test]
    fn type_ref_parses_primitive_names() {
        assert_eq!(
            TypeRef::from_bare_name("Число"),
            Some(TypeRef::Builtin(BuiltinTypeRef::Number))
        );
        assert_eq!(
            TypeRef::from_bare_name("boolean"),
            Some(TypeRef::Builtin(BuiltinTypeRef::Boolean))
        );
        assert_eq!(TypeRef::from_bare_name("Массив"), Some(TypeRef::Array(None)));
        assert_eq!(TypeRef::from_bare_name("MAP"), Some(TypeRef::Map(None)));
        assert_eq!(TypeRef::from_bare_name("Запрос"), None);
    }

    #[test]
    fn typeref_from_attribute_catalog_ref() {
        let attr =
            AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Товары".to_string() };

        match TypeRef::from_attribute_type(&attr) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.len(), 2);
                assert_eq!(qname.first().as_str(), "CatalogRef");
                assert_eq!(qname.last().as_str(), "Товары");
            }
            other => panic!("expected TypeRef::Name, got {other:?}"),
        }
    }

    #[test]
    fn typeref_from_attribute_primitives() {
        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::String { length: Some(64) }),
            TypeRef::Builtin(BuiltinTypeRef::String)
        );
        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::Number { precision: 15, scale: 2 }),
            TypeRef::Builtin(BuiltinTypeRef::Number)
        );
        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::Boolean),
            TypeRef::Builtin(BuiltinTypeRef::Boolean)
        );
        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::Date),
            TypeRef::Builtin(BuiltinTypeRef::Date)
        );
        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::DateTime),
            TypeRef::Builtin(BuiltinTypeRef::Date)
        );
    }

    #[test]
    fn typeref_from_attribute_any_ref_and_any_object_ref() {
        assert_eq!(TypeRef::from_attribute_type(&AttributeType::AnyRef), TypeRef::AnyRef);

        assert_eq!(
            TypeRef::from_attribute_type(&AttributeType::AnyObjectRef {
                mdo_type: MdoType::Document,
            }),
            TypeRef::AnyRefOf(MdoType::Document)
        );
    }

    #[test]
    fn typeref_from_attribute_defined_type() {
        let attr =
            AttributeType::DefinedType { name: "ПоказательУчёта".to_string() };
        match TypeRef::from_attribute_type(&attr) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.len(), 2);
                assert_eq!(qname.first().as_str(), "ОпределяемыйТип");
                assert_eq!(qname.last().as_str(), "ПоказательУчёта");
            }
            other => panic!("expected TypeRef::Name, got {other:?}"),
        }
    }

    #[test]
    fn typeref_from_attribute_composite_becomes_union() {
        let attr = AttributeType::Composite {
            types: vec![AttributeType::Boolean, AttributeType::String { length: None }],
        };
        assert_eq!(
            TypeRef::from_attribute_type(&attr),
            TypeRef::Union(vec![
                TypeRef::Builtin(BuiltinTypeRef::Boolean),
                TypeRef::Builtin(BuiltinTypeRef::String),
            ])
        );
    }

    #[test]
    fn typeref_from_attribute_composite_recurses_on_members() {
        let attr = AttributeType::Composite {
            types: vec![
                AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Товары".to_string() },
                AttributeType::Number { precision: 10, scale: 0 },
            ],
        };
        match TypeRef::from_attribute_type(&attr) {
            TypeRef::Union(members) => {
                assert_eq!(members.len(), 2);
                match &members[0] {
                    TypeRef::Name(q) => {
                        assert_eq!(q.first().as_str(), "CatalogRef");
                        assert_eq!(q.last().as_str(), "Товары");
                    }
                    other => panic!("expected TypeRef::Name for catalog ref, got {other:?}"),
                }
                assert_eq!(members[1], TypeRef::Builtin(BuiltinTypeRef::Number));
            }
            other => panic!("expected TypeRef::Union, got {other:?}"),
        }
    }

    #[test]
    fn typeref_from_attribute_unknown_passthrough() {
        assert_eq!(TypeRef::from_attribute_type(&AttributeType::Unknown), TypeRef::Unknown);
    }

    #[test]
    fn typeref_from_attribute_platform_kernel_backed_variants() {
        for (pvt, expected) in [
            (PlatformValueType::ValueList, BuiltinTypeRef::ValueList),
            (PlatformValueType::ValueTable, BuiltinTypeRef::ValueTable),
            (PlatformValueType::Type, BuiltinTypeRef::Type),
            (PlatformValueType::Null, BuiltinTypeRef::Null),
        ] {
            assert_eq!(
                TypeRef::from_attribute_type(&AttributeType::Platform(pvt)),
                TypeRef::Builtin(expected),
                "{pvt:?}",
            );
        }
    }

    #[test]
    fn typeref_from_attribute_platform_object_fallback_variants() {
        for (pvt, name) in [
            (PlatformValueType::ValueTree, "ДеревоЗначений"),
            (PlatformValueType::StandardPeriod, "СтандартныйПериод"),
            (PlatformValueType::StandardBeginningDate, "СтандартнаяДатаНачала"),
            (PlatformValueType::FixedStructure, "ФиксированнаяСтруктура"),
            (PlatformValueType::FixedArray, "ФиксированныйМассив"),
            (PlatformValueType::FixedMap, "ФиксированноеСоответствие"),
            (PlatformValueType::TypeDescription, "ОписаниеТипов"),
            (PlatformValueType::FormattedString, "ФорматированнаяСтрока"),
            (PlatformValueType::SpreadsheetDocument, "ТабличныйДокумент"),
            (PlatformValueType::Picture, "Картинка"),
            (PlatformValueType::Color, "Цвет"),
            (PlatformValueType::DynamicList, "ДинамическийСписок"),
            (PlatformValueType::ConstantsSet, "КонстантыНабор"),
        ] {
            match TypeRef::from_attribute_type(&AttributeType::Platform(pvt)) {
                TypeRef::Name(qname) => {
                    assert_eq!(qname.len(), 1, "{pvt:?}");
                    assert_eq!(qname.first().as_str(), name, "{pvt:?}");
                }
                other => panic!("expected single-segment Name for {pvt:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn typeref_from_attribute_register_refs_use_xml_token() {
        let reg = AttributeType::Ref {
            mdo_type: MdoType::InformationRegister,
            name: "ЦеныНоменклатуры".to_string(),
        };
        match TypeRef::from_attribute_type(&reg) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.first().as_str(), "InformationRegisterRef");
                assert_eq!(qname.last().as_str(), "ЦеныНоменклатуры");
            }
            other => panic!("expected TypeRef::Name, got {other:?}"),
        }

        for mdo in [
            MdoType::AccumulationRegister,
            MdoType::AccountingRegister,
            MdoType::CalculationRegister,
        ] {
            let attr = AttributeType::Ref { mdo_type: mdo, name: "Х".to_string() };
            match TypeRef::from_attribute_type(&attr) {
                TypeRef::Name(qname) => assert!(
                    qname.first().as_str().ends_with("RegisterRef"),
                    "expected `*RegisterRef` prefix for {mdo:?}, got {}",
                    qname.first().as_str()
                ),
                other => panic!("expected TypeRef::Name for {mdo:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn typeref_from_attribute_constant_uses_value_manager() {
        let attr = AttributeType::Ref {
            mdo_type: MdoType::Constant,
            name: "ВалютаУчёта".to_string(),
        };
        match TypeRef::from_attribute_type(&attr) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.first().as_str(), "ConstantValueManager");
                assert_eq!(qname.last().as_str(), "ВалютаУчёта");
            }
            other => panic!("expected TypeRef::Name, got {other:?}"),
        }
    }

    #[test]
    fn typeref_from_attribute_unreachable_mdo_kinds_fall_to_unknown() {
        for mdo in [MdoType::ExternalDataSource, MdoType::CommonModule] {
            let attr = AttributeType::Ref { mdo_type: mdo, name: "Х".to_string() };
            assert_eq!(
                TypeRef::from_attribute_type(&attr),
                TypeRef::Unknown,
                "expected Unknown for {mdo:?}"
            );
        }
    }

    #[test]
    fn typeref_from_attribute_data_processor_and_report_round_trip_via_object_token() {
        for (mdo, prefix) in
            [(MdoType::DataProcessor, "DataProcessorObject"), (MdoType::Report, "ReportObject")]
        {
            let attr = AttributeType::Ref { mdo_type: mdo, name: "Х".to_string() };
            match TypeRef::from_attribute_type(&attr) {
                TypeRef::Name(qname) => {
                    assert_eq!(qname.first().as_str(), prefix, "mdo_ref_prefix({mdo:?})");
                    assert_eq!(qname.last().as_str(), "Х");
                }
                other => panic!("expected TypeRef::Name({prefix}.Х) for {mdo:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn typeref_from_attribute_uuid_and_value_storage_single_segment() {
        match TypeRef::from_attribute_type(&AttributeType::Uuid) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.len(), 1);
                assert_eq!(qname.first().as_str(), "УникальныйИдентификатор");
            }
            other => panic!("expected single-segment TypeRef::Name, got {other:?}"),
        }

        match TypeRef::from_attribute_type(&AttributeType::ValueStorage) {
            TypeRef::Name(qname) => {
                assert_eq!(qname.len(), 1);
                assert_eq!(qname.first().as_str(), "ХранилищеЗначения");
            }
            other => panic!("expected single-segment TypeRef::Name, got {other:?}"),
        }
    }
}
