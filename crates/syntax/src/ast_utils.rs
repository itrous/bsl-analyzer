use crate::{comment_runs, CommentRun, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// Whether the element is an extension marker or a block of text the extension
/// removed — part of the source, but not of the module the platform compiles.
///
/// Inside an expression the parser keeps them where they stand: the markers of
/// an insertion as tokens, a removal as an opaque `PRE_DELETE_DIR`. Whatever
/// reads the expression for its meaning has to step over both, or the removed
/// argument comes back as an argument and a marker as an operand.
///
/// A conditional group that tears a construct apart is kept the same way: its
/// directives and the branches not read are opaque `PRE_TORN_IF_PART` nodes.
pub fn is_extension_directive(element: &SyntaxElement) -> bool {
    matches!(
        element.kind(),
        SyntaxKind::PRE_DELETE_DIR
            | SyntaxKind::PRE_TORN_IF_PART
            | SyntaxKind::PRE_INSERT
            | SyntaxKind::PRE_END_INSERT
    )
}

/// Child nodes of `node` that stand in the compiled module: everything but
/// the text an extension removed and the parts of a torn conditional group.
pub fn active_children(node: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> {
    node.children().filter(|child| {
        !matches!(child.kind(), SyntaxKind::PRE_DELETE_DIR | SyntaxKind::PRE_TORN_IF_PART)
    })
}

/// Children of `node`, nodes and tokens, that stand in the compiled module:
/// without the markers of extension blocks and without the text removed.
pub fn active_children_with_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxElement> {
    node.children_with_tokens().filter(|child| !is_extension_directive(child))
}

/// Точка с запятой, стоящая за узлом через одну лишь тривию.
///
/// Тривия принадлежит предку, а не узлу, поэтому между оператором и его `;`
/// стоят пробел, перевод строки или комментарий, а требование
/// непосредственного соседства даёт «точки с запятой нет» на любом
/// отформатированном коде.
///
/// Соседний УЗЕЛ поиск прекращает: за ним стоит уже другой оператор, и его
/// точка с запятой этому узлу не принадлежит.
pub fn trailing_semicolon(node: &SyntaxNode) -> Option<SyntaxToken> {
    let mut next = node.next_sibling_or_token();
    while let Some(element) = next {
        let token = element.as_token()?;
        if token.kind() == SyntaxKind::SEMICOLON {
            return Some(token.clone());
        }
        if !token.kind().is_trivia() {
            return None;
        }
        next = element.next_sibling_or_token();
    }
    None
}

pub fn extract_leading_comments(node: &SyntaxNode, source_text: &str) -> Option<Vec<String>> {
    // Leading comments are trivia of the enclosing nodes, not of `node`, so
    // the runs are taken over the whole tree.
    let root = node.ancestors().last().unwrap_or_else(|| node.clone());
    let runs = comment_runs(&root);
    let node_start: usize = node.text_range().start().into();
    extract_leading_comments_at_offset(node_start, source_text, &runs)
}

/// Documentation block right above `offset`: the comments of `runs` on the
/// lines directly above it, each one alone on its line.
///
/// A blank line or code breaks the block, so an unrelated comment further up
/// (e.g. a change-log marker) is never attached as the method's documentation.
/// `runs` are the comment runs of the tree `source_text` was parsed into.
pub fn extract_leading_comments_at_offset(
    offset: usize,
    source_text: &str,
    runs: &[CommentRun],
) -> Option<Vec<String>> {
    if offset > source_text.len() {
        return None;
    }
    leading_comments(source_text, offset, runs, LeadingScope::Method, Layout::Trimmed)
}

/// Same block as [`extract_leading_comments_at_offset`], but each line keeps
/// its indentation and empty `//` lines stay: documentation tells fields from
/// their type and description continuations by them. Only the one space right
/// after `//` is dropped.
pub fn extract_leading_comment_lines_at_offset(
    offset: usize,
    source_text: &str,
    runs: &[CommentRun],
) -> Option<Vec<String>> {
    if offset > source_text.len() {
        return None;
    }
    leading_comments(source_text, offset, runs, LeadingScope::Method, Layout::Preserved)
}

/// What stands between a declaration and its leading comments.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeadingScope {
    /// Nothing: only comment lines.
    Method,
    /// Annotation lines as well; the text of a comment the anchor stands in
    /// counts up to the anchor.
    Variable,
}

/// How much of a comment line's text is kept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Trimmed content; empty `//` lines are dropped.
    Trimmed,
    /// Indentation and empty `//` lines are kept.
    Preserved,
}

