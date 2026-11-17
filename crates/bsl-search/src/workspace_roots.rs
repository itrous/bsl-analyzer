//! Which source root a workspace file belongs to, and under which key it is
//! stored.
//!
//! A `cfe` extension repeats the configuration's directory layout, so the path
//! relative to a root is not unique across roots: `CommonModules/M/Ext/Module.bsl`
//! exists under the configuration and under every extension at once. Store rows
//! are therefore keyed by the pair `(root_id, path)`, and this table is the only
//! seam that turns an absolute path into that pair and back.
//!
//! The table knows nothing about configurations or extensions beyond which root
//! is *the* configuration one; the caller builds it from the project model.

use std::path::{Path, PathBuf};

/// The configuration root's identity. Reserved: an extension never takes it, so
/// rows written before roots existed — all of them the configuration's — keep
/// their meaning under the composite key without being rewritten.
pub const CONFIGURATION_ROOT_ID: &str = "";

/// A declared root that did not become a registered one. Reported rather than
/// swallowed: the caller has to be able to say which root it dropped and why,
/// because in both cases some files end up labelled differently than declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRoot {
    pub path: PathBuf,
    pub reason: Rejection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Canonically inside the configuration root. Its files stay searchable as
    /// the configuration's, but their root label is the configuration's too.
    InsideConfiguration { root: PathBuf },
    /// Its identifier is already taken by another root, so registering it would
    /// give two different directories one key space and let the second overwrite
    /// the first. Reachable because the identifier is a lossy string: two paths
    /// that differ only in bytes no `str` can hold render identically.
    IdentifierTaken { id: String },
}

/// The identity of an indexed file: the root it belongs to and its path relative
/// to that root.
///
/// A pair rather than one string because the root cannot be folded into the path
/// without a separator, and every separator collides with a legal configuration
/// path. It is also what the store rows are keyed by, so a key that travels as
/// one value cannot lose half of itself on the way to a lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey {
    pub root_id: String,
    pub path: String,
}

impl FileKey {
    /// Both halves are spelled with `/` on Windows ([`spell_as_key`]).
    ///
    /// A key is persisted and compared against keys built elsewhere, so its text cannot depend
    /// on the host that minted it. Both halves are minted from paths relative to a root, which
    /// is why the rewrite is safe for the root id too. Only Windows rewrites: a backslash is an
    /// ordinary character in a Unix file name.
    pub fn new(root_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self { root_id: spell_as_key(&root_id.into()), path: spell_as_key(&path.into()) }
    }

    /// A file of the configuration root.
    ///
    /// A constructor for the common case, not a statement about what a corpus contains: the
    /// publisher walks every registered root, and the PostgreSQL side keys a file by
    /// `(collection, root_id, path)` throughout — lexical and semantic serving alike — storing
    /// those identities as they are.
    pub fn configuration(path: impl Into<String>) -> Self {
        Self::new(CONFIGURATION_ROOT_ID, path)
    }

    pub fn is_configuration(&self) -> bool {
        self.root_id == CONFIGURATION_ROOT_ID
    }

