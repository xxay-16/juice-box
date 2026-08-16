use std::collections::HashMap;

pub type ContactMap = HashMap<(i32, i32), f32>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedComparison {
    Ratio,
    Difference,
}

pub fn combine_triangles(observed: &[f32], control: &[f32]) -> Vec<f32> {
    assert_eq!(observed.len(), control.len());
    let size = (observed.len() as f64).sqrt() as usize;
    assert_eq!(size * size, observed.len());
    let mut values = vec![0.0; observed.len()];
    for y in 0..size {
        for x in 0..size {
            values[y * size + x] = if y >= x {
                observed[y * size + x]
            } else {
                control[y * size + x]
            };
        }
    }
    values
}

pub fn scale_for_vs(values: &[f32], own_average: f32, other_average: f32) -> Vec<f32> {
    let shared_average = own_average * 0.5 + other_average * 0.5;
    values
        .iter()
        .map(|&value| {
            let score = value / own_average * shared_average;
            if score.is_finite() { score } else { 0.0 }
        })
        .collect()
}

/// Matches HeatmapRenderer.renderSimpleLogVSMap.  Java evaluates the scale
/// expression in float, widens that rounded value for Math.log, then casts the
/// result back to float.
pub fn scale_for_log_vs(values: &[f32], own_average: f32, other_average: f32) -> Vec<f32> {
    let shared_average = (own_average + other_average) / 2.0;
    values
        .iter()
        .map(|&value| {
            let argument = shared_average * (value / own_average) + 1.0;
            let score = f64::from(argument).ln() as f32;
            if score.is_finite() { score } else { 0.0 }
        })
        .collect()
}

pub fn observed_over_expected_score(count: f32, expected: f32, pseudo_count: f32) -> f32 {
    let score = (count + pseudo_count) / (expected + pseudo_count);
    if score.is_finite() { score } else { 0.0 }
}

pub fn rasterize_ratio_contacts(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_average: f32,
    control_average: f32,
) -> Vec<f32> {
    rasterize_ratio_with_baselines_contacts(
        observed,
        control,
        bin_bounds,
        output_size,
        symmetric,
        observed_average,
        control_average,
        0.0,
        0.0,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "Java ratio modes expose independent observed/control baselines and pseudocounts"
)]
pub fn rasterize_ratio_with_baselines_contacts(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_baseline: f32,
    control_baseline: f32,
    observed_pseudo_count: f32,
    control_pseudo_count: f32,
) -> Vec<f32> {
    assert!(output_size > 0);
    let output_size_usize = output_size as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as u64;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as u64;
    let mut values = vec![0.0; output_size_usize * output_size_usize];
    let mut samples = vec![0_u32; output_size_usize * output_size_usize];
    let mut add = |bin_x: i32, bin_y: i32, score: f32| {
        if bin_x < bin_bounds[0]
            || bin_x > bin_bounds[2]
            || bin_y < bin_bounds[1]
            || bin_y > bin_bounds[3]
            || !score.is_finite()
            || score <= 0.0
        {
            return;
        }
        let x = (((bin_x - bin_bounds[0]) as u64 * u64::from(output_size)) / width_bins)
            .min(u64::from(output_size) - 1) as usize;
        let y = (((bin_y - bin_bounds[1]) as u64 * u64::from(output_size)) / height_bins)
            .min(u64::from(output_size) - 1) as usize;
        values[y * output_size_usize + x] += score;
        samples[y * output_size_usize + x] += 1;
    };
    for (&(bin_x, bin_y), &observed_count) in observed {
        let Some(&control_count) = control.get(&(bin_x, bin_y)) else {
            continue;
        };
        // Match HeatmapRenderer.comparisonRatioWithAverageScore and
        // renderRatioWithExpMap: every addition/division happens in float,
        // then a non-finite final quotient is omitted. Baselines are either
        // zoom averages (RATIO/RATIOP1) or expected distance zero
        // (RATIO0/RATIO0P1).
        let numerator =
            (observed_count + observed_pseudo_count) / (observed_baseline + observed_pseudo_count);
        let denominator =
            (control_count + control_pseudo_count) / (control_baseline + control_pseudo_count);
        let score = numerator / denominator;
        add(bin_x, bin_y, score);
        if symmetric && bin_x != bin_y {
            add(bin_y, bin_x, score);
        }
    }
    for (value, count) in values.iter_mut().zip(samples) {
        if count != 0 {
            *value /= count as f32;
        }
    }
    values
}

