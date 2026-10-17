//! Metadata named by a string argument of a platform function.
//!
//! `Тип("СправочникСсылка.Товары")`, `ПредопределенноеЗначение("Перечисление.Цвета.Красный")`
//! and `ОткрытьФорму("Справочник.Товары.ФормаСписка")` name metadata by value. The
//! platform's extended module check resolves such a literal against the configuration
//! and reports a miss as «Возможно ошибочный параметр»; the call itself fails at run
//! time. The spelling written as code (`Справочники.Товары`, `Перечисления.Цвета.Красный`)
//! is judged by the manager lookup — this module asks the same questions of the string.
//!
//! Only what the configuration model answers exhaustively is judged: the object (for
//! every path), and the predefined value of an enum, a catalog or a chart. A form name
//! is never judged — the model does not list an object's forms, and the standard ones
//! (`ФормаСписка`, `ФормаВыбора`, …) exist without being declared.

use std::str::FromStr;

use bsl_metadata::MdoType;
use stdx::case::CaseExt;

use crate::object_resolver::ObjectResolver;

/// A platform function whose first argument is a metadata path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataPathFunction {
    /// `Тип` / `Type`: a type name, `<Вид><Фасет>.<Имя>`.
    Type,
    /// `ПредопределенноеЗначение` / `PredefinedValue`: `<Вид>.<Имя>.<Значение>`.
    PredefinedValue,
    /// `ОткрытьФорму`, `ОткрытьФормуМодально`, `ПолучитьФорму`: `<Вид>.<Имя>.<Форма…>`.
    Form,
}

impl MetadataPathFunction {
    pub(crate) fn of(name: &str) -> Option<Self> {
        match name.fold_lower().as_str() {
            "тип" | "type" => Some(Self::Type),
            "предопределенноезначение" | "predefinedvalue" => {
                Some(Self::PredefinedValue)
            }
            "открытьформу"
            | "openform"
            | "открытьформумодально"
            | "openformmodal"
            | "получитьформу"
            | "getform" => Some(Self::Form),
            _ => None,
        }
    }
}

/// The part of a metadata path the configuration does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingMetadata {
    /// No object of this kind and name.
    Object { kind: MdoType, name: String },
    /// The object exists and lists its predefined values, but not this one.
    PredefinedValue { kind: MdoType, owner: String, value: String },
}

/// What `text`, the literal argument of `function`, names that the configuration does
/// not have. `None` when the path is fine or is not one this check can judge: another
/// shape, a kind outside the configuration (external processors and reports), a part
/// the model does not list.
pub(crate) fn missing_metadata(
    function: MetadataPathFunction,
    text: &str,
    resolver: &dyn ObjectResolver,
) -> Option<MissingMetadata> {
    let segments: Vec<&str> = text.trim().split('.').collect();
    if !segments.iter().all(|segment| crate::lower::type_string::is_identifier_shaped(segment)) {
        return None;
    }
    match function {
        MetadataPathFunction::Type => {
            let [prefix, name] = segments[..] else {
                return None;
            };
            let kind = crate::lower::metadata_kind_from_prefix(prefix)?;
            let mdo = crate::field_enum::mdo_type_for_kind(kind)
                .or_else(|| crate::field_enum::register_parent_for_kind(kind))
                .filter(|mdo| is_configuration_kind(*mdo))?;
            object_missing(resolver, mdo, name)
        }
        MetadataPathFunction::Form => {
            let (mdo, name) = configuration_object(&segments)?;
            object_missing(resolver, mdo, name)
        }
        MetadataPathFunction::PredefinedValue => {
            let (mdo, name) = configuration_object(&segments)?;
            if let Some(missing) = object_missing(resolver, mdo, name) {
                return Some(missing);
            }
            let [_, _, value] = segments[..] else {
                return None;
            };
            predefined_value_missing(resolver, mdo, name, value)
        }
    }
}

/// `<Вид>.<Имя>…` of a kind the configuration holds by name.
fn configuration_object<'a>(segments: &[&'a str]) -> Option<(MdoType, &'a str)> {
    let [kind, name, ..] = segments[..] else {
        return None;
    };
    let mdo = MdoType::from_str(kind).ok().filter(|mdo| is_configuration_kind(*mdo))?;
    Some((mdo, name))
}

/// The kinds a metadata path names a configuration object of. External processors and
/// reports are not in the configuration at all; the rest of [`MdoType`] is not named
/// by a path these functions take.
fn is_configuration_kind(mdo: MdoType) -> bool {
    matches!(
        mdo,
        MdoType::Catalog
            | MdoType::Document
            | MdoType::InformationRegister
            | MdoType::AccumulationRegister
            | MdoType::AccountingRegister
            | MdoType::CalculationRegister
            | MdoType::ChartOfCharacteristicTypes
            | MdoType::ChartOfAccounts
            | MdoType::ChartOfCalculationTypes
            | MdoType::BusinessProcess
            | MdoType::Task
            | MdoType::Enum
            | MdoType::ExchangePlan
            | MdoType::Constant
            | MdoType::DataProcessor
            | MdoType::Report
    )
}

fn object_missing(
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    name: &str,
) -> Option<MissingMetadata> {
    (!crate::manager_lookup::collection_member_exists(resolver, kind, name))
        .then(|| MissingMetadata::Object { kind, name: name.to_string() })
}

/// The same membership `Перечисления.Цвета.Красный` is judged by
/// ([`crate::manager_lookup::lookup_predefined`]), plus the empty reference every one of
/// these kinds has without declaring it.
fn predefined_value_missing(
    resolver: &dyn ObjectResolver,
    kind: MdoType,
    owner: &str,
    value: &str,
) -> Option<MissingMetadata> {
    if matches!(value.fold_lower().as_str(), "пустаяссылка" | "emptyref") {
        return None;
    }
    let mdo = resolver.resolve_metadata_object(kind, owner)?;
    let found = match kind {
        MdoType::Enum => mdo.find_enum_value(value).is_some(),
        MdoType::Catalog
        | MdoType::ChartOfAccounts
        | MdoType::ChartOfCharacteristicTypes
        | MdoType::ChartOfCalculationTypes => mdo.find_predefined_item(value).is_some(),
        _ => return None,
    };
    (!found).then(|| MissingMetadata::PredefinedValue {
        kind,
        owner: owner.to_string(),
        value: value.to_string(),
    })
}
