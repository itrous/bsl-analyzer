//! CLI flags declaring the project source set — the main configuration root,
//! the extensions analyzed alongside it and the external objects analyzed
//! against it.
//!
//! These mirror the `[source]` section of the config file where the CLI can
//! express the same information. Named extensions without configured scope use
//! legacy mode and produce one project-level warning.
//!
//! Where the CLI *is* stricter than the file: anything typed explicitly must
//! either work or fail loudly. A mistyped path in a config file may be a stale
//! entry someone else left behind, but a mistyped path in argv is this
//! invocation's mistake, and silently analyzing a different source set instead
//! is worse than refusing to start.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use clap::Args;
use project_model::{ExtensionDecl, ExternalDecl, SourceSetOverride, StructuredExtensionDecl};
use stdx::case::fold_lower_per_char;

/// Source-set flags shared by every command that builds a project from argv.
#[derive(Debug, Clone, Default, Args)]
pub struct SourceSetArgs {
    /// Root of the main configuration, relative to the project root (the
    /// `Configuration.xml`-bearing directory, e.g. `src/cf`).
    #[arg(long = "configuration-root", value_name = "PATH")]
    pub configuration_root: Option<String>,

    /// Extension to analyze alongside the main configuration. Repeat per
    /// extension. A bare `PATH` behaves like a plain string entry in the config
    /// and may use a `*` glob in its final segment. `NAME=PATH` supplies a
    /// stable name and can own `--extension-depends-on` edges; without a scope
    /// flag it uses legacy mode.
    #[arg(long = "extension", value_name = "NAME=PATH|PATH")]
    pub extensions: Vec<String>,

    /// Declare that an extension uses another extension's API:
    /// `NAME=DEP[,DEP...]`. Repeatable, and repeats for the same owner
    /// accumulate. Both sides must be names declared via `--extension NAME=PATH`.
    #[arg(long = "extension-depends-on", value_name = "NAME=DEP[,DEP...]")]
    pub extension_depends_on: Vec<String>,

    /// Analyze no extensions at all, overriding both a configured list and the
    /// conventional `src/cfe/*` discovery.
    #[arg(long = "no-extensions", conflicts_with = "extensions")]
    pub no_extensions: bool,

    /// External data processor or report (an EPF/ERF export) analyzed against
    /// the main configuration: `NAME=PATH`, where PATH holds the export's
    /// `<Name>.xml` beside its `<Name>/` directory. Repeat per object. Only the
    /// named form exists, and nothing about a declared one is lenient.
    #[arg(long = "external", value_name = "NAME=PATH")]
    pub externals: Vec<String>,

    /// Narrow an external object's view to the closure of these extensions:
    /// `NAME=DEP[,DEP...]`, or `NAME=` for the base alone. Repeatable, and
    /// repeats for the same owner accumulate. The owner is a name declared via
    /// `--external NAME=PATH`, every DEP one declared via `--extension NAME=PATH`.
    /// Without this flag an external object sees every extension.
    #[arg(long = "external-depends-on", value_name = "NAME=DEP[,DEP...]")]
    pub external_depends_on: Vec<String>,

    /// Analyze no external objects at all, overriding both a configured list
    /// and the conventional `src/epf/*`, `src/erf/*` discovery.
    #[arg(long = "no-externals", conflicts_with = "externals")]
    pub no_externals: bool,
}

pub enum SourceSetArgsError {
    ConfigurationRootNotFound { value: String, expected: PathBuf },
    EmptyExtensionName { value: String },
    EmptyExtensionPath { value: String },
    DuplicateExtensionName { first: String, second: String },
    DuplicateExtensionPath { first: String, second: String, path: PathBuf },
    MalformedDependsOn { value: String },
    UnknownDependsOnOwner { name: String },
    UnknownDependsOnTarget { owner: String, name: String },
    AmbiguousExtensionValue { value: String },
    EmptyDependsOnTarget { value: String },
    MalformedExternal { value: String },
    DuplicateExternalPath { first: String, second: String, path: PathBuf },
    MalformedExternalDependsOn { value: String },
    UnknownExternalDependsOnOwner { name: String },
    UnknownExternalDependsOnTarget { owner: String, name: String },
}

impl fmt::Display for SourceSetArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigurationRootNotFound { value, expected } => write!(
                f,
                "--configuration-root {value}: no Configuration.xml at {}",
                expected.display()
            ),
            Self::EmptyExtensionName { value } => {
                write!(f, "--extension {value}: the name before '=' is empty")
            }
            Self::EmptyExtensionPath { value } => {
                write!(f, "--extension {value}: the path is empty")
            }
            Self::DuplicateExtensionName { first, second } => write!(
                f,
                "--extension {first} and --extension {second} use the same extension name (names are case-insensitive)"
            ),
            Self::DuplicateExtensionPath { first, second, path } => write!(
                f,
                "--extension {first} and --extension {second} name the same directory {}",
                path.display()
            ),
            Self::MalformedDependsOn { value } => {
                write!(f, "--extension-depends-on {value}: expected NAME=DEP[,DEP...]")
            }
            Self::UnknownDependsOnOwner { name } => write!(
                f,
                "--extension-depends-on {name}=...: {name} is not declared by any --extension NAME=PATH"
            ),
            Self::UnknownDependsOnTarget { owner, name } => write!(
                f,
                "--extension-depends-on {owner}={name}: {name} is not declared by any --extension NAME=PATH"
            ),
            Self::AmbiguousExtensionValue { value } => write!(
                f,
                "--extension {value}: reads both as a directory of that name and as NAME=PATH, \
                 and both exist; name it explicitly, as in --extension NAME={value}"
            ),
            Self::EmptyDependsOnTarget { value } => {
                write!(f, "--extension-depends-on {value}: a dependency name is empty")
            }
            Self::MalformedExternal { value } => {
                write!(f, "--external {value}: expected NAME=PATH with both parts non-empty")
            }
            Self::DuplicateExternalPath { first, second, path } => write!(
                f,
                "{first} and --external {second} name the same directory {}",
                path.display()
            ),
            Self::MalformedExternalDependsOn { value } => write!(
                f,
                "--external-depends-on {value}: expected NAME=DEP[,DEP...], or NAME= for the \
                 base alone; no name may be empty"
            ),
            Self::UnknownExternalDependsOnOwner { name } => write!(
                f,
                "--external-depends-on {name}=...: {name} is not declared by any --external NAME=PATH"
            ),
            Self::UnknownExternalDependsOnTarget { owner, name } => write!(
                f,
                "--external-depends-on {owner}={name}: {name} is not declared by any --extension NAME=PATH"
            ),
        }
    }
}

