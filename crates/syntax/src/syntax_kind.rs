use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    #[doc(hidden)]
    TOMBSTONE,
    EOF,

    WHITESPACE,
    NEWLINE,
    COMMENT,
    BOM,

    KW_PROCEDURE,
    KW_END_PROCEDURE,
    KW_FUNCTION,
    KW_END_FUNCTION,
    KW_EXPORT,
    KW_VAL,

    KW_IF,
    KW_THEN,
    KW_ELSIF,
    KW_ELSE,
    KW_END_IF,

    KW_FOR,
    KW_EACH,
    KW_IN,
    KW_TO,
    KW_WHILE,
    KW_DO,
    KW_END_DO,
    KW_RETURN,
    KW_CONTINUE,
    KW_BREAK,
    KW_GOTO,

    KW_TRY,
    KW_EXCEPT,
    KW_END_TRY,
    KW_RAISE,

    KW_VAR,
    KW_NEW,
    KW_EXECUTE,

    KW_ADD_HANDLER,
    KW_REMOVE_HANDLER,

    KW_ASYNC,
    KW_AWAIT,

    KW_AND,
    KW_OR,
    KW_NOT,

    KW_TRUE,
    KW_FALSE,

    KW_UNDEFINED,
    KW_NULL,

    PRE_IF,
    PRE_ELSIF,
    PRE_ELSE,
    PRE_END_IF,
    PRE_REGION,
    PRE_END_REGION,
    PRE_INSERT,
    PRE_END_INSERT,
    PRE_DELETE,
    PRE_END_DELETE,

    ANN_AT_CLIENT,
    ANN_AT_SERVER,
    ANN_AT_SERVER_NO_CONTEXT,
    ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT,
    ANN_AT_CLIENT_AT_SERVER,
    ANN_BEFORE,
    ANN_AFTER,
    ANN_AROUND,
    ANN_CHANGE_AND_VALIDATE,
    ANN_CUSTOM,

    EQ,
    NEQ,
    LE,
    LT,
    GE,
    GT,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    PERCENT,

    L_PAREN,
    R_PAREN,
    L_BRACE,
    R_BRACE,
    L_BRACKET,
    R_BRACKET,
    DOT,
    COMMA,
    SEMICOLON,
    COLON,
    QUESTION,
    TILDE,
    BAR,
    HASH,
    AMPERSAND,

    FLOAT,
    DECIMAL,
    STRING,
    STRING_START,
    STRING_TAIL,
    STRING_PART,
    DATE,

    IDENT,

    SOURCE_FILE,

    PROCEDURE_DEF,
    FUNCTION_DEF,
    VAR_DEF,
    PARAM_LIST,
    PARAM,
    ANNOTATION,
    ANNOTATION_PARAMS,
    ANNOTATION_PARAM,
    COMPILER_DIRECTIVE,

    STMT_LIST,
    ASSIGN_STMT,
    CALL_STMT,
    RETURN_STMT,
    IF_STMT,
    ELSIF_CLAUSE,
    ELSE_CLAUSE,
    WHILE_STMT,
    FOR_STMT,
    FOR_EACH_STMT,
    TRY_STMT,
    EXCEPT_CLAUSE,
    RAISE_STMT,
    EXECUTE_STMT,
    BREAK_STMT,
    CONTINUE_STMT,
    GOTO_STMT,
    LABEL_STMT,
    ADD_HANDLER_STMT,
    REMOVE_HANDLER_STMT,
    EMPTY_STMT,

    EXPR,
    BINARY_EXPR,
    UNARY_EXPR,
    TERNARY_EXPR,
    CALL_EXPR,
    INDEX_EXPR,
    FIELD_EXPR,
    NEW_EXPR,
    AWAIT_EXPR,
    PAREN_EXPR,
    LITERAL,
    ARG_LIST,

    PRE_IF_DIR,
    PRE_ELSIF_CLAUSE,
    PRE_ELSE_CLAUSE,
    PRE_REGION_DIR,
    PRE_DELETE_DIR,
    PRE_INSERT_DIR,
    PRE_TORN_IF_PART,
    PRE_EXPR,
    PRE_LOGICAL_EXPR,
    PRE_LOGICAL_OPERAND,
    PRE_SYMBOL,
    PRE_BOOL_OP,

    SDBL_QUERY_PACKAGE,
    SDBL_SELECT_QUERY,
    SDBL_SUBQUERY,
    SDBL_UNION_CLAUSE,
    SDBL_QUERY,
    SDBL_LIMITATIONS,
    SDBL_TOP_CLAUSE,

    SDBL_SELECT_CLAUSE,
    SDBL_FIELD_LIST,
    SDBL_SELECTED_FIELD,
    SDBL_ALIAS,
    SDBL_ASTERISK_FIELD,
    SDBL_INTO_CLAUSE,
    SDBL_TEMP_TABLE_NAME,

    SDBL_FROM_CLAUSE,
    SDBL_DATA_SOURCE,
    SDBL_TABLE_REF,
    SDBL_WHERE_CLAUSE,

    SDBL_EXPR,
    SDBL_LOGICAL_OR_EXPR,
    SDBL_LOGICAL_AND_EXPR,
    SDBL_NOT_EXPR,
    SDBL_COMPARISON_EXPR,
    SDBL_IN_EXPR,
    SDBL_IN_HIERARCHY_EXPR,
    SDBL_IS_NULL_EXPR,
    SDBL_BETWEEN_EXPR,
    SDBL_LIKE_EXPR,
    SDBL_REFS_EXPR,
    SDBL_ADDITIVE_EXPR,
    SDBL_MULTIPLICATIVE_EXPR,
    SDBL_UNARY_EXPR,
    SDBL_PAREN_EXPR,
    SDBL_TUPLE_EXPR,
    SDBL_SUBQUERY_EXPR,

    SDBL_COLUMN_REF,
    SDBL_INLINE_TABLE_FIELDS,
    SDBL_FUNCTION_CALL,
    SDBL_CASE_EXPR,
    SDBL_WHEN_CLAUSE,
    SDBL_LITERAL,
    SDBL_MULTI_STRING,
    SDBL_PARAMETER,
    SDBL_TYPE,

    SDBL_JOIN_CLAUSE,
    SDBL_GROUP_CLAUSE,
    SDBL_ORDER_CLAUSE,
    SDBL_HAVING_CLAUSE,
    SDBL_FOR_UPDATE,
    SDBL_INDEX_BY,
    SDBL_AUTOORDER,
    SDBL_TOTALS_BY,

    SDBL_DROP_QUERY,

    SDBL_QUERY_EXTENSION,

    SDBL_MISSING_ARG,

    SDBL_ERROR,

    ERROR,

    #[doc(hidden)]
    __LAST,
}

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | SyntaxKind::NEWLINE | SyntaxKind::BOM
        )
    }

    pub fn is_keyword(self) -> bool {
        matches!(
            self,
            SyntaxKind::KW_PROCEDURE
                | SyntaxKind::KW_END_PROCEDURE
                | SyntaxKind::KW_FUNCTION
                | SyntaxKind::KW_END_FUNCTION
                | SyntaxKind::KW_EXPORT
                | SyntaxKind::KW_VAL
                | SyntaxKind::KW_IF
                | SyntaxKind::KW_THEN
                | SyntaxKind::KW_ELSIF
                | SyntaxKind::KW_ELSE
                | SyntaxKind::KW_END_IF
                | SyntaxKind::KW_FOR
                | SyntaxKind::KW_EACH
                | SyntaxKind::KW_IN
                | SyntaxKind::KW_TO
                | SyntaxKind::KW_WHILE
                | SyntaxKind::KW_DO
                | SyntaxKind::KW_END_DO
                | SyntaxKind::KW_RETURN
                | SyntaxKind::KW_CONTINUE
                | SyntaxKind::KW_BREAK
                | SyntaxKind::KW_GOTO
                | SyntaxKind::KW_TRY
                | SyntaxKind::KW_EXCEPT
                | SyntaxKind::KW_END_TRY
                | SyntaxKind::KW_RAISE
                | SyntaxKind::KW_VAR
                | SyntaxKind::KW_NEW
                | SyntaxKind::KW_EXECUTE
                | SyntaxKind::KW_ADD_HANDLER
                | SyntaxKind::KW_REMOVE_HANDLER
                | SyntaxKind::KW_ASYNC
                | SyntaxKind::KW_AWAIT
                | SyntaxKind::KW_AND
                | SyntaxKind::KW_OR
                | SyntaxKind::KW_NOT
                | SyntaxKind::KW_TRUE
                | SyntaxKind::KW_FALSE
                | SyntaxKind::KW_UNDEFINED
                | SyntaxKind::KW_NULL
        )
    }

    pub fn is_name_token(self) -> bool {
        self == SyntaxKind::IDENT || self.is_keyword()
    }

    pub fn is_literal(self) -> bool {
        matches!(
            self,
            SyntaxKind::FLOAT
                | SyntaxKind::DECIMAL
                | SyntaxKind::STRING
                | SyntaxKind::STRING_START
                | SyntaxKind::STRING_TAIL
                | SyntaxKind::STRING_PART
                | SyntaxKind::DATE
                | SyntaxKind::KW_TRUE
                | SyntaxKind::KW_FALSE
                | SyntaxKind::KW_UNDEFINED
                | SyntaxKind::KW_NULL
        )
    }

    pub fn is_string_literal(self) -> bool {
        matches!(
            self,
            SyntaxKind::STRING
                | SyntaxKind::STRING_START
                | SyntaxKind::STRING_TAIL
                | SyntaxKind::STRING_PART
        )
    }

    pub fn is_number_literal(self) -> bool {
        matches!(self, SyntaxKind::DECIMAL | SyntaxKind::FLOAT | SyntaxKind::DATE)
    }

    pub fn is_boolean_literal(self) -> bool {
        matches!(self, SyntaxKind::KW_TRUE | SyntaxKind::KW_FALSE)
    }

    /// Вид — инструкция препроцессора.
    ///
    /// Разряд закрыт: перечислены все инструкции, включая четыре, законные
    /// только в расширениях конфигурации. Неучтённый вид не выпадает в другой
    /// предикат, а остаётся неклассифицированным, и единственный потребитель
    /// разряда — подсветка — на нём просто ничего не возвращает. Полноту
    /// держит перебором `the_preprocessor_predicate_covers_every_instruction_kind`.
    pub fn is_preprocessor(self) -> bool {
        matches!(
            self,
            SyntaxKind::PRE_IF
                | SyntaxKind::PRE_ELSIF
                | SyntaxKind::PRE_ELSE
                | SyntaxKind::PRE_END_IF
                | SyntaxKind::PRE_REGION
                | SyntaxKind::PRE_END_REGION
                | SyntaxKind::PRE_INSERT
                | SyntaxKind::PRE_END_INSERT
                | SyntaxKind::PRE_DELETE
                | SyntaxKind::PRE_END_DELETE
        )
    }

    pub fn is_annotation(self) -> bool {
        matches!(
            self,
            SyntaxKind::ANN_AT_CLIENT
                | SyntaxKind::ANN_AT_SERVER
                | SyntaxKind::ANN_AT_SERVER_NO_CONTEXT
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER
                | SyntaxKind::ANN_BEFORE
                | SyntaxKind::ANN_AFTER
                | SyntaxKind::ANN_AROUND
                | SyntaxKind::ANN_CHANGE_AND_VALIDATE
                | SyntaxKind::ANN_CUSTOM
        )
    }

    /// Аннотация — директива компиляции (`&НаКлиенте`, `&НаСервере`, …).
    ///
    /// Подмножество `is_annotation`: директива задаёт контекст исполнения
    /// метода, а аннотации расширений (`&Перед`, `&После`, `&Вместо`,
    /// `&ИзменениеИКонтроль`) и пользовательские аннотации — нет.
    pub fn is_compilation_directive(self) -> bool {
        matches!(
            self,
            SyntaxKind::ANN_AT_CLIENT
                | SyntaxKind::ANN_AT_SERVER
                | SyntaxKind::ANN_AT_SERVER_NO_CONTEXT
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER
        )
    }

    /// Ключевое слово объявления: заголовок и конец метода, объявление
    /// переменной, параметр по значению, признак экспорта и асинхронности.
    ///
    /// Подмножество `is_keyword`; остальные ключевые слова управляют
    /// исполнением (`Если`, `Цикл`, `Возврат`, …) или строят выражения.
    pub fn is_declaration_keyword(self) -> bool {
        matches!(
            self,
            SyntaxKind::KW_PROCEDURE
                | SyntaxKind::KW_END_PROCEDURE
                | SyntaxKind::KW_FUNCTION
                | SyntaxKind::KW_END_FUNCTION
                | SyntaxKind::KW_EXPORT
                | SyntaxKind::KW_VAL
                | SyntaxKind::KW_VAR
                | SyntaxKind::KW_ASYNC
        )
    }

    pub fn is_operator(self) -> bool {
        matches!(
            self,
            SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT
                | SyntaxKind::EQ
                | SyntaxKind::NEQ
                | SyntaxKind::LT
                | SyntaxKind::LE
                | SyntaxKind::GT
                | SyntaxKind::GE
        )
    }
}

