//! What every file-addressed tool has to agree on: how a `(root_id, path)` pair names a
//! file, which pair the answer carries back, and what a failure to reach the file is called.
//!
//! The pair is the key, so two tools that read it differently address different files while
//! reporting the same address — a wrong answer wearing the shape of a right one. The rules
//! live here once rather than in each tool, and the vocabulary of failures is shared for the
//! same reason: `diagnostics file` and `outline` answering one input with two different codes
//! would make the codes stop being a vocabulary.
//!
//! What is deliberately NOT shared is the prose. Each tool's `detail` describes what THAT
//! tool does with a file it cannot serve — the resident holds an unreadable file out of
//! service and re-reads it every drift window, a one-shot parse does neither — so the text
//! stays at the call site and only the code comes from here.

use std::path::{Path, PathBuf};

use bsl_search::WorkspaceRoots;
use serde_json::{json, Value};

use crate::tools::location as loc;

/// Why a `(root_id, path)` pair names no file. Every case is answered, never guessed: a pair
/// that cannot be resolved and a pair resolved against some other root are indistinguishable
/// to the caller, and the second is a wrong answer wearing the shape of a right one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootedPathError {
    /// No root is registered under this id. It is the caller's spelling that is wrong, or
    /// the workspace declares a different set of extensions than the index the caller read.
    RootNotRegistered(String),
    /// An absolute path carries its own root, and a second one cannot be honoured: joining a
    /// root with an absolute path discards the root silently, so the pair would be answered
    /// from whichever file the path alone names.
    AbsolutePathWithRootId(String),
    /// The path is not a plain relative name under the root: it carries `..`, `.`, a leading
    /// separator or a drive. Each of those is a way of naming something the root does not
    /// contain — `Path::join` replaces its base outright for the last two — and `..` cannot be
    /// resolved here at all, because the kernel collapses it only after dereferencing each
    /// component, so any answer computed here would disagree with the file that opens.
    PathIsNotPlainRelative(String),
    /// The id is registered, but the root it names can no longer be resolved to the directory the
    /// table minted it from. The same `(root_id, path)` then names one file to this table and
    /// another to the workspace — an id survives a retarget — so the pair is refused rather than
    /// answered from a directory the table cannot vouch for. The cause is deliberately not named:
    /// a retarget is one of the ways the declared spelling stops resolving, an unreachable target
    /// (unmounted, not yet permitted) is another, and this error reports the observation.
    RegistrationStale(String),
}

impl RootedPathError {
    /// The in-band error code. Two different facts never share one code: an unregistered root
    /// and a pair that cannot be honoured call for different corrections, and one name for
    /// both would send the reader looking for the wrong thing.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::RootNotRegistered(_) => "unknown_root",
            Self::AbsolutePathWithRootId(_) => "absolute_path_under_root",
            Self::PathIsNotPlainRelative(_) => "path_not_relative_to_root",
            Self::RegistrationStale(_) => "stale_root_registration",
        }
    }
}

impl std::fmt::Display for RootedPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootNotRegistered(root_id) => write!(
                f,
                "no source root is registered under '{root_id}'; \
                 `search` reports the roots this workspace knows"
            ),
            Self::AbsolutePathWithRootId(root_id) => write!(
                f,
                "an absolute path already names its file, so it cannot also be read under \
                 root '{root_id}'; pass the path relative to that root, or drop `root_id`"
            ),
            Self::PathIsNotPlainRelative(root_id) => write!(
                f,
                "a path read against root '{root_id}' has to be plain relative names — no `..`, \
                 no `.`, no leading separator and no drive; spell the file as the hit does"
            ),
            Self::RegistrationStale(root_id) => write!(
                f,
                "source root '{root_id}' can no longer be resolved to the directory it was \
                 registered with; the resident's view may be stale — reload the workspace"
            ),
        }
    }
}

