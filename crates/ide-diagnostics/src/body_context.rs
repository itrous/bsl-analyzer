//! The view a check of ONE body gets.
//!
//! A body check reads the body's own lowering, syntax and dataflow, plus the
//! position-free [`AnalysisContext`]; it cannot reach the file's positional
//! state, so its result depends on the method's text and the module's
//! declarations only — the property the per-method diagnostics memo relies on.
//! Ranges are relative to the body's own root (`LocalRange`): the detached
//! method node for a method, the file root for module-level code.

use std::sync::{Arc, OnceLock};

use hir::{
    Body, BodySourceMap, DefWithBodyId, InferOwnerResult, LocalRange, LowerResult, MethodDecl,
    MethodId, ModuleId,
};
use syntax::ast::{self, AstNode};
use syntax::{NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, WalkEvent};

use crate::AnalysisContext;

pub struct BodyContext<'a> {
    analysis: &'a AnalysisContext<'a>,
    owner: DefWithBodyId,
    root: SyntaxNode,
    lower: &'a LowerResult,
    decl: OnceLock<Option<Arc<MethodDecl>>>,
    line_index: OnceLock<Arc<line_index::LineIndex>>,
}

impl<'a> std::ops::Deref for BodyContext<'a> {
    type Target = AnalysisContext<'a>;

    fn deref(&self) -> &Self::Target {
        self.analysis
    }
}

impl<'a> BodyContext<'a> {
    /// `root` is the body's own root — the detached method node for a method,
    /// the file root for module-level code — and `lower` its lowering from
    /// that same root, so both speak `LocalRange` in the same coordinates.
    pub fn new(
        analysis: &'a AnalysisContext<'a>,
        owner: DefWithBodyId,
        root: SyntaxNode,
        lower: &'a LowerResult,
    ) -> Self {
        Self { analysis, owner, root, lower, decl: OnceLock::new(), line_index: OnceLock::new() }
    }

    pub fn owner(&self) -> DefWithBodyId {
        self.owner
    }

    pub fn module_id(&self) -> ModuleId {
        ModuleId::new(self.file_id)
    }

    /// `None` for module-level code.
    pub fn method_id(&self) -> Option<MethodId> {
        match self.owner {
            DefWithBodyId::Method(local_id) => {
                Some(MethodId { module: self.module_id(), local_id })
            }
            DefWithBodyId::ModuleCode => None,
        }
    }

    pub fn is_module_code(&self) -> bool {
        matches!(self.owner, DefWithBodyId::ModuleCode)
    }

    /// The method's declaration as its readers see it; `None` for module code.
    pub fn decl(&self) -> Option<&MethodDecl> {
        let method_id = self.method_id()?;
        self.decl.get_or_init(|| self.analysis.interface_method(method_id)).as_deref()
    }

    /// The method's name token — an identifier or the keyword standing in for
    /// one — or the whole method when the name is missing: the same choice the
    /// item tree makes for its `name_range`. `None` for module code.
    pub fn method_name_range(&self) -> Option<LocalRange> {
        if self.is_module_code() {
            return None;
        }
        let name = ast::ProcedureDef::cast(self.root.clone())
            .and_then(|def| def.name_or_keyword())
            .or_else(|| {
                ast::FunctionDef::cast(self.root.clone()).and_then(|def| def.name_or_keyword())
            })
            .map(|token| token.text_range())
            .unwrap_or_else(|| self.root.text_range());
        Some(LocalRange::of_detached_node(name))
    }

    pub fn body(&self) -> &Body {
        &self.lower.body
    }

    pub fn body_arc(&self) -> &Arc<Body> {
        &self.lower.body
    }

    pub fn source_map(&self) -> &BodySourceMap {
        &self.lower.source_map
    }

    pub fn lower(&self) -> &LowerResult {
        self.lower
    }

    /// The body's own root: every range this context hands out is relative to
    /// it, and every node it yields lies under it.
    pub fn root(&self) -> &SyntaxNode {
        &self.root
    }

    /// Positions in the body's own text. Module code speaks file coordinates,
    /// so it reuses the file's memoised index instead of materialising the
    /// whole file text; a method builds its own from the method text only —
    /// the file index would tie the per-method memo to every edit of the file.
    pub fn line_index(&self) -> &line_index::LineIndex {
        self.line_index.get_or_init(|| match self.owner {
            DefWithBodyId::ModuleCode => self.analysis.provider().line_index(self.file_id),
            DefWithBodyId::Method(_) => {
                Arc::new(line_index::LineIndex::new(&self.root.text().to_string()))
            }
        })
    }

