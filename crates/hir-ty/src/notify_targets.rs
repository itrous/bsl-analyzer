use std::sync::Arc;

use base_db::FileIdInput;
use bsl_metadata::MdObject;
use bsl_platform::PlatformDataInner;
use bsl_types::kind::TypeKind;
use hir_def::body::events::for_each_expr_child;
use hir_def::body::Body;
use hir_def::call_graph::{
    CallerId, DynamicNotifySignatureAnalysisGap, ModuleCallSummary, NotifyConstructorCandidate,
    NotifyReceiverSyntax, NotifyReg, NotifyTarget,
};
use hir_def::conditional_tree::{ConditionalIdx, ConditionalTree};
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt};
use hir_def::{
    call_graph::NotifyRegistrationEnvironments, execution_env::EnvFlags, DefWithBodyId, ExprId,
    IdConversion, MethodKey, ModuleId, Name, StmtId,
};
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{TextRange, TextSize};
use vfs::FileId;

use crate::body_flow::BodyFlow;
use crate::db::HirDatabase;
use crate::infer::{infer_owner, BareNameOrigin, CallArgBinding, InferOwnerResult};
use crate::notify_array::{
    callback_arguments_for_binding, collect_notify_array_registrations, reachable_statements,
    NotifyArrayRegistration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCandidateKind {
    Static,
    Dynamic,
    DynamicArray,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCallbackSlot {
    Handler,
    ErrorHandler,
    Unknown,
}

/// Semantic verdict for `UnusedParameters`: may the platform invoke this callback with
/// a signature the author cannot trim? The projection answers it from the resolved
/// receiver, the source receiver shape and the candidate kind; diagnostics consume the
/// verdict and never re-derive it. It is deliberately separate from
/// [`NotifyCandidate::protects_local_method`], which answers whether the candidate keeps
/// a usage fact, not whether the declared signature belongs to the author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifySignatureProtection {
    /// The callback may be bound to a local signature; report no unused parameters.
    Protected,
    /// The callback is proven to lie outside the local signature's reach.
    NotProtected,
    /// The containing analysis stopped early; the caller must stay silent about the body.
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyCandidateReceiver {
    ThisObject,
    ForeignModule(Name),
    Unknown,
    NotCompiled,
}

/// One callback slot attributed to a specific notification constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyCandidate {
    pub caller: CallerId,
    pub constructor: ExprId,
    pub expression: ExprId,
    pub slot: NotifyCallbackSlot,
    pub kind: NotifyCandidateKind,
    pub callback_name: Name,
    pub receiver_syntax: NotifyReceiverSyntax,
    pub receiver: NotifyCandidateReceiver,
    /// Whether this candidate conservatively counts as a local method reference.
    pub protects_local_method: bool,
    /// Whether the constructor is reachable in its owner body.
    pub site_reachable: bool,
    /// True when alias analysis for the containing dynamic constructor exhausted its budget.
    pub analysis_incomplete: bool,
    pub registration: Option<NotifyReg>,
}

impl NotifyCandidate {
    /// Ready-made signature-protection verdict for diagnostics; see
    /// [`NotifySignatureProtection`].
    pub fn signature_protection(&self) -> NotifySignatureProtection {
        if self.analysis_incomplete {
            return NotifySignatureProtection::Indeterminate;
        }
        let syntax_may_be_local = matches!(
            self.receiver_syntax,
            NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::Other
        );
        let resolved_this_object = matches!(self.receiver, NotifyCandidateReceiver::ThisObject);
        let unresolved_dynamic_array = self.kind == NotifyCandidateKind::DynamicArray
            && matches!(self.receiver, NotifyCandidateReceiver::Unknown);
        if resolved_this_object || syntax_may_be_local || unresolved_dynamic_array {
            NotifySignatureProtection::Protected
        } else {
            NotifySignatureProtection::NotProtected
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticNotifyFacts {
    pub summary: Arc<ModuleCallSummary>,
    pub notify_candidates: Arc<Vec<NotifyCandidate>>,
}

/// Work limit shared by every pass of one semantic notification projection. A pass
/// indexes bodies, constructors and bindings once; each index entry, candidate visit
/// and handler visit spends from this limit. Exhaustion is a result, not an empty
/// answer: the projection then emits an [`NotifyCandidate::analysis_incomplete`]
/// marker so diagnostics stay conservative instead of reporting the partial data as
/// complete.
const NOTIFY_TARGET_WORK_BUDGET: usize = 131_072;
const NOTIFY_TARGET_CANCEL_INTERVAL: usize = 128;

struct NotifyWorkBudget<'a> {
    remaining: usize,
    since_cancel: usize,
    cancel: &'a mut dyn FnMut(),
    exhausted: bool,
}

impl<'a> NotifyWorkBudget<'a> {
    fn new(cancel: &'a mut dyn FnMut()) -> Self {
        Self::with_limit(cancel, NOTIFY_TARGET_WORK_BUDGET)
    }

    fn with_limit(cancel: &'a mut dyn FnMut(), limit: usize) -> Self {
        Self { remaining: limit, since_cancel: 0, cancel, exhausted: false }
    }

    fn spend(&mut self) -> bool {
        self.spend_by(1)
    }

    fn spend_by(&mut self, work: usize) -> bool {
        if self.exhausted || work > self.remaining {
            self.remaining = 0;
            self.exhausted = true;
            return false;
        }
        self.remaining -= work;
        self.since_cancel += work;
        if self.since_cancel >= NOTIFY_TARGET_CANCEL_INTERVAL {
            self.since_cancel %= NOTIFY_TARGET_CANCEL_INTERVAL;
            (self.cancel)();
        }
        true
    }

    fn exhausted(&self) -> bool {
        self.exhausted
    }
}

/// One indexed pass over the module's conditional tree. Conditional ranges are sorted
/// by start and shadowed in a max-end segment tree, so a candidate range finds its
/// innermost enclosing nodes without rescanning every conditional. Each index entry and
/// visited tree node spends from the shared budget and reaches the cancellation check.
struct ConditionalEnvIndex<'a> {
    tree: &'a ConditionalTree,
    nodes: Vec<(TextRange, ConditionalIdx)>,
    max_end: Vec<TextSize>,
    tree_size: usize,
    cache: FxHashMap<TextRange, (EnvFlags, EnvFlags)>,
}

impl<'a> ConditionalEnvIndex<'a> {
    fn new(tree: &'a ConditionalTree, budget: &mut NotifyWorkBudget<'_>) -> Option<Self> {
        let mut nodes = Vec::with_capacity(tree.len());
        for (idx, data) in tree.conditionals() {
            if !budget.spend() {
                return None;
            }
            nodes.push((data.range, idx));
        }
        nodes.sort_by_key(|(range, _)| range.start());
        let tree_size = nodes.len().next_power_of_two().max(1);
        let mut max_end = vec![TextSize::new(0); tree_size * 2];
        for (slot, (range, _)) in nodes.iter().enumerate() {
            if !budget.spend() {
                return None;
            }
            max_end[tree_size + slot] = range.end();
        }
        for slot in (1..tree_size).rev() {
            if !budget.spend() {
                return None;
            }
            max_end[slot] = max_end[slot * 2].max(max_end[slot * 2 + 1]);
        }
        Some(Self { tree, nodes, max_end, tree_size, cache: FxHashMap::default() })
    }

    fn tracking(
        &mut self,
        range: TextRange,
        budget: &mut NotifyWorkBudget<'_>,
    ) -> Option<(EnvFlags, EnvFlags)> {
        if let Some(&cached) = self.cache.get(&range) {
            if !budget.spend() {
                return None;
            }
            return Some(cached);
        }
        let prefix_end = self.nodes.partition_point(|(node, _)| node.start() <= range.start());
        let mut best: Option<(ConditionalIdx, TextSize)> = None;
        if prefix_end > 0
            && !self.collect_containing(
                1,
                0,
                self.tree_size,
                0,
                prefix_end,
                range.end(),
                budget,
                &mut best,
            )
        {
            return None;
        }
        let env =
            hir_def::execution_env::conditional_env_for_node(self.tree, best.map(|(idx, _)| idx));
        self.cache.insert(range, env);
        Some(env)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "The segment-tree descent carries the query bounds, the result slot and the shared budget."
    )]
    fn collect_containing(
        &self,
        node: usize,
        left: usize,
        right: usize,
        query_left: usize,
        query_right: usize,
        query_end: TextSize,
        budget: &mut NotifyWorkBudget<'_>,
        best: &mut Option<(ConditionalIdx, TextSize)>,
    ) -> bool {
        if right <= query_left || query_right <= left || self.max_end[node] < query_end {
            return true;
        }
        if !budget.spend() {
            return false;
        }
        if right - left == 1 {
            let Some(&(range, idx)) = self.nodes.get(left) else { return true };
            if range.end() >= query_end {
                let len = range.len();
                if best.is_none_or(|(_, best_len)| len < best_len) {
                    *best = Some((idx, len));
                }
            }
            return true;
        }
        let middle = left + (right - left) / 2;
        if !self.collect_containing(
            node * 2,
            left,
            middle,
            query_left,
            query_right,
            query_end,
            budget,
            best,
        ) {
            return false;
        }
        self.collect_containing(
            node * 2 + 1,
            middle,
            right,
            query_left,
            query_right,
            query_end,
            budget,
            best,
        )
    }
}

#[salsa::tracked(
    lru = 256,
    heap_size = semantic_notify_facts_heap,
    returns(clone)
)]
pub fn semantic_notify_facts_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<SemanticNotifyFacts> {
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::new(&mut cancel);
    semantic_notify_facts_with_budget(db, file_id_input, &mut budget)
}

