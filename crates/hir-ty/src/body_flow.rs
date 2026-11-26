//! One body's flow inputs — the body itself, its syntactic events, its CFG and
//! its reaching definitions — handed to every analysis of that body instead of
//! each analysis building the graph again.
//!
//! A body the database memoises (a method's, the module-level code's) reads the
//! memos keyed by its owner, so a revision builds them once for every reader.
//! Any other body — an effective `&ИзменениеИКонтроль` splice, a woven
//! extension body, a synthetic one — has ids of its own and builds its flow
//! once, inside the flow its inference owns: the memo of a same-named method
//! describes another body.

use std::sync::{Arc, OnceLock};

use cfg::ControlFlowGraph;
use dataflow::reaching_defs::ReachingDefsResult;
use hir_def::body::events::BodyEvents;
use hir_def::body::Body;
use hir_def::{MethodId, MethodIdInput, ModuleId};
use vfs::FileId;

use crate::db::HirDatabase;

pub struct BodyFlow {
    body: Arc<Body>,
    origin: Origin,
    /// Answers the database does not memoise for this body, built on first use.
    events: OnceLock<Arc<BodyEvents>>,
    cfg: OnceLock<Arc<ControlFlowGraph>>,
    reaching_defs: OnceLock<Option<Arc<ReachingDefsResult>>>,
    executable_reaching_defs: OnceLock<Option<Arc<ReachingDefsResult>>>,
}

enum Origin {
    Method(MethodId),
    ModuleCode(FileId),
    Detached { seed_params: bool },
}

impl BodyFlow {
    /// The flow of `method` when `body` is its memoised body, and otherwise
    /// one of the body's own.
    pub fn for_method(db: &dyn HirDatabase, method: MethodIdInput<'_>, body: &Arc<Body>) -> Self {
        if Arc::ptr_eq(body, db.method_body_ref(method)) {
            Self::new(Arc::clone(body), Origin::Method(method.method_id(db)))
        } else {
            Self::detached(Arc::clone(body), true)
        }
    }

    /// The flow of the module-level code of `file_id` when `body` is its
    /// memoised body, and otherwise one of the body's own.
    pub fn for_module_code(db: &dyn HirDatabase, file_id: FileId, body: &Arc<Body>) -> Self {
        let memoised = db.module_bodies_ref(ModuleId::new(file_id)).module_code_arc().cloned();
        if memoised.is_some_and(|memoised| Arc::ptr_eq(body, &memoised)) {
            Self::new(Arc::clone(body), Origin::ModuleCode(file_id))
        } else {
            Self::detached(Arc::clone(body), false)
        }
    }

    /// A flow of the body's own; `seed_params` says whether its parameters
    /// define their names on entry, as a method's do.
    pub fn detached(body: Arc<Body>, seed_params: bool) -> Self {
        Self::new(body, Origin::Detached { seed_params })
    }

    fn new(body: Arc<Body>, origin: Origin) -> Self {
        Self {
            body,
            origin,
            events: OnceLock::new(),
            cfg: OnceLock::new(),
            reaching_defs: OnceLock::new(),
            executable_reaching_defs: OnceLock::new(),
        }
    }

    pub fn body(&self) -> &Arc<Body> {
        &self.body
    }

    pub fn events(&self, db: &dyn HirDatabase) -> Arc<BodyEvents> {
        self.events_with_work(db, &mut |_| true).expect("an unlimited work budget cannot exhaust")
    }

