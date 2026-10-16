use stdx::case::CaseExt;
use uuid::Uuid;

use crate::enums::FormType;
use crate::metadata_object::AttributeType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FormElementKind {
    Table = 0,
    Group = 1,
    Field = 2,
    Button = 3,
    Decoration = 4,
    Addition = 5,
    Other = 6,
    UsualGroup = 7,
    Pages = 8,
    Page = 9,
    CommandBar = 10,
    ButtonGroup = 11,
}

impl FormElementKind {
    pub fn base_platform_type_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Table => "ТаблицаФормы",
            Self::Group
            | Self::UsualGroup
            | Self::Pages
            | Self::Page
            | Self::CommandBar
            | Self::ButtonGroup => "ГруппаФормы",
            Self::Field => "ПолеФормы",
            Self::Button => "КнопкаФормы",
            Self::Decoration => "ДекорацияФормы",
            Self::Addition => "ДополнениеЭлементаФормы",
            Self::Other => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormElement {
    pub name: String,
    pub id: u32,
    pub data_path: Option<String>,
    pub kind: FormElementKind,
    pub parent_id: Option<u32>,
}

impl FormElement {
    /// Heap bytes owned by this element: its name/data-path strings. `id`,
    /// `kind`, `parent_id` are `Copy` and own no heap.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity() + self.data_path.as_ref().map_or(0, String::capacity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormEventHandler {
    pub event_type: String,
    pub handler_name: String,
}

impl FormEventHandler {
    /// Heap bytes owned by this handler: its event-type/handler-name strings.
    pub fn estimated_heap_size(&self) -> usize {
        self.event_type.capacity() + self.handler_name.capacity()
    }
}

impl FormElement {
    pub fn new(name: impl Into<String>, id: u32, data_path: Option<String>) -> Self {
        Self { name: name.into(), id, data_path, kind: FormElementKind::Other, parent_id: None }
    }

    pub fn with_kind(
        name: impl Into<String>,
        id: u32,
        data_path: Option<String>,
        kind: FormElementKind,
        parent_id: Option<u32>,
    ) -> Self {
        Self { name: name.into(), id, data_path, kind, parent_id }
    }

