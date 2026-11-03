use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode, Fix, TextEdit};
use hir::LocalRange;
use ide_db::TextRange;
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxToken, TextSize};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Standard, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const ILLEGAL_DASHES: &[char] =
    &['\u{00AD}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}'];

const ILLEGAL_SPACE: char = '\u{00A0}';

const SOFT_HYPHEN: char = '\u{00AD}';

enum InvalidCharType {
    IllegalDash,
    IllegalSpace,
}

/// What an illegal character becomes, or `None` for a legal one. The soft hyphen is
/// invisible, so it is removed rather than turned into a visible `-`.
fn replacement_for(ch: char) -> Option<&'static str> {
    if ch == ILLEGAL_SPACE {
        Some(" ")
    } else if ch == SOFT_HYPHEN {
        Some("")
    } else if ILLEGAL_DASHES.contains(&ch) {
        Some("-")
    } else {
        None
    }
}

/// Replaces every illegal character of one token. Safe in whitespace, where the
/// program does not see the text; manual in a comment the analyzer parses and in
/// an error token (the replacement changes how the line parses).
fn token_fix(kind: SyntaxKind, text: &str, start: TextSize) -> Option<Fix<LocalRange>> {
    let edits: Vec<TextEdit<LocalRange>> = text
        .char_indices()
        .filter_map(|(offset, ch)| {
            let new_text = replacement_for(ch)?;
            let at = start + TextSize::from(offset as u32);
            Some(TextEdit {
                range: LocalRange::of_detached_node(TextRange::at(at, TextSize::of(ch))),
                new_text: new_text.to_string(),
            })
        })
        .collect();
    if edits.is_empty() {
        return None;
    }
    // Whitespace carries no meaning for the platform, so turning the character into
    // an ordinary space there changes nothing but the compilability of the module.
    Some(if kind == SyntaxKind::WHITESPACE {
        Fix::safe("Заменить недопустимые символы", edits)
    } else {
        Fix::manual("Заменить недопустимые символы (меняет текст программы)", edits)
    })
}

/// The platform compiles a comment whatever stands in it, so only a comment the
/// analyzer itself reads is worth a report: a `//` line of a multiline literal and
/// an `Имя - Тип` line of a `Параметры:` section, whose split the character breaks.
fn comment_replacement_changes_meaning(token: &SyntaxToken, text: &str) -> bool {
    if syntax::comment_token_is_string_text(token) {
        return true;
    }
    let replaced = apply_char_replacements(text);
    parameter_doc_parts(text) != parameter_doc_parts(&replaced) && in_parameters_section(token)
}

fn apply_char_replacements(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match replacement_for(ch) {
            Some(repl) => out.push_str(repl),
            None => out.push(ch),
        }
    }
    out
}

/// `Имя - Тип` as the doc parser splits it, or `None` when the ASCII separator
/// is absent. Illegal spaces and dashes around the hyphen fail this split.
fn parameter_doc_parts(text: &str) -> Option<(String, String)> {
    let body = text.trim().trim_start_matches('/').trim().replace('\t', " ");
    let mut parts = body.splitn(3, " - ");
    let name = parts.next()?.trim();
    let ty = parts.next()?.trim();
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_alphabetic() || first == '_')
        || !chars.all(|ch| ch.is_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((name.to_string(), ty.to_string()))
}

fn in_parameters_section(token: &SyntaxToken) -> bool {
    let mut cursor = token.prev_token();
    let mut steps = 0u32;
    while let Some(current) = cursor {
        steps += 1;
        if steps > 400 {
            return false;
        }
        if current.kind() == SyntaxKind::COMMENT {
            let body = current.text().trim().trim_start_matches('/').trim().fold_lower();
            if body.starts_with("параметры:") || body.starts_with("parameters:") {
                return true;
            }
        } else if !current.kind().is_trivia() {
            return false;
        }
        cursor = current.prev_token();
    }
    false
}

