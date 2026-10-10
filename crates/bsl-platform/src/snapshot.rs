//! Owned platform help snapshot and the runtime decoder of the help corpus JSON.
//!
//! A snapshot carries the help records in the shape they have after the curated
//! JSON overlays (`data/platform_overlays.json`) and before the code-level
//! docs-gap overlays, which [`crate::PlatformData`] applies exactly once while it
//! builds its indices. Every source — a corpus JSON or package on disk, an
//! installed platform, a downloaded package — ends up here, so lookups never
//! know where the data came from.

use std::fmt;

use rustc_hash::FxHashSet;
use serde_json::{Map, Value};
use smol_str::SmolStr;

use crate::types::{
    CodeExample, ConstructorDocs, ContextAvailability, GlobalFunction, GlobalFunctionVariant,
    MethodDocs, MethodParam, MethodVariant, ParamDocs, PlatformConstructor, PlatformMethod,
    PlatformProperty, PlatformType, PropertyDocs,
};
use crate::GLOBAL_CONTEXT_OWNER;

/// The curated overlay the analyzer applies to every corpus it loads.
const CURATED_OVERLAYS: &str = include_str!("../data/platform_overlays.json");

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformSnapshot {
    pub types: Vec<PlatformType>,
    pub methods: Vec<PlatformMethod>,
    pub global_functions: Vec<GlobalFunction>,
    pub constructors: Vec<PlatformConstructor>,
    pub properties: Vec<PlatformProperty>,
    pub method_docs: Vec<MethodDocs>,
    pub global_function_docs: Vec<MethodDocs>,
    pub constructor_docs: Vec<ConstructorDocs>,
    pub property_docs: Vec<PropertyDocs>,
}

/// Why a corpus could not become a snapshot. Nothing is published on error: the
/// caller decides between a fallback and an empty snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotDecodeError(pub String);

impl fmt::Display for SnapshotDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SnapshotDecodeError {}

