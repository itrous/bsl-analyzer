use bsl_metadata::MdoType;
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;
use syntax::{TextRange, TextSize};

use crate::{
    body::{Body, ManagerType},
    hir::{Expr, ExprIdx, Literal},
    item_tree::{AnnotationKind, ItemTree},
    name::Name,
    MethodId, MethodKey, ModuleBodies, ModuleId,
};
use intern::NormName;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallSummary {
    pub methods: Vec<MethodSummary>,
    pub call_edges: Vec<CallEdge>,
    pub notify_regs: Vec<NotifyReg>,
    pub idle_handler_regs: Vec<IdleReg>,
    pub set_action_regs: Vec<SetActionReg>,
    /// Local ids of methods named by an identifier-shaped string literal somewhere in
    /// the module. Dynamic handler binding always carries the target's name as a
    /// string — directly (`УстановитьДействие`), through a command's `Действие`
    /// property, or through a helper in another module fed a parameter structure —
    /// and that literal sits in the same module as the handler. Recording every such
    /// literal covers all those shapes without cross-module flow analysis, at the
    /// cost of also matching string *data* that coincides with a method name.
    /// Sorted and deduplicated.
    pub name_literal_refs: Vec<MethodKey>,
    pub form_entries: Vec<FormEventEntry>,
}

