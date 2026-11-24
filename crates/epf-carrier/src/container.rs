use std::collections::BTreeSet;
use std::io::{Read, Write};

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};

use crate::{PackError, MAX_CARRIER_BYTES};

const TEMPLATE: &[u8] = include_bytes!("../fixtures/reference.epf");
const MODULE_ENTRY: &str = "acd46803-cf80-5192-ab04-a91e193ab6a1.0";
const SENTINEL: u32 = 0x7fff_ffff;
const FILE_HEADER_BYTES: usize = 16;
const BLOCK_HEADER_BYTES: usize = 31;
const MAX_ENTRIES: usize = 16;
const MAX_CHAIN_BLOCKS: usize = 64;
const BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    header: Vec<u8>,
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Container {
    version: u32,
    entries: Vec<Entry>,
}

fn malformed<T>() -> Result<T, PackError> {
    Err(PackError::InternalTemplate)
}

fn word(bytes: &[u8], offset: usize) -> Result<u32, PackError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(PackError::InternalTemplate)
}

fn hex_word(bytes: &[u8]) -> Result<usize, PackError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| usize::from_str_radix(text, 16).ok())
        .ok_or(PackError::InternalTemplate)
}

fn read_block(bytes: &[u8], start: usize) -> Result<Vec<u8>, PackError> {
    let mut offset = start;
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    let mut total = None;
    loop {
        if !seen.insert(offset) || seen.len() > MAX_CHAIN_BLOCKS {
            return malformed();
        }
        let header = bytes
            .get(
                offset
                    ..offset.checked_add(BLOCK_HEADER_BYTES).ok_or(PackError::InternalTemplate)?,
            )
            .ok_or(PackError::InternalTemplate)?;
        if &header[..2] != b"\r\n"
            || &header[29..] != b"\r\n"
            || header[10] != b' '
            || header[19] != b' '
            || header[28] != b' '
        {
            return malformed();
        }
        let size = hex_word(&header[2..10])?;
        let capacity = hex_word(&header[11..19])?;
        let next = hex_word(&header[20..28])?;
        let total = *total.get_or_insert(size);
        if total > MAX_CARRIER_BYTES || capacity > MAX_CARRIER_BYTES {
            return malformed();
        }
        let data_start = offset + BLOCK_HEADER_BYTES;
        let data = bytes
            .get(data_start..data_start.checked_add(capacity).ok_or(PackError::InternalTemplate)?)
            .ok_or(PackError::InternalTemplate)?;
        let remaining = total.checked_sub(output.len()).ok_or(PackError::InternalTemplate)?;
        output.extend_from_slice(&data[..capacity.min(remaining)]);
        if output.len() == total {
            return Ok(output);
        }
        if capacity == 0 || next == SENTINEL as usize || next < FILE_HEADER_BYTES {
            return malformed();
        }
        offset = next;
    }
}

impl Container {
    fn read(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() > MAX_CARRIER_BYTES
            || bytes.len() < FILE_HEADER_BYTES
            || word(bytes, 4)? != 512
            || word(bytes, 12)? != 0
        {
            return malformed();
        }
        let version = word(bytes, 8)?;
        if !matches!(version, 1 | 16) {
            return malformed();
        }
        let directory = read_block(bytes, FILE_HEADER_BYTES)?;
        if directory.is_empty() || directory.len() % 12 != 0 || directory.len() / 12 > MAX_ENTRIES {
            return malformed();
        }
        let mut entries = Vec::new();
        let mut names = BTreeSet::new();
        for record in directory.as_chunks::<12>().0 {
            if word(record, 8)? != SENTINEL {
                return malformed();
            }
            let header_offset = word(record, 0)? as usize;
            let data_offset = word(record, 4)? as usize;
            if header_offset < FILE_HEADER_BYTES || data_offset < FILE_HEADER_BYTES {
                return malformed();
            }
            let header = read_block(bytes, header_offset)?;
            if header.len() < 26 || header.len() % 2 != 0 || !header.ends_with(&[0; 4]) {
                return malformed();
            }
            let utf16: Vec<_> = header[20..header.len() - 4]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let name = String::from_utf16(&utf16).map_err(|_| PackError::InternalTemplate)?;
            if name.is_empty()
                || name.len() > 128
                || name.contains('\0')
                || !names.insert(name.clone())
            {
                return malformed();
            }
            let data = read_block(bytes, data_offset)?;
            entries.push(Entry { name, header, data });
        }
        Ok(Self { version, entries })
    }

