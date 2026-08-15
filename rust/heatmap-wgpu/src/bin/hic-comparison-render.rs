use std::{
    fmt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use heatmap_core::{GenomeViewport, IntensityTile};
use hic_core::HicFile;

macro_rules! app_log {
    ($($argument:tt)*) => {
        let _ = format_args!($($argument)*);
    };
}

#[allow(dead_code)]
#[path = "../tile_engine.rs"]
mod tile_engine;

use tile_engine::{MatrixType, TileEngine};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
const SAMPLE_GRID: usize = 6;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let observed = PathBuf::from(arguments.next().context("missing observed .hic")?);
    let control = PathBuf::from(arguments.next().context("missing control .hic")?);
    let file = HicFile::open(&observed)?;
    let matrix = file.read_matrix("1_1")?;
    let chromosome = &file.header.chromosomes[matrix.chromosome_1 as usize];
    let engine = TileEngine::spawn(observed, "1_1".to_owned(), None, Some(control))?;
    let mut viewport = GenomeViewport::new(chromosome.length, 1.0);

    let modes = [
        MatrixType::Vs,
        MatrixType::Ratio,
        MatrixType::RatioV2,
        MatrixType::ObservedOverExpectedVs,
        MatrixType::PearsonVs,
        MatrixType::ObservedOverExpectedV2,
        MatrixType::ControlOverExpectedV2,
        MatrixType::ObservedOverExpectedVsV2,
        MatrixType::LogObserved,
        MatrixType::LogControl,
        MatrixType::LogObservedExpected,
        MatrixType::LogControlExpected,
        MatrixType::LogObservedExpectedVs,
    ]
    .into_iter()
    .chain(MatrixType::EXPECTED_PSEUDOCOUNT_MODES);
    for matrix_type in modes {
        engine.update_matrix_type(matrix_type);
        viewport.generation = viewport.generation.wrapping_add(1);
        engine.request(viewport);
        let tile = wait_complete(&engine, viewport.generation)?;
        print_cells(mode_name(matrix_type), &tile);
    }
    Ok(())
}

fn wait_complete(engine: &TileEngine, generation: u64) -> Result<IntensityTile> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(result) = engine.try_result() {
            let result = result.map_err(anyhow::Error::msg)?;
            if result.viewport.generation == generation && result.complete {
                return Ok(result.tile);
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    anyhow::bail!("timed out waiting for generation {generation}")
}

fn mode_name(matrix_type: MatrixType) -> &'static str {
    match matrix_type {
        MatrixType::Vs => "VS",
        MatrixType::Ratio => "RATIO",
        MatrixType::RatioV2 => "RATIOV2",
        MatrixType::ObservedOverExpectedVs => "OEVS",
        MatrixType::PearsonVs => "PEARSONVS",
        MatrixType::ObservedOverExpectedV2 => "OEV2",
        MatrixType::ControlOverExpectedV2 => "OECTRLV2",
        MatrixType::ObservedOverExpectedVsV2 => "OEVSV2",
        MatrixType::ObservedOverExpectedP1 => "OEP1",
        MatrixType::ObservedOverExpectedP1V2 => "OEP1V2",
        MatrixType::ControlOverExpectedP1 => "OECTRLP1",
        MatrixType::ControlOverExpectedP1V2 => "OECTRLP1V2",
        MatrixType::ObservedOverExpectedVsP1 => "OEVSP1",
        MatrixType::ObservedOverExpectedVsP1V2 => "OEVSP1V2",
        MatrixType::LogObserved => "LOG",
        MatrixType::LogControl => "LOGC",
        MatrixType::LogObservedExpected => "LOGEO",
        MatrixType::LogControlExpected => "LOGCEO",
        MatrixType::LogObservedExpectedVs => "LOGEOVS",
        _ => unreachable!(),
    }
}

fn print_cells(mode: &str, tile: &IntensityTile) {
    assert_eq!(tile.width, tile_engine::OUTPUT_SIZE);
    assert_eq!(tile.height, tile_engine::OUTPUT_SIZE);
    let output_size = tile_engine::OUTPUT_SIZE as usize;
    let mut cells = Vec::with_capacity(SAMPLE_GRID * SAMPLE_GRID);
    for bin_y in 0..SAMPLE_GRID {
        let y = bin_y * output_size / SAMPLE_GRID;
        for bin_x in 0..SAMPLE_GRID {
            let x = bin_x * output_size / SAMPLE_GRID;
            cells.push(tile.values[y * output_size + x].to_bits());
        }
    }
    let mut fingerprint = FNV_OFFSET;
    let bits = cells
        .iter()
        .enumerate()
        .map(|(index, &bits)| {
            fingerprint ^= index as u64;
            fingerprint = fingerprint.wrapping_mul(FNV_PRIME);
            fingerprint ^= u64::from(bits);
            fingerprint = fingerprint.wrapping_mul(FNV_PRIME);
            format!("{bits:08x}")
        })
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "render_mode={mode} cells={} bits={bits} fingerprint={fingerprint:016x}",
        cells.len()
    );
}

impl fmt::Display for MatrixType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}
