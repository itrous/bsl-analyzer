//! Value types of `Соответствие.Получить("ключ")` read from a literal key.
//!
//! Every scenario is compared with the same source analysed under the `mapKeyTypes` profile
//! switched off. The platform data describes `Map.Get` without a result type, so on the built-in
//! facts the result without the profile is `Неопределено`; a scenario that says "the prior type"
//! pins that value, not mere equality of two runs.

use std::collections::BTreeSet;

use hir::{Semantics, TypeId, TypeKernelDb, TypeKind};
use ide::{Analysis, Diagnostic, DiagnosticCode, DiagnosticsConfig};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::ast::{self, AstNode};
use syntax::SyntaxKind;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

const PRIOR: &str = "Неопределено";

struct Case {
    db: RootDatabaseImpl,
    file: FileId,
    text: String,
}

fn case(fixture_text: &str, map_keys: bool) -> Case {
    let fixture = Fixture::parse(fixture_text);
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
    db.set_map_key_types_enabled(map_keys);
    let (file, text) = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, f)| (*id, f.content.to_string()))
        .expect("fixture must contain /test.bsl");
    Case { db, file, text }
}

fn kind_name(db: &RootDatabaseImpl, kind: TypeKind, out: &mut BTreeSet<String>) {
    let name = match kind {
        TypeKind::Union(members) => {
            for member in members.iter() {
                kind_name(db, db.lookup_type(*member).clone(), out);
            }
            return;
        }
        TypeKind::Number(_) => "Число".to_owned(),
        TypeKind::String(_) => "Строка".to_owned(),
        TypeKind::Boolean => "Булево".to_owned(),
        TypeKind::Date(_) => "Дата".to_owned(),
        TypeKind::Null => "Null".to_owned(),
        TypeKind::Undefined => PRIOR.to_owned(),
        TypeKind::Any => "Произвольный".to_owned(),
        TypeKind::Unknown => "?".to_owned(),
        TypeKind::Structure(facet) => match &facet.fields {
            Some(projection) => {
                let mut fields: Vec<String> = projection
                    .fields
                    .iter()
                    .map(|field| format!("{}:{}", field.name, render(db, field.ty)))
                    .collect();
                fields.sort();
                format!("Структура{{{}}}", fields.join(";"))
            }
            None => "Структура".to_owned(),
        },
        TypeKind::Array(facet) => format!(
            "Массив<{}>",
            facet.element.map_or_else(|| "?".to_owned(), |element| render(db, element))
        ),
        TypeKind::Map(facet) if facet.entries.is_some() => "Соответствие{ключи}".to_owned(),
        TypeKind::Map(_) => "Соответствие".to_owned(),
        other => format!("{other:?}"),
    };
    out.insert(name);
}

/// The branches of a type, sorted and joined: `Неопределено|Число`.
fn render(db: &RootDatabaseImpl, ty: TypeId) -> String {
    let mut names = BTreeSet::new();
    kind_name(db, db.lookup_type(ty).clone(), &mut names);
    names.into_iter().collect::<Vec<_>>().join("|")
}

fn is_get_call(call: &ast::CallExpr) -> bool {
    let Some(callee) = call.syntax().first_child() else { return false };
    if callee.kind() != SyntaxKind::FIELD_EXPR {
        return false;
    }
    let last = callee
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::IDENT)
        .last();
    last.is_some_and(|token| matches!(token.text().to_lowercase().as_str(), "получить" | "get"))
}

impl Case {
    /// The inferred type of every `.Получить(...)` / `.Get(...)` call, in source order.
    fn reads(&self) -> Vec<String> {
        let sema = Semantics::new(&self.db);
        let root = self.db.parse(self.file).syntax_node();
        root.descendants()
            .filter_map(ast::CallExpr::cast)
            .filter(is_get_call)
            .map(|call| render(&self.db, sema.type_of_expr(self.file, call.syntax())))
            .collect()
    }

    /// The inferred type of the first expression whose source text is exactly `text`.
    fn type_of_text(&self, text: &str) -> String {
        let sema = Semantics::new(&self.db);
        let root = self.db.parse(self.file).syntax_node();
        let node = root
            .descendants()
            .find(|node| {
                matches!(
                    node.kind(),
                    SyntaxKind::CALL_EXPR | SyntaxKind::INDEX_EXPR | SyntaxKind::FIELD_EXPR
                ) && node.text() == text
            })
            .unwrap_or_else(|| panic!("no expression `{text}` in:\n{}", self.text));
        render(&self.db, sema.type_of_expr(self.file, &node))
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        Analysis::from_database(self.db.clone())
            .diagnostics(self.file, &DiagnosticsConfig::all_enabled())
    }

    /// Findings of the given codes as `(code, source text of the range, message)`.
    fn findings(&self, codes: &[DiagnosticCode]) -> Vec<(DiagnosticCode, String, String)> {
        self.diagnostics()
            .into_iter()
            .filter(|diagnostic| codes.contains(&diagnostic.code))
            .map(|diagnostic| {
                let range = diagnostic.range;
                let text =
                    self.text[usize::from(range.start())..usize::from(range.end())].to_owned();
                (diagnostic.code, text, diagnostic.message)
            })
            .collect()
    }

    fn mismatches(&self) -> Vec<(DiagnosticCode, String, String)> {
        self.findings(&[DiagnosticCode::TypeMismatch, DiagnosticCode::TypeMismatchByDocComment])
    }

    fn hover(&self, offset: u32, locale: ide::Locale) -> String {
        Analysis::from_database(self.db.clone())
            .hover(self.file, offset, locale)
            .map(|hover| hover.markup)
            .unwrap_or_else(|| "<no hover>".to_owned())
    }
}

/// Splits the `$0` cursor out of a fixture and returns `(fixture, offset inside the test file)`.
fn with_cursor(source: &str) -> (String, u32) {
    let marker = source.find("$0").expect("fixture must contain a $0 cursor");
    let text = source.replacen("$0", "", 1);
    let header = text[..marker].rfind("//- ").expect("cursor must be inside a //- file");
    let header_end = text[header..].find('\n').expect("header ends with a newline") + header;
    (text, (marker - (header_end + 1)) as u32)
}

fn hover_of(source: &str, map_keys: bool) -> String {
    let (text, offset) = with_cursor(source);
    case(&text, map_keys).hover(offset, ide::Locale::Ru)
}

fn on(source: &str) -> Vec<String> {
    case(source, true).reads()
}

fn off(source: &str) -> Vec<String> {
    case(source, false).reads()
}

/// A procedure whose body starts with an empty `Данные = Новый Соответствие;`.
fn proc(statements: &str) -> String {
    format!(
        "//- /test.bsl\nПроцедура Тест(Условие, Список, Ключ, Переменная)\n    Данные = Новый \
         Соответствие;{statements}\nКонецПроцедуры\n"
    )
}

/// The source analysed with the profile on gives `expected` for its reads, and with the profile
/// off gives the prior type for each of them.
#[track_caller]
fn refined_source(source: &str, expected: &[&str]) {
    assert_eq!(on(source), expected, "profile on:\n{source}");
    assert_eq!(off(source), vec![PRIOR; expected.len()], "profile off:\n{source}");
}

#[track_caller]
fn refined(statements: &str, expected: &[&str]) {
    refined_source(&proc(statements), expected);
}

/// Positive control for every "no refinement" scenario: the same construction with a proven
/// `"сумма"` and no forbidding event is refined, so an implementation that never refines cannot
/// pass them.
#[track_caller]
fn control() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Р = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

#[track_caller]
fn unrefined_source(source: &str, reads: usize) {
    control();
    let prior = vec![PRIOR; reads];
    assert_eq!(on(source), prior, "profile on:\n{source}");
    assert_eq!(off(source), prior, "profile off:\n{source}");
}

#[track_caller]
fn unrefined(statements: &str, reads: usize) {
    unrefined_source(&proc(statements), reads);
}

/// For receivers the engine cannot type as a `Соответствие` at all (a module variable, a
/// property, a loop binding): the profile must change nothing, whatever the prior type is.
#[track_caller]
fn unchanged_source(source: &str, reads: usize) {
    control();
    let before = off(source);
    assert_eq!(before.len(), reads, "{source}");
    assert_eq!(on(source), before, "profile on differs from profile off:\n{source}");
}

/// The body with the given constructor text in place of `Новый Соответствие`.
fn proc_with_constructor(constructor: &str, statements: &str) -> String {
    format!(
        "//- /test.bsl\nПроцедура Тест(Условие, Список, Ключ, Переменная)\n    Данные = \
         {constructor};{statements}\nКонецПроцедуры\n"
    )
}

// ---- 1: proven pairs give the written literal type ------------------------------------------

#[test]
fn proven_pairs_give_the_written_type() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Данные.Вставить("готово", Истина);
    Сумма = Данные.Получить("сумма");
    Готово = Данные.Получить("готово");"#,
        &["Число", "Булево"],
    );
}

#[test]
fn every_scalar_literal_kind_is_typed() {
    refined(
        r#"
    Данные.Вставить("с", "текст");
    Данные.Вставить("д", '20240101');
    Данные.Вставить("н", Null);
    Данные.Вставить("ч", 1.5);
    Данные.Вставить("л", Ложь);
    А = Данные.Получить("с");
    Б = Данные.Получить("д");
    В = Данные.Получить("н");
    Г = Данные.Получить("ч");
    Д = Данные.Получить("л");"#,
        &["Строка", "Дата", "Null", "Число", "Булево"],
    );
}

