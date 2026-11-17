use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};

use super::{
    evaluate_applicability, ArgumentApplicability, ArgumentEvaluation, ArityMismatch,
    CallCandidateSet, CallSignature, CandidateApplicability, CandidateEvaluation, CandidateId,
};

/// Ordered semantic cost of a viable candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CandidateScore {
    pub indeterminate_count: usize,
    pub coercion_count: usize,
    pub assignable_count: usize,
    pub defaults_used: usize,
    pub variadic_used: usize,
}

/// Concrete reason a candidate cannot survive resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateRejection {
    Arity(ArityMismatch),
    TypeIncompatible { arguments: Box<[ArgumentEvaluation]> },
}

/// Candidate status after semantic ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateDisposition {
    Survivor,
    LowerRanked,
    Rejected(CandidateRejection),
}

/// Evaluation, score, and outcome retained for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFact {
    pub id: CandidateId,
    pub return_ty: TypeId,
    pub evaluation: CandidateEvaluation,
    pub score: Option<CandidateScore>,
    pub disposition: CandidateDisposition,
}

/// Candidate chosen only to display an arity rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArityFallback {
    pub candidate: CandidateId,
    pub mismatch: ArityMismatch,
    pub distance: usize,
}

/// Candidate and argument chosen only to display a type rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeFallback {
    pub candidate: CandidateId,
    pub argument: ArgumentEvaluation,
}

/// Why no candidate survived resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallRejection {
    NoCandidates,
    Arity { fallback: ArityFallback },
    Type,
}

/// Semantic selection state of the candidate set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSelection {
    Unique { candidate: CandidateId },
    Ambiguous { candidates: Box<[CandidateId]> },
    Rejected(CallRejection),
}

impl CallSelection {
    /// The one callee the call settles on: the unique candidate, or the sole survivor of a
    /// selection that stayed undecided (an undocumented parameter leaves it `Ambiguous`).
    /// `None` when more than one callee is possible - then the call proves nothing.
    pub(crate) fn sole_candidate(&self) -> Option<CandidateId> {
        match self {
            CallSelection::Unique { candidate } => Some(*candidate),
            CallSelection::Ambiguous { candidates } => match candidates.as_ref() {
                [only] => Some(*only),
                _ => None,
            },
            CallSelection::Rejected(_) => None,
        }
    }
}

/// Complete deterministic result of resolving one candidate set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResolution {
    pub candidates: Box<[CandidateFact]>,
    pub selection: CallSelection,
    pub return_ty: TypeId,
}

impl CallResolution {
    /// Returns the least-incompatible rejected candidate, with stable identity breaking ties.
    pub fn type_fallback(&self) -> Option<TypeFallback> {
        if self.selection != CallSelection::Rejected(CallRejection::Type) {
            return None;
        }
        self.candidates
            .iter()
            .filter_map(|fact| match &fact.disposition {
                CandidateDisposition::Rejected(CandidateRejection::TypeIncompatible {
                    arguments,
                }) => {
                    let incompatible_count = arguments
                        .iter()
                        .filter(|argument| {
                            argument.applicability == ArgumentApplicability::Incompatible
                        })
                        .count();
                    let first_incompatible = arguments
                        .iter()
                        .find(|argument| {
                            argument.applicability == ArgumentApplicability::Incompatible
                        })
                        .copied()?;
                    Some((incompatible_count, fact.id, first_incompatible))
                }
                CandidateDisposition::Survivor
                | CandidateDisposition::LowerRanked
                | CandidateDisposition::Rejected(CandidateRejection::Arity(_)) => None,
            })
            .min_by_key(|(incompatible_count, candidate, _)| (*incompatible_count, *candidate))
            .map(|(_, candidate, argument)| TypeFallback { candidate, argument })
    }
}

