use std::path::PathBuf;

use hic_core::HicFile;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: hic-info <file.hic>")?;
    let file = HicFile::open(&path)?;
    println!("path={}", file.path().display());
    println!("version={}", file.header.version);
    println!("genome={}", file.header.genome_id);
    println!("chromosomes={}", file.header.chromosomes.len());
    for chromosome in &file.header.chromosomes {
        println!(
            "chromosome index={} name={} length={}",
            chromosome.index, chromosome.name, chromosome.length
        );
    }
    println!("bp_resolutions={:?}", file.header.bp_resolutions);
    println!(
        "fragment_resolutions={:?}",
        file.header.fragment_resolutions
    );
    println!("matrix_entries={}", file.master_index.len());
    Ok(())
}
