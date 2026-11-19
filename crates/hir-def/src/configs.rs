use std::sync::Arc;

use bsl_metadata::{
    AttributeType, Configuration, EventSubscription, HTTPService, MdoType, MetadataObject,
    ModuleType, Register, Role, ScheduledJob, Subsystem, WebService,
};
use vfs::FileId;

use crate::DefDatabase;

use bsl_config::VisibleConfig;

/// The body files of one common module, each carrying whether its bytes could be read.
///
/// The distinction is the whole point: a resolver handed only the readable ones cannot
/// tell "the module does not export this method" from "the method may live in the body
/// nobody could read", and answering the first when the second is true blames a file
/// that did nothing wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommonModuleBodies {
    /// A visible configuration declares this module but its body file could not be
    /// mapped. This is a completeness gap, distinct from an enrolled unread body.
    missing_expected_body: bool,

    /// Every body of the module, readable or not, in the order its producer chose.
    ///
    /// One ordered list rather than two, because for the producer that orders by
    /// PRIORITY the order is semantic: the base declaration wins over an extension's,
    /// so a method found in a later body is only the answer if every earlier body was
    /// readable and did not have it. Two separate lists lose exactly that relation, and
    /// the loss is invisible — the wrong body simply answers.
    ///
    /// Not every producer orders by priority: the metadata substrate builds a MERGED
    /// surface extension-first and qualified resolution reverses it. So the order is a
    /// property of who built the list, and the walk must match it —
    /// [`CommonModuleBodies::search`] for priority order,
    /// [`CommonModuleBodies::search_merged_surface`] otherwise.
    ///
    /// Private, because a public field is a third walk past the barrier that looks
    /// more innocent than the one this type removed: `bodies.iter().filter(|b|
    /// !b.unread)` is just a loop over a field, and it loses exactly what `search`
    /// exists to keep.
    bodies: Vec<CommonModuleBody>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommonModuleBody {
    pub file: FileId,
    /// The file exists but its bytes could not be read, so it says nothing about
    /// what the module does or does not export.
    pub unread: bool,
}

/// Which module of a metadata object a body lookup is about.
///
/// The common module is deliberately absent: it has a metadata listing that knows
/// its body URIs, and its own lookup on `ConfigsDatabase`. These three are found
/// only through the path-derived index, which is what makes them share a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdoModuleRole {
    Manager,
    Object,
    RecordSet,
}

/// What a walk over a module's bodies found — the only shape in which an answer about
/// a common module may be phrased, because it keeps "not there" apart from "could not
/// look".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodySearch<T> {
    /// The first body that answered. Every body ahead of it was readable and did not.
    Found(T),
    /// Every body was readable and none of them answered: the module really has no
    /// such thing, and saying so is fair.
    Absent,
    /// The walk reached a body whose bytes could not be read. What that body declares
    /// is unknown, and a lower-priority body must not answer in its place — so nothing
    /// is derivable about this module, least of all against whoever asked.
    Unread,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApplicationModuleKind {
    Managed,
    Ordinary,
    Generic,
    ExternalConnection,
}

impl ApplicationModuleKind {
    pub const ALL: [Self; 4] =
        [Self::Managed, Self::Ordinary, Self::Generic, Self::ExternalConnection];

    pub fn relative_path(self) -> std::path::PathBuf {
        use bsl_conventions::ConventionalName as Conv;

        let file = match self {
            Self::Managed => Conv::ManagedApplicationModule,
            Self::Ordinary => Conv::OrdinaryApplicationModule,
            Self::Generic => Conv::ApplicationModule,
            Self::ExternalConnection => Conv::ExternalConnectionModule,
        };
        std::path::Path::new(Conv::Ext.canonical()).join(file.canonical())
    }

    pub fn module_type(self) -> bsl_metadata::ModuleType {
        match self {
            Self::Managed => bsl_metadata::ModuleType::ManagedApplicationModule,
            Self::Ordinary => bsl_metadata::ModuleType::OrdinaryApplicationModule,
            Self::Generic => bsl_metadata::ModuleType::ApplicationModule,
            Self::ExternalConnection => bsl_metadata::ModuleType::ExternalConnectionModule,
        }
    }
}

