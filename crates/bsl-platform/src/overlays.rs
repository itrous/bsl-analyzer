#[path = "overlay_schema.rs"]
mod overlay_schema;

pub(crate) use overlay_schema::OverlayError;
use overlay_schema::{
    parse_global_function_overrides, parse_overrides, parse_property_additions,
    validate_version_bounds, MethodParameterOverride, TypePropertyAddition,
};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub(crate) fn apply_method_parameter_overlays(
    data: &mut Value,
    overlay_source: &str,
) -> Result<(), OverlayError> {
    let overrides = parse_overrides(overlay_source)?;
    let type_names = extracted_type_names(data);
    let methods = data
        .get_mut("methods")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| OverlayError("base platform data has no methods array".to_owned()))?;
    apply_parameter_overrides(methods, overrides, &type_names)
}

/// Global-context functions live apart from type methods in the extract, so
/// their overrides name no `canonical_type` and resolve by name alone.
pub(crate) fn apply_global_function_parameter_overlays(
    data: &mut Value,
    overlay_source: &str,
) -> Result<(), OverlayError> {
    let overrides = parse_global_function_overrides(overlay_source)?;
    if overrides.is_empty() {
        return Ok(());
    }
    let type_names = extracted_type_names(data);
    let functions =
        data.get_mut("global_functions").and_then(Value::as_array_mut).ok_or_else(|| {
            OverlayError("base platform data has no global_functions array".to_owned())
        })?;
    apply_parameter_overrides(functions, overrides, &type_names)
}

fn extracted_type_names(data: &Value) -> Vec<String> {
    data.get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|ty| ty.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// A `Семейство: *` entry stands for every extracted type of that family
/// (`ОбъектМетаданных: Справочник`, `ОбъектМетаданных: Документ`, …). A
/// parameter documented as any metadata object admits some seventy kinds;
/// spelling them out in each override would repeat the list and drift from the
/// extract once it is regenerated with new kinds.
fn expand_replacement(
    overlay: &MethodParameterOverride,
    type_names: &[String],
) -> Result<Vec<String>, OverlayError> {
    let mut expanded = Vec::new();
    for entry in &overlay.replacement_type_list {
        let Some(family) = entry.strip_suffix('*').filter(|family| family.ends_with(": ")) else {
            if !expanded.contains(entry) {
                expanded.push(entry.clone());
            }
            continue;
        };
        let members: Vec<&String> =
            type_names.iter().filter(|name| name.starts_with(family)).collect();
        if members.is_empty() {
            return Err(OverlayError(format!(
                "overlay target {} replacement {entry} names no extracted type",
                overlay.target()
            )));
        }
        for member in members {
            if !expanded.contains(member) {
                expanded.push(member.clone());
            }
        }
    }
    Ok(expanded)
}

fn apply_parameter_overrides(
    methods: &mut [Value],
    overrides: Vec<MethodParameterOverride>,
    type_names: &[String],
) -> Result<(), OverlayError> {
    let mut overridden_parameters = HashSet::new();

    for overlay in overrides {
        let owner = overlay.canonical_type.as_deref();
        let ru_matches = method_matches(methods, owner, "name", &overlay.russian_name);
        let en_matches = method_matches(methods, owner, "english_name", &overlay.english_name);
        let method_index = resolve_method_index(&overlay, &ru_matches, &en_matches)?;
        let parameter_key = (method_index, overlay.parameter_index);
        if !overridden_parameters.insert(parameter_key) {
            return Err(OverlayError(format!(
                "duplicate override for {} parameter {}",
                overlay.target(),
                overlay.parameter_index
            )));
        }

        let replacement = expand_replacement(&overlay, type_names)?;
        let method = &mut methods[method_index];
        validate_version_bounds(method, &overlay)?;
        let parameters =
            method.get_mut("parameters").and_then(Value::as_array_mut).ok_or_else(|| {
                OverlayError(format!("overlay target {} has no parameters array", overlay.target()))
            })?;
        let parameter = parameters.get_mut(overlay.parameter_index).ok_or_else(|| {
            OverlayError(format!(
                "overlay target {} has no parameter at index {}",
                overlay.target(),
                overlay.parameter_index
            ))
        })?;
        let parameter_object = parameter.as_object_mut().ok_or_else(|| {
            OverlayError(format!(
                "overlay target {} parameter {} is not an object",
                overlay.target(),
                overlay.parameter_index
            ))
        })?;
        parameter_object.insert("param_type".to_owned(), Value::String(replacement.join(", ")));
    }

    Ok(())
}

pub(crate) fn apply_type_property_additions(
    data: &mut Value,
    overlay_source: &str,
) -> Result<(), OverlayError> {
    let additions = parse_property_additions(overlay_source)?;
    if additions.is_empty() {
        return Ok(());
    }

    // `canonical_type` must be the English canonical name: the generated
    // `properties_by_type` index keys on the property's `type_name` as the
    // English type key, so a Russian alias here would produce a property that
    // `get_type_properties` (which resolves RU → EN first) can never find.
    let known_types: HashSet<String> = data
        .get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|object| object.get("english_name").and_then(Value::as_str).map(str::to_owned))
        .collect();

    // A synthetic id keeps documentation lookups (keyed by property id) from
    // aliasing an existing property's docs; the additions carry no docs of their
    // own, and each takes the next slot after the current maximum.
    let base_id = data
        .get("properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|prop| prop.get("id").and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
        + 1;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    for (offset, addition) in additions.into_iter().enumerate() {
        if !known_types.contains(&addition.canonical_type) {
            return Err(OverlayError(format!(
                "type property addition target {} is not a known platform type (canonical_type must be the English type name)",
                addition.canonical_type
            )));
        }
        for name in [&addition.russian_name, &addition.english_name] {
            let key = (addition.canonical_type.clone(), name.to_lowercase());
            if !seen.insert(key) {
                return Err(OverlayError(format!(
                    "duplicate type property addition for {}.{}",
                    addition.canonical_type, addition.english_name
                )));
            }
        }
        if property_exists(data, &addition) {
            return Err(OverlayError(format!(
                "type property addition {}.{} already exists in the extracted data",
                addition.canonical_type, addition.english_name
            )));
        }

        let property = build_property_value(base_id + offset as u64, &addition);
        data.get_mut("properties")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| OverlayError("base platform data has no properties array".to_owned()))?
            .push(property);
    }

    Ok(())
}