impl PlatformSnapshot {
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
            && self.methods.is_empty()
            && self.global_functions.is_empty()
            && self.constructors.is_empty()
            && self.properties.is_empty()
    }

    /// Decodes the help corpus JSON (the `html-parser` output) and applies the
    /// curated overlays to it.
    pub fn from_corpus_json(bytes: &[u8]) -> Result<Self, SnapshotDecodeError> {
        Self::from_corpus_json_with_overlays(bytes, CURATED_OVERLAYS)
    }

    /// [`Self::from_corpus_json`] with an explicit overlay document. A corpus the
    /// overlay cannot apply to is rejected as a whole.
    pub fn from_corpus_json_with_overlays(
        bytes: &[u8],
        overlays: &str,
    ) -> Result<Self, SnapshotDecodeError> {
        let mut data: Value = serde_json::from_slice(bytes)
            .map_err(|error| SnapshotDecodeError(format!("malformed help corpus JSON: {error}")))?;
        let root = data
            .as_object()
            .ok_or_else(|| SnapshotDecodeError("help corpus root must be an object".to_owned()))?;
        for section in CORPUS_SECTIONS {
            match root.get(*section) {
                None | Some(Value::Array(_)) => {}
                Some(_) => {
                    return Err(SnapshotDecodeError(format!(
                        "help corpus section `{section}` must be an array"
                    )))
                }
            }
        }

        validate_shape(&data)?;
        // The extractor omits empty sections; an absent section is an empty one,
        // and the overlays expect every section they may extend to be present.
        let root = data.as_object_mut().expect("checked to be an object");
        for section in CORPUS_SECTIONS {
            root.entry(*section).or_insert_with(|| Value::Array(Vec::new()));
        }

        let overlay_error = |error: crate::overlays::OverlayError| {
            SnapshotDecodeError(format!("platform overlay: {error}"))
        };
        crate::overlays::apply_method_parameter_overlays(&mut data, overlays)
            .map_err(overlay_error)?;
        crate::overlays::apply_global_function_parameter_overlays(&mut data, overlays)
            .map_err(overlay_error)?;
        crate::overlays::apply_type_property_additions(&mut data, overlays)
            .map_err(overlay_error)?;

        let snapshot = decode(&data)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Record ids are the only links between a record and its docs; duplicates or
    /// records attached to an unknown type would make lookups answer for the wrong
    /// entry, so such a corpus is refused.
    fn validate(&self) -> Result<(), SnapshotDecodeError> {
        fn unique(kind: &str, ids: impl Iterator<Item = u32>) -> Result<(), SnapshotDecodeError> {
            let mut seen = FxHashSet::default();
            for id in ids {
                if !seen.insert(id) {
                    return Err(SnapshotDecodeError(format!("duplicate {kind} id {id}")));
                }
            }
            Ok(())
        }
        unique("method", self.methods.iter().map(|m| m.id))?;
        unique("global function", self.global_functions.iter().map(|f| f.id))?;
        unique("constructor", self.constructors.iter().map(|c| c.id))?;
        unique("property", self.properties.iter().map(|p| p.id))?;

        let known_types: FxHashSet<&str> =
            self.types.iter().map(|ty| ty.english_name.as_str()).collect();
        let owner_known =
            |owner: &str| known_types.contains(owner) || owner == GLOBAL_CONTEXT_OWNER;
        for method in &self.methods {
            if !owner_known(&method.type_name) {
                return Err(SnapshotDecodeError(format!(
                    "method {} belongs to unknown type {}",
                    method.id, method.type_name
                )));
            }
        }
        for ctor in &self.constructors {
            if !owner_known(&ctor.type_name) {
                return Err(SnapshotDecodeError(format!(
                    "constructor {} belongs to unknown type {}",
                    ctor.id, ctor.type_name
                )));
            }
        }
        for prop in &self.properties {
            if !owner_known(&prop.type_name) {
                return Err(SnapshotDecodeError(format!(
                    "property {} belongs to unknown type {}",
                    prop.id, prop.type_name
                )));
            }
        }

        let ids = |kind: &str, ids: FxHashSet<u32>, docs: &mut dyn Iterator<Item = u32>| {
            for id in docs {
                if !ids.contains(&id) {
                    return Err(SnapshotDecodeError(format!(
                        "{kind} docs refer to unknown id {id}"
                    )));
                }
            }
            Ok(())
        };
        ids(
            "method",
            self.methods.iter().map(|m| m.id).collect(),
            &mut self.method_docs.iter().map(|d| d.method_id),
        )?;
        ids(
            "global function",
            self.global_functions.iter().map(|f| f.id).collect(),
            &mut self.global_function_docs.iter().map(|d| d.method_id),
        )?;
        ids(
            "constructor",
            self.constructors.iter().map(|c| c.id).collect(),
            &mut self.constructor_docs.iter().map(|d| d.constructor_id),
        )?;
        ids(
            "property",
            self.properties.iter().map(|p| p.id).collect(),
            &mut self.property_docs.iter().map(|d| d.property_id),
        )
    }
}

/// Field kinds of corpus records. Absent optional fields take the generator's
/// defaults; a present field of the wrong kind, or a missing name, is corruption
/// that would otherwise turn silently into an empty value.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    Strings,
    Params,
    Variants,
    Context,
    Docs,
}

const TYPE_FIELDS: &[(&str, Kind, bool)] = &[
    ("name", Kind::Str, true),
    ("english_name", Kind::Str, true),
    ("min_version", Kind::Str, false),
    ("context", Kind::Context, false),
    ("iter_element_types", Kind::Strings, false),
    ("xdto_name", Kind::Str, false),
];
const METHOD_FIELDS: &[(&str, Kind, bool)] = &[
    ("type_name", Kind::Str, true),
    ("name", Kind::Str, true),
    ("english_name", Kind::Str, true),
    ("return_type", Kind::Str, false),
    ("parameters", Kind::Params, false),
    ("variants", Kind::Variants, false),
    ("min_version", Kind::Str, false),
    ("context", Kind::Context, false),
    ("documentation", Kind::Docs, false),
];
const FUNCTION_FIELDS: &[(&str, Kind, bool)] = &[
    ("name", Kind::Str, true),
    ("english_name", Kind::Str, true),
    ("return_type", Kind::Str, false),
    ("parameters", Kind::Params, false),
    ("variants", Kind::Variants, false),
    ("min_version", Kind::Str, false),
    ("context", Kind::Context, false),
    ("documentation", Kind::Docs, false),
];
const CONSTRUCTOR_FIELDS: &[(&str, Kind, bool)] = &[
    ("type_name", Kind::Str, true),
    ("variant_name", Kind::Str, false),
    ("parameters", Kind::Params, false),
    ("min_version", Kind::Str, false),
    ("context", Kind::Context, false),
    ("documentation", Kind::Docs, false),
];
const PROPERTY_FIELDS: &[(&str, Kind, bool)] = &[
    ("type_name", Kind::Str, true),
    ("name", Kind::Str, true),
    ("english_name", Kind::Str, true),
    ("property_types", Kind::Strings, false),
    ("is_readonly", Kind::Bool, false),
    ("min_version", Kind::Str, false),
    ("context", Kind::Context, false),
    ("documentation", Kind::Docs, false),
];

