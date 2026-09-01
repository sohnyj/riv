//! Fullscreen pass from the UNORM16 scene to the UNORM backbuffer; the dithered write quantizes.

use std::cell::Cell;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_IMMUTABLE, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_FLOAT, DXGI_SAMPLE_DESC};
use windows::core::Result;

use crate::view::dither::{BLUE_NOISE_EDGE_TEXELS, BLUE_NOISE_TEXELS, DitherMode};
use crate::view::pass::ConstantBuffer;

/// DXBC compiled by the build script; the viewer never runs a shader compiler.
const COPY_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/copy.dxbc"));
const ORDERED_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ordered.dxbc"));
const FRUIT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fruit.dxbc"));

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
    constant_buffer: ConstantBuffer<QuantizationConstants>,
    /// What the buffer already holds; the depth only moves when the output is rebuilt.
    written_steps: Cell<Option<u32>>,
    blue_noise_view: ID3D11ShaderResourceView,
}

impl QuantizePass {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let noise_description = D3D11_TEXTURE2D_DESC {
            Width: BLUE_NOISE_EDGE_TEXELS,
            Height: BLUE_NOISE_EDGE_TEXELS,
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
            pSysMem: BLUE_NOISE_TEXELS.as_ptr().cast(),
            SysMemPitch: BLUE_NOISE_EDGE_TEXELS * 4,
            ..Default::default()
        };
        let mut noise_texture = None;
        let mut blue_noise_view = None;
        unsafe {
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
            vertex_shader: crate::view::pass::create_vertex_shader(device)?,
            copy_shader: crate::view::pass::create_pixel_shader(device, COPY_SHADER)?,
            ordered_shader: crate::view::pass::create_pixel_shader(device, ORDERED_SHADER)?,
            fruit_shader: crate::view::pass::create_pixel_shader(device, FRUIT_SHADER)?,
            constant_buffer: crate::view::pass::create_constant_buffer::<QuantizationConstants>(
                device,
            )?,
            written_steps: Cell::new(None),
            blue_noise_view: blue_noise_view
                .expect("CreateShaderResourceView succeeded without view"),
        })
    }

    fn write_constants(
        &self,
        context: &ID3D11DeviceContext,
        quantization_steps: u32,
    ) -> Result<()> {
        if self.written_steps.get() == Some(quantization_steps) {
            return Ok(());
        }
        let constants = QuantizationConstants {
            quantization_steps: quantization_steps as f32,
            padding: [0.0; 3],
        };
        crate::view::pass::write_constants(context, &self.constant_buffer, &constants)?;
        self.written_steps.set(Some(quantization_steps));
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
        let dithered =
            dither != DitherMode::None && self.write_constants(context, quantization_steps).is_ok();
        let pixel_shader = match dither {
            DitherMode::Ordered if dithered => &self.ordered_shader,
            DitherMode::Fruit if dithered => &self.fruit_shader,
            _ => &self.copy_shader,
        };
        crate::view::pass::draw_fullscreen(
            context,
            &self.vertex_shader,
            pixel_shader,
            &self.constant_buffer,
            &[Some(scene.clone()), Some(self.blue_noise_view.clone())],
            target,
            target_size,
        );
    }
}
