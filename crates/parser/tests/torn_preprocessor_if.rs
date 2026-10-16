//! A conditional group (`#Если … #КонецЕсли`) is text-level in the platform:
//! the lines of one branch are chosen before the module is parsed. The group
//! may therefore tear a construct apart — stand inside an expression, between
//! `Если` and its `Тогда`, or give each branch its own header for one shared
//! body — and every environment still compiles a whole construct.

use syntax::{SyntaxKind, SyntaxNode};

fn assert_clean(source: &str) -> SyntaxNode {
    let parsed = parser::parse(source);
    assert!(parsed.errors().is_empty(), "unexpected errors in\n{source}\n{:?}", parsed.errors());
    let root = parsed.syntax_node();
    assert_eq!(root.text().to_string(), source, "the tree must keep the text");
    assert!(root.descendants().all(|node| node.kind() != SyntaxKind::ERROR));
    root
}

/// The method after the torn one must keep its kind and its name: a recovery
/// that ends the function early turns its `Возврат` into one of a procedure.
fn assert_methods(root: &SyntaxNode, functions: &[&str]) {
    let names: Vec<String> = root
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::FUNCTION_DEF)
        .map(|node| {
            node.children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == SyntaxKind::IDENT)
                .map(|token| token.text().to_string())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(names, functions);
    assert!(root.descendants().all(|node| node.kind() != SyntaxKind::PROCEDURE_DEF));
}

fn in_function(body: &str) -> String {
    format!(
        "Функция Первая(А, Б, В)\n{body}\n\tВозврат 1;\nКонецФункции\n\nФункция Вторая()\n\tВозврат 2;\nКонецФункции\n"
    )
}

fn parts(root: &SyntaxNode) -> Vec<String> {
    root.descendants()
        .filter(|node| node.kind() == SyntaxKind::PRE_TORN_IF_PART)
        .map(|node| node.text().to_string())
        .collect()
}

#[test]
fn a_group_inside_an_expression_is_read_as_one_variant() {
    for body in [
        // Between `Если` and `Тогда`, continuing the condition.
        "\tЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А = 1\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
        // The same with `#Иначе`, each branch its own continuation.
        "\tЕсли Ложь\n\t\t#Если Сервер Тогда\n\t\tИли А\n\t\t#Иначе\n\t\tИли Б\n\t\t#КонецЕсли\n\t\tИли В\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
        // In an `ИначеЕсли` header, after a whole branch.
        "\tЕсли А Тогда\n\t\tБ = 1;\n\tИначеЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли Б\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 2;\n\tКонецЕсли;",
        // A condition that does not hold: the branch is not read at all.
        "\tЕсли Ложь\n\t\tИли А\n\t\t#Если Не Клиент И Не Сервер Тогда\n\t\tИли Б\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
        // A group no branch of which is read, the statement ending after it.
        "\tБ = Ложь\n\t\tИли А\n\t\t#Если Не Клиент Тогда\n\t\tИли В\n\t\t#КонецЕсли\n\t\t;",
        // An assignment whose value goes on after the group.
        "\tБ = Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А\n\t\t#КонецЕсли\n\t\tИли В;",
        // The operand itself chosen by the group.
        "\tБ =\n\t\t#Если Клиент Тогда\n\t\tА\n\t\t#Иначе\n\t\tВ\n\t\t#КонецЕсли\n\t\t+ 1;",
        // An argument chosen by the group.
        "\tСообщить(1,\n\t\t#Если Клиент Тогда\n\t\tА\n\t\t#Иначе\n\t\tВ\n\t\t#КонецЕсли\n\t\t);",
        // A loop header.
        "\tПока А\n\t\t#Если Клиент Тогда\n\t\tИ Б\n\t\t#КонецЕсли\n\tЦикл\n\t\tПрервать;\n\tКонецЦикла;",
        // Groups nested inside the read branch.
        "\tЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А\n\t\t\t#Если Сервер Тогда\n\t\t\tИли Б\n\t\t\t#КонецЕсли\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
    ] {
        let source = in_function(body);
        let root = assert_clean(&source);
        assert_methods(&root, &["Первая", "Вторая"]);
        assert!(!parts(&root).is_empty(), "the group must be read as torn:\n{source}");
        assert!(
            root.descendants().all(|node| node.kind() != SyntaxKind::PRE_IF_DIR),
            "a torn group is not a structured one:\n{source}"
        );
    }
}

#[test]
fn branches_with_their_own_headers_share_one_body() {
    for body in [
        // Each branch opens the `Если` the shared body and `КонецЕсли` close.
        "#Если Клиент Тогда\n\tЕсли А Тогда\n#Иначе\n\tЕсли Истина Тогда\n#КонецЕсли\n\t\tБ = 1;\n\tКонецЕсли;",
        // Two groups with one condition tear one construct between them.
        "\tЕсли А Тогда\n\t#Если Не ВебКлиент Тогда\n\t\tЕсли Б Тогда\n\t\t\tВ = 1;\n\t\tИначе\n\t#КонецЕсли\n\t\t\tВ = 2;\n\t#Если Не ВебКлиент Тогда\n\t\tКонецЕсли;\n\t#КонецЕсли\n\tКонецЕсли;",
        // An `ИначеЕсли` of an outer `Если` inside the group.
        "\tЕсли А Тогда\n\t\tБ = 1;\n\t#Если Клиент Тогда\n\tИначеЕсли Б Тогда\n\t\tБ = 2;\n\t#КонецЕсли\n\tКонецЕсли;",
        // A loop header per branch.
        "#Если Клиент Тогда\n\tДля Каждого Э Из А Цикл\n#Иначе\n\tДля Каждого Э Из Б Цикл\n#КонецЕсли\n\t\tВ = Э;\n\tКонецЦикла;",
    ] {
        let source = in_function(body);
        let root = assert_clean(&source);
        assert_methods(&root, &["Первая", "Вторая"]);
        assert!(!parts(&root).is_empty(), "the group must be read as torn:\n{source}");
    }
}