/// The file a request names, given the root its path is spelled against.
///
/// A search hit's path is relative to ITS OWN root, so the pair is what identifies the
/// file; the path alone is ambiguous the moment an extension repeats the configuration's
/// layout. Resolution goes through the root table's own [`bsl_search::WorkspaceRoots::resolve`]
/// rather than by rebuilding the directory from `root_id`: the identifier is derived from
/// the CANONICAL spelling while the file is read back through the DECLARED one, and
/// reconstructing it here would be a second attribution procedure to keep in agreement
/// with the first.
///
/// `None` for `root_id` means the caller said nothing about roots, and the path keeps
/// today's reading. An empty `root_id` is not that — it names the configuration, and
/// resolving it is what makes a hit's path work when the configuration sits in a
/// subdirectory of the workspace.
///
/// A named root must still be registered under its id AND that registration must be current: an
/// id survives a retarget, so a table built before one answers about the directory the id left
/// behind ([`bsl_search::WorkspaceRoots::registration_is_current`]).
pub(crate) fn resolve_rooted_path(
    roots: &WorkspaceRoots,
    root_id: Option<&str>,
    path: &Path,
) -> Result<PathBuf, RootedPathError> {
    let Some(root_id) = root_id else {
        return Ok(path.to_path_buf());
    };
    if path.is_absolute() {
        return if root_id.is_empty() {
            Ok(path.to_path_buf())
        } else {
            Err(RootedPathError::AbsolutePathWithRootId(root_id.to_owned()))
        };
    }
    // Stored ids and paths are `/`-spelled on every host, but a client that learned the pair on
    // Windows may still send it with the host separator; both spellings are accepted on input.
    // The ERROR TEXT below keeps `root_id` as the caller sent it, so the caller recognises their
    // own input in the refusal; only the lookups use the key spelling.
    let sent_root_id = root_id;
    let spelled_root_id = bsl_search::spell_as_key(sent_root_id);
    let root_id = spelled_root_id.as_str();
    let spelled_path = bsl_search::spell_as_key(&path.to_string_lossy());
    let path = Path::new(&spelled_path);
    if !roots.contains_id(root_id) {
        // Asked FIRST, so a caller with two wrong halves is told about the one that will
        // still be wrong after fixing the other.
        return Err(RootedPathError::RootNotRegistered(sent_root_id.to_owned()));
    }
    // A registered id is not yet a CURRENT one: the id survives a root's retarget, so a table from
    // an earlier generation answers this pair about a directory it can no longer resolve the id to.
    // Checked before the pair is joined, because every answer below is a file of that directory.
    if !roots.registration_is_current(root_id) {
        return Err(RootedPathError::RegistrationStale(sent_root_id.to_owned()));
    }
    // One rule, and it is about the SPELLING: every component must be a plain name.
    //
    // That covers more than `..`. On Windows neither a leading separator (`\Windows\M.bsl`)
    // nor a drive-relative spelling (`C:M.bsl`) counts as absolute, yet `join` throws the
    // base away for both — so a rule written against `..` alone lets exactly the escape
    // this exists to stop back in through a platform difference. `.` is refused for a
    // smaller reason: it survives into the graph id, and the graph was built from paths
    // that never had one.
    //
    // `..` in particular is refused rather than resolved. The kernel collapses it only
    // after dereferencing each component, so a `..` behind a directory link lands where no
    // textual fold predicts; folding it here would be a second, wrong procedure for naming
    // files. Two attempts to be cleverer failed on exactly that — one comparing the
    // canonical target against the root, one asking the table who owned it.
    //
    // The cost is nothing real: a key is built by a walk INSIDE its root, so no producer
    // of `(root_id, path)` emits any of these, and the same file is always reachable by
    // its plain spelling.
    if path.components().any(|component| !matches!(component, std::path::Component::Normal(_))) {
        return Err(RootedPathError::PathIsNotPlainRelative(sent_root_id.to_owned()));
    }
    // What follows from stopping here: a link sitting inside a root is a file OF that root,
    // and reading it yields its target, exactly as it would for anything else opening that
    // path. The pair names one file; it does not promise the bytes live in this tree.
    let key = bsl_search::FileKey::new(root_id, path.to_string_lossy());
    roots.resolve(&key).ok_or_else(|| RootedPathError::RootNotRegistered(sent_root_id.to_owned()))
}

