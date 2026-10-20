//! Global common modules extend the global context: their exported methods are callable
//! without a module qualifier. These tests drive the real on-disk `designer` configuration
//! (the inline fixture format cannot carry the `<Global>` flag, which lives in metadata) and
//! exercise every surface — completion, goto-definition, signature help — plus the precedence
//! rule that a same-module method still shadows a global export.

use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn abs(rel: &str) -> String {
    designer_path().join(rel).to_string_lossy().to_string()
}

const GLOBAL_REL: &str = "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl";
const CALLER_REL: &str = "CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl";
const NONGLOBAL_REL: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";

const GLOBAL_BODY: &str = "Процедура ГлобальнаяСервернаяПроцедура() Экспорт\nКонецПроцедуры\n\n\
                           Процедура Сообщить() Экспорт\nКонецПроцедуры\n";
const NONGLOBAL_BODY: &str =
    "Функция НеглобальныйЭкспорт() Экспорт\n    Возврат 1;\nКонецФункции\n";

const CALLER_ID: u32 = 1;
const GLOBAL_ID: u32 = 2;
const NONGLOBAL_ID: u32 = 3;

fn build_db(caller_text: &str) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();

    let caller_id = FileId::from_raw(CALLER_ID);
    let global_id = FileId::from_raw(GLOBAL_ID);
    let nonglobal_id = FileId::from_raw(NONGLOBAL_ID);

    let mut file_set = FileSet::default();
    file_set.insert(caller_id, VfsPath::new(abs(CALLER_REL)));
    file_set.insert(global_id, VfsPath::new(abs(GLOBAL_REL)));
    file_set.insert(nonglobal_id, VfsPath::new(abs(NONGLOBAL_REL)));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));

    for (id, text) in
        [(caller_id, caller_text), (global_id, GLOBAL_BODY), (nonglobal_id, NONGLOBAL_BODY)]
    {
        db.set_file_source_root(id, SourceRootId(0));
        db.set_file_text(id, text);
    }

    db.set_all_config_paths(vec![(None, designer_path())], None);
    db
}

fn setup(caller_text: &str) -> Analysis {
    Analysis::from_database(build_db(caller_text))
}

/// Byte offset just past the first occurrence of `needle` in `text`.
fn after(text: &str, needle: &str) -> u32 {
    (text.find(needle).expect("needle present") + needle.len()) as u32
}

/// Byte offset two bytes into the first occurrence of `needle` — inside the first identifier
/// character, so `token_at_offset` lands on the IDENT.
fn inside(text: &str, needle: &str) -> u32 {
    (text.find(needle).expect("needle present") + 2) as u32
}

