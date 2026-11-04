use serde::Serialize;

use ide_diagnostics::DiagnosticOutput;

#[derive(Debug, Clone, Serialize)]
pub struct StartEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,

    pub total_files: usize,

    pub version: &'static str,

    // The stream is read by consumers that never see `--help`, and `"start_line": 200`
    // reads as "line 200"; a `lines[start_line - 1]` lookup then lands on the previous
    // line and the correct finding looks wrong. Fixed in `new`, not a parameter, so no
    // caller can claim another base.
    line_base: u8,

    column_base: u8,
}

impl StartEvent {
    pub fn new(total_files: usize) -> Self {
        Self {
            event_type: "start",
            total_files,
            version: env!("CARGO_PKG_VERSION"),
            line_base: 0,
            column_base: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FileMetrics {
    pub functions: usize,

    pub complexity: u32,

    pub cognitive_complexity: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,

    pub path: String,

    pub diagnostics: Vec<DiagnosticOutput>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<FileMetrics>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FileEvent {
    pub fn new(
        path: String,
        diagnostics: Vec<DiagnosticOutput>,
        metrics: Option<FileMetrics>,
        error: Option<String>,
    ) -> Self {
        Self { event_type: "file", path, diagnostics, metrics, error }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoneEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,

    pub elapsed_secs: f64,

    pub total_files: usize,

    pub total_diagnostics: usize,

    pub failed_files: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<crate::diagnostics_baseline::DiagnosticsBaselineSummary>,
}

impl DoneEvent {
    pub fn new(
        elapsed_secs: f64,
        total_files: usize,
        total_diagnostics: usize,
        failed_files: usize,
    ) -> Self {
        Self {
            event_type: "done",
            elapsed_secs,
            total_files,
            total_diagnostics,
            failed_files,
            baseline: None,
        }
    }

    pub fn with_baseline(
        mut self,
        baseline: crate::diagnostics_baseline::DiagnosticsBaselineSummary,
    ) -> Self {
        self.baseline = Some(baseline);
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct JsonlSummary {
    pub total_files: usize,

    pub total_diagnostics: usize,

    pub failed_files: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_event_serialization() {
        let event = StartEvent::new(100);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"start""#));
        assert!(json.contains(r#""total_files":100"#));
        assert!(json.contains(r#""version":"#));
        assert!(json.ends_with(r#""line_base":0,"column_base":0}"#), "{json}");
    }

    #[test]
    fn test_file_event_serialization() {
        let event = FileEvent::new(
            "src/Module.bsl".to_string(),
            vec![DiagnosticOutput {
                code: "LineLength".to_string(),
                message: "Line too long".to_string(),
                severity: "Warning".to_string(),
                start_line: 10,
                start_column: 0,
                end_line: 10,
                end_column: 150,
                tags: vec![],
            }],
            Some(FileMetrics { functions: 5, complexity: 10, cognitive_complexity: 8 }),
            None,
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"file""#));
        assert!(json.contains(r#""path":"src/Module.bsl""#));
        assert!(json.contains(r#""diagnostics":"#));
        assert!(json.contains(r#""metrics":"#));
        assert!(!json.contains(r#""error""#));
    }

    #[test]
    fn test_file_event_without_metrics() {
        let event = FileEvent::new("src/Module.bsl".to_string(), vec![], None, None);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains(r#""metrics""#));
    }

    #[test]
    fn test_file_event_with_error() {
        let event = FileEvent::new(
            "src/Module.bsl".to_string(),
            vec![],
            None,
            Some("Parse error".to_string()),
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""error":"Parse error""#));
    }

    #[test]
    fn test_done_event_serialization() {
        let event = DoneEvent::new(11.2, 6540, 1234, 5);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"done""#));
        assert!(json.contains(r#""elapsed_secs":11.2"#));
        assert!(json.contains(r#""total_files":6540"#));
        assert!(json.contains(r#""total_diagnostics":1234"#));
        assert!(json.contains(r#""failed_files":5"#));
    }

    #[test]
    fn jsonl_baseline_summary_extends_only_done_and_preserves_failed_files() {
        use crate::diagnostics_baseline::{DiagnosticsBaselineState, DiagnosticsBaselineSummary};

        for state in [
            DiagnosticsBaselineState::Disabled,
            DiagnosticsBaselineState::Full,
            DiagnosticsBaselineState::Partial,
        ] {
            let baseline = if state == DiagnosticsBaselineState::Disabled {
                DiagnosticsBaselineSummary::disabled()
            } else {
                DiagnosticsBaselineSummary {
                    state,
                    selection: None,
                    partitions_enabled: None,
                    partitions_unsuppressed: None,
                    unsuppressed: None,
                    new: Some(1),
                    known: Some(2),
                    resolved: Some(3),
                    path: Some("baseline.json".to_owned()),
                    schema_version: Some(1),
                    manifest_schema_version: None,
                    complete: state == DiagnosticsBaselineState::Full,
                    error_code: None,
                    detail: None,
                    partitions: vec![],
                    errors: vec![],
                }
            };
            let value =
                serde_json::to_value(DoneEvent::new(1.0, 4, 5, 2).with_baseline(baseline)).unwrap();
            assert_eq!(value["failed_files"], 2);
            assert!(value["baseline"]["state"].is_string());
        }

        assert!(serde_json::to_value(StartEvent::new(1)).unwrap().get("baseline").is_none());
        assert!(serde_json::to_value(FileEvent::new("x".into(), vec![], None, None))
            .unwrap()
            .get("baseline")
            .is_none());
    }
}
