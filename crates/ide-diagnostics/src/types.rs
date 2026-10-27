use crate::DiagnosticCode;
use hir::{LocalRange, MethodOffset};
use ide_db::TextRange;

/// A finding with its range in `R`: file positions (`TextRange`, the default)
/// once assembled for the file, or positions relative to the body's own root
/// (`LocalRange`) while it is the result of one body's check — the form a
/// per-method memo keeps, because it must not change when the method moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic<R = TextRange> {
    pub code: DiagnosticCode,
    pub message: String,
    pub severity: Severity,
    pub range: R,
    pub tags: Vec<DiagnosticTag>,
    pub fixes: Vec<Fix<R>>,
}

impl Diagnostic<LocalRange> {
    /// Into file positions: the finding and every edit of its fixes.
    pub fn lift(self, base: MethodOffset) -> Diagnostic {
        Diagnostic {
            code: self.code,
            message: self.message,
            severity: self.severity,
            range: self.range.lift(base),
            tags: self.tags,
            fixes: self.fixes.into_iter().map(|fix| fix.lift(base)).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticOutput {
    pub code: String,

    pub message: String,

    pub severity: String,

    /// Zero-based line of the range start. Reporters that print 1-based numbers (SARIF,
    /// JUnit, Code Quality) add 1 themselves; `--format jsonl` emits this value as is.
    pub start_line: usize,

    /// Zero-based column of the range start, counted in Unicode code points (not bytes,
    /// not UTF-16 units).
    pub start_column: usize,

    /// Zero-based line of the range end.
    pub end_line: usize,

    /// Zero-based column of the range end in Unicode code points; the end is exclusive.
    pub end_column: usize,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Diagnostic {
    pub fn to_output(&self, file_text: &str) -> DiagnosticOutput {
        let line_index = line_index::LineIndex::new(file_text);
        self.to_output_with_index(file_text, &line_index)
    }

    pub fn to_output_with_index(
        &self,
        file_text: &str,
        line_index: &line_index::LineIndex,
    ) -> DiagnosticOutput {
        use line_index::LineIndexExt;

        let file_len = file_text.len();
        let range_start: u32 = self.range.start().into();
        let range_end: u32 = self.range.end().into();

        if range_start as usize > file_len || range_end as usize > file_len {
            panic!(
                "BUG in diagnostic handler '{}': Invalid TextRange [{}, {}) exceeds file length {}\n\
                 Diagnostic message: '{}'\n\
                 This is a bug in the diagnostic handler that created this Diagnostic.\n\
                 The handler must ensure TextRange is within file bounds.",
                self.code.as_str(),
                range_start,
                range_end,
                file_len,
                self.message
            );
        }

        let start = line_index.line_col(self.range.start());
        let end = line_index.line_col(self.range.end());

        let start_char_col = line_index.byte_col_to_char_col(file_text, start.line, start.col);
        let end_char_col = line_index.byte_col_to_char_col(file_text, end.line, end.col);

        DiagnosticOutput {
            code: self.code.as_str().to_string(),
            message: self.message.clone(),
            severity: self.severity.as_str().to_string(),
            start_line: start.line as usize,
            start_column: start_char_col as usize,
            end_line: end.line as usize,
            end_column: end_char_col as usize,
            tags: self.tags.iter().map(|tag| tag.as_str().to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocker,
    Critical,
    Major,
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocker => "Blocker",
            Self::Critical => "Critical",
            Self::Major => "Major",
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Information => "Information",
            Self::Hint => "Hint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticTag {
    Unnecessary,
    Deprecated,
}

impl DiagnosticTag {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unnecessary => "Unnecessary",
            Self::Deprecated => "Deprecated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix<R = TextRange> {
    pub label: String,
    pub edits: Vec<TextEdit<R>>,
    /// Whether this fix may be applied unattended as part of a `source.fixAll`
    /// batch. Safe means the edit is deterministic, semantics-preserving, and
    /// reference-safe when applied together with every other occurrence in the
    /// file. Fixes that offer the user a choice, or that rename an identifier
    /// whose other references the batch cannot reach, stay opt-in quick fixes.
    pub safe_for_fix_all: bool,
}

impl<R> Fix<R> {
    /// A fix eligible for `source.fixAll`: deterministic, semantics-preserving,
    /// and reference-safe as an unattended batch edit.
    pub fn safe(label: impl Into<String>, edits: Vec<TextEdit<R>>) -> Self {
        Self { label: label.into(), edits, safe_for_fix_all: true }
    }

    /// A fix that stays an explicit, opt-in quick fix and is excluded from
    /// `source.fixAll` — it offers a choice or renames a symbol whose remaining
    /// references a file-local batch cannot keep in sync.
    pub fn manual(label: impl Into<String>, edits: Vec<TextEdit<R>>) -> Self {
        Self { label: label.into(), edits, safe_for_fix_all: false }
    }
}

impl Fix<LocalRange> {
    pub fn lift(self, base: MethodOffset) -> Fix {
        Fix {
            label: self.label,
            edits: self.edits.into_iter().map(|edit| edit.lift(base)).collect(),
            safe_for_fix_all: self.safe_for_fix_all,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit<R = TextRange> {
    pub range: R,
    pub new_text: String,
}

impl TextEdit<LocalRange> {
    pub fn lift(self, base: MethodOffset) -> TextEdit {
        TextEdit { range: self.range.lift(base), new_text: self.new_text }
    }
}