#[test]
fn an_undefined_literal_is_a_written_kind() {
    // `Неопределено` equals the fallback, so the only way to tell the answer from the absence of
    // one is a second kind beside it: the union must keep both.
    refined(
        r#"
    Данные.Вставить("к", Неопределено);
    Данные.Вставить("к", 1);
    А = Данные.Получить("к");"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn both_languages_and_any_case_name_the_map_the_insert_and_the_read() {
    refined_source(
        &proc_with_constructor(
            "New Map",
            r#"
    Данные.Insert("сумма", 1);
    Р = Данные.Get("сумма");"#,
        ),
        &["Число"],
    );
    refined_source(
        &proc_with_constructor(
            "новый соответствие",
            r#"
    Данные.вставить("сумма", 1);
    Р = Данные.ПОЛУЧИТЬ("сумма");"#,
        ),
        &["Число"],
    );
    refined_source(
        "//- /test.bsl\nПроцедура Тест()\n    данные = Новый Map;\n    ДАННЫЕ.Insert(\"сумма\", \
         1);\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n",
        &["Число"],
    );
}

#[test]
fn the_three_empty_constructor_forms_all_seed_a_root() {
    for constructor in [
        "Новый Соответствие",
        "Новый Соответствие()",
        "Новый Map",
        "Новый Map()",
        "New Соответствие",
    ] {
        refined_source(
            &proc_with_constructor(
                constructor,
                r#"
    Данные.Вставить("сумма", 1);
    Р = Данные.Получить("сумма");"#,
            ),
            &["Число"],
        );
    }
}

// ---- 2: any conditional write widens with Неопределено --------------------------------------

#[test]
fn a_conditional_write_of_another_key_removes_the_proof() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Если Условие Тогда
        Данные.Вставить("б", 2);
    КонецЕсли;
    Р = Данные.Получить("сумма");"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn a_conditional_write_of_the_key_itself_does_not_prove_it() {
    let forms = [
        "Если Условие Тогда\n        Данные.Вставить(\"сумма\", 1);\n    КонецЕсли;",
        "Если Условие Тогда\n    ИначеЕсли Условие Тогда\n        Данные.Вставить(\"сумма\", \
         1);\n    КонецЕсли;",
        "Если Условие Тогда\n    Иначе\n        Данные.Вставить(\"сумма\", 1);\n    КонецЕсли;",
        "Пока Условие Цикл\n        Данные.Вставить(\"сумма\", 1);\n    КонецЦикла;",
        "Для Ин = 1 По 3 Цикл\n        Данные.Вставить(\"сумма\", 1);\n    КонецЦикла;",
        "Для Каждого Эл Из Список Цикл\n        Данные.Вставить(\"сумма\", 1);\n    КонецЦикла;",
        "Попытка\n        Данные.Вставить(\"сумма\", 1);\n    Исключение\n    КонецПопытки;",
        "Попытка\n    Исключение\n        Данные.Вставить(\"сумма\", 1);\n    КонецПопытки;",
        "#Если Сервер Тогда\n    Данные.Вставить(\"сумма\", 1);\n    #КонецЕсли",
        "#Если Клиент Тогда\n    #ИначеЕсли Сервер Тогда\n    Данные.Вставить(\"сумма\", \
         1);\n    #КонецЕсли",
        "#Если Клиент Тогда\n    #Иначе\n    Данные.Вставить(\"сумма\", 1);\n    #КонецЕсли",
    ];
    for form in forms {
        refined(
            &format!("\n    {form}\n    Р = Данные.Получить(\"сумма\");"),
            &["Неопределено|Число"],
        );
    }
}

#[test]
fn a_read_beside_a_conditional_write_of_its_own_key_is_not_proven() {
    refined(
        r#"
    Если Условие Тогда
        Данные.Вставить("сумма", 1);
        Р = Данные.Получить("сумма");
    КонецЕсли;"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn a_read_in_the_condition_of_the_statement_that_writes_is_not_proven() {
    refined(
        r#"
    Если Данные.Получить("сумма") = Неопределено Тогда
        Данные.Вставить("сумма", 1);
    КонецЕсли;"#,
        &["Неопределено|Число"],
    );
    refined(
        r#"
    Пока Данные.Получить("сумма") = Неопределено Цикл
        Данные.Вставить("сумма", 1);
    КонецЦикла;"#,
        &["Неопределено|Число"],
    );
    refined(
        r#"
    Если Условие Тогда
    ИначеЕсли Данные.Получить("сумма") = Неопределено Тогда
        Данные.Вставить("сумма", 1);
    КонецЕсли;"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn an_unconditional_write_keeps_the_proof_beside_unrelated_branches() {
    // The control of the whole row: nothing conditional writes into the map, so a branch that only
    // reads it changes nothing.
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Если Условие Тогда
        Х = Данные.Получить("сумма");
    КонецЕсли;"#,
        &["Число"],
    );
}

// ---- 3: a read before the write does not prove presence -------------------------------------

#[test]
fn a_read_before_the_write_is_not_proven() {
    refined(
        r#"
    Х = Данные.Получить("сумма");
    Данные.Вставить("сумма", 1);"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn a_read_in_an_expression_before_the_write_is_not_proven() {
    refined(
        r#"
    Х = Строка(Данные.Получить("сумма")) + "т";
    Данные.Вставить("сумма", 1);"#,
        &["Неопределено|Число"],
    );
}

#[test]
fn moving_the_write_before_the_read_proves_it() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Х = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

// ---- 4: an overwrite unions every written type ----------------------------------------------

#[test]
fn an_overwrite_with_another_type_unions_before_and_after_the_second_write() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Между = Данные.Получить("сумма");
    Данные.Вставить("сумма", "х");
    После = Данные.Получить("сумма");"#,
        &["Строка|Число", "Строка|Число"],
    );
}

#[test]
fn a_single_write_has_a_single_branch() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    А = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

#[test]
fn an_overwrite_with_the_same_type_keeps_one_branch() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Данные.Вставить("сумма", 2);
    А = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

// ---- 5: a key nobody wrote is not refined ---------------------------------------------------

#[test]
fn an_unwritten_key_keeps_the_prior_type_while_its_neighbour_is_refined() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    Нет = Данные.Получить("нет");
    Есть = Данные.Получить("сумма");"#,
    );
    assert_eq!(on(&source), [PRIOR, "Число"]);
    assert_eq!(off(&source), [PRIOR, PRIOR]);
}

// ---- 6: a dynamic key and a wrong arity are not refined -------------------------------------

#[test]
fn a_dynamic_key_or_a_wrong_arity_is_not_refined() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    А = Данные.Получить(Ключ);
    Б = Данные.Получить();
    В = Данные.Получить("сумма", 1);
    Г = Данные.Получить("сумма");"#,
    );
    let before = off(&source);
    assert_eq!(before.len(), 4);
    assert_eq!(before[0], PRIOR, "a dynamic key keeps the platform's result");
    let after = on(&source);
    assert_eq!(after[..3], before[..3], "{source}");
    assert_eq!(after[3], "Число", "the correct one-argument call is refined: {source}");
    assert_eq!(before[3], PRIOR);
}

// ---- 7: an alias invalidates the whole root -------------------------------------------------

#[test]
fn an_alias_takes_the_root_back_to_the_prior_type_before_and_after_the_copy() {
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    До = Данные.Получить("сумма");
    Копия = Данные;
    Копия.Вставить("сумма", "х");
    Данные2 = Данные.Получить("сумма");
    Копия2 = Копия.Получить("сумма");"#,
        3,
    );
}

// ---- 8: a mutation or an opaque statement disables the answer -------------------------------

#[test]
fn every_unknown_mutation_takes_the_prior_type_before_and_after_it() {
    let events = [
        r#"Данные.Удалить("сумма");"#,
        "Данные.Очистить();",
        r#"Данные["сумма"] = "х";"#,
        "Данные.Вставить(Ключ, 1);",
        "Данные.НеизвестныйМетод();",
        r#"Выполнить("Х = 1");"#,
        r#"Х = Вычислить("1");"#,
        // property write, an insert used as an expression, a third argument
        "Данные.Свойство = 1;",
        r#"Х = Данные.Вставить("сумма", 2);"#,
        r#"Данные.Вставить("сумма", 1, 2);"#,
        // the receiver is not a path: the ternary may evaluate to the map
        r#"?(Условие, Данные, Новый Соответствие).Вставить("сумма", "х");"#,
        // English names
        r#"Данные.Delete("сумма");"#,
        "Данные.Clear();",
        r#"Х = Eval("1");"#,
    ];
    for event in events {
        let statements = |event: &str| {
            format!(
                "\n    Данные.Вставить(\"сумма\", 1);\n    До = Данные.Получить(\"сумма\");\n    \
                 {event}\n    После = Данные.Получить(\"сумма\");"
            )
        };
        unrefined(&statements(event), 2);
        // The same statement aimed at another name leaves the map alone, so it is the event and
        // not the shape of the statement that disables the answer.
        if event.contains("Данные") {
            refined(&statements(&event.replace("Данные", "Другое")), &["Число", "Число"]);
        }
    }
}

#[test]
fn an_event_after_the_read_alone_disables_the_answer() {
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    До = Данные.Получить("сумма");
    Данные.Очистить();"#,
        1,
    );
}

