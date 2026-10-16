use ide::{Diagnostic as IdeDiagnostic, HlMod, HlRange, HlTag, Severity};
use ide::{DiagnosticTag as IdeTag, TextRange};
use line_index::{LineIndex, TextSize};
use lsp_types::{
    CodeDescription, Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag,
    Location, NumberOrString, Position, Range, SemanticToken, SemanticTokenModifier,
    SemanticTokenType, SemanticTokensLegend, Url,
};

use crate::lsp::PositionEncoding;

pub fn range(line_index: &LineIndex, text: &str, range: TextRange) -> Option<Range> {
    range_with_encoding(line_index, text, range, PositionEncoding::Utf16)
}

pub fn range_with_encoding(
    line_index: &LineIndex,
    text: &str,
    range: TextRange,
    encoding: PositionEncoding,
) -> Option<Range> {
    if encoding == PositionEncoding::Utf8 {
        let start = position(line_index, range.start())?;
        let end = position(line_index, range.end())?;
        return Some(Range { start, end });
    }

    let start = position_utf16(line_index, text, range.start())?;
    let end = position_utf16(line_index, text, range.end())?;
    Some(Range { start, end })
}

pub fn position(line_index: &LineIndex, offset: TextSize) -> Option<Position> {
    let line_col = line_index.try_line_col(offset)?;
    Some(Position { line: line_col.line, character: line_col.col })
}

pub fn position_utf16(line_index: &LineIndex, text: &str, offset: TextSize) -> Option<Position> {
    let line_col = line_index.try_line_col(offset)?;
    let utf16_col = line_index.utf16_col(text, line_col.line, line_col.col);
    Some(Position { line: line_col.line, character: utf16_col })
}

pub fn severity(severity: Severity) -> DiagnosticSeverity {
    match severity {
        Severity::Blocker => DiagnosticSeverity::ERROR,
        Severity::Critical => DiagnosticSeverity::ERROR,
        Severity::Major => DiagnosticSeverity::ERROR,
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Information => DiagnosticSeverity::INFORMATION,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

/// Какие `Diagnostic.tags` клиент готов принять.
///
/// `publishDiagnostics.tagSupport` — не булев признак, а множество значений: клиент
/// перечисляет теги, которые он обрабатывает. Не объявлен — свойство не отправляется
/// вовсе (пустой массив вместо отсутствующего поля заявлял бы поддержку, которой нет);
/// объявлен — массив фильтруется по `valueSet`, а опустевший результат тоже не
/// публикуется.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientTags {
    unnecessary: bool,
    deprecated: bool,
}

impl ClientTags {
    /// Клиент не объявил `tagSupport`: ни один тег не отправляется.
    pub const NONE: Self = Self { unnecessary: false, deprecated: false };

    /// Оба тега: значение для путей без согласованных возможностей клиента
    /// (обёртки `diagnostic`/`diagnostics` вне LSP-проекции).
    pub const ALL: Self = Self { unnecessary: true, deprecated: true };

    /// Разбор `publishDiagnostics.tagSupport` из согласованных при `initialize`
    /// возможностей клиента.
    pub fn from_capabilities(caps: &lsp_types::ClientCapabilities) -> Self {
        Self::from_client_support(
            caps.text_document
                .as_ref()
                .and_then(|td| td.publish_diagnostics.as_ref())
                .and_then(|pd| pd.tag_support.as_ref()),
        )
    }

    fn from_client_support(support: Option<&lsp_types::TagSupport<DiagnosticTag>>) -> Self {
        let Some(support) = support else { return Self::NONE };
        Self {
            unnecessary: support.value_set.contains(&DiagnosticTag::UNNECESSARY),
            deprecated: support.value_set.contains(&DiagnosticTag::DEPRECATED),
        }
    }
}

pub fn diagnostic_tags(tags: &[IdeTag], client: ClientTags) -> Option<Vec<DiagnosticTag>> {
    let mapped: Vec<DiagnosticTag> = tags
        .iter()
        .filter_map(|tag| match tag {
            IdeTag::Unnecessary if client.unnecessary => Some(DiagnosticTag::UNNECESSARY),
            IdeTag::Deprecated if client.deprecated => Some(DiagnosticTag::DEPRECATED),
            IdeTag::Unnecessary | IdeTag::Deprecated => None,
        })
        .collect();
    if mapped.is_empty() {
        return None;
    }
    Some(mapped)
}

pub fn diagnostic(line_index: &LineIndex, text: &str, diag: &IdeDiagnostic) -> Option<Diagnostic> {
    diagnostic_with_encoding(
        line_index,
        text,
        diag,
        PositionEncoding::Utf16,
        CodeDescriptions::Omit,
        ClientTags::ALL,
    )
}

/// Публиковать ли `Diagnostic.codeDescription`.
///
/// Клиент объявляет поддержку через `publishDiagnostics.codeDescriptionSupport`;
/// не объявил — свойство не отправляется, и ссылка на стандарт доезжает до него
/// только суффиксом сообщения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeDescriptions {
    Publish,
    Omit,
}

