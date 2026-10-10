//! One name search for the whole product.
//!
//! Four independent implementations used to answer "what is called like this":
//! the graph's `resolve`, its MCP wrapper, `symbol_info`'s near-miss list and
//! `workspace/symbol`. Each had its own source, its own tiers and its own idea
//! of what a hit is worth, so the same query gave different answers and none of
//! them said what it had failed to look at.
//!
//! This module composes artefacts that already exist — no new index is built —
//! and owns the parts that must be decided once: which tier a match belongs to,
//! how candidates rank, which two hits are the same symbol, and what a provider
//! that could not answer contributes to the result. A provider only selects; it
//! never decides order or completeness.

use hir::{module_key_for_path, DefDatabase, ModuleKey, Name};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRootId, BSL_SOURCE_ROOT};
use ide_db::RootDatabaseImpl;
use salsa::Database;
use stdx::case::CaseExt;
use syntax::{TextRange, TextSize};
use vfs::FileId;

/// The source root holding `.bsl`. Extension sources are registered into it
/// alongside the base configuration, so one root covers every module.
const ROOT: SourceRootId = BSL_SOURCE_ROOT;

/// The source root holding metadata XML. A metadata object's place is its XML,
/// and that file is deliberately kept out of [`ROOT`] so the BSL iterators never
/// walk it — which also means a path lookup confined to [`ROOT`] can never place
/// an object of metadata.
const METADATA_ROOT: SourceRootId = ide_db::base_db::METADATA_SOURCE_ROOT;

/// Declare a closed vocabulary ONCE: the variants, their wire codes and the
/// list of them all come out of a single table.
///
/// A hand-written `ALL` beside the enum would be the one part a new variant is
/// not forced to join — `as_str` is an exhaustive match and the compiler
/// demands its branches, but an array of fixed length demands nothing. The
/// gates that publish the vocabulary would then keep passing while the tool
/// served a value the document never named.
macro_rules! name_vocabulary {
    (
        $(#[$enum_doc:meta])*
        $name:ident {
            $( $(#[$doc:meta])* $variant:ident => $code:literal; )+
        }
    ) => {
        $(#[$enum_doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$doc])* $variant, )+
        }

        impl $name {
            /// The whole vocabulary. Every place that PUBLISHES the list — a
            /// tool's `schema` action, the contract document — reads it here.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The wire code: what a consumer matches on.
            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $code, )+ }
            }

            /// The inverse, for a boundary that receives the code as text.
            pub fn from_code(code: &str) -> Option<Self> {
                match code { $( $code => Some(Self::$variant), )+ _ => None }
            }
        }
    };
}

name_vocabulary! {
    /// What kind of thing a candidate is.
    ///
    /// This is the only kind field in the answer. [`ide_db::SymbolKind`] cannot
    /// serve: it knows procedures, functions, variables and regions, and has no
    /// place for a metadata object, a form or a platform member — half of what
    /// this search returns. Keeping both would be two sources of truth about
    /// one fact, one of them knowingly incomplete. A consumer that needs an LSP
    /// kind maps this at its own boundary.
    ///
    /// Declaration order is part of the ranking: with the tier equal, an
    /// earlier category sorts first.
    NameCategory {
        /// A common module — callable by name from anywhere.
        CommonModule => "common_module";
        /// A module that is addressable as a whole but not by a BSL name: an
        /// object, manager or record-set module. Reached through its graph id.
        Module => "module";
        /// An exported procedure or function of some module.
        ModuleMethod => "module_method";
        /// An exported module-level variable.
        ModuleVariable => "module_variable";
        /// A configuration object: catalog, document, register, role, …
        MetadataObject => "metadata_object";
        /// A member of a metadata object: attribute, tabular section, …
        MetadataMember => "metadata_member";
        /// A form, or an item of one.
        Form => "form";
        /// A platform type, method, property or global function.
        PlatformMember => "platform_member";
    }
}

name_vocabulary! {
    /// Who supplied a candidate. Named in the answer so a consumer can tell
    /// "nothing matched" from "the index that would have known was not built".
    ProviderId {
        /// Module table derived from file paths; parses nothing.
        ModuleIndex => "module_index";
        /// The metadata listing loaded from configuration XML.
        MetadataListing => "metadata_listing";
        /// The platform singleton seeded from HBK dumps.
        Platform => "platform";
        /// Exported members of every module in the root.
        ModuleMembers => "module_members";
        /// Nodes of the built call graph.
        Graph => "graph";
    }
}

name_vocabulary! {
    /// What became of a provider during one lookup.
    ProviderState {
        /// Consulted, and it answered.
        Answered => "answered";
        /// Exists but is still being built. Retrying later can change the answer.
        NotReady => "not_ready";
        /// Not consulted, because the caller narrowed the question past
        /// everything this provider can supply. Not a gap in the answer.
        NotAsked => "not_asked";
        /// Absent from this configuration by construction. Not a gap either.
        Unavailable => "unavailable";
        /// Building it failed. Distinct from `not_ready` on purpose: retrying
        /// will not help, and calling the two the same advises useless waiting.
        Failed => "failed";
    }
}

name_vocabulary! {
    /// How a candidate's spelling matched the query.
    ///
    /// Declaration order IS rank order, and it runs from equality outward: the
    /// query spelled exactly, then case-folded, then equal to the last segment
    /// of a durable id, then merely begun, then merely contained.
    ///
    /// `Name` sits above `Prefix` because it is still an equality — the id
    /// `method/common/Настройки/Тест` IS named `Тест` — while a prefix only
    /// starts alike. Only the graph reports `Name` and only [`match_tier`]
    /// reports `Prefix`, so swapping the two demotes every graph hit below any
    /// longer resident name that happens to begin with the query, and a narrow
    /// limit then drops the exact answer.
    NameMatchTier {
        /// Spelled exactly as asked.
        Exact => "exact";
        /// Equal after bilingual case folding.
        CaseInsensitive => "case_insensitive";
        /// The query is the trailing segment of a durable identifier.
        Name => "name";
        /// Begins with the query.
        Prefix => "prefix";
        /// The query occurs somewhere inside.
        Substring => "substring";
    }
}

/// Cap on what `workspace/symbol` returns, so a broad query cannot flood the
/// editor. The protocol has no field for incompleteness, so the cut is invisible
/// to the client — [`NameLookupResult::truncated`] carries it to the log instead.
pub const WORKSPACE_SYMBOL_LIMIT: usize = 256;

/// Where a candidate is written.
///
/// Two ranges, as the location contract wants them: `range` selects the name
/// alone, `enclosing_range` the whole declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamePlace {
    pub file_id: FileId,
    /// The declared name. Absent where the thing has no declaration node of its
    /// own — a module, a metadata object's XML — where an empty range at the
    /// file start would claim a precision that is not there.
    pub range: Option<TextRange>,
    /// The whole declaration, absent for the same reason.
    pub enclosing_range: Option<TextRange>,
}

impl NamePlace {
    /// A whole file: the file is the answer, and there is no node to select.
    fn whole_file(file_id: FileId) -> Self {
        Self { file_id, range: None, enclosing_range: None }
    }

    fn declaration(file_id: FileId, name: TextRange, enclosing: TextRange) -> Self {
        Self { file_id, range: Some(name), enclosing_range: Some(enclosing) }
    }
}

/// A place as the location contract needs it: a workspace path and ranges in
/// UTF-16 code units.
///
/// The conversion lives here, next to the place it converts, so two adapters
/// cannot end up counting columns differently — the mistake the contract's
/// explicit `position_encoding` exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlace {
    pub path: Option<String>,
    pub range: Option<line_index::LineColRange>,
    pub enclosing_range: Option<line_index::LineColRange>,
}

pub fn resolve_place(db: &RootDatabaseImpl, place: &NamePlace) -> ResolvedPlace {
    resolve_file_range(db, place.file_id, place.range, place.enclosing_range)
}

/// The byte offset a `line` + `column` pair names, or `None` when the column runs past
/// the line — at a line end the token search would select the NEXT line's first token and
/// answer for a symbol the caller never pointed at.
///
/// `column` is counted in UTF-16 units, the unit every published column is counted in
/// (`position_encoding`). That makes this the exact inverse of [`resolve_file_range`], so a
/// caller may take a `start_character` out of any answer and hand it straight back — which
/// is what every surface now tells it to do. Walking Unicode scalars instead agrees on the
/// BMP, where all of BSL lives, and parts company on the one line that carries an emoji.
pub fn offset_for_line_col(
    db: &RootDatabaseImpl,
    file_id: FileId,
    line: u32,
    column: u32,
) -> Option<TextSize> {
    let text = db.file_text(file_id);
    // The memoised index, not a fresh one: the text anchor calls this once per line its
    // quote matched, and rebuilding the index each time would scan the whole file per
    // matching line — a cost that was a constant while only one position was ever resolved.
    let line_index =
        ide_db::RootDatabase::line_index(db, ide_db::base_db::FileIdInput::new(db, file_id));
    let line_start = line_index.try_line_start(line)?;
    let byte_in_line = line_index.utf16_col_to_byte_col(&text, line, column)?;
    let offset = line_start + TextSize::from(byte_in_line);

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let token = root.token_at_offset(offset).right_biased()?;
    if line_index.line_col(token.text_range().start()).line != line {
        return None;
    }
    Some(offset)
}

/// The same conversion for a place held as loose parts — a reference hit, which is not a
/// declaration and so is not a [`NamePlace`]. One implementation, so two surfaces cannot
/// end up counting columns differently.
pub fn resolve_file_range(
    db: &RootDatabaseImpl,
    file_id: FileId,
    range: Option<TextRange>,
    enclosing_range: Option<TextRange>,
) -> ResolvedPlace {
    // A place that names only its file needs no line index, and building one
    // means reading the whole file: a per-file histogram over a thousand files
    // would walk every byte of every one of them for an answer already in hand.
    if range.is_none() && enclosing_range.is_none() {
        return ResolvedPlace {
            path: workspace_path(db, file_id),
            range: None,
            enclosing_range: None,
        };
    }
    let text = db.file_text(file_id);
    let index = line_index::LineIndex::new(&text);
    let to_line_col =
        |range: Option<TextRange>| range.and_then(|r| index.utf16_line_col_range(&text, r));
    ResolvedPlace {
        path: workspace_path(db, file_id),
        range: to_line_col(range),
        enclosing_range: to_line_col(enclosing_range),
    }
}

