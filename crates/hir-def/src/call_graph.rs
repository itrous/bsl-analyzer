use bsl_metadata::MdoType;
use cfg_types::IdConversion;
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;
use syntax::{TextRange, TextSize};

use crate::{
    body::{Body, ManagerType},
    execution_env::EnvFlags,
    hir::{Expr, ExprIdx, Literal, Stmt},
    item_tree::{AnnotationKind, ItemTree},
    name::Name,
    ExprId, MethodId, MethodKey, ModuleBodies, ModuleId,
};
use intern::NormName;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallSummary {
    pub methods: Vec<MethodSummary>,
    pub call_edges: Vec<CallEdge>,
    /// Generic syntax-only constructor candidates. Platform validation and callback-slot
    /// interpretation happen in `hir-ty`; these are not notify registrations.
    pub notify_candidates: Vec<NotifyConstructorCandidate>,
    /// Callback names whose fixed signature follows from compatibility constructor names and
    /// receiver shape. Kept when platform metadata cannot normalize a registration.
    pub notify_signature_handlers: Vec<Name>,
    /// Callback names introduced by the current English `CallbackDescription` spelling. Kept
    /// separate so consumers can use it only when configured platform facts are absent, without
    /// changing compatibility behavior elsewhere.
    pub notify_additional_signature_handlers: Vec<Name>,
    /// Callback-name strings added to arrays passed to dynamic callback-description constructors.
    /// Kept separate so consumers can restrict this conservative syntax fallback to configurations
    /// without callback constructor facts.
    pub notify_dynamic_signature_handlers: Vec<Name>,
    /// Source call sites for the dynamic callback names above. The callback expression and
    /// constructor range identify a specific slot, so a resolved registration in a different slot
    /// with the same handler name cannot disprove this candidate.
    pub notify_dynamic_signature_handler_sites: Vec<DynamicNotifySignatureHandler>,
    /// Dynamic callback-array constructors whose alias analysis was incomplete. A non-empty
    /// list is module-level uncertainty: consumers must not infer that unlisted local methods
    /// or exports are unused merely because a callback was not recovered from these sites.
    pub notify_dynamic_signature_analysis_incomplete: Vec<DynamicNotifySignatureAnalysisGap>,
    /// Platform-validated registrations. Raw `hir-def` summaries leave this empty.
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
    /// Source sites for local method-name string literals. The semantic summary narrows this
    /// to local-object notify callback literals and keeps exact sites separate from other
    /// string references so an external callback name cannot mask a local method.
    pub name_literal_ref_sites: Vec<NameLiteralRefSite>,
    pub form_entries: Vec<FormEventEntry>,
}

/// A handler name a module binds by string to ANOTHER object's handler slot -
/// `Форма.ПодключитьОбработчикОжидания("Имя", …)`,
/// `Форма.Элементы.Поле.УстановитьДействие("ПриИзменении", "Имя")` - where the receiver
/// is not the current module (see [`collect_foreign_handler_bindings`]). The handler
/// lives in a different file, so only the source-root fold ([`ForeignHandlerBindings`])
/// lets that file see it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForeignHandlerBinding {
    pub handler_name: Name,
    /// First segment of the receiver chain that is not the current module — the
    /// variable or parameter holding the bound object (`Форма` in
    /// `Форма.Элементы.Поле.УстановитьДействие`). Lets the fold drop bindings whose
    /// receiver is a common module, whose handlers are exported methods anyway.
    /// `None` when the receiver is a call's result, which is never a common module.
    pub receiver_root: Option<Name>,
}

/// Every procedure name bound by string to a handler slot of another module's object
/// anywhere in a source root - the fold of every module's [`ForeignHandlerBinding`]s,
/// minus bindings received by a common module. Which object the binding targets is not
/// resolved: a name in the set answers "some module binds a handler of this name
/// dynamically".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignHandlerBindings {
    names: FxHashSet<NormName>,
}

impl ForeignHandlerBindings {
    pub fn new(names: FxHashSet<NormName>) -> Self {
        Self { names }
    }

    /// Case-insensitive, the way the platform resolves the handler name.
    pub fn is_bound(&self, name: &str) -> bool {
        self.names.contains(&NormName::intern(name))
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
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
                    && !matches!(
                        reg.target,
                        NotifyTarget::Unsupported | NotifyTarget::NotCompiled { .. }
                    )
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
            notify_candidates: Vec::new(),
            notify_signature_handlers: Vec::new(),
            notify_additional_signature_handlers: Vec::new(),
            notify_dynamic_signature_handlers: Vec::new(),
            notify_dynamic_signature_handler_sites: Vec::new(),
            notify_dynamic_signature_analysis_incomplete: Vec::new(),
            notify_regs,
            idle_handler_regs,
            set_action_regs: Vec::new(),
            name_literal_refs: Vec::new(),
            name_literal_ref_sites: Vec::new(),
            form_entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicNotifySignatureHandler {
    pub handler_name: Name,
    pub callback_expr: ExprId,
    pub constructor_range: TextRange,
}

/// One constructor for which dynamic callback-array alias analysis could not finish.
/// The containing module is semantically incomplete, so downstream negative conclusions
/// must conservatively retain relevant local methods/exports beyond just `caller`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DynamicNotifySignatureAnalysisGap {
    pub caller: CallerId,
    pub constructor: ExprId,
    pub constructor_range: TextRange,
    pub array_name: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSummary {
    pub local_id: MethodKey,
    pub name: Name,
    pub dispatch: MethodDispatch,
    pub is_export: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameLiteralRefSite {
    pub caller: CallerId,
    pub local_id: MethodKey,
    pub expr: ExprId,
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
    /// module body) links to the handler named by the string literal. This is a
    /// reference to a found name, not a guarantee that the handler can be invoked:
    /// for example, a local declaration need not be exported. It is not a direct
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

/// Syntactic shape of a notification receiver, before semantic resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyReceiverSyntax {
    /// An object or form self-name, in either language.
    SelfName,
    /// A bare name other than a self-name.
    BareName,
    /// A non-bare or omitted receiver.
    Other,
}

impl NotifyReceiverSyntax {
    /// Classify a bare receiver without assuming what the name resolves to.
    pub fn for_bare_name(name: &Name) -> Self {
        if is_this_receiver(&name.as_str().fold_lower()) {
            Self::SelfName
        } else {
            Self::BareName
        }
    }
}

/// Where a `Новый ОписаниеОповещения` callback's handler lives. `hir-ty` resolves
/// bare receivers using the same inference and name-resolution rules as other semantic
/// consumers; unresolved or unsupported receivers are represented by `Unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NotifyTarget {
    /// The registration lies only in an unavailable or unreachable compilation branch.
    NotCompiled { callback_expr: ExprId, receiver_syntax: NotifyReceiverSyntax },
    /// The receiver resolves to the current object/form module.
    ThisObject,
    /// The receiver resolves to a common module.
    Module(Name),
    /// The receiver cannot be resolved as a current object or common module.
    Unsupported,
}

/// A syntax-shaped constructor expression that may describe a notification callback.
/// This carries no platform or callback-slot conclusion; semantic consumers validate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NotifyConstructorCandidate {
    pub caller: CallerId,
    pub constructor: ExprId,
    pub range: TextRange,
}

/// Execution environments in which a notification registration may exist.
/// `definite` is proven to register; `possible` also includes undecided
/// preprocessor conditions. Consumers must not turn a possible-only
/// environment into a definitive verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NotifyRegistrationEnvironments {
    pub definite: EnvFlags,
    pub possible: EnvFlags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyReg {
    pub caller: CallerId,
    /// Expression containing the callback string literal, for source-exact reference filtering.
    pub callback_expr: ExprId,
    pub callback_name: Name,
    /// Source syntax retained independently of semantic receiver resolution.
    pub receiver_syntax: NotifyReceiverSyntax,
    pub target: NotifyTarget,
    /// Environment mask for this constructor, not merely its enclosing method.
    pub environments: NotifyRegistrationEnvironments,
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
    extract_call_summary_with_cancellation(
        item_tree,
        module_bodies,
        form_event_handlers,
        &mut || {},
    )
}

pub(crate) fn extract_call_summary_with_cancellation(
    item_tree: &ItemTree,
    module_bodies: &ModuleBodies,
    form_event_handlers: &[bsl_metadata::FormEventHandler],
    cancel: &mut dyn FnMut(),
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
    let mut notify_candidates = Vec::new();
    let mut notify_signature_handlers = Vec::new();
    let mut notify_additional_signature_handlers = Vec::new();
    let mut notify_dynamic_signature_handlers = Vec::new();
    let mut notify_dynamic_signature_handler_sites = Vec::new();
    let mut notify_dynamic_signature_analysis_incomplete = Vec::new();
    let mut idle_handler_regs = Vec::new();
    let mut set_action_regs = Vec::new();
    let mut name_literals: FxHashSet<MethodKey> = FxHashSet::default();
    let mut name_literal_ref_sites = Vec::new();

    // Bodies come in item order, which is deterministic.
    for (local_id, lower_result) in module_bodies.iter_lower_results() {
        extract_from_body(
            lower_result.body(),
            lower_result.source_map(),
            CallerId::Method(local_id),
            &local_method_ids,
            &mut call_edges,
            &mut notify_candidates,
            &mut notify_signature_handlers,
            &mut notify_additional_signature_handlers,
            &mut notify_dynamic_signature_handlers,
            &mut notify_dynamic_signature_handler_sites,
            &mut notify_dynamic_signature_analysis_incomplete,
            &mut idle_handler_regs,
            &mut set_action_regs,
            &mut name_literals,
            &mut name_literal_ref_sites,
            cancel,
        );
    }

    if let Some(module_code) = module_bodies.module_code_result() {
        extract_from_body(
            module_code.body(),
            module_code.source_map(),
            CallerId::ModuleCode,
            &local_method_ids,
            &mut call_edges,
            &mut notify_candidates,
            &mut notify_signature_handlers,
            &mut notify_additional_signature_handlers,
            &mut notify_dynamic_signature_handlers,
            &mut notify_dynamic_signature_handler_sites,
            &mut notify_dynamic_signature_analysis_incomplete,
            &mut idle_handler_regs,
            &mut set_action_regs,
            &mut name_literals,
            &mut name_literal_ref_sites,
            cancel,
        );
    }

    let mut seen_dynamic_sites = FxHashSet::default();
    notify_dynamic_signature_handler_sites.retain(|site| {
        seen_dynamic_sites.insert((
            site.handler_name.clone(),
            site.callback_expr,
            site.constructor_range,
        ))
    });
    let mut seen_dynamic_handlers = FxHashSet::default();
    notify_dynamic_signature_handlers
        .retain(|handler| seen_dynamic_handlers.insert(handler.clone()));
    let mut seen_dynamic_gaps = FxHashSet::default();
    notify_dynamic_signature_analysis_incomplete.retain(|gap| {
        seen_dynamic_gaps.insert((gap.caller, gap.constructor, gap.constructor_range))
    });

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
        notify_candidates,
        notify_signature_handlers,
        notify_additional_signature_handlers,
        notify_dynamic_signature_handlers,
        notify_dynamic_signature_handler_sites,
        notify_dynamic_signature_analysis_incomplete,
        notify_regs: Vec::new(),
        idle_handler_regs,
        set_action_regs,
        name_literal_refs,
        name_literal_ref_sites,
        form_entries,
    }
}