fn validate_shape(data: &Value) -> Result<(), SnapshotDecodeError> {
    for (section, fields) in [
        ("types", TYPE_FIELDS),
        ("methods", METHOD_FIELDS),
        ("global_functions", FUNCTION_FIELDS),
        ("constructors", CONSTRUCTOR_FIELDS),
        ("properties", PROPERTY_FIELDS),
    ] {
        for (index, entry) in section_entries(data, section).enumerate() {
            let at = |field: &str| format!("{section}[{index}].{field}");
            let object = entry.as_object().ok_or_else(|| {
                SnapshotDecodeError(format!("{section}[{index}] must be an object"))
            })?;
            for (field, kind, required) in fields {
                match object.get(*field) {
                    // `null` is how the generator writes an absent optional value.
                    None | Some(Value::Null) if !*required => {}
                    None | Some(Value::Null) => {
                        return Err(SnapshotDecodeError(format!("{} is missing", at(field))))
                    }
                    Some(value) => check_kind(value, *kind, &at(field))?,
                }
            }
        }
    }
    Ok(())
}

fn section_entries<'a>(data: &'a Value, name: &str) -> impl Iterator<Item = &'a Value> {
    data.get(name).and_then(Value::as_array).into_iter().flatten()
}

fn optional_string(value: &Value) -> bool {
    value.is_string() || value.is_null()
}

fn optional_bool(value: &Value) -> bool {
    value.is_boolean() || value.is_null()
}

fn check_kind(value: &Value, kind: Kind, at: &str) -> Result<(), SnapshotDecodeError> {
    let wrong = |what: &str| Err(SnapshotDecodeError(format!("{at} must be {what}")));
    let strings = |value: &Value| value.as_array().is_some_and(|a| a.iter().all(Value::is_string));
    let params = |value: &Value| {
        value.as_array().is_some_and(|params| {
            params.iter().all(|param| {
                param.as_object().is_some_and(|p| {
                    p.get("name").is_some_and(Value::is_string)
                        && p.get("param_type").is_none_or(optional_string)
                        && p.get("is_optional").is_none_or(optional_bool)
                        && p.get("is_variadic").is_none_or(optional_bool)
                })
            })
        })
    };
    match kind {
        Kind::Str if !value.is_string() => wrong("a string"),
        Kind::Bool if !value.is_boolean() => wrong("a boolean"),
        Kind::Strings if !strings(value) => wrong("an array of strings"),
        Kind::Params if !params(value) => wrong("an array of parameters"),
        Kind::Variants => {
            let ok = value.as_array().is_some_and(|variants| {
                variants.iter().all(|variant| {
                    variant.as_object().is_some_and(|v| {
                        v.get("variant_name").is_none_or(optional_string)
                            && v.get("parameters").is_none_or(|p| p.is_null() || params(p))
                    })
                })
            });
            if ok {
                Ok(())
            } else {
                wrong("an array of variants")
            }
        }
        Kind::Context => {
            let ok = value.as_object().is_some_and(|flags| flags.values().all(optional_bool));
            if ok {
                Ok(())
            } else {
                wrong("an object of boolean flags")
            }
        }
        Kind::Docs => {
            let Some(docs) = value.as_object() else { return wrong("an object") };
            let text = |field: &str| docs.get(field).is_none_or(optional_string);
            // `required` names the key every item must carry as a string.
            let described = |field: &str, required: Option<&str>, keys: &[&str]| {
                docs.get(field).is_none_or(|items| {
                    items.as_array().is_some_and(|items| {
                        items.iter().all(|item| {
                            item.as_object().is_some_and(|item| {
                                required
                                    .is_none_or(|key| item.get(key).is_some_and(Value::is_string))
                                    && keys
                                        .iter()
                                        .all(|key| item.get(*key).is_none_or(optional_string))
                            })
                        })
                    })
                })
            };
            let ok = text("syntax")
                && text("description")
                && text("notes")
                && docs.get("see_also").is_none_or(strings)
                && described("param_descriptions", Some("name"), &["description", "default_value"])
                && described("examples", None, &["code", "description"]);
            if ok {
                Ok(())
            } else {
                wrong("a documentation object")
            }
        }
        _ => Ok(()),
    }
}

