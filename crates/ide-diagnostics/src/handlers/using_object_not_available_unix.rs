use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 30,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Lockinos],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
    clean_code_attribute: CleanCodeAttribute::Adaptable,
};

pub fn from_hir(
    type_name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::UsingObjectNotAvailableUnix;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!(
            "Проверить, что задействованы аналоги \"{}\" при работе в Unix-клиенте.",
            type_name
        ),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_comprehensive() {
        let code = r#"Функция MD5(стрИсх) Экспорт

	МагическоеЧисло = 16;
	objUTF8 = Новый COMОбъект("System.Text.UTF8Encoding");
	objHash = Новый COMОбъект("System.Security.Cryptography.MD5CryptoServiceProvider");
	hash = objHash.ComputeHash_2(objUTF8.GetBytes_4(стрИсх));

	Результат = "";
	Для Каждого hashvalue Из hash Цикл
		Результат = Результат +
			Сред("0123456789abcdef", Цел(hashvalue / МагическоеЧисло) + 1, 1) +
			Сред("0123456789abcdef", hashvalue % МагическоеЧисло + 1, 1);
	КонецЦикла;

	Возврат Результат;

КонецФункции

Функция НоваяПочта()

	Почта = Новый Почта;
	Почта.Подключиться("Outlook");
	Возврат Почта;

КонецФункции

Процедура ПроверкаВсегоИВся()

	Перем Почта;
	СистемнаяИнформация = Новый СистемнаяИнформация();
	Если Не СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Linux_x86 Или ТипПлатформы.Linux_x86_64 Тогда
		Почта = Новый Почта;
	КонецЕсли;

	Если Не СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Linux_x86 Тогда
		Если Истина Тогда
			Почта = Новый Почта;
		КонецЕсли;
	КонецЕсли;

	Если Почта = Неопределено Тогда

		Сообщение = Новый СообщениеПользователю;
		Сообщение.Текст = "Почта не поддерживается";
		Сообщение.Сообщить();

	КонецЕсли;

	Если СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Windows_x86
        Или СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Windows_x86_64 Тогда

        Почта = Новый Почта;

    КонецЕсли;

    Если ОбщегоНазначения.ЭтоWindowsСервер() Тогда

        Почта = Новый Почта;

    КонецЕсли;

    #Если Не ВебКлиент Тогда
    Если ОбщегоНазначенияКлиент.ЭтоWindowsКлиент() Тогда

        Почта = Новый Почта;

    КонецЕсли;
    #КонецЕсли

    Если СистемнаяИнформация.ТипПлатформы = ТипПлатформы.MacOS_x86 Тогда

    	Почта = Новый Почта;

    КонецЕсли;

КонецПроцедуры

Функция НоваяИнтернетПочта()
    // срабатывания не должно быть
    Почта = Новый ИнтернетПочта();
    Возврат Почта;
КонецФункции"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#"
                UsingObjectNotAvailableUnix @ 4:12..4:55
                  message: Проверить, что задействованы аналоги "COMОбъект" при работе в Unix-клиенте.
                  severity: Warning
                UsingObjectNotAvailableUnix @ 5:12..5:84
                  message: Проверить, что задействованы аналоги "COMОбъект" при работе в Unix-клиенте.
                  severity: Warning
                UsingObjectNotAvailableUnix @ 21:10..21:21
                  message: Проверить, что задействованы аналоги "Почта" при работе в Unix-клиенте.
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_com_object_without_guard() {
        let code = r#"
Процедура Тест()
    obj = Новый COMОбъект("test");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#"
                UsingObjectNotAvailableUnix @ 3:11..3:34
                  message: Проверить, что задействованы аналоги "COMОбъект" при работе в Unix-клиенте.
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_mail_without_guard() {
        let code = r#"
Процедура Тест()
    Почта = Новый Почта;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#"
                UsingObjectNotAvailableUnix @ 3:13..3:24
                  message: Проверить, что задействованы аналоги "Почта" при работе в Unix-клиенте.
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_with_linux_guard() {
        let code = r#"
Процедура Тест()
    Если СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Linux_x86 Тогда
        Почта = Новый Почта;
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_with_windows_guard() {
        let code = r#"
Процедура Тест()
    Если СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Windows_x86 Тогда
        Почта = Новый Почта;
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_with_macos_guard() {
        let code = r#"
Процедура Тест()
    Если СистемнаяИнформация.ТипПлатформы = ТипПлатформы.MacOS_x86 Тогда
        Почта = Новый Почта;
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_nested_if_with_guard() {
        let code = r#"
Процедура Тест()
    Если Не СистемнаяИнформация.ТипПлатформы = ТипПлатформы.Linux_x86 Тогда
        Если Истина Тогда
            Почта = Новый Почта;
        КонецЕсли;
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_internet_mail_not_triggered() {
        let code = r#"
Процедура Тест()
    Почта = Новый ИнтернетПочта();
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_english_mail() {
        let code = r#"
Процедура Тест()
    m = New Mail;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#"
                UsingObjectNotAvailableUnix @ 3:9..3:17
                  message: Проверить, что задействованы аналоги "Mail" при работе в Unix-клиенте.
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_english_com_object() {
        let code = r#"
Процедура Тест()
    obj = New COMObject("test");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#"
                UsingObjectNotAvailableUnix @ 3:11..3:32
                  message: Проверить, что задействованы аналоги "COMObject" при работе в Unix-клиенте.
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_with_platform_guard() {
        let code = r#"
Процедура Тест()
    Если ТипПлатформы.Windows Тогда
        obj = Новый COMОбъект("test");
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UsingObjectNotAvailableUnix,
            expect![[r#""#]],
        );
    }
}
