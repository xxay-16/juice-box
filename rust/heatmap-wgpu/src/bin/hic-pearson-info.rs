use std::{collections::HashSet, path::PathBuf};

use heatmap_core::{MAX_PEARSON_CELLS, compute_java_pearsons};
use hic_core::{ExpectedValueKey, HicFile, MatrixUnit};

const DEFAULT_BIN_SIZES: &str = "2500000,1000000";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("usage: hic-pearson-info <file.hic> [matrix-key] [comma-separated-bin-sizes]")?;
    let matrix_key = arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "1_1".to_owned());
    let requested = parse_bin_sizes(
        arguments
            .next()
            .map(|value| value.to_string_lossy().into_owned())
            .as_deref()
            .unwrap_or(DEFAULT_BIN_SIZES),
    )?;
    let file = HicFile::open(path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    if matrix.chromosome_1 != matrix.chromosome_2 {
        return Err("Pearson is only defined for intra-chromosomal matrices".into());
    }
    let chromosome = file
        .header
        .chromosomes
        .get(matrix.chromosome_1 as usize)
        .ok_or("matrix chromosome is outside the header dictionary")?;

    for normalization in ["NONE", "KR", "VC", "VC_SQRT"] {
        for zoom in matrix
            .zooms
            .iter()
            .filter(|zoom| zoom.unit == MatrixUnit::BasePairs && requested.contains(&zoom.bin_size))
        {
            let dimension_u64 = chromosome.length / u64::from(zoom.bin_size) + 1;
            let dimension = usize::try_from(dimension_u64)?;
            let cells = dimension
                .checked_mul(dimension)
                .ok_or("Pearson matrix dimensions overflow")?;
            if cells > MAX_PEARSON_CELLS {
                return Err(format!(
                    "Pearson matrix contains {cells} cells, exceeding the safety limit {MAX_PEARSON_CELLS}"
                )
                .into());
            }
            let expected = file
                .read_expected_value_vector(&ExpectedValueKey {
                    normalization: normalization.to_owned(),
                    unit: MatrixUnit::BasePairs,
                    resolution: zoom.bin_size,
                })?
                .ok_or("requested Pearson expected vector is unavailable")?;
            let mut oe = vec![0.0_f64; cells];
            let mut valid = vec![false; dimension];
            for &block_number in zoom.blocks.keys() {
                for record in file.read_block(zoom, block_number)? {
                    let Ok(x) = usize::try_from(record.bin_x) else {
                        continue;
                    };
                    let Ok(y) = usize::try_from(record.bin_y) else {
                        continue;
                    };
                    if x >= dimension || y >= dimension {
                        continue;
                    }
                    // MatrixZoomData.populateOEMatrixAndBitset iterates the raw
                    // NONE records even when the selected expected vector is
                    // normalized. Preserve that surprising Java behavior.
                    let counts = record.counts;
                    if counts.is_nan() {
                        continue;
                    }
                    let distance = u64::from((record.bin_x - record.bin_y).unsigned_abs());
                    let Some(expected_count) = expected.value_for(matrix.chromosome_1, distance)
                    else {
                        continue;
                    };
                    let value = f64::from(counts) / expected_count;
                    oe[x * dimension + y] = value;
                    oe[y * dimension + x] = value;
                    valid[x] = true;
                    valid[y] = true;
                }
            }
            let pearson = compute_java_pearsons(oe, dimension, &valid, 16)?;
            let mut finite = 0_u64;
            let mut nan = 0_u64;
            let mut infinite = 0_u64;
            let mut sum = 0.0_f64;
            let mut fingerprint = 0xcbf29ce484222325_u64;
            for &value in &pearson.values {
                if value.is_finite() {
                    finite += 1;
                    sum += f64::from(value);
                } else if value.is_nan() {
                    nan += 1;
                } else {
                    infinite += 1;
                }
                fingerprint ^= u64::from(value.to_bits());
                fingerprint = fingerprint.wrapping_mul(0x100000001b3);
            }
            let middle = dimension / 2;
            println!(
                "norm={} bin_size={} dim={} finite={} nan={} infinite={} sum={:.9} fingerprint={:016x} sample_0_0={:08x} sample_mid_mid={:08x} sample_0_mid={:08x}",
                normalization,
                zoom.bin_size,
                dimension,
                finite,
                nan,
                infinite,
                sum,
                fingerprint,
                pearson.values[0].to_bits(),
                pearson.values[middle * dimension + middle].to_bits(),
                pearson.values[middle].to_bits(),
            );
        }
    }
    Ok(())
}

fn parse_bin_sizes(text: &str) -> Result<HashSet<u32>, Box<dyn std::error::Error>> {
    let sizes = text
        .split(',')
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse())
        .collect::<Result<HashSet<_>, _>>()?;
    if sizes.is_empty() {
        return Err("at least one Pearson bin size is required".into());
    }
    Ok(sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requested_bin_sizes() {
        assert_eq!(
            parse_bin_sizes("2500000, 1000000").unwrap(),
            HashSet::from([2_500_000, 1_000_000])
        );
        assert!(parse_bin_sizes("").is_err());
    }
}
