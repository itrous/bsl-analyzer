use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use lexer::sdbl::{tokenize_sdbl, SdblToken, SdblTokenKind};
use syntax::sdbl_query::LiteralTextMap;
use syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

/// Takes the configuration rather than a [`DiagnosticsContext`]: every SDBL rule needs the
/// project's severities, tags and parameters, and none of them needs the file. That is what
/// lets the same 18 rules serve a query embedded in a module and a bare query text.
pub type SdblDispatchFn = fn(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
);

/// Whether a query text is attested as 1C's own query language, and not a foreign
/// dialect that merely opens with `SELECT` — WQL (`SELECT Caption FROM Win32_Process`),
/// T-SQL, an ADO command. Both attestations come from vocabulary only SDBL owns: a
/// keyword spelled in Cyrillic, which no foreign dialect has, and the prefix of a
/// metadata table (`Catalog.`), which only an SDBL data source names. The prefix is read
/// from the lexer's own table roots, so a metadata kind without query tables (`Report.`,
/// `Role.`) is a foreign qualifier, and a root word away from that position does not
/// attest either: `Task` or `Document` is an ordinary column name in T-SQL and WQL. A
/// foreign schema spelled like a root (`FROM Task.Users`) still attests — it reads
/// exactly as `FROM Catalog.Library` does.
///
/// Only code attests; string literals, comments and quoted identifiers hold data. The
/// SDBL lexer marks its own `"…"` and `//…`, but leaves the foreign forms — `'…'`,
/// `--…`, `/*…*/`, `[…]`, `` `…` `` — as plain tokens, so the scan skips them as `ForeignRegion`s.
///
/// A text that attests to neither is not thereby ruled out, only unproven, which is why
/// the answer costs a quick fix its place in a `fixAll` batch rather than costing the
/// finding itself.
pub fn sdbl_dialect_attested(query_text: &str) -> bool {
    let tokens = tokenize_sdbl(query_text);
    let mut region: Option<ForeignRegion> = None;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        let next = tokens.get(index + 1);
        index += 1;
        if let Some(open) = region {
            if open.closes_at(token, next) {
                region = None;
                index += open.closer_len() - 1;
            }
            continue;
        }
        if let Some(opened) = ForeignRegion::opens_at(token, next) {
            region = Some(opened);
            index += opened.opener_len() - 1;
            continue;
        }
        if is_trivia(token.kind) || token.kind == SdblTokenKind::String {
            continue;
        }
        let is_prefix = tokens[index..]
            .iter()
            .find(|following| !is_trivia(following.kind))
            .is_some_and(|following| following.kind == SdblTokenKind::Dot);
        if is_prefix && is_query_table_root(token.kind) {
            return true;
        }
        if token.kind != SdblTokenKind::Ident
            && token.text.chars().any(|ch| matches!(ch, 'А'..='я' | 'Ё' | 'ё'))
        {
            return true;
        }
    }
    false
}

/// A stretch of foreign syntax whose words are data: SDBL has no such form, so its lexer
/// emits the delimiters as operators or `Error` tokens and the content as plain tokens.
#[derive(Clone, Copy)]
enum ForeignRegion {
    Quoted(&'static str),
    LineComment,
    BlockComment,
}

impl ForeignRegion {
    fn opens_at(token: &SdblToken, next: Option<&SdblToken>) -> Option<Self> {
        let next_kind = next.map(|next| next.kind);
        match (token.kind, token.text.as_str(), next_kind) {
            (SdblTokenKind::Error, "'", _) => Some(Self::Quoted("'")),
            (SdblTokenKind::Error, "[", _) => Some(Self::Quoted("]")),
            (SdblTokenKind::Error, "`", _) => Some(Self::Quoted("`")),
            (SdblTokenKind::Minus, _, Some(SdblTokenKind::Minus)) => Some(Self::LineComment),
            (SdblTokenKind::Slash, _, Some(SdblTokenKind::Star)) => Some(Self::BlockComment),
            _ => None,
        }
    }

