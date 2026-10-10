use base_db::FileIdInput;
use bsl_platform::deprecation::{self, ElementKind, Lookup};
use bsl_platform::security::Category as SecurityCategory;
use bsl_types::builders::Builders;
use bsl_types::facet::{DateComponent, FormDataFacet, MdoRefFacet, ProjectionSource};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{ConfigId, Projection, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use cfg_types::{BindingId, IdConversion};
use hir_def::body::Body;
use hir_def::effective_module::EffectiveModuleId;
use hir_def::hir::{BinaryOp, Expr, ExprIdx, Literal, Stmt, StmtIdx, UnaryOp};
use hir_def::module_interface::ModuleInterface;
use hir_def::resolver::Resolver;
use hir_def::{DefWithBodyId, ExprId, MethodIdInput, Name, StmtId};
use intern::NormName;
use once_cell::sync::Lazy;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use stdx::case::CaseExt;
use tracing::{debug, info, trace};
use vfs::FileId;

use crate::builtin;
use crate::db::HirDatabase;
use crate::lower::TyLoweringContext;
use crate::method_resolution;

static DEPRECATED_PLATFORM_MEMBER_OWNERS: Lazy<FxHashSet<String>> = Lazy::new(|| {
    let mut owners = FxHashSet::default();
    for entry in deprecation::registry().entries() {
        if !matches!(entry.element_kind, ElementKind::Method | ElementKind::Property) {
            continue;
        }
        if let Some(owner) = entry.owner {
            owners.insert(owner.ru.fold_lower());
            if !owner.en.is_empty() {
                owners.insert(owner.en.fold_lower());
            }
        }
    }
    owners
});

/// Heap-size estimators wired into salsa's `heap_size` hook (the `salsa_unstable`
/// memory report). salsa's default reports only the fixed slot stack size — the
/// `Arc` pointer — so the collections behind these memoised results are otherwise
/// invisible. These return an approximate live-heap byte count: hashbrown table
/// capacity is derived from length (load factor 7/8, rounded to a power of two),
/// owned `String`/`Vec` payloads are summed, and small `Copy` ids are counted by
/// `size_of`. Over-approximate by design; the goal is a per-ingredient memory map,
/// not exact accounting.
pub(crate) mod heap_estimate {
    use super::*;
    use std::mem::size_of;

    pub(crate) use stdx::heap::{map_table_bytes, vec_bytes};

    /// Heap of the `expr`/`binding`-keyed maps plus their owned-string and
    /// nested-vec payloads, shared by all three inference-result shapes.
    fn body_maps_heap(
        var_types: &FxHashMap<String, TypeId>,
        implicit_locals: &FxHashMap<String, ImplicitLocalInfo>,
        binding_types: &FxHashMap<BindingId, TypeId>,
        expr_types: &FxHashMap<ExprId, TypeId>,
        diagnostics_len: usize,
        call_arg_bindings_len: usize,
    ) -> usize {
        let mut b = map_table_bytes::<ExprId, TypeId>(expr_types.len());
        b += map_table_bytes::<BindingId, TypeId>(binding_types.len());
        b += map_table_bytes::<String, TypeId>(var_types.len());
        for k in var_types.keys() {
            b += k.capacity();
        }
        b += map_table_bytes::<String, ImplicitLocalInfo>(implicit_locals.len());
        for (k, info) in implicit_locals {
            b += k.capacity() + vec_bytes::<ImplicitLocalAssignment>(info.assignments.len());
        }
        b += vec_bytes::<InferenceDiagnostic>(diagnostics_len);
        b += vec_bytes::<CallArgBinding>(call_arg_bindings_len);
        b
    }

    pub(crate) fn inference_result_heap(v: &Arc<InferenceResult>) -> usize {
        let r = &**v;
        let mut b = size_of::<InferenceResult>();
        b +=
            map_table_bytes::<DefWithBodyId, FxHashMap<ExprId, TypeId>>(r.expr_types_by_body.len());
        for inner in r.expr_types_by_body.values() {
            b += map_table_bytes::<ExprId, TypeId>(inner.len());
        }
        b += map_table_bytes::<DefWithBodyId, FxHashMap<BindingId, TypeId>>(
            r.binding_types_by_body.len(),
        );
        for inner in r.binding_types_by_body.values() {
            b += map_table_bytes::<BindingId, TypeId>(inner.len());
        }
        b += map_table_bytes::<String, TypeId>(r.var_types.len());
        for k in r.var_types.keys() {
            b += k.capacity();
        }
        b += map_table_bytes::<DefWithBodyId, FxHashMap<String, ImplicitLocalInfo>>(
            r.implicit_locals_by_body.len(),
        );
        for inner in r.implicit_locals_by_body.values() {
            b += map_table_bytes::<String, ImplicitLocalInfo>(inner.len());
            for (k, info) in inner {
                b += k.capacity() + vec_bytes::<ImplicitLocalAssignment>(info.assignments.len());
            }
        }
        b += vec_bytes::<(DefWithBodyId, InferenceDiagnostic)>(r.diagnostics.len());
        b += vec_bytes::<CallArgBinding>(r.call_arg_bindings.len());
        b
    }

    pub(crate) fn body_inference_result_heap(v: &Arc<BodyInferenceResult>) -> usize {
        let r = &**v;
        size_of::<BodyInferenceResult>()
            + body_maps_heap(
                &r.var_types,
                &r.implicit_locals,
                &r.binding_types,
                &r.expr_types,
                r.diagnostics.len(),
                r.call_arg_bindings.len(),
            )
            + vec_bytes::<ExprId>(r.return_expr_ids.len())
    }

    pub(crate) fn module_code_inference_result_heap(v: &Arc<ModuleCodeInferenceResult>) -> usize {
        let r = &**v;
        size_of::<ModuleCodeInferenceResult>()
            + body_maps_heap(
                &r.var_types,
                &r.implicit_locals,
                &r.binding_types,
                &r.expr_types,
                r.diagnostics.len(),
                r.call_arg_bindings.len(),
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InferenceResult {
    pub expr_types_by_body: FxHashMap<DefWithBodyId, FxHashMap<ExprId, TypeId>>,

    pub var_types: FxHashMap<String, TypeId>,

    pub binding_types_by_body: FxHashMap<DefWithBodyId, FxHashMap<BindingId, TypeId>>,

    pub implicit_locals_by_body: FxHashMap<DefWithBodyId, FxHashMap<String, ImplicitLocalInfo>>,

    pub diagnostics: Vec<(DefWithBodyId, InferenceDiagnostic)>,

    pub call_arg_bindings: Vec<CallArgBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplicitLocalInfo {
    pub name: Name,
    pub first_assignment: ExprId,
    pub ty: TypeId,
    pub assignments: Vec<ImplicitLocalAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplicitLocalAssignment {
    pub target: ExprId,
    pub ty: TypeId,
}

/// One definition reaching a use: the statement that made it, and the type
/// inference recorded for the value it puts in the variable.
///
/// `stmt` is `None` for a definition that is not a statement at all — a
/// parameter, a bare `Перем`, a loop binding. `ty` is `None` when inference
/// recorded no type for it, which is the ordinary answer for a BSL parameter:
/// the language declares none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReachingDef {
    stmt: Option<StmtId>,
    ty: Option<TypeId>,
}

impl InferenceResult {
    pub fn type_id_of_expr_in(&self, owner: DefWithBodyId, expr: ExprId) -> Option<TypeId> {
        self.expr_types_by_body.get(&owner)?.get(&expr).copied()
    }

    pub fn type_id_of_binding_in(&self, owner: DefWithBodyId, id: BindingId) -> Option<TypeId> {
        self.binding_types_by_body.get(&owner)?.get(&id).copied()
    }

    pub fn has_diagnostics(&self) -> bool {
        !self.diagnostics.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareNameUse {
    ReadValue,
    AssignmentTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareNameOrigin {
    SelfName,
    FlowLocal,
    UserBinding,
    Builtin,
    FormMember,
    ObjectMember,
    CommonModule,
    GlobalExport,
    ManagerCollection,
    PlatformProperty,
    PlatformSystemEnum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareNameGap {
    MissingConfigurationRoot,
    UnreadUserGlobalSurface,
    MissingPlatformCatalog,
    UnverifiedPlatformCatalog,
    UnsupportedTargetPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BareNameResolution {
    Resolved { ty: TypeId, origin: BareNameOrigin, availability: hir_def::execution_env::EnvFlags },
    Absent,
    Indeterminate { gaps: Vec<BareNameGap> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BareNameVerdict {
    Resolved,
    Absent,
    Indeterminate,
}

/// The receiver a bare name implicitly hangs off in the module being inferred.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImplicitSelfSurface {
    /// The module has no implicit receiver, so the bare cascade is the whole story.
    None,
    Receiver(TypeId),
    /// A receiver this crate types only for member access, not as a value: its
    /// surface is the methods the platform catalog lists under these type names.
    PlatformTypes(Vec<&'static str>),
    /// A receiver whose methods the platform catalog files under a per-object
    /// prefix (`КонстантаМенеджерЗначения.<Имя>`), the same for every object.
    PlatformPrefix(&'static str),
    /// An implicit receiver exists but its surface cannot be produced — a gap, not
    /// an absence.
    Unreadable,
}

/// The catalog prefix of a constant's value manager — the receiver of its
/// `ValueManagerModule`.
const CONSTANT_VALUE_MANAGER_PREFIX: &str = "ConstantValueManager";

/// Global functions the platform still compiles although neither its help nor the
/// bundled catalog lists them: 8.2-era names kept for compatibility. Checked live on
/// 8.3.17.1549 and 8.3.27.2214 (compatibility mode 8.3.17), external data processors
/// built with 8.3.17:
/// - `КодЛокализации()` / `LocaleCode()` compiles on the server and in the thick
///   client and returns the infobase locale ("ru_UA");
/// - `УстановитьЗаголовокПриложения()`, `ПолучитьЗаголовокПриложения()` and their
///   English spellings compile in a thin-client form module;
/// - `УстановитьЗаголовокСистемы()`, `ПолучитьЗаголовокСистемы()` compile in the
///   thick client of an ordinary application (not on the server; the guessed English
///   spellings `SetSystemTitle` / `GetSystemTitle` compile nowhere and are left out).
///
/// Typical configurations call every one of them, so a bare call of one is never
/// reported absent, in whatever environment. Silence only: the call keeps the type it
/// had.
const UNDOCUMENTED_PLATFORM_GLOBALS: &[&str] = &[
    "КодЛокализации",
    "LocaleCode",
    "УстановитьЗаголовокПриложения",
    "SetApplicationCaption",
    "ПолучитьЗаголовокПриложения",
    "GetApplicationCaption",
    "УстановитьЗаголовокСистемы",
    "ПолучитьЗаголовокСистемы",
];

fn is_undocumented_platform_global(name: &str) -> bool {
    UNDOCUMENTED_PLATFORM_GLOBALS.iter().any(|global| global.to_lowercase() == name.to_lowercase())
}

fn classify_bare_name_miss(
    mut gaps: Vec<BareNameGap>,
    platform_status: bsl_platform::PlatformCatalogStatus,
) -> BareNameResolution {
    match platform_status {
        bsl_platform::PlatformCatalogStatus::Missing => {
            gaps.push(BareNameGap::MissingPlatformCatalog)
        }
        bsl_platform::PlatformCatalogStatus::Unverified => {
            gaps.push(BareNameGap::UnverifiedPlatformCatalog)
        }
        bsl_platform::PlatformCatalogStatus::UnsupportedTarget => {
            gaps.push(BareNameGap::UnsupportedTargetPlatform)
        }
        bsl_platform::PlatformCatalogStatus::Complete => {}
    }

    if gaps.is_empty() {
        BareNameResolution::Absent
    } else {
        BareNameResolution::Indeterminate { gaps }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceDiagnostic {
    UnresolvedName {
        expr: ExprId,
        name: Name,
    },

    UnresolvedMethodCall {
        expr: ExprId,
        receiver_name: Name,
        method_name: Name,
        kind: UnresolvedMethodKind,
    },

    /// A bare `Имя(...)` — no receiver — whose name is owned by nothing: not a
    /// method of this module, not a method of the module's implicit receiver, not
    /// an export of a global common or application module, not a platform global.
    /// Separate from [`InferenceDiagnostic::UnresolvedMethodCall`], which always
    /// names the receiver the method was looked for on; there is no receiver here.
    ///
    /// Reported only on the `Absent` verdict of the bare-name cascade, so a miss
    /// under any [`BareNameGap`] says nothing.
    UnresolvedBareCall {
        expr: ExprId,
        name: Name,
    },

    MismatchedArgCount {
        call_expr: ExprId,
        required_count: usize,
        total_count: usize,
        found: usize,
    },

    TypeMismatch {
        expr: ExprId,
        expected: TypeId,
        actual: TypeId,
        from_doc_comment: bool,
    },

    UnresolvedField {
        expr: ExprId,
        receiver_ty: TypeId,
        field_name: Name,
    },

    ReadOnlyPropertyAssignment {
        lhs: ExprId,
        receiver_ty: TypeId,
        field_name: Name,
    },

    /// Assignment to a bare Global-context property (`Справочники = …`). The
    /// platform refuses the write and declares no local, so the statement has no
    /// effect a reader could rely on — and the name keeps denoting the global.
    GlobalPropertyNotWritable {
        lhs: ExprId,
        name: Name,
    },

    DeprecatedPlatformMember {
        expr: ExprId,
        type_name: Name,
        member_name: Name,
        is_property: bool,
    },

    RedundantAccessToObjectTwoLevel {
        expr: ExprId,
        module: Name,
    },

    MissedRequiredParameterCommonModule {
        expr: ExprId,
        callee: Name,
        module: Name,
        args: Vec<bool>,
    },

    /// A manager-module call missing a required parameter. Mirrors
    /// `MissedRequiredParameterCommonModule`: inference decides that the chain IS a
    /// manager call (which the ownership verdict settles), the adapter reads the
    /// signature and names what is missing.
    MissedRequiredParameterManagerModule {
        expr: ExprId,
        callee: Name,
        mdo_type: Name,
        mdo_name: Name,
        args: Vec<bool>,
    },

    /// `Справочники.Товары.Метод()` written INSIDE the manager module of `Товары`:
    /// the object is already the module's own, so the chain is a detour. Decided here
    /// and not in lowering, because it takes the ownership verdict — a module variable
    /// named `Справочники` makes the same three tokens something else entirely, and
    /// lowering cannot know that.
    RedundantAccessToObjectThreeLevel {
        expr: ExprId,
        mdo_type: Name,
        mdo_name: Name,
    },

    /// A resolved platform member the catalog dates later than the project's
    /// minimum platform release. A bare global call then breaks compilation of the
    /// module on that release («Процедура или функция с указанным именем не
    /// определена», checked live on 8.3.17); a member of a typed value is late-bound
    /// and fails when executed. Raised only when `min_platform_version` is set and
    /// the member's version is known.
    PlatformMemberNewerThanMinVersion {
        expr: ExprId,
        name: Name,
        introduced: bsl_platform::PlatformVersion,
        minimum: bsl_platform::PlatformVersion,
    },

    /// A platform name the configuration's compatibility mode hides: below
    /// `visible_from` the platform does not see it at all, whatever its release. A
    /// global then breaks compilation of the module («Процедура или функция с
    /// указанным именем не определена», «Переменная не определена»); the one hidden
    /// type member (`Запрос.ТребуемаяАктуальностьДанных`) fails when executed. Raised
    /// only when the mode is known (setting or `Configuration.xml`).
    PlatformMemberHiddenByCompatibilityMode {
        expr: ExprId,
        name: Name,
        mode: bsl_platform::PlatformVersion,
        visible_from: bsl_platform::PlatformVersion,
    },

    /// A resolved platform member is not available in some of the execution
    /// environments this body runs in (`missing` — the EDT-style
    /// "[Web client]" qualifier set).
    UnavailableInEnvironment {
        expr: ExprId,
        name: Name,
        member_kind: EnvMemberKind,
        missing: hir_def::execution_env::EnvFlags,
    },

    /// User code (a common module or a same-module method) is called from an
    /// execution environment it is not compiled for: a server-only common
    /// module without `ВызовСервера` called from client code, or a
    /// `&НаКлиенте` form method called from server-side code.
    ModuleAccessibility {
        expr: ExprId,
        name: Name,
        callee_kind: EnvCalleeKind,
        missing: hir_def::execution_env::EnvFlags,
    },
    /// A call the security registry guards, already judged by who owns the
    /// name: a bare call only when nothing in the workspace shadows it, a
    /// qualified one only when the receiver is a declared owner. Platform types
    /// spell methods like the guarded ones without doing what they do, so the
    /// spelling alone never decides. The category travels as data — the
    /// diagnostic code is picked by the projection, not by this crate.
    GuardedCall {
        expr: ExprId,
        category: bsl_platform::security::Category,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvMemberKind {
    Method,
    Property,
    GlobalFunction,
    GlobalProperty,
    GlobalVariable,
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvCalleeKind {
    CommonModule,
    LocalMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedMethodKind {
    MethodNotFound,
    MethodNotExport,
    /// `ЭтотОбъект.Метод()` / `ЭтаФорма.Метод()` in a form module reaches a
    /// method of the module that is not exported: the platform exposes only
    /// exported methods through the self reference.
    SelfMethodNotExport,
    /// A body of the callee module exists but could not be read. Travels as a
    /// resolution outcome only: the call is left undiagnosed, because everything
    /// that could be said about it would be said about the wrong file.
    BodyUnread,
    ReceiverNotResolved,
    /// The receiver root is proven absent. IDE dispatch keeps this legacy
    /// fallback only while the replacement `UnresolvedName` rule is disabled.
    ReceiverNameAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallArgBinding {
    pub owner: DefWithBodyId,

    pub call_expr: ExprId,

    pub args: Vec<ExprId>,

    pub candidate: CandidateCallBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCallBinding {
    pub candidates: crate::call_resolution::CallCandidateSet,

    pub resolution: crate::call_resolution::CallResolution,
}

#[derive(Debug, Clone, Copy)]
struct GlobalCallableVariant {
    method: hir_def::MethodId,
    host: hir_def::resolver::GlobalExportHost,
}

#[derive(Debug, Clone)]
struct GlobalCallableExport {
    variants: Vec<GlobalCallableVariant>,
}

#[derive(Debug, Clone, Copy)]
struct GlobalReadableExport {
    availability: hir_def::execution_env::EnvFlags,
}

pub struct InferenceContext<'db> {
    db: &'db dyn HirDatabase,

    /// File whose configuration/metadata/cross-module context governs resolution.
    /// For an ordinary module this is the module's own file; for an effective
    /// `&ИзменениеИКонтроль` module it stays the BASE file (the effective module *is*
    /// the base module with the extension's edits applied). Same-module method/variable
    /// lookups are NOT keyed on this — they go through `local_symbols`.
    context_file_id: FileId,

    /// Effective module's symbol tree, present only when inferring a spliced
    /// `&ИзменениеИКонтроль` module. `None` for every ordinary module, in which case
    /// same-module lookups go to the declarations of `ModuleId{context_file_id}` by
    /// name, one memo per declaration.
    local_symbols: Option<Arc<ModuleInterface>>,

    /// Effective return types of the module's own methods, keyed by `MethodId.local_id`,
    /// present only on the SECOND pass of effective inference. A bare same-module call to
    /// a `&ИзменениеИКонтроль` target must see its *changed* body's return, not the base
    /// body's (which `materialise_signature_enriched` would re-derive via the base-keyed
    /// `method_return_type_query`). `None` for ordinary modules and the first effective
    /// pass → byte-identical default behavior.
    local_effective_returns: Option<Arc<FxHashMap<hir_def::MethodKey, TypeId>>>,

    /// Paired base module of a configuration-*extension* module, present only when
    /// inferring an extension module's OWN bodies under weaving (`&Вместо`/`&Перед`/
    /// `&После`). A bare same-module call/variable that misses in the extension's own
    /// symbols retries against this base module (the extension shadows the base). `None`
    /// for ordinary and effective modules → byte-identical default resolution.
    weaving_base: Option<hir_def::ModuleId>,

    /// Return type a bare `ПродолжитьВызов(...)` / `ProceedWithCall(...)` call yields in this
    /// body, present only when inferring a `&Вместо` (Around) interceptor under weaving. The
    /// call invokes the original base method `M`, so its result is `M`'s return type, not the
    /// platform global `ПродолжитьВызов`'s generic return. `None` for ordinary, effective,
    /// `&Перед`/`&После`, and module-code bodies → byte-identical platform-default typing.
    proceed_return: Option<TypeId>,

    /// Arity `(required_count, total_count)` of the base method a bare `ПродолжитьВызов(...)`
    /// re-enters, present only when inferring a `&Вместо` (Around) interceptor under weaving.
    /// The call must pass a valid argument count for the base method; unlike
    /// [`Self::proceed_return`] this is set whenever the base method resolves (a procedure base
    /// has no informative return but its arity is still checked). `None` everywhere else.
    proceed_arity: Option<(usize, usize)>,

    owner: DefWithBodyId,

    body: Arc<Body>,

    return_expr_ids: Vec<ExprId>,

    var_types: FxHashMap<NormName, TypeId>,

    implicit_locals: FxHashMap<String, ImplicitLocalInfo>,

    assigned_var_names: rustc_hash::FxHashSet<NormName>,

    /// Every bare assignment target anywhere in the body, collected once so a
    /// global read can skip CFG/reaching-definitions unless shadowing is possible.
    assignment_target_names: rustc_hash::FxHashSet<NormName>,

    /// Memoised `bare_global_name_claim` verdicts. That lookup reaches past the
    /// body into the module and the workspace while the body itself does not
    /// change during a run, and the same collection root recurs throughout a
    /// body, so it is asked once per name.
    shadowed_root_names: FxHashMap<NormName, bool>,

    binding_types: FxHashMap<BindingId, TypeId>,

    expr_types: FxHashMap<ExprId, TypeId>,

    diagnostics: Vec<InferenceDiagnostic>,

    /// Bare path expressions used as assignment targets. They keep normal type
    /// inference, but an absent name in this position is never diagnostic.
    assignment_targets: rustc_hash::FxHashSet<ExprId>,

    /// Exact bare-root verdicts keyed by the root expression. Qualified-call
    /// dispatch consumes the same verdict so an absent root is not also reported
    /// as `UnresolvedMethodCall::ReceiverNotResolved`.
    bare_name_verdicts: FxHashMap<ExprId, BareNameVerdict>,

    /// Calls whose callee module has a body that exists but could not be read.
    /// Resolution carries on past it, so the receiver keeps whatever type its
    /// platform surface gives; what is barred is any later verdict that the call
    /// is unresolved, which would be filed against the calling file.
    calls_with_unread_target: rustc_hash::FxHashSet<ExprId>,

    call_arg_bindings: Vec<CallArgBinding>,

    /// Execution environments this body runs in (module base ∩ compilation
    /// directive). Empty = unknown → availability checks are skipped.
    body_env: hir_def::execution_env::EnvFlags,

    /// Environments availability diagnostics may report on
    /// ([`EnvOptions::checked_environments`]) — the missing set is clipped to
    /// this mask, keeping opt-in environments (external connection, mobile)
    /// out of default verdicts.
    checked_env: hir_def::execution_env::EnvFlags,

    /// The statement walk is inside a `#Если` branch that nothing proves compiled:
    /// either it leaves no environment of a known body, or the body's environment
    /// is unknown (an empty [`Self::body_env`], as in effective and weaving bodies),
    /// so the branch may be compiled nowhere.
    in_uncompiled_branch: bool,

    /// Statements walked while [`Self::in_uncompiled_branch`] held.
    uncompiled_stmts: Vec<StmtId>,

    /// The project's `min_platform_version`, parsed once per body; `None` turns
    /// the min-platform check off entirely.
    min_platform: Option<bsl_platform::PlatformVersion>,

    /// The configuration's compatibility mode, parsed once per body; `None` (no
    /// mode known, or `DontUse`) turns the compatibility-mode check off.
    compat_mode: Option<bsl_platform::PlatformVersion>,

    /// Lazily-built, usage-aware user global surface. Both maps are constructed
    /// together so global modules are enumerated at most once per inference run.
    global_exports: Option<Arc<FxHashMap<NormName, GlobalCallableExport>>>,
    global_read_exports: Option<Arc<FxHashMap<NormName, GlobalReadableExport>>>,
    /// An existing visible global body was unread, so call shadowing cannot be
    /// decided without guessing. Missing configuration/index coverage still bars
    /// absence diagnostics, but preserves the historical positive platform-call
    /// fallback used by config-less editor buffers.
    global_surface_partly_unknown: bool,
    global_surface_incomplete: bool,

    /// Per-body literal `Структура` shapes (keys built via `Новый Структура` / `.Вставить`), keyed
    /// by lowercased local name. Collected once at the start of [`Self::infer_all`] and used to
    /// enrich a structure local's type with its keys on each read. Empty for bodies with no such
    /// construction → byte-identical default typing. See [`crate::structure_keys`].
    structure_shapes: FxHashMap<String, crate::structure_keys::StructureShape>,

    /// Physical-flag accessibility verdict per callee common module, so a body
    /// calling the same module many times reads its metadata once instead of
    /// recording a salsa dependency per call. `None` — not a common module
    /// (skip); `Some((env, server_call))` — feed the environment compare.
    callee_module_env: FxHashMap<FileId, Option<(hir_def::execution_env::EnvFlags, bool)>>,

    /// [`method_body_env`] per same-module callee, for the local cross-directive
    /// accessibility check.
    local_callee_env: FxHashMap<hir_def::MethodKey, hir_def::execution_env::EnvFlags>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyInferenceResult {
    pub owner: DefWithBodyId,
    pub var_types: FxHashMap<String, TypeId>,
    pub implicit_locals: FxHashMap<String, ImplicitLocalInfo>,
    pub binding_types: FxHashMap<BindingId, TypeId>,
    pub expr_types: FxHashMap<ExprId, TypeId>,
    pub diagnostics: Vec<InferenceDiagnostic>,
    pub call_arg_bindings: Vec<CallArgBinding>,
    /// Statements walked inside a `#Если` branch nothing proves compiled (see
    /// `InferenceContext::in_uncompiled_branch`): the syntax-level checks that
    /// must stay silent there read it.
    pub uncompiled_stmts: Vec<StmtId>,

    pub(crate) return_expr_ids: Vec<ExprId>,
}

impl BodyInferenceResult {
    pub fn empty_for(owner: DefWithBodyId) -> Self {
        Self {
            owner,
            var_types: FxHashMap::default(),
            implicit_locals: FxHashMap::default(),
            binding_types: FxHashMap::default(),
            expr_types: FxHashMap::default(),
            diagnostics: Vec::new(),
            call_arg_bindings: Vec::new(),
            uncompiled_stmts: Vec::new(),
            return_expr_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCodeInferenceResult {
    pub owner: DefWithBodyId,
    pub var_types: FxHashMap<String, TypeId>,
    pub implicit_locals: FxHashMap<String, ImplicitLocalInfo>,
    pub binding_types: FxHashMap<BindingId, TypeId>,
    pub expr_types: FxHashMap<ExprId, TypeId>,
    pub diagnostics: Vec<InferenceDiagnostic>,
    pub call_arg_bindings: Vec<CallArgBinding>,
    /// See [`BodyInferenceResult::uncompiled_stmts`].
    pub uncompiled_stmts: Vec<StmtId>,
}

impl Default for ModuleCodeInferenceResult {
    fn default() -> Self {
        Self {
            owner: DefWithBodyId::ModuleCode,
            var_types: FxHashMap::default(),
            implicit_locals: FxHashMap::default(),
            binding_types: FxHashMap::default(),
            expr_types: FxHashMap::default(),
            diagnostics: Vec::new(),
            call_arg_bindings: Vec::new(),
            uncompiled_stmts: Vec::new(),
        }
    }
}

impl ModuleCodeInferenceResult {
    pub fn from_body(body: BodyInferenceResult) -> Self {
        debug_assert!(
            matches!(body.owner, DefWithBodyId::ModuleCode),
            "ModuleCodeInferenceResult::from_body requires DefWithBodyId::ModuleCode owner"
        );
        Self {
            owner: body.owner,
            var_types: body.var_types,
            implicit_locals: body.implicit_locals,
            binding_types: body.binding_types,
            expr_types: body.expr_types,
            diagnostics: body.diagnostics,
            call_arg_bindings: body.call_arg_bindings,
            uncompiled_stmts: body.uncompiled_stmts,
        }
    }
}

#[derive(Debug, Clone)]
pub enum InferOwnerResult {
    Method(Arc<BodyInferenceResult>),
    ModuleCode(Arc<ModuleCodeInferenceResult>),
}

impl InferOwnerResult {
    pub fn owner(&self) -> DefWithBodyId {
        match self {
            InferOwnerResult::Method(r) => r.owner,
            InferOwnerResult::ModuleCode(r) => r.owner,
        }
    }

    pub fn type_id_of_expr(&self, expr: ExprId) -> Option<TypeId> {
        match self {
            InferOwnerResult::Method(r) => r.expr_types.get(&expr).copied(),
            InferOwnerResult::ModuleCode(r) => r.expr_types.get(&expr).copied(),
        }
    }

    pub fn expr_types(&self) -> &FxHashMap<ExprId, TypeId> {
        match self {
            InferOwnerResult::Method(r) => &r.expr_types,
            InferOwnerResult::ModuleCode(r) => &r.expr_types,
        }
    }

    pub fn type_id_of_binding(&self, binding: BindingId) -> Option<TypeId> {
        match self {
            InferOwnerResult::Method(r) => r.binding_types.get(&binding).copied(),
            InferOwnerResult::ModuleCode(r) => r.binding_types.get(&binding).copied(),
        }
    }

    pub fn var_types(&self) -> &FxHashMap<String, TypeId> {
        match self {
            InferOwnerResult::Method(r) => &r.var_types,
            InferOwnerResult::ModuleCode(r) => &r.var_types,
        }
    }

    pub fn implicit_locals(&self) -> &FxHashMap<String, ImplicitLocalInfo> {
        match self {
            InferOwnerResult::Method(r) => &r.implicit_locals,
            InferOwnerResult::ModuleCode(r) => &r.implicit_locals,
        }
    }

    pub fn binding_types(&self) -> &FxHashMap<BindingId, TypeId> {
        match self {
            InferOwnerResult::Method(r) => &r.binding_types,
            InferOwnerResult::ModuleCode(r) => &r.binding_types,
        }
    }

    pub fn diagnostics(&self) -> &[InferenceDiagnostic] {
        match self {
            InferOwnerResult::Method(r) => &r.diagnostics,
            InferOwnerResult::ModuleCode(r) => &r.diagnostics,
        }
    }

    pub fn call_arg_bindings(&self) -> &[CallArgBinding] {
        match self {
            InferOwnerResult::Method(r) => &r.call_arg_bindings,
            InferOwnerResult::ModuleCode(r) => &r.call_arg_bindings,
        }
    }

    pub fn uncompiled_stmts(&self) -> &[StmtId] {
        match self {
            InferOwnerResult::Method(r) => &r.uncompiled_stmts,
            InferOwnerResult::ModuleCode(r) => &r.uncompiled_stmts,
        }
    }
}

pub fn infer_owner(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
) -> InferOwnerResult {
    match owner {
        DefWithBodyId::Method(local_id) => {
            let module_id = hir_def::ModuleId { file_id };
            let method_id = hir_def::MethodId { module: module_id, local_id };
            let method_input = MethodIdInput::new(db, method_id);
            InferOwnerResult::Method(db.infer_method(method_input))
        }
        DefWithBodyId::ModuleCode => InferOwnerResult::ModuleCode(db.infer_module_code(file_id)),
    }
}

impl<'db> InferenceContext<'db> {
    pub fn new(
        db: &'db dyn HirDatabase,
        file_id: FileId,
        owner: DefWithBodyId,
        body: &Arc<Body>,
    ) -> Self {
        Self::new_impl(db, file_id, owner, body, true)
    }

    fn new_impl(
        db: &'db dyn HirDatabase,
        file_id: FileId,
        owner: DefWithBodyId,
        body: &Arc<Body>,
        with_body_env: bool,
    ) -> Self {
        let opts = db.env_options();
        fn implicit_assignment_name(body: &Body, target: ExprId) -> Option<NormName> {
            match body.expr(target) {
                Expr::Path(name) => Some(NormName::intern(name.as_str())),
                Expr::Index { base, .. } => implicit_assignment_name(body, ExprId::from_idx(*base)),
                _ => None,
            }
        }
        let assignment_target_names = body
            .stmts_iter()
            .filter_map(|(_, stmt)| {
                let Stmt::Assign { target, .. } = stmt else { return None };
                implicit_assignment_name(body, ExprId::from_idx(*target))
            })
            .collect();
        let body_env = if !with_body_env {
            hir_def::execution_env::EnvFlags::EMPTY
        } else {
            match owner {
                DefWithBodyId::Method(local_id) => crate::method_environment::effective_method_env(
                    db,
                    hir_def::MethodId { module: hir_def::ModuleId { file_id }, local_id },
                    &opts,
                ),
                DefWithBodyId::ModuleCode => {
                    let metadata = db.module_metadata(hir_def::ModuleId { file_id });
                    hir_def::execution_env::module_code_env(&metadata, &opts)
                }
            }
        };
        Self {
            db,
            context_file_id: file_id,
            local_symbols: None,
            local_effective_returns: None,
            weaving_base: None,
            proceed_return: None,
            proceed_arity: None,
            owner,
            body: Arc::clone(body),
            var_types: FxHashMap::default(),
            implicit_locals: FxHashMap::default(),
            assigned_var_names: rustc_hash::FxHashSet::default(),
            assignment_target_names,
            shadowed_root_names: FxHashMap::default(),
            binding_types: FxHashMap::default(),
            expr_types: FxHashMap::default(),
            diagnostics: Vec::new(),
            assignment_targets: rustc_hash::FxHashSet::default(),
            bare_name_verdicts: FxHashMap::default(),
            calls_with_unread_target: rustc_hash::FxHashSet::default(),
            call_arg_bindings: Vec::new(),
            return_expr_ids: Vec::new(),
            body_env,
            checked_env: opts.checked_environments,
            in_uncompiled_branch: false,
            uncompiled_stmts: Vec::new(),
            min_platform: db
                .min_platform_version()
                .and_then(|value| bsl_platform::PlatformVersion::parse_release(&value)),
            compat_mode: crate::compat_mode::effective_mode(db.compatibility_mode().as_deref()),
            global_exports: None,
            global_read_exports: None,
            global_surface_partly_unknown: false,
            global_surface_incomplete: false,
            structure_shapes: FxHashMap::default(),
            callee_module_env: FxHashMap::default(),
            local_callee_env: FxHashMap::default(),
        }
    }

    pub fn new_for_method(
        db: &'db dyn HirDatabase,
        method: MethodIdInput<'db>,
        body: &Arc<Body>,
    ) -> Self {
        let mid = method.method_id(db);
        Self::new(db, mid.module.file_id, DefWithBodyId::Method(mid.local_id), body)
    }

    /// Inference over an effective `&ИзменениеИКонтроль` module: local bodies/symbols come
    /// from the spliced effective text (`local_symbols`), while configuration / metadata /
    /// cross-module resolution keeps `context_file_id = base_file`. Identical to [`Self::new`]
    /// apart from same-module method/variable lookups, which route to `local_symbols`.
    pub fn new_effective(
        db: &'db dyn HirDatabase,
        base_file_id: FileId,
        local_symbols: Arc<ModuleInterface>,
        owner: DefWithBodyId,
        body: &Arc<Body>,
    ) -> Self {
        // The effective module is assembled from two files, and a directive
        // computed against the base file's item tree may belong to a woven
        // method rather than the one being inferred — disable availability
        // checks rather than misattribute them.
        let mut ctx = Self::new_impl(db, base_file_id, owner, body, false);
        ctx.local_symbols = Some(local_symbols);
        ctx
    }

    /// Inference over an extension module's OWN bodies under *weaving* (`&Вместо` /
    /// `&Перед` / `&После`): `context_file_id` stays the extension file (configuration /
    /// metadata resolution must key on the ext file), while bare same-module lookups that
    /// miss in the extension's own symbols fall back to `base_module`. Identical to
    /// [`Self::new`] apart from that fallback, threaded through [`Self::get_resolver`].
    pub fn new_weaving(
        db: &'db dyn HirDatabase,
        ext_file_id: FileId,
        base_module: hir_def::ModuleId,
        owner: DefWithBodyId,
        body: &Arc<Body>,
    ) -> Self {
        // A weaving interceptor's effective directive comes from the base
        // method it intercepts, unknown at this layer — no availability checks.
        let mut ctx = Self::new_impl(db, ext_file_id, owner, body, false);
        ctx.weaving_base = Some(base_module);
        ctx
    }

    /// Second-pass effective inference: supply the effective return types of the module's
    /// own methods so bare same-module calls to changed methods resolve their *changed*
    /// body's return. See [`Self::local_effective_returns`].
    pub fn set_local_effective_returns(
        &mut self,
        returns: Arc<FxHashMap<hir_def::MethodKey, TypeId>>,
    ) {
        self.local_effective_returns = Some(returns);
    }

    /// Weaving inference of a `&Вместо` interceptor: supply the base method's return type so a
    /// bare `ПродолжитьВызов(...)` in this body types as the original method's result rather
    /// than the platform global's generic return. See [`Self::proceed_return`].
    pub fn set_proceed_return(&mut self, ty: TypeId) {
        self.proceed_return = Some(ty);
    }

    /// Weaving inference of a `&Вместо` interceptor: supply the base method's arity so a bare
    /// `ПродолжитьВызов(...)` in this body is validated as a call to that method (it re-enters
    /// it). See [`Self::proceed_arity`].
    pub fn set_proceed_arity(&mut self, required_count: usize, total_count: usize) {
        self.proceed_arity = Some((required_count, total_count));
    }

    /// The return type a call site sees once a documented cross-reference has been followed.
    ///
    /// Whether the slot names a target at all is NOT asked here — that was recorded when the
    /// documentation was parsed. Asking it again in terms of types would reintroduce the guesswork
    /// the recorded bit exists to avoid, and would get it wrong: a slot documented
    /// `Массив из см. X.Y` lowers to an array of the permissive type, not to it. The one test on
    /// `resolved` below is a different question — whether replacing it would take something away.
    ///
    /// Applied UNDER [`Self::effective_local_return`], so an extension that supplies its own
    /// return still wins: effective and weaving inference are out of scope here.
    fn documented_return(&self, method_id: hir_def::MethodId, resolved: TypeId) -> TypeId {
        // Never over keys a BODY proved. Where the two doc-parsers disagree — a one-line
        // `Возвращаемое значение: см. Цель.Метод` is such a place — the signature falls back to
        // what the body proves, and replacing that would take keys away from the caller.
        //
        // The test is on the origin of those keys, not on their presence: a slot documented
        // `Структура:` with a field `* Поле - см. X.Y` also arrives here carrying fields, and it is
        // exactly the slot the resolved answer improves.
        if crate::lower::doc_structure::has_body_proven_structure(self.db, resolved) {
            return resolved;
        }
        crate::doc_see::doc_see_signature_query(
            self.db,
            hir_def::MethodIdInput::new(self.db, method_id),
        )
        .ret
        .unwrap_or(resolved)
    }

    /// In effective inference, prefer a LOCAL method's CHANGED-body return over the value
    /// materialised by the base-keyed `resolve_*` paths (which re-derive the base body).
    /// A no-op for cross-module resolutions (the file guard fails) and for ordinary modules
    /// (`local_effective_returns == None`), so it is safe to apply at every local-capable
    /// `MethodResolution` return site.
    fn effective_local_return(&self, method_id: hir_def::MethodId, resolved: TypeId) -> TypeId {
        if method_id.module.file_id == self.context_file_id {
            if let Some(ret) = self
                .local_effective_returns
                .as_ref()
                .and_then(|m| m.get(&method_id.local_id).copied())
            {
                if !self.is_unknown(ret) {
                    return ret;
                }
            }
        }
        resolved
    }

    /// Lazily build both usage projections of the complete user global surface.
    fn global_export_map(&mut self) -> Arc<FxHashMap<NormName, GlobalCallableExport>> {
        if let Some(map) = &self.global_exports {
            return Arc::clone(map);
        }
        let resolver = self.get_resolver();
        let common = resolver.global_common_module_exports(self.db);
        let application = resolver.application_module_exports(self.db);
        self.global_surface_incomplete = !common.is_complete() || !application.is_complete();
        self.global_surface_partly_unknown =
            common.gaps.iter().chain(application.gaps.iter()).any(|gap| {
                matches!(
                    gap,
                    hir_def::resolver::GlobalSurfaceGap::UnreadBody { .. }
                        | hir_def::resolver::GlobalSurfaceGap::UnreadApplicationModule { .. }
                )
            });

        let mut callables: FxHashMap<NormName, GlobalCallableExport> = FxHashMap::default();
        let mut readable: FxHashMap<NormName, GlobalReadableExport> = FxHashMap::default();
        for entry in common.entries.into_iter().chain(application.entries) {
            let key = NormName::intern(entry.name.as_str());
            if entry.capabilities.callable == Some(true) {
                if let hir_def::resolver::GlobalExportDefinition::Method(method) = entry.definition
                {
                    let variant = GlobalCallableVariant { method, host: entry.host };
                    match callables.entry(key) {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(GlobalCallableExport { variants: vec![variant] });
                        }
                        std::collections::hash_map::Entry::Occupied(mut slot)
                            if slot.get().variants.iter().all(|v| {
                                matches!(
                                    v.host,
                                    hir_def::resolver::GlobalExportHost::ApplicationModule(_)
                                )
                            }) && matches!(
                                entry.host,
                                hir_def::resolver::GlobalExportHost::ApplicationModule(_)
                            ) =>
                        {
                            slot.get_mut().variants.push(variant);
                        }
                        std::collections::hash_map::Entry::Occupied(_) => {}
                    }
                }
            }
            if entry.capabilities.readable_as_value == Some(true) {
                let availability = match entry.host {
                    hir_def::resolver::GlobalExportHost::CommonModule => {
                        hir_def::execution_env::EnvFlags::ALL
                    }
                    hir_def::resolver::GlobalExportHost::ApplicationModule(kind) => {
                        self.application_export_env(kind)
                    }
                };
                readable
                    .entry(key)
                    .and_modify(|export| {
                        export.availability = export.availability.union(availability)
                    })
                    .or_insert(GlobalReadableExport { availability });
            }
        }
        let callables = Arc::new(callables);
        self.global_exports = Some(Arc::clone(&callables));
        self.global_read_exports = Some(Arc::new(readable));
        callables
    }

    fn global_read_export(&mut self, name: &Name) -> Option<GlobalReadableExport> {
        self.global_export_map();
        self.global_read_exports.as_ref()?.get(&NormName::intern(name.as_str())).copied()
    }

    fn application_export_env(
        &self,
        kind: hir_def::ApplicationModuleKind,
    ) -> hir_def::execution_env::EnvFlags {
        let metadata = hir_def::ModuleMetadata::unknown(kind.module_type());
        hir_def::execution_env::module_base_env(&metadata, &self.db.env_options())
    }

    /// Whether `name` is a method of the current module (or, under weaving, its paired
    /// base) — a same-module method takes precedence over the global context, so the
    /// bare global-export path must defer to it. Mirrors the same-module lookup the
    /// `TypeKind::Unknown` call arm performs.
    fn bare_module_method_exists(&self, name: &Name) -> bool {
        let declared = match &self.local_symbols {
            Some(symbols) => symbols.find_method(name).is_some(),
            None => {
                let module_id = hir_def::ModuleId::new(self.context_file_id);
                self.db.interface_method_named(module_id, name).is_some()
            }
        };
        if declared {
            return true;
        }
        self.weaving_base.is_some_and(|base| self.db.interface_method_named(base, name).is_some())
    }

    /// Resolve a bare `Имя(...)` call against the exported methods of the visible global
    /// common modules. Returns the call's result type when `name` is such an export,
    /// running the full call contract — argument-count validation and parameter bindings —
    /// exactly as a qualified `Модуль.Имя(...)` call would, but without the qualified-only
    /// `RedundantAccessToObjectTwoLevel` lint (there is no redundant receiver to flag).
    fn resolve_bare_global_export(
        &mut self,
        name: &Name,
        args: &[ExprId],
        callee: ExprId,
    ) -> Option<TypeId> {
        let export = self.global_export_map().get(&NormName::intern(name.as_str())).cloned()?;
        let primary = *export.variants.first()?;

        match primary.host {
            hir_def::resolver::GlobalExportHost::CommonModule => {
                self.check_common_module_callee_env(callee, None, primary.method.module);
            }
            hir_def::resolver::GlobalExportHost::ApplicationModule(_) => {
                let availability = export.variants.iter().fold(
                    hir_def::execution_env::EnvFlags::EMPTY,
                    |env, variant| match variant.host {
                        hir_def::resolver::GlobalExportHost::ApplicationModule(kind) => {
                            env | self.application_export_env(kind)
                        }
                        hir_def::resolver::GlobalExportHost::CommonModule => env,
                    },
                );
                self.check_member_env(callee, name, availability, EnvMemberKind::Method);
            }
        }

        for arg in args {
            self.infer_expr(*arg);
        }

        let signatures = export
            .variants
            .iter()
            .filter_map(|variant| {
                let method_symbol = self.db.interface_method(variant.method)?;
                let signature = crate::method_resolution::materialise_signature_enriched(
                    self.db,
                    variant.method,
                    &method_symbol,
                );
                Some((variant.method, signature))
            })
            .collect::<Vec<_>>();
        let (method_id, signature) = signatures.first()?;
        self.expr_types.insert(callee, self.db.unknown());

        // Two branches, and both must be served. The multi-variant one returns before any call
        // candidate is built, so a substitution made only where `primary_return` is passed would
        // leave every bare name with more than one global export on tier 1.
        if signatures.len() > 1 {
            let returns = signatures
                .iter()
                .map(|(method, signature)| {
                    self.effective_local_return(
                        *method,
                        self.documented_return(*method, signature.ret),
                    )
                })
                .collect();
            return Some(self.db.union(returns));
        }

        let return_ty = self
            .effective_local_return(*method_id, self.documented_return(*method_id, signature.ret));
        let candidates =
            crate::user_call_candidates::for_resolved_method(self.db, *method_id, return_ty)
                .ok()?;
        Some(self.record_candidate_call_arg_binding(callee, args, candidates))
    }

    fn push_inference_diagnostic(&mut self, diag: InferenceDiagnostic) {
        let key = match &diag {
            InferenceDiagnostic::UnresolvedName { expr, .. } => *expr,
            InferenceDiagnostic::UnresolvedMethodCall { expr, .. } => *expr,
            InferenceDiagnostic::UnresolvedBareCall { expr, .. } => *expr,
            InferenceDiagnostic::MismatchedArgCount { call_expr, .. } => *call_expr,
            InferenceDiagnostic::TypeMismatch { expr, .. } => *expr,
            InferenceDiagnostic::UnresolvedField { expr, .. } => *expr,
            InferenceDiagnostic::ReadOnlyPropertyAssignment { lhs, .. } => *lhs,
            InferenceDiagnostic::GlobalPropertyNotWritable { lhs, .. } => *lhs,
            InferenceDiagnostic::DeprecatedPlatformMember { expr, .. } => *expr,
            InferenceDiagnostic::RedundantAccessToObjectTwoLevel { expr, .. } => *expr,
            InferenceDiagnostic::MissedRequiredParameterCommonModule { expr, .. } => *expr,
            InferenceDiagnostic::MissedRequiredParameterManagerModule { expr, .. } => *expr,
            InferenceDiagnostic::RedundantAccessToObjectThreeLevel { expr, .. } => *expr,
            InferenceDiagnostic::UnavailableInEnvironment { expr, .. } => *expr,
            InferenceDiagnostic::PlatformMemberNewerThanMinVersion { expr, .. } => *expr,
            InferenceDiagnostic::PlatformMemberHiddenByCompatibilityMode { expr, .. } => *expr,
            InferenceDiagnostic::ModuleAccessibility { expr, .. } => *expr,
            InferenceDiagnostic::GuardedCall { expr, .. } => *expr,
        };
        if self.body.is_recovered(key) {
            return;
        }
        // A call whose target body could not be read gets no "unresolved" verdict from
        // ANY later attempt: resolution continues past the unread module (the platform
        // surface of a manager or object does not live in the user module, and losing
        // it would cost the receiver's type), but nothing it fails to find afterwards
        // is evidence about the call.
        if matches!(diag, InferenceDiagnostic::UnresolvedMethodCall { .. })
            && self.calls_with_unread_target.contains(&key)
        {
            return;
        }
        self.diagnostics.push(diag);
    }

    fn is_unknown(&self, id: TypeId) -> bool {
        id == self.db.unknown()
    }

    fn is_undefined(&self, id: TypeId) -> bool {
        id == self.db.undefined()
    }

    fn exact_platform_owner(&self, ty: TypeId) -> Option<Name> {
        let TypeKind::PlatformObject(facet) = self.db.lookup_type(ty) else {
            return None;
        };
        Some(Name::new(facet.name.as_str()))
    }

    /// Report a resolved platform member that is missing from some of the
    /// environments this body runs in. Free when nothing is wrong: one u8
    /// mask compare against availability already carried by the lookup
    /// result. `#Если` branches narrow the body mask per their condition
    /// (see the `Stmt::PreprocIf` arm); the check is skipped when either
    /// side is unknown.
    fn check_member_env(
        &mut self,
        expr: ExprId,
        name: &Name,
        member_env: hir_def::execution_env::EnvFlags,
        member_kind: EnvMemberKind,
    ) {
        if self.body_env.is_empty() || member_env.is_empty() {
            return;
        }
        let missing = self.body_env.without(member_env) & self.checked_env;
        if missing.is_empty() {
            return;
        }
        self.push_inference_diagnostic(InferenceDiagnostic::UnavailableInEnvironment {
            expr,
            name: name.clone(),
            member_kind,
            missing,
        });
    }

    /// The release this module must still compile on: the project's
    /// `min_platform_version`. The compatibility mode does not lower it — a mode
    /// hides 30 named globals at measured thresholds, not everything newer than
    /// the mode — and is judged by its own check, [`Self::check_compat_mode`].
    fn effective_min_platform(&self) -> Option<bsl_platform::PlatformVersion> {
        self.min_platform
    }

    /// Report a resolved platform member that `introduced` dates after the
    /// effective minimum release. Silent when either version is unknown and where
    /// nothing compiles the code (an uncompiled `#Если` branch, a module compiled
    /// nowhere) — the same places a missing name cannot fail the module.
    fn check_min_platform(
        &mut self,
        expr: ExprId,
        name: &Name,
        introduced: impl FnOnce() -> Option<bsl_platform::PlatformVersion>,
    ) {
        let Some(minimum) = self.effective_min_platform() else {
            return;
        };
        let Some(introduced) = introduced() else {
            return;
        };
        if !introduced.release_newer_than(minimum) {
            return;
        }
        if self.in_uncompiled_branch || self.module_compiles_nowhere() {
            return;
        }
        self.push_inference_diagnostic(InferenceDiagnostic::PlatformMemberNewerThanMinVersion {
            expr,
            name: name.clone(),
            introduced,
            minimum,
        });
    }

    /// Report a platform name the compatibility mode hides: `visible_from` is the
    /// first mode that sees it ([`crate::compat_mode`]). Silent without a known mode
    /// and where nothing compiles the code, like [`Self::check_min_platform`].
    fn check_compat_mode(
        &mut self,
        expr: ExprId,
        name: &Name,
        visible_from: impl FnOnce() -> Option<bsl_platform::PlatformVersion>,
    ) {
        let Some(mode) = self.compat_mode else {
            return;
        };
        let Some(visible_from) = visible_from() else {
            return;
        };
        if !visible_from.release_newer_than(mode) {
            return;
        }
        if self.in_uncompiled_branch || self.module_compiles_nowhere() {
            return;
        }
        self.push_inference_diagnostic(
            InferenceDiagnostic::PlatformMemberHiddenByCompatibilityMode {
                expr,
                name: name.clone(),
                mode,
                visible_from,
            },
        );
    }

    /// [`Self::check_compat_mode`] for a property of a value typed as exactly one
    /// platform type.
    fn check_compat_mode_type_member(&mut self, expr: ExprId, receiver_ty: TypeId, member: &Name) {
        if self.compat_mode.is_none() {
            return;
        }
        // `Новый Запрос` is typed as the query kind, not as a plain platform object.
        let owner = match self.db.lookup_type(receiver_ty) {
            TypeKind::Query { .. } => Some(Name::new("Запрос")),
            _ => self.exact_platform_owner(receiver_ty),
        };
        let Some(owner) = owner else {
            return;
        };
        self.check_compat_mode(expr, member, || {
            crate::compat_mode::hidden_type_member(owner.as_str(), member.as_str())
        });
    }

    /// [`Self::check_min_platform`] for a member of `receiver_ty`, judged only when
    /// the receiver is exactly one platform type.
    fn check_min_platform_type_member(
        &mut self,
        expr: ExprId,
        receiver_ty: TypeId,
        member: &Name,
        is_property: bool,
    ) {
        if self.min_platform.is_none() {
            return;
        }
        let Some(owner) = self.exact_platform_owner(receiver_ty) else {
            return;
        };
        self.check_min_platform(expr, member, || {
            crate::min_platform::type_member(owner.as_str(), member.as_str(), is_property)
        });
    }

    /// True when a bare manager-collection name is shadowed by a user symbol
    /// and thus does not denote the platform global: a body binding (`Перем`,
    /// parameter, loop variable), a module-level variable or method, a form
    /// attribute or form-self property, an implicit `ЭтотОбъект`/record-set
    /// member, or a workspace common module.
    ///
    /// A plain assignment does NOT shadow. `Справочники = Новый Структура` does
    /// not declare a local: the name belongs to a Global-context property, and
    /// the platform refuses the write rather than creating a variable, so the
    /// name keeps denoting the collection throughout the body. Only a DECLARED
    /// owner takes it. Shadowing is preprocessor-blind, like every other
    /// shadowing decision in inference.
    fn manager_collection_shadowed(&mut self, name: &Name) -> bool {
        let key = NormName::intern(name.as_str());
        if self.body_declares_binding(name) {
            return true;
        }
        if let Some(cached) = self.shadowed_root_names.get(&key) {
            return *cached;
        }
        let resolver = self.get_resolver();
        let claimed =
            crate::platform_global_lookup::bare_global_name_claim(self.db, &resolver, None, name)
                .is_some();
        self.shadowed_root_names.insert(key, claimed);
        claimed
    }

    /// Environment check for a bare manager-collection root (`Справочники`,
    /// `Перечисления`, …). The mask compare runs first, so on the available
    /// path — every server-side body — no shadowing lookups happen at all;
    /// the resolver is consulted only when a verdict is about to be issued.
    fn check_manager_collection_env(
        &mut self,
        expr: ExprId,
        name: &Name,
        mdo_type: bsl_metadata::MdoType,
    ) {
        let member_env = crate::platform_global_lookup::manager_collection_env(mdo_type);
        if self.body_env.is_empty() || member_env.is_empty() {
            return;
        }
        if (self.body_env.without(member_env) & self.checked_env).is_empty() {
            return;
        }
        if self.manager_collection_shadowed(name) {
            return;
        }
        self.check_member_env(expr, name, member_env, EnvMemberKind::GlobalProperty);
    }

    /// The type name a `Новый(...)` names by value, trimmed once here so every
    /// consumer below sees the same spelling. A `Тип(...)` wrapper counts only
    /// when it really is the platform function: a nearer declaration takes the
    /// name over, in the Local → Module → Global-CM → Platform order the call
    /// path itself applies.
    fn constructed_type_name<'b>(&mut self, body: &'b Body, arg: ExprIdx) -> Option<&'b str> {
        if let Some(text) = crate::type_literal::bare_string_literal(body, arg) {
            return Some(text.trim());
        }
        let (callee, text) = crate::type_literal::type_ctor_literal(body, arg)?;
        if self.is_call_name_shadowed(callee) {
            return None;
        }
        Some(text.trim())
    }

    /// The one member of a platform method's return union that its `Тип`
    /// argument names. Collections that create items of several kinds
    /// (`Элементы.Добавить(Имя, Тип("ГруппаФормы"), Родитель)`,
    /// `Отбор.Элементы.Добавить(Тип("ЭлементОтбораКомпоновкиДанных"))`, …)
    /// document the result as the union of every kind they can create, while
    /// the call creates exactly the kind it was asked for. Keeping the union
    /// makes the result unusable as an argument whose slot admits only some of
    /// the kinds — a group created to be a `Родитель` is rejected because a
    /// field could not be one.
    ///
    /// Only a literal `Тип("X")` decides, and only when X is one of the
    /// documented kinds: the platform rejects any other type at runtime, so a
    /// name outside the union is a different defect, not a reason to trust it.
    /// Only platform signatures reach this: a user method is bound before the
    /// platform lookup, and its documented `Тип` parameter would promise
    /// nothing about the result anyway.
    fn return_selected_by_type_argument(
        &mut self,
        body: &Body,
        signature: &crate::call_resolution::CallSignature,
        args: &[ExprIdx],
    ) -> Option<TypeId> {
        let TypeKind::Union(members) = self.db.lookup_type(signature.return_ty) else {
            return None;
        };
        let type_param = self.db.type_descriptor();
        let position = signature.params.iter().position(|param| param.ty == type_param)?;
        let named = self.literal_type_argument(body, *args.get(position)?)?;
        members
            .iter()
            .any(|member| crate::subtype::is_assignable(self.db, named, *member))
            .then_some(named)
    }

    /// [`Self::return_selected_by_type_argument`] applied to every signature,
    /// so overload selection and the call's recorded binding agree on it.
    fn select_returns_by_type_argument(
        &mut self,
        candidates: &mut crate::call_resolution::CallCandidateSet,
        args: &[ExprIdx],
    ) {
        let body = Arc::clone(&self.body);
        for signature in candidates.signatures_mut() {
            if let Some(selected) = self.return_selected_by_type_argument(&body, signature, args) {
                signature.return_ty = selected;
            }
        }
    }

    /// The type a literal `Тип("X")` argument names, when `Тип` is the platform
    /// function and X lowers to a known type.
    fn literal_type_argument(&mut self, body: &Body, arg: ExprIdx) -> Option<TypeId> {
        let (callee, text) = crate::type_literal::type_ctor_literal(body, arg)?;
        if self.is_call_name_shadowed(callee) {
            return None;
        }
        let named = crate::lower::type_string::lower_constructed_type_name_typeid(self.db, text);
        (named != self.db.unknown()).then_some(named)
    }

    /// The result of the managed form's own `РеквизитФормыВЗначение(Имя[, Тип])`:
    /// the platform documents it as `Произвольный`, while the value is fixed by the
    /// call — by a literal `Тип` argument when one is given, otherwise by the
    /// declared type of the attribute the literal name points at.
    ///
    /// A second argument that is not a recognisable literal decides the type at
    /// run time, so it leaves the result unknown rather than falling back to the
    /// attribute: the two may legitimately differ.
    fn form_attribute_to_value_return(
        &mut self,
        name: &hir_def::Name,
        args: &[ExprId],
    ) -> Option<TypeId> {
        let method = bsl_platform::PlatformData::instance()
            .get_method(crate::form_self::FORM_TYPE_NAME, name.as_str())?;
        if method.english_name != "FormAttributeToValue" {
            return None;
        }
        let resolver = self.get_resolver();
        if !crate::this_object::is_managed_form_module(self.db, &resolver) {
            return None;
        }
        let body = Arc::clone(&self.body);
        if let Some(type_arg) = args.get(1) {
            if !matches!(body.expr(*type_arg), Expr::Missing) {
                return self.literal_type_argument(&body, type_arg.to_idx());
            }
        }
        let attribute = crate::type_literal::bare_string_literal(&body, args.first()?.to_idx())?;
        crate::form_attr::resolve_form_attribute_value(self.db, &resolver, attribute)
    }

    fn is_call_name_shadowed(&mut self, name: &Name) -> bool {
        // Variables are NOT asked: BSL keeps names of variables and of methods
        // apart, so `Имя(...)` is looked up among methods even where a local
        // `Имя` exists. The BSP idiom `КаталогПрограммы = КаталогПрограммы();`
        // is exactly that, 17 times over on a real configuration — treating the
        // local as an owner would silence every one of them.
        // A module-level `Перем` is a variable too and is not asked either: a module
        // `Перем СтрДлина` leaves `СтрДлина("abcd")` calling the platform function
        // (checked on 8.3.17 and 8.3.27).
        let key = NormName::intern(name.as_str());
        // Not through `resolve_name`: it answers `Builtin` first, and every name
        // worth asking about here — `Тип` above all — is precisely a builtin, so
        // the module would never be consulted.
        let resolver = self.get_resolver();
        if resolver.resolve_module_method(self.db, name).is_some() {
            return true;
        }
        self.global_export_map().contains_key(&key)
    }

    /// The same type-availability check as the syntactic `Новый X`, for a name
    /// that arrived as a string value (`Новый("X")`, `Новый(Тип("X"))`). A
    /// qualified name denotes a configuration object rather than a platform
    /// type, and configuration objects carry no per-environment availability.
    fn check_constructed_type_env(&mut self, expr: ExprId, name: &str) {
        if name.contains('.') {
            return;
        }
        let platform = bsl_platform::PlatformDataInner::instance();
        let Some(platform_type) = platform.get_type(name) else {
            return;
        };
        if platform.is_ambiguous_type_name(name) {
            return;
        }
        let environment =
            hir_def::execution_env::EnvFlags::from_platform_context(platform_type.context.as_ref());
        let type_name = Name::new(name);
        self.check_member_env(expr, &type_name, environment, EnvMemberKind::Type);
        self.check_min_platform(expr, &type_name, || crate::min_platform::constructed_type(name));
    }

    /// Accessibility of a cross-module call to `callee_module` (a common
    /// module): every environment this body runs in must either be one the
    /// callee is compiled for, or — for `ВызовСервера` modules — a client
    /// environment making a remote server call.
    /// `module_name` is the name as written at the call site; `None` (a bare
    /// call to a global module's export) falls back to the module's metadata
    /// name.
    fn check_common_module_callee_env(
        &mut self,
        expr: ExprId,
        module_name: Option<&Name>,
        callee_module: hir_def::ModuleId,
    ) {
        use hir_def::execution_env::{self, EnvFlags};
        if self.body_env.is_empty() || callee_module.file_id == self.context_file_id {
            return;
        }
        // Physical flags per callee module are cached per body: a body calling
        // the same module many times must not record a salsa dependency per
        // call.
        let cached = match self.callee_module_env.get(&callee_module.file_id) {
            Some(v) => *v,
            None => {
                let opts = self.db.env_options();
                let metadata = self.db.module_metadata(callee_module);
                let v = metadata
                    .common_module
                    .as_ref()
                    .map(|cm| (execution_env::common_module_env(cm, &opts), cm.is_server_call()));
                self.callee_module_env.insert(callee_module.file_id, v);
                v
            }
        };
        let Some((physical_env, physical_server_call)) = cached else { return };

        let missing_with = |env: EnvFlags, server_call: bool| {
            if env.is_empty() {
                return EnvFlags::EMPTY;
            }
            let mut missing = self.body_env.without(env) & self.checked_env;
            if server_call {
                missing = missing.without(EnvFlags::ALL_CLIENTS);
            }
            missing
        };

        // Fast path on the resolved body's physical flags. An extension that
        // NARROWS an adopted module's flags slips through here as a missed
        // diagnostic — accepted: adoption in practice widens access
        // (`ВызовСервера`, client contexts), and the trade keeps the
        // configuration-scoped lookup off the hot path entirely.
        if missing_with(physical_env, physical_server_call).is_empty() {
            return;
        }

        let metadata = self.db.module_metadata(callee_module);
        let Some(physical) = metadata.common_module.as_ref() else { return };
        let name = match module_name {
            Some(name) => name.clone(),
            None => Name::new(bsl_metadata::MdObject::name(physical.as_ref())),
        };
        // The caller's own extension may adopt the module and replace its
        // flags wholesale (enable `ВызовСервера` or client contexts), so
        // before reporting, re-judge against the flags visible TO THE CALLER.
        // The physical flags stand when the provider has no configuration
        // scoping.
        let opts = self.db.env_options();
        let caller_scoped = self.db.resolve_common_module(self.context_file_id, name.as_str());
        let cm = caller_scoped.as_deref().unwrap_or(physical);
        let missing =
            missing_with(execution_env::common_module_env(cm, &opts), cm.is_server_call());
        if missing.is_empty() {
            return;
        }
        self.push_inference_diagnostic(InferenceDiagnostic::ModuleAccessibility {
            expr,
            name,
            callee_kind: EnvCalleeKind::CommonModule,
            missing,
        });
    }

    /// Accessibility of a same-module call across compilation directives.
    /// A client-side caller invoking a server method is the form's regular
    /// remote server call, so only the server side is ever a violation: code
    /// compiled for the server cannot reach a method that exists only on the
    /// client.
    fn check_local_callee_env(
        &mut self,
        expr: ExprId,
        name: &Name,
        callee_local_id: hir_def::MethodKey,
    ) {
        use hir_def::execution_env::EnvFlags;
        if self.body_env.is_empty() {
            return;
        }
        let callee_env = match self.local_callee_env.get(&callee_local_id) {
            Some(&env) => env,
            None => {
                let opts = self.db.env_options();
                let env = crate::method_environment::effective_method_env(
                    self.db,
                    hir_def::MethodId {
                        module: hir_def::ModuleId { file_id: self.context_file_id },
                        local_id: callee_local_id,
                    },
                    &opts,
                );
                self.local_callee_env.insert(callee_local_id, env);
                env
            }
        };
        if callee_env.is_empty() {
            return;
        }
        let missing = self.body_env.without(callee_env) & self.checked_env & EnvFlags::SERVER_SIDE;
        if missing.is_empty() {
            return;
        }
        self.push_inference_diagnostic(InferenceDiagnostic::ModuleAccessibility {
            expr,
            name: name.clone(),
            callee_kind: EnvCalleeKind::LocalMethod,
            missing,
        });
    }

    fn push_deprecated_platform_member_diagnostic(
        &mut self,
        expr: ExprId,
        receiver_ty: TypeId,
        member_name: &Name,
        is_property: bool,
    ) {
        let Some(type_name) = self.exact_platform_owner(receiver_ty) else {
            return;
        };

        let owner_key = type_name.as_str().fold_lower();
        if !DEPRECATED_PLATFORM_MEMBER_OWNERS.contains(owner_key.as_str()) {
            return;
        }

        let lookup = if is_property {
            Lookup::property(type_name.as_str(), member_name.as_str())
        } else {
            Lookup::method(type_name.as_str(), member_name.as_str())
        };
        if deprecation::registry().lookup(lookup).is_none() {
            return;
        }

        self.push_inference_diagnostic(InferenceDiagnostic::DeprecatedPlatformMember {
            expr,
            type_name,
            member_name: member_name.clone(),
            is_property,
        });
    }

    pub fn finish(self) -> BodyInferenceResult {
        BodyInferenceResult {
            owner: self.owner,
            // `var_types` is `NormName`-keyed internally (the hot lookup path during
            // inference); the public result stays `String`-keyed for its many
            // consumers across crate boundaries, so materialise once here — a
            // single allocation per body, replacing a fold per lookup.
            var_types: self.var_types.iter().map(|(k, v)| (k.as_str().to_string(), *v)).collect(),
            implicit_locals: self.implicit_locals,
            binding_types: self.binding_types,
            expr_types: self.expr_types,
            diagnostics: self.diagnostics,
            call_arg_bindings: self.call_arg_bindings,
            uncompiled_stmts: self.uncompiled_stmts,
            return_expr_ids: self.return_expr_ids,
        }
    }

    fn record_candidate_call_arg_binding(
        &mut self,
        call_expr: ExprId,
        args: &[ExprId],
        candidates: crate::call_resolution::CallCandidateSet,
    ) -> TypeId {
        let argument_types = args
            .iter()
            .map(|arg| self.expr_types.get(arg).copied().unwrap_or_else(|| self.db.unknown()))
            .collect::<Vec<_>>();
        let projection = crate::call_binding::resolve_binding(self.db, candidates, &argument_types);
        let return_ty = projection.semantic.resolution.return_ty;
        if !self.body.is_recovered(call_expr) {
            self.call_arg_bindings.push(CallArgBinding {
                owner: self.owner,
                call_expr,
                args: args.to_vec(),
                candidate: projection.semantic,
            });
        }
        return_ty
    }

    /// Narrows the declared return of a first-argument-typed builtin (see
    /// [`builtin::returns_first_argument_type`]) to that argument's type. A first argument
    /// inside the comparable family narrows; a foreign one says nothing about which variant
    /// runs, so the declared union stays. An unknown one leaves the result unknown: the
    /// declared union would claim `Строка` for `Макс(Параметр, 1)` and accuse the next
    /// number-only consumer (`Формат(..., "ЧГ=0")`) on the strength of nothing.
    /// The rule is the platform function's: a module method or a global export that owns
    /// the name answers for its own result. A same-named variable does not take the name
    /// over (see [`Self::is_call_name_shadowed`]), so the narrowing stays there.
    fn refine_first_argument_return(
        &mut self,
        name: &hir_def::Name,
        args: &[ExprId],
        declared: TypeId,
    ) -> TypeId {
        if !builtin::returns_first_argument_type(name.as_str())
            || self.bare_module_method_exists(name)
            || self.is_call_name_shadowed(name)
        {
            return declared;
        }
        let Some(first) = args.first().and_then(|arg| self.expr_types.get(arg)).copied() else {
            return declared;
        };
        if self.is_unknown(first) {
            return first;
        }
        let comparable = |ty: TypeId| {
            matches!(
                self.db.lookup_type(ty),
                TypeKind::Number(_) | TypeKind::String(_) | TypeKind::Date(_) | TypeKind::Boolean
            )
        };
        let narrows = match self.db.lookup_type(first) {
            TypeKind::Union(arms) => arms.iter().all(|arm| comparable(*arm)),
            _ => comparable(first),
        };
        if narrows {
            first
        } else {
            declared
        }
    }

    fn get_resolver(&self) -> Resolver {
        let module_id = hir_def::ModuleId { file_id: self.context_file_id };
        match &self.local_symbols {
            Some(symbols) => {
                Resolver::with_builtins_and_workspace_effective(module_id, Arc::clone(symbols))
            }
            None => match self.weaving_base {
                Some(base) => Resolver::with_builtins_and_workspace_weaving(module_id, base),
                None => Resolver::with_builtins_and_workspace(module_id),
            },
        }
    }

    fn body_declares_binding(&self, name: &hir_def::Name) -> bool {
        let target = NormName::intern(name.as_str());
        self.body.bindings_iter().any(|(_, b)| NormName::intern(b.name.as_str()) == target)
    }

    /// Whether assignment syntax for `name` may introduce an implicit local.
    /// Mirrors the exclusions in assignment inference so reaching definitions
    /// cannot turn a rejected write to a module/global property into a local.
    fn assignment_can_declare_implicit_local(&mut self, name: &Name) -> bool {
        use hir_def::resolver::Resolution;

        let resolver = self.get_resolver();
        let resolved = resolver.resolve_name(self.db, name);
        if matches!(resolved, Some(Resolution::Variable(_))) {
            return false;
        }
        let declared_owner =
            matches!(resolved, Some(Resolution::Method(_) | Resolution::Variable(_)))
                || self.body_declares_binding(name);
        if !declared_owner
            && crate::form_self::resolve_form_self_property(self.db, &resolver, name).is_some()
        {
            return false;
        }
        let is_manager_collection = bsl_metadata::MdoType::from_plural(name.as_str())
            .is_some_and(|mdo| mdo.manager_type_prefix().is_some());
        !is_manager_collection || self.manager_collection_shadowed(name)
    }

    /// Whether an implicit assignment actually reaches this read. The assignment
    /// may carry `Unknown` and therefore be absent from `var_types`, but still
    /// declares the local on every path represented by reaching definitions.
    fn implicit_local_reaches(&mut self, use_expr: ExprId, name: &Name) -> bool {
        if !self.assignment_can_declare_implicit_local(name) {
            return false;
        }
        let key = name.as_str().fold_lower();
        if let Some(definitions) = self.reaching_assignment_types(use_expr, &key) {
            return !definitions.is_empty();
        }

        // Recovery/effective bodies may lack dataflow. Keep the sequential
        // fallback there without allowing an ordinary later assignment to
        // declare an earlier read.
        let Some(info) = self.implicit_locals.get(&key) else {
            return false;
        };
        let use_index = use_expr.into_raw().into_u32();
        info.assignments
            .iter()
            .any(|assignment| assignment.target.into_raw().into_u32() < use_index)
    }

    /// True when the receiver is a reassigned local variable and the definitions
    /// that actually reach this use leave the miss unprovable. Sequential
    /// inference records the textually-last assignment type, so at a use inside a
    /// sibling branch the receiver type may be a cross-branch artefact; reaching
    /// definitions restore the flow facts and keep the diagnostic alive when the
    /// stale type is the only one that reaches (e.g. straight-line reassignment).
    fn method_resolves_on_alternate_assignment(
        &self,
        receiver_expr: ExprId,
        receiver_ty: TypeId,
        method_name: &hir_def::Name,
    ) -> bool {
        let Expr::Path(name) = self.body.expr(receiver_expr) else {
            return false;
        };
        let key = name.as_str().fold_lower();
        let Some(info) = self.implicit_locals.get(&key) else {
            return false;
        };
        if info.assignments.len() < 2 {
            return false;
        }

        let resolves =
            |ty: TypeId| crate::method_lookup::lookup_method(self.db, ty, method_name).is_some();
        let is_wildcard = |ty: TypeId| matches!(self.db.lookup_type(ty), TypeKind::Any);

        // Without flow facts a non-reaching assignment must not vouch for the
        // call (it would hide a real error after a straight-line
        // reassignment), so no reaching definitions — no suppression.
        let Some(reaching) = self.reaching_assignment_types(receiver_expr, &key) else {
            return false;
        };

        // One definition that resolves the method, or that inference could not
        // type at all, is enough: it also puts the types recorded for the other
        // paths in doubt, so nothing here is provable.
        let vouches_alone = |ty: TypeId| ty != receiver_ty && (self.is_unknown(ty) || resolves(ty));
        if reaching.iter().any(|def| match def.ty {
            Some(ty) => vouches_alone(ty),
            // A reaching definition inference could not type (e.g. a loop
            // back-edge not seen yet) — "method not found" is unprovable.
            None => true,
        }) {
            return true;
        }

        // A declared wildcard — `Произвольный`, or a bare metadata-kind name that
        // documents a family rather than an object (`lower_bare_name_id` lowers a
        // documented `СправочникСсылка` with no catalog behind it to exactly this)
        // — carries no arms, so it cannot witness that a method is absent either.
        // Unlike `Unknown` it is a type inference did record, and it leaves the
        // other paths' types standing: where a definition reaching this very use
        // carries a type of its own that lacks the method, the miss is provable on
        // that path and stays reported.
        let mut wildcard_reaches = false;
        let every_path_unprovable = reaching.iter().all(|def| match def.ty {
            Some(ty) if is_wildcard(ty) => {
                wildcard_reaches = true;
                true
            }
            Some(ty) => resolves(ty),
            None => true,
        });
        every_path_unprovable && wildcard_reaches
    }

    /// True when the `Если` branch holding this call rules out every definition
    /// that reaches it.
    ///
    /// `Если Х = Неопределено … Иначе Х.Метод()` proves `Х` is not `Неопределено`
    /// in the `Иначе` branch. A value inference could only type through a lossy
    /// path — a query whose text is glued together at run time, say — comes back
    /// as a confident `Неопределено` rather than an honest unknown, and that is
    /// exactly the type the branch rules out. With every reaching definition
    /// ruled out, no type is established for the receiver here at all, so
    /// "method not found" is unprovable and reporting it is a false positive.
    ///
    /// Deliberately narrower than the guard itself: one surviving definition is
    /// enough to keep the diagnostic, and a definition made inside the guarded
    /// `Если` is left alone — the condition tested the value that flowed into the
    /// statement, not one assigned after it. A definition that establishes no
    /// type of its own — a BSL parameter, an element of an untyped collection —
    /// has nothing to survive with, so it does not hold the diagnostic open.
    fn branch_guard_rules_out_every_reaching_def(&self, receiver_expr: ExprId) -> bool {
        use crate::narrow::{apply_branch_guard, branch_guards_for_stmt, guard_var, GuardVerdict};

        let Expr::Path(name) = self.body.expr(receiver_expr) else {
            return false;
        };
        let Some(use_stmt) = self.body.enclosing_stmt(receiver_expr) else {
            return false;
        };
        let guards: Vec<_> = branch_guards_for_stmt(&self.body, use_stmt)
            .into_iter()
            .filter(|branch| guard_var(&branch.guard).eq_ignore_case(name))
            .collect();
        if guards.is_empty() {
            return false;
        }

        let key = name.as_str().fold_lower();
        let Some(reaching) = self.reaching_assignment_types(receiver_expr, &key) else {
            return false;
        };
        if reaching.is_empty() {
            return false;
        }
        reaching.iter().all(|def| {
            // A definition establishing no type of its own — a BSL parameter, an
            // element of an untyped collection — cannot keep the miss provable
            // either. `Unknown` and `Any` carry no arms, so they say as little as
            // no record at all.
            let Some(ty) = def.ty else {
                return true;
            };
            if self.is_unknown(ty) || matches!(self.db.lookup_type(ty), TypeKind::Any) {
                return true;
            }
            guards.iter().any(|branch| {
                // Only a definition made inside the guarded `Если` escapes the
                // guard; a binding is never inside one.
                let made_inside_the_guard = def.stmt.is_some_and(|def_stmt| {
                    crate::narrow::stmt_covers_stmt(&self.body, branch.if_stmt, def_stmt.to_idx())
                });
                !made_inside_the_guard
                    && apply_branch_guard(self.db, &branch.guard, branch.on_true, ty)
                        == GuardVerdict::Contradicted
            })
        })
    }

    /// The definitions of `var_key` reaching `use_expr`. `None` overall when
    /// reaching definitions are unavailable for the owner.
    fn reaching_assignment_types(
        &self,
        use_expr: ExprId,
        var_key: &str,
    ) -> Option<Vec<ReachingDef>> {
        let stmt_id = self.body.enclosing_stmt(use_expr)?;
        let module = hir_def::ModuleId::new(self.context_file_id);
        let body_defs = match self.owner {
            DefWithBodyId::Method(local_id) => self.db.method_reaching_definitions(
                MethodIdInput::new(self.db, hir_def::MethodId { module, local_id }),
            )?,
            DefWithBodyId::ModuleCode => {
                self.db.module_code_reaching_definitions(self.context_file_id)?
            }
        };
        let defs = body_defs.defs_for_var_at_stmt(var_key, stmt_id)?;
        Some(
            defs.into_iter()
                .map(|def| match def.def_site {
                    dataflow::reaching_defs::DefSite::Assignment(stmt_raw) => {
                        let stmt = StmtId::from_raw(stmt_raw);
                        let ty = match self.body.stmt(stmt) {
                            Stmt::Assign { value, .. } => {
                                self.expr_types.get(&ExprId::from_idx(*value)).copied()
                            }
                            _ => None,
                        };
                        ReachingDef { stmt: Some(stmt), ty }
                    }
                    // A binding is not a statement, but inference does record a
                    // type for it when it could work one out — a `Для` counter
                    // has one, a BSL parameter usually does not.
                    dataflow::reaching_defs::DefSite::Parameter(binding)
                    | dataflow::reaching_defs::DefSite::VarDecl(binding)
                    | dataflow::reaching_defs::DefSite::ForLoop(binding)
                    | dataflow::reaching_defs::DefSite::ForEachLoop(binding) => {
                        ReachingDef { stmt: None, ty: self.binding_types.get(&binding).copied() }
                    }
                    dataflow::reaching_defs::DefSite::Unknown => {
                        ReachingDef { stmt: None, ty: None }
                    }
                })
                .collect(),
        )
    }

    /// Gives a parameter the fields its doc-comment declares, for use inside the method's own body.
    ///
    /// The materialised signature serves calls from the outside; within the body a parameter is a
    /// binding with no reaching write, and bare-name resolution answers `Unknown` for it. Without
    /// this seed a documented structure is offered at every call site and nowhere in the method
    /// that receives it.
    ///
    /// Ordinary modules only: effective (`&ИзменениеИКонтроль`) and weaving inference build their
    /// symbols differently, and are left exactly as they were.
    fn seed_documented_structure_parameters(&mut self) {
        let DefWithBodyId::Method(local_id) = self.owner else {
            return;
        };
        if self.local_symbols.is_some() || self.weaving_base.is_some() {
            return;
        }

        let module = hir_def::ModuleId::new(self.context_file_id);
        let method_id = hir_def::MethodId { module, local_id };
        let Some(method_symbol) = self.db.interface_method(method_id) else {
            return;
        };
        if method_symbol.docs.is_none() {
            return;
        }

        let signature = crate::method_resolution::materialise_signature(self.db, &method_symbol);
        // A parameter documented by pointing at another method is seeded from that method's
        // documentation; one documented inline keeps the type the signature already lowered.
        let referenced = crate::doc_see::doc_see_signature_query(
            self.db,
            hir_def::MethodIdInput::new(self.db, method_id),
        );
        for (index, (binding, ty)) in
            self.body.params().zip(signature.params.iter().copied()).enumerate()
        {
            let ty = referenced.param(index).unwrap_or(ty);
            if !crate::lower::doc_structure::is_doc_structure(self.db, ty) {
                continue;
            }
            self.binding_types.insert(binding, ty);
            let name = NormName::intern(self.body.binding(binding).name.as_str());
            self.var_types.insert(name, ty);
        }
    }

    pub fn infer_all(&mut self) {
        let _p = tracing::debug_span!("infer_all").entered();

        self.structure_shapes = if !crate::structure_keys::body_constructs_structure(&self.body) {
            // No `Новый Структура` in this body → no tracked root → skip collection and the Stage-2
            // forwarder entirely (keeps the feature off the hot path for most bodies).
            FxHashMap::default()
        } else {
            use crate::structure_keys::{collect_structure_shapes, SeedRoots};
            // Stage 2 interprocedural forwarding is ordinary-modules only — effective
            // (`&ИзменениеИКонтроль`) and weaving inference use separate symbol contexts and are
            // out of scope, so no forwarder is built there (byte-identical Stage-1 behaviour).
            let ordinary = self.local_symbols.is_none() && self.weaving_base.is_none();
            if ordinary {
                let resolver = self.get_resolver();
                let module = hir_def::ModuleId::new(self.context_file_id);
                // Callee summaries come from the memoised (acyclic) query, so reading them here does
                // not couple inference's fixpoint with a second one.
                let summarize = |mid: hir_def::MethodId| {
                    crate::structure_param_keys::structure_param_keys_query(
                        self.db,
                        hir_def::MethodIdInput::new(self.db, mid),
                    )
                    .clone()
                };
                let forwarder = crate::structure_param_keys::Forwarder::new(
                    self.db, &resolver, module, &self.body, &summarize,
                );
                collect_structure_shapes(&self.body, SeedRoots::NewLiterals, Some(&forwarder))
            } else {
                collect_structure_shapes(&self.body, SeedRoots::NewLiterals, None)
            }
        };

        self.seed_documented_structure_parameters();

        let stmts: Vec<StmtIdx> = self.body.body_stmts_typed().to_vec();
        self.infer_stmts(&stmts);

        // Not every expr belongs to a statement: a parameter's default value
        // hangs off its binding, so the statement walk never reaches it. Such
        // exprs are typed here for the IDE layer (hover/goto resolve through
        // the source map), but they carry no execution context: statement-level
        // `#Если` narrowing never saw them, so an environment verdict from
        // this sweep would double or contradict the one issued during the
        // walk. An empty body mask disables every environment check.
        let walk_env =
            std::mem::replace(&mut self.body_env, hir_def::execution_env::EnvFlags::EMPTY);
        let expr_ids: Vec<ExprId> = self.body.exprs_iter().map(|(id, _)| id).collect();
        for expr_id in expr_ids {
            self.infer_expr(expr_id);
        }
        self.body_env = walk_env;

        debug!(
            "inferred {} expression types, {} var types, {} diagnostics",
            self.expr_types.len(),
            self.var_types.len(),
            self.diagnostics.len()
        );
    }

    fn infer_stmts(&mut self, stmts: &[StmtIdx]) {
        for &stmt_idx in stmts {
            self.infer_stmt(stmt_idx);
        }
    }

    fn infer_stmt(&mut self, stmt_idx: StmtIdx) {
        if self.in_uncompiled_branch {
            self.uncompiled_stmts.push(StmtId::from_idx(stmt_idx));
        }
        let stmt = self.body.stmt_idx(stmt_idx).clone();
        match &stmt {
            Stmt::Assign { target, value } => {
                let value_ty = self.infer_expr(ExprId::from_idx(*value));
                let target_id = ExprId::from_idx(*target);
                let mut infer_target = true;

                let target_expr = self.body.expr_idx(*target).clone();
                match &target_expr {
                    Expr::Path(name) => {
                        let resolver = self.get_resolver();
                        let resolved_name = resolver.resolve_name(self.db, name);
                        let existing_module_variable = matches!(
                            resolved_name,
                            Some(hir_def::resolver::Resolution::Variable(_))
                        );
                        let user_shadows = matches!(
                            resolved_name,
                            Some(hir_def::resolver::Resolution::Method(_))
                                | Some(hir_def::resolver::Resolution::Variable(_))
                        ) || self.body_declares_binding(name);
                        let form_self_resolution = if user_shadows {
                            None
                        } else {
                            crate::form_self::resolve_form_self_property(self.db, &resolver, name)
                        };
                        let unheld_collection = bsl_metadata::MdoType::from_plural(name.as_str())
                            .filter(|mdo| mdo.manager_type_prefix().is_some())
                            .filter(|_| !self.manager_collection_shadowed(name));
                        match form_self_resolution {
                            Some(prop) => {
                                if prop.is_readonly {
                                    let form_ty = self.db.platform_object(
                                        crate::form_self::FORM_TYPE_NAME.to_string(),
                                    );
                                    self.push_inference_diagnostic(
                                        InferenceDiagnostic::ReadOnlyPropertyAssignment {
                                            lhs: ExprId::from_idx(*target),
                                            receiver_ty: form_ty,
                                            field_name: name.clone(),
                                        },
                                    );
                                }
                            }
                            None if existing_module_variable => {}
                            // Writing to an unheld metadata-collection name targets a
                            // Global-context property, which the platform refuses — no
                            // local is declared. Type the target as the collection and
                            // stop: the availability check belongs to READING a member,
                            // and the illegal write is a different defect, not yet
                            // reported.
                            None if unheld_collection.is_some() => {
                                self.push_inference_diagnostic(
                                    InferenceDiagnostic::GlobalPropertyNotWritable {
                                        lhs: target_id,
                                        name: name.clone(),
                                    },
                                );
                                let collection = self
                                    .db
                                    .manager_collection(unheld_collection.expect("just checked"));
                                self.expr_types.insert(target_id, collection);
                                infer_target = false;
                            }
                            None => {
                                let key = name.as_str().fold_lower();
                                let norm_key = NormName::intern(name.as_str());
                                self.assigned_var_names.insert(norm_key);
                                let unknown = self.db.unknown();
                                self.implicit_locals
                                    .entry(key.clone())
                                    .and_modify(|info| {
                                        info.assignments.push(ImplicitLocalAssignment {
                                            target: target_id,
                                            ty: value_ty,
                                        });
                                        if info.ty == unknown && value_ty != unknown {
                                            info.ty = value_ty;
                                        }
                                    })
                                    .or_insert_with(|| ImplicitLocalInfo {
                                        name: name.clone(),
                                        first_assignment: target_id,
                                        ty: value_ty,
                                        assignments: vec![ImplicitLocalAssignment {
                                            target: target_id,
                                            ty: value_ty,
                                        }],
                                    });
                                if !self.is_unknown(value_ty) {
                                    self.var_types.insert(norm_key, value_ty);
                                }
                            }
                        }
                    }
                    Expr::Field { base, field } => {
                        let base_ty = self.infer_expr(ExprId::from_idx(*base));
                        let obj_resolver = crate::object_resolver::DbObjectResolver::new(
                            self.db,
                            self.context_file_id,
                        );
                        let resolver = self.get_resolver();
                        if let Some(info) = crate::form_items::lookup_form_item_field(
                            self.db, &resolver, base_ty, field,
                        ) {
                            if info.is_readonly {
                                self.push_inference_diagnostic(
                                    InferenceDiagnostic::ReadOnlyPropertyAssignment {
                                        lhs: target_id,
                                        receiver_ty: base_ty,
                                        field_name: field.clone(),
                                    },
                                );
                            }
                            self.expr_types.insert(target_id, info.ty);
                            infer_target = false;
                        } else if let Some(info) = crate::field_lookup::lookup_field(
                            self.db,
                            &obj_resolver,
                            base_ty,
                            field,
                        ) {
                            self.push_deprecated_platform_member_diagnostic(
                                target_id, base_ty, field, true,
                            );
                            if let crate::field_enum::FieldOrigin::PlatformProperty { env } =
                                info.origin
                            {
                                self.check_member_env(
                                    target_id,
                                    field,
                                    env,
                                    EnvMemberKind::Property,
                                );
                                self.check_min_platform_type_member(
                                    target_id, base_ty, field, true,
                                );
                                self.check_compat_mode_type_member(target_id, base_ty, field);
                            }
                            if info.is_readonly {
                                self.push_inference_diagnostic(
                                    InferenceDiagnostic::ReadOnlyPropertyAssignment {
                                        lhs: target_id,
                                        receiver_ty: base_ty,
                                        field_name: field.clone(),
                                    },
                                );
                            }
                            self.expr_types.insert(target_id, info.ty);
                            infer_target = false;
                        }
                    }
                    _ => {}
                }

                if infer_target {
                    if matches!(target_expr, Expr::Path(_)) {
                        self.assignment_targets.insert(target_id);
                    }
                    self.infer_expr(target_id);
                }
            }

            Stmt::Expr(expr_idx) => {
                self.infer_expr(ExprId::from_idx(*expr_idx));
            }

            Stmt::If(if_stmt) => {
                self.infer_expr(ExprId::from_idx(if_stmt.condition));
                self.infer_stmts(&if_stmt.then_branch);
                for (cond, branch) in if_stmt.elsif_branches.iter() {
                    self.infer_expr(ExprId::from_idx(*cond));
                    self.infer_stmts(branch);
                }
                if let Some(else_branch) = &if_stmt.else_branch {
                    self.infer_stmts(else_branch);
                }
            }

            Stmt::PreprocIf(preproc) => {
                // Availability checks inside a branch see only the
                // environments its condition compiles for; nesting works
                // because each frame restores its own parent mask.
                let parent = self.body_env;
                let parent_uncompiled = self.in_uncompiled_branch;
                let mut remaining = parent;
                self.body_env = preproc.condition.narrow_branch(&mut remaining);
                self.in_uncompiled_branch = parent_uncompiled || self.body_env.is_empty();
                self.infer_stmts(&preproc.then_branch);
                for (idx, (_, _, branch)) in preproc.elsif_branches.iter().enumerate() {
                    self.body_env = match preproc.elsif_conditions.get(idx) {
                        Some(cond) => cond.narrow_branch(&mut remaining),
                        // A branch without a lowered condition (broken
                        // alignment) poisons the rest of the chain,
                        // including #Иначе.
                        None => {
                            remaining = hir_def::execution_env::EnvFlags::EMPTY;
                            remaining
                        }
                    };
                    self.in_uncompiled_branch = parent_uncompiled || self.body_env.is_empty();
                    self.infer_stmts(branch);
                }
                if let Some(else_branch) = &preproc.else_branch {
                    self.body_env = remaining;
                    self.in_uncompiled_branch = parent_uncompiled || self.body_env.is_empty();
                    self.infer_stmts(else_branch);
                }
                self.body_env = parent;
                self.in_uncompiled_branch = parent_uncompiled;
            }

            Stmt::While { condition, body } => {
                self.infer_expr(ExprId::from_idx(*condition));
                self.infer_stmts(body);
            }

            Stmt::For { var, from, to, body } => {
                self.infer_expr(ExprId::from_idx(*from));
                self.infer_expr(ExprId::from_idx(*to));
                let var_name = NormName::intern(self.body.binding_idx(*var).name.as_str());
                let number = self.db.number(None, None);
                self.var_types.insert(var_name, number);
                self.binding_types.insert(BindingId::from_idx(*var), number);
                self.infer_stmts(body);
            }

            Stmt::ForEach { var, collection, body } => {
                let coll_ty = self.infer_expr(ExprId::from_idx(*collection));
                if let Some(elem_ty) =
                    crate::iteration_lookup::resolve_iter_element_ty(self.db, coll_ty)
                {
                    let var_name = NormName::intern(self.body.binding_idx(*var).name.as_str());
                    self.var_types.insert(var_name, elem_ty);
                    self.binding_types.insert(BindingId::from_idx(*var), elem_ty);
                }
                self.infer_stmts(body);
            }

            Stmt::Try { body, except } => {
                self.infer_stmts(body);
                self.infer_stmts(except);
            }

            Stmt::Return { value } => {
                if let Some(expr_idx) = value {
                    let expr_id = ExprId::from_idx(*expr_idx);
                    self.infer_expr(expr_id);
                    self.return_expr_ids.push(expr_id);
                }
            }

            Stmt::Raise { value } => {
                if let Some(expr_idx) = value {
                    self.infer_expr(ExprId::from_idx(*expr_idx));
                }
            }

            Stmt::Execute { expr } => {
                self.infer_expr(ExprId::from_idx(*expr));
            }

            Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
                self.infer_expr(ExprId::from_idx(*event));
                self.infer_expr(ExprId::from_idx(*handler));
            }

            Stmt::VarDecl { .. }
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Goto(_)
            | Stmt::Label(_) => {}
        }
    }

    fn try_synthesise_query_projections(&self, args: &[ExprIdx]) -> Arc<[Option<Arc<Projection>>]> {
        let Some(arg_idx) = args.first().copied() else {
            return Arc::from([]);
        };
        let arg_id = ExprId::from_idx(arg_idx);
        if !matches!(self.body.expr(arg_id), Expr::Literal(Literal::String(_))) {
            return Arc::from([]);
        }
        let Some(pkg) =
            hir_def::sdbl_package_for(self.db, self.context_file_id, self.owner, arg_id)
        else {
            return Arc::from([]);
        };
        crate::sdbl_bridge::package_to_projections(self.db, &pkg).into()
    }

    fn infer_expr(&mut self, expr_id: ExprId) -> TypeId {
        if let Some(ty) = self.expr_types.get(&expr_id) {
            return *ty;
        }

        let expr = self.body.expr(expr_id).clone();
        trace!("inferring expr {:?}: {:?}", expr_id, expr);

        let ty = match &expr {
            Expr::Missing => self.db.unknown(),

            Expr::Literal(lit) => self.infer_literal(lit),

            Expr::Path(name) => self.infer_path_name(name, expr_id),

            Expr::BinaryOp { lhs, rhs, op } => {
                self.infer_binary_op(ExprId::from_idx(*lhs), ExprId::from_idx(*rhs), *op)
            }

            Expr::UnaryOp { expr, op } => self.infer_unary_op(ExprId::from_idx(*expr), *op),

            Expr::Ternary { condition, then_expr, else_expr } => {
                self.infer_expr(ExprId::from_idx(*condition));
                let then_ty = self.infer_expr(ExprId::from_idx(*then_expr));
                let else_ty = self.infer_expr(ExprId::from_idx(*else_expr));

                if then_ty == else_ty {
                    then_ty
                } else {
                    self.db.unknown()
                }
            }

            Expr::Call { callee, args } => {
                let converted_args: Vec<ExprId> =
                    args.iter().map(|&arg| ExprId::from_idx(arg)).collect();
                self.infer_call(expr_id, ExprId::from_idx(*callee), &converted_args)
            }

            Expr::MethodCall { receiver, method, args } => {
                let receiver_ty = self.infer_expr(ExprId::from_idx(*receiver));
                for &arg in args.iter() {
                    self.infer_expr(ExprId::from_idx(arg));
                }

                match crate::method_lookup::lookup_method(self.db, receiver_ty, method) {
                    Some(info) => {
                        self.check_member_env(expr_id, method, info.env, EnvMemberKind::Method);
                        self.check_min_platform_type_member(expr_id, receiver_ty, method, false);
                        info.return_ty
                    }
                    None => self.db.unknown(),
                }
            }

            Expr::Index { base, index } => {
                let base_ty = self.infer_expr(ExprId::from_idx(*base));
                self.infer_expr(ExprId::from_idx(*index));

                let base_kind = self.db.lookup_type(base_ty);
                match base_kind {
                    TypeKind::Array(facet) => facet.element.unwrap_or_else(|| self.db.unknown()),
                    TypeKind::QueryBatchResult { per_query } => {
                        let index_expr = self.body.expr(ExprId::from_idx(*index));
                        let projection = const_eval_literal_index(index_expr)
                            .and_then(|i| per_query.get(i).cloned())
                            .flatten();
                        self.db.query_result(projection, ProjectionSource::Unknown)
                    }
                    _ => self.db.unknown(),
                }
            }

            Expr::Field { base, field } => {
                let base_ty = self.infer_expr(ExprId::from_idx(*base));

                let resolver = self.get_resolver();
                let obj_resolver =
                    crate::object_resolver::DbObjectResolver::new(self.db, self.context_file_id);
                if let Some(info) =
                    crate::form_items::lookup_form_item_field(self.db, &resolver, base_ty, field)
                {
                    info.ty
                } else if let Some(info) =
                    crate::field_lookup::lookup_field(self.db, &obj_resolver, base_ty, field)
                {
                    self.push_deprecated_platform_member_diagnostic(expr_id, base_ty, field, true);
                    if let crate::field_enum::FieldOrigin::PlatformProperty { env } = info.origin {
                        self.check_member_env(expr_id, field, env, EnvMemberKind::Property);
                        self.check_min_platform_type_member(expr_id, base_ty, field, true);
                        self.check_compat_mode_type_member(expr_id, base_ty, field);
                    }
                    info.ty
                } else if let Some(info) = crate::manager_lookup::lookup_manager_field(
                    self.db,
                    &obj_resolver,
                    base_ty,
                    field,
                ) {
                    info.ty
                } else {
                    let base_kind = self.db.lookup_type(base_ty);
                    // A metadata collection is as closed a receiver as an object: its
                    // members are exactly the configuration's objects of that kind, so a
                    // miss is a real defect and not an unknown shape. Reporting it here is
                    // what keeps `Справочники.НетТакого.Метод()` from going silent now that
                    // the chain is no longer folded into one node.
                    //
                    // A name the author has not finished writing (`Справочники.`) is not a
                    // miss: lowering keeps the incomplete field as the missing placeholder,
                    // and accusing the configuration on every keystroke would be noise.
                    //
                    // An object a common attribute may reach without the reader being able
                    // to say so has an open field set, and a miss on it proves nothing.
                    let closed_receiver = (matches!(
                        base_kind,
                        TypeKind::MetadataRef(_)
                            | TypeKind::ThisObject { .. }
                            | TypeKind::ManagerCollection(_)
                    ) && !crate::field_enum::metadata_fields_open(
                        self.db,
                        &obj_resolver,
                        base_ty,
                    )) || matches!(base_kind, TypeKind::Structure(facet) if facet.closed);
                    if !field.is_missing() && closed_receiver {
                        self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedField {
                            expr: expr_id,
                            receiver_ty: base_ty,
                            field_name: field.clone(),
                        });
                    }
                    self.db.unknown()
                }
            }

            Expr::New { type_name, args } => {
                for &arg in args.iter() {
                    self.infer_expr(ExprId::from_idx(arg));
                }

                // `Новый("X")` / `Новый(Тип("X"))`: имя типа приходит значением.
                // Список аргументов у этой формы — `(<Тип>, <МассивПараметров>)`,
                // то есть НЕ позиционные аргументы конструктора, поэтому ниже она
                // не участвует ни в привязке к перегрузкам, ни в синтезе проекций
                // запроса — иначе имя типа было бы разобрано как первый параметр.
                let body = Arc::clone(&self.body);
                let dynamic_name = match (type_name, args.first()) {
                    (None, Some(&first)) => self.constructed_type_name(&body, first),
                    _ => None,
                };

                if let Some(name) = type_name {
                    let platform = bsl_platform::PlatformDataInner::instance();
                    let platform_type = platform.get_type(name.as_str());
                    let environment = platform_type.map_or(
                        hir_def::execution_env::EnvFlags::ALL,
                        |platform_type| {
                            hir_def::execution_env::EnvFlags::from_platform_context(
                                platform_type.context.as_ref(),
                            )
                        },
                    );
                    if !platform.is_ambiguous_type_name(name.as_str()) && platform_type.is_some() {
                        self.check_member_env(expr_id, name, environment, EnvMemberKind::Type);
                        self.check_min_platform(expr_id, name, || {
                            crate::min_platform::constructed_type(name.as_str())
                        });
                    }
                    let ctors = platform.get_constructors(name.as_str());
                    if !ctors.is_empty() {
                        if let Ok(candidates) = builtin::candidates::constructor_candidates(
                            self.db,
                            &ctors,
                            environment,
                        ) {
                            let arg_ids: Vec<ExprId> =
                                args.iter().copied().map(ExprId::from_idx).collect();
                            let argument_types = arg_ids
                                .iter()
                                .map(|arg| {
                                    self.expr_types
                                        .get(arg)
                                        .copied()
                                        .unwrap_or_else(|| self.db.unknown())
                                })
                                .collect::<Vec<_>>();
                            let projection = crate::call_binding::resolve_binding(
                                self.db,
                                candidates,
                                &argument_types,
                            );
                            if !self.body.is_recovered(expr_id) {
                                self.call_arg_bindings.push(CallArgBinding {
                                    owner: self.owner,
                                    call_expr: expr_id,
                                    args: arg_ids,
                                    candidate: projection.semantic,
                                });
                            }
                        }
                    }
                }

                if let Some(raw) = dynamic_name {
                    self.check_constructed_type_env(expr_id, raw);
                }

                // Обе формы обязаны сходиться на одном типе, иначе `Новый Запрос`
                // и `Новый("Запрос")` начали бы конфликтовать между собой.
                let ctor_name = type_name.as_ref().map(Name::as_str).or(dynamic_name);
                let is_query_ctor = ctor_name.is_some_and(|name| {
                    crate::method_lookup::is_platform_name_str(name, "Запрос", "Query")
                });
                if is_query_ctor {
                    let projections = match type_name {
                        Some(_) => self.try_synthesise_query_projections(args),
                        None => Arc::from([]),
                    };
                    self.db.query(projections.iter().cloned().collect())
                } else {
                    match (type_name, dynamic_name) {
                        (Some(name), _) => {
                            TyLoweringContext::new().lower_bare_name_id(self.db, name)
                        }
                        (None, Some(raw)) => {
                            crate::lower::type_string::lower_constructed_type_name_typeid(
                                self.db, raw,
                            )
                        }
                        (None, None) => self.db.unknown(),
                    }
                }
            }

            Expr::Array(elements) => {
                for &elem in elements.iter() {
                    self.infer_expr(ExprId::from_idx(elem));
                }

                self.db.array(None)
            }

            Expr::Await { expr } => self.infer_expr(ExprId::from_idx(*expr)),
        };

        self.expr_types.insert(expr_id, ty);
        ty
    }

    fn infer_path_name(&mut self, name: &hir_def::Name, expr_id: ExprId) -> TypeId {
        let usage = if self.assignment_targets.contains(&expr_id) {
            BareNameUse::AssignmentTarget
        } else {
            BareNameUse::ReadValue
        };
        let resolution = self.resolve_bare_name(name, expr_id, usage);
        let verdict = match &resolution {
            BareNameResolution::Resolved { .. } => BareNameVerdict::Resolved,
            BareNameResolution::Absent => BareNameVerdict::Absent,
            BareNameResolution::Indeterminate { .. } => BareNameVerdict::Indeterminate,
        };
        self.bare_name_verdicts.insert(expr_id, verdict);

        match resolution {
            BareNameResolution::Resolved { ty, .. } => ty,
            BareNameResolution::Absent => {
                if usage == BareNameUse::ReadValue && !name.is_missing() {
                    self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedName {
                        expr: expr_id,
                        name: name.clone(),
                    });
                }
                self.db.unknown()
            }
            BareNameResolution::Indeterminate { .. } => self.db.unknown(),
        }
    }

    fn resolve_bare_name(
        &mut self,
        name: &hir_def::Name,
        expr_id: ExprId,
        usage: BareNameUse,
    ) -> BareNameResolution {
        use hir_def::resolver::Resolution;

        debug_assert!(matches!(usage, BareNameUse::ReadValue | BareNameUse::AssignmentTarget));
        let resolver = self.get_resolver();
        let found =
            |ty, origin, availability| BareNameResolution::Resolved { ty, origin, availability };
        let all_env = hir_def::execution_env::EnvFlags::ALL;

        let name_lower = name.as_str().fold_lower();
        let self_name_wins =
            is_self_name(&name_lower) && !self.self_name_is_shadowed(name, &resolver);
        if self_name_wins && (name_lower == "этотобъект" || name_lower == "thisobject") {
            if let Some(owner) = crate::this_object::resolve_this_object_owner(self.db, &resolver) {
                trace!("resolved {} as ThisObject {{ owner: {:?} }}", name, owner);
                return found(
                    self.db.mk_this_object(
                        ConfigId::Root,
                        MdoRefFacet::new(owner.0, owner.1.as_str().to_string()),
                    ),
                    BareNameOrigin::SelfName,
                    all_env,
                );
            }
            if let Some(owner) = crate::this_object::resolve_this_manager_owner(self.db, &resolver)
            {
                trace!("resolved {} as ThisManager {{ owner: {:?} }}", name, owner);
                return found(
                    self.db.mk_this_manager(
                        ConfigId::Root,
                        MdoRefFacet::new(owner.0, owner.1.as_str().to_string()),
                    ),
                    BareNameOrigin::SelfName,
                    all_env,
                );
            }
            if let Some((mdo, name)) =
                crate::this_object::resolve_this_record_set_owner(self.db, &resolver)
            {
                if let Some(kind) = hir_def::ty::MetadataKind::record_set_kind_for(mdo) {
                    trace!(
                        "resolved {} as record-set MetadataRef {{ {:?}, {} }}",
                        name.as_str(),
                        kind,
                        name
                    );
                    return found(
                        self.db.metadata_ref(kind, name.as_str().to_string(), &RootConfigCtx),
                        BareNameOrigin::SelfName,
                        all_env,
                    );
                }
            }
        }
        // `ЭтаФорма` denotes the form itself just as `ЭтотОбъект` does — the
        // deprecated spelling only earns the `UsingThisForm` hint, not a worse
        // type. The platform's own `ЭтаФорма` entry belongs to the ORDINARY form
        // (`Форма`), so a managed form module finds nothing there and would end
        // up treating the receiver as an unresolved module.
        if self_name_wins && crate::this_object::is_managed_form_module(self.db, &resolver) {
            trace!("resolved {} as managed form Self", name);
            return found(
                self.db.platform_object(crate::form_self::FORM_TYPE_NAME.to_string()),
                BareNameOrigin::SelfName,
                all_env,
            );
        }

        let resolved = resolver.resolve_name(self.db, name);

        if let Some(ty) = self.var_types.get(&NormName::intern(name.as_str())) {
            trace!("resolved {} via var_types = {:?}", name, ty);
            let ty_id = *ty;
            // Structure-literal key enrichment: a local typed as a `Структура` gets its collected
            // literal keys (and value types known up to this read) surfaced for completion/hover.
            // Soft — never feeds a diagnostic.
            if matches!(self.db.lookup_type(ty_id), TypeKind::Structure(_)) {
                let key = name.as_str().fold_lower();
                if let Some(rich) =
                    crate::structure_keys::materialize(self.db, &self.structure_shapes, &key)
                {
                    return found(rich, BareNameOrigin::FlowLocal, all_env);
                }
            }
            if crate::method_lookup::receiver_needs_refinement_id(self.db, ty_id) {
                if let Some(projections) = crate::query_text_dataflow::refine_query_at_use_site(
                    self.db,
                    self.context_file_id,
                    self.owner,
                    expr_id,
                    name,
                    &self.body,
                ) {
                    let refined = self.db.query(projections.iter().cloned().collect());
                    trace!("Phase F refined {} to {:?}", name, refined);
                    return found(refined, BareNameOrigin::FlowLocal, all_env);
                }
            }
            return found(ty_id, BareNameOrigin::FlowLocal, all_env);
        }

        let user_shadows =
            matches!(resolved, Some(Resolution::Method(_)) | Some(Resolution::Variable(_)))
                || resolver.resolve_module_variable(self.db, name).is_some();
        let body_binding_shadows = self.body_declares_binding(name);

        if user_shadows || body_binding_shadows {
            return found(self.db.unknown(), BareNameOrigin::UserBinding, all_env);
        }

        let resolver_says_builtin = matches!(resolved, Some(Resolution::Builtin(_)));
        let hir_sig = builtin::builtin_functions().get(name.as_str());
        // BSL procedures/functions are not first-class values: EDT accepts
        // `СтрДлина(...)` but reports `A = СтрДлина` as an undefined variable.
        if usage != BareNameUse::ReadValue && (resolver_says_builtin || hir_sig.is_some()) {
            return found(self.db.unknown(), BareNameOrigin::Builtin, all_env);
        }

        if !user_shadows && !body_binding_shadows {
            if let Some(resolution) =
                crate::form_self::resolve_form_self_property(self.db, &resolver, name)
            {
                trace!("resolved {} as managed-form Self property", name);
                return found(resolution.return_ty, BareNameOrigin::FormMember, all_env);
            }
        }

        if !user_shadows && !body_binding_shadows {
            if let Some(ty) = crate::form_attr::resolve_form_attribute(self.db, &resolver, name) {
                trace!("resolved {} as managed-form attribute", name);
                return found(ty, BareNameOrigin::FormMember, all_env);
            }
        }

        let workspace_owns_common_module = resolver.user_common_module_exists(self.db, name);

        // Implicit `ЭтотОбъект` members (attributes, tabular sections, record-set fields) of the
        // current object/record-set module win over a same-named common module: in that module
        // the bare name denotes the object's own member, not the workspace module. Resolution
        // returns `None` outside an object/record-set module, so a plain common-module name (no
        // colliding member) falls through to the common-module typing below.
        if !user_shadows && !body_binding_shadows {
            if let Some(ty) =
                crate::this_object_attr::resolve_this_object_member(self.db, &resolver, name)
            {
                trace!("resolved {} as implicit ЭтотОбъект.{} member", name, name);
                return found(ty, BareNameOrigin::ObjectMember, all_env);
            }
        }

        if !user_shadows && !body_binding_shadows {
            if let Some(ty) =
                crate::this_object_attr::resolve_this_record_set_member(self.db, &resolver, name)
            {
                trace!("resolved {} as implicit record-set ЭтотОбъект.{} member", name, name);
                return found(ty, BareNameOrigin::ObjectMember, all_env);
            }
        }

        if self.assignment_target_names.contains(&NormName::intern(name.as_str()))
            && self.implicit_local_reaches(expr_id, name)
        {
            return found(self.db.unknown(), BareNameOrigin::FlowLocal, all_env);
        }

        // A bare common-module reference (`ОбщегоНазначения`, used directly) is a value of that
        // module's type, so member calls / hover / completion resolve against its API. Object
        // members above already won; the remaining shadow guard mirrors
        // `dispatch_bare_ident_field_call` (local assignment, declared binding, user
        // method/variable, form attribute / form-self resolved earlier). A module named like a
        // builtin global bailed to Unknown at the builtin check and is resolved by name dispatch.
        if workspace_owns_common_module
            && !user_shadows
            && !body_binding_shadows
            && !self.assigned_var_names.contains(&NormName::intern(name.as_str()))
        {
            let source_root_id =
                self.db.file_source_root_input(self.context_file_id).source_root_id(self.db);
            if let Some(canonical) =
                self.db.module_index(source_root_id).canonical_common_module_name(name)
            {
                trace!("resolved {} as common module type", name);
                return found(
                    self.db.common_module(canonical.to_string(), ConfigId::Root),
                    BareNameOrigin::CommonModule,
                    all_env,
                );
            }
        }

        // A manager module may qualify one of its own methods with the metadata
        // object's name (`ТестовыйОтчёт.Метод()`). Name dispatch already treats
        // that spelling as this module's object manager; classify the root the
        // same way so it cannot simultaneously become `Absent`.
        let assigned_in_body = self.assigned_var_names.contains(&NormName::intern(name.as_str()));
        if !assigned_in_body {
            if let Some((mdo_type, self_name)) =
                crate::this_object::resolve_this_manager_owner(self.db, &resolver)
            {
                if name.as_str().fold_lower() == self_name.as_str().fold_lower() {
                    return found(
                        self.db.object_manager(
                            mdo_type,
                            self_name.as_str().to_string(),
                            &RootConfigCtx,
                        ),
                        BareNameOrigin::SelfName,
                        all_env,
                    );
                }
            }
        }

        // Everything below denotes a PLATFORM global, so a user symbol holding the
        // name rules them all out. Declared bindings, module variables/methods,
        // form and `ЭтотОбъект` members have already returned above; what remains
        // is an implicit local written earlier in this body and a workspace common
        // module.
        let user_holds_name = workspace_owns_common_module || assigned_in_body;

        // Exported application-context variables are user globals and therefore
        // shadow platform globals, just as exported user methods shadow platform
        // global functions on the call path.
        if !user_holds_name {
            if let Some(export) = self.global_read_export(name) {
                self.check_member_env(
                    expr_id,
                    name,
                    export.availability,
                    EnvMemberKind::GlobalVariable,
                );
                return found(self.db.unknown(), BareNameOrigin::GlobalExport, export.availability);
            }
        }

        // Metadata collections are the exception, and it is not a stylistic one: a
        // collection name is a Global-context PROPERTY, and assigning to it does not
        // declare a local — the platform refuses the write ("property is not
        // writable") — so the name keeps denoting the collection throughout the body.
        // Only a declared owner takes it, and every declared owner has returned
        // above. `manager_collection_shadowed` judges the same way, so this typing
        // and the availability diagnostic stay in agreement.
        if !workspace_owns_common_module {
            if let Some(mdo_type) = bsl_metadata::MdoType::from_plural(name.as_str()) {
                if mdo_type.manager_type_prefix().is_some() {
                    trace!("resolved {} as manager collection {:?}", name, mdo_type);
                    self.check_manager_collection_env(expr_id, name, mdo_type);
                    let availability =
                        crate::platform_global_lookup::manager_collection_env(mdo_type);
                    return found(
                        self.db.manager_collection(mdo_type),
                        BareNameOrigin::ManagerCollection,
                        availability,
                    );
                }
            }
        }

        if !user_holds_name {
            if let Some((id, env)) =
                crate::platform_global_lookup::resolve_platform_global_property(self.db, name)
            {
                trace!("resolved {} as platform global → {:?}", name, id);
                self.check_member_env(expr_id, name, env, EnvMemberKind::GlobalProperty);
                self.check_min_platform(expr_id, name, || {
                    crate::min_platform::global_property(name.as_str())
                });
                self.check_compat_mode(expr_id, name, || {
                    crate::compat_mode::hidden_global(name.as_str())
                });
                return found(id, BareNameOrigin::PlatformProperty, env);
            }
        }

        if !user_holds_name {
            if let Some(symbol) =
                bsl_platform::PlatformGlobalCatalog::instance().lookup(name.as_str())
            {
                if symbol.kind == bsl_platform::PlatformGlobalKind::Property
                    && symbol.capabilities.readable_as_value == Some(true)
                {
                    let availability = hir_def::execution_env::EnvFlags::from_platform_context(
                        symbol.context.as_ref(),
                    );
                    self.check_member_env(
                        expr_id,
                        name,
                        availability,
                        EnvMemberKind::GlobalProperty,
                    );
                    self.check_min_platform(expr_id, name, || {
                        crate::min_platform::global_property(name.as_str())
                    });
                    self.check_compat_mode(expr_id, name, || {
                        crate::compat_mode::hidden_global(name.as_str())
                    });
                    return found(
                        self.db.unknown(),
                        BareNameOrigin::PlatformProperty,
                        availability,
                    );
                }
            }
        }

        if !user_holds_name {
            if let Some(id) =
                crate::platform_global_lookup::resolve_platform_system_enum_type(self.db, name)
            {
                trace!("resolved {} as platform system enum → {:?}", name, id);
                let availability = bsl_platform::PlatformGlobalCatalog::instance()
                    .lookup(name.as_str())
                    .map_or(all_env, |symbol| {
                        hir_def::execution_env::EnvFlags::from_platform_context(
                            symbol.context.as_ref(),
                        )
                    });
                self.check_member_env(expr_id, name, availability, EnvMemberKind::Type);
                self.check_min_platform(expr_id, name, || {
                    crate::min_platform::platform_type(name.as_str())
                });
                return found(id, BareNameOrigin::PlatformSystemEnum, availability);
            }
        }

        if resolved.is_some() && !resolver_says_builtin {
            return found(self.db.unknown(), BareNameOrigin::UserBinding, all_env);
        }

        self.bare_name_miss()
    }

    /// What a miss of every owner amounts to: `Absent` only when the user global
    /// surface, the configuration root and the platform catalog are all complete.
    fn bare_name_miss(&mut self) -> BareNameResolution {
        // Building the cached maps also records whether every user-global body
        // (global common + application modules) was readable and enumerable.
        self.global_export_map();
        let mut gaps = Vec::new();
        if self.global_surface_incomplete {
            gaps.push(BareNameGap::UnreadUserGlobalSurface);
        }
        if !self.db.workspace_load_complete()
            || self.db.configurations(self.context_file_id).is_empty()
        {
            gaps.push(BareNameGap::MissingConfigurationRoot);
        }

        let target = self.db.target_platform_version();
        let platform_status =
            bsl_platform::PlatformGlobalCatalog::instance().status_for_target(target.as_deref());
        classify_bare_name_miss(gaps, platform_status)
    }

    fn infer_literal(&self, lit: &Literal) -> TypeId {
        match lit {
            Literal::Number(_) => self.db.number(None, None),
            Literal::String(_) => self.db.string(None, false),
            Literal::Date(_) => self.db.date(DateComponent::DateTime),
            Literal::Bool(_) => self.db.boolean(),
            Literal::Undefined => self.db.undefined(),
            Literal::Null => self.db.null(),
        }
    }

    fn infer_binary_op(&mut self, lhs: ExprId, rhs: ExprId, op: BinaryOp) -> TypeId {
        let lhs_ty = self.infer_expr(lhs);
        let rhs_ty = self.infer_expr(rhs);

        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                let lhs_kind = self.db.lookup_type(lhs_ty);
                let rhs_kind = self.db.lookup_type(rhs_ty);
                if op == BinaryOp::Add
                    && (matches!(lhs_kind, TypeKind::String(_))
                        || matches!(rhs_kind, TypeKind::String(_)))
                {
                    self.db.string(None, false)
                } else if matches!(lhs_kind, TypeKind::Number(_))
                    && matches!(rhs_kind, TypeKind::Number(_))
                {
                    self.db.number(None, None)
                } else {
                    self.db.unknown()
                }
            }

            BinaryOp::Eq
            | BinaryOp::Neq
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => self.db.boolean(),

            BinaryOp::And | BinaryOp::Or => self.db.boolean(),
        }
    }

    fn infer_unary_op(&mut self, expr: ExprId, op: UnaryOp) -> TypeId {
        let expr_ty = self.infer_expr(expr);

        match op {
            UnaryOp::Neg | UnaryOp::Plus => {
                let kind = self.db.lookup_type(expr_ty);
                if matches!(kind, TypeKind::Number(_)) {
                    self.db.number(None, None)
                } else {
                    self.db.unknown()
                }
            }
            UnaryOp::Not => self.db.boolean(),
        }
    }

    /// The plural as the SOURCE spells it, for a receiver literally written
    /// `<Collection>.<Object>` — or `None` when the chain is not spelled that way.
    ///
    /// Two conditions, and both are needed. The root name must be a spelling of the
    /// very collection the receiver resolved to: a variable can HOLD a collection
    /// (`Catalogs = Документы`), and then the spelling names one kind while the type is
    /// another — taking the plural from the source and the object from the type would
    /// have the adapter check a signature of a different metadata object entirely.
    /// And the name must not be held by a declared owner, which is the same verdict
    /// every other surface asks (`manager_collection_shadowed`): a local named
    /// `Документы` owns its name, so nothing about it is a global collection.
    ///
    /// The two-step form (`М = Справочники.Товары; М.Метод()`) fails the first
    /// condition and gets no verdict — nothing in that call is spelled redundantly,
    /// and there is no plural in it to name.
    fn spelled_out_collection_root(
        &mut self,
        receiver: ExprId,
        resolved_mdo: bsl_metadata::MdoType,
    ) -> Option<Name> {
        let Expr::Field { base: root, .. } = self.body.expr(receiver) else { return None };
        let root_id = ExprId::from_idx(*root);
        let Expr::Path(plural) = self.body.expr(root_id) else { return None };
        let plural = plural.clone();
        if bsl_metadata::MdoType::from_plural(plural.as_str()) != Some(resolved_mdo) {
            return None;
        }
        (!self.manager_collection_shadowed(&plural)).then_some(plural)
    }

    fn report_self_qualified_manager_diagnostic(
        &mut self,
        call_expr: ExprId,
        receiver: ExprId,
        receiver_ty: TypeId,
    ) {
        let Expr::Path(receiver_name) = self.body.expr(receiver) else { return };
        let receiver_key = NormName::intern(receiver_name.as_str());
        if self.var_types.contains_key(&receiver_key) || self.body_declares_binding(receiver_name) {
            return;
        }
        let TypeKind::ObjectManager(facet) = self.db.lookup_type(receiver_ty) else { return };
        let resolver = self.get_resolver();
        if resolver.resolve_module_variable(self.db, receiver_name).is_some() {
            return;
        }
        let Some((owner_kind, owner_name)) =
            crate::this_object::resolve_this_manager_owner(self.db, &resolver)
        else {
            return;
        };
        if facet.mdo == owner_kind
            && facet.name.as_str().fold_lower() == owner_name.as_str().fold_lower()
            && receiver_name.as_str().fold_lower() == owner_name.as_str().fold_lower()
        {
            self.push_inference_diagnostic(InferenceDiagnostic::RedundantAccessToObjectTwoLevel {
                expr: call_expr,
                module: receiver_name.clone(),
            });
        }
    }

    /// The two verdicts a spelled-out manager chain carries: the object may be the
    /// enclosing module's own (`Справочники.Товары.Метод()` inside the manager module
    /// of `Товары` is a detour), and the call may omit required parameters.
    ///
    /// Lowering decided both from three strings, and both need the ownership verdict,
    /// which lowering does not have — that is why they live here now. Whether either
    /// APPLIES is still the adapter's call: it reads the module's metadata and the
    /// callee's signature and returns nothing when they do not fit. Emitting before
    /// the method is resolved keeps that division exactly where it was.
    fn report_spelled_out_chain_diagnostics(
        &mut self,
        call_expr: ExprId,
        receiver: ExprId,
        resolved_mdo: bsl_metadata::MdoType,
        mdo_name: &Name,
        method_name: &Name,
        args: &[ExprId],
    ) {
        let Some(plural) = self.spelled_out_collection_root(receiver, resolved_mdo) else { return };
        self.push_inference_diagnostic(InferenceDiagnostic::RedundantAccessToObjectThreeLevel {
            expr: call_expr,
            mdo_type: plural.clone(),
            mdo_name: mdo_name.clone(),
        });
        let arg_presence: Vec<bool> =
            args.iter().map(|arg_id| !matches!(self.body.expr(*arg_id), Expr::Missing)).collect();
        self.push_inference_diagnostic(InferenceDiagnostic::MissedRequiredParameterManagerModule {
            expr: call_expr,
            callee: method_name.clone(),
            mdo_type: plural,
            mdo_name: mdo_name.clone(),
            args: arg_presence,
        });
    }

    fn infer_call(&mut self, call_expr: ExprId, callee: ExprId, args: &[ExprId]) -> TypeId {
        let callee_expr = self.body.expr(callee);
        if let Expr::Field { base, field } = callee_expr {
            let base_id = ExprId::from_idx(*base);
            let method_name = field.clone();

            let mut receiver_ty = self.infer_expr(base_id);

            if !self.is_unknown(receiver_ty) {
                self.report_self_qualified_manager_diagnostic(callee, base_id, receiver_ty);
            }

            if self.is_unknown(receiver_ty) {
                let base_expr = self.body.expr(base_id).clone();
                if let Expr::Path(path_name) = base_expr {
                    match self.dispatch_bare_ident_field_call(
                        &path_name,
                        &method_name,
                        args,
                        base_id,
                        callee,
                    ) {
                        BareReceiverDispatch::Resolved(return_ty) => {
                            self.expr_types.insert(callee, self.db.unknown());
                            return return_ty;
                        }
                        BareReceiverDispatch::Receiver(ty) => receiver_ty = ty,
                        BareReceiverDispatch::Unhandled => {}
                    }
                }
            }

            for arg in args {
                self.infer_expr(*arg);
            }

            let workspace_receiver_ty =
                crate::this_object::coerce_to_metadata_ref_id(self.db, receiver_ty)
                    .unwrap_or(receiver_ty);
            let workspace_receiver_kind = self.db.lookup_type(workspace_receiver_ty);
            if let TypeKind::CommonModule(facet) = &workspace_receiver_kind {
                let module = hir_def::Name::new(&facet.name);
                if let Some(category) = guarded_module_call(&facet.name, method_name.as_str()) {
                    self.push_inference_diagnostic(InferenceDiagnostic::GuardedCall {
                        expr: callee,
                        category,
                    });
                }
                // A bare module name as receiver (not a variable that holds a module) keeps the
                // self-qualified-access and missed-parameter diagnostics that the name dispatch
                // emits — the handlers filter further (TwoLevel only fires when the called module
                // is the current one). A variable is in `assigned_var_names`, so it is excluded.
                let mut static_receiver = false;
                if let Expr::Path(recv) = self.body.expr(base_id) {
                    let recv = recv.clone();
                    if !self.assigned_var_names.contains(&NormName::intern(recv.as_str()))
                        && !self.body_declares_binding(&recv)
                    {
                        static_receiver = true;
                        let arg_presence: Vec<bool> = args
                            .iter()
                            .map(|arg_id| !matches!(self.body.expr(*arg_id), Expr::Missing))
                            .collect();
                        self.push_inference_diagnostic(
                            InferenceDiagnostic::RedundantAccessToObjectTwoLevel {
                                expr: callee,
                                module: module.clone(),
                            },
                        );
                        self.push_inference_diagnostic(
                            InferenceDiagnostic::MissedRequiredParameterCommonModule {
                                expr: callee,
                                callee: method_name.clone(),
                                module: module.clone(),
                                args: arg_presence,
                            },
                        );
                    }
                }
                let return_ty =
                    self.infer_qualified_call(&module, &method_name, args, callee, static_receiver);
                self.expr_types.insert(callee, self.db.unknown());
                return return_ty;
            }
            // The spelled-out-chain report belongs to the manager receiver, not to
            // whether a user method answers, so it fires before resolution — the
            // same position it held when this was three separate branches.
            if let TypeKind::ObjectManager(facet) = &workspace_receiver_kind {
                self.report_spelled_out_chain_diagnostics(
                    call_expr,
                    base_id,
                    facet.mdo,
                    &hir_def::Name::new(&facet.name),
                    &method_name,
                    args,
                );
            }

            let resolver = self.get_resolver();
            match crate::method_resolution::resolve_user_call(
                self.db,
                receiver_ty,
                &method_name,
                &resolver,
            ) {
                crate::method_resolution::UserCallTarget::Method(hit) => {
                    let crate::method_resolution::UserCall { route, resolution, mdo_name } = hit;
                    if !resolution.is_export {
                        // The object route names the COERCED receiver, the other two
                        // the one as written; the spellings differ only on
                        // `ЭтотОбъект`/`ЭтотМенеджер`.
                        let named_ty = match route {
                            crate::method_resolution::UserCallRoute::ObjectModule => {
                                workspace_receiver_ty
                            }
                            _ => receiver_ty,
                        };
                        let receiver_name = receiver_display_name(self.db, named_ty)
                            .unwrap_or_else(|| mdo_name.clone());
                        self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                            expr: callee,
                            receiver_name,
                            method_name: method_name.clone(),
                            kind: UnresolvedMethodKind::MethodNotExport,
                        });
                    }
                    self.expr_types.insert(callee, self.db.unknown());
                    let return_ty = self.effective_local_return(
                        resolution.method_id,
                        self.documented_return(resolution.method_id, resolution.return_type),
                    );
                    let Ok(candidates) = crate::user_call_candidates::for_resolved_method(
                        self.db,
                        resolution.method_id,
                        return_ty,
                    ) else {
                        return self.db.unknown();
                    };
                    return self.record_candidate_call_arg_binding(callee, args, candidates);
                }
                crate::method_resolution::UserCallTarget::BodyUnread => {
                    // Fall through exactly like a missing user method: the platform
                    // surface is still worth resolving. Only the verdict is barred.
                    self.calls_with_unread_target.insert(callee);
                }
                crate::method_resolution::UserCallTarget::NotUserMethod => {}
            }

            let refine_ctx = crate::method_lookup::RefineCtx {
                db: self.db,
                file_id: self.context_file_id,
                owner: self.owner,
                body: &self.body,
                dispatch_expr_id: callee,
                receiver_expr_id: base_id,
                call_args: args,
            };
            let result = match crate::method_lookup::lookup_method_with_refinement(
                self.db,
                receiver_ty,
                &method_name,
                Some(&refine_ctx),
            ) {
                Some(info) => {
                    self.push_deprecated_platform_member_diagnostic(
                        callee,
                        receiver_ty,
                        &method_name,
                        false,
                    );
                    self.check_member_env(callee, &method_name, info.env, EnvMemberKind::Method);
                    self.check_min_platform_type_member(callee, receiver_ty, &method_name, false);
                    let mut candidates = info.candidates;
                    let arg_idxs: Vec<ExprIdx> = args.iter().map(|arg| arg.to_idx()).collect();
                    self.select_returns_by_type_argument(&mut candidates, &arg_idxs);
                    if self.form_self_receiver_name(base_id).is_some() {
                        if let Some(value_ty) =
                            self.form_attribute_to_value_return(&method_name, args)
                        {
                            for signature in candidates.signatures_mut() {
                                signature.return_ty = value_ty;
                            }
                        }
                    }
                    // Same coercion the user cascade dispatched on: the manager
                    // receiver and the workspace receiver are one value.
                    if let TypeKind::ObjectManager(facet) = &workspace_receiver_kind {
                        if facet.mdo == bsl_metadata::MdoType::Constant {
                            let mdo_name = hir_def::Name::new(&facet.name);
                            self.refine_constant_candidates(
                                &mdo_name,
                                &method_name,
                                &mut candidates,
                            );
                        }
                    }
                    self.record_candidate_call_arg_binding(callee, args, candidates)
                }
                None => {
                    // `ЭтотОбъект.Метод()` in a managed form module. The form type
                    // carries no such platform member, so the module's own methods
                    // are the receiver's remaining surface — resolve and judge them
                    // exactly as the equivalent bare call, and report the miss:
                    // unlike a bare name, a self receiver has nowhere else to look.
                    if let Some(self_name) = self.form_self_receiver_name(base_id) {
                        let result = self
                            .infer_form_self_call(&self_name, &method_name, args, callee)
                            .unwrap_or_else(|| {
                                self.push_inference_diagnostic(
                                    InferenceDiagnostic::UnresolvedMethodCall {
                                        expr: callee,
                                        receiver_name: self_name,
                                        method_name: method_name.clone(),
                                        kind: UnresolvedMethodKind::MethodNotFound,
                                    },
                                );
                                self.db.unknown()
                            });
                        self.expr_types.insert(callee, self.db.unknown());
                        return result;
                    }
                    if self.method_resolves_on_alternate_assignment(
                        base_id,
                        receiver_ty,
                        &method_name,
                    ) || self.branch_guard_rules_out_every_reaching_def(base_id)
                    {
                        // Inference is sequential, so at this use the variable
                        // carries the type of its textually-last assignment even
                        // when that assignment lives in a sibling branch that
                        // cannot reach this use. If any other recorded assignment
                        // type resolves the method, the receiver type is a
                        // cross-branch artefact — stay silent instead of
                        // reporting a false unresolved call.
                        //
                        // The branch guard covers the other half: the definitions
                        // that do reach this use may all be ruled out by the
                        // condition the branch settled, and then nothing about the
                        // receiver is established here either.
                    } else if let Some(receiver_name) = receiver_display_name(self.db, receiver_ty)
                    {
                        self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                            expr: callee,
                            receiver_name,
                            method_name: method_name.clone(),
                            kind: UnresolvedMethodKind::MethodNotFound,
                        });
                    } else if matches!(
                        self.db.lookup_type(receiver_ty),
                        TypeKind::PlatformObject(_)
                    ) {
                        let base_expr = self.body.expr(base_id).clone();
                        if let Expr::Path(receiver_path_name) = base_expr {
                            if matches!(
                                crate::platform_global_lookup::try_resolve_platform_global_member(
                                    self.db,
                                    &receiver_path_name,
                                    &method_name,
                                ),
                                crate::platform_global_lookup::PlatformGlobalLookup::KnownContainerMissingMember
                            ) {
                                self.push_inference_diagnostic(
                                    InferenceDiagnostic::UnresolvedMethodCall {
                                        expr: callee,
                                        receiver_name: receiver_path_name,
                                        method_name: method_name.clone(),
                                        kind: UnresolvedMethodKind::MethodNotFound,
                                    },
                                );
                            }
                        }
                    }
                    self.db.unknown()
                }
            };
            self.expr_types.insert(callee, self.db.unknown());
            return result;
        }

        let bare_callee_name: Option<hir_def::Name> = match callee_expr {
            Expr::Path(n) => Some(n.clone()),
            _ => None,
        };

        // A global common module's exported method is callable unqualified — it extends
        // the global context, so it shadows a same-named platform global function. A
        // same-module method (checked here) still wins, keeping Local → Module → Global-CM
        // → Platform precedence.
        // Judged before the early return below: that return withholds a verdict when
        // some global module body could not be read, because an unknown export may
        // own the name and the platform contract would be the wrong yardstick. This
        // verdict is about who owns the name, not about the argument contract, and a
        // stranger's unreadable body must not silence it.
        if let Some(name) = &bare_callee_name {
            let name = name.clone();
            if let Some(category) = guarded_global_call(name.as_str()) {
                if !self.is_call_name_shadowed(&name) {
                    self.push_inference_diagnostic(InferenceDiagnostic::GuardedCall {
                        expr: callee,
                        category,
                    });
                }
            }
        }

        // Variables never own a call name: a parameter, a `Перем` (in the body or the
        // module) or an implicit local named like a method leaves `Имя(...)` calling the
        // method — the module's own, a global export or the platform's. Checked on
        // 8.3.17 and 8.3.27; BSL has no way to call a variable's value.
        if let Some(name) = &bare_callee_name {
            if !self.bare_module_method_exists(name) {
                if let Some(ret) = self.resolve_bare_global_export(name, args, callee) {
                    return ret;
                }
                // The name is not in the known global surface — but if a visible global
                // common module has a body nobody could read, that surface is not the
                // whole one. The unknown body may export this very name, and a global
                // export shadows the platform global, so falling through to the platform
                // signature would measure the call against a contract that never applied.
                if self.global_surface_partly_unknown {
                    for arg in args {
                        self.infer_expr(*arg);
                    }
                    self.expr_types.insert(callee, self.db.unknown());
                    return self.db.unknown();
                }
            }
        }

        if let Some(name) = &bare_callee_name {
            let name = name.clone();
            if let Some(sigs) = builtin::builtin_functions().get(name.as_str()) {
                debug_assert!(
                    !sigs.is_empty(),
                    "BuiltinFunctions::get must never return an empty overload set"
                );

                if let Some(first) = sigs.first() {
                    self.check_member_env(
                        callee,
                        &name,
                        first.env(),
                        EnvMemberKind::GlobalFunction,
                    );
                }
                // This branch is entered even when the module declares a method of the
                // same name; such a polyfill (a local `СтрЗаменитьПоРегулярномуВыражению`
                // for an older platform) is the very code that must stay silent.
                if (self.min_platform.is_some() || self.compat_mode.is_some())
                    && !self.is_call_name_shadowed(&name)
                {
                    self.check_min_platform(callee, &name, || {
                        crate::min_platform::global_function(name.as_str())
                    });
                    self.check_compat_mode(callee, &name, || {
                        crate::compat_mode::hidden_global(name.as_str())
                    });
                }

                for arg in args {
                    self.infer_expr(*arg);
                }
                let Some(callable) = builtin::builtin_functions().callable_id(name.as_str()) else {
                    return self.db.unknown();
                };
                let Ok(mut candidates) = crate::call_resolution::CallCandidateSet::try_from(
                    sigs.iter()
                        .enumerate()
                        .map(|(ordinal, signature)| {
                            signature.to_call_signature(self.db, callable, ordinal)
                        })
                        .collect::<Vec<_>>(),
                ) else {
                    return self.db.unknown();
                };
                // Weaving `&Вместо`: a bare `ПродолжитьВызов(...)` invokes the original base
                // method, so (a) its result is that method's return type, not the platform
                // global's generic return, and (b) it must pass a valid argument count for that
                // method — the platform global is variadic, so the arity check above never fires
                // and the base-method arity is enforced here instead. Both apply only inside a
                // `&Вместо` interceptor (`proceed_*` are `None` everywhere else → no change).
                if is_proceed_with_call_name(&name) {
                    if let Some(proceed_ty) = self.proceed_return {
                        for candidate in candidates.signatures_mut() {
                            candidate.return_ty = proceed_ty;
                        }
                    }
                }
                let ret = self.record_candidate_call_arg_binding(callee, args, candidates);
                let ret = self.refine_first_argument_return(&name, args, ret);
                if is_proceed_with_call_name(&name) {
                    if let Some((required, total)) = self.proceed_arity {
                        if args.len() < required || args.len() > total {
                            self.push_inference_diagnostic(
                                InferenceDiagnostic::MismatchedArgCount {
                                    call_expr: callee,
                                    required_count: required,
                                    total_count: total,
                                    found: args.len(),
                                },
                            );
                        }
                    }
                }
                self.expr_types.insert(callee, self.db.unknown());
                return ret;
            }
        }

        // The exact EDT catalog contains a handful of current global functions
        // absent from the older HBK signature corpus. They are still known
        // callables; infer arguments and availability, but keep the return type
        // unknown until a signature is available.
        if let Some(name) = &bare_callee_name {
            if !self.bare_module_method_exists(name) {
                if let Some(symbol) =
                    bsl_platform::PlatformGlobalCatalog::instance().lookup(name.as_str())
                {
                    if symbol.kind == bsl_platform::PlatformGlobalKind::Function
                        && symbol.capabilities.callable == Some(true)
                    {
                        let availability = hir_def::execution_env::EnvFlags::from_platform_context(
                            symbol.context.as_ref(),
                        );
                        self.check_member_env(
                            callee,
                            name,
                            availability,
                            EnvMemberKind::GlobalFunction,
                        );
                        self.check_min_platform(callee, name, || {
                            crate::min_platform::global_function(name.as_str())
                        });
                        if self.compat_mode.is_some() && !self.is_call_name_shadowed(name) {
                            self.check_compat_mode(callee, name, || {
                                crate::compat_mode::hidden_global(name.as_str())
                            });
                        }
                        for arg in args {
                            self.infer_expr(*arg);
                        }
                        self.expr_types.insert(callee, self.db.unknown());
                        return self.db.unknown();
                    }
                }
            }
        }

        let callee_ty = self.infer_expr(callee);

        for arg in args {
            self.infer_expr(*arg);
        }

        let callee_kind = self.db.lookup_type(callee_ty);
        if let TypeKind::Function(facet) = callee_kind {
            let candidates = crate::call_resolution::CallCandidateSet::from_function_facet(facet);
            self.record_candidate_call_arg_binding(callee, args, candidates);
            return facet.returns;
        }
        // The callee expression typed above reads the name as a VALUE, so a variable of the
        // name — parameter, `Перем`, implicit local (`Соединение = Соединение(...)` in BSP) —
        // answered it. The call itself looks among methods only.
        if let Some(name) = bare_callee_name.as_ref() {
            if let Some(return_ty) = self.infer_local_method_call(name, args, callee) {
                return return_ty;
            }
            // The verdict recorded for the callee is about a value read. When a variable
            // answered that read it says nothing about the call, which needs the verdict
            // the cascade gives with no variable: a name no method owns does not compile
            // even with a parameter of that name in scope.
            let verdict = if self.variable_holds_name(name) {
                match self.bare_name_miss() {
                    BareNameResolution::Absent => BareNameVerdict::Absent,
                    _ => BareNameVerdict::Indeterminate,
                }
            } else {
                self.bare_name_verdicts
                    .get(&callee)
                    .copied()
                    .unwrap_or(BareNameVerdict::Indeterminate)
            };
            self.report_absent_bare_call(name, callee, verdict);
            if let Some(value_ty) = self.form_attribute_to_value_return(name, args) {
                return value_ty;
            }
        }
        self.db.unknown()
    }

    /// Whether a variable answers `name` as a value: a parameter or `Перем` of the
    /// body, an implicit local written in it, a module-level `Перем`, or an export
    /// variable of an application module.
    fn variable_holds_name(&mut self, name: &hir_def::Name) -> bool {
        self.body_declares_binding(name)
            || self.assigned_var_names.contains(&NormName::intern(name.as_str()))
            || self.get_resolver().resolve_module_variable(self.db, name).is_some()
            || self.global_read_export(name).is_some()
    }

    /// The last stop of the bare-call cascade: `Имя(...)` that no method of this
    /// module, no method of its implicit receiver, no export of a global common or
    /// application module and no platform global owns. BSL refuses to compile the
    /// WHOLE module around such a call, so silence here costs far more than a
    /// squiggle — but only a proven absence earns the verdict.
    ///
    /// Certainty is not decided a second time: `verdict` is the callee's own
    /// bare-name verdict (`classify_bare_name_miss`, recorded by
    /// [`Self::infer_path_name`] while this very call inferred the callee expression),
    /// or, when a variable answered that value read, the same miss classification
    /// asked directly ([`Self::bare_name_miss`]). A miss under ANY [`BareNameGap`] — an
    /// unread user global surface, a missing configuration root, a platform catalog
    /// that is Missing / Unverified / UnsupportedTarget — is `Indeterminate` and says
    /// nothing.
    ///
    /// The bare-name cascade knows nothing about the IMPLICIT receiver, and in an
    /// object, record-set or manager module that receiver owns a whole platform
    /// surface: `ЭтоНовый()`, `Записать()` in an object module, `Загрузить()` in a
    /// record-set module, `ПолучитьМакет()` in a report's object module, `Закрыть()`
    /// in a managed form module are all `ЭтотОбъект.<метод>()` written without the
    /// receiver. So it is cleared here, through the platform catalog the qualified
    /// spelling is judged by, and where the receiver exists but cannot be built the
    /// call keeps its silence.
    fn report_absent_bare_call(
        &mut self,
        name: &hir_def::Name,
        callee: ExprId,
        verdict: BareNameVerdict,
    ) {
        if name.is_missing() || verdict != BareNameVerdict::Absent {
            return;
        }
        if is_undocumented_platform_global(name.as_str()) || self.module_compiles_nowhere() {
            return;
        }
        // The platform compiles a module per environment and drops the branches that
        // environment rules out, so a call no environment of this body compiles cannot
        // fail the module — `#Если Клиент` inside a server-only form procedure is the
        // common shape.
        if self.in_uncompiled_branch {
            return;
        }
        match self.implicit_self_surface() {
            ImplicitSelfSurface::None => {}
            ImplicitSelfSurface::Unreadable => return,
            ImplicitSelfSurface::Receiver(receiver_ty) => {
                if crate::method_lookup::lookup_method_with_refinement(
                    self.db,
                    receiver_ty,
                    name,
                    None,
                )
                .is_some()
                {
                    return;
                }
            }
            ImplicitSelfSurface::PlatformTypes(type_names) => {
                let data = bsl_platform::PlatformData::instance();
                if type_names.iter().any(|ty| data.get_method(ty, name.as_str()).is_some()) {
                    return;
                }
            }
            ImplicitSelfSurface::PlatformPrefix(prefix) => {
                if !bsl_platform::find_prefixed_methods(prefix, name.as_str()).is_empty() {
                    return;
                }
            }
        }
        self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedBareCall {
            expr: callee,
            name: name.clone(),
        });
    }

    /// See [`hir_def::ModuleMetadata::compiles_nowhere`].
    fn module_compiles_nowhere(&self) -> bool {
        let Some(module_id) = self.get_resolver().module_id() else {
            return false;
        };
        self.db.module_metadata(module_id).compiles_nowhere()
    }

    /// What a bare name in this module implicitly hangs off — the value `ЭтотОбъект`
    /// denotes here.
    ///
    /// The match is on the module TYPE and therefore exhaustive on purpose: a new
    /// module kind must be classified deliberately, because getting it wrong in the
    /// `None` direction accuses correct code.
    ///
    /// `Unreadable` is the honest answer wherever a receiver exists but its surface
    /// cannot be produced. An ordinary form lives there: its `Form.bin` dialog is not
    /// read, and its module also sees the exports of the main attribute's object
    /// module, which nothing here enumerates. So does a form whose `Form.xml` failed
    /// to load. A managed form module sees no object module at all — the object
    /// lives on the server behind `РеквизитФормыВЗначение` — so its receiver is the
    /// form and the extension its main attribute mixes in, and nothing more.
    fn implicit_self_surface(&self) -> ImplicitSelfSurface {
        use bsl_metadata::ModuleType;

        let resolver = self.get_resolver();
        let Some(module_id) = resolver.module_id() else {
            return ImplicitSelfSurface::Unreadable;
        };
        let metadata = self.db.module_metadata(module_id);
        match metadata.module_type {
            ModuleType::CommonModule
            | ModuleType::ApplicationModule
            | ModuleType::ManagedApplicationModule
            | ModuleType::OrdinaryApplicationModule
            | ModuleType::SessionModule
            | ModuleType::ExternalConnectionModule
            | ModuleType::CommandModule
            | ModuleType::HTTPServiceModule
            | ModuleType::WebServiceModule
            | ModuleType::IntegrationServiceModule => ImplicitSelfSurface::None,

            ModuleType::ObjectModule => {
                match crate::this_object::resolve_this_object_owner(self.db, &resolver) {
                    Some((mdo_type, name)) => {
                        ImplicitSelfSurface::Receiver(self.db.mk_this_object(
                            ConfigId::Root,
                            MdoRefFacet::new(mdo_type, name.as_str().to_string()),
                        ))
                    }
                    None => ImplicitSelfSurface::Unreadable,
                }
            }

            ModuleType::ManagerModule => {
                match crate::this_object::resolve_this_manager_owner(self.db, &resolver) {
                    Some((mdo_type, name)) => {
                        ImplicitSelfSurface::Receiver(self.db.mk_this_manager(
                            ConfigId::Root,
                            MdoRefFacet::new(mdo_type, name.as_str().to_string()),
                        ))
                    }
                    None => ImplicitSelfSurface::Unreadable,
                }
            }

            ModuleType::RecordSetModule => {
                match crate::this_object::resolve_this_record_set_owner(self.db, &resolver)
                    .and_then(|(mdo_type, name)| {
                        Some((hir_def::ty::MetadataKind::record_set_kind_for(mdo_type)?, name))
                    }) {
                    Some((kind, name)) => ImplicitSelfSurface::Receiver(self.db.metadata_ref(
                        kind,
                        name.as_str().to_string(),
                        &RootConfigCtx,
                    )),
                    None => ImplicitSelfSurface::Unreadable,
                }
            }

            ModuleType::FormModule => match metadata.form.as_deref() {
                Some(form) if form.is_managed() => ImplicitSelfSurface::PlatformTypes(
                    crate::form_self::managed_form_self_method_types(form),
                ),
                _ => ImplicitSelfSurface::Unreadable,
            },

            // Only a constant owns a value-manager module; its receiver is
            // `КонстантаМенеджерЗначения.<Имя>`.
            ModuleType::ValueManagerModule => match metadata.mdo.as_deref() {
                Some(mdo) if mdo.mdo_type == bsl_metadata::MdoType::Constant => {
                    ImplicitSelfSurface::PlatformPrefix(CONSTANT_VALUE_MANAGER_PREFIX)
                }
                _ => ImplicitSelfSurface::Unreadable,
            },

            ModuleType::Unknown => ImplicitSelfSurface::Unreadable,
        }
    }

    /// Resolves a call against the methods of the module being inferred, judges it
    /// against the caller's compilation directive and binds its arguments.
    ///
    /// `None` means the module declares no such method — what that means is the
    /// caller's decision: a bare name may still be a global function or a common
    /// module export, while a self-qualified receiver has no surface left.
    fn infer_local_method_call(
        &mut self,
        name: &hir_def::Name,
        args: &[ExprId],
        callee: ExprId,
    ) -> Option<TypeId> {
        let method = self.find_local_method(name)?;

        // Weaving-base fallbacks resolve into another file whose item tree does
        // not match `context_file_id` — the local directive check only makes
        // sense for true siblings.
        if method.id.module.file_id == self.context_file_id {
            let method_name = method.name.clone();
            let local_id = method.id.local_id;
            self.check_local_callee_env(callee, &method_name, local_id);
        }
        // In effective (`&ИзменениеИКонтроль`) inference, prefer the CHANGED
        // body's return over the base-keyed query, so inserted code that consumes
        // a changed sibling's result types correctly.
        let effective_ret = self
            .local_effective_returns
            .as_ref()
            .and_then(|m| m.get(&method.id.local_id).copied())
            .filter(|ret| !self.is_unknown(*ret));
        let sig =
            crate::method_resolution::materialise_signature_enriched(self.db, method.id, &method);
        let return_ty = effective_ret.unwrap_or_else(|| self.documented_return(method.id, sig.ret));
        let Ok(candidates) =
            crate::user_call_candidates::for_resolved_method(self.db, method.id, return_ty)
        else {
            return Some(self.db.unknown());
        };
        Some(self.record_candidate_call_arg_binding(callee, args, candidates))
    }

    /// `ЭтотОбъект.Метод()` / `ЭтаФорма.Метод()` reaching this form module's own
    /// method: judged exactly as the equivalent bare call, plus the export rule —
    /// only exported methods are members of the form value. A non-exported one
    /// compiles, but 8.3.17 and 8.3.27 both fail the call at run time with «Метод
    /// объекта не обнаружен (Имя)», on the client and on the server alike. The call
    /// still resolves to the declaration, so navigation and argument checks keep
    /// working. `None` means the module declares no such method; whether that is a
    /// miss is the caller's decision.
    fn infer_form_self_call(
        &mut self,
        self_name: &hir_def::Name,
        method_name: &hir_def::Name,
        args: &[ExprId],
        callee: ExprId,
    ) -> Option<TypeId> {
        if self.find_local_method(method_name).is_some_and(|m| !m.is_export) {
            self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                expr: callee,
                receiver_name: self_name.clone(),
                method_name: method_name.clone(),
                kind: UnresolvedMethodKind::SelfMethodNotExport,
            });
        }
        self.infer_local_method_call(method_name, args, callee)
    }

    /// The declaration a call to `name` from this module's own code reaches.
    fn find_local_method(
        &self,
        name: &hir_def::Name,
    ) -> Option<Arc<hir_def::module_interface::MethodDecl>> {
        // Weaving: a `&Вместо`/`&Перед`/`&После` interceptor calling a base
        // sibling that the extension does not define falls back to the paired
        // base module's declarations (the extension shadows the base).
        match &self.local_symbols {
            Some(symbols) => symbols.find_method_shared(NormName::intern(name.as_str())).cloned(),
            None => {
                let module_id = hir_def::ModuleId::new(self.context_file_id);
                self.db.interface_method_named(module_id, name)
            }
        }
        .or_else(|| self.weaving_base.and_then(|base| self.db.interface_method_named(base, name)))
    }

    /// An ordinary symbol with a self name's spelling shadows the predefined
    /// meaning: a parameter, a `Перем` in the body or at module level, an
    /// assignment target, a user method. Typing such a receiver as the module's
    /// own object or form would invent members it does not have and lose every
    /// check that depends on its real type. Mirrors the `user_shadows` /
    /// `body_binding_shadows` rule the rest of the name cascade uses.
    fn self_name_is_shadowed(&self, name: &hir_def::Name, resolver: &Resolver) -> bool {
        self.body_declares_binding(name)
            || self.assigned_var_names.contains(&NormName::intern(name.as_str()))
            || matches!(
                resolver.resolve_name(self.db, name),
                Some(
                    hir_def::resolver::Resolution::Method(_)
                        | hir_def::resolver::Resolution::Variable(_)
                )
            )
    }

    /// The receiver is the form module's own self reference, written literally.
    ///
    /// The test is on the spelling, not on the inferred type: a parameter or
    /// variable that merely holds a `ФормаКлиентскогоПриложения` came from
    /// somewhere else, and resolving it against THIS module's methods would
    /// invent members it does not have.
    fn form_self_receiver_name(&self, base_id: ExprId) -> Option<hir_def::Name> {
        let Expr::Path(name) = self.body.expr(base_id) else { return None };
        let name = name.clone();
        if !is_self_name(&name.as_str().fold_lower()) {
            return None;
        }
        let resolver = self.get_resolver();
        if self.self_name_is_shadowed(&name, &resolver) {
            return None;
        }
        crate::this_object::is_managed_form_module(self.db, &resolver).then_some(name)
    }

    /// `static_receiver` — the call names the module directly
    /// (`Модуль.Метод()`), as opposed to a variable holding a module value
    /// (`М = ОбщегоНазначения.ОбщийМодуль(...); М.Метод()`). Flow-insensitive
    /// typing keeps only the LAST module assigned to such a variable, so
    /// accessibility verdicts against it would misfire on the common
    /// per-`#Если`-branch module selection idiom.
    fn infer_qualified_call(
        &mut self,
        module_name: &Name,
        method_name: &Name,
        args: &[ExprId],
        call_expr: ExprId,
        static_receiver: bool,
    ) -> TypeId {
        for arg in args {
            self.infer_expr(*arg);
        }

        let resolver = self.get_resolver();

        match method_resolution::resolve_qualified_call(
            self.db,
            module_name,
            method_name,
            &resolver,
        ) {
            Ok(resolution) => {
                if !resolution.is_export {
                    self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                        expr: call_expr,
                        receiver_name: module_name.clone(),
                        method_name: method_name.clone(),
                        kind: UnresolvedMethodKind::MethodNotExport,
                    });
                }

                if static_receiver && resolution.is_export {
                    self.check_common_module_callee_env(
                        call_expr,
                        Some(module_name),
                        resolution.method_id.module,
                    );
                }

                // `ОбщегоНазначения.ОбщийМодуль("Имя")` returns the named common module as a
                // value. Its own inferred return is uninformative — `Unknown` from `Вычислить`,
                // or `Undefined` because the real БСП body has an `Иначе Модуль = Неопределено`
                // arm that wins under flow-insensitive typing. Narrowing on either gives the
                // receiver a `CommonModule` type for member resolution / hover / completion,
                // without overriding a method that declares a genuinely useful return type.
                let mut return_ty =
                    self.documented_return(resolution.method_id, resolution.return_type);
                if (self.is_unknown(return_ty) || self.is_undefined(resolution.return_type))
                    && method_name.as_str().fold_lower() == "общиймодуль"
                {
                    if let Some(ty) = self.common_module_type_from_args(args) {
                        return_ty = ty;
                    }
                }
                let Ok(candidates) = crate::user_call_candidates::for_resolved_method(
                    self.db,
                    resolution.method_id,
                    return_ty,
                ) else {
                    return self.db.unknown();
                };
                self.record_candidate_call_arg_binding(call_expr, args, candidates)
            }
            Err(kind) => {
                if matches!(kind, UnresolvedMethodKind::MethodNotFound) {
                    let source_root_id = self
                        .db
                        .file_source_root_input(self.context_file_id)
                        .source_root_id(self.db);
                    let module_in_workspace = self
                        .db
                        .module_index(source_root_id)
                        .resolve_common_module(module_name)
                        .is_some();

                    if !module_in_workspace {
                        if let crate::platform_global_lookup::PlatformGlobalLookup::Resolved {
                            ty: return_ty,
                            env,
                        } = crate::platform_global_lookup::try_resolve_platform_global_member(
                            self.db,
                            module_name,
                            method_name,
                        ) {
                            self.check_member_env(
                                call_expr,
                                method_name,
                                env,
                                EnvMemberKind::Method,
                            );
                            self.check_min_platform(call_expr, method_name, || {
                                crate::min_platform::global_member(
                                    module_name.as_str(),
                                    method_name.as_str(),
                                )
                            });
                            self.expr_types.insert(call_expr, self.db.unknown());
                            return return_ty;
                        }
                    }
                }

                // `BodyUnread` travels on: inference records the outcome it actually
                // reached, and whether an outcome is worth telling the user about is
                // the diagnostics layer's call, not this one's.
                self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                    expr: call_expr,
                    receiver_name: module_name.clone(),
                    method_name: method_name.clone(),
                    kind,
                });

                self.db.unknown()
            }
        }
    }

    /// Type of `ОбщийМодуль("Имя")` when the single argument is a plain (non-dotted) string
    /// literal naming a known common module: `Some(CommonModule{canonical_name})`. The dotted
    /// form (`"Справочники.Имя"`, a manager) and unknown names return `None`.
    fn common_module_type_from_args(&self, args: &[ExprId]) -> Option<TypeId> {
        let [arg] = args else {
            return None;
        };
        let Expr::Literal(Literal::String(literal)) = self.body.expr(*arg) else {
            return None;
        };
        if literal.contains('.') {
            return None;
        }
        let name = Name::new(literal);
        // Only narrow to a module visible from this file's configuration — the same guard the
        // resolver applies, so the type and later member resolution agree on one module rather
        // than typing against a source-root sibling hidden in the current config.
        if !self.get_resolver().user_common_module_exists(self.db, &name) {
            return None;
        }
        let source_root_id =
            self.db.file_source_root_input(self.context_file_id).source_root_id(self.db);
        let module_index = self.db.module_index(source_root_id);
        let canonical = module_index.canonical_common_module_name(&name)?;
        Some(self.db.common_module(canonical.to_string(), ConfigId::Root))
    }

    fn dispatch_bare_ident_field_call(
        &mut self,
        module_name: &Name,
        method_name: &Name,
        args: &[ExprId],
        root_expr: ExprId,
        call_expr: ExprId,
    ) -> BareReceiverDispatch {
        use hir_def::resolver::Resolution;

        let resolver = self.get_resolver();

        match resolver.resolve_name(self.db, module_name) {
            Some(Resolution::Local(_) | Resolution::Variable(_) | Resolution::Method(_)) => {
                return BareReceiverDispatch::Unhandled;
            }
            Some(Resolution::Builtin(_)) | None => {}
        }

        if self.body_declares_binding(module_name) {
            return BareReceiverDispatch::Unhandled;
        }
        if self.assigned_var_names.contains(&NormName::intern(module_name.as_str())) {
            return BareReceiverDispatch::Unhandled;
        }

        // `ЭтотОбъект.Метод()` / `ЭтаФорма.Метод()` in a form module whose form metadata is
        // not a readable managed form (binary `Form.bin`, or no form metadata at all) is a
        // self-reference to the module, not a module named `ЭтотОбъект`/`ЭтаФорма`. A user
        // method of this module is judged as in a managed form; any other name is a platform
        // form member we cannot enumerate, so stay silent instead of reporting the receiver
        // as an unresolved module.
        if is_self_name(&module_name.as_str().fold_lower())
            && resolver.module_id().is_some_and(|module_id| {
                self.db.module_metadata(module_id).module_type
                    == bsl_metadata::ModuleType::FormModule
            })
        {
            for arg in args {
                self.infer_expr(*arg);
            }
            let ret = self
                .infer_form_self_call(module_name, method_name, args, call_expr)
                .unwrap_or_else(|| self.db.unknown());
            return BareReceiverDispatch::Resolved(ret);
        }

        // A form attribute shadows module and global names for a bare receiver.
        // A typed attribute never reaches this dispatch (its receiver infers to
        // a real type); an untyped one (empty <Type/> in Form.xml) lowers to
        // Unknown and lands here — it is still a form attribute, not an
        // unresolved module, so stay silent per gradual typing.
        if crate::form_attr::resolve_form_attribute(self.db, &resolver, module_name).is_some() {
            for arg in args {
                self.infer_expr(*arg);
            }
            return BareReceiverDispatch::Resolved(self.db.unknown());
        }

        if resolver.user_common_module_exists(self.db, module_name) {
            let arg_presence: Vec<bool> = args
                .iter()
                .map(|arg_id| !matches!(self.body.expr(*arg_id), Expr::Missing))
                .collect();

            self.push_inference_diagnostic(InferenceDiagnostic::RedundantAccessToObjectTwoLevel {
                expr: call_expr,
                module: module_name.clone(),
            });

            self.push_inference_diagnostic(
                InferenceDiagnostic::MissedRequiredParameterCommonModule {
                    expr: call_expr,
                    callee: method_name.clone(),
                    module: module_name.clone(),
                    args: arg_presence,
                },
            );

            return BareReceiverDispatch::Resolved(self.infer_qualified_call(
                module_name,
                method_name,
                args,
                call_expr,
                true,
            ));
        }

        // A manager module that calls one of its own methods through the object's
        // own name (`ОбъектМетаданных.Метод()`) is a redundant self-qualified
        // access — the method is reachable directly. The name denotes this
        // module's own manager, so the call itself is an ordinary manager-member
        // call: hand the receiver type back and let the one route judge it.
        if let Some((mdo_type, self_name)) =
            crate::this_object::resolve_this_manager_owner(self.db, &resolver)
        {
            if module_name.as_str().fold_lower() == self_name.as_str().fold_lower() {
                self.push_inference_diagnostic(
                    InferenceDiagnostic::RedundantAccessToObjectTwoLevel {
                        expr: call_expr,
                        module: module_name.clone(),
                    },
                );
                // The same type the collection-qualified chain produces for its
                // middle segment (`promote_collection_member`), so both spellings
                // resolve through one member lookup rather than two.
                return BareReceiverDispatch::Receiver(self.db.object_manager(
                    mdo_type,
                    self_name.as_str().to_string(),
                    &RootConfigCtx,
                ));
            }
        }

        match crate::platform_global_lookup::try_resolve_platform_global_member(
            self.db,
            module_name,
            method_name,
        ) {
            crate::platform_global_lookup::PlatformGlobalLookup::Resolved {
                ty: return_ty,
                env,
            } => {
                for arg in args {
                    self.infer_expr(*arg);
                }
                self.check_member_env(call_expr, method_name, env, EnvMemberKind::Method);
                self.check_min_platform(call_expr, method_name, || {
                    crate::min_platform::global_member(module_name.as_str(), method_name.as_str())
                });
                self.expr_types.insert(call_expr, self.db.unknown());
                return BareReceiverDispatch::Resolved(return_ty);
            }
            crate::platform_global_lookup::PlatformGlobalLookup::KnownContainerMissingMember => {
                for arg in args {
                    self.infer_expr(*arg);
                }
                self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                    expr: call_expr,
                    receiver_name: module_name.clone(),
                    method_name: method_name.clone(),
                    kind: UnresolvedMethodKind::MethodNotFound,
                });
                return BareReceiverDispatch::Resolved(self.db.unknown());
            }
            crate::platform_global_lookup::PlatformGlobalLookup::NotAContainer => {}
        }

        for arg in args {
            self.infer_expr(*arg);
        }
        let kind = if self.bare_name_verdicts.get(&root_expr) == Some(&BareNameVerdict::Absent) {
            UnresolvedMethodKind::ReceiverNameAbsent
        } else {
            UnresolvedMethodKind::ReceiverNotResolved
        };
        // Nothing typed the receiver, so no library module can be recognised by type
        // here. A security hotspot that goes quiet in that state is worse than one
        // judged by name, so the owner is matched by spelling here and only here.
        // Both unresolved verdicts qualify, and so does a receiver left silent below as
        // a possible form member: neither is evidence that the call is something else.
        if let Some(category) = guarded_module_call(module_name.as_str(), method_name.as_str()) {
            self.push_inference_diagnostic(InferenceDiagnostic::GuardedCall {
                expr: call_expr,
                category,
            });
        }
        // In a form module whose form metadata is not a readable managed form (binary
        // `Form.bin`, or no form metadata at all) the attribute list is unknown, so a bare
        // receiver that resolved to nothing above is most likely a form attribute, element
        // or the main object (`Объект`). Stay silent (untyped) instead of reporting it as an
        // unresolved module. A managed form with readable metadata keeps the precise check.
        let form_surface_unknown = resolver.module_id().is_some_and(|module_id| {
            let metadata = self.db.module_metadata(module_id);
            metadata.module_type == bsl_metadata::ModuleType::FormModule
                && !metadata.form.as_ref().is_some_and(|form| form.is_managed())
        });
        if !form_surface_unknown {
            self.push_inference_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                expr: call_expr,
                receiver_name: module_name.clone(),
                method_name: method_name.clone(),
                kind,
            });
        }
        BareReceiverDispatch::Resolved(self.db.unknown())
    }

    fn resolve_constant_value_type(&self, mdo_name: &Name) -> Option<TypeId> {
        let mdo = self.db.resolve_metadata_object(
            self.context_file_id,
            bsl_metadata::MdoType::Constant,
            mdo_name.as_str(),
        )?;
        mdo.constant_type.as_ref().map(|attr| {
            let type_ref = hir_def::TypeRef::from_attribute_type(attr);
            TyLoweringContext::new().lower_type_ref_id(self.db, &type_ref)
        })
    }

    fn refine_constant_method(
        &self,
        mdo_name: &Name,
        method_name: &Name,
        return_ty: &mut TypeId,
        params: &mut [TypeId],
    ) {
        let lc = method_name.as_str().fold_lower();
        let is_get = lc == "получить" || lc == "get";
        let is_set = lc == "установить" || lc == "set";
        if !is_get && !is_set {
            return;
        }
        // The platform documents Получить/Установить with «Произвольный» — a
        // placeholder the constant's metadata makes precise, so both wildcard
        // lowerings (Unknown and the sticky Any) are refinable here.
        let is_wildcard = |id: TypeId| id == self.db.unknown() || id == self.db.any();
        let needs_override = (is_get && is_wildcard(*return_ty))
            || (is_set && params.first().is_some_and(|id| is_wildcard(*id)));
        if !needs_override {
            return;
        }
        let Some(value_ty) = self.resolve_constant_value_type(mdo_name) else {
            return;
        };
        if is_get {
            *return_ty = value_ty;
        } else if is_set {
            params[0] = value_ty;
        }
    }

    fn refine_constant_candidates(
        &self,
        mdo_name: &Name,
        method_name: &Name,
        candidates: &mut crate::call_resolution::CallCandidateSet,
    ) {
        for candidate in candidates.signatures_mut() {
            let mut params = candidate.params.iter().map(|param| param.ty).collect::<Vec<_>>();
            self.refine_constant_method(
                mdo_name,
                method_name,
                &mut candidate.return_ty,
                &mut params,
            );
            for (param, ty) in candidate.params.iter_mut().zip(params) {
                param.ty = ty;
            }
        }
    }
}

fn const_eval_literal_index(expr: &Expr) -> Option<usize> {
    let Expr::Literal(Literal::Number(n)) = expr else {
        return None;
    };
    let f = n.into_inner();
    if !f.is_finite() || f < 0.0 || f.fract() != 0.0 {
        return None;
    }
    Some(f as usize)
}

/// Spellings that denote the module's own object or form, both languages.
fn is_self_name(name_lower: &str) -> bool {
    matches!(name_lower, "этотобъект" | "thisobject" | "этаформа" | "thisform")
}

/// Что дал разбор голого имени в позиции получателя вызова.
enum BareReceiverDispatch {
    /// Имя не принадлежит ни одной поверхности разбора: получатель остаётся
    /// неизвестным, вызов разбирается дальше как есть.
    Unhandled,
    /// Вызов разобран здесь целиком, вместе со своими диагностиками.
    Resolved(TypeId),
    /// Имя обозначает тип, и вызов на нём — обычный вызов члена: судит его
    /// общий маршрут, а не отдельная ветка.
    Receiver(TypeId),
}

fn receiver_display_name(db: &dyn TypeKernelDb, receiver_ty: TypeId) -> Option<hir_def::Name> {
    match db.lookup_type(receiver_ty) {
        TypeKind::MetadataRef(facet) => {
            let plural = mdo_kind_to_receiver_label(facet.kind)?;
            Some(hir_def::Name::new(&format!("{}.{}", plural, facet.name.as_str())))
        }
        TypeKind::ThisObject { owner, .. } | TypeKind::ThisManager { owner, .. } => {
            let plural = mdo_receiver_label(owner.mdo_type)?;
            Some(hir_def::Name::new(&format!("{}.{}", plural, owner.name.as_str())))
        }
        TypeKind::ObjectManager(facet) => {
            let plural = mdo_receiver_label(facet.mdo)?;
            Some(hir_def::Name::new(&format!("{}.{}", plural, facet.name.as_str())))
        }
        TypeKind::FormData { kind: FormDataFacet::Collection, underlying: Some(underlying) } => {
            let plural = mdo_receiver_label(underlying.mdo_type)?;
            let name = &underlying.name;
            Some(hir_def::Name::new(&format!("{}.{}", plural, name.as_str())))
        }
        _ => None,
    }
}

/// How a diagnostic names an object of this kind before the dot: the manager
/// collection where there is one, the bare platform type where there is none.
/// An external object has no collection, and a receiver without a label would
/// silence the diagnostic rather than degrade its wording.
fn mdo_receiver_label(mdo: bsl_metadata::MdoType) -> Option<&'static str> {
    mdo.russian_plural().or_else(|| {
        hir_def::ty::MetadataKind::object_kind_for(mdo)?
            .bare_platform_type_names()
            .map(|(russian, _)| russian)
    })
}

