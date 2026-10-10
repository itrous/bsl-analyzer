use crate::configuration::Configuration;
use crate::error::Result;
use crate::metadata_object::{MdoType, MetadataObject};
use crate::xml_parser;
use bsl_conventions::DirTree;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use stdx::case::CaseExt;
use stdx::path_exclusion::ExcludedPaths;

use crate::scoped_fs::ScopedFs;

pub fn load_from_directory(path: impl AsRef<Path>) -> Result<Configuration> {
    load_from_directory_scoped(path, &ExcludedPaths::default())
}

/// [`load_from_directory`] that reads nothing inside a directory the user excluded:
/// no collection, object or module there is listed, probed or read. An excluded root
/// loads as an empty configuration.
pub fn load_from_directory_scoped(
    path: impl AsRef<Path>,
    excluded: &ExcludedPaths,
) -> Result<Configuration> {
    let path = path.as_ref();
    let scope = ScopedFs::new(excluded);
    let _span = tracing::info_span!("load_from_directory", ?path).entered();

    // Reaching this under an exclusive-pool job means a pre-pool warm-up missed
    // this config root: the load is about to park a build worker for a whole
    // configuration's XML parse. `off_exclusive_pool` keeps that from
    // deadlocking, but the miss itself stays a defect worth naming.
    if stdx::par_guard::no_nested_parallelism() {
        tracing::error!(
            ?path,
            "whole-config metadata load entered from a no-nested-parallelism job; \
             the pre-pool warm-up did not cover this config root"
        );
    }

    let loaded = off_exclusive_pool(|| load_all_metadata_parallel(path, &scope));
    let mut config = build_configuration(loaded);
    // An export root holds no collection at all; its one object is the whole
    // configuration this root contributes to a visibility chain.
    if let Some(external) = crate::external_object::load_external_object_in(path, &scope) {
        config.add_metadata_object(external);
    }

    tracing::info!(
        common_modules = config.common_modules().len(),
        metadata_objects = config.metadata_objects().len(),
        registers = config.registers().len(),
        event_subscriptions = config.event_subscriptions().len(),
        scheduled_jobs = config.scheduled_jobs().len(),
        roles = config.roles().len(),
        defined_types = config.defined_types().len(),
        http_services = config.http_services().len(),
        web_services = config.web_services().len(),
        "configuration loaded"
    );

    Ok(config)
}

/// Run `f` off the pool of a job that forbids nested parallelism, and directly
/// otherwise.
///
/// The whole-config load fans out over a `rayon::scope` on the CURRENT pool.
/// Under an exclusive-pool job (a graph-build worker) that nesting can deadlock:
/// rayon keeps stealing while a worker waits on a latch, so the waiting worker
/// can pick up a sibling job whose query re-enters a memo suspended on this very
/// thread. Handing the work to a plain OS thread moves the fan-out to the global
/// pool and parks the caller in `join`, which steals nothing. Installing on
/// another rayon pool would NOT do: the caller would still wait on a latch.
/// Probe `Ext/<name>` under an object directory, canonical spelling first;
/// the returned path carries the REAL on-disk spelling (it flows into
/// `module_file` and URIs, which must agree with the scanned universe).
fn probe_ext_child(
    tree: &dyn DirTree,
    dir: &Path,
    name: bsl_conventions::ConventionalName,
) -> Option<std::path::PathBuf> {
    bsl_conventions::resolve_chain_ci_in(
        tree,
        dir,
        &[bsl_conventions::ConventionalName::Ext.canonical(), name.canonical()],
    )
}

/// The `<name>.xml` sibling of an object directory: the stem is the OBJECT's
/// name and matches exactly, only the extension is case-insensitive.
fn probe_sibling_xml(tree: &dyn DirTree, dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    bsl_conventions::find_child_stem_exact_in(tree, dir, name, bsl_conventions::XML_EXTENSION)
}

/// A collection directory under the root: canonical English spelling first,
/// else the case variant the tree actually uses. Falls back to the constructed
/// path so an absent collection keeps reading as empty.
fn collection_dir(tree: &dyn DirTree, root: &Path, name: &str) -> std::path::PathBuf {
    bsl_conventions::find_child_ci_in(tree, root, name).unwrap_or_else(|| root.join(name))
}

fn off_exclusive_pool<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    if !stdx::par_guard::no_nested_parallelism() {
        return f();
    }

    std::thread::scope(|scope| scope.spawn(f).join())
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

struct LoadedMetadata {
    common_modules: Vec<crate::common_module::CommonModule>,
    catalogs: Vec<MetadataObject>,
    documents: Vec<MetadataObject>,
    info_registers: Vec<crate::register::Register>,
    accum_registers: Vec<crate::register::Register>,
    account_registers: Vec<crate::register::Register>,
    calc_registers: Vec<crate::register::Register>,
    event_subscriptions: Vec<crate::event_subscription::EventSubscription>,
    scheduled_jobs: Vec<crate::scheduled_job::ScheduledJob>,
    roles: Vec<crate::role::Role>,
    defined_types: Vec<crate::defined_type::DefinedType>,
    charts_char_types: Vec<MetadataObject>,
    constants: Vec<MetadataObject>,
    exchange_plans: Vec<MetadataObject>,
    business_processes: Vec<MetadataObject>,
    enums: Vec<MetadataObject>,
    tasks: Vec<MetadataObject>,
    charts_accounts: Vec<MetadataObject>,
    charts_calc_types: Vec<MetadataObject>,
    external_data_sources: Vec<MetadataObject>,
    http_services: Vec<crate::http_service::HTTPService>,
    web_services: Vec<crate::web_service::WebService>,
    integration_services: Vec<crate::integration_service::IntegrationService>,
    data_processors: Vec<MetadataObject>,
    reports: Vec<MetadataObject>,
    subsystems: Vec<crate::subsystem::Subsystem>,
    common_attributes: crate::common_attribute::CommonAttributeSet,
}

fn load_all_metadata_parallel(path: &Path, scope: &ScopedFs<'_>) -> LoadedMetadata {
    let start = std::time::Instant::now();
    let common_modules = Mutex::new(Vec::new());
    let catalogs = Mutex::new(Vec::new());
    let documents = Mutex::new(Vec::new());
    let info_registers = Mutex::new(Vec::new());
    let accum_registers = Mutex::new(Vec::new());
    let account_registers = Mutex::new(Vec::new());
    let calc_registers = Mutex::new(Vec::new());
    let event_subscriptions = Mutex::new(Vec::new());
    let scheduled_jobs = Mutex::new(Vec::new());
    let roles = Mutex::new(Vec::new());
    let defined_types = Mutex::new(Vec::new());
    let charts_char_types = Mutex::new(Vec::new());
    let constants = Mutex::new(Vec::new());
    let exchange_plans = Mutex::new(Vec::new());
    let business_processes = Mutex::new(Vec::new());
    let enums = Mutex::new(Vec::new());
    let tasks = Mutex::new(Vec::new());
    let charts_accounts = Mutex::new(Vec::new());
    let charts_calc_types = Mutex::new(Vec::new());
    let external_data_sources = Mutex::new(Vec::new());
    let http_services = Mutex::new(Vec::new());
    let web_services = Mutex::new(Vec::new());
    let integration_services = Mutex::new(Vec::new());
    let data_processors = Mutex::new(Vec::new());
    let reports = Mutex::new(Vec::new());
    let subsystems = Mutex::new(Vec::new());
    let common_attributes = Mutex::new(crate::common_attribute::CommonAttributeSet::default());

    rayon::scope(|s| {
        s.spawn(|_| {
            *common_modules.lock().unwrap() =
                load_common_modules_parallel(scope, &collection_dir(scope, path, "CommonModules"))
        });
        s.spawn(|_| {
            *catalogs.lock().unwrap() =
                load_catalogs_parallel(scope, &collection_dir(scope, path, "Catalogs"))
        });
        s.spawn(|_| {
            *documents.lock().unwrap() =
                load_documents_parallel(scope, &collection_dir(scope, path, "Documents"))
        });
        s.spawn(|_| {
            *info_registers.lock().unwrap() = load_information_registers_parallel(
                scope,
                &collection_dir(scope, path, "InformationRegisters"),
            )
        });
        s.spawn(|_| {
            *accum_registers.lock().unwrap() = load_accumulation_registers_parallel(
                scope,
                &collection_dir(scope, path, "AccumulationRegisters"),
            )
        });
        s.spawn(|_| {
            *account_registers.lock().unwrap() = load_accounting_registers_parallel(
                scope,
                &collection_dir(scope, path, "AccountingRegisters"),
            )
        });
        s.spawn(|_| {
            *calc_registers.lock().unwrap() = load_calculation_registers_parallel(
                scope,
                &collection_dir(scope, path, "CalculationRegisters"),
            )
        });
        s.spawn(|_| {
            *event_subscriptions.lock().unwrap() = load_event_subscriptions_parallel(
                scope,
                &collection_dir(scope, path, "EventSubscriptions"),
            )
        });
        s.spawn(|_| {
            *scheduled_jobs.lock().unwrap() =
                load_scheduled_jobs_parallel(scope, &collection_dir(scope, path, "ScheduledJobs"))
        });
        s.spawn(|_| {
            *roles.lock().unwrap() =
                load_roles_parallel(scope, &collection_dir(scope, path, "Roles"))
        });
        s.spawn(|_| {
            *defined_types.lock().unwrap() =
                load_defined_types_parallel(scope, &collection_dir(scope, path, "DefinedTypes"))
        });
        s.spawn(|_| {
            *charts_char_types.lock().unwrap() = load_charts_of_characteristic_types_parallel(
                scope,
                &collection_dir(scope, path, "ChartsOfCharacteristicTypes"),
            )
        });
        s.spawn(|_| {
            *constants.lock().unwrap() =
                load_constants_parallel(scope, &collection_dir(scope, path, "Constants"))
        });
        s.spawn(|_| {
            *exchange_plans.lock().unwrap() =
                load_exchange_plans_parallel(scope, &collection_dir(scope, path, "ExchangePlans"))
        });
        s.spawn(|_| {
            *business_processes.lock().unwrap() = load_business_processes_parallel(
                scope,
                &collection_dir(scope, path, "BusinessProcesses"),
            )
        });
        s.spawn(|_| {
            *enums.lock().unwrap() =
                load_enums_parallel(scope, &collection_dir(scope, path, "Enums"))
        });
        s.spawn(|_| {
            *tasks.lock().unwrap() =
                load_tasks_parallel(scope, &collection_dir(scope, path, "Tasks"))
        });
        s.spawn(|_| {
            *charts_accounts.lock().unwrap() = load_charts_of_accounts_parallel(
                scope,
                &collection_dir(scope, path, "ChartsOfAccounts"),
            )
        });
        s.spawn(|_| {
            *charts_calc_types.lock().unwrap() = load_charts_of_calculation_types_parallel(
                scope,
                &collection_dir(scope, path, "ChartsOfCalculationTypes"),
            )
        });
        s.spawn(|_| {
            *external_data_sources.lock().unwrap() = load_simple_metadata_objects_parallel(
                scope,
                &collection_dir(scope, path, "ExternalDataSources"),
                MdoType::ExternalDataSource,
            )
        });
        s.spawn(|_| {
            *http_services.lock().unwrap() =
                load_http_services_parallel(scope, &collection_dir(scope, path, "HTTPServices"))
        });
        s.spawn(|_| {
            *web_services.lock().unwrap() =
                load_web_services_parallel(scope, &collection_dir(scope, path, "WebServices"))
        });
        s.spawn(|_| {
            *integration_services.lock().unwrap() = load_integration_services_parallel(
                scope,
                &collection_dir(scope, path, "IntegrationServices"),
            )
        });
        s.spawn(|_| {
            *data_processors.lock().unwrap() =
                load_data_processors_parallel(scope, &collection_dir(scope, path, "DataProcessors"))
        });
        s.spawn(|_| {
            *reports.lock().unwrap() =
                load_reports_parallel(scope, &collection_dir(scope, path, "Reports"))
        });
        s.spawn(|_| {
            *subsystems.lock().unwrap() =
                load_subsystems(scope, &collection_dir(scope, path, "Subsystems"))
        });
        s.spawn(|_| {
            *common_attributes.lock().unwrap() =
                load_common_attributes(scope, &collection_dir(scope, path, "CommonAttributes"))
        });
    });

    let result = LoadedMetadata {
        common_modules: common_modules.into_inner().unwrap(),
        catalogs: catalogs.into_inner().unwrap(),
        documents: documents.into_inner().unwrap(),
        info_registers: info_registers.into_inner().unwrap(),
        accum_registers: accum_registers.into_inner().unwrap(),
        account_registers: account_registers.into_inner().unwrap(),
        calc_registers: calc_registers.into_inner().unwrap(),
        event_subscriptions: event_subscriptions.into_inner().unwrap(),
        scheduled_jobs: scheduled_jobs.into_inner().unwrap(),
        roles: roles.into_inner().unwrap(),
        defined_types: defined_types.into_inner().unwrap(),
        charts_char_types: charts_char_types.into_inner().unwrap(),
        constants: constants.into_inner().unwrap(),
        exchange_plans: exchange_plans.into_inner().unwrap(),
        business_processes: business_processes.into_inner().unwrap(),
        enums: enums.into_inner().unwrap(),
        tasks: tasks.into_inner().unwrap(),
        charts_accounts: charts_accounts.into_inner().unwrap(),
        charts_calc_types: charts_calc_types.into_inner().unwrap(),
        external_data_sources: external_data_sources.into_inner().unwrap(),
        http_services: http_services.into_inner().unwrap(),
        web_services: web_services.into_inner().unwrap(),
        integration_services: integration_services.into_inner().unwrap(),
        data_processors: data_processors.into_inner().unwrap(),
        reports: reports.into_inner().unwrap(),
        subsystems: subsystems.into_inner().unwrap(),
        common_attributes: common_attributes.into_inner().unwrap(),
    };

    tracing::info!(
        path = %path.display(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        common_modules = result.common_modules.len(),
        catalogs = result.catalogs.len(),
        documents = result.documents.len(),
        data_processors = result.data_processors.len(),
        reports = result.reports.len(),
        "load_all_metadata_parallel complete",
    );

    result
}

fn build_configuration(loaded: LoadedMetadata) -> Configuration {
    let mut config = Configuration::new("Configuration");

    for module in loaded.common_modules {
        config.add_common_module(module);
    }
    for obj in loaded.catalogs {
        config.add_metadata_object(obj);
    }
    for obj in loaded.documents {
        config.add_metadata_object(obj);
    }
    for reg in loaded.info_registers {
        config.add_register(reg);
    }
    for reg in loaded.accum_registers {
        config.add_register(reg);
    }
    for reg in loaded.account_registers {
        config.add_register(reg);
    }
    for reg in loaded.calc_registers {
        config.add_register(reg);
    }
    for sub in loaded.event_subscriptions {
        config.add_event_subscription(sub);
    }
    for job in loaded.scheduled_jobs {
        config.add_scheduled_job(job);
    }
    for role in loaded.roles {
        config.add_role(role);
    }
    for dt in loaded.defined_types {
        config.add_defined_type(dt);
    }
    for obj in loaded.charts_char_types {
        config.add_metadata_object(obj);
    }
    for obj in loaded.constants {
        config.add_metadata_object(obj);
    }
    for obj in loaded.exchange_plans {
        config.add_metadata_object(obj);
    }
    for obj in loaded.business_processes {
        config.add_metadata_object(obj);
    }
    for obj in loaded.enums {
        config.add_metadata_object(obj);
    }
    for obj in loaded.tasks {
        config.add_metadata_object(obj);
    }
    for obj in loaded.charts_accounts {
        config.add_metadata_object(obj);
    }
    for obj in loaded.charts_calc_types {
        config.add_metadata_object(obj);
    }
    for obj in loaded.external_data_sources {
        config.add_metadata_object(obj);
    }
    for obj in loaded.data_processors {
        config.add_metadata_object(obj);
    }
    for obj in loaded.reports {
        config.add_metadata_object(obj);
    }
    for svc in loaded.http_services {
        config.add_http_service(svc);
    }
    for svc in loaded.web_services {
        config.add_web_service(svc);
    }
    for svc in loaded.integration_services {
        config.add_integration_service(svc);
    }
    for subsystem in loaded.subsystems {
        config.add_subsystem(subsystem);
    }
    config.apply_common_attributes(&loaded.common_attributes);

    config
}

/// Read every `CommonAttributes/<Name>.xml` of one root. A file that exists but does not
/// parse is remembered rather than dropped: the field it would add is unknown, so the field
/// sets of the objects it could reach are no longer exhaustive.
fn load_common_attributes(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> crate::common_attribute::CommonAttributeSet {
    let mut set = crate::common_attribute::CommonAttributeSet::default();
    for (_, path) in loose_xml_children(scope, dir) {
        match fs::read_to_string(&path) {
            Ok(xml) => set.push_parsed(parse_common_attribute_from_text(&xml)),
            Err(error) => {
                tracing::warn!(?path, %error, "common attribute XML is unreadable");
                set.unreadable = true;
            }
        }
    }
    set
}

/// Load every subsystem `.xml` under `Subsystems/`, recursing into nested
/// `<Name>/Subsystems/` directories. Each file directly inside a `Subsystems` directory is
/// one subsystem; the parent/child relationship is carried in each subsystem's
/// `child_subsystems`, not inferred from the directory layout.
fn load_subsystems(scope: &ScopedFs<'_>, dir: &Path) -> Vec<crate::subsystem::Subsystem> {
    let mut out = Vec::new();
    collect_subsystems(scope, dir, &mut out);
    out
}

fn collect_subsystems(
    scope: &ScopedFs<'_>,
    dir: &Path,
    out: &mut Vec<crate::subsystem::Subsystem>,
) {
    if !dir.exists() {
        return;
    }
    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    for entry in entries {
        let path = entry.path();
        if path.is_file() && bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION) {
            if let Ok(xml) = fs::read_to_string(&path) {
                if let Ok(subsystem) = xml_parser::parse_subsystem_xml(&xml) {
                    out.push(subsystem);
                }
            }
        } else if path.is_dir() {
            // Nested subsystems live under `<Name>/Subsystems/`.
            collect_subsystems(scope, &collection_dir(scope, &path, "Subsystems"), out);
        }
    }
}

fn load_common_modules_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::common_module::CommonModule> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let module_dir = entry.path();
            if !module_dir.is_dir() {
                return None;
            }

            let name = module_dir.file_name()?.to_str()?;
            let xml_path = probe_sibling_xml(scope, dir, name)?;
            let module_bsl_path =
                probe_ext_child(scope, &module_dir, bsl_conventions::ConventionalName::Module);
            let module_bin_path =
                probe_ext_child(scope, &module_dir, bsl_conventions::ConventionalName::ModuleBin);

            let xml = fs::read_to_string(&xml_path).ok()?;
            let mut module = xml_parser::parse_common_module_xml(&xml).ok()?;

            let is_protected = module_bin_path.is_some() && module_bsl_path.is_none();

            if let Some(found) = &module_bsl_path {
                let suffix = found
                    .strip_prefix(&module_dir)
                    .unwrap_or(found.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                // Сегмент коллекции — тоже реальное написание: `dir` пришёл из
                // пробы каталога коллекции.
                let collection =
                    dir.file_name().and_then(|n| n.to_str()).unwrap_or("CommonModules");
                let uri = format!("{}/{}/{}", collection, name, suffix);
                module.set_uri(Some(uri));
                module.set_protected(false);
            } else if is_protected {
                module.set_uri(None);
                module.set_protected(true);
            }

            Some(module)
        })
        .collect()
}

