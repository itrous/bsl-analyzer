/// Filters for a workspace sweep.
pub(crate) struct SweepOptions {
    pub min_severity: ide::SeverityBucket,
    /// Keep only these codes (empty = all).
    pub codes: Vec<String>,
    /// Cap on files swept (bounds the cost of an opt-in whole-config pass).
    pub max_files: usize,
}

/// One code's workspace-wide tally.
pub(crate) struct CodeAggregate {
    pub code: String,
    pub severity: ide::SeverityBucket,
    pub count: usize,
    pub files_affected: usize,
}

/// The result of a workspace sweep: per-code aggregates plus coverage bookkeeping.
pub(crate) struct WorkspaceSweep {
    pub aggregates: Vec<CodeAggregate>,
    /// Files actually analysed and served whole. Equals the capped request size on a
    /// completed sweep; smaller when the sweep was cancelled mid-flight, and smaller
    /// still by the files counted in `files_incomplete`, whose analysis reported
    /// itself incomplete and whose findings are withheld.
    pub files_swept: usize,
    pub files_total: usize,
    /// Files excluded by the vendor-diff analysis scope (no changed lines vs the
    /// configured base); 0 when no scope is configured.
    pub files_out_of_scope: usize,
    /// Files counted in `files_total` that could not be swept because their bytes
    /// could not be read. Beside `files_out_of_scope` and for the same reason: a gap
    /// in coverage is reported, never quietly removed from the total.
    pub files_unread: usize,
    /// Files swept whole but whose analysis reported itself incomplete (a rule's
    /// bounded projection hit its work limit). Their findings are withheld from the
    /// aggregates — classifying a partial finding set would renumber the file's
    /// occurrences and label its baseline entries on the strength of findings the
    /// file never produced — and their baseline entries are never resolved. Beside
    /// `files_unread` and for the same reason: coverage is reported, never assumed.
    pub files_incomplete: usize,
    /// Findings dropped because every covered line is attributed to an
    /// `[analysis].ignored_authors` entry; 0 when the filter is off.
    pub findings_ignored_by_author: usize,
    /// HEAD commit the author filter attributed against, folded into the
    /// result id so a filter rebuild after a ref move changes the identity.
    pub author_head: Option<String>,
    pub truncated: bool,
    /// The sweep was cancelled mid-flight (MCP `notifications/cancelled` or transport
    /// shutdown); `aggregates` cover only the `files_swept` files processed before it.
    pub cancelled: bool,
    /// How this sweep's findings classified against the diagnostics baseline; a
    /// cancelled sweep carries the unclassified summary, never a `Disabled` one.
    pub baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary,
    /// Identity of the baseline files the classification read, folded into the
    /// result id so a baseline edit alone changes it.
    pub baseline_epoch: String,
    /// The project analyzes external objects with no base to resolve their calls
    /// against, so every call into the owner is reported as unresolved. Recorded
    /// on the sweep so its envelope can say so without a separate status call.
    pub lacks_owning_configuration: bool,
}

impl WorkspaceSweep {
    /// A sweep cancelled before it selected a single file: nothing analysed, and the
    /// coverage numbers that do not depend on the selection still stated.
    pub(crate) fn nothing_swept(
        files_total: usize,
        files_unread: usize,
        baseline: ide::diagnostics_baseline::DiagnosticsBaselineSummary,
        baseline_epoch: String,
        lacks_owning_configuration: bool,
    ) -> Self {
        Self {
            aggregates: Vec::new(),
            files_swept: 0,
            files_total,
            files_out_of_scope: 0,
            files_unread,
            files_incomplete: 0,
            findings_ignored_by_author: 0,
            author_head: None,
            truncated: false,
            cancelled: true,
            baseline,
            baseline_epoch,
            lacks_owning_configuration,
        }
    }
}
