//! In-code diagnostic suppression directives.
//!
//! A single choke point ([`apply`]) filters a file's already-computed diagnostics against
//! suppression directives written in comments, so LSP push, MCP `diagnostics`, and the CLI
//! `analyze` reporters all honour them identically (every surface funnels through
//! [`crate::apply_extension_merge`]).
//!
//! Native directives (always recognised):
//! - `// bsl-analyzer:off Код1, Код2` … `// bsl-analyzer:on Код1` — suppress a range;
//!   `off` with no matching `on` extends to end of file (put it on line 1 for a whole-file mute);
//! - `// bsl-analyzer:disable-next-line Код1` — suppress the next line;
//! - `// bsl-analyzer:disable-line Код1` — suppress the directive's own line (trailing use);
//! - any suppress form with no codes mutes every diagnostic in scope.
//!
//! bsl-language-server directives (`// BSLLS:Код-off`/`-on`, `// BSLLS-off`/`-on`, localized
//! `выкл`/`вкл`) are recognised as aliases by default so an existing project's suppression
//! comments keep working; set `bsllsSuppressionCompat = false` to turn the aliases off.

use std::collections::HashMap;
use std::str::FromStr;

use ide_db::TextRange;
use syntax::{NodeOrToken, SyntaxKind, SyntaxNode, TextSize};

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, Severity};

pub const UNKNOWN_CODE_METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub const WITHOUT_CODE_METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const NATIVE_MARKER: &str = "bsl-analyzer:";
const BSLLS_MARKER: &str = "bslls";

/// The two diagnostics this module itself emits, which must never be suppressed by a directive
/// (a typo in a directive must still surface) — the retain step below skips them.
pub(crate) fn is_meta_code(code: DiagnosticCode) -> bool {
    matches!(code, DiagnosticCode::UnknownSuppressionCode | DiagnosticCode::SuppressionWithoutCode)
}

/// Filter `diags` in place against the file's suppression directives and append the module's own
/// meta-diagnostics (unknown code in a directive, code-less suppression). Returns `true` when it
/// changed `diags`; the finalizer normalizes unconditionally, so the flag is informational.
///
/// Runs after the scope gate by design: the metas below point at the directive, not at a code
/// line, and must survive the line gate the same way they survive directives and the baseline
/// (github#61). The gate removes only, so for source diagnostics the two steps commute; the
/// order is chosen for the metas this step *adds*. When the caller defers the line gate the
/// metas do meet it, after this step, and it spares them by their code rather than by this
/// order ([`crate::diagnostic_in_scope`]).
pub(crate) fn apply(
    db: &dyn ide_db::RootDatabase,
    file_id: vfs::FileId,
    config: &DiagnosticsConfig,
    diags: &mut Vec<Diagnostic>,
) -> bool {
    let text = db.file_text(file_id);

    if !has_directive_marker(&text, config.bslls_suppression_compat) {
        return false;
    }

    let line_index = line_index::LineIndex::new(&text);
    let root = db.parse(file_id).syntax_node();
    let parsed = parse_directives(&root, &line_index, config.bslls_suppression_compat);

    if parsed.map.is_empty() && parsed.metas.is_empty() {
        return false;
    }

    let before = diags.len();
    if !parsed.map.is_empty() {
        diags.retain(|d| {
            if is_meta_code(d.code) {
                return true;
            }
            !parsed.map.is_suppressed_at(&line_index, d.range.start(), d.code)
        });
    }
    let mut changed = diags.len() != before;

    for meta in parsed.metas {
        let code = meta.code();
        if config.is_disabled(code) {
            continue;
        }
        diags.push(Diagnostic {
            code,
            message: meta.message(),
            severity: severity_of(config, code),
            range: meta.range,
            tags: vec![],
            fixes: vec![],
        });
        changed = true;
    }

    changed
}

/// What the file's directives say, for a check that has to know before [`apply`] runs. It adds
/// no meta-diagnostics: [`apply`] alone reports a broken directive, once, at the end.
pub(crate) struct SuppressionLookup {
    map: SuppressionMap,
    line_index: line_index::LineIndex,
}

