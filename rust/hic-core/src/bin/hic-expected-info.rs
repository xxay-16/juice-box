use std::path::PathBuf;

use hic_core::HicFile;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: hic-expected-info <file.hic>")?;
    let file = HicFile::open(&path)?;
    for vector in file.read_expected_value_vectors()?.values() {
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
        let factor_one = vector.chromosome_factors.get(&1).copied().unwrap_or(1.0);
        let value_zero = vector.value_for(1, 0).unwrap_or(f64::NAN);
        let value_past_end = vector
            .value_for(1, vector.values.len() as u64 + 1)
            .unwrap_or(f64::NAN);
        println!(
            "type={} unit={:?} resolution={} values={} finite={} sum={:.12} factors={} factor_chr1={:.12} value0={:.12} past_end={:.12} fingerprint={:016x}",
            vector.key.normalization,
            vector.key.unit,
            vector.key.resolution,
            vector.values.len(),
            finite,
            sum,
            vector.chromosome_factors.len(),
            factor_one,
            value_zero,
            value_past_end,
            fingerprint
        );
    }
    Ok(())
}