impl ModuleCallSummary {
    /// The subset of this summary that can still influence method-only call pairs
    /// (see [`crate::call_hierarchy_index::MethodCallPair::from_resolved_edge`]):
    /// method-caller edges whose target shape can resolve to a user method
    /// (local, qualified-module, manager access with a method), plus method-caller
    /// notify and idle-handler registrations. Everything else — module-code
    /// callers, manager/register touches without a resolvable method, `SetAction`,
    /// name literals, form entries — can never produce a pair and is dropped.
    ///
    /// Source ranges are zeroed and entries deduplicated: pairs carry no ranges and
    /// resolution never reads them, so this keeps the retained residency of a
    /// whole-workspace build proportional to the distinct call facts, not to the
    /// call-site count.
    pub fn method_pair_subset(&self) -> ModuleCallSummary {
        let zero = TextRange::empty(TextSize::from(0));

        let mut seen_edges = FxHashSet::default();
        let call_edges = self
            .call_edges
            .iter()
            .filter(|edge| {
                matches!(edge.caller, CallerId::Method(_))
                    && matches!(
                        edge.target,
                        CallTarget::Local { .. }
                            | CallTarget::QualifiedModule { .. }
                            | CallTarget::ManagerAccess { method_name: Some(_), .. }
                    )
            })
            .filter(|edge| seen_edges.insert((edge.caller, edge.target.clone(), edge.kind)))
            .map(|edge| CallEdge { range: zero, ..edge.clone() })
            .collect();

        let mut seen_notify = FxHashSet::default();
        let notify_regs = self
            .notify_regs
            .iter()
            .filter(|reg| {
                matches!(reg.caller, CallerId::Method(_))
                    && !matches!(reg.target, NotifyTarget::Unsupported)
            })
            .filter(|reg| {
                seen_notify.insert((reg.caller, reg.callback_name.clone(), reg.target.clone()))
            })
            .map(|reg| NotifyReg { range: zero, ..reg.clone() })
            .collect();

        let mut seen_idle = FxHashSet::default();
        let idle_handler_regs = self
            .idle_handler_regs
            .iter()
            .filter(|reg| matches!(reg.caller, CallerId::Method(_)))
            .filter(|reg| seen_idle.insert((reg.caller, reg.handler_name.clone())))
            .map(|reg| IdleReg { range: zero, one_shot: false, ..reg.clone() })
            .collect();

        ModuleCallSummary {
            methods: Vec::new(),
            call_edges,
            notify_regs,
            idle_handler_regs,
            set_action_regs: Vec::new(),
            name_literal_refs: Vec::new(),
            form_entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSummary {
    pub local_id: MethodKey,
    pub name: Name,
    pub dispatch: MethodDispatch,
    pub is_export: bool,
}

/// Per-method facts derivable from the item tree alone — declaration name,
/// export flag, annotation dispatch, and the name/source ranges — without
/// lowering any body. This is the compact, resident "Pass A" data a streaming
/// graph build needs: enumerating methods and their durable-id/source coordinates
/// must not force the heavy body HIR that the per-module edge projection does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphMethodEntry {
    pub local_id: MethodKey,
    pub name: Name,
    pub is_export: bool,
    pub dispatch: MethodDispatch,
    /// Hash of the declaration header, including its parameter list and export marker.
    /// Filled by the graph index from the same file text used to parse this item tree.
    pub signature_hash: u64,
    /// Range of the declaration's name token — anchors the start of the signature.
    pub name_range: TextRange,
    /// End of the declaration header (closing `)` or export keyword) — anchors the
    /// end of the full, possibly multi-line, signature slice.
    pub sig_end: TextSize,
    /// Range of the whole procedure/function — the `source` slice.
    pub source_range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDispatch {
    pub can_run_on_client: bool,
    pub can_run_on_server: bool,
    pub no_context: bool,
}

impl MethodDispatch {
    pub fn from_annotation(kind: Option<&AnnotationKind>) -> Self {
        match kind {
            Some(AnnotationKind::AtClient) => {
                Self { can_run_on_client: true, can_run_on_server: false, no_context: false }
            }
            Some(AnnotationKind::AtServer) => {
                Self { can_run_on_client: false, can_run_on_server: true, no_context: false }
            }
            Some(AnnotationKind::AtServerNoContext) => {
                Self { can_run_on_client: false, can_run_on_server: true, no_context: true }
            }
            Some(AnnotationKind::AtClientAtServer) => {
                Self { can_run_on_client: true, can_run_on_server: true, no_context: false }
            }
            Some(AnnotationKind::AtClientAtServerNoContext) => {
                Self { can_run_on_client: true, can_run_on_server: true, no_context: true }
            }
            Some(
                AnnotationKind::Before
                | AnnotationKind::After
                | AnnotationKind::Instead
                | AnnotationKind::ChangeAndValidate,
            )
            | None => Self { can_run_on_client: true, can_run_on_server: false, no_context: false },
        }
    }

    pub fn is_server_only(&self) -> bool {
        self.can_run_on_server && !self.can_run_on_client
    }

    /// Module-level client/server capability from a common module's execution
    /// context (where dispatch is set per-module, not per-method). `None` for
    /// `Unknown` — the caller then falls back to per-method annotation dispatch.
    pub fn from_execution_context(ctx: crate::ExecutionContext) -> Option<Self> {
        use crate::ExecutionContext;
        let d = |can_run_on_client, can_run_on_server| Self {
            can_run_on_client,
            can_run_on_server,
            no_context: false,
        };
        Some(match ctx {
            ExecutionContext::Server
            | ExecutionContext::ServerCall
            | ExecutionContext::ExternalConnection => d(false, true),
            ExecutionContext::Client => d(true, false),
            ExecutionContext::ClientServer => d(true, true),
            ExecutionContext::Unknown => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub caller: CallerId,
    pub target: CallTarget,
    pub kind: EdgeKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    DirectLocal,
    DirectQualifiedModule,
    /// `Справочники.X.СоздатьЭлемент()` — a manager method that creates an object
    /// of its metadata type. Target is an [`GraphNode::Mdo`].
    ManagerCreates,
    /// Any other touch of an object through its manager (a platform find/select
    /// method, or a bare `Справочники.X` reference). Target is an [`GraphNode::Mdo`].
    ManagerAccess,
    /// The caller runs an SDBL query that reads a metadata object. From a
    /// `Method`/`ModuleCode` node to the read object's [`GraphNode::Mdo`].
    QueryRef,
    /// Structural metadata containment: a metadata object owns a form
    /// ([`GraphNode::Mdo`] → [`GraphNode::Form`]), or a form owns an item
    /// ([`GraphNode::Form`] → [`GraphNode::FormItem`]). Derived from form metadata,
    /// not from code.
    Contains,
    /// A form's data model binds to the metadata structure it mirrors. A UI element
    /// ([`GraphNode::FormItem`]) whose data path is `Объект.<поле>` →
    /// [`GraphNode::Attribute`]/[`GraphNode::TabularSectionAttribute`] of the backing
    /// object's field ("which object fields are shown on the form"); a Ref-typed form
    /// attribute ([`GraphNode::FormAttribute`]) → the [`GraphNode::Mdo`] it is typed
    /// as. Derived from form metadata, not from code.
    DataBinding,
    /// A `Новый ОписаниеОповещения("Метод", …)` callback: the registering method (or
    /// module body) links to the handler named by the string literal. Not a direct
    /// call — the platform invokes it later — so it is kept separate from `call`.
    NotifyRef,
    /// A `ПодключитьОбработчикОжидания("Метод", …)` idle handler: the registering
    /// method links to the named handler in the current module. Like `NotifyRef`,
    /// dispatched by the platform, not a direct call.
    IdleHandler,
    /// A `ПодпискаНаСобытие` metadata object links to its handler method. From the
    /// subscription's [`GraphNode::Mdo`] to the handler [`GraphNode::Method`]. Derived
    /// from configuration metadata, not from code.
    EventSubscriptionRef,
    /// A document touches register records through a `Движения.<Регистр>.<метод>()` call
    /// (`Добавить`/`Записать`/`Очистить`/`Загрузить`/`Выгрузить`). From the writing
    /// `Method`/`ModuleCode` node to the touched register's [`GraphNode::Mdo`]. The
    /// register name is literal; its type is resolved from configuration metadata.
    ///
    /// Scope: only the *call* form is modelled. A bare property read that captures the
    /// collection for later use (`НаборЗаписей = Движения.<Регистр>;` then writes through
    /// the variable) is not — that needs receiver dataflow, which lowering does not do.
    /// At the document grain this rarely loses coverage, because a document that records
    /// movements to a register also writes it through one of the call forms above.
    RegisterMovement,
    /// A subsystem contains a metadata object or a child subsystem. From the subsystem's
    /// [`GraphNode::Mdo`] (type [`MdoType::Subsystem`]) to the member object's
    /// [`GraphNode::Mdo`], or to a child subsystem's node. Derived from configuration
    /// metadata (the subsystem's `Content`/`ChildObjects`), not from code. Lets an impact
    /// analysis answer "which subsystems must be updated if I delete this object".
    SubsystemMembership,
    /// A role references a metadata object it grants rights on. From the role's
    /// [`GraphNode::Mdo`] (type `MdoType::Role`) to the referenced object's
    /// [`GraphNode::Mdo`]. Derived from configuration metadata (`Rights.xml`), not from code:
    /// a direct object-rights entry is `resolved`, while an object named only inside an RLS
    /// restriction condition is `inferred` (parsed from the restriction query text). Lets an
    /// impact analysis answer "which roles grant rights on / restrict this object" before
    /// deleting or renaming it.
    RoleReference,
    /// A document declares it posts records into a register. From the document's
    /// [`GraphNode::Mdo`] (type [`MdoType::Document`]) to the register's [`GraphNode::Mdo`].
    /// Derived from configuration metadata (the document's `RegisterRecords`), not from code —
    /// so it answers "which documents post this register" soundly even when the posting code
    /// addresses the register dynamically (a string name into `РегистрыНакопления[…]` or a
    /// `Движения[…]` index), which `register_movement` (literal `Движения.X.метод()`) cannot.
    RegisterRecords,
    /// Code reaches a register's record-set engine through a literal manager creator —
    /// `РегистрыНакопления.<X>.СоздатьНаборЗаписей()` / `СоздатьМенеджерЗаписи()` (and the
    /// English `CreateRecordSet` / `CreateRecordManager`). From the calling
    /// `Method`/`ModuleCode` node to the register's [`GraphNode::Mdo`], provenance `inferred`.
    ///
    /// This is register record-set *access*, which is write-capable: a record set is the
    /// engine through which non-document writers (typically common modules) post registers,
    /// but the same engine can also be read (set a filter, `Прочитать`) without writing. It
    /// is kept distinct from `manager_creates` (which would otherwise bury these among object
    /// `СоздатьЭлемент` creations) so an impact analysis can ask "which code touches this
    /// register via its record-set engine" — the code-level complement to `register_records`
    /// (declared document posts) and `register_movement` (a registrator's `Движения`).
    RegisterRecordSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CallTarget {
    Local {
        callee_local_id: MethodKey,
    },
    QualifiedModule {
        module_name: Name,
        method_name: Name,
    },
    ManagerAccess {
        manager_type: ManagerType,
        object_name: Name,
        method_name: Option<Name>,
    },
    ThisObjectMethod {
        method_name: Name,
    },
    /// A `[получатель.]Движения.<Регистр>.<метод>()` document movement touch. The register
    /// name is the literal token; lowering stays syntax-only, so the register's metadata
    /// type is resolved later from configuration (see `Resolver::resolve_register_by_name`).
    RegisterMovement {
        register_name: Name,
    },
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallerId {
    Method(MethodKey),
    ModuleCode,
}

/// Where a `Новый ОписаниеОповещения` callback's handler lives, decided purely
/// syntactically from the second constructor argument. Kept distinct from a bare
/// `Option` because "no module" and "receiver we can't classify" must not collapse:
/// the graph resolves `ThisObject` to a current-module method but must NOT invent an
/// edge for an `Unsupported` receiver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NotifyTarget {
    /// `ЭтотОбъект` / `ЭтаФорма` (or English forms) — handler is in the current module.
    ThisObject,
    /// A bare identifier receiver — treated as a (common) module name to resolve.
    Module(Name),
    /// Receiver is not a plain identifier (a call, index, `Неопределено`, …) or is
    /// absent: the handler module cannot be decided syntactically.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyReg {
    pub caller: CallerId,
    pub callback_name: Name,
    pub target: NotifyTarget,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleReg {
    pub caller: CallerId,
    pub handler_name: Name,
    pub one_shot: bool,
    pub range: TextRange,
}

/// An `Элементы.<Элемент>.УстановитьДействие("Событие", "Обработчик")` runtime event
/// binding: the registering method links to the named handler in the current form
/// module. Like [`IdleReg`]/[`NotifyReg`], the platform invokes the handler later, so it
/// is a string-named dispatch reference, not a direct call. The handler always lives in
/// the current module (a form element's action targets a form-module procedure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetActionReg {
    pub caller: CallerId,
    pub handler_name: Name,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormEventEntry {
    pub event_type: String,
    pub handler_name: Name,
}

/// A module's call edges with each target resolved to a concrete graph node
/// where possible. Produced by `resolved_module_summary_query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModuleSummary {
    pub module: ModuleId,
    pub edges: Vec<ResolvedCallEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCallEdge {
    pub caller: CallerId,
    pub target: ResolvedTarget,
    pub kind: EdgeKind,
    pub range: TextRange,
    pub provenance: EdgeProvenance,
}

/// Resolution outcome for a call edge's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    /// Resolved to a concrete method node.
    Method(MethodId),
    /// Resolved to a metadata-object node (a manager access that is not a user
    /// manager-module method — a creation/find platform method or bare reference).
    Mdo { mdo_type: MdoType, object_name: Name },
    /// Not resolved to a concrete node; the original target is preserved so
    /// the gap is surfaced honestly rather than dropped.
    Unresolved(CallTarget),
}

/// How much to trust a resolved edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeProvenance {
    /// Target points at a concrete code node by direct lookup: a local call, an
    /// exported qualified-module call, or an exported user method reached through a
    /// fully-literal `Коллекция.Объект.Метод()` manager path (the object's manager
    /// module is uniquely determined, so the lookup is not a guess).
    Resolved,
    /// Target points at a metadata-object node rather than a code node: a platform
    /// manager method (`СоздатьЭлемент`/find/…) or a bare `Справочники.X` reference,
    /// where we know which object is touched but the call itself is a platform builtin
    /// we do not model as a node.
    Inferred,
    /// Target method exists but is not exported — visible-but-unreachable across modules.
    VisibilityBlocked,
    /// Target could not be resolved to any node: a qualified call to a missing/unknown
    /// module, or a `ЭтотОбъект` platform method (a manager method like `СоздатьЭлемент`
    /// does not land here — it resolves to an `Mdo` node with `Inferred` provenance).
    Unresolved,
    /// Target was resolved from a string literal (a callback name in
    /// `ОписаниеОповещения` / `ПодключитьОбработчикОжидания`, or a subscription
    /// handler), not from a static call. Lower trust than `Resolved`: consumers can
    /// filter string-dispatch edges out of "who really calls whom".
    StringResolved,
}

/// A globally-addressable node in the workspace call graph.
///
/// Not `Copy`: an `Mdo` node carries a `Name`. Clones are cheap (`SmolStr` is
/// inline for short names) and the graph stores nodes by reference internally.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GraphNode {
    Method(MethodId),
    /// A module's top-level body (the `CallerId::ModuleCode` caller).
    ModuleCode(ModuleId),
    /// A metadata object (catalog, document, register, …) reached through its
    /// manager. The identity is the metadata type plus the object name as it
    /// appears in code, canonicalised to a single spelling per object at fold time.
    Mdo {
        mdo_type: MdoType,
        object_name: Name,
    },
    /// An attribute (or register dimension/resource, tabular-section column, …) of
    /// a metadata object, read by an SDBL query. `object_name` shares the `Mdo`
    /// canonicalisation; `attr_name` is the declared metadata field name.
    Attribute {
        mdo_type: MdoType,
        object_name: Name,
        attr_name: Name,
    },
    /// A managed/ordinary form. Owned by a metadata object (`owner = Some((type,
    /// object))`) or a common form (`owner = None`). `form_name` is the form's
    /// directory name in the source tree (edit-stable, single-spelling). The owner
    /// object name shares the `Mdo` canonicalisation.
    Form {
        owner: Option<(MdoType, Name)>,
        form_name: Name,
    },
    /// An item (control/group/field) on a form, identified by its declared element
    /// name. `owner`/`form_name` mirror the containing [`GraphNode::Form`].
    FormItem {
        owner: Option<(MdoType, Name)>,
        form_name: Name,
        item_name: Name,
    },
    /// A form attribute — an entry in the form's data model (distinct from the UI
    /// [`GraphNode::FormItem`] controls). `owner`/`form_name` mirror the containing
    /// [`GraphNode::Form`]; `attr_name` is the declared form-attribute name.
    FormAttribute {
        owner: Option<(MdoType, Name)>,
        form_name: Name,
        attr_name: Name,
    },
    /// A tabular section (табличная часть) of a metadata object. `object_name` shares
    /// the [`GraphNode::Mdo`] canonicalisation; `section_name` is the declared name.
    TabularSection {
        mdo_type: MdoType,
        object_name: Name,
        section_name: Name,
    },
    /// A column of a tabular section, reached through its [`GraphNode::TabularSection`]
    /// (the `<object>.<section>.<column>` hierarchy). Distinct identity from a
    /// top-level [`GraphNode::Attribute`] of the same object.
    TabularSectionAttribute {
        mdo_type: MdoType,
        object_name: Name,
        section_name: Name,
        attr_name: Name,
    },
}

/// Where in the caller's text an edge's call sits, as the build found it.
///
/// Three states, not an `Option<TextRange>`: a consumer asking a graph edge for its place
/// gets one of two different absences, and collapsing them would teach it to stop expecting
/// a span that a later build can supply. Which state an edge carries is decided by the pass
/// that produced it, never by its [`EdgeKind`] — the kind of a call read out of a body is
/// reassigned during resolution, so classifying by kind would call a recorded span absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSite {
    /// Read out of a body, span kept.
    Recorded(TextRange),
    /// The edge stands for something written in code whose span this build does not keep.
    NotRecorded,
    /// Derived from metadata rather than from code: there is no span to keep.
    Structural,
}

/// The location contract's code for an edge that stands for no call in code at all.
/// Spelled here as a literal rather than imported from the serving layer, which sits far
/// above this crate; the serving layer asserts the two agree.
pub const NO_CALL_SITE: &str = "no_call_site";

/// The location contract's code for an edge that DOES stand for something written in code,
/// whose span this build does not keep.
pub const CALL_SITE_NOT_RECORDED: &str = "call_site_not_recorded";

/// A resolved call edge between two workspace graph nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCallEdge {
    pub from: GraphNode,
    pub to: GraphNode,
    pub kind: EdgeKind,
    pub provenance: EdgeProvenance,
    /// Where the call is written, carried to the artefact so a served edge can name its
    /// place without re-lowering the caller's module.
    pub call_site: CallSite,
    /// A client-capable caller invoking a server-only callee — a client→server
    /// roundtrip. Dispatch comes from the module's execution context (common
    /// modules) or per-method `&НаКлиенте`/`&НаСервере` annotations (form/command
    /// modules). `false` when either endpoint's dispatch is unknown (e.g. a
    /// `ModuleCode` caller, or a module with `Unknown` context and no annotation).
    pub crosses_client_to_server: bool,
}

/// Whole-config call graph: forward (callees) and reverse (callers) adjacency
/// over the resolved edges of every module, plus per-method client/server
/// dispatch. Produced by `workspace_call_graph_query`. Only edges whose target
/// resolved to a concrete node are indexed; unresolved outgoing calls stay
/// visible per-module via `ResolvedModuleSummary`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceCallGraph {
    forward: FxHashMap<GraphNode, Vec<WorkspaceCallEdge>>,
    reverse: FxHashMap<GraphNode, Vec<WorkspaceCallEdge>>,
    node_dispatch: FxHashMap<GraphNode, MethodDispatch>,
}

impl WorkspaceCallGraph {
    pub fn insert(&mut self, edge: WorkspaceCallEdge) {
        self.forward.entry(edge.from.clone()).or_default().push(edge.clone());
        self.reverse.entry(edge.to.clone()).or_default().push(edge);
    }

    pub fn set_dispatch(&mut self, node: GraphNode, dispatch: MethodDispatch) {
        self.node_dispatch.insert(node, dispatch);
    }

    /// Client/server dispatch of a node, if known (method nodes only).
    pub fn dispatch(&self, node: &GraphNode) -> Option<MethodDispatch> {
        self.node_dispatch.get(node).copied()
    }

    /// Outgoing resolved calls from `node` (callees).
    pub fn callees(&self, node: &GraphNode) -> &[WorkspaceCallEdge] {
        self.forward.get(node).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Incoming resolved calls to `node` (callers).
    pub fn callers(&self, node: &GraphNode) -> &[WorkspaceCallEdge] {
        self.reverse.get(node).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every node that participates in the graph: it has an outgoing edge, an
    /// incoming edge, or a known dispatch. Order is unspecified.
    pub fn nodes(&self) -> impl Iterator<Item = GraphNode> + '_ {
        let mut seen = FxHashMap::default();
        self.forward
            .keys()
            .chain(self.reverse.keys())
            .chain(self.node_dispatch.keys())
            .filter(move |node| seen.insert((*node).clone(), ()).is_none())
            .cloned()
    }

    /// Every resolved edge in the graph (each edge appears once). Order is unspecified.
    pub fn edges(&self) -> impl Iterator<Item = &WorkspaceCallEdge> + '_ {
        self.forward.values().flat_map(|edges| edges.iter())
    }

    /// Number of resolved edges.
    pub fn edge_count(&self) -> usize {
        self.forward.values().map(Vec::len).sum()
    }

    /// Caller-in centrality: how many resolved calls target `node`.
    pub fn in_degree(&self, node: &GraphNode) -> usize {
        self.reverse.get(node).map_or(0, Vec::len)
    }
}

/// Heap bytes owned by a [`GraphNode`]'s `Name` fields (their `SmolStr`
/// payloads when not inlined). Every variant is enumerated so a new node kind
/// with a `Name` field cannot silently go uncounted.
fn graph_node_name_heap(node: &GraphNode) -> usize {
    use crate::heap_estimate::name_bytes;

    match node {
        GraphNode::Method(_) | GraphNode::ModuleCode(_) => 0,
        GraphNode::Mdo { object_name, .. } => name_bytes(object_name),
        GraphNode::Attribute { object_name, attr_name, .. } => {
            name_bytes(object_name) + name_bytes(attr_name)
        }
        GraphNode::Form { owner, form_name } => {
            owner.as_ref().map_or(0, |(_, name)| name_bytes(name)) + name_bytes(form_name)
        }
        GraphNode::FormItem { owner, form_name, item_name } => {
            owner.as_ref().map_or(0, |(_, name)| name_bytes(name))
                + name_bytes(form_name)
                + name_bytes(item_name)
        }
        GraphNode::FormAttribute { owner, form_name, attr_name } => {
            owner.as_ref().map_or(0, |(_, name)| name_bytes(name))
                + name_bytes(form_name)
                + name_bytes(attr_name)
        }
        GraphNode::TabularSection { object_name, section_name, .. } => {
            name_bytes(object_name) + name_bytes(section_name)
        }
        GraphNode::TabularSectionAttribute { object_name, section_name, attr_name, .. } => {
            name_bytes(object_name) + name_bytes(section_name) + name_bytes(attr_name)
        }
    }
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the
/// `forward`/`reverse` adjacency tables (each key's node `Name`s plus its
/// edge list, and each edge's endpoint `Name`s) and the `node_dispatch`
/// table (its key `Name`s; [`MethodDispatch`] itself is `Copy`). Lives here
/// (not in `queries::heap_estimate`) because the fields it reads are
/// private to this module. New heap-owning fields must be added here too.
pub(crate) fn workspace_call_graph_heap(v: &std::sync::Arc<WorkspaceCallGraph>) -> usize {
    use crate::heap_estimate::{map_table_bytes, vec_bytes};

    let g = &**v;
    let mut bytes = std::mem::size_of::<WorkspaceCallGraph>();

    let adjacency_heap = |table: &FxHashMap<GraphNode, Vec<WorkspaceCallEdge>>| -> usize {
        let mut b = map_table_bytes::<GraphNode, Vec<WorkspaceCallEdge>>(table.len());
        for (node, edges) in table {
            b += graph_node_name_heap(node);
            b += vec_bytes::<WorkspaceCallEdge>(edges.len());
            for edge in edges {
                b += graph_node_name_heap(&edge.from) + graph_node_name_heap(&edge.to);
            }
        }
        b
    };

    bytes += adjacency_heap(&g.forward);
    bytes += adjacency_heap(&g.reverse);

    bytes += map_table_bytes::<GraphNode, MethodDispatch>(g.node_dispatch.len());
    for node in g.node_dispatch.keys() {
        bytes += graph_node_name_heap(node);
    }

    bytes
}

/// Enumerate a module's methods from the item tree, in declaration order.
/// Reads declarations only and never lowers bodies — cheap enough to run over
/// a whole configuration without the RAM cost of body HIR.
pub fn extract_graph_methods(item_tree: &ItemTree) -> Vec<GraphMethodEntry> {
    item_tree
        .methods()
        .map(|method| GraphMethodEntry {
            local_id: method.key(),
            name: method.name().clone(),
            is_export: method.is_export(),
            dispatch: MethodDispatch::from_annotation(
                method.annotations().first().map(|a| &a.kind),
            ),
            signature_hash: 0,
            name_range: method.name_range(),
            sig_end: method.sig_end(),
            source_range: method.source_range(),
        })
        .collect()
}

pub fn extract_call_summary(
    item_tree: &ItemTree,
    module_bodies: &ModuleBodies,
    form_event_handlers: &[bsl_metadata::FormEventHandler],
) -> ModuleCallSummary {
    // Method enumeration is the cheap, body-free part — share it with the graph
    // build. The `local_method_ids` map (lowercased name → first local id) is what
    // body extraction uses to bind local calls.
    let graph_methods = extract_graph_methods(item_tree);
    let mut local_method_ids: FxHashMap<NormName, MethodKey> = FxHashMap::default();
    let mut methods = Vec::with_capacity(graph_methods.len());
    for method in &graph_methods {
        local_method_ids.entry(method.local_id.name).or_insert(method.local_id);
        methods.push(MethodSummary {
            local_id: method.local_id,
            name: method.name.clone(),
            dispatch: method.dispatch,
            is_export: method.is_export,
        });
    }

    let mut call_edges = Vec::new();
    let mut notify_regs = Vec::new();
    let mut idle_handler_regs = Vec::new();
    let mut set_action_regs = Vec::new();
    let mut name_literals: FxHashSet<MethodKey> = FxHashSet::default();

    // Bodies come in item order, which is deterministic.
    for (local_id, lower_result) in module_bodies.iter_lower_results() {
        extract_from_body(
            lower_result.body(),
            lower_result.source_map(),
            CallerId::Method(local_id),
            &local_method_ids,
            &mut call_edges,
            &mut notify_regs,
            &mut idle_handler_regs,
            &mut set_action_regs,
            &mut name_literals,
        );
    }

    if let Some(module_code) = module_bodies.module_code_result() {
        extract_from_body(
            module_code.body(),
            module_code.source_map(),
            CallerId::ModuleCode,
            &local_method_ids,
            &mut call_edges,
            &mut notify_regs,
            &mut idle_handler_regs,
            &mut set_action_regs,
            &mut name_literals,
        );
    }

    let mut name_literal_refs: Vec<MethodKey> = name_literals.into_iter().collect();
    name_literal_refs.sort_unstable();

    let form_entries = form_event_handlers
        .iter()
        .map(|h| FormEventEntry {
            event_type: h.event_type.clone(),
            handler_name: Name::new(&h.handler_name),
        })
        .collect();

    ModuleCallSummary {
        methods,
        call_edges,
        notify_regs,
        idle_handler_regs,
        set_action_regs,
        name_literal_refs,
        form_entries,
    }
}

// One &mut accumulator per output list (call edges + the three string-named dispatch
// registries) plus the read-only inputs; bundling them into a struct would only move the
// same fields behind one more indirection.
#[allow(clippy::too_many_arguments)]
fn extract_from_body(
    body: &Body,
    source_map: crate::body::SourceMapAt<'_>,
    caller: CallerId,
    local_method_ids: &FxHashMap<NormName, MethodKey>,
    call_edges: &mut Vec<CallEdge>,
    notify_regs: &mut Vec<NotifyReg>,
    idle_handler_regs: &mut Vec<IdleReg>,
    set_action_regs: &mut Vec<SetActionReg>,
    name_literals: &mut FxHashSet<MethodKey>,
) {
    let common_bindings = crate::common_module_ref::common_module_var_bindings(body);
    for (expr_id, expr) in body.exprs_iter() {
        match expr {
            Expr::Literal(Literal::String(s)) if is_identifier_like(s) => {
                if let Some(&local_id) = local_method_ids.get(&NormName::intern(s)) {
                    name_literals.insert(local_id);
                }
            }
            Expr::Call { callee, args } => {
                let callee_expr = body.expr_idx(*callee);
                let range = source_map.expr_range(expr_id).unwrap_or(TextRange::empty(0.into()));

                match callee_expr {
                    Expr::Path(name) => {
                        let name_lower = name.as_str().fold_lower();

                        if is_attach_idle_handler(&name_lower) {
                            if let Some(reg) = extract_idle_reg(body, caller, args, range) {
                                idle_handler_regs.push(reg);
                            }
                        } else if let Some(&callee_local_id) =
                            local_method_ids.get(&NormName::intern(name.as_str()))
                        {
                            call_edges.push(CallEdge {
                                caller,
                                target: CallTarget::Local { callee_local_id },
                                kind: EdgeKind::DirectLocal,
                                range,
                            });
                        }
                    }
                    Expr::Field { base: field_base, field } => {
                        if is_set_action(&field.as_str().fold_lower())
                            && is_form_element_action_receiver(body, *field_base)
                        {
                            if let Some(reg) = extract_set_action_reg(body, caller, args, range) {
                                set_action_regs.push(reg);
                            }
                        }
                        if let Some(edge) = field_callee_to_edge(
                            body,
                            caller,
                            *field_base,
                            field,
                            range,
                            local_method_ids,
                            &common_bindings,
                        ) {
                            call_edges.push(edge);
                        }
                    }
                    _ => {}
                }
            }
            Expr::New { type_name, args } => {
                // `Новый ОписаниеОповещения(ИмяПроцедуры, Модуль, ДополнительныеПараметры,
                // ИмяПроцедурыОбработкиОшибки, МодульОбработкиОшибки)`. `base` is the index of
                // the first constructor argument: 0 for the typed form, 1 for the string-first
                // form `Новый("ОписаниеОповещения", …)` where arg 0 is the type name.
                let base = match type_name {
                    Some(tn) if is_notify_description(tn) => Some(0usize),
                    None if !args.is_empty() => match body.expr_idx(args[0]) {
                        Expr::Literal(Literal::String(tn)) if is_notify_description_str(tn) => {
                            Some(1usize)
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(base) = base {
                    let range =
                        source_map.expr_range(expr_id).unwrap_or(TextRange::empty(0.into()));
                    // The primary handler (ИмяПроцедуры, Модуль) and the error handler
                    // (ИмяПроцедурыОбработкиОшибки, МодульОбработкиОшибки) are two independent
                    // dispatch targets: the platform invokes the second when the async call
                    // fails. A constructor that omits the error handler simply yields no second
                    // reg (the args are absent), so this never invents an edge.
                    for (method_idx, target_idx) in [(base, base + 1), (base + 3, base + 4)] {
                        if let Some(reg) =
                            extract_notify_reg_at(body, caller, args, method_idx, target_idx, range)
                        {
                            notify_regs.push(reg);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_attach_idle_handler(name_lower: &str) -> bool {
    name_lower == "подключитьобработчикожидания" || name_lower == "attachidlehandler"
}

fn is_set_action(name_lower: &str) -> bool {
    name_lower == "установитьдействие" || name_lower == "setaction"
}

/// Whether a string literal is shaped like a BSL identifier and could therefore name a
/// method. Filters message texts and data values out of [`ModuleCallSummary::name_literal_refs`]
/// before the (case-folded) comparison against local method names. Mirrors the lexer's
/// identifier grammar: leading Unicode letter or `_`, then letters, ASCII digits, or `_`
/// (the lexer accepts only `0-9` after the first char, not other Unicode digit classes).
fn is_identifier_like(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphabetic() || c.is_ascii_digit() || c == '_')
}

fn is_notify_description(name: &Name) -> bool {
    is_notify_description_str(name.as_str())
}

fn is_notify_description_str(name: &str) -> bool {
    let lower = name.fold_lower();
    lower == "описаниеоповещения" || lower == "notifydescription"
}

fn is_this_object(name_lower: &str) -> bool {
    name_lower == "этотобъект" || name_lower == "thisobject"
}

/// Recognise the `Движения` (RU) / `RegisterRecords` (EN) collection of a document's
/// register records, whether named bare (`Движения`) or reached through a receiver
/// (`Об.Движения`, `ЭтотОбъект.Движения`). The last path segment is what matters, so both
/// a plain identifier and a field access are accepted.
fn is_register_records(expr: &Expr) -> bool {
    let segment = match expr {
        Expr::Path(name) => name.as_str(),
        Expr::Field { field, .. } => field.as_str(),
        _ => return false,
    };
    let lower = segment.fold_lower();
    lower == "движения" || lower == "registerrecords"
}

/// A self-receiver for a callback registration: the object form (`ЭтотОбъект`) or the
/// managed-form form (`ЭтаФорма`). Both mean "the handler lives in the current module".
/// Wider than [`is_this_object`] because `ОписаниеОповещения` is most common in form
/// modules, where `ЭтаФорма` is the idiomatic receiver.
fn is_this_receiver(name_lower: &str) -> bool {
    is_this_object(name_lower) || name_lower == "этаформа" || name_lower == "thisform"
}

fn extract_idle_reg(
    body: &Body,
    caller: CallerId,
    args: &[ExprIdx],
    range: TextRange,
) -> Option<IdleReg> {
    if args.is_empty() {
        return None;
    }
    let handler_name = extract_string_literal(body, args[0])?;
    let one_shot = args
        .get(2)
        .and_then(|&idx| match body.expr_idx(idx) {
            Expr::Literal(Literal::Bool(v)) => Some(*v),
            _ => None,
        })
        .unwrap_or(false);
    Some(IdleReg { caller, handler_name: Name::new(&handler_name), one_shot, range })
}

/// Whether the receiver of `УстановитьДействие` plausibly denotes a form element (so the
/// second string argument is an event-handler method name). `УстановитьДействие` is not a
/// reserved name — a user can export a manager/object method by that spelling — so a
/// manager access such as `Справочники.Номенклатура.УстановитьДействие("Опция", "Имя")` must
/// NOT be read as a handler binding (its `"Имя"` is plain data).
///
/// Accepted: a bare path (a form element held in a variable, or `ЭтаФорма`/`ЭтотОбъект`) and
/// any `Элементы[...]`/`Элементы.X` access. Rejected: a multi-level access rooted elsewhere
/// (manager types like `Справочники`/`Документы`, or a qualified module). This keeps the
/// real ERP shapes (`ЭлементФормы.УстановитьДействие`, `Элементы["Кол"+п].УстановитьДействие`,
/// `ЭтаФорма.Элементы.X.УстановитьДействие`) while excluding manager-method calls.
fn is_form_element_action_receiver(body: &Body, receiver: ExprIdx) -> bool {
    match body.expr_idx(receiver) {
        Expr::Path(_) => true,
        Expr::Field { base, .. } | Expr::Index { base, .. } => is_form_items_rooted(body, *base),
        _ => false,
    }
}

/// Whether an access chain is rooted at the form-items collection (`Элементы` / `Items`).
fn is_form_items_rooted(body: &Body, idx: ExprIdx) -> bool {
    match body.expr_idx(idx) {
        Expr::Path(name) => {
            let lower = name.as_str().fold_lower();
            lower == "элементы" || lower == "items"
        }
        Expr::Field { base, field } => {
            let lower = field.as_str().fold_lower();
            lower == "элементы" || lower == "items" || is_form_items_rooted(body, *base)
        }
        Expr::Index { base, .. } => is_form_items_rooted(body, *base),
        _ => false,
    }
}

/// `<ЭлементФормы>.УстановитьДействие("Событие", "Обработчик")` — the handler is the
/// second argument's string literal (the first is the event name). A non-literal handler
/// (a variable) yields nothing rather than an invented reg.
fn extract_set_action_reg(
    body: &Body,
    caller: CallerId,
    args: &[ExprIdx],
    range: TextRange,
) -> Option<SetActionReg> {
    let handler_name = extract_string_literal(body, *args.get(1)?)?;
    Some(SetActionReg { caller, handler_name: Name::new(&handler_name), range })
}

fn extract_notify_reg_at(
    body: &Body,
    caller: CallerId,
    args: &[ExprIdx],
    method_idx: usize,
    target_idx: usize,
    range: TextRange,
) -> Option<NotifyReg> {
    let callback_name = extract_string_literal(body, *args.get(method_idx)?)?;
    let target = match args.get(target_idx).map(|&idx| body.expr_idx(idx)) {
        Some(Expr::Path(name)) if is_this_receiver(&name.as_str().fold_lower()) => {
            NotifyTarget::ThisObject
        }
        Some(Expr::Path(name)) => NotifyTarget::Module(name.clone()),
        _ => NotifyTarget::Unsupported,
    };
    Some(NotifyReg { caller, callback_name: Name::new(&callback_name), target, range })
}

fn field_callee_to_edge(
    body: &Body,
    caller: CallerId,
    field_base: ExprIdx,
    field: &Name,
    range: TextRange,
    local_method_ids: &FxHashMap<NormName, MethodKey>,
    common_bindings: &FxHashMap<String, Name>,
) -> Option<CallEdge> {
    match body.expr_idx(field_base) {
        Expr::Path(module_name) => {
            let module_name_lower = module_name.as_str().fold_lower();
            if is_this_object(&module_name_lower) {
                let target = if let Some(&callee_local_id) =
                    local_method_ids.get(&NormName::intern(field.as_str()))
                {
                    CallTarget::Local { callee_local_id }
                } else {
                    tracing::debug!(
                        method_name = field.as_str(),
                        "Unresolved this-object method call in call graph extraction"
                    );
                    CallTarget::ThisObjectMethod { method_name: field.clone() }
                };
                Some(CallEdge { caller, target, kind: EdgeKind::DirectLocal, range })
            } else {
                // A receiver bound to `ОбщегоНазначения.ОбщийМодуль("Имя")` is the named
                // common module, not the (meaningless) variable name it is held in.
                let resolved_module =
                    common_bindings.get(&module_name_lower).unwrap_or(module_name);
                Some(CallEdge {
                    caller,
                    target: CallTarget::QualifiedModule {
                        module_name: resolved_module.clone(),
                        method_name: field.clone(),
                    },
                    kind: EdgeKind::DirectQualifiedModule,
                    range,
                })
            }
        }
        Expr::Field { base: inner_base, field: inner_field } => {
            let inner = body.expr_idx(*inner_base);
            // `[получатель.]Движения.<Регистр>.<метод>()` — a document movement write/read.
            // The `Движения` collection is reached either bare (implicit `ЭтотОбъект`) or
            // through a receiver (`Об.Движения`, `ЭтотОбъект.Движения`); either way the
            // register name is `inner_field`. Its metadata type is resolved from config later.
            if is_register_records(inner) {
                return Some(CallEdge {
                    caller,
                    target: CallTarget::RegisterMovement { register_name: inner_field.clone() },
                    kind: EdgeKind::RegisterMovement,
                    range,
                });
            }
            if let Expr::Path(mdo_type_name) = inner {
                let target =
                    if let Some(manager_type) = ManagerType::from_name(mdo_type_name.as_str()) {
                        CallTarget::ManagerAccess {
                            manager_type,
                            object_name: inner_field.clone(),
                            method_name: Some(field.clone()),
                        }
                    } else {
                        CallTarget::Unresolved
                    };
                Some(CallEdge { caller, target, kind: EdgeKind::DirectQualifiedModule, range })
            } else {
                None
            }
        }
        Expr::Index { base, index } => {
            // `[получатель.]Движения[<имя>].<метод>()` — a document movement touch addressed by
            // a dynamic index rather than a literal path segment. Same relation as the literal
            // `Движения.<Регистр>.<метод>()` (the register's metadata type is resolved later), so
            // it reuses `RegisterMovement`. Only a locally-literal index resolves: a string
            // literal register name or a `Метаданные.<РегистрыКоллекция>.<X>.Имя` chain. A
            // variable index needs value flow and is left to a later tier.
            if is_register_records(body.expr_idx(*base)) {
                if let Some(register_name) = extract_literal_register_name(body, *index) {
                    return Some(CallEdge {
                        caller,
                        target: CallTarget::RegisterMovement { register_name },
                        kind: EdgeKind::RegisterMovement,
                        range,
                    });
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract a locally-literal register name from a `Движения[<expr>]` index: a string literal
/// (`Движения["ТоварыНаСкладах"]`) or a `Метаданные.<РегистрыКоллекция>.<X>.Имя` chain
/// (`Движения[Метаданные.РегистрыНакопления.СебестоимостьТоваров.Имя]`). Any other expression —
/// notably a variable — yields `None`; resolving it needs value flow (a later tier).
fn extract_literal_register_name(body: &Body, idx: ExprIdx) -> Option<Name> {
    match body.expr_idx(idx) {
        Expr::Literal(Literal::String(s)) => Some(Name::new(s)),
        Expr::Field { base, field } if is_name_property(field) => {
            let Expr::Field { base: coll_base, field: register_name } = body.expr_idx(*base) else {
                return None;
            };
            let Expr::Field { base: meta_base, field: collection } = body.expr_idx(*coll_base)
            else {
                return None;
            };
            let is_register_collection =
                ManagerType::from_name(collection.as_str()).is_some_and(ManagerType::is_register);
            (is_register_collection && is_metadata_root(body.expr_idx(*meta_base)))
                .then(|| register_name.clone())
        }
        _ => None,
    }
}

fn is_name_property(field: &Name) -> bool {
    let lower = field.as_str().fold_lower();
    lower == "имя" || lower == "name"
}

fn is_metadata_root(expr: &Expr) -> bool {
    matches!(expr, Expr::Path(name) if {
        let lower = name.as_str().fold_lower();
        lower == "метаданные" || lower == "metadata"
    })
}

fn extract_string_literal(body: &Body, idx: ExprIdx) -> Option<String> {
    match body.expr_idx(idx) {
        Expr::Literal(Literal::String(s)) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_pair_subset_keeps_only_pair_relevant_facts() {
        // Given: a summary mixing pair-relevant call facts with everything the
        // method-only hierarchy ignores, plus duplicate call sites at distinct ranges.
        let range = |start: u32| TextRange::new(TextSize::from(start), TextSize::from(start + 1));
        let edge =
            |caller, target, kind, start| CallEdge { caller, target, kind, range: range(start) };
        let qualified = || CallTarget::QualifiedModule {
            module_name: Name::new("Сервер"),
            method_name: Name::new("Считать"),
        };
        let summary = ModuleCallSummary {
            methods: vec![MethodSummary {
                local_id: MethodKey::first("Главная"),
                name: Name::new("Главная"),
                dispatch: MethodDispatch::from_annotation(None),
                is_export: true,
            }],
            call_edges: vec![
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::Local { callee_local_id: MethodKey::first("М1") },
                    EdgeKind::DirectLocal,
                    0,
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    qualified(),
                    EdgeKind::DirectQualifiedModule,
                    10,
                ),
                // A repeated call site: same fact, different range.
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    qualified(),
                    EdgeKind::DirectQualifiedModule,
                    20,
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::ManagerAccess {
                        manager_type: ManagerType::Catalogs,
                        object_name: Name::new("Контрагенты"),
                        method_name: Some(Name::new("НайтиПоИНН")),
                    },
                    EdgeKind::DirectQualifiedModule,
                    30,
                ),
                // Dropped: module-code caller, method-less manager touch, movement.
                edge(
                    CallerId::ModuleCode,
                    CallTarget::Local { callee_local_id: MethodKey::first("М1") },
                    EdgeKind::DirectLocal,
                    40,
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::ManagerAccess {
                        manager_type: ManagerType::Catalogs,
                        object_name: Name::new("Контрагенты"),
                        method_name: None,
                    },
                    EdgeKind::DirectQualifiedModule,
                    50,
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::RegisterMovement { register_name: Name::new("Продажи") },
                    EdgeKind::RegisterMovement,
                    60,
                ),
            ],
            notify_regs: vec![
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_name: Name::new("Обработчик"),
                    target: NotifyTarget::ThisObject,
                    range: range(70),
                },
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_name: Name::new("Обработчик"),
                    target: NotifyTarget::Unsupported,
                    range: range(80),
                },
            ],
            idle_handler_regs: vec![
                IdleReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    handler_name: Name::new("ОбновитьЭкран"),
                    one_shot: true,
                    range: range(90),
                },
                // A duplicate that differs only in one_shot/range: same resolved pair.
                IdleReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    handler_name: Name::new("ОбновитьЭкран"),
                    one_shot: false,
                    range: range(100),
                },
            ],
            set_action_regs: vec![SetActionReg {
                caller: CallerId::Method(MethodKey::first("Главная")),
                handler_name: Name::new("ОбработчикДействия"),
                range: range(110),
            }],
            name_literal_refs: vec![MethodKey::first("М1")],
            form_entries: Vec::new(),
        };

        // When: the pair-relevant subset is taken.
        let subset = summary.method_pair_subset();

        // Then: only method-caller edges whose target can resolve to a user method
        // survive, deduplicated with zeroed ranges, and non-pair domains are empty.
        let zero = TextRange::empty(TextSize::from(0));
        assert_eq!(
            subset.call_edges,
            vec![
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::Local { callee_local_id: MethodKey::first("М1") },
                    EdgeKind::DirectLocal,
                    0
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    qualified(),
                    EdgeKind::DirectQualifiedModule,
                    0
                ),
                edge(
                    CallerId::Method(MethodKey::first("Главная")),
                    CallTarget::ManagerAccess {
                        manager_type: ManagerType::Catalogs,
                        object_name: Name::new("Контрагенты"),
                        method_name: Some(Name::new("НайтиПоИНН")),
                    },
                    EdgeKind::DirectQualifiedModule,
                    0,
                ),
            ]
            .into_iter()
            .map(|edge| CallEdge { range: zero, ..edge })
            .collect::<Vec<_>>(),
        );
        assert_eq!(
            subset.notify_regs,
            vec![NotifyReg {
                caller: CallerId::Method(MethodKey::first("Главная")),
                callback_name: Name::new("Обработчик"),
                target: NotifyTarget::ThisObject,
                range: zero,
            }],
        );
        assert_eq!(
            subset.idle_handler_regs,
            vec![IdleReg {
                caller: CallerId::Method(MethodKey::first("Главная")),
                handler_name: Name::new("ОбновитьЭкран"),
                one_shot: false,
                range: zero,
            }],
        );
        assert!(subset.methods.is_empty());
        assert!(subset.set_action_regs.is_empty());
        assert!(subset.name_literal_refs.is_empty());
        assert!(subset.form_entries.is_empty());
    }

    #[test]
    fn dispatch_from_execution_context_maps_each_variant() {
        use crate::ExecutionContext;

        for ctx in [
            ExecutionContext::Server,
            ExecutionContext::ServerCall,
            ExecutionContext::ExternalConnection,
        ] {
            let d = MethodDispatch::from_execution_context(ctx).unwrap();
            assert!(d.is_server_only(), "{ctx:?} is server-only");
        }

        let client = MethodDispatch::from_execution_context(ExecutionContext::Client).unwrap();
        assert!(client.can_run_on_client && !client.can_run_on_server);

        let both = MethodDispatch::from_execution_context(ExecutionContext::ClientServer).unwrap();
        assert!(both.can_run_on_client && both.can_run_on_server);
        assert!(!both.is_server_only());

        // Unknown → no module-level dispatch; caller falls back to annotations.
        assert!(MethodDispatch::from_execution_context(ExecutionContext::Unknown).is_none());
    }

    #[test]
    fn test_dispatch_from_annotation() {
        let d = MethodDispatch::from_annotation(None);
        assert!(d.can_run_on_client);
        assert!(!d.can_run_on_server);
        assert!(!d.is_server_only());

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::AtServer));
        assert!(!d.can_run_on_client);
        assert!(d.can_run_on_server);
        assert!(d.is_server_only());

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::AtClient));
        assert!(d.can_run_on_client);
        assert!(!d.can_run_on_server);
        assert!(!d.is_server_only());

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::AtClientAtServer));
        assert!(d.can_run_on_client);
        assert!(d.can_run_on_server);
        assert!(!d.is_server_only());

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::AtServerNoContext));
        assert!(!d.can_run_on_client);
        assert!(d.can_run_on_server);
        assert!(d.is_server_only());
        assert!(d.no_context);

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::AtClientAtServerNoContext));
        assert!(d.can_run_on_client);
        assert!(d.can_run_on_server);
        assert!(!d.is_server_only());
        assert!(d.no_context);

        let d = MethodDispatch::from_annotation(Some(&AnnotationKind::Before));
        assert!(d.can_run_on_client);
        assert!(!d.can_run_on_server);
    }

    #[test]
    fn test_is_attach_idle_handler() {
        assert!(is_attach_idle_handler("подключитьобработчикожидания"));
        assert!(is_attach_idle_handler("attachidlehandler"));
        assert!(!is_attach_idle_handler("отключитьобработчикожидания"));
    }

    #[test]
    fn test_is_notify_description() {
        assert!(is_notify_description(&Name::new("ОписаниеОповещения")));
        assert!(is_notify_description(&Name::new("NotifyDescription")));
        assert!(is_notify_description(&Name::new("описаниеоповещения")));
        assert!(!is_notify_description(&Name::new("ОписаниеОшибки")));
    }

    #[test]
    fn test_is_this_object() {
        assert!(is_this_object("этотобъект"));
        assert!(is_this_object("thisobject"));
        assert!(!is_this_object("другойобъект"));
    }

    #[test]
    fn test_manager_type_from_name() {
        assert_eq!(ManagerType::from_name("Документы"), Some(ManagerType::Documents));
        assert_eq!(ManagerType::from_name("Documents"), Some(ManagerType::Documents));
        assert_eq!(ManagerType::from_name("документы"), Some(ManagerType::Documents));
        assert_eq!(ManagerType::from_name("Справочники"), Some(ManagerType::Catalogs));
        assert_eq!(ManagerType::from_name("catalogs"), Some(ManagerType::Catalogs));
        assert_eq!(ManagerType::from_name("НеизвестныйТип"), None);
    }

    #[test]
    fn extract_graph_methods_reports_ranges_dispatch_and_export() {
        let code = "&НаСервере\n\
                    Функция Считать() Экспорт\n\
                    Возврат 1;\n\
                    КонецФункции\n\
                    \n\
                    Процедура Делать()\n\
                    КонецПроцедуры";
        let parse = parser::parse(code);
        let item_tree = ItemTree::from_parse(&parse);
        let methods = extract_graph_methods(&item_tree);
        assert_eq!(methods.len(), 2);

        let read = &methods[0];
        assert_eq!(read.local_id, MethodKey::first("Считать"));
        assert_eq!(read.name.as_str(), "Считать");
        assert!(read.is_export);
        assert!(read.dispatch.is_server_only());
        // The name range pinpoints the identifier; the source range spans the
        // whole declaration — both index back into the original text.
        let name_slice =
            &code[usize::from(read.name_range.start())..usize::from(read.name_range.end())];
        assert_eq!(name_slice, "Считать");
        let source_slice =
            &code[usize::from(read.source_range.start())..usize::from(read.source_range.end())];
        assert!(source_slice.contains("Функция Считать"));
        assert!(source_slice.contains("КонецФункции"));

        let act = &methods[1];
        assert_eq!(act.name.as_str(), "Делать");
        assert!(!act.is_export);
        assert!(act.dispatch.can_run_on_client && !act.dispatch.can_run_on_server);
    }

    #[test]
    fn extract_graph_methods_matches_call_summary_method_list() {
        // The two enumerations must agree: `extract_call_summary` reuses
        // `extract_graph_methods`, so the body-free index is the source of truth
        // for local ids, names, dispatch, and export.
        let code = "Процедура Альфа() Экспорт\n\
                    КонецПроцедуры\n\
                    Функция Бета()\n\
                    Возврат 0;\n\
                    КонецФункции";
        let parse = parser::parse(code);
        let item_tree = ItemTree::from_parse(&parse);
        let entries = extract_graph_methods(&item_tree);
        let summary = parse_and_extract(code);

        assert_eq!(entries.len(), summary.methods.len());
        for (entry, summary_method) in entries.iter().zip(&summary.methods) {
            assert_eq!(entry.local_id, summary_method.local_id);
            assert_eq!(entry.name, summary_method.name);
            assert_eq!(entry.is_export, summary_method.is_export);
            assert_eq!(entry.dispatch, summary_method.dispatch);
        }
    }

    #[test]
    fn test_extract_call_summary_empty_module() {
        let item_tree = ItemTree::default();
        let module_bodies = ModuleBodies::new();
        let summary = extract_call_summary(&item_tree, &module_bodies, &[]);

        assert!(summary.methods.is_empty());
        assert!(summary.call_edges.is_empty());
        assert!(summary.notify_regs.is_empty());
        assert!(summary.idle_handler_regs.is_empty());
        assert!(summary.form_entries.is_empty());
    }

    #[test]
    fn test_extract_form_entries() {
        let item_tree = ItemTree::default();
        let module_bodies = ModuleBodies::new();
        let handlers = vec![
            bsl_metadata::FormEventHandler {
                event_type: "OnCreateAtServer".to_string(),
                handler_name: "ПриСозданииНаСервере".to_string(),
            },
            bsl_metadata::FormEventHandler {
                event_type: "OnActivateRow".to_string(),
                handler_name: "СписокПриАктивизацииСтроки".to_string(),
            },
        ];
        let summary = extract_call_summary(&item_tree, &module_bodies, &handlers);

        assert_eq!(summary.form_entries.len(), 2);
        assert_eq!(summary.form_entries[0].event_type, "OnCreateAtServer");
        assert_eq!(summary.form_entries[0].handler_name, Name::new("ПриСозданииНаСервере"));
        assert_eq!(summary.form_entries[1].event_type, "OnActivateRow");
    }

    fn parse_and_extract(code: &str) -> ModuleCallSummary {
        parse_and_extract_with_handlers(code, &[])
    }

    fn parse_and_extract_with_handlers(
        code: &str,
        handlers: &[bsl_metadata::FormEventHandler],
    ) -> ModuleCallSummary {
        let parse = parser::parse(code);
        let item_tree = ItemTree::from_parse(&parse);
        let module_bodies = ModuleBodies::from_parse(&parse);
        extract_call_summary(&item_tree, &module_bodies, handlers)
    }

    #[test]
    fn test_handler_to_local_client_to_server_chain() {
        let code = r#"
&НаКлиенте
Процедура ОбработчикСобытия()
    КлиентскийМетод();
КонецПроцедуры

&НаКлиенте
Процедура КлиентскийМетод()
    СерверныйМетод();
КонецПроцедуры

&НаСервере
Процедура СерверныйМетод()
    // server logic
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.methods.len(), 3);
        assert!(summary.methods[0].dispatch.can_run_on_client);
        assert!(summary.methods[2].dispatch.is_server_only());

        let local_edges: Vec<_> =
            summary.call_edges.iter().filter(|e| e.kind == EdgeKind::DirectLocal).collect();
        assert_eq!(local_edges.len(), 2);

        let key = |i: usize| summary.methods[i].local_id;
        assert_eq!(local_edges[0].caller, CallerId::Method(key(0)));
        assert_eq!(local_edges[0].target, CallTarget::Local { callee_local_id: key(1) });

        assert_eq!(local_edges[1].caller, CallerId::Method(key(1)));
        assert_eq!(local_edges[1].target, CallTarget::Local { callee_local_id: key(2) });
    }

