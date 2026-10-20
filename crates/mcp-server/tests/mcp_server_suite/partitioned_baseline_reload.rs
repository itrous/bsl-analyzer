use std::{fs, io::Write, sync::Arc};

use ide::diagnostics_baseline::{
    diagnostic_fingerprint, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
};
use ide::partitioned_diagnostics_baseline::{
    diagnostics_manifest, diagnostics_manifest_json, diagnostics_partition_json,
    partition_object_path, DiagnosticsBaselineManifestEntry,
};

#[test]
fn partitioned_baseline_reload_reuses_unchanged_arcs_and_observes_every_object() {
    let root = tempfile::tempdir().unwrap();
    for source in ["src/cf", "src/cfe/Ext"] {
        fs::create_dir_all(root.path().join(source)).unwrap();
        fs::write(root.path().join(source).join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    let config: project_model::ProjectConfig = serde_json::from_value(serde_json::json!({
        "configurationRoot": "src/cf",
        "extensions": [{"name": "Ext", "path": "src/cfe/Ext", "scope": "InfoBase"}],
        "diagnostics": {"baseline": {"directory": "baselines"}}
    }))
    .unwrap();
    let project = project_model::Project::with_config(root.path(), config).unwrap();
    let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
    let directory =
        project_model::ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();

    let publish = |main_entries: Vec<DiagnosticsBaselineEntry>| {
        let mut manifest_entries = Vec::new();
        for partition in &plan.partitions {
            let entries = if partition.id == "main" { main_entries.clone() } else { vec![] };
            let bytes = diagnostics_partition_json(partition.identity.clone(), entries).unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let path = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
            if directory.open_file(&path).is_err() {
                directory.create_file_new(&path).unwrap().write_all(&bytes).unwrap();
            }
            manifest_entries.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file: path,
                blake3: hash,
            });
        }
        let manifest =
            diagnostics_manifest(plan.project_scope_fingerprint.clone(), manifest_entries);
        let bytes = diagnostics_manifest_json(&manifest).unwrap();
        let temp = "manifest.next.json";
        if directory.open_file(temp).is_ok() {
            directory.remove_file(temp).unwrap();
        }
        directory.create_file_new(temp).unwrap().write_all(&bytes).unwrap();
        directory.replace_file(temp, "manifest.json").unwrap();
        manifest
    };

    let first_manifest = publish(vec![]);
    let first = ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(&project);
    let (first_set, _, _) = first.ready_set().unwrap();
    let old_main = first_set.partitions["main"].clone();
    let old_extension = first_set.partitions["extension:Ext"].clone();
    let first_observation = first.observation();

    let path = "src/cf/Main.bsl";
    let snippet = "Message(1);";
    let entry = DiagnosticsBaselineEntry {
        fingerprint: diagnostic_fingerprint(path, "LineLength", snippet, 0),
        path: path.to_owned(),
        code: "LineLength".to_owned(),
        snippet: snippet.to_owned(),
        occurrence: 0,
        message: "message".to_owned(),
        severity: "Warning".to_owned(),
        range: DiagnosticsBaselineRange {
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 1,
        },
    };
    let second_manifest = publish(vec![entry]);
    assert_ne!(first_manifest.generation, second_manifest.generation);
    let second = ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load_reusing(
        &project, &first,
    );
    let (second_set, _, _) = second.ready_set().unwrap();
    assert!(!Arc::ptr_eq(&old_main, &second_set.partitions["main"]));
    assert!(Arc::ptr_eq(&old_extension, &second_set.partitions["extension:Ext"]));
    assert_ne!(first_observation, second.observation());

    let extension = second_manifest
        .partitions
        .iter()
        .find(|entry| entry.partition_id == "extension:Ext")
        .unwrap();
    let extension_path = root.path().join("baselines").join(&extension.file);
    let valid = fs::read(&extension_path).unwrap();
    fs::write(&extension_path, b"{}\n").unwrap();
    let broken = ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load_reusing(
        &project, &second,
    );
    assert!(matches!(
        broken,
        ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::Error { .. }
    ));
    assert!(!broken.moved_since_load(&project), "an untouched set reads the same");
    fs::write(extension_path, valid).unwrap();
    assert!(broken.moved_since_load(&project), "the repair has to reach the host");
    assert!(ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load_reusing(
        &project, &broken,
    )
    .ready_set()
    .is_some());
}
