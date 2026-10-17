use parser::parse_sdbl;
use syntax::{SyntaxKind, SyntaxNode};

fn nodes(root: &SyntaxNode, kind: SyntaxKind) -> Vec<SyntaxNode> {
    root.descendants().filter(|node| node.kind() == kind).collect()
}

fn checked(input: &str) -> (SyntaxNode, usize) {
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    assert_eq!(root.text(), input);
    let shared = parser::parse_sdbl_with_shared_cache(input);
    assert_eq!(shared.syntax_node().text(), input);
    assert_eq!(shared.errors(), parse.errors());
    (root, parse.errors().len())
}

#[test]
fn single_parenthesized_defects_report_one_primary_error() {
    for (input, kind) in [
        ("ВЫБРАТЬ (А + ИЗ) ИЗ Т", SyntaxKind::SDBL_PAREN_EXPR),
        ("ВЫБРАТЬ ВЫРАЗИТЬ(А КАК ИЗ) ИЗ Т", SyntaxKind::SDBL_FUNCTION_CALL),
        ("ВЫБРАТЬ СУММА(ИЗ) ИЗ Т", SyntaxKind::SDBL_FUNCTION_CALL),
        ("ВЫБРАТЬ А ИЗ Т ГДЕ Б В (ИЗ)", SyntaxKind::SDBL_IN_EXPR),
    ] {
        let (root, errors) = checked(input);
        assert_eq!(errors, 1, "{input}: {:?}\n{root:#?}", parse_sdbl(input).errors());
        assert_eq!(nodes(&root, kind).len(), 1, "{input}");
        let sources = nodes(&root, SyntaxKind::SDBL_DATA_SOURCE);
        assert_eq!(sources.len(), 1, "{input}");
        assert_eq!(sources[0].text().to_string().trim(), "Т", "{input}");
        if input.contains("ГДЕ") {
            assert_eq!(nodes(&root, SyntaxKind::SDBL_WHERE_CLAUSE).len(), 1);
        }
    }
}

#[test]
fn independent_parenthesized_defects_are_not_deduplicated() {
    let input = "ВЫБРАТЬ (А + ИЗ), (Б + ИЗ) ИЗ Т";
    let (root, errors) = checked(input);
    assert_eq!(errors, 2, "{root:#?}");
    assert_eq!(nodes(&root, SyntaxKind::SDBL_SELECTED_FIELD).len(), 2);
    assert_eq!(nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).len(), 1);
    let parse = parse_sdbl(input);
    assert_ne!(parse.errors()[0].range(), parse.errors()[1].range());
}

#[test]
fn dangling_operators_preserve_each_list_owner() {
    for expression in [
        "A +, B *, C AS Last",
        "F(A +, B), C AS Last",
        "(A +, B), C AS Last",
        "(A, FROM), C AS Last",
        "A IN (1 +, 2), C AS Last",
        "T.(A +, B), C AS Last",
    ] {
        let input = format!("SELECT {expression} FROM T WHERE C > 0");
        let (root, errors) = checked(&input);
        assert!(errors > 0, "{input}");
        let fields = nodes(&root, SyntaxKind::SDBL_FIELD_LIST).remove(0);
        assert_eq!(fields.children().count(), if expression.starts_with("A +,") { 3 } else { 2 });
        let fields = nodes(&root, SyntaxKind::SDBL_SELECTED_FIELD);
        assert!(fields.last().unwrap().text().to_string().contains("C AS Last"), "{root:#?}");
        assert_eq!(nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).len(), 1, "{root:#?}");
        assert_eq!(nodes(&root, SyntaxKind::SDBL_WHERE_CLAUSE).len(), 1);
    }
}

#[test]
fn totals_expressions_have_alias_nodes() {
    for (input, aliases) in [
        ("ВЫБРАТЬ А ИЗ Т ИТОГИ КОЛИЧЕСТВО(А) КАК К ПО Б", vec!["КАК К"]),
        ("SELECT A FROM T TOTALS COUNT(A) AS K BY B", vec!["AS K"]),
        (
            "SELECT A FROM T TOTALS SUM(A) + 1 Result, COUNT(A) AS Order BY A",
            vec!["Result", "AS Order"],
        ),
    ] {
        let (root, errors) = checked(input);
        assert_eq!(errors, 0, "{root:#?}");
        let totals = nodes(&root, SyntaxKind::SDBL_TOTALS_BY).remove(0);
        let actual: Vec<_> = nodes(&totals, SyntaxKind::SDBL_ALIAS)
            .iter()
            .map(|node| node.text().to_string().trim().to_string())
            .collect();
        assert_eq!(actual, aliases, "{totals:#?}");
        assert!(nodes(&totals, SyntaxKind::SDBL_COLUMN_REF).iter().all(|node| {
            !["КАК", "К", "AS", "K", "Result", "Order"].contains(&node.text().to_string().trim())
        }));
    }
}

#[test]
fn cast_type_and_membership_branches_keep_their_owned_delimiters() {
    for input in [
        "SELECT CAST(A AS STRING(20)) AS Text FROM T",
        "SELECT CAST(A AS NUMBER(10, 2)) AS Value FROM T",
        "SELECT A FROM T WHERE A IN (SELECT B FROM Q)",
    ] {
        let (root, errors) = checked(input);
        assert_eq!(errors, 0, "{input}\n{root:#?}");
    }
    for input in [
        "SELECT CAST(A AS STRING(20, 2)), C AS Last FROM T",
        "SELECT A FROM T WHERE A IN (SELECT B + FROM Q)",
    ] {
        let (root, errors) = checked(input);
        assert!(errors > 0, "{input}\n{root:#?}");
        assert!(!nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).is_empty(), "{input}\n{root:#?}");
    }
}