fn load_catalogs_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_catalog_xml)
}

fn load_documents_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_document_xml)
}

fn load_business_processes_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_business_process_xml)
}

fn load_tasks_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_task_xml)
}

fn load_exchange_plans_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_exchange_plan_xml)
}

fn load_charts_of_characteristic_types_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_chart_of_characteristic_types_xml)
}

fn load_charts_of_accounts_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_chart_of_accounts_xml)
}

fn load_charts_of_calculation_types_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_chart_of_calculation_types_xml)
}

fn load_data_processors_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_data_processor_xml)
}

fn load_reports_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    load_metadata_objects_parallel(scope, dir, xml_parser::parse_report_xml)
}

fn load_metadata_objects_parallel<F>(
    scope: &ScopedFs<'_>,
    dir: &Path,
    parser: F,
) -> Vec<MetadataObject>
where
    F: Fn(&str) -> Result<MetadataObject> + Sync,
{
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    let dir_names: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                path.file_name()?.to_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();

            if path.is_dir() {
                let name = path.file_name()?.to_str()?;
                let xml_path = probe_sibling_xml(scope, dir, name)?;
                let main_xml = fs::read_to_string(&xml_path).ok()?;
                let predefined_xml =
                    probe_ext_child(scope, &path, bsl_conventions::ConventionalName::PredefinedXml)
                        .and_then(|p| fs::read_to_string(p).ok());

                build_metadata_object(&main_xml, predefined_xml.as_deref(), &parser)
            } else if bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION) {
                let file_stem = path.file_stem()?.to_str()?;

                if dir_names.contains(file_stem) {
                    return None;
                }

                let xml = fs::read_to_string(&path).ok()?;
                build_metadata_object(&xml, None, &parser)
            } else {
                None
            }
        })
        .collect()
}

/// Build one metadata object from its already-read composing XML texts: the main
/// `<name>.xml` plus an optional `Ext/Predefined.xml`. Pure (no filesystem access)
/// so it can back a per-MDO Salsa parse query whose reads go through the versioned
/// VFS, while the directory loaders above supply the texts from disk.
fn build_metadata_object<F>(
    main_xml: &str,
    predefined_xml: Option<&str>,
    parser: &F,
) -> Option<MetadataObject>
where
    F: Fn(&str) -> Result<MetadataObject> + ?Sized,
{
    let mut mdo = parser(main_xml).ok()?;

    if let Some(predefined_xml) = predefined_xml {
        mdo.predefined_items = xml_parser::parse_predefined_xml(predefined_xml);
        tracing::debug!(
            name = %mdo.name,
            count = mdo.predefined_items.len(),
            "Loaded predefined items"
        );
    }

    Some(mdo)
}

/// The XML parser for a content-parsed metadata-object kind, or `None` for kinds
/// that are not assembled into a [`MetadataObject`] from a single XML text here
/// (registers, roles, subsystems, defined types, services, and name-only "simple"
/// objects, which have their own loaders / are constructed by name).
fn metadata_object_parser(mdo_type: MdoType) -> Option<fn(&str) -> Result<MetadataObject>> {
    use xml_parser::*;
    Some(match mdo_type {
        MdoType::Catalog => parse_catalog_xml,
        MdoType::Document => parse_document_xml,
        MdoType::BusinessProcess => parse_business_process_xml,
        MdoType::Task => parse_task_xml,
        MdoType::ExchangePlan => parse_exchange_plan_xml,
        MdoType::ChartOfCharacteristicTypes => parse_chart_of_characteristic_types_xml,
        MdoType::ChartOfCalculationTypes => parse_chart_of_calculation_types_xml,
        MdoType::ChartOfAccounts => parse_chart_of_accounts_xml,
        MdoType::DataProcessor => parse_data_processor_xml,
        MdoType::Report => parse_report_xml,
        MdoType::ExternalDataProcessor => parse_external_data_processor_xml,
        MdoType::ExternalReport => parse_external_report_xml,
        MdoType::Enum => parse_enum_xml,
        MdoType::Constant => parse_constant_xml,
        _ => return None,
    })
}

/// Parse one metadata object of `mdo_type` from its already-read composing XML
/// texts (the main `<Name>.xml` plus an optional `Ext/Predefined.xml`). This is
/// the content-parsing entry the per-MDO Salsa query calls after reading the
/// texts through the versioned VFS. Returns `None` for kinds not content-parsed
/// into a [`MetadataObject`] here (see [`metadata_object_parser`]).
pub fn parse_metadata_object_from_texts(
    mdo_type: MdoType,
    main_xml: &str,
    predefined_xml: Option<&str>,
) -> Option<MetadataObject> {
    let parser = metadata_object_parser(mdo_type)?;
    build_metadata_object(main_xml, predefined_xml, &parser)
}

/// One discovered metadata object's *structure*: its kind and the disk paths of
/// its composing files (the main `<Name>.xml` and an optional `Ext/Predefined.xml`),
/// without reading or parsing any content. The `name` is the file/dir stem, which
/// the designer export keeps equal to the object's `<Name>` (the same invariant the
/// directory loaders rely on when locating `<name>.xml` inside a `<name>/` dir).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredMdo {
    pub mdo_type: MdoType,
    pub name: String,
    pub main: PathBuf,
    pub predefined: Option<PathBuf>,
}

/// Designer folder names for the content-parsed MetadataObject kinds that use the
/// `<name>/` dir + sibling `<name>.xml` + optional `Ext/Predefined.xml` layout.
const METADATA_OBJECT_DIRS: &[(&str, MdoType)] = &[
    ("Catalogs", MdoType::Catalog),
    ("Documents", MdoType::Document),
    ("BusinessProcesses", MdoType::BusinessProcess),
    ("Tasks", MdoType::Task),
    ("ExchangePlans", MdoType::ExchangePlan),
    ("ChartsOfCharacteristicTypes", MdoType::ChartOfCharacteristicTypes),
    ("ChartsOfCalculationTypes", MdoType::ChartOfCalculationTypes),
    ("ChartsOfAccounts", MdoType::ChartOfAccounts),
    ("DataProcessors", MdoType::DataProcessor),
    ("Reports", MdoType::Report),
];

/// Designer folder names for the content-parsed kinds stored as a loose
/// `<name>.xml` per object (no dir, no predefined sidecar).
const SIMPLE_XML_DIRS: &[(&str, MdoType)] =
    &[("Enums", MdoType::Enum), ("Constants", MdoType::Constant)];

/// Walk one config root and list its content-parsed MetadataObject-family MDOs by
/// structure only (kind + composing-file paths), reusing the same per-kind layout
/// rules as the directory loaders without reading content. This backs the per-MDO
/// Salsa substrate: each composing file becomes a versioned VFS input, and parsing
/// happens lazily in the per-MDO query. Kinds with their own shapes (registers,
/// roles, subsystems, services, name-only objects) are out of scope here.
pub fn discover_metadata_structure(root: &Path, tree: &dyn DirTree) -> Vec<DiscoveredMdo> {
    let mut out = Vec::new();
    for (subdir, mdo_type) in METADATA_OBJECT_DIRS {
        discover_dir_with_predefined(
            tree,
            &collection_dir(tree, root, subdir),
            *mdo_type,
            &mut out,
        );
    }
    for (subdir, mdo_type) in SIMPLE_XML_DIRS {
        for (name, main) in loose_xml_children(tree, &collection_dir(tree, root, subdir)) {
            out.push(DiscoveredMdo { mdo_type: *mdo_type, name, main, predefined: None });
        }
    }
    // `read_dir` yields entries in an unspecified order, so sort to a stable key:
    // a structure listing built from this must compare equal across calls when the
    // filesystem is unchanged, otherwise every watch event would needlessly re-set
    // the listing input.
    out.sort_by(|a, b| {
        (a.mdo_type as u32, a.name.fold_lower(), &a.main).cmp(&(
            b.mdo_type as u32,
            b.name.fold_lower(),
            &b.main,
        ))
    });
    out
}

fn discover_dir_with_predefined(
    tree: &dyn DirTree,
    dir: &Path,
    mdo_type: MdoType,
    out: &mut Vec<DiscoveredMdo>,
) {
    let entries = tree.entries(dir);

    let dir_names: std::collections::HashSet<String> = entries
        .iter()
        .filter(|entry| entry.is_dir())
        .filter_map(|entry| entry.path.file_name()?.to_str().map(|s| s.to_string()))
        .collect();

    for entry in &entries {
        let path = &entry.path;
        if entry.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(main) = probe_sibling_xml(tree, dir, name) else { continue };
            let predefined =
                probe_ext_child(tree, path, bsl_conventions::ConventionalName::PredefinedXml);
            out.push(DiscoveredMdo { mdo_type, name: name.to_string(), main, predefined });
        } else if bsl_conventions::has_extension(path, bsl_conventions::XML_EXTENSION) {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if dir_names.contains(stem) {
                continue;
            }
            out.push(DiscoveredMdo {
                mdo_type,
                name: stem.to_string(),
                main: path.clone(),
                predefined: None,
            });
        }
    }
}

/// The `<name>.xml` files directly under a collection directory, each with the
/// stem that names its object. The shape every loose-XML family shares.
fn loose_xml_children(tree: &dyn DirTree, dir: &Path) -> Vec<(String, PathBuf)> {
    tree.entries(dir)
        .into_iter()
        .filter(|entry| {
            entry.is_file()
                && bsl_conventions::has_extension(&entry.path, bsl_conventions::XML_EXTENSION)
        })
        .filter_map(|entry| {
            let name = entry.path.file_stem()?.to_str()?.to_string();
            Some((name, entry.path))
        })
        .collect()
}

/// Designer folder names for the register kinds, each stored as a loose
/// `<name>.xml` (no dir, no predefined) like enums/constants.
const REGISTER_DIRS: &[(&str, MdoType)] = &[
    ("InformationRegisters", MdoType::InformationRegister),
    ("AccumulationRegisters", MdoType::AccumulationRegister),
    ("AccountingRegisters", MdoType::AccountingRegister),
    ("CalculationRegisters", MdoType::CalculationRegister),
];

/// Walk one config root and list its registers by structure only (kind + main XML
/// path), the register counterpart of [`discover_metadata_structure`]. Registers
/// are a separate type (`Register`, not `MetadataObject`) parsed by
/// [`parse_register_from_text`], so they get their own discovery + parse query
/// while sharing the per-config-root structure listing and `config_index`.
pub fn discover_register_structure(root: &Path, tree: &dyn DirTree) -> Vec<DiscoveredMdo> {
    let mut out = Vec::new();
    for (subdir, mdo_type) in REGISTER_DIRS {
        for (name, main) in loose_xml_children(tree, &collection_dir(tree, root, subdir)) {
            out.push(DiscoveredMdo { mdo_type: *mdo_type, name, main, predefined: None });
        }
    }
    out.sort_by(|a, b| {
        (a.mdo_type as u32, a.name.fold_lower(), &a.main).cmp(&(
            b.mdo_type as u32,
            b.name.fold_lower(),
            &b.main,
        ))
    });
    out
}

