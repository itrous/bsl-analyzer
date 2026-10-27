use hir::{
    normalize_match_name, normalize_usage_name, Definition, Name, ReferenceScope, SemanticSymbol,
    SemanticSymbolKey, Semantics,
};
use ide_db::base_db::{RootQueryDb, SourceDatabase};
use ide_db::{RootDatabase, RootDatabaseImpl};
use rustc_hash::FxHashSet;
use salsa::Database;
use syntax::{SyntaxKind, TextRange, TextSize};
use vfs::FileId;

use crate::declarations::{
    classify_unreferenceable, resolve_declarations, Declaration, DeclarationKind,
    UnsupportedCategory,
};
use crate::name_lookup::{lookup_names, NameLookupResult, NameMatchTier, NameQuery};
use crate::reference_kind::{classify_reference_token, ReferenceKind};
use crate::Location;

pub fn find_references<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Vec<Location> {
    let _span = tracing::info_span!("find_references", ?file_id).entered();

    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let token = match root.token_at_offset(offset).right_biased() {
        Some(t) if t.kind().is_name_token() => t,
        _ => return Vec::new(),
    };

    let sema = Semantics::new(db);
    let symbol = match sema.symbol_for_token(file_id, &token) {
        Some(symbol) => symbol,
        None => return Vec::new(),
    };

    let scope = symbol.reference_scope(db);
    tracing::debug!(
        ?scope,
        target_name = %symbol.name.as_str(),
        "Reference scope determined"
    );

    let files_to_search: Vec<FileId> = match scope {
        ReferenceScope::FileLocal => vec![file_id],
        ReferenceScope::Unknown => return Vec::new(),
        ReferenceScope::ModuleSymbolWorkspace => {
            workspace_candidate_files(db, file_id, &symbol.name)
        }
    };

    let mut all_references = Vec::new();
    for &search_file_id in &files_to_search {
        db.unwind_if_revision_cancelled();
        let references = find_references_in_file(db, search_file_id, &symbol);
        all_references.extend(references);
    }

    tracing::info!(
        total_references = all_references.len(),
        files_searched = files_to_search.len(),
        "Find references completed"
    );

    all_references
}

fn workspace_candidate_files<DB: RootDatabase>(
    db: &DB,
    current_file: FileId,
    target_name: &Name,
) -> Vec<FileId> {
    let source_root_input = db.file_source_root_input(current_file);
    let source_root_id = source_root_input.source_root_id(db);
    let aggregator = db.name_usage_index(source_root_id);
    let normalized = normalize_usage_name(target_name);
    aggregator.files_with(&normalized).to_vec()
}

pub(crate) fn find_references_in_file<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    target_symbol: &SemanticSymbol,
) -> Vec<Location> {
    let _span = tracing::debug_span!("find_references_in_file", ?file_id).entered();

    // Popular names (standard event handlers) yield hundreds of candidate
    // files; the memoised per-file offsets replace a full token walk per
    // request with a lookup, so only the actual occurrences pay resolution.
    let normalized = normalize_match_name(&target_symbol.name);
    let occurrences = db.file_name_offsets_ref(file_id);
    let offsets = occurrences.offsets(&normalized);
    if offsets.is_empty() {
        return Vec::new();
    }

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let sema = Semantics::new(db);

    let mut references = Vec::new();

    for &offset in offsets {
        let Some(token) = root.token_at_offset(TextSize::from(offset)).right_biased() else {
            continue;
        };
        if !token.kind().is_name_token()
            || u32::from(token.text_range().start()) != offset
            || !Name::new(token.text()).eq_ignore_case(&target_symbol.name)
        {
            continue;
        }

        let Some(candidate_symbol) = sema.symbol_for_token(file_id, &token) else {
            continue;
        };

        if candidate_symbol.key == target_symbol.key {
            let range = token.text_range();
            references.push(Location { file_id, range });
        }
    }

    tracing::debug!(
        count = references.len(),
        target_name = %target_symbol.name.as_str(),
        "Found references"
    );

    references
}

// --- reference search by name -----------------------------------------------------------

/// The files an area or an anchor is confined to. A set of ids and not a path
/// prefix: a root is declared with one spelling and indexed with another
/// whenever a symlink is involved, and a prefix comparison would then match
/// nothing while reporting an honest-looking zero.
pub type FileIdSet = FxHashSet<FileId>;

/// How many dictionary candidates one anchor resolution may consider.
const ANCHOR_CANDIDATE_LIMIT: usize = 64;

/// What the caller points at.
#[derive(Debug, Clone)]
pub enum ReferenceAnchor {
    /// A qualified name of one to three segments, or a short one.
    Name(String),
    /// 0-based line and 0-based character offset within that line.
    Position { file_id: FileId, line: u32, column: u32 },
    /// The text of the line the caller read, which certifies the coordinates it
    /// came with. `line` and `column` narrow it; they do not address it, so a
    /// file that moved under the caller relocates the anchor instead of silently
    /// answering about whatever token now stands at those coordinates.
    Text { file_id: FileId, line: Option<u32>, column: Option<u32>, content: String },
}

/// Why the quoted line no longer anchors anything.
///
/// One code per reachable state, because they call for different moves: a quote
/// that is nowhere in the file is a stale read, a quote with no name in it is a
/// mis-aimed one, and a name that resolves to nothing is neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorStaleReason {
    /// No line of the file contains the quoted text.
    NotInFile,
    /// A line matched, and the token `column` aims at does not occur in the quote.
    ColumnOutsideQuotedText,
    /// Lines matched, and no name on any of them occurs in the quote.
    NoNameInQuotedText,
    /// A name was quoted, and semantics derives no symbol from it.
    NameNotResolved,
}

impl AnchorStaleReason {
    pub const ALL: [Self; 4] = [
        Self::NotInFile,
        Self::ColumnOutsideQuotedText,
        Self::NoNameInQuotedText,
        Self::NameNotResolved,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInFile => "not_in_file",
            Self::ColumnOutsideQuotedText => "column_outside_quoted_text",
            Self::NoNameInQuotedText => "no_name_in_quoted_text",
            Self::NameNotResolved => "name_not_resolved",
        }
    }
}

/// One place a text anchor could have meant — what `declarations` are to an
/// ambiguity by name: the list the caller picks from.
#[derive(Debug, Clone)]
pub struct AnchorSite {
    pub file_id: FileId,
    /// The name token itself.
    pub range: TextRange,
    /// The method it sits in, when it sits in one.
    pub enclosing_range: Option<TextRange>,
    /// The qualified name that addresses this symbol, when one does. Absent
    /// rather than guessed: an address the surface would refuse is worse than
    /// none, because the caller spends a round trip proving it wrong.
    pub symbol: Option<String>,
    /// This place stands on the line the request named. A quote that names several symbols
    /// is not resolved by a coordinate the caller itself distrusts — but the caller is shown
    /// which place that coordinate meant, so one more call settles it.
    pub pointed_by_line: bool,
}

/// Which part of the workspace the answer is confined to.
#[derive(Debug, Clone, Default)]
pub struct ReferenceArea {
    /// `None` — the whole workspace.
    pub files: Option<FileIdSet>,
}

