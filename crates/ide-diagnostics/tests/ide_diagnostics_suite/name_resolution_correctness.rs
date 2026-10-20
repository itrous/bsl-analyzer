use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::ConfigurationPathInput;
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

const DESIGNER_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

fn diagnostics_for(path: &str, source: &str, configuration_root: Option<&str>) -> Vec<Diagnostic> {
    let fixture = Fixture::parse(&format!("//- {path}\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }
    // The fixture exercises proof of absence. That is intentionally withheld
    // unless the project has a complete load and a target covered by the catalog.
    db.set_target_platform_version(Some("8.3.27".into()));

    let file_id = *fixture.files.keys().next().expect("fixture has one file");
    let config = DiagnosticsConfig::all_enabled();
    let configuration =
        configuration_root.map(|root| ConfigurationPathInput::new(&db, root.to_string(), 0));
    let provider = SalsaProvider::new(&db, configuration);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);
    ide_diagnostics::diagnostics(&ctx)
}

fn diagnostics_for_cfe_module(source: &str) -> Vec<Diagnostic> {
    fn visit(directory: &std::path::Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("read CFE fixture directory") {
            let path = entry.expect("read CFE fixture entry").path();
            if path.is_dir() {
                visit(&path, files);
            } else {
                files.push(path);
            }
        }
    }

    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module("Caller", source);
    let fixture = builder.build();
    let mut paths = Vec::new();
    visit(fixture.root(), &mut paths);

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let mut test_file = None;
    let mut file_ids = Vec::new();
    for (raw_id, path) in paths.into_iter().enumerate() {
        let file_id = FileId(raw_id as u32);
        let text = std::fs::read_to_string(&path).expect("read CFE fixture file");
        file_set.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
        db.set_file_text(file_id, &text);
        file_ids.push(file_id);
        if path.ends_with("CommonModules/Caller/Module.bsl") {
            test_file = Some(file_id);
        }
    }
    let test_file = test_file.expect("CFE caller module exists");
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in file_ids {
        db.set_file_source_root(file_id, SourceRootId(0));
    }
    db.set_target_platform_version(Some("8.3.27".into()));

    let provider = SalsaProvider::new(&db, None);
    let config = DiagnosticsConfig::all_enabled();
    let ctx = DiagnosticsContext::new(&config, test_file, &provider);
    ide_diagnostics::diagnostics(&ctx)
}

#[test]
fn unresolved_global_names_are_semantic_and_do_not_reject_local_names() {
    let source = r#"Процедура Тест(ЛокальноеИмя)
    Значение = ЧастиЖурналаУчетаСчетовФактур.ВыставленныеСчетаФактуры;
    Значение = Повтор("0", 64);
    Значение = ЛокальноеИмя;
КонецПроцедуры"#;
    let diagnostics = diagnostics_for_cfe_module(source);
    let unresolved = diagnostics
        .iter()
        .filter(|diag| diag.code == DiagnosticCode::UnresolvedName)
        .collect::<Vec<_>>();

    for name in ["ЧастиЖурналаУчетаСчетовФактур", "Повтор"] {
        assert!(
            unresolved.iter().any(|diag| {
                let start = usize::from(diag.range.start());
                let end = usize::from(diag.range.end());
                source[start..end].eq_ignore_ascii_case(name)
            }),
            "expected unresolved-name diagnostic on {name}: {diagnostics:?}"
        );
    }
    assert!(
        diagnostics.iter().all(|diag| diag.code != DiagnosticCode::ParseError),
        "unknown names are semantic, not syntax errors: {diagnostics:?}"
    );
    assert!(
        unresolved.iter().all(|diag| {
            let start = usize::from(diag.range.start());
            let end = usize::from(diag.range.end());
            source[start..end] != *"ЛокальноеИмя"
        }),
        "declared local names must resolve: {unresolved:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn form_function_conflicts_with_platform_global_function() {
    let form_module = PathBuf::from(DESIGNER_FIXTURE)
        .join("Catalogs/Справочник1/Forms/ФормаСписка/Ext/Form/Module.bsl");
    let source = "Функция ПредставлениеПериода(Начало, Конец)\n    Возврат Начало;\nКонецФункции";
    let diagnostics =
        diagnostics_for(&form_module.to_string_lossy(), source, Some(DESIGNER_FIXTURE));
    let conflicts = diagnostics
        .iter()
        .filter(|diag| diag.code == DiagnosticCode::GlobalContextMethodConflict)
        .collect::<Vec<_>>();

    assert_eq!(conflicts.len(), 1, "expected the form/global collision: {diagnostics:?}");
    let range = conflicts[0].range;
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    assert_eq!(&source[start..end], "ПредставлениеПериода");
}
