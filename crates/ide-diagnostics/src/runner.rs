use crate::{handlers, Diagnostic, DiagnosticCode, DiagnosticsContext};

/// Файловые строчные проверки; строчные проверки по плитам методов живут в
/// `crate::slab::SLAB_DIAGNOSTICS` и сюда не входят.
const LINE_DIAGNOSTICS: &[DiagnosticCode] =
    &[DiagnosticCode::ParseError, DiagnosticCode::ConsecutiveEmptyLines];

pub(crate) const SINGLE_PASS_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::UselessTernaryOperator,
    DiagnosticCode::DoubleNegatives,
    DiagnosticCode::UnknownPreprocessorSymbol,
    DiagnosticCode::BadWords,
    DiagnosticCode::Typo,
    DiagnosticCode::NestedTernaryOperator,
    DiagnosticCode::PostfixAccessOnExpression,
    DiagnosticCode::YoLetterUsage,
    DiagnosticCode::MagicDate,
    DiagnosticCode::UsingHardcodePath,
];

const SYNTAX_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::CodeBlockBeforeSub,
    DiagnosticCode::CodeOutOfRegion,
    DiagnosticCode::DuplicateRegion,
    DiagnosticCode::DuplicateStringLiteral,
    DiagnosticCode::EmptyRegion,
    DiagnosticCode::NonStandardRegion,
];

const ITEM_TREE_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::OrderOfParams,
    DiagnosticCode::ReservedParameterNames,
    DiagnosticCode::SeveralCompilerDirectives,
    DiagnosticCode::NonExportMethodsInApiRegion,
    DiagnosticCode::CachedPublic,
    DiagnosticCode::CompilationDirectiveLost,
    DiagnosticCode::CompilationDirectiveNeedLess,
    DiagnosticCode::CommandModuleExportMethods,
    DiagnosticCode::ServerSideExportFormMethod,
    DiagnosticCode::OrdinaryAppSupport,
    DiagnosticCode::MissingReturnedValueDescription,
    DiagnosticCode::MissingParameterDescription,
    DiagnosticCode::PublicMethodsDescription,
    DiagnosticCode::MissingVariablesDescription,
    DiagnosticCode::ExecuteExternalCodeInCommonModule,
    DiagnosticCode::CommonModuleMissingAPI,
    DiagnosticCode::PrivilegedModuleMethodCall,
    DiagnosticCode::SetPermissionsForNewObjects,
];

const MODULE_BODIES_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::ServerCallsInFormEvents,
    DiagnosticCode::DataExchangeLoading,
    DiagnosticCode::TransferringParametersBetweenClientAndServer,
    DiagnosticCode::UnusedLocalMethod,
];

const CONFIGURATION_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::ProtectedModule,
    DiagnosticCode::MissingEventSubscriptionHandler,
    DiagnosticCode::ScheduledJobHandler,
];

const SDBL_HIR_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::QueryParseError,
    DiagnosticCode::AmbiguousFieldInQuery,
    DiagnosticCode::AssignAliasFieldsInQuery,
    DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
    DiagnosticCode::DuplicateAliasInQuery,
    DiagnosticCode::FieldsFromJoinsWithoutIsNull,
    DiagnosticCode::FullOuterJoinQuery,
    DiagnosticCode::IncorrectUseLikeInQuery,
    DiagnosticCode::InvalidVirtualTableParameters,
    DiagnosticCode::JoinWithSubQuery,
    DiagnosticCode::JoinWithVirtualTable,
    DiagnosticCode::LogicalOrInJoinQuerySection,
    DiagnosticCode::LogicalOrInTheWhereSectionOfQuery,
    DiagnosticCode::MultilineStringInQuery,
    DiagnosticCode::QueryNestedFieldsByDot,
    DiagnosticCode::QueryToMissingMetadata,
    DiagnosticCode::QueryParameterSourceWithoutInto,
    DiagnosticCode::QueryTotalsInValueTableInto,
    DiagnosticCode::UnknownFieldInQuery,
    DiagnosticCode::UnlimitedLengthStringUsageInQuery,
    DiagnosticCode::RefOveruse,
    DiagnosticCode::SelectTopWithoutOrderBy,
    DiagnosticCode::UnionAll,
    DiagnosticCode::UsingLikeInQuery,
    DiagnosticCode::VirtualTableCallWithoutParameters,
];

