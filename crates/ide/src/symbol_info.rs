//! Orchestrator for the MCP `symbol_info` tool: interpret a qualified BSL name (or a
//! `path`+`line`+`column` position for locals) against the resident analysis database and
//! assemble a consolidated semantic card — kind, signature, type, doc, and definition.
//!
//! All BSL semantics live here (dotted-name interpretation, resident name resolution, card
//! assembly). The MCP adapter is a thin projection that adds graph-derived usages/candidates,
//! applies the output budget, and serializes. Resolution is resident-led: the card is servable
//! whenever the resident host is `Ready`, independent of the call graph.

pub(crate) mod form;

use bsl_metadata::{AttributeType, MdoType};
use bsl_platform::{PlatformCatalogStatus, PlatformGlobalCatalog};
use hir::execution_env::EnvFlags;
use hir::{
    compute_execution_context, execution_environment_at, kernel_type_label, method_return_type,
    root_metadata_object_type, root_metadata_ref_type, root_object_manager_type, DefDatabase,
    Definition, ExecutionContext, HirFieldOrigin, ManagerType, MethodId, ModuleId, Name, Semantics,
    Type as HirType,
};
use ide_db::base_db::{Locale, RootQueryDb, SourceDatabase, SourceRootId};
use ide_db::{
    effective_module_exports_query, EffectiveMetadataMemberValue, EffectiveModuleRole,
    RootDatabaseImpl,
};
use line_index::{LineColRange, LineIndex};
use symbol_info::{build_signature, render_declaration, CalleeKind, MethodKind, SymbolSignature};
use syntax::TextRange;
use vfs::FileId;

/// The whole workspace is loaded into a single local source root; the resident host mirrors
/// the graph's `GRAPH_SOURCE_ROOT`, so the module index is keyed here.
const CONFIG_SOURCE_ROOT: SourceRootId = SourceRootId(0);

/// A position in a resident file, used by the local/positional resolution path.
#[derive(Debug, Clone, Copy)]
pub struct SymbolPosition {
    pub file_id: FileId,
    /// 0-based line.
    pub line: u32,
    /// 0-based UTF-16-agnostic column (character offset within the line).
    pub column: u32,
}

/// Which card sections the caller wants. An all-false request means "all sections".
#[derive(Debug, Clone, Copy)]
pub struct SymbolInfoSections {
    pub definition: bool,
    pub type_: bool,
    pub doc: bool,
}

impl SymbolInfoSections {
    /// All sections enabled — the default when the caller passes no `include` filter.
    pub fn all() -> Self {
        Self { definition: true, type_: true, doc: true }
    }
}

/// A request for one symbol's card: either a qualified `symbol` name (primary) or a
/// `position` (fallback, for locals/parameters). Exactly one is expected; `symbol` wins if
/// both are set.
#[derive(Debug, Clone)]
pub struct SymbolInfoRequest {
    pub symbol: Option<String>,
    pub position: Option<SymbolPosition>,
    pub locale: Locale,
    pub sections: SymbolInfoSections,
    /// The root the call graph was built against, in the spelling the generation serving this
    /// request walked its files under, used to encode a form event-handler's path-fallback
    /// `method/file/<rel>::<name>` graph id byte-identically to the graph builder. This is NOT the
    /// config root (`db.all_config_paths()` — e.g. `<workspace>/src/cf`): the graph strips the
    /// workspace root, so using the config root would mint a mismatched, non-resolving id and drop
    /// form-handler usages. `None` disables the form-handler `graph_id` (usages) only.
    pub workspace_root: Option<crate::graph::StripRoot>,
}

/// The container a symbol lives in (its owning module or metadata object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolContainer {
    /// The container kind label, e.g. `ОбщийМодуль`, `Справочник`, `Документ`.
    pub kind: String,
    /// The container's name, e.g. the common-module or object name.
    pub name: String,
    /// Where the container's code runs (server/client), when known.
    pub context: Option<String>,
}

/// One member of an aggregate card (a metadata object's attribute or tabular-section field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMember {
    pub name: String,
    /// `Реквизит`, `ТабличнаяЧасть`, `Измерение`, `Ресурс`, …
    pub kind: String,
    pub member_kind: &'static str,
    pub ty: Option<String>,
    pub type_variants: Vec<SymbolTypeVariant>,
    pub signature: Option<SymbolMemberSignature>,
    pub origin: SymbolMemberOrigin,
    pub source_extension: Option<String>,
    pub availability: SymbolMemberAvailability,
}

impl SymbolMember {
    pub fn metadata(
        name: impl Into<String>,
        kind: impl Into<String>,
        member_kind: &'static str,
        ty: Option<String>,
    ) -> Self {
        let type_variants = ty
            .as_ref()
            .map(|presentation| {
                vec![SymbolTypeVariant {
                    presentation: presentation.clone(),
                    technical_name: None,
                    resolution: "unresolved",
                    reason: Some("technical_name_unavailable".to_string()),
                }]
            })
            .unwrap_or_default();
        Self {
            name: name.into(),
            kind: kind.into(),
            member_kind,
            ty,
            type_variants,
            signature: None,
            origin: SymbolMemberOrigin::Metadata,
            source_extension: None,
            availability: SymbolMemberAvailability::not_evaluated(None),
        }
    }