impl SuppressionLookup {
    pub(crate) fn new(root: &SyntaxNode, text: &str, bslls: bool) -> Self {
        let line_index = line_index::LineIndex::new(text);
        let map = if has_directive_marker(text, bslls) {
            parse_directives(root, &line_index, bslls).map
        } else {
            SuppressionMap::default()
        };
        Self { map, line_index }
    }

    /// Whether [`apply`] would drop a finding of `code` starting at `offset`.
    pub(crate) fn is_suppressed(&self, offset: TextSize, code: DiagnosticCode) -> bool {
        self.map.is_suppressed_at(&self.line_index, offset, code)
    }
}

fn has_directive_marker(text: &str, bslls: bool) -> bool {
    contains_ignore_ascii_case(text, NATIVE_MARKER)
        || (bslls && contains_ignore_ascii_case(text, BSLLS_MARKER))
}

fn severity_of(config: &DiagnosticsConfig, code: DiagnosticCode) -> Severity {
    config.get_effective_metadata(code).map(|m| m.severity_value()).unwrap_or(Severity::Warning)
}

// ---------------------------------------------------------------------------
// Directive parsing
// ---------------------------------------------------------------------------

/// An inclusive 0-based line range; `end == u32::MAX` means "to end of file".
type LineRange = (u32, u32);

#[derive(Default)]
struct SuppressionMap {
    all: Vec<LineRange>,
    per_code: HashMap<DiagnosticCode, Vec<LineRange>>,
}

impl SuppressionMap {
    fn is_empty(&self) -> bool {
        self.all.is_empty() && self.per_code.is_empty()
    }

    fn is_suppressed(&self, line: u32, code: DiagnosticCode) -> bool {
        in_ranges(&self.all, line)
            || self.per_code.get(&code).is_some_and(|ranges| in_ranges(ranges, line))
    }

    /// A finding is addressed by the line its range starts on.
    fn is_suppressed_at(
        &self,
        line_index: &line_index::LineIndex,
        offset: TextSize,
        code: DiagnosticCode,
    ) -> bool {
        line_index.try_line_col(offset).is_some_and(|lc| self.is_suppressed(lc.line, code))
    }
}

fn in_ranges(ranges: &[LineRange], line: u32) -> bool {
    ranges.iter().any(|&(start, end)| line >= start && line <= end)
}

enum MetaKind {
    UnknownCode(String),
    WithoutCode,
}

struct Meta {
    range: TextRange,
    kind: MetaKind,
}

impl Meta {
    fn code(&self) -> DiagnosticCode {
        match self.kind {
            MetaKind::UnknownCode(_) => DiagnosticCode::UnknownSuppressionCode,
            MetaKind::WithoutCode => DiagnosticCode::SuppressionWithoutCode,
        }
    }

    fn message(&self) -> String {
        match &self.kind {
            MetaKind::UnknownCode(token) => {
                format!("Неизвестный код диагностики в директиве подавления: {token}")
            }
            MetaKind::WithoutCode => {
                "Директива подавления без указания кода подавляет все диагностики в области"
                    .to_string()
            }
        }
    }
}

struct Parsed {
    map: SuppressionMap,
    metas: Vec<Meta>,
}

/// Open `off` ranges awaiting an `on`, per key. Flushed to end-of-file when unclosed.
#[derive(Default)]
struct OpenStacks {
    all: Vec<u32>,
    per_code: HashMap<DiagnosticCode, Vec<u32>>,
}

fn parse_directives(root: &SyntaxNode, line_index: &line_index::LineIndex, bslls: bool) -> Parsed {
    let mut map = SuppressionMap::default();
    let mut metas = Vec::new();
    let mut stacks = OpenStacks::default();

    // Lines that carry executable code (any non-trivia token), for the trailing-comment rule.
    let mut code_lines = std::collections::HashSet::new();
    let mut comments: Vec<(TextRange, u32, String)> = Vec::new();

    for element in root.descendants_with_tokens() {
        let NodeOrToken::Token(token) = element else { continue };
        let kind = token.kind();
        let line = line_index.line_col(token.text_range().start()).line;
        match kind {
            SyntaxKind::COMMENT => {
                comments.push((token.text_range(), line, token.text().to_string()));
            }
            SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE => {}
            _ => {
                code_lines.insert(line);
            }
        }
    }

    for (range, line, text) in &comments {
        let body = strip_comment_slashes(text);
        if let Some(directive) = parse_native(body) {
            apply_native(directive, *range, *line, &mut map, &mut stacks, &mut metas);
        }
        if bslls {
            for directive in parse_bslls(body) {
                apply_bslls(directive, *line, &code_lines, &mut map, &mut stacks);
            }
        }
    }

    // Unclosed `off` ranges suppress to end of file.
    for start in stacks.all {
        map.all.push((start, u32::MAX));
    }
    for (code, starts) in stacks.per_code {
        for start in starts {
            map.per_code.entry(code).or_default().push((start, u32::MAX));
        }
    }

    Parsed { map, metas }
}

