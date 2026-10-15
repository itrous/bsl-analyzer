use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OverlayError(pub(crate) String);

impl fmt::Display for OverlayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OverlayError {}

#[derive(Debug)]
pub(crate) struct MethodParameterOverride {
    /// The owning type of a method; `None` for a global-context function.
    pub(crate) canonical_type: Option<String>,
    pub(crate) russian_name: String,
    pub(crate) english_name: String,
    pub(crate) min_version: Option<Vec<u32>>,
    pub(crate) max_version: Option<Vec<u32>>,
    pub(crate) parameter_index: usize,
    pub(crate) replacement_type_list: Vec<String>,
}

impl MethodParameterOverride {
    /// `Type.Method` for a method, `Function` for a global-context function.
    pub(crate) fn target(&self) -> String {
        match &self.canonical_type {
            Some(canonical_type) => format!("{canonical_type}.{}", self.english_name),
            None => self.english_name.clone(),
        }
    }
}

/// A standard property the extracted help archive omits or files under a
/// misleading name (e.g. the managed-form `РежимОткрытияОкна`, whose help page
/// is stored as the enum type `FormWindowOpeningMode`). The addition attaches
/// the property to `canonical_type` so name resolution and the unused-local
/// check recognise it.
#[derive(Debug)]
pub(crate) struct TypePropertyAddition {
    pub(crate) canonical_type: String,
    pub(crate) russian_name: String,
    pub(crate) english_name: String,
    pub(crate) property_types: Vec<String>,
    pub(crate) is_readonly: bool,
    pub(crate) min_version: Option<String>,
}

/// A correction to a property the extract already carries, but with a wrong
/// fact — the help page omits the read-only mark, so the extracted record says
/// the property is writable and the analyzer accepts a write the runtime
/// refuses. Unlike [`TypePropertyAddition`] this never invents a property: the
/// target must exist in the corpus, and a corpus of another platform release
/// that lacks it is left alone (the same overlay serves every source).
#[derive(Debug)]
pub(crate) struct TypePropertyCorrection {
    /// The owning type, by its Russian or English spelling.
    pub(crate) type_name: String,
    /// The property, by its Russian or English spelling.
    pub(crate) property_name: String,
    /// `Some` replaces the extracted flag, `None` leaves it as extracted.
    pub(crate) is_readonly: Option<bool>,
    /// `Some` replaces the whole extracted value-type list.
    pub(crate) property_types: Option<Vec<String>>,
}

/// A platform type whose extracted member surface — every method and property —
/// is attested as complete for the named platform release. A miss on such a
/// receiver is a defect of the checked code rather than a gap in the extract,
/// which is what lets the absent-member checks judge a platform type at all:
/// for the types without an attestation the extract is not known to be
/// exhaustive, and a miss there proves nothing.
#[derive(Debug)]
pub(crate) struct TypeMemberSurfaceAttestation {
    /// The attested type, by its Russian or English spelling.
    pub(crate) type_name: String,
    /// The platform release the surface was verified against, `8.3.27` — the
    /// check runs only where the served corpus is of that version or newer.
    pub(crate) verified_against_version: String,
}

const OVERLAY_ROOT_FIELDS: &[&str] = &[
    "schema_version",
    "method_parameter_overrides",
    "global_function_parameter_overrides",
    "type_property_additions",
    "type_property_corrections",
    "type_member_surface_attestations",
];

