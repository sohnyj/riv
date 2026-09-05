//! Output-dither settings and inputs for the quantize pass; the math lives in the HLSL.

/// Single-channel f32 texels; the build script runs the void-and-cluster construction.
pub const BLUE_NOISE_TEXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blue_noise.bin"));
/// The square table's edge, read back from the table so the generator stays its one definition.
pub const BLUE_NOISE_EDGE_TEXELS: u32 = (BLUE_NOISE_TEXELS.len() / size_of::<f32>()).isqrt() as u32;

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

    /// Name for the information panel.
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
        // A non-square table would truncate in isqrt and fail this equality.
        let cell_count = (BLUE_NOISE_EDGE_TEXELS * BLUE_NOISE_EDGE_TEXELS) as usize;
        assert_eq!(BLUE_NOISE_TEXELS.len(), cell_count * size_of::<f32>());
        let mut seen = vec![false; cell_count];
        for texel in BLUE_NOISE_TEXELS.as_chunks::<{ size_of::<f32>() }>().0 {
            let value = f32::from_le_bytes(*texel);
            assert!((0.0..1.0).contains(&value));
            let rank = (value * cell_count as f32) as usize;
            assert!(!seen[rank], "duplicate rank {rank}");
            seen[rank] = true;
        }
    }
}