impl CodeDescriptions {
    pub fn from_client_support(supported: bool) -> Self {
        if supported {
            Self::Publish
        } else {
            Self::Omit
        }
    }
}

pub fn diagnostic_with_encoding(
    line_index: &LineIndex,
    text: &str,
    diag: &IdeDiagnostic,
    encoding: PositionEncoding,
    code_descriptions: CodeDescriptions,
    client_tags: ClientTags,
) -> Option<Diagnostic> {
    let range = range_with_encoding(line_index, text, diag.range, encoding)?;
    let severity = severity(diag.severity);
    let code = Some(NumberOrString::String(diag.code.as_str().to_string()));
    let tags = diagnostic_tags(&diag.tags, client_tags);

    Some(Diagnostic {
        range,
        severity: Some(severity),
        code,
        code_description: match code_descriptions {
            CodeDescriptions::Publish => standard_code_description(diag),
            CodeDescriptions::Omit => None,
        },
        source: Some("bsl-analyzer".to_string()),
        message: ide::message_with_standards(diag.code, &diag.message),
        related_information: None,
        tags,
        data: None,
    })
}

/// Ссылка «подробнее о диагностике» — на проверяемое требование стандарта.
///
/// Клиенты, рендерящие `codeDescription`, показывают её отдельной ссылкой; те,
/// что поле игнорируют, видят тот же адрес в суффиксе сообщения. Диагностика без
/// нормативного источника поля не получает: пустая ссылка выглядела бы как
/// битая.
fn standard_code_description(diag: &IdeDiagnostic) -> Option<CodeDescription> {
    let primary = *ide::standards(diag.code).first()?;
    let href = Url::parse(&ide::standard_url(primary)).ok()?;
    Some(CodeDescription { href })
}

pub fn diagnostics(line_index: &LineIndex, text: &str, diags: &[IdeDiagnostic]) -> Vec<Diagnostic> {
    diagnostics_with_encoding(
        line_index,
        text,
        diags,
        PositionEncoding::Utf16,
        CodeDescriptions::Omit,
        ClientTags::ALL,
    )
}

pub fn diagnostics_with_encoding(
    line_index: &LineIndex,
    text: &str,
    diags: &[IdeDiagnostic],
    encoding: PositionEncoding,
    code_descriptions: CodeDescriptions,
    client_tags: ClientTags,
) -> Vec<Diagnostic> {
    diags
        .iter()
        .filter_map(|d| {
            diagnostic_with_encoding(line_index, text, d, encoding, code_descriptions, client_tags)
        })
        .collect()
}

pub fn location(
    line_index: &LineIndex,
    text: &str,
    url: &Url,
    text_range: TextRange,
) -> Option<Location> {
    location_with_encoding(line_index, text, url, text_range, PositionEncoding::Utf16)
}

pub fn location_with_encoding(
    line_index: &LineIndex,
    text: &str,
    url: &Url,
    text_range: TextRange,
    encoding: PositionEncoding,
) -> Option<Location> {
    let lsp_range = range_with_encoding(line_index, text, text_range, encoding)?;
    Some(Location { uri: url.clone(), range: lsp_range })
}

pub fn related_information(
    line_index: &LineIndex,
    text: &str,
    url: &Url,
    message: String,
    text_range: TextRange,
) -> Option<DiagnosticRelatedInformation> {
    let loc = location_with_encoding(line_index, text, url, text_range, PositionEncoding::Utf16)?;
    Some(DiagnosticRelatedInformation { location: loc, message })
}

pub fn code_action(
    line_index: &LineIndex,
    text: &str,
    uri: &Url,
    diag: &IdeDiagnostic,
    fix: &ide::Fix,
) -> Option<lsp_types::CodeAction> {
    code_action_with_encoding(line_index, text, uri, diag, fix, PositionEncoding::Utf16)
}

