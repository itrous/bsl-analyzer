use crate::{SyntaxKind, SyntaxNode, SyntaxToken};
use stdx::case::CaseExt;

pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(node: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

#[derive(Debug, Clone)]
pub struct SourceFile(SyntaxNode);

impl AstNode for SourceFile {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SOURCE_FILE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ProcedureDef(SyntaxNode);

impl AstNode for ProcedureDef {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PROCEDURE_DEF
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl ProcedureDef {
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT)
    }

    pub fn name_or_keyword(&self) -> Option<SyntaxToken> {
        self.0.children_with_tokens().filter_map(|it| it.into_token()).find(|it| {
            let k = it.kind();
            k == SyntaxKind::IDENT
                || (k.is_keyword()
                    && !matches!(
                        k,
                        SyntaxKind::KW_PROCEDURE
                            | SyntaxKind::KW_END_PROCEDURE
                            | SyntaxKind::KW_ASYNC
                            | SyntaxKind::KW_EXPORT
                    ))
        })
    }

    pub fn export_keyword(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::KW_EXPORT)
    }

    pub fn param_list(&self) -> Option<ParamList> {
        self.0.children().find_map(ParamList::cast)
    }

    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        self.0.children().filter_map(Annotation::cast)
    }

    pub fn body(&self) -> Option<StmtList> {
        self.0.children().find_map(StmtList::cast)
    }
}

#[derive(Debug, Clone)]
pub struct FunctionDef(SyntaxNode);

impl AstNode for FunctionDef {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FUNCTION_DEF
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl FunctionDef {
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT)
    }

    pub fn name_or_keyword(&self) -> Option<SyntaxToken> {
        self.0.children_with_tokens().filter_map(|it| it.into_token()).find(|it| {
            let k = it.kind();
            k == SyntaxKind::IDENT
                || (k.is_keyword()
                    && !matches!(
                        k,
                        SyntaxKind::KW_FUNCTION
                            | SyntaxKind::KW_END_FUNCTION
                            | SyntaxKind::KW_ASYNC
                            | SyntaxKind::KW_EXPORT
                    ))
        })
    }

    pub fn export_keyword(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::KW_EXPORT)
    }

    pub fn param_list(&self) -> Option<ParamList> {
        self.0.children().find_map(ParamList::cast)
    }

    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        self.0.children().filter_map(Annotation::cast)
    }

    pub fn body(&self) -> Option<StmtList> {
        self.0.children().find_map(StmtList::cast)
    }
}

#[derive(Debug, Clone)]
pub struct VarDef(SyntaxNode);

impl AstNode for VarDef {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::VAR_DEF
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl VarDef {
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT)
    }

    pub fn names(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|it| it.kind() == SyntaxKind::IDENT)
    }

    pub fn export_keyword(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::KW_EXPORT)
    }

    /// Each declared name with whether its own `Экспорт` follows it.
    ///
    /// `Экспорт` exports only the name it is written after: in
    /// `Перем А Экспорт, Б;` the platform exports `А` alone, and in
    /// `Перем А, Б Экспорт;` — `Б` alone.
    pub fn names_with_export(&self) -> Vec<(SyntaxToken, bool)> {
        let mut names: Vec<(SyntaxToken, bool)> = Vec::new();
        for token in self.0.children_with_tokens().filter_map(|it| it.into_token()) {
            match token.kind() {
                SyntaxKind::IDENT => names.push((token, false)),
                SyntaxKind::KW_EXPORT => {
                    if let Some(last) = names.last_mut() {
                        last.1 = true;
                    }
                }
                _ => {}
            }
        }
        names
    }

    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        self.0.children().filter_map(Annotation::cast)
    }
}

#[derive(Debug, Clone)]
pub struct ParamList(SyntaxNode);

impl AstNode for ParamList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PARAM_LIST
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl ParamList {
    pub fn params(&self) -> impl Iterator<Item = Param> + '_ {
        self.0.children().filter_map(Param::cast)
    }
}

#[derive(Debug, Clone)]
pub struct Param(SyntaxNode);

