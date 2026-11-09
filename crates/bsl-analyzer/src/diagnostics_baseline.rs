use std::path::Path;

use base_db::AnalysisScope;
use ide::diagnostics_baseline::{
    BaselineDiagnosticCandidate, DiagnosticsBaselineCoverage, DiagnosticsBaselineRange,
};
use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;

#[cfg(test)]
pub(crate) mod finality_fixture;
pub mod transaction;

/// Whether a file's diagnostics must be computed with the scope's line gate deferred.
///
/// A fingerprint carries the ordinal of a repeated diagnostic among the identical ones of
/// its file, so the classifier has to see every diagnostic of the file: handed only the
/// scoped lines, a NEW diagnostic could inherit the ordinal — and the fingerprint — of a
/// recorded one and be hidden as known. Only a snapshot that can change the set needs that;
/// for `Disabled` and `Error` the ordinary scoped computation stays, and with it the early
/// return of [`active_for_file`].
pub(crate) fn defers_scope_line_gate(
    snapshot: &DiagnosticsBaselineSnapshot,
    scope_is_active: bool,
) -> bool {
    scope_is_active && snapshot.affects_diagnostics()
}

/// The diagnostics a publication shows for one file: the baseline's active ones, then the
/// scope's line gate when the computation deferred it (`late_scope`).
///
/// Without a project root there is no baseline to classify against, but a deferred
/// computation still holds the whole file, so the scope is applied all the same. Without
/// a path neither can be done; callers do not defer the gate for such a file.
pub(crate) fn active_for_publication(
    snapshot: &DiagnosticsBaselineSnapshot,
    project_root: Option<&Path>,
    path: Option<&Path>,
    text: &str,
    diagnostics: Vec<ide::Diagnostic>,
    late_scope: Option<&AnalysisScope>,
) -> Vec<ide::Diagnostic> {
    match (project_root, path) {
        (Some(root), Some(path)) => {
            active_for_file(snapshot, root, path, text, diagnostics, late_scope)
        }
        (None, Some(path)) => {
            let Some(scope) = late_scope else { return diagnostics };
            let line_index = line_index::LineIndex::new(text);
            diagnostics
                .into_iter()
                .filter(|diagnostic| {
                    ide::diagnostic_in_scope(Some(scope), Some(path), &line_index, diagnostic)
                })
                .collect()
        }
        (_, None) => diagnostics,
    }
}