    pub fn callable(
        name: impl Into<String>,
        kind: impl Into<String>,
        member_kind: &'static str,
        presentation: impl Into<String>,
        origin: SymbolMemberOrigin,
    ) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            member_kind,
            ty: None,
            type_variants: Vec::new(),
            signature: Some(SymbolMemberSignature { presentation: presentation.into() }),
            origin,
            source_extension: None,
            availability: SymbolMemberAvailability::not_evaluated(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTypeVariant {
    pub presentation: String,
    pub technical_name: Option<String>,
    pub resolution: &'static str,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMemberSignature {
    pub presentation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolMemberOrigin {
    Metadata,
    Module,
    Platform,
}

impl SymbolMemberOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Module => "module",
            Self::Platform => "platform",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMemberAvailability {
    pub contexts: Option<Vec<&'static str>>,
    pub context_status: SymbolMemberContextStatus,
    pub reason: Option<String>,
}

impl SymbolMemberAvailability {
    pub fn not_evaluated(contexts: Option<Vec<&'static str>>) -> Self {
        Self { contexts, context_status: SymbolMemberContextStatus::NotEvaluated, reason: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolMemberContextStatus {
    NotEvaluated,
    Available,
    Unavailable,
    Unknown,
}

impl SymbolMemberContextStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// The definition site of a symbol whose source is a BSL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDefinition {
    /// Workspace path of the defining file, when it maps to a source root.
    pub path: Option<String>,
    /// 1-based line of the declaration.
    pub line: u32,
    /// The declaration line (or the local's line) as a source snippet.
    pub snippet: Option<String>,
    /// The declared name alone, 0-based with UTF-16 columns. Absent where the name has
    /// no range of its own — a local, a module card anchored at the file start.
    pub name_range: Option<LineColRange>,
    /// The whole declaration, same units. Absent for the same reason.
    pub enclosing_range: Option<LineColRange>,
}

/// The two byte ranges a definition may know about, kept as one value so the two
/// `Option<TextRange>` cannot be passed in the wrong order.
#[derive(Debug, Clone, Copy, Default)]
struct DefinitionRanges {
    /// The declared name.
    name: Option<TextRange>,
    /// The whole declaration node.
    enclosing: Option<TextRange>,
}

/// Every value `SymbolInfoCard::kind` is ever served with.
///
/// One list, because the kind of a symbol is published by more than one tool and a second
/// spelling of the same idea is a vocabulary that drifts silently: a consumer switching on
/// `kind` cannot tell a value it has never seen from one that quietly changed spelling. Any
/// tool that publishes a kind of its own projects into THIS list (see
/// [`DeclarationKind::symbol_kind`]) rather than inventing a parallel one.
pub const SYMBOL_KINDS: &[&str] = &[
    "attribute",
    "common module",
    "field",
    "form",
    "form attribute",
    "form item",
    "function",
    "local variable",
    "manager module",
    "member candidates",
    "metadata object",
    "method",
    "module",
    "module variable",
    "parameter",
    "platform function",
    "platform procedure",
    "procedure",
    "tabular section",
    "unresolved",
];

/// What a declaration site plays for the requested name.
///
/// A configuration extension may weave onto a method instead of replacing its file, so one
/// name can have several declarations at once. Naming the part each one plays is what lets a
/// client tell "this is the body that runs" from "this runs around it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionRole {
    /// The declaration the configuration itself carries, or the only one there is.
    Base,
    /// `&Вместо` — replaces the base body, which then runs only via `ПродолжитьВызов`.
    Instead,
    /// `&Перед` — runs before the base body.
    Before,
    /// `&После` — runs after the base body.
    After,
}

impl DefinitionRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Instead => "instead",
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// One declaration of the requested name, and the part it plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDefinitionSite {
    pub definition: SymbolDefinition,
    pub role: DefinitionRole,
    /// The extension that carries this site, `None` for the configuration's own.
    pub source_extension: Option<String>,
}

/// The consolidated semantic card for one symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfoCard {
    /// The canonical symbol string this card describes.
    pub symbol: String,
    /// A stable kind label, e.g. `function`, `procedure`, `metadata object`, `attribute`,
    /// `local variable`, `parameter`, `platform function`, `common module`.
    pub kind: &'static str,
    pub container: Option<SymbolContainer>,
    /// A one-line signature (methods) or structure summary (aggregate objects).
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// Declared/inferred return type (methods) or the attribute's type (attribute cards).
    pub return_type: Option<String>,
    pub definition: Option<SymbolDefinition>,
    /// Every declaration of this name with the part it plays: the base one, then any
    /// interceptors an extension weaves onto it. Empty where the card has no source at all.
    pub definitions: Vec<SymbolDefinitionSite>,
    /// Aggregate members for a whole-object card (empty otherwise).
    pub members: Vec<SymbolMember>,
    /// The durable call-graph id for this symbol, when it is a method that the graph can address.
    /// The adapter uses it to attach the `usages` summary directly (no fuzzy re-resolution) and
    /// echoes it back so an agent can walk the full caller list via `graph`.
    pub graph_id: Option<String>,
    /// Set when the graph knew the symbol but the resident host no longer has it (a stale
    /// candidate). The card is otherwise empty and the caller should treat it as best-effort.
    pub semantics_unavailable: bool,
}

impl SymbolInfoCard {
    /// Record the declaration the resolver picked. The deprecated `definition` field keeps
    /// naming this same site, so its 1-based values never move when interceptor sites are
    /// appended beside it.
    ///
    /// `source_extension` is the label of the root the declaration lives in, and it is asked
    /// for here rather than assumed absent: an extension may DECLARE a method the base
    /// configuration never had, and that site is a `base` — the only one there is — while
    /// still coming from an extension. The field follows the root, not the role.
    fn set_base_definition(
        &mut self,
        definition: Option<SymbolDefinition>,
        source_extension: Option<String>,
    ) {
        if let Some(site) = definition.clone() {
            self.definitions.push(SymbolDefinitionSite {
                definition: site,
                role: DefinitionRole::Base,
                source_extension,
            });
        }
        self.definition = definition;
    }

    fn empty(symbol: String, kind: &'static str) -> Self {
        Self {
            symbol,
            kind,
            container: None,
            signature: None,
            doc: None,
            return_type: None,
            definition: None,
            definitions: Vec::new(),
            members: Vec::new(),
            graph_id: None,
            semantics_unavailable: false,
        }
    }
}

/// Resolve a symbol on the resident database and assemble its card. Returns `None` when the
/// resident cannot resolve the request — the adapter then offers graph-derived candidates
/// (imprecise-name path) rather than a hard error.
pub fn symbol_info(db: &RootDatabaseImpl, req: &SymbolInfoRequest) -> Option<SymbolInfoCard> {
    if let Some(symbol) = req.symbol.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_qualified(db, symbol, req);
    }
    if let Some(pos) = req.position {
        return resolve_position(db, pos, req);
    }
    None
}

/// A qualified symbol is a dot-separated sequence of BSL-like identifiers.
/// Keep keywords accepted for compatibility: this checks shape, not whether a
/// segment may be declared as a new identifier.
pub fn is_well_formed_symbol(symbol: &str) -> bool {
    let symbol = symbol.trim();
    !symbol.is_empty()
        && symbol.split('.').all(|segment| {
            let mut chars = segment.trim().chars();
            chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        })
}

// --- qualified-name resolution ----------------------------------------------------------

fn resolve_qualified(
    db: &RootDatabaseImpl,
    symbol: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let segments: Vec<&str> = symbol.split('.').map(str::trim).filter(|s| !s.is_empty()).collect();
    if let Some(card) = form::resolve_form(db, symbol, &segments, req) {
        return Some(card);
    }
    match segments.as_slice() {
        [one] => resolve_single(db, symbol, one, req),
        [a, b] => resolve_pair(db, symbol, a, b, req),
        [a, b, c] => resolve_triple(db, symbol, a, b, c, req),
        _ => None,
    }
}

/// `<Name>`: a common module (whole-module card) or a platform global function.
fn resolve_single(
    db: &RootDatabaseImpl,
    symbol: &str,
    name: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let module_index = db.module_index(CONFIG_SOURCE_ROOT);
    // The body ranked FIRST by config root, not the one the path-derived index happened to
    // answer with. A whole-module card names a place like any other card, and taking it from
    // an earlier-sorting extension makes the card say the module is declared by the extension
    // — root_id and `source_extension` both.
    if let Some(file_id) = ranked_module_body(db, &module_index, &Name::new(name)) {
        let display =
            module_index.canonical_common_module_name(&Name::new(name)).unwrap_or(name).to_string();
        let mut card = SymbolInfoCard::empty(symbol.to_string(), "common module");
        card.container = Some(SymbolContainer {
            kind: "ОбщийМодуль".to_string(),
            name: display,
            context: module_context(db, ModuleId::new(file_id)),
        });
        // A module card is anchored at the file start: the module has no declaration
        // node of its own, so there is no range to publish.
        card.set_base_definition(
            def_from_file_line(
                db,
                file_id,
                syntax::TextSize::from(0u32),
                DefinitionRanges::default(),
                req.sections.definition,
            ),
            root_label(db, file_id),
        );
        return Some(card);
    }

    // A bare platform global function (`СтрНайти`, `ТекущаяДата`, …).
    let callee = CalleeKind::GlobalFunction { name: name.into() };
    let sigs = build_signature(db, FileId(0), &callee)?;
    let sig = sigs.first()?;
    Some(card_from_platform_sig(symbol, sig, req))
}

/// `<A>.<B>`: a common-module method, a whole metadata object, or a platform type method.
fn resolve_pair(
    db: &RootDatabaseImpl,
    symbol: &str,
    a: &str,
    b: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let module_index = db.module_index(CONFIG_SOURCE_ROOT);

    // Common module method.
    if let Some(module_file) = module_index.resolve_common_module(&Name::new(a)) {
        let display =
            module_index.canonical_common_module_name(&Name::new(a)).unwrap_or(a).to_string();
        let callee =
            CalleeKind::CommonModuleMethod { module: Name::new(&display), method: Name::new(b) };
        if let Some(sigs) = build_signature(db, module_file, &callee) {
            if let Some(sig) = sigs.first() {
                // The execution context comes from the body that DECLARES the method, not
                // from whichever body the path-derived index happened to answer with. The
                // two differ exactly when an extension's path sorts first, and a card that
                // takes the definition from one body and the context from another describes
                // an environment the published declaration does not run in.
                let declaring_file =
                    sig.method_id.map(|id| id.module.file_id).unwrap_or(module_file);
                let container = SymbolContainer {
                    kind: "ОбщийМодуль".to_string(),
                    name: display,
                    context: module_context(db, ModuleId::new(declaring_file)),
                };
                return Some(card_from_method_sig(db, symbol, sig, Some(container), req));
            }
        }
    }

    if let Some(facet) = parse_applied_facet(a) {
        return applied_facet_card(db, symbol, facet, b, req);
    }

    // Whole metadata object (`Справочник.Товары`).
    if let Some(mdo_type) = parse_mdo_type(a) {
        if let Some(card) = object_card(db, symbol, mdo_type, b, req) {
            return Some(card);
        }
    }

    // A platform type method (`Массив.Добавить`) — brief card; full reference lives in
    // `syntax_help`.
    let callee = CalleeKind::PlatformMethod { type_name: a.into(), method_name: b.into() };
    if let Some(sigs) = build_signature(db, FileId(0), &callee) {
        if let Some(sig) = sigs.first() {
            return Some(card_from_platform_sig(symbol, sig, req));
        }
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedFacet {
    Object(MdoType),
    Reference(MdoType),
    Manager(MdoType),
}

impl AppliedFacet {
    fn mdo_type(self) -> MdoType {
        match self {
            Self::Object(mdo_type) | Self::Reference(mdo_type) | Self::Manager(mdo_type) => {
                mdo_type
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Object(MdoType::Catalog) => "СправочникОбъект",
            Self::Reference(MdoType::Catalog) => "СправочникСсылка",
            Self::Manager(MdoType::Catalog) => "СправочникМенеджер",
            Self::Object(MdoType::DataProcessor) => "ОбработкаОбъект",
            _ => unreachable!("only declared applied facets are constructed"),
        }
    }
}

fn parse_applied_facet(segment: &str) -> Option<AppliedFacet> {
    match segment.to_lowercase().as_str() {
        "справочникобъект" => Some(AppliedFacet::Object(MdoType::Catalog)),
        "справочникссылка" => Some(AppliedFacet::Reference(MdoType::Catalog)),
        "справочникменеджер" => Some(AppliedFacet::Manager(MdoType::Catalog)),
        "обработкаобъект" => Some(AppliedFacet::Object(MdoType::DataProcessor)),
        _ => None,
    }
}

fn applied_facet_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    facet: AppliedFacet,
    name: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let mdo_type = facet.mdo_type();
    let object = match req.position {
        Some(position) => db.resolve_metadata_object_for_file(position.file_id, mdo_type, name),
        None => db.resolve_metadata_object_across_roots(mdo_type, name),
    }?;
    let mut card = if matches!(facet, AppliedFacet::Object(_)) {
        object_card(db, symbol, mdo_type, name, req)?
    } else {
        let mut card = SymbolInfoCard::empty(symbol.to_string(), "metadata object");
        card.container = Some(SymbolContainer {
            kind: facet.label().to_string(),
            name: object.name.clone(),
            context: None,
        });
        card.signature = Some(format!("{}.{}", facet.label(), object.name));
        card
    };
    card.container.as_mut()?.kind = facet.label().to_string();
    let members = match facet {
        AppliedFacet::Object(_) => applied_object_members(db, mdo_type, name, req),
        AppliedFacet::Reference(_) => applied_reference_members(db, mdo_type, name, req),
        AppliedFacet::Manager(_) => applied_manager_members(db, mdo_type, name, req),
    };
    card.members = collect_applied_members(members, None);
    let availability = MemberAvailabilityContext::from_request(db, req);
    for member in &mut card.members {
        if member.origin != SymbolMemberOrigin::Platform {
            member.availability = availability.evaluate(None, false);
        }
    }
    Some(card)
}

fn applied_object_members(
    db: &RootDatabaseImpl,
    mdo_type: MdoType,
    object_name: &str,
    req: &SymbolInfoRequest,
) -> Vec<SymbolMember> {
    let mut members = Vec::new();

    let metadata = match req.position {
        Some(position) => {
            db.effective_metadata_members_for_file(position.file_id, mdo_type, object_name)
        }
        None => db.effective_metadata_members_across_roots(mdo_type, object_name),
    };
    if let Some(metadata) = metadata {
        members.extend(metadata.iter().map(|candidate| {
            let mut member = match &candidate.member {
                EffectiveMetadataMemberValue::Attribute(attribute) => static_metadata_member(
                    db,
                    attribute.name.clone(),
                    "Реквизит",
                    "attribute",
                    &attribute.attr_type,
                ),
                EffectiveMetadataMemberValue::TabularSection(section) => SymbolMember::metadata(
                    section.name(),
                    "ТабличнаяЧасть",
                    "tabular_section",
                    None,
                ),
            };
            member.source_extension = candidate.source_extension.clone();
            member
        }));
    }

    members.extend(module_export_members(
        db,
        EffectiveModuleRole::Object,
        mdo_type,
        object_name,
        None,
        req.position.map(|position| position.file_id),
    ));

    if let (Some(file_id), Some(type_id)) = (
        metadata_object_file(db, mdo_type, object_name),
        root_metadata_object_type(db, mdo_type, object_name),
    ) {
        members.extend(platform_receiver_members(db, file_id, type_id, req));
    }

    members
}

fn applied_reference_members(
    db: &RootDatabaseImpl,
    mdo_type: MdoType,
    object_name: &str,
    req: &SymbolInfoRequest,
) -> Vec<SymbolMember> {
    let mut members = Vec::new();
    let metadata = match req.position {
        Some(position) => {
            db.effective_metadata_members_for_file(position.file_id, mdo_type, object_name)
        }
        None => db.effective_metadata_members_across_roots(mdo_type, object_name),
    };
    if let Some(metadata) = metadata {
        members.extend(metadata.iter().filter_map(|candidate| {
            let EffectiveMetadataMemberValue::Attribute(attribute) = &candidate.member else {
                return None;
            };
            let mut member = static_metadata_member(
                db,
                attribute.name.clone(),
                "Реквизит",
                "attribute",
                &attribute.attr_type,
            );
            member.source_extension = candidate.source_extension.clone();
            Some(member)
        }));
    }
    if let (Some(file_id), Some(type_id)) = (
        metadata_object_file(db, mdo_type, object_name),
        root_metadata_ref_type(db, mdo_type, object_name),
    ) {
        members.extend(platform_receiver_members(db, file_id, type_id, req));
    }
    members
}

fn static_metadata_member(
    db: &RootDatabaseImpl,
    name: impl Into<String>,
    kind: impl Into<String>,
    member_kind: &'static str,
    attr_type: &AttributeType,
) -> SymbolMember {
    let mut member = SymbolMember::metadata(name, kind, member_kind, Some(attr_type.to_string()));
    member.type_variants = static_type_variants(db, attr_type);
    member
}

fn static_type_variants(
    db: &RootDatabaseImpl,
    attr_type: &AttributeType,
) -> Vec<SymbolTypeVariant> {
    if let AttributeType::Composite { types } = attr_type {
        return types.iter().flat_map(|ty| static_type_variants(db, ty)).collect();
    }

    let technical_name = match attr_type {
        AttributeType::String { .. } => Some("Строка".to_string()),
        AttributeType::Number { .. } => Some("Число".to_string()),
        AttributeType::Boolean => Some("Булево".to_string()),
        AttributeType::Date | AttributeType::DateTime => Some("Дата".to_string()),
        AttributeType::Ref { mdo_type, name } => root_metadata_ref_type(db, *mdo_type, name)
            .map(|type_id| kernel_type_label(db, type_id, Locale::Ru, false)),
        AttributeType::InformationRegisterRecordManager { .. } => Some(attr_type.to_string()),
        AttributeType::AnyRef => Some("ЛюбаяСсылка".to_string()),
        AttributeType::Uuid => Some("УникальныйИдентификатор".to_string()),
        AttributeType::ValueStorage => Some("ХранилищеЗначения".to_string()),
        AttributeType::DefinedType { name } => Some(format!("ОпределяемыйТип.{name}")),
        AttributeType::Platform(kind) => Some(kind.russian_name().to_string()),
        AttributeType::PlatformNamed(name) if !name.is_empty() => Some(name.clone()),
        AttributeType::AnyObjectRef { .. }
        | AttributeType::PlatformNamed(_)
        | AttributeType::UnknownNamed(_)
        | AttributeType::Unknown
        | AttributeType::Composite { .. } => None,
    };
    let resolution = if technical_name.is_some() { "static" } else { "unresolved" };
    vec![SymbolTypeVariant {
        presentation: attr_type.to_string(),
        technical_name,
        resolution,
        reason: (resolution == "unresolved").then(|| "static_type_unresolved".to_string()),
    }]
}

fn applied_manager_members(
    db: &RootDatabaseImpl,
    mdo_type: MdoType,
    object_name: &str,
    req: &SymbolInfoRequest,
) -> Vec<SymbolMember> {
    let mut members = module_export_members(
        db,
        EffectiveModuleRole::Manager,
        mdo_type,
        object_name,
        None,
        req.position.map(|position| position.file_id),
    );
    if let (Some(file_id), Some(type_id)) = (
        metadata_object_file(db, mdo_type, object_name),
        root_object_manager_type(db, mdo_type, object_name),
    ) {
        members.extend(platform_receiver_members(db, file_id, type_id, req));
    }
    members
}

fn module_export_members(
    db: &RootDatabaseImpl,
    role: EffectiveModuleRole,
    mdo_type: MdoType,
    object_name: &str,
    form_name: Option<&str>,
    visibility_file: Option<FileId>,
) -> Vec<SymbolMember> {
    let exports = effective_module_exports_query(
        db,
        CONFIG_SOURCE_ROOT,
        visibility_file,
        role,
        mdo_type,
        object_name.to_string(),
        form_name.map(str::to_owned),
    );
    let methods = exports.methods.iter().map(|candidate| {
        let params = candidate
            .method
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let keyword =
            if candidate.method.is_function { "Функция" } else { "Процедура" };
        let mut member = SymbolMember::callable(
            candidate.name.as_str(),
            "Метод",
            "method",
            format!("{keyword} {}({params}) Экспорт", candidate.name.as_str()),
            SymbolMemberOrigin::Module,
        );
        member.source_extension = candidate.source_extension.clone();
        member
    });
    let variables = exports.variables.iter().map(|candidate| {
        let mut member = SymbolMember::metadata(
            candidate.variable.name.as_str(),
            "Переменная",
            "property",
            Some("Неизвестно".to_string()),
        );
        member.origin = SymbolMemberOrigin::Module;
        member.source_extension = candidate.source_extension.clone();
        member
    });
    methods.chain(variables).collect()
}

fn platform_receiver_members(
    db: &RootDatabaseImpl,
    file_id: FileId,
    type_id: hir::TypeId,
    req: &SymbolInfoRequest,
) -> Vec<SymbolMember> {
    let receiver = HirType::from_id(db, file_id, type_id);
    let availability = MemberAvailabilityContext::from_request(db, req);
    let fields = receiver.fields().into_iter().filter_map(|field| {
        let (kind, member_kind) = match field.origin {
            HirFieldOrigin::StandardAttribute => ("СтандартныйРеквизит", "attribute"),
            HirFieldOrigin::PlatformProperty => ("Свойство", "property"),
            _ => return None,
        };
        let mut member = SymbolMember::metadata(
            field.name.as_str(),
            kind,
            member_kind,
            Some(type_label(db, field.ty, req.locale).unwrap_or_else(|| "Неизвестно".into())),
        );
        let technical_name = type_label(db, field.ty, Locale::Ru);
        member.type_variants = vec![SymbolTypeVariant {
            presentation: member.ty.clone().unwrap_or_else(|| "Неизвестно".to_string()),
            resolution: if technical_name.is_some() { "static" } else { "unresolved" },
            reason: technical_name.is_none().then(|| "static_type_unresolved".to_string()),
            technical_name,
        }];
        member.origin = SymbolMemberOrigin::Platform;
        member.availability = availability.evaluate(field.env, true);
        Some(member)
    });
    let methods = receiver.methods().into_iter().map(|method| {
        let params =
            method.params.iter().map(|param| param.name.as_str()).collect::<Vec<_>>().join(", ");
        let mut member = SymbolMember::callable(
            method.name.as_str(),
            "Метод",
            "method",
            format!("{}({params})", method.name.as_str()),
            SymbolMemberOrigin::Platform,
        );
        member.availability = availability.evaluate(method.env, true);
        member
    });
    fields.chain(methods).collect()
}

#[derive(Debug, Clone, Copy)]
struct MemberAvailabilityContext {
    has_position: bool,
    caller: EnvFlags,
    checked: EnvFlags,
    platform_catalog_complete: bool,
}

impl MemberAvailabilityContext {
    fn from_request(db: &RootDatabaseImpl, req: &SymbolInfoRequest) -> Self {
        let caller = req
            .position
            .and_then(|position| {
                crate::name_lookup::offset_for_line_col(
                    db,
                    position.file_id,
                    position.line,
                    position.column,
                )
                .map(|offset| execution_environment_at(db, position.file_id, offset))
            })
            .unwrap_or(EnvFlags::EMPTY);
        let target = db.target_platform_version();
        Self {
            has_position: req.position.is_some(),
            caller,
            checked: db.env_options().checked_environments,
            platform_catalog_complete: matches!(
                PlatformGlobalCatalog::instance().status_for_target(target.as_deref()),
                PlatformCatalogStatus::Complete
            ),
        }
    }

    fn evaluate(
        self,
        declarative: Option<EnvFlags>,
        from_platform_catalog: bool,
    ) -> SymbolMemberAvailability {
        let contexts = declarative.map(env_contexts);
        if !self.has_position {
            return SymbolMemberAvailability::not_evaluated(contexts);
        }
        let Some(declarative) = declarative else {
            return SymbolMemberAvailability {
                contexts,
                context_status: SymbolMemberContextStatus::Unknown,
                reason: Some("declarative_context_unknown".to_string()),
            };
        };
        if from_platform_catalog && !self.platform_catalog_complete {
            return SymbolMemberAvailability {
                contexts,
                context_status: SymbolMemberContextStatus::Unknown,
                reason: Some("platform_catalog_unverified".to_string()),
            };
        }
        if self.caller.is_empty() {
            return SymbolMemberAvailability {
                contexts,
                context_status: SymbolMemberContextStatus::Unknown,
                reason: Some("module_context_unknown".to_string()),
            };
        }
        let unavailable = !(self.caller.without(declarative) & self.checked).is_empty();
        SymbolMemberAvailability {
            contexts,
            context_status: if unavailable {
                SymbolMemberContextStatus::Unavailable
            } else {
                SymbolMemberContextStatus::Available
            },
            reason: None,
        }
    }
}

fn env_contexts(environment: EnvFlags) -> Vec<&'static str> {
    environment
        .iter()
        .map(|environment| match environment {
            EnvFlags::THIN_CLIENT => "thin_client",
            EnvFlags::WEB_CLIENT => "web_client",
            EnvFlags::THICK_CLIENT_MANAGED => "thick_client_managed",
            EnvFlags::THICK_CLIENT_ORDINARY => "thick_client_ordinary",
            EnvFlags::SERVER => "server",
            EnvFlags::MOBILE_CLIENT => "mobile_client",
            EnvFlags::EXTERNAL_CONNECTION => "external_connection",
            _ => unreachable!(),
        })
        .collect()
}

fn metadata_object_file(
    db: &RootDatabaseImpl,
    mdo_type: MdoType,
    object_name: &str,
) -> Option<FileId> {
    let paths = db.all_config_paths();
    let base_first = paths
        .iter()
        .filter(|(label, _)| label.is_none())
        .chain(paths.iter().filter(|(label, _)| label.is_some()));
    for (_, root) in base_first {
        let Some(listing) = db.metadata_listing(&root.to_string_lossy()) else { continue };
        if let Some(entry) = listing
            .entries(db)
            .iter()
            .find(|entry| entry.kind == mdo_type && name_eq(&entry.name, object_name))
        {
            return Some(entry.main);
        }
    }
    None
}

fn applied_facet_member_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    facet: AppliedFacet,
    object: &str,
    member: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let mut card = applied_facet_card(db, symbol, facet, object, req)?;
    card.members = collect_applied_members(card.members, Some(member));
    if card.members.is_empty() {
        return None;
    }
    card.kind = "member candidates";
    card.signature = Some(format!("{} candidate(s)", card.members.len()));
    Some(card)
}

fn collect_applied_members(
    candidates: Vec<SymbolMember>,
    exact_name: Option<&str>,
) -> Vec<SymbolMember> {
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| exact_name.is_none_or(|name| name_eq(&candidate.name, name)))
        .collect();
    candidates.sort_by_key(|candidate| {
        (
            candidate.name.to_lowercase(),
            match candidate.origin {
                SymbolMemberOrigin::Metadata => 0,
                SymbolMemberOrigin::Module => 1,
                SymbolMemberOrigin::Platform => 2,
            },
            candidate.member_kind,
            candidate.source_extension.clone(),
            candidate.kind.clone(),
        )
    });
    candidates
}

