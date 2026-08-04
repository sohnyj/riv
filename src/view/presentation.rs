//! Presentation-manager buffers composed by DWM, never promoted to independent flip.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::CompositionSwapchain::{
    IPresentationBuffer, IPresentationFactory, IPresentationManager, IPresentationSurface,
};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_PREVENT_INTERNAL_THREADING_OPTIMIZATIONS,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    COMPOSITIONOBJECT_READ, COMPOSITIONOBJECT_WRITE, DCompositionCreateDevice,
    DCompositionCreateSurfaceHandle, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::{GUID, HRESULT, IUnknown, Interface, Result, s, w};

/// The presentation factory refuses devices created without this flag.
pub const REQUIRED_DEVICE_FLAG: D3D11_CREATE_DEVICE_FLAG =
    D3D11_CREATE_DEVICE_PREVENT_INTERNAL_THREADING_OPTIMIZATIONS;

/// One presentation buffer with its render bindings; the event signals availability.
pub struct BufferSlot {
    pub buffer: IPresentationBuffer,
    pub texture: ID3D11Texture2D,
    available_event: HANDLE,
    /// D2D draws here when the pass is absent; None when the quantize pass writes the buffer.
    pub d2d_target: Option<ID2D1Bitmap1>,
    pub render_target_view: Option<ID3D11RenderTargetView>,
}

impl Drop for BufferSlot {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.available_event) };
    }
}

/// Presentation manager, its surface bound into a DComp visual tree, and the buffer ring.
pub struct CompositionPresenter {
    manager: IPresentationManager,
    surface: IPresentationSurface,
    lost_event: HANDLE,
    surface_handle: HANDLE,
    buffers: Vec<BufferSlot>,
    next_buffer_index: usize,
    /// Format, size, and count of the current ring, so an unchanged target skips reallocation.
    allocated: Option<(DXGI_FORMAT, (u32, u32), usize)>,
    _composition_device: IDCompositionDevice,
    _composition_target: IDCompositionTarget,
    _composition_visual: IDCompositionVisual,
    _composition_content: IUnknown,
}

impl Drop for CompositionPresenter {
    fn drop(&mut self) {
        self.buffers.clear();
        let _ = unsafe { CloseHandle(self.lost_event) };
        let _ = unsafe { CloseHandle(self.surface_handle) };
    }
}

/// The factory entry point, resolved at run time: wine's dcomp.dll lacks the export.
fn create_presentation_factory(d3d_device: &ID3D11Device) -> Option<IPresentationFactory> {
    type CreatePresentationFactoryFunction = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *const GUID,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT;
    let module = unsafe { LoadLibraryW(w!("dcomp.dll")) }.ok()?;
    let address = unsafe { GetProcAddress(module, s!("CreatePresentationFactory")) }?;
    let create: CreatePresentationFactoryFunction = unsafe { std::mem::transmute(address) };
    let mut pointer: *mut core::ffi::c_void = core::ptr::null_mut();
    unsafe {
        create(
            d3d_device.as_raw(),
            &IPresentationFactory::IID,
            &raw mut pointer,
        )
    }
    .ok()
    .ok()?;
    Some(unsafe { IPresentationFactory::from_raw(pointer) })
}

impl CompositionPresenter {
    /// None when the system cannot present this way; the caller keeps the hwnd swapchain.
    pub fn new(d3d_device: &ID3D11Device, window: HWND) -> Option<Self> {
        let factory = create_presentation_factory(d3d_device)?;
        if unsafe { factory.IsPresentationSupported() } == 0 {
            return None;
        }
        Self::bind(&factory, d3d_device, window).ok()
    }

