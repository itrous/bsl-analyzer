//! Metadata-substrate boot and incremental refresh, shared by every consumer.
//!
//! The metadata substrate is the per-MDO Salsa representation (`set_metadata_listing`
//! plus per-file revisions feeding `parse_mdo_query` / `resolve_metadata_object`). Building
//! and refreshing it is a *policy* about the analysis substrate, not about any one
//! frontend — so it lives here on [`AnalysisHost`], and the LSP server, MCP server, and
//! CLI drive it through the same code. Each frontend owns its own VFS and its own change
//! *trigger* (a push watcher, a pull drift-scan, or a one-shot batch load); only the
//! substrate-mutation policy is shared.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use base_db::{content_revision, read_disk_text, SourceDatabase, SourceRoot, METADATA_SOURCE_ROOT};
use ide_db::metadata::{
    CommonAttributeEntry, CommonModuleEntry, DefinedTypeEntry, EventSubscriptionEntry,
    HTTPServiceEntry, IntegrationServiceEntry, MdoEntry, MetadataListingData, RoleEntry,
    ScheduledJobEntry, SubsystemEntry, WebServiceEntry,
};
use vfs::{file_set::FileSet, FileId, Vfs, VfsPath};

use ide::RootDatabaseImpl;

use crate::AnalysisHost;

/// A lock-flavour-neutral handle to the consumer's VFS. The metadata policy does its
/// expensive discovery and disk reads *outside* this, then acquires a mutable [`Vfs`]
/// only for the short interning critical section. Keeping the lock type behind this
/// trait stops `ide-host-core` from imposing a lock flavour (the LSP uses
/// `parking_lot::RwLock`, the MCP server `tokio::sync::RwLock`) on its consumers.
pub trait VfsWrite {
    fn with_write<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R;
}

struct RootStructureListing {
    root: String,
    data: MetadataListingData,
}