// Delegated rather than derived: `main` returns `Result`, and the runtime
// renders the error with `Debug`. A derived one would dump struct fields at a
// user who mistyped a path, when the whole point of failing fast here is to say
// what is wrong in a sentence.
impl fmt::Debug for SourceSetArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for SourceSetArgsError {}

impl SourceSetArgs {
    /// True when no source-set flag was given, i.e. the config file and its own
    /// discovery decide everything.
    pub fn is_empty(&self) -> bool {
        self.configuration_root.is_none()
            && self.extensions.is_empty()
            && self.extension_depends_on.is_empty()
            && !self.no_extensions
            && self.externals.is_empty()
            && self.external_depends_on.is_empty()
            && !self.no_externals
    }

    /// Re-emits the flags as argv, for handing this process's source set to a
    /// child that must resolve the same project.
    ///
    /// Always in the `--option=value` form. These options do not accept
    /// hyphen-leading values as a separate argument, so a directory whose name
    /// starts with `-` — accepted by this process only because the caller wrote
    /// `--option=-name` — would come back to the child as a stray flag and kill
    /// its parse. In broker mode that costs the whole launch: the proxy retries
    /// the spawn until its timeout and then falls back to stdio.
    pub fn to_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(ref root) = self.configuration_root {
            out.push(format!("--configuration-root={root}"));
        }
        for value in &self.extensions {
            out.push(format!("--extension={value}"));
        }
        for value in &self.extension_depends_on {
            out.push(format!("--extension-depends-on={value}"));
        }
        if self.no_extensions {
            out.push("--no-extensions".to_owned());
        }
        for value in &self.externals {
            out.push(format!("--external={value}"));
        }
        for value in &self.external_depends_on {
            out.push(format!("--external-depends-on={value}"));
        }
        if self.no_externals {
            out.push("--no-externals".to_owned());
        }
        out
    }

    /// Whether `--extension` or `--no-extensions` claimed the extension list.
    /// `--extension-depends-on` alone cannot: it only annotates entries the
    /// same invocation declared.
    pub fn declares_extensions(&self) -> bool {
        self.no_extensions || !self.extensions.is_empty()
    }

    /// Whether `--external` or `--no-externals` claimed the external list.
    pub fn declares_externals(&self) -> bool {
        self.no_externals || !self.externals.is_empty()
    }

    /// Validates the flags and turns them into an override applied to the
    /// project config. `root` is the project root the relative paths resolve
    /// against — the same base the config file's own paths use.
    pub fn resolve(&self, root: &Path) -> Result<SourceSetOverride, SourceSetArgsError> {
        let configuration_root = self.resolve_configuration_root(root)?;
        let externals = self.resolve_externals(root)?;
        let extensions = self.resolve_extensions(root)?;
        Ok(SourceSetOverride { configuration_root, extensions, externals })
    }

    /// `--external NAME=PATH` entries as declarations. `None` when neither
    /// `--external` nor `--no-externals` was given, so the config file's list
    /// (or discovery) stays.
    ///
    /// Only the split, the duplicate check and the `--external-depends-on`
    /// binding live here. Whether PATH is one external object export is the
    /// project model's verdict, reported through its own error when the project
    /// is built.
    fn resolve_externals(
        &self,
        root: &Path,
    ) -> Result<Option<Vec<ExternalDecl>>, SourceSetArgsError> {
        if !self.declares_externals() {
            // Nothing here can own a dependency, as with extensions.
            if let Some((owner, _)) = self.parse_external_dependencies()?.into_iter().next() {
                return Err(SourceSetArgsError::UnknownExternalDependsOnOwner { name: owner });
            }
            return Ok(None);
        }
        if self.no_externals {
            if let Some((owner, _)) = self.parse_external_dependencies()?.into_iter().next() {
                return Err(SourceSetArgsError::UnknownExternalDependsOnOwner { name: owner });
            }
            return Ok(Some(Vec::new()));
        }
        // Canonical path → the flag spelling that claimed it, across BOTH lists:
        // the same directory as an extension and as an external would be two
        // roots for one tree.
        let mut claimed: HashMap<PathBuf, String> = HashMap::new();
        for value in &self.extensions {
            let (_, path) = split_extension_value(value, root)?;
            if !path.contains('*') {
                let resolved = root.join(&path);
                claimed.insert(
                    std::fs::canonicalize(&resolved).unwrap_or(resolved),
                    format!("--extension {value}"),
                );
            }
        }
        let mut decls = Vec::with_capacity(self.externals.len());
        for value in &self.externals {
            let Some((name, path)) = value.split_once('=') else {
                return Err(SourceSetArgsError::MalformedExternal { value: value.clone() });
            };
            let name = name.trim();
            if name.is_empty() || path.is_empty() {
                return Err(SourceSetArgsError::MalformedExternal { value: value.clone() });
            }
            let resolved = root.join(path);
            let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
            if let Some(first) = claimed.get(&canonical) {
                return Err(SourceSetArgsError::DuplicateExternalPath {
                    first: first.clone(),
                    second: value.clone(),
                    path: canonical,
                });
            }
            claimed.insert(canonical, format!("--external {value}"));
            decls.push(ExternalDecl {
                name: name.to_string(),
                path: path.to_string(),
                depends_on: None,
            });
        }

        // Targets must be names of named extensions, as for extension edges:
        // the topology would resolve a target against a bare entry's derived
        // directory name, which this flag must not bind to.
        let extension_names: std::collections::HashSet<String> = self
            .extensions
            .iter()
            .filter_map(|value| split_extension_value(value, root).ok().and_then(|(name, _)| name))
            .map(|name| fold_lower_per_char(&name))
            .collect();
        for (owner, targets) in self.parse_external_dependencies()? {
            let key = fold_lower_per_char(&owner);
            let Some(decl) = decls.iter_mut().find(|decl| fold_lower_per_char(&decl.name) == key)
            else {
                return Err(SourceSetArgsError::UnknownExternalDependsOnOwner { name: owner });
            };
            for target in &targets {
                if !extension_names.contains(&fold_lower_per_char(target)) {
                    return Err(SourceSetArgsError::UnknownExternalDependsOnTarget {
                        owner,
                        name: target.clone(),
                    });
                }
            }
            decl.depends_on.get_or_insert_with(Vec::new).extend(targets);
        }
        Ok(Some(decls))
    }

    /// Owner → declared targets of `--external-depends-on`, in declaration
    /// order, repeats of one owner accumulated. `NAME=` yields an owner with no
    /// targets: the base alone, which is not the same as no flag.
    fn parse_external_dependencies(
        &self,
    ) -> Result<Vec<(String, Vec<String>)>, SourceSetArgsError> {
        let mut owners: Vec<(String, Vec<String>)> = Vec::new();
        for value in &self.external_depends_on {
            let malformed =
                || SourceSetArgsError::MalformedExternalDependsOn { value: value.clone() };
            let (owner, targets) = value.split_once('=').ok_or_else(malformed)?;
            let owner = owner.trim();
            if owner.is_empty() {
                return Err(malformed());
            }
            let mut parsed = Vec::new();
            if !targets.trim().is_empty() {
                for target in targets.split(',') {
                    let target = target.trim();
                    if target.is_empty() {
                        return Err(malformed());
                    }
                    parsed.push(target.to_string());
                }
            }
            let key = fold_lower_per_char(owner);
            match owners.iter_mut().find(|(seen, _)| fold_lower_per_char(seen) == key) {
                Some((_, existing)) => existing.extend(parsed),
                None => owners.push((owner.to_string(), parsed)),
            }
        }
        Ok(owners)
    }

    fn resolve_configuration_root(
        &self,
        root: &Path,
    ) -> Result<Option<String>, SourceSetArgsError> {
        let Some(value) = self.configuration_root.as_ref() else {
            return Ok(None);
        };
        let candidate = root.join(value);
        // `is_file`, not `exists`: a directory (or a fifo) named
        // `Configuration.xml` satisfies mere existence, and the root would be
        // accepted as a configuration it cannot possibly be.
        if root_configuration_xml(&candidate).is_none() {
            // The config file's own key degrades to auto-discovery here; a flag
            // must not, or the command would quietly analyze whichever
            // configuration the search happens to find instead of the named one.
            return Err(SourceSetArgsError::ConfigurationRootNotFound {
                value: value.clone(),
                expected: candidate,
            });
        }
        Ok(Some(value.clone()))
    }

    fn resolve_extensions(
        &self,
        root: &Path,
    ) -> Result<Option<Vec<ExtensionDecl>>, SourceSetArgsError> {
        if !self.declares_extensions() || self.no_extensions {
            // Nothing here can own a dependency: either no extension was
            // declared at all, or the list is explicitly empty. Rejecting beats
            // silently dropping the edge the caller asked for.
            if let Some((owner, _)) = self.parse_dependencies()?.into_iter().next() {
                return Err(SourceSetArgsError::UnknownDependsOnOwner { name: owner });
            }
            return Ok(if self.no_extensions { Some(Vec::new()) } else { None });
        }

        let mut decls: Vec<ExtensionDecl> = Vec::with_capacity(self.extensions.len());
        // Folded name → original spelling, matching how the topology compares names.
        let mut named: HashMap<String, String> = HashMap::new();
        let mut named_indices = HashMap::new();
        // Canonical path → the spelling that claimed it, for the duplicate check.
        let mut claimed: HashMap<PathBuf, String> = HashMap::new();

        for value in &self.extensions {
            let (name, path) = split_extension_value(value, root)?;
            if let Some(ref name) = name {
                let key = fold_lower_per_char(name);
                if let Some(first) = named.get(&key) {
                    return Err(SourceSetArgsError::DuplicateExtensionName {
                        first: first.clone(),
                        second: name.clone(),
                    });
                }
                named.insert(key, name.clone());
            }

            // Globs describe a set, so overlapping one with an explicit entry is
            // legitimate and left to the model's own de-duplication. Only
            // literal spellings are held to "name each directory once".
            if !path.contains('*') {
                let resolved = root.join(&path);
                let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
                if let Some(first) = claimed.get(&canonical) {
                    return Err(SourceSetArgsError::DuplicateExtensionPath {
                        first: first.clone(),
                        second: value.clone(),
                        path: canonical,
                    });
                }
                claimed.insert(canonical, value.clone());
            }

            if let Some(name) = name {
                named_indices.insert(fold_lower_per_char(&name), decls.len());
                decls.push(ExtensionDecl::Structured(StructuredExtensionDecl {
                    name,
                    path,
                    depends_on: Vec::new(),
                    scope: None,
                }));
            } else {
                decls.push(ExtensionDecl::Path(path));
            }
        }

        let dependencies = self.parse_dependencies()?;
        for (owner, targets) in &dependencies {
            if !named.contains_key(&fold_lower_per_char(owner)) {
                return Err(SourceSetArgsError::UnknownDependsOnOwner { name: owner.clone() });
            }
            // Targets are checked too, not just the owner. The topology would
            // happily resolve a target against the name a bare entry derives
            // from its directory, which is exactly the binding this flag must
            // not create: that name is incidental and collides with any other
            // directory spelled the same.
            for target in targets {
                if !named.contains_key(&fold_lower_per_char(target)) {
                    return Err(SourceSetArgsError::UnknownDependsOnTarget {
                        owner: owner.clone(),
                        name: target.clone(),
                    });
                }
            }
        }

        for (owner, targets) in dependencies {
            let index = named_indices[&fold_lower_per_char(&owner)];
            let ExtensionDecl::Structured(decl) = &mut decls[index] else { unreachable!() };
            decl.depends_on.extend(targets);
        }

        Ok(Some(decls))
    }

    /// Owner → declared targets, in declaration order. Repeats of the same
    /// owner accumulate instead of the last flag winning, so
    /// `--extension-depends-on T=A --extension-depends-on T=B` and
    /// `--extension-depends-on T=A,B` describe the same edges.
    fn parse_dependencies(&self) -> Result<Vec<(String, Vec<String>)>, SourceSetArgsError> {
        let mut owners: Vec<(String, Vec<String>)> = Vec::new();

        for value in &self.extension_depends_on {
            let (owner, targets) = value
                .split_once('=')
                .ok_or_else(|| SourceSetArgsError::MalformedDependsOn { value: value.clone() })?;
            let owner = owner.trim();
            if owner.is_empty() {
                return Err(SourceSetArgsError::MalformedDependsOn { value: value.clone() });
            }
            let mut parsed = Vec::new();
            for target in targets.split(',') {
                let target = target.trim();
                if target.is_empty() {
                    return Err(SourceSetArgsError::EmptyDependsOnTarget { value: value.clone() });
                }
                parsed.push(target.to_string());
            }

            let key = fold_lower_per_char(owner);
            match owners.iter_mut().find(|(seen, _)| fold_lower_per_char(seen) == key) {
                Some((_, existing)) => existing.extend(parsed),
                None => owners.push((owner.to_string(), parsed)),
            }
        }

        Ok(owners)
    }
}