/// The `source.fixAll` kind this server advertises and emits. It is a subkind of the
/// standard `source.fixAll`, so a client requesting either matches it.
pub const FIX_ALL_BSL: &str = "source.fixAll.bsl-analyzer";

/// Convert edits to LSP, all-or-nothing: if any edit's range cannot be encoded (a stale,
/// out-of-bounds offset) the whole set is rejected, so a fix is never applied partially.
fn convert_edits(
    line_index: &LineIndex,
    text: &str,
    edits: &[ide::TextEdit],
    encoding: PositionEncoding,
) -> Option<Vec<lsp_types::TextEdit>> {
    edits
        .iter()
        .map(|edit| {
            let edit_range = range_with_encoding(line_index, text, edit.range, encoding)?;
            Some(lsp_types::TextEdit { range: edit_range, new_text: edit.new_text.clone() })
        })
        .collect()
}

fn workspace_edit(uri: &Url, edits: Vec<lsp_types::TextEdit>) -> lsp_types::WorkspaceEdit {
    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);
    lsp_types::WorkspaceEdit { changes: Some(changes), ..Default::default() }
}

pub fn code_action_with_encoding(
    line_index: &LineIndex,
    text: &str,
    uri: &Url,
    diag: &IdeDiagnostic,
    fix: &ide::Fix,
    encoding: PositionEncoding,
) -> Option<lsp_types::CodeAction> {
    let edits = convert_edits(line_index, text, &fix.edits, encoding)?;
    if edits.is_empty() {
        return None;
    }

    Some(lsp_types::CodeAction {
        title: fix.label.clone(),
        kind: Some(lsp_types::CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic_with_encoding(
            line_index,
            text,
            diag,
            encoding,
            CodeDescriptions::Omit,
            // The attached diagnostic is what the client matches the action against,
            // not something it renders from a publication: `publishDiagnostics.tagSupport`
            // does not govern it, and tags keep travelling as they always have.
            ClientTags::ALL,
        )?]),
        edit: Some(workspace_edit(uri, edits)),
        is_preferred: Some(true),
        ..Default::default()
    })
}

/// Build an aggregate code action (a `source.fixAll` batch, or a "fix all occurrences of
/// X" quick fix) from already-merged edits. Unlike a single quick fix it carries no
/// attached diagnostic and is not `preferred`.
pub fn aggregate_code_action(
    line_index: &LineIndex,
    text: &str,
    uri: &Url,
    title: String,
    kind: lsp_types::CodeActionKind,
    edits: &[ide::TextEdit],
    encoding: PositionEncoding,
) -> Option<lsp_types::CodeAction> {
    let edits = convert_edits(line_index, text, edits, encoding)?;
    if edits.is_empty() {
        return None;
    }

    Some(lsp_types::CodeAction {
        title,
        kind: Some(kind),
        diagnostics: None,
        edit: Some(workspace_edit(uri, edits)),
        is_preferred: None,
        ..Default::default()
    })
}

/// Build a `refactor.rewrite` code action from an assist. An assist is a refactoring at
/// the cursor, not a fix for a diagnostic: it carries no attached diagnostic and is not
/// `preferred`. Edits are encoded against this document only, so an assist touching any
/// other file is not offered rather than applied partially.
pub fn code_action_from_assist(
    line_index: &LineIndex,
    text: &str,
    uri: &Url,
    file_id: vfs::FileId,
    assist: &ide::Assist,
    encoding: PositionEncoding,
) -> Option<lsp_types::CodeAction> {
    let mut edits = Vec::new();
    for file_edit in &assist.source_change.edits {
        if file_edit.file_id != file_id {
            return None;
        }
        for edit in &file_edit.edits {
            let range = range_with_encoding(line_index, text, edit.range, encoding)?;
            edits.push(lsp_types::TextEdit { range, new_text: edit.new_text.clone() });
        }
    }
    if edits.is_empty() {
        return None;
    }

    Some(lsp_types::CodeAction {
        title: assist.label.clone(),
        kind: Some(lsp_types::CodeActionKind::REFACTOR_REWRITE),
        edit: Some(workspace_edit(uri, edits)),
        ..Default::default()
    })
}