pub fn rasterize_difference_contacts(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_average: f32,
    control_average: f32,
) -> Vec<f32> {
    assert!(output_size > 0);
    let size = output_size as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as u64;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as u64;
    let shared_average = observed_average * 0.5 + control_average * 0.5;
    let mut values = vec![0.0; size * size];
    let mut samples = vec![0_u32; size * size];
    let mut add = |bin_x: i32, bin_y: i32, score: f32| {
        if bin_x < bin_bounds[0]
            || bin_x > bin_bounds[2]
            || bin_y < bin_bounds[1]
            || bin_y > bin_bounds[3]
            || !score.is_finite()
        {
            return;
        }
        let x = (((bin_x - bin_bounds[0]) as u64 * u64::from(output_size)) / width_bins)
            .min(u64::from(output_size) - 1) as usize;
        let y = (((bin_y - bin_bounds[1]) as u64 * u64::from(output_size)) / height_bins)
            .min(u64::from(output_size) - 1) as usize;
        values[y * size + x] += score;
        samples[y * size + x] += 1;
    };
    for (&(bin_x, bin_y), &observed_count) in observed {
        let Some(&control_count) = control.get(&(bin_x, bin_y)) else {
            continue;
        };
        let score =
            (observed_count / observed_average - control_count / control_average) * shared_average;
        add(bin_x, bin_y, score);
        if symmetric && bin_x != bin_y {
            add(bin_y, bin_x, score);
        }
    }
    for (value, count) in values.iter_mut().zip(samples) {
        if count != 0 {
            *value /= count as f32;
        }
    }
    values
}

