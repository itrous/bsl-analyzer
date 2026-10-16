pub mod all_function_path_must_have_return;
pub mod ambiguous_field_in_query;
pub mod assign_alias_fields_in_query;
pub mod bad_words;
pub mod begin_transaction_before_try_catch;
pub mod cached_public;
pub mod canonical_spelling_keywords;
pub mod code_after_async_call;
pub mod code_block_before_sub;
pub mod code_out_of_region;
pub mod cognitive_complexity;
pub mod command_module_export_methods;
pub mod commented_code;
pub mod commit_transaction_outside_try_catch;
pub mod common_module_assign;
pub mod common_module_invalid_type;
pub mod common_module_missing_api;
pub mod common_module_name_cached;
pub mod common_module_name_client;
pub mod common_module_name_client_server;
pub mod common_module_name_full_access;
pub mod common_module_name_global;
pub mod common_module_name_global_client;
pub mod common_module_name_server_call;
pub mod common_module_name_words;
pub mod compilation_directive_lost;
pub mod compilation_directive_need_less;
pub mod consecutive_empty_lines;
pub mod create_query_in_cycle;
pub mod cyclomatic_complexity;
pub mod data_exchange_loading;
pub mod deleting_collection_item;
pub mod deny_incomplete_values;
pub mod deprecated_attributes_8312;
pub mod deprecated_current_date;
pub mod deprecated_find;
pub mod deprecated_message;
pub mod deprecated_method;
pub mod deprecated_method_call;
pub mod deprecated_platform_api;
mod deprecated_platform_facts;
pub mod deprecated_type_managed_form;
pub mod disable_safe_mode;
pub mod double_negatives;
pub mod duplicate_alias_in_query;
pub mod duplicate_region;
pub mod duplicate_string_literal;
pub mod duplicated_insertion_into_collection;
pub mod empty_code_block;
pub mod empty_region;
pub mod empty_statement;
pub mod excessive_auto_test_check;
pub mod execute_external_code;
pub mod execute_external_code_in_common_module;
pub mod export_variables;
pub mod external_app_starting;
pub mod extra_commas;
pub mod fields_from_joins_without_is_null;
pub mod file_system_access;
pub mod forbidden_metadata_name;
pub mod form_data_to_value;
pub mod full_outer_join_query;
pub mod function_name_starts_with_get;
pub mod function_out_parameter;
pub mod function_returns_same_primitive;
pub mod function_should_have_return;
pub mod get_form_method;
pub mod global_context_method_collision8312;
pub mod global_context_method_conflict;
pub mod global_property_not_writable;
pub mod identical_expressions;
pub mod if_condition_complexity;
pub mod if_else_duplicated_code_block;
pub mod if_else_duplicated_condition;
pub mod if_else_if_ends_with_else;
pub mod incorrect_line_break;
pub mod incorrect_use_like_in_query;
pub mod incorrect_use_of_str_template;
pub mod internet_access;
pub mod invalid_character_in_file;
pub mod invalid_virtual_table_parameters;
pub mod is_in_role_method;
pub mod join_with_sub_query;
pub mod join_with_virtual_table;
pub mod latin_and_cyrillic_symbol_in_word;
pub mod line_length;
pub mod logical_or_in_join_query_section;
pub mod logical_or_in_the_where_section_of_query;
pub mod magic_date;
pub mod magic_number;
pub mod metadata_object_name_length;
pub mod method_size;
pub mod mismatched_arg_count;
pub mod misplaced_loop_control;
pub mod missed_required_parameter;
pub mod missing_code_try_catch_ex;
pub mod missing_common_module_method;
pub mod missing_event_subscription_handler;
pub mod missing_form_event_handler;
pub mod missing_parameter_description;
pub mod missing_returned_value_description;
pub mod missing_space;
pub mod missing_temp_storage_deletion;
pub mod missing_temporary_file_deletion;
pub mod missing_variables_description;
pub mod module_accessibility;
pub mod multiline_preprocessor_instruction;
pub mod multiline_string_in_query;
pub mod multilingual_string_has_all_declared_languages;
pub mod multilingual_string_using_with_template;
pub mod nested_constructors_in_structure_declaration;
pub mod nested_function_in_parameters;
pub mod nested_statements;
pub mod nested_ternary_operator;
pub mod non_export_methods_in_api_region;
pub mod non_standard_region;
pub mod number_of_optional_params;
pub mod number_of_params;
pub mod number_of_values_in_structure_constructor;
pub mod one_statement_per_line;
pub mod order_of_params;
pub mod ordinary_app_support;
pub mod os_users_method;
pub mod pairing_broken_transaction;
pub mod parse_error;
pub mod platform_member_hidden_by_compatibility_mode;
pub mod platform_member_newer_than_min_version;
pub mod postfix_access_on_expression;
pub mod privileged_module_method_call;
pub mod procedure_returns_value;
pub mod protected_module;
pub mod public_methods_description;
pub mod query_nested_fields_by_dot;
pub mod query_parse_error;
pub mod query_to_missing_metadata;
pub mod read_only_property;
pub mod redundant_access_to_object;
pub mod ref_overuse;
pub mod reserved_parameter_names;
pub mod reserved_word_as_method_name;
pub mod rewrite_method_parameter;
pub mod same_metadata_object_and_child_names;
pub mod scheduled_job_handler;
pub mod select_top_without_order_by;
pub mod self_assign;
pub mod self_insertion;
pub mod semicolon_presence;
pub mod server_calls_in_form_events;
pub mod server_side_export_form_method;
pub mod set_permissions_for_new_objects;
pub mod set_privileged_mode;
pub mod several_compiler_directives;
pub mod space_at_start_comment;
pub mod style_element_constructors;
pub mod temp_files_dir;
pub mod ternary_operator_usage;
pub mod this_object_assign;
pub mod timeouts_in_external_resources;
pub mod too_many_returns;
pub mod transferring_parameters_between_client_and_server;
pub mod try_number;
pub mod type_mismatch;
pub mod type_mismatch_by_doc_comment;
pub mod typo;
pub mod unary_plus_in_concatenation;
pub mod unavailable_in_environment;
pub mod union_all;
pub mod unknown_field_in_query;
pub mod unknown_preprocessor_symbol;
pub mod unlimited_length_string_usage_in_query;
pub mod unreachable_code;
pub mod unresolved_bare_call;
pub mod unresolved_field;
pub mod unresolved_method_call;
pub mod unresolved_name;
pub mod unsafe_find_by_code;
pub mod unsafe_safe_mode_method_call;
pub mod unused_local_method;
pub mod unused_local_variable;
pub mod unused_parameters;
pub mod usage_write_log_event;
pub mod use_system_information;
pub mod useless_for_each;
pub mod useless_ternary_operator;
pub mod using_cancel_parameter;
pub mod using_external_code_tools;
pub mod using_find_element_by_string;
pub mod using_goto;
pub mod using_hardcode_network_address;
pub mod using_hardcode_path;
pub mod using_hardcode_secret_information;
pub mod using_like_in_query;
pub mod using_modal_windows;
pub mod using_object_not_available_unix;
pub mod using_service_tag;
pub mod using_synchronous_calls;
pub mod using_this_form;
pub mod virtual_table_call_without_parameters;
pub mod weaving_annotation_not_applicable;
pub mod weaving_signature_mismatch;
pub mod wrong_data_path_for_form_elements;
pub mod wrong_http_service_handler;
pub mod wrong_use_function_proceed_with_call;
pub mod wrong_use_of_rollback_transaction_method;
pub mod wrong_web_service_handler;
pub mod yo_letter_usage;

