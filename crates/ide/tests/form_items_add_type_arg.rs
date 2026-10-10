//! A collection that creates items of several kinds returns the kind its
//! `Тип` argument names, not the union of every kind it could create.

use std::path::PathBuf;

use hir::{Builders, HirDatabase, InferenceDiagnostic, TypeId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// A managed form module of the designer fixture: there `Элементы` is the
/// form's real `ВсеЭлементыФормы`, as in every module this change is about.
fn setup(bsl_text: &str) -> (RootDatabaseImpl, FileId) {
    let disk_path = designer_fixture_path()
        .join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl");
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(disk_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl_text);
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    (db, file_id)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> TypeId {
    db.infer(file_id)
        .var_types
        .get(var_lower)
        .copied()
        .unwrap_or_else(|| panic!("`{var_lower}` must be inferred"))
}

fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(TypeId, TypeId)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((*expected, *actual))
            }
            _ => None,
        })
        .collect()
}

fn item(db: &RootDatabaseImpl, name: &str) -> TypeId {
    db.platform_object(name.to_string())
}

fn all_item_kinds(db: &RootDatabaseImpl) -> TypeId {
    db.union(
        ["ДекорацияФормы", "ГруппаФормы", "КнопкаФормы", "ТаблицаФормы", "ПолеФормы"]
            .into_iter()
            .map(|name| item(db, name))
            .collect(),
    )
}

const HEADER: &str = "&НаСервере
Процедура Тест(ТипЭлемента)
";