/// The XML parser for a register kind, or `None` for non-register kinds.
fn register_parser(mdo_type: MdoType) -> Option<fn(&str) -> Result<crate::register::Register>> {
    use xml_parser::*;
    Some(match mdo_type {
        MdoType::InformationRegister => parse_information_register_xml,
        MdoType::AccumulationRegister => parse_accumulation_register_xml,
        MdoType::AccountingRegister => parse_accounting_register_xml,
        MdoType::CalculationRegister => parse_calculation_register_xml,
        _ => return None,
    })
}

/// Parse one register of `mdo_type` from its main XML text. The register
/// counterpart of [`parse_metadata_object_from_texts`]; the per-register Salsa
/// query calls it after reading the text through the versioned VFS. Returns `None`
/// for non-register kinds.
pub fn parse_register_from_text(
    mdo_type: MdoType,
    main_xml: &str,
) -> Option<crate::register::Register> {
    register_parser(mdo_type)?(main_xml).ok()
}

/// One discovered defined type in a config root's *structure* listing: its name
/// and the main XML path. Defined types are global (keyed by name, no `MdoType`,
/// no predefined sidecar), so they get a dedicated discovery struct rather than
/// riding [`DiscoveredMdo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDefinedType {
    pub name: String,
    pub main: PathBuf,
}

/// One discovered event subscription in a config root's *structure* listing: its
/// name and the main XML path. Event subscriptions are flat loose XML files under
/// `EventSubscriptions/`, so they get a dedicated one-file discovery struct rather
/// than riding [`DiscoveredMdo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEventSubscription {
    pub name: String,
    pub main: PathBuf,
}

/// One discovered scheduled job in a config root's *structure* listing: its name
/// and its main XML path. Scheduled jobs are stored as loose `<name>.xml` files under
/// `ScheduledJobs/`, so they get their own discovery + parse query while sharing the
/// per-config-root structure listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredScheduledJob {
    pub name: String,
    pub main: PathBuf,
}

/// One discovered role in a config root's *structure* listing: its name, main XML
/// path, and optional `Ext/Rights.xml` sidecar. Roles are stored as loose
/// `Roles/<name>.xml` files, with the rights sidecar under a same-named directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRole {
    pub name: String,
    pub main: PathBuf,
    pub rights: Option<PathBuf>,
}

/// Walk one config root and list its defined types by structure only (name + main
/// XML path), the defined-type counterpart of [`discover_register_structure`].
/// Defined types are stored as loose `<name>.xml` under `DefinedTypes/` and parsed
/// by [`parse_defined_type_from_text`], so they get their own discovery + parse
/// query while sharing the per-config-root structure listing.
pub fn discover_defined_type_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredDefinedType> {
    let dir = collection_dir(tree, root, "DefinedTypes");
    let mut out: Vec<DiscoveredDefinedType> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| DiscoveredDefinedType { name, main })
        .collect();
    // Stable order so a structure listing built from this compares equal across
    // watch events on an unchanged filesystem (see `discover_metadata_structure`).
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

/// Walk one config root and list its event subscriptions by structure only (name +
/// main XML path), the event-subscription counterpart of
/// [`discover_defined_type_structure`].
pub fn discover_event_subscription_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredEventSubscription> {
    let dir = collection_dir(tree, root, "EventSubscriptions");
    let mut out: Vec<DiscoveredEventSubscription> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| DiscoveredEventSubscription { name, main })
        .collect();
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

/// Walk one config root and list its scheduled jobs by structure only (name + main
/// XML path), the scheduled-job counterpart of
/// [`discover_event_subscription_structure`].
pub fn discover_scheduled_job_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredScheduledJob> {
    let dir = collection_dir(tree, root, "ScheduledJobs");
    let mut out: Vec<DiscoveredScheduledJob> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| DiscoveredScheduledJob { name, main })
        .collect();
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

pub fn discover_subsystem_structure(root: &Path, tree: &dyn DirTree) -> Vec<DiscoveredSubsystem> {
    let mut out = Vec::new();
    collect_subsystem_structure(tree, &collection_dir(tree, root, "Subsystems"), &mut out);
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

fn collect_subsystem_structure(tree: &dyn DirTree, dir: &Path, out: &mut Vec<DiscoveredSubsystem>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in tree.entries(&dir) {
            let path = entry.path;
            if entry.kind == bsl_conventions::EntryKind::File
                && bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                out.push(DiscoveredSubsystem { name: stem.to_string(), main: path });
            } else if entry.kind == bsl_conventions::EntryKind::Dir {
                stack.push(collection_dir(tree, &path, "Subsystems"));
            }
        }
    }
}

/// One discovered common module in a config root's *structure* listing: its name,
/// its metadata XML path, and the path of its `Ext/Module.bsl` if present. Common
/// modules carry only metadata (flags + name), so they get a dedicated discovery
/// struct rather than riding [`DiscoveredMdo`]; `module_file` backs a reverse
/// "which common module owns this `.bsl`" lookup that the metadata-XML identity
/// alone cannot answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCommonModule {
    pub name: String,
    pub main: PathBuf,
    pub module_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSubsystem {
    pub name: String,
    pub main: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHTTPService {
    pub name: String,
    pub main: PathBuf,
    pub module_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredWebService {
    pub name: String,
    pub main: PathBuf,
    pub module_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredIntegrationService {
    pub name: String,
    pub main: PathBuf,
    pub module_file: Option<PathBuf>,
}

/// Walk one config root and list its common modules by structure only (name + main
/// XML path + `Ext/Module.bsl` path). Common modules use the `<name>/` dir + sibling
/// `<name>.xml` layout (the module source lives at `<name>/Ext/Module.bsl`), parsed
/// by [`parse_common_module_from_text`], so they get their own discovery + parse
/// query while sharing the per-config-root structure listing.
///
/// The XML names the module, exactly as it does for the services below; the
/// directory only accompanies a body. Keying on the directory instead would lose
/// a protected module — whose directory holds nothing a scan collects — to any
/// tree source that knows files rather than directories.
pub fn discover_common_module_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredCommonModule> {
    let dir = collection_dir(tree, root, "CommonModules");
    let mut out: Vec<DiscoveredCommonModule> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| {
            let module_file =
                probe_ext_child(tree, &dir.join(&name), bsl_conventions::ConventionalName::Module);
            DiscoveredCommonModule { name, main, module_file }
        })
        .collect();
    // Stable order so a structure listing built from this compares equal across
    // watch events on an unchanged filesystem (see `discover_metadata_structure`).
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

pub fn discover_http_service_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredHTTPService> {
    let dir = collection_dir(tree, root, "HTTPServices");
    let mut out: Vec<DiscoveredHTTPService> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| {
            let module_file =
                probe_ext_child(tree, &dir.join(&name), bsl_conventions::ConventionalName::Module);
            DiscoveredHTTPService { name, main, module_file }
        })
        .collect();

    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

pub fn discover_web_service_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredWebService> {
    let dir = collection_dir(tree, root, "WebServices");
    let mut out: Vec<DiscoveredWebService> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| {
            let module_file =
                probe_ext_child(tree, &dir.join(&name), bsl_conventions::ConventionalName::Module);
            DiscoveredWebService { name, main, module_file }
        })
        .collect();

    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

pub fn discover_integration_service_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredIntegrationService> {
    let dir = collection_dir(tree, root, "IntegrationServices");
    let mut out: Vec<DiscoveredIntegrationService> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| {
            let module_file =
                probe_ext_child(tree, &dir.join(&name), bsl_conventions::ConventionalName::Module);
            DiscoveredIntegrationService { name, main, module_file }
        })
        .collect();

    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

/// Parse one common module from its main XML text. The common-module counterpart of
/// [`parse_defined_type_from_text`]; the per-common-module Salsa query calls it after
/// reading the text through the versioned VFS. Only metadata (flags + name) is read;
/// the module body is resolved separately through the symbol tree.
/// Parse one common attribute from its main XML text. `None` when the text is not a readable
/// common attribute; callers treat that as an attribute of unknown name and composition.
pub fn parse_common_attribute_from_text(
    main_xml: &str,
) -> Option<crate::common_attribute::CommonAttribute> {
    match xml_parser::parse_common_attribute_xml(main_xml) {
        Ok(attribute) => Some(attribute),
        Err(error) => {
            tracing::warn!(%error, "common attribute XML does not parse");
            None
        }
    }
}

/// One discovered common attribute in a config root's *structure* listing: its name (file
/// stem) and main XML path. Common attributes are flat loose XML under `CommonAttributes/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCommonAttribute {
    pub name: String,
    pub main: PathBuf,
}

/// Walk one config root and list its common attributes by structure only, the
/// common-attribute counterpart of [`discover_event_subscription_structure`].
pub fn discover_common_attribute_structure(
    root: &Path,
    tree: &dyn DirTree,
) -> Vec<DiscoveredCommonAttribute> {
    let dir = collection_dir(tree, root, "CommonAttributes");
    let mut out: Vec<DiscoveredCommonAttribute> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| DiscoveredCommonAttribute { name, main })
        .collect();
    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

pub fn parse_common_module_from_text(main_xml: &str) -> Option<crate::common_module::CommonModule> {
    xml_parser::parse_common_module_xml(main_xml).ok()
}

/// Parse one defined type from its main XML text. The defined-type counterpart of
/// [`parse_register_from_text`]; the per-defined-type Salsa query calls it after
/// reading the text through the versioned VFS.
pub fn parse_defined_type_from_text(main_xml: &str) -> Option<crate::defined_type::DefinedType> {
    xml_parser::parse_defined_type_xml(main_xml).ok()
}

/// Parse one event subscription from its main XML text. The event-subscription
/// counterpart of [`parse_defined_type_from_text`]; the per-event-subscription Salsa
/// query calls it after reading the text through the versioned VFS.
pub fn parse_event_subscription_from_text(
    main_xml: &str,
) -> Option<crate::event_subscription::EventSubscription> {
    xml_parser::parse_event_subscription_xml(main_xml).ok()
}

/// Parse one scheduled job from its main XML text. The scheduled-job counterpart of
/// [`parse_event_subscription_from_text`]; the per-scheduled-job Salsa query calls it
/// after reading the text through the versioned VFS.
pub fn parse_scheduled_job_from_text(main_xml: &str) -> Option<crate::scheduled_job::ScheduledJob> {
    xml_parser::parse_scheduled_job_xml(main_xml).ok()
}

pub fn parse_subsystem_from_text(main_xml: &str) -> Option<crate::subsystem::Subsystem> {
    xml_parser::parse_subsystem_xml(main_xml).ok()
}

pub fn parse_http_service_from_text(
    main_xml: &str,
    name: &str,
) -> Option<crate::http_service::HTTPService> {
    xml_parser::parse_http_service_xml(main_xml, name).ok()
}

pub fn parse_web_service_from_text(
    main_xml: &str,
    name: &str,
) -> Option<crate::web_service::WebService> {
    xml_parser::parse_web_service_xml(main_xml, name).ok()
}

pub fn parse_integration_service_from_text(
    main_xml: &str,
    name: &str,
) -> Option<crate::integration_service::IntegrationService> {
    xml_parser::parse_integration_service_xml(main_xml, name).ok()
}

/// Parse one role from its main XML text plus an optional rights XML text. The role
/// counterpart of [`parse_scheduled_job_from_text`]; the per-role Salsa query calls it
/// after reading the main `<name>.xml` and optional `Ext/Rights.xml` through the
/// versioned VFS.
pub fn parse_role_from_texts(
    main_xml: &str,
    rights_xml: Option<&str>,
) -> Option<crate::role::Role> {
    let role = xml_parser::parse_role_xml(main_xml).ok()?;

    match rights_xml.and_then(|rights_xml| xml_parser::parse_rights_xml(rights_xml).ok()) {
        Some(rights_data) => {
            Some(crate::role::Role::with_data(*role.uuid(), role.name().to_string(), rights_data))
        }
        None => Some(role),
    }
}

/// Walk one config root and list its roles by structure only (name + main XML path +
/// optional rights sidecar), the role counterpart of
/// [`discover_scheduled_job_structure`]. Roles are stored as loose `Roles/<name>.xml`
/// files, with optional rights under `Roles/<name>/Ext/Rights.xml`.
pub fn discover_role_structure(root: &Path, tree: &dyn DirTree) -> Vec<DiscoveredRole> {
    let dir = collection_dir(tree, root, "Roles");
    let mut out: Vec<DiscoveredRole> = loose_xml_children(tree, &dir)
        .into_iter()
        .map(|(name, main)| {
            let rights = probe_ext_child(
                tree,
                &dir.join(&name),
                bsl_conventions::ConventionalName::RightsXml,
            )
            .filter(|p| tree.kind_of(p) == Some(bsl_conventions::EntryKind::File));
            DiscoveredRole { name, main, rights }
        })
        .collect();

    out.sort_by(|a, b| (a.name.fold_lower(), &a.main).cmp(&(b.name.fold_lower(), &b.main)));
    out
}

fn load_enums_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            xml_parser::parse_enum_xml(&xml).ok()
        })
        .collect()
}

fn load_constants_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<MetadataObject> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            xml_parser::parse_constant_xml(&xml).ok()
        })
        .collect()
}

fn load_registers_parallel<F>(
    scope: &ScopedFs<'_>,
    dir: &Path,
    parser: F,
) -> Vec<crate::register::Register>
where
    F: Fn(&str) -> Result<crate::register::Register> + Sync,
{
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            parser(&xml).ok()
        })
        .collect()
}

fn load_information_registers_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::register::Register> {
    load_registers_parallel(scope, dir, xml_parser::parse_information_register_xml)
}

fn load_accumulation_registers_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::register::Register> {
    load_registers_parallel(scope, dir, xml_parser::parse_accumulation_register_xml)
}

fn load_accounting_registers_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::register::Register> {
    load_registers_parallel(scope, dir, xml_parser::parse_accounting_register_xml)
}

fn load_calculation_registers_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::register::Register> {
    load_registers_parallel(scope, dir, xml_parser::parse_calculation_register_xml)
}

fn load_event_subscriptions_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::event_subscription::EventSubscription> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            xml_parser::parse_event_subscription_xml(&xml).ok()
        })
        .collect()
}

fn load_scheduled_jobs_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::scheduled_job::ScheduledJob> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            xml_parser::parse_scheduled_job_xml(&xml).ok()
        })
        .collect()
}

fn load_roles_parallel(scope: &ScopedFs<'_>, dir: &Path) -> Vec<crate::role::Role> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let name = path.file_stem()?.to_str()?;
            let xml = fs::read_to_string(&path).ok()?;
            let mut role = xml_parser::parse_role_xml(&xml).ok()?;

            let rights_path = probe_ext_child(
                scope,
                &dir.join(name),
                bsl_conventions::ConventionalName::RightsXml,
            );
            if let Some(rights_path) = rights_path {
                if let Ok(rights_xml) = fs::read_to_string(&rights_path) {
                    if let Ok(rights_data) = xml_parser::parse_rights_xml(&rights_xml) {
                        role = crate::role::Role::with_data(
                            *role.uuid(),
                            role.name().to_string(),
                            rights_data,
                        );
                    }
                }
            }

            Some(role)
        })
        .collect()
}

