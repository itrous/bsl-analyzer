use bsl_metadata::Configuration;
use intern::NormName;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stdx::case::CaseExt;

/// Heap-size estimators for Salsa's `memory_usage` introspection, wrapping each
/// metadata substrate's own `estimated_heap_size` so it reads `None` (a query
/// that found nothing) as zero.
pub(crate) mod heap_estimate {
    use std::sync::Arc;

    pub(crate) fn mdo_heap(v: &Option<Arc<bsl_metadata::MetadataObject>>) -> usize {
        v.as_ref().map_or(0, |m| m.estimated_heap_size())
    }

    pub(crate) fn register_heap(v: &Option<Arc<bsl_metadata::Register>>) -> usize {
        v.as_ref().map_or(0, |r| r.estimated_heap_size())
    }

    pub(crate) fn defined_type_heap(v: &Option<Arc<bsl_metadata::DefinedType>>) -> usize {
        v.as_ref().map_or(0, |dt| dt.estimated_heap_size())
    }

    pub(crate) fn common_module_heap(v: &Option<Arc<bsl_metadata::CommonModule>>) -> usize {
        v.as_ref().map_or(0, |m| m.estimated_heap_size())
    }

    pub(crate) fn http_service_heap(v: &Option<Arc<bsl_metadata::HTTPService>>) -> usize {
        v.as_ref().map_or(0, |s| s.estimated_heap_size())
    }

    pub(crate) fn web_service_heap(v: &Option<Arc<bsl_metadata::WebService>>) -> usize {
        v.as_ref().map_or(0, |s| s.estimated_heap_size())
    }

    pub(crate) fn integration_service_heap(
        v: &Option<Arc<bsl_metadata::IntegrationService>>,
    ) -> usize {
        v.as_ref().map_or(0, |s| s.estimated_heap_size())
    }

    pub(crate) fn event_subscription_heap(
        v: &Option<Arc<bsl_metadata::EventSubscription>>,
    ) -> usize {
        v.as_ref().map_or(0, |s| s.estimated_heap_size())
    }

    pub(crate) fn scheduled_job_heap(v: &Option<Arc<bsl_metadata::ScheduledJob>>) -> usize {
        v.as_ref().map_or(0, |j| j.estimated_heap_size())
    }

    pub(crate) fn role_heap(v: &Option<Arc<bsl_metadata::Role>>) -> usize {
        v.as_ref().map_or(0, |r| r.estimated_heap_size())
    }

    pub(crate) fn subsystem_heap(v: &Option<Arc<bsl_metadata::Subsystem>>) -> usize {
        v.as_ref().map_or(0, |s| s.estimated_heap_size())
    }

    pub(crate) fn configuration_heap(v: &Arc<bsl_metadata::Configuration>) -> usize {
        v.estimated_heap_size()
    }

    /// Heap of a [`super::ConfigurationPathInput`]: the `path` string's own bytes
    /// (`root_revision` is `Copy`). The estimator receives the tuple of ALL
    /// declared fields in order, per the fork's `heap_size = path` convention for
    /// interned structs.
    pub(crate) fn configuration_path_input_heap((path, _root_revision): &(String, u32)) -> usize {
        path.capacity()
    }

    /// Heap of a [`super::WorkspaceConfigsInput`]: the snapshot's path vecs
    /// (declared + canonical) with their owned labels and roots, the closure
    /// index vecs, and the optional fingerprint string.
    pub(crate) fn workspace_configs_input_heap(
        (snapshot,): &(std::sync::Arc<super::WorkspaceConfigsSnapshot>,),
    ) -> usize {
        let paths = &snapshot.paths;
        stdx::heap::vec_bytes::<(Option<String>, std::path::PathBuf)>(paths.len())
            + paths
                .iter()
                .map(|(label, root)| label.as_ref().map_or(0, String::capacity) + root.capacity())
                .sum::<usize>()
            + stdx::heap::vec_bytes::<std::path::PathBuf>(snapshot.canonical_paths.len())
            + snapshot.canonical_paths.iter().map(|p| p.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::RootKind>(snapshot.kinds.len())
            + stdx::heap::vec_bytes::<Vec<usize>>(snapshot.closures.len())
            + snapshot
                .closures
                .iter()
                .map(|c| stdx::heap::vec_bytes::<usize>(c.len()))
                .sum::<usize>()
            + snapshot.fingerprint.as_ref().map_or(0, String::capacity)
            + snapshot.source_exclusions.heap_bytes()
    }

    /// Heap of a [`super::MetadataListingInput`]: the ten per-family `Arc<Vec<_>>`
    /// entry lists plus each entry's owned name string (the other entry fields —
    /// `FileId`s, the MDO kind — are `Copy`). New heap-owning listing families
    /// must be added here too.
    #[allow(clippy::type_complexity)] // mirrors MetadataListingInput's field tuple exactly
    pub(crate) fn metadata_listing_input_heap(
        fields: &(
            Arc<Vec<super::MdoEntry>>,
            Arc<Vec<super::DefinedTypeEntry>>,
            Arc<Vec<super::CommonModuleEntry>>,
            Arc<Vec<super::EventSubscriptionEntry>>,
            Arc<Vec<super::ScheduledJobEntry>>,
            Arc<Vec<super::RoleEntry>>,
            Arc<Vec<super::HTTPServiceEntry>>,
            Arc<Vec<super::WebServiceEntry>>,
            Arc<Vec<super::IntegrationServiceEntry>>,
            Arc<Vec<super::SubsystemEntry>>,
            Arc<Vec<super::CommonAttributeEntry>>,
        ),
    ) -> usize {
        let (
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
            common_attributes,
        ) = fields;

        stdx::heap::vec_bytes::<super::MdoEntry>(entries.len())
            + entries.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::DefinedTypeEntry>(defined_types.len())
            + defined_types.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::CommonModuleEntry>(common_modules.len())
            + common_modules.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::EventSubscriptionEntry>(event_subscriptions.len())
            + event_subscriptions.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::ScheduledJobEntry>(scheduled_jobs.len())
            + scheduled_jobs.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::RoleEntry>(roles.len())
            + roles.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::HTTPServiceEntry>(http_services.len())
            + http_services.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::WebServiceEntry>(web_services.len())
            + web_services.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::IntegrationServiceEntry>(integration_services.len())
            + integration_services.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::SubsystemEntry>(subsystems.len())
            + subsystems.iter().map(|e| e.name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<super::CommonAttributeEntry>(common_attributes.len())
            + common_attributes.iter().map(|e| e.name.capacity()).sum::<usize>()
    }

    /// A `resolve_metadata_object` result is a clone of an `Arc` owned by
    /// `parse_mdo_query`; the payload is counted there once.
    pub(crate) fn shared_mdo_heap(_v: &Option<Arc<bsl_metadata::MetadataObject>>) -> usize {
        0
    }

    /// A `resolve_register`/`resolve_register_by_name` result is a clone of an
    /// `Arc` owned by `parse_register_query`; the payload is counted there once.
    pub(crate) fn shared_register_heap(_v: &Option<Arc<bsl_metadata::Register>>) -> usize {
        0
    }

    pub(crate) fn common_attribute_set_heap(v: &Arc<bsl_metadata::CommonAttributeSet>) -> usize {
        stdx::heap::vec_bytes::<bsl_metadata::CommonAttribute>(v.attributes.len())
            + v.attributes
                .iter()
                .map(|a| {
                    a.name.capacity()
                        + a.attr_type.estimated_heap_size()
                        + stdx::heap::map_table_bytes::<
                            (bsl_metadata::MdoType, String),
                            bsl_metadata::CommonAttributeUse,
                        >(a.content.len())
                        + a.content.keys().map(|(_, name)| name.capacity()).sum::<usize>()
                })
                .sum::<usize>()
    }

    /// A `resolve_common_module`/`resolve_common_module_by_file` result is a
    /// clone of an `Arc` owned by `parse_common_module_query`; the payload is
    /// counted there once.
    pub(crate) fn shared_common_module_heap(_v: &Option<Arc<bsl_metadata::CommonModule>>) -> usize {
        0
    }

    /// A `resolve_http_service`/`resolve_http_service_by_file` result is a clone
    /// of an `Arc` owned by `parse_http_service_query`; the payload is counted
    /// there once.
    pub(crate) fn shared_http_service_heap(_v: &Option<Arc<bsl_metadata::HTTPService>>) -> usize {
        0
    }

    /// A `resolve_web_service`/`resolve_web_service_by_file` result is a clone of
    /// an `Arc` owned by `parse_web_service_query`; the payload is counted there
    /// once.
    pub(crate) fn shared_web_service_heap(_v: &Option<Arc<bsl_metadata::WebService>>) -> usize {
        0
    }

    /// A `resolve_integration_service`/`resolve_integration_service_by_file`
    /// result is a clone of an `Arc` owned by `parse_integration_service_query`;
    /// the payload is counted there once.
    pub(crate) fn shared_integration_service_heap(
        _v: &Option<Arc<bsl_metadata::IntegrationService>>,
    ) -> usize {
        0
    }

    /// A `resolve_event_subscription` result is a clone of an `Arc` owned by
    /// `parse_event_subscription_query`; the payload is counted there once.
    pub(crate) fn shared_event_subscription_heap(
        _v: &Option<Arc<bsl_metadata::EventSubscription>>,
    ) -> usize {
        0
    }

    /// A `resolve_scheduled_job` result is a clone of an `Arc` owned by
    /// `parse_scheduled_job_query`; the payload is counted there once.
    pub(crate) fn shared_scheduled_job_heap(_v: &Option<Arc<bsl_metadata::ScheduledJob>>) -> usize {
        0
    }

    /// A `resolve_role` result is a clone of an `Arc` owned by `parse_role_query`;
    /// the payload is counted there once.
    pub(crate) fn shared_role_heap(_v: &Option<Arc<bsl_metadata::Role>>) -> usize {
        0
    }

    /// A `resolve_subsystem` result is a clone of an `Arc` owned by
    /// `parse_subsystem_query`; the payload is counted there once.
    pub(crate) fn shared_subsystem_heap(_v: &Option<Arc<bsl_metadata::Subsystem>>) -> usize {
        0
    }

    /// Unlike the other `resolve_*` accessors, `resolve_defined_type` projects to
    /// the defined type's *underlying* type via a fresh `Arc::new(...)` — it does
    /// NOT alias `parse_defined_type_query`'s memo — so its payload must be
    /// counted here for real.
    pub(crate) fn defined_type_projection_heap(
        v: &Option<Arc<bsl_metadata::AttributeType>>,
    ) -> usize {
        v.as_ref().map_or(0, |t| t.estimated_heap_size())
    }

    /// Heap of a `HashMap<String, V>` name index: the table plus each key's owned
    /// string bytes.
    fn string_key_map_heap<V>(map: &std::collections::HashMap<String, V>) -> usize {
        stdx::heap::map_table_bytes::<String, V>(map.len())
            + map.keys().map(String::capacity).sum::<usize>()
    }

    /// Heap of the `by_name` / `by_module_file` / `module_file_by_name` triple
    /// shared by [`super::CommonModuleIndex`], [`super::HTTPServiceIndex`],
    /// [`super::WebServiceIndex`], and [`super::IntegrationServiceIndex`]: each
    /// table plus the name strings each of the three separately owns (a fresh
    /// `fold_lower()` allocation per table, not shared across them).
    fn triple_name_index_heap(
        by_name: &std::collections::HashMap<String, vfs::FileId>,
        by_module_file: &std::collections::HashMap<vfs::FileId, String>,
        module_file_by_name: &std::collections::HashMap<String, vfs::FileId>,
    ) -> usize {
        string_key_map_heap(by_name)
            + stdx::heap::map_table_bytes::<vfs::FileId, String>(by_module_file.len())
            + by_module_file.values().map(String::capacity).sum::<usize>()
            + string_key_map_heap(module_file_by_name)
    }

    // `NormName` keys are `Copy` ids into the global intern pool: the table
    // itself is the whole cost, unlike the `String`-keyed indexes below whose
    // key bytes are each table's own allocation.
    pub(crate) fn config_index_heap(v: &Arc<super::ConfigIndex>) -> usize {
        let by_name_bytes = stdx::heap::map_table_bytes::<
            (bsl_metadata::MdoType, intern::NormName),
            super::MdoFileIds,
        >(v.by_name.len());
        let register_bytes = stdx::heap::map_table_bytes::<
            intern::NormName,
            (bsl_metadata::MdoType, super::MdoFileIds),
        >(v.register_by_name.len());
        by_name_bytes + register_bytes
    }

    pub(crate) fn defined_type_index_heap(v: &Arc<super::DefinedTypeIndex>) -> usize {
        stdx::heap::map_table_bytes::<intern::NormName, vfs::FileId>(v.by_name.len())
    }

    /// The shared triple plus the fourth table this index alone carries. The
    /// compiler does not ask for this summand when a field is added, so a new table
    /// silently under-reports the index's weight unless it is added by hand.
    pub(crate) fn common_module_index_heap(v: &Arc<super::CommonModuleIndex>) -> usize {
        triple_name_index_heap(&v.by_name, &v.by_module_file, &v.module_file_by_name)
            + string_key_map_heap(&v.unread_module_file_by_name)
    }

    pub(crate) fn event_subscription_index_heap(v: &Arc<super::EventSubscriptionIndex>) -> usize {
        string_key_map_heap(&v.by_name)
    }

    pub(crate) fn scheduled_job_index_heap(v: &Arc<super::ScheduledJobIndex>) -> usize {
        string_key_map_heap(&v.by_name)
    }

    pub(crate) fn role_index_heap(v: &Arc<super::RoleIndex>) -> usize {
        string_key_map_heap(&v.by_name)
    }

    pub(crate) fn http_service_index_heap(v: &Arc<super::HTTPServiceIndex>) -> usize {
        triple_name_index_heap(&v.by_name, &v.by_module_file, &v.module_file_by_name)
    }

    pub(crate) fn web_service_index_heap(v: &Arc<super::WebServiceIndex>) -> usize {
        triple_name_index_heap(&v.by_name, &v.by_module_file, &v.module_file_by_name)
    }

    pub(crate) fn integration_service_index_heap(v: &Arc<super::IntegrationServiceIndex>) -> usize {
        triple_name_index_heap(&v.by_name, &v.by_module_file, &v.module_file_by_name)
    }

    pub(crate) fn subsystem_index_heap(v: &Arc<super::SubsystemIndex>) -> usize {
        string_key_map_heap(&v.by_name)
    }
}

#[salsa::interned(debug, heap_size = heap_estimate::configuration_path_input_heap)]
pub struct ConfigurationPathInput {
    pub path: String,
    #[returns(copy)]
    pub root_revision: u32,
}

pub fn intern_configuration_path<'db>(
    db: &'db dyn salsa::Database,
    raw_path: &str,
    root_revision: u32,
) -> ConfigurationPathInput<'db> {
    let canonical = canonicalize_configuration_path(raw_path);
    ConfigurationPathInput::new(db, canonical, root_revision)
}

/// Per-config-root revision counter, as a Salsa input so that config-dependent
/// queries which read it (via [`intern_configuration_path`] callers running
/// inside a tracked query) record a dependency on the specific root. Bumping one
/// root's revision then invalidates only the queries that touched that root,
/// instead of a single global counter invalidating every configuration.
#[salsa::input(debug, heap_size = stdx::heap::zero)]
pub struct ConfigRevisionInput {
    #[returns(copy)]
    pub revision: u32,
}

