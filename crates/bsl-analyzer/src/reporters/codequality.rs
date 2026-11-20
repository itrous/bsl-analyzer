use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::ser::{Serialize, SerializeSeq, Serializer};

use ide::diagnostics_baseline::{diagnostic_fingerprint, diagnostic_line_snippet};

use super::{AnalysisResults, FileAnalysis, Reporter};

/// GitLab Code Quality reporter (CodeClimate JSON schema).
///
/// GitLab renders this artifact (`artifacts:reports:codequality`) as a widget in
/// the merge-request diff, annotating newly introduced findings. The diff between
/// base and head pipelines is computed by `fingerprint`, so the fingerprint must
/// be stable across unrelated line shifts — it is derived from the file path, the
/// rule code, and the normalized source line, never from the line number.
pub struct CodeQualityReporter;

impl Reporter for CodeQualityReporter {
    fn key(&self) -> &'static str {
        "codequality"
    }

    fn report(&self, results: &AnalysisResults, output_dir: &Path) -> anyhow::Result<()> {
        let output_file = output_dir.join("gl-code-quality-report.json");
        let file = std::fs::File::create(&output_file)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &CodeQualitySeq { results })?;
        writer.flush()?;

        tracing::info!("GitLab Code Quality report written to {:?}", output_file);
        Ok(())
    }
}

/// Streams every finding as one CodeClimate issue without materializing the full
/// array. Emission order is canonical (files by path, findings within a file by
/// position/code/message) so two runs over the same tree produce byte-identical
/// documents.
struct CodeQualitySeq<'a> {
    results: &'a AnalysisResults,
}

impl Serialize for CodeQualitySeq<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut files: Vec<&FileAnalysis> = self.results.diagnostics.iter().collect();
        files.sort_by_key(|f| cq_path(&f.relative_path));

        let mut seq = serializer.serialize_seq(None)?;
        for file in files {
            // Producers that carry the analyzed text (the Salsa pipeline) supply a
            // snippet per finding up front. When they don't (the legacy streaming
            // path leaves `line_snippets` empty), fall back to reading the file
            // once and normalizing its lines here, so the fingerprint keeps the
            // same source-line identity instead of silently degrading to
            // path+code+occurrence only.
            let fallback_snippets: Option<Vec<String>> =
                if file.line_snippets.is_empty() && !file.diagnostics.is_empty() {
                    std::fs::read_to_string(&file.path).ok().map(|text| {
                        let lines: Vec<_> = text.lines().collect();
                        let index = line_index::LineIndex::new(&text);
                        file.diagnostics
                            .iter()
                            .map(|d| diagnostic_line_snippet(&lines, &index, d.start_line))
                            .collect()
                    })
                } else {
                    None
                };
            let snippets = fallback_snippets.as_ref().unwrap_or(&file.line_snippets);

            // Pair each finding with its source line, then sort the pairs together
            // so the snippet stays aligned with its finding through the canonical
            // ordering.
            let mut rows: Vec<(&_, &str)> = file
                .diagnostics
                .iter()
                .enumerate()
                .map(|(i, d)| (d, snippets.get(i).map(String::as_str).unwrap_or("")))
                .collect();
            rows.sort_by(|(a, _), (b, _)| {
                (a.start_line, a.start_column, a.end_line, a.end_column, &a.code, &a.message).cmp(
                    &(b.start_line, b.start_column, b.end_line, b.end_column, &b.code, &b.message),
                )
            });

            // Disambiguates findings that share the same (code, source line): the
            // Nth occurrence folds its index into the fingerprint so identical
            // findings do not collapse to one entry in the widget. Reset per file
            // because the fingerprint is already scoped by path.
            let mut occurrences: HashMap<(&str, &str), u32> = HashMap::new();

            for (index, (diagnostic, snippet)) in rows.into_iter().enumerate() {
                // The producer's ordinal wins when it is there: it was counted before
                // the baseline suppressed anything, and counting again here would give
                // an active finding the fingerprint of a suppressed one.
                let counted = occurrences.entry((&diagnostic.code, snippet)).or_insert(0);
                let occurrence = file.occurrences.get(index).copied().unwrap_or(*counted);
                *counted += 1;
                let fingerprint = diagnostic_fingerprint(
                    &cq_path(&file.relative_path),
                    &diagnostic.code,
                    snippet,
                    occurrence,
                );

                seq.serialize_element(&Issue {
                    description: &diagnostic.message,
                    check_name: &diagnostic.code,
                    fingerprint: &fingerprint,
                    severity: cq_severity(&diagnostic.severity),
                    path: cq_path(&file.relative_path),
                    begin: diagnostic.start_line + 1,
                })?;
            }
        }
        seq.end()
    }
}

