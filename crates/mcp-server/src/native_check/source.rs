use bsl_conventions::ConventionalName;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ServerLocation<'a> {
    pub host: &'a str,
    pub base: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptVariant {
    Russian,
    English,
}

pub(crate) fn parse_script_variant(
    xml_root: &Path,
) -> Result<ScriptVariant, FlatModuleResolveError> {
    use quick_xml::events::Event;

    let path = xml_root.join(ConventionalName::ConfigurationXml.canonical());
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 2 * 1024 * 1024
    {
        return Err(FlatModuleResolveError::Unavailable);
    }
    let xml = std::fs::read_to_string(path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut in_script_variant = false;
    let mut variant = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == b"ScriptVariant" => {
                if in_script_variant || variant.is_some() {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                in_script_variant = true;
            }
            Ok(Event::Text(text)) if in_script_variant => {
                let decoded = text.decode().map_err(|_| FlatModuleResolveError::Unavailable)?;
                variant = Some(match decoded.as_ref() {
                    "Russian" => ScriptVariant::Russian,
                    "English" => ScriptVariant::English,
                    _ => return Err(FlatModuleResolveError::Unavailable),
                });
            }
            Ok(Event::End(element)) if element.name().as_ref() == b"ScriptVariant" => {
                if !in_script_variant || variant.is_none() {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                in_script_variant = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(FlatModuleResolveError::Unavailable),
            _ => {}
        }
    }
    variant.ok_or(FlatModuleResolveError::Unavailable)
}

pub(crate) fn parse_compatibility_mode(xml_root: &Path) -> Result<String, FlatModuleResolveError> {
    use quick_xml::events::Event;

    let path = xml_root.join(ConventionalName::ConfigurationXml.canonical());
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 2 * 1024 * 1024
    {
        return Err(FlatModuleResolveError::Unavailable);
    }
    let xml = std::fs::read_to_string(path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    let mut value = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == b"CompatibilityMode" => {
                if inside || value.is_some() {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                inside = true;
            }
            Ok(Event::Text(text)) if inside => {
                let decoded = text.decode().map_err(|_| FlatModuleResolveError::Unavailable)?;
                if decoded.is_empty() || decoded.len() > 64 {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                value = Some(decoded.into_owned());
            }
            Ok(Event::End(element)) if element.name().as_ref() == b"CompatibilityMode" => {
                if !inside || value.is_none() {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                inside = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(FlatModuleResolveError::Unavailable),
            _ => {}
        }
    }
    value.ok_or(FlatModuleResolveError::Unavailable)
}

pub(crate) fn parse_configuration_boolean(
    xml_root: &Path,
    property_name: &str,
) -> Result<bool, FlatModuleResolveError> {
    use quick_xml::events::Event;

    if property_name.is_empty()
        || property_name.len() > 64
        || !property_name.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(FlatModuleResolveError::InvalidOwner);
    }
    let path = xml_root.join(ConventionalName::ConfigurationXml.canonical());
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 2 * 1024 * 1024
    {
        return Err(FlatModuleResolveError::Unavailable);
    }
    let xml = std::fs::read_to_string(path).map_err(|_| FlatModuleResolveError::Unavailable)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    let mut value = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == property_name.as_bytes() => {
                if inside || value.is_some() {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                inside = true;
            }
            Ok(Event::Text(text)) if inside => {
                let decoded = text.decode().map_err(|_| FlatModuleResolveError::Unavailable)?;
                value = Some(match decoded.as_ref() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(FlatModuleResolveError::Unavailable),
                });
            }
            Ok(Event::End(element)) if element.name().as_ref() == property_name.as_bytes() => {
                if !inside || value.is_none() {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                inside = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(FlatModuleResolveError::Unavailable),
            _ => {}
        }
    }
    value.ok_or(FlatModuleResolveError::Unavailable)
}

pub(crate) fn parse_server_location(value: &str) -> Option<ServerLocation<'_>> {
    let value = value.trim();
    let separator = value.rfind(['/', '\\'])?;
    let (host, base_with_sep) = value.split_at(separator);
    let base = &base_with_sep[1..];
    if host.is_empty()
        || base.is_empty()
        || host.contains('/')
        || host.contains('\\')
        || host.bytes().any(|byte| byte.is_ascii_control())
        || base.bytes().any(|byte| byte.is_ascii_control())
        || base == "."
        || base == ".."
    {
        return None;
    }
    Some(ServerLocation { host, base })
}

pub(crate) fn canonical_file_source(path: &Path) -> Option<std::path::PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    if canonical != path || !canonical.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::symlink_metadata(&canonical).ok()?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return None;
        }
    }
    let marker = canonical.join(".native-check-fixture");
    let marker_metadata = std::fs::symlink_metadata(&marker).ok()?;
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if marker_metadata.uid() != unsafe { libc::geteuid() }
            || marker_metadata.permissions().mode() & 0o777 != 0o600
        {
            return None;
        }
    }
    if std::fs::read(&marker).ok()?.as_slice() != b"bsl-analyzer-native-check-fixture-v1\n" {
        return None;
    }
    Some(canonical)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceProofError {
    Unavailable,
    Cancelled,
    DeadlineExceeded,
}

