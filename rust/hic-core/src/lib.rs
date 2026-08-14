//! UI-independent primitives for reading Juicebox `.hic` files.
//!
//! Supports header/footer metadata, matrix/block indexes, bounded zlib block
//! decoding, normalization vectors, and expected-value vectors. The real-data
//! verification tools compare these results against the Java reader.

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
const MAX_COMPRESSED_BLOCK_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECOMPRESSED_BLOCK_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BLOCK_RECORDS: usize = 16_000_000;
const MAX_DENSE_BLOCK_POINTS: usize = 64_000_000;
const MAX_NORMALIZATION_VECTOR_BYTES: usize = 256 * 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizationKey {
    pub normalization: String,
    pub chromosome: u32,
    pub unit: MatrixUnit,
    pub resolution: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizationIndexEntry {
    pub position: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizationVector {
    pub key: NormalizationKey,
    pub values: Vec<f64>,
}

/// Identity of a distance-dependent expected-value vector stored in a HIC
/// footer.  Unnormalized vectors use normalization == "NONE".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedValueKey {
    pub normalization: String,
    pub unit: MatrixUnit,
    pub resolution: u32,
}

/// Genome-wide expected contacts by diagonal distance, accompanied by the
/// per-chromosome scale factors used by Juicebox.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedValueVector {
    pub key: ExpectedValueKey,
    pub values: Vec<f64>,
    pub chromosome_factors: BTreeMap<u32, f64>,
}

