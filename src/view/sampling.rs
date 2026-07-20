//! Custom scaling passes. Axis-separable pair: Lanczos (sinc-windowed sinc,
//! radius 3) for upscaling and Hermite (cubic B = 0, C = 0, radius 1) for
//! downscaling, one 1D convolution per direction with kernel widening on
//! downscale. Polar alternative: EWA Lanczos (jinc-windowed jinc, radius
//! 3.2383 = the jinc third zero, kernel sharpened by the ewa_lanczossharp
//! blur constant) as a single-pass upscale kernel; its downscaling still
//! runs the separable Hermite.
//!
//! The kernels and pass structure are ported from libplacebo (src/filters.c,
//! pl_shader_sample_ortho2 and the polar sampler in src/shaders/sampling.c,
//! LGPL-2.1-or-later), with ALU-computed weights in place of the upstream
//! weight LUT; Bessel J1 uses the Numerical Recipes rational approximation.
//! The bytecode is plain shader model 5.0 DXBC, loadable by Direct3D 11 and
//! Direct3D 12 alike.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::D3D11_FILTER_MIN_MAG_MIP_POINT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE_DISCARD,
    D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_BORDER, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;
use windows::core::{Result, s};

use crate::view::dither::{FULLSCREEN_TRIANGLE_VERTEX_SHADER, compile_shader};

const SHADER_PRELUDE: &str = "\
Texture2D source_texture : register(t0);
SamplerState source_sampler : register(s0);

cbuffer sampling_constants : register(b0)
{
    float2 inverse_source_size;
    float2 direction;
    float2 position_scale;
    float2 position_offset;
    float kernel_inverse_scale;
    float3 unused;
};

static const float PI = 3.14159265358979323846;
";

const SEPARABLE_SHADER_BODY: &str = "\
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

float4 main(float4 position : SV_POSITION) : SV_Target
{
    float axis_scale = dot(position_scale, direction);
    float axis_offset = dot(position_offset, direction);
    float center = dot(position.xy, direction) * axis_scale + axis_offset - 0.5;
    float base = floor(center);
    float fraction = center - base;
    float inverse_extent = dot(inverse_source_size, direction);
    float source_extent = 1.0 / inverse_extent;
    int taps = int(ceil(FILTER_RADIUS * kernel_inverse_scale));
    float4 accumulated = float4(0.0, 0.0, 0.0, 0.0);
    float weight_sum = 0.0;
    [loop]
    for (int offset = 1 - taps; offset <= taps; offset++)
    {
        float distance = abs((float(offset) - fraction) / kernel_inverse_scale);
        float weight = distance < FILTER_RADIUS ? FILTER_WEIGHT(distance) : 0.0;
        float along_index = clamp(base + float(offset), 0.0, source_extent - 1.0);
        float along = (along_index + 0.5) * inverse_extent;
        float2 coordinate = position.xy * inverse_source_size * (float2(1.0, 1.0) - direction)
            + along * direction;
        accumulated += weight * source_texture.SampleLevel(source_sampler, coordinate, 0.0);
        weight_sum += weight;
    }
    // Clamped taps extend the edge; a one-output-pixel ramp masks outside the rect.
    float edge = min(center + 0.5, source_extent - 0.5 - center);
    float coverage = saturate(edge / abs(axis_scale) + 0.5);
    return accumulated / weight_sum * coverage;
}
";

const EWA_LANCZOS_SHADER_BODY: &str = "\
static const float RADIUS = 3.2383154841662362;
static const float WINDOW_SCALE = 0.37663714274600113;
static const float BLUR = 0.98125058372237073562;

float bessel_j1(float x)
{
    if (x < 8.0)
    {
        float y = x * x;
        return x * (72362614232.0 + y * (-7895059235.0 + y * (242396853.1
            + y * (-2972611.439 + y * (15704.48260 + y * (-30.16036606))))))
            / (144725228442.0 + y * (2300535178.0 + y * (18583304.74
            + y * (99447.43394 + y * (376.9991397 + y)))));
    }
    float z = 8.0 / x;
    float y = z * z;
    float phase = x - 2.356194491;
    float modulation = 1.0 + y * (0.183105e-2 + y * (-0.3516396496e-4
        + y * (0.2457520174e-5 + y * (-0.240337019e-6))));
    float correction = 0.04687499995 + y * (-0.2002690873e-3
        + y * (0.8449199096e-5 + y * (-0.88228987e-6 + y * 0.105787412e-6)));
    return sqrt(0.636619772 / x) * (cos(phase) * modulation - z * sin(phase) * correction);
}