impl SourceProofError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "source_proof_unavailable",
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Unavailable => "The configured Designer binary version cannot be verified",
            Self::Cancelled => "Native module check was cancelled",
            Self::DeadlineExceeded => "Native module check exceeded its deadline",
        }
    }
}

pub(crate) fn flat_module_export_name(
    owner: &str,
    module_type: super::types::ModuleType,
    script_variant: ScriptVariant,
) -> Option<String> {
    use super::types::ModuleType;
    let canonical = canonical_owner(owner)?;
    let parts: Vec<&str> = canonical.split('.').collect();
    let suffix = match module_type {
        ModuleType::Object => {
            if parts.len() != 2 || !is_metadata_type(parts[0]) {
                return None;
            }
            match script_variant {
                ScriptVariant::Russian => ".МодульОбъекта.txt",
                ScriptVariant::English => ".ObjectModule.txt",
            }
        }
        ModuleType::Manager => {
            if parts.len() != 2 || !is_manager_metadata_type(parts[0]) {
                return None;
            }
            match script_variant {
                ScriptVariant::Russian => ".МодульМенеджера.txt",
                ScriptVariant::English => ".ManagerModule.txt",
            }
        }
        ModuleType::Common => {
            if parts.len() != 2 || parts[0] != "ОбщийМодуль" {
                return None;
            }
            match script_variant {
                ScriptVariant::Russian => ".Модуль.txt",
                ScriptVariant::English => ".Module.txt",
            }
        }
        ModuleType::ManagedForm | ModuleType::OrdinaryForm => {
            if parts.len() == 2 {
                if parts[0] != "ОбщаяФорма" {
                    return None;
                }
            } else if parts.len() == 4 {
                if !is_metadata_type(parts[0]) || parts[2] != "Форма" {
                    return None;
                }
            } else {
                return None;
            }
            match script_variant {
                ScriptVariant::Russian => ".Форма.Модуль.txt",
                ScriptVariant::English => ".Form.Module.txt",
            }
        }
    };
    let rendered = render_owner(&canonical, module_type, script_variant)?;
    Some(format!("{rendered}{suffix}"))
}

pub(crate) fn native_diagnostic_owner(
    owner: &str,
    module_type: super::types::ModuleType,
    script_variant: ScriptVariant,
) -> Option<String> {
    use super::types::ModuleType;
    flat_module_export_name(owner, module_type, script_variant)?;
    let canonical = canonical_owner(owner)?;
    let rendered_owner = render_owner(&canonical, module_type, script_variant)?;
    let suffix = match (module_type, script_variant) {
        (ModuleType::Object, ScriptVariant::Russian) => ".МодульОбъекта",
        (ModuleType::Object, ScriptVariant::English) => ".ObjectModule",
        (ModuleType::Manager, ScriptVariant::Russian) => ".МодульМенеджера",
        (ModuleType::Manager, ScriptVariant::English) => ".ManagerModule",
        (ModuleType::Common, ScriptVariant::Russian) => ".Модуль",
        (ModuleType::Common, ScriptVariant::English) => ".Module",
        (ModuleType::ManagedForm | ModuleType::OrdinaryForm, ScriptVariant::Russian) => ".Форма",
        (ModuleType::ManagedForm | ModuleType::OrdinaryForm, ScriptVariant::English) => ".Form",
    };
    Some(format!("{rendered_owner}{suffix}"))
}