pub(crate) fn canonicalize_configuration_path(raw_path: &str) -> String {
    if cfg!(windows) {
        let trimmed = raw_path.strip_prefix(r"\\?\").unwrap_or(raw_path);
        let mut s = trimmed.replace('\\', "/");
        s.make_ascii_lowercase();
        s
    } else if raw_path.contains('\\') {
        raw_path.replace('\\', "/")
    } else {
        raw_path.to_owned()
    }
}

/// What a registered config root is. The label in `paths` tells the base from
/// the rest; the kind tells an extension from an external object, which shares
/// the extension's per-file plumbing (own-root matching, chain entry, listing)
/// but is a leaf: it sees every extension and no one sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Base,
    Extension,
    /// An external data processor or report export (EPF/ERF), of the kind the
    /// project established when it validated the root.
    External(bsl_metadata::ExternalObjectKind),
}

impl RootKind {
    /// The legacy reading of a label list: the unlabelled slot is the base and
    /// every other one an extension. External objects only ever enter through
    /// [`WorkspaceConfigsSnapshot::from_project`].
    pub fn from_labels(paths: &[(Option<String>, PathBuf)]) -> Vec<RootKind> {
        paths
            .iter()
            .map(|(label, _)| if label.is_none() { RootKind::Base } else { RootKind::Extension })
            .collect()
    }
}

/// Atomic description of the workspace's config roots and their dependency
/// topology. ONE input field holds the whole value because Salsa tracks input
/// FIELDS independently (a revision per setter): splitting paths, closures and
/// fingerprint across fields would let a reload be observed half-applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfigsSnapshot {
    /// Config roots in declaration order. At most one `None` label — the base;
    /// an extension-only project has none, and every entry is an extension or
    /// an external object.
    pub paths: Vec<(Option<String>, PathBuf)>,
    /// Per entry (same order as `paths`): what the root is.
    pub kinds: Vec<RootKind>,
    /// Canonicalized counterpart of `paths` (same order) — what per-file
    /// longest-prefix matching uses, so a symlinked workspace still matches.
    pub canonical_paths: Vec<PathBuf>,
    /// Per entry: ordered transitive dependency chain (indices into `paths`,
    /// dependencies first, the entry itself excluded). Empty for the base and
    /// for independent extensions — which keeps the pre-dependency semantics:
    /// a file sees the base plus its own extension only. For an external object
    /// it is what the topology gave it: every extension in topological order
    /// unless the object narrowed its view with `dependsOn`.
    pub closures: Vec<Vec<usize>>,
    /// Global root order for whole-workspace composition: the base first when the
    /// project has one, then extensions in dependencies-before-dependents order.
    /// Indices point into
    /// `paths`; unlike `closures`, this is the designer-wide view used when no
    /// file anchors visibility.
    pub topological_order: Vec<usize>,
    /// Topology fingerprint (full hex digest) when built from a validated
    /// project; `None` for legacy path-only registration.
    pub fingerprint: Option<String>,
    /// The project's `[source].exclude`. Part of this one value so a reload that
    /// narrows the project never shows new roots with old exclusions or the reverse;
    /// no metadata loader reads anything inside these directories.
    pub source_exclusions: project_model::ExcludedPaths,
}

impl WorkspaceConfigsSnapshot {
    /// Legacy shape: bare roots, no dependency edges. Every extension is
    /// independent, exactly the pre-`dependsOn` visibility.
    pub fn from_paths(paths: Vec<(Option<String>, PathBuf)>) -> Self {
        let canonical_paths = paths
            .iter()
            .map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect();
        let closures = vec![Vec::new(); paths.len()];
        let topological_order = (0..paths.len()).collect();
        let kinds = RootKind::from_labels(&paths);
        Self {
            paths,
            kinds,
            canonical_paths,
            closures,
            topological_order,
            fingerprint: None,
            source_exclusions: project_model::ExcludedPaths::default(),
        }
    }

    /// The full shape from a validated project. A configured base occupies the
    /// first slot; extension-only projects contain only extension slots. Each
    /// extension carries its ordered transitive dependency chain and the topology
    /// fingerprint.
    pub fn from_project(project: &project_model::Project) -> Self {
        let topology = project.extension_topology();
        let mut paths: Vec<(Option<String>, PathBuf)> = Vec::new();
        let mut kinds: Vec<RootKind> = Vec::new();
        let mut canonical_paths = Vec::new();
        let mut closures: Vec<Vec<usize>> = Vec::new();
        let base = project.semantic_base_path();
        let offset = usize::from(base.is_some());
        if let Some(base) = base {
            let base = base.to_path_buf();
            let canonical_base = std::fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
            paths.push((None, base));
            kinds.push(RootKind::Base);
            canonical_paths.push(canonical_base);
            closures.push(Vec::new());
        }
        for node in topology.nodes() {
            paths.push((Some(node.name().to_string()), node.path().to_path_buf()));
            canonical_paths.push(node.canonical_path().to_path_buf());
            kinds.push(match node.kind() {
                project_model::NodeKind::External(kind) => RootKind::External(kind),
                project_model::NodeKind::Extension => RootKind::Extension,
            });
            closures.push(node.closure().iter().map(|id| id.index() + offset).collect());
        }
        let topological_order = base
            .map(|_| 0)
            .into_iter()
            .chain(topology.topological_order().iter().map(|id| id.index() + offset))
            .collect();
        Self {
            paths,
            kinds,
            canonical_paths,
            closures,
            topological_order,
            fingerprint: Some(topology.fingerprint().to_hex()),
            source_exclusions: project.source_exclusions().clone(),
        }
    }

    /// Replace each configured root spelling with its canonical form. For hosts
    /// whose file universe is enumerated canonically (the MCP resident/graph
    /// scans): substrate back-links join a root against a canonical `.bsl`
    /// path, so the registered roots must be canonical too or the reverse
    /// lookup misses on a symlinked workspace.
    pub fn canonicalized(mut self) -> Self {
        for (idx, (_, path)) in self.paths.iter_mut().enumerate() {
            *path = self.canonical_paths[idx].clone();
        }
        self
    }

    pub fn is_external(&self, idx: usize) -> bool {
        self.kinds.get(idx).is_some_and(|kind| matches!(kind, RootKind::External(_)))
    }

    /// Whether any registered root is an external object.
    pub fn has_externals(&self) -> bool {
        (0..self.paths.len()).any(|idx| self.is_external(idx))
    }

    pub fn has_base(&self) -> bool {
        self.kinds.contains(&RootKind::Base)
    }

    /// The slot of the root owning `path`: the longest registered root — by its
    /// configured or its canonical spelling — that is a prefix of it.
    pub fn owner_of_path(&self, path: &std::path::Path) -> Option<usize> {
        self.paths
            .iter()
            .zip(&self.canonical_paths)
            .enumerate()
            .filter_map(|(idx, ((_, configured), canonical))| {
                [configured, canonical]
                    .into_iter()
                    .filter(|root| path.starts_with(root))
                    .map(|root| root.as_os_str().len())
                    .max()
                    .map(|len| (len, idx))
            })
            .max_by_key(|&(len, _)| len)
            .map(|(_, idx)| idx)
    }

    pub fn owner_kind_of_path(&self, path: &std::path::Path) -> Option<RootKind> {
        self.owner_of_path(path).map(|idx| self.kinds[idx])
    }

    /// The external root owning `path`, with its kind: the configured spelling
    /// (so a relative object path can be cut from a file enumerated either way,
    /// the caller strips whichever prefix matches) and the export's kind.
    pub fn external_root_of_path(
        &self,
        path: &std::path::Path,
    ) -> Option<(PathBuf, PathBuf, bsl_metadata::ExternalObjectKind)> {
        let idx = self.owner_of_path(path)?;
        match self.kinds[idx] {
            RootKind::External(kind) => {
                Some((self.paths[idx].1.clone(), self.canonical_paths[idx].clone(), kind))
            }
            _ => None,
        }
    }

    /// The designer's view of the project: the base and its extensions in
    /// topological order, external objects left out. What a consumer with no
    /// file to anchor visibility to composes over — an external object is
    /// visible from its own files only.
    pub fn designer_order(&self) -> impl Iterator<Item = usize> + '_ {
        self.topological_order.iter().copied().filter(|&idx| !self.is_external(idx))
    }

    /// The designer's view followed by every external object: what a lookup
    /// keyed by `(kind, name)` composes over. An external kind never names a
    /// designer object, so the externals at the tail shadow nothing and are the
    /// only roots that hold their own kinds.
    pub fn inventory_order(&self) -> impl Iterator<Item = usize> + '_ {
        let externals = self.topological_order.iter().copied().filter(|&idx| self.is_external(idx));
        self.designer_order().chain(externals)
    }
}

#[salsa::input(singleton, debug, heap_size = heap_estimate::workspace_configs_input_heap)]
pub struct WorkspaceConfigsInput {
    #[returns(clone)]
    pub snapshot: Arc<WorkspaceConfigsSnapshot>,
}

/// Fallback revision counter for files that match no registered config root
/// (e.g. a single file opened without a workspace). Such reads record a
/// dependency here; coarse "everything changed" events bump it. A separate type
/// from the per-root [`ConfigRevisionInput`] so it can be a Salsa singleton:
/// there is exactly one global fallback per database.
#[salsa::input(singleton, debug, heap_size = stdx::heap::zero)]
pub struct GlobalConfigRevisionInput {
    #[returns(copy)]
    pub revision: u32,
}

/// Whether the host's initial workspace load has completed. Defaults to `true`
/// (batch analysis, the graph build, MCP and tests have no boot window); the
/// LSP server sets it to `false` while the initial VFS scan streams in and back
/// to `true` in its finalize, right before the metadata bootstrap and warm-up.
///
/// The whole-configuration loader consults it so that nothing computed during
/// the boot window can trigger the full-config XML parse — minutes of
/// non-cancellable work inside a single query, against a workspace that is not
/// even fully on disk in the VFS yet. The flag is a Salsa input (not a plain
/// field) so the read inside a calling query records a dependency: the finalize
/// flip then invalidates anything that resolved against the gated stub.
#[salsa::input(singleton, debug, heap_size = stdx::heap::zero)]
pub struct WorkspaceLoadStateInput {
    #[returns(copy)]
    pub complete: bool,
}

// Keyed by config root (base config + each extension), so the cache holds one entry
// per root, not per file/module — its size tracks the number of configurations, which
// is small. The cap must exceed the realistic number of extension roots: the graph
// build pre-warms every root before its parallel region (a per-root reload there would
// re-enter the metadata loader's `rayon::scope` inside a worker thread), so an eviction
// under the cap would let that load run in parallel and break the build's concurrency
// invariant. 1024 is far above any real extension count while still bounded.
#[salsa::tracked(
    lru = 1024,
    heap_size = heap_estimate::configuration_heap,
    returns(clone)
)]
pub fn load_configuration<'db>(
    db: &'db dyn salsa::Database,
    path_input: ConfigurationPathInput<'db>,
) -> Arc<Configuration> {
    let _span = tracing::info_span!("load_configuration").entered();

    let path_str = path_input.path(db);
    let path = PathBuf::from(path_str);

    tracing::warn!(?path, "METADATA LOAD: loading configuration from directory");

    let excluded = source_exclusions_for(db, path_input);
    let config = bsl_metadata::load_from_directory_scoped(&path, &excluded).unwrap_or_else(|e| {
        tracing::error!(error = %e, ?path, "failed to load configuration");
        Configuration::new("Configuration")
    });

    tracing::warn!(
        common_modules = config.common_modules().len(),
        metadata_objects = config.metadata_objects().len(),
        "METADATA LOAD: configuration loaded successfully"
    );

    Arc::new(config)
}

/// The user's exclusions, whole, as the whole-config load of `path_input` reads them.
///
/// A projection of the workspace snapshot rather than a read of it, so a snapshot set
/// again with the same exclusions — every project reload sets one — is backdated here
/// and leaves the root's load standing instead of re-parsing it. Deliberately not
/// narrowed to the exclusions near the root: a symlink inside the root can lead
/// anywhere, and the loader must be able to refuse to follow it into any of them.
#[salsa::tracked(returns(clone))]
fn source_exclusions_for<'db>(
    db: &'db dyn salsa::Database,
    path_input: ConfigurationPathInput<'db>,
) -> project_model::ExcludedPaths {
    let _ = path_input;
    WorkspaceConfigsInput::try_get(db)
        .map(|input| input.snapshot(db).source_exclusions.clone())
        .unwrap_or_default()
}

/// The base configuration merged with one extension's overlay, memoised per
/// (base, extension) path pair. `Configuration::merged_with_extension` deep-clones the
/// whole base configuration, so without this every metadata lookup of every extension
/// file re-cloned the entire configuration — the dominant cost when analysing a
/// heavily-extended project. Keyed on the two path inputs (which carry the config-root
/// revisions), so the merge runs once per extension and re-runs only when a config
/// actually changes. The result is identical to the inline merge, just shared.
#[salsa::tracked(
    lru = 1024,
    heap_size = heap_estimate::configuration_heap,
    returns(clone)
)]
pub fn merged_configuration<'db>(
    db: &'db dyn MetadataDb,
    main_input: ConfigurationPathInput<'db>,
    extension_input: ConfigurationPathInput<'db>,
) -> Arc<Configuration> {
    let _span = tracing::info_span!("merged_configuration").entered();
    // Through the trait method, never the free query: the graph build's batch
    // databases interpose a build-wide config cache there, and bypassing it made
    // every batch re-run the whole-config XML load inside the worker pool.
    let main = db.load_configuration(main_input);
    let extension = db.load_configuration(extension_input);
    Arc::new(main.merged_with_extension(&extension))
}

/// An ordered visibility chain of config roots: the base first, then the
/// file's transitive extension dependencies, then its own extension last.
/// Interned so equal chains (every file of one extension) share one query key,
/// and so the recursive prefix of a chain is itself a chain.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct ConfigChainInput<'db> {
    #[returns(ref)]
    pub inputs: Vec<ConfigurationPathInput<'db>>,
}

/// The whole-config merge of an ordered visibility chain, composed FORWARD:
/// each overlay is applied onto the accumulated configuration, so dependencies
/// apply before their dependents and the file's own extension wins last. The
/// recursion memoizes every prefix — the chain `[base, d1, own]` reuses
/// `[base, d1]`, which is exactly the chain of `d1`'s own files. Memoized
/// entries are bounded by the number of unique prefixes: at most the sum of
/// all chain lengths, far below the LRU cap for realistic extension counts.
#[salsa::tracked(
    lru = 1024,
    heap_size = heap_estimate::configuration_heap,
    returns(clone)
)]
pub fn chain_configuration<'db>(
    db: &'db dyn MetadataDb,
    chain: ConfigChainInput<'db>,
) -> Arc<Configuration> {
    let _span = tracing::info_span!("chain_configuration").entered();
    let inputs = chain.inputs(db.as_dyn_database());
    match inputs.split_last() {
        None => Arc::new(Configuration::new("Configuration")),
        Some((only, [])) => db.load_configuration(*only),
        Some((last, prefix)) => {
            let prefix_chain = ConfigChainInput::new(db.as_dyn_database(), prefix.to_vec());
            let base = chain_configuration(db, prefix_chain);
            let overlay = db.load_configuration(*last);
            Arc::new(base.merged_with_extension(&overlay))
        }
    }
}

/// The composing files of a single metadata object: the main `<Name>.xml` and an
/// optional `Ext/Predefined.xml`, plus the kind that selects the parser. Interned
/// so [`parse_mdo_query`] keys on the file identities; the per-file content
/// revisions drive invalidation, so editing one MDO's XML re-parses only it.
///
/// `listing` is the config root the object belongs to: its common attributes add fields to
/// the object, so the parse attaches them and the object is held once, already complete.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct MdoFiles<'db> {
    #[returns(copy)]
    pub mdo_type: bsl_metadata::MdoType,
    #[returns(copy)]
    pub main: vfs::FileId,
    #[returns(copy)]
    pub predefined: Option<vfs::FileId>,
    #[returns(copy)]
    pub listing: MetadataListingInput,
}

