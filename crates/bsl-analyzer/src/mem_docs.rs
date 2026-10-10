use std::sync::Arc;

use anyhow::Result;
use line_index::LineIndex;
use lsp_types::{TextDocumentContentChangeEvent, Url};
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::lsp::PositionEncoding;

#[derive(Debug, Clone, Default)]
pub struct MemDocs {
    docs: Arc<RwLock<FxHashMap<Url, DocumentData>>>,
}

#[derive(Debug, Clone)]
struct DocumentData {
    text: String,
    version: i32,
    line_index: LineIndex,
}

#[derive(Debug, Clone)]
pub struct FrozenDocument {
    text: String,
    version: i32,
    line_index: LineIndex,
}

impl FrozenDocument {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }
}

#[derive(Debug, Clone, Default)]
pub struct FrozenMemDocs {
    docs: Arc<FxHashMap<Url, FrozenDocument>>,
}

impl FrozenMemDocs {
    pub fn get(&self, uri: &Url) -> Option<&FrozenDocument> {
        self.docs.get(uri)
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

impl MemDocs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, uri: Url, text: String, version: i32) {
        let line_index = LineIndex::new(&text);
        let data = DocumentData { text, version, line_index };
        self.docs.write().insert(uri, data);
    }

    pub fn update(&mut self, uri: &Url, changes: Vec<TextDocumentContentChangeEvent>) {
        if let Err(err) = self.update_with_encoding(uri, changes, PositionEncoding::Utf16) {
            tracing::error!(%uri, error = %err, "failed to apply document changes");
        }
    }

    pub fn update_with_encoding(
        &mut self,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
        encoding: PositionEncoding,
    ) -> Result<()> {
        let mut docs = self.docs.write();

        if let Some(data) = docs.get_mut(uri) {
            // The batch lands whole or not at all: on an error the caller leaves
            // the VFS untouched, so a half-applied batch would split the two.
            let mut text = data.text.clone();
            let mut line_index = data.line_index.clone();
            for change in changes {
                if let Some(range) = change.range {
                    let start = lsp_position_to_offset(&line_index, &text, range.start, encoding)?;
                    let end = lsp_position_to_offset(&line_index, &text, range.end, encoding)?;

                    text.replace_range(start..end, &change.text);
                } else {
                    text = change.text;
                }

                line_index = LineIndex::new(&text);
            }
            data.text = text;
            data.line_index = line_index;
            data.version += 1;
        } else {
            tracing::warn!("Attempted to update non-existent document: {}", uri);
        }

        Ok(())
    }

    pub fn remove(&mut self, uri: &Url) {
        self.docs.write().remove(uri);
    }

    pub fn get(&self, uri: &Url) -> Option<String> {
        self.docs.read().get(uri).map(|data| data.text.clone())
    }

    pub fn get_version(&self, uri: &Url) -> Option<i32> {
        self.docs.read().get(uri).map(|data| data.version)
    }

    pub fn get_line_index(&self, uri: &Url) -> Option<LineIndex> {
        self.docs.read().get(uri).map(|data| data.line_index.clone())
    }

    pub fn contains(&self, uri: &Url) -> bool {
        self.docs.read().contains_key(uri)
    }

    pub fn len(&self) -> usize {
        self.docs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.read().is_empty()
    }

    pub fn uris(&self) -> Vec<Url> {
        self.docs.read().keys().cloned().collect()
    }

    pub fn freeze(&self) -> FrozenMemDocs {
        let live = self.docs.read();
        let frozen: FxHashMap<Url, FrozenDocument> = live
            .iter()
            .map(|(uri, data)| {
                (
                    uri.clone(),
                    FrozenDocument {
                        text: data.text.clone(),
                        version: data.version,
                        line_index: data.line_index.clone(),
                    },
                )
            })
            .collect();
        FrozenMemDocs { docs: Arc::new(frozen) }
    }
}