impl CommonModuleBodies {
    /// No body of this module is known at all. The signal to degrade to the
    /// path-derived module index.
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// Walk the bodies in priority order, stopping at the first one that answers — and
    /// at the first one that cannot be read.
    ///
    /// Stopping at the unread body is the whole barrier: it is what keeps a
    /// lower-priority body from answering for a higher-priority one whose surface is
    /// unknown. There is deliberately no iterator over the readable bodies — every
    /// consumer handed one walked straight past the barrier and lost it silently.
    pub fn search<T>(&self, mut probe: impl FnMut(FileId) -> Option<T>) -> BodySearch<T> {
        for body in &self.bodies {
            if body.unread {
                return BodySearch::Unread;
            }
            if let Some(found) = probe(body.file) {
                return BodySearch::Found(found);
            }
        }
        BodySearch::Absent
    }

    /// Look through the whole MERGED surface, answering only when all of it was
    /// readable.
    ///
    /// The counterpart of [`Self::search`] for consumers handed the bodies in merged
    /// order rather than priority order (see the reversal in the metadata substrate):
    /// there is no "first" body to stop at, so position cannot say whose declaration
    /// wins. What stays true regardless of order is that an unread body leaves the
    /// surface partly unknown, and a verdict drawn from the rest is a guess — so any
    /// unread body at all yields [`BodySearch::Unread`].
    pub fn search_merged_surface<T>(
        &self,
        probe: impl FnMut(FileId) -> Option<T>,
    ) -> BodySearch<T> {
        if self.bodies.iter().any(|b| b.unread) {
            return BodySearch::Unread;
        }
        self.search(probe)
    }

    /// Every body, readable or not, for callers that record a RELATION to the module
    /// rather than read anything out of it — the call graph's reverse references, which
    /// must survive a body becoming readable. Never a substitute for [`Self::search`].
    pub fn all_for_reference(&self) -> impl Iterator<Item = FileId> + '_ {
        self.bodies.iter().map(|b| b.file)
    }

    /// The bodies that exist but could not be read, in priority order.
    pub fn unread(&self) -> impl Iterator<Item = FileId> + '_ {
        self.bodies.iter().filter(|b| b.unread).map(|b| b.file)
    }

    pub fn push(&mut self, file: FileId, unread: bool) {
        if !self.bodies.iter().any(|b| b.file == file) {
            self.bodies.push(CommonModuleBody { file, unread });
        }
    }

    pub fn mark_missing_expected_body(&mut self) {
        self.missing_expected_body = true;
    }

    pub fn has_missing_expected_body(&self) -> bool {
        self.missing_expected_body
    }

    /// Flip the list between the two orders a producer may build it in — merged
    /// (extension-first) and priority (base-first). Named for what it does to meaning,
    /// not to the vector, because that is the only reason to do it.
    pub fn reverse_priority(&mut self) {
        self.bodies.reverse();
    }

    /// Each body with its readability, in the producer's order. For building another
    /// composition out of this one — never for deciding what the module declares,
    /// which is what [`Self::search`] is for.
    pub fn iter(&self) -> impl Iterator<Item = CommonModuleBody> + '_ {
        self.bodies.iter().copied()
    }
}

#[salsa::db]
pub trait ConfigsDatabase: DefDatabase {
    /// Owner-published per-file compilation context. `None` preserves legacy single-file settings.
    fn file_compilation_context(
        &self,
        _file_id: FileId,
    ) -> Option<Arc<crate::compilation_context::FileCompilationContext>> {
        None
    }

    /// The configurations VISIBLE to `file_id` as separate entries (no merge):
    /// base first, then the file's dependency chain in order (own extension
    /// last). Never includes an unrelated sibling extension.
    fn configurations(&self, file_id: FileId) -> Vec<VisibleConfig>;

    /// EVERY registered root (base, all extensions, then the external objects)
    /// — the inventory view for index/graph builders covering the whole
    /// workspace. Deliberately ignores per-file dependency-scoped visibility;
    /// semantic resolution must use [`Self::configurations`].
    fn configurations_inventory(&self) -> Vec<VisibleConfig>;

