use std::sync::Arc;

use base_db::FileIdInput;
use bsl_metadata::MdObject;
use bsl_platform::PlatformDataInner;
use bsl_types::kind::TypeKind;
use hir_def::body::Body;
use hir_def::call_graph::{
    CallerId, DynamicNotifySignatureAnalysisGap, ModuleCallSummary, NotifyConstructorCandidate,
    NotifyReceiverSyntax, NotifyReg, NotifyTarget,
};
use hir_def::conditional_tree::{ConditionalIdx, ConditionalTree};
use hir_def::effective_module::EffectiveModuleId;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt};
use hir_def::weaving::WeavingModuleId;
use hir_def::{
    call_graph::NotifyRegistrationEnvironments, execution_env::EnvFlags, DefWithBodyId, ExprId,
    IdConversion, MethodKey, ModuleBodies, ModuleId, Name, StmtId,
};
use intern::NormName;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{TextRange, TextSize};
use vfs::FileId;

use crate::body_flow::BodyFlow;
use crate::db::HirDatabase;
use crate::infer::{
    infer_effective_owner_for_notify, infer_owner_for_notify, infer_weaving_owner_for_notify,
    BareNameOrigin, CallArgBinding, InferOwnerResult,
};
use crate::notify_array::{
    callback_arguments_for_binding_with_budget, CallbackArguments, NotifyArrayRegistration,
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
    /// A resolved local/global/object/form value whose runtime receiver is unknown:
    /// it may hold `ЭтотОбъект`, but is not itself a proven callback reference.
    LocalBinding,
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
        if !self.site_reachable {
            return NotifySignatureProtection::NotProtected;
        }
        if self.analysis_incomplete {
            return NotifySignatureProtection::Indeterminate;
        }
        let syntax_may_be_local = self.receiver_syntax.may_name_local_callback();
        let resolved_this_object = matches!(self.receiver, NotifyCandidateReceiver::ThisObject);
        // A dynamic array's contents may name a local handler, and a resolved bare
        // receiver may still be `ЭтотОбъект`. Neither uncertainty may become a proven-
        // foreign verdict; an unresolved name is not promoted to an object value.
        let receiver_may_be_local = matches!(
            self.receiver,
            NotifyCandidateReceiver::Unknown | NotifyCandidateReceiver::LocalBinding
        ) && (self.kind == NotifyCandidateKind::DynamicArray
            || self.receiver == NotifyCandidateReceiver::LocalBinding);
        if resolved_this_object || syntax_may_be_local || receiver_may_be_local {
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
const NOTIFY_TARGET_WORK_PER_BODY_ITEM: usize = 128;
const NOTIFY_TARGET_BUDGET_SATURATION_SIZE: usize = 3_072;
const NOTIFY_TARGET_MAX_WORK_BUDGET: usize = NOTIFY_TARGET_WORK_BUDGET
    + NOTIFY_TARGET_WORK_PER_BODY_ITEM * NOTIFY_TARGET_BUDGET_SATURATION_SIZE;
const NOTIFY_TARGET_CANCEL_INTERVAL: usize = 128;

pub(crate) struct NotifyWorkBudget<'a> {
    remaining: usize,
    since_cancel: usize,
    cancel: &'a mut dyn FnMut(),
    exhausted: bool,
}

impl<'a> NotifyWorkBudget<'a> {
    fn for_module(cancel: &'a mut dyn FnMut(), bodies: &hir_def::ModuleBodies) -> Self {
        let mut body_items = 0usize;
        let mut since_cancel = 0usize;
        for (_, body) in bodies.iter_bodies() {
            body_items = body_items
                .saturating_add(body_item_count(body))
                .min(NOTIFY_TARGET_BUDGET_SATURATION_SIZE);
            since_cancel += 1;
            if since_cancel == NOTIFY_TARGET_CANCEL_INTERVAL {
                since_cancel = 0;
                cancel();
            }
            if body_items == NOTIFY_TARGET_BUDGET_SATURATION_SIZE {
                break;
            }
        }
        if body_items < NOTIFY_TARGET_BUDGET_SATURATION_SIZE {
            if let Some(body) = bodies.module_code() {
                body_items = body_items
                    .saturating_add(body_item_count(body))
                    .min(NOTIFY_TARGET_BUDGET_SATURATION_SIZE);
                since_cancel += 1;
            }
        }
        if since_cancel != 0 {
            cancel();
        }
        Self::with_limit(cancel, notify_target_work_budget(body_items))
    }

    pub(crate) fn with_limit(cancel: &'a mut dyn FnMut(), limit: usize) -> Self {
        Self { remaining: limit, since_cancel: 0, cancel, exhausted: false }
    }

    pub(crate) fn spend(&mut self) -> bool {
        self.spend_by(1)
    }

    pub(crate) fn spend_by(&mut self, work: usize) -> bool {
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

    pub(crate) fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Records that the pass cannot produce a complete result although it has not
    /// spent its remaining work: an input it must prove against is unavailable.
    /// Callers read the flag as the projection's completeness, so a stopped input
    /// must set it instead of leaving an empty answer that looks complete.
    pub(crate) fn mark_incomplete(&mut self) {
        self.remaining = 0;
        self.exhausted = true;
    }
}

/// Every body contributes at least one item, even when it has no expressions, statements,
/// or bindings; the caller saturates the aggregate before it becomes an unbounded sizing pass.
fn body_item_count(body: &Body) -> usize {
    body.expr_count().saturating_add(body.stmt_count()).saturating_add(body.binding_count()).max(1)
}

/// Give larger modules a proportionally larger allowance while keeping the total
/// semantic projection bounded, including for adversarially large bodies.
fn notify_target_work_budget(body_items: usize) -> usize {
    NOTIFY_TARGET_WORK_BUDGET
        .saturating_add(
            body_items
                .min(NOTIFY_TARGET_BUDGET_SATURATION_SIZE)
                .saturating_mul(NOTIFY_TARGET_WORK_PER_BODY_ITEM),
        )
        .min(NOTIFY_TARGET_MAX_WORK_BUDGET)
}

/// Budget cost of one comparison sort over `len` items: one unit per element per
/// merge level (ceil(log2(len))), saturating so absurd sizes still fit the type.
fn sort_work(len: usize) -> usize {
    if len <= 1 {
        return len;
    }
    let levels = usize::BITS - (len - 1).leading_zeros();
    len.saturating_mul(levels as usize)
}

/// One indexed pass over the module's conditional tree. Conditional ranges are sorted
/// by start and shadowed in a max-end segment tree, so a candidate range finds its
/// innermost enclosing nodes without rescanning every conditional. Each index entry and
/// visited tree node spends from the shared budget and reaches the cancellation check;
/// the range sort charges its log-linear cost before running, so a huge tree exhausts
/// the budget instead of stretching one uncancelled stretch.
struct ConditionalEnvIndex<'a> {
    tree: &'a ConditionalTree,
    nodes: Vec<(TextRange, ConditionalIdx)>,
    max_end: Vec<TextSize>,
    tree_size: usize,
    cache: FxHashMap<TextRange, (EnvFlags, EnvFlags)>,
}

impl<'a> ConditionalEnvIndex<'a> {
    fn new(tree: &'a ConditionalTree, budget: &mut NotifyWorkBudget<'_>) -> Option<Self> {
        let mut nodes = Vec::new();
        for (idx, data) in tree.conditionals() {
            if !budget.spend() {
                return None;
            }
            nodes.push((data.range, idx));
        }
        // The sort cannot interleave budget checks, so its log-linear cost is charged
        // up front: the charge itself reaches a cancellation checkpoint, and a tree too
        // large for the remaining budget exhausts here instead of inside the sort.
        if !budget.spend_by(sort_work(nodes.len())) {
            return None;
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
        let env = {
            let mut spend = || budget.spend();
            hir_def::execution_env::conditional_env_for_node_with_work(
                self.tree,
                best.map(|(idx, _)| idx),
                &mut spend,
            )?
        };
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
        if !budget.spend() {
            return false;
        }
        if right <= query_left || query_right <= left || self.max_end[node] < query_end {
            return true;
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

/// One module's inputs to the notification projection: the bodies to walk, the
/// conditional tree their ranges index, the inference that resolves each caller's
/// receivers, and the file identity used for metadata and CFG lookups. Ordinary,
/// effective (`&ИзменениеИКонтроль`) and weaving (`&Вместо`/`&Перед`/`&После`) modules
/// differ only in how this view is assembled; the projection below is one algorithm.
type NotifyInference<'a> = Box<dyn Fn(DefWithBodyId, &Body) -> InferOwnerResult + 'a>;
type NotifyFlow<'a> = Box<dyn Fn(&dyn HirDatabase, DefWithBodyId, &Body) -> BodyFlow + 'a>;

struct NotifyModuleProjection<'a> {
    file_id: FileId,
    bodies: Arc<ModuleBodies>,
    conditional_tree: Arc<ConditionalTree>,
    current_common_module: Option<Name>,
    infer: NotifyInference<'a>,
    flow: NotifyFlow<'a>,
}

impl<'db> NotifyModuleProjection<'db> {
    fn for_file(db: &'db dyn HirDatabase, file_id: FileId) -> Self {
        let module_id = ModuleId::new(file_id);
        Self {
            file_id,
            bodies: db.module_bodies_ref(module_id).clone(),
            conditional_tree: db.conditional_tree_ref(file_id).clone(),
            current_common_module: common_module_name(db, module_id),
            infer: Box::new(move |owner, body| infer_owner_for_notify(db, file_id, owner, body)),
            flow: Box::new(move |db, owner, body| ordinary_flow(db, file_id, owner, body)),
        }
    }

    fn for_effective(db: &'db dyn HirDatabase, eid: EffectiveModuleId<'db>) -> Self {
        let base_file = eid.base_file(db);
        let bodies = hir_def::effective_module::module_bodies_effective(db, eid);
        let parse = hir_def::effective_module::parse_effective(db, eid);
        Self {
            file_id: base_file,
            bodies,
            conditional_tree: Arc::new(hir_def::conditional_tree::lower_conditionals(
                &parse.syntax_node(),
            )),
            current_common_module: common_module_name(db, ModuleId::new(base_file)),
            infer: Box::new(move |owner, body| {
                infer_effective_owner_for_notify(db, eid, owner, body)
            }),
            // The effective bodies are a splice, not the memoised bodies under either
            // file, so their flow is their own; params seed a method exactly as in
            // `InferenceContext::new_effective`.
            flow: Box::new(|_db, owner, body| {
                BodyFlow::detached(
                    Arc::new(body.clone()),
                    matches!(owner, DefWithBodyId::Method(_)),
                )
            }),
        }
    }

    fn for_weaving(db: &'db dyn HirDatabase, wid: WeavingModuleId<'db>) -> Self {
        let file_id = wid.ext_file(db);
        let module_id = ModuleId::new(file_id);
        Self {
            file_id,
            bodies: db.module_bodies_ref(module_id).clone(),
            conditional_tree: db.conditional_tree_ref(file_id).clone(),
            current_common_module: common_module_name(db, module_id),
            infer: Box::new(move |owner, body| {
                infer_weaving_owner_for_notify(db, wid, owner, body)
            }),
            // Weaving has no splice: the extension keeps its own memoised bodies.
            flow: Box::new(move |db, owner, body| ordinary_flow(db, file_id, owner, body)),
        }
    }
}

/// The flow of a file's own memoised body. Both ordinary and weaving routes use it; an
/// effective splice supplies its own flow because its text is neither file's memo.
fn ordinary_flow(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    body: &Body,
) -> BodyFlow {
    match owner {
        DefWithBodyId::Method(local_id) => {
            let method = hir_def::MethodId { module: ModuleId::new(file_id), local_id };
            let input = hir_def::MethodIdInput::new(db, method);
            BodyFlow::for_method(db, input, db.method_body_ref(input))
        }
        DefWithBodyId::ModuleCode => {
            let bodies = db.module_bodies_ref(ModuleId::new(file_id));
            bodies.module_code_arc().map_or_else(
                || BodyFlow::detached(Arc::new(body.clone()), false),
                |body| BodyFlow::for_module_code(db, file_id, body),
            )
        }
    }
}

fn common_module_name(db: &dyn HirDatabase, module_id: ModuleId) -> Option<Name> {
    db.module_metadata(module_id).common_module.as_ref().map(|module| Name::new(module.name()))
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
    let raw = hir_def::queries::module_call_summary_query(db, file_id_input);
    if raw.notify_candidates.is_empty() {
        return empty_notify_facts(&raw);
    }
    let projection = NotifyModuleProjection::for_file(db, file_id_input.file_id(db));
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::for_module(&mut cancel, &projection.bodies);
    semantic_notify_facts_with_budget(db, &projection, &raw, &mut budget)
}

/// The effective `&ИзменениеИКонтроль` module's notification projection: the merged
/// source supplies constructors, ranges and callers, while inference runs through the
/// effective context so a receiver like `ЭтотОбъект` resolves against the base module's
/// metadata. Ranges stay in effective-text coordinates; the diagnostics merge remaps
/// `#Вставка` findings to the extension source.
#[salsa::tracked(
    lru = 256,
    heap_size = semantic_notify_facts_heap,
    returns(clone)
)]
pub fn semantic_notify_facts_effective_query<'db>(
    db: &'db dyn HirDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<SemanticNotifyFacts> {
    let raw = hir_def::effective_module::module_call_summary_effective(db, eid);
    if raw.notify_candidates.is_empty() {
        return empty_notify_facts(&raw);
    }
    let projection = NotifyModuleProjection::for_effective(db, eid);
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::for_module(&mut cancel, &projection.bodies);
    semantic_notify_facts_with_budget(db, &projection, &raw, &mut budget)
}

/// The weaving extension module's notification projection: its own bodies and ranges,
/// with inference routed through the base module as a same-module sibling fallback.
#[salsa::tracked(
    lru = 256,
    heap_size = semantic_notify_facts_heap,
    returns(clone)
)]
pub fn semantic_notify_facts_weaving_query<'db>(
    db: &'db dyn HirDatabase,
    wid: WeavingModuleId<'db>,
) -> Arc<SemanticNotifyFacts> {
    let file_id = wid.ext_file(db);
    let raw = hir_def::queries::module_call_summary_query(db, FileIdInput::new(db, file_id));
    if raw.notify_candidates.is_empty() {
        return empty_notify_facts(&raw);
    }
    let projection = NotifyModuleProjection::for_weaving(db, wid);
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::for_module(&mut cancel, &projection.bodies);
    semantic_notify_facts_with_budget(db, &projection, &raw, &mut budget)
}

