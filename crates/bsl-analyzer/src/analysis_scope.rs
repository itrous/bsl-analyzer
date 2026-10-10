//! Lifecycle of the vendor-diff analysis scope in the LSP server.
//!
//! When `[analysis].diff_base` is configured, the scope is computed in the
//! background (a workdir git diff can take seconds on a large configuration)
//! and attached to the diagnostics config input, so every consumer — push
//! publishing, pull reports, the workspace batch — filters identically and
//! salsa re-keys its cache on every scope replacement.
//!
//! Rebuild triggers: workspace/config (re)load, a document save, an external
//! change batch (e.g. `git pull` / branch checkout). Requests coalesce: at
//! most one build is in flight, and a request arriving meanwhile re-runs once
//! it finishes. A failed build fails open — analysis continues unfiltered —
//! with a one-shot client warning, because a silently empty result set would
//! be indistinguishable from "no findings".

use std::sync::Arc;

use base_db::AnalysisScope;

use crate::global_state::{GlobalState, Task};

#[derive(Debug, Default)]
pub enum ScopeState {
    /// No `[analysis].diff_base` configured: no filtering.
    #[default]
    Disabled,
    /// A build is in flight; only its own generation may publish a result.
    Loading {
        generation: u64,
    },
    Ready {
        generation: u64,
        scope: Arc<AnalysisScope>,
    },
    /// The configured base could not be resolved: analyzing without a filter.
    Failed {
        generation: u64,
    },
}

impl ScopeState {
    pub fn current(&self) -> Option<Arc<AnalysisScope>> {
        match self {
            ScopeState::Ready { scope, .. } => Some(scope.clone()),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, ScopeState::Loading { .. })
    }
}

impl GlobalState {
    fn scope_diff_base(&self) -> Option<String> {
        self.project.as_ref()?.config.analysis.diff_base.clone()
    }

    /// Ask for a (re)build of the analysis scope. Cheap and coalescing — the
    /// actual work starts in [`Self::maybe_spawn_scope_build`].
    pub fn request_scope_rebuild(&mut self) {
        if self.scope_diff_base().is_some() || !matches!(self.analysis_scope, ScopeState::Disabled)
        {
            self.scope_build_queued = true;
        }
    }