/// The text of one line, its terminator excluded and nothing else stripped.
///
/// Reads the memoised line index over `db.file_text` — the same text every occurrence
/// offset was counted against, so a line quoted beside a range describes the revision that
/// range belongs to. Reading the file from disk instead would quote a revision the answer
/// is not signed with.
///
/// Trimming stays with the caller, and deliberately: a declaration card drops trailing
/// whitespace, while a preview keeps its leading indentation because the published columns
/// index it. One reader deciding that for both would rewrite bytes on a surface that never
/// asked.
pub fn line_text(db: &RootDatabaseImpl, file_id: FileId, line: u32) -> Option<String> {
    let text = db.file_text(file_id);
    let index =
        ide_db::RootDatabase::line_index(db, ide_db::base_db::FileIdInput::new(db, file_id));
    index
        .safe_line_str(text.as_ref(), line)
        // `line_range` stops one byte before the `\n`, so a CRLF line keeps its `\r`. The
        // terminator of such a line is `\r\n` whole, and a caller that trusted the promise
        // without trimming would publish half of it.
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
}

/// One line together with everything before it that can change how it must be masked, and
/// where the line begins inside that slice.
///
/// A secret is armed by a marker — `Пароль`, `Токен` — standing before its literal in the
/// same STATEMENT, and BSL wraps long assignments across lines freely. Handing a redactor one
/// physical line therefore hides the marker whenever the wrap falls between them, and the
/// literal goes out in clear text. `context` is the earliest byte a statement covering this
/// line can start at — the enclosing method, or the file.
///
/// The terminator and trailing whitespace are NOT stripped here: the caller needs the slice
/// to end exactly where the line does, and trims what it publishes.
pub fn line_with_context(
    db: &RootDatabaseImpl,
    file_id: FileId,
    line: u32,
    context: u32,
) -> Option<(String, usize)> {
    let text = db.file_text(file_id);
    let index =
        ide_db::RootDatabase::line_index(db, ide_db::base_db::FileIdInput::new(db, file_id));
    let range = index.line_range(line)?;
    let start = usize::from(range.start()).min(text.len());
    let end = usize::from(range.end()).min(text.len());
    let context = (context as usize).min(start);
    let slice = text.get(context..end)?;
    Some((slice.to_owned(), start - context))
}

/// The qualified name that addresses a symbol, when one does.
///
/// Only an exported module method has a spelling the surfaces accept back. A
/// name published for anything else would be an address that answers
/// `not_found`, and this module's rule is that a key some tool would refuse is
/// not published at all.
///
/// The name is built from the method's OWN module, never from wherever it was found.
pub fn qualified_method_symbol(
    db: &RootDatabaseImpl,
    symbol: &hir::SemanticSymbol,
) -> Option<String> {
    let definition = symbol.definition.as_ref()?;
    let hir::Definition::Method(method_id) = definition else { return None };
    if !definition.is_export(db) {
        return None;
    }
    // The module that DECLARES the method, which is where its qualified name comes from.
    // Spelling it from the file the caller pointed at would prefix a method of `Продажи`
    // with `Клиент` whenever the call site is elsewhere — an address that resolves to a
    // third method, or to nothing.
    let file_id = method_id.module.file_id;
    let source_root = db.source_root_input(ROOT).root(db);
    let path = source_root.file_set().path_for_file(&file_id)?;
    let key = module_key_for_path(&path.as_path().to_string_lossy())?;
    Some(method_symbol(&key, symbol.name.as_str()))
}

/// Ask the FILE which root it belongs to rather than assuming the source one.
///
/// A metadata object's place is its XML, and the XML is registered under the
/// metadata root, not among the `.bsl` sources — looking it up in the source
/// root returns nothing and the object arrives with `source_path_unavailable`
/// on exactly the category this search was extended to find.
pub(crate) fn workspace_path(db: &RootDatabaseImpl, file_id: FileId) -> Option<String> {
    let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
    let source_root = db.source_root_input(source_root_id).root(db);
    let vfs_path = source_root.file_set().path_for_file(&file_id)?;
    Some(vfs_path.as_path().to_str()?.replace('\\', "/"))
}

/// A platform member as `syntax_help` accepts it.
///
/// Platform types and properties have no file and no graph node, and
/// `symbol_info` resolves only global functions and type methods among them. A
/// type would therefore be found and then be unreachable — so the reference
/// that DOES reach it is published instead of dropping the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformRef {
    pub name: String,
    pub type_name: Option<String>,
}

/// How a candidate's `symbol` is spelled — recorded instead of spelled while
/// the answer is still being assembled.
///
/// A broad query matches tens of thousands of members and publishes a few
/// hundred. Spelling every match costs a path parse and a `format!` apiece,
/// and all but the survivors are then thrown away. This carries no data of its
/// own — it is `Copy` — so recording it is free, and the spelling happens once
/// the cut is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolSpelling {
    /// From the module that owns the candidate's file: `<Модуль>.<Метод>` for a
    /// common module, `<ВидMDO>.<Объект>.<Метод>` for the rest.
    OwningModule,
}

/// One hit, with every way to address it that actually works.
///
/// A key some tool would refuse is not published at all: the defect this module
/// exists to fix was a candidate list of durable ids that nothing accepted back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameCandidate {
    pub display: String,
    /// Accepted by `symbol_info`'s `symbol` parameter.
    pub symbol: Option<String>,
    /// Accepted by `graph action=node`.
    pub graph_id: Option<String>,
    /// Accepted by `syntax_help`.
    pub platform_ref: Option<PlatformRef>,
    pub place: Option<NamePlace>,
    pub category: NameCategory,
    pub match_tier: NameMatchTier,
    pub provider: ProviderId,
    /// A workspace path from a provider that knows where a thing is written but
    /// not the id the database files it under. Turned into a [`NamePlace`] on
    /// arrival, so nothing downstream sees two ways of saying where something
    /// is; left as `None` by providers that hand over a place directly.
    source_path: Option<String>,
    /// Private on purpose: it is bookkeeping for the assembly, not part of the
    /// answer. Filled in for the survivors before the result leaves
    /// [`lookup_names`], and `None` by the time a consumer sees it.
    spelling: Option<SymbolSpelling>,
}

impl NameCandidate {
    pub fn new(
        display: impl Into<String>,
        category: NameCategory,
        match_tier: NameMatchTier,
        provider: ProviderId,
    ) -> Self {
        Self {
            display: display.into(),
            source_path: None,
            symbol: None,
            graph_id: None,
            platform_ref: None,
            place: None,
            category,
            match_tier,
            provider,
            spelling: None,
        }
    }

    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    pub fn with_place(mut self, place: NamePlace) -> Self {
        self.place = Some(place);
        self
    }

    /// Spell the `symbol` later, from the module owning this candidate's file.
    fn spelled_by_owning_module(mut self) -> Self {
        self.spelling = Some(SymbolSpelling::OwningModule);
        self
    }

    /// A durable graph id, the key `graph action=node` accepts.
    pub fn with_graph_id(mut self, id: impl Into<String>) -> Self {
        self.graph_id = Some(id.into());
        self
    }

    /// Where this is written, as a path, for a provider that has no file id of
    /// its own. The lookup resolves it against the database the answer is being
    /// built from — the only place where a path and an id are the same file by
    /// construction rather than by resemblance.
    pub fn with_source_path(mut self, path: impl Into<String>) -> Self {
        self.source_path = Some(path.into());
        self
    }

    /// A candidate nothing can address is not worth publishing.
    pub fn is_addressable(&self) -> bool {
        self.symbol.is_some()
            || self.graph_id.is_some()
            || self.platform_ref.is_some()
            || self.place.is_some()
    }

    /// Which durable id an entity answers with when the graph holds it under
    /// more than one.
    ///
    /// A common module listed in a subsystem's `<Content>` gets an `mdo` node
    /// beside its module node. The module node is the callable identity that
    /// graph traversal follows; the membership endpoint yields to it, and says
    /// so here rather than by whichever provider happened to arrive first.
    fn graph_id_rank(id: &str) -> u8 {
        u8::from(id.starts_with("mdo/CommonModule/"))
    }

    /// Take from another row of the SAME thing every address this one lacks.
    ///
    /// Only gaps are filled: the row that got here first sorted better, so its
    /// tier and its provider are the answer's, and an address it already has is
    /// the one a consumer was going to get anyway.
    fn absorb_addresses(&mut self, other: NameCandidate) {
        self.symbol = self.symbol.take().or(other.symbol);
        self.graph_id = match (self.graph_id.take(), other.graph_id) {
            (Some(mine), Some(theirs)) => {
                Some(if Self::graph_id_rank(&theirs) < Self::graph_id_rank(&mine) {
                    theirs
                } else {
                    mine
                })
            }
            (mine, theirs) => mine.or(theirs),
        };
        self.platform_ref = self.platform_ref.take().or(other.platform_ref);
        // A place with ranges says more than a whole-file one, and the graph
        // hands over the file alone.
        if self.place.is_none_or(|place| place.range.is_none()) {
            if let Some(better) = other.place.filter(|p| p.range.is_some()) {
                self.place = Some(better);
            } else {
                self.place = self.place.or(other.place);
            }
        }
        self.spelling = self.spelling.or(other.spelling);
    }

    /// Identity for de-duplication: the ADDRESS, never the displayed name.
    ///
    /// Hundreds of modules declare `ПриСозданииНаСервере`; folding them into
    /// one because they share a name would delete the very answer that was
    /// asked for.
    ///
    /// The file is part of the identity, and that is the whole difference
    /// between two facts that look alike. The same common module reported by
    /// two providers is one file and folds; a common module of the same name in
    /// a configuration AND in an extension is two files and two answers — they
    /// share a `symbol` and nothing else, and keeping whichever happened to
    /// sort first would hide one of them by accident.
    fn identity(&self) -> (NameCategory, String, Option<FileId>) {
        if let Some(place) = self.place {
            return (self.category, self.display.fold_lower(), Some(place.file_id));
        }
        // A candidate with a place is identified by its NAME IN ITS FILE, not by
        // its `symbol`. Both say the same thing — a module holds one member of a
        // given name — and the name is already in hand, while the `symbol` has
        // to be spelled out. That is what lets the spelling wait until the cut
        // (see [`SymbolSpelling`]).

        let key = match (&self.symbol, &self.graph_id, &self.platform_ref) {
            (Some(symbol), ..) => symbol.fold_lower(),
            (None, Some(id), _) => id.fold_lower(),
            (None, None, Some(r)) => {
                format!("{}.{}", r.type_name.as_deref().unwrap_or(""), r.name).fold_lower()
            }
            (None, None, None) => self.display.fold_lower(),
        };
        (self.category, key, None)
    }
}

