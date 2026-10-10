use std::path::Path;

use ide::diagnostics_baseline::{
    BaselineDiagnosticCandidate, DiagnosticsBaselineCoverage, DiagnosticsBaselineRange,
};
use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;

pub mod transaction;

/// Whether the baseline may be applied to a diagnostics set produced under `scope`.
///
/// A fingerprint carries the ordinal of a repeated diagnostic, and that ordinal is only
/// stable when the classifier sees every diagnostic of the file. Under `[analysis]
/// diff_base` the set reaching it is already line-gated, so a NEW diagnostic can inherit
/// the ordinal — and therefore the fingerprint — of a recorded one and be suppressed as
/// known. The CLI refuses baseline operations under a scope for the same reason; here
/// the baseline is simply not applied, which can only show more diagnostics, never fewer.
pub(crate) fn applies_under_scope(scope_is_active: bool) -> bool {
    !scope_is_active
}

pub(crate) fn active_for_file(
    snapshot: &DiagnosticsBaselineSnapshot,
    project_root: &Path,
    path: &Path,
    text: &str,
    diagnostics: Vec<ide::Diagnostic>,
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
    let Ok(relative) = path.strip_prefix(project_root) else { return diagnostics };
    let relative = relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let source_lines: Vec<_> = text.lines().collect();
    // One index for the file, not one per diagnostic: `to_output` builds its own.
    let line_index = line_index::LineIndex::new(text);
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
                return diagnostics;
            };
            classified.new.into_iter().map(|item| item.diagnostic).collect()
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            let Some(owner) = plan.owner_for_project_path(&relative) else { return diagnostics };
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
                return diagnostics;
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
                return diagnostics;
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
        .filter_map(|(index, diagnostic)| active.contains(&index).then_some(diagnostic))
        .collect()
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
        let active = active_for_file(&snapshot, root, &path, text, vec![beyond]);
        assert!(active.is_empty(), "the desynchronised finding must not reach the converter");
    }

    /// The ordinal in a fingerprint counts repetitions within the file, so it is only
    /// stable when the classifier sees the whole file. Under an analysis scope the set
    /// is already line-gated, and a NEW diagnostic would inherit the ordinal — and the
    /// fingerprint — of a recorded one. The guard is what keeps that from happening.
    #[test]
    fn a_scoped_set_must_not_be_classified_against_the_baseline() {
        assert!(applies_under_scope(false), "without a scope the baseline applies");
        assert!(!applies_under_scope(true), "under a scope it must not");

        // The suppression the guard prevents, shown directly: the second occurrence of
        // an identical line, handed over alone, takes the first one's fingerprint.
        let root = Path::new("/workspace");
        let path = root.join("Module.bsl");
        let text = "Сообщить(А);\nX = 1;\nСообщить(А);\n";
        let snippet = normalize_diagnostic_snippet("Сообщить(А);");
        let code = DiagnosticCode::EmptyCodeBlock;
        let baseline = DiagnosticsBaseline {
            schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![DiagnosticsBaselineEntry {
                fingerprint: diagnostic_fingerprint("Module.bsl", code.as_str(), &snippet, 0),
                path: "Module.bsl".to_owned(),
                code: code.as_str().to_owned(),
                snippet,
                occurrence: 0,
                message: code.as_str().to_owned(),
                severity: "Warning".to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 12,
                },
            }],
        };
        let snapshot = DiagnosticsBaselineSnapshot::Ready {
            baseline,
            project_path: "baseline.json".to_owned(),
            path: root.join("baseline.json"),
            epoch: "e".to_owned(),
            ground: Default::default(),
        };
        let mut third_line = diagnostic(code);
        third_line.range = TextRange::new(20.into(), 32.into());
        let gated = active_for_file(&snapshot, root, &path, text, vec![third_line]);
        assert!(gated.is_empty(), "documents the very suppression the guard exists to prevent");
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
                    let out = active_for_file(snapshot, root, &path, &text, input);
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
extensions = [{ name = "Ext", path = "src/cfe/Ext" }]
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
        );
        assert_eq!(all.len(), 1, "LSP fails open for an invalid partition set");
    }
}
