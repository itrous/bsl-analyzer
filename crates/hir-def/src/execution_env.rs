//! Execution-environment model: in which runtime environments (thin client,
//! web client, server, …) a given body of code can execute.
//!
//! The platform documents API availability per environment ("Доступность" in
//! the syntax helper). To compare that against a call site, the call site
//! itself needs an environment set: the intersection of where the *module*
//! runs and what the method's *compilation directive* selects. This module
//! computes that set from data already resident in [`ModuleMetadata`] and the
//! item tree — no extra queries.
//!
//! An empty [`EnvFlags`] means "don't know where this runs" — either the
//! metadata is missing (unknown) or the module and directive contradict each
//! other (impossible). Consumers must skip availability checks in both cases
//! rather than treat it as "available nowhere".
//!
//! This is deliberately finer-grained than [`call_graph::MethodDispatch`],
//! which collapses everything to can-run-on-client/server for dispatch
//! purposes; availability verdicts need the individual client kinds
//! (e.g. `ЧтениеТекста` exists on the thin client but not the web client).
//!
//! [`call_graph::MethodDispatch`]: crate::call_graph::MethodDispatch

use crate::item_tree::{Annotation, AnnotationKind};
use crate::ModuleMetadata;
use bsl_metadata::ModuleType;
use syntax::preproc_symbols::PreprocSymbolId;

/// Bit set of 1C:Enterprise execution environments.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EnvFlags(u8);

impl EnvFlags {
    pub const THIN_CLIENT: EnvFlags = EnvFlags(1 << 0);
    pub const WEB_CLIENT: EnvFlags = EnvFlags(1 << 1);
    pub const THICK_CLIENT_MANAGED: EnvFlags = EnvFlags(1 << 2);
    pub const THICK_CLIENT_ORDINARY: EnvFlags = EnvFlags(1 << 3);
    pub const SERVER: EnvFlags = EnvFlags(1 << 4);
    pub const MOBILE_CLIENT: EnvFlags = EnvFlags(1 << 5);
    pub const EXTERNAL_CONNECTION: EnvFlags = EnvFlags(1 << 6);

    pub const EMPTY: EnvFlags = EnvFlags(0);
    pub const ALL: EnvFlags = EnvFlags(0x7f);
    /// Non-interactive server-side environments.
    pub const SERVER_SIDE: EnvFlags = EnvFlags(Self::SERVER.0 | Self::EXTERNAL_CONNECTION.0);
    /// Managed-application client environments — every environment
    /// `&НаКлиенте` / `ClientManagedApplication` may legally select. The
    /// legacy thick client (ordinary application) is separate: it is added
    /// only through [`EnvOptions::ordinary_app_support`].
    pub const MANAGED_CLIENTS: EnvFlags = EnvFlags(
        Self::THIN_CLIENT.0
            | Self::WEB_CLIENT.0
            | Self::THICK_CLIENT_MANAGED.0
            | Self::MOBILE_CLIENT.0,
    );
    /// Every interactive client environment, including the legacy thick
    /// client — the set a `ВызовСервера` common module may be invoked from
    /// without being compiled into it.
    pub const ALL_CLIENTS: EnvFlags =
        EnvFlags(Self::MANAGED_CLIENTS.0 | Self::THICK_CLIENT_ORDINARY.0);

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: EnvFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: EnvFlags) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn union(self, other: EnvFlags) -> EnvFlags {
        EnvFlags(self.0 | other.0)
    }

    pub const fn intersection(self, other: EnvFlags) -> EnvFlags {
        EnvFlags(self.0 & other.0)
    }

    /// Environments in `self` that are absent from `other` — the "missing"
    /// set an availability diagnostic reports.
    pub const fn without(self, other: EnvFlags) -> EnvFlags {
        EnvFlags(self.0 & !other.0)
    }

    /// Iterate the individual environments in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = EnvFlags> {
        [
            Self::THIN_CLIENT,
            Self::WEB_CLIENT,
            Self::THICK_CLIENT_MANAGED,
            Self::THICK_CLIENT_ORDINARY,
            Self::SERVER,
            Self::MOBILE_CLIENT,
            Self::EXTERNAL_CONNECTION,
        ]
        .into_iter()
        .filter(move |flag| self.contains(*flag))
    }

    /// Russian name of a single environment, in the wording of the platform
    /// syntax helper ("Доступность").
    pub fn name_ru(self) -> &'static str {
        match self {
            Self::THIN_CLIENT => "Тонкий клиент",
            Self::WEB_CLIENT => "Веб-клиент",
            Self::THICK_CLIENT_MANAGED => "Толстый клиент (управляемое приложение)",
            Self::THICK_CLIENT_ORDINARY => "Толстый клиент (обычное приложение)",
            Self::SERVER => "Сервер",
            Self::MOBILE_CLIENT => "Мобильный клиент",
            Self::EXTERNAL_CONNECTION => "Внешнее соединение",
            _ => "?",
        }
    }

    /// Availability set of a platform API entry, from its syntax-helper
    /// "Доступность" markup. The platform documents one generic thick-client
    /// availability, so it spans both the managed and the ordinary variant.
    /// `None` (no markup) maps to [`Self::ALL`] — never diagnosed.
    pub fn from_platform_context(context: Option<&bsl_platform::ContextAvailability>) -> EnvFlags {
        let ctx = bsl_platform::ContextAvailability::effective(context);
        let mut env = Self::EMPTY;
        if ctx.thin_client {
            env = env | Self::THIN_CLIENT;
        }
        if ctx.web_client {
            env = env | Self::WEB_CLIENT;
        }
        if ctx.thick_client {
            env = env | Self::THICK_CLIENT_MANAGED | Self::THICK_CLIENT_ORDINARY;
        }
        if ctx.server {
            env = env | Self::SERVER;
        }
        if ctx.mobile_client {
            env = env | Self::MOBILE_CLIENT;
        }
        if ctx.external_connection {
            env = env | Self::EXTERNAL_CONNECTION;
        }
        env
    }

    /// English name of a single environment, in EDT qualifier wording
    /// ("… is not defined [Web client]").
    pub fn name_en(self) -> &'static str {
        match self {
            Self::THIN_CLIENT => "Thin client",
            Self::WEB_CLIENT => "Web client",
            Self::THICK_CLIENT_MANAGED => "Thick client (managed application)",
            Self::THICK_CLIENT_ORDINARY => "Thick client (ordinary application)",
            Self::SERVER => "Server",
            Self::MOBILE_CLIENT => "Mobile client",
            Self::EXTERNAL_CONNECTION => "External connection",
            _ => "?",
        }
    }

    /// Parse an environment name from configuration, spelled like the
    /// preprocessor symbols developers already know (`ВебКлиент` /
    /// `WebClient`), case-insensitively. `None` — the spelling names no
    /// modelled environment.
    ///
    /// Spellings and their identity come from the preprocessor symbol
    /// registry; the mapping onto environments stays here, and it is its own
    /// table rather than one shared with `preproc_condition`: there the
    /// question is whether a symbol holds in an environment, here it is which
    /// environments a configuration name selects.
    pub fn from_config_name(name: &str) -> Option<EnvFlags> {
        let mask = Self::checked_envs_of(syntax::preproc_symbols::lookup(name)?);
        (!mask.is_empty()).then_some(mask)
    }

    /// Environments a preprocessor symbol names. An empty mask means the
    /// symbol names no modelled environment — the mobile application
    /// runtimes and the standalone mobile server are genuinely different
    /// runtimes, and `checked_environments` cannot select them.
    ///
    /// `Клиент` / `НаКлиенте` yield the managed clients, not every client:
    /// the legacy thick client (ordinary application) enters the model only
    /// through its own name, which is the contract
    /// [`EnvOptions::ordinary_app_support`] states.
    fn checked_envs_of(id: PreprocSymbolId) -> EnvFlags {
        match id {
            PreprocSymbolId::Server | PreprocSymbolId::AtServer => Self::SERVER,
            PreprocSymbolId::Client | PreprocSymbolId::AtClient => Self::MANAGED_CLIENTS,
            PreprocSymbolId::ThinClient => Self::THIN_CLIENT,
            PreprocSymbolId::WebClient => Self::WEB_CLIENT,
            PreprocSymbolId::MobileClient => Self::MOBILE_CLIENT,
            PreprocSymbolId::ThickClientManagedApplication => Self::THICK_CLIENT_MANAGED,
            PreprocSymbolId::ThickClientOrdinaryApplication => Self::THICK_CLIENT_ORDINARY,
            PreprocSymbolId::ExternalConnection => Self::EXTERNAL_CONNECTION,
            PreprocSymbolId::MobileAppClient
            | PreprocSymbolId::MobileAppServer
            | PreprocSymbolId::MobileStandaloneServer => Self::EMPTY,
        }
    }
}