/// Verified on 8.3.27: a non-breaking space breaks compilation only when it is
/// absorbed into the preceding identifier, keyword or number; isolated by an
/// ordinary space, after punctuation or an operator, and at the start or the end of
/// a line the module compiles.
fn whitespace_corrupts_previous_token(token: &SyntaxToken, text: &str) -> bool {
    if !text.starts_with(ILLEGAL_SPACE) {
        return false;
    }
    let Some(previous) = token.prev_token() else {
        return false;
    };
    if previous.kind().is_trivia() {
        return false;
    }
    previous.text().chars().next_back().is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

fn create_diagnostic(
    range: LocalRange,
    char_type: InvalidCharType,
    fixes: Vec<Fix<LocalRange>>,
    code: DiagnosticCode,
    ctx: &BodyContext,
) -> Diagnostic<LocalRange> {
    let message = match char_type {
        InvalidCharType::IllegalDash => "Нужно исправить на правильный символ \"-\"",
        InvalidCharType::IllegalSpace => {
            "Нужно заменить символ неразрывного пробела на обычный пробел"
        }
    };

    Diagnostic {
        code,
        message: message.to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes,
    }
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::InvalidCharacterInFile;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    let mut diagnostics = Vec::new();

    for token in ctx.tokens() {
        let text = token.text();

        let has_illegal_space = text.chars().any(|ch| ch == ILLEGAL_SPACE);
        let has_illegal_dash = text.chars().any(|ch| ILLEGAL_DASHES.contains(&ch));

        if !has_illegal_space && !has_illegal_dash {
            continue;
        }

        // Report only where the compiler reads the character. A string literal is
        // data - the platform compiles it and the character may be exactly what the
        // code displays or searches for; a comment it does not read at all; a
        // whitespace run only corrupts the token it is glued to. In an error token
        // the character has already broken parsing, or it stands in code.
        let worth_reporting = match token.kind() {
            SyntaxKind::STRING
            | SyntaxKind::STRING_PART
            | SyntaxKind::STRING_START
            | SyntaxKind::STRING_TAIL => false,
            SyntaxKind::COMMENT => comment_replacement_changes_meaning(&token, text),
            SyntaxKind::WHITESPACE => whitespace_corrupts_previous_token(&token, text),
            SyntaxKind::ERROR => true,
            _ => false,
        };

        if !worth_reporting {
            continue;
        }

        let char_type = if has_illegal_space {
            InvalidCharType::IllegalSpace
        } else {
            InvalidCharType::IllegalDash
        };

        let fix = token_fix(token.kind(), text, token.text_range().start());
        diagnostics.push(create_diagnostic(
            LocalRange::of_detached_node(token.text_range()),
            char_type,
            fix.into_iter().collect(),
            code,
            ctx,
        ));
    }

    acc.extend(diagnostics);
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;
    #[test]
    fn test_comprehensive() {
        let code = concat!(
            "// минусы с ошибками\n",
            "СреднееТире = \"\u{2013}\";\n",
            "ЦифровоеТире = \"\u{2012}\";\n",
            "ДлинноеТире = \"\u{2014}\";\n",
            "ГоризонтальнаяЛиния = \"\u{2015}\";\n",
            "НеправильныйМинус = \"\u{2212}\";\n",
            "// Мягкий перенос в комментарии \u{00AD}\n",
            "\n",
            "// минус без ошибки\n",
            "ПравильныйДефисМинус = \"-\";\n",
            "\n",
            "// ошибочные неразрывные пробелы\n",
            "// В этом комментарии только\u{00A0}НПП\n",
            "\n",
            "Строка = \"А\" + \"\u{00A0}\" + \"И\";\n",
            "\n",
            "//в строке ниже неразрывный пробел\n",
            "\u{00A0}\n",
            "// минусы с ошибками\n",
            "//СреднееТире = \"\n",
            "\u{2013};\n",
            "//ЦифровоеТире = \"\n",
            "\u{2012};\n",
            "//ДлинноеТире = \"\n",
            "\u{2014};\n",
            "//ГоризонтальнаяЛиния = \"\n",
            "\u{2015};\n",
            "//НеправильныйМинус = \"\n",
            "\u{2212};\n",
            "//конец файла\n",
        );
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#"
                InvalidCharacterInFile @ 21:1..21:2
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 23:1..23:2
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 25:1..25:2
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 27:1..27:2
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 29:1..29:2
                  message: Нужно исправить на правильный символ "-"
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_en_dash_in_string() {
        let code = r#"
А = "тест–тест";
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_nbsp_in_string_literal() {
        let code = "А = \"метр\u{00A0}куб\";\n";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_nbsp_in_comment() {
        let code = "// Тест\u{00A0}неразрывный\u{00A0}пробел";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_dash_in_ordinary_comment() {
        let code = concat!("// тире \u{2014}\n", "Ч3 = 4;\n");
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_nbsp_glued_to_identifier_or_number() {
        let code = concat!(
            "Ч2\u{00A0}= 1;\n",
            "Ч = 1\u{00A0}+ 2;\n",
            "Ч7 = 1\u{00A0};\n",
            "Если\u{00A0}Истина Тогда Ч9 = 1; КонецЕсли;\n",
            "Ч11 = 1\u{00A0}\u{00A0}+ 2;\n",
            "Ч12 = 1\u{00A0}\n",
            "+ 2;\n",
            "Ч13 = 1\u{00A0} + 2;\n",
        );
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#"
                InvalidCharacterInFile @ 1:3..1:4
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 2:6..2:7
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 3:7..3:8
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 4:5..4:6
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 5:8..5:10
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 6:8..6:9
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major
                InvalidCharacterInFile @ 8:8..8:10
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_nbsp_separated_from_code() {
        let code = concat!(
            "Ч1   \u{00A0}   = 1;\n",
            "Ч10 =\u{00A0}1;\n",
            "Ч5 = Макс(1,\u{00A0}2);\n",
            "Ч8 = Макс(\u{00A0}1, 2);\n",
            "\u{00A0}\u{00A0}Ч2 = 3;\n",
            "\u{00A0} Ч3 = 1;\n",
            " \u{00A0}Ч4 = 1;\n",
            "Ч6 = 1;\u{00A0}\n",
        );
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_nbsp_breaks_parameter_doc_line() {
        let code = concat!(
            "// Описание.\n",
            "//\n",
            "// Параметры:\n",
            "//  Имя\u{00A0}- Строка - описание\n",
            "//\n",
            "Процедура Тест(Имя) Экспорт\n",
            "КонецПроцедуры\n",
        );
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#"
                InvalidCharacterInFile @ 4:1..4:28
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_no_false_positives() {
        let code = r#"
Процедура Тест()
    А = "тест-тест";
    Б = "тест тест";
    // Комментарий - нормальный
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_all_illegal_dashes_in_expression() {
        let code = concat!(
            "А = 5 \u{AD} 2;\n",
            "Б = 5 \u{2012} 2;\n",
            "В = 5 \u{2013} 2;\n",
            "Г = 5 \u{2014} 2;\n",
            "Д = 5 \u{2015} 2;\n",
            "Е = 5 \u{2212} 2;\n",
        );
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#"
                InvalidCharacterInFile @ 1:7..1:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 2:7..2:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 3:7..3:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 4:7..4:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 5:7..5:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 6:7..6:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_mixed_invalid_chars() {
        let code = concat!("А = 5 \u{2013} 2;\n", "Б2\u{00A0}= 1;\n");
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::InvalidCharacterInFile,
            expect![[r#"
                InvalidCharacterInFile @ 1:7..1:8
                  message: Нужно исправить на правильный символ "-"
                  severity: Major
                InvalidCharacterInFile @ 2:3..2:4
                  message: Нужно заменить символ неразрывного пробела на обычный пробел
                  severity: Major"#]],
        );
    }
}
