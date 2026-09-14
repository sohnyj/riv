//! Textures the renderer draws into and the views over them.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};

/// A texture from its description; a successful create always fills the out parameter.
pub fn create_texture(
    device: &ID3D11Device,
    description: &D3D11_TEXTURE2D_DESC,
    initial_data: Option<&D3D11_SUBRESOURCE_DATA>,
) -> windows::core::Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &raw const *description,
            initial_data.map(|data| &raw const *data),
            Some(&raw mut texture),
        )?
    };
    Ok(texture.expect("CreateTexture2D succeeded without texture"))
}
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// Render-target + shader-resource texture: the intermediate scene or a presentation buffer.
pub fn create_render_texture(
    device: &ID3D11Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
    misc_flags: D3D11_RESOURCE_MISC_FLAG,
) -> windows::core::Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        MiscFlags: misc_flags.0 as u32,
        ..Default::default()
    };
    create_texture(device, &description, None)
}

pub fn create_render_target_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11RenderTargetView> {
    let mut view = None;
    unsafe { device.CreateRenderTargetView(texture, None, Some(&raw mut view))? };
    Ok(view.expect("CreateRenderTargetView succeeded without view"))
}

pub fn create_shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11ShaderResourceView> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(texture, None, Some(&raw mut view))? };
    Ok(view.expect("CreateShaderResourceView succeeded without view"))
}