/// The pair the ANSWER carries, given how the request spelled the file.
///
/// When the caller named a root, that root IS the answer: deriving it again by canonicalizing
/// the resolved path would rename a file reached through a link inside one root into the root
/// its target physically lives in, and a consumer feeding the pair back would address a
/// different copy. Only a caller that named no root — or one whose path was absolute, which
/// already names its file — gets one derived from the root table.
pub(crate) fn answer_location(
    roots: &WorkspaceRoots,
    root_id: Option<&str>,
    path_as_given: &Path,
    abs: &Path,
) -> Result<loc::Location, loc::LocationUnavailable> {
    let rooted_spelling =
        root_id.zip(path_as_given.to_str().filter(|spelling| !Path::new(spelling).is_absolute()));
    match rooted_spelling {
        // `from_key` answers in the store's `/` spelling however the request was spelled: the
        // pair is fed back by consumers and has to match the store.
        Some((root_id, relative)) => Ok(loc::Location::from_key(root_id, relative)),
        None => loc::Location::from_path(roots, abs),
    }
}

/// Why a file-addressed request has no file to answer about.
///
/// One closed vocabulary for every such tool. Two tools that answered the same input with
/// different codes would leave a consumer unable to write one branch per code, which is the
/// only thing a code is worth over free-form prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileError {
    /// The pair itself does not name a file; the reason is the rooted error's own.
    Rooted(RootedPathError),
    /// The pair resolved, but there is no workspace `.bsl` file at the end of it — nothing
    /// there, a directory wearing the name, or a path outside every root.
    NotInWorkspace,
    /// The file is there and its bytes are not: permissions, or not valid UTF-8.
    Unreadable,
}