/// `<MdoType>.<Object>.<Member>`: a metadata attribute, or an object/manager module method.
fn resolve_triple(
    db: &RootDatabaseImpl,
    symbol: &str,
    a: &str,
    b: &str,
    c: &str,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    if let Some(facet) = parse_applied_facet(a) {
        return applied_facet_member_card(db, symbol, facet, b, c, req);
    }
    let mdo_type = parse_mdo_type(a)?;

    // Attribute / tabular field on the metadata object (type + ownership).
    if let Some(card) = attribute_card(db, symbol, mdo_type, b, c) {
        return Some(card);
    }

    let module_index = db.module_index(CONFIG_SOURCE_ROOT);
    let object = Name::new(b);
    let method = Name::new(c);

    // Object / record-set module method → routed through LocalMethod on the def module.
    //
    // Ranked by config root, not taken from the path-derived winner: an extension adopts the
    // base module's relative path, and when its directory sorts first the index answers with
    // the extension body — where a base-only method is simply absent, so the card reports the
    // method as not existing at all. The same rule already governs common modules; a module
    // of another kind is no less a module.
    for module_file in declaring_bodies(
        db,
        [
            module_index.object_module_candidates(mdo_type, &object),
            module_index.record_set_candidates(mdo_type, &object),
        ]
        .concat(),
        &method,
    ) {
        let module_id = ModuleId::new(module_file);
        let callee = CalleeKind::LocalMethod { module_id, method: method.clone() };
        if let Some(sigs) = build_signature(db, module_file, &callee) {
            if let Some(sig) = sigs.first() {
                if !sig.is_export {
                    continue;
                }
                let container = SymbolContainer {
                    kind: mdo_type.russian_name().to_string(),
                    name: b.to_string(),
                    context: None,
                };
                return Some(card_from_method_sig(db, symbol, sig, Some(container), req));
            }
        }
    }

    // Manager module method (`Справочники.Товары.СоздатьЭлемент` style overrides).
    // Routed through `LocalMethod` on the body this function chose, exactly like the object
    // module above. `ManagerModuleMethod` looks the module up by name again, and that lookup
    // is the path-order one — the ranking computed here would be thrown away, leaving the base
    // declaration invisible behind an earlier-sorting extension.
    for module_file in declaring_bodies(
        db,
        module_index.manager_candidates(ManagerType::from_mdo_type(mdo_type)?, &object).to_vec(),
        &method,
    ) {
        let callee = CalleeKind::LocalMethod {
            module_id: ModuleId::new(module_file),
            method: method.clone(),
        };
        if let Some(sigs) = build_signature(db, module_file, &callee) {
            if let Some(sig) = sigs.first() {
                let container = SymbolContainer {
                    kind: format!("Менеджер{}", mdo_type.russian_name()),
                    name: b.to_string(),
                    context: None,
                };
                return Some(card_from_method_sig(db, symbol, sig, Some(container), req));
            }
        }
    }

    None
}