pub(crate) fn parse_overrides(
    overlay_source: &str,
) -> Result<Vec<MethodParameterOverride>, OverlayError> {
    let root: Value = serde_json::from_str(overlay_source)
        .map_err(|error| OverlayError(format!("malformed overlay JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| OverlayError("overlay root must be an object".to_owned()))?;
    require_only_fields(object, OVERLAY_ROOT_FIELDS, "overlay root")?;
    validate_schema_version(object)?;
    let entries =
        object.get("method_parameter_overrides").and_then(Value::as_array).ok_or_else(|| {
            OverlayError("overlay method_parameter_overrides must be an array".to_owned())
        })?;

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_override(index, entry, OverrideTarget::TypeMethod))
        .collect()
}

/// Parses the optional `global_function_parameter_overrides` section: the same
/// entry shape as a method override, without `canonical_type`. An absent section
/// yields no overrides.
pub(crate) fn parse_global_function_overrides(
    overlay_source: &str,
) -> Result<Vec<MethodParameterOverride>, OverlayError> {
    let root: Value = serde_json::from_str(overlay_source)
        .map_err(|error| OverlayError(format!("malformed overlay JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| OverlayError("overlay root must be an object".to_owned()))?;
    require_only_fields(object, OVERLAY_ROOT_FIELDS, "overlay root")?;
    validate_schema_version(object)?;
    let entries = match object.get("global_function_parameter_overrides") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| {
            OverlayError("overlay global_function_parameter_overrides must be an array".to_owned())
        })?,
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_override(index, entry, OverrideTarget::GlobalFunction))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OverrideTarget {
    TypeMethod,
    GlobalFunction,
}

/// Parses the optional `type_property_additions` section. An absent section
/// yields no additions; the root schema is validated the same way as
/// [`parse_overrides`].
pub(crate) fn parse_property_additions(
    overlay_source: &str,
) -> Result<Vec<TypePropertyAddition>, OverlayError> {
    let root: Value = serde_json::from_str(overlay_source)
        .map_err(|error| OverlayError(format!("malformed overlay JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| OverlayError("overlay root must be an object".to_owned()))?;
    require_only_fields(object, OVERLAY_ROOT_FIELDS, "overlay root")?;
    validate_schema_version(object)?;
    let entries = match object.get("type_property_additions") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| {
            OverlayError("overlay type_property_additions must be an array".to_owned())
        })?,
    };

    entries.iter().enumerate().map(|(index, entry)| parse_property_addition(index, entry)).collect()
}

/// Parses the optional `type_property_corrections` section. An absent section
/// yields no corrections; the root schema is validated the same way as
/// [`parse_overrides`].
pub(crate) fn parse_property_corrections(
    overlay_source: &str,
) -> Result<Vec<TypePropertyCorrection>, OverlayError> {
    let root: Value = serde_json::from_str(overlay_source)
        .map_err(|error| OverlayError(format!("malformed overlay JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| OverlayError("overlay root must be an object".to_owned()))?;
    require_only_fields(object, OVERLAY_ROOT_FIELDS, "overlay root")?;
    validate_schema_version(object)?;
    let entries = match object.get("type_property_corrections") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| {
            OverlayError("overlay type_property_corrections must be an array".to_owned())
        })?,
    };

    let corrections: Vec<TypePropertyCorrection> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_property_correction(index, entry))
        .collect::<Result<_, _>>()?;
    // Two corrections of one property would fight over the same record; the
    // loser's correction would silently not apply, so refuse the document.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for correction in &corrections {
        let key = (correction.type_name.to_lowercase(), correction.property_name.to_lowercase());
        if !seen.insert(key) {
            return Err(OverlayError(format!(
                "duplicate type property correction for {}.{}",
                correction.type_name, correction.property_name
            )));
        }
    }
    Ok(corrections)
}

/// Parses the optional `type_member_surface_attestations` section. An absent
/// section yields no attestations; the root schema is validated the same way as
/// [`parse_overrides`].
pub(crate) fn parse_member_surface_attestations(
    overlay_source: &str,
) -> Result<Vec<TypeMemberSurfaceAttestation>, OverlayError> {
    let root: Value = serde_json::from_str(overlay_source)
        .map_err(|error| OverlayError(format!("malformed overlay JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| OverlayError("overlay root must be an object".to_owned()))?;
    require_only_fields(object, OVERLAY_ROOT_FIELDS, "overlay root")?;
    validate_schema_version(object)?;
    let entries = match object.get("type_member_surface_attestations") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| {
            OverlayError("overlay type_member_surface_attestations must be an array".to_owned())
        })?,
    };

    let attestations: Vec<TypeMemberSurfaceAttestation> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_member_surface_attestation(index, entry))
        .collect::<Result<_, _>>()?;
    let mut seen: HashSet<String> = HashSet::new();
    for attestation in &attestations {
        if !seen.insert(attestation.type_name.to_lowercase()) {
            return Err(OverlayError(format!(
                "duplicate type member surface attestation for {}",
                attestation.type_name
            )));
        }
    }
    Ok(attestations)
}

fn parse_property_correction(
    index: usize,
    entry: &Value,
) -> Result<TypePropertyCorrection, OverlayError> {
    let context = format!("type property correction {index}");
    let object =
        entry.as_object().ok_or_else(|| OverlayError(format!("{context} must be an object")))?;
    require_only_fields(
        object,
        &["type", "name", "is_readonly", "property_types", "evidence_source", "rationale"],
        &context,
    )?;
    let type_name = required_string(object, "type", index)?;
    let property_name = required_string(object, "name", index)?;
    let is_readonly = match object.get("is_readonly") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| OverlayError(format!("{context} is_readonly must be a boolean")))?,
        ),
    };
    let property_types = match object.get("property_types") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let types = value
                .as_array()
                .ok_or_else(|| OverlayError(format!("{context} property_types must be an array")))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            OverlayError(format!(
                                "{context} property_types must contain non-empty strings"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if types.is_empty() {
                return Err(OverlayError(format!("{context} property_types must not be empty")));
            }
            let unique: HashSet<&str> = types.iter().map(String::as_str).collect();
            if unique.len() != types.len() {
                return Err(OverlayError(format!(
                    "{context} property_types contains duplicate types"
                )));
            }
            Some(types)
        }
    };
    if is_readonly.is_none() && property_types.is_none() {
        return Err(OverlayError(format!(
            "{context} changes nothing: it needs is_readonly or property_types"
        )));
    }
    required_string(object, "evidence_source", index)?;
    required_string(object, "rationale", index)?;

    Ok(TypePropertyCorrection { type_name, property_name, is_readonly, property_types })
}

fn parse_member_surface_attestation(
    index: usize,
    entry: &Value,
) -> Result<TypeMemberSurfaceAttestation, OverlayError> {
    let context = format!("type member surface attestation {index}");
    let object =
        entry.as_object().ok_or_else(|| OverlayError(format!("{context} must be an object")))?;
    require_only_fields(
        object,
        &["type", "verified_against_version", "evidence_source", "rationale"],
        &context,
    )?;
    let type_name = required_string(object, "type", index)?;
    let verified_against_version = required_string(object, "verified_against_version", index)?;
    match parse_version(&verified_against_version) {
        // A release, not a version line: `8` or `8.3` would stand for every
        // release of that line and open the check on each of them, so the
        // attestation has to name at least major.minor.patch.
        Some(components) if components.len() >= 3 => {}
        Some(_) => {
            return Err(OverlayError(format!(
                "{context} verified_against_version must be at least major.minor.patch"
            )))
        }
        None => {
            return Err(OverlayError(format!(
                "{context} verified_against_version must be a dotted numeric version"
            )))
        }
    }
    required_string(object, "evidence_source", index)?;
    required_string(object, "rationale", index)?;

    Ok(TypeMemberSurfaceAttestation { type_name, verified_against_version })
}

fn validate_schema_version(object: &Map<String, Value>) -> Result<(), OverlayError> {
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| OverlayError("overlay schema_version must be the number 1".to_owned()))?;
    if schema_version != 1 {
        return Err(OverlayError(format!(
            "unsupported overlay schema_version {schema_version}; expected 1"
        )));
    }
    Ok(())
}

