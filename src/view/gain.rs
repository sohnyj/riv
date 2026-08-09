//! Gain application pass: base x 2^(boost x W) baked into a linear FP16 texture.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_WRITE_DISCARD, D3D11_SAMPLER_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::core::Result;

use crate::image::gain_map::GainMapMetadata;

/// DXBC compiled by the build script; the viewer never runs a shader compiler.
const VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen_triangle.dxbc"));
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

/// One bake's inputs: the source views, the target, and the scalar constants.
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
    constant_buffer: ID3D11Buffer,
    /// Bilinear for the gain map, which is usually smaller than the base.
    sampler: ID3D11SamplerState,
}

impl GainMapPass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let buffer_description = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<GainConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let sampler_description = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ..Default::default()
        };
        let mut vertex_shader = None;
        let mut pixel_shader = None;
        let mut constant_buffer = None;
        let mut sampler = None;
        unsafe {
            device.CreateVertexShader(VERTEX_SHADER, None, Some(&raw mut vertex_shader))?;
            device.CreatePixelShader(GAIN_APPLY_SHADER, None, Some(&raw mut pixel_shader))?;
            device.CreateBuffer(
                &raw const buffer_description,
                None,
                Some(&raw mut constant_buffer),
            )?;
            device.CreateSamplerState(&raw const sampler_description, Some(&raw mut sampler))?;
        }
        Ok(Self {
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded without shader"),
            pixel_shader: pixel_shader.expect("CreatePixelShader succeeded without shader"),
            constant_buffer: constant_buffer.expect("CreateBuffer succeeded without buffer"),
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
                (&raw const constants).cast::<u8>(),
                mapped.pData.cast::<u8>(),
                size_of::<GainConstants>(),
            );
            context.Unmap(&self.constant_buffer, 0);
        }
        Ok(())
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
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(base.clone()), Some(gain_map.clone())]));
            context.Draw(3, 0);
            // Unbind so D2D can retake the textures next frame.
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}