/// Evaluates and ranks a complete candidate set without emitting diagnostics.
pub fn resolve_candidates(
    db: &dyn TypeKernelDb,
    candidates: &CallCandidateSet,
    argument_types: &[TypeId],
) -> CallResolution {
    let mut facts = candidates
        .as_slice()
        .iter()
        .map(|candidate| candidate_fact(db, candidate, argument_types))
        .collect::<Vec<_>>();
    facts.sort_by_key(|fact| fact.id);

    let best_known_score = facts
        .iter()
        .filter(|fact| is_known_applicable(&fact.evaluation))
        .filter_map(|fact| fact.score)
        .min();
    let (survivors, known_selection) = match best_known_score {
        Some(best_score) => (
            facts
                .iter()
                .filter(|fact| {
                    is_known_applicable(&fact.evaluation) && fact.score == Some(best_score)
                })
                .map(|fact| fact.id)
                .collect::<Vec<_>>(),
            true,
        ),
        None => (
            facts
                .iter()
                .filter(|fact| is_indeterminate(&fact.evaluation))
                .map(|fact| fact.id)
                .collect::<Vec<_>>(),
            false,
        ),
    };

    mark_survivors(&mut facts, &survivors);
    let selection = selection(&facts, &survivors, known_selection);
    let return_ty = aggregate_return(db, &facts, &survivors);
    CallResolution { candidates: facts.into_boxed_slice(), selection, return_ty }
}

fn candidate_fact(
    db: &dyn TypeKernelDb,
    candidate: &CallSignature,
    argument_types: &[TypeId],
) -> CandidateFact {
    let evaluation = evaluate_applicability(db, candidate, argument_types);
    let (score, disposition) = match &evaluation {
        CandidateEvaluation::ArityIncompatible(mismatch) => {
            (None, CandidateDisposition::Rejected(CandidateRejection::Arity(*mismatch)))
        }
        CandidateEvaluation::ArityCompatible {
            applicability: CandidateApplicability::Incompatible { arguments },
            ..
        } => (
            None,
            CandidateDisposition::Rejected(CandidateRejection::TypeIncompatible {
                arguments: arguments.clone(),
            }),
        ),
        CandidateEvaluation::ArityCompatible { usage, applicability } => {
            (Some(candidate_score(*usage, applicability)), CandidateDisposition::LowerRanked)
        }
    };
    CandidateFact {
        id: candidate.id,
        return_ty: candidate.return_ty,
        evaluation,
        score,
        disposition,
    }
}

fn candidate_score(
    usage: super::ArityUsage,
    applicability: &CandidateApplicability,
) -> CandidateScore {
    let arguments = match applicability {
        CandidateApplicability::Applicable { arguments }
        | CandidateApplicability::Indeterminate { arguments }
        | CandidateApplicability::Incompatible { arguments } => arguments,
    };
    let mut score = CandidateScore {
        indeterminate_count: 0,
        coercion_count: 0,
        assignable_count: 0,
        defaults_used: usage.defaults_used,
        variadic_used: usage.variadic_arguments,
    };
    for argument in arguments {
        match argument.applicability {
            ArgumentApplicability::Exact => {}
            ArgumentApplicability::Assignable => score.assignable_count += 1,
            ArgumentApplicability::ArgumentCoercion => score.coercion_count += 1,
            ArgumentApplicability::Indeterminate(_) => score.indeterminate_count += 1,
            ArgumentApplicability::Incompatible => {}
        }
    }
    score
}

fn is_known_applicable(evaluation: &CandidateEvaluation) -> bool {
    matches!(
        evaluation,
        CandidateEvaluation::ArityCompatible {
            applicability: CandidateApplicability::Applicable { .. },
            ..
        }
    )
}

fn is_indeterminate(evaluation: &CandidateEvaluation) -> bool {
    matches!(
        evaluation,
        CandidateEvaluation::ArityCompatible {
            applicability: CandidateApplicability::Indeterminate { .. },
            ..
        }
    )
}