impl std::ops::BitOr for EnvFlags {
    type Output = EnvFlags;
    fn bitor(self, rhs: EnvFlags) -> EnvFlags {
        self.union(rhs)
    }
}

impl std::ops::BitAnd for EnvFlags {
    type Output = EnvFlags;
    fn bitand(self, rhs: EnvFlags) -> EnvFlags {
        self.intersection(rhs)
    }
}

impl std::fmt::Debug for EnvFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return write!(f, "EnvFlags()");
        }
        write!(f, "EnvFlags(")?;
        for (i, flag) in self.iter().enumerate() {
            if i > 0 {
                write!(f, "|")?;
            }
            write!(f, "{}", flag.name_en())?;
        }
        write!(f, ")")
    }
}

/// Project-level settings that shape environment sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnvOptions {
    /// Interactive client environments the configuration targets. Client
    /// code (`&НаКлиенте`, client-flagged common modules) is assumed to run
    /// in exactly these. Mobile client is excluded by default: most
    /// configurations do not target it, and its availability markup in the
    /// platform data is the least reliable.
    pub client_environments: EnvFlags,
    /// Whether the legacy thick client (ordinary application) is part of the
    /// project's runtime surface. The same effective value as the
    /// diagnostics-level `ordinary_app_support` (the setting, else the main
    /// configuration's `DefaultRunMode`); naming the legacy thick client in
    /// `checked_environments` also turns it on.
    pub ordinary_app_support: bool,
    /// Environments availability diagnostics report on. Server-side modules
    /// formally compile into the external connection too, but flagging every
    /// `БиблиотекаКартинок` in a manager module as "[Внешнее соединение]" is
    /// pedantic noise for configurations that never run there — so external
    /// connection (like the mobile and legacy thick clients) is opt-in.
    pub checked_environments: EnvFlags,
    /// `UseOrdinaryFormInManagedApplication` of the main configuration: whether
    /// an ordinary form also opens in the thick client of the managed
    /// application. `None` — not known (no main configuration, as in a project
    /// of external data processors alone): both thick clients count.
    pub ordinary_forms_in_managed_app: Option<bool>,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self {
            client_environments: EnvFlags::THIN_CLIENT
                | EnvFlags::WEB_CLIENT
                | EnvFlags::THICK_CLIENT_MANAGED,
            ordinary_app_support: false,
            checked_environments: EnvFlags::THIN_CLIENT
                | EnvFlags::WEB_CLIENT
                | EnvFlags::THICK_CLIENT_MANAGED
                | EnvFlags::SERVER,
            ordinary_forms_in_managed_app: None,
        }
    }
}