// --- metadata object / attribute cards --------------------------------------------------

fn object_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    mdo_type: MdoType,
    name: &str,
    _req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    // Registers live in a separate substrate (`Register`, not `MetadataObject`) with
    // dimensions/resources/attributes rather than plain attributes + tabular sections.
    if mdo_type.is_register() {
        return register_card(db, symbol, mdo_type, name);
    }
    let obj = db.resolve_metadata_object_across_roots(mdo_type, name)?;
    let mut card = SymbolInfoCard::empty(symbol.to_string(), "metadata object");
    card.container = Some(SymbolContainer {
        kind: mdo_type.russian_name().to_string(),
        name: obj.name.clone(),
        context: None,
    });
    let mut members = Vec::new();
    for attr in &obj.attributes {
        members.push(static_metadata_member(
            db,
            attr.name.clone(),
            "Реквизит",
            "attribute",
            &attr.attr_type,
        ));
    }
    for ts in &obj.tabular_sections {
        members.push(SymbolMember::metadata(ts.name(), "ТабличнаяЧасть", "tabular_section", None));
    }
    card.signature = Some(format!(
        "{}.{} — {} реквизит(ов), {} табличн. част(и)",
        mdo_type.russian_name(),
        obj.name,
        obj.attributes.len(),
        obj.tabular_sections.len()
    ));
    card.members = members;
    Some(card)
}