enum Keyword {
    Off,
    On,
    DisableNextLine,
    DisableLine,
}

struct NativeDirective<'a> {
    keyword: Keyword,
    args: &'a str,
}

/// Parse an anchored native directive from a comment body (slashes already stripped).
fn parse_native(body: &str) -> Option<NativeDirective<'_>> {
    let rest = strip_prefix_ignore_ascii_case(body.trim_start(), NATIVE_MARKER)?;
    let rest = rest.trim_start();
    let (word, args) = split_first_word(rest);
    let keyword = match word.to_ascii_lowercase().as_str() {
        "off" => Keyword::Off,
        "on" => Keyword::On,
        "disable-next-line" => Keyword::DisableNextLine,
        "disable-line" => Keyword::DisableLine,
        _ => return None,
    };
    Some(NativeDirective { keyword, args })
}

/// Split `args` into resolved codes and record unknown tokens as meta-diagnostics.
fn resolve_codes(args: &str, range: TextRange, metas: &mut Vec<Meta>) -> Vec<DiagnosticCode> {
    let mut codes = Vec::new();
    for token in args.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
        match DiagnosticCode::from_str(token) {
            Ok(code) => codes.push(code),
            Err(_) => {
                metas.push(Meta { range, kind: MetaKind::UnknownCode(token.to_string()) });
            }
        }
    }
    codes
}

fn apply_native(
    directive: NativeDirective<'_>,
    range: TextRange,
    line: u32,
    map: &mut SuppressionMap,
    stacks: &mut OpenStacks,
    metas: &mut Vec<Meta>,
) {
    let codes = resolve_codes(directive.args, range, metas);
    // "All" only when the author wrote no code tokens at all. If tokens were present but none
    // resolved (every one a typo, already flagged), the directive targets nothing — it must not
    // silently mute everything.
    let has_tokens = directive.args.split([',', ' ', '\t']).any(|t| !t.is_empty());
    let all = !has_tokens;

    let suppress_form =
        matches!(directive.keyword, Keyword::Off | Keyword::DisableNextLine | Keyword::DisableLine);
    if suppress_form && all {
        metas.push(Meta { range, kind: MetaKind::WithoutCode });
    }

    match directive.keyword {
        Keyword::Off => push_off(line, all, &codes, stacks),
        Keyword::On => pop_on(line, all, &codes, map, stacks),
        Keyword::DisableNextLine => push_single(line.saturating_add(1), all, &codes, map),
        Keyword::DisableLine => push_single(line, all, &codes, map),
    }
}

fn push_off(line: u32, all: bool, codes: &[DiagnosticCode], stacks: &mut OpenStacks) {
    if all {
        stacks.all.push(line);
    } else {
        for &code in codes {
            stacks.per_code.entry(code).or_default().push(line);
        }
    }
}

fn pop_on(
    line: u32,
    all: bool,
    codes: &[DiagnosticCode],
    map: &mut SuppressionMap,
    stacks: &mut OpenStacks,
) {
    if all {
        if let Some(start) = stacks.all.pop() {
            map.all.push((start, line));
        }
    } else {
        for &code in codes {
            if let Some(start) = stacks.per_code.get_mut(&code).and_then(Vec::pop) {
                map.per_code.entry(code).or_default().push((start, line));
            }
        }
    }
}

