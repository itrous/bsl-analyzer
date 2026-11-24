use std::collections::BTreeSet;

use bsl_platform::{ContextAvailability, PlatformGlobalCatalog, PlatformGlobalKind};
use check_contract::{CheckRequest, InputKind, ModuleContext, ModuleType, Status};
use stdx::case::CaseExt;
use syntax::ast::{self, AstNode};
use syntax::{SyntaxKind, SyntaxNode};

use crate::{source_position, AdmittedModule, SourcePosition, MAX_MODULE_BYTES};

/// Why the limited carrier cannot accept this input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionReason {
    /// The request does not satisfy the unchanged check contract.
    InvalidRequest,
    /// Only complete modules enter this backend.
    InputKind,
    /// An explicit synthetic object context is required.
    ContextRequired,
    /// The requested module kind is outside the synthetic object context.
    ModuleType,
    /// The module is empty.
    EmptySource,
    /// Input exceeds either the caller's bound or the fixed carrier ceiling.
    SourceTooLarge,
    /// The parser could not produce a complete tree.
    IncompleteSyntax,
    /// An executable statement occurs outside every method, in any branch.
    MainProgram,
    /// A client or asynchronous context has not been proved by this backend.
    ClientContext,
    /// Conditional or extension directives are outside the admitted subset.
    UnsupportedDirective,
    /// A declaration form is outside the admitted subset.
    UnsupportedDeclaration,
    /// A reference requires an owner or a callable surface the carrier cannot prove.
    OwnerContext,
}

/// Admission refusal; it never claims native `invalid` or `valid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionError {
    /// Stable refusal category.
    pub reason: AdmissionReason,
    /// Original UTF-16 source location, when the refusal refers to source text.
    pub position: Option<SourcePosition>,
}

impl AdmissionError {
    /// The existing check status for this refusal, without a new wire version.
    pub fn status(&self) -> Status {
        match self.reason {
            AdmissionReason::ContextRequired
            | AdmissionReason::ClientContext
            | AdmissionReason::OwnerContext => Status::ContextRequired,
            _ => Status::Unsupported,
        }
    }
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.reason {
            AdmissionReason::InvalidRequest => "The request does not satisfy check contract v1",
            AdmissionReason::InputKind => "The EPF carrier accepts complete modules only",
            AdmissionReason::ContextRequired => {
                "An explicit synthetic object module context is required"
            }
            AdmissionReason::ModuleType => "This module kind requires a different check backend",
            AdmissionReason::EmptySource => "The module source is empty",
            AdmissionReason::SourceTooLarge => {
                "The module source exceeds the configured or fixed byte limit"
            }
            AdmissionReason::IncompleteSyntax => {
                "A complete AST is required before creating an EPF carrier"
            }
            AdmissionReason::MainProgram => "Executable module-level statements cannot be packaged",
            AdmissionReason::ClientContext => {
                "The source requires an unverified client or asynchronous context"
            }
            AdmissionReason::UnsupportedDirective => {
                "This directive is outside the admitted server module subset"
            }
            AdmissionReason::UnsupportedDeclaration => {
                "This declaration form is outside the admitted module subset"
            }
            AdmissionReason::OwnerContext => {
                "The source requires names or owner context not provided by the carrier"
            }
        })?;
        if let Some(position) = self.position {
            write!(f, " (line {}, UTF-16 column {})", position.line, position.column)?;
        }
        Ok(())
    }
}

impl std::error::Error for AdmissionError {}

fn refusal(source: &str, reason: AdmissionReason, offset: Option<usize>) -> AdmissionError {
    AdmissionError { reason, position: offset.and_then(|offset| source_position(source, offset)) }
}

fn at(source: &str, reason: AdmissionReason, node: &SyntaxNode) -> AdmissionError {
    refusal(source, reason, Some(node.text_range().start().into()))
}