#[test]
fn completion_lists_global_export_unqualified() {
    // From a server-capable module the global server module's export is legal
    // and must appear unqualified.
    let probe = "Процедура Тест()\n    Глобальн\nКонецПроцедуры\n";
    let mut db = build_db("Процедура Пусто()\nКонецПроцедуры\n");
    db.set_file_text(FileId::from_raw(NONGLOBAL_ID), probe);
    let analysis = Analysis::from_database(db);
    let offset = after(probe, "Глобальн");

    let items = analysis.completions(FileId::from_raw(NONGLOBAL_ID), offset, None, ide::Locale::Ru);
    assert!(
        items.iter().any(|i| i.label == "ГлобальнаяСервернаяПроцедура"),
        "global common module export must appear unqualified in completion, got {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn dot_completion_offers_no_members_of_inaccessible_module() {
    // `ГлобальныйСерверныйМодуль.` typed in the client-only module: the
    // static receiver is exactly what ModuleAccessibility judges, so no
    // members are offered.
    let caller = "Процедура Тест()\n    ГлобальныйСерверныйМодуль.\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = after(caller, "ГлобальныйСерверныйМодуль.");

    let items = analysis.completions(FileId::from_raw(CALLER_ID), offset, None, ide::Locale::Ru);
    assert!(
        !items.iter().any(|i| i.label == "ГлобальнаяСервернаяПроцедура"),
        "members of a module the caller cannot reach must not be offered, got {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_hides_server_only_global_export_in_client_module() {
    // The client-only module cannot call the server-only global module (no
    // ВызовСервера) — completion mirrors the ModuleAccessibility diagnostic
    // and does not offer the export there.
    let caller = "Процедура Тест()\n    Глобальн\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = after(caller, "Глобальн");

    let items = analysis.completions(FileId::from_raw(CALLER_ID), offset, None, ide::Locale::Ru);
    assert!(
        !items.iter().any(|i| i.label == "ГлобальнаяСервернаяПроцедура"),
        "a client-only module must not be offered a server-only global export"
    );
}

#[test]
fn goto_resolves_bare_global_export_to_owning_module() {
    let caller = "Процедура Тест()\n    ГлобальнаяСервернаяПроцедура();\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = inside(caller, "ГлобальнаяСервернаяПроцедура");

    let target = analysis
        .goto_definition(FileId::from_raw(CALLER_ID), offset)
        .expect("bare global export must resolve");
    assert_eq!(target.file_id, FileId::from_raw(GLOBAL_ID), "must point at the global module");
    assert_eq!(target.name, "ГлобальнаяСервернаяПроцедура");
}

#[test]
fn signature_help_for_bare_global_export() {
    let caller = "Процедура Тест()\n    ГлобальнаяСервернаяПроцедура();\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = after(caller, "ГлобальнаяСервернаяПроцедура(");

    let help = analysis
        .signature_help(FileId::from_raw(CALLER_ID), offset)
        .expect("signature help for a bare global export");
    assert!(
        help.signatures
            .first()
            .expect("at least one signature")
            .signature
            .contains("ГлобальнаяСервернаяПроцедура"),
        "signature must name the global export, got {:?}",
        help.signatures.first().expect("at least one signature").signature
    );
}

#[test]
fn same_module_method_shadows_global_export() {
    // Local → Module → Global-CM: a same-module method of the SAME name wins over the
    // global export, so goto stays inside the caller file.
    let caller = "Процедура ГлобальнаяСервернаяПроцедура() Экспорт\nКонецПроцедуры\n\n\
                  Процедура Тест()\n    ГлобальнаяСервернаяПроцедура();\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let call_site = caller.rfind("ГлобальнаяСервернаяПроцедура").unwrap() + 2;

    let target = analysis
        .goto_definition(FileId::from_raw(CALLER_ID), call_site as u32)
        .expect("same-module method resolves");
    assert_eq!(
        target.file_id,
        FileId::from_raw(CALLER_ID),
        "a same-module method must shadow the global export"
    );
}

#[test]
fn bare_global_call_runs_full_call_contract() {
    use hir::HirDatabase;

    // A bare global export call goes through the same argument-count contract as a qualified
    // call: ГлобальнаяСервернаяПроцедура takes no parameters, so passing one must flag a count
    // mismatch — and crucially must NOT be reported as an unresolved call.
    let caller = "Процедура Тест()\n    ГлобальнаяСервернаяПроцедура(1);\nКонецПроцедуры\n";
    let db = build_db(caller);
    let result = db.infer(FileId::from_raw(CALLER_ID));
    let arg_diagnostics = db.arg_diagnostics(FileId::from_raw(CALLER_ID));

    assert!(
        format!("{arg_diagnostics:?}").contains("MismatchedArgCount"),
        "bare global export call must be argument-count checked, got {arg_diagnostics:?}"
    );
    let diagnostics = format!("{:?}", result.diagnostics);
    assert!(
        !diagnostics.contains("UnresolvedMethodCall"),
        "a resolved global export must not be reported unresolved, got {diagnostics}"
    );
}

#[test]
fn global_export_shadows_platform_builtin_in_goto() {
    // Global-CM → Platform: the global module exports `Сообщить`, a name that is also a
    // platform global. Goto must resolve to the global module's export, not the builtin —
    // keeping HIR-layer resolution consistent with inference and signature help.
    let caller = "Процедура Тест()\n    Сообщить();\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = inside(caller, "Сообщить");

    let target = analysis
        .goto_definition(FileId::from_raw(CALLER_ID), offset)
        .expect("global export shadowing a builtin must resolve");
    assert_eq!(
        target.file_id,
        FileId::from_raw(GLOBAL_ID),
        "a global export must shadow the same-named platform global in goto"
    );
}

#[test]
fn non_global_module_export_not_callable_unqualified() {
    // ПервыйОбщийМодуль is NOT global, so its export must NOT resolve without a qualifier.
    let caller = "Процедура Тест()\n    НеглобальныйЭкспорт();\nКонецПроцедуры\n";
    let analysis = setup(caller);
    let offset = inside(caller, "НеглобальныйЭкспорт");

    assert!(
        analysis.goto_definition(FileId::from_raw(CALLER_ID), offset).is_none(),
        "a non-global module's export must not be callable unqualified"
    );
}

/// `Новый(Тип("X"))` разворачивает строку в имя типа только тогда, когда `Тип` —
/// действительно платформенная функция. Экспорт глобального общего модуля её
/// перекрывает, и тогда строка ничего не говорит о типе результата.
#[test]
fn shadowed_type_function_is_not_a_type_name_wrapper() {
    use hir::HirDatabase;

    let caller = "Процедура Тест()\n    Х = Новый(Тип(\"Массив\"));\nКонецПроцедуры\n";

    let db = build_db(caller);
    let array = {
        use hir::Builders;
        db.array(None)
    };
    assert_eq!(
        db.infer(FileId::from_raw(CALLER_ID)).var_types.get("х").copied(),
        Some(array),
        "control: with no user-defined Тип the platform wrapper must still be unwrapped"
    );

    let mut db = build_db(caller);
    db.set_file_text(
        FileId::from_raw(GLOBAL_ID),
        "Функция Тип(Имя) Экспорт\n    Возврат \"ТаблицаЗначений\";\nКонецФункции\n",
    );
    assert_eq!(
        db.infer(FileId::from_raw(CALLER_ID)).var_types.get("х").copied(),
        None,
        "a global export named Тип shadows the platform function, so the literal says nothing"
    );
}