#[derive(Debug, Clone)]
pub struct ReferencesRequest {
    pub anchor: ReferenceAnchor,
    /// Files among which the DECLARATION is looked for. `None` — the whole
    /// workspace. Narrows the candidate set before its size is counted, so it
    /// is the way out of [`ReferencesOutcome::Ambiguous`] — unlike
    /// [`ReferenceArea`], which narrows the references already found.
    pub anchor_files: Option<FileIdSet>,
    pub area: ReferenceArea,
    /// `None` — every kind.
    pub kinds: Option<Vec<ReferenceKind>>,
    pub include_declaration: bool,
    /// Cap on candidate files walked. Reaching it makes the total a lower
    /// bound, which the caller is told about rather than left to guess.
    pub max_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceHit {
    pub file_id: FileId,
    /// The occurrence itself.
    pub range: TextRange,
    /// The method the occurrence sits in, when it sits in one.
    pub enclosing_range: Option<TextRange>,
    pub kind: ReferenceKind,
}

/// Whether the symbol was determined at all — a different question from whether
/// the answer is complete, which the freshness envelope answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferencesOutcome {
    /// The anchor is one symbol; the hit list IS the answer, and an empty list
    /// means exactly zero references.
    Resolved,
    /// More than one symbol answers to this anchor; no references are counted.
    Ambiguous,
    /// Nothing matched exactly enough to anchor on.
    NotFound,
    /// The name was resolved precisely, but nothing can enumerate references to
    /// it. Told apart from a zero-length list on purpose: a client renames on
    /// the strength of that zero.
    UnsupportedSymbol { category: UnsupportedCategory },
    /// The quoted line does not describe this file any more. Kept apart from
    /// `NotFound`, which means "nothing matched exactly": here the caller's
    /// picture of the file and ours parted, and the caller has to be told which
    /// of the two moved rather than handed a confident list about another symbol.
    AnchorStale {
        reason: AnchorStaleReason,
        /// How many lines of the file carry the quoted text — zero says the
        /// quote is gone, more than zero says it is the aim that missed.
        line_matches: usize,
    },
}

/// Whose data COMPOSES the body of the answer — not who was asked along the way.
///
/// The location contract gives the name dictionary a null revision and a null
/// topology fingerprint, because several artefacts with different revisions
/// stand behind it. Stamping that on a body the resident composed would erase
/// the fingerprint the caller needs; stamping the resident's revision on a body
/// of dictionary candidates would claim an identity those candidates do not
/// share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Resident,
    NameDictionary,
}

#[derive(Debug, Clone)]
pub struct ReferencesResult {
    pub outcome: ReferencesOutcome,
    pub body_source: BodySource,
    /// Every reference that survived the area and kind filters, before any
    /// display limit. Empty for every outcome other than `Resolved`.
    pub hits: Vec<ReferenceHit>,
    /// Per-file counts over the same set as `hits`, most-populated first.
    pub per_file: Vec<(FileId, usize)>,
    /// The declarations found when the anchor is a qualified name — one when
    /// resolved, several when ambiguous.
    pub declarations: Vec<Declaration>,
    /// The dictionary's answer, when the dictionary is what composed the body.
    pub candidates: Option<NameLookupResult>,
    pub files_scanned: usize,
    /// `max_files` cut the walk short: `hits.len()` is a lower bound, and two
    /// answers taken under this flag are not comparable with each other.
    pub files_capped: bool,
    /// The exact matches alone filled the dictionary's candidate budget, so one more
    /// may have been cut before the anchor was chosen. Unlike [`Self::files_capped`],
    /// this cap can change the OUTCOME itself — a declaration that fell off the list is
    /// one the answer could have resolved to or been ambiguous about — so it travels
    /// with every outcome, not just `Resolved`.
    pub anchor_candidates_capped: bool,
    /// The line a text anchor actually landed on. `None` for every other anchor:
    /// a name has no line, and a position stands where it was told to.
    pub anchor_line: Option<u32>,
    /// The places a text anchor could not choose between. Empty for every
    /// outcome other than an ambiguity the quoted line produced.
    pub anchor_sites: Vec<AnchorSite>,
}

impl ReferencesResult {
    fn outcome_only(outcome: ReferencesOutcome, body_source: BodySource) -> Self {
        Self {
            outcome,
            body_source,
            hits: Vec::new(),
            per_file: Vec::new(),
            declarations: Vec::new(),
            candidates: None,
            files_scanned: 0,
            files_capped: false,
            anchor_candidates_capped: false,
            anchor_line: None,
            anchor_sites: Vec::new(),
        }
    }
}

/// Find every reference to the symbol the request anchors on.
/// `external` are the name sources beyond the resident's own tables — the call graph among
/// them. They take part in ANCHORING: a name only the graph knows can be anchored on, and a
/// source's state is what `providers` reports, so passing none leaves every external source
/// reported as never consulted.
pub fn find_references_by_name(
    db: &RootDatabaseImpl,
    req: &ReferencesRequest,
    external: &[&dyn crate::name_lookup::ExternalNameSource],
) -> ReferencesResult {
    let _span = tracing::info_span!("find_references_by_name").entered();

    match anchor_references(db, req, external) {
        Anchored::Answered(result) => result,
        Anchored::Walk(walk) => {
            let mut hits = Vec::new();
            for &file_id in walk.scanned() {
                db.unwind_if_revision_cancelled();
                hits.extend(walk_reference_file(db, file_id, walk.symbol(), req));
            }
            finish_references(db, walk, hits)
        }
    }
}

/// The walk one anchored symbol calls for, before any file of it is entered.
///
/// Everything in it is a value: the walk is handed from one database handle to another
/// by a caller that drops the handle between the files it enters, so nothing here may
/// borrow the database.
#[derive(Debug, Clone)]
pub struct ReferenceWalk {
    symbol: SemanticSymbol,
    /// The candidate files in walk order, already cut to `max_files`.
    scanned: Vec<FileId>,
    files_capped: bool,
    /// The declarations an anchor by name resolved, which outrank the one the symbol
    /// itself names; `None` for the other anchors.
    declarations: Option<Vec<Declaration>>,
    anchor_line: Option<u32>,
    anchor_candidates_capped: bool,
}

impl ReferenceWalk {
    pub fn symbol(&self) -> &SemanticSymbol {
        &self.symbol
    }

    /// The files the walk enters, in order. `files_scanned` of the result is their
    /// count whatever the walk finds in them.
    pub fn scanned(&self) -> &[FileId] {
        &self.scanned
    }
}

/// What anchoring settled: an answer that needs no walk, or the walk to run.
#[derive(Debug, Clone)]
pub enum Anchored {
    Answered(ReferencesResult),
    Walk(ReferenceWalk),
}

impl Anchored {
    /// The fields an anchoring branch settles AFTER it knows the symbol go into both
    /// shapes alike: an answer that skipped the walk still carries the declarations a
    /// graph id is derived from and the line a text anchor landed on.
    fn with_declarations(self, declarations: Vec<Declaration>) -> Self {
        match self {
            Self::Answered(result) => Self::Answered(ReferencesResult { declarations, ..result }),
            Self::Walk(walk) => {
                Self::Walk(ReferenceWalk { declarations: Some(declarations), ..walk })
            }
        }
    }

    fn with_anchor_line(self, anchor_line: u32) -> Self {
        let anchor_line = Some(anchor_line);
        match self {
            Self::Answered(result) => Self::Answered(ReferencesResult { anchor_line, ..result }),
            Self::Walk(walk) => Self::Walk(ReferenceWalk { anchor_line, ..walk }),
        }
    }

    fn with_anchor_candidates_capped(self, anchor_candidates_capped: bool) -> Self {
        match self {
            Self::Answered(result) => {
                Self::Answered(ReferencesResult { anchor_candidates_capped, ..result })
            }
            Self::Walk(walk) => Self::Walk(ReferenceWalk { anchor_candidates_capped, ..walk }),
        }
    }
}

