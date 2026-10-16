use crate::{handlers, Diagnostic, DiagnosticCode, DiagnosticsContext};

pub(crate) const METADATA_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::CommonModuleInvalidType,
    DiagnosticCode::CommonModuleNameClient,
    DiagnosticCode::CommonModuleNameGlobal,
    DiagnosticCode::CommonModuleNameCached,
    DiagnosticCode::CommonModuleNameClientServer,
    DiagnosticCode::CommonModuleNameFullAccess,
    DiagnosticCode::CommonModuleNameGlobalClient,
    DiagnosticCode::CommonModuleNameServerCall,
    DiagnosticCode::CommonModuleNameWords,
    DiagnosticCode::ExportVariables,
    DiagnosticCode::SameMetadataObjectAndChildNames,
    DiagnosticCode::DenyIncompleteValues,
    DiagnosticCode::ForbiddenMetadataName,
    DiagnosticCode::MetadataObjectNameLength,
    DiagnosticCode::MissingFormEventHandler,
    DiagnosticCode::WrongDataPathForFormElements,
    DiagnosticCode::WrongHttpServiceHandler,
    DiagnosticCode::WrongWebServiceHandler,
];

pub fn collect_metadata_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(METADATA_DIAGNOSTICS) {
        return Vec::new();
    }

    let module_bodies = ctx.module_bodies();

    let mut diagnostics = Vec::new();

    let metadata = ctx.module_metadata();
    let metadata_ref = metadata.as_ref();

    diagnostics.extend(handlers::common_module_invalid_type::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_client::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_global::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_cached::from_metadata(metadata_ref, ctx));

    diagnostics
        .extend(handlers::common_module_name_client_server::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_full_access::from_metadata(metadata_ref, ctx));

    diagnostics
        .extend(handlers::common_module_name_global_client::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_server_call::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::common_module_name_words::from_metadata(metadata_ref, ctx));

    for var in module_bodies.module_vars() {
        if var.is_export {
            if let Some(diag) = handlers::export_variables::from_hir(&var.name, var.range, ctx) {
                diagnostics.push(diag);
            }
        }
    }

    diagnostics
        .extend(handlers::same_metadata_object_and_child_names::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::deny_incomplete_values::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::forbidden_metadata_name::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::metadata_object_name_length::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::missing_form_event_handler::from_metadata(metadata_ref, ctx));

    diagnostics
        .extend(handlers::wrong_data_path_for_form_elements::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::wrong_http_service_handler::from_metadata(metadata_ref, ctx));

    diagnostics.extend(handlers::wrong_web_service_handler::from_metadata(metadata_ref, ctx));

    diagnostics
}