/// A whole-register card: dimensions, resources, and attributes as members.
fn register_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    mdo_type: MdoType,
    name: &str,
) -> Option<SymbolInfoCard> {
    let reg = db.resolve_register_across_roots(mdo_type, name)?;
    let mut card = SymbolInfoCard::empty(symbol.to_string(), "metadata object");
    card.container = Some(SymbolContainer {
        kind: mdo_type.russian_name().to_string(),
        name: reg.name().to_string(),
        context: None,
    });
    let mut members = Vec::new();
    for d in reg.dimensions() {
        members.push(register_symbol_member(
            db,
            d.name(),
            "Измерение",
            d.attr_type(),
            d.type_str(),
        ));
    }
    for r in reg.resources() {
        members.push(register_symbol_member(db, r.name(), "Ресурс", r.attr_type(), r.type_str()));
    }
    for a in reg.attributes() {
        members.push(register_symbol_member(db, a.name(), "Реквизит", a.attr_type(), a.type_str()));
    }
    card.signature = Some(format!(
        "{}.{} — {} измерен., {} ресурс., {} реквизит.",
        mdo_type.russian_name(),
        reg.name(),
        reg.dimensions().len(),
        reg.resources().len(),
        reg.attributes().len()
    ));
    card.members = members;
    Some(card)
}

/// A single register member (dimension/resource/attribute): its type + owning register.
fn register_member_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    mdo_type: MdoType,
    object: &str,
    member: &str,
) -> Option<SymbolInfoCard> {
    let reg = db.resolve_register_across_roots(mdo_type, object)?;
    let found = reg
        .dimensions()
        .iter()
        .find(|d| name_eq(d.name(), member))
        .map(|d| ("Измерение", register_member_type(d.attr_type(), d.type_str())))
        .or_else(|| {
            reg.resources()
                .iter()
                .find(|r| name_eq(r.name(), member))
                .map(|r| ("Ресурс", register_member_type(r.attr_type(), r.type_str())))
        })
        .or_else(|| {
            reg.attributes()
                .iter()
                .find(|a| name_eq(a.name(), member))
                .map(|a| ("Реквизит", register_member_type(a.attr_type(), a.type_str())))
        })?;
    let mut card = SymbolInfoCard::empty(symbol.to_string(), "attribute");
    card.return_type = found.1;
    card.container = Some(SymbolContainer {
        kind: format!("{} ({})", mdo_type.russian_name(), found.0),
        name: reg.name().to_string(),
        context: None,
    });
    Some(card)
}