fn canonical_owner(owner: &str) -> Option<String> {
    if owner.is_empty()
        || owner.len() > 512
        || owner.chars().any(|character| {
            character.is_control() || matches!(character, '/' | '\\' | ':' | '<' | '>' | '"' | '|')
        })
    {
        return None;
    }
    let mut parts: Vec<&str> = owner.split('.').collect();
    if parts.iter().any(|segment| segment.is_empty() || *segment == "..") {
        return None;
    }
    let prefix = match parts[0] {
        value
            if value == bsl_metadata::MdoType::CommonModule.russian_name()
                || value == bsl_metadata::MdoType::CommonModule.english_name() =>
        {
            bsl_metadata::MdoType::CommonModule.russian_name()
        }
        "ОбщаяФорма" | "CommonForm" => "ОбщаяФорма",
        value => value.parse::<bsl_metadata::MdoType>().ok()?.russian_name(),
    };
    parts[0] = prefix;
    Some(parts.join("."))
}

fn render_owner(
    owner: &str,
    module_type: super::types::ModuleType,
    variant: ScriptVariant,
) -> Option<String> {
    if variant == ScriptVariant::Russian {
        return Some(owner.to_owned());
    }
    let mut parts: Vec<&str> = owner.split('.').collect();
    parts[0] = match parts[0] {
        value if value == bsl_metadata::MdoType::CommonModule.russian_name() => {
            bsl_metadata::MdoType::CommonModule.english_name()
        }
        "ОбщаяФорма" => "CommonForm",
        value => value.parse::<bsl_metadata::MdoType>().ok()?.english_name(),
    };
    if matches!(
        module_type,
        super::types::ModuleType::ManagedForm | super::types::ModuleType::OrdinaryForm
    ) && parts.len() == 4
    {
        parts[2] = ConventionalName::Form.canonical();
    }
    Some(parts.join("."))
}

fn is_metadata_type(name: &str) -> bool {
    name.parse::<bsl_metadata::MdoType>().is_ok()
}

fn is_manager_metadata_type(name: &str) -> bool {
    name.parse::<bsl_metadata::MdoType>().is_ok_and(|kind| kind.russian_plural().is_some())
}

