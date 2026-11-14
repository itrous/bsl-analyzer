use bsl_types::kind::TypeId;
use bsl_types::testing::InMemoryDb;
use hir_def::execution_env::EnvFlags;

use super::super::{
    BuiltinCallableId, CallParam, CallParamMode, CallSignature, CandidateId, CandidateOrigin,
    CandidateProvenance,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct AritySpec {
    pub required: usize,
    pub maximum: Option<usize>,
}

pub(super) struct CandidateSpec {
    pub ordinal: usize,
    pub params: Vec<CallParam>,
    pub arity: AritySpec,
    pub return_ty: TypeId,
}

pub(super) fn candidate(spec: CandidateSpec) -> CallSignature {
    CallSignature {
        id: CandidateId::Builtin {
            callable: BuiltinCallableId::Intrinsic(80),
            signature_ordinal: spec.ordinal,
        },
        params: spec.params.into_boxed_slice(),
        required_args: spec.arity.required,
        max_args: spec.arity.maximum,
        return_ty: spec.return_ty,
        origin: CandidateOrigin::Builtin,
        environment: EnvFlags::EMPTY,
        provenance: CandidateProvenance::Builtin(BuiltinCallableId::Intrinsic(80)),
        from_doc_comment: false,
    }
}

pub(super) fn signature(
    db: &InMemoryDb,
    params: Vec<CallParam>,
    arity: AritySpec,
) -> CallSignature {
    candidate(CandidateSpec { ordinal: 0, params, arity, return_ty: db.undefined() })
}

pub(super) fn positional(name: &str, ty: TypeId, has_default: bool) -> CallParam {
    CallParam { name: name.into(), ty, has_default, mode: CallParamMode::Positional, passing: None }
}

pub(super) fn variadic(name: &str, ty: TypeId) -> CallParam {
    CallParam {
        name: name.into(),
        ty,
        has_default: false,
        mode: CallParamMode::Variadic,
        passing: None,
    }
}