/// Resolve the anchor and decide which files to walk — the first of the three steps
/// [`find_references_by_name`] is composed of, for a caller that walks the files itself
/// ([`walk_reference_file`]) and reduces the hits at the end ([`finish_references`]).
pub fn anchor_references(
    db: &RootDatabaseImpl,
    req: &ReferencesRequest,
    external: &[&dyn crate::name_lookup::ExternalNameSource],
) -> Anchored {
    match &req.anchor {
        ReferenceAnchor::Name(name) => resolve_by_name(db, name, req, external),
        ReferenceAnchor::Position { file_id, line, column } => {
            resolve_by_position(db, *file_id, *line, *column, req)
        }
        ReferenceAnchor::Text { file_id, line, column, content } => {
            resolve_by_text(db, *file_id, *line, *column, content, req)
        }
    }
}

/// Stage one, then stage two — and they are not interchangeable. A qualified
/// name is matched against declarations; the name dictionary compares the whole
/// needle with a short member name and would score `Продажи.Расчёт` against
/// `Расчёт` at no tier at all.
fn resolve_by_name(
    db: &RootDatabaseImpl,
    name: &str,
    req: &ReferencesRequest,
    external: &[&dyn crate::name_lookup::ExternalNameSource],
) -> Anchored {
    let mut declarations = resolve_declarations(db, name);
    if let Some(files) = &req.anchor_files {
        declarations.retain(|decl| files.contains(&decl.file_id));
    }

    match declarations.len() {
        1 => {
            let declaration = declarations[0];
            let Some(symbol) = symbol_at(db, declaration.file_id, declaration.name_range.start())
            else {
                // The declaration is there and its symbol is not: nothing knows
                // where to look, which is what this category says.
                return Anchored::Answered(ReferencesResult::outcome_only(
                    ReferencesOutcome::UnsupportedSymbol {
                        category: UnsupportedCategory::UnknownScope,
                    },
                    BodySource::Resident,
                ));
            };
            begin_walk(db, declaration.file_id, symbol, req).with_declarations(declarations)
        }
        0 => {
            let dictionary = resolve_by_dictionary(db, name, req, external);
            // The dictionary is holding a declaration this name can be walked from, so
            // the string is not "something no walk enumerates" however the card surface
            // reads it. Asking the card first is what made a module's own `Сообщить` a
            // platform member: `symbol_info` answers about the platform for a bare name,
            // and it is right about the string — but the walk exists all the same.
            if dictionary.anchorable {
                return dictionary.result;
            }
            match classify_unreferenceable(db, name) {
                Some(category) => Anchored::Answered(ReferencesResult::outcome_only(
                    ReferencesOutcome::UnsupportedSymbol { category },
                    BodySource::Resident,
                )),
                None => dictionary.result,
            }
        }
        _ => Anchored::Answered(ReferencesResult {
            declarations,
            ..ReferencesResult::outcome_only(ReferencesOutcome::Ambiguous, BodySource::Resident)
        }),
    }
}

/// Stage two: a short or inexact name, answered by the name dictionary.
///
/// Only `Exact` and `CaseInsensitive` can anchor — a prefix or a substring
/// match names a different symbol, and counting its references would answer a
/// question nobody asked.
/// What stage two made of the name, and whether the dictionary held a declaration the
/// name could be walked from at all — the question that decides whether the card surface
/// gets to call the name unwalkable.
struct DictionaryAnswer {
    result: Anchored,
    anchorable: bool,
}

fn resolve_by_dictionary(
    db: &RootDatabaseImpl,
    name: &str,
    req: &ReferencesRequest,
    external: &[&dyn crate::name_lookup::ExternalNameSource],
) -> DictionaryAnswer {
    let query = NameQuery::new(name, ANCHOR_CANDIDATE_LIMIT);
    let lookup = lookup_names(db, &query, external);

    let exact: Vec<&crate::name_lookup::NameCandidate> = lookup
        .candidates
        .iter()
        .filter(|candidate| {
            matches!(candidate.match_tier, NameMatchTier::Exact | NameMatchTier::CaseInsensitive)
        })
        .collect();

    // Whether a reference walk EXISTS is decided before any narrowing: an exact
    // match with a range of its own is a declaration something can be walked
    // from, whatever file set the caller passed.
    let anchorable: Vec<&crate::name_lookup::NameCandidate> = exact
        .iter()
        .copied()
        .filter(|candidate| candidate.place.is_some_and(|place| place.range.is_some()))
        .collect();

    // The anchor's file set narrows WHICH DECLARATION is taken, so it applies to
    // the candidates that could be one. Applying it a step earlier would drop a
    // platform member — which belongs to no root by construction — and turn
    // `unsupported_symbol` into `not_found`: a narrowing parameter would be
    // changing the CLASS of the answer, not the set it is drawn from.
    let anchored: Vec<&crate::name_lookup::NameCandidate> = anchorable
        .iter()
        .copied()
        .filter(|candidate| match &req.anchor_files {
            Some(files) => candidate.place.is_some_and(|place| files.contains(&place.file_id)),
            None => true,
        })
        .collect();

    // What could never be an anchor, whatever the file set: an exact match with no
    // range of its own. Kept apart from "an anchor that this file set excludes",
    // because only the first means "no reference walk exists for this symbol".
    let unanchorable: Vec<UnsupportedCategory> = exact
        .iter()
        .filter(|candidate| candidate.place.is_none_or(|place| place.range.is_none()))
        .filter_map(|candidate| UnsupportedCategory::from_name_category(candidate.category))
        .collect();

    let result = match anchored.len() {
        1 => {
            let place = anchored[0].place.expect("filtered on a place");
            let range = place.range.expect("filtered on a range");
            match symbol_at(db, place.file_id, range.start()) {
                // The body is the reference list the resident walked, even
                // though it was the dictionary that found the anchor.
                Some(symbol) => begin_walk(db, place.file_id, symbol, req),
                None => Anchored::Answered(ReferencesResult {
                    candidates: Some(lookup.clone()),
                    ..ReferencesResult::outcome_only(
                        ReferencesOutcome::NotFound,
                        BodySource::NameDictionary,
                    )
                }),
            }
        }
        // Matched exactly, but nothing to walk: a metadata object, a platform
        // member, a module as a whole. Saying `not_found` here would deny a
        // name the search did find — while saying it about a method the anchor's
        // file set merely excluded would deny a walk that exists. A short name
        // is regularly both at once (a module method named like a platform
        // member), so the walk's existence is read off `anchorable`, which the
        // file set never touched, and not off "nothing anchored here".
        0 if anchorable.is_empty() && !unanchorable.is_empty() => {
            let category = unanchorable[0];
            Anchored::Answered(ReferencesResult {
                candidates: Some(lookup.clone()),
                ..ReferencesResult::outcome_only(
                    ReferencesOutcome::UnsupportedSymbol { category },
                    BodySource::NameDictionary,
                )
            })
        }
        0 => Anchored::Answered(ReferencesResult {
            candidates: Some(lookup.clone()),
            ..ReferencesResult::outcome_only(
                ReferencesOutcome::NotFound,
                BodySource::NameDictionary,
            )
        }),
        _ => Anchored::Answered(ReferencesResult {
            candidates: Some(lookup.clone()),
            ..ReferencesResult::outcome_only(
                ReferencesOutcome::Ambiguous,
                BodySource::NameDictionary,
            )
        }),
    };

    DictionaryAnswer {
        // The list is sorted by tier before it is cut, so exact matches are the last
        // to go: they are lost only once they fill the budget by themselves.
        // `NameLookupResult::truncated` is the wrong signal here — it is also true when
        // an unrelated provider capped its own prefix matches.
        result: result.with_anchor_candidates_capped(exact.len() >= ANCHOR_CANDIDATE_LIMIT),
        anchorable: !anchorable.is_empty(),
    }
}