float jinc(float x)
{
    if (x < 1e-8)
        return 1.0;
    x *= PI;
    return 2.0 * bessel_j1(x) / x;
}

// The sharpened variant: only the kernel argument stretches, the window does not.
float ewa_lanczos_weight(float x)
{
    return jinc(x / BLUR) * jinc(x * WINDOW_SCALE);
}

float4 main(float4 position : SV_POSITION) : SV_Target
{
    float2 center = position.xy * position_scale + position_offset - 0.5;
    float2 base = floor(center);
    float2 fraction = center - base;
    float2 source_extent = float2(1.0, 1.0) / inverse_source_size;
    int taps = int(ceil(RADIUS));
    float4 accumulated = float4(0.0, 0.0, 0.0, 0.0);
    float weight_sum = 0.0;
    [loop]
    for (int row = 1 - taps; row <= taps; row++)
    {
        [loop]
        for (int column = 1 - taps; column <= taps; column++)
        {
            float2 offset = float2(column, row);
            float distance = length(offset - fraction);
            float weight = distance < RADIUS ? ewa_lanczos_weight(distance) : 0.0;
            float2 index = clamp(base + offset, float2(0.0, 0.0), source_extent - 1.0);
            float2 coordinate = (index + 0.5) * inverse_source_size;
            accumulated += weight * source_texture.SampleLevel(source_sampler, coordinate, 0.0);
            weight_sum += weight;
        }
    }
    // Clamped taps extend the edge; a one-output-pixel ramp masks outside the rect.
    float2 edge = min(center + 0.5, source_extent - 0.5 - center);
    float2 coverage = saturate(edge / abs(position_scale) + 0.5);
    return accumulated / weight_sum * coverage.x * coverage.y;
}
";

/// Upscale kernel choice (Scaling values 4 and 5); downscaling is always Hermite.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UpscaleKernel {
    Lanczos,
    EwaLanczos,
}

#[derive(Clone, Copy)]
enum SamplingFilter {
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

fn compiled_separable_shader(filter: SamplingFilter) -> Result<Vec<u8>> {
    let source = format!("{SHADER_PRELUDE}{SEPARABLE_SHADER_BODY}")
        .replace("FILTER_WEIGHT", filter.weight_function())
        .replace("FILTER_RADIUS", filter.radius());
    compile_shader(&source, s!("ps_5_0"))
}

fn compiled_ewa_lanczos_shader() -> Result<Vec<u8>> {
    compile_shader(
        &format!("{SHADER_PRELUDE}{EWA_LANCZOS_SHADER_BODY}"),
        s!("ps_5_0"),
    )
}

/// Output pixel center -> continuous source position along one axis.
#[derive(Clone, Copy)]
pub struct AxisMapping {
    pub position_scale: f32,
    pub position_offset: f32,
}

impl AxisMapping {
    /// True when this axis magnifies (an output pixel spans at most one texel).
    pub fn magnifies(&self) -> bool {
        self.position_scale.abs() <= 1.0
    }

    fn filter(&self) -> SamplingFilter {
        if self.magnifies() {
            SamplingFilter::Lanczos
        } else {
            SamplingFilter::Hermite
        }
    }

    pub fn filter_name(&self) -> &'static str {
        match self.filter() {
            SamplingFilter::Lanczos => "Lanczos",
            SamplingFilter::Hermite => "Hermite",
        }
    }

    fn kernel_inverse_scale(&self) -> f32 {
        self.position_scale.abs().max(1.0)
    }
}

#[repr(C)]
struct SamplingConstants {
    inverse_source_size: [f32; 2],
    direction: [f32; 2],
    position_scale: [f32; 2],
    position_offset: [f32; 2],
    kernel_inverse_scale: f32,
    unused: [f32; 3],
}