impl EnvOptions {
    /// `client_environments` restricted to what a client mask may legally
    /// contain — stray server/external bits from config are dropped, and the
    /// legacy thick client enters only through `ordinary_app_support`.
    pub fn managed_client_envs(&self) -> EnvFlags {
        self.client_environments & EnvFlags::MANAGED_CLIENTS
    }

    /// The full client set including the legacy thick client when enabled.
    pub fn client_envs(&self) -> EnvFlags {
        if self.ordinary_app_support {
            self.managed_client_envs() | EnvFlags::THICK_CLIENT_ORDINARY
        } else {
            self.managed_client_envs()
        }
    }

    fn server_envs(&self) -> EnvFlags {
        EnvFlags::SERVER_SIDE
    }
}

/// Environments a module's code can execute in, before any per-method
/// directive narrows it. Empty means unknown.
pub fn module_base_env(metadata: &ModuleMetadata, opts: &EnvOptions) -> EnvFlags {
    match metadata.module_type {
        ModuleType::CommonModule => match &metadata.common_module {
            Some(cm) => common_module_env(cm, opts),
            None => EnvFlags::EMPTY,
        },
        // An ordinary form runs only in a thick client: the ordinary application's,
        // and the managed application's when the configuration allows ordinary forms
        // there. Where that is not known, both thick clients count.
        ModuleType::FormModule if metadata.is_ordinary_form_module() => {
            match opts.ordinary_forms_in_managed_app {
                Some(false) => EnvFlags::THICK_CLIENT_ORDINARY,
                Some(true) | None => {
                    EnvFlags::THICK_CLIENT_ORDINARY | EnvFlags::THICK_CLIENT_MANAGED
                }
            }
        }
        // Form and command methods pick client or server per directive; the
        // module as a whole spans both.
        ModuleType::FormModule | ModuleType::CommandModule => opts.client_envs() | EnvFlags::SERVER,
        ModuleType::ManagedApplicationModule | ModuleType::ApplicationModule => {
            opts.managed_client_envs()
        }
        ModuleType::OrdinaryApplicationModule => EnvFlags::THICK_CLIENT_ORDINARY,
        ModuleType::ObjectModule
        | ModuleType::ManagerModule
        | ModuleType::RecordSetModule
        | ModuleType::ValueManagerModule => {
            if opts.ordinary_app_support {
                EnvFlags::SERVER_SIDE | EnvFlags::THICK_CLIENT_ORDINARY
            } else {
                EnvFlags::SERVER_SIDE
            }
        }
        ModuleType::SessionModule => EnvFlags::SERVER_SIDE,
        ModuleType::ExternalConnectionModule => EnvFlags::EXTERNAL_CONNECTION,
        ModuleType::HTTPServiceModule
        | ModuleType::WebServiceModule
        | ModuleType::IntegrationServiceModule => EnvFlags::SERVER,
        ModuleType::Unknown => EnvFlags::EMPTY,
    }
}

/// Environment set of a common module from its metadata flags. Unlike
/// [`crate::compute_execution_context`], which collapses to a dispatch
/// verdict, this keeps the individual client kinds and honours
/// `ClientOrdinaryApplication` (gated on `ordinary_app_support`), because
/// availability — not naming or dispatch — is decided here.
/// Environments a common module compiles into, from its metadata flags alone.
pub fn common_module_env(cm: &bsl_metadata::CommonModule, opts: &EnvOptions) -> EnvFlags {
    let mut env = EnvFlags::EMPTY;
    if cm.is_server() || cm.is_server_call() {
        env = env | EnvFlags::SERVER;
    }
    if cm.is_external_connection() {
        env = env | EnvFlags::EXTERNAL_CONNECTION;
    }
    // `ServerCall` modules are *called from* the client but *run* on the
    // server, so client flags on them are not added here.
    if !cm.is_server_call() {
        if cm.is_client_managed_application() {
            env = env | opts.managed_client_envs();
        }
        if cm.is_client_ordinary_application() && opts.ordinary_app_support {
            env = env | EnvFlags::THICK_CLIENT_ORDINARY;
        }
    }
    env
}

/// Environments a compilation directive selects, as a mask to intersect with
/// the module's base set. `None` in a form/command module means the platform
/// default `&НаСервере`; elsewhere the module alone decides.
pub fn directive_env(
    kind: Option<AnnotationKind>,
    module_type: ModuleType,
    opts: &EnvOptions,
) -> EnvFlags {
    match kind {
        Some(AnnotationKind::AtClient) => opts.client_envs(),
        Some(AnnotationKind::AtServer | AnnotationKind::AtServerNoContext) => opts.server_envs(),
        Some(AnnotationKind::AtClientAtServer | AnnotationKind::AtClientAtServerNoContext) => {
            opts.client_envs() | opts.server_envs()
        }
        // Weaving annotations carry no compilation directive of their own —
        // the effective directive comes from the intercepted base method.
        Some(
            AnnotationKind::Before
            | AnnotationKind::After
            | AnnotationKind::Instead
            | AnnotationKind::ChangeAndValidate,
        ) => EnvFlags::ALL,
        None => match module_type {
            ModuleType::FormModule | ModuleType::CommandModule => opts.server_envs(),
            _ => EnvFlags::ALL,
        },
    }
}