/// Build the metadata substrate for every config root the database knows about.
/// `alloc_file_id` is idempotent, so it reuses the FileId already interned for a
/// watched path. Runs after the source root is initialised and re-runs on reload.
///
/// Free function over a raw database so a consumer that owns a bare
/// [`RootDatabaseImpl`] (the MCP diagnostics resident) can share the exact policy
/// [`AnalysisHost`] runs, without wrapping its db in an `AnalysisHost`.
pub fn bootstrap_metadata_substrate(db: &mut RootDatabaseImpl, vfs: &impl VfsWrite) {
    let start = Instant::now();

    let snapshot = db.workspace_configs_snapshot();
    // Discovery reads through the user's exclusions, so nothing inside them is listed,
    // probed or registered.
    let tree = bsl_metadata::ScopedFs::new(&snapshot.source_exclusions);
    let config_paths = db.all_config_paths();
    if config_paths.is_empty() {
        return;
    }

    // One config root's discovered (stat-only) structure, before any file is
    // read or interned.
    struct RootDiscovery {
        root_string: String,
        mdos: Vec<bsl_metadata::DiscoveredMdo>,
        defined_types: Vec<bsl_metadata::DiscoveredDefinedType>,
        common_modules: Vec<bsl_metadata::DiscoveredCommonModule>,
        event_subscriptions: Vec<bsl_metadata::DiscoveredEventSubscription>,
        scheduled_jobs: Vec<bsl_metadata::DiscoveredScheduledJob>,
        roles: Vec<bsl_metadata::DiscoveredRole>,
        http_services: Vec<bsl_metadata::DiscoveredHTTPService>,
        web_services: Vec<bsl_metadata::DiscoveredWebService>,
        integration_services: Vec<bsl_metadata::DiscoveredIntegrationService>,
        subsystems: Vec<bsl_metadata::DiscoveredSubsystem>,
        common_attributes: Vec<bsl_metadata::DiscoveredCommonAttribute>,
    }

    // Discover every root's structure WITHOUT the vfs lock — discovery walks
    // and stats the filesystem but never touches the vfs.
    let discoveries: Vec<RootDiscovery> = config_paths
        .iter()
        .zip(&snapshot.kinds)
        .map(|((_, root_path), kind)| {
            let mut mdos = bsl_metadata::discover_metadata_structure(root_path, &tree);
            mdos.extend(bsl_metadata::discover_register_structure(root_path, &tree));
            // An export root lists nothing by collection: its one object is
            // discovered by the kind the project established for the root.
            if let ide_db::metadata::RootKind::External(external) = kind {
                mdos.extend(bsl_metadata::discover_external_object_structure(
                    root_path, &tree, *external,
                ));
            }
            RootDiscovery {
                root_string: root_path.to_string_lossy().to_string(),
                mdos,
                defined_types: bsl_metadata::discover_defined_type_structure(root_path, &tree),
                common_modules: bsl_metadata::discover_common_module_structure(root_path, &tree),
                event_subscriptions: bsl_metadata::discover_event_subscription_structure(
                    root_path, &tree,
                ),
                scheduled_jobs: bsl_metadata::discover_scheduled_job_structure(root_path, &tree),
                roles: bsl_metadata::discover_role_structure(root_path, &tree),
                http_services: bsl_metadata::discover_http_service_structure(root_path, &tree),
                web_services: bsl_metadata::discover_web_service_structure(root_path, &tree),
                integration_services: bsl_metadata::discover_integration_service_structure(
                    root_path, &tree,
                ),
                subsystems: bsl_metadata::discover_subsystem_structure(root_path, &tree),
                common_attributes: bsl_metadata::discover_common_attribute_structure(
                    root_path, &tree,
                ),
            }
        })
        .collect();

    // Gather every composing file that needs its content revision, then read
    // and hash them in parallel OFF the vfs lock. The text itself is not
    // retained — only `(path, revision)`. (`module_file` is BSL source owned by
    // root(0), read elsewhere — not hashed here.)
    let mut to_read: Vec<PathBuf> = Vec::new();
    for d in &discoveries {
        for m in &d.mdos {
            to_read.push(m.main.clone());
            if let Some(p) = &m.predefined {
                to_read.push(p.clone());
            }
        }
        to_read.extend(d.defined_types.iter().map(|t| t.main.clone()));
        to_read.extend(d.common_modules.iter().map(|c| c.main.clone()));
        to_read.extend(d.event_subscriptions.iter().map(|s| s.main.clone()));
        to_read.extend(d.scheduled_jobs.iter().map(|j| j.main.clone()));
        to_read.extend(d.http_services.iter().map(|s| s.main.clone()));
        to_read.extend(d.web_services.iter().map(|s| s.main.clone()));
        to_read.extend(d.integration_services.iter().map(|s| s.main.clone()));
        to_read.extend(d.subsystems.iter().map(|s| s.main.clone()));
        to_read.extend(d.common_attributes.iter().map(|a| a.main.clone()));
        for role in &d.roles {
            to_read.push(role.main.clone());
            if let Some(rights) = &role.rights {
                to_read.push(rights.clone());
            }
        }
    }
    let revisions_by_path: HashMap<PathBuf, u64> = {
        use rayon::prelude::*;
        to_read
            .par_iter()
            .filter_map(|path| {
                let text = read_disk_text(path).ok()?;
                Some((path.clone(), content_revision(&text)))
            })
            .collect()
    };

    let mut metadata_file_set = FileSet::new();
    let mut revisions: Vec<(FileId, u64)> = Vec::new();
    let mut listings: Vec<RootStructureListing> = Vec::new();

    // Intern under a single short vfs write: allocate FileIds and grow the
    // metadata file set. This is the only lock-held work; the expensive reads
    // already happened above. A file whose read failed (vanished between
    // discovery and the read pass) is absent from `revisions_by_path`, so
    // `intern_metadata_file` returns `None` and the MDO is dropped.
    vfs.with_write(|vfs| {
        for d in discoveries {
            let mut entries = Vec::with_capacity(d.mdos.len());
            for m in d.mdos {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &m.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                let predefined = m.predefined.as_ref().and_then(|p| {
                    intern_metadata_file(
                        vfs,
                        p,
                        &revisions_by_path,
                        &mut metadata_file_set,
                        &mut revisions,
                    )
                });
                entries.push(MdoEntry { kind: m.mdo_type, name: m.name, main, predefined });
            }
            let mut defined_types = Vec::new();
            for t in d.defined_types {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &t.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                defined_types.push(DefinedTypeEntry { name: t.name, main });
            }
            let mut common_modules = Vec::new();
            for c in d.common_modules {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &c.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                // The module's `Ext/Module.bsl` is BSL source owned by root(0),
                // not a metadata file — look up the analyzer's existing FileId for
                // it (bootstrap runs after the source root is interned) rather than
                // enrolling a duplicate. `None` when the path is absent or unloaded;
                // the reverse lookup then misses, which is correct.
                // Both outcomes are recorded: the readable id backs the ordinary
                // back-link, the unread one lets name resolution refuse to read the
                // shorter list as proof that a method is missing.
                let body = c.module_file.as_ref().map(|p| module_file_id(db, vfs, p));
                let module_file = match body {
                    Some(BodyRef::Readable(id)) => Some(id),
                    _ => None,
                };
                let unread_module_file = match body {
                    Some(BodyRef::Unread(id)) => Some(id),
                    _ => None,
                };
                common_modules.push(CommonModuleEntry {
                    name: c.name,
                    main,
                    module_file,
                    unread_module_file,
                });
            }
            let mut event_subscriptions = Vec::new();
            for s in d.event_subscriptions {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &s.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                event_subscriptions.push(EventSubscriptionEntry { name: s.name, main });
            }
            let mut scheduled_jobs = Vec::new();
            for j in d.scheduled_jobs {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &j.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                scheduled_jobs.push(ScheduledJobEntry { name: j.name, main });
            }
            let mut roles = Vec::new();
            for role in d.roles {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &role.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                let rights = role.rights.as_ref().and_then(|p| {
                    intern_metadata_file(
                        vfs,
                        p,
                        &revisions_by_path,
                        &mut metadata_file_set,
                        &mut revisions,
                    )
                });
                roles.push(RoleEntry { name: role.name, main, rights });
            }
            let mut http_services = Vec::new();
            for service in d.http_services {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &service.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file = service
                    .module_file
                    .as_ref()
                    .and_then(|p| module_file_id(db, vfs, p).readable());
                http_services.push(HTTPServiceEntry { name: service.name, main, module_file });
            }
            let mut web_services = Vec::new();
            for service in d.web_services {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &service.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file = service
                    .module_file
                    .as_ref()
                    .and_then(|p| module_file_id(db, vfs, p).readable());
                web_services.push(WebServiceEntry { name: service.name, main, module_file });
            }
            let mut integration_services = Vec::new();
            for service in d.integration_services {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &service.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file = service
                    .module_file
                    .as_ref()
                    .and_then(|p| module_file_id(db, vfs, p).readable());
                integration_services.push(IntegrationServiceEntry {
                    name: service.name,
                    main,
                    module_file,
                });
            }
            let mut subsystems = Vec::new();
            for subsystem in d.subsystems {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &subsystem.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                subsystems.push(SubsystemEntry { name: subsystem.name, main });
            }
            let mut common_attributes = Vec::new();
            for attribute in d.common_attributes {
                let Some(main) = intern_metadata_file(
                    vfs,
                    &attribute.main,
                    &revisions_by_path,
                    &mut metadata_file_set,
                    &mut revisions,
                ) else {
                    continue;
                };
                common_attributes.push(CommonAttributeEntry { name: attribute.name, main });
            }
            listings.push(RootStructureListing {
                root: d.root_string,
                data: MetadataListingData {
                    common_attributes,
                    entries,
                    defined_types,
                    common_modules,
                    event_subscriptions,
                    scheduled_jobs,
                    roles,
                    http_services,
                    web_services,
                    integration_services,
                    subsystems,
                },
            });
        }
    });

    let mdo_count: usize = listings.iter().map(|listing| listing.data.entries.len()).sum();
    let file_count = revisions.len();

    db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata_file_set));
    for (fid, _) in &revisions {
        db.set_file_source_root(*fid, METADATA_SOURCE_ROOT);
    }
    for (fid, revision) in &revisions {
        db.set_file_revision_from_disk(*fid, *revision);
    }
    for listing in listings {
        db.set_metadata_listing(&listing.root, listing.data);
    }

    tracing::info!(
        mdo_count,
        file_count,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "bootstrapped metadata substrate",
    );
}