/// The displayable type of a register member: the structured `ОписаниеТипов` when parsed,
/// otherwise the raw type string, dropping the empty case.
fn register_member_type(
    attr_type: Option<&bsl_metadata::AttributeType>,
    type_str: &str,
) -> Option<String> {
    if let Some(t) = attr_type {
        return Some(t.to_string());
    }
    (!type_str.is_empty()).then(|| type_str.to_string())
}

fn register_symbol_member(
    db: &RootDatabaseImpl,
    name: &str,
    kind: &str,
    attr_type: Option<&AttributeType>,
    type_str: &str,
) -> SymbolMember {
    attr_type.map_or_else(
        || SymbolMember::metadata(name, kind, "attribute", register_member_type(None, type_str)),
        |attr_type| static_metadata_member(db, name, kind, "attribute", attr_type),
    )
}

fn attribute_card(
    db: &RootDatabaseImpl,
    symbol: &str,
    mdo_type: MdoType,
    object: &str,
    member: &str,
) -> Option<SymbolInfoCard> {
    if mdo_type.is_register() {
        return register_member_card(db, symbol, mdo_type, object, member);
    }
    let obj = db.resolve_metadata_object_across_roots(mdo_type, object)?;

    if let Some(attr) = obj.attributes.iter().find(|a| name_eq(&a.name, member)) {
        let mut card = SymbolInfoCard::empty(symbol.to_string(), "attribute");
        card.return_type = Some(attr.attr_type.to_string());
        card.container = Some(SymbolContainer {
            kind: mdo_type.russian_name().to_string(),
            name: obj.name.clone(),
            context: None,
        });
        return Some(card);
    }

    // A tabular-section name, or a `TabularSection.Field` under this object.
    for ts in &obj.tabular_sections {
        if name_eq(ts.name(), member) {
            let mut card = SymbolInfoCard::empty(symbol.to_string(), "tabular section");
            card.container = Some(SymbolContainer {
                kind: mdo_type.russian_name().to_string(),
                name: obj.name.clone(),
                context: None,
            });
            card.members = ts
                .attributes()
                .iter()
                .map(|a| {
                    static_metadata_member(db, a.name(), "Реквизит", "attribute", a.attr_type())
                })
                .collect();
            return Some(card);
        }
    }

    None
}

// --- positional (local / parameter) resolution ------------------------------------------

fn resolve_position(
    db: &RootDatabaseImpl,
    pos: SymbolPosition,
    req: &SymbolInfoRequest,
) -> Option<SymbolInfoCard> {
    let offset = crate::name_lookup::offset_for_line_col(db, pos.file_id, pos.line, pos.column)?;
    let parse = db.parse(pos.file_id);
    let root = parse.syntax_node();
    let token = root.token_at_offset(offset).right_biased()?;

    let sema = Semantics::new(db);
    let def = sema.resolve_name_to_definition(pos.file_id, &token)?;

    let name = def.name(db).map(|n| n.as_str().to_string()).unwrap_or_else(|| token.text().into());
    let kind = kind_for_definition(&def);
    let mut card = SymbolInfoCard::empty(name.clone(), kind);
    card.symbol = name;
    card.signature = Some(def.label(db));

    if req.sections.doc {
        card.doc = def.docs(db).and_then(|d| d.purpose.clone());
    }

    if req.sections.type_ {
        if let Definition::Method(method_id) = &def {
            card.return_type = method_return_type_label(db, *method_id, req.locale);
        }
    }

    // Outside the section filter, exactly where the named path puts it: the node id is a
    // property of the symbol, and `card_from_method_sig` publishes it whatever sections were
    // asked for. Computing it under `definition` would make a narrowed `include` decide
    // whether two tools name the same node.
    if let (Definition::Method(_), Some(module)) = (&def, def.module(db)) {
        if let Some(name) = def.name(db) {
            card.graph_id = crate::graph::graph_id_of_method(
                db,
                module.file_id,
                name.as_str(),
                req.workspace_root.as_ref(),
            );
        }
    }

    if req.sections.definition {
        if let (Some(module), Some(range)) = (def.module(db), def.source_range(db)) {
            card.set_base_definition(
                def_from_file_line(
                    db,
                    module.file_id,
                    range.start(),
                    DefinitionRanges { name: def.name_range(db), enclosing: Some(range) },
                    true,
                ),
                root_label(db, module.file_id),
            );
            // The same method, reached by position instead of by name, is the same method:
            // without this the card a client gets depends on how it addressed the symbol.
            if let Some(name) = def.name(db) {
                card.definitions.extend(weaving_sites(db, &name, module.file_id));
            }
        } else {
            // Locals and parameters have no method-level source range, but their POSITION is
            // known — it is how the caller reached them. Publishing a location without ranges
            // here would mean "somewhere in this file" while the legacy `line` names the line.
            card.set_base_definition(
                def_from_file_line(
                    db,
                    pos.file_id,
                    offset,
                    DefinitionRanges { name: Some(token.text_range()), enclosing: None },
                    true,
                ),
                root_label(db, pos.file_id),
            );
        }
    }

    Some(card)
}

fn kind_for_definition(def: &Definition) -> &'static str {
    match def {
        Definition::Method(_) => "method",
        Definition::Variable(_) => "module variable",
        Definition::Parameter { .. } => "parameter",
        Definition::Local { .. } => "local variable",
        Definition::BuiltinFunction(_) | Definition::BuiltinMethodHandle { .. } => {
            "platform function"
        }
        Definition::Module(_) => "module",
        Definition::MdoObject { .. } | Definition::MdoCollectionType(_) => "metadata object",
        Definition::MdoManagerModule { .. } => "manager module",
        Definition::VirtualTableField { .. } => "field",
        Definition::Unresolved => "unresolved",
    }
}

// --- shared card assembly ---------------------------------------------------------------

fn card_from_method_sig(
    db: &RootDatabaseImpl,
    symbol: &str,
    sig: &SymbolSignature,
    container: Option<SymbolContainer>,
    req: &SymbolInfoRequest,
) -> SymbolInfoCard {
    let kind = match sig.kind {
        MethodKind::Function => "function",
        MethodKind::Procedure => "procedure",
    };
    let mut card = SymbolInfoCard::empty(symbol.to_string(), kind);
    card.container = container;
    card.signature = Some(signature_string(sig));
    card.graph_id = method_graph_id(db, sig, req.workspace_root.as_ref());

    if req.sections.doc {
        card.doc = sig.purpose.clone().or_else(|| sig.description.clone());
    }
    if req.sections.type_ {
        card.return_type = return_type_label(db, sig, req.locale);
    }
    if req.sections.definition {
        if let Some(method_id) = sig.method_id {
            card.set_base_definition(
                def_for_method(db, method_id),
                root_label(db, method_id.module.file_id),
            );
            // Collected here rather than in each resolver branch: a common module, an object
            // module, a manager and a record set all arrive at this one function, and a
            // collector wired into one branch answers the other three with the base site
            // alone. `sig.name_russian` is the DECLARED spelling, which is what an
            // interceptor's annotation names.
            card.definitions.extend(weaving_sites(
                db,
                &Name::new(&sig.name_russian),
                method_id.module.file_id,
            ));
        }
    }
    card
}