fn load_defined_types_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::defined_type::DefinedType> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !bsl_conventions::has_extension(&path, bsl_conventions::XML_EXTENSION)
            {
                return None;
            }

            let xml = fs::read_to_string(&path).ok()?;
            xml_parser::parse_defined_type_xml(&xml).ok()
        })
        .collect()
}

fn load_http_services_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::http_service::HTTPService> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let service_dir = entry.path();
            if !service_dir.is_dir() {
                return None;
            }

            let name = service_dir.file_name()?.to_str()?;
            let xml_path = probe_sibling_xml(scope, dir, name)?;

            let xml = fs::read_to_string(&xml_path).ok()?;
            let mut service = xml_parser::parse_http_service_xml(&xml, name).ok()?;
            if let Some(found) =
                probe_ext_child(scope, &service_dir, bsl_conventions::ConventionalName::Module)
            {
                let suffix = found
                    .strip_prefix(&service_dir)
                    .unwrap_or(found.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let collection = dir.file_name().and_then(|n| n.to_str()).unwrap_or("HTTPServices");
                service.set_uri(format!("{}/{}/{}", collection, name, suffix));
            }
            Some(service)
        })
        .collect()
}

fn load_web_services_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::web_service::WebService> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let service_dir = entry.path();
            if !service_dir.is_dir() {
                return None;
            }

            let name = service_dir.file_name()?.to_str()?;
            let xml_path = probe_sibling_xml(scope, dir, name)?;

            let xml = fs::read_to_string(&xml_path).ok()?;
            let mut service = xml_parser::parse_web_service_xml(&xml, name).ok()?;
            if let Some(found) =
                probe_ext_child(scope, &service_dir, bsl_conventions::ConventionalName::Module)
            {
                let suffix = found
                    .strip_prefix(&service_dir)
                    .unwrap_or(found.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let collection = dir.file_name().and_then(|n| n.to_str()).unwrap_or("WebServices");
                service.set_uri(format!("{}/{}/{}", collection, name, suffix));
            }
            Some(service)
        })
        .collect()
}

fn load_integration_services_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
) -> Vec<crate::integration_service::IntegrationService> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let service_dir = entry.path();
            if !service_dir.is_dir() {
                return None;
            }

            let name = service_dir.file_name()?.to_str()?;
            let xml_path = probe_sibling_xml(scope, dir, name)?;

            let xml = fs::read_to_string(&xml_path).ok()?;
            xml_parser::parse_integration_service_xml(&xml, name).ok()
        })
        .collect()
}

fn load_simple_metadata_objects_parallel(
    scope: &ScopedFs<'_>,
    dir: &Path,
    mdo_type: MdoType,
) -> Vec<MetadataObject> {
    if !dir.exists() {
        return Vec::new();
    }

    let entries: Vec<_> = match scope.read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
        Err(_) => return Vec::new(),
    };

    entries
        .into_par_iter()
        .filter_map(|entry| {
            let obj_dir = entry.path();
            if !obj_dir.is_dir() {
                return None;
            }

            let name = obj_dir.file_name()?.to_str()?;
            probe_sibling_xml(scope, dir, name)?;

            Some(MetadataObject::new(mdo_type, name))
        })
        .collect()
}

