use bsl_metadata::MdoType;
use bsl_platform::{MethodParam, PlatformMethod};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, TypeId};
use hir_def::ty::FunctionSignature;
use hir_def::Name;

use super::{
    map_generic_metadata_return_type, map_generic_metadata_return_type_typeid,
    PlatformMethodRecordResolution,
};
use crate::call_resolution::{CallCandidateSet, CandidateId, ParamReceiver, PlatformSignatureSlot};
use crate::lower::type_string::{lower_param_type_string_typeid, lower_platform_type_name_typeid};

pub(super) fn build_method_record_resolution(
    db: &dyn TypeKernelDb,
    method: &PlatformMethod,
    mdo_type: MdoType,
    mdo_name: &Name,
) -> PlatformMethodRecordResolution {
    let params: Vec<TypeId> = method
        .parameters
        .iter()
        .map(|p| {
            p.param_type
                .as_ref()
                .map(|t| lower_metadata_parameter(db, t, mdo_type))
                .unwrap_or(db.unknown())
        })
        .collect();
    let defaults: Vec<bool> = method.parameters.iter().map(|p| p.is_optional).collect();

    let return_ty = method
        .return_type
        .as_ref()
        .map(|raw| {
            map_generic_metadata_return_type_typeid(db, raw, mdo_type, mdo_name)
                .unwrap_or_else(|| lower_platform_type_name_typeid(db, raw))
        })
        .unwrap_or(db.undefined());

    let signature = FunctionSignature {
        max_args: Some(params.len() as u32),
        params: params.into_boxed_slice(),
        defaults: defaults.into_boxed_slice(),
        ret: return_ty,
        from_doc_comment: false,
        doc_see: Default::default(),
    };
    PlatformMethodRecordResolution {
        method_id: method.id,
        signature,
        return_ty,
        overloads: metadata_overloads(db, method, mdo_type),
        env: hir_def::execution_env::EnvFlags::from_platform_context(method.context.as_ref()),
        candidates: metadata_candidates(
            db,
            method,
            return_ty,
            mdo_type,
            Some(ParamReceiver { mdo_type, name: mdo_name, config_id: &ConfigId::Root }),
        ),
    }
}

pub(super) fn build_any_metadata_ref_method_record(
    db: &dyn TypeKernelDb,
    method: &PlatformMethod,
    parent_mdo: MdoType,
) -> PlatformMethodRecordResolution {
    let params: Vec<TypeId> = method
        .parameters
        .iter()
        .map(|p| {
            p.param_type
                .as_ref()
                .map(|t| lower_metadata_parameter(db, t, parent_mdo))
                .unwrap_or(db.unknown())
        })
        .collect();
    let defaults: Vec<bool> = method.parameters.iter().map(|p| p.is_optional).collect();

    let return_ty = method
        .return_type
        .as_ref()
        .map(|raw| match map_generic_metadata_return_type(raw, parent_mdo) {
            Some(kind) if kind.ref_mdo_type().is_some() => db.any_metadata_ref(parent_mdo),
            Some(_) => db.unknown(),
            None => lower_platform_type_name_typeid(db, raw),
        })
        .unwrap_or(db.undefined());

    let signature = FunctionSignature {
        max_args: Some(params.len() as u32),
        params: params.into_boxed_slice(),
        defaults: defaults.into_boxed_slice(),
        ret: return_ty,
        from_doc_comment: false,
        doc_see: Default::default(),
    };
    PlatformMethodRecordResolution {
        method_id: method.id,
        signature,
        return_ty,
        overloads: metadata_overloads(db, method, parent_mdo),
        env: hir_def::execution_env::EnvFlags::from_platform_context(method.context.as_ref()),
        candidates: metadata_candidates(db, method, return_ty, parent_mdo, None),
    }
}

fn lower_metadata_parameter(db: &dyn TypeKernelDb, raw: &str, mdo_type: MdoType) -> TypeId {
    match map_generic_metadata_return_type(raw, mdo_type) {
        Some(kind) if kind.ref_mdo_type().is_some() => db.any_metadata_ref(mdo_type),
        _ => lower_param_type_string_typeid(db, raw),
    }
}