/// Test hook: the same projection with an explicit work limit, so callers can
/// observe the conservative exhaustion result without a pathological fixture.
#[doc(hidden)]
pub fn semantic_notify_facts_with_work_limit<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
    work_limit: usize,
) -> Arc<SemanticNotifyFacts> {
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::with_limit(&mut cancel, work_limit);
    semantic_notify_facts_with_budget(db, file_id_input, &mut budget)
}

fn semantic_notify_facts_with_budget<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Arc<SemanticNotifyFacts> {
    let file_id = file_id_input.file_id(db);
    let raw = hir_def::queries::module_call_summary_query(db, file_id_input);
    if raw.notify_candidates.is_empty() {
        let mut summary = (*raw).clone();
        if !summary.name_literal_ref_sites.is_empty() {
            summary.name_literal_ref_sites.clear();
        }
        let mut notify_candidates = syntax_only_dynamic_candidates(
            &summary,
            &raw.notify_candidates,
            &[],
            &FxHashSet::default(),
            budget,
        );
        if !budget.exhausted() {
            notify_candidates.extend(incomplete_dynamic_candidates(
                &summary,
                &FxHashSet::default(),
                budget,
            ));
        }
        if budget.exhausted() {
            return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
        }
        return Arc::new(SemanticNotifyFacts {
            summary: Arc::new(summary),
            notify_candidates: Arc::new(notify_candidates),
        });
    }

    let module_id = ModuleId::new(file_id);
    let mut summary = (*raw).clone();
    let candidates = summary.notify_candidates.clone();
    let mut notify_candidates = Vec::new();
    let mut reachable_dynamic_constructors: FxHashSet<(CallerId, ExprId)> = FxHashSet::default();

    let bodies = db.module_bodies_ref(module_id);
    let mut bodies_by_caller: FxHashMap<CallerId, &Body> = FxHashMap::default();
    for (local_id, lower) in bodies.iter_lower_results() {
        let caller = CallerId::Method(local_id);
        if !budget.spend() {
            return finish_incomplete_facts(summary, notify_candidates, caller);
        }
        bodies_by_caller.insert(caller, lower.body());
    }
    if let Some(lower) = bodies.module_code_result() {
        if !budget.spend() {
            return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
        }
        bodies_by_caller.insert(CallerId::ModuleCode, lower.body());
    }

    let mut candidates_by_caller: FxHashMap<CallerId, Vec<usize>> = FxHashMap::default();
    for (index, candidate) in candidates.iter().enumerate() {
        if !budget.spend() {
            return finish_incomplete_facts(summary, notify_candidates, candidate.caller);
        }
        candidates_by_caller.entry(candidate.caller).or_default().push(index);
    }

    let platform = PlatformDataInner::instance();
    let metadata = db.module_metadata(module_id);
    let current_common_module =
        metadata.common_module.as_ref().map(|module| Name::new(module.name()));
    let conditional_tree = db.conditional_tree_ref(file_id);
    let Some(mut conditional_envs) = ConditionalEnvIndex::new(conditional_tree, budget) else {
        return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
    };

    for (local_id, lower) in bodies.iter_lower_results() {
        let caller = CallerId::Method(local_id);
        let Some(indices) = candidates_by_caller.remove(&caller) else { continue };
        normalize_owner_candidates(
            db,
            file_id,
            DefWithBodyId::Method(local_id),
            lower.body(),
            &indices,
            &candidates,
            platform,
            current_common_module.as_ref(),
            &mut conditional_envs,
            &mut summary.notify_regs,
            &mut notify_candidates,
            &mut reachable_dynamic_constructors,
            budget,
        );
        if budget.exhausted() {
            return finish_incomplete_facts(summary, notify_candidates, caller);
        }
    }

    if let Some(lower) = bodies.module_code_result() {
        if let Some(indices) = candidates_by_caller.remove(&CallerId::ModuleCode) {
            normalize_owner_candidates(
                db,
                file_id,
                DefWithBodyId::ModuleCode,
                lower.body(),
                &indices,
                &candidates,
                platform,
                current_common_module.as_ref(),
                &mut conditional_envs,
                &mut summary.notify_regs,
                &mut notify_candidates,
                &mut reachable_dynamic_constructors,
                budget,
            );
            if budget.exhausted() {
                return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
            }
        }
    }

    // Candidates without a readable owner body cannot establish a registration.
    summary.notify_candidates.clear();
    retain_notify_name_literal_sites(&mut summary, &notify_candidates, budget);
    if !budget.exhausted() {
        filter_uncompiled_notify_name_literals(&mut summary, &notify_candidates, budget);
    }
    if !budget.exhausted() {
        notify_candidates.extend(syntax_only_static_candidates(
            &summary,
            &candidates,
            &bodies_by_caller,
            &notify_candidates,
            budget,
        ));
    }
    if !budget.exhausted() {
        notify_candidates.extend(syntax_only_dynamic_candidates(
            &summary,
            &candidates,
            &notify_candidates,
            &reachable_dynamic_constructors,
            budget,
        ));
    }
    if !budget.exhausted() {
        notify_candidates.extend(incomplete_dynamic_candidates(
            &summary,
            &reachable_dynamic_constructors,
            budget,
        ));
    }
    if budget.exhausted() {
        return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
    }

    Arc::new(SemanticNotifyFacts {
        summary: Arc::new(summary),
        notify_candidates: Arc::new(notify_candidates),
    })
}