    /// Whether this key names a file strictly inside the directory `dir` names.
    ///
    /// Compared by whole components, not by text: a textual prefix would let `Dir`
    /// swallow `Dir2`, and the two are unrelated directories. Roots must match — the
    /// same relative path under two roots is two different files.
    pub fn is_under(&self, dir: &FileKey) -> bool {
        self.root_id == dir.root_id && starts_at(Path::new(&self.path), Path::new(&dir.path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Root {
    id: String,
    /// The spelling the project declared, which is what exists on disk for a
    /// caller that wants to read the file back.
    ///
    /// Stable across a retarget — it is the link itself — which is why it is what identity is
    /// matched by when a generation is re-stated ([`WorkspaceRoots::inheriting_root_ids`]).
    declared: PathBuf,
    /// The spelling every topology decision is made on.
    ///
    /// Read off the disk ONCE when this table was built, so it is a fact about the GENERATION
    /// the table describes, not about the disk at answer time: a root reached through a link
    /// that is later retargeted names a different directory, and an answer recomputed from a
    /// fresh read would rank a target this generation's rows were not minted against. A retarget
    /// is therefore a next generation, re-stated EXPLICITLY
    /// ([`WorkspaceRoots::inheriting_root_ids`]); it is never re-read here, and nothing in
    /// [`WorkspaceRoots::root_of`] consults the disk (github#277).
    canonical: PathBuf,
}

/// The registered source roots of one workspace.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRoots {
    /// The workspace itself. Not a source root — root ids are relative to it,
    /// and callers that speak of "the project directory" (the graph, the
    /// resident host) mean this and not the configuration root, which may sit in
    /// a subdirectory.
    workspace: PathBuf,
    /// The workspace's own spelling with every link resolved, read off the disk ONCE
    /// when this table was built.
    ///
    /// It is what a durable path-keyed id is stripped against: the file universe of the
    /// generation this table describes is enumerated canonically, so only the canonical
    /// workspace is a prefix of those paths. Kept here rather than re-read per use
    /// because it is a fact about THIS generation — a root reached through a link that
    /// is later retargeted names a different directory, and a base read at answer time
    /// would then decode ids the generation never minted.
    ///
    /// A workspace that itself resolves elsewhere is not this table's next generation but
    /// another registration sharing no key space with it: root ids are stripped against this
    /// value, so re-stating one under the other would decode ids of the wrong base.
    /// [`WorkspaceRoots::inheriting_root_ids`] therefore refuses it and keeps the fresh table.
    workspace_canonical: PathBuf,
    roots: Vec<Root>,
    /// Subtrees inside the roots that a scan must not descend into.
    ///
    /// Carried here because the roots are what every scan of this workspace is driven
    /// from, and a scan that walked a hole the watcher does not follow would index
    /// files whose later edits never arrive — present in the corpus, frozen at the
    /// content of the last full walk. The value belongs to whoever owns the subtree
    /// (the server's derived cache); this type only carries it to the walk.
    ///
    /// Not part of root identity: the transition machinery compares roots, and a cache
    /// moved without the roots moving is not a topology change.
    excluded: Vec<PathBuf>,
    /// Directories the user took out of the project (`[source].exclude`). Unlike
    /// `excluded`, part of identity: changing them changes which files the roots hold,
    /// so a table that differs only here describes a different source universe. A path
    /// inside one belongs to no root, whether a walk reaches it or a caller names it.
    user_excluded: project_model::ExcludedPaths,
}

/// Equality is the ROOTS, and the doc on `excluded` is what the machinery around this
/// type acts on: whole values are compared in both directions — as "the roots did not
/// move, take the fast path" and as "these are not the roots the plan was made for,
/// drop it". A derived `PartialEq` puts the cache path on both of those, so the same
/// unrelated difference would force a full root transition in one place and discard a
/// valid transition in another.
///
/// `workspace_canonical` is compared, and it cannot be derived from what already is: a root
/// is allowed to lie OUTSIDE the workspace and be identified by its absolute spelling
/// ([`root_id_for`]), so a workspace whose link is retargeted while every registered root
/// stays put moves no other field here. Two tables that strip their ids against different
/// directories are not one registration, whatever their roots are spelled like — leaving it
/// out would let such a pair pass as "the roots did not move" and keep serving a base the
/// next build would not mint against.
///
/// A root whose DECLARED spelling is the same while its canonical directory moved compares
/// UNEQUAL, and that is the transition this whole type is asked to drive: the canonical
/// spelling is the generation, so a retarget is a different generation under the SAME id — the
/// form [`WorkspaceRoots::inheriting_root_ids`] states explicitly, which is what keeps the
/// id-keyed rows and external addresses of the old generation attributable to the root after it
/// moved (github#277).
impl PartialEq for WorkspaceRoots {
    fn eq(&self, other: &Self) -> bool {
        self.workspace == other.workspace
            && self.workspace_canonical == other.workspace_canonical
            && self.roots == other.roots
            && self.user_excluded == other.user_excluded
    }
}

impl Eq for WorkspaceRoots {}

impl WorkspaceRoots {
    /// Build the table from the configuration root and the declared extension
    /// roots, both spelled as the project declares them.
    ///
    /// `workspace_root` is what root ids are relative to. Extension roots that
    /// canonically lie inside the configuration root are left out and returned
    /// separately: those files are already reachable — and already published in
    /// a baseline — as the configuration's, and giving them a second identity
    /// would mean two rows for one file and a split from the published snapshot.
    pub fn build(
        workspace_root: &Path,
        configuration: &Path,
        extensions: &[PathBuf],
    ) -> (Self, Vec<RejectedRoot>) {
        Self::build_optional(workspace_root, Some(configuration), extensions)
    }

    /// Build a root table for a project whose base configuration is optional.
    /// Extension-only projects register only extension roots; no synthetic
    /// configuration root is introduced.
    pub fn build_optional(
        workspace_root: &Path,
        configuration: Option<&Path>,
        extensions: &[PathBuf],
    ) -> (Self, Vec<RejectedRoot>) {
        let workspace_canonical = canonicalize(workspace_root);
        let configuration_canonical = configuration.map(canonicalize);
        let mut roots = Vec::new();
        if let (Some(configuration), Some(canonical)) =
            (configuration, configuration_canonical.as_ref())
        {
            roots.push(Root {
                id: CONFIGURATION_ROOT_ID.to_owned(),
                declared: configuration.to_path_buf(),
                canonical: canonical.clone(),
            });
        }
        let mut rejected = Vec::new();

        for extension in extensions {
            let canonical = canonicalize(extension);
            if let (Some(configuration), Some(configuration_canonical)) =
                (configuration, configuration_canonical.as_ref())
            {
                if canonical == *configuration_canonical
                    || starts_at(&canonical, configuration_canonical)
                {
                    rejected.push(RejectedRoot {
                        path: extension.clone(),
                        reason: Rejection::InsideConfiguration {
                            root: configuration.to_path_buf(),
                        },
                    });
                    continue;
                }
            }
            if roots.iter().any(|root| root.canonical == canonical) {
                continue;
            }
            let id = root_id_for(&workspace_canonical, &canonical, extension);
            if roots.iter().any(|root| root.id == id) {
                rejected.push(RejectedRoot {
                    path: extension.clone(),
                    reason: Rejection::IdentifierTaken { id },
                });
                continue;
            }
            roots.push(Root { id, declared: extension.clone(), canonical });
        }

        (
            Self {
                workspace: workspace_root.to_path_buf(),
                workspace_canonical,
                roots,
                excluded: Vec::new(),
                user_excluded: project_model::ExcludedPaths::default(),
            },
            rejected,
        )
    }

    /// The root a file belongs to and the key it is stored under, or `None` when
    /// the file lies outside every root.
    ///
    /// Two spellings go in because the enumerator has two: `canonical` is what
    /// the semantics rank roots by and what makes attribution independent of
    /// which alias the walk happened to arrive through, while `walked` is the
    /// only handle on a file the walk reached through a symlink that leaves
    /// every root. Such a file is not dropped — the graph has always seen it —
    /// it is keyed by the walking root instead.
    ///
    /// Both spellings are matched against the roots BYTE for byte, which decides what
    /// happens to a path that reached the caller already rendered — the graph keeps its
    /// file paths as strings, so bytes no `str` can carry come back replaced:
    ///
    /// - rendered BELOW the deepest root that still matches, the answer is the key the row
    ///   lives under: the key of the relative part is a rendering too ([`key_of`]), so both
    ///   spellings arrive at one key. This is the ordinary case and nothing is guessed;
    /// - rendered inside a root's own name, that root stops matching. An ANCESTOR root may
    ///   still match — nested extension roots are legal — and then the answer is the
    ///   ancestor's key, while the file's own row is keyed by the inner root. Usually no row
    ///   carries that key and a mark written under it clears itself unused; where the key
    ///   space's own collision puts a real file there, that file is re-rendered needlessly —
    ///   its own context, so nothing wrong is stored — and the real one stays stale;
    /// - with no root matching at all, the answer is `None` and the caller skips its mark.
    ///
    /// What is deliberately NOT done is ranking such a path in the rendered alphabet. That
    /// was tried and taken out: the rendering is not reversible, so it fits several roots at
    /// once (two roots differing only in those bytes; a root and a neighbour holding a
    /// genuine `U+FFFD`; a nesting that exists only after rendering), and a key guessed from
    /// it does not merely mark the wrong file — this is the seam a REMOVAL resolves its keys
    /// through. What remains is the key space's own long-standing property: two names that
    /// differ only in unrepresentable bytes share a key, whichever spelling reaches it.
    pub fn root_of(&self, walked: &Path, canonical: &Path) -> Option<FileKey> {
        if self.user_excluded.is_excluded(walked) || self.user_excluded.is_excluded(canonical) {
            return None;
        }
        self.longest_match(canonical, |root| &root.canonical)
            .or_else(|| self.longest_match(walked, |root| &root.declared))
    }

    /// The owning root ranked by the DECLARED spellings alone. For a file whose canonical
    /// target must not take part in attribution (a non-source target), plain [`Self::root_of`]
    /// would still rank the walked path against the CANONICAL root spellings first — and under
    /// a root declared through a link, the walked path also lies under the enclosing root's
    /// canonical spelling, handing the key to the wrong root.
    pub(crate) fn root_of_declared(&self, walked: &Path) -> Option<FileKey> {
        if self.user_excluded.is_excluded(walked) {
            return None;
        }
        self.longest_match(walked, |root| &root.declared)
    }

    /// The key a workspace file carries, given its two spellings and whether its canonical
    /// target takes part in attribution: the ONE place the ROLE branch lives.
    ///
    /// The ROLE branch, and not attribution as a whole: [`Self::root_of`] is called directly
    /// elsewhere, and that is allowed where the role cannot differ — on the output of the shared
    /// walk (which admits only files whose two spellings agree by role) and on canonical paths,
    /// which have no role at all. What no caller may do is rank a canonical target that does NOT
    /// take part in attribution; this method and [`Self::root_of_declared`] are the only answers
    /// to that question.
    ///
    /// When the target is a source, the canonical spelling ranks first — that is what makes the
    /// answer independent of the alias the walk happened to arrive through — and the walked
    /// spelling keeps a file that leaves every root in the universe ([`Self::root_of`]). When it
    /// is not, a key under the target's root would be one that is FORBIDDEN to exist: the walk
    /// drops such files, so the only spelling that could ever have indexed one is the walked one,
    /// and ranking by the target would miss the row a removal has to reach
    /// ([`Self::root_of_declared`]).
    ///
    /// Pure by construction: nothing here reads the disk, so one input answers one key however
    /// the table was obtained. The adapters stay thin — [`crate::workspace_file_key_in`] computes
    /// the two spellings and the role, and the baseline publisher calls that same adapter, so a
    /// key it writes is a key the daemon reading the snapshot derives (github#278). Callers that
    /// ask the question of a path that is not a stored document (a descriptor, a directory) have
    /// no role to branch on and use [`Self::key_of_path`] instead.
    pub fn file_key(
        &self,
        walked: &Path,
        canonical: &Path,
        target_is_source: bool,
    ) -> Option<FileKey> {
        if target_is_source {
            self.root_of(walked, canonical)
        } else {
            self.root_of_declared(walked)
        }
    }

    /// The two spellings attribution ranks a path by.
    ///
    /// A relative path is read against the CONFIGURATION root: that is how every
    /// stored path with the reserved id is spelled, and it is the prefix callers
    /// strip before handing one over. The table's workspace exists to make root
    /// identifiers relative and is a directory higher whenever the configuration
    /// sits in a subdirectory.
    pub(crate) fn spellings_of(&self, path: &Path) -> (PathBuf, PathBuf) {
        let walked = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.configuration().unwrap_or_else(|| self.workspace()).join(path)
        };
        let canonical = canonical_spelling(&walked);
        (walked, canonical)
    }

    /// The key a workspace path carries whatever its role — the question "which
    /// root does this path belong to, and where in it" asked of a path that is
    /// not a stored document: a metadata descriptor, a directory.
    ///
    /// The role branch [`crate::SearchEngine`] applies to a `.bsl` has no meaning
    /// here: it exists to keep a key REMOVABLE under the spelling its file was
    /// indexed with, and nothing is indexed under this one. Where the path
    /// physically lies is the whole answer, so the canonical spelling ranks
    /// first, as it does for a live file.
    pub fn key_of_path(&self, path: &Path) -> Option<FileKey> {
        let (walked, canonical) = self.spellings_of(path);
        self.root_of(&walked, &canonical)
    }

    /// The file a stored key points at, spelled as the project declared it.
    pub fn resolve(&self, key: &FileKey) -> Option<PathBuf> {
        let root = self.roots.iter().find(|root| root.id == key.root_id)?;
        validate_relative_key_path(&key.path)?;
        Some(join_key_path(&root.declared, &key.path))
    }

    /// The file a stored key points at, spelled as the WALK reached it.
    ///
    /// [`Self::resolve`] is for opening the file, and the declared spelling is what a reader
    /// opens. This one is for naming it: a durable path-keyed id is the walk's spelling minus
    /// the workspace, and the walk descended from the canonical roots this table fixed when it
    /// was built. Asking the disk for the same answer at use time would resolve the DECLARED
    /// spelling through whatever the links point at now, which is a different file whenever one
    /// of them has been retargeted — and then the id names a node this generation never held.
    pub fn resolve_walked(&self, key: &FileKey) -> Option<PathBuf> {
        let root = self.roots.iter().find(|root| root.id == key.root_id)?;
        validate_relative_key_path(&key.path)?;
        Some(join_key_path(&root.canonical, &key.path))
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The workspace spelling a durable path-keyed id is stripped against — see the field.
    pub fn workspace_canonical(&self) -> &Path {
        &self.workspace_canonical
    }

    /// The declared spelling of the configuration root.
    pub fn configuration(&self) -> Option<&Path> {
        self.roots
            .iter()
            .find(|root| root.id == CONFIGURATION_ROOT_ID)
            .map(|root| root.declared.as_path())
    }

    /// Whether a root is registered under this id.
    pub fn contains_id(&self, root_id: &str) -> bool {
        self.roots.iter().any(|root| root.id == root_id)
    }

    /// Whether the root registered under this id still leads where THIS table minted it.
    ///
    /// The canonical spelling is read off the disk once, when the table is built, and the id
    /// survives a retarget ([`Self::inheriting_root_ids`]): a link declared as a root can afterwards
    /// point at another directory while the id — and every row, journal spelling and external
    /// address keyed by it — stays. A reader holding a table from an earlier generation (the
    /// diagnostics resident holds its own across a config-only reload) therefore answers about a
    /// directory the id no longer names, and the same `(root_id, path)` names one file to that
    /// reader and another to the workspace.
    ///
    /// So the registration is re-checked before it is used, and the answer depends on HOW THE ID WAS
    /// MINTED — what the table could not resolve when it was built leaves no target behind to
    /// disprove:
    ///
    /// - **spelling-minted** (`canonical == declared`): always current, whatever the declared spelling
    ///   resolves to now. The equality arises BOTH ways and means the same thing either way — the
    ///   table could not canonicalise the declared spelling when it was built and fell back to that
    ///   spelling, OR canonicalisation succeeded and returned the spelling itself (a plain directory,
    ///   a spelling that resolves to itself). In both cases the id was minted by the SPELLING and not
    ///   by a remembered target, so there is no target it could have left: such a root is attributed
    ///   by its own spelling in the engine and the resident, and a reader opens it through that
    ///   spelling again, so the root follows whatever the spelling leads to now — a link that appears
    ///   later and leads somewhere else is a legal configuration, not a stale registration;
    /// - **resolved-minted** and the declared spelling still canonicalises to the SAME directory:
    ///   current — that is the directory the id was minted from;
    /// - **resolved-minted** and the declared spelling now canonicalises ELSEWHERE, or no longer
    ///   canonicalises at all: stale. The id names a directory that was reachable once and is not
    ///   reachable now (a link whose target moved or vanished is exactly the retarget that must not
    ///   be answered through), and nothing in the table can prove the binding to a reader that still
    ///   holds it.
    ///
    /// A successful canonicalisation of a spelling-minted root is answered by the first clause rather
    /// than by the comparison; the comparison would give the same answer for the spelling that
    /// resolves to itself and the wrong one for every other.
    ///
    /// An id no root is registered under is not this question: callers ask [`Self::contains_id`]
    /// first, and a table that has never heard of the id has no registration that could be stale.
    pub fn registration_is_current(&self, id: &str) -> bool {
        let Some(root) = self.roots.iter().find(|root| root.id == id) else {
            return true;
        };
        // The spelling itself is the id, and it is followed wherever it leads: whether the equality
        // came from a fallback or from a coincidence, there is no remembered target that could have
        // been left.
        if root.canonical == root.declared {
            return true;
        }
        match std::fs::canonicalize(&root.declared) {
            Ok(now) => now == root.canonical,
            // Resolved once, unresolvable now: the remembered target is gone and nothing proves the
            // binding the reader is holding.
            Err(_) => false,
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(|root| root.id.as_str())
    }

    /// Root ids present in both tables whose directory binding changed.
    ///
    /// The id alone survives a move: the configuration keeps the empty id wherever its directory
    /// is, and an extension alias can be retargeted. A rebound id keeps its key space, so matching
    /// bytes may keep their rows — but nothing learned from the old directory about a key whose
    /// new bytes were not compared may be kept.
    pub(crate) fn rebound_root_ids(&self, next: &Self) -> std::collections::HashSet<String> {
        self.roots
            .iter()
            .filter(|root| {
                next.roots
                    .iter()
                    .find(|candidate| candidate.id == root.id)
                    .is_some_and(|candidate| candidate != *root)
            })
            .map(|root| root.id.clone())
            .collect()
    }

    /// Re-state a freshly built table as the NEXT generation of this one: the same root ids, the
    /// fresh directories, and the fresh workspace and exclusions.
    ///
    /// The canonical spelling of a root is a fact about ONE generation: a root declared through a
    /// link resolves to whatever that link points at when the table is built. Rebuilding the table
    /// after the link is retargeted therefore mints a NEW id for the same declared root, because
    /// [`root_id_for`] derives the id from the canonical spelling — and an id is the durable
    /// identity: store rows, journal spellings and every external address are keyed by it, so
    /// taking the rebuild as it stands would drop everything the old id held and add a stranger in
    /// its place. This is the explicit re-canonicalisation github#277 asks for: attribution ranks
    /// the FRESH canonical spelling from here on, and the root keeps its identity across its own
    /// retarget (a REBIND, which [`Self::rebound_root_ids`] and the transition machinery already
    /// know how to handle, and not a removal plus an addition).
    ///
    /// A root is matched by its DECLARED spelling, which is what a retarget does not change: the
    /// same declaration now leads elsewhere. The match is ROLE-AWARE — a non-configuration root is
    /// only ever matched against a non-configuration one, and the fresh configuration root never
    /// inherits at all — so the reserved empty id can neither leave the configuration nor be taken
    /// by an extension that merely stands where a configuration used to.
    ///
    /// The rule is applied to the fresh table AS A WHOLE, not root by root, because an id has to
    /// belong unambiguously to one of the fresh roots for the answer to be independent of the order
    /// they are declared in. A fresh root therefore inherits only if the id is
    ///
    /// - not [`CONFIGURATION_ROOT_ID`] — the reserved empty id belongs to the configuration and to
    ///   nothing else, whatever the two are spelled like;
    /// - not the NATURAL id of ANOTHER fresh root ([`root_id_for`]'s answer for that root's fresh
    ///   canonical spelling) — such an id belongs to the root that spelled it that way, so a
    ///   retargeted root whose former directory (or former id) was declared as a root of THIS
    ///   generation keeps its own fresh id instead;
    /// - not already inherited by another root of this generation.
    ///
    /// A refused inheritance, like everything else here, is not a panic: the fresh root keeps the
    /// id it was built with. It IS an explicit refusal of the re-canonicalisation —
    /// [`Self::inheriting_root_ids`] states the next generation under the old ids wherever an id is
    /// unambiguously the same root's, and declines to guess where it is not. What the refusal costs
    /// is NOT a removal plus an addition: the conflicting id stays with the fresh root it is natural
    /// for, and that root keeps a REBIND — which is what the transition machinery already handles:
    /// rows are reused where the bytes they name still match (the local mode), and rows an external
    /// baseline no longer covers are cleaned. The root that was refused arrives under its own fresh
    /// id, and that fresh id is the one genuinely new key space of the pair.
    ///
    /// A table whose WORKSPACE resolves elsewhere is another registration rather than the next
    /// generation of this one — ids are stripped against the workspace, so the two do not share a
    /// key space — and the fresh table is returned untouched, which is the existing behaviour
    /// ([`Self::eq`] says the same thing).
    pub fn inheriting_root_ids(&self, fresh: &Self) -> Self {
        if self.workspace_canonical != fresh.workspace_canonical {
            return fresh.clone();
        }
        // Every id the fresh table carries by itself, gathered BEFORE any assignment: an id that
        // one of the fresh roots minted for itself can never be inherited by another, whatever
        // order the two are declared in.
        let natural: std::collections::HashSet<&str> =
            fresh.roots.iter().map(|root| root.id.as_str()).collect();
        let mut taken: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut roots: Vec<Root> = Vec::with_capacity(fresh.roots.len());
        for root in &fresh.roots {
            let inherited = if root.id == CONFIGURATION_ROOT_ID {
                // The reserved id never moves: a configuration root keeps it wherever its
                // directory is, and nothing else may take it.
                None
            } else {
                self.roots
                    .iter()
                    // Role is part of identity: extension with extension only, so a base
                    // configuration that moved onto an ex-extension's directory is not handed
                    // that extension's id and the empty id cannot wander off the configuration.
                    .find(|known| {
                        known.id != CONFIGURATION_ROOT_ID && known.declared == root.declared
                    })
                    .map(|known| known.id.as_str())
                    .filter(|id| !natural.contains(id) && !taken.contains(id))
            };
            let id = inherited.unwrap_or(root.id.as_str());
            taken.insert(id);
            roots.push(Root {
                id: id.to_owned(),
                declared: root.declared.clone(),
                canonical: root.canonical.clone(),
            });
        }
        debug_assert!(
            {
                let mut seen = std::collections::HashSet::new();
                roots.iter().all(|root| seen.insert(root.id.as_str()))
            },
            "re-stating a generation gave two directories one key space: {:?}",
            roots.iter().map(|root| root.id.as_str()).collect::<Vec<_>>(),
        );
        Self {
            workspace: fresh.workspace.clone(),
            workspace_canonical: fresh.workspace_canonical.clone(),
            roots,
            excluded: fresh.excluded.clone(),
            user_excluded: fresh.user_excluded.clone(),
        }
    }

    /// Registered roots as `(id, declared path)`, in registration order.
    /// Take the subtrees a scan of these roots must not descend into.
    ///
    /// A builder step rather than a `build` parameter: every one of the hundred-odd
    /// call sites that has no cache to speak of keeps working unchanged, and the few
    /// that own one say so explicitly.
    #[must_use]
    pub fn with_excluded(mut self, excluded: Vec<PathBuf>) -> Self {
        self.excluded = excluded;
        self
    }

    /// See [`Self::with_excluded`].
    pub fn excluded(&self) -> &[PathBuf] {
        &self.excluded
    }

    /// The directories the user took out of the project, re-spelled under each root so
    /// that a walk from a root declared through a link recognises them too.
    #[must_use]
    pub fn with_user_excluded(mut self, user_excluded: &project_model::ExcludedPaths) -> Self {
        let declared: Vec<PathBuf> = self.roots.iter().map(|root| root.declared.clone()).collect();
        self.user_excluded = user_excluded.respelled_under(&declared);
        self
    }

    /// See [`Self::with_user_excluded`].
    pub fn user_excluded(&self) -> &project_model::ExcludedPaths {
        &self.user_excluded
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.roots.iter().map(|root| (root.id.as_str(), root.declared.as_path()))
    }

    /// Registered roots as `(id, declared spelling, canonical spelling)`, in registration
    /// order.
    ///
    /// [`Self::entries`] answers where a root is spelled; this also answers where it led
    /// when the table was built — the only handle on a root reached through a link once
    /// that link no longer resolves, and on the roots registered under the same target
    /// (github#190).
    pub fn entries_with_canonical(&self) -> impl Iterator<Item = (&str, &Path, &Path)> {
        self.roots
            .iter()
            .map(|root| (root.id.as_str(), root.declared.as_path(), root.canonical.as_path()))
    }

    /// A table whose two spellings are STATED rather than read off the disk.
    ///
    /// Attribution is interesting exactly where the spellings differ, and a real link is
    /// neither available on every test platform nor something a test wants to keep alive
    /// while it asks what a removal does once the link's target is gone (github#190).
    #[cfg(test)]
    pub(crate) fn with_stated_spellings(
        workspace: &Path,
        roots: Vec<(String, PathBuf, PathBuf)>,
    ) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            workspace_canonical: workspace.to_path_buf(),
            roots: roots
                .into_iter()
                .map(|(id, declared, canonical)| Root { id, declared, canonical })
                .collect(),
            excluded: Vec::new(),
            user_excluded: project_model::ExcludedPaths::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The root with the longest matching prefix, under the given spelling.
    ///
    /// Longest rather than first: roots may nest, and the innermost one is what
    /// the semantics call the file's owner. The configuration competes like any
    /// other root, so an extension declared *around* it keeps only the files
    /// outside it.
    fn longest_match(&self, path: &Path, spelling: impl Fn(&Root) -> &PathBuf) -> Option<FileKey> {
        self.roots
            .iter()
            .filter_map(|root| {
                let rel = path.strip_prefix(spelling(root)).ok()?;
                // `strip_prefix` compares whole components, so `cf` never
                // swallows `cf_ext`. An empty remainder means the path *is* the
                // root, which is not a file in it.
                (rel.components().next().is_some()).then(|| {
                    (spelling(root).components().count(), FileKey::new(&root.id, key_of(rel)))
                })
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, key)| key)
    }
}

/// A persisted file key is data, not a path supplied by the current process.
/// Keep the check in this module so every graph/search reader gets the same
/// rejection rules before joining it to a root.  In particular, `Path::join`
/// would otherwise accept an absolute right hand side and escape the declared
/// root, while a parent component could reach an unrelated directory.
fn validate_relative_key_path(path: &str) -> Option<()> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    // Durable keys are emitted with `/`, but a store written before that carries
    // the Windows separator and may be inspected on another platform.  Do
    // the security check for both separator conventions before asking `Path` to
    // validate the native spelling; otherwise Linux treats `..\\outside` as one
    // ordinary filename and Windows can later escape the declared root with it.
    if path.starts_with('/')
        || path.starts_with('\\')
        || path.as_bytes().get(1) == Some(&b':')
        || path.contains(':')
    {
        return None;
    }
    if path
        .split(['/', '\\'])
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }
    if Path::new(path)
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(())
}

/// The store key of a path relative to its root.
fn key_of(rel: &Path) -> String {
    spell_as_key(&rel.to_string_lossy())
}

/// A text spelled the way store keys are spelled: `/` between components on every platform.
///
/// Keys are persisted and compared against keys built from configuration paths, so a key's text
/// cannot depend on the host separator. Only Windows rewrites: a backslash is an ordinary
/// character in a Unix file name. This is the single place that knows the rule; a caller that
/// accepts a root id or a relative path from outside applies it before comparing with keys.
pub fn spell_as_key(text: &str) -> String {
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_owned()
    }
}

/// A host path rebuilt from a key's `/`-spelled relative part.
///
/// Joined segment by segment: handing the whole string to `join` would leave the embedded `/`
/// as given on Windows, a mixed spelling no walked path matches. Unconditional on purpose, so
/// a literal backslash in a Unix file name stays inside its segment.
fn join_key_path(root: &Path, key_path: &str) -> PathBuf {
    key_path.split('/').fold(root.to_path_buf(), |path, segment| path.join(segment))
}

fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` lies strictly inside `prefix`.
pub(crate) fn starts_at(path: &Path, prefix: &Path) -> bool {
    path.strip_prefix(prefix).is_ok_and(|rest| rest.components().next().is_some())
}

/// A root's identity: its path relative to the workspace, or absolute when it
/// lies outside.
///
/// Not the extension's name — several entries are allowed to share one — and
/// not an ordinal, which would not survive a restart.
fn root_id_for(workspace_canonical: &Path, canonical: &Path, declared: &Path) -> String {
    let relative = canonical.strip_prefix(workspace_canonical).ok().map(key_of);
    match relative {
        // A root spelled `.` relative to the workspace would take the
        // configuration's reserved empty id and overwrite its rows.
        Some(relative) if relative.is_empty() => ".".to_owned(),
        Some(relative) => relative,
        None if canonical.is_absolute() => key_of(canonical),
        // Neither under the workspace nor absolute: keep the declared spelling
        // rather than invent one, and make sure it cannot be read as empty.
        None => {
            let declared = key_of(declared);
            if declared.is_empty() || declared == "." {
                ".".to_owned()
            } else {
                declared
            }
        }
    }
}

/// The canonical spelling of a path, falling back to the canonical spelling of
/// its directory when the file itself is gone.
///
/// Deletion is the case this exists for: a removed file cannot be canonicalized,
/// and dropping all the way to the walked spelling would leave attribution
/// ranking roots by their declared paths alone. A file that lived under a root
/// reached through an alias would then be removed under a DIFFERENT root's key,
/// while its real row stayed behind serving a dead hit.
pub(crate) fn canonical_spelling(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roots must exist on disk for the canonical spelling to mean anything, so
    /// every fixture builds real directories.
    fn dirs(root: &Path, rels: &[&str]) -> Vec<PathBuf> {
        rels.iter()
            .map(|rel| {
                let path = root.join(rel);
                std::fs::create_dir_all(&path).unwrap();
                path
            })
            .collect()
    }

    /// The fixture root, spelled canonically.
    ///
    /// A temporary directory may itself be reached through a link (`/var` is one to
    /// `/private/var` on macOS). A fixture that mixes directories that exist with names
    /// no filesystem can hold would then carry two spellings of one directory: `build`
    /// canonicalizes what it can reach and keeps the declared spelling of the rest.
    #[cfg(unix)]
    fn canonical_root(dir: &tempfile::TempDir) -> PathBuf {
        std::fs::canonicalize(dir.path()).unwrap()
    }

    /// Two tables that strip their ids against different directories are not one
    /// registration, even when every root path is spelled identically in both.
    ///
    /// A root may lie outside the workspace and be identified by its absolute spelling, and
    /// then a workspace link retargeted onto another directory moves nothing else here. What
    /// it does move is the base every path-keyed id is stripped against, so a comparison that
    /// missed it would report "the roots did not move" and keep serving a base the next build
    /// would not mint against.
    #[cfg(unix)]
    #[test]
    fn a_workspace_that_resolves_elsewhere_is_another_registration() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let configuration = dirs(&root, &["a/sub/cf"]).remove(0);
        let workspace = root.join("workspace");
        std::os::unix::fs::symlink(root.join("a"), &workspace).unwrap();
        let (before, _) = WorkspaceRoots::build(&workspace, &configuration, &[]);

        std::fs::remove_file(&workspace).unwrap();
        std::os::unix::fs::symlink(root.join("a/sub"), &workspace).unwrap();
        let (after, _) = WorkspaceRoots::build(&workspace, &configuration, &[]);

        assert_eq!(
            before.workspace(),
            after.workspace(),
            "control: the workspace is declared by the same path in both",
        );
        assert_ne!(
            before.workspace_canonical(),
            after.workspace_canonical(),
            "control: and the link makes that one path name two directories",
        );
        assert_ne!(before, after, "so the two tables are not the same registration");
    }

    /// A directory named by bytes no `str` can hold, created where the filesystem takes
    /// such a name.
    ///
    /// APFS keeps names in valid UTF-8 and refuses anything else with `EILSEQ`, so on
    /// macOS this directory cannot exist and the path is all there is of it. Attribution
    /// answers the same either way: `build` falls back to the declared spelling of a path
    /// it cannot canonicalize, and under a canonical root that spelling IS the canonical
    /// one.
    #[cfg(unix)]
    fn unrepresentable_dir(path: PathBuf) -> PathBuf {
        assert!(path.to_str().is_none(), "the fixture is about bytes no `str` can carry");
        let _ = std::fs::create_dir_all(&path);
        path
    }

    /// The attribution of a file that exists only as a path: both spellings are
    /// the same when no alias is involved.
    fn owner(roots: &WorkspaceRoots, file: &Path) -> Option<FileKey> {
        roots.root_of(file, file)
    }

    const MODULE: &str = "CommonModules/М/Ext/Module.bsl";

    /// Where the cache lives is not a topology fact, and equality is read in both
    /// directions: as "the roots did not move, take the fast path" and as "these are
    /// not the roots this plan was made for, drop it". Counting the cache in would
    /// force a full root transition on one path and discard a valid transition on the
    /// other — from the same unrelated difference.
    #[test]
    fn where_the_cache_lives_is_not_part_of_root_identity() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cfe/one"]);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone()]);

        let with_cache = roots.clone().with_excluded(vec![dir.path().join(".build")]);
        let with_another = roots.clone().with_excluded(vec![dir.path().join("elsewhere")]);
        assert_eq!(with_cache, with_another, "the cache path entered root identity");
        assert_eq!(with_cache, roots, "declaring a cache at all entered root identity");

        // Positive control: the roots themselves still decide, or the assertions above
        // would hold on a build where every table equals every other.
        let (moved, _) = WorkspaceRoots::build(dir.path(), &made[1], &[]);
        assert_ne!(with_cache, moved, "two different root sets compared equal");
    }

    #[test]
    fn user_exclusions_change_identity_and_reject_addressed_files() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf"]);
        let hidden = made[0].join("Generated");
        let visible = made[0].join("Generated2");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::create_dir_all(&visible).unwrap();
        let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[]);
        let scoped =
            roots.clone().with_user_excluded(&project_model::ExcludedPaths::new([hidden.clone()]));

        assert_ne!(roots, scoped, "changing only [source].exclude reused root identity");
        assert!(scoped.key_of_path(&hidden.join("Module.bsl")).is_none());
        assert_eq!(
            scoped.key_of_path(&visible.join("Module.bsl")),
            Some(FileKey::configuration("Generated2/Module.bsl"))
        );
    }

    #[test]
    fn optional_roots_do_not_invent_a_configuration_for_extension_only_projects() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["Расширения/FeatureA", "Расширения/FeatureB"]);
        let (roots, rejected) =
            WorkspaceRoots::build_optional(dir.path(), None, &[made[0].clone(), made[1].clone()]);

        assert!(rejected.is_empty());
        assert!(roots.configuration().is_none(), "workspace root is not a synthetic configuration");
        let entries: Vec<_> =
            roots.entries().map(|(id, path)| (id.to_owned(), path.to_path_buf())).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            owner(&roots, &made[0].join(MODULE)),
            Some(FileKey::new("Расширения/FeatureA", MODULE))
        );
        assert_eq!(
            owner(&roots, &made[1].join(MODULE)),
            Some(FileKey::new("Расширения/FeatureB", MODULE))
        );
    }

    #[test]
    fn each_root_keeps_its_own_copy_of_the_same_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cfe/one", "cfe/two"]);
        let (roots, rejected) =
            WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone(), made[2].clone()]);

        assert!(rejected.is_empty(), "roots beside the configuration are all registered");
        for (root, expected_id) in
            [(&made[0], CONFIGURATION_ROOT_ID), (&made[1], "cfe/one"), (&made[2], "cfe/two")]
        {
            assert_eq!(
                owner(&roots, &root.join(MODULE)),
                Some(FileKey::new(expected_id, MODULE.to_owned())),
                "the same relative path under {root:?} must key to its own root"
            );
        }
    }

    /// The key is spelled with `/` however the host joins path components.
    ///
    /// The fixture assembles every path component by component with `Path::join`, which
    /// yields the HOST separator: a literal `"cfe/one"` would hide the defect, because
    /// Windows accepts a forward slash in input and the key would come out right by luck.
    #[test]
    fn a_key_is_spelled_with_slashes_whatever_the_host_separator() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let configuration = workspace.join("cf");
        let one = Path::new("cfe").join("one");
        let two = Path::new("cfe").join("two");
        for rel in [&one, &two] {
            std::fs::create_dir_all(workspace.join(rel)).unwrap();
        }
        std::fs::create_dir_all(&configuration).unwrap();
        let (roots, rejected) = WorkspaceRoots::build(
            workspace,
            &configuration,
            &[workspace.join(&one), workspace.join(&two)],
        );
        assert!(rejected.is_empty(), "both extensions are registered");

        let file =
            workspace.join(&one).join("CommonModules").join("М").join("Ext").join("Module.bsl");
        let key = owner(&roots, &file).expect("the file lies under a registered root");
        assert_eq!(key, FileKey::new("cfe/one", "CommonModules/М/Ext/Module.bsl".to_owned()));

        let other = owner(&roots, &workspace.join(&two).join("CommonModules"))
            .expect("the sibling root owns its own files");
        assert_eq!(other.root_id, "cfe/two", "control: distinct roots stay distinct ids");
        assert_ne!(key.root_id, other.root_id);
    }

    /// A key built through `FileKey::new` is `/`-spelled whatever separator the caller used.
    ///
    /// Assembled component by component with `Path::join`, which yields the HOST separator: a
    /// literal `"a/b"` would pass on Windows by luck and hide the defect.
    #[test]
    fn a_key_built_from_a_host_joined_path_is_slash_spelled() {
        let root = Path::new("extensions").join("provider-a");
        let rel = Path::new("CommonModules").join("М").join("Ext").join("Module.bsl");
        let key = FileKey::new(root.to_string_lossy(), rel.to_string_lossy());
        assert_eq!(key.root_id, "extensions/provider-a");
        assert_eq!(key.path, "CommonModules/М/Ext/Module.bsl");
        assert_eq!(key, FileKey::new("extensions/provider-a", "CommonModules/М/Ext/Module.bsl"));
    }

    /// Control: a backslash inside a Unix file name is a character, not a separator.
    #[cfg(not(windows))]
    #[test]
    fn a_backslash_in_a_unix_file_name_is_not_rewritten() {
        let key = FileKey::new("ext\\one", "dir\\file.bsl");
        assert_eq!(key.root_id, "ext\\one");
        assert_eq!(key.path, "dir\\file.bsl");
    }

    #[test]
    fn the_innermost_root_owns_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cfe/outer", "cfe/outer/inner"]);
        let (roots, _) =
            WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone(), made[2].clone()]);

        assert_eq!(
            owner(&roots, &made[2].join(MODULE)),
            Some(FileKey::new("cfe/outer/inner", MODULE.to_owned())),
            "the nested root is the owner, not the one that merely contains it"
        );
        assert_eq!(
            owner(&roots, &made[1].join(MODULE)),
            Some(FileKey::new("cfe/outer", MODULE.to_owned())),
            "a file outside the nested root still belongs to the outer one"
        );
    }

    /// Prefix matching that ignored component boundaries would hand every file
    /// of `cf_ext` to `cf`, keyed by a path starting with `_ext/`.
    #[test]
    fn a_root_never_swallows_a_sibling_whose_name_starts_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cf_ext"]);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone()]);

        assert_eq!(
            owner(&roots, &made[1].join(MODULE)),
            Some(FileKey::new("cf_ext", MODULE.to_owned())),
            "`cf` must not claim `cf_ext`"
        );
    }

    /// Declaration order decides nothing: the roots are ranked by how deep they
    /// reach, exactly as the semantics rank them.
    #[test]
    fn attribution_does_not_depend_on_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cfe/outer", "cfe/outer/inner"]);
        let file = made[2].join(MODULE);

        let (forward, _) =
            WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone(), made[2].clone()]);
        let (backward, _) =
            WorkspaceRoots::build(dir.path(), &made[0], &[made[2].clone(), made[1].clone()]);

        assert_eq!(owner(&forward, &file), owner(&backward, &file));
    }

    #[test]
    fn an_extension_inside_the_configuration_is_reported_and_left_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cf/nested"]);
        let (roots, rejected) = WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone()]);

        assert_eq!(
            rejected,
            vec![RejectedRoot {
                path: made[1].clone(),
                reason: Rejection::InsideConfiguration { root: made[0].clone() },
            }],
            "the overlap must be named, not swallowed"
        );
        // Not lost: the configuration walk is recursive, so the file is still
        // found — as the configuration's, which is what the publisher and the
        // graph enumeration call it too.
        assert_eq!(
            owner(&roots, &made[1].join(MODULE)),
            Some(FileKey::new(CONFIGURATION_ROOT_ID, format!("nested/{MODULE}"))),
        );
    }

    /// The other direction of the same overlap: an extension declared *around*
    /// the configuration. Rejecting it would lose the files outside the
    /// configuration entirely — its walk never reaches them.
    #[test]
    fn an_extension_around_the_configuration_keeps_what_lies_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["src/cf", "src/own"]);
        let workspace = dir.path().to_path_buf();
        let (roots, rejected) =
            WorkspaceRoots::build(&workspace, &made[0], std::slice::from_ref(&workspace));

        assert!(rejected.is_empty(), "containing the configuration is not an overlap to reject");
        assert_eq!(
            owner(&roots, &made[0].join(MODULE)),
            Some(FileKey::new(CONFIGURATION_ROOT_ID, MODULE.to_owned())),
            "the configuration's own subtree stays its own"
        );
        assert_eq!(
            owner(&roots, &made[1].join(MODULE)),
            Some(FileKey::new(".", format!("src/own/{MODULE}"))),
            "everything outside it belongs to the surrounding root"
        );
    }

    /// A root equal to the workspace must not take the configuration's reserved
    /// id: the two would then share one key space, and the second write would
    /// overwrite the first.
    #[test]
    fn a_root_spanning_the_workspace_gets_a_non_empty_id() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["src/cf"]);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[dir.path().to_path_buf()]);

        let ids: Vec<&str> = roots.ids().collect();
        assert_eq!(ids, vec![CONFIGURATION_ROOT_ID, "."]);
    }

    #[test]
    fn a_root_equal_to_the_configuration_stays_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf"]);
        let (roots, rejected) = WorkspaceRoots::build(dir.path(), &made[0], &[made[0].clone()]);

        assert_eq!(roots.ids().collect::<Vec<_>>(), vec![CONFIGURATION_ROOT_ID]);
        assert_eq!(rejected.len(), 1, "the duplicate root is reported");
        assert_eq!(
            owner(&roots, &made[0].join(MODULE)),
            Some(FileKey::new(CONFIGURATION_ROOT_ID, MODULE.to_owned())),
            "one file must not produce both a configuration row and an extension row"
        );
    }

    /// The identifier is a lossy rendering of a path, so two directories that
    /// differ only in bytes no `str` can hold render the same. Registering both
    /// would give one key space to two roots: the second's rows would overwrite
    /// the first's, and `resolve` would hand back the wrong directory for either.
    #[cfg(unix)]
    #[test]
    fn a_root_whose_identifier_is_already_taken_is_rejected() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let workspace = canonical_root(&dir);
        let made = dirs(&workspace, &["cf"]);
        let first = unrepresentable_dir(workspace.join(OsString::from_vec(vec![b'a', 0x80])));
        let second = unrepresentable_dir(workspace.join(OsString::from_vec(vec![b'a', 0x81])));

        let (roots, rejected) =
            WorkspaceRoots::build(&workspace, &made[0], &[first.clone(), second.clone()]);

        let ids: Vec<&str> = roots.ids().collect();
        assert_eq!(ids.len(), 2, "the configuration and exactly one of the two: {ids:?}");
        assert_eq!(
            rejected,
            vec![RejectedRoot {
                path: second,
                reason: Rejection::IdentifierTaken { id: ids[1].to_owned() },
            }],
            "the dropped root is named, not swallowed"
        );
        assert_eq!(
            roots.resolve(&FileKey::new(ids[1], MODULE)).as_deref(),
            Some(first.join(MODULE).as_path()),
            "the surviving root keeps its own directory"
        );
    }

    #[test]
    fn a_key_resolves_back_to_the_file_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let made = dirs(dir.path(), &["cf", "cfe/one"]);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone()]);

        for root in [&made[0], &made[1]] {
            let file = root.join(MODULE);
            let key = owner(&roots, &file).unwrap();
            assert_eq!(roots.resolve(&key).as_deref(), Some(file.as_path()));
        }
    }

    #[cfg(unix)]
    mod aliases {
        use super::*;

        fn link(target: &Path, at: &Path) {
            std::os::unix::fs::symlink(target, at).unwrap();
        }

        /// Reached through `one/Linked`, the file physically lives under `two`.
        /// Ranking by the canonical spelling is what makes the answer the same
        /// as the semantics', and the same in both declaration orders.
        #[test]
        fn an_alias_does_not_move_a_file_to_the_root_it_was_reached_through() {
            let dir = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf", "cfe/one", "cfe/two"]);
            link(&made[2], &made[1].join("Linked"));

            let (roots, _) =
                WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone(), made[2].clone()]);

            let walked = made[1].join("Linked").join(MODULE);
            // Read off the filesystem, as the enumerator reads it: joining onto the
            // DECLARED root would carry over whatever links lead to the fixture itself
            // (`/var` → `/private/var` on macOS) and hand `root_of` a spelling that is
            // not canonical at all, which is the one question this test does not ask.
            let canonical = std::fs::canonicalize(&made[2]).unwrap().join(MODULE);
            assert_eq!(
                roots.root_of(&walked, &canonical),
                Some(FileKey::new("cfe/two", MODULE.to_owned())),
                "the root the file lives in owns it, not the one that links to it"
            );
        }

        /// A link out of every root. The graph has always followed it, so the
        /// file must stay in the universe; with no canonical owner it is kept by
        /// the root whose walk arrived there, keyed by the walked spelling.
        #[test]
        fn a_file_outside_every_root_is_kept_by_the_root_that_walked_to_it() {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf"]);
            std::fs::create_dir_all(outside.path().join("tree")).unwrap();
            link(&outside.path().join("tree"), &made[0].join("Linked"));

            let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[]);

            let walked = made[0].join("Linked").join(MODULE);
            let canonical =
                std::fs::canonicalize(outside.path()).unwrap().join("tree").join(MODULE);
            assert_eq!(
                roots.root_of(&walked, &canonical),
                Some(FileKey::new(CONFIGURATION_ROOT_ID, format!("Linked/{MODULE}"))),
            );
        }

        /// An extension declared through an alias that sits inside the
        /// configuration, while the extension itself lies outside it. Deciding
        /// on the declared spelling would reject the root and lose its files.
        #[test]
        fn an_extension_declared_through_an_alias_inside_the_configuration_is_registered() {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf"]);
            let real = outside.path().join("ext");
            std::fs::create_dir_all(&real).unwrap();
            let alias = made[0].join("Linked");
            link(&real, &alias);

            let (roots, rejected) =
                WorkspaceRoots::build(dir.path(), &made[0], std::slice::from_ref(&alias));

            assert!(
                rejected.is_empty(),
                "only the alias is inside the configuration, not the root"
            );
            let file = alias.join(MODULE);
            let canonical = std::fs::canonicalize(&real).unwrap().join(MODULE);
            let key = roots.root_of(&file, &canonical).unwrap();
            assert_ne!(key.root_id, CONFIGURATION_ROOT_ID, "the extension keeps its own identity");
            assert_eq!(key.path, MODULE);
        }
    }

    /// Attribution of a path that is not a stored document: a metadata descriptor asks the
    /// same question a `.bsl` does, and must not get a second answer.
    mod any_path {
        use super::*;

        #[test]
        fn a_descriptor_belongs_to_the_root_it_lies_in() {
            let dir = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf", "cfe/one"]);
            let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[made[1].clone()]);

            assert_eq!(
                roots.key_of_path(&made[1].join("Configuration.xml")),
                Some(FileKey::new("cfe/one", "Configuration.xml".to_owned())),
            );
            assert_eq!(
                roots.key_of_path(&made[0].join("Catalogs/Товары.xml")),
                Some(FileKey::configuration("Catalogs/Товары.xml")),
            );
        }

        /// The reading a relative path gets everywhere in the engine: the configuration
        /// root, which is where every stored path with the reserved id is spelled from.
        #[test]
        fn a_relative_path_is_read_against_the_configuration_root() {
            let dir = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["src/cf"]);
            let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[]);

            assert_eq!(
                roots.key_of_path(Path::new("Configuration.xml")),
                Some(FileKey::configuration("Configuration.xml")),
            );
        }

        #[test]
        fn a_path_outside_every_root_has_no_key() {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf"]);
            let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[]);

            assert_eq!(roots.key_of_path(&outside.path().join("Configuration.xml")), None);
        }
    }

    /// Paths that arrive already rendered, bytes no `str` can carry replaced. Where those
    /// bytes sat decides the answer: below a root the key is the row's own, in a root's own
    /// name there is no answer to give.
    #[cfg(unix)]
    mod rendered_paths {
        use super::*;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        /// A workspace whose OWN name holds bytes no `str` can carry, holding a
        /// configuration root and one extension.
        fn workspace_named_by_bytes(dir: &tempfile::TempDir) -> (PathBuf, Vec<PathBuf>) {
            let workspace = unrepresentable_dir(
                canonical_root(dir).join(OsString::from_vec(b"ws\xff".to_vec())),
            );
            let made =
                ["cf", "cfe"].iter().map(|rel| unrepresentable_dir(workspace.join(rel))).collect();
            (workspace, made)
        }

        /// Below the root the rendering costs nothing, because the stored key is a rendering
        /// too: `key_of` puts the relative path through the same conversion, so the real
        /// bytes and what came back from a string-keyed store name ONE key — the one the row
        /// lives under. What stays true of that key space is what has always been true of it:
        /// two files whose names differ only in unrepresentable bytes share a key, whichever
        /// spelling reaches it.
        #[test]
        fn a_rendering_below_the_root_names_the_key_the_row_lives_under() {
            let dir = tempfile::tempdir().unwrap();
            let made = dirs(dir.path(), &["cf"]);
            let (roots, _) = WorkspaceRoots::build(dir.path(), &made[0], &[]);

            let file =
                made[0].join(OsString::from_vec(b"CommonModules/M\xff/Ext/Module.bsl".to_vec()));
            let rendered = PathBuf::from(file.to_string_lossy().into_owned());
            assert_eq!(
                owner(&roots, &rendered),
                owner(&roots, &file),
                "both spellings key the same row",
            );
            assert!(owner(&roots, &file).is_some(), "and that row exists to be keyed");
        }

        /// Roots may nest, so a rendering that unseats the inner root can still land on an
        /// ancestor. The file's own row stays keyed by the inner root, so the key this
        /// produces normally carries no row at all. Said plainly here because the shorter
        /// reading — that such a path attributes to nothing — is what a tidier contract
        /// would promise and the code does not deliver.
        #[test]
        fn a_rendering_inside_a_nested_roots_name_lands_on_the_ancestor() {
            let dir = tempfile::tempdir().unwrap();
            let workspace = canonical_root(&dir);
            let made = dirs(&workspace, &["cf", "ext"]);
            let inner =
                unrepresentable_dir(made[1].join(OsString::from_vec(b"d\xff".to_vec())).join("e"));
            let (roots, rejected) =
                WorkspaceRoots::build(&workspace, &made[0], &[made[1].clone(), inner.clone()]);
            assert!(rejected.is_empty(), "an extension inside an extension is a root of its own");

            let file = inner.join(MODULE);
            assert_eq!(
                owner(&roots, &file),
                Some(FileKey::new("ext/d\u{FFFD}/e", MODULE.to_owned())),
                "the real bytes key the file under the innermost root",
            );
            let rendered = PathBuf::from(file.to_string_lossy().into_owned());
            assert_eq!(
                owner(&roots, &rendered),
                Some(FileKey::new("ext", format!("d\u{FFFD}/e/{MODULE}"))),
                "the rendering keeps the ancestor, whose key no row carries",
            );
        }

        /// A rendering of the ROOT's own name fits several roots at once — two that differ
        /// only in those bytes, a root and a neighbour holding a genuine `U+FFFD`, a nesting
        /// that exists only after rendering — so it names no file anyone may act on. Every
        /// caller already reads `None` as "not ours": the graph-sourced context mark is
        /// skipped and the module waits for a wider mark. Marks driven by the watcher carry
        /// the real bytes and are untouched by this.
        #[test]
        fn a_path_that_arrives_rendered_belongs_to_no_root() {
            let dir = tempfile::tempdir().unwrap();
            let (workspace, made) = workspace_named_by_bytes(&dir);
            let (roots, _) = WorkspaceRoots::build(&workspace, &made[0], &[made[1].clone()]);

            let file = made[1].join(MODULE);
            assert_eq!(
                owner(&roots, &file),
                Some(FileKey::new("cfe", MODULE.to_owned())),
                "the real bytes attribute as always",
            );

            let rendered = PathBuf::from(file.to_string_lossy().into_owned());
            assert_eq!(owner(&roots, &rendered), None, "the rendering names no file we can act on");
        }

        /// The identifier of such a root IS a rendering, and that is not the same question:
        /// it names a key space, it is never resolved back to bytes.
        #[test]
        fn the_identifier_of_an_unrepresentable_root_is_still_usable() {
            let dir = tempfile::tempdir().unwrap();
            let (workspace, made) = workspace_named_by_bytes(&dir);
            let (roots, _) = WorkspaceRoots::build(&workspace, &made[0], &[made[1].clone()]);

            let key = owner(&roots, &made[1].join(MODULE)).unwrap();
            assert_eq!(roots.resolve(&key).as_deref(), Some(made[1].join(MODULE).as_path()));
        }
    }

    #[test]
    fn rebound_ids_are_the_kept_ids_whose_directory_changed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let old_made = dirs(&workspace, &["old-cf", "ext/kept", "ext/removed"]);
        let new_made = dirs(&workspace, &["new-cf", "ext/kept", "ext/added"]);
        let old = WorkspaceRoots::build(
            &workspace,
            &old_made[0],
            &[old_made[1].clone(), old_made[2].clone()],
        )
        .0;
        let next = WorkspaceRoots::build(
            &workspace,
            &new_made[0],
            &[new_made[1].clone(), new_made[2].clone()],
        )
        .0;

        let rebound = old.rebound_root_ids(&next);
        assert!(rebound.contains(CONFIGURATION_ROOT_ID), "the stable empty id was rebound");
        assert!(!rebound.contains("ext/kept"));
        assert!(!rebound.contains("ext/removed"), "a removed id is not rebound");
        assert!(!rebound.contains("ext/added"), "an added id is not rebound");
    }

    /// A retargeted root keeps its id and takes the fresh directory (github#277).
    ///
    /// The canonical spelling of a root is a fact about ONE generation: a root declared through a
    /// link resolves to whatever the link points at when the table is built. A rebuild after the
    /// link moved mints a NEW id for the same declared root, because `root_id_for` derives the id
    /// from the canonical spelling — and the id is the durable identity of every row and external
    /// address keyed by the root. `inheriting_root_ids` states the next generation without moving
    /// identity: same declared spelling, same id, fresh directory.
    #[test]
    fn a_retargeted_root_keeps_its_id_and_takes_the_fresh_directory() {
        let workspace = Path::new("/ws");
        let cf_declared = workspace.join("cf");
        let ext_declared = workspace.join("ext");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), cf_declared.clone(), cf_declared.clone()),
                ("ext".to_owned(), ext_declared.clone(), workspace.join("A")),
            ],
        );
        let fresh = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), cf_declared.clone(), cf_declared.clone()),
                ("/ws/B".to_owned(), ext_declared.clone(), workspace.join("B")),
            ],
        );

        let next = old.inheriting_root_ids(&fresh);

        assert_eq!(
            next.ids().collect::<Vec<_>>(),
            vec![CONFIGURATION_ROOT_ID, "ext"],
            "the ids of the generation being replaced are the ids of the next one",
        );
        let ext = next
            .entries_with_canonical()
            .find(|(id, ..)| *id == "ext")
            .expect("the declared root survived the retarget");
        assert_eq!(ext.1, ext_declared.as_path(), "the declared spelling is the fresh one");
        assert_eq!(ext.2, workspace.join("B").as_path(), "so is the canonical spelling");
    }

    /// What the stale snapshot does, and what re-stating the generation buys (github#277).
    ///
    /// The link `ext -> A` is retargeted to `B`, so one declared spelling walks to a different
    /// file. Against the stale table both files answer ONE key — the new target's canonical
    /// spelling is not a prefix of any registered root, so attribution falls back to the walked
    /// spelling — and a path named by the NEW target's own canonical spelling has no key at all.
    /// Re-stating the generation under the same id fixes the second and keeps the first: the id
    /// keeps its key space across a rebind (that is what `rebound_root_ids` and the transition are
    /// for), so the two files are still told apart by their CONTENT under one id, not by two ids.
    #[test]
    fn a_stale_generation_hides_a_retargeted_root_until_it_is_restated() {
        let workspace = Path::new("/ws");
        let cf_declared = workspace.join("cf");
        let ext_declared = workspace.join("ext");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), cf_declared.clone(), cf_declared.clone()),
                ("ext".to_owned(), ext_declared.clone(), workspace.join("A")),
            ],
        );
        let fresh = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), cf_declared.clone(), cf_declared.clone()),
                ("/ws/B".to_owned(), ext_declared.clone(), workspace.join("B")),
            ],
        );
        let relative = "CommonModules/М/Ext/Module.bsl";
        let walked = ext_declared.join(relative);
        let was = workspace.join("A").join(relative);
        let now = workspace.join("B").join(relative);

        // The defect as the issue states it: one key stands for the old file and the new one.
        assert_eq!(old.root_of(&walked, &was), Some(FileKey::new("ext", relative)));
        assert_eq!(
            old.root_of(&walked, &now),
            Some(FileKey::new("ext", relative)),
            "control of the defect: the stale snapshot collapses the new target onto the old key",
        );
        // ... and a file NAMED by the new target's canonical spelling is outside every root it
        // knows, so the walk of the new target cannot reach its own key.
        assert_eq!(
            old.root_of(&now, &now),
            None,
            "control of the defect: the stale snapshot does not know the fresh target at all",
        );

        let next = old.inheriting_root_ids(&fresh);

        assert_eq!(
            next.root_of(&now, &now),
            Some(FileKey::new("ext", relative)),
            "the fresh generation attributes the retargeted root's file under the SAME id",
        );
        assert_eq!(
            next.root_of(&walked, &now),
            Some(FileKey::new("ext", relative)),
            "and the walked spelling still reaches it, as it did before the retarget",
        );
        assert_eq!(
            next.root_of(&walked, &was),
            Some(FileKey::new("ext", relative)),
            "the row of the old target is still attributable by the spelling it was indexed under",
        );
    }

    /// A table whose WORKSPACE resolves elsewhere is another registration, not this one's next
    /// generation: ids are stripped against the workspace, so the two share no key space and
    /// re-stating one under the other would decode ids of the wrong base. The fresh table is kept
    /// as it is.
    ///
    /// The two registrations mint DIFFERENT ids for the same declared root — which is what a
    /// workspace link retarget does, since the id is drawn from the canonical spelling relative to
    /// the workspace — and that is the control below: with the guard removed, the fresh root would
    /// inherit `ext` from the other registration and the answer would read as a re-statement. A
    /// fixture where both tables minted one id could not tell the two apart.
    #[test]
    fn a_workspace_that_resolves_elsewhere_is_not_restated() {
        let old = WorkspaceRoots::with_stated_spellings(
            Path::new("/ws"),
            vec![("ext".to_owned(), PathBuf::from("/ws/ext"), PathBuf::from("/ws/A"))],
        );
        let elsewhere = WorkspaceRoots::with_stated_spellings(
            Path::new("/other"),
            vec![("/other/B".to_owned(), PathBuf::from("/ws/ext"), PathBuf::from("/other/B"))],
        );

        assert_ne!(
            old.workspace_canonical(),
            elsewhere.workspace_canonical(),
            "control: the two tables strip their ids against different directories",
        );
        assert_ne!(
            old.ids().collect::<Vec<_>>(),
            elsewhere.ids().collect::<Vec<_>>(),
            "control: and they mint different ids, so a re-statement would be visible here",
        );
        assert_eq!(old.inheriting_root_ids(&elsewhere), elsewhere);
    }

    /// Two fresh roots cannot inherit one id: that would give two directories one key space, the
    /// one thing a root id may not do. The refused root keeps the id it was built with, and
    /// nothing panics.
    #[test]
    fn a_second_root_may_not_inherit_an_id_that_is_already_taken() {
        let workspace = Path::new("/ws");
        let declared = PathBuf::from("/ws/one");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![("kept".to_owned(), declared.clone(), PathBuf::from("/ws/A"))],
        );
        // Both fresh roots carry the SAME declared spelling, which `build` refuses to produce but
        // a caller of the stating constructor can: the guard is about the id, not the directory.
        let fresh = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                ("first".to_owned(), declared.clone(), PathBuf::from("/ws/B")),
                ("second".to_owned(), declared, PathBuf::from("/ws/C")),
            ],
        );

        let next = old.inheriting_root_ids(&fresh);

        assert_eq!(
            next.ids().collect::<Vec<_>>(),
            vec!["kept", "second"],
            "the id is inherited once; the refused root keeps its own",
        );
        assert_eq!(
            next.resolve_walked(&FileKey::new("second", "M.bsl")).as_deref(),
            Some(Path::new("/ws/C/M.bsl")),
            "and the refused root keeps the directory it was built with",
        );
    }

    /// A retarget whose abandoned directory is declared as a root of the SAME generation
    /// (github#277, MAJOR-1).
    ///
    /// The link `ext` used to lead to `y`, so the old table minted `y` as its id. In the next
    /// generation the link leads to `z` (its natural id is `z`) and `y` — no longer reached through
    /// the link — is declared as a root of its own (natural id `y`). One id cannot be both: it
    /// belongs to the root it is NATURAL for, so the abandoned directory keeps `y` and the
    /// retargeted link keeps its fresh `z`. The re-canonicalisation of the link is refused here —
    /// deliberately, and documented on `inheriting_root_ids` — because inheriting would put two
    /// directories in one key space.
    ///
    /// Both declaration orders are built, because a rule applied root by root would hand `y` to the
    /// first root seen and pass exactly one of these.
    #[test]
    fn an_id_belongs_to_the_fresh_root_it_is_natural_for_whatever_the_order() {
        let workspace = Path::new("/ws");
        let link = workspace.join("ext");
        let abandoned = workspace.join("y");
        let retargeted = workspace.join("z");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![("y".to_owned(), link.clone(), abandoned.clone())],
        );
        let fresh_forward = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                ("z".to_owned(), link.clone(), retargeted.clone()),
                ("y".to_owned(), abandoned.clone(), abandoned.clone()),
            ],
        );
        let fresh_backward = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                ("y".to_owned(), abandoned.clone(), abandoned.clone()),
                ("z".to_owned(), link.clone(), retargeted.clone()),
            ],
        );

        for (order, fresh) in [("retarget first", &fresh_forward), ("owner first", &fresh_backward)]
        {
            let next = old.inheriting_root_ids(fresh);
            let ids: Vec<&str> = next.ids().collect();
            assert_eq!(ids.len(), 2, "one root per declared spelling ({order}): {ids:?}",);
            let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
            assert_eq!(unique.len(), ids.len(), "two roots share one key space ({order}): {ids:?}",);
            let mut bound: Vec<(String, PathBuf)> = next
                .entries_with_canonical()
                .map(|(id, _, canonical)| (id.to_owned(), canonical.to_path_buf()))
                .collect();
            bound.sort();
            assert_eq!(
                bound,
                vec![("y".to_owned(), abandoned.clone()), ("z".to_owned(), retargeted.clone())],
                "the id stays with the root it is natural for, and the retargeted link gets its \
                 own ({order})",
            );
        }

        // The stability control: the SAME retarget with no new owner of the old id still inherits,
        // which is the promise `inheriting_root_ids` exists for (github#277).
        let without_owner = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![("z".to_owned(), link.clone(), retargeted.clone())],
        );
        let next = old.inheriting_root_ids(&without_owner);
        assert_eq!(
            next.ids().collect::<Vec<_>>(),
            vec!["y"],
            "a retarget that nobody else claims still keeps the root's id",
        );
        assert_eq!(
            next.entries_with_canonical().next().map(|(_, declared, canonical)| {
                (declared.to_path_buf(), canonical.to_path_buf())
            }),
            Some((link, retargeted)),
            "under the fresh declared and canonical spellings",
        );
    }

    /// The reserved empty id never moves (MAJOR-2 of the audit round).
    ///
    /// A legal config edit moves the base onto the directory an extension used to be declared at.
    /// Matching by the declared spelling alone hands the configuration the retired extension's id
    /// — the empty id then disappears, `configuration()`, `configuration_root()` and every stored
    /// path with the reserved id break, and the transition reads `""` as changed and clears the
    /// whole configuration corpus.
    #[test]
    fn the_configuration_keeps_the_reserved_id_when_the_base_moves_onto_an_extension() {
        let workspace = Path::new("/ws");
        let old_base = workspace.join("src/cf");
        let ex_extension = workspace.join("vendor/cf");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), old_base.clone(), old_base.clone()),
                ("vendor/cf".to_owned(), ex_extension.clone(), ex_extension.clone()),
            ],
        );
        // The extension is gone; the base now sits where it was declared.
        let fresh = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![(CONFIGURATION_ROOT_ID.to_owned(), ex_extension.clone(), ex_extension.clone())],
        );

        let next = old.inheriting_root_ids(&fresh);

        assert_eq!(
            next.ids().collect::<Vec<_>>(),
            vec![CONFIGURATION_ROOT_ID],
            "the reserved id stays with the configuration root and the retired extension's id is \
             not resurrected",
        );
        assert_eq!(
            next.configuration(),
            Some(ex_extension.as_path()),
            "so `configuration()` still names the base configuration",
        );
    }

    /// The same invariant in the other direction: an extension declared where the BASE used to be
    /// must not take the reserved empty id, or the configuration's rows would be read as the
    /// extension's.
    #[test]
    fn an_extension_declared_where_the_base_used_to_be_does_not_take_the_reserved_id() {
        let workspace = Path::new("/ws");
        let old_base = workspace.join("src/cf");
        let new_base = workspace.join("vendor/cf");
        let extension = workspace.join("ext");
        let old = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), old_base.clone(), old_base.clone()),
                ("ext".to_owned(), extension.clone(), extension.clone()),
            ],
        );
        let fresh = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![
                (CONFIGURATION_ROOT_ID.to_owned(), new_base.clone(), new_base.clone()),
                ("src/cf".to_owned(), old_base.clone(), old_base.clone()),
            ],
        );

        let next = old.inheriting_root_ids(&fresh);

        let ids: Vec<&str> = next.ids().collect();
        assert_eq!(ids.len(), 2, "every fresh root survives: {ids:?}");
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 2, "two roots share the reserved id: {ids:?}");
        assert_eq!(
            next.configuration(),
            Some(new_base.as_path()),
            "the configuration keeps the reserved id wherever its directory is",
        );
        assert_eq!(
            next.entries_with_canonical()
                .find(|(id, ..)| *id == "src/cf")
                .map(|(_, declared, _)| declared.to_path_buf()),
            Some(old_base.clone()),
            "and the extension declared on the old base's directory keeps its own id",
        );

        // The same match with no configuration root in the fresh table to mask it: an
        // extension-only generation must not resurrect the reserved id either, or `configuration()`
        // would name an extension and the configuration's stored paths would be read under it.
        let extension_only = WorkspaceRoots::with_stated_spellings(
            workspace,
            vec![("src/cf".to_owned(), old_base.clone(), old_base.clone())],
        );
        let next = old.inheriting_root_ids(&extension_only);
        assert_eq!(
            next.ids().collect::<Vec<_>>(),
            vec!["src/cf"],
            "an extension never takes the reserved empty id",
        );
        assert!(next.configuration().is_none(), "and it must not become the configuration",);
    }

    /// The registration of a reader that holds an older generation reads as stale once the declared
    /// root leads elsewhere, and as current whenever the id was minted from a spelling that resolved
    /// to nowhere — the fail-open half of the rule (M6/M9 of the audit round).
    #[test]
    fn a_registration_reads_stale_only_when_the_declared_root_leads_somewhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let made = dirs(&base, &["a", "b"]);

        let stale = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("ext".to_owned(), made[0].clone(), made[1].clone())],
        );
        assert!(
            !stale.registration_is_current("ext"),
            "the declared spelling leads to `a` while the table states `b`",
        );

        let current = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("ext".to_owned(), made[0].clone(), std::fs::canonicalize(&made[0]).unwrap())],
        );
        assert!(
            current.registration_is_current("ext"),
            "declared and stated canonical spellings agree",
        );

        let vanished = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![(
                "gone".to_owned(),
                base.join("no-such-directory"),
                base.join("no-such-directory"),
            )],
        );
        assert!(
            vanished.registration_is_current("gone"),
            "a spelling that never resolved cannot prove anything — nothing to disprove",
        );

        assert!(
            current.registration_is_current("never-registered"),
            "an id the table never registered has no registration to be stale",
        );
    }

    /// The fail-open half of the rule, on its own so it can be mutated and read separately (M9).
    ///
    /// An id minted from a spelling the table could NOT resolve (`canonical == declared`) is current
    /// while that spelling stays unresolvable — nothing was proven then, nothing is disproven now —
    /// and stays current when the destination appears AT THAT SAME SPELLING, which is the spelling
    /// resolving to itself. A root that is merely absent, unreadable or never created is answered
    /// for exactly as the generation that minted it answered for it.
    #[test]
    fn a_registration_minted_from_an_unresolvable_spelling_stays_current() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();

        // (1) The destination did not exist when the table was built: the table stored the spelling
        // itself. Absent now, and absent then — current.
        let appears_later = base.join("appears-later");
        let unresolvable = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("late".to_owned(), appears_later.clone(), appears_later.clone())],
        );
        assert!(
            unresolvable.registration_is_current("late"),
            "the table never resolved this spelling, so its absence disproves nothing",
        );
        // ... and the destination appearing IS the spelling resolving to itself: still current.
        std::fs::create_dir_all(&appears_later).unwrap();
        assert!(
            unresolvable.registration_is_current("late"),
            "the spelling now resolves to the directory the id was minted from",
        );

        // (2) The same shape on a plain (non-link) root whose declared spelling simply is not there.
        let never = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("never".to_owned(), base.join("never-created"), base.join("never-created"))],
        );
        assert!(
            never.registration_is_current("never"),
            "a root that was never on disk has no resolved directory to have left",
        );
    }

    /// The two sides of the resolved/spelling split, on real links.
    ///
    /// An id minted from a RESOLVED directory (`canonical != declared`, i.e. a link onto a live
    /// target) remembers that target: it is stale once the link no longer leads there, whether it was
    /// redirected onto a live directory elsewhere or onto nothing at all (M10). An id minted from the
    /// SPELLING (the table could not resolve it, so `canonical == declared`) remembers no target and
    /// follows its spelling wherever it leads — a link that appears later, and even one redirected
    /// onto a third directory, stays current (M9, the contract this round settled).
    #[cfg(unix)]
    #[test]
    fn an_id_minted_from_a_resolved_directory_is_stale_once_that_directory_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let made = dirs(&base, &["targets/one", "targets/two"]);

        // The link is live when the table is built, so the id is minted from where it resolved to.
        let link = base.join("ext");
        std::os::unix::fs::symlink(&made[0], &link).unwrap();
        let resolved = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("ext".to_owned(), link.clone(), std::fs::canonicalize(&link).unwrap())],
        );
        assert!(
            resolved.registration_is_current("ext"),
            "control: a live link onto the directory the id was minted from is current",
        );

        // Redirected onto a target that does not exist: the declared spelling stops resolving, and
        // the id was NOT minted from the spelling — nothing can be proven, so it is stale.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(base.join("targets/gone"), &link).unwrap();
        assert!(
            !resolved.registration_is_current("ext"),
            "the id was minted from the target this link resolved to, and it no longer leads there",
        );

        // The other half of the contract, and the shape that decides it: a link the table could not
        // resolve when the id was minted (`canonical == declared` — "spelling-minted") remembers NO
        // target, so it is current whatever its spelling leads to now. Such a root is attributed by
        // its own spelling and opened through it again, i.e. it FOLLOWS the spelling; a link that
        // appears later and leads somewhere else is a legal configuration, not a stale registration.
        let later_link = base.join("later-link");
        std::os::unix::fs::symlink(base.join("targets/new"), &later_link).unwrap();
        let dangling = WorkspaceRoots::with_stated_spellings(
            &base,
            vec![("later-link".to_owned(), later_link.clone(), later_link.clone())],
        );
        assert!(
            dangling.registration_is_current("later-link"),
            "while the target is missing the spelling is followed as it stands",
        );
        std::fs::create_dir_all(base.join("targets/new")).unwrap();
        assert!(
            dangling.registration_is_current("later-link"),
            "and the spelling-minted id follows the target that appeared, wherever it leads",
        );
        // ... including when it leads somewhere OTHER than the directory that appeared first — the
        // id has no target memory, so a redirect cannot contradict it.
        let other = base.join("targets/other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::remove_file(&later_link).unwrap();
        std::os::unix::fs::symlink(&other, &later_link).unwrap();
        assert!(
            dangling.registration_is_current("later-link"),
            "a spelling-minted root follows its spelling: there is no remembered target to disprove",
        );
    }

    /// The same re-statement on a real retarget: a root declared through a live link moves to
    /// another target, and the generation built afterwards keeps the id while attribution follows
    /// the new target. A rebuild WITHOUT re-stating is the control: it reaches for a new id,
    /// because the id is drawn from the canonical spelling.
    #[cfg(unix)]
    #[test]
    fn a_live_retarget_is_restated_under_the_same_root_id() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = canonical_root(&dir);
        let made = dirs(&workspace, &["cf", "targets/one", "targets/two"]);
        let link = workspace.join("ext");
        std::os::unix::fs::symlink(&made[1], &link).unwrap();
        let (old, _) = WorkspaceRoots::build(&workspace, &made[0], std::slice::from_ref(&link));
        let ext_id = old
            .ids()
            .find(|id| *id != CONFIGURATION_ROOT_ID)
            .expect("the linked root is registered")
            .to_owned();
        assert_eq!(ext_id, "targets/one", "the id is the canonical spelling of the target");

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&made[2], &link).unwrap();
        let (fresh, _) = WorkspaceRoots::build(&workspace, &made[0], std::slice::from_ref(&link));

        assert!(
            !fresh.contains_id(&ext_id),
            "control: a rebuild alone mints the new target's id and orphans the old one",
        );
        assert!(fresh.contains_id("targets/two"));

        let next = old.inheriting_root_ids(&fresh);

        assert!(next.contains_id(&ext_id), "the id survives the retarget");
        assert!(!next.contains_id("targets/two"), "and no second id is minted for the same root");
        let walked = link.join(MODULE);
        let canonical = std::fs::canonicalize(&made[2]).unwrap().join(MODULE);
        assert_eq!(
            next.root_of(&walked, &canonical),
            Some(FileKey::new(ext_id.clone(), MODULE.to_owned())),
            "attribution follows the FRESH target while the key space stays the old one",
        );
        let named_by_target = made[2].join(MODULE);
        assert_eq!(
            old.root_of(&named_by_target, &named_by_target),
            None,
            "control: the stale snapshot does not hold the new target at all",
        );
        assert_eq!(
            next.root_of(&named_by_target, &named_by_target),
            Some(FileKey::new(ext_id, MODULE.to_owned())),
        );
    }

    mod containment {
        use super::*;

        #[test]
        fn a_directory_contains_its_files_at_any_depth_but_not_itself() {
            let dir = FileKey::configuration("Dir");
            assert!(FileKey::configuration("Dir/A.bsl").is_under(&dir));
            assert!(FileKey::configuration("Dir/Deep/B.bsl").is_under(&dir));
            assert!(!dir.is_under(&dir), "a directory is not a file inside itself");
        }

        /// The whole reason containment is compared by components: `Dir2` merely starts
        /// with the same text and is an unrelated directory.
        #[test]
        fn a_namesake_directory_is_not_inside() {
            assert!(!FileKey::configuration("Dir2/A.bsl").is_under(&FileKey::configuration("Dir")));
        }

        /// The same relative path under two roots is two different files, so a removal
        /// aimed at one root must not reach the other's.
        #[test]
        fn containment_does_not_cross_roots() {
            let file = FileKey::new("ext-1", "Dir/A.bsl");
            assert!(file.is_under(&FileKey::new("ext-1", "Dir")));
            assert!(!file.is_under(&FileKey::configuration("Dir")));
        }
    }

    #[test]
    fn resolve_rejects_native_and_foreign_absolute_or_parent_paths() {
        let dir = tempfile::tempdir().unwrap();
        let configuration = dirs(dir.path(), &["cf"]).remove(0);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &configuration, &[]);

        for path in [
            "/outside/file.bsl",
            "\\outside\\file.bsl",
            "C:/outside/file.bsl",
            r"C:\\outside\\file.bsl",
            r"\\server\share\file.bsl",
            "nested/../outside/file.bsl",
            r"nested\..\outside\file.bsl",
            "nested/./file.bsl",
            r"nested\.\file.bsl",
        ] {
            assert_eq!(
                roots.resolve(&FileKey::configuration(path)),
                None,
                "malformed persisted path must not be joined to a root: {path:?}",
            );
        }
    }

    #[test]
    fn resolve_accepts_a_relative_path_in_either_persisted_separator_form() {
        let dir = tempfile::tempdir().unwrap();
        let configuration = dirs(dir.path(), &["cf"]).remove(0);
        let (roots, _) = WorkspaceRoots::build(dir.path(), &configuration, &[]);

        assert_eq!(
            roots.resolve(&FileKey::configuration("nested/file.bsl")),
            Some(configuration.join("nested/file.bsl")),
        );
        assert_eq!(
            roots.resolve(&FileKey::configuration(r"nested\file.bsl")),
            Some(configuration.join(r"nested\file.bsl")),
        );
    }
}
