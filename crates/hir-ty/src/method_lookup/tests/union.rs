use super::*;

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn union_lookup_merges_all_arm_signatures() {
    let db = InMemoryDb::new();
    let recv = db.union(vec![db.array(None), db.structure(None)]);
    let info = lookup(&db, recv, &Name::new("Вставить"))
        .expect("Вставить must resolve on a Массив | Структура union");

    // Every arm's signature must be present so an argument accepted by EITHER
    // arm is not reported as a mismatch (a union receiver is an
    // over-approximation). Массив.Вставить wants a numeric index;
    // Структура.Вставить wants a string key.
    let first_accepts = |arg: TypeId| {
        info.candidates.as_slice().iter().any(|candidate| {
            candidate
                .params
                .first()
                .is_some_and(|param| crate::subtype::is_coercible_to(&db, arg, param.ty))
        })
    };
    assert!(
        first_accepts(db.string(None, false)),
        "Структура.Вставить arm must accept a String key, got candidates {:?}",
        info.candidates
    );
    assert!(
        first_accepts(db.number(None, None)),
        "Массив.Вставить arm must accept a numeric index, got candidates {:?}",
        info.candidates
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn union_lookup_preserves_distinct_arm_candidate_ids_and_parameters() {
    let db = InMemoryDb::new();
    let receiver = db.union(vec![db.array(None), db.structure(None)]);
    let info = lookup(&db, receiver, &Name::new("Вставить"))
        .expect("Array | Structure Insert must resolve through both arms");
    let candidates = info.candidates.as_slice();

    assert_eq!(
        candidates.iter().map(|candidate| candidate.id).collect::<Vec<_>>(),
        vec![
            crate::call_resolution::CandidateId::Platform {
                method_id: 1755,
                signature: crate::call_resolution::PlatformSignatureSlot::Base,
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 1787,
                signature: crate::call_resolution::PlatformSignatureSlot::Base,
            },
        ],
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.params.iter().map(|param| param.ty).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        vec![vec![db.number(None, None), db.any()], vec![db.string(None, false), db.any()],],
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn union_lookup_deduplicates_candidates_by_stable_id() {
    let db = InMemoryDb::new();
    let untyped = db.array(None);
    let platform_array = platform_id(&db, "Array");
    let direct = lookup(&db, untyped, &Name::new("Добавить")).expect("Array.Add must resolve");
    let union = lookup(&db, db.union(vec![untyped, platform_array]), &Name::new("Добавить"))
        .expect("Array union Add must resolve");

    let direct_ids =
        direct.candidates.as_slice().iter().map(|candidate| candidate.id).collect::<Vec<_>>();
    let union_ids =
        union.candidates.as_slice().iter().map(|candidate| candidate.id).collect::<Vec<_>>();

    assert_eq!(union_ids, direct_ids);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn union_lookup_preserves_complete_signatures_without_cross_product() {
    let db = InMemoryDb::new();
    let receiver = db.union(vec![
        platform_id(&db, "СертификатКриптографии"),
        platform_id(&db, "КонтейнерКлючейКриптографии"),
    ]);
    let info = lookup(&db, receiver, &Name::new("Выгрузить"))
        .expect("both receiver arms must expose Unload");
    let candidates = info.candidates;

    assert_eq!(
        candidates.as_slice().iter().map(|candidate| candidate.id).collect::<Vec<_>>(),
        vec![
            crate::call_resolution::CandidateId::Platform {
                method_id: 4008,
                signature: crate::call_resolution::PlatformSignatureSlot::Base,
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4008,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(0),
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4008,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(1),
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4008,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(2),
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4034,
                signature: crate::call_resolution::PlatformSignatureSlot::Base,
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4034,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(0),
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4034,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(1),
            },
            crate::call_resolution::CandidateId::Platform {
                method_id: 4034,
                signature: crate::call_resolution::PlatformSignatureSlot::Variant(2),
            },
        ],
    );

    let arguments = [db.string(None, false), db.string(None, false)];
    let resolution = crate::call_resolution::resolve_candidates(&db, &candidates, &arguments);
    assert!(
        matches!(resolution.selection, crate::call_resolution::CallSelection::Rejected(_)),
        "mixed signatures must reject cross-product arguments: {resolution:?}"
    );
}

#[test]
fn method_lookup_returns_none_for_union_without_live_method() {
    let db = InMemoryDb::new();
    let receiver = db.union(vec![db.number(None, None), db.string(None, false)]);
    assert!(lookup(&db, receiver, &Name::new("Любой")).is_none());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn method_lookup_union_narrows_past_undefined_sentinel() {
    let db = InMemoryDb::new();
    let receiver = db.union(vec![platform_id(&db, "РезультатЗапроса"), db.undefined()]);
    let info = lookup(&db, receiver, &Name::new("Выгрузить"))
        .expect("Union([QueryResult, Undefined]).Выгрузить must resolve through the live branch");
    let contains_value_table = match db.lookup_type(info.return_ty) {
        TypeKind::ValueTable(_) => true,
        TypeKind::Union(members) => {
            members.iter().any(|id| matches!(db.lookup_type(*id), TypeKind::ValueTable(_)))
        }
        _ => false,
    };
    assert!(
        contains_value_table,
        "return type must include ValueTable, got {:?}",
        db.lookup_type(info.return_ty),
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn method_lookup_union_resolution_has_no_first_arm_winner() {
    let db = InMemoryDb::new();
    let receiver = db.union(vec![platform_id(&db, "ValueTable"), platform_id(&db, "ValueTree")]);
    let info = lookup(&db, receiver, &Name::new("ВыбратьСтроку"))
        .expect("both receiver arms must expose ChooseRow");
    let candidates = info.candidates;
    assert_eq!(candidates.as_slice().len(), 2);
    let candidate_returns =
        candidates.as_slice().iter().map(|candidate| candidate.return_ty).collect::<Vec<_>>();
    assert_ne!(candidate_returns[0], candidate_returns[1]);

    let resolution = crate::call_resolution::resolve_candidates(&db, &candidates, &[]);
    assert_eq!(resolution.return_ty, info.return_ty);
    assert_ne!(resolution.return_ty, candidate_returns[0]);
    assert_ne!(resolution.return_ty, candidate_returns[1]);
}