fn mdo_kind_to_receiver_label(kind: hir_def::ty::MetadataKind) -> Option<&'static str> {
    use hir_def::ty::MetadataKind;
    let mdo = match kind {
        MetadataKind::CatalogObject | MetadataKind::CatalogRef => bsl_metadata::MdoType::Catalog,
        MetadataKind::DocumentObject | MetadataKind::DocumentRef => bsl_metadata::MdoType::Document,
        MetadataKind::EnumRef => bsl_metadata::MdoType::Enum,
        MetadataKind::TaskRef | MetadataKind::TaskObject => bsl_metadata::MdoType::Task,
        MetadataKind::BusinessProcessRef | MetadataKind::BusinessProcessObject => {
            bsl_metadata::MdoType::BusinessProcess
        }
        MetadataKind::DataProcessorObject => bsl_metadata::MdoType::DataProcessor,
        MetadataKind::ReportObject => bsl_metadata::MdoType::Report,
        MetadataKind::ExternalDataProcessorObject => bsl_metadata::MdoType::ExternalDataProcessor,
        MetadataKind::ExternalReportObject => bsl_metadata::MdoType::ExternalReport,
        MetadataKind::ExchangePlanRef | MetadataKind::ExchangePlanObject => {
            bsl_metadata::MdoType::ExchangePlan
        }
        MetadataKind::ChartOfAccountsRef | MetadataKind::ChartOfAccountsObject => {
            bsl_metadata::MdoType::ChartOfAccounts
        }
        MetadataKind::ChartOfCharacteristicTypesRef
        | MetadataKind::ChartOfCharacteristicTypesObject => {
            bsl_metadata::MdoType::ChartOfCharacteristicTypes
        }
        MetadataKind::ChartOfCalculationTypesRef | MetadataKind::ChartOfCalculationTypesObject => {
            bsl_metadata::MdoType::ChartOfCalculationTypes
        }
        MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecordSet => bsl_metadata::MdoType::InformationRegister,
        MetadataKind::AccumulationRegisterRecordSet => bsl_metadata::MdoType::AccumulationRegister,
        MetadataKind::AccountingRegisterRecordSet => bsl_metadata::MdoType::AccountingRegister,
        MetadataKind::CalculationRegisterRecordSet => bsl_metadata::MdoType::CalculationRegister,
        MetadataKind::InformationRegisterRef
        | MetadataKind::AccumulationRegisterRef
        | MetadataKind::AccountingRegisterRef
        | MetadataKind::CalculationRegisterRef
        | MetadataKind::InformationRegisterRecord
        | MetadataKind::AccumulationRegisterRecord
        | MetadataKind::AccountingRegisterRecord
        | MetadataKind::CalculationRegisterRecord => return None,
        MetadataKind::TabularSection { parent } | MetadataKind::TabularSectionRow { parent } => {
            parent
        }
        MetadataKind::RegisterDimension { .. }
        | MetadataKind::RegisterResource { .. }
        | MetadataKind::RegisterAttribute { .. }
        | MetadataKind::RegisterFilter { .. } => return None,
    };
    mdo_receiver_label(mdo)
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::inference_result_heap, returns(clone))]
pub fn infer_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<InferenceResult> {
    let file_id = file_id_input.file_id(db);
    let _p = tracing::info_span!("infer_query", ?file_id).entered();

    let module_id = hir_def::ModuleId { file_id };
    let module_bodies = db.module_bodies_ref(module_id);

    let mut result = InferenceResult::default();

    let fold_body = |result: &mut InferenceResult, body_result: &BodyInferenceResult| {
        let owner = body_result.owner;
        result.expr_types_by_body.insert(owner, body_result.expr_types.clone());
        result.binding_types_by_body.insert(owner, body_result.binding_types.clone());
        result.var_types.extend(body_result.var_types.iter().map(|(k, v)| (k.clone(), *v)));
        result.implicit_locals_by_body.insert(owner, body_result.implicit_locals.clone());
        result.diagnostics.extend(body_result.diagnostics.iter().map(|d| (owner, d.clone())));
        result.call_arg_bindings.extend(body_result.call_arg_bindings.iter().cloned());
    };

    let fold_module_code = |result: &mut InferenceResult,
                            module_code: &ModuleCodeInferenceResult| {
        let owner = module_code.owner;
        result.expr_types_by_body.insert(owner, module_code.expr_types.clone());
        result.binding_types_by_body.insert(owner, module_code.binding_types.clone());
        result.var_types.extend(module_code.var_types.iter().map(|(k, v)| (k.clone(), *v)));
        result.implicit_locals_by_body.insert(owner, module_code.implicit_locals.clone());
        result.diagnostics.extend(module_code.diagnostics.iter().map(|d| (owner, d.clone())));
        result.call_arg_bindings.extend(module_code.call_arg_bindings.iter().cloned());
    };

    {
        let _bspan = tracing::info_span!("infer_query.body", kind = "module_code").entered();
        let module_code = db.infer_module_code_ref(file_id);
        fold_module_code(&mut result, module_code);
    }

    for (local_id, _body) in module_bodies.iter_bodies() {
        let _bspan = tracing::info_span!("infer_query.body", kind = "method").entered();
        let method_id = hir_def::MethodId { module: module_id, local_id };
        let method_input = MethodIdInput::new(db, method_id);
        let body_result = db.infer_method_ref(method_input);
        fold_body(&mut result, body_result);
    }

    info!(
        "type inference complete: {} bodies, {} var types, {} diagnostics",
        result.expr_types_by_body.len(),
        result.var_types.len(),
        result.diagnostics.len()
    );

    Arc::new(result)
}

