use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::LocalRange;
use regex::Regex;
use syntax::{SyntaxKind, SyntaxToken};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 15,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

const DEFAULT_SEARCH_WORDS_STD_PATHS_UNIX: &str =
    r"bin|boot|dev|etc|home|lib|lost\+found|misc|mnt|media|opt|proc|root|run|sbin|tmp|usr|var";

fn is_hardcode_path(content: &str) -> bool {
    let bytes = content.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let first_char = bytes[0];
    let has_separator = content.contains('\\') || content.contains('/');

    match first_char {
        b'/' => has_separator && content.len() > 1,
        b'~' => has_separator,
        b'%' => {
            if let Some(end_percent) = content[1..].find('%') {
                let after_var = &content[end_percent + 2..];
                after_var.starts_with('\\') || after_var.starts_with('/')
            } else {
                false
            }
        }
        b'\\' => bytes.len() > 1 && (bytes[1] == b'\\' || bytes[1] == b'/'),
        c if c.is_ascii_alphabetic() => {
            bytes.len() > 2 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
        }
        _ => false,
    }
}

#[derive(Debug, Clone)]
struct Config {
    search_words_std_paths_unix: Regex,
}

impl Config {
    fn from_context(ctx: &BodyContext) -> Self {
        let std_paths_str = ctx
            .config
            .get_string(DiagnosticCode::UsingHardcodePath, "searchWordsStdPathsUnix")
            .unwrap_or(DEFAULT_SEARCH_WORDS_STD_PATHS_UNIX);

        let search_words_std_paths_unix = Regex::new(&format!("(?i)^/({})(/|$)", std_paths_str))
            .unwrap_or_else(|_| {
                Regex::new(&format!("(?i)^/({})(/|$)", DEFAULT_SEARCH_WORDS_STD_PATHS_UNIX))
                    .unwrap()
            });

        Self { search_words_std_paths_unix }
    }
}

fn extract_string_content(text: &str) -> Option<String> {
    if text.len() < 3 {
        return None;
    }
    Some(text[1..text.len() - 1].to_string())
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
    after_scheme.is_some_and(|pos| bytes.get(pos).is_some_and(|&b| b != b' '))
}

