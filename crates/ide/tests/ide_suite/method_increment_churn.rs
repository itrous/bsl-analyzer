//! Churn of a method-body edit in a many-method module (github#113).
//!
//! The unit of recomputation is meant to be the method: after an edit inside
//! one body, the per-method queries of every *other* method must validate
//! from cache, not execute. The salsa event counters make that observable —
//! `execute` is a re-run, `validate` a reuse — and the synthetic module keeps
//! the edit sites and the caller graph known, so the positive control (an
//! interface edit must cost more than a body edit) cannot pass by accident.

use hir::{DefDatabase, ModuleId, Name};
use ide_db::base_db::{ParseStats, RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::DiagnosticsConfig;
use test_fixture::{SyntheticModule, SyntheticModuleSpec};
use vfs::{FileId, FileSet, VfsPath};

const METHODS: usize = 300;
const _: () = assert!(
    METHODS > hir::NAME_SET_METHOD_LIMIT,
    "the stand must be a module whose by-name misses cost a memo per name, not one set"
);
/// Edited method: in the middle of the file, with one caller (the next method);
/// a function, so that the dataflow only a function's body demands (path
/// termination behind a conditional return) is exercised by the edit too.
const EDITED: usize = METHODS / 2 + 1;

/// Every per-method link a body edit must leave alone for the other methods:
/// lowering and inference, the dataflow computed over the body, and the
/// diagnostics assembled from all of them.
const PER_METHOD_FAMILIES: &[&str] = &[
    "method_body_query",
    "infer_method_query",
    "method_cfg_query",
    "reaching_definitions_query",
    "method_path_terminates_query",
    "method_hir_metrics_query",
    "method_cyclomatic_query",
    "method_diagnostics_query",
    "method_line_diagnostics_query",
];

/// How many of the OTHER methods of a `methods`-method stand read `family`,
/// and so must validate after an edit elsewhere: every body reads all of them
/// except path termination, which is asked only for a function whose return
/// is conditional — on this stand every function, i.e. every odd index.
fn other_readers(family: &str, methods: usize) -> u64 {
    let readers = if family == "method_path_terminates_query" { methods / 2 } else { methods };
    (readers - 1) as u64
}

struct Stand {
    db: RootDatabaseImpl,
    file_id: FileId,
    module: SyntheticModule,
}

/// The stand's module: every method calls the previous one, every function
/// returns behind a condition.
fn synthetic_module() -> SyntheticModule {
    SyntheticModuleSpec { methods: METHODS, conditional_return_every: 1, ..Default::default() }
        .build()
}

impl Stand {
    fn new() -> Self {
        Self::with_module(synthetic_module())
    }

    fn with_module(module: SyntheticModule) -> Self {
        let mut db = RootDatabaseImpl::new_with_salsa_events();
        let mut file_set = FileSet::default();
        let file_id = FileId(0);
        file_set.insert(file_id, VfsPath::new("/Module.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, &module.text);
        Stand { db, file_id, module }
    }

    fn diagnostics_count(&self) -> usize {
        self.diagnostics().len()
    }

    fn diagnostics(&self) -> Vec<ide_diagnostics::Diagnostic> {
        ide_diagnostics::file_diagnostics(&self.db, self.file_id, &DiagnosticsConfig::all_enabled())
    }

    /// Warm every query the diagnostics read, then open a fresh event window.
    fn warm(&self) {
        let _ = self.diagnostics_count();
        let _ = self.diagnostics_count();
        assert!(self.db.salsa_events_reset(), "the stand database must count salsa events");
    }

    fn churn(&self, family: &str) -> (u64, u64) {
        self.db.salsa_family_churn(family).expect("events enabled")
    }

    /// Apply `text` as the next revision and run diagnostics once. The parse
    /// the diagnostics read is checked against a full parse of the same text:
    /// the spliced path (snapshot, diff, guards, fallback) must never be told
    /// apart from a full parse by tree, text, errors or memory estimate.
    fn edit_and_diagnose(&mut self, text: &str) -> usize {
        self.db.set_file_text(self.file_id, text);
        let diagnostics = self.diagnostics();
        let count = diagnostics.len();
        // The incremental assembly — per-method memos backdated or re-executed,
        // slabs, the file remainder, the lifts — must equal what a database
        // that has only ever seen this text computes from scratch.
        let fresh =
            Stand::with_module(SyntheticModule { text: text.to_string(), ..self.module.clone() });
        assert_eq!(
            diagnostics,
            fresh.diagnostics(),
            "the incremental diagnostics must equal a fresh computation"
        );
        let parse = self.db.parse(self.file_id);
        let full = parser::parse_with_shared_cache(text);
        assert!(syntax::green_eq(parse.green(), full.green()), "the parse must equal a full parse");
        assert_eq!(
            parse.syntax_node().text().to_string(),
            text,
            "the parse must carry the new text"
        );
        assert_eq!(parse.errors(), full.errors(), "the parse must carry the full parse's errors");
        assert_eq!(parse.heap_bytes(), full.heap_bytes(), "the memory estimate must match");
        count
    }

    fn parse_stats(&self) -> ParseStats {
        self.db.parse_stats()
    }
}

/// The parse outcome of one edit, as counter deltas: the warm-up already paid
/// one full parse without a snapshot, so absolute counts say nothing.
fn parse_outcome_of(edit: impl FnOnce(&SyntheticModule) -> String) -> ParseStats {
    let mut stand = Stand::new();
    stand.warm();
    let before = stand.parse_stats();
    let text = edit(&stand.module);
    stand.edit_and_diagnose(&text);
    let after = stand.parse_stats();
    let mut refused = [0u64; 8];
    for (slot, (a, b)) in refused.iter_mut().zip(after.refused.iter().zip(&before.refused)) {
        *slot = a - b;
    }
    ParseStats {
        full: after.full - before.full,
        spliced: after.spliced - before.spliced,
        mismatched: after.mismatched - before.mismatched,
        refused,
    }
}

/// An edit inside a method — body or header — is parsed as that method and
/// spliced into the old tree; a method inserted between methods is not
/// inside any, the guard refuses and the file is parsed in full. Insertion
/// is the positive control: without it a splice counter that never moves
/// would pass. `full` stays at zero throughout: it counts parses without a
/// snapshot, and the warm-up has already left one.
#[test]
fn edits_inside_a_method_are_spliced_and_insertions_are_not() {
    use parser::reparse::Refusal;

    let body = parse_outcome_of(|m| m.with_body_statement(EDITED, "Правка", "\n"));
    assert_eq!((body.spliced, body.full, body.mismatched), (1, 0, 0), "body edit: {body:?}");

    let signature = parse_outcome_of(|m| m.with_parameter(EDITED, "Новый"));
    assert_eq!((signature.spliced, signature.full), (1, 0), "signature edit: {signature:?}");

    for (label, outcome) in [
        ("insertion below", parse_outcome_of(|m| m.with_method_appended("Новая", "\n"))),
        ("insertion above", parse_outcome_of(|m| m.with_method_inserted_before(0, "Новая", "\n"))),
    ] {
        assert_eq!((outcome.spliced, outcome.full), (0, 0), "{label}: {outcome:?}");
        assert_eq!(outcome.refused[Refusal::OutsideMethod.index()], 1, "{label}: {outcome:?}");
    }
}

/// The stand is not vacuous: the edit is seen by the counters and by the
/// diagnostics themselves (the inserted assignment is an unused local).
#[test]
fn stand_observes_a_body_edit() {
    let mut stand = Stand::new();
    let symbol_tree = stand.db.symbol_tree(ModuleId::new(stand.file_id));
    let name = Name::new(&stand.module.methods[EDITED].name);
    let edited = symbol_tree.find_method(&name).expect("generated method must resolve");
    assert_eq!(edited.id.local_id, hir::MethodKey::first(&stand.module.methods[EDITED].name));
    assert_eq!(stand.module.methods[EDITED].callers, 1);

    let before = stand.diagnostics_count();
    stand.warm();
    let after = stand.edit_and_diagnose(&stand.module.with_body_statement(EDITED, "Правка", "\n"));
    assert!(after > before, "the inserted unused local must add a diagnostic: {before} → {after}");
    let (execute, _) = stand.churn("method_body_query");
    assert!(execute >= 1, "the edited body must be re-lowered at least once");
    let (execute, _) = stand.churn("infer_method_query");
    assert!(execute >= 1, "the edited body must be re-inferred at least once");
}

/// I1: a body edit re-executes the per-method queries of the edited method
/// only; every other method validates from cache.
#[test]
fn body_edit_reexecutes_only_the_edited_method() {
    let mut stand = Stand::new();
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_body_statement(EDITED, "Правка", "\n"));
    for family in PER_METHOD_FAMILIES {
        let (execute, validate) = stand.churn(family);
        assert_eq!(execute, 1, "{family}: only the edited method may execute");
        let expected = other_readers(family, METHODS);
        assert!(
            validate >= expected,
            "{family}: the other {expected} readers must validate, observed {validate}"
        );
    }
}

/// I1 positive control: an interface edit re-infers the edited method and its
/// callers, so it must cost strictly more executions than a body edit.
#[test]
fn signature_edit_costs_more_than_body_edit() {
    let mut body_stand = Stand::new();
    body_stand.warm();
    body_stand.edit_and_diagnose(&body_stand.module.with_body_statement(EDITED, "Правка", "\n"));
    let (body_execute, _) = body_stand.churn("infer_method_query");

    let mut sig_stand = Stand::new();
    let callers = sig_stand.module.methods[EDITED].callers as u64;
    assert!(callers >= 1, "the control needs a caller, or `>= 1 + callers` degenerates");
    sig_stand.warm();
    sig_stand.edit_and_diagnose(&sig_stand.module.with_parameter(EDITED, "Новый"));
    let (sig_execute, _) = sig_stand.churn("infer_method_query");

    // The method itself plus every caller: `execute >= 1 + callers`.
    assert!(
        sig_execute > callers,
        "a signature edit must re-infer the method and its {callers} caller(s): {sig_execute}"
    );
    assert!(
        sig_execute > body_execute,
        "the stand must tell a signature edit ({sig_execute}) from a body edit ({body_execute})"
    );
}

/// The method-keyed families that re-execute for every method an edit
/// reaches through the file and backdate: the syntax cut from a new parse,
/// and the declaration projected from a changed interface. Their equality is
/// what lets everything above them validate, so their count is not gated —
/// the families reading them are.
const BACKDATING_FAMILIES: &[&str] =
    &["method_syntax_query", "interface_method_query", "method_slab_query"];

/// The per-method families whose memo depends on the method's own text
/// alone — no declaration of the module, its own included — so a method
/// added or removed elsewhere in the file must leave every one of them
/// standing.
const TEXT_ONLY_FAMILIES: &[&str] = &[
    "method_lower_query",
    "method_body_query",
    "method_cfg_query",
    "reaching_definitions_query",
    "method_path_terminates_query",
    "method_hir_metrics_query",
    "method_cyclomatic_query",
    "method_sdbl_hir_query",
    "method_security_state_query",
    "method_line_diagnostics_query",
];

/// Every family keyed by a method, plus the diagnostics memo assembled from
/// them, which is keyed the same way but registered outside `ide-db`.
fn method_keyed_families() -> Vec<&'static str> {
    let mut families = ide_db::METHOD_KEYED_QUERY_FAMILIES.to_vec();
    families.push("method_diagnostics_query");
    families.push("method_line_diagnostics_query");
    families
}

/// Executions per family after `edit` on a fresh, warmed stand.
fn executions_after(
    edit: impl FnOnce(&SyntheticModule) -> String,
) -> Vec<(&'static str, u64, u64)> {
    let mut stand = Stand::new();
    stand.warm();
    let text = edit(&stand.module);
    stand.edit_and_diagnose(&text);
    method_keyed_families()
        .into_iter()
        .map(|family| {
            let (execute, validate) = stand.churn(family);
            (family, execute, validate)
        })
        .collect()
}

/// A method inserted in front of the first one costs exactly what the same
/// method inserted after the last one costs: the identity of the other
/// methods does not depend on their position. Insertion at the end is the
/// control — it moves nobody and nobody calls the new method, so every
/// per-method family executes for the new method alone: the others read
/// their own declaration, their callees' and the misses of their own names,
/// never the file's.
#[test]
fn method_insertion_above_costs_what_insertion_below_costs() {
    let above = executions_after(|m| m.with_method_inserted_before(0, "Новая", "\n"));
    let below = executions_after(|m| m.with_method_appended("Новая", "\n"));

    for ((family, above_execute, above_validate), (_, below_execute, _)) in above.iter().zip(&below)
    {
        assert_eq!(
            above_execute, below_execute,
            "{family}: insertion above must execute as much as insertion below"
        );
        if !BACKDATING_FAMILIES.contains(family) {
            assert!(
                *below_execute <= 1,
                "{family}: the control must execute for the new method only, observed {below_execute}"
            );
        }
        if *family == "method_lower_query" {
            assert_eq!(*below_execute, 1, "the new method must be lowered once");
            assert!(
                *above_validate >= (METHODS - 1) as u64,
                "the other methods must validate from cache above, observed {above_validate}"
            );
        }
    }
}

/// Removing the first method re-executes nothing for the survivors but its
/// callers: their keys are theirs, not their positions, and only the callers
/// read a declaration that is gone — their lookup of the name is a miss now.
#[test]
fn method_removal_above_reexecutes_only_the_callers_below() {
    let callers = synthetic_module().methods[0].callers as u64;
    assert!(callers >= 1, "the removed method needs a caller, or the gate cannot tell a miss");
    for (family, execute, validate) in executions_after(|m| m.with_method_removed(0)) {
        if BACKDATING_FAMILIES.contains(&family) {
            continue;
        }
        assert!(
            execute <= callers,
            "{family}: at most the {callers} caller(s) of the removed method may execute, observed {execute}"
        );
        if family == "infer_method_query" {
            assert_eq!(execute, callers, "the callers of the removed method resolve a miss now");
        }
        if TEXT_ONLY_FAMILIES.contains(&family) {
            assert_eq!(execute, 0, "{family}: no survivor's text changed");
        }
        if family == "method_lower_query" {
            assert!(
                validate >= (METHODS - 1) as u64,
                "survivors must validate, observed {validate}"
            );
        }
    }
}

/// A signature edit re-executes the edited method and its callers, and no
/// one else: the callers read the callee's declaration by name, the method
/// reads its own by id, and every other method reads neither.
#[test]
fn signature_edit_reexecutes_the_method_and_its_callers_only() {
    let mut stand = Stand::new();
    let callers = stand.module.methods[EDITED].callers as u64;
    assert!(callers >= 1, "the stand needs a caller, or `1 + callers` degenerates to 1");
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_parameter(EDITED, "Новый"));

    for family in ["infer_method_query", "method_diagnostics_query", "method_arg_diagnostics_query"]
    {
        let (execute, _) = stand.churn(family);
        assert_eq!(execute, 1 + callers, "{family}: the edited method and its {callers} caller(s)");
    }
    // The doc links of a method are resolved from its own declaration alone.
    let (execute, _) = stand.churn("doc_see_signature_query");
    assert_eq!(execute, 1, "doc_see_signature_query: the edited method only");
    // Every method reads its own declaration, and the interface is new, so the
    // projection re-executes at least once per method — and backdates, which
    // is what keeps the counts above at the edited method and its callers.
    let (execute, _) = stand.churn("interface_method_query");
    assert!(
        execute >= METHODS as u64,
        "interface_method_query: one re-execution per method at least, observed {execute}"
    );
    for family in method_keyed_families() {
        if BACKDATING_FAMILIES.contains(&family) {
            continue;
        }
        let (execute, _) = stand.churn(family);
        assert!(
            execute <= 1 + callers,
            "{family}: at most the edited method and its callers, observed {execute}"
        );
    }
}

