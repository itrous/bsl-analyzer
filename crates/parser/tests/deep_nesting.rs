//! Nesting deeper than the parser descends is a syntax error, not a crash.
//!
//! Every input here is far deeper than any stack holds when each level is a
//! recursive call or a node of the tree, and each is parsed on a thread with
//! the 1 MiB stack of a Windows main thread: the parse has to come back with
//! the whole text in the tree and the depth reported where it was exceeded.

const DEPTH: usize = 100_000;
const TOO_DEEP: &str = "Слишком глубокая вложенность";

fn on_small_stack<T: Send + 'static>(label: &str, parse: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name(format!("deep-nesting-{label}"))
        .stack_size(1024 * 1024)
        .spawn(parse)
        .expect("spawn small-stack parser thread")
        .join()
        .unwrap_or_else(|_| panic!("{label}: deep nesting must not abort the parse"))
}

fn too_deep_reports(parse: &syntax::Parse<syntax::SyntaxNode>) -> usize {
    parse.errors().iter().filter(|error| error.message() == TOO_DEEP).count()
}

fn assert_reported(
    label: &str,
    input: String,
    parse: fn(&str) -> syntax::Parse<syntax::SyntaxNode>,
) {
    let (text, reports) = on_small_stack(label, move || {
        let parse = parse(&input);
        let text = parse.syntax_node().text().to_string() == input;
        (text, too_deep_reports(&parse))
    });
    assert!(text, "{label}: the tree must keep every byte of the input");
    assert!(reports > 0, "{label}: the exceeded depth must be reported");
}

fn in_method(statement: String) -> String {
    format!("Процедура П()\n{statement}\nКонецПроцедуры\n")
}

fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}

#[test]
fn deep_bsl_nesting_is_reported_not_overflowed() {
    let cases = [
        ("parens", in_method(format!("А = {};", nested("(", "1", ")", DEPTH)))),
        ("unary-minus", in_method(format!("А = {}1;", "-".repeat(DEPTH)))),
        ("not", in_method(format!("А = {}Истина;", "НЕ ".repeat(DEPTH)))),
        ("index", in_method(format!("А = {};", nested("а[", "1", "]", DEPTH)))),
        ("call-args", in_method(format!("А = {};", nested("ф(", "1", ")", DEPTH)))),
        ("ternary", in_method(format!("А = {};", nested("?(1, 2, ", "3", ")", DEPTH)))),
        ("if", in_method(nested("Если 1 Тогда ", "А = 1;", " КонецЕсли;", DEPTH))),
        ("while", in_method(nested("Пока 1 Цикл ", "А = 1;", " КонецЦикла;", DEPTH))),
        ("try", in_method(nested("Попытка ", "А = 1;", " Исключение КонецПопытки;", DEPTH))),
        ("preprocessor-if", nested("#Если Сервер Тогда\n", "А = 1;\n", "#КонецЕсли\n", DEPTH)),
        (
            "torn-preprocessor-if",
            in_method(format!(
                "А = {};",
                nested("\n#Если Сервер Тогда\n(", "1", "\n#КонецЕсли\n)", DEPTH)
            )),
        ),
        (
            "preprocessor-condition",
            format!("#Если {}Сервер Тогда\n#КонецЕсли\n", "НЕ ".repeat(DEPTH)),
        ),
    ];
    for (label, input) in cases {
        assert_reported(label, input, parser::parse);
    }
}

#[test]
fn deep_sdbl_nesting_is_reported_not_overflowed() {
    let cases = [
        ("parens", format!("ВЫБРАТЬ {}", nested("(", "1", ")", DEPTH))),
        ("not", format!("ВЫБРАТЬ 1 ГДЕ {}ИСТИНА", "НЕ ".repeat(DEPTH))),
        ("unary-minus", format!("ВЫБРАТЬ {}1", "-".repeat(DEPTH))),
        ("case", format!("ВЫБРАТЬ {}", nested("ВЫБОР КОГДА ", "1", " ТОГДА 1 КОНЕЦ", DEPTH))),
        (
            "source-subquery",
            format!("ВЫБРАТЬ 1 ИЗ {}", nested("(ВЫБРАТЬ 1 ИЗ ", "Т", ") КАК Х", DEPTH)),
        ),
        (
            "in-subquery",
            format!("ВЫБРАТЬ 1 ГДЕ {}", nested("А В (ВЫБРАТЬ 1 ИЗ Т ГДЕ ", "1", ")", DEPTH)),
        ),
        ("joins", format!("ВЫБРАТЬ 1 ИЗ А {}", "ЛЕВОЕ СОЕДИНЕНИЕ Б ".repeat(DEPTH))),
    ];
    for (label, input) in cases {
        assert_reported(label, input, parser::parse_sdbl);
    }
}

