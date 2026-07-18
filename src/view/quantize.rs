//! Fullscreen copy of the UNORM16 scene to the 10-bit backbuffer; the UNORM write quantizes.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_VIEWPORT, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::core::{Result, s};

use crate::view::dither::{FULLSCREEN_TRIANGLE_VERTEX_SHADER, compile_shader};

const PIXEL_SHADER_SOURCE: &str = "\
Texture2D scene_texture : register(t0);

float4 main(float4 position : SV_POSITION) : SV_Target
{
    return scene_texture.Load(int3(position.xy, 0));
}
";

pub struct QuantizePass {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
}

impl QuantizePass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex_bytecode = compile_shader(FULLSCREEN_TRIANGLE_VERTEX_SHADER, s!("vs_5_0"))?;
        let pixel_bytecode = compile_shader(PIXEL_SHADER_SOURCE, s!("ps_5_0"))?;
        let mut vertex_shader = None;
        let mut pixel_shader = None;
        unsafe {
            device.CreateVertexShader(&vertex_bytecode, None, Some(&raw mut vertex_shader))?;
            device.CreatePixelShader(&pixel_bytecode, None, Some(&raw mut pixel_shader))?;
        }
        Ok(Self {
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded without shader"),
            pixel_shader: pixel_shader.expect("CreatePixelShader succeeded without shader"),
        })
    }

    pub fn draw(
        &self,
        context: &ID3D11DeviceContext,
        scene: &ID3D11ShaderResourceView,
        target: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
    ) {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
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
            context.PSSetShaderResources(0, Some(&[Some(scene.clone())]));
            context.Draw(3, 0);
            // Unbind so D2D can retake the scene texture as a target next frame.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
    }
}
