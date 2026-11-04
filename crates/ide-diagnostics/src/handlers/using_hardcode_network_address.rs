use crate::define_metadata;
use crate::metadata::*;
use crate::utils::regex_cache::cached_regex;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{Body, BodySourceMap, Expr, Literal};
use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::Arc;
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Vulnerability,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

const DEFAULT_SEARCH_WORDS_EXCLUSION: &str =
    "Верси|Version|ЗапуститьПриложение|RunApp|Пространств|Namespace|Драйвер|Driver";

const DEFAULT_POPULAR_VERSION_EXCLUSION: &str = r"^(1|2|3|8\.3|11)\.";

const DOTS_IN_IPV4: usize = 3;

static PATTERN_NETWORK_ADDRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(([0-9a-fA-F]{1,4}:){7,7}[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,7}:|([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,5}(:[0-9a-fA-F]{1,4}){1,2}|([0-9a-fA-F]{1,4}:){1,4}(:[0-9a-fA-F]{1,4}){1,3}|([0-9a-fA-F]{1,4}:){1,3}(:[0-9a-fA-F]{1,4}){1,4}|([0-9a-fA-F]{1,4}:){1,2}(:[0-9a-fA-F]{1,4}){1,5}|[0-9a-fA-F]{1,4}:((:[0-9a-fA-F]{1,4}){1,6})|:((:[0-9a-fA-F]{1,4}){1,7}|\s:)|fe80:(:[0-9a-fA-F]{0,4}){0,4}%[0-9a-zA-Z]{1,}|::(ffff(:0{1,4}){0,1}:){0,1}((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\.){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])|([0-9a-fA-F]{1,4}:){1,4}:((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\.){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9]))|((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\.){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])"
    ).unwrap()
});

#[derive(Debug, Clone)]
struct Config {
    search_words_exclusion: Arc<Regex>,
    search_popular_version_exclusion: Arc<Regex>,
}

impl Config {
    fn from_context(ctx: &BodyContext) -> Self {
        let words_exclusion_str = ctx
            .config
            .get_string(DiagnosticCode::UsingHardcodeNetworkAddress, "searchWordsExclusion")
            .unwrap_or(DEFAULT_SEARCH_WORDS_EXCLUSION);

        let version_exclusion_str = ctx
            .config
            .get_string(
                DiagnosticCode::UsingHardcodeNetworkAddress,
                "searchPopularVersionExclusion",
            )
            .unwrap_or(DEFAULT_POPULAR_VERSION_EXCLUSION);

        let search_words_exclusion = cached_regex(&format!("(?i){}", words_exclusion_str))
            .unwrap_or_else(|| {
                cached_regex(&format!("(?i){}", DEFAULT_SEARCH_WORDS_EXCLUSION)).unwrap()
            });

        let search_popular_version_exclusion =
            cached_regex(&format!("(?i){}", version_exclusion_str)).unwrap_or_else(|| {
                cached_regex(&format!("(?i){}", DEFAULT_POPULAR_VERSION_EXCLUSION)).unwrap()
            });

        Self { search_words_exclusion, search_popular_version_exclusion }
    }
}

fn is_localhost(ip: &str) -> bool {
    ip.starts_with("127.") || ip == "::1"
}

fn is_url(content: &str) -> bool {
    let bytes = content.as_bytes();
    let after_scheme = if bytes.len() > 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        Some(8)
    } else if bytes.len() > 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        Some(7)
    } else if bytes.len() > 6 && bytes[..6].eq_ignore_ascii_case(b"ftp://") {
        Some(6)
    } else {
        None
    };
    after_scheme.is_some_and(|pos| bytes.get(pos).is_some_and(|&b| b != b' ' && b != b'"'))
}

fn contains_letter(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic() || matches!(c, 'А'..='Я' | 'а'..='я' | 'Ё' | 'ё'))
}

fn count_char(s: &str, c: char) -> usize {
    s.chars().filter(|&ch| ch == c).count()
}