pub fn rasterize_log_ratio_contacts(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_average: f32,
    control_average: f32,
) -> Vec<f32> {
    rasterize_paired_contacts(
        observed,
        control,
        bin_bounds,
        output_size,
        symmetric,
        |observed_count, control_count, _, _| {
            let observed_argument = observed_count / observed_average + 1.0;
            let control_argument = control_count / control_average + 1.0;
            let numerator = f64::from(observed_argument).ln() as f32;
            let denominator = f64::from(control_argument).ln() as f32;
            Some(numerator / denominator)
        },
        |_, _| Some((0.0, 0.0)),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "Java log expected ratio requires paired contacts, two expected sources, and viewport raster inputs"
)]
pub fn rasterize_log_expected_ratio_contacts<F, G>(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_expected: F,
    control_expected: G,
) -> Vec<f32>
where
    F: Fn(i32, i32) -> Option<f32>,
    G: Fn(i32, i32) -> Option<f32>,
{
    rasterize_paired_contacts(
        observed,
        control,
        bin_bounds,
        output_size,
        symmetric,
        |observed_count, control_count, observed_expected, control_expected| {
            let observed_count_log = f64::from(observed_count + 1.0).ln();
            let observed_expected_log = f64::from(observed_expected + 1.0).ln();
            let control_count_log = f64::from(control_count + 1.0).ln();
            let control_expected_log = f64::from(control_expected + 1.0).ln();
            Some(
                ((observed_count_log / observed_expected_log)
                    / (control_count_log / control_expected_log)) as f32,
            )
        },
        |bin_x, bin_y| {
            Some((
                observed_expected(bin_x, bin_y)?,
                control_expected(bin_x, bin_y)?,
            ))
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "paired rasterization keeps the Java contact join, score transform, and viewport mapping explicit"
)]
fn rasterize_paired_contacts<S, E>(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    score: S,
    expected: E,
) -> Vec<f32>
where
    S: Fn(f32, f32, f32, f32) -> Option<f32>,
    E: Fn(i32, i32) -> Option<(f32, f32)>,
{
    assert!(output_size > 0);
    let size = output_size as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as u64;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as u64;
    let mut values = vec![0.0; size * size];
    let mut samples = vec![0_u32; size * size];
    let mut add = |bin_x: i32, bin_y: i32, score: f32| {
        if bin_x < bin_bounds[0]
            || bin_x > bin_bounds[2]
            || bin_y < bin_bounds[1]
            || bin_y > bin_bounds[3]
            || !score.is_finite()
        {
            return;
        }
        let x = (((bin_x - bin_bounds[0]) as u64 * u64::from(output_size)) / width_bins)
            .min(u64::from(output_size) - 1) as usize;
        let y = (((bin_y - bin_bounds[1]) as u64 * u64::from(output_size)) / height_bins)
            .min(u64::from(output_size) - 1) as usize;
        values[y * size + x] += score;
        samples[y * size + x] += 1;
    };
    for (&(bin_x, bin_y), &observed_count) in observed {
        let Some(&control_count) = control.get(&(bin_x, bin_y)) else {
            continue;
        };
        let Some((observed_expected, control_expected)) = expected(bin_x, bin_y) else {
            continue;
        };
        let Some(value) = score(
            observed_count,
            control_count,
            observed_expected,
            control_expected,
        ) else {
            continue;
        };
        add(bin_x, bin_y, value);
        if symmetric && bin_x != bin_y {
            add(bin_y, bin_x, value);
        }
    }
    for (value, count) in values.iter_mut().zip(samples) {
        if count != 0 {
            *value /= count as f32;
        }
    }
    values
}

#[allow(
    clippy::too_many_arguments,
    reason = "Java O/E comparison modes require paired contacts, two expected sources, pseudocounts, and viewport raster inputs"
)]
pub fn rasterize_expected_comparison_contacts<F, G>(
    observed: &ContactMap,
    control: &ContactMap,
    bin_bounds: [i32; 4],
    output_size: u32,
    symmetric: bool,
    observed_expected: F,
    control_expected: G,
    observed_pseudo_count: f32,
    control_pseudo_count: f32,
    operation: ExpectedComparison,
) -> Vec<f32>
where
    F: Fn(i32, i32) -> Option<f32>,
    G: Fn(i32, i32) -> Option<f32>,
{
    assert!(output_size > 0);
    let size = output_size as usize;
    let width_bins = (bin_bounds[2] - bin_bounds[0] + 1).max(1) as u64;
    let height_bins = (bin_bounds[3] - bin_bounds[1] + 1).max(1) as u64;
    let mut values = vec![0.0; size * size];
    let mut samples = vec![0_u32; size * size];
    let mut add = |bin_x: i32, bin_y: i32, score: f32| {
        if bin_x < bin_bounds[0]
            || bin_x > bin_bounds[2]
            || bin_y < bin_bounds[1]
            || bin_y > bin_bounds[3]
            || !score.is_finite()
        {
            return;
        }
        let x = (((bin_x - bin_bounds[0]) as u64 * u64::from(output_size)) / width_bins)
            .min(u64::from(output_size) - 1) as usize;
        let y = (((bin_y - bin_bounds[1]) as u64 * u64::from(output_size)) / height_bins)
            .min(u64::from(output_size) - 1) as usize;
        values[y * size + x] += score;
        samples[y * size + x] += 1;
    };
    for (&(bin_x, bin_y), &observed_count) in observed {
        let Some(&control_count) = control.get(&(bin_x, bin_y)) else {
            continue;
        };
        let Some(observed_expected) = observed_expected(bin_x, bin_y) else {
            continue;
        };
        let Some(control_expected) = control_expected(bin_x, bin_y) else {
            continue;
        };
        let observed_oe =
            (observed_count + observed_pseudo_count) / (observed_expected + observed_pseudo_count);
        let control_oe =
            (control_count + control_pseudo_count) / (control_expected + control_pseudo_count);
        let score = match operation {
            ExpectedComparison::Ratio => observed_oe / control_oe,
            ExpectedComparison::Difference => observed_oe - control_oe,
        };
        add(bin_x, bin_y, score);
        if symmetric && bin_x != bin_y {
            add(bin_y, bin_x, score);
        }
    }
    for (value, count) in values.iter_mut().zip(samples) {
        if count != 0 {
            *value /= count as f32;
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_split_supports_small_square_fixtures() {
        assert_eq!(
            combine_triangles(&[1.0, 2.0, 3.0, 4.0], &[5.0, 6.0, 7.0, 8.0]),
            vec![1.0, 6.0, 3.0, 4.0]
        );
    }

    #[test]
    fn ratio_fixture_omits_unpaired_contacts() {
        let mut observed = ContactMap::new();
        observed.insert((0, 0), 4.0);
        observed.insert((0, 1), 6.0);
        observed.insert((1, 1), 2.0);
        observed.insert((1, 2), 3.0);
        observed.insert((0, 2), 9.0);
        let mut control = ContactMap::new();
        control.insert((0, 0), 8.0);
        control.insert((0, 1), 3.0);
        control.insert((1, 1), 8.0);
        control.insert((1, 2), 6.0);
        control.insert((2, 2), 5.0);
        let ratio = rasterize_ratio_contacts(&observed, &control, [0, 0, 2, 2], 3, true, 2.0, 4.0);
        assert_eq!(ratio, [1.0, 4.0, 0.0, 4.0, 0.5, 1.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn ratio_p1_keeps_zero_counts_and_uses_pseudocount_baselines() {
        let observed = ContactMap::from([((0, 0), 0.0), ((0, 1), 3.0), ((1, 1), 9.0)]);
        let control = ContactMap::from([((0, 0), 0.0), ((0, 1), 1.0)]);
        let values = rasterize_ratio_with_baselines_contacts(
            &observed,
            &control,
            [0, 0, 1, 1],
            2,
            true,
            4.0,
            2.0,
            1.0,
            1.0,
        );
        assert_eq!(
            values.into_iter().map(f32::to_bits).collect::<Vec<_>>(),
            [0x3f19_9999, 0x3f99_9999, 0x3f99_9999, 0,]
        );
    }

    #[test]
    fn ratio_expected_zero_uses_expected_distance_zero_baselines() {
        let observed = ContactMap::from([((0, 0), 8.0)]);
        let control = ContactMap::from([((0, 0), 4.0)]);
        let values = rasterize_ratio_with_baselines_contacts(
            &observed,
            &control,
            [0, 0, 0, 0],
            1,
            true,
            16.0,
            4.0,
            0.0,
            0.0,
        );
        assert_eq!(values[0].to_bits(), 0.5_f32.to_bits());
    }

    #[test]
    fn ratio_expected_zero_p1_uses_expected_baseline_pseudocounts() {
        let observed = ContactMap::from([((0, 0), 0.0), ((1, 1), 5.0)]);
        let control = ContactMap::from([((0, 0), 0.0)]);
        let values = rasterize_ratio_with_baselines_contacts(
            &observed,
            &control,
            [0, 0, 1, 1],
            2,
            true,
            3.0,
            7.0,
            1.0,
            1.0,
        );
        assert_eq!(values, [2.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn difference_fixture_uses_only_paired_contacts_and_java_average_scaling() {
        let observed = ContactMap::from([((0, 0), 8.0), ((0, 1), 6.0), ((1, 1), 5.0)]);
        let control = ContactMap::from([((0, 0), 4.0), ((0, 1), 8.0), ((1, 2), 9.0)]);
        let values =
            rasterize_difference_contacts(&observed, &control, [0, 0, 1, 1], 2, true, 4.0, 2.0);
        assert_eq!(values, [0.0, -7.5, -7.5, 0.0]);
    }

    #[test]
    fn expected_ratio_uses_distance_specific_expected_and_paired_contacts() {
        let observed = ContactMap::from([((0, 0), 8.0), ((0, 1), 6.0), ((1, 1), 5.0)]);
        let control = ContactMap::from([((0, 0), 4.0), ((0, 1), 3.0)]);
        let values = rasterize_expected_comparison_contacts(
            &observed,
            &control,
            [0, 0, 1, 1],
            2,
            true,
            |x, y| Some(if x == y { 4.0 } else { 2.0 }),
            |x, y| Some(if x == y { 8.0 } else { 1.0 }),
            0.0,
            0.0,
            ExpectedComparison::Ratio,
        );
        assert_eq!(values, [4.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn expected_ratio_p1_preserves_java_float_pseudocount_order() {
        let observed = ContactMap::from([((0, 0), 0.0)]);
        let control = ContactMap::from([((0, 0), 0.0)]);
        let values = rasterize_expected_comparison_contacts(
            &observed,
            &control,
            [0, 0, 0, 0],
            1,
            true,
            |_, _| Some(3.0),
            |_, _| Some(7.0),
            1.0,
            1.0,
            ExpectedComparison::Ratio,
        );
        assert_eq!(values[0].to_bits(), 2.0_f32.to_bits());
    }

    #[test]
    fn expected_minus_returns_signed_oe_difference() {
        let observed = ContactMap::from([((0, 0), 9.0)]);
        let control = ContactMap::from([((0, 0), 3.0)]);
        let values = rasterize_expected_comparison_contacts(
            &observed,
            &control,
            [0, 0, 0, 0],
            1,
            true,
            |_, _| Some(3.0),
            |_, _| Some(6.0),
            0.0,
            0.0,
            ExpectedComparison::Difference,
        );
        assert_eq!(values[0].to_bits(), 2.5_f32.to_bits());
    }

    #[test]
    fn log_vs_preserves_float_expression_before_java_double_log() {
        let values = scale_for_log_vs(&[3.0], 2.0, 6.0);
        let argument = 4.0_f32 * (3.0_f32 / 2.0_f32) + 1.0_f32;
        assert_eq!(
            values[0].to_bits(),
            (f64::from(argument).ln() as f32).to_bits()
        );
    }

    #[test]
    fn log_ratio_uses_only_paired_contacts_and_float_log_operands() {
        let observed = ContactMap::from([((0, 0), 3.0), ((0, 1), 7.0)]);
        let control = ContactMap::from([((0, 0), 8.0)]);
        let values =
            rasterize_log_ratio_contacts(&observed, &control, [0, 0, 1, 1], 2, true, 2.0, 4.0);
        let numerator = f64::from(3.0_f32 / 2.0_f32 + 1.0).ln() as f32;
        let denominator = f64::from(8.0_f32 / 4.0_f32 + 1.0).ln() as f32;
        assert_eq!(values[0].to_bits(), (numerator / denominator).to_bits());
        assert_eq!(values[1], 0.0);
    }

    #[test]
    fn log_expected_ratio_uses_distance_specific_expected_and_double_intermediates() {
        let observed = ContactMap::from([((0, 1), 8.0)]);
        let control = ContactMap::from([((0, 1), 3.0)]);
        let values = rasterize_log_expected_ratio_contacts(
            &observed,
            &control,
            [0, 0, 1, 1],
            2,
            true,
            |x, y| Some(if x == y { 9.0 } else { 2.0 }),
            |x, y| Some(if x == y { 7.0 } else { 5.0 }),
        );
        let expected = ((f64::from(8.0_f32 + 1.0).ln() / f64::from(2.0_f32 + 1.0).ln())
            / (f64::from(3.0_f32 + 1.0).ln() / f64::from(5.0_f32 + 1.0).ln()))
            as f32;
        assert_eq!(values[1].to_bits(), expected.to_bits());
        assert_eq!(values[2].to_bits(), expected.to_bits());
    }
}
