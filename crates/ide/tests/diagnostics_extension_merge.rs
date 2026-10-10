//! End-to-end check of `&ИзменениеИКонтроль` extension merging through the diagnostics
//! orchestrator (`Analysis::diagnostics` → `ide_diagnostics::file_diagnostics`).
//!
//! The merge's INFERENCE correctness (base-sibling resolution, missing-method, changed
//! return propagation) is proven against the real Salsa db in `ide-db`'s
//! `infer_effective_*` tests; the orchestrator's remap/suppression arithmetic is unit-
//! tested in `ide-diagnostics`'s `effective` module. These tests cover the remaining
//! seam: that `file_diagnostics` actually pairs the extension file, runs the effective
//! pass, and stays a stable superset — without a real `Configuration.xml` on disk, kernel
//! types resolve but configured-metadata inference diagnostics do not fire, so this asserts
//! the wiring and liveness rather than a specific published inference diagnostic.

use ide::{Analysis, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use stdx::case::CaseExt;
use vfs::{FileId, FileSet, VfsPath};

const BASE: &str = "Функция Сосед() Экспорт\n\tВозврат 1;\nКонецФункции\n\nФункция Цель() Экспорт\n\tВозврат 0;\nКонецФункции";
// `Цель` is rewritten by the extension; its `#Вставка` calls the base sibling `Сосед`
// (resolvable only through the merge) and a genuinely missing method. The copied-base tail
// uses a deliberately lowercase `возврат` so a standalone style diagnostic is present to
// prove it survives the merge orchestrator (the `#Вставка`/`#КонецВставки` directives are
// themselves canonical and must NOT be flagged).
const EXT: &str = "&ИзменениеИКонтроль(\"Цель\")\nФункция Расш1_Цель()\n#Вставка\n\tЗначение1 = Сосед();\n\tЗначение2 = НетТакого();\n#КонецВставки\n\tвозврат 0;\nКонецФункции";

// Weaving signature check: base has a two-parameter procedure and a one-parameter
// by-value function the extension can intercept.
const SIG_BASE: &str = "Процедура ПриЗаписи(Отказ, Параметры) Экспорт\nКонецПроцедуры\n\nФункция Вычислить(Знач А) Экспорт\n\tВозврат А;\nКонецФункции";
// `&Перед` interceptor drops a parameter (one instead of two) → applicability defect.
const SIG_EXT_BAD: &str = "&Перед(\"ПриЗаписи\")\nПроцедура Расш1_ПриЗаписи(Отказ)\nКонецПроцедуры";
// `&Вместо` interceptor mirrors the function signature exactly, including `Знач`.
const SIG_EXT_GOOD: &str =
    "&Вместо(\"Вычислить\")\nФункция Расш1_Вычислить(Знач А)\n\tВозврат А;\nКонецФункции";
// `&После` on the base *function* `Вычислить` is not applicable at all (functions accept
// only `&Вместо`); the deliberate parameter-count divergence must NOT also surface as a
// signature mismatch — the applicability error owns the report.
const SIG_EXT_FUNC_AFTER: &str =
    "&После(\"Вычислить\")\nПроцедура Расш1_Вычислить()\nКонецПроцедуры";

// `ПродолжитьВызов` re-enters the base method, so its argument count must be valid for it.
const PROCEED_BASE: &str = "Функция Вычислить(Знач А, Б) Экспорт\n\tВозврат А;\nКонецФункции";
// Interceptor signature matches the base (two params), but `ПродолжитьВызов` passes only one.
const PROCEED_EXT_BAD: &str = "&Вместо(\"Вычислить\")\nФункция Расш1_Вычислить(Знач А, Б)\n\tВозврат ПродолжитьВызов(А);\nКонецФункции";
// `ПродолжитьВызов` passes both parameters → a valid call to the base method.
const PROCEED_EXT_GOOD: &str = "&Вместо(\"Вычислить\")\nФункция Расш1_Вычислить(Знач А, Б)\n\tВозврат ПродолжитьВызов(А, Б);\nКонецФункции";

struct Fixture {
    analysis: Analysis,
    main_file: FileId,
    ext_file: FileId,
}

fn setup() -> Fixture {
    setup_with(BASE, EXT)
}

fn setup_with(base: &str, ext: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();
    std::mem::forget(temp);

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::default();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));
    db.set_file_text(main_file, base);
    db.set_file_text(ext_file, ext);

    Fixture { analysis: Analysis::from_database(db), main_file, ext_file }
}