    fn closes_at(self, token: &SdblToken, next: Option<&SdblToken>) -> bool {
        match self {
            Self::Quoted(closer) => token.kind == SdblTokenKind::Error && token.text == closer,
            Self::LineComment => token.kind == SdblTokenKind::Newline,
            Self::BlockComment => {
                token.kind == SdblTokenKind::Star
                    && next.is_some_and(|next| next.kind == SdblTokenKind::Slash)
            }
        }
    }

    fn opener_len(self) -> usize {
        match self {
            Self::Quoted(_) => 1,
            Self::LineComment | Self::BlockComment => 2,
        }
    }

    fn closer_len(self) -> usize {
        match self {
            Self::Quoted(_) | Self::LineComment => 1,
            Self::BlockComment => 2,
        }
    }
}

fn is_trivia(kind: SdblTokenKind) -> bool {
    matches!(kind, SdblTokenKind::Whitespace | SdblTokenKind::Newline | SdblTokenKind::Comment)
}

fn is_query_table_root(kind: SdblTokenKind) -> bool {
    use SdblTokenKind as S;
    matches!(
        kind,
        S::MdoCatalog
            | S::MdoDocument
            | S::MdoEnum
            | S::MdoChartOfCharacteristicTypes
            | S::MdoChartOfAccounts
            | S::MdoChartOfCalculationTypes
            | S::MdoInformationRegister
            | S::MdoAccumulationRegister
            | S::MdoAccountingRegister
            | S::MdoCalculationRegister
            | S::MdoBusinessProcess
            | S::MdoTask
            | S::MdoExternalDataSource
            | S::MdoConstant
            | S::MdoConstants
            | S::MdoSequence
            | S::MdoDocumentJournal
            | S::MdoExchangePlan
            | S::MdoFilterCriterion
    )
}

pub fn dispatch_simple(
    config: &DiagnosticsConfig,
    code: DiagnosticCode,
    message: &str,
    range: TextRange,
    mapper: &SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    diagnostics.push(Diagnostic {
        code,
        message: message.to_string(),
        severity: config.severity(code),
        range: mapper.map_range(range, query_text),
        tags: config.tags(code),
        fixes: vec![],
    });
}

pub fn collect_sdbl_via_dispatch(
    ctx: &DiagnosticsContext,
    code: DiagnosticCode,
    dispatch_fn: SdblDispatchFn,
) -> Vec<Diagnostic> {
    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let sdbl_hirs = ctx.sdbl_hir_in_file();
    let bsl_source = ctx.file_text();
    let sdbl_queries = ctx.all_sdbl_in_file();
    let line_starts = build_line_index_shared(&bsl_source);

    let mut diagnostics = Vec::new();

    for ((_expr_id, sdbl_package), (_query_expr_id, query_info)) in
        sdbl_hirs.iter().zip(sdbl_queries.iter())
    {
        let mapper = SdblPositionMapper::from_query_info(query_info, &bsl_source, &line_starts);

        for hir_diag in sdbl_package.all_diagnostics() {
            dispatch_fn(ctx.config, hir_diag, &mapper, &query_info.query_text, &mut diagnostics);
        }
    }

    diagnostics
}

/// Where a query's own coordinates have to land.
///
/// Two modes, because there are two kinds of query and only one of them sits inside
/// something else. `Standalone` is not `Embedded` with zeroed offsets: the embedded
/// projection adds one for the opening quote and re-anchors every continuation line on its
/// `|`, so feeding it the query text as its own source would still shift every range.
#[derive(Debug, Clone)]
pub enum SdblPositionMapper<'a> {
    /// The query is a BSL string literal in a module; ranges project into the module's text.
    Embedded(EmbeddedMapper<'a>),
    /// The query IS the document; its ranges are already the answer.
    Standalone,
}

#[derive(Debug, Clone)]
pub struct EmbeddedMapper<'a> {
    bsl_source: &'a str,

    bsl_literal_line: u32,
    bsl_literal_col: u32,

    line_starts: Vec<usize>,