impl AstNode for Param {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PARAM
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl Param {
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT)
    }

    pub fn val_keyword(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::KW_VAL)
    }

    pub fn default_value(&self) -> bool {
        self.0.children().next().is_some()
    }

    pub fn default_value_expr(&self) -> Option<SyntaxNode> {
        self.0.children().find(|n| n.kind() == SyntaxKind::EXPR)
    }
}

#[derive(Debug, Clone)]
pub struct AssignStmt(SyntaxNode);

impl AstNode for AssignStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::ASSIGN_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct CallStmt(SyntaxNode);

impl AstNode for CallStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CALL_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ReturnStmt(SyntaxNode);

impl AstNode for ReturnStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RETURN_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct IfStmt(SyntaxNode);

impl AstNode for IfStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::IF_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct WhileStmt(SyntaxNode);

impl AstNode for WhileStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::WHILE_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ForStmt(SyntaxNode);

impl AstNode for ForStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FOR_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ForEachStmt(SyntaxNode);

impl AstNode for ForEachStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FOR_EACH_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct TryStmt(SyntaxNode);

impl AstNode for TryStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TRY_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct RaiseStmt(SyntaxNode);

impl AstNode for RaiseStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::RAISE_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct BreakStmt(SyntaxNode);

impl AstNode for BreakStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::BREAK_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ContinueStmt(SyntaxNode);

impl AstNode for ContinueStmt {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CONTINUE_STMT
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct BinaryExpr(SyntaxNode);

impl AstNode for BinaryExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::BINARY_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct UnaryExpr(SyntaxNode);

impl AstNode for UnaryExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::UNARY_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct TernaryExpr(SyntaxNode);

impl AstNode for TernaryExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::TERNARY_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct CallExpr(SyntaxNode);

impl AstNode for CallExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::CALL_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct IndexExpr(SyntaxNode);

impl AstNode for IndexExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::INDEX_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct FieldExpr(SyntaxNode);

impl AstNode for FieldExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::FIELD_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct NewExpr(SyntaxNode);

impl AstNode for NewExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::NEW_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ParenExpr(SyntaxNode);

impl AstNode for ParenExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PAREN_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct Literal(SyntaxNode);

impl AstNode for Literal {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::LITERAL
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct Annotation(SyntaxNode);

impl AstNode for Annotation {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::ANNOTATION | SyntaxKind::COMPILER_DIRECTIVE)
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl Annotation {
    pub fn kind_token(&self) -> Option<SyntaxToken> {
        self.0.children_with_tokens().filter_map(|it| it.into_token()).find(|token| {
            matches!(
                token.kind(),
                SyntaxKind::ANN_AT_CLIENT
                    | SyntaxKind::ANN_AT_SERVER
                    | SyntaxKind::ANN_AT_SERVER_NO_CONTEXT
                    | SyntaxKind::ANN_AT_CLIENT_AT_SERVER
                    | SyntaxKind::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT
                    | SyntaxKind::ANN_BEFORE
                    | SyntaxKind::ANN_AFTER
                    | SyntaxKind::ANN_AROUND
                    | SyntaxKind::ANN_CHANGE_AND_VALIDATE
                    | SyntaxKind::ANN_CUSTOM
                    | SyntaxKind::IDENT
            )
        })
    }
}

#[derive(Debug, Clone)]
pub struct PreRegionDir(SyntaxNode);

impl AstNode for PreRegionDir {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_REGION_DIR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreRegionDir {
    /// The directive keyword token. Its kind already carries the start/end
    /// distinction: the lexer matches every spelling 1C accepts — either
    /// language, any case, with or without blanks after `#`. Re-deriving the
    /// kind from the node text would put that knowledge in a second place.
    pub fn marker(&self) -> Option<SyntaxToken> {
        self.0.children_with_tokens().filter_map(|element| element.into_token()).find(|token| {
            matches!(token.kind(), SyntaxKind::PRE_REGION | SyntaxKind::PRE_END_REGION)
        })
    }

    /// The region name, or `None` when the directive closes a region or carries
    /// no name at all.
    pub fn name(&self) -> Option<String> {
        if !self.is_start() {
            return None;
        }

        self.0
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .find(|token| token.kind() == SyntaxKind::IDENT)
            .map(|token| token.text().to_string())
    }

