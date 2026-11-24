use rustc_hash::FxHashMap;
use serde_json::Value;
use stdx::case::eq_ignore_case;

use crate::overlays::{
    call_value_effect_entries, require_only_fields, required_string, OverlayError,
};
use crate::types::MethodParam;
use crate::PlatformSnapshot;

/// Signature identities are local to their section in the served snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallValueEffectTarget {
    GlobalFunction(u32),
    Method(u32),
    Constructor(u32),
}

/// A call effect described by the overlay. Positions are zero-based signature parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallValueEffect {
    ArgumentToResult(usize),
    ArgumentToReceiver(usize),
    ReceiverToResult,
    ClearReceiver,
    /// The call leaves the caller's argument variables bound to their prior values.
    NoArgumentWriteback {
        parameter_count: usize,
    },
}

enum Target {
    GlobalFunction(String),
    Method { owner: String, name: String },
    Constructor { owner: String, variant: String },
}

pub(crate) fn resolve(
    snapshot: &PlatformSnapshot,
    overlays: &str,
) -> Result<FxHashMap<CallValueEffectTarget, Vec<CallValueEffect>>, OverlayError> {
    let mut resolved: FxHashMap<_, Vec<_>> = FxHashMap::default();
    for (index, entry) in call_value_effect_entries(overlays)?.into_iter().enumerate() {
        let invalid = |message: &str| OverlayError(format!("overlay entry {index} {message}"));
        let entry = entry.as_object().ok_or_else(|| invalid("must be an object"))?;
        require_only_fields(
            entry,
            &[
                "target",
                "effect",
                "parameter_index",
                "parameter_count",
                "evidence_source",
                "rationale",
            ],
            &format!("overlay entry {index}"),
        )?;
        required_string(entry, "evidence_source", index)?;
        required_string(entry, "rationale", index)?;
        let target = entry
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("must name a function, method or constructor target"))?;
        let only =
            |fields| require_only_fields(target, fields, &format!("overlay entry {index} target"));
        let target = match required_string(target, "kind", index)?.as_str() {
            "function" => {
                only(&["kind", "name"])?;
                Target::GlobalFunction(required_string(target, "name", index)?)
            }
            "method" => {
                only(&["kind", "type", "name"])?;
                Target::Method {
                    owner: required_string(target, "type", index)?,
                    name: required_string(target, "name", index)?,
                }
            }
            "constructor" => {
                only(&["kind", "type", "variant"])?;
                Target::Constructor {
                    owner: required_string(target, "type", index)?,
                    variant: required_string(target, "variant", index)?,
                }
            }
            _ => return Err(invalid("must name a function, method or constructor target")),
        };
        let parameter_number = |field| {
            entry
                .get(field)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| invalid(&format!("{field} must be a non-negative integer")))
        };
        let effect_name = required_string(entry, "effect", index)?;
        if effect_name != "no_argument_writeback" && entry.contains_key("parameter_count") {
            return Err(invalid("parameter_count is only valid for no_argument_writeback"));
        }
        let effect = match effect_name.as_str() {
            "argument_to_result" => {
                CallValueEffect::ArgumentToResult(parameter_number("parameter_index")?)
            }
            "argument_to_receiver" if matches!(target, Target::Method { .. }) => {
                CallValueEffect::ArgumentToReceiver(parameter_number("parameter_index")?)
            }
            "no_argument_writeback"
                if matches!(target, Target::Method { .. })
                    && !entry.contains_key("parameter_index") =>
            {
                CallValueEffect::NoArgumentWriteback {
                    parameter_count: parameter_number("parameter_count")?,
                }
            }
            effect @ ("receiver_to_result" | "clear_receiver")
                if matches!(target, Target::Method { .. })
                    && !entry.contains_key("parameter_index") =>
            {
                if effect == "receiver_to_result" {
                    CallValueEffect::ReceiverToResult
                } else {
                    CallValueEffect::ClearReceiver
                }
            }
            _ => return Err(invalid("has an unknown or inapplicable call value effect")),
        };

        // An extract from another release may omit the target or represent a different signature.
        let Some((target, params)) = target.signature(snapshot) else { continue };
        match effect {
            CallValueEffect::ArgumentToResult(index)
            | CallValueEffect::ArgumentToReceiver(index)
                if index >= params.len() =>
            {
                continue
            }
            CallValueEffect::NoArgumentWriteback { parameter_count }
                if parameter_count != params.len()
                    || params.iter().any(|param| param.is_variadic) =>
            {
                continue
            }
            _ => {}
        }
        let effects = resolved.entry(target).or_default();
        if effects.contains(&effect) {
            return Err(invalid("duplicates a call value effect for this signature"));
        }
        effects.push(effect);
    }
    Ok(resolved)
}

impl Target {
    fn signature<'a>(
        &self,
        snapshot: &'a PlatformSnapshot,
    ) -> Option<(CallValueEffectTarget, &'a [MethodParam])> {
        let unique_owner = |name: &str| {
            exactly_one(snapshot.types.iter().filter(|ty| {
                eq_ignore_case(&ty.english_name, name) || eq_ignore_case(&ty.name, name)
            }))
            .map(|ty| ty.english_name.as_str())
        };
        match self {
            Self::GlobalFunction(name) => {
                let function = exactly_one(snapshot.global_functions.iter().filter(|function| {
                    eq_ignore_case(&function.english_name, name)
                        || eq_ignore_case(&function.name, name)
                }))?;
                function.variants.is_empty().then_some((
                    CallValueEffectTarget::GlobalFunction(function.id),
                    function.parameters.as_slice(),
                ))
            }
            Self::Method { owner, name } => {
                let owner = unique_owner(owner)?;
                let method = exactly_one(snapshot.methods.iter().filter(|method| {
                    eq_ignore_case(&method.type_name, owner)
                        && (eq_ignore_case(&method.english_name, name)
                            || eq_ignore_case(&method.name, name))
                }))?;
                method.variants.is_empty().then_some((
                    CallValueEffectTarget::Method(method.id),
                    method.parameters.as_slice(),
                ))
            }
            Self::Constructor { owner, variant } => {
                let owner = unique_owner(owner)?;
                let constructor =
                    exactly_one(snapshot.constructors.iter().filter(|constructor| {
                        eq_ignore_case(&constructor.type_name, owner)
                            && constructor
                                .variant_name
                                .as_deref()
                                .is_some_and(|name| eq_ignore_case(name, variant))
                    }))?;
                Some((
                    CallValueEffectTarget::Constructor(constructor.id),
                    constructor.parameters.as_slice(),
                ))
            }
        }
    }
}

fn exactly_one<T>(mut matches: impl Iterator<Item = T>) -> Option<T> {
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}