    quote_corrections: Vec<(usize, usize)>,

    /// Where the literal starts and how its query lies in it, for a literal
    /// torn by extension markers. Line arithmetic cannot place such a query:
    /// its lines are not the literal's lines of the same number.
    torn_literal: Option<(TextSize, LiteralTextMap)>,
}

impl<'a> SdblPositionMapper<'a> {
    pub fn new_from_range(
        bsl_literal_range: TextRange,
        bsl_source: &'a str,
        quote_corrections: Vec<(usize, usize)>,
    ) -> Self {
        let (bsl_literal_line, bsl_literal_col) =
            byte_offset_to_line_col(bsl_source, u32::from(bsl_literal_range.start()));

        let line_starts = build_line_index(bsl_source);

        Self::Embedded(EmbeddedMapper {
            bsl_source,
            bsl_literal_line,
            bsl_literal_col,
            line_starts,
            quote_corrections,
            torn_literal: None,
        })
    }

    pub fn new_from_range_with_line_index(
        bsl_literal_range: TextRange,
        bsl_source: &'a str,
        line_starts: &'a [usize],
        quote_corrections: Vec<(usize, usize)>,
    ) -> Self {
        let (bsl_literal_line, bsl_literal_col) = byte_offset_to_line_col_fast(
            bsl_source,
            line_starts,
            u32::from(bsl_literal_range.start()),
        );

        let line_starts = line_starts.to_vec();

        Self::Embedded(EmbeddedMapper {
            bsl_source,
            bsl_literal_line,
            bsl_literal_col,
            line_starts,
            quote_corrections,
            torn_literal: None,
        })
    }

    pub fn from_query_info(
        query_info: &syntax::SdblQueryInfo,
        bsl_source: &'a str,
        line_starts: &'a [usize],
    ) -> Self {
        Self::new_from_range_with_line_index(
            query_info.bsl_literal_range,
            bsl_source,
            line_starts,
            query_info.quote_corrections.clone(),
        )
        .with_literal_map(query_info)
    }

    /// The same as [`Self::from_query_info`] for a caller without a line index
    /// of the file.
    pub fn from_query_info_without_line_index(
        query_info: &syntax::SdblQueryInfo,
        bsl_source: &'a str,
    ) -> Self {
        Self::new_from_range(
            query_info.bsl_literal_range,
            bsl_source,
            query_info.quote_corrections.clone(),
        )
        .with_literal_map(query_info)
    }

    fn with_literal_map(mut self, query_info: &syntax::SdblQueryInfo) -> Self {
        if let (Self::Embedded(this), Some(map)) = (&mut self, &query_info.literal_map) {
            this.torn_literal = Some((query_info.bsl_literal_range.start(), map.clone()));
        }
        self
    }

