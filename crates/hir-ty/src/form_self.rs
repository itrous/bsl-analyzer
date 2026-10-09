use bsl_metadata::{AttributeType, Form, MdoType, PlatformValueType};
use bsl_platform::PlatformDataInner;
use hir_def::resolver::Resolver;
use hir_def::Name;

use crate::db::HirDatabase;
use crate::platform_property_lookup::{
    lookup_platform_property_by_type, PlatformPropertyResolution,
};

pub const FORM_TYPE_NAME: &str = "ФормаКлиентскогоПриложения";

pub fn managed_form_platform_type_names(form: &Form) -> impl Iterator<Item = &'static str> {
    std::iter::once(FORM_TYPE_NAME).chain(
        form.main_attribute()
            .and_then(|attribute| managed_form_extension_type_name(&attribute.attr_type)),
    )
}

/// Every extension the platform can mix into a managed form through its main
/// attribute. A form whose main attribute type was not read may carry any one of
/// them, so their union is the honest upper bound of what it adds.
const MANAGED_FORM_EXTENSION_TYPE_NAMES: &[&str] = &[
    "Расширение формы клиентского приложения для справочника",
    "Расширение формы клиентского приложения для документа",
    "Расширение формы клиентского приложения для плана видов характеристик",
    "Расширение формы клиентского приложения для бизнес-процесса",
    "Расширение формы клиентского приложения для задачи",
    "Расширение формы клиентского приложения для обработки",
    "Расширение формы клиентского приложения для отчета",
    "Расширение формы клиентского приложения для констант",
    "Расширение формы клиентского приложения для набора записей",
    "Расширение формы клиентского приложения для записи регистра сведений",
    "Расширение формы клиентского приложения для объектов",
    "Расширение формы клиентского приложения для динамического списка",
    "Расширение формы клиентского приложения для компоновщика настроек",
];

/// The platform types whose methods a managed form module calls without a
/// receiver: the form itself plus the extension its main attribute mixes in.
///
/// A main attribute of a type that was not read (a defined type, a composite)
/// cannot name its extension, so every extension is included: a bare name that
/// misses even that union is absent on any reading of the attribute.
pub(crate) fn managed_form_self_method_types(form: &Form) -> Vec<&'static str> {
    let mut types = vec![FORM_TYPE_NAME];
    let Some(attribute) = form.main_attribute() else { return types };
    match managed_form_extension_type_name(&attribute.attr_type) {
        Some(extension) => types.push(extension),
        None if main_attribute_type_is_opaque(&attribute.attr_type) => {
            types.extend_from_slice(MANAGED_FORM_EXTENSION_TYPE_NAMES)
        }
        None => {}
    }
    types
}

/// Scalars and the platform collections never carry a form extension; anything
/// else may stand for an object the parser did not classify.
fn main_attribute_type_is_opaque(attr_type: &AttributeType) -> bool {
    !matches!(
        attr_type,
        AttributeType::String { .. }
            | AttributeType::Number { .. }
            | AttributeType::Boolean
            | AttributeType::Date
            | AttributeType::DateTime
            | AttributeType::Uuid
            | AttributeType::ValueStorage
            | AttributeType::Platform(_)
    )
}

fn managed_form_extension_type_name(attr_type: &AttributeType) -> Option<&'static str> {
    match attr_type {
        AttributeType::AnyObjectRef { mdo_type } | AttributeType::Ref { mdo_type, .. } => {
            Some(match mdo_type {
                MdoType::Catalog => "Расширение формы клиентского приложения для справочника",
                MdoType::Document => "Расширение формы клиентского приложения для документа",
                MdoType::ChartOfCharacteristicTypes => {
                    "Расширение формы клиентского приложения для плана видов характеристик"
                }
                MdoType::BusinessProcess => {
                    "Расширение формы клиентского приложения для бизнес-процесса"
                }
                MdoType::Task => "Расширение формы клиентского приложения для задачи",
                // An external data processor's or report's form is extended exactly as
                // the configuration's own: the platform lists no separate extension.
                MdoType::DataProcessor | MdoType::ExternalDataProcessor => {
                    "Расширение формы клиентского приложения для обработки"
                }
                MdoType::Report | MdoType::ExternalReport => {
                    "Расширение формы клиентского приложения для отчета"
                }
                MdoType::Constant => "Расширение формы клиентского приложения для констант",
                mdo_type if mdo_type.is_register() => {
                    "Расширение формы клиентского приложения для набора записей"
                }
                _ => "Расширение формы клиентского приложения для объектов",
            })
        }
        AttributeType::InformationRegisterRecordManager { .. } => {
            Some("Расширение формы клиентского приложения для записи регистра сведений")
        }
        AttributeType::Platform(PlatformValueType::ConstantsSet) => {
            Some("Расширение формы клиентского приложения для констант")
        }
        AttributeType::Platform(PlatformValueType::DynamicList) => {
            Some("Расширение формы клиентского приложения для динамического списка")
        }
        AttributeType::Platform(PlatformValueType::SettingsComposer) => {
            Some("Расширение формы клиентского приложения для компоновщика настроек")
        }
        _ => None,
    }
}

