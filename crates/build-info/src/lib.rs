//! Identity of the commit this binary was built from, stamped in by `build.rs`.
//!
//! A version number does not change between a day's builds, so a report that names only
//! the version cannot date the code it describes.
//!
//! A crate of its own, depended on only by the code that reports the identity: the stamp
//! changes on every commit and every branch switch, and cargo rebuilds every dependent of a
//! crate whose build script output changed.

/// The short commit hash the crate was built from, or `None` when the build could not
/// determine it (no git, or a source tree that is not a repository).
pub fn commit() -> Option<&'static str> {
    option_env!("BSL_BUILD_COMMIT")
}

/// That commit's committer date, RFC 3339. `Some` exactly when [`commit`] is `Some`.
pub fn committed() -> Option<&'static str> {
    option_env!("BSL_BUILD_COMMITTED")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_and_date_are_stamped_together() {
        assert_eq!(
            commit().is_some(),
            committed().is_some(),
            "the build script must emit both BSL_BUILD_COMMIT and BSL_BUILD_COMMITTED or neither",
        );
    }

    #[test]
    fn a_stamped_commit_is_short_lowercase_hex() {
        // Meaningful either way: a stamped build is checked for shape, and an unstamped one
        // (no git at build time) has nothing to check, which is the honest outcome.
        if let Some(commit) = commit() {
            assert!((7..=40).contains(&commit.len()), "unexpected length: {commit}");
            assert!(
                commit.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "not lowercase hex: {commit}",
            );
        }
    }

    #[test]
    fn a_stamped_date_starts_with_a_year() {
        if let Some(date) = committed() {
            let mut chars = date.chars();
            let year: Vec<char> = chars.by_ref().take(4).collect();
            assert!(
                year.len() == 4 && year.iter().all(char::is_ascii_digit),
                "no leading year: {date}",
            );
            assert_eq!(chars.next(), Some('-'), "year not followed by '-': {date}");
        }
    }
}