/// A marker with no source slot: consumers read only the uncertainty flags. It keeps
/// budget exhaustion from looking like a module without callback candidates.
fn budget_incomplete_candidate(caller: CallerId, constructor: ExprId) -> NotifyCandidate {
    NotifyCandidate {
        caller,
        constructor,
        expression: constructor,
        slot: NotifyCallbackSlot::Unknown,
        kind: NotifyCandidateKind::DynamicArray,
        callback_name: Name::new(""),
        receiver_syntax: NotifyReceiverSyntax::Other,
        receiver: NotifyCandidateReceiver::Unknown,
        protects_local_method: true,
        site_reachable: true,
        analysis_incomplete: true,
        registration: None,
    }
}

fn finish_incomplete_facts(
    mut summary: ModuleCallSummary,
    mut candidates: Vec<NotifyCandidate>,
    caller: CallerId,
) -> Arc<SemanticNotifyFacts> {
    let first_constructor = summary.notify_candidates.first().copied();
    let marker_constructor = first_constructor
        .map_or_else(|| ExprId::from_raw(0.into()), |candidate| candidate.constructor);
    // Leave the partial registrations in place, but make the module-level uncertainty
    // explicit in the field whose contract already tells consumers not to draw negative
    // conclusions from missing callbacks.
    if !summary.notify_dynamic_signature_analysis_incomplete.iter().any(|gap| gap.caller == caller)
    {
        summary.notify_dynamic_signature_analysis_incomplete.push(
            DynamicNotifySignatureAnalysisGap {
                caller,
                constructor: marker_constructor,
                constructor_range: first_constructor.map_or_else(
                    || TextRange::empty(TextSize::new(0)),
                    |candidate| candidate.range,
                ),
                array_name: Name::new(""),
            },
        );
    }
    summary.notify_candidates.clear();
    if !candidates.iter().any(|candidate| candidate.analysis_incomplete) {
        candidates.push(budget_incomplete_candidate(caller, marker_constructor));
    }
    Arc::new(SemanticNotifyFacts {
        summary: Arc::new(summary),
        notify_candidates: Arc::new(candidates),
    })
}

pub fn semantic_module_call_summary_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleCallSummary> {
    semantic_notify_facts_query(db, file_id_input).summary.clone()
}

pub fn semantic_notify_candidates_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<Vec<NotifyCandidate>> {
    semantic_notify_facts_query(db, file_id_input).notify_candidates.clone()
}

