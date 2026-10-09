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

    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    for (global_id, global_text) in extra {
        db.set_file_source_root(global_id, SourceRootId(0));
        db.set_file_text(global_id, &global_text);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
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

/// An ordinary form's module also sees the exports of the main attribute's object
/// module, and its dialog is not read — no absence can be proven inside one.
#[test]
fn an_ordinary_form_module_stays_silent() {
    let text = "Процедура Тест()\n    СовсемНеизвестныйВызов();\nКонецПроцедуры\n";
    let (db, file_id) = setup_at(ORDINARY_FORM_MODULE, text);
    assert!(
        absent_bare_calls(&db, file_id).is_empty(),
        "ordinary form modules keep their silence: {:?}",
        absent_bare_calls(&db, file_id)
    );
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
