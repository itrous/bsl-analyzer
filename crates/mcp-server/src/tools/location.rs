//! The location contract: one shape for "where" and one shape for "how fresh and how
//! complete", shared by every agent-facing tool.
//!
//! Before this module each tool spelled a place its own way — 1-based lines here,
//! 0-based ranges there, a bare line number in a third, nothing at all in a fourth — and
//! a consumer had to know which tool it was talking to before it could read an answer.
//! The rule that keeps that from coming back: this module is the ONLY place that
//! serializes a location or a freshness envelope. A tool assembles the typed value and
//! hands it over; it never writes the keys itself.

use std::path::Path;

use bsl_search::WorkspaceRoots;
use line_index::LineColRange;
use serde_json::{json, Value};

/// Version of the location block itself, independent of any tool's response version:
/// the block travels across tools, so it cannot be versioned by one of them.
pub const LOCATION_SCHEMA_VERSION: &str = "1";

/// The position unit every published range is measured in. Declared in the payload
/// rather than assumed, because the same build serves both LSP (UTF-16) and older
/// fields counting bytes or code points.
pub const POSITION_ENCODING: &str = "utf-16";

/// A span inside a file: 0-based, end-exclusive, characters in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl From<LineColRange> for PositionRange {
    fn from(r: LineColRange) -> Self {
        Self {
            start_line: r.start_line,
            start_character: r.start_character,
            end_line: r.end_line,
            end_character: r.end_character,
        }
    }
}

impl PositionRange {
    /// Public because an answer about ONE file publishes the pair once at its root and gives
    /// its nodes bare ranges. Those ranges are still the contract's, and this module is still
    /// the only place that writes their keys.
    pub fn to_value(self) -> Value {
        json!({
            "start_line": self.start_line,
            "start_character": self.start_character,
            "end_line": self.end_line,
            "end_character": self.end_character,
        })
    }
}

/// The module a location belongs to, when the producer already holds both halves.
/// Never reconstructed by parsing a display string: a guess in a contract field is
/// worse than an absent field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRef {
    pub kind: String,
    pub name: String,
}

/// A place in the workspace, addressable by the pair `(root_id, path)`.
///
/// `range` is the symbol's name (or, for a diagnostic, the finding's own span);
/// `enclosing_range` is the whole node. Both are optional: an answer that only knows
/// the file — a hit from a shared baseline, a card without a source — says so by
/// omitting them rather than inventing zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub root_id: String,
    pub path: String,
    pub range: Option<PositionRange>,
    pub enclosing_range: Option<PositionRange>,
    pub module: Option<ModuleRef>,
}