/// Inference over the *effective* module of an `&ИзменениеИКонтроль` extension/base
/// pair. Mirrors [`infer_query`] but runs every body inline through
/// [`InferenceContext::new_effective`] — same-module method/variable lookups resolve
/// against the effective symbol tree (so `#Вставка` code sees base siblings), while
/// metadata / cross-module context stays the base file. It deliberately does NOT
/// reuse `infer_method` / `infer_module_code`: those key on `ModuleId{base_file}` and
/// would collide with the base module's own cached inference.
#[salsa::tracked(lru = 256, heap_size = heap_estimate::inference_result_heap, returns(clone))]
pub fn infer_effective<'db>(
    db: &'db dyn HirDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<InferenceResult> {
    let base_file = eid.base_file(db);
    let _p =
        tracing::info_span!("infer_effective", ?base_file, ext_file = ?eid.ext_file(db)).entered();

    let module_bodies = hir_def::effective_module::module_bodies_effective(db, eid);
    let interface = hir_def::effective_module::module_interface_effective(db, eid);

    // Only the `&ИзменениеИКонтроль` methods actually differ from the base module; every other
    // body is copied verbatim. A copied body's diagnostics are dropped by the orchestrator's
    // `#Вставка` remap, and its effective return equals the base return (identical body +
    // base-keyed sibling resolution), so `effective_returns` would hold exactly the value the
    // base fallback already yields for it. Restricting both passes to the changed methods is
    // therefore output-identical, while avoiding a re-inference of the whole — often large —
    // base module twice per extension file (the dominant cost on heavily-extended configs).
    let changed_ids: rustc_hash::FxHashSet<hir_def::MethodKey> = {
        let ext_parse = db.parse(eid.ext_file(db));
        let changed_targets: rustc_hash::FxHashSet<String> = ext_parse
            .syntax_node()
            .children()
            .filter(|n| {
                matches!(
                    n.kind(),
                    syntax::SyntaxKind::PROCEDURE_DEF | syntax::SyntaxKind::FUNCTION_DEF
                )
            })
            .filter_map(|m| hir_def::extension_merge::extract_change_and_validate(&m))
            .map(|cc| cc.target.fold_lower())
            .collect();
        interface
            .methods()
            .filter(|m| changed_targets.contains(&m.name.as_str().fold_lower()))
            .map(|m| m.id.local_id)
            .collect()
    };

    // Pass 1: infer each CHANGED method body once to capture its EFFECTIVE return type. A bare
    // same-module call to a `&ИзменениеИКонтроль` target must see its changed body's
    // return, not the base body's. Module code has no return, so only methods contribute.
    // This resolves one level of method→method return dependency; deeper chains keep the
    // base inference's same pragmatic bound (no panic, no infinite loop).
    let mut effective_returns: FxHashMap<hir_def::MethodKey, TypeId> = FxHashMap::default();
    for (local_id, body) in module_bodies.iter_bodies() {
        if !changed_ids.contains(&local_id) {
            continue;
        }
        let mut ctx = InferenceContext::new_effective(
            db,
            base_file,
            interface.clone(),
            DefWithBodyId::Method(local_id),
            &Arc::new(body.clone()),
        );
        ctx.infer_all();
        let ret = body_return_type(db, &ctx.finish());
        if ret != db.unknown() {
            effective_returns.insert(local_id, ret);
        }
    }
    let effective_returns = Arc::new(effective_returns);

    // Pass 2: re-infer every body with the effective returns threaded in; these results are
    // authoritative (a changed method's call-expr types and diagnostics are now correct).
    let mut result = InferenceResult::default();

    let fold_body = |result: &mut InferenceResult, body_result: &BodyInferenceResult| {
        let owner = body_result.owner;
        result.expr_types_by_body.insert(owner, body_result.expr_types.clone());
        result.binding_types_by_body.insert(owner, body_result.binding_types.clone());
        result.var_types.extend(body_result.var_types.iter().map(|(k, v)| (k.clone(), *v)));
        result.implicit_locals_by_body.insert(owner, body_result.implicit_locals.clone());
        result.diagnostics.extend(body_result.diagnostics.iter().map(|d| (owner, d.clone())));
        result.call_arg_bindings.extend(body_result.call_arg_bindings.iter().cloned());
    };

    if let Some(body) = module_bodies.module_code() {
        let mut ctx = InferenceContext::new_effective(
            db,
            base_file,
            interface.clone(),
            DefWithBodyId::ModuleCode,
            &Arc::new(body.clone()),
        );
        ctx.set_local_effective_returns(Arc::clone(&effective_returns));
        ctx.infer_all();
        fold_body(&mut result, &ctx.finish());
    }

    for (local_id, body) in module_bodies.iter_bodies() {
        if !changed_ids.contains(&local_id) {
            continue;
        }
        let mut ctx = InferenceContext::new_effective(
            db,
            base_file,
            interface.clone(),
            DefWithBodyId::Method(local_id),
            &Arc::new(body.clone()),
        );
        ctx.set_local_effective_returns(Arc::clone(&effective_returns));
        ctx.infer_all();
        fold_body(&mut result, &ctx.finish());
    }

    Arc::new(result)
}

