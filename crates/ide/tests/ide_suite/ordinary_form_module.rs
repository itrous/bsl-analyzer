//! A form module whose form is ORDINARY (`<FormType>Ordinary</FormType>` in its
//! descriptor, a binary `Ext/Form.bin` dialog, no `Ext/Form.xml`) is thick-client code
//! of the ordinary application. Managed-form rules do not apply there.

use hir::{DefDatabase, ModuleId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn ordinary_form_module() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Forms/ФормаОбычная/Ext/Form/Module.bsl")
}

fn managed_form_module() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Forms/ФормаСписка/Ext/Form/Module.bsl")
}

fn setup(disk_path: PathBuf, bsl: &str) -> (RootDatabaseImpl, FileId) {
    assert!(disk_path.exists(), "fixture missing: {}", disk_path.display());
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(disk_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

/// `(code, line)` of every diagnostic, lines 0-based.
fn findings(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(DiagnosticCode, u32)> {
    ide_diagnostics::file_diagnostics(db, file_id, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .map(|d| {
            let start = usize::from(d.range.start());
            let line = CODE[..start].matches('\n').count();
            (d.code, u32::try_from(line).unwrap())
        })
        .collect()
}

fn lines_of(found: &[(DiagnosticCode, u32)], code: DiagnosticCode) -> Vec<u32> {
    found.iter().filter(|(c, _)| *c == code).map(|(_, line)| *line).collect()
}

/// The same text in both forms: directive-less handlers, `ЭтаФорма`, `ПолучитьФорму`,
/// a modal and a synchronous call. Each of these ran in an ordinary form opened in the
/// thick client of 8.3.17.1549 and 8.3.27.2214, in a configuration whose modality and
/// synchronous-call modes are both "do not use" - as in this fixture.
const CODE: &str = "\
Процедура ПередОткрытием(Отказ, СтандартнаяОбработка)
	Заголовок = ЭтаФорма.Заголовок;
	Форма = ПолучитьФорму(\"ФормаВыбора\");
	Предупреждение(\"Проба\", 1);
	ПодключитьРасширениеРаботыСФайлами();
КонецПроцедуры

Процедура НеПривязана(Параметр)
КонецПроцедуры
";

#[test]
fn ordinary_form_module_is_not_held_to_managed_form_rules() {
    let (db, file_id) = setup(ordinary_form_module(), CODE);

    let metadata = db.module_metadata(ModuleId::new(file_id));
    assert!(metadata.is_ordinary_form_module(), "the descriptor says Ordinary");
    let form = metadata.form.as_ref().expect("form metadata from the descriptor");
    assert_eq!(form.name(), "ФормаОбычная");
    assert!(form.is_handler("ПередОткрытием"));
    assert!(!form.is_handler("НеПривязана"));

    let found = findings(&db, file_id);
    for code in [
        DiagnosticCode::CompilationDirectiveLost,
        DiagnosticCode::UsingThisForm,
        DiagnosticCode::GetFormMethod,
        DiagnosticCode::UsingModalWindows,
        DiagnosticCode::UsingSynchronousCalls,
        DiagnosticCode::UnavailableInEnvironment,
    ] {
        assert!(lines_of(&found, code).is_empty(), "{code:?} in an ordinary form: {found:?}");
    }
    // A procedure named after a form event counts as bound, with the event's signature.
    assert!(!lines_of(&found, DiagnosticCode::UnusedLocalMethod).contains(&0), "{found:?}");
    assert!(!lines_of(&found, DiagnosticCode::UnusedParameters).contains(&0), "{found:?}");
    // Any other procedure is still checked: the default names cover events only.
    assert_eq!(lines_of(&found, DiagnosticCode::UnusedLocalMethod), vec![7], "{found:?}");
}

#[test]
fn managed_form_module_keeps_reporting_the_same_code() {
    let (db, file_id) = setup(managed_form_module(), CODE);

    let metadata = db.module_metadata(ModuleId::new(file_id));
    assert!(!metadata.is_ordinary_form_module());
    assert!(metadata.form.as_ref().is_some_and(|form| form.is_managed()));

    let found = findings(&db, file_id);
    assert_eq!(lines_of(&found, DiagnosticCode::CompilationDirectiveLost), vec![0, 7]);
    assert_eq!(lines_of(&found, DiagnosticCode::UsingThisForm), vec![1]);
    assert_eq!(lines_of(&found, DiagnosticCode::GetFormMethod), vec![2]);
    assert_eq!(lines_of(&found, DiagnosticCode::UsingModalWindows), vec![3]);
    // `Предупреждение` is both a modal and a synchronous call.
    assert_eq!(lines_of(&found, DiagnosticCode::UsingSynchronousCalls), vec![3, 4]);
    // `ПередОткрытием` is no event of a managed form: there it is an unbound procedure.
    assert_eq!(lines_of(&found, DiagnosticCode::UnusedLocalMethod), vec![0, 7], "{found:?}");
    assert!(lines_of(&found, DiagnosticCode::UnusedParameters).contains(&0), "{found:?}");
}

/// An ordinary form opens in the thick client of the managed application too, when the
/// configuration allows ordinary forms there. Without a project that setting is not
/// known, so both thick clients count: a call no thick client can make is still
/// reported.
#[test]
fn ordinary_form_module_is_checked_for_the_managed_thick_client() {
    const CALL: &str = "\
Процедура ПередОткрытием(Отказ, СтандартнаяОбработка)
	Доступен = ОсновнойСерверДоступен();
КонецПроцедуры
";
    let (db, file_id) = setup(ordinary_form_module(), CALL);
    assert!(db.module_metadata(ModuleId::new(file_id)).is_ordinary_form_module());

    let unavailable: Vec<String> =
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .map(|d| d.message)
            .collect();
    assert_eq!(unavailable.len(), 1, "{unavailable:?}");
    assert!(unavailable[0].contains("ОсновнойСерверДоступен"), "{unavailable:?}");
}
