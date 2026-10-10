use expect_test::{expect, Expect};
use hir::{Builders, DefDatabase, HirDatabase, ModuleId, Type, TypeId};
use ide::{Analysis, CompletionItem};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

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

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projection_fields_visible_via_hir_type_accessors() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выборка;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "выборка").expect("выборка must be inferred");
    let type_facade = Type::from_id(&db, file_id, ty);
    assert!(type_facade.is_query_projection(), "is_query_projection must return true");
    let fields =
        type_facade.projection_fields().expect("projection_fields must surface the column slice");
    assert_eq!(fields.len(), 1, "single-column SELECT yields one projection field");
    assert_eq!(fields[0].0.as_str(), "Имя");
    assert_eq!(fields[0].1, db.string(None, false));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn projection_fields_surface_in_enumerate_fields() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя, 42 КАК Цена");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выборка;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "выборка").expect("выборка must be inferred");
    let type_facade = Type::from_id(&db, file_id, ty);
    let fields = type_facade.fields();
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"Имя"),
        "enumerate_fields must include projection column `Имя`, got {names:?}",
    );
    assert!(
        names.contains(&"Цена"),
        "enumerate_fields must include projection column `Цена`, got {names:?}",
    );
    let projection_columns: Vec<_> =
        fields.iter().filter(|f| matches!(f.name.as_str(), "Имя" | "Цена")).collect();
    for col in &projection_columns {
        assert!(col.is_readonly, "projection column `{}` must be read-only", col.name.as_str());
    }
}

#[test]
fn structure_literal_keys_surface_in_enumerate_fields() {
    let fixture = r#"//- /test.bsl
Функция Построить()
    Пар = Новый Структура("Город, Индекс");
    Пар.Вставить("Улица");
    Возврат Пар;
КонецФункции

Функция Тест()
    Стр = Построить();
    Возврат Стр;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    // The enriched typed structure flows out of `Построить()` and is stored on the caller's local.
    let ty = var_ty(&db, file_id, "стр").expect("стр must be inferred");
    let fields = Type::from_id(&db, file_id, ty).fields();
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    for key in ["Город", "Индекс", "Улица"] {
        assert!(
            names.contains(&key),
            "enumerate_fields must include literal key `{key}`, got {names:?}"
        );
    }
    for f in fields.iter().filter(|f| matches!(f.name.as_str(), "Город" | "Индекс" | "Улица"))
    {
        assert!(!f.is_readonly, "structure key `{}` must be mutable", f.name.as_str());
    }
}

#[test]
fn structure_key_reseed_after_insert_uses_source_order_last_wins() {
    // `Цена` is written by an insert (Число) and then by a later re-seed (Строка). Source order
    // must win, so the value type is the re-seed's Строка — not the earlier insert's Число.
    let fixture = r#"//- /test.bsl
Функция Построить()
    Стр = Новый Структура;
    Стр.Вставить("Цена", 5);
    Стр = Новый Структура("Цена", "x");
    Возврат Стр;
КонецФункции

Функция Тест()
    Р = Построить();
    Возврат Р;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "р").expect("р must be inferred");
    let fields = Type::from_id(&db, file_id, ty).fields();
    let price = fields.iter().find(|f| f.name.as_str() == "Цена").expect("key `Цена` present");
    assert_eq!(
        price.ty,
        db.string(None, false),
        "later re-seed (Строка) must win over the earlier insert (Число)",
    );
}

fn hover_baseline_setup(fixture_text: &str) -> (Analysis, FileId, u32) {
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

    let fixture = Fixture::parse(&cleaned);
    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(path_line))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");
    (Analysis::from_database(db), test_file, cursor_in_file)
}

fn check_hover_contains(fixture: &str, expected_substring: Expect) {
    let (analysis, file_id, offset) = hover_baseline_setup(fixture);
    let hover = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("hover must produce a result for projection-typed receivers");
    expected_substring.assert_eq(extract_fields_line(&hover.markup).as_str());
}

