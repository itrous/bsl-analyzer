//! Lowering of a method header: which declared names raise
//! `BodyDiagnostic::BuiltinFunctionNameAsDeclaration`.
//!
//! The event is decided on the syntax of the declaration alone — no database, no platform
//! help, no module metadata, no target version — so these tests lower a parsed method with
//! none of them installed.

use hir_def::{lower_method, BodyDiagnostic};
use syntax::{SyntaxKind, SyntaxNode};
use text_size::{TextRange, TextSize};

/// The `nth` procedure or function of `source`, in file order.
fn method(source: &str, nth: usize) -> SyntaxNode {
    let parsed = parser::parse(source);
    assert!(!parsed.has_errors(), "parse errors: {:?}", parsed.errors());
    parsed
        .syntax_node()
        .descendants()
        .filter(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .nth(nth)
        .expect("the method exists")
}

/// The declared-name events of the `nth` method as `(name, range in the method's own
/// coordinates)`, lowered the way its kind says.
fn declaration_events(source: &str, nth: usize) -> Vec<(String, TextRange)> {
    let node = method(source, nth);
    let is_function = node.kind() == SyntaxKind::FUNCTION_DEF;
    lower_method(&node, is_function)
        .diagnostics
        .into_iter()
        .filter_map(|diagnostic| match diagnostic {
            BodyDiagnostic::BuiltinFunctionNameAsDeclaration { name, range } => {
                Some((name, range.in_root()))
            }
            _ => None,
        })
        .collect()
}

fn reserved_word_events(source: &str, nth: usize) -> usize {
    let node = method(source, nth);
    let is_function = node.kind() == SyntaxKind::FUNCTION_DEF;
    lower_method(&node, is_function)
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic, BodyDiagnostic::ReservedWordAsMethodName { .. }))
        .count()
}

#[test]
fn the_event_sits_on_the_name_token_in_the_methods_own_coordinates() {
    let source = "\
Функция ИндексВМассиве(Массив, Значение) Экспорт
    Возврат Массив.Найти(Значение);
КонецФункции

Функция Найти(Значение) Экспорт
    Возврат Значение;
КонецФункции
";
    // The call inside the first method is not a declaration.
    assert_eq!(declaration_events(source, 0), Vec::new());

    // `Функция ` is 15 bytes, `Найти` 10: the range is the name, counted from the start of
    // the second method — not from the start of the file, where the first method comes before.
    let events = declaration_events(source, 1);
    let start = TextSize::from(u32::try_from("Функция ".len()).unwrap());
    let end = start + TextSize::from(u32::try_from("Найти".len()).unwrap());
    assert_eq!(events, vec![("Найти".to_string(), TextRange::new(start, end))]);
}

#[test]
fn every_spelling_of_the_pair_is_an_event_under_the_spelling_written() {
    for name in ["Найти", "нАйТи", "НАЙТИ", "найти"] {
        let source = format!("Функция {name}(Значение)\n    Возврат Значение;\nКонецФункции\n");
        let events = declaration_events(&source, 0);
        assert_eq!(events.len(), 1, "{name}: {events:?}");
        assert_eq!(events[0].0, name);
    }
    for name in ["Find", "fInD", "FIND", "find"] {
        let source = format!("Function {name}(Value)\n    Return Value;\nEndFunction\n");
        let events = declaration_events(&source, 0);
        assert_eq!(events.len(), 1, "{name}: {events:?}");
        assert_eq!(events[0].0, name);
    }
}

#[test]
fn only_a_function_is_an_event() {
    assert_eq!(declaration_events("Процедура Найти(Значение)\nКонецПроцедуры\n", 0), Vec::new());
    assert_eq!(declaration_events("Procedure Find(Value)\nEndProcedure\n", 0), Vec::new());
    // Control: the same header as a function is.
    assert_eq!(declaration_events("Функция Найти(Значение)\nКонецФункции\n", 0).len(), 1);
    assert_eq!(declaration_events("Function Find(Value)\nEndFunction\n", 0).len(), 1);
}

#[test]
fn neighbouring_names_are_not_events() {
    for name in ["СтрНайти", "НайтиЗначение", "ПолучитьЗначение", "Найти1", "ПоискНайти"]
    {
        let source = format!("Функция {name}(Значение)\n    Возврат Значение;\nКонецФункции\n");
        assert_eq!(declaration_events(&source, 0), Vec::new(), "{name}");
    }
    for name in ["StrFind", "FindValue", "GetValue", "Find1", "FindIt"] {
        let source = format!("Function {name}(Value)\n    Return Value;\nEndFunction\n");
        assert_eq!(declaration_events(&source, 0), Vec::new(), "{name}");
    }
}

#[test]
fn a_reserved_word_stays_the_reserved_word_event() {
    // The pair is an identifier, so the old event never fires on it...
    assert_eq!(reserved_word_events("Функция Найти()\nКонецФункции\n", 0), 0);
    assert_eq!(reserved_word_events("Function Find()\nEndFunction\n", 0), 0);
    // ...and a keyword used as a name never raises the new one.
    assert_eq!(reserved_word_events("Функция Новый()\n    Возврат 1;\nКонецФункции\n", 0), 1);
    assert_eq!(
        declaration_events("Функция Новый()\n    Возврат 1;\nКонецФункции\n", 0),
        Vec::new()
    );
}
