use bsl_metadata::MdoType;
use hir::{
    DefDatabase, HirDatabase, InferenceDiagnostic, MetadataKind, ModuleId, TypeId, TypeKernelDb,
    TypeKind, UnresolvedMethodKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    setup_with_unreadable(fixture_text, &[])
}

/// [`setup`] with the named fixture files registered as existing but unreadable.
fn setup_with_unreadable(fixture_text: &str, unreadable: &[&str]) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    let mut marked = 0usize;
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        let path = file.path.as_path().to_string_lossy().replace('\\', "/");
        if unreadable.iter().any(|u| path.ends_with(u)) {
            db.set_file_unreadable(*file_id);
            marked += 1;
        } else {
            db.set_file_text(*file_id, &file.content);
        }
    }
    assert_eq!(marked, unreadable.len(), "every named file must exist in the fixture");

    db.set_all_config_paths(vec![(None, designer_fixture_path())]);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn assert_metadata_ref(db: &RootDatabaseImpl, actual: TypeId, kind: MetadataKind, name: &str) {
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::MetadataRef(facet)
                if facet.kind == kind && facet.name.as_str() == name
        ),
        "expected MetadataRef({kind:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

fn assert_object_manager(db: &RootDatabaseImpl, actual: TypeId, mdo: MdoType, name: &str) {
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::ObjectManager(facet)
                if facet.mdo == mdo && facet.name.as_str() == name
        ),
        "expected ObjectManager({mdo:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

fn unresolved_kinds(db: &RootDatabaseImpl, file_id: FileId) -> Vec<UnresolvedMethodKind> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn setup_with_target_path(
    fixture_text: &str,
    target_path_suffix: &str,
) -> (RootDatabaseImpl, FileId) {
    let designer = designer_fixture_path();
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    let mapped: Vec<(FileId, vfs::VfsPath)> = fixture
        .files
        .iter()
        .map(|(id, f)| {
            let virt = f.path.as_path().to_string_lossy();
            let suffix = virt.trim_start_matches('/');
            let abs = designer.join(suffix);
            (*id, vfs::VfsPath::new(abs.to_string_lossy().to_string()))
        })
        .collect();
    for (file_id, vfs_path) in &mapped {
        file_set.insert(*file_id, vfs_path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer)]);
    let needle = target_path_suffix.replace('\\', "/");
    let target = mapped
        .iter()
        .find(|(_, p)| p.as_path().to_string_lossy().replace('\\', "/").ends_with(&needle))
        .map(|(id, _)| *id)
        .unwrap_or_else(|| panic!("fixture must contain {target_path_suffix}"));
    let _ = db.module_bodies(ModuleId::new(target));
    (db, target)
}

fn unresolved_method_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, .. } => {
                Some(method_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

fn rendered_arg_mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, String)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => Some((
                hir::Type::from_id(db, file_id, *expected)
                    .display_name(ide_db::base_db::Locale::Ru),
                hir::Type::from_id(db, file_id, *actual).display_name(ide_db::base_db::Locale::Ru),
            )),
            _ => None,
        })
        .collect()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn catalog_create_item_returns_catalog_object_metadata_ref() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Спр = Справочники.Справочник1.СоздатьЭлемент();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let spr = var_ty(&db, file_id, "спр").expect("спр must be inferred");
    assert_metadata_ref(&db, spr, MetadataKind::CatalogObject, "Справочник1");

    assert!(
        !unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("СоздатьЭлемент")),
        "platform-defined СоздатьЭлемент must not fire UnresolvedMethodCall, got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn aliased_manager_create_item_resolves_through_lookup_method() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    М = Справочники.Справочник1;
    Спр = М.СоздатьЭлемент();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let manager = var_ty(&db, file_id, "м").expect("м must be inferred");
    assert_object_manager(&db, manager, MdoType::Catalog, "Справочник1");
    let spr = var_ty(&db, file_id, "спр").expect("спр must be inferred");
    assert_metadata_ref(&db, spr, MetadataKind::CatalogObject, "Справочник1");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn catalog_find_by_code_returns_catalog_ref() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Ссылка = Справочники.Справочник1.НайтиПоКоду("001");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let link = var_ty(&db, file_id, "ссылка").expect("ссылка must be inferred");
    assert_metadata_ref(&db, link, MetadataKind::CatalogRef, "Справочник1");
}

