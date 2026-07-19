//! Fullscreen pass from the UNORM16 scene to the UNORM backbuffer: applies the
//! output dither in destination pixel space, then the UNORM write quantizes.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE_DISCARD, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_FLOAT, DXGI_SAMPLE_DESC};
use windows::core::{Result, s};

use crate::view::dither::{
    BLUE_NOISE_SIZE, DITHER_SHADER_FUNCTIONS, DitherMode, FULLSCREEN_TRIANGLE_VERTEX_SHADER,
    blue_noise_texels, compile_shader,
};

const SHADER_PROLOGUE: &str = "\
Texture2D scene_texture : register(t0);
Texture2D blue_noise_texture : register(t1);

cbuffer QuantizationConstants : register(b0)
{
    float quantization_steps;
    float3 padding;
};
";

const COPY_SHADER_SOURCE: &str = "\
float4 main(float4 position : SV_POSITION) : SV_Target
{
    return scene_texture.Load(int3(position.xy, 0));
}
";

const ORDERED_SHADER_SOURCE: &str = "\
float4 main(float4 position : SV_POSITION) : SV_Target
{
    float4 color = scene_texture.Load(int3(position.xy, 0));
    color.rgb = dither_quantize(color.rgb, ordered_bias(position.xy));
    return color;
}
";

const FRUIT_SHADER_SOURCE: &str = "\
float4 main(float4 position : SV_POSITION) : SV_Target
{
    float4 color = scene_texture.Load(int3(position.xy, 0));
    color.rgb = dither_quantize(color.rgb, blue_noise_bias(blue_noise_texture, position.xy));
    return color;
}
";

fn pixel_shader_source(body: &str) -> String {
    format!("{SHADER_PROLOGUE}{DITHER_SHADER_FUNCTIONS}{body}")
}

#[repr(C)]
struct QuantizationConstants {
    quantization_steps: f32,
    padding: [f32; 3],
}

pub struct QuantizePass {
    vertex_shader: ID3D11VertexShader,
    copy_shader: ID3D11PixelShader,
    ordered_shader: ID3D11PixelShader,
    fruit_shader: ID3D11PixelShader,
    constant_buffer: ID3D11Buffer,
    blue_noise_view: ID3D11ShaderResourceView,
}

impl QuantizePass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex_bytecode = compile_shader(FULLSCREEN_TRIANGLE_VERTEX_SHADER, s!("vs_5_0"))?;
        let copy_bytecode = compile_shader(&pixel_shader_source(COPY_SHADER_SOURCE), s!("ps_5_0"))?;
        let ordered_bytecode =
            compile_shader(&pixel_shader_source(ORDERED_SHADER_SOURCE), s!("ps_5_0"))?;
        let fruit_bytecode =
            compile_shader(&pixel_shader_source(FRUIT_SHADER_SOURCE), s!("ps_5_0"))?;
        let buffer_description = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<QuantizationConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let noise_description = D3D11_TEXTURE2D_DESC {
            Width: BLUE_NOISE_SIZE,
            Height: BLUE_NOISE_SIZE,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let noise_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: blue_noise_texels().as_ptr().cast(),
            SysMemPitch: BLUE_NOISE_SIZE * 4,
            ..Default::default()
        };
        let mut vertex_shader = None;
        let mut copy_shader = None;
        let mut ordered_shader = None;
        let mut fruit_shader = None;
        let mut constant_buffer = None;
        let mut noise_texture = None;
        let mut blue_noise_view = None;
        unsafe {
            device.CreateVertexShader(&vertex_bytecode, None, Some(&raw mut vertex_shader))?;
            device.CreatePixelShader(&copy_bytecode, None, Some(&raw mut copy_shader))?;
            device.CreatePixelShader(&ordered_bytecode, None, Some(&raw mut ordered_shader))?;
            device.CreatePixelShader(&fruit_bytecode, None, Some(&raw mut fruit_shader))?;
            device.CreateBuffer(
                &raw const buffer_description,
                None,
                Some(&raw mut constant_buffer),
            )?;
            device.CreateTexture2D(
                &raw const noise_description,
                Some(&raw const noise_data),
                Some(&raw mut noise_texture),
            )?;
            let noise_texture = noise_texture.ok_or_else(windows::core::Error::empty)?;
            device.CreateShaderResourceView(
                &noise_texture,
                None,
                Some(&raw mut blue_noise_view),
            )?;
        }
        Ok(Self {
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded without shader"),
            copy_shader: copy_shader.expect("CreatePixelShader succeeded without shader"),
            ordered_shader: ordered_shader.expect("CreatePixelShader succeeded without shader"),
            fruit_shader: fruit_shader.expect("CreatePixelShader succeeded without shader"),
            constant_buffer: constant_buffer.expect("CreateBuffer succeeded without buffer"),
            blue_noise_view: blue_noise_view
                .expect("CreateShaderResourceView succeeded without view"),
        })
    }

    fn write_constants(
        &self,
        context: &ID3D11DeviceContext,
        quantization_steps: u32,
    ) -> Result<()> {
        let constants = QuantizationConstants {
            quantization_steps: quantization_steps as f32,
            padding: [0.0; 3],
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
                size_of::<QuantizationConstants>(),
            );
            context.Unmap(&self.constant_buffer, 0);
        }
        Ok(())
    }

    pub fn draw(
        &self,
        context: &ID3D11DeviceContext,
        scene: &ID3D11ShaderResourceView,
        target: &ID3D11RenderTargetView,
        target_size: (u32, u32),
        dither: DitherMode,
        quantization_steps: u32,
    ) {
        // A constants write failure degrades to the undithered copy.
        let pixel_shader = match dither {
            DitherMode::None => &self.copy_shader,
            DitherMode::Ordered | DitherMode::Fruit
                if self.write_constants(context, quantization_steps).is_err() =>
            {
                &self.copy_shader
            }
            DitherMode::Ordered => &self.ordered_shader,
            DitherMode::Fruit => &self.fruit_shader,
        };
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
            context.PSSetShaderResources(
                0,
                Some(&[Some(scene.clone()), Some(self.blue_noise_view.clone())]),
            );
            context.Draw(3, 0);
            // Unbind so D2D can retake the scene texture as a target next frame.
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_shaders_compile_as_shader_model_5() {
        let vertex_bytecode =
            compile_shader(FULLSCREEN_TRIANGLE_VERTEX_SHADER, s!("vs_5_0")).expect("vertex shader");
        assert_eq!(&vertex_bytecode[..4], b"DXBC");
        for body in [
            COPY_SHADER_SOURCE,
            ORDERED_SHADER_SOURCE,
            FRUIT_SHADER_SOURCE,
        ] {
            let pixel_bytecode =
                compile_shader(&pixel_shader_source(body), s!("ps_5_0")).expect("pixel shader");
            assert_eq!(&pixel_bytecode[..4], b"DXBC");
        }
    }
}