/// Declare the vocabulary of "why there is no place" ONCE: the variants, the wire codes,
/// the published glosses and the list of them all come out of a single table.
///
/// A hand-written `ALL` beside the enum would be the one part a new variant does not have
/// to join — `code` and `describe` are exhaustive matches and the compiler demands their
/// branches, but an array of fixed length demands nothing. The gates that iterate that list
/// would then keep passing while `graph schema` published a vocabulary short by exactly the
/// code it had started serving, which is the failure the list exists to prevent.
macro_rules! location_unavailable {
    ($( $(#[$doc:meta])* $variant:ident => $code:literal, $gloss:literal; )+) => {
        /// Why a place could not be named. Always emitted in place of a location, so a
        /// consumer never has to tell "absent because unknown" from "absent because none".
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum LocationUnavailable {
            $( $(#[$doc])* $variant, )+
        }

        impl LocationUnavailable {
            /// The whole vocabulary. Every place that PUBLISHES the list — a tool's
            /// `schema` action, the contract document — reads it from here.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The wire code: what a consumer matches on.
            pub fn code(self) -> &'static str {
                match self { $( Self::$variant => $code, )+ }
            }

            /// A one-line gloss, so a tool's `schema` action publishes the vocabulary
            /// instead of paraphrasing it in prose that drifts from the enum.
            pub fn describe(self) -> &'static str {
                match self { $( Self::$variant => $gloss, )+ }
            }
        }
    };
}

location_unavailable! {
    /// The path lies outside every registered root, so no pair addresses it.
    PathOutsideRegisteredRoots => "path_outside_registered_roots",
        "the file lies outside every registered root";

    /// The producer holds an absolute path where the pair was expected — a search hit whose
    /// `file_path` is not root-relative. Its file is named, but not as `(root_id, path)`.
    AbsolutePathWithoutPair => "absolute_path_without_pair",
        "the producer holds an absolute path rather than a (root_id, path) pair";

    /// Only a relative path is known, so the root table cannot place it. The mirror image of
    /// [`LocationUnavailable::AbsolutePathWithoutPair`], and a separate code for exactly that
    /// reason: one name for both facts would leave a consumer unable to tell which happened.
    RelativePathWithoutRoot => "relative_path_without_root",
        "only a relative path is known, so no root table can place it";

    /// The entity has no source file by construction (a metadata object, a form item).
    NoSourceLocation => "no_source_location",
        "metadata objects, forms and attributes have no file";

    /// The root table is not available in this answer — a stale graph cache published as
    /// ready while the catch-up reload is still running.
    RootsUnavailable => "roots_unavailable",
        "answered from a cache published before the project's root table was known";

    /// The entity HAS a source file, but its path could not be named — a VFS entry that is
    /// not valid UTF-8, or one missing from the source root's file set. Distinct from
    /// `RootsUnavailable`: the table was there, the path was not.
    SourcePathUnavailable => "source_path_unavailable",
        "the entity has a file, but its path could not be named";

    /// A graph edge with no call in code behind it: it was derived from metadata rather than
    /// read out of a body. There is no span to publish and there never will be.
    NoCallSite => "no_call_site",
        "the edge is derived from metadata, not from a call written in code";

    /// A graph edge that DOES stand for something written in code, whose span the build does
    /// not keep. Distinct from `NoCallSite` for the reason the vocabulary exists: one code for
    /// both would teach a consumer to stop expecting a span that a later build can supply.
    CallSiteNotRecorded => "call_site_not_recorded",
        "the call site exists in code, but the graph does not record its span";

    /// The file moved under a span the build recorded. Publishing it would hand back a
    /// plausible and wrong range — the one failure a place is least able to survive, since a
    /// consumer cuts text with it.
    SourceDrifted => "source_drifted",
        "the file changed after the graph was built, so its recorded spans cannot be trusted";
}

impl LocationUnavailable {
    /// The whole vocabulary on one line, `code (gloss)` joined — what a tool's `schema`
    /// action serves.
    pub fn vocabulary() -> String {
        Self::ALL
            .iter()
            .map(|reason| format!("{} ({})", reason.code(), reason.describe()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Location {
    /// A location from a pair the caller already holds.
    ///
    /// The root is NOT validated against any table: a hit from a shared baseline
    /// carries a root registered in the workspace that produced it, and replacing it
    /// with the configuration's empty id — or dropping the location — would lose an
    /// address its source knows and we do not.
    pub fn from_key(root_id: &str, rel_path: &str) -> Self {
        Self {
            root_id: root_id.to_string(),
            path: normalize_separators(rel_path),
            range: None,
            enclosing_range: None,
            module: None,
        }
    }

    /// A location for an absolute path, deriving the root from the root table.
    ///
    /// Only for callers that hold nothing but a path. A caller holding a pair must use
    /// [`Location::from_key`]: a relative path handed to the table is resolved against
    /// the CONFIGURATION root, so an extension file and a configuration file sharing a
    /// relative path would both come back as the configuration's.
    pub fn from_path(roots: &WorkspaceRoots, abs: &Path) -> Result<Self, LocationUnavailable> {
        if !abs.is_absolute() {
            return Err(LocationUnavailable::RelativePathWithoutRoot);
        }
        let key = roots.key_of_path(abs).ok_or(LocationUnavailable::PathOutsideRegisteredRoots)?;
        Ok(Self::from_key(&key.root_id, &key.path))
    }

    pub fn with_range(mut self, range: Option<PositionRange>) -> Self {
        self.range = range;
        self
    }

    pub fn with_enclosing_range(mut self, range: Option<PositionRange>) -> Self {
        self.enclosing_range = range;
        self
    }

    pub fn with_module(mut self, module: Option<ModuleRef>) -> Self {
        self.module = module;
        self
    }

    pub fn to_value(&self) -> Value {
        let mut body = json!({
            "root_id": self.root_id,
            "path": self.path,
            "position_encoding": POSITION_ENCODING,
            "schema_version": LOCATION_SCHEMA_VERSION,
        });
        let map = body.as_object_mut().expect("object literal");
        if let Some(range) = self.range {
            map.insert("range".into(), range.to_value());
        }
        if let Some(range) = self.enclosing_range {
            map.insert("enclosing_range".into(), range.to_value());
        }
        if let Some(module) = &self.module {
            map.insert("module".into(), json!({ "kind": module.kind, "name": module.name }));
        }
        body
    }
}

/// Windows spells a stored relative path with `\`, and the contract publishes `/`. On
/// UNIX a backslash is an ordinary character in a file name, so rewriting it there would
/// mint an address that looks right and points elsewhere — the exact failure this contract
/// exists to prevent.
fn normalize_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

/// Declared by one table for the reason spelled out over [`LocationUnavailable`]: a list
/// written by hand beside the enum is the one place a new variant is not forced to join.
macro_rules! reason_code {
    ($( $(#[$doc:meta])* $variant:ident => $code:literal; )+) => {
        /// Why an answer is not the whole answer. A closed vocabulary: a new reason means a
        /// new version of the contract, not a new free-form string.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ReasonCode {
            $( $(#[$doc])* $variant, )+
        }

        impl ReasonCode {
            /// The whole vocabulary, for every place that PUBLISHES the list.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $code, )+ }
            }
        }
    };
}

reason_code! {
    /// The output budget cut the answer short.
    OutputBudget => "output_budget";
    /// A count limit (`max_findings`, `max_nodes`, a candidate cap) cut it short.
    ResultCap => "result_cap";
    /// An index needed for part of the answer is still being built.
    IndexBuilding => "index_building";
    /// Some files could not be read.
    UnreadableFiles => "unreadable_files";
    /// Part of the request lies outside the configured analysis scope.
    OutOfAnalysisScope => "out_of_analysis_scope";
    /// A modality or subsystem the answer would normally use was unavailable.
    ModalityDegraded => "modality_degraded";
    /// The file belongs to an external data processor or report analyzed without
    /// the configuration it was written for: its calls into that configuration
    /// are reported as unresolved.
    OwningConfigurationMissing => "owning_configuration_missing";
}

/// The accepted values of one field, `a | b | c`, for a tool's `schema` action to publish.
///
/// Generated rather than retyped: a `schema` action exists to tell a consumer what it may
/// match on, and a hand-written list there is a promise nothing keeps.
fn joined(values: impl Iterator<Item = &'static str>) -> String {
    values.collect::<Vec<_>>().join(" | ")
}

impl ReasonCode {
    pub fn vocabulary() -> String {
        joined(Self::ALL.iter().map(|code| code.as_str()))
    }
}

impl FreshnessSource {
    pub fn vocabulary() -> String {
        joined(Self::ALL.iter().map(|source| source.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub code: ReasonCode,
    pub detail: String,
}

/// Whether the answer is whole, and if not, why.
///
/// Built where the fact is known — the return of a trimming helper, a cap flag, an
/// unread counter — and never reconstructed by inspecting the rendered JSON: a reason
/// recovered from output is a guess about our own behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completeness {
    reasons: Vec<Reason>,
}

impl Completeness {
    pub fn complete() -> Self {
        Self::default()
    }

    pub fn partial(code: ReasonCode, detail: impl Into<String>) -> Self {
        Self { reasons: vec![Reason { code, detail: detail.into() }] }
    }

    /// Record a reason when `present` holds. Takes the flag rather than being called
    /// under an `if` so the call site reads as "this fact, that code" and cannot
    /// silently skip a reason it computed.
    pub fn when(mut self, present: bool, code: ReasonCode, detail: impl Into<String>) -> Self {
        if present {
            self.reasons.push(Reason { code, detail: detail.into() });
        }
        self
    }

    pub fn is_complete(&self) -> bool {
        self.reasons.is_empty()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "status": if self.is_complete() { "complete" } else { "partial" },
            "reasons": self
                .reasons
                .iter()
                .map(|r| json!({ "code": r.code.as_str(), "detail": r.detail }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Declare the vocabulary of "who answered" ONCE, for the reason spelled out over
/// [`LocationUnavailable`]: a hand-written `ALL` is the one part a new variant is not forced
/// to join, and the gate that checks the published vocabulary would then keep passing while
/// serving a source the contract document never names.
macro_rules! freshness_source {
    ($( $(#[$doc:meta])* $variant:ident => $code:literal; )+) => {
        /// Which subsystem produced an answer. Named because the freshness fields below mean
        /// different things — and are known to a different extent — per source.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum FreshnessSource {
            $( $(#[$doc])* $variant, )+
        }

        impl FreshnessSource {
            /// The whole vocabulary, for every place that PUBLISHES the list.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $code, )+ }
            }
        }
    };
}

freshness_source! {
    /// The resident analysis database: its own revision, topology and drift verdict.
    Resident => "resident";
    /// The call graph's store.
    Graph => "graph";
    /// The search index, which has no revision of its own.
    SearchIndex => "search-index";
    /// One file parsed for this call and nothing else — no index, no resident, no graph.
    /// It has no identity to report: there is no revision such a parse belongs to and no
    /// topology it was taken under, so both come back `null` rather than borrowed.
    FileParse => "file-parse";
    /// Several artefacts at once, each with a revision of its own — the name lookup asks
    /// the module tables, the metadata listing, the platform and the graph in one call.
    /// No single revision describes such an answer, so all three fields come back `null`
    /// and the per-source truth travels in the answer's own `providers` array. Borrowing
    /// one participant's revision would claim freshness for the four that did not lend it.
    NameDictionary => "name-dictionary";
}

/// Who is watching the workspace for the source that answered, and how.
///
/// `stale` says whether the answer is known to be behind; this says what that verdict rests
/// on. A source nobody watches cannot know it is behind, so its answer is never called fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftWatch {
    /// The source's watcher has not finished its first look at the workspace.
    Starting,
    /// Every change the watch delivers reaches the source.
    Watching,
    /// Nothing could be watched, or part of the workspace cannot: changes are found by a
    /// periodic walk and a rolling content check, and can take up to one of its cycles.
    Polling,
    /// The source's watcher has stopped: changes since then go unnoticed.
    Unobserved,
}

impl DriftWatch {
    pub const ALL: &'static [Self] =
        &[Self::Starting, Self::Watching, Self::Polling, Self::Unobserved];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Watching => "watching",
            Self::Polling => "polling",
            Self::Unobserved => "unobserved",
        }
    }

    pub fn vocabulary() -> String {
        joined(Self::ALL.iter().map(|watch| watch.as_str()))
    }
}

/// The envelope every tool of the contract carries: what answered, at which revision
/// and topology, whether it had drifted, and whether the answer is whole.
///
/// `revision`, `topology_fingerprint` and `stale` are `Option` because a source may
/// genuinely have no such identity — the search index has no revision of its own, and
/// stamping the resident's or the graph's onto its answers is exactly the borrowed
/// identity this contract exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    pub source: FreshnessSource,
    pub revision: Option<u64>,
    pub topology_fingerprint: Option<u64>,
    pub stale: Option<bool>,
    pub completeness: Completeness,
    /// Written only by the sources that follow the workspace themselves — the graph and
    /// the workspace search index. Every other envelope stays byte-identical.
    pub drift_watch: Option<DriftWatch>,
}

impl Freshness {
    pub fn new(source: FreshnessSource, completeness: Completeness) -> Self {
        Self {
            source,
            revision: None,
            topology_fingerprint: None,
            stale: None,
            completeness,
            drift_watch: None,
        }
    }

    pub fn with_drift_watch(mut self, watch: DriftWatch) -> Self {
        self.drift_watch = Some(watch);
        self
    }

    pub fn with_revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }

    pub fn with_topology(mut self, topology: u64) -> Self {
        self.topology_fingerprint = Some(topology);
        self
    }

    pub fn with_stale(mut self, stale: bool) -> Self {
        self.stale = Some(stale);
        self
    }

    pub fn to_value(&self) -> Value {
        let mut value = json!({
            "source": self.source.as_str(),
            "revision": self.revision,
            // Hex rather than a JSON number: the value is a u64 and a JS consumer
            // silently rounds anything above 2^53.
            "topology_fingerprint": self.topology_fingerprint.map(|fp| format!("{fp:016x}")),
            "stale": self.stale,
            "completeness": self.completeness.to_value(),
        });
        if let Some(watch) = self.drift_watch {
            value["drift_watch"] = json!(watch.as_str());
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A workspace with the configuration and one extension root beside it — the
    /// shape where one relative path exists in two roots at once.
    fn roots() -> WorkspaceRoots {
        let (roots, rejected) = WorkspaceRoots::build(
            Path::new("/ws"),
            Path::new("/ws/src/cf"),
            &[PathBuf::from("/ws/src/cfe/ext-a")],
        );
        assert!(rejected.is_empty(), "both roots must register for this stand to mean anything");
        roots
    }

    fn extension_root_id(roots: &WorkspaceRoots) -> String {
        roots.ids().find(|id| !id.is_empty()).expect("an extension root is registered").to_owned()
    }

    /// `ide` spells two of these codes itself, because its in-memory projection must emit
    /// the SAME string as the SQLite one for the byte-identity gates to keep comparing like
    /// with like — and it sits below this crate, so it cannot import the enum. That makes
    /// the two spellings a pair that can drift silently; this is the test that they cannot.
    /// Every key [`Location::to_value`] writes is a key [`WireLocation`] declares, and every
    /// key it declares as required is one the writer always writes.
    ///
    /// The published schema is a SECOND spelling of the block, and a second spelling drifts.
    /// This holds them together in both directions: `deny_unknown_fields` fails on a key the
    /// writer gained and the type did not, a missing required field fails on one the writer
    /// dropped, and the explicit key list below fails on an optional key that quietly left —
    /// the one case neither serde rule can see.
    #[test]
    fn wire_location_matches_what_the_contract_serializes() {
        let full = Location {
            root_id: "ext-a".to_string(),
            path: "CommonModules/Общий/Ext/Module.bsl".to_string(),
            range: Some(PositionRange {
                start_line: 4,
                start_character: 8,
                end_line: 4,
                end_character: 15,
            }),
            enclosing_range: Some(PositionRange {
                start_line: 3,
                start_character: 0,
                end_line: 6,
                end_character: 12,
            }),
            module: Some(ModuleRef {
                kind: "ОбщийМодуль".to_string(), name: "Общий".to_string()
            }),
        };
        let written = full.to_value();
        let mut keys: Vec<&str> = written
            .as_object()
            .expect("a location is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "enclosing_range",
                "module",
                "path",
                "position_encoding",
                "range",
                "root_id",
                "schema_version",
            ],
            "the block gained or lost a key: {written}"
        );
        let parsed: WireLocation =
            serde_json::from_value(written.clone()).unwrap_or_else(|error| {
                panic!(
                    "the published type must accept what the contract writes: {error}: {written}"
                )
            });
        assert_eq!(parsed.position_encoding, POSITION_ENCODING);
        assert_eq!(parsed.schema_version, LOCATION_SCHEMA_VERSION);

        // A place that knows only its file: the optional halves are absent, not zeroed, and
        // the published type has to accept that shape too — it is what a baseline hit looks
        // like.
        let bare = Location {
            root_id: String::new(),
            path: "Configuration.xml".to_string(),
            range: None,
            enclosing_range: None,
            module: None,
        };
        let written = bare.to_value();
        let parsed: WireLocation =
            serde_json::from_value(written.clone()).unwrap_or_else(|error| {
                panic!("a file-only place is a location too: {error}: {written}")
            });
        assert!(parsed.range.is_none() && parsed.enclosing_range.is_none());
    }

    #[test]
    fn the_reasons_ide_spells_itself_match_this_vocabulary() {
        assert_eq!(LocationUnavailable::NoSourceLocation.code(), ide::NO_SOURCE_LOCATION);
        assert_eq!(LocationUnavailable::RootsUnavailable.code(), ide::ROOTS_UNAVAILABLE);
    }

    #[test]
    fn a_location_names_its_encoding_and_version() {
        let value = Location::from_key("", "CommonModules/Сервер/Ext/Module.bsl")
            .with_range(Some(PositionRange {
                start_line: 119,
                start_character: 10,
                end_line: 119,
                end_character: 22,
            }))
            .to_value();

        assert_eq!(value["position_encoding"], POSITION_ENCODING);
        assert_eq!(value["schema_version"], LOCATION_SCHEMA_VERSION);
        assert_eq!(value["root_id"], "");
        assert_eq!(value["range"]["start_line"], 119);
        assert_eq!(value["range"]["end_character"], 22);
        // A location that knows only the file omits the ranges instead of zeroing them.
        assert!(value.get("enclosing_range").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_backslash_in_a_unix_name_is_a_name_and_not_a_separator() {
        // A path component may legally contain a backslash on UNIX; rewriting it would
        // address a different file, and `resolve` would hand back the wrong one.
        let location = Location::from_key("", "CommonModules/A\\B/Ext/Module.bsl");
        assert_eq!(location.path, "CommonModules/A\\B/Ext/Module.bsl");
    }

    #[test]
    fn a_pair_is_taken_as_given_and_not_re_derived() {
        // The same relative path in two roots: the pair-taking constructor keeps them
        // apart, which is the whole point of the pair being the key.
        let config = Location::from_key("", "CommonModules/Общий/Ext/Module.bsl");
        let extension = Location::from_key("ext-a", "CommonModules/Общий/Ext/Module.bsl");

        assert_eq!(config.path, extension.path);
        assert_ne!(config.root_id, extension.root_id);
        assert_eq!(extension.to_value()["root_id"], "ext-a");
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "the fixture spells POSIX absolute paths, which are not absolute on Windows"
    )]
    fn a_path_derives_the_root_it_lies_in() {
        let roots = roots();
        let ext = extension_root_id(&roots);

        let in_extension = Location::from_path(
            &roots,
            Path::new("/ws/src/cfe/ext-a/CommonModules/М/Ext/Module.bsl"),
        )
        .expect("inside the extension root");
        assert_eq!(in_extension.root_id, ext);
        assert_eq!(in_extension.path, "CommonModules/М/Ext/Module.bsl");

        // Same relative path, other root: the pair keeps them apart.
        let in_configuration =
            Location::from_path(&roots, Path::new("/ws/src/cf/CommonModules/М/Ext/Module.bsl"))
                .expect("inside the configuration root");
        assert_eq!(in_configuration.root_id, "");
        assert_eq!(in_configuration.path, in_extension.path);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "the fixture spells POSIX absolute paths, which are not absolute on Windows"
    )]
    fn a_path_outside_every_root_names_the_reason() {
        let roots = roots();

        let err = Location::from_path(&roots, Path::new("/elsewhere/Module.bsl"))
            .expect_err("outside every root");

        assert_eq!(err.code(), "path_outside_registered_roots");
    }

    #[test]
    fn a_relative_path_is_refused_rather_than_attached_to_a_root() {
        let roots = roots();

        let err = Location::from_path(&roots, Path::new("CommonModules/М/Ext/Module.bsl"))
            .expect_err("a relative path addresses no root on its own");

        assert_eq!(err.code(), "relative_path_without_root");
    }

    /// The vocabulary is closed, and its only worth over a free-form string is that a
    /// consumer may match on it. That holds while every code names ONE fact and while every
    /// published list carries every code — the contract document among them, since it is
    /// what a consumer's own closed list gets written from.
    #[test]
    fn every_reason_is_a_distinct_code_and_the_contract_publishes_all_of_them() {
        let contract = include_str!("../../../../docs/mcp/LOCATION_CONTRACT.md");

        let mut seen = std::collections::BTreeSet::new();
        for reason in LocationUnavailable::ALL {
            assert!(seen.insert(reason.code()), "two facts share the code {}", reason.code());
            assert!(
                contract.contains(&format!("`{}`", reason.code())),
                "{} is served but the contract document does not list it",
                reason.code(),
            );
        }
    }

    /// The section of the contract document that describes the envelope.
    fn envelope_section() -> &'static str {
        let contract = include_str!("../../../../docs/mcp/LOCATION_CONTRACT.md");
        let after_heading = contract
            .split_once("\n## Конверт\n")
            .expect("the contract describes the envelope under its own heading")
            .1;
        match after_heading.split_once("\n## ") {
            Some((section, _)) => section,
            None => after_heading,
        }
    }

    /// The same rule as for the reasons above, with one addition: the envelope's two closed
    /// vocabularies have to be published WHERE the envelope is described, not merely somewhere
    /// in the document.
    ///
    /// The section bound is not decoration. `` `graph` `` appears four times in this file and
    /// none of them is in the envelope section — a gate reading the whole document would pass
    /// on a source the envelope never mentions, which is precisely the reader who needs it.
    #[test]
    fn both_envelope_vocabularies_are_named_where_the_envelope_is_described() {
        let envelope = envelope_section();

        let mut seen = std::collections::BTreeSet::new();
        for source in FreshnessSource::ALL {
            assert!(seen.insert(source.as_str()), "two sources share the name {}", source.as_str());
            assert!(
                envelope.contains(&format!("`{}`", source.as_str())),
                "{} answers requests but the envelope section does not name it",
                source.as_str(),
            );
        }

        for watch in DriftWatch::ALL {
            assert!(
                envelope.contains(&format!("`{}`", watch.as_str())),
                "{} can appear in an envelope but the envelope section does not name it",
                watch.as_str(),
            );
        }

        let mut seen = std::collections::BTreeSet::new();
        for reason in ReasonCode::ALL {
            assert!(seen.insert(reason.as_str()), "two reasons share the code {}", reason.as_str());
            assert!(
                envelope.contains(&format!("`{}`", reason.as_str())),
                "{} can appear in an envelope but the envelope section does not name it",
                reason.as_str(),
            );
        }
    }

    #[test]
    fn completeness_defaults_to_whole_and_names_each_reason() {
        assert_eq!(Completeness::complete().to_value()["status"], "complete");
        assert_eq!(Completeness::complete().to_value()["reasons"].as_array().unwrap().len(), 0);

        let partial = Completeness::complete()
            .when(true, ReasonCode::OutputBudget, "raise max_output_tokens")
            .when(false, ReasonCode::ResultCap, "not this time");
        let value = partial.to_value();

        assert_eq!(value["status"], "partial");
        assert_eq!(value["reasons"].as_array().unwrap().len(), 1);
        assert_eq!(value["reasons"][0]["code"], "output_budget");
    }

    #[test]
    fn a_source_without_identity_says_null_rather_than_borrowing_one() {
        let value =
            Freshness::new(FreshnessSource::SearchIndex, Completeness::complete()).to_value();

        assert_eq!(value["source"], "search-index");
        assert!(value["revision"].is_null());
        assert!(value["topology_fingerprint"].is_null());
        assert!(value["stale"].is_null());
    }

    /// The drift watch is written only where a source set it: an envelope of any other source
    /// is the same bytes it always was.
    #[test]
    fn the_drift_watch_is_written_only_where_it_is_set() {
        let plain = Freshness::new(FreshnessSource::Resident, Completeness::complete()).to_value();
        assert!(plain.get("drift_watch").is_none(), "{plain}");
        assert_eq!(plain.as_object().unwrap().len(), 5);

        let watched = Freshness::new(FreshnessSource::Graph, Completeness::complete())
            .with_drift_watch(DriftWatch::Polling)
            .to_value();
        assert_eq!(watched["drift_watch"], "polling");
    }

    #[test]
    fn a_topology_fingerprint_survives_as_hex() {
        let value = Freshness::new(FreshnessSource::Graph, Completeness::complete())
            .with_revision(42)
            .with_topology(0x0a1b_2c3d_4e5f_6071)
            .with_stale(false)
            .to_value();

        assert_eq!(value["revision"], 42);
        assert_eq!(value["topology_fingerprint"], "0a1b2c3d4e5f6071");
        assert_eq!(value["stale"], false);
    }
}

/// The published shape of a location block, for the tools that declare their output schema.
///
/// A second spelling of what [`Location::to_value`] writes, and the danger in that is
/// exactly the drift it exists to catch: a schema that agreed with the writer only on the
/// day it was written would validate a document example against a shape nothing serves.
/// [`wire_location_matches_what_the_contract_serializes`] holds the two together — the
/// writer's own output has to parse into this type with no key left over and none missing,
/// so a renamed or added key fails here rather than in a client.
#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireLocation {
    /// Which root the `path` is relative to. Empty string for the configuration root.
    pub root_id: String,
    /// Path relative to ITS OWN root, `/`-separated.
    pub path: String,
    /// The symbol's own span, absent when the answer only knows the file.
    pub range: Option<WirePositionRange>,
    /// The whole node the symbol belongs to, absent for the same reason.
    pub enclosing_range: Option<WirePositionRange>,
    /// The module this place belongs to, when the producer holds both halves.
    pub module: Option<WireModuleRef>,
    /// The unit `range` characters are counted in. Always `"utf-16"`.
    pub position_encoding: String,
    /// Version of the location block itself. Always `"1"`.
    pub schema_version: String,
}

/// A span inside a file: 0-based, end-exclusive, characters in UTF-16 code units.
#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WirePositionRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// The module a place belongs to.
#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireModuleRef {
    pub kind: String,
    pub name: String,
}
