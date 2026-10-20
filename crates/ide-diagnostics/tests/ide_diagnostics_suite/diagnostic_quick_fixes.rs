//! Quick fixes of four diagnostics, through the public pipeline (`file_diagnostics`):
//! the deprecated `Найти` and `ТекущаяДата` calls, illegal characters, and a server
//! export of a managed form. Each case prints the fix label, its `fix_all` flag and
//! the source after the fix, or `no fix` when the diagnostic must stay without one.

use std::path::PathBuf;
use std::sync::Arc;

use hir::execution_env::{EnvFlags, EnvOptions};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, TextEdit};
use vfs::{FileId, FileSet, VfsPath};

#[derive(Default)]
struct Project<'a> {
    compatibility_mode: Option<&'a str>,
    min_platform_version: Option<&'a str>,
}

fn module_diagnostics(source: &str, project: Project<'_>) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    db.set_compatibility_mode(project.compatibility_mode.map(Arc::from));
    db.set_min_platform_version(project.min_platform_version.map(Arc::from));
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/CommonModules/Тест/Ext/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
}

fn designer_fixture() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// `source` as the module of a managed form of the designer fixture.
fn form_module_diagnostics(source: &str) -> Vec<Diagnostic> {
    form_module_diagnostics_with(source, EnvOptions::default())
}

fn form_module_diagnostics_with(source: &str, options: EnvOptions) -> Vec<Diagnostic> {
    let path =
        designer_fixture().join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl");
    assert!(path.exists(), "fixture missing: {}", path.display());
    let mut db = RootDatabaseImpl::new();
    db.set_env_options(options);
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_all_config_paths(vec![(None, designer_fixture())], None);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
}