/// Environment set of one method body: where the module runs, narrowed by the
/// method's compilation directive. Empty means unknown — skip availability
/// checks. `annotations` is the method's item-tree annotation slice; the
/// first compilation directive wins (more than one is itself a diagnostic,
/// see `SeveralCompilerDirectives`).
pub fn body_env(
    metadata: &ModuleMetadata,
    annotations: &[AnnotationKind],
    opts: &EnvOptions,
) -> EnvFlags {
    let base = module_base_env(metadata, opts);
    if base.is_empty() {
        return EnvFlags::EMPTY;
    }
    // A directive does not move a method of an ordinary form: `&НаСервере` there still
    // runs in a thick client, so neither it nor the form-module `&НаСервере`
    // default may narrow the environment.
    if metadata.is_ordinary_form_module() {
        return base;
    }
    let directive = annotations.iter().copied().find(is_compilation_directive);
    // A weaving-only interceptor (`&Вместо("…")` with no directive of its
    // own) inherits the intercepted method's directive, which is unknowable
    // here — so it must not fall into the form-module `&НаСервере` default.
    if directive.is_none() && annotations.iter().any(|a| !is_compilation_directive(a)) {
        return base;
    }
    base & directive_env(directive, metadata.module_type, opts)
}

/// Environment set of module-level code (top-level statements and variable
/// initializers), which never carries a directive.
pub fn module_code_env(metadata: &ModuleMetadata, opts: &EnvOptions) -> EnvFlags {
    module_base_env(metadata, opts)
}

/// [`body_env`] for a method of the item tree.
pub fn method_env(
    item_tree: &crate::item_tree::ItemTree,
    key: crate::MethodKey,
    metadata: &ModuleMetadata,
    opts: &EnvOptions,
) -> EnvFlags {
    let annotations: &[Annotation] = item_tree.method(key).map_or(&[], |m| m.annotations());
    let kinds: Vec<AnnotationKind> = annotations.iter().map(|a| a.kind).collect();
    body_env(metadata, &kinds, opts)
}

/// Full source range of a method of the item tree, for locating it inside
/// item-level preprocessor regions.
pub fn method_source_range(
    item_tree: &crate::item_tree::ItemTree,
    key: crate::MethodKey,
) -> Option<text_size::TextRange> {
    Some(item_tree.method(key)?.source_range())
}

/// Environment mask claimed by the `#Если` branches enclosing `range`, relative
/// to [`EnvFlags::ALL`]. Module-level preprocessor regions wrap whole method
/// definitions, so a method's body environment is its directive-derived set
/// intersected with this mask. An item outside any region keeps [`EnvFlags::ALL`];
/// branches whose condition is undecidable poison the whole chain (see
/// [`crate::preproc_condition::PreprocCondition::narrow_branch`]).
pub fn conditional_env(
    tree: &crate::conditional_tree::ConditionalTree,
    range: text_size::TextRange,
) -> EnvFlags {
    conditional_env_where(tree, |node| node.contains_range(range))
}

/// [`conditional_env`] for a cursor point, with half-open containment: the
/// position right after `#КонецЕсли` is already outside the chain.
pub fn conditional_env_at(
    tree: &crate::conditional_tree::ConditionalTree,
    offset: text_size::TextSize,
) -> EnvFlags {
    conditional_env_where(tree, |node| node.contains(offset))
}

fn conditional_env_where(
    tree: &crate::conditional_tree::ConditionalTree,
    encloses: impl Fn(&text_size::TextRange) -> bool,
) -> EnvFlags {
    use crate::conditional_tree::ConditionalKind;
    use crate::preproc_condition::PreprocCondition;

    // The innermost enclosing node is the smallest containing range: an `If`
    // node's range covers its ElsIf/Else clauses, so depth alone cannot
    // distinguish the then-branch from a sibling clause.
    let mut node = None;
    let mut best_len = None;
    for (idx, data) in tree.conditionals() {
        if encloses(&data.range) {
            let len = data.range.len();
            if best_len.is_none_or(|best| len < best) {
                best_len = Some(len);
                node = Some(idx);
            }
        }
    }

    let mut env = EnvFlags::ALL;
    let mut cur = node;
    while let Some(idx) = cur {
        let chain_if = match tree.conditional(idx).kind {
            ConditionalKind::If => idx,
            // ElsIf/Else clauses always point at their chain's `If`.
            ConditionalKind::ElsIf | ConditionalKind::Else => match tree.parent(idx) {
                Some(parent) => parent,
                None => return EnvFlags::EMPTY,
            },
        };

        let mut remaining = EnvFlags::ALL;
        let mut claimed = EnvFlags::EMPTY;
        for branch in tree.all_branches(chain_if) {
            let data = tree.conditional(branch);
            let branch_env = match data.kind {
                ConditionalKind::If | ConditionalKind::ElsIf => data
                    .condition_text
                    .as_deref()
                    .map_or(PreprocCondition::Unknown, PreprocCondition::parse)
                    .narrow_branch(&mut remaining),
                ConditionalKind::Else => std::mem::take(&mut remaining),
            };
            if branch == idx {
                claimed = branch_env;
                break;
            }
        }
        env = env & claimed;
        cur = tree.parent(chain_if);
    }
    env
}

