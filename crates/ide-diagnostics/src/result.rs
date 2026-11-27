use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use serde::Serialize;
use std::ops::Deref;

/// Why a file's diagnostics cannot be treated as a complete list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsIncompleteReason {
    /// The semantic projection of notification registrations hit its work limit.
    NotifyProjection,
}

/// Whether all enabled diagnostic rules contributed to the file's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiagnosticsCompleteness {
    Complete,
    Incomplete { reason: DiagnosticsIncompleteReason },
}

impl DiagnosticsCompleteness {
    pub fn incomplete_reason(self) -> Option<DiagnosticsIncompleteReason> {
        match self {
            Self::Complete => None,
            Self::Incomplete { reason } => Some(reason),
        }
    }
}

/// The notify projection contributes to these enabled diagnostic rules.
const NOTIFY_PROJECTION_CONSUMERS: [DiagnosticCode; 4] = [
    DiagnosticCode::MissingNotifyHandler,
    DiagnosticCode::UnusedParameters,
    DiagnosticCode::UnusedLocalMethod,
    DiagnosticCode::CommandModuleExportMethods,
];

/// Determine completeness lazily so disabled projection consumers do not trigger
/// the semantic summary query.
pub(crate) fn notify_projection_completeness(
    config: &DiagnosticsConfig,
    notify_projection_incomplete: impl FnOnce() -> bool,
) -> DiagnosticsCompleteness {
    if config.any_enabled(&NOTIFY_PROJECTION_CONSUMERS) && notify_projection_incomplete() {
        DiagnosticsCompleteness::Incomplete {
            reason: DiagnosticsIncompleteReason::NotifyProjection,
        }
    } else {
        DiagnosticsCompleteness::Complete
    }
}

/// Final diagnostic findings together with the completeness of their analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsResult {
    pub diagnostics: Vec<Diagnostic>,
    pub completeness: DiagnosticsCompleteness,
}

impl Deref for DiagnosticsResult {
    type Target = [Diagnostic];

    fn deref(&self) -> &Self::Target {
        &self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_results_keep_their_completeness() {
        let complete = DiagnosticsResult {
            diagnostics: Vec::new(),
            completeness: DiagnosticsCompleteness::Complete,
        };
        let incomplete = DiagnosticsResult {
            diagnostics: Vec::new(),
            completeness: DiagnosticsCompleteness::Incomplete {
                reason: DiagnosticsIncompleteReason::NotifyProjection,
            },
        };

        assert!(complete.is_empty());
        assert_eq!(complete.completeness, DiagnosticsCompleteness::Complete);
        assert!(incomplete.is_empty());
        assert_eq!(
            incomplete.completeness.incomplete_reason(),
            Some(DiagnosticsIncompleteReason::NotifyProjection)
        );
    }

    #[test]
    fn each_notify_projection_consumer_independently_marks_completeness() {
        for code in NOTIFY_PROJECTION_CONSUMERS {
            let config = DiagnosticsConfig { only_enabled: Some(vec![code]), ..Default::default() };
            let completeness = notify_projection_completeness(&config, || true);
            assert_eq!(
                completeness.incomplete_reason(),
                Some(DiagnosticsIncompleteReason::NotifyProjection),
                "{code:?} consumes the projection independently",
            );
        }
    }

    #[test]
    fn all_notify_projection_consumers_disabled_skip_the_semantic_check() {
        let config = DiagnosticsConfig {
            disabled: NOTIFY_PROJECTION_CONSUMERS.to_vec(),
            ..Default::default()
        };
        let mut queried = false;
        let completeness = notify_projection_completeness(&config, || {
            queried = true;
            true
        });

        assert_eq!(completeness, DiagnosticsCompleteness::Complete);
        assert!(!queried, "no disabled consumer should trigger the summary query");
    }
}
