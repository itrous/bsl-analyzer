use std::error::Error;
use std::io;

/// The corpus of a source tree, together with the walk that produced it.
///
/// The walk is returned rather than consumed here because completeness is a publishing
/// POLICY: whether a tree that could not be read whole may still be shipped is the
/// publisher's decision, not the corpus builder's. Handing back the scan itself — not a
/// summary of it — keeps that decision answerable about THIS corpus.
pub(super) struct WorkspaceCorpus {
    pub(super) indexed_files: usize,
    pub(super) documents: Vec<bsl_search::IndexedDocument>,
    pub(super) walk: project_model::SourceSet,
    /// Files the walk reached and called source, whose bytes the ingest could not read. The
    /// walk's own counters are blind to these — `stat` needs no read permission — so a corpus
    /// can be short while the walk reports itself whole.
    pub(super) unreadable_files: usize,
}

/// Whether a declared root that did not become a registered one may be published anyway.
///
/// For the daemon a rejected root is a warning: the search index is a best effort and the
/// next scan can improve on it. For a published snapshot it is heavier — the snapshot says
/// what the corpus holds AND that everything else is gone, so a root whose files end up
/// under no key of their own is published as a deletion of every file in it.
///
/// Decided per variant rather than in bulk, because the two variants differ exactly here.
fn ensure_every_declared_root_is_accounted_for(
    rejected: &[bsl_search::RejectedRoot],
) -> Result<(), io::Error> {
    for rejection in rejected {
        match &rejection.reason {
            // Its files are already in the corpus: the configuration's own walk reaches
            // them and attribution hands them the configuration's key. Nothing is lost, so
            // the publish goes on.
            bsl_search::Rejection::InsideConfiguration { root } => {
                tracing::info!(
                    path = %rejection.path.display(),
                    configuration = %root.display(),
                    "extension root lies inside the configuration; its files are published \
                     under the configuration's key",
                );
            }
            // Two directories would share one key space, so one of them silently overwrites
            // the other. Whichever loses, the snapshot claims files it does not hold.
            bsl_search::Rejection::IdentifierTaken { id } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "refusing to publish: source root {} could not be registered because \
                         another root already holds the identifier {id:?}. Two roots sharing \
                         one key space give the snapshot one row where two files live, so the \
                         second is published as a deletion of the first. Rename or move the \
                         root and publish again.",
                        rejection.path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The key the publisher stores a walked file under: the engine's ONE role branch, never a second
/// one of its own.
///
/// `workspace_file_key_in` is the thin adapter over that branch, so a key written here is a key the
/// daemon derives for the same file: the drift and removal paths on the reading side already go
/// through this very adapter, and for a file the WALK produced its answer is the one a direct
/// [`bsl_search::WorkspaceRoots::root_of`] gives — the walk admits only files whose two spellings
/// agree by role. What the adapter adds is the input a walk cannot yield (a `.bsl`-spelled link onto
/// a non-source target), and routing the publisher through it is what makes that one implementation
/// rather than two that have to stay in step (github#278).
fn walked_file_key(
    roots: &bsl_search::WorkspaceRoots,
    file: &project_model::WalkedFile,
) -> Option<bsl_search::FileKey> {
    bsl_search::workspace_file_key_in(roots, &file.walked)
}

/// Walk the source tree ONCE and index what it found.
///
/// The walk happens here, not inside the engine, so that its verdict and its corpus cannot
/// come from different traversals: a tree can change between two walks, and then a clean
/// verdict would answer for files some later, shorter walk collected.
///
/// The root table is built by the SAME call the daemon reading these rows makes, off the
/// same project model — parity of keys has to follow from one construction, not from two
/// implementations that happen to agree today.
pub(super) fn build_workspace_code(
    project: &project_model::Project,
) -> Result<WorkspaceCorpus, Box<dyn Error + Send + Sync>> {
    use bsl_search::SearchEngine;

    let temp_dir = tempfile::tempdir()?;
    let db_path = temp_dir.path().join("baseline-sync.db");
    let mut engine = SearchEngine::fts_only(&db_path)?;
    let extensions: Vec<std::path::PathBuf> = project
        .extension_paths()
        .iter()
        .chain(project.external_paths())
        .map(|(_, path)| path.clone())
        .collect();
    let (roots, rejected) = bsl_search::WorkspaceRoots::build_optional(
        &project.root,
        project.semantic_base_path(),
        &extensions,
    );
    ensure_every_declared_root_is_accounted_for(&rejected)?;
    let roots = roots.with_user_excluded(project.source_exclusions());
    // The REGISTERED roots, which is also what the daemon's own walk covers. Not the declared
    // list: a rejected root adds no file to it, since `InsideConfiguration` means the
    // configuration root already contains it and `IdentifierTaken` has just refused the
    // publish outright. It would only be entered a second time, and every place inside it
    // classified twice — the walk's own counters have no notion of a file it has already
    // seen, and those counters are the numbers the refusal reports.
    let declared: Vec<std::path::PathBuf> =
        roots.entries().map(|(_, path)| path.to_path_buf()).collect();
    engine.initialize_workspace_roots(roots.clone())?;

    let walk = project_model::SourceSet::scan_in_scope(&declared, &[], project.source_exclusions());
    if let Some(config) = super::postgres::embedder_config(project)? {
        if config.token_policy.is_some() {
            let embedder = bsl_search::Embedder::new(config);
            let mut documents = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut indexed_files = 0;
            let mut unreadable_files = 0;
            for file in &walk.files {
                if file.role != project_model::FileRole::Source {
                    continue;
                }
                let Some(key) = walked_file_key(&roots, file) else {
                    continue;
                };
                if !seen.insert(key.clone()) {
                    continue;
                }
                let content = match std::fs::read_to_string(&file.walked) {
                    Ok(content) => content,
                    Err(_) => {
                        unreadable_files += 1;
                        continue;
                    }
                };
                let prepared = bsl_search::prepare_file_documents(
                    &key,
                    &content,
                    None,
                    embedder.token_policy().expect("checked above"),
                    embedder.document_prefix(),
                    |input| embedder.check_singleton_input(input),
                )?;
                indexed_files += usize::from(!prepared.is_empty());
                documents.extend(prepared);
            }
            return Ok(WorkspaceCorpus { indexed_files, documents, walk, unreadable_files });
        }
    }
    let ingest = engine.ingest_scanned_fts(&walk)?;
    let documents = engine.load_indexed_documents(Some("code"))?;
    Ok(WorkspaceCorpus {
        indexed_files: ingest.indexed,
        documents,
        walk,
        unreadable_files: ingest.unread,
    })
}

pub(super) fn build_reference(
) -> Result<(usize, Vec<bsl_search::IndexedDocument>), Box<dyn Error + Send + Sync>> {
    use bsl_search::SearchEngine;

    let documents = build_reference_source_documents();

    let temp_dir = tempfile::tempdir()?;
    let db_path = temp_dir.path().join("reference-baseline-sync.db");
    let mut engine = SearchEngine::fts_only(&db_path)?;
    let indexed_files = engine.index_documents(
        "platform",
        "platform://docs",
        env!("CARGO_PKG_VERSION").as_bytes(),
        &documents,
        None,
    )?;
    let indexed_documents = engine.load_indexed_documents(Some("platform"))?;
    Ok((indexed_files, indexed_documents))
}

pub(super) fn build_reference_source_documents() -> Vec<bsl_search::Document> {
    mcp_server::build_reference_documents()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    type Keys = BTreeSet<(String, String)>;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn write_source_config(root: &Path, configuration: &str, extensions: &[(&str, &str)]) {
        let entries = extensions
            .iter()
            .map(|(name, path)| {
                format!("  {{ name = {name:?}, path = {path:?}, scope = \"InfoBase\" }}")
            })
            .collect::<Vec<_>>()
            .join(",\n");
        write(
            &root.join("bsl-analyzer.toml"),
            &format!("[source]\nroot = {configuration:?}\nextensions = [\n{entries}\n]\n"),
        );
    }

    fn module(name: &str) -> String {
        format!("Процедура {name}()\n    Сообщить(\"{name}\");\nКонецПроцедуры\n")
    }

    fn project_at(root: &Path) -> project_model::Project {
        project_model::Project::new(root).unwrap()
    }

    /// The keys the published corpus carries, one entry per file however many chunks it holds.
    /// Durable keys carry the platform's separator; the assertions spell them with `/`.
    fn portable(keys: &Keys) -> Keys {
        keys.iter().map(|(root, path)| (root.replace('\\', "/"), path.replace('\\', "/"))).collect()
    }

    fn published_keys(root: &Path) -> Keys {
        build_workspace_code(&project_at(root))
            .unwrap()
            .documents
            .into_iter()
            .map(|d| (d.root_id, d.path))
            .collect()
    }

    /// The root table `build_workspace_code` publishes under: the same call, off the same model,
    /// with the user's `[source].exclude` applied — the table, the walk of the consumer below and
    /// the publisher's own walk must all be built the same way, or a parity fixture that uses
    /// exclusions would compare two different source universes.
    fn publisher_roots(project: &project_model::Project) -> bsl_search::WorkspaceRoots {
        let extensions: Vec<std::path::PathBuf> = project
            .extension_paths()
            .iter()
            .chain(project.external_paths())
            .map(|(_, path)| path.clone())
            .collect();
        bsl_search::WorkspaceRoots::build_optional(
            &project.root,
            project.semantic_base_path(),
            &extensions,
        )
        .0
        .with_user_excluded(project.source_exclusions())
    }

    /// A walk entry as the walk hands it over: the spelling the walk used, the physical
    /// spelling, the target's metadata and the role of both.
    #[cfg(unix)]
    fn walked_file(walked: &Path) -> project_model::WalkedFile {
        project_model::WalkedFile {
            root: walked.parent().unwrap().to_path_buf(),
            role: project_model::FileRole::Source,
            walked: walked.to_path_buf(),
            canonical: std::fs::canonicalize(walked).unwrap(),
            metadata: std::fs::metadata(walked).unwrap(),
        }
    }

    /// The keys the CONSUMER derives from the same tree: the daemon's own root table
    /// (`project::workspace_roots`), its own walk universe and its own attribution.
    ///
    /// Both halves are built the way the publisher builds them, and deliberately by the same calls:
    /// the root table through `publisher_roots` (which applies `with_user_excluded`), and the walk
    /// through `SourceSet::scan_in_scope` over the REGISTERED roots with the project's exclusions.
    /// `SourceSet::scan` or a table without the user's exclusions would compare two source universes
    /// and read a false divergence on any fixture that excludes something.
    ///
    /// The attribution here goes through the SAME engine adapter the publisher calls, so the ROLE
    /// branch has one implementation instead of two (github#278). The role branch is what the two
    /// used to disagree about, and the divergent inputs are the links a walk cannot yield at all:
    /// the tests that pin the branch call the procedure directly, on a `WalkedFile` the shared walk
    /// would have dropped.
    ///
    /// Both halves matter. A model built with no extensions, or walking the configuration
    /// root alone, agrees with any publisher on a tree without extensions and cannot
    /// disagree with one on a tree with them — which is exactly the case this exists for.
    fn consumed_keys(root: &Path) -> Keys {
        let project = project_at(root);
        let roots = publisher_roots(&project);
        let declared: Vec<std::path::PathBuf> =
            roots.entries().map(|(_, path)| path.to_path_buf()).collect();
        project_model::SourceSet::scan_in_scope(&declared, &[], project.source_exclusions())
            .files
            .iter()
            .filter(|file| file.role == project_model::FileRole::Source)
            .filter_map(|file| bsl_search::workspace_file_key_in(&roots, &file.walked))
            .map(|key| (key.root_id, key.path))
            .collect()
    }

    /// The files the SAME shared walk yields for a project, spelled as the walk reached them.
    ///
    /// A parity fixture needs this to state the half a walk can speak about: the walk DROPS every
    /// entry whose two spellings disagree by role, so it can never contain the divergent inputs the
    /// direct tests below are about.
    #[cfg(unix)]
    fn walked_paths(root: &Path) -> Vec<std::path::PathBuf> {
        let project = project_at(root);
        let roots = publisher_roots(&project);
        let declared: Vec<std::path::PathBuf> =
            roots.entries().map(|(_, path)| path.to_path_buf()).collect();
        project_model::SourceSet::scan_in_scope(&declared, &[], project.source_exclusions())
            .files
            .iter()
            .map(|file| file.walked.clone())
            .collect()
    }

    /// A project laid out the conventional way — configuration under `src/cf`, extensions
    /// under `src/cfe/*` — with the collision the composite key exists for: the first
    /// extension holds a module at the SAME relative path as the configuration's.
    fn workspace_with_two_extensions(dir: &Path) -> std::path::PathBuf {
        let root = dir.join("project");
        for (owner, relative) in [
            ("src/cf", "CommonModules/Общий/Ext/Module.bsl"),
            ("src/cfe/Первое", "CommonModules/Общий/Ext/Module.bsl"),
            ("src/cfe/Второе", "CommonModules/Второй/Ext/Module.bsl"),
        ] {
            let owner_root = root.join(owner);
            write(&owner_root.join("Configuration.xml"), "<MetaDataObject/>");
            write(&owner_root.join(relative), &module(&owner.replace('/', "_")));
        }
        write_source_config(
            &root,
            "src/cf",
            &[("Первое", "src/cfe/Первое"), ("Второе", "src/cfe/Второе")],
        );
        root
    }

    #[cfg(unix)]
    #[test]
    fn a_file_behind_a_directory_link_is_published() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("configuration");
        write(&root.join("CommonModules/Прямой/Ext/Module.bsl"), &module("Прямой"));
        let outside = dir.path().join("outside");
        write(&outside.join("Ссылочный.bsl"), &module("Ссылочный"));
        std::os::unix::fs::symlink(&outside, root.join("Связанные")).unwrap();

        let published = published_keys(&root);

        assert!(
            published.contains(&(String::new(), "Связанные/Ссылочный.bsl".to_owned())),
            "a module reachable only through a directory link belongs in the corpus, \
             because the consumer's walk sees it; published: {published:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bsl_named_link_to_a_foreign_target_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("configuration");
        write(&root.join("CommonModules/Прямой/Ext/Module.bsl"), &module("Прямой"));
        write(&root.join("Заметки.txt"), &module("НеИсходник"));
        std::os::unix::fs::symlink(root.join("Заметки.txt"), root.join("Призрак.bsl")).unwrap();

        let published = published_keys(&root);

        assert!(
            !published.contains(&(String::new(), "Призрак.bsl".to_owned())),
            "a name that claims to be BSL over a target that is not gets no row: the key \
             would name a file no walk of the target's root can rebuild; published: {published:?}"
        );
    }

    /// The ROLE branch is ONE procedure, and it is the engine's (github#278). The publisher used to
    /// call the canonical-ranked `root_of(walked, canonical)` directly, without the engine's role
    /// branch; on a `.bsl`-spelled link onto a target that is not a source the two answer DIFFERENT
    /// keys, and the canonical-ranked one names a key the walk forbids to exist.
    ///
    /// This is the first of the two divergent shapes the engine pins in `bsl-search`
    /// (`a_link_to_a_non_source_target_keys_by_its_walked_spelling`): the link sits in the
    /// configuration and its target in an extension. The positive control below asserts the two
    /// procedures really do disagree here — that is why the publisher may not carry a call of its
    /// own, and what reverting `walked_file_key` to `root_of` would break.
    ///
    /// It is a DIRECT test of the procedure: the input is a `WalkedFile` the shared walk would have
    /// dropped, because its two spellings disagree by role. The parity tests elsewhere in this
    /// module therefore cannot see this shape — and must not be read as if the old publisher had
    /// been publishing a forbidden key: it could only ever mint this one for a path no walk yields.
    #[cfg(unix)]
    #[test]
    fn a_bsl_link_to_a_non_source_target_keeps_the_walked_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace_with_two_extensions(dir.path());
        let notes = root.join("src/cfe/Первое/Заметки.txt");
        write(&notes, "не исходник");
        let alias = root.join("src/cf/Призрак.bsl");
        std::os::unix::fs::symlink(&notes, &alias).unwrap();
        let file = walked_file(&alias);
        let roots = publisher_roots(&project_at(&root));

        assert_eq!(
            walked_file_key(&roots, &file),
            Some(bsl_search::FileKey::configuration("Призрак.bsl")),
            "a name that claims to be BSL over a target that is not keeps the spelling it was \
             walked under: the key under the target's root is one the walk forbids to exist"
        );
        assert_eq!(
            roots.root_of(&file.walked, &file.canonical),
            Some(bsl_search::FileKey::new("src/cfe/Первое", "Заметки.txt")),
            "control: the canonical-ranked answer disagrees with the single procedure, and this \
             is the key the publisher used to store"
        );
        let walked = walked_paths(&root);
        assert!(
            walked.iter().all(|path| !path.ends_with("Призрак.bsl")),
            "control: the shared walk drops the divergent entry, so no walk ever rebuilds the \
             canonical-ranked key for it; walked: {walked:?}",
        );
    }

    /// The second divergent shape, the one the root itself makes: the extension is declared
    /// THROUGH A LINK, so the walked path also lies under the enclosing root's canonical spelling.
    /// Ranking the canonical spelling hands the file to the CONFIGURATION, while the declared
    /// spelling names the extension that walked to it — the key a removal has to reach
    /// (`a_non_source_target_under_a_linked_root_keys_by_the_declared_root`).
    ///
    /// The walk of this tree holds no such entry at all: both spellings of a walked file have to
    /// agree by role, and this one's do not. The direct call below is therefore the only way this
    /// input reaches the procedure.
    #[cfg(unix)]
    #[test]
    fn a_non_source_target_under_a_linked_root_keeps_the_declared_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        write(&root.join("src/cf/Configuration.xml"), "<MetaDataObject/>");
        write(&root.join("src/cf/Заметки.txt"), "не исходник");
        let outside = dir.path().join("outside/Первое");
        write(&outside.join("Configuration.xml"), "<MetaDataObject/>");
        let link = root.join("src/cf/ext");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // The alias of the extension is a `.bsl` NAME over a target under the configuration.
        std::os::unix::fs::symlink(root.join("src/cf/Заметки.txt"), outside.join("Alias.bsl"))
            .unwrap();
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [\"src/cf/ext\"]\n",
        )
        .unwrap();

        let file = walked_file(&link.join("Alias.bsl"));
        let roots = publisher_roots(&project_at(&root));
        let ext_id = roots
            .ids()
            .find(|id| !id.is_empty())
            .expect("the linked extension is registered")
            .to_owned();

        assert_eq!(
            walked_file_key(&roots, &file),
            Some(bsl_search::FileKey::new(ext_id, "Alias.bsl")),
            "the extension that walked to the file owns the key it could have been indexed under"
        );
        assert_eq!(
            roots.root_of(&file.walked, &file.canonical),
            Some(bsl_search::FileKey::configuration("Заметки.txt")),
            "control: the canonical-ranked answer hands the file to the configuration instead"
        );
        let walked = walked_paths(&root);
        assert!(
            walked.iter().all(|path| !path.ends_with("Alias.bsl")),
            "control: the shared walk agrees the divergent entry belongs to no walk of either \
             root; walked: {walked:?}",
        );
    }

    /// Parity on the tree shape the two procedures were able to disagree about: a root declared
    /// through a link, holding a `.bsl`-spelled link onto a non-source target. The shared walk
    /// DROPS such a file — its two spellings do not share a role — so this also pins that the
    /// publisher and the consumer agree about its absence, and about the real module of the
    /// linked root, which is keyed from the target it resolves to.
    #[cfg(unix)]
    #[test]
    fn the_published_keys_match_the_consumer_on_a_tree_with_a_linked_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let configuration = root.join("src/cf");
        write(&configuration.join("Configuration.xml"), "<MetaDataObject/>");
        write(&configuration.join("Настоящий/Module.bsl"), &module("Настоящий"));
        write(&configuration.join("Заметки.txt"), "не исходник");
        let outside = dir.path().join("outside/Первое");
        write(&outside.join("Configuration.xml"), "<MetaDataObject/>");
        write(&outside.join("CommonModules/Расш/Ext/Module.bsl"), &module("Расш"));
        let link = configuration.join("ext");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        std::os::unix::fs::symlink(configuration.join("Заметки.txt"), outside.join("Alias.bsl"))
            .unwrap();
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [\"src/cf/ext\"]\n",
        )
        .unwrap();

        let published = published_keys(&root);

        assert_eq!(
            published,
            consumed_keys(&root),
            "the writer of the baseline and its reader must agree on what a file is called, \
             including on the files the shared walk drops; published: {published:?}"
        );
        assert!(
            published.contains(&(
                std::fs::canonicalize(&outside).unwrap().to_string_lossy().into_owned(),
                "CommonModules/Расш/Ext/Module.bsl".to_owned(),
            )),
            "the linked root's module is keyed by the target the link resolves to; \
             published: {published:?}"
        );
        assert!(
            published
                .iter()
                .all(|(_, path)| !path.ends_with("Заметки.txt") && !path.ends_with("Alias.bsl")),
            "a key under the configuration for the extension's non-source target, or one under \
             the link for it, is a row the walk can never rebuild; published: {published:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_published_keys_are_the_keys_the_consumer_derives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("configuration");
        write(&root.join("Настоящий/Module.bsl"), &module("Настоящий"));
        // An alias INSIDE the root: both spellings canonicalise under it, so the consumer
        // collapses them into one key.
        std::os::unix::fs::symlink(root.join("Настоящий"), root.join("Псевдоним")).unwrap();
        // An alias OUT of the root: the canonical path leaves it, so attribution falls back
        // to the walked spelling and the file keeps a key of its own.
        let outside = dir.path().join("outside");
        write(&outside.join("Внешний.bsl"), &module("Внешний"));
        std::os::unix::fs::symlink(&outside, root.join("Наружу")).unwrap();

        assert_eq!(
            published_keys(&root),
            consumed_keys(&root),
            "the writer of the baseline and its reader must agree on what a file is called; \
             a key only one of them can produce is a row the other can never match, update or drop"
        );
    }

    /// The same parity on a tree that HAS extensions — the case the composite key exists
    /// for, and the one the test above cannot reach: with no extension declared, a
    /// publisher that ignores them agrees with the consumer perfectly.
    ///
    /// The tree carries one module at a relative path shared with the configuration, so a
    /// publisher that dropped the root would not merely mislabel a row: it would produce
    /// ONE key where the consumer looks for two, and the extension's module would read
    /// downstream as an edit of the configuration's.
    #[test]
    fn the_published_keys_match_the_consumer_on_a_tree_with_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace_with_two_extensions(dir.path());

        let published = published_keys(&root);

        assert_eq!(
            published,
            consumed_keys(&root),
            "a baseline whose keys the daemon cannot derive is a baseline it can never match, \
             update or drop"
        );
        assert!(
            portable(&published).contains(&(
                "src/cfe/Первое".to_owned(),
                "CommonModules/Общий/Ext/Module.bsl".to_owned()
            )),
            "the extension's module must reach the corpus under ITS root, not merged into the \
             configuration's key; published: {published:?}"
        );
    }

    #[test]
    fn workspace_code_baseline_uses_the_projects_source_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        write(&root.join("Configuration.xml"), "<MetaDataObject/>");
        write(&root.join("Allowed.bsl"), &module("Разрешенная"));
        write(&root.join(".tmp/Hidden.bsl"), &module("Скрытая"));
        write(&root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\nexclude = [\".tmp\"]\n");

        let corpus = build_workspace_code(&project_at(&root)).unwrap();
        assert!(
            corpus.documents.iter().any(|document| document.path.ends_with("Allowed.bsl")),
            "the allowed positive control did not reach the local baseline"
        );
        assert!(
            corpus.documents.iter().all(|document| !document.path.ends_with("Hidden.bsl")),
            "the local baseline published a document from [source].exclude"
        );

        write(&root.join("bsl-analyzer.toml"), "[source]\nroot = \".\"\nexclude = [\".\"]\n");
        let empty = build_workspace_code(&project_at(&root)).unwrap();
        assert!(empty.documents.is_empty(), "an excluded root was restored as a baseline fallback");
        assert_eq!(empty.indexed_files, 0);
    }

    /// The numbers in the refusal text tell the operator how many places to fix, so a place
    /// counted twice sends them looking for a second one that does not exist.
    ///
    /// Overlapping roots are not exotic here: an extension inside the configuration is a
    /// declared root AND a subtree of another declared root, so a walk over the DECLARED list
    /// enters it twice and classifies everything in it twice.
    #[cfg(unix)]
    #[test]
    fn one_unreadable_place_under_overlapping_roots_is_counted_once() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        write(&root.join("src/cf/Configuration.xml"), "<MetaDataObject/>");
        write(&root.join("src/cf/CommonModules/Общий/Ext/Module.bsl"), &module("Общий"));
        let nested = root.join("src/cf/Вложенное");
        write(&nested.join("Configuration.xml"), "<MetaDataObject/>");
        let closed = nested.join("CommonModules/Закрытый");
        write(&closed.join("Ext/Module.bsl"), &module("Закрытый"));
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&closed).is_ok() {
            return;
        }
        write_source_config(&root, "src/cf", &[("Вложенное", "src/cf/Вложенное")]);

        let corpus = build_workspace_code(&project_at(&root)).unwrap();

        assert_eq!(
            corpus.walk.unreadable, 1,
            "one directory is unreadable, so the operator must be told about one"
        );
    }

    /// A root rejected for lying inside the configuration costs nothing: the configuration's
    /// own walk reaches its files and attribution hands them the configuration's key, which
    /// is the key they already have in every published baseline. So the publish goes on, and
    /// the files are there.
    ///
    /// Nothing has to be deduplicated for that: the walk covers the REGISTERED roots, and a
    /// root rejected for lying inside the configuration is not among them, so its subtree is
    /// entered once — by the configuration that contains it.
    #[test]
    fn an_extension_inside_the_configuration_is_published_under_the_configuration_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        write(&root.join("src/cf/Configuration.xml"), "<MetaDataObject/>");
        write(&root.join("src/cf/CommonModules/Общий/Ext/Module.bsl"), &module("Общий"));
        let nested = root.join("src/cf/Вложенное");
        write(&nested.join("Configuration.xml"), "<MetaDataObject/>");
        write(&nested.join("CommonModules/Вложенный/Ext/Module.bsl"), &module("Вложенный"));
        write_source_config(&root, "src/cf", &[("Вложенное", "src/cf/Вложенное")]);

        let published = published_keys(&root);

        assert_eq!(
            portable(&published)
                .iter()
                .filter(|(_, path)| path.ends_with("Вложенный/Ext/Module.bsl"))
                .collect::<Vec<_>>(),
            vec![&(String::new(), "Вложенное/CommonModules/Вложенный/Ext/Module.bsl".to_owned())],
            "the nested extension's module belongs in the corpus exactly once, under the \
             configuration's key; published: {published:?}"
        );
        assert_eq!(published, consumed_keys(&root), "and the consumer must derive the same set");
    }

    /// The other rejection is not survivable. Two roots sharing one identifier share one key
    /// space, so whichever registers second is not merely mislabelled — its files reach the
    /// corpus under keys the first root's files already hold, and the snapshot ends up
    /// claiming one file where two live. Refusing names the collision; publishing hides it.
    #[cfg(unix)]
    #[test]
    // The fixture needs two directory names that differ only in bytes no `str` can hold, and
    // APFS refuses to create such a name at all: the collision cannot be staged on macOS.
    #[cfg(not(target_os = "macos"))]
    fn two_roots_that_would_share_one_identifier_stop_the_publish() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        write(&root.join("src/cf/Configuration.xml"), "<MetaDataObject/>");
        write(&root.join("src/cf/CommonModules/Общий/Ext/Module.bsl"), &module("Общий"));
        // Structured declarations give both roots a Scope through UTF-8 symlinks. Their
        // canonical target names differ only in bytes that the root ID cannot retain.
        for (name, tail) in [("Первое", vec![b'a', 0x80]), ("Второе", vec![b'a', 0x81])]
        {
            let extension = root.join("src/cfe").join(OsString::from_vec(tail));
            write(&extension.join("Configuration.xml"), "<MetaDataObject/>");
            write(&extension.join("CommonModules/Расш/Ext/Module.bsl"), &module("Расш"));
            std::os::unix::fs::symlink(&extension, root.join("src/cfe").join(name)).unwrap();
        }
        write_source_config(
            &root,
            "src/cf",
            &[("Первое", "src/cfe/Первое"), ("Второе", "src/cfe/Второе")],
        );

        let error = match build_workspace_code(&project_at(&root)) {
            Ok(corpus) => panic!(
                "a corpus with two roots in one key space may not be published; it produced \
                 {} keys instead",
                corpus.documents.len()
            ),
            Err(error) => error.to_string(),
        };

        assert!(
            error.contains("identifier"),
            "the refusal must name what collided, or the operator cannot act on it: {error}"
        );
    }

    /// The corpus and the completeness verdict must describe ONE traversal. Two walks of a tree
    /// that changed in between let a clean verdict answer for a short corpus — the very defect
    /// the refusal exists to prevent, rebuilt inside its own fix.
    ///
    /// The count proves no SECOND `SourceSet::scan`; that no walk happens OUTSIDE `SourceSet` is
    /// what the structural gate below proves, since a hand-rolled traversal would not move this
    /// counter at all.
    ///
    /// Measured on a tree with SEVERAL roots, the shape a per-root loop would be written for:
    /// on a single-root tree one walk per root and one walk in total are the same number.
    #[test]
    fn building_the_corpus_walks_the_tree_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace_with_two_extensions(dir.path());

        let before = project_model::source_set::scans_performed_on_thread();
        build_workspace_code(&project_at(&root)).unwrap();
        let walked = project_model::source_set::scans_performed_on_thread() - before;

        assert_eq!(walked, 1, "the corpus and its verdict must come from one and the same walk");
    }

    /// The publisher must not grow a traversal of its own. It holds the only walk the corpus is
    /// built from, so a second one here would be exactly the divergence the counter above cannot
    /// see: a private `read_dir` or `WalkDir` never reaches `SourceSet::scan`.
    #[test]
    fn the_publisher_does_not_carry_its_own_tree_walk() {
        let source = include_str!("documents.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(source);
        assert!(
            production.contains("fn build_workspace_code"),
            "the production/test cut moved; this gate scans only what it can prove it scanned"
        );
        for needle in [["walk", "dir::Walk", "Dir"].concat(), ["read", "_dir("].concat()] {
            assert!(
                !production.contains(&needle),
                "the corpus must come from project_model::SourceSet::scan and nothing else, \
                 or its completeness verdict answers for a walk it did not describe ({needle})"
            );
        }
    }

    /// The single-root shortcut would silently undo the whole node. The publisher must pass
    /// its complete configuration+extensions table to the plural boot initializer.
    #[test]
    fn the_publisher_does_not_fall_back_to_a_single_root_table() {
        let source = include_str!("documents.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(source);
        assert!(
            production.contains("initialize_workspace_roots("),
            "the production/test cut moved, or the root table is no longer installed here; \
             this gate scans only what it can prove it scanned"
        );
        assert!(
            !production.contains("set_workspace_root("),
            "the publisher must key its rows through the project's root table, or the baseline \
             it writes has keys the daemon reading it cannot derive"
        );
    }

    #[test]
    fn a_root_that_is_itself_a_file_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let standalone = dir.path().join("Одиночный.bsl");
        write(&standalone, &module("Одиночный"));

        assert!(
            published_keys(&standalone).is_empty(),
            "attribution rejects a path equal to its own root, so a file root has no key the \
             consumer could ever resolve; publishing one row under the empty path puts a record \
             into the baseline that its only reader cannot reach"
        );
    }
}