/// The fallback anchor: a position, for what no name addresses — a local, a
/// parameter.
///
/// Each way of missing gets its own outcome. Today's `find_references` folds
/// three different facts into one empty vector, and that is the defect this
/// surface exists to close.
fn resolve_by_position(
    db: &RootDatabaseImpl,
    file_id: FileId,
    line: u32,
    column: u32,
    req: &ReferencesRequest,
) -> Anchored {
    let not_found = || {
        Anchored::Answered(ReferencesResult::outcome_only(
            ReferencesOutcome::NotFound,
            BodySource::Resident,
        ))
    };
    let Some(offset) = crate::name_lookup::offset_for_line_col(db, file_id, line, column) else {
        return not_found();
    };
    let Some(symbol) = symbol_at(db, file_id, offset) else {
        return not_found();
    };
    begin_walk(db, file_id, symbol, req)
}

/// The self-certifying anchor: the caller quotes the line it read, and the quote
/// is checked against the file before anything is counted.
///
/// The positional anchor cannot do this. A client that read a file, kept
/// `line`/`column` and sent them after someone else edited gets a confident list
/// about whatever token now stands there — the one silent wrong answer this
/// surface exists to close. Here `line` and `column` only narrow: when the
/// quoted text moved, the anchor moves with it and says so.
fn resolve_by_text(
    db: &RootDatabaseImpl,
    file_id: FileId,
    line: Option<u32>,
    column: Option<u32>,
    content: &str,
    req: &ReferencesRequest,
) -> Anchored {
    let stale = |reason: AnchorStaleReason, line_matches: usize| {
        ReferencesResult::outcome_only(
            ReferencesOutcome::AnchorStale { reason, line_matches },
            BodySource::Resident,
        )
    };

    let needle = content.trim();
    // `contains("")` is true of every line, so an empty quote would report itself found on
    // all of them — the file's length dressed up as evidence.
    if needle.is_empty() {
        return Anchored::Answered(stale(AnchorStaleReason::NotInFile, 0));
    }
    let text = db.file_text(file_id);
    let index =
        ide_db::RootDatabase::line_index(db, ide_db::base_db::FileIdInput::new(db, file_id));

    // Every place the quote actually STANDS, as a range of the file — not merely which lines
    // carry it. A quote is usually a slice of a line, and comparing names against the whole
    // line accepted a token that shares a name with something in the quote while standing
    // outside it: `Продажи.Расчёт(); Склад.Расчёт();` quoted as `Продажи.Расчёт()` picked up
    // the OTHER call's `Расчёт` and answered `ambiguous` about a quote that named exactly one
    // occurrence. Containment also settles the half-quoted name for free: a quote cutting an
    // identifier holds no whole token, so it certifies nothing and says so.
    //
    // Exact comparison, no case folding: the caller quotes what it read, and folding would
    // match a DIFFERENT spelling — the very likeness-instead-of-identity match this anchor
    // exists to rule out.
    let mut spans: Vec<TextRange> = Vec::new();
    let mut matching_lines: Vec<u32> = Vec::new();
    let mut number = 0u32;
    while let Some(line_text) = index.safe_line_str(&text, number) {
        db.unwind_if_revision_cancelled();
        let Some(line_start) = index.try_line_start(number) else { break };
        let before = spans.len();
        for (at, _) in line_text.trim_end().match_indices(needle) {
            let from = line_start + TextSize::from(at as u32);
            spans.push(TextRange::at(from, TextSize::of(needle)));
        }
        if spans.len() > before {
            matching_lines.push(number);
        }
        number += 1;
    }
    let line_matches = matching_lines.len();
    if spans.is_empty() {
        return Anchored::Answered(stale(AnchorStaleReason::NotInFile, 0));
    }

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    // A name the caller did not quote is not what it pointed at, even when it shares the line
    // — or the spelling.
    let inside_quote = |token: &syntax::SyntaxToken| {
        token.kind().is_name_token()
            && spans.iter().any(|span| span.contains_range(token.text_range()))
    };

    let (tokens, aimed_by_column) = match column {
        Some(column) => {
            let tokens: Vec<syntax::SyntaxToken> = matching_lines
                .iter()
                .filter_map(|&line| {
                    let offset =
                        crate::name_lookup::offset_for_line_col(db, file_id, line, column)?;
                    root.token_at_offset(offset).right_biased()
                })
                .filter(inside_quote)
                .collect();
            (tokens, true)
        }
        None => {
            // Descendants, not a `next_token()` chain: rowan ends such a walk at the first
            // empty node, and a recovered file has them.
            let tokens: Vec<syntax::SyntaxToken> = root
                .descendants_with_tokens()
                .filter_map(|element| element.into_token())
                .filter(inside_quote)
                .collect();
            (tokens, false)
        }
    };

    if tokens.is_empty() {
        // The axis the caller aimed with is the one the answer names: a column
        // that missed is a different mistake from a quote with no name in it.
        return Anchored::Answered(stale(
            if aimed_by_column {
                AnchorStaleReason::ColumnOutsideQuotedText
            } else {
                AnchorStaleReason::NoNameInQuotedText
            },
            line_matches,
        ));
    }

    // Deduplicated by SYMBOL, not by token: one symbol quoted on five lines is
    // one answer, and there is nothing for the caller to choose between.
    //
    // The cap counts symbols for the same reason. Counting tokens would raise the flag on an
    // ordinary long method that writes one variable sixty-five times — where the outcome
    // could not have been different — and, worse, would drop a SECOND symbol standing past
    // the sixty-fourth token, turning an ambiguity into a silent choice.
    let line_of = |token: &syntax::SyntaxToken| index.line_col(token.text_range().start()).line;

    let mut anchors: Vec<AnchorCandidate> = Vec::new();
    let mut capped = false;
    let sema = Semantics::new(db);
    for token in tokens {
        let Some(symbol) = sema.symbol_for_token(file_id, &token) else { continue };
        let asked_for = line.is_some_and(|asked| asked == line_of(&token));
        if let Some(known) = anchors.iter_mut().find(|known| same_anchor(&known.symbol, &symbol)) {
            // A later token of a symbol already known replaces the stored one only when it
            // stands where the caller pointed: the reported line is then the caller's own,
            // and a relocation is not announced where nothing moved.
            if asked_for && !known.pointed_by_line {
                known.token = token;
                known.pointed_by_line = true;
            }
            continue;
        }
        if anchors.len() == ANCHOR_CANDIDATE_LIMIT {
            capped = true;
            break;
        }
        anchors.push(AnchorCandidate { symbol, token, pointed_by_line: asked_for });
    }

    if anchors.is_empty() {
        return Anchored::Answered(ReferencesResult {
            anchor_candidates_capped: capped,
            ..stale(AnchorStaleReason::NameNotResolved, line_matches)
        });
    }

    if anchors.len() > 1 {
        let sites = anchors
            .iter()
            .map(|candidate| AnchorSite {
                file_id,
                range: candidate.token.text_range(),
                enclosing_range: enclosing_method_range(&candidate.token),
                symbol: crate::name_lookup::qualified_method_symbol(db, &candidate.symbol),
                pointed_by_line: candidate.pointed_by_line,
            })
            .collect();
        return Anchored::Answered(ReferencesResult {
            anchor_sites: sites,
            anchor_candidates_capped: capped,
            ..ReferencesResult::outcome_only(ReferencesOutcome::Ambiguous, BodySource::Resident)
        });
    }

    let anchor = anchors.swap_remove(0);
    begin_walk(db, file_id, anchor.symbol, req)
        .with_anchor_line(line_of(&anchor.token))
        .with_anchor_candidates_capped(capped)
}