fn is_statement_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::ASSIGN_STMT
            | SyntaxKind::CALL_STMT
            | SyntaxKind::RETURN_STMT
            | SyntaxKind::IF_STMT
            | SyntaxKind::WHILE_STMT
            | SyntaxKind::FOR_STMT
            | SyntaxKind::FOR_EACH_STMT
            | SyntaxKind::TRY_STMT
            | SyntaxKind::RAISE_STMT
            | SyntaxKind::EXECUTE_STMT
            | SyntaxKind::BREAK_STMT
            | SyntaxKind::CONTINUE_STMT
            | SyntaxKind::GOTO_STMT
            | SyntaxKind::LABEL_STMT
            | SyntaxKind::ADD_HANDLER_STMT
            | SyntaxKind::REMOVE_HANDLER_STMT
            | SyntaxKind::EMPTY_STMT
    )
}

fn find_ancestor_by_kind(token: &SyntaxToken, kind: SyntaxKind) -> Option<SyntaxNode> {
    let mut node = token.parent();
    while let Some(current) = node {
        if current.kind() == kind {
            return Some(current);
        }
        node = current.parent();
    }
    None
}

fn find_statement_ancestor(token: &SyntaxToken) -> Option<SyntaxNode> {
    let mut node = token.parent();
    while let Some(current) = node {
        if is_statement_kind(current.kind()) {
            return Some(current);
        }
        node = current.parent();
    }
    None
}

fn skip_statement_context(token: &SyntaxToken, config: &Config) -> bool {
    if let Some(stmt) = find_statement_ancestor(token) {
        let text = stmt.text().to_string();
        return config.search_words_exclusion.is_match(&text);
    }
    false
}

fn skip_param_context(token: &SyntaxToken, config: &Config) -> bool {
    if let Some(param) = find_ancestor_by_kind(token, SyntaxKind::PARAM) {
        let text = param.text().to_string();
        return config.search_words_exclusion.is_match(&text);
    }
    false
}

fn is_version_return(token: &SyntaxToken, config: &Config) -> bool {
    if find_ancestor_by_kind(token, SyntaxKind::RETURN_STMT).is_some() {
        if let Some(func) = find_ancestor_by_kind(token, SyntaxKind::FUNCTION_DEF) {
            let text = func.text().to_string();
            return config.search_words_exclusion.is_match(&text);
        }
    }
    false
}

fn is_letter_before_match(content: &str, match_start: usize) -> bool {
    if match_start == 0 {
        return false;
    }
    let bytes = content.as_bytes();
    if match_start > bytes.len() {
        return false;
    }
    let before = &content[..match_start];
    if let Some(c) = before.chars().last() {
        let lower = c.to_ascii_lowercase();
        return ('g'..='z').contains(&lower) || matches!(c, 'А'..='Я' | 'а'..='я' | 'Ё' | 'ё');
    }
    false
}

fn find_string_token(root: &SyntaxNode, range: LocalRange) -> Option<SyntaxToken> {
    let range = range.in_root();
    root.token_at_offset(range.start())
        .find(|t| t.kind() == SyntaxKind::STRING && t.text_range() == range)
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::UsingHardcodeNetworkAddress;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    let config = Config::from_context(ctx);

    check_body_literals(ctx.body(), ctx.source_map(), ctx.root(), &config, code, ctx, acc);
}