fn metadata_parameters(
    db: &dyn TypeKernelDb,
    parameters: &[MethodParam],
    mdo_type: MdoType,
) -> Vec<TypeId> {
    parameters
        .iter()
        .map(|parameter| {
            parameter
                .param_type
                .as_deref()
                .map(|raw| lower_metadata_parameter(db, raw, mdo_type))
                .unwrap_or_else(|| db.unknown())
        })
        .collect()
}

fn metadata_overloads(
    db: &dyn TypeKernelDb,
    method: &PlatformMethod,
    mdo_type: MdoType,
) -> Vec<Vec<TypeId>> {
    method
        .variants
        .iter()
        .map(|variant| metadata_parameters(db, &variant.parameters, mdo_type))
        .collect()
}

fn metadata_candidates(
    db: &dyn TypeKernelDb,
    method: &PlatformMethod,
    return_ty: TypeId,
    mdo_type: MdoType,
    receiver: Option<ParamReceiver<'_>>,
) -> CallCandidateSet {
    let mut candidates = CallCandidateSet::from_platform_method(db, method, return_ty, receiver);
    for candidate in candidates.signatures_mut() {
        let parameters = match candidate.id {
            CandidateId::Platform { signature: PlatformSignatureSlot::Base, .. } => {
                &method.parameters
            }
            CandidateId::Platform { signature: PlatformSignatureSlot::Variant(index), .. } => {
                &method.variants[index].parameters
            }
            _ => continue,
        };
        for (parameter, (declared, ty)) in candidate
            .params
            .iter_mut()
            .zip(parameters.iter().zip(metadata_parameters(db, parameters, mdo_type)))
        {
            // A slot the receiver already narrowed to its own reference is stricter
            // than the generic kind and stays as the candidate builder made it.
            let plain =
                declared.param_type.as_deref().map(|raw| lower_param_type_string_typeid(db, raw));
            if plain.is_none_or(|plain| parameter.ty == plain) {
                parameter.ty = ty;
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_platform::MethodVariant;
    use bsl_types::testing::InMemoryDb;

    #[test]
    fn generic_reference_parameter_keeps_all_signature_projections_typed() {
        let db = InMemoryDb::new();
        // Not a parameter name the receiver narrows, so only the generic
        // metadata lowering decides the slot.
        let parameter = |name: &str| MethodParam {
            name: "Основание".into(),
            param_type: Some(name.into()),
            is_optional: false,
            is_variadic: false,
            passing: None,
            result_follows_argument_text: false,
            min_version: None,
        };
        let method = PlatformMethod {
            id: 1,
            type_name: "ChartOfCharacteristicTypesManager.<Name>".into(),
            name: "Метод".into(),
            english_name: "Method".into(),
            return_type: Some("Строка".into()),
            parameters: vec![parameter("ПланВидовХарактеристикСсылка")],
            variants: vec![MethodVariant {
                variant_name: None,
                parameters: vec![
                    parameter("ChartOfCharacteristicTypesRef"),
                    parameter("НеподтвержденныйТип"),
                ],
            }],
            min_version: None,
            context: None,
        };
        let result = build_method_record_resolution(
            &db,
            &method,
            MdoType::ChartOfCharacteristicTypes,
            &Name::new("План"),
        );
        let reference = db.any_metadata_ref(MdoType::ChartOfCharacteristicTypes);
        assert_eq!(&*result.signature.params, &[reference]);
        assert_eq!(result.overloads, [vec![reference, db.unknown()]]);
        assert_eq!(result.candidates.as_slice()[0].params[0].ty, reference);
        assert_eq!(result.candidates.as_slice()[1].params[0].ty, reference);
        assert_eq!(result.candidates.as_slice()[1].params[1].ty, db.unknown());
        assert_eq!(result.candidates.as_slice()[0].environment, result.env);
    }
}