#[test]
fn a_local_function_named_like_eval_does_not_hide_the_opacity() {
    let source = "//- /test.bsl\nФункция Вычислить(Выражение)\n    Возврат 1;\nКонецФункции\n\n\
                  Процедура Тест()\n    Данные = Новый Соответствие;\n    Данные.Вставить(\"сумма\", \
                  1);\n    Х = Вычислить(\"1\");\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n";
    unrefined_source(source, 1);
}

#[test]
fn reads_and_non_escaping_uses_keep_the_proof() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Счёт = Данные.Количество();
    Эл = Данные["сумма"];
    Св = Данные.Свойство;
    Р = Данные.Получить("сумма");
    Ц = Данные.Получить("сумма").Метод();"#,
        &["Число", "Число"],
    );
}

// ---- 9: the map itself in an exit position is not tracked -----------------------------------

#[test]
fn a_map_in_an_exit_position_takes_the_prior_type() {
    let exits = [
        "Обработать(Данные);",
        "М = Новый Массив; М.Добавить(Данные);",
        r#"С = Новый Структура("Д", Данные);"#,
        "Х = Обработать(Данные);",
        "ОбработатьЗнач(Данные);",
        "М = Новый Массив(Данные);",
        "ДобавитьОбработчик Объект.Событие, Данные;",
        "УдалитьОбработчик Объект.Событие, Данные;",
        "Х = Новый Структура(Ключ, Данные);",
    ];
    for exit in exits {
        let statements = |exit: &str| {
            format!(
                "\n    Данные.Вставить(\"сумма\", 1);\n    До = Данные.Получить(\"сумма\");\n    \
                 {exit}\n    После = Данные.Получить(\"сумма\");"
            )
        };
        unrefined(&statements(exit), 2);
        // The same statement with another value in the exit position leaves the map tracked.
        refined(&statements(&exit.replace("Данные", "Другое")), &["Число", "Число"]);
    }
}

#[test]
fn a_map_passed_to_a_by_value_parameter_still_leaves_the_tracked_scope() {
    let source = "//- /test.bsl\nПроцедура Принять(Знач Копия)\nКонецПроцедуры\n\nПроцедура \
                  Тест()\n    Данные = Новый Соответствие;\n    Данные.Вставить(\"сумма\", \
                  1);\n    Принять(Данные);\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n";
    unrefined_source(source, 1);
    let other = source.replace("Принять(Данные);", "Принять(Другое);");
    refined_source(&other, &["Число"]);
}

#[test]
fn returning_or_raising_the_map_takes_the_prior_type() {
    unrefined_source(
        "//- /test.bsl\nФункция Тест()\n    Данные = Новый Соответствие;\n    \
         Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\n    Возврат \
         Данные;\nКонецФункции\n",
        1,
    );
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    Р = Данные.Получить("сумма");
    ВызватьИсключение Данные;"#,
        1,
    );
}

#[test]
fn passing_or_returning_a_read_result_does_not_release_the_map() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Обработать(Данные.Получить("сумма"));
    М = Новый Массив;
    М.Добавить(Данные.Получить("сумма"));
    Р = Данные.Получить("сумма");"#,
        &["Число", "Число", "Число"],
    );
}

// ---- 10: an unknown value disables only its own key -----------------------------------------

#[test]
fn a_non_literal_value_disables_only_its_key() {
    let values = ["Переменная", "-1", "Новый Соответствие", "Ф()", "Переменная + 1"];
    for value in values {
        let source = proc(&format!(
            "\n    Данные.Вставить(\"сумма\", {value});\n    Данные.Вставить(\"готово\", \
             Истина);\n    Сумма = Данные.Получить(\"сумма\");\n    Готово = \
             Данные.Получить(\"готово\");"
        ));
        assert_eq!(on(&source), [PRIOR, "Булево"], "value {value}:\n{source}");
        assert_eq!(off(&source), [PRIOR, PRIOR]);
    }
}

#[test]
fn an_insert_without_a_value_disables_only_its_key() {
    let source = proc(
        r#"
    Данные.Вставить("сумма");
    Данные.Вставить("готово", Истина);
    Сумма = Данные.Получить("сумма");
    Готово = Данные.Получить("готово");"#,
    );
    assert_eq!(on(&source), [PRIOR, "Булево"]);
    assert_eq!(off(&source), [PRIOR, PRIOR]);
}

#[test]
fn a_known_write_beside_an_unknown_one_does_not_drop_the_unknown() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    Данные.Вставить("сумма", Переменная);
    Сумма = Данные.Получить("сумма");
    Данные.Вставить("готово", Истина);
    Готово = Данные.Получить("готово");"#,
    );
    assert_eq!(on(&source), [PRIOR, "Булево"]);
    assert_eq!(off(&source), [PRIOR, PRIOR]);
}

#[test]
fn a_negative_number_is_not_typed_but_a_positive_one_is() {
    let source = proc(
        r#"
    Данные.Вставить("минус", -1);
    Данные.Вставить("плюс", 1);
    А = Данные.Получить("минус");
    Б = Данные.Получить("плюс");"#,
    );
    assert_eq!(on(&source), [PRIOR, "Число"]);
}

// ---- 11: an unprovable root is not refined --------------------------------------------------

#[test]
fn a_parameter_is_not_a_root() {
    unrefined_source(
        "//- /test.bsl\nПроцедура Тест(Данные)\n    Данные = Новый Соответствие;\n    \
         Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n",
        1,
    );
}

#[test]
fn a_module_variable_is_not_a_root() {
    unchanged_source(
        "//- /test.bsl\nПерем Данные;\n\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    \
         Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n",
        1,
    );
}

#[test]
fn a_second_assignment_to_the_root_disables_it() {
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    Данные = Новый Соответствие;
    Р = Данные.Получить("сумма");"#,
        1,
    );
    unrefined(
        r#"
    Другая = Новый Соответствие;
    Данные.Вставить("сумма", 1);
    Данные = Другая;
    Р = Данные.Получить("сумма");"#,
        1,
    );
    unchanged_source(
        &proc(
            r#"
    Данные.Вставить("сумма", 1);
    Данные = Переменная;
    Р = Данные.Получить("сумма");"#,
        ),
        1,
    );
}

#[test]
fn a_mention_outside_the_constructor_subtree_disables_the_root() {
    unrefined_source(
        "//- /test.bsl\nПроцедура Тест(Условие)\n    Если Условие Тогда\n        Данные = Новый \
         Соответствие;\n        Данные.Вставить(\"сумма\", 1);\n    КонецЕсли;\n    Р = \
         Данные.Получить(\"сумма\");\nКонецПроцедуры\n",
        1,
    );
}

#[test]
fn a_goto_in_the_body_disables_every_root() {
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    Р = Данные.Получить("сумма");
    Перейти ~Метка;
    ~Метка:"#,
        1,
    );
}

#[test]
fn a_read_before_the_constructor_disables_the_root() {
    let source = "//- /test.bsl\nПроцедура Тест()\n    До = Данные.Получить(\"сумма\");\n    \
                  Данные = Новый Соответствие;\n    Данные.Вставить(\"сумма\", 1);\n    Р = \
                  Данные.Получить(\"сумма\");\nКонецПроцедуры\n";
    control();
    let before = off(source);
    assert_eq!(before.len(), 2);
    assert_eq!(before[1], PRIOR, "after the constructor the receiver is a known map");
    assert_eq!(on(source), before, "{source}");
}

#[test]
fn a_constructor_with_arguments_is_not_a_seed() {
    unrefined_source(
        "//- /test.bsl\nПроцедура Тест(Другое)\n    Данные = Новый Соответствие(Другое);\n    \
         Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\nКонецПроцедуры\n",
        1,
    );
}

#[test]
fn a_loop_binding_with_the_roots_name_is_one_more_assignment() {
    unchanged_source(
        &proc(
            r#"
    Данные.Вставить("сумма", 1);
    Для Каждого Данные Из Список Цикл
    КонецЦикла;
    Р = Данные.Получить("сумма");"#,
        ),
        1,
    );
    unchanged_source(
        &proc(
            r#"
    Данные.Вставить("сумма", 1);
    Для Данные = 1 По 3 Цикл
    КонецЦикла;
    Р = Данные.Получить("сумма");"#,
        ),
        1,
    );
}

#[test]
fn a_property_that_shares_the_roots_name_is_not_a_root() {
    unchanged_source(
        "//- /test.bsl\nПроцедура Тест(Форма)\n    Форма.Данные = Новый Соответствие;\n    \
         Форма.Данные.Вставить(\"сумма\", 1);\n    Р = Форма.Данные.Получить(\"сумма\");\n\
         КонецПроцедуры\n",
        1,
    );
}

// ---- 12: a constructor inside a loop is a fresh root ----------------------------------------

#[test]
fn a_constructor_inside_a_loop_body_is_a_root() {
    refined_source(
        "//- /test.bsl\nПроцедура Тест(Строки)\n    Для Каждого Стр Из Строки Цикл\n        Д = \
         Новый Соответствие;\n        Д.Вставить(\"а\", 1);\n        Х = Д.Получить(\"а\");\n    \
         КонецЦикла;\nКонецПроцедуры\n",
        &["Число"],
    );
}

#[test]
fn a_read_before_the_write_in_a_loop_is_not_proven_by_the_previous_iteration() {
    refined_source(
        "//- /test.bsl\nПроцедура Тест(Строки)\n    Для Каждого Стр Из Строки Цикл\n        Д = \
         Новый Соответствие;\n        Х = Д.Получить(\"а\");\n        Д.Вставить(\"а\", 1);\n    \
         КонецЦикла;\nКонецПроцедуры\n",
        &["Неопределено|Число"],
    );
}

