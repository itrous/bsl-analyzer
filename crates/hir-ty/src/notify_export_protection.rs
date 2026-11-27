use std::sync::Arc;

use hir_def::call_graph::NotifyReg;
use hir_def::{normalize_match_name, MethodId, Name, SymbolTree};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::db::HirDatabase;
use crate::method_environment::notify_methods_exported_in_registrations;
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

/// Semantic protection for command-module exports.
#[derive(Debug, Default)]
pub struct CommandModuleExportProtection {
    protected_names: FxHashSet<Name>,
    protected_methods: FxHashSet<MethodId>,
}

impl CommandModuleExportProtection {
    /// Whether removing this declaration's export might break a callback.
    pub fn protects(&self, name: &Name, method: MethodId) -> bool {
        self.protected_names.contains(&normalize_match_name(name))
            || self.protected_methods.contains(&method)
    }
}

/// Returns protection for command-module exports, or `None` when platform facts or an
/// incomplete reachable registration make classification unsafe. The inputs are lazy
/// so missing platform help does not trigger callback analysis.
pub fn command_module_export_protection(
    db: &dyn HirDatabase,
    candidates: impl FnOnce() -> Arc<Vec<NotifyCandidate>>,
    symbols: impl FnOnce() -> Arc<SymbolTree>,
) -> Option<CommandModuleExportProtection> {
    bsl_platform::PlatformData::instance().help_origin()?;

    let candidates = candidates();
    let mut by_name: FxHashMap<Name, Vec<&NotifyCandidate>> = FxHashMap::default();
    for (index, candidate) in candidates.iter().enumerate() {
        if index & 0xff == 0 {
            db.unwind_if_revision_cancelled();
        }
        if command_module_export_verdict(candidate).global_uncertainty {
            return None;
        }
        by_name.entry(normalize_match_name(&candidate.callback_name)).or_default().push(candidate);
    }

    let symbols = symbols();
    let mut protected = CommandModuleExportProtection::default();
    for (index, (name, matching)) in by_name.into_iter().enumerate() {
        if index & 0xff == 0 {
            db.unwind_if_revision_cancelled();
        }
        let methods =
            symbols.find_methods(&name).into_iter().map(|method| method.id).collect::<Vec<_>>();
        if methods.is_empty() {
            continue;
        }
        let mut registrations = Vec::new();
        let mut syntax_protected = false;
        for (index, candidate) in matching.into_iter().enumerate() {
            if index & 0xff == 0 {
                db.unwind_if_revision_cancelled();
            }
            let verdict = command_module_export_verdict(candidate);
            if let Some(registration) = verdict.local_registration {
                registrations.push(registration);
            }
            syntax_protected |= verdict.protects_export;
        }
        if syntax_protected {
            protected.protected_names.insert(name);
        } else {
            protected.protected_methods.extend(notify_methods_exported_in_registrations(
                db,
                &methods,
                &registrations,
            ));
        }
    }
    Some(protected)
}
