use std::{error::Error, io};

use super::{postgres, SearchBaselineAdminGcArgs, SearchBaselineAdminMigrateArgs};

pub(super) fn gc(args: SearchBaselineAdminGcArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let adapter = postgres::build_project_adapter(
        &args.source_dir,
        project_model::PostgresAccessMode::Migrator,
    )?;
    let report = adapter.garbage_collect(args.execute)?;

    if args.execute {
        println!("Shared baseline garbage collection finished.");
        println!("  Deleted file objects:       {}", report.deleted_file_objects);
        println!("  Deleted file object items:  {}", report.deleted_file_object_items);
        println!("  Deleted semantic rows:      {}", report.deleted_semantic_embeddings);
    } else {
        println!("Shared baseline garbage collection dry-run.");
        println!("  Use --execute to apply deletions.");
    }
    println!("  Orphan file objects:        {}", report.orphan_file_objects);
    println!("  Orphan file object items:   {}", report.orphan_file_object_items);
    println!("  Orphan semantic rows:       {}", report.orphan_semantic_embeddings);

    Ok(())
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
