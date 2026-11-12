use hir::{Body, Expr, ExprId, ExprIdx, IdConversion, Literal, LocalRange};
use lexer::is_identifier_letter;
use stdx::case::fold_lower_per_char;

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MESSAGE: &str = "Ключ структуры не является идентификатором: платформа отвечает «Задано неправильное имя атрибута структуры»";

/// Lowercased spellings (Russian and English) of the pieces the predicate matches.
const CONSTRUCTED_TYPES: &[&str] =
    &["структура", "structure", "фиксированнаяструктура", "fixedstructure"];
const KEY_METHODS: &[&str] = &["вставить", "insert", "свойство", "property", "удалить", "delete"];

/// Reports a string literal that names a structure key but is not an identifier: the
/// platform refuses it at runtime (measured on 8.3.27.2214).
///
/// * `Новый Структура("<ключи>")` / `Новый ФиксированнаяСтруктура("<ключи>")` take a
///   COMMA-SEPARATED list: each entry is trimmed, an empty entry is skipped, and an
///   entirely empty literal is legal. One finding per call, on the literal.
/// * `Вставить` / `Свойство` / `Удалить` take ONE key, unsplit; whitespace around it is
///   trimmed (`"Ключ "` is legal, a key of whitespace only is refused like an empty one).
///   Judged only when the receiver is known to be a structure: a `Соответствие` does
///   not restrict its keys, so an unknown or any other receiver type yields nothing.
///
/// A key that is not a string literal is not statically known and is never reported.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::StructureKeyIsNotIdentifier;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let body = ctx.body();
    if body.is_lowering_incomplete() {
        return;
    }
    let mut literals: Vec<ExprIdx> = Vec::new();
    for (_, expr) in body.exprs_iter() {
        match expr {
            Expr::New { type_name: Some(name), args }
                if is_one_of(name.as_str(), CONSTRUCTED_TYPES) =>
            {
                if let Some(literal) = args.first().copied() {
                    if string_literal(body, literal).is_some_and(constructor_list_is_bad) {
                        literals.push(literal);
                    }
                }
            }
            Expr::MethodCall { receiver, method, args } => {
                if key_method_call_is_bad(ctx, *receiver, method.as_str(), args) {
                    literals.push(args[0]);
                }
            }
            Expr::Call { callee, args } => {
                let Expr::Field { base, field } = body.expr_idx(*callee) else { continue };
                if key_method_call_is_bad(ctx, *base, field.as_str(), args) {
                    literals.push(args[0]);
                }
            }
            _ => {}
        }
    }

    for literal in literals {
        let Some(range) = ctx.source_map().expr_range(ExprId::from_idx(literal)) else {
            continue;
        };
        acc.push(Diagnostic {
            code,
            message: MESSAGE.to_string(),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn key_method_call_is_bad(
    ctx: &BodyContext,
    receiver: ExprIdx,
    method: &str,
    args: &[ExprIdx],
) -> bool {
    if !is_one_of(method, KEY_METHODS) {
        return false;
    }
    let Some(&first) = args.first() else { return false };
    let Some(key) = string_literal(ctx.body(), first) else { return false };
    if is_identifier(key.trim()) {
        return false;
    }
    receiver_is_structure(ctx, receiver)
}

/// The receiver is typed by the inference engine and must come out as exactly a structure
/// (not a union, not unknown, not a map).
fn receiver_is_structure(ctx: &BodyContext, receiver: ExprIdx) -> bool {
    ctx.infer()
        .type_id_of_expr(ExprId::from_idx(receiver))
        .is_some_and(|ty| ctx.kernel_type_is_structure(ty))
}

/// A literal that spans lines (`|` continuation) is not judged: its value is not the text
/// between the quotes.
fn string_literal(body: &Body, expr: ExprIdx) -> Option<&str> {
    match body.expr_idx(expr) {
        Expr::Literal(Literal::String(text)) if !text.contains(['\n', '\r']) => Some(text.as_str()),
        _ => None,
    }
}

/// Whitespace around an entry is trimmed and an empty entry is skipped, as the platform does.
fn constructor_list_is_bad(list: &str) -> bool {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| !is_identifier(entry))
}

/// First character a letter or `_`; the rest letters, ASCII digits or `_`. A bare `_` is valid.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return false };
    (first == '_' || is_identifier_letter(first))
        && chars.all(|c| c == '_' || c.is_ascii_digit() || is_identifier_letter(c))
}

fn is_one_of(name: &str, folded_options: &[&str]) -> bool {
    folded_options.contains(&fold_lower_per_char(name).as_str())
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for, range_to_line_col};
    use crate::DiagnosticCode;

    fn run(body: &str) -> Vec<crate::Diagnostic> {
        let code = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        check_diagnostics_for(&code, DiagnosticCode::StructureKeyIsNotIdentifier)
    }

    fn constructor(key: &str) -> Vec<crate::Diagnostic> {
        run(&format!("    С = Новый Структура(\"{key}\");"))
    }

    #[test]
    fn dot_space_hyphen_and_leading_digit_in_a_constructor_key_are_flagged() {
        for key in ["а.б", "а б", "а-б", "1а", "а, б в"] {
            let d = constructor(key);
            assert_eq!(d.len(), 1, "{key}: {d:?}");
            assert_eq!(
                d[0].message,
                "Ключ структуры не является идентификатором: платформа отвечает «Задано неправильное имя атрибута структуры»"
            );
        }
    }

    #[test]
    fn finding_is_anchored_on_the_literal_once_per_call() {
        let body = "    С = Новый Структура(\"а.б, в-г\", 1, 2);";
        let d = run(body);
        assert_eq!(d.len(), 1, "{d:?}");
        let text = format!("Процедура Проба()\n{body}\nКонецПроцедуры\n");
        let (line, start, _, end) = range_to_line_col(&text, d[0].range);
        assert_eq!((line, start, end), (1, 24, 34), "{:?}", d[0]);
    }

    #[test]
    fn constructor_list_with_trimmed_or_skipped_entries_is_not_flagged() {
        for key in ["а, б", "а,б", " а , б ", "а,, б", "а,", ""] {
            let d = constructor(key);
            assert!(d.is_empty(), "{key:?}: {d:?}");
        }
    }

    #[test]
    fn valid_identifiers_are_not_flagged() {
        for key in ["_а", "а_1", "Name1", "_", "ЁЙ", "Ключ"] {
            let d = constructor(key);
            assert!(d.is_empty(), "{key}: {d:?}");
        }
    }

    #[test]
    fn non_ascii_digits_and_number_signs_are_not_identifier_characters() {
        // Arabic-Indic digit, Roman numeral, superscript two: numeric to `char::is_alphanumeric`,
        // but not BSL identifier characters.
        for key in ["Ключ٢", "КлючⅫ", "Ключ²"] {
            let d = constructor(key);
            assert_eq!(d.len(), 1, "{key}: {d:?}");
            let d = run(&format!("    С = Новый Структура;\n    С.Вставить(\"{key}\", 1);"));
            assert_eq!(d.len(), 1, "{key}: {d:?}");
        }
    }

    #[test]
    fn insert_of_a_bad_key_on_a_structure_is_flagged() {
        let d = run("    С = Новый Структура;\n    С.Вставить(\"а.б\", 1);");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn insert_of_an_empty_key_on_a_structure_is_flagged() {
        let d = run("    С = Новый Структура;\n    С.Вставить(\"\", 1);");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn property_and_delete_of_a_bad_key_on_a_structure_are_flagged() {
        let d = run("    С = Новый Структура;\n    С.Свойство(\"а.б\");\n    С.Удалить(\"а б\");");
        assert_eq!(d.len(), 2, "{d:?}");
    }

    #[test]
    fn insert_of_a_key_with_surrounding_whitespace_on_a_structure_is_not_flagged() {
        let d = run("    С = Новый Структура;
    С.Вставить(\"Ключ  \", 1);
    С.Вставить(\"  Ключ\", 1);
    С.Свойство(\"Ключ \");
    С.Удалить(\" Ключ\");");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn insert_of_a_key_made_of_whitespace_only_on_a_structure_is_flagged() {
        let d = run("    С = Новый Структура;
    С.Вставить(\"   \", 1);");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn insert_of_a_valid_key_on_a_structure_is_not_flagged() {
        let d = run("    С = Новый Структура;\n    С.Вставить(\"Ключ\", 1);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn insert_of_a_bad_key_on_a_map_is_not_flagged() {
        let d = run(
            "    С = Новый Соответствие;\n    С.Вставить(\"а.б\", 1);\n    С.Вставить(\"\", 1);",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn insert_on_a_receiver_assigned_a_structure_in_one_branch_and_a_map_in_the_other_is_not_flagged(
    ) {
        // Inference gives this receiver the plain map type (`Соответствие`), not a union.
        let d = run("    Если Условие Тогда
        С = Новый Структура;
    Иначе
        С = Новый Соответствие;
    КонецЕсли;
    С.Вставить(\"а.б\", 1);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn insert_on_a_receiver_of_unknown_type_is_not_flagged() {
        let d = run("    Неизвестно.Вставить(\"а.б\", 1);\n    Параметр.Вставить(\"\", 1);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn key_taken_from_a_variable_is_not_flagged() {
        let d =
            run("    Ключ = \"а.б\";\n    С = Новый Структура(Ключ);\n    С.Вставить(Ключ, 1);");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn english_spellings_are_flagged_the_same_way() {
        let d = run(
            "    S = New Structure(\"a.b\");\n    S.Insert(\"a b\", 1);\n    S.Property(\"a-b\");\n    S.Delete(\"1a\");\n    F = New FixedStructure(\"a.b\");",
        );
        assert_eq!(d.len(), 5, "{d:?}");
    }

    #[test]
    fn fixed_structure_constructor_is_judged_like_the_structure_one() {
        let d = run("    С = Новый ФиксированнаяСтруктура(\"а.б\");\n    Д = Новый ФиксированнаяСтруктура(\"а, б\");");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn russian_and_english_parity_fixtures_each_emit_one_identical_finding() {
        let ru = check_diagnostics_for(
            "Процедура Тест()\n    С = Новый Структура(\"а.б\");\nКонецПроцедуры",
            DiagnosticCode::StructureKeyIsNotIdentifier,
        );
        let en = check_diagnostics_for(
            "Procedure Test()\n    S = New Structure(\"a.b\");\nEndProcedure",
            DiagnosticCode::StructureKeyIsNotIdentifier,
        );
        assert_eq!(ru.len(), 1, "{ru:?}");
        assert_eq!(en.len(), 1, "{en:?}");
        assert_eq!(ru[0].message, en[0].message);
        assert_eq!(ru[0].severity, en[0].severity);
    }
}