impl ExpectedValueVector {
    /// Matches ExpectedValueFunctionImpl.getExpectedValue: use the final
    /// vector entry past its recorded range and divide by the chromosome factor
    /// when one exists.
    pub fn value_for(&self, chromosome: u32, distance: u64) -> Option<f64> {
        let value = *self
            .values
            .get(usize::try_from(distance).ok()?)
            .or_else(|| self.values.last())?;
        Some(
            value
                / self
                    .chromosome_factors
                    .get(&chromosome)
                    .copied()
                    .unwrap_or(1.0),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

impl MatrixZoom {
    /// Returns existing v8-style row-major blocks intersecting a bin-space
    /// rectangle. For intra-chromosomal matrices the transposed rectangle is
    /// included because files commonly store only one triangle.
    pub fn block_numbers_for_bounds(
        &self,
        bin_bounds: [i32; 4],
        include_transpose: bool,
    ) -> Vec<i32> {
        use std::collections::BTreeSet;

        let block_bins = self.block_bin_count.max(1) as i32;
        let columns = self.block_column_count.max(1) as i32;
        let mut selected = BTreeSet::new();
        let mut add_rectangle = |x1: i32, y1: i32, x2: i32, y2: i32| {
            let column_start = x1.max(0) / block_bins;
            let column_end = x2.max(0) / block_bins;
            let row_start = y1.max(0) / block_bins;
            let row_end = y2.max(0) / block_bins;
            for row in row_start..=row_end {
                for column in column_start..=column_end {
                    let number = row.saturating_mul(columns).saturating_add(column);
                    if self.blocks.contains_key(&number) {
                        selected.insert(number);
                    }
                }
            }
        };
        add_rectangle(bin_bounds[0], bin_bounds[1], bin_bounds[2], bin_bounds[3]);
        if include_transpose {
            add_rectangle(bin_bounds[1], bin_bounds[0], bin_bounds[3], bin_bounds[2]);
        }
        selected.into_iter().collect()
    }
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
    file: File,
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
    #[error("invalid .hic normalization data: {0}")]
    InvalidNormalization(&'static str),
}

impl HicFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HicError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let mut reader = BufReader::new(file.try_clone()?);
        let header = read_header(&mut reader)?;
        let master_index = read_master_index(&mut reader, &header)?;
        Ok(Self {
            path,
            file,
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
        let mut reader = BufReader::new(self.file.try_clone()?);
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
        if compressed_size > MAX_COMPRESSED_BLOCK_BYTES {
            return Err(HicError::InvalidBlock(
                "compressed block exceeds the safety limit",
            ));
        }
        let mut compressed = vec![0_u8; compressed_size];
        read_exact_at(&self.file, &mut compressed, entry.position)?;
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut decompressed = Vec::new();
        decoder
            .by_ref()
            .take(MAX_DECOMPRESSED_BLOCK_BYTES + 1)
            .read_to_end(&mut decompressed)?;
        if decompressed.len() as u64 > MAX_DECOMPRESSED_BLOCK_BYTES {
            return Err(HicError::InvalidBlock(
                "decompressed block exceeds the safety limit",
            ));
        }
        parse_block(&decompressed, self.header.version)
    }

    pub fn read_all_blocks(&self, zoom: &MatrixZoom) -> Result<Vec<ContactRecord>, HicError> {
        let mut records = Vec::new();
        for &block_number in zoom.blocks.keys() {
            records.extend(self.read_block(zoom, block_number)?);
        }
        Ok(records)
    }

    /// Reads the raw and normalized expected-value vectors in the footer.
    /// Values are promoted to f64, while preserving the v8 double versus v9+
    /// float on-disk representation exactly.
    pub fn read_expected_value_vectors(
        &self,
    ) -> Result<BTreeMap<ExpectedValueKey, ExpectedValueVector>, HicError> {
        let mut reader = BufReader::new(self.file.try_clone()?);
        reader.seek(SeekFrom::Start(self.header.footer_position))?;
        let footer_bytes = if self.header.version > 8 {
            read_u64(&mut reader)?
        } else {
            u64::from(read_u32(&mut reader)?)
        };
        let length_field_bytes = if self.header.version > 8 { 8 } else { 4 };
        let normalized_position = self
            .header
            .footer_position
            .checked_add(length_field_bytes)
            .and_then(|position| position.checked_add(footer_bytes))
            .ok_or(HicError::InvalidNormalization("footer offset overflow"))?;

        // The raw expected vectors follow the master index within the legacy
        // footer. Parsing rather than assuming its byte layout protects us
        // against v8/v9 index-entry-size differences.
        let master_count = read_count(&mut reader, "master index entry count")?;
        for _ in 0..master_count {
            let _key = read_c_string(&mut reader)?;
            let _position = read_u64(&mut reader)?;
            if self.header.version > 8 {
                let _size = read_u64(&mut reader)?;
            } else {
                let _size = read_u32(&mut reader)?;
            }
        }

        let mut vectors = BTreeMap::new();
        let raw_count = read_count(&mut reader, "raw expected vector count")?;
        for _ in 0..raw_count {
            let unit = read_matrix_unit(&mut reader)?;
            let resolution = read_u32(&mut reader)?;
            let values = read_expected_values(&mut reader, self.header.version)?;
            let factors = read_expected_factors(&mut reader, self.header.version)?;
            let key = ExpectedValueKey {
                normalization: "NONE".to_owned(),
                unit,
                resolution,
            };
            vectors.insert(
                key.clone(),
                ExpectedValueVector {
                    key,
                    values,
                    chromosome_factors: factors,
                },
            );
        }

        if self.header.version < 6 {
            return Ok(vectors);
        }
        reader.seek(SeekFrom::Start(normalized_position))?;
        let normalized_count = read_count(&mut reader, "normalized expected vector count")?;
        for _ in 0..normalized_count {
            let normalization = read_c_string(&mut reader)?;
            let unit = read_matrix_unit(&mut reader)?;
            let resolution = read_u32(&mut reader)?;
            let values = read_expected_values(&mut reader, self.header.version)?;
            let factors = read_expected_factors(&mut reader, self.header.version)?;
            let key = ExpectedValueKey {
                normalization,
                unit,
                resolution,
            };
            vectors.insert(
                key.clone(),
                ExpectedValueVector {
                    key,
                    values,
                    chromosome_factors: factors,
                },
            );
        }
        Ok(vectors)
    }

    pub fn read_expected_value_vector(
        &self,
        key: &ExpectedValueKey,
    ) -> Result<Option<ExpectedValueVector>, HicError> {
        Ok(self.read_expected_value_vectors()?.remove(key))
    }

    pub fn read_normalization_index(
        &self,
    ) -> Result<BTreeMap<NormalizationKey, NormalizationIndexEntry>, HicError> {
        if self.header.version < 6 {
            return Ok(BTreeMap::new());
        }
        let mut reader = BufReader::new(self.file.try_clone()?);
        reader.seek(SeekFrom::Start(self.header.footer_position))?;
        let footer_bytes = if self.header.version > 8 {
            read_u64(&mut reader)?
        } else {
            u64::from(read_u32(&mut reader)?)
        };
        let length_field_bytes = if self.header.version > 8 { 8 } else { 4 };
        let normalization_position = self
            .header
            .footer_position
            .checked_add(length_field_bytes)
            .and_then(|position| position.checked_add(footer_bytes))
            .ok_or(HicError::InvalidNormalization("footer offset overflow"))?;
        reader.seek(SeekFrom::Start(normalization_position))?;

        let normalized_expected_count =
            read_count(&mut reader, "normalized expected vector count")?;
        for _ in 0..normalized_expected_count {
            let _normalization = read_c_string(&mut reader)?;
            let _unit = read_matrix_unit(&mut reader)?;
            let _resolution = read_u32(&mut reader)?;
            let value_count = read_versioned_count(&mut reader, self.header.version)?;
            skip_bytes(
                &mut reader,
                value_count
                    .checked_mul(if self.header.version > 8 { 4 } else { 8 })
                    .ok_or(HicError::InvalidNormalization(
                        "expected vector size overflow",
                    ))?,
            )?;
            let factor_count = read_count(&mut reader, "normalization factor count")?;
            skip_bytes(
                &mut reader,
                factor_count
                    .checked_mul(if self.header.version > 8 { 8 } else { 12 })
                    .ok_or(HicError::InvalidNormalization("factor map size overflow"))?,
            )?;
        }

        let vector_count = read_count(&mut reader, "normalization vector count")?;
        let mut index = BTreeMap::new();
        for _ in 0..vector_count {
            let normalization = read_c_string(&mut reader)?;
            let chromosome = read_u32(&mut reader)?;
            let unit = read_matrix_unit(&mut reader)?;
            let resolution = read_u32(&mut reader)?;
            let position = read_u64(&mut reader)?;
            let size = if self.header.version > 8 {
                read_u64(&mut reader)?
            } else {
                u64::from(read_u32(&mut reader)?)
            };
            index.insert(
                NormalizationKey {
                    normalization,
                    chromosome,
                    unit,
                    resolution,
                },
                NormalizationIndexEntry { position, size },
            );
        }
        Ok(index)
    }

    pub fn read_normalization_vector(
        &self,
        key: &NormalizationKey,
    ) -> Result<Option<NormalizationVector>, HicError> {
        let index = self.read_normalization_index()?;
        let Some(entry) = index.get(key) else {
            return Ok(None);
        };
        let size = usize::try_from(entry.size)
            .map_err(|_| HicError::InvalidNormalization("vector is too large"))?;
        validate_indexed_payload(
            &self.file,
            entry.position,
            size,
            MAX_NORMALIZATION_VECTOR_BYTES,
            HicError::InvalidNormalization,
        )?;
        let mut bytes = vec![0_u8; size];
        read_exact_at(&self.file, &mut bytes, entry.position)?;
        let mut reader = Cursor::new(bytes);
        let value_count = read_versioned_count(&mut reader, self.header.version)?;
        if value_count > MAX_COLLECTION_ITEMS {
            return Err(HicError::InvalidCount {
                field: "normalization value count",
                value: value_count as i64,
            });
        }
        let mut values = Vec::with_capacity(value_count);
        let mut has_value = false;
        for _ in 0..value_count {
            let value = if self.header.version > 8 {
                f64::from(read_f32(&mut reader)?)
            } else {
                read_f64(&mut reader)?
            };
            has_value |= !value.is_nan();
            values.push(value);
        }
        Ok(has_value.then(|| NormalizationVector {
            key: key.clone(),
            values,
        }))
    }
}

fn validate_indexed_payload(
    file: &File,
    position: u64,
    size: usize,
    maximum_size: usize,
    invalid: fn(&'static str) -> HicError,
) -> Result<(), HicError> {
    if size > maximum_size {
        return Err(invalid("indexed payload exceeds the safety limit"));
    }
    let end = position
        .checked_add(size as u64)
        .ok_or_else(|| invalid("indexed payload range overflows"))?;
    if end > file.metadata()?.len() {
        return Err(invalid("indexed payload is outside the file"));
    }
    Ok(())
}

fn read_matrix_unit<R: Read>(reader: &mut R) -> Result<MatrixUnit, HicError> {
    match read_c_string(reader)?.as_str() {
        "BP" => Ok(MatrixUnit::BasePairs),
        "FRAG" => Ok(MatrixUnit::Fragments),
        unit => Err(HicError::InvalidUnit(unit.to_owned())),
    }
}

fn read_versioned_count<R: Read>(reader: &mut R, version: u32) -> Result<usize, HicError> {
    let value = if version > 8 {
        read_u64(reader)?
    } else {
        u64::from(read_u32(reader)?)
    };
    usize::try_from(value)
        .map_err(|_| HicError::InvalidNormalization("value count does not fit memory"))
}

fn read_expected_values<R: Read>(reader: &mut R, version: u32) -> Result<Vec<f64>, HicError> {
    let value_count = read_versioned_count(reader, version)?;
    if value_count > MAX_COLLECTION_ITEMS {
        return Err(HicError::InvalidCount {
            field: "expected value count",
            value: value_count as i64,
        });
    }
    (0..value_count)
        .map(|_| {
            if version > 8 {
                Ok(f64::from(read_f32(reader)?))
            } else {
                read_f64(reader)
            }
        })
        .collect()
}

fn read_expected_factors<R: Read>(
    reader: &mut R,
    version: u32,
) -> Result<BTreeMap<u32, f64>, HicError> {
    let factor_count = read_count(reader, "expected chromosome factor count")?;
    let mut factors = BTreeMap::new();
    for _ in 0..factor_count {
        let chromosome = read_u32(reader)?;
        let factor = if version > 8 {
            f64::from(read_f32(reader)?)
        } else {
            read_f64(reader)?
        };
        factors.insert(chromosome, factor);
    }
    Ok(factors)
}

fn skip_bytes<R: Seek>(reader: &mut R, bytes: usize) -> Result<(), HicError> {
    let offset = i64::try_from(bytes)
        .map_err(|_| HicError::InvalidNormalization("skip offset is too large"))?;
    reader.seek(SeekFrom::Current(offset))?;
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let read = file.seek_read(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to fill positional read buffer",
            ));
        }
        offset = offset.saturating_add(read as u64);
        buffer = &mut buffer[read..];
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let read = file.read_at(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to fill positional read buffer",
            ));
        }
        offset = offset.saturating_add(read as u64);
        buffer = &mut buffer[read..];
    }
    Ok(())
}