fn push_single(target: u32, all: bool, codes: &[DiagnosticCode], map: &mut SuppressionMap) {
    if all {
        map.all.push((target, target));
    } else {
        for &code in codes {
            map.per_code.entry(code).or_default().push((target, target));
        }
    }
}

// ---------------------------------------------------------------------------
// bsl-language-server alias directives
// ---------------------------------------------------------------------------

struct BsllsDirective {
    /// `None` targets every diagnostic (`BSLLS-off`), `Some` a specific code.
    code: Option<DiagnosticCode>,
    on: bool,
}

/// Scan a comment body for every `BSLLS[:Код]-off/-on` occurrence (case-insensitive, localized
/// `выкл`/`вкл`). Unknown foreign codes are skipped silently — they belong to another tool's
/// namespace, so a typo there is not ours to flag.
fn parse_bslls(body: &str) -> Vec<BsllsDirective> {
    let lower = body.to_ascii_lowercase();
    let mut directives = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(BSLLS_MARKER) {
        let start = search_from + rel;
        let after = &body[start + BSLLS_MARKER.len()..];
        search_from = start + BSLLS_MARKER.len();

        let (key, rest) = if let Some(rest) = after.strip_prefix(':') {
            let (word, rest) = split_key(rest);
            if word.is_empty() {
                continue;
            }
            (Some(word), rest)
        } else {
            (None, after)
        };

        let Some(on) = parse_bslls_switch(rest) else { continue };
        let code = match key {
            Some(word) => match resolve_bslls_key(word) {
                Some(code) => Some(code),
                None => continue,
            },
            None => None,
        };
        directives.push(BsllsDirective { code, on });
    }
    directives
}

/// bsl-language-server diagnostic keys that name the same rule as one of ours under a different
/// identifier. Most bslls keys match our `DiagnosticCode` names verbatim (our naming was modeled
/// on bslls); this table covers the few genuine renames so a `// BSLLS:Ключ-off` written for the
/// bslls name still suppresses our diagnostic. Extend it as diagnostics from the coverage backlog
/// land under names that diverge from bslls.
const BSLLS_KEY_ALIASES: &[(&str, DiagnosticCode)] =
    &[("AssignToReadOnlyProperty", DiagnosticCode::ReadOnlyPropertyAssignment)];

/// Resolve a bslls diagnostic key to our code: exact-name match first, then the rename table.
/// Returns `None` for keys we do not implement (a foreign namespace we simply do not suppress).
fn resolve_bslls_key(key: &str) -> Option<DiagnosticCode> {
    if let Ok(code) = DiagnosticCode::from_str(key) {
        return Some(code);
    }
    BSLLS_KEY_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(key))
        .map(|(_, code)| *code)
}

/// Parse the `-off`/`-on`/`-выкл`/`-вкл` suffix that follows a BSLLS key. Matching is
/// case-insensitive (including Cyrillic case) and requires a word boundary after the switch so
/// `-выключено` is not read as `-выкл`.
fn parse_bslls_switch(rest: &str) -> Option<bool> {
    let rest = rest.strip_prefix('-')?;
    for (token, on) in [("off", false), ("on", true), ("выкл", false), ("вкл", true)] {
        if let Some(tail) = strip_prefix_ignore_case(rest, token) {
            if !is_word_char(tail.chars().next()) {
                return Some(on);
            }
        }
    }
    None
}

