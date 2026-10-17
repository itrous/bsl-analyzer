pub mod arg_diagnostics;
pub mod builtin;
mod call_binding;
pub mod call_resolution;
pub mod compat_mode;
pub mod db;
pub mod doc_see;
pub mod field_enum;
pub mod field_lookup;
pub mod form_attr;
pub mod form_items;
pub mod form_self;
pub mod infer;
pub mod iteration_lookup;
pub mod lower;
pub mod manager_lookup;
pub mod metadata_path;
mod method_environment;
pub mod method_graph;
pub mod method_lookup;
pub mod method_resolution;
pub mod min_platform;
pub mod module_implicit;
pub mod narrow;
pub mod object_resolver;
pub mod platform_global_lookup;
pub mod platform_manager_lookup;
pub mod platform_property_lookup;
pub mod platform_resolution;
pub mod platform_type_name;
pub mod proc_signature;
pub mod proc_signature_lookup;
pub mod query_text_dataflow;
pub mod query_unload_refinement;
pub mod sdbl_bridge;
pub mod structure_keys;
pub mod structure_param_keys;
pub mod subtype;
pub mod this_object;
pub mod this_object_attr;
mod type_literal;
mod user_call_candidates;

pub use bsl_config::VisibleConfig;
pub use call_resolution::{
    ArityFallback, BuiltinCallableId, CallCandidateSet, CallParam, CallParamMode, CallRejection,
    CallResolution, CallSelection, CallSignature, CandidateDisposition, CandidateFact, CandidateId,
    CandidateOrigin, CandidateProvenance, CandidateRejection, CandidateScore, DuplicateCandidateId,
    PlatformSignatureSlot, TypeFallback, UserMethodId,
};
pub use field_enum::{enumerate_fields, FieldInfo, FieldOrigin};
pub use field_lookup::lookup_field;
pub use form_items::{
    is_form_items_collection_ty, lower_form_element_for_file, FORM_ITEMS_TYPE_EN,
    FORM_ITEMS_TYPE_RU,
};
pub use form_self::managed_form_platform_type_names;
pub use hir_def::ty::{
    form_control_platform_type_chain, form_control_platform_type_name, form_element_kind_label,
    form_element_kind_sort_band, FormDataKind, FormElementKind, FunctionSignature, MetadataKind,
};
pub use hir_def::type_ref::{BuiltinTypeRef, TypeRef};
pub use hir_def::ConfigsDatabase;
pub use infer::{
    BareNameGap, BareNameOrigin, BareNameResolution, BareNameUse, BodyInferenceResult,
    CallArgBinding, CandidateCallBinding, EnvCalleeKind, EnvMemberKind, ImplicitLocalAssignment,
    ImplicitLocalInfo, InferOwnerResult, InferenceContext, InferenceDiagnostic, InferenceResult,
    ModuleCodeInferenceResult, UnresolvedMethodKind,
};
pub use lower::TyLoweringContext;
pub use manager_lookup::{lookup_manager_field, ManagerMemberInfo};
pub use method_lookup::{lookup_method, MethodInfo};
pub use method_resolution::{resolve_qualified_call, MethodResolution};
pub use module_implicit::module_implicit_fields;
pub use object_resolver::{
    ConfigsObjectResolver, DbObjectResolver, MetadataResolution, ObjectResolver,
};
pub use platform_global_lookup::{
    bare_global_name_claim, manager_collection_env, resolve_platform_global_property_type,
    BareGlobalClaim, BodyShadowScope,
};
pub use platform_manager_lookup::{
    platform_methods_for_manager, platform_methods_for_metadata_kind,
    resolve_platform_manager_method, resolve_platform_metadata_ref_method,
    PlatformMethodResolution,
};
pub use platform_property_lookup::{lookup_platform_property, PlatformPropertyResolution};
pub use platform_resolution::{
    resolve_method, PlatformMethodHandle, PlatformMethodOrigin, ResolvedPlatformMethod,
};
pub use subtype::{is_assignable, is_coercible_to, is_ref_ty};