#[test]
fn recovery_reports_stray_text_after_a_valid_star_operand() {
    assert_eq!(checked("SELECT COUNT(*) FROM T").1, 0);
    for input in [
        "SELECT COUNT(* Stray) FROM T",
        "SELECT F(* Stray), C AS Last FROM T",
        "SELECT F(A +, * Stray), C AS Last FROM T",
    ] {
        let (root, errors) = checked(input);
        assert_eq!(errors, if input.contains("A +,") { 2 } else { 1 }, "{input}\n{root:#?}");
        assert_eq!(nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).len(), 1, "{input}\n{root:#?}");
        if input.contains("Last") {
            assert!(nodes(&root, SyntaxKind::SDBL_ALIAS).iter().any(|alias| alias
                .text()
                .to_string()
                .trim()
                == "AS Last"));
        }
    }
    let input = "SELECT (A * FROM) FROM T";
    let (root, errors) = checked(input);
    assert_eq!(errors, 1, "{root:#?}");
    assert_eq!(nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).len(), 1);
}

#[test]
fn alias_separator_cannot_supply_a_required_operand() {
    for input in ["ВЫБРАТЬ А + КАК Б ИЗ Т", "SELECT A + AS B FROM T"] {
        let (root, errors) = checked(input);
        assert_eq!(errors, 1, "{root:#?}");
        assert_eq!(nodes(&root, SyntaxKind::SDBL_ALIAS).len(), 1);
        assert_eq!(nodes(&root, SyntaxKind::SDBL_FROM_CLAUSE).len(), 1);
    }
    for tail in ["AS , SUM(A) BY A", "AS BY A", "AS", "AS ; SELECT 2"] {
        let input = format!("SELECT A FROM T TOTALS SUM(A) {tail}");
        let (root, errors) = checked(&input);
        assert!(errors > 0, "{input}\n{root:#?}");
        if tail.contains("BY A") {
            assert!(nodes(&root, SyntaxKind::SDBL_TOTALS_BY)[0]
                .text()
                .to_string()
                .contains("BY A"));
        }
        if tail.contains(';') {
            assert_eq!(nodes(&root, SyntaxKind::SDBL_SELECT_QUERY).len(), 2);
        }
    }
}

#[test]
fn frozen_clean_inputs_have_nonzero_nodes_and_metrics() {
    let inputs = include_str!("fixtures/sdbl_expression_boundaries.sdbl")
        .lines()
        .chain(std::iter::once(include_str!("fixtures/sdbl_independent_package.sdbl")));
    for (index, input) in inputs.enumerate() {
        let (root, errors) = checked(input);
        let count = root.descendants().count();
        let aliases = nodes(&root, SyntaxKind::SDBL_ALIAS).len();
        println!(
            "CLEAN {index} bytes={} errors={errors} nodes={count} aliases={aliases}",
            input.len()
        );
        assert!(count > 0);
        assert_eq!(errors, 0, "{input}: {:?}", parse_sdbl(input).errors());
    }
}

#[test]
fn malformed_packages_make_bounded_progress() {
    let forms = [
        "SELECT (A + FROM) FROM T",
        "SELECT (A + FROM T",
        "SELECT A IN 1) FROM T",
        "SELECT A IN HIERARCHY 1) FROM T",
        "SELECT A IN (FROM) FROM T",
        "SELECT A IN HIERARCHY (FROM) FROM T",
        "SELECT F(F(A + FROM), B) FROM T",
        "SELECT F(A + {Extension(() X)}, B) FROM T",
        "SELECT CAST(A AS NUMBER(10, )) FROM T",
        "SELECT T.(A +, B) FROM T",
        "SELECT A FROM (SELECT B + FROM T) AS Q",
        "SELECT A FROM Register.R.Turnovers(A +, , &End) AS R",
        "SELECT A FROM T TOTALS SUM(A) BY A PERIODS(MONTH, )",
        "SELECT F(A +, B IN (SELECT C + FROM Q)) FROM T",
        "SELECT F(A +,, B) FROM T",
        "SELECT (A +",
    ];
    for copies in [1, 10, 100] {
        let mut timings = Vec::new();
        let mut counts = (0, 0, 0);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            counts = (0, 0, 0);
            for form in forms {
                let input = format!("{}; SELECT 2 AS Tail", vec![form; copies].join("; "));
                let (root, errors) = checked(&input);
                assert!(
                    nodes(&root, SyntaxKind::SDBL_SELECTED_FIELD).iter().any(|node| node
                        .text()
                        .to_string()
                        .trim()
                        == "2 AS Tail"),
                    "{input}\n{root:#?}"
                );
                counts.0 += errors;
                counts.1 += root.descendants().count();
                counts.2 += nodes(&root, SyntaxKind::SDBL_ALIAS).len();
            }
            timings.push(start.elapsed().as_micros());
        }
        timings.sort_unstable();
        println!(
            "MALFORMED copies={copies} forms={} errors={} nodes={} aliases={} median_us={}",
            forms.len(),
            counts.0,
            counts.1,
            counts.2,
            timings[2]
        );
    }
}
