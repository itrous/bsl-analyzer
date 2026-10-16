use la_arena::{Arena, Idx};
use rustc_hash::FxHashMap;
use syntax::{SyntaxKind, SyntaxNode};
use text_size::TextRange;

pub type ConditionalIdx = Idx<ConditionalData>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalData {
    pub kind: ConditionalKind,

    pub condition_text: Option<String>,

    pub range: TextRange,

    pub directive_range: TextRange,

    pub condition_range: Option<TextRange>,

    pub parent: Option<ConditionalIdx>,

    pub children: Vec<ConditionalIdx>,

    pub siblings: Vec<ConditionalIdx>,

    pub depth: u32,

    pub parent_ast_kind: Option<SyntaxKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalKind {
    If,
    ElsIf,
    Else,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalTree {
    conditionals: Arena<ConditionalData>,

    root_conditionals: Vec<ConditionalIdx>,

    position_map: FxHashMap<u32, ConditionalIdx>,
}

impl Default for ConditionalTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ConditionalTree {
    pub fn new() -> Self {
        Self {
            conditionals: Arena::new(),
            root_conditionals: Vec::new(),
            position_map: FxHashMap::default(),
        }
    }

    pub fn conditionals(&self) -> impl Iterator<Item = (ConditionalIdx, &ConditionalData)> {
        self.conditionals.iter()
    }

    pub fn len(&self) -> usize {
        self.conditionals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.conditionals.is_empty()
    }

    pub fn root_conditionals(&self) -> &[ConditionalIdx] {
        &self.root_conditionals
    }

    pub fn conditional(&self, idx: ConditionalIdx) -> &ConditionalData {
        &self.conditionals[idx]
    }

    pub fn parent(&self, idx: ConditionalIdx) -> Option<ConditionalIdx> {
        self.conditionals[idx].parent
    }

    pub fn children(&self, idx: ConditionalIdx) -> &[ConditionalIdx] {
        &self.conditionals[idx].children
    }

    pub fn conditional_at(&self, offset: text_size::TextSize) -> Option<ConditionalIdx> {
        let mut best: Option<(ConditionalIdx, u32)> = None;

        for (idx, cond) in self.conditionals.iter() {
            if cond.range.contains(offset) {
                match best {
                    None => best = Some((idx, cond.depth)),
                    Some((_, best_depth)) if cond.depth > best_depth => {
                        best = Some((idx, cond.depth));
                    }
                    _ => {}
                }
            }
        }

        best.map(|(idx, _)| idx)
    }

    pub fn conditional_containing(&self, range: TextRange) -> Option<ConditionalIdx> {
        let mut best: Option<(ConditionalIdx, u32)> = None;

        for (idx, cond) in self.conditionals.iter() {
            if cond.range.contains_range(range) {
                match best {
                    None => best = Some((idx, cond.depth)),
                    Some((_, best_depth)) if cond.depth > best_depth => {
                        best = Some((idx, cond.depth));
                    }
                    _ => {}
                }
            }
        }

        best.map(|(idx, _)| idx)
    }

    pub fn is_inside_conditional(&self, offset: text_size::TextSize) -> bool {
        self.conditional_at(offset).is_some()
    }

    pub fn main_if_branch(&self, idx: ConditionalIdx) -> Option<ConditionalIdx> {
        let cond = &self.conditionals[idx];
        match cond.kind {
            ConditionalKind::If => None,
            ConditionalKind::ElsIf | ConditionalKind::Else => {
                for (candidate_idx, candidate) in self.conditionals.iter() {
                    if candidate.kind == ConditionalKind::If && candidate.siblings.contains(&idx) {
                        return Some(candidate_idx);
                    }
                }
                None
            }
        }
    }

    pub fn all_branches(&self, idx: ConditionalIdx) -> Vec<ConditionalIdx> {
        let main_if = self.main_if_branch(idx).unwrap_or(idx);
        let main = &self.conditionals[main_if];

        let mut branches = vec![main_if];
        branches.extend(main.siblings.iter().copied());
        branches
    }
}

struct ConditionalTreeBuilder {
    tree: ConditionalTree,
    parent_stack: Vec<ConditionalIdx>,
    /// Where each torn group ends, by the start of its `#Если` part.
    torn_ends: FxHashMap<u32, text_size::TextSize>,
}

impl ConditionalTreeBuilder {
    fn new() -> Self {
        Self {
            tree: ConditionalTree::new(),
            parent_stack: Vec::new(),
            torn_ends: FxHashMap::default(),
        }
    }

    fn build(mut self, root: &SyntaxNode) -> ConditionalTree {
        self.torn_ends = torn_group_ends(root);
        self.collect_conditionals(root);
        self.tree
    }

    /// A group that tears a construct apart is read as one branch, kept as
    /// code between two opaque parts, and that branch is where the group's
    /// condition holds: the conditional covers both parts and the branch.
    ///
    /// Only a group read as its first branch gets its header's condition. A
    /// later branch holds where the earlier conditions fail, which a single
    /// node cannot state; it gets no condition, which availability checks
    /// read as undecidable and skip.
    fn process_torn_if(&mut self, node: &SyntaxNode) {
        let Some(&end) = self.torn_ends.get(&u32::from(node.text_range().start())) else {
            return;
        };
        let directives = node
            .descendants_with_tokens()
            .filter(|element| {
                matches!(
                    element.kind(),
                    SyntaxKind::PRE_IF
                        | SyntaxKind::PRE_ELSIF
                        | SyntaxKind::PRE_ELSE
                        | SyntaxKind::PRE_END_IF
                )
            })
            .count();
        let (condition_text, condition_range) = if directives == 1 {
            let (text, range) = self.extract_condition(node);
            (Some(text), Some(range))
        } else {
            (None, None)
        };

        let range = TextRange::new(node.text_range().start(), end);
        let parent = self.parent_stack.last().copied();
        let idx = self.tree.conditionals.alloc(ConditionalData {
            kind: ConditionalKind::If,
            condition_text,
            range,
            directive_range: self.find_first_line_range(node),
            condition_range,
            parent,
            children: Vec::new(),
            siblings: Vec::new(),
            depth: self.parent_stack.len() as u32,
            parent_ast_kind: find_parent_ast_kind(node),
        });
        self.tree.position_map.insert(range.start().into(), idx);
        if let Some(parent_idx) = parent {
            self.tree.conditionals[parent_idx].children.push(idx);
        } else {
            self.tree.root_conditionals.push(idx);
        }
    }

    fn collect_conditionals(&mut self, node: &SyntaxNode) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::PRE_IF_DIR => {
                    self.process_if_directive(&child);
                }
                SyntaxKind::PRE_TORN_IF_PART => self.process_torn_if(&child),
                _ => {
                    self.collect_conditionals(&child);
                }
            }
        }
    }

    fn process_if_directive(&mut self, node: &SyntaxNode) {
        let (condition_text, condition_range) = self.extract_condition(node);

        let range = node.text_range();
        let directive_range = self.find_first_line_range(node);
        let parent = self.parent_stack.last().copied();
        let depth = self.parent_stack.len() as u32;
        let parent_ast_kind = find_parent_ast_kind(node);

        let if_idx = self.tree.conditionals.alloc(ConditionalData {
            kind: ConditionalKind::If,
            condition_text: Some(condition_text),
            range,
            directive_range,
            condition_range: Some(condition_range),
            parent,
            children: Vec::new(),
            siblings: Vec::new(),
            depth,
            parent_ast_kind,
        });

        self.tree.position_map.insert(range.start().into(), if_idx);

        if let Some(parent_idx) = parent {
            self.tree.conditionals[parent_idx].children.push(if_idx);
        } else {
            self.tree.root_conditionals.push(if_idx);
        }

        let mut siblings = Vec::new();
        for elsif in node.children().filter(|n| n.kind() == SyntaxKind::PRE_ELSIF_CLAUSE) {
            let elsif_idx = self.process_elsif_clause(&elsif, if_idx, depth);
            siblings.push(elsif_idx);
        }

        for else_clause in node.children().filter(|n| n.kind() == SyntaxKind::PRE_ELSE_CLAUSE) {
            let else_idx = self.process_else_clause(&else_clause, if_idx, depth);
            siblings.push(else_idx);
        }

        self.tree.conditionals[if_idx].siblings = siblings;

        // Only the then-branch subtree belongs to the `If` itself: ElsIf/Else
        // clauses have already collected their nested directives under their
        // own parent, and descending into them again would duplicate every
        // nested conditional once per nesting level.
        self.parent_stack.push(if_idx);
        for child in node.children() {
            match child.kind() {
                SyntaxKind::PRE_ELSIF_CLAUSE | SyntaxKind::PRE_ELSE_CLAUSE => {}
                SyntaxKind::PRE_IF_DIR => self.process_if_directive(&child),
                SyntaxKind::PRE_TORN_IF_PART => self.process_torn_if(&child),
                _ => self.collect_nested_conditionals(&child),
            }
        }
        self.parent_stack.pop();
    }

    fn process_elsif_clause(
        &mut self,
        node: &SyntaxNode,
        parent_if: ConditionalIdx,
        depth: u32,
    ) -> ConditionalIdx {
        let (condition_text, condition_range) = self.extract_condition(node);
        let range = node.text_range();
        let directive_range = self.find_first_line_range(node);
        let parent_ast_kind = find_parent_ast_kind(node);

        let idx = self.tree.conditionals.alloc(ConditionalData {
            kind: ConditionalKind::ElsIf,
            condition_text: Some(condition_text),
            range,
            directive_range,
            condition_range: Some(condition_range),
            parent: Some(parent_if),
            children: Vec::new(),
            siblings: Vec::new(),
            depth,
            parent_ast_kind,
        });

        self.tree.position_map.insert(range.start().into(), idx);

        self.parent_stack.push(idx);
        self.collect_nested_conditionals(node);
        self.parent_stack.pop();

        idx
    }

    fn process_else_clause(
        &mut self,
        node: &SyntaxNode,
        parent_if: ConditionalIdx,
        depth: u32,
    ) -> ConditionalIdx {
        let range = node.text_range();
        let directive_range = self.find_first_line_range(node);
        let parent_ast_kind = find_parent_ast_kind(node);

        let idx = self.tree.conditionals.alloc(ConditionalData {
            kind: ConditionalKind::Else,
            condition_text: None,
            range,
            directive_range,
            condition_range: None,
            parent: Some(parent_if),
            children: Vec::new(),
            siblings: Vec::new(),
            depth,
            parent_ast_kind,
        });

        self.tree.position_map.insert(range.start().into(), idx);

        self.parent_stack.push(idx);
        self.collect_nested_conditionals(node);
        self.parent_stack.pop();

        idx
    }

    fn collect_nested_conditionals(&mut self, parent_node: &SyntaxNode) {
        for child in parent_node.children() {
            match child.kind() {
                SyntaxKind::PRE_IF_DIR => {
                    self.process_if_directive(&child);
                }
                SyntaxKind::PRE_TORN_IF_PART => self.process_torn_if(&child),
                _ => {
                    self.collect_nested_conditionals(&child);
                }
            }
        }
    }

    fn extract_condition(&self, node: &SyntaxNode) -> (String, TextRange) {
        let text = node.text().to_string();
        let first_line = text.lines().next().unwrap_or(&text);

        // The shared header parser strips the directive keyword and the
        // trailing `Тогда` in any case and around any whitespace; fall back
        // to the raw line for malformed headers so the display text stays
        // non-empty (such a condition never parses as a valid one anyway).
        let condition = crate::preproc_condition::extract_condition_text(first_line)
            .unwrap_or(first_line.trim())
            .to_string();

        let condition_range = node
            .children()
            .find(|child| {
                matches!(child.kind(), SyntaxKind::PRE_EXPR | SyntaxKind::PRE_LOGICAL_EXPR)
            })
            .map(|child| child.text_range())
            .unwrap_or_else(|| {
                let start_offset = first_line.find(condition.as_str()).unwrap_or(0);
                let start =
                    node.text_range().start() + text_size::TextSize::from(start_offset as u32);
                let end = start + text_size::TextSize::from(condition.len() as u32);
                TextRange::new(start, end)
            });

        (condition, condition_range)
    }

    fn find_first_line_range(&self, node: &SyntaxNode) -> TextRange {
        let text = node.text().to_string();
        let first_line_len = text.lines().next().map(|l| l.len()).unwrap_or(0);
        TextRange::new(
            node.text_range().start(),
            node.text_range().start() + text_size::TextSize::from(first_line_len as u32),
        )
    }
}

