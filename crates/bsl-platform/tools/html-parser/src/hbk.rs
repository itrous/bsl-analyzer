//! Reader of the 1C help archive container (`*.hbk`).
//!
//! The container is a chain-of-blocks file: a 16-byte header, then a descriptor
//! block listing named entities, each with a header block (its UTF-16LE name)
//! and a body block chain. The help pages live in the `FileStorage` entity, an
//! ordinary ZIP archive; it is unpacked with the `zip` crate in-process.
//!
//! The block layout follows the container reader of v8-context-hbk
//! (https://github.com/alkoleft/v8-context-hbk, commit
//! 832a5bff810e368f34d005c471481c2091395455, `crates/hbk-container`), used under
//! the MIT license; see `NOTICE`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};

const CONTAINER_HEADER_SIZE: usize = 16;
const DESCRIPTOR_SIZE: usize = 12;
const BLOCK_HEADER_SIZE: usize = 31;
const SPLITTER: u32 = i32::MAX as u32;
/// Help pages are a few hundred kilobytes at most; anything far larger is a
/// corrupt size field, not a page.
const MAX_ENTRY_SIZE: u64 = 64 * 1024 * 1024;

pub const FILE_STORAGE: &str = "FileStorage";

#[derive(Debug)]
pub enum HbkError {
    Io { path: PathBuf, source: io::Error },
    Container { path: PathBuf, message: String },
    Zip { path: PathBuf, message: String },
}

