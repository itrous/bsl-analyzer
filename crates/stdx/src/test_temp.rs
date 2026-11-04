use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const PREFIX: &str = "bsl-tests-";
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-process scratch root `<system temp>/bsl-tests-<pid>`.
///
/// The root is deliberately not removed at process exit: a failing test is easier to debug when
/// its files survive, and the 24-hour sweep of sibling roots bounds the disk use.
pub fn root() -> &'static Path {
    ROOT.get_or_init(|| {
        let temp = std::env::temp_dir();
        let root = temp.join(format!("{PREFIX}{}", std::process::id()));
        prepare_root(&root);
        sweep_stale(&temp, &root, SystemTime::now(), STALE_AFTER);
        root
    })
}

/// Fresh empty directory inside [`root`]; `name` stays in the path to show which test made it.
pub fn unique_dir(name: &str) -> PathBuf {
    let dir = root().join(format!("{name}-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).expect("failed to create a test temp directory");
    dir
}

// A root that already exists was left by a finished process with the same pid, and reusing it
// would break the promise of `unique_dir`, whose counter starts over in every process.
fn prepare_root(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).expect("failed to create the test temp root");
}

// Every error is ignored: a sibling may belong to a live run, be locked, or be owned by another
// user, and cleanup of foreign leftovers must never fail a test.
fn sweep_stale(temp: &Path, own_root: &Path, now: SystemTime, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(temp) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own_root || !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else { continue };
        if now.duration_since(modified).is_ok_and(|age| age > max_age) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_stable_and_pid_named() {
        assert_eq!(root(), root());
        assert!(root().is_dir());
        assert!(root().ends_with(format!("{PREFIX}{}", std::process::id())));
    }

    #[test]
    fn unique_dirs_differ() {
        let a = unique_dir("same");
        let b = unique_dir("same");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        assert!(a.starts_with(root()));
    }

    #[test]
    fn sweep_removes_only_stale_prefixed_siblings() {
        let temp = unique_dir("sweep");
        let own = temp.join(format!("{PREFIX}own"));
        let sibling = temp.join(format!("{PREFIX}sibling"));
        let other = temp.join("unrelated");
        for dir in [&own, &sibling, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let now = SystemTime::now();
        sweep_stale(&temp, &own, now, STALE_AFTER);
        assert!(sibling.exists(), "a sibling younger than the limit stays");

        sweep_stale(&temp, &own, now + 2 * STALE_AFTER, STALE_AFTER);
        assert!(!sibling.exists(), "a sibling older than the limit goes");
        assert!(own.exists() && other.exists());
    }

    // Roots outlive their process, so a later process can get the same pid and find the old root.
    #[test]
    fn a_reused_pid_root_starts_empty() {
        let root = unique_dir("reused-pid").join(format!("{PREFIX}7"));
        prepare_root(&root);
        std::fs::write(root.join("leftover"), "").unwrap();

        prepare_root(&root);

        assert!(root.is_dir());
        assert!(!root.join("leftover").exists());
    }
}
