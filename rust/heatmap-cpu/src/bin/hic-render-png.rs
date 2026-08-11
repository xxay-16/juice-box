use std::path::PathBuf;

use anyhow::{Context, Result};
use heatmap_core::{IntensityTile, TileKey};
use hic_core::{HicFile, MatrixUnit};

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let hic_path = arguments
        .next()
        .map(PathBuf::from)
        .context("usage: hic-render-png <file.hic> <output.png> [matrix-key] [bin-size]")?;
    let output_path = arguments
        .next()
        .map(PathBuf::from)
        .context("usage: hic-render-png <file.hic> <output.png> [matrix-key] [bin-size]")?;
    let matrix_key = arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "1_1".to_owned());
    let requested_bin_size = arguments
        .next()
        .map(|value| value.to_string_lossy().parse::<u32>())
        .transpose()?
        .unwrap_or(500_000);

    let file = HicFile::open(&hic_path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    let zoom = matrix
        .zooms
        .iter()
        .filter(|zoom| zoom.unit == MatrixUnit::BasePairs)
        .min_by_key(|zoom| zoom.bin_size.abs_diff(requested_bin_size))
        .context("matrix contains no BP zoom")?;
    let records = file.read_all_blocks(zoom)?;
    let chromosome_x = file
        .header
        .chromosomes
        .get(matrix.chromosome_1 as usize)
        .context("matrix chromosome 1 is outside the header dictionary")?;
    let chromosome_y = file
        .header
        .chromosomes
        .get(matrix.chromosome_2 as usize)
        .context("matrix chromosome 2 is outside the header dictionary")?;
    let tile = IntensityTile::from_contacts(
        TileKey {
            dataset: 1,
            matrix_type: 0,
            normalization: 0,
            assembly_version: 0,
            resolution: zoom.bin_size,
            x: 0,
            y: 0,
        },
        chromosome_x.length.div_ceil(u64::from(zoom.bin_size)) as u32,
        chromosome_y.length.div_ceil(u64::from(zoom.bin_size)) as u32,
        1024,
        matrix.chromosome_1 == matrix.chromosome_2,
        records
            .iter()
            .map(|record| (record.bin_x, record.bin_y, record.counts)),
    );
    let color_max = tile.positive_percentile(0.995);
    heatmap_cpu::render_rgb(&tile, 0.0, color_max).save(&output_path)?;
    println!(
        "output={} matrix={} bin_size={} contacts={} color_max={}",
        output_path.display(),
        matrix_key,
        zoom.bin_size,
        records.len(),
        color_max
    );
    Ok(())
}