    /// Load, on the CALLING thread, every configuration root `modules` can reach,
    /// so a parallel region over them never enters the internally-parallel
    /// whole-config loader from a worker.
    ///
    /// A module's root is attributed from its own path on disk, which is NOT the
    /// declared-root set of [`Self::configurations_inventory`]: a workspace whose
    /// configuration was never discovered is itself the only declared root, while
    /// its files attribute to whatever nested directory actually holds their
    /// metadata. Warming the declared roots alone therefore leaves those nested
    /// roots to be loaded lazily, from inside the pool.
    fn warm_config_roots(&self, modules: &[crate::ModuleId]);

    fn merged_visible_configuration(&self, file_id: FileId) -> Option<Arc<Configuration>>;

    /// Resolve a single MetadataObject-family object (catalog, document, enum,
    /// constant, …) visible to `file_id` at per-MDO Salsa granularity: the base
    /// config composed with the file's own extension (extension priority), with no
    /// other extension visible. Depending on this records a dependency on just that
    /// MDO, so editing an unrelated MDO does not invalidate the caller — unlike
    /// reading the whole [`configurations`]/[`merged_visible_configuration`].
    ///
    /// Registers are a separate type/query; resolve them via [`resolve_register`].
    fn resolve_metadata_object(
        &self,
        file_id: FileId,
        mdo_type: MdoType,
        name: &str,
    ) -> Option<Arc<MetadataObject>>;

    /// The register counterpart of [`resolve_metadata_object`] (registers are a
    /// separate type). Same per-MDO granularity and base + own-extension scoping.
    fn resolve_register(
        &self,
        file_id: FileId,
        mdo_type: MdoType,
        name: &str,
    ) -> Option<Arc<Register>>;

    /// Resolve a register visible to `file_id` by NAME alone, when the caller does
    /// not know its kind (e.g. a `Движения.<Register>` movement touch). Same
    /// per-MDO granularity and base + own-extension scoping as [`resolve_register`].
    fn resolve_register_by_name(&self, file_id: FileId, name: &str) -> Option<Arc<Register>>;

    /// Whether the effective object/manager module surface visible to `file_id`
    /// exports `variable_name` for the metadata object.
    fn has_effective_module_variable(
        &self,
        file_id: FileId,
        module_type: ModuleType,
        mdo_type: MdoType,
        object_name: &str,
        variable_name: &str,
    ) -> bool;

    /// Resolve the underlying type of the defined type `name` visible to
    /// `file_id`, at per-defined-type Salsa granularity. Base + the file's own
    /// extension (which replaces the underlying type wholesale). Returns the
    /// terminal type only after one hop; defined-type chains are unwound by
    /// [`bsl_metadata::resolve_defined_type_terminal`] in the type-lowering layer.
    fn resolve_defined_type(&self, file_id: FileId, name: &str) -> Option<AttributeType>;

