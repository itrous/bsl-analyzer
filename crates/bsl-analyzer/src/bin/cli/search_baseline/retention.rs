use std::{collections::BTreeMap, error::Error, io};

use chrono::{DateTime, Utc};

use super::{postgres, SearchBaselineRetentionArgs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotRetentionStatus {
    ActiveHead,
    SafetyHead,
    WithinWindow,
    ExpiredCandidate,
}

impl SnapshotRetentionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::ActiveHead => "active-head",
            Self::SafetyHead => "safety-head",
            Self::WithinWindow => "within-window",
            Self::ExpiredCandidate => "expired-candidate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SnapshotRetentionAssessment {
    pub(super) snapshot_id: String,
    pub(super) branch: String,
    pub(super) created_at: String,
    pub(super) age_days: Option<u32>,
    pub(super) status: SnapshotRetentionStatus,
    pub(super) protections: Vec<String>,
    pub(super) reason: String,
}

pub(super) fn run(args: SearchBaselineRetentionArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let project = project_model::Project::new(&args.source_dir)?;
    project.warn_about_external_discovery_rejections();
    let policy = &project.config.search.baseline.workspace_code.policy;
    require_configured(policy)?;

    let adapter = postgres::build_project_adapter_for_project(
        &project,
        project_model::PostgresAccessMode::Reader,
    )?;
    let catalog = adapter.snapshot_catalog()?;
    let snapshots = records_from_catalog(&catalog, branch_filter(args.branch.as_deref()));

    if snapshots.is_empty() {
        println!("No workspace-code snapshots found.");
        return Ok(());
    }

    print!("{}", render(policy, &snapshots, args.limit, Utc::now()));
    Ok(())
}

fn require_configured(
    policy: &project_model::SearchBaselinePolicyConfig,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if policy.is_configured() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "workspace baseline policy is not configured in .bsl-analyzer.json",
    )
    .into())
}

/// The `--branch` value as the listing it replaces read it: surrounding blanks dropped, blank
/// means every branch.
fn branch_filter(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|branch| !branch.is_empty())
}

/// The `workspace-code` snapshots of the catalogue, optionally of one branch, in the shape
/// `analyze` reads. Totals are not part of the catalogue and stay zero: retention never looks at
/// them.
pub(super) fn records_from_catalog(
    catalog: &bsl_search::BaselineSnapshotCatalog,
    branch: Option<&str>,
) -> Vec<bsl_search::BaselineSnapshotRecord> {
    catalog
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.corpus == bsl_search::CorpusId::WorkspaceCode.as_str())
        .filter(|snapshot| branch.is_none_or(|branch| snapshot.branch.as_deref() == Some(branch)))
        .map(|snapshot| bsl_search::BaselineSnapshotRecord {
            snapshot_id: snapshot.snapshot_id.clone(),
            corpus: snapshot.corpus.clone(),
            fingerprint: None,
            parent_snapshot_id: snapshot.parent_snapshot_id.clone(),
            branch: snapshot.branch.clone(),
            commit: None,
            created_at: snapshot.created_at.clone(),
            files: 0,
            documents: 0,
        })
        .collect()
}

