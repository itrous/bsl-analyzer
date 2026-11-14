//! A workspace for the finality tests of the LSP surfaces.
//!
//! One module holds three identical self-assignments a, b and c on 1-based lines 2, 3
//! and 4. The baseline records the first `recorded` of them by occurrence, and the scope
//! names the hunks a test chooses. With a and b recorded and a scope of lines 3–4 the one
//! visible finding is c: a classification of the scoped set alone would number c as 1 and
//! hide it, and with no baseline b and c are both visible.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use base_db::{AnalysisScope, DiagnosticsConfigInput, Locale};
use ide::diagnostics_baseline::{
    diagnostic_fingerprint, DiagnosticsBaseline, DiagnosticsBaselineEntry,
    DiagnosticsBaselineRange, DiagnosticsBaselineScope, DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
};
use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;
use vfs::VfsPath;

use crate::global_state::GlobalState;

pub(crate) const MODULE: &str =
    "Процедура Тест()\n    А = А;\n    А = А;\n    А = А;\nКонецПроцедуры\n";
pub(crate) const RELATIVE: &str = "src/cf/CommonModules/М/Ext/Module.bsl";
const SNIPPET: &str = "А = А;";

/// The baseline's form, and for the partitioned one the policy of the module's partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    V1,
    Set,
    Unsuppressed,
}

pub(crate) struct Workspace {
    _dir: tempfile::TempDir,
    pub(crate) root: PathBuf,
    pub(crate) module: PathBuf,
    form: Form,
}

impl Workspace {
    pub(crate) fn new(form: Form) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, because the temp directory may sit behind a link (`/var` on macOS), and in
        // the spelling the rest of the server uses: `canonicalize` alone answers with the
        // Windows verbatim prefix, which a `file:` URL round-trip drops — the scope key and the
        // path the pull handlers derive from the URI would then name the same file differently.
        let root = stdx::path_exclusion::resolve(dir.path()).unwrap();
        for source in ["src/cf", "src/cfe/Ext"] {
            std::fs::create_dir_all(root.join(source)).unwrap();
            std::fs::write(root.join(source).join("Configuration.xml"), "<Configuration/>")
                .unwrap();
        }
        let baseline = match form {
            Form::V1 => "path = \"baseline.json\"\n",
            Form::Set => "directory = \"baselines\"\n",
            Form::Unsuppressed => "directory = \"baselines\"\ninclude = [\"extension:Ext\"]\n",
        };
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            format!(
                "[source]\nroot = \"src/cf\"\n\
                 extensions = [{{ name = \"Ext\", path = \"src/cfe/Ext\", scope = \"InfoBase\" }}]\n\
                 [diagnostics.baseline]\n{baseline}"
            ),
        )
        .unwrap();
        let module = root.join(RELATIVE);
        std::fs::create_dir_all(module.parent().unwrap()).unwrap();
        std::fs::write(&module, MODULE).unwrap();
        Self { _dir: dir, root, module, form }
    }

    pub(crate) fn uri(&self) -> lsp_types::Url {
        lsp_types::Url::from_file_path(&self.module).unwrap()
    }

    pub(crate) fn project(&self) -> project_model::Project {
        let mut project = project_model::Project::new(&self.root).unwrap();
        project.config.features.workspace_diagnostics =
            project_model::WorkspaceDiagnosticsScope::All;
        project
    }

    /// The scope of the module with `hunks` (1-based, inclusive); `None` for the whole file.
    pub(crate) fn scope(&self, hunks: Option<Vec<[u32; 2]>>) -> Arc<AnalysisScope> {
        Arc::new(AnalysisScope::from_report("vendor", &self.root, [(RELATIVE.to_owned(), hunks)]))
    }

    /// A ready snapshot recording occurrences `0..recorded` of the module's self-assignment.
    pub(crate) fn snapshot(&self, recorded: u32) -> DiagnosticsBaselineSnapshot {
        let entries: Vec<_> = (0..recorded).map(entry).collect();
        match self.form {
            Form::V1 => DiagnosticsBaselineSnapshot::Ready {
                baseline: DiagnosticsBaseline {
                    schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                    scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
                    diagnostics: entries,
                },
                project_path: "baseline.json".to_owned(),
                path: self.root.join("baseline.json"),
                epoch: format!("v1-{recorded}"),
                ground: Default::default(),
            },
            Form::Set | Form::Unsuppressed => self.load_set(entries),
        }
    }

    fn load_set(&self, entries: Vec<DiagnosticsBaselineEntry>) -> DiagnosticsBaselineSnapshot {
        use ide::partitioned_diagnostics_baseline::{
            diagnostics_manifest, diagnostics_manifest_json, diagnostics_partition_json,
            partition_object_path, DiagnosticsBaselineManifestEntry,
        };

        let _ = std::fs::remove_dir_all(self.root.join("baselines"));
        let project = project_model::Project::new(&self.root).unwrap();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let directory =
            project_model::ManagedBaselineDirectory::open(&self.root, "baselines", true).unwrap();
        let mut manifest_entries = Vec::new();
        for partition in &plan.partitions {
            if !plan.enabled_partition_ids.contains(&partition.id) {
                continue;
            }
            let diagnostics = if partition.id == "main" { entries.clone() } else { vec![] };
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
        assert!(snapshot.affects_diagnostics(), "the fixture set must load: {snapshot:?}");
        snapshot
    }

    /// A loaded server state serving the module, open in the editor when `open`.
    pub(crate) fn state(
        &self,
        snapshot: DiagnosticsBaselineSnapshot,
        scope: Option<Arc<AnalysisScope>>,
        open: bool,
    ) -> GlobalState {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.vfs_done = true;
        state.workspace_root = Some(self.root.clone());
        state.project = Some(self.project());
        state.diagnostics_baseline = Arc::new(snapshot);
        state.diagnostics_config = config(scope);
        let uri = self.uri();
        if open {
            state.mem_docs.insert(uri.clone(), MODULE.to_owned(), 1);
            let file_id = state.vfs_file_for_url(&uri).unwrap();
            state.open_files.insert(file_id);
        }
        state
            .vfs
            .write()
            .set_file_contents(VfsPath::new(self.module.clone()), Some(Arc::from(MODULE)));
        state.process_changes(false);
        state
    }
}

