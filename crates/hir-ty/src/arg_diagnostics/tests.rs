use bsl_types::builders::Builders;
use bsl_types::kind::TypeId;
use bsl_types::testing::InMemoryDb;
use hir_def::execution_env::EnvFlags;
use hir_def::{DefWithBodyId, ExprId};

use super::diagnostic_for_binding;
use crate::call_resolution::{
    resolve_candidates, BuiltinCallableId, CallCandidateSet, CallParam, CallParamMode,
    CallSignature, CandidateId, CandidateOrigin, CandidateProvenance,
};
use crate::infer::{CallArgBinding, CandidateCallBinding, InferenceDiagnostic};

fn expr(raw: u32) -> ExprId {
    ExprId::from_raw(la_arena::RawIdx::from_u32(raw))
}

fn candidates(db: &InMemoryDb, parameter_sets: &[&[TypeId]]) -> CallCandidateSet {
    CallCandidateSet::try_from(
        parameter_sets
            .iter()
            .enumerate()
            .map(|(signature_ordinal, parameter_types)| {
                let callable = BuiltinCallableId::Intrinsic(100);
                CallSignature {
                    id: CandidateId::Builtin { callable, signature_ordinal },
                    params: parameter_types
                        .iter()
                        .copied()
                        .enumerate()
                        .map(|(index, ty)| CallParam {
                            name: format!("p{index}").into(),
                            ty,
                            has_default: false,
                            mode: CallParamMode::Positional,
                        })
                        .collect(),
                    required_args: parameter_types.len(),
                    max_args: Some(parameter_types.len()),
                    return_ty: db.unknown(),
                    origin: CandidateOrigin::Builtin,
                    environment: EnvFlags::EMPTY,
                    provenance: CandidateProvenance::Builtin(callable),
                    from_doc_comment: false,
                }
            })
            .collect::<Vec<_>>(),
    )
    .expect("test candidates must have unique identities")
}

fn binding(db: &InMemoryDb, parameter_sets: &[&[TypeId]], pre_types: &[TypeId]) -> CallArgBinding {
    let candidates = candidates(db, parameter_sets);
    let resolution = resolve_candidates(db, &candidates, pre_types);
    CallArgBinding {
        owner: DefWithBodyId::ModuleCode,
        call_expr: expr(0),
        args: (0..pre_types.len()).map(|index| expr(index as u32 + 1)).collect(),
        candidate: CandidateCallBinding { candidates, resolution },
        in_dead_branch: false,
    }
}

#[test]
fn t12_post_narrowing_applicable_survivor_suppresses_mismatch() {
    let db = InMemoryDb::new();
    let number = db.number(None, None);
    let string = db.string(None, false);
    let binding = binding(&db, &[&[number]], &[string]);

    assert_eq!(diagnostic_for_binding(&db, &binding, &[number], &mut |_| false), None);
}

#[test]
fn t12_post_narrowing_indeterminate_survivor_suppresses_mismatch() {
    let db = InMemoryDb::new();
    let number = db.number(None, None);
    let string = db.string(None, false);
    let binding = binding(&db, &[&[number]], &[string]);

    assert_eq!(diagnostic_for_binding(&db, &binding, &[db.unknown()], &mut |_| false), None);
}

#[test]
fn t12_type_rejection_uses_best_candidate_and_first_incompatible_argument() {
    let db = InMemoryDb::new();
    let number = db.number(None, None);
    let string = db.string(None, false);
    let boolean = db.boolean();
    let date = db.date(bsl_types::facet::DateComponent::DateTime);
    let binding = binding(
        &db,
        &[&[boolean, boolean], &[number, date], &[number, boolean]],
        &[number, string],
    );

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[number, string], &mut |_| false),
        Some(InferenceDiagnostic::TypeMismatch {
            expr: expr(2),
            expected: date,
            actual: string,
            from_doc_comment: false,
        })
    );
}

#[test]
fn t12_arity_rejection_uses_resolver_fallback_once() {
    let db = InMemoryDb::new();
    let number = db.number(None, None);
    let binding = binding(&db, &[&[number, number], &[number, number, number]], &[number]);

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[number], &mut |_| false),
        Some(InferenceDiagnostic::MismatchedArgCount {
            call_expr: expr(0),
            required_count: 2,
            total_count: 2,
            found: 1,
        })
    );
}

#[test]
fn documented_union_with_a_fitting_member_is_relieved() {
    let db = InMemoryDb::new();
    let structure = db.structure(None);
    let maybe_structure = db.union(vec![structure, db.undefined()]);
    let binding = binding(&db, &[&[structure]], &[maybe_structure]);

    assert_eq!(diagnostic_for_binding(&db, &binding, &[maybe_structure], &mut |_| true), None);
}

#[test]
fn computed_union_with_a_fitting_member_is_still_rejected() {
    let db = InMemoryDb::new();
    let structure = db.structure(None);
    let maybe_structure = db.union(vec![structure, db.undefined()]);
    let binding = binding(&db, &[&[structure]], &[maybe_structure]);

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[maybe_structure], &mut |_| false),
        Some(InferenceDiagnostic::TypeMismatch {
            expr: expr(1),
            expected: structure,
            actual: maybe_structure,
            from_doc_comment: false,
        })
    );
}

#[test]
fn documented_union_without_a_fitting_member_is_still_rejected() {
    let db = InMemoryDb::new();
    let structure = db.structure(None);
    let maybe_number = db.union(vec![db.number(None, None), db.undefined()]);
    let binding = binding(&db, &[&[structure]], &[maybe_number]);

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[maybe_number], &mut |_| true),
        Some(InferenceDiagnostic::TypeMismatch {
            expr: expr(1),
            expected: structure,
            actual: maybe_number,
            from_doc_comment: false,
        })
    );
}

#[test]
fn documented_value_of_a_foreign_type_is_still_rejected() {
    let db = InMemoryDb::new();
    let structure = db.structure(None);
    let number = db.number(None, None);
    let binding = binding(&db, &[&[structure]], &[number]);

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[number], &mut |_| true),
        Some(InferenceDiagnostic::TypeMismatch {
            expr: expr(1),
            expected: structure,
            actual: number,
            from_doc_comment: false,
        })
    );
}

#[test]
fn every_incompatible_argument_must_be_relieved() {
    let db = InMemoryDb::new();
    let structure = db.structure(None);
    let number = db.number(None, None);
    let maybe_structure = db.union(vec![structure, db.undefined()]);
    let string = db.string(None, false);
    let binding = binding(&db, &[&[structure, number]], &[maybe_structure, string]);

    assert_eq!(
        diagnostic_for_binding(&db, &binding, &[maybe_structure, string], &mut |_| true),
        Some(InferenceDiagnostic::TypeMismatch {
            expr: expr(1),
            expected: structure,
            actual: maybe_structure,
            from_doc_comment: false,
        })
    );
}