    #[test]
    fn test_qualified_call_to_common_module() {
        let code = r#"
Процедура МойМетод()
    ОбщийМодуль.ВнешнийМетод();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        let qual_edges: Vec<_> = summary
            .call_edges
            .iter()
            .filter(|e| e.kind == EdgeKind::DirectQualifiedModule)
            .collect();
        assert_eq!(qual_edges.len(), 1);
        assert!(matches!(
            &qual_edges[0].target,
            CallTarget::QualifiedModule { module_name, method_name }
                if module_name.as_str() == "ОбщийМодуль"
                    && method_name.as_str() == "ВнешнийМетод"
        ));
    }

    #[test]
    fn common_module_via_obshchiy_modul_resolves_to_named_module() {
        let code = r#"
Процедура МойМетод()
    Модуль = ОбщегоНазначения.ОбщийМодуль("РаботаСФайлами");
    Модуль.СохранитьФайл();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let edge = summary
            .call_edges
            .iter()
            .find(|e| {
                matches!(
                    &e.target,
                    CallTarget::QualifiedModule { method_name, .. }
                        if method_name.as_str() == "СохранитьФайл"
                )
            })
            .expect("call through the bound variable must produce a qualified edge");
        assert!(
            matches!(
                &edge.target,
                CallTarget::QualifiedModule { module_name, .. }
                    if module_name.as_str() == "РаботаСФайлами"
            ),
            "receiver bound to ОбщийМодуль(\"РаботаСФайлами\") must resolve to that module, got {:?}",
            edge.target
        );
    }