const DATAFLOW_DIAGNOSTICS: &[DiagnosticCode] =
    &[DiagnosticCode::UnusedLocalVariable, DiagnosticCode::UnusedParameters];

/// Diagnostics that require the configuration-extension merge context (a base module paired
/// to the analyzed extension file) and therefore run from `apply_extension_merge` rather than
/// the standalone collector pass. Listed here so the coverage invariant accounts for them.
pub(crate) const WEAVING_DIAGNOSTICS: &[DiagnosticCode] =
    &[DiagnosticCode::WeavingSignatureMismatch, DiagnosticCode::WeavingAnnotationNotApplicable];

/// Diagnostics emitted by the in-code suppression pass (`crate::suppression`), which runs from
/// `apply_extension_merge` rather than a collector. Only the coverage-invariant test consumes this
/// (unlike `WEAVING_DIAGNOSTICS`, which the merge pass also reads at runtime), so it is test-only.
#[cfg(test)]
pub(crate) const SUPPRESSION_DIAGNOSTICS: &[DiagnosticCode] =
    &[DiagnosticCode::UnknownSuppressionCode, DiagnosticCode::SuppressionWithoutCode];

pub fn run_diagnostic<F>(
    name: &'static str,
    ctx: &DiagnosticsContext,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: FnOnce(&DiagnosticsContext) -> Vec<Diagnostic>,
{
    let start = std::time::Instant::now();
    let _span = tracing::debug_span!("diagnostic", name = name).entered();

    let result = check_fn(ctx);

    let elapsed = start.elapsed();
    if elapsed.as_millis() > 80 {
        tracing::info!(
            diagnostic = name,
            elapsed_ms = elapsed.as_millis(),
            count = result.len(),
            "Slow diagnostic"
        );
    }

    result
}

pub fn collect_line_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(LINE_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(handlers::parse_error::check(ctx));
    diagnostics.extend(handlers::consecutive_empty_lines::check(ctx));

    diagnostics
}

pub fn collect_syntax_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(SYNTAX_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic(
        "CodeBlockBeforeSub",
        ctx,
        handlers::code_block_before_sub::check,
    ));
    diagnostics.extend(run_diagnostic("CodeOutOfRegion", ctx, handlers::code_out_of_region::check));
    diagnostics.extend(run_diagnostic("DuplicateRegion", ctx, handlers::duplicate_region::check));
    diagnostics.extend(run_diagnostic(
        "DuplicateStringLiteral",
        ctx,
        handlers::duplicate_string_literal::check,
    ));
    diagnostics.extend(run_diagnostic("EmptyRegion", ctx, handlers::empty_region::check));
    diagnostics.extend(run_diagnostic(
        "NonStandardRegion",
        ctx,
        handlers::non_standard_region::check,
    ));

    diagnostics
}

