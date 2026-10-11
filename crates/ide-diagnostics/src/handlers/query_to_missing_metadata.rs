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
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::QueryToMissingMetadata,
        dispatch,
    )
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
}