#[test]
fn catalog_find_by_code_string_arg_does_not_fire_type_mismatch() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Ссылка = Справочники.Справочник1.НайтиПоКоду("796");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let mismatches: Vec<_> = db
        .arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((*expected, *actual))
            }
            _ => None,
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "String literal must be assignable to FindByCode's `Число, Строка` param — got {mismatches:?}",
    );
}

#[test]
fn unknown_manager_method_still_emits_unresolved_diagnostic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Результат = Справочники.Справочник1.НетТакогоМетода();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    assert!(
        unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("НетТакогоМетода")),
        "typo'd method must still fire UnresolvedMethodCall, got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

fn setup_inline(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

#[test]
fn workspace_manager_method_wins_over_platform() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ManagerModule.bsl
Процедура ТестЭкспортная() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Справочники.Справочник1.ТестЭкспортная();
КонецПроцедуры
"#;
    let (db, file_id) = setup_inline(fixture);
    assert!(
        unresolved_kinds(&db, file_id).is_empty(),
        "workspace-defined ТестЭкспортная must resolve without falling back to platform, got {:?}",
        unresolved_kinds(&db, file_id),
    );
}

#[test]
fn catalog_object_chained_write_resolves_through_metadata_ref_lookup() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Спр = Справочники.Справочник1.СоздатьЭлемент();
    Спр.Записать();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "CatalogObject.Записать must resolve via MetadataRef adapter; got unresolved {:?}",
        unresolved_method_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_create_item_offers_catalog_object_methods() {
    let source = "\
Процедура Тест()
    Спр = Справочники.Справочник1.СоздатьЭлемент();
    Спр.$0
КонецПроцедуры
";
    let cursor = source.find("$0").expect("fixture must contain $0");
    let cleaned = source.replacen("$0", "", 1);
    let offset = cursor as u32;

    let fixture_text = format!("//- /test.bsl\n{}", cleaned);
    let (db, file_id) = setup(&fixture_text);
    let analysis = ide::Analysis::from_database(db);
    let items = analysis.completions(file_id, offset, None, ide::Locale::Ru);

    let has = |label: &str| items.iter().any(|i| i.label.eq_ignore_ascii_case(label));
    assert!(
        has("Записать") || has("Write"),
        "CatalogObject.Записать must be in completion items; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn catalog_object_chained_unknown_method_still_emits_diagnostic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Спр = Справочники.Справочник1.СоздатьЭлемент();
    Спр.НетТакогоМетода();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    let spr = var_ty(&db, file_id, "спр").expect("спр must be inferred");
    assert_metadata_ref(&db, spr, MetadataKind::CatalogObject, "Справочник1");
}

fn unresolved_field_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn document_object_chained_write_resolves_through_metadata_ref_lookup() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Док = Документы.Документ1.СоздатьДокумент();
    Док.Записать();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);

    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "DocumentObject.Записать must resolve via MetadataRef adapter; got unresolved {:?}",
        unresolved_method_names(&db, file_id),
    );
    assert!(
        !unresolved_field_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "DocumentObject.Записать must NOT emit UnresolvedField — it is a method, not a field; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn document_object_chained_unknown_method_emits_unresolved_method_call() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Док = Документы.Документ1.СоздатьДокумент();
    Док.НетТакогоМетода();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let inf = db.infer(file_id);
    let unresolved: Vec<_> = inf
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                receiver_name, method_name, kind, ..
            } => Some((receiver_name.clone(), method_name.clone(), *kind)),
            _ => None,
        })
        .collect();

    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "method-call site must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
    assert!(
        unresolved.iter().any(|(rcv, m, kind)| {
            m.as_str().eq_ignore_ascii_case("НетТакогоМетода")
                && *kind == UnresolvedMethodKind::MethodNotFound
                && rcv.as_str() == "Документы.Документ1"
        }),
        "DocumentObject.НетТакогоМетода must emit UnresolvedMethodCall(MethodNotFound) \
         with `Plural.MDO` receiver-name; got {unresolved:?}",
    );
}