/// `source` as a module of the designer fixture, so its metadata (form kind,
/// common-module flags) is the fixture's, not an empty stand-in.
fn fixture_module_diagnostics(relative: &str, source: &str) -> Vec<Diagnostic> {
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

fn apply(source: &str, edits: &[TextEdit]) -> String {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| edit.range.start());
    let mut out = String::new();
    let mut cursor = 0usize;
    for edit in sorted {
        out.push_str(&source[cursor..usize::from(edit.range.start())]);
        out.push_str(&edit.new_text);
        cursor = usize::from(edit.range.end());
    }
    out.push_str(&source[cursor..]);
    out
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

/// Every diagnostic of `code` with its fixes applied one at a time.
fn fixes_of(source: &str, diagnostics: &[Diagnostic], code: DiagnosticCode) -> String {
    let mut out = Vec::new();
    for diagnostic in diagnostics.iter().filter(|d| d.code == code) {
        let (line, col) = line_col(source, usize::from(diagnostic.range.start()));
        let head = format!("{} @ {line}:{col}", code.as_str());
        if diagnostic.fixes.is_empty() {
            out.push(format!("{head} no fix"));
        }
        for fix in &diagnostic.fixes {
            out.push(format!(
                "{head} {} [fix_all={}]\n{}",
                fix.label,
                fix.safe_for_fix_all,
                apply(source, &fix.edits)
            ));
        }
    }
    out.join("\n---\n")
}

// --- Найти -> СтрНайти -------------------------------------------------------------

const FIND_RU: &str = "Процедура Тест()\n    Позиция = Найти(\"Строка\", \"о\");\nКонецПроцедуры\n";

#[test]
fn find_is_renamed_to_strfind_as_a_safe_fix() {
    let diagnostics = module_diagnostics(FIND_RU, Project::default());
    assert_eq!(
        fixes_of(FIND_RU, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:15 Заменить на \"СтрНайти\" [fix_all=true]\n\
         Процедура Тест()\n    Позиция = СтрНайти(\"Строка\", \"о\");\nКонецПроцедуры\n"
    );
}

#[test]
fn english_find_becomes_strfind() {
    let source = "Procedure Test()\n    Position = FIND(\"String\", \"S\");\nEndProcedure\n";
    let diagnostics = module_diagnostics(source, Project::default());
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:16 Заменить на \"StrFind\" [fix_all=true]\n\
         Procedure Test()\n    Position = StrFind(\"String\", \"S\");\nEndProcedure\n"
    );
}

#[test]
fn find_has_no_fix_where_the_compatibility_mode_hides_strfind() {
    // Below mode 8.3.6 the platform does not compile `СтрНайти` at all.
    let project = Project { compatibility_mode: Some("Version8_3_5"), ..Project::default() };
    let diagnostics = module_diagnostics(FIND_RU, project);
    assert_eq!(
        fixes_of(FIND_RU, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:15 no fix"
    );
}

#[test]
fn find_keeps_the_fix_from_compatibility_mode_8_3_6() {
    let project = Project { compatibility_mode: Some("Version8_3_6"), ..Project::default() };
    let diagnostics = module_diagnostics(FIND_RU, project);
    let fixes = fixes_of(FIND_RU, &diagnostics, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("СтрНайти(\"Строка\", \"о\")"), "got: {fixes}");
}

#[test]
fn find_has_no_fix_below_the_project_minimum_platform() {
    let project = Project { min_platform_version: Some("8.3.5"), ..Project::default() };
    let diagnostics = module_diagnostics(FIND_RU, project);
    assert_eq!(
        fixes_of(FIND_RU, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:15 no fix"
    );
}

// --- ТекущаяДата -> ТекущаяДатаСеанса ------------------------------------------------

#[test]
fn current_date_fix_is_manual() {
    let source = "Процедура Тест()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let diagnostics = module_diagnostics(source, Project::default());
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:12 Заменить на \"ТекущаяДатаСеанса\" (время в часовом поясе сеанса) [fix_all=false]\n\
         Процедура Тест()\n    Дата = ТекущаяДатаСеанса();\nКонецПроцедуры\n"
    );
}

#[test]
fn english_current_date_becomes_current_session_date() {
    let source = "Procedure Test()\n    Date = CurrentDate();\nEndProcedure\n";
    let diagnostics = module_diagnostics(source, Project::default());
    let fixes = fixes_of(source, &diagnostics, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("[fix_all=false]"), "got: {fixes}");
    assert!(fixes.contains("Date = CurrentSessionDate();"), "got: {fixes}");
}

#[test]
fn current_date_has_no_fix_where_the_session_date_does_not_compile() {
    // `ТекущаяДатаСеанса` is absent on the thin and web clients. A client method,
    // and a client common module, must not be offered a rename that will not compile.
    let client_form =
        "&НаКлиенте\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let client_diags = form_module_diagnostics(client_form);
    assert_eq!(
        fixes_of(client_form, &client_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 3:12 no fix"
    );

    let client_module = "Процедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let module_diags = fixture_module_diagnostics(
        "CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl",
        client_module,
    );
    assert_eq!(
        fixes_of(client_module, &module_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 2:12 no fix"
    );

    // Compiles on the client as well as the server, so the rename still will not.
    let both = "&НаКлиентеНаСервереБезКонтекста\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let both_diags = form_module_diagnostics(both);
    assert_eq!(
        fixes_of(both, &both_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 3:12 no fix"
    );
}

#[test]
fn current_date_keeps_the_fix_when_the_project_has_no_thin_client() {
    // A configuration that targets only the thick managed client can call
    // `ТекущаяДатаСеанса` from `&НаКлиенте`. The gate must follow the project
    // settings, not withhold every client method.
    let source = "&НаКлиенте\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let options = EnvOptions {
        client_environments: EnvFlags::THICK_CLIENT_MANAGED,
        checked_environments: EnvFlags::THICK_CLIENT_MANAGED | EnvFlags::SERVER,
        ..EnvOptions::default()
    };
    let diagnostics = form_module_diagnostics_with(source, options);
    let fixes = fixes_of(source, &diagnostics, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("Дата = ТекущаяДатаСеанса();"), "got: {fixes}");
    assert!(fixes.contains("[fix_all=false]"), "got: {fixes}");
}

#[test]
fn current_date_keeps_the_manual_fix_on_the_server() {
    let server_form =
        "&НаСервере\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let form_diags = form_module_diagnostics(server_form);
    assert_eq!(
        fixes_of(server_form, &form_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 3:12 Заменить на \"ТекущаяДатаСеанса\" (время в часовом поясе сеанса) [fix_all=false]\n\
         &НаСервере\nПроцедура Обновить()\n    Дата = ТекущаяДатаСеанса();\nКонецПроцедуры\n"
    );

    let server_module = "Процедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let module_diags = fixture_module_diagnostics(
        "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl",
        server_module,
    );
    let fixes = fixes_of(server_module, &module_diags, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("[fix_all=false]"), "got: {fixes}");
    assert!(fixes.contains("Дата = ТекущаяДатаСеанса();"), "got: {fixes}");
}

#[test]
fn a_module_method_of_the_replacement_name_is_not_renamed() {
    let find = "Функция СтрНайти(Строка, Что)\n    Возврат 0;\nКонецФункции\n\nПроцедура Тест()\n    Позиция = Найти(\"Строка\", \"о\");\nКонецПроцедуры\n";
    let find_diags = module_diagnostics(find, Project::default());
    assert_eq!(
        fixes_of(find, &find_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 6:15 no fix",
        "replacement must not retarget a module method"
    );

    let date = "Функция ТекущаяДатаСеанса()\n    Возврат '00010101';\nКонецФункции\n\nПроцедура Тест()\n    ТекущаяДата();\nКонецПроцедуры\n";
    let date_diags = module_diagnostics(date, Project::default());
    assert_eq!(
        fixes_of(date, &date_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 6:5 no fix",
        "replacement must not retarget a module method"
    );
}

#[test]
fn an_interceptor_without_its_own_directive_keeps_the_module_environment() {
    let source = "&Вместо(\"Обновить\")\nПроцедура Расш_Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let diagnostics = form_module_diagnostics(source);
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 3:12 no fix"
    );

    let server = "&НаСервере\n&Вместо(\"Обновить\")\nПроцедура Расш_Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n";
    let server_diags = form_module_diagnostics(server);
    let fixes = fixes_of(server, &server_diags, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("Дата = ТекущаяДатаСеанса();"), "got: {fixes}");
    assert!(fixes.contains("[fix_all=false]"), "got: {fixes}");
}

#[test]
fn a_module_method_of_the_same_name_is_not_renamed() {
    // The bare call resolves to the module method, not the platform global.
    let find = "Процедура Найти(Строка, Что)\nКонецПроцедуры\n\nПроцедура Тест()\n    Найти(\"Строка\", \"о\");\nКонецПроцедуры\n";
    let find_diags = module_diagnostics(find, Project::default());
    assert_eq!(
        fixes_of(find, &find_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 5:5 no fix"
    );

    let date = "Процедура ТекущаяДата()\nКонецПроцедуры\n\nПроцедура Тест()\n    ТекущаяДата();\nКонецПроцедуры\n";
    let date_diags = module_diagnostics(date, Project::default());
    assert_eq!(
        fixes_of(date, &date_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 5:5 no fix"
    );
}

// --- InvalidCharacterInFile ----------------------------------------------------------

#[test]
fn illegal_characters_in_a_comment_are_replaced_safely() {
    let source = "// Итог\u{00A0}\u{2014} сумма с\u{00AD}кидки\nА = 1;\n";
    let diagnostics = module_diagnostics(source, Project::default());
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::InvalidCharacterInFile),
        "InvalidCharacterInFile @ 1:1 Заменить недопустимые символы [fix_all=true]\n\
         // Итог - сумма скидки\nА = 1;\n"
    );
}

#[test]
fn illegal_character_in_a_multiline_string_comment_line_is_manual() {
    // A continuation line that starts with `//` (no `|`) is a COMMENT token, but
    // it is text of the literal. A safe fix would change that value unattended.
    let source = "Процедура Тест()\n    Т = \"первая\n    // вторая\u{00A0}строка\n    |конец\";\nКонецПроцедуры\n";
    let diagnostics = module_diagnostics(source, Project::default());
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::InvalidCharacterInFile),
        "InvalidCharacterInFile @ 3:5 Заменить недопустимые символы (меняет текст программы) [fix_all=false]\n\
         Процедура Тест()\n    Т = \"первая\n    // вторая строка\n    |конец\";\nКонецПроцедуры\n"
    );
}

#[test]
fn an_undecidable_region_does_not_erase_the_directive_environment() {
    let client = "#Если Линукс Тогда\n&НаКлиенте\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n#КонецЕсли\n";
    let client_diags = form_module_diagnostics(client);
    assert_eq!(
        fixes_of(client, &client_diags, DiagnosticCode::DeprecatedPlatformApi),
        "DeprecatedPlatformApi @ 4:12 no fix"
    );

    let server = "#Если Линукс Тогда\n&НаСервере\nПроцедура Обновить()\n    Дата = ТекущаяДата();\nКонецПроцедуры\n#КонецЕсли\n";
    let server_diags = form_module_diagnostics(server);
    let fixes = fixes_of(server, &server_diags, DiagnosticCode::DeprecatedPlatformApi);
    assert!(fixes.contains("Дата = ТекущаяДатаСеанса();"), "got: {fixes}");
    assert!(fixes.contains("[fix_all=false]"), "got: {fixes}");
}

#[test]
fn illegal_character_in_a_parameter_doc_separator_is_manual() {
    // The analyzer reads `Имя - Тип` under `Параметры:`. A non-breaking space
    // hides that split; a safe fix would invent the type unattended.
    let source = "// Параметры:\n//   Имя\u{00A0}-\u{00A0}Строка\nФункция Тест(Имя) Экспорт\n    Возврат Имя;\nКонецФункции\n";
    let diagnostics = module_diagnostics(source, Project::default());
    let fixes = fixes_of(source, &diagnostics, DiagnosticCode::InvalidCharacterInFile);
    assert!(
        fixes.contains("Заменить недопустимые символы (меняет текст программы) [fix_all=false]"),
        "got: {fixes}"
    );
    assert!(fixes.contains("//   Имя - Строка"), "got: {fixes}");
}

#[test]
fn illegal_character_in_a_string_is_a_manual_fix() {
    // A non-breaking space inside a literal may be exactly what the code looks for.
    let source = "Процедура Тест(Текст)\n    Текст = СтрЗаменить(Текст, \"\u{00A0}\", \"\");\nКонецПроцедуры\n";
    let diagnostics = module_diagnostics(source, Project::default());
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::InvalidCharacterInFile),
        "InvalidCharacterInFile @ 2:32 Заменить недопустимые символы (меняет текст программы) [fix_all=false]\n\
         Процедура Тест(Текст)\n    Текст = СтрЗаменить(Текст, \" \", \"\");\nКонецПроцедуры\n"
    );
}