pub fn parse_block(bytes: &[u8], version: u32) -> Result<Vec<ContactRecord>, HicError> {
    let mut reader = Cursor::new(bytes);
    let record_count = read_nonnegative_i32(&mut reader, "negative record count")?;
    if record_count > MAX_BLOCK_RECORDS {
        return Err(HicError::InvalidBlock(
            "record count exceeds the safety limit",
        ));
    }
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
            if point_count > MAX_DENSE_BLOCK_POINTS {
                return Err(HicError::InvalidBlock(
                    "dense point count exceeds the safety limit",
                ));
            }
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

fn read_f64<R: Read>(reader: &mut R) -> Result<f64, HicError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(f64::from_le_bytes(bytes))
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
    use std::{
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn push_string(buffer: &mut Vec<u8>, value: &str) {
        buffer.extend_from_slice(value.as_bytes());
        buffer.push(0);
    }

    fn expected_test_file(version: u32, truncate_last_byte: bool) -> PathBuf {
        let mut bytes = Vec::new();
        push_string(&mut bytes, "HIC");
        bytes.extend_from_slice(&version.to_le_bytes());
        let footer_offset_position = bytes.len();
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        push_string(&mut bytes, "test-genome");
        if version > 8 {
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u32.to_le_bytes()); // attributes
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        push_string(&mut bytes, "All");
        if version > 8 {
            bytes.extend_from_slice(&1_000_u64.to_le_bytes());
        } else {
            bytes.extend_from_slice(&1_000_u32.to_le_bytes());
        }
        push_string(&mut bytes, "chr1");
        if version > 8 {
            bytes.extend_from_slice(&1_000_u64.to_le_bytes());
        } else {
            bytes.extend_from_slice(&1_000_u32.to_le_bytes());
        }
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&100_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());

        let footer = bytes.len() as u64;
        bytes[footer_offset_position..footer_offset_position + 8]
            .copy_from_slice(&footer.to_le_bytes());
        let length_position = bytes.len();
        if version > 8 {
            bytes.extend_from_slice(&0_u64.to_le_bytes());
        } else {
            bytes.extend_from_slice(&0_u32.to_le_bytes());
        }
        let legacy_start = bytes.len();
        bytes.extend_from_slice(&0_u32.to_le_bytes()); // master index
        bytes.extend_from_slice(&1_u32.to_le_bytes()); // raw expected
        push_string(&mut bytes, "BP");
        bytes.extend_from_slice(&100_u32.to_le_bytes());
        if version > 8 {
            bytes.extend_from_slice(&2_u64.to_le_bytes());
            bytes.extend_from_slice(&4.0_f32.to_le_bytes());
            bytes.extend_from_slice(&2.0_f32.to_le_bytes());
        } else {
            bytes.extend_from_slice(&2_u32.to_le_bytes());
            bytes.extend_from_slice(&4.0_f64.to_le_bytes());
            bytes.extend_from_slice(&2.0_f64.to_le_bytes());
        }
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        if version > 8 {
            bytes.extend_from_slice(&2.0_f32.to_le_bytes());
        } else {
            bytes.extend_from_slice(&2.0_f64.to_le_bytes());
        }
        let legacy_bytes = bytes.len() - legacy_start;
        if version > 8 {
            bytes[length_position..length_position + 8]
                .copy_from_slice(&(legacy_bytes as u64).to_le_bytes());
        } else {
            bytes[length_position..length_position + 4]
                .copy_from_slice(&(legacy_bytes as u32).to_le_bytes());
        }

        bytes.extend_from_slice(&1_u32.to_le_bytes()); // normalized expected
        push_string(&mut bytes, "KR");
        push_string(&mut bytes, "BP");
        bytes.extend_from_slice(&100_u32.to_le_bytes());
        if version > 8 {
            bytes.extend_from_slice(&2_u64.to_le_bytes());
            bytes.extend_from_slice(&8.0_f32.to_le_bytes());
            bytes.extend_from_slice(&3.0_f32.to_le_bytes());
        } else {
            bytes.extend_from_slice(&2_u32.to_le_bytes());
            bytes.extend_from_slice(&8.0_f64.to_le_bytes());
            bytes.extend_from_slice(&3.0_f64.to_le_bytes());
        }
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        if version > 8 {
            bytes.extend_from_slice(&4.0_f32.to_le_bytes());
        } else {
            bytes.extend_from_slice(&4.0_f64.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u32.to_le_bytes()); // normalization vectors
        if truncate_last_byte {
            // Remove the normalization-vector count and one byte of the final
            // expected factor so expected parsing itself observes the EOF.
            bytes.truncate(bytes.len() - 5);
        }

        let sequence = TEST_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hic-core-expected-{}-{version}-{sequence}.hic",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn assert_expected_footer(version: u32) {
        let path = expected_test_file(version, false);
        let file = HicFile::open(&path).unwrap();
        let vectors = file.read_expected_value_vectors().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(vectors.len(), 2);
        let raw = &vectors[&ExpectedValueKey {
            normalization: "NONE".to_owned(),
            unit: MatrixUnit::BasePairs,
            resolution: 100,
        }];
        assert_eq!(raw.values, vec![4.0, 2.0]);
        assert_eq!(raw.chromosome_factors[&1], 2.0);
        assert_eq!(raw.value_for(1, 0), Some(2.0));
        assert_eq!(raw.value_for(1, 99), Some(1.0));
        let normalized = &vectors[&ExpectedValueKey {
            normalization: "KR".to_owned(),
            unit: MatrixUnit::BasePairs,
            resolution: 100,
        }];
        assert_eq!(normalized.values, vec![8.0, 3.0]);
        assert_eq!(normalized.value_for(1, 0), Some(2.0));
        assert_eq!(normalized.value_for(1, 99), Some(0.75));
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
    fn reads_v8_raw_and_normalized_expected_footer() {
        assert_expected_footer(8);
    }

    #[test]
    fn reads_v9_float_expected_footer() {
        assert_expected_footer(9);
    }

    #[test]
    fn rejects_truncated_expected_footer() {
        let path = expected_test_file(9, true);
        let file = HicFile::open(&path).unwrap();
        let error = file.read_expected_value_vectors().unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(error, HicError::Io(ref io) if io.kind() == io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn rejects_block_record_count_above_safety_limit_before_allocation() {
        let bytes = ((MAX_BLOCK_RECORDS + 1) as i32).to_le_bytes();
        assert!(matches!(
            parse_block(&bytes, 8),
            Err(HicError::InvalidBlock(
                "record count exceeds the safety limit"
            ))
        ));
    }

    #[test]
    fn rejects_dense_point_count_above_safety_limit() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.push(0);
        bytes.push(2);
        bytes.extend_from_slice(&((MAX_DENSE_BLOCK_POINTS + 1) as i32).to_le_bytes());
        bytes.extend_from_slice(&1_i16.to_le_bytes());
        assert!(matches!(
            parse_block(&bytes, 8),
            Err(HicError::InvalidBlock(
                "dense point count exceeds the safety limit"
            ))
        ));
    }

    #[test]
    fn rejects_indexed_payload_above_limit_before_allocation() {
        let path =
            std::env::temp_dir().join(format!("hic-core-index-limit-{}.bin", std::process::id()));
        std::fs::write(&path, [0_u8; 1]).unwrap();
        let file = File::open(&path).unwrap();
        let error = validate_indexed_payload(
            &file,
            0,
            MAX_NORMALIZATION_VECTOR_BYTES + 1,
            MAX_NORMALIZATION_VECTOR_BYTES,
            HicError::InvalidNormalization,
        )
        .unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            error,
            HicError::InvalidNormalization("indexed payload exceeds the safety limit")
        ));
    }

    #[test]
    fn rejects_indexed_payload_outside_file() {
        let path =
            std::env::temp_dir().join(format!("hic-core-index-range-{}.bin", std::process::id()));
        std::fs::write(&path, [0_u8; 4]).unwrap();
        let file = File::open(&path).unwrap();
        let error = validate_indexed_payload(
            &file,
            3,
            2,
            MAX_NORMALIZATION_VECTOR_BYTES,
            HicError::InvalidNormalization,
        )
        .unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            error,
            HicError::InvalidNormalization("indexed payload is outside the file")
        ));
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
