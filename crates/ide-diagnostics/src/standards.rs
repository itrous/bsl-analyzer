//! Нормативный источник диагностики: номера стандартов разработки 1С.
//!
//! Номер стандарта — характеристика правила, а не синтаксиса и не вывода типов,
//! поэтому таблица живёт рядом с остальными метаданными диагностик, а адаптеры
//! (LSP, MCP, SARIF) остаются тонкими проекциями над ней.
//!
//! Таблица отделена от [`crate::metadata::DiagnosticMetadata`] сознательно.
//! Метаданные собираются макросом с полями по умолчанию, и пропущенное поле
//! молча дало бы пустой результат — то есть «стандарта нет» было бы неотличимо
//! от «решение не принято». Исчерпывающий `match` без ветви-заглушки переносит
//! это различие на компилятор: новый вариант [`DiagnosticCode`] не соберётся,
//! пока автор не запишет решение.

use crate::DiagnosticCode;

/// Публичное зеркало стандартов под CC0: доступно без входа в ИТС, в отличие от
/// `its.1c.ru`, которым размечены доки диагностик.
const STD_URL_PREFIX: &str = "https://v8std.ru/std/";

/// Ссылка на страницу стандарта по его номеру.
pub fn standard_url(id: u16) -> String {
    format!("{STD_URL_PREFIX}{id}/")
}

