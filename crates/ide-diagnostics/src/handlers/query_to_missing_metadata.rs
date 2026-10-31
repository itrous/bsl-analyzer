use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Sql],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::QueryToMissingMetadata { table_name, range } = diag {
        let code = DiagnosticCode::QueryToMissingMetadata;
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Источник запроса \"{}\" не разрешается в таблицу метаданных конфигурации",
                table_name
            ),
            severity: config.severity(code),
            range: mapper.map_range(*range, query_text),
            tags: config.tags(code),
            fixes: vec![],
        });
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let mut diagnostics = crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::QueryToMissingMetadata,
        dispatch,
    );
    drop_substituted_templates(ctx, &mut diagnostics);
    diagnostics
}

// The runner pass does not call `check`, so it applies this filter itself; without it the
// module-aware narrowing would exist only in a path production never takes.
pub(crate) fn drop_substituted_templates(
    ctx: &DiagnosticsContext,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !diagnostics.iter().any(|d| d.code == DiagnosticCode::QueryToMissingMetadata) {
        return;
    }
    let needles = substituted_needles(ctx);
    if needles.is_empty() {
        return;
    }
    diagnostics.retain(|d| {
        d.code != DiagnosticCode::QueryToMissingMetadata
            || !path_has_substituted_segment(&d.message, &needles)
    });
}

// A query text kept as a template has a placeholder inside the table path that the module
// replaces at run time; without this the placeholder is reported as a missing table forever.
fn substituted_needles(ctx: &DiagnosticsContext) -> Vec<String> {
    use syntax::SyntaxKind;
    let root = ctx.parse().syntax_node();
    let mut needles = Vec::new();
    for tok in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        if tok.kind() != SyntaxKind::IDENT {
            continue;
        }
        // `eq_ignore_ascii_case` folds ASCII only, so a Russian name has to be folded in full:
        // a module that writes `стрзаменить` would otherwise keep the false finding.
        let name = tok.text().to_lowercase();
        if name != "стрзаменить" && name != "strreplace" {
            continue;
        }
        let Some(call) =
            tok.parent().and_then(|p| p.ancestors().find(|n| n.kind() == SyntaxKind::CALL_EXPR))
        else {
            continue;
        };
        let Some(arg_list) = call.children().find(|n| n.kind() == SyntaxKind::ARG_LIST) else {
            continue;
        };
        let Some(second) = arg_list.children().filter(|n| n.kind() == SyntaxKind::EXPR).nth(1)
        else {
            continue;
        };
        let Some(literal) = second.descendants().find(|n| n.kind() == SyntaxKind::LITERAL) else {
            continue;
        };
        if let Some(content) = crate::sdbl_utils::extract_string_content(&literal) {
            if !content.is_empty() {
                needles.push(content);
            }
        }
    }
    needles
}

