use bsl_metadata::MdoType;
use hir::{
    Builders, HirDatabase, InferenceDiagnostic, MetadataKind, TypeId, TypeKernelDb, TypeKind,
    UnresolvedMethodKind,
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
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn assert_metadata_ref(
    db: &RootDatabaseImpl,
    actual: Option<TypeId>,
    kind: MetadataKind,
    name: &str,
) {
    let actual = actual.expect("expected metadata ref type");
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

const OBJECT_RETURNING_MODULE: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникОбъект.Справочник1
Функция Объект() Экспорт
    Возврат Неопределено;
КонецФункции

"#;

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_full_tabular_section_chain() {
    let fixture = format!(
        r#"{OBJECT_RETURNING_MODULE}
//- /test.bsl
Функция Тест()
    ОбъектСпр   = ПервыйОбщийМодуль.Объект();
    Тч          = ОбъектСпр.ТабличнаяЧасть1;
    НоваяСтрока = Тч.Добавить();
    Кол         = НоваяСтрока.Реквизит2;
    НомСтр      = НоваяСтрока.НомерСтроки;
    Возврат Кол;
КонецФункции
"#
    );
    let (db, file_id) = setup(&fixture);

    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "объектспр"),
        MetadataKind::CatalogObject,
        "Справочник1",
    );
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "тч"),
        MetadataKind::TabularSection { parent: MdoType::Catalog },
        "Справочник1.ТабличнаяЧасть1",
    );
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "новаястрока"),
        MetadataKind::TabularSectionRow { parent: MdoType::Catalog },
        "Справочник1.ТабличнаяЧасть1",
    );
    assert_eq!(
        var_ty(&db, file_id, "кол"),
        Some(db.number(None, None)),
        "row attribute Реквизит2 must resolve to Number",
    );
    assert_eq!(
        var_ty(&db, file_id, "номстр"),
        Some(db.number(None, None)),
        "platform standard row property НомерСтроки must resolve to Number",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_tabular_section_count_returns_number() {
    let fixture = format!(
        r#"{OBJECT_RETURNING_MODULE}
//- /test.bsl
Функция Тест()
    ОбъектСпр = ПервыйОбщийМодуль.Объект();
    Кол       = ОбъектСпр.ТабличнаяЧасть1.Количество();
    Возврат Кол;
КонецФункции
"#
    );
    let (db, file_id) = setup(&fixture);
    assert_eq!(var_ty(&db, file_id, "кол"), Some(db.number(None, None)));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn infer_tabular_section_unload_returns_value_table() {
    let fixture = format!(
        r#"{OBJECT_RETURNING_MODULE}
//- /test.bsl
Функция Тест()
    ОбъектСпр = ПервыйОбщийМодуль.Объект();
    ТЗ        = ОбъектСпр.ТабличнаяЧасть1.Выгрузить();
    Возврат ТЗ;
КонецФункции
"#
    );
    let (db, file_id) = setup(&fixture);
    let actual = var_ty(&db, file_id, "тз").expect("ТЗ must be inferred");
    assert!(
        matches!(db.lookup_type(actual), TypeKind::ValueTable(facet) if facet.projection.is_none()),
        "Выгрузить() must return unprojected ValueTable, got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn unresolved_method_call_fires_on_tabular_section_typo() {
    let fixture = format!(
        r#"{OBJECT_RETURNING_MODULE}
//- /test.bsl
Процедура Тест()
    ОбъектСпр = ПервыйОбщийМодуль.Объект();
    ОбъектСпр.ТабличнаяЧасть1.НетТакогоМетодаНаТЧ();
КонецПроцедуры
"#
    );
    let (db, file_id) = setup(&fixture);
    let infer = db.infer(file_id);
    let unresolved: Vec<_> = infer
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                receiver_name, method_name, kind, ..
            } => Some((receiver_name.clone(), method_name.clone(), *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        unresolved.len(),
        1,
        "exactly one UnresolvedMethodCall must fire on a TS typo, got {unresolved:?}",
    );
    let (receiver, method, kind) = &unresolved[0];
    assert_eq!(method.as_str(), "НетТакогоМетодаНаТЧ");
    assert!(matches!(kind, UnresolvedMethodKind::MethodNotFound));
    assert_eq!(
        receiver.as_str(),
        "Справочники.Справочник1.ТабличнаяЧасть1",
        "receiver must render as <Plural>.<MdoName>.<Section>",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn no_unresolved_method_call_on_valid_tabular_section_method() {
    let fixture = format!(
        r#"{OBJECT_RETURNING_MODULE}
//- /test.bsl
Процедура Тест()
    ОбъектСпр = ПервыйОбщийМодуль.Объект();
    ОбъектСпр.ТабличнаяЧасть1.Добавить();
КонецПроцедуры
"#
    );
    let (db, file_id) = setup(&fixture);
    let infer = db.infer(file_id);
    let unresolved: Vec<_> = infer
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedMethodCall { .. }))
        .collect();
    assert!(
        unresolved.is_empty(),
        "valid TS method must not trigger UnresolvedMethodCall, got {unresolved:?}",
    );
}
