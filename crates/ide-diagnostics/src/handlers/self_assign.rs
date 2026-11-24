use crate::define_metadata;
use crate::metadata::*;
use crate::BodyContext;
use crate::{Diagnostic, DiagnosticCode, Fix, TextEdit};
use hir::LocalRange;
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(range: LocalRange, ctx: &BodyContext) -> Option<Diagnostic<LocalRange>> {
    let mut diagnostic = crate::simple_hir_diagnostic(
        DiagnosticCode::SelfAssign,
        "Присваивание переменной самой себе",
        range,
        ctx,
    )?;

    // A self-assignment is a no-op, so the fix removes its whole line — but only when the
    // line holds nothing else (no trailing comment or statement to preserve). Deletion is
    // opt-in, never an unattended `source.fixAll` edit.
    let text = ctx.root().text().to_string();
    if let Some(line) = self_assign_line_to_delete(&text, range.in_root()) {
        diagnostic.fixes = vec![Fix::manual(
            "Удалить самоприсваивание",
            vec![TextEdit { range: LocalRange::of_detached_node(line), new_text: String::new() }],
        )];
    }

    Some(diagnostic)
}

/// The full line to delete for a self-assignment: from its indentation through the
/// terminating newline. Returns `None` if anything other than the statement and its
/// optional `;` shares the line, so no comment or sibling statement is lost.
fn self_assign_line_to_delete(text: &str, range: TextRange) -> Option<TextRange> {
    let stmt_start: usize = range.start().into();
    let stmt_end: usize = range.end().into();
    let line_start = text[..stmt_start].rfind('\n').map_or(0, |nl| nl + 1);

    // A statement may precede the self-assign on the same line; deleting the whole line
    // would drop it, so only proceed when the prefix is indentation.
    if !text[line_start..stmt_start].trim().is_empty() {
        return None;
    }

    // Skip whitespace, then an optional `;`, to find where the statement really ends.
    let rest = &text[stmt_end..];
    let mut cursor = stmt_end + rest.len() - rest.trim_start().len();
    if text[cursor..].starts_with(';') {
        cursor += 1;
    }

    // The remainder of the line must be blank; otherwise deleting it would drop content.
    let line_end = text[cursor..].find('\n').map_or(text.len(), |nl| cursor + nl);
    if !text[cursor..line_end].trim().is_empty() {
        return None;
    }
    let delete_end = if line_end < text.len() { line_end + 1 } else { line_end };

    Some(TextRange::new((line_start as u32).into(), (delete_end as u32).into()))
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_snapshot_for, check_fix_snapshot_for};
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_unary_minus_changes_the_value() {
        let code = include_str!("fixtures/self_assign_unary.bsl");
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
                SelfAssign @ 4:5..4:46
                  message: Присваивание переменной самой себе
                  severity: Major"#]],
        );
    }

    #[test]
    fn test_fix_deletes_line_only_when_alone() {
        // Only the assignment that owns its line is deleted; the trailing comment and the
        // leading statement on the other two lines would be lost, so they get no fix.
        let code = "Процедура ПересчитатьОстаток(Остаток, Резерв)\n\tРезерв = Резерв; // оставить\n\tОстаток = Остаток;\n\tИтог = 0; Резерв = Резерв;\nКонецПроцедуры\n";
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
            SelfAssign @ 2:2..2:17
              message: Присваивание переменной самой себе
              severity: Major
            SelfAssign @ 3:2..3:19
              message: Присваивание переменной самой себе
              severity: Major
            SelfAssign @ 4:12..4:27
              message: Присваивание переменной самой себе
              severity: Major"#]],
        );
        check_fix_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
            SelfAssign @ 3:2..3:19 — Удалить самоприсваивание [fix_all=false]
            Процедура ПересчитатьОстаток(Остаток, Резерв)
            	Резерв = Резерв; // оставить
            	Итог = 0; Резерв = Резерв;
            КонецПроцедуры
        "#]],
        );
    }

    #[test]
    fn test_distinct_paths_are_silent() {
        let code = r#"Процедура ОбновитьКарточку(Карточка, ВесБрутто)
	Вес = ВесБрутто;
	Карточка.Артикул = Карточка.Код;
	Если Вес = ВесБрутто Тогда
		Вес = 0;
	КонецЕсли;
	Упаковка = Новый Упаковка;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::SelfAssign, expect![[r#""#]]);
    }

    #[test]
    fn test_self_assign() {
        let code = r#"Процедура ОбновитьКарточку(Карточка, ВесБрутто)
	Вес = Вес;
	Карточка.Артикул = Карточка.Код;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
            SelfAssign @ 2:2..2:11
              message: Присваивание переменной самой себе
              severity: Major"#]],
        );
    }

    #[test]
    fn test_self_assign_case_insensitive() {
        let code = r#"Процедура ОбновитьКарточку(Карточка, ВесБрутто)
	ВЕС = вес;
	Карточка.Артикул = Карточка.Код;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
            SelfAssign @ 2:2..2:11
              message: Присваивание переменной самой себе
              severity: Major"#]],
        );
    }

    #[test]
    fn test_property_self_assign_case_insensitive() {
        let code = r#"Процедура ОбновитьКарточку(Карточка, ВесБрутто)
	Вес = ВесБрутто;
	Карточка.Артикул = КАРТОЧКА.артикул; // путь тот же
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::SelfAssign,
            expect![[r#"
            SelfAssign @ 3:2..3:37
              message: Присваивание переменной самой себе
              severity: Major"#]],
        );
    }
}