/// One symbol the quote could name, with the token that will speak for it.
struct AnchorCandidate {
    symbol: SemanticSymbol,
    token: syntax::SyntaxToken,
    /// This candidate stands on the line the caller sent. It decides nothing — the quote
    /// does the choosing — and it is published so a caller looking at several places can see
    /// which one its own coordinate meant.
    pointed_by_line: bool,
}

/// Whether two candidates are the SAME symbol, for the purpose of choosing an anchor.
///
/// The key IS that identity, and a typed member's key names the member — its receiver and its
/// folded field name — so two readings of one local's field collapse by themselves, while
/// same-named fields of two different locals stay two. The walk and the anchor read one
/// equality; the field type is no substitute, since a scalar field's type is soft (`unknown`)
/// and separates nothing.
fn same_anchor(left: &SemanticSymbol, right: &SemanticSymbol) -> bool {
    match (&left.key, &right.key) {
        // A definition keyed by a name carries the spelling of the occurrence, and BSL does
        // not distinguish spellings: one platform function written twice in two cases is one
        // function. `same_entity` folds exactly the variants where that holds and keeps exact
        // equality for the ones keyed by an id, where two same-named things are genuinely two.
        (SemanticSymbolKey::Definition(left), SemanticSymbolKey::Definition(right)) => {
            left.same_entity(right)
        }
        (left, right) => left == right,
    }
}

