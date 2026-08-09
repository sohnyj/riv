//! Output-dither settings and inputs for the quantize pass; the math lives in the HLSL.

pub const BLUE_NOISE_SIZE: u32 = 64;

/// Single-channel f32 texels; the build script runs the void-and-cluster construction.
pub const BLUE_NOISE_TEXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blue_noise.bin"));

/// Settings-selected output dither (0 = None, 1 = Ordered, 2 = Fruit).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DitherMode {
    None,
    Ordered,
    Fruit,
}

impl DitherMode {
    /// Stored order: the settings value is a position here, and so is the combo row.
    pub const IN_SETTING_ORDER: [Self; 3] = [Self::None, Self::Ordered, Self::Fruit];

    pub fn from_setting(value: u32) -> Self {
        Self::IN_SETTING_ORDER
            .get(value as usize)
            .copied()
            .unwrap_or(Self::None)
    }

    /// Name for the info panel.
    pub fn description(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Ordered => "Ordered",
            Self::Fruit => "Fruit",
        }
    }
}

#[cfg(test)]
mod blue_noise_tests {
    use super::*;

    #[test]
    fn the_built_matrix_is_a_permutation_of_all_ranks() {
        let cell_count = (BLUE_NOISE_SIZE * BLUE_NOISE_SIZE) as usize;
        assert_eq!(BLUE_NOISE_TEXELS.len(), cell_count * size_of::<f32>());
        let mut seen = vec![false; cell_count];
        for texel in BLUE_NOISE_TEXELS.chunks_exact(size_of::<f32>()) {
            let value = f32::from_le_bytes(texel.try_into().expect("four byte texel"));
            assert!((0.0..1.0).contains(&value));
            let rank = (value * cell_count as f32) as usize;
            assert!(!seen[rank], "duplicate rank {rank}");
            seen[rank] = true;
        }
    }
}