/// Collects [`ForeignHandlerBinding`]s of one body: identifier-shaped handler names
/// handed by string to a handler slot of an object that is not the current module.
///
/// - `<получатель>.ПодключитьОбработчикОжидания("Имя", …)` (first argument);
/// - `<получатель>.ПодключитьОбработчикИзмененияДанных(Данные, "Имя", …)` (second argument);
/// - `<получатель>.<…>.УстановитьДействие("Событие", "Имя")` (second argument) - only
///   through an access chain (`Форма.Элементы.Поле`): a bare variable receiver holds an
///   element whose form cannot be told apart from the current one.
///
/// The receiver is foreign when its chain, after an optional leading `ЭтаФорма` /
/// `ЭтотОбъект`, does not start at the current form's items (`Элементы`,
/// `ЭлементыФормы`) - `Форма.…`, `ВладелецФормы.…`, `ЭтаФорма.ВладелецФормы.…`,
/// `Форма.ПолучитьФорму().…`. A bare `Элементы` that is a parameter or local of the body
/// holds some other form's items. Bindings to the current module are already covered by
/// the same-module summary facts.
pub fn collect_foreign_handler_bindings(body: &Body, out: &mut Vec<ForeignHandlerBinding>) {
    for (_, expr) in body.exprs_iter() {
        let Expr::Call { callee, args } = expr else { continue };
        let Expr::Field { base, field } = body.expr_idx(*callee) else { continue };
        let method = field.as_str().fold_lower();
        let handler_arg = if is_attach_idle_handler(&method) {
            0
        } else if is_attach_data_change_handler(&method)
            || (is_set_action(&method) && !matches!(body.expr_idx(*base), Expr::Path(_)))
        {
            1
        } else {
            continue;
        };
        let Some(name) = args.get(handler_arg).and_then(|&arg| extract_string_literal(body, arg))
        else {
            continue;
        };
        if !is_identifier_like(&name) {
            continue;
        }
        if let Some(receiver_root) = foreign_receiver_root(body, *base) {
            out.push(ForeignHandlerBinding { handler_name: Name::new(&name), receiver_root });
        }
    }
}

/// Whether `receiver` denotes an object other than the current module, and if so the
/// root to report as [`ForeignHandlerBinding::receiver_root`]. `None` when the chain
/// stays in the current module (`ЭтаФорма`, `Элементы.Поле`, `ЭтотОбъект.ЭлементыФормы.Поле`,
/// `Элементы.Найти("Поле")`) or is not rooted at a name (a literal, `Новый …`).
fn foreign_receiver_root(body: &Body, receiver: ExprIdx) -> Option<Option<Name>> {
    let mut chain = Vec::new();
    let mut through_call = false;
    let mut cursor = receiver;
    loop {
        match body.expr_idx(cursor) {
            Expr::Path(name) => {
                chain.push(name);
                break;
            }
            Expr::Field { base, field } => {
                chain.push(field);
                cursor = *base;
            }
            Expr::Index { base, .. } => cursor = *base,
            Expr::Call { callee, .. } => {
                through_call = true;
                cursor = *callee;
            }
            _ => return None,
        }
    }
    chain.reverse();
    let mut segments = chain.into_iter();
    let mut first = segments.next()?;
    let qualified = is_this_receiver(&first.as_str().fold_lower());
    if qualified {
        first = segments.next()?;
    }
    if is_current_form_items(&first.as_str().fold_lower())
        && (qualified || !is_body_local(body, first))
    {
        return None;
    }
    Some((!through_call).then(|| first.clone()))
}

fn is_current_form_items(name_lower: &str) -> bool {
    matches!(name_lower, "элементы" | "items" | "элементыформы" | "controls")
}

/// Whether `name` is a parameter, a declared variable or an assigned local of `body` -
/// a binding that shadows a context property of the same name.
///
/// Keys are `NormName`, the same match bucket the module classification uses, and NOT
/// `fold_lower`: the latter falls back to `str::to_lowercase`, which applies contextual
/// mappings (Greek final sigma), so two names equal for the classification would be
/// different here - a shadowing binding would be missed and the receiver would be taken
/// for a common module.
fn is_body_local(body: &Body, name: &Name) -> bool {
    let key = NormName::intern(name.as_str());
    body_local_names(body).any(|local| NormName::intern(local.as_str()) == key)
}

fn body_local_names(body: &Body) -> impl Iterator<Item = &Name> {
    body.bindings_iter().map(|(_, binding)| &binding.name).chain(body.stmts_iter().filter_map(
        |(_, stmt)| match stmt {
            Stmt::Assign { target, .. } => match body.expr_idx(*target) {
                Expr::Path(name) => Some(name),
                _ => None,
            },
            _ => None,
        },
    ))
}

fn is_attach_data_change_handler(name_lower: &str) -> bool {
    name_lower == "подключитьобработчикизмененияданных" || name_lower == "attachdatachangehandler"
}