pub fn semantic_notify_candidates_effective_query<'db>(
    db: &'db dyn HirDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<Vec<NotifyCandidate>> {
    semantic_notify_facts_effective_query(db, eid).notify_candidates.clone()
}

pub fn semantic_notify_candidates_weaving_query<'db>(
    db: &'db dyn HirDatabase,
    wid: WeavingModuleId<'db>,
) -> Arc<Vec<NotifyCandidate>> {
    semantic_notify_facts_weaving_query(db, wid).notify_candidates.clone()
}

/// The semantic summary of the effective module: registrations and the projection's
/// completeness verdict, exactly as the plain per-file query answers for its own module.
pub fn semantic_module_call_summary_effective_query<'db>(
    db: &'db dyn HirDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<ModuleCallSummary> {
    semantic_notify_facts_effective_query(db, eid).summary.clone()
}

/// The semantic summary of the weaving extension module, with its own ranges.
pub fn semantic_module_call_summary_weaving_query<'db>(
    db: &'db dyn HirDatabase,
    wid: WeavingModuleId<'db>,
) -> Arc<ModuleCallSummary> {
    semantic_notify_facts_weaving_query(db, wid).summary.clone()
}

/// Test hook: the same projection with an explicit work limit, so callers can
/// observe the conservative exhaustion result without a pathological fixture.
#[doc(hidden)]
pub fn semantic_notify_facts_with_work_limit<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
    work_limit: usize,
) -> Arc<SemanticNotifyFacts> {
    let raw = hir_def::queries::module_call_summary_query(db, file_id_input);
    if raw.notify_candidates.is_empty() {
        return empty_notify_facts(&raw);
    }
    let projection = NotifyModuleProjection::for_file(db, file_id_input.file_id(db));
    let mut cancel = || db.unwind_if_revision_cancelled();
    let mut budget = NotifyWorkBudget::with_limit(&mut cancel, work_limit);
    semantic_notify_facts_with_budget(db, &projection, &raw, &mut budget)
}

