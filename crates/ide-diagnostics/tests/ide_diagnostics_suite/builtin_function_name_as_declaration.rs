//! `BuiltinFunctionNameAsDeclaration`: a function declared as `Найти`/`Find` is rejected by
//! the 1C:Enterprise 8.3.27 compiler («Ожидается имя функции»). The rule marks the declared
//! name and nothing else: not a call, not a procedure, not another built-in name.
//!
//! Sources live in `src/handlers/fixtures/builtin_function_name_as_declaration/`. Every
//! expectation is derived from the declaration it points at (line, column, spelling), never
//! from a run of the rule, and every "silent" case is paired with a control that differs by
//! one change and must report.

use expect_test::{expect, Expect};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticType, DiagnosticsConfig, Severity};
use std::path::Path;
use std::sync::Arc;
use vfs::{FileId, FileSet, VfsPath};

const NEW_CODE: DiagnosticCode = DiagnosticCode::BuiltinFunctionNameAsDeclaration;
const OLD_CODE: DiagnosticCode = DiagnosticCode::ReservedWordAsMethodName;

const ISSUE_FIND: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/issue_find_function.bsl"
);
const ISSUE_CONTROLS: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/issue_controls.bsl"
);
const FIND_AFTER_NEUTRAL: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/find_after_neutral_method.bsl"
);
const ENGLISH_FIND: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/en_find_function.bsl"
);
const CASE_AND_EXPORT: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/case_and_export_variants.bsl"
);
const CALLS_OF_FIND: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/calls_of_find.bsl"
);
const PROCEDURES_NAMED_FIND: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/procedures_named_find.bsl"
);
const NAMES_AROUND_THE_PAIR: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/names_outside_pair.bsl"
);
const FIND_AND_RESERVED_WORD: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/find_and_reserved_word.bsl"
);
const MODULE_CONTEXT_PROBE: &str = include_str!(
    "../../src/handlers/fixtures/builtin_function_name_as_declaration/module_context_probe.bsl"
);

/// What the rule says about `Найти` on the first line of a one-function source.
const RU_FIND_FINDING: &str = r#"BuiltinFunctionNameAsDeclaration @ 1:9..1:14
  message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
  severity: Blocker"#;

