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
use crate::metadata_object::AttributeType;

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
    attr_type: Option<AttributeType>,
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
            let mut attribute =
                FormAttribute::new(attr.name, attr.attr_type.unwrap_or(AttributeType::Unknown));
            attribute.is_main = attr.is_main;
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
    fn malformed_json_is_rejected() {
        assert!(matches!(parse_sidecar("{not json", "Ф"), Err(SidecarRejection::Malformed(_))));
    }
}
