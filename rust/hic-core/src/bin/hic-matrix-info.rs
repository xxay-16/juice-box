use std::path::PathBuf;

use hic_core::{ExpectedValueKey, HicFile, MatrixUnit, NormalizationKey};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("usage: hic-matrix-info <file.hic> [matrix-key]")?;
    let file = HicFile::open(&path)?;
    let key = arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "1_1".to_owned());
    let matrix = file.read_matrix(&key)?;
    println!("key={key}");
    println!(
        "chromosomes={}_{}",
        matrix.chromosome_1, matrix.chromosome_2
    );
    for normalization in ["NONE", "KR", "VC", "VC_SQRT"] {
        for zoom in &matrix.zooms {
            let norm_vector = if normalization == "NONE" {
                None
            } else {
                file.read_normalization_vector(&NormalizationKey {
                    normalization: normalization.to_owned(),
                    chromosome: matrix.chromosome_1,
                    unit: MatrixUnit::BasePairs,
                    resolution: zoom.bin_size,
                })?
            };
            let mut records: usize = 0;
            let mut finite: usize = 0;
            let mut stored_count_sum = 0.0_f64;
            let mut symmetric_count_sum = 0.0_f64;
            let mut fingerprint = 0xcbf29ce484222325_u64;
            for &block in zoom.blocks.keys() {
                for mut record in file.read_block(zoom, block)? {
                    if let Some(vector) = &norm_vector {
                        let denominator = vector.values[record.bin_x as usize]
                            * vector.values[record.bin_y as usize];
                        record.counts = (f64::from(record.counts) / denominator) as f32;
                        if record.counts.is_nan() {
                            continue;
                        }
                    }
                    records += 1;
                    let counts = f64::from(record.counts);
                    if record.counts.is_finite() {
                        finite += 1;
                        stored_count_sum += counts;
                    }
                    symmetric_count_sum += if record.bin_x == record.bin_y {
                        counts
                    } else {
                        2.0 * counts
                    };
                    for value in [
                        record.bin_x as u32,
                        record.bin_y as u32,
                        record.counts.to_bits(),
                    ] {
                        fingerprint ^= u64::from(value);
                        fingerprint = fingerprint.wrapping_mul(0x100000001b3);
                    }
                }
            }
            println!(
                "norm={} unit={:?} bin_size={} block_bins={} block_columns={} blocks={} records={} finite={} stored_count_sum={:.9} symmetric_count_sum={:.9} fingerprint={:016x} declared_sum_counts={}",
                normalization,
                zoom.unit,
                zoom.bin_size,
                zoom.block_bin_count,
                zoom.block_column_count,
                zoom.blocks.len(),
                records,
                finite,
                stored_count_sum,
                symmetric_count_sum,
                fingerprint,
                zoom.sum_counts
            );

            let expected = file.read_expected_value_vector(&ExpectedValueKey {
                normalization: normalization.to_owned(),
                unit: zoom.unit,
                resolution: zoom.bin_size,
            })?;
            if let Some(expected) = expected {
                let mut oe_records = 0_usize;
                let mut oe_finite = 0_usize;
                let mut oe_sum = 0.0_f64;
                let mut oe_fingerprint = 0xcbf29ce484222325_u64;
                for &block in zoom.blocks.keys() {
                    for mut record in file.read_block(zoom, block)? {
                        if let Some(vector) = &norm_vector {
                            let denominator = vector.values[record.bin_x as usize]
                                * vector.values[record.bin_y as usize];
                            record.counts = (f64::from(record.counts) / denominator) as f32;
                            if record.counts.is_nan() {
                                continue;
                            }
                        }
                        let distance = u64::from((record.bin_x - record.bin_y).unsigned_abs());
                        let Some(expected_count) =
                            expected.value_for(matrix.chromosome_1, distance)
                        else {
                            continue;
                        };
                        // HeatmapRenderer stores the ratio in a Java float.
                        let oe = (f64::from(record.counts) / expected_count) as f32;
                        if oe.is_nan() {
                            continue;
                        }
                        oe_records += 1;
                        if oe.is_finite() {
                            oe_finite += 1;
                            oe_sum += f64::from(oe);
                        }
                        for value in [record.bin_x as u32, record.bin_y as u32, oe.to_bits()] {
                            oe_fingerprint ^= u64::from(value);
                            oe_fingerprint = oe_fingerprint.wrapping_mul(0x100000001b3);
                        }
                    }
                }
                println!(
                    "oe_norm={} unit={:?} bin_size={} records={} finite={} sum={:.9} fingerprint={:016x}",
                    normalization,
                    zoom.unit,
                    zoom.bin_size,
                    oe_records,
                    oe_finite,
                    oe_sum,
                    oe_fingerprint
                );
            }
        }
    }
    Ok(())
}
