use std::path::PathBuf;

use assembly_core::AssemblyDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: assembly-info <file.assembly>")?;
    let document = AssemblyDocument::open(&path)?;
    println!("path={}", path.display());
    println!("scaffolds={}", document.scaffolds.len());
    println!("superscaffolds={}", document.superscaffolds.len());
    println!("total_length={}", document.total_length());
    let placements = document.placements();
    let placed_length = placements.last().map_or(0, |placement| placement.end);
    println!("placed_scaffolds={}", placements.len());
    println!("placed_length={placed_length}");
    Ok(())
}
