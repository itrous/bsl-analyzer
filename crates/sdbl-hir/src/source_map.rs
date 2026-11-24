use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use syntax::SyntaxKind;
use text_size::TextRange;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdblSourceMap {
    metadata_references: Vec<crate::metadata_reference::MetadataReference>,
    metadata_reference_issues: Vec<crate::metadata_reference::MetadataReferenceIssue>,
    pub(crate) clause_keywords: Vec<TokenInfo>,

    pub(crate) operators: Vec<TokenInfo>,

    pub(crate) special_keywords: Vec<TokenInfo>,

    pub(crate) join_keywords: Vec<TokenInfo>,

    pub(crate) modifiers: Vec<TokenInfo>,

    pub(crate) aggregate_functions: Vec<TokenInfo>,

    pub(crate) builtin_functions: Vec<TokenInfo>,

    pub(crate) mdo_types: Vec<TokenInfo>,

    pub(crate) table_names: Vec<TokenInfo>,

    pub(crate) unresolved_table_names: Vec<TokenInfo>,

    pub(crate) table_aliases: Vec<TokenInfo>,

    pub(crate) field_names: Vec<TokenInfo>,

    pub(crate) unresolved_field_names: Vec<TokenInfo>,

    pub(crate) field_aliases: Vec<TokenInfo>,

    range_to_category: FxHashMap<TextRange, TokenCategory>,
}