    pub fn is_start(&self) -> bool {
        self.marker().is_some_and(|token| token.kind() == SyntaxKind::PRE_REGION)
    }

    pub fn is_end(&self) -> bool {
        self.marker().is_some_and(|token| token.kind() == SyntaxKind::PRE_END_REGION)
    }
}

#[derive(Debug, Clone)]
pub struct PreSymbol(SyntaxNode);

impl AstNode for PreSymbol {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_SYMBOL
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreSymbol {
    pub fn text(&self) -> Option<String> {
        self.name_token().map(|token| token.text().fold_lower())
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        self.0
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|token| token.kind() == SyntaxKind::IDENT)
    }
}

#[derive(Debug, Clone)]
pub struct PreExpr(SyntaxNode);

impl AstNode for PreExpr {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_EXPR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreExpr {
    pub fn symbols(&self) -> impl Iterator<Item = PreSymbol> + '_ {
        self.0.descendants().filter_map(PreSymbol::cast)
    }
}

#[derive(Debug, Clone)]
pub struct PreIfDir(SyntaxNode);

impl AstNode for PreIfDir {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_IF_DIR
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreIfDir {
    pub fn condition(&self) -> Option<PreExpr> {
        self.0.children().find_map(PreExpr::cast)
    }

    pub fn then_body_nodes(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.0.children().filter(|node| node.kind() != SyntaxKind::PRE_EXPR).take_while(|node| {
            !matches!(node.kind(), SyntaxKind::PRE_ELSIF_CLAUSE | SyntaxKind::PRE_ELSE_CLAUSE)
        })
    }

    pub fn elsif_clauses(&self) -> impl Iterator<Item = PreElsIfClause> + '_ {
        self.0.children().filter_map(PreElsIfClause::cast)
    }

    pub fn else_clause(&self) -> Option<PreElseClause> {
        self.0.children().find_map(PreElseClause::cast)
    }
}

#[derive(Debug, Clone)]
pub struct PreElsIfClause(SyntaxNode);

impl AstNode for PreElsIfClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_ELSIF_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreElsIfClause {
    pub fn condition(&self) -> Option<PreExpr> {
        self.0.children().find_map(PreExpr::cast)
    }