/// Proves the AST belongs to the supported synthetic server object subset.
/// All branches are inspected before any carrier is constructed.
pub fn admit(request: &CheckRequest) -> Result<AdmittedModule<'_>, AdmissionError> {
    let source = request.code.as_str();
    if source.len() > MAX_MODULE_BYTES || source.len() as u64 > request.limits.max_code_bytes {
        return Err(refusal(source, AdmissionReason::SourceTooLarge, None));
    }
    request.validate().map_err(|_| refusal(source, AdmissionReason::InvalidRequest, None))?;
    if request.input_kind != InputKind::Module {
        return Err(refusal(source, AdmissionReason::InputKind, None));
    }
    if request.module_type.is_some_and(|kind| kind != ModuleType::Object) {
        return Err(refusal(source, AdmissionReason::ModuleType, None));
    }
    if !matches!(
        request.context,
        Some(ModuleContext::Synthetic { module_type: ModuleType::Object })
    ) {
        return Err(refusal(source, AdmissionReason::ContextRequired, None));
    }
    if source.trim_start_matches('\u{feff}').trim().is_empty() {
        return Err(refusal(source, AdmissionReason::EmptySource, None));
    }
    let parsed = parser::parse(source);
    if let Some(error) = parsed.errors().first() {
        return Err(refusal(
            source,
            AdmissionReason::IncompleteSyntax,
            Some(error.range().start().into()),
        ));
    }
    let root = parsed.syntax_node();
    if root.text() != source {
        return Err(refusal(source, AdmissionReason::IncompleteSyntax, None));
    }
    if let Some(error) = root.descendants().find(|node| node.kind() == SyntaxKind::ERROR) {
        return Err(at(source, AdmissionReason::IncompleteSyntax, &error));
    }
    // Inactive branches are still inspected: preprocessing cannot hide initialization.
    for node in root.descendants() {
        if is_statement(node.kind()) && enclosing_method(&node).is_none() {
            return Err(at(source, AdmissionReason::MainProgram, &node));
        }
    }
    for token in root.descendants_with_tokens().filter_map(|element| element.into_token()) {
        if matches!(
            token.kind(),
            SyntaxKind::ANN_AT_CLIENT
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER
                | SyntaxKind::ANN_AT_CLIENT_AT_SERVER_NO_CONTEXT
                | SyntaxKind::KW_ASYNC
                | SyntaxKind::KW_AWAIT
        ) {
            return Err(refusal(
                source,
                AdmissionReason::ClientContext,
                Some(token.text_range().start().into()),
            ));
        }
        if token.kind() == SyntaxKind::BOM && token.text_range().start() != syntax::TextSize::new(0)
        {
            return Err(refusal(
                source,
                AdmissionReason::UnsupportedDeclaration,
                Some(token.text_range().start().into()),
            ));
        }
    }
    for node in root.descendants() {
        if matches!(
            node.kind(),
            SyntaxKind::PRE_IF_DIR
                | SyntaxKind::PRE_INSERT_DIR
                | SyntaxKind::PRE_DELETE_DIR
                | SyntaxKind::PRE_TORN_IF_PART
                | SyntaxKind::ANNOTATION
                | SyntaxKind::COMPILER_DIRECTIVE
        ) {
            return Err(at(source, AdmissionReason::UnsupportedDirective, &node));
        }
    }
    for child in root.children() {
        if !matches!(
            child.kind(),
            SyntaxKind::PROCEDURE_DEF
                | SyntaxKind::FUNCTION_DEF
                | SyntaxKind::VAR_DEF
                | SyntaxKind::PRE_REGION_DIR
        ) {
            return Err(at(source, AdmissionReason::UnsupportedDeclaration, &child));
        }
    }
    if let Some(token) = root
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| !token.kind().is_trivia() && !token.text().is_empty())
    {
        return Err(refusal(
            source,
            AdmissionReason::IncompleteSyntax,
            Some(token.text_range().start().into()),
        ));
    }
    for param in root.descendants().filter_map(ast::Param::cast) {
        if let Some(value) = param.default_value_expr() {
            if value.descendants().any(|node| {
                !matches!(
                    node.kind(),
                    SyntaxKind::EXPR
                        | SyntaxKind::PAREN_EXPR
                        | SyntaxKind::UNARY_EXPR
                        | SyntaxKind::LITERAL
                )
            }) {
                return Err(at(source, AdmissionReason::UnsupportedDeclaration, &value));
            }
        }
    }
    check_names(source, &root)?;
    Ok(AdmittedModule { source })
}

fn enclosing_method(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.ancestors()
        .skip(1)
        .find(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
}

fn is_statement(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::ASSIGN_STMT
            | SyntaxKind::CALL_STMT
            | SyntaxKind::RETURN_STMT
            | SyntaxKind::IF_STMT
            | SyntaxKind::WHILE_STMT
            | SyntaxKind::FOR_STMT
            | SyntaxKind::FOR_EACH_STMT
            | SyntaxKind::TRY_STMT
            | SyntaxKind::RAISE_STMT
            | SyntaxKind::EXECUTE_STMT
            | SyntaxKind::BREAK_STMT
            | SyntaxKind::CONTINUE_STMT
            | SyntaxKind::GOTO_STMT
            | SyntaxKind::LABEL_STMT
            | SyntaxKind::ADD_HANDLER_STMT
            | SyntaxKind::REMOVE_HANDLER_STMT
    )
}