// Whole-segment comparison only: a substring match would silence real misses whose name merely
// contains the substituted text.
fn path_has_substituted_segment(message: &str, needles: &[String]) -> bool {
    let Some(path) = message.split('"').nth(1) else {
        return false;
    };
    path.split('.')
        .any(|segment| needles.iter().any(|needle| segment.to_lowercase() == needle.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_snapshot_for, check_snapshot_with_config_xml};
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_no_metadata_no_diagnostics() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле ИЗ Справочник.НесуществующийСправочник КАК Т";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryToMissingMetadata,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_diagnostic_properties() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Поле ИЗ Справочник.Валюты КАК Т";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryToMissingMetadata,
            expect![[r#""#]],
        );
    }

    #[test]
    fn track3_existing_common_module_reference_with_config_xml_snapshot() {
        let config_xml = r#"
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>ЗапросыМетаданных</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>
"#;
        let common_module = r#"
#Область ПрограммныйИнтерфейс
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;

        check_snapshot_with_config_xml(
            r#"
#Область ПрограммныйИнтерфейс
// Проверяет ссылку на существующий общий модуль в запросе.
Процедура Тест() Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Модуль.Ссылка КАК Ссылка ИЗ ОбщийМодуль.ЗапросыМетаданных КАК Модуль";
КонецПроцедуры
#КонецОбласти
"#,
            config_xml,
            &[("ЗапросыМетаданных", common_module)],
            expect![[r#"
                QueryToMissingMetadata @ 6:57..6:86
                  message: Источник запроса "ОбщийМодуль.ЗапросыМетаданных" не разрешается в таблицу метаданных конфигурации
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn track3_missing_common_module_reference_with_config_xml_snapshot() {
        let config_xml = r#"
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>ЗапросыМетаданных</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>
"#;
        let common_module = r#"
#Область ПрограммныйИнтерфейс
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;

        check_snapshot_with_config_xml(
            r#"
#Область ПрограммныйИнтерфейс
// Проверяет ссылку на отсутствующий общий модуль в запросе.
Процедура Тест() Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Модуль.Ссылка КАК Ссылка ИЗ ОбщийМодуль.НесуществующийМодуль КАК Модуль";
КонецПроцедуры
#КонецОбласти
"#,
            config_xml,
            &[("ЗапросыМетаданных", common_module)],
            expect![[r#"
                QueryToMissingMetadata @ 6:57..6:89
                  message: Источник запроса "ОбщийМодуль.НесуществующийМодуль" не разрешается в таблицу метаданных конфигурации
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn track3_bilingual_common_module_references_with_config_xml_snapshot() {
        let config_xml = r#"
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>ЗапросыМетаданных</CommonModule>
            <CommonModule>MetadataQueries</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>
"#;
        let common_module = r#"
#Область ПрограммныйИнтерфейс
Процедура Тест() Экспорт
КонецПроцедуры
#КонецОбласти
"#;

        check_snapshot_with_config_xml(
            r#"
#Область ПрограммныйИнтерфейс
// Проверяет русские и английские ссылки на общие модули.
Процедура Тест() Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст =
        "ВЫБРАТЬ Русский.Ссылка КАК РусскаяСсылка
        |ИЗ ОбщийМодуль.ЗапросыМетаданных КАК Русский
        |
        |ОБЪЕДИНИТЬ ВСЕ
        |
        |SELECT English.Ref AS EnglishRef
        |FROM CommonModule.MetadataQueries AS English";
КонецПроцедуры
#КонецОбласти
"#,
            config_xml,
            &[("ЗапросыМетаданных", common_module), ("MetadataQueries", common_module)],
            expect![[r#"
                QueryToMissingMetadata @ 8:13..8:42
                  message: Источник запроса "ОбщийМодуль.ЗапросыМетаданных" не разрешается в таблицу метаданных конфигурации
                  severity: Blocker
                QueryToMissingMetadata @ 13:15..13:43
                  message: Источник запроса "CommonModule.MetadataQueries" не разрешается в таблицу метаданных конфигурации
                  severity: Blocker"#]],
        );
    }

    const DOCUMENT_CONFIG_XML: &str = r#"
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <Document>РеализацияТоваров</Document>
        </ChildObjects>
    </Configuration>
</MetaDataObject>
"#;

    #[test]
    fn a_template_table_name_substituted_in_the_module_is_not_a_missing_table() {
        let diagnostics = crate::test_utils::check_with_config_xml(
            r#"
Процедура Тест(КлючЗначение) Экспорт
    Шаблон =
    "ВЫБРАТЬ
    |	Товары.Ссылка КАК Ссылка
    |ИЗ
    |	Документ.ИмяВидаДокумента.Товары КАК Товары";

    Блок = СтрЗаменить(Шаблон, "ИмяВидаДокумента", КлючЗначение.Ключ);
КонецПроцедуры
"#,
            DOCUMENT_CONFIG_XML,
            &[],
        );
        let found: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::QueryToMissingMetadata)
            .collect();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn the_same_literal_without_the_substitution_is_still_reported() {
        let diagnostics = crate::test_utils::check_with_config_xml(
            r#"
Процедура Тест(КлючЗначение) Экспорт
    Шаблон =
    "ВЫБРАТЬ
    |	Товары.Ссылка КАК Ссылка
    |ИЗ
    |	Документ.ИмяВидаДокумента.Товары КАК Товары";
КонецПроцедуры
"#,
            DOCUMENT_CONFIG_XML,
            &[],
        );
        let found: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::QueryToMissingMetadata)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn the_substitution_is_recognised_whatever_case_its_name_is_written_in() {
        let diagnostics = crate::test_utils::check_with_config_xml(
            r#"
Процедура Тест(КлючЗначение) Экспорт
    Шаблон =
    "ВЫБРАТЬ
    |	Товары.Ссылка КАК Ссылка
    |ИЗ
    |	Документ.ИмяВидаДокумента.Товары КАК Товары";

    Блок = стрзаменить(Шаблон, "ИмяВидаДокумента", КлючЗначение.Ключ);
КонецПроцедуры
"#,
            DOCUMENT_CONFIG_XML,
            &[],
        );
        let found: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::QueryToMissingMetadata)
            .collect();
        assert!(found.is_empty(), "{found:?}");
    }
}
