//! Pins the coordinate system documented in `docs/CI_REPORTERS.md`: the columns of
//! `DiagnosticOutput` (what `analyze --format jsonl` emits) are zero-based offsets in
//! characters, not bytes, and the range end is exclusive.

use ide_db::TextRange;
use ide_diagnostics::{Diagnostic, DiagnosticCode, Severity};
use text_size::TextSize;

fn finding_over(source: &str, token: &str) -> Diagnostic {
    let start = TextSize::try_from(source.find(token).unwrap()).unwrap();
    let end = start + TextSize::try_from(token.len()).unwrap();
    Diagnostic {
        code: DiagnosticCode::QueryParseError,
        message: String::new(),
        severity: Severity::Warning,
        range: TextRange::new(start, end),
        tags: Vec::new(),
        fixes: Vec::new(),
    }
}

#[test]
fn jsonl_columns_are_zero_based_character_offsets() {
    let source = "Перем А;\r\n\tПеременная = \"Значение\" + Прочее;\r\n";
    let token = "Значение";
    let output = finding_over(source, token).to_output(source);

    // Lines count from zero: the finding is on the second line.
    assert_eq!(output.start_line, 1);
    assert_eq!(output.end_line, 1);

    // Columns are characters: slicing the line by characters yields exactly the token,
    // and the end column is exclusive.
    let line = source.lines().nth(output.start_line).unwrap();
    let sliced: String = line
        .chars()
        .skip(output.start_column)
        .take(output.end_column - output.start_column)
        .collect();
    assert_eq!(sliced, token);
    assert_eq!(output.end_column - output.start_column, token.chars().count());

    // Cyrillic to the left makes bytes and characters differ, so a byte-based reading fails.
    let byte_start = line.find(token).unwrap();
    assert_ne!(
        byte_start, output.start_column,
        "the line must have non-ASCII text left of the token"
    );

    // A finding on the very first line starts at line 0.
    let first = finding_over(source, "Перем").to_output(source);
    assert_eq!((first.start_line, first.start_column), (0, 0));
}

#[test]
fn jsonl_lines_break_on_a_lone_carriage_return() {
    let source = "Перем А;\rПеременная = \"Значение\";\n";
    let token = "Значение";
    let output = finding_over(source, token).to_output(source);

    assert_eq!(output.start_line, 1);
    assert_eq!(source.lines().count(), 1, "a `\\n`-only split must not see the second line");

    let line = line_index::split_lines(source).nth(output.start_line).unwrap();
    let sliced: String = line
        .chars()
        .skip(output.start_column)
        .take(output.end_column - output.start_column)
        .collect();
    assert_eq!(sliced, token);
}

#[test]
fn jsonl_end_column_of_a_multiline_finding_counts_in_the_end_line() {
    let source = "Перем А;\r\nПеременная = 1;\r\n";
    let start = TextSize::from(0);
    let end = TextSize::try_from(source.find("Переменная").unwrap() + "Пер".len()).unwrap();
    let mut finding = finding_over(source, "Перем");
    finding.range = TextRange::new(start, end);
    let output = finding.to_output(source);

    assert_eq!((output.start_line, output.start_column), (0, 0));
    assert_eq!((output.end_line, output.end_column), (1, 3));
}