/// Byte offset of an LSP position inside the document text.
///
/// The bounds and the character boundary are proven by
/// [`crate::lsp::edit_offset_with_encoding`], which refuses a column past the
/// line end: clamping an edit would silently rewrite the span between the line
/// end and the column the client actually named. A cursor position on a request
/// clamps there instead (LSP 3.17) — the two conversions share every other rule.
fn lsp_position_to_offset(
    line_index: &LineIndex,
    text: &str,
    position: lsp_types::Position,
    encoding: PositionEncoding,
) -> Result<usize> {
    crate::lsp::edit_offset_with_encoding(line_index, text, position, encoding).map(usize::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use line_index::{LineCol, TextSize};

    #[test]
    fn test_insert_and_get() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");
        let text = "Процедура Тест()\nКонецПроцедуры".to_string();

        mem_docs.insert(uri.clone(), text.clone(), 1);

        assert_eq!(mem_docs.get(&uri), Some(text));
        assert_eq!(mem_docs.get_version(&uri), Some(1));
        assert!(mem_docs.contains(&uri));
    }

    #[test]
    fn test_update_full() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "old text".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "new text".to_string(),
        }];

        mem_docs.update(&uri, changes);

        assert_eq!(mem_docs.get(&uri), Some("new text".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(2));
    }

    #[test]
    fn test_update_incremental() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "hello world".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 6 },
                end: lsp_types::Position { line: 0, character: 11 },
            }),
            range_length: Some(5),
            text: "rust".to_string(),
        }];

        mem_docs.update(&uri, changes);

        assert_eq!(mem_docs.get(&uri), Some("hello rust".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(2));
    }

    /// Правка с колонкой за концом строки отвергается: усечение молча
    /// переписало бы не тот участок буфера — ради этого у правок и у позиций
    /// запросов разные политики (github#91).
    #[test]
    fn an_edit_range_past_the_line_end_is_refused_and_changes_nothing() {
        let mut mem_docs = MemDocs::new();
        let uri = Url::parse("file:///test.bsl").unwrap();

        mem_docs.insert(uri.clone(), "Процедура\nТест".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 40 },
                end: lsp_types::Position { line: 0, character: 40 },
            }),
            range_length: None,
            text: "X".to_string(),
        }];

        assert!(mem_docs.update_with_encoding(&uri, changes, PositionEncoding::Utf16).is_err());
        assert_eq!(mem_docs.get(&uri), Some("Процедура\nТест".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(1));
    }

    #[test]
    fn an_edit_on_the_carriage_return_of_a_crlf_line_is_refused_and_changes_nothing() {
        let mut mem_docs = MemDocs::new();
        let uri = Url::parse("file:///test.bsl").unwrap();

        mem_docs.insert(uri.clone(), "a\r\nb".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 2 },
                end: lsp_types::Position { line: 0, character: 2 },
            }),
            range_length: None,
            text: "X".to_string(),
        }];

        assert!(mem_docs.update_with_encoding(&uri, changes, PositionEncoding::Utf16).is_err());
        assert_eq!(mem_docs.get(&uri), Some("a\r\nb".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(1));
    }

    #[test]
    fn a_refused_change_rolls_back_the_changes_before_it_in_the_batch() {
        let mut mem_docs = MemDocs::new();
        let uri = Url::parse("file:///test.bsl").unwrap();

        mem_docs.insert(uri.clone(), "abc\ndef".to_string(), 1);

        let change =
            |start: (u32, u32), end: (u32, u32), text: &str| TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position { line: start.0, character: start.1 },
                    end: lsp_types::Position { line: end.0, character: end.1 },
                }),
                range_length: None,
                text: text.to_string(),
            };
        // The caller skips the VFS on an error, so a half-applied batch would
        // leave the two holding different texts.
        let changes = vec![change((0, 0), (0, 1), "X"), change((1, 0), (1, 99), "Y")];

        assert!(mem_docs.update_with_encoding(&uri, changes, PositionEncoding::Utf16).is_err());
        assert_eq!(mem_docs.get(&uri), Some("abc\ndef".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(1));
    }

    #[test]
    fn test_remove() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "test".to_string(), 1);
        assert!(mem_docs.contains(&uri));

        mem_docs.remove(&uri);
        assert!(!mem_docs.contains(&uri));
        assert_eq!(mem_docs.get(&uri), None);
    }

    #[test]
    fn test_line_index() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");
        let text = "line1\nline2\nline3".to_string();

        mem_docs.insert(uri.clone(), text, 1);

        let line_index = mem_docs.get_line_index(&uri).unwrap();

        let pos = line_index.line_col(TextSize::from(6));
        assert_eq!(pos, LineCol { line: 1, col: 0 });
    }

    #[test]
    fn test_update_incremental_cyrillic() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "Процедура Тест()\nКонецПроцедуры".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 10 },
                end: lsp_types::Position { line: 0, character: 10 },
            }),
            range_length: Some(0),
            text: "Новая".to_string(),
        }];

        mem_docs.update(&uri, changes);

        assert_eq!(mem_docs.get(&uri), Some("Процедура НоваяТест()\nКонецПроцедуры".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(2));
    }

    #[test]
    fn test_update_incremental_replace_cyrillic() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "Функция Старый()\nКонецФункции".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 8 },
                end: lsp_types::Position { line: 0, character: 14 },
            }),
            range_length: Some(6),
            text: "Новый".to_string(),
        }];

        mem_docs.update(&uri, changes);

        assert_eq!(mem_docs.get(&uri), Some("Функция Новый()\nКонецФункции".to_string()));
    }

    #[test]
    fn test_update_incremental_cyrillic_utf8_encoding() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "Процедура Тест()\nКонецПроцедуры".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 19 },
                end: lsp_types::Position { line: 0, character: 19 },
            }),
            range_length: Some(0),
            text: "Новая".to_string(),
        }];

        mem_docs.update_with_encoding(&uri, changes, PositionEncoding::Utf8).unwrap();

        assert_eq!(mem_docs.get(&uri), Some("Процедура НоваяТест()\nКонецПроцедуры".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(2));
    }

    #[test]
    fn test_update_incremental_utf8_rejects_non_char_boundary() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "Функция Старый()\nКонецФункции".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 1 },
                end: lsp_types::Position { line: 0, character: 1 },
            }),
            range_length: Some(0),
            text: "X".to_string(),
        }];

        let result = mem_docs.update_with_encoding(&uri, changes, PositionEncoding::Utf8);

        assert!(result.is_err());
        assert_eq!(mem_docs.get(&uri), Some("Функция Старый()\nКонецФункции".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(1));
    }

    #[test]
    fn freeze_is_independent_of_mutation() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");
        mem_docs.insert(uri.clone(), "original".to_string(), 1);

        let frozen = mem_docs.freeze();

        let changes = vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "mutated".to_string(),
        }];
        mem_docs.update(&uri, changes);

        assert_eq!(frozen.get(&uri).map(|d| d.text()), Some("original"));
        assert_eq!(frozen.get(&uri).map(|d| d.version()), Some(1));
        assert_eq!(mem_docs.get(&uri), Some("mutated".to_string()));
        assert_eq!(mem_docs.get_version(&uri), Some(2));

        mem_docs.remove(&uri);
        assert!(frozen.get(&uri).is_some(), "frozen view must survive removal");
    }

    #[test]
    fn test_update_incremental_multiline_cyrillic() {
        let mut mem_docs = MemDocs::new();
        let uri = crate::test_uri::file_uri("test.bsl");

        mem_docs.insert(uri.clone(), "Процедура\nТест()\nКонецПроцедуры".to_string(), 1);

        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position { line: 0, character: 9 },
                end: lsp_types::Position { line: 1, character: 0 },
            }),
            range_length: Some(1),
            text: " ".to_string(),
        }];

        mem_docs.update(&uri, changes);

        assert_eq!(mem_docs.get(&uri), Some("Процедура Тест()\nКонецПроцедуры".to_string()));
    }
}
