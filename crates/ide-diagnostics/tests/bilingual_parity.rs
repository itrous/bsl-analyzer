use std::collections::HashSet;

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use test_fixture::Fixture;

const IDENTIFIER_AWARE_CODES: &[DiagnosticCode] = &[
    DiagnosticCode::BeginTransactionBeforeTryCatch,
    DiagnosticCode::CommitTransactionOutsideTryCatch,
    DiagnosticCode::CreateQueryInCycle,
    DiagnosticCode::DataExchangeLoading,
    DiagnosticCode::DisableSafeMode,
    DiagnosticCode::ExecuteExternalCode,
    DiagnosticCode::ExternalAppStarting,
    DiagnosticCode::FileSystemAccess,
    DiagnosticCode::FormDataToValue,
    DiagnosticCode::InternetAccess,
    DiagnosticCode::IsInRoleMethod,
    DiagnosticCode::MissingTempStorageDeletion,
    DiagnosticCode::MissingTemporaryFileDeletion,
    DiagnosticCode::NestedConstructorsInStructureDeclaration,
    DiagnosticCode::NumberOfValuesInStructureConstructor,
    DiagnosticCode::OSUsersMethod,
    DiagnosticCode::SetPrivilegedMode,
    DiagnosticCode::TryNumber,
    DiagnosticCode::UnsafeSafeModeMethodCall,
    DiagnosticCode::UseSystemInformation,
    DiagnosticCode::UsingExternalCodeTools,
    DiagnosticCode::UsingFindElementByString,
    DiagnosticCode::UsingHardcodeSecretInformation,
    DiagnosticCode::WrongUseFunctionProceedWithCall,
    DiagnosticCode::WrongUseOfRollbackTransactionMethod,
];

