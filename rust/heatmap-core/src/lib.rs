//! Framework-neutral heatmap viewport and scalar-tile types.

use std::fmt;

/// Maximum dense Pearson input accepted by the core.  Pearson is inherently
/// quadratic in chromosome-bin count; bounding the number of cells prevents a
/// malformed request from turning into an uncontrolled allocation while still
/// covering the resolutions Juicebox exposes for normal chromosome views.
pub const MAX_PEARSON_CELLS: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PearsonError {
    Empty,
    DimensionOverflow,
    InvalidMatrixLength { expected: usize, actual: usize },
    InvalidValidityLength { expected: usize, actual: usize },
    CellLimitExceeded { cells: usize, limit: usize },
    Cancelled,
}

impl fmt::Display for PearsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Pearson matrix dimension must be positive"),
            Self::DimensionOverflow => formatter.write_str("Pearson matrix dimensions overflow"),
            Self::InvalidMatrixLength { expected, actual } => write!(
                formatter,
                "Pearson O/E matrix contains {actual} cells; expected {expected}"
            ),
            Self::InvalidValidityLength { expected, actual } => write!(
                formatter,
                "Pearson validity mask contains {actual} entries; expected {expected}"
            ),
            Self::CellLimitExceeded { cells, limit } => write!(
                formatter,
                "Pearson matrix contains {cells} cells, exceeding the safety limit {limit}"
            ),
            Self::Cancelled => formatter.write_str("Pearson computation was superseded"),
        }
    }
}

impl std::error::Error for PearsonError {}

#[derive(Debug, Clone)]
pub struct PearsonMatrix {
    pub dimension: usize,
    pub values: Vec<f32>,
}

impl PearsonMatrix {
    pub fn get(&self, row: usize, column: usize) -> Option<f32> {
        (row < self.dimension && column < self.dimension)
            .then(|| self.values[row * self.dimension + column])
    }
}

/// Reproduces `Pearsons.computePearsons` and
/// `PearsonCorrelationMetric.corr` from the Java application.
///
/// The Java implementation names its first step "subtract row means", but
/// subtracts `rowMeans[column]` from every valid row.  The distinction matters
/// for byte-for-byte scientific comparison, so it is preserved here.  Zeros
/// participate in both means and correlations; only NaNs are omitted from the
/// preliminary means, exactly as in Java.
pub fn compute_java_pearsons(
    observed_over_expected: Vec<f64>,
    dimension: usize,
    valid_bins: &[bool],
    worker_count: usize,
) -> Result<PearsonMatrix, PearsonError> {
    compute_java_pearsons_cancellable(
        observed_over_expected,
        dimension,
        valid_bins,
        worker_count,
        &|| false,
    )
}

