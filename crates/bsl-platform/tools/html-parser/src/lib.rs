//! HTML help pages of the 1C syntax helper → the help corpus JSON.
//!
//! [`extract_corpus_from_hbk`] reads the two help archives of an installed
//! platform in-process ([`hbk`]) and parses the unpacked pages;
//! [`parse_help_dirs`] parses an already unpacked page tree.

#[cfg(any(test, feature = "test-support"))]
pub mod fixture_hbk;
pub mod hbk;
#[cfg(any(test, feature = "test-support"))]
pub mod hbk_writer;
mod scraper_parser;

use anyhow::{Context, Result};
use scraper_parser::{CodeExample, ParamDescription, ParameterVariant};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct PlatformData {
    keywords: Vec<KeywordInfo>,
    types: Vec<TypeInfo>,
    methods: Vec<MethodInfo>,
    global_functions: Vec<GlobalFunctionInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    constructors: Vec<ConstructorInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    properties: Vec<PropertyInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeywordInfo {
    keyword_ru: String,
    keyword_en: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<KeywordDocumentation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeywordDocumentation {
    syntax: String,
    description: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    param_descriptions: Vec<ParamDescription>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TypeInfo {
    name: String,
    english_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ContextAvailability>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    iter_element_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    xdto_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MethodInfo {
    id: u32,
    type_name: String,
    name: String,
    english_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    return_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    parameters: Vec<MethodParameter>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    variants: Vec<MethodVariantInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ContextAvailability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<MethodDocumentation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MethodVariantInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    variant_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    parameters: Vec<MethodParameter>,
}

impl From<ParameterVariant> for MethodVariantInfo {
    fn from(v: ParameterVariant) -> Self {
        Self { variant_name: v.variant_name, parameters: v.parameters }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GlobalFunctionInfo {
    id: u32,
    name: String,
    english_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    return_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    parameters: Vec<MethodParameter>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    variants: Vec<GlobalFunctionVariantInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ContextAvailability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<MethodDocumentation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GlobalFunctionVariantInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    variant_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    parameters: Vec<MethodParameter>,
}

impl From<ParameterVariant> for GlobalFunctionVariantInfo {
    fn from(v: ParameterVariant) -> Self {
        Self { variant_name: v.variant_name, parameters: v.parameters }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ConstructorInfo {
    id: u32,
    type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    parameters: Vec<MethodParameter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ContextAvailability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<ConstructorDocumentation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConstructorDocumentation {
    syntax: String,
    description: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    param_descriptions: Vec<ParamDescription>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    examples: Vec<CodeExample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    see_also: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PropertyInfo {
    id: u32,
    type_name: String,
    name: String,
    english_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    property_types: Vec<String>,
    is_readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ContextAvailability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<PropertyDocumentation>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PropertyDocumentation {
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    see_also: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MethodParameter {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    param_type: Option<String>,
    is_optional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    is_variadic: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn mark_trailing_variadic_from_syntax(parameters: &mut [MethodParameter], html: &str) {
    if parameters.is_empty() {
        return;
    }
    let Some(syntax) = scraper_parser::extract_syntax(html) else {
        return;
    };
    if syntax.contains(">,...,<") {
        if let Some(last) = parameters.last_mut() {
            last.is_variadic = true;
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct MethodDocumentation {
    syntax: String,
    description: String,
    param_descriptions: Vec<ParamDescription>,
    examples: Vec<CodeExample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    see_also: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextAvailability {
    thick_client: bool,
    thin_client: bool,
    web_client: bool,
    server: bool,
    mobile_client: bool,
    external_connection: bool,
}

impl PlatformData {
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.methods.is_empty() && self.global_functions.is_empty()
    }

    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    pub fn method_count(&self) -> usize {
        self.methods.len()
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("failed to serialize the help corpus")
    }
}

/// Parses unpacked help page trees: `shlang_dir` holds the language pages
/// (`*.st` plus their HTML), `shcntx_dir` the `objects/` context catalog.
/// Keyword pages that fail to parse leave the keyword list empty, as before.
pub fn parse_help_dirs(shlang_dir: &Path, shcntx_dir: &Path) -> Result<PlatformData> {
    let keywords = match parse_shlang_keywords(shlang_dir) {
        Ok(keywords) => keywords,
        Err(error) => {
            tracing::warn!(%error, "failed to parse shlang keywords; continuing without them");
            Vec::new()
        }
    };
    let (types, methods, global_functions, constructors, properties) =
        parse_shcntx_data(shcntx_dir).context("failed to parse shcntx data")?;
    tracing::info!(
        types = types.len(),
        methods = methods.len(),
        global_functions = global_functions.len(),
        constructors = constructors.len(),
        properties = properties.len(),
        "parsed platform help pages"
    );
    Ok(PlatformData { keywords, types, methods, global_functions, constructors, properties })
}

/// Version of the HBK extraction and HTML→JSON rules; part of every cache key
/// derived from extracted corpora.
pub const EXTRACTOR_VERSION: &str = concat!("html-parser/", env!("CARGO_PKG_VERSION"), "+hbk1");

/// Extracts the help corpus from the two help archives of a platform, entirely
/// in-process. Pages are unpacked into a new directory of their own created
/// under `scratch_parent` and removed afterwards; nothing else under
/// `scratch_parent` is touched. A container that yields no platform types is an
/// error, not an empty corpus.
pub fn extract_corpus_from_hbk(
    shcntx_hbk: &Path,
    shlang_hbk: &Path,
    scratch_parent: &Path,
) -> Result<PlatformData> {
    fs::create_dir_all(scratch_parent)
        .with_context(|| format!("failed to create {}", scratch_parent.display()))?;
    remove_abandoned_work_dirs(scratch_parent);
    let work_dir = scratch_parent.join(unique_work_name());
    fs::create_dir(&work_dir)
        .with_context(|| format!("failed to create {}", work_dir.display()))?;
    // Held for the whole extraction: a cleanup in another process sees the
    // directory as live while this lock stands, and the operating system drops
    // it if this process dies.
    // The marker is written only once the lock is held, so a cleanup that finds
    // the lock free and the file empty knows the owner may not have locked yet.
    let liveness = fs::File::create(work_dir.join(WORK_LOCK))
        .and_then(|mut file| {
            file.lock()?;
            std::io::Write::write_all(&mut file, b"locked")?;
            Ok(file)
        })
        .with_context(|| format!("failed to lock {}", work_dir.display()))?;
    let result = (|| {
        let shlang_dir = work_dir.join("shlang");
        let shcntx_dir = work_dir.join("shcntx");
        let shlang_files = hbk::extract_file_storage(shlang_hbk, &shlang_dir)?;
        let shcntx_files = hbk::extract_file_storage(shcntx_hbk, &shcntx_dir)?;
        tracing::info!(shlang_files, shcntx_files, "unpacked platform help archives");
        let data = parse_help_dirs(&shlang_dir, &shcntx_dir)?;
        if data.types.is_empty() {
            anyhow::bail!(
                "{} yields no platform types; the archive layout is not recognized",
                shcntx_hbk.display()
            );
        }
        Ok(data)
    })();
    drop(liveness);
    let _ = fs::remove_dir_all(&work_dir);
    result
}

const WORK_PREFIX: &str = ".hbk-extract-";

const WORK_LOCK: &str = ".lock";

/// Work directories of extractions killed midway, recognized by this
/// extractor's own naming. A directory is dead when its lock can be taken and
/// its lock file carries the marker its owner writes after locking: the owner
/// locked, and the lock is free only because the owner is gone. A lock file
/// still empty, or none at all, may belong to an owner about to lock; such a
/// directory is only taken once it is an hour old.
fn remove_abandoned_work_dirs(scratch_parent: &Path) {
    const UNLOCKED_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);
    let Ok(entries) = fs::read_dir(scratch_parent) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(WORK_PREFIX) || !path.is_dir() {
            continue;
        }
        let old = || {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > UNLOCKED_GRACE)
        };
        let abandoned = match fs::OpenOptions::new().write(true).open(path.join(WORK_LOCK)) {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => lock.metadata().is_ok_and(|m| m.len() > 0) || old(),
                Err(_) => false,
            },
            Err(_) => old(),
        };
        if abandoned {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// A directory name no concurrent extraction — thread or process — uses.
fn unique_work_name() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{WORK_PREFIX}{}-{count}-{nanos}", std::process::id())
}

fn parse_shlang_keywords(shlang_dir: &Path) -> Result<Vec<KeywordInfo>> {
    let mut keywords = Vec::new();
    let mut st_file_count = 0;
    let mut html_found_count = 0;

    for entry in fs::read_dir(shlang_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("st") {
            st_file_count += 1;
            if let Some(keyword) = parse_keyword_file(&path)? {
                html_found_count += 1;
                keywords.push(keyword);
            }
        }
    }

    tracing::debug!(
        st_files = st_file_count,
        with_html = html_found_count,
        "parsed shlang keywords"
    );

    Ok(keywords)
}

fn parse_keyword_file(st_path: &Path) -> Result<Option<KeywordInfo>> {
    let html_path = st_path.with_extension("");
    if !html_path.exists() {
        return Ok(None);
    }

    let html_content = fs::read_to_string(&html_path)?;

    if let Some(keyword_doc) = scraper_parser::parse_keyword_html(&html_content) {
        let documentation = KeywordDocumentation {
            syntax: keyword_doc.syntax,
            description: keyword_doc.description,
            param_descriptions: keyword_doc.params,
            min_version: keyword_doc.min_version,
        };

        Ok(Some(KeywordInfo {
            keyword_ru: keyword_doc.keyword_ru,
            keyword_en: keyword_doc.keyword_en,
            documentation: Some(documentation),
        }))
    } else {
        Ok(None)
    }
}

/// Types, methods, global functions, constructors and properties of a context tree.
type ContextRecords = (
    Vec<TypeInfo>,
    Vec<MethodInfo>,
    Vec<GlobalFunctionInfo>,
    Vec<ConstructorInfo>,
    Vec<PropertyInfo>,
);

fn parse_shcntx_data(shcntx_dir: &Path) -> Result<ContextRecords> {
    let mut types = Vec::new();
    let mut methods = Vec::new();
    let mut global_functions = Vec::new();
    let mut constructors = Vec::new();
    let mut properties = Vec::new();
    let mut method_id_counter = 0u32;
    let mut function_id_counter = 0u32;
    let mut ctor_id_counter = 0u32;
    let mut prop_id_counter = 0u32;

    let objects_dir = shcntx_dir.join("objects");
    if !objects_dir.exists() {
        return Ok((types, methods, global_functions, constructors, properties));
    }

    let global_context_dir = objects_dir.join("Global context");
    if global_context_dir.exists() {
        tracing::debug!("parsing Global context directory");
        global_functions = parse_global_functions(&global_context_dir, &mut function_id_counter)?;

        let global_properties =
            parse_type_properties(&global_context_dir, "Global context", &mut prop_id_counter)?;
        tracing::debug!(count = global_properties.len(), "parsed global context properties");
        properties.extend(global_properties);
    }

    let mut catalog_entries: Vec<_> = fs::read_dir(&objects_dir)?.collect::<Result<_, _>>()?;
    catalog_entries.sort_by_key(|e| e.file_name());
    for catalog_entry in catalog_entries {
        let catalog_path = catalog_entry.path();

        if !catalog_path.is_dir() {
            continue;
        }

        if catalog_path.file_name().unwrap().to_string_lossy() == "Global context" {
            continue;
        }

        parse_catalog_directory(
            &catalog_path,
            &mut types,
            &mut methods,
            &mut constructors,
            &mut properties,
            &mut method_id_counter,
            &mut ctor_id_counter,
            &mut prop_id_counter,
        )?;
    }

    Ok((types, methods, global_functions, constructors, properties))
}

#[allow(clippy::too_many_arguments)]
fn parse_catalog_directory(
    catalog_path: &Path,
    types: &mut Vec<TypeInfo>,
    methods: &mut Vec<MethodInfo>,
    constructors: &mut Vec<ConstructorInfo>,
    properties: &mut Vec<PropertyInfo>,
    method_id_counter: &mut u32,
    ctor_id_counter: &mut u32,
    prop_id_counter: &mut u32,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(catalog_path)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let entry_path = entry.path();

        if !entry_path.is_dir() {
            continue;
        }

        let dir_name = entry_path.file_name().unwrap().to_string_lossy().to_string();

        if dir_name.starts_with("catalog") {
            parse_catalog_directory(
                &entry_path,
                types,
                methods,
                constructors,
                properties,
                method_id_counter,
                ctor_id_counter,
                prop_id_counter,
            )?;
            continue;
        }

        let type_html = entry_path.with_extension("html");
        if type_html.exists() {
            if let Some(type_info) = parse_type_html(&type_html, &entry_path)? {
                tracing::trace!(name = %type_info.name, english = %type_info.english_name, "found type");

                let type_methods =
                    parse_type_methods(&entry_path, &type_info.english_name, method_id_counter)?;
                methods.extend(type_methods);

                let type_ctors =
                    parse_type_constructors(&entry_path, &type_info.english_name, ctor_id_counter)?;
                constructors.extend(type_ctors);

                let type_properties =
                    parse_type_properties(&entry_path, &type_info.english_name, prop_id_counter)?;
                properties.extend(type_properties);

                types.push(type_info);
            }
        }
    }

    Ok(())
}

fn parse_type_html(html_path: &Path, _type_dir: &Path) -> Result<Option<TypeInfo>> {
    let html_content = fs::read_to_string(html_path)?;

    if let Some(title_start) = html_content.find(r#"<h1 class="V8SH_pagetitle">"#) {
        let after_title = &html_content[title_start + r#"<h1 class="V8SH_pagetitle">"#.len()..];
        if let Some(title_end) = after_title.find("</h1>") {
            let title = &after_title[..title_end];

            if let Some(paren_start) = title.find('(') {
                let russian = title[..paren_start].trim().to_string();
                let after_paren = &title[paren_start + 1..];
                if let Some(paren_end) = after_paren.find(')') {
                    let english = after_paren[..paren_end].trim().to_string();

                    let min_version = scraper_parser::extract_version(&html_content);
                    let context = scraper_parser::extract_context(&html_content);
                    let iter_element_types =
                        scraper_parser::extract_iter_element_types(&html_content);
                    let xdto_name = scraper_parser::extract_xdto_name(&html_content);

                    return Ok(Some(TypeInfo {
                        name: russian,
                        english_name: english,
                        min_version,
                        context,
                        iter_element_types,
                        xdto_name,
                    }));
                }
            }
        }
    }

    Ok(None)
}

fn parse_type_methods(
    type_dir: &Path,
    type_name: &str,
    method_id_counter: &mut u32,
) -> Result<Vec<MethodInfo>> {
    let mut methods = Vec::new();

    let methods_dir = type_dir.join("methods");
    if !methods_dir.exists() {
        return Ok(methods);
    }

    for method_entry in fs::read_dir(&methods_dir)? {
        let method_entry = method_entry?;
        let method_path = method_entry.path();

        if method_path.extension().and_then(|s| s.to_str()) != Some("html") {
            continue;
        }

        if let Some(method_info) = parse_method_html(&method_path, type_name, method_id_counter)? {
            methods.push(method_info);
        }
    }

    Ok(methods)
}

fn parse_method_html(
    html_path: &Path,
    type_name: &str,
    method_id_counter: &mut u32,
) -> Result<Option<MethodInfo>> {
    let html_content = fs::read_to_string(html_path)?;

    if let Some(title_start) = html_content.find(r#"<h1 class="V8SH_pagetitle">"#) {
        let after_title = &html_content[title_start + r#"<h1 class="V8SH_pagetitle">"#.len()..];
        if let Some(title_end) = after_title.find("</h1>") {
            let title = &after_title[..title_end];

            if let Some(dot_pos) = title.find('.') {
                let after_dot = &title[dot_pos + 1..];

                let russian_method = if let Some(space_pos) = after_dot.find(' ') {
                    after_dot[..space_pos].trim().to_string()
                } else {
                    return Ok(None);
                };

                if let Some(paren_start) = title.find('(') {
                    let after_paren = &title[paren_start + 1..];
                    if let Some(dot_in_paren) = after_paren.find('.') {
                        let after_paren_dot = &after_paren[dot_in_paren + 1..];
                        if let Some(paren_end) = after_paren_dot.find(')') {
                            let english_method = after_paren_dot[..paren_end].trim().to_string();

                            let id = *method_id_counter;
                            *method_id_counter += 1;

                            let return_type = scraper_parser::extract_return_type(&html_content);
                            let mut parameters = scraper_parser::extract_parameters(&html_content);
                            mark_trailing_variadic_from_syntax(&mut parameters, &html_content);
                            let raw_variants =
                                scraper_parser::extract_parameter_variants(&html_content);
                            let mut variants: Vec<MethodVariantInfo> = if raw_variants.len() > 1 {
                                raw_variants.into_iter().map(Into::into).collect()
                            } else {
                                Vec::new()
                            };
                            for variant in variants.iter_mut() {
                                mark_trailing_variadic_from_syntax(
                                    &mut variant.parameters,
                                    &html_content,
                                );
                            }
                            let min_version = scraper_parser::extract_version(&html_content);
                            let context = scraper_parser::extract_context(&html_content);

                            let documentation = extract_method_documentation(&html_content);

                            return Ok(Some(MethodInfo {
                                id,
                                type_name: type_name.to_string(),
                                name: russian_method,
                                english_name: english_method,
                                return_type,
                                parameters,
                                variants,
                                min_version,
                                context,
                                documentation,
                            }));
                        }
                    }
                }
            }
        }
    }

    Ok(None)
}

fn extract_method_documentation(html_content: &str) -> Option<MethodDocumentation> {
    let syntax = scraper_parser::extract_syntax(html_content).unwrap_or_default();
    let description = scraper_parser::extract_description(html_content)?;

    let param_descriptions_raw = scraper_parser::extract_parameter_descriptions(html_content);
    let param_descriptions = param_descriptions_raw
        .into_iter()
        .map(|p| ParamDescription {
            name: p.name,
            description: p.description,
            default_value: p.default_value,
        })
        .collect();

    let examples = scraper_parser::extract_examples(html_content);

    let notes = scraper_parser::extract_notes(html_content);

    Some(MethodDocumentation {
        syntax,
        description,
        param_descriptions,
        examples,
        notes,
        see_also: scraper_parser::extract_see_also(html_content),
    })
}

fn parse_type_constructors(
    type_dir: &Path,
    type_name_en: &str,
    ctor_id_counter: &mut u32,
) -> Result<Vec<ConstructorInfo>> {
    let ctors_dir = type_dir.join("ctors");
    if !ctors_dir.exists() {
        return Ok(Vec::new());
    }

    let mut html_paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in fs::read_dir(&ctors_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("html") {
            html_paths.push(path);
        }
    }
    html_paths.sort();

    let mut ctors = Vec::new();
    for path in html_paths {
        if let Some(info) = parse_constructor_html(&path, type_name_en, ctor_id_counter)? {
            ctors.push(info);
        }
    }
    Ok(ctors)
}

fn parse_constructor_html(
    html_path: &Path,
    type_name_en: &str,
    ctor_id_counter: &mut u32,
) -> Result<Option<ConstructorInfo>> {
    let html_content = fs::read_to_string(html_path)?;

    let variant_name = scraper_parser::extract_constructor_variant_name(&html_content);
    if variant_name.is_none() {
        return Ok(None);
    }

    let id = *ctor_id_counter;
    *ctor_id_counter += 1;

    let mut parameters = scraper_parser::extract_parameters(&html_content);
    mark_trailing_variadic_from_syntax(&mut parameters, &html_content);
    let min_version = scraper_parser::extract_version(&html_content);
    let context = scraper_parser::extract_context(&html_content);
    let documentation = extract_constructor_documentation(&html_content);

    Ok(Some(ConstructorInfo {
        id,
        type_name: type_name_en.to_string(),
        variant_name,
        parameters,
        min_version,
        context,
        documentation,
    }))
}

fn parse_type_properties(
    type_dir: &Path,
    type_name_en: &str,
    prop_id_counter: &mut u32,
) -> Result<Vec<PropertyInfo>> {
    let properties_dir = type_dir.join("properties");
    if !properties_dir.exists() {
        return Ok(Vec::new());
    }

    let mut html_paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in fs::read_dir(&properties_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("html") {
            html_paths.push(path);
        }
    }
    html_paths.sort();

    let mut out = Vec::new();
    for path in html_paths {
        if let Some(info) = parse_property_html(&path, type_name_en, prop_id_counter)? {
            out.push(info);
        }
    }
    Ok(out)
}

fn parse_property_html(
    html_path: &Path,
    type_name_en: &str,
    prop_id_counter: &mut u32,
) -> Result<Option<PropertyInfo>> {
    let html_content = fs::read_to_string(html_path)?;

    let Some(title_start) = html_content.find(r#"<h1 class="V8SH_pagetitle">"#) else {
        return Ok(None);
    };
    let after_title = &html_content[title_start + r#"<h1 class="V8SH_pagetitle">"#.len()..];
    let Some(title_end) = after_title.find("</h1>") else {
        return Ok(None);
    };
    let title = &after_title[..title_end];

    let Some(paren_start) = title.find('(') else {
        return Ok(None);
    };
    let russian_half = title[..paren_start].trim_end();
    let Some(last_dot_ru) = russian_half.rfind('.') else {
        return Ok(None);
    };
    let russian_prop = russian_half[last_dot_ru + 1..].trim().to_string();

    let after_paren = &title[paren_start + 1..];
    let Some(paren_end) = after_paren.rfind(')') else {
        return Ok(None);
    };
    let english_half = after_paren[..paren_end].trim();
    let Some(last_dot_en) = english_half.rfind('.') else {
        return Ok(None);
    };
    let english_prop = english_half[last_dot_en + 1..].trim().to_string();

    if russian_prop.is_empty() || english_prop.is_empty() {
        return Ok(None);
    }

    let looks_like_placeholder = |s: &str| s.starts_with('<') || s.starts_with("&lt;");
    if looks_like_placeholder(&russian_prop) || looks_like_placeholder(&english_prop) {
        return Ok(None);
    }

    let id = *prop_id_counter;
    *prop_id_counter += 1;

    let property_types = scraper_parser::extract_property_types(&html_content);
    let is_readonly = scraper_parser::extract_readonly(&html_content);
    let min_version = scraper_parser::extract_version(&html_content);
    let context = scraper_parser::extract_context(&html_content);
    let documentation = extract_property_documentation(&html_content);

    Ok(Some(PropertyInfo {
        id,
        type_name: type_name_en.to_string(),
        name: russian_prop,
        english_name: english_prop,
        property_types,
        is_readonly,
        min_version,
        context,
        documentation,
    }))
}

fn extract_property_documentation(html_content: &str) -> Option<PropertyDocumentation> {
    let description = scraper_parser::extract_description(html_content)
        .map(|d| strip_leading_tip_prefix(&d))
        .unwrap_or_default();
    let notes = scraper_parser::extract_notes(html_content);
    let see_also = scraper_parser::extract_see_also(html_content);

    if description.is_empty() && notes.is_none() && see_also.is_empty() {
        return None;
    }

    Some(PropertyDocumentation { description, notes, see_also })
}

fn strip_leading_tip_prefix(desc: &str) -> String {
    let trimmed = desc.trim_start();
    let Some(after_label) = trimmed.strip_prefix("Тип:") else {
        return desc.to_string();
    };
    let rest = after_label.trim_start();
    match rest.find('.') {
        Some(idx) => rest[idx + 1..].trim_start().to_string(),
        None => String::new(),
    }
}

fn extract_constructor_documentation(html_content: &str) -> Option<ConstructorDocumentation> {
    let syntax = scraper_parser::extract_syntax(html_content).unwrap_or_default();
    let description = scraper_parser::extract_description(html_content).unwrap_or_default();

    let param_descriptions = scraper_parser::extract_parameter_descriptions(html_content)
        .into_iter()
        .map(|p| ParamDescription {
            name: p.name,
            description: p.description,
            default_value: p.default_value,
        })
        .collect();

    let examples = scraper_parser::extract_examples(html_content);
    let notes = scraper_parser::extract_notes(html_content);
    let see_also = scraper_parser::extract_see_also(html_content);

    Some(ConstructorDocumentation {
        syntax,
        description,
        param_descriptions,
        examples,
        notes,
        see_also,
    })
}

fn parse_global_functions(
    global_context_dir: &Path,
    function_id_counter: &mut u32,
) -> Result<Vec<GlobalFunctionInfo>> {
    let mut functions = Vec::new();

    let methods_dir = global_context_dir.join("methods");
    if !methods_dir.exists() {
        return Ok(functions);
    }

    for catalog_entry in fs::read_dir(&methods_dir)? {
        let catalog_entry = catalog_entry?;
        let catalog_path = catalog_entry.path();

        if !catalog_path.is_dir() {
            continue;
        }

        let dir_name = catalog_path.file_name().unwrap().to_string_lossy().to_string();

        if !dir_name.starts_with("catalog") {
            continue;
        }

        for function_entry in fs::read_dir(&catalog_path)? {
            let function_entry = function_entry?;
            let function_path = function_entry.path();

            if function_path.extension().and_then(|s| s.to_str()) != Some("html") {
                continue;
            }

            if let Some(function_info) =
                parse_global_function_html(&function_path, function_id_counter)?
            {
                functions.push(function_info);
            }
        }
    }

    Ok(functions)
}

fn parse_global_function_html(
    html_path: &Path,
    function_id_counter: &mut u32,
) -> Result<Option<GlobalFunctionInfo>> {
    let html_content = fs::read_to_string(html_path)?;

    if let Some(title_start) = html_content.find(r#"<h1 class="V8SH_pagetitle">"#) {
        let after_title = &html_content[title_start + r#"<h1 class="V8SH_pagetitle">"#.len()..];
        if let Some(title_end) = after_title.find("</h1>") {
            let title = &after_title[..title_end];

            if let Some(first_dot) = title.find('.') {
                let after_first_dot = &title[first_dot + 1..];

                let russian_function = if let Some(space_pos) = after_first_dot.find(" (") {
                    after_first_dot[..space_pos].trim().to_string()
                } else {
                    return Ok(None);
                };

                if let Some(paren_start) = title.find("Global context.") {
                    let after_global_context = &title[paren_start + "Global context.".len()..];
                    if let Some(paren_end) = after_global_context.find(')') {
                        let english_function = after_global_context[..paren_end].trim().to_string();

                        let id = *function_id_counter;
                        *function_id_counter += 1;

                        let return_type = scraper_parser::extract_return_type(&html_content);
                        let mut parameters = scraper_parser::extract_parameters(&html_content);
                        mark_trailing_variadic_from_syntax(&mut parameters, &html_content);
                        let raw_variants =
                            scraper_parser::extract_parameter_variants(&html_content);
                        let mut variants: Vec<GlobalFunctionVariantInfo> = if raw_variants.len() > 1
                        {
                            raw_variants.into_iter().map(Into::into).collect()
                        } else {
                            Vec::new()
                        };
                        for variant in variants.iter_mut() {
                            mark_trailing_variadic_from_syntax(
                                &mut variant.parameters,
                                &html_content,
                            );
                        }
                        let min_version = scraper_parser::extract_version(&html_content);
                        let context = scraper_parser::extract_context(&html_content);

                        let documentation = extract_method_documentation(&html_content);

                        tracing::trace!(
                            name = %russian_function,
                            english = %english_function,
                            "found global function"
                        );

                        return Ok(Some(GlobalFunctionInfo {
                            id,
                            name: russian_function,
                            english_name: english_function,
                            return_type,
                            parameters,
                            variants,
                            min_version,
                            context,
                            documentation,
                        }));
                    }
                }
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod ctor_tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("html-parser-ctor-{tag}-{pid}-{ts}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    #[test]
    fn variant_name_prefers_heading_over_h1() {
        let html = r#"<html><body>
            <h1 class="V8SH_pagetitle">ФейкТип.H1-вариант</h1>
            <p class="V8SH_heading">Heading-вариант</p>
            </body></html>"#;
        assert_eq!(
            scraper_parser::extract_constructor_variant_name(html),
            Some("Heading-вариант".to_string())
        );
    }

    #[test]
    fn variant_name_falls_back_to_h1_after_dot() {
        let html = r#"<html><body>
            <h1 class="V8SH_pagetitle">ФейкТип.По ключам</h1>
            </body></html>"#;
        assert_eq!(
            scraper_parser::extract_constructor_variant_name(html),
            Some("По ключам".to_string())
        );
    }

    #[test]
    fn variant_name_returns_none_when_both_selectors_miss() {
        let html = "<html><body><p>unrelated</p></body></html>";
        assert!(scraper_parser::extract_constructor_variant_name(html).is_none());
    }

    fn synthetic_ctor_html(variant: &str, param_block: &str, description: &str) -> String {
        format!(
            r#"<html><body>
<h1 class="V8SH_pagetitle">ФейкТип.{variant}</h1>
<p class="V8SH_title">ФейкТип (FakeType)</p>
<p class="V8SH_heading">{variant}</p>
<div class="__SINCE_SHOW_STYLE__"><p class="not_used">Доступен, начиная с версии 8.3.10.</p></div>
<p class="V8SH_chapter">Синтаксис:</p>Новый ФейкТип(…)
<p class="V8SH_chapter">Параметры:</p>{param_block}
<p class="V8SH_chapter">Описание:</p><p>{description}</p>
<p class="V8SH_chapter">Использование в версии:</p><p class="V8SH_versionInfo">Доступен, начиная с версии 8.3.10.</p>
</body></html>"#
        )
    }

    #[test]
    fn parse_constructor_html_extracts_single_param() {
        let html = synthetic_ctor_html(
            "С одним параметром",
            r#"<div class="V8SH_rubric"> <p>&lt;Значение&gt; (необязательный)</div>Тип: <a href="x">Число</a>. <br>Описание значения.<br>"#,
            "Пример конструктора с одним параметром.",
        );

        let dir = unique_tmpdir("single");
        let path = dir.join("ctor42.html");
        fs::write(&path, &html).unwrap();

        let mut counter: u32 = 100;
        let info = parse_constructor_html(&path, "FakeType", &mut counter)
            .unwrap()
            .expect("got ctor info");
        assert_eq!(info.id, 100, "monotonic counter must hand out the current value");
        assert_eq!(counter, 101, "counter must advance after a successful parse");
        assert_eq!(info.type_name, "FakeType");
        assert_eq!(info.variant_name.as_deref(), Some("С одним параметром"));
        assert_eq!(info.parameters.len(), 1);
        assert_eq!(info.parameters[0].name, "Значение");
        assert_eq!(info.parameters[0].param_type.as_deref(), Some("Число"));
        assert!(info.parameters[0].is_optional);

        let docs = info.documentation.expect("docs present");
        assert!(docs.syntax.contains("Новый ФейкТип"));
        assert!(docs.description.contains("Пример конструктора"));
        assert_eq!(docs.param_descriptions.len(), 1);
        assert_eq!(docs.param_descriptions[0].name, "Значение");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_constructor_html_returns_none_without_variant_marker() {
        let html = "<html><body><h1 class=\"V8SH_pagetitle\">Plain</h1></body></html>";
        let dir = unique_tmpdir("no-variant");
        let path = dir.join("ctor7.html");
        fs::write(&path, html).unwrap();
        let mut counter: u32 = 0;
        assert!(parse_constructor_html(&path, "FakeType", &mut counter).unwrap().is_none());
        assert_eq!(counter, 0, "counter must not advance when parse returns None");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_type_constructors_returns_empty_when_dir_missing() {
        let dir = unique_tmpdir("no-ctors");
        let mut counter: u32 = 0;
        let got = parse_type_constructors(&dir, "FakeType", &mut counter).unwrap();
        assert!(got.is_empty());
        assert_eq!(counter, 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_type_constructors_picks_up_multi_overload_with_stable_order() {
        let type_dir = unique_tmpdir("multi");
        let ctors_dir = type_dir.join("ctors");
        fs::create_dir_all(&ctors_dir).unwrap();

        fs::write(
            ctors_dir.join("ctor2.html"),
            synthetic_ctor_html(
                "По значению",
                r#"<div class="V8SH_rubric"> <p>&lt;Значение&gt;</div>Тип: <a href="x">Строка</a>. <br>"#,
                "По значению.",
            ),
        )
        .unwrap();
        fs::write(
            ctors_dir.join("ctor1.html"),
            synthetic_ctor_html("Пустой", "", "Без параметров."),
        )
        .unwrap();
        fs::write(
            ctors_dir.join("ctor_Auto.html"),
            synthetic_ctor_html("По умолчанию", "", "Автоконструктор."),
        )
        .unwrap();
        fs::write(ctors_dir.join("readme.txt"), "noise").unwrap();

        let mut counter: u32 = 500;
        let got = parse_type_constructors(&type_dir, "FakeType", &mut counter).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, 500);
        assert_eq!(got[0].variant_name.as_deref(), Some("Пустой"));
        assert_eq!(got[1].id, 501);
        assert_eq!(got[1].variant_name.as_deref(), Some("По значению"));
        assert_eq!(got[2].id, 502);
        assert_eq!(got[2].variant_name.as_deref(), Some("По умолчанию"));
        assert_eq!(counter, 503, "counter must advance once per parsed ctor");

        fs::remove_dir_all(&type_dir).ok();
    }

    fn synthetic_page_with_syntax(syntax_body: &str, rubric_inner: &str) -> String {
        format!(
            r#"<html><body>
<h1 class="V8SH_pagetitle">Глобальный контекст.Тест (Global context.Test)</h1>
<p class="V8SH_chapter">Синтаксис:</p>{syntax_body}
<p class="V8SH_chapter">Параметры:</p><div class="V8SH_rubric"> <p>{rubric_inner}</div>Тип: <a href="x">Произвольный</a>.<br>
<p class="V8SH_chapter">Описание:</p><p>desc</p>
</body></html>"#
        )
    }

    #[test]
    fn detector_marks_separate_bracket_ellipsis() {
        let html = synthetic_page_with_syntax(
            "Тест(&lt;Значение1>,...,<ЗначениеN&gt;)",
            "&lt;Значение1>,...,<ЗначениеN&gt; (обязательный)",
        );
        let mut params = scraper_parser::extract_parameters(&html);
        assert!(!params.is_empty(), "rubric must yield at least one param");
        mark_trailing_variadic_from_syntax(&mut params, &html);
        assert!(
            params.last().unwrap().is_variadic,
            "Мин-style `<X1>,...,<XN>` must lift is_variadic"
        );
    }

    #[test]
    fn detector_marks_double_angle_quirk() {
        let html = synthetic_page_with_syntax(
            "Тест(&lt;<Значение1>,...,<ЗначениеN>&gt;)",
            "&lt;Значение1>,...,<ЗначениеN&gt; (обязательный)",
        );
        let mut params = scraper_parser::extract_parameters(&html);
        mark_trailing_variadic_from_syntax(&mut params, &html);
        assert!(
            params.last().unwrap().is_variadic,
            "ПродолжитьВызов-style `<<X1>,...,<XN>>` must still lift is_variadic"
        );
    }

    #[test]
    fn detector_skips_single_bracket_idiom() {
        let html = synthetic_page_with_syntax(
            "Тест(&lt;Содержимое1,...,СодержимоеN&gt;)",
            "&lt;Содержимое1,...,СодержимоеN&gt; (обязательный)",
        );
        let mut params = scraper_parser::extract_parameters(&html);
        mark_trailing_variadic_from_syntax(&mut params, &html);
        assert!(
            !params.last().unwrap().is_variadic,
            "single-bracket idiom must NOT lift is_variadic — name idiom owns this shape"
        );
    }

    #[test]
    fn detector_skips_fixed_arity() {
        let html =
            synthetic_page_with_syntax("Тест(&lt;Параметр&gt;)", "&lt;Параметр&gt; (обязательный)");
        let mut params = scraper_parser::extract_parameters(&html);
        mark_trailing_variadic_from_syntax(&mut params, &html);
        assert!(!params.last().unwrap().is_variadic, "fixed-arity must NOT lift is_variadic");
    }

    #[test]
    fn detector_handles_empty_params_list() {
        let html = synthetic_page_with_syntax("Тест(&lt;X1&gt;,...,&lt;XN&gt;)", "&lt;X&gt;");
        let mut params: Vec<MethodParameter> = Vec::new();
        mark_trailing_variadic_from_syntax(&mut params, &html);
        assert!(params.is_empty(), "empty input must remain empty");
    }

    #[test]
    fn detector_handles_missing_syntax_chapter() {
        let html = r#"<html><body>
<h1 class="V8SH_pagetitle">FakeType</h1>
<p class="V8SH_chapter">Параметры:</p><div class="V8SH_rubric"> <p>&lt;X&gt; (обязательный)</div>Тип: <a href="x">Число</a>.
</body></html>"#;
        let mut params = scraper_parser::extract_parameters(html);
        mark_trailing_variadic_from_syntax(&mut params, html);
        assert!(
            !params.last().unwrap().is_variadic,
            "absent Синтаксис: chapter must keep is_variadic at false"
        );
    }

    #[test]
    fn parse_constructor_html_lifts_variadic_for_array_count_shape() {
        let html = synthetic_ctor_html(
            "По количеству элементов",
            r#"<div class="V8SH_rubric"> <p>&lt;КоличествоЭлементов1>,...,<КоличествоЭлементовN&gt; (необязательный)</div>Тип: <a href="x">Число</a>. <br>Размерности.<br>"#,
            "Создаёт массив указанной размерности.",
        );
        let html = html.replace(
            "<p class=\"V8SH_chapter\">Синтаксис:</p>Новый ФейкТип(…)",
            "<p class=\"V8SH_chapter\">Синтаксис:</p>Новый ФейкТип(&lt;КоличествоЭлементов1>,...,<КоличествоЭлементовN&gt;)",
        );

        let dir = unique_tmpdir("array-variadic");
        let path = dir.join("ctor13.html");
        fs::write(&path, &html).unwrap();
        let mut counter: u32 = 0;
        let info =
            parse_constructor_html(&path, "FakeType", &mut counter).unwrap().expect("ctor info");
        assert_eq!(info.parameters.len(), 1);
        assert!(
            info.parameters[0].is_variadic,
            "Array's count-list ctor must mark its sole param as variadic"
        );
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("html-parser-prop-{tag}-{pid}-{ts}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    fn synthetic_property_page(title: &str) -> String {
        format!(
            r#"<html><body>
<h1 class="V8SH_pagetitle">{title}</h1>
<p class="V8SH_chapter">Использование:</p><p>Чтение и запись.</p>
<p class="V8SH_chapter">Описание:</p><p>Тип: Булево. Свойство для теста.</p>
</body></html>"#
        )
    }

    fn parse(title: &str, type_name_en: &str) -> Option<PropertyInfo> {
        let dir = unique_tmpdir("title");
        let path = dir.join("Prop.html");
        fs::write(&path, synthetic_property_page(title)).unwrap();
        let mut counter: u32 = 0;
        let info = parse_property_html(&path, type_name_en, &mut counter).unwrap();
        fs::remove_dir_all(&dir).ok();
        info
    }

    #[test]
    fn scalar_property_title_extracts_property_name() {
        let info = parse("Запрос.Параметры (Query.Parameters)", "Query")
            .expect("scalar property page must parse");
        assert_eq!(info.name, "Параметры");
        assert_eq!(info.english_name, "Parameters");
        assert_eq!(info.type_name, "Query");
    }

    #[test]
    fn composite_property_title_extracts_rightmost_segment() {
        let info = parse(
            "РегистрСведенийНаборЗаписей.&lt;Имя регистра сведений&gt;.ДополнительныеСвойства \
             (InformationRegisterRecordSet.&lt;Information register name&gt;.AdditionalProperties)",
            "InformationRegisterRecordSet",
        )
        .expect("composite property page must parse");
        assert_eq!(info.name, "ДополнительныеСвойства");
        assert_eq!(info.english_name, "AdditionalProperties");
        assert_eq!(info.type_name, "InformationRegisterRecordSet");
    }

    #[test]
    fn composite_property_with_placeholder_segment_still_parses_real_name() {
        let info = parse(
            "РегистрСведенийНаборЗаписей.&lt;Имя регистра сведений&gt;.Отбор \
             (InformationRegisterRecordSet.&lt;Information register name&gt;.Filter)",
            "InformationRegisterRecordSet",
        )
        .expect("composite property with bracketed qualifier must parse");
        assert_eq!(info.name, "Отбор");
        assert_eq!(info.english_name, "Filter");
    }

    #[test]
    fn composite_placeholder_property_name_is_still_dropped() {
        let info = parse("Структура.&lt;Имя ключа&gt; (Structure.&lt;Key name&gt;)", "Structure");
        assert!(info.is_none(), "placeholder-as-property-name must still be dropped");
    }
}

#[cfg(test)]
mod hbk_corpus_tests {
    use super::*;

    fn method<'a>(data: &'a PlatformData, english: &str) -> &'a MethodInfo {
        data.methods.iter().find(|m| m.english_name == english).unwrap()
    }

    #[test]
    fn archive_pair_yields_the_corpus_of_its_pages() {
        let dir = tempfile::tempdir().unwrap();
        let platform = dir.path().join("8.3.27.9");
        fixture_hbk::write_pair(&platform, &fixture_hbk::shcntx_pages(), 97);
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("keep.txt"), b"keep").unwrap();
        // A killed extraction: its lock file outlived its process.
        let abandoned = work.join(".hbk-extract-1-0-0");
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join(".lock"), b"locked").unwrap();
        // An owner that created its lock file but has not locked it yet.
        let starting = work.join(".hbk-extract-3-0-0");
        std::fs::create_dir_all(&starting).unwrap();
        std::fs::write(starting.join(".lock"), b"").unwrap();
        // A live one, however old: its lock is held.
        let live = work.join(".hbk-extract-2-0-0");
        std::fs::create_dir_all(&live).unwrap();
        let held = std::fs::File::create(live.join(".lock")).unwrap();
        held.lock().unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
        std::fs::File::open(&live).unwrap().set_modified(long_ago).unwrap();

        let data = extract_corpus_from_hbk(
            &platform.join("shcntx_ru.hbk"),
            &platform.join("shlang_ru.hbk"),
            &work,
        )
        .unwrap();
        let mut left: Vec<_> =
            std::fs::read_dir(&work).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(
            left,
            [".hbk-extract-2-0-0", ".hbk-extract-3-0-0", "keep.txt"],
            "the dead one goes; the live and the starting ones stay"
        );
        drop(held);

        let array = data.types.iter().find(|t| t.english_name == "Array").unwrap();
        assert_eq!(array.name, "Массив");
        let add = method(&data, "Add");
        assert_eq!((add.name.as_str(), add.type_name.as_str()), ("Добавить", "Array"));
        assert_eq!(add.parameters.len(), 1);
        assert!(add.parameters[0].is_optional);
        let docs = add.documentation.as_ref().unwrap();
        assert!(docs.description.contains("appends one element"), "{}", docs.description);
        assert_eq!(method(&data, "Count").return_type.as_deref(), Some("Число"));
        let strlen = data.global_functions.iter().find(|f| f.english_name == "StrLen").unwrap();
        assert_eq!((strlen.name.as_str(), strlen.english_name.as_str()), ("СтрДлина", "StrLen"));
    }

    #[test]
    fn changed_page_changes_the_corpus_and_broken_archive_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut pages = fixture_hbk::shcntx_pages();
        pages[1].1 = pages[1].1.replace("appends one element", "puts a value last");
        let platform = dir.path().join("p");
        fixture_hbk::write_pair(&platform, &pages, 4096);
        let data = extract_corpus_from_hbk(
            &platform.join("shcntx_ru.hbk"),
            &platform.join("shlang_ru.hbk"),
            &dir.path().join("w1"),
        )
        .unwrap();
        let description = &method(&data, "Add").documentation.as_ref().unwrap().description;
        assert!(description.contains("puts a value last"), "{description}");

        let shcntx = platform.join("shcntx_ru.hbk");
        let mut bytes = std::fs::read(&shcntx).unwrap();
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&shcntx, bytes).unwrap();
        let work = dir.path().join("w2");
        assert!(extract_corpus_from_hbk(&shcntx, &platform.join("shlang_ru.hbk"), &work).is_err());
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0, "a failure leaves nothing");

        // A valid container whose storage holds no context pages is not a corpus.
        let empty = dir.path().join("empty");
        fixture_hbk::write_pair(
            &empty,
            &[("objects/readme.txt".to_owned(), "none".to_owned())],
            4096,
        );
        let error = extract_corpus_from_hbk(
            &empty.join("shcntx_ru.hbk"),
            &empty.join("shlang_ru.hbk"),
            &dir.path().join("w3"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no platform types"), "{error}");
    }
}