fn syntax_only_dynamic_candidates(
    summary: &ModuleCallSummary,
    constructors: &[NotifyConstructorCandidate],
    classified_candidates: &[NotifyCandidate],
    reachable_dynamic_constructors: &FxHashSet<(CallerId, ExprId)>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Vec<NotifyCandidate> {
    let mut constructors_by_range: FxHashMap<TextRange, &NotifyConstructorCandidate> =
        FxHashMap::default();
    for constructor in constructors {
        if !budget.spend() {
            return Vec::new();
        }
        constructors_by_range.entry(constructor.range).or_insert(constructor);
    }
    let mut classified: FxHashSet<(CallerId, ExprId, ExprId)> = FxHashSet::default();
    for candidate in classified_candidates {
        if !budget.spend() {
            return Vec::new();
        }
        classified.insert((candidate.caller, candidate.constructor, candidate.expression));
    }
    let mut result = Vec::new();
    for site in &summary.notify_dynamic_signature_handler_sites {
        if !budget.spend() {
            return result;
        }
        let Some(constructor) = constructors_by_range.get(&site.constructor_range) else {
            continue;
        };
        let key = (constructor.caller, constructor.constructor, site.callback_expr);
        if classified.contains(&key) {
            continue;
        }
        let site_reachable =
            reachable_dynamic_constructors.contains(&(constructor.caller, constructor.constructor));
        result.push(NotifyCandidate {
            caller: constructor.caller,
            constructor: constructor.constructor,
            expression: site.callback_expr,
            slot: NotifyCallbackSlot::Unknown,
            kind: NotifyCandidateKind::DynamicArray,
            callback_name: site.handler_name.clone(),
            receiver_syntax: NotifyReceiverSyntax::Other,
            receiver: NotifyCandidateReceiver::Unknown,
            protects_local_method: site_reachable,
            site_reachable,
            analysis_incomplete: false,
            registration: None,
        });
        classified.insert(key);
    }
    result
}

fn syntax_only_static_candidates(
    summary: &ModuleCallSummary,
    constructors: &[NotifyConstructorCandidate],
    bodies_by_caller: &FxHashMap<CallerId, &Body>,
    classified: &[NotifyCandidate],
    budget: &mut NotifyWorkBudget<'_>,
) -> Vec<NotifyCandidate> {
    let protect_source_fallback = !bsl_platform::has_callback_description_constructor_facts(
        bsl_platform::PlatformData::instance(),
    );
    let slots = bsl_platform::bundled_callback_description_slots();
    let mut source_names: FxHashSet<NormName> = FxHashSet::default();
    for source in summary
        .notify_signature_handlers
        .iter()
        .chain(&summary.notify_additional_signature_handlers)
    {
        if !budget.spend() {
            return Vec::new();
        }
        source_names.insert(NormName::intern(source.as_str()));
    }
    let mut classified_keys: FxHashSet<(CallerId, ExprId, ExprId)> = FxHashSet::default();
    for candidate in classified {
        if !budget.spend() {
            return Vec::new();
        }
        classified_keys.insert((candidate.caller, candidate.constructor, candidate.expression));
    }

    let mut result = Vec::new();
    for constructor in constructors {
        if !budget.spend() {
            return result;
        }
        let Some(&body) = bodies_by_caller.get(&constructor.caller) else { continue };
        let Expr::New { type_name, args } = body.expr(constructor.constructor) else {
            continue;
        };
        let (type_name, slot_offset, kind) = match type_name {
            Some(type_name) => (type_name.as_str(), 0, NotifyCandidateKind::Static),
            None => {
                let Some(first_arg) = args.first() else { continue };
                let Expr::Literal(Literal::String(type_name)) = body.expr_idx(*first_arg) else {
                    continue;
                };
                (type_name.as_str(), 1, NotifyCandidateKind::Dynamic)
            }
        };
        if !bsl_platform::is_callback_description_type_name(type_name) {
            continue;
        }
        for (slot, handler_arg, receiver_arg) in [
            (
                NotifyCallbackSlot::Handler,
                slots.handler + slot_offset,
                slots.receiver + slot_offset,
            ),
            (
                NotifyCallbackSlot::ErrorHandler,
                slots.error_handler + slot_offset,
                slots.error_receiver + slot_offset,
            ),
        ] {
            if !budget.spend() {
                return result;
            }
            let Some(&handler_expr) = args.get(handler_arg) else { continue };
            let Expr::Literal(Literal::String(name)) = body.expr_idx(handler_expr) else {
                continue;
            };
            let callback_name = Name::new(name);
            if !source_names.contains(&NormName::intern(callback_name.as_str())) {
                continue;
            }
            let expression = ExprId::from_idx(handler_expr);
            let key = (constructor.caller, constructor.constructor, expression);
            if classified_keys.contains(&key) {
                continue;
            }
            let receiver_syntax =
                args.get(receiver_arg).map_or(NotifyReceiverSyntax::Other, |&receiver| match body
                    .expr_idx(receiver)
                {
                    Expr::Path(name) => NotifyReceiverSyntax::for_bare_name(name),
                    _ => NotifyReceiverSyntax::Other,
                });
            if !matches!(
                receiver_syntax,
                NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::Other
            ) {
                continue;
            }
            result.push(NotifyCandidate {
                caller: constructor.caller,
                constructor: constructor.constructor,
                expression,
                slot,
                kind,
                callback_name,
                receiver_syntax,
                receiver: NotifyCandidateReceiver::Unknown,
                protects_local_method: protect_source_fallback,
                site_reachable: true,
                analysis_incomplete: false,
                registration: None,
            });
            classified_keys.insert(key);
        }
    }
    result
}

fn incomplete_dynamic_candidates(
    summary: &ModuleCallSummary,
    reachable_dynamic_constructors: &FxHashSet<(CallerId, ExprId)>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Vec<NotifyCandidate> {
    let mut result = Vec::new();
    for gap in &summary.notify_dynamic_signature_analysis_incomplete {
        if !budget.spend() {
            return result;
        }
        if !reachable_dynamic_constructors.contains(&(gap.caller, gap.constructor)) {
            continue;
        }
        result.push(NotifyCandidate {
            caller: gap.caller,
            constructor: gap.constructor,
            expression: gap.constructor,
            slot: NotifyCallbackSlot::Unknown,
            kind: NotifyCandidateKind::DynamicArray,
            callback_name: Name::new(""),
            receiver_syntax: NotifyReceiverSyntax::Other,
            receiver: NotifyCandidateReceiver::Unknown,
            protects_local_method: true,
            site_reachable: true,
            analysis_incomplete: true,
            registration: None,
        });
    }
    result
}

fn retain_notify_name_literal_sites(
    summary: &mut ModuleCallSummary,
    candidates: &[NotifyCandidate],
    budget: &mut NotifyWorkBudget<'_>,
) {
    let mut local_methods: FxHashMap<NormName, MethodKey> = FxHashMap::default();
    for method in &summary.methods {
        if !budget.spend() {
            return;
        }
        local_methods.entry(NormName::intern(method.name.as_str())).or_insert(method.local_id);
    }

    let mut notify_handler_methods = FxHashSet::default();
    let mut notify_callback_sites = FxHashSet::default();
    let mut local_callback_sites = FxHashSet::default();
    for registration in candidates.iter().filter_map(|candidate| candidate.registration.as_ref()) {
        if !budget.spend() {
            return;
        }
        let site = (registration.caller, registration.callback_expr);
        notify_callback_sites.insert(site);
        if matches!(&registration.target, NotifyTarget::Module(_)) {
            continue;
        }
        if let Some(&local_id) =
            local_methods.get(&NormName::intern(registration.callback_name.as_str()))
        {
            notify_handler_methods.insert(local_id);
        }
        if matches!(&registration.target, NotifyTarget::ThisObject) {
            local_callback_sites.insert(site);
        }
    }

    let mut retained_local_methods = FxHashSet::default();
    for site in &summary.name_literal_ref_sites {
        if !budget.spend() {
            return;
        }
        let key = (site.caller, site.expr);
        if !notify_callback_sites.contains(&key) || local_callback_sites.contains(&key) {
            retained_local_methods.insert(site.local_id);
        }
    }
    summary.name_literal_refs.retain(|method| retained_local_methods.contains(method));
    let mut retained_sites = Vec::with_capacity(summary.name_literal_ref_sites.len());
    for site in &summary.name_literal_ref_sites {
        if !budget.spend() {
            return;
        }
        if notify_handler_methods.contains(&site.local_id) {
            retained_sites.push(*site);
        }
    }
    summary.name_literal_ref_sites = retained_sites;
}

fn filter_uncompiled_notify_name_literals(
    summary: &mut ModuleCallSummary,
    candidates: &[NotifyCandidate],
    budget: &mut NotifyWorkBudget<'_>,
) {
    let mut uncompiled_callbacks = FxHashSet::default();
    for candidate in candidates {
        if !budget.spend() {
            return;
        }
        let Some(reg) = candidate.registration.as_ref() else { continue };
        if let NotifyTarget::NotCompiled { callback_expr, .. } = &reg.target {
            uncompiled_callbacks.insert((reg.caller, *callback_expr));
        }
    }
    if uncompiled_callbacks.is_empty() {
        return;
    }

    let mut uncompiled_methods = FxHashSet::default();
    let mut live_methods = FxHashSet::default();
    for site in &summary.name_literal_ref_sites {
        if !budget.spend() {
            return;
        }
        let method_ids = if uncompiled_callbacks.contains(&(site.caller, site.expr)) {
            &mut uncompiled_methods
        } else {
            &mut live_methods
        };
        method_ids.insert(site.local_id);
    }
    summary
        .name_literal_refs
        .retain(|method| !uncompiled_methods.contains(method) || live_methods.contains(method));
}

fn semantic_notify_facts_heap(facts: &Arc<SemanticNotifyFacts>) -> usize {
    hir_def::queries::module_call_summary_heap(&facts.summary)
        + facts.notify_candidates.len() * std::mem::size_of::<NotifyCandidate>()
}

#[allow(
    clippy::too_many_arguments,
    reason = "The projection keeps database context and caller-specific inputs separate from its output."
)]
fn normalize_owner_candidates(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
    candidate_indices: &[usize],
    candidates: &[NotifyConstructorCandidate],
    platform: &PlatformDataInner,
    current_common_module: Option<&Name>,
    conditional_envs: &mut ConditionalEnvIndex<'_>,
    registrations: &mut Vec<NotifyReg>,
    notify_candidates: &mut Vec<NotifyCandidate>,
    reachable_dynamic_constructors: &mut FxHashSet<(CallerId, ExprId)>,
    budget: &mut NotifyWorkBudget<'_>,
) {
    if !budget.spend() {
        return;
    }
    let inference = infer_owner(db, file_id, owner);
    let dead_stmts: FxHashSet<StmtId> = inference.dead_stmts().iter().copied().collect();
    if !budget.spend_by(dead_stmts.len()) {
        return;
    }
    let flow = match owner {
        DefWithBodyId::Method(local_id) => {
            let method = hir_def::MethodId { module: ModuleId::new(file_id), local_id };
            let input = hir_def::MethodIdInput::new(db, method);
            BodyFlow::for_method(db, input, db.method_body_ref(input))
        }
        DefWithBodyId::ModuleCode => {
            let bodies = db.module_bodies_ref(ModuleId::new(file_id));
            bodies.module_code_arc().map_or_else(
                || BodyFlow::detached(std::sync::Arc::new(body.clone()), false),
                |body| BodyFlow::for_module_code(db, file_id, body),
            )
        }
    };
    let cfg = flow.cfg(db);
    let reachable_stmts = reachable_statements(&cfg);
    let reaching_defs = flow.executable_reaching_defs(db);
    let array_registrations = collect_notify_array_registrations(
        db,
        body,
        &reachable_stmts,
        reaching_defs.as_deref(),
        &inference,
        platform,
    );

    let mut array_by_constructor: FxHashMap<ExprId, &NotifyArrayRegistration> =
        FxHashMap::default();
    for registration in &array_registrations {
        if !budget.spend() {
            return;
        }
        array_by_constructor.entry(registration.constructor).or_insert(registration);
    }
    let mut bindings_by_call: FxHashMap<ExprId, &CallArgBinding> = FxHashMap::default();
    for binding in inference.call_arg_bindings() {
        if !budget.spend() {
            return;
        }
        bindings_by_call.entry(binding.call_expr).or_insert(binding);
    }
    let Some(enclosing_stmts) = enclosing_stmt_index(body, budget) else { return };

    for &index in candidate_indices {
        if !budget.spend() {
            return;
        }
        let candidate = candidates[index];
        let Expr::New { type_name, args } = body.expr(candidate.constructor) else { continue };
        let dynamic = type_name.is_none();
        let not_compiled = constructor_is_not_compiled(
            enclosing_stmts.get(&candidate.constructor).copied(),
            &dead_stmts,
            &reachable_stmts,
            &inference,
            candidate.constructor,
        );
        if dynamic && !not_compiled {
            reachable_dynamic_constructors.insert((candidate.caller, candidate.constructor));
        }
        let platform_type_name = type_name
            .as_ref()
            .map(Name::as_str)
            .or_else(|| args.first().and_then(|&arg| string_literal(body, arg)));
        let Some(platform_type_name) = platform_type_name else { continue };
        if !platform.is_notify_description_type_name(platform_type_name) {
            continue;
        }
        let Some(environments) = notify_registration_environments(
            &inference,
            candidate,
            not_compiled,
            conditional_envs,
            budget,
        ) else {
            return;
        };

        if dynamic {
            let Some(array) = array_by_constructor.get(&candidate.constructor).copied() else {
                continue;
            };
            let mut candidate_regs = Vec::with_capacity(array.handlers.len());
            for handler in &array.handlers {
                if !budget.spend() {
                    return;
                }
                let registration = build_notify_registration(
                    db,
                    candidate,
                    handler.callback_name.as_str(),
                    handler.callback_expr,
                    handler.receiver,
                    handler.receiver_syntax,
                    not_compiled,
                    environments,
                    &inference,
                );
                notify_candidates.push(classify_registration(
                    candidate.constructor,
                    handler.slot,
                    NotifyCandidateKind::DynamicArray,
                    registration.clone(),
                    current_common_module,
                ));
                candidate_regs.push(registration);
            }
            dedup_identical_callback_slots(&mut candidate_regs);
            registrations.extend(candidate_regs);
            continue;
        }

        let Some(binding) = bindings_by_call.get(&candidate.constructor).copied() else {
            continue;
        };
        let Some(arguments) = callback_arguments_for_binding(binding) else { continue };
        let mut candidate_regs = Vec::with_capacity(2);
        for (handler_argument, receiver_argument, slot) in [
            (arguments.handler, arguments.receiver, NotifyCallbackSlot::Handler),
            (arguments.error_handler, arguments.error_receiver, NotifyCallbackSlot::ErrorHandler),
        ] {
            if !budget.spend() {
                return;
            }
            let Some(&callback_expr) = handler_argument.and_then(|index| binding.args.get(index))
            else {
                continue;
            };
            let Some(callback_name) = string_literal_id(body, callback_expr) else { continue };
            let receiver = receiver_argument.and_then(|index| binding.args.get(index)).copied();
            let receiver_syntax = receiver_syntax(body, receiver);
            let registration = build_notify_registration(
                db,
                candidate,
                callback_name,
                callback_expr,
                receiver,
                receiver_syntax,
                not_compiled,
                environments,
                &inference,
            );
            notify_candidates.push(classify_registration(
                candidate.constructor,
                slot,
                NotifyCandidateKind::Static,
                registration.clone(),
                current_common_module,
            ));
            candidate_regs.push(registration);
        }
        dedup_identical_callback_slots(&mut candidate_regs);
        registrations.extend(candidate_regs);
    }
}