const EXCLUSIONS_DOCUMENTED: &[(DiagnosticCode, &str)] = &[
    (
        DiagnosticCode::UnknownSuppressionCode,
        "reports a typo'd code in a suppression comment directive; no BSL identifier lookup",
    ),
    (
        DiagnosticCode::SuppressionWithoutCode,
        "reports a code-less suppression comment directive; no BSL identifier lookup",
    ),
    (
        DiagnosticCode::WeavingSignatureMismatch,
        "structural signature comparison across an ext/base module pair; no identifier lookup",
    ),
    (
        DiagnosticCode::WeavingAnnotationNotApplicable,
        "structural annotation/method-kind check across an ext/base module pair; no identifier lookup",
    ),
    (DiagnosticCode::ParseError, "structural parser diagnostic; no identifier recognition"),
    (
        DiagnosticCode::CanonicalSpellingKeywords,
        "keyword spelling policy, not platform identifier lookup",
    ),
    (DiagnosticCode::ConsecutiveEmptyLines, "formatting-only diagnostic"),
    (
        DiagnosticCode::MultilinePreprocessorInstruction,
        "line-layout check on preprocessor instructions; no identifier lookup",
    ),
    (DiagnosticCode::LineLength, "formatting-only diagnostic"),
    (
        DiagnosticCode::GlobalContextMethodConflict,
        "form declaration checked against the platform global catalog; does not inspect a bilingual use-site",
    ),
    (DiagnosticCode::MissingSpace, "formatting-only diagnostic"),
    (DiagnosticCode::OneStatementPerLine, "statement layout diagnostic"),
    (DiagnosticCode::SemicolonPresence, "punctuation/style diagnostic"),
    (DiagnosticCode::SpaceAtStartComment, "comment formatting diagnostic"),
    (DiagnosticCode::IncorrectLineBreak, "line-break formatting diagnostic"),
    (
        DiagnosticCode::IncorrectUseOfStrTemplate,
        "known bug: message/trigger is tied to concrete StrTemplate call text",
    ),
    (DiagnosticCode::ExtraCommas, "punctuation diagnostic"),
    (DiagnosticCode::CommentedCode, "comment-content heuristic; no platform identifier lookup"),
    (DiagnosticCode::EmptyCodeBlock, "structural empty-block diagnostic"),
    (DiagnosticCode::EmptyRegion, "region structure diagnostic"),
    (DiagnosticCode::EmptyStatement, "structural empty-statement diagnostic"),
    (DiagnosticCode::UnreachableCode, "control-flow diagnostic"),
    (
        DiagnosticCode::MisplacedLoopControl,
        "structural keyword-context diagnostic (Прервать/Продолжить вне цикла); no platform identifier lookup",
    ),
    (DiagnosticCode::CodeBlockBeforeSub, "module layout diagnostic"),
    (DiagnosticCode::CodeOutOfRegion, "region placement diagnostic"),
    (DiagnosticCode::MagicNumber, "literal-value diagnostic"),
    (DiagnosticCode::MagicDate, "literal-value diagnostic"),
    (DiagnosticCode::YoLetterUsage, "orthography diagnostic"),
    (DiagnosticCode::LatinAndCyrillicSymbolInWord, "mixed-script lexical diagnostic"),
    (DiagnosticCode::InvalidCharacterInFile, "lexical character diagnostic"),
    (DiagnosticCode::DoubleNegatives, "expression-shape diagnostic"),
    (DiagnosticCode::NestedTernaryOperator, "expression-shape diagnostic"),
    (DiagnosticCode::PostfixAccessOnExpression, "expression-shape diagnostic"),
    (
        DiagnosticCode::NonExportMethodsInApiRegion,
        "region/export policy; no platform bilingual identifier lookup",
    ),
    (DiagnosticCode::TernaryOperatorUsage, "expression-style diagnostic"),
    (DiagnosticCode::UnaryPlusInConcatenation, "operator-use diagnostic"),
    (DiagnosticCode::UselessTernaryOperator, "expression simplification diagnostic"),
    (DiagnosticCode::BadWords, "lexical dictionary diagnostic"),
    (DiagnosticCode::DuplicateStringLiteral, "literal duplication diagnostic"),
    (DiagnosticCode::DuplicateRegion, "region-name duplication diagnostic"),
    (DiagnosticCode::NonStandardRegion, "region-name policy diagnostic"),
    (
        DiagnosticCode::DuplicatedInsertionIntoCollection,
        "known bug: message embeds collection/value expressions",
    ),
    (
        DiagnosticCode::ExcessiveAutoTestCheck,
        "test-marker heuristic; no platform identifier lookup",
    ),
    (DiagnosticCode::IdenticalExpressions, "expression-equivalence diagnostic"),
    (DiagnosticCode::IfElseDuplicatedCodeBlock, "block-structure diagnostic"),
    (DiagnosticCode::IfElseDuplicatedCondition, "condition-structure diagnostic"),
    (DiagnosticCode::IfElseIfEndsWithElse, "branch-structure diagnostic"),
    (
        DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
        "localized string literal diagnostic",
    ),
    (DiagnosticCode::MultilingualStringUsingWithTemplate, "localized string/template diagnostic"),
    (DiagnosticCode::NestedFunctionInParameters, "call-shape diagnostic with configurable names"),
    (DiagnosticCode::Typo, "spellcheck diagnostic"),
    (DiagnosticCode::AllFunctionPathMustHaveReturn, "control-flow return diagnostic"),
    (DiagnosticCode::FunctionShouldHaveReturn, "function body return diagnostic"),
    (DiagnosticCode::ProcedureReturnsValue, "procedure/function-kind diagnostic"),
    (DiagnosticCode::FunctionReturnsSamePrimitive, "return-value diagnostic"),
    (
        DiagnosticCode::FunctionNameStartsWithGet,
        "naming convention diagnostic; no RU/EN platform lookup",
    ),
    (DiagnosticCode::TooManyReturns, "control-flow count diagnostic"),
    (DiagnosticCode::NumberOfParams, "signature-size diagnostic"),
    (DiagnosticCode::NumberOfOptionalParams, "signature-shape diagnostic"),
    (DiagnosticCode::OrderOfParams, "parameter ordering diagnostic"),
    (
        DiagnosticCode::MissedRequiredParameter,
        "known bug: message/resolution depends on concrete callee metadata",
    ),
    (DiagnosticCode::FunctionOutParameter, "parameter mutation style diagnostic"),
    (DiagnosticCode::UnusedParameters, "dataflow liveness diagnostic"),
    (DiagnosticCode::MissingParameterDescription, "documentation diagnostic"),
    (DiagnosticCode::MissingReturnedValueDescription, "documentation diagnostic"),
    (DiagnosticCode::ReservedParameterNames, "reserved-name convention diagnostic"),
    (DiagnosticCode::ReservedWordAsMethodName, "reserved keyword diagnostic"),
    (DiagnosticCode::RewriteMethodParameter, "parameter reassignment diagnostic"),
    (DiagnosticCode::UnusedLocalVariable, "dataflow liveness diagnostic"),
    (DiagnosticCode::UnusedLocalMethod, "symbol liveness diagnostic"),
    (DiagnosticCode::ExportVariables, "exported variable policy diagnostic"),
    (DiagnosticCode::MissingVariablesDescription, "documentation diagnostic"),
    (DiagnosticCode::SelfAssign, "expression-equivalence diagnostic"),
    (DiagnosticCode::ThisObjectAssign, "assignment target diagnostic"),
    (DiagnosticCode::CyclomaticComplexity, "metric diagnostic"),
    (DiagnosticCode::CognitiveComplexity, "metric diagnostic"),
    (DiagnosticCode::NestedStatements, "metric diagnostic"),
    (DiagnosticCode::MethodSize, "metric diagnostic"),
    (DiagnosticCode::IfConditionComplexity, "metric diagnostic"),
    (DiagnosticCode::MissingCodeTryCatchEx, "exception-handler structure diagnostic"),
    (DiagnosticCode::UseLessForEach, "loop-shape diagnostic"),
    (DiagnosticCode::UsingGoto, "control-flow style diagnostic"),
    (
        DiagnosticCode::CodeAfterAsyncCall,
        "known bug: message embeds the original RU/EN async method name",
    ),
    (DiagnosticCode::CompilationDirectiveLost, "directive/module-context diagnostic"),
    (DiagnosticCode::CompilationDirectiveNeedLess, "directive/module-context diagnostic"),
    (DiagnosticCode::DeletingCollectionItem, "collection loop mutation diagnostic"),
    (DiagnosticCode::SelfInsertion, "collection self-insertion diagnostic"),
    (DiagnosticCode::SeveralCompilerDirectives, "directive structure diagnostic"),
    (DiagnosticCode::StyleElementConstructors, "known bug: message embeds style type names"),
    (
        DiagnosticCode::DeprecatedPlatformApi,
        "known bug: message embeds deprecated platform API names and replacements",
    ),
    (
        DiagnosticCode::DeprecatedMethodCall,
        "known bug: message embeds deprecated method names and metadata text",
    ),
    (
        DiagnosticCode::ExecuteExternalCodeInCommonModule,
        "metadata CommonModule policy; not source-only RU/EN parity",
    ),
    (DiagnosticCode::TempFilesDir, "known bug: RU and EN messages intentionally differ today"),
    (DiagnosticCode::GetFormMethod, "known bug: message embeds the original RU/EN method name"),
    (
        DiagnosticCode::GlobalContextMethodCollision8312,
        "known bug: message embeds colliding global method name",
    ),
    (
        DiagnosticCode::PairingBrokenTransaction,
        "transaction balance diagnostic; no distinct identifier parity fixture",
    ),
    (
        DiagnosticCode::TimeoutsInExternalResources,
        "field-value policy; no bilingual method-name lookup",
    ),
    (DiagnosticCode::UnknownPreprocessorSymbol, "preprocessor symbol diagnostic"),
    (DiagnosticCode::UsageWriteLogEvent, "argument-shape diagnostic"),
    (DiagnosticCode::UsingCancelParameter, "parameter usage diagnostic"),
    (DiagnosticCode::UsingHardcodeNetworkAddress, "literal network-address diagnostic"),
    (DiagnosticCode::UsingHardcodePath, "literal filesystem-path diagnostic"),
    (
        DiagnosticCode::UsingModalWindows,
        "known bug: message embeds original modal method and replacement names",
    ),
    (
        DiagnosticCode::UsingObjectNotAvailableUnix,
        "known bug: message embeds constructor type name",
    ),
    (
        DiagnosticCode::UsingSynchronousCalls,
        "known bug: message embeds original synchronous method and replacement names",
    ),
    (DiagnosticCode::UsingServiceTag, "comment tag diagnostic"),
    (
        DiagnosticCode::UsingThisForm,
        "known bug: identifier-aware but tied to form-module metadata context",
    ),
    (
        DiagnosticCode::WrongHttpServiceHandler,
        "metadata handler-name resolution; requires service metadata fixture",
    ),
    (
        DiagnosticCode::WrongWebServiceHandler,
        "metadata handler-name resolution; requires service metadata fixture",
    ),
    (DiagnosticCode::WrongDataPathForFormElements, "form metadata path diagnostic"),
    (DiagnosticCode::PublicMethodsDescription, "documentation/region diagnostic"),
    (DiagnosticCode::CachedPublic, "metadata return-value-reuse policy"),
    (DiagnosticCode::CommandModuleExportMethods, "metadata command-module policy"),
    (
        DiagnosticCode::CommonModuleAssign,
        "known bug: message embeds concrete CommonModule binding name",
    ),
    (DiagnosticCode::CommonModuleInvalidType, "metadata CommonModule type policy"),
    (DiagnosticCode::CommonModuleMissingAPI, "region policy for CommonModule API"),
    (DiagnosticCode::CommonModuleNameCached, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameClient, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameClientServer, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameFullAccess, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameGlobal, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameGlobalClient, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameServerCall, "metadata CommonModule naming policy"),
    (DiagnosticCode::CommonModuleNameWords, "metadata CommonModule naming policy"),
    (DiagnosticCode::DenyIncompleteValues, "metadata register dimension policy"),
    (DiagnosticCode::ForbiddenMetadataName, "metadata object naming policy"),
    (DiagnosticCode::MetadataObjectNameLength, "metadata object length policy"),
    (DiagnosticCode::MissingCommonModuleMethod, "known bug: message embeds module/method names"),
    (
        DiagnosticCode::MissingEventSubscriptionHandler,
        "metadata event subscription handler diagnostic",
    ),
    (DiagnosticCode::OrdinaryAppSupport, "configuration compatibility diagnostic"),
    (
        DiagnosticCode::PrivilegedModuleMethodCall,
        "metadata/cross-module call diagnostic; requires privileged module fixture",
    ),
    (DiagnosticCode::ProtectedModule, "configuration module protection diagnostic"),
    (
        DiagnosticCode::RedundantAccessToObject,
        "known bug: message embeds metadata object/module names",
    ),
    (DiagnosticCode::SameMetadataObjectAndChildNames, "metadata parent/child naming diagnostic"),
    (DiagnosticCode::ScheduledJobHandler, "metadata scheduled-job handler diagnostic"),
    (DiagnosticCode::ServerCallsInFormEvents, "metadata form event/cross-module diagnostic"),
    (DiagnosticCode::ServerSideExportFormMethod, "form module export policy"),
    (DiagnosticCode::SetPermissionsForNewObjects, "metadata managed-application policy"),
    (
        DiagnosticCode::TransferringParametersBetweenClientAndServer,
        "annotation/parameter transfer diagnostic",
    ),
    (DiagnosticCode::UnsafeFindByCode, "known bug: message embeds manager/object names"),
    (
        DiagnosticCode::UnresolvedName,
        "message names the unresolved root exactly as the source spells it, by design",
    ),
    (DiagnosticCode::UnresolvedMethodCall, "known bug: message embeds receiver and method names"),
    (
        DiagnosticCode::UnresolvedBareCall,
        "message names the absent callee exactly as the source spells it, by design",
    ),
    (
        DiagnosticCode::PlatformMemberNewerThanMinVersion,
        "message names the member as the source spells it and the two versions, by design",
    ),
    (
        DiagnosticCode::PlatformMemberHiddenByCompatibilityMode,
        "message names the member as the source spells it and the two modes, by design",
    ),
    (
        DiagnosticCode::MismatchedArgCount,
        "known bug: message embeds argument counts from resolved callee",
    ),
    (DiagnosticCode::TypeMismatch, "known bug: message embeds localized type names"),
    (DiagnosticCode::TypeMismatchByDocComment, "known bug: message embeds localized type names"),
    (DiagnosticCode::UnresolvedField, "known bug: message embeds field/type names"),
    (DiagnosticCode::ReadOnlyPropertyAssignment, "known bug: message embeds property/type names"),
    (
        DiagnosticCode::GlobalPropertyNotWritable,
        "message names the property as the source spells it, by design",
    ),
    (
        DiagnosticCode::UnavailableInEnvironment,
        "message embeds the member name and environment qualifiers by design",
    ),
    (
        DiagnosticCode::ModuleAccessibility,
        "message embeds the module/method name and environment qualifiers by design",
    ),
    (
        DiagnosticCode::AmbiguousFieldInQuery,
        "SDBL name-resolution diagnostic; requires configuration metadata fixture",
    ),
    (
        DiagnosticCode::AssignAliasFieldsInQuery,
        "SDBL alias policy; query-language parity needs dedicated metadata/query harness",
    ),
    (DiagnosticCode::DuplicateAliasInQuery, "SDBL source-alias policy; not BSL identifier parity"),
    (
        DiagnosticCode::FieldsFromJoinsWithoutIsNull,
        "SDBL join-field policy; not BSL identifier parity",
    ),
    (DiagnosticCode::FullOuterJoinQuery, "SDBL join-kind policy; not BSL identifier parity"),
    (
        DiagnosticCode::IncorrectUseLikeInQuery,
        "SDBL LIKE pattern policy; not BSL identifier parity",
    ),
    (DiagnosticCode::JoinWithSubQuery, "SDBL subquery join policy; not BSL identifier parity"),
    (DiagnosticCode::JoinWithVirtualTable, "SDBL virtual table policy; not BSL identifier parity"),
    (
        DiagnosticCode::LogicalOrInJoinQuerySection,
        "SDBL boolean-expression policy; not BSL identifier parity",
    ),
    (
        DiagnosticCode::LogicalOrInTheWhereSectionOfQuery,
        "SDBL boolean-expression policy; not BSL identifier parity",
    ),
    (
        DiagnosticCode::MultilineStringInQuery,
        "SDBL string literal policy; not BSL identifier parity",
    ),
    (
        DiagnosticCode::QueryNestedFieldsByDot,
        "SDBL field-path policy; query metadata parity needs dedicated harness",
    ),
    (DiagnosticCode::QueryParseError, "SDBL parser diagnostic; no BSL platform identifier lookup"),
    (
        DiagnosticCode::QueryToMissingMetadata,
        "SDBL metadata resolution diagnostic; requires configuration metadata fixture",
    ),
    (
        DiagnosticCode::UnknownFieldInQuery,
        "SDBL field resolution diagnostic; requires configuration metadata fixture",
    ),
    (
        DiagnosticCode::UnlimitedLengthStringUsageInQuery,
        "SDBL field-type diagnostic; requires configuration metadata fixture",
    ),
    (DiagnosticCode::RefOveruse, "SDBL reference-overuse policy; not BSL identifier parity"),
    (DiagnosticCode::SelectTopWithoutOrderBy, "SDBL ordering policy; not BSL identifier parity"),
    (DiagnosticCode::UnionAll, "SDBL union policy; not BSL identifier parity"),
    (DiagnosticCode::UsingLikeInQuery, "SDBL LIKE policy; not BSL identifier parity"),
    (
        DiagnosticCode::VirtualTableCallWithoutParameters,
        "SDBL virtual table parameter policy; not BSL identifier parity",
    ),
];