fn is_compilation_directive(kind: &AnnotationKind) -> bool {
    matches!(
        kind,
        AnnotationKind::AtClient
            | AnnotationKind::AtServer
            | AnnotationKind::AtServerNoContext
            | AnnotationKind::AtClientAtServer
            | AnnotationKind::AtClientAtServerNoContext
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn form_metadata() -> ModuleMetadata {
        ModuleMetadata::unknown(ModuleType::FormModule)
    }

    fn common_metadata(cm: bsl_metadata::CommonModule) -> ModuleMetadata {
        let mut metadata = ModuleMetadata::unknown(ModuleType::CommonModule);
        metadata.execution_context = Some(crate::compute_execution_context(&cm));
        metadata.common_module = Some(Arc::new(cm));
        metadata
    }

    const CLIENTS: EnvFlags = EnvFlags(
        EnvFlags::THIN_CLIENT.0 | EnvFlags::WEB_CLIENT.0 | EnvFlags::THICK_CLIENT_MANAGED.0,
    );

    #[test]
    fn form_module_directives() {
        let opts = EnvOptions::default();
        let md = form_metadata();

        assert_eq!(body_env(&md, &[AnnotationKind::AtClient], &opts), CLIENTS);
        assert_eq!(body_env(&md, &[AnnotationKind::AtServer], &opts), EnvFlags::SERVER);
        assert_eq!(body_env(&md, &[AnnotationKind::AtServerNoContext], &opts), EnvFlags::SERVER);
        assert_eq!(
            body_env(&md, &[AnnotationKind::AtClientAtServer], &opts),
            CLIENTS | EnvFlags::SERVER
        );
        assert_eq!(
            body_env(&md, &[AnnotationKind::AtClientAtServerNoContext], &opts),
            CLIENTS | EnvFlags::SERVER
        );
    }

    #[test]
    fn form_module_without_directive_defaults_to_server() {
        let opts = EnvOptions::default();
        assert_eq!(body_env(&form_metadata(), &[], &opts), EnvFlags::SERVER);

        let cmd = ModuleMetadata::unknown(ModuleType::CommandModule);
        assert_eq!(body_env(&cmd, &[], &opts), EnvFlags::SERVER);
    }

    #[test]
    fn weaving_annotation_does_not_narrow() {
        let opts = EnvOptions::default();
        let md = form_metadata();
        assert_eq!(
            body_env(&md, &[AnnotationKind::Before, AnnotationKind::AtClient], &opts),
            CLIENTS
        );
        // Weaving-only: the interceptor inherits the base method's directive,
        // so nothing narrows — the whole module base stays.
        assert_eq!(body_env(&md, &[AnnotationKind::Instead], &opts), CLIENTS | EnvFlags::SERVER);
    }

    #[test]
    fn server_common_module() {
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default()
            .server(true)
            .external_connection(true)
            .build();
        let md = common_metadata(cm);
        assert_eq!(body_env(&md, &[], &opts), EnvFlags::SERVER_SIDE);
    }

    #[test]
    fn client_server_common_module() {
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default()
            .server(true)
            .client_managed_application(true)
            .build();
        let md = common_metadata(cm);
        assert_eq!(body_env(&md, &[], &opts), CLIENTS | EnvFlags::SERVER);
    }

    #[test]
    fn client_only_common_module() {
        let opts = EnvOptions::default();
        let cm =
            bsl_metadata::CommonModuleBuilder::default().client_managed_application(true).build();
        let md = common_metadata(cm);
        assert_eq!(body_env(&md, &[], &opts), CLIENTS);
    }

    #[test]
    fn server_call_module_runs_on_server_only() {
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default()
            .server(true)
            .server_call(true)
            .client_managed_application(true)
            .build();
        let md = common_metadata(cm);
        assert_eq!(body_env(&md, &[], &opts), EnvFlags::SERVER);
    }

    #[test]
    fn ordinary_form_module_is_thick_client_only_whatever_the_directive() {
        let opts = EnvOptions::default();
        let thick = EnvFlags::THICK_CLIENT_ORDINARY | EnvFlags::THICK_CLIENT_MANAGED;
        let mut ordinary = ModuleMetadata::unknown(ModuleType::FormModule);
        ordinary.form = Some(Arc::new(bsl_metadata::Form::ordinary_without_dialog("Форма")));
        assert!(ordinary.is_ordinary_form_module());
        assert_eq!(module_base_env(&ordinary, &opts), thick);
        assert_eq!(module_code_env(&ordinary, &opts), thick);
        assert_eq!(body_env(&ordinary, &[], &opts), thick);
        for directive in [
            AnnotationKind::AtServer,
            AnnotationKind::AtServerNoContext,
            AnnotationKind::AtClient,
            AnnotationKind::AtClientAtServer,
        ] {
            assert_eq!(body_env(&ordinary, &[directive], &opts), thick, "{directive:?}");
        }

        // A managed form keeps the form-module default (`&НаСервере`) and its directives.
        let mut managed = ModuleMetadata::unknown(ModuleType::FormModule);
        managed.form = Some(Arc::new(bsl_metadata::Form::new(
            "Форма".to_string(),
            bsl_metadata::FormType::Managed,
            Default::default(),
        )));
        assert!(!managed.is_ordinary_form_module());
        assert_eq!(body_env(&managed, &[], &opts), EnvFlags::SERVER);
        assert_ne!(body_env(&managed, &[AnnotationKind::AtClient], &opts), EnvFlags::SERVER);
        // A form whose metadata could not be read stays as before.
        let unknown = ModuleMetadata::unknown(ModuleType::FormModule);
        assert!(!unknown.is_ordinary_form_module());
        assert_eq!(body_env(&unknown, &[], &opts), EnvFlags::SERVER);
    }

    /// The managed thick client opens an ordinary form only when the configuration
    /// allows ordinary forms in the managed application.
    #[test]
    fn ordinary_form_in_managed_thick_client_follows_the_configuration() {
        let mut ordinary = ModuleMetadata::unknown(ModuleType::FormModule);
        ordinary.form = Some(Arc::new(bsl_metadata::Form::ordinary_without_dialog("Форма")));
        let with =
            |flag| EnvOptions { ordinary_forms_in_managed_app: flag, ..EnvOptions::default() };
        let thick = EnvFlags::THICK_CLIENT_ORDINARY | EnvFlags::THICK_CLIENT_MANAGED;

        assert_eq!(body_env(&ordinary, &[], &with(None)), thick);
        assert_eq!(body_env(&ordinary, &[], &with(Some(true))), thick);
        assert_eq!(body_env(&ordinary, &[], &with(Some(false))), EnvFlags::THICK_CLIENT_ORDINARY);
        assert_eq!(
            body_env(&ordinary, &[AnnotationKind::AtClient], &with(Some(false))),
            EnvFlags::THICK_CLIENT_ORDINARY
        );
        assert_eq!(module_code_env(&ordinary, &with(Some(false))), EnvFlags::THICK_CLIENT_ORDINARY);

        // A managed form does not depend on the property.
        let managed = form_metadata();
        for flag in [None, Some(true), Some(false)] {
            assert_eq!(body_env(&managed, &[AnnotationKind::AtClient], &with(flag)), CLIENTS);
            assert_eq!(body_env(&managed, &[], &with(flag)), EnvFlags::SERVER);
        }
    }

    #[test]
    fn ordinary_client_flag_gated_on_option() {
        let cm = || {
            bsl_metadata::CommonModuleBuilder::default()
                .server(true)
                .client_ordinary_application(true)
                .build()
        };
        let off = EnvOptions::default();
        assert_eq!(body_env(&common_metadata(cm()), &[], &off), EnvFlags::SERVER);

        let on = EnvOptions { ordinary_app_support: true, ..EnvOptions::default() };
        assert_eq!(
            body_env(&common_metadata(cm()), &[], &on),
            EnvFlags::SERVER | EnvFlags::THICK_CLIENT_ORDINARY
        );
    }

    #[test]
    fn common_module_without_metadata_is_unknown() {
        let opts = EnvOptions::default();
        let md = ModuleMetadata::unknown(ModuleType::CommonModule);
        assert!(body_env(&md, &[], &opts).is_empty());

        let cm = bsl_metadata::CommonModuleBuilder::default().build();
        assert!(body_env(&common_metadata(cm), &[], &opts).is_empty());
    }

    #[test]
    fn directive_in_common_module_does_not_widen() {
        // A stray &НаКлиенте in a server-only common module must not make the
        // body client-capable: the module base wins on intersection.
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default().server(true).build();
        let md = common_metadata(cm);
        assert_eq!(body_env(&md, &[AnnotationKind::AtClient], &opts), EnvFlags::EMPTY);
    }

    #[test]
    fn object_family_modules_are_server_side() {
        let opts = EnvOptions::default();
        for ty in [
            ModuleType::ObjectModule,
            ModuleType::ManagerModule,
            ModuleType::RecordSetModule,
            ModuleType::ValueManagerModule,
        ] {
            let md = ModuleMetadata::unknown(ty);
            assert_eq!(body_env(&md, &[], &opts), EnvFlags::SERVER_SIDE, "{ty:?}");
        }

        let on = EnvOptions { ordinary_app_support: true, ..EnvOptions::default() };
        let md = ModuleMetadata::unknown(ModuleType::ObjectModule);
        assert_eq!(
            body_env(&md, &[], &on),
            EnvFlags::SERVER_SIDE | EnvFlags::THICK_CLIENT_ORDINARY
        );
    }

    #[test]
    fn service_and_app_modules() {
        let opts = EnvOptions::default();
        for ty in [
            ModuleType::HTTPServiceModule,
            ModuleType::WebServiceModule,
            ModuleType::IntegrationServiceModule,
        ] {
            assert_eq!(
                body_env(&ModuleMetadata::unknown(ty), &[], &opts),
                EnvFlags::SERVER,
                "{ty:?}"
            );
        }
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::SessionModule), &[], &opts),
            EnvFlags::SERVER_SIDE
        );
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::ExternalConnectionModule), &[], &opts),
            EnvFlags::EXTERNAL_CONNECTION
        );
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::ManagedApplicationModule), &[], &opts),
            CLIENTS
        );
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::OrdinaryApplicationModule), &[], &opts),
            EnvFlags::THICK_CLIENT_ORDINARY
        );
        assert!(body_env(&ModuleMetadata::unknown(ModuleType::Unknown), &[], &opts).is_empty());
    }

    #[test]
    fn external_only_common_module() {
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default().external_connection(true).build();
        assert_eq!(body_env(&common_metadata(cm), &[], &opts), EnvFlags::EXTERNAL_CONNECTION);
    }

    #[test]
    fn server_call_without_server_flag_still_runs_on_server() {
        let opts = EnvOptions::default();
        let cm = bsl_metadata::CommonModuleBuilder::default().server_call(true).build();
        assert_eq!(body_env(&common_metadata(cm), &[], &opts), EnvFlags::SERVER);
    }

    #[test]
    fn application_modules_are_managed_clients() {
        let opts = EnvOptions::default();
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::ApplicationModule), &[], &opts),
            CLIENTS
        );
        // ordinary_app_support widens forms and object modules, but never the
        // managed-application module itself.
        let on = EnvOptions { ordinary_app_support: true, ..EnvOptions::default() };
        assert_eq!(
            body_env(&ModuleMetadata::unknown(ModuleType::ManagedApplicationModule), &[], &on),
            CLIENTS
        );
    }

    #[test]
    fn client_environments_config_is_masked() {
        // Stray non-client bits from config must not leak into client masks,
        // and the legacy thick client enters only via ordinary_app_support.
        let opts = EnvOptions {
            client_environments: EnvFlags::THIN_CLIENT
                | EnvFlags::SERVER
                | EnvFlags::EXTERNAL_CONNECTION
                | EnvFlags::THICK_CLIENT_ORDINARY,
            ..EnvOptions::default()
        };
        assert_eq!(opts.client_envs(), EnvFlags::THIN_CLIENT);

        let md = form_metadata();
        assert_eq!(body_env(&md, &[AnnotationKind::AtClient], &opts), EnvFlags::THIN_CLIENT);
    }

    #[test]
    fn mobile_client_opt_in() {
        let opts = EnvOptions {
            client_environments: EnvFlags::THIN_CLIENT
                | EnvFlags::WEB_CLIENT
                | EnvFlags::THICK_CLIENT_MANAGED
                | EnvFlags::MOBILE_CLIENT,
            ..EnvOptions::default()
        };
        let md = form_metadata();
        assert_eq!(
            body_env(&md, &[AnnotationKind::AtClient], &opts),
            CLIENTS | EnvFlags::MOBILE_CLIENT
        );
    }

    #[test]
    fn platform_context_conversion() {
        assert_eq!(EnvFlags::from_platform_context(None), EnvFlags::ALL);

        let ctx = bsl_platform::ContextAvailability {
            thick_client: true,
            thin_client: true,
            web_client: false,
            server: true,
            mobile_client: false,
            external_connection: true,
        };
        let env = EnvFlags::from_platform_context(Some(&ctx));
        assert_eq!(
            env,
            EnvFlags::THIN_CLIENT
                | EnvFlags::THICK_CLIENT_MANAGED
                | EnvFlags::THICK_CLIENT_ORDINARY
                | EnvFlags::SERVER
                | EnvFlags::EXTERNAL_CONNECTION
        );
        assert!(!env.contains(EnvFlags::WEB_CLIENT));

        let nowhere = bsl_platform::ContextAvailability {
            thick_client: false,
            thin_client: false,
            web_client: false,
            server: false,
            mobile_client: false,
            external_connection: false,
        };
        assert!(EnvFlags::from_platform_context(Some(&nowhere)).is_empty());
    }

    #[test]
    fn method_env_reads_item_tree_annotations() {
        let parse = parser::parse(
            "&НаКлиенте\nПроцедура Клиентская()\nКонецПроцедуры\n\
             &НаСервере\nПроцедура Серверная()\nКонецПроцедуры\n",
        );
        let item_tree = crate::item_tree::ItemTree::from_parse(&parse);
        let opts = EnvOptions::default();
        let md = form_metadata();
        let key = crate::MethodKey::first;
        assert_eq!(method_env(&item_tree, key("Клиентская"), &md, &opts), CLIENTS);
        assert_eq!(method_env(&item_tree, key("Серверная"), &md, &opts), EnvFlags::SERVER);
        // An unknown method behaves like "no directive".
        assert_eq!(method_env(&item_tree, key("Нет"), &md, &opts), EnvFlags::SERVER);
    }

    fn conditional_env_for(source: &str, method_idx: u32) -> EnvFlags {
        let parse = parser::parse(source);
        let item_tree = crate::item_tree::ItemTree::from_parse(&parse);
        let tree = crate::conditional_tree::lower_conditionals(&parse.syntax_node());
        let key = item_tree.methods().nth(method_idx as usize).expect("method must exist").key();
        let range = method_source_range(&item_tree, key).expect("method must exist");
        conditional_env(&tree, range)
    }

    #[test]
    fn conditional_env_outside_regions_keeps_all() {
        let env = conditional_env_for("Процедура П()\nКонецПроцедуры\n", 0);
        assert_eq!(env, EnvFlags::ALL);
    }

    #[test]
    fn conditional_env_narrows_guarded_method() {
        let env = conditional_env_for(
            "#Если НЕ ВебКлиент Тогда\nПроцедура П()\nКонецПроцедуры\n#КонецЕсли\n",
            0,
        );
        assert_eq!(env, EnvFlags::ALL.without(EnvFlags::WEB_CLIENT));
    }

    #[test]
    fn conditional_env_else_gets_complement() {
        let source = "#Если ВебКлиент Тогда\nПроцедура Веб()\nКонецПроцедуры\n\
                      #Иначе\nПроцедура НеВеб()\nКонецПроцедуры\n#КонецЕсли\n";
        assert_eq!(conditional_env_for(source, 0), EnvFlags::WEB_CLIENT);
        assert_eq!(conditional_env_for(source, 1), EnvFlags::ALL.without(EnvFlags::WEB_CLIENT));
    }

    #[test]
    fn conditional_env_elsif_chain_partitions() {
        let source = "#Если ВебКлиент Тогда\nПроцедура Веб()\nКонецПроцедуры\n\
                      #ИначеЕсли ТонкийКлиент Тогда\nПроцедура Тонкий()\nКонецПроцедуры\n\
                      #Иначе\nПроцедура Прочие()\nКонецПроцедуры\n#КонецЕсли\n";
        assert_eq!(conditional_env_for(source, 0), EnvFlags::WEB_CLIENT);
        assert_eq!(conditional_env_for(source, 1), EnvFlags::THIN_CLIENT);
        assert_eq!(
            conditional_env_for(source, 2),
            EnvFlags::ALL.without(EnvFlags::WEB_CLIENT | EnvFlags::THIN_CLIENT)
        );
    }

    #[test]
    fn conditional_env_nested_regions_intersect() {
        let env = conditional_env_for(
            "#Если Клиент Тогда\n#Если НЕ ВебКлиент Тогда\n\
             Процедура П()\nКонецПроцедуры\n#КонецЕсли\n#КонецЕсли\n",
            0,
        );
        assert!(!env.contains(EnvFlags::WEB_CLIENT));
        assert!(!env.contains(EnvFlags::SERVER));
        assert!(env.contains(EnvFlags::THIN_CLIENT));
    }

    #[test]
    fn conditional_env_undecidable_condition_poisons_chain() {
        let source = "#Если Линукс Тогда\nПроцедура А()\nКонецПроцедуры\n\
                      #Иначе\nПроцедура Б()\nКонецПроцедуры\n#КонецЕсли\n";
        assert_eq!(conditional_env_for(source, 0), EnvFlags::EMPTY);
        assert_eq!(conditional_env_for(source, 1), EnvFlags::EMPTY);
    }

    #[test]
    fn config_names_parse_bilingually_and_case_insensitively() {
        assert_eq!(EnvFlags::from_config_name("ВебКлиент"), Some(EnvFlags::WEB_CLIENT));
        assert_eq!(EnvFlags::from_config_name("webclient"), Some(EnvFlags::WEB_CLIENT));
        assert_eq!(EnvFlags::from_config_name("СЕРВЕР"), Some(EnvFlags::SERVER));
        assert_eq!(
            EnvFlags::from_config_name("ТолстыйКлиентУправляемоеПриложение"),
            Some(EnvFlags::THICK_CLIENT_MANAGED)
        );
        assert_eq!(EnvFlags::from_config_name("Линукс"), None);
    }

    /// Ответ на каждое написание реестра разобран поимённо, в обе стороны:
    /// и что принимается, и что нет.
    ///
    /// Таблица здесь, а не в реестре: реестр знает написания, а какие среды
    /// они выбирают — решение этого слоя, и его надо предъявить целиком.
    #[test]
    fn every_registry_spelling_has_a_decided_config_answer() {
        use syntax::preproc_symbols::PreprocSymbolId as Id;

        let expected = |id: Id| match id {
            Id::Server | Id::AtServer => Some(EnvFlags::SERVER),
            Id::Client | Id::AtClient => Some(EnvFlags::MANAGED_CLIENTS),
            Id::ThinClient => Some(EnvFlags::THIN_CLIENT),
            Id::WebClient => Some(EnvFlags::WEB_CLIENT),
            Id::MobileClient => Some(EnvFlags::MOBILE_CLIENT),
            Id::ThickClientManagedApplication => Some(EnvFlags::THICK_CLIENT_MANAGED),
            Id::ThickClientOrdinaryApplication => Some(EnvFlags::THICK_CLIENT_ORDINARY),
            Id::ExternalConnection => Some(EnvFlags::EXTERNAL_CONNECTION),
            // Отдельные среды исполнения, моделью не покрытые.
            Id::MobileAppClient | Id::MobileAppServer | Id::MobileStandaloneServer => None,
        };

        for &id in Id::ALL {
            for spelling in id.spellings() {
                assert_eq!(
                    EnvFlags::from_config_name(spelling),
                    expected(id),
                    "{spelling:?}: ответ конфигурации не тот"
                );
                assert_eq!(
                    EnvFlags::from_config_name(&spelling.to_uppercase()),
                    expected(id),
                    "{spelling:?}: регистр изменил ответ"
                );
            }
        }
    }

    /// `Клиент` не включает устаревший толстый клиент обычного приложения:
    /// тот входит в модель только собственным именем.
    #[test]
    fn client_names_the_managed_clients_only() {
        let clients = EnvFlags::from_config_name("Клиент").expect("Клиент — имя сред");
        assert_eq!(clients, EnvFlags::MANAGED_CLIENTS);
        assert!(!clients.contains(EnvFlags::THICK_CLIENT_ORDINARY));
        assert_eq!(EnvFlags::from_config_name("НаКлиенте"), Some(EnvFlags::MANAGED_CLIENTS));
        assert_eq!(EnvFlags::from_config_name("НаСервере"), Some(EnvFlags::SERVER));
        assert_eq!(
            EnvFlags::from_config_name("ТолстыйКлиентОбычноеПриложение"),
            Some(EnvFlags::THICK_CLIENT_ORDINARY)
        );
    }

    /// ОС-символы источника не имеют, и конфигурация их не принимает — тот
    /// же ответ, что у остальных потребителей реестра.
    #[test]
    fn os_symbols_are_not_configuration_environments() {
        for os in ["Linux", "Windows", "MacOS", "linux"] {
            assert_eq!(EnvFlags::from_config_name(os), None, "{os:?} принят как среда");
        }
    }

    /// Написание сравнивается по правилу лексера: `ſ` за `s` проходит.
    #[test]
    fn config_names_fold_case_the_way_the_lexer_does() {
        assert_eq!(EnvFlags::from_config_name("\u{17F}erver"), Some(EnvFlags::SERVER));
        assert_eq!(EnvFlags::from_config_name("\u{1C83}ервер"), Some(EnvFlags::SERVER));
        assert_eq!(EnvFlags::from_config_name("Thi\u{131}Client"), None);
    }

    #[test]
    fn set_operations_and_names() {
        let e = EnvFlags::SERVER | EnvFlags::WEB_CLIENT;
        assert!(e.contains(EnvFlags::SERVER));
        assert!(!e.contains(EnvFlags::THIN_CLIENT));
        assert!(e.intersects(EnvFlags::WEB_CLIENT));
        assert_eq!(e.without(EnvFlags::SERVER), EnvFlags::WEB_CLIENT);
        assert_eq!(e.iter().count(), 2);
        assert_eq!(EnvFlags::WEB_CLIENT.name_en(), "Web client");
        assert_eq!(EnvFlags::WEB_CLIENT.name_ru(), "Веб-клиент");
        assert_eq!(format!("{:?}", e), "EnvFlags(Web client|Server)");
        assert_eq!(format!("{:?}", EnvFlags::EMPTY), "EnvFlags()");
    }
}