fn classify_registration(
    constructor: ExprId,
    slot: NotifyCallbackSlot,
    kind: NotifyCandidateKind,
    registration: NotifyReg,
    current_common_module: Option<&Name>,
) -> NotifyCandidate {
    let receiver = match &registration.target {
        NotifyTarget::ThisObject => NotifyCandidateReceiver::ThisObject,
        NotifyTarget::Module(module)
            if current_common_module.is_some_and(|current| module.eq_ignore_case(current)) =>
        {
            NotifyCandidateReceiver::ThisObject
        }
        NotifyTarget::Module(module) => NotifyCandidateReceiver::ForeignModule(module.clone()),
        NotifyTarget::NotCompiled { .. } => NotifyCandidateReceiver::NotCompiled,
        NotifyTarget::Unsupported => NotifyCandidateReceiver::Unknown,
    };
    NotifyCandidate {
        caller: registration.caller,
        constructor,
        expression: registration.callback_expr,
        slot,
        kind,
        callback_name: registration.callback_name.clone(),
        receiver_syntax: registration.receiver_syntax,
        receiver,
        protects_local_method: match &registration.target {
            NotifyTarget::ThisObject => true,
            NotifyTarget::Module(module) => {
                current_common_module.is_some_and(|current| module.eq_ignore_case(current))
            }
            NotifyTarget::Unsupported => kind == NotifyCandidateKind::DynamicArray,
            NotifyTarget::NotCompiled { .. } => false,
        },
        site_reachable: !matches!(&registration.target, NotifyTarget::NotCompiled { .. }),
        analysis_incomplete: false,
        registration: Some(registration),
    }
}

