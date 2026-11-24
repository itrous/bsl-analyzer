use bsl_types::kind::TypeId;
use hir_def::MethodId;

use crate::call_resolution::{CallCandidateSet, CallSignature, DuplicateCandidateId};
use crate::db::HirDatabase;

pub(crate) fn for_resolved_method(
    db: &dyn HirDatabase,
    primary_method: MethodId,
    primary_return: TypeId,
) -> Result<CallCandidateSet, DuplicateCandidateId> {
    let options = db.env_options_for_file(primary_method.module.file_id);
    let candidates: Vec<CallSignature> = db
        .interface_method(primary_method)
        .into_iter()
        .map(|method| {
            let signature =
                crate::method_resolution::materialise_signature_enriched(db, method.id, &method);
            let environment =
                crate::method_environment::effective_method_env(db, method.id, &options);
            CallSignature::from_user_method(&method, &signature, primary_return, environment)
        })
        .collect();
    CallCandidateSet::try_from(candidates)
}
