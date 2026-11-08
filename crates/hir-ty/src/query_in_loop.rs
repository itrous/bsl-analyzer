//! Where a body executes a query: the semantic facts behind `CreateQueryInCycle`.
//!
//! The facts name expressions, not positions, and say nothing about suppression or the
//! diagnostics profile, so they stay equal under a comment edit or a move within the file.

use std::collections::VecDeque;

use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};
use hir_def::{DefWithBodyId, ExprId, MethodId, Name};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::call_resolution::{CallSelection, CandidateId};
use crate::method_lookup::{
    is_platform_name, is_platform_name_str, projection_of_query_receiver_id,
};

/// The platform operation that sends a query to the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryExecutionKind {
    Execute,
    ExecuteBatch,
}

/// A call inference proved to execute a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryExecutionSite {
    /// The whole call expression, not its callee.
    pub call: ExprId,
    pub kind: QueryExecutionKind,
    /// The call sits in a loop body. A header of the outermost loop does not count.
    pub in_loop_body: bool,
}

/// A call inference resolved to a method of the module under analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalQueryCall {
    /// The whole call expression, not its callee.
    pub call: ExprId,
    pub target: MethodId,
    /// See [`QueryExecutionSite::in_loop_body`].
    pub in_loop_body: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyQueryFacts {
    pub executions: Vec<QueryExecutionSite>,
    pub local_calls: Vec<LocalQueryCall>,
}

/// A call in a file: expression ids are local to their body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryCallKey {
    pub owner: DefWithBodyId,
    pub call: ExprId,
}

/// A call in a loop body that leads to a query execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryInLoopCandidate {
    Execution(QueryCallKey),
    LocalCall { site: QueryCallKey, target: MethodId },
}

/// The calls in loop bodies that reach an execution in `active`, directly or through local calls.
///
/// A least fixed point over the reversed local calls: a method is marked at most once, so
/// recursion ends by itself, a component no active execution feeds stays unmarked, and nothing
/// limits the length of a chain. `O(methods + local calls + executions)`.
pub fn query_in_loop_candidates(
    facts_by_body: &FxHashMap<DefWithBodyId, BodyQueryFacts>,
    active: &FxHashSet<QueryCallKey>,
) -> Vec<QueryInLoopCandidate> {
    let mut callers: FxHashMap<hir_def::MethodKey, Vec<DefWithBodyId>> = FxHashMap::default();
    let mut marked: FxHashSet<hir_def::MethodKey> = FxHashSet::default();
    let mut queue = VecDeque::new();
    for (&owner, facts) in facts_by_body {
        for local_call in &facts.local_calls {
            callers.entry(local_call.target.local_id).or_default().push(owner);
        }
        let has_active_execution = facts
            .executions
            .iter()
            .any(|execution| active.contains(&QueryCallKey { owner, call: execution.call }));
        if let DefWithBodyId::Method(key) = owner {
            if has_active_execution && marked.insert(key) {
                queue.push_back(key);
            }
        }
    }
    while let Some(callee) = queue.pop_front() {
        for caller in callers.get(&callee).into_iter().flatten() {
            // Module code runs on its own: nothing calls it, so nothing is marked through it.
            if let DefWithBodyId::Method(key) = caller {
                if marked.insert(*key) {
                    queue.push_back(*key);
                }
            }
        }
    }

    let mut candidates = Vec::new();
    for (&owner, facts) in facts_by_body {
        candidates.extend(
            facts
                .executions
                .iter()
                .filter(|execution| execution.in_loop_body)
                .map(|execution| QueryCallKey { owner, call: execution.call })
                .filter(|site| active.contains(site))
                .map(QueryInLoopCandidate::Execution),
        );
        candidates.extend(
            facts
                .local_calls
                .iter()
                .filter(|local_call| {
                    local_call.in_loop_body && marked.contains(&local_call.target.local_id)
                })
                .map(|local_call| QueryInLoopCandidate::LocalCall {
                    site: QueryCallKey { owner, call: local_call.call },
                    target: local_call.target,
                }),
        );
    }
    candidates
}