const BILINGUAL_FIXTURES: &[(DiagnosticCode, &str, &str)] = &[
    (
        DiagnosticCode::BeginTransactionBeforeTryCatch,
        r#"Процедура Тест()
    Попытка
        НачатьТранзакцию();
    Исключение
    КонецПопытки;
КонецПроцедуры"#,
        r#"Procedure Test()
    Try
        BeginTransaction();
    Except
    EndTry;
EndProcedure"#,
    ),
    (
        DiagnosticCode::CommitTransactionOutsideTryCatch,
        r#"Процедура Тест()
    ЗафиксироватьТранзакцию();
КонецПроцедуры"#,
        r#"Procedure Test()
    CommitTransaction();
EndProcedure"#,
    ),
    (
        DiagnosticCode::CreateQueryInCycle,
        r#"Процедура Тест()
    Запрос = Новый Запрос("ВЫБРАТЬ 1");
    Для Индекс = 1 По 2 Цикл
        Запрос.Выполнить();
    КонецЦикла;
КонецПроцедуры"#,
        r#"Procedure Test()
    Query = New Query("SELECT 1");
    For Index = 1 To 2 Do
        Query.Execute();
    EndDo;
EndProcedure"#,
    ),
    (
        DiagnosticCode::DataExchangeLoading,
        r#"Процедура ПередЗаписью(Отказ)
    Значение = 1;
КонецПроцедуры"#,
        r#"Procedure BeforeWrite(Cancel)
    Value = 1;
EndProcedure"#,
    ),
    (
        DiagnosticCode::DisableSafeMode,
        r#"Процедура Тест()
    УстановитьБезопасныйРежим(Ложь);
КонецПроцедуры"#,
        r#"Procedure Test()
    SetSafeMode(False);
EndProcedure"#,
    ),
    (
        DiagnosticCode::ExecuteExternalCode,
        r#"Процедура Тест()
    Вычислить("1");
КонецПроцедуры"#,
        r#"Procedure Test()
    Eval("1");
EndProcedure"#,
    ),
    (
        DiagnosticCode::ExternalAppStarting,
        r#"Процедура Тест()
    ЗапуститьПриложение("calc.exe");
КонецПроцедуры"#,
        r#"Procedure Test()
    RunApp("calc.exe");
EndProcedure"#,
    ),
    (
        DiagnosticCode::FileSystemAccess,
        r#"Процедура Тест()
    Объект = Новый Файл("a.txt");
КонецПроцедуры"#,
        r#"Procedure Test()
    Object = New File("a.txt");
EndProcedure"#,
    ),
    (
        DiagnosticCode::FormDataToValue,
        r#"Процедура Тест()
    ДанныеФормыВЗначение(Объект, Тип("СправочникОбъект.Товары"));
КонецПроцедуры"#,
        r#"Procedure Test()
    FormDataToValue(Object, Type("CatalogObject.Items"));
EndProcedure"#,
    ),
    (
        DiagnosticCode::InternetAccess,
        r#"Процедура Тест()
    Соединение = Новый HTTPСоединение("example.com", 80);
КонецПроцедуры"#,
        r#"Procedure Test()
    Connection = New HTTPConnection("example.com", 80);
EndProcedure"#,
    ),
    (
        DiagnosticCode::IsInRoleMethod,
        r#"Процедура Тест()
    Если РольДоступна("Администратор") Тогда
        Значение = 1;
    КонецЕсли;
КонецПроцедуры"#,
        r#"Procedure Test()
    If IsInRole("Administrator") Then
        Value = 1;
    EndIf;
EndProcedure"#,
    ),
    (
        DiagnosticCode::MissingTempStorageDeletion,
        r#"Процедура Тест()
    Адрес = ПоместитьВоВременноеХранилище(1);
    Значение = ПолучитьИзВременногоХранилища(Адрес);
КонецПроцедуры"#,
        r#"Procedure Test()
    Address = PutToTempStorage(1);
    Value = GetFromTempStorage(Address);
EndProcedure"#,
    ),
    (
        DiagnosticCode::MissingTemporaryFileDeletion,
        r#"Процедура Тест()
    Tmp = ПолучитьИмяВременногоФайла();
КонецПроцедуры"#,
        r#"Procedure Test()
    Tmp = GetTempFileName();
EndProcedure"#,
    ),
    (
        DiagnosticCode::NestedConstructorsInStructureDeclaration,
        r#"Процедура Тест()
    Значение = Новый Структура("Поле", Новый Структура("Код"), 1);
КонецПроцедуры"#,
        r#"Procedure Test()
    Value = New Structure("Field", New Structure("Code"), 1);
EndProcedure"#,
    ),
    (
        DiagnosticCode::NumberOfValuesInStructureConstructor,
        r#"Процедура Тест()
    Значение = Новый Структура("А, Б, В", 1, 2, 3, 4);
КонецПроцедуры"#,
        r#"Procedure Test()
    Value = New Structure("A, B, C", 1, 2, 3, 4);
EndProcedure"#,
    ),
    (
        DiagnosticCode::OSUsersMethod,
        r#"Процедура Тест()
    ПользователиОС();
КонецПроцедуры"#,
        r#"Procedure Test()
    OSUsers();
EndProcedure"#,
    ),
    (
        DiagnosticCode::SetPrivilegedMode,
        r#"Процедура Тест()
    УстановитьПривилегированныйРежим(Истина);
КонецПроцедуры"#,
        r#"Procedure Test()
    SetPrivilegedMode(True);
EndProcedure"#,
    ),
    (
        DiagnosticCode::TryNumber,
        r#"Процедура Тест()
    Попытка
        Значение = Число("1");
    Исключение
    КонецПопытки;
КонецПроцедуры"#,
        r#"Procedure Test()
    Try
        Value = Number("1");
    Except
    EndTry;
EndProcedure"#,
    ),
    (
        DiagnosticCode::UnsafeSafeModeMethodCall,
        r#"Процедура Тест()
    Если БезопасныйРежим() Тогда
        Значение = 1;
    КонецЕсли;
КонецПроцедуры"#,
        r#"Procedure Test()
    If SafeMode() Then
        Value = 1;
    EndIf;
EndProcedure"#,
    ),
    (
        DiagnosticCode::UseSystemInformation,
        r#"Процедура Тест()
    Сведения = Новый СистемнаяИнформация();
КонецПроцедуры"#,
        r#"Procedure Test()
    Info = New SystemInfo();
EndProcedure"#,
    ),
    (
        DiagnosticCode::UsingExternalCodeTools,
        r#"Процедура Тест()
    ВнешниеОбработки.Создать("tools.epf");
КонецПроцедуры"#,
        r#"Procedure Test()
    ExternalDataProcessors.Create("tools.epf");
EndProcedure"#,
    ),
    (
        DiagnosticCode::UsingFindElementByString,
        r#"Процедура Тест()
    Значение = Справочники.Товары.НайтиПоКоду("1");
КонецПроцедуры"#,
        r#"Procedure Test()
    Value = Catalogs.Items.FindByCode("1");
EndProcedure"#,
    ),
    (
        DiagnosticCode::UsingHardcodeSecretInformation,
        r#"Процедура Тест()
    Параметры = Новый Структура("Password", "secret");
КонецПроцедуры"#,
        r#"Procedure Test()
    Params = New Structure("Password", "secret");
EndProcedure"#,
    ),
    (
        DiagnosticCode::WrongUseFunctionProceedWithCall,
        r#"Процедура Тест()
    ПродолжитьВызов();
КонецПроцедуры"#,
        r#"Procedure Test()
    ProceedWithCall();
EndProcedure"#,
    ),
    (
        DiagnosticCode::WrongUseOfRollbackTransactionMethod,
        r#"Процедура Тест()
    ОтменитьТранзакцию();
КонецПроцедуры"#,
        r#"Procedure Test()
    RollbackTransaction();
EndProcedure"#,
    ),
];