fn mark_survivors(facts: &mut [CandidateFact], survivors: &[CandidateId]) {
    for fact in facts {
        if survivors.binary_search(&fact.id).is_ok() {
            fact.disposition = CandidateDisposition::Survivor;
        }
    }
}

fn selection(
    facts: &[CandidateFact],
    survivors: &[CandidateId],
    known_selection: bool,
) -> CallSelection {
    if known_selection && survivors.len() == 1 {
        return CallSelection::Unique { candidate: survivors[0] };
    }
    if !survivors.is_empty() {
        return CallSelection::Ambiguous { candidates: survivors.into() };
    }
    if facts.is_empty() {
        return CallSelection::Rejected(CallRejection::NoCandidates);
    }
    if facts
        .iter()
        .any(|fact| matches!(fact.evaluation, CandidateEvaluation::ArityCompatible { .. }))
    {
        return CallSelection::Rejected(CallRejection::Type);
    }
    match nearest_arity(facts) {
        Some(fallback) => CallSelection::Rejected(CallRejection::Arity { fallback }),
        None => CallSelection::Rejected(CallRejection::NoCandidates),
    }
}

fn nearest_arity(facts: &[CandidateFact]) -> Option<ArityFallback> {
    facts
        .iter()
        .filter_map(|fact| match fact.evaluation {
            CandidateEvaluation::ArityIncompatible(mismatch) => Some(ArityFallback {
                candidate: fact.id,
                mismatch,
                distance: arity_distance(mismatch),
            }),
            CandidateEvaluation::ArityCompatible { .. } => None,
        })
        .min_by_key(|fallback| (fallback.distance, fallback.candidate))
}

const fn arity_distance(mismatch: ArityMismatch) -> usize {
    match mismatch {
        ArityMismatch::TooFew { actual, required } => required - actual,
        ArityMismatch::TooMany { actual, maximum } => actual - maximum,
    }
}

fn aggregate_return(
    db: &dyn TypeKernelDb,
    facts: &[CandidateFact],
    survivors: &[CandidateId],
) -> TypeId {
    let returns = facts
        .iter()
        .filter(|fact| survivors.binary_search(&fact.id).is_ok())
        .map(|fact| fact.return_ty)
        .collect::<Vec<_>>();
    if returns.is_empty()
        || returns.iter().any(|return_ty| matches!(db.lookup_type(*return_ty), TypeKind::Unknown))
    {
        return db.unknown();
    }
    db.union(returns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unique_candidate_is_the_sole_one() {
        let selection = CallSelection::Unique { candidate: CandidateId::FunctionValue };

        assert_eq!(selection.sole_candidate(), Some(CandidateId::FunctionValue));
    }

    #[test]
    fn an_ambiguous_selection_with_one_survivor_is_decided() {
        let selection =
            CallSelection::Ambiguous { candidates: Box::new([CandidateId::FunctionValue]) };

        assert_eq!(selection.sole_candidate(), Some(CandidateId::FunctionValue));
    }

    #[test]
    fn an_ambiguous_selection_with_two_survivors_proves_nothing() {
        let selection = CallSelection::Ambiguous {
            candidates: Box::new([CandidateId::FunctionValue, CandidateId::FunctionValue]),
        };

        assert_eq!(selection.sole_candidate(), None);
    }

    #[test]
    fn an_ambiguous_selection_without_candidates_proves_nothing() {
        let selection = CallSelection::Ambiguous { candidates: Box::new([]) };

        assert_eq!(selection.sole_candidate(), None);
    }

    #[test]
    fn a_rejected_selection_proves_nothing_whatever_the_reason() {
        let fallback = ArityFallback {
            candidate: CandidateId::FunctionValue,
            mismatch: ArityMismatch::TooFew { actual: 0, required: 1 },
            distance: 1,
        };
        for rejection in
            [CallRejection::NoCandidates, CallRejection::Arity { fallback }, CallRejection::Type]
        {
            assert_eq!(CallSelection::Rejected(rejection).sole_candidate(), None);
        }
    }
}
