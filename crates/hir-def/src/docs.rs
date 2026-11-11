use self::reshape::{DescriptionUse, LineRole};
use crate::item_tree::ModItem;
use crate::{MethodId, VariableId};
use std::sync::Arc;
use stdx::case::CaseExt;
use syntax::{
    extract_leading_comment_lines_at_offset, extract_variable_comments_at_offset, CommentRun,
    Parse, SyntaxKind, SyntaxNode,
};

mod fields;
mod reshape;
mod tokens;
mod type_expr;

pub use reshape::doc_lines_reshaped_by_replacement;
pub use tokens::{doc_comment_tokens, DocCommentToken, DocCommentTokenKind};
pub use type_expr::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDocs {
    pub raw: String,

    pub purpose: Option<String>,

    pub parameters: Vec<ParameterDoc>,

    pub returned_value: Vec<TypeDoc>,

    pub examples: Vec<String>,

    pub call_options: Vec<String>,

    pub deprecation: Option<String>,

    pub link: Option<String>,
}

impl MethodDocs {
    pub fn empty() -> Self {
        Self {
            raw: String::new(),
            purpose: None,
            parameters: Vec::new(),
            returned_value: Vec::new(),
            examples: Vec::new(),
            call_options: Vec::new(),
            deprecation: None,
            link: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.purpose.is_none()
            && self.parameters.is_empty()
            && self.returned_value.is_empty()
            && self.examples.is_empty()
            && self.call_options.is_empty()
            && self.deprecation.is_none()
            && self.link.is_none()
    }

    pub fn is_hyperlink(&self) -> bool {
        self.link.is_some()
    }

    /// A `См.` / `See` cross-reference in the method's free-text description (purpose).
    /// bsl-ls treats such a reference as documentation of the whole method and does not
    /// demand a parameter section. References inside parameter lines are excluded, since
    /// those describe individual parameters rather than the method as a whole.
    pub fn has_see_reference(&self) -> bool {
        self.purpose.as_deref().is_some_and(contains_see_reference)
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecation.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterDoc {
    pub name: String,

    pub types: Vec<TypeDoc>,
}

impl ParameterDoc {
    pub fn new(name: String, types: Vec<TypeDoc>) -> Self {
        Self { name, types }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDoc {
    pub name: String,

    pub description: Option<String>,

    pub parameters: Vec<ParameterDoc>,

    pub is_hyperlink: bool,

    /// Items `* "ключ" - Тип` written directly under the slot: keys of a documented map.
    pub map_entries: Vec<MapEntryDoc>,
}

/// One quoted key item of a documented `Соответствие`: the unquoted key as written and its types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntryDoc {
    pub key: String,

    pub types: Vec<TypeDoc>,
}

impl TypeDoc {
    pub fn simple(name: String, description: Option<String>) -> Self {
        Self {
            name,
            description,
            parameters: Vec::new(),
            is_hyperlink: false,
            map_entries: Vec::new(),
        }
    }

    pub fn structured(
        name: String,
        description: Option<String>,
        parameters: Vec<ParameterDoc>,
    ) -> Self {
        Self { name, description, parameters, is_hyperlink: false, map_entries: Vec::new() }
    }

    pub fn hyperlink(link: String) -> Self {
        Self {
            name: link,
            description: None,
            parameters: Vec::new(),
            is_hyperlink: true,
            map_entries: Vec::new(),
        }
    }

    /// Separates the full documented type from prose without changing the legacy representation
    /// used by type inference, where a collection's element can be stored in `description`.
    pub fn type_label_and_description(&self) -> (String, Option<&str>) {
        let description = self.description.as_deref().filter(|text| !text.trim().is_empty());
        let Some(text) = description else {
            return (self.name.clone(), None);
        };
        if !is_collection_type_name(&self.name) {
            return (self.name.clone(), description);
        }
        let (element, prose) = split_type_description(text)
            .map_or((text, None), |(element, prose)| (element, Some(prose)));
        let label = format!("{} {}", self.name, element.trim());
        if parse_collection_type(&label).is_some() {
            (label, prose.filter(|text| !text.is_empty()))
        } else {
            (self.name.clone(), description)
        }
    }
}

fn is_collection_type_name(name: &str) -> bool {
    matches!(
        name.fold_lower().as_str(),
        "массив"
            | "фиксированныймассив"
            | "соответствие"
            | "фиксированноесоответствие"
            | "структура"
            | "фиксированнаяструктура"
            | "таблицазначений"
            | "списокзначений"
            | "деревозначений"
            | "array"
            | "fixedarray"
            | "map"
            | "fixedmap"
            | "structure"
            | "fixedstructure"
            | "valuetable"
            | "valuelist"
            | "valuetree"
    )
}

/// `comment_runs` are the comment runs of the module tree `file_text` was
/// parsed into, computed once per module.
pub fn compute_method_docs(
    comment_runs: &[CommentRun],
    tree: &crate::item_tree::ItemTree,
    method_id: MethodId,
    file_text: &str,
) -> Option<Arc<MethodDocs>> {
    let source_range = tree.method(method_id.local_id)?.source_range();

    let offset: usize = source_range.start().into();
    let comments = extract_leading_comment_lines_at_offset(offset, file_text, comment_runs)?;

    let docs = parse_method_docs(&comments)?;

    Some(Arc::new(docs))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDocs {
    pub raw: Vec<String>,
    pub purpose: Option<String>,
    pub types: Vec<TypeDoc>,
    pub deprecation: Option<String>,
    pub link: Option<String>,
}

impl VariableDocs {
    pub fn is_hyperlink(&self) -> bool {
        self.link.is_some()
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecation.is_some()
    }
}

pub fn compute_variable_docs(
    parse: &Parse<SyntaxNode>,
    tree: &crate::item_tree::ItemTree,
    variable_id: VariableId,
    file_text: &str,
    comment_runs: &[CommentRun],
) -> Option<Arc<VariableDocs>> {
    let items = tree.top_level_items();
    let item = items.get(variable_id.local_id as usize)?;
    let var_idx = match item {
        ModItem::Variable(idx) => *idx,
        _ => return None,
    };
    let variable = tree.variable(var_idx);

    let root = parse.syntax_node();
    let var_node = root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::VAR_DEF && n.text_range() == variable.source_range)?;

    compute_variable_docs_with_node(&var_node, variable, file_text, comment_runs)
}

pub fn compute_variable_docs_with_node(
    var_node: &SyntaxNode,
    variable: &crate::item_tree::Variable,
    file_text: &str,
    comment_runs: &[CommentRun],
) -> Option<Arc<VariableDocs>> {
    debug_assert_eq!(
        var_node.kind(),
        SyntaxKind::VAR_DEF,
        "compute_variable_docs_with_node expects a VAR_DEF node",
    );
    debug_assert_eq!(
        var_node.text_range(),
        variable.source_range,
        "var_node range must match Variable::source_range",
    );

    let var_keyword_offset: usize = var_node
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::KW_VAR)
        .map(|t| t.text_range().start().into())?;

    let var_end_offset: usize = variable.source_range.end().into();
    let first_annotation_offset: Option<usize> =
        variable.annotations.first().map(|ann| ann.range.start().into());

    let comments = extract_variable_comments_at_offset(
        file_text,
        var_keyword_offset,
        var_end_offset,
        first_annotation_offset,
        comment_runs,
    )?;

    parse_variable_docs(&comments).map(Arc::new)
}

pub fn parse_variable_docs(comments: &[String]) -> Option<VariableDocs> {
    if comments.is_empty() {
        return None;
    }

    let raw: Vec<String> = comments.to_vec();

    if let Some(first_non_empty) = comments.iter().find(|c| !c.trim().is_empty()) {
        let trimmed = first_non_empty.trim();
        if is_hyperlink_line(trimmed) {
            return Some(VariableDocs {
                raw,
                purpose: None,
                types: Vec::new(),
                deprecation: None,
                link: Some(trimmed.to_string()),
            });
        }
    }
    let has_deprecation_marker =
        comments.iter().any(|line| is_deprecated_keyword(&line.trim().fold_lower()));
    if !has_deprecation_marker {
        if let Some(last_non_empty) = comments.iter().rev().find(|c| !c.trim().is_empty()) {
            let trimmed = last_non_empty.trim();
            if is_hyperlink_line(trimmed) {
                return Some(VariableDocs {
                    raw,
                    purpose: None,
                    types: Vec::new(),
                    deprecation: None,
                    link: Some(trimmed.to_string()),
                });
            }
        }
    }

    let deprecated_idx =
        comments.iter().position(|line| is_deprecated_keyword(&line.trim().fold_lower()));

    let (purpose_lines, deprecated_slice): (&[String], &[String]) = match deprecated_idx {
        Some(idx) => (&comments[..idx], &comments[idx..]),
        None => (comments, &[]),
    };

    let purpose_collected: Vec<&str> =
        purpose_lines.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let purpose =
        if purpose_collected.is_empty() { None } else { Some(purpose_collected.join("\n")) };

    let deprecation = match deprecated_slice.first() {
        Some(keyword_line) => parse_deprecated_section(keyword_line, &deprecated_slice[1..]),
        None => None,
    };

    Some(VariableDocs { raw, purpose, types: Vec::new(), deprecation, link: None })
}

fn parse_method_docs(comments: &[String]) -> Option<MethodDocs> {
    if comments.is_empty() {
        return None;
    }

    let raw = comments.join("\n");

    let mut docs = MethodDocs {
        raw,
        purpose: None,
        parameters: Vec::new(),
        returned_value: Vec::new(),
        examples: Vec::new(),
        call_options: Vec::new(),
        deprecation: None,
        link: None,
    };

    if let Some(first_non_empty) = comments.iter().find(|c| !c.trim().is_empty()) {
        if is_hyperlink_line(first_non_empty.trim()) {
            docs.link = Some(first_non_empty.trim().to_string());
            return Some(docs);
        }
    }
    if let Some(last_non_empty) = comments.iter().rev().find(|c| !c.trim().is_empty()) {
        if is_hyperlink_line(last_non_empty.trim()) && !has_structural_section(comments) {
            docs.link = Some(last_non_empty.trim().to_string());
            return Some(docs);
        }
    }

    let mut section_indices = Vec::new();
    let mut in_parameters = false;
    for (i, line) in comments.iter().enumerate() {
        let trimmed = line.trim();
        if let Some((section, payload)) = section_header(trimmed, in_parameters) {
            in_parameters = section == Section::Parameters;
            section_indices.push(SectionMarker::new(i, section, payload));
        }
    }

    let purpose_end = section_indices.first().map(|marker| marker.index).unwrap_or(comments.len());
    if purpose_end > 0 {
        let purpose_lines: Vec<_> =
            comments[..purpose_end].iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

        if !purpose_lines.is_empty() {
            docs.purpose = Some(purpose_lines.join("\n"));
        }
    }

    for (idx, marker) in section_indices.iter().enumerate() {
        let end = section_indices
            .get(idx + 1)
            .map(|next_marker| next_marker.index)
            .unwrap_or(comments.len());

        let mut section_lines = Vec::new();
        if marker.section == Section::Returns {
            if let Some(payload) = &marker.inline_payload {
                section_lines.push(payload.clone());
            }
        }
        section_lines.extend(comments[marker.index + 1..end].iter().cloned());

        match marker.section {
            Section::Parameters => {
                docs.parameters = parse_parameters(&section_lines);
            }
            Section::Returns => {
                docs.returned_value = parse_returns(&section_lines);
            }
            Section::Examples => {
                docs.examples = parse_code_section(&section_lines);
            }
            Section::CallOptions => {
                docs.call_options = parse_code_section(&section_lines);
            }
            Section::Deprecated => {
                docs.deprecation =
                    parse_deprecated_section(&comments[marker.index], &section_lines);
            }
        }
    }

    Some(docs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Parameters,
    Returns,
    Examples,
    CallOptions,
    Deprecated,
}

/// Keeps structural documentation and its source tokens on the same section rules.
fn section_header(line: &str, in_parameters: bool) -> Option<(Section, Option<String>)> {
    let lower = line.fold_lower();
    let returns_header = returns_section_header(line);
    if is_parameters_keyword(&lower) {
        Some((Section::Parameters, None))
    } else if returns_header != ReturnsHeader::NotReturns
        && !is_parameter_named_like_returns(in_parameters, &lower, line)
    {
        let payload = match returns_header {
            ReturnsHeader::WithPayload(payload) => Some(payload),
            _ => None,
        };
        Some((Section::Returns, payload))
    } else if is_example_keyword(&lower) {
        Some((Section::Examples, None))
    } else if is_call_options_keyword(&lower) {
        Some((Section::CallOptions, None))
    } else if is_deprecated_keyword(&lower) {
        Some((Section::Deprecated, None))
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionMarker {
    index: usize,
    section: Section,
    inline_payload: Option<String>,
}

impl SectionMarker {
    fn new(index: usize, section: Section, inline_payload: Option<String>) -> Self {
        Self { index, section, inline_payload }
    }
}

fn is_parameters_keyword(lower_line: &str) -> bool {
    lower_line.starts_with("параметры:") || lower_line.starts_with("parameters:")
}

fn has_structural_section(comments: &[String]) -> bool {
    comments.iter().any(|line| is_structural_section_line(line.trim()))
}

fn is_structural_section_line(line: &str) -> bool {
    let lower = line.fold_lower();
    is_parameters_keyword(&lower)
        || returns_section_header(line) != ReturnsHeader::NotReturns
        || is_example_keyword(&lower)
        || is_call_options_keyword(&lower)
        || is_deprecated_keyword(&lower)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReturnsHeader {
    NotReturns,
    NoPayload,
    WithPayload(String),
}

fn returns_section_header(line: &str) -> ReturnsHeader {
    let trimmed = line.trim();
    let lower = trimmed.fold_lower();

    for keyword in ["возвращаемое значение", "return value", "returns", "результат", "result"]
    {
        if !lower.starts_with(keyword) {
            continue;
        }
        let header = match parse_section_payload_after_keyword(trimmed, keyword.len()) {
            Some(header) => header,
            None => return ReturnsHeader::NotReturns,
        };
        if let ReturnsHeader::WithPayload(text) = &header {
            if is_ambiguous_returns_keyword(keyword) && !payload_looks_like_type_section(text) {
                return ReturnsHeader::NotReturns;
            }
        }
        return header;
    }

    ReturnsHeader::NotReturns
}

fn is_ambiguous_returns_keyword(keyword: &str) -> bool {
    matches!(keyword, "результат" | "result")
}

/// Inside the parameters section, a line whose name is the ambiguous returns keyword
/// `Результат`/`Result` but which is shaped like a parameter entry (`Результат - Тип -
/// описание`) is a parameter, not the start of a Returns section. Without this guard such
/// a line would truncate the parameter list and make the parameters after it look
/// undocumented. A real returns header (`Результат:`) is not a parameter line and is
/// unaffected.
fn is_parameter_named_like_returns(in_parameters: bool, lower: &str, line: &str) -> bool {
    in_parameters
        && (lower.starts_with("результат") || lower.starts_with("result"))
        && parse_parameter_line(line).is_some()
}

fn payload_looks_like_type_section(payload: &str) -> bool {
    let stripped = payload.trim_end_matches(['.', ',', ';', ':', '!', '?']).trim();
    if stripped.is_empty() {
        return false;
    }

    let type_part = [" -- ", " — ", " – ", " - "]
        .iter()
        .find_map(|sep| stripped.find(*sep).map(|pos| stripped[..pos].trim()))
        .unwrap_or(stripped);

    is_likely_type_name(type_part)
}

fn parse_section_payload_after_keyword(line: &str, keyword_len: usize) -> Option<ReturnsHeader> {
    let mut rest = line[keyword_len..].trim_start();

    if rest.starts_with('(') {
        let closing_paren = rest.find(')')?;
        rest = rest[closing_paren + 1..].trim_start();
    }

    if rest.is_empty() {
        return Some(ReturnsHeader::NoPayload);
    }

    if let Some(payload) = rest.strip_prefix(':') {
        return Some(returns_header_from_payload(payload));
    }

    if let Some(payload) = rest.strip_prefix('-') {
        return Some(returns_header_from_payload(payload.trim_start_matches('-')));
    }

    if let Some(payload) = rest.strip_prefix(';') {
        return Some(returns_header_from_payload(payload));
    }

    None
}

fn returns_header_from_payload(payload: &str) -> ReturnsHeader {
    let payload = payload.trim();
    if payload.is_empty() {
        ReturnsHeader::NoPayload
    } else {
        ReturnsHeader::WithPayload(payload.to_string())
    }
}

fn is_example_keyword(lower_line: &str) -> bool {
    // Anchored at the line start: a section header is a standalone line. Using `contains`
    // would misread the common Russian word `Например:` ("for example"), which contains
    // `пример:`, as an Examples header and truncate the preceding section.
    let line = lower_line.trim_start();
    line.starts_with("пример:")
        || line.starts_with("примеры:")
        || line.starts_with("example:")
        || line.starts_with("examples:")
}

fn is_call_options_keyword(lower_line: &str) -> bool {
    let line = lower_line.trim_start();
    line.starts_with("варианты вызова:") || line.starts_with("call options:")
}

fn is_deprecated_keyword(lower_line: &str) -> bool {
    lower_line.contains("устарела") || lower_line.contains("deprecated")
}

fn is_hyperlink_line(line: &str) -> bool {
    let lower = line.fold_lower();
    lower.starts_with("см.") || lower.starts_with("see ")
}

/// Whether the text contains a `См.` / `See` cross-reference (not only at the start of a
/// line), e.g. `Продолжение процедуры (см. выше)`. Mirrors bsl-ls, which parses such
/// references as documentation links. Delegates to the same scanner that collects link
/// targets, so detection and collection cannot drift apart: a reference counts only when
/// a plausible target follows the marker.
fn contains_see_reference(text: &str) -> bool {
    let mut targets = Vec::new();
    for line in text.lines() {
        append_see_targets(line, &mut targets);
        if !targets.is_empty() {
            return true;
        }
    }
    false
}

/// Collect `см.` / `see` reference targets from a single line, applying the
/// word-boundary guards that make the marker a real cross-reference: a
/// Russian `см.` counts anywhere except after a letter (compound word) or a
/// number (the unit abbreviation `5 см.`), and it must be followed by a
/// plausible target; the English `see` only at line start, so the common
/// verb mid-sentence is not matched.
fn append_see_targets(line: &str, out: &mut Vec<String>) {
    out.extend(see_reference_ranges(line).into_iter().map(|(_, target)| line[target].to_string()));
}

/// Byte ranges keep link detection and editor highlighting on the same boundaries.
fn see_reference_ranges(line: &str) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let offsets: Vec<_> = line.char_indices().map(|(i, _)| i).chain([line.len()]).collect();
    let chars: Vec<char> = line.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        let is_marker = i + 2 < chars.len()
            && chars[i].to_lowercase().eq(['с'])
            && chars[i + 1].to_lowercase().eq(['м'])
            && chars[i + 2] == '.';
        if !is_marker {
            i += 1;
            continue;
        }

        let preceded_by_letter = i > 0 && chars[i - 1].is_alphabetic();
        let preceded_by_number =
            chars[..i].iter().rev().find(|c| !c.is_whitespace()).is_some_and(|c| c.is_numeric());
        if preceded_by_letter || preceded_by_number {
            i += 1;
            continue;
        }

        let mut j = i + 3;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if !chars.get(j).is_some_and(|c| c.is_alphabetic()) {
            i += 1;
            continue;
        }

        let start = j;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.')
        {
            j += 1;
        }
        let target: String = chars[start..j].iter().collect();
        let target = target.trim_end_matches('.');
        if !target.is_empty() {
            out.push((offsets[i]..offsets[i + 3], offsets[start]..offsets[start] + target.len()));
        }
        i = j;
    }

    let trimmed = line.trim_start();
    if trimmed.is_char_boundary(4) && trimmed[..4].eq_ignore_ascii_case("see ") {
        let target: String = trimmed[4..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        let target = target.trim_end_matches('.');
        if target.chars().next().is_some_and(char::is_alphabetic) {
            let marker_start = line.len() - trimmed.len();
            let target_start = line.len() - trimmed[4..].trim_start().len();
            out.push((marker_start..marker_start + 3, target_start..target_start + target.len()));
        }
    }
    out
}

fn parse_parameters(lines: &[String]) -> Vec<ParameterDoc> {
    parse_parameters_with_roles(lines, None)
}

/// The parser itself reports each line's role, so the roles cannot drift from the parse.
fn parse_parameters_with_roles(
    lines: &[String],
    mut roles: Option<&mut [LineRole]>,
) -> Vec<ParameterDoc> {
    let mut parameters = Vec::new();
    let mut current_param: Option<(String, Vec<TypeDoc>)> = None;

    let mut cursor = 0;
    while let Some(line) = lines.get(cursor) {
        let trimmed = line.trim();
        cursor += 1;

        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('*') {
            cursor -= 1;
            let (fields, entries) = fields::parse_fields(lines, &mut cursor, roles.as_deref_mut());
            if let Some((_, types)) = &mut current_param {
                if let Some(last_type) = types.last_mut() {
                    last_type.parameters.extend(fields);
                    last_type.map_entries.extend(entries);
                }
            }
            continue;
        }

        if current_param.is_some() && trimmed.starts_with('-') {
            if let Some((type_name, description)) = parse_type_line(trimmed) {
                if is_likely_type_name(&type_name) {
                    if let Some((_, types)) = &mut current_param {
                        types.push(TypeDoc::simple(type_name, description));
                    }
                    mark_role(&mut roles, cursor - 1, LineRole::Type(0));
                    continue;
                }
            }
        }

        // A multi-line union type may continue on the next line without a leading
        // `-`, e.g. `Письмо - Спр.Входящее,` then `Спр.Исходящее - текст`. A dotted
        // type reference is never a valid parameter name, so attach it to the current
        // parameter instead of registering a phantom parameter "not in the signature".
        if current_param.is_some() {
            if let Some((type_name, description)) = parse_type_line(trimmed) {
                if is_dotted_type_reference(&type_name) {
                    if let Some((_, types)) = &mut current_param {
                        types.push(TypeDoc::simple(type_name, description));
                    }
                    mark_role(&mut roles, cursor - 1, LineRole::Type(0));
                    continue;
                }
            }
        }

        if let Some((param_name, types)) = parse_parameter_line(trimmed) {
            if let Some((name, types)) = current_param.take() {
                parameters.push(ParameterDoc { name, types });
            }
            current_param = Some((param_name, types));
            mark_role(&mut roles, cursor - 1, LineRole::Parameter);
            continue;
        }

        let mut description = DescriptionUse::Prose;
        if let Some((_, types)) = &mut current_param {
            append_type_description(types, trimmed);
            description = DescriptionUse::of(types.last());
        }
        mark_role(&mut roles, cursor - 1, LineRole::Text { level: 0, description });
    }

    if let Some((name, types)) = current_param {
        parameters.push(ParameterDoc { name, types });
    }

    parameters
}

fn mark_role(roles: &mut Option<&mut [LineRole]>, line: usize, role: LineRole) {
    if let Some(roles) = roles {
        roles[line] = role;
    }
}

fn parse_parameter_line(line: &str) -> Option<(String, Vec<TypeDoc>)> {
    // Tabs are frequently used to align the separator (`Имя\t\t- Тип`); the split
    // below only recognizes a space-flanked dash, so normalize tabs to spaces first.
    let normalized = line.replace('\t', " ");
    let (param_name, types) = normalized.split_once(" - ")?;

    let param_name = param_name.trim().to_string();
    if !is_likely_parameter_doc_name(&param_name) {
        return None;
    }

    Some((param_name, parse_parameter_types(types)))
}

/// The `Тип1, Тип2 - описание` part of a parameter or field line, after the name's separator.
fn parse_parameter_types(text: &str) -> Vec<TypeDoc> {
    let normalized = text.replace('\t', " ");
    let (type_part, description) = match normalized.split_once(" - ") {
        Some((types, description)) => (types.trim(), Some(description.trim().to_string())),
        None => (normalized.trim(), None),
    };

    if type_part.contains(',') {
        type_part
            .split(',')
            .filter(|t| !t.trim().is_empty())
            .map(|t| TypeDoc::simple(t.trim().to_string(), description.clone()))
            .collect()
    } else {
        vec![TypeDoc::simple(type_part.to_string(), description)]
    }
}

fn is_likely_parameter_doc_name(name: &str) -> bool {
    is_likely_parameter_name(name) || is_dotted_type_reference(name)
}

fn is_likely_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first_char) = chars.next() else {
        return false;
    };

    if !(first_char.is_alphabetic() || first_char == '_') {
        return false;
    }

    chars.all(|c| c.is_alphanumeric() || c == '_')
}

pub fn is_dotted_type_reference(name: &str) -> bool {
    name.contains('.') && is_likely_type_name(name)
}

fn parse_returns(lines: &[String]) -> Vec<TypeDoc> {
    let mut types = Vec::new();
    let mut current_type: Option<TypeDoc> = None;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A whole-slot reference is a link, but a comma union that happens to START with
        // `см.` is a list of alternatives whose first member is a reference: the union parser
        // must see it, or the slot is lost to the link branch (`см. База.Создать, Неопределено`
        // must stay two alternatives, one of them the target).
        if is_hyperlink_line(trimmed) && parse_return_type_union(trimmed).is_none() {
            return vec![TypeDoc::hyperlink(trimmed.to_string())];
        }
        break;
    }

    let mut cursor = 0;
    while let Some(line) = lines.get(cursor) {
        let trimmed = line.trim();
        cursor += 1;

        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('*') {
            cursor -= 1;
            let (fields, entries) = fields::parse_fields(lines, &mut cursor, None);
            if let Some(ref mut type_doc) = current_type {
                type_doc.parameters.extend(fields);
                type_doc.map_entries.extend(entries);
            }
            continue;
        }

        if let Some(mut union) = parse_return_type_union(trimmed) {
            if let Some(type_doc) = current_type.take() {
                types.push(type_doc);
            }
            // Keep the last union member open so `*` sub-fields attach to it
            // (the structure/collection type in a union is conventionally last).
            let last = union.pop();
            types.extend(union);
            current_type = last;
            continue;
        }

        if let Some((type_name, description)) = parse_type_line(trimmed) {
            if let Some(type_doc) = current_type.take() {
                types.push(type_doc);
            }
            current_type = Some(TypeDoc::simple(type_name, description));
        } else if current_type.is_none() && types.is_empty() {
            let stripped = trimmed.trim_end_matches(['.', ',', ';', ':', '!', '?']).trim();
            if !stripped.is_empty() && is_likely_type_name(stripped) {
                current_type = Some(TypeDoc::simple(stripped.to_string(), None));
            }
        } else if let Some(type_doc) = &mut current_type {
            append_description(&mut type_doc.description, trimmed);
        }
    }

    if let Some(type_doc) = current_type {
        types.push(type_doc);
    }

    types
}

/// Wrapped source lines extend the last declared alternative instead of disappearing from hover.
fn append_type_description(types: &mut [TypeDoc], continuation: &str) {
    if let Some(last) = types.last_mut() {
        append_description(&mut last.description, continuation);
    }
}

fn append_description(description: &mut Option<String>, continuation: &str) {
    let continuation = continuation.trim().trim_start_matches('-').trim();
    if continuation.is_empty() {
        return;
    }
    let description = description.get_or_insert_with(String::new);
    if !description.is_empty() {
        description.push('\n');
    }
    description.push_str(continuation);
}

fn parse_type_line(line: &str) -> Option<(String, Option<String>)> {
    // Mirror parse_parameter_line: tabs frequently flank the `-` separator on type
    // (continuation) lines; normalize them so the separator is recognized.
    let normalized = line.replace('\t', " ");
    let trimmed = normalized.trim();

    if trimmed.starts_with('*') {
        return None;
    }

    let type_line = trimmed.strip_prefix('-').map(str::trim).unwrap_or(trimmed);
    if type_line.is_empty() {
        return None;
    }

    if let Some((type_part, description)) = split_type_description(type_line) {
        let (type_name, type_description) = parse_return_type_name(type_part)?;
        let explicit = (!description.is_empty()).then(|| description.to_string());
        return Some((type_name, merge_type_descriptions(type_description, explicit)));
    }

    parse_return_type_name(type_line)
}

/// Parse a return-type line declaring a comma-separated union
/// (`Тип1, Тип2 - описание`, `СправочникСсылка.X, Неопределено`). Mirrors the
/// parameter side's union split so a documented union return value is not seen
/// as an empty Returns section. Returns one `TypeDoc` per member sharing the
/// line's description, or `None` when the line is not a comma union of plausible
/// types (so prose like `Строка, содержащая имя` is rejected, not misparsed).
fn parse_return_type_union(line: &str) -> Option<Vec<TypeDoc>> {
    let normalized = line.replace('\t', " ");
    let trimmed = normalized.trim();

    if trimmed.starts_with('*') {
        return None;
    }

    let type_line = trimmed.strip_prefix('-').map(str::trim).unwrap_or(trimmed);

    let (type_part, description) = match split_type_description(type_line) {
        Some((type_part, description)) => {
            (type_part, (!description.is_empty()).then(|| description.to_string()))
        }
        None => (type_line, None),
    };

    if !type_part.contains(',') {
        return None;
    }

    let mut members = Vec::new();
    for member in type_part.split(',') {
        let member = member.trim();
        // A leading/trailing/double comma yields an empty member: the union is
        // malformed, so reject the whole line rather than silently accepting a
        // partial type list that would suppress the diagnostic.
        if member.is_empty() {
            return None;
        }
        let (type_name, member_description) =
            parse_return_type_name(member).or_else(|| see_member(member))?;
        let description = merge_type_descriptions(member_description, description.clone());
        members.push(TypeDoc::simple(type_name, description));
    }

    if members.len() < 2 {
        return None;
    }

    Some(members)
}

fn split_type_description(line: &str) -> Option<(&str, &str)> {
    for separator in [" -- ", " — ", " – ", " - "] {
        if let Some(separator_pos) = line.find(separator) {
            return Some((
                line[..separator_pos].trim(),
                line[separator_pos + separator.len()..].trim(),
            ));
        }
    }

    // Tolerate a dash separator missing one of its surrounding spaces: a
    // dangling `Тип -` (description omitted), `Тип -описание` (no space after)
    // or `Тип- описание` (no space before). Require whitespace on at least one
    // side so a hyphenated compound name (`COM-класс`) is NOT split. Pick the
    // earliest such dash so the type, which always comes first, stays intact.
    let space_dash = line.find(" -").map(|pos| pos + 1);
    let dash_space = line.find("- ");
    let dash_pos = match (space_dash, dash_space) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if let Some(dash_pos) = dash_pos {
        let type_part = line[..dash_pos].trim();
        if !type_part.is_empty() {
            return Some((type_part, line[dash_pos + 1..].trim()));
        }
    }

    None
}

fn parse_return_type_name(type_part: &str) -> Option<(String, Option<String>)> {
    let type_part = type_part.trim().trim_end_matches(':').trim();

    if type_part.is_empty() {
        return None;
    }

    if let Some((collection_type, description)) = parse_collection_type(type_part) {
        return Some((collection_type, Some(description)));
    }

    if is_likely_type_name(type_part) {
        return Some((type_part.to_string(), None));
    }

    None
}

/// A `см. Модуль.Метод` member of a union is an alternative of its own: the parameter side
/// keeps it (its slot splits by the same comma), and the return side must not lose the whole
/// slot because of it.
fn see_member(member: &str) -> Option<(String, Option<String>)> {
    type_expr::see_reference_of(member).map(|_| (member.to_string(), None))
}

fn parse_collection_type(type_part: &str) -> Option<(String, String)> {
    for (marker, normalized_prefix) in [(" из ", "из"), (" of ", "of")] {
        let Some(at) = stdx::case::find_ignore_case(type_part, marker) else {
            continue;
        };
        let collection_type = type_part[..at.start].trim();
        let element_type = type_part[at.end..].trim();

        if collection_type.is_empty() || element_type.is_empty() {
            return None;
        }

        if is_likely_type_name(collection_type) {
            return Some((
                collection_type.to_string(),
                format!("{normalized_prefix} {element_type}"),
            ));
        }
    }

    None
}

fn merge_type_descriptions(
    type_description: Option<String>,
    explicit_description: Option<String>,
) -> Option<String> {
    match (type_description, explicit_description) {
        (Some(type_description), Some(explicit_description))
            if !explicit_description.trim().is_empty() =>
        {
            Some(format!("{type_description} - {explicit_description}"))
        }
        (Some(type_description), _) => Some(type_description),
        (None, Some(explicit_description)) => Some(explicit_description),
        (None, None) => None,
    }
}

fn is_likely_type_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    if !s.chars().all(is_type_name_char) {
        return false;
    }

    let first_char = s.chars().next().unwrap();
    if first_char.is_uppercase() {
        return true;
    }

    let lower = s.fold_lower();
    matches!(
        lower.as_str(),
        "строка"
            | "string"
            | "число"
            | "number"
            | "булево"
            | "boolean"
            | "дата"
            | "date"
            | "неопределено"
            | "undefined"
            | "null"
            | "произвольный"
            | "arbitrary"
            | "структура"
            | "structure"
            | "массив"
            | "array"
            | "соответствие"
            | "map"
    )
}

fn is_type_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

fn parse_simple_section(lines: &[String]) -> Vec<String> {
    lines.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
}

/// Only the common comment margin is layout; indentation within example code is meaningful.
fn parse_code_section(lines: &[String]) -> Vec<String> {
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return Vec::new();
    };
    let last = lines.iter().rposition(|line| !line.trim().is_empty()).unwrap();
    let lines = &lines[first..=last];
    let margin = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| fields::indentation(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                // Expand only the leading tabs, so tab stops remain aligned after dedenting.
                format!("{}{}", " ".repeat(fields::indentation(line) - margin), line.trim())
            }
        })
        .collect()
}