#[allow(clippy::too_many_arguments)]
fn check_body_literals(
    body: &Body,
    source_map: &BodySourceMap,
    root: &SyntaxNode,
    config: &Config,
    code: DiagnosticCode,
    ctx: &BodyContext,
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
) {
    for (expr_id, expr) in body.exprs_iter() {
        let Expr::Literal(Literal::String(content)) = expr else { continue };

        if content.len() <= 2 || is_url(content) {
            continue;
        }

        let Some(matched) = PATTERN_NETWORK_ADDRESS.find(content) else { continue };

        let first_value = matched.as_str();
        let count_dots = count_char(first_value, '.');
        let count_dots_all = count_char(content, '.');
        let find_alphabet = contains_letter(first_value);

        if count_dots > 0 && (count_dots_all > DOTS_IN_IPV4 || find_alphabet) {
            continue;
        }

        if is_localhost(first_value) {
            continue;
        }

        if first_value.starts_with(':') && is_letter_before_match(content, matched.start()) {
            continue;
        }

        let Some(range) = source_map.expr_range(expr_id) else { continue };

        if let Some(token) = find_string_token(root, range) {
            if skip_statement_context(&token, config)
                || skip_param_context(&token, config)
                || is_version_return(&token, config)
            {
                continue;
            }
        }

        if config.search_popular_version_exclusion.is_match(content) {
            continue;
        }

        diagnostics.push(Diagnostic {
            code,
            message: "Используется хранение в коде ip-адреса".to_string(),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::check_body;
    use crate::test_utils::{check_body_diagnostic_with_config, check_diagnostics_snapshot_for};
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;
    #[test]
    fn test_comprehensive() {
        let code = r#"
// IP адреса
СетевойАдрес = "232.252.181.67"; // <-- ошибка
НеСетевойАдрес = "22.00";
НеСетевойАдресЕще = "10.10.2010";
МассивАдресов = Новый Массив();
МассивАдресов.Добавить("160.211.46.126"); // <-- ошибка
МассивАдресов.Добавить("127.0.0.1"); // <- игнорим (localhost)
МассивАдресов.Добавить("300.300.300.300");
МассивАдресов.Добавить("da52:04d9:f9fc:8c45:7180:332d:527f:0c08"); // <-- ошибка
МассивАдресов.Добавить("6885:e6fb:e56e:f46c:2da2:a736:f9dc:68b6"); // <-- ошибка
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:-:d0af:bb82:0eaf:2225");
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:783d:d0af:bb82:0eaf:2225"); // <-- ошибка
Обработчики = Новый ТаблицаЗначений;
Обработчики.Колонки.Добавить("Версия");
Обработчик = Обработчики.Добавить();
Обработчик.Версия = "1.1.1.2";
МассивВерсий = Новый Массив;
МассивВерсий.Добавить("3.1.10.155");
СписокIP = Новый Массив;
СписокIP.Добавить("10.4.10.1"); // <-- ошибка

Тест = "(.*<transportConnector name="")(.*)(""" uri=""tcp://0.0.0.0:61616\?maximumConnections=1000&amp;wireFormat.maxFrameSize=104857600""/>)"; // <-- ошибка
Тест = "_to_SRVMIX"" uri=""static:(tcp://10.1.4.55:61616)"" duplex=""true"" UserName=""system"" Password=""manager""/>"; // <-- ошибка

ЗапуститьПриложение("ping -n 60 127.0.0.1 >nul", , Истина);

// исключаем "Драйвер"
Справочники.ДрайверыОборудования.ЗаполнитьПредопределенныйЭлемент(
	Перечисления.ОбработчикиДрайверовПодключаемогоОборудования.ОбработчикККСДисплеиПокупателя,
	"AddIn.VFCD220E", "ДрайверККСДисплеиПокупателя", Ложь, "1.0.1.1");

Разрешение = ФабрикаXDTO.Создать(
	ФабрикаXDTO.Тип(
		"http://www.1c.ru/1cFresh/ApplicationExtensions/Permissions/1.0.0.1", // игнорим
		РазрешениеВДанных.ВидРазрешения
	)
);

КакойТоКлассификатор = "1.2.3.4.5.6.7.8";

Субъект.Вставить("РНСФСС", Строка(Свойства["OID.1.2.643.3.141.1.1"]));

СвойстваНаселенныйПункт.Добавить("2.5.4.7"); // <- игнорим

СтрокаСообщения = НСтр("ru = '%1
  |Параметры синхронизации данных для этой программы:
  |========================================================
  |%2%3Префикс информационной базы: %4
  |
  |Параметры синхронизации данных для другой программы:: // <-- игнорим
  |========================================================
  |%5%6Префикс информационной базы: %7'");

FoundedElement = HTMLDocument.evaluate(".//img[ancestor::div[@aria-hidden=""False""]]", HTMLDocument.body, null, 0, null).iterateNext(); // <-- игнорим
Правильный = "::1" // <- игнорим (localhost)

Функция ПодготовитьСведенияОВнешнейОбработке(Вид, Назначение, Наименование, Версия, Информация, Адрес = "8.8.8.10", ВерсияБСП = "1.2.1.4", РассылкаОтчетов = Ложь) Экспорт // <-- 1 ошибка 1 игнор
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    Возврат "2.9.4.0";
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    ip = "10.10.10.55"; // <-- 1 ошибка
КонецФункции

СписокПараметров.Добавить("2.5.4.10", Поля.O);
ПараметрыРегистрации = Модуль.СведенияОВнешнейОбработке("2.1.3.1");

ДНС = "8.8.8.8"; // <- ошибка
ПроизвольнаяВерсияСВосьмеркиИТройки = "8.3.10.1";
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#"
                UsingHardcodeNetworkAddress @ 3:16..3:32
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 7:24..7:40
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 10:24..10:65
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 11:24..11:65
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 13:45..13:86
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 21:19..21:30
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 24:8..24:120
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 58:105..58:115
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 66:10..66:23
                  message: Используется хранение в коде ip-адреса
                  severity: Warning
                UsingHardcodeNetworkAddress @ 72:7..72:16
                  message: Используется хранение в коде ip-адреса
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_configure_search_words() {
        let code = r#"
// IP адреса
СетевойАдрес = "232.252.181.67"; // <-- ошибка
НеСетевойАдрес = "22.00";
НеСетевойАдресЕще = "10.10.2010";
МассивАдресов = Новый Массив();
МассивАдресов.Добавить("160.211.46.126"); // <-- ошибка
МассивАдресов.Добавить("127.0.0.1"); // <- игнорим (localhost)
МассивАдресов.Добавить("300.300.300.300");
МассивАдресов.Добавить("da52:04d9:f9fc:8c45:7180:332d:527f:0c08"); // <-- ошибка
МассивАдресов.Добавить("6885:e6fb:e56e:f46c:2da2:a736:f9dc:68b6"); // <-- ошибка
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:-:d0af:bb82:0eaf:2225");
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:783d:d0af:bb82:0eaf:2225"); // <-- ошибка
Обработчики = Новый ТаблицаЗначений;
Обработчики.Колонки.Добавить("Версия");
Обработчик = Обработчики.Добавить();
Обработчик.Версия = "1.1.1.2";
МассивВерсий = Новый Массив;
МассивВерсий.Добавить("3.1.10.155");
СписокIP = Новый Массив;
СписокIP.Добавить("10.4.10.1"); // <-- ошибка

Тест = "(.*<transportConnector name="")(.*)(""" uri=""tcp://0.0.0.0:61616\?maximumConnections=1000&amp;wireFormat.maxFrameSize=104857600""/>)"; // <-- ошибка
Тест = "_to_SRVMIX"" uri=""static:(tcp://10.1.4.55:61616)"" duplex=""true"" UserName=""system"" Password=""manager""/>"; // <-- ошибка

ЗапуститьПриложение("ping -n 60 127.0.0.1 >nul", , Истина); // <- игнорим (localhost)

// исключаем "Драйвер"
Справочники.ДрайверыОборудования.ЗаполнитьПредопределенныйЭлемент(
	Перечисления.ОбработчикиДрайверовПодключаемогоОборудования.ОбработчикККСДисплеиПокупателя,
	"AddIn.VFCD220E", "ДрайверККСДисплеиПокупателя", Ложь, "1.0.1.1");

Разрешение = ФабрикаXDTO.Создать(
	ФабрикаXDTO.Тип(
		"http://www.1c.ru/1cFresh/ApplicationExtensions/Permissions/1.0.0.1", // игнорим
		РазрешениеВДанных.ВидРазрешения
	)
);

КакойТоКлассификатор = "1.2.3.4.5.6.7.8";

Субъект.Вставить("РНСФСС", Строка(Свойства["OID.1.2.643.3.141.1.1"]));

СвойстваНаселенныйПункт.Добавить("2.5.4.7"); // <- игнорим

СтрокаСообщения = НСтр("ru = '%1
  |Параметры синхронизации данных для этой программы:
  |========================================================
  |%2%3Префикс информационной базы: %4
  |
  |Параметры синхронизации данных для другой программы:: // <-- игнорим
  |========================================================
  |%5%6Префикс информационной базы: %7'");

FoundedElement = HTMLDocument.evaluate(".//img[ancestor::div[@aria-hidden=""False""]]", HTMLDocument.body, null, 0, null).iterateNext(); // <-- игнорим
Правильный = "::1" // <- игнорим (localhost)

Функция ПодготовитьСведенияОВнешнейОбработке(Вид, Назначение, Наименование, Версия, Информация, Адрес = "8.8.8.10", ВерсияБСП = "1.2.1.4", РассылкаОтчетов = Ложь) Экспорт // <-- 1 ошибка 1 игнор
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    Возврат "2.9.4.0";
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    ip = "10.10.10.55"; // <-- 1 ошибка
КонецФункции

СписокПараметров.Добавить("2.5.4.10", Поля.O);
ПараметрыРегистрации = Модуль.СведенияОВнешнейОбработке("2.1.3.1");

ДНС = "8.8.8.8"; // <- ошибка
ПроизвольнаяВерсияСВосьмеркиИТройки = "8.3.10.1";
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::UsingHardcodeNetworkAddress,
            serde_json::json!({
                "searchWordsExclusion": "Version"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        assert_eq!(diagnostics.len(), 10, "Expected 10 diagnostics with reduced exclusion");
    }

    #[test]
    fn test_configure_version_exclusion() {
        let code = r#"
// IP адреса
СетевойАдрес = "232.252.181.67"; // <-- ошибка
НеСетевойАдрес = "22.00";
НеСетевойАдресЕще = "10.10.2010";
МассивАдресов = Новый Массив();
МассивАдресов.Добавить("160.211.46.126"); // <-- ошибка
МассивАдресов.Добавить("127.0.0.1"); // <- игнорим (localhost)
МассивАдресов.Добавить("300.300.300.300");
МассивАдресов.Добавить("da52:04d9:f9fc:8c45:7180:332d:527f:0c08"); // <-- ошибка
МассивАдресов.Добавить("6885:e6fb:e56e:f46c:2da2:a736:f9dc:68b6"); // <-- ошибка
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:-:d0af:bb82:0eaf:2225");
Структура = Новый Структура("СетевойАдрес", "6dd7:1388:7fa1:783d:d0af:bb82:0eaf:2225"); // <-- ошибка
Обработчики = Новый ТаблицаЗначений;
Обработчики.Колонки.Добавить("Версия");
Обработчик = Обработчики.Добавить();
Обработчик.Версия = "1.1.1.2";
МассивВерсий = Новый Массив;
МассивВерсий.Добавить("3.1.10.155");
СписокIP = Новый Массив;
СписокIP.Добавить("10.4.10.1"); // <-- ошибка

Тест = "(.*<transportConnector name="")(.*)(""" uri=""tcp://0.0.0.0:61616\?maximumConnections=1000&amp;wireFormat.maxFrameSize=104857600""/>)"; // <-- ошибка
Тест = "_to_SRVMIX"" uri=""static:(tcp://10.1.4.55:61616)"" duplex=""true"" UserName=""system"" Password=""manager""/>"; // <-- ошибка

ЗапуститьПриложение("ping -n 60 127.0.0.1 >nul", , Истина);

// исключаем "Драйвер"
Справочники.ДрайверыОборудования.ЗаполнитьПредопределенныйЭлемент(
	Перечисления.ОбработчикиДрайверовПодключаемогоОборудования.ОбработчикККСДисплеиПокупателя,
	"AddIn.VFCD220E", "ДрайверККСДисплеиПокупателя", Ложь, "1.0.1.1");

Разрешение = ФабрикаXDTO.Создать(
	ФабрикаXDTO.Тип(
		"http://www.1c.ru/1cFresh/ApplicationExtensions/Permissions/1.0.0.1", // игнорим
		РазрешениеВДанных.ВидРазрешения
	)
);

КакойТоКлассификатор = "1.2.3.4.5.6.7.8";

Субъект.Вставить("РНСФСС", Строка(Свойства["OID.1.2.643.3.141.1.1"]));

СвойстваНаселенныйПункт.Добавить("2.5.4.7"); // <- игнорим

СтрокаСообщения = НСтр("ru = '%1
  |Параметры синхронизации данных для этой программы:
  |========================================================
  |%2%3Префикс информационной базы: %4
  |
  |Параметры синхронизации данных для другой программы:: // <-- игнорим
  |========================================================
  |%5%6Префикс информационной базы: %7'");

FoundedElement = HTMLDocument.evaluate(".//img[ancestor::div[@aria-hidden=""False""]]", HTMLDocument.body, null, 0, null).iterateNext(); // <-- игнорим
Правильный = "::1" // <-- ошибка

Функция ПодготовитьСведенияОВнешнейОбработке(Вид, Назначение, Наименование, Версия, Информация, Адрес = "8.8.8.10", ВерсияБСП = "1.2.1.4", РассылкаОтчетов = Ложь) Экспорт // <-- 1 ошибка 1 игнор
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    Возврат "2.9.4.0";
КонецФункции

Функция ВерсияБиблиотеки() Экспорт
    ip = "10.10.10.55"; // <-- 1 ошибка
КонецФункции

СписокПараметров.Добавить("2.5.4.10", Поля.O);
ПараметрыРегистрации = Модуль.СведенияОВнешнейОбработке("2.1.3.1");

ДНС = "8.8.8.8"; // <- ошибка
ПроизвольнаяВерсияСВосьмеркиИТройки = "8.3.10.1";
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::UsingHardcodeNetworkAddress,
            serde_json::json!({
                "searchPopularVersionExclusion": r"^(1|3|8\.3|11)\."
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        assert_eq!(diagnostics.len(), 13, "Expected 13 diagnostics without 2.* exclusion");
    }

    #[test]
    fn test_ipv4_detection() {
        let code = r#"
Процедура Тест()
    IP = "192.168.1.1";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#"
                UsingHardcodeNetworkAddress @ 3:10..3:23
                  message: Используется хранение в коде ip-адреса
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_ipv6_detection() {
        let code = r#"
Процедура Тест()
    IP = "2001:0db8:85a3:0000:0000:8a2e:0370:7334";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#"
                UsingHardcodeNetworkAddress @ 3:10..3:51
                  message: Используется хранение в коде ip-адреса
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_localhost_excluded() {
        let code = r#"
Процедура Тест()
    IP = "127.0.0.1";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_ipv6_loopback_excluded() {
        let code = r#"
Процедура Тест()
    IP = "::1";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_url_excluded() {
        let code = r#"
Процедура Тест()
    URL = "http://192.168.1.1/api";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_version_excluded() {
        let code = r#"
Процедура Тест()
    Версия = "1.2.3.4";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_run_app_excluded() {
        let code = r#"
Процедура Тест()
    ЗапуститьПриложение("ping -n 60 127.0.0.1 >nul", , Истина);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_driver_excluded() {
        let code = r#"
Процедура Тест()
    Справочники.ДрайверыОборудования.ЗаполнитьПредопределенныйЭлемент(
        "Драйвер", "AddIn.VFCD220E", "1.0.1.1");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_invalid_ip_not_detected() {
        let code = r#"
Процедура Тест()
    НеIP = "300.300.300.300";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_version_return_excluded() {
        let code = r#"
Функция ВерсияБиблиотеки() Экспорт
    Возврат "2.9.4.0";
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodeNetworkAddress,
            expect![[r#""#]],
        );
    }
}