// ---- 13: keys are compared without regard to case -------------------------------------------

#[test]
fn a_write_in_another_case_proves_the_read() {
    refined(
        r#"
    Данные.Вставить("Сумма", 1);
    Х = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

#[test]
fn writes_in_two_cases_are_one_key() {
    refined(
        r#"
    Данные.Вставить("Сумма", 1);
    Данные.Вставить("сумма", "х");
    Х = Данные.Получить("Сумма");"#,
        &["Строка|Число"],
    );
}

// ---- 14: other characters of a key are significant ------------------------------------------

#[test]
fn dashes_spaces_and_separators_inside_a_key_keep_keys_apart() {
    let source = proc(
        r#"
    Данные.Вставить("а-б", 1);
    Данные.Вставить("а б", "х");
    Данные.Вставить("а - б", Истина);
    Один = Данные.Получить("а-б");
    Два = Данные.Получить("а б");
    Три = Данные.Получить("а - б");
    Нет = Данные.Получить("аб");"#,
    );
    assert_eq!(on(&source), ["Число", "Строка", "Булево", PRIOR]);
    assert_eq!(off(&source), [PRIOR; 4]);
}

#[test]
fn edge_spaces_empty_keys_and_escaped_quotes_are_not_normalised() {
    let source = proc(
        r#"
    Данные.Вставить(" а", 1);
    Данные.Вставить("а ", "х");
    Данные.Вставить("", Истина);
    Данные.Вставить("а""б", '20240101');
    Без = Данные.Получить("а");
    Ведущий = Данные.Получить(" а");
    Замыкающий = Данные.Получить("а ");
    Пустой = Данные.Получить("");
    Кавычка = Данные.Получить("а""б");
    БезКавычки = Данные.Получить("аб");"#,
    );
    assert_eq!(on(&source), [PRIOR, "Число", "Строка", "Булево", "Дата", PRIOR], "{source}");
    assert_eq!(off(&source), [PRIOR; 6]);
}

// ---- 15: the size limit after folding -------------------------------------------------------

fn numbered_inserts(count: usize) -> String {
    (0..count).map(|index| format!("\n    Данные.Вставить(\"ключ{index}\", 1);")).collect()
}

#[test]
fn sixty_four_distinct_keys_still_give_an_answer() {
    let statements =
        format!("{}\n    Первый = Данные.Получить(\"ключ0\");\n    Последний = Данные.Получить(\"ключ63\");", numbered_inserts(64));
    refined(&statements, &["Число", "Число"]);
}

#[test]
fn sixty_five_distinct_keys_give_no_answer_even_for_the_first() {
    let statements =
        format!("{}\n    Первый = Данные.Получить(\"ключ0\");\n    Последний = Данные.Получить(\"ключ64\");", numbered_inserts(65));
    unrefined(&statements, 2);
}

#[test]
fn case_duplicates_do_not_spend_the_limit() {
    let statements = format!(
        "{}\n    Данные.Вставить(\"КЛЮЧ0\", 1);\n    Первый = Данные.Получить(\"ключ0\");",
        numbered_inserts(64)
    );
    refined(&statements, &["Число"]);
}

// ---- 1, 12, 16: what hover shows ------------------------------------------------------------

/// Hover on the last use of `name`, with the cursor inside the word.
fn hover_on_use(source: &str, name: &str, map_keys: bool) -> String {
    let split = name.char_indices().nth(1).map_or(name.len(), |(index, _)| index);
    let marked = source.replacen("@@", &format!("{}$0{}", &name[..split], &name[split..]), 1);
    hover_of(&marked, map_keys)
}

#[test]
fn hover_of_the_result_variable_shows_the_refined_type() {
    let source = "//- /test.bsl\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    \
                  Данные.Вставить(\"сумма\", 1);\n    Данные.Вставить(\"готово\", Истина);\n    \
                  Сумма = Данные.Получить(\"сумма\");\n    Готово = Данные.Получить(\"готово\");\n    \
                  Н = @@;\nКонецПроцедуры\n";
    assert_eq!(hover_on_use(source, "Сумма", true), "**Сумма**\n\n**Тип:** Число\n\n");
    assert_eq!(hover_on_use(source, "Сумма", false), "**Сумма**\n\n**Тип:** Неопределено\n\n");
    assert_eq!(hover_on_use(source, "Готово", true), "**Готово**\n\n**Тип:** Булево\n\n");
    assert_eq!(hover_on_use(source, "Готово", false), "**Готово**\n\n**Тип:** Неопределено\n\n");
}

#[test]
fn hover_of_a_loop_body_result_shows_the_refined_type() {
    let source =
        "//- /test.bsl\nПроцедура Тест(Строки)\n    Для Каждого Стр Из Строки Цикл\n        \
                  Д = Новый Соответствие;\n        Д.Вставить(\"а\", 1);\n        Рез = \
                  Д.Получить(\"а\");\n        Н = @@;\n    КонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(hover_on_use(source, "Рез", true), "**Рез**\n\n**Тип:** Число\n\n");
    assert_eq!(hover_on_use(source, "Рез", false), "**Рез**\n\n**Тип:** Неопределено\n\n");
}

// ---- 16: the map itself and the platform method stay as they were ---------------------------

#[test]
fn the_map_variable_and_the_platform_method_hover_do_not_change() {
    let variable = "//- /test.bsl\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    \
                    Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\n    Н = \
                    Дан$0ные;\nКонецПроцедуры\n";
    assert_eq!(hover_of(variable, true), "**Данные**\n\n**Тип:** Соответствие\n\n");
    assert_eq!(hover_of(variable, false), "**Данные**\n\n**Тип:** Соответствие\n\n");

    let method = "//- /test.bsl\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    \
                  Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Пол$0учить(\"сумма\");\n\
                  КонецПроцедуры\n";
    let platform = hover_of(method, false);
    assert!(platform.contains("**Метод:** Получить / Get"), "{platform}");
    assert_eq!(hover_of(method, true), platform, "the method hover is the platform's");
}

const ACCEPT_MAP: &str = "// Параметры:\n//   Значение - Соответствие - значение.\nПроцедура \
                          ПринятьСоответствие(Значение)\nКонецПроцедуры\n\n";

#[test]
fn passing_the_map_to_a_map_parameter_reports_nothing() {
    let source = format!(
        "//- /test.bsl\n{ACCEPT_MAP}Процедура Тест()\n    Данные = Новый Соответствие;\n    \
         Данные.Вставить(\"сумма\", 1);\n    ПринятьСоответствие(Данные);\nКонецПроцедуры\n"
    );
    for map_keys in [true, false] {
        assert_eq!(case(&source, map_keys).mismatches(), [], "profile {map_keys}");
    }
}

// ---- 17 and 19: only a proof removes the argument mismatch ----------------------------------

const ACCEPT_NUMBER: &str = "// Параметры:\n//   Значение - Число - значение.\nПроцедура \
                             ПринятьЧисло(Значение)\nКонецПроцедуры\n\n";

fn accept_number(statements: &str) -> String {
    format!(
        "//- /test.bsl\n{ACCEPT_NUMBER}Процедура Тест(Условие)\n    Данные = Новый \
         Соответствие;{statements}\nКонецПроцедуры\n"
    )
}

#[test]
fn a_proven_key_passed_to_a_number_parameter_reports_nothing_and_the_prior_reports_one() {
    let source = accept_number(
        r#"
    Данные.Вставить("сумма", 1);
    ПринятьЧисло(Данные.Получить("сумма"));"#,
    );
    assert_eq!(case(&source, true).mismatches(), []);

    let prior = case(&source, false).mismatches();
    assert_eq!(prior.len(), 1, "{prior:?}");
    assert_eq!(prior[0].0, DiagnosticCode::TypeMismatchByDocComment);
    assert_eq!(prior[0].1, "Данные.Получить(\"сумма\")", "the finding sits on the argument");
    assert!(
        prior[0].2.contains("ожидалось 'Число', получено 'Неопределено'"),
        "the prior finding is the one observed before the change: {}",
        prior[0].2
    );
}

#[test]
fn an_unproven_key_passed_to_a_number_parameter_reports_exactly_once_as_before() {
    let source = accept_number(
        r#"
    Данные.Вставить("сумма", 1);
    Если Условие Тогда
        Данные.Вставить("б", 2);
    КонецЕсли;
    ПринятьЧисло(Данные.Получить("сумма"));"#,
    );
    let refined = case(&source, true).mismatches();
    let prior = case(&source, false).mismatches();
    assert_eq!(refined.len(), 1, "{refined:?}");
    assert_eq!(prior.len(), 1, "{prior:?}");
    assert_eq!(refined[0].0, prior[0].0);
    assert_eq!(refined[0].1, "Данные.Получить(\"сумма\")");
    assert_eq!(refined[0].1, prior[0].1, "the finding stays on the same argument");
    assert!(refined[0].2.contains("получено 'Число | Неопределено'"), "{}", refined[0].2);
    assert!(prior[0].2.contains("получено 'Неопределено'"), "{}", prior[0].2);
}

#[test]
fn only_the_unproven_argument_of_two_reports() {
    let source = accept_number(
        r#"
    Данные.Вставить("сумма", 1);
    ПринятьЧисло(Данные.Получить("сумма"));
    ПринятьЧисло(Данные.Получить("нет"));"#,
    );
    let refined = case(&source, true).mismatches();
    assert_eq!(refined.len(), 1, "{refined:?}");
    assert_eq!(refined[0].1, "Данные.Получить(\"нет\")");
    assert_eq!(case(&source, false).mismatches().len(), 2);
}

// ---- 18: a member sees the refined receiver and no new finding appears ----------------------

#[test]
fn a_member_chain_sees_the_refined_receiver_type() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    Данные.Получить("сумма").Метод();
    Х = Данные.Получить("сумма").Поле;"#,
    );
    let refined = case(&source, true);
    let prior = case(&source, false);
    assert_eq!(refined.reads(), ["Число", "Число"]);
    assert_eq!(prior.reads(), [PRIOR, PRIOR]);
    assert_eq!(refined.type_of_text("Данные.Получить(\"сумма\")"), "Число");
    assert_eq!(prior.type_of_text("Данные.Получить(\"сумма\")"), PRIOR);
    let codes = [DiagnosticCode::UnresolvedMethodCall, DiagnosticCode::UnresolvedField];
    assert_eq!(refined.findings(&codes), [], "refined profile");
    assert_eq!(prior.findings(&codes), [], "prior profile");
}