#[test]
fn bilingual_inventory_has_expected_size() {
    let all = all_codes();
    assert_eq!(all.len(), 199, "update the Track 3 Phase E inventory when DiagnosticCode changes");
}

#[test]
fn coverage_invariant_forces_identifier_decision() {
    let all = all_codes();
    let allow: HashSet<_> = IDENTIFIER_AWARE_CODES.iter().copied().collect();
    let exclusions: HashSet<_> = EXCLUSIONS_DOCUMENTED.iter().map(|(code, _)| *code).collect();

    assert_eq!(allow.len(), IDENTIFIER_AWARE_CODES.len(), "duplicate allowlist entries");
    assert_eq!(exclusions.len(), EXCLUSIONS_DOCUMENTED.len(), "duplicate exclusion entries");
    assert!(allow.is_disjoint(&exclusions), "code cannot be both allowlisted and excluded");

    for code in &all {
        assert!(
            allow.contains(code) || exclusions.contains(code),
            "{code:?} must be added to IDENTIFIER_AWARE_CODES or EXCLUSIONS_DOCUMENTED",
        );
    }

    assert_eq!(
        IDENTIFIER_AWARE_CODES.len() + EXCLUSIONS_DOCUMENTED.len(),
        all.len(),
        "bilingual parity inventory must account for every DiagnosticCode",
    );
}

