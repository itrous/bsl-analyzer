#![cfg(target_os = "linux")]

use std::io::{BufWriter, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ide::diagnostics_baseline::{
    diagnostic_fingerprint, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
};
use ide::partitioned_diagnostics_baseline::migrate_v1_reader;
use project_model::{
    DiagnosticsBaselinePartition, DiagnosticsBaselinePartitionIdentity,
    DiagnosticsBaselinePartitionPlan, DiagnosticsBaselineProjectExtension,
    DiagnosticsBaselineProjectScope, DiagnosticsBaselineRootOwner, DiagnosticsBaselineSelection,
};

const RECORDS: usize = 1_600_000;
const ENABLED_RECORDS: usize = RECORDS / 10;
const TEST_NAME: &str = "large_selective_v1_migration_streams_skipped_entries_with_bounded_rss";

fn plan(selection: DiagnosticsBaselineSelection) -> DiagnosticsBaselinePartitionPlan {
    DiagnosticsBaselinePartitionPlan {
        project_scope: DiagnosticsBaselineProjectScope {
            source_root: Some("src/cf".to_owned()),
            extensions: vec![DiagnosticsBaselineProjectExtension {
                name: "Ext".to_owned(),
                path: "src/cfe/Ext".to_owned(),
                scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                legacy_scope: None,
                dependency_scopes: vec![],
                depends_on: vec![],
            }],
            externals: vec![],
        },
        project_scope_fingerprint: "a".repeat(64),
        selection_fingerprint: match selection {
            DiagnosticsBaselineSelection::All => "b".repeat(64),
            DiagnosticsBaselineSelection::Selective => "c".repeat(64),
        },
        partitions: vec![
            DiagnosticsBaselinePartition {
                id: "main".to_owned(),
                key: "main".to_owned(),
                identity: DiagnosticsBaselinePartitionIdentity::Main { path: "src/cf".to_owned() },
            },
            DiagnosticsBaselinePartition {
                id: "extension:Ext".to_owned(),
                key: "ext".to_owned(),
                identity: DiagnosticsBaselinePartitionIdentity::Extension {
                    name: "Ext".to_owned(),
                    path: "src/cfe/Ext".to_owned(),
                    scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                    legacy_scope: None,
                    dependency_scopes: vec![],
                    depends_on: vec![],
                },
            },
        ],
        enabled_partition_ids: match selection {
            DiagnosticsBaselineSelection::All => {
                vec!["main".to_owned(), "extension:Ext".to_owned()]
            }
            DiagnosticsBaselineSelection::Selective => vec!["main".to_owned()],
        },
        selection,
        roots: vec![
            DiagnosticsBaselineRootOwner {
                root: "src/cfe/Ext".to_owned(),
                partition_id: "extension:Ext".to_owned(),
            },
            DiagnosticsBaselineRootOwner {
                root: "src/cf".to_owned(),
                partition_id: "main".to_owned(),
            },
        ],
    }
}

fn child_peak_rss(child: &mut Child) -> (std::process::ExitStatus, u64) {
    // VmHWM, not sampled VmRSS: the kernel keeps the true high-water mark, while polling
    // misses any peak shorter than the sampling interval — exactly the regression these
    // gates exist to catch (a partition buffered whole for a few milliseconds). Read
    // just before reaping, since /proc disappears with the process.
    let mut peak = 0;
    loop {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", child.id())) {
            if let Some(hwm) = status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")?.split_whitespace().next()?.parse::<u64>().ok()
            }) {
                peak = peak.max(hwm * 1024);
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            return (status, peak);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_child(mode: &str, root: &std::path::Path) -> u64 {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([TEST_NAME, "--ignored", "--exact", "--nocapture"])
        .env("BSL_SELECTIVE_MIGRATION_SCALE_MODE", mode)
        .env("BSL_SELECTIVE_MIGRATION_SCALE_ROOT", root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let (status, peak) = child_peak_rss(&mut child);
    assert!(status.success(), "{mode} migration child failed");
    assert!(peak > 0, "{mode} migration child RSS was never observed");
    peak
}

fn child_mode(mode: &str, root: &std::path::Path) {
    // The idle child does nothing but start: its peak IS the process floor, which both
    // real modes carry too. Comparing raw peaks would measure the test binary's startup
    // footprint alongside the code under test, and the ratio would drift with it.
    if mode == "idle" {
        std::thread::sleep(Duration::from_millis(150));
        return;
    }
    let selection = if mode == "full" {
        DiagnosticsBaselineSelection::All
    } else {
        DiagnosticsBaselineSelection::Selective
    };
    let stats = migrate_v1_reader(
        std::fs::File::open(root.join("legacy.json")).unwrap(),
        &plan(selection),
        |_, entry| {
            std::hint::black_box(entry);
            Ok(())
        },
    )
    .unwrap();
    if mode == "full" {
        assert_eq!(stats.migrated, RECORDS);
        assert_eq!(stats.skipped_unsuppressed, 0);
    } else {
        assert_eq!(stats.migrated, ENABLED_RECORDS);
        assert_eq!(stats.skipped_unsuppressed, RECORDS - ENABLED_RECORDS);
    }
    assert_eq!(stats.migrated + stats.skipped_unsuppressed, RECORDS);
    std::hint::black_box(stats);
    std::thread::sleep(Duration::from_millis(150));
}

#[test]
#[ignore = "release-only paired 1.6M-entry selective migration gate"]
fn large_selective_v1_migration_streams_skipped_entries_with_bounded_rss() {
    if let Ok(mode) = std::env::var("BSL_SELECTIVE_MIGRATION_SCALE_MODE") {
        child_mode(
            &mode,
            &std::path::PathBuf::from(
                std::env::var_os("BSL_SELECTIVE_MIGRATION_SCALE_ROOT").unwrap(),
            ),
        );
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut writer = BufWriter::new(std::fs::File::create(root.join("legacy.json")).unwrap());
    writer
        .write_all(
            br#"{"schema_version":1,"scope":{"source_root":"src/cf","extensions":[{"name":"Ext","path":"src/cfe/Ext","scope":"InfoBase","depends_on":[]}]},"diagnostics":["#,
        )
        .unwrap();
    for index in 0..RECORDS {
        if index > 0 {
            writer.write_all(b",").unwrap();
        }
        // Zero-padded so the paths sort in index order: streaming migration cannot sort,
        // so it rejects a legacy set whose entries are out of canonical order, and
        // unpadded names put `modules/10.bsl` before `modules/2.bsl`.
        let path = if index < ENABLED_RECORDS {
            format!("src/cf/modules/{index:07}.bsl")
        } else {
            format!("src/cfe/Ext/modules/{index:07}.bsl")
        };
        let snippet = format!("Message({index});");
        serde_json::to_writer(
            &mut writer,
            &DiagnosticsBaselineEntry {
                fingerprint: diagnostic_fingerprint(&path, "LineLength", &snippet, 0),
                path,
                code: "LineLength".to_owned(),
                snippet,
                occurrence: 0,
                message: "m".to_owned(),
                severity: "Warning".to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 1,
                },
            },
        )
        .unwrap();
    }
    writer.write_all(b"]}\n").unwrap();
    writer.flush().unwrap();

    let floor = run_child("idle", root);
    let full_peak = run_child("full", root);
    let selective_peak = run_child("selective", root);
    let full_growth = full_peak.saturating_sub(floor);
    let selective_growth = selective_peak.saturating_sub(floor);
    eprintln!("floor={floor} full={full_peak} selective={selective_peak}");
    assert!(
        full_growth > 0,
        "positive control: the full migration must grow over an idle child ({full_peak} vs {floor})"
    );
    assert!(
        selective_growth <= full_growth / 4,
        "selective growth {selective_growth} exceeds 25% of full growth {full_growth} \
         (floor {floor})"
    );
}
