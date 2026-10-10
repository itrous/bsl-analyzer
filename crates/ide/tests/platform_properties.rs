use hir::{Builders, HirDatabase, InferenceDiagnostic, Name, Type, TypeId, TypeKernelDb, TypeKind};
use ide::{Analysis, CompletionItem, CompletionItemKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn setup_inline(code: &str) -> (RootDatabaseImpl, FileId) {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, code);
    (db, file_id)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn readonly_diagnostics(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(Name, TypeId)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::ReadOnlyPropertyAssignment { receiver_ty, field_name, .. } => {
                Some((field_name.clone(), *receiver_ty))
            }
            _ => None,
        })
        .collect()
}

fn completions_at(code: &str) -> Vec<CompletionItem> {
    let cursor = code.find("$0").expect("fixture must mark cursor with $0");
    let without_cursor: String = format!("{}{}", &code[..cursor], &code[cursor + 2..]);
    let (db, file_id) = setup_inline(&without_cursor);
    Analysis::from_database(db).completions(file_id, cursor as u32, None, ide::Locale::Ru)
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn new_query_text_field_infers_to_string() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Т = Зап.Текст;
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    assert_eq!(
        var_ty(&db, file_id, "т"),
        Some(db.string(None, false)),
        "Т must carry Ty::String via property inference, not Ty::Unknown"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn new_query_parameters_field_infers_to_structure() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    П = Зап.Параметры;
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    assert_eq!(
        var_ty(&db, file_id, "п"),
        Some(db.structure(None)),
        "Зап.Параметры must carry Ty::Structure via property inference"
    );
}

#[test]
fn chained_property_method_resolves() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Параметры.Вставить(\"Ключ\", 1);
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    let unresolved: Vec<String> = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, .. } => {
                Some(method_name.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    assert!(
        !unresolved.iter().any(|n| n.eq_ignore_ascii_case("Вставить")),
        "chained Зап.Параметры.Вставить must resolve, got UnresolvedMethodCall for: {unresolved:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn read_only_property_assignment_emits_diagnostic() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Параметры = Новый Структура;
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    let diags = readonly_diagnostics(&db, file_id);
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one ReadOnlyPropertyAssignment diagnostic, got: {diags:?}"
    );
    let (field, recv) = &diags[0];
    assert_eq!(field.as_str(), "Параметры");
    assert!(
        matches!(db.lookup_type(*recv), TypeKind::Query { .. }),
        "receiver_ty must be Ty::Query, got {:?}",
        db.lookup_type(*recv)
    );
}

#[test]
fn writable_property_assignment_no_diagnostic() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    assert!(
        readonly_diagnostics(&db, file_id).is_empty(),
        "writable property must not fire ReadOnlyPropertyAssignment"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_on_new_query_lists_text_and_parameters() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.$0
КонецПроцедуры
";
    let items = completions_at(code);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"Текст"),
        "completion must list property Текст, got labels: {labels:?}"
    );
    assert!(
        labels.contains(&"Параметры"),
        "completion must list property Параметры, got labels: {labels:?}"
    );
    assert!(
        labels.contains(&"Выполнить"),
        "completion must keep method Выполнить alongside properties, got labels: {labels:?}"
    );

    let params = items.iter().find(|i| i.label == "Параметры").expect("Параметры item");
    assert_eq!(
        params.kind,
        CompletionItemKind::Property,
        "Параметры must surface as Property, not {:?}",
        params.kind
    );
    assert!(
        params.detail.as_deref().unwrap_or_default().contains("Только чтение"),
        "read-only property detail must include [Только чтение], got: {:?}",
        params.detail,
    );

    assert_eq!(params.insert_text, "Параметры");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_query_parameters_renders_readonly_structure_block() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    П = Зап.Параметры;
КонецПроцедуры
";
    let cursor = code.find("Параметры").expect("fixture must contain Параметры");
    let (db, file_id) = setup_inline(code);
    let hover = Analysis::from_database(db)
        .hover(file_id, cursor as u32, ide::Locale::Ru)
        .expect("hover must return a result for Зап.Параметры");
    let markup = &hover.markup;
    assert!(
        markup.contains("Параметры") && markup.contains("Parameters"),
        "hover markup must include bilingual name, got: {markup}"
    );
    assert!(
        markup.contains("Только чтение"),
        "hover markup must mark Параметры read-only, got: {markup}"
    );
    assert!(
        markup.contains("Структура"),
        "hover markup must include value type Структура, got: {markup}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn query_execute_unload_chain_infers_value_table() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
    Таблица = Зап.Выполнить().Выгрузить();
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    let ty = var_ty(&db, file_id, "таблица");
    let contains_value_table = match &ty {
        Some(id) => match db.lookup_type(*id) {
            TypeKind::ValueTable(_) => true,
            TypeKind::Union(members) => members
                .iter()
                .any(|member| matches!(db.lookup_type(*member), TypeKind::ValueTable(_))),
            _ => false,
        },
        _ => false,
    };
    assert!(
        contains_value_table,
        "Запрос.Выполнить().Выгрузить() must carry Ty::ValueTable (optionally in a union) — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_query_execute_unload_lists_value_table_members() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Выполнить().Выгрузить().$0
КонецПроцедуры
";
    let items = completions_at(code);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    for expected in ["Добавить", "Колонки", "Количество"] {
        assert!(
            labels.contains(&expected),
            "completion on ValueTable must include {expected}, got: {labels:?}",
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_on_query_parameters_lists_structure_methods() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Параметры.$0
КонецПроцедуры
";
    let items = completions_at(code);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    for expected in ["Вставить", "Количество", "Очистить"] {
        assert!(
            labels.contains(&expected),
            "completion on Зап.Параметры must include {expected}, got: {labels:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_chained_method_resolves_platform_method() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
    Результат = Зап.Выполнить().Выгрузить().ВыгрузитьКолонку(\"Ссылка\");
КонецПроцедуры
";
    let cursor = code.rfind("ВыгрузитьКолонку").expect("fixture must contain ВыгрузитьКолонку");
    let (db, file_id) = setup_inline(code);
    let hover = Analysis::from_database(db)
        .hover(file_id, cursor as u32, ide::Locale::Ru)
        .expect("hover must produce a result on chained ВыгрузитьКолонку");
    let markup = &hover.markup;
    assert!(
        markup.contains("ВыгрузитьКолонку") && markup.contains("UnloadColumn"),
        "hover must show bilingual platform-method header, got: {markup}"
    );
    assert!(
        !markup.contains("КоллекцияСтрок"),
        "hover must NOT include workspace free-function param list, got: {markup}"
    );
}

#[test]
fn hover_on_chained_method_does_not_match_workspace_free_function() {
    let code = "\
Функция ВыгрузитьКолонку(КоллекцияСтрок, ИмяКолонки) Экспорт
    Возврат Новый Массив;
КонецФункции

Процедура Тест()
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
    Результат = Зап.Выполнить().Выгрузить().ВыгрузитьКолонку(\"Ссылка\");
КонецПроцедуры
";
    let cursor = code
        .rfind("ВыгрузитьКолонку(\"Ссылка")
        .expect("fixture must contain the chained call site");
    let (db, file_id) = setup_inline(code);
    let result = Analysis::from_database(db).hover(file_id, cursor as u32, ide::Locale::Ru);
    if let Some(hover) = result {
        assert!(
            !hover.markup.contains("Функция ВыгрузитьКолонку()"),
            "hover must NOT render the workspace free-function header, got: {markup}",
            markup = hover.markup
        );
        assert!(
            !hover.markup.contains("КоллекцияСтрок"),
            "hover must NOT mention the workspace function's param, got: {markup}",
            markup = hover.markup
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_on_chained_method_ignores_local_var_with_same_name() {
    let code = "\
Процедура Тест()
    ВыгрузитьКолонку = \"shadow-name\";
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
    Результат = Зап.Выполнить().Выгрузить().ВыгрузитьКолонку(\"Ссылка\");
КонецПроцедуры
";
    let cursor = code
        .rfind("ВыгрузитьКолонку(\"Ссылка")
        .expect("fixture must contain the chained call site");
    let (db, file_id) = setup_inline(code);
    let hover = Analysis::from_database(db)
        .hover(file_id, cursor as u32, ide::Locale::Ru)
        .expect("hover must produce a result");
    assert!(
        hover.markup.contains("ВыгрузитьКолонку") && hover.markup.contains("UnloadColumn"),
        "hover must show platform-method header, got: {markup}",
        markup = hover.markup
    );
    assert!(
        !hover.markup.contains("Локальная переменная"),
        "hover must NOT show the shadowing local-var hover, got: {markup}",
        markup = hover.markup
    );
}

#[test]
fn unload_column_string_arg_does_not_emit_type_mismatch() {
    let code = "\
Процедура Тест()
    Зап = Новый Запрос;
    Зап.Текст = \"ВЫБРАТЬ 1\";
    Результат = Зап.Выполнить().Выгрузить().ВыгрузитьКолонку(\"Ссылка\");
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    let infer = db.infer(file_id);
    let arg_diags = db.arg_diagnostics(file_id);
    let mismatches: Vec<&InferenceDiagnostic> = infer
        .diagnostics
        .iter()
        .chain(arg_diags.iter())
        .map(|(_, d)| d)
        .filter(|d| matches!(d, InferenceDiagnostic::TypeMismatch { .. }))
        .collect();
    assert!(
        mismatches.is_empty(),
        "ВыгрузитьКолонку(\"Ссылка\") must not emit TypeMismatch — \
         param type after fix is Ty::Union([Number, String, ValueTableColumn]). \
         Got: {mismatches:#?}",
    );
}

/// Columns of a row taken from an unloaded query result are ordinary value-table columns: the
/// platform accepts `Стр.Код = ...` there, so only the query-result cursor is read-only.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn value_table_row_column_assignment_no_diagnostic() {
    let code = "\
Функция ДанныеСчетов()
    Запрос = Новый Запрос(\"ВЫБРАТЬ \"\" 01 \"\" КАК Код\");
    Т = Запрос.Выполнить().Выгрузить();
    Для Каждого Стр Из Т Цикл
        Стр.Код = СокрЛП(Стр.Код);
    КонецЦикла;
    Возврат Т;
КонецФункции
";
    let (db, file_id) = setup_inline(code);
    // Without a resolved column the assignment is silent for any readonly flag, so pin the
    // receiver to a projected value-table row first.
    let row = var_ty(&db, file_id, "стр").expect("стр must be inferred");
    assert!(
        matches!(db.lookup_type(row), TypeKind::ValueTableRow(ref f) if f.projection.is_some()),
        "стр must be a projected value-table row, got {:?}",
        db.lookup_type(row)
    );
    let fields = Type::from_id(&db, file_id, row).fields();
    let code_col = fields
        .iter()
        .find(|f| f.name.as_str() == "Код")
        .unwrap_or_else(|| panic!("row must enumerate column `Код`, got {fields:?}"));
    assert!(!code_col.is_readonly, "enumerated value-table row column must be writable");
    let diags = readonly_diagnostics(&db, file_id);
    assert!(diags.is_empty(), "a value-table row column is writable, got: {diags:?}");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn query_selection_field_assignment_still_emits_diagnostic() {
    let code = "\
Процедура Тест()
    Запрос = Новый Запрос(\"ВЫБРАТЬ \"\" 01 \"\" КАК Код\");
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Выборка.Код = СокрЛП(Выборка.Код);
    КонецЦикла;
КонецПроцедуры
";
    let (db, file_id) = setup_inline(code);
    let diags = readonly_diagnostics(&db, file_id);
    assert_eq!(diags.len(), 1, "a query-result cursor field is read-only, got: {diags:?}");
    assert_eq!(diags[0].0.as_str(), "Код");
    assert!(
        matches!(db.lookup_type(diags[0].1), TypeKind::QueryResultSelection(_)),
        "receiver must be the selection, got {:?}",
        db.lookup_type(diags[0].1)
    );
}