/// Whether a bare call name is the platform `ПродолжитьВызов` / `ProceedWithCall` global, the
/// call that re-enters the original base method from a `&Вместо` interceptor. Mirrors the
/// recognition in `hir_def::body::lower::expr` (which is private there), folding the name to
/// lower so the bilingual case-insensitive spellings match.
fn is_proceed_with_call_name(name: &Name) -> bool {
    matches!(name.as_str().fold_lower().as_str(), "продолжитьвызов" | "proceedwithcall")
}

/// Categories whose verdict is drawn here rather than in lowering, because it
/// depends on who owns the name. The two surfaces keep separate lists: a
/// category is admitted to a surface only together with the projection that
/// reports it and with its own measure on a real configuration, and the lists
/// differ today — the file-system category has been measured on bare calls
/// alone, so a future library method of that category cannot slip onto the
/// qualified path by inheriting one shared list.
const QUALIFIED_GUARDED_CATEGORIES: &[SecurityCategory] = &[SecurityCategory::ExternalApp];
const BARE_GUARDED_CATEGORIES: &[SecurityCategory] =
    &[SecurityCategory::ExternalApp, SecurityCategory::FileSystem];

fn guarded_category(
    entry: &bsl_platform::security::SecurityEntry,
    admitted: &[SecurityCategory],
) -> Option<SecurityCategory> {
    admitted.contains(&entry.category).then_some(entry.category)
}