#[inline]
pub fn check_token(token: &SyntaxToken, acc: &mut Vec<Diagnostic<LocalRange>>, ctx: &BodyContext) {
    let code = DiagnosticCode::UsingHardcodePath;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    if token.kind() != SyntaxKind::STRING {
        return;
    }

    let text = token.text();
    if text.len() <= 4 {
        return;
    }

    let Some(content) = extract_string_content(text) else {
        return;
    };

    if is_url(&content) {
        return;
    }

    if !is_hardcode_path(&content) {
        return;
    }

    if content.starts_with('/') {
        let config = Config::from_context(ctx);
        if !config.search_words_std_paths_unix.is_match(&content) {
            return;
        }
    }

    acc.push(Diagnostic {
        code,
        message: "Используется хранение в коде пути к файлу".to_string(),
        severity: ctx.severity(code),
        range: LocalRange::of_detached_node(token.text_range()),
        tags: ctx.tags(code),
        fixes: vec![],
    });
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for_with_config, check_diagnostics_snapshot_for};
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;
    #[test]
    fn test_comprehensive() {
        let code = r#"// Пути к файлам и каталогам
//////////////////////////////////////////

Массив = Новый Массив;
// windows
Массив.Добавить("\\test\test\TEST.xls"); // <-- ошибка
Массив.Добавить("\\server\share\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("\\server\share\myfile.txt"); // <-- ошибка
Массив.Добавить("\\123.123.123.123\share\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("c:\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("c:\folder\myfileWithoutExtension"); // <-- ошибка
Массив.Добавить("c:\folder"); // <-- ошибка
Массив.Добавить("c:/folder"); // <-- ошибка
Массив.Добавить("c://folder"); // <-- ошибка
Массив.Добавить("//test/test/TEST.xls"); // <-- ошибка
Массив.Добавить("C:\Program Files (x86)\"); // <-- ошибка
Массив.Добавить("F:\DATA\Develop\Module.bsl"); // <-- ошибка
// unix
Массив.Добавить("/dev/test"); // <-- ошибка
Массив.Добавить("/dev/test/test.bsl"); // <-- ошибка
Массив.Добавить("/home/root/test.bsl"); // <-- ошибка
Массив.Добавить("/etc/php-fpm/php.ini"); // <-- ошибка
Массив.Добавить("/less/test");

// не сработает, там нет путей
ПутьОтносительный = "./catalog"; // не должно срабатывать
Путь = "/catalog"; // исключаем, т.к. может использовано для URL и будет FP
ПутьКФайлу = "d:тутНетПутиКФайлу";
ПутьКоВторомуФайлу = "тут/нетпутикфайлу";
Структура = Новый Структура("Путь", "Съешь ещё этих мягких французских булок, да выпей же чаю.");

// Снова проверки путей
ПутьWin = "~/temp/file.txt"; // <-- ошибка
ПутьДляСомневающихся = "C:\temp/обратный слеш\тоже/работает"; // <-- ошибка
НеПуть = "!/bin/sh";

// Тут невозможно отличить fp с доменным путем от обычного сетевого пути в windows.
ДоменныйПуть = "\\domain\user"; // <-- ошибка

ПутьОтКаталогаЮзера = "%UserProfile%/что/то/там"; // <-- ошибка

ТипыНастроек.Добавить("/CurrentVarianKey");

КомандаЗапуска.Добавить("/LoadExternalDataProcessorOrReportFromFile");

Разрешение = ФабрикаXDTO.Создать(
	ФабрикаXDTO.Тип(
		"http://www.1c.ru/1cFresh/ApplicationExtensions/Permissions/1.0.0.1",
		РазрешениеВДанных.ВидРазрешения
	)
);
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 6:17..6:39
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 7:17..7:51
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 8:17..8:44
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 9:17..9:60
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 10:17..10:39
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 11:17..11:51
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 12:17..12:28
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 13:17..13:28
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 14:17..14:29
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 16:17..16:42
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 17:17..17:45
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 19:17..19:28
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 20:17..20:37
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 21:17..21:38
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 22:17..22:39
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 33:11..33:28
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 34:24..34:61
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 38:16..38:31
              message: Используется хранение в коде пути к файлу
              severity: Warning
            UsingHardcodePath @ 40:23..40:49
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_configure() {
        let code = r#"// Пути к файлам и каталогам
//////////////////////////////////////////

Массив = Новый Массив;
// windows
Массив.Добавить("\\test\test\TEST.xls"); // <-- ошибка
Массив.Добавить("\\server\share\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("\\server\share\myfile.txt"); // <-- ошибка
Массив.Добавить("\\123.123.123.123\share\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("c:\folder\myfile.txt"); // <-- ошибка
Массив.Добавить("c:\folder\myfileWithoutExtension"); // <-- ошибка
Массив.Добавить("c:\folder"); // <-- ошибка
Массив.Добавить("c:/folder"); // <-- ошибка
Массив.Добавить("c://folder"); // <-- ошибка
Массив.Добавить("//test/test/TEST.xls"); // <-- ошибка
Массив.Добавить("C:\Program Files (x86)\"); // <-- ошибка
Массив.Добавить("F:\DATA\Develop\Module.bsl"); // <-- ошибка
// unix
Массив.Добавить("/dev/test"); // <-- ошибка
Массив.Добавить("/dev/test/test.bsl"); // <-- ошибка
Массив.Добавить("/home/root/test.bsl"); // <-- ошибка
Массив.Добавить("/etc/php-fpm/php.ini"); // <-- ошибка
Массив.Добавить("/less/test");

// не сработает, там нет путей
ПутьОтносительный = "./catalog"; // не должно срабатывать
Путь = "/catalog"; // исключаем, т.к. может использовано для URL и будет FP
ПутьКФайлу = "d:тутНетПутиКФайлу";
ПутьКоВторомуФайлу = "тут/нетпутикфайлу";
Структура = Новый Структура("Путь", "Съешь ещё этих мягких французских булок, да выпей же чаю.");

// Снова проверки путей
ПутьWin = "~/temp/file.txt"; // <-- ошибка
ПутьДляСомневающихся = "C:\temp/обратный слеш\тоже/работает"; // <-- ошибка
НеПуть = "!/bin/sh";

// Тут невозможно отличить fp с доменным путем от обычного сетевого пути в windows.
ДоменныйПуть = "\\domain\user"; // <-- ошибка

ПутьОтКаталогаЮзера = "%UserProfile%/что/то/там"; // <-- ошибка

ТипыНастроек.Добавить("/CurrentVarianKey");

КомандаЗапуска.Добавить("/LoadExternalDataProcessorOrReportFromFile");

Разрешение = ФабрикаXDTO.Создать(
	ФабрикаXDTO.Тип(
		"http://www.1c.ru/1cFresh/ApplicationExtensions/Permissions/1.0.0.1",
		РазрешениеВДанных.ВидРазрешения
	)
);
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::UsingHardcodePath,
            serde_json::json!({
                "searchWordsStdPathsUnix": "home|lib"
            }),
        );

        let diagnostics =
            check_diagnostics_for_with_config(code, config, DiagnosticCode::UsingHardcodePath);
        assert_eq!(diagnostics.len(), 16, "Expected 16 diagnostics with reduced Unix paths");
    }

    #[test]
    fn test_windows_path_detection() {
        let code = r#"
Процедура Тест()
    Путь = "C:\folder\file.txt";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 3:12..3:32
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_unix_path_detection() {
        let code = r#"
Процедура Тест()
    Путь = "/home/user/file.txt";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 3:12..3:33
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_unc_path_detection() {
        let code = r#"
Процедура Тест()
    Путь = "\\server\share\folder";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 3:12..3:35
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_tilde_path_detection() {
        let code = r#"
Процедура Тест()
    Путь = "~/Documents/file.txt";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 3:12..3:34
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_env_var_path_detection() {
        let code = r#"
Процедура Тест()
    Путь = "%UserProfile%/Documents/file.txt";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingHardcodePath,
            expect![[r#"
            UsingHardcodePath @ 3:12..3:46
              message: Используется хранение в коде пути к файлу
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_url_excluded() {
        let code = r#"
Процедура Тест()
    URL = "http://www.1c.ru/path/to/resource";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UsingHardcodePath, expect![[r#""#]]);
    }

    #[test]
    fn test_relative_path_excluded() {
        let code = r#"
Процедура Тест()
    Путь = "./catalog";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UsingHardcodePath, expect![[r#""#]]);
    }

    #[test]
    fn test_single_slash_excluded() {
        let code = r#"
Процедура Тест()
    Путь = "/catalog";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UsingHardcodePath, expect![[r#""#]]);
    }

    #[test]
    fn test_non_standard_unix_root_excluded() {
        let code = r#"
Процедура Тест()
    Путь = "/less/test";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UsingHardcodePath, expect![[r#""#]]);
    }
}
