use bsl_platform::PlatformConstructor;
use bsl_types::intern::TypeKernelDb;
use hir_def::execution_env::EnvFlags;
use rustc_hash::FxHashMap;
use stdx::case::CaseExt;

use super::{
    descriptor_from_params, descriptors_from_global_function, register_fallbacks, BuiltinFunctions,
    BuiltinSignature, ReturnTypeSpec, BUILTIN_FUNCTIONS,
};
use crate::{
    BuiltinCallableId, CallCandidateSet, CallParam, CallParamMode, CallSignature, CandidateId,
    CandidateOrigin, CandidateProvenance, DuplicateCandidateId, PlatformSignatureSlot,
};

#[cfg(test)]
mod tests;

pub fn builtin_functions() -> &'static BuiltinFunctions {
    BUILTIN_FUNCTIONS.get_or_init(BuiltinFunctions::new)
}

pub(crate) fn constructor_candidates(
    db: &dyn TypeKernelDb,
    constructors: &[&PlatformConstructor],
    environment: EnvFlags,
) -> Result<CallCandidateSet, DuplicateCandidateId> {
    CallCandidateSet::try_from(
        constructors
            .iter()
            .map(|constructor| {
                descriptor_from_params(&constructor.parameters, ReturnTypeSpec::Unknown)
                    .to_constructor_call_signature(db, constructor, environment)
            })
            .collect::<Vec<_>>(),
    )
}

impl BuiltinSignature {
    pub(crate) fn to_call_signature(
        &self,
        db: &dyn TypeKernelDb,
        callable: BuiltinCallableId,
        signature_ordinal: usize,
    ) -> CallSignature {
        let signature = self.lower(db);
        let params = self
            .names
            .iter()
            .zip(signature.params.iter().copied())
            .enumerate()
            .map(|(index, (name, ty))| CallParam {
                name: name.clone(),
                ty,
                has_default: signature.defaults.get(index).copied().unwrap_or(false),
                mode: if self.variadic_param == Some(index) {
                    CallParamMode::Variadic
                } else {
                    CallParamMode::Positional
                },
                // Global platform functions reach the resolver as builtin signatures, not through
                // `PlatformMethod`; the fact is carried along or it never gets here.
                passing: self.passing.get(index).copied().flatten(),
                result_follows_argument_text: self
                    .result_follows_argument_text
                    .get(index)
                    .copied()
                    .unwrap_or(false),
            })
            .collect();
        CallSignature {
            id: CandidateId::Builtin { callable, signature_ordinal },
            params,
            required_args: signature.required_count(),
            max_args: signature.max_args.map(|count| count as usize),
            return_ty: signature.ret,
            origin: CandidateOrigin::Builtin,
            environment: self.env,
            provenance: CandidateProvenance::Builtin(callable),
            from_doc_comment: false,
        }
    }

    fn to_constructor_call_signature(
        &self,
        db: &dyn TypeKernelDb,
        constructor: &PlatformConstructor,
        environment: EnvFlags,
    ) -> CallSignature {
        let mut signature = self.to_call_signature(db, BuiltinCallableId::Intrinsic(0), 0);
        let id = CandidateId::Platform {
            method_id: constructor.id,
            signature: PlatformSignatureSlot::Base,
        };
        signature.id = id;
        signature.origin = CandidateOrigin::Platform;
        signature.environment = environment;
        signature.provenance = CandidateProvenance::PlatformMethod {
            method_id: constructor.id,
            signature: PlatformSignatureSlot::Base,
        };
        signature
    }
}

impl BuiltinFunctions {
    pub(super) fn new() -> Self {
        let mut signatures: FxHashMap<String, Vec<BuiltinSignature>> = FxHashMap::default();
        let mut callable_ids = FxHashMap::default();

        let platform = bsl_platform::PlatformData::instance();
        for func in platform.all_global_functions() {
            let sigs = descriptors_from_global_function(func);
            let id = BuiltinCallableId::PlatformGlobal(func.id);
            let ru = func.name.fold_lower();
            let en = func.english_name.fold_lower();
            signatures.insert(ru.clone(), sigs.clone());
            signatures.insert(en.clone(), sigs);
            callable_ids.insert(ru, id);
            callable_ids.insert(en, id);
        }

        for (ru, legacy_en) in bsl_platform::LEGACY_GLOBAL_FUNCTION_EN_ALIASES {
            if let Some(sigs) = signatures.get(&ru.fold_lower()).cloned() {
                signatures.entry(legacy_en.fold_lower()).or_insert(sigs);
            }
            if let Some(id) = callable_ids.get(&ru.fold_lower()).copied() {
                callable_ids.entry(legacy_en.fold_lower()).or_insert(id);
            }
        }

        register_fallbacks(&mut signatures);
        for (ordinal, (ru, en)) in
            [("новый", "new"), ("описаниетипов", "typedescription")].into_iter().enumerate()
        {
            let id = BuiltinCallableId::Intrinsic(ordinal as u32);
            callable_ids.entry(ru.to_string()).or_insert(id);
            callable_ids.entry(en.to_string()).or_insert(id);
        }

        tracing::debug!("initialized {} built-in function signature keys", signatures.len());

        Self { signatures, callable_ids }
    }

    pub fn get(&self, name: &str) -> Option<&[BuiltinSignature]> {
        let name_lower = name.fold_lower();
        self.signatures.get(&name_lower).map(Vec::as_slice)
    }

    pub fn callable_id(&self, name: &str) -> Option<BuiltinCallableId> {
        self.callable_ids.get(&name.fold_lower()).copied()
    }

    pub fn canonical_name(&self, callable: BuiltinCallableId) -> Option<&str> {
        self.callable_ids
            .iter()
            .filter_map(|(name, id)| (*id == callable).then_some(name.as_str()))
            .min()
    }
}