use crate::metadata::*;
use crate::DiagnosticCode;

pub fn get_metadata(code: DiagnosticCode) -> Option<&'static DiagnosticMetadata> {
    match code {
        DiagnosticCode::BadWords => Some(&bad_words::METADATA),
        DiagnosticCode::CodeAfterAsyncCall => Some(&code_after_async_call::METADATA),
        DiagnosticCode::DenyIncompleteValues => Some(&deny_incomplete_values::METADATA),
        DiagnosticCode::ForbiddenMetadataName => Some(&forbidden_metadata_name::METADATA),
        DiagnosticCode::FieldsFromJoinsWithoutIsNull => {
            Some(&fields_from_joins_without_is_null::METADATA)
        }
        DiagnosticCode::FileSystemAccess => Some(&file_system_access::METADATA),
        DiagnosticCode::FunctionNameStartsWithGet => Some(&function_name_starts_with_get::METADATA),
        DiagnosticCode::FunctionOutParameter => Some(&function_out_parameter::METADATA),
        DiagnosticCode::InternetAccess => Some(&internet_access::METADATA),
        DiagnosticCode::MissingTempStorageDeletion => {
            Some(&missing_temp_storage_deletion::METADATA)
        }
        DiagnosticCode::TernaryOperatorUsage => Some(&ternary_operator_usage::METADATA),
        DiagnosticCode::TooManyReturns => Some(&too_many_returns::METADATA),

        DiagnosticCode::ParseError => Some(&parse_error::METADATA),
        DiagnosticCode::CanonicalSpellingKeywords => Some(&canonical_spelling_keywords::METADATA),
        DiagnosticCode::ConsecutiveEmptyLines => Some(&consecutive_empty_lines::METADATA),
        DiagnosticCode::MultilinePreprocessorInstruction => {
            Some(&multiline_preprocessor_instruction::METADATA)
        }
        DiagnosticCode::LineLength => Some(&line_length::METADATA),
        DiagnosticCode::MissingSpace => Some(&missing_space::METADATA),
        DiagnosticCode::OneStatementPerLine => Some(&one_statement_per_line::METADATA),
        DiagnosticCode::SemicolonPresence => Some(&semicolon_presence::METADATA),
        DiagnosticCode::SpaceAtStartComment => Some(&space_at_start_comment::METADATA),
        DiagnosticCode::IncorrectLineBreak => Some(&incorrect_line_break::METADATA),
        DiagnosticCode::IncorrectUseOfStrTemplate => Some(&incorrect_use_of_str_template::METADATA),
        DiagnosticCode::ExtraCommas => Some(&extra_commas::METADATA),
        DiagnosticCode::CommentedCode => Some(&commented_code::METADATA),
        DiagnosticCode::EmptyCodeBlock => Some(&empty_code_block::METADATA),
        DiagnosticCode::EmptyRegion => Some(&empty_region::METADATA),
        DiagnosticCode::EmptyStatement => Some(&empty_statement::METADATA),
        DiagnosticCode::UnreachableCode => Some(&unreachable_code::METADATA),
        DiagnosticCode::CodeBlockBeforeSub => Some(&code_block_before_sub::METADATA),
        DiagnosticCode::CodeOutOfRegion => Some(&code_out_of_region::METADATA),
        DiagnosticCode::MagicNumber => Some(&magic_number::METADATA),
        DiagnosticCode::MagicDate => Some(&magic_date::METADATA),
        DiagnosticCode::YoLetterUsage => Some(&yo_letter_usage::METADATA),
        DiagnosticCode::LatinAndCyrillicSymbolInWord => {
            Some(&latin_and_cyrillic_symbol_in_word::METADATA)
        }
        DiagnosticCode::InvalidCharacterInFile => Some(&invalid_character_in_file::METADATA),
        DiagnosticCode::DoubleNegatives => Some(&double_negatives::METADATA),
        DiagnosticCode::NestedTernaryOperator => Some(&nested_ternary_operator::METADATA),
        DiagnosticCode::PostfixAccessOnExpression => Some(&postfix_access_on_expression::METADATA),
        DiagnosticCode::NonExportMethodsInApiRegion => {
            Some(&non_export_methods_in_api_region::METADATA)
        }
        DiagnosticCode::UnaryPlusInConcatenation => Some(&unary_plus_in_concatenation::METADATA),
        DiagnosticCode::UselessTernaryOperator => Some(&useless_ternary_operator::METADATA),
        DiagnosticCode::DuplicateStringLiteral => Some(&duplicate_string_literal::METADATA),
        DiagnosticCode::DuplicateRegion => Some(&duplicate_region::METADATA),
        DiagnosticCode::NonStandardRegion => Some(&non_standard_region::METADATA),
        DiagnosticCode::DuplicatedInsertionIntoCollection => {
            Some(&duplicated_insertion_into_collection::METADATA)
        }
        DiagnosticCode::ExcessiveAutoTestCheck => Some(&excessive_auto_test_check::METADATA),
        DiagnosticCode::IdenticalExpressions => Some(&identical_expressions::METADATA),
        DiagnosticCode::IfElseDuplicatedCodeBlock => Some(&if_else_duplicated_code_block::METADATA),
        DiagnosticCode::IfElseDuplicatedCondition => Some(&if_else_duplicated_condition::METADATA),
        DiagnosticCode::IfElseIfEndsWithElse => Some(&if_else_if_ends_with_else::METADATA),
        DiagnosticCode::MultilingualStringHasAllDeclaredLanguages => {
            Some(&multilingual_string_has_all_declared_languages::METADATA)
        }
        DiagnosticCode::MultilingualStringUsingWithTemplate => {
            Some(&multilingual_string_using_with_template::METADATA)
        }
        DiagnosticCode::NestedConstructorsInStructureDeclaration => {
            Some(&nested_constructors_in_structure_declaration::METADATA)
        }
        DiagnosticCode::NestedFunctionInParameters => {
            Some(&nested_function_in_parameters::METADATA)
        }

        DiagnosticCode::AllFunctionPathMustHaveReturn => {
            Some(&all_function_path_must_have_return::METADATA)
        }
        DiagnosticCode::FunctionShouldHaveReturn => Some(&function_should_have_return::METADATA),
        DiagnosticCode::ProcedureReturnsValue => Some(&procedure_returns_value::METADATA),
        DiagnosticCode::FunctionReturnsSamePrimitive => {
            Some(&function_returns_same_primitive::METADATA)
        }
        DiagnosticCode::NumberOfParams => Some(&number_of_params::METADATA),
        DiagnosticCode::NumberOfOptionalParams => Some(&number_of_optional_params::METADATA),
        DiagnosticCode::NumberOfValuesInStructureConstructor => {
            Some(&number_of_values_in_structure_constructor::METADATA)
        }
        DiagnosticCode::OrderOfParams => Some(&order_of_params::METADATA),
        DiagnosticCode::MissedRequiredParameter => Some(&missed_required_parameter::METADATA),
        DiagnosticCode::UnusedParameters => Some(&unused_parameters::METADATA),
        DiagnosticCode::MissingParameterDescription => {
            Some(&missing_parameter_description::METADATA)
        }
        DiagnosticCode::MissingReturnedValueDescription => {
            Some(&missing_returned_value_description::METADATA)
        }
        DiagnosticCode::ReservedParameterNames => Some(&reserved_parameter_names::METADATA),
        DiagnosticCode::ReservedWordAsMethodName => Some(&reserved_word_as_method_name::METADATA),
        DiagnosticCode::RewriteMethodParameter => Some(&rewrite_method_parameter::METADATA),
        DiagnosticCode::UnusedLocalMethod => Some(&unused_local_method::METADATA),
        DiagnosticCode::ExportVariables => Some(&export_variables::METADATA),
        DiagnosticCode::MissingVariablesDescription => {
            Some(&missing_variables_description::METADATA)
        }
        DiagnosticCode::SelfAssign => Some(&self_assign::METADATA),
        DiagnosticCode::ThisObjectAssign => Some(&this_object_assign::METADATA),
        DiagnosticCode::CyclomaticComplexity => Some(&cyclomatic_complexity::METADATA),
        DiagnosticCode::CognitiveComplexity => Some(&cognitive_complexity::METADATA),
        DiagnosticCode::NestedStatements => Some(&nested_statements::METADATA),
        DiagnosticCode::MethodSize => Some(&method_size::METADATA),
        DiagnosticCode::IfConditionComplexity => Some(&if_condition_complexity::METADATA),
        DiagnosticCode::MissingCodeTryCatchEx => Some(&missing_code_try_catch_ex::METADATA),
        DiagnosticCode::MissingTemporaryFileDeletion => {
            Some(&missing_temporary_file_deletion::METADATA)
        }
        DiagnosticCode::MisplacedLoopControl => Some(&misplaced_loop_control::METADATA),
        DiagnosticCode::UseLessForEach => Some(&useless_for_each::METADATA),
        DiagnosticCode::UsingGoto => Some(&using_goto::METADATA),
        DiagnosticCode::BeginTransactionBeforeTryCatch => {
            Some(&begin_transaction_before_try_catch::METADATA)
        }
        DiagnosticCode::CommitTransactionOutsideTryCatch => {
            Some(&commit_transaction_outside_try_catch::METADATA)
        }
        DiagnosticCode::CompilationDirectiveLost => Some(&compilation_directive_lost::METADATA),
        DiagnosticCode::CompilationDirectiveNeedLess => {
            Some(&compilation_directive_need_less::METADATA)
        }
        DiagnosticCode::CreateQueryInCycle => Some(&create_query_in_cycle::METADATA),
        DiagnosticCode::DeletingCollectionItem => Some(&deleting_collection_item::METADATA),
        DiagnosticCode::SelfInsertion => Some(&self_insertion::METADATA),
        DiagnosticCode::SeveralCompilerDirectives => Some(&several_compiler_directives::METADATA),
        DiagnosticCode::StyleElementConstructors => Some(&style_element_constructors::METADATA),
        DiagnosticCode::DeprecatedPlatformApi => Some(&deprecated_platform_api::METADATA),
        DiagnosticCode::DeprecatedMethodCall => Some(&deprecated_method_call::METADATA),
        DiagnosticCode::DisableSafeMode => Some(&disable_safe_mode::METADATA),
        DiagnosticCode::ExternalAppStarting => Some(&external_app_starting::METADATA),
        DiagnosticCode::OSUsersMethod => Some(&os_users_method::METADATA),
        DiagnosticCode::TempFilesDir => Some(&temp_files_dir::METADATA),
        DiagnosticCode::FormDataToValue => Some(&form_data_to_value::METADATA),
        DiagnosticCode::GetFormMethod => Some(&get_form_method::METADATA),
        DiagnosticCode::GlobalContextMethodCollision8312 => {
            Some(&global_context_method_collision8312::METADATA)
        }
        DiagnosticCode::GlobalContextMethodConflict => {
            Some(&global_context_method_conflict::METADATA)
        }
        DiagnosticCode::IsInRoleMethod => Some(&is_in_role_method::METADATA),
        DiagnosticCode::PairingBrokenTransaction => Some(&pairing_broken_transaction::METADATA),
        DiagnosticCode::WrongUseOfRollbackTransactionMethod => {
            Some(&wrong_use_of_rollback_transaction_method::METADATA)
        }

        DiagnosticCode::AssignAliasFieldsInQuery => Some(&assign_alias_fields_in_query::METADATA),
        DiagnosticCode::CachedPublic => Some(&cached_public::METADATA),
        DiagnosticCode::CommandModuleExportMethods => {
            Some(&command_module_export_methods::METADATA)
        }
        DiagnosticCode::CommonModuleAssign => Some(&common_module_assign::METADATA),
        DiagnosticCode::CommonModuleInvalidType => Some(&common_module_invalid_type::METADATA),
        DiagnosticCode::CommonModuleMissingAPI => Some(&common_module_missing_api::METADATA),
        DiagnosticCode::CommonModuleNameCached => Some(&common_module_name_cached::METADATA),
        DiagnosticCode::CommonModuleNameClient => Some(&common_module_name_client::METADATA),
        DiagnosticCode::CommonModuleNameClientServer => {
            Some(&common_module_name_client_server::METADATA)
        }
        DiagnosticCode::CommonModuleNameFullAccess => {
            Some(&common_module_name_full_access::METADATA)
        }
        DiagnosticCode::CommonModuleNameGlobal => Some(&common_module_name_global::METADATA),
        DiagnosticCode::CommonModuleNameGlobalClient => {
            Some(&common_module_name_global_client::METADATA)
        }
        DiagnosticCode::CommonModuleNameServerCall => {
            Some(&common_module_name_server_call::METADATA)
        }
        DiagnosticCode::CommonModuleNameWords => Some(&common_module_name_words::METADATA),
        DiagnosticCode::FullOuterJoinQuery => Some(&full_outer_join_query::METADATA),
        DiagnosticCode::IncorrectUseLikeInQuery => Some(&incorrect_use_like_in_query::METADATA),
        DiagnosticCode::JoinWithSubQuery => Some(&join_with_sub_query::METADATA),
        DiagnosticCode::JoinWithVirtualTable => Some(&join_with_virtual_table::METADATA),
        DiagnosticCode::LogicalOrInJoinQuerySection => {
            Some(&logical_or_in_join_query_section::METADATA)
        }
        DiagnosticCode::LogicalOrInTheWhereSectionOfQuery => {
            Some(&logical_or_in_the_where_section_of_query::METADATA)
        }
        DiagnosticCode::MetadataObjectNameLength => Some(&metadata_object_name_length::METADATA),
        DiagnosticCode::MissingCommonModuleMethod => Some(&missing_common_module_method::METADATA),
        DiagnosticCode::MissingEventSubscriptionHandler => {
            Some(&missing_event_subscription_handler::METADATA)
        }
        DiagnosticCode::MissingFormEventHandler => Some(&missing_form_event_handler::METADATA),
        DiagnosticCode::MultilineStringInQuery => Some(&multiline_string_in_query::METADATA),
        DiagnosticCode::OrdinaryAppSupport => Some(&ordinary_app_support::METADATA),
        DiagnosticCode::PrivilegedModuleMethodCall => {
            Some(&privileged_module_method_call::METADATA)
        }
        DiagnosticCode::ProtectedModule => Some(&protected_module::METADATA),
        DiagnosticCode::PublicMethodsDescription => Some(&public_methods_description::METADATA),
        DiagnosticCode::QueryNestedFieldsByDot => Some(&query_nested_fields_by_dot::METADATA),
        DiagnosticCode::QueryParseError => Some(&query_parse_error::METADATA),
        DiagnosticCode::QueryToMissingMetadata => Some(&query_to_missing_metadata::METADATA),
        DiagnosticCode::AmbiguousFieldInQuery => Some(&ambiguous_field_in_query::METADATA),
        DiagnosticCode::DuplicateAliasInQuery => Some(&duplicate_alias_in_query::METADATA),
        DiagnosticCode::UnknownFieldInQuery => Some(&unknown_field_in_query::METADATA),
        DiagnosticCode::InvalidVirtualTableParameters => {
            Some(&invalid_virtual_table_parameters::METADATA)
        }
        DiagnosticCode::UnlimitedLengthStringUsageInQuery => {
            Some(&unlimited_length_string_usage_in_query::METADATA)
        }
        DiagnosticCode::RefOveruse => Some(&ref_overuse::METADATA),
        DiagnosticCode::SelectTopWithoutOrderBy => Some(&select_top_without_order_by::METADATA),
        DiagnosticCode::UnionAll => Some(&union_all::METADATA),
        DiagnosticCode::UsingLikeInQuery => Some(&using_like_in_query::METADATA),
        DiagnosticCode::VirtualTableCallWithoutParameters => {
            Some(&virtual_table_call_without_parameters::METADATA)
        }
        DiagnosticCode::ScheduledJobHandler => Some(&scheduled_job_handler::METADATA),
        DiagnosticCode::ServerCallsInFormEvents => Some(&server_calls_in_form_events::METADATA),
        DiagnosticCode::ServerSideExportFormMethod => {
            Some(&server_side_export_form_method::METADATA)
        }
        DiagnosticCode::SetPermissionsForNewObjects => {
            Some(&set_permissions_for_new_objects::METADATA)
        }
        DiagnosticCode::SetPrivilegedMode => Some(&set_privileged_mode::METADATA),
        DiagnosticCode::TransferringParametersBetweenClientAndServer => {
            Some(&transferring_parameters_between_client_and_server::METADATA)
        }
        DiagnosticCode::UnsafeFindByCode => Some(&unsafe_find_by_code::METADATA),
        DiagnosticCode::WeavingAnnotationNotApplicable => {
            Some(&weaving_annotation_not_applicable::METADATA)
        }
        DiagnosticCode::WeavingSignatureMismatch => Some(&weaving_signature_mismatch::METADATA),

        DiagnosticCode::DataExchangeLoading => Some(&data_exchange_loading::METADATA),
        DiagnosticCode::ExecuteExternalCode => Some(&execute_external_code::METADATA),
        DiagnosticCode::ExecuteExternalCodeInCommonModule => {
            Some(&execute_external_code_in_common_module::METADATA)
        }
        DiagnosticCode::RedundantAccessToObject => Some(&redundant_access_to_object::METADATA),
        DiagnosticCode::SameMetadataObjectAndChildNames => {
            Some(&same_metadata_object_and_child_names::METADATA)
        }
        DiagnosticCode::UnusedLocalVariable => Some(&unused_local_variable::METADATA),
        DiagnosticCode::TimeoutsInExternalResources => {
            Some(&timeouts_in_external_resources::METADATA)
        }
        DiagnosticCode::TryNumber => Some(&try_number::METADATA),
        DiagnosticCode::Typo => Some(&typo::METADATA),
        DiagnosticCode::UnknownPreprocessorSymbol => Some(&unknown_preprocessor_symbol::METADATA),
        DiagnosticCode::UnsafeSafeModeMethodCall => Some(&unsafe_safe_mode_method_call::METADATA),
        DiagnosticCode::UsageWriteLogEvent => Some(&usage_write_log_event::METADATA),
        DiagnosticCode::UseSystemInformation => Some(&use_system_information::METADATA),
        DiagnosticCode::UsingCancelParameter => Some(&using_cancel_parameter::METADATA),
        DiagnosticCode::UsingExternalCodeTools => Some(&using_external_code_tools::METADATA),
        DiagnosticCode::UsingFindElementByString => Some(&using_find_element_by_string::METADATA),
        DiagnosticCode::UsingHardcodeNetworkAddress => {
            Some(&using_hardcode_network_address::METADATA)
        }
        DiagnosticCode::UsingHardcodePath => Some(&using_hardcode_path::METADATA),
        DiagnosticCode::UsingHardcodeSecretInformation => {
            Some(&using_hardcode_secret_information::METADATA)
        }
        DiagnosticCode::UsingModalWindows => Some(&using_modal_windows::METADATA),
        DiagnosticCode::UsingObjectNotAvailableUnix => {
            Some(&using_object_not_available_unix::METADATA)
        }
        DiagnosticCode::UsingSynchronousCalls => Some(&using_synchronous_calls::METADATA),
        DiagnosticCode::UsingServiceTag => Some(&using_service_tag::METADATA),
        DiagnosticCode::UsingThisForm => Some(&using_this_form::METADATA),
        DiagnosticCode::WrongDataPathForFormElements => {
            Some(&wrong_data_path_for_form_elements::METADATA)
        }
        DiagnosticCode::WrongHttpServiceHandler => Some(&wrong_http_service_handler::METADATA),
        DiagnosticCode::WrongWebServiceHandler => Some(&wrong_web_service_handler::METADATA),
        DiagnosticCode::WrongUseFunctionProceedWithCall => {
            Some(&wrong_use_function_proceed_with_call::METADATA)
        }

        DiagnosticCode::UnresolvedName => Some(&unresolved_name::METADATA),
        DiagnosticCode::UnresolvedMethodCall => Some(&unresolved_method_call::METADATA),
        DiagnosticCode::UnresolvedBareCall => Some(&unresolved_bare_call::METADATA),
        DiagnosticCode::MismatchedArgCount => Some(&mismatched_arg_count::METADATA),
        DiagnosticCode::TypeMismatch => Some(&type_mismatch::METADATA),
        DiagnosticCode::TypeMismatchByDocComment => Some(&type_mismatch_by_doc_comment::METADATA),
        DiagnosticCode::UnresolvedField => Some(&unresolved_field::METADATA),
        DiagnosticCode::ReadOnlyPropertyAssignment => Some(&read_only_property::METADATA),
        DiagnosticCode::GlobalPropertyNotWritable => Some(&global_property_not_writable::METADATA),
        DiagnosticCode::UnavailableInEnvironment => Some(&unavailable_in_environment::METADATA),
        DiagnosticCode::PlatformMemberNewerThanMinVersion => {
            Some(&platform_member_newer_than_min_version::METADATA)
        }
        DiagnosticCode::PlatformMemberHiddenByCompatibilityMode => {
            Some(&platform_member_hidden_by_compatibility_mode::METADATA)
        }
        DiagnosticCode::ModuleAccessibility => Some(&module_accessibility::METADATA),

        DiagnosticCode::UnknownSuppressionCode => Some(&crate::suppression::UNKNOWN_CODE_METADATA),
        DiagnosticCode::SuppressionWithoutCode => Some(&crate::suppression::WITHOUT_CODE_METADATA),
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn test_ternary_operator_usage_metadata() {
        let meta = get_metadata(DiagnosticCode::TernaryOperatorUsage).unwrap();

        assert_eq!(meta.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(meta.severity, DiagnosticSeverityLevel::Minor);
        assert_eq!(meta.minutes_to_fix, 3);
        assert!(!meta.activated_by_default);
        assert_eq!(meta.tags, &[MetadataTag::Brainoverload]);

        assert_eq!(meta.calculate_severity(), crate::Severity::Information);
    }

    #[test]
    fn test_bad_words_metadata() {
        let meta = get_metadata(DiagnosticCode::BadWords).unwrap();

        assert_eq!(meta.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(meta.severity, DiagnosticSeverityLevel::Major);
        assert_eq!(meta.minutes_to_fix, 1);
        assert!(!meta.activated_by_default);
        assert_eq!(meta.tags, &[MetadataTag::Design]);

        assert_eq!(meta.calculate_severity(), crate::Severity::Warning);
    }

    #[test]
    fn test_fields_from_joins_without_is_null_metadata() {
        let meta = get_metadata(DiagnosticCode::FieldsFromJoinsWithoutIsNull).unwrap();

        assert_eq!(meta.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(meta.severity, DiagnosticSeverityLevel::Critical);
        assert!(!meta.activated_by_default);
        assert_eq!(
            meta.tags,
            &[MetadataTag::Sql, MetadataTag::Suspicious, MetadataTag::Unpredictable]
        );

        assert_eq!(meta.calculate_severity(), crate::Severity::Warning);
    }

    #[test]
    fn test_file_system_access_metadata() {
        let meta = get_metadata(DiagnosticCode::FileSystemAccess).unwrap();

        assert_eq!(meta.diagnostic_type, DiagnosticType::Vulnerability);
        assert_eq!(meta.scope, DiagnosticScope::Bsl);
        assert!(!meta.activated_by_default);

        assert_eq!(meta.calculate_severity(), crate::Severity::Major);
    }

    #[test]
    fn test_function_name_starts_with_get_metadata() {
        let meta = get_metadata(DiagnosticCode::FunctionNameStartsWithGet).unwrap();

        assert_eq!(meta.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(meta.severity, DiagnosticSeverityLevel::Info);
        assert!(!meta.activated_by_default);

        assert_eq!(meta.calculate_severity(), crate::Severity::Hint);
    }

    #[test]
    fn test_all_disabled_by_default_have_metadata() {
        let codes = [
            DiagnosticCode::BadWords,
            DiagnosticCode::CodeAfterAsyncCall,
            DiagnosticCode::DenyIncompleteValues,
            DiagnosticCode::FieldsFromJoinsWithoutIsNull,
            DiagnosticCode::FileSystemAccess,
            DiagnosticCode::FunctionNameStartsWithGet,
            DiagnosticCode::FunctionOutParameter,
            DiagnosticCode::InternetAccess,
            DiagnosticCode::MissingTempStorageDeletion,
            DiagnosticCode::TernaryOperatorUsage,
            DiagnosticCode::TooManyReturns,
            DiagnosticCode::UnresolvedName,
        ];

        for code in codes {
            let meta = get_metadata(code).unwrap();
            assert!(!meta.activated_by_default, "{:?} should be disabled by default", code);
        }
    }

    #[test]
    fn test_deny_incomplete_values_metadata() {
        let meta = get_metadata(DiagnosticCode::DenyIncompleteValues).unwrap();

        assert_eq!(meta.scope, DiagnosticScope::Bsl);
        assert!(meta.can_locate_on_project);
        assert!(!meta.activated_by_default);
    }

    #[test]
    fn test_all_diagnostics_have_metadata() {
        use crate::DiagnosticCode;
        use strum::IntoEnumIterator;

        let mut missing = Vec::new();
        let mut count = 0;

        for code in DiagnosticCode::iter() {
            count += 1;
            if get_metadata(code).is_none() {
                missing.push(code);
            }
        }

        assert!(
            missing.is_empty(),
            "Found {} diagnostics without metadata (total {}): {:#?}",
            missing.len(),
            count,
            missing
        );
    }

    #[test]
    fn test_lsp_severity_mapping() {
        use crate::Severity;

        let data_exchange = get_metadata(DiagnosticCode::DataExchangeLoading).unwrap();
        assert_eq!(data_exchange.diagnostic_type, DiagnosticType::Error);
        assert_eq!(data_exchange.severity, DiagnosticSeverityLevel::Critical);
        assert_eq!(data_exchange.calculate_severity(), Severity::Critical);

        let execute_ext = get_metadata(DiagnosticCode::ExecuteExternalCode).unwrap();
        assert_eq!(execute_ext.diagnostic_type, DiagnosticType::Vulnerability);
        assert_eq!(execute_ext.severity, DiagnosticSeverityLevel::Critical);
        assert_eq!(execute_ext.calculate_severity(), Severity::Critical);

        let redundant = get_metadata(DiagnosticCode::RedundantAccessToObject).unwrap();
        assert_eq!(redundant.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(redundant.severity, DiagnosticSeverityLevel::Info);
        assert_eq!(redundant.calculate_severity(), Severity::Hint);

        let ternary = get_metadata(DiagnosticCode::TernaryOperatorUsage).unwrap();
        assert_eq!(ternary.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(ternary.severity, DiagnosticSeverityLevel::Minor);
        assert_eq!(ternary.calculate_severity(), Severity::Information);

        let unused = get_metadata(DiagnosticCode::UnusedLocalVariable).unwrap();
        assert_eq!(unused.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(unused.severity, DiagnosticSeverityLevel::Major);
        assert_eq!(unused.calculate_severity(), Severity::Warning);

        let privileged = get_metadata(DiagnosticCode::SetPrivilegedMode).unwrap();
        assert_eq!(privileged.diagnostic_type, DiagnosticType::SecurityHotspot);
        assert_eq!(privileged.calculate_severity(), Severity::Warning);
    }

    #[test]
    fn test_tags_coverage() {
        let bad_words = get_metadata(DiagnosticCode::BadWords).unwrap();
        assert!(bad_words.tags.contains(&MetadataTag::Design));

        let same_meta = get_metadata(DiagnosticCode::SameMetadataObjectAndChildNames).unwrap();
        assert!(same_meta.tags.contains(&MetadataTag::Standard));
        assert!(same_meta.tags.contains(&MetadataTag::Sql));
        assert!(same_meta.tags.contains(&MetadataTag::Design));

        let unused = get_metadata(DiagnosticCode::UnusedLocalVariable).unwrap();
        assert!(unused.tags.contains(&MetadataTag::Unused));
        assert!(unused.tags.contains(&MetadataTag::Brainoverload));
        assert!(unused.tags.contains(&MetadataTag::Badpractice));
    }

    #[test]
    fn test_activated_by_default_consistency() {
        let disabled_codes = [
            DiagnosticCode::BadWords,
            DiagnosticCode::CodeAfterAsyncCall,
            DiagnosticCode::DenyIncompleteValues,
            DiagnosticCode::FieldsFromJoinsWithoutIsNull,
            DiagnosticCode::FileSystemAccess,
            DiagnosticCode::FunctionNameStartsWithGet,
            DiagnosticCode::FunctionOutParameter,
            DiagnosticCode::InternetAccess,
            DiagnosticCode::MissingTempStorageDeletion,
            DiagnosticCode::TernaryOperatorUsage,
            DiagnosticCode::TooManyReturns,
            DiagnosticCode::TypeMismatchByDocComment,
            DiagnosticCode::UnresolvedName,
        ];

        for code in disabled_codes {
            let meta = get_metadata(code).unwrap();
            assert!(!meta.activated_by_default, "{:?} should be disabled by default", code);
        }

        let enabled_codes = [
            DiagnosticCode::DataExchangeLoading,
            DiagnosticCode::ExecuteExternalCode,
            DiagnosticCode::RedundantAccessToObject,
            DiagnosticCode::SameMetadataObjectAndChildNames,
            DiagnosticCode::UnusedLocalVariable,
        ];

        for code in enabled_codes {
            let meta = get_metadata(code).unwrap();
            assert!(meta.activated_by_default, "{:?} should be enabled by default", code);
        }
    }

    #[test]
    fn test_scope_consistency() {
        let data_exchange = get_metadata(DiagnosticCode::DataExchangeLoading).unwrap();
        assert_eq!(data_exchange.scope, DiagnosticScope::Bsl);

        let unused = get_metadata(DiagnosticCode::UnusedLocalVariable).unwrap();
        assert_eq!(unused.scope, DiagnosticScope::All);
    }

    #[test]
    fn test_can_locate_on_project() {
        let same_meta = get_metadata(DiagnosticCode::SameMetadataObjectAndChildNames).unwrap();
        assert!(same_meta.can_locate_on_project);

        let unused = get_metadata(DiagnosticCode::UnusedLocalVariable).unwrap();
        assert!(!unused.can_locate_on_project);
    }

    #[test]
    fn test_minutes_to_fix_reasonable() {
        use strum::IntoEnumIterator;

        for code in DiagnosticCode::iter() {
            if let Some(meta) = get_metadata(code) {
                assert!(
                    meta.minutes_to_fix >= 1 && meta.minutes_to_fix <= 60,
                    "{:?} has unreasonable minutes_to_fix: {}",
                    code,
                    meta.minutes_to_fix
                );
            }
        }
    }

    #[test]
    fn test_new_diagnostics_metadata() {
        let data_exchange = get_metadata(DiagnosticCode::DataExchangeLoading).unwrap();
        assert_eq!(data_exchange.diagnostic_type, DiagnosticType::Error);
        assert_eq!(data_exchange.severity, DiagnosticSeverityLevel::Critical);
        assert_eq!(data_exchange.minutes_to_fix, 5);
        assert!(data_exchange.tags.contains(&MetadataTag::Standard));

        let execute_ext = get_metadata(DiagnosticCode::ExecuteExternalCode).unwrap();
        assert_eq!(execute_ext.diagnostic_type, DiagnosticType::Vulnerability);
        assert_eq!(execute_ext.severity, DiagnosticSeverityLevel::Critical);
        assert!(execute_ext.tags.contains(&MetadataTag::Error));

        let redundant = get_metadata(DiagnosticCode::RedundantAccessToObject).unwrap();
        assert_eq!(redundant.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(redundant.severity, DiagnosticSeverityLevel::Info);
        assert!(redundant.tags.contains(&MetadataTag::Clumsy));

        let same_meta = get_metadata(DiagnosticCode::SameMetadataObjectAndChildNames).unwrap();
        assert_eq!(same_meta.diagnostic_type, DiagnosticType::Error);
        assert_eq!(same_meta.minutes_to_fix, 30);
        assert!(same_meta.can_locate_on_project);

        let unused = get_metadata(DiagnosticCode::UnusedLocalVariable).unwrap();
        assert_eq!(unused.diagnostic_type, DiagnosticType::CodeSmell);
        assert_eq!(unused.severity, DiagnosticSeverityLevel::Major);
        assert!(unused.tags.contains(&MetadataTag::Unused));
    }
}