/// Whether a loose-XML metadata object's discovery key (its file stem) disagrees
/// with the `<Name>` parsed from the file, compared case-insensitively (BSL names
/// fold). Discovery keys these substrates by stem for structure-only invalidation;
/// the 1C designer keeps stem == `<Name>` (a rename touches both), so a divergence
/// only arises in hand-built or non-standard dumps.
fn stem_name_diverges(stem_key: &str, xml_name: &str) -> bool {
    stem_key.fold_lower() != xml_name.fold_lower()
}

/// Surface a stem-vs-`<Name>` divergence for a loose-XML metadata object as a
/// `warn`. Bootstrapped `resolve_*`/enumeration keys by the file stem while the
/// whole-config `find_*` keys by the parsed `<Name>`, so a divergence makes the two
/// paths disagree silently (a `<Name>` lookup misses the stem index; enumeration
/// reports the stem, not the `<Name>`). This is observability only — the resolve
/// still keys by stem. `stem_key` is the lookup key that matched the stem index.
fn warn_on_stem_name_divergence(kind: &str, stem_key: &str, xml_name: &str) {
    if stem_name_diverges(stem_key, xml_name) {
        tracing::warn!(
            kind,
            stem_key,
            xml_name,
            "metadata file stem disagrees with its <Name>; bootstrapped resolve keys by \
             stem and diverges from the whole-config <Name> lookup"
        );
    }
}

/// Parse one metadata object from its composing files, read through the versioned
/// VFS (`file_text`). Memoised per MDO and backdated on an unchanged object, so a
/// reload re-parses only the files that actually changed and only that object's
/// consumers are invalidated.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::mdo_heap, returns(clone))]
pub fn parse_mdo_query<'db>(
    db: &'db dyn base_db::SourceDatabase,
    files: MdoFiles<'db>,
) -> Option<Arc<bsl_metadata::MetadataObject>> {
    let _span = tracing::info_span!("parse_mdo").entered();

    let main_text = db.file_text_ref(files.main(db));
    let predefined_text = files.predefined(db).map(|fid| db.file_text_ref(fid));

    let mut object = bsl_metadata::parse_metadata_object_from_texts(
        files.mdo_type(db),
        main_text,
        predefined_text.map(|t| &**t),
    )?;
    let applied =
        common_attribute_set(db, files.listing(db)).for_object(object.mdo_type, &object.name);
    if !applied.is_empty() {
        object.set_common_attributes(applied);
    }
    Some(Arc::new(object))
}

/// One discovered metadata object in a config root's *structure* listing: which
/// kind, its name, and the [`vfs::FileId`]s of its composing files. Carries no
/// parsed content — only identities — so the listing changes on add/remove/rename
/// of an MDO, never on a content edit within one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MdoEntry {
    pub kind: bsl_metadata::MdoType,
    pub name: String,
    pub main: vfs::FileId,
    pub predefined: Option<vfs::FileId>,
}

/// One discovered defined type in a config root's *structure* listing: its name
/// and the [`vfs::FileId`] of its main XML. Defined types are global (keyed by
/// name, no kind, no predefined sidecar), so they ride a separate field of the
/// listing rather than [`MdoEntry`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefinedTypeEntry {
    pub name: String,
    pub main: vfs::FileId,
}

/// One discovered common module in a config root's *structure* listing: its name,
/// the [`vfs::FileId`] of its metadata XML, and the [`vfs::FileId`] of its
/// `Ext/Module.bsl` (the module source) when present. The module-file id backs the
/// reverse "which common module owns this `.bsl`" lookup ([`CommonModuleIndex`]'s
/// `by_module_file`); it is `None` for protected/binary modules with no source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommonModuleEntry {
    pub name: String,
    pub main: vfs::FileId,
    pub module_file: Option<vfs::FileId>,
    /// The body's id when the body exists but could not be read. Kept beside
    /// `module_file` rather than inside it because the two answer different
    /// questions: `module_file` is "a body you can read", which every consumer of
    /// the back-link wants, while this one is "a body whose absence from the list
    /// above proves nothing" — the only thing that lets name resolution refuse to
    /// conclude a module has no API.
    pub unread_module_file: Option<vfs::FileId>,
}

/// One discovered event subscription in a config root's *structure* listing: its
/// name and the [`vfs::FileId`] of its main XML. Event subscriptions are global
/// flat metadata objects, keyed by name, so they ride a dedicated listing field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EventSubscriptionEntry {
    pub name: String,
    pub main: vfs::FileId,
}

/// One discovered scheduled job in a config root's *structure* listing: its name
/// and the [`vfs::FileId`] of its main XML (`ScheduledJobs/<Name>.xml`). Scheduled
/// jobs are global flat metadata objects, keyed by name, so they ride a dedicated
/// listing field — the scheduled-job counterpart of [`EventSubscriptionEntry`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScheduledJobEntry {
    pub name: String,
    pub main: vfs::FileId,
}

/// One discovered role in a config root's *structure* listing: its name, main
/// XML (`Roles/<Name>.xml`), and optional rights sidecar
/// (`Roles/<Name>/Ext/Rights.xml`). Roles are flat metadata keyed by name, but
/// parsing needs both files when the sidecar exists.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RoleEntry {
    pub name: String,
    pub main: vfs::FileId,
    pub rights: Option<vfs::FileId>,
}

/// One discovered HTTP service in a config root's *structure* listing: its main
/// XML and optional module body (`HTTPServices/<Name>/Ext/Module.bsl`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HTTPServiceEntry {
    pub name: String,
    pub main: vfs::FileId,
    pub module_file: Option<vfs::FileId>,
}

/// One discovered Web service in a config root's *structure* listing: its main
/// XML and optional module body (`WebServices/<Name>/Ext/Module.bsl`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WebServiceEntry {
    pub name: String,
    pub main: vfs::FileId,
    pub module_file: Option<vfs::FileId>,
}

/// One discovered integration service in a config root's *structure* listing: its
/// main XML and optional module body (`IntegrationServices/<Name>/Ext/Module.bsl`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IntegrationServiceEntry {
    pub name: String,
    pub main: vfs::FileId,
    pub module_file: Option<vfs::FileId>,
}

/// One discovered subsystem in a config root's *structure* listing: its name and
/// the [`vfs::FileId`] of its main XML (`Subsystems/<Name>.xml`). Subsystems are
/// global flat metadata objects, keyed by name, so they ride a dedicated listing
/// field — the subsystem counterpart of [`ScheduledJobEntry`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SubsystemEntry {
    pub name: String,
    pub main: vfs::FileId,
}

/// One discovered common attribute in a config root's *structure* listing: its name and the
/// [`vfs::FileId`] of its XML (`CommonAttributes/<Name>.xml`). Common attributes are read as a
/// set per root ([`common_attribute_set`]) because each one may add a field to any object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommonAttributeEntry {
    pub name: String,
    pub main: vfs::FileId,
}

/// One config root's discovered metadata listing, grouped for the database setter
/// while keeping each metadata family typed separately.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MetadataListingData {
    pub entries: Vec<MdoEntry>,
    pub defined_types: Vec<DefinedTypeEntry>,
    pub common_modules: Vec<CommonModuleEntry>,
    pub event_subscriptions: Vec<EventSubscriptionEntry>,
    pub scheduled_jobs: Vec<ScheduledJobEntry>,
    pub roles: Vec<RoleEntry>,
    pub http_services: Vec<HTTPServiceEntry>,
    pub web_services: Vec<WebServiceEntry>,
    pub integration_services: Vec<IntegrationServiceEntry>,
    pub subsystems: Vec<SubsystemEntry>,
    pub common_attributes: Vec<CommonAttributeEntry>,
}

/// The per-config-root *structure* input: the MDOs and defined types that exist in
/// one config root (base config or one extension). Set out-of-query by the
/// bootstrap; re-set only when the directory structure changes. Keeping it per-root
/// means an extension's MDOs never collide with the base config's in
/// [`config_index`], and a structure change in one root does not invalidate
/// another. `entries` (MDOs + registers), `defined_types`, and `common_modules` are
/// separate fields, so a structure change to one family does not invalidate the
/// indexes derived from the others.
#[salsa::input(debug, heap_size = heap_estimate::metadata_listing_input_heap)]
pub struct MetadataListingInput {
    pub entries: Arc<Vec<MdoEntry>>,
    pub defined_types: Arc<Vec<DefinedTypeEntry>>,
    pub common_modules: Arc<Vec<CommonModuleEntry>>,
    pub event_subscriptions: Arc<Vec<EventSubscriptionEntry>>,
    pub scheduled_jobs: Arc<Vec<ScheduledJobEntry>>,
    pub roles: Arc<Vec<RoleEntry>>,
    pub http_services: Arc<Vec<HTTPServiceEntry>>,
    pub web_services: Arc<Vec<WebServiceEntry>>,
    pub integration_services: Arc<Vec<IntegrationServiceEntry>>,
    pub subsystems: Arc<Vec<SubsystemEntry>>,
    pub common_attributes: Arc<Vec<CommonAttributeEntry>>,
}

/// The composing-file identities for one MDO, as held in a [`ConfigIndex`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MdoFileIds {
    pub main: vfs::FileId,
    pub predefined: Option<vfs::FileId>,
}

/// A config root's `(kind, lowercased-name) -> files` lookup, derived from its
/// [`MetadataListingInput`]. Built by [`config_index`]; depends only on the
/// structure input, so a content edit leaves it memoised.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct ConfigIndex {
    by_name: FxHashMap<(bsl_metadata::MdoType, NormName), MdoFileIds>,
    /// Registers keyed by name alone (no kind), for callers that know only the
    /// register name (e.g. a `Движения.<Register>` movement touch). A register
    /// name is unique within a config root, so this carries its kind alongside.
    register_by_name: FxHashMap<NormName, (bsl_metadata::MdoType, MdoFileIds)>,
}

impl ConfigIndex {
    pub fn lookup(&self, kind: bsl_metadata::MdoType, name: &str) -> Option<MdoFileIds> {
        self.by_name.get(&(kind, NormName::intern(name))).copied()
    }

    pub fn lookup_register_by_name(
        &self,
        name: &str,
    ) -> Option<(bsl_metadata::MdoType, MdoFileIds)> {
        self.register_by_name.get(&NormName::intern(name)).copied()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Build a config root's name lookup from its structure listing. Tracked on the
/// listing input alone, so it re-runs only on a structure change (add/remove/
/// rename), not on a content edit — those flow through [`parse_mdo_query`].
#[salsa::tracked(heap_size = heap_estimate::config_index_heap, returns(ref))]
pub fn config_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<ConfigIndex> {
    let _span = tracing::info_span!("config_index").entered();

    let entries = listing.entries(db);
    let mut by_name: FxHashMap<_, _> =
        FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
    let mut register_by_name = FxHashMap::default();
    for entry in entries.iter() {
        let ids = MdoFileIds { main: entry.main, predefined: entry.predefined };
        by_name.insert((entry.kind, NormName::intern(&entry.name)), ids);
        if entry.kind.is_register() {
            register_by_name.insert(NormName::intern(&entry.name), (entry.kind, ids));
        }
    }
    Arc::new(ConfigIndex { by_name, register_by_name })
}

/// Every common attribute of one config root, read through the versioned VFS. Depends on the
/// listing's `common_attributes` family and on the content of those files only, so editing an
/// object's XML leaves it memoised. A file that does not parse marks the set unreadable.
#[salsa::tracked(heap_size = heap_estimate::common_attribute_set_heap, returns(clone))]
pub fn common_attribute_set(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<bsl_metadata::CommonAttributeSet> {
    let _span = tracing::info_span!("common_attribute_set").entered();

    let mut set = bsl_metadata::CommonAttributeSet::default();
    for entry in listing.common_attributes(db).iter() {
        let text = db.file_text_ref(entry.main);
        set.push_parsed(bsl_metadata::parse_common_attribute_from_text(text));
    }
    Arc::new(set)
}

/// Resolve a single metadata object within one config root, at per-MDO Salsa
/// granularity. Depends on [`config_index`] (structure) to map the name to its
/// files, then on [`parse_mdo_query`] (content) for that one MDO. A content edit
/// re-parses only the edited MDO and re-runs only this resolution for it; sibling
/// resolutions in the same root stay memoised. An add/remove re-runs
/// `config_index`, so an absent-name miss correctly invalidates when the MDO later
/// appears. Extension overlay across roots is composed by callers, not here.
#[salsa::tracked(heap_size = heap_estimate::shared_mdo_heap, returns(clone))]
pub fn resolve_metadata_object(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    mdo_type: bsl_metadata::MdoType,
    name: String,
) -> Option<Arc<bsl_metadata::MetadataObject>> {
    let _span = tracing::info_span!("resolve_metadata_object").entered();

    let index = config_index(db, listing);
    let ids = index.lookup(mdo_type, &name)?;
    let files = MdoFiles::new(db, mdo_type, ids.main, ids.predefined, listing);
    let object = parse_mdo_query(db, files)?;
    warn_on_stem_name_divergence(mdo_type.english_name(), &name, &object.name);
    Some(object)
}

/// Parse one register from its main XML, read through the versioned VFS. The
/// register counterpart of [`parse_mdo_query`]; keyed on the same interned
/// [`MdoFiles`] (registers have no predefined sidecar) but a separate tracked fn,
/// so a register and an object never share a memo. Backdates on an unchanged
/// register.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::register_heap, returns(clone))]
pub fn parse_register_query(
    db: &dyn base_db::SourceDatabase,
    files: MdoFiles<'_>,
) -> Option<Arc<bsl_metadata::Register>> {
    let _span = tracing::info_span!("parse_register").entered();

    let main_text = db.file_text_ref(files.main(db));
    let mut register = bsl_metadata::parse_register_from_text(files.mdo_type(db), main_text)?;
    let common = common_attribute_set(db, files.listing(db));
    let applied = common.for_object(register.mdo_type(), register.name());
    if !applied.is_empty() {
        register.set_common_attributes(applied);
    }
    Some(Arc::new(register))
}

/// Resolve a single register within one config root, the register counterpart of
/// [`resolve_metadata_object`]. Shares [`config_index`] (the listing carries
/// register entries too) but parses via [`parse_register_query`]. Extension
/// overlay across roots is composed by callers.
#[salsa::tracked(heap_size = heap_estimate::shared_register_heap, returns(clone))]
pub fn resolve_register(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    mdo_type: bsl_metadata::MdoType,
    name: String,
) -> Option<Arc<bsl_metadata::Register>> {
    let _span = tracing::info_span!("resolve_register").entered();

    let index = config_index(db, listing);
    let ids = index.lookup(mdo_type, &name)?;
    let files = MdoFiles::new(db, mdo_type, ids.main, ids.predefined, listing);
    let register = parse_register_query(db, files)?;
    warn_on_stem_name_divergence(mdo_type.english_name(), &name, register.name());
    Some(register)
}

/// Resolve a register within one config root by NAME alone (its kind is unknown
/// to the caller — e.g. a `Движения.<Register>` movement touch). Shares
/// [`config_index`]'s name-only register map, then parses the one register via
/// [`parse_register_query`]. Same per-MDO granularity and absent-name
/// invalidation as [`resolve_register`]; extension overlay across roots is
/// composed by callers.
#[salsa::tracked(heap_size = heap_estimate::shared_register_heap, returns(clone))]
pub fn resolve_register_by_name(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::Register>> {
    let _span = tracing::info_span!("resolve_register_by_name").entered();

    let index = config_index(db, listing);
    let (kind, ids) = index.lookup_register_by_name(&name)?;
    let files = MdoFiles::new(db, kind, ids.main, ids.predefined, listing);
    let register = parse_register_query(db, files)?;
    warn_on_stem_name_divergence(kind.english_name(), &name, register.name());
    Some(register)
}

/// The main XML file of a single defined type, interned so
/// [`parse_defined_type_query`] keys on the file identity; its content revision
/// drives invalidation, so editing one defined type re-parses only it.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct DefinedTypeFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

/// Parse one defined type from its main XML, read through the versioned VFS. The
/// defined-type counterpart of [`parse_register_query`]; keeps the parsed `<Name>`
/// so [`resolve_defined_type`] can flag a stem-vs-`<Name>` divergence, then projects
/// to the underlying type (the resolution unit; an extension overlay replaces it
/// wholesale). Backdates on an unchanged defined type.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::defined_type_heap, returns(clone))]
pub fn parse_defined_type_query(
    db: &dyn base_db::SourceDatabase,
    file: DefinedTypeFile<'_>,
) -> Option<Arc<bsl_metadata::DefinedType>> {
    let _span = tracing::info_span!("parse_defined_type").entered();

    let main_text = db.file_text_ref(file.main(db));
    bsl_metadata::parse_defined_type_from_text(main_text).map(Arc::new)
}

/// A config root's `lowercased-name -> defined-type file` lookup, derived from its
/// [`MetadataListingInput`]'s `defined_types` field. Tracked on that field alone,
/// so a content edit leaves it memoised and an MDO structure change does not
/// invalidate it.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct DefinedTypeIndex {
    by_name: FxHashMap<NormName, vfs::FileId>,
}

impl DefinedTypeIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&NormName::intern(name)).copied()
    }
}

