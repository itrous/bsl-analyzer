use std::fmt;

use bsl_types::kind::TypeId;
use hir_def::execution_env::EnvFlags;
use smol_str::SmolStr;
use vfs::FileId;

mod applicability;
mod candidate_builders;
pub(crate) use candidate_builders::ParamReceiver;
mod candidate_set;
mod resolution;

pub use applicability::{
    evaluate_applicability, evaluate_arity, ArgumentApplicability, ArgumentEvaluation,
    ArgumentIndeterminateReason, ArgumentParameter, ArityMismatch, ArityUsage,
    CandidateApplicability, CandidateEvaluation,
};
pub use resolution::{
    resolve_candidates, ArityFallback, CallRejection, CallResolution, CallSelection,
    CandidateDisposition, CandidateFact, CandidateRejection, CandidateScore, TypeFallback,
};

/// Stable position of a platform signature within one method record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformSignatureSlot {
    /// The method record's own signature.
    Base,
    /// A signature from `PlatformMethod::variants`, indexed in source order.
    Variant(usize),
}

/// Stable workspace method identity independent of filesystem paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserMethodId {
    file_id: FileId,
    local_id: hir_def::MethodKey,
}

impl UserMethodId {
    /// Creates an identity from a Salsa-stable file id and the method's key.
    pub const fn new(file_id: FileId, local_id: hir_def::MethodKey) -> Self {
        Self { file_id, local_id }
    }
}

impl From<hir_def::MethodId> for UserMethodId {
    fn from(method: hir_def::MethodId) -> Self {
        Self::new(method.module.file_id, method.local_id)
    }
}

impl From<UserMethodId> for hir_def::MethodId {
    fn from(method: UserMethodId) -> Self {
        Self { module: hir_def::ModuleId::new(method.file_id), local_id: method.local_id }
    }
}

/// Stable identity of a built-in callable's defining registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuiltinCallableId {
    /// A global function from the platform database.
    PlatformGlobal(u32),
    /// A language intrinsic from the analyzer's numeric registry.
    Intrinsic(u32),
}

/// Semantic identity of one callable signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidateId {
    Platform { method_id: u32, signature: PlatformSignatureSlot },
    User { method: UserMethodId, signature_ordinal: usize },
    Builtin { callable: BuiltinCallableId, signature_ordinal: usize },
    FunctionValue,
}

impl CandidateId {
    pub const fn is_platform(self) -> bool {
        matches!(self, Self::Platform { .. })
    }

    pub const fn is_user(self) -> bool {
        matches!(self, Self::User { .. })
    }

    pub const fn is_builtin(self) -> bool {
        matches!(self, Self::Builtin { .. })
    }
}

/// Layer that supplied a call candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateOrigin {
    Platform,
    User,
    Builtin,
    FunctionValue,
}

/// Stable source record from which a candidate was lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateProvenance {
    PlatformMethod { method_id: u32, signature: PlatformSignatureSlot },
    UserMethod(UserMethodId),
    Builtin(BuiltinCallableId),
    FunctionValue,
}

/// How a parameter contributes to the call's arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallParamMode {
    Positional,
    Variadic,
}

/// One lowered call parameter with its source name and arity metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallParam {
    pub name: SmolStr,
    pub ty: TypeId,
    pub has_default: bool,
    pub mode: CallParamMode,
    /// The platform corpus's fact about how this parameter is passed; `None` is unknown and is
    /// all that anything but a platform signature carries.
    pub passing: Option<bsl_platform::ParamPassing>,
    /// The platform corpus's fact that the call's result is the text of this argument or text
    /// derived from it preserving what it says; `false` is "not known to", not "known not to".
    pub result_follows_argument_text: bool,
}

/// Complete, Salsa-safe signature metadata for one call candidate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallSignature {
    pub id: CandidateId,
    pub params: Box<[CallParam]>,
    pub required_args: usize,
    pub max_args: Option<usize>,
    pub return_ty: TypeId,
    pub origin: CandidateOrigin,
    pub environment: EnvFlags,
    pub provenance: CandidateProvenance,
    pub from_doc_comment: bool,
}

impl CallSignature {
    /// Returns the variadic element parameter when the source declares one.
    pub fn variadic_param(&self) -> Option<&CallParam> {
        self.params.iter().find(|param| param.mode == CallParamMode::Variadic)
    }
}

/// Deterministically ordered signatures with unique semantic identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallCandidateSet(Box<[CallSignature]>);

impl CallCandidateSet {
    /// Returns candidates in stable identity order.
    pub fn as_slice(&self) -> &[CallSignature] {
        &self.0
    }

    pub(crate) fn signatures_mut(&mut self) -> &mut [CallSignature] {
        &mut self.0
    }
}

/// Error returned when two signatures claim the same semantic identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateCandidateId {
    pub id: CandidateId,
}

impl fmt::Display for DuplicateCandidateId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "duplicate call candidate identity: {:?}", self.id)
    }
}

impl std::error::Error for DuplicateCandidateId {}

#[cfg(test)]
mod tests;
