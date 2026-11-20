use crate::define_metadata;
use crate::metadata::*;
use crate::utils::nstr::{
    extract_language_entries, extract_language_keys, get_assigned_variable_name,
    has_template_in_parents, is_nstr_call, is_variable_used_in_template, NstrConfig,
};
use crate::BodyContext;
use crate::{sdbl_utils, Diagnostic, DiagnosticCode};
use hir::LocalRange;
use syntax::SyntaxKind;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Localize],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

/// `MultilingualStringHasEmptyTranslation` is emitted by this same handler: it reuses the
/// `НСтр` call recognition and the declared-language list, and the language list stays a
/// single setting (`declaredLanguages` of `MultilingualStringHasAllDeclaredLanguages`).
pub const EMPTY_TRANSLATION_METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Localize],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

const EMPTY_TRANSLATION_MESSAGE: &str =
    "Объявленный язык с пустым переводом: текст в интерфейсе будет пустым";

/// The first `НСтр` argument when it is exactly one string literal, possibly in parentheses,
/// so its text is known statically; concatenations and variables are not.
fn static_literal_content(first_arg: &syntax::SyntaxNode) -> Option<String> {
    let mut node = first_arg.clone();
    while matches!(node.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR) {
        let only_parens = node.children_with_tokens().filter_map(|it| it.into_token()).all(|t| {
            t.kind().is_trivia() || matches!(t.kind(), SyntaxKind::L_PAREN | SyntaxKind::R_PAREN)
        });
        let mut children = node.children();
        let inner = children.next()?;
        if !only_parens || children.next().is_some() {
            return None;
        }
        node = inner;
    }
    if node.kind() != SyntaxKind::LITERAL {
        return None;
    }
    sdbl_utils::extract_string_content(&node)
}