/// The retention report: the summary counts every snapshot, `limit` only bounds the listing.
pub(super) fn render(
    policy: &project_model::SearchBaselinePolicyConfig,
    snapshots: &[bsl_search::BaselineSnapshotRecord],
    limit: usize,
    now: DateTime<Utc>,
) -> String {
    let assessments = analyze(policy, snapshots, now);
    let count = |status: SnapshotRetentionStatus| {
        assessments.iter().filter(|assessment| assessment.status == status).count()
    };
    let protected_by = |protection: &str| {
        assessments
            .iter()
            .filter(|assessment| assessment.protections.iter().any(|value| value == protection))
            .count()
    };

    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line("Shared baseline retention analysis:".to_owned());
    line("  Corpus:                     workspace-code".to_owned());
    line(format!("  Develop retention:          {} days", policy.retention.develop_retention_days));
    line(format!("  Vendor heads kept:          {}", policy.retention.vendor_keep_heads));
    line(format!("  Min snapshots per branch:   {}", policy.retention.min_snapshots_per_branch));
    line("  Note:                       delete expired candidates with `search baseline admin prune` (dry-run first); a retained snapshot whose parent is deleted is made standalone first.".to_owned());
    line(String::new());
    line("Summary:".to_owned());
    line(format!("  Active heads:               {}", count(SnapshotRetentionStatus::ActiveHead)));
    line(format!("  Safety heads:               {}", count(SnapshotRetentionStatus::SafetyHead)));
    line(format!("  Within window:              {}", count(SnapshotRetentionStatus::WithinWindow)));
    line(format!(
        "  Expired candidates:         {}",
        count(SnapshotRetentionStatus::ExpiredCandidate)
    ));
    line(format!("  Protected by min rule:      {}", protected_by("minimum-preservation")));
    line(format!("  Protected by ancestry:      {}", protected_by("has-descendants")));

    for assessment in assessments.into_iter().take(limit) {
        line(String::new());
        line(format!("  Snapshot:     {}", assessment.snapshot_id));
        line(format!("  Branch:       {}", assessment.branch));
        line(format!("  Created:      {}", assessment.created_at));
        line(format!(
            "  Age:          {}",
            assessment
                .age_days
                .map(|days| format!("{days} days"))
                .unwrap_or_else(|| "unknown".to_owned())
        ));
        line(format!("  Retention:    {}", assessment.status.as_str()));
        line(format!("  Reason:       {}", assessment.reason));
        line(format!(
            "  Protections:  {}",
            if assessment.protections.is_empty() {
                "-".to_owned()
            } else {
                assessment.protections.join(", ")
            }
        ));
    }
    out
}

pub(super) fn analyze(
    policy: &project_model::SearchBaselinePolicyConfig,
    snapshots: &[bsl_search::BaselineSnapshotRecord],
    now: DateTime<Utc>,
) -> Vec<SnapshotRetentionAssessment> {
    let mut by_branch = BTreeMap::<String, Vec<&bsl_search::BaselineSnapshotRecord>>::new();
    let mut children = std::collections::HashMap::<String, usize>::new();

    for snapshot in snapshots {
        if let Some(parent_snapshot_id) = snapshot.parent_snapshot_id.as_deref() {
            *children.entry(parent_snapshot_id.to_owned()).or_default() += 1;
        }
        if let Some(branch) = snapshot.branch.as_deref().filter(|branch| !branch.trim().is_empty())
        {
            by_branch.entry(branch.to_owned()).or_default().push(snapshot);
        }
    }

    for branch_snapshots in by_branch.values_mut() {
        branch_snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot_created_at(snapshot)));
    }

    let mut assessments = Vec::new();
    for (branch, branch_snapshots) in by_branch {
        for (index, snapshot) in branch_snapshots.into_iter().enumerate() {
            let age_days = snapshot_created_at(snapshot).and_then(|created_at| {
                now.signed_duration_since(created_at).num_days().try_into().ok()
            });
            let mut protections = Vec::new();
            if index < policy.retention.min_snapshots_per_branch {
                protections.push("minimum-preservation".to_owned());
            }
            if children.contains_key(&snapshot.snapshot_id) {
                protections.push("has-descendants".to_owned());
            }

            let (status, reason) = if index == 0 {
                (SnapshotRetentionStatus::ActiveHead, "latest snapshot for branch".to_owned())
            } else if branch == "vendor" && index < policy.retention.vendor_keep_heads {
                (
                    SnapshotRetentionStatus::SafetyHead,
                    format!("vendor keeps {} latest heads", policy.retention.vendor_keep_heads),
                )
            } else if branch != "vendor"
                && age_days.is_some_and(|days| days <= policy.retention.develop_retention_days)
            {
                (
                    SnapshotRetentionStatus::WithinWindow,
                    format!(
                        "branch is within {}-day retention window",
                        policy.retention.develop_retention_days
                    ),
                )
            } else {
                (
                    SnapshotRetentionStatus::ExpiredCandidate,
                    format!(
                        "outside branch retention policy (develop-like window: {} days)",
                        policy.retention.develop_retention_days
                    ),
                )
            };

            assessments.push(SnapshotRetentionAssessment {
                snapshot_id: snapshot.snapshot_id.clone(),
                branch: branch.clone(),
                created_at: snapshot.created_at.clone(),
                age_days,
                status,
                protections,
                reason,
            });
        }
    }

    assessments.sort_by(|lhs, rhs| {
        rhs.branch
            .cmp(&lhs.branch)
            .then_with(|| rhs.age_days.unwrap_or(0).cmp(&lhs.age_days.unwrap_or(0)))
            .then_with(|| lhs.snapshot_id.cmp(&rhs.snapshot_id))
    });
    assessments
}