/// The durable call-graph id for a resolved user method, encoded with the SAME path-derived
/// grammar the graph builder uses (`method/<scope>/<name>`), so the adapter can read its
/// usages by id instead of round-tripping the qualified name through the graph's fuzzy
/// resolver (which does not match a dotted `Module.Method` against a `method/.../...` id).
/// `None` when the method has no addressable module (platform members, file-layout modules).
fn method_graph_id(
    db: &RootDatabaseImpl,
    sig: &SymbolSignature,
    workspace_root: Option<&crate::graph::StripRoot>,
) -> Option<String> {
    let method_id = sig.method_id?;
    crate::graph::graph_id_of_method(
        db,
        method_id.module.file_id,
        &sig.name_russian,
        workspace_root,
    )
}

fn card_from_platform_sig(
    symbol: &str,
    sig: &SymbolSignature,
    req: &SymbolInfoRequest,
) -> SymbolInfoCard {
    let kind = match sig.kind {
        MethodKind::Function => "platform function",
        MethodKind::Procedure => "platform procedure",
    };
    let mut card = SymbolInfoCard::empty(symbol.to_string(), kind);
    card.signature = Some(signature_string(sig));
    if req.sections.doc {
        // Point at `syntax_help` for the full reference rather than duplicating it here.
        card.doc = sig
            .purpose
            .clone()
            .or_else(|| sig.description.clone())
            .map(|d| format!("{d}\n\nПолная справка → syntax_help"))
            .or_else(|| Some("Полная справка → syntax_help".to_string()));
    }
    if req.sections.type_ && !sig.returns.is_empty() {
        card.return_type = Some(join_type_refs(&sig.returns));
    }
    card
}

fn signature_string(sig: &SymbolSignature) -> String {
    // The real BSL declaration (`Функция Имя(П) Экспорт`) — the return type is surfaced in the
    // dedicated `return_type` field, not appended here as the signature-help view would.
    render_declaration(sig)
}

fn return_type_label(
    db: &RootDatabaseImpl,
    sig: &SymbolSignature,
    locale: Locale,
) -> Option<String> {
    if !sig.returns.is_empty() {
        return Some(join_type_refs(&sig.returns));
    }
    let method_id = sig.method_id?;
    method_return_type_label(db, method_id, locale)
}

fn method_return_type_label(
    db: &RootDatabaseImpl,
    method_id: MethodId,
    locale: Locale,
) -> Option<String> {
    let ty = method_return_type(db, method_id);
    type_label(db, ty, locale)
}

fn type_label(db: &RootDatabaseImpl, ty: hir::TypeId, locale: Locale) -> Option<String> {
    let label = kernel_type_label(db, ty, locale, false);
    (!label.is_empty()).then_some(label)
}

fn join_type_refs(returns: &[symbol_info::TypeRef]) -> String {
    returns.iter().map(|t| t.russian.as_str()).collect::<Vec<_>>().join(" | ")
}

// --- helpers ----------------------------------------------------------------------------

/// Interpret a metadata-object-type keyword, accepting both singular (`Справочник`) and
/// plural (`Справочники`) Russian/English forms without regex.
fn parse_mdo_type(s: &str) -> Option<MdoType> {
    s.parse::<MdoType>().ok().or_else(|| MdoType::from_plural(s))
}

/// Unicode-aware case-insensitive name match (BSL identifiers are case-insensitive and
/// bilingual identifiers include Cyrillic, which ASCII folding would miss).
fn name_eq(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn module_context(db: &RootDatabaseImpl, module_id: ModuleId) -> Option<String> {
    let meta = db.module_metadata(module_id);
    let ctx = meta
        .execution_context
        .or_else(|| meta.common_module.as_ref().map(|cm| compute_execution_context(cm)))?;
    Some(execution_context_label(ctx).to_string())
}

fn execution_context_label(ctx: ExecutionContext) -> &'static str {
    match ctx {
        ExecutionContext::Server => "Сервер",
        ExecutionContext::ServerCall => "Сервер (вызов сервера)",
        ExecutionContext::Client => "Клиент",
        ExecutionContext::ClientServer => "Клиент и сервер",
        ExecutionContext::ExternalConnection => "Внешнее соединение",
        ExecutionContext::Unknown => "Не определён",
    }
}

/// Bodies that DECLARE `method`, lowest config-root rank first.
///
/// The path-derived index cannot rank roots, so a caller that must not lose the base
/// declaration orders the candidates itself. Bodies that do not declare the method at all are
/// dropped: an extension body sharing the module's path is not an answer about a method it
/// never had.
fn declaring_bodies(db: &RootDatabaseImpl, candidates: Vec<FileId>, method: &Name) -> Vec<FileId> {
    let mut bodies = candidates;
    bodies.sort_by_key(|&file| {
        db.config_root_rank_and_label(file).map(|(rank, _)| rank).unwrap_or(usize::MAX)
    });
    // Exported, not merely declared. A private method of the base body is not an answer about
    // a name asked from outside, and keeping it here would let it stand in front of an
    // EXPORTED body of the same method in an extension — the caller then reports the method as
    // absent while a usable one exists one root away.
    bodies.retain(|&file| {
        db.symbol_tree(ModuleId::new(file))
            .find_method(method)
            .is_some_and(|symbol| symbol.is_export)
    });
    bodies
}

/// The body of a common module whose config root ranks lowest.
///
/// Falls back to the path-derived answer when the candidate list is empty, so a module the
/// index knows only through the single-answer map is still found.
fn ranked_module_body(
    db: &RootDatabaseImpl,
    module_index: &hir::ModuleIndex,
    name: &Name,
) -> Option<vfs::FileId> {
    let mut bodies = module_index.common_module_candidates(name).to_vec();
    if bodies.is_empty() {
        bodies.extend(module_index.resolve_common_module(name));
    }
    bodies.sort_by_key(|&file| {
        db.config_root_rank_and_label(file).map(|(rank, _)| rank).unwrap_or(usize::MAX)
    });
    bodies.first().copied()
}

/// The label of the root a file lives in, or `None` for the configuration itself.
fn root_label(db: &RootDatabaseImpl, file: FileId) -> Option<String> {
    db.config_root_rank_and_label(file).and_then(|(_, label)| label)
}

/// Every interceptor an extension weaves onto `method` of a common module.
///
/// Ordered by config-root rank, so a client reads the sites in the order their roots stack
/// rather than in whatever order the paths happened to sort. The base declaration is not
/// among them — it is the site the card already carries.
fn weaving_sites(
    db: &RootDatabaseImpl,
    method: &Name,
    base_file: FileId,
) -> Vec<SymbolDefinitionSite> {
    let module_index = db.module_index(CONFIG_SOURCE_ROOT);
    // Asked of the file, not of the module's kind. Weaving happens between two BODIES of one
    // module, and an extension can weave onto an object module or a manager exactly as it
    // weaves onto a common one — a collector that knows only common modules answers a card
    // for a catalog's `ПриЗаписи` with the base body alone and calls that the whole truth.
    let mut bodies = module_index.sibling_bodies(base_file);
    bodies.sort_by_key(|&file| {
        db.config_root_rank_and_label(file).map(|(rank, _)| rank).unwrap_or(usize::MAX)
    });

    // The base method itself, so an annotation that cannot apply to it is not published as
    // though it ran. 1C does not allow `&Перед` / `&После` on an extended FUNCTION, and the
    // effective-exports calculation drops such an interceptor; a card that keeps it tells a
    // client that code runs around this function when none does.
    //
    // A signature MISMATCH is deliberately not filtered here. That one is a mistake in the
    // configuration rather than an inapplicable annotation, and a card that shows the site is
    // more useful than one that silently omits it.
    let base_symbols = db.symbol_tree(ModuleId::new(base_file));
    let base_symbol = base_symbols.find_method(method);

    let mut sites = Vec::new();
    for file in bodies {
        let parse = db.parse(file);
        let symbols = db.symbol_tree(ModuleId::new(file));
        let label = db.config_root_rank_and_label(file).and_then(|(_, label)| label);
        for candidate in symbols.methods() {
            let Some(node) = candidate.syntax_node(&parse) else {
                continue;
            };
            let Some(interception) = hir::interceptor_target(&node) else {
                continue;
            };
            if !name_eq(&interception.target, method.as_str()) {
                continue;
            }
            // The one predicate the effective-exports calculation uses, not a subset of it:
            // an interceptor that does not run is not a place where anything happens, and a
            // card publishing it says code runs around this method when none does.
            if let Some(base) = base_symbol {
                if !hir::interception_effective(interception.kind, candidate, base) {
                    continue;
                }
            }
            let role = match interception.kind {
                hir::InterceptionKind::Around => DefinitionRole::Instead,
                hir::InterceptionKind::Before => DefinitionRole::Before,
                hir::InterceptionKind::After => DefinitionRole::After,
            };
            let Some(definition) = def_from_file_line(
                db,
                file,
                candidate.source_range.start(),
                DefinitionRanges {
                    name: Some(candidate.name_range),
                    enclosing: Some(candidate.source_range),
                },
                true,
            ) else {
                continue;
            };
            sites.push(SymbolDefinitionSite { definition, role, source_extension: label.clone() });
        }
    }
    sites
}

