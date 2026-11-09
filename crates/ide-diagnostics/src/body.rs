//! Diagnostics of one body — the per-method unit of recomputation.
//!
//! [`body_diagnostics`] is the whole check set that reads nothing outside the
//! body and the module's position-free interface; the salsa query in
//! [`crate::query`] memoises it per method, and [`bodies_via_provider`] drives
//! the same function over every body through an [`ide_db::AnalysisProvider`]
//! — the unmemoised path the in-crate tests and non-database providers use.

use hir::{DefWithBodyId, LocalRange, MethodId, MethodOffset, ModuleId};

use crate::{
    handlers, hir_dispatch, hir_inference_dispatch, BodyContext, Diagnostic, DiagnosticCode,
    DiagnosticsContext,
};

/// The checks that run per body (see `runner.rs` for the file-level ones);
/// the registration invariant keeps every code in exactly one place.
pub(crate) const BODY_DIAGNOSTICS: &[DiagnosticCode] = &[
    DiagnosticCode::InternetAccess,
    DiagnosticCode::IsInRoleMethod,
    DiagnosticCode::TimeoutsInExternalResources,
    DiagnosticCode::UsingHardcodeSecretInformation,
    DiagnosticCode::NumberOfValuesInStructureConstructor,
    DiagnosticCode::NestedConstructorsInStructureDeclaration,
    DiagnosticCode::UsingHardcodeNetworkAddress,
    DiagnosticCode::MissingCodeTryCatchEx,
    DiagnosticCode::IdenticalExpressions,
    DiagnosticCode::NestedFunctionInParameters,
    DiagnosticCode::DuplicatedInsertionIntoCollection,
    DiagnosticCode::IncorrectUseOfStrTemplate,
    DiagnosticCode::PairingBrokenTransaction,
    DiagnosticCode::UnreachableCode,
    DiagnosticCode::MissingTemporaryFileDeletion,
    DiagnosticCode::ObjectModifiedInOnWrite,
    DiagnosticCode::PostingWhileDataExchangeLoading,
    DiagnosticCode::StructureKeyIsNotIdentifier,
    DiagnosticCode::ValueIsFilledOfBoolean,
    DiagnosticCode::MissingTempStorageDeletion,
    DiagnosticCode::SetPrivilegedMode,
    DiagnosticCode::DisableSafeMode,
    DiagnosticCode::CognitiveComplexity,
    DiagnosticCode::CyclomaticComplexity,
    DiagnosticCode::NestedStatements,
    DiagnosticCode::IfConditionComplexity,
    DiagnosticCode::MethodSize,
    DiagnosticCode::NumberOfParams,
    DiagnosticCode::NumberOfOptionalParams,
    DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
    DiagnosticCode::MultilingualStringHasEmptyTranslation,
    DiagnosticCode::MultilingualStringUsingWithTemplate,
    DiagnosticCode::ExcessiveAutoTestCheck,
    DiagnosticCode::LatinAndCyrillicSymbolInWord,
    DiagnosticCode::MultilinePreprocessorInstruction,
    DiagnosticCode::CanonicalSpellingKeywords,
    DiagnosticCode::InvalidCharacterInFile,
    DiagnosticCode::SpaceAtStartComment,
    DiagnosticCode::UsingServiceTag,
    DiagnosticCode::DuplicateStringLiteral,
    DiagnosticCode::GlobalContextMethodConflict,
    DiagnosticCode::PlatformMemberNewerThanMinVersion,
];