pub fn semantic_tokens_legend() -> SemanticTokensLegend {
    let token_types = vec![
        SemanticTokenType::KEYWORD,
        SemanticTokenType::FUNCTION,
        SemanticTokenType::PARAMETER,
        SemanticTokenType::VARIABLE,
        SemanticTokenType::STRING,
        SemanticTokenType::NUMBER,
        SemanticTokenType::COMMENT,
        SemanticTokenType::MACRO,
        SemanticTokenType::DECORATOR,
        SemanticTokenType::PROPERTY,
        SemanticTokenType::OPERATOR,
        SemanticTokenType::new("unresolvedReference"),
        SemanticTokenType::TYPE,
        SemanticTokenType::ENUM_MEMBER,
        SemanticTokenType::NAMESPACE,
        SemanticTokenType::CLASS,
        SemanticTokenType::MODIFIER,
    ];

    let token_modifiers = vec![
        SemanticTokenModifier::new("defaultLibrary"),
        SemanticTokenModifier::new("deprecated"),
        SemanticTokenModifier::new("async"),
        SemanticTokenModifier::new("declaration"),
        SemanticTokenModifier::new("definition"),
        SemanticTokenModifier::DOCUMENTATION,
    ];

    SemanticTokensLegend { token_types, token_modifiers }
}

fn token_type_index(tag: HlTag) -> u32 {
    match tag {
        HlTag::Keyword | HlTag::BooleanLiteral => 0,
        HlTag::Function | HlTag::Procedure | HlTag::BuiltinFunction => 1,
        HlTag::Parameter => 2,
        HlTag::Variable => 3,
        HlTag::StringLiteral => 4,
        HlTag::NumberLiteral => 5,
        HlTag::Comment => 6,
        HlTag::Preprocessor => 7,
        HlTag::Annotation => 8,
        HlTag::Property => 9,
        HlTag::Operator => 10,
        HlTag::UnresolvedReference => 11,
        HlTag::Type => 12,
        HlTag::EnumMember => 13,
        HlTag::Namespace => 14,
        HlTag::Class => 15,
        HlTag::CompilerDirective => 16,
    }
}

fn token_modifiers_bitset(mods: HlMod) -> u32 {
    let mut bitset = 0u32;
    if mods.contains(HlMod::EXPORT) {
        bitset |= 1 << 0;
    }
    if mods.contains(HlMod::DEPRECATED) {
        bitset |= 1 << 1;
    }
    if mods.contains(HlMod::ASYNC) {
        bitset |= 1 << 2;
    }
    if mods.contains(HlMod::DECLARATION) {
        bitset |= 1 << 3;
    }
    if mods.contains(HlMod::DEFINITION) {
        bitset |= 1 << 4;
    }
    if mods.contains(HlMod::DOCUMENTATION) {
        bitset |= 1 << 5;
    }
    bitset
}

pub fn semantic_tokens(
    line_index: &LineIndex,
    text: &str,
    highlights: &[HlRange],
) -> Vec<SemanticToken> {
    semantic_tokens_with_encoding(line_index, text, highlights, PositionEncoding::Utf16)
}

pub fn semantic_tokens_with_encoding(
    line_index: &LineIndex,
    text: &str,
    highlights: &[HlRange],
    encoding: PositionEncoding,
) -> Vec<SemanticToken> {
    let mut tokens = Vec::with_capacity(highlights.len());
    let mut prev_line = 0;
    let mut prev_start = 0;
    let mut prev_max_end: Option<TextSize> = None;

    for hl in highlights {
        if let Some(prev_end) = prev_max_end {
            if hl.range.start() < prev_end {
                tracing::warn!(
                    target: "bsl_analyzer::lsp::semantic_tokens",
                    range = ?hl.range,
                    tag = ?hl.tag,
                    prev_max_end = ?prev_end,
                    "ide::highlight() returned an out-of-order or overlapping HlRange; skipping",
                );
                continue;
            }
        }

        let start_pos = match position_for_encoding(line_index, text, hl.range.start(), encoding) {
            Some(pos) => pos,
            None => continue,
        };

        let length = token_len_for_encoding(text, hl.range, encoding);

        let delta_line = start_pos.line - prev_line;
        let delta_start =
            if delta_line == 0 { start_pos.character - prev_start } else { start_pos.character };

        tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: token_type_index(hl.tag),
            token_modifiers_bitset: token_modifiers_bitset(hl.modifiers),
        });

        prev_line = start_pos.line;
        prev_start = start_pos.character;
        prev_max_end = Some(prev_max_end.map_or(hl.range.end(), |p| p.max(hl.range.end())));
    }

    tokens
}