fn fixture(body: &str) -> String {
    format!("{HEADER}{body}КонецПроцедуры\n")
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn literal_group_type_narrows_and_serves_as_parent() {
    let (db, file_id) = setup(&fixture(
        "    Группа = Элементы.Добавить(\"Группа\", Тип(\"ГруппаФормы\"));
    Поле = Элементы.Добавить(\"Поле\", Тип(\"ПолеФормы\"), Группа);
",
    ));
    assert_eq!(var_ty(&db, file_id, "группа"), item(&db, "ГруппаФормы"));
    assert_eq!(var_ty(&db, file_id, "поле"), item(&db, "ПолеФормы"));
    assert!(mismatches(&db, file_id).is_empty(), "got {:?}", mismatches(&db, file_id));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn literal_field_type_used_as_parent_is_still_a_mismatch() {
    let (db, file_id) = setup(&fixture(
        "    Поле = Элементы.Добавить(\"Поле\", Тип(\"ПолеФормы\"));
    Вложенное = Элементы.Добавить(\"Вложенное\", Тип(\"ПолеФормы\"), Поле);
",
    ));
    let found = mismatches(&db, file_id);
    assert_eq!(found.len(), 1, "a field cannot be a parent; got {found:?}");
    assert_eq!(found[0].1, item(&db, "ПолеФормы"));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn english_type_name_narrows_to_the_same_kind() {
    let (db, file_id) = setup(&fixture(
        "    Группа = Элементы.Вставить(\"Группа\", Type(\"FormGroup\"));
    Кнопка = Элементы.Добавить(\"Кнопка\", Тип(\"FormButton\"), Группа);
",
    ));
    assert_eq!(var_ty(&db, file_id, "группа"), item(&db, "ГруппаФормы"));
    assert_eq!(var_ty(&db, file_id, "кнопка"), item(&db, "КнопкаФормы"));
    assert!(mismatches(&db, file_id).is_empty(), "got {:?}", mismatches(&db, file_id));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn non_literal_type_keeps_the_union_and_is_accepted_as_parent() {
    let (db, file_id) = setup(&fixture(
        "    Родитель = Элементы.Добавить(\"Родитель\", ТипЭлемента);
    Поле = Элементы.Добавить(\"Поле\", Тип(\"ПолеФормы\"), Родитель);
",
    ));
    assert_eq!(var_ty(&db, file_id, "родитель"), all_item_kinds(&db));
    assert!(mismatches(&db, file_id).is_empty(), "got {:?}", mismatches(&db, file_id));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn type_outside_the_documented_kinds_keeps_the_union() {
    let (db, file_id) = setup(&fixture(
        "    Элемент = Элементы.Добавить(\"Элемент\", Тип(\"Массив\"));
",
    ));
    assert_eq!(var_ty(&db, file_id, "элемент"), all_item_kinds(&db));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn shadowed_type_function_does_not_narrow() {
    let text = "&НаСервере
Функция Тип(Имя)
    Возврат Имя;
КонецФункции

&НаСервере
Процедура Тест()
    Элемент = Элементы.Добавить(\"Элемент\", Тип(\"ГруппаФормы\"));
КонецПроцедуры
";
    let (db, file_id) = setup(text);
    assert_eq!(var_ty(&db, file_id, "элемент"), all_item_kinds(&db));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn members_resolve_on_the_narrowed_kind() {
    let (db, file_id) = setup(&fixture(
        "    Поле = Элементы.Добавить(\"Поле\", Тип(\"ПолеФормы\"));
    Путь = Поле.ПутьКДанным;
",
    ));
    assert_eq!(var_ty(&db, file_id, "путь"), db.string(None, false));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn data_composition_collections_follow_the_type_argument_too() {
    let (db, file_id) = setup(
        "&НаСервере
Процедура Тест()
    Настройки = Новый НастройкиКомпоновкиДанных;
    Отбор = Настройки.Отбор.Элементы.Добавить(Тип(\"ЭлементОтбораКомпоновкиДанных\"));
    Группа = Настройки.Отбор.Элементы.Добавить(Тип(\"ГруппаЭлементовОтбораКомпоновкиДанных\"));
КонецПроцедуры
",
    );
    assert_eq!(var_ty(&db, file_id, "отбор"), item(&db, "ЭлементОтбораКомпоновкиДанных"));
    assert_eq!(var_ty(&db, file_id, "группа"), item(&db, "ГруппаЭлементовОтбораКомпоновкиДанных"));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn an_item_of_unknown_kind_is_still_no_number() {
    // The latitude is for a slot that admits some item kinds; a slot that admits
    // none of them still reports the item.
    let (db, file_id) = setup(&fixture(
        "    Элемент = Элементы.Добавить(\"Элемент\", ТипЭлемента);
    Текст = Формат(Элемент, \"ЧГ=0\");
",
    ));
    assert_eq!(var_ty(&db, file_id, "элемент"), all_item_kinds(&db));
    let found = mismatches(&db, file_id);
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].1, all_item_kinds(&db));
}

#[test]
fn a_user_function_with_a_documented_type_parameter_keeps_its_return() {
    let text = "// Параметры:
//  Вид - Тип - вид создаваемого элемента.
// Возвращаемое значение:
//  ГруппаФормы, ПолеФормы - созданный элемент.
&НаСервере
Функция СоздатьЭлемент(Вид)
    Возврат Элементы.Добавить(\"Элемент\", Вид);
КонецФункции

&НаСервере
Процедура Тест()
    Элемент = СоздатьЭлемент(Тип(\"ГруппаФормы\"));
КонецПроцедуры
";
    let (db, file_id) = setup(text);
    assert_eq!(
        var_ty(&db, file_id, "элемент"),
        db.union(vec![item(&db, "ГруппаФормы"), item(&db, "ПолеФормы")])
    );
}

#[test]
fn an_item_from_the_items_loop_is_accepted_before_which_to_insert() {
    // Copying items next to their prototypes: the loop variable may be any item
    // kind, and the analyzer cannot tell which one the author filtered for.
    let (db, file_id) = setup(&fixture(
        "    Для Каждого Элемент Из Элементы Цикл
        Если ТипЗнч(Элемент) = Тип(\"ПолеФормы\") Тогда
            Копия = Элементы.Вставить(Элемент.Имя + \"Копия\", ТипЗнч(Элемент), Элемент.Родитель, Элемент);
        КонецЕсли;
    КонецЦикла;
",
    ));
    assert!(mismatches(&db, file_id).is_empty(), "got {:?}", mismatches(&db, file_id));
}