/// Incrementally refresh the metadata substrate for the config roots that own
/// any of `changed_paths` (a post-boot metadata batch: content edits, adds,
/// removes, renames). Re-discovers each affected root's structure (stat-only, no
/// content read), then:
/// - reads on disk **only** the changed or brand-new composing files, bumping
///   their revision so `parse_mdo_query` re-parses just those MDOs; unchanged
///   MDOs keep their revision and stay memoised;
/// - augments root(1) with any new composing files (removed files linger
///   harmlessly — nothing in a listing references them);
/// - re-sets a root's structure listing **only** when its entries actually
///   changed (add / remove / rename), so a pure content edit does not churn
///   `config_index`.
///
/// Vanished mains drop out of the re-discovered structure (and so out of the
/// listing), tombstoning them: `resolve_metadata_object` then returns `None`.
/// Runs after the boot bootstrap (root(1) already exists). Returns whether any
/// substrate input actually changed, so callers can gate a diagnostics refresh.
pub fn refresh_metadata_substrate(
    db: &mut RootDatabaseImpl,
    vfs: &impl VfsWrite,
    changed_paths: &[PathBuf],
) -> bool {
    if changed_paths.is_empty() {
        return false;
    }

    let snapshot = db.workspace_configs_snapshot();
    // Discovery reads through the user's exclusions, so nothing inside them is listed,
    // probed or registered.
    let tree = bsl_metadata::ScopedFs::new(&snapshot.source_exclusions);
    let config_paths = db.all_config_paths();
    let mut affected: Vec<(PathBuf, ide_db::metadata::RootKind)> = Vec::new();
    for ((_, root), kind) in config_paths.iter().zip(&snapshot.kinds) {
        if !affected.iter().any(|(r, _)| r == root)
            && changed_paths.iter().any(|p| p.starts_with(root))
        {
            affected.push((root.clone(), *kind));
        }
    }
    if affected.is_empty() {
        return false;
    }

    let changed_set: HashSet<&Path> = changed_paths.iter().map(|p| p.as_path()).collect();

    let mut metadata_file_set = {
        let db = &*db;
        db.source_root_input(METADATA_SOURCE_ROOT).root(db).file_set().clone()
    };
    let files_before = metadata_file_set.len();

    let mut new_file_ids: Vec<FileId> = Vec::new();
    let mut revisions: Vec<(FileId, u64)> = Vec::new();
    let mut listings: Vec<RootStructureListing> = Vec::new();

    vfs.with_write(|vfs| {
        for (root, kind) in &affected {
            let mut discovered = bsl_metadata::discover_metadata_structure(root, &tree);
            discovered.extend(bsl_metadata::discover_register_structure(root, &tree));
            if let ide_db::metadata::RootKind::External(external) = kind {
                discovered.extend(bsl_metadata::discover_external_object_structure(
                    root, &tree, *external,
                ));
            }
            let mut entries = Vec::with_capacity(discovered.len());
            for d in discovered {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                let predefined = d.predefined.as_ref().and_then(|p| {
                    enroll_refresh(
                        vfs,
                        p,
                        &changed_set,
                        &mut metadata_file_set,
                        &mut new_file_ids,
                        &mut revisions,
                    )
                });
                entries.push(MdoEntry { kind: d.mdo_type, name: d.name, main, predefined });
            }
            let mut defined_types = Vec::new();
            for d in bsl_metadata::discover_defined_type_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                defined_types.push(DefinedTypeEntry { name: d.name, main });
            }
            let mut common_modules = Vec::new();
            for d in bsl_metadata::discover_common_module_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                // The module's `Ext/Module.bsl` is BSL source owned by root(0),
                // not a metadata file — reuse the analyzer's existing FileId for
                // it rather than enrolling a duplicate (see `bootstrap_metadata_substrate`).
                // Both outcomes are recorded: the readable id backs the ordinary
                // back-link, the unread one lets name resolution refuse to read the
                // shorter list as proof that a method is missing.
                let body = d.module_file.as_ref().map(|p| module_file_id(db, vfs, p));
                let module_file = match body {
                    Some(BodyRef::Readable(id)) => Some(id),
                    _ => None,
                };
                let unread_module_file = match body {
                    Some(BodyRef::Unread(id)) => Some(id),
                    _ => None,
                };
                common_modules.push(CommonModuleEntry {
                    name: d.name,
                    main,
                    module_file,
                    unread_module_file,
                });
            }
            let mut event_subscriptions = Vec::new();
            for d in bsl_metadata::discover_event_subscription_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                event_subscriptions.push(EventSubscriptionEntry { name: d.name, main });
            }
            let mut scheduled_jobs = Vec::new();
            for d in bsl_metadata::discover_scheduled_job_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                scheduled_jobs.push(ScheduledJobEntry { name: d.name, main });
            }
            let mut roles = Vec::new();
            for d in bsl_metadata::discover_role_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                let rights = d.rights.as_ref().and_then(|p| {
                    enroll_refresh(
                        vfs,
                        p,
                        &changed_set,
                        &mut metadata_file_set,
                        &mut new_file_ids,
                        &mut revisions,
                    )
                });
                roles.push(RoleEntry { name: d.name, main, rights });
            }
            let mut http_services = Vec::new();
            for d in bsl_metadata::discover_http_service_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file =
                    d.module_file.as_ref().and_then(|p| module_file_id(db, vfs, p).readable());
                http_services.push(HTTPServiceEntry { name: d.name, main, module_file });
            }
            let mut web_services = Vec::new();
            for d in bsl_metadata::discover_web_service_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file =
                    d.module_file.as_ref().and_then(|p| module_file_id(db, vfs, p).readable());
                web_services.push(WebServiceEntry { name: d.name, main, module_file });
            }
            let mut integration_services = Vec::new();
            for d in bsl_metadata::discover_integration_service_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                let module_file =
                    d.module_file.as_ref().and_then(|p| module_file_id(db, vfs, p).readable());
                integration_services.push(IntegrationServiceEntry {
                    name: d.name,
                    main,
                    module_file,
                });
            }
            let mut subsystems = Vec::new();
            for d in bsl_metadata::discover_subsystem_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                subsystems.push(SubsystemEntry { name: d.name, main });
            }
            let mut common_attributes = Vec::new();
            for d in bsl_metadata::discover_common_attribute_structure(root, &tree) {
                let Some(main) = enroll_refresh(
                    vfs,
                    &d.main,
                    &changed_set,
                    &mut metadata_file_set,
                    &mut new_file_ids,
                    &mut revisions,
                ) else {
                    continue;
                };
                common_attributes.push(CommonAttributeEntry { name: d.name, main });
            }
            listings.push(RootStructureListing {
                root: root.to_string_lossy().to_string(),
                data: MetadataListingData {
                    common_attributes,
                    entries,
                    defined_types,
                    common_modules,
                    event_subscriptions,
                    scheduled_jobs,
                    roles,
                    http_services,
                    web_services,
                    integration_services,
                    subsystems,
                },
            });
        }
    });

    let reread = revisions.len();
    let added = new_file_ids.len();

    let mut changed = false;
    if metadata_file_set.len() != files_before {
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata_file_set));
        changed = true;
    }
    for fid in &new_file_ids {
        db.set_file_source_root(*fid, METADATA_SOURCE_ROOT);
    }
    for (fid, revision) in &revisions {
        db.set_file_revision_from_disk(*fid, *revision);
        changed = true;
    }
    for listing in listings {
        let data = &listing.data;
        let structure_changed = match db.metadata_listing(&listing.root) {
            Some(input) => {
                input.entries(db).as_ref() != &data.entries
                    || input.defined_types(db).as_ref() != &data.defined_types
                    || input.common_modules(db).as_ref() != &data.common_modules
                    || input.event_subscriptions(db).as_ref() != &data.event_subscriptions
                    || input.scheduled_jobs(db).as_ref() != &data.scheduled_jobs
                    || input.roles(db).as_ref() != &data.roles
                    || input.http_services(db).as_ref() != &data.http_services
                    || input.web_services(db).as_ref() != &data.web_services
                    || input.integration_services(db).as_ref() != &data.integration_services
                    || input.subsystems(db).as_ref() != &data.subsystems
                    || input.common_attributes(db).as_ref() != &data.common_attributes
            }
            None => true,
        };
        if structure_changed {
            db.set_metadata_listing(&listing.root, listing.data);
            changed = true;
        }
    }

    tracing::debug!(
        roots = affected.len(),
        reread,
        added,
        changed,
        "refreshed metadata substrate incrementally",
    );
    changed
}

