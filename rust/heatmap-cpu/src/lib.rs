use heatmap_core::IntensityTile;
use image::{ImageBuffer, Rgb, RgbImage};

pub fn render_rgb(tile: &IntensityTile, color_min: f32, color_max: f32) -> RgbImage {
    let mut image: RgbImage = ImageBuffer::new(tile.width, tile.height);
    let range = (color_max - color_min).max(f32::EPSILON);
    for (pixel, intensity) in image.pixels_mut().zip(tile.values.iter().copied()) {
        let normalized = ((intensity - color_min) / range).clamp(0.0, 1.0);
        *pixel = Rgb(heat_color(normalized));
    }
    image
}

fn heat_color(value: f32) -> [u8; 3] {
    let low = [0.035, 0.055, 0.10];
    let middle = [0.88, 0.16, 0.08];
    let high = [1.0, 0.93, 0.46];
    let (start, end, amount) = if value < 0.5 {
        (low, middle, value * 2.0)
    } else {
        (middle, high, (value - 0.5) * 2.0)
    };
    std::array::from_fn(|index| {
        ((start[index] + (end[index] - start[index]) * amount) * 255.0).round() as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use heatmap_core::{IntensityTile, TileKey};

    #[test]
    fn maps_lower_and_upper_range_to_palette_endpoints() {
        let tile = IntensityTile::new(
            TileKey {
                dataset: 0,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 1,
                x: 0,
                y: 0,
            },
            2,
            1,
            vec![0.0, 1.0],
        );
        let image = render_rgb(&tile, 0.0, 1.0);
        assert_eq!(image.get_pixel(0, 0).0, [9, 14, 26]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 237, 117]);
    }
}