/// Does this volume keep two names that differ only in case apart?
///
/// A case-insensitive volume (APFS and NTFS in their default setup) answers a
/// lookup for any spelling, so `Alpha` and `alpha` are one entry there and a
/// fixture cannot lay both down; the exact probe of a constructed path hits
/// whatever the spelling, and the path handed back carries the CONSTRUCTED
/// spelling instead of the one on disk. Both facts are observable, so a test
/// asks the volume rather than the target OS: a case-sensitive volume mounted
/// on macOS or Windows then still gets the full assertion.
#[cfg(test)]
fn fs_keeps_case_distinct(dir: &Path) -> bool {
    let probe = dir.join("bsl_case_distinction_probe");
    if fs::write(&probe, "").is_err() {
        return false;
    }
    let distinct = fs::metadata(dir.join("BSL_CASE_DISTINCTION_PROBE")).is_err();
    let _ = fs::remove_file(&probe);
    distinct
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Module;

    /// Both polarities on purpose: a hop that never happens leaves the deadlock
    /// in place, and one that always happens pays a thread per load on every
    /// ordinary path.
    #[test]
    fn the_fan_out_leaves_the_calling_thread_only_under_the_guard() {
        let caller = std::thread::current().id();

        assert_eq!(
            off_exclusive_pool(|| std::thread::current().id()),
            caller,
            "an unguarded load must stay on the calling thread"
        );

        let _guard = stdx::par_guard::enter_no_nested_parallelism();
        assert_ne!(
            off_exclusive_pool(|| std::thread::current().id()),
            caller,
            "a guarded load must not fan out on the calling pool"
        );
    }

    /// The hop must not change what is loaded. Compared against the same
    /// directory read without the guard rather than against a fixed count, so
    /// the check keeps its meaning as the fixture grows.
    #[test]
    fn the_guarded_load_yields_the_same_configuration() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        let direct = load_from_directory(path).unwrap();
        let off_pool = {
            let _guard = stdx::par_guard::enter_no_nested_parallelism();
            load_from_directory(path).unwrap()
        };

        assert!(!direct.metadata_objects().is_empty(), "fixture sanity: the load is not empty");
        assert_eq!(direct.metadata_objects(), off_pool.metadata_objects());
        assert_eq!(direct.common_modules(), off_pool.common_modules());
        assert_eq!(direct.registers(), off_pool.registers());
    }

    #[test]
    fn extension_metadata_loader_preserves_absent_common_module_properties() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/extension_metadata/extension");
        let loaded = load_from_directory(path).unwrap();
        let overlay = loaded.find_common_module("Сервер").expect("borrowed module is loaded");
        assert_eq!(overlay.uri(), Some("CommonModules/Сервер/Ext/Module.bsl"));
        assert!(!overlay.is_protected());

        let mut effective = crate::CommonModule::builder()
            .name("Сервер")
            .server(true)
            .global(true)
            .client_managed_application(true)
            .client_ordinary_application(true)
            .external_connection(true)
            .server_call(true)
            .privileged(true)
            .return_values_reuse(crate::ReturnValueReuse::DontUse)
            .uri(Some("base/CommonModules/Сервер/Ext/Module.bsl"))
            .protected(true)
            .build();
        effective.apply_extension_overlay(overlay);

        assert!(!effective.is_server(), "the extension explicitly disables Server");
        assert!(!effective.is_global(), "the extension explicitly disables Global");
        assert!(effective.is_client_managed_application());
        assert!(effective.is_client_ordinary_application());
        assert!(
            !effective.is_external_connection(),
            "the extension explicitly disables ExternalConnection"
        );
        assert!(!effective.is_server_call(), "the extension explicitly disables ServerCall");
        assert!(effective.is_privileged());
        assert_eq!(effective.return_values_reuse(), crate::ReturnValueReuse::DontUse);
        assert_eq!(effective.uri(), Some("CommonModules/Сервер/Ext/Module.bsl"));
        assert!(!effective.is_protected(), "URI/protected identity stays with the overlay");
    }

    #[test]
    fn extension_metadata_loader_preserves_every_absent_boolean_property() {
        let root = tempfile::tempdir().unwrap();
        let module_dir = root.path().join("CommonModules/Сервер/Ext");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("Module.bsl"), "").unwrap();
        std::fs::write(
            root.path().join("CommonModules/Сервер.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<CommonModule uuid="15500000-0000-0000-0000-000000000502"><Properties>
<Name>Сервер</Name>
</Properties></CommonModule></MetaDataObject>"#,
        )
        .unwrap();

        let loaded = load_from_directory(root.path()).unwrap();
        let overlay = loaded.find_common_module("Сервер").unwrap();
        let mut effective = crate::CommonModule::builder()
            .name("Сервер")
            .server(true)
            .global(true)
            .client_managed_application(true)
            .client_ordinary_application(true)
            .external_connection(true)
            .server_call(true)
            .privileged(true)
            .build();
        effective.apply_extension_overlay(overlay);
        assert!(effective.is_server());
        assert!(effective.is_global());
        assert!(effective.is_client_managed_application());
        assert!(effective.is_client_ordinary_application());
        assert!(effective.is_external_connection());
        assert!(effective.is_server_call());
        assert!(effective.is_privileged());

        let tags = [
            "Server",
            "Global",
            "ClientManagedApplication",
            "ClientOrdinaryApplication",
            "ExternalConnection",
            "ServerCall",
            "Privileged",
        ];
        for omitted in 0..tags.len() {
            let root = tempfile::tempdir().unwrap();
            let module_dir = root.path().join("CommonModules/Сервер/Ext");
            std::fs::create_dir_all(&module_dir).unwrap();
            std::fs::write(module_dir.join("Module.bsl"), "").unwrap();
            let properties = tags
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != omitted)
                .map(|(_, tag)| format!("<{tag}>false</{tag}>"))
                .collect::<String>();
            std::fs::write(
                root.path().join("CommonModules/Сервер.xml"),
                format!(
                    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<CommonModule uuid="15500000-0000-0000-0000-000000000503"><Properties>
<Name>Сервер</Name>{properties}
</Properties></CommonModule></MetaDataObject>"#
                ),
            )
            .unwrap();
            let loaded = load_from_directory(root.path()).unwrap();
            let overlay = loaded.find_common_module("Сервер").unwrap();
            let mut effective = crate::CommonModule::builder()
                .name("Сервер")
                .server(true)
                .global(true)
                .client_managed_application(true)
                .client_ordinary_application(true)
                .external_connection(true)
                .server_call(true)
                .privileged(true)
                .build();
            effective.apply_extension_overlay(overlay);
            let values = [
                effective.is_server(),
                effective.is_global(),
                effective.is_client_managed_application(),
                effective.is_client_ordinary_application(),
                effective.is_external_connection(),
                effective.is_server_call(),
                effective.is_privileged(),
            ];
            for (index, value) in values.into_iter().enumerate() {
                assert_eq!(
                    value,
                    index == omitted,
                    "omitted filesystem bool index {omitted}, checked {index}"
                );
            }
        }
    }

    #[test]
    fn extension_metadata_loader_preserves_every_explicit_false_property() {
        let root = tempfile::tempdir().unwrap();
        let module_dir = root.path().join("CommonModules/Сервер/Ext");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("Module.bsl"), "").unwrap();
        std::fs::write(
            root.path().join("CommonModules/Сервер.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<CommonModule uuid="15500000-0000-0000-0000-000000000500"><Properties>
<Name>Сервер</Name><Server>false</Server><Global>false</Global>
<ClientManagedApplication>false</ClientManagedApplication>
<ClientOrdinaryApplication>false</ClientOrdinaryApplication>
<ExternalConnection>false</ExternalConnection><ServerCall>false</ServerCall>
<Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse>
</Properties></CommonModule></MetaDataObject>"#,
        )
        .unwrap();

        let loaded = load_from_directory(root.path()).unwrap();
        let overlay = loaded.find_common_module("Сервер").unwrap();
        let mut effective = crate::CommonModule::builder()
            .name("Сервер")
            .server(true)
            .global(true)
            .client_managed_application(true)
            .client_ordinary_application(true)
            .external_connection(true)
            .server_call(true)
            .privileged(true)
            .return_values_reuse(crate::ReturnValueReuse::DuringRequest)
            .build();
        effective.apply_extension_overlay(overlay);
        assert!(!effective.is_server());
        assert!(!effective.is_global());
        assert!(!effective.is_client_managed_application());
        assert!(!effective.is_client_ordinary_application());
        assert!(!effective.is_external_connection());
        assert!(!effective.is_server_call());
        assert!(!effective.is_privileged());
        assert_eq!(effective.return_values_reuse(), crate::ReturnValueReuse::DontUse);
    }

    #[test]
    fn extension_metadata_loader_preserves_explicit_unknown_return_values_reuse() {
        let root = tempfile::tempdir().unwrap();
        let module_dir = root.path().join("CommonModules/Сервер/Ext");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("Module.bsl"), "").unwrap();
        std::fs::write(
            root.path().join("CommonModules/Сервер.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<CommonModule uuid="15500000-0000-0000-0000-000000000501"><Properties>
<Name>Сервер</Name><ReturnValuesReuse/>
</Properties></CommonModule></MetaDataObject>"#,
        )
        .unwrap();

        let loaded = load_from_directory(root.path()).unwrap();
        let overlay = loaded.find_common_module("Сервер").unwrap();
        assert_eq!(overlay.return_values_reuse(), crate::ReturnValueReuse::Unknown);
        assert!(
            serde_json::to_string(overlay).unwrap().contains("returnValuesReuse"),
            "the loader must preserve explicit Unknown rather than materialize absence"
        );
        let mut effective = crate::CommonModule::builder()
            .name("Сервер")
            .return_values_reuse(crate::ReturnValueReuse::DontUse)
            .build();
        effective.apply_extension_overlay(overlay);
        assert_eq!(effective.return_values_reuse(), crate::ReturnValueReuse::Unknown);
    }

    #[test]
    fn scoped_directory_load_omits_an_excluded_metadata_family() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer"));
        let full = load_from_directory(path).unwrap();
        assert!(
            full.find_metadata_object(MdoType::Catalog, "Справочник1").is_some(),
            "fixture sanity: the catalog must exist before applying the exclusion"
        );

        let scoped =
            load_from_directory_scoped(path, &ExcludedPaths::new([path.join("Catalogs")])).unwrap();
        assert!(
            scoped.find_metadata_object(MdoType::Catalog, "Справочник1").is_none(),
            "the public loader read a metadata object through an excluded family directory"
        );
        assert!(
            !scoped.common_modules().is_empty(),
            "positive control: unrelated metadata must still be loaded"
        );
    }

    #[test]
    fn scoped_directory_load_omits_excluded_common_attributes() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/common_attributes"));
        let names = |config: &Configuration| {
            let catalog = config.find_metadata_object(MdoType::Catalog, "Справочник1").unwrap();
            let mut names: Vec<_> =
                catalog.common_attributes.iter().map(|f| f.name.clone()).collect();
            names.sort();
            names
        };
        assert_eq!(
            names(&load_from_directory(path).unwrap()),
            ["ОбластьДанныхОсновныеДанные", "ОбщийКомментарий", "Организация"],
            "fixture sanity: every common attribute reaches the catalog before any exclusion"
        );

        let without_family =
            load_from_directory_scoped(path, &ExcludedPaths::new([path.join("CommonAttributes")]))
                .unwrap();
        assert!(
            names(&without_family).is_empty(),
            "a common attribute was read through an excluded family directory"
        );

        let without_file = load_from_directory_scoped(
            path,
            &ExcludedPaths::new([path.join("CommonAttributes").join("Организация.xml")]),
        )
        .unwrap();
        assert_eq!(
            names(&without_file),
            ["ОбластьДанныхОсновныеДанные", "ОбщийКомментарий"],
            "an excluded common attribute file was read, or its siblings were lost"
        );
    }

    #[test]
    fn parse_metadata_object_from_texts_matches_directory_load() {
        let xml = include_str!("../fixtures/designer/Catalogs/Справочник1.xml");
        let parsed = parse_metadata_object_from_texts(MdoType::Catalog, xml, None)
            .expect("catalog parsed from text");
        assert_eq!(parsed.name, "Справочник1");

        // The per-MDO text parse must equal what the directory loader yields for
        // the same object (this fixture catalog has no Predefined sidecar).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();
        let from_dir = config
            .find_metadata_object(MdoType::Catalog, "Справочник1")
            .expect("catalog from directory load");
        assert_eq!(&parsed, from_dir);
    }

    #[test]
    fn discover_metadata_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        // The structure walk must find exactly the content-parsed MetadataObject
        // family that the full directory load yields — pinning the discovery rules
        // against drift from the loaders they mirror.
        let kinds: BTreeSet<MdoType> =
            METADATA_OBJECT_DIRS.iter().chain(SIMPLE_XML_DIRS.iter()).map(|(_, k)| *k).collect();

        let discovered: BTreeSet<(MdoType, String)> =
            discover_metadata_structure(Path::new(path), &bsl_conventions::RealFs)
                .into_iter()
                .map(|d| (d.mdo_type, d.name))
                .collect();

        let config = load_from_directory(path).unwrap();
        let from_load: BTreeSet<(MdoType, String)> = config
            .metadata_objects()
            .iter()
            .filter(|o| kinds.contains(&o.mdo_type))
            .map(|o| (o.mdo_type, o.name.clone()))
            .collect();

        assert_eq!(discovered, from_load, "discovery must match the directory loader");
        assert!(
            discovered.contains(&(MdoType::Catalog, "Справочник1".to_string())),
            "fixture sanity: Справочник1 catalog is present"
        );
    }

    #[test]
    fn discover_subsystem_structure_is_deterministic_and_matches_directory_load_set() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_subsystems_{}_{}",
            std::process::id(),
            line!()
        ));
        let subsystems_dir = collection_dir(&bsl_conventions::RealFs, &root, "Subsystems");
        let parent_dir = subsystems_dir.join("Группа").join("Subsystems");
        std::fs::create_dir_all(&parent_dir).unwrap();

        let subsystem_xml = |name: &str, child: Option<&str>| {
            let child_block = child
                .map(|child| format!("<ChildObjects><Subsystem>{child}</Subsystem></ChildObjects>"))
                .unwrap_or_default();
            format!(
                concat!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                    "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\" version=\"2.10\">",
                    "  <Subsystem uuid=\"00000000-0000-0000-0000-000000000041\">",
                    "    <Properties><Name>{}</Name></Properties>",
                    "    {}",
                    "  </Subsystem>",
                    "</MetaDataObject>"
                ),
                name, child_block,
            )
        };

        std::fs::write(
            subsystems_dir.join("Группа.xml"),
            subsystem_xml("Группа", Some("Дочерняя")),
        )
        .unwrap();
        std::fs::write(parent_dir.join("Дочерняя.xml"), subsystem_xml("Дочерняя", None)).unwrap();
        std::fs::write(subsystems_dir.join("Бета.xml"), subsystem_xml("Бета", None)).unwrap();
        std::fs::write(subsystems_dir.join("Альфа.xml"), subsystem_xml("Альфа", None)).unwrap();

        let discovered = discover_subsystem_structure(&root, &bsl_conventions::RealFs);
        let discovered_names: Vec<_> =
            discovered.iter().map(|subsystem| subsystem.name.clone()).collect();

        assert_eq!(
            discovered_names,
            ["Альфа", "Бета", "Группа", "Дочерняя"],
            "typed subsystem discovery should be stable across recursive directory walks"
        );

        let config = load_from_directory(&root).unwrap();
        let discovered_set: std::collections::BTreeSet<_> =
            discovered.iter().map(|subsystem| subsystem.name.clone()).collect();
        let loaded_set: std::collections::BTreeSet<_> =
            config.subsystems().iter().map(|subsystem| subsystem.name().to_string()).collect();
        assert_eq!(discovered_set, loaded_set, "typed discovery should keep load parity by set");

        let parent = discovered
            .iter()
            .find(|subsystem| subsystem.name == "Группа")
            .expect("parent subsystem");
        let child = discovered
            .iter()
            .find(|subsystem| subsystem.name == "Дочерняя")
            .expect("child subsystem");
        assert!(parent.main.ends_with("Subsystems/Группа.xml"));
        assert!(child.main.ends_with("Subsystems/Группа/Subsystems/Дочерняя.xml"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn discover_subsystem_structure_ignores_symlink_directory_cycles() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_subsystems_symlink_{}_{}",
            std::process::id(),
            line!()
        ));
        let subsystems_dir = collection_dir(&bsl_conventions::RealFs, &root, "Subsystems");
        let child_dir = subsystems_dir.join("Группа").join("Subsystems");
        std::fs::create_dir_all(&child_dir).unwrap();

        let subsystem_xml = |name: &str| {
            format!(
                concat!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                    "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\" version=\"2.10\">",
                    "  <Subsystem uuid=\"00000000-0000-0000-0000-000000000041\">",
                    "    <Properties><Name>{}</Name></Properties>",
                    "  </Subsystem>",
                    "</MetaDataObject>"
                ),
                name,
            )
        };

        std::fs::write(subsystems_dir.join("Альфа.xml"), subsystem_xml("Альфа")).unwrap();
        std::fs::write(child_dir.join("Дочерняя.xml"), subsystem_xml("Дочерняя")).unwrap();
        std::os::unix::fs::symlink(
            &subsystems_dir,
            collection_dir(&bsl_conventions::RealFs, &child_dir, "Subsystems"),
        )
        .unwrap();

        let discovered = discover_subsystem_structure(&root, &bsl_conventions::RealFs);
        let discovered_names: Vec<_> =
            discovered.into_iter().map(|subsystem| subsystem.name).collect();

        assert_eq!(discovered_names, ["Альфа", "Дочерняя"]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_register_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        let discovered: BTreeSet<(MdoType, String)> =
            discover_register_structure(Path::new(path), &bsl_conventions::RealFs)
                .into_iter()
                .map(|d| (d.mdo_type, d.name))
                .collect();

        let config = load_from_directory(path).unwrap();
        let from_load: BTreeSet<(MdoType, String)> =
            config.registers().iter().map(|r| (r.mdo_type(), r.name().to_string())).collect();

        assert_eq!(discovered, from_load, "register discovery must match the directory loader");
    }

    #[test]
    fn discover_defined_type_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let root = std::env::temp_dir().join(format!(
            "bsl_discover_dt_{}_{}",
            std::process::id(),
            line!()
        ));
        let dt_dir = collection_dir(&bsl_conventions::RealFs, &root, "DefinedTypes");
        std::fs::create_dir_all(&dt_dir).unwrap();
        let xml = |name: &str| {
            format!(
                concat!(
                    "<MetaDataObject>",
                    "<DefinedType uuid=\"00000000-0000-0000-0000-000000000001\">",
                    "<Properties><Name>{}</Name>",
                    "<Type><Type>xs:boolean</Type></Type>",
                    "</Properties></DefinedType></MetaDataObject>"
                ),
                name
            )
        };
        std::fs::write(dt_dir.join("ОтметкаВремени.xml"), xml("ОтметкаВремени")).unwrap();
        std::fs::write(dt_dir.join("ДенежнаяСумма.xml"), xml("ДенежнаяСумма")).unwrap();

        let discovered: BTreeSet<String> =
            discover_defined_type_structure(&root, &bsl_conventions::RealFs)
                .into_iter()
                .map(|d| d.name)
                .collect();

        let config = load_from_directory(&root).unwrap();
        let from_load: BTreeSet<String> =
            config.defined_types().iter().map(|d| d.name().to_string()).collect();

        assert_eq!(discovered, from_load, "defined-type discovery must match the directory loader",);
        assert!(discovered.contains("ДенежнаяСумма"), "fixture sanity");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_event_subscription_structure_finds_loose_xml_stably() {
        use std::collections::BTreeSet;

        let root = std::env::temp_dir().join(format!(
            "bsl_discover_event_subscription_{}_{}",
            std::process::id(),
            line!()
        ));
        let dir = collection_dir(&bsl_conventions::RealFs, &root, "EventSubscriptions");
        std::fs::create_dir_all(&dir).unwrap();

        let xml = |name: &str, event: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000041">
        <Properties>
            <Name>{name}</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>{event}</Event>
            <Handler>CommonModule.ПодпискиНаСобытия.Обработать</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#
            )
        };

        std::fs::write(dir.join("ПослеЗаписи.xml"), xml("ПослеЗаписи", "AfterWrite")).unwrap();
        std::fs::write(dir.join("ПередЗаписью.xml"), xml("ПередЗаписью", "BeforeWrite")).unwrap();

        let first = discover_event_subscription_structure(&root, &bsl_conventions::RealFs);
        let second = discover_event_subscription_structure(&root, &bsl_conventions::RealFs);

        assert_eq!(first, second, "event-subscription discovery order must be stable");
        assert!(
            first.iter().all(|subscription| subscription.main.starts_with(&dir)),
            "discovery must point at loose EventSubscriptions/*.xml files"
        );

        let discovered: BTreeSet<String> = first.into_iter().map(|d| d.name).collect();
        let config = load_from_directory(&root).unwrap();
        let from_load: BTreeSet<String> = config
            .event_subscriptions()
            .iter()
            .map(|subscription| subscription.name().to_string())
            .collect();

        assert_eq!(
            discovered, from_load,
            "event-subscription discovery must match the directory loader"
        );
        assert!(discovered.contains("ПередЗаписью"), "fixture sanity");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_scheduled_job_structure_finds_loose_xml_stably() {
        use std::collections::BTreeSet;

        let root = std::env::temp_dir().join(format!(
            "bsl_discover_scheduled_job_{}_{}",
            std::process::id(),
            line!()
        ));
        let dir = collection_dir(&bsl_conventions::RealFs, &root, "ScheduledJobs");
        std::fs::create_dir_all(&dir).unwrap();

        let xml = |name: &str, method_name: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ScheduledJob uuid="00000000-0000-0000-0000-000000000042">
        <Properties>
            <Name>{name}</Name>
            <MethodName>{method_name}</MethodName>
            <Predefined>false</Predefined>
            <Use>true</Use>
        </Properties>
    </ScheduledJob>
</MetaDataObject>"#
            )
        };

        std::fs::write(dir.join("ПослеЗаписи.xml"), xml("ПослеЗаписи", "CommonModule.Job.OnWrite"))
            .unwrap();
        std::fs::write(
            dir.join("ПередЗаписью.xml"),
            xml("ПередЗаписью", "CommonModule.Job.BeforeWrite"),
        )
        .unwrap();

        let first = discover_scheduled_job_structure(&root, &bsl_conventions::RealFs);
        let second = discover_scheduled_job_structure(&root, &bsl_conventions::RealFs);

        assert_eq!(first, second, "scheduled-job discovery order must be stable");
        assert!(
            first.iter().all(|job| job.main.starts_with(&dir)),
            "discovery must point at loose ScheduledJobs/*.xml files"
        );

        let discovered: BTreeSet<String> = first.into_iter().map(|d| d.name).collect();
        let config = load_from_directory(&root).unwrap();
        let from_load: BTreeSet<String> =
            config.scheduled_jobs().iter().map(|job| job.name().to_string()).collect();

        assert_eq!(
            discovered, from_load,
            "scheduled-job discovery must match the directory loader"
        );
        assert!(discovered.contains("ПередЗаписью"), "fixture sanity");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_scheduled_job_from_text_uses_xml_parser() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <ScheduledJob uuid="00000000-0000-0000-0000-000000000042">
        <Properties>
            <Name>Планировщик</Name>
            <MethodName>CommonModule.Job.Run</MethodName>
            <Predefined>false</Predefined>
            <Use>true</Use>
        </Properties>
    </ScheduledJob>