/// The findings of `code` in `source` as a plain file under the default test configuration.
fn findings(source: &str, code: DiagnosticCode) -> Vec<Diagnostic> {
    let mut found: Vec<Diagnostic> =
        diagnostics_in(Context::PlainFile, None, source, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .collect();
    found.sort_by_key(|diagnostic| (diagnostic.range.start(), diagnostic.range.end()));
    found
}

/// One finding per block: `code @ line:col..line:col` (1-based, columns in characters), then
/// the message and the severity.
fn render(source: &str, diagnostics: &[Diagnostic]) -> String {
    fn position(source: &str, offset: usize) -> (usize, usize) {
        let before = &source[..offset];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (line, column)
    }

    let mut sorted: Vec<&Diagnostic> = diagnostics.iter().collect();
    sorted.sort_by_key(|diagnostic| {
        (diagnostic.range.start(), diagnostic.range.end(), diagnostic.code.as_str())
    });
    sorted
        .into_iter()
        .map(|diagnostic| {
            let (start_line, start_col) = position(source, usize::from(diagnostic.range.start()));
            let (end_line, end_col) = position(source, usize::from(diagnostic.range.end()));
            format!(
                "{} @ {start_line}:{start_col}..{end_line}:{end_col}\n  message: {}\n  severity: {}",
                diagnostic.code.as_str(),
                diagnostic.message,
                diagnostic.severity.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_snapshot(source: &str, code: DiagnosticCode, expected: Expect) {
    expected.assert_eq(&render(source, &findings(source, code)));
}

fn texts<'a>(source: &'a str, diagnostics: &[Diagnostic]) -> Vec<&'a str> {
    diagnostics.iter().map(|diagnostic| &source[diagnostic.range]).collect()
}

// --- I1 -------------------------------------------------------------------------------

#[test]
fn issue_example_is_reported_on_the_declared_name() {
    check_snapshot(
        ISSUE_FIND,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 1:9..1:14
              message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );

    let found = findings(ISSUE_FIND, NEW_CODE);
    assert_eq!(found.len(), 1, "one declaration, one finding: {found:?}");
    let finding = &found[0];
    assert_eq!(texts(ISSUE_FIND, &found), ["Найти"], "the range is the name token, not the method");
    assert_eq!(finding.severity, Severity::Blocker);
    assert!(finding.fixes.is_empty(), "no quick fix is offered: {:?}", finding.fixes);
}

#[test]
fn name_of_a_later_method_is_located_in_file_coordinates() {
    check_snapshot(
        FIND_AFTER_NEUTRAL,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 5:9..5:14
              message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );

    // The call `Массив.Найти(...)` above the declaration is not the finding.
    let expected_start =
        FIND_AFTER_NEUTRAL.find("Функция Найти").expect("the declaration") + "Функция ".len();
    let found = findings(FIND_AFTER_NEUTRAL, NEW_CODE);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(usize::from(found[0].range.start()), expected_start);
    assert_eq!(usize::from(found[0].range.end()), expected_start + "Найти".len());
    assert!(expected_start > 0, "the control must not sit at the start of the file");
}

// --- I2 -------------------------------------------------------------------------------

#[test]
fn english_declaration_is_reported_under_its_own_spelling() {
    check_snapshot(
        ENGLISH_FIND,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 1:10..1:14
              message: Имя "Find" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );
}

#[test]
fn letter_case_and_export_do_not_change_the_finding() {
    // Four declarations of the pair: mixed-case RU with Export, mixed-case EN with Export,
    // upper-case RU and upper-case EN without Export.
    check_snapshot(
        CASE_AND_EXPORT,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 1:9..1:14
              message: Имя "нАйТи" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker
            BuiltinFunctionNameAsDeclaration @ 5:10..5:14
              message: Имя "fInD" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker
            BuiltinFunctionNameAsDeclaration @ 9:9..9:14
              message: Имя "НАЙТИ" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker
            BuiltinFunctionNameAsDeclaration @ 13:10..13:14
              message: Имя "FIND" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );
}

// --- I3 -------------------------------------------------------------------------------

#[test]
fn get_value_function_is_not_reported() {
    check_snapshot(ISSUE_CONTROLS, NEW_CODE, expect![[r#""#]]);
    check_snapshot(ISSUE_CONTROLS, OLD_CODE, expect![[r#""#]]);

    // Other rules stay live on the same name: the silence is the new rule's, not a blanket.
    let naming = findings(ISSUE_CONTROLS, DiagnosticCode::FunctionNameStartsWithGet);
    assert_eq!(texts(ISSUE_CONTROLS, &naming), ["ПолучитьЗначение"], "{naming:?}");

    // Control: the same declaration under the name `Найти` is the finding.
    let renamed = ISSUE_CONTROLS.replacen("ПолучитьЗначение", "Найти", 1);
    assert_eq!(texts(&renamed, &findings(&renamed, NEW_CODE)), ["Найти"]);
}

// --- I4 -------------------------------------------------------------------------------

#[test]
fn calls_of_find_are_not_reported() {
    // `Массив.Найти(...)`, `Array.Find(...)` and the global `Найти("abc", "b")`, each inside
    // a neutrally named function.
    check_snapshot(CALLS_OF_FIND, NEW_CODE, expect![[r#""#]]);
    check_snapshot(CALLS_OF_FIND, OLD_CODE, expect![[r#""#]]);
    check_snapshot(ISSUE_CONTROLS, NEW_CODE, expect![[r#""#]]);

    // Control: a declaration appended to the same module is the only finding, and it is on the
    // declaration, not on any of the three calls above it.
    let with_declaration = format!("{CALLS_OF_FIND}\n{ISSUE_FIND}");
    check_snapshot(
        &with_declaration,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 13:9..13:14
              message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );
}

// --- I5 -------------------------------------------------------------------------------

#[test]
fn procedures_named_find_are_not_reported() {
    check_snapshot(PROCEDURES_NAMED_FIND, NEW_CODE, expect![[r#""#]]);
    check_snapshot(PROCEDURES_NAMED_FIND, OLD_CODE, expect![[r#""#]]);

    // Control: the same two declarations as functions are findings, so the silence above is
    // about the kind of declaration and not about the file.
    let as_functions = PROCEDURES_NAMED_FIND
        .replace("КонецПроцедуры", "КонецФункции")
        .replace("Процедура", "Функция")
        .replace("Procedure", "Function");
    check_snapshot(
        &as_functions,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 1:9..1:14
              message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker
            BuiltinFunctionNameAsDeclaration @ 4:10..4:14
              message: Имя "Find" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );
}

// --- I6 -------------------------------------------------------------------------------

#[test]
fn names_around_the_pair_are_not_reported() {
    // `СтрНайти`/`StrFind` are in the platform's global function catalog; `НайтиЗначение`/
    // `FindValue` merely start with the pair. None of them is in the set.
    check_snapshot(NAMES_AROUND_THE_PAIR, NEW_CODE, expect![[r#""#]]);

    // Control: the pair itself, appended to the same module, is the only finding.
    let with_pair = format!("{NAMES_AROUND_THE_PAIR}\n{ISSUE_FIND}");
    check_snapshot(
        &with_pair,
        NEW_CODE,
        expect![[r#"
            BuiltinFunctionNameAsDeclaration @ 17:9..17:14
              message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
              severity: Blocker"#]],
    );
}

// --- I7 -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Context {
    /// A file with no metadata around it: `/test.bsl`.
    PlainFile,
    /// The module of a non-global common module available on the server and over an
    /// external connection — the module of the issue.
    CommonModule,
    /// The object module of a catalog.
    ObjectModule,
}

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.20">"#;

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))
        .expect("create fixture directory");
    std::fs::write(path, text).expect("write fixture file");
}

fn write_configuration(root: &Path) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"{HEADER}
    <Configuration uuid="5a5a0000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Синтетика</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>Модуль</CommonModule>
            <Catalog>Товары</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "CommonModules/Модуль.xml",
        &format!(
            r#"{HEADER}
    <CommonModule uuid="5a5a0000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Модуль</Name>
            <Global>false</Global>
            <ClientManagedApplication>false</ClientManagedApplication>
            <Server>true</Server>
            <ExternalConnection>true</ExternalConnection>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <ServerCall>false</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "Catalogs/Товары.xml",
        &format!(
            r#"{HEADER}
    <Catalog uuid="5a5a0000-0000-0000-0000-000000000003">
        <Properties>
            <Name>Товары</Name>
        </Properties>
    </Catalog>
</MetaDataObject>"#
        ),
    );
}

/// The findings of the production pipeline (`file_diagnostics`) for `source` as the module of
/// `context`, with the project's `target_platform_version` set to `target`.
fn diagnostics_in(
    context: Context,
    target: Option<&str>,
    source: &str,
    config: &DiagnosticsConfig,
) -> Vec<Diagnostic> {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let root = dir.path();
    let relative = match context {
        Context::PlainFile => None,
        Context::CommonModule => Some("CommonModules/Модуль/Ext/Module.bsl"),
        Context::ObjectModule => Some("Catalogs/Товары/Ext/ObjectModule.bsl"),
    };

    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    let path = match relative {
        Some(relative) => {
            write_configuration(root);
            write(root, relative, source);
            db.set_all_config_paths(
                vec![(None, root.to_path_buf())],
                Some(bsl_metadata::ExtensionScope::InfoBase),
            );
            db.set_workspace_load_complete(true);
            root.join(relative).to_string_lossy().into_owned()
        }
        None => "/test.bsl".to_string(),
    };
    files.insert(file_id, VfsPath::new(path));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_target_platform_version(target.map(Arc::<str>::from));
    ide_diagnostics::file_diagnostics(&db, file_id, config)
}

#[test]
fn the_finding_does_not_depend_on_module_context_or_target_version() {
    let contexts = [Context::PlainFile, Context::CommonModule, Context::ObjectModule];
    let targets = [None, Some("8.3.27.2074"), Some("8.3.26")];

    for context in contexts {
        for target in targets {
            let diagnostics = diagnostics_in(
                context,
                target,
                MODULE_CONTEXT_PROBE,
                &DiagnosticsConfig::all_enabled(),
            );
            let ours: Vec<Diagnostic> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == NEW_CODE)
                .cloned()
                .collect();
            assert_eq!(
                render(MODULE_CONTEXT_PROBE, &ours),
                RU_FIND_FINDING,
                "{context:?} with target {target:?}"
            );

            // The context really is the one named: `Выполнить(...)` in a server common module is
            // `ExecuteExternalCodeInCommonModule`, anywhere else it is not. Without this a
            // mislaid fixture would make every context the plain file and prove nothing.
            let probe = diagnostics
                .iter()
                .filter(|d| d.code == DiagnosticCode::ExecuteExternalCodeInCommonModule);
            let expected_probe = match context {
                Context::CommonModule => 1,
                Context::PlainFile | Context::ObjectModule => 0,
            };
            assert_eq!(
                probe.count(),
                expected_probe,
                "{context:?} with target {target:?}: the module context is not the one under test"
            );

            // `ЭтотОбъект.Код` is `RedundantAccessToObject` only in an object module, which tells
            // the object module apart from the plain file the common-module probe cannot.
            let object_probe =
                diagnostics.iter().filter(|d| d.code == DiagnosticCode::RedundantAccessToObject);
            let expected_object_probe = match context {
                Context::ObjectModule => 1,
                Context::PlainFile | Context::CommonModule => 0,
            };
            assert_eq!(
                object_probe.count(),
                expected_object_probe,
                "{context:?} with target {target:?}: the module context is not the one under test"
            );
        }
    }
}

#[test]
fn get_value_is_silent_in_every_module_context() {
    for context in [Context::PlainFile, Context::CommonModule, Context::ObjectModule] {
        let diagnostics = diagnostics_in(
            context,
            Some("8.3.27.2074"),
            ISSUE_CONTROLS,
            &DiagnosticsConfig::all_enabled(),
        );
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.code != NEW_CODE),
            "{context:?}: {diagnostics:?}"
        );
    }
}

// --- I8 -------------------------------------------------------------------------------

#[test]
fn existing_reserved_word_inputs_keep_their_findings_and_get_no_new_code() {
    // The six inputs of the `ReservedWordAsMethodName` tests, with the number of old findings
    // each has there.
    let cases: [(&str, usize); 6] = [
        ("Процедура Выполнить(Команда)\nКонецПроцедуры", 1),
        ("Функция Новый()\n    Возврат 1;\nКонецФункции", 1),
        ("Процедура Если()\nКонецПроцедуры", 1),
        ("Procedure Execute(Command)\nEndProcedure", 1),
        ("Процедура МояПроцедура()\nКонецПроцедуры", 0),
        ("Функция ПолучитьЗначение()\n    Возврат 1;\nКонецФункции", 0),
    ];
    for (source, old_count) in cases {
        assert_eq!(findings(source, OLD_CODE).len(), old_count, "old rule on {source:?}");
        assert!(findings(source, NEW_CODE).is_empty(), "new rule on {source:?}");
    }
}

#[test]
fn find_functions_are_not_reserved_words() {
    for source in [ISSUE_FIND, ENGLISH_FIND, CASE_AND_EXPORT] {
        assert!(findings(source, OLD_CODE).is_empty(), "{source:?}");
        assert!(!findings(source, NEW_CODE).is_empty(), "{source:?}");
    }
}

// --- I9 -------------------------------------------------------------------------------

fn project_config(section: serde_json::Value) -> DiagnosticsConfig {
    DiagnosticsConfig::from_project_json(&section, base_db::Locale::En)
}

/// The two name rules' findings on `FIND_AND_RESERVED_WORD` under `config`, formatted.
fn name_rules_under(config: DiagnosticsConfig) -> String {
    let diagnostics: Vec<Diagnostic> =
        diagnostics_in(Context::PlainFile, None, FIND_AND_RESERVED_WORD, &config)
            .into_iter()
            .filter(|diagnostic| diagnostic.code == NEW_CODE || diagnostic.code == OLD_CODE)
            .collect();
    render(FIND_AND_RESERVED_WORD, &diagnostics)
}

#[test]
fn default_configuration_reports_both_name_rules() {
    expect![[r#"
        BuiltinFunctionNameAsDeclaration @ 1:9..1:14
          message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
          severity: Blocker
        ReservedWordAsMethodName @ 5:11..5:20
          message: Имя "Выполнить" является зарезервированным словом и не может использоваться как имя процедуры/функции
          severity: Blocker"#]]
    .assert_eq(&name_rules_under(project_config(serde_json::json!({}))));
}

#[test]
fn disabling_the_new_code_leaves_the_reserved_word_finding() {
    expect![[r#"
        ReservedWordAsMethodName @ 5:11..5:20
          message: Имя "Выполнить" является зарезервированным словом и не может использоваться как имя процедуры/функции
          severity: Blocker"#]]
    .assert_eq(&name_rules_under(project_config(serde_json::json!({
        "parameters": { "BuiltinFunctionNameAsDeclaration": false }
    }))));
}

#[test]
fn disabling_the_reserved_word_rule_leaves_the_new_finding() {
    expect![[r#"
        BuiltinFunctionNameAsDeclaration @ 1:9..1:14
          message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
          severity: Blocker"#]]
    .assert_eq(&name_rules_under(project_config(serde_json::json!({
        "parameters": { "ReservedWordAsMethodName": false }
    }))));
}

#[test]
fn a_user_chosen_level_replaces_the_default_blocker() {
    expect![[r#"
        BuiltinFunctionNameAsDeclaration @ 1:9..1:14
          message: Имя "Найти" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции
          severity: Warning
        ReservedWordAsMethodName @ 5:11..5:20
          message: Имя "Выполнить" является зарезервированным словом и не может использоваться как имя процедуры/функции
          severity: Blocker"#]]
    .assert_eq(&name_rules_under(project_config(serde_json::json!({
        "metadataOverrides": { "BuiltinFunctionNameAsDeclaration": { "lspSeverity": "warning" } }
    }))));
}

#[test]
fn a_run_selecting_only_the_new_code_still_reports_it() {
    // The selective run (`--only-diagnostic`): the lowering collector is entered only when
    // some enabled code is one of its own, so the code must be among them.
    let mut only_new = DiagnosticsConfig::all_enabled();
    only_new.only_enabled = Some(vec![NEW_CODE]);
    let found = diagnostics_in(Context::PlainFile, None, ISSUE_FIND, &only_new);
    assert_eq!(render(ISSUE_FIND, &found), RU_FIND_FINDING);

    // Control: selecting another code leaves the declaration unreported.
    let mut only_old = DiagnosticsConfig::all_enabled();
    only_old.only_enabled = Some(vec![OLD_CODE]);
    let found = diagnostics_in(Context::PlainFile, None, ISSUE_FIND, &only_old);
    assert_eq!(render(ISSUE_FIND, &found), "");
}

#[test]
fn the_new_code_is_an_error_enabled_by_default() {
    let metadata = ide_diagnostics::get_metadata(NEW_CODE).expect("the code has metadata");
    assert!(metadata.activated_by_default);
    assert_eq!(metadata.diagnostic_type, DiagnosticType::Error);
    assert_eq!(metadata.effective_severity(), Severity::Blocker);
    assert_eq!(
        DiagnosticsConfig::default().severity(NEW_CODE),
        Severity::Blocker,
        "no override: the default level of the code"
    );
}
