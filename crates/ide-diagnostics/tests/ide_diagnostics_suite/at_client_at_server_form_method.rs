//! `AtClientAtServerFormMethod` through the public pipeline (`file_diagnostics`) on the
//! designer fixture, so the module kind and the form kind come from real metadata.
//! The finding sits on the declaration whether or not the method is called, and never
//! on a call site. The editor shows a warning; the diagnostic class stays an error,
//! the same split `ServerSideExportFormMethod` and `UnavailableInEnvironment` use.

use std::path::PathBuf;

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{
    Diagnostic, DiagnosticCode, DiagnosticSeverityLevel, DiagnosticType, DiagnosticsConfig,
    Severity,
};
use vfs::{FileId, FileSet, VfsPath};

const MANAGED_FORM: &str = "DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl";
const ORDINARY_FORM: &str = "Catalogs/Справочник1/Forms/ФормаОбычная/Ext/Form/Module.bsl";
const COMMON_ORDINARY_FORM: &str = "CommonForms/ТестоваяФорма/Ext/Form/Module.bsl";
const COMMAND: &str = "Catalogs/Справочник1/Commands/Команда1/Ext/CommandModule.bsl";

fn designer_fixture() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn diagnostics_at(relative: &str, source: &str) -> Vec<Diagnostic> {
    let path = designer_fixture().join(relative);
    assert!(path.exists(), "fixture missing: {}", path.display());
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_all_config_paths(vec![(None, designer_fixture())], None);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
}

fn flagged(relative: &str, source: &str) -> Vec<Diagnostic> {
    diagnostics_at(relative, source)
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::AtClientAtServerFormMethod)
        .collect()
}

const CALLED: &str = "&НаСервере\nПроцедура Пересчитать()\n    Общая();\nКонецПроцедуры\n\n\
                      &НаКлиентеНаСервере\nПроцедура Общая()\nКонецПроцедуры\n";

const NOT_CALLED: &str =
    "&НаКлиентеНаСервере\nФункция Сумма(А, Б)\n    Возврат А + Б;\nКонецФункции\n";

fn declaration_name_offset(source: &str, header: &str, name: &str) -> u32 {
    let header_at = source.find(header).expect("declaration header");
    u32::try_from(header_at + header.len() - name.len()).expect("offset")
}

#[test]
fn called_method_is_flagged_on_its_declaration_only() {
    let found = flagged(MANAGED_FORM, CALLED);
    assert_eq!(found.len(), 1);
    assert_eq!(&CALLED[found[0].range], "Общая");
    assert_eq!(
        u32::from(found[0].range.start()),
        declaration_name_offset(CALLED, "Процедура Общая", "Общая")
    );
    assert_eq!(found[0].severity, Severity::Warning);
}

#[test]
fn uncalled_method_is_flagged_on_its_declaration() {
    let found = flagged(MANAGED_FORM, NOT_CALLED);
    assert_eq!(found.len(), 1);
    assert_eq!(&NOT_CALLED[found[0].range], "Сумма");
    assert_eq!(
        u32::from(found[0].range.start()),
        declaration_name_offset(NOT_CALLED, "Функция Сумма", "Сумма")
    );
    assert_eq!(found[0].severity, Severity::Warning);
}

#[test]
fn english_directive_is_flagged() {
    let source = "&AtClientAtServer\nProcedure Refresh()\nEndProcedure\n";
    let found = flagged(MANAGED_FORM, source);
    assert_eq!(found.len(), 1);
    assert_eq!(&source[found[0].range], "Refresh");
}

#[test]
fn no_context_variant_is_not_flagged_even_when_called() {
    for directive in ["&НаКлиентеНаСервереБезКонтекста\n", "&AtClientAtServerNoContext\n"]
    {
        let source = CALLED.replace("&НаКлиентеНаСервере\n", directive);
        assert!(flagged(MANAGED_FORM, &source).is_empty(), "{directive}");
    }
}

#[test]
fn ordinary_forms_and_command_modules_are_not_checked() {
    for relative in [ORDINARY_FORM, COMMON_ORDINARY_FORM, COMMAND] {
        assert!(flagged(relative, CALLED).is_empty(), "{relative}");
        assert!(flagged(relative, NOT_CALLED).is_empty(), "{relative}");
    }
}

#[test]
fn an_export_is_not_also_a_server_side_export() {
    let source = "&НаКлиентеНаСервере\nПроцедура Общая() Экспорт\nКонецПроцедуры\n";
    let found = diagnostics_at(MANAGED_FORM, source);
    assert_eq!(
        found.iter().filter(|d| d.code == DiagnosticCode::AtClientAtServerFormMethod).count(),
        1
    );
    assert!(
        found.iter().all(|d| d.code != DiagnosticCode::ServerSideExportFormMethod),
        "the same declaration must not be reported twice: {found:?}"
    );
}

#[test]
fn editor_warning_keeps_the_error_class_of_the_neighbors() {
    let meta = ide_diagnostics::get_metadata(DiagnosticCode::AtClientAtServerFormMethod).unwrap();
    assert_eq!(meta.diagnostic_type, DiagnosticType::Error);
    assert_eq!(meta.severity, DiagnosticSeverityLevel::Critical);
    assert_eq!(meta.calculate_severity(), Severity::Critical);
    assert_eq!(meta.effective_severity(), Severity::Warning);
    assert_eq!(meta.lsp_severity_override, "warning");

    for code in
        [DiagnosticCode::ServerSideExportFormMethod, DiagnosticCode::UnavailableInEnvironment]
    {
        let neighbor = ide_diagnostics::get_metadata(code).unwrap();
        assert_eq!(neighbor.diagnostic_type, DiagnosticType::Error, "{code:?}");
        assert_eq!(neighbor.lsp_severity_override, "warning", "{code:?}");
        assert_eq!(neighbor.effective_severity(), Severity::Warning, "{code:?}");
        assert_ne!(neighbor.calculate_severity(), Severity::Warning, "{code:?}");
    }
}