/// Java-compatible Pearson computation with cooperative cancellation.
///
/// The callback is checked before and during every quadratic phase so a newer
/// viewport or MatrixType request can supersede expensive work without waiting
/// for the full dense matrix to finish.
pub fn compute_java_pearsons_cancellable<C>(
    mut observed_over_expected: Vec<f64>,
    dimension: usize,
    valid_bins: &[bool],
    worker_count: usize,
    cancelled: &C,
) -> Result<PearsonMatrix, PearsonError>
where
    C: Fn() -> bool + Sync,
{
    if dimension == 0 {
        return Err(PearsonError::Empty);
    }
    let cells = dimension
        .checked_mul(dimension)
        .ok_or(PearsonError::DimensionOverflow)?;
    if cells > MAX_PEARSON_CELLS {
        return Err(PearsonError::CellLimitExceeded {
            cells,
            limit: MAX_PEARSON_CELLS,
        });
    }
    if observed_over_expected.len() != cells {
        return Err(PearsonError::InvalidMatrixLength {
            expected: cells,
            actual: observed_over_expected.len(),
        });
    }
    if valid_bins.len() != dimension {
        return Err(PearsonError::InvalidValidityLength {
            expected: dimension,
            actual: valid_bins.len(),
        });
    }

    let mut row_means = vec![0.0_f64; dimension];
    for row in 0..dimension {
        if cancelled() {
            return Err(PearsonError::Cancelled);
        }
        if !valid_bins[row] {
            continue;
        }
        let values = &observed_over_expected[row * dimension..(row + 1) * dimension];
        let mut sum = 0.0;
        let mut count = 0_usize;
        for &value in values {
            if !value.is_nan() {
                sum += value;
                count += 1;
            }
        }
        if count != 0 {
            row_means[row] = sum / count as f64;
        }
    }
    for row in 0..dimension {
        if cancelled() {
            return Err(PearsonError::Cancelled);
        }
        if valid_bins[row] {
            for column in 0..dimension {
                observed_over_expected[row * dimension + column] -= row_means[column];
            }
        }
    }

    let mut values = vec![f32::NAN; cells];
    let workers = worker_count.max(1).min(dimension);
    let rows_per_worker = dimension.div_ceil(workers);
    std::thread::scope(|scope| {
        for (chunk_index, rows) in values.chunks_mut(rows_per_worker * dimension).enumerate() {
            let first_row = chunk_index * rows_per_worker;
            let centered = &observed_over_expected;
            scope.spawn(move || {
                let row_count = rows.len() / dimension;
                for local_row in 0..row_count {
                    let row = first_row + local_row;
                    if !valid_bins[row] {
                        continue;
                    }
                    rows[local_row * dimension + row] = 1.0;
                    for column in row + 1..dimension {
                        if cancelled() {
                            return;
                        }
                        if valid_bins[column] {
                            rows[local_row * dimension + column] = java_correlation(
                                &centered[row * dimension..(row + 1) * dimension],
                                &centered[column * dimension..(column + 1) * dimension],
                            )
                                as f32;
                        }
                    }
                }
            });
        }
    });
    if cancelled() {
        return Err(PearsonError::Cancelled);
    }
    for row in 0..dimension {
        if cancelled() {
            return Err(PearsonError::Cancelled);
        }
        for column in row + 1..dimension {
            values[column * dimension + row] = values[row * dimension + column];
        }
    }
    Ok(PearsonMatrix { dimension, values })
}

fn java_correlation(left: &[f64], right: &[f64]) -> f64 {
    let mut sum_left = 0.0;
    let mut sum_right = 0.0;
    for index in 0..left.len() {
        sum_left += left[index];
        sum_right += right[index];
    }
    let mean_left = sum_left / left.len() as f64;
    let mean_right = sum_right / left.len() as f64;
    let mut dot_product = 0.0;
    let mut norm_left = 0.0;
    let mut norm_right = 0.0;
    for index in 0..left.len() {
        let normalized_left = left[index] - mean_left;
        let normalized_right = right[index] - mean_right;
        dot_product += normalized_left * normalized_right;
        norm_left += normalized_left * normalized_left;
        norm_right += normalized_right * normalized_right;
    }
    dot_product / (norm_left * norm_right).sqrt()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub dataset: u64,
    pub matrix_type: u32,
    pub normalization: u32,
    pub assembly_version: u64,
    pub resolution: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone)]
pub struct IntensityTile {
    pub key: TileKey,
    pub width: u32,
    pub height: u32,
    pub values: Vec<f32>,
}

impl IntensityTile {
    pub fn new(key: TileKey, width: u32, height: u32, values: Vec<f32>) -> Self {
        assert_eq!(values.len(), width as usize * height as usize);
        Self {
            key,
            width,
            height,
            values,
        }
    }

    pub fn demo(size: u32) -> Self {
        let mut values = Vec::with_capacity(size as usize * size as usize);
        for y in 0..size {
            for x in 0..size {
                let diagonal = 1.0 / (1.0 + (x as f32 - y as f32).abs() * 0.045);
                let bands = ((x as f32 * 0.073).sin() * (y as f32 * 0.051).cos()).abs();
                values.push((diagonal * 0.78 + bands * 0.22).powf(1.6));
            }
        }
        Self::new(
            TileKey {
                dataset: 0,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 1,
                x: 0,
                y: 0,
            },
            size,
            size,
            values,
        )
    }