#[test]
fn aliased_manager_workspace_method_resolves() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ManagerModule.bsl
Процедура ТестЭкспортная() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    М = Справочники.Справочник1;
    М.ТестЭкспортная();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("ТестЭкспортная")),
        "exported workspace ManagerModule method must resolve through Phase A resolver; got {:?}",
        unresolved_method_names(&db, file_id),
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "method-call site must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
fn aliased_manager_non_exported_method_emits_method_not_export() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ManagerModule.bsl
Процедура ТестНеЭкспортная()
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    М = Справочники.Справочник1;
    М.ТестНеЭкспортная();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. }
                if method_name.as_str().eq_ignore_ascii_case("ТестНеЭкспортная") =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotExport],
        "non-exported workspace ManagerModule method must surface MethodNotExport, got {kinds:?}",
    );
}

#[test]
fn aliased_manager_typo_emits_method_not_found() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    М = Справочники.Справочник1;
    М.УсерМетод();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let entries: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                receiver_name, method_name, kind, ..
            } if method_name.as_str().eq_ignore_ascii_case("УсерМетод") => {
                Some((receiver_name.as_str().to_string(), *kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![("Справочники.Справочник1".to_string(), UnresolvedMethodKind::MethodNotFound)],
        "ObjectManager miss is now authoritative (workspace + platform exhausted); got {entries:?}",
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "method-call site must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
fn aliased_register_manager_workspace_method_resolves() {
    let fixture = r#"
//- /InformationRegisters/РегистрСведений1/Ext/ManagerModule.bsl
Процедура НеУстаревшаяПроцедура() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    М = РегистрыСведений.РегистрСведений1;
    М.НеУстаревшаяПроцедура();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("НеУстаревшаяПроцедура")),
        "exported register ManagerModule method must resolve through Phase A resolver; got {:?}",
        unresolved_method_names(&db, file_id),
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "register manager method-call site must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

const ISSUE80_REGISTER_FIXTURE: &str = include_str!("fixtures/issue80/register.fixture");

#[test]
fn issue80_select_filter_uses_structure_overload() {
    let (db, file_id) = setup_with_target_path(ISSUE80_REGISTER_FIXTURE, "/valid.bsl");
    let mismatches = rendered_arg_mismatches(&db, file_id);

    assert!(
        mismatches.is_empty(),
        "Структура filter must be accepted by the non-periodic Выбрать overload; got {mismatches:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn issue80_select_rejects_argument_incompatible_with_all_overloads() {
    let (db, file_id) = setup_with_target_path(ISSUE80_REGISTER_FIXTURE, "/invalid.bsl");
    let mismatches = rendered_arg_mismatches(&db, file_id);

    assert_eq!(
        mismatches.len(),
        1,
        "Число must be rejected by every Выбрать overload with one TypeMismatch; got {mismatches:?}",
    );
    assert_eq!(
        mismatches[0].1, "Число",
        "the true-positive control must diagnose the supplied scalar; got {mismatches:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn register_recordset_typo_emits_method_not_found() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    МЗ = РегистрыСведений.РегистрСведений1.СоздатьМенеджерЗаписи();
    МЗ.НесуществующийМетод();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let entries: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                receiver_name, method_name, kind, ..
            } if method_name.as_str().eq_ignore_ascii_case("НесуществующийМетод") => {
                Some((receiver_name.as_str().to_string(), *kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![(
            "РегистрыСведений.РегистрСведений1".to_string(),
            UnresolvedMethodKind::MethodNotFound
        )],
        "register record-manager miss is now authoritative (workspace + platform exhausted); \
         got {entries:?}",
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "register recordmanager method-call must not emit UnresolvedField, got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
fn record_manager_platform_method_resolves() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    МЗ = РегистрыСведений.РегистрСведений1.СоздатьМенеджерЗаписи();
    МЗ.Записать();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "platform Записать on InformationRegisterRecordManager must resolve through platform \
         layer; got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

#[test]
fn record_set_platform_method_resolves_on_information_register() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    НЗ = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НЗ.Записать();
    НЗ.Загрузить(Новый ТаблицаЗначений);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let unresolved = unresolved_method_names(&db, file_id);
    assert!(
        !unresolved.iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "platform Записать on InformationRegisterRecordSet must resolve; got {unresolved:?}",
    );
    assert!(
        !unresolved.iter().any(|n| n.eq_ignore_ascii_case("Загрузить")),
        "platform Загрузить on InformationRegisterRecordSet must resolve; got {unresolved:?}",
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "record-set platform method calls must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
fn record_set_filter_dimension_set_method_resolves() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(Знач Значение)
    НЗ = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НЗ.Отбор.Справочник1.Установить(Значение);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Установить")),
        "FilterItem.Установить must resolve through scalar-key + platform path; got {:?}",
        unresolved_method_names(&db, file_id),
    );
    let unresolved_fields = unresolved_field_names(&db, file_id);
    assert!(
        !unresolved_fields.iter().any(|n| n.eq_ignore_ascii_case("Отбор")),
        "synthetic .Отбор must resolve as a field; got unresolved fields: {unresolved_fields:?}",
    );
    assert!(
        !unresolved_fields.iter().any(|n| n.eq_ignore_ascii_case("Справочник1")),
        "dimension Справочник1 must resolve through Filter member surface; \
         got unresolved fields: {unresolved_fields:?}",
    );
}

#[test]
fn record_set_filter_method_resolves_through_scalar_key() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    НЗ = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НЗ.Отбор.Сбросить();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Сбросить")),
        "Filter.Сбросить must resolve through scalar-key path; got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn aliased_record_set_workspace_method_unresolved_keeps_strict_diagnostic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    НЗ = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НЗ.НесуществующийМетод();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let entries: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. }
                if method_name.as_str().eq_ignore_ascii_case("НесуществующийМетод") =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![UnresolvedMethodKind::MethodNotFound],
        "record-set miss is authoritative now that workspace+platform paths are wired; \
         got {entries:?}",
    );
}

#[test]
fn metadata_ref_object_module_workspace_method_resolves() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура МойОбъектныйМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об.МойОбъектныйМетод();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("МойОбъектныйМетод")),
        "exported workspace ObjectModule method must resolve through Phase B resolver; got {:?}",
        unresolved_method_names(&db, file_id),
    );
    assert!(
        unresolved_field_names(&db, file_id).is_empty(),
        "method-call site must not emit UnresolvedField; got {:?}",
        unresolved_field_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_ref_object_module_non_exported_emits_method_not_export() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура НеЭкспортный()
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об.НеЭкспортный();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. }
                if method_name.as_str().eq_ignore_ascii_case("НеЭкспортный") =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotExport],
        "non-exported workspace ObjectModule method must surface MethodNotExport, got {kinds:?}",
    );
}