impl From<u16> for SyntaxKind {
    fn from(value: u16) -> Self {
        assert!(value < SyntaxKind::__LAST as u16, "SyntaxKind value out of range: {}", value);
        unsafe { std::mem::transmute(value) }
    }
}

impl From<SyntaxKind> for u16 {
    fn from(kind: SyntaxKind) -> Self {
        kind as u16
    }
}

impl fmt::Display for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let kind = SyntaxKind::KW_PROCEDURE;
        let raw: u16 = kind.into();
        let restored: SyntaxKind = raw.into();
        assert_eq!(kind, restored);
    }

    #[test]
    fn test_is_trivia() {
        assert!(SyntaxKind::WHITESPACE.is_trivia());
        assert!(SyntaxKind::COMMENT.is_trivia());
        assert!(SyntaxKind::NEWLINE.is_trivia());
        assert!(!SyntaxKind::IDENT.is_trivia());
    }

    #[test]
    fn test_is_keyword() {
        assert!(SyntaxKind::KW_PROCEDURE.is_keyword());
        assert!(SyntaxKind::KW_IF.is_keyword());
        assert!(!SyntaxKind::IDENT.is_keyword());
    }

    #[test]
    fn test_is_literal() {
        assert!(SyntaxKind::DECIMAL.is_literal());
        assert!(SyntaxKind::STRING.is_literal());
        assert!(SyntaxKind::KW_TRUE.is_literal());
        assert!(!SyntaxKind::IDENT.is_literal());
    }

    #[test]
    fn test_is_name_token() {
        assert!(SyntaxKind::IDENT.is_name_token());
        assert!(SyntaxKind::KW_EXECUTE.is_name_token());
        assert!(SyntaxKind::KW_NEW.is_name_token());
        assert!(SyntaxKind::KW_IF.is_name_token());
        assert!(!SyntaxKind::WHITESPACE.is_name_token());
        assert!(!SyntaxKind::DOT.is_name_token());
        assert!(!SyntaxKind::DECIMAL.is_name_token());
        assert!(!SyntaxKind::STRING.is_name_token());
    }
}
