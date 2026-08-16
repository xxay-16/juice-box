use crate::tile_engine::MatrixType;

pub const PICKER_WIDTH: u32 = 768;
pub const PICKER_HEIGHT: u32 = 520;
const ROWS: usize = 20;
const ROW_HEIGHT: usize = 21;
const TEXT_SCALE: usize = 2;

pub fn selectable_modes(control_available: bool, intrachromosomal: bool) -> Vec<MatrixType> {
    MatrixType::selectable_modes(control_available)
        .iter()
        .copied()
        .filter(|mode| mode.available_for_axes(intrachromosomal))
        .collect()
}

#[derive(Debug, Clone)]
pub struct ModePicker {
    modes: Vec<MatrixType>,
    selected: usize,
}

impl ModePicker {
    pub fn new(modes: Vec<MatrixType>, current: MatrixType) -> Self {
        let selected = modes.iter().position(|mode| *mode == current).unwrap_or(0);
        Self { modes, selected }
    }

    pub fn selected(&self) -> Option<MatrixType> {
        self.modes.get(self.selected).copied()
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.modes.is_empty() {
            return;
        }
        self.selected = (self.selected as i32 + delta)
            .clamp(0, self.modes.len().saturating_sub(1) as i32) as usize;
    }

    pub fn home(&mut self) {
        self.selected = 0;
    }

    pub fn end(&mut self) {
        self.selected = self.modes.len().saturating_sub(1);
    }

    pub fn page(&mut self, delta: i32) {
        self.move_by(delta.saturating_mul(ROWS as i32));
    }

    pub fn render_rgba(&self) -> Vec<u8> {
        let width = PICKER_WIDTH as usize;
        let height = PICKER_HEIGHT as usize;
        let mut pixels = vec![0_u8; width * height * 4];
        fill_rect(&mut pixels, width, 0, 0, width, height, [9, 14, 25, 238]);
        fill_rect(&mut pixels, width, 0, 0, width, 42, [20, 31, 52, 250]);
        draw_text(
            &mut pixels,
            width,
            20,
            14,
            "MATRIX VIEW",
            [232, 241, 255, 255],
        );

        let start = self
            .selected
            .saturating_sub(ROWS / 2)
            .min(self.modes.len().saturating_sub(ROWS));
        let end = (start + ROWS).min(self.modes.len());
        for (visible_index, mode_index) in (start..end).enumerate() {
            let mode = self.modes[mode_index];
            let y = 48 + visible_index * ROW_HEIGHT;
            if mode_index == self.selected {
                fill_rect(
                    &mut pixels,
                    width,
                    10,
                    y.saturating_sub(2),
                    width - 20,
                    ROW_HEIGHT,
                    [26, 112, 179, 245],
                );
            }
            let marker = if mode_index == self.selected {
                ">"
            } else {
                " "
            };
            let line = format!(
                "{marker} {:02} {:<14} {}",
                mode_index + 1,
                mode.java_name(),
                mode.label()
            );
            draw_text(
                &mut pixels,
                width,
                18,
                y,
                &truncate_ascii(&line, 58),
                [240, 245, 252, 255],
            );
        }

        let footer_y = height - 34;
        fill_rect(
            &mut pixels,
            width,
            0,
            footer_y - 8,
            width,
            42,
            [16, 25, 42, 252],
        );
        draw_text(
            &mut pixels,
            width,
            20,
            footer_y,
            "UP/DOWN MOVE  PGUP/PGDN PAGE  ENTER APPLY  ESC CANCEL",
            [183, 205, 234, 255],
        );
        pixels
    }
}

fn truncate_ascii(value: &str, maximum: usize) -> String {
    let mut output = value.chars().take(maximum).collect::<String>();
    if value.chars().count() > maximum {
        output.pop();
        output.push('>');
    }
    output
}

