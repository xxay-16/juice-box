use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use assembly_core::AssemblyCoordinateMap;
use heatmap_core::{GenomeViewport, IntensityTile, TileKey};
use hic_core::{
    ContactRecord, ExpectedValueKey, ExpectedValueVector, HicFile, Matrix, MatrixUnit, MatrixZoom,
    NormalizationKey,
};

const OUTPUT_SIZE: u32 = 1024;
const PREFETCH_BLOCK_RINGS: i32 = 2;
const BLOCK_CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const VIEW_OVERSCAN_FACTOR: f64 = 1.5;
const MAX_BLOCK_READ_CONCURRENCY: usize = 16;
// A dirty rectangle which covers most of the texture costs more to stage as
// padded rows than a direct full R32F upload.  More importantly, emitting a
// complete raster in that case lets the UI replace the entire newly panned
// view atomically rather than leaving its broad changed region waiting on a
// succession of large partial copies.
const FULL_TEXTURE_UPLOAD_AREA_NUMERATOR: u64 = 3;
const FULL_TEXTURE_UPLOAD_AREA_DENOMINATOR: u64 = 5;

#[derive(Debug, Clone)]
pub struct TileRequest {
    pub viewport: GenomeViewport,
    pub assembly_map: Option<Arc<AssemblyCoordinateMap>>,
    pub assembly_version: u64,
    pub normalization: Normalization,
    pub matrix_type: MatrixType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Normalization {
    None,
    Kr,
    Vc,
    VcSqrt,
}

impl Normalization {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Kr => "KR",
            Self::Vc => "VC",
            Self::VcSqrt => "VC_SQRT",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::None => Self::Kr,
            Self::Kr => Self::Vc,
            Self::Vc => Self::VcSqrt,
            Self::VcSqrt => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatrixType {
    Observed,
    Expected,
    ObservedOverExpected,
}

impl MatrixType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Observed => "Observed",
            Self::Expected => "Expected",
            Self::ObservedOverExpected => "O/E",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Observed => Self::Expected,
            Self::Expected => Self::ObservedOverExpected,
            Self::ObservedOverExpected => Self::Observed,
        }
    }
}

#[derive(Debug)]
pub struct TileResult {
    pub viewport: GenomeViewport,
    pub assembly_version: u64,
    pub normalization: Normalization,
    pub matrix_type: MatrixType,
    pub resolution: u32,
    pub tile: IntensityTile,
    /// Optional inclusive-exclusive pixel rectangle changed since the prior
    /// result for this generation. `None` means upload the complete texture.
    pub dirty_rect: Option<[u32; 4]>,
    pub upload_bytes: usize,
    pub color_max: f32,
    pub visible_blocks: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    /// Number of viewport blocks which have been rasterized into `tile`.
    /// Results are sent as this increases so a newly exposed region can be
    /// shown while the remaining compressed Blocks are still arriving.
    pub loaded_blocks: usize,
    pub total_blocks: usize,
    pub complete: bool,
    pub elapsed_ms: f64,
}

pub struct TileEngine {
    request: Arc<(Mutex<Option<TileRequest>>, Condvar)>,
    latest_generation: Arc<AtomicU64>,
    assembly_map: Arc<Mutex<Option<Arc<AssemblyCoordinateMap>>>>,
    assembly_version: Arc<AtomicU64>,
    normalization: Arc<Mutex<Normalization>>,
    matrix_type: Arc<Mutex<MatrixType>>,
    results: Receiver<Result<TileResult, String>>,
}

impl TileEngine {
    pub fn spawn(
        path: PathBuf,
        matrix_key: String,
        assembly_map: Option<AssemblyCoordinateMap>,
    ) -> Result<Self> {
        // Fail fast on metadata errors before starting the worker.
        let file = HicFile::open(&path)?;
        file.read_matrix(&matrix_key)?;

        let request = Arc::new((Mutex::new(None), Condvar::new()));
        let latest_generation = Arc::new(AtomicU64::new(0));
        let assembly_map = Arc::new(Mutex::new(assembly_map.map(Arc::new)));
        let assembly_version = Arc::new(AtomicU64::new(0));
        let normalization = Arc::new(Mutex::new(Normalization::None));
        let matrix_type = Arc::new(Mutex::new(MatrixType::Observed));
        let (sender, results) = mpsc::channel();
        let worker_request = Arc::clone(&request);
        let worker_generation = Arc::clone(&latest_generation);
        thread::Builder::new()
            .name("hic-tile-worker".to_owned())
            .spawn(move || {
                if let Err(error) =
                    worker_loop(path, matrix_key, worker_request, worker_generation, sender)
                {
                    app_log!("tile worker stopped: {error:#}");
                }
            })?;
        Ok(Self {
            request,
            latest_generation,
            assembly_map,
            assembly_version,
            normalization,
            matrix_type,
            results,
        })
    }

    pub fn request(&self, viewport: GenomeViewport) {
        self.latest_generation
            .store(viewport.generation, Ordering::Release);
        let (slot, changed) = &*self.request;
        let assembly_map = self
            .assembly_map
            .lock()
            .expect("assembly map mutex poisoned")
            .clone();
        *slot.lock().expect("tile request mutex poisoned") = Some(TileRequest {
            viewport,
            assembly_map,
            assembly_version: self.assembly_version.load(Ordering::Acquire),
            normalization: *self
                .normalization
                .lock()
                .expect("normalization mutex poisoned"),
            matrix_type: *self.matrix_type.lock().expect("matrix type mutex poisoned"),
        });
        changed.notify_one();
    }

