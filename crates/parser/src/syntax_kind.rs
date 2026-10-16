use lexer::TokenKind;

use crate::event::NodeKind;

pub fn node_kind_to_syntax(kind: NodeKind) -> syntax::SyntaxKind {
    use syntax::SyntaxKind as SK;

    match kind {
        NodeKind::SourceFile => SK::SOURCE_FILE,

        NodeKind::ProcedureDef => SK::PROCEDURE_DEF,
        NodeKind::FunctionDef => SK::FUNCTION_DEF,
        NodeKind::VarDef => SK::VAR_DEF,
        NodeKind::ParamList => SK::PARAM_LIST,
        NodeKind::Param => SK::PARAM,
        NodeKind::Annotation => SK::ANNOTATION,
        NodeKind::AnnotationParams => SK::ANNOTATION_PARAMS,
        NodeKind::AnnotationParam => SK::ANNOTATION_PARAM,
        NodeKind::CompilerDirective => SK::COMPILER_DIRECTIVE,

        NodeKind::StmtList => SK::STMT_LIST,
        NodeKind::AssignStmt => SK::ASSIGN_STMT,
        NodeKind::CallStmt => SK::CALL_STMT,
        NodeKind::ReturnStmt => SK::RETURN_STMT,
        NodeKind::IfStmt => SK::IF_STMT,
        NodeKind::ElseIfClause => SK::ELSIF_CLAUSE,
        NodeKind::ElseClause => SK::ELSE_CLAUSE,
        NodeKind::WhileStmt => SK::WHILE_STMT,
        NodeKind::ForStmt => SK::FOR_STMT,
        NodeKind::ForEachStmt => SK::FOR_EACH_STMT,
        NodeKind::TryStmt => SK::TRY_STMT,
        NodeKind::ExceptClause => SK::EXCEPT_CLAUSE,
        NodeKind::RaiseStmt => SK::RAISE_STMT,
        NodeKind::ExecuteStmt => SK::EXECUTE_STMT,
        NodeKind::BreakStmt => SK::BREAK_STMT,
        NodeKind::ContinueStmt => SK::CONTINUE_STMT,
        NodeKind::GotoStmt => SK::GOTO_STMT,
        NodeKind::LabelStmt => SK::LABEL_STMT,
        NodeKind::AddHandlerStmt => SK::ADD_HANDLER_STMT,
        NodeKind::RemoveHandlerStmt => SK::REMOVE_HANDLER_STMT,
        NodeKind::EmptyStmt => SK::EMPTY_STMT,

        NodeKind::Expr => SK::EXPR,
        NodeKind::BinaryExpr => SK::BINARY_EXPR,
        NodeKind::UnaryExpr => SK::UNARY_EXPR,
        NodeKind::TernaryExpr => SK::TERNARY_EXPR,
        NodeKind::CallExpr => SK::CALL_EXPR,
        NodeKind::IndexExpr => SK::INDEX_EXPR,
        NodeKind::FieldExpr => SK::FIELD_EXPR,
        NodeKind::NewExpr => SK::NEW_EXPR,
        NodeKind::AwaitExpr => SK::AWAIT_EXPR,
        NodeKind::ParenExpr => SK::PAREN_EXPR,
        NodeKind::Literal => SK::LITERAL,
        NodeKind::Ident => SK::IDENT,
        NodeKind::ArgList => SK::ARG_LIST,

        NodeKind::PreIfDir => SK::PRE_IF_DIR,
        NodeKind::PreElsIfClause => SK::PRE_ELSIF_CLAUSE,
        NodeKind::PreElseClause => SK::PRE_ELSE_CLAUSE,
        NodeKind::PreRegionDir => SK::PRE_REGION_DIR,
        NodeKind::PreDeleteDir => SK::PRE_DELETE_DIR,
        NodeKind::PreInsertDir => SK::PRE_INSERT_DIR,
        NodeKind::PreTornIfPart => SK::PRE_TORN_IF_PART,
        NodeKind::PreExpr => SK::PRE_EXPR,
        NodeKind::PreLogicalExpr => SK::PRE_LOGICAL_EXPR,
        NodeKind::PreLogicalOperand => SK::PRE_LOGICAL_OPERAND,
        NodeKind::PreSymbol => SK::PRE_SYMBOL,
        NodeKind::PreBoolOp => SK::PRE_BOOL_OP,

        NodeKind::SdblQueryPackage => SK::SDBL_QUERY_PACKAGE,
        NodeKind::SdblSelectQuery => SK::SDBL_SELECT_QUERY,
        NodeKind::SdblSubquery => SK::SDBL_SUBQUERY,
        NodeKind::SdblUnionClause => SK::SDBL_UNION_CLAUSE,
        NodeKind::SdblQuery => SK::SDBL_QUERY,
        NodeKind::SdblQueryExtension => SK::SDBL_QUERY_EXTENSION,
        NodeKind::SdblLimitations => SK::SDBL_LIMITATIONS,
        NodeKind::SdblTopClause => SK::SDBL_TOP_CLAUSE,
        NodeKind::SdblSelectClause => SK::SDBL_SELECT_CLAUSE,
        NodeKind::SdblFieldList => SK::SDBL_FIELD_LIST,
        NodeKind::SdblSelectedField => SK::SDBL_SELECTED_FIELD,
        NodeKind::SdblAlias => SK::SDBL_ALIAS,
        NodeKind::SdblAsteriskField => SK::SDBL_ASTERISK_FIELD,
        NodeKind::SdblIntoClause => SK::SDBL_INTO_CLAUSE,
        NodeKind::SdblTempTableName => SK::SDBL_TEMP_TABLE_NAME,
        NodeKind::SdblFromClause => SK::SDBL_FROM_CLAUSE,
        NodeKind::SdblDataSource => SK::SDBL_DATA_SOURCE,
        NodeKind::SdblTableRef => SK::SDBL_TABLE_REF,
        NodeKind::SdblJoinClause => SK::SDBL_JOIN_CLAUSE,
        NodeKind::SdblWhereClause => SK::SDBL_WHERE_CLAUSE,
        NodeKind::SdblGroupClause => SK::SDBL_GROUP_CLAUSE,
        NodeKind::SdblOrderClause => SK::SDBL_ORDER_CLAUSE,
        NodeKind::SdblHavingClause => SK::SDBL_HAVING_CLAUSE,
        NodeKind::SdblForUpdate => SK::SDBL_FOR_UPDATE,
        NodeKind::SdblIndexBy => SK::SDBL_INDEX_BY,
        NodeKind::SdblAutoorder => SK::SDBL_AUTOORDER,
        NodeKind::SdblTotalsBy => SK::SDBL_TOTALS_BY,
        NodeKind::SdblExpr => SK::SDBL_EXPR,
        NodeKind::SdblLogicalOrExpr => SK::SDBL_LOGICAL_OR_EXPR,
        NodeKind::SdblLogicalAndExpr => SK::SDBL_LOGICAL_AND_EXPR,
        NodeKind::SdblNotExpr => SK::SDBL_NOT_EXPR,
        NodeKind::SdblComparisonExpr => SK::SDBL_COMPARISON_EXPR,
        NodeKind::SdblInExpr => SK::SDBL_IN_EXPR,
        NodeKind::SdblInHierarchyExpr => SK::SDBL_IN_HIERARCHY_EXPR,
        NodeKind::SdblIsNullExpr => SK::SDBL_IS_NULL_EXPR,
        NodeKind::SdblBetweenExpr => SK::SDBL_BETWEEN_EXPR,
        NodeKind::SdblLikeExpr => SK::SDBL_LIKE_EXPR,
        NodeKind::SdblRefsExpr => SK::SDBL_REFS_EXPR,
        NodeKind::SdblAdditiveExpr => SK::SDBL_ADDITIVE_EXPR,
        NodeKind::SdblMultiplicativeExpr => SK::SDBL_MULTIPLICATIVE_EXPR,
        NodeKind::SdblUnaryExpr => SK::SDBL_UNARY_EXPR,
        NodeKind::SdblParenExpr => SK::SDBL_PAREN_EXPR,
        NodeKind::SdblTupleExpr => SK::SDBL_TUPLE_EXPR,
        NodeKind::SdblSubqueryExpr => SK::SDBL_SUBQUERY_EXPR,
        NodeKind::SdblColumnRef => SK::SDBL_COLUMN_REF,
        NodeKind::SdblInlineTableFields => SK::SDBL_INLINE_TABLE_FIELDS,
        NodeKind::SdblFunctionCall => SK::SDBL_FUNCTION_CALL,
        NodeKind::SdblCaseExpr => SK::SDBL_CASE_EXPR,
        NodeKind::SdblWhenClause => SK::SDBL_WHEN_CLAUSE,
        NodeKind::SdblLiteral => SK::SDBL_LITERAL,
        NodeKind::SdblMultiString => SK::SDBL_MULTI_STRING,
        NodeKind::SdblParameter => SK::SDBL_PARAMETER,
        NodeKind::SdblType => SK::SDBL_TYPE,
        NodeKind::SdblDropQuery => SK::SDBL_DROP_QUERY,
        NodeKind::SdblMissingArg => SK::SDBL_MISSING_ARG,
        NodeKind::SdblError => SK::SDBL_ERROR,

        NodeKind::Error => SK::ERROR,
        NodeKind::Comment => SK::COMMENT,
    }
}