// --- ServerSideExportFormMethod ------------------------------------------------------

#[test]
fn server_export_of_a_form_method_loses_export_as_a_manual_fix() {
    let source = "&НаСервере\nПроцедура Пересчитать() Экспорт // итог\nКонецПроцедуры\n\n\
                  &НаСервере\nФункция Сумма(А, Б) Export\n    Возврат А + Б;\nКонецФункции\n";
    let diagnostics = form_module_diagnostics(source);
    assert_eq!(
        fixes_of(source, &diagnostics, DiagnosticCode::ServerSideExportFormMethod),
        "ServerSideExportFormMethod @ 2:11 Убрать \"Экспорт\" [fix_all=false]\n\
         &НаСервере\nПроцедура Пересчитать() // итог\nКонецПроцедуры\n\n\
         &НаСервере\nФункция Сумма(А, Б) Export\n    Возврат А + Б;\nКонецФункции\n\
         \n---\n\
         ServerSideExportFormMethod @ 6:9 Убрать \"Экспорт\" [fix_all=false]\n\
         &НаСервере\nПроцедура Пересчитать() Экспорт // итог\nКонецПроцедуры\n\n\
         &НаСервере\nФункция Сумма(А, Б)\n    Возврат А + Б;\nКонецФункции\n"
    );
}

#[test]
fn client_export_of_a_form_method_gets_no_diagnostic() {
    let source = "&НаКлиенте\nПроцедура Обновить() Экспорт\nКонецПроцедуры\n";
    let diagnostics = form_module_diagnostics(source);
    assert_eq!(fixes_of(source, &diagnostics, DiagnosticCode::ServerSideExportFormMethod), "");
}