/// Номера стандартов разработки 1С, на которых стоит требование диагностики.
///
/// Порядок содержательный, а не числовой: первым идёт стандарт, чьё требование
/// диагностика проверяет, за ним — смежные, поясняющие контекст. На первый
/// номер указывает `codeDescription` в LSP и `helpUri` в SARIF, поэтому
/// перестановка меняет наблюдаемое поведение.
///
/// Пустой срез означает принятое решение «нормативного источника-стандарта
/// нет», а не пропуск. Часть диагностик опирается на документацию платформы или
/// на правила сообщества, у которых нет номера стандарта; такие источники
/// остаются в доках диагностики и в эту таблицу не переносятся.
pub fn standards(code: DiagnosticCode) -> &'static [u16] {
    match code {
        DiagnosticCode::ParseError => &[],
        DiagnosticCode::CanonicalSpellingKeywords => &[441],
        DiagnosticCode::ConsecutiveEmptyLines => &[],
        DiagnosticCode::MultilinePreprocessorInstruction => &[],
        DiagnosticCode::LineLength => &[456],
        DiagnosticCode::MissingSpace => &[],
        DiagnosticCode::OneStatementPerLine => &[456],
        DiagnosticCode::SemicolonPresence => &[456],
        DiagnosticCode::SpaceAtStartComment => &[456],
        DiagnosticCode::IncorrectLineBreak => &[444],
        DiagnosticCode::IncorrectUseOfStrTemplate => &[],
        DiagnosticCode::ExtraCommas => &[640],
        DiagnosticCode::CommentedCode => &[456],
        DiagnosticCode::EmptyCodeBlock => &[],
        DiagnosticCode::EmptyRegion => &[455],
        DiagnosticCode::EmptyStatement => &[],
        DiagnosticCode::UnreachableCode => &[],
        DiagnosticCode::CodeBlockBeforeSub => &[455],
        DiagnosticCode::CodeOutOfRegion => &[455],
        DiagnosticCode::MagicNumber => &[],
        DiagnosticCode::MagicDate => &[],
        DiagnosticCode::YoLetterUsage => &[456],
        DiagnosticCode::LatinAndCyrillicSymbolInWord => &[],
        DiagnosticCode::InvalidCharacterInFile => &[456],
        DiagnosticCode::DoubleNegatives => &[],
        DiagnosticCode::NestedTernaryOperator => &[],
        DiagnosticCode::PostfixAccessOnExpression => &[],
        DiagnosticCode::NonExportMethodsInApiRegion => &[455],
        DiagnosticCode::TernaryOperatorUsage => &[],
        DiagnosticCode::UnaryPlusInConcatenation => &[],
        DiagnosticCode::UselessTernaryOperator => &[],
        DiagnosticCode::BadWords => &[],
        DiagnosticCode::DuplicateStringLiteral => &[],
        DiagnosticCode::DuplicateRegion => &[455],
        DiagnosticCode::NonStandardRegion => &[455],
        DiagnosticCode::DuplicatedInsertionIntoCollection => &[],
        DiagnosticCode::ExcessiveAutoTestCheck => &[456],
        DiagnosticCode::IdenticalExpressions => &[],
        DiagnosticCode::IfElseDuplicatedCodeBlock => &[],
        DiagnosticCode::IfElseDuplicatedCondition => &[],
        DiagnosticCode::IfElseIfEndsWithElse => &[],
        DiagnosticCode::MultilingualStringHasAllDeclaredLanguages => &[763],
        DiagnosticCode::MultilingualStringUsingWithTemplate => &[763],
        DiagnosticCode::NestedConstructorsInStructureDeclaration => &[],
        DiagnosticCode::NestedFunctionInParameters => &[640],
        DiagnosticCode::Typo => &[],
        DiagnosticCode::AllFunctionPathMustHaveReturn => &[],
        DiagnosticCode::FunctionShouldHaveReturn => &[],
        DiagnosticCode::ProcedureReturnsValue => &[],
        DiagnosticCode::FunctionReturnsSamePrimitive => &[],
        DiagnosticCode::FunctionNameStartsWithGet => &[647],
        DiagnosticCode::TooManyReturns => &[],
        DiagnosticCode::NumberOfParams => &[640],
        DiagnosticCode::NumberOfOptionalParams => &[640],
        DiagnosticCode::NumberOfValuesInStructureConstructor => &[693],
        DiagnosticCode::OrderOfParams => &[640],
        DiagnosticCode::MissedRequiredParameter => &[640],
        DiagnosticCode::FunctionOutParameter => &[],
        DiagnosticCode::UnusedParameters => &[],
        DiagnosticCode::MissingParameterDescription => &[453],
        DiagnosticCode::MissingReturnedValueDescription => &[453],
        // Проверяемое требование — std640 «Параметры процедур и функций»; остальные поясняют контекст.
        DiagnosticCode::ReservedParameterNames => &[640, 454],
        DiagnosticCode::ReservedWordAsMethodName => &[],
        DiagnosticCode::RewriteMethodParameter => &[],
        DiagnosticCode::UnusedLocalVariable => &[],
        DiagnosticCode::UnusedLocalMethod => &[456],
        DiagnosticCode::ExportVariables => &[639],
        DiagnosticCode::MissingVariablesDescription => &[455],
        DiagnosticCode::SelfAssign => &[],
        DiagnosticCode::ThisObjectAssign => &[],
        DiagnosticCode::CyclomaticComplexity => &[],
        DiagnosticCode::CognitiveComplexity => &[],
        DiagnosticCode::NestedStatements => &[],
        DiagnosticCode::MethodSize => &[],
        DiagnosticCode::IfConditionComplexity => &[],
        DiagnosticCode::MissingCodeTryCatchEx => &[499],
        // Проверяемое требование — std642 «Длительные операции на сервере»; остальные поясняют контекст.
        DiagnosticCode::MissingTempStorageDeletion => &[642, 487],
        DiagnosticCode::MissingTemporaryFileDeletion => &[542],
        DiagnosticCode::MisplacedLoopControl => &[],
        DiagnosticCode::UseLessForEach => &[],
        DiagnosticCode::UsingGoto => &[547],
        DiagnosticCode::BeginTransactionBeforeTryCatch => &[783],
        DiagnosticCode::CodeAfterAsyncCall => &[],
        DiagnosticCode::CommitTransactionOutsideTryCatch => &[783],
        DiagnosticCode::CompilationDirectiveLost => &[439],
        DiagnosticCode::CompilationDirectiveNeedLess => &[439],
        DiagnosticCode::CreateQueryInCycle => &[436],
        // Проверяемое требование — std773 «Использование признака ОбменДанными.Загрузка в обработчиках событий объекта»; остальные поясняют контекст.
        DiagnosticCode::DataExchangeLoading => &[773, 465, 464, 752],
        DiagnosticCode::DeletingCollectionItem => &[],
        DiagnosticCode::SelfInsertion => &[],
        DiagnosticCode::SeveralCompilerDirectives => &[],
        DiagnosticCode::StyleElementConstructors => &[667],
        DiagnosticCode::DeprecatedPlatformApi => &[],
        DiagnosticCode::DeprecatedMethodCall => &[453],
        // Проверяемое требование — std669 «Ограничение на выполнение внешнего кода»; остальные поясняют контекст.
        DiagnosticCode::DisableSafeMode => &[669, 678, 770, 485],
        DiagnosticCode::ExecuteExternalCode => &[770],
        DiagnosticCode::ExecuteExternalCodeInCommonModule => &[770],
        // Проверяемое требование — std774 «Безопасность запуска приложений»; остальные поясняют контекст.
        DiagnosticCode::ExternalAppStarting => &[774, 669],
        // Проверяемое требование — std542 «Доступ к файловой системе из кода конфигурации»; остальные поясняют контекст.
        DiagnosticCode::FileSystemAccess => &[542, 774],
        DiagnosticCode::OSUsersMethod => &[],
        DiagnosticCode::TempFilesDir => &[542],
        DiagnosticCode::FormDataToValue => &[409],
        DiagnosticCode::GetFormMethod => &[404],
        DiagnosticCode::GlobalContextMethodCollision8312 => &[],
        DiagnosticCode::GlobalContextMethodConflict => &[],
        DiagnosticCode::InternetAccess => &[],
        // Проверяемое требование — std737 «Проверка прав доступа»; остальные поясняют контекст.
        DiagnosticCode::IsInRoleMethod => &[737, 689],
        DiagnosticCode::PairingBrokenTransaction => &[783],
        DiagnosticCode::WrongUseOfRollbackTransactionMethod => &[783],
        DiagnosticCode::TimeoutsInExternalResources => &[748],
        DiagnosticCode::TryNumber => &[499],
        DiagnosticCode::UnknownPreprocessorSymbol => &[],
        DiagnosticCode::UnsafeSafeModeMethodCall => &[],
        // Проверяемое требование — std498 «Использование Журнала регистрации»; остальные поясняют контекст.
        DiagnosticCode::UsageWriteLogEvent => &[498, 499],
        DiagnosticCode::UseSystemInformation => &[],
        DiagnosticCode::UsingCancelParameter => &[686],
        DiagnosticCode::UsingExternalCodeTools => &[669],
        DiagnosticCode::UsingFindElementByString => &[],
        DiagnosticCode::UsingHardcodeNetworkAddress => &[],
        DiagnosticCode::UsingHardcodePath => &[],
        DiagnosticCode::UsingHardcodeSecretInformation => &[740],
        DiagnosticCode::UsingModalWindows => &[703],
        DiagnosticCode::UsingObjectNotAvailableUnix => &[],
        DiagnosticCode::UsingSynchronousCalls => &[703],
        DiagnosticCode::UsingServiceTag => &[],
        DiagnosticCode::UsingThisForm => &[],
        DiagnosticCode::WrongUseFunctionProceedWithCall => &[],
        DiagnosticCode::WrongHttpServiceHandler => &[],
        DiagnosticCode::WrongWebServiceHandler => &[],
        DiagnosticCode::WrongDataPathForFormElements => &[467],
        DiagnosticCode::PublicMethodsDescription => &[453],
        DiagnosticCode::CachedPublic => &[644],
        DiagnosticCode::CommandModuleExportMethods => &[544],
        DiagnosticCode::CommonModuleAssign => &[],
        DiagnosticCode::CommonModuleInvalidType => &[469],
        DiagnosticCode::CommonModuleMissingAPI => &[455],
        DiagnosticCode::CommonModuleNameCached => &[469],
        DiagnosticCode::CommonModuleNameClient => &[469],
        DiagnosticCode::CommonModuleNameClientServer => &[469],
        DiagnosticCode::CommonModuleNameFullAccess => &[469],
        DiagnosticCode::CommonModuleNameGlobal => &[469],
        DiagnosticCode::CommonModuleNameGlobalClient => &[469],
        DiagnosticCode::CommonModuleNameServerCall => &[469],
        DiagnosticCode::CommonModuleNameWords => &[469],
        DiagnosticCode::DenyIncompleteValues => &[],
        DiagnosticCode::ForbiddenMetadataName => &[474],
        DiagnosticCode::MetadataObjectNameLength => &[474],
        DiagnosticCode::MissingCommonModuleMethod => &[],
        DiagnosticCode::MissingEventSubscriptionHandler => &[],
        DiagnosticCode::OrdinaryAppSupport => &[467],
        DiagnosticCode::PrivilegedModuleMethodCall => &[],
        DiagnosticCode::ProtectedModule => &[],
        DiagnosticCode::RedundantAccessToObject => &[],
        DiagnosticCode::SameMetadataObjectAndChildNames => &[474],
        DiagnosticCode::ScheduledJobHandler => &[540],
        // Проверяемое требование — std487 «Минимизация количества серверных вызовов и трафика»; остальные поясняют контекст.
        DiagnosticCode::ServerCallsInFormEvents => &[487, 630],
        // Проверяемое требование — std630 «Правила создания модулей форм»; остальные поясняют контекст.
        DiagnosticCode::ServerSideExportFormMethod => &[630, 544],
        // Проверяемое требование — std532 «Установка прав для новых объектов и полей объектов»; остальные поясняют контекст.
        DiagnosticCode::SetPermissionsForNewObjects => &[532, 689],
        // Проверяемое требование — std485 «Использование привилегированного режима»; остальные поясняют контекст.
        DiagnosticCode::SetPrivilegedMode => &[485, 678, 669],
        DiagnosticCode::TransferringParametersBetweenClientAndServer => &[487],
        DiagnosticCode::UnsafeFindByCode => &[],
        DiagnosticCode::WeavingAnnotationNotApplicable => &[],
        DiagnosticCode::WeavingSignatureMismatch => &[],
        DiagnosticCode::UnresolvedName => &[],
        DiagnosticCode::UnresolvedMethodCall => &[],
        DiagnosticCode::UnresolvedBareCall => &[],
        DiagnosticCode::MismatchedArgCount => &[],
        DiagnosticCode::TypeMismatch => &[],
        DiagnosticCode::TypeMismatchByDocComment => &[],
        DiagnosticCode::UnresolvedField => &[],
        DiagnosticCode::UnresolvedMetadataPath => &[],
        DiagnosticCode::ReadOnlyPropertyAssignment => &[],
        DiagnosticCode::GlobalPropertyNotWritable => &[],
        DiagnosticCode::UnavailableInEnvironment => &[],
        DiagnosticCode::PlatformMemberNewerThanMinVersion => &[],
        DiagnosticCode::PlatformMemberHiddenByCompatibilityMode => &[],
        DiagnosticCode::ModuleAccessibility => &[469],
        DiagnosticCode::AmbiguousFieldInQuery => &[],
        DiagnosticCode::AssignAliasFieldsInQuery => &[437],
        DiagnosticCode::DuplicateAliasInQuery => &[],
        DiagnosticCode::FieldsFromJoinsWithoutIsNull => &[412],
        DiagnosticCode::FullOuterJoinQuery => &[435],
        DiagnosticCode::IncorrectUseLikeInQuery => &[726],
        DiagnosticCode::JoinWithSubQuery => &[655],
        DiagnosticCode::JoinWithVirtualTable => &[655],
        DiagnosticCode::LogicalOrInJoinQuerySection => &[658],
        DiagnosticCode::LogicalOrInTheWhereSectionOfQuery => &[658],
        DiagnosticCode::MultilineStringInQuery => &[],
        DiagnosticCode::QueryNestedFieldsByDot => &[],
        DiagnosticCode::QueryParseError => &[437],
        DiagnosticCode::QueryToMissingMetadata => &[],
        DiagnosticCode::UnknownFieldInQuery => &[],
        DiagnosticCode::InvalidVirtualTableParameters => &[],
        DiagnosticCode::UnlimitedLengthStringUsageInQuery => &[432],
        DiagnosticCode::RefOveruse => &[654],
        DiagnosticCode::SelectTopWithoutOrderBy => &[412],
        DiagnosticCode::UnionAll => &[434],
        DiagnosticCode::UsingLikeInQuery => &[726],
        // Проверяемое требование — std657 «Обращения к виртуальным таблицам»; остальные поясняют контекст.
        DiagnosticCode::VirtualTableCallWithoutParameters => &[657, 733],
        DiagnosticCode::UnknownSuppressionCode => &[456],
        DiagnosticCode::SuppressionWithoutCode => &[456],
    }
}