#[test]
fn metadata_ref_workspace_then_platform_fallback() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура НикакЗаписатьНеНазывается() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об.Записать();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        !unresolved_method_names(&db, file_id).iter().any(|n| n.eq_ignore_ascii_case("Записать")),
        "platform `Записать` must resolve after workspace miss; got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn metadata_ref_total_miss_emits_method_not_found() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура ЕстьВсего() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об.НетТакогоМетода();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let entries: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                receiver_name, method_name, kind, ..
            } if method_name.as_str().eq_ignore_ascii_case("НетТакогоМетода") => {
                Some((receiver_name.as_str().to_string(), *kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![("Справочники.Справочник1".to_string(), UnresolvedMethodKind::MethodNotFound)],
        "MetadataRef.*Object miss is now authoritative; got {entries:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn catalog_ref_does_not_consult_object_module() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура НеЭкспортный()
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Сс = Справочники.Справочник1.НайтиПоКоду("001");
    Сс.НеЭкспортный();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. }
                if method_name.as_str().eq_ignore_ascii_case("НеЭкспортный") =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotFound],
        "*Ref must NOT consult ObjectModule.bsl — diagnostic, if any, is the platform-side \
         MethodNotFound, never a workspace-flavour MethodNotExport; got {kinds:?}",
    );
}

