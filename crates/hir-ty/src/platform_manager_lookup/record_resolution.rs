use bsl_metadata::MdoType;
use bsl_platform::PlatformMethod;
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, TypeId};
use hir_def::ty::FunctionSignature;
use hir_def::Name;

use super::{
    map_generic_metadata_return_type, map_generic_metadata_return_type_typeid,
    PlatformMethodRecordResolution,
};
use crate::call_resolution::{CallCandidateSet, ParamReceiver};
use crate::lower::type_string::{lower_param_type_string_typeid, lower_platform_type_name_typeid};
use crate::method_lookup::lower_overloads_typeid;

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
                .map(|t| lower_param_type_string_typeid(db, t))
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
        overloads: lower_overloads_typeid(db, method),
        env: hir_def::execution_env::EnvFlags::from_platform_context(method.context.as_ref()),
        candidates: CallCandidateSet::from_platform_method(
            db,
            method,
            return_ty,
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
                .map(|t| lower_param_type_string_typeid(db, t))
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
        overloads: lower_overloads_typeid(db, method),
        env: hir_def::execution_env::EnvFlags::from_platform_context(method.context.as_ref()),
        candidates: CallCandidateSet::from_platform_method(db, method, return_ty, None),
    }
}
