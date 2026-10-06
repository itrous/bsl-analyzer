use std::ffi::OsString;
use std::path::Path;

use super::profile::SourceKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckModulesMode {
    ThinClient,
    Server,
    WebClient,
    ExternalConnection,
    ThickClientOrdinaryApplication,
}

impl CheckModulesMode {
    fn argument(self) -> &'static str {
        match self {
            Self::ThinClient => "-ThinClient",
            Self::Server => "-Server",
            Self::WebClient => "-WebClient",
            Self::ExternalConnection => "-ExternalConnection",
            Self::ThickClientOrdinaryApplication => "-ThickClientOrdinaryApplication",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LocalAction<'a> {
    LoadConfig(&'a Path),
    LoadExtensionConfig(&'a Path, &'a str),
    UpdateDatabase,
    UpdateExtensionDatabase(&'a str),
    DumpConfigToFiles(&'a Path),
    DumpExtensionConfigToFiles(&'a Path, &'a str),
    LoadConfigFromFiles(&'a Path),
    DumpModuleFiles(&'a Path),
    DumpExtensionModuleFiles(&'a Path, &'a str),
    LoadModuleFiles(&'a Path),
    LoadExtensionModuleFiles(&'a Path, &'a str),
    CheckModules(CheckModulesMode),
    CheckExtensionModules(CheckModulesMode, &'a str),
    CheckExtensionApplicability,
}

pub(crate) fn create_file_infobase_args(path: &Path) -> Option<Vec<OsString>> {
    let path = path_argument(path)?;
    Some(vec![
        "CREATEINFOBASE".into(),
        format!("File=\"{path}\";").into(),
        "/DisableStartupMessages".into(),
        "/DisableStartupDialogs".into(),
    ])
}

pub(crate) fn source_dump_args(
    source_kind: SourceKind,
    source: &std::ffi::OsStr,
    user: Option<&std::ffi::OsStr>,
    password: Option<&std::ffi::OsStr>,
    output: &Path,
) -> Option<Vec<OsString>> {
    source_dump_extension_args(source_kind, source, user, password, output, None)
}

pub(crate) fn source_dump_extension_args(
    source_kind: SourceKind,
    source: &std::ffi::OsStr,
    user: Option<&std::ffi::OsStr>,
    password: Option<&std::ffi::OsStr>,
    output: &Path,
    extension: Option<&str>,
) -> Option<Vec<OsString>> {
    let mut args = vec![OsString::from("DESIGNER")];
    match source_kind {
        SourceKind::Server => {
            args.extend([OsString::from("/S"), source.to_owned()]);
            args.extend([OsString::from("/N"), user?.to_owned()]);
            if let Some(password) = password {
                args.extend([OsString::from("/P"), password.to_owned()]);
            }
        }
        SourceKind::File => {
            let source = std::path::Path::new(source);
            args.extend([OsString::from("/F"), source.as_os_str().to_owned()]);
        }
    }
    args.extend(startup_options());
    args.extend([OsString::from("/DumpDBCfg"), output.as_os_str().to_owned()]);
    if let Some(extension) = extension {
        args.extend([OsString::from("-Extension"), extension_argument(extension)?]);
    }
    Some(args)
}

pub(crate) fn local_action_args(infobase: &Path, action: LocalAction<'_>) -> Option<Vec<OsString>> {
    let mut args =
        vec![OsString::from("DESIGNER"), OsString::from("/F"), infobase.as_os_str().to_owned()];
    args.extend(startup_options());
    match action {
        LocalAction::LoadConfig(path) => {
            args.extend(["/LoadCfg".into(), path.as_os_str().to_owned()])
        }
        LocalAction::LoadExtensionConfig(path, extension) => {
            args.extend(["/LoadCfg".into(), path.as_os_str().to_owned()]);
            args.extend(["-Extension".into(), extension_argument(extension)?]);
        }
        LocalAction::UpdateDatabase => args.push("/UpdateDBCfg".into()),
        LocalAction::UpdateExtensionDatabase(extension) => {
            args.extend(["/UpdateDBCfg".into(), "-Extension".into()]);
            args.push(extension_argument(extension)?);
        }
        LocalAction::DumpConfigToFiles(path) => args.extend([
            "/DumpConfigToFiles".into(),
            path.as_os_str().to_owned(),
            "-Format".into(),
            "Hierarchical".into(),
        ]),
        LocalAction::DumpExtensionConfigToFiles(path, extension) => {
            args.extend([
                "/DumpConfigToFiles".into(),
                path.as_os_str().to_owned(),
                "-Format".into(),
                "Hierarchical".into(),
                "-Extension".into(),
                extension_argument(extension)?,
            ]);
        }
        LocalAction::LoadConfigFromFiles(path) => {
            args.extend(["/LoadConfigFromFiles".into(), path.as_os_str().to_owned()]);
        }
        LocalAction::DumpModuleFiles(path) => {
            args.extend(["/DumpConfigFiles".into(), path.as_os_str().to_owned(), "-Module".into()]);
        }
        LocalAction::DumpExtensionModuleFiles(path, extension) => {
            args.extend([
                "/DumpConfigFiles".into(),
                path.as_os_str().to_owned(),
                "-Module".into(),
                "-Extension".into(),
                extension_argument(extension)?,
            ]);
        }
        LocalAction::LoadModuleFiles(path) => {
            args.extend(["/LoadConfigFiles".into(), path.as_os_str().to_owned(), "-Module".into()]);
        }
        LocalAction::LoadExtensionModuleFiles(path, extension) => {
            args.extend([
                "/LoadConfigFiles".into(),
                path.as_os_str().to_owned(),
                "-Module".into(),
                "-Extension".into(),
                extension_argument(extension)?,
            ]);
        }
        LocalAction::CheckModules(mode) => {
            args.extend(["/CheckModules".into(), mode.argument().into()]);
        }
        LocalAction::CheckExtensionModules(mode, extension) => {
            args.extend([
                "/CheckModules".into(),
                mode.argument().into(),
                "-Extension".into(),
                extension_argument(extension)?,
            ]);
        }
        LocalAction::CheckExtensionApplicability => {
            args.extend(["/CheckCanApplyConfigurationExtensions".into(), "-AllZones".into()]);
        }
    }
    Some(args)
}

fn extension_argument(extension: &str) -> Option<OsString> {
    if extension.is_empty() || extension.len() > 128 || extension.chars().any(char::is_control) {
        return None;
    }
    Some(extension.into())
}

fn startup_options() -> [OsString; 2] {
    ["/DisableStartupMessages".into(), "/DisableStartupDialogs".into()]
}

fn path_argument(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let path = path.to_str()?;
    if path.is_empty() || path.chars().any(|character| character.is_control() || character == '"') {
        return None;
    }
    Some(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|value| value.into_string().unwrap()).collect()
    }

    #[test]
    fn source_credentials_are_separate_arguments_and_file_paths_are_explicit() {
        let server = source_dump_args(
            SourceKind::Server,
            std::ffi::OsStr::new("ba023-fixture.invalid/BA023Fixture"),
            Some(std::ffi::OsStr::new("user")),
            Some(std::ffi::OsStr::new("secret")),
            Path::new("/private/job/applied.cf"),
        )
        .unwrap();
        assert_eq!(
            strings(server),
            [
                "DESIGNER",
                "/S",
                "ba023-fixture.invalid/BA023Fixture",
                "/N",
                "user",
                "/P",
                "secret",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/DumpDBCfg",
                "/private/job/applied.cf",
            ]
        );

        let file = source_dump_args(
            SourceKind::File,
            std::ffi::OsStr::new("/private/fixture/ib"),
            None,
            None,
            Path::new("/private/job/applied.cf"),
        )
        .unwrap();
        assert_eq!(
            strings(file),
            [
                "DESIGNER",
                "/F",
                "/private/fixture/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/DumpDBCfg",
                "/private/job/applied.cf",
            ]
        );
        let extension = source_dump_extension_args(
            SourceKind::File,
            std::ffi::OsStr::new("/private/source/ib"),
            None,
            None,
            Path::new("/private/job/source.cfe"),
            Some("Demo"),
        )
        .unwrap();
        assert_eq!(
            strings(extension),
            [
                "DESIGNER",
                "/F",
                "/private/source/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/DumpDBCfg",
                "/private/job/source.cfe",
                "-Extension",
                "Demo",
            ]
        );
    }

    #[test]
    fn fixed_action_arguments_cannot_be_supplied_by_a_request() {
        let args = local_action_args(
            Path::new("/private/job/ib"),
            LocalAction::CheckModules(CheckModulesMode::Server),
        )
        .unwrap();
        assert_eq!(
            strings(args),
            [
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/CheckModules",
                "-Server",
            ]
        );
        assert!(create_file_infobase_args(Path::new("relative")).is_none());
        assert!(create_file_infobase_args(Path::new("/private/has\"quote")).is_none());
    }

    #[test]
    fn extension_actions_use_only_verified_fixed_argument_shapes() {
        fn assert_action(action: LocalAction<'_>, expected: &[&str]) {
            assert_eq!(
                strings(local_action_args(Path::new("/private/job/ib"), action).unwrap()),
                expected
            );
        }
        assert_action(
            LocalAction::LoadExtensionConfig(Path::new("/private/e.cfe"), "Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/LoadCfg",
                "/private/e.cfe",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::UpdateExtensionDatabase("Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/UpdateDBCfg",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::DumpExtensionConfigToFiles(Path::new("/private/job/ext-xml"), "Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/DumpConfigToFiles",
                "/private/job/ext-xml",
                "-Format",
                "Hierarchical",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::DumpExtensionModuleFiles(Path::new("/private/job/ext-modules"), "Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/DumpConfigFiles",
                "/private/job/ext-modules",
                "-Module",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::LoadExtensionModuleFiles(Path::new("/private/job/ext-modules"), "Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/LoadConfigFiles",
                "/private/job/ext-modules",
                "-Module",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::CheckExtensionModules(CheckModulesMode::Server, "Demo"),
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/CheckModules",
                "-Server",
                "-Extension",
                "Demo",
            ],
        );
        assert_action(
            LocalAction::CheckExtensionApplicability,
            &[
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/CheckCanApplyConfigurationExtensions",
                "-AllZones",
            ],
        );
        assert!(local_action_args(
            Path::new("/private/job/ib"),
            LocalAction::CheckExtensionModules(CheckModulesMode::Server, "bad\nname")
        )
        .is_none());
    }
}