// One &mut accumulator per output list (call edges, constructor candidates, and the string-named
// dispatch registries) plus the read-only inputs; bundling them into a struct would only move the
// same fields behind one more indirection.
#[allow(clippy::too_many_arguments)]
fn extract_from_body(
    body: &Body,
    source_map: crate::body::SourceMapAt<'_>,
    caller: CallerId,
    local_method_ids: &FxHashMap<NormName, MethodKey>,
    call_edges: &mut Vec<CallEdge>,
    notify_candidates: &mut Vec<NotifyConstructorCandidate>,
    notify_signature_handlers: &mut Vec<Name>,
    notify_additional_signature_handlers: &mut Vec<Name>,
    notify_dynamic_signature_handlers: &mut Vec<Name>,
    notify_dynamic_signature_handler_sites: &mut Vec<DynamicNotifySignatureHandler>,
    notify_dynamic_signature_analysis_incomplete: &mut Vec<DynamicNotifySignatureAnalysisGap>,
    idle_handler_regs: &mut Vec<IdleReg>,
    set_action_regs: &mut Vec<SetActionReg>,
    name_literals: &mut FxHashSet<MethodKey>,
    name_literal_ref_sites: &mut Vec<NameLiteralRefSite>,
    cancel: &mut dyn FnMut(),
) {
    let common_bindings = crate::common_module_ref::common_module_var_bindings(body);
    let mut alias_resolver = None;
    let mut resolver_initialized = false;
    let mut dynamic_candidates = Vec::new();
    for (expr_id, expr) in body.exprs_iter() {
        cancel();
        let Expr::New { type_name: None, args } = expr else { continue };
        let Some(type_name) = args.first().and_then(|&idx| extract_string_literal(body, idx))
        else {
            continue;
        };
        if !bsl_platform::is_callback_description_type_name(&type_name)
            || !matches!(args.get(1), Some(idx) if matches!(body.expr_idx(*idx), Expr::Path(_)))
        {
            continue;
        }
        if !resolver_initialized {
            alias_resolver = DynamicArrayResolver::new(body, source_map, cancel);
            resolver_initialized = true;
        }
        let Some(constructor_range) = source_map.expr_range(expr_id) else { continue };
        let Some(aliases) = dynamic_notify_array_aliases(
            body,
            args,
            alias_resolver.as_mut(),
            DynamicNotifyAnalysisContext {
                caller,
                constructor: expr_id,
                constructor_range,
                incomplete: notify_dynamic_signature_analysis_incomplete,
            },
            cancel,
        ) else {
            continue;
        };
        dynamic_candidates.extend(aliases.into_iter().map(|alias| DynamicNotifyArrayCandidate {
            array_name: alias.array_name,
            array_start: alias.array_start,
            array_end: alias.array_end,
            constructor_start: constructor_range.start(),
            constructor_range,
        }));
    }
    if !dynamic_candidates.is_empty() {
        let candidate_arrays: FxHashSet<_> =
            dynamic_candidates.iter().map(|candidate| candidate.array_name).collect();
        let mut array_additions = body
            .exprs_iter()
            .filter_map(|(expr_id, expr)| {
                let Expr::Call { callee, args } = expr else { return None };
                let [value, ..] = args.as_ref() else { return None };
                let Expr::Field { base, field } = body.expr_idx(*callee) else { return None };
                let Expr::Path(array_name) = body.expr_idx(*base) else { return None };
                let key = NormName::intern(array_name.as_str());
                if !candidate_arrays.contains(&key) {
                    return None;
                }
                let method = field.as_str().fold_lower();
                if method != "добавить" && method != "add" {
                    return None;
                }
                let range = source_map.expr_range(expr_id)?;
                Some((key, range, *value))
            })
            .collect::<Vec<_>>();
        array_additions.sort_unstable_by_key(|(_, range, _)| range.start());
        for candidate in &dynamic_candidates {
            extract_dynamic_notify_handlers(
                body,
                source_map,
                candidate,
                &array_additions,
                alias_resolver.as_ref(),
                notify_dynamic_signature_handlers,
                notify_dynamic_signature_handler_sites,
            );
        }
    }

    for (expr_id, expr) in body.exprs_iter() {
        match expr {
            Expr::Literal(Literal::String(s)) if is_identifier_like(s) => {
                if let Some(&local_id) = local_method_ids.get(&NormName::intern(s)) {
                    name_literals.insert(local_id);
                    name_literal_ref_sites.push(NameLiteralRefSite {
                        caller,
                        local_id,
                        expr: expr_id,
                    });
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
                if is_notify_constructor_candidate(body, type_name.is_some(), args) {
                    notify_candidates.push(NotifyConstructorCandidate {
                        caller,
                        constructor: expr_id,
                        range: source_map.expr_range(expr_id).unwrap_or(TextRange::empty(0.into())),
                    });
                }
                extract_notify_handlers(
                    body,
                    type_name.as_ref(),
                    args,
                    notify_signature_handlers,
                    notify_additional_signature_handlers,
                );
            }
            _ => {}
        }
    }
}

fn is_additional_callback_description_type_name(name: &str) -> bool {
    bsl_platform::is_callback_description_type_name(name)
        && !bsl_platform::is_callback_description_compatibility_type_name(name)
}

fn extract_notify_handlers(
    body: &Body,
    type_name: Option<&Name>,
    args: &[ExprIdx],
    compatibility_handlers: &mut Vec<Name>,
    additional_handlers: &mut Vec<Name>,
) {
    let (type_name, slot_offset) = match type_name {
        Some(type_name) => (type_name.as_str().to_owned(), 0),
        None => {
            let Some(type_name) = args.first().and_then(|&idx| extract_string_literal(body, idx))
            else {
                return;
            };
            (type_name, 1)
        }
    };
    let is_compatibility_name =
        bsl_platform::is_callback_description_compatibility_type_name(&type_name);
    let is_additional_name = is_additional_callback_description_type_name(&type_name);
    if !is_compatibility_name && !is_additional_name {
        return;
    }
    let slots = bsl_platform::bundled_callback_description_slots();

    // Preserve the syntax-only signature protection even for invalid arity. Semantic
    // registration, unlike this compatibility path, requires constructor binding.
    for (handler_slot, receiver_slot) in
        [(slots.handler, slots.receiver), (slots.error_handler, slots.error_receiver)]
    {
        let Some(&handler_idx) = args.get(handler_slot + slot_offset) else { continue };
        let Some(handler) = extract_string_literal(body, handler_idx) else { continue };
        let receiver_syntax =
            args.get(receiver_slot + slot_offset).map_or(NotifyReceiverSyntax::Other, |&idx| {
                match body.expr_idx(idx) {
                    Expr::Path(name) => NotifyReceiverSyntax::for_bare_name(name),
                    _ => NotifyReceiverSyntax::Other,
                }
            });
        if matches!(receiver_syntax, NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::Other) {
            if is_compatibility_name {
                compatibility_handlers.push(Name::new(&handler));
            }
            if is_additional_name {
                additional_handlers.push(Name::new(&handler));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DynamicNotifyArrayCandidate {
    array_name: NormName,
    array_start: Option<TextSize>,
    array_end: TextSize,
    constructor_start: TextSize,
    constructor_range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DynamicNotifyArrayAlias {
    array_name: NormName,
    array_start: Option<TextSize>,
    array_end: TextSize,
}

struct DynamicNotifyAnalysisContext<'a> {
    caller: CallerId,
    constructor: ExprId,
    constructor_range: TextRange,
    incomplete: &'a mut Vec<DynamicNotifySignatureAnalysisGap>,
}

fn dynamic_notify_array_aliases(
    body: &Body,
    args: &[ExprIdx],
    resolver: Option<&mut DynamicArrayResolver<'_>>,
    context: DynamicNotifyAnalysisContext<'_>,
    cancel: &mut dyn FnMut(),
) -> Option<Vec<DynamicNotifyArrayAlias>> {
    let DynamicNotifyAnalysisContext { caller, constructor, constructor_range, incomplete } =
        context;
    let type_name = args.first().and_then(|&idx| extract_string_literal(body, idx))?;
    if !bsl_platform::is_callback_description_type_name(&type_name) {
        return None;
    }
    let Expr::Path(name) = body.expr_idx(*args.get(1)?) else { return None };
    let array_name = Name::new(name.as_str());
    let name = NormName::intern(name.as_str());
    let Some(resolver) = resolver else {
        incomplete.push(DynamicNotifySignatureAnalysisGap {
            caller,
            constructor,
            constructor_range,
            array_name,
        });
        return Some(Vec::new());
    };
    let (mut aliases, complete) =
        resolver.resolve(name, constructor_range.start(), constructor_range.start(), cancel);
    if !complete {
        incomplete.push(DynamicNotifySignatureAnalysisGap {
            caller,
            constructor,
            constructor_range,
            array_name,
        });
        return Some(Vec::new());
    }
    if aliases.is_empty() {
        aliases.push(DynamicNotifyArrayAlias {
            array_name: name,
            array_start: None,
            array_end: constructor_range.start(),
        });
    }
    Some(aliases)
}

/// Trace known array references at the binding site. A later reassignment changes the variable,
/// not the object already copied into another local; conditional bindings retain both the
/// preceding value and each possible branch value.
const DYNAMIC_ARRAY_ALIAS_WORK_BUDGET: usize = 8_192;
const DYNAMIC_ARRAY_CANCEL_INTERVAL: usize = 128;

#[derive(Clone, Copy)]
struct DynamicArrayAssignment<'a> {
    range: TextRange,
    value: &'a Expr,
    branch: Option<TextRange>,
}

/// Shared, bounded body index. Every statement/assignment/branch examined spends budget,
/// so alias fan-out and large bodies have a fixed upper bound independent of path shape.
struct DynamicArrayResolver<'a> {
    body: &'a Body,
    assignments: FxHashMap<NormName, Vec<DynamicArrayAssignment<'a>>>,
    assignment_branches: FxHashMap<TextRange, Option<TextRange>>,
    remaining: usize,
    since_cancel: usize,
}

impl<'a> DynamicArrayResolver<'a> {
    fn new(
        body: &'a Body,
        source_map: crate::body::SourceMapAt<'_>,
        cancel: &mut dyn FnMut(),
    ) -> Option<Self> {
        let mut this = Self {
            body,
            assignments: FxHashMap::default(),
            assignment_branches: FxHashMap::default(),
            remaining: DYNAMIC_ARRAY_ALIAS_WORK_BUDGET,
            since_cancel: 0,
        };
        let mut branches = Vec::new();
        let mut unclassified = Vec::new();
        for (stmt_id, stmt) in body.stmts_iter() {
            if !this.spend(cancel) {
                return None;
            }
            let Some(range) = source_map.stmt_range(stmt_id) else { continue };
            if matches!(
                stmt,
                Stmt::If(_)
                    | Stmt::PreprocIf(_)
                    | Stmt::While { .. }
                    | Stmt::For { .. }
                    | Stmt::ForEach { .. }
                    | Stmt::Try { .. }
            ) {
                branches.push(range);
            }
            if let Stmt::Assign { target, value } = stmt {
                if let Expr::Path(name) = body.expr_idx(*target) {
                    unclassified.push((
                        NormName::intern(name.as_str()),
                        range,
                        body.expr_idx(*value),
                    ));
                }
            }
        }
        for (name, range, value) in unclassified {
            let mut branch = None;
            for &candidate in &branches {
                if !this.spend(cancel) {
                    return None;
                }
                if candidate.contains_range(range)
                    && branch.is_none_or(|previous: TextRange| candidate.len() < previous.len())
                {
                    branch = Some(candidate);
                }
            }
            this.assignment_branches.insert(range, branch);
            this.assignments.entry(name).or_default().push(DynamicArrayAssignment {
                range,
                value,
                branch,
            });
        }
        Some(this)
    }

    fn spend(&mut self, cancel: &mut dyn FnMut()) -> bool {
        Self::spend_fields(&mut self.remaining, &mut self.since_cancel, cancel)
    }

    fn spend_fields(
        remaining: &mut usize,
        since_cancel: &mut usize,
        cancel: &mut dyn FnMut(),
    ) -> bool {
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        *since_cancel += 1;
        if *since_cancel >= DYNAMIC_ARRAY_CANCEL_INTERVAL {
            *since_cancel = 0;
            cancel();
        }
        true
    }

    fn assignment_branch(&self, range: TextRange) -> Option<Option<TextRange>> {
        self.assignment_branches.get(&range).copied()
    }

    fn next_unconditional(
        &mut self,
        name: NormName,
        after: TextSize,
        before: TextSize,
        cancel: &mut dyn FnMut(),
    ) -> Option<TextSize> {
        let remaining = &mut self.remaining;
        let since_cancel = &mut self.since_cancel;
        let Some(assignments) = self.assignments.get(&name) else {
            return Some(before);
        };
        let mut next = None;
        for assignment in assignments {
            if !Self::spend_fields(remaining, since_cancel, cancel) {
                return None;
            }
            if assignment.range.start() > after
                && assignment.range.start() < before
                && assignment.branch.is_none()
            {
                next = Some(next.map_or(assignment.range.start(), |at: TextSize| {
                    at.min(assignment.range.start())
                }));
            }
        }
        Some(next.unwrap_or(before))
    }

    fn resolve(
        &mut self,
        name: NormName,
        before: TextSize,
        candidate_end: TextSize,
        cancel: &mut dyn FnMut(),
    ) -> (Vec<DynamicNotifyArrayAlias>, bool) {
        let mut pending = vec![(name, before, candidate_end)];
        let mut visited = FxHashSet::default();
        let mut results = Vec::new();
        while let Some((current_name, current_before, end_cap)) = pending.pop() {
            if !visited.insert((current_name, current_before, end_cap)) {
                continue;
            }
            let assignment_index = &self.assignments;
            let remaining = &mut self.remaining;
            let since_cancel = &mut self.since_cancel;
            let Some(all) = assignment_index.get(&current_name) else { continue };
            let mut assignments = Vec::new();
            for assignment in all {
                if !Self::spend_fields(remaining, since_cancel, cancel) {
                    return (Vec::new(), false);
                }
                if assignment.range.start() < current_before {
                    assignments.push(*assignment);
                }
            }
            let mut latest = None;
            for assignment in &assignments {
                if !self.spend(cancel) {
                    return (Vec::new(), false);
                }
                if latest.is_none_or(|old: DynamicArrayAssignment<'_>| {
                    assignment.range.start() > old.range.start()
                }) {
                    latest = Some(*assignment);
                }
            }
            let Some(latest) = latest else { continue };
            let mut selected = vec![latest];
            let mut selected_ranges = FxHashSet::default();
            selected_ranges.insert(latest.range);
            let mut branch_pending = vec![latest];
            while let Some(site) = branch_pending.pop() {
                let Some(branch) = site.branch else { continue };
                for assignment in &assignments {
                    if !self.spend(cancel) {
                        return (Vec::new(), false);
                    }
                    if branch.contains_range(assignment.range)
                        && selected_ranges.insert(assignment.range)
                    {
                        selected.push(*assignment);
                        branch_pending.push(*assignment);
                    }
                }
                let mut previous = None;
                for assignment in &assignments {
                    if !self.spend(cancel) {
                        return (Vec::new(), false);
                    }
                    if assignment.range.end() <= branch.start()
                        && previous.is_none_or(|old: DynamicArrayAssignment<'_>| {
                            assignment.range.start() > old.range.start()
                        })
                    {
                        previous = Some(*assignment);
                    }
                }
                if let Some(previous) = previous {
                    if selected_ranges.insert(previous.range) {
                        selected.push(previous);
                        branch_pending.push(previous);
                    }
                }
            }
            for assignment in selected {
                if !self.spend(cancel) {
                    return (Vec::new(), false);
                }
                let binding_start = assignment.range.start();
                match assignment.value {
                    Expr::New { .. } if is_array_constructor(self.body, assignment.value) => {
                        let Some(array_end) = self.next_unconditional(
                            current_name,
                            binding_start,
                            candidate_end,
                            cancel,
                        ) else {
                            return (Vec::new(), false);
                        };
                        results.push(DynamicNotifyArrayAlias {
                            array_name: current_name,
                            array_start: Some(binding_start),
                            array_end: end_cap.min(array_end),
                        });
                    }
                    Expr::Path(source_name) => {
                        let source = NormName::intern(source_name.as_str());
                        if source != current_name {
                            let Some(source_end) = self.next_unconditional(
                                source,
                                binding_start,
                                candidate_end,
                                cancel,
                            ) else {
                                return (Vec::new(), false);
                            };
                            pending.push((source, binding_start, end_cap.min(source_end)));
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut seen = FxHashSet::default();
        let mut unique = Vec::with_capacity(results.len());
        for alias in results {
            if !self.spend(cancel) {
                return (Vec::new(), false);
            }
            if seen.insert(alias) {
                unique.push(alias);
            }
        }
        (unique, true)
    }
}

fn extract_dynamic_notify_handlers(
    body: &Body,
    source_map: crate::body::SourceMapAt<'_>,
    candidate: &DynamicNotifyArrayCandidate,
    array_additions: &[(NormName, TextRange, ExprIdx)],
    alias_resolver: Option<&DynamicArrayResolver<'_>>,
    handlers: &mut Vec<Name>,
    handler_sites: &mut Vec<DynamicNotifySignatureHandler>,
) {
    // Only a top-level reassignment to a fresh array is an unconditional reset; a nested
    // reset cannot be pinpointed this way, and the branch scan below accounts for it.
    let reset_start = candidate.array_start.or_else(|| {
        body.body_stmts()
            .filter_map(|stmt_id| {
                let Stmt::Assign { target, value } = body.stmt(stmt_id) else { return None };
                let Expr::Path(name) = body.expr_idx(*target) else { return None };
                if NormName::intern(name.as_str()) != candidate.array_name
                    || !is_array_constructor(body, body.expr_idx(*value))
                {
                    return None;
                }
                let start = source_map.stmt_range(stmt_id)?.start();
                (start < candidate.constructor_start && start < candidate.array_end)
                    .then_some(start)
            })
            .max()
    });
    let sequence_start = reset_start.unwrap_or(TextSize::from(0));
    let branched_activity = dynamic_array_activity_in_branch(
        body,
        source_map,
        candidate,
        array_additions,
        sequence_start,
        candidate.array_end,
    );
    let uncertainty = dynamic_array_uncertain_writes(
        body,
        source_map,
        candidate,
        sequence_start,
        candidate.array_end,
        alias_resolver,
    );

    let mut values = array_additions
        .iter()
        .filter(|(name, range, _)| {
            *name == candidate.array_name
                && range.start() > sequence_start
                && range.start() < candidate.constructor_start
                && range.start() < candidate.array_end
        })
        .map(|(_, _, value)| *value)
        .collect::<Vec<_>>();
    values.extend(uncertainty.possible_values);

    // Without an unconditional reset the observed appends may be partial, so the exact
    // slot sequence is never trusted; the same holds when an append or a reset may run
    // conditionally, or an opaque write shifts the array contents.
    if reset_start.is_none() || branched_activity || uncertainty.uncertain {
        for value in values {
            let Some(name) = extract_string_literal(body, value) else { continue };
            if is_identifier_like(&name) {
                push_dynamic_notify_handler(
                    &name,
                    ExprId::from_idx(value),
                    candidate,
                    handlers,
                    handler_sites,
                );
            }
        }
        return;
    }

    let slots = bsl_platform::bundled_callback_description_slots();
    for (handler_slot, receiver_slot) in
        [(slots.handler, slots.receiver), (slots.error_handler, slots.error_receiver)]
    {
        let Some(handler) =
            values.get(handler_slot).and_then(|&idx| extract_string_literal(body, idx))
        else {
            continue;
        };
        if !is_identifier_like(&handler) {
            continue;
        }
        let receiver_syntax =
            values.get(receiver_slot).map_or(NotifyReceiverSyntax::Other, |&idx| {
                match body.expr_idx(idx) {
                    Expr::Path(name) => NotifyReceiverSyntax::for_bare_name(name),
                    _ => NotifyReceiverSyntax::Other,
                }
            });
        if matches!(receiver_syntax, NotifyReceiverSyntax::SelfName | NotifyReceiverSyntax::Other) {
            push_dynamic_notify_handler(
                &handler,
                ExprId::from_idx(values[handler_slot]),
                candidate,
                handlers,
                handler_sites,
            );
        }
    }
}

fn push_dynamic_notify_handler(
    name: &str,
    callback_expr: ExprId,
    candidate: &DynamicNotifyArrayCandidate,
    handlers: &mut Vec<Name>,
    handler_sites: &mut Vec<DynamicNotifySignatureHandler>,
) {
    let handler_name = Name::new(name);
    handlers.push(handler_name.clone());
    handler_sites.push(DynamicNotifySignatureHandler {
        handler_name,
        callback_expr,
        constructor_range: candidate.constructor_range,
    });
}

fn is_array_constructor(body: &Body, expr: &Expr) -> bool {
    match expr {
        Expr::New { type_name: Some(name), .. } => {
            matches!(name.as_str().fold_lower().as_str(), "массив" | "array")
        }
        Expr::New { type_name: None, args } => args
            .first()
            .and_then(|&idx| extract_string_literal(body, idx))
            .is_some_and(|name| matches!(name.fold_lower().as_str(), "массив" | "array")),
        _ => false,
    }
}

/// Whether a branch between `start` and the constructor conditionally executes an append
/// to, or a reset of, the candidate array. A branch that never touches the array leaves
/// the append sequence intact, so exact handler slots survive unrelated control flow;
/// a conditional append or reset makes the slot positions unknowable.
fn dynamic_array_activity_in_branch(
    body: &Body,
    source_map: crate::body::SourceMapAt<'_>,
    candidate: &DynamicNotifyArrayCandidate,
    array_additions: &[(NormName, TextRange, ExprIdx)],
    start: TextSize,
    end: TextSize,
) -> bool {
    let mut sequence_sites: Vec<TextRange> = array_additions
        .iter()
        .filter(|(name, range, _)| {
            *name == candidate.array_name
                && range.start() >= start
                && range.start() < candidate.constructor_start
                && range.start() < end
        })
        .map(|(_, range, _)| *range)
        .collect();
    for (stmt_id, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, value } = stmt else { continue };
        let Expr::Path(name) = body.expr_idx(*target) else { continue };
        if NormName::intern(name.as_str()) != candidate.array_name
            || !is_array_constructor(body, body.expr_idx(*value))
        {
            continue;
        }
        match source_map.stmt_range(stmt_id) {
            Some(range)
                if range.start() >= start
                    && range.start() < candidate.constructor_start
                    && range.start() < end =>
            {
                sequence_sites.push(range);
            }
            _ => {}
        }
    }
    if sequence_sites.is_empty() {
        return false;
    }
    body.stmts_iter().any(|(stmt_id, stmt)| {
        if !matches!(
            stmt,
            Stmt::If(_)
                | Stmt::PreprocIf(_)
                | Stmt::While { .. }
                | Stmt::For { .. }
                | Stmt::ForEach { .. }
                | Stmt::Try { .. }
        ) {
            return false;
        }
        let Some(branch_range) = source_map.stmt_range(stmt_id) else { return false };
        branch_range.start() >= start
            && branch_range.start() < candidate.constructor_start
            && branch_range.start() < end
            && sequence_sites.iter().any(|site| branch_range.contains_range(*site))
    })
}

/// Writes to the candidate array that the append sequence cannot account for: an indexed
/// write, a write through a local alias of the array, or a same-named member reached off
/// `ЭтотОбъект`. Any such write makes the exact slot sequence untrustworthy, and its
/// string value is a possible handler name.
#[derive(Debug, Default)]
struct DynamicArrayUncertainty {
    uncertain: bool,
    possible_values: Vec<ExprIdx>,
}

/// Whether `expr` may designate the candidate array itself: the variable, a local alias
/// bound to it, or a same-named member of `ЭтотОбъект`. A same-named member of any other
/// receiver is an unrelated object.
fn dynamic_array_alias_is_active(
    aliases: &FxHashMap<NormName, Vec<(TextSize, TextSize)>>,
    name: NormName,
    at: TextSize,
) -> bool {
    aliases
        .get(&name)
        .is_some_and(|intervals| intervals.iter().any(|(start, end)| *start <= at && at < *end))
}

fn expr_may_designate_candidate_array(
    body: &Body,
    expr: &Expr,
    candidate: &DynamicNotifyArrayCandidate,
    aliases: &FxHashMap<NormName, Vec<(TextSize, TextSize)>>,
    at: TextSize,
) -> bool {
    match expr {
        Expr::Path(name) => {
            let key = NormName::intern(name.as_str());
            key == candidate.array_name || dynamic_array_alias_is_active(aliases, key, at)
        }
        Expr::Field { base, field } => {
            NormName::intern(field.as_str()) == candidate.array_name
                && matches!(body.expr_idx(*base), Expr::Path(name)
                    if is_this_object(&name.as_str().fold_lower()))
        }
        _ => false,
    }
}

fn dynamic_array_uncertain_writes(
    body: &Body,
    source_map: crate::body::SourceMapAt<'_>,
    candidate: &DynamicNotifyArrayCandidate,
    start: TextSize,
    end: TextSize,
    alias_resolver: Option<&DynamicArrayResolver<'_>>,
) -> DynamicArrayUncertainty {
    let mut result = DynamicArrayUncertainty::default();

    // Track aliases in source order so an unconditional rebinding releases the old array.
    // Conditional rebinding keeps both possibilities because either arm may execute.
    let mut bindings = Vec::new();
    for (stmt_id, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, value } = stmt else { continue };
        let Expr::Path(target_name) = body.expr_idx(*target) else { continue };
        let Some(range) = source_map.stmt_range(stmt_id) else { continue };
        if range.start() < start || range.start() >= candidate.constructor_start {
            continue;
        }
        let value = match body.expr_idx(*value) {
            Expr::Path(value_name) => Some(NormName::intern(value_name.as_str())),
            _ => None,
        };
        bindings.push((range, NormName::intern(target_name.as_str()), value));
    }
    bindings.sort_unstable_by_key(|(range, _, _)| range.start());
    let mut active_aliases: FxHashSet<NormName> = FxHashSet::default();
    let mut open_aliases: FxHashMap<NormName, TextSize> = FxHashMap::default();
    let mut alias_intervals: FxHashMap<NormName, Vec<(TextSize, TextSize)>> = FxHashMap::default();
    for (range, target, value) in bindings {
        if target == candidate.array_name {
            continue;
        }
        let bound_to_candidate = value.is_some_and(|value| {
            (value == candidate.array_name && range.start() < end)
                || active_aliases.contains(&value)
        });
        let conditional = alias_resolver
            .and_then(|resolver| resolver.assignment_branch(range))
            .flatten()
            .is_some();
        if bound_to_candidate {
            if active_aliases.insert(target) {
                open_aliases.insert(target, range.start());
            }
        } else if !conditional && active_aliases.remove(&target) {
            if let Some(opened_at) = open_aliases.remove(&target) {
                alias_intervals.entry(target).or_default().push((opened_at, range.start()));
            }
        }
    }
    for alias in active_aliases {
        if let Some(opened_at) = open_aliases.remove(&alias) {
            alias_intervals
                .entry(alias)
                .or_default()
                .push((opened_at, candidate.constructor_start));
        }
    }

    for (stmt_id, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, value } = stmt else { continue };
        // An indexed write (`Параметры[0] = …`) targets its base receiver.
        let target_receiver = match body.expr_idx(*target) {
            Expr::Index { base, .. } => body.expr_idx(*base),
            other => other,
        };
        let Some(range) = source_map.stmt_range(stmt_id) else {
            // Without a position, any historical alias could have named the candidate.
            let maybe_alias = match target_receiver {
                Expr::Path(name) => alias_intervals.contains_key(&NormName::intern(name.as_str())),
                _ => false,
            };
            if maybe_alias
                || expr_may_designate_candidate_array(
                    body,
                    target_receiver,
                    candidate,
                    &alias_intervals,
                    start,
                )
            {
                result.uncertain = true;
            }
            continue;
        };
        let writes_through_alias = match body.expr_idx(*target) {
            Expr::Path(name) => dynamic_array_alias_is_active(
                &alias_intervals,
                NormName::intern(name.as_str()),
                range.start(),
            ),
            _ => matches!(target_receiver, Expr::Path(name)
            if dynamic_array_alias_is_active(
                &alias_intervals,
                NormName::intern(name.as_str()),
                range.start()
            )),
        };
        if range.start() < start
            || range.start() >= candidate.constructor_start
            || (range.start() >= end && !writes_through_alias)
        {
            continue;
        }
        match body.expr_idx(*target) {
            Expr::Path(name) => {
                let key = NormName::intern(name.as_str());
                // A reassignment to something other than a fresh array is opaque; a
                // fresh array is the reset the caller already accounts for.
                if (key == candidate.array_name
                    || dynamic_array_alias_is_active(&alias_intervals, key, range.start()))
                    && !is_array_constructor(body, body.expr_idx(*value))
                {
                    result.uncertain = true;
                }
            }
            _ if expr_may_designate_candidate_array(
                body,
                target_receiver,
                candidate,
                &alias_intervals,
                range.start(),
            ) =>
            {
                // An indexed or same-named qualified write replaces a slot the pass
                // cannot resolve, and its value is a possible handler name.
                result.uncertain = true;
                result.possible_values.push(*value);
            }
            _ => {}
        }
    }

    for (expr_id, expr) in body.exprs_iter() {
        let Expr::Call { callee, args } = expr else { continue };
        let range = source_map.expr_range(expr_id);
        let at = range.map_or(start, |range| range.start());
        let calls_through_alias = match body.expr_idx(*callee) {
            Expr::Field { base, .. } => matches!(body.expr_idx(*base), Expr::Path(name)
            if dynamic_array_alias_is_active(
                &alias_intervals,
                NormName::intern(name.as_str()),
                at
            )),
            _ => false,
        };
        let passes_alias = args.iter().any(|&arg| {
            matches!(body.expr_idx(arg), Expr::Path(name)
            if dynamic_array_alias_is_active(
                &alias_intervals,
                NormName::intern(name.as_str()),
                at
            ))
        });
        let in_window = range.is_some_and(|range| {
            range.start() >= start
                && range.start() < candidate.constructor_start
                && (range.start() < end || calls_through_alias || passes_alias)
        });
        if !in_window {
            continue;
        }
        // Handing the array to a call exposes it to effects this syntax pass cannot see.
        if args.iter().any(|&arg| {
            expr_may_designate_candidate_array(
                body,
                body.expr_idx(arg),
                candidate,
                &alias_intervals,
                at,
            )
        }) {
            result.uncertain = true;
        }
        let Expr::Field { base, field } = body.expr_idx(*callee) else { continue };
        let method = field.as_str().fold_lower();
        let is_append = method == "добавить" || method == "add";
        match body.expr_idx(*base) {
            Expr::Path(name) if NormName::intern(name.as_str()) == candidate.array_name => {
                // A call through the array variable other than a recognized append can
                // change its contents in a way this pass cannot place in a slot, and
                // any string it passes may end up in the array; appends are already
                // part of the sequence.
                if !is_append {
                    result.uncertain = true;
                    result.possible_values.extend(args.iter().copied());
                }
            }
            receiver
                if matches!(receiver, Expr::Path(name)
                if dynamic_array_alias_is_active(
                    &alias_intervals,
                    NormName::intern(name.as_str()),
                    at
                )) || expr_may_designate_candidate_array(
                    body,
                    receiver,
                    candidate,
                    &alias_intervals,
                    at,
                ) =>
            {
                // A call through an alias or a same-named member of `ЭтотОбъект` may
                // reach the same array: any string it passes is a possible handler
                // name, and the call itself leaves the append sequence untrustworthy.
                result.uncertain = true;
                result.possible_values.extend(args.iter().copied());
            }
            _ => {}
        }
    }

    result
}

fn is_notify_constructor_candidate(body: &Body, typed: bool, args: &[ExprIdx]) -> bool {
    if typed {
        args.iter().any(|&arg| matches!(body.expr_idx(arg), Expr::Literal(Literal::String(_))))
    } else {
        // Dynamic `New(type, parameters)` is only a candidate by shape. The type and
        // parameter-array semantics are established later by `hir-ty`.
        args.len() == 2
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
            notify_candidates: Vec::new(),
            notify_signature_handlers: Vec::new(),
            notify_additional_signature_handlers: Vec::new(),
            notify_dynamic_signature_handlers: Vec::new(),
            notify_dynamic_signature_handler_sites: Vec::new(),
            notify_dynamic_signature_analysis_incomplete: Vec::new(),
            notify_regs: vec![
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_expr: ExprId::from_raw(0.into()),
                    callback_name: Name::new("Обработчик"),
                    receiver_syntax: NotifyReceiverSyntax::SelfName,
                    target: NotifyTarget::ThisObject,
                    environments: NotifyRegistrationEnvironments::default(),
                    range: range(70),
                },
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_expr: ExprId::from_raw(0.into()),
                    callback_name: Name::new("Обработчик"),
                    receiver_syntax: NotifyReceiverSyntax::BareName,
                    target: NotifyTarget::Unsupported,
                    environments: NotifyRegistrationEnvironments::default(),
                    range: range(80),
                },
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_expr: ExprId::from_raw(0.into()),
                    callback_name: Name::new("ВнешнийОбработчик"),
                    receiver_syntax: NotifyReceiverSyntax::BareName,
                    target: NotifyTarget::Module(Name::new("Handlers")),
                    environments: NotifyRegistrationEnvironments::default(),
                    range: range(85),
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
            name_literal_ref_sites: Vec::new(),
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
            vec![
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_expr: ExprId::from_raw(0.into()),
                    callback_name: Name::new("Обработчик"),
                    receiver_syntax: NotifyReceiverSyntax::SelfName,
                    target: NotifyTarget::ThisObject,
                    environments: NotifyRegistrationEnvironments::default(),
                    range: zero,
                },
                NotifyReg {
                    caller: CallerId::Method(MethodKey::first("Главная")),
                    callback_expr: ExprId::from_raw(0.into()),
                    callback_name: Name::new("ВнешнийОбработчик"),
                    receiver_syntax: NotifyReceiverSyntax::BareName,
                    target: NotifyTarget::Module(Name::new("Handlers")),
                    environments: NotifyRegistrationEnvironments::default(),
                    range: zero,
                },
            ],
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
        assert!(subset.name_literal_ref_sites.is_empty());
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

    #[test]
    fn foreign_handler_bindings_keep_only_receivers_outside_the_module() {
        let parse = parser::parse(
            r#"
Процедура Подключить(Форма, Элемент) Экспорт
    Форма.ПодключитьОбработчикОжидания("ОбработчикОжидания1", 0.1, Истина);
    Форма.ОтключитьОбработчикОжидания("ОбработчикОжидания2");
    Форма.ПолучитьФорму().ПодключитьОбработчикОжидания("ОбработчикРезультата", 1);
    Элементы.Найти("Поле3").УстановитьДействие("ПриИзменении", "СвойОбработчик5");
    ЭтаФорма.ВладелецФормы.ПодключитьОбработчикОжидания("ОбработчикВладельца", 1);
    Форма.Элементы.Поле1.УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    Форма.ПодключитьОбработчикИзмененияДанных("Реквизит1", "ОбработчикИзмененияДанных", Ложь);
    ПодключитьОбработчикОжидания("СвойОбработчик", 1);
    ЭтаФорма.ПодключитьОбработчикОжидания("СвойОбработчик2", 1);
    Элементы.Поле2.УстановитьДействие("ПриИзменении", "СвойОбработчик3");
    Элемент.УстановитьДействие("ПриИзменении", "НеясныйПолучатель");
    Оповещение = Новый ОписаниеОповещения("СвойОбработчик4", ЭтотОбъект);
    Форма.ПодключитьОбработчикОжидания("не идентификатор", 1);
    Форма.ПодключитьОбработчикОжидания(Элемент.Имя, 1);
    Сообщить("ПростоСтрока");
КонецПроцедуры
"#,
        );
        let module_bodies = ModuleBodies::from_parse(&parse);
        let mut found = Vec::new();
        for (_, body) in module_bodies.iter_bodies() {
            collect_foreign_handler_bindings(body, &mut found);
        }
        found.sort();
        found.dedup();
        let bindings: Vec<(&str, Option<&str>)> = found
            .iter()
            .map(|b| (b.handler_name.as_str(), b.receiver_root.as_ref().map(Name::as_str)))
            .collect();
        assert_eq!(
            bindings,
            vec![
                ("ОбработчикВладельца", Some("ВладелецФормы")),
                ("ОбработчикИзмененияДанных", Some("Форма")),
                ("ОбработчикОжидания1", Some("Форма")),
                ("ОбработчикРезультата", None),
                ("Поле1ПриИзменении", Some("Форма")),
            ]
        );
    }

    #[test]
    fn foreign_handler_bindings_match_case_insensitively() {
        let mut names = FxHashSet::default();
        names.insert(NormName::intern("ОбработчикЗавершения"));
        let bindings = ForeignHandlerBindings::new(names);
        assert!(bindings.is_bound("обработчикзавершения"));
        assert!(bindings.is_bound("ОБРАБОТЧИКЗАВЕРШЕНИЯ"));
        assert!(!bindings.is_bound("ОбработчикЗавершения2"));
    }

    fn parse_and_extract(code: &str) -> ModuleCallSummary {
        parse_and_extract_with_handlers(code, &[])
    }

    fn parse_and_extract_with_handlers(
        code: &str,
        handlers: &[bsl_metadata::FormEventHandler],
    ) -> ModuleCallSummary {
        parse_and_extract_with_cancel_and_handlers(code, handlers, &mut || {})
    }

    fn parse_and_extract_with_cancel(code: &str, cancel: &mut dyn FnMut()) -> ModuleCallSummary {
        parse_and_extract_with_cancel_and_handlers(code, &[], cancel)
    }

    fn parse_and_extract_with_cancel_and_handlers(
        code: &str,
        handlers: &[bsl_metadata::FormEventHandler],
        cancel: &mut dyn FnMut(),
    ) -> ModuleCallSummary {
        let parse = parser::parse(code);
        let item_tree = ItemTree::from_parse(&parse);
        let module_bodies = ModuleBodies::from_parse(&parse);
        extract_call_summary_with_cancellation(&item_tree, &module_bodies, handlers, cancel)
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
    fn notify_receiver_syntax_recognizes_self_names_case_insensitively() {
        for name in [
            "ЭтотОбъект",
            "эТоТоБъЕкТ",
            "ThisObject",
            "THISOBJECT",
            "ЭтаФорма",
            "этаформа",
            "ThisForm",
            "thisform",
        ] {
            assert_eq!(
                NotifyReceiverSyntax::for_bare_name(&Name::new(name)),
                NotifyReceiverSyntax::SelfName,
                "{name}"
            );
        }
        for name in ["ДругойМодуль", "CommonModule", "ЭтотОбъектДругой"]
        {
            assert_eq!(
                NotifyReceiverSyntax::for_bare_name(&Name::new(name)),
                NotifyReceiverSyntax::BareName,
                "{name}"
            );
        }
    }

    #[test]
    fn raw_summary_keeps_generic_constructor_candidates_unverified() {
        let code = r#"
Перем Оповещение;
Оповещение = Новый ОписаниеОповещения("ОбработатьОповещение", ЭтотОбъект);

Procedure Test()
    Notification = New NotifyDescription("HandleResult", ThisObject);
    Other = New UnknownType("AnotherHandler", CommonModule);
    Params = New Array;
    Params.Add("DynamicHandler");
    Params.Add(ThisObject);
    Params.Add(Undefined);
    Params.Add("DynamicErrorHandler");
    Params.Add(ThisObject);
    Dynamic = New("CallbackDescription", Params);
    Positional = New("NotifyDescription", "PositionalHandler", ThisObject);
    PositionalWithError = New("NotifyDescription", "PrimaryHandler", ThisObject, Undefined, "ErrorHandler", ThisObject);
    CallbackDescriptionPositional = New("CallbackDescription", "NotProtectedBySource", ThisObject);
EndProcedure
"#;
        let summary = parse_and_extract(code);

        assert!(summary.notify_regs.is_empty(), "raw syntax candidates are not registrations");
        assert_eq!(
            summary.notify_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>(),
            [
                "HandleResult",
                "PositionalHandler",
                "PrimaryHandler",
                "ErrorHandler",
                "ОбработатьОповещение",
            ]
        );
        assert_eq!(
            summary
                .notify_additional_signature_handlers
                .iter()
                .map(Name::as_str)
                .collect::<Vec<_>>(),
            ["NotProtectedBySource"]
        );
        assert_eq!(
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>(),
            ["DynamicHandler", "DynamicErrorHandler"]
        );
        assert_eq!(summary.notify_candidates.len(), 4, "{:#?}", summary.notify_candidates);
        assert_eq!(summary.notify_candidates[0].caller, CallerId::Method(MethodKey::first("Test")));
        assert_eq!(summary.notify_candidates[1].caller, CallerId::Method(MethodKey::first("Test")));
        assert_eq!(summary.notify_candidates[2].caller, CallerId::Method(MethodKey::first("Test")));
        assert_eq!(summary.notify_candidates[3].caller, CallerId::ModuleCode);
        assert!(summary
            .notify_candidates
            .iter()
            .all(|candidate| candidate.range.start() < candidate.range.end()));
    }

    #[test]
    fn dynamic_callback_array_alias_reads_the_original_array_under_none() {
        let code = r#"
Процедура Тест()
    Перем Параметры;
    Перем Алиас;
    Параметры = Новый Массив;
    Алиас = Параметры;
    Параметры.Добавить("ИзИсходногоМассива");
    Параметры.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", Алиас);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert_eq!(
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>(),
            ["ИзИсходногоМассива"]
        );

        let negative_control =
            code.replace("Алиас = Параметры;", "Алиас = Параметры;\n    Параметры = Новый Массив;");
        let summary = parse_and_extract(&negative_control);
        assert!(summary.notify_dynamic_signature_handlers.is_empty(), "{summary:#?}");
    }

    #[test]
    fn dynamic_callback_array_alias_survives_source_rebinding_and_branching() {
        let code = r#"
Процедура Тест(Условие)
    Перем Параметры;
    Перем Копия;
    Перем Альтернативный;
    Параметры = Новый Массив;
    Параметры.Добавить("ДоКопирования");
    Параметры.Добавить(ЭтотОбъект);
    Альтернативный = Новый Массив;
    Альтернативный.Добавить("ИзВетви");
    Альтернативный.Добавить(ЭтотОбъект);
    Копия = Параметры;
    Параметры = Новый Массив;
    Если Условие Тогда
        Копия = Альтернативный;
    КонецЕсли;
    О = Новый("CallbackDescription", Копия);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let mut handlers =
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>();
        handlers.sort_unstable();
        assert_eq!(handlers, ["ДоКопирования", "ИзВетви"]);
    }

    #[test]
    fn dynamic_callback_array_alias_chain_survives_source_rebinding() {
        let code = r#"
Процедура Тест()
    Перем Источник;
    Перем Алиас;
    Перем Итог;
    Источник = Новый Массив;
    Источник.Добавить("ДоПерепривязки");
    Источник.Добавить(ЭтотОбъект);
    Алиас = Источник;
    Источник = Новый Массив;
    Итог = Алиас;
    Итог.Добавить("ЧерезЦепочку");
    Итог.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", Итог);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let mut handlers =
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>();
        handlers.sort_unstable();
        assert_eq!(handlers, ["ДоПерепривязки", "ЧерезЦепочку"]);

        let rebound = r#"
Процедура Тест()
    Перем Источник;
    Перем ДолгоживущийАлиас;
    Перем КраткийАлиас;
    Источник = Новый Массив;
    Источник.Добавить("ПравильныйОбработчик");
    Источник.Добавить(ЭтотОбъект);
    ДолгоживущийАлиас = Источник;
    Источник = Новый Массив;
    КраткийАлиас = ДолгоживущийАлиас;
    КраткийАлиас = Новый Массив;
    КраткийАлиас.Добавить("НеИзСтарогоМассива");
    КраткийАлиас.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", ДолгоживущийАлиас);
КонецПроцедуры
"#;
        let summary = parse_and_extract(rebound);
        assert!(
            !summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("НеИзСтарогоМассива"))),
            "a rebound alias must not contribute values to the old array: {summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_alias_chain_does_not_follow_source_after_reset() {
        let code = r#"
Процедура Тест()
    Перем Источник;
    Перем ДолгоживущийАлиас;
    Перем КопияАлиаса;
    Перем НовыйАлиас;
    Источник = Новый Массив;
    Источник.Добавить("СтарыйОбработчик");
    Источник.Добавить(ЭтотОбъект);
    ДолгоживущийАлиас = Источник;
    КопияАлиаса = ДолгоживущийАлиас;
    Источник = Новый Массив;
    НовыйАлиас = Источник;
    НовыйАлиас[0] = "НеИзСтарогоМассива";
    О = Новый("CallbackDescription", КопияАлиаса);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(
            summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("СтарыйОбработчик"))),
            "aliases captured before source reset must remain linked: {summary:#?}"
        );
        assert!(
            !summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("НеИзСтарогоМассива"))),
            "aliases copied from the source after reset must not reach the old array: {summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_array_alias_write_before_rebinding_is_preserved() {
        let code = r#"
