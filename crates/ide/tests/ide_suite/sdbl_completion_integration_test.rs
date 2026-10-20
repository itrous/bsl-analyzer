use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn create_analysis(code: &str) -> (Analysis, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{}", code));

    let mut db = RootDatabaseImpl::default();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }

    let source_root = SourceRoot::new_local(file_set);

    db.set_source_root(source_root_id, source_root);

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

    let file_id = fixture.first_file().expect("No files in fixture");

    (Analysis::from_database(db), file_id)
}

#[test]
fn test_completion_for_nested_union_with_into_clause() {
    let code = r#"Функция ТестЗапрос()
    Запрос = Новый Запрос;
    Запрос.Текст =
    "ВЫБРАТЬ
    |   Вложенный.Поле1 КАК Поле1,
    |   Вложенный.Поле2 КАК Поле2
    |ПОМЕСТИТЬ ВТ_Результат
    |ИЗ
    |   (ВЫБРАТЬ
    |       Т1.Наименование КАК Поле1,
    |       Т1.Код КАК Поле2
    |   ИЗ
    |       Справочник.Номенклатура КАК Т1
    |
    |   ОБЪЕДИНИТЬ ВСЕ
    |
    |   ВЫБРАТЬ
    |       Т2.Наименование,
    |       Т2.Код
    |   ИЗ
    |       Справочник.Контрагенты КАК Т2) КАК Вложенный
    |;
    |
    |////////////////////////////////////////////////////////////////////////////////
    |ВЫБРАТЬ
    |   ВТ_Результат.Поле1 КАК Поле1,
    |   ВТ_Результат.Поле2 КАК Поле2
    |ИЗ
    |   ВТ_Результат КАК ВТ_Результат";

    Возврат Запрос.Выполнить();
КонецФункции"#;

    let (analysis, file_id) = create_analysis(code);

    let cursor_offset_1 = code.find("Вложенный.Поле1").expect("Should find 'Вложенный.Поле1'");
    let cursor_offset_1 = cursor_offset_1 + "Вложенный.".len();

    let completions_1 =
        analysis.completions(file_id, cursor_offset_1.try_into().unwrap(), None, ide::Locale::Ru);

    assert!(
        completions_1.iter().any(|c| c.label == "Поле1"),
        "Should complete 'Поле1' from subquery. Got completions: {:?}",
        completions_1.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
    assert!(
        completions_1.iter().any(|c| c.label == "Поле2"),
        "Should complete 'Поле2' from subquery. Got completions: {:?}",
        completions_1.iter().map(|c| &c.label).collect::<Vec<_>>()
    );

    let cursor_offset_2 =
        code.rfind("ВТ_Результат.Поле1").expect("Should find second 'ВТ_Результат.Поле1'");
    let cursor_offset_2 = cursor_offset_2 + "ВТ_Результат.".len();

    let completions_2 =
        analysis.completions(file_id, cursor_offset_2.try_into().unwrap(), None, ide::Locale::Ru);

    assert!(
        completions_2.iter().any(|c| c.label == "Поле1"),
        "Should complete 'Поле1' from temp table. Got completions: {:?}",
        completions_2.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
    assert!(
        completions_2.iter().any(|c| c.label == "Поле2"),
        "Should complete 'Поле2' from temp table. Got completions: {:?}",
        completions_2.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[test]
fn test_completion_after_dot_followed_by_kak_alias() {
    let code = r#"Функция ТестЗапрос()
    Запрос = Новый Запрос;
    Запрос.Текст =
    "ВЫБРАТЬ
    |   Внутренний. КАК Ссылка
    |ИЗ
    |   (ВЫБРАТЬ
    |       Т.Наименование КАК Поле1,
    |       Т.Код КАК Поле2
    |   ИЗ
    |       Справочник.Номенклатура КАК Т) КАК Внутренний";
    Возврат Запрос.Выполнить();
КонецФункции"#;

    let (analysis, file_id) = create_analysis(code);

    let dot_pos =
        code.find("Внутренний. КАК").expect("must find dot-then-КАК site") + "Внутренний.".len();

    let completions = analysis.completions(file_id, dot_pos as u32, None, ide::Locale::Ru);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(
        labels.contains(&"Поле1"),
        "trailing-dot before КАК alias must surface subquery field `Поле1`; got: {labels:?}"
    );
    assert!(
        labels.contains(&"Поле2"),
        "trailing-dot before КАК alias must surface subquery field `Поле2`; got: {labels:?}"
    );
}
