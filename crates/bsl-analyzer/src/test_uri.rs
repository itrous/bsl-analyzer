//! Absolute paths and `file:` URIs for tests, valid on every platform.
//!
//! A fixture written as `file:///rename.bsl` or `/tmp/test.bsl` is an ordinary absolute path on
//! Unix, but on Windows it carries no drive, so `Url::from_file_path` and `Url::to_file_path`
//! refuse it and every test that turns a URI into a VFS path fails there for a reason that has
//! nothing to do with what it asserts. Anchoring the fixture at a drive on Windows keeps the two
//! platforms equal without changing what the Unix side has always produced.

use std::path::PathBuf;

/// `C:\<rel>` on Windows, `/<rel>` elsewhere. `rel` is relative: it must not start with a separator.
pub(crate) fn file_path(rel: &str) -> PathBuf {
    debug_assert!(!rel.starts_with('/'), "fixture path is relative to the root: {rel}");
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"C:\{}", rel.replace('/', r"\")))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(format!("/{rel}"))
    }
}

/// The `file:` URI of [`file_path`].
pub(crate) fn file_uri(rel: &str) -> lsp_types::Url {
    lsp_types::Url::from_file_path(file_path(rel)).expect("fixture path must be absolute")
}
