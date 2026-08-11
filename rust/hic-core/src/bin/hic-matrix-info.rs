use std::path::PathBuf;

use hic_core::HicFile;

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
    for zoom in &matrix.zooms {
        let mut records: usize = 0;
        let mut stored_count_sum = 0.0_f64;
        let mut symmetric_count_sum = 0.0_f64;
        let mut fingerprint = 0xcbf29ce484222325_u64;
        for &block in zoom.blocks.keys() {
            for record in file.read_block(zoom, block)? {
                records += 1;
                let counts = f64::from(record.counts);
                stored_count_sum += counts;
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
            "unit={:?} bin_size={} block_bins={} block_columns={} blocks={} records={} stored_count_sum={:.9} symmetric_count_sum={:.9} fingerprint={:016x} declared_sum_counts={}",
            zoom.unit,
            zoom.bin_size,
            zoom.block_bin_count,
            zoom.block_column_count,
            zoom.blocks.len(),
            records,
            stored_count_sum,
            symmetric_count_sum,
            fingerprint,
            zoom.sum_counts
        );
    }
    Ok(())
}
