use std::path::PathBuf;

use session_core::read_legacy_session;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: session-info <JuiceboxStatesForExport.xml>")?;
    for state in read_legacy_session(path)? {
        println!(
            "state={:?} maps={} controls={} chromosomes={}_{} unit={} bin_size={} origin=({:.6},{:.6}) scale={:.6} display={} normalization={} tracks={}",
            state.id,
            state.map_urls.len(),
            state.control_urls.len(),
            state.x_chromosome,
            state.y_chromosome,
            state.unit,
            state.bin_size,
            state.x_origin_bins,
            state.y_origin_bins,
            state.scale_factor,
            state.display_option,
            state.normalization,
            state.loaded_track_urls
        );
    }
    Ok(())
}