// ---- 29: mutually recursive readers converge ------------------------------------------------

#[test]
fn mutually_recursive_functions_reading_a_proven_key_converge() {
    let source = "//- /test.bsl\nФункция А(Д)\n    М = Новый Соответствие;\n    М.Вставить(\"а\", \
                  1);\n    Если Д Тогда\n        Возврат Б(Ложь);\n    КонецЕсли;\n    Возврат \
                  М.Получить(\"а\");\nКонецФункции\n\nФункция Б(Д)\n    М = Новый \
                  Соответствие;\n    М.Вставить(\"а\", 1);\n    Если Д Тогда\n        Возврат \
                  А(Ложь);\n    КонецЕсли;\n    Возврат М.Получить(\"а\");\nКонецФункции\n\n\
                  Процедура Тест()\n    Х = А(Истина);\nКонецПроцедуры\n";
    let refined = case(source, true);
    assert_eq!(refined.type_of_text("А(Истина)"), "Число");
    assert_eq!(refined.type_of_text("Б(Ложь)"), "Число");
    assert_eq!(case(source, false).type_of_text("А(Истина)"), PRIOR);
}

// ---- documented keys: shared fixtures -------------------------------------------------------

/// Greek omicron + capital sigma and omicron + small sigma: `fold_lower` keeps the capital final
/// sigma apart from the small medial one, so they are two keys.
const CAPITAL_SIGMA_KEY: &str = "\u{39F}\u{3A3}";
const SMALL_SIGMA_KEY: &str = "\u{3BF}\u{3C3}";

const KEYED_SUM: &str = "Соответствие:\n//     * \"сумма\" - Число - сумма документа.";
const READ_SUM: &str = "    Х = Данные.Получить(\"сумма\");";

/// A procedure `Обработать(Данные)` whose parameter slot is documented as `slot`.
fn param_doc(slot: &str, body: &str) -> String {
    format!(
        "//- /test.bsl\n// Параметры:\n//   Данные - {slot}\nПроцедура \
         Обработать(Данные)\n{body}\nКонецПроцедуры\n"
    )
}

/// A function `Ф()` returning a new map whose return slot is documented as `slot`, and a caller.
fn return_doc(slot: &str, caller: &str) -> String {
    format!(
        "//- /test.bsl\n// Возвращаемое значение:\n//   {slot}\nФункция Ф()\n    Возврат Новый \
         Соответствие;\nКонецФункции\n\nПроцедура Тест(Условие, Список)\n{caller}\nКонецПроцедуры\n"
    )
}

// ---- 21: a documented return reaches the caller ---------------------------------------------

#[test]
fn a_keyed_return_reaches_the_caller_through_a_local_and_a_direct_call() {
    let source = return_doc(
        KEYED_SUM,
        r#"    Д = Ф();
    Х = Д.Получить("сумма");
    У = Ф().Получить("сумма");"#,
    );
    refined_source(&source, &["Неопределено|Число", "Неопределено|Число"]);
}

#[test]
fn a_keyed_return_keeps_its_entries_on_the_function_result() {
    let source = return_doc(KEYED_SUM, "    Д = Ф();");
    assert_eq!(case(&source, true).type_of_text("Ф()"), "Соответствие{ключи}");
    assert_eq!(case(&source, false).type_of_text("Ф()"), "Соответствие");
}

#[test]
fn a_keyed_return_of_a_common_module_reaches_the_caller() {
    let source = "//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl\n// Возвращаемое \
                  значение:\n//   Соответствие:\n//     * \"сумма\" - Число\nФункция Ф() \
                  Экспорт\n    Возврат Новый Соответствие;\nКонецФункции\n\n//- /test.bsl\n\
                  Процедура Тест()\n    Д = ПервыйОбщийМодуль.Ф();\n    Х = \
                  Д.Получить(\"сумма\");\n    У = ПервыйОбщийМодуль.Ф().Получить(\"сумма\");\n\
                  КонецПроцедуры\n";
    refined_source(source, &["Неопределено|Число", "Неопределено|Число"]);
}

#[test]
fn a_keyed_return_in_english_reaches_the_caller() {
    let source = return_doc("Map:\n//     * \"сумма\" - Число", "    У = Ф().Получить(\"сумма\");");
    refined_source(&source, &["Неопределено|Число"]);
}

#[test]
fn a_return_without_key_items_is_not_refined() {
    let source = return_doc("Соответствие", "    У = Ф().Получить(\"сумма\");");
    assert_eq!(on(&source), [PRIOR]);
    assert_eq!(off(&source), [PRIOR]);
}

// ---- 22: a documented parameter is read inside the method -----------------------------------

#[test]
fn a_keyed_parameter_is_read_inside_its_method() {
    let source = param_doc(KEYED_SUM, READ_SUM);
    assert_eq!(on(&source), ["Неопределено|Число"]);
    assert_eq!(off(&source), ["?"], "the prior engine does not feed the parameter into the body");
}

#[test]
fn a_parameter_without_key_items_is_not_refined() {
    let source = param_doc("Соответствие - значение.", READ_SUM);
    assert_eq!(on(&source), ["?"]);
    assert_eq!(off(&source), ["?"]);
}

#[test]
fn a_keyed_parameter_in_english_is_read_inside_its_method() {
    let source = param_doc("Map:\n//     * \"сумма\" - Число", READ_SUM);
    assert_eq!(on(&source), ["Неопределено|Число"]);
}

// ---- 24: unknown keys and types do not narrow; Any absorbs; depth is one --------------------

#[test]
fn a_key_that_is_not_documented_keeps_the_platform_result() {
    let source = param_doc(
        KEYED_SUM,
        "    Х = Данные.Получить(\"нет\");\n    У = Данные.Получить(\"сумма\");",
    );
    assert_eq!(on(&source), [PRIOR, "Неопределено|Число"]);
}

#[test]
fn an_item_of_an_unknown_type_does_not_narrow_the_key() {
    let source = param_doc(
        "Соответствие:\n//     * \"а\" - НеТакогоТипа\n//     * \"сумма\" - Число",
        "    Х = Данные.Получить(\"а\");\n    У = Данные.Получить(\"сумма\");",
    );
    assert_eq!(on(&source), [PRIOR, "Неопределено|Число"], "{source}");
}

#[test]
fn an_unreadable_alternative_makes_the_whole_key_unknown() {
    let source = param_doc(
        "Соответствие:\n//     * \"а\" - Число, Соответствие из Строка\n//     * \"сумма\" - Число",
        "    Х = Данные.Получить(\"а\");\n    У = Данные.Получить(\"сумма\");",
    );
    assert_eq!(on(&source), [PRIOR, "Неопределено|Число"], "{source}");
}

#[test]
fn an_any_item_absorbs_the_undefined_union() {
    let source = param_doc(
        "Соответствие:\n//     * \"а\" - Произвольный\n//     * \"сумма\" - Число",
        "    Х = Данные.Получить(\"а\");\n    У = Данные.Получить(\"сумма\");",
    );
    assert_eq!(on(&source), ["Произвольный", "Неопределено|Число"], "{source}");
}

#[test]
fn a_nested_map_item_is_a_plain_map_beside_undefined_and_has_no_keys_of_its_own() {
    let source = param_doc(
        "Соответствие:\n//     * \"а\" - Соответствие:\n//       ** \"б\" - Число",
        "    Х = Данные.Получить(\"а\");\n    У = Данные.Получить(\"а\").Получить(\"б\");",
    );
    // The first read is the nested map beside `Неопределено`; the outer call of the chain reads
    // key `б` of a union receiver, which has no keys to offer; the inner call is the first read
    // again.
    assert_eq!(
        on(&source),
        ["Неопределено|Соответствие", PRIOR, "Неопределено|Соответствие"],
        "{source}"
    );
}