pub fn token_kind_to_syntax(kind: TokenKind) -> syntax::SyntaxKind {
    use syntax::SyntaxKind as SK;

    match kind {
        TokenKind::KwProcedure => SK::KW_PROCEDURE,
        TokenKind::KwEndProcedure => SK::KW_END_PROCEDURE,
        TokenKind::KwFunction => SK::KW_FUNCTION,
        TokenKind::KwEndFunction => SK::KW_END_FUNCTION,
        TokenKind::KwExport => SK::KW_EXPORT,
        TokenKind::KwVal => SK::KW_VAL,
        TokenKind::KwIf => SK::KW_IF,
        TokenKind::KwThen => SK::KW_THEN,
        TokenKind::KwElsIf => SK::KW_ELSIF,
        TokenKind::KwElse => SK::KW_ELSE,
        TokenKind::KwEndIf => SK::KW_END_IF,
        TokenKind::KwFor => SK::KW_FOR,
        TokenKind::KwEach => SK::KW_EACH,
        TokenKind::KwIn => SK::KW_IN,
        TokenKind::KwTo => SK::KW_TO,
        TokenKind::KwWhile => SK::KW_WHILE,
        TokenKind::KwDo => SK::KW_DO,
        TokenKind::KwEndDo => SK::KW_END_DO,
        TokenKind::KwReturn => SK::KW_RETURN,
        TokenKind::KwContinue => SK::KW_CONTINUE,
        TokenKind::KwBreak => SK::KW_BREAK,
        TokenKind::KwGoto => SK::KW_GOTO,
        TokenKind::KwTry => SK::KW_TRY,
        TokenKind::KwExcept => SK::KW_EXCEPT,
        TokenKind::KwEndTry => SK::KW_END_TRY,
        TokenKind::KwRaise => SK::KW_RAISE,
        TokenKind::KwVar => SK::KW_VAR,
        TokenKind::KwNew => SK::KW_NEW,
        TokenKind::KwExecute => SK::KW_EXECUTE,
        TokenKind::KwAddHandler => SK::KW_ADD_HANDLER,
        TokenKind::KwRemoveHandler => SK::KW_REMOVE_HANDLER,
        TokenKind::KwAsync => SK::KW_ASYNC,
        TokenKind::KwAwait => SK::KW_AWAIT,
        TokenKind::KwAnd => SK::KW_AND,
        TokenKind::KwOr => SK::KW_OR,
        TokenKind::KwNot => SK::KW_NOT,
        TokenKind::KwTrue => SK::KW_TRUE,
        TokenKind::KwFalse => SK::KW_FALSE,
        TokenKind::KwUndefined => SK::KW_UNDEFINED,
        TokenKind::KwNull => SK::KW_NULL,

        TokenKind::PreIf => SK::PRE_IF,
        TokenKind::PreElsIf => SK::PRE_ELSIF,
        TokenKind::PreElse => SK::PRE_ELSE,
        TokenKind::PreEndIf => SK::PRE_END_IF,
        TokenKind::PreRegion => SK::PRE_REGION,
        TokenKind::PreEndRegion => SK::PRE_END_REGION,
        TokenKind::PreInsert => SK::PRE_INSERT,
        TokenKind::PreEndInsert => SK::PRE_END_INSERT,
        TokenKind::PreDelete => SK::PRE_DELETE,
        TokenKind::PreEndDelete => SK::PRE_END_DELETE,

        TokenKind::AnnAtClient => SK::ANN_AT_CLIENT,
        TokenKind::AnnAtServer => SK::ANN_AT_SERVER,
        TokenKind::AnnAtServerNoContext => SK::ANN_AT_SERVER_NO_CONTEXT,
        TokenKind::AnnAtClientAtServerNoContext => SK::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT,
        TokenKind::AnnAtClientAtServer => SK::ANN_AT_CLIENT_AT_SERVER,
        TokenKind::AnnBefore => SK::ANN_BEFORE,
        TokenKind::AnnAfter => SK::ANN_AFTER,
        TokenKind::AnnAround => SK::ANN_AROUND,
        TokenKind::AnnChangeAndValidate => SK::ANN_CHANGE_AND_VALIDATE,
        TokenKind::AnnCustom => SK::ANN_CUSTOM,

        TokenKind::Eq => SK::EQ,
        TokenKind::Neq => SK::NEQ,
        TokenKind::Le => SK::LE,
        TokenKind::Lt => SK::LT,
        TokenKind::Ge => SK::GE,
        TokenKind::Gt => SK::GT,
        TokenKind::Plus => SK::PLUS,
        TokenKind::Minus => SK::MINUS,
        TokenKind::Star => SK::STAR,
        TokenKind::Slash => SK::SLASH,
        TokenKind::Percent => SK::PERCENT,

        TokenKind::LParen => SK::L_PAREN,
        TokenKind::RParen => SK::R_PAREN,
        TokenKind::LBrace => SK::L_BRACE,
        TokenKind::RBrace => SK::R_BRACE,
        TokenKind::LBracket => SK::L_BRACKET,
        TokenKind::RBracket => SK::R_BRACKET,
        TokenKind::Dot => SK::DOT,
        TokenKind::Comma => SK::COMMA,
        TokenKind::Semicolon => SK::SEMICOLON,
        TokenKind::Colon => SK::COLON,
        TokenKind::Question => SK::QUESTION,
        TokenKind::Tilde => SK::TILDE,
        TokenKind::Bar => SK::BAR,
        TokenKind::Hash => SK::HASH,
        TokenKind::Ampersand => SK::AMPERSAND,

        TokenKind::Float => SK::FLOAT,
        TokenKind::Decimal => SK::DECIMAL,
        TokenKind::String => SK::STRING,
        TokenKind::StringStart => SK::STRING_START,
        TokenKind::StringTail => SK::STRING_TAIL,
        TokenKind::StringPart => SK::STRING_PART,
        TokenKind::Date => SK::DATE,

        TokenKind::Ident => SK::IDENT,

        TokenKind::Whitespace => SK::WHITESPACE,
        TokenKind::Newline => SK::NEWLINE,
        TokenKind::Comment => SK::COMMENT,
        TokenKind::Bom => SK::BOM,

        TokenKind::Error => SK::ERROR,
    }
}