/// The end of every torn group, by the start of its `#Если` part.
///
/// The parts of one group are not linked in the tree: the `#Если` part stands
/// where the group opened and the part with its other directives where the
/// branch read ended, possibly in another construct. Groups nest in the text,
/// so the parts pair up in document order. A `#Если` part that ends with the
/// group's `#КонецЕсли` read no branch and is a group by itself.
fn torn_group_ends(root: &SyntaxNode) -> FxHashMap<u32, text_size::TextSize> {
    let mut ends = FxHashMap::default();
    let mut open = Vec::new();
    for node in root.descendants().filter(|node| node.kind() == SyntaxKind::PRE_TORN_IF_PART) {
        let range = node.text_range();
        let first = node.first_token().map(|token| token.kind());
        let last = node.last_token().map(|token| token.kind());
        if first == Some(SyntaxKind::PRE_IF) {
            if last == Some(SyntaxKind::PRE_END_IF) {
                ends.insert(u32::from(range.start()), range.end());
            } else {
                open.push(range.start());
            }
        } else if let Some(start) = open.pop() {
            ends.insert(u32::from(start), range.end());
        }
    }
    ends
}

fn find_parent_ast_kind(directive_node: &SyntaxNode) -> Option<SyntaxKind> {
    let mut parent = directive_node.parent();
    while let Some(node) = parent {
        match node.kind() {
            SyntaxKind::BINARY_EXPR
            | SyntaxKind::CALL_EXPR
            | SyntaxKind::INDEX_EXPR
            | SyntaxKind::TERNARY_EXPR
            | SyntaxKind::UNARY_EXPR => return Some(node.kind()),

            SyntaxKind::IF_STMT
            | SyntaxKind::WHILE_STMT
            | SyntaxKind::FOR_STMT
            | SyntaxKind::FOR_EACH_STMT
            | SyntaxKind::TRY_STMT
            | SyntaxKind::ASSIGN_STMT
            | SyntaxKind::CALL_STMT => return Some(node.kind()),

            SyntaxKind::PROCEDURE_DEF
            | SyntaxKind::FUNCTION_DEF
            | SyntaxKind::PARAM_LIST
            | SyntaxKind::VAR_DEF => return Some(node.kind()),

            SyntaxKind::SOURCE_FILE | SyntaxKind::STMT_LIST => return None,

            SyntaxKind::PRE_IF_DIR
            | SyntaxKind::PRE_ELSIF_CLAUSE
            | SyntaxKind::PRE_ELSE_CLAUSE
            | SyntaxKind::PRE_REGION_DIR => {
                parent = node.parent();
            }

            _ => parent = node.parent(),
        }
    }
    None
}