/// `relative` with `/` separators, the spelling config files use.
pub fn slash_path(relative: &Path) -> String {
    relative.to_string_lossy().replace('\\', "/")
}

/// Flag paths are written relative to `-s`, but the project resolves them against `project_root`;
/// prefixing keeps them meaning the same directories when a discovered config moved that root.
pub fn rebase_override(
    mut over: SourceSetOverride,
    source_dir: &Path,
    project_root: &Path,
) -> SourceSetOverride {
    let Ok(prefix) = source_dir.strip_prefix(project_root) else { return over };
    if prefix.as_os_str().is_empty() {
        return over;
    }
    let prefixed = |path: &str| slash_path(&prefix.join(path));
    over.configuration_root = over.configuration_root.map(|root| prefixed(&root));
    for decl in over.extensions.iter_mut().flatten() {
        match decl {
            ExtensionDecl::Path(path) => *path = prefixed(path),
            ExtensionDecl::Structured(structured) => structured.path = prefixed(&structured.path),
        }
    }
    for decl in over.externals.iter_mut().flatten() {
        decl.path = prefixed(&decl.path);
    }
    over
}

/// `NAME=PATH` splits on the first `=`; anything else is a bare path.
///
/// The split is unconditional, which costs one case: a directory whose own name
/// contains `=` cannot be given in the bare form, and combined with a glob it
/// is unreachable. That is deliberate. The two readings of `Named=cfe/*` — a
/// named entry with a glob, or a bare path under a directory called `Named=cfe`
/// — are textually identical, and deciding by whether a `*` is present picks
/// the bare one for `Named=cfe/*` too, which then matches nothing and drops the
/// extension without a word. A rare directory name that fails loudly beats a
/// common spelling that silently analyzes a different source set.
///
/// The name is trimmed and the path is not. A name is an identifier, where
/// surrounding blanks cannot be meaningful; a path is a filesystem path, where
/// they can — trimming it would quietly analyze a different directory than the
/// one that was passed.
fn split_extension_value(
    value: &str,
    root: &Path,
) -> Result<(Option<String>, String), SourceSetArgsError> {
    // The one case where both readings are live: the whole value names an
    // extension directory *and* the split names another one. Guessing here
    // would analyze a directory the caller did not ask for, so say so instead.
    if value.contains('=')
        && is_extension_dir(root, value)
        && value.split_once('=').is_some_and(|(_, path)| is_extension_dir(root, path))
    {
        return Err(SourceSetArgsError::AmbiguousExtensionValue { value: value.to_string() });
    }
    match value.split_once('=') {
        Some((name, path)) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(SourceSetArgsError::EmptyExtensionName { value: value.to_string() });
            }
            if path.is_empty() {
                return Err(SourceSetArgsError::EmptyExtensionPath { value: value.to_string() });
            }
            Ok((Some(name.to_string()), path.to_string()))
        }
        _ => {
            if value.is_empty() {
                return Err(SourceSetArgsError::EmptyExtensionPath { value: value.to_string() });
            }
            Ok((None, value.to_string()))
        }
    }
}