/// The diagnostics configuration of the fixture: every default rule, with `SelfAssign`
/// named explicitly so the counterexample does not rest on its default.
pub(crate) fn config(scope: Option<Arc<AnalysisScope>>) -> DiagnosticsConfigInput {
    DiagnosticsConfigInput::from_raw(
        Vec::<String>::new(),
        vec!["SelfAssign".to_owned()],
        Vec::<(String, String)>::new(),
        false,
        hir::dataflow::DEFAULT_MAX_ITERATIONS,
        Locale::Ru,
        true,
    )
    .with_scope(scope)
}

fn entry(occurrence: u32) -> DiagnosticsBaselineEntry {
    let line = occurrence + 1;
    DiagnosticsBaselineEntry {
        fingerprint: diagnostic_fingerprint(RELATIVE, "SelfAssign", SNIPPET, occurrence),
        path: RELATIVE.to_owned(),
        code: "SelfAssign".to_owned(),
        snippet: SNIPPET.to_owned(),
        occurrence,
        message: "SelfAssign".to_owned(),
        severity: "warning".to_owned(),
        range: DiagnosticsBaselineRange {
            start_line: line,
            start_column: 4,
            end_line: line,
            end_column: 10,
        },
    }
}

/// The 1-based lines of the self-assignments among `diagnostics`.
pub(crate) fn self_assign_lines(diagnostics: &[ide::Diagnostic]) -> Vec<usize> {
    diagnostics
        .iter()
        .filter(|d| d.code == ide::DiagnosticCode::SelfAssign)
        .map(|d| MODULE[..usize::from(d.range.start())].matches('\n').count() + 1)
        .collect()
}

/// The 1-based lines of the self-assignments among published LSP diagnostics.
pub(crate) fn published_self_assign_lines(diagnostics: &[lsp_types::Diagnostic]) -> Vec<u32> {
    diagnostics
        .iter()
        .filter(|d| {
            matches!(&d.code, Some(lsp_types::NumberOrString::String(code)) if code == "SelfAssign")
        })
        .map(|d| d.range.start.line + 1)
        .collect()
}
