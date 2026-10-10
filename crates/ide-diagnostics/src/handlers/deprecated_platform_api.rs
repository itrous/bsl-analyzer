use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
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
    use crate::test_utils::{check_diagnostics_snapshot_for, check_hir_diagnostic};
    use crate::{DiagnosticCode, DiagnosticTag};
    use expect_test::expect;

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
}