/// Build a config root's defined-type name lookup from its structure listing.
#[salsa::tracked(heap_size = heap_estimate::defined_type_index_heap, returns(ref))]
pub fn defined_type_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<DefinedTypeIndex> {
    let _span = tracing::info_span!("defined_type_index").entered();

    let entries = listing.defined_types(db);
    let mut by_name: FxHashMap<_, _> =
        FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
    for entry in entries.iter() {
        by_name.insert(NormName::intern(&entry.name), entry.main);
    }
    Arc::new(DefinedTypeIndex { by_name })
}

/// Resolve a single defined type's underlying type within one config root, at
/// per-defined-type Salsa granularity. The defined-type counterpart of
/// [`resolve_metadata_object`]; extension overlay across roots is composed by
/// callers (an extension replaces the underlying type wholesale).
#[salsa::tracked(heap_size = heap_estimate::defined_type_projection_heap, returns(clone))]
pub fn resolve_defined_type(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::AttributeType>> {
    let _span = tracing::info_span!("resolve_defined_type").entered();

    let index = defined_type_index(db, listing);
    let main = index.lookup(&name)?;
    let file = DefinedTypeFile::new(db, main);
    let defined_type = parse_defined_type_query(db, file)?;
    warn_on_stem_name_divergence("DefinedType", &name, defined_type.name());
    Some(Arc::new(defined_type.underlying_type().clone()))
}

/// The main XML file of a single common module, interned so
/// [`parse_common_module_query`] keys on the file identity; its content revision
/// drives invalidation, so editing one common module re-parses only it.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct CommonModuleFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

/// Parse one common module's metadata from its main XML, read through the versioned
/// VFS. The common-module counterpart of [`parse_defined_type_query`]; only metadata
/// (flags + name) is read — the module body is resolved through the symbol tree.
/// Backdates on unchanged metadata.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::common_module_heap, returns(clone))]
pub fn parse_common_module_query(
    db: &dyn base_db::SourceDatabase,
    file: CommonModuleFile<'_>,
) -> Option<Arc<bsl_metadata::CommonModule>> {
    let _span = tracing::info_span!("parse_common_module").entered();

    let main_text = db.file_text_ref(file.main(db));
    bsl_metadata::parse_common_module_from_text(main_text).map(Arc::new)
}

/// A config root's common-module lookup, derived from its [`MetadataListingInput`]'s
/// `common_modules` field: `lowercased-name -> main XML` for the by-name resolution,
/// and `module-file id -> name` for the reverse "which common module owns this
/// `.bsl`" lookup. Tracked on that field alone, so a content edit leaves it memoised.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct CommonModuleIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
    by_module_file: std::collections::HashMap<vfs::FileId, String>,
    /// `lowercased-name -> Ext/Module.bsl id`, for resolving a common module's
    /// body file scoped to this root (method/parameter validation needs the body,
    /// not the metadata XML). Absent for modules whose body was not enrolled.
    module_file_by_name: std::collections::HashMap<String, vfs::FileId>,
    /// `lowercased-name -> Ext/Module.bsl id` for bodies that exist but could not
    /// be read. Disjoint from `module_file_by_name`: a body is in one or neither.
    unread_module_file_by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl CommonModuleIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }

    /// The `Ext/Module.bsl` id of the common module `name` in this root, if known.
    pub fn lookup_module_file(&self, name: &str) -> Option<vfs::FileId> {
        self.module_file_by_name.get(&name.fold_lower()).copied()
    }

    /// The `Ext/Module.bsl` id of the common module `name` in this root when that
    /// body exists but could not be read.
    pub fn lookup_unread_module_file(&self, name: &str) -> Option<vfs::FileId> {
        self.unread_module_file_by_name.get(&name.fold_lower()).copied()
    }

    /// The lowercased name of the common module whose `Ext/Module.bsl` is
    /// `module_file`, if any.
    pub fn name_for_module_file(&self, module_file: vfs::FileId) -> Option<&str> {
        self.by_module_file.get(&module_file).map(String::as_str)
    }
}

/// Build a config root's common-module lookup from its structure listing.
#[salsa::tracked(heap_size = heap_estimate::common_module_index_heap, returns(ref))]
pub fn common_module_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<CommonModuleIndex> {
    let _span = tracing::info_span!("common_module_index").entered();

    let entries = listing.common_modules(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    let mut by_module_file = std::collections::HashMap::new();
    let mut module_file_by_name = std::collections::HashMap::new();
    let mut unread_module_file_by_name = std::collections::HashMap::new();
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
        if let Some(module_file) = entry.module_file {
            by_module_file.insert(module_file, entry.name.fold_lower());
            module_file_by_name.insert(entry.name.fold_lower(), module_file);
        }
        if let Some(unread) = entry.unread_module_file {
            unread_module_file_by_name.insert(entry.name.fold_lower(), unread);
        }
    }
    Arc::new(CommonModuleIndex {
        by_name,
        by_module_file,
        module_file_by_name,
        unread_module_file_by_name,
    })
}

/// Resolve a single common module's metadata by name within one config root, at
/// per-common-module Salsa granularity. The common-module counterpart of
/// [`resolve_defined_type`]; extension overlay across roots is composed by callers
/// (a borrowed module inherits the properties absent in its XML from the base).
#[salsa::tracked(heap_size = heap_estimate::shared_common_module_heap, returns(clone))]
pub fn resolve_common_module(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::CommonModule>> {
    let _span = tracing::info_span!("resolve_common_module").entered();

    use bsl_metadata::traits::MdObject;
    let index = common_module_index(db, listing);
    let main = index.lookup(&name)?;
    let file = CommonModuleFile::new(db, main);
    let module = parse_common_module_query(db, file)?;
    warn_on_stem_name_divergence("CommonModule", &name, module.name());
    Some(module)
}

/// Resolve the common module whose `Ext/Module.bsl` is `module_file` within one
/// config root. Answers "which common module owns this `.bsl`?" via the reverse
/// index, then parses that module's metadata at per-common-module granularity.
#[salsa::tracked(heap_size = heap_estimate::shared_common_module_heap, returns(clone))]
pub fn resolve_common_module_by_file(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    module_file: vfs::FileId,
) -> Option<Arc<bsl_metadata::CommonModule>> {
    let _span = tracing::info_span!("resolve_common_module_by_file").entered();

    let index = common_module_index(db, listing);
    let name = index.name_for_module_file(module_file)?;
    let main = index.lookup(name)?;
    let file = CommonModuleFile::new(db, main);
    parse_common_module_query(db, file)
}

fn service_name_from_main_file(db: &dyn crate::RootDatabase, main: vfs::FileId) -> String {
    crate::vfs_helpers::get_file_path(db, main)
        .and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct HTTPServiceFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

#[salsa::tracked(lru = 8192, heap_size = heap_estimate::http_service_heap, returns(clone))]
pub fn parse_http_service_query(
    db: &dyn crate::RootDatabase,
    file: HTTPServiceFile<'_>,
) -> Option<Arc<bsl_metadata::HTTPService>> {
    let _span = tracing::info_span!("parse_http_service").entered();

    let main = file.main(db);
    let main_text = db.file_text_ref(main);
    let name = service_name_from_main_file(db, main);
    bsl_metadata::parse_http_service_from_text(main_text, &name).map(Arc::new)
}

#[derive(Default, PartialEq, Eq, Debug)]
pub struct HTTPServiceIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
    by_module_file: std::collections::HashMap<vfs::FileId, String>,
    module_file_by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl HTTPServiceIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }

    pub fn lookup_module_file(&self, name: &str) -> Option<vfs::FileId> {
        self.module_file_by_name.get(&name.fold_lower()).copied()
    }

    pub fn name_for_module_file(&self, module_file: vfs::FileId) -> Option<&str> {
        self.by_module_file.get(&module_file).map(String::as_str)
    }
}

#[salsa::tracked(heap_size = heap_estimate::http_service_index_heap, returns(ref))]
pub fn http_service_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<HTTPServiceIndex> {
    let _span = tracing::info_span!("http_service_index").entered();

    let entries = listing.http_services(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    let mut by_module_file = std::collections::HashMap::new();
    let mut module_file_by_name = std::collections::HashMap::new();
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
        if let Some(module_file) = entry.module_file {
            by_module_file.insert(module_file, entry.name.fold_lower());
            module_file_by_name.insert(entry.name.fold_lower(), module_file);
        }
    }
    Arc::new(HTTPServiceIndex { by_name, by_module_file, module_file_by_name })
}

#[salsa::tracked(heap_size = heap_estimate::shared_http_service_heap, returns(clone))]
pub fn resolve_http_service(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::HTTPService>> {
    let _span = tracing::info_span!("resolve_http_service").entered();

    let index = http_service_index(db, listing);
    let main = index.lookup(&name)?;
    let file = HTTPServiceFile::new(db, main);
    parse_http_service_query(db, file)
}

#[salsa::tracked(heap_size = heap_estimate::shared_http_service_heap, returns(clone))]
pub fn resolve_http_service_by_file(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    module_file: vfs::FileId,
) -> Option<Arc<bsl_metadata::HTTPService>> {
    let _span = tracing::info_span!("resolve_http_service_by_file").entered();

    let index = http_service_index(db, listing);
    let name = index.name_for_module_file(module_file)?;
    let main = index.lookup(name)?;
    let file = HTTPServiceFile::new(db, main);
    parse_http_service_query(db, file)
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct WebServiceFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

#[salsa::tracked(lru = 8192, heap_size = heap_estimate::web_service_heap, returns(clone))]
pub fn parse_web_service_query(
    db: &dyn crate::RootDatabase,
    file: WebServiceFile<'_>,
) -> Option<Arc<bsl_metadata::WebService>> {
    let _span = tracing::info_span!("parse_web_service").entered();

    let main = file.main(db);
    let main_text = db.file_text_ref(main);
    let name = service_name_from_main_file(db, main);
    bsl_metadata::parse_web_service_from_text(main_text, &name).map(Arc::new)
}

#[derive(Default, PartialEq, Eq, Debug)]
pub struct WebServiceIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
    by_module_file: std::collections::HashMap<vfs::FileId, String>,
    module_file_by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl WebServiceIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }

    pub fn lookup_module_file(&self, name: &str) -> Option<vfs::FileId> {
        self.module_file_by_name.get(&name.fold_lower()).copied()
    }

    pub fn name_for_module_file(&self, module_file: vfs::FileId) -> Option<&str> {
        self.by_module_file.get(&module_file).map(String::as_str)
    }
}

#[salsa::tracked(heap_size = heap_estimate::web_service_index_heap, returns(ref))]
pub fn web_service_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<WebServiceIndex> {
    let _span = tracing::info_span!("web_service_index").entered();

    let entries = listing.web_services(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    let mut by_module_file = std::collections::HashMap::new();
    let mut module_file_by_name = std::collections::HashMap::new();
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
        if let Some(module_file) = entry.module_file {
            by_module_file.insert(module_file, entry.name.fold_lower());
            module_file_by_name.insert(entry.name.fold_lower(), module_file);
        }
    }
    Arc::new(WebServiceIndex { by_name, by_module_file, module_file_by_name })
}

#[salsa::tracked(heap_size = heap_estimate::shared_web_service_heap, returns(clone))]
pub fn resolve_web_service(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::WebService>> {
    let _span = tracing::info_span!("resolve_web_service").entered();

    let index = web_service_index(db, listing);
    let main = index.lookup(&name)?;
    let file = WebServiceFile::new(db, main);
    parse_web_service_query(db, file)
}

#[salsa::tracked(heap_size = heap_estimate::shared_web_service_heap, returns(clone))]
pub fn resolve_web_service_by_file(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    module_file: vfs::FileId,
) -> Option<Arc<bsl_metadata::WebService>> {
    let _span = tracing::info_span!("resolve_web_service_by_file").entered();

    let index = web_service_index(db, listing);
    let name = index.name_for_module_file(module_file)?;
    let main = index.lookup(name)?;
    let file = WebServiceFile::new(db, main);
    parse_web_service_query(db, file)
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct IntegrationServiceFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

#[salsa::tracked(lru = 8192, heap_size = heap_estimate::integration_service_heap, returns(clone))]
pub fn parse_integration_service_query(
    db: &dyn crate::RootDatabase,
    file: IntegrationServiceFile<'_>,
) -> Option<Arc<bsl_metadata::IntegrationService>> {
    let _span = tracing::info_span!("parse_integration_service").entered();

    let main = file.main(db);
    let main_text = db.file_text_ref(main);
    let name = service_name_from_main_file(db, main);
    bsl_metadata::parse_integration_service_from_text(main_text, &name).map(Arc::new)
}

#[derive(Default, PartialEq, Eq, Debug)]
pub struct IntegrationServiceIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
    by_module_file: std::collections::HashMap<vfs::FileId, String>,
    module_file_by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl IntegrationServiceIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }

    pub fn lookup_module_file(&self, name: &str) -> Option<vfs::FileId> {
        self.module_file_by_name.get(&name.fold_lower()).copied()
    }

    pub fn name_for_module_file(&self, module_file: vfs::FileId) -> Option<&str> {
        self.by_module_file.get(&module_file).map(String::as_str)
    }
}

#[salsa::tracked(heap_size = heap_estimate::integration_service_index_heap, returns(ref))]
pub fn integration_service_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<IntegrationServiceIndex> {
    let _span = tracing::info_span!("integration_service_index").entered();

    let entries = listing.integration_services(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    let mut by_module_file = std::collections::HashMap::new();
    let mut module_file_by_name = std::collections::HashMap::new();
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
        if let Some(module_file) = entry.module_file {
            by_module_file.insert(module_file, entry.name.fold_lower());
            module_file_by_name.insert(entry.name.fold_lower(), module_file);
        }
    }
    Arc::new(IntegrationServiceIndex { by_name, by_module_file, module_file_by_name })
}

