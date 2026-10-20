use bsl_platform::PlatformDataInner;
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }

    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;

    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file = (abs_idx - file_offset_in_prefix) as u32;

    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn has_platform_data() -> bool {
    !PlatformDataInner::instance().all_methods().is_empty()
}

fn assert_hover_contains(fixture: &str, expected_substring: &str) {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available (HBK not present at build time)");
        return;
    }
    let (analysis, file_id, offset) = setup(fixture);
    let result = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("hover must resolve when platform data is present");
    assert!(
        result.markup.contains(expected_substring),
        "hover markup did not contain `{}`; got:\n{}",
        expected_substring,
        result.markup,
    );
}

#[test]
fn hover_record_set_read_resolves_via_composite_prefix() {
    assert_hover_contains(
        r#"//- /test.bsl
Процедура Тест()
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.Проч$0итать();
КонецПроцедуры
"#,
        "Прочитать",
    );
}

#[test]
fn hover_record_set_write_resolves_via_composite_prefix() {
    assert_hover_contains(
        r#"//- /test.bsl
Процедура Тест()
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.Запис$0ать();
КонецПроцедуры
"#,
        "Записать",
    );
}

#[test]
fn hover_record_set_load_resolves_via_composite_prefix() {
    assert_hover_contains(
        r#"//- /test.bsl
Процедура Тест()
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.Загр$0узить(Неопределено);
КонецПроцедуры
"#,
        "Загрузить",
    );
}

#[test]
fn hover_record_manager_read_resolves_via_composite_prefix() {
    assert_hover_contains(
        r#"//- /test.bsl
Процедура Тест()
    Менеджер = РегистрыСведений.РегистрСведений1.СоздатьМенеджерЗаписи();
    Менеджер.Проч$0итать();
КонецПроцедуры
"#,
        "Прочитать",
    );
}

#[test]
fn hover_record_set_english_method_name_resolves() {
    assert_hover_contains(
        r#"//- /test.bsl
Процедура Тест()
    Набор = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    Набор.Re$0ad();
КонецПроцедуры
"#,
        "Read",
    );
}