/// Look up the native flat module export by an exact generated file name.
/// The caller never joins request data to a filesystem path.
pub(crate) fn resolve_flat_module_export(
    directory: &Path,
    owner: &str,
    module_type: super::types::ModuleType,
    script_variant: ScriptVariant,
) -> Result<PathBuf, FlatModuleResolveError> {
    let filename = flat_module_export_name(owner, module_type, script_variant)
        .ok_or(FlatModuleResolveError::InvalidOwner)?;
    let entries = std::fs::read_dir(directory).map_err(|_| FlatModuleResolveError::Unavailable)?;
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| FlatModuleResolveError::Unavailable)?;
        if entry.file_name().to_str() != Some(filename.as_str()) {
            continue;
        }
        let file_type = entry.file_type().map_err(|_| FlatModuleResolveError::Unavailable)?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(FlatModuleResolveError::Unavailable);
        }
        matches.push(entry.path());
    }
    if matches.is_empty() {
        return Err(FlatModuleResolveError::NotFound);
    }
    if matches.len() != 1 {
        return Err(FlatModuleResolveError::Ambiguous);
    }
    Ok(matches.pop().expect("single matching module export"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlatModuleResolveError {
    InvalidOwner,
    NotFound,
    Ambiguous,
    Unavailable,
}

pub(crate) fn resolve_form_kind(
    xml_root: &Path,
    owner: &str,
) -> Result<Option<bsl_metadata::FormType>, FlatModuleResolveError> {
    let requested = canonical_owner(owner).ok_or(FlatModuleResolveError::InvalidOwner)?;
    let parts: Vec<&str> = requested.split('.').collect();
    let metadata_path = match parts.as_slice() {
        ["ОбщаяФорма", name] => {
            xml_root.join("CommonForms").join(format!("{name}.{}", bsl_conventions::XML_EXTENSION))
        }
        [kind_name, object, "Форма", form] => {
            let kind = kind_name
                .parse::<bsl_metadata::MdoType>()
                .map_err(|_| FlatModuleResolveError::InvalidOwner)?;
            let collection =
                metadata_collection_directory(kind).ok_or(FlatModuleResolveError::InvalidOwner)?;
            xml_root
                .join(collection)
                .join(object)
                .join(ConventionalName::Forms.canonical())
                .join(format!("{form}.{}", bsl_conventions::XML_EXTENSION))
        }
        _ => return Err(FlatModuleResolveError::InvalidOwner),
    };
    let relative =
        metadata_path.strip_prefix(xml_root).map_err(|_| FlatModuleResolveError::InvalidOwner)?;
    let root_metadata =
        std::fs::symlink_metadata(xml_root).map_err(|_| FlatModuleResolveError::Unavailable)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(FlatModuleResolveError::Unavailable);
    }
    let mut current = xml_root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(FlatModuleResolveError::Unavailable),
        };
        if metadata.file_type().is_symlink() {
            return Err(FlatModuleResolveError::Unavailable);
        }
        let last = index + 1 == components.len();
        if (last && !metadata.is_file()) || (!last && !metadata.is_dir()) {
            return Err(FlatModuleResolveError::Unavailable);
        }
        if last {
            if metadata.len() > 2 * 1024 * 1024 {
                return Err(FlatModuleResolveError::Unavailable);
            }
            let xml = std::fs::read_to_string(&current)
                .map_err(|_| FlatModuleResolveError::Unavailable)?;
            return parse_metadata_form_type(&xml).map(Some);
        }
    }
    Err(FlatModuleResolveError::Unavailable)
}

fn metadata_collection_directory(kind: bsl_metadata::MdoType) -> Option<&'static str> {
    use bsl_metadata::MdoType as Kind;
    Some(match kind {
        Kind::Catalog => "Catalogs",
        Kind::Document => "Documents",
        Kind::InformationRegister => "InformationRegisters",
        Kind::AccumulationRegister => "AccumulationRegisters",
        Kind::AccountingRegister => "AccountingRegisters",
        Kind::CalculationRegister => "CalculationRegisters",
        Kind::ChartOfCharacteristicTypes => "ChartsOfCharacteristicTypes",
        Kind::ChartOfAccounts => "ChartsOfAccounts",
        Kind::ChartOfCalculationTypes => "ChartsOfCalculationTypes",
        Kind::BusinessProcess => "BusinessProcesses",
        Kind::Task => "Tasks",
        Kind::Enum => "Enums",
        Kind::ExchangePlan => "ExchangePlans",
        Kind::ExternalDataSource => "ExternalDataSources",
        Kind::Constant => "Constants",
        Kind::DataProcessor => "DataProcessors",
        Kind::Report => "Reports",
        Kind::ExternalDataProcessor
        | Kind::ExternalReport
        | Kind::CommonModule
        | Kind::EventSubscription
        | Kind::Subsystem
        | Kind::Role => return None,
    })
}