pub fn body_diagnostics(ctx: &BodyContext) -> Vec<Diagnostic<LocalRange>> {
    let mut acc = Vec::new();
    hir_dispatch::collect_body_hir_diagnostics(ctx, &mut acc);
    hir_inference_dispatch::collect_body_inference_diagnostics(ctx, &mut acc);
    hir_inference_dispatch::collect_body_arg_diagnostics(ctx, &mut acc);
    crate::single_pass::collect_body_single_pass(ctx, &mut acc);
    if ctx.config.any_enabled(BODY_DIAGNOSTICS) {
        handlers::internet_access::check_body(ctx, &mut acc);
        handlers::is_in_role_method::check_body(ctx, &mut acc);
        handlers::timeouts_in_external_resources::check_body(ctx, &mut acc);
        handlers::using_hardcode_secret_information::check_body(ctx, &mut acc);
        handlers::number_of_values_in_structure_constructor::check_body(ctx, &mut acc);
        handlers::nested_constructors_in_structure_declaration::check_body(ctx, &mut acc);
        handlers::using_hardcode_network_address::check_body(ctx, &mut acc);
        handlers::missing_code_try_catch_ex::check_body(ctx, &mut acc);
        handlers::identical_expressions::check_body(ctx, &mut acc);
        handlers::nested_function_in_parameters::check_body(ctx, &mut acc);
        handlers::duplicated_insertion_into_collection::check_body(ctx, &mut acc);
        handlers::incorrect_use_of_str_template::check_body(ctx, &mut acc);
        handlers::pairing_broken_transaction::check_body(ctx, &mut acc);
        handlers::unreachable_code::check_body(ctx, &mut acc);
        handlers::missing_temporary_file_deletion::check_body(ctx, &mut acc);
        handlers::object_modified_in_on_write::check_body(ctx, &mut acc);
        handlers::posting_while_data_exchange_loading::check_body(ctx, &mut acc);
        handlers::structure_key_is_not_identifier::check_body(ctx, &mut acc);
        handlers::value_is_filled_of_boolean::check_body(ctx, &mut acc);
        handlers::missing_temp_storage_deletion::check_body(ctx, &mut acc);
        handlers::set_privileged_mode::check_body(ctx, &mut acc);
        handlers::disable_safe_mode::check_body(ctx, &mut acc);
        handlers::cognitive_complexity::check_body(ctx, &mut acc);
        handlers::cyclomatic_complexity::check_body(ctx, &mut acc);
        handlers::nested_statements::check_body(ctx, &mut acc);
        handlers::if_condition_complexity::check_body(ctx, &mut acc);
        handlers::method_size::check_body(ctx, &mut acc);
        handlers::number_of_params::check_body(ctx, &mut acc);
        handlers::number_of_optional_params::check_body(ctx, &mut acc);
        handlers::multilingual_string_has_all_declared_languages::check_body(ctx, &mut acc);
        handlers::multilingual_string_using_with_template::check_body(ctx, &mut acc);
        handlers::excessive_auto_test_check::check_body(ctx, &mut acc);
        handlers::latin_and_cyrillic_symbol_in_word::check_body(ctx, &mut acc);
        handlers::multiline_preprocessor_instruction::check_body(ctx, &mut acc);
        handlers::canonical_spelling_keywords::check_body(ctx, &mut acc);
        handlers::invalid_character_in_file::check_body(ctx, &mut acc);
        handlers::space_at_start_comment::check_body(ctx, &mut acc);
        handlers::using_service_tag::check_body(ctx, &mut acc);
        handlers::duplicate_string_literal::check_body(ctx, &mut acc);
        handlers::global_context_method_conflict::check_body(ctx, &mut acc);
        handlers::platform_member_newer_than_min_version::check_body(ctx, &mut acc);
    }
    acc
}

/// Run `check` on every body of the file through the context's provider,
/// lifting each body's findings into the file. The unmemoised driver: the
/// in-crate tests and the non-database providers use it.
pub(crate) fn for_each_body_context(
    ctx: &DiagnosticsContext,
    mut check: impl FnMut(&BodyContext, &mut Vec<Diagnostic<LocalRange>>),
) -> Vec<Diagnostic> {
    let module_bodies = ctx.module_bodies();
    let module = ModuleId::new(ctx.file_id);
    let analysis = ctx.analysis();
    let mut out = Vec::new();

    for (local_id, lower) in module_bodies.iter_lower_results() {
        let Some(root) = analysis.provider().method_syntax(MethodId { module, local_id }) else {
            continue;
        };
        let body_ctx =
            BodyContext::new(analysis, DefWithBodyId::Method(local_id), root, lower.result);
        let mut local = Vec::new();
        check(&body_ctx, &mut local);
        out.extend(local.into_iter().map(|d| d.lift(lower.base)));
    }

    if let Some(module_code) = module_bodies.module_code_result() {
        let root = ctx.parse().syntax_node();
        let body_ctx =
            BodyContext::new(analysis, DefWithBodyId::ModuleCode, root, module_code.result);
        let mut local = Vec::new();
        check(&body_ctx, &mut local);
        out.extend(local.into_iter().map(|d| d.lift(MethodOffset::ZERO)));
    }

    out
}

/// Every body's diagnostics lifted into the file, computed through the
/// context's provider without memoisation.
pub fn bodies_via_provider(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    for_each_body_context(ctx, |body_ctx, acc| acc.extend(body_diagnostics(body_ctx)))
}
