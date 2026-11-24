//! The optional description of an ORDINARY form's dialog - `Ext/Form/Form.json`.
//!
//! An ordinary (non-managed) form has no `Ext/Form.xml`: its dialog lives in the
//! binary `Ext/Form.bin`, and the configurator export writes nothing readable next
//! to it. A tool that can read that container may describe the dialog in a JSON
//! file next to the extracted module: the named controls, the form attributes and
//! the procedures the form binds to events. The format is documented in
//! `docs/configuration/ordinary-form-sidecar.md`; this module is its only reader.
//!
//! The reader takes only what the analyzer uses and ignores every other key, so a
//! producer may add fields freely. The format only grows: a newer `schema` adds
//! fields and never changes the meaning of one the reader uses (a breaking change
//! needs a different file name), so any valid version is read. What the reader will
//! not do is guess: an invalid version, an unparsable file, or one older than
//! `Form.bin` is left unread, and the form falls back to what its descriptor alone
//! says (see [`Form::ordinary_without_dialog`]). A fingerprinted sidecar must match
//! the container's bytes. Legacy sidecars retain their modification-time check.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use stdx::case::CaseExt;

use crate::enums::FormType;
use crate::error::{MetadataError, Result};
use crate::form::{Form, FormAttribute, FormElement, FormElementKind, FormEventHandler};
use crate::metadata_object::{AttributeType, MdoType};

/// The `event_type` given to a `boundHandlers` entry no event map names: the
/// sidecar says the procedure is bound, not to which event.
pub const BOUND_HANDLER_EVENT: &str = "Привязка";

/// Why a sidecar next to an ordinary form module was left unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarRejection {
    /// The file could not be read.
    Unreadable(String),
    /// The text is not JSON of the expected shape.
    Malformed(String),
    /// `schema` is not an integer from 1 (as written in the file).
    InvalidSchema(String),
    /// `Ext/Form.bin` was modified after the sidecar was written.
    Stale,
    Fingerprint(String),
}

impl std::fmt::Display for SidecarRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarRejection::Unreadable(e) => write!(f, "cannot read the file: {e}"),
            SidecarRejection::Malformed(e) => write!(f, "invalid JSON: {e}"),
            SidecarRejection::InvalidSchema(v) => {
                write!(f, "schema {v} is not a version (an integer from 1)")
            }
            SidecarRejection::Stale => write!(f, "Form.bin is newer than the sidecar"),
            SidecarRejection::Fingerprint(reason) => write!(f, "source fingerprint: {reason}"),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Sidecar {
    /// Version of the format; absent = 1. Kept raw so that an invalid value is
    /// reported as such rather than as malformed JSON.
    #[serde(deserialize_with = "deserialize_schema")]
    schema: Option<serde_json::Value>,
    form: SidecarForm,
    attributes: Vec<SidecarAttribute>,
    controls: Vec<SidecarControl>,
    #[serde(rename = "inspectionLimitations")]
    inspection_limitations: Vec<String>,
}