/// A method of a library common module, matched only under a declared owner.
fn guarded_module_call(receiver: &str, method: &str) -> Option<SecurityCategory> {
    bsl_platform::security::registry()
        .lookup_module_method(receiver, method)
        .and_then(|entry| guarded_category(entry, QUALIFIED_GUARDED_CATEGORIES))
}

/// A global-context method. Reachable only as a bare call: a qualified one names
/// something else that shares the spelling.
fn guarded_global_call(name: &str) -> Option<SecurityCategory> {
    bsl_platform::security::registry()
        .lookup_global(name)
        .and_then(|entry| guarded_category(entry, BARE_GUARDED_CATEGORIES))
}

/// Inference over an extension module's OWN bodies under *weaving* (`&Вместо` /
/// `&Перед` / `&После`). Unlike [`infer_effective`] there is no text splice: the
/// extension module keeps its native text and is inferred through
/// [`InferenceContext::new_weaving`], so a bare same-module call that targets a base
/// method resolves via the base fallback (no spurious `UnresolvedMethodCall`) while
/// configuration / metadata context stays the extension file. Single pass — the
/// changed-return threading of effective inference is a later increment.
#[salsa::tracked(lru = 256, heap_size = heap_estimate::inference_result_heap, returns(clone))]
pub fn infer_weaving<'db>(
    db: &'db dyn HirDatabase,
    wid: hir_def::weaving::WeavingModuleId<'db>,
) -> Arc<InferenceResult> {
    let ext_file = wid.ext_file(db);
    let base_file = wid.base_file(db);
    let _p = tracing::info_span!("infer_weaving", ?ext_file, ?base_file).entered();

    let base_module = hir_def::ModuleId::new(base_file);
    let ext_module = hir_def::ModuleId::new(ext_file);
    let module_bodies = db.module_bodies_ref(ext_module);

    // A `&Вместо("M")` interceptor's body may call `ПродолжитьВызов(...)` to re-enter the
    // original base method `M`. Pre-compute, per ext method `local_id`: (a) `M`'s return type so
    // the call types as `M`'s result, and (b) `M`'s arity `(required, total)` so the call is
    // validated as a call to `M`. `&Перед`/`&После` carry no `ПродолжитьВызов`, so they are
    // skipped. The return is dropped when uninformative (Unknown/Undefined) to preserve the
    // platform default, but the arity is kept whenever `M` resolves — a procedure base has no
    // informative return yet its arguments still need checking.
    let ext_symbols = db.symbol_tree_ref(ext_module);
    let base_symbols = db.symbol_tree_ref(base_module);
    let ext_parse = db.parse(ext_file);
    let mut proceed_returns: FxHashMap<hir_def::MethodKey, TypeId> = FxHashMap::default();
    let mut proceed_arities: FxHashMap<hir_def::MethodKey, (usize, usize)> = FxHashMap::default();
    for method in ext_symbols.methods() {
        let Some(node) = method.syntax_node(&ext_parse) else {
            continue;
        };
        let Some(interception) = hir_def::weaving::interceptor_target(&node) else {
            continue;
        };
        if interception.kind != hir_def::weaving::InterceptionKind::Around {
            continue;
        }
        let Some(base_method) = base_symbols.find_method(&Name::new(&interception.target)) else {
            continue;
        };
        // `required` mirrors `FunctionSignature::required_count`: one past the last
        // non-defaulted parameter (defaults are trailing in well-formed BSL).
        let total = base_method.params.len();
        let required = base_method.params.iter().rposition(|p| !p.has_default).map_or(0, |i| i + 1);
        proceed_arities.insert(method.id.local_id, (required, total));

        let base_input = hir_def::MethodIdInput::new(db, base_method.id);
        let ret = crate::method_graph::method_return_type_query(db, base_input);
        if ret != db.unknown() && ret != db.undefined() {
            proceed_returns.insert(method.id.local_id, ret);
        }
    }

    let mut result = InferenceResult::default();

    let fold_body = |result: &mut InferenceResult, body_result: &BodyInferenceResult| {
        let owner = body_result.owner;
        result.expr_types_by_body.insert(owner, body_result.expr_types.clone());
        result.binding_types_by_body.insert(owner, body_result.binding_types.clone());
        result.var_types.extend(body_result.var_types.iter().map(|(k, v)| (k.clone(), *v)));
        result.implicit_locals_by_body.insert(owner, body_result.implicit_locals.clone());
        result.diagnostics.extend(body_result.diagnostics.iter().map(|d| (owner, d.clone())));
        result.call_arg_bindings.extend(body_result.call_arg_bindings.iter().cloned());
    };

    if let Some(body) = module_bodies.module_code() {
        let mut ctx = InferenceContext::new_weaving(
            db,
            ext_file,
            base_module,
            DefWithBodyId::ModuleCode,
            &Arc::new(body.clone()),
        );
        ctx.infer_all();
        fold_body(&mut result, &ctx.finish());
    }

    for (local_id, body) in module_bodies.iter_bodies() {
        let mut ctx = InferenceContext::new_weaving(
            db,
            ext_file,
            base_module,
            DefWithBodyId::Method(local_id),
            &Arc::new(body.clone()),
        );
        if let Some(ret) = proceed_returns.get(&local_id) {
            ctx.set_proceed_return(*ret);
        }
        if let Some(&(required, total)) = proceed_arities.get(&local_id) {
            ctx.set_proceed_arity(required, total);
        }
        ctx.infer_all();
        fold_body(&mut result, &ctx.finish());
    }

    Arc::new(result)
}

