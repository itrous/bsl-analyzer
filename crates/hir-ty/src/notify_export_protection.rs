use hir_def::call_graph::NotifyReg;

use crate::notify_targets::{NotifyCandidate, NotifyCandidateReceiver};

/// Semantic projection used by the command-module export diagnostic.
#[derive(Debug, Clone, Copy)]
pub struct CommandModuleExportVerdict<'a> {
    /// A reachable candidate could not be fully analysed, so no export can be classified.
    pub global_uncertainty: bool,
    /// A resolved local registration, retained even when its site is unreachable.
    pub local_registration: Option<&'a NotifyReg>,
    /// Whether this candidate may refer to a local method whose export must be preserved.
    pub protects_export: bool,
}

/// Projects a notification candidate into the facts needed by the command-module
/// export diagnostic, keeping receiver, reachability and incompleteness rules semantic.
pub fn command_module_export_verdict(
    candidate: &NotifyCandidate,
) -> CommandModuleExportVerdict<'_> {
    let global_uncertainty = candidate.site_reachable && candidate.analysis_incomplete;
    let local_registration = (candidate.receiver == NotifyCandidateReceiver::ThisObject)
        .then_some(candidate.registration.as_ref())
        .flatten();
    let protects_export = candidate.site_reachable
        && !candidate.analysis_incomplete
        && candidate.protects_local_method
        && (candidate.registration.is_none()
            || matches!(
                candidate.receiver,
                NotifyCandidateReceiver::Unknown | NotifyCandidateReceiver::LocalBinding
            ));

    CommandModuleExportVerdict { global_uncertainty, local_registration, protects_export }
}
