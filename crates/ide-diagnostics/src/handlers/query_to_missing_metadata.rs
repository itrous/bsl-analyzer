use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;
use stdx::case::CaseExt;
use syntax::ast_utils::{active_children, significant_tokens};
use syntax::{SyntaxKind, SyntaxNode, TextRange};

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

/// Whether the query of the literal at `literal_range` is a template the module completes by
/// `СтрЗаменить` / `StrReplace` before it runs, replacing a whole segment of `table_name`.
///
/// Such a template parses cleanly, so its placeholder would otherwise be reported as a missing
/// table forever. The replacement is tied to this query: its first argument is the literal
/// itself or the variable the literal is assigned to, used later in the same method, and its
/// result is used. The search string must equal a whole segment, case included, as the platform
/// searches: a substring match would silence real misses whose name merely contains it.
pub(crate) fn is_substituted_template(
    root: &SyntaxNode,
    literal_range: TextRange,
    table_name: &str,
) -> bool {
    let Some(literal) = root
        .covering_element(literal_range)
        .ancestors()
        .find(|node| node.kind() == SyntaxKind::LITERAL && node.text_range() == literal_range)
    else {
        return false;
    };
    let text = enclosing_text(&literal);
    let Some(parent) = text.parent() else { return false };
    let calls: Vec<SyntaxNode> = match parent.kind() {
        SyntaxKind::ARG_LIST if active_children(&parent).next().as_ref() == Some(&text) => {
            parent.parent().filter(is_str_replace).into_iter().collect()
        }
        SyntaxKind::ASSIGN_STMT => {
            let mut sides = active_children(&parent);
            let (Some(target), Some(value)) = (sides.next(), sides.next()) else { return false };
            if value != text {
                return false;
            }
            let holder = folded(&target);
            let scope = parent
                .ancestors()
                .find(|node| {
                    matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
                })
                .unwrap_or_else(|| root.clone());
            scope
                .descendants()
                .filter(|call| {
                    call.text_range().start() >= parent.text_range().end()
                        && is_str_replace(call)
                        && argument(call, 0).is_some_and(|arg| folded(&arg) == holder)
                })
                .collect()
        }
        _ => return false,
    };
    calls
        .iter()
        .filter(|call| unwrap_up(call).parent().map(|p| p.kind()) != Some(SyntaxKind::CALL_STMT))
        .filter_map(|call| argument(call, 1))
        .filter(|search| search.kind() == SyntaxKind::LITERAL)
        .filter_map(|search| crate::sdbl_utils::extract_string_content(&search))
        .any(|search| table_name.split('.').any(|segment| segment == search))
}

/// The whole text expression the literal belongs to: itself, or the constant concatenation it
/// is a piece of, with any wrapping parentheses.
fn enclosing_text(literal: &SyntaxNode) -> SyntaxNode {
    let mut current = unwrap_up(literal);
    while let Some(parent) = current.parent().filter(is_concatenation) {
        current = unwrap_up(&parent);
    }
    current
}

fn is_concatenation(node: &SyntaxNode) -> bool {
    node.kind() == SyntaxKind::BINARY_EXPR
        && node
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .any(|token| token.kind() == SyntaxKind::PLUS)
}

/// The outermost `EXPR` or parentheses wrapper whose only content is `node`.
fn unwrap_up(node: &SyntaxNode) -> SyntaxNode {
    let mut current = node.clone();
    while let Some(parent) = current
        .parent()
        .filter(|parent| matches!(parent.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR))
    {
        current = parent;
    }
    current
}

/// The node an `EXPR` or parentheses wrapper stands for.
fn unwrap_down(node: &SyntaxNode) -> SyntaxNode {
    let mut current = node.clone();
    while matches!(current.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR) {
        match active_children(&current).next() {
            Some(child) => current = child,
            None => break,
        }
    }
    current
}

/// The global `СтрЗаменить` / `StrReplace`, called by its bare name.
fn is_str_replace(call: &SyntaxNode) -> bool {
    call.kind() == SyntaxKind::CALL_EXPR
        && active_children(call).next().is_some_and(|callee| {
            let name = folded(&unwrap_down(&callee));
            name == "стрзаменить" || name == "strreplace"
        })
}

fn argument(call: &SyntaxNode, index: usize) -> Option<SyntaxNode> {
    let args = active_children(call).find(|node| node.kind() == SyntaxKind::ARG_LIST)?;
    active_children(&args).nth(index).map(|arg| unwrap_down(&arg))
}

/// The node's significant text, case-folded: `Шаблон` and `шаблон` name one variable.
fn folded(node: &SyntaxNode) -> String {
    significant_tokens(node).map(|token| token.text().to_string()).collect::<String>().fold_lower()
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

    fn missing_tables(source: &str) -> Vec<crate::Diagnostic> {
        crate::test_utils::check_with_config_xml(source, DOCUMENT_CONFIG_XML, &[])
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::QueryToMissingMetadata)
            .collect()
    }

    #[test]
    fn a_replacement_of_another_template_leaves_a_separate_query_reported() {
        let found = missing_tables(
            r#"
Процедура Тест(КлючЗначение) Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Блок = СтрЗаменить(Шаблон, "ИмяВидаДокумента", КлючЗначение.Ключ);
    Запрос = Новый Запрос("ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т");
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_replacement_of_unrelated_text_does_not_hide_the_query() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Иное = СтрЗаменить("abc", "ИмяВидаДокумента", "X");
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_replacement_whose_result_is_dropped_does_not_complete_the_query() {
        let found = missing_tables(
            r#"
Процедура Тест(Имя) Экспорт
    ТекстЗапроса = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    СтрЗаменить(ТекстЗапроса, "ИмяВидаДокумента", Имя);
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_search_string_in_another_case_does_not_match_the_placeholder() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Блок = СтрЗаменить(Шаблон, "имявидадокумента", "РеализацияТоваров");
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_literal_passed_straight_to_the_replacement_is_a_template() {
        let found = missing_tables(
            r#"
Процедура Тест(Имя) Экспорт
    Текст = СтрЗаменить("ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т", "ИмяВидаДокумента", Имя);
КонецПроцедуры
"#,
        );
        assert!(found.is_empty(), "{found:?}");
    }
}