pub fn collect_item_tree_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(ITEM_TREE_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic("OrderOfParams", ctx, handlers::order_of_params::check));
    diagnostics.extend(run_diagnostic(
        "ReservedParameterNames",
        ctx,
        handlers::reserved_parameter_names::check,
    ));

    diagnostics.extend(run_diagnostic(
        "SeveralCompilerDirectives",
        ctx,
        handlers::several_compiler_directives::check,
    ));

    diagnostics.extend(run_diagnostic(
        "NonExportMethodsInApiRegion",
        ctx,
        handlers::non_export_methods_in_api_region::check,
    ));

    diagnostics.extend(run_diagnostic("CachedPublic", ctx, handlers::cached_public::check));

    diagnostics.extend(run_diagnostic(
        "CompilationDirectiveLost",
        ctx,
        handlers::compilation_directive_lost::check,
    ));
    diagnostics.extend(run_diagnostic(
        "CompilationDirectiveNeedLess",
        ctx,
        handlers::compilation_directive_need_less::check,
    ));
    diagnostics.extend(run_diagnostic(
        "CommandModuleExportMethods",
        ctx,
        handlers::command_module_export_methods::check,
    ));
    diagnostics.extend(run_diagnostic(
        "ServerSideExportFormMethod",
        ctx,
        handlers::server_side_export_form_method::check,
    ));
    diagnostics.extend(run_diagnostic(
        "OrdinaryAppSupport",
        ctx,
        handlers::ordinary_app_support::check,
    ));

    diagnostics.extend(run_diagnostic(
        "MissingReturnedValueDescription",
        ctx,
        handlers::missing_returned_value_description::check,
    ));
    diagnostics.extend(run_diagnostic(
        "MissingParameterDescription",
        ctx,
        handlers::missing_parameter_description::check,
    ));
    diagnostics.extend(run_diagnostic(
        "PublicMethodsDescription",
        ctx,
        handlers::public_methods_description::check,
    ));
    diagnostics.extend(run_diagnostic(
        "MissingVariablesDescription",
        ctx,
        handlers::missing_variables_description::check,
    ));

    diagnostics.extend(run_diagnostic(
        "ExecuteExternalCodeInCommonModule",
        ctx,
        handlers::execute_external_code_in_common_module::check,
    ));
    diagnostics.extend(run_diagnostic(
        "CommonModuleMissingAPI",
        ctx,
        handlers::common_module_missing_api::check,
    ));
    diagnostics.extend(run_diagnostic(
        "PrivilegedModuleMethodCall",
        ctx,
        handlers::privileged_module_method_call::check,
    ));
    diagnostics.extend(run_diagnostic(
        "SetPermissionsForNewObjects",
        ctx,
        handlers::set_permissions_for_new_objects::check,
    ));

    diagnostics
}

pub fn collect_module_bodies_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(MODULE_BODIES_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic(
        "DataExchangeLoading",
        ctx,
        handlers::data_exchange_loading::check,
    ));
    diagnostics.extend(run_diagnostic(
        "TransferringParametersBetweenClientAndServer",
        ctx,
        handlers::transferring_parameters_between_client_and_server::check,
    ));
    diagnostics.extend(run_diagnostic(
        "UnusedLocalMethod",
        ctx,
        handlers::unused_local_method::check,
    ));
    diagnostics.extend(run_diagnostic(
        "ServerCallsInFormEvents",
        ctx,
        handlers::server_calls_in_form_events::check,
    ));

    diagnostics
}

pub fn collect_configuration_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(CONFIGURATION_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic("ProtectedModule", ctx, handlers::protected_module::check));

    diagnostics.extend(run_diagnostic(
        "MissingEventSubscriptionHandler",
        ctx,
        handlers::missing_event_subscription_handler::check,
    ));
    diagnostics.extend(run_diagnostic(
        "ScheduledJobHandler",
        ctx,
        handlers::scheduled_job_handler::check,
    ));

    diagnostics
}

pub fn collect_sdbl_hir_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(SDBL_HIR_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic("QueryParseError", ctx, handlers::query_parse_error::check));
    diagnostics.extend(run_diagnostic(
        "QueryParameterSourceWithoutInto",
        ctx,
        handlers::query_parameter_source_without_into::check,
    ));
    diagnostics.extend(run_diagnostic(
        "QueryTotalsInValueTableInto",
        ctx,
        handlers::query_totals_in_value_table_into::check,
    ));

    diagnostics.extend(collect_sdbl_hir_single_pass(ctx));

    diagnostics
}