#[salsa::tracked(heap_size = heap_estimate::shared_integration_service_heap, returns(clone))]
pub fn resolve_integration_service(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::IntegrationService>> {
    let _span = tracing::info_span!("resolve_integration_service").entered();

    let index = integration_service_index(db, listing);
    let main = index.lookup(&name)?;
    let file = IntegrationServiceFile::new(db, main);
    parse_integration_service_query(db, file)
}

#[salsa::tracked(heap_size = heap_estimate::shared_integration_service_heap, returns(clone))]
pub fn resolve_integration_service_by_file(
    db: &dyn crate::RootDatabase,
    listing: MetadataListingInput,
    module_file: vfs::FileId,
) -> Option<Arc<bsl_metadata::IntegrationService>> {
    let _span = tracing::info_span!("resolve_integration_service_by_file").entered();

    let index = integration_service_index(db, listing);
    let name = index.name_for_module_file(module_file)?;
    let main = index.lookup(name)?;
    let file = IntegrationServiceFile::new(db, main);
    parse_integration_service_query(db, file)
}

/// The main XML file of a single event subscription, interned so
/// [`parse_event_subscription_query`] keys on the file identity; its content
/// revision drives invalidation, so editing one subscription re-parses only it.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct EventSubscriptionFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

/// Parse one event subscription from its main XML, read through the versioned VFS.
/// Backdates on unchanged metadata.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::event_subscription_heap, returns(clone))]
pub fn parse_event_subscription_query(
    db: &dyn base_db::SourceDatabase,
    file: EventSubscriptionFile<'_>,
) -> Option<Arc<bsl_metadata::EventSubscription>> {
    let _span = tracing::info_span!("parse_event_subscription").entered();

    let main_text = db.file_text_ref(file.main(db));
    bsl_metadata::parse_event_subscription_from_text(main_text).map(Arc::new)
}

/// A config root's event-subscription lookup, derived from its
/// [`MetadataListingInput`]'s `event_subscriptions` field. Tracked on that field
/// alone, so a content edit leaves it memoised and unrelated MDO structure changes
/// do not invalidate it.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct EventSubscriptionIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl EventSubscriptionIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }
}

/// Build a config root's event-subscription name lookup from its structure listing.
#[salsa::tracked(heap_size = heap_estimate::event_subscription_index_heap, returns(ref))]
pub fn event_subscription_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<EventSubscriptionIndex> {
    let _span = tracing::info_span!("event_subscription_index").entered();

    let entries = listing.event_subscriptions(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
    }
    Arc::new(EventSubscriptionIndex { by_name })
}

/// Resolve a single event subscription's metadata by name within one config root,
/// at per-event-subscription Salsa granularity. Extension overlay across roots is
/// composed by callers (an extension replaces the subscription wholesale).
#[salsa::tracked(heap_size = heap_estimate::shared_event_subscription_heap, returns(clone))]
pub fn resolve_event_subscription(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::EventSubscription>> {
    let _span = tracing::info_span!("resolve_event_subscription").entered();

    let index = event_subscription_index(db, listing);
    let main = index.lookup(&name)?;
    let file = EventSubscriptionFile::new(db, main);
    let subscription = parse_event_subscription_query(db, file)?;
    warn_on_stem_name_divergence("EventSubscription", &name, subscription.name());
    Some(subscription)
}

/// The main XML file of a single scheduled job, interned so
/// [`parse_scheduled_job_query`] keys on the file identity; its content revision
/// drives invalidation, so editing one scheduled job re-parses only it.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct ScheduledJobFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

/// Parse one scheduled job from its main XML, read through the versioned VFS. The
/// scheduled-job counterpart of [`parse_event_subscription_query`]; backdates on
/// unchanged metadata.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::scheduled_job_heap, returns(clone))]
pub fn parse_scheduled_job_query(
    db: &dyn base_db::SourceDatabase,
    file: ScheduledJobFile<'_>,
) -> Option<Arc<bsl_metadata::ScheduledJob>> {
    let _span = tracing::info_span!("parse_scheduled_job").entered();

    let main_text = db.file_text_ref(file.main(db));
    bsl_metadata::parse_scheduled_job_from_text(main_text).map(Arc::new)
}

/// A config root's scheduled-job lookup, derived from its
/// [`MetadataListingInput`]'s `scheduled_jobs` field. Tracked on that field alone,
/// so a content edit leaves it memoised and unrelated MDO structure changes do
/// not invalidate it.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct ScheduledJobIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl ScheduledJobIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }
}

/// Build a config root's scheduled-job name lookup from its structure listing.
#[salsa::tracked(heap_size = heap_estimate::scheduled_job_index_heap, returns(ref))]
pub fn scheduled_job_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<ScheduledJobIndex> {
    let _span = tracing::info_span!("scheduled_job_index").entered();

    let entries = listing.scheduled_jobs(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
    }
    Arc::new(ScheduledJobIndex { by_name })
}