#[test]
fn extension_file_pairs_and_merge_resolves_base_sibling() {
    let fx = setup();
    let db = fx.analysis.database();

    // The orchestrator's pairing gate fires for the extension file…
    let eid = ide_db::effective_target(db, fx.ext_file)
        .expect("extension file with a usable change-and-validate must pair to its base");
    // …and not for the base file.
    assert!(
        ide_db::effective_target(db, fx.main_file).is_none(),
        "a base-config file must not pair (only extension modules route to effective)",
    );

    // The merge is live end-to-end: `#Вставка` code sees the base sibling `Сосед` (returns
    // 1 → Число), which is invisible to the standalone extension file.
    use hir::Builders;
    let eff = hir::infer_effective(db, eid);
    assert_eq!(
        eff.var_types.get(&"Значение1".fold_lower()).copied(),
        Some(db.number(None, None)),
        "base sibling `Сосед()` must resolve through the effective module; var_types = {:?}",
        eff.var_types,
    );
}

#[test]
fn extension_diagnostics_are_a_stable_superset_without_spurious_inference() {
    let fx = setup();
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    // The orchestrator runs (standalone + effective passes) and publishes the standalone
    // style/metadata diagnostics — here the non-canonical lowercase `возврат` keyword.
    assert!(
        diags.iter().any(|d| d.code == ide::DiagnosticCode::CanonicalSpellingKeywords),
        "standalone style diagnostics must survive the orchestrator; got {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>(),
    );

    // No inference false positive is published for the change-and-validate body: the copied
    // base reference resolves through the merge, the missing `НетТакого()` does not surface
    // as a method on a configured type (no real config), and the inserted region carries no
    // spurious `UnresolvedMethodCall`. (Remap of a real inserted diagnostic is covered by
    // the `remap_inserted` unit tests + `infer_effective_*` integration tests.)
    assert!(
        !diags.iter().any(|d| d.code == ide::DiagnosticCode::UnresolvedMethodCall),
        "no spurious UnresolvedMethodCall must be published for the merged body; got {:?}",
        diags.iter().map(|d| (d.code, d.range)).collect::<Vec<_>>(),
    );
}

#[test]
fn weaving_interceptor_signature_mismatch_is_reported() {
    let fx = setup_with(SIG_BASE, SIG_EXT_BAD);
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    assert!(
        diags.iter().any(|d| d.code == ide::DiagnosticCode::WeavingSignatureMismatch),
        "a &Перед interceptor declaring fewer parameters than the base method must be \
         reported; got {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>(),
    );
    // The base config file itself never pairs for weaving, so it carries no such diagnostic.
    let base_diags = fx.analysis.diagnostics(fx.main_file, &DiagnosticsConfig::all_enabled());
    assert!(
        !base_diags.iter().any(|d| d.code == ide::DiagnosticCode::WeavingSignatureMismatch),
        "the base module must not produce a weaving signature diagnostic",
    );
}

#[test]
fn weaving_interceptor_matching_signature_is_clean() {
    let fx = setup_with(SIG_BASE, SIG_EXT_GOOD);
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    assert!(
        !diags.iter().any(|d| d.code == ide::DiagnosticCode::WeavingSignatureMismatch),
        "a &Вместо interceptor whose signature matches the base function (including Знач) must \
         not be flagged; got {:?}",
        diags.iter().map(|d| (d.code, d.range)).collect::<Vec<_>>(),
    );
}