    pub fn has_wrong_data_path(&self) -> bool {
        self.data_path.as_ref().is_some_and(|dp| dp.starts_with('~'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormAttributeColumn {
    pub name: String,
    pub attr_type: AttributeType,
}

impl FormAttributeColumn {
    /// Heap bytes owned by this column: its name plus its type's own payload
    /// (only the name-carrying/composite `AttributeType` variants own heap).
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity() + self.attr_type.estimated_heap_size()
    }
}

/// Columns a form adds to a table inside one of its attributes
/// (`<AdditionalColumns table="Объект.Товары">`): they exist on that table's rows
/// in this form only, never in the object itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormAdditionalColumns {
    /// Data path of the extended table, as the form spells it: `Объект.Товары`.
    pub table: String,
    pub columns: Vec<FormAttributeColumn>,
}

impl FormAdditionalColumns {
    /// Heap bytes owned by this group: the table path and the column vec.
    pub fn estimated_heap_size(&self) -> usize {
        self.table.capacity()
            + stdx::heap::vec_bytes::<FormAttributeColumn>(self.columns.len())
            + self.columns.iter().map(FormAttributeColumn::estimated_heap_size).sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormAttribute {
    pub name: String,
    pub attr_type: AttributeType,
    pub is_main: bool,
    pub columns: Vec<FormAttributeColumn>,
    pub additional_columns: Vec<FormAdditionalColumns>,
}

impl FormAttribute {
    pub fn new(name: impl Into<String>, attr_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            attr_type,
            is_main: false,
            columns: Vec::new(),
            additional_columns: Vec::new(),
        }
    }

    /// Heap bytes owned by this attribute: its name, its type's own payload,
    /// the tabular-section column vec and the additional-column groups.
    /// `is_main` is `Copy` and owns no heap.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.attr_type.estimated_heap_size()
            + stdx::heap::vec_bytes::<FormAttributeColumn>(self.columns.len())
            + self.columns.iter().map(FormAttributeColumn::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<FormAdditionalColumns>(self.additional_columns.len())
            + self
                .additional_columns
                .iter()
                .map(FormAdditionalColumns::estimated_heap_size)
                .sum::<usize>()
    }
}

/// Events of an ordinary form and of its object/saved-values extension, as
/// `(event, default handler)` pairs: the configurator names a new handler after its
/// event, in the configuration's script language. Used only when the form's real
/// bindings cannot be read - see [`Form::ordinary_without_dialog`].
pub const ORDINARY_FORM_DEFAULT_HANDLERS: &[(&str, &str)] = &[
    ("ПередОткрытием", "ПередОткрытием"),
    ("BeforeOpen", "BeforeOpen"),
    ("ПриОткрытии", "ПриОткрытии"),
    ("OnOpen", "OnOpen"),
    ("ПриПовторномОткрытии", "ПриПовторномОткрытии"),
    ("OnReopen", "OnReopen"),
    ("ПередЗакрытием", "ПередЗакрытием"),
    ("BeforeClose", "BeforeClose"),
    ("ПриЗакрытии", "ПриЗакрытии"),
    ("OnClose", "OnClose"),
    ("ОбработкаВыбора", "ОбработкаВыбора"),
    ("ChoiceProcessing", "ChoiceProcessing"),
    ("ОбработкаОповещения", "ОбработкаОповещения"),
    ("NotificationProcessing", "NotificationProcessing"),
    ("ОбработкаЗаписиНовогоОбъекта", "ОбработкаЗаписиНовогоОбъекта"),
    ("NewObjectWriteProcessing", "NewObjectWriteProcessing"),
    ("ОбновлениеОтображения", "ОбновлениеОтображения"),
    ("RefreshDisplay", "RefreshDisplay"),
    ("ВнешнееСобытие", "ВнешнееСобытие"),
    ("ExternalEvent", "ExternalEvent"),
    // The form extension: object forms bind the write events and the data change,
    // report and data processor forms the saved-values pair.
    ("ПередЗаписью", "ПередЗаписью"),
    ("BeforeWrite", "BeforeWrite"),
    ("ПриЗаписи", "ПриЗаписи"),
    ("OnWrite", "OnWrite"),
    ("ПослеЗаписи", "ПослеЗаписи"),
    ("AfterWrite", "AfterWrite"),
    ("ПриИзмененииДанных", "ПриИзмененииДанных"),
    ("OnDataChange", "OnDataChange"),
    ("ПередСохранениемЗначений", "ПередСохранениемЗначений"),
    ("BeforeSaveValues", "BeforeSaveValues"),
    ("ПослеВосстановленияЗначений", "ПослеВосстановленияЗначений"),
    ("AfterRestoreValues", "AfterRestoreValues"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub name: String,
    pub form_type: FormType,
    pub uuid: Uuid,
    pub elements: Vec<FormElement>,
    pub event_handlers: Vec<FormEventHandler>,
    pub command_handlers: Vec<String>,
    pub attributes: Vec<FormAttribute>,
}

impl Form {
    pub fn new(name: String, form_type: FormType, uuid: Uuid) -> Self {
        Self {
            name,
            form_type,
            uuid,
            elements: Vec::new(),
            event_handlers: Vec::new(),
            command_handlers: Vec::new(),
            attributes: Vec::new(),
        }
    }

    pub fn with_elements(
        name: String,
        form_type: FormType,
        uuid: Uuid,
        elements: Vec<FormElement>,
    ) -> Self {
        Self {
            name,
            form_type,
            uuid,
            elements,
            event_handlers: Vec::new(),
            command_handlers: Vec::new(),
            attributes: Vec::new(),
        }
    }

    pub fn with_handlers(
        name: String,
        form_type: FormType,
        uuid: Uuid,
        elements: Vec<FormElement>,
        event_handlers: Vec<FormEventHandler>,
        command_handlers: Vec<String>,
    ) -> Self {
        Self {
            name,
            form_type,
            uuid,
            elements,
            event_handlers,
            command_handlers,
            attributes: Vec::new(),
        }
    }

    pub fn elements(&self) -> &[FormElement] {
        &self.elements
    }

    pub fn elements_with_wrong_data_path(&self) -> impl Iterator<Item = &FormElement> {
        self.elements.iter().filter(|e| e.has_wrong_data_path())
    }

    pub fn find_element(&self, name: &str) -> Option<&FormElement> {
        let name_lower = name.fold_lower();
        self.elements.iter().find(|e| e.name.fold_lower() == name_lower)
    }

    pub fn children_of(&self, parent_id: u32) -> impl Iterator<Item = &FormElement> {
        self.elements.iter().filter(move |e| e.parent_id == Some(parent_id))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn form_type(&self) -> FormType {
        self.form_type
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn attributes(&self) -> &[FormAttribute] {
        &self.attributes
    }

    pub fn attribute_names(&self) -> impl Iterator<Item = &str> {
        self.attributes.iter().map(|a| a.name.as_str())
    }

    pub fn find_attribute(&self, name: &str) -> Option<&FormAttribute> {
        self.attributes.iter().find(|a| stdx::case::eq_ignore_case(&a.name, name))
    }

    pub fn main_attribute(&self) -> Option<&FormAttribute> {
        self.attributes.iter().find(|a| a.is_main)
    }

    pub fn is_managed(&self) -> bool {
        self.form_type == FormType::Managed
    }

    pub fn is_ordinary(&self) -> bool {
        self.form_type == FormType::Ordinary
    }

    pub fn event_handlers(&self) -> &[FormEventHandler] {
        &self.event_handlers
    }

    pub fn event_handler_names(&self) -> Vec<&str> {
        self.event_handlers.iter().map(|h| h.handler_name.as_str()).collect()
    }

    pub fn command_handlers(&self) -> &[String] {
        &self.command_handlers
    }

    /// Heap bytes owned by this form, memoised by `ide-db`'s
    /// `module_metadata_query` for Salsa's `heap_size` hook: its name plus the
    /// element/handler/command-handler/attribute vecs and each entry's own
    /// owned payload. `form_type`/`uuid` are `Copy` and own no heap. New
    /// heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + stdx::heap::vec_bytes::<FormElement>(self.elements.len())
            + self.elements.iter().map(FormElement::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<FormEventHandler>(self.event_handlers.len())
            + self.event_handlers.iter().map(FormEventHandler::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<String>(self.command_handlers.len())
            + self.command_handlers.iter().map(String::capacity).sum::<usize>()
            + stdx::heap::vec_bytes::<FormAttribute>(self.attributes.len())
            + self.attributes.iter().map(FormAttribute::estimated_heap_size).sum::<usize>()
    }

    /// An ORDINARY form whose dialog cannot be read: it lives in the binary
    /// `Ext/Form.bin`, not in `Ext/Form.xml`. Its bindings are unknown, so the form
    /// gets the configurator's default handler name for each of its own events
    /// ([`ORDINARY_FORM_DEFAULT_HANDLERS`]): a procedure named `ПередОткрытием` in
    /// such a module is almost certainly bound, and reporting it unused would be the
    /// louder mistake.
    pub fn ordinary_without_dialog(name: impl Into<String>) -> Self {
        let mut form = Self::new(name.into(), FormType::Ordinary, Uuid::nil());
        form.event_handlers = ORDINARY_FORM_DEFAULT_HANDLERS
            .iter()
            .map(|&(event_type, handler_name)| FormEventHandler {
                event_type: event_type.to_string(),
                handler_name: handler_name.to_string(),
            })
            .collect();
        form
    }

    /// Apply an extension overlay (a borrowed form of the same name) onto this
    /// base form. The dialog — identity, elements, handlers — comes from the
    /// overlay alone; the base attributes stay, and an overlay attribute
    /// replaces a same-named one whole (`is_main`, type, columns) or is added.
    pub fn apply_extension_overlay(&mut self, overlay: &Form) {
        let inherited = std::mem::replace(self, overlay.clone());

        let mut attributes = inherited.attributes;
        for attr in &overlay.attributes {
            match attributes
                .iter_mut()
                .find(|existing| stdx::case::eq_ignore_case(&existing.name, &attr.name))
            {
                Some(existing) => *existing = attr.clone(),
                None => attributes.push(attr.clone()),
            }
        }
        self.attributes = attributes;
    }

    pub fn is_handler(&self, method_name: &str) -> bool {
        let name_lower = method_name.fold_lower();
        self.event_handlers.iter().any(|h| h.handler_name.fold_lower() == name_lower)
            || self.command_handlers.iter().any(|h| h.fold_lower() == name_lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_form_creation() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        let form = Form::new("ФормаЭлемента".to_string(), FormType::Managed, uuid);

        assert_eq!(form.name(), "ФормаЭлемента");
        assert_eq!(form.form_type(), FormType::Managed);
        assert!(form.is_managed());
        assert!(!form.is_ordinary());
        assert!(form.elements().is_empty());
    }

    #[test]
    fn test_ordinary_form() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        let form = Form::new("ОбычнаяФорма".to_string(), FormType::Ordinary, uuid);

        assert!(form.is_ordinary());
        assert!(!form.is_managed());
    }

    #[test]
    fn test_form_element_has_wrong_data_path() {
        let wrong = FormElement::new(
            "НесуществующийРеквизит",
            1,
            Some("~Объект.НесуществующийРеквизит".to_string()),
        );
        assert!(wrong.has_wrong_data_path());

        let ok = FormElement::new("Код", 2, Some("Объект.Code".to_string()));
        assert!(!ok.has_wrong_data_path());

        let no_path = FormElement::new("Кнопка", 3, None);
        assert!(!no_path.has_wrong_data_path());
    }

    #[test]
    fn test_form_element_new_defaults_to_other_kind_and_no_parent() {
        let elem = FormElement::new("Код", 1, Some("Объект.Code".to_string()));
        assert_eq!(elem.kind, FormElementKind::Other);
        assert_eq!(elem.parent_id, None);
    }

    #[test]
    fn test_form_element_kind_discriminants_are_pinned() {
        assert_eq!(FormElementKind::Table as u8, 0);
        assert_eq!(FormElementKind::Group as u8, 1);
        assert_eq!(FormElementKind::Field as u8, 2);
        assert_eq!(FormElementKind::Button as u8, 3);
        assert_eq!(FormElementKind::Decoration as u8, 4);
        assert_eq!(FormElementKind::Addition as u8, 5);
        assert_eq!(FormElementKind::Other as u8, 6);
        assert_eq!(FormElementKind::UsualGroup as u8, 7);
        assert_eq!(FormElementKind::Pages as u8, 8);
        assert_eq!(FormElementKind::Page as u8, 9);
        assert_eq!(FormElementKind::CommandBar as u8, 10);
        assert_eq!(FormElementKind::ButtonGroup as u8, 11);

        assert!(FormElementKind::Table < FormElementKind::Group);
        assert!(FormElementKind::Group < FormElementKind::Field);
        assert!(FormElementKind::Field < FormElementKind::Button);
        assert!(FormElementKind::Button < FormElementKind::Decoration);
        assert!(FormElementKind::Decoration < FormElementKind::Addition);
        assert!(FormElementKind::Addition < FormElementKind::Other);
        assert!(FormElementKind::Other < FormElementKind::UsualGroup);
        assert!(FormElementKind::UsualGroup < FormElementKind::Pages);
        assert!(FormElementKind::Pages < FormElementKind::Page);
        assert!(FormElementKind::Page < FormElementKind::CommandBar);
        assert!(FormElementKind::CommandBar < FormElementKind::ButtonGroup);
    }

    #[test]
    fn test_form_with_elements() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        let elements = vec![
            FormElement::new("Код", 1, Some("Объект.Code".to_string())),
            FormElement::new(
                "НесуществующийРеквизит",
                2,
                Some("~Объект.НесуществующийРеквизит".to_string()),
            ),
        ];

        let form =
            Form::with_elements("ФормаЭлемента".to_string(), FormType::Managed, uuid, elements);

        assert_eq!(form.elements().len(), 2);
        let wrong: Vec<_> = form.elements_with_wrong_data_path().collect();
        assert_eq!(wrong.len(), 1);
        assert_eq!(wrong[0].name, "НесуществующийРеквизит");
    }

    #[test]
    fn test_find_element_case_insensitive() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        let elements = vec![
            FormElement {
                name: "Переприемка".to_string(),
                id: 255,
                data_path: Some("Объект.Переприемка".to_string()),
                kind: FormElementKind::Table,
                parent_id: None,
            },
            FormElement {
                name: "ШтрихКод".to_string(),
                id: 256,
                data_path: Some("Объект.Переприемка.ШтрихКод".to_string()),
                kind: FormElementKind::Field,
                parent_id: Some(255),
            },
        ];
        let form = Form::with_elements("Ф".to_string(), FormType::Managed, uuid, elements);

        assert_eq!(form.find_element("Переприемка").map(|e| e.id), Some(255));
        assert_eq!(form.find_element("переприемка").map(|e| e.id), Some(255));
        assert_eq!(form.find_element("ПЕРЕПРИЕМКА").map(|e| e.id), Some(255));
        assert!(form.find_element("Несуществующий").is_none());
    }

    #[test]
    fn test_children_of_returns_immediate_children() {
        let uuid = Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap();
        let elements = vec![
            FormElement {
                name: "ГруппаОбщая".to_string(),
                id: 100,
                data_path: None,
                kind: FormElementKind::Group,
                parent_id: None,
            },
            FormElement {
                name: "ПолеВГруппе".to_string(),
                id: 101,
                data_path: Some("Объект.Code".to_string()),
                kind: FormElementKind::Field,
                parent_id: Some(100),
            },
            FormElement {
                name: "КнопкаТопЛевел".to_string(),
                id: 200,
                data_path: None,
                kind: FormElementKind::Button,
                parent_id: None,
            },
        ];
        let form = Form::with_elements("Ф".to_string(), FormType::Managed, uuid, elements);

        let group_kids: Vec<_> = form.children_of(100).map(|e| e.id).collect();
        assert_eq!(group_kids, vec![101]);

        assert!(form.children_of(200).next().is_none());
    }

    fn borrowed_form_base() -> Form {
        let uuid = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let mut form = Form::with_handlers(
            "ФормаДокумента".to_string(),
            FormType::Managed,
            uuid,
            vec![FormElement::new("БазовоеПоле", 1, Some("Объект.Номер".to_string()))],
            vec![FormEventHandler {
                event_type: "OnOpen".to_string(),
                handler_name: "БазовыйПриОткрытии".to_string(),
            }],
            vec!["БазоваяКоманда".to_string()],
        );
        let mut main = FormAttribute::new(
            "Объект",
            AttributeType::Ref {
                mdo_type: crate::MdoType::Document, name: "Заказ".to_string()
            },
        );
        main.is_main = true;
        main.columns = vec![FormAttributeColumn {
            name: "Колонка".to_string(),
            attr_type: AttributeType::Boolean,
        }];
        form.attributes = vec![
            main,
            FormAttribute::new("СчЁтчик", AttributeType::Number { precision: 10, scale: 0 }),
            FormAttribute::new("БазовыйРеквизитФормы", AttributeType::String { length: None }),
        ];
        form
    }

    fn borrowed_form_overlay() -> Form {
        let uuid = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        Form::with_handlers(
            "ФормаДокумента".to_string(),
            FormType::Managed,
            uuid,
            vec![FormElement::new("ПолеРасширения", 7, None)],
            vec![FormEventHandler {
                event_type: "OnOpen".to_string(),
                handler_name: "Расш_ПриОткрытии".to_string(),
            }],
            vec!["КомандаРасширения".to_string()],
        )
    }

    #[test]
    fn borrowed_form_empty_overlay_inherits_every_base_attribute() {
        let base = borrowed_form_base();
        let mut merged = base.clone();
        merged.apply_extension_overlay(&borrowed_form_overlay());

        assert_eq!(merged.attributes, base.attributes);
        let main = merged.main_attribute().expect("основной реквизит унаследован");
        assert_eq!(main.name, "Объект");
        assert!(main.is_main);
        assert_eq!(main.columns.len(), 1);
    }

    #[test]
    fn borrowed_form_overlay_attribute_replaces_unicode_case_twin_whole() {
        let mut overlay = borrowed_form_overlay();
        overlay.attributes = vec![
            FormAttribute::new("счётчик", AttributeType::String { length: Some(5) }),
            FormAttribute::new("РасшРеквизитФормы", AttributeType::Date),
        ];
        let mut merged = borrowed_form_base();
        merged.apply_extension_overlay(&overlay);

        let names: Vec<&str> = merged.attribute_names().collect();
        assert_eq!(names, ["Объект", "счётчик", "БазовыйРеквизитФормы", "РасшРеквизитФормы"]);
        let twins: Vec<_> = merged
            .attributes
            .iter()
            .filter(|a| stdx::case::eq_ignore_case(&a.name, "СЧЁТЧИК"))
            .collect();
        assert_eq!(twins.len(), 1, "конфликт даёт единственный реквизит");
        assert_eq!(twins[0], &overlay.attributes[0]);
    }

    #[test]
    fn borrowed_form_overlay_main_attribute_wins_with_its_own_flag_and_columns() {
        let mut overlay = borrowed_form_overlay();
        overlay.attributes =
            vec![FormAttribute::new("ОБЪЕКТ", AttributeType::String { length: None })];
        let mut merged = borrowed_form_base();
        merged.apply_extension_overlay(&overlay);

        let object = merged.find_attribute("Объект").expect("реквизит на месте");
        assert_eq!(object, &overlay.attributes[0]);
        assert!(!object.is_main);
        assert!(object.columns.is_empty());
        assert!(merged.main_attribute().is_none());

        overlay.attributes[0].is_main = true;
        overlay.attributes[0].columns = vec![FormAttributeColumn {
            name: "КолонкаРасширения".to_string(),
            attr_type: AttributeType::Date,
        }];
        let mut merged = borrowed_form_base();
        merged.apply_extension_overlay(&overlay);
        assert_eq!(merged.main_attribute(), Some(&overlay.attributes[0]));
        assert_eq!(merged.find_attribute("объект"), Some(&overlay.attributes[0]));
    }

    #[test]
    fn borrowed_form_dialog_comes_from_overlay_and_sources_stay_intact() {
        let base = borrowed_form_base();
        let mut overlay = borrowed_form_overlay();
        overlay.attributes = vec![FormAttribute::new("РасшРеквизитФормы", AttributeType::Date)];
        let base_before = base.clone();
        let overlay_before = overlay.clone();

        let mut merged = base.clone();
        merged.apply_extension_overlay(&overlay);

        let mut expected = overlay.clone();
        expected.attributes = base.attributes.iter().chain(&overlay.attributes).cloned().collect();
        assert_eq!(merged, expected);
        assert!(merged.find_element("БазовоеПоле").is_none());
        assert!(!merged.is_handler("БазовыйПриОткрытии"));
        assert!(!merged.is_handler("БазоваяКоманда"));
        assert_eq!(base, base_before);
        assert_eq!(overlay, overlay_before);
    }
}