    /// Launch the queued scope build unless one is already in flight or the
    /// pool is saturated. Called from the event-loop bottom (retried on every
    /// wake, like pending diagnostics) and after a finished build applies.
    pub fn maybe_spawn_scope_build(&mut self) {
        if !self.scope_build_queued || self.analysis_scope.is_loading() {
            return;
        }

        let Some(base) = self.scope_diff_base() else {
            // The base was removed from the config: drop the filter.
            self.scope_build_queued = false;
            if !matches!(self.analysis_scope, ScopeState::Disabled) {
                self.analysis_scope = ScopeState::Disabled;
                self.apply_scope_to_config();
                self.rescope_published_diagnostics();
            }
            return;
        };
        let Some(root) = self.workspace_root.clone() else {
            self.scope_build_queued = false;
            return;
        };
        if !self.task_pool.pool.has_capacity() {
            return;
        }

        self.scope_generation = self.scope_generation.wrapping_add(1);
        let generation = self.scope_generation;
        tracing::info!(base = %base, generation, "building analysis scope");

        let spawned = self.task_pool.pool.try_spawn(move || {
            let started = std::time::Instant::now();
            // A panic inside the git computation must still produce a task:
            // a swallowed unwind would leave the state `Loading` — and the
            // workspace batch deferred — forever.
            let computed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let identity = vcs::scope_ref_identity(&root, &base).ok();
                let result = vcs::generate_workdir_diff_report(&root, &base, true)
                    .map(|diff| {
                        // Anchor realpath'd git keys onto the workspace-root
                        // spelling the VFS uses (the root may be a symlink).
                        Arc::new(AnalysisScope::from_report_anchored(
                            diff.report.base_ref,
                            &diff.workdir,
                            &root,
                            diff.report
                                .files
                                .into_iter()
                                .map(|(path, change)| (path, change.hunks)),
                        ))
                    })
                    .map_err(|e| e.to_string());
                (result, identity)
            }));
            let (result, identity) = computed.unwrap_or_else(|panic| {
                let detail = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic during scope build".to_string());
                (Err(detail), None)
            });
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                generation,
                "analysis scope build finished"
            );
            Task::AnalysisScopeReady { generation, result, identity }
        });

        match spawned {
            Ok(()) => {
                self.scope_build_queued = false;
                self.analysis_scope = ScopeState::Loading { generation };
            }
            Err(_) => {
                // Capacity raced away; the loop bottom retries. The bumped
                // generation is harmless — no task will ever carry it.
            }
        }
    }

    /// Apply a finished scope build. Stale generations (superseded by a newer
    /// request) are dropped without touching the published state.
    pub fn handle_analysis_scope_ready(
        &mut self,
        generation: u64,
        result: Result<Arc<AnalysisScope>, String>,
        identity: Option<(String, String)>,
    ) {
        let ScopeState::Loading { generation: current } = self.analysis_scope else {
            tracing::debug!(generation, "dropping scope result: no build in flight");
            return;
        };
        if generation != current {
            tracing::debug!(generation, current, "dropping stale scope result");
            return;
        }

        // A rebuild was requested while this one ran: its input is already
        // obsolete. Discard it without publishing (no churn on every save
        // burst) and start the fresh build instead.
        if self.scope_build_queued {
            tracing::debug!(generation, "discarding obsolete scope result; a rebuild is queued");
            self.analysis_scope = ScopeState::Failed { generation };
            self.maybe_spawn_scope_build();
            return;
        }

        self.scope_ref_identity = identity;
        match result {
            Ok(scope) => {
                tracing::info!(
                    base = scope.base_ref(),
                    files_in_scope = scope.in_scope_file_count(),
                    "analysis scope ready"
                );
                self.scope_warning_shown = None;
                self.analysis_scope = ScopeState::Ready { generation, scope };
            }
            Err(error) => {
                tracing::warn!(%error, "analysis scope build failed; analyzing without the filter");
                self.show_scope_warning(&error);
                self.analysis_scope = ScopeState::Failed { generation };
            }
        }

        self.apply_scope_to_config();
        self.rescope_published_diagnostics();
        // A rebuild requested while this one ran starts now.
        self.maybe_spawn_scope_build();
    }

    /// Idle-tick check for ref-only movement: a fetch/rebase/reset can move the
    /// base or `HEAD` without touching any watched file, leaving the scope
    /// stale with no event to notice it. Comparing resolved OIDs is a few
    /// milliseconds, so the 60s idle cadence is essentially free.
    pub fn check_scope_ref_drift(&mut self) {
        if self.scope_diff_base().is_none() || self.analysis_scope.is_loading() {
            return;
        }
        let (Some(base), Some(root)) = (self.scope_diff_base(), self.workspace_root.clone()) else {
            return;
        };
        let Ok(identity) = vcs::scope_ref_identity(&root, &base) else {
            return;
        };
        if self.scope_ref_identity.as_ref().is_some_and(|stored| stored != &identity) {
            tracing::info!("scope refs moved without file events; rebuilding analysis scope");
            self.request_scope_rebuild();
            self.maybe_spawn_scope_build();
        }
    }

    /// Attach the current scope to the diagnostics config input. Must run after
    /// every rebuild of the input (config reload) and every scope transition.
    pub(crate) fn apply_scope_to_config(&mut self) {
        self.diagnostics_config.scope = self.analysis_scope.current();
    }

    /// Re-drive every diagnostics surface after the scope changed: open
    /// documents recompute, the workspace batch re-sweeps (clearing pushes for
    /// files that left the scope), and a pull client is asked to re-pull.
    fn rescope_published_diagnostics(&mut self) {
        for uri in self.opened_document_uris() {
            self.enqueue_pending_diagnostics(uri);
        }
        self.reset_workspace_batch();
        self.mark_workspace_batch_dirty();
        self.request_workspace_diagnostic_refresh();
    }

    /// `[analysis].ignored_authors` is applied by CLI `analyze` and MCP, but
    /// not (yet) by the LSP diagnostics pipeline. Say so once instead of
    /// silently showing findings the other surfaces suppress.
    pub(crate) fn warn_author_filter_unsupported(&mut self) {
        let configured =
            self.project.as_ref().is_some_and(|p| !p.config.analysis.ignored_authors.is_empty());
        if !configured {
            self.author_warning_shown = false;
            return;
        }
        if self.author_warning_shown {
            return;
        }
        self.author_warning_shown = true;
        let params = lsp_types::ShowMessageParams {
            typ: lsp_types::MessageType::INFO,
            message: "bsl-analyzer: [analysis].ignored_authors is applied by `analyze` and MCP \
                      diagnostics, but not by editor diagnostics yet — the editor may show \
                      findings those surfaces suppress."
                .to_string(),
        };
        let notification = lsp_server::Notification::new("window/showMessage".to_string(), params);
        if let Err(e) = self.sender.send(notification.into()) {
            tracing::error!("failed to send author filter notice: {e}");
        }
    }

    fn show_scope_warning(&mut self, error: &str) {
        // One warning per distinct failure: a save-triggered rebuild against a
        // permanently invalid base must not spam the client on every save.
        if self.scope_warning_shown.as_deref() == Some(error) {
            return;
        }
        self.scope_warning_shown = Some(error.to_string());
        let params = lsp_types::ShowMessageParams {
            typ: lsp_types::MessageType::WARNING,
            message: format!(
                "bsl-analyzer: analysis scope is unavailable, analyzing everything: {error}"
            ),
        };
        let notification = lsp_server::Notification::new("window/showMessage".to_string(), params);
        if let Err(e) = self.sender.send(notification.into()) {
            tracing::error!("failed to send scope warning: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use base_db::AnalysisScope;

    use super::ScopeState;
    use crate::global_state::GlobalState;

    fn test_scope() -> Arc<AnalysisScope> {
        Arc::new(AnalysisScope::from_report(
            "vendor",
            Path::new("/repo"),
            [("Module.bsl".to_string(), None)],
        ))
    }

    #[test]
    fn ready_scope_attaches_to_config_and_stale_results_are_dropped() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.scope_generation = 7;
        state.analysis_scope = ScopeState::Loading { generation: 7 };

        // A result from a superseded build must not publish.
        state.handle_analysis_scope_ready(6, Ok(test_scope()), None);
        assert!(state.analysis_scope.is_loading(), "stale result must be dropped");
        assert!(state.diagnostics_config().scope.is_none());

        state.handle_analysis_scope_ready(7, Ok(test_scope()), None);
        assert!(matches!(state.analysis_scope, ScopeState::Ready { .. }));
        assert!(
            state.diagnostics_config().scope.is_some(),
            "the ready scope must reach the diagnostics config input"
        );
    }

    #[test]
    fn failed_scope_fails_open_and_warns_the_client() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.scope_generation = 1;
        state.analysis_scope = ScopeState::Loading { generation: 1 };

        state.handle_analysis_scope_ready(1, Err("ref 'vendor' not found".to_string()), None);

        assert!(matches!(state.analysis_scope, ScopeState::Failed { .. }));
        assert!(
            state.diagnostics_config().scope.is_none(),
            "a failed build must fail open (no filtering)"
        );

        let saw_warning = std::iter::from_fn(|| receiver.try_recv().ok()).any(|msg| {
            matches!(msg, lsp_server::Message::Notification(n) if n.method == "window/showMessage")
        });
        assert!(saw_warning, "the client must be told the filter is off");
    }

    #[test]
    fn config_rebuild_keeps_the_attached_scope() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.scope_generation = 1;
        state.analysis_scope = ScopeState::Loading { generation: 1 };
        state.handle_analysis_scope_ready(1, Ok(test_scope()), None);
        assert!(state.diagnostics_config().scope.is_some());

        // A config reload rebuilds the input from scratch; the scope must survive.
        state.update_diagnostics_config();
        assert!(state.diagnostics_config().scope.is_some());
    }

    #[test]
    fn save_clears_the_dirty_mark() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        let uri = crate::test_uri::file_uri("repo/Module.bsl");
        state.scope_dirty_docs.insert(uri.clone());

        crate::handlers::handle_did_save(
            &mut state,
            lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                text: None,
            },
        )
        .unwrap();

        assert!(
            !state.scope_dirty_docs.contains(&uri),
            "after a save the disk-derived scope describes the file again"
        );
    }
}
