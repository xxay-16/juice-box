use std::path::PathBuf;

use hic_core::HicFile;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: hic-normalization-info <file.hic>")?;
    let file = HicFile::open(&path)?;
    for key in file.read_normalization_index()?.keys() {
        let Some(vector) = file.read_normalization_vector(key)? else {
            continue;
        };
        let mut finite = 0_usize;
        let mut sum = 0.0_f64;
        let mut fingerprint = 0xcbf29ce484222325_u64;
        for value in &vector.values {
            if value.is_finite() {
                finite += 1;
                sum += value;
            }
            fingerprint ^= value.to_bits();
            fingerprint = fingerprint.wrapping_mul(0x100000001b3);
        }
        println!(
            "type={} chr={} unit={:?} resolution={} values={} finite={} sum={:.12} fingerprint={:016x}",
            key.normalization,
            key.chromosome,
            key.unit,
            key.resolution,
            vector.values.len(),
            finite,
            sum,
            fingerprint
        );
    }
    Ok(())
}