const CORPUS_SECTIONS: &[&str] =
    &["keywords", "types", "methods", "global_functions", "constructors", "properties"];

/// Field defaults mirror the generator that produced the bundled arrays, so the
/// same JSON yields the same records on both paths.
fn decode(data: &Value) -> Result<PlatformSnapshot, SnapshotDecodeError> {
    let mut snapshot = PlatformSnapshot::default();

    for ty in section(data, "types") {
        snapshot.types.push(PlatformType {
            name: string(ty, "name"),
            english_name: string(ty, "english_name"),
            min_version: opt_string(ty, "min_version"),
            context: context(ty),
            iter_element_types: string_list(ty, "iter_element_types"),
            xdto_name: opt_string(ty, "xdto_name"),
        });
    }

    for method in section(data, "methods") {
        let id = id(method, "method")?;
        snapshot.methods.push(PlatformMethod {
            id,
            type_name: string(method, "type_name"),
            name: string(method, "name"),
            english_name: string(method, "english_name"),
            return_type: opt_string(method, "return_type"),
            parameters: params(method),
            variants: variants(method)
                .map(|(variant_name, parameters)| MethodVariant { variant_name, parameters })
                .collect(),
            min_version: opt_string(method, "min_version"),
            context: context(method),
        });
        if let Some(docs) = method.get("documentation").and_then(Value::as_object) {
            snapshot.method_docs.push(method_docs(id, docs));
        }
    }

    for function in section(data, "global_functions") {
        let id = id(function, "global function")?;
        snapshot.global_functions.push(GlobalFunction {
            id,
            name: string(function, "name"),
            english_name: string(function, "english_name"),
            return_type: opt_string(function, "return_type"),
            parameters: params(function),
            variants: variants(function)
                .map(|(variant_name, parameters)| GlobalFunctionVariant {
                    variant_name,
                    parameters,
                })
                .collect(),
            min_version: opt_string(function, "min_version"),
            context: context(function),
        });
        if let Some(docs) = function.get("documentation").and_then(Value::as_object) {
            snapshot.global_function_docs.push(method_docs(id, docs));
        }
    }

    for ctor in section(data, "constructors") {
        let id = id(ctor, "constructor")?;
        snapshot.constructors.push(PlatformConstructor {
            id,
            type_name: string(ctor, "type_name"),
            variant_name: opt_string(ctor, "variant_name"),
            parameters: params(ctor),
            min_version: opt_string(ctor, "min_version"),
            context: context(ctor),
        });
        if let Some(docs) = ctor.get("documentation").and_then(Value::as_object) {
            snapshot.constructor_docs.push(ConstructorDocs {
                constructor_id: id,
                syntax: text(docs, "syntax"),
                description: text(docs, "description"),
                params: param_docs(docs),
                examples: examples(docs),
                notes: opt_text(docs, "notes"),
                see_also: see_also(docs),
            });
        }
    }

    for prop in section(data, "properties") {
        let id = id(prop, "property")?;
        snapshot.properties.push(PlatformProperty {
            id,
            type_name: string(prop, "type_name"),
            name: string(prop, "name"),
            english_name: string(prop, "english_name"),
            property_types: string_list(prop, "property_types"),
            is_readonly: prop.get("is_readonly").and_then(Value::as_bool).unwrap_or(false),
            min_version: opt_string(prop, "min_version"),
            context: context(prop),
        });
        if let Some(docs) = prop.get("documentation").and_then(Value::as_object) {
            snapshot.property_docs.push(PropertyDocs {
                property_id: id,
                description: text(docs, "description"),
                notes: opt_text(docs, "notes"),
                see_also: see_also(docs),
            });
        }
    }

    Ok(snapshot)
}