fn parse_property_addition(
    index: usize,
    entry: &Value,
) -> Result<TypePropertyAddition, OverlayError> {
    let object = entry
        .as_object()
        .ok_or_else(|| OverlayError(format!("type property addition {index} must be an object")))?;
    require_only_fields(
        object,
        &[
            "canonical_type",
            "russian_name",
            "english_name",
            "property_types",
            "is_readonly",
            "min_version",
            "evidence_source",
            "rationale",
        ],
        &format!("type property addition {index}"),
    )?;
    let canonical_type = required_string(object, "canonical_type", index)?;
    let russian_name = required_string(object, "russian_name", index)?;
    let english_name = required_string(object, "english_name", index)?;
    let property_types = object
        .get("property_types")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            OverlayError(format!("type property addition {index} property_types must be an array"))
        })?
        .iter()
        .map(|value| {
            value.as_str().filter(|name| !name.trim().is_empty()).map(str::to_owned).ok_or_else(
                || {
                    OverlayError(format!(
                        "type property addition {index} property_types must contain non-empty strings"
                    ))
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    if property_types.is_empty() {
        return Err(OverlayError(format!(
            "type property addition {index} property_types must not be empty"
        )));
    }
    let unique_types: HashSet<&str> = property_types.iter().map(String::as_str).collect();
    if unique_types.len() != property_types.len() {
        return Err(OverlayError(format!(
            "type property addition {index} property_types contains duplicate types"
        )));
    }
    let is_readonly = match object.get("is_readonly") {
        None | Some(Value::Null) => false,
        Some(value) => value.as_bool().ok_or_else(|| {
            OverlayError(format!("type property addition {index} is_readonly must be a boolean"))
        })?,
    };
    let min_version = match object.get("min_version") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => {
            if parse_version(value).is_none() {
                return Err(OverlayError(format!(
                    "type property addition {index} min_version must be a dotted numeric version"
                )));
            }
            Some(value.clone())
        }
        Some(_) => {
            return Err(OverlayError(format!(
            "type property addition {index} min_version must be a dotted numeric version or null"
        )))
        }
    };
    required_string(object, "evidence_source", index)?;
    required_string(object, "rationale", index)?;

    Ok(TypePropertyAddition {
        canonical_type,
        russian_name,
        english_name,
        property_types,
        is_readonly,
        min_version,
    })
}

pub(crate) fn validate_version_bounds(
    method: &Value,
    overlay: &MethodParameterOverride,
) -> Result<(), OverlayError> {
    if overlay.min_version.is_none() && overlay.max_version.is_none() {
        return Ok(());
    }
    let target_version =
        method.get("min_version").and_then(Value::as_str).and_then(parse_version).ok_or_else(
            || {
                OverlayError(format!(
                    "overlay target {} has no valid minimum version",
                    overlay.target()
                ))
            },
        )?;
    let below_minimum =
        overlay.min_version.as_ref().is_some_and(|min_version| target_version < *min_version);
    let above_maximum =
        overlay.max_version.as_ref().is_some_and(|max_version| target_version > *max_version);
    if below_minimum || above_maximum {
        return Err(OverlayError(format!(
            "overlay target {} version is outside its declared bounds",
            overlay.target()
        )));
    }
    Ok(())
}

fn parse_override(
    index: usize,
    entry: &Value,
    target: OverrideTarget,
) -> Result<MethodParameterOverride, OverlayError> {
    let object = entry
        .as_object()
        .ok_or_else(|| OverlayError(format!("overlay entry {index} must be an object")))?;
    const ENTRY_FIELDS: &[&str] = &[
        "russian_name",
        "english_name",
        "min_version",
        "max_version",
        "parameter_index",
        "replacement_type_list",
        "evidence_source",
        "rationale",
    ];
    let allowed_fields = match target {
        OverrideTarget::TypeMethod => [&["canonical_type"], ENTRY_FIELDS].concat(),
        OverrideTarget::GlobalFunction => ENTRY_FIELDS.to_vec(),
    };
    require_only_fields(object, &allowed_fields, &format!("overlay entry {index}"))?;
    let canonical_type = match target {
        OverrideTarget::TypeMethod => Some(required_string(object, "canonical_type", index)?),
        OverrideTarget::GlobalFunction => None,
    };
    let russian_name = required_string(object, "russian_name", index)?;
    let english_name = required_string(object, "english_name", index)?;
    let min_version = optional_version(object, "min_version", index)?;
    let max_version = optional_version(object, "max_version", index)?;
    if min_version.as_ref().zip(max_version.as_ref()).is_some_and(|(min, max)| min > max) {
        return Err(OverlayError(format!(
            "overlay entry {index} has min_version after max_version"
        )));
    }
    let parameter_index = object
        .get("parameter_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            OverlayError(format!("overlay entry {index} parameter_index must be an integer"))
        })?;
    let replacement_type_list = object
        .get("replacement_type_list")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            OverlayError(format!("overlay entry {index} replacement_type_list must be an array"))
        })?
        .iter()
        .map(|value| {
            value.as_str().filter(|name| !name.trim().is_empty()).map(str::to_owned).ok_or_else(
                || {
                    OverlayError(format!(
                    "overlay entry {index} replacement_type_list must contain non-empty strings"
                ))
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    if replacement_type_list.is_empty() {
        return Err(OverlayError(format!(
            "overlay entry {index} replacement_type_list must not be empty"
        )));
    }
    let unique_types: HashSet<&str> = replacement_type_list.iter().map(String::as_str).collect();
    if unique_types.len() != replacement_type_list.len() {
        return Err(OverlayError(format!(
            "overlay entry {index} replacement_type_list contains duplicate types"
        )));
    }
    required_string(object, "evidence_source", index)?;
    required_string(object, "rationale", index)?;

    Ok(MethodParameterOverride {
        canonical_type,
        russian_name,
        english_name,
        min_version,
        max_version,
        parameter_index,
        replacement_type_list,
    })
}

fn require_only_fields(
    object: &Map<String, Value>,
    allowed_fields: &[&str],
    context: &str,
) -> Result<(), OverlayError> {
    if let Some(field) = object.keys().find(|field| !allowed_fields.contains(&field.as_str())) {
        return Err(OverlayError(format!("{context} has unknown field {field}")));
    }
    Ok(())
}

fn required_string(
    object: &Map<String, Value>,
    field: &str,
    entry_index: usize,
) -> Result<String, OverlayError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            OverlayError(format!("overlay entry {entry_index} {field} must be a non-empty string"))
        })
}

fn optional_version(
    object: &Map<String, Value>,
    field: &str,
    entry_index: usize,
) -> Result<Option<Vec<u32>>, OverlayError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => parse_version(value).map(Some).ok_or_else(|| {
            OverlayError(format!(
                "overlay entry {entry_index} {field} must be a dotted numeric version"
            ))
        }),
        Some(_) => Err(OverlayError(format!(
            "overlay entry {entry_index} {field} must be a dotted numeric version or null"
        ))),
    }
}

/// A dotted numeric version (`8.3.27.2214`) as comparable components. Components
/// are plain decimal digits: `u32`'s own parser would accept a sign, and
/// `+8.3.27` is no release spelling.
pub(crate) fn parse_version(value: &str) -> Option<Vec<u32>> {
    let components = value
        .split('.')
        .map(|component| {
            if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            component.parse::<u32>().ok()
        })
        .collect::<Option<Vec<u32>>>()?;
    (!components.is_empty()).then_some(components)
}