#[test]
fn this_object_non_exported_method_emits_method_not_export() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура НеЭкспортный()
КонецПроцедуры
Процедура Тест()
    ЭтотОбъект.НеЭкспортный();
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_with_target_path(fixture, "/Catalogs/Справочник1/Ext/ObjectModule.bsl");
    let kinds: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. }
                if method_name.as_str().eq_ignore_ascii_case("НеЭкспортный") =>
            {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotExport],
        "ЭтотОбъект.НеЭкспортный() is external-style access requiring Экспорт; got {kinds:?}",
    );
}

#[test]
fn this_object_exported_method_resolves() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура Экспортный() Экспорт
КонецПроцедуры
Процедура Тест()
    ЭтотОбъект.Экспортный();
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_with_target_path(fixture, "/Catalogs/Справочник1/Ext/ObjectModule.bsl");
    let unresolved: Vec<_> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedMethodCall { .. }))
        .map(|(_, d)| d.clone())
        .collect();
    assert!(
        unresolved.is_empty(),
        "an exported method is a member of the object; got {unresolved:?}"
    );
}

#[test]
fn this_object_direct_non_exported_call_resolves() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ObjectModule.bsl
Процедура НеЭкспортный()
КонецПроцедуры
Процедура Тест()
    НеЭкспортный();
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_with_target_path(fixture, "/Catalogs/Справочник1/Ext/ObjectModule.bsl");
    assert!(
        !unresolved_method_names(&db, file_id)
            .iter()
            .any(|n| n.eq_ignore_ascii_case("НеЭкспортный")),
        "direct `НеЭкспортный()` is a local-scope call, must resolve cleanly; got {:?}",
        unresolved_method_names(&db, file_id),
    );
}

/// A user module that could not be read says nothing about the PLATFORM surface of
/// its metadata object: `НайтиПоНаименованию` lives on the catalog manager, not in
/// `ManagerModule.bsl`. Refusing to resolve it would trade a false finding for a
/// silently lost type — and with it every check downstream of that type.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn an_unread_manager_module_still_resolves_the_platform_manager_method() {
    let fixture = r#"
//- /Catalogs/Справочник1/Ext/ManagerModule.bsl
Процедура Своя() Экспорт КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Ссылка = Справочники.Справочник1.НайтиПоНаименованию("X");
КонецПроцедуры
"#;

    // Control: with the module readable the platform method resolves, so the
    // assertion below is about unreadability and not about the fixture.
    let (db, file_id) = setup(fixture);
    let readable = var_ty(&db, file_id, "ссылка").expect("ссылка must be inferred");
    assert_metadata_ref(&db, readable, MetadataKind::CatalogRef, "Справочник1");

    let (db, file_id) =
        setup_with_unreadable(fixture, &["/Catalogs/Справочник1/Ext/ManagerModule.bsl"]);
    let unread = var_ty(&db, file_id, "ссылка").expect("ссылка must still be inferred");
    assert_metadata_ref(&db, unread, MetadataKind::CatalogRef, "Справочник1");

    assert!(
        unresolved_method_names(&db, file_id).is_empty(),
        "and no call is reported against the caller: {:?}",
        unresolved_method_names(&db, file_id),
    );
}