// A present null is an invalid version, not the absent version's default.
fn deserialize_schema<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SidecarForm {
    name: String,
    /// Events of the form itself and of its extension, event -> procedure. `None`
    /// in version 1 means the version predates the key, not that nothing is bound.
    handlers: Option<BTreeMap<String, String>>,
    /// Every procedure the form binds anywhere (version 2+): a superset of the
    /// event maps that also covers command-bar and menu buttons, which have no
    /// name in the dialog and so are not controls.
    #[serde(rename = "boundHandlers")]
    bound_handlers: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SidecarAttribute {
    name: String,
    /// The readable 1C type (`ОтчетОбъект.ОтчетДиаграмма`), schema 14+; absent before.
    #[serde(rename = "type")]
    type_name: Option<String>,
    /// Set on the form's main attribute only, schema 14+.
    main: bool,
    /// The type already structured by the producer, where it emits one. Preferred
    /// over `type_name`: it needs no mapping, so it cannot lose a case the mapping
    /// does not know.
    attr_type: Option<AttributeType>,
    /// The producer's own spelling of `main`. Either spelling marks the main attribute.
    is_main: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SidecarControl {
    kind: String,
    id: u32,
    name: Option<String>,
    #[serde(rename = "dataPath")]
    data_path: Option<String>,
    handlers: Option<BTreeMap<String, String>>,
    children: Vec<SidecarControl>,
    columns: Vec<SidecarColumn>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SidecarColumn {
    handlers: Option<BTreeMap<String, String>>,
}

/// Reads the sidecar at `sidecar_path` for the ordinary form `form_name`, unless
/// `Ext/Form.bin` (the parent of the sidecar's directory) is newer than it.
pub fn read_ordinary_form_sidecar(
    sidecar_path: &Path,
    form_name: &str,
) -> std::result::Result<Form, SidecarRejection> {
    let json = std::fs::read_to_string(sidecar_path)
        .map_err(|e| SidecarRejection::Unreadable(e.to_string()))?;
    let source: serde_json::Value = serde_json::from_str(json.trim_start_matches('\u{feff}'))
        .map_err(|e| SidecarRejection::Malformed(e.to_string()))?;
    if source.pointer("/source/sha256").is_some() {
        let binary = sidecar_path
            .parent()
            .and_then(Path::parent)
            .and_then(|ext| {
                bsl_conventions::find_child_ci(
                    ext,
                    bsl_conventions::ConventionalName::FormBin.canonical(),
                )
            })
            .ok_or_else(|| SidecarRejection::Fingerprint("Form.bin unavailable".into()))?;
        let fingerprint = crate::inspection_sources::binary_fingerprint(&binary)
            .map_err(|e| SidecarRejection::Fingerprint(e.to_string()))?;
        super::form_inspection::validate_sidecar_fingerprint(&source, Some(&fingerprint.sha256))
            .map_err(|e| SidecarRejection::Fingerprint(e.to_string()))?;
    } else if is_stale(sidecar_path) {
        return Err(SidecarRejection::Stale);
    }
    parse_sidecar(&json, form_name)
}

/// Whether the dialog the sidecar describes changed after the sidecar was written.
/// A missing `Form.bin` or an unavailable modification time proves nothing, and
/// equal times count as fresh: a producer writes both files in one pass.
fn is_stale(sidecar_path: &Path) -> bool {
    let Some(ext_dir) = sidecar_path.parent().and_then(Path::parent) else { return false };
    let Some(form_bin) = bsl_conventions::find_child_ci(
        ext_dir,
        bsl_conventions::ConventionalName::FormBin.canonical(),
    ) else {
        return false;
    };
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    match (modified(&form_bin), modified(sidecar_path)) {
        (Some(dialog), Some(sidecar)) => dialog > sidecar,
        _ => false,
    }
}

/// Parses the sidecar text into an ordinary [`Form`]. `form_name` is the name
/// from the path (`Forms/<Name>/...`), used when the file carries none.
pub fn parse_ordinary_form_sidecar(json: &str, form_name: &str) -> Result<Form> {
    parse_sidecar(json, form_name)
        .map_err(|e| MetadataError::InvalidFormat(format!("form sidecar: {e}")))
}

fn parse_sidecar(json: &str, form_name: &str) -> std::result::Result<Form, SidecarRejection> {
    let _span = tracing::debug_span!("parse_ordinary_form_sidecar").entered();

    let sidecar: Sidecar = serde_json::from_str(json.trim_start_matches('\u{feff}'))
        .map_err(|e| SidecarRejection::Malformed(e.to_string()))?;

    let schema = match &sidecar.schema {
        None => 1,
        Some(value) => match value.as_u64() {
            Some(version) if version >= 1 => version,
            _ => return Err(SidecarRejection::InvalidSchema(value.to_string())),
        },
    };

    let name = if sidecar.form.name.is_empty() { form_name.to_string() } else { sidecar.form.name };
    let mut form = Form::new(name, FormType::Ordinary, uuid::Uuid::nil());
    form.ordinary_dialog_known = true;
    form.ordinary_bindings_known =
        !sidecar.inspection_limitations.iter().any(|reason| reason == "event_bindings_unread");

    form.attributes = sidecar
        .attributes
        .into_iter()
        .filter(|attr| !attr.name.is_empty())
        .map(|attr| {
            let attr_type = attr
                .attr_type
                .or_else(|| attr.type_name.as_deref().and_then(sidecar_type_of))
                .unwrap_or(AttributeType::Unknown);
            let mut attribute = FormAttribute::new(attr.name, attr_type);
            attribute.is_main = attr.is_main || attr.main;
            attribute
        })
        .collect();

    let mut event_handlers = match sidecar.form.handlers {
        Some(handlers) => handlers_of(handlers),
        // Version 1 never wrote the form's own events: the configurator's default
        // handler names stand in for them, as for a form without a sidecar.
        None if schema < 2 => crate::form::ordinary_form_default_handlers(),
        None => Vec::new(),
    };

    let mut elements = Vec::new();
    for control in sidecar.controls {
        collect_control(control, None, &mut elements, &mut event_handlers);
    }

    // The flat list repeats what the maps name; keep each procedure once.
    let mut known: rustc_hash::FxHashSet<String> =
        event_handlers.iter().map(|h| h.handler_name.fold_lower()).collect();
    for handler_name in sidecar.form.bound_handlers {
        let handler_name = handler_name.trim().to_string();
        if handler_name.is_empty() || !known.insert(handler_name.fold_lower()) {
            continue;
        }
        event_handlers
            .push(FormEventHandler { event_type: BOUND_HANDLER_EVENT.to_string(), handler_name });
    }

    form.elements = elements;
    form.event_handlers = event_handlers;

    tracing::debug!(
        form_name = %form.name(),
        elements_count = form.elements().len(),
        event_handlers_count = form.event_handlers().len(),
        attributes_count = form.attributes().len(),
        "parsed ordinary form sidecar"
    );

    Ok(form)
}

/// Russian spellings of the object types the sidecar's `type` may carry, with the
/// metadata kind that owns each. Only OBJECT types are read: a reference type
/// (`СправочникСсылка.X`) would map to the same `AttributeType::Ref` as an object
/// type, so reading it would pass a reference off as the object itself.
const OBJECT_TYPE_PREFIXES: &[(&str, MdoType)] = &[
    ("СправочникОбъект", MdoType::Catalog),
    ("ДокументОбъект", MdoType::Document),
    ("ОтчетОбъект", MdoType::Report),
    ("ОбработкаОбъект", MdoType::DataProcessor),
    ("ЗадачаОбъект", MdoType::Task),
    ("БизнесПроцессОбъект", MdoType::BusinessProcess),
    ("ПланОбменаОбъект", MdoType::ExchangePlan),
    ("ПланСчетовОбъект", MdoType::ChartOfAccounts),
    ("ПланВидовХарактеристикОбъект", MdoType::ChartOfCharacteristicTypes),
    ("ПланВидовРасчетаОбъект", MdoType::ChartOfCalculationTypes),
    ("ВнешняяОбработкаОбъект", MdoType::ExternalDataProcessor),
    ("ВнешнийОтчетОбъект", MdoType::ExternalReport),
];

/// `<Вид>Объект.<Имя>` as `AttributeType::Ref`; any other spelling is `None`, so the
/// attribute keeps `Unknown` rather than a guess.
fn object_type_of(type_name: &str) -> Option<AttributeType> {
    let (prefix, name) = type_name.trim().split_once('.')?;
    if name.is_empty() || name.contains('.') {
        return None;
    }
    let (_, mdo_type) =
        OBJECT_TYPE_PREFIXES.iter().find(|(known, _)| stdx::case::eq_ignore_case(known, prefix))?;
    Some(AttributeType::Ref { mdo_type: *mdo_type, name: name.to_string() })
}

/// A sidecar `type` as the attribute type this reader can name: an object type, a
/// register's record manager or record set, or a platform LIST type. Anything else is
/// `None`, so the attribute keeps `Unknown` rather than a guess.
fn sidecar_type_of(type_name: &str) -> Option<AttributeType> {
    object_type_of(type_name)
        .or_else(|| register_type_of(type_name))
        .or_else(|| list_type_of(type_name))
}

/// `<Регистр>МенеджерЗаписи.<Имя>` and `<Регистр>НаборЗаписей.<Имя>` in either notation.
/// The heads are built from the metadata kind's own names, so no spelling is listed here;
/// a register has one `Ref` shape (no object/reference split), which is its record set.
fn register_type_of(type_name: &str) -> Option<AttributeType> {
    let (prefix, name) = split_type_name(type_name)?;
    MdoType::all().iter().filter(|mdo| mdo.is_register()).find_map(|mdo| {
        let is = |ru: &str, en: &str| {
            stdx::case::eq_ignore_case(&format!("{}{ru}", mdo.russian_name()), prefix)
                || stdx::case::eq_ignore_case(&format!("{}{en}", mdo.english_name()), prefix)
        };
        if is("НаборЗаписей", "RecordSet") {
            Some(AttributeType::Ref { mdo_type: *mdo, name: name.to_string() })
        } else if *mdo == MdoType::InformationRegister && is("МенеджерЗаписи", "RecordManager")
        {
            Some(AttributeType::InformationRegisterRecordManager { name: name.to_string() })
        } else {
            None
        }
    })
}

/// `<Вид>Список.<Имя>` in either notation, as `PlatformNamed("<русское имя>.<Имя>")`.
fn list_type_of(type_name: &str) -> Option<AttributeType> {
    let (prefix, name) = split_type_name(type_name)?;
    let (ru, _) = platform_list_heads(prefix)?;
    Some(AttributeType::PlatformNamed(format!("{ru}.{name}")))
}

fn split_type_name(type_name: &str) -> Option<(&str, &str)> {
    let (prefix, name) = type_name.trim().split_once('.')?;
    (!name.is_empty() && !name.contains('.')).then_some((prefix, name))
}

/// The Russian and English heads (`СправочникСписок`, `CatalogList`) of the platform
/// LIST type `spelling` names in either notation. The catalog is the only source: its
/// templated types (`СправочникСписок.<Имя справочника>`) whose Russian head ends in
/// `Список` and English head in `List`. `None` for any other head.
pub fn platform_list_heads(spelling: &str) -> Option<(&'static str, &'static str)> {
    static HEADS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    let heads = HEADS.get_or_init(|| {
        let mut found: Vec<(String, String)> = Vec::new();
        for ty in bsl_platform::PlatformDataInner::instance().all_types() {
            let (Some((ru, _)), Some((en, _))) =
                (ty.name.split_once('.'), ty.english_name.split_once('.'))
            else {
                continue;
            };
            if ru.ends_with("Список") && en.ends_with("List") && !found.iter().any(|(r, _)| r == ru)
            {
                found.push((ru.to_string(), en.to_string()));
            }
        }
        found
    });
    heads
        .iter()
        .find(|(ru, en)| {
            stdx::case::eq_ignore_case(ru, spelling) || stdx::case::eq_ignore_case(en, spelling)
        })
        .map(|(ru, en)| (ru.as_str(), en.as_str()))
}

fn handlers_of(handlers: BTreeMap<String, String>) -> Vec<FormEventHandler> {
    handlers
        .into_iter()
        .filter(|(_, handler)| !handler.trim().is_empty())
        .map(|(event_type, handler_name)| FormEventHandler {
            event_type,
            handler_name: handler_name.trim().to_string(),
        })
        .collect()
}

fn collect_control(
    control: SidecarControl,
    parent_id: Option<u32>,
    elements: &mut Vec<FormElement>,
    event_handlers: &mut Vec<FormEventHandler>,
) {
    let id = control.id;
    let name = control.name.filter(|name| !name.is_empty());
    let named = name.is_some();
    if let Some(name) = name {
        elements.push(FormElement::with_kind(
            name,
            id,
            control.data_path,
            kind_of(&control.kind),
            parent_id,
        ));
    }
    if let Some(handlers) = control.handlers {
        event_handlers.extend(handlers_of(handlers));
    }
    for column in control.columns {
        if let Some(handlers) = column.handlers {
            event_handlers.extend(handlers_of(handlers));
        }
    }
    // A nameless container (the dialog's root node) is transparent for the parent
    // chain: its children hang off whatever it hangs off.
    let child_parent = if named { Some(id) } else { parent_id };
    for child in control.children {
        collect_control(child, child_parent, elements, event_handlers);
    }
}

/// The sidecar's control kinds in the vocabulary of the managed-form element
/// kinds the rest of the analyzer speaks. Several ordinary controls have no exact
/// counterpart; the mapping picks the closest role, and an unknown kind is `Other`.
fn kind_of(kind: &str) -> FormElementKind {
    match kind {
        "list" => FormElementKind::Table,
        "input" | "choice" | "checkbox" | "radio" | "listbox" | "textDoc" | "spreadsheet"
        | "html" | "picture" | "calendar" | "indicator" | "chart" | "graphScheme" => {
            FormElementKind::Field
        }
        "button" => FormElementKind::Button,
        "label" | "separator" => FormElementKind::Decoration,
        "commandPanel" => FormElementKind::CommandBar,
        "panel" => FormElementKind::Pages,
        "group" => FormElementKind::Group,
        _ => FormElementKind::Other,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SIDECAR: &str = r#"{
 "schema": 2,
 "form": {
  "name": "ФормаСписка",
  "handlers": {"ПередОткрытием": "ПередОткрытием", "ОбновлениеОтображения": "ОбновитьОтображение"},
  "boundHandlers": ["ДействияФормыПодбор", "ПередОткрытием", "СписокПриАктивизацииСтроки", " ", "передоткрытием"]
 },
 "attributes": [
  {"id": 0, "name": "Список"},
  {"id": 4, "name": "ТекстПодсказки"}
 ],
 "controls": [
  {
   "kind": "list", "id": 1, "name": "Список", "dataPath": "Список",
   "handlers": {"ПриАктивизацииСтроки": "СписокПриАктивизацииСтроки"},
   "columns": [
    {"name": "Код", "handlers": {"ПриИзменении": "КодПриИзменении"}},
    {"name": "Наименование"}
   ]
  },
  {"kind": "commandPanel", "id": 3, "name": "ДействияФормы"},
  {
   "kind": "panel", "id": 7, "name": "Панель1",
   "children": [
    {"kind": "input", "id": 8, "name": "Поле1", "page": 0,
     "handlers": {"ПриИзменении": "Поле1ПриИзменении", "НачалоВыбора": ""}}
   ]
  },
  {"kind": "generic", "id": 0}
 ]
}"#;

    #[test]
    fn reads_form_element_and_column_handlers() {
        let form = parse_ordinary_form_sidecar(SIDECAR, "ИмяИзПути").unwrap();
        assert_eq!(form.name(), "ФормаСписка");
        assert!(form.is_ordinary());
        assert!(form.ordinary_dialog_known);

        assert!(form.is_handler("ПередОткрытием"));
        assert!(form.is_handler("ОбновитьОтображение"));
        assert!(form.is_handler("СписокПриАктивизацииСтроки"));
        assert!(form.is_handler("КодПриИзменении"));
        assert!(form.is_handler("Поле1ПриИзменении"));
        // The sidecar names the bindings: an event the form does not bind is no handler.
        assert!(!form.is_handler("ПриОткрытии"));
        assert!(form.event_handlers().iter().all(|h| !h.handler_name.trim().is_empty()));
    }

    #[test]
    fn bound_handlers_cover_nameless_buttons_once() {
        let form = parse_ordinary_form_sidecar(SIDECAR, "ФормаСписка").unwrap();
        assert!(form.is_handler("ДействияФормыПодбор"));
        let bound = form
            .event_handlers()
            .iter()
            .filter(|h| h.event_type == BOUND_HANDLER_EVENT)
            .map(|h| h.handler_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(bound, vec!["ДействияФормыПодбор"], "{bound:?}");
        let opens = form
            .event_handlers()
            .iter()
            .filter(|h| stdx::case::eq_ignore_case(&h.handler_name, "ПередОткрытием"))
            .count();
        assert_eq!(opens, 1);
    }

    #[test]
    fn preserves_declared_ordinary_attribute_types_and_legacy_unknowns() {
        let json = r#"{"attributes":[
            {"name":"ОбработкаОбъект","is_main":true,
             "attr_type":{"Ref":{"mdo_type":"ExternalDataProcessor","name":"Проба"}}},
            {"name":"БезТипа"}
        ]}"#;
        let form = parse_ordinary_form_sidecar(json, "Форма").unwrap();
        let main = form.find_attribute("ОбработкаОбъект").unwrap();
        assert!(main.is_main);
        assert_eq!(
            main.attr_type,
            AttributeType::Ref {
                mdo_type: crate::MdoType::ExternalDataProcessor,
                name: "Проба".into()
            }
        );
        let legacy = form.find_attribute("БезТипа").unwrap();
        assert!(!legacy.is_main);
        assert_eq!(legacy.attr_type, AttributeType::Unknown);
        let inspection = crate::xml_parser::form_inspection::parse_ordinary_form_inspection(
            json,
            "Форма",
            None,
            "base",
        )
        .unwrap();
        let attributes = inspection.attributes.unwrap();
        assert_eq!(attributes[0].attr_type.as_ref(), Some(&main.attr_type));
        assert_eq!(attributes[0].type_origin.as_deref(), Some("base"));
        assert_eq!(attributes[1].attr_type, None);
        assert!(parse_ordinary_form_sidecar(
            r#"{"attributes":[{"name":"Объект","attr_type":"InvalidType"}]}"#,
            "Форма"
        )
        .is_err());
    }

    #[test]
    fn reads_attributes_and_nested_elements() {
        let form = parse_ordinary_form_sidecar(SIDECAR, "ФормаСписка").unwrap();
        assert!(form.find_attribute("ТекстПодсказки").is_some());
        assert!(form.find_attribute("список").is_some());

        let list = form.find_element("Список").unwrap();
        assert_eq!(list.kind, FormElementKind::Table);
        assert_eq!(list.data_path.as_deref(), Some("Список"));
        assert_eq!(list.parent_id, None);

        let field = form.find_element("Поле1").unwrap();
        assert_eq!(field.kind, FormElementKind::Field);
        assert_eq!(field.parent_id, Some(7));
        assert_eq!(form.find_element("Панель1").unwrap().kind, FormElementKind::Pages);
        // Columns and the nameless root node are not controls.
        assert_eq!(form.elements().len(), 4);
    }

    /// Only an entry of `controls` that carries a name is a control: a table column and
    /// a command-bar button are named in the file but are not members of the form.
    #[test]
    fn columns_and_command_bar_buttons_are_not_controls() {
        let json = r#"{"controls": [
            {"kind": "list", "id": 1, "name": "Таблица", "columns": [{"name": "Колонка"}]},
            {"kind": "commandPanel", "id": 2, "name": "Панель",
             "buttons": [{"name": "Кнопка", "kind": "button"}]},
            {"kind": "input", "id": 3}
        ]}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        let names: Vec<&str> = form.elements().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Таблица", "Панель"]);
    }

    #[test]
    fn version_2_without_form_handlers_binds_no_form_event() {
        let json = r#"{"schema": 2, "form": {"name": "Ф"}, "controls": []}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        assert!(!form.is_handler("ПередОткрытием"));
    }

    #[test]
    fn version_1_falls_back_to_default_event_handler_names() {
        let json = r#"{"form": {"name": "Ф"}, "attributes": [], "controls": []}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        assert!(form.is_handler("ПередОткрытием"));
        assert!(form.is_handler("BeforeOpen"));
        assert!(!form.is_handler("ПриАктивизацииСтроки"));
    }

    #[test]
    fn invalid_versions_are_rejected() {
        for schema in ["0", "-1", "1.5", "\"2\"", "null", "true", "{}", "[]"] {
            let json = format!(r#"{{"schema": {schema}, "form": {{"name": "Ф"}}}}"#);
            assert_eq!(
                parse_sidecar(&json, "Ф").unwrap_err(),
                SidecarRejection::InvalidSchema(schema.to_string()),
                "{schema}"
            );
        }
    }

    #[test]
    fn a_future_version_is_read_as_the_fields_it_shares() {
        let json = r#"{"schema": 99, "newKey": {"x": 1},
            "form": {"name": "Ф", "handlers": {}, "boundHandlers": ["Ф1Нажатие"]},
            "controls": [{"kind": "button", "id": 1, "name": "Кнопка1"}]}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        assert!(form.find_element("Кнопка1").is_some());
        assert!(form.is_handler("Ф1Нажатие"));
        // Version 2+ semantics: the form binds no event of its own.
        assert!(!form.is_handler("ПередОткрытием"));
    }

    #[test]
    fn a_bom_and_unknown_keys_are_tolerated_and_the_path_name_fills_in() {
        let json = "\u{feff}{\"whatever\": 1, \"controls\": [{\"kind\": \"weird\", \"id\": 1, \"name\": \"Э\", \"extra\": true}]}";
        let form = parse_ordinary_form_sidecar(json, "ИзПути").unwrap();
        assert_eq!(form.name(), "ИзПути");
        assert_eq!(form.find_element("Э").unwrap().kind, FormElementKind::Other);
    }

    #[test]
    fn a_typed_main_attribute_is_read_and_everything_else_stays_unknown() {
        let json = r#"{"schema": 14, "attributes": [
            {"id": 0, "name": "ОбщийОтчет", "type": "ОтчетОбъект.ОтчетДиаграмма", "main": true},
            {"id": 1, "name": "А", "type": "СправочникСсылка.Валюты"},
            {"id": 2, "name": "Б", "type": "Строка(50)"},
            {"id": 3, "name": "В", "type": "Произвольный"},
            {"id": 4, "name": "Г", "type": "ОтчетОбъект.A.B"},
            {"id": 5, "name": "Д"}
        ]}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        let main = form.main_attribute().unwrap();
        assert_eq!(main.name, "ОбщийОтчет");
        assert_eq!(
            main.attr_type,
            AttributeType::Ref {
                mdo_type: MdoType::Report, name: "ОтчетДиаграмма".to_string()
            }
        );
        for name in ["А", "Б", "В", "Г", "Д"] {
            let attr = form.find_attribute(name).unwrap();
            assert_eq!(attr.attr_type, AttributeType::Unknown, "{name}");
            assert!(!attr.is_main, "{name}");
        }
    }

    #[test]
    fn a_sidecar_without_type_and_main_has_no_main_attribute() {
        let json = r#"{"schema": 13, "attributes": [{"id": 0, "name": "ОбщийОтчет"}]}"#;
        let form = parse_ordinary_form_sidecar(json, "Ф").unwrap();
        assert!(form.main_attribute().is_none());
        assert_eq!(form.find_attribute("ОбщийОтчет").unwrap().attr_type, AttributeType::Unknown);
    }

    fn main_type_of(type_name: &str) -> AttributeType {
        let json = format!(
            r#"{{"schema": 14, "attributes": [{{"id": 0, "name": "А", "type": "{type_name}", "main": true}}]}}"#
        );
        parse_ordinary_form_sidecar(&json, "Ф").unwrap().main_attribute().unwrap().attr_type.clone()
    }

    /// Every list spelling the 3097-form corpus carries, in both notations, names the
    /// platform list type; the stored name is the Russian one whichever was written.
    #[test]
    fn a_list_type_is_read_in_both_notations() {
        let pairs = [
            ("СправочникСписок", "CatalogList"),
            ("ДокументСписок", "DocumentList"),
            ("РегистрСведенийСписок", "InformationRegisterList"),
            ("РегистрНакопленияСписок", "AccumulationRegisterList"),
            ("РегистрБухгалтерииСписок", "AccountingRegisterList"),
            ("РегистрРасчетаСписок", "CalculationRegisterList"),
            ("ПланВидовХарактеристикСписок", "ChartOfCharacteristicTypesList"),
            ("ПланСчетовСписок", "ChartOfAccountsList"),
            ("ПланВидовРасчетаСписок", "ChartOfCalculationTypesList"),
            ("ПланОбменаСписок", "ExchangePlanList"),
            ("БизнесПроцессСписок", "BusinessProcessList"),
            ("ЗадачаСписок", "TaskList"),
            ("ЖурналДокументовСписок", "DocumentJournalList"),
            ("КритерийОтбораСписок", "FilterCriterionList"),
            ("ПеречислениеСписок", "EnumList"),
        ];
        for (ru, en) in pairs {
            let expected = AttributeType::PlatformNamed(format!("{ru}.Имя"));
            assert_eq!(main_type_of(&format!("{ru}.Имя")), expected, "{ru}");
            assert_eq!(main_type_of(&format!("{en}.Имя")), expected, "{en}");
            assert_eq!(platform_list_heads(en).map(|(ru, _)| ru), Some(ru));
            assert_eq!(platform_list_heads(ru).map(|(_, en)| en), Some(en));
        }
    }

    /// The control of the list reading: spellings that look alike but are not a list
    /// type keep `Unknown`.
    #[test]
    fn a_type_that_is_not_a_list_keeps_unknown() {
        for spelling in [
            "СправочникМенеджер.Имя",
            "СправочникСсылка.Имя",
            "СписокЗначений.Имя",
            "СправочникСписок",
            "СправочникСписок.",
            "СправочникСписок.А.Б",
            "НесуществующийСписок.Имя",
            "Список.Имя",
        ] {
            assert_eq!(main_type_of(spelling), AttributeType::Unknown, "{spelling}");
        }
    }

    #[test]
    fn a_register_record_set_and_record_manager_are_read_in_both_notations() {
        let set = |mdo_type| AttributeType::Ref { mdo_type, name: "Имя".to_string() };
        assert_eq!(
            main_type_of("РегистрСведенийНаборЗаписей.Имя"),
            set(MdoType::InformationRegister)
        );
        assert_eq!(
            main_type_of("InformationRegisterRecordSet.Имя"),
            set(MdoType::InformationRegister)
        );
        assert_eq!(
            main_type_of("РегистрНакопленияНаборЗаписей.Имя"),
            set(MdoType::AccumulationRegister)
        );
        assert_eq!(
            main_type_of("CalculationRegisterRecordSet.Имя"),
            set(MdoType::CalculationRegister)
        );
        let manager =
            AttributeType::InformationRegisterRecordManager { name: "Имя".to_string() };
        assert_eq!(main_type_of("РегистрСведенийМенеджерЗаписи.Имя"), manager);
        assert_eq!(main_type_of("InformationRegisterRecordManager.Имя"), manager);
        // Only an information register has a record manager.
        assert_eq!(main_type_of("РегистрНакопленияМенеджерЗаписи.Имя"), AttributeType::Unknown);
        assert_eq!(main_type_of("РегистрСведенийМенеджер.Имя"), AttributeType::Unknown);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(parse_sidecar("{not json", "Ф"), Err(SidecarRejection::Malformed(_))));
    }
}