fn snapshot_created_at(snapshot: &bsl_search::BaselineSnapshotRecord) -> Option<DateTime<Utc>> {
    project_model::parse_timestamp_utc(&snapshot.created_at)
}

#[cfg(test)]
mod tests {
    use bsl_search::BaselineSnapshotRecord;
    use chrono::{TimeZone, Utc};

    use super::{
        analyze, branch_filter, records_from_catalog, render, require_configured,
        SnapshotRetentionStatus,
    };

    #[test]
    fn the_branch_flag_drops_blanks_and_a_blank_one_means_every_branch() {
        assert_eq!(branch_filter(Some(" develop ")), Some("develop"));
        assert_eq!(branch_filter(Some("develop")), Some("develop"));
        assert_eq!(branch_filter(Some("   ")), None);
        assert_eq!(branch_filter(Some("")), None);
        assert_eq!(branch_filter(None), None);
    }

    #[test]
    fn retention_without_a_policy_is_refused_by_name() {
        let error = require_configured(&project_model::SearchBaselinePolicyConfig::default())
            .expect_err("an unconfigured policy is refused");
        assert_eq!(
            error.to_string(),
            "workspace baseline policy is not configured in .bsl-analyzer.json"
        );
        let configured: project_model::SearchBaselinePolicyConfig =
            serde_json::from_value(serde_json::json!({ "publishBranches": ["develop"] })).unwrap();
        assert!(require_configured(&configured).is_ok());
    }