#[test]
fn every_identifier_aware_code_has_a_fixture() {
    for code in IDENTIFIER_AWARE_CODES {
        assert!(
            BILINGUAL_FIXTURES.iter().any(|(fixture_code, _, _)| fixture_code == code),
            "{code:?} is allowlisted but has no bilingual parity fixture",
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn bilingual_identifier_fixtures_have_parity() {
    for (code, ru_source, en_source) in BILINGUAL_FIXTURES {
        assert!(
            IDENTIFIER_AWARE_CODES.contains(code),
            "{code:?} has a fixture but is not allowlisted",
        );

        let ru = normalized_diagnostics_for(*code, ru_source);
        let en = normalized_diagnostics_for(*code, en_source);

        assert!(!ru.is_empty(), "{code:?} RU fixture did not emit the diagnostic");
        assert!(!en.is_empty(), "{code:?} EN fixture did not emit the diagnostic");
        assert_eq!(ru, en, "{code:?} RU/EN diagnostic parity mismatch");
    }
}

fn normalized_diagnostics_for(
    code: DiagnosticCode,
    source: &str,
) -> Vec<(DiagnosticCode, String, String)> {
    let mut diagnostics = run_diagnostics(source)
        .into_iter()
        .filter(|diag| diag.code == code)
        .map(|diag| (diag.code, format!("{:?}", diag.severity), normalize_message(&diag.message)))
        .collect::<Vec<_>>();

    diagnostics.sort_by(|left, right| {
        (left.0.as_str(), left.1.as_str(), left.2.as_str()).cmp(&(
            right.0.as_str(),
            right.1.as_str(),
            right.2.as_str(),
        ))
    });
    diagnostics
}

fn run_diagnostics(source: &str) -> Vec<Diagnostic> {
    let source = source.replace("<CURSOR>", "");
    let fixture_text =
        if source.contains("//- ") { source } else { format!("//- /test.bsl\n{source}") };
    let fixture = Fixture::parse(&fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();

    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let file_id = *fixture.files.keys().last().expect("fixture should contain a test file");
    let config = DiagnosticsConfig::all_enabled();
    let provider = SalsaProvider::new(&db, None);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    ide_diagnostics::diagnostics(&ctx)
}

fn normalize_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn all_codes() -> Vec<DiagnosticCode> {
    ide_diagnostics::all_diagnostic_codes().collect()
}