/// The file's diagnostics that stay active under `snapshot`, cut to `late_scope` after the
/// classification.
///
/// `late_scope` is the scope whose line gate the computation deferred
/// ([`defers_scope_line_gate`]), and then `diagnostics` is the file's whole finalized set:
/// the classifier numbers identical findings over all of it, and only the active ones are
/// cut to the scope afterwards. A classification that cannot be applied shows every
/// finding rather than hide one by the baseline, but still only those the scope admits —
/// the whole-file set is an intermediate result, never an answer. `Disabled` and `Error`
/// return before any preparation: their input was computed with the ordinary line gate.
pub(crate) fn active_for_file(
    snapshot: &DiagnosticsBaselineSnapshot,
    project_root: &Path,
    path: &Path,
    text: &str,
    diagnostics: Vec<ide::Diagnostic>,
    late_scope: Option<&AnalysisScope>,
) -> Vec<ide::Diagnostic> {
    // Before any work: a project without a baseline — the default — must not pay for
    // one. Both states below return the input unchanged, and preparing candidates for
    // them means indexing the whole file on every publication for a discarded result.
    if matches!(
        snapshot,
        DiagnosticsBaselineSnapshot::Disabled | DiagnosticsBaselineSnapshot::Error { .. }
    ) {
        return diagnostics;
    }
    #[cfg(test)]
    CANDIDATE_PREPARATIONS.with(|count| count.set(count.get() + 1));
    // One index for the file, not one per diagnostic: `to_output` builds its own.
    let line_index = line_index::LineIndex::new(text);
    let in_scope = |diagnostic: &ide::Diagnostic| {
        ide::diagnostic_in_scope(late_scope, Some(path), &line_index, diagnostic)
    };
    let fail_open = |diagnostics: Vec<ide::Diagnostic>| -> Vec<ide::Diagnostic> {
        diagnostics.into_iter().filter(|diagnostic| in_scope(diagnostic)).collect()
    };
    let Ok(relative) = path.strip_prefix(project_root) else { return fail_open(diagnostics) };
    let relative = relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let source_lines: Vec<_> = text.lines().collect();
    // Diagnostics whose range runs past the text are dropped rather than converted:
    // `to_output` panics on them, and this runs inside the publication worker, where a
    // panic costs the whole file's diagnostics. The LSP converter beside it drops such a
    // finding too, so the two agree on what a desynchronised pair yields.
    let diagnostics: Vec<_> = diagnostics
        .into_iter()
        .filter(|diagnostic| usize::from(diagnostic.range.end()) <= text.len())
        .collect();
    let candidates: Vec<_> = diagnostics
        .iter()
        .enumerate()
        .map(|(index, diagnostic)| {
            let output = diagnostic.to_output_with_index(text, &line_index);
            BaselineDiagnosticCandidate {
                diagnostic: index,
                path: relative.clone(),
                code: output.code,
                snippet: Some(ide::diagnostics_baseline::diagnostic_line_snippet(
                    &source_lines,
                    &line_index,
                    output.start_line,
                )),
                message: output.message,
                severity: output.severity,
                range: DiagnosticsBaselineRange {
                    start_line: output.start_line as u32,
                    start_column: output.start_column as u32,
                    end_line: output.end_line as u32,
                    end_column: output.end_column as u32,
                },
            }
        })
        .collect();
    let coverage = DiagnosticsBaselineCoverage::Partial {
        completed_files: std::collections::BTreeSet::from([relative.clone()]),
    };
    let active: std::collections::HashSet<_> = match snapshot {
        DiagnosticsBaselineSnapshot::Ready { baseline, project_path, .. } => {
            let Ok(classified) = ide::diagnostics_baseline::classify_diagnostics_with(
                baseline,
                project_path.clone(),
                candidates,
                &coverage,
                ide::diagnostics_baseline::ResolvedPolicy::Skip,
            ) else {
                return fail_open(diagnostics);
            };
            classified.new.into_iter().map(|item| item.diagnostic).collect()
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            let Some(owner) = plan.owner_for_project_path(&relative) else {
                return fail_open(diagnostics);
            };
            let candidates = candidates
                .into_iter()
                .map(|candidate| {
                    ide::partitioned_diagnostics_baseline::PartitionedBaselineDiagnosticCandidate {
                        partition_id: owner.to_owned(),
                        candidate,
                    }
                })
                .collect();
            let Ok(coverage) =
                ide::partitioned_diagnostics_baseline::partitioned_coverage(plan, &coverage, None)
            else {
                return fail_open(diagnostics);
            };
            // Only this file's active diagnostics are wanted; `resolved` would walk
            // every entry of every enabled partition on each publication and be
            // discarded, making editor latency grow with the accumulated debt.
            let Ok(classified) =
                ide::partitioned_diagnostics_baseline::classify_partitioned_diagnostics_with(
                    baseline,
                    plan,
                    project_path.clone(),
                    candidates,
                    &coverage,
                    ide::diagnostics_baseline::ResolvedPolicy::Skip,
                )
            else {
                return fail_open(diagnostics);
            };
            classified
                .new
                .into_iter()
                .chain(classified.unsuppressed)
                .map(|item| item.diagnostic)
                .collect()
        }
        DiagnosticsBaselineSnapshot::Disabled | DiagnosticsBaselineSnapshot::Error { .. } => {
            return diagnostics;
        }
    };
    diagnostics
        .into_iter()
        .enumerate()
        .filter_map(|(index, diagnostic)| {
            (active.contains(&index) && in_scope(&diagnostic)).then_some(diagnostic)
        })
        .collect()
}

