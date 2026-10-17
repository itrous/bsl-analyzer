use crate::{
    handlers, AnalysisContext, BodyContext, Diagnostic, DiagnosticCode, DiagnosticsContext,
};
use bsl_platform::security::Category as SecurityCategory;
use hir::{
    BodySourceMap, DefWithBodyId, ExprId, InferenceDiagnostic, LocalRange, RedundantAccessKind,
};

pub(crate) const INFERENCE_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::UnresolvedName,
    DiagnosticCode::UnresolvedMethodCall,
    DiagnosticCode::UnresolvedBareCall,
    DiagnosticCode::MismatchedArgCount,
    DiagnosticCode::TypeMismatch,
    DiagnosticCode::TypeMismatchByDocComment,
    DiagnosticCode::UnresolvedField,
    DiagnosticCode::UnresolvedMetadataPath,
    DiagnosticCode::ReadOnlyPropertyAssignment,
    DiagnosticCode::GlobalPropertyNotWritable,
    DiagnosticCode::DeprecatedPlatformApi,
    DiagnosticCode::RedundantAccessToObject,
    DiagnosticCode::MissedRequiredParameter,
    DiagnosticCode::UnavailableInEnvironment,
    DiagnosticCode::PlatformMemberNewerThanMinVersion,
    DiagnosticCode::PlatformMemberHiddenByCompatibilityMode,
    DiagnosticCode::ModuleAccessibility,
    DiagnosticCode::ExternalAppStarting,
    DiagnosticCode::FileSystemAccess,
];

/// The body's own inference diagnostics, in the body's coordinates.
pub fn collect_body_inference_diagnostics(
    ctx: &BodyContext,
    acc: &mut Vec<Diagnostic<LocalRange>>,
) {
    if !ctx.config.any_enabled(INFERENCE_DIAGNOSTICS) {
        return;
    }
    let infer = ctx.infer();
    dispatch_local(ctx, ctx.source_map(), infer.diagnostics(), acc);
}

pub fn collect_body_arg_diagnostics(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    if !ctx.config.any_enabled(ARG_DIAGNOSTICS) {
        return;
    }
    let arg_diags = ctx.arg_diagnostics();
    dispatch_local(ctx, ctx.source_map(), &arg_diags, acc);
}

const ARG_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::MismatchedArgCount,
    DiagnosticCode::TypeMismatch,
    DiagnosticCode::TypeMismatchByDocComment,
];

/// The base-aware passes of the extension merge re-infer a whole module
/// through a weaving or effective provider; their diagnostics come as a file
/// fold and are placed body by body through the fold's own source maps.
pub fn collect_inference_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(INFERENCE_DIAGNOSTICS) {
        return Vec::new();
    }
    let infer = ctx.infer();
    if infer.diagnostics.is_empty() {
        return Vec::new();
    }
    let module_bodies = ctx.module_bodies();
    let mut diagnostics = Vec::new();
    for (owner, diag) in &infer.diagnostics {
        let lower = match owner {
            DefWithBodyId::Method(local_id) => module_bodies.lower_result(*local_id),
            DefWithBodyId::ModuleCode => module_bodies.module_code_result(),
        };
        let Some(lower) = lower else {
            tracing::debug!(
                ?owner,
                ?diag,
                "dropping inference diagnostic: owning body has no lowering"
            );
            continue;
        };
        let mut local = Vec::new();
        dispatch_local(ctx, lower.source_map().local(), std::slice::from_ref(diag), &mut local);
        diagnostics.extend(local.into_iter().map(|d| d.lift(lower.base)));
    }
    diagnostics
}

fn dispatch_local(
    ctx: &AnalysisContext,
    source_map: &BodySourceMap,
    diags: &[InferenceDiagnostic],
    acc: &mut Vec<Diagnostic<LocalRange>>,
) {
    for diag in diags {
        let Some(range) = source_map.expr_range(diagnostic_expr(diag)) else {
            tracing::debug!(?diag, "dropping inference diagnostic: ExprId has no range");
            continue;
        };
        if let Some(d) = dispatch_inference_diagnostic(diag, range, ctx) {
            acc.push(d);
        }
    }
}

fn diagnostic_expr(diag: &InferenceDiagnostic) -> ExprId {
    match diag {
        InferenceDiagnostic::UnresolvedName { expr, .. } => *expr,
        InferenceDiagnostic::UnresolvedMethodCall { expr, .. } => *expr,
        InferenceDiagnostic::UnresolvedBareCall { expr, .. } => *expr,
        InferenceDiagnostic::MismatchedArgCount { call_expr, .. } => *call_expr,
        InferenceDiagnostic::TypeMismatch { expr, .. } => *expr,
        InferenceDiagnostic::UnresolvedField { expr, .. } => *expr,
        InferenceDiagnostic::UnresolvedMetadataPath { expr, .. } => *expr,
        InferenceDiagnostic::ReadOnlyPropertyAssignment { lhs, .. } => *lhs,
        InferenceDiagnostic::GlobalPropertyNotWritable { lhs, .. } => *lhs,
        InferenceDiagnostic::DeprecatedPlatformMember { expr, .. } => *expr,
        InferenceDiagnostic::RedundantAccessToObjectTwoLevel { expr, .. } => *expr,
        InferenceDiagnostic::MissedRequiredParameterCommonModule { expr, .. } => *expr,
        InferenceDiagnostic::MissedRequiredParameterManagerModule { expr, .. } => *expr,
        InferenceDiagnostic::RedundantAccessToObjectThreeLevel { expr, .. } => *expr,
        InferenceDiagnostic::UnavailableInEnvironment { expr, .. } => *expr,
        InferenceDiagnostic::PlatformMemberNewerThanMinVersion { expr, .. } => *expr,
        InferenceDiagnostic::PlatformMemberHiddenByCompatibilityMode { expr, .. } => *expr,
        InferenceDiagnostic::ModuleAccessibility { expr, .. } => *expr,
        InferenceDiagnostic::GuardedCall { expr, .. } => *expr,
    }
}