/// The error-handler slot repeating the main handler on the same receiver names one platform
/// invocation target; summary registrations merge it, while `NotifyCandidate` retains both slots.
fn dedup_identical_callback_slots(regs: &mut Vec<NotifyReg>) {
    regs.dedup_by(|later, earlier| {
        NormName::intern(later.callback_name.as_str())
            == NormName::intern(earlier.callback_name.as_str())
            && later.receiver_syntax == earlier.receiver_syntax
            && same_normalized_target(&later.target, &earlier.target)
    });
}

/// Dedup merges only proven-identical invocation targets: `ThisObject` is the current
/// module itself, and equal folded module names are the same common module. `Unsupported`
/// and `NotCompiled` mean "unknown receiver", not "the same receiver" — different
/// unsupported expressions share the classification, so their registrations stay.
fn same_normalized_target(left: &NotifyTarget, right: &NotifyTarget) -> bool {
    match (left, right) {
        (NotifyTarget::ThisObject, NotifyTarget::ThisObject) => true,
        (NotifyTarget::Module(left), NotifyTarget::Module(right)) => {
            NormName::intern(left.as_str()) == NormName::intern(right.as_str())
        }
        _ => false,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Source provenance and inferred receiver state are both needed to create each registration."
)]
fn build_notify_registration(
    db: &dyn HirDatabase,
    candidate: NotifyConstructorCandidate,
    callback_name: &str,
    callback_expr: ExprId,
    receiver: Option<ExprId>,
    receiver_syntax: NotifyReceiverSyntax,
    not_compiled: bool,
    environments: NotifyRegistrationEnvironments,
    inference: &InferOwnerResult,
) -> NotifyReg {
    let target = if not_compiled {
        NotifyTarget::NotCompiled { callback_expr, receiver_syntax }
    } else {
        receiver.map_or(NotifyTarget::Unsupported, |receiver| {
            normalize_receiver(db, inference, receiver)
        })
    };
    let callback_name = Name::new(callback_name);
    NotifyReg {
        caller: candidate.caller,
        callback_expr,
        callback_name,
        receiver_syntax,
        target,
        environments,
        range: candidate.range,
    }
}