#[test]
fn weaving_before_after_on_function_is_not_applicable() {
    let fx = setup_with(SIG_BASE, SIG_EXT_FUNC_AFTER);
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    assert!(
        diags.iter().any(|d| d.code == ide::DiagnosticCode::WeavingAnnotationNotApplicable),
        "a &После interceptor targeting a base function must be reported as not applicable; \
         got {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>(),
    );
    // The applicability error owns the report — the parameter-count divergence must not also
    // surface as a signature mismatch on the same method.
    assert!(
        !diags.iter().any(|d| d.code == ide::DiagnosticCode::WeavingSignatureMismatch),
        "an inapplicable annotation must suppress the signature diagnostic; got {:?}",
        diags.iter().map(|d| (d.code, d.range)).collect::<Vec<_>>(),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn proceed_with_call_wrong_arg_count_is_reported() {
    let fx = setup_with(PROCEED_BASE, PROCEED_EXT_BAD);
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    // The only call in the interceptor is `ПродолжитьВызов(А)`; it re-enters the two-parameter
    // base method with one argument → a mismatched argument count.
    assert!(
        diags.iter().any(|d| d.code == ide::DiagnosticCode::MismatchedArgCount),
        "ПродолжитьВызов passing too few arguments for the base method must be reported; got {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>(),
    );
}

#[test]
fn proceed_with_call_matching_arg_count_is_clean() {
    let fx = setup_with(PROCEED_BASE, PROCEED_EXT_GOOD);
    let diags = fx.analysis.diagnostics(fx.ext_file, &DiagnosticsConfig::all_enabled());

    assert!(
        !diags.iter().any(|d| d.code == ide::DiagnosticCode::MismatchedArgCount),
        "ПродолжитьВызов passing the base method's full argument list must not be flagged; \
         got {:?}",
        diags.iter().map(|d| (d.code, d.range)).collect::<Vec<_>>(),
    );
}

#[test]
fn goto_definition_on_weaving_annotation_jumps_to_base_method() {
    let fx = setup_with(SIG_BASE, SIG_EXT_GOOD);
    // Cursor on the `Вычислить` name inside the `&Вместо("...")` annotation string (its first
    // occurrence in the ext text is the annotation argument, before the prefixed method name).
    let offset = SIG_EXT_GOOD.find("Вычислить").expect("annotation target present") as u32;

    let target = fx
        .analysis
        .goto_definition(fx.ext_file, offset)
        .expect("goto-definition must resolve the annotation target to the base method");
    assert_eq!(target.file_id, fx.main_file, "must navigate to the base module file");
    assert_eq!(target.name, "Вычислить");
}

#[test]
fn goto_definition_on_change_and_validate_annotation_jumps_to_base_method() {
    // `&ИзменениеИКонтроль("Цель")` is not a weaving interceptor but still names a base method;
    // goto-definition resolves it through the same path.
    let fx = setup();
    let offset = EXT.find("Цель").expect("annotation target present") as u32;

    let target = fx
        .analysis
        .goto_definition(fx.ext_file, offset)
        .expect("goto-definition must resolve the change-and-validate target to the base method");
    assert_eq!(target.file_id, fx.main_file, "must navigate to the base module file");
    assert_eq!(target.name, "Цель");
}

// ---------------------------------------------------------------------------
// Adopted common module: cross-module qualified calls.
//
// A module adopted by an extension shares its base module's name. Qualified
// calls must see the base body from anywhere, the extension's additions only
// from that extension's own code:
//   - base-config caller → base methods resolve, extension-added methods stay
//     unresolved (the extension can be detached at any time);
//   - the adopting extension's caller → base methods AND its own additions;
//   - a sibling extension's caller → base methods only.

const ADOPTED_BASE: &str = "Функция БазовыйМетод() Экспорт\n\tВозврат 1;\nКонецФункции";
const ADOPTED_EXT: &str = "&Вместо(\"БазовыйМетод\")\nФункция X_БазовыйМетод()\n\tВозврат ПродолжитьВызов();\nКонецФункции\n\nФункция ДобавленныйМетод() Экспорт\n\tВозврат 2;\nКонецФункции";
const ADOPTED_CALLER: &str = "Процедура Проверка() Экспорт\n\tА = М.БазовыйМетод();\n\tБ = М.ДобавленныйМетод();\nКонецПроцедуры";

struct AdoptedFixture {
    analysis: Analysis,
    base_caller: FileId,
    own_ext_caller: FileId,
    other_ext_caller: FileId,
}

// The metadata loader discovers a common module only when both the sibling
// `<name>.xml` and the `<name>/Ext/Module.bsl` body exist on disk, so the body
// text is written to the filesystem as well as into the VFS.
fn write_common_module(root: &std::path::Path, name: &str, uuid_tail: &str, body: &str) {
    std::fs::create_dir_all(root.join(format!("CommonModules/{name}/Ext"))).unwrap();
    std::fs::write(root.join(format!("CommonModules/{name}/Ext/Module.bsl")), body).unwrap();
    std::fs::write(
        root.join(format!("CommonModules/{name}.xml")),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-0000000000{uuid_tail}">
        <Properties>
            <Name>{name}</Name>
            <Global>false</Global>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        ),
    )
    .unwrap();
}

fn setup_adopted() -> AdoptedFixture {
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_x_root = temp.path().join("src/cfe/X");
    let ext_y_root = temp.path().join("src/cfe/Y");
    let write_config = |root: &std::path::Path, name: &str, modules: &[&str]| {
        std::fs::create_dir_all(root).unwrap();
        let children = modules
            .iter()
            .map(|m| format!("            <CommonModule>{m}</CommonModule>\n"))
            .collect::<String>();
        std::fs::write(
            root.join("Configuration.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>{name}</Name>
        </Properties>
        <ChildObjects>
{children}        </ChildObjects>
    </Configuration>
</MetaDataObject>"#,
            ),
        )
        .unwrap();
    };
    write_config(&main_root, "Тестовая", &["М", "Вызывающий"]);
    write_config(&ext_x_root, "X", &["М", "СвойМодульИкс"]);
    write_config(&ext_y_root, "Y", &["ЧужойМодульИгрек"]);
    std::mem::forget(temp);

    write_common_module(&main_root, "М", "01", ADOPTED_BASE);
    write_common_module(&main_root, "Вызывающий", "02", ADOPTED_CALLER);
    write_common_module(&ext_x_root, "М", "03", ADOPTED_EXT);
    write_common_module(&ext_x_root, "СвойМодульИкс", "04", ADOPTED_CALLER);
    write_common_module(&ext_y_root, "ЧужойМодульИгрек", "05", ADOPTED_CALLER);

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_x_root.clone()),
        (Some("Y".to_string()), ext_y_root.clone()),
    ]);

    // The extension's adopted body is inserted BEFORE the base body — the
    // adversarial order for a last-write-wins name index.
    let ext_module = FileId(0);
    let base_module = FileId(1);
    let base_caller = FileId(2);
    let own_ext_caller = FileId(3);
    let other_ext_caller = FileId(4);
    let mut file_set = FileSet::default();
    let insert = |fs: &mut FileSet, id, path: std::path::PathBuf| {
        fs.insert(id, VfsPath::new(path.to_string_lossy().as_ref()));
    };
    insert(&mut file_set, ext_module, ext_x_root.join("CommonModules/М/Ext/Module.bsl"));
    insert(&mut file_set, base_module, main_root.join("CommonModules/М/Ext/Module.bsl"));
    insert(&mut file_set, base_caller, main_root.join("CommonModules/Вызывающий/Ext/Module.bsl"));
    insert(
        &mut file_set,
        own_ext_caller,
        ext_x_root.join("CommonModules/СвойМодульИкс/Ext/Module.bsl"),
    );
    insert(
        &mut file_set,
        other_ext_caller,
        ext_y_root.join("CommonModules/ЧужойМодульИгрек/Ext/Module.bsl"),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for id in [ext_module, base_module, base_caller, own_ext_caller, other_ext_caller] {
        db.set_file_source_root(id, SourceRootId(0));
    }
    db.set_file_text(ext_module, ADOPTED_EXT);
    db.set_file_text(base_module, ADOPTED_BASE);
    db.set_file_text(base_caller, ADOPTED_CALLER);
    db.set_file_text(own_ext_caller, ADOPTED_CALLER);
    db.set_file_text(other_ext_caller, ADOPTED_CALLER);

    AdoptedFixture {
        analysis: Analysis::from_database(db),
        base_caller,
        own_ext_caller,
        other_ext_caller,
    }
}