</MetaDataObject>"#;

        let job = parse_scheduled_job_from_text(xml).expect("scheduled job should parse");

        assert_eq!(job.name(), "Планировщик");
        assert_eq!(job.method_name(), "CommonModule.Job.Run");
    }

    #[test]
    fn discover_role_structure_finds_main_and_optional_rights_stably() {
        use std::collections::BTreeSet;

        let root = std::env::temp_dir().join(format!(
            "bsl_discover_role_{}_{}",
            std::process::id(),
            line!()
        ));
        let roles_dir = collection_dir(&bsl_conventions::RealFs, &root, "Roles");
        std::fs::create_dir_all(roles_dir.join("Alpha/Ext")).unwrap();
        std::fs::create_dir_all(roles_dir.join("Gamma/Ext")).unwrap();
        let case_twin = fs_keeps_case_distinct(&roles_dir);
        if case_twin {
            std::fs::create_dir_all(roles_dir.join("alpha/Ext")).unwrap();
        }

        let role_xml = |name: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000043">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
            )
        };

        std::fs::write(roles_dir.join("Alpha.xml"), role_xml("Alpha")).unwrap();
        std::fs::write(roles_dir.join("Beta.xml"), role_xml("Beta")).unwrap();
        std::fs::write(roles_dir.join("Gamma.xml"), role_xml("Gamma")).unwrap();
        std::fs::write(roles_dir.join("Alpha/Ext/Rights.xml"), "<Rights/>").unwrap();
        std::fs::write(roles_dir.join("Gamma/Ext/Rights.txt"), "not xml").unwrap();
        if case_twin {
            std::fs::write(roles_dir.join("alpha.xml"), role_xml("alpha")).unwrap();
            std::fs::write(roles_dir.join("alpha/Ext/Rights.txt"), "not xml").unwrap();
        }
        std::fs::write(roles_dir.join("ignored.txt"), "ignored").unwrap();

        let first = discover_role_structure(&root, &bsl_conventions::RealFs);
        let second = discover_role_structure(&root, &bsl_conventions::RealFs);

        assert_eq!(first, second, "role discovery order must be stable");
        assert!(first.iter().all(|role| role.main.starts_with(&roles_dir)));
        assert!(first
            .iter()
            .all(|role| role.main.extension().and_then(|ext| ext.to_str()) == Some("xml")));

        let discovered: BTreeSet<String> = first.iter().map(|d| d.name.clone()).collect();
        assert_eq!(first[0].name, "Alpha");
        assert_eq!(first[0].rights, Some(roles_dir.join("Alpha/Ext/Rights.xml")));
        if case_twin {
            assert_eq!(
                first[1].name, "alpha",
                "roles with the same folded name should sort by main path"
            );
            assert_eq!(first[1].rights, None);
        }
        assert!(discovered.contains("Beta"), "fixture sanity");
        // Gamma is no case twin, so this rides on every volume: an `Ext` child
        // that is not `Rights.xml` is not the rights sidecar.
        let gamma = first.iter().find(|role| role.name == "Gamma").expect("fixture sanity");
        assert_eq!(gamma.rights, None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_role_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let root = std::env::temp_dir().join(format!(
            "bsl_discover_role_load_{}_{}",
            std::process::id(),
            line!()
        ));
        let roles_dir = collection_dir(&bsl_conventions::RealFs, &root, "Roles");
        std::fs::create_dir_all(roles_dir.join("Alpha/Ext")).unwrap();

        let role_xml = |name: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000044">
        <Properties>
            <Name>{name}</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#
            )
        };

        std::fs::write(roles_dir.join("Alpha.xml"), role_xml("Alpha")).unwrap();
        std::fs::write(roles_dir.join("Alpha/Ext/Rights.xml"), "<Rights/>").unwrap();
        std::fs::write(roles_dir.join("Beta.xml"), role_xml("Beta")).unwrap();

        let discovered: BTreeSet<String> = discover_role_structure(&root, &bsl_conventions::RealFs)
            .into_iter()
            .map(|d| d.name)
            .collect();
        let config = load_from_directory(&root).unwrap();
        let from_load: BTreeSet<String> =
            config.roles().iter().map(|role| role.name().to_string()).collect();

        assert_eq!(discovered, from_load, "role discovery must match the directory loader");
        assert!(discovered.contains("Alpha"), "fixture sanity");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_role_from_texts_combines_main_and_rights() {
        let main_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000045">
        <Properties>
            <Name>Alpha</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#;
        let rights_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Rights xmlns="http://v8.1c.ru/8.2/roles" version="2.10">
    <setForNewObjects>true</setForNewObjects>
    <setForAttributesByDefault>false</setForAttributesByDefault>
    <independentRightsOfChildObjects>true</independentRightsOfChildObjects>
    <object>
        <name>Catalog.Контрагенты</name>
        <right>
            <name>Read</name>
            <value>true</value>
            <restrictionByCondition>
                <condition>Контрагенты.Организация = &amp;Организация</condition>
            </restrictionByCondition>
            <restrictionByCondition>
                <condition>Контрагенты.Удален = Ложь</condition>
            </restrictionByCondition>
        </right>
    </object>
</Rights>"#;

        let role = parse_role_from_texts(main_xml, Some(rights_xml)).expect("role should parse");

        assert_eq!(role.name(), "Alpha");
        assert!(role.data().set_for_new_objects());
        assert_eq!(role.objects().len(), 1);
        assert_eq!(role.objects()[0].mdo_type, MdoType::Catalog);
        assert_eq!(role.objects()[0].name, "Контрагенты");
        assert_eq!(
            role.objects()[0].restrictions,
            vec![
                "Контрагенты.Организация = &Организация".to_string(),
                "Контрагенты.Удален = Ложь".to_string(),
            ]
        );
    }

    #[test]
    fn parse_role_from_texts_without_rights_preserves_identity() {
        let main_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000046">
        <Properties>
            <Name>Beta</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#;

        let role = parse_role_from_texts(main_xml, None).expect("role should parse without rights");

        assert_eq!(role.name(), "Beta");
        assert_eq!(role.uuid().to_string(), "00000000-0000-0000-0000-000000000046");
        assert!(!role.data().set_for_new_objects());
        assert!(role.objects().is_empty());
    }

    #[test]
    fn parse_role_from_texts_with_malformed_rights_preserves_main_role() {
        let main_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Role uuid="00000000-0000-0000-0000-000000000047">
        <Properties>
            <Name>Gamma</Name>
            <Synonym/>
            <Comment/>
        </Properties>
    </Role>
</MetaDataObject>"#;

        let role = parse_role_from_texts(main_xml, Some("<Rights>")).expect(
            "malformed optional rights XML must match load_roles_parallel and keep the main role",
        );

        assert_eq!(role.name(), "Gamma");
        assert_eq!(role.uuid().to_string(), "00000000-0000-0000-0000-000000000047");
        assert!(!role.data().set_for_new_objects());
        assert!(role.objects().is_empty());
    }

    #[test]
    fn discover_metadata_structure_order_is_stable() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        // Repeated discovery must yield byte-identical Vecs so a structure listing
        // compares equal across watch events on an unchanged filesystem.
        assert_eq!(
            discover_metadata_structure(Path::new(path), &bsl_conventions::RealFs),
            discover_metadata_structure(Path::new(path), &bsl_conventions::RealFs),
        );
    }

    /// Discovery must not inherit the order a listing happens to arrive in:
    /// `read_dir` promises none, so the same universe fed in two orders has to
    /// discover the same list, and objects whose names fold together have to
    /// sort by their main path. An in-memory tree is what pins this down —
    /// it hands out the order it was given, and it holds the `Alpha`/`alpha`
    /// twin that a case-insensitive volume cannot keep apart.
    #[test]
    fn discovery_order_does_not_follow_the_listing_order() {
        use bsl_conventions::PathSetTree;

        let files = [
            "/ws/HTTPServices/alpha.xml",
            "/ws/HTTPServices/alpha/Ext/Module.bsl",
            "/ws/HTTPServices/Beta.xml",
            "/ws/HTTPServices/Alpha.xml",
            "/ws/WebServices/Beta.xml",
            "/ws/WebServices/alpha.xml",
            "/ws/WebServices/Alpha.xml",
            "/ws/IntegrationServices/alpha.xml",
            "/ws/IntegrationServices/Alpha.xml",
            "/ws/IntegrationServices/Beta.xml",
            "/ws/Roles/Beta.xml",
            "/ws/Roles/alpha.xml",
            "/ws/Roles/Alpha.xml",
            "/ws/Roles/Alpha/Ext/Rights.xml",
        ];
        let forward = PathSetTree::from_files(files.iter().map(PathBuf::from));
        let reversed = PathSetTree::from_files(files.iter().rev().map(PathBuf::from));
        let root = Path::new("/ws");

        // Each collection has its own discovery function with its own sort, so
        // each is pinned in turn: a copy losing the rule stays visible.
        macro_rules! assert_folded_names_sort_by_main {
            ($discover:ident, $collection:literal) => {{
                let discovered = $discover(root, &forward);
                assert_eq!(
                    discovered,
                    $discover(root, &reversed),
                    concat!($collection, " discovery follows the listing order")
                );
                assert_eq!(
                    discovered.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
                    ["Alpha", "alpha", "Beta"],
                    concat!($collection, ": names that fold together sort by main path")
                );
                discovered
            }};
        }

        let services =
            assert_folded_names_sort_by_main!(discover_http_service_structure, "HTTPServices");
        assert_folded_names_sort_by_main!(discover_web_service_structure, "WebServices");
        assert_folded_names_sort_by_main!(
            discover_integration_service_structure,
            "IntegrationServices"
        );
        let roles = assert_folded_names_sort_by_main!(discover_role_structure, "Roles");

        assert_eq!(services[0].main, PathBuf::from("/ws/HTTPServices/Alpha.xml"));
        assert_eq!(services[0].module_file, None);
        assert_eq!(
            services[1].module_file,
            Some(PathBuf::from("/ws/HTTPServices/alpha/Ext/Module.bsl")),
            "the body belongs to the twin that owns the directory"
        );

        assert_eq!(roles[0].rights, Some(PathBuf::from("/ws/Roles/Alpha/Ext/Rights.xml")));
        assert_eq!(roles[1].rights, None);
    }

    #[test]
    fn discover_metadata_structure_attaches_predefined_sidecar() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_predef_{}_{}",
            std::process::id(),
            line!()
        ));
        let cat_dir = collection_dir(&bsl_conventions::RealFs, &root, "Catalogs").join("Товары");
        std::fs::create_dir_all(cat_dir.join("Ext")).unwrap();
        std::fs::write(root.join("Catalogs/Товары.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(cat_dir.join("Ext/Predefined.xml"), "<Predefined/>").unwrap();

        let discovered = discover_metadata_structure(&root, &bsl_conventions::RealFs);
        let tovary = discovered
            .iter()
            .find(|d| d.name == "Товары")
            .expect("Товары discovered from its dir + sibling xml");
        assert_eq!(tovary.mdo_type, MdoType::Catalog);
        assert_eq!(tovary.main, root.join("Catalogs/Товары.xml"));
        assert_eq!(tovary.predefined, Some(cat_dir.join("Ext/Predefined.xml")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_load_from_directory() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();

        assert!(!config.common_modules().is_empty(), "No common modules loaded");
        assert_eq!(config.common_modules().len(), 4, "Expected 4 common modules");

        let global_server = config.find_common_module("ГлобальныйСерверныйМодуль");
        assert!(global_server.is_some(), "ГлобальныйСерверныйМодуль not found");
        let module = global_server.unwrap();
        assert!(module.is_server(), "Should be server module");
        assert!(module.is_global(), "Should be global module");
        assert!(module.uri().is_some(), "Should have URI");
        assert_eq!(module.uri().unwrap(), "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl");

        assert!(!config.registers().is_empty(), "No registers loaded");

        let register = config.find_register("РегистрСведений1");
        if let Some(reg) = register {
            assert!(reg.is_information_register(), "Should be InformationRegister");
            assert_eq!(reg.dimensions().len(), 1, "Should have 1 dimension");
            assert_eq!(reg.dimensions()[0].name(), "Справочник1", "Dimension name should match");
            assert!(
                !reg.dimensions()[0].is_deny_incomplete_values(),
                "DenyIncompleteValues should be false"
            );
        }

        assert!(!config.metadata_objects().is_empty(), "No metadata objects loaded");

        let catalog = config.metadata_objects().iter().find(|obj| {
            obj.mdo_type == crate::metadata_object::MdoType::Catalog && obj.name == "Справочник1"
        });

        if let Some(cat) = catalog {
            assert!(
                cat.attributes.len() >= 3,
                "Expected at least 3 custom attributes in Справочник1"
            );

            assert!(cat.find_attribute("Реквизит1").is_some(), "Expected Реквизит1");
            assert!(cat.find_attribute("Реквизит2").is_some(), "Expected Реквизит2");
            assert!(cat.find_attribute("Реквизит3").is_some(), "Expected Реквизит3");

            let attr1 = cat.find_attribute("Реквизит1").unwrap();
            assert!(
                matches!(attr1.attr_type, crate::metadata_object::AttributeType::String { .. }),
                "Реквизит1 should be String type"
            );

            assert_eq!(cat.tabular_sections.len(), 1, "Expected 1 tabular section");
            let ts = &cat.tabular_sections[0];
            assert_eq!(ts.name(), "ТабличнаяЧасть1");
        }
    }

    #[test]
    fn loads_integration_service_with_receive_handler() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();

        let service = config
            .find_integration_service("ОбменСообщениями")
            .expect("ОбменСообщениями integration service not loaded");

        // Two channels in the fixture; only the Receive channel binds a handler.
        assert_eq!(service.channels().len(), 2);
        let handlers: Vec<_> = service.receive_handlers().collect();
        assert_eq!(handlers, vec!["ОбработатьСообщениеОбычныйПриоритет"]);
    }

    #[test]
    fn discover_http_service_structure_finds_dir_xml_and_module_stably() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_http_service_{}_{}",
            std::process::id(),
            line!()
        ));
        let services_dir = collection_dir(&bsl_conventions::RealFs, &root, "HTTPServices");
        std::fs::create_dir_all(services_dir.join("Alpha/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Beta/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Gamma/Ext")).unwrap();
        let case_twin = fs_keeps_case_distinct(&services_dir);
        if case_twin {
            std::fs::create_dir_all(services_dir.join("alpha/Ext")).unwrap();
        }

        std::fs::write(services_dir.join("Alpha.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Beta.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Gamma.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Alpha/Ext/Module.bsl"), "// alpha module").unwrap();
        std::fs::write(services_dir.join("Beta/Ext/Module.bsl"), "// beta module").unwrap();
        std::fs::write(services_dir.join("Gamma/Ext/Module.txt"), "not a module").unwrap();
        if case_twin {
            std::fs::write(services_dir.join("alpha.xml"), "<MetaDataObject/>").unwrap();
            std::fs::write(services_dir.join("alpha/Ext/Module.txt"), "ignored").unwrap();
        }
        std::fs::write(services_dir.join("sidecar.json"), "ignored").unwrap();

        let first = discover_http_service_structure(&root, &bsl_conventions::RealFs);
        let second = discover_http_service_structure(&root, &bsl_conventions::RealFs);

        let first_view: Vec<_> = first
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();
        let second_view: Vec<_> = second
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();

        assert_eq!(first_view, second_view, "HTTP service discovery order must be stable");
        let mut expected = vec![(
            "Alpha".to_string(),
            services_dir.join("Alpha.xml"),
            Some(services_dir.join("Alpha/Ext/Module.bsl")),
        )];
        if case_twin {
            expected.push(("alpha".to_string(), services_dir.join("alpha.xml"), None));
        }
        expected.push((
            "Beta".to_string(),
            services_dir.join("Beta.xml"),
            Some(services_dir.join("Beta/Ext/Module.bsl")),
        ));
        // Gamma is no case twin, so this rides on every volume: an `Ext` child
        // that is not `Module.bsl` is not a module body.
        expected.push(("Gamma".to_string(), services_dir.join("Gamma.xml"), None));
        assert_eq!(first_view, expected);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_web_service_structure_finds_dir_xml_and_module_stably() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_web_service_{}_{}",
            std::process::id(),
            line!()
        ));
        let services_dir = collection_dir(&bsl_conventions::RealFs, &root, "WebServices");
        std::fs::create_dir_all(services_dir.join("Alpha/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Beta/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Gamma/Ext")).unwrap();
        let case_twin = fs_keeps_case_distinct(&services_dir);
        if case_twin {
            std::fs::create_dir_all(services_dir.join("alpha/Ext")).unwrap();
        }

        std::fs::write(services_dir.join("Alpha.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Beta.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Gamma.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Alpha/Ext/Module.bsl"), "// alpha module").unwrap();
        std::fs::write(services_dir.join("Beta/Ext/Module.bsl"), "// beta module").unwrap();
        std::fs::write(services_dir.join("Gamma/Ext/Module.txt"), "not a module").unwrap();
        if case_twin {
            std::fs::write(services_dir.join("alpha.xml"), "<MetaDataObject/>").unwrap();
            std::fs::write(services_dir.join("alpha/Ext/Module.txt"), "ignored").unwrap();
        }
        std::fs::write(services_dir.join("sidecar.json"), "ignored").unwrap();

        let first = discover_web_service_structure(&root, &bsl_conventions::RealFs);
        let second = discover_web_service_structure(&root, &bsl_conventions::RealFs);

        let first_view: Vec<_> = first
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();
        let second_view: Vec<_> = second
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();

        assert_eq!(first_view, second_view, "Web service discovery order must be stable");
        let mut expected = vec![(
            "Alpha".to_string(),
            services_dir.join("Alpha.xml"),
            Some(services_dir.join("Alpha/Ext/Module.bsl")),
        )];
        if case_twin {
            expected.push(("alpha".to_string(), services_dir.join("alpha.xml"), None));
        }
        expected.push((
            "Beta".to_string(),
            services_dir.join("Beta.xml"),
            Some(services_dir.join("Beta/Ext/Module.bsl")),
        ));
        // Gamma is no case twin, so this rides on every volume: an `Ext` child
        // that is not `Module.bsl` is not a module body.
        expected.push(("Gamma".to_string(), services_dir.join("Gamma.xml"), None));
        assert_eq!(first_view, expected);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_integration_service_structure_finds_dir_xml_and_module_stably() {
        let root = std::env::temp_dir().join(format!(
            "bsl_discover_integration_service_{}_{}",
            std::process::id(),
            line!()
        ));
        let services_dir = collection_dir(&bsl_conventions::RealFs, &root, "IntegrationServices");
        std::fs::create_dir_all(services_dir.join("Alpha/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Beta/Ext")).unwrap();
        std::fs::create_dir_all(services_dir.join("Gamma/Ext")).unwrap();
        let case_twin = fs_keeps_case_distinct(&services_dir);
        if case_twin {
            std::fs::create_dir_all(services_dir.join("alpha/Ext")).unwrap();
        }

        std::fs::write(services_dir.join("Alpha.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Beta.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Gamma.xml"), "<MetaDataObject/>").unwrap();
        std::fs::write(services_dir.join("Alpha/Ext/Module.bsl"), "// alpha module").unwrap();
        std::fs::write(services_dir.join("Beta/Ext/Module.bsl"), "// beta module").unwrap();
        std::fs::write(services_dir.join("Gamma/Ext/Module.txt"), "not a module").unwrap();
        if case_twin {
            std::fs::write(services_dir.join("alpha.xml"), "<MetaDataObject/>").unwrap();
            std::fs::write(services_dir.join("alpha/Ext/Module.txt"), "ignored").unwrap();
        }
        std::fs::write(services_dir.join("sidecar.json"), "ignored").unwrap();

        let first = discover_integration_service_structure(&root, &bsl_conventions::RealFs);
        let second = discover_integration_service_structure(&root, &bsl_conventions::RealFs);

        let first_view: Vec<_> = first
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();
        let second_view: Vec<_> = second
            .iter()
            .map(|service| {
                (service.name.clone(), service.main.clone(), service.module_file.clone())
            })
            .collect();

        assert_eq!(first_view, second_view, "integration service discovery order must be stable");
        let mut expected = vec![(
            "Alpha".to_string(),
            services_dir.join("Alpha.xml"),
            Some(services_dir.join("Alpha/Ext/Module.bsl")),
        )];
        if case_twin {
            expected.push(("alpha".to_string(), services_dir.join("alpha.xml"), None));
        }
        expected.push((
            "Beta".to_string(),
            services_dir.join("Beta.xml"),
            Some(services_dir.join("Beta/Ext/Module.bsl")),
        ));
        // Gamma is no case twin, so this rides on every volume: an `Ext` child
        // that is not `Module.bsl` is not a module body.
        expected.push(("Gamma".to_string(), services_dir.join("Gamma.xml"), None));
        assert_eq!(first_view, expected);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_http_service_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        let discovered: BTreeSet<String> =
            discover_http_service_structure(Path::new(root), &bsl_conventions::RealFs)
                .into_iter()
                .map(|service| service.name)
                .collect();

        let config = load_from_directory(root).unwrap();
        let from_load: BTreeSet<String> =
            config.http_services().iter().map(|service| service.name().to_string()).collect();

        assert_eq!(discovered, from_load, "HTTP service discovery must match the directory loader");
    }

    #[test]
    fn discover_web_service_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        let discovered: BTreeSet<String> =
            discover_web_service_structure(Path::new(root), &bsl_conventions::RealFs)
                .into_iter()
                .map(|service| service.name)
                .collect();

        let config = load_from_directory(root).unwrap();
        let from_load: BTreeSet<String> =
            config.web_services().iter().map(|service| service.name().to_string()).collect();

        assert_eq!(discovered, from_load, "Web service discovery must match the directory loader");
    }

    #[test]
    fn discover_integration_service_structure_matches_directory_load() {
        use std::collections::BTreeSet;

        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");

        let discovered: BTreeSet<String> =
            discover_integration_service_structure(Path::new(root), &bsl_conventions::RealFs)
                .into_iter()
                .map(|service| service.name)
                .collect();

        let config = load_from_directory(root).unwrap();
        let from_load: BTreeSet<String> = config
            .integration_services()
            .iter()
            .map(|service| service.name().to_string())
            .collect();

        assert_eq!(
            discovered, from_load,
            "integration service discovery must match the directory loader"
        );
    }

    #[test]
    fn parse_http_service_from_text_preserves_nested_url_template_methods() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <HTTPService uuid="4797cd39-952d-4e4d-9685-014e4d5a8e25">
        <Properties>
            <Name>HTTPСервис1</Name>
            <RootURL>http</RootURL>
        </Properties>
        <ChildObjects>
            <URLTemplate uuid="7124b2c7-d38e-40b9-a934-e6eb9de99340">
                <Properties>
                    <Name>URLTemplate1</Name>
                    <Template>/storage/{Storage}/{ID}</Template>
                </Properties>
                <ChildObjects>
                    <Method uuid="605f52a9-e95b-4900-9e41-449d7da01348">
                        <Properties>
                            <Name>GET</Name>
                            <HTTPMethod>GET</HTTPMethod>
                            <Handler>URLTemplate1GET</Handler>
                        </Properties>
                    </Method>
                    <Method uuid="462355c3-a1d9-488b-91ea-979f880f910f">
                        <Properties>
                            <Name>POST</Name>
                            <HTTPMethod>POST</HTTPMethod>
                            <Handler>URLTemplate1POST</Handler>
                        </Properties>
                    </Method>
                </ChildObjects>
            </URLTemplate>
        </ChildObjects>
    </HTTPService>
