//! Stamps the commit this crate is built from into the binary (`BSL_BUILD_COMMIT`,
//! `BSL_BUILD_COMMITTED`), so a defect report can name the code that produced it.
//!
//! Everything here is best effort: a build must never fail because git is missing or
//! because the source tree is not a repository (source tarballs are built that way).

use std::path::Path;
use std::process::Command;

fn main() {
    track_head();

    // Both or neither: a date without a commit identifies nothing, and a commit without a
    // date cannot be placed in time. No `unknown` placeholder either - an absent field is
    // honest, a fabricated one is read as a real build identity.
    if let Some((sha, date)) = commit_identity() {
        println!("cargo:rustc-env=BSL_BUILD_COMMIT={sha}");
        println!("cargo:rustc-env=BSL_BUILD_COMMITTED={date}");
    }
}

/// Run git and return its trimmed stdout, or `None` on spawn failure, non-zero exit or
/// empty output.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn commit_identity() -> Option<(String, String)> {
    let sha = git(&["rev-parse", "--short=12", "HEAD"])?;
    let sha_ok = (7..=40).contains(&sha.len())
        && sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !sha_ok {
        return None;
    }
    let date = git(&["log", "-1", "--format=%cI"])?;
    if date.chars().take(4).filter(char::is_ascii_digit).count() != 4 {
        return None;
    }
    Some((sha, date))
}

/// Ask git where HEAD lives instead of building `../../.git/HEAD` by hand: in a linked
/// worktree `.git` is a FILE pointing elsewhere, so a hand-built path never exists and the
/// stamped commit would go stale when HEAD moves.
fn track_head() {
    for name in ["HEAD", "logs/HEAD"] {
        if let Some(path) = existing_git_path(name) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // HEAD itself does not change on a commit to a branch; the branch ref does. A packed
    // ref has no file of its own, so fall back to `packed-refs`. A commit on a packed branch
    // writes a new loose ref and leaves `packed-refs` alone; the reflog above records it,
    // unless reflogs are disabled.
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &branch]) {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            } else if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"]) {
                println!("cargo:rerun-if-changed={packed}");
            }
        }
    }
}

/// A path inside the git directory of this worktree, only when the file exists: cargo
/// reruns the script on every build for a tracked path that does not exist.
fn existing_git_path(name: &str) -> Option<String> {
    git(&["rev-parse", "--git-path", name]).filter(|path| Path::new(path).exists())
}
