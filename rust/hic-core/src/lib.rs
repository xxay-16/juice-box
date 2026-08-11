//! UI-independent primitives for reading Juicebox `.hic` files.
//!
//! The first migration slice deliberately implements metadata and master-index
//! parsing only. Matrix and compressed block decoding will be added behind the
//! same API after byte-for-byte comparison against the Java reader.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use flate2::read::ZlibDecoder;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixUnit {
    BasePairs,
    Fragments,
}

#[derive(Debug, Clone)]
pub struct MatrixZoom {
    pub unit: MatrixUnit,
    pub bin_size: u32,
    pub sum_counts: f32,
    pub occupied_cell_count: f32,
    pub standard_deviation: f32,
    pub percentile_95: f32,
    pub block_bin_count: u32,
    pub block_column_count: u32,
    pub blocks: BTreeMap<i32, IndexEntry>,
}

#[derive(Debug, Clone)]
pub struct Matrix {
    pub chromosome_1: u32,
    pub chromosome_2: u32,
    pub zooms: Vec<MatrixZoom>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactRecord {
    pub bin_x: i32,
    pub bin_y: i32,
    pub counts: f32,
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
    #[error("matrix {0} is not present in the .hic master index")]
    MissingMatrix(String),
    #[error("block {block} is not present at {unit:?}/{bin_size}")]
    MissingBlock {
        block: i32,
        unit: MatrixUnit,
        bin_size: u32,
    },
    #[error("invalid .hic matrix unit {0:?}")]
    InvalidUnit(String),
    #[error("invalid or truncated .hic block: {0}")]
    InvalidBlock(&'static str),
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

    pub fn read_matrix(&self, key: &str) -> Result<Matrix, HicError> {
        let entry = self
            .master_index
            .get(key)
            .ok_or_else(|| HicError::MissingMatrix(key.to_owned()))?;
        let mut reader = BufReader::new(File::open(&self.path)?);
        reader.seek(SeekFrom::Start(entry.position))?;
        let chromosome_1 = read_u32(&mut reader)?;
        let chromosome_2 = read_u32(&mut reader)?;
        let zoom_count = read_count(&mut reader, "matrix zoom count")?;
        let mut zooms = Vec::with_capacity(zoom_count);
        for _ in 0..zoom_count {
            let unit_text = read_c_string(&mut reader)?;
            let unit = match unit_text.as_str() {
                "BP" => MatrixUnit::BasePairs,
                "FRAG" => MatrixUnit::Fragments,
                _ => return Err(HicError::InvalidUnit(unit_text)),
            };
            let _legacy_zoom_index = read_u32(&mut reader)?;
            let sum_counts = read_f32(&mut reader)?;
            let occupied_cell_count = read_f32(&mut reader)?;
            let standard_deviation = read_f32(&mut reader)?;
            let percentile_95 = read_f32(&mut reader)?;
            let bin_size = read_u32(&mut reader)?;
            let block_bin_count = read_u32(&mut reader)?;
            let block_column_count = read_u32(&mut reader)?;
            let block_count = read_count(&mut reader, "matrix block count")?;
            let mut blocks = BTreeMap::new();
            for _ in 0..block_count {
                let number = read_i32(&mut reader)?;
                let position = read_u64(&mut reader)?;
                let size = u64::from(read_u32(&mut reader)?);
                blocks.insert(number, IndexEntry { position, size });
            }
            zooms.push(MatrixZoom {
                unit,
                bin_size,
                sum_counts,
                occupied_cell_count,
                standard_deviation,
                percentile_95,
                block_bin_count,
                block_column_count,
                blocks,
            });
        }
        Ok(Matrix {
            chromosome_1,
            chromosome_2,
            zooms,
        })
    }

    pub fn read_block(
        &self,
        zoom: &MatrixZoom,
        block_number: i32,
    ) -> Result<Vec<ContactRecord>, HicError> {
        let entry = zoom
            .blocks
            .get(&block_number)
            .ok_or(HicError::MissingBlock {
                block: block_number,
                unit: zoom.unit,
                bin_size: zoom.bin_size,
            })?;
        let compressed_size = usize::try_from(entry.size)
            .map_err(|_| HicError::InvalidBlock("compressed block is too large"))?;
        let mut compressed = vec![0_u8; compressed_size];
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(entry.position))?;
        file.read_exact(&mut compressed)?;
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;
        parse_block(&decompressed, self.header.version)
    }

    pub fn read_all_blocks(&self, zoom: &MatrixZoom) -> Result<Vec<ContactRecord>, HicError> {
        let mut records = Vec::new();
        for &block_number in zoom.blocks.keys() {
            records.extend(self.read_block(zoom, block_number)?);
        }
        Ok(records)
    }
}

pub fn parse_block(bytes: &[u8], version: u32) -> Result<Vec<ContactRecord>, HicError> {
    let mut reader = Cursor::new(bytes);
    let record_count = read_nonnegative_i32(&mut reader, "negative record count")?;
    let mut records = Vec::with_capacity(record_count);
    if version < 7 {
        for _ in 0..record_count {
            records.push(ContactRecord {
                bin_x: read_i32(&mut reader)?,
                bin_y: read_i32(&mut reader)?,
                counts: read_f32(&mut reader)?,
            });
        }
        return Ok(records);
    }

    let bin_x_offset = read_i32(&mut reader)?;
    let bin_y_offset = read_i32(&mut reader)?;
    let use_short_counts = read_u8(&mut reader)? == 0;
    let (use_short_bin_x, use_short_bin_y) = if version > 8 {
        (read_u8(&mut reader)? == 0, read_u8(&mut reader)? == 0)
    } else {
        (true, true)
    };
    match read_u8(&mut reader)? {
        1 => {
            let row_count = read_coord_count(&mut reader, use_short_bin_y)?;
            for _ in 0..row_count {
                let bin_y = bin_y_offset + read_coordinate(&mut reader, use_short_bin_y)?;
                let column_count = read_coord_count(&mut reader, use_short_bin_x)?;
                for _ in 0..column_count {
                    let bin_x = bin_x_offset + read_coordinate(&mut reader, use_short_bin_x)?;
                    let counts = read_counts(&mut reader, use_short_counts)?;
                    records.push(ContactRecord {
                        bin_x,
                        bin_y,
                        counts,
                    });
                }
            }
        }
        2 => {
            let point_count = read_nonnegative_i32(&mut reader, "negative dense point count")?;
            let width = read_nonnegative_i16(&mut reader, "non-positive dense width")?;
            if width == 0 {
                return Err(HicError::InvalidBlock("zero dense width"));
            }
            for index in 0..point_count {
                let row = index / width;
                let column = index - row * width;
                let bin_x = bin_x_offset + column as i32;
                let bin_y = bin_y_offset + row as i32;
                if use_short_counts {
                    let counts = read_i16(&mut reader)?;
                    if counts != i16::MIN {
                        records.push(ContactRecord {
                            bin_x,
                            bin_y,
                            counts: f32::from(counts),
                        });
                    }
                } else {
                    let counts = read_f32(&mut reader)?;
                    if !counts.is_nan() {
                        records.push(ContactRecord {
                            bin_x,
                            bin_y,
                            counts,
                        });
                    }
                }
            }
        }
        _ => return Err(HicError::InvalidBlock("unknown block representation")),
    }
    if records.len() != record_count {
        return Err(HicError::InvalidBlock(
            "decoded record count differs from declared count",
        ));
    }
    Ok(records)
}

fn read_coordinate<R: Read>(reader: &mut R, short: bool) -> Result<i32, HicError> {
    if short {
        Ok(i32::from(read_i16(reader)?))
    } else {
        read_i32(reader)
    }
}

fn read_coord_count<R: Read>(reader: &mut R, short: bool) -> Result<usize, HicError> {
    if short {
        read_nonnegative_i16(reader, "negative sparse coordinate count")
    } else {
        read_nonnegative_i32(reader, "negative sparse coordinate count")
    }
}

fn read_counts<R: Read>(reader: &mut R, short: bool) -> Result<f32, HicError> {
    if short {
        Ok(f32::from(read_i16(reader)?))
    } else {
        read_f32(reader)
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

fn read_i16<R: Read>(reader: &mut R) -> Result<i16, HicError> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(i16::from_le_bytes(bytes))
}

fn read_i32<R: Read>(reader: &mut R) -> Result<i32, HicError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_f32<R: Read>(reader: &mut R) -> Result<f32, HicError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(f32::from_le_bytes(bytes))
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, HicError> {
    let mut byte = [0_u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_nonnegative_i16<R: Read>(reader: &mut R, message: &'static str) -> Result<usize, HicError> {
    usize::try_from(read_i16(reader)?).map_err(|_| HicError::InvalidBlock(message))
}

fn read_nonnegative_i32<R: Read>(reader: &mut R, message: &'static str) -> Result<usize, HicError> {
    usize::try_from(read_i32(reader)?).map_err(|_| HicError::InvalidBlock(message))
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

    #[test]
    fn decodes_v8_sparse_short_block() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2_i32.to_le_bytes());
        bytes.extend_from_slice(&10_i32.to_le_bytes());
        bytes.extend_from_slice(&20_i32.to_le_bytes());
        bytes.push(0);
        bytes.push(1);
        bytes.extend_from_slice(&1_i16.to_le_bytes());
        bytes.extend_from_slice(&3_i16.to_le_bytes());
        bytes.extend_from_slice(&2_i16.to_le_bytes());
        bytes.extend_from_slice(&4_i16.to_le_bytes());
        bytes.extend_from_slice(&7_i16.to_le_bytes());
        bytes.extend_from_slice(&9_i16.to_le_bytes());
        bytes.extend_from_slice(&11_i16.to_le_bytes());
        assert_eq!(
            parse_block(&bytes, 8).unwrap(),
            vec![
                ContactRecord {
                    bin_x: 14,
                    bin_y: 23,
                    counts: 7.0
                },
                ContactRecord {
                    bin_x: 19,
                    bin_y: 23,
                    counts: 11.0
                },
            ]
        );
    }

    #[test]
    fn decodes_v8_dense_float_block_and_ignores_nan() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2_i32.to_le_bytes());
        bytes.extend_from_slice(&5_i32.to_le_bytes());
        bytes.extend_from_slice(&8_i32.to_le_bytes());
        bytes.push(1);
        bytes.push(2);
        bytes.extend_from_slice(&3_i32.to_le_bytes());
        bytes.extend_from_slice(&2_i16.to_le_bytes());
        bytes.extend_from_slice(&1.5_f32.to_le_bytes());
        bytes.extend_from_slice(&f32::NAN.to_le_bytes());
        bytes.extend_from_slice(&2.5_f32.to_le_bytes());
        assert_eq!(
            parse_block(&bytes, 8).unwrap(),
            vec![
                ContactRecord {
                    bin_x: 5,
                    bin_y: 8,
                    counts: 1.5
                },
                ContactRecord {
                    bin_x: 5,
                    bin_y: 9,
                    counts: 2.5
                },
            ]
        );
    }
}