fn section<'a>(data: &'a Value, name: &str) -> impl Iterator<Item = &'a Value> {
    data.get(name).and_then(Value::as_array).into_iter().flatten()
}

fn id(entry: &Value, kind: &str) -> Result<u32, SnapshotDecodeError> {
    let raw = entry
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| SnapshotDecodeError(format!("{kind} entry has no numeric id")))?;
    u32::try_from(raw).map_err(|_| SnapshotDecodeError(format!("{kind} id {raw} is out of range")))
}

fn string(entry: &Value, field: &str) -> SmolStr {
    SmolStr::new(entry.get(field).and_then(Value::as_str).unwrap_or(""))
}

fn opt_string(entry: &Value, field: &str) -> Option<SmolStr> {
    entry.get(field).and_then(Value::as_str).map(SmolStr::new)
}

fn string_list(entry: &Value, field: &str) -> Vec<SmolStr> {
    entry
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| SmolStr::new(item.as_str().unwrap_or("")))
        .collect()
}

fn context(entry: &Value) -> Option<ContextAvailability> {
    let context = entry.get("context").and_then(Value::as_object)?;
    let flag = |name: &str| context.get(name).and_then(Value::as_bool).unwrap_or(false);
    Some(ContextAvailability {
        thick_client: flag("thick_client"),
        thin_client: flag("thin_client"),
        web_client: flag("web_client"),
        server: flag("server"),
        mobile_client: flag("mobile_client"),
        external_connection: flag("external_connection"),
    })
}

fn param_list(params: &[Value]) -> Vec<MethodParam> {
    params
        .iter()
        .map(|param| MethodParam {
            name: string(param, "name"),
            param_type: opt_string(param, "param_type"),
            is_optional: param.get("is_optional").and_then(Value::as_bool).unwrap_or(false),
            is_variadic: param.get("is_variadic").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

fn params(entry: &Value) -> Vec<MethodParam> {
    entry.get("parameters").and_then(Value::as_array).map(|p| param_list(p)).unwrap_or_default()
}

fn variants(entry: &Value) -> impl Iterator<Item = (Option<SmolStr>, Vec<MethodParam>)> + '_ {
    entry
        .get("variants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|variant| (opt_string(variant, "variant_name"), params(variant)))
}

fn text(docs: &Map<String, Value>, field: &str) -> String {
    docs.get(field).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn opt_text(docs: &Map<String, Value>, field: &str) -> Option<String> {
    docs.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn param_docs(docs: &Map<String, Value>) -> Vec<ParamDocs> {
    docs.get("param_descriptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|param| ParamDocs {
            name: string(param, "name"),
            description: param.get("description").and_then(Value::as_str).unwrap_or("").to_owned(),
            default_value: param.get("default_value").and_then(Value::as_str).map(str::to_owned),
        })
        .collect()
}

fn examples(docs: &Map<String, Value>) -> Vec<CodeExample> {
    docs.get("examples")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|example| CodeExample {
            code: example.get("code").and_then(Value::as_str).unwrap_or("").to_owned(),
            description: example.get("description").and_then(Value::as_str).map(str::to_owned),
        })
        .collect()
}

fn see_also(docs: &Map<String, Value>) -> Vec<String> {
    docs.get("see_also")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| item.as_str().unwrap_or("").to_owned())
        .collect()
}

fn method_docs(id: u32, docs: &Map<String, Value>) -> MethodDocs {
    MethodDocs {
        method_id: id,
        syntax: text(docs, "syntax"),
        description: text(docs, "description"),
        params: param_docs(docs),
        examples: examples(docs),
        notes: opt_text(docs, "notes"),
        see_also: see_also(docs),
    }
}