    pub fn update_assembly(&self, map: AssemblyCoordinateMap, version: u64) {
        *self
            .assembly_map
            .lock()
            .expect("assembly map mutex poisoned") = Some(Arc::new(map));
        self.assembly_version.store(version, Ordering::Release);
    }

    pub fn update_normalization(&self, normalization: Normalization) {
        *self
            .normalization
            .lock()
            .expect("normalization mutex poisoned") = normalization;
    }

    pub fn update_matrix_type(&self, matrix_type: MatrixType) {
        *self.matrix_type.lock().expect("matrix type mutex poisoned") = matrix_type;
    }

    pub fn try_result(&self) -> Option<Result<TileResult, String>> {
        self.results.try_recv().ok()
    }
}

fn worker_loop(
    path: PathBuf,
    matrix_key: String,
    request: Arc<(Mutex<Option<TileRequest>>, Condvar)>,
    latest_generation: Arc<AtomicU64>,
    sender: Sender<Result<TileResult, String>>,
) -> Result<()> {
    let file = HicFile::open(&path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    let symmetric = matrix.chromosome_1 == matrix.chromosome_2;
    let mut cache = BlockCache::new(BLOCK_CACHE_BUDGET_BYTES);
    let mut normalization_cache: HashMap<(Normalization, u32, u32), Arc<Vec<f64>>> = HashMap::new();
    let mut expected_cache: HashMap<(Normalization, u32), Arc<ExpectedValueVector>> =
        HashMap::new();

    loop {
        let next = {
            let (slot, changed) = &*request;
            let mut pending = slot.lock().expect("tile request mutex poisoned");
            while pending.is_none() {
                pending = changed
                    .wait(pending)
                    .expect("tile request mutex poisoned while waiting");
            }
            pending.take().expect("pending request disappeared")
        };

        let request_assembly_map = next.assembly_map.clone();
        match build_tile_streaming(
            &file,
            &matrix,
            symmetric,
            next,
            &latest_generation,
            &mut cache,
            &mut normalization_cache,
            &mut expected_cache,
            &sender,
        ) {
            Ok(Some(completed_viewport)) => {
                prefetch_viewport(
                    &file,
                    &matrix,
                    symmetric,
                    request_assembly_map.as_deref(),
                    completed_viewport,
                    &latest_generation,
                    &request,
                    &mut cache,
                )?;
            }
            Ok(None) => {} // Superseded by a newer generation.
            Err(error) => {
                if sender.send(Err(format!("{error:#}"))).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "tile construction shares immutable reader state, cache state, cancellation, and progressive result output"
)]
fn build_tile_streaming(
    file: &HicFile,
    matrix: &Matrix,
    symmetric: bool,
    request: TileRequest,
    latest_generation: &AtomicU64,
    cache: &mut BlockCache,
    normalization_cache: &mut HashMap<(Normalization, u32, u32), Arc<Vec<f64>>>,
    expected_cache: &mut HashMap<(Normalization, u32), Arc<ExpectedValueVector>>,
    sender: &Sender<Result<TileResult, String>>,
) -> Result<Option<GenomeViewport>> {
    let started = Instant::now();
    let generation = request.viewport.generation;
    let assembly_map = request.assembly_map.as_deref();
    let zoom = choose_zoom(matrix, request.viewport.span_bp / f64::from(OUTPUT_SIZE))
        .context("matrix has no base-pair zoom")?;
    // Expected uses the footer vector directly; no per-bin normalization
    // vector is needed to build its dense diagonal field.  Observed and O/E
    // retain Java's normalized-contact calculation before rasterization.
    let normalization_vector = (request.matrix_type != MatrixType::Expected)
        .then(|| {
            normalization_vector(
                file,
                matrix.chromosome_1,
                request.normalization,
                normalization_cache,
                request.viewport.span_bp / f64::from(OUTPUT_SIZE),
                matrix,
            )
        })
        .transpose()?
        .flatten();
    let expected_vector = (request.matrix_type != MatrixType::Observed)
        .then(|| expected_vector(file, request.normalization, zoom.bin_size, expected_cache))
        .transpose()?;
    let rendered_viewport = rendered_viewport_for(request.viewport);
    let bin_bounds = viewport_bin_bounds(rendered_viewport, zoom.bin_size);
    if request.matrix_type == MatrixType::Expected {
        let expected = expected_vector
            .as_deref()
            .context("expected values are unavailable for the selected normalization/resolution")?;
        let mut raw_values = vec![0.0_f32; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
        accumulate_expected(&mut raw_values, bin_bounds, matrix.chromosome_1, expected);
        send_stream_result(
            sender,
            &request,
            rendered_viewport,
            zoom.bin_size,
            &raw_values,
            0,
            0,
            0,
            0,
            0,
            true,
            None,
            started,
        )?;
        return Ok(Some(rendered_viewport));
    }
    let visible_blocks =
        block_numbers_for_viewport(zoom, rendered_viewport, symmetric, assembly_map);
    // Keep the untransformed accumulator so individual Blocks can be applied
    // and published without double-log-transforming pixels shared by several
    // source Blocks.  This turns one all-or-nothing viewport texture into a
    // stream of progressively filled regions.
    let mut raw_values = vec![0.0_f32; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
    let mut cache_hits = 0;
    let mut missing_blocks = Vec::new();

    for &block_number in &visible_blocks {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
        if let Some(block) = cache.get(zoom, block_number) {
            cache_hits += 1;
            accumulate_block(
                &mut raw_values,
                bin_bounds,
                symmetric,
                zoom,
                assembly_map,
                normalization_vector.as_deref().map(Vec::as_slice),
                expected_vector.as_deref(),
                request.matrix_type,
                matrix.chromosome_1,
                &block,
            );
        } else {
            missing_blocks.push(block_number);
        }
    }
    let cache_misses = missing_blocks.len();
    let total_blocks = visible_blocks.len();
    let mut loaded_blocks = cache_hits;
    let mut published_generation_texture = false;

    // If cache hits already provide part of the new area, display them before
    // doing any I/O.  Otherwise leave the previous texture visible until the
    // first real Block arrives, avoiding a black full-screen flash.
    if loaded_blocks > 0 && cache_misses > 0 {
        send_stream_result(
            sender,
            &request,
            rendered_viewport,
            zoom.bin_size,
            &raw_values,
            visible_blocks.len(),
            cache_hits,
            cache_misses,
            loaded_blocks,
            total_blocks,
            false,
            None,
            started,
        )?;
        published_generation_texture = true;
    }

    // Every read worker sends each completed Block back as soon as it is
    // decompressed. The owner thread applies that Block to the accumulator
    // and publishes a texture update immediately, preserving the configured
    // 16-way reader concurrency without delaying visual fill until all
    // workers have joined.
    let mut became_stale = false;
    stream_blocks_parallel(file, zoom, &missing_blocks, |block_number, records| {
        if is_stale(latest_generation, generation) {
            became_stale = true;
            return Ok(());
        }
        let block = cache.insert(zoom, block_number, records);
        let dirty_rect = accumulate_block(
            &mut raw_values,
            bin_bounds,
            symmetric,
            zoom,
            assembly_map,
            normalization_vector.as_deref().map(Vec::as_slice),
            expected_vector.as_deref(),
            request.matrix_type,
            matrix.chromosome_1,
            &block,
        );
        loaded_blocks += 1;
        let upload_rect = published_generation_texture
            .then_some(dirty_rect.unwrap_or([0, 0, 0, 0]))
            .filter(|rect| !dirty_rect_requires_full_upload(*rect));
        send_stream_result(
            sender,
            &request,
            rendered_viewport,
            zoom.bin_size,
            &raw_values,
            visible_blocks.len(),
            cache_hits,
            cache_misses,
            loaded_blocks,
            total_blocks,
            loaded_blocks == total_blocks,
            upload_rect,
            started,
        )?;
        published_generation_texture = true;
        Ok(())
    })?;
    if became_stale || is_stale(latest_generation, generation) {
        return Ok(None);
    }
    if is_stale(latest_generation, generation) {
        return Ok(None);
    }

    // A viewport consisting entirely of cached Blocks has no I/O loop above,
    // so publish its final state here.
    if cache_misses == 0 {
        send_stream_result(
            sender,
            &request,
            rendered_viewport,
            zoom.bin_size,
            &raw_values,
            visible_blocks.len(),
            cache_hits,
            cache_misses,
            loaded_blocks,
            total_blocks,
            true,
            None,
            started,
        )?;
    }
    Ok(Some(rendered_viewport))
}

#[allow(
    clippy::too_many_arguments,
    reason = "a stream update carries one immutable viewport snapshot and its progress metadata"
)]
fn send_stream_result(
    sender: &Sender<Result<TileResult, String>>,
    request: &TileRequest,
    viewport: GenomeViewport,
    resolution: u32,
    raw_values: &[f32],
    visible_blocks: usize,
    cache_hits: usize,
    cache_misses: usize,
    loaded_blocks: usize,
    total_blocks: usize,
    complete: bool,
    dirty_rect: Option<[u32; 4]>,
    started: Instant,
) -> Result<()> {
    // Keep the scientific matrix values in the R32F texture. MatrixType
    // transforms (normalization and O/E) belong above, while display-only
    // scaling belongs in the shader/color range. Applying ln(1+x) here made
    // Observed, Expected, and O/E numerically different from Java.
    let color_max = if complete {
        let full = IntensityTile::new(
            TileKey {
                dataset: 1,
                matrix_type: matrix_type_id(request.matrix_type),
                normalization: normalization_id(request.normalization),
                assembly_version: request.assembly_version,
                resolution,
                x: 0,
                y: 0,
            },
            OUTPUT_SIZE,
            OUTPUT_SIZE,
            raw_values.to_vec(),
        );
        full.positive_percentile(0.995)
    } else {
        1.0
    };
    let (width, height, values) = dirty_rect.map_or_else(
        || (OUTPUT_SIZE, OUTPUT_SIZE, raw_values.to_vec()),
        |[x, y, width, height]| {
            let mut values = Vec::with_capacity(width as usize * height as usize);
            for row in y..y + height {
                let start = (row * OUTPUT_SIZE + x) as usize;
                values.extend_from_slice(&raw_values[start..start + width as usize]);
            }
            (width, height, values)
        },
    );
    let upload_bytes = values.len() * std::mem::size_of::<f32>();
    let tile = IntensityTile::new(
        TileKey {
            dataset: 1,
            matrix_type: matrix_type_id(request.matrix_type),
            normalization: normalization_id(request.normalization),
            assembly_version: request.assembly_version,
            resolution,
            x: 0,
            y: 0,
        },
        width,
        height,
        values,
    );
    sender
        .send(Ok(TileResult {
            viewport,
            assembly_version: request.assembly_version,
            normalization: request.normalization,
            matrix_type: request.matrix_type,
            resolution,
            color_max,
            tile,
            dirty_rect,
            upload_bytes,
            visible_blocks,
            cache_hits,
            cache_misses,
            loaded_blocks,
            total_blocks,
            complete,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        }))
        .map_err(|_| anyhow::anyhow!("heatmap UI dropped the tile-result receiver"))
}

fn dirty_rect_requires_full_upload([_x, _y, width, height]: [u32; 4]) -> bool {
    u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(FULL_TEXTURE_UPLOAD_AREA_DENOMINATOR)
        >= u64::from(OUTPUT_SIZE)
            .saturating_mul(u64::from(OUTPUT_SIZE))
            .saturating_mul(FULL_TEXTURE_UPLOAD_AREA_NUMERATOR)
}

#[allow(
    clippy::too_many_arguments,
    reason = "contact transformation keeps each scientific coordinate and immutable rendering context explicit"
)]
fn accumulate_block(
    values: &mut [f32],
    bin_bounds: [i32; 4],
    symmetric: bool,
    zoom: &MatrixZoom,
    assembly_map: Option<&AssemblyCoordinateMap>,
    normalization_vector: Option<&[f64]>,
    expected_vector: Option<&ExpectedValueVector>,
    matrix_type: MatrixType,
    chromosome: u32,
    records: &[ContactRecord],
) -> Option<[u32; 4]> {
    IntensityTile::accumulate_contact_window_dirty(
        values,
        bin_bounds,
        OUTPUT_SIZE,
        symmetric,
        records.iter().filter_map(|record| {
            let normalized = normalize_contact(record, normalization_vector)?;
            let (bin_x, bin_y, counts) = map_contact(&normalized, zoom.bin_size, assembly_map)?;
            let counts = match matrix_type {
                MatrixType::Observed => counts,
                MatrixType::ObservedOverExpected => observed_over_expected(
                    ContactRecord {
                        bin_x,
                        bin_y,
                        counts,
                    },
                    expected_vector?,
                    chromosome,
                    bin_x,
                    bin_y,
                )?,
                MatrixType::Expected => unreachable!("expected tiles do not read sparse contacts"),
            };
            Some((bin_x, bin_y, counts))
        }),
    )
}

fn accumulate_expected(
    values: &mut [f32],
    bin_bounds: [i32; 4],
    chromosome: u32,
    expected: &ExpectedValueVector,
) {
    let output_size = OUTPUT_SIZE as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as f64;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as f64;
    for y in 0..output_size {
        let bin_y = bin_bounds[1] as f64 + (y as f64 + 0.5) * height_bins / OUTPUT_SIZE as f64;
        for x in 0..output_size {
            let bin_x = bin_bounds[0] as f64 + (x as f64 + 0.5) * width_bins / OUTPUT_SIZE as f64;
            let distance = (bin_x.round() as i64 - bin_y.round() as i64).unsigned_abs();
            if let Some(value) = expected.value_for(chromosome, distance)
                && !value.is_nan()
            {
                values[y * output_size + x] = value as f32;
            }
        }
    }
}

fn normalization_id(normalization: Normalization) -> u32 {
    match normalization {
        Normalization::None => 0,
        Normalization::Kr => 1,
        Normalization::Vc => 2,
        Normalization::VcSqrt => 3,
    }
}

fn matrix_type_id(matrix_type: MatrixType) -> u32 {
    match matrix_type {
        MatrixType::Observed => 0,
        MatrixType::Expected => 1,
        MatrixType::ObservedOverExpected => 2,
    }
}

fn observed_over_expected(
    record: ContactRecord,
    expected: &ExpectedValueVector,
    chromosome: u32,
    mapped_bin_x: i32,
    mapped_bin_y: i32,
) -> Option<f32> {
    let distance = u64::from((mapped_bin_x - mapped_bin_y).unsigned_abs());
    let score = f64::from(record.counts) / expected.value_for(chromosome, distance)?;
    score.is_finite().then_some(score as f32)
}

fn expected_vector(
    file: &HicFile,
    normalization: Normalization,
    resolution: u32,
    cache: &mut HashMap<(Normalization, u32), Arc<ExpectedValueVector>>,
) -> Result<Arc<ExpectedValueVector>> {
    let cache_key = (normalization, resolution);
    if let Some(vector) = cache.get(&cache_key) {
        return Ok(Arc::clone(vector));
    }
    let key = ExpectedValueKey {
        normalization: normalization.label().to_owned(),
        unit: MatrixUnit::BasePairs,
        resolution,
    };
    let vector = Arc::new(file.read_expected_value_vector(&key)?.with_context(|| {
        format!(
            "expected values for {} are unavailable at {} bp",
            key.normalization, key.resolution
        )
    })?);
    cache.insert(cache_key, Arc::clone(&vector));
    Ok(vector)
}

fn normalization_vector(
    file: &HicFile,
    chromosome: u32,
    normalization: Normalization,
    cache: &mut HashMap<(Normalization, u32, u32), Arc<Vec<f64>>>,
    target_bp_per_pixel: f64,
    matrix: &Matrix,
) -> Result<Option<Arc<Vec<f64>>>> {
    if normalization == Normalization::None {
        return Ok(None);
    }
    let zoom = choose_zoom(matrix, target_bp_per_pixel).context("matrix has no base-pair zoom")?;
    let cache_key = (normalization, chromosome, zoom.bin_size);
    if let Some(vector) = cache.get(&cache_key) {
        return Ok(Some(Arc::clone(vector)));
    }
    let key = NormalizationKey {
        normalization: normalization.label().to_owned(),
        chromosome,
        unit: MatrixUnit::BasePairs,
        resolution: zoom.bin_size,
    };
    let vector = file.read_normalization_vector(&key)?.with_context(|| {
        format!(
            "normalization {} is unavailable at {} bp",
            key.normalization, key.resolution
        )
    })?;
    let vector = Arc::new(vector.values);
    cache.insert(cache_key, Arc::clone(&vector));
    Ok(Some(vector))
}

fn normalize_contact(
    record: &ContactRecord,
    normalization: Option<&[f64]>,
) -> Option<ContactRecord> {
    let Some(vector) = normalization else {
        return Some(*record);
    };
    let x = usize::try_from(record.bin_x).ok()?;
    let y = usize::try_from(record.bin_y).ok()?;
    let denominator = *vector.get(x)? * *vector.get(y)?;
    let counts = (f64::from(record.counts) / denominator) as f32;
    (!counts.is_nan()).then_some(ContactRecord {
        bin_x: record.bin_x,
        bin_y: record.bin_y,
        counts,
    })
}

fn map_contact(
    record: &ContactRecord,
    bin_size: u32,
    assembly_map: Option<&AssemblyCoordinateMap>,
) -> Option<(i32, i32, f32)> {
    let (bin_x, bin_y) = if let Some(map) = assembly_map {
        (
            map.source_bin_to_assembly_bin(record.bin_x, bin_size)?,
            map.source_bin_to_assembly_bin(record.bin_y, bin_size)?,
        )
    } else {
        (record.bin_x, record.bin_y)
    };
    Some((bin_x, bin_y, record.counts))
}

fn stream_blocks_parallel<F>(
    file: &HicFile,
    zoom: &MatrixZoom,
    block_numbers: &[i32],
    mut consume: F,
) -> Result<()>
where
    F: FnMut(i32, Vec<ContactRecord>) -> Result<()>,
{
    if block_numbers.is_empty() {
        return Ok(());
    }
    let worker_count = MAX_BLOCK_READ_CONCURRENCY.min(block_numbers.len());
    let (sender, receiver) = mpsc::channel::<Result<(i32, Vec<ContactRecord>), String>>();
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for worker_index in 0..worker_count {
            let sender = sender.clone();
            handles.push(scope.spawn(move || {
                for &block_number in block_numbers
                    .iter()
                    .skip(worker_index)
                    .step_by(worker_count)
                {
                    let result = file
                        .read_block(zoom, block_number)
                        .map(|records| (block_number, records))
                        .map_err(|error| format!("{error:#}"));
                    if sender.send(result).is_err() {
                        break;
                    }
                }
            }));
        }
        drop(sender);
        for result in receiver {
            let (block_number, records) = result.map_err(anyhow::Error::msg)?;
            consume(block_number, records)?;
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("block reader thread panicked"))?;
        }
        Ok(())
    })
}