</MetaDataObject>"#;

        let service =
            parse_http_service_from_text(xml, "HTTPСервис1").expect("HTTP service should parse");

        assert_eq!(service.name(), "HTTPСервис1");
        assert_eq!(service.root_url(), "http");
        assert_eq!(service.url_templates().len(), 1);

        let template = &service.url_templates()[0];
        assert_eq!(template.name(), "URLTemplate1");
        assert_eq!(template.template(), "/storage/{Storage}/{ID}");
        assert_eq!(template.methods().len(), 2);

        assert_eq!(template.methods()[0].name(), "GET");
        assert_eq!(template.methods()[0].handler(), "URLTemplate1GET");
        assert_eq!(template.methods()[1].name(), "POST");
        assert_eq!(template.methods()[1].handler(), "URLTemplate1POST");
    }

    #[test]
    fn parse_web_service_from_text_preserves_operation_handlers() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <WebService uuid="0b4a4c9c-76e9-455c-9471-249051a8301d">
        <Properties>
            <Name>WebСервис1</Name>
            <Namespace>test.com</Namespace>
        </Properties>
        <ChildObjects>
            <Operation uuid="bc99d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>Операция1</Name>
                    <ProcedureName>Операция1</ProcedureName>
                </Properties>
                <ChildObjects/>
            </Operation>
            <Operation uuid="bc09d837-aee6-40ee-8940-3a81dddf477c">
                <Properties>
                    <Name>ОперацияБезОбработчика</Name>
                    <ProcedureName/>
                </Properties>
                <ChildObjects/>
            </Operation>
        </ChildObjects>
    </WebService>
</MetaDataObject>"#;

        let service =
            parse_web_service_from_text(xml, "WebСервис1").expect("web service should parse");

        assert_eq!(service.name(), "WebСервис1");
        assert_eq!(service.namespace(), "test.com");
        assert_eq!(service.operations().len(), 2);

        let operation = &service.operations()[0];
        assert_eq!(operation.name(), "Операция1");
        assert_eq!(operation.procedure_name(), "Операция1");

        let empty_operation = &service.operations()[1];
        assert_eq!(empty_operation.name(), "ОперацияБезОбработчика");
        assert_eq!(empty_operation.procedure_name(), "");
    }

    #[test]
    fn parse_integration_service_from_text_preserves_channel_handlers() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <IntegrationService uuid="c512a1cd-1240-4e46-8bad-8b7b27c5c25a">
        <Properties>
            <Name>ОбменСообщениями</Name>
        </Properties>
        <ChildObjects>
            <IntegrationServiceChannel uuid="1ef0581c-b1d8-4115-87f1-7856f6c06bb6">
                <Properties>
                    <Name>input_from_SM_normal_priority</Name>
                    <MessageDirection>Receive</MessageDirection>
                    <ReceiveMessageProcessing>ОбработатьСообщениеОбычныйПриоритет</ReceiveMessageProcessing>
                </Properties>
            </IntegrationServiceChannel>
            <IntegrationServiceChannel uuid="b017ac62-a4a2-47bd-b963-50e0764a7d4e">
                <Properties>
                    <Name>output_to_SM_high_priority</Name>
                    <MessageDirection>Send</MessageDirection>
                    <ReceiveMessageProcessing/>
                </Properties>
            </IntegrationServiceChannel>
            <IntegrationServiceChannel uuid="c017ac62-a4a2-47bd-b963-50e0764a7d4e">
                <Properties>
                    <Name>input_from_SM_empty_handler</Name>
                    <MessageDirection>Receive</MessageDirection>
                    <ReceiveMessageProcessing/>
                </Properties>
            </IntegrationServiceChannel>
        </ChildObjects>
    </IntegrationService>
</MetaDataObject>"#;

        let service = parse_integration_service_from_text(xml, "ОбменСообщениями")
            .expect("integration service should parse");

        assert_eq!(service.name(), "ОбменСообщениями");
        assert_eq!(service.channels().len(), 3);

        let handlers: Vec<_> = service.receive_handlers().collect();
        assert_eq!(handlers, vec!["ОбработатьСообщениеОбычныйПриоритет"]);
        assert_eq!(service.channels()[0].name(), "input_from_SM_normal_priority");
        assert_eq!(
            service.channels()[0].receive_message_processing(),
            "ОбработатьСообщениеОбычныйПриоритет"
        );
        assert_eq!(service.channels()[1].name(), "output_to_SM_high_priority");
        assert_eq!(service.channels()[1].receive_message_processing(), "");
        assert_eq!(service.channels()[2].name(), "input_from_SM_empty_handler");
        assert_eq!(service.channels()[2].receive_message_processing(), "");
    }

    #[test]
    fn loads_data_processors_with_attributes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();

        let mdo = config
            .find_metadata_object(
                crate::metadata_object::MdoType::DataProcessor,
                "ТестоваяОбработка",
            )
            .expect("ТестоваяОбработка not loaded");
        assert_eq!(
            mdo.attributes.len(),
            2,
            "expected 2 user attributes (no standard for DataProcessor)"
        );
        assert!(mdo.find_attribute("АдресСайта").is_some());
        assert!(mdo.find_attribute("СоздаватьГруппы").is_some());
    }

    #[test]
    fn loads_reports_with_attributes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();

        let mdo = config
            .find_metadata_object(crate::metadata_object::MdoType::Report, "ТестовыйОтчёт")
            .expect("ТестовыйОтчёт not loaded");
        assert_eq!(mdo.attributes.len(), 1);
        assert!(mdo.find_attribute("ПериодОтчёта").is_some());
    }

    #[test]
    fn test_load_protected_module() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/designer");
        let config = load_from_directory(path).unwrap();

        let protected_module = config.find_common_module("ЗащищенныйМодуль");
        assert!(protected_module.is_some(), "ЗащищенныйМодуль not found");

        let module = protected_module.unwrap();
        assert!(module.is_protected(), "Module should be protected");
        assert!(module.uri().is_none(), "Protected module should not have URI");
        assert!(module.is_server(), "Should be server module");
        assert!(module.is_server_call(), "Should have server call");
    }

    #[test]
    #[ignore]
    fn test_load_enum_values_from_doc3() {
        let doc3_path = concat!(env!("HOME"), "/src/doc3/src/cf");

        if !std::path::Path::new(doc3_path).exists() {
            eprintln!("Skipping test: doc3 project not found at {}", doc3_path);
            return;
        }

        let config = load_from_directory(doc3_path).expect("Failed to load doc3 configuration");

        let enums: Vec<_> = config
            .metadata_objects()
            .iter()
            .filter(|obj| obj.mdo_type == crate::metadata_object::MdoType::Enum)
            .collect();

        println!("Total Enums loaded: {}", enums.len());
        assert!(!enums.is_empty(), "No enums loaded");

        println!("\nFirst 10 Enums:");
        for (i, enum_obj) in enums.iter().take(10).enumerate() {
            println!("  {}: {} (values: {})", i + 1, enum_obj.name, enum_obj.enum_values.len());
        }

        let target_name = "СпособыУстановкиКурсаВалюты";
        let target_enum_specific = enums.iter().find(|e| e.name == target_name);

        if let Some(enum_obj) = target_enum_specific {
            println!("\n✅ Found target enum: {}", target_name);
            println!("  EnumValues count: {}", enum_obj.enum_values.len());
            for (i, ev) in enum_obj.enum_values.iter().enumerate() {
                println!("    {}: {}", i + 1, ev.name);
            }
        } else {
            println!("\n❌ Target enum '{}' NOT FOUND", target_name);
            println!("\nAll enum names:");
            for (i, e) in enums.iter().enumerate() {
                println!("  {}: {}", i + 1, e.name);
            }
        }

        let target_enum = enums.iter().find(|e| !e.enum_values.is_empty());

        if let Some(enum_obj) = target_enum {
            println!("✅ Found enum: {}", enum_obj.name);
            println!("  EnumValues count: {}", enum_obj.enum_values.len());

            assert!(!enum_obj.enum_values.is_empty(), "EnumValues should not be empty");

            println!("  First 5 EnumValues:");
            for (i, ev) in enum_obj.enum_values.iter().take(5).enumerate() {
                println!("    {}: {} (uuid: {})", i + 1, ev.name, ev.uuid);
            }

            if let Some(first_value) = enum_obj.enum_values.first() {
                let found = enum_obj.find_enum_value(&first_value.name);
                assert!(found.is_some(), "find_enum_value should work");

                let found_lower = enum_obj.find_enum_value(&first_value.name.fold_lower());
                assert!(found_lower.is_some(), "find_enum_value should be case-insensitive");
            }
        } else {
            panic!("❌ Enum 'ЗаданияОчередиОбновленияПрав' not found");
        }
    }

    #[test]
    #[ignore]
    fn test_niagara_field_resolution() {
        let path = concat!(env!("HOME"), "/src/niagara_ut/src/cf");
        if !std::path::Path::new(path).exists() {
            return;
        }
        let config = load_from_directory(path).unwrap();

        let kl = config
            .metadata_objects()
            .iter()
            .find(|o| o.name == "КартыЛояльности" && o.mdo_type == MdoType::Catalog);
        if let Some(kl) = kl {
            println!("КартыЛояльности: {} attrs", kl.attributes.len());
            for a in &kl.attributes {
                println!("  {}", a.name);
            }
            assert!(kl.attributes.iter().any(|a| a.name == "Партнер"), "Партнер not found");
            assert!(kl.attributes.iter().any(|a| a.name == "Статус"), "Статус not found");
        } else {
            println!("КартыЛояльности not found in metadata");
        }

        let reg =
            config.registers().iter().find(|r| r.name() == "СостояниеАдресовЭлектроннойПочты");
        if let Some(reg) = reg {
            println!(
                "Регистр: dims={}, res={}, attrs={}",
                reg.dimensions().len(),
                reg.resources().len(),
                reg.attributes().len()
            );
            for d in reg.dimensions() {
                println!("  dim: {}", d.name());
            }
            for r in reg.resources() {
                println!("  res: {}", r.name());
            }
            for a in reg.attributes() {
                println!("  attr: {}", a.name());
            }
        } else {
            println!("Register not found");
        }

        let p = config
            .metadata_objects()
            .iter()
            .find(|o| o.name == "Партнеры" && o.mdo_type == MdoType::Catalog);
        if let Some(p) = p {
            for ts in &p.tabular_sections {
                if ts.name() == "КонтактнаяИнформация" {
                    println!("ТЧ КонтактнаяИнформация: {} attrs", ts.attributes().len());
                    for a in ts.attributes() {
                        println!("  {}", a.name());
                    }
                }
            }
        } else {
            println!("Партнеры not found");
        }
    }

    #[test]
    #[ignore]
    fn test_load_from_doc3_project() {
        let doc3_path = concat!(env!("HOME"), "/src/doc3/src/cf");

        if !std::path::Path::new(doc3_path).exists() {
            eprintln!("Skipping test: doc3 project not found at {}", doc3_path);
            return;
        }

        let config = load_from_directory(doc3_path).expect("Failed to load doc3 configuration");

        println!("Total registers loaded: {}", config.registers().len());

        let info_registers: Vec<_> =
            config.registers().iter().filter(|r| r.is_information_register()).collect();

        println!("InformationRegisters count: {}", info_registers.len());

        println!("\nFirst 20 InformationRegisters:");
        for (i, reg) in info_registers.iter().take(20).enumerate() {
            println!("  {}: {}", i + 1, reg.name());
        }

        println!("\nRegisters containing 'Значения':");
        for reg in info_registers.iter() {
            if reg.name().contains("Значения") {
                println!("  - {}", reg.name());
            }
        }

        println!("\nRegisters containing 'Действий':");
        for reg in info_registers.iter() {
            if reg.name().contains("Действий") {
                println!("  - {}", reg.name());
            }
        }

        println!("\nRegisters containing 'Писем':");
        for reg in info_registers.iter() {
            if reg.name().contains("Писем") {
                println!("  - {}", reg.name());
            }
        }

        let target_register = config.find_register("ЗначенияДействийПриОбработкеПисем");

        if let Some(register) = target_register {
            println!("✅ Found register: {}", register.name());
            println!("  Type: {:?}", register.mdo_type());
            println!("  Dimensions: {}", register.dimensions().len());
            println!("  Resources: {}", register.resources().len());
            println!("  Attributes: {}", register.attributes().len());

            for dim in register.dimensions() {
                println!("    Dimension: {}", dim.name());
            }

            for res in register.resources() {
                println!("    Resource: {} - Type: {:?}", res.name(), res.attr_type());
            }

            for attr in register.attributes() {
                println!("    Attribute: {} - Type: {:?}", attr.name(), attr.attr_type());
            }

            assert!(register.is_information_register(), "Should be InformationRegister");
        } else {
            panic!("❌ Register 'ЗначенияДействийПриОбработкеПисем' not found in loaded configuration!");
        }
    }

    #[test]
    #[ignore]
    fn test_catalog_xml_only_without_directory() {
        let doc3_path = concat!(env!("HOME"), "/src/doc3/src/cf");

        if !std::path::Path::new(doc3_path).exists() {
            eprintln!("Skipping test: doc3 project not found at {}", doc3_path);
            return;
        }

        let config = load_from_directory(doc3_path).expect("Failed to load doc3 configuration");

        let catalog_name = "ПоставляемыеДополнительныеОтчетыИОбработки";

        let xml_path = format!("{}/Catalogs/{}.xml", doc3_path, catalog_name);
        let dir_path = format!("{}/Catalogs/{}", doc3_path, catalog_name);
        assert!(std::path::Path::new(&xml_path).exists(), "XML file should exist");
        assert!(
            !std::path::Path::new(&dir_path).exists(),
            "Directory should NOT exist (this is the test case)"
        );

        let exists =
            config.has_metadata_object(crate::metadata_object::MdoType::Catalog, catalog_name);
        assert!(exists, "Catalog '{}' should be loaded from XML-only file", catalog_name);
    }
}

