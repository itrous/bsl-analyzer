//! Vendor support map of a configuration dump: `Ext/ParentConfigurations.bin`.
//!
//! Despite the extension the file is text (UTF-8, usually with a BOM): one
//! brace list of comma-separated tokens, strings quoted with `"` and a quote
//! inside a string doubled. The layout below was established by observation of
//! real dumps and of dumps the platform wrote for a configuration put on
//! support from a delivery `.cf` (8.3.17 and 8.3.27):
//!
//! ```text
//! {format, flag, vendor_count,
//!    <per vendor configuration>
//!    uuid, flag, uuid, "version", "vendor", "name", object_count,
//!       <per object> mode, flag, object_uuid, vendor_object_uuid,
//!    <trailing settings>}
//! ```
//!
//! A configuration taken off support keeps the file, with no vendor
//! configuration at all (`{6,0,0,0,1,0}`). Every object of the vendor
//! configuration has a record, the configuration root and its languages
//! included; an object added on top of the vendor configuration has none.
//!
//! The first uuid of a record is the object in THIS configuration, the second
//! the vendor object it is matched with. They usually coincide; they differ for
//! an object the vendor delivered under another identity (observed on renamed
//! copies), and only the first one is ever found in the dump.

use std::path::Path;

use rustc_hash::FxHashMap;
use uuid::Uuid;

/// File name of the support map, under `<configuration root>/Ext/`.
pub const PARENT_CONFIGURATIONS_BIN: &str =
    bsl_conventions::ConventionalName::ParentConfigurationsBin.canonical();

/// Support mode of one vendor object, as recorded in the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportMode {
    /// Code `0`: the object was taken off support; the vendor no longer owns it.
    /// Not seen in the observed dumps.
    Removed,
    /// Code `1`: on support. Every record of a configuration freshly put on
    /// support carries it, and so do the vendor objects a project edits in place.
    Editable,
    /// Code `2`: on support; seen on a few records of real dumps, read as an
    /// object the vendor keeps locked.
    Locked,
    /// Any other code: a record of the vendor all the same.
    Other(u32),
}

impl SupportMode {
    fn from_code(code: u32) -> Self {
        match code {
            0 => SupportMode::Removed,
            1 => SupportMode::Editable,
            2 => SupportMode::Locked,
            other => SupportMode::Other(other),
        }
    }

    /// Whether an object in this mode still belongs to the vendor.
    pub fn is_supported(self) -> bool {
        !matches!(self, SupportMode::Removed)
    }
}

/// Why a support map could not be read; `offset` is the byte position in the
/// file (BOM included) where reading stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorSupportError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for VendorSupportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PARENT_CONFIGURATIONS_BIN}: byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for VendorSupportError {}

/// The vendor records of a configuration: object uuid -> support mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorSupport {
    modes: FxHashMap<Uuid, SupportMode>,
    /// The uuid of the configuration root (`Configuration.xml`), the owner of
    /// the application, session and external-connection modules.
    configuration_root: Option<Uuid>,
}

impl VendorSupport {
    /// The map of the configuration rooted at `config_root`; `Ok(None)` when the
    /// dump has no `Ext/ParentConfigurations.bin` (never was on support).
    pub fn load(config_root: &Path) -> Result<Option<Self>, VendorSupportError> {
        let ext = bsl_conventions::find_child_ci(
            config_root,
            bsl_conventions::ConventionalName::Ext.canonical(),
        )
        .unwrap_or_else(|| config_root.join(bsl_conventions::ConventionalName::Ext.canonical()));
        let path = bsl_conventions::find_child_ci(&ext, PARENT_CONFIGURATIONS_BIN)
            .unwrap_or_else(|| ext.join(PARENT_CONFIGURATIONS_BIN));
        let mut support = match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(VendorSupportError {
                    offset: 0,
                    message: format!("cannot read: {error}"),
                });
            }
        };
        support.configuration_root = configuration_root_uuid(config_root);
        Ok(Some(support))
    }

    /// The same map with the configuration root's uuid known.
    pub fn with_configuration_root(mut self, uuid: Uuid) -> Self {
        self.configuration_root = Some(uuid);
        self
    }

    /// The uuid of the configuration root, when its `Configuration.xml` was read.
    pub fn configuration_root(&self) -> Option<Uuid> {
        self.configuration_root
    }

    /// Parse the content of `ParentConfigurations.bin`.
    pub fn parse(bytes: &[u8]) -> Result<Self, VendorSupportError> {
        let tokens = tokenize(bytes)?;
        let mut cursor = Cursor { tokens: &tokens, index: 0, end: bytes.len() };
        cursor.number("format version")?;
        cursor.number("support flag")?;
        let vendor_count = cursor.number("vendor configuration count")?;
        let mut modes = FxHashMap::default();
        for _ in 0..vendor_count {
            cursor.uuid("vendor configuration uuid")?;
            cursor.number("vendor configuration flag")?;
            cursor.uuid("vendor configuration uuid")?;
            cursor.string("vendor configuration version")?;
            cursor.string("vendor name")?;
            cursor.string("vendor configuration name")?;
            let object_count = cursor.number("object count")?;
            if (object_count as usize).saturating_mul(4) > cursor.remaining() {
                return Err(cursor.error(format!(
                    "the header announces {object_count} objects, the file holds fewer"
                )));
            }
            for _ in 0..object_count {
                let mode = SupportMode::from_code(cursor.number("support mode")?);
                cursor.number("object flag")?;
                let object = cursor.uuid("object uuid")?;
                cursor.uuid("vendor object uuid")?;
                modes.insert(object, mode);
            }
        }
        Ok(Self { modes, configuration_root: None })
    }

    /// The recorded mode of `uuid`; `None` for an object the vendor never had.
    pub fn mode(&self, uuid: &Uuid) -> Option<SupportMode> {
        self.modes.get(uuid).copied()
    }

    /// Whether `uuid` is a vendor object still on support.
    pub fn is_supported(&self, uuid: &Uuid) -> bool {
        self.mode(uuid).is_some_and(SupportMode::is_supported)
    }

    /// Number of vendor records.
    pub fn len(&self) -> usize {
        self.modes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modes.is_empty()
    }
}

