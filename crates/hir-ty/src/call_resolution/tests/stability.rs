#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn applicability_is_stable_across_aliases_and_recomputation() {
    use super::evaluate_applicability;

    // Given bilingual aliases and independent type-kernel databases.
    let first_db = InMemoryDb::new();
    let russian = select_candidates(&first_db, "Выбрать");
    let english = select_candidates(&first_db, "Select");
    let first_argument = first_db.structure(None);
    let russian_evaluations = russian
        .candidates
        .as_slice()
        .iter()
        .map(|candidate| evaluate_applicability(&first_db, candidate, &[first_argument]))
        .collect::<Vec<_>>();
    let english_evaluations = english
        .candidates
        .as_slice()
        .iter()
        .map(|candidate| evaluate_applicability(&first_db, candidate, &[first_argument]))
        .collect::<Vec<_>>();

    // When the same call is recomputed from a fresh database.
    let fresh_db = InMemoryDb::new();
    let recomputed = select_candidates(&fresh_db, "Select");
    let fresh_argument = fresh_db.structure(None);
    let recomputed_evaluations = recomputed
        .candidates
        .as_slice()
        .iter()
        .map(|candidate| evaluate_applicability(&fresh_db, candidate, &[fresh_argument]))
        .collect::<Vec<_>>();

    // Then candidate applicability evidence is stable across aliases and recomputation.
    assert_eq!(russian_evaluations, english_evaluations);
    assert_eq!(english_evaluations, recomputed_evaluations);
}
