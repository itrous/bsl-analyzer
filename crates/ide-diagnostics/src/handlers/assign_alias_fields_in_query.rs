use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext, Fix, TextEdit};
use ide_db::TextRange;
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub(crate) fn build_alias_fix(
    field_name: &Option<String>,
    raw_name: &Option<String>,
    bsl_range: TextRange,
) -> Vec<Fix> {
    match (field_name, raw_name) {
        (None, Some(name)) => {
            let insert = bsl_range.end();
            vec![Fix::safe(
                format!("Добавить псевдоним КАК {}", name),
                vec![TextEdit {
                    range: TextRange::new(insert, insert),
                    new_text: format!(" КАК {}", name),
                }],
            )]
        }
        (Some(name), _) => {
            let alias_byte_len = name.len() as u32;
            let insert = bsl_range.end() - line_index::TextSize::from(alias_byte_len);
            vec![Fix::safe(
                format!("Добавить ключевое слово КАК перед '{}'", name),
                vec![TextEdit {
                    range: TextRange::new(insert, insert),
                    new_text: "КАК ".to_string(),
                }],
            )]
        }
        _ => vec![],
    }
}

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::AliasWithoutAsKeyword { field_name, raw_name, range } = diag {
        let code = DiagnosticCode::AssignAliasFieldsInQuery;
        let bsl_range = mapper.map_range(*range, query_text);
        let message = if let Some(name) = field_name {
            format!("Поле '{}' должно иметь явный псевдоним с ключевым словом AS/КАК", name)
        } else {
            "Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК".to_string()
        };
        let fixes = build_alias_fix(field_name, raw_name, bsl_range);
        diagnostics.push(Diagnostic {
            code,
            message,
            severity: config.severity(code),
            range: bsl_range,
            tags: config.tags(code),
            fixes,
        });
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::AssignAliasFieldsInQuery,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_snapshot_for, format_diags};
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;

    fn check_standalone_query_snapshot(query_text: &str, expected: expect_test::Expect) {
        let config = DiagnosticsConfig {
            only_enabled: Some(vec![DiagnosticCode::AssignAliasFieldsInQuery]),
            ..Default::default()
        };
        let diagnostics = crate::validate_query_text(&config, None, query_text);
        expected.assert_eq(&format_diags(query_text, &diagnostics));
    }

    #[test]
    fn test_field_with_explicit_as() {
        let query = "SELECT Shelf AS ShelfCode FROM Library";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_field_without_as_keyword() {
        let query = "SELECT Shelf ShelfCode FROM Library";
        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 1:8..1:23
              message: Поле 'ShelfCode' должно иметь явный псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_field_without_alias() {
        let query = "SELECT Shelf FROM Library";
        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 1:8..1:13
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_asterisk_field() {
        let query = "SELECT * FROM Library";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_table_asterisk() {
        let query = "SELECT Library.* FROM Library";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_multiple_fields_mixed() {
        let query = "SELECT Author, Title AS BookTitle, Year Published FROM Library";
        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 1:8..1:14
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning
            AssignAliasFieldsInQuery @ 1:36..1:50
              message: Поле 'Published' должно иметь явный псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_russian_kak_keyword() {
        let query = "ВЫБРАТЬ Стаж КАК ЛетРаботы ИЗ Кадры";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_union_query() {
        let query = "SELECT Title AS T FROM Library UNION SELECT Caption FROM Archive";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_sdbl_russian_query() {
        let query = "ВЫБРАТЬ Должность КАК Позиция, Оклад ИЗ Справочник.Сотрудники";
        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 1:32..1:37
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_query_with_comments() {
        let query = r#"ВЫБРАТЬ
	Поездки.Город КАК Назначение, // имя задано
	Поездки.Суточные, // без имени
	Поездки.ДатаВыезда Выезд // имя без КАК
ИЗ
	Документ.Командировка КАК Поездки // источник

ОБЪЕДИНИТЬ ВСЕ

ВЫБРАТЬ
	Архив.Город, // имена берутся из первой части
	Архив.Суточные,
	Архив.ДатаВыезда
ИЗ
	Документ.КомандировкаАрхив КАК Архив"#;

        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 3:2..3:18
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning
            AssignAliasFieldsInQuery @ 4:2..4:26
              message: Поле 'Выезд' должно иметь явный псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_simple_query_with_hir() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::{RootDatabase, RootDatabaseImpl};
        use test_fixture::Fixture;
        use vfs::VfsPath;

        let code = r#"Процедура ЗагрузитьШтат()
Запрос = "ВЫБРАТЬ Штат.Подразделение, Штат.Ставка КАК Ставка ИЗ РегистрСведений.Штатка КАК Штат";
КонецПроцедуры"#;

        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have at least one file");

        let mut db = RootDatabaseImpl::new();
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        let sdbl_hirs = db.sdbl_hir_in_file(file_id);

        assert_eq!(sdbl_hirs.len(), 1);
        assert!(
            !sdbl_hirs[0].1.queries()[0].hir.diagnostics.is_empty(),
            "Expected diagnostics for fields without AS keyword"
        );
    }

    #[test]
    fn test_wrapped_vs_unwrapped_code() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::{RootDatabase, RootDatabaseImpl};
        use test_fixture::Fixture;
        use vfs::VfsPath;

        let code_wrapped = r#"Процедура ПрочитатьОтпуска()
    Текст = "ВЫБРАТЬ Отпуска.Работник Сотрудник, Отпуска.Дней КАК Дней ИЗ Документ.Отпуск КАК Отпуска";
КонецПроцедуры"#;

        let fixture_text = format!("//- /test.bsl\n{}", code_wrapped);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have at least one file");

        let mut db = RootDatabaseImpl::new();
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        let sdbl_hirs_wrapped = db.sdbl_hir_in_file(file_id);

        let code_unwrapped = r#"Текст = "ВЫБРАТЬ Отпуска.Работник Сотрудник, Отпуска.Дней КАК Дней ИЗ Документ.Отпуск КАК Отпуска";"#;

        let fixture_text = format!("//- /test.bsl\n{}", code_unwrapped);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have at least one file");

        let mut db = RootDatabaseImpl::new();
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        let sdbl_hirs_unwrapped = db.sdbl_hir_in_file(file_id);

        assert!(!sdbl_hirs_wrapped.is_empty() || !sdbl_hirs_unwrapped.is_empty());
    }

    #[test]
    fn test_union_with_diagnostics() {
        let query = r#"ВЫБРАТЬ
	Взносы.Фонд Получатель,
	Взносы.Ставка КАК Процент,
	Взносы.Предел
ИЗ
	РегистрСведений.СтавкиВзносов КАК Взносы

ОБЪЕДИНИТЬ ВСЕ

ВЫБРАТЬ
	Льготы.Фонд,
	Льготы.Ставка,
	Льготы.Предел
ИЗ
	РегистрСведений.ЛьготныеСтавки КАК Льготы"#;

        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 2:2..2:24
              message: Поле 'Получатель' должно иметь явный псевдоним с ключевым словом AS/КАК
              severity: Warning
            AssignAliasFieldsInQuery @ 4:2..4:15
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_top_clause_with_explicit_alias() {
        let query = r#"ВЫБРАТЬ ПЕРВЫЕ 25
Анкеты.Кандидат КАК Кандидат
ИЗ
Документ.Анкета КАК Анкеты"#;

        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_top_clause_parsing() {
        let query = r#"ВЫБРАТЬ ПЕРВЫЕ 7
Вакансии.Позиция КАК Позиция,
Вакансии.Оклад КАК Оклад
ИЗ
Справочник.Вакансии КАК Вакансии"#;

        let parse = parser::parse_sdbl(query);
        assert!(!parse.has_errors(), "Parse should not have errors");

        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_top_clause_without_alias() {
        let query = r#"ВЫБРАТЬ ПЕРВЫЕ 25
Анкеты.Кандидат
ИЗ
Документ.Анкета КАК Анкеты"#;

        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 2:1..2:16
              message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_top_clause_implicit_alias() {
        let query = r#"ВЫБРАТЬ ПЕРВЫЕ 25
Анкеты.Кандидат Соискатель
ИЗ
Документ.Анкета КАК Анкеты"#;

        check_standalone_query_snapshot(
            query,
            expect![[r#"
            AssignAliasFieldsInQuery @ 2:1..2:27
              message: Поле 'Соискатель' должно иметь явный псевдоним с ключевым словом AS/КАК
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_distinct_clause() {
        let query = "SELECT DISTINCT Genre AS Kind FROM Library";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_distinct_top_combination() {
        let query = "ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 3 Город КАК Г ИЗ Филиалы";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_top_distinct_order() {
        let query = "SELECT TOP 5 DISTINCT Genre AS G FROM Library";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_query_with_union_two_diagnostics() {
        let code = r#"Отчёт = Новый Запрос;
Отчёт.Текст =
	"ВЫБРАТЬ
	|	Табель.Работник Сотрудник,
	|	Табель.Часы КАК Часы,
	|	Табель.Месяц
	|ИЗ
	|	РегистрНакопления.Табель КАК Табель
	|
	|ОБЪЕДИНИТЬ ВСЕ
	|
	|ВЫБРАТЬ
	|	Подработка.Работник,
	|	Подработка.Часы,
	|	Подработка.Месяц
	|ИЗ
	|	РегистрНакопления.Подработка КАК Подработка";"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#"
                AssignAliasFieldsInQuery @ 4:4..4:29
                  message: Поле 'Сотрудник' должно иметь явный псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 6:4..6:16
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_second_query_with_union_two_diagnostics() {
        let code = r#"Премии = Новый Запрос;
Премии.Текст =
	"ВЫБРАТЬ
	|	Начисления.Отдел,
	|	Начисления.Сумма КАК Сумма,
	|	Начисления.Повод Основание
	|ИЗ
	|	Документ.Премия КАК Начисления
	|
	|ОБЪЕДИНИТЬ ВСЕ
	|
	|ВЫБРАТЬ
	|	Разовые.Отдел,
	|	Разовые.Сумма,
	|	Разовые.Повод
	|ИЗ
	|	Документ.РазоваяВыплата КАК Разовые";

Удержания = Новый Запрос;
Удержания.Текст =
	"ВЫБРАТЬ
	|	Штрафы.Работник Нарушитель,
	|	Штрафы.Размер,
	|	Штрафы.Дата КАК Дата
	|ИЗ
	|	Документ.Взыскание КАК Штрафы
	|
	|ОБЪЕДИНИТЬ
	|
	|ВЫБРАТЬ
	|	Займы.Работник,
	|	Займы.Размер,
	|	Займы.Дата
	|ИЗ
	|	Документ.ВозвратЗайма КАК Займы";"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#"
                AssignAliasFieldsInQuery @ 4:4..4:20
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 6:4..6:30
                  message: Поле 'Основание' должно иметь явный псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 22:4..22:30
                  message: Поле 'Нарушитель' должно иметь явный псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 23:4..23:17
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_nested_subquery_field_without_alias() {
        let code = r#"Сводка = Новый Запрос;
Сводка.Текст =
	"ВЫБРАТЬ
	|	Итог.Отдел КАК Отдел,
	|	Итог.Людей КАК Людей
	|ИЗ
	|	(ВЫБРАТЬ
	|		Кадры.Отдел КАК Отдел,
	|		КОЛИЧЕСТВО(Кадры.Работник) КАК Людей,
	|		Кадры.Город
	|	ИЗ
	|		РегистрСведений.Кадры КАК Кадры
	|	СГРУППИРОВАТЬ ПО
	|		Кадры.Отдел, Кадры.Город) КАК Итог";"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#"
                AssignAliasFieldsInQuery @ 10:5..10:16
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_union_part_does_not_emit_when_alias_missing() {
        let query = "SELECT Title AS Title FROM Library UNION ALL SELECT Caption FROM Archive";
        check_standalone_query_snapshot(query, expect![[r#""#]]);
    }

    #[test]
    fn test_union_part_uses_first_query_aliases_regression() {
        let code = r#"Обучение = Новый Запрос;
Обучение.Текст =
	"ВЫБРАТЬ
	|	Курсы.Ссылка КАК Программа,
	|	Курсы.Преподаватель КАК Ведущий
	|ПОМЕСТИТЬ ВТ_Занятия
	|ИЗ
	|	Справочник.ПланОбучения.ОчныеКурсы КАК Курсы
	|
	|ОБЪЕДИНИТЬ ВСЕ
	|
	|ВЫБРАТЬ
	|	Вебинары.Ссылка,
	|	Вебинары.Преподаватель
	|ИЗ
	|	Справочник.ПланОбучения.ДистанционныеКурсы КАК Вебинары";"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_query_with_leading_newline_field_without_alias() {
        let code = "Текст = \"\n\t|ВЫБРАТЬ\n\t|\tВТ_Смены.Работник\n\t|ИЗ\n\t|\t&ВТ_Смены КАК ВТ_Смены\n\t|;\n\t|\n\t|ВЫБРАТЬ\n\t|\t\" + КолонкиГрафика + \"\n\t|ИЗ\n\t|\t&ВТ_График КАК График\";";

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#"
                AssignAliasFieldsInQuery @ 3:4..3:21
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning"#]],
        );
    }

    #[test]
    fn track3_function_aggregate_and_case_fields_require_explicit_aliases_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура ПосчитатьОтпускные()
    Расчёт = Новый Запрос;
    Расчёт.Текст =
        "ВЫБРАТЬ
        |   МАКСИМУМ(Отпуска.Дней),
        |   ЕСТЬNULL(Отпуска.Замещающий, НЕОПРЕДЕЛЕНО),
        |   ВЫБОР
        |       КОГДА Отпуска.Дней > 14 ТОГДА ""Основной""
        |       ИНАЧЕ ""Дробный""
        |   КОНЕЦ
        |ИЗ
        |   Документ.Отпуск КАК Отпуска";
КонецПроцедуры"#,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#"
                AssignAliasFieldsInQuery @ 5:13..5:35
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 6:13..6:55
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning
                AssignAliasFieldsInQuery @ 7:13..10:18
                  message: Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК
                  severity: Warning"#]],
        );
    }

    #[test]
    fn track3_split_concatenated_query_is_not_reconstructed_snapshot() {
        check_diagnostics_snapshot_for(
            r#"Процедура СобратьВыборку()
    Выборка = Новый Запрос;
    Выборка.Текст =
        "ВЫБРАТЬ
        |   " + СписокКолонок + "
        |ИЗ
        |   Справочник.Сотрудники КАК Работники";
КонецПроцедуры"#,
            DiagnosticCode::AssignAliasFieldsInQuery,
            expect![[r#""#]],
        );
    }
}