fn parse_metadata_form_type(xml: &str) -> Result<bsl_metadata::FormType, FlatModuleResolveError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut inside_properties = false;
    let mut inside_form_type = false;
    let mut saw_properties = false;
    let mut value = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) if element.name().as_ref() == b"Properties" => {
                if inside_properties || saw_properties {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                inside_properties = true;
                saw_properties = true;
            }
            Ok(Event::End(element)) if element.name().as_ref() == b"Properties" => {
                if !inside_properties || inside_form_type {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                inside_properties = false;
            }
            Ok(Event::Start(element))
                if inside_properties && element.name().as_ref() == b"FormType" =>
            {
                if inside_form_type || value.is_some() {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                inside_form_type = true;
            }
            Ok(Event::Text(text)) if inside_form_type => {
                let decoded = text.decode().map_err(|_| FlatModuleResolveError::Unavailable)?;
                if value.is_some() {
                    return Err(FlatModuleResolveError::Ambiguous);
                }
                value = Some(match decoded.as_ref() {
                    "Managed" => bsl_metadata::FormType::Managed,
                    "Ordinary" => bsl_metadata::FormType::Ordinary,
                    _ => return Err(FlatModuleResolveError::Unavailable),
                });
            }
            Ok(Event::End(element)) if element.name().as_ref() == b"FormType" => {
                if !inside_form_type || value.is_none() {
                    return Err(FlatModuleResolveError::Unavailable);
                }
                inside_form_type = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(FlatModuleResolveError::Unavailable),
            _ => {}
        }
    }
    if inside_properties || inside_form_type || !saw_properties {
        return Err(FlatModuleResolveError::Unavailable);
    }
    value.ok_or(FlatModuleResolveError::Unavailable)
}

pub(crate) async fn probe_platform_build(
    python_path: &Path,
    core_library: &Path,
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
    max_output_bytes: usize,
) -> Result<String, SourceProofError> {
    use super::compiler::{run_bounded_isolated, ProcessFailure, Termination};

    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(SourceProofError::Unavailable);
    }

    const PROBE: &str = concat!(
        "import ctypes, os, sys\n",
        "class Version(ctypes.Structure):\n",
        "    _fields_ = [('major', ctypes.c_int), ('minor', ctypes.c_int), ('patch', ctypes.c_int), ('build', ctypes.c_int)]\n",
        "lib = ctypes.CDLL(sys.argv[1], mode=os.RTLD_NOW)\n",
        "fn = getattr(lib, '_ZN4core7Version17getCurrentVersionEv')\n",
        "fn.argtypes = []\n",
        "fn.restype = Version\n",
        "v = fn()\n",
        "parts = (v.major, v.minor, v.patch, v.build)\n",
        "if any(x < 0 or x > 999999 for x in parts): sys.exit(3)\n",
        "print('.'.join(str(x) for x in parts))\n",
    );

    let library = core_library.canonicalize().map_err(|_| SourceProofError::Unavailable)?;
    if !library.is_file() {
        return Err(SourceProofError::Unavailable);
    }
    let library_dir = library.parent().ok_or(SourceProofError::Unavailable)?;
    let args = [OsString::from("-c"), OsString::from(PROBE), library.as_os_str().to_owned()];
    let env = [(OsString::from("LD_LIBRARY_PATH"), library_dir.as_os_str().to_owned())];
    let output = run_bounded_isolated(
        python_path,
        &args,
        cwd,
        deadline,
        cancel,
        max_output_bytes.min(4096),
        &env,
    )
    .await
    .map_err(|_: ProcessFailure| SourceProofError::Unavailable)?;
    match output.termination {
        Termination::Cancelled => return Err(SourceProofError::Cancelled),
        Termination::Deadline => return Err(SourceProofError::DeadlineExceeded),
        Termination::Completed(Some(0)) => {}
        _ => return Err(SourceProofError::Unavailable),
    }
    if output.truncated || !output.stderr.is_empty() {
        return Err(SourceProofError::Unavailable);
    }
    let build =
        std::str::from_utf8(&output.stdout).map_err(|_| SourceProofError::Unavailable)?.trim();
    let parts: Vec<&str> = build.split('.').collect();
    if parts.len() != 4
        || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(SourceProofError::Unavailable);
    }
    if !valid_build_token(build) {
        return Err(SourceProofError::Unavailable);
    }
    Ok(build.to_owned())
}

