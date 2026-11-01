use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;
use stdx::case::CaseExt;
use syntax::ast_utils::{active_children, significant_tokens};
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange};

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
/// table forever. The replacements are tied to this query: their first argument is the literal
/// itself or the variable the literal is assigned to, used later in the same method only by
/// such replacements, and every one whose result is used replaces a whole segment of the path.
/// The search string must equal the segment, case included, as the platform searches: a
/// substring match would silence real misses whose name merely contains it.
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
            let scope = parent
                .ancestors()
                .find(|node| {
                    matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
                })
                .unwrap_or_else(|| root.clone());
            match template_replacements(&scope, &parent, &folded(&target)) {
                Some(calls) => calls,
                None => return false,
            }
        }
        _ => return false,
    };
    // Each used result may be the text that runs, so every one of them has to replace a whole
    // segment of this path, and by something other than the search string itself.
    let mut used = calls.iter().filter(|call| result_is_used(call)).peekable();
    used.peek().is_some()
        && used.all(|call| {
            let Some(search) = argument(call, 1).and_then(|arg| string_literal(&arg)) else {
                return false;
            };
            table_name.split('.').any(|segment| segment == search)
                && argument(call, 2).and_then(|arg| string_literal(&arg)).as_ref() != Some(&search)
        })
}

fn string_literal(node: &SyntaxNode) -> Option<String> {
    (node.kind() == SyntaxKind::LITERAL)
        .then(|| crate::sdbl_utils::extract_string_content(node))
        .flatten()
}

/// The replacements that read the text `assignment` stores in `holder`, or `None` when the text
/// is also read some other way — passed to a query, concatenated, returned — and so may run as
/// written. The text lives until the holder or its owner is assigned again in the same block of
/// statements; that assignment's own value still reads it, as `Текст = СтрЗаменить(Текст, ...)`
/// does. An assignment in a nested branch or loop may not run, so the text outlives it.
fn template_replacements(
    scope: &SyntaxNode,
    assignment: &SyntaxNode,
    holder: &str,
) -> Option<Vec<SyntaxNode>> {
    let owner = holder.split('.').next().unwrap_or(holder);
    let is_overwrite = |target: &SyntaxNode| {
        let target = folded(target);
        target == holder || target == owner
    };
    let after = assignment.text_range().end();
    let until = scope
        .descendants()
        .filter(|stmt| {
            stmt.kind() == SyntaxKind::ASSIGN_STMT
                && stmt.text_range().start() >= after
                && stmt.parent() == assignment.parent()
        })
        .find(|stmt| active_children(stmt).next().is_some_and(|target| is_overwrite(&target)))
        .map_or(scope.text_range().end(), |stmt| stmt.text_range().end());
    let mut calls = Vec::new();
    for token in scope.descendants_with_tokens().filter_map(|element| element.into_token()) {
        let start = token.text_range().start();
        if start < after
            || start >= until
            || !token.kind().is_name_token()
            || token.text().fold_lower() != owner
            || is_member_name(&token)
        {
            continue;
        }
        let written = token
            .parent_ancestors()
            .find(|node| node.kind() == SyntaxKind::ASSIGN_STMT)
            .and_then(|stmt| active_children(&stmt).next())
            .is_some_and(|target| target.text_range().contains(start) && is_overwrite(&target));
        if written {
            continue;
        }
        let call = token
            .parent_ancestors()
            .find(|node| node.kind() == SyntaxKind::ARG_LIST)
            .and_then(|args| args.parent())
            .filter(|call| {
                is_str_replace(call)
                    && result_is_used(call)
                    && argument(call, 0).is_some_and(|arg| {
                        arg.text_range().contains(start) && folded(&arg) == holder
                    })
            })?;
        calls.push(call);
    }
    Some(calls)
}

/// A name after a dot is a member of something else, not the variable of the same name.
fn is_member_name(token: &SyntaxToken) -> bool {
    std::iter::successors(token.prev_token(), |prev| prev.prev_token())
        .find(|prev| !prev.kind().is_trivia())
        .is_some_and(|prev| prev.kind() == SyntaxKind::DOT)
}

/// `СтрЗаменить` returns the new text; called as a statement it changes nothing.
fn result_is_used(call: &SyntaxNode) -> bool {
    unwrap_up(call).parent().map(|parent| parent.kind()) != Some(SyntaxKind::CALL_STMT)
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

    #[test]
    fn a_template_also_run_as_written_is_still_reported() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Результат = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "РеализацияТоваров");
    Запрос = Новый Запрос(Шаблон);
    Запрос.Выполнить();
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_query_text_replaced_in_place_is_a_template() {
        let found = missing_tables(
            r#"
Процедура Тест(Имя) Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Запрос.Текст = СтрЗаменить(Запрос.Текст, "ИмяВидаДокумента", Имя);
    Запрос.Выполнить();
КонецПроцедуры
"#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_template_replaced_once_per_key_is_a_template() {
        let found = missing_tables(
            r#"
Процедура Тест(Виды) Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Тексты = Новый Массив;
    Для Каждого КлючЗначение Из Виды Цикл
        Тексты.Добавить(СтрЗаменить(Шаблон, "ИмяВидаДокумента", КлючЗначение.Ключ));
    КонецЦикла;
КонецПроцедуры
"#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_replacement_that_keeps_the_text_does_not_complete_the_template() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Текст = СтрЗаменить("ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т", "ИмяВидаДокумента", "ИмяВидаДокумента");
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn an_overwrite_in_a_branch_does_not_end_the_template() {
        let found = missing_tables(
            r#"
Процедура Тест(Флаг) Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Блок = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "РеализацияТоваров");
    Если Флаг Тогда
        Шаблон = "";
    КонецЕсли;
    Запрос = Новый Запрос(Шаблон);
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn an_overwrite_in_the_same_block_ends_the_template() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Блок = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "РеализацияТоваров");
    Шаблон = "";
    Запрос = Новый Запрос(Шаблон);
КонецПроцедуры
"#,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_replacement_that_keeps_the_text_beside_one_that_changes_it_is_still_reported() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Исправленный = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "РеализацияТоваров");
    Неизмененный = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "ИмяВидаДокумента");
    Запрос = Новый Запрос(Неизмененный);
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_template_also_replaced_elsewhere_is_still_reported() {
        let found = missing_tables(
            r#"
Процедура Тест() Экспорт
    Шаблон = "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Документ.ИмяВидаДокумента.Товары КАК Т";
    Исправленный = СтрЗаменить(Шаблон, "ИмяВидаДокумента", "РеализацияТоваров");
    БезИзменений = СтрЗаменить(Шаблон, "НесуществующийФрагмент", "X");
    Запрос = Новый Запрос(БезИзменений);
КонецПроцедуры
"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }
}
