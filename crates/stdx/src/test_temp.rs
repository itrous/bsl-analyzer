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
        std::fs::create_dir_all(&root).expect("failed to create the test temp root");
        sweep_stale(&temp, &root, STALE_AFTER);
        root
    })
}

/// Fresh empty directory inside [`root`]; `name` stays in the path to show which test made it.
pub fn unique_dir(name: &str) -> PathBuf {
    let dir = root().join(format!("{name}-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).expect("failed to create a test temp directory");
    dir
}

// Every error is ignored: a sibling may belong to a live run, be locked, or be owned by another
// user, and cleanup of foreign leftovers must never fail a test.
fn sweep_stale(temp: &Path, own_root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(temp) else { return };
    let now = SystemTime::now();
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
    fn sweep_leaves_fresh_sibling_and_removes_stale() {
        let temp = unique_dir("sweep");
        let own = temp.join(format!("{PREFIX}own"));
        let fresh = temp.join(format!("{PREFIX}fresh"));
        let stale = temp.join(format!("{PREFIX}stale"));
        let other = temp.join("unrelated");
        for dir in [&own, &fresh, &stale, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A tiny max age stands in for an old mtime, which cannot be set without a new dependency.
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale(&temp, &own, Duration::from_millis(10));
        assert!(own.exists() && other.exists());
        assert!(!fresh.exists() && !stale.exists());

        let fresh_again = temp.join(format!("{PREFIX}fresh2"));
        std::fs::create_dir_all(&fresh_again).unwrap();
        sweep_stale(&temp, &own, STALE_AFTER);
        assert!(fresh_again.exists());
    }
}