    pub fn map_range(&self, sdbl_range: TextRange, sdbl_text: &str) -> TextRange {
        let Self::Embedded(this) = self else { return sdbl_range };

        if let Some((literal_start, map)) = &this.torn_literal {
            return map.map_range_to_literal(sdbl_range) + *literal_start;
        }

        let sdbl_line_starts = build_line_index(sdbl_text);

        let (sdbl_start_line, sdbl_start_col) = byte_offset_to_line_col_fast(
            sdbl_text,
            &sdbl_line_starts,
            u32::from(sdbl_range.start()),
        );
        let (sdbl_end_line, sdbl_end_col) =
            byte_offset_to_line_col_fast(sdbl_text, &sdbl_line_starts, u32::from(sdbl_range.end()));

        let sdbl_start = u32::from(sdbl_range.start()) as usize;
        let sdbl_end = u32::from(sdbl_range.end()) as usize;

        let start_correction: usize = this
            .quote_corrections
            .iter()
            .filter(|(pos, _)| {
                let (line, _col) =
                    byte_offset_to_line_col_fast(sdbl_text, &sdbl_line_starts, *pos as u32);
                line == sdbl_start_line && *pos < sdbl_start
            })
            .map(|(_, chars)| chars)
            .sum();

        let end_correction: usize = this
            .quote_corrections
            .iter()
            .filter(|(pos, _)| {
                let (line, _col) =
                    byte_offset_to_line_col_fast(sdbl_text, &sdbl_line_starts, *pos as u32);
                line == sdbl_end_line && *pos < sdbl_end
            })
            .map(|(_, chars)| chars)
            .sum();

        let bsl_literal_line = this.bsl_literal_line;
        let bsl_literal_col = this.bsl_literal_col;

        let bsl_start_line = bsl_literal_line + sdbl_start_line;
        let bsl_start_col = if sdbl_start_line == 0 {
            bsl_literal_col + sdbl_start_col + 1 + (start_correction as u32)
        } else {
            let bsl_line_text =
                get_line_text(this.bsl_source, &this.line_starts, bsl_start_line as usize);
            if let Some(pipe_pos) = bsl_line_text.find('|') {
                (pipe_pos as u32) + 1 + sdbl_start_col + (start_correction as u32)
            } else {
                sdbl_start_col + (start_correction as u32)
            }
        };

        let bsl_end_line = bsl_literal_line + sdbl_end_line;
        let bsl_end_col = if sdbl_end_line == 0 {
            bsl_literal_col + sdbl_end_col + 1 + (end_correction as u32)
        } else {
            let bsl_line_text =
                get_line_text(this.bsl_source, &this.line_starts, bsl_end_line as usize);
            if let Some(pipe_pos) = bsl_line_text.find('|') {
                (pipe_pos as u32) + 1 + sdbl_end_col + (end_correction as u32)
            } else {
                sdbl_end_col + (end_correction as u32)
            }
        };

        let bsl_start_offset = line_col_to_byte_offset_fast(
            this.bsl_source,
            &this.line_starts,
            bsl_start_line,
            bsl_start_col,
        );
        let bsl_end_offset = line_col_to_byte_offset_fast(
            this.bsl_source,
            &this.line_starts,
            bsl_end_line,
            bsl_end_col,
        );

        TextRange::new(bsl_start_offset.into(), bsl_end_offset.into())
    }
}

pub fn byte_offset_to_line_col(text: &str, offset: u32) -> (u32, u32) {
    let mut line = 0;
    let mut col = 0;

    for (idx, ch) in text.char_indices() {
        if idx as u32 >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }

    (line, col)
}

fn byte_offset_to_line_col_fast(text: &str, line_starts: &[usize], offset: u32) -> (u32, u32) {
    let offset = offset as usize;

    let offset = offset.min(text.len());

    let line = match line_starts.binary_search(&offset) {
        Ok(exact) => exact,
        Err(insert_pos) => insert_pos.saturating_sub(1),
    };

    let line_start = line_starts[line];
    let col = if offset > line_start {
        let mut char_count = 0;
        for (byte_idx, _) in text[line_start..].char_indices() {
            if line_start + byte_idx >= offset {
                break;
            }
            char_count += 1;
        }
        char_count
    } else {
        0
    };

    (line as u32, col as u32)
}

pub fn build_line_index_shared(text: &str) -> Vec<usize> {
    build_line_index(text)
}

fn build_line_index(text: &str) -> Vec<usize> {
    let mut line_starts = vec![0];

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            line_starts.push(idx + 1);
        }
    }

    line_starts
}

fn get_line_text<'a>(text: &'a str, line_starts: &[usize], line: usize) -> &'a str {
    if line >= line_starts.len() {
        return "";
    }

    let start = line_starts[line];
    let end = line_starts.get(line + 1).copied().unwrap_or(text.len());

    let line_text = &text[start..end];
    line_text.strip_suffix('\n').unwrap_or(line_text)
}

