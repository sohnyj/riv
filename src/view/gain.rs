//! Gain application pass: base x 2^(boost x W) baked into a linear FP16 texture.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::core::Result;

use crate::image::gain_map::GainMapMetadata;
use crate::view::pass::ConstantBuffer;

/// DXBC compiled by the build script; the viewer never runs a shader compiler.
const GAIN_APPLY_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gain_apply.dxbc"));

#[repr(C)]
struct GainConstants {
    /// x = weight W, y = SDR white boost; the rest pads the float4.
    weight_and_boost: [f32; 4],
    map_gamma: [f32; 4],
    boost_minimum: [f32; 4],
    boost_maximum: [f32; 4],
    offset_sdr: [f32; 4],
    offset_hdr: [f32; 4],
}

fn padded(values: [f32; 3]) -> [f32; 4] {
    [values[0], values[1], values[2], 0.0]
}

pub struct BakeInputs<'resources> {
    pub base: &'resources ID3D11ShaderResourceView,
    pub gain_map: &'resources ID3D11ShaderResourceView,
    pub target: &'resources ID3D11RenderTargetView,
    pub target_size: (u32, u32),
    pub metadata: &'resources GainMapMetadata,
    pub weight: f32,
    pub sdr_white_boost: f32,
}

pub struct GainMapPass {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    constant_buffer: ConstantBuffer<GainConstants>,
    /// Bilinear for the gain map, which is usually smaller than the base.
    sampler: ID3D11SamplerState,
}

impl GainMapPass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let sampler_description = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ..Default::default()
        };
        let mut pixel_shader = None;
        let mut sampler = None;
        unsafe {
            device.CreatePixelShader(GAIN_APPLY_SHADER, None, Some(&raw mut pixel_shader))?;
            device.CreateSamplerState(&raw const sampler_description, Some(&raw mut sampler))?;
        }
        Ok(Self {
            vertex_shader: crate::view::pass::create_vertex_shader(device)?,
            pixel_shader: pixel_shader.expect("CreatePixelShader succeeded without shader"),
            constant_buffer: crate::view::pass::create_constant_buffer::<GainConstants>(device)?,
            sampler: sampler.expect("CreateSamplerState succeeded without sampler"),
        })
    }

    fn write_constants(
        &self,
        context: &ID3D11DeviceContext,
        metadata: &GainMapMetadata,
        weight: f32,
        sdr_white_boost: f32,
    ) -> Result<()> {
        let constants = GainConstants {
            weight_and_boost: [weight, sdr_white_boost, 0.0, 0.0],
            map_gamma: padded(metadata.gamma),
            boost_minimum: padded(metadata.gain_map_minimum),
            boost_maximum: padded(metadata.gain_map_maximum),
            offset_sdr: padded(metadata.offset_sdr),
            offset_hdr: padded(metadata.offset_hdr),
        };
        crate::view::pass::write_constants(context, &self.constant_buffer, &constants)
    }

    /// Applies the gain map over the whole target; the caller wraps the texture for D2D.
    pub fn bake(&self, context: &ID3D11DeviceContext, inputs: BakeInputs) -> Result<()> {
        let BakeInputs {
            base,
            gain_map,
            target,
            target_size,
            metadata,
            weight,
            sdr_white_boost,
        } = inputs;
        self.write_constants(context, metadata, weight, sdr_white_boost)?;
        unsafe { context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())])) };
        crate::view::pass::draw_fullscreen(
            context,
            &self.vertex_shader,
            &self.pixel_shader,
            &self.constant_buffer,
            &[Some(base.clone()), Some(gain_map.clone())],
            target,
            target_size,
        );
        Ok(())
    }
}