Процедура Тест()
    Перем Параметры;
    Перем Алиас;
    Параметры = Новый Массив;
    Параметры.Добавить("Old");
    Параметры.Добавить(ЭтотОбъект);
    Алиас = Параметры;
    Алиас[0] = "Actual";
    Алиас = Новый Массив;
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(
            summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("Actual"))),
            "writes made through an alias before its rebinding must be retained: {summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_alias_branch_fanout_is_bounded_and_keeps_handler() {
        const ALIAS_COUNT: usize = 32;
        let mut code = String::from("Процедура Тест(Условие)\nПерем Root;\n");
        for index in 0..=ALIAS_COUNT {
            code.push_str(&format!("Перем Alias{index};\n"));
        }
        code.push_str(
            "Root = Новый Массив;\nRoot.Добавить(\"ОжидаемыйОбработчик\");\nRoot.Добавить(ЭтотОбъект);\nAlias0 = Root;\n",
        );
        for index in 1..=ALIAS_COUNT {
            code.push_str(&format!(
                "Если Условие Тогда\nAlias{index} = Alias{};\nИначе\nAlias{index} = Alias{};\nКонецЕсли;\n",
                index - 1,
                index - 1,
            ));
        }
        code.push_str(&format!(
            "О = Новый(\"CallbackDescription\", Alias{ALIAS_COUNT});\nКонецПроцедуры\n"
        ));

        let summary = parse_and_extract(&code);
        assert!(
            summary.notify_dynamic_signature_analysis_incomplete.is_empty(),
            "32 aliases with two branch bindings each must fit the bounded walk: {summary:#?}"
        );
        assert!(
            summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("ОжидаемыйОбработчик"))),
            "the handler reachable through either branch must be retained: {summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_alias_analysis_is_bounded_and_cancellable() {
        let mut code = String::from("Процедура Тест()\nПерем А;\n");
        for _ in 0..5_000 {
            code.push_str("А = НеизвестноеЗначение;\n");
        }
        code.push_str("О = Новый(\"CallbackDescription\", А);\nКонецПроцедуры\n");

        let mut cancellation_checks = 0;
        let summary = parse_and_extract_with_cancel(&code, &mut || cancellation_checks += 1);
        assert!(cancellation_checks > 1, "large scans must periodically check cancellation");
        assert!(
            summary.notify_dynamic_signature_handlers.is_empty(),
            "an incomplete alias analysis must not recommend handlers: {summary:#?}"
        );
        assert_eq!(summary.notify_dynamic_signature_analysis_incomplete.len(), 1);
        assert_eq!(
            summary.notify_dynamic_signature_analysis_incomplete[0].caller,
            CallerId::Method(MethodKey::first("Тест"))
        );
        assert_eq!(
            summary.notify_dynamic_signature_analysis_incomplete[0].array_name,
            Name::new("А")
        );
    }

    #[test]
    fn dynamic_callback_array_alias_writes_survive_source_rebinding() {
        let code = r#"
Процедура Тест()
    Перем Параметры;
    Перем Копия;
    Параметры = Новый Массив;
    Параметры.Добавить("ДоПерепривязки");
    Параметры.Добавить(ЭтотОбъект);
    Копия = Параметры;
    Параметры = Новый Массив;
    Копия.Добавить("ЧерезКопию");
    Копия.Добавить(ЭтотОбъект);
    О = Новый("CallbackDescription", Копия);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let mut handlers =
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>();
        handlers.sort_unstable();
        assert_eq!(handlers, ["ДоПерепривязки", "ЧерезКопию"]);

        // Rebinding and appending through the source changes a different object.
        let source_write =
            code.replace("Копия.Добавить(\"ЧерезКопию\");", "Параметры.Добавить(\"ЧужаяЗапись\");");
        let summary = parse_and_extract(&source_write);
        assert!(
            !summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("ЧужаяЗапись"))),
            "{summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_array_keeps_values_before_sequential_conditional_rebindings() {
        let code = r#"
