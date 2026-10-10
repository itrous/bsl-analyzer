use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;

use crate::call_resolution::{
    resolve_candidates, CallCandidateSet, CallResolution, CallSelection, CandidateDisposition,
    CandidateId,
};
use crate::infer::CandidateCallBinding;

pub(crate) struct BindingProjection {
    pub semantic: CandidateCallBinding,
}

impl CallResolution {
    pub fn unique_candidate(&self) -> Option<CandidateId> {
        match self.selection {
            CallSelection::Unique { candidate } => Some(candidate),
            CallSelection::Ambiguous { .. } | CallSelection::Rejected(_) => None,
        }
    }

    pub fn is_survivor(&self, candidate: CandidateId) -> bool {
        self.candidates
            .iter()
            .any(|fact| fact.id == candidate && fact.disposition == CandidateDisposition::Survivor)
    }
}

pub(crate) fn resolve_binding(
    db: &dyn TypeKernelDb,
    candidates: CallCandidateSet,
    argument_types: &[TypeId],
) -> BindingProjection {
    let resolution = resolve_candidates(db, &candidates, argument_types);
    BindingProjection { semantic: CandidateCallBinding { candidates, resolution } }
}

#[cfg(test)]
mod tests {
    use bsl_types::testing::InMemoryDb;

    use super::{resolve_binding, CallCandidateSet};
    use crate::builtin::builtin_functions;

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn capped_builtin_uses_declared_params_for_compatibility_arity() {
        let db = InMemoryDb::new();
        let builtins = builtin_functions();
        let callable = builtins.callable_id("StrTemplate").expect("StrTemplate must be registered");
        let candidates = CallCandidateSet::try_from(
            builtins
                .get("StrTemplate")
                .expect("StrTemplate signature must be registered")
                .iter()
                .enumerate()
                .map(|(ordinal, signature)| signature.to_call_signature(&db, callable, ordinal))
                .collect::<Vec<_>>(),
        )
        .expect("builtin signatures must have unique identities");

        assert_eq!(candidates.as_slice().len(), 1);
        assert_eq!(candidates.as_slice()[0].max_args, Some(11));
        assert_eq!(candidates.as_slice()[0].params.len(), 2);

        let projection = resolve_binding(&db, candidates, &[]);
        let signature = projection
            .semantic
            .candidates
            .as_slice()
            .first()
            .expect("StrTemplate has one signature");
        assert_eq!((signature.required_args, signature.params.len()), (1, 2));
    }
}