impl AnalysisHost {
    /// Build the metadata substrate for every config root the database knows about.
    /// Thin wrapper over the free [`bootstrap_metadata_substrate`]; the LSP and CLI
    /// drive the shared policy through this, supplying only their VFS.
    pub fn bootstrap_metadata_substrate(&mut self, vfs: &impl VfsWrite) {
        bootstrap_metadata_substrate(self.raw_database_mut(), vfs);
    }

    /// Incrementally refresh the metadata substrate for the config roots owning any
    /// of `changed_paths`. Thin wrapper over the free [`refresh_metadata_substrate`];
    /// returns whether any substrate input actually changed.
    pub fn refresh_metadata_substrate(
        &mut self,
        vfs: &impl VfsWrite,
        changed_paths: &[PathBuf],
    ) -> bool {
        refresh_metadata_substrate(self.raw_database_mut(), vfs, changed_paths)
    }
}

/// What the substrate found behind a module's declared body path.
///
/// Three outcomes, not two: a body can be absent, present and readable, or present
/// and unreadable. The last one used to be indistinguishable from the second and had
/// to be supplied as a side-channel set of paths; it is now a property of the file in
/// the database, asked by id.
enum BodyRef {
    Readable(FileId),
    Unread(FileId),
    Absent,
}

impl BodyRef {
    /// The id a consumer of the back-link may use — that is, one whose text stands
    /// for the module's API. An unread body deliberately answers `None`: handing its
    /// id to a module-level consumer would let it read an empty text and conclude
    /// the module has no API.
    fn readable(self) -> Option<FileId> {
        match self {
            BodyRef::Readable(id) => Some(id),
            BodyRef::Unread(_) | BodyRef::Absent => None,
        }
    }
}