impl QueryExecutionKind {
    /// The operation `method` spells. Exact names: `ExecuteBatchWithIntermediateData` is a
    /// different operation, not a longer spelling of this one.
    pub(crate) fn of_method(method: &Name) -> Option<Self> {
        if is_platform_name(method, "Выполнить", "Execute") {
            Some(Self::Execute)
        } else if is_platform_name(method, "ВыполнитьПакет", "ExecuteBatch") {
            Some(Self::ExecuteBatch)
        } else {
            None
        }
    }

    /// Whether a value of type `receiver` executes a query through this operation. Every arm
    /// of a union has to: a foreign arm leaves the call unproved.
    pub(crate) fn proved_by(self, db: &dyn TypeKernelDb, receiver: TypeId) -> bool {
        if projection_of_query_receiver_id(db, receiver).is_some() {
            return true;
        }
        match db.lookup_type(receiver) {
            TypeKind::Union(arms) => arms.iter().all(|arm| self.proved_by(db, *arm)),
            TypeKind::PlatformObject(facet) => {
                self == Self::Execute
                    && (is_platform_name_str(&facet.name, "ПостроительЗапроса", "QueryBuilder")
                        || is_platform_name_str(&facet.name, "ПостроительОтчета", "ReportBuilder"))
            }
            _ => false,
        }
    }
}

/// The user method resolution settled on: signature variants of one method fold into it, while
/// a choice between different methods names none.
pub(crate) fn selected_user_method(selection: &CallSelection) -> Option<MethodId> {
    let method_of = |id: &CandidateId| match id {
        CandidateId::User { method, .. } => Some(*method),
        _ => None,
    };
    let method = match selection {
        CallSelection::Unique { candidate } => method_of(candidate)?,
        CallSelection::Ambiguous { candidates } => {
            let first = method_of(candidates.first()?)?;
            candidates.iter().all(|id| method_of(id) == Some(first)).then_some(first)?
        }
        CallSelection::Rejected(_) => return None,
    };
    Some(method.into())
}