fn valid_build_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_accept_only_the_verified_slash_and_backslash_shapes() {
        assert_eq!(
            parse_server_location("host.example/demo"),
            Some(ServerLocation { host: "host.example", base: "demo" })
        );
        assert_eq!(
            parse_server_location("host.example:1541\\demo"),
            Some(ServerLocation { host: "host.example:1541", base: "demo" })
        );
        for invalid in ["demo", "/demo", "host/", "host/..", "host/.", "host/one/two"] {
            assert_eq!(parse_server_location(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn configuration_script_variant_is_read_from_the_dump_not_assumed() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Configuration.xml"),
            "<Configuration><Properties><ScriptVariant>English</ScriptVariant></Properties></Configuration>",
        )
        .unwrap();
        assert_eq!(parse_script_variant(directory.path()), Ok(ScriptVariant::English));
        std::fs::write(
            directory.path().join("Configuration.xml"),
            "<Configuration><Properties><ScriptVariant>unknown</ScriptVariant></Properties></Configuration>",
        )
        .unwrap();
        assert_eq!(
            parse_script_variant(directory.path()),
            Err(FlatModuleResolveError::Unavailable)
        );
    }

    #[test]
    fn native_flat_export_names_match_verified_object_common_and_form_labels() {
        use super::super::types::ModuleType;

        assert_eq!(
            flat_module_export_name("Документ.Заказ", ModuleType::Object, ScriptVariant::Russian),
            Some("Документ.Заказ.МодульОбъекта.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "ОбщийМодуль.УведомленияСервер",
                ModuleType::Common,
                ScriptVariant::Russian
            ),
            Some("ОбщийМодуль.УведомленияСервер.Модуль.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "ОбщаяФорма.Форма",
                ModuleType::ManagedForm,
                ScriptVariant::Russian
            ),
            Some("ОбщаяФорма.Форма.Форма.Модуль.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "Документ.Заказ.Форма.ФормаДокумента",
                ModuleType::OrdinaryForm,
                ScriptVariant::Russian
            ),
            Some("Документ.Заказ.Форма.ФормаДокумента.Форма.Модуль.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "Документ.Заказ",
                ModuleType::ManagedForm,
                ScriptVariant::Russian
            ),
            None
        );
        assert_eq!(
            native_diagnostic_owner(
                "ОбщаяФорма.Форма",
                ModuleType::ManagedForm,
                ScriptVariant::Russian
            ),
            Some("ОбщаяФорма.Форма.Форма".to_owned())
        );
        assert_eq!(
            native_diagnostic_owner(
                "CommonModule.BA023Common",
                ModuleType::Common,
                ScriptVariant::English
            ),
            Some("CommonModule.BA023Common.Module".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "Документ.BA023Catalog",
                ModuleType::Object,
                ScriptVariant::English
            ),
            Some("Document.BA023Catalog.ObjectModule.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "ОбщийМодуль.BA023Common",
                ModuleType::Common,
                ScriptVariant::English
            ),
            Some("CommonModule.BA023Common.Module.txt".to_owned())
        );
        assert_eq!(
            flat_module_export_name(
                "ОбщаяФорма.BA023Managed",
                ModuleType::ManagedForm,
                ScriptVariant::English
            ),
            Some("CommonForm.BA023Managed.Form.Module.txt".to_owned())
        );
        assert_eq!(
            native_diagnostic_owner(
                "ОбщаяФорма.BA023Managed",
                ModuleType::ManagedForm,
                ScriptVariant::English
            ),
            Some("CommonForm.BA023Managed.Form".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn flat_module_lookup_uses_exact_files_without_following_symlinks() {
        use super::super::types::ModuleType;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("Документ.Заказ.МодульОбъекта.txt");
        std::fs::write(&target, "old module").unwrap();
        assert_eq!(
            resolve_flat_module_export(
                directory.path(),
                "Документ.Заказ",
                ModuleType::Object,
                ScriptVariant::Russian
            )
            .unwrap(),
            target
        );

        std::fs::remove_file(&target).unwrap();
        let foreign = directory.path().join("foreign.txt");
        std::fs::write(&foreign, "foreign").unwrap();
        symlink(&foreign, &target).unwrap();
        assert_eq!(
            resolve_flat_module_export(
                directory.path(),
                "Документ.Заказ",
                ModuleType::Object,
                ScriptVariant::Russian
            ),
            Err(FlatModuleResolveError::Unavailable)
        );
    }

    #[test]
    fn form_context_uses_metadata_owner_xml_not_form_body_xml() {
        let directory = tempfile::tempdir().unwrap();
        let common_forms = directory.path().join("CommonForms");
        std::fs::create_dir_all(common_forms.join("Форма/Ext")).unwrap();
        std::fs::write(
            common_forms.join("Форма.xml"),
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20"><CommonForm uuid="12345678-1234-1234-1234-123456789012"><Properties><Name>Форма</Name><FormType>Managed</FormType></Properties></CommonForm></MetaDataObject>"#,
        )
        .unwrap();
        std::fs::write(
            common_forms.join("Форма/Ext/Form.xml"),
            r#"<FormRoot xmlns="http://v8.1c.ru/8.3/MDClasses"><Form uuid="12345678-1234-1234-1234-123456789012"><Properties><Name>Форма</Name></Properties><FormType>Ordinary</FormType></Form></FormRoot>"#,
        )
        .unwrap();

        assert_eq!(
            resolve_form_kind(directory.path(), "ОбщаяФорма.Форма").unwrap(),
            Some(bsl_metadata::FormType::Managed)
        );
        let nested_form = directory.path().join("Documents/Заказ/Forms/ФормаДокумента.xml");
        std::fs::create_dir_all(nested_form.parent().unwrap()).unwrap();
        std::fs::write(
            &nested_form,
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20"><Form uuid="12345678-1234-1234-1234-123456789012"><Properties><Name>ФормаДокумента</Name><FormType>Ordinary</FormType></Properties></Form></MetaDataObject>"#,
        )
        .unwrap();
        assert_eq!(
            resolve_form_kind(directory.path(), "Документ.Заказ.Форма.ФормаДокумента").unwrap(),
            Some(bsl_metadata::FormType::Ordinary)
        );
        std::fs::write(
            &nested_form,
            r#"<MetaDataObject><Form><Properties><Name>ФормаДокумента</Name><FormType>Managed</FormType><FormType>Ordinary</FormType></Properties></Form></MetaDataObject>"#,
        )
        .unwrap();
        assert_eq!(
            resolve_form_kind(directory.path(), "Документ.Заказ.Форма.ФормаДокумента"),
            Err(FlatModuleResolveError::Ambiguous)
        );
        assert_eq!(
            resolve_form_kind(directory.path(), "../ОбщаяФорма.Форма"),
            Err(FlatModuleResolveError::InvalidOwner)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_probe_accepts_only_bounded_four_part_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let python = directory.path().join("python3-stub");
        std::fs::write(&python, "#!/bin/sh\nprintf '8.3.27.1989\\n'\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = directory.path().join("core83.so");
        std::fs::write(&library, b"stub library").unwrap();
        let build = probe_platform_build(
            &python,
            &library,
            directory.path(),
            Instant::now() + std::time::Duration::from_secs(5),
            &CancellationToken::new(),
            128,
        )
        .await
        .unwrap();
        assert_eq!(build, "8.3.27.1989");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_probe_preserves_cancellation() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let python = directory.path().join("python3-stub");
        std::fs::write(&python, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = directory.path().join("core83.so");
        std::fs::write(&library, b"stub library").unwrap();
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let python_task = python.clone();
        let library_task = library.clone();
        let cwd = directory.path().to_owned();
        let task = tokio::spawn(async move {
            probe_platform_build(
                &python_task,
                &library_task,
                &cwd,
                Instant::now() + std::time::Duration::from_secs(5),
                &child_cancel,
                128,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Err(SourceProofError::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_probe_preserves_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let python = directory.path().join("python3-stub");
        std::fs::write(&python, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = directory.path().join("core83.so");
        std::fs::write(&library, b"stub library").unwrap();
        assert_eq!(
            probe_platform_build(
                &python,
                &library,
                directory.path(),
                Instant::now() + std::time::Duration::from_millis(50),
                &CancellationToken::new(),
                128,
            )
            .await,
            Err(SourceProofError::DeadlineExceeded)
        );
    }
}
