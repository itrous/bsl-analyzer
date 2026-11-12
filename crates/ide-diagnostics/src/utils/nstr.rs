use crate::{AnalysisContext, DiagnosticCode};
use std::collections::{BTreeSet, HashSet};
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxNode};

pub const DEFAULT_DECLARED_LANGUAGES: &str = "ru";

pub fn is_nstr_call(name: &str) -> bool {
    name.eq_ignore_ascii_case("НСтр") || name.eq_ignore_ascii_case("NStr")
}

pub fn is_template_call(name: &str) -> bool {
    name.eq_ignore_ascii_case("СтрШаблон") || name.eq_ignore_ascii_case("StrTemplate")
}

pub fn has_template_in_parents(node: &SyntaxNode) -> bool {
    for ancestor in node.ancestors() {
        if ancestor.kind() == SyntaxKind::CALL_EXPR {
            for token in ancestor.descendants_with_tokens() {
                if let syntax::NodeOrToken::Token(t) = token {
                    if t.kind() == SyntaxKind::IDENT && is_template_call(t.text()) {
                        let arg_list_start = ancestor
                            .descendants()
                            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
                            .map(|n| n.text_range().start());
                        if let Some(al_start) = arg_list_start {
                            if t.text_range().start() < al_start {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct NstrConfig {
    /// Ordered so that language lists rendered into diagnostic messages are
    /// byte-stable run to run (SARIF baselines are compared with plain `cmp`).
    pub declared_languages: BTreeSet<String>,
}

impl NstrConfig {
    pub fn from_context(ctx: &AnalysisContext, code: DiagnosticCode) -> Self {
        let declared_str =
            ctx.config.get_string(code, "declaredLanguages").unwrap_or(DEFAULT_DECLARED_LANGUAGES);

        let declared_languages: BTreeSet<String> = declared_str
            .split(',')
            .map(|s| s.trim().fold_lower())
            .filter(|s| !s.is_empty())
            .collect();

        Self { declared_languages }
    }
}

pub fn extract_language_keys(text: &str) -> HashSet<String> {
    let mut keys = HashSet::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i].is_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();

            while i < len && chars[i].is_whitespace() {
                i += 1;
            }

            if i < len && chars[i] == '=' {
                i += 1;
                while i < len && chars[i].is_whitespace() {
                    i += 1;
                }
                if i < len && (chars[i] == '\'' || chars[i] == '"') {
                    keys.insert(ident.fold_lower());
                }
            }
        } else {
            i += 1;
        }
    }

    keys
}

/// Language entries of an `НСтр` literal as `(lowercased key, value)`. The value is `None`
/// when the quoted text is never closed or its closing quote is not followed by `;` or the
/// end of the text (`ru='' на '` - the platform's reading of that is unclear). Unlike `extract_language_keys`, the scan
/// steps over a whole quoted value, so text inside a value is never read as a key.
pub fn extract_language_entries(text: &str) -> Vec<(String, Option<String>)> {
    let mut entries = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !(chars[i].is_alphabetic() || chars[i] == '_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();

        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || chars[i] != '=' {
            continue;
        }
        i += 1;
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || (chars[i] != '\'' && chars[i] != '"') {
            continue;
        }

        let quote = chars[i];
        i += 1;
        let mut value = String::new();
        let mut closed = false;
        while i < len {
            if chars[i] == quote {
                if i + 1 < len && chars[i + 1] == quote {
                    value.push(quote);
                    i += 2;
                    continue;
                }
                closed = true;
                i += 1;
                break;
            }
            value.push(chars[i]);
            i += 1;
        }
        let mut rest = i;
        while rest < len && chars[rest].is_whitespace() {
            rest += 1;
        }
        let clean_end = rest >= len || chars[rest] == ';';
        entries.push((ident.fold_lower(), (closed && clean_end).then_some(value)));
    }

    entries
}

pub fn get_assigned_variable_name(nstr_node: &SyntaxNode) -> Option<String> {
    let mut current = nstr_node.parent();
    while let Some(parent) = current {
        if parent.kind() == SyntaxKind::ASSIGN_STMT {
            let eq_pos = parent
                .children_with_tokens()
                .filter_map(|c| c.into_token())
                .find(|t| t.kind() == SyntaxKind::EQ)
                .map(|t| t.text_range().start());

            if let Some(eq_pos) = eq_pos {
                for child in parent.children() {
                    if child.kind() == SyntaxKind::IDENT && child.text_range().end() <= eq_pos {
                        for inner in child.children_with_tokens() {
                            if let syntax::NodeOrToken::Token(token) = inner {
                                if token.kind() == SyntaxKind::IDENT {
                                    return Some(token.text().to_string());
                                }
                            }
                        }
                    }
                }
            }
            return None;
        }
        if matches!(parent.kind(), SyntaxKind::FUNCTION_DEF | SyntaxKind::PROCEDURE_DEF) {
            break;
        }
        current = parent.parent();
    }
    None
}

pub fn is_variable_used_in_template(var_name: &str, nstr_node: &SyntaxNode) -> bool {
    let stmt_list = nstr_node.ancestors().find(|n| n.kind() == SyntaxKind::STMT_LIST);
    let stmt_list = match stmt_list {
        Some(s) => s,
        None => return false,
    };

    let nstr_offset = nstr_node.text_range().start();

    for node in stmt_list.descendants() {
        if node.kind() != SyntaxKind::CALL_EXPR {
            continue;
        }

        if node.text_range().start() <= nstr_offset {
            continue;
        }

        let arg_list_start = node
            .descendants()
            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
            .map(|n| n.text_range().start());

        let mut is_str_template = false;
        let mut has_var_in_args = false;

        for token in node.descendants_with_tokens() {
            if let syntax::NodeOrToken::Token(t) = token {
                if t.kind() == SyntaxKind::IDENT {
                    if let Some(al_start) = arg_list_start {
                        if t.text_range().start() < al_start && is_template_call(t.text()) {
                            is_str_template = true;
                        } else if t.text_range().start() >= al_start
                            && t.text().eq_ignore_ascii_case(var_name)
                        {
                            has_var_in_args = true;
                        }
                    }
                }
            }
        }

        if is_str_template && has_var_in_args {
            return true;
        }
    }

    false
}
