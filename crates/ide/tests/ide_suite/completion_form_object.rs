use bsl_platform::PlatformDataInner;
use ide::{Analysis, CompletionItem};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn data_processor_module_path() -> PathBuf {
    designer_fixture_path().join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl")
}

fn report_module_path() -> PathBuf {
    designer_fixture_path().join("Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl")
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

fn label(item: &CompletionItem) -> &str {
    item.label.as_str()
}

fn contains_label(items: &[CompletionItem], expected: &str) -> bool {
    items.iter().any(|i| label(i).eq_ignore_ascii_case(expected))
}

#[test]
fn completion_on_object_dot_in_data_processor_form_lists_attributes() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Х = Объект.;\nКонецПроцедуры\n";
    let cursor = bsl.find("Объект.").expect("anchor") as u32 + "Объект.".len() as u32;

    let (analysis, file_id, offset) = setup_form_module(data_processor_module_path(), bsl, cursor);
    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);

    assert!(
        contains_label(&items, "АдресСайта"),
        "DataProcessor attribute `АдресСайта` must surface; got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
    assert!(
        contains_label(&items, "СоздаватьГруппы"),
        "DataProcessor attribute `СоздаватьГруппы` must surface; got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_on_object_dot_in_data_processor_form_hides_object_methods() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Объект.;\nКонецПроцедуры\n";
    let cursor = bsl.find("Объект.").expect("anchor") as u32 + "Объект.".len() as u32;

    let (analysis, file_id, offset) = setup_form_module(data_processor_module_path(), bsl, cursor);
    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);

    assert!(
        !contains_label(&items, "Записать"),
        "DataProcessor's `Записать()` must NOT leak through the FormData wrapper; \
         got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_after_findrows_index_lists_row_columns() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест(Отбор)\n    Х = Объект.НастройкиЭксель.НайтиСтроки(Отбор)[0].;\nКонецПроцедуры\n";
    let cursor = bsl.find("[0].").expect("anchor") as u32 + "[0].".len() as u32;

    let (analysis, file_id, offset) = setup_form_module(data_processor_module_path(), bsl, cursor);
    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);

    assert!(
        contains_label(&items, "Значение"),
        "row column `Значение` must surface; got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
    assert!(
        contains_label(&items, "Активна"),
        "row column `Активна` must surface; got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_on_object_dot_in_report_form_lists_attributes() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Х = Объект.;\nКонецПроцедуры\n";
    let cursor = bsl.find("Объект.").expect("anchor") as u32 + "Объект.".len() as u32;

    let (analysis, file_id, offset) = setup_form_module(report_module_path(), bsl, cursor);
    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);

    assert!(
        contains_label(&items, "ПериодОтчёта"),
        "Report attribute `ПериодОтчёта` must surface; got: {:?}",
        items.iter().map(label).collect::<Vec<_>>()
    );
}
