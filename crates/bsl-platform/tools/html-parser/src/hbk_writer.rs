//! Writes help archive containers for tests: the inverse of [`crate::hbk`],
//! kept independent of the reader so that one mistake cannot hide in both.

use std::io::{Cursor, Write};

const DESCRIPTOR_SIZE: usize = 12;
const BLOCK_HEADER_SIZE: usize = 31;
const SPLITTER: u32 = i32::MAX as u32;

/// Builds a container from scratch: a descriptor block, then for each entity a
/// name header and a body split into `chunk`-byte blocks chained by offset.
pub fn build_container(entities: &[(&str, Option<&[u8]>)], chunk: usize) -> Vec<u8> {
    fn block(out: &mut Vec<u8>, payload_size: usize, data: &[u8], next: u32) {
        out.extend_from_slice(
            format!("\r\n{:08x} {:08x} {:08x} \r\n", payload_size, data.len(), next).as_bytes(),
        );
        out.extend_from_slice(data);
    }
    fn chain(out: &mut Vec<u8>, data: &[u8], chunk: usize) -> u32 {
        let start = out.len() as u32;
        let pieces: Vec<&[u8]> =
            if data.is_empty() { vec![&[][..]] } else { data.chunks(chunk).collect() };
        for (index, piece) in pieces.iter().enumerate() {
            let here = out.len();
            let next_offset = here + BLOCK_HEADER_SIZE + piece.len();
            let next = if index + 1 < pieces.len() { next_offset as u32 } else { SPLITTER };
            let declared = if index == 0 { data.len() } else { 0 };
            block(out, declared, piece, next);
        }
        start
    }

    let descriptors_len = entities.len() * DESCRIPTOR_SIZE;
    let mut out = Vec::new();
    out.extend_from_slice(&SPLITTER.to_le_bytes());
    out.extend_from_slice(&512u32.to_le_bytes());
    out.extend_from_slice(&(entities.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let descriptors_at = out.len();
    block(&mut out, descriptors_len, &vec![0; descriptors_len], SPLITTER);

    let mut descriptors = Vec::new();
    for (name, body) in entities {
        let mut header = vec![0u8; 20];
        for unit in name.encode_utf16() {
            header.extend_from_slice(&unit.to_le_bytes());
        }
        header.extend_from_slice(&[0; 4]);
        let header_offset = chain(&mut out, &header, chunk.max(header.len()));
        let body_offset = match body {
            Some(body) => chain(&mut out, body, chunk),
            None => SPLITTER,
        };
        descriptors.extend_from_slice(&header_offset.to_le_bytes());
        descriptors.extend_from_slice(&body_offset.to_le_bytes());
        descriptors.extend_from_slice(&SPLITTER.to_le_bytes());
    }
    let body_at = descriptors_at + BLOCK_HEADER_SIZE;
    out[body_at..body_at + descriptors_len].copy_from_slice(&descriptors);
    out
}

pub fn build_zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in files {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