impl fmt::Display for HbkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Container { path, message } => {
                write!(formatter, "{}: invalid HBK container: {message}", path.display())
            }
            Self::Zip { path, message } => {
                write!(formatter, "{}: invalid help archive: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for HbkError {}

/// An opened container: its bytes and the body offset of every named entity.
pub struct HbkContainer {
    path: PathBuf,
    bytes: Vec<u8>,
    entities: BTreeMap<String, Option<usize>>,
}

struct BlockHeader {
    payload_size: usize,
    block_size: usize,
    next: Option<usize>,
}

impl HbkContainer {
    pub fn open(path: &Path) -> Result<Self, HbkError> {
        let bytes =
            fs::read(path).map_err(|source| HbkError::Io { path: path.to_path_buf(), source })?;
        Self::from_bytes(path.to_path_buf(), bytes)
    }

    pub fn from_bytes(path: PathBuf, bytes: Vec<u8>) -> Result<Self, HbkError> {
        let mut container = Self { path, bytes, entities: BTreeMap::new() };
        if container.bytes.len() < CONTAINER_HEADER_SIZE {
            return Err(container.error(format!(
                "file has {} bytes, a container header needs {CONTAINER_HEADER_SIZE}",
                container.bytes.len()
            )));
        }
        let descriptors = container.read_chain(CONTAINER_HEADER_SIZE)?;
        if descriptors.len() % DESCRIPTOR_SIZE != 0 {
            return Err(container.error(format!(
                "descriptor block of {} bytes is not a whole number of descriptors",
                descriptors.len()
            )));
        }
        let (descriptors, _) = descriptors.as_chunks::<DESCRIPTOR_SIZE>();
        for descriptor in descriptors {
            let header_offset = u32_le(descriptor, 0) as usize;
            let body_offset = u32_le(descriptor, 4);
            if u32_le(descriptor, 8) != SPLITTER {
                return Err(container.error("descriptor terminator is missing".to_owned()));
            }
            let name = container.entity_name(header_offset)?;
            let body = (body_offset != SPLITTER).then_some(body_offset as usize);
            container.entities.insert(name, body);
        }
        Ok(container)
    }

    pub fn entity_names(&self) -> impl Iterator<Item = &str> {
        self.entities.keys().map(String::as_str)
    }

    /// The body of entity `name`.
    pub fn entity(&self, name: &str) -> Result<Vec<u8>, HbkError> {
        match self.entities.get(name) {
            Some(Some(offset)) => self.read_chain(*offset),
            Some(None) => Err(self.error(format!("entity {name} has no body"))),
            None => Err(self.error(format!("entity {name} is absent"))),
        }
    }

    fn error(&self, message: String) -> HbkError {
        HbkError::Container { path: self.path.clone(), message }
    }

    fn entity_name(&self, offset: usize) -> Result<String, HbkError> {
        let header = self.read_chain(offset)?;
        // 8-byte creation and modification stamps, 4 attribute bytes, the
        // UTF-16LE name, then 4 trailing bytes.
        if header.len() < 24 || (header.len() - 24) % 2 != 0 {
            return Err(self.error(format!("entity header at {offset} is malformed")));
        }
        let (pairs, _) = header[20..header.len() - 4].as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        String::from_utf16(&units)
            .map_err(|_| self.error(format!("entity name at {offset} is not UTF-16")))
    }

    fn block_header(&self, offset: usize) -> Result<BlockHeader, HbkError> {
        let end = offset.checked_add(BLOCK_HEADER_SIZE).filter(|&end| end <= self.bytes.len());
        let Some(end) = end else {
            return Err(self.error(format!("block header at {offset} lies outside the file")));
        };
        let header = &self.bytes[offset..end];
        if header[0..2] != *b"\r\n"
            || header[29..31] != *b"\r\n"
            || header[10] != b' '
            || header[19] != b' '
            || header[28] != b' '
        {
            return Err(self.error(format!("block header at {offset} has invalid markers")));
        }
        let hex = |field: &[u8], label: &str| {
            std::str::from_utf8(field)
                .ok()
                .and_then(|text| u32::from_str_radix(text, 16).ok())
                .ok_or_else(|| self.error(format!("block header at {offset}: bad {label}")))
        };
        let payload_size = hex(&header[2..10], "payload size")? as usize;
        let block_size = hex(&header[11..19], "block size")? as usize;
        let next = hex(&header[20..28], "next block")?;
        Ok(BlockHeader {
            payload_size,
            block_size,
            next: (next != SPLITTER).then_some(next as usize),
        })
    }

    /// Concatenates a block chain up to the payload size its first block declares.
    /// A chain that leaves the file, loops, stalls or ends early is an error.
    fn read_chain(&self, offset: usize) -> Result<Vec<u8>, HbkError> {
        let first = self.block_header(offset)?;
        if first.payload_size > self.bytes.len() {
            return Err(self.error(format!(
                "block at {offset} declares {} bytes, more than the file holds",
                first.payload_size
            )));
        }
        let total = first.payload_size;
        let mut out = Vec::with_capacity(total);
        let mut visited = BTreeSet::new();
        let (mut current, mut header) = (offset, first);
        loop {
            if !visited.insert(current) {
                return Err(self.error(format!("block chain loops back to {current}")));
            }
            let body = current + BLOCK_HEADER_SIZE;
            let wanted = header.block_size.min(total - out.len());
            if wanted == 0 && out.len() < total {
                return Err(self.error(format!("block at {current} adds no data")));
            }
            let chunk = body
                .checked_add(wanted)
                .filter(|&end| end <= self.bytes.len())
                .map(|end| &self.bytes[body..end])
                .ok_or_else(|| {
                    self.error(format!("block body at {current} lies outside the file"))
                })?;
            out.extend_from_slice(chunk);
            match header.next {
                Some(next) if out.len() < total => {
                    current = next;
                    header = self.block_header(next)?;
                }
                _ => break,
            }
        }
        if out.len() != total {
            return Err(self.error(format!(
                "block chain at {offset} ended after {} of {total} bytes",
                out.len()
            )));
        }
        Ok(out)
    }
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Unpacks the `FileStorage` archive of `hbk` into `dest` (which must not exist
/// yet), giving the page layout the HTML parser walks. Entry paths must stay
/// inside `dest`; a hostile or corrupt name fails the whole extraction.
pub fn extract_file_storage(hbk: &Path, dest: &Path) -> Result<usize, HbkError> {
    let container = HbkContainer::open(hbk)?;
    let storage = container.entity(FILE_STORAGE)?;
    fs::create_dir(dest).map_err(|source| HbkError::Io { path: dest.to_path_buf(), source })?;
    let result = unpack_zip(hbk, &storage, dest);
    if result.is_err() {
        // `dest` was created here; a failed extraction leaves nothing behind.
        let _ = fs::remove_dir_all(dest);
    }
    result
}

fn unpack_zip(source: &Path, archive: &[u8], dest: &Path) -> Result<usize, HbkError> {
    let zip_error = |message: String| HbkError::Zip { path: source.to_path_buf(), message };
    let io_error =
        |path: &Path, source: io::Error| HbkError::Io { path: path.to_path_buf(), source };
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))
        .map_err(|error| zip_error(format!("FileStorage is not a ZIP archive: {error}")))?;

    let mut files = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| zip_error(error.to_string()))?;
        let relative = safe_relative_path(entry.name()).ok_or_else(|| {
            zip_error(format!("entry `{}` escapes the extraction directory", entry.name()))
        })?;
        let target = dest.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|error| io_error(&target, error))?;
            continue;
        }
        if entry.size() > MAX_ENTRY_SIZE {
            return Err(zip_error(format!("entry `{}` is implausibly large", entry.name())));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        (&mut entry)
            .take(MAX_ENTRY_SIZE + 1)
            .read_to_end(&mut content)
            .map_err(|error| zip_error(format!("entry `{}`: {error}", entry.name())))?;
        if content.len() as u64 > MAX_ENTRY_SIZE {
            return Err(zip_error(format!("entry `{}` is implausibly large", entry.name())));
        }
        fs::write(&target, content).map_err(|error| io_error(&target, error))?;
        files += 1;
    }
    Ok(files)
}

/// `name` as a path made only of normal components, or `None` when it is
/// absolute, climbs out with `..`, or is empty.
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    let mut path = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(test)]
#[path = "hbk_tests.rs"]
mod tests;