#[test]
fn keys_do_not_appear_under_an_array_a_structure_field_or_a_reference_to_a_foreign_map() {
    let under_array = param_doc(
        "Массив из Соответствие:\n//     * \"сумма\" - Число",
        "    Для Каждого Эл Из Данные Цикл\n        Х = Эл.Получить(\"сумма\");\n    КонецЦикла;",
    );
    let in_field = param_doc(
        "Структура:\n//     * Поле - Соответствие:\n//       ** \"сумма\" - Число",
        "    Х = Данные.Поле.Получить(\"сумма\");",
    );
    for source in [&under_array, &in_field] {
        for map_keys in [true, false] {
            let reads = case(source, map_keys).reads();
            assert_eq!(reads.len(), 1, "{source}");
            assert!(!reads[0].contains("Число"), "profile {map_keys}: {reads:?}\n{source}");
        }
    }

    let source = "//- /CommonModules/База/Ext/Module.bsl\n// Возвращаемое значение:\n//   \
                  Соответствие:\n//     * \"сумма\" - Число\nФункция Ключевое() Экспорт\n    \
                  Возврат Новый Соответствие;\nКонецФункции\n\n//- /test.bsl\n// Параметры:\n//   \
                  Данные - см. База.Ключевое\nПроцедура Обработать(Данные)\n    Х = \
                  Данные.Получить(\"сумма\");\nКонецПроцедуры\n";
    for map_keys in [true, false] {
        let reads = case(source, map_keys).reads();
        assert!(!reads[0].contains("Число"), "profile {map_keys}: {reads:?}");
    }
}

#[test]
fn a_direct_keyed_slot_is_the_control_of_the_depth_one_rule() {
    // The same item under the slot itself is refined, so the cases above are not green merely
    // because documentation keys never reach a body.
    let source = param_doc(KEYED_SUM, READ_SUM);
    assert_eq!(on(&source), ["Неопределено|Число"]);
}

// ---- 25: a changed or unnarrowed documented receiver is not refined -------------------------

#[test]
fn a_documented_parameter_changed_or_released_in_the_body_is_not_refined() {
    let events = [
        r#"Данные.Вставить("сумма", "х");"#,
        "Обработать(Данные);",
        "Данные = Новый Соответствие;",
        r#"Данные.Удалить("сумма");"#,
        "Данные.Очистить();",
        r#"Данные["сумма"] = "х";"#,
        "Данные.Свойство = 1;",
        "Копия = Данные;",
        r#"Выполнить("Х = 1");"#,
        "М = Новый Массив; М.Добавить(Данные);",
        "ВызватьИсключение Данные;",
        r#"Данные.Вставить(Ключ, 1);"#,
        "Данные.НеизвестныйМетод();",
        r#"?(Условие, Данные, Новый Соответствие).Вставить("сумма", "х");"#,
    ];
    for event in events {
        let source = format!(
            "//- /test.bsl\n// Параметры:\n//   Данные - {KEYED_SUM}\nПроцедура \
             Обработать(Данные, Ключ, Условие)\n    {event}\n    Х = \
             Данные.Получить(\"сумма\");\nКонецПроцедуры\n"
        );
        assert_eq!(on(&source), [PRIOR], "event {event}:\n{source}");
        if event.contains("Данные") {
            let defanged = source.replace(event, &event.replace("Данные", "Другое"));
            assert_eq!(
                on(&defanged),
                ["Неопределено|Число"],
                "the same statement aimed at another name keeps the answer:\n{defanged}"
            );
        }
    }
}

#[test]
fn returning_a_documented_parameter_releases_it() {
    let source = format!(
        "//- /test.bsl\n// Параметры:\n//   Данные - {KEYED_SUM}\nФункция Обработать(Данные)\n    Х = \
         Данные.Получить(\"сумма\");\n    Возврат Данные;\nКонецФункции\n"
    );
    assert_eq!(on(&source), [PRIOR], "{source}");
    let control = source.replace("Возврат Данные;", "Возврат Х;");
    assert_eq!(on(&control), ["Неопределено|Число"], "returning a result does not release it");
}

#[test]
fn a_documented_parameter_event_after_the_read_alone_disables_the_answer() {
    let source =
        param_doc(KEYED_SUM, "    Х = Данные.Получить(\"сумма\");\n    Данные.Очистить();");
    assert_eq!(on(&source), [PRIOR]);
}

#[test]
fn deleting_before_the_read_returns_the_platform_result_not_the_prior_engine_type() {
    // Deviation from the plan's wording: the plan expects the type "equal to the profile off".
    // Without the profile the parameter is not fed into the body at all, so the read has no
    // receiver type; with it, the unrefined read has the platform's `Неопределено`.
    let deleted =
        param_doc(KEYED_SUM, "    Данные.Удалить(\"сумма\");\n    Х = Данные.Получить(\"сумма\");");
    assert_eq!(on(&deleted), [PRIOR], "{deleted}");
    assert_eq!(off(&deleted), ["?"], "{deleted}");
    // The control: without `Удалить` the same read is refined.
    let intact = param_doc(KEYED_SUM, "    Х = Данные.Получить(\"сумма\");");
    assert_eq!(on(&intact), ["Неопределено|Число"]);
}

#[test]
fn an_unnarrowed_union_receiver_is_not_refined() {
    let source = param_doc(
        "Неопределено, Соответствие:\n//     * \"сумма\" - Число - сумма документа.",
        READ_SUM,
    );
    assert_eq!(on(&source), [PRIOR], "{source}");
}

#[test]
fn a_documented_local_from_a_call_is_changed_released_or_reassigned() {
    let events = [
        r#"Д.Вставить("сумма", "х");"#,
        "Обработать(Д);",
        "Д = Новый Соответствие;",
        "Д.Очистить();",
        "К = Д;",
    ];
    for event in events {
        let source = return_doc(
            KEYED_SUM,
            &format!("    Д = Ф();\n    {event}\n    Х = Д.Получить(\"сумма\");"),
        );
        assert_eq!(on(&source), [PRIOR], "event {event}:\n{source}");
    }
    let control = return_doc(KEYED_SUM, "    Д = Ф();\n    Х = Д.Получить(\"сумма\");");
    assert_eq!(on(&control), ["Неопределено|Число"]);
}

// ---- 3 (deviations): an alias of a documented map; folding with a final sigma ---------------

#[test]
fn a_copy_of_a_documented_parameter_is_not_refined_after_the_original_is_written() {
    let source = format!(
        "//- /test.bsl\n// Параметры:\n//   Данные - {KEYED_SUM}\nПроцедура Обработать(Данные)\n    \
         Копия = Данные;\n    Данные.Вставить(\"сумма\", \"текст\");\n    Результат = \
         Копия.Получить(\"сумма\");\nКонецПроцедуры\n"
    );
    let reads = on(&source);
    assert_eq!(reads, [PRIOR], "{source}");
    assert_ne!(reads, ["Неопределено|Число"], "the copy must not keep the documented answer");
}

#[test]
fn a_copy_of_a_documented_parameter_is_not_refined_even_without_a_write() {
    let source = format!(
        "//- /test.bsl\n// Параметры:\n//   Данные - {KEYED_SUM}\nПроцедура Обработать(Данные)\n    \
         Копия = Данные;\n    Результат = Копия.Получить(\"сумма\");\nКонецПроцедуры\n"
    );
    assert_eq!(on(&source), [PRIOR], "{source}");
}

#[test]
fn a_write_through_a_copy_hides_the_original_answer() {
    let source = format!(
        "//- /test.bsl\n// Параметры:\n//   Данные - {KEYED_SUM}\nПроцедура Обработать(Данные)\n    \
         Копия = Данные;\n    Копия.Вставить(\"сумма\", \"текст\");\n    Результат = \
         Данные.Получить(\"сумма\");\nКонецПроцедуры\n"
    );
    assert_eq!(on(&source), [PRIOR], "{source}");
}

#[test]
fn a_copy_of_a_documented_call_result_is_not_refined() {
    let source = return_doc(
        KEYED_SUM,
        "    Д = Ф();\n    Копия = Д;\n    Д.Вставить(\"сумма\", \"текст\");\n    Результат = \
         Копия.Получить(\"сумма\");",
    );
    assert_eq!(on(&source), [PRIOR], "{source}");
}

#[test]
fn keys_that_differ_only_by_a_final_sigma_stay_two_keys() {
    // `fold_lower` turns the capital final sigma into the final form, not into the medial one,
    // so these two spellings are different keys for the merge and for the lookup alike.
    let source = param_doc(
        &format!(
            "Соответствие:\n//     * \"{CAPITAL_SIGMA_KEY}\" - Число\n//     * \"{SMALL_SIGMA_KEY}\" - Строка"
        ),
        &format!(
            "    А = Данные.Получить(\"{SMALL_SIGMA_KEY}\");\n    Б = Данные.Получить(\"{CAPITAL_SIGMA_KEY}\");"
        ),
    );
    assert_eq!(on(&source), ["Неопределено|Строка", "Неопределено|Число"], "{source}");
}

#[test]
fn constructor_keys_that_differ_only_by_a_final_sigma_stay_two_keys() {
    let source = proc(&format!(
        "\n    Данные.Вставить(\"{CAPITAL_SIGMA_KEY}\", 1);\n    Данные.Вставить(\"{SMALL_SIGMA_KEY}\", \
         \"х\");\n    А = Данные.Получить(\"{SMALL_SIGMA_KEY}\");\n    Б = \
         Данные.Получить(\"{CAPITAL_SIGMA_KEY}\");"
    ));
    assert_eq!(on(&source), ["Строка", "Число"], "{source}");
}

// ---- 26: keyed and plain maps are compatible ------------------------------------------------