#[cfg(test)]
mod kind_agreement_tests {
    use super::token_kind_to_syntax;
    use lexer::TokenKind;
    use syntax::SyntaxKind;

    /// Два канонических предиката тривии согласны на КАЖДОМ виде.
    ///
    /// Слоёв два, и предикат каждого живёт у себя: вид лексемы принадлежит
    /// лексеру, вид узла — дереву. Наводить один на другой нельзя, а
    /// разойтись они могут молча — отображение видов правится отдельно от
    /// обоих.
    ///
    /// Перебор, а не выборка: проверка на трёх видах зелена и у таблицы,
    /// разошедшейся на четвёртом. Полноту перебора держит
    /// `TokenKind::ALL` со своим тестом.
    #[test]
    fn both_layers_agree_on_what_trivia_is() {
        for kind in TokenKind::ALL {
            assert_eq!(
                kind.is_trivia(),
                token_kind_to_syntax(*kind).is_trivia(),
                "{kind:?}: лексер и дерево разошлись в том, тривия ли это"
            );
        }
    }

    /// Виды дерева, которые лексер выдаёт за инструкции препроцессора.
    ///
    /// Разряд задан именованием, а не вторым списком: `TokenKind` содержит
    /// только виды лексем, поэтому префикс `Pre` в нём означает ровно
    /// инструкцию препроцессора — в отличие от `SyntaxKind`, где `PRE_*`
    /// носят ещё и узлы (`PRE_IF_DIR`, `PRE_EXPR`). Полноту перебора по
    /// стороне лексера держит `TokenKind::ALL` со своим тестом.
    fn preprocessor_kinds_of_the_tree() -> Vec<SyntaxKind> {
        TokenKind::ALL
            .iter()
            .filter(|kind| format!("{kind:?}").starts_with("Pre"))
            .map(|kind| token_kind_to_syntax(*kind))
            .collect()
    }

    /// `SyntaxKind::is_preprocessor` истинен ровно на разряде инструкций.
    ///
    /// Перебор с обеих сторон: разряд собирается по полному списку видов
    /// лексера, а сверяется на КАЖДОМ виде дерева. Выборка из четырёх
    /// представителей зелена и у предиката, отставшего на пятом, — а
    /// отстать он может молча: единственный потребитель предиката,
    /// подсветка, на неучтённом виде просто ничего не возвращает.
    #[test]
    fn the_preprocessor_predicate_covers_every_instruction_kind() {
        let instructions = preprocessor_kinds_of_the_tree();
        assert!(
            !instructions.is_empty(),
            "разряд инструкций пуст: сверять нечего, проверка прошла бы вхолостую"
        );

        for raw in 0..SyntaxKind::__LAST as u16 {
            let kind = SyntaxKind::from(raw);
            assert_eq!(
                kind.is_preprocessor(),
                instructions.contains(&kind),
                "{kind:?}: предикат дерева разошёлся с разрядом инструкций препроцессора"
            );
        }
    }
}