/// A declared language present in the literal with a value that is empty or whitespace only,
/// while another entry of the same literal carries text: the text exists, but not for this
/// language. A literal blank in every language (`ru=''; uk=''`) loses nothing and is a
/// deliberate empty string.
fn has_empty_declared_translation(content: &str, config: &NstrConfig) -> bool {
    let entries = extract_language_entries(content);
    let has_text =
        entries.iter().any(|(_, value)| value.as_deref().is_some_and(|v| !v.trim().is_empty()));
    has_text
        && entries.iter().any(|(lang, value)| {
            config.declared_languages.contains(lang)
                && value.as_deref().is_some_and(|v| v.trim().is_empty())
        })
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let _span = tracing::debug_span!("MultilingualStringHasAllDeclaredLanguages::check").entered();

    let code = DiagnosticCode::MultilingualStringHasAllDeclaredLanguages;
    let empty_code = DiagnosticCode::MultilingualStringHasEmptyTranslation;

    let missing_enabled = !ctx.is_disabled_with_metadata(code);
    let empty_enabled = !ctx.is_disabled_with_metadata(empty_code);
    if !missing_enabled && !empty_enabled {
        return;
    }

    let config = NstrConfig::from_context(ctx, code);

    let mut diagnostics = Vec::new();

    for tok in ctx.tokens() {
        if tok.kind() != SyntaxKind::IDENT || !is_nstr_call(tok.text()) {
            continue;
        }

        let call_expr = match tok
            .parent()
            .and_then(|p| p.ancestors().find(|n| n.kind() == SyntaxKind::CALL_EXPR))
        {
            Some(ce) => ce,
            None => continue,
        };

        let arg_list = call_expr.children().find(|n| n.kind() == SyntaxKind::ARG_LIST);
        let arg_list = match arg_list {
            Some(al) => al,
            None => {
                if !missing_enabled {
                    continue;
                }
                diagnostics.push(Diagnostic {
                    code,
                    message: format!(
                        "Добавьте строки для языков: [{}]",
                        config.declared_languages.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                    severity: ctx.severity(code),
                    range: LocalRange::of_detached_node(call_expr.text_range()),
                    tags: ctx.tags(code),
                    fixes: vec![],
                });
                continue;
            }
        };

        let first_arg = arg_list.children().find(|n| n.kind() == SyntaxKind::EXPR);

        // An empty translation is lost in a `СтрШаблон` flow too, so this runs before the
        // template skips that only silence the missing-language finding.
        if empty_enabled {
            let is_empty = first_arg
                .as_ref()
                .and_then(static_literal_content)
                .is_some_and(|content| has_empty_declared_translation(&content, &config));
            if is_empty {
                diagnostics.push(Diagnostic {
                    code: empty_code,
                    message: EMPTY_TRANSLATION_MESSAGE.to_string(),
                    severity: ctx.severity(empty_code),
                    range: LocalRange::of_detached_node(call_expr.text_range()),
                    tags: ctx.tags(empty_code),
                    fixes: vec![],
                });
            }
        }

        if !missing_enabled {
            continue;
        }

        if has_template_in_parents(&call_expr) {
            continue;
        }

        if let Some(var_name) = get_assigned_variable_name(&call_expr) {
            if is_variable_used_in_template(&var_name, &call_expr) {
                continue;
            }
        }

        let first_arg = match first_arg {
            Some(a) => a,
            None => {
                diagnostics.push(Diagnostic {
                    code,
                    message: format!(
                        "Добавьте строки для языков: [{}]",
                        config.declared_languages.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                    severity: ctx.severity(code),
                    range: LocalRange::of_detached_node(call_expr.text_range()),
                    tags: ctx.tags(code),
                    fixes: vec![],
                });
                continue;
            }
        };

        let literal = first_arg.descendants().find(|n| n.kind() == SyntaxKind::LITERAL);
        let literal = match literal {
            Some(l) => l,
            None => continue,
        };

        let string_content = match sdbl_utils::extract_string_content(&literal) {
            Some(s) => s,
            None => continue,
        };

        let found_languages = extract_language_keys(&string_content);

        let missing: Vec<&String> = config
            .declared_languages
            .iter()
            .filter(|lang| !found_languages.contains(*lang))
            .collect();

        if !missing.is_empty() {
            let missing_str = missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");

            diagnostics.push(Diagnostic {
                code,
                message: format!("Добавьте строки для языков: [{}]", missing_str),
                severity: ctx.severity(code),
                range: LocalRange::of_detached_node(call_expr.text_range()),
                tags: ctx.tags(code),
                fixes: vec![],
            });
        }
    }

    tracing::debug!(
        count = diagnostics.len(),
        "MultilingualStringHasAllDeclaredLanguages diagnostics found"
    );

    acc.extend(diagnostics);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_body_diagnostic_with_config, format_diags};
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;
    #[test]
    fn test_extract_language_keys() {
        let keys = extract_language_keys("ru='Привет'; en='Hello'");
        assert!(keys.contains("ru"));
        assert!(keys.contains("en"));
        assert_eq!(keys.len(), 2);

        let keys2 = extract_language_keys("ru = 'Привет' ; en = \"Hello\"");
        assert!(keys2.contains("ru"));
        assert!(keys2.contains("en"));

        let keys3 = extract_language_keys("Просто текст без языков");
        assert!(keys3.is_empty());
    }

    #[test]
    fn test_only_ru() {
        let code = r#"// Считаем, что в конфигурации два языка ru и en
Процедура БезОшибок()

    Приветствие = НСтр("ru='Привет, я простая строка';
        |en='Hi, i'm a simple string'");

    ПолеЗапроса = СтрШаблон("%1.%2 КАК %2", "Документ", "Автомобиль");

КонецПроцедуры

Функция СОшибками(Строка)

    БезТекста = НСтр();
    СНевернымФорматомСтроки = НСтр("Тут текст который не относиться к ниодному языку");

    ТекстТолькоНаРусском = НСтр("ru='Привет, я простая строка''");
    ТекстТолькоНаАнглийском = НСтр("en='Hi, i'm a simple string'");

    СообщениеПользователю = СтрШаблон(НСтр("ru='В строке №%1 не заполнена номенклатура'"), Строка.Номер); // <-- для ru и en ошибка
    СообщениеПользователю = СтрШаблон(НСтр("en='In line №%1 nomenclature is not filled'"), Строка.Номер); // <-- для ru, ru и en ошибка

    ТекстТолькоНаРусском2 = НСтр("ru='В строке №%1 не заполнена номенклатура'");
    СообщениеПользователю = СтрШаблон(ТекстТолькоНаРусском2, Строка.Номер);

    ТекстТолькоНаАнглийском2 = НСтр("en='In line №%1 nomenclature is not filled'");
    СообщениеПользователю = СтрШаблон(ТекстТолькоНаАнглийском2, Строка.Номер);

    ТекстТолькоНаРусскомСПробелами = НСтр("ru='Привет, я простая строка''");
    ПриветствиеСПробелами = НСтр("ru = 'Привет, я простая строка'; en = 'Hi, i'm a simple string'");
    Возврат КонструкторАдресов();

    Порция = Сервис.Autocomplete(ИдентификаторАдресногоОбъекта, 0, НСтр("ru = 'ДОМ='"), 1, КодЯзыка, Метаданные.Имя);

    ВычисляемоеПоле.Оформление.УстановитьЗначениеПараметра("Формат", НСтр("ru = 'ДФ=''д ММММ'''"));

    Возврат НСтр("en=""You must specify the user's extension number to the PBX."";ru='Необходимо указать внутренний номер пользователя АТС.'");

КонецФункции

Процедура СноваБезОшибок()

    БыстрыйДоступПредставление = СтроковыеФункцииКлиентСервер.СтрокаСЧисломДляЛюбогоЯзыка(
        НСтр("ru = ';%1 пользователь;;%1 пользователя;%1 пользователей;%1 пользователя'"), КоличествоПользователей);

    Для каждого КодЯзыка Из КодыЯзыков Цикл

    	Элемент[ИмяЛокализуемогоРеквизита(ИмяРеквизита, КодЯзыка)] = НСтр(ИсходнаяСтрока, КодЯзыка);

    КонецЦикла;

КонецПроцелуры
"#;
        let config = DiagnosticsConfig::default();
        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        expect![[r#"
            MultilingualStringHasAllDeclaredLanguages @ 13:17..13:23
              message: Добавьте строки для языков: [ru]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 14:31..14:87
              message: Добавьте строки для языков: [ru]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 17:31..17:67
              message: Добавьте строки для языков: [ru]
              severity: Error"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_ru_and_en() {
        let code = r#"// Считаем, что в конфигурации два языка ru и en
Процедура БезОшибок()

    Приветствие = НСтр("ru='Привет, я простая строка';
        |en='Hi, i'm a simple string'");

    ПолеЗапроса = СтрШаблон("%1.%2 КАК %2", "Документ", "Автомобиль");

КонецПроцедуры

Функция СОшибками(Строка)

    БезТекста = НСтр();
    СНевернымФорматомСтроки = НСтр("Тут текст который не относиться к ниодному языку");

    ТекстТолькоНаРусском = НСтр("ru='Привет, я простая строка''");
    ТекстТолькоНаАнглийском = НСтр("en='Hi, i'm a simple string'");

    СообщениеПользователю = СтрШаблон(НСтр("ru='В строке №%1 не заполнена номенклатура'"), Строка.Номер); // <-- для ru и en ошибка
    СообщениеПользователю = СтрШаблон(НСтр("en='In line №%1 nomenclature is not filled'"), Строка.Номер); // <-- для ru, ru и en ошибка

    ТекстТолькоНаРусском2 = НСтр("ru='В строке №%1 не заполнена номенклатура'");
    СообщениеПользователю = СтрШаблон(ТекстТолькоНаРусском2, Строка.Номер);

    ТекстТолькоНаАнглийском2 = НСтр("en='In line №%1 nomenclature is not filled'");
    СообщениеПользователю = СтрШаблон(ТекстТолькоНаАнглийском2, Строка.Номер);

    ТекстТолькоНаРусскомСПробелами = НСтр("ru='Привет, я простая строка''");
    ПриветствиеСПробелами = НСтр("ru = 'Привет, я простая строка'; en = 'Hi, i'm a simple string'");
    Возврат КонструкторАдресов();

    Порция = Сервис.Autocomplete(ИдентификаторАдресногоОбъекта, 0, НСтр("ru = 'ДОМ='"), 1, КодЯзыка, Метаданные.Имя);

    ВычисляемоеПоле.Оформление.УстановитьЗначениеПараметра("Формат", НСтр("ru = 'ДФ=''д ММММ'''"));

    Возврат НСтр("en=""You must specify the user's extension number to the PBX."";ru='Необходимо указать внутренний номер пользователя АТС.'");

КонецФункции

Процедура СноваБезОшибок()

    БыстрыйДоступПредставление = СтроковыеФункцииКлиентСервер.СтрокаСЧисломДляЛюбогоЯзыка(
        НСтр("ru = ';%1 пользователь;;%1 пользователя;%1 пользователей;%1 пользователя'"), КоличествоПользователей);

    Для каждого КодЯзыка Из КодыЯзыков Цикл

    	Элемент[ИмяЛокализуемогоРеквизита(ИмяРеквизита, КодЯзыка)] = НСтр(ИсходнаяСтрока, КодЯзыка);

    КонецЦикла;

КонецПроцелуры
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
            serde_json::json!({
                "declaredLanguages": "ru,en"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        let snapshot = format_diags(code, &diagnostics);
        expect![[r#"
            MultilingualStringHasAllDeclaredLanguages @ 13:17..13:23
              message: Добавьте строки для языков: [en, ru]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 14:31..14:87
              message: Добавьте строки для языков: [en, ru]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 16:28..16:66
              message: Добавьте строки для языков: [en]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 17:31..17:67
              message: Добавьте строки для языков: [ru]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 28:38..28:76
              message: Добавьте строки для языков: [en]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 32:68..32:87
              message: Добавьте строки для языков: [en]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 34:70..34:98
              message: Добавьте строки для языков: [en]
              severity: Error
            MultilingualStringHasAllDeclaredLanguages @ 43:9..43:90
              message: Добавьте строки для языков: [en]
              severity: Error"#]]
        .assert_eq(&snapshot);
    }

    #[test]
    fn test_no_error_when_all_languages_present() {
        let code = r#"
Процедура Тест()
    Приветствие = НСтр("ru='Привет'; en='Hello'");
КонецПроцедуры
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
            serde_json::json!({
                "declaredLanguages": "ru,en"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_skip_nstr_in_str_template() {
        let code = r#"
Процедура Тест()
    Сообщение = СтрШаблон(НСтр("ru='Значение: %1'"), Значение);
КонецПроцедуры
"#;
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
            serde_json::json!({
                "declaredLanguages": "ru,en"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    fn codes_for(body: &str) -> Vec<(DiagnosticCode, String)> {
        let code = format!("Процедура Тест()\n{body}\nКонецПроцедуры\n");
        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MultilingualStringHasAllDeclaredLanguages,
            serde_json::json!({
                "declaredLanguages": "ru,uk"
            }),
        );
        check_body_diagnostic_with_config(&code, config, check_body)
            .into_iter()
            .map(|d| (d.code, d.message))
            .collect()
    }

    const EMPTY_MESSAGE: &str =
        "Объявленный язык с пустым переводом: текст в интерфейсе будет пустым";

    #[test]
    fn empty_value_for_declared_language_is_flagged() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'; uk = ''");"#);
        assert_eq!(
            found,
            vec![(
                DiagnosticCode::MultilingualStringHasEmptyTranslation,
                EMPTY_MESSAGE.to_string()
            )]
        );
    }

    #[test]
    fn whitespace_only_value_is_flagged_as_empty() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'; uk = '   '");"#);
        assert_eq!(
            found,
            vec![(
                DiagnosticCode::MultilingualStringHasEmptyTranslation,
                EMPTY_MESSAGE.to_string()
            )]
        );
    }

    #[test]
    fn non_empty_value_is_not_flagged() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'; uk = 'Залишок'");"#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn missing_entry_gives_only_the_missing_language_diagnostic() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'");"#);
        assert_eq!(
            found.iter().map(|(code, _)| *code).collect::<Vec<_>>(),
            vec![DiagnosticCode::MultilingualStringHasAllDeclaredLanguages]
        );
    }

    #[test]
    fn parenthesized_literal_is_statically_known() {
        let found = codes_for(r#"    Текст = НСтр((("ru = 'Остаток'; uk = ''")));"#);
        assert_eq!(
            found,
            vec![(
                DiagnosticCode::MultilingualStringHasEmptyTranslation,
                EMPTY_MESSAGE.to_string()
            )]
        );
    }

    #[test]
    fn literal_with_empty_translation_inside_a_wider_expression_is_not_known() {
        let found = codes_for(
            "    Текст = НСтр((\"ru = 'Остаток'; uk = ''\") + Хвост);\n    Другой = НСтр(-\"ru = 'Остаток'; uk = ''\");",
        );
        assert!(
            found
                .iter()
                .all(|(code, _)| *code != DiagnosticCode::MultilingualStringHasEmptyTranslation),
            "{found:?}"
        );
    }

    #[test]
    fn literal_that_is_not_statically_known_yields_nothing() {
        let found = codes_for(
            "    Текст = НСтр(ИсходнаяСтрока);\n    Другой = НСтр(\"ru = 'Остаток'; uk = '\" + Хвост + \"'\");",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn english_spelling_behaves_the_same() {
        let found = codes_for(
            "    Text = NStr(\"ru = 'Остаток'; uk = ''\");\n    Other = NStr(\"ru = 'Остаток'; uk = 'Залишок'\");",
        );
        assert_eq!(
            found,
            vec![(
                DiagnosticCode::MultilingualStringHasEmptyTranslation,
                EMPTY_MESSAGE.to_string()
            )]
        );
    }

    #[test]
    fn empty_value_of_undeclared_language_is_not_flagged() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'; uk = 'Залишок'; en = ''");"#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn empty_value_inside_str_template_is_flagged() {
        let found =
            codes_for(r#"    Текст = СтрШаблон(НСтр("ru = 'Остаток %1'; uk = ''"), Значение);"#);
        assert_eq!(
            found,
            vec![(
                DiagnosticCode::MultilingualStringHasEmptyTranslation,
                EMPTY_MESSAGE.to_string()
            )]
        );
    }

    #[test]
    fn doubled_quote_is_an_escaped_quote_not_an_empty_value() {
        let found = codes_for(r#"    Текст = НСтр("ru = 'Остаток'; uk = '''Залишок'''");"#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn literal_blank_in_every_language_is_not_flagged() {
        let found = codes_for(r#"    Текст = НСтр("ru = ''; uk = ''");"#);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn quote_pair_followed_by_text_is_not_read_as_an_empty_value() {
        let found =
            codes_for(r#"    Текст = НСтр("ru='' на '") + НСтр("ru='Остаток'; uk=''Залишок'");"#);
        assert!(
            found
                .iter()
                .all(|(code, _)| *code != DiagnosticCode::MultilingualStringHasEmptyTranslation),
            "{found:?}"
        );
    }
}