#[test]
fn a_plain_map_fits_a_keyed_parameter_and_a_keyed_return_fits_a_plain_one() {
    let keyed_param = "//- /test.bsl\n// Параметры:\n//   Данные - Соответствие:\n//     * \"сумма\" - Число\nПроцедура Проц(Данные)\nКонецПроцедуры\n\nПроцедура Тест()\n    Проц(Новый Соответствие);\nКонецПроцедуры\n";
    for map_keys in [true, false] {
        assert_eq!(case(keyed_param, map_keys).mismatches(), [], "profile {map_keys}");
    }
    let keyed_return = format!(
        "//- /test.bsl\n{ACCEPT_MAP}// Возвращаемое значение:\n//   {KEYED_SUM}\nФункция \
         Ф()\n    Возврат Новый Соответствие;\nКонецФункции\n\nПроцедура Тест2()\n    \
         ПринятьСоответствие(Ф());\nКонецПроцедуры\n"
    );
    for map_keys in [true, false] {
        assert_eq!(
            case(&keyed_return, map_keys).mismatches(),
            [],
            "profile {map_keys}\n{keyed_return}"
        );
    }
}

#[test]
fn a_keyed_parameter_still_rejects_a_value_that_is_not_a_map() {
    // Compatibility is key/value compatibility of two maps, not "any map-typed slot accepts
    // anything".
    let source = "//- /test.bsl\n// Параметры:\n//   Данные - Соответствие:\n//     * \"сумма\" - Число\nПроцедура Проц(Данные)\nКонецПроцедуры\n\nПроцедура Тест()\n    Проц(1);\nКонецПроцедуры\n";
    for map_keys in [true, false] {
        assert_eq!(case(source, map_keys).mismatches().len(), 1, "profile {map_keys}");
    }
}

#[test]
fn a_function_returning_a_new_plain_map_keeps_its_documented_keys() {
    let source = return_doc(KEYED_SUM, "    У = Ф().Получить(\"сумма\");");
    assert_eq!(on(&source), ["Неопределено|Число"]);
}

// ---- 27: documented duplicates and the size limit -------------------------------------------

#[test]
fn documented_items_that_differ_only_by_case_are_one_key() {
    let source = param_doc(
        "Соответствие:\n//     * \"Сумма\" - Число\n//     * \"сумма\" - Строка",
        "    А = Данные.Получить(\"сумма\");\n    Б = Данные.Получить(\"Сумма\");",
    );
    assert_eq!(on(&source), ["Неопределено|Строка|Число", "Неопределено|Строка|Число"]);
}

fn numbered_items(count: usize) -> String {
    (0..count).map(|index| format!("\n//     * \"ключ{index}\" - Число")).collect()
}

#[test]
fn sixty_four_documented_keys_answer_and_sixty_five_do_not() {
    let sixty_four = param_doc(
        &format!("Соответствие:{}", numbered_items(64)),
        "    А = Данные.Получить(\"ключ0\");\n    Б = Данные.Получить(\"ключ63\");",
    );
    assert_eq!(on(&sixty_four), ["Неопределено|Число", "Неопределено|Число"]);

    let sixty_five = param_doc(
        &format!("Соответствие:{}", numbered_items(65)),
        "    А = Данные.Получить(\"ключ0\");\n    Б = Данные.Получить(\"ключ64\");",
    );
    for reads in [on(&sixty_five), off(&sixty_five)] {
        assert!(reads.iter().all(|read| !read.contains("Число")), "{reads:?}");
    }
    let by_return = return_doc(
        &format!("Соответствие:{}", numbered_items(65)),
        "    А = Ф().Получить(\"ключ0\");",
    );
    assert_eq!(on(&by_return), [PRIOR]);
    assert_eq!(case(&by_return, true).type_of_text("Ф()"), "Соответствие", "no entries at all");
}

#[test]
fn documented_case_duplicates_do_not_spend_the_limit() {
    let source = param_doc(
        &format!("Соответствие:{}\n//     * \"КЛЮЧ0\" - Строка", numbered_items(64)),
        "    А = Данные.Получить(\"ключ0\");\n    Б = Данные.Получить(\"ключ63\");",
    );
    assert_eq!(on(&source), ["Неопределено|Строка|Число", "Неопределено|Число"]);
}

// ---- 28: the profile off removes the keys on every documentation path -----------------------

#[test]
fn the_profile_off_reads_every_documented_input_as_the_prior_engine_did() {
    // "Prior" for a parameter is the documentation without key items under the same profile.
    let keyed_param = param_doc(KEYED_SUM, READ_SUM);
    let plain_param = param_doc("Соответствие", READ_SUM);
    assert_eq!(off(&keyed_param), on(&plain_param));
    assert_eq!(off(&keyed_param), ["?"]);

    let keyed_return = return_doc(KEYED_SUM, "    У = Ф().Получить(\"сумма\");");
    let plain_return = return_doc("Соответствие", "    У = Ф().Получить(\"сумма\");");
    assert_eq!(off(&keyed_return), on(&plain_return));
    assert_eq!(off(&keyed_return), [PRIOR]);
    assert_eq!(
        case(&keyed_return, false).type_of_text("Ф()"),
        case(&plain_return, false).type_of_text("Ф()"),
    );
}

#[test]
fn the_profile_off_leaves_documented_argument_findings_as_before() {
    let call = "    ПринятьЧисло(Ф().Получить(\"сумма\"));";
    let with_docs = |slot: &str| {
        format!(
            "//- /test.bsl\n{ACCEPT_NUMBER}{}",
            return_doc(slot, call).replacen("//- /test.bsl\n", "", 1)
        )
    };
    let keyed = with_docs(KEYED_SUM);
    let plain = with_docs("Соответствие");

    let with_profile = case(&keyed, true).mismatches();
    assert_eq!(with_profile.len(), 1, "{with_profile:?}");
    assert!(with_profile[0].2.contains("получено 'Число | Неопределено'"), "{}", with_profile[0].2);

    // Without the profile the keyed documentation reports what the plain documentation reports.
    let without = case(&keyed, false).mismatches();
    let prior = case(&plain, true).mismatches();
    assert_eq!(without.len(), 1, "{without:?}");
    assert_eq!(without, prior);
}

#[test]
fn a_keyed_map_renders_as_the_plain_map_name_in_both_languages_without_its_keys() {
    // The keyed map renders as the plain `Соответствие`/`Map`; the key list is not shown.
    let source = "//- /test.bsl\n// Параметры:\n//   Данные - Соответствие:\n//     * \"сумма\" - Число\nПроцедура Обработать(Данные)\n    Н = Дан$0ные;\nКонецПроцедуры\n";
    let (text, offset) = with_cursor(source);
    for map_keys in [true, false] {
        let case = case(&text, map_keys);
        let ru = case.hover(offset, ide::Locale::Ru);
        let en = case.hover(offset, ide::Locale::En);
        if map_keys {
            assert!(ru.contains("**Тип:** Соответствие\n"), "{ru}");
            assert!(en.contains("**Тип:** Map\n"), "{en}");
        }
        assert!(!ru.contains("сумма") && !en.contains("сумма"), "no key list: {ru} {en}");
    }
}

// ---- 30: the boundaries of applicability ----------------------------------------------------

#[test]
fn a_fixed_map_and_an_index_read_are_not_refined() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    Фикс = Новый ФиксированноеСоответствие(Данные);
    А = Фикс.Получить("сумма");
    Б = Данные["сумма"];"#,
    );
    let refined = case(&source, true);
    let prior = case(&source, false);
    assert_eq!(refined.type_of_text("Данные[\"сумма\"]"), prior.type_of_text("Данные[\"сумма\"]"));
    let fixed_on = refined.reads();
    let fixed_off = prior.reads();
    assert_eq!(fixed_on, fixed_off, "the fixed map read is the platform's");
    assert_eq!(fixed_on, ["Произвольный"]);
}

#[test]
fn an_undocumented_parameter_or_return_of_another_body_brings_no_keys() {
    let source = "//- /test.bsl\nФункция Ф()\n    Д = Новый Соответствие;\n    Д.Вставить(\"сумма\", 1);\n    Возврат Д;\nКонецФункции\n\nПроцедура Обработать(Данные)\n    Х = Данные.Получить(\"сумма\");\nКонецПроцедуры\n\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    Данные.Вставить(\"сумма\", 1);\n    Обработать(Данные);\n    У = Ф().Получить(\"сумма\");\nКонецПроцедуры\n";
    let reads = on(source);
    assert_eq!(reads, ["?", PRIOR], "no key crosses a body boundary");
    assert_eq!(reads, off(source));
}

#[test]
fn the_platform_version_settings_do_not_change_the_refinement() {
    let source = proc(
        r#"
    Данные.Вставить("сумма", 1);
    Р = Данные.Получить("сумма");"#,
    );
    for (target, min) in [(None, None), (Some("8.3.10"), None), (Some("8.3.27"), Some("8.3.10"))] {
        let mut refined = case(&source, true);
        refined.db.set_target_platform_version(target.map(std::sync::Arc::from));
        refined.db.set_min_platform_version(min.map(std::sync::Arc::from));
        assert_eq!(refined.reads(), ["Число"], "target {target:?} min {min:?}");
    }
}

// ---- 28: the reference-resolving path follows the profile too -------------------------------

const SEE_TARGET: &str = "//- /CommonModules/База/Ext/Module.bsl\n// Возвращаемое значение:\n//   \
                          Структура:\n//    * Таймаут - Число - секунды.\n//    * Адрес - Строка - \
                          куда.\nФункция Создать() Экспорт\n\tВозврат Новый Структура;\nКонецФункции\n";

fn see_slot_source() -> String {
    format!(
        "{SEE_TARGET}\n//- /test.bsl\n// Параметры:\n//   Данные - Соответствие:\n//     * \
         \"настройки\" - см. База.Создать\n// Возвращаемое значение:\n//   Соответствие:\n//     * \
         \"настройки\" - см. База.Создать\nФункция Читает(Данные) Экспорт\n\tВозврат Новый \
         Соответствие;\nКонецФункции\n"
    )
}