/// Resolve a single scheduled job's metadata by name within one config root, at
/// per-scheduled-job Salsa granularity. The scheduled-job counterpart of
/// [`resolve_event_subscription`]; extension overlay across roots is composed by
/// callers (an extension replaces the job wholesale).
#[salsa::tracked(heap_size = heap_estimate::shared_scheduled_job_heap, returns(clone))]
pub fn resolve_scheduled_job(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::ScheduledJob>> {
    let _span = tracing::info_span!("resolve_scheduled_job").entered();

    let index = scheduled_job_index(db, listing);
    let main = index.lookup(&name)?;
    let file = ScheduledJobFile::new(db, main);
    let job = parse_scheduled_job_query(db, file)?;
    warn_on_stem_name_divergence("ScheduledJob", &name, job.name());
    Some(job)
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct RoleFiles<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
    #[returns(copy)]
    pub rights: Option<vfs::FileId>,
}

#[salsa::tracked(lru = 8192, heap_size = heap_estimate::role_heap, returns(clone))]
pub fn parse_role_query(
    db: &dyn base_db::SourceDatabase,
    files: RoleFiles<'_>,
) -> Option<Arc<bsl_metadata::Role>> {
    let _span = tracing::info_span!("parse_role").entered();

    let main_text = db.file_text_ref(files.main(db));
    let rights_text = files.rights(db).map(|fid| db.file_text_ref(fid));
    bsl_metadata::parse_role_from_texts(main_text, rights_text.map(|t| &**t)).map(Arc::new)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoleFileIds {
    pub main: vfs::FileId,
    pub rights: Option<vfs::FileId>,
}

#[derive(Default, PartialEq, Eq, Debug)]
pub struct RoleIndex {
    by_name: std::collections::HashMap<String, RoleFileIds>,
}

impl RoleIndex {
    pub fn lookup(&self, name: &str) -> Option<RoleFileIds> {
        self.by_name.get(&name.fold_lower()).copied()
    }
}

#[salsa::tracked(heap_size = heap_estimate::role_index_heap, returns(ref))]
pub fn role_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<RoleIndex> {
    let _span = tracing::info_span!("role_index").entered();

    let entries = listing.roles(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    for entry in entries.iter() {
        by_name.insert(
            entry.name.fold_lower(),
            RoleFileIds { main: entry.main, rights: entry.rights },
        );
    }
    Arc::new(RoleIndex { by_name })
}

#[salsa::tracked(heap_size = heap_estimate::shared_role_heap, returns(clone))]
pub fn resolve_role(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::Role>> {
    let _span = tracing::info_span!("resolve_role").entered();

    let index = role_index(db, listing);
    let ids = index.lookup(&name)?;
    let files = RoleFiles::new(db, ids.main, ids.rights);
    let role = parse_role_query(db, files)?;
    warn_on_stem_name_divergence("Role", &name, role.name());
    Some(role)
}

/// The main XML file of a single subsystem, interned so
/// [`parse_subsystem_query`] keys on the file identity; its content revision
/// drives invalidation, so editing one subsystem re-parses only it. The
/// subsystem counterpart of [`ScheduledJobFile`].
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct SubsystemFile<'db> {
    #[returns(copy)]
    pub main: vfs::FileId,
}

/// Parse one subsystem from its main XML, read through the versioned VFS. The
/// subsystem counterpart of [`parse_scheduled_job_query`]; backdates on
/// unchanged metadata.
#[salsa::tracked(lru = 8192, heap_size = heap_estimate::subsystem_heap, returns(clone))]
pub fn parse_subsystem_query(
    db: &dyn base_db::SourceDatabase,
    file: SubsystemFile<'_>,
) -> Option<Arc<bsl_metadata::Subsystem>> {
    let _span = tracing::info_span!("parse_subsystem").entered();

    let main_text = db.file_text_ref(file.main(db));
    bsl_metadata::parse_subsystem_from_text(main_text).map(Arc::new)
}

/// A config root's subsystem lookup, derived from its [`MetadataListingInput`]'s
/// `subsystems` field. Tracked on that field alone, so a content edit leaves it
/// memoised and unrelated MDO structure changes do not invalidate it. The
/// subsystem counterpart of [`ScheduledJobIndex`].
#[derive(Default, PartialEq, Eq, Debug)]
pub struct SubsystemIndex {
    by_name: std::collections::HashMap<String, vfs::FileId>,
}

impl SubsystemIndex {
    pub fn lookup(&self, name: &str) -> Option<vfs::FileId> {
        self.by_name.get(&name.fold_lower()).copied()
    }
}

/// Build a config root's subsystem name lookup from its structure listing.
#[salsa::tracked(heap_size = heap_estimate::subsystem_index_heap, returns(ref))]
pub fn subsystem_index(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
) -> Arc<SubsystemIndex> {
    let _span = tracing::info_span!("subsystem_index").entered();

    let entries = listing.subsystems(db);
    let mut by_name = std::collections::HashMap::with_capacity(entries.len());
    for entry in entries.iter() {
        by_name.insert(entry.name.fold_lower(), entry.main);
    }
    Arc::new(SubsystemIndex { by_name })
}

/// Resolve a single subsystem's metadata by name within one config root, at
/// per-subsystem Salsa granularity. The subsystem counterpart of
/// [`resolve_scheduled_job`]; extension overlay across roots is composed by
/// callers (an extension adds to a same-name base via
/// [`bsl_metadata::Subsystem::merge_from`]).
#[salsa::tracked(heap_size = heap_estimate::shared_subsystem_heap, returns(clone))]
pub fn resolve_subsystem(
    db: &dyn base_db::SourceDatabase,
    listing: MetadataListingInput,
    name: String,
) -> Option<Arc<bsl_metadata::Subsystem>> {
    let _span = tracing::info_span!("resolve_subsystem").entered();

    let index = subsystem_index(db, listing);
    let main = index.lookup(&name)?;
    let file = SubsystemFile::new(db, main);
    let subsystem = parse_subsystem_query(db, file)?;
    warn_on_stem_name_divergence("Subsystem", &name, subsystem.name());
    Some(subsystem)
}

#[salsa::db]
pub trait MetadataDb: salsa::Database {
    /// The ONE entry point for whole-config loads. Required (no default) and
    /// object-safe on purpose: every reader — including queries that only hold
    /// `&dyn MetadataDb` (`merged_configuration`) or a supertrait object (the
    /// resolver provider) — must dispatch through the implementor, because an
    /// implementor may interpose a cross-database cache (the graph build's
    /// `GraphConfigCache`). A path that calls the free [`load_configuration`]
    /// query directly bypasses that cache and re-runs the internally-parallel
    /// XML load on every fresh batch database — inside the build's worker pool,
    /// where its nested `rayon::scope` can deadlock the build.
    fn load_configuration<'db>(
        &'db self,
        path_input: ConfigurationPathInput<'db>,
    ) -> Arc<Configuration>;
}

pub fn get_module_type_from_uri(file_uri: &str) -> Option<bsl_metadata::ModuleType> {
    // On Windows the LSP feeds native paths (`C:\…\Forms\F\Ext\Form\Module.bsl`);
    // segment matching below keys on `/`, so a backslash path would collapse into
    // one segment and misclassify every module as Unknown — e.g. a form module
    // would lose its metadata and form-self members (`Команды`, `Элементы`) would
    // fail to resolve. Normalize separators the same way the `find_*_by_path`
    // helpers do.
    let normalized = file_uri.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();

    if parts.is_empty() {
        return None;
    }

    use bsl_conventions::{conventional_of, ConventionalName as Conv};
    let last_kind = conventional_of(parts[parts.len() - 1]);
    if parts.len() >= 2 && conventional_of(parts[parts.len() - 2]) == Some(Conv::Ext) {
        match last_kind {
            Some(Conv::ManagedApplicationModule) => {
                return Some(bsl_metadata::ModuleType::ManagedApplicationModule);
            }
            Some(Conv::OrdinaryApplicationModule) => {
                return Some(bsl_metadata::ModuleType::OrdinaryApplicationModule);
            }
            Some(Conv::ApplicationModule) => {
                return Some(bsl_metadata::ModuleType::ApplicationModule);
            }
            Some(Conv::SessionModule) => return Some(bsl_metadata::ModuleType::SessionModule),
            Some(Conv::ExternalConnectionModule) => {
                return Some(bsl_metadata::ModuleType::ExternalConnectionModule);
            }
            _ => {}
        }
    }

    let form_suffix = |parts: &[&str]| {
        conventional_of(parts[parts.len() - 1]) == Some(Conv::Module)
            && conventional_of(parts[parts.len() - 2]) == Some(Conv::Form)
            && conventional_of(parts[parts.len() - 3]) == Some(Conv::Ext)
    };
    if let Some(idx) = parts
        .iter()
        .rposition(|&p| p.eq_ignore_ascii_case("CommonForms") || p.fold_lower() == "общиеформы")
    {
        if parts.len() == idx + 5 && form_suffix(&parts) {
            return Some(bsl_metadata::ModuleType::FormModule);
        }
    }

    // Позиция структурная, как у зеркала в hir-def: `Forms` стоит ровно перед
    // именем формы (`…/Forms/<Форма>/Ext/Form/Module.bsl`), иначе посторонний
    // каталог `forms` выше по дереву объявил бы форму, которой нет.
    if parts.len() >= 7
        && conventional_of(parts[parts.len() - 5]) == Some(Conv::Forms)
        && form_suffix(&parts)
    {
        return Some(bsl_metadata::ModuleType::FormModule);
    }

    // Everything else lives in a collection, and the shape of that path has exactly
    // one specification. Deciding by "does some ancestor look like a collection"
    // let a directory far up the path take the type, and deciding by segment count
    // alone gave an ordinary file a module type it has no claim to.
    if let Some(split) = bsl_metadata::module_path::split_module_path(&normalized, |segment| {
        module_collection(segment).is_some()
    }) {
        if let Some(collection) = module_collection(split.collection) {
            return collection_module_type(collection, split.module_file);
        }
    }

    // A dump also holds collections this crate has no `MdoType` for — settings
    // storages, filter criteria, document journals. Their object/manager modules are
    // still object/manager modules, and the file name says so; what the unknown
    // collection costs is the shape evidence, so the SERVICE level has to supply it.
    // Without either the path is just a file that happens to be named `ObjectModule.bsl`.
    let has_service_level =
        parts.len() >= 2 && conventional_of(parts[parts.len() - 2]) == Some(Conv::Ext);
    if has_service_level && parts.len() >= 4 {
        return match last_kind {
            Some(Conv::ObjectModule) => Some(bsl_metadata::ModuleType::ObjectModule),
            Some(Conv::ManagerModule) => Some(bsl_metadata::ModuleType::ManagerModule),
            // A record set too: four of its eight owners — sequences,
            // recalculations, an external data source's tables and cubes — are not
            // in `MdoType`, and this branch is where their paths land.
            Some(Conv::RecordSetModule) => Some(bsl_metadata::ModuleType::RecordSetModule),
            _ => None,
        };
    }

    None
}

/// A dump directory that holds modules of one kind. The module type follows from
/// the collection and the file name TOGETHER — `Module.bsl` means a common module
/// under `CommonModules` and an HTTP service under `HTTPServices`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleCollection {
    Mdo(bsl_metadata::MdoType),
    CommonModules,
    HttpServices,
    WebServices,
    IntegrationServices,
    CommonCommands,
    Commands,
}

fn module_collection(segment: &str) -> Option<ModuleCollection> {
    // Which spellings name a collection is decided once, in
    // `bsl_metadata::module_path::collection_directory` — a directory name is not a
    // name written in code, and the two layers that read paths must not answer that
    // question apart.
    let folded = segment.fold_lower();
    match folded.as_str() {
        "httpservices" => Some(ModuleCollection::HttpServices),
        "webservices" => Some(ModuleCollection::WebServices),
        "integrationservices" => Some(ModuleCollection::IntegrationServices),
        "commoncommands" | "общиекоманды" => Some(ModuleCollection::CommonCommands),
        "commands" => Some(ModuleCollection::Commands),
        // Common modules answer to both spellings, as the module index does — an
        // English-only branch left the Russian directory without a module type at all.
        _ => match bsl_metadata::module_path::collection_directory(segment) {
            Some(bsl_metadata::MdoType::CommonModule) => Some(ModuleCollection::CommonModules),
            Some(mdo) if mdo.manager_type_prefix().is_some() => Some(ModuleCollection::Mdo(mdo)),
            _ => None,
        },
    }
}

fn collection_module_type(
    collection: ModuleCollection,
    module_file: &str,
) -> Option<bsl_metadata::ModuleType> {
    use bsl_metadata::ModuleType;
    // A collection holds one kind of object, but not one FILE: the dump puts a
    // command module next to an object module. The file name has to be named
    // explicitly everywhere, or a sibling `.bsl` inherits a type it has no claim to.
    use bsl_conventions::{conventional_of, ConventionalName as Conv};
    let file_kind = conventional_of(module_file);
    match collection {
        ModuleCollection::CommonModules if file_kind == Some(Conv::Module) => {
            Some(ModuleType::CommonModule)
        }
        ModuleCollection::HttpServices if file_kind == Some(Conv::Module) => {
            Some(ModuleType::HTTPServiceModule)
        }
        ModuleCollection::WebServices if file_kind == Some(Conv::Module) => {
            Some(ModuleType::WebServiceModule)
        }
        ModuleCollection::IntegrationServices if file_kind == Some(Conv::Module) => {
            Some(ModuleType::IntegrationServiceModule)
        }
        ModuleCollection::CommonCommands if file_kind == Some(Conv::CommandModule) => {
            Some(ModuleType::CommandModule)
        }
        ModuleCollection::Commands if file_kind == Some(Conv::CommandModule) => {
            Some(ModuleType::CommandModule)
        }
        // A constant owns a module no other collection has.
        ModuleCollection::Mdo(bsl_metadata::MdoType::Constant)
            if file_kind == Some(Conv::ValueManagerModule) =>
        {
            Some(ModuleType::ValueManagerModule)
        }
        ModuleCollection::Mdo(mdo) => match file_kind {
            Some(Conv::ObjectModule) => Some(ModuleType::ObjectModule),
            Some(Conv::ManagerModule) => Some(ModuleType::ManagerModule),
            // A record set belongs to a register and to nothing else — the same
            // list `build_module_metadata` uses to load a register owner.
            Some(Conv::RecordSetModule) if mdo.is_register() => Some(ModuleType::RecordSetModule),
            _ => None,
        },
        ModuleCollection::CommonModules
        | ModuleCollection::HttpServices
        | ModuleCollection::WebServices
        | ModuleCollection::IntegrationServices
        | ModuleCollection::CommonCommands
        | ModuleCollection::Commands => None,
    }
}

#[derive(Debug, Clone)]
pub struct ModulePathInfo {
    pub mdo_type: Option<bsl_metadata::MdoType>,
    pub name: Option<String>,
    pub module_type: bsl_metadata::ModuleType,
}

pub fn parse_module_path(file_uri: &str) -> Option<ModulePathInfo> {
    // Normalize Windows separators before `/`-keyed segment matching (see
    // `get_module_type_from_uri`).
    let normalized = file_uri.replace('\\', "/");

    // Only the collections this function promises an owner for: an mdo object, or a
    // common module. The service collections (`HTTPServices`, …) have an owner of
    // their own kind and their callers resolve it elsewhere.
    let split = bsl_metadata::module_path::split_module_path(&normalized, |segment| {
        matches!(
            module_collection(segment),
            Some(ModuleCollection::Mdo(_) | ModuleCollection::CommonModules)
        )
    })?;
    let collection = module_collection(split.collection)?;

    let mdo_type = match collection {
        ModuleCollection::Mdo(mdo) => Some(mdo),
        _ => None,
    };
    // The module type comes from the same table `get_module_type_from_uri` reads, so
    // the two answers about one path cannot drift apart.
    let module_type = collection_module_type(collection, split.module_file)
        .unwrap_or(bsl_metadata::ModuleType::Unknown);

    Some(ModulePathInfo { mdo_type, name: Some(split.object_name.to_string()), module_type })
}

pub fn find_metadata_object<DB: MetadataDb>(
    db: &DB,
    path_input: ConfigurationPathInput,
    mdo_type: bsl_metadata::MdoType,
    name: &str,
) -> Option<bsl_metadata::MetadataObject> {
    let config = db.load_configuration(path_input);

    if let Some(mdo) =
        config.metadata_objects().iter().find(|mdo| mdo.mdo_type == mdo_type && mdo.name == name)
    {
        return Some(mdo.clone());
    }

    use bsl_metadata::MdoType;
    if matches!(
        mdo_type,
        MdoType::InformationRegister
            | MdoType::AccumulationRegister
            | MdoType::AccountingRegister
            | MdoType::CalculationRegister
    ) {
        #[allow(unused_imports)]
        use bsl_metadata::traits::MdObject;
        config
            .registers()
            .iter()
            .find(|reg| reg.mdo_type() == mdo_type && reg.name() == name)
            .map(|reg| bsl_metadata::MetadataObject::new(mdo_type, reg.name()))
    } else {
        None
    }
}

pub(crate) fn find_common_module_by_path(
    configuration: &bsl_metadata::Configuration,
    file_path: &Path,
) -> Option<bsl_metadata::CommonModule> {
    // The configuration stores each common module's `uri` relative to the config root
    // (`CommonModules/<Имя>/Ext/Module.bsl`), while the analyzed `file_path` is absolute
    // (or scan-relative). Matching the two by full-string equality never holds, so the
    // module's metadata stayed `None` and every metadata diagnostic over it was silent.
    // Resolve by the name segment instead, the way the service matchers above already do.
    //
    // The object segment is the module's directory name, which 1C keeps identical to
    // the metadata `<Name>` (the designer enforces it) — so `find_common_module`
    // (keyed on the parsed name) resolves it. The segment comes from the shared path
    // specification and nowhere else: a scan of its own would have to repeat both the
    // accepted spellings of the directory and the rule that keeps an ancestor
    // directory from passing for the collection.
    let file_str = file_path.to_string_lossy().replace('\\', "/");
    let split = bsl_metadata::module_path::split_module_path(&file_str, |segment| {
        matches!(module_collection(segment), Some(ModuleCollection::CommonModules))
    })?;

    configuration.find_common_module(split.object_name).cloned()
}

pub(crate) fn load_form_from_path(file_path: &Path) -> Option<Arc<bsl_metadata::Form>> {
    use bsl_metadata::xml_parser::parse_form_from_bsl_path;

    tracing::debug!(path = %file_path.display(), "Attempting to load form metadata");

    match parse_form_from_bsl_path(file_path) {
        Ok(form) => {
            tracing::debug!(
                form_name = %form.name(),
                form_type = ?form.form_type(),
                event_handlers = form.event_handlers().len(),
                command_handlers = form.command_handlers().len(),
                "Loaded form metadata"
            );
            Some(Arc::new(form))
        }
        Err(e) => {
            tracing::debug!(?e, path = %file_path.display(), "Could not load form metadata");
            None
        }
    }
}

pub fn build_module_metadata(
    file_path: &Path,
    configuration: Option<&bsl_metadata::Configuration>,
) -> hir::ModuleMetadata {
    let uri = file_path.to_string_lossy().to_string();

    let path_info = parse_module_path(&uri);

    let module_type = get_module_type_from_uri(&uri).unwrap_or(bsl_metadata::ModuleType::Unknown);

    tracing::debug!(uri = %uri, module_type = ?module_type, "build_module_metadata");

    let mut execution_context = None;
    let mut common_module = None;
    let mut mdo = None;
    let mut register = None;
    let mut form = None;
    let mut http_service = None;
    let mut web_service = None;
    let mut integration_service = None;

    if let Some(config) = configuration {
        match module_type {
            bsl_metadata::ModuleType::CommonModule => {
                if let Some(cm) = find_common_module_by_path(config, file_path) {
                    execution_context = Some(hir::compute_execution_context(&cm));
                    common_module = Some(Arc::new(cm));
                }
            }
            // A constant's value-manager module has the same owner as its manager
            // module: classifying it without loading that owner would leave the
            // diagnostics it was classified FOR without the metadata they read.
            bsl_metadata::ModuleType::ManagerModule
            | bsl_metadata::ModuleType::ObjectModule
            | bsl_metadata::ModuleType::RecordSetModule
            | bsl_metadata::ModuleType::ValueManagerModule => {
                if let Some(ref info) = path_info {
                    if let (Some(mdo_type), Some(ref name)) = (info.mdo_type, &info.name) {
                        if mdo_type.is_register() {
                            if let Some(reg) = config.find_register_by_type_and_name(mdo_type, name)
                            {
                                register = Some(Arc::new(reg.clone()));
                            }
                        } else {
                            if let Some(obj) = config.find_metadata_object(mdo_type, name) {
                                mdo = Some(Arc::new(obj.clone()));
                            }
                        }
                    }
                }
            }
            bsl_metadata::ModuleType::FormModule => {
                form = load_form_from_path(file_path);
            }
            bsl_metadata::ModuleType::HTTPServiceModule => {
                http_service = find_http_service_by_path(config, file_path);
            }
            bsl_metadata::ModuleType::WebServiceModule => {
                web_service = find_web_service_by_path(config, file_path);
            }
            bsl_metadata::ModuleType::IntegrationServiceModule => {
                integration_service = find_integration_service_by_path(config, file_path);
            }
            _ => {}
        }
    }

    if module_type == bsl_metadata::ModuleType::FormModule && form.is_none() {
        form = load_form_from_path(file_path);
    }

    hir::ModuleMetadata {
        module_type,
        execution_context,
        common_module,
        mdo,
        register,
        form,
        http_service,
        web_service,
        integration_service,
    }
}

/// The owner of an object module inside an external root, by the export's own
/// layout: `<root>/<Name>/Ext/ObjectModule.bsl` names the object, and the root's
/// kind names its type. The path carries no collection segment, so the
/// collection-based owner lookup finds nothing here — and it must not be trusted
/// even when the root directory happens to be spelled like a collection: an
/// export placed under a `DataProcessors/` folder would otherwise borrow a
/// same-named INTERNAL object's metadata from the base. Form modules keep their
/// owner through the form's main attribute, as every form does.
pub fn attach_external_owner(
    metadata: &mut hir::ModuleMetadata,
    file_path: &Path,
    root: &Path,
    canonical_root: &Path,
    kind: bsl_metadata::ExternalObjectKind,
    configuration: &bsl_metadata::Configuration,
) {
    if metadata.module_type != bsl_metadata::ModuleType::ObjectModule {
        return;
    }
    let relative =
        match file_path.strip_prefix(root).or_else(|_| file_path.strip_prefix(canonical_root)) {
            Ok(relative) => relative,
            Err(_) => return,
        };
    let name = relative.components().next().and_then(|first| first.as_os_str().to_str());
    metadata.mdo = name
        .and_then(|name| configuration.find_metadata_object(kind.mdo_type(), name))
        .map(|object| Arc::new(object.clone()));
}

/// The object segment of a service module path, taken from the shared
/// specification — the same one the module type comes from. A scan of its own put
/// the owner and the type on different rules, and the first ancestor directory
/// that happened to be named like the collection took the lookup with it.
fn service_object_name(file_str: &str, collection: ModuleCollection) -> Option<&str> {
    let split = bsl_metadata::module_path::split_module_path(file_str, |segment| {
        module_collection(segment) == Some(collection)
    })?;
    Some(split.object_name)
}

pub(crate) fn find_http_service_by_path(
    configuration: &bsl_metadata::Configuration,
    file_path: &Path,
) -> Option<Arc<bsl_metadata::HTTPService>> {
    let file_str = file_path.to_string_lossy().replace('\\', "/");
    let name = service_object_name(&file_str, ModuleCollection::HttpServices)?;

    configuration.find_http_service(name).map(|hs| Arc::new(hs.clone()))
}

pub(crate) fn find_web_service_by_path(
    configuration: &bsl_metadata::Configuration,
    file_path: &Path,
) -> Option<Arc<bsl_metadata::WebService>> {
    let file_str = file_path.to_string_lossy().replace('\\', "/");
    let name = service_object_name(&file_str, ModuleCollection::WebServices)?;

    configuration.find_web_service(name).map(|ws| Arc::new(ws.clone()))
}

pub(crate) fn find_integration_service_by_path(
    configuration: &bsl_metadata::Configuration,
    file_path: &Path,
) -> Option<Arc<bsl_metadata::IntegrationService>> {
    let file_str = file_path.to_string_lossy().replace('\\', "/");
    let name = service_object_name(&file_str, ModuleCollection::IntegrationServices)?;

    configuration.find_integration_service(name).map(|svc| Arc::new(svc.clone()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_snapshot_carries_toml_source_exclusions_into_the_metadata_loader() {
        use super::MetadataDb as _;
        use crate::RootDatabaseImpl;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        let write_module = |directory: &str, name: &str| {
            let module = root.join("CommonModules").join(directory);
            std::fs::create_dir_all(module.join("Ext")).unwrap();
            std::fs::write(module.join("Ext/Module.bsl"), "").unwrap();
            std::fs::write(
                root.join("CommonModules").join(format!("{directory}.xml")),
                format!(
                    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
<CommonModule uuid="00000000-0000-0000-0000-000000000061">
<Properties><Name>{name}</Name><Server>true</Server></Properties>
</CommonModule></MetaDataObject>"#
                ),
            )
            .unwrap();
        };
        write_module("Allowed", "Разрешенный");
        write_module("Hidden", "Скрытый");
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \".\"\nexclude = [\"CommonModules/Hidden\"]\n",
        )
        .unwrap();

        let project = project_model::Project::new(root).unwrap();
        let snapshot = super::WorkspaceConfigsSnapshot::from_project(&project);
        assert!(snapshot.source_exclusions.is_excluded(&root.join("CommonModules/Hidden")));

        let mut db = RootDatabaseImpl::new();
        db.set_workspace_configs_snapshot(snapshot);
        let root_str = root.to_string_lossy().to_string();
        let revision = db.config_root_revision_for_path(root);
        let input = super::intern_configuration_path(&db, &root_str, revision);
        let loaded = db.load_configuration(input);
        assert!(loaded.find_common_module("Разрешенный").is_some());
        assert!(loaded.find_common_module("Скрытый").is_none());
    }

    #[test]
    fn extension_only_project_snapshot_has_no_synthetic_base_slot() {
        let dir = tempfile::tempdir().unwrap();
        let extension = dir.path().join("Расширения/Feature");
        std::fs::create_dir_all(&extension).unwrap();
        std::fs::write(
            extension.join("Configuration.xml"),
            "<Properties><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties>",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nextensions = [\"Расширения/Feature\"]\n",
        )
        .unwrap();

        let project = project_model::Project::new(dir.path()).unwrap();
        assert!(project.configuration_path().is_none());
        let snapshot = super::WorkspaceConfigsSnapshot::from_project(&project);

        assert_eq!(snapshot.paths.len(), 1);
        assert!(snapshot.paths.iter().all(|(label, _)| label.is_some()));
        assert_eq!(snapshot.paths[0].0.as_deref(), Some("Feature"));
        assert_eq!(snapshot.topological_order, vec![0]);
        assert_eq!(snapshot.closures, vec![Vec::<usize>::new()]);
        assert_eq!(snapshot.kinds, vec![super::RootKind::Extension]);
    }

    /// An external object declared first still lands after every extension, sees
    /// all of them, and is absent from the designer's order — while the
    /// extensions keep the closures the topology gave them.
    #[test]
    fn an_external_object_sees_every_extension_and_is_seen_by_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfe = "<Properties><ConfigurationExtensionPurpose>Customization\
                   </ConfigurationExtensionPurpose></Properties>";
        for name in ["A", "B"] {
            let extension = dir.path().join("src/cfe").join(name);
            std::fs::create_dir_all(&extension).unwrap();
            std::fs::write(extension.join("Configuration.xml"), cfe).unwrap();
        }
        let base = dir.path().join("src/cf");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("Configuration.xml"), "<Configuration/>").unwrap();
        let epf = dir.path().join("src/epf/Обработка");
        std::fs::create_dir_all(epf.join("Обработка")).unwrap();
        std::fs::write(
            epf.join("Обработка.xml"),
            "<MetaDataObject><ExternalDataProcessor uuid=\"1\"/></MetaDataObject>",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\
             externals = [{ name = \"EPF\", path = \"src/epf/Обработка\" }]\n\
             extensions = [\n\
               { name = \"A\", path = \"src/cfe/A\", dependsOn = [\"B\"] },\n\
               { name = \"B\", path = \"src/cfe/B\" },\n\
             ]\n",
        )
        .unwrap();

        let project = project_model::Project::new(dir.path()).unwrap();
        let snapshot = super::WorkspaceConfigsSnapshot::from_project(&project);

        let labels: Vec<Option<&str>> =
            snapshot.paths.iter().map(|(label, _)| label.as_deref()).collect();
        assert_eq!(labels, vec![None, Some("A"), Some("B"), Some("EPF")]);
        assert_eq!(
            snapshot.kinds,
            vec![
                super::RootKind::Base,
                super::RootKind::Extension,
                super::RootKind::Extension,
                super::RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor)
            ]
        );
        assert_eq!(snapshot.closures[1], vec![2], "A keeps its declared dependency on B");
        assert_eq!(snapshot.closures[2], Vec::<usize>::new(), "B stays independent");
        assert_eq!(snapshot.closures[3], vec![2, 1], "the external sees B then A");
        assert_eq!(snapshot.topological_order, vec![0, 2, 1, 3], "and comes last");
        assert_eq!(snapshot.designer_order().collect::<Vec<_>>(), vec![0, 2, 1]);
    }

    /// Таблица по словарю: для каждого конвенционного имени файла модуля
    /// верхнерегистровый путь классифицируется как канонический близнец.
    /// Полнота пар сверяется с перечислением словаря, а не с памятью.
    #[test]
    fn every_dictionary_module_name_classifies_case_insensitively() {
        let pairs = [
            ("CommonModules/X/Ext/Module.bsl", true),
            ("Catalogs/X/Ext/ObjectModule.bsl", true),
            ("Catalogs/X/Ext/ManagerModule.bsl", true),
            ("Catalogs/X/Forms/F/Ext/Form/Module.bsl", true),
            ("CommonCommands/C/Ext/CommandModule.bsl", true),
            ("InformationRegisters/X/Ext/RecordSetModule.bsl", true),
            ("Constants/X/Ext/ValueManagerModule.bsl", true),
            ("Ext/SessionModule.bsl", true),
            ("Ext/ExternalConnectionModule.bsl", true),
            ("Ext/ManagedApplicationModule.bsl", true),
            ("Ext/OrdinaryApplicationModule.bsl", true),
            ("Ext/ApplicationModule.bsl", true),
            // Раскладки EDT в дампе конфигуратора нет: плоское имя формы этот
            // классификатор не принимал и не принимает — ни в одном регистре.
            ("Catalogs/X/Forms/F/Ext/FormModule.bsl", false),
        ];
        for &name in bsl_conventions::ConventionalName::ALL {
            let canonical = name.canonical();
            if !canonical.ends_with(".bsl") {
                continue;
            }
            assert!(
                pairs.iter().any(|(p, _)| p.ends_with(canonical)),
                "в таблице нет пары для {canonical}"
            );
        }
        for (path, classified) in pairs {
            let canonical = get_module_type_from_uri(path);
            assert_eq!(canonical.is_some(), classified, "{path}");
            let upper = path.to_ascii_uppercase();
            assert_eq!(get_module_type_from_uri(&upper), canonical, "{upper}");
        }
    }

    /// Имя объекта может совпадать с именем коллекции (`Справочник.Константы`).
    /// Форма пути жёсткая — `<Коллекция>/<Имя>/Ext/<Модуль>.bsl`, поэтому тип
    /// берётся по позиции, а не поиском последнего похожего сегмента: иначе имя
    /// объекта выигрывает у настоящего типа и путь не разбирается вовсе.
    #[test]
    fn object_named_like_a_collection_still_parses() {
        for (path, expected_type, expected_name) in [
            (
                "Catalogs/Constants/Ext/ManagerModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Constants",
            ),
            (
                "Catalogs/Documents/Ext/ManagerModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Documents",
            ),
            (
                "Справочники/Константы/Ext/ManagerModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Константы",
            ),
            (
                "src/cf/Documents/Enums/Ext/ObjectModule.bsl",
                bsl_metadata::MdoType::Document,
                "Enums",
            ),
            // Каталог-предок может называться как коллекция (типичный случай —
            // `C:\Users\...\Documents`), и у пути без `Ext` он стоит ровно там, где
            // при наличии `Ext` стоит настоящая коллекция.
            (
                "/home/Documents/Catalogs/Товары/ManagerModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Товары",
            ),
            (
                r"C:\Users\Alice\Documents\Catalogs\Товары\ManagerModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Товары",
            ),
            // Сегмент `Ext` не обязателен — так выглядят фикстуры и часть выгрузок.
            ("/Catalogs/Constants/ManagerModule.bsl", bsl_metadata::MdoType::Catalog, "Constants"),
            ("/Documents/Enums/ObjectModule.bsl", bsl_metadata::MdoType::Document, "Enums"),
            // Контроль: обычное имя разбирался и раньше.
            ("Catalogs/Товары/Ext/ManagerModule.bsl", bsl_metadata::MdoType::Catalog, "Товары"),
        ] {
            let info =
                super::parse_module_path(path).unwrap_or_else(|| panic!("{path} must parse"));
            assert_eq!(info.mdo_type, Some(expected_type), "{path}");
            assert_eq!(info.name.as_deref(), Some(expected_name), "{path}");
        }
    }

    use super::*;

    #[test]
    fn stem_name_divergence_is_case_insensitive() {
        // Equal names — including a case-only difference, since BSL folds — do not diverge.
        assert!(!stem_name_diverges("Роль1", "Роль1"));
        assert!(!stem_name_diverges("роль1", "Роль1"));
        assert!(!stem_name_diverges("HTTPService", "httpservice"));
        // A genuine stem-vs-<Name> mismatch diverges.
        assert!(stem_name_diverges("Роль1", "Роль2"));
        assert!(stem_name_diverges("Справочник1", "Справочник2"));
    }

    #[salsa::db]
    #[derive(Default, Clone)]
    struct TestDatabase {
        storage: salsa::Storage<Self>,
    }

    #[salsa::db]
    impl salsa::Database for TestDatabase {}

    #[salsa::db]
    impl MetadataDb for TestDatabase {
        fn load_configuration<'db>(
            &'db self,
            path_input: ConfigurationPathInput<'db>,
        ) -> Arc<Configuration> {
            load_configuration(self, path_input)
        }
    }

    /// Владелец сервисного модуля ищется по той же форме пути, что и тип. Иначе
    /// каталог-предок с именем коллекции уводит поиск на чужое имя, и модуль
    /// остаётся с типом, но без метаданных.
    #[test]
    fn a_service_module_owner_follows_the_same_path_shape() {
        let root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        let config = bsl_metadata::load_from_directory(&root).expect("fixture must load");

        let http = build_module_metadata(
            std::path::Path::new("/tmp/HTTPServices/work/HTTPServices/HTTPСервис1/Ext/Module.bsl"),
            Some(&config),
        );
        assert_eq!(http.module_type, bsl_metadata::ModuleType::HTTPServiceModule);
        assert!(http.http_service.is_some(), "the service next to the module owns it");

        let web = build_module_metadata(
            std::path::Path::new("/tmp/WebServices/work/WebServices/WebСервис1/Ext/Module.bsl"),
            Some(&config),
        );
        assert_eq!(web.module_type, bsl_metadata::ModuleType::WebServiceModule);
        assert!(web.web_service.is_some());

        // Контроль: обычный путь без каталога-двойника работал и раньше.
        let plain = build_module_metadata(
            &root.join("HTTPServices/HTTPСервис1/Ext/Module.bsl"),
            Some(&config),
        );
        assert!(plain.http_service.is_some());
    }

    /// Классификация без загрузки владельца бесполезна: диагностики, ради которых
    /// тип и понадобился, читают метаданные константы.
    #[test]
    fn value_manager_module_loads_its_constant() {
        let mut config = bsl_metadata::Configuration::new("Test");
        config.add_metadata_object(bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Constant,
            "СтрокаКонст",
        ));

        let metadata = build_module_metadata(
            std::path::Path::new("Constants/СтрокаКонст/Ext/ValueManagerModule.bsl"),
            Some(&config),
        );

        assert_eq!(metadata.module_type, bsl_metadata::ModuleType::ValueManagerModule);
        assert!(metadata.mdo.is_some(), "the constant owns this module");
    }

    /// То же и для русского написания каталога общих модулей: тип есть, а поиск
    /// владельца шёл своим обходом и знал только английское имя.
    #[test]
    fn a_russian_common_module_directory_loads_its_metadata() {
        let root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        let config = bsl_metadata::load_from_directory(&root).expect("fixture must load");

        for dir in ["CommonModules", "ОбщиеМодули"] {
            let path = root.join(dir).join("КлиентскийОбщийМодуль/Ext/Module.bsl");
            let metadata = build_module_metadata(&path, Some(&config));
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::CommonModule, "{dir}");
            assert!(metadata.common_module.is_some(), "{dir}: module metadata");
            assert!(metadata.execution_context.is_some(), "{dir}: execution context");
        }
    }

    #[test]
    fn test_load_configuration_caching() {
        let db = TestDatabase::default();

        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer").to_string();
        let path_input = ConfigurationPathInput::new(&db, path, 0);

        let config1 = db.load_configuration(path_input);
        let config2 = db.load_configuration(path_input);

        assert!(Arc::ptr_eq(&config1, &config2), "Salsa should cache configuration");

        assert!(!config1.common_modules().is_empty(), "Should load common modules");
    }

    #[test]
    fn test_load_configuration_different_paths() {
        let db = TestDatabase::default();

        let input1 = ConfigurationPathInput::new(&db, "/path/to/config1".to_string(), 0);
        let input2 = ConfigurationPathInput::new(&db, "/path/to/config2".to_string(), 0);

        assert_ne!(input1, input2, "Different paths should create different inputs");
    }

    #[test]
    fn intern_configuration_path_collapses_separator_variants() {
        let db = TestDatabase::default();
        let backslash = intern_configuration_path(&db, r"C:\foo\bar", 0);
        let forward = intern_configuration_path(&db, "C:/foo/bar", 0);
        assert_eq!(
            backslash, forward,
            "backslash and forward-slash paths must intern as the same Salsa key",
        );
    }

    #[cfg(windows)]
    #[test]
    fn intern_configuration_path_is_case_insensitive_on_windows() {
        let db = TestDatabase::default();
        let upper = intern_configuration_path(&db, r"C:\Foo\Bar", 0);
        let lower = intern_configuration_path(&db, r"c:\foo\bar", 0);
        assert_eq!(upper, lower);
    }

    #[cfg(windows)]
    #[test]
    fn intern_configuration_path_strips_extended_prefix_on_windows() {
        let db = TestDatabase::default();
        let extended = intern_configuration_path(&db, r"\\?\C:\foo\bar", 0);
        let plain = intern_configuration_path(&db, r"C:\foo\bar", 0);
        assert_eq!(extended, plain);
    }

    #[cfg(not(windows))]
    #[test]
    fn intern_configuration_path_preserves_case_on_posix() {
        let db = TestDatabase::default();
        let upper = intern_configuration_path(&db, "/Foo/Bar", 0);
        let lower = intern_configuration_path(&db, "/foo/bar", 0);
        assert_ne!(upper, lower, "POSIX file systems are case-sensitive");
    }

    #[test]
    fn test_find_metadata_object() {
        let db = TestDatabase::default();

        let path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer").to_string();
        let path_input = ConfigurationPathInput::new(&db, path, 0);

        let catalog =
            find_metadata_object(&db, path_input, bsl_metadata::MdoType::Catalog, "Справочник1");
        assert!(catalog.is_some(), "Should find Справочник1");
        assert_eq!(catalog.unwrap().name, "Справочник1");

        let not_found =
            find_metadata_object(&db, path_input, bsl_metadata::MdoType::Catalog, "NonExistent");
        assert!(not_found.is_none(), "Should not find non-existent object");
    }

    #[test]
    fn test_get_module_type_command_module() {
        let uri = "Catalogs/Справочник1/Commands/Команда1/Ext/CommandModule.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommandModule));
    }

    #[test]
    fn test_get_module_type_common_command_module() {
        let uri = "CommonCommands/АвтономнаяРабота/Ext/CommandModule.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommandModule));

        let uri = "src/cf/CommonCommands/АвтономнаяРабота/Ext/CommandModule.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommandModule));

        let uri = "ОбщиеКоманды/ВыполнитьДействие/Ext/CommandModule.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommandModule));
    }

    #[test]
    fn test_get_module_type_common_module() {
        let uri = "CommonModules/ГлобальныйМодуль/Ext/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommonModule));

        let uri = "/home/user/project/src/cf/CommonModules/ГлобальныйМодуль/Ext/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommonModule));
    }

    #[test]
    fn test_get_module_type_form_module() {
        let uri = "Catalogs/Номенклатура/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));

        let uri = "/home/user/project/src/cf/BusinessProcesses/Исполнение/Forms/ВводОписанияЗадачиИсполнителя/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));
    }

    #[test]
    fn get_module_type_handles_windows_backslash_paths() {
        // The LSP feeds native Windows paths (backslash separators); classification
        // must still recognize a form module, otherwise its metadata is never loaded
        // and form-self members (`Команды`, `Элементы`) become UnresolvedMethodCall.
        let uri = r"C:\work\erp\src\cf\Documents\ЭтапПроизводства2_2\Forms\Диспетчирование\Ext\Form\Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));

        let uri = r"C:\work\erp\src\cf\CommonModules\ГлобальныйМодуль\Ext\Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::CommonModule));

        // Mixed separators (drive prefix backslash, rest forward) must also work.
        let uri =
            r"C:\work/erp/src/cf/Catalogs/Номенклатура/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));
    }

    #[test]
    fn test_get_module_type_common_form_module() {
        let uri = "CommonForms/ТестоваяФорма/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));

        let uri = "/home/user/project/src/cf/CommonForms/ТестоваяФорма/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));

        let uri = "ОбщиеФормы/ТестоваяФорма/Ext/Form/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::FormModule));

        let uri = "CommonForms/ТестоваяФорма/Ext/Module.bsl";
        assert_eq!(get_module_type_from_uri(uri), None);
    }

    #[test]
    fn test_build_module_metadata_loads_common_form_without_configuration() {
        let fixture_root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        let bsl_path = fixture_root.join("CommonForms/ТестоваяФорма/Ext/Form/Module.bsl");

        let metadata = build_module_metadata(&bsl_path, None);

        assert_eq!(metadata.module_type, bsl_metadata::ModuleType::FormModule);
        let form = metadata.form.as_ref().expect("common form metadata should be loaded");
        assert_eq!(form.name(), "ТестоваяФорма");
        assert!(form.is_handler("ПриСозданииНаСервере"));
    }

    #[test]
    fn test_build_module_metadata_populates_integration_service() {
        let fixture_root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        let config = bsl_metadata::load_from_directory(&fixture_root).unwrap();

        let bsl_path = fixture_root.join("IntegrationServices/ОбменСообщениями/Ext/Module.bsl");
        let metadata = build_module_metadata(&bsl_path, Some(&config));

        assert_eq!(metadata.module_type, bsl_metadata::ModuleType::IntegrationServiceModule);
        let service =
            metadata.integration_service.as_ref().expect("integration service should be populated");
        let handlers: Vec<_> = service.receive_handlers().collect();
        assert_eq!(handlers, vec!["ОбработатьСообщениеОбычныйПриоритет"]);
    }

    #[test]
    fn test_build_module_metadata_populates_common_module_from_absolute_path() {
        // The config stores common-module `uri` relative to the config root, but analysis
        // passes an absolute path. Resolution must still find the module (by name segment),
        // otherwise every common-module metadata diagnostic stays silent.
        let fixture_root = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        let config = bsl_metadata::load_from_directory(&fixture_root).unwrap();

        let bsl_path = fixture_root.join("CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl");
        let metadata = build_module_metadata(&bsl_path, Some(&config));

        assert_eq!(metadata.module_type, bsl_metadata::ModuleType::CommonModule);
        use bsl_metadata::traits::MdObject;
        let cm = metadata.common_module.as_ref().expect("common module should be populated");
        assert_eq!(cm.name(), "КлиентскийОбщийМодуль");
        assert!(cm.is_client_managed_application(), "client flag must be parsed through");
        assert!(metadata.execution_context.is_some(), "execution context must be derived");
    }

    #[test]
    fn test_get_module_type_object_module() {
        let uri = "Catalogs/Номенклатура/Ext/ObjectModule.bsl";
        assert_eq!(get_module_type_from_uri(uri), Some(bsl_metadata::ModuleType::ObjectModule));
    }

    #[test]
    fn test_get_module_type_unknown() {
        let uri = "SomeRandomPath/File.bsl";
        assert_eq!(get_module_type_from_uri(uri), None);
    }

    #[test]
    fn test_get_module_type_managed_application_module() {
        let uri = "Ext/ManagedApplicationModule.bsl";
        assert_eq!(
            get_module_type_from_uri(uri),
            Some(bsl_metadata::ModuleType::ManagedApplicationModule)
        );

        let uri = "Configuration/Ext/ManagedApplicationModule.bsl";
        assert_eq!(
            get_module_type_from_uri(uri),
            Some(bsl_metadata::ModuleType::ManagedApplicationModule)
        );
    }

    #[test]
    fn test_get_module_type_application_family() {
        assert_eq!(
            get_module_type_from_uri("Ext/SessionModule.bsl"),
            Some(bsl_metadata::ModuleType::SessionModule)
        );
        assert_eq!(
            get_module_type_from_uri("Configuration/Ext/SessionModule.bsl"),
            Some(bsl_metadata::ModuleType::SessionModule)
        );
        assert_eq!(
            get_module_type_from_uri("Ext/OrdinaryApplicationModule.bsl"),
            Some(bsl_metadata::ModuleType::OrdinaryApplicationModule)
        );
        assert_eq!(
            get_module_type_from_uri("Ext/ExternalConnectionModule.bsl"),
            Some(bsl_metadata::ModuleType::ExternalConnectionModule)
        );
        // A common module's Ext/Module.bsl must NOT be swallowed by the Ext arm.
        assert_eq!(
            get_module_type_from_uri("CommonModules/Foo/Ext/Module.bsl"),
            Some(bsl_metadata::ModuleType::CommonModule)
        );
    }

    #[test]
    fn test_parse_module_path_simple() {
        let info = parse_module_path("Catalogs/Справочник1/Ext/ObjectModule.bsl").unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::Catalog));
        assert_eq!(info.name.as_deref(), Some("Справочник1"));
        assert_eq!(info.module_type, bsl_metadata::ModuleType::ObjectModule);
    }

    /// Два ответа об одном пути обязаны совпадать. Раньше это были два независимых
    /// обхода, и расходились они в обе стороны: общий модуль без `Ext` разбирался,
    /// но тип не получал, а обычный файл вне выгрузки тип получал ни за что.
    #[test]
    fn the_two_answers_about_one_path_agree() {
        for path in [
            "CommonModules/Общий/Module.bsl",
            "/CommonModules/Общий/Ext/Module.bsl",
            "Catalogs/Товары/ObjectModule.bsl",
            "src/cf/Catalogs/Товары/Ext/ManagerModule.bsl",
            "src/cf/InformationRegisters/Р/Ext/RecordSetModule.bsl",
            r"C:\src\cf\Documents\ПКО\Ext\ObjectModule.bsl",
            "/home/Documents/Catalogs/Товары/ManagerModule.bsl",
        ] {
            let info = parse_module_path(path).unwrap_or_else(|| panic!("{path} must parse"));
            assert_ne!(info.module_type, bsl_metadata::ModuleType::Unknown, "{path}");
            assert_eq!(
                get_module_type_from_uri(path),
                Some(info.module_type),
                "the two classifiers disagree on {path}"
            );
        }

        // Ни один из ответов не выдаётся файлу вне выгрузки: имя файла само по себе
        // формы не доказывает.
        for path in ["tmp/generated/ObjectModule.bsl", "ObjectModule.bsl", "a/ManagerModule.bsl"] {
            assert!(parse_module_path(path).is_none(), "{path}");
            assert_eq!(get_module_type_from_uri(path), None, "{path}");
        }

        // Имя файла обязано подходить коллекции: сосед по каталогу чужой тип не
        // наследует.
        for path in [
            "CommonModules/Общий/ObjectModule.bsl",
            "HTTPServices/API/ManagerModule.bsl",
            "WebServices/В/RecordSetModule.bsl",
            "IntegrationServices/И/ObjectModule.bsl",
            "Catalogs/Товары/Ext/ValueManagerModule.bsl",
        ] {
            assert_eq!(get_module_type_from_uri(path), None, "{path}");
        }

        // Обе записи каталога общих модулей, как в индексе модулей.
        for path in ["ОбщиеМодули/Общий/Ext/Module.bsl", "CommonModules/Общий/Ext/Module.bsl"]
        {
            assert_eq!(
                get_module_type_from_uri(path),
                Some(bsl_metadata::ModuleType::CommonModule),
                "{path}"
            );
            assert_eq!(
                parse_module_path(path).unwrap().module_type,
                bsl_metadata::ModuleType::CommonModule,
                "{path}"
            );
        }

        // Модуль менеджера значения есть только у константы, и тип у него свой.
        assert_eq!(
            get_module_type_from_uri("Constants/Условие/Ext/ValueManagerModule.bsl"),
            Some(bsl_metadata::ModuleType::ValueManagerModule)
        );
        assert_eq!(
            parse_module_path("Constants/Условие/Ext/ValueManagerModule.bsl").unwrap().module_type,
            bsl_metadata::ModuleType::ValueManagerModule
        );

        // Модуль набора записей есть только у регистров — тот же список, по
        // которому владельца ищет `build_module_metadata`. Правило одно для обеих
        // веток: и для распознанной коллекции, и для неизвестной.
        assert_eq!(
            get_module_type_from_uri("InformationRegisters/Р/Ext/RecordSetModule.bsl"),
            Some(bsl_metadata::ModuleType::RecordSetModule)
        );
        assert_eq!(get_module_type_from_uri("Catalogs/Товары/Ext/RecordSetModule.bsl"), None);

        // Но набор записей есть не только у регистров: платформа даёт его восьми
        // видам, и четыре из них — последовательности, перерасчёты, таблицы и кубы
        // внешних источников — в `MdoType` отсутствуют. Про такую коллекцию
        // известна только форма пути, и ею же приходится обходиться: любое
        // написание, любая вложенность.
        for path in [
            "Sequences/Очередность/Ext/RecordSetModule.bsl",
            "Последовательности/Очередность/Ext/RecordSetModule.bsl",
            "CalculationRegisters/Р/Recalculations/П/Ext/RecordSetModule.bsl",
            "РегистрыРасчёта/Р/Перерасчёты/П/Ext/RecordSetModule.bsl",
            "ExternalDataSources/И/Tables/Т/Ext/RecordSetModule.bsl",
            "ExternalDataSources/И/Cubes/К/Ext/RecordSetModule.bsl",
        ] {
            assert_eq!(
                get_module_type_from_uri(path),
                Some(bsl_metadata::ModuleType::RecordSetModule),
                "{path}"
            );
        }

        // Оборотная сторона той же неопределённости, названная прямо: `Ext` плюс имя
        // файла — это всё свидетельство, поэтому нераспознанная коллекция получит
        // тип и там, где владельцем не является. Цена ошибок несимметрична: лишний
        // тип у пути, которого в выгрузке не бывает, безвреден, а потеря типа у
        // настоящего модуля тиха и уже случалась трижды.
        assert_eq!(
            get_module_type_from_uri("SettingsStorages/Х/Ext/RecordSetModule.bsl"),
            Some(bsl_metadata::ModuleType::RecordSetModule),
            "declared limitation: shape is the only evidence for an unknown collection"
        );

        // Каталог выгрузки — не имя из кода: написание через `ё` называет ту же
        // коллекцию, поэтому регистр расчёта распознаётся в обеих записях и его
        // набор записей не проваливается в резервную ветку.
        for dir in ["РегистрыРасчета", "РегистрыРасчёта", "CalculationRegisters"]
        {
            let path = format!("src/cf/{dir}/Р/Ext/RecordSetModule.bsl");
            assert_eq!(
                get_module_type_from_uri(&path),
                Some(bsl_metadata::ModuleType::RecordSetModule),
                "{path}"
            );
            assert_eq!(
                parse_module_path(&path).and_then(|i| i.mdo_type),
                Some(bsl_metadata::MdoType::CalculationRegister),
                "{path}"
            );
        }

        // Имя модуля команды сравнивается целиком: сосед с похожим окончанием
        // командным модулем не становится.
        assert_eq!(
            get_module_type_from_uri("Catalogs/Товары/Commands/К/Ext/CommandModule.bsl"),
            Some(bsl_metadata::ModuleType::CommandModule)
        );
        assert_eq!(
            get_module_type_from_uri("Catalogs/Товары/Commands/К/Ext/NotACommandModule.bsl"),
            None
        );

        // Коллекция, которой нет в `MdoType`, всё ещё держит модули своего вида —
        // форму в этом случае доказывает служебный уровень, и его написание
        // спецификация сверяет без учёта регистра.
        for path in [
            "src/cf/SettingsStorages/Х/Ext/ManagerModule.bsl",
            "src/cf/SettingsStorages/Х/ext/ManagerModule.bsl",
        ] {
            assert_eq!(
                get_module_type_from_uri(path),
                Some(bsl_metadata::ModuleType::ManagerModule),
                "{path}: an unknown collection still holds a manager module"
            );
        }
        assert_eq!(
            get_module_type_from_uri("src/cf/SettingsStorages/Х/Ext/ManagerModule.bsl"),
            Some(bsl_metadata::ModuleType::ManagerModule),
            "an unknown collection still holds a manager module"
        );
        assert!(parse_module_path("src/cf/SettingsStorages/Х/Ext/ManagerModule.bsl").is_none());

        // Служебные коллекции классифицируются, но владельца этой формы не имеют.
        for (path, expected) in [
            ("src/cf/HTTPServices/API/Ext/Module.bsl", bsl_metadata::ModuleType::HTTPServiceModule),
            ("src/cf/WebServices/В/Ext/Module.bsl", bsl_metadata::ModuleType::WebServiceModule),
            (
                "src/cf/Catalogs/Товары/Commands/К/Ext/CommandModule.bsl",
                bsl_metadata::ModuleType::CommandModule,
            ),
            (
                "src/cf/CommonCommands/К/Ext/CommandModule.bsl",
                bsl_metadata::ModuleType::CommandModule,
            ),
        ] {
            assert_eq!(get_module_type_from_uri(path), Some(expected), "{path}");
            assert!(parse_module_path(path).is_none(), "{path} has no mdo owner");
        }
    }

    /// Кратчайшая форма: относительный путь без служебного `Ext` и без префикса.
    /// Спецификация пути объявляет её допустимой, и отдельная проверка минимальной
    /// длины у вызывающего отсекала её до разбора.
    #[test]
    fn test_parse_module_path_relative_without_service_level() {
        for (path, expected_type, expected_name, expected_module) in [
            (
                "Catalogs/Товары/ObjectModule.bsl",
                bsl_metadata::MdoType::Catalog,
                "Товары",
                bsl_metadata::ModuleType::ObjectModule,
            ),
            (
                "Documents/ПКО/ManagerModule.bsl",
                bsl_metadata::MdoType::Document,
                "ПКО",
                bsl_metadata::ModuleType::ManagerModule,
            ),
        ] {
            let info = parse_module_path(path).unwrap_or_else(|| panic!("{path} must parse"));
            assert_eq!(info.mdo_type, Some(expected_type), "{path}");
            assert_eq!(info.name.as_deref(), Some(expected_name), "{path}");
            assert_eq!(info.module_type, expected_module, "{path}");
            assert_eq!(get_module_type_from_uri(path), Some(expected_module), "{path}");
        }
    }

    #[test]
    fn test_parse_module_path_with_prefix() {
        let info = parse_module_path("./src/cf/Catalogs/ДействияСогласования/Ext/ObjectModule.bsl")
            .unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::Catalog));
        assert_eq!(info.name.as_deref(), Some("ДействияСогласования"));
        assert_eq!(info.module_type, bsl_metadata::ModuleType::ObjectModule);
    }

    #[test]
    fn test_parse_module_path_with_absolute_documents_prefix() {
        let info = parse_module_path(
            "/Users/test/Documents/git/project/Catalogs/Справочник1/Ext/ObjectModule.bsl",
        )
        .unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::Catalog));
        assert_eq!(info.name.as_deref(), Some("Справочник1"));
        assert_eq!(info.module_type, bsl_metadata::ModuleType::ObjectModule);
    }

    #[test]
    fn test_parse_module_path_document() {
        let info =
            parse_module_path("src/cf/Documents/ПриходнаяНакладная/Ext/ObjectModule.bsl").unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::Document));
        assert_eq!(info.name.as_deref(), Some("ПриходнаяНакладная"));
    }

    #[test]
    fn test_parse_module_path_register() {
        let info = parse_module_path(
            "src/cf/InformationRegisters/НастройкиОбмена/Ext/RecordSetModule.bsl",
        )
        .unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::InformationRegister));
        assert_eq!(info.name.as_deref(), Some("НастройкиОбмена"));
        assert_eq!(info.module_type, bsl_metadata::ModuleType::RecordSetModule);
    }

    #[test]
    fn test_parse_module_path_data_processor() {
        let info = parse_module_path("DataProcessors/ЗагрузкаДанных/Ext/ObjectModule.bsl").unwrap();
        assert_eq!(info.mdo_type, Some(bsl_metadata::MdoType::DataProcessor));
        assert_eq!(info.name.as_deref(), Some("ЗагрузкаДанных"));
    }
}
