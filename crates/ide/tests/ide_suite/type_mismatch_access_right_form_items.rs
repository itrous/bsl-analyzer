//! Type compatibility of configuration access rights and managed form items.
//!
//! - `ПравоДоступа(<Право>, Метаданные)`: the rights of the configuration as a
//!   whole (`Администрирование`, `Вывод`, `ВнешнееСоединение`, …) are asked of
//!   the global `Метаданные`, an `ОбъектМетаданныхКонфигурация`, which the
//!   restored «any metadata object» list of the parameter did not name.
//! - `Элементы.Добавить(…, Элементы.<Подменю>)`, `Элементы.Переместить(…)`: the
//!   form reader knew no `Popup`, `ColumnGroup`, `LabelDecoration`,
//!   `PictureDecoration` and most field tags, so such items of the form were
//!   typed as a bare `ЭлементФормы`, which no item slot accepted. A value
//!   documented as `ЭлементФормы` was rejected the same way.

use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup_loose(text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{text}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    let file_id = *fixture.files.keys().next().expect("one file");
    (db, file_id)
}

fn render(db: &RootDatabaseImpl, ty: TypeId) -> String {
    match db.lookup_type(ty) {
        TypeKind::Union(parts) => {
            parts.iter().map(|part| render(db, *part)).collect::<Vec<_>>().join(" | ")
        }
        TypeKind::PlatformObject(facet) => facet.name.to_string(),
        TypeKind::FormControl { kind, .. } => {
            kind.base_platform_type_name().unwrap_or("ЭлементФормы").to_owned()
        }
        TypeKind::String(_) => "Строка".to_owned(),
        other => format!("{other:?}"),
    }
}

/// `(expected, actual)` of every reported mismatch, rendered.
fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, String)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((render(db, *expected), render(db, *actual)))
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ПравоДоступа and the configuration as a whole
// ---------------------------------------------------------------------------