    fn write(&self) -> Result<Vec<u8>, PackError> {
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES {
            return malformed();
        }
        let mut bytes = Vec::new();
        for word in [SENTINEL, 512, self.version, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let directory = vec![0; self.entries.len() * 12];
        let directory_capacity = if self.version == 16 { 512 } else { directory.len() };
        write_block(&mut bytes, &directory, directory_capacity)?;
        for (index, entry) in self.entries.iter().enumerate() {
            let header_offset =
                u32::try_from(bytes.len()).map_err(|_| PackError::CarrierTooLarge)?;
            let header_capacity = entry.header.len() + if self.version == 16 { 8 } else { 0 };
            write_block(&mut bytes, &entry.header, header_capacity)?;
            let data_offset = u32::try_from(bytes.len()).map_err(|_| PackError::CarrierTooLarge)?;
            write_block(&mut bytes, &entry.data, entry.data.len().max(512))?;
            let at = FILE_HEADER_BYTES + BLOCK_HEADER_BYTES + index * 12;
            for (word_index, word) in [header_offset, data_offset, SENTINEL].iter().enumerate() {
                bytes[at + word_index * 4..at + word_index * 4 + 4]
                    .copy_from_slice(&word.to_le_bytes());
            }
        }
        Ok(bytes)
    }
}

fn write_block(output: &mut Vec<u8>, data: &[u8], capacity: usize) -> Result<(), PackError> {
    if capacity < data.len() {
        return malformed();
    }
    if output
        .len()
        .checked_add(BLOCK_HEADER_BYTES)
        .and_then(|size| size.checked_add(capacity))
        .is_none_or(|size| size > MAX_CARRIER_BYTES)
    {
        return Err(PackError::CarrierTooLarge);
    }
    output.extend_from_slice(
        format!("\r\n{:08x} {capacity:08x} 7fffffff \r\n", data.len()).as_bytes(),
    );
    output.extend_from_slice(data);
    output.resize(output.len() + capacity - data.len(), 0);
    Ok(())
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut output = Vec::new();
    DeflateDecoder::new(data)
        .take((MAX_CARRIER_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|_| PackError::InternalTemplate)?;
    if output.len() > MAX_CARRIER_BYTES {
        return malformed();
    }
    Ok(output)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).map_err(|_| PackError::CompressionFailed)?;
    encoder.finish().map_err(|_| PackError::CompressionFailed)
}

pub(crate) fn pack(source: &str) -> Result<Vec<u8>, PackError> {
    let mut outer = Container::read(TEMPLATE)?;
    if outer.entries.len() != 6 {
        return malformed();
    }
    let module = outer
        .entries
        .iter_mut()
        .find(|entry| entry.name == MODULE_ENTRY)
        .ok_or(PackError::InternalTemplate)?;
    let mut nested = Container::read(&inflate(&module.data)?)?;
    if nested.entries.len() != 2 || !nested.entries.iter().any(|entry| entry.name == "info") {
        return malformed();
    }
    let text = nested
        .entries
        .iter_mut()
        .find(|entry| entry.name == "text")
        .ok_or(PackError::InternalTemplate)?;
    // The signature declares UTF-8; no source lines or line terminators are rewritten.
    let text_source = source.strip_prefix('\u{feff}').unwrap_or(source);
    text.data = BOM.iter().copied().chain(text_source.bytes()).collect();
    module.data = deflate(&nested.write()?)?;
    outer.write()
}

#[cfg(test)]
pub(crate) use tests::module_text;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn module_text(bytes: &[u8]) -> Vec<u8> {
        let outer = Container::read(bytes).unwrap();
        let module = outer.entries.iter().find(|entry| entry.name == MODULE_ENTRY).unwrap();
        let nested = Container::read(&inflate(&module.data).unwrap()).unwrap();
        nested.entries.iter().find(|entry| entry.name == "text").unwrap().data.clone()
    }

    #[test]
    fn reads_the_native_reference_and_roundtrips_its_entries() {
        let reference = Container::read(TEMPLATE).unwrap();
        assert_eq!(reference.entries.len(), 6);
        assert!(reference.entries.iter().any(|entry| entry.name == "root"));
        let encoded = reference.write().unwrap();
        assert_eq!(Container::read(&encoded).unwrap(), reference);
        assert_eq!(module_text(TEMPLATE), include_bytes!("../fixtures/reference-module.bin"));
    }

    #[test]
    fn packaging_changes_only_the_module_text_and_its_container_layout() {
        let source = "Procedure Check()\r\nReturn;\r\nEndProcedure";
        let packed = pack(source).unwrap();
        assert_eq!(pack(source).unwrap(), packed);
        let before = Container::read(TEMPLATE).unwrap();
        let after = Container::read(&packed).unwrap();
        for (before, after) in before.entries.iter().zip(after.entries.iter()) {
            assert_eq!(before.header, after.header);
            assert_eq!(before.name, after.name);
            if before.name != MODULE_ENTRY {
                assert_eq!(before.data, after.data);
            } else {
                let nested_before = Container::read(&inflate(&before.data).unwrap()).unwrap();
                let nested_after = Container::read(&inflate(&after.data).unwrap()).unwrap();
                for (before, after) in nested_before.entries.iter().zip(nested_after.entries.iter())
                {
                    assert_eq!(before.header, after.header);
                    if before.name != "text" {
                        assert_eq!(before.data, after.data);
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_truncation_and_corrupt_block_headers() {
        for length in [0, 15, 16, 30, 46, TEMPLATE.len() / 2] {
            assert_eq!(Container::read(&TEMPLATE[..length]), Err(PackError::InternalTemplate));
        }
        let mut broken = TEMPLATE.to_vec();
        broken[16] = b'!';
        assert_eq!(Container::read(&broken), Err(PackError::InternalTemplate));
        broken = TEMPLATE.to_vec();
        broken[18..26].copy_from_slice(b"ffffffff");
        assert_eq!(Container::read(&broken), Err(PackError::InternalTemplate));
    }

    #[test]
    fn rejects_oversized_containers_and_inflated_payloads() {
        assert_eq!(
            Container::read(&vec![0; MAX_CARRIER_BYTES + 1]),
            Err(PackError::InternalTemplate)
        );
        let compressed = deflate(&vec![0; MAX_CARRIER_BYTES + 1]).unwrap();
        assert_eq!(inflate(&compressed), Err(PackError::InternalTemplate));
        assert_eq!(
            write_block(&mut Vec::new(), &vec![0; MAX_CARRIER_BYTES], MAX_CARRIER_BYTES),
            Err(PackError::CarrierTooLarge)
        );
    }

    #[test]
    fn rejects_a_directory_cycle_without_following_it() {
        let mut broken = TEMPLATE.to_vec();
        broken[18..26].copy_from_slice(b"00000800");
        broken[36..44].copy_from_slice(b"00000010");
        assert_eq!(Container::read(&broken), Err(PackError::InternalTemplate));
    }
}