fn line_col_to_byte_offset_fast(
    text: &str,
    line_starts: &[usize],
    target_line: u32,
    target_col: u32,
) -> u32 {
    let line = target_line as usize;
    if line >= line_starts.len() {
        return line_starts.last().copied().unwrap_or(0) as u32;
    }

    let line_start = line_starts[line];

    if target_col == 0 {
        return line_start as u32;
    }

    let next_line_start = line_starts.get(line + 1).copied().unwrap_or(text.len());
    let line_text = &text[line_start..next_line_start];

    for (char_count, (byte_idx, _ch)) in line_text.char_indices().enumerate() {
        if char_count as u32 == target_col {
            return (line_start + byte_idx) as u32;
        }
    }

    next_line_start as u32
}

pub fn extract_string_content(node: &SyntaxNode) -> Option<String> {
    if let Some((text, _)) = syntax::sdbl_query::extract_torn_literal(node) {
        return Some(text);
    }
    let mut result = String::new();
    let mut tokens = node.children_with_tokens().filter_map(|it| it.into_token());

    let first_token = tokens.next()?;

    match first_token.kind() {
        SyntaxKind::STRING => {
            let text = first_token.text();
            if text.len() < 2 {
                return None;
            }
            let inner = &text[1..text.len() - 1];
            result = inner.replace("\"\"", "\"");
        }
        SyntaxKind::STRING_START => {
            let text = first_token.text();
            if text.is_empty() {
                return None;
            }
            result.push_str(&text[1..]);

            for token in tokens {
                match token.kind() {
                    SyntaxKind::NEWLINE => {
                        result.push('\n');
                    }
                    SyntaxKind::STRING_PART => {
                        let text = token.text();
                        if let Some(content) = text.strip_prefix('|') {
                            result.push_str(content);
                        }
                    }
                    SyntaxKind::STRING_TAIL => {
                        let text = token.text();
                        if let Some(content) = text.strip_prefix('|') {
                            if let Some(content) = content.strip_suffix('"') {
                                result.push_str(content);
                            }
                        }
                        break;
                    }
                    _ => {}
                }
            }

            result = result.replace("\"\"", "\"");
        }
        _ => return None,
    }

    Some(result)
}