fn string_literal(body: &Body, expr: ExprIdx) -> Option<&str> {
    match body.expr_idx(expr) {
        Expr::Literal(Literal::String(value)) => Some(value),
        _ => None,
    }
}

fn string_literal_id(body: &Body, expr: ExprId) -> Option<&str> {
    match body.expr(expr) {
        Expr::Literal(Literal::String(value)) => Some(value),
        _ => None,
    }
}

fn receiver_syntax(body: &Body, receiver: Option<ExprId>) -> NotifyReceiverSyntax {
    match receiver.map(|expr| body.expr(expr)) {
        Some(Expr::Path(name)) => NotifyReceiverSyntax::for_bare_name(name),
        _ => NotifyReceiverSyntax::Other,
    }
}

/// One pass over the body maps every expression to the first statement that owns it,
/// exactly as [`Body::enclosing_stmt`] would answer per constructor, but without
/// rescanning all statements for every candidate.
fn enclosing_stmt_index(
    body: &Body,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashMap<ExprId, StmtId>> {
    let mut index = FxHashMap::default();
    for (stmt_id, stmt) in body.stmts_iter() {
        if !budget.spend() {
            return None;
        }
        let mut roots = Vec::new();
        collect_stmt_roots(stmt, &mut roots);
        for root in roots {
            if !index_expr_tree(body, root, stmt_id, &mut index, budget) {
                return None;
            }
        }
    }
    Some(index)
}

fn collect_stmt_roots(stmt: &Stmt, roots: &mut Vec<ExprIdx>) {
    match stmt {
        Stmt::Expr(expr) => roots.push(*expr),
        Stmt::Assign { target, value } => {
            roots.push(*target);
            roots.push(*value);
        }
        Stmt::Return { value } | Stmt::Raise { value } => {
            if let Some(value) = value {
                roots.push(*value);
            }
        }
        Stmt::Execute { expr } => roots.push(*expr),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            roots.push(*event);
            roots.push(*handler);
        }
        Stmt::If(if_stmt) => {
            roots.push(if_stmt.condition);
            roots.extend(if_stmt.elsif_branches.iter().map(|(condition, _)| *condition));
        }
        Stmt::While { condition, .. } => roots.push(*condition),
        Stmt::For { from, to, .. } => {
            roots.push(*from);
            roots.push(*to);
        }
        Stmt::ForEach { collection, .. } => roots.push(*collection),
        Stmt::PreprocIf(_)
        | Stmt::Try { .. }
        | Stmt::VarDecl { .. }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Goto(_)
        | Stmt::Label(_) => {}
    }
}

fn index_expr_tree(
    body: &Body,
    expr: ExprIdx,
    stmt: StmtId,
    index: &mut FxHashMap<ExprId, StmtId>,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    let expr_id = ExprId::from_idx(expr);
    if index.contains_key(&expr_id) {
        return true;
    }
    if !budget.spend() {
        return false;
    }
    index.insert(expr_id, stmt);
    let mut children = Vec::new();
    for_each_expr_child(body, expr, &mut |child| children.push(child));
    for child in children {
        if !index_expr_tree(body, child, stmt, index, budget) {
            return false;
        }
    }
    true
}

fn constructor_is_not_compiled(
    enclosing_stmt: Option<StmtId>,
    dead_stmts: &FxHashSet<StmtId>,
    reachable_stmts: &FxHashSet<StmtId>,
    inference: &InferOwnerResult,
    constructor: ExprId,
) -> bool {
    enclosing_stmt
        .is_some_and(|stmt| dead_stmts.contains(&stmt) || !reachable_stmts.contains(&stmt))
        || inference
            .notify_constructor_environment(constructor)
            .is_some_and(|environment| !environment.may_be_available())
}