pub(crate) fn rendered_viewport_for(viewport: GenomeViewport) -> GenomeViewport {
    let span_bp = (viewport.span_bp * VIEW_OVERSCAN_FACTOR).min(viewport.genome_length_bp);
    let half = span_bp * 0.5;
    let maximum = (viewport.genome_length_bp - half).max(half);
    GenomeViewport {
        center_bp: [
            viewport.center_bp[0].clamp(half, maximum),
            viewport.center_bp[1].clamp(half, maximum),
        ],
        span_bp,
        ..viewport
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "prefetch needs the same immutable reader/matrix inputs as visible rendering plus its cancellation state"
)]
fn prefetch_viewport(
    file: &HicFile,
    matrix: &Matrix,
    symmetric: bool,
    assembly_map: Option<&AssemblyCoordinateMap>,
    viewport: GenomeViewport,
    latest_generation: &AtomicU64,
    request: &Arc<(Mutex<Option<TileRequest>>, Condvar)>,
    cache: &mut BlockCache,
) -> Result<()> {
    let generation = viewport.generation;
    let Some(zoom) = choose_zoom(matrix, viewport.span_bp / f64::from(OUTPUT_SIZE)) else {
        return Ok(());
    };
    let visible_bounds = viewport_bin_bounds(viewport, zoom.bin_size);
    let visible: HashSet<i32> = block_numbers_for_viewport(zoom, viewport, symmetric, assembly_map)
        .into_iter()
        .collect();
    let prefetch_bounds = expand_bounds_by_block_rings(visible_bounds, zoom, PREFETCH_BLOCK_RINGS);
    let prefetch_viewport = viewport_from_bin_bounds(viewport, prefetch_bounds, zoom.bin_size);
    for block_number in block_numbers_for_viewport(zoom, prefetch_viewport, symmetric, assembly_map)
    {
        if visible.contains(&block_number) {
            continue;
        }
        // Prefetch is deliberately opportunistic.  It must never make a newly
        // visible region wait behind the two-ring cache warm-up: the worker
        // owns the cache, so it used to finish every synchronous prefetch read
        // before it could dequeue the next viewport request.
        if is_stale(latest_generation, generation) || has_pending_request(request) {
            app_log!(
                "prefetch interrupted: generation={} next visible viewport is pending",
                generation
            );
            break;
        }
        let _ = cache.get_or_read(file, zoom, block_number)?;
    }
    Ok(())
}

