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
use heatmap_core::{
    GenomeViewport, IntensityTile, MAX_PEARSON_CELLS, PearsonError, PearsonMatrix, TileKey,
    compute_java_pearsons_cancellable,
};
use heatmap_wgpu::comparison::{
    ContactMap, combine_triangles, observed_over_expected_score, rasterize_ratio_contacts,
    scale_for_vs,
};
use hic_core::{
    ContactRecord, ExpectedValueKey, ExpectedValueVector, HicFile, Matrix, MatrixUnit, MatrixZoom,
    NormalizationKey,
};

pub(crate) const OUTPUT_SIZE: u32 = 1024;
const PREFETCH_BLOCK_RINGS: i32 = 2;
const BLOCK_CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const VIEW_OVERSCAN_FACTOR: f64 = 1.5;
const MAX_BLOCK_READ_CONCURRENCY: usize = 16;
const MIN_PEARSON_BIN_SIZE_BP: u32 = 50_000;
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
    pub observed_normalization: Normalization,
    pub control_normalization: Normalization,
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
    Pearson,
    Control,
    ControlOverExpected,
    ControlPearson,
    Vs,
    Ratio,
    RatioV2,
    ObservedOverExpectedVs,
    PearsonVs,
}

impl MatrixType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Observed => "Observed",
            Self::Expected => "Expected",
            Self::ObservedOverExpected => "O/E",
            Self::Pearson => "Pearson",
            Self::Control => "Control",
            Self::ControlOverExpected => "Control/ExpectedC",
            Self::ControlPearson => "Control Pearson",
            Self::Vs => "Observed vs Control",
            Self::Ratio => "Observed/Control * (AvgC/AvgO)",
            Self::RatioV2 => "Log[Observed/Control * (AvgC/AvgO)]",
            Self::ObservedOverExpectedVs => "O/E vs Control/ExpectedC",
            Self::PearsonVs => "Observed Pearson vs Control Pearson",
        }
    }

    pub fn next(self, control_available: bool) -> Self {
        if !control_available {
            return match self {
                Self::Observed => Self::Expected,
                Self::Expected => Self::ObservedOverExpected,
                Self::ObservedOverExpected => Self::Pearson,
                _ => Self::Observed,
            };
        }
        match self {
            Self::Observed => Self::Control,
            Self::Control => Self::Expected,
            Self::Expected => Self::Vs,
            Self::Vs => Self::Ratio,
            Self::Ratio => Self::RatioV2,
            Self::RatioV2 => Self::ObservedOverExpected,
            Self::ObservedOverExpected => Self::ControlOverExpected,
            Self::ControlOverExpected => Self::ObservedOverExpectedVs,
            Self::ObservedOverExpectedVs => Self::Pearson,
            Self::Pearson => Self::ControlPearson,
            Self::ControlPearson => Self::PearsonVs,
            Self::PearsonVs => Self::Observed,
        }
    }

    pub fn uses_control(self) -> bool {
        matches!(
            self,
            Self::Control | Self::ControlOverExpected | Self::ControlPearson
        )
    }

    fn is_pearson(self) -> bool {
        matches!(self, Self::Pearson | Self::ControlPearson | Self::PearsonVs)
    }

    fn is_observed(self) -> bool {
        matches!(self, Self::Observed | Self::Control)
    }

    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Vs | Self::Ratio | Self::RatioV2 | Self::ObservedOverExpectedVs | Self::PearsonVs
        )
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
    observed_normalization: Arc<Mutex<Normalization>>,
    control_normalization: Arc<Mutex<Normalization>>,
    matrix_type: Arc<Mutex<MatrixType>>,
    results: Receiver<Result<TileResult, String>>,
}