    /// [`BodyFlow::events`] that charges a whole-body estimate before asking for
    /// a database memo which may be cold. The collection has no internal
    /// cancellation point, so a caller that cannot afford the estimate starts
    /// none of the build.
    pub fn events_with_work(
        &self,
        db: &dyn HirDatabase,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<BodyEvents>> {
        match self.origin {
            Origin::Method(method) => {
                self.memoized_with_work(spend)?;
                Some(db.method_body_events(MethodIdInput::new(db, method)))
            }
            Origin::ModuleCode(file_id) => {
                self.memoized_with_work(spend)?;
                db.module_code_body_events(file_id)
            }
            Origin::Detached { .. } => self.own_events_with_work(spend),
        }
    }

    pub fn cfg(&self, db: &dyn HirDatabase) -> Arc<ControlFlowGraph> {
        self.cfg_with_work(db, &mut |_| true).expect("an unlimited work budget cannot exhaust")
    }

    /// [`BodyFlow::cfg`] that charges a whole-body estimate before asking for a
    /// database memo which may be cold. The builder has no internal cancellation
    /// point, so it runs whole once the estimate is affordable; see
    /// [`BodyFlow::events_with_work`].
    pub fn cfg_with_work(
        &self,
        db: &dyn HirDatabase,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<ControlFlowGraph>> {
        match self.origin {
            Origin::Method(method) => {
                self.memoized_with_work(spend)?;
                Some(db.method_cfg(MethodIdInput::new(db, method)))
            }
            Origin::ModuleCode(file_id) => {
                self.memoized_with_work(spend)?;
                Some(db.module_code_cfg(file_id))
            }
            Origin::Detached { .. } => self.own_cfg_with_work(spend),
        }
    }

    /// Reaching definitions over every edge of the graph, executable or not.
    pub fn reaching_defs(&self, db: &dyn HirDatabase) -> Option<Arc<ReachingDefsResult>> {
        match self.origin {
            Origin::Method(method) => {
                db.method_reaching_definitions(MethodIdInput::new(db, method))
            }
            Origin::ModuleCode(file_id) => db.module_code_reaching_definitions(file_id),
            Origin::Detached { seed_params } => self
                .reaching_defs
                .get_or_init(|| {
                    dataflow::reaching_defs::solve_reaching_definitions(
                        self.cfg(db),
                        &self.body,
                        self.events(db),
                        seed_params,
                    )
                })
                .clone(),
        }
    }

    /// [`BodyFlow::reaching_defs`] with the solve's work charged to `spend`. The
    /// database memo is not used, because its first build would run outside this
    /// caller's budget; a refused solve is not cached, while a solution or a
    /// solver failure is.
    pub fn reaching_defs_with_work(
        &self,
        db: &dyn HirDatabase,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<ReachingDefsResult>> {
        if let Some(cached) = self.reaching_defs.get() {
            return cached.clone();
        }

        let mut refused = false;
        let mut metered = |work: usize| {
            if spend(work) {
                true
            } else {
                refused = true;
                false
            }
        };
        let cfg = self.cfg_with_work(db, &mut metered);
        let events = if cfg.is_some() { self.events_with_work(db, &mut metered) } else { None };
        let solved = match (cfg, events) {
            (Some(cfg), Some(events)) => {
                dataflow::reaching_defs::solve_reaching_definitions_with_work(
                    cfg,
                    &self.body,
                    events,
                    self.seeds_params(),
                    &mut metered,
                )
            }
            _ => None,
        };
        if refused {
            return solved;
        }

        let _ = self.reaching_defs.set(solved.clone());
        solved
    }

    /// Reaching definitions where nothing crosses an unexecutable edge. No
    /// memo holds this solution, so every flow solves it once.
    ///
    /// A body with a syntax error is solved over the statements its lowering
    /// kept: a call proved to run a query stays proved in a method that fails
    /// to parse elsewhere. A reader that needs the value a name holds must
    /// refuse such a body itself ([`hir_def::body::events::hir_suppressed`]).
    pub fn executable_reaching_defs(
        &self,
        db: &dyn HirDatabase,
    ) -> Option<Arc<ReachingDefsResult>> {
        self.executable_reaching_defs_with_work(db, &mut |_| true)
    }

    /// [`BodyFlow::executable_reaching_defs`] that charges the solve's actual work
    /// to `spend` and stops when it refuses more. The index build, the entry state
    /// and every block visit are covered, so a caller's work budget also bounds a
    /// body whose dynamic-array proof would otherwise solve on its own.
    ///
    /// A stop caused by `spend` says how much of the body this caller could afford,
    /// not what the body is, so it is not cached: a later reader with room solves
    /// the same flow from scratch. A solution, or a failure of the solve's own
    /// visit bound, is a property of the body and is kept.
    pub fn executable_reaching_defs_with_work(
        &self,
        db: &dyn HirDatabase,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<ReachingDefsResult>> {
        if let Some(cached) = self.executable_reaching_defs.get() {
            return cached.clone();
        }

        let mut refused = false;
        let mut metered = |work: usize| {
            if spend(work) {
                true
            } else {
                refused = true;
                false
            }
        };
        let cfg = self.cfg_with_work(db, &mut metered);
        let events = if cfg.is_some() { self.events_with_work(db, &mut metered) } else { None };
        let solved = match (cfg, events) {
            (Some(cfg), Some(events)) => {
                dataflow::reaching_defs::solve_executable_reaching_definitions_with_work(
                    cfg,
                    &self.body,
                    events,
                    self.seeds_params(),
                    dataflow::reaching_defs::RecoveredBody::Solve,
                    &mut metered,
                )
            }
            _ => None,
        };
        if refused {
            return solved;
        }

        match self.executable_reaching_defs.set(solved.clone()) {
            Ok(()) => solved,
            Err(_) => self.executable_reaching_defs.get().cloned().unwrap_or(solved),
        }
    }

    /// Salsa does not expose whether these queries are currently memoized. Charge
    /// the conservative whole-body build estimate before asking for one, so a cold
    /// query cannot do unmetered work; warm queries conservatively pay the same cost.
    fn memoized_with_work(&self, spend: &mut dyn FnMut(usize) -> bool) -> Option<()> {
        spend(body_build_work(&self.body)).then_some(())
    }

    fn seeds_params(&self) -> bool {
        match self.origin {
            Origin::Method(_) => true,
            Origin::ModuleCode(_) => false,
            Origin::Detached { seed_params } => seed_params,
        }
    }

    fn own_events_with_work(
        &self,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<BodyEvents>> {
        if let Some(cached) = self.events.get() {
            return Some(Arc::clone(cached));
        }
        if !spend(body_build_work(&self.body)) {
            return None;
        }
        let events = Arc::new(BodyEvents::collect(&self.body));
        let _ = self.events.set(Arc::clone(&events));
        Some(events)
    }

    fn own_cfg_with_work(
        &self,
        spend: &mut dyn FnMut(usize) -> bool,
    ) -> Option<Arc<ControlFlowGraph>> {
        if let Some(cached) = self.cfg.get() {
            return Some(Arc::clone(cached));
        }
        if !spend(body_build_work(&self.body)) {
            return None;
        }
        let _span = tracing::info_span!("body_flow_cfg").entered();
        let cfg = Arc::new(
            cfg::CfgBuilder::new().build_graph_from_hir(self.body.body_stmts_typed(), &self.body),
        );
        let _ = self.cfg.set(Arc::clone(&cfg));
        Some(cfg)
    }
}

/// A conservative one-shot estimate for building a detached body's events or CFG.
/// The database-memoised bodies read their memo instead; only the uncached build
/// this flow owns is charged, because its builders expose no progress callback.
fn body_build_work(body: &Body) -> usize {
    body.expr_count().saturating_add(body.stmt_count()).saturating_add(body.binding_count()).max(1)
}
