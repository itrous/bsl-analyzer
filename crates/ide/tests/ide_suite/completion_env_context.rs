//! Completion must not offer what the availability diagnostics would
//! immediately underline: candidates are filtered by the execution
//! environments of the code at the cursor, with `#Если` narrowing restoring
//! suggestions inside matching branches.

use ide::{Analysis, CompletionItem};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root);

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;
    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file = (abs_idx - file_offset_in_prefix) as u32;
    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn has_label(items: &[CompletionItem], label: &str) -> bool {
    items.iter().any(|i| i.label == label)
}

fn platform_data_available() -> bool {
    !bsl_platform::PlatformDataInner::instance().all_global_functions().is_empty()
}

#[test]
fn server_global_function_hidden_in_client_method() {
    if !platform_data_available() {
        return;
    }
    // ЗаписьЖурналаРегистрации is unavailable on the thin and web clients.
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сохранить()
    ЗаписьЖурнал$0
КонецПроцедуры
"#,
    );
    assert!(
        !has_label(&items, "ЗаписьЖурналаРегистрации"),
        "a client method must not offer the server-side global function"
    );
}

#[test]
fn server_global_function_offered_in_server_method() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Записать()
    ЗаписьЖурнал$0
КонецПроцедуры
"#,
    );
    assert!(
        has_label(&items, "ЗаписьЖурналаРегистрации"),
        "the server method must keep the suggestion"
    );
}

#[test]
fn universal_global_function_offered_everywhere() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сохранить()
    Сообщ$0
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "Сообщить"), "universally available functions stay suggested");
}

#[test]
fn web_incapable_constructor_hidden_in_client_method() {
    if !platform_data_available() {
        return;
    }
    // ЧтениеТекста cannot be constructed in the web client, and a form's
    // client method compiles for the web client too.
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    Чтение = Новый ЧтениеТек$0
КонецПроцедуры
"#,
    );
    assert!(
        !has_label(&items, "ЧтениеТекста"),
        "the constructor list must not offer a type the diagnostic would underline"
    );
}

#[test]
fn preprocessor_guard_restores_constructor_suggestion() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если НЕ ВебКлиент Тогда
    Чтение = Новый ЧтениеТек$0
    #КонецЕсли
КонецПроцедуры
"#,
    );
    assert!(
        has_label(&items, "ЧтениеТекста"),
        "the guard excludes the web client, so the suggestion returns"
    );
}

#[test]
fn constructor_offered_in_server_method() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Прочитать()
    Чтение = Новый ЧтениеТек$0
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "ЧтениеТекста"), "the server side admits the constructor");
}

#[test]
fn module_without_metadata_stays_suggested() {
    // The in-memory fixture provides no CommonModule XML: unknown flags must
    // never silently hide a module.
    let items = complete(
        r#"
//- /CommonModules/МойМодуль/Ext/Module.bsl
Процедура Внутренняя() Экспорт
КонецПроцедуры
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сохранить()
    МойМод$0
КонецПроцедуры
"#,
    );
    assert!(has_label(&items, "МойМодуль"), "a module with unknown flags must stay suggested");
}

#[test]
fn unfinished_server_method_at_eof_keeps_suggestion() {
    if !platform_data_available() {
        return;
    }
    // While typing a new method at the end of the file the cursor sits at the
    // method's current end — it must still count as inside that method.
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Тест()
    ЗаписьЖурнал$0"#,
    );
    assert!(
        has_label(&items, "ЗаписьЖурналаРегистрации"),
        "an unfinished server method must keep server suggestions"
    );
}

#[test]
fn client_only_local_method_hidden_in_server_method() {
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Показать()
КонецПроцедуры

&НаСервере
Процедура Обработать()
    Показ$0
КонецПроцедуры
"#,
    );
    assert!(
        !has_label(&items, "Показать"),
        "server code cannot reach a client-only method — completion must not offer it"
    );
}

#[test]
fn server_local_method_offered_in_client_method() {
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Записать()
КонецПроцедуры

&НаКлиенте
Процедура Сохранить()
    Запис$0
КонецПроцедуры
"#,
    );
    assert!(
        has_label(&items, "Записать"),
        "client-to-server is the form's remote call — the suggestion stays"
    );
}

#[test]
fn weaving_interceptor_not_judged() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /CommonModules/Перехватчик/Ext/Module.bsl
&Вместо("Записать")
Процедура МойПерехват()
    ЗаписьЖурнал$0
КонецПроцедуры
"#,
    );
    assert!(
        has_label(&items, "ЗаписьЖурналаРегистрации"),
        "a weaving interceptor's directive is unknown — availability must not be judged"
    );
}

#[test]
fn metadata_root_members_hidden_in_client_method() {
    if !platform_data_available() {
        return;
    }
    // `Метаданные` is unavailable on the thin and web clients.
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сохранить()
    Метаданные.$0
КонецПроцедуры
"#,
    );
    assert!(
        items.is_empty(),
        "a client method must not offer members behind the unavailable `Метаданные` root; got: {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn metadata_root_members_offered_in_server_method() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура Записать()
    Метаданные.$0
КонецПроцедуры
"#,
    );
    assert!(
        has_label(&items, "Справочники"),
        "the server method must keep the metadata collection suggestions; got: {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn metadata_collection_objects_hidden_in_client_method() {
    if !platform_data_available() {
        return;
    }
    let items = complete(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сохранить()
    Метаданные.Справочники.$0
КонецПроцедуры
"#,
    );
    assert!(
        items.is_empty(),
        "a client method must not offer objects of a metadata collection behind the unavailable root; got: {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );
}