pub(crate) const SDBL_DISPATCH: &[(DiagnosticCode, crate::sdbl_utils::SdblDispatchFn)] = &[
    (DiagnosticCode::AmbiguousFieldInQuery, handlers::ambiguous_field_in_query::dispatch),
    (DiagnosticCode::DuplicateAliasInQuery, handlers::duplicate_alias_in_query::dispatch),
    (DiagnosticCode::AssignAliasFieldsInQuery, handlers::assign_alias_fields_in_query::dispatch),
    (
        DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
        handlers::calculation_register_virtual_table_without_recorder::dispatch,
    ),
    (
        DiagnosticCode::FieldsFromJoinsWithoutIsNull,
        handlers::fields_from_joins_without_is_null::dispatch,
    ),
    (DiagnosticCode::FullOuterJoinQuery, handlers::full_outer_join_query::dispatch),
    (DiagnosticCode::IncorrectUseLikeInQuery, handlers::incorrect_use_like_in_query::dispatch),
    (
        DiagnosticCode::InvalidVirtualTableParameters,
        handlers::invalid_virtual_table_parameters::dispatch,
    ),
    (DiagnosticCode::JoinWithSubQuery, handlers::join_with_sub_query::dispatch),
    (DiagnosticCode::JoinWithVirtualTable, handlers::join_with_virtual_table::dispatch),
    (
        DiagnosticCode::LogicalOrInJoinQuerySection,
        handlers::logical_or_in_join_query_section::dispatch,
    ),
    (
        DiagnosticCode::LogicalOrInTheWhereSectionOfQuery,
        handlers::logical_or_in_the_where_section_of_query::dispatch,
    ),
    (DiagnosticCode::MultilineStringInQuery, handlers::multiline_string_in_query::dispatch),
    (DiagnosticCode::QueryNestedFieldsByDot, handlers::query_nested_fields_by_dot::dispatch),
    (DiagnosticCode::QueryToMissingMetadata, handlers::query_to_missing_metadata::dispatch),
    (DiagnosticCode::UnknownFieldInQuery, handlers::unknown_field_in_query::dispatch),
    (
        DiagnosticCode::UnlimitedLengthStringUsageInQuery,
        handlers::unlimited_length_string_usage_in_query::dispatch,
    ),
    (DiagnosticCode::RefOveruse, handlers::ref_overuse::dispatch),
    (DiagnosticCode::SelectTopWithoutOrderBy, handlers::select_top_without_order_by::dispatch),
    (DiagnosticCode::UnionAll, handlers::union_all::dispatch),
    (DiagnosticCode::UsingLikeInQuery, handlers::using_like_in_query::dispatch),
    (
        DiagnosticCode::VirtualTableCallWithoutParameters,
        handlers::virtual_table_call_without_parameters::dispatch,
    ),
];

fn collect_sdbl_hir_single_pass(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    use crate::sdbl_utils::{build_line_index_shared, SdblPositionMapper};

    let enabled: Vec<_> =
        SDBL_DISPATCH.iter().filter(|(code, _)| !ctx.is_disabled_with_metadata(*code)).collect();

    if enabled.is_empty() {
        return Vec::new();
    }

    let sdbl_hirs = ctx.sdbl_hir_in_file();
    let bsl_source = ctx.file_text();
    let sdbl_queries = ctx.all_sdbl_in_file();
    let line_starts = build_line_index_shared(&bsl_source);

    // The narrowing needs the module, so it lives here rather than in the dispatch a bare query
    // text shares.
    let writing_module_root = ctx
        .config_bool(
            DiagnosticCode::CalculationRegisterVirtualTableWithoutRecorder,
            handlers::calculation_register_virtual_table_without_recorder::ONLY_IN_MODULES_WRITING_REGISTER,
            false,
        )
        .then(|| ctx.parse().syntax_node());

    let mut diagnostics = Vec::new();

    for ((_, sdbl_package), (_, query_info)) in sdbl_hirs.iter().zip(sdbl_queries.iter()) {
        let mapper = SdblPositionMapper::from_query_info(query_info, &bsl_source, &line_starts);

        for hir_diag in sdbl_package.all_diagnostics() {
            if let (
                Some(root),
                sdbl_hir::SdblDiagnostic::CalculationRegisterVirtualTableWithoutRecorder {
                    register_name,
                    ..
                },
            ) = (&writing_module_root, hir_diag)
            {
                if !handlers::calculation_register_virtual_table_without_recorder::writes_register(
                    root,
                    register_name,
                ) {
                    continue;
                }
            }
            if let sdbl_hir::SdblDiagnostic::QueryToMissingMetadata { table_name, .. } = hir_diag {
                if handlers::query_to_missing_metadata::is_substituted_template(
                    &ctx.parse().syntax_node(),
                    query_info.bsl_literal_range,
                    table_name,
                ) {
                    continue;
                }
            }
            for (_, dispatch_fn) in &enabled {
                dispatch_fn(
                    ctx.config,
                    hir_diag,
                    &mapper,
                    &query_info.query_text,
                    &mut diagnostics,
                );
            }
        }
    }

    diagnostics
}