/// What one provider found, and how much of it there was.
///
/// `total` is counted BEFORE the provider's own limit. Without it a provider
/// that capped its own output would make the merge report an answer as
/// exhaustive while hundreds of matches were dropped — exactly the silent
/// truncation this whole exercise is against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHits {
    pub candidates: Vec<NameCandidate>,
    pub total: usize,
}

impl ProviderHits {
    pub fn new(candidates: Vec<NameCandidate>, total: usize) -> Self {
        Self { candidates, total }
    }

    fn capped(&self) -> bool {
        self.total > self.candidates.len()
    }
}

/// A candidate source living outside this crate.
///
/// The graph is stored in SQLite, which `ide` does not depend on; it is handed
/// in instead. The trait selects and counts, and nothing else — order, limit,
/// de-duplication and completeness stay in [`lookup_names`], or "one
/// dictionary" would be two.
pub trait ExternalNameSource {
    fn provider(&self) -> ProviderId;

    /// Consulted only when this is [`ProviderState::Answered`].
    fn state(&self) -> ProviderState;

    /// Which categories this source can supply, so a narrowed question can skip
    /// it without pretending it had nothing to say.
    fn categories(&self) -> &'static [NameCategory];

    /// Whether its candidates can carry a place in a file. A source that cannot
    /// is not consulted when the caller asked for places only — running it and
    /// discarding everything afterwards is the same answer at a cost.
    fn supplies_location(&self) -> bool;

    /// `limit` is the GLOBAL limit, never a share of it: a provider given less
    /// could drop a candidate that outranks everything the others found. Asking
    /// each for `limit` is enough — if one returns `limit` candidates better
    /// than anyone else's, the global top is entirely inside them.
    fn candidates(&self, query: &str, limit: usize) -> Result<ProviderHits, String>;
}

/// What was asked.
#[derive(Debug, Clone)]
pub struct NameQuery<'a> {
    pub text: &'a str,
    pub limit: usize,
    /// `None` — every category. Narrowing the QUESTION, never trimming the
    /// answer: a provider that supplies none of the requested categories is
    /// reported `not_asked` and does not make the result partial.
    pub categories: Option<&'a [NameCategory]>,
    /// Keep only candidates that have a place in a file. `workspace/symbol`
    /// needs it — a symbol it cannot jump to is not one it can offer.
    pub require_location: bool,
    /// What the database holding the workspace is doing, as the three
    /// database-backed providers should report it when it is not
    /// [`ProviderState::Answered`].
    ///
    /// A state and not a flag, because the three answers a caller draws from it
    /// differ: `not_ready` says waiting helps, `failed` says it does not, and
    /// `unavailable` says there is nothing to wait for in this configuration.
    /// Collapsing them into "ready or not" told an agent to wait out a build
    /// that had already failed.
    pub workspace: ProviderState,
}

impl<'a> NameQuery<'a> {
    pub fn new(text: &'a str, limit: usize) -> Self {
        Self {
            text,
            limit,
            categories: None,
            require_location: false,
            workspace: ProviderState::Answered,
        }
    }

    pub fn with_categories(mut self, categories: &'a [NameCategory]) -> Self {
        self.categories = Some(categories);
        self
    }

    pub fn requiring_location(mut self) -> Self {
        self.require_location = true;
        self
    }

    pub fn with_workspace(mut self, state: ProviderState) -> Self {
        self.workspace = state;
        self
    }

    fn wants(&self, category: NameCategory) -> bool {
        match self.categories {
            None => true,
            Some(list) => list.contains(&category),
        }
    }

    /// A provider is consulted when it can supply at least one wanted category.
    fn wants_any(&self, categories: &[NameCategory]) -> bool {
        categories.iter().any(|&c| self.wants(c))
    }
}

/// How one provider fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderReport {
    pub provider: ProviderId,
    pub state: ProviderState,
}

/// The answer, with enough about its own making to be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameLookupResult {
    pub candidates: Vec<NameCandidate>,
    /// How many matched. An exact count while nobody hit a cap.
    pub total: usize,
    /// Whether `total` is a count or an estimate. When a provider capped its
    /// own output, the candidates it withheld could not be folded against the
    /// others, so `total` may exceed the true distinct count — it is never
    /// below the number actually delivered. Reporting an estimate as a count
    /// is the same lie as dropping candidates without saying so.
    pub total_exact: bool,
    pub truncated: bool,
    pub providers: Vec<ProviderReport>,
}

impl NameLookupResult {
    /// Nothing was asked, and the answer says so about every source.
    ///
    /// An empty `providers` list here would be the one shape this envelope
    /// exists to forbid: a complete, exact zero that names not a single source
    /// it consulted. Naming them all as `not_asked` says the truth instead —
    /// the question was empty, so nobody was asked.
    fn nothing_asked(external: &[&dyn ExternalNameSource]) -> Self {
        let mut providers: Vec<ProviderReport> =
            [MODULE_INDEX, METADATA_LISTING, PLATFORM, MODULE_MEMBERS]
                .into_iter()
                .map(|shape| ProviderReport { provider: shape.id, state: ProviderState::NotAsked })
                .collect();
        providers.extend(external.iter().map(|source| ProviderReport {
            provider: source.provider(),
            state: ProviderState::NotAsked,
        }));
        Self { candidates: Vec::new(), total: 0, total_exact: true, truncated: false, providers }
    }

    /// Whether some index that could have contributed was not in a position to.
    /// `not_asked` and `unavailable` do not count: the first is the caller's own
    /// narrowing, the second a property of the configuration.
    pub fn is_partial(&self) -> bool {
        self.providers
            .iter()
            .any(|r| matches!(r.state, ProviderState::NotReady | ProviderState::Failed))
    }

    /// The providers that made it partial, for the reason detail.
    pub fn incomplete_providers(&self) -> Vec<ProviderReport> {
        self.providers
            .iter()
            .copied()
            .filter(|r| matches!(r.state, ProviderState::NotReady | ProviderState::Failed))
            .collect()
    }

    pub fn state_of(&self, provider: ProviderId) -> Option<ProviderState> {
        self.providers.iter().find(|r| r.provider == provider).map(|r| r.state)
    }
}

/// Which tier a plain name lands in, or `None` when it does not match at all.
///
/// `needle_folded` must already be folded; `candidate` is the spelling as
/// written.
pub fn match_tier(needle_raw: &str, needle_folded: &str, candidate: &str) -> Option<NameMatchTier> {
    if candidate == needle_raw {
        return Some(NameMatchTier::Exact);
    }
    let folded = candidate.fold_lower();
    if folded == needle_folded {
        return Some(NameMatchTier::CaseInsensitive);
    }
    if folded.starts_with(needle_folded) {
        return Some(NameMatchTier::Prefix);
    }
    if folded.contains(needle_folded) {
        return Some(NameMatchTier::Substring);
    }
    None
}

/// What a provider needs and what it can give — enough to decide whether asking
/// it is worth anything before asking.
#[derive(Debug, Clone, Copy)]
struct ProviderShape {
    id: ProviderId,
    serves: &'static [NameCategory],
    /// Reads the workspace database, so an unbuilt workspace makes its silence
    /// mean "not yet" rather than "nothing there". The platform singleton is
    /// the one that does NOT — it is seeded at process start and answers with
    /// no workspace at all, which is what makes a name findable on a host that
    /// has built neither the resident nor the graph.
    needs_workspace: bool,
    /// Its candidates can carry a place in a file.
    supplies_location: bool,
}

const MODULE_INDEX: ProviderShape = ProviderShape {
    id: ProviderId::ModuleIndex,
    serves: &[NameCategory::CommonModule],
    needs_workspace: true,
    supplies_location: true,
};

const METADATA_LISTING: ProviderShape = ProviderShape {
    id: ProviderId::MetadataListing,
    serves: &[NameCategory::MetadataObject, NameCategory::CommonModule],
    needs_workspace: true,
    supplies_location: true,
};

const PLATFORM: ProviderShape = ProviderShape {
    id: ProviderId::Platform,
    serves: &[NameCategory::PlatformMember],
    needs_workspace: false,
    supplies_location: false,
};

const MODULE_MEMBERS: ProviderShape = ProviderShape {
    id: ProviderId::ModuleMembers,
    serves: &[NameCategory::ModuleMethod, NameCategory::ModuleVariable],
    needs_workspace: true,
    supplies_location: true,
};

/// Turns a workspace path into the file id the database knows it by.
///
/// Two spellings of one path are the ordinary case: the graph stores them with
/// forward slashes whatever the platform, while the file set holds them as the
/// platform writes them. The exact match is tried first and costs nothing; the
/// folded table is built once and only after a miss — where the spellings agree
/// it is never built at all.
///
/// The root is ASKED for, not asserted. A database with no source root is an
/// ordinary state on the path this exists to serve — the graph answering while
/// the resident is still building — and asserting one there turned the whole
/// answer into a panic.
struct PathPlaces<'a> {
    db: &'a RootDatabaseImpl,
    /// A `.bsl` lives in [`ROOT`], a metadata XML in [`METADATA_ROOT`]. Both are
    /// searched: confining the lookup to one of them places half the answers and
    /// silently leaves the other half unaddressed.
    roots: [RootPlaces; 2],
}

/// One root's lazily-resolved path lookup.
struct RootPlaces {
    id: SourceRootId,
    /// The root input, not the root itself: it is a handle, while a `SourceRoot`
    /// holds the whole file set and cloning one to keep it here would copy every
    /// path in the workspace.
    input: Option<Option<ide_db::base_db::SourceRootInput>>,
    normalized: Option<rustc_hash::FxHashMap<String, FileId>>,
}