fn extract_fields_line(markup: &str) -> String {
    markup
        .lines()
        .find(|line| line.starts_with("**Поля:**"))
        .map(|line| line.to_string())
        .unwrap_or_default()
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = hover_baseline_setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

#[test]
fn hover_on_literal_structure_lists_keys_as_typed_fields() {
    let (analysis, file_id, offset) = hover_baseline_setup(
        r#"//- /test.bsl
Функция Построить()
    Пар = Новый Структура("Город, Индекс", "Москва", 101);
    Возврат Пар;
КонецФункции

Функция Тест()
    Стр = Построить();
    Возврат Ст$0р;
КонецФункции
"#,
    );
    let hover =
        analysis.hover(file_id, offset, ide::Locale::Ru).expect("hover must produce a result");
    let fields = extract_fields_line(&hover.markup);
    assert!(
        fields.contains("Город"),
        "hover fields block must list structure key `Город`; got markup:\n{}",
        hover.markup
    );
    assert!(fields.contains("Индекс"), "hover fields block must list key `Индекс`; got: {fields}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_on_projection_selection_lists_columns_and_platform_members() {
    let items = complete(
        r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя, 42 КАК Цена");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выборка.$0;
КонецФункции
"#,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Имя"),
        "projection column `Имя` must appear in completion, got {labels:?}",
    );
    assert!(
        labels.contains(&"Цена"),
        "projection column `Цена` must appear in completion, got {labels:?}",
    );
    assert!(
        labels.contains(&"Следующий"),
        "platform method `Следующий` must still appear in completion, got {labels:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_on_inline_query_union_receiver_lists_projection_columns() {
    let items = complete(
        r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Возврат ЗапросОбъект.Выполнить().Выбрать().$0;
КонецФункции
"#,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Имя"),
        "inline union receiver must preserve projection column completion, got {labels:?}",
    );
    assert!(
        labels.contains(&"Следующий"),
        "inline union receiver must preserve selection platform methods, got {labels:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_inline_query_union_receiver_field_renders_type() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Возврат ЗапросОбъект.Выполнить().Выбрать().Им$0я;
КонецФункции
"#;
    let (analysis, file_id, offset) = hover_baseline_setup(fixture);
    let hover = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("hover must resolve the inline projection field");
    assert!(
        hover.markup.contains("Строка") || hover.markup.contains("String"),
        "hover on inline union receiver field must render string type, got: {}",
        hover.markup,
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_projection_none_omits_fields_block() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Текст = ПолучитьТекстЗапроса();
    ЗапросОбъект = Новый Запрос(Текст);
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выб$0орка;
КонецФункции
"#;
    let (analysis, file_id, offset) = hover_baseline_setup(fixture);
    let hover = analysis
        .hover(file_id, offset, ide::Locale::Ru)
        .expect("hover must produce a result for projection-less selections");
    assert!(
        !hover.markup.contains("**Поля:**"),
        "projection-less selection must not surface a fields block — got: {markup}",
        markup = hover.markup,
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_projection_selection_lists_field_names() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выб$0орка;
КонецФункции
"#;
    check_hover_contains(fixture, expect!["**Поля:** Имя: Строка"]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_projection_selection_renders_cast_precision_and_scale() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Число(15, 2)) КАК Цена");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выб$0орка;
КонецФункции
"#;
    check_hover_contains(fixture, expect!["**Поля:** Цена: Число(15, 2)"]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_projection_selection_renders_cast_string_length() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ВЫРАЗИТЬ("""" КАК Строка(50)) КАК Имя");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выб$0орка;
КонецФункции
"#;
    check_hover_contains(fixture, expect!["**Поля:** Имя: Строка(50)"]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_iteration_row_from_batched_helper_renders_cast_precision() {
    let fixture = r#"//- /test.bsl
Функция ПолучитьТЗ() Экспорт
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ 1 КАК X ПОМЕСТИТЬ ВТ; ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Число(15, 2)) КАК Цена ИЗ ВТ КАК ВТ";
    Возврат Зап.Выполнить().Выгрузить();
КонецФункции

Функция Тест()
    Для Каждого Стр Из ПолучитьТЗ() Цикл
        Возврат Ст$0р;
    КонецЦикла;
КонецФункции
"#;
    check_hover_contains(fixture, expect!["**Поля:** Цена: Число(15, 2)"]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_projection_selection_renders_cast_precision_only_number() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ВЫРАЗИТЬ(0 КАК Число(15)) КАК Сумма");
    Выборка = ЗапросОбъект.Выполнить().Выбрать();
    Возврат Выб$0орка;
КонецФункции
"#;
    check_hover_contains(fixture, expect!["**Поля:** Сумма: Число(15)"]);
}