fn fill_rect(
    pixels: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: [u8; 4],
) {
    let maximum_y = (y + height).min(pixels.len() / 4 / stride);
    let maximum_x = (x + width).min(stride);
    for row in y..maximum_y {
        for column in x..maximum_x {
            let offset = (row * stride + column) * 4;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

fn draw_text(pixels: &mut [u8], stride: usize, x: usize, y: usize, text: &str, color: [u8; 4]) {
    let mut cursor = x;
    for character in text.chars() {
        draw_glyph(pixels, stride, cursor, y, character, color);
        cursor += 6 * TEXT_SCALE;
        if cursor + 5 * TEXT_SCALE >= stride {
            break;
        }
    }
}

fn draw_glyph(
    pixels: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    character: char,
    color: [u8; 4],
) {
    let rows = glyph(character.to_ascii_uppercase());
    for (row, bits) in rows.into_iter().enumerate() {
        for column in 0..5 {
            if bits & (1 << (4 - column)) == 0 {
                continue;
            }
            fill_rect(
                pixels,
                stride,
                x + column * TEXT_SCALE,
                y + row * TEXT_SCALE,
                TEXT_SCALE,
                TEXT_SCALE,
                color,
            );
        }
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [31, 4, 4, 4, 4, 4, 31],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ' ' => [0; 7],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '/' => [1, 1, 2, 4, 8, 16, 16],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        '*' => [0, 21, 14, 31, 14, 21, 0],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '[' => [14, 8, 8, 8, 8, 8, 14],
        ']' => [14, 2, 2, 2, 2, 2, 14],
        '=' => [0, 31, 0, 31, 0, 0, 0],
        '^' => [4, 10, 17, 0, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        '>' => [16, 8, 4, 2, 4, 8, 16],
        '<' => [1, 2, 4, 8, 4, 2, 1],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn navigation_stays_in_bounds_and_preserves_current_mode() {
        let modes = MatrixType::selectable_modes(false).to_vec();
        let mut picker = ModePicker::new(modes.clone(), MatrixType::Pearson);
        assert_eq!(picker.selected(), Some(MatrixType::Pearson));
        picker.move_by(1000);
        assert_eq!(picker.selected(), modes.last().copied());
        picker.home();
        assert_eq!(picker.selected(), modes.first().copied());
        picker.end();
        picker.page(-1);
        assert_eq!(picker.selected(), modes.first().copied());
    }

    #[test]
    fn overlay_is_rgba_and_contains_opaque_pixels() {
        let picker = ModePicker::new(
            MatrixType::selectable_modes(true).to_vec(),
            MatrixType::Ratio,
        );
        let rgba = picker.render_rgba();
        assert_eq!(rgba.len(), (PICKER_WIDTH * PICKER_HEIGHT * 4) as usize);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn selectable_mode_lists_are_unique_and_round_trip_java_names() {
        for control_available in [false, true] {
            let modes = selectable_modes(control_available, true);
            let unique: HashSet<_> = modes.iter().copied().collect();
            assert_eq!(unique.len(), modes.len());
            for mode in modes {
                assert_eq!(MatrixType::from_java_name(mode.java_name()), Some(mode));
            }
        }
    }

    #[test]
    fn control_picker_contains_every_production_gated_mode() {
        let modes: HashSet<_> = selectable_modes(true, true).into_iter().collect();
        for mode in MatrixType::PRODUCTION_RAW_PIXEL_MODES
            .into_iter()
            .chain(MatrixType::NORM_SQUARED_MODES)
        {
            assert!(modes.contains(&mode), "missing {}", mode.java_name());
        }
        assert!(!modes.contains(&MatrixType::ObservedMinusExpected));
        assert!(!modes.contains(&MatrixType::ControlMinusExpected));
    }

    #[test]
    fn no_control_picker_never_contains_control_or_comparison_modes() {
        for mode in selectable_modes(false, true) {
            assert!(!mode.uses_control(), "{} uses control", mode.java_name());
            assert!(
                !mode.is_comparison(),
                "{} is a comparison",
                mode.java_name()
            );
        }
    }

    #[test]
    fn cross_chromosome_picker_removes_intrachromosomal_modes() {
        let modes: HashSet<_> = selectable_modes(true, false).into_iter().collect();
        for unavailable in [
            MatrixType::Expected,
            MatrixType::ObservedOverExpected,
            MatrixType::ControlOverExpected,
            MatrixType::Pearson,
            MatrixType::ControlPearson,
            MatrixType::PearsonVs,
            MatrixType::Vs,
            MatrixType::ObservedOverExpectedVs,
            MatrixType::NormSquaredVs,
        ] {
            assert!(
                !modes.contains(&unavailable),
                "{} leaked",
                unavailable.java_name()
            );
        }
        assert!(modes.contains(&MatrixType::Observed));
        assert!(modes.contains(&MatrixType::Ratio));
        assert!(modes.contains(&MatrixType::ObservedExpectedRatio));
    }
}