fn def_for_method(db: &RootDatabaseImpl, method_id: MethodId) -> Option<SymbolDefinition> {
    let def = Definition::Method(method_id);
    let enclosing = def.source_range(db)?;
    def_from_file_line(
        db,
        method_id.module.file_id,
        enclosing.start(),
        DefinitionRanges { name: def.name_range(db), enclosing: Some(enclosing) },
        true,
    )
}

fn def_from_file_line(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: syntax::TextSize,
    ranges: DefinitionRanges,
    include: bool,
) -> Option<SymbolDefinition> {
    if !include {
        return None;
    }
    let text = db.file_text(file_id);
    let line_index = LineIndex::new(&text);
    let line_col = line_index.line_col(offset);
    let snippet = crate::name_lookup::line_text(db, file_id, line_col.line)
        .map(|line| line.trim_end().to_owned());
    let to_line_col = |range: Option<TextRange>| {
        range.and_then(|range| line_index.utf16_line_col_range(&text, range))
    };
    Some(SymbolDefinition {
        path: file_path(db, file_id),
        line: line_col.line + 1,
        snippet,
        name_range: to_line_col(ranges.name),
        enclosing_range: to_line_col(ranges.enclosing),
    })
}

pub(crate) fn file_path(db: &RootDatabaseImpl, file_id: FileId) -> Option<String> {
    let source_root = db.source_root_input(CONFIG_SOURCE_ROOT).root(db);
    let vfs_path = source_root.file_set().path_for_file(&file_id)?;
    Some(vfs_path.as_path().to_str()?.replace('\\', "/"))
}

#[cfg(test)]
mod applied_member_collector_tests {
    use super::*;

    fn candidates() -> Vec<SymbolMember> {
        let metadata = SymbolMember::metadata(
            "Состояние",
            "Реквизит",
            "attribute",
            Some("Строка".to_string()),
        );
        let mut module = SymbolMember::metadata(
            "состояние",
            "Переменная",
            "property",
            Some("Неизвестно".to_string()),
        );
        module.origin = SymbolMemberOrigin::Module;
        module.source_extension = Some("РасширениеА".to_string());
        let platform = SymbolMember::callable(
            "Состояние",
            "Метод",
            "method",
            "Состояние()",
            SymbolMemberOrigin::Platform,
        );
        let other =
            SymbolMember::metadata("Артикул", "Реквизит", "attribute", Some("Строка".to_string()));
        vec![platform, other, module, metadata]
    }

    #[test]
    fn full_and_exact_collection_share_stable_source_preserving_order() {
        let full = collect_applied_members(candidates(), None);
        assert_eq!(full.len(), 4);
        assert_eq!(full[0].name, "Артикул");
        assert_eq!(full[1].origin, SymbolMemberOrigin::Metadata);
        assert_eq!(full[2].origin, SymbolMemberOrigin::Module);
        assert_eq!(full[2].member_kind, "property");
        assert_eq!(full[3].origin, SymbolMemberOrigin::Platform);

        let exact = collect_applied_members(candidates(), Some("СОСТОЯНИЕ"));
        assert_eq!(exact.len(), 3, "same-name candidates from every source survive");
        assert_eq!(
            exact.iter().map(|m| m.origin).collect::<Vec<_>>(),
            [
                SymbolMemberOrigin::Metadata,
                SymbolMemberOrigin::Module,
                SymbolMemberOrigin::Platform,
            ]
        );
    }

    #[test]
    fn exact_collection_reports_a_miss_as_an_empty_result() {
        assert!(collect_applied_members(candidates(), Some("НетТакого"),).is_empty());
    }

    #[test]
    fn composite_static_type_keeps_each_machine_variant() {
        let db = RootDatabaseImpl::new();
        let variants = static_type_variants(
            &db,
            &AttributeType::Composite {
                types: vec![
                    AttributeType::Number { precision: 10, scale: 2 },
                    AttributeType::Ref {
                        mdo_type: MdoType::Catalog,
                        name: "Контрагенты".to_string(),
                    },
                ],
            },
        );

        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].technical_name.as_deref(), Some("Число"));
        assert_eq!(variants[0].resolution, "static");
        assert!(variants[1]
            .technical_name
            .as_deref()
            .is_some_and(|name| name.contains("СправочникСсылка.Контрагенты")));
    }

    #[test]
    fn availability_evaluates_client_server_without_filtering_members() {
        let context = |caller, platform_catalog_complete| MemberAvailabilityContext {
            has_position: true,
            caller,
            checked: EnvFlags::ALL,
            platform_catalog_complete,
        };
        let server = context(EnvFlags::SERVER, true).evaluate(Some(EnvFlags::SERVER), true);
        assert_eq!(server.context_status, SymbolMemberContextStatus::Available);
        let client = context(EnvFlags::THIN_CLIENT, true).evaluate(Some(EnvFlags::SERVER), true);
        assert_eq!(client.context_status, SymbolMemberContextStatus::Unavailable);

        let members = [server, client];
        assert_eq!(members.len(), 2, "availability annotates candidates; it never filters them");
    }

    #[test]
    fn availability_expands_generic_thick_and_reports_unknown_inputs() {
        let generic_thick =
            EnvFlags::from_platform_context(Some(&bsl_platform::ContextAvailability {
                thick_client: true,
                thin_client: false,
                web_client: false,
                server: false,
                mobile_client: false,
                external_connection: false,
            }));
        assert_eq!(env_contexts(generic_thick), ["thick_client_managed", "thick_client_ordinary"]);

        let no_catalog = MemberAvailabilityContext {
            has_position: true,
            caller: EnvFlags::SERVER,
            checked: EnvFlags::ALL,
            platform_catalog_complete: false,
        }
        .evaluate(Some(EnvFlags::SERVER), true);
        assert_eq!(no_catalog.context_status, SymbolMemberContextStatus::Unknown);
        assert_eq!(no_catalog.reason.as_deref(), Some("platform_catalog_unverified"));

        let no_position = MemberAvailabilityContext {
            has_position: false,
            caller: EnvFlags::EMPTY,
            checked: EnvFlags::ALL,
            platform_catalog_complete: true,
        }
        .evaluate(Some(EnvFlags::ALL), true);
        assert_eq!(no_position.context_status, SymbolMemberContextStatus::NotEvaluated);
        assert_eq!(no_position.contexts.as_ref().map(Vec::len), Some(7));

        let unknown_declaration = MemberAvailabilityContext {
            has_position: true,
            caller: EnvFlags::SERVER,
            checked: EnvFlags::ALL,
            platform_catalog_complete: true,
        }
        .evaluate(None, false);
        assert_eq!(unknown_declaration.context_status, SymbolMemberContextStatus::Unknown);
        assert!(unknown_declaration.contexts.is_none());
    }
}