fn empty_notify_facts(raw: &ModuleCallSummary) -> Arc<SemanticNotifyFacts> {
    let mut summary = raw.clone();
    summary.name_literal_ref_sites.clear();
    // Without a constructor identity, syntax-only callback sites and gaps cannot be
    // associated; unrelated module data must not create notify uncertainty.
    Arc::new(SemanticNotifyFacts {
        summary: Arc::new(summary),
        notify_candidates: Arc::new(Vec::new()),
    })
}

fn semantic_notify_facts_with_budget<'db>(
    db: &'db dyn HirDatabase,
    projection: &NotifyModuleProjection<'db>,
    raw: &ModuleCallSummary,
    budget: &mut NotifyWorkBudget<'_>,
) -> Arc<SemanticNotifyFacts> {
    let mut summary = raw.clone();
    let mut candidates = Vec::new();
    let mut notify_candidates = Vec::new();
    let mut reachable_dynamic_constructors: FxHashSet<(CallerId, ExprId)> = FxHashSet::default();

    let bodies = &*projection.bodies;
    let platform = PlatformDataInner::instance();
    let use_syntax_fallback = !bsl_platform::has_callback_description_constructor_facts(platform);
    // A served snapshot that lacks constructor facts still describes the callback
    // constructor through the bundled compatibility slots. A missing snapshot
    // (`platform_help = none`) has no such source and keeps its syntax-only route.
    let partial_callback_facts = use_syntax_fallback && platform.help_origin().is_some();
    let mut bodies_by_caller: FxHashMap<CallerId, &Body> = FxHashMap::default();
    let mut candidates_by_caller: FxHashMap<CallerId, Vec<usize>> = FxHashMap::default();
    let mut owner_order = Vec::new();
    let mut seen_owners = FxHashSet::default();
    let mut filtering_exhausted = None;

    for raw_candidate in summary.notify_candidates.iter().copied() {
        let body = match raw_candidate.caller {
            CallerId::Method(local_id) => bodies.lower_result(local_id).map(|lower| lower.body()),
            CallerId::ModuleCode => bodies.module_code(),
        };
        let Some(body) = body else { continue };
        let Expr::New { type_name, args } = body.expr(raw_candidate.constructor) else { continue };
        let platform_type_name = type_name
            .as_ref()
            .map(Name::as_str)
            .or_else(|| args.first().and_then(|&arg| string_literal(body, arg)));
        if !candidate_supported_without_construction(
            platform,
            use_syntax_fallback,
            platform_type_name,
        ) {
            continue;
        }
        if !budget.spend() {
            filtering_exhausted = Some(raw_candidate.caller);
            break;
        }
        let index = candidates.len();
        candidates.push(raw_candidate);
        bodies_by_caller.insert(raw_candidate.caller, body);
        candidates_by_caller.entry(raw_candidate.caller).or_default().push(index);
        if seen_owners.insert(raw_candidate.caller) {
            owner_order.push(raw_candidate.caller);
        }
    }
    if let Some(caller) = filtering_exhausted {
        return finish_incomplete_facts(summary, notify_candidates, caller);
    }
    if candidates.is_empty() {
        summary.notify_candidates.clear();
        summary.name_literal_ref_sites.clear();
        return Arc::new(SemanticNotifyFacts {
            summary: Arc::new(summary),
            notify_candidates: Arc::new(Vec::new()),
        });
    }

    let current_common_module = projection.current_common_module.clone();
    let Some(mut conditional_envs) = ConditionalEnvIndex::new(&projection.conditional_tree, budget)
    else {
        return finish_incomplete_facts(summary, notify_candidates, CallerId::ModuleCode);
    };

    for caller in owner_order {
        let Some(indices) = candidates_by_caller.remove(&caller) else { continue };
        let (owner, body) = match caller {
            CallerId::Method(local_id) => {
                let Some(lower) = bodies.lower_result(local_id) else {
                    return finish_incomplete_facts(summary, notify_candidates, caller);
                };
                (DefWithBodyId::Method(local_id), lower.body())
            }
            CallerId::ModuleCode => {
                let Some(body) = bodies.module_code() else {
                    return finish_incomplete_facts(summary, notify_candidates, caller);
                };
                (DefWithBodyId::ModuleCode, body)
            }
        };
        normalize_owner_candidates(
            db,
            projection,
            owner,
            body,
            &indices,
            &candidates,
            platform,
            partial_callback_facts,
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
            &reachable_dynamic_constructors,
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

/// The name-only verdict the candidate filter reads before any budget spend: a
/// constructor whose type name cannot be related to callbacks is dropped without
/// spending, so exhausting the filter loop always leaves a genuine callback pending
/// and never marks a callback-free module incomplete. Under `use_syntax_fallback`
/// the bundled callback-description names pass without any served platform facts
/// (platform_help = none).
fn candidate_supported_without_construction(
    platform: &PlatformDataInner,
    use_syntax_fallback: bool,
    platform_type_name: Option<&str>,
) -> bool {
    platform_type_name.is_some_and(|name| {
        platform.is_notify_description_type_name(name)
            || (use_syntax_fallback && bsl_platform::is_callback_description_type_name(name))
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
    summary.notify_projection_incomplete = true;
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
    reachable_constructors: &FxHashSet<(CallerId, ExprId)>,
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
            if !receiver_syntax.may_name_local_callback() {
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
                site_reachable: reachable_constructors
                    .contains(&(constructor.caller, constructor.constructor)),
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
    projection: &NotifyModuleProjection<'_>,
    owner: DefWithBodyId,
    body: &Body,
    candidate_indices: &[usize],
    candidates: &[NotifyConstructorCandidate],
    platform: &PlatformDataInner,
    partial_callback_facts: bool,
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
    let inference = (projection.infer)(owner, body);
    if inference.reaching_defs_incomplete() {
        budget.mark_incomplete();
        return;
    }
    let mut dead_stmts: FxHashSet<StmtId> = FxHashSet::default();
    for &stmt in inference.dead_stmts() {
        if !budget.spend() {
            return;
        }
        dead_stmts.insert(stmt);
    }
    let flow = (projection.flow)(db, owner, body);
    let cfg = flow.cfg(db);
    let Some(reachable_stmts) = reachable_statements_with_budget(&cfg, budget) else {
        return;
    };
    if !budget.spend() {
        return;
    }
    let owner_possible_env = owner_possible_in_any_build(db, projection.file_id, owner);
    let mut has_dynamic_array_candidate = false;
    for &index in candidate_indices {
        if !budget.spend() {
            return;
        }
        let candidate = candidates[index];
        if let Expr::New { type_name: None, args } = body.expr(candidate.constructor) {
            if args
                .first()
                .and_then(|&arg| string_literal(body, arg))
                .is_some_and(|name| platform.is_notify_description_type_name(name))
            {
                has_dynamic_array_candidate = true;
                break;
            }
        }
    }
    let array_registrations = if has_dynamic_array_candidate {
        // Only a dynamic array needs the possible writes of its constructor point.
        // The executable solve is both expensive and unmemoised, so a static-only
        // owner never pays for it; when it runs, its index and visits spend from
        // this projection's budget and reach its cancellation callback.
        let reaching_defs = {
            let mut spend = |work: usize| budget.spend_by(work);
            flow.executable_reaching_defs_with_work(db, &mut spend)
        };
        crate::notify_array::collect_notify_array_registrations_with_budget(
            db,
            body,
            &reachable_stmts,
            reaching_defs.as_deref(),
            &inference,
            platform,
            budget,
        )
    } else {
        Vec::new()
    };
    if budget.exhausted() {
        return;
    }

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
        let platform_type_name = type_name
            .as_ref()
            .map(Name::as_str)
            .or_else(|| args.first().and_then(|&arg| string_literal(body, arg)));
        let Some(platform_type_name) = platform_type_name else { continue };
        let type_env = platform
            .get_type(platform_type_name)
            .map(|ty| EnvFlags::from_platform_context(ty.context.as_ref()))
            .unwrap_or(EnvFlags::ALL);
        let Some(site_reachable) = candidate_site_reachable_in_any_build(
            type_env,
            owner_possible_env,
            enclosing_stmts.get(&candidate.constructor).copied(),
            &reachable_stmts,
            candidate.range,
            conditional_envs,
            budget,
        ) else {
            return;
        };
        if site_reachable {
            reachable_dynamic_constructors.insert((candidate.caller, candidate.constructor));
        }
        if !platform.is_notify_description_type_name(platform_type_name)
            && !(partial_callback_facts
                && bsl_platform::is_callback_description_type_name(platform_type_name))
        {
            continue;
        }
        let not_compiled = constructor_is_not_compiled(
            enclosing_stmts.get(&candidate.constructor).copied(),
            &dead_stmts,
            &reachable_stmts,
            &inference,
            candidate.constructor,
        );
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
                    site_reachable,
                ));
                candidate_regs.push(registration);
            }
            dedup_identical_callback_slots(&mut candidate_regs);
            registrations.extend(candidate_regs);
            continue;
        }

        let binding = bindings_by_call.get(&candidate.constructor).copied();
        let arguments = match binding {
            Some(binding) => callback_arguments_for_binding_with_budget(binding, budget)
                .or_else(|| partial_callback_facts.then(bundled_callback_arguments)),
            None if partial_callback_facts => Some(bundled_callback_arguments()),
            None => None,
        };
        let Some(arguments) = arguments else {
            if budget.exhausted() {
                return;
            }
            continue;
        };
        let argument_expr = |index: usize| -> Option<ExprId> {
            match binding {
                Some(binding) => binding.args.get(index).copied(),
                None => args.get(index).map(|&idx| ExprId::from_idx(idx)),
            }
        };
        let mut candidate_regs = Vec::with_capacity(2);
        for (handler_argument, receiver_argument, slot) in [
            (arguments.handler, arguments.receiver, NotifyCallbackSlot::Handler),
            (arguments.error_handler, arguments.error_receiver, NotifyCallbackSlot::ErrorHandler),
        ] {
            if !budget.spend() {
                return;
            }
            let Some(callback_expr) = handler_argument.and_then(argument_expr) else {
                continue;
            };
            let Some(callback_name) = string_literal_id(body, callback_expr) else { continue };
            let receiver = receiver_argument.and_then(argument_expr);
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
                site_reachable,
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
    site_reachable: bool,
) -> NotifyCandidate {
    let receiver = match &registration.target {
        NotifyTarget::ThisObject => NotifyCandidateReceiver::ThisObject,
        NotifyTarget::Module(module)
            if current_common_module.is_some_and(|current| module.eq_ignore_case(current)) =>
        {
            NotifyCandidateReceiver::ThisObject
        }
        NotifyTarget::Module(module) => NotifyCandidateReceiver::ForeignModule(module.clone()),
        NotifyTarget::LocalBinding => NotifyCandidateReceiver::LocalBinding,
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
            // An uncertain receiver may be the current object. Preserve the local
            // method conservatively without creating a NotifyRef edge.
            NotifyTarget::LocalBinding => true,
            NotifyTarget::Unsupported => kind == NotifyCandidateKind::DynamicArray,
            NotifyTarget::NotCompiled { .. } => false,
        },
        site_reachable,
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
        match receiver {
            Some(receiver) => normalize_receiver(db, inference, receiver).unwrap_or_else(|| {
                unresolved_receiver_target(db, inference, receiver, receiver_syntax)
            }),
            // An omitted receiver argument names no value at all, so it cannot be
            // promoted to the uncertainty of an unresolved expression.
            None => NotifyTarget::Unsupported,
        }
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

/// The bundled compatibility projection of callback argument positions. A served
/// snapshot without constructor facts cannot project them from its own schema; the
/// bundled slots describe the same constructor, and only the receiver's runtime
/// identity still needs inference.
fn bundled_callback_arguments() -> CallbackArguments {
    let slots = bsl_platform::bundled_callback_description_slots();
    CallbackArguments {
        handler: Some(slots.handler),
        receiver: Some(slots.receiver),
        error_handler: Some(slots.error_handler),
        error_receiver: Some(slots.error_receiver),
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
        if !collect_stmt_roots(stmt, &mut roots, budget) {
            return None;
        }
        for root in roots {
            if !index_expr_tree(body, root, stmt_id, &mut index, budget) {
                return None;
            }
        }
    }
    Some(index)
}

fn collect_stmt_roots(
    stmt: &Stmt,
    roots: &mut Vec<ExprIdx>,
    budget: &mut NotifyWorkBudget<'_>,
) -> bool {
    let push = |root, roots: &mut Vec<ExprIdx>, budget: &mut NotifyWorkBudget<'_>| {
        if !budget.spend() {
            return false;
        }
        roots.push(root);
        true
    };
    match stmt {
        Stmt::Expr(expr) => push(*expr, roots, budget),
        Stmt::Assign { target, value } => {
            push(*target, roots, budget) && push(*value, roots, budget)
        }
        Stmt::Return { value } | Stmt::Raise { value } => {
            value.is_none_or(|value| push(value, roots, budget))
        }
        Stmt::Execute { expr } => push(*expr, roots, budget),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            push(*event, roots, budget) && push(*handler, roots, budget)
        }
        Stmt::If(if_stmt) => {
            if !push(if_stmt.condition, roots, budget) {
                return false;
            }
            for (condition, _) in if_stmt.elsif_branches.iter() {
                if !push(*condition, roots, budget) {
                    return false;
                }
            }
            true
        }
        Stmt::While { condition, .. } => push(*condition, roots, budget),
        Stmt::For { from, to, .. } => push(*from, roots, budget) && push(*to, roots, budget),
        Stmt::ForEach { collection, .. } => push(*collection, roots, budget),
        Stmt::PreprocIf(_)
        | Stmt::Try { .. }
        | Stmt::VarDecl { .. }
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Goto(_)
        | Stmt::Label(_) => true,
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
    let mut visit_child =
        |child| budget.spend() && index_expr_tree(body, child, stmt, index, budget);
    match body.expr_idx(expr) {
        Expr::Missing | Expr::Literal(_) | Expr::Path(_) => true,
        Expr::BinaryOp { lhs, rhs, .. } => visit_child(*lhs) && visit_child(*rhs),
        Expr::UnaryOp { expr, .. } | Expr::Await { expr } => visit_child(*expr),
        Expr::Ternary { condition, then_expr, else_expr } => {
            visit_child(*condition) && visit_child(*then_expr) && visit_child(*else_expr)
        }
        Expr::Call { callee, args } => {
            if !visit_child(*callee) {
                return false;
            }
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
        Expr::MethodCall { receiver, args, .. } => {
            if !visit_child(*receiver) {
                return false;
            }
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
        Expr::Index { base, index } => visit_child(*base) && visit_child(*index),
        Expr::Field { base, .. } => visit_child(*base),
        Expr::New { args, .. } | Expr::Array(args) => {
            for &arg in args.iter() {
                if !visit_child(arg) {
                    return false;
                }
            }
            true
        }
    }
}

pub(crate) fn reachable_statements_with_budget(
    cfg: &cfg::ControlFlowGraph,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<FxHashSet<StmtId>> {
    let Some(start) = cfg.start() else { return Some(FxHashSet::default()) };
    let mut reached_vertices = FxHashSet::default();
    let mut reached_statements = FxHashSet::default();
    let mut stack = vec![start];
    while let Some(vertex) = stack.pop() {
        if !budget.spend() {
            return None;
        }
        if !reached_vertices.insert(vertex) {
            continue;
        }
        if let Some(stmt) = cfg.source_stmt_id(vertex) {
            if !budget.spend() {
                return None;
            }
            reached_statements.insert(stmt);
        }
        if let Some(cfg::vertex::CfgVertex::BasicBlock(block)) = cfg.vertex(vertex) {
            for &stmt in block.statements() {
                if !budget.spend() {
                    return None;
                }
                reached_statements.insert(stmt);
            }
        }
        for (target, kind) in cfg.outgoing_edges(vertex) {
            if !budget.spend() {
                return None;
            }
            if kind.is_executable() {
                stack.push(target);
            }
        }
    }
    Some(reached_statements)
}

fn owner_possible_in_any_build(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
) -> EnvFlags {
    let options = crate::method_environment::notify_env_options();
    match owner {
        DefWithBodyId::Method(local_id) => {
            let method = hir_def::MethodId { module: ModuleId::new(file_id), local_id };
            crate::method_environment::possible_method_env(db, method, &options)
                .unwrap_or(EnvFlags::ALL)
        }
        DefWithBodyId::ModuleCode => {
            let metadata = db.module_metadata(ModuleId::new(file_id));
            let env = hir_def::execution_env::module_code_env(&metadata, &options);
            if env.is_empty() && metadata.module_type == bsl_metadata::ModuleType::Unknown {
                EnvFlags::ALL
            } else {
                env
            }
        }
    }
}

fn candidate_site_reachable_in_any_build(
    type_env: EnvFlags,
    owner_env: EnvFlags,
    enclosing_stmt: Option<StmtId>,
    reachable_stmts: &FxHashSet<StmtId>,
    range: TextRange,
    conditional_envs: &mut ConditionalEnvIndex<'_>,
    budget: &mut NotifyWorkBudget<'_>,
) -> Option<bool> {
    if enclosing_stmt.is_some_and(|stmt| !reachable_stmts.contains(&stmt)) {
        return Some(false);
    }
    let (definite, undecided) = conditional_envs.tracking(range, budget)?;
    let possible_site_env = definite | undecided;
    Some(possible_site_env.intersects(owner_env & type_env))
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
) -> Option<NotifyTarget> {
    inference
        .notify_receiver_resolution(receiver)
        .map(|(origin, ty)| normalize_resolved_receiver(origin, db, ty))
}

/// A receiver inference could not attribute to a bare name is not proven foreign: a
/// non-bare expression whose type is not concrete data may still hold `ЭтотОбъект`, and
/// an indeterminate bare read stopped on an incomplete surface. Both stay uncertain, so
/// they keep the local callback's signature without creating an edge or a finding.
fn unresolved_receiver_target(
    db: &dyn HirDatabase,
    inference: &InferOwnerResult,
    receiver: ExprId,
    receiver_syntax: NotifyReceiverSyntax,
) -> NotifyTarget {
    let may_be_local = match receiver_syntax {
        NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::BareName => {
            inference.notify_receiver_is_indeterminate(receiver)
        }
        NotifyReceiverSyntax::Other => inference
            .type_id_of_expr(receiver)
            .is_none_or(|ty| !is_known_data_value(db, db.lookup_type(ty))),
    };
    if may_be_local {
        NotifyTarget::LocalBinding
    } else {
        NotifyTarget::Unsupported
    }
}

fn normalize_resolved_receiver(
    origin: BareNameOrigin,
    db: &dyn bsl_types::intern::TypeKernelDb,
    ty: bsl_types::kind::TypeId,
) -> NotifyTarget {
    match (origin, db.lookup_type(ty)) {
        (BareNameOrigin::SelfName, _) => NotifyTarget::ThisObject,
        (BareNameOrigin::CommonModule, TypeKind::CommonModule(module)) => {
            NotifyTarget::Module(Name::new(&module.name))
        }
        (
            BareNameOrigin::FlowLocal
            | BareNameOrigin::UserBinding
            | BareNameOrigin::GlobalExport
            | BareNameOrigin::ObjectMember
            | BareNameOrigin::FormMember,
            ty,
        ) if !is_known_data_value(db, ty) => NotifyTarget::LocalBinding,
        _ => NotifyTarget::Unsupported,
    }
}

/// Whether this type proves the receiver cannot be the current module object.
/// Object-shaped context types deliberately remain unknown; unions prove this only
/// when every alternative independently proves it.
fn is_known_data_value(db: &dyn bsl_types::intern::TypeKernelDb, ty: &TypeKind) -> bool {
    match ty {
        TypeKind::Never
        | TypeKind::Null
        | TypeKind::Undefined
        | TypeKind::Number(_)
        | TypeKind::String(_)
        | TypeKind::Date(_)
        | TypeKind::Boolean
        | TypeKind::Uuid
        | TypeKind::Array(_)
        | TypeKind::Map(_)
        | TypeKind::Structure(_)
        | TypeKind::ValueList(_)
        | TypeKind::ValueTable(_)
        | TypeKind::ValueTableRow(_)
        | TypeKind::MetadataRef(_)
        | TypeKind::MetadataReferenceCollection(_)
        | TypeKind::MetadataReference { .. }
        | TypeKind::MetadataObjectCollection(_)
        | TypeKind::TabularSection { .. }
        | TypeKind::TabularSectionRow { .. }
        | TypeKind::RegisterDimension { .. }
        | TypeKind::RegisterResource { .. }
        | TypeKind::RegisterAttribute { .. }
        | TypeKind::RegisterFilter { .. }
        | TypeKind::Attribute { .. }
        | TypeKind::FormData { .. }
        | TypeKind::FormControl { .. }
        | TypeKind::ValueStorage
        | TypeKind::TypeDescriptor
        | TypeKind::Function(_)
        | TypeKind::QueryResult(_)
        | TypeKind::QueryResultSelection(_)
        | TypeKind::QueryBatchResult { .. }
        | TypeKind::Query { .. }
        | TypeKind::ManagerCollection(_) => true,
        TypeKind::Union(members) => {
            !members.is_empty()
                && members.iter().all(|member| is_known_data_value(db, db.lookup_type(*member)))
        }
        // Unknown, broad references and module/object context types may include self.
        TypeKind::Unknown
        | TypeKind::Any
        | TypeKind::AnyMetadataRef { .. }
        | TypeKind::AnyRef
        | TypeKind::MetadataObject(_)
        | TypeKind::ThisObject { .. }
        | TypeKind::ThisManager { .. }
        | TypeKind::CommonModule(_)
        | TypeKind::PlatformObject(_)
        | TypeKind::ObjectManager(_) => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::builders::Builders;
    use bsl_types::intern::TypeKernelDb;
    use bsl_types::testing::InMemoryDb;

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
    fn signature_verdict_does_not_protect_a_site_unreachable_in_every_build() {
        let mut unreachable = candidate(
            NotifyReceiverSyntax::SelfName,
            NotifyCandidateReceiver::NotCompiled,
            NotifyCandidateKind::Static,
            false,
        );
        unreachable.site_reachable = false;
        assert_eq!(unreachable.signature_protection(), NotifySignatureProtection::NotProtected);
    }

    #[test]
    fn signature_verdict_preserves_a_candidate_available_in_an_alternative_build() {
        let candidate = candidate(
            NotifyReceiverSyntax::SelfName,
            NotifyCandidateReceiver::NotCompiled,
            NotifyCandidateKind::Static,
            false,
        );
        assert!(candidate.site_reachable);
        assert_eq!(candidate.signature_protection(), NotifySignatureProtection::Protected);
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
    fn signature_verdict_protects_a_local_binding_that_may_be_this_object() {
        // A local binding of unknown runtime value may hold `ЭтотОбъект`; so does an
        // unresolved non-bare or indeterminate receiver, which classifies the same way.
        // A static constructor therefore stays protected without asserting a local
        // callback reference or an edge to it.
        for kind in [NotifyCandidateKind::Static, NotifyCandidateKind::DynamicArray] {
            assert_eq!(
                candidate(
                    NotifyReceiverSyntax::BareName,
                    NotifyCandidateReceiver::LocalBinding,
                    kind,
                    false
                )
                .signature_protection(),
                NotifySignatureProtection::Protected,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn classification_conservatively_protects_an_uncertain_receiver() {
        let registration = NotifyReg {
            caller: CallerId::ModuleCode,
            callback_expr: ExprId::from_raw(0.into()),
            callback_name: Name::new("H"),
            receiver_syntax: NotifyReceiverSyntax::BareName,
            target: NotifyTarget::LocalBinding,
            environments: NotifyRegistrationEnvironments::default(),
            range: TextRange::empty(TextSize::new(0)),
        };
        let classified = classify_registration(
            ExprId::from_raw(0.into()),
            NotifyCallbackSlot::Handler,
            NotifyCandidateKind::Static,
            registration,
            None,
            true,
        );
        assert_eq!(classified.receiver, NotifyCandidateReceiver::LocalBinding);
        assert!(
            classified.protects_local_method,
            "an uncertain receiver must conservatively protect the local method"
        );
    }

    #[test]
    fn resolved_object_shaped_bare_receivers_are_conservatively_protected() {
        let db = InMemoryDb::new();
        let unknown = db.unknown();
        for origin in
            [BareNameOrigin::GlobalExport, BareNameOrigin::ObjectMember, BareNameOrigin::FormMember]
        {
            let target = normalize_resolved_receiver(origin, &db, unknown);
            assert_eq!(target, NotifyTarget::LocalBinding, "{origin:?}");
            let registration = NotifyReg {
                caller: CallerId::ModuleCode,
                callback_expr: ExprId::from_raw(0.into()),
                callback_name: Name::new("H"),
                receiver_syntax: NotifyReceiverSyntax::BareName,
                target,
                environments: NotifyRegistrationEnvironments::default(),
                range: TextRange::empty(TextSize::new(0)),
            };
            let candidate = classify_registration(
                ExprId::from_raw(0.into()),
                NotifyCallbackSlot::Handler,
                NotifyCandidateKind::Static,
                registration,
                None,
                true,
            );
            assert_eq!(
                candidate.signature_protection(),
                NotifySignatureProtection::Protected,
                "{origin:?}"
            );
            assert!(candidate.protects_local_method, "{origin:?}");
        }
    }

    #[test]
    fn concrete_value_receivers_are_foreign_but_self_and_unknown_stay_conservative() {
        let db = InMemoryDb::new();
        for origin in [
            BareNameOrigin::FlowLocal,
            BareNameOrigin::UserBinding,
            BareNameOrigin::GlobalExport,
            BareNameOrigin::ObjectMember,
            BareNameOrigin::FormMember,
        ] {
            assert_eq!(
                normalize_resolved_receiver(origin, &db, db.boolean()),
                NotifyTarget::Unsupported,
                "known data for {origin:?} cannot be the current object"
            );
        }

        let value_storage = db.value_storage();
        let metadata_ref = db.metadata_ref(
            bsl_types::kind::MetadataKind::CatalogRef,
            "Контрагенты".to_string(),
            &bsl_types::testing::RootConfigCtx,
        );
        let query_result = db.query_result(None, bsl_types::facet::ProjectionSource::Unknown);
        for ty in [value_storage, metadata_ref, query_result] {
            assert_eq!(
                normalize_resolved_receiver(BareNameOrigin::FlowLocal, &db, ty),
                NotifyTarget::Unsupported,
                "{:#?} is a concrete value, not the current module object",
                db.lookup_type(ty)
            );
        }

        let only_values = db.union(vec![value_storage, db.boolean()]);
        assert_eq!(
            normalize_resolved_receiver(BareNameOrigin::FlowLocal, &db, only_values),
            NotifyTarget::Unsupported,
            "a union is foreign only when every alternative is known data"
        );
        let mixed_with_unknown = db.union(vec![value_storage, db.any_ref()]);
        assert_eq!(
            normalize_resolved_receiver(BareNameOrigin::FlowLocal, &db, mixed_with_unknown),
            NotifyTarget::LocalBinding,
            "a broad reference union arm preserves the possibility of self"
        );
        assert!(
            !is_known_data_value(&db, &TypeKind::Union(Arc::from([value_storage, db.unknown()])),),
            "an explicit unknown union arm cannot prove a foreign receiver"
        );
        assert_eq!(
            normalize_resolved_receiver(BareNameOrigin::SelfName, &db, db.unknown()),
            NotifyTarget::ThisObject,
            "explicit self stays self regardless of inferred type"
        );
        let this_object = db.mk_this_object(
            bsl_types::kind::ConfigId::Root,
            bsl_types::facet::MdoRefFacet::new(
                bsl_metadata::MdoType::Catalog,
                "Контрагенты".to_string(),
            ),
        );
        assert_eq!(
            normalize_resolved_receiver(BareNameOrigin::FlowLocal, &db, this_object),
            NotifyTarget::LocalBinding,
            "a local assigned from ЭтотОбъект remains conservatively possible self"
        );
        let metadata_object = db.metadata_object(
            bsl_types::kind::MetadataKind::CatalogObject,
            "Контрагенты".to_string(),
            &bsl_types::testing::RootConfigCtx,
        );
        assert_eq!(
            normalize_resolved_receiver(BareNameOrigin::FlowLocal, &db, metadata_object),
            NotifyTarget::LocalBinding,
            "an object of the same metadata type can execute this module's handler"
        );

        let foreign = candidate(
            NotifyReceiverSyntax::BareName,
            NotifyCandidateReceiver::Unknown,
            NotifyCandidateKind::Static,
            false,
        );
        assert_eq!(
            foreign.signature_protection(),
            NotifySignatureProtection::NotProtected,
            "a proven-foreign receiver is not promoted to an object-shaped value"
        );

        let unknown_array = candidate(
            NotifyReceiverSyntax::BareName,
            NotifyCandidateReceiver::Unknown,
            NotifyCandidateKind::DynamicArray,
            false,
        );
        assert_eq!(
            unknown_array.signature_protection(),
            NotifySignatureProtection::Protected,
            "the pre-existing fully unknown array boundary remains unchanged"
        );

        let unavailable = candidate(
            NotifyReceiverSyntax::BareName,
            NotifyCandidateReceiver::NotCompiled,
            NotifyCandidateKind::Static,
            false,
        );
        assert_eq!(unavailable.signature_protection(), NotifySignatureProtection::NotProtected);
    }

    #[test]
    fn conditional_sort_charge_is_log_linear_and_saturating() {
        assert_eq!(sort_work(0), 0);
        assert_eq!(sort_work(1), 1);
        assert_eq!(sort_work(2), 2);
        assert_eq!(sort_work(3), 6);
        assert_eq!(sort_work(4), 8);
        assert_eq!(sort_work(1024), 10_240);
        assert_eq!(sort_work(usize::MAX), usize::MAX);
    }

    #[test]
    fn candidate_name_verdict_stays_cheap_and_never_needs_constructor_facts() {
        let platform = PlatformDataInner::instance();
        for name in ["ОписаниеОповещения", "NotifyDescription", "CallbackDescription"]
        {
            assert!(
                candidate_supported_without_construction(platform, true, Some(name)),
                "{name} must pass under the syntax fallback without served facts"
            );
        }
        assert!(!candidate_supported_without_construction(platform, true, Some("Структура")));
        assert!(!candidate_supported_without_construction(platform, false, Some("Структура")));
        assert!(!candidate_supported_without_construction(platform, true, None));
    }

    #[test]
    fn production_work_budget_scales_with_body_size_and_has_a_hard_cap() {
        let small = notify_target_work_budget(1);
        let larger = notify_target_work_budget(2);
        assert_eq!(small, NOTIFY_TARGET_WORK_BUDGET + NOTIFY_TARGET_WORK_PER_BODY_ITEM);
        assert!(larger > small);
        assert_eq!(notify_target_work_budget(usize::MAX), NOTIFY_TARGET_MAX_WORK_BUDGET);
        assert_eq!(NOTIFY_TARGET_MAX_WORK_BUDGET, 524_288);
    }

    #[test]
    fn empty_bodies_saturate_budget_sizing_early_with_cancellation_checks() {
        let empty_bodies = NOTIFY_TARGET_BUDGET_SATURATION_SIZE + 1;
        let source = (0..empty_bodies)
            .map(|index| format!("Процедура Пустая{index}()\nКонецПроцедуры\n"))
            .collect::<String>();
        let parse = parser::parse(&source);
        let bodies = hir_def::ModuleBodies::from_parse(&parse);
        assert_eq!(bodies.len(), empty_bodies);
        assert!(bodies
            .iter_bodies()
            .take(NOTIFY_TARGET_BUDGET_SATURATION_SIZE)
            .all(|(_, body)| body_item_count(body) == 1));

        let mut cancel_checks = 0;
        let mut cancel = || cancel_checks += 1;
        let budget = NotifyWorkBudget::for_module(&mut cancel, &bodies);
        assert_eq!(budget.remaining, NOTIFY_TARGET_MAX_WORK_BUDGET);
        assert_eq!(
            cancel_checks,
            NOTIFY_TARGET_BUDGET_SATURATION_SIZE / NOTIFY_TARGET_CANCEL_INTERVAL
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
        let mut budget = NotifyWorkBudget::with_limit(&mut cancel, NOTIFY_TARGET_WORK_BUDGET);
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
