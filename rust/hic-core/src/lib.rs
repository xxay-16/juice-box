//! UI-independent primitives for reading Juicebox `.hic` files.
//!
//! The first migration slice deliberately implements metadata and master-index
//! parsing only. Matrix and compressed block decoding will be added behind the
//! same API after byte-for-byte comparison against the Java reader.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use thiserror::Error;

const MAX_STRING_BYTES: usize = 1 << 20;
const MAX_COLLECTION_ITEMS: usize = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chromosome {
    pub index: u32,
    pub name: String,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HicHeader {
    pub version: u32,
    pub footer_position: u64,
    pub genome_id: String,
    pub normalization_index: Option<(u64, u64)>,
    pub attributes: BTreeMap<String, String>,
    pub chromosomes: Vec<Chromosome>,
    pub bp_resolutions: Vec<u32>,
    pub fragment_resolutions: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub position: u64,
    pub size: u64,
}

#[derive(Debug)]
pub struct HicFile {
    path: PathBuf,
    pub header: HicHeader,
    pub master_index: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Error)]
pub enum HicError {
    #[error("I/O error while reading .hic file: {0}")]
    Io(#[from] io::Error),
    #[error("invalid .hic magic: expected HIC, got {0:?}")]
    InvalidMagic(String),
    #[error("unsupported or unreasonable {field}: {value}")]
    InvalidCount { field: &'static str, value: i64 },
    #[error("unterminated or oversized string in .hic file")]
    InvalidString,
}

impl HicFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HicError> {
        let path = path.as_ref().to_path_buf();
        let mut reader = BufReader::new(File::open(&path)?);
        let header = read_header(&mut reader)?;
        let master_index = read_master_index(&mut reader, &header)?;
        Ok(Self {
            path,
            header,
            master_index,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn read_header<R: Read + Seek>(reader: &mut R) -> Result<HicHeader, HicError> {
    reader.seek(SeekFrom::Start(0))?;
    let magic = read_c_string(reader)?;
    if magic != "HIC" {
        return Err(HicError::InvalidMagic(magic));
    }

    let version = read_u32(reader)?;
    let footer_position = read_u64(reader)?;
    let genome_id = read_c_string(reader)?;
    let normalization_index = if version > 8 {
        Some((read_u64(reader)?, read_u64(reader)?))
    } else {
        None
    };

    let attributes = if version > 4 {
        let count = read_count(reader, "attribute count")?;
        let mut values = BTreeMap::new();
        for _ in 0..count {
            values.insert(read_c_string(reader)?, read_c_string(reader)?);
        }
        values
    } else {
        BTreeMap::new()
    };

    let chromosome_count = read_count(reader, "chromosome count")?;
    let mut chromosomes = Vec::with_capacity(chromosome_count);
    for index in 0..chromosome_count {
        let name = read_c_string(reader)?;
        let length = if version > 8 {
            read_u64(reader)?
        } else {
            u64::from(read_u32(reader)?)
        };
        chromosomes.push(Chromosome {
            index: index as u32,
            name,
            length,
        });
    }

    Ok(HicHeader {
        version,
        footer_position,
        genome_id,
        normalization_index,
        attributes,
        chromosomes,
        bp_resolutions: read_u32_vec(reader, "BP resolution count")?,
        fragment_resolutions: read_u32_vec(reader, "fragment resolution count")?,
    })
}

pub fn read_master_index<R: Read + Seek>(
    reader: &mut R,
    header: &HicHeader,
) -> Result<BTreeMap<String, IndexEntry>, HicError> {
    reader.seek(SeekFrom::Start(header.footer_position))?;
    if header.version > 8 {
        let _footer_byte_count = read_u64(reader)?;
    } else {
        let _footer_byte_count = read_u32(reader)?;
    }

    let count = read_count(reader, "master index entry count")?;
    let mut index = BTreeMap::new();
    for _ in 0..count {
        let key = read_c_string(reader)?;
        let position = read_u64(reader)?;
        let size = if header.version > 8 {
            read_u64(reader)?
        } else {
            u64::from(read_u32(reader)?)
        };
        index.insert(key, IndexEntry { position, size });
    }
    Ok(index)
}

fn read_u32_vec<R: Read>(reader: &mut R, field: &'static str) -> Result<Vec<u32>, HicError> {
    let count = read_count(reader, field)?;
    (0..count).map(|_| read_u32(reader)).collect()
}

fn read_count<R: Read>(reader: &mut R, field: &'static str) -> Result<usize, HicError> {
    let value = read_u32(reader)? as usize;
    if value > MAX_COLLECTION_ITEMS {
        return Err(HicError::InvalidCount {
            field,
            value: value as i64,
        });
    }
    Ok(value)
}

fn read_c_string<R: Read>(reader: &mut R) -> Result<String, HicError> {
    let mut bytes = Vec::new();
    for _ in 0..MAX_STRING_BYTES {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        if byte[0] == 0 {
            return String::from_utf8(bytes).map_err(|_| HicError::InvalidString);
        }
        bytes.push(byte[0]);
    }
    Err(HicError::InvalidString)
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, HicError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, HicError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn push_string(buffer: &mut Vec<u8>, value: &str) {
        buffer.extend_from_slice(value.as_bytes());
        buffer.push(0);
    }

    #[test]
    fn reads_v8_header_and_master_index() {
        let mut bytes = Vec::new();
        push_string(&mut bytes, "HIC");
        bytes.extend_from_slice(&8_u32.to_le_bytes());
        let footer_offset_position = bytes.len();
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        push_string(&mut bytes, "test-genome");
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        push_string(&mut bytes, "software");
        push_string(&mut bytes, "juicebox");
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        push_string(&mut bytes, "chr1");
        bytes.extend_from_slice(&1000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&5000_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());

        let footer = bytes.len() as u64;
        bytes[footer_offset_position..footer_offset_position + 8]
            .copy_from_slice(&footer.to_le_bytes());
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        push_string(&mut bytes, "0_0");
        bytes.extend_from_slice(&123_u64.to_le_bytes());
        bytes.extend_from_slice(&45_u32.to_le_bytes());

        let mut cursor = Cursor::new(bytes);
        let header = read_header(&mut cursor).unwrap();
        let index = read_master_index(&mut cursor, &header).unwrap();
        assert_eq!(header.version, 8);
        assert_eq!(header.chromosomes[0].length, 1000);
        assert_eq!(header.bp_resolutions, vec![5000]);
        assert_eq!(index["0_0"].position, 123);
    }
}