fn dispatch_inference_diagnostic(
    diag: &InferenceDiagnostic,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    match diag {
        InferenceDiagnostic::UnresolvedName { name, .. } => {
            handlers::unresolved_name::from_hir(name, range, ctx)
        }
        InferenceDiagnostic::UnresolvedMethodCall { receiver_name, method_name, kind, .. } => {
            if *kind == hir::UnresolvedMethodKind::ReceiverNameAbsent
                && !ctx.config.is_disabled(DiagnosticCode::UnresolvedName)
            {
                return None;
            }
            handlers::unresolved_method_call::from_hir(
                receiver_name,
                method_name,
                *kind,
                range,
                ctx,
            )
        }
        // Same token, same verdict: the bare callee IS a bare-name read, and
        // `UnresolvedName` reports it whenever it is on. Defer to it rather than
        // stack two errors on one range — the rule `ReceiverNameAbsent` above
        // follows for the qualified spelling.
        InferenceDiagnostic::UnresolvedBareCall { name, .. } => {
            if !ctx.config.is_disabled(DiagnosticCode::UnresolvedName) {
                return None;
            }
            handlers::unresolved_bare_call::from_hir(name, range, ctx)
        }
        InferenceDiagnostic::MismatchedArgCount { required_count, total_count, found, .. } => {
            handlers::mismatched_arg_count::from_hir(
                *required_count,
                *total_count,
                *found,
                range,
                ctx,
            )
        }
        InferenceDiagnostic::TypeMismatch { expected, actual, from_doc_comment, .. } => {
            if *from_doc_comment {
                handlers::type_mismatch_by_doc_comment::from_hir(*expected, *actual, range, ctx)
            } else {
                handlers::type_mismatch::from_hir(*expected, *actual, range, ctx)
            }
        }
        InferenceDiagnostic::UnresolvedField { receiver_ty, field_name, .. } => {
            handlers::unresolved_field::from_hir(*receiver_ty, field_name, range, ctx)
        }
        InferenceDiagnostic::UnresolvedMetadataPath { missing, .. } => {
            handlers::unresolved_metadata_path::from_hir(missing, range, ctx)
        }
        InferenceDiagnostic::ReadOnlyPropertyAssignment { receiver_ty, field_name, .. } => {
            handlers::read_only_property::from_hir(*receiver_ty, field_name, range, ctx)
        }
        InferenceDiagnostic::GlobalPropertyNotWritable { name, .. } => {
            handlers::global_property_not_writable::from_hir(name, range, ctx)
        }
        InferenceDiagnostic::DeprecatedPlatformMember {
            type_name,
            member_name,
            is_property,
            ..
        } => handlers::deprecated_platform_api::from_hir(
            type_name,
            member_name,
            *is_property,
            range,
            ctx,
        ),
        InferenceDiagnostic::RedundantAccessToObjectTwoLevel { module, .. } => {
            let kind = RedundantAccessKind::TwoLevel { module: module.as_str().to_string() };
            handlers::redundant_access_to_object::from_hir(&kind, range, ctx)
        }
        InferenceDiagnostic::MissedRequiredParameterManagerModule {
            callee,
            mdo_type,
            mdo_name,
            args,
            ..
        } => handlers::missed_required_parameter::from_hir(
            callee.as_str(),
            None,
            Some(mdo_type.as_str()),
            Some(mdo_name.as_str()),
            args,
            range,
            ctx,
        ),
        InferenceDiagnostic::RedundantAccessToObjectThreeLevel { mdo_type, mdo_name, .. } => {
            let kind = RedundantAccessKind::ThreeLevel {
                mdo_type: mdo_type.as_str().to_string(),
                mdo_name: mdo_name.as_str().to_string(),
            };
            handlers::redundant_access_to_object::from_hir(&kind, range, ctx)
        }
        InferenceDiagnostic::MissedRequiredParameterCommonModule {
            callee, module, args, ..
        } => handlers::missed_required_parameter::from_hir(
            callee.as_str(),
            Some(module.as_str()),
            None,
            None,
            args,
            range,
            ctx,
        ),
        InferenceDiagnostic::UnavailableInEnvironment { name, member_kind, missing, .. } => {
            handlers::unavailable_in_environment::from_hir(name, *member_kind, *missing, range, ctx)
        }
        InferenceDiagnostic::PlatformMemberNewerThanMinVersion {
            name,
            introduced,
            minimum,
            ..
        } => handlers::platform_member_newer_than_min_version::from_hir(
            name,
            *introduced,
            *minimum,
            range,
            ctx,
        ),
        InferenceDiagnostic::PlatformMemberHiddenByCompatibilityMode {
            name,
            mode,
            visible_from,
            ..
        } => handlers::platform_member_hidden_by_compatibility_mode::from_hir(
            name,
            *mode,
            *visible_from,
            range,
            ctx,
        ),
        InferenceDiagnostic::ModuleAccessibility { name, callee_kind, missing, .. } => {
            handlers::module_accessibility::from_hir(name, *callee_kind, *missing, range, ctx)
        }
        InferenceDiagnostic::GuardedCall { category, .. } => match category {
            SecurityCategory::ExternalApp => handlers::external_app_starting::from_hir(range, ctx),
            SecurityCategory::FileSystem => handlers::file_system_access::from_hir(range, ctx),
            _ => None,
        },
    }
}