#[cfg(test)]
mod case_parity_tests {
    //! По контролю на каждую функцию discovery/загрузки: фильтры листингов и
    //! пробы сконструированных имён сидят в независимых функциях, представитель
    //! одной не исполняет остальные.

    use super::*;
    use crate::traits::MdObject;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bsl_case_parity_{tag}_{}_{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The spelling a probe hands back under `root`: the one really on disk
    /// where a wrong-case construction misses, the constructed canonical one
    /// where the volume answers any spelling and the exact probe hits first.
    fn probed_spelling(
        root: &Path,
        on_disk: &'static str,
        canonical: &'static str,
    ) -> &'static str {
        if fs_keeps_case_distinct(root) {
            on_disk
        } else {
            canonical
        }
    }

    const COMMON_XML: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\">",
        "<CommonModule uuid=\"00000000-0000-0000-0000-000000000001\">",
        "<Properties><Name>X</Name><Server>true</Server></Properties>",
        "</CommonModule></MetaDataObject>"
    );

    #[test]
    fn discovery_of_a_common_module_takes_case_variant_probes() {
        let root = temp_root("cm");
        write(&root.join("CommonModules/X.xml"), COMMON_XML);
        write(&root.join("CommonModules/X/EXT/MODULE.BSL"), "// тело");
        let found = discover_common_module_structure(&root, &bsl_conventions::RealFs);
        assert_eq!(found.len(), 1);
        let expected = probed_spelling(&root, "EXT/MODULE.BSL", "Ext/Module.bsl");
        assert!(
            found[0].module_file.as_deref().is_some_and(|p| p.ends_with(expected)),
            "тело найдено и несёт написание {expected}: {:?}",
            found[0].module_file
        );
    }

    #[test]
    fn discovery_of_services_takes_case_variant_probes() {
        let root = temp_root("svc");
        write(&root.join("HTTPServices/S.xml"), "<x/>");
        write(&root.join("HTTPServices/S/EXT/MODULE.BSL"), "//");
        write(&root.join("WebServices/W.xml"), "<x/>");
        write(&root.join("WebServices/W/EXT/MODULE.BSL"), "//");
        write(&root.join("IntegrationServices/I.xml"), "<x/>");
        write(&root.join("IntegrationServices/I/EXT/MODULE.BSL"), "//");
        let expected = probed_spelling(&root, "EXT/MODULE.BSL", "Ext/Module.bsl");
        let http = discover_http_service_structure(&root, &bsl_conventions::RealFs);
        assert!(http[0].module_file.as_deref().is_some_and(|p| p.ends_with(expected)));
        let web = discover_web_service_structure(&root, &bsl_conventions::RealFs);
        assert!(web[0].module_file.as_deref().is_some_and(|p| p.ends_with(expected)));
        let integration = discover_integration_service_structure(&root, &bsl_conventions::RealFs);
        assert!(integration[0].module_file.as_deref().is_some_and(|p| p.ends_with(expected)));
    }

    #[test]
    fn discovery_listing_filters_take_case_variant_xml() {
        let root = temp_root("lists");
        write(&root.join("DefinedTypes/Новый.XML"), "<x/>");
        write(&root.join("ScheduledJobs/Job.XML"), "<x/>");
        write(&root.join("EventSubscriptions/Событие.XML"), "<x/>");
        assert_eq!(
            discover_defined_type_structure(&root, &bsl_conventions::RealFs).len(),
            1,
            "DefinedTypes"
        );
        assert_eq!(
            discover_scheduled_job_structure(&root, &bsl_conventions::RealFs).len(),
            1,
            "ScheduledJobs"
        );
        assert_eq!(
            discover_event_subscription_structure(&root, &bsl_conventions::RealFs).len(),
            1,
            "EventSubscriptions"
        );
    }

    #[test]
    fn discovery_of_roles_takes_case_variant_listing_and_rights_probe() {
        let root = temp_root("roles");
        write(&root.join("Roles/Роль.XML"), "<x/>");
        write(&root.join("Roles/Роль/EXT/RIGHTS.XML"), "<x/>");
        let roles = discover_role_structure(&root, &bsl_conventions::RealFs);
        assert_eq!(roles.len(), 1);
        let expected = probed_spelling(&root, "EXT/RIGHTS.XML", "Ext/Rights.xml");
        assert!(
            roles[0].rights.as_deref().is_some_and(|p| p.ends_with(expected)),
            "права найдены через регистронезависимую пробу: {:?}",
            roles[0].rights
        );
    }

    #[test]
    fn discovery_of_catalogs_takes_a_case_variant_sibling_and_predefined() {
        let root = temp_root("cat");
        std::fs::create_dir_all(root.join("Catalogs/Товар")).unwrap();
        write(&root.join("Catalogs/Товар.XML"), "<x/>");
        write(&root.join("Catalogs/Товар/EXT/PREDEFINED.XML"), "<x/>");
        let found = discover_metadata_structure(&root, &bsl_conventions::RealFs);
        let catalog = found.iter().find(|m| m.name == "Товар");
        assert!(catalog.is_some(), "каталог с соседним Товар.XML обнаружен");
        let expected = probed_spelling(&root, "EXT/PREDEFINED.XML", "Ext/Predefined.xml");
        assert!(
            catalog.unwrap().predefined.as_deref().is_some_and(|p| p.ends_with(expected)),
            "predefined найден через пробу"
        );
    }

    #[test]
    fn a_case_variant_collection_directory_is_still_the_collection() {
        let root = temp_root("coll");
        write(&root.join("CATALOGS/Товар.XML"), "<x/>");
        std::fs::create_dir_all(root.join("CATALOGS/Товар")).unwrap();
        let found = discover_metadata_structure(&root, &bsl_conventions::RealFs);
        assert!(
            found.iter().any(|m| m.name == "Товар"),
            "CATALOGS/ — та же коллекция в другом регистре"
        );
    }

    #[test]
    fn full_load_takes_a_case_variant_module_and_keeps_its_spelling_in_uri() {
        let root = temp_root("load");
        write(&root.join("COMMONMODULES/X.xml"), COMMON_XML);
        write(&root.join("COMMONMODULES/X/EXT/MODULE.BSL"), "// тело");
        let config = load_from_directory(&root).unwrap();
        let module = config.common_modules().iter().find(|m| m.name() == "X").unwrap();
        use crate::traits::Module as _;
        let expected = probed_spelling(
            &root,
            "COMMONMODULES/X/EXT/MODULE.BSL",
            "CommonModules/X/Ext/Module.bsl",
        );
        assert_eq!(
            module.uri(),
            Some(expected),
            "URI несёт написание найденного пути, включая сегмент коллекции"
        );
    }
}

#[cfg(test)]
mod tree_source_tests {
    //! Один и тот же обход поверх двух источников: реальной ФС, которую читает
    //! резидент, и набора путей, который уже просканировал драйвер графа.

    use super::*;
    use bsl_conventions::{PathSetTree, RealFs};

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bsl_tree_source_{tag}_{}_{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn every_file_under(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                every_file_under(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// What a scan collects: `.bsl` and `.xml`, by extension alone.
    fn scanned_universe(root: &Path) -> PathSetTree {
        let mut files = Vec::new();
        every_file_under(root, &mut files);
        PathSetTree::from_files(files.into_iter().filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("bsl") || e.eq_ignore_ascii_case("xml"))
        }))
    }

    fn designer_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/designer")
    }

    /// The two sources have to discover the same structure, or the map the graph
    /// builds names files the resident's listing never names — and the pair that
    /// should merge stays two rows.
    ///
    /// The input is chosen for the one shape that separates the sources: the
    /// protected module's `Ext/` holds only `Module.bin`, which no scan collects,
    /// so its directory does not exist for the path set at all. On an input
    /// without such a directory this test is green however discovery is keyed.
    #[test]
    fn both_tree_sources_discover_the_same_structure() {
        let root = designer_fixture();
        let set = scanned_universe(&root);

        assert!(
            set.kind_of(&root.join("CommonModules/ЗащищенныйМодуль")).is_none(),
            "the fixture stopped carrying a directory a scan cannot see; without one \
             this test proves nothing",
        );

        assert_eq!(
            discover_metadata_structure(&root, &RealFs),
            discover_metadata_structure(&root, &set),
        );
        assert_eq!(
            discover_register_structure(&root, &RealFs),
            discover_register_structure(&root, &set),
        );
        assert_eq!(
            discover_defined_type_structure(&root, &RealFs),
            discover_defined_type_structure(&root, &set),
        );
        assert_eq!(
            discover_common_module_structure(&root, &RealFs),
            discover_common_module_structure(&root, &set),
        );
        assert_eq!(
            discover_event_subscription_structure(&root, &RealFs),
            discover_event_subscription_structure(&root, &set),
        );
        assert_eq!(
            discover_scheduled_job_structure(&root, &RealFs),
            discover_scheduled_job_structure(&root, &set),
        );
        assert_eq!(
            discover_subsystem_structure(&root, &RealFs),
            discover_subsystem_structure(&root, &set),
        );
        assert_eq!(discover_role_structure(&root, &RealFs), discover_role_structure(&root, &set),);
        assert_eq!(
            discover_http_service_structure(&root, &RealFs),
            discover_http_service_structure(&root, &set),
        );
        assert_eq!(
            discover_web_service_structure(&root, &RealFs),
            discover_web_service_structure(&root, &set),
        );
        assert_eq!(
            discover_integration_service_structure(&root, &RealFs),
            discover_integration_service_structure(&root, &set),
        );
    }

    /// A dump may place an object's directory behind a symlink. The kind has to
    /// be the TARGET's, or the object takes the branch that has no sidecar and
    /// its `Ext/Predefined.xml` is silently dropped — while the loader beside
    /// this one, which follows the link, still parses it.
    #[cfg(unix)]
    #[test]
    fn an_object_behind_a_directory_symlink_keeps_its_predefined_sidecar() {
        let root = temp_root("symlink");
        let target = root.join("общее/Товары");
        write(&target.join("Ext/Predefined.xml"), "<PredefinedItems/>");
        write(&root.join("Catalogs/Товары.xml"), "<MetaDataObject/>");
        std::os::unix::fs::symlink(&target, root.join("Catalogs/Товары")).unwrap();

        let found = discover_metadata_structure(&root, &RealFs);
        let object = found.iter().find(|mdo| mdo.name == "Товары").expect("the catalog is listed");
        assert!(object.predefined.is_some(), "the sidecar behind the link was dropped: {object:?}",);
    }

    /// The protected module is found through this input on the real filesystem
    /// too — the one above cannot say so, because there the directory exists.
    #[test]
    fn a_common_module_without_a_directory_is_still_listed() {
        let root = temp_root("no_dir");
        write(&root.join("CommonModules/Защищенный.xml"), "<MetaDataObject/>");

        let found = discover_common_module_structure(&root, &RealFs);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Защищенный");
        assert_eq!(found[0].module_file, None, "no body, and none invented");

        let set = scanned_universe(&root);
        assert_eq!(discover_common_module_structure(&root, &set), found);
    }
}