pub fn lower_conditionals(root: &SyntaxNode) -> ConditionalTree {
    ConditionalTreeBuilder::new().build(root)
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the conditional
/// arena (one [`ConditionalData`] per node plus each node's `condition_text` string
/// and its `children`/`siblings` vectors), the `root_conditionals` vector, and the
/// `position_map` table.
fn conditional_tree_heap(v: &std::sync::Arc<ConditionalTree>) -> usize {
    use crate::heap_estimate::{map_table_bytes, vec_bytes};

    let t = &**v;
    let mut bytes = std::mem::size_of::<ConditionalTree>();
    bytes += vec_bytes::<ConditionalData>(t.conditionals.len());
    for (_, data) in t.conditionals.iter() {
        if let Some(text) = &data.condition_text {
            bytes += text.capacity();
        }
        bytes += vec_bytes::<ConditionalIdx>(data.children.len());
        bytes += vec_bytes::<ConditionalIdx>(data.siblings.len());
    }
    bytes += vec_bytes::<ConditionalIdx>(t.root_conditionals.len());
    bytes += map_table_bytes::<u32, ConditionalIdx>(t.position_map.len());
    bytes
}

#[salsa::tracked(lru = 256, heap_size = conditional_tree_heap, returns(ref))]
pub fn conditional_tree_query<'db>(
    db: &'db dyn base_db::RootQueryDb,
    file_id_input: base_db::FileIdInput<'db>,
) -> std::sync::Arc<ConditionalTree> {
    let _span = tracing::info_span!("conditional_tree", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let parse = db.parse_ref(file_id);
    std::sync::Arc::new(lower_conditionals(&parse.syntax_node()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_lower(code: &str) -> ConditionalTree {
        let parse = parser::parse(code);
        lower_conditionals(&parse.syntax_node())
    }

    #[test]
    fn test_empty_file() {
        let tree = parse_and_lower("");
        assert!(tree.is_empty());
        assert_eq!(tree.root_conditionals().len(), 0);
    }

    #[test]
    fn nested_conditional_in_elsif_clause_lowered_once() {
        let code = r#"
#Если Клиент Тогда
Процедура А()
КонецПроцедуры
#ИначеЕсли Сервер Тогда
#Если НЕ ВебКлиент Тогда
Процедура Б()
КонецПроцедуры
#КонецЕсли
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        // if + elsif + one nested if; a duplicate of the nested node would
        // both inflate the arena and hang it under the wrong parent.
        assert_eq!(tree.len(), 3);
        let nested: Vec<_> = tree
            .conditionals()
            .filter(|(_, data)| data.condition_text.as_deref() == Some("НЕ ВебКлиент"))
            .collect();
        assert_eq!(nested.len(), 1);
        let (_, nested_data) = nested[0];
        let parent = tree.conditional(nested_data.parent.expect("nested must have a parent"));
        assert_eq!(parent.kind, ConditionalKind::ElsIf);
    }

    #[test]
    fn condition_text_strips_keyword_in_any_case() {
        let tree = parse_and_lower("#ЕСЛИ ВебКлиент ТОГДА\n#КонецЕсли\n");
        assert_eq!(tree.len(), 1);
        let cond = tree.conditional(tree.root_conditionals()[0]);
        assert_eq!(cond.condition_text.as_deref(), Some("ВебКлиент"));
    }

    #[test]
    fn test_single_if() {
        let code = r#"
#Если Клиент Тогда
Процедура Тест()
КонецПроцедуры
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.root_conditionals().len(), 1);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        assert_eq!(cond.kind, ConditionalKind::If);
        assert_eq!(cond.condition_text.as_ref().unwrap(), "Клиент");
        assert_eq!(cond.depth, 0);
        assert!(cond.parent.is_none());
        assert!(cond.children.is_empty());
        assert!(cond.siblings.is_empty());
    }

    #[test]
    fn test_if_elsif_else_chain() {
        let code = r#"
#Если Клиент Тогда
    Сообщить("Клиент");
#ИначеЕсли Сервер Тогда
    Сообщить("Сервер");
#Иначе
    Сообщить("Другое");
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 3, "Should have If + ElsIf + Else");
        assert_eq!(tree.root_conditionals().len(), 1);

        let if_idx = tree.root_conditionals()[0];
        let if_cond = tree.conditional(if_idx);
        assert_eq!(if_cond.kind, ConditionalKind::If);
        assert_eq!(if_cond.condition_text.as_ref().unwrap(), "Клиент");
        assert_eq!(if_cond.siblings.len(), 2, "Should have 2 siblings (ElsIf + Else)");

        let elsif_idx = if_cond.siblings[0];
        let elsif_cond = tree.conditional(elsif_idx);
        assert_eq!(elsif_cond.kind, ConditionalKind::ElsIf);
        assert_eq!(elsif_cond.condition_text.as_ref().unwrap(), "Сервер");

        let else_idx = if_cond.siblings[1];
        let else_cond = tree.conditional(else_idx);
        assert_eq!(else_cond.kind, ConditionalKind::Else);
        assert!(else_cond.condition_text.is_none());

        assert_eq!(tree.main_if_branch(elsif_idx), Some(if_idx));
        assert_eq!(tree.main_if_branch(else_idx), Some(if_idx));
        assert_eq!(tree.main_if_branch(if_idx), None);

        let branches = tree.all_branches(elsif_idx);
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0], if_idx);
        assert_eq!(branches[1], elsif_idx);
        assert_eq!(branches[2], else_idx);
    }

    #[test]
    fn test_nested_conditionals() {
        let code = r#"
#Если Клиент Тогда
    #Если ВебКлиент Тогда
        Сообщить("Веб");
    #КонецЕсли
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 2);
        assert_eq!(tree.root_conditionals().len(), 1);

        let outer_idx = tree.root_conditionals()[0];
        let outer = tree.conditional(outer_idx);
        assert_eq!(outer.depth, 0);
        assert_eq!(outer.children.len(), 1);

        let inner_idx = outer.children[0];
        let inner = tree.conditional(inner_idx);
        assert_eq!(inner.depth, 1);
        assert_eq!(inner.parent, Some(outer_idx));
        assert_eq!(inner.condition_text.as_ref().unwrap(), "ВебКлиент");
    }

    #[test]
    fn test_conditionals_inside_regions() {
        let code = r#"
#Область ПрограммныйИнтерфейс

#Если Клиент Тогда
Функция Тест() Экспорт
КонецФункции
#КонецЕсли

#КонецОбласти
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.root_conditionals().len(), 1);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        assert_eq!(cond.condition_text.as_ref().unwrap(), "Клиент");
    }

    #[test]
    fn test_position_based_lookup() {
        let code = r#"
#Если Клиент Тогда
Процедура Тест()
КонецПроцедуры
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        let inside_pos = text_size::TextSize::from(30);
        assert!(tree.conditional_at(inside_pos).is_some());

        let before_pos = text_size::TextSize::from(0);
        assert!(tree.conditional_at(before_pos).is_none());
    }

    #[test]
    fn test_english_keywords() {
        let code = r#"
#If Client Then
    Message("Client");
#ElsIf Server Then
    Message("Server");
#Else
    Message("Other");
#EndIf
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 3);

        let if_idx = tree.root_conditionals()[0];
        let if_cond = tree.conditional(if_idx);
        assert_eq!(if_cond.condition_text.as_ref().unwrap(), "Client");

        let elsif_idx = if_cond.siblings[0];
        let elsif_cond = tree.conditional(elsif_idx);
        assert_eq!(elsif_cond.condition_text.as_ref().unwrap(), "Server");
    }

    #[test]
    fn test_parent_ast_kind_binary_expr() {
        let code = r#"
Процедура Тест()
    а = 1
#Если Клиент Тогда
        + 2
#КонецЕсли
        ;
КонецПроцедуры
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 1);
        let _cond = tree.conditional(tree.root_conditionals()[0]);
    }

    #[test]
    fn torn_group_covers_the_branch_read() {
        let code = r#"
Процедура Тест()
    Если Ложь
        #Если Клиент Тогда
        Или Прочитанное
        #Иначе
        Или Пропущенное
        #КонецЕсли
    Тогда
    КонецЕсли;
    #Если Не Клиент Тогда
    Если Пропущенное
    #Иначе
    Если Прочитанное
    #КонецЕсли
    Тогда
    КонецЕсли;
КонецПроцедуры
"#;
        let tree = parse_and_lower(code);
        assert_eq!(tree.len(), 2);
        let roots = tree.root_conditionals();
        let first = tree.conditional(roots[0]);
        assert_eq!(first.condition_text.as_deref(), Some("Клиент"));
        let read = code.find("Прочитанное").unwrap() as u32;
        assert!(first.range.contains(read.into()));
        assert!(
            first.range.end() > text_size::TextSize::from(code.find("Пропущенное").unwrap() as u32)
        );
        // Read as its `#Иначе` branch: no single condition states where it holds.
        let second = tree.conditional(roots[1]);
        assert_eq!(second.condition_text, None);
        let read_second = code.rfind("Прочитанное").unwrap() as u32;
        assert!(second.range.contains(read_second.into()));
    }

    #[test]
    fn test_parent_ast_kind_valid_stmt_list() {
        let code = r#"
Процедура Тест()
    Сообщить("До");
#Если Клиент Тогда
    Сообщить("Клиент");
#КонецЕсли
    Сообщить("После");
КонецПроцедуры
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 1);
        let cond = tree.conditional(tree.root_conditionals()[0]);

        assert!(
            cond.parent_ast_kind.is_none(),
            "Directive in STMT_LIST should have no parent_ast_kind (valid placement)"
        );
    }

    #[test]
    fn test_parent_ast_kind_top_level() {
        let code = r#"
#Если Клиент Тогда
Процедура КлиентскаяПроцедура()
КонецПроцедуры
#КонецЕсли

#Если Сервер Тогда
Процедура СерверПроцедура()
КонецПроцедуры
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 2);

        for idx in tree.root_conditionals() {
            let cond = tree.conditional(*idx);
            assert!(
                cond.parent_ast_kind.is_none(),
                "Top-level directive should have no parent_ast_kind"
            );
        }
    }

    #[test]
    fn test_platform_context_client() {
        let code = r#"
#Если Клиент Тогда
    Сообщить("Клиент");
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 1);
        let cond = tree.conditional(tree.root_conditionals()[0]);
        let condition_text = cond.condition_text.as_ref().unwrap();

        assert_eq!(condition_text, "Клиент");
        assert!(
            condition_text.to_lowercase().contains("клиент"),
            "Should contain 'клиент' keyword"
        );
    }

    #[test]
    fn test_platform_context_server() {
        let code = r#"
#Если Сервер Тогда
    Сообщить("Сервер");
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        let condition_text = cond.condition_text.as_ref().unwrap();

        assert_eq!(condition_text, "Сервер");
        assert!(
            condition_text.to_lowercase().contains("сервер"),
            "Should contain 'сервер' keyword"
        );
    }

    #[test]
    fn test_platform_context_thin_client() {
        let code = r#"
#Если ТонкийКлиент Тогда
    Сообщить("ТонкийКлиент");
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        let condition_text = cond.condition_text.as_ref().unwrap();

        assert_eq!(condition_text, "ТонкийКлиент");
    }

    #[test]
    fn test_platform_context_complex_condition() {
        let code = r#"
#Если НЕ Сервер ИЛИ МобильныйКлиент Тогда
    Сообщить("Сложное условие");
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        let condition_text = cond.condition_text.as_ref().unwrap();

        assert!(condition_text.contains("Сервер"), "Should contain 'Сервер'");
        assert!(condition_text.contains("МобильныйКлиент"), "Should contain 'МобильныйКлиент'");

        let lower = condition_text.to_lowercase();
        assert!(lower.contains("не") || lower.contains("или"), "Should contain logical operators");
    }

    #[test]
    fn test_platform_context_english() {
        let code = r#"
#If Client OR Server Then
    Message("Client or Server");
#EndIf
"#;
        let tree = parse_and_lower(code);

        let cond = tree.conditional(tree.root_conditionals()[0]);
        let condition_text = cond.condition_text.as_ref().unwrap();

        let lower = condition_text.to_lowercase();
        assert!(
            lower.contains("client") || lower.contains("server"),
            "Should contain platform keywords in English"
        );
    }

    #[test]
    fn test_condition_range() {
        let code = r#"
#Если Клиент Тогда
КонецФункции
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        let cond = tree.conditional(tree.root_conditionals()[0]);

        assert!(cond.condition_range.is_some(), "If branch should have condition_range");

        let code_with_else = r#"
#Если Клиент Тогда
    А = 1;
#Иначе
    А = 2;
#КонецЕсли
"#;
        let tree2 = parse_and_lower(code_with_else);
        let if_cond = tree2.conditional(tree2.root_conditionals()[0]);
        let else_idx = if_cond.siblings[0];
        let else_cond = tree2.conditional(else_idx);

        assert!(else_cond.condition_range.is_none(), "Else branch should not have condition_range");
    }

    #[test]
    fn test_multilevel_nesting() {
        let code = r#"
#Если Клиент Тогда
    #Если ТонкийКлиент Тогда
        #Если ВебКлиент Тогда
            Сообщить("ВебКлиент");
        #КонецЕсли
    #КонецЕсли
#КонецЕсли
"#;
        let tree = parse_and_lower(code);

        assert_eq!(tree.len(), 3);

        let level0_idx = tree.root_conditionals()[0];
        let level0 = tree.conditional(level0_idx);
        assert_eq!(level0.depth, 0);
        assert_eq!(level0.children.len(), 1);

        let level1_idx = level0.children[0];
        let level1 = tree.conditional(level1_idx);
        assert_eq!(level1.depth, 1);
        assert_eq!(level1.parent, Some(level0_idx));
        assert_eq!(level1.children.len(), 1);

        let level2_idx = level1.children[0];
        let level2 = tree.conditional(level2_idx);
        assert_eq!(level2.depth, 2);
        assert_eq!(level2.parent, Some(level1_idx));
    }
}