/// Resolve a module's `Ext/Module.bsl` path to the FileId already interned for it,
/// so the metadata reverse index points at the consumer's own source id.
///
/// A consumer may seed its VFS with *canonicalised* file paths (the MCP resident
/// interns the `canonicalize`d paths its `.bsl` enumeration produced). The caller here
/// composes `root.join(relative)`, which does NOT resolve a symlink *inside* the tree
/// (e.g. a symlinked `CommonModules` directory). So a direct lookup can miss even
/// though the file is interned under its real path — retry once with the path
/// canonicalised. The syscall stays off the common case: it runs only on a miss, so
/// an already-canonical VFS (the LSP) never pays for it and its behaviour is unchanged.
///
/// Readability is asked of the database by id, so the spelling the caller composed and
/// the spelling the host interned no longer have to agree for the answer to be right.
fn module_file_id(db: &RootDatabaseImpl, vfs: &Vfs, path: &Path) -> BodyRef {
    let interned = match vfs.file_id(&VfsPath::new(path.to_path_buf())) {
        Some(id) => Some(id),
        None => {
            // A consumer may seed its VFS with canonicalised paths while the caller
            // here composes `root.join(relative)`, which does not resolve a symlink
            // inside the tree. Retry once resolved; the syscall runs only on a miss.
            path.canonicalize().ok().and_then(|c| vfs.file_id(&VfsPath::new(c)))
        }
    };
    let Some(id) = interned else { return BodyRef::Absent };
    if db.file_is_unread(id) {
        BodyRef::Unread(id)
    } else {
        BodyRef::Readable(id)
    }
}

/// Intern a metadata composing file (already read and hashed in the parallel pass)
/// to a stable [`FileId`] and add it to the metadata file set, recording its
/// pre-computed content revision. Returns `None` when the file is absent from
/// `revisions_by_path` — it could not be read (discovered then vanished) — so the
/// caller drops it from the MDO. `alloc_file_id` is idempotent: an already-watched
/// path keeps its id. This runs under the vfs lock and does no I/O.
fn intern_metadata_file(
    vfs: &mut Vfs,
    path: &Path,
    revisions_by_path: &HashMap<PathBuf, u64>,
    file_set: &mut FileSet,
    revisions: &mut Vec<(FileId, u64)>,
) -> Option<FileId> {
    let revision = *revisions_by_path.get(path)?;
    let vfs_path = VfsPath::new(path.to_path_buf());
    let file_id = vfs.alloc_file_id(vfs_path.clone());
    file_set.insert(file_id, vfs_path);
    revisions.push((file_id, revision));
    Some(file_id)
}

/// Whether `path` is in the caller's `changed` set. The set may hold *canonicalised*
/// paths (a consumer that derives its change list from a `canonicalize`d disk scan),
/// while `path` is a discovery join (`root` + relative) that does NOT resolve a symlink
/// *inside* the tree. So on a direct miss, retry against the canonicalised path — else a
/// changed file under a symlinked subdirectory would keep its stale revision and never
/// re-parse. The syscall runs only on a miss, so an already-canonical caller (the LSP)
/// never pays it.
fn is_changed(changed: &HashSet<&Path>, path: &Path) -> bool {
    if changed.contains(path) {
        return true;
    }
    matches!(path.canonicalize(), Ok(canonical) if changed.contains(canonical.as_path()))
}