#[test]
fn access_right_of_the_whole_configuration_is_accepted() {
    let (db, file_id) = setup_loose(
        "Процедура Тест()
    Администратор = ПравоДоступа(\"Администрирование\", Метаданные);
    ПравоВывода = ПравоДоступа(\"Вывод\", Метаданные);
    ДляПользователя = ПравоДоступа(\"Вывод\", Метаданные, ПользователиИнформационнойБазы.ТекущийПользователь());
    ВыполнитьПроверкуПравДоступа(\"Администрирование\", Метаданные);
КонецПроцедуры
",
    );
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn access_right_of_a_description_typed_as_the_bare_family_is_accepted() {
    // A guard: the list now names one type outside the family, and a description
    // documented with the bare family name must stay accepted.
    let (db, file_id) = setup_loose(
        "// Параметры:
//  МетаданныеРеквизита - ОбъектМетаданных - реквизит, право на который проверяется.
Функция ЕстьПравоЧтения(МетаданныеРеквизита) Экспорт
    Возврат ПравоДоступа(\"Чтение\", МетаданныеРеквизита);
КонецФункции
",
    );
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn access_right_of_a_string_is_still_a_mismatch() {
    // The platform rejects it: «Несоответствие типов (параметр номер '2')».
    let (db, file_id) = setup_loose(
        "Процедура Тест()
    Право = ПравоДоступа(\"Вывод\", \"Метаданные\");
КонецПроцедуры
",
    );
    let found = mismatches(&db, file_id);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, "Строка", "{found:?}");
}

// ---------------------------------------------------------------------------
// Form items whose tags the form reader did not know
// ---------------------------------------------------------------------------

/// A synthetic catalog form carrying the item tags of a real dump.
fn form_config(root: &Path) -> PathBuf {
    let form_dir = root.join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext");
    std::fs::create_dir_all(form_dir.join("Form")).expect("create form directory");
    std::fs::write(
        root.join("Configuration.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>Конфигурация1</Name>
        </Properties>
        <ChildObjects>
            <Catalog>Справочник1</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#,
    )
    .expect("write Configuration.xml");
    std::fs::write(
        root.join("Catalogs/Справочник1.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="22222222-2222-2222-2222-222222222222">
        <Properties>
            <Name>Справочник1</Name>
            <CodeLength>9</CodeLength>
            <DescriptionLength>25</DescriptionLength>
        </Properties>
        <ChildObjects>
            <Form>ФормаЭлемента</Form>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#,
    )
    .expect("write catalog XML");
    std::fs::write(
        root.join("Catalogs/Справочник1/Forms/ФормаЭлемента.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Form uuid="33333333-3333-3333-3333-333333333333">
        <Properties>
            <Name>ФормаЭлемента</Name>
            <FormType>Managed</FormType>
        </Properties>
    </Form>
</MetaDataObject>"#,
    )
    .expect("write form descriptor");
    std::fs::write(
        form_dir.join("Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <ChildItems>
        <CommandBar name="КоманднаяПанель" id="9">
            <ChildItems>
                <Popup name="Подменю" id="1"/>
            </ChildItems>
        </CommandBar>
        <UsualGroup name="Группа" id="2">
            <ChildItems>
                <LabelDecoration name="Надпись" id="3"/>
                <PictureDecoration name="Картинка" id="4"/>
                <InputField name="Поле" id="5"/>
                <HTMLDocumentField name="ПолеHTML" id="6"/>
                <FutureWidget name="НовыйЭлемент" id="10"/>
            </ChildItems>
        </UsualGroup>
        <Table name="Таблица" id="7">
            <ChildItems>
                <ColumnGroup name="ГруппаКолонок" id="8"/>
            </ChildItems>
        </Table>
    </ChildItems>
</Form>"#,
    )
    .expect("write Form.xml");
    root.join("Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl")
}

fn setup_form(body: &str) -> (TempDir, RootDatabaseImpl, FileId) {
    setup_form_module(&format!("&НаСервере\nПроцедура Тест()\n{body}КонецПроцедуры\n"))
}

fn setup_form_module(text: &str) -> (TempDir, RootDatabaseImpl, FileId) {
    let temp = tempfile::tempdir().expect("tempdir");
    let module = form_config(temp.path());
    std::fs::write(&module, text).expect("write module");
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(vec![(None, temp.path().to_path_buf())], None);
    (temp, db, file_id)
}

fn var_kind(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> String {
    let ty = db.infer(file_id).var_types.get(var_lower).copied().unwrap_or_else(|| {
        let known: Vec<_> = db.infer(file_id).var_types.keys().cloned().collect();
        panic!("`{var_lower}` must be inferred, have {known:?}")
    });
    render(db, ty)
}

#[test]
fn items_of_every_dump_tag_get_their_kind() {
    let (_temp, db, file_id) = setup_form(
        "    Подменю = Элементы.Подменю;
    ГруппаКолонок = Элементы.ГруппаКолонок;
    Надпись = Элементы.Надпись;
    Картинка = Элементы.Картинка;
    ПолеHTML = Элементы.ПолеHTML;
",
    );
    for (var, kind) in [
        ("подменю", "ГруппаФормы"),
        ("группаколонок", "ГруппаФормы"),
        ("надпись", "ДекорацияФормы"),
        ("картинка", "ДекорацияФормы"),
        ("полеhtml", "ПолеФормы"),
    ] {
        assert_eq!(var_kind(&db, file_id, var), kind, "`{var}`");
    }
}

#[test]
fn a_submenu_and_a_column_group_are_parents() {
    let (_temp, db, file_id) = setup_form(
        "    Кнопка = Элементы.Добавить(\"Кнопка\", Тип(\"КнопкаФормы\"), Элементы.Подменю);
    Колонка = Элементы.Добавить(\"Колонка\", Тип(\"ПолеФормы\"), Элементы.ГруппаКолонок);
    Элементы.Переместить(Элементы.Поле, Элементы.ГруппаКолонок);
",
    );
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn decorations_are_moved_and_serve_as_the_item_before() {
    let (_temp, db, file_id) = setup_form(
        "    Элементы.Переместить(Элементы.Надпись, Элементы.Группа, Элементы.Картинка);
    Элементы.Переместить(Элементы.ПолеHTML, Элементы.Группа, Элементы.Надпись);
",
    );
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn a_parent_given_by_name_or_a_field_is_still_a_mismatch() {
    // The platform rejects a name with a type mismatch, and a field or decoration
    // with an invalid-value error: none is a valid parent.
    let (_temp, db, file_id) = setup_form(
        "    ПоИмени = Элементы.Добавить(\"Кнопка1\", Тип(\"КнопкаФормы\"), \"Подменю\");
    ВПоле = Элементы.Добавить(\"Кнопка2\", Тип(\"КнопкаФормы\"), Элементы.Поле);
    ВНадпись = Элементы.Добавить(\"Кнопка3\", Тип(\"КнопкаФормы\"), Элементы.Надпись);
",
    );
    let actual: Vec<String> = mismatches(&db, file_id).into_iter().map(|(_, a)| a).collect();
    assert_eq!(actual, ["Строка", "ПолеФормы", "ДекорацияФормы"]);
}

#[test]
fn an_item_of_unknown_kind_is_accepted_where_some_kind_is() {
    // A tag the reader does not map leaves the item's kind unknown: it may be a
    // group, so it is no reason to report the call.
    let (_temp, db, file_id) = setup_form(
        "    Элементы.Переместить(Элементы.НовыйЭлемент, Элементы.Группа, Элементы.Надпись);
    Кнопка = Элементы.Добавить(\"Кнопка\", Тип(\"КнопкаФормы\"), Элементы.НовыйЭлемент);
",
    );
    assert_eq!(var_kind(&db, file_id, "кнопка"), "КнопкаФормы");
    assert_eq!(mismatches(&db, file_id), Vec::<(String, String)>::new());
}

#[test]
fn an_item_documented_as_a_form_item_is_accepted() {
    // A helper documented with the base type: the kind of what it returns is
    // unknown, and the caller passes it where a group is expected.
    let (_temp, db, file_id) = setup_form_module(
        "// Возвращаемое значение:
//  ЭлементФормы - найденный элемент.
&НаСервере
Функция НайтиЭлемент(Имя)
    Возврат Элементы.Найти(Имя);
КонецФункции

&НаСервере
Процедура Тест()
    Родитель = НайтиЭлемент(\"Группа\");
    Элементы.Переместить(Элементы.Поле, Родитель);
    Кнопка = Элементы.Добавить(\"Кнопка\", Тип(\"КнопкаФормы\"), Родитель);
    Число = Окр(Родитель);
КонецПроцедуры
",
    );
    assert_eq!(var_kind(&db, file_id, "родитель"), "ЭлементФормы");
    let actual: Vec<String> = mismatches(&db, file_id).into_iter().map(|(_, a)| a).collect();
    assert_eq!(actual, ["ЭлементФормы"], "only the number slot reports the item");
}
