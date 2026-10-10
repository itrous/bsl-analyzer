use bsl_metadata::MdoType;
use bsl_platform::{MethodParam, PlatformMethod};
use bsl_types::builders::Builders;
use bsl_types::testing::InMemoryDb;
use hir_def::execution_env::EnvFlags;
use hir_def::Name;

use super::{
    BuiltinCallableId, CallCandidateSet, CallParamMode, CandidateId, CandidateOrigin,
    CandidateProvenance, DuplicateCandidateId, PlatformSignatureSlot, UserMethodId,
};
use crate::platform_manager_lookup::resolve_platform_manager_method;

mod resolution_adversarial;
mod resolution_cases;
mod support;

fn select_candidates(
    db: &InMemoryDb,
    method_name: &str,
) -> crate::platform_manager_lookup::PlatformMethodResolution {
    resolve_platform_manager_method(
        db,
        MdoType::InformationRegister,
        &Name::new("Курсы"),
        &Name::new(method_name),
    )
    .expect("InformationRegisterManager.Select must resolve")
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn candidate_shape() {
    let db = InMemoryDb::new();
    let russian = select_candidates(&db, "Выбрать");
    let english = select_candidates(&db, "Select");
    let fresh_db = InMemoryDb::new();
    let recomputed = select_candidates(&fresh_db, "Select");

    assert_eq!(russian.candidates, english.candidates);
    assert_eq!(english.candidates, recomputed.candidates);

    let candidates = russian.candidates.as_slice();
    assert_eq!(
        candidates.iter().map(|candidate| candidate.id).collect::<Vec<_>>(),
        vec![
            CandidateId::Platform { method_id: 174, signature: PlatformSignatureSlot::Base },
            CandidateId::Platform { method_id: 174, signature: PlatformSignatureSlot::Variant(0) },
            CandidateId::Platform { method_id: 174, signature: PlatformSignatureSlot::Variant(1) },
        ]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| (candidate.required_args, candidate.max_args))
            .collect::<Vec<_>>(),
        vec![(0, Some(6)), (0, Some(4)), (0, Some(2))]
    );
    assert!(candidates.iter().all(|candidate| {
        candidate
            .params
            .iter()
            .all(|param| param.has_default && param.mode == CallParamMode::Positional)
            && candidate.variadic_param().is_none()
            && candidate.return_ty == russian.return_ty
            && candidate.origin == CandidateOrigin::Platform
            && candidate.environment
                == (EnvFlags::THICK_CLIENT_MANAGED
                    | EnvFlags::THICK_CLIENT_ORDINARY
                    | EnvFlags::SERVER
                    | EnvFlags::EXTERNAL_CONNECTION)
            && !candidate.from_doc_comment
    }));
    assert_eq!(candidates[2].params[0].name.as_str(), "Отбор");
    assert_eq!(candidates[2].params[0].ty, db.structure(None));
    assert_eq!(
        candidates[2].provenance,
        CandidateProvenance::PlatformMethod {
            method_id: 174,
            signature: PlatformSignatureSlot::Variant(1),
        }
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn candidate_identity_rejects_duplicates() {
    let db = InMemoryDb::new();
    let resolution = select_candidates(&db, "Select");
    let mut duplicated = resolution.candidates.as_slice().to_vec();
    duplicated.insert(0, duplicated[2].clone());

    assert_eq!(
        CallCandidateSet::try_from(duplicated),
        Err(DuplicateCandidateId {
            id: CandidateId::Platform {
                method_id: 174,
                signature: PlatformSignatureSlot::Variant(1),
            },
        })
    );

    let user = CandidateId::User {
        method: UserMethodId::new(vfs::FileId::from_raw(7), hir_def::MethodKey::first("М11")),
        signature_ordinal: 0,
    };
    let builtin =
        CandidateId::Builtin { callable: BuiltinCallableId::Intrinsic(3), signature_ordinal: 0 };
    assert_ne!(user, builtin);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn checked_candidate_set_rejects_duplicates_in_release() {
    let db = InMemoryDb::new();
    let resolution = select_candidates(&db, "Select");
    let duplicate = resolution.candidates.as_slice()[0].clone();
    let duplicate_id = duplicate.id;

    assert_eq!(
        CallCandidateSet::try_from(vec![duplicate.clone(), duplicate]),
        Err(DuplicateCandidateId { id: duplicate_id })
    );
}

#[test]
fn platform_candidate_preserves_incomplete_variadic_metadata() {
    let db = InMemoryDb::new();
    let method = PlatformMethod {
        id: 9,
        type_name: "SyntheticManager".into(),
        name: "Вызвать".into(),
        english_name: "Call".into(),
        return_type: None,
        parameters: vec![
            MethodParam {
                name: "Обязательный".into(),
                param_type: Some("Число".into()),
                is_optional: false,
                is_variadic: false,
            },
            MethodParam {
                name: "Остальные".into(),
                param_type: None,
                is_optional: false,
                is_variadic: true,
            },
        ],
        variants: Vec::new(),
        min_version: None,
        context: None,
    };

    let candidates = CallCandidateSet::from_platform_method(&db, &method, db.undefined());
    let candidate = &candidates.as_slice()[0];

    assert_eq!(candidate.required_args, 1);
    assert_eq!(candidate.max_args, None);
    assert_eq!(candidate.variadic_param().map(|param| param.ty), Some(db.unknown()));
    assert_eq!(candidate.params[1].mode, CallParamMode::Variadic);
}

include!("tests/applicability.rs");
include!("tests/arity.rs");
include!("tests/rejections.rs");
include!("tests/resolution.rs");
include!("tests/stability.rs");