#[test]
fn branches_with_their_own_annotations_share_one_declaration() {
    let source = "#Если Сервер И Не Сервер Тогда\n&НаСервере\n#Иначе\n&НаКлиентеНаСервереБезКонтекста\n#КонецЕсли\nФункция Первая()\n\tВозврат 1;\nКонецФункции\n\nФункция Вторая()\n\tВозврат 2;\nКонецФункции\n";
    let root = assert_clean(source);
    assert_methods(&root, &["Первая", "Вторая"]);
    assert!(!parts(&root).is_empty());
    let first = root
        .descendants()
        .find(|node| node.kind() == SyntaxKind::FUNCTION_DEF)
        .expect("the first function");
    assert!(
        first.children().any(|node| node.kind() == SyntaxKind::COMPILER_DIRECTIVE),
        "the branch read keeps its annotation on the function"
    );
}

#[test]
fn the_branch_read_is_code_and_the_rest_is_opaque() {
    let source = in_function(
        "\tЕсли Ложь\n\t\t#Если Сервер Тогда\n\t\tИли Прочитанное\n\t\t#Иначе\n\t\tИли Пропущенное\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
    );
    let root = assert_clean(&source);

    let parts = parts(&root);
    assert_eq!(parts, ["#Если Сервер Тогда", "#Иначе\n\t\tИли Пропущенное\n\t\t#КонецЕсли"]);

    let read = root
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.text() == "Прочитанное")
        .expect("the branch read is in the tree");
    assert!(
        read.parent_ancestors().all(|node| node.kind() != SyntaxKind::PRE_TORN_IF_PART),
        "the branch read is code, not an opaque part"
    );
    assert!(read.parent_ancestors().any(|node| node.kind() == SyntaxKind::BINARY_EXPR));
}

#[test]
fn the_semicolon_after_the_group_ends_the_statement_torn_by_it() {
    let source =
        in_function("\tБ = Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А\n\t\t#КонецЕсли\n\t\t;");
    let root = assert_clean(&source);
    assert!(
        root.descendants().all(|node| node.kind() != SyntaxKind::EMPTY_STMT),
        "the `;` belongs to the assignment, not to a statement of its own"
    );
}

#[test]
fn a_group_of_whole_statements_keeps_the_structured_reading() {
    let source = "Процедура П()\n#Если Клиент Тогда\n\tА = 1;\n#ИначеЕсли Сервер Тогда\n\tЕсли Б Тогда\n\t\tА = 2;\n\tКонецЕсли;\n#Иначе\n\tА = 3;\n#КонецЕсли\nКонецПроцедуры\n";
    let root = assert_clean(source);
    assert!(parts(&root).is_empty(), "{:?}", parts(&root));
    let group = root
        .descendants()
        .find(|node| node.kind() == SyntaxKind::PRE_IF_DIR)
        .expect("structured group");
    assert!(group.children().any(|node| node.kind() == SyntaxKind::PRE_ELSIF_CLAUSE));
    assert!(group.children().any(|node| node.kind() == SyntaxKind::PRE_ELSE_CLAUSE));
    // Every branch is still parsed.
    let assignments =
        root.descendants().filter(|node| node.kind() == SyntaxKind::ASSIGN_STMT).count();
    assert_eq!(assignments, 3);
}

#[test]
fn a_closer_word_used_as_a_name_does_not_tear_its_group() {
    // A member name and a label may be spelled as a construct word; neither
    // opens or closes anything, so the group around them stays structured.
    let source = "#Если Клиент Тогда\nПроцедура П()\n\tА = Б.Иначе;\n\tПерейти ~КонецЦикла;\n~КонецЦикла:\n\tА = 1;\nКонецПроцедуры\n#КонецЕсли\n";
    let root = parser::parse(source).syntax_node();
    assert!(parts(&root).is_empty(), "{:?}", parts(&root));
    assert!(root.descendants().any(|node| node.kind() == SyntaxKind::PRE_IF_DIR));
}

#[test]
fn a_broken_construct_still_reports_its_error() {
    for body in [
        // An operator with no operand in the branch read.
        "\tЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли\n\t\t#КонецЕсли\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
        // No `Тогда` at all.
        "\tЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А\n\t\t#КонецЕсли\n\t\tБ = 1;\n\tКонецЕсли;",
        // A group that is never closed.
        "\tЕсли Ложь\n\t\t#Если Клиент Тогда\n\t\tИли А\n\tТогда\n\t\tБ = 1;\n\tКонецЕсли;",
        // A body whose `КонецЕсли` is in neither branch.
        "#Если Клиент Тогда\n\tЕсли А Тогда\n#Иначе\n\tЕсли Истина Тогда\n#КонецЕсли\n\t\tБ = 1;",
    ] {
        let source = in_function(body);
        let parsed = parser::parse(&source);
        assert!(!parsed.errors().is_empty(), "the error must survive:\n{source}");
        assert_eq!(parsed.syntax_node().text().to_string(), source);
    }
}