fn has_pending_request(request: &Arc<(Mutex<Option<TileRequest>>, Condvar)>) -> bool {
    request
        .0
        .lock()
        .map(|pending| pending.is_some())
        .unwrap_or(true)
}

fn block_numbers_for_viewport(
    zoom: &MatrixZoom,
    viewport: GenomeViewport,
    symmetric: bool,
    assembly_map: Option<&AssemblyCoordinateMap>,
) -> Vec<i32> {
    let Some(map) = assembly_map else {
        return zoom
            .block_numbers_for_bounds(viewport_bin_bounds(viewport, zoom.bin_size), symmetric);
    };
    let bounds = viewport.bounds_bp();
    let x_segments = map.segments_for_assembly_range(
        bounds[0].floor().max(0.0) as u64,
        bounds[2].ceil().max(0.0) as u64,
    );
    let y_segments = map.segments_for_assembly_range(
        bounds[1].floor().max(0.0) as u64,
        bounds[3].ceil().max(0.0) as u64,
    );
    let block_bins = i64::from(zoom.block_bin_count.max(1));
    let columns = i64::from(zoom.block_column_count.max(1));
    let bin_size = u64::from(zoom.bin_size.max(1));
    let x_blocks = source_block_indices_for_segments(&x_segments, bin_size, block_bins);
    let y_blocks = source_block_indices_for_segments(&y_segments, bin_size, block_bins);
    let mut selected = std::collections::BTreeSet::new();
    let mut add_products = |rows: &HashSet<i64>, columns_set: &HashSet<i64>| {
        for &row in rows {
            for &column in columns_set {
                if let Ok(number) =
                    i32::try_from(row.saturating_mul(columns).saturating_add(column))
                    && zoom.blocks.contains_key(&number)
                {
                    selected.insert(number);
                }
            }
        }
    };
    add_products(&y_blocks, &x_blocks);
    if symmetric {
        add_products(&x_blocks, &y_blocks);
    }
    selected.into_iter().collect()
}

