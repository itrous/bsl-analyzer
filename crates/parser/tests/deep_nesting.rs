//! Nesting deeper than the parser descends is a syntax error, not a crash.
//!
//! Every input here is far deeper than any stack holds when each level is a
//! recursive call, and each is parsed on a thread with a small stack: the
//! parse has to come back with the whole text in the tree and the depth
//! reported where it was exceeded.

const DEPTH: usize = 100_000;
const TOO_DEEP: &str = "Слишком глубокая вложенность";

fn on_small_stack<T: Send + 'static>(label: &str, parse: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name(format!("deep-nesting-{label}"))
        .stack_size(256 * 1024)
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