    fn bind(
        factory: &IPresentationFactory,
        d3d_device: &ID3D11Device,
        window: HWND,
    ) -> Result<Self> {
        // dcomp.h COMPOSITIONOBJECT_ALL_ACCESS.
        let access = (COMPOSITIONOBJECT_READ | COMPOSITIONOBJECT_WRITE) as u32;
        let manager = unsafe { factory.CreatePresentationManager() }?;
        let lost_event = unsafe { manager.GetLostEvent() }?;
        let surface_handle = match unsafe { DCompositionCreateSurfaceHandle(access, None) } {
            Ok(handle) => handle,
            Err(error) => {
                let _ = unsafe { CloseHandle(lost_event) };
                return Err(error);
            }
        };
        let bound = (|| {
            let surface = unsafe { manager.CreatePresentationSurface(surface_handle) }?;
            unsafe { surface.SetAlphaMode(DXGI_ALPHA_MODE_IGNORE) }?;
            let dxgi_device: IDXGIDevice = d3d_device.cast()?;
            let composition_device: IDCompositionDevice =
                unsafe { DCompositionCreateDevice(&dxgi_device) }?;
            let composition_target =
                unsafe { composition_device.CreateTargetForHwnd(window, true) }?;
            let composition_visual = unsafe { composition_device.CreateVisual() }?;
            let composition_content =
                unsafe { composition_device.CreateSurfaceFromHandle(surface_handle) }?;
            unsafe { composition_visual.SetContent(&composition_content) }?;
            unsafe { composition_target.SetRoot(&composition_visual) }?;
            unsafe { composition_device.Commit() }?;
            Ok((
                surface,
                composition_device,
                composition_target,
                composition_visual,
                composition_content,
            ))
        })();
        match bound {
            Ok((surface, device, target, visual, content)) => Ok(Self {
                manager,
                surface,
                lost_event,
                surface_handle,
                buffers: Vec::new(),
                next_buffer_index: 0,
                allocated: None,
                _composition_device: device,
                _composition_target: target,
                _composition_visual: visual,
                _composition_content: content,
            }),
            Err(error) => {
                let _ = unsafe { CloseHandle(lost_event) };
                let _ = unsafe { CloseHandle(surface_handle) };
                Err(error)
            }
        }
    }

    pub fn set_color_space(&self, color_space: DXGI_COLOR_SPACE_TYPE) -> Result<()> {
        unsafe { self.surface.SetColorSpace(color_space) }
    }

    /// Reallocates the ring only when the format or size changed.
    pub fn ensure_buffers(
        &mut self,
        d3d_device: &ID3D11Device,
        format: DXGI_FORMAT,
        size: (u32, u32),
        count: usize,
    ) -> Result<()> {
        if self.allocated == Some((format, size, count)) {
            return Ok(());
        }
        self.allocated = None;
        self.allocate_buffers(d3d_device, format, size, count)?;
        self.allocated = Some((format, size, count));
        Ok(())
    }

    /// Replaces the buffer ring with `count` fresh textures and scopes the surface to them.
    fn allocate_buffers(
        &mut self,
        d3d_device: &ID3D11Device,
        format: DXGI_FORMAT,
        size: (u32, u32),
        count: usize,
    ) -> Result<()> {
        self.buffers.clear();
        self.next_buffer_index = 0;
        for _ in 0..count {
            // Shareable but not displayable: composition only, never independent flip.
            let texture = crate::view::texture::create_render_texture(
                d3d_device,
                size,
                format,
                D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
            )?;
            let buffer = unsafe {
                self.manager
                    .AddBufferFromResource(&texture.cast::<IUnknown>()?)
            }?;
            let available_event = unsafe { buffer.GetAvailableEvent() }?;
            self.buffers.push(BufferSlot {
                buffer,
                texture,
                available_event,
                d2d_target: None,
                render_target_view: None,
            });
        }
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: size.0 as i32,
            bottom: size.1 as i32,
        };
        unsafe { self.surface.SetSourceRect(&raw const source_rect) }
    }

    pub fn buffers_mut(&mut self) -> &mut [BufferSlot] {
        &mut self.buffers
    }

    /// The slot the next frame draws into.
    pub fn next_slot(&self) -> Option<&BufferSlot> {
        self.buffers.get(self.next_buffer_index)
    }

    /// The event the pump waits on before the next frame; signaled while the buffer is free.
    pub fn next_available_event(&self) -> Option<HANDLE> {
        self.next_slot().map(|slot| slot.available_event)
    }

    /// The composition system dropped this manager; the renderer must be rebuilt.
    pub fn is_lost(&self) -> bool {
        let waited = unsafe { WaitForSingleObjectEx(self.lost_event, 0, false) };
        waited == WAIT_OBJECT_0
    }

    /// Shows the drawn slot and advances the ring.
    pub fn present_next(&mut self, d3d_context: &ID3D11DeviceContext) -> Result<()> {
        let slot = self.next_slot().ok_or_else(windows::core::Error::empty)?;
        unsafe {
            self.surface.SetBuffer(&slot.buffer)?;
            // The manager tracks submitted work; make sure the frame is submitted.
            d3d_context.Flush();
            self.manager.Present()?;
        }
        self.next_buffer_index = (self.next_buffer_index + 1) % self.buffers.len().max(1);
        Ok(())
    }
}
