use bsl_platform::PlatformDataInner;
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup_form_module(disk_path: PathBuf, bsl: &str, cursor: u32) -> (Analysis, FileId, u32) {
    assert!(disk_path.exists(), "fixture missing: {}", disk_path.display());
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(disk_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (Analysis::from_database(db), file_id, cursor)
}

fn has_platform_data() -> bool {
    !PlatformDataInner::instance().all_methods().is_empty()
}

#[test]
fn hover_on_data_processor_object_attribute_renders_type() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Х = Объект.СоздаватьГруппы;\nКонецПроцедуры\n";
    let cursor = bsl.find("СоздаватьГруппы").expect("anchor") as u32;

    let (analysis, file_id, offset) = setup_form_module(
        designer_fixture_path()
            .join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl"),
        bsl,
        cursor,
    );
    let result = analysis.hover(file_id, offset, ide::Locale::Ru).expect("hover must resolve");

    assert!(
        result.markup.contains("Булево"),
        "hover must render `Булево` for `Объект.СоздаватьГруппы`; got:\n{}",
        result.markup
    );
}

#[test]
fn hover_after_findrows_index_renders_column_type() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест(Отбор)\n    Х = Объект.НастройкиЭксель.НайтиСтроки(Отбор)[0].Значение;\nКонецПроцедуры\n";
    let cursor = bsl.rfind("Значение").expect("anchor") as u32;

    let (analysis, file_id, offset) = setup_form_module(
        designer_fixture_path()
            .join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl"),
        bsl,
        cursor,
    );
    let result = analysis.hover(file_id, offset, ide::Locale::Ru).expect("hover must resolve");

    assert!(
        result.markup.contains("**Тип:** Строка")
            || result.markup.contains("**Type:** Строка")
            || result.markup.contains("Type: Строка")
            || result.markup.contains("Тип: Строка"),
        "hover must render `Строка` as the column type for `[0].Значение`; got:\n{}",
        result.markup
    );
}

#[test]
fn hover_on_report_object_attribute_renders_type() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Х = Объект.ПериодОтчёта;\nКонецПроцедуры\n";
    let cursor = bsl.find("ПериодОтчёта").expect("anchor") as u32;

    let (analysis, file_id, offset) = setup_form_module(
        designer_fixture_path().join("Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl"),
        bsl,
        cursor,
    );
    let result = analysis.hover(file_id, offset, ide::Locale::Ru).expect("hover must resolve");

    assert!(
        result.markup.contains("Дата") || result.markup.contains("Date"),
        "hover must render `Дата` for `Объект.ПериодОтчёта`; got:\n{}",
        result.markup
    );
}