    pub fn text_of(&self, range: LocalRange) -> String {
        self.root.text().slice(range.in_root()).to_string()
    }

    pub fn range_of(&self, node: &SyntaxNode) -> LocalRange {
        LocalRange::of_detached_node(node.text_range())
    }

    pub fn token_range(&self, token: &SyntaxToken) -> LocalRange {
        LocalRange::of_detached_node(token.text_range())
    }

    /// Every node of the body, the root included. For module-level code the
    /// walk leaves method subtrees to their own bodies, so each node of the
    /// file belongs to exactly one body.
    pub fn nodes(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        let skip_methods = self.is_module_code();
        let mut preorder = self.root.preorder();
        std::iter::from_fn(move || loop {
            match preorder.next()? {
                WalkEvent::Enter(node) => {
                    if skip_methods && is_method_node(&node) {
                        preorder.skip_subtree();
                        continue;
                    }
                    return Some(node);
                }
                WalkEvent::Leave(_) => {}
            }
        })
    }

    /// Every token of the body in source order — neighbours in this stream are
    /// neighbours in the text — under the same ownership rule as
    /// [`Self::nodes`].
    pub fn tokens(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        let skip_methods = self.is_module_code();
        let mut preorder = self.root.preorder_with_tokens();
        std::iter::from_fn(move || loop {
            match preorder.next()? {
                WalkEvent::Enter(NodeOrToken::Token(token)) => return Some(token),
                WalkEvent::Enter(NodeOrToken::Node(node)) => {
                    if skip_methods && is_method_node(&node) {
                        preorder.skip_subtree();
                    }
                }
                WalkEvent::Leave(_) => {}
            }
        })
    }

    pub fn infer(&self) -> InferOwnerResult {
        self.analysis.infer_owner(self.owner)
    }

    /// Where the value of `expr` came from, proven on every path that reaches it in this body.
    pub fn value_origin(&self, expr: hir::ExprId) -> hir::ValueOrigin {
        self.analysis.provider().value_origin(self.file_id, self.owner(), expr)
    }

    pub fn arg_diagnostics(&self) -> Arc<Vec<hir::InferenceDiagnostic>> {
        self.analysis.arg_diagnostics_of(self.owner)
    }

    pub fn cfg(&self) -> Arc<hir::cfg::ControlFlowGraph> {
        match self.method_id() {
            Some(method_id) => self.analysis.method_cfg(method_id),
            None => self.analysis.module_level_cfg(),
        }
    }

    pub fn reaching_definitions(
        &self,
    ) -> Option<Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        match self.method_id() {
            Some(method_id) => self.analysis.reaching_definitions(method_id),
            None => self.analysis.module_code_reaching_definitions(),
        }
    }

    /// Path termination is a question about a function's body; module code
    /// has no return to reach.
    pub fn path_terminates(
        &self,
    ) -> Option<Arc<hir::dataflow::path_terminates::PathTerminatesResult>> {
        self.method_id().and_then(|method_id| self.analysis.method_path_terminates(method_id))
    }

    pub fn hir_metrics(&self) -> Arc<hir::metrics::HirMethodMetrics> {
        match self.method_id() {
            Some(method_id) => self.analysis.method_hir_metrics(method_id),
            None => self
                .analysis
                .module_code_hir_metrics()
                .unwrap_or_else(|| Arc::new(hir::metrics::HirMethodMetrics::default())),
        }
    }

    /// `None` for module code, whose complexity no check reports.
    pub fn cyclomatic(&self) -> Option<u32> {
        self.method_id().map(|method_id| self.analysis.method_cyclomatic(method_id))
    }

    pub fn security_state(
        &self,
    ) -> Option<Arc<hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>>>
    {
        match self.method_id() {
            Some(method_id) => self.analysis.method_security_state(method_id),
            None => self.analysis.module_code_security_state(),
        }
    }

    pub fn effect_summary(&self) -> Option<Arc<hir::dataflow::effect_summary::EffectSummary>> {
        self.method_id().map(|method_id| self.analysis.method_effect_summary(method_id))
    }
}

pub(crate) fn is_method_node(node: &SyntaxNode) -> bool {
    matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
}
