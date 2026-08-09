//! Shared machinery of the fullscreen passes: constant upload and the draw protocol.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_MAP_WRITE_DISCARD, D3D11_VIEWPORT, ID3D11Buffer, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::core::Result;

/// Overwrites a dynamic constant buffer with one plain-layout value.
pub fn write_constants<Constants>(
    context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    constants: &Constants,
) -> Result<()> {
    unsafe {
        let mut mapped = Default::default();
        context.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&raw mut mapped))?;
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(constants).cast::<u8>(),
            mapped.pData.cast::<u8>(),
            size_of::<Constants>(),
        );
        context.Unmap(buffer, 0);
    }
    Ok(())
}

/// One fullscreen triangle through the pixel shader, with D2D state reset around it.
pub fn draw_fullscreen(
    context: &ID3D11DeviceContext,
    vertex_shader: &ID3D11VertexShader,
    pixel_shader: &ID3D11PixelShader,
    constant_buffer: &ID3D11Buffer,
    shader_resources: &[Option<ID3D11ShaderResourceView>],
    target: &ID3D11RenderTargetView,
    target_size: (u32, u32),
) {
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
        context.VSSetShader(vertex_shader, None);
        context.PSSetShader(pixel_shader, None);
        context.PSSetConstantBuffers(0, Some(&[Some(constant_buffer.clone())]));
        context.PSSetShaderResources(0, Some(shader_resources));
        context.Draw(3, 0);
        // Unbind so D2D can retake the textures next frame.
        context.PSSetShaderResources(0, Some(&vec![None; shader_resources.len()]));
        context.OMSetRenderTargets(None, None);
    }
}