Процедура Тест(Условие1, Условие2)
    Перем Параметры;
    Перем Запасной;
    Параметры = Новый Массив;
    Параметры.Добавить("Базовый");
    Параметры.Добавить(ЭтотОбъект);
    Если Условие1 Тогда
        Параметры = Запасной;
    КонецЕсли;
    Если Условие2 Тогда
        Параметры = Новый Массив;
        Параметры.Добавить("ИзВторойВетви");
        Параметры.Добавить(ЭтотОбъект);
    КонецЕсли;
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let mut handlers =
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>();
        handlers.sort_unstable();
        assert_eq!(handlers, ["Базовый", "ИзВторойВетви"]);

        let no_base = code.replace(
            "Параметры.Добавить(ЭтотОбъект);\n    Если Условие1 Тогда",
            "Параметры.Добавить(ЭтотОбъект);\n    Параметры = Новый Массив;\n    Если Условие1 Тогда",
        );
        let summary = parse_and_extract(&no_base);
        assert!(
            !summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("Базовый"))),
            "{summary:#?}"
        );
    }

    #[test]
    fn dynamic_callback_array_keeps_values_before_nested_conditional_rebindings() {
        let code = r#"
Процедура Тест(Первое, Второе)
    Перем Параметры;
    Перем Альт;
    Параметры = Новый Массив;
    Параметры.Добавить("Главный");
    Параметры.Добавить(ЭтотОбъект);
    Альт = Новый Массив;
    Альт.Добавить("ИзВетви");
    Альт.Добавить(ЭтотОбъект);
    Если Первое Тогда
        Параметры = Альт;
        Если Второе Тогда
            Параметры = Новый Массив;
        КонецЕсли;
    КонецЕсли;
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        let mut handlers =
            summary.notify_dynamic_signature_handlers.iter().map(Name::as_str).collect::<Vec<_>>();
        handlers.sort_unstable();
        assert_eq!(handlers, ["Главный", "ИзВетви"]);

        // If the only incoming array is definitely replaced, its handlers are not candidates.
        let reset =
            code.replace("Если Первое Тогда", "Параметры = Новый Массив;\n    Если Первое Тогда");
        let summary = parse_and_extract(&reset);
        assert!(
            !summary
                .notify_dynamic_signature_handlers
                .iter()
                .any(|handler| handler.eq_ignore_case(&Name::new("Главный"))),
            "{summary:#?}"
        );
    }

    #[test]
    fn raw_generic_candidates_do_not_resolve_receiver_shadowing() {
        let code = r#"
Перем ЭтотОбъект;
Процедура Тест(ЭтаФорма, МойОбщийМодуль)
    Перем Местный;
    А = Новый ОписаниеОповещения("Первый", этаформа);
    Б = Новый ОписаниеОповещения("Второй", местный);
    В = Новый ОписаниеОповещения("Третий", МойОбщийМодуль);
    МойОбщийМодуль = Новый Структура;
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert!(summary.notify_regs.is_empty());
        assert_eq!(summary.notify_candidates.len(), 3);
        assert!(summary.notify_candidates.iter().all(|candidate| {
            candidate.caller == CallerId::Method(MethodKey::first("Тест"))
        }));
    }

    #[test]
    fn raw_candidate_keeps_non_bare_receiver_for_semantic_filtering() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("ОбработатьРезультат", Объекты[0]);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);
        assert!(summary.notify_regs.is_empty());
        assert_eq!(summary.notify_candidates.len(), 1);
    }

    #[test]
    fn raw_candidate_does_not_assign_platform_callback_slots() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("Готово", ЭтотОбъект, Параметры, "ПриОшибке", МодульОшибок);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert!(summary.notify_regs.is_empty());
        assert_eq!(summary.notify_candidates.len(), 1);
    }

    #[test]
    fn raw_candidate_does_not_assume_an_error_handler_slot() {
        let code = r#"
Процедура Тест()
    Оповещение = Новый ОписаниеОповещения("Готово", ЭтотОбъект);
КонецПроцедуры
"#;
        let summary = parse_and_extract(code);

        assert!(summary.notify_regs.is_empty());
        assert_eq!(summary.notify_candidates.len(), 1);
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