impl TileEngine {
    pub fn spawn(
        path: PathBuf,
        matrix_key: String,
        assembly_map: Option<AssemblyCoordinateMap>,
        control_path: Option<PathBuf>,
    ) -> Result<Self> {
        // Fail fast on metadata errors before starting the worker.
        let file = HicFile::open(&path)?;
        let matrix = file.read_matrix(&matrix_key)?;
        if let Some(control_path) = &control_path {
            let control_file = HicFile::open(control_path)?;
            let control_matrix = control_file.read_matrix(&matrix_key)?;
            validate_control_compatibility(&file, &matrix, &control_file, &control_matrix)?;
        }

        let request = Arc::new((Mutex::new(None), Condvar::new()));
        let latest_generation = Arc::new(AtomicU64::new(0));
        let assembly_map = Arc::new(Mutex::new(assembly_map.map(Arc::new)));
        let assembly_version = Arc::new(AtomicU64::new(0));
        let observed_normalization = Arc::new(Mutex::new(Normalization::None));
        let control_normalization = Arc::new(Mutex::new(Normalization::None));
        let matrix_type = Arc::new(Mutex::new(MatrixType::Observed));
        let (sender, results) = mpsc::channel();
        let worker_request = Arc::clone(&request);
        let worker_generation = Arc::clone(&latest_generation);
        thread::Builder::new()
            .name("hic-tile-worker".to_owned())
            .spawn(move || {
                if let Err(error) = worker_loop(
                    path,
                    control_path,
                    matrix_key,
                    worker_request,
                    worker_generation,
                    sender,
                ) {
                    app_log!("tile worker stopped: {error:#}");
                }
            })?;
        Ok(Self {
            request,
            latest_generation,
            assembly_map,
            assembly_version,
            observed_normalization,
            control_normalization,
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
            observed_normalization: *self
                .observed_normalization
                .lock()
                .expect("observed normalization mutex poisoned"),
            control_normalization: *self
                .control_normalization
                .lock()
                .expect("control normalization mutex poisoned"),
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
            .observed_normalization
            .lock()
            .expect("observed normalization mutex poisoned") = normalization;
    }

    pub fn update_control_normalization(&self, normalization: Normalization) {
        *self
            .control_normalization
            .lock()
            .expect("control normalization mutex poisoned") = normalization;
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
    control_path: Option<PathBuf>,
    matrix_key: String,
    request: Arc<(Mutex<Option<TileRequest>>, Condvar)>,
    latest_generation: Arc<AtomicU64>,
    sender: Sender<Result<TileResult, String>>,
) -> Result<()> {
    let file = HicFile::open(&path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    let control = control_path
        .map(|path| -> Result<_> {
            let control_file = HicFile::open(path)?;
            let control_matrix = control_file.read_matrix(&matrix_key)?;
            validate_control_compatibility(&file, &matrix, &control_file, &control_matrix)?;
            Ok((control_file, control_matrix))
        })
        .transpose()?;
    let mut observed_state = DatasetWorkerState::new();
    let mut control_state = DatasetWorkerState::new();

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
        if next.matrix_type.is_comparison() {
            let (control_file, control_matrix) = control
                .as_ref()
                .context("selected comparison MatrixType requires a control .hic dataset")?;
            match build_comparison_tile(
                &file,
                &matrix,
                &mut observed_state,
                control_file,
                control_matrix,
                &mut control_state,
                next,
                &latest_generation,
                &sender,
            ) {
                Ok(Some(completed_viewport)) => {
                    prefetch_viewport(
                        &file,
                        &matrix,
                        matrix.chromosome_1 == matrix.chromosome_2,
                        request_assembly_map.as_deref(),
                        completed_viewport,
                        &latest_generation,
                        &request,
                        &mut observed_state.block_cache,
                    )?;
                    if !has_pending_request(&request) {
                        prefetch_viewport(
                            control_file,
                            control_matrix,
                            control_matrix.chromosome_1 == control_matrix.chromosome_2,
                            request_assembly_map.as_deref(),
                            completed_viewport,
                            &latest_generation,
                            &request,
                            &mut control_state.block_cache,
                        )?;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if sender.send(Err(format!("{error:#}"))).is_err() {
                        return Ok(());
                    }
                }
            }
            continue;
        }
        let use_control = next.matrix_type.uses_control();
        let (active_file, active_matrix, state) = if use_control {
            let (control_file, control_matrix) = control
                .as_ref()
                .context("selected MatrixType requires a control .hic dataset")?;
            (control_file, control_matrix, &mut control_state)
        } else {
            (&file, &matrix, &mut observed_state)
        };
        let symmetric = active_matrix.chromosome_1 == active_matrix.chromosome_2;
        match build_tile_streaming(
            active_file,
            active_matrix,
            symmetric,
            next,
            &latest_generation,
            &mut state.block_cache,
            &mut state.normalization_cache,
            &mut state.expected_cache,
            &mut state.pearson_cache,
            &sender,
        ) {
            Ok(Some(completed_viewport)) => {
                prefetch_viewport(
                    active_file,
                    active_matrix,
                    symmetric,
                    request_assembly_map.as_deref(),
                    completed_viewport,
                    &latest_generation,
                    &request,
                    &mut state.block_cache,
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
    pearson_cache: &mut PearsonCache,
    sender: &Sender<Result<TileResult, String>>,
) -> Result<Option<GenomeViewport>> {
    let started = Instant::now();
    let generation = request.viewport.generation;
    let assembly_map = request.assembly_map.as_deref();
    let normalization = if request.matrix_type.uses_control() {
        request.control_normalization
    } else {
        request.observed_normalization
    };
    let zoom = if request.matrix_type.is_pearson() {
        choose_pearson_zoom(
            matrix,
            request.viewport.span_bp / f64::from(OUTPUT_SIZE),
            request.viewport.genome_length_bp,
        )
        .context("matrix has no Pearson-compatible base-pair zoom within the memory budget")?
    } else {
        choose_zoom(matrix, request.viewport.span_bp / f64::from(OUTPUT_SIZE))
            .context("matrix has no base-pair zoom")?
    };
    // Expected uses the footer vector directly; no per-bin normalization
    // vector is needed to build its dense diagonal field.  Observed and O/E
    // retain Java's normalized-contact calculation before rasterization.
    let normalization_vector = matches!(
        request.matrix_type,
        MatrixType::Observed
            | MatrixType::ObservedOverExpected
            | MatrixType::Control
            | MatrixType::ControlOverExpected
    )
    .then(|| {
        normalization_vector(
            file,
            matrix.chromosome_1,
            normalization,
            normalization_cache,
            request.viewport.span_bp / f64::from(OUTPUT_SIZE),
            matrix,
        )
    })
    .transpose()?
    .flatten();
    let expected_vector = (!request.matrix_type.is_observed())
        .then(|| expected_vector(file, normalization, zoom.bin_size, expected_cache))
        .transpose()?;
    let rendered_viewport = rendered_viewport_for(request.viewport);
    let bin_bounds = viewport_bin_bounds(rendered_viewport, zoom.bin_size);
    if request.matrix_type.is_pearson() {
        let pearson = pearson_matrix(
            file,
            matrix,
            zoom,
            normalization,
            expected_vector
                .as_deref()
                .context("Pearson expected values are unavailable")?,
            pearson_cache,
            latest_generation,
            generation,
        )?;
        let Some(pearson) = pearson else {
            return Ok(None);
        };
        let raw_values = rasterize_dense_pearson(&pearson, bin_bounds, zoom.bin_size, assembly_map);
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
    // Keep cached blocks in their viewport order instead of accumulating all
    // of them before publishing.  An assembly edit normally reuses precisely
    // the same compressed source blocks but maps them to different display
    // coordinates.  Waiting to rasterize every cache hit made that edit look
    // inert for hundreds of milliseconds: no I/O was pending, yet no new
    // texture was ever sent until the entire remap had finished.
    let mut cached_blocks = Vec::new();
    let mut missing_blocks = Vec::new();

    for &block_number in &visible_blocks {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
        if let Some(block) = cache.get(zoom, block_number) {
            cached_blocks.push(block);
        } else {
            missing_blocks.push(block_number);
        }
    }
    let cache_hits = cached_blocks.len();
    let cache_misses = missing_blocks.len();
    let total_blocks = visible_blocks.len();
    let mut loaded_blocks = 0;
    let mut published_generation_texture = false;

    // Cached data is still new display data after an assembly reorder.  Send
    // it block-by-block just like freshly decompressed data: the first result
    // atomically replaces the old assembly texture, and later results fill
    // only the newly changed pixel regions.
    for block in cached_blocks {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
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

    // An empty viewport has no cached or missing Block to trigger the stream
    // above, but it still needs a complete all-zero texture.
    if cache_misses == 0 && !published_generation_texture {
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
    reason = "comparison construction deliberately keeps the two isolated dataset states explicit"
)]
fn build_comparison_tile(
    observed_file: &HicFile,
    observed_matrix: &Matrix,
    observed_state: &mut DatasetWorkerState,
    control_file: &HicFile,
    control_matrix: &Matrix,
    control_state: &mut DatasetWorkerState,
    request: TileRequest,
    latest_generation: &AtomicU64,
    sender: &Sender<Result<TileResult, String>>,
) -> Result<Option<GenomeViewport>> {
    let started = Instant::now();
    let generation = request.viewport.generation;
    let target_bp_per_pixel = request.viewport.span_bp / f64::from(OUTPUT_SIZE);
    let (observed_zoom, control_zoom) = choose_common_zoom(
        observed_matrix,
        control_matrix,
        target_bp_per_pixel,
        request.matrix_type == MatrixType::PearsonVs,
        request.viewport.genome_length_bp,
    )
    .context("observed and control have no common compatible base-pair zoom")?;
    let rendered_viewport = rendered_viewport_for(request.viewport);
    let bin_bounds = viewport_bin_bounds(rendered_viewport, observed_zoom.bin_size);
    if matches!(request.matrix_type, MatrixType::Ratio | MatrixType::RatioV2) {
        let Some((observed_contacts, observed_stats)) = collect_dataset_contacts(
            observed_file,
            observed_matrix,
            observed_zoom,
            observed_state,
            request.observed_normalization,
            &request,
            latest_generation,
        )?
        else {
            return Ok(None);
        };
        let Some((control_contacts, control_stats)) = collect_dataset_contacts(
            control_file,
            control_matrix,
            control_zoom,
            control_state,
            request.control_normalization,
            &request,
            latest_generation,
        )?
        else {
            return Ok(None);
        };
        let values = rasterize_ratio_contacts(
            &observed_contacts,
            &control_contacts,
            bin_bounds,
            OUTPUT_SIZE,
            observed_matrix.chromosome_1 == observed_matrix.chromosome_2,
            zoom_average_count(observed_file, observed_matrix, observed_zoom),
            zoom_average_count(control_file, control_matrix, control_zoom),
        );
        let visible_blocks = observed_stats.visible_blocks + control_stats.visible_blocks;
        let cache_hits = observed_stats.cache_hits + control_stats.cache_hits;
        let cache_misses = observed_stats.cache_misses + control_stats.cache_misses;
        send_stream_result(
            sender,
            &request,
            rendered_viewport,
            observed_zoom.bin_size,
            &values,
            visible_blocks,
            cache_hits,
            cache_misses,
            visible_blocks,
            visible_blocks,
            true,
            None,
            started,
        )?;
        return Ok(Some(rendered_viewport));
    }

    let (observed_values, control_values, observed_stats, control_stats) = if request.matrix_type
        == MatrixType::PearsonVs
    {
        let observed_expected = expected_vector(
            observed_file,
            request.observed_normalization,
            observed_zoom.bin_size,
            &mut observed_state.expected_cache,
        )?;
        let control_expected = expected_vector(
            control_file,
            request.control_normalization,
            control_zoom.bin_size,
            &mut control_state.expected_cache,
        )?;
        let Some(observed_pearson) = pearson_matrix(
            observed_file,
            observed_matrix,
            observed_zoom,
            request.observed_normalization,
            &observed_expected,
            &mut observed_state.pearson_cache,
            latest_generation,
            generation,
        )?
        else {
            return Ok(None);
        };
        let Some(control_pearson) = pearson_matrix(
            control_file,
            control_matrix,
            control_zoom,
            request.control_normalization,
            &control_expected,
            &mut control_state.pearson_cache,
            latest_generation,
            generation,
        )?
        else {
            return Ok(None);
        };
        (
            rasterize_dense_pearson(
                &observed_pearson,
                bin_bounds,
                observed_zoom.bin_size,
                request.assembly_map.as_deref(),
            ),
            rasterize_dense_pearson(
                &control_pearson,
                bin_bounds,
                control_zoom.bin_size,
                request.assembly_map.as_deref(),
            ),
            DatasetRasterStats::default(),
            DatasetRasterStats::default(),
        )
    } else {
        let observed_source_type = if request.matrix_type == MatrixType::ObservedOverExpectedVs {
            MatrixType::ObservedOverExpected
        } else {
            MatrixType::Observed
        };
        let control_source_type = if request.matrix_type == MatrixType::ObservedOverExpectedVs {
            MatrixType::ControlOverExpected
        } else {
            MatrixType::Control
        };
        let Some((observed_values, observed_stats)) = rasterize_dataset(
            observed_file,
            observed_matrix,
            observed_zoom,
            observed_state,
            request.observed_normalization,
            observed_source_type,
            &request,
            bin_bounds,
            latest_generation,
        )?
        else {
            return Ok(None);
        };
        let Some((control_values, control_stats)) = rasterize_dataset(
            control_file,
            control_matrix,
            control_zoom,
            control_state,
            request.control_normalization,
            control_source_type,
            &request,
            bin_bounds,
            latest_generation,
        )?
        else {
            return Ok(None);
        };
        (
            observed_values,
            control_values,
            observed_stats,
            control_stats,
        )
    };
    if is_stale(latest_generation, generation) {
        return Ok(None);
    }

    let values = match request.matrix_type {
        MatrixType::Vs => combine_triangles(
            &scale_for_vs(
                &observed_values,
                zoom_average_count(observed_file, observed_matrix, observed_zoom),
                zoom_average_count(control_file, control_matrix, control_zoom),
            ),
            &scale_for_vs(
                &control_values,
                zoom_average_count(control_file, control_matrix, control_zoom),
                zoom_average_count(observed_file, observed_matrix, observed_zoom),
            ),
        ),
        MatrixType::Ratio | MatrixType::RatioV2 => {
            unreachable!("ratio modes use contact-level pairing before rasterization")
        }
        MatrixType::ObservedOverExpectedVs | MatrixType::PearsonVs => {
            combine_triangles(&observed_values, &control_values)
        }
        _ => unreachable!("comparison builder received a non-comparison MatrixType"),
    };
    let visible_blocks = observed_stats.visible_blocks + control_stats.visible_blocks;
    let cache_hits = observed_stats.cache_hits + control_stats.cache_hits;
    let cache_misses = observed_stats.cache_misses + control_stats.cache_misses;
    send_stream_result(
        sender,
        &request,
        rendered_viewport,
        observed_zoom.bin_size,
        &values,
        visible_blocks,
        cache_hits,
        cache_misses,
        visible_blocks,
        visible_blocks,
        true,
        None,
        started,
    )?;
    Ok(Some(rendered_viewport))
}

#[derive(Default)]
struct DatasetRasterStats {
    visible_blocks: usize,
    cache_hits: usize,
    cache_misses: usize,
}

#[allow(
    clippy::too_many_arguments,
    reason = "contact collection keeps the isolated dataset and cancellation inputs explicit"
)]
fn collect_dataset_contacts(
    file: &HicFile,
    matrix: &Matrix,
    zoom: &MatrixZoom,
    state: &mut DatasetWorkerState,
    normalization: Normalization,
    request: &TileRequest,
    latest_generation: &AtomicU64,
) -> Result<Option<(ContactMap, DatasetRasterStats)>> {
    let generation = request.viewport.generation;
    let normalization_vector = normalization_vector(
        file,
        matrix.chromosome_1,
        normalization,
        &mut state.normalization_cache,
        f64::from(zoom.bin_size),
        matrix,
    )?;
    let symmetric = matrix.chromosome_1 == matrix.chromosome_2;
    let blocks = block_numbers_for_viewport(
        zoom,
        rendered_viewport_for(request.viewport),
        symmetric,
        request.assembly_map.as_deref(),
    );
    let mut contacts = ContactMap::new();
    let mut missing = Vec::new();
    let mut cache_hits = 0;
    let mut collect = |records: &[ContactRecord]| {
        for record in records {
            let Some(normalized) =
                normalize_contact(record, normalization_vector.as_deref().map(Vec::as_slice))
            else {
                continue;
            };
            let Some((bin_x, bin_y, counts)) =
                map_contact(&normalized, zoom.bin_size, request.assembly_map.as_deref())
            else {
                continue;
            };
            let key = if symmetric && bin_y < bin_x {
                (bin_y, bin_x)
            } else {
                (bin_x, bin_y)
            };
            *contacts.entry(key).or_insert(0.0) += counts;
        }
    };
    for &block_number in &blocks {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
        if let Some(block) = state.block_cache.get(zoom, block_number) {
            cache_hits += 1;
            collect(&block);
        } else {
            missing.push(block_number);
        }
    }
    let mut became_stale = false;
    stream_blocks_parallel(file, zoom, &missing, |block_number, records| {
        if is_stale(latest_generation, generation) {
            became_stale = true;
            return Ok(());
        }
        let block = state.block_cache.insert(zoom, block_number, records);
        collect(&block);
        Ok(())
    })?;
    if became_stale || is_stale(latest_generation, generation) {
        return Ok(None);
    }
    Ok(Some((
        contacts,
        DatasetRasterStats {
            visible_blocks: blocks.len(),
            cache_hits,
            cache_misses: missing.len(),
        },
    )))
}

#[allow(
    clippy::too_many_arguments,
    reason = "dataset rasterization keeps scientific and cache inputs explicit"
)]
fn rasterize_dataset(
    file: &HicFile,
    matrix: &Matrix,
    zoom: &MatrixZoom,
    state: &mut DatasetWorkerState,
    normalization: Normalization,
    source_type: MatrixType,
    request: &TileRequest,
    bin_bounds: [i32; 4],
    latest_generation: &AtomicU64,
) -> Result<Option<(Vec<f32>, DatasetRasterStats)>> {
    let generation = request.viewport.generation;
    let normalization_vector = normalization_vector(
        file,
        matrix.chromosome_1,
        normalization,
        &mut state.normalization_cache,
        f64::from(zoom.bin_size),
        matrix,
    )?;
    let expected = matches!(
        source_type,
        MatrixType::ObservedOverExpected | MatrixType::ControlOverExpected
    )
    .then(|| {
        expected_vector(
            file,
            normalization,
            zoom.bin_size,
            &mut state.expected_cache,
        )
    })
    .transpose()?;
    let symmetric = matrix.chromosome_1 == matrix.chromosome_2;
    let blocks = block_numbers_for_viewport(
        zoom,
        rendered_viewport_for(request.viewport),
        symmetric,
        request.assembly_map.as_deref(),
    );
    let mut values = vec![0.0_f32; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
    let mut missing = Vec::new();
    let mut cache_hits = 0;
    for &block_number in &blocks {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
        if let Some(block) = state.block_cache.get(zoom, block_number) {
            cache_hits += 1;
            accumulate_block(
                &mut values,
                bin_bounds,
                symmetric,
                zoom,
                request.assembly_map.as_deref(),
                normalization_vector.as_deref().map(Vec::as_slice),
                expected.as_deref(),
                source_type,
                matrix.chromosome_1,
                &block,
            );
        } else {
            missing.push(block_number);
        }
    }
    let mut became_stale = false;
    stream_blocks_parallel(file, zoom, &missing, |block_number, records| {
        if is_stale(latest_generation, generation) {
            became_stale = true;
            return Ok(());
        }
        let block = state.block_cache.insert(zoom, block_number, records);
        accumulate_block(
            &mut values,
            bin_bounds,
            symmetric,
            zoom,
            request.assembly_map.as_deref(),
            normalization_vector.as_deref().map(Vec::as_slice),
            expected.as_deref(),
            source_type,
            matrix.chromosome_1,
            &block,
        );
        Ok(())
    })?;
    if became_stale || is_stale(latest_generation, generation) {
        return Ok(None);
    }
    Ok(Some((
        values,
        DatasetRasterStats {
            visible_blocks: blocks.len(),
            cache_hits,
            cache_misses: missing.len(),
        },
    )))
}

fn zoom_average_count(file: &HicFile, matrix: &Matrix, zoom: &MatrixZoom) -> f32 {
    let chromosome_1 = &file.header.chromosomes[matrix.chromosome_1 as usize];
    let chromosome_2 = &file.header.chromosomes[matrix.chromosome_2 as usize];
    let bins_1 = (chromosome_1.length / u64::from(zoom.bin_size)).max(1) as f64;
    let bins_2 = (chromosome_2.length / u64::from(zoom.bin_size)).max(1) as f64;
    (f64::from(zoom.sum_counts) / bins_1 / bins_2) as f32
}

fn choose_common_zoom<'a>(
    observed: &'a Matrix,
    control: &'a Matrix,
    target_bp_per_pixel: f64,
    pearson: bool,
    genome_length_bp: f64,
) -> Option<(&'a MatrixZoom, &'a MatrixZoom)> {
    observed
        .zooms
        .iter()
        .filter(|zoom| zoom.unit == MatrixUnit::BasePairs)
        .filter(|zoom| {
            !pearson
                || (zoom.bin_size >= MIN_PEARSON_BIN_SIZE_BP
                    && pearson_cells_within_budget(genome_length_bp, zoom.bin_size))
        })
        .filter_map(|observed_zoom| {
            control
                .zooms
                .iter()
                .find(|control_zoom| {
                    control_zoom.unit == MatrixUnit::BasePairs
                        && control_zoom.bin_size == observed_zoom.bin_size
                })
                .map(|control_zoom| (observed_zoom, control_zoom))
        })
        .min_by(|(left, _), (right, _)| {
            let left_distance = (f64::from(left.bin_size) / target_bp_per_pixel.max(1.0))
                .ln()
                .abs();
            let right_distance = (f64::from(right.bin_size) / target_bp_per_pixel.max(1.0))
                .ln()
                .abs();
            left_distance.total_cmp(&right_distance)
        })
}

fn pearson_cells_within_budget(genome_length_bp: f64, bin_size: u32) -> bool {
    let dimension = (genome_length_bp / f64::from(bin_size)).floor() as usize + 1;
    dimension
        .checked_mul(dimension)
        .is_some_and(|cells| cells <= MAX_PEARSON_CELLS)
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
    let normalization = if request.matrix_type.uses_control() {
        request.control_normalization
    } else {
        request.observed_normalization
    };
    // Keep the scientific matrix values in the R32F texture. MatrixType
    // transforms (normalization and O/E) belong above, while display-only
    // scaling belongs in the shader/color range. Applying ln(1+x) here made
    // Observed, Expected, and O/E numerically different from Java.
    let color_max = if request.matrix_type.is_pearson() {
        1.0
    } else if complete {
        let full = IntensityTile::new(
            TileKey {
                dataset: 1,
                matrix_type: matrix_type_id(request.matrix_type),
                normalization: normalization_id(normalization),
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
            normalization: normalization_id(normalization),
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
            normalization,
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
                MatrixType::Observed | MatrixType::Control => counts,
                MatrixType::ObservedOverExpected | MatrixType::ControlOverExpected => {
                    observed_over_expected(
                        ContactRecord {
                            bin_x,
                            bin_y,
                            counts,
                        },
                        expected_vector?,
                        chromosome,
                        bin_x,
                        bin_y,
                    )?
                }
                MatrixType::Expected => unreachable!("expected tiles do not read sparse contacts"),
                MatrixType::Pearson | MatrixType::ControlPearson => {
                    unreachable!("Pearson tiles use the dense matrix path")
                }
                MatrixType::Vs
                | MatrixType::Ratio
                | MatrixType::RatioV2
                | MatrixType::ObservedOverExpectedVs
                | MatrixType::PearsonVs => {
                    unreachable!("comparison tiles use the dual-dataset path")
                }
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
        MatrixType::Pearson => 3,
        MatrixType::Control => 4,
        MatrixType::ControlOverExpected => 5,
        MatrixType::ControlPearson => 6,
        MatrixType::Vs => 7,
        MatrixType::Ratio => 8,
        MatrixType::RatioV2 => 9,
        MatrixType::ObservedOverExpectedVs => 10,
        MatrixType::PearsonVs => 11,
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
    let expected = expected.value_for(chromosome, distance)? as f32;
    let score = observed_over_expected_score(record.counts, expected, 0.0);
    (score != 0.0).then_some(score)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Pearson construction needs dataset metadata, scientific inputs, cache state, and cooperative cancellation"
)]
fn pearson_matrix(
    file: &HicFile,
    matrix: &Matrix,
    zoom: &MatrixZoom,
    normalization: Normalization,
    expected: &ExpectedValueVector,
    cache: &mut PearsonCache,
    latest_generation: &AtomicU64,
    generation: u64,
) -> Result<Option<Arc<PearsonMatrix>>> {
    if matrix.chromosome_1 != matrix.chromosome_2 {
        anyhow::bail!("Pearson is only defined for intra-chromosomal matrices");
    }
    let key = (normalization, zoom.bin_size);
    if let Some(matrix) = cache.get(key) {
        return Ok(Some(matrix));
    }
    // Retain only the active Pearson matrix. Drop an incompatible entry before
    // allocating the next dense O/E buffer so repeated normalization/LOD
    // changes cannot accumulate hundreds of MiB for the worker lifetime.
    cache.remove_if_different(key);
    let chromosome = file
        .header
        .chromosomes
        .get(matrix.chromosome_1 as usize)
        .context("matrix chromosome is outside the header dictionary")?;
    let dimension = usize::try_from(chromosome.length / u64::from(zoom.bin_size) + 1)?;
    let cells = dimension
        .checked_mul(dimension)
        .context("Pearson matrix dimensions overflow")?;
    if cells > MAX_PEARSON_CELLS {
        anyhow::bail!(
            "Pearson matrix contains {} cells, exceeding the safety limit {}",
            cells,
            MAX_PEARSON_CELLS
        );
    }
    let mut oe = vec![0.0_f64; cells];
    let mut valid = vec![false; dimension];
    // Match MatrixZoomData.populateOEMatrixAndBitset exactly: the selected
    // normalization only changes the expected-value vector. Java still feeds
    // raw/NONE contact records into the dense O/E matrix.
    for &block_number in zoom.blocks.keys() {
        if is_stale(latest_generation, generation) {
            return Ok(None);
        }
        for record in file.read_block(zoom, block_number)? {
            if record.counts.is_nan() {
                continue;
            }
            let Ok(x) = usize::try_from(record.bin_x) else {
                continue;
            };
            let Ok(y) = usize::try_from(record.bin_y) else {
                continue;
            };
            if x >= dimension || y >= dimension {
                continue;
            }
            let distance = u64::from((record.bin_x - record.bin_y).unsigned_abs());
            let Some(expected_count) = expected.value_for(matrix.chromosome_1, distance) else {
                continue;
            };
            let value = f64::from(record.counts) / expected_count;
            oe[x * dimension + y] = value;
            oe[y * dimension + x] = value;
            valid[x] = true;
            valid[y] = true;
        }
    }
    let matrix = match compute_java_pearsons_cancellable(
        oe,
        dimension,
        &valid,
        MAX_BLOCK_READ_CONCURRENCY,
        &|| is_stale(latest_generation, generation),
    ) {
        Ok(matrix) => Arc::new(matrix),
        Err(PearsonError::Cancelled) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if is_stale(latest_generation, generation) {
        return Ok(None);
    }
    cache.insert(key, Arc::clone(&matrix));
    Ok(Some(matrix))
}

fn rasterize_dense_pearson(
    matrix: &PearsonMatrix,
    bin_bounds: [i32; 4],
    bin_size: u32,
    assembly_map: Option<&AssemblyCoordinateMap>,
) -> Vec<f32> {
    let output_size = OUTPUT_SIZE as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as usize;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as usize;
    let mut values = vec![f32::NAN; output_size * output_size];
    for y in 0..output_size {
        let source_y = bin_bounds[1]
            + i32::try_from(output_pixel_to_source_bin(y, height_bins, output_size))
                .unwrap_or(i32::MAX);
        for x in 0..output_size {
            let source_x = bin_bounds[0]
                + i32::try_from(output_pixel_to_source_bin(x, width_bins, output_size))
                    .unwrap_or(i32::MAX);
            let Some(source_y) = dense_source_bin(source_y, bin_size, assembly_map) else {
                continue;
            };
            let Some(source_x) = dense_source_bin(source_x, bin_size, assembly_map) else {
                continue;
            };
            if let Some(value) = matrix.get(source_y, source_x) {
                values[y * output_size + x] = value;
            }
        }
    }
    values
}

/// Inverse of the sparse-contact raster boundary
/// `floor(source_bin * output_size / source_bins)`.  A simple
/// `floor(pixel * source_bins / output_size)` maps the first pixel of most
/// bins back to the preceding bin whenever the scale is not integral (for
/// example pixel 170 in a 6-bin, 1024-pixel raster).  The ceil-based inverse
/// keeps dense Pearson cells aligned with sparse MatrixType cells and Java's
/// one-bin-per-pixel renderer.
fn output_pixel_to_source_bin(pixel: usize, source_bins: usize, output_size: usize) -> usize {
    assert!(source_bins > 0);
    assert!(output_size > 0);
    pixel
        .saturating_add(1)
        .saturating_mul(source_bins)
        .div_ceil(output_size)
        .saturating_sub(1)
        .min(source_bins - 1)
}

fn dense_source_bin(
    displayed_bin: i32,
    bin_size: u32,
    assembly_map: Option<&AssemblyCoordinateMap>,
) -> Option<usize> {
    let displayed_bin = u64::try_from(displayed_bin).ok()?;
    let bin_size = u64::from(bin_size);
    let displayed_coordinate = displayed_bin.checked_mul(bin_size)?;
    let source_coordinate = assembly_map
        .map(|map| {
            map.assembly_to_source(displayed_coordinate.min(map.total_length().saturating_sub(1)))
        })
        .unwrap_or(Some(displayed_coordinate))?;
    usize::try_from(source_coordinate / bin_size).ok()
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

fn choose_pearson_zoom(
    matrix: &Matrix,
    target_bp_per_pixel: f64,
    genome_length_bp: f64,
) -> Option<&MatrixZoom> {
    matrix
        .zooms
        .iter()
        .filter(|zoom| {
            if zoom.unit != MatrixUnit::BasePairs || zoom.bin_size < MIN_PEARSON_BIN_SIZE_BP {
                return false;
            }
            let dimension = (genome_length_bp / f64::from(zoom.bin_size)).floor() as usize + 1;
            dimension
                .checked_mul(dimension)
                .is_some_and(|cells| cells <= MAX_PEARSON_CELLS)
        })
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

fn validate_control_compatibility(
    observed_file: &HicFile,
    observed_matrix: &Matrix,
    control_file: &HicFile,
    control_matrix: &Matrix,
) -> Result<()> {
    let observed_chr_1 = observed_file
        .header
        .chromosomes
        .get(observed_matrix.chromosome_1 as usize)
        .context("observed matrix chromosome 1 is outside the header dictionary")?;
    let observed_chr_2 = observed_file
        .header
        .chromosomes
        .get(observed_matrix.chromosome_2 as usize)
        .context("observed matrix chromosome 2 is outside the header dictionary")?;
    let control_chr_1 = control_file
        .header
        .chromosomes
        .get(control_matrix.chromosome_1 as usize)
        .context("control matrix chromosome 1 is outside the header dictionary")?;
    let control_chr_2 = control_file
        .header
        .chromosomes
        .get(control_matrix.chromosome_2 as usize)
        .context("control matrix chromosome 2 is outside the header dictionary")?;
    if (observed_chr_1.name.as_str(), observed_chr_1.length)
        != (control_chr_1.name.as_str(), control_chr_1.length)
        || (observed_chr_2.name.as_str(), observed_chr_2.length)
            != (control_chr_2.name.as_str(), control_chr_2.length)
    {
        anyhow::bail!(
            "control matrix chromosomes do not match observed: {}({}) x {}({}) versus {}({}) x {}({})",
            observed_chr_1.name,
            observed_chr_1.length,
            observed_chr_2.name,
            observed_chr_2.length,
            control_chr_1.name,
            control_chr_1.length,
            control_chr_2.name,
            control_chr_2.length
        );
    }
    let observed_resolutions: HashSet<u32> = observed_matrix
        .zooms
        .iter()
        .filter(|zoom| zoom.unit == MatrixUnit::BasePairs)
        .map(|zoom| zoom.bin_size)
        .collect();
    if !control_matrix.zooms.iter().any(|zoom| {
        zoom.unit == MatrixUnit::BasePairs && observed_resolutions.contains(&zoom.bin_size)
    }) {
        anyhow::bail!("control matrix has no base-pair resolution shared with observed");
    }
    Ok(())
}

struct DatasetWorkerState {
    block_cache: BlockCache,
    normalization_cache: HashMap<(Normalization, u32, u32), Arc<Vec<f64>>>,
    expected_cache: HashMap<(Normalization, u32), Arc<ExpectedValueVector>>,
    pearson_cache: PearsonCache,
}

impl DatasetWorkerState {
    fn new() -> Self {
        Self {
            block_cache: BlockCache::new(BLOCK_CACHE_BUDGET_BYTES),
            normalization_cache: HashMap::new(),
            expected_cache: HashMap::new(),
            pearson_cache: PearsonCache::default(),
        }
    }
}

#[derive(Default)]
struct PearsonCache {
    entry: Option<((Normalization, u32), Arc<PearsonMatrix>)>,
}

impl PearsonCache {
    fn get(&self, key: (Normalization, u32)) -> Option<Arc<PearsonMatrix>> {
        self.entry
            .as_ref()
            .filter(|(cached_key, _)| *cached_key == key)
            .map(|(_, matrix)| Arc::clone(matrix))
    }

    fn remove_if_different(&mut self, key: (Normalization, u32)) {
        if self
            .entry
            .as_ref()
            .is_some_and(|(cached_key, _)| *cached_key != key)
        {
            self.entry = None;
        }
    }

    fn insert(&mut self, key: (Normalization, u32), matrix: Arc<PearsonMatrix>) {
        self.entry = Some((key, matrix));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        usize::from(self.entry.is_some())
    }
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

    #[test]
    fn dense_pearson_raster_follows_assembly_coordinates() {
        let document = AssemblyDocument::parse(">a 1 10\n>b 2 10\n-2 1\n").unwrap();
        let map = document.coordinate_map().unwrap();
        let matrix = PearsonMatrix {
            dimension: 4,
            values: (0..16).map(|value| value as f32).collect(),
        };
        let raster = rasterize_dense_pearson(&matrix, [0, 0, 3, 3], 5, Some(&map));
        // Displayed assembly bin zero is the last source bin because scaffold
        // 2 is reversed and placed first.
        assert_eq!(raster[0], matrix.values[3 * 4 + 3]);
        assert_eq!(dense_source_bin(1, 5, Some(&map)), Some(2));
    }

    #[test]
    fn pearson_lod_never_selects_a_dense_matrix_above_the_memory_budget() {
        let zoom = |bin_size| MatrixZoom {
            unit: MatrixUnit::BasePairs,
            bin_size,
            sum_counts: 0.0,
            occupied_cell_count: 0.0,
            standard_deviation: 0.0,
            percentile_95: 0.0,
            block_bin_count: 1,
            block_column_count: 1,
            blocks: BTreeMap::new(),
        };
        let matrix = Matrix {
            chromosome_1: 1,
            chromosome_2: 1,
            zooms: vec![zoom(50_000), zoom(100_000), zoom(1_000_000)],
        };
        let chosen = choose_pearson_zoom(&matrix, 50_000.0, 1_000_000_000.0).unwrap();
        assert_eq!(chosen.bin_size, 1_000_000);
    }

    #[test]
    fn pearson_cache_retains_only_the_active_normalization_and_resolution() {
        let matrix = |value| {
            Arc::new(PearsonMatrix {
                dimension: 1,
                values: vec![value],
            })
        };
        let mut cache = PearsonCache::default();
        let first_key = (Normalization::None, 1_000_000);
        let second_key = (Normalization::Kr, 2_500_000);
        cache.insert(first_key, matrix(1.0));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(first_key).unwrap().values, vec![1.0]);

        cache.remove_if_different(second_key);
        assert_eq!(cache.len(), 0);
        cache.insert(second_key, matrix(2.0));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(first_key).is_none());
        assert_eq!(cache.get(second_key).unwrap().values, vec![2.0]);
    }

    #[test]
    fn matrix_type_cycle_exposes_control_only_when_a_dataset_is_loaded() {
        let mut without_control = MatrixType::Observed;
        let mut observed_cycle = Vec::new();
        for _ in 0..4 {
            observed_cycle.push(without_control);
            without_control = without_control.next(false);
        }
        assert_eq!(
            observed_cycle,
            vec![
                MatrixType::Observed,
                MatrixType::Expected,
                MatrixType::ObservedOverExpected,
                MatrixType::Pearson,
            ]
        );
        assert_eq!(without_control, MatrixType::Observed);

        let mut with_control = MatrixType::Observed;
        let mut control_cycle = Vec::new();
        for _ in 0..12 {
            control_cycle.push(with_control);
            with_control = with_control.next(true);
        }
        assert_eq!(
            control_cycle,
            vec![
                MatrixType::Observed,
                MatrixType::Control,
                MatrixType::Expected,
                MatrixType::Vs,
                MatrixType::Ratio,
                MatrixType::RatioV2,
                MatrixType::ObservedOverExpected,
                MatrixType::ControlOverExpected,
                MatrixType::ObservedOverExpectedVs,
                MatrixType::Pearson,
                MatrixType::ControlPearson,
                MatrixType::PearsonVs,
            ]
        );
        assert_eq!(with_control, MatrixType::Observed);
    }

    #[test]
    fn comparison_helpers_match_java_identity_invariants() {
        let observed = vec![2.0; OUTPUT_SIZE as usize * OUTPUT_SIZE as usize];
        let control = observed.clone();
        assert_eq!(scale_for_vs(&observed, 3.0, 3.0), observed);
        assert_eq!(
            combine_triangles(&observed, &control),
            observed,
            "same-file VS must preserve identical values on both triangles"
        );
    }

    #[test]
    fn comparison_helpers_split_observed_and_control_at_the_diagonal() {
        let size = OUTPUT_SIZE as usize;
        let observed = vec![2.0; size * size];
        let control = vec![7.0; size * size];
        let values = combine_triangles(&observed, &control);
        assert_eq!(values[0], 2.0);
        assert_eq!(values[size], 2.0);
        assert_eq!(values[1], 7.0);
    }

    #[test]
    fn contact_level_ratio_matches_java_identity_and_omits_unpaired_contacts() {
        let mut observed = ContactMap::new();
        observed.insert((0, 0), 4.0);
        observed.insert((0, 1), 8.0);
        let mut control = ContactMap::new();
        control.insert((0, 0), 4.0);
        let values = rasterize_ratio_contacts(
            &observed,
            &control,
            [0, 0, 1, 1],
            OUTPUT_SIZE,
            true,
            2.0,
            2.0,
        );
        assert_eq!(values[0], 1.0);
        assert!(values.iter().skip(1).all(|&value| value == 0.0));
    }

    #[test]
    fn dense_inverse_sampling_matches_sparse_raster_boundaries() {
        let source_bins = 6;
        let output_size = OUTPUT_SIZE as usize;
        for source_bin in 0..source_bins {
            let first_pixel = source_bin * output_size / source_bins;
            assert_eq!(
                output_pixel_to_source_bin(first_pixel, source_bins, output_size),
                source_bin,
                "first raster pixel for source bin {source_bin} must sample that bin"
            );
        }
        assert_eq!(output_pixel_to_source_bin(169, source_bins, output_size), 0);
        assert_eq!(output_pixel_to_source_bin(170, source_bins, output_size), 1);
    }
}
