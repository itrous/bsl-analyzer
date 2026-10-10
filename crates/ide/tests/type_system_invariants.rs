use hir::{
    Builders, DefDatabase, HirDatabase, MetadataKind, ModuleId, Name, TypeKernelDb, TypeKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
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
fn single_resolver_cascade_across_builtins_locals_and_managers() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    М = Новый Массив();
    К = Документы;
    Возврат К;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let infer = db.infer(file_id);
    let var_ty = |n: &str| infer.var_types.get(n).copied();

    assert_eq!(
        var_ty("м"),
        Some(db.array(None)),
        "`Новый Массив()` must still lower through TyLoweringContext"
    );
    assert_eq!(
        var_ty("к"),
        Some(db.manager_collection(bsl_metadata::MdoType::Document)),
        "`Документы` must still resolve to Ty::ManagerCollection via MdoType::from_plural"
    );
}

#[test]
fn jsdoc_and_three_level_share_signature_materialisation() {
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   Строка - имя
Функция Имя() Экспорт
    Возврат "";
КонецФункции

//- /Documents/ПКО/Ext/ManagerModule.bsl
// Возвращаемое значение:
//   ДокументСсылка.ПКО - ссылка на документ
Функция ПолучитьСсылку() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    А = ОбщегоНазначения.Имя();
    Б = Документы.ПКО.ПолучитьСсылку();
    Возврат Б;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let infer = db.infer(file_id);
    let var_ty = |n: &str| infer.var_types.get(n).copied();

    assert_eq!(
        var_ty("а"),
        Some(db.string(None, false)),
        "2-segment call must materialise signature from JSDoc"
    );
    let doc_ref = var_ty("б").expect("3-segment call must produce a var_type entry");
    assert!(
        matches!(
            db.lookup_type(doc_ref),
            TypeKind::MetadataRef(facet)
                if facet.kind == MetadataKind::DocumentRef && facet.name.as_str() == "ПКО"
        ),
        "3-segment call must materialise signature through the same path, got {:?}",
        db.lookup_type(doc_ref)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn single_method_lookup_path_agrees_across_infer_and_facade() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    А = Новый Массив;
    Б = А.Количество();
    Возврат Б;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let infer = db.infer(file_id);

    let infer_ty = infer
        .var_types
        .get("б")
        .copied()
        .expect("Массив.Количество() must produce a var_type entry");

    let array_id = db.array(None);
    let facade_ret = hir::Type::from_id(&db, file_id, array_id)
        .method_return_type(&Name::new("Количество"))
        .id();

    assert_eq!(
        infer_ty, facade_ret,
        "Expr::MethodCall inference and hir::Type::method_return_type must \
         return the same Ty for `Массив.Количество()` — both must route \
         through `method_lookup::lookup_method`"
    );
    assert_eq!(
        infer_ty,
        db.number(None, None),
        "`Массив.Количество()` must resolve to Ty::Number — a change here \
         means the platform-data index drifted, not a facade regression",
    );
}

#[test]
fn single_field_lookup_path_agrees_across_infer_and_facade() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Р = С.Реквизит2;
    Возврат Р;
КонецФункции
"#;
    let (mut db, file_id) = setup(fixture);
    db.set_all_config_paths(vec![(
        None,
        std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        )),
    )]);

    let infer = db.infer(file_id);
    let infer_ty =
        infer.var_types.get("р").copied().expect("С.Реквизит2 must produce a var_type entry");

    let receiver_id =
        infer.var_types.get("с").copied().expect("С must carry the CatalogRef receiver type");
    let facade_field_id =
        hir::Type::from_id(&db, file_id, receiver_id).field_type(&Name::new("Реквизит2")).id();

    assert_eq!(
        infer_ty, facade_field_id,
        "Expr::Field inference and hir::Type::field_type must return the \
         same Ty for `Справочник1.Реквизит2` — both must route through \
         `field_lookup::lookup_field`"
    );
    assert_eq!(
        infer_ty,
        db.number(None, None),
        "`Справочник1.Реквизит2` must resolve to Ty::Number per the \
         designer fixture XML — drift here indicates the XML changed",
    );
}