    #[test]
    fn test_this_object_call_normalized_to_direct_local() {
        let code = r#"
Процедура A()
    ЭтотОбъект.B();
КонецПроцедуры

Процедура B()
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        let b_id = summary
            .methods
            .iter()
            .find(|method| method.name.as_str() == "B")
            .expect("B method must be extracted")
            .local_id;
        assert_eq!(summary.call_edges.len(), 1);
        assert_eq!(summary.call_edges[0].kind, EdgeKind::DirectLocal);
        assert_eq!(summary.call_edges[0].target, CallTarget::Local { callee_local_id: b_id });
    }

    #[test]
    fn test_this_object_call_english_normalized() {
        let code = r#"
Procedure A()
    ThisObject.B();
EndProcedure

Procedure B()
EndProcedure
"#;
        let summary = parse_and_extract(code);

        let b_id = summary
            .methods
            .iter()
            .find(|method| method.name.as_str() == "B")
            .expect("B method must be extracted")
            .local_id;
        assert_eq!(summary.call_edges.len(), 1);
        assert_eq!(summary.call_edges[0].kind, EdgeKind::DirectLocal);
        assert_eq!(summary.call_edges[0].target, CallTarget::Local { callee_local_id: b_id });
    }

    #[test]
    fn test_this_object_call_unknown_method_emits_this_object_method_variant() {
        let code = r#"
Процедура A()
    ЭтотОбъект.Unknown();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.call_edges.len(), 1);
        assert_eq!(summary.call_edges[0].kind, EdgeKind::DirectLocal);
        assert_eq!(
            summary.call_edges[0].target,
            CallTarget::ThisObjectMethod { method_name: Name::new("Unknown") }
        );
    }

    #[test]
    fn test_qualified_module_call_unchanged() {
        let code = r#"
Процедура Тест()
    СоседнийМодуль.Метод();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.call_edges.len(), 1);
        assert_eq!(summary.call_edges[0].kind, EdgeKind::DirectQualifiedModule);
        assert!(matches!(
            &summary.call_edges[0].target,
            CallTarget::QualifiedModule { module_name, method_name }
                if module_name.as_str() == "СоседнийМодуль"
                    && method_name.as_str() == "Метод"
        ));
    }

    #[test]
    fn test_no_duplicate_edges_for_single_qualified_call() {
        let code = r#"
Процедура Тест()
    ОбщийМодуль.Метод();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        let qual_edges: Vec<_> = summary
            .call_edges
            .iter()
            .filter(|e| e.kind == EdgeKind::DirectQualifiedModule)
            .collect();
        assert_eq!(qual_edges.len(), 1, "Should be exactly 1 edge, no duplicates");
    }

    #[test]
    fn test_qualified_call_to_nonexistent_method_still_produces_edge() {
        let code = r#"
Процедура Тест()
    НесуществующийМодуль.НесуществующийМетод();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        let qual_edges: Vec<_> = summary
            .call_edges
            .iter()
            .filter(|e| e.kind == EdgeKind::DirectQualifiedModule)
            .collect();
        assert_eq!(qual_edges.len(), 1);
        assert!(matches!(
            &qual_edges[0].target,
            CallTarget::QualifiedModule { module_name, method_name }
                if module_name.as_str() == "НесуществующийМодуль"
                    && method_name.as_str() == "НесуществующийМетод"
        ));
    }

    #[test]
    fn test_notify_description_in_module_code() {
        let code = r#"
Перем Оповещение;
Оповещение = Новый ОписаниеОповещения("ОбработатьОповещение", ЭтотОбъект);

Процедура ОбработатьОповещение(Результат, ДопПараметры) Экспорт
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(summary.notify_regs[0].caller, CallerId::ModuleCode);
        assert_eq!(summary.notify_regs[0].callback_name, Name::new("ОбработатьОповещение"));
        assert_eq!(
            summary.notify_regs[0].target,
            NotifyTarget::ThisObject,
            "ЭтотОбъект means current module"
        );
    }

    #[test]
    fn test_notify_description_english() {
        let code = r#"
Procedure Test()
    Notification = New NotifyDescription("HandleResult", ThisObject);
EndProcedure

Procedure HandleResult(Result, AdditionalParameters) Export
EndProcedure
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(summary.notify_regs[0].caller, CallerId::Method(MethodKey::first("Test")));
        assert_eq!(summary.notify_regs[0].callback_name, Name::new("HandleResult"));
        assert_eq!(summary.notify_regs[0].target, NotifyTarget::ThisObject);
    }

    #[test]
    fn test_notify_description_this_form_is_this_object() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("ПослеЗакрытия", ЭтаФорма, Параметры);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(summary.notify_regs[0].target, NotifyTarget::ThisObject);
    }

    #[test]
    fn test_notify_description_common_module_target() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("ОбработатьРезультат", МойОбщийМодуль);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(
            summary.notify_regs[0].target,
            NotifyTarget::Module(Name::new("МойОбщийМодуль"))
        );
    }

    #[test]
    fn test_notify_description_unsupported_receiver() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("ОбработатьРезультат", Объекты[0]);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(summary.notify_regs[0].target, NotifyTarget::Unsupported);
    }

    #[test]
    fn test_notify_description_error_handler_is_second_reg() {
        // `Новый ОписаниеОповещения(ИмяПроцедуры, Модуль, ДопПараметры,
        // ИмяПроцедурыОбработкиОшибки, МодульОбработкиОшибки)` — both the success and the
        // error handler are independent dispatch targets and must each yield a reg.
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("Готово", ЭтотОбъект, Параметры, "ПриОшибке", МодульОшибок);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.notify_regs.len(), 2);
        assert_eq!(summary.notify_regs[0].callback_name, Name::new("Готово"));
        assert_eq!(summary.notify_regs[0].target, NotifyTarget::ThisObject);
        assert_eq!(summary.notify_regs[1].callback_name, Name::new("ПриОшибке"));
        assert_eq!(summary.notify_regs[1].target, NotifyTarget::Module(Name::new("МодульОшибок")));
    }

    #[test]
    fn test_notify_description_omitted_error_handler_yields_one_reg() {
        // The error handler is optional: a 2-arg constructor must not invent a second reg.
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("Готово", ЭтотОбъект);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.notify_regs.len(), 1);
        assert_eq!(summary.notify_regs[0].callback_name, Name::new("Готово"));
    }

    #[test]
    fn test_manager_access_three_segment() {
        let code = r#"
Процедура Тест()
    Документы.ПриходнаяНакладная.СоздатьЭлемент();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        let manager_edges: Vec<_> = summary
            .call_edges
            .iter()
            .filter(|e| matches!(&e.target, CallTarget::ManagerAccess { .. }))
            .collect();
        assert_eq!(manager_edges.len(), 1);
        assert!(matches!(
            &manager_edges[0].target,
            CallTarget::ManagerAccess {
                manager_type: ManagerType::Documents,
                object_name,
                method_name: Some(method),
            } if object_name.as_str() == "ПриходнаяНакладная"
                && method.as_str() == "СоздатьЭлемент"
        ));
    }

    /// Ребро графа для трёхзвенной цепочки строит единственная функция
    /// `field_callee_to_edge`, поэтому её покрытие обязано быть полным по видам
    /// метаданных: пропуск одного вида — молча потерянная зависимость.
    #[test]
    fn every_manager_type_yields_a_manager_access_edge() {
        for &mdo in bsl_metadata::MdoType::all() {
            let Some(manager_type) = ManagerType::from_mdo_type(mdo) else {
                continue;
            };
            let plural = mdo
                .russian_plural()
                .unwrap_or_else(|| panic!("{mdo:?} has a manager module but no collection name"));
            let code = format!("Процедура Тест()\n    {plural}.Объект1.Метод();\nКонецПроцедуры\n");
            let summary = parse_and_extract(&code);

            let found = summary.call_edges.iter().any(|e| {
                matches!(
                    &e.target,
                    CallTarget::ManagerAccess { manager_type: got, object_name, method_name: Some(method) }
                        if *got == manager_type
                            && object_name.as_str() == "Объект1"
                            && method.as_str() == "Метод"
                )
            });
            assert!(
                found,
                "{mdo:?} ({plural}): no ManagerAccess edge, got {:?}",
                summary.call_edges
            );
        }
    }

    #[test]
    fn test_register_movement_bare_and_with_receiver() {
        // `Движения.<Регистр>.<метод>()` is a movement touch in three idiomatic forms:
        // bare (implicit ЭтотОбъект), through `ЭтотОбъект.`, and through an object variable.
        let code = r#"
Процедура ОбработкаПроведения()
    Движение = Движения.ТоварыНаСкладах.Добавить();
    ЭтотОбъект.Движения.ТоварыНаСкладах.Очистить();
    Об.Движения.Взаиморасчеты.Записать();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let movement_registers: Vec<&str> = summary
            .call_edges
            .iter()
            .filter_map(|e| match &e.target {
                CallTarget::RegisterMovement { register_name } => Some(register_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(movement_registers, ["ТоварыНаСкладах", "ТоварыНаСкладах", "Взаиморасчеты"]);
        assert!(summary
            .call_edges
            .iter()
            .all(|e| !matches!(&e.target, CallTarget::RegisterMovement { .. })
                || e.kind == EdgeKind::RegisterMovement));
    }

    #[test]
    fn register_movement_dynamic_literal_index() {
        // A document can address its `Движения` collection by a dynamic index instead of a
        // literal segment. A locally-literal index resolves: a `Метаданные.…Имя` chain or a
        // string literal. A variable index needs value flow and stays unmodelled.
        let code = r#"
Процедура ОбработкаПроведения(ИмяРегистра)
    Движения[Метаданные.РегистрыНакопления.СебестоимостьТоваров.Имя].Записать();
    Движения["Взаиморасчеты"].Очистить();
    Движения[ИмяРегистра].Записать();
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let movement_registers: Vec<&str> = summary
            .call_edges
            .iter()
            .filter_map(|e| match &e.target {
                CallTarget::RegisterMovement { register_name } => Some(register_name.as_str()),
                _ => None,
            })
            .collect();
        // The literal chain and the string literal resolve; the variable index does not.
        assert_eq!(movement_registers, ["СебестоимостьТоваров", "Взаиморасчеты"]);
        assert!(summary
            .call_edges
            .iter()
            .all(|e| !matches!(&e.target, CallTarget::RegisterMovement { .. })
                || e.kind == EdgeKind::RegisterMovement));
    }

    #[test]
    fn manager_edge_kind_classifies_register_record_set() {
        use crate::queries::manager_edge_kind;
        // Record-set creators on a register manager → RegisterRecordSet, in either language.
        for method in ["СоздатьНаборЗаписей", "СоздатьМенеджерЗаписи", "CreateRecordSet"]
        {
            assert_eq!(
                manager_edge_kind(ManagerType::AccumulationRegisters, method),
                EdgeKind::RegisterRecordSet,
                "{method} on a register manager is a record-set access"
            );
        }
        // The same method name on a non-register manager stays a generic create.
        assert_eq!(
            manager_edge_kind(ManagerType::Catalogs, "СоздатьНаборЗаписей"),
            EdgeKind::ManagerCreates
        );
        // Other register manager methods keep their generic create/access classification.
        assert_eq!(
            manager_edge_kind(
                ManagerType::AccumulationRegisters,
                "СоздатьМенеджерЗаписиНесуществующий"
            ),
            EdgeKind::ManagerCreates
        );
        assert_eq!(
            manager_edge_kind(ManagerType::AccumulationRegisters, "Выбрать"),
            EdgeKind::ManagerAccess
        );
    }

    #[test]
    #[ignore = "known gap: a bare `Var = Движения.X` property read (no call) is not modelled \
                — capturing the recordset into a variable needs receiver dataflow; tracked here"]
    fn register_movement_property_read_is_a_known_gap() {
        let code = r#"
Процедура ОбработкаПроведения()
    НаборЗаписей = Движения.ТоварыНаСкладах;
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(
            summary
                .call_edges
                .iter()
                .any(|e| matches!(&e.target, CallTarget::RegisterMovement { register_name }
                    if register_name.as_str() == "ТоварыНаСкладах")),
            "a captured-recordset property read should also count as a register touch"
        );
    }

    #[test]
    fn test_idle_handler_not_edge() {
        let code = r#"
&НаКлиенте
Процедура ПриОткрытии()
    ПодключитьОбработчикОжидания("Обновить", 5, Истина);
КонецПроцедуры

&НаКлиенте
Процедура Обновить()
    // update
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.idle_handler_regs.len(), 1);
        assert_eq!(summary.idle_handler_regs[0].handler_name, Name::new("Обновить"));
        assert!(summary.idle_handler_regs[0].one_shot);
        assert_eq!(
            summary.idle_handler_regs[0].caller,
            CallerId::Method(MethodKey::first("ПриОткрытии"))
        );

        let local_edges: Vec<_> =
            summary.call_edges.iter().filter(|e| e.kind == EdgeKind::DirectLocal).collect();
        assert!(local_edges.is_empty(), "Idle handler registration should not produce a call edge");
    }

    #[test]
    fn test_set_action_reg_extracted() {
        let code = r#"
&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Элементы.Валюта.УстановитьДействие("ПриИзменении", "ВалютаПриИзменении");
КонецПроцедуры

&НаКлиенте
Процедура ВалютаПриИзменении(Элемент)
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert_eq!(summary.set_action_regs.len(), 1);
        assert_eq!(summary.set_action_regs[0].handler_name, Name::new("ВалютаПриИзменении"));
        assert_eq!(
            summary.set_action_regs[0].caller,
            CallerId::Method(summary.methods[0].local_id)
        );
    }

    #[test]
    fn test_set_action_via_this_form_items_extracted() {
        let code = r#"
&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    ЭтаФорма.Элементы.Валюта.УстановитьДействие("ПриИзменении", "ВалютаПриИзменении");
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.set_action_regs.len(), 1);
        assert_eq!(summary.set_action_regs[0].handler_name, Name::new("ВалютаПриИзменении"));
    }

    #[test]
    fn test_set_action_non_literal_handler_ignored() {
        let code = r#"
&НаСервере
Процедура Настроить(ИмяОбработчика)
    Элементы.Валюта.УстановитьДействие("ПриИзменении", ИмяОбработчика);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(summary.set_action_regs.is_empty());
    }

    #[test]
    fn test_set_action_on_element_variable_extracted() {
        // Real form code holds the item in a variable (`НовыйЭлемент = Элементы.Добавить(…)`)
        // then binds via it. УстановитьДействие is form-element-specific, so the second
        // string argument is a handler name regardless of the receiver's syntactic shape.
        let code = r#"
&НаКлиенте
Процедура ДобавитьКолонку()
    ЭлементФормы.УстановитьДействие("ПриИзменении", "КолонкаПриИзменении");
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.set_action_regs.len(), 1);
        assert_eq!(summary.set_action_regs[0].handler_name, Name::new("КолонкаПриИзменении"));
    }

    #[test]
    fn test_set_action_dynamic_index_receiver_extracted() {
        let code = r#"
&НаКлиенте
Процедура ДобавитьКолонку(Постфикс)
    Элементы["Количество" + Постфикс].УстановитьДействие("ПриИзменении", "КоличествоПриИзменении");
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.set_action_regs.len(), 1);
        assert_eq!(summary.set_action_regs[0].handler_name, Name::new("КоличествоПриИзменении"));
    }

    #[test]
    fn test_set_action_on_manager_receiver_ignored() {
        // `УстановитьДействие` is not reserved: a user can export it on a manager module.
        // `Справочники.X.УстановитьДействие("Опция", "Имя")` is such a call, not a form
        // binding, so its second string is data and must not be recorded as a handler.
        let code = r#"
&НаСервере
Процедура Настроить()
    Справочники.Номенклатура.УстановитьДействие("Опция", "Имя");
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(summary.set_action_regs.is_empty());
    }

    #[test]
    fn test_name_literal_refs_extracted() {
        // The handler name reaches `УстановитьДействие` only inside a helper module; the
        // sole same-module trace is the string literal, in a structure argument or in a
        // code-created command's `Действие` assignment.
        let code = r#"
&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Параметры = Новый Структура("ИмяСобытия, ИмяПроцедурыОбработчика", "ПриИзменении", "ВесБруттоПриИзменении");
    Помощники.ДобавитьПолеФормы(ЭтаФорма, Параметры);
    НоваяКоманда = Команды.Добавить("Дозагруз");
    НоваяКоманда.Действие = "ДозагрузКоманда";
КонецПроцедуры

&НаКлиенте
Процедура ВесБруттоПриИзменении(Элемент)
КонецПроцедуры

&НаКлиенте
Процедура ДозагрузКоманда(Команда)
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let names: Vec<&str> = summary
            .name_literal_refs
            .iter()
            .filter_map(|id| summary.methods.iter().find(|m| m.local_id == *id))
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["ВесБруттоПриИзменении", "ДозагрузКоманда"],
            "only literals naming a local method are recorded"
        );
    }

    #[test]
    fn test_name_literal_refs_ignore_concatenation() {
        // A concatenated handler name is not a single literal; neither part alone names
        // a local method, so nothing is recorded and the method stays unreferenced.
        let code = r#"
&НаКлиенте
Процедура Настроить()
    Элементы.Валюта.УстановитьДействие("ПриИзменении", "Валюта" + "ПриИзменении");
КонецПроцедуры

&НаКлиенте
Процедура ВалютаПриИзменении(Элемент)
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(summary.name_literal_refs.is_empty());
    }

    #[test]
    fn test_name_literal_refs_identifier_shape_and_dedup() {
        let code = r#"
&НаКлиенте
Процедура Настроить()
    Текст = "Вызовите ОбработчикСобытия вручную";
    Имя1 = "обработчикСобытия";
    Имя2 = "ОбработчикСобытия";
КонецПроцедуры

&НаКлиенте
Процедура ОбработчикСобытия()
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        // The message text is not identifier-shaped (substrings do not count); the two
        // case variants resolve to the same local method.
        assert_eq!(summary.name_literal_refs.len(), 1);
        let method = summary
            .methods
            .iter()
            .find(|m| m.local_id == summary.name_literal_refs[0])
            .expect("recorded id must resolve to a local method");
        assert_eq!(method.name, Name::new("ОбработчикСобытия"));
    }

    #[test]
    fn test_method_dispatch_from_code() {
        let code = r#"
&НаКлиенте
Процедура КлиентМетод()
КонецПроцедуры

&НаСервере
Функция СерверФункция()
    Возврат 1;
КонецФункции

&НаКлиентеНаСервереБезКонтекста
Функция ОбщаяФункция()
    Возврат 2;
КонецФункции

Процедура БезАннотации()
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(summary.methods.len(), 4);

        assert!(summary.methods[0].dispatch.can_run_on_client);
        assert!(!summary.methods[0].dispatch.can_run_on_server);

        assert!(summary.methods[1].dispatch.is_server_only());

        assert!(summary.methods[2].dispatch.can_run_on_client);
        assert!(summary.methods[2].dispatch.can_run_on_server);
        assert!(summary.methods[2].dispatch.no_context);

        assert!(summary.methods[3].dispatch.can_run_on_client);
        assert!(!summary.methods[3].dispatch.can_run_on_server);
    }

    #[test]
    fn workspace_call_graph_heap_counts_tables_edges_and_node_names() {
        // Given: a graph with one edge from a method to a metadata object whose
        // spelling exceeds the SmolStr inline cap (so its `Name` heap is nonzero),
        // plus a dispatch entry for that same method.
        let module = ModuleId::new(vfs::FileId(0));
        let method = GraphNode::Method(MethodId { module, local_id: MethodKey::first("М0") });
        let long_object_name = "ОченьДлинноеИмяСправочникаБольше23Символов";
        let object_name_heap = long_object_name.len();
        let mdo =
            GraphNode::Mdo { mdo_type: MdoType::Catalog, object_name: Name::new(long_object_name) };

        let mut graph = WorkspaceCallGraph::default();
        graph.set_dispatch(
            method.clone(),
            MethodDispatch { can_run_on_client: true, can_run_on_server: false, no_context: false },
        );
        graph.insert(WorkspaceCallEdge {
            from: method,
            to: mdo,
            kind: EdgeKind::ManagerAccess,
            provenance: EdgeProvenance::Inferred,
            call_site: CallSite::Recorded(TextRange::new(0.into(), 8.into())),
            crosses_client_to_server: false,
        });

        // When: the heap estimator runs on the graph.
        let bytes = workspace_call_graph_heap(&std::sync::Arc::new(graph));

        // Then: at least the spilled object name (charged once per adjacency
        // side, forward + reverse); order of magnitude away from a blowup for
        // a single edge (the three-table structure and the `GraphNode`
        // enum's stack size dominate a one-entry estimate, unlike a plain
        // string map).
        assert!(bytes > object_name_heap * 2);
        assert!(bytes < 8192);
    }
}