/// Source blocks needed to remap an assembly-axis interval.  Scaffold ends
/// usually do not align to a `.hic` bin, and `.hic` blocks group many such
/// bins.  Juicebox deliberately includes the block immediately beyond the
/// strict source interval (`(lastBin + 1) / blockBinCount`); without that guard
/// a reordered scaffold boundary can leave a blank stripe because contacts
/// from that neighbouring source block never reach the assembly rasterizer.
fn source_block_indices_for_segments(
    segments: &[assembly_core::AssemblyMappingSegment],
    bin_size: u64,
    block_bins: i64,
) -> HashSet<i64> {
    let mut blocks = HashSet::new();
    for segment in segments {
        if segment.source_start >= segment.source_end {
            continue;
        }
        let first_bin = (segment.source_start / bin_size) as i64;
        // Match MatrixZoomData#getAssemblyAxisBlockIndices: the Java assembly
        // renderer treats the segment end as a guarded boundary rather than
        // stopping at `end - 1`, then advances one bin before choosing the last
        // source block. Existing-block filtering below makes this safe at the
        // chromosome end.
        let last_bin = (segment.source_end / bin_size) as i64;
        let first_block = first_bin / block_bins;
        let guarded_last_block = last_bin.saturating_add(1) / block_bins;
        for block in first_block..=guarded_last_block {
            blocks.insert(block);
        }
    }
    blocks
}