/// Сообщение диагностики с дописанным нормативным источником.
///
/// Суффикс живёт в проекциях, а не в [`crate::Diagnostic::message`]: внутренний
/// слой не должен нести форму подачи, а снапшоты диагностик печатают сообщение
/// как есть и уехали бы все разом.
///
/// Ссылка даётся одна — на первый номер среза, то есть на проверяемое
/// требование. Остальные номера перечисляются без ссылок: они поясняют контекст,
/// и четыре URL в одной строке панели проблем нечитаемы.
///
/// Текст суффикса русский. Локаль до проекций не доходит, а сообщения самих
/// диагностик де-факто русские, так что английский суффикс дал бы смесь языков
/// в одной строке.
pub fn message_with_standards(code: DiagnosticCode, message: &str) -> String {
    let ids = standards(code);
    let Some((primary, rest)) = ids.split_first() else {
        return message.to_string();
    };
    let mut listed = primary.to_string();
    for id in rest {
        listed.push_str(", ");
        listed.push_str(&id.to_string());
    }
    let word = if rest.is_empty() { "Стандарт" } else { "Стандарты" };
    format!("{message} ({word} {listed}: {})", standard_url(*primary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use strum::IntoEnumIterator;

    /// Префиксы ссылок, за которыми следует номер стандарта.
    ///
    /// Форм записи в доках больше, чем префиксов: `#content:456` встречается и
    /// сама по себе, и с хвостами `:hdoc`, `:hdoc:2.3`. Сканер читает весь
    /// прогон цифр и хвост игнорирует, поэтому один префикс покрывает их все.
    /// Условие «за цифрами не идёт `:hdoc`» здесь было бы ошибкой: в `456:hdoc`
    /// ему удовлетворяет усечённое `45`.
    const STANDARD_PREFIXES: &[&str] =
        &["v8std.ru/std/", "its.1c.ru/db/v8std#content:", "its.1c.ru/db/v8std/content/"];

    /// Номера стандартов, размеченные в тексте дока.
    ///
    /// На том же домене живут карточки диагностик (`v8std.ru/diagnostics/bslls/…`)
    /// и проверки ACC (`v8std.ru/diagnostics/acc/1248/`); последние несут цифры,
    /// поэтому отбор идёт по полному префиксу пути, а не по домену.
    fn standards_in_doc(text: &str) -> BTreeSet<u16> {
        let visible = without_html_comments(text);
        let mut found = BTreeSet::new();
        for prefix in STANDARD_PREFIXES {
            let mut rest = visible.as_str();
            while let Some(at) = rest.find(prefix) {
                rest = &rest[at + prefix.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(id) = digits.parse::<u16>() {
                    found.insert(id);
                }
            }
        }
        found
    }

    /// Текст дока без HTML-комментариев.
    ///
    /// Английские доки несут закомментированный блок-образец «Примеры
    /// источников» со ссылкой на std456; без маскировки этот номер читается как
    /// разметка диагностики и расходится с русским доком, где образца нет.
    fn without_html_comments(text: &str) -> String {
        let mut visible = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find("<!--") {
            visible.push_str(&rest[..open]);
            match rest[open..].find("-->") {
                Some(close) => rest = &rest[open + close + "-->".len()..],
                None => return visible,
            }
        }
        visible.push_str(rest);
        visible
    }

    /// Диагностики, чьи доки ссылаются на стандарт как на СМЕЖНЫЙ контекст, а не
    /// как на проверяемое требование.
    ///
    /// Такая ссылка полезна человеку и вредна машине: `standards` объявляет
    /// стандарты, которые правило проверяет, и выдача смежного номера называет
    /// пользователю ложную причину срабатывания. Все шесть — правила, которые
    /// шире любого одного стандарта: ошибка разбора возникает не только на
    /// директивах, устаревшим объявляет API документация платформы, а обращение
    /// в интернет диагностика лишь отмечает для ручного разбора.
    ///
    /// Оговорка живёт в тексте дока, поэтому перечень поддержан сторожем
    /// [`excluded_docs_still_disclaim`]: исключение не переживёт своё основание.
    const CONTEXT_ONLY_DOCS: &[DiagnosticCode] = &[
        DiagnosticCode::DeprecatedPlatformApi,
        DiagnosticCode::IfElseDuplicatedCodeBlock,
        DiagnosticCode::IncorrectUseOfStrTemplate,
        DiagnosticCode::InternetAccess,
        DiagnosticCode::ParseError,
        DiagnosticCode::QueryNestedFieldsByDot,
    ];

    /// Обороты, которыми доки отделяют смежный источник от проверяемого.
    const DISCLAIMERS: &[&str] = &[
        "нет прямого нормативного стандарта",
        "Связанный публичный контекст",
        "Связанная публичная рекомендация",
        "no direct 1C standard",
        "Related public context",
        "Related public guidance",
    ];

    fn doc_text(lang: &str, code: DiagnosticCode) -> String {
        let path = format!("{}/docs/{}/{}.md", env!("CARGO_MANIFEST_DIR"), lang, code.as_str());
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("нет дока {path}: {e}"))
    }

    #[test]
    fn extractor_ignores_acc_checks_on_the_same_domain() {
        let text = doc_text("ru", DiagnosticCode::CanonicalSpellingKeywords);
        assert!(text.contains("v8std.ru/diagnostics/acc/1248/"), "вход потерял проверку ACC");
        assert_eq!(standards_in_doc(&text), BTreeSet::from([441]));
    }

    #[test]
    fn extractor_reads_anchor_form_without_hdoc_suffix() {
        let text = doc_text("ru", DiagnosticCode::DataExchangeLoading);
        assert!(text.contains("v8std#content:464\n") || text.contains("v8std#content:464)"));
        assert_eq!(standards_in_doc(&text), BTreeSet::from([464, 465, 752, 773]));
    }

    #[test]
    fn extractor_skips_commented_out_source_template() {
        let text = doc_text("en", DiagnosticCode::ForbiddenMetadataName);
        assert!(
            text.contains("<!-- Примеры источников"),
            "вход потерял закомментированный образец, контроль стал холостым"
        );
        assert!(text.contains("v8std#content:456"), "образец потерял номер std456");
        assert_eq!(standards_in_doc(&text), BTreeSet::from([474]));
    }

    #[test]
    fn formatter_leaves_a_diagnostic_without_standard_untouched() {
        assert!(standards(DiagnosticCode::CognitiveComplexity).is_empty(), "вход потерял смысл");
        assert_eq!(message_with_standards(DiagnosticCode::CognitiveComplexity, "Сложно"), "Сложно");
    }

    #[test]
    fn internal_layers_never_carry_the_suffix() {
        // Отпечаток базовой линии считается без `message`, поэтому проверка
        // «старый baseline даёт 0 new» зелена при любой реализации и гейтом не
        // является. Прямая проверка текста — является.
        let diag = crate::Diagnostic {
            code: DiagnosticCode::LineLength,
            message: "Строка слишком длинная".to_string(),
            severity: crate::Severity::Warning,
            range: ide_db::TextRange::new(0.into(), 1.into()),
            tags: Vec::new(),
            fixes: Vec::new(),
        };
        assert!(!diag.message.contains("v8std.ru"), "суффикс просочился в Diagnostic");
        let out = diag.to_output("А = 1;");
        assert!(!out.message.contains("v8std.ru"), "суффикс просочился в DiagnosticOutput");
        assert!(
            message_with_standards(diag.code, &diag.message).contains("v8std.ru"),
            "форматтер молчит — проверка выше стала холостой"
        );
    }

    #[test]
    fn excluded_docs_still_disclaim() {
        // Перечень исключений держится на обороте в тексте дока. Уйдёт оборот —
        // уйдёт и основание, а исключение осталось бы молча.
        for &code in CONTEXT_ONLY_DOCS {
            for lang in ["ru", "en"] {
                let text = doc_text(lang, code);
                assert!(
                    DISCLAIMERS.iter().any(|d| text.contains(d)),
                    "{code:?} [{lang}]: док больше не называет ссылку смежной — \
                     либо вернуть оговорку, либо убрать код из перечня"
                );
                assert!(
                    !standards_in_doc(&text).is_empty(),
                    "{code:?} [{lang}]: в доке не осталось номера — исключение лишнее"
                );
            }
        }
    }

    #[test]
    fn table_matches_docs_in_both_languages() {
        let mut mismatched = Vec::new();
        for code in DiagnosticCode::iter() {
            if CONTEXT_ONLY_DOCS.contains(&code) {
                assert!(
                    standards(code).is_empty(),
                    "{code:?}: док называет номер смежным контекстом, таблица не вправе \
                     объявлять его проверяемым требованием"
                );
                continue;
            }
            let table: BTreeSet<u16> = standards(code).iter().copied().collect();
            for lang in ["ru", "en"] {
                let doc = standards_in_doc(&doc_text(lang, code));
                if doc != table {
                    mismatched.push(format!(
                        "{} [{}]: док {:?}, таблица {:?}",
                        code.as_str(),
                        lang,
                        doc,
                        table
                    ));
                }
            }
        }
        assert!(
            mismatched.is_empty(),
            "номера стандартов разошлись у {} пар:\n{}",
            mismatched.len(),
            mismatched.join("\n")
        );
    }
}