pub fn collect_dataflow_diagnostics(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if !ctx.config.any_enabled(DATAFLOW_DIAGNOSTICS) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    diagnostics.extend(run_diagnostic(
        "UnusedLocalVariable",
        ctx,
        handlers::unused_local_variable::check,
    ));
    diagnostics.extend(run_diagnostic("UnusedParameters", ctx, handlers::unused_parameters::check));
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiagnosticCode;

    #[test]
    fn test_all_diagnostic_codes_in_exactly_one_collector() {
        use std::collections::HashMap;
        use strum::IntoEnumIterator;

        let all_arrays: &[(&str, &[DiagnosticCode])] = &[
            ("LINE_DIAGNOSTICS", LINE_DIAGNOSTICS),
            ("SLAB_DIAGNOSTICS", crate::slab::SLAB_DIAGNOSTICS),
            ("SINGLE_PASS_DIAGNOSTICS", SINGLE_PASS_DIAGNOSTICS),
            ("SYNTAX_DIAGNOSTICS", SYNTAX_DIAGNOSTICS),
            ("ITEM_TREE_DIAGNOSTICS", ITEM_TREE_DIAGNOSTICS),
            ("MODULE_BODIES_DIAGNOSTICS", MODULE_BODIES_DIAGNOSTICS),
            ("CONFIGURATION_DIAGNOSTICS", CONFIGURATION_DIAGNOSTICS),
            ("SDBL_HIR_DIAGNOSTICS", SDBL_HIR_DIAGNOSTICS),
            ("DATAFLOW_DIAGNOSTICS", DATAFLOW_DIAGNOSTICS),
            ("BODY_DIAGNOSTICS", crate::body::BODY_DIAGNOSTICS),
            ("HIR_DIAGNOSTICS", crate::hir_dispatch::HIR_DIAGNOSTICS),
            ("METADATA_DIAGNOSTICS", crate::metadata_dispatch::METADATA_DIAGNOSTICS),
            ("INFERENCE_DIAGNOSTICS", crate::hir_inference_dispatch::INFERENCE_DIAGNOSTICS),
            ("WEAVING_DIAGNOSTICS", WEAVING_DIAGNOSTICS),
            ("SUPPRESSION_DIAGNOSTICS", SUPPRESSION_DIAGNOSTICS),
        ];

        let mut code_to_arrays: HashMap<DiagnosticCode, Vec<&str>> = HashMap::new();
        for (name, codes) in all_arrays {
            for code in *codes {
                code_to_arrays.entry(*code).or_default().push(name);
            }
        }

        let known_dual_registration: &[DiagnosticCode] = &[
            DiagnosticCode::IncorrectUseOfStrTemplate,
            DiagnosticCode::RedundantAccessToObject,
            DiagnosticCode::MissedRequiredParameter,
            DiagnosticCode::DeprecatedPlatformApi,
            // Two emitters by design: the constructor is a syntactic fact and
            // stays in lowering, the bare call needs the shadowing guard and is
            // judged in inference.
            DiagnosticCode::FileSystemAccess,
            // Область по умолчанию — метод, и её судит тело; свод по файлу
            // (`analyzeFile=true`) остаётся файловым. Оба входа читают один
            // ключ конфига и взаимно исключают друг друга.
            DiagnosticCode::DuplicateStringLiteral,
            // Members are resolved by inference; `Асинх` / `Ждать` are syntax no
            // member lookup sees and are read off the body's tokens.
            DiagnosticCode::PlatformMemberNewerThanMinVersion,
        ];

        let mut duplicates = Vec::new();
        for (code, arrays) in &code_to_arrays {
            if arrays.len() > 1 && !known_dual_registration.contains(code) {
                duplicates.push(format!("{:?} in: {}", code, arrays.join(", ")));
            }
        }
        assert!(
            duplicates.is_empty(),
            "Diagnostic codes registered in multiple collectors (not in known_dual_registration):\n{}",
            duplicates.join("\n")
        );

        let mut missing = Vec::new();
        for code in DiagnosticCode::iter() {
            if !code_to_arrays.contains_key(&code) {
                missing.push(format!("{:?}", code));
            }
        }
        assert!(
            missing.is_empty(),
            "Diagnostic codes not registered in any collector:\n{}",
            missing.join("\n")
        );
    }
}