fn viewport_from_bin_bounds(
    template: GenomeViewport,
    bounds: [i32; 4],
    bin_size: u32,
) -> GenomeViewport {
    let bin_size = f64::from(bin_size);
    let x_start = f64::from(bounds[0]) * bin_size;
    let y_start = f64::from(bounds[1]) * bin_size;
    let x_end = f64::from(bounds[2].saturating_add(1)) * bin_size;
    let y_end = f64::from(bounds[3].saturating_add(1)) * bin_size;
    let span = (x_end - x_start).max(y_end - y_start);
    GenomeViewport {
        center_bp: [(x_start + x_end) * 0.5, (y_start + y_end) * 0.5],
        span_bp: span,
        ..template
    }
}

fn choose_zoom(matrix: &Matrix, target_bp_per_pixel: f64) -> Option<&MatrixZoom> {
    matrix
        .zooms
        .iter()
        .filter(|zoom| zoom.unit == MatrixUnit::BasePairs)
        .min_by(|left, right| {
            let left_distance = (f64::from(left.bin_size) / target_bp_per_pixel.max(1.0))
                .ln()
                .abs();
            let right_distance = (f64::from(right.bin_size) / target_bp_per_pixel.max(1.0))
                .ln()
                .abs();
            left_distance.total_cmp(&right_distance)
        })
}

fn viewport_bin_bounds(viewport: GenomeViewport, bin_size: u32) -> [i32; 4] {
    let bounds = viewport.bounds_bp();
    let maximum_bin = (viewport.genome_length_bp / f64::from(bin_size)).ceil() as i32 - 1;
    [
        (bounds[0] / f64::from(bin_size)).floor().max(0.0) as i32,
        (bounds[1] / f64::from(bin_size)).floor().max(0.0) as i32,
        (bounds[2] / f64::from(bin_size)).ceil().max(1.0) as i32 - 1,
        (bounds[3] / f64::from(bin_size)).ceil().max(1.0) as i32 - 1,
    ]
    .map(|value| value.clamp(0, maximum_bin.max(0)))
}

fn expand_bounds_by_block_rings(bounds: [i32; 4], zoom: &MatrixZoom, rings: i32) -> [i32; 4] {
    let padding = zoom.block_bin_count.max(1) as i32 * rings.max(0);
    [
        bounds[0].saturating_sub(padding),
        bounds[1].saturating_sub(padding),
        bounds[2].saturating_add(padding),
        bounds[3].saturating_add(padding),
    ]
}

