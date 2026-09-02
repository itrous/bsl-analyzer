use std::path::{Path, PathBuf};

const BASE: &str = "f8bf4da5831840070aa19477be68e74d78014fa6";

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_name().is_none_or(|name| name != "inventory.rs")
        {
            out.push(path);
        }
    }
}

fn production_sources() -> Vec<PathBuf> {
    let mut sources = Vec::new();
    rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut sources);
    sources
}

fn production_prefix(source: &str) -> &str {
    source.split("\n#[cfg(test)]\nmod ").next().unwrap_or(source)
}

#[test]
fn no_generic_or_unclassified_production_lease_callers() {
    let forbidden = ["with_ownership_outcome", "with_ownership_checkpointed", "LeaseOutcome"];
    for path in production_sources() {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        for token in forbidden {
            assert!(!source.contains(token), "{} still contains {token}", path.display());
        }
    }
}

#[test]
fn prepared_work_stays_outside_lease_fences() {
    let allowed = [
        "graph/build.rs",
        "graph/snapshot.rs",
        "graph/state.rs",
        "state/bootstrap.rs",
        "state/embed.rs",
        "state/mod.rs",
        "workspace_lease.rs",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in production_sources() {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        if source.contains(".publish_short(") || source.contains(".publish_checkpointed(") {
            let relative = path.strip_prefix(&root).unwrap().to_string_lossy();
            assert!(allowed.contains(&relative.as_ref()), "unclassified fence caller: {relative}");
        }
    }
}

#[test]
fn no_new_runtime_or_compatibility_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root is readable");
    let cargo_lock = std::process::Command::new("git")
        .args(["diff", "--exit-code", BASE, "--", "Cargo.lock"])
        .current_dir(&root)
        .status()
        .expect("git is available");
    assert!(cargo_lock.success(), "Cargo.lock changed from the locked base");

    let diff = std::process::Command::new("git")
        .args([
            "diff",
            "--unified=0",
            BASE,
            "--",
            "crates/bsl-search/src",
            "crates/mcp-server/src",
            ":(exclude)crates/mcp-server/src/inventory.rs",
        ])
        .current_dir(&root)
        .output()
        .expect("git is available");
    assert!(diff.status.success(), "git diff failed");
    let added = String::from_utf8(diff.stdout)
        .expect("git diff is UTF-8")
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in
        ["SCHEMA_VERSION_CURRENT", "ALTER TABLE", "CREATE TABLE", "Serialize", "Deserialize"]
    {
        assert!(!added.contains(forbidden), "new compatibility/runtime surface: {forbidden}");
    }

    for path in production_sources() {
        let relative = path.strip_prefix(&root).expect("source is under repository root");
        let current = std::fs::read_to_string(&path).expect("Rust source is readable");
        let previous = std::process::Command::new("git")
            .args(["show", &format!("{BASE}:{}", relative.display())])
            .current_dir(&root)
            .output()
            .expect("git is available");
        let previous = String::from_utf8(previous.stdout).expect("base source is UTF-8");
        for token in ["tokio::spawn", "std::thread::spawn", "thread::Builder::new"] {
            assert!(
                production_prefix(&current).matches(token).count()
                    <= production_prefix(&previous).matches(token).count(),
                "new production runtime surface in {}: {token}",
                relative.display()
            );
        }
    }
}