fn variable_names(scope: &SyntaxNode) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for node in scope.descendants() {
        if let Some(variable) = ast::VarDef::cast(node.clone()) {
            names.extend(variable.names().map(|name| name.text().fold_lower()));
        }
        if let Some(param) = ast::Param::cast(node.clone()) {
            names.extend(param.name().map(|name| name.text().fold_lower()));
        }
        if matches!(node.kind(), SyntaxKind::FOR_STMT | SyntaxKind::FOR_EACH_STMT) {
            names.extend(
                node.children_with_tokens()
                    .filter_map(|element| element.into_token())
                    .find(|token| token.kind() == SyntaxKind::IDENT)
                    .map(|token| token.text().fold_lower()),
            );
        }
        if node.kind() == SyntaxKind::ASSIGN_STMT {
            if let Some(target) = node.children().next().map(unwrap_expression) {
                if target.kind() == SyntaxKind::IDENT {
                    if let Some(name) = identifier_name(&target) {
                        names.insert(name);
                    }
                }
            }
        }
    }
    names
}

fn unwrap_expression(mut node: SyntaxNode) -> SyntaxNode {
    while matches!(node.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR) {
        let mut children = node.children();
        let Some(child) = children.next() else { break };
        if children.next().is_some() {
            break;
        }
        node = child;
    }
    node
}

fn identifier_name(node: &SyntaxNode) -> Option<String> {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == SyntaxKind::IDENT)
        .map(|token| token.text().fold_lower())
}

fn is_callee(node: &SyntaxNode) -> bool {
    let mut expression = node.clone();
    while let Some(parent) = expression.parent() {
        if matches!(parent.kind(), SyntaxKind::EXPR | SyntaxKind::PAREN_EXPR) {
            expression = parent;
        } else {
            return parent.kind() == SyntaxKind::CALL_EXPR
                && parent.children().next().as_ref() == Some(&expression);
        }
    }
    false
}

fn check_names(source: &str, root: &SyntaxNode) -> Result<(), AdmissionError> {
    let methods: BTreeSet<_> = root
        .children()
        .filter_map(|node| {
            ast::ProcedureDef::cast(node.clone())
                .and_then(|method| method.name_or_keyword())
                .or_else(|| {
                    ast::FunctionDef::cast(node).and_then(|method| method.name_or_keyword())
                })
                .map(|name| name.text().fold_lower())
        })
        .collect();
    let globals: BTreeSet<_> = root
        .children()
        .filter_map(ast::VarDef::cast)
        .flat_map(|variable| {
            variable.names().map(|name| name.text().fold_lower()).collect::<Vec<_>>()
        })
        .collect();
    let catalog = PlatformGlobalCatalog::instance();
    for method in root
        .children()
        .filter(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
    {
        let locals = variable_names(&method);
        for name_node in method.descendants().filter(|node| node.kind() == SyntaxKind::IDENT) {
            let Some(name) = identifier_name(&name_node) else {
                return Err(at(source, AdmissionReason::UnsupportedDeclaration, &name_node));
            };
            if matches!(name.as_str(), "этотобъект" | "thisobject") {
                return Err(at(source, AdmissionReason::OwnerContext, &name_node));
            }
            let callee = is_callee(&name_node);
            if (callee && methods.contains(&name))
                || (!callee && (locals.contains(&name) || globals.contains(&name)))
            {
                continue;
            }
            let known = catalog.lookup(&name);
            if known.is_some_and(|symbol| {
                !ContextAvailability::effective(symbol.context.as_ref()).server
            }) {
                return Err(at(source, AdmissionReason::ClientContext, &name_node));
            }
            let known = known.filter(|symbol| {
                if callee {
                    symbol.kind == PlatformGlobalKind::Function
                        && symbol.capabilities.callable == Some(true)
                } else {
                    symbol.kind != PlatformGlobalKind::Function
                        && symbol.capabilities.readable_as_value == Some(true)
                }
            });
            if known.is_none() || catalog.ambiguous_names().contains(name.as_str()) {
                return Err(at(source, AdmissionReason::OwnerContext, &name_node));
            }
        }
        for constructor in method.descendants().filter(|node| node.kind() == SyntaxKind::NEW_EXPR) {
            let name = constructor
                .children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == SyntaxKind::IDENT);
            let Some(name) = name else {
                return Err(at(source, AdmissionReason::UnsupportedDeclaration, &constructor));
            };
            let platform = bsl_platform::PlatformDataInner::instance();
            if platform.is_ambiguous_type_name(name.text())
                || !platform
                    .get_type(name.text())
                    .is_some_and(|ty| ContextAvailability::effective(ty.context.as_ref()).server)
            {
                return Err(at(source, AdmissionReason::OwnerContext, &constructor));
            }
        }
    }
    Ok(())
}