/// `(entry names, field names of the entry "настройки")` of a type that is expected to be a map.
fn keyed_shape(db: &RootDatabaseImpl, ty: TypeId) -> Option<(Vec<String>, Vec<String>)> {
    let TypeKind::Map(facet) = db.lookup_type(ty).clone() else {
        panic!("a map expected, got {:?}", db.lookup_type(ty));
    };
    let entries = facet.entries.as_ref()?;
    let names = entries.fields.iter().map(|field| field.name.to_string()).collect();
    let inner = entries.fields.iter().find(|field| field.name == "настройки").map(|field| {
        let TypeKind::Structure(structure) = db.lookup_type(field.ty).clone() else {
            return Vec::new();
        };
        let mut fields: Vec<String> = structure
            .fields
            .as_ref()
            .map(|projection| projection.fields.iter().map(|f| f.name.to_string()).collect())
            .unwrap_or_default();
        fields.sort();
        fields
    });
    Some((names, inner.unwrap_or_default()))
}

fn see_signature(map_keys: bool) -> (RootDatabaseImpl, hir::DocSeeSignature) {
    let Case { db, file, .. } = case(&see_slot_source(), map_keys);
    let symbol_tree = hir::DefDatabase::symbol_tree_ref(&db, hir::ModuleId::new(file));
    let symbol = symbol_tree.find_method(&hir::Name::new("Читает")).expect("method declared");
    let resolved =
        hir::doc_see_signature_query(&db, hir::MethodIdInput::new(&db, symbol.id)).as_ref().clone();
    (db, resolved)
}

#[test]
fn a_keyed_slot_that_names_a_reference_resolves_it_inside_the_entry_with_the_profile_on() {
    let (db, resolved) = see_signature(true);
    let param = resolved.param(0).expect("the parameter slot is resolved");
    let ret = resolved.ret.expect("the return slot is resolved");
    let expected =
        Some((vec!["настройки".to_owned()], vec!["Адрес".to_owned(), "Таймаут".to_owned()]));
    assert_eq!(keyed_shape(&db, param), expected);
    assert_eq!(keyed_shape(&db, ret), expected);
}

#[test]
fn the_same_slots_resolve_to_plain_maps_with_the_profile_off() {
    let (_, resolved) = see_signature(false);
    // The key items are not read, so the slots name no target and there is nothing to resolve.
    assert!(resolved.param(0).is_none() && resolved.ret.is_none(), "{resolved:?}");
}

// ---- 31: structures keep their behaviour under both profiles --------------------------------

/// The type of the first expression with the given text, under both profiles.
#[track_caller]
fn both_profiles(source: &str, text: &str) -> String {
    let with = case(source, true).type_of_text(text);
    let without = case(source, false).type_of_text(text);
    assert_eq!(with, without, "`{text}` must not depend on mapKeyTypes:\n{source}");
    with
}

const BUILD: &str = "//- /test.bsl\nФункция Построить()\n    Стр = Новый Структура;\n    \
                     Стр.Вставить(\"Цена\", 5);\n    Стр.Вставить(\"Цена\", \"x\");\n    \
                     Стр.Вставить(\"Имя\", \"п\");\n    Стр.Вставить(\"Вложенная\", Новый \
                     Структура(\"Город\"));\n    Возврат Стр;\nКонецФункции\n\nПроцедура \
                     Тест()\n    Р = Построить();\n    Ц = Р.Цена;\n    В = Р.Вложенная;\n    Г = \
                     Р.Вложенная.Город;\nКонецПроцедуры\n";

#[test]
fn a_structure_is_built_from_its_inserts_with_the_last_write_of_a_key_winning() {
    assert_eq!(
        both_profiles(BUILD, "Построить()"),
        "Структура{Вложенная:Структура{Город:?};Имя:Строка;Цена:Строка}"
    );
}

#[test]
fn a_literal_scalar_field_read_stays_unknown_and_a_nested_structure_is_reachable() {
    assert_eq!(both_profiles(BUILD, "Р.Цена"), "?");
    assert_eq!(both_profiles(BUILD, "Р.Вложенная"), "Структура{Город:?}");
    assert_eq!(both_profiles(BUILD, "Р.Вложенная.Город"), "?");
}

#[test]
fn documented_structures_references_and_arrays_read_the_same_under_both_profiles() {
    let documented = "//- /test.bsl\n// Возвращаемое значение:\n//   Структура:\n//    * Поле - \
                      Число - число.\n//    * Список - Массив из Структура:\n//     ** Имя - \
                      Строка - имя.\nФункция Ф() Экспорт\n    Возврат Новый Структура;\n\
                      КонецФункции\n\nПроцедура Тест()\n    Р = Ф();\n    П = Р.Поле;\n    С = \
                      Р.Список;\nКонецПроцедуры\n";
    assert_eq!(
        both_profiles(documented, "Ф()"),
        "Структура{Поле:Число;Список:Массив<Структура{Имя:Строка}>}"
    );
    both_profiles(documented, "Р.Поле");
    let by_reference = format!(
        "{SEE_TARGET}\n//- /test.bsl\n// Возвращаемое значение:\n//   см. База.Создать\nФункция \
         Читает() Экспорт\n    Возврат Новый Структура;\nКонецФункции\n\nПроцедура Тест()\n    Р \
         = Читает();\nКонецПроцедуры\n"
    );
    assert_eq!(both_profiles(&by_reference, "Читает()"), "Структура{Адрес:Строка;Таймаут:Число}");
}

#[test]
fn a_structure_parameter_is_fed_into_the_body_under_both_profiles() {
    let source = "//- /test.bsl\n// Параметры:\n//   Данные - Структура:\n//    * Поле - Число - \
                  число.\nПроцедура Обработать(Данные)\n    Н = Дан$0ные;\nКонецПроцедуры\n";
    let with = hover_of(source, true);
    assert_eq!(with, hover_of(source, false));
    assert!(with.contains("**Тип:** Структура(Поле)") && with.contains("Поле: Число"), "{with}");
}

// ---- additional controls --------------------------------------------------------------------

#[test]
fn a_constructor_inside_a_branch_is_a_root_for_everything_inside_that_branch() {
    // The mention rule is about the constructor's own subtree: a name used only below the
    // constructor, in the same branch, keeps its proof.
    refined_source(
        "//- /test.bsl\nПроцедура Тест(Условие)\n    Если Условие Тогда\n        Д = Новый \
         Соответствие;\n        Д.Вставить(\"а\", 1);\n        Х = Д.Получить(\"а\");\n    \
         КонецЕсли;\nКонецПроцедуры\n",
        &["Число"],
    );
}

#[test]
fn iterating_over_the_map_does_not_release_it() {
    refined(
        r#"
    Данные.Вставить("сумма", 1);
    Для Каждого Эл Из Данные Цикл
    КонецЦикла;
    Р = Данные.Получить("сумма");"#,
        &["Число"],
    );
}

#[test]
fn a_map_inserted_into_another_map_leaves_the_tracked_scope() {
    unrefined(
        r#"
    Другая = Новый Соответствие;
    Данные.Вставить("сумма", 1);
    Другая.Вставить("вложенная", Данные);
    Р = Данные.Получить("сумма");"#,
        1,
    );
}

#[test]
fn a_map_inserted_into_itself_leaves_the_tracked_scope() {
    unrefined(
        r#"
    Данные.Вставить("сумма", 1);
    Данные.Вставить("сам", Данные);
    Р = Данные.Получить("сумма");"#,
        1,
    );
}

/// The return slot lists a reference and, last, a keyed map: the reference makes the slot go
/// through the resolving path, and the keyed map must still follow the profile there.
#[test]
fn a_union_beside_a_reference_loses_its_keys_with_the_profile_off() {
    let source = format!(
        "{SEE_TARGET}\n//- /test.bsl\n// Возвращаемое значение:\n//   см. База.Создать, \
         Соответствие:\n//     * \"сумма\" - Число\nФункция Читает() Экспорт\n\tВозврат Новый \
         Соответствие;\nКонецФункции\n"
    );
    let shapes = |map_keys: bool| -> Vec<Option<Vec<String>>> {
        let Case { db, file, .. } = case(&source, map_keys);
        let symbol_tree = hir::DefDatabase::symbol_tree_ref(&db, hir::ModuleId::new(file));
        let symbol = symbol_tree.find_method(&hir::Name::new("Читает")).expect("method declared");
        let resolved = hir::doc_see_signature_query(&db, hir::MethodIdInput::new(&db, symbol.id))
            .as_ref()
            .clone();
        let ret = resolved.ret.expect("the return slot names a target and is resolved");
        let members = match db.lookup_type(ret).clone() {
            TypeKind::Union(members) => members.to_vec(),
            _ => vec![ret],
        };
        members
            .into_iter()
            .filter_map(|member| match db.lookup_type(member).clone() {
                TypeKind::Map(facet) => Some(facet.entries.as_ref().map(|entries| {
                    entries.fields.iter().map(|field| field.name.to_string()).collect()
                })),
                _ => None,
            })
            .collect()
    };
    assert_eq!(shapes(true), [Some(vec!["сумма".to_owned()])], "the keyed map arm keeps its key");
    assert_eq!(shapes(false), [None], "the same arm is a plain map without the profile");
}