pub(crate) fn resolve_form_self_property(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<PlatformPropertyResolution> {
    let resolution = lookup_platform_property_by_type(db, FORM_TYPE_NAME, name)?;
    if !crate::this_object::is_managed_form_module(db, resolver) {
        return None;
    }
    Some(resolution)
}

pub fn is_form_self_property_name(name: &str) -> bool {
    PlatformDataInner::instance().get_property(FORM_TYPE_NAME, name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::{FormAttribute, FormType};
    use uuid::Uuid;

    fn form_with_main_type(attr_type: AttributeType) -> Form {
        let mut form = Form::new("Форма".to_string(), FormType::Managed, Uuid::nil());
        form.attributes.push(FormAttribute {
            name: "Объект".to_string(),
            attr_type,
            is_main: true,
            columns: vec![],
        });
        form
    }

    #[test]
    fn managed_form_platform_types_follow_main_attribute() {
        let document =
            form_with_main_type(AttributeType::AnyObjectRef { mdo_type: MdoType::Document });
        assert_eq!(
            managed_form_platform_type_names(&document).collect::<Vec<_>>(),
            [FORM_TYPE_NAME, "Расширение формы клиентского приложения для документа"]
        );

        let report = form_with_main_type(AttributeType::AnyObjectRef { mdo_type: MdoType::Report });
        assert_eq!(
            managed_form_platform_type_names(&report).collect::<Vec<_>>(),
            [FORM_TYPE_NAME, "Расширение формы клиентского приложения для отчета"]
        );

        let list = form_with_main_type(AttributeType::Platform(PlatformValueType::DynamicList));
        assert_eq!(
            managed_form_platform_type_names(&list).collect::<Vec<_>>(),
            [FORM_TYPE_NAME, "Расширение формы клиентского приложения для динамического списка"]
        );
    }

    /// The union stands in for an unread main attribute only while it holds every
    /// extension the catalog knows; a new platform extension must join it.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn the_extension_union_is_the_catalog_extension_set() {
        let data = PlatformDataInner::instance();
        let mut catalog: Vec<String> = data
            .all_types()
            .iter()
            .map(|ty| ty.name.to_string())
            .filter(|name| name.starts_with("Расширение формы клиентского приложения"))
            .collect();
        catalog.sort();
        let mut listed: Vec<String> =
            MANAGED_FORM_EXTENSION_TYPE_NAMES.iter().map(|name| name.to_string()).collect();
        listed.sort();
        assert_eq!(listed, catalog);
    }

    #[test]
    fn an_unread_main_attribute_widens_to_every_extension() {
        let record = form_with_main_type(AttributeType::UnknownNamed(
            "cfg:InformationRegisterRecordManager.Курсы".to_string(),
        ));
        let types = managed_form_self_method_types(&record);
        assert!(
            types.contains(&"Расширение формы клиентского приложения для записи регистра сведений")
        );
        assert_eq!(types.len(), 1 + MANAGED_FORM_EXTENSION_TYPE_NAMES.len());

        let scalar = form_with_main_type(AttributeType::String { length: None });
        assert_eq!(managed_form_self_method_types(&scalar), [FORM_TYPE_NAME]);

        let external =
            form_with_main_type(AttributeType::AnyObjectRef { mdo_type: MdoType::ExternalReport });
        assert_eq!(
            managed_form_self_method_types(&external),
            [FORM_TYPE_NAME, "Расширение формы клиентского приложения для отчета"]
        );
    }

    #[test]
    fn a_record_form_gets_the_record_extension_alone() {
        let record = form_with_main_type(AttributeType::InformationRegisterRecordManager {
            name: "Курсы".to_string(),
        });
        assert_eq!(
            managed_form_self_method_types(&record),
            [
                FORM_TYPE_NAME,
                "Расширение формы клиентского приложения для записи регистра сведений"
            ]
        );

        let record_set = form_with_main_type(AttributeType::Ref {
            mdo_type: MdoType::InformationRegister,
            name: "Курсы".to_string(),
        });
        assert_eq!(
            managed_form_self_method_types(&record_set),
            [FORM_TYPE_NAME, "Расширение формы клиентского приложения для набора записей"]
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn is_form_self_property_name_recognizes_known_russian_props() {
        for name in &["Элементы", "Команды", "Параметры", "ТекущийЭлемент", "Заголовок"]
        {
            assert!(
                is_form_self_property_name(name),
                "expected {name:?} to be a form-self property"
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn is_form_self_property_name_is_bilingual() {
        for name in &["Items", "Commands", "Title"] {
            assert!(is_form_self_property_name(name), "expected English alias {name:?} to resolve");
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn is_form_self_property_name_is_case_insensitive() {
        assert!(is_form_self_property_name("элементы"));
        assert!(is_form_self_property_name("ЭЛЕМЕНТЫ"));
    }

    #[test]
    fn is_form_self_property_name_rejects_non_members() {
        assert!(!is_form_self_property_name("ЭтоТочноНеСвойствоФормы12345"));
    }

    #[test]
    fn no_form_property_collides_with_mdo_plural() {
        let data = PlatformDataInner::instance();
        for prop in data.get_type_properties(FORM_TYPE_NAME) {
            assert!(
                bsl_metadata::MdoType::from_plural(&prop.name).is_none(),
                "form property {:?} collides with an MdoType plural — cascade order \
                 in infer_path_name must be revisited",
                prop.name
            );
            assert!(
                bsl_metadata::MdoType::from_plural(&prop.english_name).is_none(),
                "English form property {:?} collides with an MdoType plural — cascade \
                 order in infer_path_name must be revisited",
                prop.english_name
            );
        }
    }
}
