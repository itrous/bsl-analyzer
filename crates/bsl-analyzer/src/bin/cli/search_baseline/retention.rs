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
    if !policy.is_configured() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace baseline policy is not configured in .bsl-analyzer.json",
        )
        .into());
    }

    let adapter = postgres::build_project_adapter_for_project(
        &project,
        project_model::PostgresAccessMode::Reader,
    )?;
    let snapshots = adapter.list_snapshots(
        Some(bsl_search::CorpusId::WorkspaceCode.as_str()),
        args.branch.as_deref(),
        None,
        args.limit,
    )?;

    if snapshots.is_empty() {
        println!("No workspace-code snapshots found.");
        return Ok(());
    }

    let assessments = analyze(policy, &snapshots, Utc::now());
    let active_heads = assessments
        .iter()
        .filter(|assessment| matches!(assessment.status, SnapshotRetentionStatus::ActiveHead))
        .count();
    let safety_heads = assessments
        .iter()
        .filter(|assessment| matches!(assessment.status, SnapshotRetentionStatus::SafetyHead))
        .count();
    let within_window = assessments
        .iter()
        .filter(|assessment| matches!(assessment.status, SnapshotRetentionStatus::WithinWindow))
        .count();
    let expired_candidates = assessments
        .iter()
        .filter(|assessment| matches!(assessment.status, SnapshotRetentionStatus::ExpiredCandidate))
        .count();
    let protected_by_min = assessments
        .iter()
        .filter(|assessment| {
            assessment.protections.iter().any(|value| value == "minimum-preservation")
        })
        .count();
    let protected_by_ancestry = assessments
        .iter()
        .filter(|assessment| assessment.protections.iter().any(|value| value == "has-descendants"))
        .count();

    println!("Shared baseline retention analysis:");
    println!("  Corpus:                     workspace-code");
    println!("  Develop retention:          {} days", policy.retention.develop_retention_days);
    println!("  Vendor heads kept:          {}", policy.retention.vendor_keep_heads);
    println!("  Min snapshots per branch:   {}", policy.retention.min_snapshots_per_branch);
    println!("  Note:                       destructive snapshot cleanup is not implemented; candidates are advisory only.");
    println!();
    println!("Summary:");
    println!("  Active heads:               {}", active_heads);
    println!("  Safety heads:               {}", safety_heads);
    println!("  Within window:              {}", within_window);
    println!("  Expired candidates:         {}", expired_candidates);
    println!("  Protected by min rule:      {}", protected_by_min);
    println!("  Protected by ancestry:      {}", protected_by_ancestry);

    for assessment in assessments {
        println!();
        println!("  Snapshot:     {}", assessment.snapshot_id);
        println!("  Branch:       {}", assessment.branch);
        println!("  Created:      {}", assessment.created_at);
        println!(
            "  Age:          {}",
            assessment
                .age_days
                .map(|days| format!("{days} days"))
                .unwrap_or_else(|| "unknown".to_owned())
        );
        println!("  Retention:    {}", assessment.status.as_str());
        println!("  Reason:       {}", assessment.reason);
        println!(
            "  Protections:  {}",
            if assessment.protections.is_empty() {
                "-".to_owned()
            } else {
                assessment.protections.join(", ")
            }
        );
    }

    Ok(())
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

    use super::{analyze, SnapshotRetentionStatus};

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