fn property_exists(data: &Value, addition: &TypePropertyAddition) -> bool {
    data.get("properties").and_then(Value::as_array).is_some_and(|properties| {
        properties.iter().any(|prop| {
            let Some(object) = prop.as_object() else {
                return false;
            };
            if object.get("type_name").and_then(Value::as_str) != Some(&addition.canonical_type) {
                return false;
            }
            // The runtime name index keys both the RU and EN alias of every
            // property into one namespace, so a collision on *either* alias
            // silently shadows the existing entry. Compare both of the
            // addition's names against both of the existing property's names.
            let addition_names = [&addition.russian_name, &addition.english_name];
            ["name", "english_name"].iter().any(|field| {
                object.get(*field).and_then(Value::as_str).is_some_and(|existing| {
                    addition_names.iter().any(|name| name.to_lowercase() == existing.to_lowercase())
                })
            })
        })
    })
}

fn build_property_value(id: u64, addition: &TypePropertyAddition) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::from(id));
    object.insert("type_name".to_owned(), Value::String(addition.canonical_type.clone()));
    object.insert("name".to_owned(), Value::String(addition.russian_name.clone()));
    object.insert("english_name".to_owned(), Value::String(addition.english_name.clone()));
    object.insert(
        "property_types".to_owned(),
        Value::Array(addition.property_types.iter().cloned().map(Value::String).collect()),
    );
    object.insert("is_readonly".to_owned(), Value::Bool(addition.is_readonly));
    if let Some(min_version) = &addition.min_version {
        object.insert("min_version".to_owned(), Value::String(min_version.clone()));
    }
    Value::Object(object)
}

/// Entries named `name`; with an owner, only that type's methods.
fn method_matches(
    methods: &[Value],
    canonical_type: Option<&str>,
    name_field: &str,
    name: &str,
) -> Vec<usize> {
    methods
        .iter()
        .enumerate()
        .filter_map(|(index, method)| {
            let object = method.as_object()?;
            let owned = match canonical_type {
                Some(canonical_type) => object.get("type_name")?.as_str()? == canonical_type,
                None => true,
            };
            let method_name = object.get(name_field)?.as_str()?;
            (owned && method_name.to_lowercase() == name.to_lowercase()).then_some(index)
        })
        .collect()
}

fn resolve_method_index(
    overlay: &MethodParameterOverride,
    ru_matches: &[usize],
    en_matches: &[usize],
) -> Result<usize, OverlayError> {
    match (ru_matches, en_matches) {
        ([ru_index], [en_index]) if ru_index == en_index => Ok(*ru_index),
        ([ru_index], [en_index]) => Err(OverlayError(format!(
            "overlay aliases for {} resolve to different methods ({ru_index} and {en_index})",
            overlay.target()
        ))),
        _ => Err(OverlayError(format!(
            "overlay target {} is missing or ambiguous",
            overlay.target()
        ))),
    }
}

#[cfg(test)]
#[path = "overlays_tests.rs"]
mod tests;