    /// Resolve the common module `name` visible to `file_id` at per-common-module
    /// Salsa granularity: base + the file's own extension (which replaces the module
    /// wholesale). Used for by-name visibility/flag checks; the module's body is
    /// resolved separately through the symbol tree. Depending on this records a
    /// dependency on just that common module.
    fn resolve_common_module(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<bsl_metadata::CommonModule>>;

    /// Body file candidates of the common module `name` visible to `file_id`:
    /// the base-configuration body first, then the body adopted by the file's
    /// own extension. A base file and its extension sibling share one module
    /// name, so qualified lookup must see both — the base declares the
    /// canonical surface, the extension adds its own exported methods, and no
    /// other extension's adoption is visible to this file.
    ///
    /// `None` means the provider has no visibility-scoped body lookup — the
    /// caller falls back to the path-derived module index. An empty `Some` means
    /// the configs know the module but no body file mapped (metadata-URI drift);
    /// callers should degrade to the path index rather than report the module
    /// missing.
    fn resolve_common_module_file_candidates(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<CommonModuleBodies> {
        let _ = (file_id, name);
        None
    }

    /// Bodies of a metadata object's module, visible to `file_id`, base first.
    ///
    /// The visibility question this answers is NOT the one `mdo_visible` answers.
    /// That one asks whether the caller can see the OBJECT, and a catalog adopted
    /// by the base configuration is visible to every extension. This one asks
    /// whose BODY may answer, and a body living in a root the caller does not
    /// depend on may not — which is the whole defect this exists to close.
    ///
    /// `None` means the provider has no visibility-scoped lookup — the caller
    /// falls back to the path-derived module index and behaves as it did before.
    ///
    /// An empty `Some` is a genuine "no visible root holds such a body", and the
    /// caller must report it as absent. This is the OPPOSITE of
    /// [`ConfigsDatabase::resolve_common_module_file_candidates`], where an empty
    /// `Some` means the configs know the module but no body file mapped and the
    /// caller degrades to the path index. The two differ because the substrates
    /// differ: the common module has a listing that can disagree with the file
    /// set, these three have only the path index — degrading here would hand back
    /// exactly the invisible body the filter just removed.
    fn resolve_mdo_module_file_candidates(
        &self,
        file_id: FileId,
        role: MdoModuleRole,
        mdo_type: bsl_metadata::MdoType,
        name: &str,
    ) -> Option<CommonModuleBodies> {
        let _ = (file_id, role, mdo_type, name);
        None
    }

    /// Application-module bodies visible to `file_id`, base first and the
    /// caller's extension/dependency chain afterwards. `Some(empty)` proves the
    /// fixed module path is absent from every visible root; `None` means the
    /// provider cannot enumerate the surface and callers must stay conservative.
    fn resolve_application_module_file_candidates(
        &self,
        file_id: FileId,
        kind: ApplicationModuleKind,
    ) -> Option<CommonModuleBodies> {
        let _ = (file_id, kind);
        None
    }

    /// Resolve the event subscription `name` visible to `file_id` at
    /// per-event-subscription Salsa granularity: base + the file's own extension
    /// (extension priority). Depending on this records a dependency on just that
    /// subscription when the substrate is bootstrapped.
    fn resolve_event_subscription(
        &self,
        file_id: FileId,
        name: &str,
    ) -> Option<Arc<EventSubscription>>;

    /// Names of event subscriptions visible to `file_id`, for completion/member
    /// enumeration. File-scoped like [`resolve_event_subscription`].
    fn event_subscription_names(&self, file_id: FileId) -> Vec<String>;

    /// Explicit project/config enumeration for graph-style consumers that need the
    /// subscription handler metadata, not just a single hot lookup.
    fn enumerate_event_subscriptions(&self, file_id: FileId) -> Vec<Arc<EventSubscription>>;

    /// Resolve the role `name` visible to `file_id` at per-role Salsa granularity.
    /// This follows the same main-listing single-name parity as [`role_names`]:
    /// a single-name lookup resolves against the roles recorded for the file's
    /// main listing, while graph-style callers that need every root should use
    /// [`enumerate_roles`].
    fn resolve_role(&self, file_id: FileId, name: &str) -> Option<Arc<Role>>;

    /// Names of roles visible to `file_id`, for completion/member enumeration.
    /// File-scoped like [`resolve_role`], and likewise tied to the main listing.
    fn role_names(&self, file_id: FileId) -> Vec<String>;

    /// Explicit project/config enumeration for graph-style consumers that need
    /// every listed role object, not just main-listing names.
    fn enumerate_roles(&self, file_id: FileId) -> Vec<Arc<Role>>;

    /// Resolve the scheduled job `name` visible to `file_id` at per-scheduled-job
    /// Salsa granularity. File-scoped like [`resolve_event_subscription`], but
    /// Wave 2b resolves from the main listing only: `Configuration`'s extension
    /// overlay does not merge scheduled jobs today, so the bootstrapped path
    /// intentionally matches the merged whole-config lookup by ignoring the
    /// file's own extension root for the per-name resolution. The scheduled-job
    /// counterpart of [`resolve_event_subscription`].
    fn resolve_scheduled_job(&self, file_id: FileId, name: &str) -> Option<Arc<ScheduledJob>>;

    /// Names of scheduled jobs visible to `file_id`, for completion/member
    /// enumeration. File-scoped like [`resolve_scheduled_job`].
    fn scheduled_job_names(&self, file_id: FileId) -> Vec<String>;

    /// Resolve the HTTP service `name` visible to `file_id` at per-service Salsa
    /// granularity. Wave 2d keeps main-listing parity with the current whole-config
    /// service surface.
    fn resolve_http_service(&self, file_id: FileId, name: &str) -> Option<Arc<HTTPService>>;

    /// Names of HTTP services visible to `file_id`, for completion/member enumeration.
    fn http_service_names(&self, file_id: FileId) -> Vec<String>;

    /// Resolve the Web service `name` visible to `file_id` at per-service Salsa
    /// granularity. Wave 2d keeps main-listing parity with the current whole-config
    /// service surface.
    fn resolve_web_service(&self, file_id: FileId, name: &str) -> Option<Arc<WebService>>;

    /// Names of Web services visible to `file_id`, for completion/member enumeration.
    fn web_service_names(&self, file_id: FileId) -> Vec<String>;

    /// Resolve the subsystem `name` visible to `file_id` at per-subsystem Salsa
    /// granularity. The subsystem counterpart of [`resolve_scheduled_job`]; the
    /// bootstrapped path composes the main listing with the file's own extension
    /// listing, merging a same-name extension into the base via
    /// [`Subsystem::merge_from`] and resolving an extension-only subsystem from
    /// the extension listing. When the substrate is not bootstrapped, falls back
    /// to the merged whole-config subsystem lookup.
    fn resolve_subsystem(&self, file_id: FileId, name: &str) -> Option<Arc<Subsystem>>;

    /// Names of subsystems visible to `file_id`, for completion/member enumeration.
    /// File-scoped like [`resolve_subsystem`].
    fn subsystem_names(&self, file_id: FileId) -> Vec<String>;

    /// Explicit project/config enumeration for graph-style consumers that need
    /// every listed subsystem, not just a single hot lookup. The subsystem
    /// counterpart of [`enumerate_roles`]: prefers all configured listings when
    /// all are bootstrapped, merging same-name subsystems deterministically (base
    /// first, then extension order); falls back to the whole-config enumeration
    /// only behind this explicit enumeration API.
    fn enumerate_subsystems(&self, file_id: FileId) -> Vec<Arc<Subsystem>>;

    /// Whether `file_id` belongs to a configured project (has at least one visible
    /// config root). In the workspace path this reads only the config-paths input,
    /// so callers can gate on config presence — distinguishing "no config, defer to
    /// the module index" from "config present, name genuinely absent" — without
    /// taking a dependency on the whole loaded configuration.
    fn has_config_root(&self, file_id: FileId) -> bool;

    /// Whether `file_id` has a visible main configuration or an applicable
    /// extension configuration without loading the merged configuration.
    fn file_has_visible_config(&self, file_id: FileId) -> bool;

    /// Whether `file_id` lies in the main configuration root or in one of its
    /// extension roots, and that main root holds `Configuration.xml`. `false` for an
    /// external data processor or report, a file outside every configuration root,
    /// and a project without a main configuration (only extensions, only external
    /// objects, or a bare directory of sources): what such a module refers to by
    /// name is not described anywhere the analyzer can read.
    fn file_in_main_configuration(&self, file_id: FileId) -> bool;

    /// The documents recording into the register `(parent, register_name)` visible
    /// to `file_id` — a cross-MDO reverse relation aggregated across the base and
    /// the file's extension. Returns owned names (not a borrow) so a db-backed
    /// resolver can compose them. Not yet narrowed to a reverse index, so it still
    /// depends on the whole visible configuration.
    fn recorders_for_register(
        &self,
        file_id: FileId,
        parent: MdoType,
        register_name: &str,
    ) -> Vec<String>;

    fn resolved_module_summary(
        &self,
        module_id: crate::ModuleId,
    ) -> Arc<crate::call_graph::ResolvedModuleSummary>;

    fn workspace_call_graph(
        &self,
        source_root_id: base_db::SourceRootId,
    ) -> Arc<crate::call_graph::WorkspaceCallGraph>;
}