// Counts the publications that went past the early return of `active_for_file` and
// prepared candidates, so a test can tell the disabled fast path from the work by the
// count rather than by the clock.
#[cfg(test)]
thread_local! {
    pub(crate) static CANDIDATE_PREPARATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide::diagnostics_baseline::{
        diagnostic_fingerprint, normalize_diagnostic_snippet, DiagnosticsBaseline,
        DiagnosticsBaselineEntry, DiagnosticsBaselineRange, DiagnosticsBaselineScope,
        DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
    };
    use ide::{Diagnostic, DiagnosticCode, Severity, TextRange};

    fn diagnostic(code: DiagnosticCode) -> Diagnostic {
        Diagnostic {
            code,
            message: code.as_str().to_owned(),
            severity: Severity::Warning,
            range: TextRange::new(0.into(), 8.into()),
            tags: Vec::new(),
            fixes: Vec::new(),
        }
    }

    /// A diagnostic whose range runs past the text would panic the converter, and this
    /// code runs inside the publication worker — the panic would cost the whole file's
    /// diagnostics. Such a finding is dropped, exactly as the LSP converter drops it.
    #[test]
    fn a_diagnostic_past_the_end_of_the_text_is_dropped_not_panicked_on() {
        let root = Path::new("/workspace");
        let path = root.join("Module.bsl");
        let text = "Процедура П()\n";
        let mut beyond = diagnostic(DiagnosticCode::EmptyCodeBlock);
        beyond.range = TextRange::new(0.into(), 9_000.into());

        let baseline = DiagnosticsBaseline {
            schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![],
        };
        let snapshot = DiagnosticsBaselineSnapshot::Ready {
            baseline,
            project_path: "baseline.json".to_owned(),
            path: root.join("baseline.json"),
            epoch: "e".to_owned(),
            ground: Default::default(),
        };
        let active = active_for_file(&snapshot, root, &path, text, vec![beyond], None);
        assert!(active.is_empty(), "the desynchronised finding must not reach the converter");
    }

    /// A project without a baseline is the default, and its publications must not pay
    /// for the feature. The gate is time because the work is preparation whose result is
    /// discarded: with the early return it is nothing, without it the file is indexed
    /// once per diagnostic. The `Ready` run beside it is the positive control — it does
    /// the preparation for real, so a measurement blind to it would fail here.
    #[test]
    fn disabled_baseline_does_not_prepare_candidates() {
        let root = Path::new("/workspace");
        let path = root.join("Module.bsl");
        let text = "Сообщить(А);\n".repeat(20_000);
        let diagnostics: Vec<_> =
            (0..2_000).map(|_| diagnostic(DiagnosticCode::EmptyCodeBlock)).collect();

        // What the timer must contain is the call and nothing else. Handing the input over
        // costs a clone of two thousand diagnostics — on its own more than the disabled
        // call, and allocation-bound, so on a loaded machine it grows far faster than the
        // work being measured. The fastest of several runs is taken for the same reason: a
        // call of a few microseconds otherwise reports whatever pause the scheduler put in
        // the middle of it, and a single such pause is enough to close the gap.
        let fastest = |snapshot: &DiagnosticsBaselineSnapshot| {
            (0..5)
                .map(|_| {
                    let input = diagnostics.clone();
                    let started = std::time::Instant::now();
                    let out = active_for_file(snapshot, root, &path, &text, input, None);
                    (started.elapsed(), out.len())
                })
                .min_by_key(|(elapsed, _)| *elapsed)
                .expect("five runs")
        };

        let (disabled, kept) = fastest(&DiagnosticsBaselineSnapshot::Disabled);
        assert_eq!(kept, diagnostics.len(), "a disabled baseline changes nothing");

        let baseline = DiagnosticsBaseline {
            schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![],
        };
        let ready = DiagnosticsBaselineSnapshot::Ready {
            baseline,
            project_path: "baseline.json".to_owned(),
            path: root.join("baseline.json"),
            epoch: "e".to_owned(),
            ground: Default::default(),
        };
        let (enabled, _) = fastest(&ready);

        assert!(
            disabled * 8 < enabled,
            "the disabled path must not do the enabled path's work: {disabled:?} against {enabled:?}"
        );
    }

    #[test]
    fn lsp_diagnostics_baseline_publish_keeps_new_and_protected_only() {
        let root = Path::new("/workspace");
        let path = root.join("Module.bsl");
        let text = "Процедура П()\nКонецПроцедуры\n";
        let snippet = normalize_diagnostic_snippet(text.lines().next().unwrap());
        let known = DiagnosticCode::EmptyCodeBlock;
        let baseline = DiagnosticsBaseline {
            schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![DiagnosticsBaselineEntry {
                fingerprint: diagnostic_fingerprint("Module.bsl", known.as_str(), &snippet, 0),
                path: "Module.bsl".to_owned(),
                code: known.as_str().to_owned(),
                snippet,
                occurrence: 0,
                message: known.as_str().to_owned(),
                severity: "warning".to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 8,
                },
            }],
        };
        let snapshot = DiagnosticsBaselineSnapshot::Ready {
            baseline,
            project_path: "baseline.json".to_owned(),
            path: root.join("baseline.json"),
            epoch: "test".to_owned(),
            ground: Default::default(),
        };
        let active = active_for_file(
            &snapshot,
            root,
            &path,
            text,
            vec![
                diagnostic(known),
                diagnostic(DiagnosticCode::UnreachableCode),
                diagnostic(DiagnosticCode::UnknownSuppressionCode),
            ],
            None,
        );
        let codes: Vec<_> = active.iter().map(|item| item.code).collect();
        assert!(!codes.contains(&known));
        assert!(codes.contains(&DiagnosticCode::UnreachableCode));
        assert!(codes.contains(&DiagnosticCode::UnknownSuppressionCode));
    }

    #[test]
    fn lsp_partitioned_baseline_publish_uses_the_owner_partition() {
        use ide::partitioned_diagnostics_baseline::{
            diagnostics_manifest, diagnostics_manifest_json, diagnostics_partition_json,
            partition_object_path, DiagnosticsBaselineManifestEntry,
        };
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for source in ["src/cf", "src/cfe/Ext"] {
            std::fs::create_dir_all(root.join(source)).unwrap();
            std::fs::write(root.join(source).join("Configuration.xml"), "<Configuration/>")
                .unwrap();
        }
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            r#"[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext", scope = "InfoBase" }]
[diagnostics.baseline]
directory = "baselines"
"#,
        )
        .unwrap();
        let text = "Процедура П()\nКонецПроцедуры\n";
        let relative = "src/cfe/Ext/Module.bsl";
        let known = DiagnosticCode::EmptyCodeBlock;
        let snippet = normalize_diagnostic_snippet(text.lines().next().unwrap());
        let entry = DiagnosticsBaselineEntry {
            fingerprint: diagnostic_fingerprint(relative, known.as_str(), &snippet, 0),
            path: relative.to_owned(),
            code: known.as_str().to_owned(),
            snippet,
            occurrence: 0,
            message: known.as_str().to_owned(),
            severity: "warning".to_owned(),
            range: DiagnosticsBaselineRange {
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 8,
            },
        };
        let project = project_model::Project::new(root).unwrap();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let directory =
            project_model::ManagedBaselineDirectory::open(root, "baselines", true).unwrap();
        let mut manifest_entries = Vec::new();
        for partition in &plan.partitions {
            let diagnostics =
                if partition.id == "extension:Ext" { vec![entry.clone()] } else { vec![] };
            let bytes =
                diagnostics_partition_json(partition.identity.clone(), diagnostics).unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let path = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
            directory.create_file_new(&path).unwrap().write_all(&bytes).unwrap();
            manifest_entries.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file: path,
                blake3: hash,
            });
        }
        let manifest =
            diagnostics_manifest(plan.project_scope_fingerprint.clone(), manifest_entries);
        directory
            .create_file_new("manifest.json")
            .unwrap()
            .write_all(&diagnostics_manifest_json(&manifest).unwrap())
            .unwrap();
        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        let active = active_for_file(
            &snapshot,
            root,
            &root.join(relative),
            text,
            vec![diagnostic(known), diagnostic(DiagnosticCode::UnknownSuppressionCode)],
            None,
        );
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].code, DiagnosticCode::UnknownSuppressionCode);

        let all = active_for_file(
            &DiagnosticsBaselineSnapshot::Error {
                path: None,
                project_path: None,
                observation_paths: vec![],
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                code: "invalid_set".to_owned(),
                detail: "broken".to_owned(),
                epoch: "error".to_owned(),
                errors: vec![ide::diagnostics_baseline::DiagnosticsBaselineErrorSummary {
                    partition_id: None,
                    code: "invalid_set".to_owned(),
                    detail: "broken".to_owned(),
                    epoch: "error".to_owned(),
                }],
                ground: Default::default(),
                read_empty: false,
            },
            root,
            &root.join(relative),
            text,
            vec![diagnostic(known)],
            None,
        );
        assert_eq!(all.len(), 1, "LSP fails open for an invalid partition set");
    }

    fn diagnostic_on(text: &str, line: usize, code: DiagnosticCode) -> Diagnostic {
        let start: usize = text.split_inclusive('\n').take(line - 1).map(str::len).sum();
        let len = text.split_inclusive('\n').nth(line - 1).unwrap().trim_end().len();
        let mut diagnostic = diagnostic(code);
        diagnostic.range = TextRange::new((start as u32).into(), ((start + len) as u32).into());
        diagnostic
    }

    fn empty_ready(root: &Path) -> DiagnosticsBaselineSnapshot {
        DiagnosticsBaselineSnapshot::Ready {
            baseline: DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
                diagnostics: vec![],
            },
            project_path: "baseline.json".to_owned(),
            path: root.join("baseline.json"),
            epoch: "e".to_owned(),
            ground: Default::default(),
        }
    }

    /// The baseline applies under a scope now, and safely: handed the file's whole set it
    /// numbers c as 2 and keeps it, hides the recorded a and b, and cuts the result to the
    /// scope afterwards. Classifying the scoped set — what the removed refusal guarded
    /// against — would number c as 1 and hide it.
    #[test]
    fn finality_scoped_baseline() {
        use finality_fixture::{self_assign_lines, Form, Workspace, MODULE};

        let workspace = Workspace::new(Form::V1);
        let snapshot = workspace.snapshot(2);
        let scope = workspace.scope(Some(vec![[3, 4]]));
        let full: Vec<_> =
            (2..=4).map(|line| diagnostic_on(MODULE, line, DiagnosticCode::SelfAssign)).collect();

        let active = active_for_file(
            &snapshot,
            &workspace.root,
            &workspace.module,
            MODULE,
            full.clone(),
            Some(&scope),
        );
        assert_eq!(self_assign_lines(&active), vec![4], "only c stays: {active:#?}");

        let scoped_first: Vec<_> = full[1..].to_vec();
        let renumbered = active_for_file(
            &snapshot,
            &workspace.root,
            &workspace.module,
            MODULE,
            scoped_first,
            None,
        );
        assert!(renumbered.is_empty(), "control: the scoped set alone loses c: {renumbered:#?}");

        let unrecorded = active_for_file(
            &workspace.snapshot(0),
            &workspace.root,
            &workspace.module,
            MODULE,
            full,
            Some(&scope),
        );
        assert_eq!(self_assign_lines(&unrecorded), vec![3, 4], "control: no baseline shows b, c");
    }

    /// A classification that cannot be applied — here the file lies outside the project
    /// root — hides nothing by the baseline, yet the answer is still cut to the scope:
    /// the in-hunk finding and the broken-directive meta stay, the off-hunk one does not.
    #[test]
    fn finality_failed_classification_still_projects_scope() {
        let root = Path::new("/workspace");
        let path = Path::new("/elsewhere/Module.bsl");
        let text = "Процедура П()\n    А = А;\n    Б = Б;\n    // bsl-analyzer:off NoSuchRule\nКонецПроцедуры\n";
        let full = vec![
            diagnostic_on(text, 2, DiagnosticCode::SelfAssign),
            diagnostic_on(text, 3, DiagnosticCode::SelfAssign),
            diagnostic_on(text, 4, DiagnosticCode::UnknownSuppressionCode),
        ];
        let scope = base_db::AnalysisScope::from_report(
            "vendor",
            Path::new("/elsewhere"),
            [("Module.bsl".to_owned(), Some(vec![[2, 2]]))],
        );
        let snapshot = empty_ready(root);

        let shown = active_for_file(&snapshot, root, path, text, full.clone(), Some(&scope));
        let shown: Vec<_> = shown.iter().map(|d| (d.code, d.range)).collect();
        assert_eq!(
            shown,
            vec![(full[0].code, full[0].range), (full[2].code, full[2].range)],
            "the in-hunk finding and the meta stay, the off-hunk one does not"
        );

        let unscoped = active_for_file(&snapshot, root, path, text, full.clone(), None);
        assert_eq!(unscoped.len(), full.len(), "control: without a scope everything stays");
    }

    /// `Disabled` and `Error` keep the fast path: the gate is not deferred for them, and
    /// `active_for_file` returns their (already scoped) input before preparing anything —
    /// counted, not timed. The `Ready` run beside them prepares for real.
    #[test]
    fn finality_disabled_keeps_fast_path() {
        let root = Path::new("/workspace");
        let path = root.join("Module.bsl");
        let text = "Процедура П()\n    А = А;\n    Б = Б;\nКонецПроцедуры\n";
        let input = vec![diagnostic_on(text, 2, DiagnosticCode::SelfAssign)];
        let scope = base_db::AnalysisScope::from_report(
            "vendor",
            root,
            [("Module.bsl".to_owned(), Some(vec![[3, 3]]))],
        );
        let error = DiagnosticsBaselineSnapshot::Error {
            path: None,
            project_path: None,
            observation_paths: vec![],
            selection: None,
            partitions_enabled: None,
            partitions_unsuppressed: None,
            code: "invalid".to_owned(),
            detail: "broken".to_owned(),
            epoch: "error".to_owned(),
            errors: vec![],
            ground: Default::default(),
            read_empty: false,
        };
        let prepared = || CANDIDATE_PREPARATIONS.with(std::cell::Cell::get);

        for snapshot in [DiagnosticsBaselineSnapshot::Disabled, error] {
            assert!(!defers_scope_line_gate(&snapshot, true), "{snapshot:?} must not defer");
            let before = prepared();
            let out = active_for_file(&snapshot, root, &path, text, input.clone(), Some(&scope));
            assert_eq!(out, input, "{snapshot:?} returns its input unchanged");
            assert_eq!(prepared(), before, "{snapshot:?} must not prepare candidates");
        }

        let ready = empty_ready(root);
        assert!(defers_scope_line_gate(&ready, true));
        assert!(!defers_scope_line_gate(&ready, false));
        let before = prepared();
        let _ = active_for_file(&ready, root, &path, text, input, Some(&scope));
        assert_eq!(prepared(), before + 1, "control: Ready prepares candidates");
    }
}