fn apply_bslls(
    directive: BsllsDirective,
    line: u32,
    code_lines: &std::collections::HashSet<u32>,
    map: &mut SuppressionMap,
    stacks: &mut OpenStacks,
) {
    let all = directive.code.is_none();
    let codes: Vec<DiagnosticCode> = directive.code.into_iter().collect();
    if directive.on {
        pop_on(line, all, &codes, map, stacks);
    } else if code_lines.contains(&line) {
        // A trailing `off` on a line that also carries code mutes just that line (BSLLS semantics).
        push_single(line, all, &codes, map);
    } else {
        push_off(line, all, &codes, stacks);
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// The comment body: leading slashes off, and the line break the lexer keeps inside the
/// token on a CRLF file off too — otherwise the last code of a directive arrives as
/// `Код\r` and no directive in a 1C dump (always CRLF) ever resolves.
fn strip_comment_slashes(text: &str) -> &str {
    text.trim_start_matches('/').trim_end_matches(['\r', '\n'])
}

fn split_first_word(s: &str) -> (&str, &str) {
    match s.find([' ', '\t']) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

/// Word character for directive boundaries — Unicode-aware so a Cyrillic letter right after a
/// switch (`выключено`) counts as continuing the word.
fn is_word_char(ch: Option<char>) -> bool {
    ch.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn split_key(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` refuses a byte index inside a multi-byte character (e.g. a Cyrillic
    // comment shorter in chars than the prefix in bytes) — such a head cannot
    // equal an ASCII prefix anyway.
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Strip `prefix` from `s` comparing case-insensitively, including non-ASCII (Cyrillic) case.
/// `prefix` is expected lowercase; returns the remainder of `s` after the matched prefix.
fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let mut consumed = 0;
    let mut s_chars = s.chars();
    for pc in prefix.chars() {
        match s_chars.next() {
            Some(sc) if sc == pc || sc.to_lowercase().eq(pc.to_lowercase()) => {
                consumed += sc.len_utf8();
            }
            _ => return None,
        }
    }
    Some(&s[consumed..])
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_first = needle.as_bytes()[0];
    haystack.as_bytes().windows(needle.len()).any(|window| {
        window[0].eq_ignore_ascii_case(&needle_first)
            && window.eq_ignore_ascii_case(needle.as_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{check_file_diagnostics, check_file_diagnostics_with_config};
    use crate::DiagnosticsConfig;

    fn has(diags: &[Diagnostic], code: DiagnosticCode) -> bool {
        diags.iter().any(|d| d.code == code)
    }

    fn self_assign_lines(diags: &[Diagnostic], source: &str) -> Vec<u32> {
        diags
            .iter()
            .filter(|d| d.code == DiagnosticCode::SelfAssign)
            .map(|d| source[..usize::from(d.range.start())].matches('\n').count() as u32)
            .collect()
    }

    #[test]
    fn contains_ignore_ascii_case_matches_mixed_case() {
        assert!(contains_ignore_ascii_case("xx BSL-Analyzer:Off", "bsl-analyzer:"));
        assert!(!contains_ignore_ascii_case("no directive here", "bsl-analyzer:"));
    }

    #[test]
    fn cyrillic_comment_shorter_than_marker_in_chars_is_not_a_directive() {
        // The prefix length in bytes lands inside a multi-byte character here;
        // slicing at it used to panic and silently drop the whole file's
        // diagnostics.
        assert!(strip_prefix_ignore_ascii_case("Возвращает строку", NATIVE_MARKER).is_none());
        let code = "Процедура Тест()\n    // Возвращает строку-представление координат\n    А = А;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert_eq!(self_assign_lines(&diags, code), vec![2]);
    }

    #[test]
    fn native_range_suppresses_between_off_and_on() {
        let code = "Процедура Тест()\n    // bsl-analyzer:off SelfAssign\n    А = А;\n    // bsl-analyzer:on SelfAssign\n    Б = Б;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        // `А = А;` (line 2) is inside the off/on range; `Б = Б;` (line 4) is outside it.
        assert_eq!(self_assign_lines(&diags, code), vec![4]);
    }

    #[test]
    fn native_disable_next_line_suppresses_only_next_line() {
        let code = "Процедура Тест()\n    // bsl-analyzer:disable-next-line SelfAssign\n    А = А;\n    Б = Б;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert_eq!(self_assign_lines(&diags, code), vec![3]);
    }

    #[test]
    fn native_disable_line_suppresses_trailing_same_line() {
        let code = "Процедура Тест()\n    А = А; // bsl-analyzer:disable-line SelfAssign\n    Б = Б;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert_eq!(self_assign_lines(&diags, code), vec![2]);
    }

    #[test]
    fn native_off_without_on_mutes_to_end_of_file() {
        let code = "Процедура Тест()\n    // bsl-analyzer:off SelfAssign\n    А = А;\n    Б = Б;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert!(self_assign_lines(&diags, code).is_empty(), "{diags:#?}");
    }

    #[test]
    fn unknown_code_in_directive_is_flagged() {
        let code =
            "Процедура Тест()\n    // bsl-analyzer:off NoSuchRule\n    А = А;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert!(has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
        // A typo'd code must not silently mute anything.
        assert_eq!(self_assign_lines(&diags, code), vec![2]);
    }

    #[test]
    fn code_less_off_is_flagged_and_suppresses_all() {
        let code = "Процедура Тест()\n    // bsl-analyzer:off\n    А = А;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        // The WithoutCode meta must surface even though the directive covers its own line.
        assert!(has(&diags, DiagnosticCode::SuppressionWithoutCode), "{diags:#?}");
        assert!(!has(&diags, DiagnosticCode::SelfAssign), "{diags:#?}");
    }

    #[test]
    fn bslls_alias_inert_when_disabled() {
        // bslls compat is on by default; explicitly disabling it makes BSLLS directives inert.
        let code = "Процедура Тест()\n    // BSLLS:SelfAssign-off\n    А = А;\n    // BSLLS:SelfAssign-on\n    Б = Б;\nКонецПроцедуры\n";
        let config = DiagnosticsConfig {
            bslls_suppression_compat: false,
            ..DiagnosticsConfig::all_enabled()
        };
        let diags = check_file_diagnostics_with_config(code, config);
        assert_eq!(self_assign_lines(&diags, code), vec![2, 4]);
    }

    #[test]
    fn bslls_alias_range_honored_by_default() {
        // Default config: bslls compat is enabled, so the directive range is honoured.
        let code = "Процедура Тест()\n    // BSLLS:SelfAssign-off\n    А = А;\n    // BSLLS:SelfAssign-on\n    Б = Б;\nКонецПроцедуры\n";
        let diags = check_file_diagnostics(code);
        assert_eq!(self_assign_lines(&diags, code), vec![4]);
    }

    #[test]
    fn bslls_trailing_off_suppresses_single_line() {
        let code =
            "Процедура Тест()\n    А = А; // BSLLS:SelfAssign-off\n    Б = Б;\nКонецПроцедуры\n";
        let config = DiagnosticsConfig {
            bslls_suppression_compat: true,
            ..DiagnosticsConfig::all_enabled()
        };
        let diags = check_file_diagnostics_with_config(code, config);
        assert_eq!(self_assign_lines(&diags, code), vec![2]);
    }

    #[test]
    fn bslls_localized_uppercase_switch_honored() {
        // Cyrillic `ВЫКЛ`/`ВКЛ` must fold case like their lowercase forms.
        let code = "Процедура Тест()\n    // BSLLS:SelfAssign-ВЫКЛ\n    А = А;\n    // BSLLS:SelfAssign-ВКЛ\n    Б = Б;\nКонецПроцедуры\n";
        let config = DiagnosticsConfig {
            bslls_suppression_compat: true,
            ..DiagnosticsConfig::all_enabled()
        };
        let diags = check_file_diagnostics_with_config(code, config);
        assert_eq!(self_assign_lines(&diags, code), vec![4]);
    }

    #[test]
    fn bslls_localized_switch_requires_word_boundary() {
        // `выключено` must not be read as the `выкл` switch.
        let code = "Процедура Тест()\n    // BSLLS:SelfAssign-выключено временно\n    А = А;\nКонецПроцедуры\n";
        let config = DiagnosticsConfig {
            bslls_suppression_compat: true,
            ..DiagnosticsConfig::all_enabled()
        };
        let diags = check_file_diagnostics_with_config(code, config);
        assert_eq!(self_assign_lines(&diags, code), vec![2]);
    }

    #[test]
    fn resolve_bslls_key_exact_alias_and_unknown() {
        // Exact name match (the common case).
        assert_eq!(resolve_bslls_key("SelfAssign"), Some(DiagnosticCode::SelfAssign));
        // Genuine rename: bslls `AssignToReadOnlyProperty` == our `ReadOnlyPropertyAssignment`.
        assert_eq!(
            resolve_bslls_key("AssignToReadOnlyProperty"),
            Some(DiagnosticCode::ReadOnlyPropertyAssignment)
        );
        assert_eq!(
            resolve_bslls_key("assigntoreadonlyproperty"),
            Some(DiagnosticCode::ReadOnlyPropertyAssignment)
        );
        // A bslls diagnostic we do not implement resolves to nothing.
        assert_eq!(resolve_bslls_key("CompareWithBoolean"), None);
    }

    #[test]
    fn bslls_unknown_foreign_code_is_not_flagged() {
        let code =
            "Процедура Тест()\n    // BSLLS:SomeForeignRule-off\n    А = А;\nКонецПроцедуры\n";
        let config = DiagnosticsConfig {
            bslls_suppression_compat: true,
            ..DiagnosticsConfig::all_enabled()
        };
        let diags = check_file_diagnostics_with_config(code, config);
        assert!(!has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
    }

    // 1C dumps are always CRLF; the lexer keeps the `\r` inside the comment token.
    fn crlf(code: &str) -> String {
        code.replace('\n', "\r\n")
    }

    // `check_file_diagnostics` goes through `Fixture::parse`, which splits on `.lines()` and so
    // silently turns CRLF into LF; this helper hands the text to the database untouched.
    fn check_raw_diagnostics(code: &str) -> Vec<Diagnostic> {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;

        let mut fixture = test_fixture::Fixture::new();
        fixture.add_file("/test.bsl", code);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }
        crate::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
    }

    fn unknown_code_messages(diags: &[Diagnostic]) -> Vec<&str> {
        diags
            .iter()
            .filter(|d| d.code == DiagnosticCode::UnknownSuppressionCode)
            .map(|d| d.message.as_str())
            .collect()
    }

    const OFF_FIXTURE: &str =
        "// bsl-analyzer:off SelfAssign\nПроцедура Тест()\n    А = А;\nКонецПроцедуры\n";

    #[test]
    fn crlf_off_directive_suppresses_without_unknown_code() {
        let code = crlf(OFF_FIXTURE);
        assert!(code.contains("\r\n"));
        let diags = check_raw_diagnostics(&code);
        assert!(!has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
        assert!(!has(&diags, DiagnosticCode::SelfAssign), "{diags:#?}");
    }

    #[test]
    fn lf_off_directive_control_suppresses_without_unknown_code() {
        let diags = check_raw_diagnostics(OFF_FIXTURE);
        assert!(!has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
        assert!(!has(&diags, DiagnosticCode::SelfAssign), "{diags:#?}");
    }

    #[test]
    fn crlf_disable_next_line_suppresses_next_line() {
        let code = crlf(
            "Процедура Тест()\n    // bsl-analyzer:disable-next-line SelfAssign\n    А = А;\n    Б = Б;\nКонецПроцедуры\n",
        );
        let diags = check_raw_diagnostics(&code);
        assert!(!has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
        assert_eq!(self_assign_lines(&diags, &code), vec![3]);
    }

    #[test]
    fn crlf_unknown_code_is_flagged_without_carriage_return() {
        let code = crlf(
            "Процедура Тест()\n    // bsl-analyzer:off NoSuchRule\n    А = А;\nКонецПроцедуры\n",
        );
        let diags = check_raw_diagnostics(&code);
        assert_eq!(
            unknown_code_messages(&diags),
            vec!["Неизвестный код диагностики в директиве подавления: NoSuchRule"],
            "{diags:#?}"
        );
        assert_eq!(self_assign_lines(&diags, &code), vec![2]);
    }

    // Control, not a regression test: the BSLLS switch parser already stops at a trailing `\r`
    // (not a word char); this guards that stripping the line break keeps the alias working.
    #[test]
    fn crlf_bslls_alias_range_control_honored() {
        let code = crlf(
            "Процедура Тест()\n    // BSLLS:SelfAssign-off\n    А = А;\n    // BSLLS:SelfAssign-on\n    Б = Б;\nКонецПроцедуры\n",
        );
        let diags = check_raw_diagnostics(&code);
        assert!(!has(&diags, DiagnosticCode::UnknownSuppressionCode), "{diags:#?}");
        assert_eq!(self_assign_lines(&diags, &code), vec![4]);
    }

    /// Every line of `code` that holds a self-assignment, by 0-based number.
    fn self_assign_candidates(code: &str) -> Vec<u32> {
        code.lines()
            .enumerate()
            .filter(|(_, line)| line.contains("А = А;") || line.contains("Б = Б;"))
            .map(|(number, _)| number as u32)
            .collect()
    }

    fn line_start(code: &str, line: u32) -> TextSize {
        let offset: usize = code.split_inclusive('\n').take(line as usize).map(str::len).sum();
        TextSize::from(offset as u32)
    }

    /// The read-only lookup says, line by line, what [`apply`] removes from the diagnostics and
    /// leaves the file's meta-diagnostics to `apply` alone.
    #[test]
    fn lookup_agrees_with_apply_on_every_directive_shape() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};

        const HEAD: &str = "Процедура Тест()\n";
        const TAIL: &str = "КонецПроцедуры\n";
        let shapes: [(&str, Vec<u32>, Vec<u32>); 10] = [
            // (statements, suppressed lines with BSLLS compat, suppressed lines without it)
            ("    // bsl-analyzer:disable-next-line SelfAssign\n    А = А;\n    Б = Б;\n", vec![2], vec![2]),
            ("    А = А; // bsl-analyzer:disable-line SelfAssign\n    Б = Б;\n", vec![1], vec![1]),
            ("    // bsl-analyzer:off SelfAssign\n    А = А;\n    // bsl-analyzer:on SelfAssign\n    Б = Б;\n", vec![2], vec![2]),
            ("    // bsl-analyzer:off SelfAssign\n    А = А;\n    Б = Б;\n", vec![2, 3], vec![2, 3]),
            ("    // BSLLS:SelfAssign-off\n    А = А;\n    // BSLLS:SelfAssign-on\n    Б = Б;\n", vec![2], vec![]),
            ("    // bsl-analyzer:off\n    А = А;\n", vec![2], vec![2]),
            ("    // bsl-analyzer:off SelfAssig\n    А = А;\n", vec![], vec![]),
            ("    // bsl-analyzer:on SelfAssign\n    А = А;\n", vec![], vec![]),
            ("    А = А;\n    Б = Б;\n", vec![], vec![]),
            // A native directive makes the file worth parsing; the BSLLS one in it counts only with compat.
            (
                "    // bsl-analyzer:disable-next-line SelfAssign\n    А = А;\n    // BSLLS:SelfAssign-off\n    Б = Б;\n    // BSLLS:SelfAssign-on\n",
                vec![2, 4],
                vec![2],
            ),
        ];
        for (statements, with_compat, without_compat) in shapes {
            for line_break in ["\n", "\r\n"] {
                let code = format!("{HEAD}{statements}{TAIL}").replace('\n', line_break);
                for (compat, expected) in [(true, &with_compat), (false, &without_compat)] {
                    let config = DiagnosticsConfig {
                        bslls_suppression_compat: compat,
                        ..DiagnosticsConfig::all_enabled()
                    };
                    let mut fixture = test_fixture::Fixture::new();
                    fixture.add_file("/test.bsl", &code);
                    let file_id = fixture.first_file().expect("fixture should have a file");
                    let mut db = ide_db::RootDatabaseImpl::new();
                    let mut file_set = vfs::FileSet::default();
                    file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
                    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
                    db.set_file_source_root(file_id, SourceRootId(0));
                    db.set_file_text(file_id, &code);

                    let read: &dyn ide_db::RootDatabase = &db;
                    let lookup = SuppressionLookup::new(
                        &read.parse(file_id).syntax_node(),
                        &read.file_text(file_id),
                        compat,
                    );
                    let candidates = self_assign_candidates(&code);
                    let hidden: Vec<u32> = candidates
                        .iter()
                        .copied()
                        .filter(|&line| {
                            lookup
                                .is_suppressed(line_start(&code, line), DiagnosticCode::SelfAssign)
                        })
                        .collect();
                    assert_eq!(&hidden, expected, "lookup, compat={compat}:\n{code}");

                    let diags = crate::file_diagnostics(&db, file_id, &config);
                    let shown = self_assign_lines(&diags, &code);
                    let left: Vec<u32> =
                        candidates.into_iter().filter(|line| !expected.contains(line)).collect();
                    assert_eq!(shown, left, "apply, compat={compat}:\n{code}");
                }
            }
        }
    }
}
