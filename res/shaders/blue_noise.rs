//! Blue-noise dither matrix, ported from libplacebo src/dither.c
//! (originally from mpv, Copyright 2013 Wessel Dankers, LGPL-2.1-or-later):
//! void-and-cluster construction over a toroidal gaussian energy field.
//! Deterministic, so the build bakes the table and the viewer reads bytes.

use std::path::Path;

/// Writes the matrix as little-endian f32 texels for the quantize pass to include.
pub fn write_table(output_directory: &Path) {
    let texels: Vec<u8> = generate()
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    std::fs::write(output_directory.join("blue_noise.bin"), texels).expect("blue noise writable");
}

const SIZE_BITS: usize = 6;
pub const SIZE: usize = 1 << SIZE_BITS;
const CELL_COUNT: usize = SIZE * SIZE;

fn cell_index(x: usize, y: usize) -> usize {
    x | (y << SIZE_BITS)
}

struct Generator {
    gaussian: Vec<u64>,
    gaussian_accumulator: Vec<u64>,
    placed: Vec<bool>,
    ranks: Vec<usize>,
    tie_candidates: Vec<usize>,
    random_state: u32,
}

impl Generator {
    fn new() -> Self {
        Self {
            gaussian: vec![0; CELL_COUNT],
            gaussian_accumulator: vec![0; CELL_COUNT],
            placed: vec![false; CELL_COUNT],
            ranks: vec![0; CELL_COUNT],
            tie_candidates: Vec::new(),
            random_state: 1,
        }
    }

    fn random(&mut self) -> usize {
        self.random_state = self.random_state.wrapping_mul(214013).wrapping_add(2531011);
        ((self.random_state >> 16) & 0x7FFF) as usize
    }

    fn fill_gaussian(&mut self) {
        let radius = SIZE / 2 - 1;
        let diameter = radius * 2 + 1;
        let area = (diameter * diameter) as f64;
        let sigma = -(1.5 / u64::MAX as f64 * area).ln() / radius as f64;
        for grid_y in 0..=radius {
            for grid_x in 0..=grid_y {
                let offset_x = grid_x as f64 - radius as f64;
                let offset_y = grid_y as f64 - radius as f64;
                let distance = (offset_x * offset_x + offset_y * offset_y).sqrt();
                let value = ((-distance * sigma).exp() / area * u64::MAX as f64) as u64;
                let last = diameter - 1;
                for (x, y) in [
                    (grid_x, grid_y),
                    (grid_y, grid_x),
                    (grid_x, last - grid_y),
                    (grid_y, last - grid_x),
                    (last - grid_x, grid_y),
                    (last - grid_y, grid_x),
                    (last - grid_x, last - grid_y),
                    (last - grid_y, last - grid_x),
                ] {
                    self.gaussian[cell_index(x, y)] = value;
                }
            }
        }
    }

    fn place(&mut self, cell: usize) {
        if self.placed[cell] {
            return;
        }
        self.placed[cell] = true;
        let middle = cell_index(SIZE / 2 - 1, SIZE / 2 - 1);
        let offset = (middle + CELL_COUNT - cell) & (CELL_COUNT - 1);
        let split = CELL_COUNT - offset;
        for index in 0..split {
            self.gaussian_accumulator[index] =
                self.gaussian_accumulator[index].wrapping_add(self.gaussian[offset + index]);
        }
        for index in 0..offset {
            self.gaussian_accumulator[split + index] =
                self.gaussian_accumulator[split + index].wrapping_add(self.gaussian[index]);
        }
    }

    fn minimum_cell(&mut self) -> usize {
        let mut minimum = u64::MAX;
        self.tie_candidates.clear();
        for cell in 0..CELL_COUNT {
            if self.placed[cell] {
                continue;
            }
            let total = self.gaussian_accumulator[cell];
            if total <= minimum {
                if total != minimum {
                    minimum = total;
                    self.tie_candidates.clear();
                }
                self.tie_candidates.push(cell);
            }
        }
        if self.tie_candidates.len() == 1 {
            return self.tie_candidates[0];
        }
        if self.tie_candidates.len() == CELL_COUNT {
            return CELL_COUNT / 2;
        }
        let pick = self.random() % self.tie_candidates.len();
        self.tie_candidates[pick]
    }
}

/// 64x64 matrix of unique rank values in [0, 1), row-major.
fn generate() -> Vec<f32> {
    let mut generator = Generator::new();
    generator.fill_gaussian();
    for rank in 0..CELL_COUNT {
        let cell = generator.minimum_cell();
        generator.place(cell);
        generator.ranks[cell] = rank;
    }
    let mut matrix = vec![0.0f32; CELL_COUNT];
    for y in 0..SIZE {
        for x in 0..SIZE {
            matrix[x + y * SIZE] = generator.ranks[cell_index(x, y)] as f32 / CELL_COUNT as f32;
        }
    }
    matrix
}