fn parse_deprecated_section(keyword_line: &str, following_lines: &[String]) -> Option<String> {
    let after_keyword = ["устарела", "deprecated"]
        .iter()
        .find_map(|keyword| stdx::case::find_ignore_case(keyword_line, keyword))
        .map_or("", |at| &keyword_line[at.end..]);

    let info_on_same_line = after_keyword
        .trim_start_matches(|c: char| c == '.' || c == ':' || c.is_whitespace())
        .trim();

    if !info_on_same_line.is_empty() {
        return Some(info_on_same_line.to_string());
    }

    let following_info = parse_simple_section(following_lines);
    if !following_info.is_empty() {
        return Some(following_info.join("\n"));
    }

    Some(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{item_tree::ItemTree, ModuleId};
    use vfs::FileId;

    /// Presentation uses the same bilingual collection syntax as return-type parsing.
    #[test]
    fn collection_labels_keep_element_types_separate_from_prose() {
        for declaration in [
            "ФиксированныйМассив из Строка",
            "Array из Строка",
            "Соответствие of KeyAndValue",
            "FixedMap of KeyAndValue",
            "ТаблицаЗначений из СтрокаТаблицыЗначений",
        ] {
            let comments = ["Returns:".to_owned(), format!("{declaration} - values")];
            let docs = parse_method_docs(&comments).unwrap();
            assert_eq!(
                docs.returned_value[0].type_label_and_description(),
                (declaration.to_owned(), Some("values"))
            );
        }
        let prose = TypeDoc::simple("String".into(), Some("of the object".into()));
        assert_eq!(prose.type_label_and_description(), ("String".into(), Some("of the object")));
        let bare = TypeDoc::simple("Array".into(), Some("of String".into()));
        assert_eq!(bare.type_label_and_description(), ("Array of String".into(), None));
    }

    #[test]
    fn test_method_docs_empty() {
        let docs = MethodDocs::empty();
        assert!(docs.is_empty());
        assert!(!docs.is_hyperlink());
        assert!(!docs.is_deprecated());
    }

    #[test]
    fn test_type_doc_simple() {
        let type_doc = TypeDoc::simple("Строка".to_string(), Some("описание".to_string()));
        assert_eq!(type_doc.name, "Строка");
        assert_eq!(type_doc.description, Some("описание".to_string()));
        assert!(type_doc.parameters.is_empty());
        assert!(!type_doc.is_hyperlink);
    }

    #[test]
    fn test_type_doc_structured() {
        let params = vec![ParameterDoc::new(
            "Поле".to_string(),
            vec![TypeDoc::simple("Число".to_string(), Some("значение".to_string()))],
        )];
        let type_doc = TypeDoc::structured("Структура".to_string(), None, params);
        assert_eq!(type_doc.name, "Структура");
        assert_eq!(type_doc.parameters.len(), 1);
        assert!(!type_doc.is_hyperlink);
    }

    #[test]
    fn test_type_doc_hyperlink() {
        let type_doc = TypeDoc::hyperlink("См. ДругойМетод()".to_string());
        assert!(type_doc.is_hyperlink);
        assert_eq!(type_doc.name, "См. ДругойМетод()");
    }

    #[test]
    fn variable_docs_fallback_uses_module_comment_runs() {
        let text = "// Назначение переменной.\nПерем Данные; // Дополнение.";
        let parse = parser::parse_with_shared_cache(text);
        let tree = ItemTree::from_parse(&parse);
        let comment_runs = syntax::comment_runs(&parse.syntax_node());

        let docs = compute_variable_docs(
            &parse,
            &tree,
            VariableId { module: ModuleId::new(FileId(0)), local_id: 0 },
            text,
            &comment_runs,
        )
        .expect("variable documentation");

        assert_eq!(docs.raw, ["Назначение переменной.", "Дополнение."]);
        assert_eq!(docs.purpose.as_deref(), Some("Назначение переменной.\nДополнение."));
    }

    #[test]
    fn test_parse_empty_comments() {
        let docs = parse_method_docs(&[]);
        assert!(docs.is_none());
    }

    #[test]
    fn test_parse_minimal_comments() {
        let comments = vec!["Описание метода".to_string()];
        let docs = parse_method_docs(&comments);
        assert!(docs.is_some());
        let docs = docs.unwrap();
        assert_eq!(docs.raw, "Описание метода");
        assert_eq!(docs.purpose, Some("Описание метода".to_string()));
    }

    #[test]
    fn test_parse_complete_documentation() {
        let comments = vec![
            "Вычисляет сумму двух чисел.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  А - Число - первое слагаемое".to_string(),
            "  Б - Число - второе слагаемое".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Число - результат сложения".to_string(),
            "".to_string(),
            "Пример:".to_string(),
            "  Результат = Сумма(2, 3); // Результат = 5".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.purpose, Some("Вычисляет сумму двух чисел.".to_string()));

        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.parameters[0].name, "А");
        assert_eq!(docs.parameters[0].types[0].name, "Число");
        assert_eq!(docs.parameters[0].types[0].description, Some("первое слагаемое".to_string()));
        assert_eq!(docs.parameters[1].name, "Б");
        assert_eq!(docs.parameters[1].types[0].name, "Число");

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Число");
        assert_eq!(docs.returned_value[0].description, Some("результат сложения".to_string()));

        assert_eq!(docs.examples.len(), 1);
        assert!(docs.examples[0].contains("Результат = Сумма(2, 3)"));
    }

    #[test]
    fn test_parse_structured_return_value() {
        let comments = vec![
            "Возвращает информацию о пользователе.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Структура:".to_string(),
            "    * Имя - Строка - имя пользователя".to_string(),
            "    * Возраст - Число - возраст пользователя".to_string(),
            "    * Email - Строка - адрес электронной почты".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Структура");

        let sub_params = &docs.returned_value[0].parameters;
        assert_eq!(sub_params.len(), 3);
        assert_eq!(sub_params[0].name, "Имя");
        assert_eq!(sub_params[0].types[0].name, "Строка");
        assert_eq!(sub_params[1].name, "Возраст");
        assert_eq!(sub_params[2].name, "Email");
    }

    #[test]
    fn test_parse_return_value_variants_header() {
        let comments = vec![
            "Возвращает дату начала периода.".to_string(),
            "".to_string(),
            "Возвращаемое значение (варианты):".to_string(),
            "  Дата - дата начала периода.".to_string(),
            "  Неопределено - если период не применим.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[0].name, "Дата");
        assert_eq!(docs.returned_value[1].name, "Неопределено");
    }

    #[test]
    fn test_parse_return_comma_union_dash_bullet() {
        let comments = vec![
            "Функция возвращает статус работы для нового клиента.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  - СправочникСсылка.CRM_СтатусыРаботыСКлиентом, Неопределено".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[0].name, "СправочникСсылка.CRM_СтатусыРаботыСКлиентом");
        assert_eq!(docs.returned_value[1].name, "Неопределено");
    }

    #[test]
    fn test_parse_return_comma_union_with_description() {
        let comments = vec![
            "Добавляет команду.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  СтрокаТаблицыЗначений, Неопределено - описание добавленной команды.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[0].name, "СтрокаТаблицыЗначений");
        assert_eq!(docs.returned_value[1].name, "Неопределено");
        assert_eq!(
            docs.returned_value[1].description,
            Some("описание добавленной команды.".to_string())
        );
    }

    #[test]
    fn test_parse_return_comma_union_no_space_with_fields() {
        let comments = vec![
            "Возвращает аудио данные.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Неопределено,Структура - Аудио данные:".to_string(),
            "   *ИмяФайла - Строка - Имя файла.".to_string(),
            "   *Размер - Число - Размер в байтах.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[0].name, "Неопределено");
        assert_eq!(docs.returned_value[1].name, "Структура");
        // `*` fields attach to the last union member (the structure).
        assert_eq!(docs.returned_value[1].parameters.len(), 2);
    }

    #[test]
    fn test_parse_return_comma_prose_is_not_a_union() {
        let comments = vec![
            "Возвращает имя.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Строка, содержащая имя пользователя".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        // Prose with a comma must not be split into phantom types; the line is
        // not a union of plausible type names.
        assert!(docs.returned_value.iter().all(|t| t.name != "содержащая имя пользователя"));
    }

    #[test]
    fn test_parse_return_dangling_dash_no_description() {
        let comments = vec![
            "Возвращает действие.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Строка -".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Строка");
        assert_eq!(docs.returned_value[0].description, None);
    }

    #[test]
    fn test_parse_return_union_with_dangling_dash() {
        let comments = vec![
            "Получить задание.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  ФоновоеЗадание, Неопределено -".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[0].name, "ФоновоеЗадание");
        assert_eq!(docs.returned_value[1].name, "Неопределено");
    }

    #[test]
    fn test_parse_return_dash_without_surrounding_spaces() {
        let comments = vec![
            "Версия пакета.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Строка -версия пакета.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Строка");
        assert_eq!(docs.returned_value[0].description, Some("версия пакета.".to_string()));
    }

    #[test]
    fn test_parse_return_dash_without_space_before() {
        let comments = vec![
            "Проверка.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Булево- результат проверки.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Булево");
        assert_eq!(docs.returned_value[0].description, Some("результат проверки.".to_string()));
    }

    #[test]
    fn test_parse_return_hyphenated_type_name_not_split() {
        let comments = vec![
            "Возвращает класс.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  COM-класс".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        // A glued hyphen (no surrounding space) is part of a compound name, not
        // a separator: it must not be split into a phantom "COM" type.
        assert!(docs.returned_value.iter().all(|t| t.name != "COM"));
    }

    #[test]
    fn test_parse_return_value_english_header() {
        let comments = vec![
            "Calculates total amount.".to_string(),
            "".to_string(),
            "Return value:".to_string(),
            "  Number - total amount.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Number");
    }

    #[test]
    fn test_parse_return_value_inline_dash_header() {
        let comments = vec![
            "Возвращает соответствие настроек.".to_string(),
            "".to_string(),
            "Возвращаемое значение - соответствие.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "соответствие");
        assert_eq!(docs.returned_value[0].description, None);
    }

    #[test]
    fn test_parse_result_freetext_is_not_returns_section() {
        let comments =
            vec!["Описание метода.".to_string(), "Результат: упрощает работу.".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(
            docs.returned_value.is_empty(),
            "Free-text \"Результат: ...\" must not be detected as a Returns section, got: {:?}",
            docs.returned_value
        );
        let purpose = docs.purpose.as_deref().unwrap_or("");
        assert!(
            purpose.contains("упрощает работу"),
            "Expected purpose to include the prose, got: {purpose:?}"
        );
    }

    #[test]
    fn test_parse_english_result_freetext_is_not_returns_section() {
        let comments =
            vec!["Method description.".to_string(), "Result: simplifies the workflow.".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(
            docs.returned_value.is_empty(),
            "Free-text \"Result: ...\" must not be detected as a Returns section, got: {:?}",
            docs.returned_value
        );
        let purpose = docs.purpose.as_deref().unwrap_or("");
        assert!(
            purpose.contains("simplifies the workflow"),
            "Expected purpose to include the prose, got: {purpose:?}"
        );
    }

    #[test]
    fn test_parse_return_collection_of_structure() {
        let comments = vec![
            "Возвращает список правил.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Массив из Структура:".to_string(),
            "    * Ссылка - СправочникСсылка.Правила - правило.".to_string(),
            "    * Представление - Строка - представление правила.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Массив");
        assert_eq!(docs.returned_value[0].description.as_deref(), Some("из Структура"));
        assert_eq!(docs.returned_value[0].parameters.len(), 2);
    }

    #[test]
    fn test_parse_return_map_of_key_and_value() {
        let comments = vec![
            "Возвращает шаблоны выражений.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Соответствие из КлючИЗначение:".to_string(),
            "    * Ключ - Строка - имя шаблона.".to_string(),
            "    * Значение - Строка - выражение на встроенном языке.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Соответствие");
        assert_eq!(docs.returned_value[0].description.as_deref(), Some("из КлючИЗначение"));
        assert_eq!(docs.returned_value[0].parameters.len(), 2);
    }

    #[test]
    fn test_parse_return_collection_of_see_reference() {
        let comments = vec![
            "Возвращает хранимые файлы.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Массив из см. РаботаСФайлами.ДанныеФайла".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Массив");
        assert_eq!(
            docs.returned_value[0].description.as_deref(),
            Some("из см. РаботаСФайлами.ДанныеФайла")
        );
    }

    #[test]
    fn test_parse_return_structure_double_dash() {
        let comments = vec![
            "Возвращает результат фонового задания.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Структура -- содержит следующие параметры:".to_string(),
            "    * ЗаданиеВыполнено - Булево - Истина, если задание выполнено.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Структура");
        assert_eq!(
            docs.returned_value[0].description.as_deref(),
            Some("содержит следующие параметры:")
        );
        assert_eq!(docs.returned_value[0].parameters.len(), 1);
    }

    #[test]
    fn test_parse_returns_does_not_swallow_description_continuation() {
        let comments = vec![
            "Возвращает значения реквизита.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Произвольный - если передана пустая ссылка, возвращается Неопределено.".to_string(),
            "                 Если передана ссылка несуществующего объекта (битая ссылка),"
                .to_string(),
            "                 то возвращается Неопределено.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(
            docs.returned_value.len(),
            1,
            "Description continuation must not be parsed as additional return types, got: {:?}",
            docs.returned_value
        );
        assert_eq!(docs.returned_value[0].name, "Произвольный");
        assert_eq!(
            docs.returned_value[0].description.as_deref(),
            Some(
                "если передана пустая ссылка, возвращается Неопределено.\nЕсли передана ссылка несуществующего объекта (битая ссылка),\nто возвращается Неопределено."
            )
        );
    }

    #[test]
    fn test_parse_hyperlink() {
        let comments = vec!["См. ДругойМетод()".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_hyperlink());
        assert_eq!(docs.link, Some("См. ДругойМетод()".to_string()));
    }

    #[test]
    fn test_parse_hyperlink_with_service_prefix() {
        let comments = vec![
            "СтандартныеПодсистемы.УправлениеДоступом".to_string(),
            "".to_string(),
            "См. УправлениеДоступомПереопределяемый.ПриЗаполненииСписковСОграничениемДоступа."
                .to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_hyperlink());
        assert_eq!(
            docs.link.as_deref(),
            Some(
                "См. УправлениеДоступомПереопределяемый.ПриЗаполненииСписковСОграничениемДоступа."
            )
        );
    }

    #[test]
    fn test_parse_result_section_ends_parameters() {
        let comments = vec![
            "Для переданной организации определяет, является ли она юридическим лицом".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Организация - СправочникСсылка.Организации - организация.".to_string(),
            "".to_string(),
            "Результат:".to_string(),
            "  Булево - Истина, если организация - юридическое лицо.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 1);
        assert_eq!(docs.parameters[0].name, "Организация");
        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Булево");
    }

    #[test]
    fn test_parse_english_result_section_ends_parameters() {
        let comments = vec![
            "Checks whether the organization is legal entity.".to_string(),
            "".to_string(),
            "Parameters:".to_string(),
            "  Organization - CatalogRef.Organizations - organization.".to_string(),
            "".to_string(),
            "Result:".to_string(),
            "  Boolean - true when organization is legal entity.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 1);
        assert_eq!(docs.parameters[0].name, "Organization");
        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Boolean");
    }

    #[test]
    fn test_parse_parameter_description_continuation_not_extra_parameter() {
        let comments = vec![
            "Получает оформленное накладными по заказам количество.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  ОтборПоИзмерениям - Структура - Ключ структуры определяет имя измерения,"
                .to_string(),
            "                      а значение структуры - искомое значение.".to_string(),
            "  ИсключитьЗаказ - Булево - признак исключения заказа.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.parameters[0].name, "ОтборПоИзмерениям");
        assert_eq!(
            docs.parameters[0].types[0].description.as_deref(),
            Some(
                "Ключ структуры определяет имя измерения,\nа значение структуры - искомое значение."
            )
        );
        assert_eq!(docs.parameters[1].name, "ИсключитьЗаказ");
    }

    #[test]
    fn test_parse_nested_parameter_fields_still_attached() {
        let comments = vec![
            "Заполняет настройки.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Настройки - Структура - настройки заполнения:".to_string(),
            "    * Организация - СправочникСсылка.Организации - организация.".to_string(),
            "    * Дата - Дата - дата заполнения.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 1);
        assert_eq!(docs.parameters[0].name, "Настройки");
        assert_eq!(docs.parameters[0].types[0].parameters.len(), 2);
        assert_eq!(docs.parameters[0].types[0].parameters[0].name, "Организация");
        assert_eq!(docs.parameters[0].types[0].parameters[1].name, "Дата");
    }

    #[test]
    fn test_parse_deprecated() {
        let comments = vec![
            "Старый метод.".to_string(),
            "".to_string(),
            "Устарела:".to_string(),
            "Используйте НовыйМетод() вместо этого метода.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_deprecated());
        assert_eq!(
            docs.deprecation,
            Some("Используйте НовыйМетод() вместо этого метода.".to_string())
        );
    }

    #[test]
    fn test_parse_deprecated_with_dot() {
        let comments = vec!["Устарела.".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_deprecated());
        assert_eq!(docs.deprecation, Some("".to_string()));
    }

    #[test]
    fn test_parse_deprecated_with_dot_and_info() {
        let comments = vec!["Устарела. Используйте НовыйМетод().".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_deprecated());
        assert_eq!(docs.deprecation, Some("Используйте НовыйМетод().".to_string()));
    }

    #[test]
    fn test_parse_deprecated_english() {
        let comments = vec!["Deprecated.".to_string()];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.is_deprecated());
    }

    #[test]
    fn test_parse_parameters_with_multiple_types() {
        let comments = vec![
            "Обрабатывает значение.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Значение - Число, Строка - значение для обработки".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 1);
        assert_eq!(docs.parameters[0].name, "Значение");
        assert_eq!(docs.parameters[0].types.len(), 2);
        assert_eq!(docs.parameters[0].types[0].name, "Число");
        assert_eq!(docs.parameters[0].types[1].name, "Строка");
    }

    #[test]
    fn test_parse_call_options() {
        let comments = vec![
            "Выполняет операцию.".to_string(),
            "".to_string(),
            "Варианты вызова:".to_string(),
            "  Вариант 1: Выполнить(Параметр1)".to_string(),
            "  Вариант 2: Выполнить(Параметр1, Параметр2)".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.call_options.len(), 2);
        assert!(docs.call_options[0].contains("Вариант 1"));
        assert!(docs.call_options[1].contains("Вариант 2"));
    }

    #[test]
    fn test_parse_english_documentation() {
        let comments = vec![
            "Calculates the sum.".to_string(),
            "".to_string(),
            "Parameters:".to_string(),
            "  A - Number - first addend".to_string(),
            "  B - Number - second addend".to_string(),
            "".to_string(),
            "Returns:".to_string(),
            "  Number - sum result".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.purpose, Some("Calculates the sum.".to_string()));
        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Number");
    }

    #[test]
    fn test_parse_multiline_purpose() {
        let comments = vec![
            "Первая строка описания.".to_string(),
            "Вторая строка описания.".to_string(),
            "Третья строка описания.".to_string(),
            "".to_string(),
            "Возвращаемое значение:".to_string(),
            "  Булево".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.purpose.is_some());
        let purpose = docs.purpose.unwrap();
        assert!(purpose.contains("Первая строка"));
        assert!(purpose.contains("Вторая строка"));
        assert!(purpose.contains("Третья строка"));
    }

    #[test]
    fn test_parameter_named_result_not_treated_as_returns_section() {
        // A parameter named `Результат` inside the parameters section must not be parsed
        // as a Returns header; otherwise the parameters after it look undocumented.
        let comments = vec![
            "Записывает оценку.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  ИмяЗадачи - Строка - имя задачи.".to_string(),
            "  Результат - Структура - результат поиска.".to_string(),
            "  Оценка - Структура - оценка.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        let names: Vec<_> = docs.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ИмяЗадачи", "Результат", "Оценка"]);
        assert!(docs.returned_value.is_empty());
    }

    #[test]
    fn test_result_with_type_outside_parameters_is_returns() {
        // Without a parameters section, `Результат - Тип` is still a Returns section.
        let comments = vec![
            "Проверяет условие.".to_string(),
            "".to_string(),
            "Результат - Булево - истина, если условие выполнено.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert!(docs.parameters.is_empty());
        assert_eq!(docs.returned_value.len(), 1);
        assert_eq!(docs.returned_value[0].name, "Булево");
    }

    #[test]
    fn test_naprimer_in_description_does_not_truncate_parameters() {
        // `Например:` ("for example") contains `пример:` but is not an Examples header;
        // it must not end the parameters section and drop the parameters after it.
        let comments = vec![
            "Проверяет данные.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  ПолноеИмя - Строка - имя объекта метаданных.".to_string(),
            "             Например: \"Документ.ПриходнаяНакладная\".".to_string(),
            "  Идентификатор - Ссылка - ссылка на элемент данных.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        let names: Vec<_> = docs.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ПолноеИмя", "Идентификатор"]);
        assert!(docs.examples.is_empty());
    }

    #[test]
    fn test_example_header_still_detected() {
        let comments = vec![
            "Делает что-то.".to_string(),
            "".to_string(),
            "Пример:".to_string(),
            "  Функция(Параметр);".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.examples.len(), 1);
        assert!(docs.examples[0].contains("Функция(Параметр)"));
    }

    #[test]
    fn test_blank_line_between_parameters_keeps_result_parameter() {
        for name in ["Результат", "Result"] {
            let comments = vec![
                "Записывает оценку.".to_string(),
                "".to_string(),
                "Параметры:".to_string(),
                "  ИмяЗадачи - Строка - имя задачи.".to_string(),
                "".to_string(),
                format!("  {name} - Структура - результат поиска."),
                "  Оценка - Структура - оценка.".to_string(),
            ];

            let docs = parse_method_docs(&comments).unwrap();

            let names: Vec<_> = docs.parameters.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, vec!["ИмяЗадачи", name, "Оценка"]);
            assert!(docs.returned_value.is_empty());
        }
    }

    #[test]
    fn test_blank_line_does_not_end_parameters_section() {
        // Only the next section header closes "Параметры:"; a blank line does not.
        // Consequence: an abbreviated return without a colon after a blank line is read as
        // one more parameter.
        let comments = vec![
            "Определяет, является ли организация юридическим лицом.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Организация - СправочникСсылка.Организации - организация.".to_string(),
            "".to_string(),
            "Результат - Булево - Истина, если это юридическое лицо.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        let names: Vec<_> = docs.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Организация", "Результат"]);
        assert!(docs.returned_value.is_empty());
    }

    #[test]
    fn test_result_header_with_colon_after_blank_line_is_returns() {
        for header in ["Результат:", "Возвращаемое значение:", "Returns:", "Return value:"]
        {
            let comments = vec![
                "Определяет условие.".to_string(),
                "".to_string(),
                "Параметры:".to_string(),
                "  Организация - СправочникСсылка.Организации - организация.".to_string(),
                "".to_string(),
                header.to_string(),
                "  Булево - Истина, если условие выполнено.".to_string(),
            ];

            let docs = parse_method_docs(&comments).unwrap();

            assert_eq!(docs.parameters.len(), 1, "{header}");
            assert_eq!(docs.returned_value.len(), 1, "{header}");
            assert_eq!(docs.returned_value[0].name, "Булево", "{header}");
        }
    }

    #[test]
    fn test_parse_parameters_with_multiline_union_types() {
        let comments = vec![
            "Возвращает значения реквизита.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Ссылка       - ЛюбаяСсылка - объект, значения реквизитов которого получить."
                .to_string(),
            "               - Строка      - полное имя предопределенного элемента.".to_string(),
            "  ИмяРеквизита - Строка      - имя получаемого реквизита.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2, "Expected exactly 2 parameters, no phantom '-'");

        assert_eq!(docs.parameters[0].name, "Ссылка");
        assert_eq!(docs.parameters[0].types.len(), 2);
        assert_eq!(docs.parameters[0].types[0].name, "ЛюбаяСсылка");
        assert_eq!(
            docs.parameters[0].types[0].description.as_deref(),
            Some("объект, значения реквизитов которого получить.")
        );
        assert_eq!(docs.parameters[0].types[1].name, "Строка");
        assert_eq!(
            docs.parameters[0].types[1].description.as_deref(),
            Some("полное имя предопределенного элемента.")
        );

        assert_eq!(docs.parameters[1].name, "ИмяРеквизита");
        assert_eq!(docs.parameters[1].types.len(), 1);
        assert_eq!(docs.parameters[1].types[0].name, "Строка");
    }

    #[test]
    fn test_parse_parameters_continuation_does_not_swallow_bullet_descriptions() {
        let comments = vec![
            "Описание.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Параметр - Число - значение, особенности:".to_string(),
            "             - дополнительное примечание ниже описания".to_string(),
            "  Другой - Строка - имя".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.parameters[0].name, "Параметр");
        assert_eq!(
            docs.parameters[0].types.len(),
            1,
            "Bullet line must NOT be absorbed as an extra type, got: {:?}",
            docs.parameters[0].types
        );
        assert_eq!(docs.parameters[0].types[0].name, "Число");
        assert_eq!(docs.parameters[1].name, "Другой");
    }

    #[test]
    fn test_method_docs_detects_inline_see_reference() {
        let comments = vec!["Продолжение процедуры (см. выше).".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(docs.has_see_reference());
        assert!(!docs.is_hyperlink(), "a mid-line см. is not a whole-doc hyperlink");
    }

    #[test]
    fn test_method_docs_no_false_see_reference() {
        let comments = vec!["Возвращает обработанное значение без ссылок.".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(!docs.has_see_reference());
    }

    #[test]
    fn test_centimeter_abbreviation_is_not_see_reference() {
        let comments = vec!["Ширина поля равна 5 см. по умолчанию.".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(!docs.has_see_reference(), "the unit abbreviation '5 см.' is not a link");
    }

    #[test]
    fn test_see_reference_with_method_target() {
        let comments = vec!["Обрабатывает данные (см. ОбщийМодуль.Метод).".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(docs.has_see_reference());
    }

    #[test]
    fn test_english_see_verb_midsentence_is_not_reference() {
        let comments = vec!["You can see that this works.".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(!docs.has_see_reference());
    }

    #[test]
    fn test_see_reference_inside_parameter_line_is_not_method_link() {
        let comments = vec![
            "Устанавливает состояние.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Состояние - см. СостояниеЗапроса.".to_string(),
        ];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(
            !docs.has_see_reference(),
            "a см. reference inside a parameter line is not a method-level link"
        );
    }

    #[test]
    fn test_uppercase_see_marker_is_detected() {
        let comments = vec!["Данные реквизита (СМ. ОбщийМодуль.Метод).".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(docs.has_see_reference());
    }

    #[test]
    fn test_see_marker_without_target_is_not_reference() {
        let comments =
            vec!["подробности см.".to_string(), "раздел см. 123 справочника".to_string()];
        let docs = parse_method_docs(&comments).unwrap();
        assert!(
            !docs.has_see_reference(),
            "a marker with no target or a non-alphabetic target is not a reference"
        );
    }

    #[test]
    fn test_parse_parameter_tab_separated_separator() {
        // The first parameter aligns the `-` with tabs (no space before the dash);
        // it must still be recognized as a described parameter.
        let comments = vec![
            "Проверяет условия триггера.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  ДанныеТриггера\t\t\t- Структура, см. ОбщийМодуль.Структура".to_string(),
            "  ОбъектПроверки\t - Ссылка - объект".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.parameters[0].name, "ДанныеТриггера");
        assert_eq!(docs.parameters[0].types[0].name, "Структура");
        assert_eq!(docs.parameters[1].name, "ОбъектПроверки");
    }

    #[test]
    fn test_parse_parameter_bare_dotted_union_continuation_not_phantom() {
        // A union type continued on the next line without a leading `-`: the dotted
        // type must attach to `Письмо`, not become a phantom parameter.
        let comments = vec![
            "Обрабатывает письмо.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Письмо - ДокументСсылка.ЭлектронноеПисьмоВходящее,".to_string(),
            "           ДокументСсылка.ЭлектронноеПисьмоИсходящее - письмо для оценки.".to_string(),
            "  ТекстHTML - Строка - обрабатываемый текст.".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2, "no phantom parameter from union continuation");
        assert_eq!(docs.parameters[0].name, "Письмо");
        assert!(
            docs.parameters[0]
                .types
                .iter()
                .any(|t| t.name == "ДокументСсылка.ЭлектронноеПисьмоИсходящее"),
            "continuation type must attach to Письмо, got: {:?}",
            docs.parameters[0].types
        );
        assert_eq!(docs.parameters[1].name, "ТекстHTML");
    }

    #[test]
    fn test_parse_parameter_dotted_union_continuation_with_tab_dash() {
        // The continuation's inline description is separated by a tab-flanked dash;
        // it must still attach to `Письмо` and not spawn a phantom parameter.
        let comments = vec![
            "Обрабатывает письмо.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Письмо - ДокументСсылка.ЭлектронноеПисьмоВходящее,".to_string(),
            "           ДокументСсылка.ЭлектронноеПисьмоИсходящее\t-\tписьмо для оценки"
                .to_string(),
            "  ТекстHTML - Строка - текст".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 2, "no phantom parameter from tab-dash continuation");
        assert_eq!(docs.parameters[0].name, "Письмо");
        assert!(
            docs.parameters[0]
                .types
                .iter()
                .any(|t| t.name == "ДокументСсылка.ЭлектронноеПисьмоИсходящее"),
            "continuation type must attach to Письмо, got: {:?}",
            docs.parameters[0].types
        );
        assert_eq!(docs.parameters[1].name, "ТекстHTML");
    }

    #[test]
    fn test_parse_parameter_with_structured_type() {
        let comments = vec![
            "Обрабатывает настройки.".to_string(),
            "".to_string(),
            "Параметры:".to_string(),
            "  Настройки - Структура - настройки подключения".to_string(),
            "    * Сервер - Строка - адрес сервера".to_string(),
            "    * Порт - Число - номер порта".to_string(),
        ];

        let docs = parse_method_docs(&comments).unwrap();

        assert_eq!(docs.parameters.len(), 1);
        assert_eq!(docs.parameters[0].name, "Настройки");
        assert_eq!(docs.parameters[0].types[0].name, "Структура");

        let nested = &docs.parameters[0].types[0].parameters;
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0].name, "Сервер");
        assert_eq!(nested[1].name, "Порт");
    }

    #[test]
    fn variable_docs_empty_input_returns_none() {
        assert_eq!(parse_variable_docs(&[]), None);
    }

    #[test]
    fn variable_docs_plain_purpose() {
        let comments = vec!["описание переменной".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert_eq!(docs.purpose.as_deref(), Some("описание переменной"));
        assert!(docs.deprecation.is_none());
        assert!(docs.link.is_none());
        assert!(!docs.is_hyperlink());
        assert!(!docs.is_deprecated());
    }

    #[test]
    fn variable_docs_multiline_purpose_joined_by_newline() {
        let comments = vec!["первая строка".to_string(), "вторая строка".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert_eq!(docs.purpose.as_deref(), Some("первая строка\nвторая строка"));
    }

    #[test]
    fn variable_docs_hyperlink_only() {
        let comments = vec!["См. ОбщегоНазначения.ИмяПеременной".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.is_hyperlink());
        assert_eq!(docs.link.as_deref(), Some("См. ОбщегоНазначения.ИмяПеременной"));
        assert!(docs.purpose.is_none());
        assert!(docs.deprecation.is_none());
    }

    #[test]
    fn variable_docs_hyperlink_english() {
        let comments = vec!["See Common.SomeVariable".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.is_hyperlink());
        assert_eq!(docs.link.as_deref(), Some("See Common.SomeVariable"));
    }

    #[test]
    fn variable_docs_deprecation_section() {
        let comments = vec![
            "Старое описание.".to_string(),
            "Устарела:".to_string(),
            "используйте НовоеИмя.".to_string(),
        ];
        let docs = parse_variable_docs(&comments).unwrap();
        assert_eq!(docs.purpose.as_deref(), Some("Старое описание."));
        assert!(docs.is_deprecated());
        assert!(docs.deprecation.as_deref().unwrap_or("").contains("используйте НовоеИмя"));
    }

    #[test]
    fn variable_docs_deprecation_inline() {
        let comments = vec!["Устарела: используйте НовоеИмя.".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.is_deprecated());
        assert!(docs.purpose.is_none());
    }

    #[test]
    fn variable_docs_keeps_raw_input() {
        let comments = vec!["a".to_string(), "b".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert_eq!(docs.raw, comments);
    }

    #[test]
    fn variable_docs_hyperlink_on_last_line() {
        let comments = vec!["См. ОбщегоНазначения.СомеVariable".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.is_hyperlink());
    }

    #[test]
    fn variable_docs_hyperlink_after_purpose() {
        let comments = vec!["Описание переменной.".to_string(), "См. ДругойМодуль.Имя".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.is_hyperlink());
        assert_eq!(docs.link.as_deref(), Some("См. ДругойМодуль.Имя"));
        assert!(docs.purpose.is_none());
    }

    #[test]
    fn variable_docs_deprecation_suppresses_last_line_hyperlink() {
        let comments = vec![
            "Старое описание.".to_string(),
            "Устарела: используйте НовоеИмя.".to_string(),
            "См. ДругойМодуль".to_string(),
        ];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(!docs.is_hyperlink());
        assert!(docs.is_deprecated());
    }

    #[test]
    fn variable_docs_types_field_unused_for_now() {
        let comments = vec!["ИмяПеременной - Строка - комментарий".to_string()];
        let docs = parse_variable_docs(&comments).unwrap();
        assert!(docs.types.is_empty());
        assert_eq!(docs.purpose.as_deref(), Some("ИмяПеременной - Строка - комментарий"));
    }
}
