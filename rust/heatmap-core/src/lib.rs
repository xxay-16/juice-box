//! Framework-neutral heatmap viewport and scalar-tile types.

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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub offset: [f32; 2],
    pub scale: f32,
    pub color_min: f32,
    pub color_max: f32,
    pub generation: u64,
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
}
