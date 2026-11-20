use bsl_types::builders::Builders as _;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};

use super::{CallParamMode, CallSignature};

/// Reason an argument comparison cannot produce a concrete compatibility verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgumentIndeterminateReason {
    UnknownArgument,
    UnknownParameter,
    UnknownArgumentAndParameter,
    MissingParameterMetadata,
}

/// Compatibility of one argument with its declared parameter position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgumentApplicability {
    Exact,
    Assignable,
    ArgumentCoercion,
    Indeterminate(ArgumentIndeterminateReason),
    Incompatible,
}

/// Parameter metadata used to classify one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgumentParameter {
    Declared { index: usize, mode: CallParamMode, ty: TypeId },
    MissingMetadata,
}

/// Structured evidence for one argument comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArgumentEvaluation {
    pub index: usize,
    pub argument_ty: TypeId,
    pub parameter: ArgumentParameter,
    pub applicability: ArgumentApplicability,
}

/// Arity usage facts for an arity-compatible candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArityUsage {
    pub actual: usize,
    pub defaults_used: usize,
    pub variadic_arguments: usize,
}

/// Concrete reason a call cannot fit a candidate's accepted arity range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArityMismatch {
    TooFew { actual: usize, required: usize },
    TooMany { actual: usize, maximum: usize },
}

/// Type applicability of an arity-compatible candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateApplicability {
    Applicable { arguments: Box<[ArgumentEvaluation]> },
    Indeterminate { arguments: Box<[ArgumentEvaluation]> },
    Incompatible { arguments: Box<[ArgumentEvaluation]> },
}

/// Complete pure evaluation of one candidate against one argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateEvaluation {
    ArityIncompatible(ArityMismatch),
    ArityCompatible { usage: ArityUsage, applicability: CandidateApplicability },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterLayout {
    Fixed,
    VariadicTail { index: usize },
    MalformedVariadic { first_index: usize },
}

/// Evaluates only the candidate's accepted argument-count range and usage facts.
pub fn evaluate_arity(
    signature: &CallSignature,
    argument_count: usize,
) -> Result<ArityUsage, ArityMismatch> {
    if argument_count < signature.required_args {
        return Err(ArityMismatch::TooFew {
            actual: argument_count,
            required: signature.required_args,
        });
    }
    if let Some(maximum) = signature.max_args {
        if argument_count > maximum {
            return Err(ArityMismatch::TooMany { actual: argument_count, maximum });
        }
    }

    let defaults_used = signature
        .params
        .iter()
        .enumerate()
        .filter(|(index, param)| {
            *index >= argument_count && param.mode == CallParamMode::Positional && param.has_default
        })
        .count();
    let variadic_arguments = match parameter_layout(signature) {
        ParameterLayout::VariadicTail { index } => argument_count.saturating_sub(index),
        ParameterLayout::Fixed | ParameterLayout::MalformedVariadic { .. } => 0,
    };
    Ok(ArityUsage { actual: argument_count, defaults_used, variadic_arguments })
}

/// Evaluates arity first, then classifies every argument against one signature position.
pub fn evaluate_applicability(
    db: &dyn TypeKernelDb,
    signature: &CallSignature,
    argument_types: &[TypeId],
) -> CandidateEvaluation {
    let usage = match evaluate_arity(signature, argument_types.len()) {
        Ok(usage) => usage,
        Err(reason) => return CandidateEvaluation::ArityIncompatible(reason),
    };

    let layout = parameter_layout(signature);
    let arguments: Box<[ArgumentEvaluation]> = argument_types
        .iter()
        .copied()
        .enumerate()
        .map(|(index, argument_ty)| {
            let parameter = parameter_for_argument(signature, layout, index);
            let applicability = match parameter {
                ArgumentParameter::Declared { ty: parameter_ty, .. } => {
                    classify_argument(db, argument_ty, parameter_ty)
                }
                ArgumentParameter::MissingMetadata => ArgumentApplicability::Indeterminate(
                    ArgumentIndeterminateReason::MissingParameterMetadata,
                ),
            };
            ArgumentEvaluation { index, argument_ty, parameter, applicability }
        })
        .collect();
    let applicability = if arguments
        .iter()
        .any(|argument| argument.applicability == ArgumentApplicability::Incompatible)
    {
        CandidateApplicability::Incompatible { arguments }
    } else if arguments
        .iter()
        .any(|argument| matches!(argument.applicability, ArgumentApplicability::Indeterminate(_)))
    {
        CandidateApplicability::Indeterminate { arguments }
    } else {
        CandidateApplicability::Applicable { arguments }
    };
    CandidateEvaluation::ArityCompatible { usage, applicability }
}

fn classify_argument(
    db: &dyn TypeKernelDb,
    argument_ty: TypeId,
    parameter_ty: TypeId,
) -> ArgumentApplicability {
    let argument_unknown = matches!(db.lookup_type(argument_ty), TypeKind::Unknown);
    let parameter_unknown = matches!(db.lookup_type(parameter_ty), TypeKind::Unknown);
    match (argument_unknown, parameter_unknown) {
        (true, true) => ArgumentApplicability::Indeterminate(
            ArgumentIndeterminateReason::UnknownArgumentAndParameter,
        ),
        (true, false) => {
            ArgumentApplicability::Indeterminate(ArgumentIndeterminateReason::UnknownArgument)
        }
        (false, true) => {
            ArgumentApplicability::Indeterminate(ArgumentIndeterminateReason::UnknownParameter)
        }
        (false, false)
            if argument_ty == parameter_ty
                || db.forget_described(argument_ty) == db.forget_described(parameter_ty) =>
        {
            ArgumentApplicability::Exact
        }
        (false, false) if crate::subtype::is_assignable(db, argument_ty, parameter_ty) => {
            ArgumentApplicability::Assignable
        }
        (false, false) if crate::subtype::is_coercible_to(db, argument_ty, parameter_ty) => {
            ArgumentApplicability::ArgumentCoercion
        }
        (false, false) => ArgumentApplicability::Incompatible,
    }
}

fn parameter_for_argument(
    signature: &CallSignature,
    layout: ParameterLayout,
    argument_index: usize,
) -> ArgumentParameter {
    let parameter_index = match signature.params.get(argument_index) {
        Some(_) => match layout {
            ParameterLayout::MalformedVariadic { first_index } if argument_index >= first_index => {
                return ArgumentParameter::MissingMetadata;
            }
            ParameterLayout::Fixed
            | ParameterLayout::VariadicTail { .. }
            | ParameterLayout::MalformedVariadic { .. } => argument_index,
        },
        None => match layout {
            ParameterLayout::VariadicTail { index } => index,
            ParameterLayout::Fixed | ParameterLayout::MalformedVariadic { .. } => {
                return ArgumentParameter::MissingMetadata;
            }
        },
    };
    let Some(param) = signature.params.get(parameter_index) else {
        return ArgumentParameter::MissingMetadata;
    };
    ArgumentParameter::Declared { index: parameter_index, mode: param.mode, ty: param.ty }
}

fn parameter_layout(signature: &CallSignature) -> ParameterLayout {
    let mut variadic_indices = signature
        .params
        .iter()
        .enumerate()
        .filter_map(|(index, param)| (param.mode == CallParamMode::Variadic).then_some(index));
    let Some(first_index) = variadic_indices.next() else {
        return ParameterLayout::Fixed;
    };
    if first_index + 1 == signature.params.len() && variadic_indices.next().is_none() {
        ParameterLayout::VariadicTail { index: first_index }
    } else {
        ParameterLayout::MalformedVariadic { first_index }
    }
}