/// A method body's inferred return type: the union of its non-`Unknown` return-expression
/// types (mirrors `method_graph::method_return_type_query`, reused here so effective
/// inference can capture a changed method's return without the base-keyed query).
fn body_return_type(db: &dyn HirDatabase, body_result: &BodyInferenceResult) -> TypeId {
    let unknown = db.unknown();
    let tys: Vec<TypeId> = body_result
        .return_expr_ids
        .iter()
        .filter_map(|id| body_result.expr_types.get(id).copied())
        .filter(|t| *t != unknown)
        .collect();
    if tys.is_empty() {
        unknown
    } else {
        db.union(tys)
    }
}

#[salsa::tracked(lru = 1024, heap_size = heap_estimate::module_code_inference_result_heap, returns(ref))]
pub fn infer_module_code_query<'db>(
    db: &'db dyn HirDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<ModuleCodeInferenceResult> {
    let file_id = file_id_input.file_id(db);
    let _p = tracing::info_span!("infer_module_code_query", ?file_id).entered();

    let module_id = hir_def::ModuleId { file_id };
    let module_bodies = db.module_bodies_ref(module_id);

    let Some(body) = module_bodies.module_code() else {
        return Arc::new(ModuleCodeInferenceResult::default());
    };

    let mut ctx =
        InferenceContext::new(db, file_id, DefWithBodyId::ModuleCode, &Arc::new(body.clone()));
    ctx.infer_all();
    let body_result = ctx.finish();
    Arc::new(ModuleCodeInferenceResult::from_body(body_result))
}