/// Enroll a composing file during an incremental refresh: intern it, ensure it is
/// in the metadata file set, and (re)read its revision only if it changed or is
/// brand-new — an unchanged, already-enrolled file keeps its boot revision and is
/// not read. A newly added file is recorded in `new_file_ids` so the caller maps
/// its source root. Returns `None` only when a changed/new file cannot be read
/// (vanished), so the caller drops that MDO.
fn enroll_refresh(
    vfs: &mut Vfs,
    path: &Path,
    changed: &HashSet<&Path>,
    file_set: &mut FileSet,
    new_file_ids: &mut Vec<FileId>,
    revisions: &mut Vec<(FileId, u64)>,
) -> Option<FileId> {
    let vfs_path = VfsPath::new(path.to_path_buf());
    let is_new = file_set.file_for_path(&vfs_path).is_none();

    if is_changed(changed, path) || is_new {
        let revision = content_revision(&read_disk_text(path).ok()?);
        let file_id = vfs.alloc_file_id(vfs_path.clone());
        file_set.insert(file_id, vfs_path);
        if is_new {
            new_file_ids.push(file_id);
        }
        revisions.push((file_id, revision));
        Some(file_id)
    } else {
        file_set.file_for_path(&vfs_path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Arc;

    use ide_db::metadata::resolve_metadata_object;

    /// A caller-owned VFS for tests: a single-threaded `RefCell` is enough since the
    /// host's metadata policy acquires the write only for the interning critical
    /// section and never holds it across a yield.
    struct TestVfs(RefCell<Vfs>);
    impl VfsWrite for TestVfs {
        fn with_write<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
            f(&mut self.0.borrow_mut())
        }
    }

    fn catalog_xml(name: &str, uuid: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="{uuid}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
        )
    }

    #[test]
    fn bootstrap_and_refresh_never_register_metadata_inside_source_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let catalogs = root.join("Catalogs");
        let common = root.join("CommonModules/Hidden");
        std::fs::create_dir_all(&catalogs).unwrap();
        std::fs::create_dir_all(common.join("Ext")).unwrap();
        let catalog = catalogs.join("Allowed.xml");
        let hidden_xml = root.join("CommonModules/Hidden.xml");
        let hidden_module = common.join("Ext/Module.bsl");
        std::fs::write(&catalog, catalog_xml("Allowed", "00000000-0000-0000-0000-000000000051"))
            .unwrap();
        std::fs::write(
            &hidden_xml,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000052">
        <Properties><Name>Hidden</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(&hidden_module, "Процедура Скрытая()\nКонецПроцедуры").unwrap();

        let mut host = AnalysisHost::default();
        host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        let mut snapshot =
            ide_db::metadata::WorkspaceConfigsSnapshot::from_paths(vec![(None, root.clone())]);
        snapshot.source_exclusions =
            project_model::ExcludedPaths::new([root.join("CommonModules")]);
        host.raw_database_mut().set_workspace_configs_snapshot(snapshot);
        let vfs = TestVfs(RefCell::new(Vfs::default()));
        host.bootstrap_metadata_substrate(&vfs);

        let root_key = root.to_string_lossy().to_string();
        let listing = host.raw_database().metadata_listing(&root_key).unwrap();
        assert!(
            resolve_metadata_object(
                host.raw_database(),
                listing,
                bsl_metadata::MdoType::Catalog,
                "Allowed".to_owned(),
            )
            .is_some(),
            "allowed metadata did not reach the substrate"
        );
        assert!(listing.common_modules(host.raw_database()).is_empty());
        vfs.with_write(|vfs| {
            assert!(vfs.file_id(&VfsPath::new(catalog.clone())).is_some());
            assert!(vfs.file_id(&VfsPath::new(hidden_xml.clone())).is_none());
            assert!(vfs.file_id(&VfsPath::new(hidden_module.clone())).is_none());
        });

        std::fs::write(&hidden_module, "Процедура Новая()\nКонецПроцедуры").unwrap();
        assert!(
            !host.refresh_metadata_substrate(&vfs, &[hidden_xml.clone(), hidden_module.clone()],)
        );
        vfs.with_write(|vfs| {
            assert!(vfs.file_id(&VfsPath::new(hidden_xml.clone())).is_none());
            assert!(vfs.file_id(&VfsPath::new(hidden_module.clone())).is_none());
        });

        host.raw_database_mut().set_workspace_configs_snapshot(
            ide_db::metadata::WorkspaceConfigsSnapshot::from_paths(vec![(None, root)]),
        );
        host.bootstrap_metadata_substrate(&vfs);
        let listing = host.raw_database().metadata_listing(&root_key).unwrap();
        assert_eq!(listing.common_modules(host.raw_database()).len(), 1);
        vfs.with_write(|vfs| {
            assert!(vfs.file_id(&VfsPath::new(hidden_xml)).is_some());
        });
    }

    /// The shared policy works on a bare `AnalysisHost` with a caller-owned VFS — the
    /// reuse contract for the MCP server and CLI. Mirrors the LSP-side incremental test:
    /// a content edit re-parses only the touched MDO, a structural add resolves and
    /// invalidates the prior absent-key miss, and a removal tombstones the object.
    #[test]
    fn refresh_is_incremental_and_tracks_structure_on_bare_host() {
        let cat = bsl_metadata::MdoType::Catalog;
        let root = std::env::temp_dir().join(format!(
            "ihc_refresh_meta_{}_{}",
            std::process::id(),
            line!()
        ));
        let catalogs = root.join("Catalogs");
        std::fs::create_dir_all(&catalogs).unwrap();
        let write = |name: &str, uuid: &str| {
            std::fs::write(catalogs.join(format!("{name}.xml")), catalog_xml(name, uuid)).unwrap()
        };
        write("Справочник1", "00000000-0000-0000-0000-000000000001");
        write("Товары", "00000000-0000-0000-0000-000000000002");

        let mut host = AnalysisHost::default();
        host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        let vfs = TestVfs(RefCell::new(Vfs::default()));
        host.bootstrap_metadata_substrate(&vfs);

        let root_key = root.to_string_lossy().to_string();
        let resolve = |host: &AnalysisHost, name: &str| {
            let db = host.raw_database();
            let listing = db.metadata_listing(&root_key).unwrap();
            resolve_metadata_object(db, listing, cat, name.to_string())
        };

        let c1 = resolve(&host, "Справочник1").expect("Справочник1");
        let tovary = resolve(&host, "Товары").expect("Товары");

        // CONTENT edit to Товары: re-read only it.
        write("Товары", "00000000-0000-0000-0000-0000000000ff");
        assert!(host.refresh_metadata_substrate(&vfs, &[catalogs.join("Товары.xml")]));
        assert!(
            Arc::ptr_eq(&c1, &resolve(&host, "Справочник1").unwrap()),
            "a content edit to Товары must not re-resolve the sibling"
        );
        assert!(
            !Arc::ptr_eq(&tovary, &resolve(&host, "Товары").unwrap()),
            "Товары re-parses after its content changed"
        );
        let c1 = resolve(&host, "Справочник1").unwrap();

        // STRUCTURE add: a brand-new catalog appears and resolves.
        assert!(resolve(&host, "Услуги").is_none(), "Услуги absent before the add");
        write("Услуги", "00000000-0000-0000-0000-000000000003");
        assert!(host.refresh_metadata_substrate(&vfs, &[catalogs.join("Услуги.xml")]));
        assert_eq!(resolve(&host, "Услуги").expect("Услуги after add").name, "Услуги");
        assert!(
            Arc::ptr_eq(&c1, &resolve(&host, "Справочник1").unwrap()),
            "a structure add must not re-resolve an untouched sibling"
        );

        // STRUCTURE remove: deleting a catalog tombstones it.
        std::fs::remove_file(catalogs.join("Товары.xml")).unwrap();
        assert!(host.refresh_metadata_substrate(&vfs, &[catalogs.join("Товары.xml")]));
        assert!(resolve(&host, "Товары").is_none(), "removed catalog resolves to None");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refresh_role_rights_is_incremental_and_tracks_structure_on_bare_host() {
        use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
        use ide_db::metadata::{resolve_role, RoleEntry};

        fn role_xml(name: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000081">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
            )
        }

        fn rights_xml(set_for_new_objects: bool, object_name: &str, condition: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>{set_for_new_objects}</setForNewObjects>
    <setForAttributesByDefault>false</setForAttributesByDefault>
    <independentRightsOfChildObjects>false</independentRightsOfChildObjects>
    <object>
        <name>{object_name}</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>{condition}</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "ihc_role_refresh_{}_{}",
            std::process::id(),
            line!()
        ));
        let roles = root.join("Roles");
        std::fs::create_dir_all(roles.join("ТестоваяРоль/Ext")).unwrap();
        std::fs::write(roles.join("ТестоваяРоль.xml"), role_xml("ТестоваяРоль")).unwrap();
        let rights_path = roles.join("ТестоваяРоль/Ext/Rights.xml");
        std::fs::write(
            &rights_path,
            rights_xml(
                false,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.Организации)",
            ),
        )
        .unwrap();
        let _: Option<RoleEntry> = None;

        let mut host = AnalysisHost::default();
        host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        let vfs = TestVfs(RefCell::new(Vfs::default()));
        host.bootstrap_metadata_substrate(&vfs);

        let consumer_path = root.join("RoleConsumer.bsl");
        let consumer_vfs_path = VfsPath::new(consumer_path.to_string_lossy().as_ref());
        let consumer_file = vfs.with_write(|vfs| vfs.alloc_file_id(consumer_vfs_path.clone()));
        let mut file_set = FileSet::new();
        file_set.insert(consumer_file, consumer_vfs_path);
        let db = host.raw_database_mut();
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(file_set));
        db.set_file_source_root(consumer_file, BSL_SOURCE_ROOT);
        db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

        let db = host.raw_database();
        let root_key = root.to_string_lossy().to_string();
        let listing = db.metadata_listing(&root_key).expect("listing set for the config root");
        let resolved_before = resolve_role(db, listing, "ТестоваяРоль".to_string())
            .expect("role resolves through the bootstrapped listing");
        assert!(!resolved_before.data().set_for_new_objects());
        assert_eq!(resolved_before.data().objects().len(), 1);
        assert_eq!(resolved_before.data().objects()[0].name, "Контрагенты");

        std::fs::write(
            &rights_path,
            rights_xml(
                true,
                "Catalog.Контрагенты",
                "Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)",
            ),
        )
        .unwrap();
        assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&rights_path)));

        let resolved_after = resolve_role(
            host.raw_database(),
            host.raw_database()
                .metadata_listing(&root_key)
                .expect("listing stays available after refresh"),
            "ТестоваяРоль".to_string(),
        )
        .expect("role re-resolves after the rights edit");
        assert!(resolved_after.data().set_for_new_objects());
        assert_eq!(
            resolved_after.data().objects()[0].restrictions,
            vec!["Контрагенты.Ссылка В (ВЫБРАТЬ Ссылка ИЗ Справочник.ФизическиеЛица)".to_string()]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Wave 2e: the host bootstrap and incremental refresh must track recursively
    /// discovered subsystem XML (a nested child under
    /// `Subsystems/<Name>/Subsystems/`) through the metadata substrate — adding a
    /// new subsystem file surfaces it, removing one tombstones it, and an
    /// untouched sibling is not churned. Red until the substrate carries a
    /// `subsystems` listing field and `bootstrap`/`refresh` discover subsystems.
    #[test]
    fn refresh_subsystem_structure_tracks_recursive_add_and_remove_on_bare_host() {
        fn subsystem_xml(name: &str) -> String {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000093">
        <Properties>
            <Name>{name}</Name>
        </Properties>
    </Subsystem>
</MetaDataObject>"#
            )
        }

        let root = std::env::temp_dir().join(format!(
            "ihc_subsystem_refresh_{}_{}",
            std::process::id(),
            line!()
        ));
        let subsystems = root.join("Subsystems");
        let nested = subsystems.join("Корневая/Subsystems");
        std::fs::create_dir_all(&nested).unwrap();
        let root_xml = subsystems.join("Корневая.xml");
        let child_xml = nested.join("Дочерняя.xml");
        std::fs::write(&root_xml, subsystem_xml("Корневая")).unwrap();
        std::fs::write(&child_xml, subsystem_xml("Дочерняя")).unwrap();

        let mut host = AnalysisHost::default();
        host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        let vfs = TestVfs(RefCell::new(Vfs::default()));
        host.bootstrap_metadata_substrate(&vfs);

        let root_key = root.to_string_lossy().to_string();
        let subsystem_names = |host: &AnalysisHost| -> Vec<String> {
            let db = host.raw_database();
            let listing = db.metadata_listing(&root_key).expect("listing set for the config root");
            listing.subsystems(db).iter().map(|entry| entry.name.clone()).collect()
        };

        let after_bootstrap = subsystem_names(&host);
        assert!(
            after_bootstrap.iter().any(|n| n == "Корневая"),
            "root subsystem discovered by the bootstrap"
        );
        assert!(
            after_bootstrap.iter().any(|n| n == "Дочерняя"),
            "nested child subsystem discovered through recursive structure walk"
        );

        let new_xml = subsystems.join("Новая.xml");
        std::fs::write(&new_xml, subsystem_xml("Новая")).unwrap();
        assert!(
            host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&new_xml)),
            "refresh must pick up the new subsystem XML"
        );
        let after_add = subsystem_names(&host);
        assert!(
            after_add.iter().any(|n| n == "Новая"),
            "newly added subsystem appears in the substrate after refresh"
        );

        std::fs::remove_file(&child_xml).unwrap();
        assert!(
            host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&child_xml)),
            "refresh must pick up the removed subsystem XML"
        );
        let after_remove = subsystem_names(&host);
        assert!(
            !after_remove.iter().any(|n| n == "Дочерняя"),
            "removed nested subsystem disappears from the substrate"
        );
        assert!(
            after_remove.iter().any(|n| n == "Корневая"),
            "untouched parent subsystem stays after the child removal"
        );

        std::fs::remove_dir_all(&root).ok();
    }
    /// The bootstrapped substrate attaches common attributes to the objects their
    /// composition names, and an edit of a common attribute's `Content` re-derives the
    /// objects it reaches without touching their own XML.
    #[test]
    fn bootstrap_and_refresh_track_common_attribute_composition() {
        let fixture = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/common_attributes"
        ));
        let root = std::env::temp_dir().join(format!(
            "ihc_common_attributes_{}_{}",
            std::process::id(),
            line!()
        ));
        for dir in ["Catalogs", "CommonAttributes"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            for entry in std::fs::read_dir(fixture.join(dir)).unwrap() {
                let entry = entry.unwrap();
                std::fs::copy(entry.path(), root.join(dir).join(entry.file_name())).unwrap();
            }
        }

        let mut host = AnalysisHost::default();
        host.raw_database_mut().set_all_config_paths(vec![(None, root.clone())]);
        let vfs = TestVfs(RefCell::new(Vfs::default()));
        host.bootstrap_metadata_substrate(&vfs);

        let root_key = root.to_string_lossy().to_string();
        let common_names = |host: &AnalysisHost, name: &str| -> Vec<String> {
            let db = host.raw_database();
            let listing = db.metadata_listing(&root_key).expect("listing set for the config root");
            let object = resolve_metadata_object(
                db,
                listing,
                bsl_metadata::MdoType::Catalog,
                name.to_string(),
            )
            .expect("catalog resolves through the bootstrapped listing");
            let mut names: Vec<String> =
                object.common_attributes.iter().map(|f| f.name.clone()).collect();
            names.sort();
            names
        };

        assert_eq!(
            common_names(&host, "Справочник1"),
            ["ОбластьДанныхОсновныеДанные", "ОбщийКомментарий", "Организация"]
        );
        assert_eq!(common_names(&host, "СправочникБезРазделения"), ["ОбщийКомментарий"]);

        let organisation = root.join("CommonAttributes").join("Организация.xml");
        let text = std::fs::read_to_string(&organisation).unwrap();
        // Flip the setting that follows this object's reference; the search is positional
        // so the checkout's line endings do not matter.
        let anchor = text.find("Catalog.СправочникБезРазделения").unwrap();
        let setting = anchor + text[anchor..].find("<xr:Use>DontUse</xr:Use>").unwrap();
        let edited = format!(
            "{}<xr:Use>Use</xr:Use>{}",
            &text[..setting],
            &text[setting + "<xr:Use>DontUse</xr:Use>".len()..]
        );
        assert_ne!(edited, text, "the fixture still carries the setting this test flips");
        std::fs::write(&organisation, edited).unwrap();
        assert!(host.refresh_metadata_substrate(&vfs, std::slice::from_ref(&organisation)));

        assert_eq!(
            common_names(&host, "СправочникБезРазделения"),
            ["ОбщийКомментарий", "Организация"]
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
