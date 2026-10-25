use std::{error::Error, io};

use super::{postgres, SearchBaselineAdminGcArgs, SearchBaselineAdminMigrateArgs};

pub(super) fn gc(args: SearchBaselineAdminGcArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let adapter = postgres::build_project_adapter(
        &args.source_dir,
        project_model::PostgresAccessMode::Migrator,
    )?;
    let report = adapter.garbage_collect(args.execute)?;
    print!("{}", render_gc(&report, args.execute));
    Ok(())
}

fn render_gc(report: &bsl_search::BaselineGcReport, executed: bool) -> String {
    let mut lines = Vec::new();
    if executed {
        lines.push("Shared baseline garbage collection finished.".to_owned());
        lines.push(format!("  Deleted file objects:       {}", report.deleted_file_objects));
        lines.push(format!("  Deleted file object items:  {}", report.deleted_file_object_items));
        lines.push(format!("  Deleted semantic rows:      {}", report.deleted_semantic_embeddings));
        lines.push(format!("  Deleted content objects:    {}", report.deleted_content_objects));
    } else {
        lines.push("Shared baseline garbage collection dry-run.".to_owned());
        lines.push("  Use --execute to apply deletions.".to_owned());
    }
    lines.push(format!("  Orphan file objects:        {}", report.orphan_file_objects));
    lines.push(format!("  Orphan file object items:   {}", report.orphan_file_object_items));
    lines.push(format!("  Orphan semantic rows:       {}", report.orphan_semantic_embeddings));
    lines.push(format!("  Orphan content objects:     {}", report.orphan_content_objects));
    lines.iter().map(|line| format!("{line}\n")).collect()
}

pub(super) fn migrate(
    args: SearchBaselineAdminMigrateArgs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let project = project_model::Project::new(&args.source_dir)?;
    project.warn_about_external_discovery_rejections();
    let resolved_pg = postgres::resolve_project_url(
        &project.config.search.baseline.postgres,
        project_model::PostgresAccessMode::Migrator,
    )
    .map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("failed to resolve PostgreSQL migrator credentials: {error}"),
        )
    })?;
    let adapter = postgres::build_adapter(
        &resolved_pg.url,
        project.config.search.baseline.postgres.schema.as_deref(),
    )?;
    let schema_label = project
        .config
        .search
        .baseline
        .postgres
        .schema
        .clone()
        .unwrap_or_else(|| "bsl_search".to_owned());

    adapter.migrate_storage()?;

    println!("PostgreSQL baseline storage is ready.");
    println!("  Schema: {}", schema_label);
    println!("  Role:   migrator");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::render_gc;

    fn report() -> bsl_search::BaselineGcReport {
        bsl_search::BaselineGcReport {
            orphan_file_objects: 1,
            orphan_file_object_items: 2,
            orphan_semantic_embeddings: 3,
            orphan_content_objects: 4,
            deleted_file_objects: 5,
            deleted_file_object_items: 6,
            deleted_semantic_embeddings: 7,
            deleted_content_objects: 8,
        }
    }

    #[test]
    fn gc_reports_every_count_it_deleted() {
        expect_test::expect![[r#"
            Shared baseline garbage collection finished.
              Deleted file objects:       5
              Deleted file object items:  6
              Deleted semantic rows:      7
              Deleted content objects:    8
              Orphan file objects:        1
              Orphan file object items:   2
              Orphan semantic rows:       3
              Orphan content objects:     4
        "#]]
        .assert_eq(&render_gc(&report(), true));
    }

    #[test]
    fn a_gc_dry_run_reports_the_orphans_and_deletes_nothing() {
        expect_test::expect![[r#"
            Shared baseline garbage collection dry-run.
              Use --execute to apply deletions.
              Orphan file objects:        1
              Orphan file object items:   2
              Orphan semantic rows:       3
              Orphan content objects:     4
        "#]]
        .assert_eq(&render_gc(&report(), false));
    }
}