impl RootPlaces {
    fn new(id: SourceRootId) -> Self {
        Self { id, input: None, normalized: None }
    }

    fn file_for(&mut self, db: &RootDatabaseImpl, path: &str) -> Option<FileId> {
        let id = self.id;
        let root = (*self.input.get_or_insert_with(|| db.try_source_root_input(id)))?;
        let file_set = root.root(db).file_set();

        if let Some(&file_id) =
            file_set.file_for_path(&vfs::VfsPath::new(std::path::PathBuf::from(path)))
        {
            return Some(file_id);
        }
        // The fallback table is built from EVERY file of the root, and it is built from
        // inside a per-candidate loop whose checkpoint has already been passed.
        let table = self.normalized.get_or_insert_with(|| {
            file_set
                .iter()
                .filter_map(|file_id| {
                    db.unwind_if_revision_cancelled();
                    let path = file_set.path_for_file(&file_id)?;
                    Some((path.as_path().to_str()?.replace('\\', "/"), file_id))
                })
                .collect()
        });
        table.get(&path.replace('\\', "/")).copied()
    }
}

impl<'a> PathPlaces<'a> {
    fn new(db: &'a RootDatabaseImpl) -> Self {
        Self { db, roots: [RootPlaces::new(ROOT), RootPlaces::new(METADATA_ROOT)] }
    }

    fn of(&mut self, path: &str) -> Option<NamePlace> {
        let db = self.db;
        self.roots.iter_mut().find_map(|root| root.file_for(db, path)).map(NamePlace::whole_file)
    }
}

/// Accumulates one lookup: candidates, provider verdicts and whether anyone
/// capped. Every provider funnels through here so no second place can decide
/// what the answer contains.
struct Merge<'q> {
    db: &'q RootDatabaseImpl,
    query: &'q NameQuery<'q>,
    needle_raw: &'q str,
    needle_folded: String,
    candidates: Vec<NameCandidate>,
    providers: Vec<ProviderReport>,
    provider_total: usize,
    capped: bool,
}

impl<'q> Merge<'q> {
    fn new(db: &'q RootDatabaseImpl, query: &'q NameQuery<'q>, needle_folded: String) -> Self {
        Self {
            db,
            query,
            needle_raw: query.text,
            needle_folded,
            candidates: Vec::new(),
            providers: Vec::new(),
            provider_total: 0,
            capped: false,
        }
    }

    fn tier(&self, candidate: &str) -> Option<NameMatchTier> {
        match_tier(self.needle_raw, &self.needle_folded, candidate)
    }

    /// The strongest tier across a member's spellings.
    ///
    /// A platform member is written twice, in Russian and in English, and both
    /// are the same member. Publishing whichever spelling happened to match
    /// would make `strfind` and `СтрНайти` two different answers to one
    /// question — so the tier is taken from the best of them and the canonical
    /// spelling is published.
    fn best_tier(&self, spellings: &[&str]) -> Option<NameMatchTier> {
        spellings.iter().filter_map(|s| self.tier(s)).min()
    }

    /// A candidate the caller can neither open nor pass on is not an answer.
    fn accepts(&self, candidate: &NameCandidate) -> bool {
        candidate.is_addressable()
            && (!self.query.require_location || candidate.place.is_some())
            && self.query.wants(candidate.category)
    }

    fn report(&mut self, provider: ProviderId, state: ProviderState) {
        self.providers.push(ProviderReport { provider, state });
    }

    /// Run one in-process provider, or record why it was not run.
    fn run(&mut self, shape: ProviderShape, collect: impl FnOnce(&mut Self) -> Vec<NameCandidate>) {
        if !self.query.wants_any(shape.serves)
            || (self.query.require_location && !shape.supplies_location)
        {
            self.report(shape.id, ProviderState::NotAsked);
            return;
        }
        if shape.needs_workspace && self.query.workspace != ProviderState::Answered {
            self.report(shape.id, self.query.workspace);
            return;
        }
        let provider = shape.id;
        let found = collect(self);
        // The provider's own loops carry checkpoints; this pass over what it returned
        // is a second walk of the same size, and it runs no query of its own.
        let db = self.db;
        let kept: Vec<_> = found
            .into_iter()
            .inspect(|_| db.unwind_if_revision_cancelled())
            .filter(|c| self.accepts(c))
            .collect();
        self.provider_total += kept.len();
        self.candidates.extend(kept);
        self.report(provider, ProviderState::Answered);
    }

    fn absorb(&mut self, provider: ProviderId, hits: ProviderHits) {
        if hits.capped() {
            self.capped = true;
        }
        self.provider_total += hits.total;
        let db = self.db;
        let mut placed = PathPlaces::new(self.db);
        let kept: Vec<_> = hits
            .candidates
            .into_iter()
            .inspect(|_| db.unwind_if_revision_cancelled())
            .map(|mut candidate| {
                // Before `accepts`: a path IS a place, and a caller asking only
                // for locatable candidates must not lose one for having said so
                // in the other of the two ways.
                if let Some(path) = candidate.source_path.take() {
                    candidate.place = candidate.place.or_else(|| placed.of(&path));
                }
                candidate
            })
            .filter(|c| self.accepts(c))
            .collect();
        self.candidates.extend(kept);
        self.report(provider, ProviderState::Answered);
    }

    /// Rank, fold and cut — the single place where order and completeness are
    /// decided, so adding a provider cannot change either by accident.
    fn finish(mut self) -> NameLookupResult {
        // Cached, not recomputed per comparison: the ordering key folds the
        // name, and a broad query ranks tens of thousands of candidates — two
        // allocations per COMPARISON is where the whole answer goes.
        // Ranking and folding are one indivisible step: once the sort starts there is
        // no boundary inside it to unwind at, so a cancel that has already arrived must
        // be seen before it — not after tens of thousands of comparisons.
        self.db.unwind_if_revision_cancelled();
        self.candidates.sort_by_cached_key(|c| {
            (c.match_tier, c.category, c.display.fold_lower(), c.display.clone())
        });

        // One entity, one row, with every address any provider knew for it.
        //
        // Dropping the later row instead of absorbing it is what used to split
        // an entity in two: the graph knows a durable id, the resident a symbol
        // and a place, and whichever sorted first took the slot while the
        // other's address went in the bin. They are the same row because they
        // are the same FILE — proven, not inferred from the name.
        let mut first: rustc_hash::FxHashMap<_, usize> = Default::default();
        let mut merged: Vec<NameCandidate> = Vec::with_capacity(self.candidates.len());
        for candidate in std::mem::take(&mut self.candidates) {
            self.db.unwind_if_revision_cancelled();
            match first.get(&candidate.identity()) {
                Some(&at) => merged[at].absorb_addresses(candidate),
                None => {
                    first.insert(candidate.identity(), merged.len());
                    merged.push(candidate);
                }
            }
        }
        self.candidates = merged;

        let distinct = self.candidates.len();
        let (total, total_exact) =
            if self.capped { (distinct.max(self.provider_total), false) } else { (distinct, true) };
        let truncated = distinct > self.query.limit || self.capped;
        self.candidates.truncate(self.query.limit);

        NameLookupResult {
            candidates: self.candidates,
            total,
            total_exact,
            truncated,
            providers: self.providers,
        }
    }
}

/// Search every artefact that knows names.
pub fn lookup_names(
    db: &RootDatabaseImpl,
    query: &NameQuery<'_>,
    external: &[&dyn ExternalNameSource],
) -> NameLookupResult {
    let needle_folded = query.text.fold_lower();
    if needle_folded.is_empty() {
        return NameLookupResult::nothing_asked(external);
    }

    let mut merge = Merge::new(db, query, needle_folded);

    merge.run(MODULE_INDEX, |m| from_module_index(db, m));
    merge.run(METADATA_LISTING, |m| from_metadata_listing(db, m));
    merge.run(PLATFORM, |m| from_platform(db, m));
    merge.run(MODULE_MEMBERS, |m| from_module_members(db, m));

    for source in external {
        let id = source.provider();
        if !query.wants_any(source.categories())
            || (query.require_location && !source.supplies_location())
        {
            merge.report(id, ProviderState::NotAsked);
            continue;
        }
        let state = source.state();
        if state != ProviderState::Answered {
            merge.report(id, state);
            continue;
        }
        match source.candidates(query.text, query.limit) {
            Ok(hits) => merge.absorb(id, hits),
            Err(error) => {
                tracing::warn!(provider = id.as_str(), %error, "name provider failed");
                merge.report(id, ProviderState::Failed);
            }
        }
    }

    let mut result = merge.finish();
    spell_symbols(db, &mut result.candidates);
    result
}

/// Spell the deferred `symbol`s — for the survivors only.
///
/// Every candidate here has passed ranking, folding and the cut, so the path
/// parse and the `format!` happen at most `limit` times instead of once per
/// match. The result is the same string the provider used to build eagerly; it
/// is only built later.
fn spell_symbols(db: &RootDatabaseImpl, candidates: &mut [NameCandidate]) {
    if !candidates.iter().any(|c| c.spelling.is_some()) {
        return;
    }
    let source_root = db.source_root_input(ROOT).root(db);
    let file_set = source_root.file_set();
    let mut owners: rustc_hash::FxHashMap<FileId, Option<ModuleKey>> = Default::default();

    for candidate in candidates.iter_mut() {
        // `top` is the caller's number, so this list is as long as the client asked for.
        db.unwind_if_revision_cancelled();
        let Some(SymbolSpelling::OwningModule) = candidate.spelling.take() else { continue };
        let Some(place) = candidate.place else { continue };
        let owner = owners.entry(place.file_id).or_insert_with(|| {
            file_set
                .path_for_file(&place.file_id)
                .and_then(|path| module_key_for_path(&path.as_path().to_string_lossy()))
        });
        candidate.symbol = owner.as_ref().map(|key| method_symbol(key, &candidate.display));
    }
}