/// The method an occurrence sits in, when it sits in one.
fn enclosing_method_range(token: &syntax::SyntaxToken) -> Option<TextRange> {
    token
        .parent_ancestors()
        .find(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .map(|node| node.text_range())
}

fn symbol_at(db: &RootDatabaseImpl, file_id: FileId, offset: TextSize) -> Option<SemanticSymbol> {
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let token = root.token_at_offset(offset).right_biased()?;
    if !token.kind().is_name_token() {
        return None;
    }
    Semantics::new(db).symbol_for_token(file_id, &token)
}

/// Order the hits and count them per file — the walk's reduction step.
///
/// Its own function so it can be cancelled and shown to be cancelled: the walk that
/// produced `hits` observes cancellation at every file, and this phase is as long as
/// the hit list. A sort has no boundary inside it to unwind at, so the checkpoint goes
/// immediately before it; the counting pass does have one, and takes it per hit.
fn order_and_count_by_file(
    db: &RootDatabaseImpl,
    hits: &mut [ReferenceHit],
) -> Vec<(FileId, usize)> {
    db.unwind_if_revision_cancelled();
    hits.sort_by_key(|hit| (hit.file_id.0, hit.range.start()));

    let mut per_file: Vec<(FileId, usize)> = Vec::new();
    for hit in hits.iter() {
        db.unwind_if_revision_cancelled();
        match per_file.last_mut() {
            Some((file_id, count)) if *file_id == hit.file_id => *count += 1,
            _ => per_file.push((hit.file_id, 1)),
        }
    }
    db.unwind_if_revision_cancelled();
    per_file.sort_by_key(|(file_id, count)| (std::cmp::Reverse(*count), file_id.0));
    per_file
}

/// Which files the symbol's references can stand in, cut to the request's cap.
fn begin_walk(
    db: &RootDatabaseImpl,
    home_file: FileId,
    symbol: SemanticSymbol,
    req: &ReferencesRequest,
) -> Anchored {
    let scope = symbol.reference_scope(db);
    let candidate_files: Vec<FileId> = match scope {
        ReferenceScope::FileLocal => vec![home_file],
        // The one case where an empty list is not an answer: nothing knows
        // where to look, so nothing was looked at.
        ReferenceScope::Unknown => {
            return Anchored::Answered(ReferencesResult::outcome_only(
                ReferencesOutcome::UnsupportedSymbol {
                    category: UnsupportedCategory::UnknownScope,
                },
                BodySource::Resident,
            ))
        }
        ReferenceScope::ModuleSymbolWorkspace => {
            workspace_candidate_files(db, home_file, &symbol.name)
        }
    };

    // The area is applied to the candidate files, before the walk: the total
    // must count what the filter admits, not what it later hides.
    let mut scanned: Vec<FileId> = match &req.area.files {
        Some(files) => candidate_files.into_iter().filter(|id| files.contains(id)).collect(),
        None => candidate_files,
    };

    let files_capped = scanned.len() > req.max_files;
    scanned.truncate(req.max_files);

    Anchored::Walk(ReferenceWalk {
        symbol,
        scanned,
        files_capped,
        declarations: None,
        anchor_line: None,
        anchor_candidates_capped: false,
    })
}

/// The hits of `symbol` in one file of the walk, filtered by kind as the request asks —
/// the second step. The cancellation checkpoint is the caller's, taken before each
/// file: one checkpoint per file boundary, however the caller groups the files.
pub fn walk_reference_file(
    db: &RootDatabaseImpl,
    file_id: FileId,
    symbol: &SemanticSymbol,
    req: &ReferencesRequest,
) -> Vec<ReferenceHit> {
    let locations = find_references_in_file(db, file_id, symbol);
    if locations.is_empty() {
        return Vec::new();
    }
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let mut hits = Vec::new();
    for location in locations {
        let Some(token) = root.token_at_offset(location.range.start()).right_biased() else {
            continue;
        };
        let kind = classify_reference_token(&token);
        if let Some(wanted) = &req.kinds {
            if !wanted.contains(&kind) {
                continue;
            }
        }
        if !req.include_declaration && kind == ReferenceKind::Declaration {
            continue;
        }
        let enclosing_range = enclosing_method_range(&token);
        hits.push(ReferenceHit { file_id, range: location.range, enclosing_range, kind });
    }
    hits
}

/// Reduce the hits of every scanned file into the answer — the third step. The order
/// of `hits` does not matter: the reduction sorts, so the answer is the same whatever
/// order the files were walked in.
pub fn finish_references(
    db: &RootDatabaseImpl,
    walk: ReferenceWalk,
    mut hits: Vec<ReferenceHit>,
) -> ReferencesResult {
    let per_file = order_and_count_by_file(db, &mut hits);
    let ReferenceWalk {
        symbol,
        scanned,
        files_capped,
        declarations,
        anchor_line,
        anchor_candidates_capped,
    } = walk;

    ReferencesResult {
        outcome: ReferencesOutcome::Resolved,
        body_source: BodySource::Resident,
        hits,
        per_file,
        declarations: declarations.unwrap_or_else(|| declaration_of(db, &symbol)),
        candidates: None,
        files_scanned: scanned.len(),
        files_capped,
        anchor_candidates_capped,
        anchor_line,
        anchor_sites: Vec::new(),
    }
}

/// The declaration the anchored symbol was declared at, as a one-element list.
///
/// An anchor by NAME resolves declarations of its own and overwrites this. An anchor by
/// position or by quoted line never did, and the answer then carried no declaration at all —
/// so the graph id, which is derived from one, was missing for exactly the same method that
/// carries it when addressed by name. The id is a property of the symbol, not of how a
/// caller spelled its way to it.
fn declaration_of(db: &RootDatabaseImpl, symbol: &hir::SemanticSymbol) -> Vec<Declaration> {
    // Taken from `definition`, not from `declaration`: the latter is filled for locals and
    // implicit locals only, and a method — the very thing the graph addresses — never carries
    // one. Reading it there returns an empty list for exactly the symbols this exists for.
    let Some(definition) = symbol.definition.as_ref() else {
        return Vec::new();
    };
    let kind = match definition {
        Definition::Method(_) => DeclarationKind::Method,
        Definition::Variable(_) => DeclarationKind::Variable,
        _ => return Vec::new(),
    };
    let (Some(module), Some(name_range)) = (definition.module(db), definition.name_range(db))
    else {
        return Vec::new();
    };
    vec![Declaration {
        file_id: module.file_id,
        name_range,
        enclosing_range: definition.source_range(db),
        kind,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, VfsPath};

    fn create_db_with_file(source: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        (db, file_id)
    }

    /// The reduction after the walk answers to the cancel too: ordering the hits and
    /// counting them per file is as long as the hit list, and runs no query.
    #[test]
    fn the_reduction_observes_a_cancelled_request() {
        let (db, file_id) = create_db_with_file("Процедура П()\nКонецПроцедуры\n");
        let mut hits: Vec<ReferenceHit> = (0..20)
            .map(|i| ReferenceHit {
                file_id,
                range: TextRange::new(TextSize::from(i), TextSize::from(i + 1)),
                enclosing_range: None,
                kind: ReferenceKind::Read,
            })
            .collect();

        salsa::Database::cancellation_token(&db).cancel();
        let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            order_and_count_by_file(&db, &mut hits)
        }));

        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "the hits were ordered and counted after the request was cancelled"
        );
    }

    #[test]
    fn test_find_method_references() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    МояПроцедура();
    МояПроцедура();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let def_offset = source.find("МояПроцедура").unwrap();
        let offset = TextSize::from(def_offset as u32);

        let references = find_references(&db, file_id, offset);

        assert!(
            references.len() >= 3,
            "Expected at least 3 references, found {}",
            references.len()
        );

        for loc in &references {
            assert_eq!(loc.file_id, file_id);
            assert!(!loc.range.is_empty());
        }
    }

    #[test]
    fn test_find_variable_references() {
        let source = r#"
Перем МодульнаяПеременная;

Процедура Тест()
    МодульнаяПеременная = 1;
    Результат = МодульнаяПеременная + 2;
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let decl_offset = source.find("МодульнаяПеременная").unwrap();
        let offset = TextSize::from(decl_offset as u32);

        let references = find_references(&db, file_id, offset);

        assert!(
            references.len() >= 3,
            "Expected at least 3 references, found {}",
            references.len()
        );

        for loc in &references {
            assert_eq!(loc.file_id, file_id);
        }
    }

    #[test]
    fn test_find_references_case_insensitive() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    мояпроцедура();
    МОЯПРОЦЕДУРА();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.find("мояпроцедура").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let references = find_references(&db, file_id, offset);

        assert!(
            references.len() >= 3,
            "Expected at least 3 references, found {}",
            references.len()
        );
    }

    #[test]
    fn find_references_survive_edit_that_shifts_offsets() {
        let source = "Процедура МояПроцедура()\nКонецПроцедуры\n\nПроцедура Тест()\n    МояПроцедура();\nКонецПроцедуры\n";
        let (mut db, file_id) = create_db_with_file(source);

        let def_offset = source.find("МояПроцедура").unwrap();
        let before = find_references(&db, file_id, TextSize::from(def_offset as u32));
        assert_eq!(before.len(), 2);

        // Prepend a comment so every occurrence moves; memoised per-file
        // offsets must be recomputed, not replayed at the stale positions.
        let shifted = format!("// сдвиг\n{source}");
        db.set_file_text(file_id, &shifted);

        let def_offset = shifted.find("МояПроцедура").unwrap();
        let after = find_references(&db, file_id, TextSize::from(def_offset as u32));
        assert_eq!(after.len(), 2);
        for loc in &after {
            let start = usize::from(loc.range.start());
            assert_eq!(&shifted[start..start + "МояПроцедура".len()], "МояПроцедура");
        }
    }

    #[test]
    fn find_references_final_sigma_keeps_token_walk_semantics() {
        // Final-sigma pair: `eq_ignore_case`-equal tokens whose contextual
        // `to_lowercase` keys differ. Local-symbol keys normalise via
        // `fold_lower` (`SemanticSymbolKey::BodyLocal`), so the usage never
        // matches the declaration's key: exactly one reference, same as the
        // pre-offsets token walk. The offsets bucket uses the per-char fold
        // (`normalize_match_name`) so its candidate set equals the old
        // `eq_ignore_case` prefilter; this pins that neither a missed token
        // nor a new false match appears for such identifiers.
        let source = "Процедура Тест(ΟΔΟΣ)\n    Рез = οδοσ + 1;\nКонецПроцедуры\n";
        let (db, file_id) = create_db_with_file(source);

        let decl_offset = source.find("ΟΔΟΣ").unwrap();
        let references = find_references(&db, file_id, TextSize::from(decl_offset as u32));
        assert_eq!(references.len(), 1, "declaration only, got {references:?}");
        assert_eq!(usize::from(references[0].range.start()), decl_offset);
    }

    #[test]
    fn test_find_references_not_found() {
        let source = r#"
Процедура Тест()
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let offset = source.find("Процедура").unwrap();
        let offset = TextSize::from(offset as u32);

        let references = find_references(&db, file_id, offset);
        assert!(references.is_empty());
    }

    #[test]
    fn test_find_references_from_usage() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    МояПроцедура();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.rfind("МояПроцедура").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let references = find_references(&db, file_id, offset);

        assert!(
            references.len() >= 2,
            "Expected at least 2 references, found {}",
            references.len()
        );
    }

    #[test]
    fn test_find_references_function() {
        let source = r#"
Функция МояФункция()
    Возврат 1;
КонецФункции

Процедура Тест()
    Результат = МояФункция();
    Другой = МояФункция();
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let def_offset = source.find("МояФункция").unwrap();
        let offset = TextSize::from(def_offset as u32);

        let references = find_references(&db, file_id, offset);

        assert!(
            references.len() >= 3,
            "Expected at least 3 references, found {}",
            references.len()
        );
    }

    #[test]
    fn test_find_parameter_references() {
        let source = r#"
Процедура Тест(МойПараметр)
    Если МойПараметр > 0 Тогда
        Результат = МойПараметр + 1;
    КонецЕсли;
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let param_offset = source.find("МойПараметр").unwrap();
        let offset = TextSize::from(param_offset as u32);

        let references = find_references(&db, file_id, offset);

        println!("Found {} parameter references", references.len());

        assert_eq!(
            references.len(),
            3,
            "Expected exactly 3 references (declaration + 2 usages), found {}",
            references.len()
        );
    }

    #[test]
    fn test_find_implicit_local_references_do_not_cross_methods() {
        let source = r#"
Процедура Первый()
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
КонецПроцедуры

Процедура Второй()
    НаборЗаписей = 2;
    Сообщить(НаборЗаписей);
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let offset = TextSize::from(source.find("НаборЗаписей").unwrap() as u32);

        let references = find_references(&db, file_id, offset);

        assert_eq!(references.len(), 2, "implicit locals must be scoped to their body");
        for location in references {
            let start: u32 = location.range.start().into();
            assert!(
                start < source.find("Процедура Второй").unwrap() as u32,
                "reference from the second method leaked into the first method result"
            );
        }
    }

    /// Bodies used by the implicit-local identity gates. A variable written twice
    /// is one variable; the pair differs only in whether the second write changes
    /// the inferred type, and `DECLARED` is the same body with the declaration a
    /// BSL author may or may not write.
    const REASSIGNED_SAME_TYPE: &str = r#"
