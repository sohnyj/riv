//! Output-dither building blocks for the quantize pass: Ordered (Bayer 16x16)
//! and Fruit (blue noise).
//!
//! The dither math is ported from libplacebo's pl_shader_dither
//! (src/shaders/dithering.c, LGPL-2.1-or-later): ordered-fixed and
//! blue-noise bias generation followed by a biased floor quantization.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::core::{PCSTR, Result, s};

pub const BLUE_NOISE_SIZE: u32 = super::blue_noise::SIZE as u32;

/// Bias and quantization functions for the quantize-pass pixel shaders; the
/// including source declares `quantization_steps` in a constant buffer.
pub const DITHER_SHADER_FUNCTIONS: &str = "\
float ordered_bias(float2 position)
{
    float2 pos = frac(position * (1.0 / 16.0));
    uint2 xy = uint2(pos * 16.0) % 16u;
    xy.x = xy.x ^ xy.y;
    xy = (xy | xy << 2) & 0x33333333u;
    xy = (xy | xy << 1) & 0x55555555u;
    uint b = xy.x + (xy.y << 1);
    b = (b * 0x0802u & 0x22110u) | (b * 0x8020u & 0x88440u);
    b = 0x10101u * b;
    b = (b >> 16) & 0xFFu;
    return float(b) * (1.0 / 256.0);
}

float blue_noise_bias(Texture2D noise, float2 position)
{
    float2 pos = frac(position * (1.0 / 64.0));
    return noise.Load(int3(int2(pos * 64.0), 0)).r;
}

float3 dither_quantize(float3 color, float bias)
{
    const float scale = quantization_steps;
    color = (abs(color) < 1e-5) ? float3(0.0, 0.0, 0.0) : color;
    color = scale * color + bias;
    return floor(color) * (1.0 / scale);
}
";

/// Fullscreen triangle from SV_VertexID, shared by the D3D11 passes.
pub const FULLSCREEN_TRIANGLE_VERTEX_SHADER: &str = "\
float4 main(uint vertex_id : SV_VertexID) : SV_POSITION
{
    float2 position = float2((vertex_id << 1) & 2, vertex_id & 2);
    return float4(position * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}
";

pub fn compile_shader(source: &str, profile: PCSTR) -> Result<Vec<u8>> {
    let mut code = None;
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("riv_shader"),
            None,
            None,
            s!("main"),
            profile,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &raw mut code,
            None,
        )?;
    }
    let blob = code.ok_or_else(windows::core::Error::empty)?;
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

/// Single-channel f32 texels, generated once per process.
pub fn blue_noise_texels() -> &'static [u8] {
    static TEXELS: OnceLock<Vec<u8>> = OnceLock::new();
    TEXELS.get_or_init(|| {
        super::blue_noise::generate()
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect()
    })
}

/// Settings-selected output dither (0 = None, 1 = Ordered, 2 = Fruit).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DitherMode {
    None,
    Ordered,
    Fruit,
}

impl DitherMode {
    pub fn from_setting(value: u32) -> Self {
        match value {
            1 => Self::Ordered,
            2 => Self::Fruit,
            _ => Self::None,
        }
    }
}