/// Contents of the comments directly above `anchor`, in text order; `None`
/// when there is nothing but empty `//` markers.
///
/// Comments come only from `runs`; the text is read for what the runs do not
/// say: what precedes a comment on its line and which lines lie between two
/// comments. A run may go on below the anchor or pass through a trailing
/// comment of a code line, so the block is cut out of the runs line by line
/// rather than taken as a whole run.
fn leading_comments(
    text: &str,
    anchor: usize,
    runs: &[CommentRun],
    scope: LeadingScope,
    layout: Layout,
) -> Option<Vec<String>> {
    let before_anchor = runs.partition_point(|run| usize::from(run.range().start()) < anchor);
    let mut candidates = runs[..before_anchor]
        .iter()
        .rev()
        .flat_map(|run| run.lines().iter().rev())
        .filter(|line| usize::from(line.range.start()) < anchor)
        .peekable();

    let mut comments = Vec::new();
    let mut push = |content: &str| match layout {
        Layout::Trimmed => {
            let content = content.trim();
            if !content.is_empty() {
                comments.push(content.to_string());
            }
        }
        Layout::Preserved => {
            let content = content.strip_prefix(' ').unwrap_or(content).trim_end();
            comments.push(content.to_string());
        }
    };

    // The anchor's own line up to the anchor is indentation, an annotation or
    // the part of a comment the anchor stands in; it is not a separate line
    // and does not end the block.
    let mut line_start = text[..anchor].rfind('\n').map_or(0, |newline| newline + 1);
    let fragment = &text[line_start..anchor];
    // An anchor between the two slashes leaves a lone `/`, which is code.
    let fragment_comment = candidates
        .next_if(|line| usize::from(line.range.start()) >= line_start)
        .map(|line| usize::from(line.range.start()))
        .filter(|&start| text[line_start..start].trim().is_empty() && anchor >= start + 2);
    let fragment_trimmed = fragment.trim();
    let fragment_is_open = fragment_trimmed.is_empty()
        || (scope == LeadingScope::Variable && fragment_trimmed.starts_with('&'));
    if !fragment_is_open {
        let start = fragment_comment?;
        if scope == LeadingScope::Variable {
            push(&text[start + 2..anchor]);
        }
    }

    // A byte order mark opens the file, not a line of code: it must not hide
    // the first line of a method's documentation.
    let line_prefix = |start: usize, end: usize| {
        let prefix = &text[start..end];
        let prefix = if start == 0 && scope == LeadingScope::Method {
            prefix.strip_prefix('\u{feff}').unwrap_or(prefix)
        } else {
            prefix
        };
        prefix.trim()
    };
    while line_start > 0 {
        let line_end = line_start - 1;
        let prev_start = text[..line_end].rfind('\n').map_or(0, |newline| newline + 1);
        // A comment runs to the end of its line, so the one starting on this
        // line is the last thing on it.
        let comment = candidates.next_if(|line| usize::from(line.range.start()) >= prev_start);
        match comment {
            Some(comment) if line_prefix(prev_start, comment.range.start().into()).is_empty() => {
                push(text[comment.range].strip_prefix("//").unwrap_or_default());
            }
            _ if scope == LeadingScope::Variable
                && text[prev_start..line_end].trim().starts_with('&') => {}
            _ => break,
        }
        line_start = prev_start;
    }

    if comments.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    comments.reverse();
    Some(comments)
}

pub fn has_trailing_comment(node: &SyntaxNode, source_text: &str) -> bool {
    let node_range = node.text_range();
    let node_end: usize = node_range.end().into();

    if node_end >= source_text.len() {
        return false;
    }

    let text_after = &source_text[node_end..];
    let mut chars = text_after.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\r' => return false,
            '/' => {
                if chars.peek() == Some(&'/') {
                    return true;
                }
                return false;
            }
            ' ' | '\t' => continue,
            _ => return false,
        }
    }
    false
}

pub fn has_variable_leading_description(
    var_keyword_offset: usize,
    source_text: &str,
    first_annotation_offset: Option<usize>,
) -> bool {
    let check_from = first_annotation_offset.unwrap_or(var_keyword_offset);

    if check_from == 0 || check_from > source_text.len() {
        return false;
    }

    let text_before = &source_text[..check_from];

    // Backwards scan from the anchor: the loop exits on the first decisive
    // line, so the cost is bounded by the annotation/comment block, not by
    // the length of the preceding text.
    let mut rev_lines = text_before.rsplit('\n');
    let last_line = rev_lines.next().unwrap_or("");
    let trimmed_last = last_line.trim();
    let skip_last = trimmed_last.is_empty() || trimmed_last.starts_with('&');

    for line in (!skip_last).then_some(last_line).into_iter().chain(rev_lines) {
        let line = line.trim();

        if line.starts_with("//") {
            return true;
        }

        if line.is_empty() {
            return false;
        }

        if line.starts_with('&') {
            continue;
        }

        return false;
    }

    false
}

pub fn has_variable_description(
    node: &SyntaxNode,
    var_keyword_offset: usize,
    source_text: &str,
    first_annotation_offset: Option<usize>,
) -> bool {
    if has_trailing_comment(node, source_text) {
        return true;
    }

    if first_annotation_offset.is_some()
        && has_annotation_comments(var_keyword_offset, source_text, first_annotation_offset)
    {
        return true;
    }

    has_variable_leading_description(var_keyword_offset, source_text, first_annotation_offset)
}