impl FileError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Rooted(error) => error.code(),
            Self::NotInWorkspace => "not_in_workspace",
            Self::Unreadable => "unreadable",
        }
    }

    /// Whether the answer is whole despite the error.
    ///
    /// A path that names no workspace file is a COMPLETE answer to a wrong question — there
    /// is nothing more to say about it. A file whose bytes cannot be read is not: it exists,
    /// it belongs to the workspace, and what we could not tell about it is missing from
    /// everything computed over the workspace.
    pub(crate) fn completeness(&self) -> loc::Completeness {
        match self {
            Self::Rooted(_) | Self::NotInWorkspace => loc::Completeness::complete(),
            Self::Unreadable => loc::Completeness::partial(
                loc::ReasonCode::UnreadableFiles,
                "this file's bytes could not be read, so nothing was analysed in it",
            ),
        }
    }

    /// The in-band body. `detail` stays the caller's own: see the module header.
    pub(crate) fn to_value(&self, detail: &str, path: &Path) -> Value {
        json!({
            "error": self.code(),
            "detail": detail,
            "path": path.to_string_lossy(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every failure names ONE fact. A consumer branches on the code, so two facts sharing a
    /// code leave it unable to tell which correction to make.
    #[test]
    fn every_file_error_has_its_own_code() {
        let all = [
            FileError::Rooted(RootedPathError::RootNotRegistered("a".into())),
            FileError::Rooted(RootedPathError::AbsolutePathWithRootId("a".into())),
            FileError::Rooted(RootedPathError::PathIsNotPlainRelative("a".into())),
            FileError::Rooted(RootedPathError::RegistrationStale("a".into())),
            FileError::NotInWorkspace,
            FileError::Unreadable,
        ];

        let codes: std::collections::BTreeSet<&str> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len(), "two facts share a code");
    }

    /// The two file-level failures differ in completeness as well as in code, and that is the
    /// half a reader acts on: a wrong path leaves nothing missing, an unreadable file does.
    #[test]
    fn an_unreadable_file_is_partial_and_a_wrong_path_is_not() {
        assert!(FileError::NotInWorkspace.completeness().is_complete());

        let partial = FileError::Unreadable.completeness().to_value();
        assert_eq!(partial["status"], "partial");
        assert_eq!(partial["reasons"][0]["code"], "unreadable_files");
    }

    /// An id survives a root's retarget, so a table built before one would answer a
    /// `(root_id, path)` pair about the directory the id has left — a wrong answer wearing the
    /// shape of a right one. The pair is refused instead, before it is joined to anything, and the
    /// refusal names what was observed — the registered directory can no longer be reached through
    /// the spelling — rather than guessing a cause for it.
    #[cfg(unix)]
    #[test]
    fn a_retargeted_registration_is_refused_before_the_pair_is_joined() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let targets = dir.path().join("targets");
        for (target, module) in [("one", "СтараяЦель"), ("two", "НоваяЦель")] {
            std::fs::create_dir_all(targets.join(target)).unwrap();
            std::fs::write(targets.join(target).join("M.bsl"), module).unwrap();
        }
        std::fs::create_dir_all(&configuration).unwrap();
        let link = workspace.join("ext");
        std::os::unix::fs::symlink(targets.join("one"), &link).unwrap();
        let (roots, _) =
            WorkspaceRoots::build(&workspace, &configuration, std::slice::from_ref(&link));
        let ext_id =
            roots.ids().find(|id| !id.is_empty()).expect("the link is registered").to_owned();

        assert!(
            resolve_rooted_path(&roots, Some(&ext_id), Path::new("M.bsl")).is_ok(),
            "control: the registration is current before the retarget",
        );

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(targets.join("two"), &link).unwrap();

        assert_eq!(
            resolve_rooted_path(&roots, Some(&ext_id), Path::new("M.bsl")),
            Err(RootedPathError::RegistrationStale(ext_id.clone())),
            "the pair must not be answered from the directory the id left",
        );
        let refusal = RootedPathError::RegistrationStale(ext_id.clone()).to_string();
        assert!(
            refusal.contains(&ext_id) && refusal.contains("can no longer be resolved"),
            "the refusal must name both the root and what was observed: {refusal}",
        );
    }

    /// A client that learned a pair on Windows may send it with the host separator; the server
    /// accepts either spelling, resolves the same file, and answers with the key spelling.
    ///
    /// The host-spelled request is assembled with `Path::join` (a literal would hide the defect
    /// on Windows), and the control proves an unregistered root is still refused, with the
    /// refusal echoing the spelling the caller sent.
    #[test]
    fn a_host_spelled_rooted_request_resolves_like_the_slash_spelled_one() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let ext_rel = Path::new("extensions").join("provider-a");
        let module_rel = Path::new("CommonModules").join("M").join("Module.bsl");
        let module_dir = workspace.join(&ext_rel).join("CommonModules").join("M");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("Module.bsl"), "x").unwrap();
        std::fs::create_dir_all(&configuration).unwrap();
        let (roots, rejected) =
            WorkspaceRoots::build(workspace, &configuration, &[workspace.join(&ext_rel)]);
        assert!(rejected.is_empty(), "the extension root is registered");
        assert!(roots.contains_id("extensions/provider-a"), "ids are stored `/`-spelled");

        let host_root = ext_rel.to_string_lossy().into_owned();
        let slash = resolve_rooted_path(
            &roots,
            Some("extensions/provider-a"),
            Path::new("CommonModules/M/Module.bsl"),
        )
        .expect("the key spelling resolves");
        let host = resolve_rooted_path(&roots, Some(&host_root), &module_rel)
            .expect("the host spelling resolves too");
        assert_eq!(host, slash, "both spellings name the same file");

        let answer = answer_location(&roots, Some(&host_root), &module_rel, &host).unwrap();
        assert_eq!(answer.root_id, "extensions/provider-a");
        assert_eq!(answer.path, "CommonModules/M/Module.bsl");

        let unregistered = Path::new("extensions").join("nobody");
        let sent = unregistered.to_string_lossy().into_owned();
        let refusal = resolve_rooted_path(&roots, Some(&sent), &module_rel);
        assert_eq!(
            refusal,
            Err(RootedPathError::RootNotRegistered(sent)),
            "control: an unregistered root is refused, echoing the caller's own spelling",
        );
    }
}