struct Issue<'a> {
    description: &'a str,
    check_name: &'a str,
    fingerprint: &'a str,
    severity: &'static str,
    path: String,
    begin: usize,
}

impl Serialize for Issue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut issue = serializer.serialize_struct("issue", 5)?;
        issue.serialize_field("description", self.description)?;
        issue.serialize_field("check_name", self.check_name)?;
        issue.serialize_field("fingerprint", self.fingerprint)?;
        issue.serialize_field("severity", self.severity)?;
        issue.serialize_field("location", &Location { path: &self.path, begin: self.begin })?;
        issue.end()
    }
}

struct Location<'a> {
    path: &'a str,
    begin: usize,
}

impl Serialize for Location<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut location = serializer.serialize_struct("location", 2)?;
        location.serialize_field("path", self.path)?;
        location.serialize_field("lines", &Lines { begin: self.begin })?;
        location.end()
    }
}

struct Lines {
    begin: usize,
}

impl Serialize for Lines {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut lines = serializer.serialize_struct("lines", 1)?;
        lines.serialize_field("begin", &self.begin)?;
        lines.end()
    }
}

/// Repo-relative path with forward slashes — GitLab matches these against the
/// files changed in the merge request.
fn cq_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// CodeClimate severity scale is `info | minor | major | critical | blocker`.
fn cq_severity(severity: &str) -> &'static str {
    match severity {
        "Blocker" => "blocker",
        "Critical" | "Error" => "critical",
        "Major" | "Warning" => "major",
        "Information" => "minor",
        "Hint" => "info",
        _ => "major",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ide::DiagnosticOutput;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::reporters::{AnalysisResults, FileAnalysis, Reporter};

    fn diag_at(code: &str, severity: &str, start_line: usize) -> DiagnosticOutput {
        DiagnosticOutput {
            code: code.to_string(),
            message: format!("{code} finding"),
            severity: severity.to_string(),
            start_line,
            start_column: 0,
            end_line: start_line,
            end_column: 1,
            tags: vec![],
        }
    }

    fn read_report(temp: &TempDir) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(temp.path().join("gl-code-quality-report.json"))
                .expect("report"),
        )
        .expect("valid json")
    }

    fn one_file(
        rel: &str,
        diagnostics: Vec<DiagnosticOutput>,
        snippets: &[&str],
    ) -> AnalysisResults {
        AnalysisResults {
            files_analyzed: 1,
            files_with_issues: 1,
            total_diagnostics: diagnostics.len(),
            elapsed_secs: 0.1,
            diagnostics: vec![FileAnalysis {
                path: PathBuf::from(rel),
                relative_path: PathBuf::from(rel),
                diagnostics,
                line_snippets: snippets.iter().map(|s| s.to_string()).collect(),
                occurrences: Vec::new(),
            }],
            source_dir: PathBuf::from("."),
            workspace_dir: PathBuf::from("."),
            baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
        }
    }

    /// The fingerprint folds in the ordinal of a repeated finding. When the baseline has
    /// suppressed an earlier duplicate, the survivor must keep ITS ordinal: renumbering
    /// would hand it the fingerprint the baseline file holds for the suppressed one.
    #[test]
    fn suppressed_duplicates_do_not_shift_the_fingerprint() {
        let snippet = "// aaaa";
        let file = |occurrences: Vec<u32>, count: usize| FileAnalysis {
            path: PathBuf::from("a.bsl"),
            relative_path: PathBuf::from("a.bsl"),
            diagnostics: (0..count).map(|_| diag_at("LineLength", "Warning", 0)).collect(),
            line_snippets: (0..count).map(|_| snippet.to_owned()).collect(),
            occurrences,
        };
        let render = |analysis: FileAnalysis| {
            let results = AnalysisResults {
                files_analyzed: 1,
                files_with_issues: 1,
                total_diagnostics: analysis.diagnostics.len(),
                elapsed_secs: 0.0,
                diagnostics: vec![analysis],
                source_dir: PathBuf::from("."),
                workspace_dir: PathBuf::from("."),
                baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
            };
            serde_json::to_value(CodeQualitySeq { results: &results }).unwrap()
        };

        let full = render(file(vec![0, 1, 2], 3));
        let third = full.as_array().unwrap()[2]["fingerprint"].clone();

        // Two earlier duplicates suppressed: one finding left, ordinal 2.
        let survivor = render(file(vec![2], 1));
        assert_eq!(
            survivor.as_array().unwrap()[0]["fingerprint"],
            third,
            "the survivor keeps the fingerprint it had in the full set"
        );

        // Without the producer's ordinals the reporter counts positions itself, which is
        // exactly the behaviour that shifts the fingerprint.
        let renumbered = render(file(Vec::new(), 1));
        assert_ne!(renumbered.as_array().unwrap()[0]["fingerprint"], third);
    }

    /// Code Quality reads the occurrence the classification gave a survivor. In the
    /// counterexample c keeps occurrence 2 after a scope dropped a and the baseline hid b,
    /// and after one more retain of the survivors it still pairs with its own snippet; its
    /// fingerprint is the one the full input gives the third finding. Numbering the
    /// survivors from zero after the retain would hand c the fingerprint of a.
    #[test]
    fn finality_existing_baseline_and_reporter_identity() {
        let snippet = "А = А;";
        let render = |analysis: FileAnalysis| {
            let results = AnalysisResults {
                files_analyzed: 1,
                files_with_issues: 1,
                total_diagnostics: analysis.diagnostics.len(),
                elapsed_secs: 0.0,
                diagnostics: vec![analysis],
                source_dir: PathBuf::from("."),
                workspace_dir: PathBuf::from("."),
                baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
            };
            serde_json::to_value(CodeQualitySeq { results: &results }).unwrap()
        };
        let file = |lines: &[usize], occurrences: Vec<u32>| FileAnalysis {
            path: PathBuf::from("module.bsl"),
            relative_path: PathBuf::from("module.bsl"),
            diagnostics: lines.iter().map(|line| diag_at("SelfAssign", "Warning", *line)).collect(),
            line_snippets: lines.iter().map(|_| snippet.to_owned()).collect(),
            occurrences,
        };

        let full = render(file(&[1, 2, 3], vec![0, 1, 2]));
        let c = full.as_array().unwrap()[2]["fingerprint"].clone();
        let a = full.as_array().unwrap()[0]["fingerprint"].clone();

        // b and c survived the scope and the baseline with their classified numbers; a
        // later retain drops b.
        let mut survivors = file(&[2, 3], vec![1, 2]);
        survivors.retain_findings(|index| index == 1);
        assert_eq!(survivors.diagnostics[0].start_line, 3);
        assert_eq!((survivors.line_snippets.len(), survivors.occurrences.clone()), (1, vec![2]));
        let shown = render(survivors);
        assert_eq!(shown.as_array().unwrap()[0]["fingerprint"], c, "c keeps its fingerprint");

        let renumbered = render(file(&[3], vec![0]));
        assert_eq!(
            renumbered.as_array().unwrap()[0]["fingerprint"],
            a,
            "control: numbering from zero after the retain gives c the fingerprint of a"
        );
    }

    #[test]
    fn emits_codeclimate_entries_with_expected_shape() {
        let temp = TempDir::new().unwrap();
        let results = one_file(
            "CommonModules/Module/Ext/Module.bsl",
            vec![diag_at("LineLength", "Warning", 1)],
            &["Значение = СтрНайти(Строка, Подстрока);"],
        );

        CodeQualityReporter.report(&results, temp.path()).unwrap();
        let report = read_report(&temp);

        let entry = &report[0];
        assert_eq!(entry["description"], "LineLength finding");
        assert_eq!(entry["check_name"], "LineLength");
        assert_eq!(entry["severity"], "major");
        assert_eq!(entry["location"]["path"], "CommonModules/Module/Ext/Module.bsl");
        assert_eq!(entry["location"]["lines"]["begin"], 2);
        assert!(entry["fingerprint"].as_str().unwrap().len() >= 32);
    }

    #[test]
    fn fingerprint_is_stable_across_line_shifts() {
        // Same source line, different line number → same fingerprint: the line
        // number must not feed the hash.
        let render = |start_line: usize| {
            let temp = TempDir::new().unwrap();
            let results = one_file(
                "a.bsl",
                vec![diag_at("SomeRule", "Warning", start_line)],
                &["Значение = СтрНайти(Строка, Подстрока);"],
            );
            CodeQualityReporter.report(&results, temp.path()).unwrap();
            read_report(&temp)[0]["fingerprint"].as_str().unwrap().to_string()
        };

        assert_eq!(
            render(0),
            render(5),
            "inserting lines above a finding must not change its fingerprint"
        );
    }

    #[test]
    fn reads_source_when_snippets_absent_matching_supplied_snippet() {
        // Legacy path leaves `line_snippets` empty; the reporter must read the file
        // and fingerprint from the same normalized source line as the Salsa path,
        // not silently drop to a path+code+occurrence-only fingerprint.
        let line = "Значение = СтрНайти(Строка, Подстрока);";

        let supplied = {
            let temp = TempDir::new().unwrap();
            let results = one_file("a.bsl", vec![diag_at("Rule", "Warning", 2)], &[line]);
            CodeQualityReporter.report(&results, temp.path()).unwrap();
            read_report(&temp)[0]["fingerprint"].as_str().unwrap().to_string()
        };

        let from_file = {
            let temp = TempDir::new().unwrap();
            let src = TempDir::new().unwrap();
            let file_path = src.path().join("Module.bsl");
            std::fs::write(&file_path, format!("\n\n{line}\n")).unwrap();
            let results = AnalysisResults {
                files_analyzed: 1,
                files_with_issues: 1,
                total_diagnostics: 1,
                elapsed_secs: 0.1,
                diagnostics: vec![FileAnalysis {
                    path: file_path,
                    relative_path: PathBuf::from("a.bsl"),
                    diagnostics: vec![diag_at("Rule", "Warning", 2)],
                    line_snippets: vec![],
                    occurrences: Vec::new(),
                }],
                source_dir: PathBuf::from("."),
                workspace_dir: PathBuf::from("."),
                baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
            };
            CodeQualityReporter.report(&results, temp.path()).unwrap();
            read_report(&temp)[0]["fingerprint"].as_str().unwrap().to_string()
        };

        assert_eq!(
            supplied, from_file,
            "read-fallback must match the supplied-snippet fingerprint"
        );
    }

    #[test]
    fn identical_findings_get_distinct_fingerprints() {
        let temp = TempDir::new().unwrap();
        let results = one_file(
            "a.bsl",
            vec![diag_at("Rule", "Warning", 0), diag_at("Rule", "Warning", 1)],
            &["Сообщить(А);", "Сообщить(А);"],
        );
        CodeQualityReporter.report(&results, temp.path()).unwrap();
        let report = read_report(&temp);

        let f0 = report[0]["fingerprint"].as_str().unwrap();
        let f1 = report[1]["fingerprint"].as_str().unwrap();
        assert_ne!(f0, f1, "identical findings on identical lines must not collapse");
    }

    #[test]
    fn orders_entries_canonically_regardless_of_input_order() {
        let temp = TempDir::new().unwrap();
        let results = AnalysisResults {
            files_analyzed: 2,
            files_with_issues: 2,
            total_diagnostics: 3,
            elapsed_secs: 0.1,
            diagnostics: vec![
                FileAnalysis {
                    path: PathBuf::from("b.bsl"),
                    relative_path: PathBuf::from("b.bsl"),
                    diagnostics: vec![diag_at("R", "Warning", 0)],
                    line_snippets: vec![],
                    occurrences: Vec::new(),
                },
                FileAnalysis {
                    path: PathBuf::from("a.bsl"),
                    relative_path: PathBuf::from("a.bsl"),
                    diagnostics: vec![diag_at("Z", "Warning", 5), diag_at("A", "Warning", 5)],
                    line_snippets: vec![],
                    occurrences: Vec::new(),
                },
            ],
            source_dir: PathBuf::from("."),
            workspace_dir: PathBuf::from("."),
            baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary::disabled(),
        };
        CodeQualityReporter.report(&results, temp.path()).unwrap();
        let report = read_report(&temp);

        let paths: Vec<_> = report
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["location"]["path"].as_str().unwrap().to_string(),
                    e["check_name"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                ("a.bsl".to_string(), "A".to_string()),
                ("a.bsl".to_string(), "Z".to_string()),
                ("b.bsl".to_string(), "R".to_string()),
            ]
        );
    }

    #[test]
    fn maps_severity_to_codeclimate_scale() {
        assert_eq!(cq_severity("Blocker"), "blocker");
        assert_eq!(cq_severity("Critical"), "critical");
        assert_eq!(cq_severity("Error"), "critical");
        assert_eq!(cq_severity("Major"), "major");
        assert_eq!(cq_severity("Warning"), "major");
        assert_eq!(cq_severity("Information"), "minor");
        assert_eq!(cq_severity("Hint"), "info");
    }
}