pub fn extract_variable_comments_at_offset(
    file_text: &str,
    var_keyword_offset: usize,
    var_end_offset: usize,
    first_annotation_offset: Option<usize>,
    runs: &[CommentRun],
) -> Option<Vec<String>> {
    debug_assert!(
        var_keyword_offset == 0 || file_text.is_char_boundary(var_keyword_offset),
        "var_keyword_offset {var_keyword_offset} not on a char boundary"
    );
    debug_assert!(
        var_end_offset == 0 || file_text.is_char_boundary(var_end_offset),
        "var_end_offset {var_end_offset} not on a char boundary"
    );
    debug_assert!(
        first_annotation_offset.is_none_or(|o| o == 0 || file_text.is_char_boundary(o)),
        "first_annotation_offset {first_annotation_offset:?} not on a char boundary"
    );

    let mut comments: Vec<String> = Vec::new();

    let leading_anchor = first_annotation_offset.unwrap_or(var_keyword_offset);
    if let Some(leading) = collect_variable_leading_comments(file_text, leading_anchor, runs) {
        comments.extend(leading);
    }

    if let Some(first_ann) = first_annotation_offset {
        if first_ann < var_keyword_offset && var_keyword_offset <= file_text.len() {
            let block = &file_text[first_ann..var_keyword_offset];
            for line in block.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("//") {
                    let comment_text = rest.trim();
                    if !comment_text.is_empty() {
                        comments.push(comment_text.to_string());
                    }
                }
            }
        }
    }

    if let Some(trailing) = scan_variable_trailing_comment(file_text, var_end_offset) {
        comments.push(trailing);
    }

    if comments.is_empty() {
        None
    } else {
        Some(comments)
    }
}

fn collect_variable_leading_comments(
    file_text: &str,
    anchor: usize,
    runs: &[CommentRun],
) -> Option<Vec<String>> {
    if anchor == 0 || anchor > file_text.len() {
        return None;
    }
    leading_comments(file_text, anchor, runs, LeadingScope::Variable, Layout::Trimmed)
}

fn scan_variable_trailing_comment(file_text: &str, var_end_offset: usize) -> Option<String> {
    if var_end_offset >= file_text.len() {
        return None;
    }
    let text_after = &file_text[var_end_offset..];
    for (i, ch) in text_after.char_indices() {
        match ch {
            '\n' | '\r' => return None,
            ' ' | '\t' => continue,
            '/' => {
                let after_first = &text_after[i + ch.len_utf8()..];
                if !after_first.starts_with('/') {
                    return None;
                }
                let after_slashes = &after_first['/'.len_utf8()..];
                let line = after_slashes.lines().next().unwrap_or("").trim();
                if line.is_empty() {
                    return None;
                }
                return Some(line.to_string());
            }
            _ => return None,
        }
    }
    None
}

fn has_annotation_comments(
    var_keyword_offset: usize,
    source_text: &str,
    first_annotation_offset: Option<usize>,
) -> bool {
    let first_ann = match first_annotation_offset {
        Some(off) => off,
        None => return false,
    };

    if first_ann >= var_keyword_offset {
        return false;
    }

    let annotation_block = &source_text[first_ann..var_keyword_offset];

    for line in annotation_block.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return true;
        }
    }

    false
}

/// Токены-имена, лежащие непосредственно в узле, в порядке текста.
///
/// Составное имя собирается отсюда, а не из `node.text()`: тривия узла —
/// пробелы, переводы строк и комментарии — лежит внутри него и ушла бы в имя.
pub fn direct_name_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|token| token.kind().is_name_token())
}

/// Значимые токены узла на любой глубине: всё, кроме тривии.
pub fn significant_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    node.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|token| !token.kind().is_trivia())
}

/// Два узла состоят из одних и тех же значимых токенов, то есть различаются
/// только тривией.
pub fn same_significant_tokens(left: &SyntaxNode, right: &SyntaxNode) -> bool {
    let mut left = significant_tokens(left);
    let mut right = significant_tokens(right);

    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(l), Some(r)) if l.kind() == r.kind() && l.text() == r.text() => continue,
            _ => return false,
        }
    }
}

pub fn field_tail_name_token(field_expr: &SyntaxNode) -> Option<SyntaxToken> {
    if field_expr.kind() != SyntaxKind::FIELD_EXPR {
        return None;
    }
    let mut saw_dot = false;
    field_expr.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
        if !saw_dot {
            saw_dot = tok.kind() == SyntaxKind::DOT;
            return false;
        }
        tok.kind().is_name_token()
    })
}

pub fn new_expr_type_name_token(new_expr: &SyntaxNode) -> Option<SyntaxToken> {
    if new_expr.kind() != SyntaxKind::NEW_EXPR {
        return None;
    }
    let mut saw_new = false;
    new_expr.children_with_tokens().filter_map(|el| el.into_token()).find(|tok| {
        if !saw_new {
            saw_new = tok.kind() == SyntaxKind::KW_NEW;
            return false;
        }
        tok.kind().is_name_token()
    })
}