fn position_for_encoding(
    line_index: &LineIndex,
    text: &str,
    offset: TextSize,
    encoding: PositionEncoding,
) -> Option<Position> {
    match encoding {
        PositionEncoding::Utf8 => position(line_index, offset),
        PositionEncoding::Utf16 => position_utf16(line_index, text, offset),
    }
}

fn token_len_for_encoding(text: &str, range: TextRange, encoding: PositionEncoding) -> u32 {
    match encoding {
        PositionEncoding::Utf8 => u32::from(range.len()),
        PositionEncoding::Utf16 => LineIndex::utf16_len(text, range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide::DiagnosticCode;

    fn projected_for(
        code: DiagnosticCode,
        message: &str,
        support: CodeDescriptions,
        client_tags: ClientTags,
    ) -> Diagnostic {
        let text = "А = 1;";
        let line_index = LineIndex::new(text);
        let diag = IdeDiagnostic {
            code,
            message: message.to_string(),
            range: TextRange::new(0.into(), 1.into()),
            severity: Severity::Warning,
            tags: Vec::new(),
            fixes: Vec::new(),
        };
        diagnostic_with_encoding(
            &line_index,
            text,
            &diag,
            PositionEncoding::Utf16,
            support,
            client_tags,
        )
        .expect("проекция диагностики")
    }

    fn projected(code: DiagnosticCode, message: &str) -> Diagnostic {
        projected_for(code, message, CodeDescriptions::Publish, ClientTags::ALL)
    }

    /// Возможности клиента собираются из того же JSON-документа, что приходит в
    /// `initialize`, — вход тестов не расходится с проводом.
    fn client_with(tags: &[DiagnosticTag]) -> ClientTags {
        let caps: lsp_types::ClientCapabilities = serde_json::from_value(serde_json::json!({
            "textDocument": {
                "publishDiagnostics": { "tagSupport": { "valueSet": tags } }
            }
        }))
        .expect("capabilities");
        ClientTags::from_capabilities(&caps)
    }

    fn tagged(tags: Vec<IdeTag>, client: ClientTags) -> Diagnostic {
        let text = "А = 1;";
        let line_index = LineIndex::new(text);
        let diag = IdeDiagnostic {
            code: DiagnosticCode::LineLength,
            message: "Строка слишком длинная".to_string(),
            range: TextRange::new(0.into(), 1.into()),
            severity: Severity::Warning,
            tags,
            fixes: Vec::new(),
        };
        diagnostic_with_encoding(
            &line_index,
            text,
            &diag,
            PositionEncoding::Utf16,
            CodeDescriptions::Omit,
            client,
        )
        .expect("проекция диагностики")
    }

    #[test]
    fn tags_wait_for_the_client_to_ask_for_them() {
        let unnecessary = vec![IdeTag::Unnecessary];
        let deprecated = vec![IdeTag::Deprecated];

        // Без `tagSupport` свойство обязано отсутствовать — даже когда диагностика
        // несёт тег, и именно отсутствовать, а не быть пустым массивом.
        assert_eq!(tagged(unnecessary.clone(), ClientTags::NONE).tags, None);
        assert_eq!(tagged(deprecated.clone(), ClientTags::NONE).tags, None);

        // Частичный valueSet: клиент объявил только `Unnecessary` — `Deprecated`
        // не уезжает. Без этого входа реализация «шлём всё, если tagSupport вообще
        // объявлен» проходила бы проверку и не проверяла ничего.
        let only_unnecessary = client_with(&[DiagnosticTag::UNNECESSARY]);
        assert_eq!(
            tagged(deprecated.clone(), only_unnecessary).tags,
            None,
            "тег вне valueSet не отправляется"
        );
        assert_eq!(
            tagged(unnecessary.clone(), only_unnecessary).tags,
            Some(vec![DiagnosticTag::UNNECESSARY])
        );

        // Пустой valueSet — тоже «ничего не отправляем», а не пустой массив.
        assert_eq!(tagged(unnecessary.clone(), client_with(&[])).tags, None);
    }

    #[test]
    fn a_client_without_tag_support_gets_no_tags_at_all() {
        // Разбор возможностей — отдельный вход от проекции: мутант «нет tagSupport →
        // ALL» внутри `from_capabilities` не виден тестам на `ClientTags::NONE`.
        let bare: lsp_types::ClientCapabilities =
            serde_json::from_value(serde_json::json!({})).expect("capabilities");
        assert_eq!(ClientTags::from_capabilities(&bare), ClientTags::NONE);
    }

    #[test]
    fn each_tag_is_kept_by_its_own_half_of_the_value_set() {
        // Мутант «не слать никогда» краснеет здесь: полный valueSet обязан
        // пропустить оба тега, а не только один из них.
        let both = client_with(&[DiagnosticTag::UNNECESSARY, DiagnosticTag::DEPRECATED]);
        assert_eq!(
            tagged(vec![IdeTag::Unnecessary], both).tags,
            Some(vec![DiagnosticTag::UNNECESSARY])
        );
        assert_eq!(
            tagged(vec![IdeTag::Deprecated], both).tags,
            Some(vec![DiagnosticTag::DEPRECATED])
        );
        assert_eq!(
            tagged(Vec::new(), both).tags,
            None,
            "диагностика без тегов не получает свойство"
        );
    }

    #[test]
    fn code_description_waits_for_the_client_to_ask_for_it() {
        // Клиент, не объявивший `codeDescriptionSupport`, свойства не получает —
        // но ссылку на стандарт всё равно видит, потому что она есть в суффиксе.
        let quiet = projected_for(
            DiagnosticCode::LineLength,
            "Строка слишком длинная",
            CodeDescriptions::Omit,
            ClientTags::ALL,
        );
        assert!(quiet.code_description.is_none());
        assert!(
            quiet.message.contains("https://v8std.ru/std/456/"),
            "без свойства ссылка обязана доезжать суффиксом, иначе клиент теряет её вовсе"
        );

        // Без этой половины гейт «никогда не публиковать» прошёл бы проверку.
        let loud = projected_for(
            DiagnosticCode::LineLength,
            "Строка слишком длинная",
            CodeDescriptions::Publish,
            ClientTags::ALL,
        );
        assert!(loud.code_description.is_some());
    }

    #[test]
    fn standard_reaches_message_and_code_description() {
        let projected = projected(DiagnosticCode::LineLength, "Строка слишком длинная");
        assert_eq!(
            projected.message,
            "Строка слишком длинная (Стандарт 456: https://v8std.ru/std/456/)"
        );
        assert_eq!(
            projected.code_description.expect("ссылка на стандарт").href.as_str(),
            "https://v8std.ru/std/456/"
        );
    }

    #[test]
    fn diagnostic_without_standard_keeps_message_byte_for_byte() {
        // Без этого входа реализация «суффикс всегда» зелена: пустой суффикс
        // строку не меняет, а `code_description: Some("")` прошёл бы проверку
        // на непустоту.
        assert!(
            ide::standards(DiagnosticCode::CognitiveComplexity).is_empty(),
            "вход потерял смысл"
        );
        let projected = projected(DiagnosticCode::CognitiveComplexity, "Слишком сложно");
        assert_eq!(projected.message, "Слишком сложно");
        assert!(projected.code_description.is_none());
    }

    #[test]
    fn multi_standard_lists_all_numbers_and_links_the_checked_one() {
        // Порядок среза содержательный: std773 — проверяемое требование, а
        // std464 лишь наименьший номер. Числовая сортировка увела бы ссылку на
        // смежный стандарт, оставив тест зелёным.
        let projected = projected(DiagnosticCode::DataExchangeLoading, "Нет проверки");
        assert_eq!(
            projected.message,
            "Нет проверки (Стандарты 773, 465, 464, 752: https://v8std.ru/std/773/)"
        );
        assert_eq!(
            projected.code_description.expect("ссылка на стандарт").href.as_str(),
            "https://v8std.ru/std/773/"
        );
    }

    #[test]
    fn suffix_stays_out_of_the_internal_diagnostic() {
        // I5: внутренний слой не несёт формы подачи — иначе уехали бы снапшоты
        // всех диагностик со стандартом.
        let text = "А = 1;";
        let line_index = LineIndex::new(text);
        let diag = IdeDiagnostic {
            code: DiagnosticCode::LineLength,
            message: "Строка слишком длинная".to_string(),
            range: TextRange::new(0.into(), 1.into()),
            severity: Severity::Warning,
            tags: Vec::new(),
            fixes: Vec::new(),
        };
        let _ = diagnostic(&line_index, text, &diag);
        assert!(!diag.message.contains("v8std.ru"));
    }

    #[test]
    fn test_range_conversion() {
        let text = "hello\nworld";
        let line_index = LineIndex::new(text);

        let text_range = TextRange::new(6.into(), 11.into());
        let lsp_range = range(&line_index, text, text_range).unwrap();

        assert_eq!(lsp_range.start.line, 1);
        assert_eq!(lsp_range.start.character, 0);
        assert_eq!(lsp_range.end.line, 1);
        assert_eq!(lsp_range.end.character, 5);
    }

    #[test]
    fn test_severity_conversion() {
        assert_eq!(severity(Severity::Error), DiagnosticSeverity::ERROR);
        assert_eq!(severity(Severity::Warning), DiagnosticSeverity::WARNING);
        assert_eq!(severity(Severity::Hint), DiagnosticSeverity::HINT);
    }

    #[test]
    fn test_diagnostic_conversion() {
        let text = "hello\nworld";
        let line_index = LineIndex::new(text);

        let ide_diag = IdeDiagnostic {
            code: DiagnosticCode::EmptyCodeBlock,
            message: "Empty code block".to_string(),
            severity: Severity::Warning,
            range: TextRange::new(6.into(), 11.into()),
            tags: vec![IdeTag::Unnecessary],
            fixes: vec![],
        };

        let lsp_diag = diagnostic(&line_index, text, &ide_diag).unwrap();

        assert_eq!(lsp_diag.message, "Empty code block");
        assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(lsp_diag.code, Some(NumberOrString::String("EmptyCodeBlock".to_string())));
        assert_eq!(lsp_diag.source, Some("bsl-analyzer".to_string()));
        assert_eq!(lsp_diag.tags, Some(vec![DiagnosticTag::UNNECESSARY]));
    }

    /// Documentation tokens use the advertised modifier and negotiated Unicode units.
    #[test]
    fn test_documentation_semantic_tokens_unicode() {
        let text = "// 😀 Параметры:\r\n// Имя - Строка\r\n";
        let line_index = LineIndex::new(text);
        let highlights: Vec<_> =
            [("Параметры:", HlTag::Keyword), ("Имя", HlTag::Parameter), ("Строка", HlTag::Type)]
                .into_iter()
                .map(|(part, tag)| HlRange {
                    range: TextRange::at(
                        TextSize::from(text.find(part).unwrap() as u32),
                        TextSize::of(part),
                    ),
                    tag,
                    modifiers: HlMod::new().with(HlMod::DOCUMENTATION),
                })
                .collect();
        let legend = semantic_tokens_legend();
        let documentation = legend
            .token_modifiers
            .iter()
            .position(|modifier| *modifier == SemanticTokenModifier::DOCUMENTATION)
            .unwrap();
        for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            let tokens = semantic_tokens_with_encoding(&line_index, text, &highlights, encoding);
            assert_eq!(tokens.len(), 3);
            for (token, highlight) in tokens.iter().zip(&highlights) {
                assert_eq!(token.token_modifiers_bitset, 1 << documentation);
                assert_eq!(
                    legend.token_types[token.token_type as usize].as_str(),
                    highlight.tag.as_str()
                );
            }
            let (start, length) = match encoding {
                PositionEncoding::Utf8 => ("// 😀 ".len(), "Параметры:".len()),
                PositionEncoding::Utf16 => {
                    ("// 😀 ".encode_utf16().count(), "Параметры:".encode_utf16().count())
                }
            };
            assert_eq!(tokens[0].delta_start, start as u32);
            assert_eq!(tokens[0].length, length as u32);
            assert_eq!(tokens[1].delta_line, 1);
            assert_eq!(tokens[1].delta_start, 3);
        }
    }

    #[test]
    fn test_semantic_tokens_encodes_disjoint_tokens() {
        let text = "abc\ndef\n";
        let line_index = LineIndex::new(text);

        let highlights = vec![
            HlRange {
                range: TextRange::new(0.into(), 3.into()),
                tag: HlTag::Variable,
                modifiers: HlMod::new(),
            },
            HlRange {
                range: TextRange::new(4.into(), 7.into()),
                tag: HlTag::Function,
                modifiers: HlMod::new(),
            },
        ];

        let tokens = semantic_tokens(&line_index, text, &highlights);

        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[1].delta_line, 1);
    }

    #[test]
    fn test_semantic_tokens_encode_cyrillic_identifier_span_as_utf16() {
        let text = "    НаборЗаписей = НаборЗаписей\n";
        let line_index = LineIndex::new(text);
        let start = text.find("НаборЗаписей").unwrap() as u32;
        let end = start + "НаборЗаписей".len() as u32;

        let highlights = vec![HlRange {
            range: TextRange::new(start.into(), end.into()),
            tag: HlTag::Variable,
            modifiers: HlMod::new(),
        }];

        let tokens = semantic_tokens(&line_index, text, &highlights);

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 4);
        assert_eq!(tokens[0].length, "НаборЗаписей".encode_utf16().count() as u32);
    }

    #[test]
    fn test_semantic_tokens_utf8_encoding_does_not_shift_record_set_identifier() {
        let text = "\t\t\tНаборЗаписей = НаборЗаписей;\n";
        let line_index = LineIndex::new(text);
        let start = text.rfind("НаборЗаписей").unwrap() as u32;
        let end = start + "НаборЗаписей".len() as u32;

        let highlights = vec![HlRange {
            range: TextRange::new(start.into(), end.into()),
            tag: HlTag::Variable,
            modifiers: HlMod::new(),
        }];

        let utf8_tokens =
            semantic_tokens_with_encoding(&line_index, text, &highlights, PositionEncoding::Utf8);

        assert_eq!(utf8_tokens.len(), 1);
        assert_eq!(utf8_tokens[0].delta_start, start);
        assert_eq!(utf8_tokens[0].length, "НаборЗаписей".len() as u32);
        assert!(text[start as usize..].starts_with("НаборЗаписей"));

        let utf16_tokens =
            semantic_tokens_with_encoding(&line_index, text, &highlights, PositionEncoding::Utf16);
        let utf16_col_as_byte_col = utf16_tokens[0].delta_start as usize;
        let shifted_prefix = text.find("писей = НаборЗаписей").unwrap();
        assert_eq!(
            utf16_col_as_byte_col - 1,
            shifted_prefix,
            "the old UTF-16 token column lands in the observed shifted highlight"
        );
    }

    #[test]
    fn test_semantic_tokens_end_to_end_no_overlap_on_procedure_name() {
        use ide::highlight;
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{FileId, FileSet, VfsPath};

        let code = "Процедура Тест()\n    Форма = ПолучитьФорму(\"Обработка.Тест.Форма\");\nКонецПроцедуры\n";

        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, code);

        let result = highlight(&db, file_id);

        for window in result.highlights.windows(2) {
            assert!(
                window[0].range.start() <= window[1].range.start(),
                "ide::highlight() must return highlights sorted by start; got {:?} then {:?}",
                window[0],
                window[1]
            );
            assert!(
                window[0].range.end() <= window[1].range.start(),
                "ide::highlight() must return non-overlapping highlights; got {:?} overlapping with {:?}",
                window[0],
                window[1]
            );
        }

        let line_index = LineIndex::new(code);
        let tokens = semantic_tokens(&line_index, code, &result.highlights);

        let mut absolute = Vec::with_capacity(tokens.len());
        let (mut line, mut col) = (0u32, 0u32);
        for tok in &tokens {
            line += tok.delta_line;
            if tok.delta_line != 0 {
                col = 0;
            }
            col += tok.delta_start;
            absolute.push((line, col, col + tok.length));
        }

        for window in absolute.windows(2) {
            let (l1, _, e1) = window[0];
            let (l2, s2, _) = window[1];
            assert!(
                l1 != l2 || e1 <= s2,
                "LSP semantic tokens must not overlap; got {:?} then {:?}",
                window[0],
                window[1]
            );
        }

        let proc_name_tokens: Vec<_> = absolute
            .iter()
            .filter(|(line, start, end)| *line == 0 && *start == 10 && *end == 14)
            .collect();
        assert_eq!(
            proc_name_tokens.len(),
            1,
            "expected exactly one token covering the procedure name range, got {proc_name_tokens:?}"
        );
    }

    #[test]
    fn test_range_utf16_cyrillic() {
        let text = "// Описание\nФункция ЗапросВERP(СервисПублика) Экспорт";
        let line_index = LineIndex::new(text);

        let text_range = TextRange::new(35.into(), 52.into());
        let lsp_range = range(&line_index, text, text_range).unwrap();

        assert_eq!(lsp_range.start.line, 1, "Start line should be 1");
        assert_eq!(lsp_range.start.character, 8, "Start character should be 8 (UTF-16 code units)");
        assert_eq!(lsp_range.end.line, 1, "End line should be 1");
        assert_eq!(lsp_range.end.character, 18, "End character should be 18 (UTF-16 code units)");
    }
}
