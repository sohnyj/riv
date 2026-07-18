//! Axis-separable scaling shaders: Lanczos (sinc-windowed sinc, radius 3)
//! and Hermite (cubic B = 0, C = 0, radius 1), one 1D convolution pass per
//! direction with kernel widening on downscale.
//!
//! The kernels and pass structure are ported from libplacebo (src/filters.c
//! and pl_shader_sample_ortho2 in src/shaders/sampling.c, LGPL-2.1-or-later),
//! with ALU-computed weights in place of the upstream weight LUT. The
//! bytecode is plain shader model 5.0 DXBC, loadable by Direct3D 11 and
//! Direct3D 12 alike. Not yet wired into the renderer.

use windows::core::{Result, s};

use crate::view::dither::compile_shader;

const VERTEX_SHADER_SOURCE: &str = "\
struct VertexOutput
{
    float4 position : SV_POSITION;
    float2 texture_coordinate : TEXCOORD0;
};

VertexOutput main(uint vertex_id : SV_VertexID)
{
    VertexOutput output;
    float2 unit = float2((vertex_id << 1) & 2, vertex_id & 2);
    output.position = float4(unit * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    output.texture_coordinate = unit;
    return output;
}
";

const PIXEL_SHADER_SOURCE: &str = "\
Texture2D source_texture : register(t0);
SamplerState source_sampler : register(s0);

cbuffer sampling_constants : register(b0)
{
    float2 source_size;
    float2 inverse_source_size;
    float2 direction;
    float inverse_scale;
    float unused;
};

static const float PI = 3.14159265358979323846;

float sinc(float x)
{
    if (x < 1e-8)
        return 1.0;
    x *= PI;
    return sin(x) / x;
}

float lanczos_weight(float x)
{
    return sinc(x) * sinc(x / 3.0);
}

float hermite_weight(float x)
{
    return (2.0 * x - 3.0) * x * x + 1.0;
}

float4 main(float4 position : SV_POSITION,
            float2 texture_coordinate : TEXCOORD0) : SV_Target
{
    float center = dot(texture_coordinate * source_size - 0.5, direction);
    float fraction = center - floor(center);
    int taps = int(ceil(FILTER_RADIUS * inverse_scale));
    float4 accumulated = float4(0.0, 0.0, 0.0, 0.0);
    float weight_sum = 0.0;
    [loop]
    for (int offset = 1 - taps; offset <= taps; offset++)
    {
        float distance = abs((float(offset) - fraction) / inverse_scale);
        float weight = distance < FILTER_RADIUS ? FILTER_WEIGHT(distance) : 0.0;
        float2 coordinate =
            texture_coordinate + (float(offset) - fraction) * direction * inverse_source_size;
        accumulated += weight * source_texture.SampleLevel(source_sampler, coordinate, 0.0);
        weight_sum += weight;
    }
    return accumulated / weight_sum;
}
";

#[derive(Clone, Copy)]
pub enum SamplingFilter {
    Lanczos,
    Hermite,
}

impl SamplingFilter {
    fn weight_function(self) -> &'static str {
        match self {
            Self::Lanczos => "lanczos_weight",
            Self::Hermite => "hermite_weight",
        }
    }

    fn radius(self) -> &'static str {
        match self {
            Self::Lanczos => "3.0",
            Self::Hermite => "1.0",
        }
    }
}

pub fn compiled_vertex_shader() -> Result<Vec<u8>> {
    compile_shader(VERTEX_SHADER_SOURCE, s!("vs_5_0"))
}

pub fn compiled_pixel_shader(filter: SamplingFilter) -> Result<Vec<u8>> {
    let source = PIXEL_SHADER_SOURCE
        .replace("FILTER_WEIGHT", filter.weight_function())
        .replace("FILTER_RADIUS", filter.radius());
    compile_shader(&source, s!("ps_5_0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_shaders_compile_as_shader_model_5() {
        let vertex_bytecode = compiled_vertex_shader().expect("vertex shader");
        assert_eq!(&vertex_bytecode[..4], b"DXBC");
        for filter in [SamplingFilter::Lanczos, SamplingFilter::Hermite] {
            let pixel_bytecode = compiled_pixel_shader(filter).expect("pixel shader");
            assert_eq!(&pixel_bytecode[..4], b"DXBC");
        }
    }
}