Процедура Тест()
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = 2;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;

    const REASSIGNED_OTHER_TYPE: &str = r#"
Процедура Тест()
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = "строка";
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;

    const DECLARED_AND_REASSIGNED: &str = r#"
Процедура Тест()
    Перем НаборЗаписей;
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = "строка";
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;

    fn occurrences(source: &str, name: &str) -> Vec<usize> {
        let mut found = Vec::new();
        let mut from = 0usize;
        while let Some(at) = source[from..].find(name) {
            found.push(from + at);
            from += at + name.len();
        }
        found
    }

    fn reference_starts(db: &RootDatabaseImpl, file_id: FileId, offset: usize) -> Vec<u32> {
        let mut starts: Vec<u32> = find_references(db, file_id, TextSize::from(offset as u32))
            .iter()
            .map(|location| location.range.start().into())
            .collect();
        starts.sort_unstable();
        starts
    }

    /// A variable is one variable however many times it is written: every occurrence
    /// answers with the same set, and that set is every occurrence.
    ///
    /// Fails on an identity keyed by the chosen assignment: each anchor then answers
    /// with its own half and calls it complete.
    #[test]
    fn implicit_local_references_span_every_assignment() {
        let (db, file_id) = create_db_with_file(REASSIGNED_SAME_TYPE);
        let sites = occurrences(REASSIGNED_SAME_TYPE, "НаборЗаписей");
        let expected: Vec<u32> = sites.iter().map(|&at| at as u32).collect();
        assert_eq!(expected.len(), 4, "the input must carry four occurrences");

        for &at in &sites {
            assert_eq!(
                reference_starts(&db, file_id, at),
                expected,
                "the anchor at {at} answered with a subset"
            );
        }
    }

    /// The type of a write is a property of a program point, not of the variable's
    /// identity: reassigning to another type does not make a second variable.
    ///
    /// Paired with the same-type body above, which is the input a type-shaped split
    /// would still pass.
    #[test]
    fn implicit_local_references_ignore_the_type_of_the_reassignment() {
        let (db, file_id) = create_db_with_file(REASSIGNED_OTHER_TYPE);
        let sites = occurrences(REASSIGNED_OTHER_TYPE, "НаборЗаписей");
        let expected: Vec<u32> = sites.iter().map(|&at| at as u32).collect();
        assert_eq!(expected.len(), 4, "the input must carry four occurrences");

        for &at in &sites {
            assert_eq!(
                reference_starts(&db, file_id, at),
                expected,
                "the anchor at {at} answered with one type's occurrences"
            );
        }
    }

    /// Writing `Перем` changes where the variable is declared, not which occurrences
    /// belong to it. The declared body is the control: it answers correctly today,
    /// so a run where only the implicit body fails is the split and nothing else.
    #[test]
    fn an_implicit_local_answers_like_a_declared_one() {
        let (db_implicit, implicit_file) = create_db_with_file(REASSIGNED_OTHER_TYPE);
        let (db_declared, declared_file) = create_db_with_file(DECLARED_AND_REASSIGNED);

        let implicit_sites = occurrences(REASSIGNED_OTHER_TYPE, "НаборЗаписей");
        let declared_sites = occurrences(DECLARED_AND_REASSIGNED, "НаборЗаписей");
        assert_eq!(implicit_sites.len() + 1, declared_sites.len(), "one extra site: `Перем`");

        for &at in &implicit_sites {
            assert_eq!(
                reference_starts(&db_implicit, implicit_file, at).len(),
                implicit_sites.len(),
                "implicit body: the anchor at {at} lost occurrences"
            );
        }
        for &at in &declared_sites {
            assert_eq!(
                reference_starts(&db_declared, declared_file, at).len(),
                declared_sites.len(),
                "control: the declared body must already answer in full"
            );
        }
    }

    /// Identity itself, not merely its effect on the walk: the key an occurrence
    /// resolves to may not depend on which occurrence was asked.
    #[test]
    fn implicit_local_identity_does_not_depend_on_the_anchor() {
        let (db, file_id) = create_db_with_file(REASSIGNED_OTHER_TYPE);
        let parse = db.parse(file_id);
        let root = parse.syntax_node();
        let sema = Semantics::new(&db);

        let keys: Vec<SemanticSymbolKey> = occurrences(REASSIGNED_OTHER_TYPE, "НаборЗаписей")
            .into_iter()
            .map(|at| {
                let token = root
                    .token_at_offset(TextSize::from(at as u32))
                    .right_biased()
                    .expect("a name token stands at every occurrence");
                sema.symbol_for_token(file_id, &token)
                    .expect("an implicit local resolves to a symbol")
                    .key
            })
            .collect();

        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "occurrences of one variable resolved to different keys: {keys:?}"
        );
    }

    /// Spelling is not identity either: BSL folds case, and so must the key.
    #[test]
    fn implicit_local_references_fold_the_spelling() {
        let source = r#"
Процедура Тест()
    НаборЗаписей = 1;
    Сообщить(НАБОРЗАПИСЕЙ);
    наборзаписей = 2;
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let first = source.find("НаборЗаписей").unwrap();

        assert_eq!(reference_starts(&db, file_id, first).len(), 3, "spellings of one variable");
    }

    /// The identity that collapses is the implicit one. A name a body declares
    /// syntactically keeps resolving through its binding — inference records writes
    /// to some such bindings in the same table as implicit locals, and only the
    /// binding lookup tells the two apart.
    #[test]
    fn a_declared_name_is_not_an_implicit_local() {
        let (db, file_id) = create_db_with_file(DECLARED_AND_REASSIGNED);
        let parse = db.parse(file_id);
        let root = parse.syntax_node();
        let sema = Semantics::new(&db);
        let at = DECLARED_AND_REASSIGNED.find("НаборЗаписей").unwrap();
        let token = root.token_at_offset(TextSize::from(at as u32)).right_biased().unwrap();

        let symbol = sema.symbol_for_token(file_id, &token).expect("a declared local resolves");

        assert!(
            matches!(symbol.key, SemanticSymbolKey::BodyLocal { .. }),
            "a declared local must stay a body local: {:?}",
            symbol.key
        );
    }

    #[test]
    fn test_find_local_variable_references() {
        let source = r#"
Процедура Тест()
    Перем МояПеременная;

    МояПеременная = 10;
    Результат = МояПеременная * 2;
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let var_offset = source.find("МояПеременная").unwrap();
        let offset = TextSize::from(var_offset as u32);

        let references = find_references(&db, file_id, offset);

        println!("Found {} local variable references", references.len());

        assert_eq!(
            references.len(),
            3,
            "Expected exactly 3 references (declaration + 2 usages), found {}",
            references.len()
        );
    }

    #[test]
    fn test_find_references_no_false_positives() {
        let source = r#"
Перем Значение;

Процедура Тест1()
    Перем Значение;  // Local variable with same name

    Значение = 1;
КонецПроцедуры

Процедура Тест2()
    Значение = 2;  // Module variable
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let module_var_offset = source.find("Значение").unwrap();
        let offset = TextSize::from(module_var_offset as u32);

        let references = find_references(&db, file_id, offset);

        println!("Found {} module variable references", references.len());
        for (i, loc) in references.iter().enumerate() {
            let start: u32 = loc.range.start().into();
            let end: u32 = loc.range.end().into();
            let text = &source[start as usize..end as usize];
            println!("  Ref {}: offset={}, text={:?}", i, start, text);
        }

        assert!(
            references.len() >= 2,
            "Expected at least 2 references (module var), found {}",
            references.len()
        );
    }

    #[test]
    fn test_find_module_method_multiple_files() {
        let mut db = RootDatabaseImpl::default();

        let file1_id = FileId(0);
        let file1_source = r#"
Процедура МояПроцедура() Экспорт
    // Определение
КонецПроцедуры

Функция Тест1()
    МояПроцедура();  // Вызов в том же модуле
КонецФункции
        "#;

        let file2_id = FileId(1);
        let file2_source = r#"
Процедура МояПроцедура()
    // Другая процедура с тем же именем
КонецПроцедуры

Функция Тест2()
    МояПроцедура();  // Вызов локального метода
КонецПроцедуры
        "#;

        let mut file_set = FileSet::new();
        file_set.insert(file1_id, VfsPath::new("/module1.bsl"));
        file_set.insert(file2_id, VfsPath::new("/module2.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file1_id, SourceRootId(0));
        db.set_file_source_root(file2_id, SourceRootId(0));

        db.set_file_text(file1_id, file1_source);
        db.set_file_text(file2_id, file2_source);

        let def_offset = file1_source.find("МояПроцедура").unwrap();
        let offset = TextSize::from(def_offset as u32);

        let references = find_references(&db, file1_id, offset);

        println!("Found {} references for file1 method", references.len());
        for (i, loc) in references.iter().enumerate() {
            println!("  Ref {}: file={:?}, range={:?}", i, loc.file_id, loc.range);
        }

        assert_eq!(references.len(), 2, "Expected 2 references in file1 only");

        for loc in &references {
            assert_eq!(loc.file_id, file1_id, "All references should be in file1");
        }
    }

    #[test]
    fn test_find_module_variable_multiple_files() {
        let mut db = RootDatabaseImpl::default();

        let file1_id = FileId(0);
        let file1_source = r#"
Перем МояПеременная Экспорт;

Процедура Тест1()
    МояПеременная = 10;
    Сообщить(МояПеременная);
КонецПроцедуры
        "#;

        let file2_id = FileId(1);
        let file2_source = r#"
Перем МояПеременная;

Процедура Тест2()
    МояПеременная = 20;
КонецПроцедуры
        "#;

        let mut file_set = FileSet::new();
        file_set.insert(file1_id, VfsPath::new("/module1.bsl"));
        file_set.insert(file2_id, VfsPath::new("/module2.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file1_id, SourceRootId(0));
        db.set_file_source_root(file2_id, SourceRootId(0));

        db.set_file_text(file1_id, file1_source);
        db.set_file_text(file2_id, file2_source);

        let def_offset = file1_source.find("МояПеременная").unwrap();
        let offset = TextSize::from(def_offset as u32);

        let references = find_references(&db, file1_id, offset);

        println!("Found {} variable references for file1", references.len());

        assert_eq!(references.len(), 3, "Expected 3 references in file1 only");

        for loc in &references {
            assert_eq!(loc.file_id, file1_id, "All references should be in file1");
        }
    }

    #[test]
    fn test_local_symbols_only_in_current_file() {
        let mut db = RootDatabaseImpl::default();

        let file1_id = FileId(0);
        let file1_source = r#"
Процедура Метод1()
    Перем ЛокальнаяПеременная;
    ЛокальнаяПеременная = 1;
    Сообщить(ЛокальнаяПеременная);
КонецПроцедуры
        "#;

        let file2_id = FileId(1);
        let file2_source = r#"
Процедура Метод2()
    Перем ЛокальнаяПеременная;  // Same name, different scope
    ЛокальнаяПеременная = 2;
КонецПроцедуры
        "#;

        let mut file_set = FileSet::new();
        file_set.insert(file1_id, VfsPath::new("/local1.bsl"));
        file_set.insert(file2_id, VfsPath::new("/local2.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file1_id, SourceRootId(0));
        db.set_file_source_root(file2_id, SourceRootId(0));

        db.set_file_text(file1_id, file1_source);
        db.set_file_text(file2_id, file2_source);

        let def_offset = file1_source.find("ЛокальнаяПеременная").unwrap();
        let offset = TextSize::from(def_offset as u32);

        let references = find_references(&db, file1_id, offset);

        println!("Found {} local variable references", references.len());

        assert_eq!(references.len(), 3, "Expected exactly 3 references in same file");

        for loc in &references {
            assert_eq!(
                loc.file_id, file1_id,
                "Local variable references should not cross file boundaries"
            );
        }
    }

    #[test]
    fn export_method_uses_name_usage_index_to_narrow_scope() {
        let mut db = RootDatabaseImpl::default();
        let file_a = FileId(0);
        let file_b = FileId(1);
        let file_c = FileId(2);

        let file_a_src = r#"
Процедура МояПроцедура() Экспорт
    МояПроцедура();
КонецПроцедуры
"#;
        let file_b_src = r#"
Процедура МояПроцедура()
    МояПроцедура();
КонецПроцедуры
"#;
        let file_c_src = r#"
Процедура НеПохожийМетод()
КонецПроцедуры
"#;

        let mut file_set = FileSet::new();
        file_set.insert(file_a, VfsPath::new("/a.bsl"));
        file_set.insert(file_b, VfsPath::new("/b.bsl"));
        file_set.insert(file_c, VfsPath::new("/c.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_a, SourceRootId(0));
        db.set_file_source_root(file_b, SourceRootId(0));
        db.set_file_source_root(file_c, SourceRootId(0));
        db.set_file_text(file_a, file_a_src);
        db.set_file_text(file_b, file_b_src);
        db.set_file_text(file_c, file_c_src);

        let def_offset = file_a_src.find("МояПроцедура").unwrap();
        let references = find_references(&db, file_a, TextSize::from(def_offset as u32));

        assert_eq!(references.len(), 2, "expected definition + 1 call in file A");
        for loc in &references {
            assert_eq!(loc.file_id, file_a);
        }
    }

    #[test]
    fn non_export_method_stays_file_local() {
        let mut db = RootDatabaseImpl::default();

        let file1_id = FileId(0);
        let file1_source = r#"
Процедура Помощник()
КонецПроцедуры

Процедура Тест1()
    Помощник();
КонецПроцедуры
"#;
        let file2_id = FileId(1);
        let file2_source = r#"
Процедура Помощник()
КонецПроцедуры

Процедура Тест2()
    Помощник();
КонецПроцедуры
"#;

        let mut file_set = FileSet::new();
        file_set.insert(file1_id, VfsPath::new("/a.bsl"));
        file_set.insert(file2_id, VfsPath::new("/b.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file1_id, SourceRootId(0));
        db.set_file_source_root(file2_id, SourceRootId(0));
        db.set_file_text(file1_id, file1_source);
        db.set_file_text(file2_id, file2_source);

        let def_offset = file1_source.find("Помощник").unwrap();
        let references = find_references(&db, file1_id, TextSize::from(def_offset as u32));

        assert_eq!(references.len(), 2, "definition + 1 call in file 1");
        for loc in &references {
            assert_eq!(
                loc.file_id, file1_id,
                "non-export procedure references must not cross file boundaries"
            );
        }
    }
}
