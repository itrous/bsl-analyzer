//! `Имя(...)` that resolves nowhere, and the surfaces that must be cleared first.
//!
//! The implicit receiver is the one the bare cascade never consults on its own:
//! inside an object or record-set module `ЭтоНовый()` / `Загрузить()` are
//! `ЭтотОбъект.<метод>()` with the receiver left out, and on a real configuration
//! that class outnumbered every true finding 110 to 7.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// Every body that enters the GLOBAL context of the designer fixture. Without them
/// in the VFS the global surface is unread, the miss is `Indeterminate`, and the
/// rule says nothing — which is the certainty model working, not the stand.
const GLOBAL_SURFACE: &[&str] = &[
    "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl",
    "Ext/ManagedApplicationModule.bsl",
    "Ext/ExternalConnectionModule.bsl",
    "Ext/SessionModule.bsl",
];

fn setup_at(relative: &str, text: &str) -> (RootDatabaseImpl, FileId) {
    setup_at_with(relative, text, &[])
}

/// Like [`setup_at`], with more fixture files in the workspace under the given text.
fn setup_at_with(
    relative: &str,
    text: &str,
    siblings: &[(&str, &str)],
) -> (RootDatabaseImpl, FileId) {
    let path = designer_fixture_path().join(relative);
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));

    let mut extra = Vec::new();
    for (index, global) in GLOBAL_SURFACE.iter().enumerate() {
        if *global == relative {
            continue;
        }
        let global_path = designer_fixture_path().join(global);
        let Ok(text) = std::fs::read_to_string(&global_path) else { continue };
        let global_id = FileId::from_raw(100 + index as u32);
        file_set.insert(global_id, VfsPath::new(global_path.to_string_lossy().to_string()));
        extra.push((global_id, text));
    }

    for (index, (sibling, sibling_text)) in siblings.iter().enumerate() {
        let sibling_path = designer_fixture_path().join(sibling);
        let sibling_id = FileId::from_raw(200 + index as u32);
        file_set.insert(sibling_id, VfsPath::new(sibling_path.to_string_lossy().to_string()));
        extra.push((sibling_id, (*sibling_text).to_string()));
    }

    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    for (global_id, global_text) in extra {
        db.set_file_source_root(global_id, SourceRootId(0));
        db.set_file_text(global_id, &global_text);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

fn absent_bare_calls(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedBareCall { name, .. } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect()
}

const COMMON_MODULE: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";
const CATALOG_OBJECT_MODULE: &str = "Catalogs/Справочник1/Ext/ObjectModule.bsl";
const CATALOG_MANAGER_MODULE: &str = "Catalogs/Справочник1/Ext/ManagerModule.bsl";
const REGISTER_RECORD_SET_MODULE: &str =
    "InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl";
const DATA_PROCESSOR_FORM_MODULE: &str =
    "DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl";

#[test]
fn a_name_owned_by_nothing_is_reported_in_a_common_module() {
    let text = "Процедура Тест()\n    СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(COMMON_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

#[test]
fn a_platform_global_and_a_sibling_method_are_silent() {
    let text = "Процедура Сосед()\nКонецПроцедуры\n\n\
                Процедура Тест()\n    Сосед();\n    Результат = СтрДлина(\"x\");\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(COMMON_MODULE, text);
    assert!(absent_bare_calls(&db, file_id).is_empty(), "resolved bare calls must stay silent");
}

/// A variable does not own a call name: `Имя(...)` with only a parameter, a `Перем` or
/// a module `Перем` of that name in scope does not compile — "Процедура или функция с
/// указанным именем не определена" (checked live on 8.3.17 and 8.3.27).
#[test]
fn a_name_only_a_variable_holds_is_still_absent() {
    let text = "Перем МодульнаяПеременная;\n\n\
                Процедура ЧерезПараметр(ТолькоПараметр)\n    ТолькоПараметр();\nКонецПроцедуры\n\n\
                Процедура ЧерезПерем()\n    Перем ТолькоПерем;\n    ТолькоПерем = 1;\n    \
                Результат = ТолькоПерем();\nКонецПроцедуры\n\n\
                Процедура ЧерезМодульную()\n    МодульнаяПеременная();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(COMMON_MODULE, text);
    assert_eq!(
        absent_bare_calls(&db, file_id),
        vec![
            "ТолькоПараметр".to_string(),
            "ТолькоПерем".to_string(),
            "МодульнаяПеременная".to_string()
        ]
    );
}

/// The other half of the same fact: a variable named like a sibling, a global export or
/// a platform function leaves the call to that method, which resolves.
#[test]
fn a_variable_named_like_a_method_leaves_the_call_resolved() {
    let text = "Перем СтрДлина;\n\n\
                Процедура Сосед()\nКонецПроцедуры\n\n\
                Процедура Тест(Сосед, Формат)\n    Перем ГлобальнаяСервернаяПроцедура;\n    \
                Сосед();\n    ГлобальнаяСервернаяПроцедура();\n    \
                Результат = Формат(1, \"ЧГ=0\") + СтрДлина(\"x\");\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(COMMON_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "calls owned by methods must stay silent: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

/// `ЭтоНовый()` / `Записать()` in an object module are `ЭтотОбъект.<метод>()`
/// with the receiver left out — the platform surface behind the implicit
/// receiver has to be cleared before a name is called absent.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn implicit_object_methods_are_not_absent() {
    let text = "Процедура Тест()\n    Если ЭтоНовый() Тогда\n        Записать();\n    \
                КонецЕсли;\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CATALOG_OBJECT_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "implicit ЭтотОбъект methods must resolve: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

#[test]
fn an_object_module_still_reports_a_name_owned_by_nothing() {
    let text = "Процедура Тест()\n    СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CATALOG_OBJECT_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn implicit_record_set_methods_are_not_absent() {
    let text = "Процедура Тест()\n    Загрузить(Неопределено);\n    Записать();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(REGISTER_RECORD_SET_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "implicit record-set methods must resolve: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn implicit_manager_methods_are_not_absent() {
    let text = "Процедура Тест()\n    Результат = СоздатьЭлемент();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CATALOG_MANAGER_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "implicit manager methods must resolve: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

const DOCUMENT_FORM_MODULE: &str = "Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl";
const CATALOG_LIST_FORM_MODULE: &str = "Catalogs/Справочник1/Forms/ФормаСписка/Ext/Form/Module.bsl";
const RECORD_FORM_MODULE: &str =
    "InformationRegisters/РегистрСведений1/Forms/ФормаЗаписи/Ext/Form/Module.bsl";
/// A managed form with no main attribute.
const PLAIN_FORM_MODULE: &str = "Catalogs/рдт_Рецептура/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
const ORDINARY_FORM_MODULE: &str = "Catalogs/Справочник1/Forms/ФормаОбычная/Ext/Form/Module.bsl";
const CONSTANT_VALUE_MANAGER_MODULE: &str = "Constants/СтрокаКонст/Ext/ValueManagerModule.bsl";

#[test]
fn a_managed_form_module_reports_a_name_owned_by_nothing() {
    let text = "&НаКлиенте\nПроцедура Тест()\n    СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(DATA_PROCESSOR_FORM_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

/// The managed form is the implicit receiver: its own platform methods are written
/// without `ЭтаФорма`, next to the module's methods and the global context.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn managed_form_methods_siblings_and_globals_are_silent() {
    let text = "&НаКлиенте\nПроцедура Сосед()\nКонецПроцедуры\n\n\
                &НаКлиенте\nПроцедура Тест()\n    Сосед();\n    Закрыть();\n    \
                ОбновитьОтображениеДанных();\n    \
                ПодключитьОбработчикОжидания(\"Сосед\", 1, Истина);\n    \
                Close();\n    Результат = СтрДлина(\"x\");\n    ПоказатьПредупреждение(, \"x\");\n\
                КонецПроцедуры\n\n\
                &НаСервере\nПроцедура НаСервере()\n    \
                Значение = РеквизитФормыВЗначение(\"Объект\");\n    \
                ЗначениеВРеквизитФормы(Значение, \"Объект\");\n\
                КонецПроцедуры\n";
    let (db, file_id) = setup_at(DATA_PROCESSOR_FORM_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "form, sibling and global calls must resolve: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

/// A local reset by an untyped write, a local that holds a form method's name and a
/// local that holds a sibling function's name leave the form surface as it was: the
/// form's own methods stay silent, a name owned by nothing is still reported.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn form_methods_stay_silent_past_reset_and_same_named_locals() {
    let text = "&НаКлиенте
Функция Сосед()
    Возврат 1;
КонецФункции

&НаКлиенте
Процедура Тест(Параметр)
    Результат = 1;
    Результат = Параметр.Что;
    Закрыть(Результат);
    ОбновитьОтображениеДанных = Истина;
    ОбновитьОтображениеДанных();
    Сосед = Сосед();
    Сосед = Сосед() + 1;
    СовсемНеизвестныйВызов();
КонецПроцедуры
";
    let (db, file_id) = setup_at(DATA_PROCESSOR_FORM_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

/// The main attribute mixes its own extension into the form: a document form can
/// `Записать()`, a form without a main attribute only has the form itself.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn the_main_attribute_extension_is_part_of_the_receiver() {
    let text = "&НаКлиенте\nПроцедура Тест()\n    Записать();\n    Прочитать();\n    \
                Ссылка = ПолучитьНавигационнуюСсылкуОбъекта();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(DOCUMENT_FORM_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "the document-form extension must resolve: {:?}",
        absent_bare_calls(&db, file_id)
    );

    let (db, file_id) = setup_at(PLAIN_FORM_MODULE, text);
    assert_eq!(
        absent_bare_calls(&db, file_id),
        vec![
            "Записать".to_string(),
            "Прочитать".to_string(),
            "ПолучитьНавигационнуюСсылкуОбъекта".to_string()
        ],
        "a form without a main attribute has no object extension"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_list_form_has_the_dynamic_list_extension_not_the_object_one() {
    let text = "&НаКлиенте\nПроцедура Тест()\n    \
                Ссылка = ПолучитьНавигационнуюСсылкуСписка();\n    Записать();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CATALOG_LIST_FORM_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["Записать".to_string()]);
}

/// A record form's `Запись` is an information register record manager, and its type
/// names the form's extension exactly: the record extension's methods stay silent,
/// while the object and list extensions' methods and a name owned by nothing are
/// reported.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_record_form_has_the_record_extension_not_the_object_or_list_one() {
    let text = "&НаКлиенте\nПроцедура Тест()\n    Записать();\n    Прочитать();\n    \
                Ссылка = ПолучитьНавигационнуюСсылкуЗаписи();\n    \
                Ссылка = ПолучитьНавигационнуюСсылкуОбъекта();\n    \
                Ссылка = ПолучитьНавигационнуюСсылкуСписка();\n    \
                СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(RECORD_FORM_MODULE, text);
    assert_eq!(
        absent_bare_calls(&db, file_id),
        vec![
            "ПолучитьНавигационнуюСсылкуОбъекта".to_string(),
            "ПолучитьНавигационнуюСсылкуСписка".to_string(),
            "СовсемНеизвестныйВызов".to_string()
        ]
    );
}

#[test]
fn a_global_common_module_export_is_silent_in_a_form_module() {
    let text =
        "&НаСервере\nПроцедура Тест()\n    ГлобальнаяСервернаяПроцедура();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(DATA_PROCESSOR_FORM_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "a global export is callable bare from a form: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

/// A procedure without a directive runs on the server only, so its `#Если Клиент`
/// branch is compiled nowhere and cannot fail the module; the same branch in a
/// client procedure is compiled and still reported.
#[test]
fn a_branch_no_environment_compiles_stays_silent() {
    let text = "Функция НаСервере()\n    #Если Клиент Тогда\n        НетНаСервере();\n    \
                #КонецЕсли\n    Возврат 1;\nКонецФункции\n\n\
                &НаКлиенте\nПроцедура НаКлиенте()\n    #Если Клиент Тогда\n        \
                ЕстьНаКлиенте();\n    #КонецЕсли\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(DATA_PROCESSOR_FORM_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["ЕстьНаКлиенте".to_string()]);
}

/// The bare calls an ordinary form module reaches: the form type (`Закрыть`,
/// `Обновить`), its extensions (`ЗаписатьВФорме`), the platform type of the object its
/// MAIN ATTRIBUTE holds (`ПолучитьМакет`) and the exports of that object's module.
/// Only the name none of them owns is absent.
const ORDINARY_FORM_CALLS: &str = "Процедура Тест()
    СовсемНеизвестныйВызов();
        Закрыть();
    Обновить();
    ЗаписатьВФорме();
    ПолучитьМакет();
        ЭкспортнаяПроцедураОбъекта();
КонецПроцедуры
";

const DATA_PROCESSOR_OBJECT_MODULE: &str = "DataProcessors/ТестоваяОбработка/Ext/ObjectModule.bsl";
/// Both forms live under `Справочник1`; the sidecar of the first one names the data
/// processor as the main attribute's type, the second one's is identical without the
/// `type` and `main` keys.
const TYPED_ORDINARY_FORM_MODULE: &str =
    "Catalogs/Справочник1/Forms/ФормаОбычнаяСТипом/Ext/Form/Module.bsl";
const UNTYPED_ORDINARY_FORM_MODULE: &str =
    "Catalogs/Справочник1/Forms/ФормаОбычнаяБезТипа/Ext/Form/Module.bsl";

const DATA_PROCESSOR_EXPORT: &str = "Процедура ЭкспортнаяПроцедураОбъекта() Экспорт
КонецПроцедуры
";

#[test]
fn an_ordinary_form_is_judged_against_its_main_attributes_object() {
    let (db, file_id) = setup_at_with(
        TYPED_ORDINARY_FORM_MODULE,
        ORDINARY_FORM_CALLS,
        &[(DATA_PROCESSOR_OBJECT_MODULE, DATA_PROCESSOR_EXPORT)],
    );
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

/// The same form without `type`/`main` in its sidecar: nothing is known about the
/// owner, so nothing is reported - a guess here is a false finding per call.
#[test]
fn an_ordinary_form_without_a_typed_main_attribute_stays_silent() {
    let (db, file_id) = setup_at_with(
        UNTYPED_ORDINARY_FORM_MODULE,
        ORDINARY_FORM_CALLS,
        &[(DATA_PROCESSOR_OBJECT_MODULE, DATA_PROCESSOR_EXPORT)],
    );
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "unknown owner must keep the silence: {:?}",
        absent_bare_calls(&db, file_id)
    );
}

/// The export lives in the module of the path owner, not of the main attribute's
/// object: the owner of the path must not be consulted.
#[test]
fn the_path_owner_is_not_the_form_owner() {
    let text = "Процедура Тест()
    ЭкспортПутевогоВладельца();
КонецПроцедуры
";
    let (db, file_id) = setup_at_with(
        TYPED_ORDINARY_FORM_MODULE,
        text,
        &[(
            CATALOG_OBJECT_MODULE,
            "Процедура ЭкспортПутевогоВладельца() Экспорт
КонецПроцедуры
",
        )],
    );
    assert_eq!(absent_bare_calls(&db, file_id), vec!["ЭкспортПутевогоВладельца".to_string()]);
}

/// A form with no sidecar at all keeps its silence as well.
#[test]
fn an_ordinary_form_without_a_sidecar_stays_silent() {
    let (db, file_id) = setup_at(ORDINARY_FORM_MODULE, ORDINARY_FORM_CALLS);
    assert!(absent_bare_calls(&db, file_id).is_empty());
}

fn absent_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedName { name, .. } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect()
}

const DATA_PROCESSOR_MANAGER_MODULE: &str =
    "DataProcessors/ТестоваяОбработка/Ext/ManagerModule.bsl";
const TYPED_READ_FORM_MODULE: &str =
    "Catalogs/Справочник1/Forms/ФормаОбычнаяЧтениеСТипом/Ext/Form/Module.bsl";
const UNTYPED_READ_FORM_MODULE: &str =
    "Catalogs/Справочник1/Forms/ФормаОбычнаяЧтениеБезТипа/Ext/Form/Module.bsl";

/// One read per surface, each owned by exactly one of them, plus two names nothing
/// owns: a form attribute, an attribute and a tabular section of the main
/// attribute's object, an export of its object module and of its manager module,
/// and platform properties of the form type and of a form extension.
const ORDINARY_FORM_READS: &str = "Процедура Тест()
    А = СовсемНеизвестноеИмя;
    Б = РеквизитФормы;
    В = АдресСайта;
    Г = НастройкиЭксель;
    Д = ЭкспортнаяПроцедураОбъекта;
    Е = ЭкспортМенеджера;
    Ж = Модифицированность;
    З = ЭтаФорма;
    И = ПараметрОснование;
    К = ЕщёОдноНеизвестноеИмя;
КонецПроцедуры
";

const DATA_PROCESSOR_MANAGER_EXPORT: &str = "Процедура ЭкспортМенеджера() Экспорт
КонецПроцедуры
";

fn ordinary_read_siblings() -> [(&'static str, &'static str); 2] {
    [
        (DATA_PROCESSOR_OBJECT_MODULE, DATA_PROCESSOR_EXPORT),
        (DATA_PROCESSOR_MANAGER_MODULE, DATA_PROCESSOR_MANAGER_EXPORT),
    ]
}

#[test]
fn an_ordinary_form_read_resolves_the_names_its_surface_owns() {
    let (db, file_id) =
        setup_at_with(TYPED_READ_FORM_MODULE, ORDINARY_FORM_READS, &ordinary_read_siblings());
    assert_eq!(
        absent_names(&db, file_id),
        vec!["СовсемНеизвестноеИмя".to_string(), "ЕщёОдноНеизвестноеИмя".to_string()]
    );
}

/// The control of the test above: the same module and workspace, the sidecar without
/// `type` and `main`. The form's own attribute is resolved from its sidecar whatever the
/// main attribute is, so it is no longer reported; what this control pins is that no
/// OWNER-dependent name is resolved by a guess.
#[test]
fn an_ordinary_form_read_without_a_typed_main_attribute_is_judged_as_before() {
    let (db, file_id) =
        setup_at_with(UNTYPED_READ_FORM_MODULE, ORDINARY_FORM_READS, &ordinary_read_siblings());
    assert_eq!(
        absent_names(&db, file_id),
        [
            "СовсемНеизвестноеИмя",
            "АдресСайта",
            "НастройкиЭксель",
            "ЭкспортнаяПроцедураОбъекта",
            "ЭкспортМенеджера",
            "Модифицированность",
            "ЭтаФорма",
            "ПараметрОснование",
            "ЕщёОдноНеизвестноеИмя",
        ]
        .map(String::from)
        .to_vec()
    );
}

#[test]
fn an_ordinary_form_read_without_a_sidecar_is_judged_as_before() {
    let (db, file_id) =
        setup_at_with(ORDINARY_FORM_MODULE, ORDINARY_FORM_READS, &ordinary_read_siblings());
    let names = absent_names(&db, file_id);
    assert_eq!(names.len(), 10, "{names:?}");
}

/// The read surface must not move a call verdict: a bare CALL of a manager export is
/// still judged by the call channel, which does not count the manager module in.
#[test]
fn a_bare_call_of_a_manager_export_is_still_reported() {
    let text = "Процедура Тест()
    ЭкспортМенеджера();
КонецПроцедуры
";
    let (db, file_id) = setup_at_with(TYPED_READ_FORM_MODULE, text, &ordinary_read_siblings());
    assert_eq!(absent_bare_calls(&db, file_id), vec!["ЭкспортМенеджера".to_string()]);
}

/// The standard attributes of the main attribute's object are read bare too; the
/// neighbouring name nothing owns is still reported.
#[test]
fn an_ordinary_form_read_resolves_the_standard_attributes_of_its_object() {
    let text = "Процедура Тест()
    А = Ссылка;
    Б = Код;
    В = Наименование;
    Г = ПометкаУдаления;
    Д = СовсемНеизвестноеИмя;
КонецПроцедуры
";
    let (db, file_id) = setup_at(
        "Catalogs/Справочник1/Forms/ФормаОбычнаяЧтениеСправочника/Ext/Form/Module.bsl",
        text,
    );
    assert_eq!(absent_names(&db, file_id), vec!["СовсемНеизвестноеИмя".to_string()]);
}

fn unresolved_method_calls(db: &RootDatabaseImpl, file_id: FileId) -> Vec<String> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedMethodCall { receiver_name, method_name, .. } => {
                Some(format!("{}.{}", receiver_name.as_str(), method_name.as_str()))
            }
            _ => None,
        })
        .collect()
}

const COLLISION_FORM_MODULE: &str =
    "Catalogs/Справочник1/Forms/ФормаОбычнаяКоллизия/Ext/Form/Module.bsl";

/// A tabular section named like a global metadata collection is that section inside an
/// ordinary form module, so a member called on it is not judged against the collection
/// manager (which has no `НайтиСтроки`).
const COLLISION_CALLS: &str = "Процедура Тест()
    А = Документы.НайтиСтроки(Новый Структура(\"Значение\", 1));
    Б = Справочники.НайтиСтроки(Новый Структура(\"Значение\", 1));
    В = Отчеты.Количество();
    Г = Документы;
КонецПроцедуры
";

#[test]
fn a_tabular_section_named_like_a_collection_wins_over_the_collection() {
    let (db, file_id) = setup_at(COLLISION_FORM_MODULE, COLLISION_CALLS);
    assert_eq!(unresolved_method_calls(&db, file_id), Vec::<String>::new());
    assert!(absent_names(&db, file_id).is_empty(), "{:?}", absent_names(&db, file_id));
}

/// Control: the same module over a form whose sidecar has no `type`/`main`. The owner is
/// unknown, so the names keep resolving to the global collections and nothing changes.
#[test]
fn a_collection_named_member_without_a_typed_main_attribute_is_judged_as_before() {
    let (db, file_id) = setup_at(UNTYPED_READ_FORM_MODULE, COLLISION_CALLS);
    assert_eq!(unresolved_method_calls(&db, file_id), Vec::<String>::new());
    assert!(absent_names(&db, file_id).is_empty());
}

/// A constant's value-manager module hangs off `КонстантаМенеджерЗначения`:
/// `Записать()` / `Прочитать()` are its methods written without the receiver.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn a_constant_value_manager_module_resolves_its_receiver() {
    let text = "Процедура Тест()\n    Прочитать();\n    Записать();\n    \
                СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(CONSTANT_VALUE_MANAGER_MODULE, text);
    assert_eq!(absent_bare_calls(&db, file_id), vec!["СовсемНеизвестныйВызов".to_string()]);
}

const FORMS: &str = "Catalogs/Справочник1/Forms";

fn form_module(form: &str) -> String {
    format!("{FORMS}/{form}/Ext/Form/Module.bsl")
}

/// One read per surface of a LIST form. Owned by the list type the catalog files
/// under the main attribute's type (`Отбор`, `Порядок`, `Колонки`), by the form type
/// (`Модифицированность`, `ЭтаФорма`, `ВладелецФормы`), by a form extension
/// (`ПараметрОтборПоРегистратору`) and by the form's own attributes. Not owned: an
/// unknown name, a name the platform lists for no form or list type
/// (`ТекущаяСтрока`), and `Наименование` - an attribute of the listed catalog,
/// which a list is not.
const LIST_FORM_READS: &str = "Процедура Тест()
    А = СовсемНеизвестноеИмя;
    Б = РеквизитФормы;
    В = Отбор;
    Г = Порядок;
    Д = Колонки;
    Е = Модифицированность;
    Ж = ЭтаФорма;
    З = ВладелецФормы;
    И = ПараметрОтборПоРегистратору;
    К = Наименование;
    Л = ТекущаяСтрока;
КонецПроцедуры
";

fn reported(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

const ALL_LIST_READS: [&str; 10] = [
    "СовсемНеизвестноеИмя",
    "Отбор",
    "Порядок",
    "Колонки",
    "Модифицированность",
    "ЭтаФорма",
    "ВладелецФормы",
    "ПараметрОтборПоРегистратору",
    "Наименование",
    "ТекущаяСтрока",
];

#[test]
fn a_list_form_read_resolves_the_list_and_form_surface_and_not_the_listed_object() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаСТипом"), LIST_FORM_READS);
    assert_eq!(
        absent_names(&db, file_id),
        reported(&["СовсемНеизвестноеИмя", "Наименование", "ТекущаяСтрока"])
    );
}

/// The English spelling of a list type, in a notation the object spellings of the
/// same sidecar reader never use.
#[test]
fn a_list_form_with_an_english_list_type_resolves_the_same_surface() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаАнглийская"), LIST_FORM_READS);
    assert_eq!(
        absent_names(&db, file_id),
        reported(&["СовсемНеизвестноеИмя", "Наименование", "ТекущаяСтрока"])
    );
}

/// The control of the list tests: the same module over the sidecar that differs only
/// in the missing `type` and `main` keys. The form's own attribute is resolved from its
/// sidecar whatever the main attribute is, so it is no longer reported; every other name is
/// reported as before.
#[test]
fn a_list_form_without_a_main_attribute_is_judged_as_before() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаБезТипа"), LIST_FORM_READS);
    assert_eq!(absent_names(&db, file_id), reported(&ALL_LIST_READS));
}

/// A plain value as the main attribute names no list: the surface stays unreadable. The
/// form's own attribute is resolved from its sidecar whatever the main attribute is, so it
/// is no longer reported.
#[test]
fn a_list_form_whose_main_attribute_is_a_plain_string_is_judged_as_before() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаСтрока"), LIST_FORM_READS);
    assert_eq!(absent_names(&db, file_id), reported(&ALL_LIST_READS));
}

/// A bare CALL keeps its earlier verdict on a list form: silent, as for an unreadable
/// surface. The surface built for reads must not start judging calls.
#[test]
fn a_bare_call_in_a_list_form_stays_silent() {
    let text = "Процедура Тест()
    СовсемНеизвестныйВызов();
КонецПроцедуры
";
    let (db, file_id) = setup_at(&form_module("ФормаСпискаСТипом"), text);
    assert!(absent_bare_calls(&db, file_id).is_empty());
    let (db, file_id) = setup_at(&form_module("ФормаСпискаБезТипа"), text);
    assert!(absent_bare_calls(&db, file_id).is_empty());
}

/// A list form's own attribute named like a global metadata collection is that
/// attribute: a member called on it is not judged against the collection manager
/// (which has no `НайтиСтроки`).
const LIST_COLLISION_CALLS: &str = "Процедура Тест()
    А = Справочники.НайтиСтроки(Новый Структура(\"Значение\", 1));
    Б = Отчеты.Количество();
    В = Справочники;
    Г = СовсемНеизвестноеИмя;
КонецПроцедуры
";

#[test]
fn a_list_form_attribute_named_like_a_global_collection_wins_as_a_call_receiver() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаКоллизия"), LIST_COLLISION_CALLS);
    assert_eq!(unresolved_method_calls(&db, file_id), Vec::<String>::new());
    assert_eq!(absent_names(&db, file_id), reported(&["СовсемНеизвестноеИмя"]));
}

/// The control of the collision test: the same module and the same typed main
/// attribute over a sidecar WITHOUT the two attributes named like collections. The
/// surface owns neither name, so both resolve to the global collections as before and
/// the unknown name is still reported. (With the surface not consulted by the
/// receiver dispatch, the test above reports `Справочники.НайтиСтроки` and
/// `Отчеты.Количество` - checked by disabling that one consultation.)
#[test]
fn a_collection_name_the_list_surface_does_not_own_is_still_the_collection() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаСТипом"), LIST_COLLISION_CALLS);
    assert_eq!(unresolved_method_calls(&db, file_id), Vec::<String>::new());
    assert_eq!(absent_names(&db, file_id), reported(&["СовсемНеизвестноеИмя"]));
}

/// Reads of a register form. The register of the fixture is not periodic, so `Период`
/// is owned by nothing; its dimension `Справочник1` is a field of the register, and
/// `Отбор` is a property the platform lists for a record SET but not for a record
/// manager.
const REGISTER_FORM_READS: &str = "Процедура Тест()
    А = СовсемНеизвестноеИмя;
    Б = РеквизитФормы;
    В = Справочник1;
    Г = Период;
    Д = Отбор;
    Е = Модифицированность;
КонецПроцедуры
";

#[test]
fn a_record_manager_form_read_resolves_the_registers_fields() {
    let (db, file_id) = setup_at(&form_module("ФормаЗаписиРегистра"), REGISTER_FORM_READS);
    assert_eq!(absent_names(&db, file_id), reported(&["СовсемНеизвестноеИмя", "Период", "Отбор"]));
}

#[test]
fn a_record_set_form_read_resolves_the_registers_fields_in_both_notations() {
    for form in ["ФормаНабораРегистра", "ФормаНабораАнглийская"]
    {
        let (db, file_id) = setup_at(&form_module(form), REGISTER_FORM_READS);
        assert_eq!(
            absent_names(&db, file_id),
            reported(&["СовсемНеизвестноеИмя", "Период"]),
            "{form}"
        );
    }
}

/// The control of the register tests: the same module over the sidecar without `type`
/// and `main`. The form's own attribute is resolved from its sidecar whatever the main
/// attribute is, so it is no longer reported; every other name is reported as before.
#[test]
fn a_register_form_without_a_main_attribute_is_judged_as_before() {
    let (db, file_id) = setup_at(&form_module("ФормаСпискаБезТипа"), REGISTER_FORM_READS);
    assert_eq!(
        absent_names(&db, file_id),
        reported(&["СовсемНеизвестноеИмя", "Справочник1", "Период", "Отбор", "Модифицированность"])
    );
}