impl SdblSourceMap {
    pub fn metadata_references(&self) -> &[crate::metadata_reference::MetadataReference] {
        &self.metadata_references
    }
    pub fn metadata_reference_issues(
        &self,
    ) -> &[crate::metadata_reference::MetadataReferenceIssue] {
        &self.metadata_reference_issues
    }
    pub(crate) fn record_metadata_reference(
        &mut self,
        reference: crate::metadata_reference::MetadataReference,
    ) {
        self.metadata_references.push(reference);
    }
    pub(crate) fn record_metadata_columns(
        &mut self,
        references: Vec<crate::metadata_reference::MetadataReference>,
        issues: Vec<crate::metadata_reference::MetadataReferenceIssue>,
    ) {
        self.metadata_references.extend(references);
        self.metadata_reference_issues.extend(issues);
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn all_tokens(&self) -> impl Iterator<Item = (&TokenInfo, TokenCategory)> {
        let clause_iter = self.clause_keywords.iter().map(|t| (t, TokenCategory::ClauseKeyword));
        let op_iter = self.operators.iter().map(|t| (t, TokenCategory::Operator));
        let special_iter = self.special_keywords.iter().map(|t| (t, TokenCategory::SpecialKeyword));
        let join_iter = self.join_keywords.iter().map(|t| (t, TokenCategory::JoinKeyword));
        let modifier_iter = self.modifiers.iter().map(|t| (t, TokenCategory::Modifier));
        let agg_iter =
            self.aggregate_functions.iter().map(|t| (t, TokenCategory::AggregateFunction));
        let builtin_iter =
            self.builtin_functions.iter().map(|t| (t, TokenCategory::BuiltinFunction));

        let table_name_iter = self.table_names.iter().map(|t| (t, TokenCategory::TableName));
        let unresolved_table_iter =
            self.unresolved_table_names.iter().map(|t| (t, TokenCategory::UnresolvedTableName));
        let table_alias_iter = self.table_aliases.iter().map(|t| (t, TokenCategory::TableAlias));
        let field_name_iter = self.field_names.iter().map(|t| (t, TokenCategory::FieldName));
        let unresolved_field_iter =
            self.unresolved_field_names.iter().map(|t| (t, TokenCategory::UnresolvedFieldName));
        let field_alias_iter = self.field_aliases.iter().map(|t| (t, TokenCategory::FieldAlias));

        clause_iter
            .chain(op_iter)
            .chain(special_iter)
            .chain(join_iter)
            .chain(modifier_iter)
            .chain(agg_iter)
            .chain(builtin_iter)
            .chain(table_name_iter)
            .chain(unresolved_table_iter)
            .chain(table_alias_iter)
            .chain(field_name_iter)
            .chain(unresolved_field_iter)
            .chain(field_alias_iter)
    }

    pub fn token_at_position(
        &self,
        offset: text_size::TextSize,
    ) -> Option<(&TokenInfo, TokenCategory)> {
        self.all_tokens().find(|(info, _)| info.range.contains(offset))
    }

    pub fn token_at_range(&self, range: TextRange) -> Option<(&TokenInfo, TokenCategory)> {
        self.range_to_category.get(&range).and_then(|cat| {
            let tokens = self.tokens_by_category(*cat);
            tokens.iter().find(|info| info.range == range).map(|t| (t, *cat))
        })
    }

    pub fn tokens_by_category(&self, category: TokenCategory) -> &[TokenInfo] {
        match category {
            TokenCategory::ClauseKeyword => &self.clause_keywords,
            TokenCategory::Operator => &self.operators,
            TokenCategory::SpecialKeyword => &self.special_keywords,
            TokenCategory::JoinKeyword => &self.join_keywords,
            TokenCategory::Modifier => &self.modifiers,
            TokenCategory::AggregateFunction => &self.aggregate_functions,
            TokenCategory::BuiltinFunction => &self.builtin_functions,
            TokenCategory::MdoType => &self.mdo_types,
            TokenCategory::TableName => &self.table_names,
            TokenCategory::UnresolvedTableName => &self.unresolved_table_names,
            TokenCategory::TableAlias => &self.table_aliases,
            TokenCategory::FieldName => &self.field_names,
            TokenCategory::UnresolvedFieldName => &self.unresolved_field_names,
            TokenCategory::FieldAlias => &self.field_aliases,
        }
    }

    pub(crate) fn add_token(&mut self, info: TokenInfo, category: TokenCategory) {
        self.range_to_category.insert(info.range, category);

        match category {
            TokenCategory::ClauseKeyword => self.clause_keywords.push(info),
            TokenCategory::Operator => self.operators.push(info),
            TokenCategory::SpecialKeyword => self.special_keywords.push(info),
            TokenCategory::JoinKeyword => self.join_keywords.push(info),
            TokenCategory::Modifier => self.modifiers.push(info),
            TokenCategory::AggregateFunction => self.aggregate_functions.push(info),
            TokenCategory::BuiltinFunction => self.builtin_functions.push(info),
            TokenCategory::MdoType => self.mdo_types.push(info),
            TokenCategory::TableName => self.table_names.push(info),
            TokenCategory::UnresolvedTableName => self.unresolved_table_names.push(info),
            TokenCategory::TableAlias => self.table_aliases.push(info),
            TokenCategory::FieldName => self.field_names.push(info),
            TokenCategory::UnresolvedFieldName => self.unresolved_field_names.push(info),
            TokenCategory::FieldAlias => self.field_aliases.push(info),
        }
    }

    /// Approximate live heap bytes for Salsa's `memory_usage` report: the fourteen
    /// per-category `Vec<TokenInfo>` backing stores (one element each, plus any
    /// non-inlined `SmolStr` token text) and the `range_to_category` hashbrown
    /// table. Spare capacity is ignored, so the figure tracks live content within a
    /// small factor.
    pub fn estimated_heap(&self) -> usize {
        use std::mem::size_of;

        let token_vecs: [&Vec<TokenInfo>; 14] = [
            &self.clause_keywords,
            &self.operators,
            &self.special_keywords,
            &self.join_keywords,
            &self.modifiers,
            &self.aggregate_functions,
            &self.builtin_functions,
            &self.mdo_types,
            &self.table_names,
            &self.unresolved_table_names,
            &self.table_aliases,
            &self.field_names,
            &self.unresolved_field_names,
            &self.field_aliases,
        ];

        let mut bytes = 0;
        for vec in token_vecs {
            bytes += vec.len() * size_of::<TokenInfo>();
            for token in vec {
                bytes += smol_str_heap(&token.text);
            }
        }

        let len = self.range_to_category.len();
        if len != 0 {
            let cap = (len * 8 / 7 + 1).checked_next_power_of_two().unwrap_or(len);
            bytes += cap * (size_of::<TextRange>() + size_of::<TokenCategory>() + 1);
        }

        bytes += stdx::heap::vec_bytes::<crate::metadata_reference::MetadataReference>(
            self.metadata_references.len(),
        );
        bytes +=
            self.metadata_references
                .iter()
                .map(|reference| {
                    reference.owner_name.capacity()
                        + stdx::heap::vec_bytes::<
                            bsl_metadata::declaration_identity::DeclarationSegment,
                        >(reference.members.len())
                        + reference
                            .members
                            .iter()
                            .map(|member| member.kind.capacity() + member.name.capacity())
                            .sum::<usize>()
                })
                .sum::<usize>();
        bytes += stdx::heap::vec_bytes::<crate::metadata_reference::MetadataReferenceIssue>(
            self.metadata_reference_issues.len(),
        );
        bytes
    }

    pub(crate) fn finalize(&mut self) {
        self.metadata_references.sort_by_key(|reference| reference.range.start());
        self.metadata_reference_issues.sort_by_key(|issue| issue.range.start());
        self.clause_keywords.sort_by_key(|t| t.range.start());
        self.operators.sort_by_key(|t| t.range.start());
        self.special_keywords.sort_by_key(|t| t.range.start());
        self.join_keywords.sort_by_key(|t| t.range.start());
        self.modifiers.sort_by_key(|t| t.range.start());
        self.aggregate_functions.sort_by_key(|t| t.range.start());
        self.builtin_functions.sort_by_key(|t| t.range.start());
        self.table_names.sort_by_key(|t| t.range.start());
        self.unresolved_table_names.sort_by_key(|t| t.range.start());
        self.table_aliases.sort_by_key(|t| t.range.start());
        self.field_names.sort_by_key(|t| t.range.start());
        self.unresolved_field_names.sort_by_key(|t| t.range.start());
        self.field_aliases.sort_by_key(|t| t.range.start());
    }
}

/// Heap bytes owned by a `SmolStr`: zero while it fits inline (≤ 22 bytes),
/// its full length otherwise.
fn smol_str_heap(s: &SmolStr) -> usize {
    let len = s.len();
    if len > 22 {
        len
    } else {
        0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub range: TextRange,

    pub kind: SyntaxKind,

    pub text: SmolStr,
}

impl TokenInfo {
    pub fn new(range: TextRange, kind: SyntaxKind, text: impl Into<SmolStr>) -> Self {
        Self { range, kind, text: text.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenCategory {
    ClauseKeyword,
    Operator,
    SpecialKeyword,
    JoinKeyword,
    Modifier,
    AggregateFunction,
    BuiltinFunction,

    MdoType,
    TableName,
    UnresolvedTableName,
    TableAlias,
    FieldName,
    UnresolvedFieldName,
    FieldAlias,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_map_empty() {
        let map = SdblSourceMap::new();
        assert_eq!(map.clause_keywords.len(), 0);
        assert_eq!(map.operators.len(), 0);
        assert_eq!(map.all_tokens().count(), 0);
    }

    #[test]
    fn test_add_token() {
        let mut map = SdblSourceMap::new();

        let token = TokenInfo::new(TextRange::new(0.into(), 6.into()), SyntaxKind::IDENT, "SELECT");
        map.add_token(token.clone(), TokenCategory::ClauseKeyword);

        assert_eq!(map.clause_keywords.len(), 1);
        assert_eq!(map.clause_keywords[0].text, "SELECT");
        assert_eq!(map.all_tokens().count(), 1);
    }

    #[test]
    fn test_token_at_position() {
        let mut map = SdblSourceMap::new();

        let token = TokenInfo::new(TextRange::new(0.into(), 6.into()), SyntaxKind::IDENT, "SELECT");
        map.add_token(token, TokenCategory::ClauseKeyword);

        let result = map.token_at_position(3.into());
        assert!(result.is_some());
        let (info, category) = result.unwrap();
        assert_eq!(info.text, "SELECT");
        assert_eq!(category, TokenCategory::ClauseKeyword);

        let result = map.token_at_position(10.into());
        assert!(result.is_none());
    }

    #[test]
    fn test_token_at_range() {
        let mut map = SdblSourceMap::new();

        let range = TextRange::new(0.into(), 6.into());
        let token = TokenInfo::new(range, SyntaxKind::IDENT, "SELECT");
        map.add_token(token, TokenCategory::ClauseKeyword);

        let result = map.token_at_range(range);
        assert!(result.is_some());
        let (info, category) = result.unwrap();
        assert_eq!(info.text, "SELECT");
        assert_eq!(category, TokenCategory::ClauseKeyword);
    }

    #[test]
    fn test_finalize_sorts_tokens() {
        let mut map = SdblSourceMap::new();

        map.add_token(
            TokenInfo::new(TextRange::new(20.into(), 25.into()), SyntaxKind::IDENT, "WHERE"),
            TokenCategory::ClauseKeyword,
        );
        map.add_token(
            TokenInfo::new(TextRange::new(0.into(), 6.into()), SyntaxKind::IDENT, "SELECT"),
            TokenCategory::ClauseKeyword,
        );
        map.add_token(
            TokenInfo::new(TextRange::new(10.into(), 14.into()), SyntaxKind::IDENT, "FROM"),
            TokenCategory::ClauseKeyword,
        );

        map.finalize();

        assert_eq!(map.clause_keywords[0].text, "SELECT");
        assert_eq!(map.clause_keywords[1].text, "FROM");
        assert_eq!(map.clause_keywords[2].text, "WHERE");
    }

    #[test]
    fn test_tokens_by_category() {
        let mut map = SdblSourceMap::new();

        map.add_token(
            TokenInfo::new(TextRange::new(0.into(), 6.into()), SyntaxKind::IDENT, "SELECT"),
            TokenCategory::ClauseKeyword,
        );
        map.add_token(
            TokenInfo::new(TextRange::new(20.into(), 21.into()), SyntaxKind::EQ, "="),
            TokenCategory::Operator,
        );

        let keywords = map.tokens_by_category(TokenCategory::ClauseKeyword);
        assert_eq!(keywords.len(), 1);
        assert_eq!(keywords[0].text, "SELECT");

        let operators = map.tokens_by_category(TokenCategory::Operator);
        assert_eq!(operators.len(), 1);
        assert_eq!(operators[0].text, "=");
    }
}