fn is_extension_dir(root: &Path, rel: &str) -> bool {
    root_configuration_xml(&root.join(rel)).is_some()
}

/// The directory's `Configuration.xml` as a real file, in whatever ASCII case
/// the tree spells it.
pub(crate) fn directory_has_configuration_xml(dir: &Path) -> bool {
    root_configuration_xml(dir).is_some()
}

fn root_configuration_xml(dir: &Path) -> Option<std::path::PathBuf> {
    bsl_conventions::find_child_ci(
        dir,
        bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
    )
    .filter(|p| p.is_file())
}

/// Where a resolved source-set field actually came from. Reported per field
/// because the root and the extension list are decided independently: a flag
/// may claim one while the other still comes from the file or from discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceProvider {
    Cli,
    ConfigFile,
    AutoDiscovery,
}

impl SourceProvider {
    pub fn label(self, config_file: &Path) -> String {
        match self {
            Self::Cli => "cli".to_string(),
            Self::ConfigFile => config_file.display().to_string(),
            Self::AutoDiscovery => "auto-discovery".to_string(),
        }
    }
}

/// Provider of the main configuration root. A configured root that does not
/// hold a `Configuration.xml` is not the answer: the model warns and falls
/// through to discovery, so reporting the file here would name a source that
/// did not decide anything.
pub fn configuration_root_provider(
    args: &SourceSetArgs,
    config_root: Option<&str>,
    project_root: &Path,
) -> SourceProvider {
    if args.configuration_root.is_some() {
        return SourceProvider::Cli;
    }
    match config_root {
        Some(value) if root_configuration_xml(&project_root.join(value)).is_some() => {
            SourceProvider::ConfigFile
        }
        _ => SourceProvider::AutoDiscovery,
    }
}

/// Provider of the extension list. Unlike the root, an explicit list is
/// authoritative whether or not any entry resolves, so presence decides.
pub fn extensions_provider(
    args: &SourceSetArgs,
    config_extensions: Option<&Vec<ExtensionDecl>>,
) -> SourceProvider {
    if args.declares_extensions() {
        SourceProvider::Cli
    } else if config_extensions.is_some() {
        SourceProvider::ConfigFile
    } else {
        SourceProvider::AutoDiscovery
    }
}

