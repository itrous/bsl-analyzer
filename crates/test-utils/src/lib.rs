pub mod wait;
pub mod walk_probe;

pub use expect_test::{expect, Expect};

/// Capture scoped tracing events even when another thread registers a callsite first.
pub fn with_subscriber<S, F, T>(subscriber: S, f: F) -> T
where
    S: tracing::Subscriber + Send + Sync + 'static,
    F: FnOnce() -> T,
{
    // Keep tracing-core's single-dispatch fast path off: a parallel thread without
    // a scoped subscriber must not cache a newly registered callsite as disabled.
    static DISPATCHES: std::sync::OnceLock<[tracing::Dispatch; 2]> = std::sync::OnceLock::new();
    DISPATCHES.get_or_init(|| {
        std::array::from_fn(
            |_| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        )
    });
    tracing::subscriber::with_default(subscriber, f)
}

/// Writes an executable through a child process. A file some process still holds open for
/// writing cannot be executed (`ETXTBSY`), and a child that another test thread forks while
/// this one writes holds the descriptor until it execs: a program written here and spawned
/// at once would start or not depending on what the other threads were doing.
#[cfg(unix)]
pub fn write_executable(path: &std::path::Path, text: &str) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, text).unwrap();
    let installed = std::process::Command::new("install")
        .args(["-m", "700"])
        .arg(&staged)
        .arg(path)
        .status()
        .unwrap();
    assert!(installed.success(), "install {}", path.display());
    std::fs::remove_file(&staged).unwrap();
}

pub fn check(actual: &str, expect: Expect) {
    expect.assert_eq(actual);
}

pub fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

pub fn extract_cursor(input: &str) -> (String, Option<usize>) {
    if let Some(pos) = input.find("$0") {
        let text = format!("{}{}", &input[..pos], &input[pos + 2..]);
        (text, Some(pos))
    } else {
        (input.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cursor() {
        let (text, pos) = extract_cursor("hello$0world");
        assert_eq!(text, "helloworld");
        assert_eq!(pos, Some(5));
    }
}
