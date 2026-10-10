//! A bare call `Имя(...)` looks among methods only. A parameter, a `Перем` of the body or
//! of the module, or an implicit local named like a method leaves the call calling that
//! method: the module's own function, a global common-module export or the platform
//! function. Checked on 8.3.17 and 8.3.27 (compatibility 8.3.17) with external data
//! processors: each such call returned the method's own result, and a call of a name only
//! a parameter holds failed to compile with "Процедура или функция с указанным именем не
//! определена".
//!
//! Every test pins the inferred type of the call result (or, where the callee has no
//! return type, the contract the call was judged by) before it counts diagnostics: an
//! empty diagnostic list also holds when the call resolves to nothing.

use hir::{Builders, HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

// The inline fixture format cannot carry the `<Global>` flag, so the caller and the global
// module live at their paths in the on-disk `designer` configuration.
const SERVER_REL: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";
const CLIENT_REL: &str = "CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl";
const GLOBAL_REL: &str = "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl";
const GLOBAL_BODY: &str = r#"
Функция ВКавычках(Строка) Экспорт
    Возврат """" + Строка + """";
КонецФункции
"#;

fn setup(caller_text: &str) -> (RootDatabaseImpl, FileId) {
    setup_at(SERVER_REL, caller_text)
}

fn setup_at(caller_rel: &str, caller_text: &str) -> (RootDatabaseImpl, FileId) {
    let caller_id = FileId::from_raw(1);
    let global_id = FileId::from_raw(2);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (id, rel) in [(caller_id, caller_rel), (global_id, GLOBAL_REL)] {
        file_set.insert(id, VfsPath::new(designer_path().join(rel).to_string_lossy().to_string()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, text) in [(caller_id, caller_text), (global_id, GLOBAL_BODY)] {
        db.set_file_source_root(id, SourceRootId(0));
        db.set_file_text(id, text);
    }
    db.set_all_config_paths(vec![(None, designer_path())]);
    (db, caller_id)
}

/// Type of the value last assigned to the implicit local `name_lower`. Read from the
/// implicit locals rather than `var_types`, which leaves out locals of unknown type.
fn local_ty(db: &RootDatabaseImpl, file_id: FileId, name_lower: &str) -> TypeId {
    db.infer(file_id)
        .implicit_locals_by_body
        .values()
        .find_map(|locals| locals.get(name_lower))
        .and_then(|info| info.assignments.last())
        .map(|assignment| assignment.ty)
        .unwrap_or_else(|| panic!("`{name_lower}` must be an inferred local"))
}

fn assert_locals(db: &RootDatabaseImpl, file_id: FileId, locals: &[&str], expected: TypeId) {
    for local in locals {
        let ty = local_ty(db, file_id, local);
        assert_eq!(
            ty,
            expected,
            "`{local}`: expected {:?}, got {:?}",
            db.lookup_type(expected),
            db.lookup_type(ty)
        );
    }
}

fn count(
    db: &RootDatabaseImpl,
    file_id: FileId,
    pick: impl Fn(&InferenceDiagnostic) -> bool,
) -> usize {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter(|(_, d)| pick(d))
        .count()
}

fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> usize {
    count(db, file_id, |d| matches!(d, InferenceDiagnostic::TypeMismatch { .. }))
}

fn arg_count_mismatches(db: &RootDatabaseImpl, file_id: FileId) -> usize {
    count(db, file_id, |d| matches!(d, InferenceDiagnostic::MismatchedArgCount { .. }))
}

const LOCAL_KINDS: [&str; 3] = ["черезпараметр", "черезперем", "черезлокальную"];

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_local_variable_leaves_the_call_to_the_module_function() {
    let (db, file_id) = setup(
        r#"
Функция Проба()
    Возврат "МЕТОД_МОДУЛЯ";
КонецФункции

Процедура Тест1(Проба)
    ЧерезПараметр = Проба();
    Текст = Формат(ЧерезПараметр, "ЧГ=0");
КонецПроцедуры

Процедура Тест2()
    Перем Проба;
    Проба = 1;
    ЧерезПерем = Проба();
    Текст = Формат(ЧерезПерем, "ЧГ=0");
КонецПроцедуры

Процедура Тест3()
    Проба = 1;
    ЧерезЛокальную = Проба();
    Текст = Формат(ЧерезЛокальную, "ЧГ=0");
КонецПроцедуры
"#,
    );
    assert_locals(&db, file_id, &LOCAL_KINDS, db.string(None, false));
    assert_eq!(mismatches(&db, file_id), 3, "Формат does not accept the function's Строка");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_module_variable_leaves_the_call_to_the_module_function() {
    // The write types the module variable as a Число, so the callee read as a value is
    // neither unknown nor a function; the call still reaches the function.
    let (db, file_id) = setup(
        r#"
Перем Проба Экспорт;

Функция Проба()
    Возврат "МЕТОД_МОДУЛЯ";
КонецФункции

Процедура Тест1()
    Проба = 1;
    ПослеЗаписи = Проба();
    Текст = Формат(ПослеЗаписи, "ЧГ=0");
КонецПроцедуры

Процедура Тест2()
    БезЗаписи = Проба();
КонецПроцедуры
"#,
    );
    assert_locals(&db, file_id, &["послезаписи", "беззаписи"], db.string(None, false));
    assert_eq!(mismatches(&db, file_id), 1, "Формат does not accept the function's Строка");
}

#[test]
fn a_local_variable_leaves_the_call_to_the_global_export() {
    let (db, file_id) = setup(
        r#"
Процедура Тест1(ВКавычках)
    ЧерезПараметр = ВКавычках("x");
    ВКавычках();
КонецПроцедуры

Процедура Тест2()
    Перем ВКавычках;
    ЧерезПерем = ВКавычках("x");
    ВКавычках();
КонецПроцедуры

Процедура Тест3()
    ВКавычках = 1;
    ЧерезЛокальную = ВКавычках("x");
    ВКавычках();
КонецПроцедуры
"#,
    );
    let string = local_ty(&db, file_id, "черезпараметр");
    assert!(
        matches!(db.lookup_type(string), TypeKind::String(_)),
        "the export returns a Строка, got {:?}",
        db.lookup_type(string)
    );
    assert_locals(&db, file_id, &LOCAL_KINDS, string);
    assert_eq!(
        arg_count_mismatches(&db, file_id),
        3,
        "each argument-less call is checked against the export's one parameter"
    );
}

#[test]
fn a_module_variable_leaves_the_call_to_the_global_export() {
    let (db, file_id) = setup(
        r#"
Перем ВКавычках;

Процедура Тест()
    ЧерезПеремМодуля = ВКавычках("x");
    ВКавычках();
КонецПроцедуры
"#,
    );
    let ty = local_ty(&db, file_id, "черезпереммодуля");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::String(_)),
        "the export returns a Строка, got {:?}",
        db.lookup_type(ty)
    );
    assert_eq!(arg_count_mismatches(&db, file_id), 1);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_variable_leaves_the_call_to_the_platform_function() {
    let (db, file_id) = setup(
        r#"
Перем Формат;

Процедура Тест1(СтрДлина)
    ЧерезПараметр = СтрДлина("abcd");
    Текст = Формат(ЧерезПараметр, "ЧГ=0");
КонецПроцедуры

Процедура Тест2()
    Перем СтрДлина;
    ЧерезПерем = СтрДлина("abcd");
    Текст = Формат(ЧерезПерем, "ЧГ=0");
КонецПроцедуры

Процедура Тест3()
    СтрДлина = "строка";
    ЧерезЛокальную = СтрДлина("abcd");
    Текст = Формат(ЧерезЛокальную, "ЧГ=0");
КонецПроцедуры
"#,
    );
    assert_locals(&db, file_id, &LOCAL_KINDS, db.number(None, None));
    assert_eq!(mismatches(&db, file_id), 0, "a Число is a valid Формат argument");
    assert_eq!(arg_count_mismatches(&db, file_id), 0);
}

#[test]
fn a_variable_leaves_the_call_to_a_catalog_only_platform_function() {
    // `ПолучитьИнформациюОСетевыхАдаптерах` is in the platform catalog without an HBK
    // signature, and the thin client lacks it: a client module call is judged by the
    // catalog's availability, which is the evidence that the call reached the function.
    let unavailable = |db: &RootDatabaseImpl, file_id| {
        count(db, file_id, |d| matches!(d, InferenceDiagnostic::UnavailableInEnvironment { .. }))
    };
    let (db, file_id) = setup_at(
        CLIENT_REL,
        r#"
Процедура Тест()
    ПолучитьИнформациюОСетевыхАдаптерах();
КонецПроцедуры
"#,
    );
    assert_eq!(unavailable(&db, file_id), 1, "control: the plain call is judged");

    let (db, file_id) = setup_at(
        CLIENT_REL,
        r#"
Процедура Тест1(ПолучитьИнформациюОСетевыхАдаптерах)
    ПолучитьИнформациюОСетевыхАдаптерах();
КонецПроцедуры

Процедура Тест2()
    Перем ПолучитьИнформациюОСетевыхАдаптерах;
    ПолучитьИнформациюОСетевыхАдаптерах();
КонецПроцедуры

Процедура Тест3()
    ПолучитьИнформациюОСетевыхАдаптерах = 1;
    ПолучитьИнформациюОСетевыхАдаптерах();
КонецПроцедуры
"#,
    );
    assert_eq!(unavailable(&db, file_id), 3, "each call reaches the platform function");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_value_held_in_the_name_and_the_call_of_the_name_coexist() {
    let (db, file_id) = setup(
        r#"
Функция Проба()
    Возврат "МЕТОД_МОДУЛЯ";
КонецФункции

Процедура Тест()
    Проба = Новый Массив;
    Количество = Проба.Количество();
    Результат = Проба();
КонецПроцедуры
"#,
    );
    assert_locals(&db, file_id, &["количество"], db.number(None, None));
    assert_locals(&db, file_id, &["результат"], db.string(None, false));
}

#[test]
fn a_procedure_statement_reaches_the_procedure_past_a_parameter() {
    let (db, file_id) = setup(
        r#"
Процедура ПробаП()
КонецПроцедуры

Процедура Тест(ПробаП)
    ПробаП(1);
КонецПроцедуры
"#,
    );
    assert_eq!(
        arg_count_mismatches(&db, file_id),
        1,
        "the statement calls the parameterless procedure, so one argument is too many"
    );
}

#[test]
fn a_name_only_a_variable_holds_calls_nothing() {
    let (db, file_id) = setup(
        r#"
Процедура Тест(НетТакойФункции)
    Результат = НетТакойФункции(1);
КонецПроцедуры
"#,
    );
    assert_locals(&db, file_id, &["результат"], db.unknown());
    assert_eq!(arg_count_mismatches(&db, file_id), 0, "there is no contract to check against");
}
