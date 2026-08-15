use std::collections::HashMap;

pub type ContactMap = HashMap<(i32, i32), f32>;

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
        if observed_count == 0.0 || control_count == 0.0 {
            continue;
        }
        let score = (observed_count / observed_average) / (control_count / control_average);
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
}
