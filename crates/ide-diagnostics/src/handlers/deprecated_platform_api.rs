use crate::config::REMOVED_CODES;
use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use bsl_platform::deprecation::{self, DeprecationEntry, DisplayKind, ElementKind, Lookup};
use hir::LocalRange;
use hir::Name;
use stdx::case::CaseExt;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Deprecated],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    type_name: &Name,
    member_name: &Name,
    is_property: bool,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::DeprecatedPlatformApi;
    if ctx.is_disabled_with_metadata(code) {
        return None;
    }
    if is_excluded(ctx.config, member_name.as_str()) {
        return None;
    }

    let entry = if is_property {
        deprecation::registry()
            .lookup(Lookup::property(type_name.as_str(), member_name.as_str()))?
    } else {
        deprecation::registry().lookup(Lookup::method(type_name.as_str(), member_name.as_str()))?
    };
    if !matches_expected_kind(entry, is_property) {
        return None;
    }

    let alias = member_alias(entry, member_name.as_str())?;
    let replacement = replacement_for_alias(entry, alias)?;
    let message = type_member_message(
        type_name_for_alias(type_name.as_str(), alias),
        member_name.as_str(),
        replacement,
        alias,
        is_property,
    );

    Some(Diagnostic {
        code,
        message,
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

/// The `[diagnostics.parameters.DeprecatedPlatformApi]` key listing the deprecated
/// platform features whose findings are not reported.
pub(crate) const EXCLUDE_PARAM: &str = "exclude";

/// Whether the finding about `flagged_name` (the name the finding calls
/// deprecated) is silenced by `exclude`. A listed name silences every spelling of
/// the registry entry it names, so `ТекущаяДата` also covers `CurrentDate()`, and a
/// member name covers that member on every owner in the registry.
pub(crate) fn is_excluded(config: &DiagnosticsConfig, flagged_name: &str) -> bool {
    let Some(excluded) =
        config.get_string_array(DiagnosticCode::DeprecatedPlatformApi, EXCLUDE_PARAM)
    else {
        return false;
    };
    if excluded.is_empty() {
        return false;
    }
    let flagged = flagged_name.fold_lower();
    let excluded: Vec<String> = excluded.iter().map(|name| name.fold_lower()).collect();
    deprecation::registry().entries().iter().any(|entry| {
        let names = entry_names(entry);
        names.contains(&flagged) && excluded.iter().any(|name| names.contains(name))
    })
}

/// Both spellings of an entry, case-folded; an entry without an English name has
/// only the Russian one.
fn entry_names(entry: &DeprecationEntry) -> Vec<String> {
    let mut names = vec![entry.ru.fold_lower()];
    if !entry.en.is_empty() {
        names.push(entry.en.fold_lower());
    }
    names
}

/// What is wrong with `[diagnostics.parameters.DeprecatedPlatformApi].exclude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExcludeProblem {
    /// An element that names no deprecated platform feature: an unknown string, or
    /// a non-string element written as JSON.
    UnknownName(String),
    /// The value is not an array; it is ignored as a whole.
    NotAnArray,
}

/// The problems of `exclude` in a raw `[diagnostics]` section, one per element
/// that names nothing in the deprecation registry. Empty when the key is absent.
pub(crate) fn unknown_excluded_names(diagnostics: &serde_json::Value) -> Vec<ExcludeProblem> {
    let Some(value) = diagnostics
        .get("parameters")
        .and_then(|parameters| parameters.get(DiagnosticCode::DeprecatedPlatformApi.as_str()))
        .and_then(|parameters| parameters.get(EXCLUDE_PARAM))
    else {
        return Vec::new();
    };
    let Some(elements) = value.as_array() else {
        return vec![ExcludeProblem::NotAnArray];
    };
    elements
        .iter()
        .filter_map(|element| match element.as_str() {
            Some(name) if is_registry_name(name) => None,
            Some(name) => Some(ExcludeProblem::UnknownName(name.to_owned())),
            None => Some(ExcludeProblem::UnknownName(element.to_string())),
        })
        .collect()
}

fn is_registry_name(name: &str) -> bool {
    let name = name.fold_lower();
    deprecation::registry().entries().iter().any(|entry| entry_names(entry).contains(&name))
}

/// A ready `exclude` entry for an unknown `Deprecated…` key, so that the author of
/// a stale key can silence the same findings without switching the whole rule off.
/// The names come from the registry: entries whose name is the rest of the key, or
/// whose replacement lives in the manager of that name. A removed code with no
/// such entries gets a general pointer to `exclude`; any other key gets nothing.
pub(crate) fn exclude_suggestion(key: &str) -> Option<String> {
    let mut names: Vec<&'static str> = Vec::new();
    if let Some(rest) = key.strip_prefix("Deprecated").filter(|rest| !rest.is_empty()) {
        let rest = rest.fold_lower();
        for entry in deprecation::registry().entries() {
            if suggests_entry(entry, &rest) && !names.contains(&entry.ru) {
                names.push(entry.ru);
            }
        }
    }
    let code = DiagnosticCode::DeprecatedPlatformApi.as_str();
    if !names.is_empty() {
        let list = names.iter().map(|name| format!("\"{name}\"")).collect::<Vec<_>>().join(", ");
        return Some(format!(
            "to silence only these findings keep `{code}` on and write \
             `[diagnostics.parameters.{code}] exclude = [{list}]`"
        ));
    }
    REMOVED_CODES.iter().any(|(removed, successor, _)| *removed == key && *successor == code).then(
        || {
            format!(
                "to silence individual deprecated features list their names in \
                 `[diagnostics.parameters.{code}] exclude`"
            )
        },
    )
}

fn suggests_entry(entry: &DeprecationEntry, rest: &str) -> bool {
    if !entry.en.is_empty() && entry.en.fold_lower() == rest {
        return true;
    }
    if entry.ru.fold_lower() == rest {
        return true;
    }
    entry
        .replacement
        .and_then(|replacement| replacement.en.split_once('.'))
        .is_some_and(|(manager, _)| manager.fold_lower() == rest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberAlias {
    Russian,
    English,
}

fn matches_expected_kind(entry: &DeprecationEntry, is_property: bool) -> bool {
    if is_property {
        return entry.element_kind == ElementKind::Property
            && entry.display == DisplayKind::Property;
    }
    entry.element_kind == ElementKind::Method && entry.display == DisplayKind::Method
}

fn member_alias(entry: &DeprecationEntry, member_name: &str) -> Option<MemberAlias> {
    let lower = member_name.fold_lower();
    if lower == entry.ru.fold_lower() {
        return Some(MemberAlias::Russian);
    }
    if !entry.en.is_empty() && lower == entry.en.fold_lower() {
        return Some(MemberAlias::English);
    }
    None
}

/// Имя типа хранится в каноническом русском написании, а язык сообщения выбирает
/// алиас, которым записан сам член. Владелец обязан следовать за членом, иначе
/// английское сообщение получит русское имя типа: `Method "HTTPСоединение.Get"`.
fn type_name_for_alias(type_name: &str, alias: MemberAlias) -> &str {
    match alias {
        MemberAlias::Russian => type_name,
        MemberAlias::English => match bsl_platform::PlatformData::instance().get_type(type_name) {
            Some(ty) if !ty.english_name.is_empty() => ty.english_name.as_str(),
            _ => type_name,
        },
    }
}

fn replacement_for_alias(entry: &DeprecationEntry, alias: MemberAlias) -> Option<&'static str> {
    let replacement = entry.replacement?;
    match alias {
        MemberAlias::Russian => Some(replacement.ru),
        MemberAlias::English => Some(replacement.en),
    }
}

fn type_member_message(
    type_name: &str,
    member_name: &str,
    replacement: &str,
    alias: MemberAlias,
    is_property: bool,
) -> String {
    let qualified = format!("{type_name}.{member_name}");
    match (alias, is_property) {
        (MemberAlias::Russian, false) => {
            format!("Метод \"{qualified}\" устарел. Следует использовать \"{replacement}\".")
        }
        (MemberAlias::English, false) => {
            format!("Method \"{qualified}\" is deprecated. You should use \"{replacement}\".")
        }
        (MemberAlias::Russian, true) => {
            format!("Свойство \"{qualified}\" устарело. Следует использовать \"{replacement}\".")
        }
        (MemberAlias::English, true) => {
            format!("Property \"{qualified}\" is deprecated. You should use \"{replacement}\".")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{
        check_diagnostics_for_with_config, check_diagnostics_snapshot_for, check_hir_diagnostic,
        format_diags, range_to_line_col,
    };
    use crate::{Diagnostic, DiagnosticCode, DiagnosticTag, DiagnosticsConfig};
    use base_db::Locale;
    use expect_test::expect;
    use serde_json::json;

    /// Every call but `СтрНайти` is one finding when nothing is excluded, ten in all;
    /// the calls go through seven different handlers.
    const EXCLUDE_FIXTURE: &str = r#"Процедура Тест(Цвет)
Дата1 = ТекущаяДата();
Дата2 = CurrentDate();
Поз1 = Найти("abc", "b");
Поз2 = СтрНайти("abc", "b");
Сообщить("x");
Кратко = КраткоеПредставлениеОшибки(ИнформацияОбОшибке());
Подробно = ПодробноеПредставлениеОшибки(ИнформацияОбОшибке());
Заголовок = ПолучитьКраткийЗаголовокПриложения();
ОбластьПостроенияДиаграммы.ОтображатьШкалу = Истина;
ОбластьПостроенияДиаграммы.ЦветШкалы = Цвет;
ОчиститьЖурналРегистрации();
КонецПроцедуры"#;

    /// One line number per finding of [`EXCLUDE_FIXTURE`] when nothing is excluded.
    const FLAGGED_LINES: [u32; 10] = [2, 3, 4, 6, 7, 8, 9, 10, 11, 12];

    fn exclude_config(names: &[&str]) -> DiagnosticsConfig {
        DiagnosticsConfig::from_project_json(
            &json!({ "parameters": { "DeprecatedPlatformApi": { "exclude": names } } }),
            Locale::Ru,
        )
    }

    fn findings_of(source: &str, config: DiagnosticsConfig) -> Vec<Diagnostic> {
        check_diagnostics_for_with_config(source, config, DiagnosticCode::DeprecatedPlatformApi)
    }

    fn lines_of(source: &str, findings: &[Diagnostic]) -> Vec<u32> {
        findings.iter().map(|d| range_to_line_col(source, d.range).0 + 1).collect()
    }

    fn fixture_lines(config: DiagnosticsConfig) -> Vec<u32> {
        lines_of(EXCLUDE_FIXTURE, &findings_of(EXCLUDE_FIXTURE, config))
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn effective_tags_include_lsp_deprecated() {
        let diagnostics = check_hir_diagnostic(
            r#"Процедура Тест()
    Соединение = Новый HTTPСоединение("example.com", 80);
    Ответ = Соединение.Получить(Новый HTTPЗапрос("/"));
КонецПроцедуры"#,
        );
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == DiagnosticCode::DeprecatedPlatformApi)
            .expect("DeprecatedPlatformApi diagnostic should be emitted");

        assert!(diagnostic.tags.contains(&DiagnosticTag::Deprecated));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn flags_deprecated_http_connection_get_ru_and_en() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Соединение = Новый HTTPСоединение("example.com", 80);
    Ответ = Соединение.Получить(Новый HTTPЗапрос("/"));
КонецПроцедуры

Procedure TestEn()
    Connection = New HTTPConnection("example.com", 80);
    Response = Connection.Get(New HTTPRequest("/"));
EndProcedure"#,
            DiagnosticCode::DeprecatedPlatformApi,
            expect![[r#"
                DeprecatedPlatformApi @ 3:13..3:32
                  message: Метод "HTTPСоединение.Получить" устарел. Следует использовать "ПолучитьАсинх".
                  severity: Warning
                DeprecatedPlatformApi @ 8:16..8:30
                  message: Method "HTTPConnection.Get" is deprecated. You should use "GetAsync".
                  severity: Warning"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn flags_deprecated_internet_proxy_password_read_and_assignment() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Прокси = Новый ИнтернетПрокси();
    Пароль = Прокси.Пароль;
    Прокси.Пароль = "secret";
КонецПроцедуры"#,
            DiagnosticCode::DeprecatedPlatformApi,
            expect![[r#"
                DeprecatedPlatformApi @ 3:14..3:27
                  message: Свойство "ИнтернетПрокси.Пароль" устарело. Следует использовать "Пароль".
                  severity: Warning
                DeprecatedPlatformApi @ 4:5..4:18
                  message: Свойство "ИнтернетПрокси.Пароль" устарело. Следует использовать "Пароль".
                  severity: Warning"#]],
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn flags_deprecated_internet_proxy_user_ru_and_en() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Прокси = Новый ИнтернетПрокси();
    Пользователь = Прокси.Пользователь;
    Прокси.Пользователь = "root";
КонецПроцедуры

Procedure TestEn()
    Proxy = New InternetProxy();
    UserName = Proxy.User;
    Proxy.User = "root";
EndProcedure"#,
            DiagnosticCode::DeprecatedPlatformApi,
            expect![[r#"
                DeprecatedPlatformApi @ 3:20..3:39
                  message: Свойство "ИнтернетПрокси.Пользователь" устарело. Следует использовать "Пользователь".
                  severity: Warning
                DeprecatedPlatformApi @ 4:5..4:24
                  message: Свойство "ИнтернетПрокси.Пользователь" устарело. Следует использовать "Пользователь".
                  severity: Warning
                DeprecatedPlatformApi @ 9:16..9:26
                  message: Property "InternetProxy.User" is deprecated. You should use "User".
                  severity: Warning
                DeprecatedPlatformApi @ 10:5..10:15
                  message: Property "InternetProxy.User" is deprecated. You should use "User".
                  severity: Warning"#]],
        );
    }

    #[test]
    fn skips_same_named_members_on_other_receivers_and_unknowns() {
        check_diagnostics_snapshot_for(
            r#"Процедура Тест()
    Структура = Новый Структура("Пароль", "secret");
    Значение = Структура.Получить("Пароль");
    НеизвестныйПолучатель.Получить("Пароль");
КонецПроцедуры"#,
            DiagnosticCode::DeprecatedPlatformApi,
            expect![[r#""#]],
        );
    }

    #[test]
    fn skips_union_receiver_even_when_one_arm_has_deprecated_member() {
        check_diagnostics_snapshot_for(
            r#"// Возвращаемое значение:
//   HTTPСоединение, Структура - соединение или параметры
Функция ПолучитьПриемник()
    Возврат Новый HTTPСоединение("example.com", 80);
КонецФункции

Процедура Тест()
    Приемник = ПолучитьПриемник();
    Ответ = Приемник.Получить(Новый HTTPЗапрос("/"));
КонецПроцедуры"#,
            DiagnosticCode::DeprecatedPlatformApi,
            expect![[r#""#]],
        );
    }

    #[test]
    fn exclude_silences_listed_features() {
        // The control: without the parameter the calls are all flagged, so a silent
        // line below is the parameter's doing and not a call nobody flags.
        assert_eq!(fixture_lines(DiagnosticsConfig::default()), FLAGGED_LINES);

        // Both spellings, any case, and the five paths a finding can come from.
        let listed = [
            "ТекущаяДата",
            "Find",
            "сообщить",
            "BriefErrorDescription",
            "ОтображатьШкалу",
            "ОчиститьЖурналРегистрации",
        ];
        let remaining = fixture_lines(exclude_config(&listed));
        for silenced in [2, 3, 4, 6, 7, 10, 12] {
            assert!(
                !remaining.contains(&silenced),
                "line {silenced} is still flagged: {remaining:?}"
            );
        }

        // Each name on its own silences its feature and nothing else, so a missing
        // filter on one path cannot hide behind the others.
        let own_lines: [(&str, &[u32]); 12] = [
            ("ТекущаяДата", &[2, 3]),
            ("CURRENTDATE", &[2, 3]),
            ("Find", &[4]),
            ("найти", &[4]),
            ("сообщить", &[6]),
            ("Message", &[6]),
            ("BriefErrorDescription", &[7]),
            ("краткоепредставлениеошибки", &[7]),
            ("ОтображатьШкалу", &[10]),
            ("ShowScale", &[10]),
            ("ОчиститьЖурналРегистрации", &[12]),
            ("ClearEventLog", &[12]),
        ];
        for (name, own) in own_lines {
            let expected: Vec<u32> =
                FLAGGED_LINES.iter().copied().filter(|line| !own.contains(line)).collect();
            assert_eq!(fixture_lines(exclude_config(&[name])), expected, "exclude = [{name}]");
        }
    }

    #[test]
    fn exclude_without_a_matching_feature_silences_nothing() {
        // A feature of the registry that the fixture does not call and a name that is
        // no feature: neither may silence anything.
        for name in ["ПолучитьФорму", "Nope"] {
            assert_eq!(fixture_lines(exclude_config(&[name])), FLAGGED_LINES, "exclude = [{name}]");
        }
        assert_eq!(fixture_lines(exclude_config(&[])), FLAGGED_LINES, "an empty list");
    }

    #[test]
    fn exclude_keeps_every_other_finding() {
        let baseline = findings_of(EXCLUDE_FIXTURE, DiagnosticsConfig::default());
        let listed = [
            "ТекущаяДата",
            "Find",
            "сообщить",
            "BriefErrorDescription",
            "ОтображатьШкалу",
            "ОчиститьЖурналРегистрации",
        ];
        let kept = findings_of(EXCLUDE_FIXTURE, exclude_config(&listed));

        // Positions, texts, severities, tags and fixes are those of the run without
        // the parameter: the filter removes findings, it does not rewrite them.
        let expected: Vec<Diagnostic> = baseline
            .iter()
            .filter(|d| {
                ![2, 3, 4, 6, 7, 10, 12]
                    .contains(&(range_to_line_col(EXCLUDE_FIXTURE, d.range).0 + 1))
            })
            .cloned()
            .collect();
        assert_eq!(kept, expected);

        // The neighbours of the excluded names in the same handlers stay.
        expect![[r#"
            DeprecatedPlatformApi @ 8:12..8:62
              message: Метод "ПодробноеПредставлениеОшибки" устарел. Следует использовать "ОбработкаОшибок.ПодробноеПредставлениеОшибки".
              severity: Warning
            DeprecatedPlatformApi @ 9:13..9:49
              message: Метод "ПолучитьКраткийЗаголовокПриложения" устарел. Следует использовать "КлиентскоеПриложение.ПолучитьКраткийЗаголовок".
              severity: Warning
            DeprecatedPlatformApi @ 11:28..11:37
              message: Атрибут "ЦветШкалы" устарел. Используйте: ЦветШкал
              severity: Warning"#]]
        .assert_eq(&format_diags(EXCLUDE_FIXTURE, &kept));
    }

    #[test]
    fn changing_exclude_on_a_live_database_changes_the_findings() {
        // An editor changes the config under a database that has already answered:
        // the memoised findings of the old list must not be served for the new one.
        let (db, file_id) = crate::test_utils::create_test_db(EXCLUDE_FIXTURE);
        let flagged = |config: DiagnosticsConfig| {
            crate::file_diagnostics(&db, file_id, &config)
                .into_iter()
                .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
                .count()
        };

        assert_eq!(flagged(DiagnosticsConfig::default()), 10);
        assert_eq!(flagged(exclude_config(&["ТекущаяДата"])), 8);
        assert_eq!(flagged(exclude_config(&["ТекущаяДата", "Найти"])), 7);
        assert_eq!(flagged(DiagnosticsConfig::default()), 10);
    }

    #[test]
    fn disabled_rule_ignores_exclude() {
        let listed = exclude_config(&["ТекущаяДата"]);

        // The controls: the same list leaves eight findings, the empty config ten.
        assert_eq!(fixture_lines(DiagnosticsConfig::default()).len(), 10);
        assert_eq!(fixture_lines(listed.clone()).len(), 8);

        let mut disabled = listed.clone();
        disabled.apply_cli_filters(&[], &["DeprecatedPlatformApi".to_owned()]);
        assert_eq!(fixture_lines(disabled), Vec::<u32>::new(), "the rule is disabled");

        let mut only_other = listed;
        only_other.apply_cli_filters(&["LineLength".to_owned()], &[]);
        assert_eq!(fixture_lines(only_other), Vec::<u32>::new(), "the rule is not enabled");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn exclude_covers_type_and_member_paths() {
        const SOURCE: &str = r#"Процедура Тест()
    Форма = Тип("УправляемаяФорма");
    Соединение = Новый HTTPСоединение("example.com", 80);
    Ответ = Соединение.Получить(Новый HTTPЗапрос("/"));
    Прокси = Новый ИнтернетПрокси();
    Пароль = Прокси.Пароль;
КонецПроцедуры"#;
        let flagged =
            |names: &[&str]| lines_of(SOURCE, &findings_of(SOURCE, exclude_config(names)));

        // The control: one finding on each of the three lines.
        assert_eq!(flagged(&[]), [2, 4, 6]);

        // Both names: only the proxy password is left, and it is the property finding.
        let kept = findings_of(SOURCE, exclude_config(&["ManagedForm", "Получить"]));
        assert_eq!(lines_of(SOURCE, &kept), [6]);
        assert!(kept[0].message.contains("ИнтернетПрокси.Пароль"), "{}", kept[0].message);

        // Each path on its own, so neither can be silenced by the other's name.
        assert_eq!(flagged(&["УправляемаяФорма"]), [4, 6], "the type path");
        assert_eq!(flagged(&["Get"]), [2, 6], "the member path, one spelling for both");
        assert_eq!(flagged(&["HTTPСоединение.Получить"]), [2, 4, 6], "a qualified name");
    }
}
