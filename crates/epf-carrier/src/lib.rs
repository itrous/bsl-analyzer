//! A bounded, internal EPF carrier for a synthetic server object module.
//!
//! Admission does not compile or execute BSL. The resulting bytes still require
//! native compilation in safe mode; they do not prove another owner's validity.
#![deny(missing_docs)]

mod admission;
mod container;

pub use admission::{admit, AdmissionError, AdmissionReason};

/// Maximum UTF-8 input accepted by this carrier, including an optional BOM.
pub const MAX_MODULE_BYTES: usize = 1024 * 1024;
/// Maximum complete EPF size produced by this carrier.
pub const MAX_CARRIER_BYTES: usize = 2 * 1024 * 1024;
/// Processing name of the embedded internal template, used to attribute native compiler frames.
pub const INTERNAL_PROCESSOR_NAME: &str = "BSLProbe_valid";

/// One-based source position; the column counts UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePosition {
    /// One-based source line.
    pub line: u32,
    /// One-based UTF-16 column in the unmodified source.
    pub column: u32,
}

/// Source admitted for the fixed synthetic server object context.
///
/// Its private field prevents packaging unvalidated text through the public API.
pub struct AdmittedModule<'a> {
    source: &'a str,
}

impl std::fmt::Debug for AdmittedModule<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedModule").field("source_bytes", &self.source.len()).finish()
    }
}

impl AdmittedModule<'_> {
    /// The original text, without wrapper lines or newline normalization.
    pub fn source(&self) -> &str {
        self.source
    }

    /// Projects a UTF-8 byte boundary into the original UTF-16 coordinates.
    pub fn position(&self, byte_offset: usize) -> Option<SourcePosition> {
        source_position(self.source, byte_offset)
    }

    /// Packages this admitted source using only the internal native template.
    /// No path, external processing file, or executable is accepted or opened.
    pub fn pack(&self) -> Result<Vec<u8>, PackError> {
        container::pack(self.source)
    }
}

/// Internal carrier construction failure, distinct from invalid BSL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackError {
    /// The embedded reference no longer matches its bounded internal format.
    InternalTemplate,
    /// The generated carrier exceeds the fixed byte ceiling.
    CarrierTooLarge,
    /// Compression could not complete.
    CompressionFailed,
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InternalTemplate => "The internal EPF template is malformed",
            Self::CarrierTooLarge => "The internal EPF carrier exceeds its byte limit",
            Self::CompressionFailed => "The internal EPF carrier could not be compressed",
        })
    }
}

impl std::error::Error for PackError {}

fn source_position(source: &str, byte_offset: usize) -> Option<SourcePosition> {
    if byte_offset > source.len() || !source.is_char_boundary(byte_offset) {
        return None;
    }
    let index = line_index::LineIndex::new(source);
    let point = index.try_line_col(line_index::TextSize::new(byte_offset.try_into().ok()?))?;
    Some(SourcePosition {
        line: point.line + 1,
        column: index.utf16_col(source, point.line, point.col) + 1,
    })
}

#[cfg(test)]
mod tests;
