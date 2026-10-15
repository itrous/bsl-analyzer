pub mod ast;
pub mod ast_utils;
mod comment_run;
pub mod preproc_symbols;
pub mod sdbl_query;
mod syntax_kind;
mod syntax_node;
mod token_walk;

use std::marker::PhantomData;

use parser_error::ParseError;

pub use crate::{
    ast_utils::{
        extract_leading_comment_lines_at_offset, extract_leading_comments,
        extract_leading_comments_at_offset, extract_variable_comments_at_offset,
        has_trailing_comment, has_variable_description, trailing_semicolon,
    },
    comment_run::{
        comment_runs, comment_runs_of, comment_token_is_string_text, CommentLine, CommentRun,
    },
    sdbl_query::{extract_sdbl_with_corrections, SdblQuery, SdblQueryInfo},
    syntax_kind::SyntaxKind,
    syntax_node::{
        clear_shared_node_cache, with_shared_node_cache, BslLanguage, SyntaxElement, SyntaxNode,
        SyntaxNodePtr, SyntaxToken, SyntaxTreeBuilder,
    },
    token_walk::prev_token_past_empty,
};
pub use rowan::{
    Direction, GreenNode, GreenNodeData, NodeCache, NodeOrToken, TextRange, TextSize,
    TokenAtOffset, WalkEvent,
};

pub const MODULE_RANGE: TextRange = TextRange::empty(TextSize::new(0));

/// Токен текста без дерева: вид и диапазон в тексте, из которого он получен.
/// Лексер без состояния режет любые целые строки так же, как файл, поэтому
/// блок строк можно судить по своим токенам, не разбирая файл.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineToken {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

/// Оценка памяти одного элемента дерева: указатель на слот ребёнка плюс
/// упакованный заголовок вид/длина. Байты текста токена считаются отдельно.
pub const HEAP_PER_ELEMENT: usize = std::mem::size_of::<usize>() * 2;

#[derive(Debug, Clone, Eq)]
pub struct Parse<T> {
    green: GreenNode,
    errors: Vec<SyntaxError>,
    heap_bytes: usize,
    _marker: PhantomData<fn() -> T>,
}

/// Структурное равенство, как у производного, но с замыканием по указателю на
/// каждом узле: после переразбора одного метода старое и новое деревья делят
/// все поддеревья, кроме хребта до изменённого, и глубокое сравнение заново
/// обходило бы мегабайты ради ответа «нет».
impl<T> PartialEq for Parse<T> {
    fn eq(&self, other: &Self) -> bool {
        self.heap_bytes == other.heap_bytes
            && self.errors == other.errors
            && green_eq(&self.green, &other.green)
    }
}

impl<T> Parse<T> {
    pub(crate) fn new(green: GreenNode, errors: Vec<SyntaxError>, heap_bytes: usize) -> Self {
        Self { green, errors, heap_bytes, _marker: PhantomData }
    }

    /// Собрать разбор из готового дерева: путь переразбора, где дерево
    /// получено вклейкой, а оценка памяти выведена из старой.
    pub fn from_parts(green: GreenNode, errors: Vec<SyntaxError>, heap_bytes: usize) -> Self {
        Self::new(green, errors, heap_bytes)
    }

    pub fn syntax_node(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn green(&self) -> &GreenNode {
        &self.green
    }

    /// Оценка памяти дерева: [`HEAP_PER_ELEMENT`] на элемент плюс байты
    /// токенов — то же, что даёт обход `descendants_with_tokens`, но
    /// посчитанное при построении, а не обходом на каждую запись мемо.
    pub fn heap_bytes(&self) -> usize {
        self.heap_bytes
    }

    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

impl Parse<SyntaxNode> {
    pub fn tree(&self) -> SyntaxNode {
        self.syntax_node()
    }
}

/// Равенство зелёных деревьев с замыканием по указателю на каждом узле.
///
/// Ответ тот же, что у структурного `==` rowan; цена — O(различий), а не
/// O(дерева), когда деревья делят поддеревья. У rowan 0.16.1 замыкания нет:
/// `GreenNode == GreenNode` идёт через `ThinArc`, который сравнивает данные.
pub fn green_eq(a: &GreenNode, b: &GreenNode) -> bool {
    green_data_eq(a, b)
}

fn green_data_eq(a: &rowan::GreenNodeData, b: &rowan::GreenNodeData) -> bool {
    if std::ptr::eq(a, b) {
        return true;
    }
    if a.kind() != b.kind() || a.text_len() != b.text_len() {
        return false;
    }
    let mut a_children = a.children();
    let mut b_children = b.children();
    loop {
        match (a_children.next(), b_children.next()) {
            (None, None) => return true,
            (Some(NodeOrToken::Node(x)), Some(NodeOrToken::Node(y))) => {
                if !green_data_eq(x, y) {
                    return false;
                }
            }
            (Some(NodeOrToken::Token(x)), Some(NodeOrToken::Token(y))) => {
                if x != y {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    message: String,
    range: TextRange,
    error: ParseError,
}

impl SyntaxError {
    pub fn new(range: TextRange, err: ParseError) -> Self {
        Self { message: err.format_ru(), range, error: err }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn range(&self) -> TextRange {
        self.range
    }

    pub fn structured(&self) -> &ParseError {
        &self.error
    }
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {:?}", self.message, self.range)
    }
}

impl std::error::Error for SyntaxError {}

#[cfg(test)]
mod tests {
    use parser_error::{ParseError, RecoveryKind};

    use super::*;

    #[test]
    fn syntax_error_carries_structured_payload() {
        let range = TextRange::new(0.into(), 5.into());
        let err = ParseError::Unexpected { found: None, recovery: RecoveryKind::MissingToken };
        let syntax_err = SyntaxError::new(range, err.clone());

        assert_eq!(syntax_err.range(), range);
        assert_eq!(syntax_err.structured(), &err);
        assert!(syntax_err.message().contains("конец файла"));
    }
}