pub fn type_of_expr_query(
    db: &dyn HirDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    expr: ExprId,
) -> TypeId {
    infer_owner(db, file_id, owner).type_id_of_expr(expr).unwrap_or_else(|| db.unknown())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::testing::InMemoryDb;

    #[test]
    fn t12_function_facet_synthesizes_candidate_with_argument_diagnostics() {
        use bsl_types::facet::{ArgArity, FunctionFacet, FunctionOrigin, ParamPassing, ParamSpec};

        let db = InMemoryDb::new();
        let number = db.number(None, None);
        let string = db.string(None, false);
        let facet = FunctionFacet::new(
            Arc::from([ParamSpec::new("value".to_string(), number, ParamPassing::ByVal, false)]),
            Arc::from([None]),
            1,
            ArgArity::Fixed(1),
            number,
            FunctionOrigin::Unknown,
        );

        let candidates = crate::call_resolution::CallCandidateSet::from_function_facet(&facet);
        let resolution = crate::call_resolution::resolve_candidates(&db, &candidates, &[string]);

        assert!(matches!(
            resolution.selection,
            crate::call_resolution::CallSelection::Rejected(
                crate::call_resolution::CallRejection::Type
            )
        ));
        assert_eq!(resolution.return_ty, db.unknown());
        assert_eq!(facet.returns, number);
        assert_eq!(candidates.as_slice()[0].params[0].ty, number);
    }

    #[test]
    fn test_inference_result_default() {
        let result = InferenceResult::default();
        assert_eq!(result.expr_types_by_body.len(), 0);
        assert_eq!(result.diagnostics.len(), 0);
        assert!(!result.has_diagnostics());
    }

    #[test]
    fn a_bare_name_miss_requires_complete_surfaces() {
        assert_eq!(
            classify_bare_name_miss(Vec::new(), bsl_platform::PlatformCatalogStatus::Complete),
            BareNameResolution::Absent
        );
        assert_eq!(
            classify_bare_name_miss(Vec::new(), bsl_platform::PlatformCatalogStatus::Unverified),
            BareNameResolution::Indeterminate {
                gaps: vec![BareNameGap::UnverifiedPlatformCatalog]
            }
        );
        assert_eq!(
            classify_bare_name_miss(
                Vec::new(),
                bsl_platform::PlatformCatalogStatus::UnsupportedTarget,
            ),
            BareNameResolution::Indeterminate {
                gaps: vec![BareNameGap::UnsupportedTargetPlatform]
            }
        );
        assert_eq!(
            classify_bare_name_miss(
                vec![BareNameGap::UnreadUserGlobalSurface],
                bsl_platform::PlatformCatalogStatus::Complete,
            ),
            BareNameResolution::Indeterminate { gaps: vec![BareNameGap::UnreadUserGlobalSurface] }
        );
    }

    #[test]
    fn test_mismatched_arg_count_diagnostic() {
        let expr_id = ExprId::from_raw(la_arena::RawIdx::from_u32(0));
        let diag = InferenceDiagnostic::MismatchedArgCount {
            call_expr: expr_id,
            required_count: 2,
            total_count: 2,
            found: 1,
        };

        match diag {
            InferenceDiagnostic::MismatchedArgCount {
                call_expr,
                required_count,
                total_count,
                found,
            } => {
                assert_eq!(call_expr, expr_id);
                assert_eq!(required_count, 2);
                assert_eq!(total_count, 2);
                assert_eq!(found, 1);
            }
            _ => panic!("Expected MismatchedArgCount"),
        }
    }

    #[test]
    fn test_unresolved_method_call_diagnostic() {
        let expr_id = ExprId::from_raw(la_arena::RawIdx::from_u32(0));
        let receiver_name = Name::new("CommonModule");
        let method_name = Name::new("Method");

        let diag = InferenceDiagnostic::UnresolvedMethodCall {
            expr: expr_id,
            receiver_name: receiver_name.clone(),
            method_name: method_name.clone(),
            kind: UnresolvedMethodKind::MethodNotFound,
        };

        match diag {
            InferenceDiagnostic::UnresolvedMethodCall {
                expr,
                receiver_name: r,
                method_name: m,
                kind,
            } => {
                assert_eq!(expr, expr_id);
                assert_eq!(r, receiver_name);
                assert_eq!(m, method_name);
                assert_eq!(kind, UnresolvedMethodKind::MethodNotFound);
            }
            _ => panic!("Expected UnresolvedMethodCall"),
        }
    }

    #[test]
    fn test_type_mismatch_diagnostic() {
        let db = InMemoryDb::new();
        let expr_id = ExprId::from_raw(la_arena::RawIdx::from_u32(0));
        let expected_ty = db.number(None, None);
        let actual_ty = db.string(None, false);

        let diag = InferenceDiagnostic::TypeMismatch {
            expr: expr_id,
            expected: expected_ty,
            actual: actual_ty,
            from_doc_comment: false,
        };

        match diag {
            InferenceDiagnostic::TypeMismatch { expr, expected, actual, .. } => {
                assert_eq!(expr, expr_id);
                assert_eq!(expected, expected_ty);
                assert_eq!(actual, actual_ty);
            }
            _ => panic!("Expected TypeMismatch"),
        }
    }

    #[test]
    fn test_unresolved_field_diagnostic() {
        let db = InMemoryDb::new();
        let expr_id = ExprId::from_raw(la_arena::RawIdx::from_u32(0));
        let receiver_ty = db.metadata_ref(
            hir_def::ty::MetadataKind::CatalogRef,
            "Номенклатура".to_string(),
            &RootConfigCtx,
        );
        let field_name = Name::new("НесуществующееПоле");

        let diag = InferenceDiagnostic::UnresolvedField {
            expr: expr_id,
            receiver_ty,
            field_name: field_name.clone(),
        };

        match diag {
            InferenceDiagnostic::UnresolvedField { expr, receiver_ty: r, field_name: f } => {
                assert_eq!(expr, expr_id);
                assert_eq!(r, receiver_ty);
                assert_eq!(f, field_name);
            }
            _ => panic!("Expected UnresolvedField"),
        }
    }

    #[test]
    fn test_inference_result_with_diagnostics() {
        let mut result = InferenceResult::default();
        assert!(!result.has_diagnostics());

        let expr_id = ExprId::from_raw(la_arena::RawIdx::from_u32(0));
        result.diagnostics.push((
            DefWithBodyId::ModuleCode,
            InferenceDiagnostic::MismatchedArgCount {
                call_expr: expr_id,
                required_count: 2,
                total_count: 2,
                found: 1,
            },
        ));

        assert!(result.has_diagnostics());
        assert_eq!(result.diagnostics.len(), 1);
    }

    fn first_sig(
        db: &dyn bsl_types::intern::TypeKernelDb,
        builtins: &builtin::BuiltinFunctions,
        name: &str,
    ) -> hir_def::ty::FunctionSignature {
        let sigs = builtins.get(name).unwrap_or_else(|| panic!("{name} should exist"));
        sigs[0].lower(db)
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtin_function_lookup() {
        let db = bsl_types::testing::InMemoryDb::new();
        let builtins = builtin::builtin_functions();

        let strlen_sig = first_sig(&db, builtins, "стрдлина");
        assert_eq!(strlen_sig.ret, db.number(None, None));
        assert_eq!(strlen_sig.params.len(), 1);
        assert_eq!(strlen_sig.params[0], db.string(None, false));

        let strlen_en = first_sig(&db, builtins, "strlen");
        assert_eq!(strlen_en.ret, db.number(None, None));

        let upper_case = builtins.get("СТРДЛИНА");
        assert!(upper_case.is_some(), "Lookup should be case-insensitive");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtin_date_function() {
        let db = bsl_types::testing::InMemoryDb::new();
        let builtins = builtin::builtin_functions();

        let current_date = first_sig(&db, builtins, "текущаядата");
        assert_eq!(current_date.ret, db.date(bsl_types::facet::DateComponent::DateTime));
        assert!(current_date.params.is_empty());

        let year = first_sig(&db, builtins, "год");
        assert_eq!(year.ret, db.number(None, None));
        assert_eq!(year.params.len(), 1);
        assert_eq!(year.params[0], db.date(bsl_types::facet::DateComponent::DateTime));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_builtin_type_function() {
        let db = bsl_types::testing::InMemoryDb::new();
        let builtins = builtin::builtin_functions();

        let type_of = first_sig(&db, builtins, "типзнч");
        assert_eq!(type_of.ret, db.type_descriptor());
    }

    fn make_expr(idx: u32) -> ExprId {
        ExprId::from_raw(la_arena::RawIdx::from_u32(idx))
    }

    #[test]
    fn body_inference_result_empty_for_preserves_owner() {
        let method_owner = DefWithBodyId::Method(hir_def::MethodKey::first("М7"));
        let method_empty = BodyInferenceResult::empty_for(method_owner);
        assert_eq!(method_empty.owner, method_owner);
        assert!(method_empty.var_types.is_empty());
        assert!(method_empty.implicit_locals.is_empty());
        assert!(method_empty.binding_types.is_empty());
        assert!(method_empty.expr_types.is_empty());
        assert!(method_empty.diagnostics.is_empty());
        assert!(method_empty.call_arg_bindings.is_empty());
        assert!(method_empty.return_expr_ids.is_empty());

        let module_empty = BodyInferenceResult::empty_for(DefWithBodyId::ModuleCode);
        assert_eq!(module_empty.owner, DefWithBodyId::ModuleCode);
    }

    #[test]
    fn module_code_inference_result_default_is_module_owner() {
        let result = ModuleCodeInferenceResult::default();
        assert_eq!(result.owner, DefWithBodyId::ModuleCode);
        assert!(result.var_types.is_empty());
        assert!(result.expr_types.is_empty());
    }

    #[test]
    fn module_code_inference_result_from_body_preserves_fields() {
        let db = InMemoryDb::new();
        let mut body = BodyInferenceResult::empty_for(DefWithBodyId::ModuleCode);
        body.var_types.insert("х".to_string(), db.string(None, false));
        body.expr_types.insert(make_expr(3), db.boolean());
        body.diagnostics.push(InferenceDiagnostic::TypeMismatch {
            expr: make_expr(4),
            expected: db.string(None, false),
            actual: db.number(None, None),
            from_doc_comment: false,
        });

        let lifted = ModuleCodeInferenceResult::from_body(body);

        assert_eq!(lifted.owner, DefWithBodyId::ModuleCode);
        assert_eq!(lifted.var_types.get("х").copied(), Some(db.string(None, false)),);
        assert_eq!(lifted.expr_types.get(&make_expr(3)).copied(), Some(db.boolean()),);
        assert_eq!(lifted.diagnostics.len(), 1);
    }

    #[test]
    fn infer_owner_result_method_accessors_route_to_method_payload() {
        let db = InMemoryDb::new();
        let mut body =
            BodyInferenceResult::empty_for(DefWithBodyId::Method(hir_def::MethodKey::first("М2")));
        body.var_types.insert("х".to_string(), db.number(None, None));
        body.expr_types.insert(make_expr(5), db.string(None, false));

        let routed = InferOwnerResult::Method(Arc::new(body));

        assert_eq!(routed.owner(), DefWithBodyId::Method(hir_def::MethodKey::first("М2")));
        assert_eq!(routed.type_id_of_expr(make_expr(5)), Some(db.string(None, false)));
        assert_eq!(routed.type_id_of_expr(make_expr(99)), None);
        assert_eq!(routed.var_types().get("х").copied(), Some(db.number(None, None)),);
        assert!(routed.implicit_locals().is_empty());
        assert!(routed.binding_types().is_empty());
    }

    #[test]
    fn infer_owner_result_module_code_accessors_route_to_module_payload() {
        let db = InMemoryDb::new();
        let mut mc = ModuleCodeInferenceResult::default();
        mc.var_types.insert("у".to_string(), db.boolean());
        mc.expr_types.insert(make_expr(1), db.date(DateComponent::DateTime));

        let routed = InferOwnerResult::ModuleCode(Arc::new(mc));

        assert_eq!(routed.owner(), DefWithBodyId::ModuleCode);
        assert_eq!(routed.type_id_of_expr(make_expr(1)), Some(db.date(DateComponent::DateTime)));
        assert_eq!(routed.var_types().get("у").copied(), Some(db.boolean()),);
    }
}