fn notify_registration_environments(
    inference: &InferOwnerResult,
    candidate: NotifyConstructorCandidate,
    not_compiled: bool,
    conditional_envs: &mut ConditionalEnvIndex<'_>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<NotifyRegistrationEnvironments> {
    if not_compiled {
        return Some(NotifyRegistrationEnvironments::default());
    }
    let (constructor_definite, constructor_possible) = inference
        .notify_constructor_registration_envs(candidate.constructor)
        .unwrap_or((EnvFlags::EMPTY, EnvFlags::ALL));
    let (site_definite, site_undecided) = conditional_envs.tracking(candidate.range, budget)?;
    Some(NotifyRegistrationEnvironments {
        definite: constructor_definite & site_definite,
        possible: constructor_possible & (site_definite | site_undecided),
    })
}

fn normalize_receiver(
    db: &dyn HirDatabase,
    inference: &InferOwnerResult,
    receiver: ExprId,
) -> NotifyTarget {
    match inference.notify_receiver_resolution(receiver) {
        Some((BareNameOrigin::CommonModule, ty)) => match db.lookup_type(ty) {
            TypeKind::CommonModule(module) => NotifyTarget::Module(Name::new(&module.name)),
            _ => NotifyTarget::Unsupported,
        },
        Some((BareNameOrigin::SelfName, _)) => NotifyTarget::ThisObject,
        _ => NotifyTarget::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        receiver_syntax: NotifyReceiverSyntax,
        receiver: NotifyCandidateReceiver,
        kind: NotifyCandidateKind,
        analysis_incomplete: bool,
    ) -> NotifyCandidate {
        NotifyCandidate {
            caller: CallerId::ModuleCode,
            constructor: ExprId::from_raw(0.into()),
            expression: ExprId::from_raw(0.into()),
            slot: NotifyCallbackSlot::Handler,
            kind,
            callback_name: Name::new("H"),
            receiver_syntax,
            receiver,
            protects_local_method: false,
            site_reachable: true,
            analysis_incomplete,
            registration: None,
        }
    }

    #[test]
    fn signature_verdict_keeps_syntactically_local_receivers() {
        for receiver_syntax in [NotifyReceiverSyntax::SelfName, NotifyReceiverSyntax::Other] {
            for receiver in [
                NotifyCandidateReceiver::ThisObject,
                NotifyCandidateReceiver::ForeignModule(Name::new("Модуль")),
                NotifyCandidateReceiver::Unknown,
            ] {
                assert_eq!(
                    candidate(receiver_syntax, receiver, NotifyCandidateKind::Static, false)
                        .signature_protection(),
                    NotifySignatureProtection::Protected,
                    "{receiver_syntax:?}"
                );
            }
        }
    }

    #[test]
    fn signature_verdict_protects_a_resolved_self_receiver_with_a_bare_name() {
        assert_eq!(
            candidate(
                NotifyReceiverSyntax::BareName,
                NotifyCandidateReceiver::ThisObject,
                NotifyCandidateKind::Static,
                false
            )
            .signature_protection(),
            NotifySignatureProtection::Protected
        );
    }

    #[test]
    fn signature_verdict_rejects_a_foreign_bare_receiver() {
        assert_eq!(
            candidate(
                NotifyReceiverSyntax::BareName,
                NotifyCandidateReceiver::ForeignModule(Name::new("Модуль")),
                NotifyCandidateKind::Static,
                false
            )
            .signature_protection(),
            NotifySignatureProtection::NotProtected
        );
    }

    #[test]
    fn signature_verdict_reports_incomplete_analysis_as_indeterminate() {
        assert_eq!(
            candidate(
                NotifyReceiverSyntax::BareName,
                NotifyCandidateReceiver::ForeignModule(Name::new("Модуль")),
                NotifyCandidateKind::Static,
                true
            )
            .signature_protection(),
            NotifySignatureProtection::Indeterminate
        );
    }

    #[test]
    fn work_budget_is_sticky_and_checks_cancellation_periodically() {
        let mut cancellations = 0;
        {
            let mut cancel = || cancellations += 1;
            let mut budget = NotifyWorkBudget::with_limit(&mut cancel, 256);
            for _ in 0..256 {
                assert!(budget.spend());
            }
            assert!(!budget.spend());
            assert!(budget.exhausted());
            assert!(!budget.spend(), "exhaustion must stay sticky");
        }
        assert_eq!(cancellations, 2, "one cancellation per 128 work units");
    }

    #[test]
    fn enclosing_stmt_index_matches_the_per_expression_body_lookup() {
        let source = r#"
Процедура Тест(Флаг)
    А = Новый Массив;
    А.Добавить("H");
    Если Флаг Тогда
        Б = А.Добавить("X");
    КонецЕсли;
    Пока Флаг Цикл
        Прервать;
    КонецЦикла;
КонецПроцедуры
"#;
        let parse = parser::parse(source);
        let bodies = hir_def::ModuleBodies::from_parse(&parse);
        let body = bodies.iter_bodies().next().expect("fixture declares a method").1;
        let mut cancel = || {};
        let mut budget = NotifyWorkBudget::new(&mut cancel);
        let index = enclosing_stmt_index(body, &mut budget).expect("the fixture fits the budget");
        for (expr_id, _) in body.exprs_iter() {
            assert_eq!(
                index.get(&expr_id).copied(),
                body.enclosing_stmt(expr_id),
                "expression {expr_id:?}"
            );
        }
    }

    #[test]
    fn budget_exhaustion_marker_is_conservative() {
        let marker = budget_incomplete_candidate(CallerId::ModuleCode, ExprId::from_raw(0.into()));
        assert!(marker.analysis_incomplete);
        assert_eq!(marker.signature_protection(), NotifySignatureProtection::Indeterminate);
        assert!(marker.registration.is_none());
    }
}