/// Visits every query literal of the file that its method executes as the whole text of a
/// `Запрос` object, with that execution's facts and the mapper of the literal. The file's two
/// query lists are ordered differently, so they are joined by identity, never by position.
pub(crate) fn for_each_executed_query(
    ctx: &DiagnosticsContext,
    mut f: impl FnMut(&hir::ExecutedQueryText, &sdbl_hir::SdblPackage, &SdblPositionMapper<'_>, &str),
) {
    let sdbl_hirs = ctx.sdbl_hir_in_file();
    if sdbl_hirs.is_empty() {
        return;
    }
    let sdbl_queries = ctx.all_sdbl_in_file();
    let bsl_source = ctx.file_text();
    let line_starts = build_line_index_shared(&bsl_source);

    let mut executed: rustc_hash::FxHashMap<
        hir::MethodKey,
        std::sync::Arc<[hir::ExecutedQueryText]>,
    > = Default::default();
    for (sdbl_id, package) in sdbl_hirs.iter() {
        let hir::DefWithBodyId::Method(local_id) = sdbl_id.owner else {
            continue;
        };
        let texts = executed.entry(local_id).or_insert_with(|| {
            ctx.executed_query_texts(hir::MethodId {
                module: hir::ModuleId::new(ctx.file_id),
                local_id,
            })
        });
        let Some(text) = texts.iter().find(|text| text.literal == sdbl_id.expr_id) else {
            continue;
        };
        let Some((_, query_info)) = sdbl_queries.iter().find(|(id, _)| id == sdbl_id) else {
            continue;
        };
        let mapper = SdblPositionMapper::from_query_info(query_info, &bsl_source, &line_starts);
        f(text, package, &mapper, &query_info.query_text);
    }
}

#[cfg(test)]
mod dialect_tests {
    use super::sdbl_dialect_attested;

    #[test]
    fn a_cyrillic_keyword_attests() {
        assert!(sdbl_dialect_attested("ВЫБРАТЬ Оклад ИЗ Кадры"));
    }

    #[test]
    fn a_metadata_prefix_attests() {
        assert!(sdbl_dialect_attested("SELECT Shelf FROM Catalog.Library"));
    }

    #[test]
    fn a_wql_text_attests_to_nothing() {
        assert!(!sdbl_dialect_attested("SELECT Caption, ProcessId FROM Win32_Process"));
    }

    // Cyrillic inside a string literal is data, not language.
    #[test]
    fn cyrillic_in_a_literal_does_not_attest() {
        assert!(!sdbl_dialect_attested(
            "SELECT Caption FROM Win32_Process WHERE Caption = \"Блокнот\""
        ));
    }

    // A metadata-type word attests only as a prefix: `Report` and `Role` are ordinary
    // table and column names in T-SQL and WQL.
    #[test]
    fn a_metadata_type_word_outside_prefix_position_does_not_attest() {
        assert!(!sdbl_dialect_attested("SELECT Report, Body FROM Reports"));
        assert!(!sdbl_dialect_attested("SELECT Caption, Role FROM Win32_Process"));
        assert!(!sdbl_dialect_attested("SELECT Caption FROM enum"));
    }

    // Reports, roles and common modules are metadata kinds without query tables: a
    // `Report.` qualifier can only be a foreign table name.
    #[test]
    fn a_metadata_kind_without_query_tables_does_not_attest() {
        assert!(!sdbl_dialect_attested("SELECT Report.Name FROM Report"));
        assert!(!sdbl_dialect_attested("SELECT Role.Name FROM Role"));
        assert!(!sdbl_dialect_attested("SELECT Name FROM CommonModule.Items"));
    }

    #[test]
    fn a_query_table_root_attests_in_either_language() {
        assert!(sdbl_dialect_attested("SELECT Amount FROM AccumulationRegister.Stock"));
        assert!(sdbl_dialect_attested("SELECT Amount FROM Document.Invoice"));
    }

    #[test]
    fn a_metadata_prefix_attests_across_whitespace() {
        assert!(sdbl_dialect_attested("SELECT Shelf FROM Catalog\n\t.Library"));
    }

    // WQL and T-SQL quote strings with apostrophes; what sits between them is data.
    #[test]
    fn a_single_quoted_literal_does_not_attest() {
        assert!(!sdbl_dialect_attested(
            "SELECT Caption FROM Win32_Process WHERE Caption = 'Task.Run'"
        ));
        assert!(!sdbl_dialect_attested("SELECT Caption FROM Win32_Process WHERE Name = 'Строка'"));
        assert!(!sdbl_dialect_attested("SELECT Caption FROM Win32_Process WHERE Caption = 'из'"));
    }

    // The SDBL lexer knows only `//` comments; foreign comments and quoted identifiers
    // would otherwise reach the check as code.
    #[test]
    fn a_foreign_comment_or_quoted_identifier_does_not_attest() {
        assert!(!sdbl_dialect_attested("SELECT Caption FROM Win32_Process -- выгрузка из базы"));
        assert!(!sdbl_dialect_attested("SELECT Caption /* выбрать из Catalog.Library */ FROM T"));
        assert!(!sdbl_dialect_attested("SELECT [Имя из базы] FROM Users"));
        assert!(!sdbl_dialect_attested("SELECT `Имя из базы` FROM Users"));
    }

    #[test]
    fn code_after_a_foreign_comment_still_attests() {
        assert!(sdbl_dialect_attested("-- don't\nSELECT Shelf FROM Catalog.Library"));
        assert!(sdbl_dialect_attested("SELECT /* note */ Shelf ИЗ Кадры"));
        assert!(sdbl_dialect_attested("SELECT [Shelf] FROM Catalog.Library"));
        assert!(sdbl_dialect_attested("SELECT Сумма - -1 КАК Остаток"));
    }

    #[test]
    fn a_date_literal_does_not_hide_what_follows() {
        assert!(sdbl_dialect_attested("SELECT Shelf FROM Catalog.Library WHERE Date > '20200101'"));
        assert!(sdbl_dialect_attested("SELECT Shelf WHERE Date > '20200101' ИЛИ Shelf = 1"));
    }
}