fn unresolved_method_messages(analysis: &Analysis, file: FileId) -> Vec<String> {
    analysis
        .diagnostics(file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|d| d.code == ide::DiagnosticCode::UnresolvedMethodCall)
        .map(|d| d.message)
        .collect()
}

#[test]
fn base_caller_resolves_base_method_of_adopted_module() {
    let fx = setup_adopted();
    let unresolved = unresolved_method_messages(&fx.analysis, fx.base_caller);
    assert!(
        !unresolved.iter().any(|m| m.contains("БазовыйМетод")),
        "a base-only exported method must resolve from base-config code even though an \
         extension adopts the module; got {unresolved:?}",
    );
}

#[test]
fn base_caller_does_not_see_extension_added_method() {
    let fx = setup_adopted();
    let unresolved = unresolved_method_messages(&fx.analysis, fx.base_caller);
    assert!(
        unresolved.iter().any(|m| m.contains("ДобавленныйМетод")),
        "an extension-added export must stay unresolved for base-config code; got {unresolved:?}",
    );
}

#[test]
fn adopting_extension_caller_sees_base_and_own_added_methods() {
    let fx = setup_adopted();
    let unresolved = unresolved_method_messages(&fx.analysis, fx.own_ext_caller);
    assert!(
        unresolved.is_empty(),
        "the adopting extension's code must see both the base method and its own added \
         method; got {unresolved:?}",
    );
}

