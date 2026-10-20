use bsl_platform::PlatformDataInner;
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn module_disk_path() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl")
}

fn setup_form_module(bsl_text: &str, cursor_offset: u32) -> (Analysis, FileId, u32) {
    let disk_path = module_disk_path();
    assert!(disk_path.exists(), "designer fixture Module.bsl missing at {}", disk_path.display());

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    let vfs_path = VfsPath::new(disk_path.to_string_lossy().as_ref());
    file_set.insert(file_id, vfs_path);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl_text);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    (Analysis::from_database(db), file_id, cursor_offset)
}

fn has_platform_data() -> bool {
    !PlatformDataInner::instance().all_methods().is_empty()
}

#[test]
fn hover_on_form_input_field_name_renders_kind_label() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available (HBK not present)");
        return;
    }

    let bsl = "Процедура Тест()\n    Х = Элементы.Код;\nКонецПроцедуры\n";
    let cursor_offset = bsl.find("Код;").expect("cursor anchor present") as u32;

    let (analysis, file_id, offset) = setup_form_module(bsl, cursor_offset);
    let result = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("hover must resolve via the Phase-13.1 type_of_expr fallback");

    assert!(
        result.markup.contains("**Код**"),
        "hover must surface the form-element name as a heading; got:\n{}",
        result.markup
    );
    assert!(
        result.markup.contains("ПолеФормы"),
        "hover must render the kind display label `ПолеФормы`; got:\n{}",
        result.markup
    );
}