    #[test]
    fn analyze_marks_vendor_safety_and_descendant_protection() {
        let policy: project_model::SearchBaselinePolicyConfig = serde_json::from_value(serde_json::json!({
            "publishBranches": ["vendor", "develop"],
            "branches": [{ "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }],
            "retention": { "developRetentionDays": 30, "vendorKeepHeads": 2, "minSnapshotsPerBranch": 1 }
        }))
        .unwrap();
        let snapshots = vec![
            snapshot_with_branch(
                "workspace-code:vendor@new",
                "vendor",
                "2026-04-01T00:00:00Z",
                None,
            ),
            snapshot_with_branch(
                "workspace-code:vendor@old",
                "vendor",
                "2026-03-01T00:00:00Z",
                Some("workspace-code:vendor@ancestor"),
            ),
            snapshot_with_branch(
                "workspace-code:vendor@ancestor",
                "vendor",
                "2026-02-01T00:00:00Z",
                None,
            ),
        ];

        let assessments =
            analyze(&policy, &snapshots, Utc.with_ymd_and_hms(2026, 4, 2, 0, 0, 0).unwrap());

        let new = assessments
            .iter()
            .find(|item| item.snapshot_id == "workspace-code:vendor@new")
            .unwrap();
        assert_eq!(new.status, SnapshotRetentionStatus::ActiveHead);

        let old = assessments
            .iter()
            .find(|item| item.snapshot_id == "workspace-code:vendor@old")
            .unwrap();
        assert_eq!(old.status, SnapshotRetentionStatus::SafetyHead);

        let ancestor = assessments
            .iter()
            .find(|item| item.snapshot_id == "workspace-code:vendor@ancestor")
            .unwrap();
        assert_eq!(ancestor.status, SnapshotRetentionStatus::ExpiredCandidate);
        assert!(ancestor.protections.iter().any(|value| value == "has-descendants"));
    }

    #[test]
    fn analyze_marks_recent_develop_snapshot_within_window() {
        let policy: project_model::SearchBaselinePolicyConfig = serde_json::from_value(serde_json::json!({
            "publishBranches": ["vendor", "develop"],
            "branches": [{ "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }],
            "retention": { "developRetentionDays": 30, "vendorKeepHeads": 2, "minSnapshotsPerBranch": 1 }
        }))
        .unwrap();
        let snapshots = vec![
            snapshot_with_branch(
                "workspace-code:develop@new",
                "develop",
                "2026-04-01T00:00:00Z",
                None,
            ),
            snapshot_with_branch(
                "workspace-code:develop@recent",
                "develop",
                "2026-03-20T00:00:00Z",
                None,
            ),
            snapshot_with_branch(
                "workspace-code:develop@old",
                "develop",
                "2026-01-01T00:00:00Z",
                None,
            ),
        ];

        let assessments =
            analyze(&policy, &snapshots, Utc.with_ymd_and_hms(2026, 4, 2, 0, 0, 0).unwrap());

        let recent = assessments
            .iter()
            .find(|item| item.snapshot_id == "workspace-code:develop@recent")
            .unwrap();
        assert_eq!(recent.status, SnapshotRetentionStatus::WithinWindow);

        let old = assessments
            .iter()
            .find(|item| item.snapshot_id == "workspace-code:develop@old")
            .unwrap();
        assert_eq!(old.status, SnapshotRetentionStatus::ExpiredCandidate);
    }

    #[test]
    fn retention_assesses_the_whole_catalog() {
        let policy: project_model::SearchBaselinePolicyConfig = serde_json::from_value(serde_json::json!({
            "publishBranches": ["vendor", "develop"],
            "branches": [{ "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }],
            "retention": { "developRetentionDays": 30, "vendorKeepHeads": 2, "minSnapshotsPerBranch": 1 }
        }))
        .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap();
        let at = |days: i64| {
            (now - chrono::Duration::days(days) - chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        };
        // One more than the listing limit of 200, so a reader bounded by it cannot pass.
        let mut snapshots: Vec<bsl_search::BaselineCatalogSnapshot> = (0..201)
            .map(|day| bsl_search::BaselineCatalogSnapshot {
                snapshot_id: format!("d{day:03}"),
                corpus: "workspace-code".to_owned(),
                branch: Some("develop".to_owned()),
                parent_snapshot_id: None,
                created_at: at(day),
            })
            .collect();
        snapshots.push(bsl_search::BaselineCatalogSnapshot {
            snapshot_id: "ref".to_owned(),
            corpus: "reference".to_owned(),
            branch: Some("develop".to_owned()),
            parent_snapshot_id: None,
            created_at: at(1),
        });
        let catalog = bsl_search::BaselineSnapshotCatalog { snapshots, heads: vec![] };

        let records = records_from_catalog(&catalog, None);
        assert_eq!(records.len(), 201, "only workspace-code is assessed");
        let report = render(&policy, &records, 5, now);
        for line in [
            "  Active heads:               1",
            "  Within window:              30",
            "  Expired candidates:         170",
        ] {
            assert!(report.contains(line), "{line} missing from:\n{report}");
        }
        assert_eq!(report.matches("  Snapshot:     ").count(), 5, "the limit bounds the listing");
        assert!(report.contains("search baseline admin prune"), "{report}");

        let yesterday = analyze(&policy, &records, now)
            .into_iter()
            .find(|assessment| assessment.snapshot_id == "d001")
            .unwrap();
        assert_eq!(yesterday.status, SnapshotRetentionStatus::WithinWindow);
        assert!(records_from_catalog(&catalog, Some("vendor")).is_empty());
    }

    fn snapshot_with_branch(
        snapshot_id: &str,
        branch: &str,
        created_at: &str,
        parent_snapshot_id: Option<&str>,
    ) -> BaselineSnapshotRecord {
        BaselineSnapshotRecord {
            snapshot_id: snapshot_id.to_owned(),
            corpus: "workspace-code".to_owned(),
            fingerprint: None,
            parent_snapshot_id: parent_snapshot_id.map(ToOwned::to_owned),
            branch: Some(branch.to_owned()),
            commit: None,
            created_at: created_at.to_owned(),
            files: 0,
            documents: 0,
        }
    }
}