/// Provider of the external object list, decided by presence like the
/// extension list: unset on both sides is the `src/epf/*`, `src/erf/*` search.
pub fn externals_provider(
    args: &SourceSetArgs,
    config_externals: Option<&Vec<ExternalDecl>>,
) -> SourceProvider {
    if args.declares_externals() {
        SourceProvider::Cli
    } else if config_externals.is_some() {
        SourceProvider::ConfigFile
    } else {
        SourceProvider::AutoDiscovery
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_external_flag_is_a_named_declaration_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = args(&["--external", "АРМ=src/epf/АРМ"]).resolve(dir.path()).unwrap();
        assert_eq!(
            resolved.externals,
            Some(vec![ExternalDecl {
                name: "АРМ".into(),
                path: "src/epf/АРМ".into(),
                depends_on: None
            }])
        );
        assert_eq!(resolved.extensions, None, "an external flag must not claim the extension list");

        assert!(args(&[]).resolve(dir.path()).unwrap().externals.is_none(), "unset stays unset");
        for bad in ["src/epf/АРМ", "=src/epf/АРМ", "АРМ=", " =p"] {
            let err = args(&["--external", bad]).resolve(dir.path()).unwrap_err();
            assert!(matches!(err, SourceSetArgsError::MalformedExternal { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn the_same_directory_as_extension_and_external_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        extension_dir(dir.path(), "shared");
        let err = args(&["--extension", "EXT=shared", "--external", "E=./shared"])
            .resolve(dir.path())
            .unwrap_err();
        assert!(matches!(err, SourceSetArgsError::DuplicateExternalPath { .. }), "{err}");
        let err = args(&["--external", "A=shared", "--external", "B=shared"])
            .resolve(dir.path())
            .unwrap_err();
        assert!(matches!(err, SourceSetArgsError::DuplicateExternalPath { .. }), "{err}");
    }

    #[test]
    fn external_flags_are_re_emitted_and_reported() {
        let flags = args(&["--external", "АРМ=src/epf/АРМ", "--external-depends-on", "АРМ=A"]);
        assert!(!flags.is_empty());
        assert_eq!(flags.to_args(), ["--external=АРМ=src/epf/АРМ", "--external-depends-on=АРМ=A"]);
        assert_eq!(externals_provider(&flags, None), SourceProvider::Cli);
        let none = args(&["--no-externals"]);
        assert!(!none.is_empty());
        assert_eq!(none.to_args(), ["--no-externals"]);
        assert_eq!(externals_provider(&none, None), SourceProvider::Cli);
        let declared = vec![ExternalDecl { name: "X".into(), path: "x".into(), depends_on: None }];
        assert_eq!(externals_provider(&args(&[]), Some(&declared)), SourceProvider::ConfigFile);
        assert_eq!(externals_provider(&args(&[]), Some(&vec![])), SourceProvider::ConfigFile);
        assert_eq!(externals_provider(&args(&[]), None), SourceProvider::AutoDiscovery);
    }

    #[test]
    fn no_externals_is_an_explicit_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = args(&["--no-externals"]).resolve(dir.path()).unwrap();
        assert_eq!(resolved.externals, Some(vec![]));
        assert_eq!(resolved.extensions, None, "the extension list is untouched");
        use clap::Parser;
        #[derive(Parser)]
        struct Harness {
            #[command(flatten)]
            source_set: SourceSetArgs,
        }
        assert!(
            Harness::try_parse_from(["h", "--no-externals", "--external", "A=a"]).is_err(),
            "the two flags conflict"
        );
    }

    #[test]
    fn external_depends_on_narrows_a_declared_external_to_named_extensions() {
        let dir = tempfile::tempdir().unwrap();
        extension_dir(dir.path(), "a");
        extension_dir(dir.path(), "b");
        let resolve = |flags: &[&str]| args(flags).resolve(dir.path());
        let base = ["--extension", "A=a", "--extension", "B=b", "--external", "E=e"];
        let with = |extra: &[&str]| {
            let mut flags = base.to_vec();
            flags.extend_from_slice(extra);
            resolve(&flags)
        };

        let valid = with(&[]).unwrap();
        assert_eq!(names(valid.extensions.as_ref().unwrap()), ["A", "B"]);
        let repeated = args(&[
            "--extension",
            "A=a",
            "--extension",
            "B=b",
            "--external",
            "E=e",
            "--external-depends-on",
            "e=A",
            "--external-depends-on",
            "E=b",
        ]);
        assert_eq!(
            repeated.parse_external_dependencies().unwrap(),
            vec![("e".to_owned(), vec!["A".to_owned(), "b".to_owned()])],
            "repeats accumulate and the owner matches case-insensitively"
        );
        let repeated = repeated.resolve(dir.path()).unwrap();
        let external = &repeated.externals.as_ref().unwrap()[0];
        let ExternalDecl { depends_on: Some(dependencies), .. } = external else {
            panic!("external E must retain its selected extension dependencies: {external:?}");
        };
        assert_eq!(dependencies, &["A".to_owned(), "b".to_owned()]);
        let base_only =
            args(&["--extension", "A=a", "--external", "E=e", "--external-depends-on", "E="]);
        assert_eq!(
            base_only.parse_external_dependencies().unwrap(),
            vec![("E".to_owned(), vec![])]
        );
        assert!(base_only.resolve(dir.path()).is_ok());

        let err = with(&["--external-depends-on", "E=C"]).unwrap_err();
        assert!(matches!(err, SourceSetArgsError::UnknownExternalDependsOnTarget { .. }), "{err}");
        let err = with(&["--external-depends-on", "F=A"]).unwrap_err();
        assert!(matches!(err, SourceSetArgsError::UnknownExternalDependsOnOwner { .. }), "{err}");
        for bad in ["E", "=A", "E=A,,B", "E=,"] {
            let err = with(&["--external-depends-on", bad]).unwrap_err();
            assert!(
                matches!(err, SourceSetArgsError::MalformedExternalDependsOn { .. }),
                "{bad}: {err}"
            );
        }
        // A bare extension's derived name is not a target.
        let err =
            resolve(&["--extension", "a", "--external", "E=e", "--external-depends-on", "E=a"])
                .unwrap_err();
        assert!(matches!(err, SourceSetArgsError::UnknownExternalDependsOnTarget { .. }), "{err}");
        // Without an external list nothing owns the edge.
        for flags in [
            &["--external-depends-on", "E=A"][..],
            &["--no-externals", "--external-depends-on", "E=A"][..],
        ] {
            let err = resolve(flags).unwrap_err();
            assert!(
                matches!(err, SourceSetArgsError::UnknownExternalDependsOnOwner { .. }),
                "{err}"
            );
        }
    }

    #[test]
    fn a_case_variant_configuration_xml_satisfies_root_probes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("CONFIGURATION.XML"), "<x/>").unwrap();
        assert!(
            super::root_configuration_xml(&base).is_some(),
            "явный --configuration-root на каталог с CONFIGURATION.XML проходит валидацию"
        );
        assert!(super::is_extension_dir(dir.path(), "base"));
    }

    use super::*;
    use tempfile::tempdir;

    fn args(flags: &[&str]) -> SourceSetArgs {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(flatten)]
            source_set: SourceSetArgs,
        }

        let mut argv = vec!["harness"];
        argv.extend_from_slice(flags);
        // `try_parse_from`, not `parse_from`: a rejected argv would otherwise
        // exit the test process outright, turning an assertion failure into an
        // unreadable abort.
        Harness::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("clap rejected {flags:?}: {e}"))
            .source_set
    }

    fn extension_dir(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Configuration.xml"), "<xml/>").unwrap();
    }

    fn names(decls: &[ExtensionDecl]) -> Vec<String> {
        decls
            .iter()
            .map(|decl| match decl {
                ExtensionDecl::Path(path) => path.clone(),
                ExtensionDecl::Structured(entry) => entry.name.clone(),
            })
            .collect()
    }

    #[test]
    fn cli_named_and_bare_extensions_resolve_in_legacy_mode() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "ext");
        extension_dir(dir.path(), "vendor/flag");

        assert_eq!(
            super::split_extension_value("Flag=vendor/flag", dir.path()).unwrap(),
            (Some("Flag".to_owned()), "vendor/flag".to_owned())
        );
        assert_eq!(
            super::split_extension_value("ext", dir.path()).unwrap(),
            (None, "ext".to_owned())
        );
        let bare = args(&["--extension", "ext"]).resolve(dir.path()).unwrap();
        assert_eq!(names(&bare.extensions.unwrap()), vec!["ext"]);
        let named = args(&["--extension", "Flag=vendor/flag"]).resolve(dir.path()).unwrap();
        assert!(matches!(
            &named.extensions.as_ref().unwrap()[0],
            ExtensionDecl::Structured(entry) if entry.name == "Flag" && entry.scope.is_none()
        ));

        let malformed_external =
            args(&["--extension", "Flag=vendor/flag", "--external", "not-a-name-path"])
                .resolve(dir.path())
                .unwrap_err();
        assert!(matches!(malformed_external, SourceSetArgsError::MalformedExternal { .. }));
        let duplicate_name = args(&["--extension", "Flag=vendor/flag", "--extension", "fLaG=ext"])
            .resolve(dir.path())
            .unwrap_err();
        assert!(matches!(duplicate_name, SourceSetArgsError::DuplicateExtensionName { .. }));
    }

    #[test]
    fn scope_less_cli_toml_and_api_declarations_share_topology_and_cache_identity() {
        let workspace = tempdir().unwrap();
        let cache = tempdir().unwrap();
        extension_dir(workspace.path(), "src/cf");
        extension_dir(workspace.path(), "vendor/A");
        extension_dir(workspace.path(), "vendor/B");
        let config_path = workspace.path().join("bsl-analyzer.toml");
        std::fs::write(
            &config_path,
            r#"
[source]
root = "src/cf"
extensions = [
  { name = "A", path = "vendor/A", dependsOn = ["B"] },
  { name = "B", path = "vendor/B" },
]
"#,
        )
        .unwrap();

        let declared_config = project_model::ProjectConfig::load_from_file(&config_path).unwrap();
        let declared =
            project_model::Project::with_config(workspace.path(), declared_config).unwrap();

        let mut cli_config = project_model::ProjectConfig::load_from_file(&config_path).unwrap();
        args(&[
            "--configuration-root",
            "src/cf",
            "--extension",
            "A=vendor/A",
            "--extension",
            "B=vendor/B",
            "--extension-depends-on",
            "A=B",
        ])
        .resolve(workspace.path())
        .unwrap()
        .apply_to(&mut cli_config);
        let cli = project_model::Project::with_config(workspace.path(), cli_config).unwrap();

        let mut api_config = project_model::ProjectConfig::default();
        project_model::SourceSetOverride {
            configuration_root: Some("src/cf".to_owned()),
            extensions: Some(vec![
                project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                    name: "A".to_owned(),
                    path: "vendor/A".to_owned(),
                    depends_on: vec!["B".to_owned()],
                    scope: None,
                }),
                project_model::ExtensionDecl::Structured(project_model::StructuredExtensionDecl {
                    name: "B".to_owned(),
                    path: "vendor/B".to_owned(),
                    depends_on: Vec::new(),
                    scope: None,
                }),
            ]),
            externals: None,
        }
        .apply_to(&mut api_config);
        let api = project_model::Project::with_config(workspace.path(), api_config).unwrap();

        let projects = [&declared, &cli, &api];
        let base = declared.configuration_path().unwrap();
        let physical = declared.extension_topology().fingerprint();
        let portable = declared.extension_topology().portable_fingerprint(base, workspace.path());
        let layouts = projects.map(|project| {
            mcp_server::WorkspaceCacheLayout::for_project(
                project,
                Some(cache.path()),
                workspace.path(),
                None,
            )
            .unwrap()
        });
        for project in &projects {
            assert_eq!(project.extension_topology().fingerprint(), physical);
            let topology = project.extension_topology();
            let extension_a = topology.nodes().iter().find(|node| node.name() == "A").unwrap();
            assert_eq!(
                extension_a
                    .depends_on()
                    .iter()
                    .map(|id| topology.node(*id).name())
                    .collect::<Vec<_>>(),
                ["B"]
            );
            assert_eq!(topology.portable_fingerprint(base, workspace.path()), portable);
        }
        assert_eq!(layouts[0].scope_stamp(), layouts[1].scope_stamp());
        assert_eq!(layouts[0].scope_stamp(), layouts[2].scope_stamp());
        assert_eq!(layouts[0].root(), layouts[1].root());
        assert_eq!(layouts[0].root(), layouts[2].root());
    }

    #[test]
    fn configuration_root_without_configuration_xml_is_refused() {
        let dir = tempdir().unwrap();
        // Present so that the model's own search would have found *something*:
        // the flag must not degrade into that search.
        extension_dir(dir.path(), "src/cf");

        let err = args(&["--configuration-root", "nope"]).resolve(dir.path()).unwrap_err();

        assert!(matches!(err, SourceSetArgsError::ConfigurationRootNotFound { .. }), "got {err}");
    }

    #[test]
    fn configuration_root_that_exists_is_accepted() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "src/cf");

        let resolved = args(&["--configuration-root", "src/cf"]).resolve(dir.path()).unwrap();

        assert_eq!(resolved.configuration_root.as_deref(), Some("src/cf"));
    }

    #[test]
    fn no_extensions_yields_the_explicit_opt_out() {
        let dir = tempdir().unwrap();

        let resolved = args(&["--no-extensions"]).resolve(dir.path()).unwrap();

        assert_eq!(resolved.extensions, Some(Vec::new()), "must be the opt-out, not 'unset'");
    }

    #[test]
    fn no_source_set_flags_leave_the_config_alone() {
        let dir = tempdir().unwrap();

        let resolved = args(&[]).resolve(dir.path()).unwrap();

        assert_eq!(resolved.configuration_root, None);
        assert_eq!(resolved.extensions, None, "unset must stay unset, not become an empty list");
    }

    #[test]
    fn dependency_owner_is_matched_case_insensitively() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "b");
        extension_dir(dir.path(), "t");

        // The topology compares folded names on both sides, so the CLI barrier
        // must not reject spellings the same model accepts from a config file.
        let cli = args(&[
            "--extension",
            "Base=b",
            "--extension",
            "Target=t",
            "--extension-depends-on",
            "TARGET=BASE",
        ]);
        assert_eq!(
            cli.parse_dependencies().unwrap(),
            vec![("TARGET".to_owned(), vec!["BASE".to_owned()])]
        );
        let resolved = cli.resolve(dir.path()).unwrap();
        assert!(
            matches!(&resolved.extensions.as_ref().unwrap()[1], ExtensionDecl::Structured(entry) if entry.depends_on == ["BASE"])
        );
    }

    #[test]
    fn dependency_targets_survive_both_spellings() {
        let dir = tempdir().unwrap();
        for name in ["a", "b", "t"] {
            extension_dir(dir.path(), name);
        }
        let decl = ["--extension", "A=a", "--extension", "B=b", "--extension", "T=t"];

        let inline = args(&[&decl[..], &["--extension-depends-on", "T=A,B"]].concat());
        let repeated = args(
            &[&decl[..], &["--extension-depends-on", "T=A", "--extension-depends-on", "T=B"]]
                .concat(),
        );

        let expected = vec![("T".to_owned(), vec!["A".to_owned(), "B".to_owned()])];
        assert_eq!(inline.parse_dependencies().unwrap(), expected);
        assert_eq!(
            repeated.parse_dependencies().unwrap(),
            expected,
            "repeats must accumulate rather than the last flag winning"
        );
        let resolved = inline.resolve(dir.path()).unwrap();
        assert!(
            matches!(&resolved.extensions.as_ref().unwrap()[2], ExtensionDecl::Structured(entry) if entry.depends_on == ["A", "B"])
        );
    }

    #[test]
    fn dependency_on_a_bare_path_entry_is_refused() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "t");
        extension_dir(dir.path(), "legacy");

        // The topology would resolve `legacy` against the name that bare entry
        // derives from its directory. That name is incidental, so binding to it
        // is exactly what this flag must not do.
        let err = args(&[
            "--extension",
            "T=t",
            "--extension",
            "legacy",
            "--extension-depends-on",
            "T=legacy",
        ])
        .resolve(dir.path())
        .unwrap_err();

        assert!(matches!(err, SourceSetArgsError::UnknownDependsOnTarget { .. }), "got {err}");
    }

    #[test]
    fn a_value_that_reads_both_ways_is_refused() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "wanted=real");
        extension_dir(dir.path(), "real");

        // Both readings name a real extension directory. Picking one silently
        // would analyze a directory the caller never asked for.
        let err = args(&["--extension", "wanted=real"]).resolve(dir.path()).unwrap_err();

        assert!(matches!(err, SourceSetArgsError::AmbiguousExtensionValue { .. }), "got {err}");
    }

    #[test]
    fn named_values_split_at_the_first_equals() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "wanted=real");
        extension_dir(dir.path(), "real");

        // Preserve the established first-equals syntax for named legacy overrides.
        assert_eq!(
            super::split_extension_value("W=wanted=real", dir.path()).unwrap(),
            (Some("W".to_owned()), "wanted=real".to_owned())
        );
        assert!(
            matches!(args(&["--extension", "W=wanted=real"]).resolve(dir.path()).unwrap().extensions.as_ref().unwrap()[0], ExtensionDecl::Structured(ref entry) if entry.path == "wanted=real" && entry.scope.is_none())
        );
    }

    #[test]
    fn only_one_live_reading_is_still_parsed_as_a_named_override() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "real");

        assert_eq!(
            super::split_extension_value("wanted=real", dir.path()).unwrap(),
            (Some("wanted".to_owned()), "real".to_owned())
        );
        assert!(
            matches!(args(&["--extension", "wanted=real"]).resolve(dir.path()).unwrap().extensions.as_ref().unwrap()[0], ExtensionDecl::Structured(ref entry) if entry.name == "wanted" && entry.scope.is_none())
        );
    }

    #[test]
    fn a_directory_named_configuration_xml_is_not_a_configuration() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("base/Configuration.xml")).unwrap();

        // Mere existence is satisfied by a directory or a fifo; the flag is
        // authoritative, so it has to insist on a file.
        let err = args(&["--configuration-root", "base"]).resolve(dir.path()).unwrap_err();

        assert!(matches!(err, SourceSetArgsError::ConfigurationRootNotFound { .. }), "got {err}");
    }

    #[test]
    fn a_named_glob_is_not_reinterpreted_as_a_bare_path() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "cfe/E");

        // Re-reading this value as a bare path would change its meaning and
        // could silently drop the extension. The named entry remains explicit.
        assert_eq!(
            super::split_extension_value("Named=cfe/*", dir.path()).unwrap(),
            (Some("Named".to_owned()), "cfe/*".to_owned())
        );
        assert!(
            matches!(args(&["--extension", "Named=cfe/*"]).resolve(dir.path()).unwrap().extensions.as_ref().unwrap()[0], ExtensionDecl::Structured(ref entry) if entry.path == "cfe/*" && entry.scope.is_none())
        );
    }

    #[test]
    fn surrounding_blanks_are_kept_in_paths_and_dropped_in_names() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), " ext ");

        let resolved =
            args(&["--extension", " ext ", "--extension", " Named = ext "]).resolve(dir.path());

        // The two spell the same directory, so the duplicate barrier fires —
        // proving the path kept its blanks while the name lost them.
        match resolved.unwrap_err() {
            SourceSetArgsError::DuplicateExtensionPath { .. } => {}
            other => panic!("got {other}"),
        }

        let single = args(&["--extension", " ext "]).resolve(dir.path()).unwrap();
        assert_eq!(
            single.extensions.unwrap(),
            vec![ExtensionDecl::Path(" ext ".to_string())],
            "a trimmed path would name a directory that was never passed"
        );
    }

    #[test]
    fn dependency_on_an_undeclared_owner_is_refused() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "y");

        let err = args(&["--extension", "Y=y", "--extension-depends-on", "Q=Y"])
            .resolve(dir.path())
            .unwrap_err();

        assert!(matches!(err, SourceSetArgsError::UnknownDependsOnOwner { .. }), "got {err}");
    }

    #[test]
    fn dependency_without_any_named_extension_is_refused() {
        let dir = tempdir().unwrap();

        let err = args(&["--extension-depends-on", "T=Y"]).resolve(dir.path()).unwrap_err();

        assert!(matches!(err, SourceSetArgsError::UnknownDependsOnOwner { .. }), "got {err}");
    }

    #[test]
    fn the_same_directory_named_twice_is_refused_in_either_order() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "ext");

        // The second order is the one the model itself accepts silently, so a
        // barrier that only covers the first would look fine on the model's own
        // tests and still let a duplicate through.
        for flags in [
            ["--extension", "ext", "--extension", "Named=ext"],
            ["--extension", "Named=ext", "--extension", "ext"],
        ] {
            let err = args(&flags).resolve(dir.path()).unwrap_err();
            assert!(
                matches!(err, SourceSetArgsError::DuplicateExtensionPath { .. }),
                "{flags:?} got {err}"
            );
        }
    }

    #[test]
    fn duplicate_detection_sees_through_path_spellings() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "ext");

        let err = args(&["--extension", "Named=ext", "--extension", "./ext"])
            .resolve(dir.path())
            .unwrap_err();

        assert!(matches!(err, SourceSetArgsError::DuplicateExtensionPath { .. }), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_detection_sees_through_symlinks() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "ext");
        std::os::unix::fs::symlink(dir.path().join("ext"), dir.path().join("alias")).unwrap();

        // A lexical normalization would call these two different directories;
        // the model canonicalizes, so the barrier has to as well.
        let err = args(&["--extension", "Named=ext", "--extension", "alias"])
            .resolve(dir.path())
            .unwrap_err();

        assert!(matches!(err, SourceSetArgsError::DuplicateExtensionPath { .. }), "got {err}");
    }

    #[test]
    fn a_glob_may_overlap_an_explicit_entry() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "cfe/base");

        // A glob describes a set; overlapping it with a named entry is ordinary
        // usage, and de-duplicating it belongs to the model, not to a refusal.
        let resolved =
            args(&["--extension", "cfe/*", "--extension", "cfe/base"]).resolve(dir.path()).unwrap();

        assert_eq!(resolved.extensions.unwrap().len(), 2, "both path entries reach the model");
    }

    #[test]
    fn re_emitted_flags_survive_a_value_that_starts_with_a_hyphen() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "-cf");
        extension_dir(dir.path(), "-ext");

        // Accepted here only as `--option=-value`; re-emitted as a separate
        // argument it would come back to the child as a stray flag, and in
        // broker mode that costs the whole launch.
        let parsed = args(&["--configuration-root=-cf", "--extension=-ext"]);
        let emitted = parsed.to_args();
        let round_tripped: Vec<&str> = emitted.iter().map(String::as_str).collect();
        let reparsed = args(&round_tripped);

        assert_eq!(reparsed.configuration_root.as_deref(), Some("-cf"));
        assert_eq!(reparsed.extensions, vec!["-ext".to_string()]);
    }

    #[test]
    fn providers_are_reported_per_field() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "src/cf");
        let config_extensions = vec![ExtensionDecl::Path("ext".to_string())];

        let root_from_cli = args(&["--configuration-root", "src/cf"]);
        assert_eq!(
            configuration_root_provider(&root_from_cli, None, dir.path()),
            SourceProvider::Cli
        );
        assert_eq!(
            extensions_provider(&root_from_cli, Some(&config_extensions)),
            SourceProvider::ConfigFile,
            "a root flag must not claim the extension list too"
        );

        let extensions_from_cli = args(&["--extension", "ext"]);
        assert_eq!(
            configuration_root_provider(&extensions_from_cli, Some("src/cf"), dir.path()),
            SourceProvider::ConfigFile,
            "an extension flag must not claim the root too"
        );
        assert_eq!(
            extensions_provider(&extensions_from_cli, Some(&config_extensions)),
            SourceProvider::Cli
        );
    }

    #[test]
    fn a_configured_root_that_does_not_resolve_is_reported_as_discovery() {
        let dir = tempdir().unwrap();
        extension_dir(dir.path(), "src/cf");

        // The model warns and searches instead of using it, so naming the file
        // as the provider would credit a source that decided nothing.
        assert_eq!(
            configuration_root_provider(&args(&[]), Some("missing"), dir.path()),
            SourceProvider::AutoDiscovery
        );
        assert_eq!(
            configuration_root_provider(&args(&[]), Some("src/cf"), dir.path()),
            SourceProvider::ConfigFile
        );
        assert_eq!(
            configuration_root_provider(&args(&[]), None, dir.path()),
            SourceProvider::AutoDiscovery
        );
    }
}