    pub fn from_contacts<I>(
        key: TileKey,
        source_bin_count_x: u32,
        source_bin_count_y: u32,
        output_size: u32,
        symmetric: bool,
        contacts: I,
    ) -> Self
    where
        I: IntoIterator<Item = (i32, i32, f32)>,
    {
        assert!(source_bin_count_x > 0 && source_bin_count_y > 0 && output_size > 0);
        let mut values = vec![0.0_f32; output_size as usize * output_size as usize];
        for (bin_x, bin_y, counts) in contacts {
            if bin_x < 0 || bin_y < 0 || !counts.is_finite() || counts <= 0.0 {
                continue;
            }
            let x = ((bin_x as u64 * output_size as u64) / source_bin_count_x as u64)
                .min(output_size as u64 - 1) as usize;
            let y = ((bin_y as u64 * output_size as u64) / source_bin_count_y as u64)
                .min(output_size as u64 - 1) as usize;
            values[y * output_size as usize + x] += counts;
            if symmetric && x != y {
                values[x * output_size as usize + y] += counts;
            }
        }
        for value in &mut values {
            *value = value.ln_1p();
        }
        Self::new(key, output_size, output_size, values)
    }

    pub fn positive_percentile(&self, percentile: f32) -> f32 {
        let mut values: Vec<f32> = self
            .values
            .iter()
            .copied()
            .filter(|value| value.is_finite() && *value > 0.0)
            .collect();
        if values.is_empty() {
            return 1.0;
        }
        values.sort_unstable_by(f32::total_cmp);
        let index = ((values.len() - 1) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
        values[index].max(f32::EPSILON)
    }

    pub fn from_contact_window<I>(
        key: TileKey,
        bin_bounds: [i32; 4],
        output_size: u32,
        symmetric: bool,
        contacts: I,
    ) -> Self
    where
        I: IntoIterator<Item = (i32, i32, f32)>,
    {
        assert!(output_size > 0);
        let mut values = vec![0.0_f32; output_size as usize * output_size as usize];
        Self::accumulate_contact_window(&mut values, bin_bounds, output_size, symmetric, contacts);
        Self::log_transform_values(&mut values);
        Self::new(key, output_size, output_size, values)
    }

    /// Adds contacts to an untransformed square raster.  Keeping the
    /// accumulation separate from the `ln(1 + value)` display transform lets
    /// a caller publish a partially populated raster without losing later
    /// additions to pixels which are shared by several source blocks.
    pub fn accumulate_contact_window<I>(
        values: &mut [f32],
        bin_bounds: [i32; 4],
        output_size: u32,
        symmetric: bool,
        contacts: I,
    ) where
        I: IntoIterator<Item = (i32, i32, f32)>,
    {
        assert!(output_size > 0);
        assert_eq!(values.len(), output_size as usize * output_size as usize);
        let _ = Self::accumulate_contact_window_dirty(
            values,
            bin_bounds,
            output_size,
            symmetric,
            contacts,
        );
    }

    /// Adds contacts and returns the inclusive-exclusive output-pixel bounds
    /// changed by this batch as `[x, y, width, height]`.
    pub fn accumulate_contact_window_dirty<I>(
        values: &mut [f32],
        bin_bounds: [i32; 4],
        output_size: u32,
        symmetric: bool,
        contacts: I,
    ) -> Option<[u32; 4]>
    where
        I: IntoIterator<Item = (i32, i32, f32)>,
    {
        assert!(output_size > 0);
        assert_eq!(values.len(), output_size as usize * output_size as usize);
        let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as u64;
        let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as u64;
        let mut dirty: Option<[u32; 4]> = None;
        let mut add = |bin_x: i32, bin_y: i32, counts: f32| {
            if bin_x < bin_bounds[0]
                || bin_x > bin_bounds[2]
                || bin_y < bin_bounds[1]
                || bin_y > bin_bounds[3]
                || !counts.is_finite()
                || counts <= 0.0
            {
                return;
            }
            let x = (((bin_x - bin_bounds[0]) as u64 * output_size as u64) / width_bins)
                .min(output_size as u64 - 1) as usize;
            let y = (((bin_y - bin_bounds[1]) as u64 * output_size as u64) / height_bins)
                .min(output_size as u64 - 1) as usize;
            values[y * output_size as usize + x] += counts;
            dirty = Some(match dirty {
                Some([left, top, width, height]) => {
                    let right = left + width;
                    let bottom = top + height;
                    let new_left = left.min(x as u32);
                    let new_top = top.min(y as u32);
                    let new_right = right.max(x as u32 + 1);
                    let new_bottom = bottom.max(y as u32 + 1);
                    [
                        new_left,
                        new_top,
                        new_right - new_left,
                        new_bottom - new_top,
                    ]
                }
                None => [x as u32, y as u32, 1, 1],
            });
        };
        for (bin_x, bin_y, counts) in contacts {
            add(bin_x, bin_y, counts);
            if symmetric && bin_x != bin_y {
                add(bin_y, bin_x, counts);
            }
        }
        dirty
    }

    /// Converts an accumulated contact raster into the display intensity used
    /// by the heatmap shader.
    pub fn log_transform_values(values: &mut [f32]) {
        for value in values {
            *value = value.ln_1p();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub offset: [f32; 2],
    pub scale: f32,
    pub color_min: f32,
    pub color_max: f32,
    pub generation: u64,
}

/// A square genomic viewport expressed in base-pair coordinates.
///
/// The same span is used for both axes so a contact matrix remains square on
/// screen. The center is clamped to the matrix bounds after every operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenomeViewport {
    pub center_bp: [f64; 2],
    pub span_bp: f64,
    /// Independent matrix-axis bounds. Intrachromosomal and assembly views
    /// keep these equal; interchromosomal saved states may not.
    pub axis_lengths_bp: [f64; 2],
    /// Longest axis, retained for whole-matrix LOD and memory-budget logic.
    pub genome_length_bp: f64,
    pub minimum_span_bp: f64,
    pub generation: u64,
}

impl GenomeViewport {
    pub fn new(genome_length_bp: u64, initial_span_fraction: f64) -> Self {
        let genome_length_bp = genome_length_bp.max(1) as f64;
        let span_bp = (genome_length_bp * initial_span_fraction.clamp(0.01, 1.0))
            .clamp(1.0, genome_length_bp);
        Self {
            center_bp: [genome_length_bp * 0.5, genome_length_bp * 0.5],
            span_bp,
            axis_lengths_bp: [genome_length_bp, genome_length_bp],
            genome_length_bp,
            minimum_span_bp: genome_length_bp.min(1_000_000.0),
            generation: 0,
        }
    }

    pub fn bounds_bp(self) -> [f64; 4] {
        let half = self.span_bp * 0.5;
        [
            self.center_bp[0] - half,
            self.center_bp[1] - half,
            self.center_bp[0] + half,
            self.center_bp[1] + half,
        ]
    }

    pub fn normalized_rect(self) -> [f32; 4] {
        let bounds = self.bounds_bp();
        [
            (bounds[0] / self.axis_lengths_bp[0]) as f32,
            (bounds[1] / self.axis_lengths_bp[1]) as f32,
            (self.span_bp / self.axis_lengths_bp[0]) as f32,
            (self.span_bp / self.axis_lengths_bp[1]) as f32,
        ]
    }

    pub fn set_axis_lengths(&mut self, axis_lengths_bp: [u64; 2]) {
        self.axis_lengths_bp = [
            axis_lengths_bp[0].max(1) as f64,
            axis_lengths_bp[1].max(1) as f64,
        ];
        self.genome_length_bp = self.axis_lengths_bp[0].max(self.axis_lengths_bp[1]);
        self.span_bp = self
            .span_bp
            .min(self.axis_lengths_bp[0].min(self.axis_lengths_bp[1]));
        self.minimum_span_bp = self.genome_length_bp.min(1_000_000.0);
        self.clamp_center();
    }

    pub fn pan_fraction(&mut self, delta: [f64; 2]) {
        self.center_bp[0] += delta[0] * self.span_bp;
        self.center_bp[1] += delta[1] * self.span_bp;
        self.clamp_center();
        self.generation = self.generation.wrapping_add(1);
    }

    /// Zooms by `factor` while preserving the genome coordinate beneath the
    /// normalized square-viewport anchor. A factor greater than one zooms in.
    pub fn zoom_at(&mut self, factor: f64, anchor: [f64; 2]) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor = [anchor[0].clamp(0.0, 1.0), anchor[1].clamp(0.0, 1.0)];
        let old_span = self.span_bp;
        let anchor_bp = [
            self.center_bp[0] + (anchor[0] - 0.5) * old_span,
            self.center_bp[1] + (anchor[1] - 0.5) * old_span,
        ];
        let maximum_span = self.axis_lengths_bp[0].min(self.axis_lengths_bp[1]);
        self.span_bp = (old_span / factor).clamp(
            self.minimum_span_bp.max(1.0).min(maximum_span),
            maximum_span,
        );
        self.center_bp = [
            anchor_bp[0] - (anchor[0] - 0.5) * self.span_bp,
            anchor_bp[1] - (anchor[1] - 0.5) * self.span_bp,
        ];
        self.clamp_center();
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn reset(&mut self, initial_span_fraction: f64) {
        let generation = self.generation.wrapping_add(1);
        let minimum_span_bp = self.minimum_span_bp;
        let axis_lengths_bp = self.axis_lengths_bp;
        *self = Self::new(self.genome_length_bp as u64, initial_span_fraction);
        self.axis_lengths_bp = axis_lengths_bp;
        self.span_bp = self
            .span_bp
            .min(self.axis_lengths_bp[0].min(self.axis_lengths_bp[1]));
        self.minimum_span_bp = minimum_span_bp;
        self.clamp_center();
        self.generation = generation;
    }

    pub fn clamp_center(&mut self) {
        let half = self.span_bp * 0.5;
        let maximum_x = (self.axis_lengths_bp[0] - half).max(half);
        let maximum_y = (self.axis_lengths_bp[1] - half).max(half);
        self.center_bp[0] = self.center_bp[0].clamp(half, maximum_x);
        self.center_bp[1] = self.center_bp[1].clamp(half, maximum_y);
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: [0.0, 0.0],
            scale: 1.0,
            color_min: 0.0,
            color_max: 1.0,
            generation: 0,
        }
    }
}

impl Viewport {
    pub fn pan(&mut self, delta: [f32; 2]) {
        self.offset[0] += delta[0];
        self.offset[1] += delta[1];
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn zoom(&mut self, factor: f32) {
        self.scale = (self.scale * factor).clamp(0.1, 32.0);
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_changes_advance_generation() {
        let mut viewport = Viewport::default();
        viewport.pan([1.0, 2.0]);
        viewport.zoom(2.0);
        assert_eq!(viewport.generation, 2);
        assert_eq!(viewport.offset, [1.0, 2.0]);
        assert_eq!(viewport.scale, 2.0);
    }

    #[test]
    fn rasterizes_and_mirrors_contacts() {
        let tile = IntensityTile::from_contacts(
            TileKey {
                dataset: 1,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 10,
                x: 0,
                y: 0,
            },
            10,
            10,
            10,
            true,
            [(2, 7, 3.0)],
        );
        assert_eq!(tile.values[7 * 10 + 2], 4.0_f32.ln());
        assert_eq!(tile.values[2 * 10 + 7], 4.0_f32.ln());
    }

    #[test]
    fn genome_viewport_pans_clamps_and_zooms_around_anchor() {
        let mut viewport = GenomeViewport::new(1_000, 0.5);
        viewport.minimum_span_bp = 10.0;
        viewport.pan_fraction([1.0, -1.0]);
        assert_eq!(viewport.center_bp, [750.0, 250.0]);
        viewport.zoom_at(2.0, [1.0, 0.0]);
        assert_eq!(viewport.span_bp, 250.0);
        assert_eq!(viewport.center_bp, [875.0, 125.0]);
        assert_eq!(viewport.bounds_bp(), [750.0, 0.0, 1000.0, 250.0]);
    }

    #[test]
    fn genome_viewport_clamps_each_interchromosomal_axis_independently() {
        let mut viewport = GenomeViewport::new(1_000, 0.4);
        viewport.set_axis_lengths([1_000, 600]);
        viewport.span_bp = 200.0;
        viewport.center_bp = [950.0, 550.0];
        viewport.clamp_center();
        assert_eq!(viewport.center_bp, [900.0, 500.0]);

        viewport.pan_fraction([1.0, 1.0]);
        assert_eq!(viewport.center_bp, [900.0, 500.0]);
        assert_eq!(viewport.bounds_bp(), [800.0, 400.0, 1_000.0, 600.0]);
    }

    #[test]
    fn rasterizes_only_contacts_inside_window() {
        let tile = IntensityTile::from_contact_window(
            TileKey {
                dataset: 1,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 1,
                x: 0,
                y: 0,
            },
            [100, 200, 199, 299],
            10,
            false,
            [(100, 200, 3.0), (199, 299, 7.0), (99, 200, 100.0)],
        );
        assert_eq!(tile.values[0], 4.0_f32.ln());
        assert_eq!(tile.values[99], 8.0_f32.ln());
        assert_eq!(tile.values.iter().filter(|value| **value > 0.0).count(), 2);
    }

    #[test]
    fn streaming_accumulation_matches_single_pass_window_rasterization() {
        let key = TileKey {
            dataset: 1,
            matrix_type: 0,
            normalization: 0,
            assembly_version: 0,
            resolution: 1,
            x: 0,
            y: 0,
        };
        let all = IntensityTile::from_contact_window(
            key,
            [0, 0, 9, 9],
            10,
            true,
            [(1, 2, 3.0), (1, 2, 5.0), (7, 6, 11.0)],
        );
        let mut streamed = vec![0.0; 100];
        IntensityTile::accumulate_contact_window(
            &mut streamed,
            [0, 0, 9, 9],
            10,
            true,
            [(1, 2, 3.0)],
        );
        IntensityTile::accumulate_contact_window(
            &mut streamed,
            [0, 0, 9, 9],
            10,
            true,
            [(1, 2, 5.0), (7, 6, 11.0)],
        );
        IntensityTile::log_transform_values(&mut streamed);
        assert_eq!(streamed, all.values);
    }

    #[test]
    fn dirty_bounds_cover_only_changed_pixels() {
        let mut values = vec![0.0; 100];
        let dirty = IntensityTile::accumulate_contact_window_dirty(
            &mut values,
            [0, 0, 9, 9],
            10,
            false,
            [(2, 3, 1.0), (5, 7, 2.0)],
        );
        assert_eq!(dirty, Some([2, 3, 4, 5]));
        assert_eq!(values[3 * 10 + 2], 1.0);
        assert_eq!(values[7 * 10 + 5], 2.0);
    }

    #[test]
    fn symmetric_dirty_bounds_include_transposed_pixels() {
        let mut values = vec![0.0; 100];
        let dirty = IntensityTile::accumulate_contact_window_dirty(
            &mut values,
            [0, 0, 9, 9],
            10,
            true,
            [(1, 8, 1.0)],
        );
        assert_eq!(dirty, Some([1, 1, 8, 8]));
        assert_eq!(values[8 * 10 + 1], 1.0);
        assert_eq!(values[10 + 8], 1.0);
    }

    #[test]
    fn java_pearsons_marks_invalid_bins_and_keeps_valid_diagonal() {
        let matrix = compute_java_pearsons(
            vec![2.0, 0.0, 4.0, 0.0, 0.0, 0.0, 4.0, 0.0, 8.0],
            3,
            &[true, false, true],
            2,
        )
        .unwrap();
        assert_eq!(matrix.get(0, 0), Some(1.0));
        assert_eq!(matrix.get(2, 2), Some(1.0));
        assert!(matrix.get(1, 1).unwrap().is_nan());
        assert!(matrix.get(0, 1).unwrap().is_nan());
        assert!(
            matrix.get(0, 2).unwrap().is_nan() && matrix.get(2, 0).unwrap().is_nan(),
            "proportional rows have zero variance after Java centering"
        );
    }

    #[test]
    fn java_pearsons_preserves_column_mean_subtraction_quirk() {
        let matrix = compute_java_pearsons(
            vec![1.0, 2.0, 4.0, 3.0, 8.0, 5.0, 7.0, 6.0, 9.0],
            3,
            &[true; 3],
            1,
        )
        .unwrap();
        assert!((matrix.get(0, 1).unwrap() - 0.114_707_865).abs() < 1.0e-6);
        assert!((matrix.get(0, 2).unwrap() - 0.970_725_36).abs() < 1.0e-6);
    }

    #[test]
    fn java_pearsons_rejects_unbounded_or_malformed_dense_inputs() {
        assert!(matches!(
            compute_java_pearsons(vec![0.0; 3], 2, &[true; 2], 1),
            Err(PearsonError::InvalidMatrixLength {
                expected: 4,
                actual: 3,
            })
        ));
        let dimension = (MAX_PEARSON_CELLS as f64).sqrt() as usize + 1;
        assert!(matches!(
            compute_java_pearsons(Vec::new(), dimension, &[], 1),
            Err(PearsonError::CellLimitExceeded { .. })
        ));
    }

    #[test]
    fn java_pearsons_cooperatively_cancels_quadratic_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let checks = AtomicUsize::new(0);
        let result =
            compute_java_pearsons_cancellable(vec![1.0; 128 * 128], 128, &[true; 128], 4, &|| {
                checks.fetch_add(1, Ordering::Relaxed) >= 8
            });
        assert!(matches!(result, Err(PearsonError::Cancelled)));
    }
}