/// XML whitespace, quoting and a long preamble do not change the root's identity.
fn configuration_root_uuid(config_root: &Path) -> Option<Uuid> {
    use crate::xml_parser::helpers::{find_mdo_element, parse_xml};

    let path = bsl_conventions::find_child_ci(
        config_root,
        bsl_conventions::ConventionalName::ConfigurationXml.canonical(),
    )?;
    let xml = std::fs::read_to_string(path).ok()?;
    let doc = parse_xml(&xml).ok()?;
    let element = find_mdo_element(&doc).filter(|node| node.has_tag_name("Configuration"))?;
    Uuid::parse_str(element.attribute("uuid")?).ok()
}

struct Token<'a> {
    offset: usize,
    text: &'a str,
    quoted: bool,
}

/// Top-level tokens of the brace list. Nested lists are not part of the
/// observed format and are rejected rather than guessed at.
fn tokenize(bytes: &[u8]) -> Result<Vec<Token<'_>>, VendorSupportError> {
    let error =
        |offset: usize, message: &str| VendorSupportError { offset, message: message.to_owned() };
    let text = std::str::from_utf8(bytes).map_err(|e| error(e.valid_up_to(), "not UTF-8"))?;
    let start = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let body_start = start + text[start..].len() - text[start..].trim_start().len();
    if !text[body_start..].starts_with('{') {
        return Err(error(body_start, "expected `{`"));
    }
    let mut tokens = Vec::new();
    let mut pos = body_start + 1;
    loop {
        while text[pos..].starts_with(char::is_whitespace) {
            pos += text[pos..].chars().next().map_or(1, char::len_utf8);
        }
        let token_start = pos;
        let token = if text[pos..].starts_with('"') {
            let mut end = pos + 1;
            loop {
                match text[end..].find('"') {
                    None => return Err(error(token_start, "unterminated string")),
                    Some(at) if text[end + at + 1..].starts_with('"') => end += at + 2,
                    Some(at) => {
                        end += at + 1;
                        break;
                    }
                }
            }
            pos = end;
            Token { offset: token_start, text: &text[token_start + 1..end - 1], quoted: true }
        } else {
            let len = text[pos..].find([',', '}', '{', '"']).unwrap_or(text.len() - pos);
            pos += len;
            Token { offset: token_start, text: text[token_start..pos].trim(), quoted: false }
        };
        while text[pos..].starts_with(char::is_whitespace) {
            pos += text[pos..].chars().next().map_or(1, char::len_utf8);
        }
        match text[pos..].chars().next() {
            Some(',') => {
                tokens.push(token);
                pos += 1;
            }
            Some('}') => {
                if !(token.text.is_empty() && !token.quoted && tokens.is_empty()) {
                    tokens.push(token);
                }
                return Ok(tokens);
            }
            Some('{') => return Err(error(pos, "nested lists are not expected")),
            Some(_) => return Err(error(pos, "expected `,` or `}`")),
            None => return Err(error(pos, "the list is not closed")),
        }
    }
}

struct Cursor<'t, 'a> {
    tokens: &'t [Token<'a>],
    index: usize,
    end: usize,
}