/// A namesake declared above the edited method takes its place as the first
/// declaration of the name: the callers resolve to the new one, the old one
/// lives on under the next ordinal, and nobody else re-executes. Fewer
/// executions than that means a by-name lookup served a stale declaration.
#[test]
fn namesake_above_reexecutes_both_namesakes_and_the_callers() {
    let mut stand = Stand::new();
    let callers = stand.module.methods[EDITED].callers as u64;
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_duplicate_inserted_above(EDITED, "\n"));
    let (execute, _) = stand.churn("infer_method_query");
    assert_eq!(
        execute,
        2 + callers,
        "the namesake, the displaced method and its {callers} caller(s)"
    );
}

/// A module small enough for its by-name misses to share one set of names:
/// a signature edit still costs the method and its callers, while a method
/// added at the end re-infers the whole module — every body misses some name
/// — and nothing beyond it. The bound is the module, which is what keeps the
/// misses of the many small modules of a workspace from costing a memo each.
#[test]
fn small_module_misses_share_one_set_of_names() {
    const SMALL: usize = 40;
    const _: () = assert!(SMALL <= hir::NAME_SET_METHOD_LIMIT);
    let module =
        SyntheticModuleSpec { methods: SMALL, conditional_return_every: 1, ..Default::default() }
            .build();
    let mut stand = Stand::with_module(module);
    let callers = stand.module.methods[EDITED_SMALL].callers as u64;
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_parameter(EDITED_SMALL, "Новый"));
    let (execute, _) = stand.churn("infer_method_query");
    assert_eq!(execute, 1 + callers, "a signature edit: the method and its {callers} caller(s)");

    let mut stand = Stand::with_module(
        SyntheticModuleSpec { methods: SMALL, conditional_return_every: 1, ..Default::default() }
            .build(),
    );
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_method_appended("Новая", "\n"));
    let (execute, _) = stand.churn("infer_method_query");
    assert_eq!(execute, SMALL as u64 + 1, "a method added re-infers the module and only it");
    let (execute, _) = stand.churn("method_lower_query");
    assert_eq!(execute, 1, "the new method is lowered once, the others keep their text");
}