fn is_stale(latest_generation: &AtomicU64, generation: u64) -> bool {
    latest_generation.load(Ordering::Acquire) != generation
}

struct CachedBlock {
    records: Arc<Vec<ContactRecord>>,
    bytes: usize,
    last_used: u64,
}

struct BlockCache {
    entries: HashMap<(u32, i32), CachedBlock>,
    budget_bytes: usize,
    used_bytes: usize,
    clock: u64,
}

impl BlockCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes,
            used_bytes: 0,
            clock: 0,
        }
    }

    fn get_or_read(
        &mut self,
        file: &HicFile,
        zoom: &MatrixZoom,
        block_number: i32,
    ) -> Result<(Arc<Vec<ContactRecord>>, bool)> {
        if let Some(records) = self.get(zoom, block_number) {
            return Ok((records, true));
        }
        let records = self.insert(zoom, block_number, file.read_block(zoom, block_number)?);
        Ok((records, false))
    }

    fn get(&mut self, zoom: &MatrixZoom, block_number: i32) -> Option<Arc<Vec<ContactRecord>>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&(zoom.bin_size, block_number))?;
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.records))
    }

    fn insert(
        &mut self,
        zoom: &MatrixZoom,
        block_number: i32,
        records: Vec<ContactRecord>,
    ) -> Arc<Vec<ContactRecord>> {
        self.clock = self.clock.wrapping_add(1);
        let key = (zoom.bin_size, block_number);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.clock;
            return Arc::clone(&entry.records);
        }
        let records = Arc::new(records);
        let bytes = records
            .len()
            .saturating_mul(std::mem::size_of::<ContactRecord>());
        self.evict_for(bytes);
        self.used_bytes = self.used_bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            CachedBlock {
                records: Arc::clone(&records),
                bytes,
                last_used: self.clock,
            },
        );
        records
    }

    fn evict_for(&mut self, incoming_bytes: usize) {
        while !self.entries.is_empty()
            && self.used_bytes.saturating_add(incoming_bytes) > self.budget_bytes
        {
            let Some((&oldest_key, _)) =
                self.entries.iter().min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest_key) {
                self.used_bytes = self.used_bytes.saturating_sub(removed.bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_core::{AssemblyDocument, AssemblyEditor};
    use hic_core::IndexEntry;
    use std::collections::BTreeMap;

    #[test]
    fn overscan_expands_and_clamps_to_genome_edges() {
        let mut viewport = GenomeViewport::new(1_000, 0.4);
        viewport.center_bp = [200.0, 800.0];
        viewport.generation = 7;
        let overscan = rendered_viewport_for(viewport);
        assert_eq!(overscan.span_bp, 600.0);
        assert_eq!(overscan.center_bp, [300.0, 700.0]);
        assert_eq!(overscan.generation, 7);
        assert_eq!(overscan.bounds_bp(), [0.0, 400.0, 600.0, 1000.0]);
    }

    #[test]
    fn broad_streamed_dirty_regions_request_a_full_texture_upload() {
        assert!(dirty_rect_requires_full_upload([0, 0, 1024, 1024]));
        assert!(dirty_rect_requires_full_upload([52, 52, 936, 936]));
        assert!(!dirty_rect_requires_full_upload([100, 100, 512, 512]));
        assert!(!dirty_rect_requires_full_upload([666, 715, 4, 4]));
    }

    #[test]
    fn assembly_edits_change_block_selection_and_rasterized_contacts_end_to_end() {
        let document = AssemblyDocument::parse(">a 1 10\n>b 2 10\n-2 1\n").unwrap();
        let mut editor = AssemblyEditor::new(document);
        let zoom = MatrixZoom {
            unit: MatrixUnit::BasePairs,
            bin_size: 5,
            sum_counts: 0.0,
            occupied_cell_count: 0.0,
            standard_deviation: 0.0,
            percentile_95: 0.0,
            block_bin_count: 2,
            block_column_count: 2,
            blocks: (0..4)
                .map(|number| {
                    (
                        number,
                        IndexEntry {
                            position: 0,
                            size: 1,
                        },
                    )
                })
                .collect(),
        };
        let viewport = GenomeViewport {
            center_bp: [5.0, 5.0],
            span_bp: 10.0,
            genome_length_bp: 20.0,
            minimum_span_bp: 1.0,
            generation: 0,
        };
        let contact = ContactRecord {
            bin_x: 2,
            bin_y: 3,
            counts: 3.0,
        };

        let reversed_map = editor.document().coordinate_map().unwrap();
        assert_eq!(
            block_numbers_for_viewport(&zoom, viewport, true, Some(&reversed_map)),
            vec![3]
        );
        let reversed_contact = map_contact(&contact, zoom.bin_size, Some(&reversed_map)).unwrap();
        assert_eq!(reversed_contact, (1, 0, 3.0));
        let reversed_tile = IntensityTile::from_contact_window(
            TileKey {
                dataset: 1,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 5,
                x: 0,
                y: 0,
            },
            [0, 0, 1, 1],
            2,
            true,
            [reversed_contact],
        );
        assert!(reversed_tile.values[1] > 0.0);
        assert!(reversed_tile.values[2] > 0.0);

        editor.move_scaffold(0, 1, 0, 0).unwrap();
        let reordered_map = editor.document().coordinate_map().unwrap();
        assert_eq!(
            block_numbers_for_viewport(&zoom, viewport, true, Some(&reordered_map)),
            vec![0, 1, 2, 3]
        );
        assert!(map_contact(&contact, zoom.bin_size, Some(&reordered_map)).is_some());

        assert!(editor.undo());
        let undo_map = editor.document().coordinate_map().unwrap();
        assert_eq!(
            block_numbers_for_viewport(&zoom, viewport, true, Some(&undo_map)),
            vec![3]
        );
        assert_eq!(map_contact(&contact, 5, Some(&undo_map)), Some((1, 0, 3.0)));

        assert!(editor.redo());
        let redo_map = editor.document().coordinate_map().unwrap();
        assert_eq!(
            block_numbers_for_viewport(&zoom, viewport, true, Some(&redo_map)),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn assembly_segment_loads_java_boundary_guard_block() {
        let segment = assembly_core::AssemblyMappingSegment {
            scaffold_id: 1,
            source_start: 0,
            source_end: 19,
            assembly_start: 0,
            assembly_end: 19,
            reversed: false,
        };

        // With 10 bp bins and two bins per source block, the strict interval
        // only intersects source block 0. Java's assembly renderer also loads
        // block 1 so contacts straddling the off-grid scaffold end can still be
        // remapped into the newly adjacent assembly region.
        assert_eq!(
            source_block_indices_for_segments(&[segment], 10, 2),
            HashSet::from([0, 1])
        );
    }

    #[test]
    fn normalized_contacts_match_java_division_and_nan_filtering() {
        let record = ContactRecord {
            bin_x: 0,
            bin_y: 1,
            counts: 12.0,
        };
        assert_eq!(
            normalize_contact(&record, Some(&[2.0, 3.0])),
            Some(ContactRecord {
                bin_x: 0,
                bin_y: 1,
                counts: 2.0,
            })
        );
        assert!(normalize_contact(&record, Some(&[f64::NAN, 3.0])).is_none());
        assert!(
            normalize_contact(&record, Some(&[0.0, 3.0]))
                .unwrap()
                .counts
                .is_infinite()
        );
    }

    #[test]
    fn expected_value_uses_chromosome_factor_and_last_distance_value() {
        let expected = ExpectedValueVector {
            key: ExpectedValueKey {
                normalization: "NONE".to_owned(),
                unit: MatrixUnit::BasePairs,
                resolution: 100,
            },
            values: vec![10.0, 4.0],
            chromosome_factors: [(1, 2.0)].into_iter().collect(),
        };
        assert_eq!(expected.value_for(1, 0), Some(5.0));
        assert_eq!(expected.value_for(1, 100), Some(2.0));
        assert_eq!(expected.value_for(2, 1), Some(4.0));
    }

    #[test]
    fn observed_over_expected_raster_uses_contact_distance() {
        let expected = ExpectedValueVector {
            key: ExpectedValueKey {
                normalization: "NONE".to_owned(),
                unit: MatrixUnit::BasePairs,
                resolution: 100,
            },
            values: vec![1.0, 2.0, 4.0],
            chromosome_factors: std::collections::BTreeMap::new(),
        };
        let record = ContactRecord {
            bin_x: 2,
            bin_y: 0,
            counts: 8.0,
        };
        let mut values = vec![0.0; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
        accumulate_block(
            &mut values,
            [0, 0, 3, 3],
            false,
            &MatrixZoom {
                unit: MatrixUnit::BasePairs,
                bin_size: 100,
                sum_counts: 0.0,
                occupied_cell_count: 0.0,
                standard_deviation: 0.0,
                percentile_95: 0.0,
                block_bin_count: 1,
                block_column_count: 1,
                blocks: BTreeMap::new(),
            },
            None,
            None,
            Some(&expected),
            MatrixType::ObservedOverExpected,
            1,
            &[record],
        );
        // The mapped positions are two bins apart, so 8 / expected[2] = 2.
        let x = (2 * OUTPUT_SIZE / 4) as usize;
        assert_eq!(values[x], 2.0);
    }

    #[test]
    fn observed_over_expected_uses_mapped_assembly_distance() {
        let expected = ExpectedValueVector {
            key: ExpectedValueKey {
                normalization: "NONE".to_owned(),
                unit: MatrixUnit::BasePairs,
                resolution: 1,
            },
            values: vec![20.0, 10.0, 5.0],
            chromosome_factors: [(1, 2.0)].into_iter().collect(),
        };
        let source_record = ContactRecord {
            bin_x: 0,
            bin_y: 2,
            counts: 20.0,
        };

        // A reordered assembly made this source pair adjacent.  Java remaps
        // contact bins before HeatmapRenderer calculates abs(binX - binY), so
        // the denominator must be the distance-one entry (10 / factor 2).
        assert_eq!(
            observed_over_expected(source_record, &expected, 1, 4, 5),
            Some(4.0)
        );
    }

    #[test]
    fn dense_expected_fills_zero_contact_cells_by_distance() {
        let expected = ExpectedValueVector {
            key: ExpectedValueKey {
                normalization: "NONE".to_owned(),
                unit: MatrixUnit::BasePairs,
                resolution: 1,
            },
            values: vec![4.0, 2.0],
            chromosome_factors: BTreeMap::new(),
        };
        let mut values = vec![0.0; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
        accumulate_expected(&mut values, [0, 0, 1, 1], 1, &expected);

        assert!(values.iter().all(|value| *value > 0.0));
        assert_eq!(values[0], 4.0);
        assert_eq!(values[1], 4.0);
        assert_eq!(values[OUTPUT_SIZE as usize], 4.0);
    }
}