impl<'a> Cursor<'_, 'a> {
    fn remaining(&self) -> usize {
        self.tokens.len() - self.index
    }

    fn error(&self, message: String) -> VendorSupportError {
        let offset = self.tokens.get(self.index).map_or(self.end, |token| token.offset);
        VendorSupportError { offset, message }
    }

    fn next(&mut self, what: &str) -> Result<&Token<'a>, VendorSupportError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.error(format!("the file ends before the {what}")))?;
        self.index += 1;
        Ok(token)
    }

    fn number(&mut self, what: &str) -> Result<u32, VendorSupportError> {
        let token = self.next(what)?;
        match token.text.parse::<u32>() {
            Ok(value) if !token.quoted => Ok(value),
            _ => Err(VendorSupportError {
                offset: token.offset,
                message: format!("expected a number ({what}), found `{}`", token.text),
            }),
        }
    }

    fn uuid(&mut self, what: &str) -> Result<Uuid, VendorSupportError> {
        let token = self.next(what)?;
        match Uuid::parse_str(token.text) {
            Ok(value) if !token.quoted => Ok(value),
            _ => Err(VendorSupportError {
                offset: token.offset,
                message: format!("expected a uuid ({what}), found `{}`", token.text),
            }),
        }
    }

    fn string(&mut self, what: &str) -> Result<(), VendorSupportError> {
        let token = self.next(what)?;
        if token.quoted {
            Ok(())
        } else {
            Err(VendorSupportError {
                offset: token.offset,
                message: format!("expected a quoted string ({what}), found `{}`", token.text),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "0f6a1c2e-0000-4000-8000-000000000001";
    const EDITABLE: &str = "0f6a1c2e-0000-4000-8000-000000000002";
    const LOCKED: &str = "0f6a1c2e-0000-4000-8000-000000000003";
    const REMOVED: &str = "0f6a1c2e-0000-4000-8000-000000000004";
    const RENAMED: &str = "0f6a1c2e-0000-4000-8000-000000000005";
    const RENAMED_VENDOR: &str = "0f6a1c2e-0000-4000-8000-000000000006";
    const OWN: &str = "0f6a1c2e-0000-4000-8000-0000000000ff";

    /// A synthetic map in the layout the platform writes: BOM, header with a
    /// Cyrillic configuration name, records `mode, flag, uuid, uuid`, the
    /// trailing settings the platform appends.
    fn fixture(records: &[(u32, &str, &str)]) -> Vec<u8> {
        let mut text = format!(
            "\u{feff}{{6,0,1,0f6a1c2e-0000-4000-8000-0000000000a1,0,0f6a1c2e-0000-4000-8000-0000000000a2,\
             \"1.0.0.1\",\"Поставщик \"\"Один\"\"\",\"Конфигурация1\",{}",
            records.len()
        );
        for (mode, object, vendor) in records {
            text.push_str(&format!(",{mode},0,{object},{vendor}"));
        }
        text.push_str(",0,0,0,1,0,0,0,1,0,1,0,1,1,1,1}");
        text.into_bytes()
    }

    fn uuid(text: &str) -> Uuid {
        Uuid::parse_str(text).unwrap()
    }

    #[test]
    fn records_are_read_with_their_modes() {
        let map = VendorSupport::parse(&fixture(&[
            (1, ROOT, ROOT),
            (1, EDITABLE, EDITABLE),
            (2, LOCKED, LOCKED),
            (0, REMOVED, REMOVED),
            (1, RENAMED, RENAMED_VENDOR),
        ]))
        .unwrap();

        assert_eq!(map.len(), 5);
        assert_eq!(map.mode(&uuid(ROOT)), Some(SupportMode::Editable));
        assert_eq!(map.mode(&uuid(EDITABLE)), Some(SupportMode::Editable));
        assert_eq!(map.mode(&uuid(LOCKED)), Some(SupportMode::Locked));
        assert_eq!(map.mode(&uuid(REMOVED)), Some(SupportMode::Removed));
        // The record is keyed by the object of this configuration, not by its
        // vendor counterpart.
        assert_eq!(map.mode(&uuid(RENAMED)), Some(SupportMode::Editable));
        assert_eq!(map.mode(&uuid(RENAMED_VENDOR)), None);
        // Header uuids are not records.
        assert_eq!(map.mode(&uuid("0f6a1c2e-0000-4000-8000-0000000000a1")), None);
    }

    #[test]
    fn supported_means_recorded_and_not_removed() {
        let map = VendorSupport::parse(&fixture(&[
            (1, EDITABLE, EDITABLE),
            (2, LOCKED, LOCKED),
            (0, REMOVED, REMOVED),
            (7, ROOT, ROOT),
        ]))
        .unwrap();

        assert!(map.is_supported(&uuid(EDITABLE)));
        assert!(map.is_supported(&uuid(LOCKED)));
        assert!(map.is_supported(&uuid(ROOT)), "an unknown mode is still a vendor record");
        assert!(!map.is_supported(&uuid(REMOVED)));
        assert!(!map.is_supported(&uuid(OWN)), "an object added on top is not the vendor's");
    }

    /// A configuration taken off support keeps the file with no vendor at all.
    #[test]
    fn detached_configuration_has_no_records() {
        let map = VendorSupport::parse(b"{6,0,0,0,1,0}").unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(VendorSupport::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn file_is_found_under_ext() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join(bsl_conventions::ConventionalName::Ext.canonical());
        std::fs::create_dir(&ext).unwrap();
        std::fs::write(ext.join(PARENT_CONFIGURATIONS_BIN), fixture(&[(1, EDITABLE, EDITABLE)]))
            .unwrap();
        let map = VendorSupport::load(dir.path()).unwrap().unwrap();
        assert!(map.is_supported(&uuid(EDITABLE)));
        assert_eq!(map.configuration_root(), None, "no Configuration.xml, no root uuid");
    }

    /// The application modules belong to the configuration root, so the loader
    /// reads its uuid next to the map.
    #[test]
    fn configuration_root_uuid_is_read_with_the_map() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join(bsl_conventions::ConventionalName::Ext.canonical());
        std::fs::create_dir(&ext).unwrap();
        std::fs::write(ext.join(PARENT_CONFIGURATIONS_BIN), fixture(&[(1, ROOT, ROOT)])).unwrap();
        std::fs::write(
            dir.path().join(bsl_conventions::ConventionalName::ConfigurationXml.canonical()),
            format!(
                "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<MetaDataObject \
                 xmlns=\"http://v8.1c.ru/8.3/MDClasses\" version=\"2.10\">\r\n\t<Configuration \
                 uuid=\"{ROOT}\">\r\n\t\t<Properties><Name>Конфигурация1</Name></Properties>\r\n\
                 \t</Configuration>\r\n</MetaDataObject>"
            ),
        )
        .unwrap();
        let map = VendorSupport::load(dir.path()).unwrap().unwrap();
        assert_eq!(map.configuration_root(), Some(uuid(ROOT)));
        assert!(map.is_supported(&uuid(ROOT)));
    }

    #[test]
    fn configuration_uuid_accepts_xml_whitespace_and_long_preambles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Ext")).unwrap();
        std::fs::write(
            dir.path().join("Ext/ParentConfigurations.bin"),
            fixture(&[(1, ROOT, ROOT)]),
        )
        .unwrap();
        for padding in [0, 20 * 1024] {
            let xml = format!(
                "<MetaDataObject><!--{}--><Configuration\n uuid='{ROOT}'/></MetaDataObject>",
                "x".repeat(padding)
            );
            std::fs::write(dir.path().join("Configuration.xml"), xml).unwrap();
            let map = VendorSupport::load(dir.path()).unwrap().unwrap();
            assert_eq!(map.configuration_root(), Some(uuid(ROOT)), "padding={padding}");
        }
    }

    #[test]
    fn truncated_file_names_the_position() {
        let full = fixture(&[(1, EDITABLE, EDITABLE), (2, LOCKED, LOCKED)]);
        let cut = full.len() / 2;
        let error = VendorSupport::parse(&full[..cut]).unwrap_err();
        assert_eq!(error.message, "the list is not closed");
        assert_eq!(error.offset, cut);
    }

    #[test]
    fn count_beyond_the_records_is_an_error() {
        let mut text = String::from_utf8(fixture(&[(1, EDITABLE, EDITABLE)])).unwrap();
        text = text.replacen("\"Конфигурация1\",1,", "\"Конфигурация1\",9,", 1);
        let error = VendorSupport::parse(text.as_bytes()).unwrap_err();
        assert!(error.message.contains("announces 9 objects"), "{error}");
        let at = text.find("\"Конфигурация1\",9,").unwrap() + "\"Конфигурация1\",9,".len();
        assert_eq!(error.offset, at);
    }

    #[test]
    fn damaged_record_names_the_field_and_position() {
        let text = String::from_utf8(fixture(&[(1, EDITABLE, EDITABLE)])).unwrap();
        let damaged = text.replacen(EDITABLE, "not-a-uuid", 1);
        let error = VendorSupport::parse(damaged.as_bytes()).unwrap_err();
        assert_eq!(error.offset, damaged.find("not-a-uuid").unwrap());
        assert_eq!(error.message, "expected a uuid (object uuid), found `not-a-uuid`");
        assert!(error.to_string().starts_with(&format!("{PARENT_CONFIGURATIONS_BIN}: byte ")));
    }

    #[test]
    fn not_a_list_is_an_error() {
        let error = VendorSupport::parse(b"\xef\xbb\xbf<xml/>").unwrap_err();
        assert_eq!(error.offset, 3);
    }
}