const EDITED_SMALL: usize = 21;

/// I1 at a scale the CI stand cannot afford: more methods than the retention
/// cap a per-method link used to have (2048) and than the target module of
/// github#113 (2349). A link whose memo is evicted has no old value to
/// backdate against, so the inference above it re-runs — invisible below the
/// cap, which is why the small stand passed while ERP did not.
#[test]
#[ignore = "scale stand (~2400 methods): run on demand with --ignored"]
fn body_edit_stays_local_beyond_the_old_retention_cap() {
    const METHODS: usize = 2400;
    let module = SyntheticModuleSpec {
        methods: METHODS,
        call_next_every: 1,
        conditional_return_every: 1,
        ..Default::default()
    }
    .build();
    let mut db = RootDatabaseImpl::new_with_salsa_events();
    let mut file_set = FileSet::default();
    let file_id = FileId(0);
    file_set.insert(file_id, VfsPath::new("/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, &module.text);
    let mut stand = Stand { db, file_id, module };
    stand.warm();
    stand.edit_and_diagnose(&stand.module.with_body_statement(METHODS / 2 + 1, "Правка", "\n"));
    for family in PER_METHOD_FAMILIES {
        let (execute, validate) = stand.churn(family);
        assert_eq!(execute, 1, "{family}: only the edited method may execute");
        let expected = other_readers(family, METHODS);
        assert!(validate >= expected, "{family}: expected {expected} validations, got {validate}");
    }
}