#[test]
fn long_bsl_chains_are_reported_not_overflowed() {
    let cases = [
        ("fields", in_method(format!("А = а{};", ".б".repeat(DEPTH)))),
        ("calls", in_method(format!("А = ф{};", "()".repeat(DEPTH)))),
        ("indexes", in_method(format!("А = а{};", "[1]".repeat(DEPTH)))),
        ("sum", in_method(format!("А = 1{};", " + 1".repeat(DEPTH)))),
        ("product", in_method(format!("А = 1{};", " * 1".repeat(DEPTH)))),
        ("comparison", in_method(format!("А = 1{};", " = 1".repeat(DEPTH)))),
        ("and", in_method(format!("А = 1{};", " И 1".repeat(DEPTH)))),
        ("or", in_method(format!("А = 1{};", " ИЛИ 1".repeat(DEPTH)))),
        // Chains under every level of nesting add up in one tree.
        (
            "nested-chains",
            in_method(format!("А = {};", nested("(", "а", &format!("){}", ".б".repeat(100)), 120))),
        ),
    ];
    for (label, input) in cases {
        assert_reported(label, input, parser::parse);
    }
}

#[test]
fn chains_real_code_builds_are_not_reported() {
    let concatenation =
        in_method(format!("Текст = \"\"{};", " + Символы.ПС + \"строка\"".repeat(1_000)));
    let reports =
        on_small_stack("concatenation", move || too_deep_reports(&parser::parse(&concatenation)));
    assert_eq!(reports, 0);
}

#[test]
fn nesting_real_code_reaches_is_not_reported() {
    const SHALLOW: usize = 30;
    let bsl = in_method(nested(
        "Если 1 Тогда ",
        &format!("А = {};", nested("ф(", &nested("(", "-1", ")", SHALLOW), ")", SHALLOW)),
        " КонецЕсли;",
        SHALLOW,
    ));
    let sdbl = format!(
        "ВЫБРАТЬ 1 ИЗ {}",
        nested(
            "(ВЫБРАТЬ 1 ИЗ ",
            &format!("Т ГДЕ {}", nested("(", "1", ")", SHALLOW)),
            ") КАК Х",
            SHALLOW
        )
    );
    assert_eq!(too_deep_reports(&parser::parse(&bsl)), 0);
    assert_eq!(too_deep_reports(&parser::parse_sdbl(&sdbl)), 0);
}

#[test]
fn unclosed_groups_past_the_bound_end_with_their_statement_or_method() {
    use syntax::ast::{AstNode, ProcedureDef};

    let next_method = "\nПроцедура Д()\nВ = ;\nКонецПроцедуры\n";
    let cases = [
        (
            "parens",
            format!(
                "Процедура П()\nА = {};\nБ = 2;\nКонецПроцедуры{next_method}",
                "(".repeat(DEPTH)
            ),
        ),
        (
            "index",
            format!(
                "Процедура П()\nА = {};\nБ = 2;\nКонецПроцедуры{next_method}",
                "а[".repeat(DEPTH)
            ),
        ),
        (
            "if",
            format!(
                "Процедура П()\n{}\nКонецПроцедуры{next_method}",
                "Если 1 Тогда ".repeat(DEPTH)
            ),
        ),
    ];
    for (label, input) in cases {
        let (methods, reports, unexpected) = on_small_stack(label, move || {
            let parse = parser::parse(&input);
            let methods = parse.syntax_node().descendants().filter_map(ProcedureDef::cast).count();
            let unexpected = parse
                .errors()
                .iter()
                .filter(|error| error.message() == "Неожиданный токен ';'")
                .count();
            (methods, too_deep_reports(&parse), unexpected)
        });
        assert!(reports > 0, "{label}: the exceeded depth must be reported");
        assert_eq!(methods, 2, "{label}: the method after the unclosed groups must be parsed");
        assert_eq!(unexpected, 1, "{label}: the error in the next method must be reported");
    }
}

#[test]
fn a_block_past_the_bound_ends_with_its_closer() {
    use syntax::ast::{AstNode, ProcedureDef, ReturnStmt};

    // As many `#Если` as the parser descends into, so the next one is past
    // the bound; their conditions are plain symbols, which add no depth.
    let outer = 128;
    let deep_block = "#Если Клиент Тогда\nА = 1;\n#КонецЕсли\n";
    let module = format!(
        "{}{deep_block}#Если Клиент Тогда\n#КонецЕсли\nПроцедура П()\nКонецПроцедуры\n{}",
        "#Если Сервер Тогда\n".repeat(outer),
        "#КонецЕсли\n".repeat(outer),
    );
    let method = format!(
        "Процедура П()\n{}{deep_block}Возврат;\n{}КонецПроцедуры\n",
        "#Если Сервер Тогда\n".repeat(outer),
        "#КонецЕсли\n".repeat(outer),
    );
    let (methods, module_reports) = on_small_stack("block-in-module", move || {
        let parse = parser::parse(&module);
        let methods = parse.syntax_node().descendants().filter_map(ProcedureDef::cast).count();
        (methods, too_deep_reports(&parse))
    });
    assert_eq!(module_reports, 2, "each of the two blocks past the bound is reported on its own");
    assert_eq!(methods, 1, "the method after the block past the bound must be parsed");

    let (returns, method_reports) = on_small_stack("block-in-method", move || {
        let parse = parser::parse(&method);
        let returns = parse.syntax_node().descendants().filter_map(ReturnStmt::cast).count();
        (returns, too_deep_reports(&parse))
    });
    assert_eq!(method_reports, 1, "only the block past the bound is reported");
    assert_eq!(returns, 1, "the statement after the block past the bound must be parsed");
}