/// Whether resolution settled on the platform's own method. A rejected call or one that went
/// to a user method proves nothing, whatever the name says.
pub(crate) fn selected_platform_method(selection: &CallSelection) -> bool {
    match selection {
        CallSelection::Unique { candidate } => candidate.is_platform(),
        CallSelection::Ambiguous { candidates } => candidates.iter().all(|id| id.is_platform()),
        CallSelection::Rejected(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir_def::{MethodKey, ModuleId};
    use la_arena::RawIdx;
    use vfs::FileId;

    fn expr(n: u32) -> ExprId {
        ExprId::from_raw(RawIdx::from(n))
    }

    fn method(name: &str) -> DefWithBodyId {
        DefWithBodyId::Method(MethodKey::first(name))
    }

    fn call_to(call: u32, target: &str, in_loop_body: bool) -> LocalQueryCall {
        LocalQueryCall {
            call: expr(call),
            target: MethodId {
                module: ModuleId::new(FileId(0)),
                local_id: MethodKey::first(target),
            },
            in_loop_body,
        }
    }

    fn execution(call: u32, in_loop_body: bool) -> QueryExecutionSite {
        QueryExecutionSite { call: expr(call), kind: QueryExecutionKind::Execute, in_loop_body }
    }

    /// `main` calls `entry` in a loop; every body gets its own execution ids.
    fn graph(edges: &[(&str, &str)], seeds: &[&str]) -> FxHashMap<DefWithBodyId, BodyQueryFacts> {
        let mut facts: FxHashMap<DefWithBodyId, BodyQueryFacts> = FxHashMap::default();
        for (i, (from, to)) in edges.iter().enumerate() {
            let in_loop_body = *from == "main";
            facts.entry(method(from)).or_default().local_calls.push(call_to(
                i as u32,
                to,
                in_loop_body,
            ));
        }
        for (i, seed) in seeds.iter().enumerate() {
            facts
                .entry(method(seed))
                .or_default()
                .executions
                .push(execution(1000 + i as u32, false));
        }
        facts
    }

    fn all_active(facts: &FxHashMap<DefWithBodyId, BodyQueryFacts>) -> FxHashSet<QueryCallKey> {
        facts
            .iter()
            .flat_map(|(&owner, f)| {
                f.executions.iter().map(move |e| QueryCallKey { owner, call: e.call })
            })
            .collect()
    }

    fn found(edges: &[(&str, &str)], seeds: &[&str]) -> usize {
        let facts = graph(edges, seeds);
        query_in_loop_candidates(&facts, &all_active(&facts)).len()
    }

    #[test]
    fn query_in_loop_chain_reaches_the_seed_or_nothing() {
        let edges = [("main", "a"), ("a", "b"), ("b", "c")];
        assert_eq!(found(&edges, &["c"]), 1);
        assert_eq!(found(&edges, &[]), 0);
        assert_eq!(found(&[("main", "a"), ("b", "c")], &["c"]), 0);
    }

    #[test]
    fn query_in_loop_long_chain_has_no_depth_limit() {
        let names: Vec<String> = (0..20_000).map(|i| format!("m{i}")).collect();
        let mut edges: Vec<(&str, &str)> = vec![("main", names[0].as_str())];
        edges.extend(names.windows(2).map(|w| (w[0].as_str(), w[1].as_str())));
        assert_eq!(found(&edges, &[names.last().unwrap().as_str()]), 1);
        assert_eq!(found(&edges, &[]), 0);
    }

    #[test]
    fn query_in_loop_recursion_terminates_with_and_without_a_seed() {
        assert_eq!(found(&[("main", "a"), ("a", "a")], &["a"]), 1);
        assert_eq!(found(&[("main", "a"), ("a", "a")], &[]), 0);
        let mutual = [("main", "a"), ("a", "b"), ("b", "a")];
        assert_eq!(found(&mutual, &["b"]), 1);
        assert_eq!(found(&mutual, &["a"]), 1);
        assert_eq!(found(&mutual, &[]), 0);
        // Seed behind the component, reachable by its last edge only.
        let behind = [("main", "a"), ("a", "b"), ("b", "a"), ("b", "c")];
        assert_eq!(found(&behind, &["c"]), 1);
    }

    #[test]
    fn query_in_loop_diamond_and_two_seeds_keep_one_candidate() {
        let diamond = [("main", "top"), ("top", "l"), ("top", "r"), ("l", "low"), ("r", "low")];
        assert_eq!(found(&diamond, &["low"]), 1);
        assert_eq!(found(&diamond, &["l", "r"]), 1);
    }

    #[test]
    fn query_in_loop_suppressed_seed_is_cut_before_propagation() {
        let facts = graph(&[("main", "a"), ("a", "b"), ("a", "c")], &["b", "c"]);
        let key = |name: &str, n: u32| QueryCallKey { owner: method(name), call: expr(1000 + n) };
        let only = |keys: &[QueryCallKey]| keys.iter().copied().collect::<FxHashSet<_>>();
        assert_eq!(query_in_loop_candidates(&facts, &only(&[key("c", 1)])).len(), 1);
        assert_eq!(query_in_loop_candidates(&facts, &only(&[key("b", 0)])).len(), 1);
        assert_eq!(query_in_loop_candidates(&facts, &only(&[])).len(), 0);
    }

    #[test]
    fn query_in_loop_module_code_is_a_source_and_never_a_callee() {
        let mut facts: FxHashMap<DefWithBodyId, BodyQueryFacts> = FxHashMap::default();
        facts.entry(DefWithBodyId::ModuleCode).or_default().local_calls.push(call_to(1, "a", true));
        facts.entry(DefWithBodyId::ModuleCode).or_default().executions.push(execution(2, true));
        facts.entry(method("a")).or_default().executions.push(execution(3, false));
        let candidates = query_in_loop_candidates(&facts, &all_active(&facts));
        assert_eq!(candidates.len(), 2);
        // An execution in module code does not mark a method that module code calls.
        let mut silent = facts.clone();
        silent.get_mut(&method("a")).unwrap().executions.clear();
        assert_eq!(query_in_loop_candidates(&silent, &all_active(&silent)).len(), 1);
    }

    #[test]
    fn query_in_loop_same_expression_id_in_two_bodies_is_two_sites() {
        let mut facts: FxHashMap<DefWithBodyId, BodyQueryFacts> = FxHashMap::default();
        facts.entry(method("a")).or_default().executions.push(execution(7, true));
        facts.entry(method("b")).or_default().executions.push(execution(7, true));
        let active: FxHashSet<_> =
            [QueryCallKey { owner: method("a"), call: expr(7) }].into_iter().collect();
        assert_eq!(
            query_in_loop_candidates(&facts, &active),
            vec![QueryInLoopCandidate::Execution(QueryCallKey {
                owner: method("a"),
                call: expr(7)
            })]
        );
    }
}