/// Common modules, from the path-derived module table. The only module kind it
/// can enumerate — managers, object modules and forms are keyed for lookup and
/// have no name iterator.
fn from_module_index(db: &RootDatabaseImpl, merge: &Merge<'_>) -> Vec<NameCandidate> {
    let index = db.module_index(ROOT);
    let mut out = Vec::new();
    for display in index.common_module_display_names() {
        db.unwind_if_revision_cancelled();
        let Some(tier) = merge.tier(display) else { continue };
        let Some(file_id) = index.resolve_common_module(&Name::new(display)) else { continue };
        out.push(
            NameCandidate::new(display, NameCategory::CommonModule, tier, ProviderId::ModuleIndex)
                .with_symbol(display)
                .with_place(NamePlace::whole_file(file_id)),
        );
    }
    out
}

/// Configuration objects and common modules, from the metadata listing of every
/// registered root (base first, then extensions — the same order navigation
/// uses — then external objects, which are listed under their own kind).
fn from_metadata_listing(db: &RootDatabaseImpl, merge: &Merge<'_>) -> Vec<NameCandidate> {
    let mut out = Vec::new();
    let paths = db.all_config_paths();
    let base_first = paths
        .iter()
        .filter(|(label, _)| label.is_none())
        .chain(paths.iter().filter(|(label, _)| label.is_some()));

    for (_label, root) in base_first {
        let root = root.to_string_lossy();
        let Some(listing) = db.metadata_listing(root.as_ref()) else { continue };

        for entry in listing.entries(db).iter() {
            db.unwind_if_revision_cancelled();
            let Some(tier) = merge.tier(&entry.name) else { continue };
            out.push(
                NameCandidate::new(
                    &entry.name,
                    NameCategory::MetadataObject,
                    tier,
                    ProviderId::MetadataListing,
                )
                .with_symbol(format!("{}.{}", entry.kind.russian_name(), entry.name))
                .with_place(NamePlace::whole_file(entry.main)),
            );
        }

        for entry in listing.common_modules(db).iter() {
            db.unwind_if_revision_cancelled();
            let Some(tier) = merge.tier(&entry.name) else { continue };
            // The module body is the useful destination; its XML is the fallback
            // for a module whose `.bsl` could not be read.
            let file_id = entry.module_file.unwrap_or(entry.main);
            let mut candidate = NameCandidate::new(
                &entry.name,
                NameCategory::CommonModule,
                tier,
                ProviderId::MetadataListing,
            )
            .with_place(NamePlace::whole_file(file_id));
            // `symbol_info` reaches a common module through the path-derived
            // module index, which only knows modules that have a readable body.
            // A protected module — listed in the XML, no `.bsl` beside it — is
            // real, findable and openable, and naming it as a `symbol` would
            // hand out a key the card refuses.
            if entry.module_file.is_some() {
                candidate = candidate.with_symbol(&entry.name);
            }
            out.push(candidate);
        }

        // Objects with no BSL-addressable spelling: found by name and opened by
        // location, but never handed to `symbol_info`, which would refuse them.
        for (name, file_id) in listing
            .roles(db)
            .iter()
            .map(|e| (&e.name, e.main))
            .chain(listing.subsystems(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.event_subscriptions(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.scheduled_jobs(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.defined_types(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.http_services(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.web_services(db).iter().map(|e| (&e.name, e.main)))
            .chain(listing.integration_services(db).iter().map(|e| (&e.name, e.main)))
        {
            db.unwind_if_revision_cancelled();
            let Some(tier) = merge.tier(name) else { continue };
            out.push(
                NameCandidate::new(
                    name,
                    NameCategory::MetadataObject,
                    tier,
                    ProviderId::MetadataListing,
                )
                .with_place(NamePlace::whole_file(file_id)),
            );
        }
    }
    out
}

/// Platform members. There is no name index to consult — the singleton exposes
/// whole collections and nothing narrower — so this is a linear scan, the same
/// one `syntax_help` already performs.
fn from_platform(db: &RootDatabaseImpl, merge: &mut Merge<'_>) -> Vec<NameCandidate> {
    let mut out = Vec::new();

    let mut push = |canonical: &str,
                    symbol: Option<String>,
                    r: PlatformRef,
                    tier: NameMatchTier| {
        let mut candidate =
            NameCandidate::new(canonical, NameCategory::PlatformMember, tier, ProviderId::Platform);
        candidate.symbol = symbol;
        candidate.platform_ref = Some(r);
        out.push(candidate);
    };

    for entry in hir::platform_name_entries() {
        db.unwind_if_revision_cancelled();
        let Some(tier) = merge.best_tier(&[entry.name, entry.english_name]) else {
            continue;
        };
        let canonical = entry.name;
        let owner = entry.owner.map(str::to_owned);
        let symbol = match entry.kind {
            hir::PlatformNameKind::GlobalFunction => Some(canonical.to_owned()),
            hir::PlatformNameKind::Method => owner
                .as_deref()
                .filter(|owner| !owner.contains('.'))
                .map(|owner| format!("{owner}.{canonical}")),
            hir::PlatformNameKind::Type | hir::PlatformNameKind::Property => None,
        };
        push(canonical, symbol, PlatformRef { name: canonical.to_owned(), type_name: owner }, tier);
    }

    out
}

/// Exported methods and module variables of every module in the root.
fn from_module_members(db: &RootDatabaseImpl, merge: &Merge<'_>) -> Vec<NameCandidate> {
    let members = db.module_members(ROOT);

    let mut out = Vec::new();
    for module in members.modules.values() {
        db.unwind_if_revision_cancelled();
        for method in &module.methods {
            let Some(tier) = merge.tier(method.name.as_str()) else { continue };
            // The `symbol` is not spelled here. A broad query matches tens of
            // thousands of members and publishes a few hundred; the spelling
            // costs a path parse and a `format!` each, and identity does not
            // need it (see `NameCandidate::identity`).
            out.push(
                NameCandidate::new(
                    method.name.as_str(),
                    NameCategory::ModuleMethod,
                    tier,
                    ProviderId::ModuleMembers,
                )
                .with_place(NamePlace::declaration(
                    module.file_id,
                    method.name_range,
                    method.source_range,
                ))
                .spelled_by_owning_module(),
            );
        }

        for variable in &module.variables {
            let Some(tier) = merge.tier(variable.name.as_str()) else { continue };
            // No `symbol` on purpose: `symbol_info` resolves methods and
            // objects, and a module variable is neither — publishing one would
            // hand back a key that answers `not found`.
            out.push(
                NameCandidate::new(
                    variable.name.as_str(),
                    NameCategory::ModuleVariable,
                    tier,
                    ProviderId::ModuleMembers,
                )
                .with_place(NamePlace::declaration(
                    module.file_id,
                    variable.name_range,
                    variable.source_range,
                )),
            );
        }
    }
    out
}

/// The `symbol_info` spelling of a module method, by the module's kind.
///
/// Each form is one `symbol_info` resolution path: a common module method is a
/// pair, everything else a triple routed by metadata type.
fn method_symbol(key: &ModuleKey, method: &str) -> String {
    match key {
        ModuleKey::Common { name } => format!("{name}.{method}"),
        ModuleKey::Manager { mdo_type, name }
        | ModuleKey::Object { mdo_type, name }
        | ModuleKey::RecordSet { mdo_type, name } => {
            format!("{}.{name}.{method}", mdo_type.russian_name())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::SourceRoot;
    use ide_db::metadata::{CommonModuleEntry, MdoEntry, MetadataListingData};
    use vfs::{file_set::FileSet, VfsPath};

    /// The phases that follow the providers answer to the cancel too: keeping what a
    /// provider returned, ranking, and folding duplicates each walk the whole candidate
    /// list and run no query of their own.
    ///
    /// Called directly, because a cancel arriving mid-lookup cannot be aimed at one
    /// phase from outside: a pre-cancelled token unwinds in the first provider.
    #[test]
    fn the_phases_after_the_providers_observe_a_cancelled_request() {
        let db = RootDatabaseImpl::default();
        let query = NameQuery::new("Имя", 100);
        let candidates = || {
            (0..50)
                .map(|i| {
                    NameCandidate::new(
                        format!("Имя{i}"),
                        NameCategory::CommonModule,
                        NameMatchTier::Prefix,
                        MODULE_INDEX.id,
                    )
                    .with_graph_id(format!("id{i}"))
                })
                .collect::<Vec<_>>()
        };

        salsa::Database::cancellation_token(&db).cancel();

        let mut keeping = Merge::new(&db, &query, "имя".to_string());
        let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            keeping.run(MODULE_INDEX, |_| candidates())
        }));
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "keeping a provider's candidates walked the whole list after the cancel"
        );

        let mut merge = Merge::new(&db, &query, "имя".to_string());
        merge.candidates = candidates();
        let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(move || merge.finish()));
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "ranking and folding ran to the end after the cancel"
        );
    }

    /// Two more phases that run no query of their own: spelling the survivors, whose
    /// length is the number the CALLER asked for, and the fallback path table, which is
    /// built from every file of a root — from inside a loop whose checkpoint has
    /// already been passed for that candidate.
    #[test]
    fn the_address_phases_observe_a_cancelled_request() {
        let db = db_with_files(&[
            ("/ws/CommonModules/Первый/Ext/Module.bsl", "Процедура П() Экспорт КонецПроцедуры"),
            ("/ws/CommonModules/Второй/Ext/Module.bsl", "Процедура В() Экспорт КонецПроцедуры"),
        ]);
        salsa::Database::cancellation_token(&db).cancel();

        let mut candidates: Vec<NameCandidate> = (0..10)
            .map(|i| {
                let mut candidate = NameCandidate::new(
                    format!("П{i}"),
                    NameCategory::ModuleMethod,
                    NameMatchTier::Prefix,
                    MODULE_INDEX.id,
                );
                candidate.spelling = Some(SymbolSpelling::OwningModule);
                candidate.place = Some(NamePlace::whole_file(FileId(0)));
                candidate
            })
            .collect();
        let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            spell_symbols(&db, &mut candidates)
        }));
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "the survivors were spelled after the request was cancelled"
        );

        // A spelling the direct lookup misses, so the fallback table gets built.
        let mut places = PathPlaces::new(&db);
        let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            places.of("\\ws\\CommonModules\\Первый\\Ext\\Module.bsl")
        }));
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "the path table walked the whole root after the request was cancelled"
        );
    }

    fn db_with_files(files: &[(&str, &str)]) -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::default();
        let mut file_set = FileSet::new();
        for (i, (path, _)) in files.iter().enumerate() {
            file_set.insert(FileId(i as u32), VfsPath::new(*path));
        }
        db.set_source_root(ROOT, SourceRoot::new_local(file_set));
        for (i, (_, text)) in files.iter().enumerate() {
            let file_id = FileId(i as u32);
            db.set_file_source_root(file_id, ROOT);
            db.set_file_text(file_id, text);
        }
        db
    }

    /// A stand where an object's XML lives where it really lives — the metadata
    /// root — and a base config root's listing publishes it. `db_with_files`
    /// registers every path under `BSL_SOURCE_ROOT`, where a metadata path
    /// resolves by accident, so a test built on it never reaches the metadata
    /// root at all.
    fn db_with_metadata_object(
        root: &str,
        kind: bsl_metadata::MdoType,
        name: &str,
        xml: &str,
    ) -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new(xml));
        db.set_source_root(
            ide_db::base_db::METADATA_SOURCE_ROOT,
            SourceRoot::new_metadata(file_set),
        );
        db.set_file_source_root(file_id, ide_db::base_db::METADATA_SOURCE_ROOT);
        db.set_file_text(file_id, "<MetaDataObject/>");

        let root_path = std::path::PathBuf::from(root);
        db.set_all_config_paths(vec![(None, root_path.clone())]);
        db.set_metadata_listing(
            &root_path.to_string_lossy(),
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind,
                    name: name.to_string(),
                    main: file_id,
                    predefined: None,
                }],
                ..MetadataListingData::default()
            },
        );
        db
    }

    fn common_module(name: &str, body: &str) -> (String, String) {
        (format!("/ws/CommonModules/{name}/Ext/Module.bsl"), body.to_string())
    }

    fn build(modules: &[(String, String)]) -> RootDatabaseImpl {
        let files: Vec<(&str, &str)> =
            modules.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        db_with_files(&files)
    }

    fn look(db: &RootDatabaseImpl, query: &NameQuery<'_>) -> NameLookupResult {
        lookup_names(db, query, &[])
    }

    /// Merge mechanics — order, counting, folding — are asked about module
    /// methods alone. The platform answers a substring query with dozens of its
    /// own members, which is correct of it and only noise here.
    const METHODS: &[NameCategory] = &[NameCategory::ModuleMethod];

    fn displays(result: &NameLookupResult) -> Vec<&str> {
        result.candidates.iter().map(|c| c.display.as_str()).collect()
    }

    /// A stub source standing in for the graph: it decides only what it found
    /// and how much of it there was.
    struct FakeSource {
        provider: ProviderId,
        state: ProviderState,
        hits: ProviderHits,
    }

    impl FakeSource {
        fn answering(provider: ProviderId, candidates: Vec<NameCandidate>, total: usize) -> Self {
            Self {
                provider,
                state: ProviderState::Answered,
                hits: ProviderHits::new(candidates, total),
            }
        }
    }

    impl ExternalNameSource for FakeSource {
        fn provider(&self) -> ProviderId {
            self.provider
        }
        fn state(&self) -> ProviderState {
            self.state
        }
        fn categories(&self) -> &'static [NameCategory] {
            NameCategory::ALL
        }
        /// Claims it can, and then hands over candidates with no place: the
        /// post-filter is a separate guard from the skip above, and this fake
        /// is what keeps it under test.
        fn supplies_location(&self) -> bool {
            true
        }
        fn candidates(&self, _query: &str, _limit: usize) -> Result<ProviderHits, String> {
            Ok(self.hits.clone())
        }
    }

    fn graph_candidate(display: &str, id: &str) -> NameCandidate {
        let mut candidate = NameCandidate::new(
            display,
            NameCategory::ModuleMethod,
            NameMatchTier::Name,
            ProviderId::Graph,
        );
        candidate.graph_id = Some(id.to_string());
        candidate
    }

    /// The graph as it really answers: a durable id AND the file the node is
    /// written in. A stand that hands over the id alone tests a source that no
    /// longer exists.
    fn graph_candidate_in(display: &str, id: &str, path: &str) -> NameCandidate {
        graph_candidate(display, id).with_source_path(path)
    }

    #[test]
    fn empty_query_finds_nothing() {
        let db = build(&[common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")]);
        assert!(look(&db, &NameQuery::new("", 20)).candidates.is_empty());
    }

    /// An empty question is answered by nobody, and the envelope has to say
    /// that rather than present silence as an exhaustive zero — the very shape
    /// `providers` was added to make impossible.
    #[test]
    fn an_empty_question_still_names_every_source() {
        let db = build(&[common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")]);
        let graph = FakeSource::answering(ProviderId::Graph, Vec::new(), 0);

        let found = lookup_names(&db, &NameQuery::new("", 20), &[&graph]);

        let named: std::collections::BTreeSet<_> =
            found.providers.iter().map(|r| r.provider).collect();
        assert_eq!(named.len(), ProviderId::ALL.len(), "{:?}", found.providers);
        assert!(found.providers.iter().all(|r| r.state == ProviderState::NotAsked));
    }

    /// A build that failed and a build still running are different advice.
    ///
    /// The three database-backed providers report whatever the caller says the
    /// workspace is doing; reporting `not_ready` for a failed resident sends an
    /// agent to wait for something that will never arrive.
    #[test]
    fn a_failed_workspace_is_not_reported_as_one_still_building() {
        let db = build(&[]);

        let found =
            look(&db, &NameQuery::new("Настройки", 20).with_workspace(ProviderState::Failed));

        for provider in
            [ProviderId::ModuleIndex, ProviderId::MetadataListing, ProviderId::ModuleMembers]
        {
            assert_eq!(found.state_of(provider), Some(ProviderState::Failed), "{provider:?}");
        }
        // The platform reads no workspace, so a broken one changes nothing for it.
        assert_eq!(found.state_of(ProviderId::Platform), Some(ProviderState::Answered));
    }

    /// The fixture is chosen so the alphabet works AGAINST the answer: without
    /// a prefix tier the secondary key puts `АааТест` first.
    #[test]
    fn a_name_starting_with_the_query_outranks_one_merely_containing_it() {
        let db = build(&[common_module(
            "Настройки",
            "Процедура АааТест() Экспорт\nКонецПроцедуры\n\n\
             Процедура ТестБ() Экспорт\nКонецПроцедуры\n",
        )]);

        let found = look(&db, &NameQuery::new("Тест", 20).with_categories(METHODS));
        assert_eq!(displays(&found), vec!["ТестБ", "АааТест"]);
    }

    /// Hundreds of modules declare the same handler; folding them by name would
    /// delete the answer instead of tidying it.
    #[test]
    fn one_name_in_three_modules_stays_three_candidates() {
        let body = "Процедура ПриСозданииНаСервере() Экспорт\nКонецПроцедуры\n";
        let db =
            build(&[common_module("А", body), common_module("Б", body), common_module("В", body)]);

        let found = look(&db, &NameQuery::new("ПриСоздании", 20).with_categories(METHODS));
        assert_eq!(found.candidates.len(), 3, "{:?}", displays(&found));
        let places: std::collections::HashSet<_> =
            found.candidates.iter().map(|c| c.place.map(|p| p.file_id)).collect();
        assert_eq!(places.len(), 3);
    }

    /// The mirror of the test above: without de-duplication a common module
    /// known to two providers would be offered twice.
    #[test]
    fn one_module_known_to_two_providers_folds_into_one() {
        let modules = [common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")];
        let mut db = build(&modules);

        let root = std::path::PathBuf::from("/ws");
        db.set_all_config_paths(vec![(None, root.clone())]);
        db.set_metadata_listing(
            &root.to_string_lossy(),
            MetadataListingData {
                common_modules: vec![CommonModuleEntry {
                    name: "Настройки".to_string(),
                    main: FileId(0),
                    module_file: Some(FileId(0)),
                    unread_module_file: None,
                }],
                ..MetadataListingData::default()
            },
        );

        let found = look(&db, &NameQuery::new("Настройки", 20));
        let modules: Vec<_> =
            found.candidates.iter().filter(|c| c.category == NameCategory::CommonModule).collect();
        assert_eq!(modules.len(), 1, "{modules:?}");
        assert_eq!(found.state_of(ProviderId::MetadataListing), Some(ProviderState::Answered));
    }

    /// A configuration and its extension may each hold a common module of the
    /// same name. They are two modules in two files, and the answer has to be
    /// two candidates — otherwise which of them survives is decided by sort
    /// order, and the other disappears without a word.
    #[test]
    fn a_name_shared_by_two_roots_stays_two_candidates() {
        let base = "/ws/src/cf/CommonModules/Настройки/Ext/Module.bsl";
        let extension = "/ws/src/cfe/CommonModules/Настройки/Ext/Module.bsl";
        let body = "Процедура Прочитать() Экспорт\nКонецПроцедуры\n";
        let mut db = db_with_files(&[(base, body), (extension, body)]);

        let base_root = std::path::PathBuf::from("/ws/src/cf");
        let extension_root = std::path::PathBuf::from("/ws/src/cfe");
        db.set_all_config_paths(vec![
            (None, base_root.clone()),
            (Some("Расширение".to_string()), extension_root.clone()),
        ]);
        for (root, file_id) in [(&base_root, FileId(0)), (&extension_root, FileId(1))] {
            db.set_metadata_listing(
                &root.to_string_lossy(),
                MetadataListingData {
                    common_modules: vec![CommonModuleEntry {
                        name: "Настройки".to_string(),
                        main: file_id,
                        module_file: Some(file_id),
                        unread_module_file: None,
                    }],
                    ..MetadataListingData::default()
                },
            );
        }

        let found = look(&db, &NameQuery::new("Настройки", 20));
        let files: std::collections::BTreeSet<u32> = found
            .candidates
            .iter()
            .filter(|c| c.category == NameCategory::CommonModule)
            .filter_map(|c| c.place.map(|p| p.file_id.0))
            .collect();

        assert_eq!(files, [0, 1].into_iter().collect(), "{:?}", found.candidates);
    }

    /// Narrowing the question is not trimming the answer: the providers that
    /// were not asked say so, and the result stays complete.
    #[test]
    fn narrowing_the_question_is_not_truncating_the_answer() {
        let db =
            build(&[common_module("Настройки", "Процедура Сброс() Экспорт\nКонецПроцедуры\n")]);

        let wide = look(&db, &NameQuery::new("Сброс", 20));
        let narrow =
            look(&db, &NameQuery::new("Сброс", 20).with_categories(&[NameCategory::ModuleMethod]));

        assert!(!wide.truncated);
        assert!(!narrow.truncated);
        assert!(!narrow.is_partial(), "not_asked must not make the answer partial");
        assert_eq!(narrow.state_of(ProviderId::Platform), Some(ProviderState::NotAsked));
        assert_eq!(narrow.state_of(ProviderId::ModuleMembers), Some(ProviderState::Answered));
    }

    /// The input that a merge counting only delivered candidates cannot survive:
    /// everything over the limit sits inside ONE provider.
    #[test]
    fn a_provider_that_capped_itself_makes_the_answer_say_so() {
        let db = build(&[]);
        let limit = 5;
        let delivered: Vec<_> = (0..limit)
            .map(|i| graph_candidate(&format!("Обработчик{i}"), &format!("m/{i}")))
            .collect();
        let source = FakeSource::answering(ProviderId::Graph, delivered, limit + 5);

        let query = NameQuery::new("Обработчик", limit).with_categories(METHODS);
        let found = lookup_names(&db, &query, &[&source]);

        assert!(found.truncated, "a provider hit its own cap");
        assert!(!found.total_exact, "the withheld candidates could not be counted");
        assert!(found.total >= limit + 5, "total {} must not undersell", found.total);
    }

    /// The same overflow spread across providers, none of which capped. Here
    /// the merge knows the true number — which is why this input alone is not
    /// a gate: it stays green with the per-provider count missing.
    #[test]
    fn spread_over_providers_the_count_stays_exact() {
        let db = build(&[]);
        let limit = 5;
        let split = |provider: ProviderId, from: usize, to: usize| {
            let items: Vec<_> = (from..to)
                .map(|i| graph_candidate(&format!("Обработчик{i}"), &format!("m/{i}")))
                .collect();
            let total = items.len();
            FakeSource::answering(provider, items, total)
        };
        let a = split(ProviderId::Graph, 0, 4);
        let b = split(ProviderId::ModuleIndex, 4, 7);
        let c = split(ProviderId::Platform, 7, 10);

        let query = NameQuery::new("Обработчик", limit).with_categories(METHODS);
        let found = lookup_names(&db, &query, &[&a, &b, &c]);

        assert!(found.truncated, "ten distinct matches do not fit a limit of five");
        assert!(found.total_exact);
        assert_eq!(found.total, 10);
        assert_eq!(found.candidates.len(), limit);
    }

    /// Raising the limit past the match count takes both flags down — without
    /// this the two tests above would pass on an implementation that always
    /// reports truncation.
    #[test]
    fn a_limit_nobody_reaches_leaves_the_answer_whole() {
        let db = build(&[]);
        let items: Vec<_> =
            (0..3).map(|i| graph_candidate(&format!("Обработчик{i}"), &format!("m/{i}"))).collect();
        let source = FakeSource::answering(ProviderId::Graph, items, 3);

        let found = lookup_names(
            &db,
            &NameQuery::new("Обработчик", 50).with_categories(METHODS),
            &[&source],
        );

        assert!(!found.truncated);
        assert!(found.total_exact);
        assert_eq!(found.total, 3);
    }

    /// Concatenation without a shared sort would let the order depend on which
    /// provider ran first.
    #[test]
    fn ranking_does_not_depend_on_the_order_providers_ran_in() {
        let db = build(&[common_module(
            "Настройки",
            "Процедура ТестБ() Экспорт\nКонецПроцедуры\n\nПроцедура ТестА() Экспорт\nКонецПроцедуры\n",
        )]);
        let one = FakeSource::answering(
            ProviderId::Graph,
            vec![graph_candidate("ТестВ", "method/common/Настройки/ТестВ")],
            1,
        );
        let two = FakeSource::answering(
            ProviderId::MetadataListing,
            vec![graph_candidate("ТестГ", "method/common/Настройки/ТестГ")],
            1,
        );

        let query = NameQuery::new("Тест", 20).with_categories(METHODS);
        let forward = lookup_names(&db, &query, &[&one, &two]);
        let backward = lookup_names(&db, &query, &[&two, &one]);

        assert_eq!(displays(&forward), displays(&backward));
    }

    /// Russian, English and shouted — one member, one answer, one `symbol`.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn three_spellings_of_one_platform_member_are_one_candidate() {
        let db = build(&[]);
        let ask = |text: &str| {
            let query = NameQuery::new(text, 50).with_categories(&[NameCategory::PlatformMember]);
            look(&db, &query)
                .candidates
                .into_iter()
                .find(|c| c.display == "СтрНайти")
                .unwrap_or_else(|| panic!("`{text}` did not find СтрНайти"))
        };

        let ru = ask("СтрНайти");
        let en = ask("strfind");
        let shouted = ask("СТРНАЙТИ");

        assert_eq!(ru.symbol.as_deref(), Some("СтрНайти"));
        assert_eq!(en.symbol, ru.symbol);
        assert_eq!(shouted.symbol, ru.symbol);
        assert_eq!(ru.match_tier, NameMatchTier::Exact);
        assert_eq!(en.match_tier, NameMatchTier::CaseInsensitive);
    }

    /// `symbol_info` resolves methods and objects; a module variable is
    /// neither, so it travels by location and publishes no key that would
    /// answer "not found".
    #[test]
    fn a_module_variable_travels_by_location_without_a_symbol() {
        let db = build(&[common_module("Настройки", "Перем СчётчикЗапросов Экспорт;\n")]);

        let found = look(&db, &NameQuery::new("СчётчикЗапросов", 20));
        let variable = found
            .candidates
            .iter()
            .find(|c| c.category == NameCategory::ModuleVariable)
            .expect("the exported variable is in the dictionary");

        assert!(variable.symbol.is_none());
        assert!(variable.graph_id.is_none());
        assert!(variable.place.is_some());
    }

    /// A durable id whose trailing segment IS the query beats a longer name
    /// that merely begins with it.
    ///
    /// The two tiers come from different providers — only the graph reports
    /// `Name`, only [`match_tier`] reports `Prefix` — so their relative order is
    /// what decides whether the graph's exact hit survives a narrow limit. The
    /// ranking this replaced put an exact name second, right after the
    /// case-folded one, and nothing between.
    #[test]
    fn an_exact_name_outranks_a_longer_prefix() {
        let db =
            build(&[common_module("Настройки", "Процедура ТестА() Экспорт\nКонецПроцедуры\n")]);
        let graph = FakeSource::answering(
            ProviderId::Graph,
            vec![graph_candidate("Тест", "method/common/Прочее/Тест")],
            1,
        );

        let found =
            lookup_names(&db, &NameQuery::new("Тест", 20).with_categories(METHODS), &[&graph]);

        assert_eq!(displays(&found), vec!["Тест", "ТестА"]);
    }

    /// A graph node and a resident record of the same thing are one answer with
    /// two addresses, not two answers with one each.
    ///
    /// The graph carries no place and the resident carries no durable id, so
    /// nothing folds them by identity alone. Left apart they cost two slots of
    /// the limit, inflate `total` under `total_exact`, and — once the cut bites —
    /// drop the `graph_id` that `resolve` exists to hand out.
    #[test]
    fn a_graph_node_and_its_resident_record_fold_into_one_candidate() {
        let db = build(&[common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")]);
        let mut node = NameCandidate::new(
            "Настройки",
            NameCategory::CommonModule,
            NameMatchTier::Name,
            ProviderId::Graph,
        );
        node.graph_id = Some("module/common/Настройки".to_string());
        let node = node.with_source_path("/ws/CommonModules/Настройки/Ext/Module.bsl");
        let graph = FakeSource::answering(ProviderId::Graph, vec![node], 1);

        let found = lookup_names(
            &db,
            &NameQuery::new("Настройки", 20).with_categories(&[NameCategory::CommonModule]),
            &[&graph],
        );

        assert_eq!(found.candidates.len(), 1, "{:?}", displays(&found));
        let merged = &found.candidates[0];
        assert!(merged.place.is_some(), "the place survived the fold");
        assert_eq!(merged.graph_id.as_deref(), Some("module/common/Настройки"));
        assert_eq!(found.total, 1, "a folded pair is one answer, not two");
        assert!(found.total_exact);
    }

    /// A metadata object lives in an XML, and that XML is registered under the
    /// metadata root — not the `.bsl` root. A stand that puts it in
    /// `BSL_SOURCE_ROOT` resolves the path today and would stay green through
    /// every change this test exists to guard.
    #[test]
    fn a_metadata_object_known_to_both_sides_is_one_candidate() {
        let xml = "/ws/src/cf/Catalogs/Товары/Товары.xml";
        let db =
            db_with_metadata_object("/ws/src/cf", bsl_metadata::MdoType::Catalog, "Товары", xml);

        let mut node = NameCandidate::new(
            "Товары",
            NameCategory::MetadataObject,
            NameMatchTier::Name,
            ProviderId::Graph,
        )
        .with_source_path(xml);
        node.graph_id = Some("mdo/Catalog/Товары".to_string());
        let graph = FakeSource::answering(ProviderId::Graph, vec![node], 1);

        let found = lookup_names(
            &db,
            &NameQuery::new("Товары", 20).with_categories(&[NameCategory::MetadataObject]),
            &[&graph],
        );

        assert_eq!(found.candidates.len(), 1, "{:?}", displays(&found));
        let merged = &found.candidates[0];
        assert!(merged.place.is_some(), "the XML resolved into a place");
        assert_eq!(merged.graph_id.as_deref(), Some("mdo/Catalog/Товары"));
    }

    /// A common module named in a subsystem's `<Content>` reaches the graph
    /// twice: as the module it is, and as the membership edge's endpoint. Both
    /// land on the file the module is written in, so they fold — and the id the
    /// answer keeps has to be the callable one, whichever provider order the
    /// ranker happened to produce.
    #[test]
    fn a_common_module_answers_with_its_module_id_not_its_membership_endpoint() {
        for (first, second) in [
            ("module/common/Настройки", "mdo/CommonModule/Настройки"),
            ("mdo/CommonModule/Настройки", "module/common/Настройки"),
        ] {
            let db =
                build(&[common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")]);
            let path = "/ws/CommonModules/Настройки/Ext/Module.bsl";
            let node = |id: &str| {
                let mut candidate = NameCandidate::new(
                    "Настройки",
                    NameCategory::CommonModule,
                    NameMatchTier::Name,
                    ProviderId::Graph,
                )
                .with_source_path(path);
                candidate.graph_id = Some(id.to_string());
                candidate
            };
            let graph =
                FakeSource::answering(ProviderId::Graph, vec![node(first), node(second)], 2);

            let found = lookup_names(
                &db,
                &NameQuery::new("Настройки", 20).with_categories(&[NameCategory::CommonModule]),
                &[&graph],
            );

            assert_eq!(found.candidates.len(), 1, "{:?}", displays(&found));
            assert_eq!(
                found.candidates[0].graph_id.as_deref(),
                Some("module/common/Настройки"),
                "delivered as {first} then {second}",
            );
        }
    }

    /// The graph holds a node for every method; `module_members` publishes only
    /// the exported ones. So a private `Тест` in one module and an exported
    /// `Тест` in another leave ONE placed candidate against TWO graph rows, and
    /// a fold that only checks its own side would hang the private method's id
    /// on the exported method's row — one answer whose `location` and
    /// `graph_id` point at different code, which is worse than the two rows it
    /// replaced.
    #[test]
    fn a_durable_id_is_never_hung_on_a_namesake_in_another_module() {
        let db = build(&[
            common_module("Утил", "Процедура Тест() Экспорт\nКонецПроцедуры\n"),
            common_module("Другое", "Процедура Тест()\nКонецПроцедуры\n"),
        ]);
        // Sorted as the real ranker sorts them — by id — so the row for the
        // OTHER module comes first, and a merge that took whatever it met first
        // would take this one.
        let graph = FakeSource::answering(
            ProviderId::Graph,
            vec![
                graph_candidate_in(
                    "Тест",
                    "method/common/Другое/Тест",
                    "/ws/CommonModules/Другое/Ext/Module.bsl",
                ),
                graph_candidate_in(
                    "Тест",
                    "method/common/Утил/Тест",
                    "/ws/CommonModules/Утил/Ext/Module.bsl",
                ),
            ],
            2,
        );

        let found =
            lookup_names(&db, &NameQuery::new("Тест", 20).with_categories(METHODS), &[&graph]);

        let exported = found
            .candidates
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Утил.Тест"))
            .expect("the exported method is an answer");
        assert_eq!(
            exported.graph_id.as_deref(),
            Some("method/common/Утил/Тест"),
            "the id on it must name the module it is written in",
        );
        // The namesake in the other module is an answer of its own, filed under
        // its own file — not merged into the row above.
        let other = found
            .candidates
            .iter()
            .find(|c| c.graph_id.as_deref() == Some("method/common/Другое/Тест"))
            .expect("the namesake in the other module is an answer too");
        assert_ne!(other.place, exported.place);
    }

    /// The answer this action exists to give: the graph is built, the resident
    /// is not, and a database with no source root at all is what the caller
    /// hands over. Asking it for a root is fine; asserting one is a panic, and a
    /// panic here is an internal error instead of the candidates the graph and
    /// the platform were ready to give.
    #[test]
    fn a_ready_graph_answers_over_a_database_with_no_source_root() {
        let db = RootDatabaseImpl::new();
        let graph = FakeSource::answering(
            ProviderId::Graph,
            vec![graph_candidate_in(
                "Тест",
                "method/common/Утил/Тест",
                "/ws/CommonModules/Утил/Ext/Module.bsl",
            )],
            1,
        );

        let found = lookup_names(
            &db,
            &NameQuery::new("Тест", 20)
                .with_categories(METHODS)
                .with_workspace(ProviderState::NotReady),
            &[&graph],
        );

        assert_eq!(found.candidates.len(), 1, "{:?}", displays(&found));
        assert_eq!(found.candidates[0].graph_id.as_deref(), Some("method/common/Утил/Тест"));
        // No root, so no place — and that is said by omission, not by a guess.
        assert!(found.candidates[0].place.is_none());
    }

    /// A provider that hit its own cap delivers SOME of its matches, and which
    /// ones is the ranker's business. What arrives must still land on the file
    /// it names — a merge that reasoned from how many rows it happened to
    /// receive would put a capped provider's one row on whatever else carried
    /// the name.
    #[test]
    fn a_capped_provider_still_lands_its_row_on_the_file_it_names() {
        let db = build(&[
            common_module("Утил", "Процедура Тест() Экспорт\nКонецПроцедуры\n"),
            common_module("Другое", "Процедура Тест() Экспорт\nКонецПроцедуры\n"),
        ]);
        // Two matched, one was delivered: `total` above the list is the cap.
        let graph = FakeSource::answering(
            ProviderId::Graph,
            vec![graph_candidate_in(
                "Тест",
                "method/common/Другое/Тест",
                "/ws/CommonModules/Другое/Ext/Module.bsl",
            )],
            2,
        );

        let found =
            lookup_names(&db, &NameQuery::new("Тест", 20).with_categories(METHODS), &[&graph]);

        let carrying: Vec<_> = found
            .candidates
            .iter()
            .filter(|c| c.graph_id.is_some())
            .map(|c| c.symbol.as_deref())
            .collect();
        assert_eq!(carrying, vec![Some("Другое.Тест")], "{:?}", found.candidates);
    }

    /// One name, two files — a configuration module and the extension module
    /// beside it — are two answers, and the durable id belongs to exactly one of
    /// them. The graph says which by naming its file; the case that used to need
    /// a guard needs none, because there is nothing left to guess.
    #[test]
    fn a_name_shared_by_two_files_takes_the_id_to_the_file_it_names() {
        let base = "/ws/src/cf/CommonModules/Настройки/Ext/Module.bsl";
        let extension = "/ws/src/cfe/CommonModules/Настройки/Ext/Module.bsl";
        let body = "Процедура Ф() Экспорт\nКонецПроцедуры\n";
        let mut db = db_with_files(&[(base, body), (extension, body)]);
        let base_root = std::path::PathBuf::from("/ws/src/cf");
        let extension_root = std::path::PathBuf::from("/ws/src/cfe");
        db.set_all_config_paths(vec![
            (None, base_root.clone()),
            (Some("Расширение".to_string()), extension_root.clone()),
        ]);
        for (root, file_id) in [(&base_root, FileId(0)), (&extension_root, FileId(1))] {
            db.set_metadata_listing(
                &root.to_string_lossy(),
                MetadataListingData {
                    common_modules: vec![CommonModuleEntry {
                        name: "Настройки".to_string(),
                        main: file_id,
                        module_file: Some(file_id),
                        unread_module_file: None,
                    }],
                    ..MetadataListingData::default()
                },
            );
        }
        let mut node = NameCandidate::new(
            "Настройки",
            NameCategory::CommonModule,
            NameMatchTier::Name,
            ProviderId::Graph,
        );
        node.graph_id = Some("module/common/Настройки".to_string());
        // The graph's node is the EXTENSION's module, and it says so.
        let node = node.with_source_path(extension);
        let graph = FakeSource::answering(ProviderId::Graph, vec![node], 1);

        let found = lookup_names(
            &db,
            &NameQuery::new("Настройки", 20).with_categories(&[NameCategory::CommonModule]),
            &[&graph],
        );

        // Two modules, two answers; the id sits on the one the graph named.
        assert_eq!(found.candidates.len(), 2, "{:?}", displays(&found));
        let carrying: Vec<_> = found
            .candidates
            .iter()
            .filter(|c| c.graph_id.is_some())
            .filter_map(|c| c.place.map(|p| p.file_id))
            .collect();
        assert_eq!(carrying, vec![FileId(1)], "{:?}", found.candidates);
    }

    /// What `workspace/symbol` needs: a candidate it cannot jump to is not one
    /// it can offer, and asking for that must not turn into a silent filter
    /// downstream.
    #[test]
    fn requiring_a_location_drops_what_has_none() {
        let db = build(&[]);
        let placeless = FakeSource::answering(
            ProviderId::Graph,
            vec![graph_candidate("Тест", "method/common/Настройки/Тест")],
            1,
        );

        let query = NameQuery::new("Тест", 20).with_categories(METHODS).requiring_location();
        let found = lookup_names(&db, &query, &[&placeless]);

        assert!(found.candidates.is_empty());
    }

    /// The platform is a process singleton, not a workspace artefact: on a host
    /// that has built nothing at all it still answers, and that is what makes a
    /// name findable before anything is indexed.
    ///
    /// The neighbouring test cannot see this. It asks for a name nothing holds,
    /// so its list is empty whether the platform was consulted or reported
    /// `not_ready` and skipped — the input has to be a name the platform DOES
    /// hold.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn the_platform_answers_on_a_host_that_has_built_nothing() {
        let db = build(&[]);

        let found =
            look(&db, &NameQuery::new("СтрНайти", 20).with_workspace(ProviderState::NotReady));

        assert_eq!(found.state_of(ProviderId::Platform), Some(ProviderState::Answered));
        assert_eq!(found.state_of(ProviderId::ModuleMembers), Some(ProviderState::NotReady));
        assert!(
            found.candidates.iter().any(|c| c.category == NameCategory::PlatformMember),
            "{:?}",
            found.candidates,
        );
        // The three database-backed sources are still missing, and the answer
        // has to keep saying so.
        assert!(found.is_partial());
    }

    /// An empty answer means one of two different things, and the difference is
    /// the whole point of naming providers.
    #[test]
    fn an_empty_answer_from_an_unbuilt_index_is_not_a_proven_zero() {
        let db = build(&[common_module("Настройки", "Процедура Ф() Экспорт\nКонецПроцедуры\n")]);

        let proven = look(&db, &NameQuery::new("НетТакого", 20));
        assert!(proven.candidates.is_empty());
        assert!(!proven.is_partial(), "every provider answered");

        let unbuilt =
            look(&db, &NameQuery::new("НетТакого", 20).with_workspace(ProviderState::NotReady));
        assert!(unbuilt.candidates.is_empty());
        assert!(unbuilt.is_partial());
        assert_eq!(unbuilt.state_of(ProviderId::ModuleMembers), Some(ProviderState::NotReady));
    }
}