#[test]
fn sibling_extension_caller_sees_base_but_not_foreign_added_method() {
    let fx = setup_adopted();
    let unresolved = unresolved_method_messages(&fx.analysis, fx.other_ext_caller);
    assert!(
        !unresolved.iter().any(|m| m.contains("БазовыйМетод")),
        "the base method must resolve from a sibling extension; got {unresolved:?}",
    );
    assert!(
        unresolved.iter().any(|m| m.contains("ДобавленныйМетод")),
        "another extension's added export must stay unresolved for a sibling extension; \
         got {unresolved:?}",
    );
}

/// Пара base/расширение, где написание конвенционных сегментов у расширения
/// отличается регистром: связывание обязано найти нижнерегистровую базу.
fn setup_with_rel_paths(base_rel: &str, ext_rel: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();
    std::mem::forget(temp);

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![
        (None, main_root.clone()),
        (Some("X".to_string()), ext_root.clone()),
    ]);

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::default();
    file_set.insert(main_file, VfsPath::new(main_root.join(base_rel).to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_root.join(ext_rel).to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));
    db.set_file_text(main_file, BASE);
    db.set_file_text(ext_file, EXT);

    Fixture { analysis: Analysis::from_database(db), main_file, ext_file }
}

#[test]
fn a_case_variant_extension_module_pairs_to_its_base() {
    let fx =
        setup_with_rel_paths("CommonModules/М/Ext/Module.bsl", "CommonModules/М/EXT/MODULE.BSL");
    let db = fx.analysis.database();
    assert!(
        ide_db::effective_target(db, fx.ext_file).is_some(),
        "конвенционные сегменты расширения в верхнем регистре связываются с базой"
    );
    assert!(
        ide_db::weaving_target(db, fx.ext_file).is_some(),
        "обе независимые функции связывания обязаны видеть пару"
    );
}

/// Позиция имени объекта — точная: модуль объекта `м` не связывается с базовым
/// объектом `М`, даже когда всё остальное совпадает.
#[test]
fn an_object_name_case_variant_never_pairs() {
    let fx =
        setup_with_rel_paths("CommonModules/М/Ext/Module.bsl", "CommonModules/м/Ext/Module.bsl");
    let db = fx.analysis.database();
    assert!(
        ide_db::effective_target(db, fx.ext_file).is_none(),
        "объект м — не объект М: регистр имени значим"
    );
    assert!(ide_db::weaving_target(db, fx.ext_file).is_none());
}