    pub fn body_nodes(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.0.children().filter(|node| node.kind() != SyntaxKind::PRE_EXPR)
    }
}

#[derive(Debug, Clone)]
pub struct PreElseClause(SyntaxNode);

impl AstNode for PreElseClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::PRE_ELSE_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl PreElseClause {
    pub fn body_nodes(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.0.children()
    }
}

#[derive(Debug, Clone)]
pub struct StmtList(SyntaxNode);

impl AstNode for StmtList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::STMT_LIST
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl StmtList {
    pub fn var_decls(&self) -> impl Iterator<Item = VarDef> + '_ {
        self.0.children().filter_map(VarDef::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblQueryPackage(SyntaxNode);

impl AstNode for SdblQueryPackage {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_QUERY_PACKAGE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblQueryPackage {
    pub fn queries(&self) -> impl Iterator<Item = SdblSelectQuery> + '_ {
        self.0.children().filter_map(SdblSelectQuery::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblSelectQuery(SyntaxNode);

impl AstNode for SdblSelectQuery {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_SELECT_QUERY
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblSelectQuery {
    pub fn subquery(&self) -> Option<SdblSubquery> {
        self.0.children().find_map(SdblSubquery::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblSubquery(SyntaxNode);

impl AstNode for SdblSubquery {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_SUBQUERY
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblSubquery {
    pub fn main_query(&self) -> Option<SdblQuery> {
        self.0.children().find_map(SdblQuery::cast)
    }

    pub fn union_clauses(&self) -> impl Iterator<Item = SdblUnionClause> + '_ {
        self.0.children().filter_map(SdblUnionClause::cast)
    }

    pub fn queries(&self) -> impl Iterator<Item = SdblQuery> + '_ {
        let main = self.main_query().into_iter();
        let unions = self.union_clauses().filter_map(|union_clause| union_clause.query());
        main.chain(unions)
    }

    pub fn union_queries(&self) -> impl Iterator<Item = SdblQuery> + '_ {
        self.union_clauses().filter_map(|union_clause| union_clause.query())
    }
}

#[derive(Debug, Clone)]
pub struct SdblUnionClause(SyntaxNode);

impl AstNode for SdblUnionClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_UNION_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblUnionClause {
    pub fn query(&self) -> Option<SdblQuery> {
        self.0.children().find_map(SdblQuery::cast)
    }

    pub fn has_all(&self) -> bool {
        self.0.children_with_tokens().filter_map(|it| it.into_token()).any(|t| {
            if t.kind() == SyntaxKind::IDENT {
                let text = t.text();
                text.eq_ignore_ascii_case("ALL") || text.eq_ignore_ascii_case("ВСЕ")
            } else {
                false
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct SdblQuery(SyntaxNode);

impl AstNode for SdblQuery {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_QUERY
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblQuery {
    pub fn field_list(&self) -> Option<SdblFieldList> {
        self.0.children().find_map(SdblFieldList::cast)
    }

    pub fn from_clause(&self) -> Option<SdblFromClause> {
        self.0.children().find_map(SdblFromClause::cast)
    }

    pub fn where_clause(&self) -> Option<SdblWhereClause> {
        self.0.children().find_map(SdblWhereClause::cast)
    }

    pub fn having_clause(&self) -> Option<SdblHavingClause> {
        self.0.children().find_map(SdblHavingClause::cast)
    }

    pub fn group_by_clause(&self) -> Option<SdblGroupClause> {
        self.0.children().find_map(SdblGroupClause::cast)
    }

    pub fn order_by_clause(&self) -> Option<SdblOrderClause> {
        self.0.children().find_map(SdblOrderClause::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblFieldList(SyntaxNode);

impl AstNode for SdblFieldList {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_FIELD_LIST
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblFieldList {
    pub fn fields(&self) -> impl Iterator<Item = SdblSelectedField> + '_ {
        self.0.children().filter_map(SdblSelectedField::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblSelectedField(SyntaxNode);

impl AstNode for SdblSelectedField {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_SELECTED_FIELD
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblSelectedField {
    pub fn is_asterisk(&self) -> bool {
        self.0.children().any(|n| n.kind() == SyntaxKind::SDBL_ASTERISK_FIELD)
    }

    pub fn alias(&self) -> Option<SdblAlias> {
        self.0.children().find_map(SdblAlias::cast)
    }

    pub fn expression(&self) -> Option<SyntaxNode> {
        self.0.children().find(|n| {
            matches!(
                n.kind(),
                SyntaxKind::SDBL_COLUMN_REF
                    | SyntaxKind::SDBL_FUNCTION_CALL
                    | SyntaxKind::SDBL_LITERAL
                    | SyntaxKind::SDBL_PAREN_EXPR
                    | SyntaxKind::SDBL_LOGICAL_OR_EXPR
                    | SyntaxKind::SDBL_LOGICAL_AND_EXPR
                    | SyntaxKind::SDBL_NOT_EXPR
                    | SyntaxKind::SDBL_COMPARISON_EXPR
                    | SyntaxKind::SDBL_ADDITIVE_EXPR
                    | SyntaxKind::SDBL_MULTIPLICATIVE_EXPR
                    | SyntaxKind::SDBL_UNARY_EXPR
            )
        })
    }
}

#[derive(Debug, Clone)]
pub struct SdblAlias(SyntaxNode);

impl AstNode for SdblAlias {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_ALIAS
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblAlias {
    pub fn has_as_keyword(&self) -> bool {
        self.0.children_with_tokens().filter_map(|it| it.into_token()).any(|t| {
            if t.kind() == SyntaxKind::IDENT {
                let text = t.text();
                text.eq_ignore_ascii_case("AS") || text.eq_ignore_ascii_case("КАК")
            } else {
                false
            }
        })
    }

    pub fn identifier(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .filter(|t| {
                let text = t.text();
                !text.eq_ignore_ascii_case("AS") && !text.eq_ignore_ascii_case("КАК")
            })
            .last()
    }

    pub fn name(&self) -> Option<String> {
        self.identifier().map(|tok| tok.text().to_string())
    }
}

#[derive(Debug, Clone)]
pub struct SdblAsteriskField(SyntaxNode);

impl AstNode for SdblAsteriskField {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_ASTERISK_FIELD
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblAsteriskField {
    pub fn qualifier_parts(&self) -> Vec<String> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct SdblFromClause(SyntaxNode);

impl AstNode for SdblFromClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_FROM_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblFromClause {
    pub fn data_sources(&self) -> impl Iterator<Item = SdblDataSource> + '_ {
        self.0.children().filter_map(SdblDataSource::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblDataSource(SyntaxNode);

impl AstNode for SdblDataSource {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_DATA_SOURCE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblDataSource {
    pub fn table_ref(&self) -> Option<SdblTableRef> {
        self.0.children().find_map(SdblTableRef::cast)
    }

    pub fn subquery(&self) -> Option<SdblSubquery> {
        self.0.children().find_map(SdblSubquery::cast)
    }

    pub fn alias(&self) -> Option<SdblAlias> {
        self.0.children().find_map(SdblAlias::cast)
    }

    pub fn join_clauses(&self) -> impl Iterator<Item = SdblJoinClause> + '_ {
        self.0.children().filter_map(SdblJoinClause::cast)
    }
}

#[derive(Debug, Clone)]
pub struct SdblTableRef(SyntaxNode);

impl AstNode for SdblTableRef {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_TABLE_REF
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct SdblJoinClause(SyntaxNode);

impl AstNode for SdblJoinClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_JOIN_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl SdblJoinClause {
    pub fn data_source(&self) -> Option<SdblDataSource> {
        self.0.children().find_map(SdblDataSource::cast)
    }

    pub fn join_type(&self) -> JoinType {
        let own_tokens: String = self
            .0
            .children_with_tokens()
            .filter_map(|child| child.into_token().map(|t| t.text().to_string()))
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();

        let parent_tokens: String = self
            .0
            .parent()
            .map(|p| {
                p.children_with_tokens()
                    .filter_map(|child| child.into_token().map(|t| t.text().to_string()))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_uppercase()
            })
            .unwrap_or_default();

        let combined = format!("{} {}", parent_tokens, own_tokens);

        if combined.contains("LEFT") || combined.contains("ЛЕВОЕ") {
            JoinType::Left
        } else if combined.contains("RIGHT") || combined.contains("ПРАВОЕ") {
            JoinType::Right
        } else if combined.contains("FULL") || combined.contains("ПОЛНОЕ") {
            JoinType::Full
        } else {
            JoinType::Inner
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Left,
    Right,
    Full,
    Inner,
}

#[derive(Debug, Clone)]
pub struct SdblWhereClause(SyntaxNode);

impl AstNode for SdblWhereClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_WHERE_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct SdblHavingClause(SyntaxNode);

impl AstNode for SdblHavingClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_HAVING_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct SdblGroupClause(SyntaxNode);

impl AstNode for SdblGroupClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_GROUP_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct SdblOrderClause(SyntaxNode);

impl AstNode for SdblOrderClause {
    fn can_cast(kind: SyntaxKind) -> bool {
        kind == SyntaxKind::SDBL_ORDER_CLAUSE
    }

    fn cast(node: SyntaxNode) -> Option<Self> {
        if Self::can_cast(node.kind()) {
            Some(Self(node))
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

#[cfg(test)]
mod preproc_wrappers_tests {
    use super::*;
    use crate::SyntaxTreeBuilder;

    fn add_pre_expr(builder: &mut SyntaxTreeBuilder<'_>, symbol: &str) {
        builder.start_node(SyntaxKind::PRE_EXPR);
        builder.start_node(SyntaxKind::PRE_LOGICAL_EXPR);
        builder.start_node(SyntaxKind::PRE_LOGICAL_OPERAND);
        builder.start_node(SyntaxKind::PRE_SYMBOL);
        builder.token(SyntaxKind::IDENT, symbol);
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
    }

    fn add_body_node(builder: &mut SyntaxTreeBuilder<'_>, name: &str) {
        builder.start_node(SyntaxKind::CALL_STMT);
        builder.token(SyntaxKind::IDENT, name);
        builder.token(SyntaxKind::L_PAREN, "(");
        builder.token(SyntaxKind::R_PAREN, ")");
        builder.token(SyntaxKind::SEMICOLON, ";");
        builder.finish_node();
    }

    fn parse(input: &str) -> SyntaxNode {
        let condition =
            input.strip_prefix("#Если ").and_then(|rest| rest.split_once(" Тогда")).unwrap().0;
        let mut builder = SyntaxTreeBuilder::new();

        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        builder.token(SyntaxKind::PRE_IF, "#Если");
        builder.token(SyntaxKind::WHITESPACE, " ");
        builder.start_node(SyntaxKind::PRE_EXPR);
        builder.start_node(SyntaxKind::PRE_LOGICAL_EXPR);

        let mut pending_not = false;
        for part in condition.split_whitespace() {
            match part {
                "И" => {
                    builder.token(SyntaxKind::WHITESPACE, " ");
                    builder.start_node(SyntaxKind::PRE_BOOL_OP);
                    builder.token(SyntaxKind::KW_AND, "И");
                    builder.finish_node();
                    builder.token(SyntaxKind::WHITESPACE, " ");
                }
                "НЕ" => {
                    pending_not = true;
                }
                symbol => {
                    builder.start_node(SyntaxKind::PRE_LOGICAL_OPERAND);
                    if pending_not {
                        builder.token(SyntaxKind::KW_NOT, "НЕ");
                        builder.token(SyntaxKind::WHITESPACE, " ");
                        pending_not = false;
                    }
                    builder.start_node(SyntaxKind::PRE_SYMBOL);
                    builder.token(SyntaxKind::IDENT, symbol);
                    builder.finish_node();
                    builder.finish_node();
                }
            }
        }

        builder.finish_node();
        builder.finish_node();
        builder.token(SyntaxKind::WHITESPACE, " ");
        builder.token(SyntaxKind::KW_THEN, "Тогда");
        builder.token(SyntaxKind::WHITESPACE, " ");
        builder.token(SyntaxKind::IDENT, "X");
        builder.token(SyntaxKind::L_PAREN, "(");
        builder.token(SyntaxKind::R_PAREN, ")");
        builder.token(SyntaxKind::SEMICOLON, ";");
        builder.token(SyntaxKind::WHITESPACE, " ");
        builder.token(SyntaxKind::PRE_END_IF, "#КонецЕсли");
        builder.finish_node();
        builder.finish_node();
        builder.finish().syntax_node()
    }

    #[test]
    fn parse_pre_symbol_lowercases() {
        let root = parse("#Если Клиент Тогда X(); #КонецЕсли");
        let symbol = root.descendants().find_map(PreSymbol::cast).expect("pre symbol");

        assert_eq!(symbol.text(), Some("клиент".into()));
    }

    #[test]
    fn parse_pre_expr_collects_symbols_in_order() {
        let root = parse("#Если Клиент И НЕ Сервер Тогда X(); #КонецЕсли");
        let expr = root.descendants().find_map(PreExpr::cast).expect("pre expr");
        let symbols = expr.symbols().filter_map(|symbol| symbol.text()).collect::<Vec<_>>();

        assert_eq!(symbols, vec!["клиент", "сервер"]);
    }

    #[test]
    fn pre_symbol_can_cast_rejects_wrong_kind() {
        let root = parse("#Если Клиент Тогда X(); #КонецЕсли");
        let other_node =
            root.descendants().find(|node| node.kind() != SyntaxKind::PRE_SYMBOL).expect("node");

        assert!(PreSymbol::cast(other_node).is_none());
    }

    #[test]
    fn pre_if_dir_extracts_condition_and_then_body() {
        let mut builder = SyntaxTreeBuilder::new();
        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "A");
        add_body_node(&mut builder, "X");
        add_body_node(&mut builder, "Y");
        builder.finish_node();
        builder.finish_node();
        let root = builder.finish().syntax_node();

        let pre_if = root.descendants().find_map(PreIfDir::cast).expect("pre if");
        let condition = pre_if.condition().expect("condition");

        assert_eq!(condition.symbols().count(), 1);
        assert_eq!(pre_if.then_body_nodes().count(), 2);
    }

    #[test]
    fn pre_if_dir_collects_elsif_clauses_in_order() {
        let mut builder = SyntaxTreeBuilder::new();
        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "A");
        add_body_node(&mut builder, "X");
        builder.start_node(SyntaxKind::PRE_ELSIF_CLAUSE);
        add_pre_expr(&mut builder, "B");
        add_body_node(&mut builder, "Y");
        builder.finish_node();
        builder.start_node(SyntaxKind::PRE_ELSIF_CLAUSE);
        add_pre_expr(&mut builder, "C");
        add_body_node(&mut builder, "Z");
        builder.finish_node();
        builder.start_node(SyntaxKind::PRE_ELSE_CLAUSE);
        add_body_node(&mut builder, "W");
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        let root = builder.finish().syntax_node();

        let pre_if = root.descendants().find_map(PreIfDir::cast).expect("pre if");
        let clauses = pre_if.elsif_clauses().collect::<Vec<_>>();

        assert_eq!(clauses.len(), 2);
        assert!(pre_if.else_clause().is_some());
        assert_eq!(
            clauses[0]
                .condition()
                .and_then(|expr| expr.symbols().next())
                .and_then(|symbol| symbol.name_token())
                .map(|token| token.text().to_string()),
            Some("B".into())
        );
        assert_eq!(
            clauses[1]
                .condition()
                .and_then(|expr| expr.symbols().next())
                .and_then(|symbol| symbol.name_token())
                .map(|token| token.text().to_string()),
            Some("C".into())
        );
    }

    #[test]
    fn pre_if_dir_without_else() {
        let mut builder = SyntaxTreeBuilder::new();
        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "A");
        add_body_node(&mut builder, "X");
        builder.finish_node();
        builder.finish_node();
        let root = builder.finish().syntax_node();

        let pre_if = root.descendants().find_map(PreIfDir::cast).expect("pre if");

        assert!(pre_if.else_clause().is_none());
        assert_eq!(pre_if.elsif_clauses().count(), 0);
    }

    #[test]
    fn pre_if_dir_then_body_stops_at_elsif() {
        let mut builder = SyntaxTreeBuilder::new();
        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "A");
        add_body_node(&mut builder, "X");
        builder.start_node(SyntaxKind::PRE_ELSIF_CLAUSE);
        add_pre_expr(&mut builder, "B");
        add_body_node(&mut builder, "Y");
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        let root = builder.finish().syntax_node();

        let pre_if = root.descendants().find_map(PreIfDir::cast).expect("pre if");
        let then_body = pre_if.then_body_nodes().collect::<Vec<_>>();
        let elsif = pre_if.elsif_clauses().next().expect("elsif");
        let elsif_body = elsif.body_nodes().collect::<Vec<_>>();

        assert_eq!(then_body.len(), 1);
        assert_eq!(then_body[0].kind(), SyntaxKind::CALL_STMT);
        assert_eq!(elsif_body.len(), 1);
        assert_eq!(elsif_body[0].kind(), SyntaxKind::CALL_STMT);
    }

    #[test]
    fn pre_if_dir_nested_does_not_leak_branches() {
        let mut builder = SyntaxTreeBuilder::new();
        builder.start_node(SyntaxKind::SOURCE_FILE);
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "A");
        builder.start_node(SyntaxKind::PRE_IF_DIR);
        add_pre_expr(&mut builder, "B");
        add_body_node(&mut builder, "C");
        builder.start_node(SyntaxKind::PRE_ELSE_CLAUSE);
        add_body_node(&mut builder, "D");
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        let root = builder.finish().syntax_node();

        let mut pre_ifs = root.descendants().filter_map(PreIfDir::cast);
        let outer = pre_ifs.next().expect("outer pre if");
        let inner = pre_ifs.next().expect("inner pre if");

        assert!(outer.else_clause().is_none(), "inner #Иначе must not leak as outer's else");
        assert_eq!(outer.elsif_clauses().count(), 0);

        let outer_then: Vec<_> = outer.then_body_nodes().collect();
        assert_eq!(outer_then.len(), 1);
        assert_eq!(outer_then[0].kind(), SyntaxKind::PRE_IF_DIR);

        let inner_then: Vec<_> = inner.then_body_nodes().collect();
        assert_eq!(inner_then.len(), 1);
        assert_eq!(inner_then[0].kind(), SyntaxKind::CALL_STMT);
        assert!(inner.else_clause().is_some());
    }
}
