use bsl_metadata::MdoType;
use bsl_platform::{MethodParam, PlatformMethod};
use bsl_types::builders::Builders;
use bsl_types::facet::{ArgArity, FunctionFacet};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, TypeId};
use hir_def::execution_env::EnvFlags;
use hir_def::module_interface::MethodDecl;
use hir_def::ty::FunctionSignature;
use hir_def::Name;

use super::{
    CallCandidateSet, CallParam, CallParamMode, CallSignature, CandidateId, CandidateOrigin,
    CandidateProvenance, PlatformSignatureSlot, UserMethodId,
};
use crate::field_enum::specialize_self_ref_ty;
use crate::lower::type_string::{lower_param_type_string_typeid, lower_platform_type_name_typeid};

/// The concrete metadata object a platform method is called on. Its bare kind
/// names in parameter position (`СправочникСсылка`) are narrowed to a reference
/// of this very object.
#[derive(Clone, Copy)]
pub(crate) struct ParamReceiver<'a> {
    pub(crate) mdo_type: MdoType,
    pub(crate) name: &'a Name,
    pub(crate) config_id: &'a ConfigId,
}

impl CallCandidateSet {
    pub(crate) fn from_function_facet(facet: &FunctionFacet) -> Self {
        let params = facet
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| CallParam {
                name: param.name.as_str().into(),
                ty: param.ty,
                has_default: facet.defaults.get(index).is_some_and(Option::is_some),
                mode: if param.variadic {
                    CallParamMode::Variadic
                } else {
                    CallParamMode::Positional
                },
            })
            .collect();
        let max_args = match facet.max_args {
            ArgArity::Fixed(count) => Some(count as usize),
            ArgArity::Variadic => None,
            _ => None,
        };
        Self(
            vec![CallSignature {
                id: CandidateId::FunctionValue,
                params,
                required_args: facet.min_args as usize,
                max_args,
                return_ty: facet.returns,
                origin: CandidateOrigin::FunctionValue,
                environment: EnvFlags::EMPTY,
                provenance: CandidateProvenance::FunctionValue,
                from_doc_comment: false,
            }]
            .into_boxed_slice(),
        )
    }

    pub(crate) fn from_platform_method(
        db: &dyn TypeKernelDb,
        method: &PlatformMethod,
        return_ty: TypeId,
        receiver: Option<ParamReceiver<'_>>,
    ) -> Self {
        let context = PlatformCandidateContext {
            db,
            method,
            return_ty,
            receiver,
            environment: EnvFlags::from_platform_context(method.context.as_ref()),
        };
        let mut candidates = Vec::with_capacity(method.variants.len() + 1);
        candidates.push(context.lower(PlatformSignatureSlot::Base, &method.parameters));
        candidates.extend(method.variants.iter().enumerate().map(|(ordinal, variant)| {
            context.lower(PlatformSignatureSlot::Variant(ordinal), &variant.parameters)
        }));
        Self(candidates.into_boxed_slice())
    }
}

impl CallSignature {
    pub(crate) fn from_user_method(
        method: &MethodDecl,
        signature: &FunctionSignature,
        return_ty: TypeId,
        environment: EnvFlags,
    ) -> Self {
        let params = method
            .params
            .iter()
            .zip(signature.params.iter().copied())
            .map(|(param, ty)| CallParam {
                name: param.name.as_str().into(),
                ty,
                has_default: param.has_default,
                mode: CallParamMode::Positional,
            })
            .collect();
        let method_id = UserMethodId::from(method.id);
        Self {
            id: CandidateId::User { method: method_id, signature_ordinal: 0 },
            params,
            required_args: signature.required_count(),
            max_args: signature.max_args.map(|count| count as usize),
            return_ty,
            origin: CandidateOrigin::User,
            environment,
            provenance: CandidateProvenance::UserMethod(method_id),
            from_doc_comment: signature.from_doc_comment,
        }
    }
}

struct PlatformCandidateContext<'a> {
    db: &'a dyn TypeKernelDb,
    method: &'a PlatformMethod,
    return_ty: TypeId,
    receiver: Option<ParamReceiver<'a>>,
    environment: EnvFlags,
}

impl PlatformCandidateContext<'_> {
    fn lower_param_type(&self, raw: &str) -> TypeId {
        // Only a concrete receiver can say which object a bare kind name stands
        // for; without one the slot stays as permissive as the plain lowering.
        if let Some(receiver) = self.receiver {
            if !receiver.name.as_str().is_empty() && !raw.contains(',') {
                let bare = lower_platform_type_name_typeid(self.db, raw.trim());
                if let Some(narrowed) = specialize_self_ref_ty(
                    self.db,
                    receiver.mdo_type,
                    receiver.name,
                    receiver.config_id,
                    bare,
                ) {
                    // The platform accepts `Неопределено` where it takes a reference.
                    return self.db.union(vec![narrowed, self.db.undefined()]);
                }
            }
        }
        lower_param_type_string_typeid(self.db, raw)
    }

    fn lower(&self, signature: PlatformSignatureSlot, parameters: &[MethodParam]) -> CallSignature {
        let params: Box<[CallParam]> = parameters
            .iter()
            .map(|param| CallParam {
                name: param.name.clone(),
                ty: param
                    .param_type
                    .as_deref()
                    .map(|raw| self.lower_param_type(raw))
                    .unwrap_or_else(|| self.db.unknown()),
                has_default: param.is_optional,
                mode: if param.is_variadic {
                    CallParamMode::Variadic
                } else {
                    CallParamMode::Positional
                },
            })
            .collect();
        let required_args = params
            .iter()
            .rposition(|param| !param.has_default && param.mode == CallParamMode::Positional)
            .map_or(0, |index| index + 1);
        let max_args = params
            .iter()
            .all(|param| param.mode == CallParamMode::Positional)
            .then_some(params.len());
        let id = CandidateId::Platform { method_id: self.method.id, signature };
        CallSignature {
            id,
            params,
            required_args,
            max_args,
            return_ty: self.return_ty,
            origin: CandidateOrigin::Platform,
            environment: self.environment,
            provenance: CandidateProvenance::PlatformMethod {
                method_id: self.method.id,
                signature,
            },
            from_doc_comment: false,
        }
    }
}