impl SamplingConstants {
    fn new(
        source_size: (u32, u32),
        direction: [f32; 2],
        horizontal: AxisMapping,
        vertical: AxisMapping,
        kernel_inverse_scale: f32,
    ) -> Self {
        Self {
            inverse_source_size: [
                1.0 / source_size.0.max(1) as f32,
                1.0 / source_size.1.max(1) as f32,
            ],
            direction,
            position_scale: [horizontal.position_scale, vertical.position_scale],
            position_offset: [horizontal.position_offset, vertical.position_offset],
            kernel_inverse_scale,
            unused: [0.0; 3],
        }
    }
}

struct Intermediate {
    shader_resource_view: ID3D11ShaderResourceView,
    render_target_view: ID3D11RenderTargetView,
    size: (u32, u32),
}

pub struct SamplingPass {
    vertex_shader: ID3D11VertexShader,
    lanczos_shader: ID3D11PixelShader,
    hermite_shader: ID3D11PixelShader,
    ewa_lanczos_shader: ID3D11PixelShader,
    constant_buffer: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    intermediate: Option<Intermediate>,
}

impl SamplingPass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex_bytecode = compile_shader(FULLSCREEN_TRIANGLE_VERTEX_SHADER, s!("vs_5_0"))?;
        let lanczos_bytecode = compiled_separable_shader(SamplingFilter::Lanczos)?;
        let hermite_bytecode = compiled_separable_shader(SamplingFilter::Hermite)?;
        let ewa_lanczos_bytecode = compiled_ewa_lanczos_shader()?;
        let mut vertex_shader = None;
        let mut lanczos_shader = None;
        let mut hermite_shader = None;
        let mut ewa_lanczos_shader = None;
        let buffer_description = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<SamplingConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut constant_buffer = None;
        let sampler_description = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_BORDER,
            AddressV: D3D11_TEXTURE_ADDRESS_BORDER,
            AddressW: D3D11_TEXTURE_ADDRESS_BORDER,
            BorderColor: [0.0; 4],
            ..Default::default()
        };
        let mut sampler = None;
        unsafe {
            device.CreateVertexShader(&vertex_bytecode, None, Some(&raw mut vertex_shader))?;
            device.CreatePixelShader(&lanczos_bytecode, None, Some(&raw mut lanczos_shader))?;
            device.CreatePixelShader(&hermite_bytecode, None, Some(&raw mut hermite_shader))?;
            device.CreatePixelShader(
                &ewa_lanczos_bytecode,
                None,
                Some(&raw mut ewa_lanczos_shader),
            )?;
            device.CreateBuffer(
                &raw const buffer_description,
                None,
                Some(&raw mut constant_buffer),
            )?;
            device.CreateSamplerState(&raw const sampler_description, Some(&raw mut sampler))?;
        }
        Ok(Self {
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded without shader"),
            lanczos_shader: lanczos_shader.expect("CreatePixelShader succeeded without shader"),
            hermite_shader: hermite_shader.expect("CreatePixelShader succeeded without shader"),
            ewa_lanczos_shader: ewa_lanczos_shader
                .expect("CreatePixelShader succeeded without shader"),
            constant_buffer: constant_buffer.expect("CreateBuffer succeeded without buffer"),
            sampler: sampler.expect("CreateSamplerState succeeded without sampler"),
            intermediate: None,
        })
    }

    pub fn invalidate(&mut self) {
        self.intermediate = None;
    }

    fn ensure_intermediate(
        &mut self,
        device: &ID3D11Device,
        size: (u32, u32),
    ) -> Result<&Intermediate> {
        if self
            .intermediate
            .as_ref()
            .is_none_or(|held| held.size != size)
        {
            let texture =
                crate::view::create_scene_texture(device, size, DXGI_FORMAT_R16G16B16A16_FLOAT)?;
            let mut shader_resource_view = None;
            let mut render_target_view = None;
            unsafe {
                device.CreateShaderResourceView(
                    &texture,
                    None,
                    Some(&raw mut shader_resource_view),
                )?;
                device.CreateRenderTargetView(&texture, None, Some(&raw mut render_target_view))?;
            }
            self.intermediate = Some(Intermediate {
                shader_resource_view: shader_resource_view
                    .ok_or_else(windows::core::Error::empty)?,
                render_target_view: render_target_view.ok_or_else(windows::core::Error::empty)?,
                size,
            });
        }
        Ok(self
            .intermediate
            .as_ref()
            .expect("intermediate just ensured"))
    }

    fn write_constants(
        &self,
        context: &ID3D11DeviceContext,
        constants: &SamplingConstants,
    ) -> Result<()> {
        unsafe {
            let mut mapped = Default::default();
            context.Map(
                &self.constant_buffer,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&raw mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(
                (constants as *const SamplingConstants).cast::<u8>(),
                mapped.pData.cast::<u8>(),
                size_of::<SamplingConstants>(),
            );
            context.Unmap(&self.constant_buffer, 0);
        }
        Ok(())
    }

    fn draw_pass(
        &self,
        context: &ID3D11DeviceContext,
        pixel_shader: &ID3D11PixelShader,
        source: &ID3D11ShaderResourceView,
        target: &ID3D11RenderTargetView,
        target_size: (u32, u32),
        constants: &SamplingConstants,
    ) -> Result<()> {
        self.write_constants(context, constants)?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: target_size.0 as f32,
            Height: target_size.1 as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            // D2D leaves undefined pipeline state behind; reset to opaque overwrite.
            context.OMSetBlendState(None, None, u32::MAX);
            context.OMSetDepthStencilState(None, 0);
            context.RSSetState(None);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(source.clone())]));
            context.Draw(3, 0);
            // Unbind so the textures can rebind as targets or D2D surfaces.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }

    fn shader_for(&self, filter: SamplingFilter) -> &ID3D11PixelShader {
        match filter {
            SamplingFilter::Lanczos => &self.lanczos_shader,
            SamplingFilter::Hermite => &self.hermite_shader,
        }
    }

    /// Horizontal then vertical convolution from `source` into `target`.
    #[expect(clippy::too_many_arguments)]
    pub fn scale(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11ShaderResourceView,
        source_size: (u32, u32),
        target: &ID3D11RenderTargetView,
        target_size: (u32, u32),
        horizontal: AxisMapping,
        vertical: AxisMapping,
    ) -> Result<()> {
        let intermediate_size = (target_size.0, source_size.1);
        self.ensure_intermediate(device, intermediate_size)?;
        let intermediate = self.intermediate.as_ref().expect("intermediate ensured");
        self.draw_pass(
            context,
            self.shader_for(horizontal.filter()),
            source,
            &intermediate.render_target_view,
            intermediate_size,
            &SamplingConstants::new(
                source_size,
                [1.0, 0.0],
                horizontal,
                vertical,
                horizontal.kernel_inverse_scale(),
            ),
        )?;
        self.draw_pass(
            context,
            self.shader_for(vertical.filter()),
            &intermediate.shader_resource_view,
            target,
            target_size,
            &SamplingConstants::new(
                intermediate_size,
                [0.0, 1.0],
                horizontal,
                vertical,
                vertical.kernel_inverse_scale(),
            ),
        )
    }

    /// Single polar EWA Lanczos pass from `source` into `target`; both axes must magnify.
    #[expect(clippy::too_many_arguments)]
    pub fn scale_ewa_lanczos(
        &self,
        context: &ID3D11DeviceContext,
        source: &ID3D11ShaderResourceView,
        source_size: (u32, u32),
        target: &ID3D11RenderTargetView,
        target_size: (u32, u32),
        horizontal: AxisMapping,
        vertical: AxisMapping,
    ) -> Result<()> {
        self.draw_pass(
            context,
            &self.ewa_lanczos_shader,
            source,
            target,
            target_size,
            &SamplingConstants::new(source_size, [0.0, 0.0], horizontal, vertical, 1.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_shaders_compile_as_shader_model_5() {
        let vertex_bytecode =
            compile_shader(FULLSCREEN_TRIANGLE_VERTEX_SHADER, s!("vs_5_0")).expect("vertex shader");
        assert_eq!(&vertex_bytecode[..4], b"DXBC");
        for filter in [SamplingFilter::Lanczos, SamplingFilter::Hermite] {
            let pixel_bytecode = compiled_separable_shader(filter).expect("pixel shader");
            assert_eq!(&pixel_bytecode[..4], b"DXBC");
        }
        let ewa_lanczos_bytecode = compiled_ewa_lanczos_shader().expect("pixel shader");
        assert_eq!(&ewa_lanczos_bytecode[..4], b"DXBC");
    }
}
