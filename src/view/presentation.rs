//! Presentation-manager buffers composed by DWM, never promoted to independent flip.

use windows::Win32::Foundation::{HANDLE, HWND, RECT, WAIT_OBJECT_0};
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
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_UNSUPPORTED, IDXGIDevice};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::WindowsAndMessaging::{WINDOW_EX_STYLE, WS_EX_NOREDIRECTIONBITMAP};
use windows::core::{GUID, HRESULT, IUnknown, Interface, Owned, Result, s, w};

/// The presentation factory refuses devices created without this flag.
pub const REQUIRED_DEVICE_FLAG: D3D11_CREATE_DEVICE_FLAG =
    D3D11_CREATE_DEVICE_PREVENT_INTERNAL_THREADING_OPTIMIZATIONS;

/// Composed content is all the client shows, so the window needs no surface of its own.
pub const REQUIRED_WINDOW_STYLE: WINDOW_EX_STYLE = WS_EX_NOREDIRECTIONBITMAP;

pub struct BufferSlot {
    pub buffer: IPresentationBuffer,
    pub texture: ID3D11Texture2D,
    available_event: Owned<HANDLE>,
    /// D2D draws here when the pass is absent; None when the quantize pass writes the buffer.
    pub d2d_target: Option<ID2D1Bitmap1>,
    pub render_target_view: Option<ID3D11RenderTargetView>,
}

pub struct CompositionPresenter {
    // Declared first: the buffers drop before the manager and the handles they came from.
    buffers: Vec<BufferSlot>,
    next_buffer_index: usize,
    /// Format, size, and count of the current ring, so an unchanged target skips reallocation.
    allocated_ring: Option<(DXGI_FORMAT, (u32, u32), usize)>,
    manager: IPresentationManager,
    surface: IPresentationSurface,
    lost_event: Owned<HANDLE>,
    /// Held for the surface's lifetime; nothing reads it after creation.
    _surface_handle: Owned<HANDLE>,
    _composition_device: IDCompositionDevice,
    _composition_target: IDCompositionTarget,
    _composition_visual: IDCompositionVisual,
    _composition_content: IUnknown,
}

/// Resolved at run time: wine's dcomp.dll lacks the export and the test executable must load there.
fn create_presentation_factory(d3d_device: &ID3D11Device) -> Result<IPresentationFactory> {
    type CreatePresentationFactoryFunction = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *const GUID,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT;
    // Resolved once: every renderer rebuild makes a presenter, and the module never unloads.
    static ENTRY_POINT: std::sync::OnceLock<Option<CreatePresentationFactoryFunction>> =
        std::sync::OnceLock::new();
    let create = (*ENTRY_POINT.get_or_init(|| {
        let module = unsafe { LoadLibraryW(w!("dcomp.dll")) }.ok()?;
        let address = unsafe { GetProcAddress(module, s!("CreatePresentationFactory")) }?;
        Some(unsafe {
            std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                CreatePresentationFactoryFunction,
            >(address)
        })
    }))
    .ok_or_else(unsupported)?;
    let mut pointer: *mut core::ffi::c_void = core::ptr::null_mut();
    unsafe {
        create(
            d3d_device.as_raw(),
            &IPresentationFactory::IID,
            &raw mut pointer,
        )
    }
    .ok()?;
    Ok(unsafe { IPresentationFactory::from_raw(pointer) })
}

/// The system cannot present this way, and the renderer has no other way.
fn unsupported() -> windows::core::Error {
    windows::core::Error::from_hresult(DXGI_ERROR_UNSUPPORTED)
}

/// The DirectComposition tree that shows the surface in the window, committed once.
fn build_visual_tree(
    d3d_device: &ID3D11Device,
    window: HWND,
    surface_handle: HANDLE,
) -> Result<(
    IDCompositionDevice,
    IDCompositionTarget,
    IDCompositionVisual,
    IUnknown,
)> {
    let dxgi_device: IDXGIDevice = d3d_device.cast()?;
    let composition_device: IDCompositionDevice =
        unsafe { DCompositionCreateDevice(&dxgi_device) }?;
    let composition_target = unsafe { composition_device.CreateTargetForHwnd(window, true) }?;
    let composition_visual = unsafe { composition_device.CreateVisual() }?;
    let composition_content =
        unsafe { composition_device.CreateSurfaceFromHandle(surface_handle) }?;
    unsafe { composition_visual.SetContent(&composition_content) }?;
    unsafe { composition_target.SetRoot(&composition_visual) }?;
    unsafe { composition_device.Commit() }?;
    Ok((
        composition_device,
        composition_target,
        composition_visual,
        composition_content,
    ))
}

impl CompositionPresenter {
    pub fn new(d3d_device: &ID3D11Device, window: HWND) -> Result<Self> {
        let factory = create_presentation_factory(d3d_device)?;
        if unsafe { factory.IsPresentationSupported() } == 0 {
            return Err(unsupported());
        }
        Self::bind(&factory, d3d_device, window)
    }

    fn bind(
        factory: &IPresentationFactory,
        d3d_device: &ID3D11Device,
        window: HWND,
    ) -> Result<Self> {
        // dcomp.h COMPOSITIONOBJECT_ALL_ACCESS.
        let access = (COMPOSITIONOBJECT_READ | COMPOSITIONOBJECT_WRITE) as u32;
        let manager = unsafe { factory.CreatePresentationManager() }?;
        let lost_event = unsafe { Owned::new(manager.GetLostEvent()?) };
        let surface_handle = unsafe { Owned::new(DCompositionCreateSurfaceHandle(access, None)?) };
        let surface = unsafe { manager.CreatePresentationSurface(*surface_handle) }?;
        unsafe { surface.SetAlphaMode(DXGI_ALPHA_MODE_IGNORE) }?;
        let (device, target, visual, content) =
            build_visual_tree(d3d_device, window, *surface_handle)?;
        Ok(Self {
            buffers: Vec::new(),
            next_buffer_index: 0,
            allocated_ring: None,
            manager,
            surface,
            lost_event,
            _surface_handle: surface_handle,
            _composition_device: device,
            _composition_target: target,
            _composition_visual: visual,
            _composition_content: content,
        })
    }

    pub fn set_color_space(&self, color_space: DXGI_COLOR_SPACE_TYPE) -> Result<()> {
        unsafe { self.surface.SetColorSpace(color_space) }
    }

    pub fn ensure_buffers(
        &mut self,
        d3d_device: &ID3D11Device,
        format: DXGI_FORMAT,
        size: (u32, u32),
        count: usize,
    ) -> Result<()> {
        if self.allocated_ring == Some((format, size, count)) {
            return Ok(());
        }
        self.allocated_ring = None;
        self.allocate_buffers(d3d_device, format, size, count)?;
        self.allocated_ring = Some((format, size, count));
        Ok(())
    }

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
            let available_event = unsafe { Owned::new(buffer.GetAvailableEvent()?) };
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

    pub fn next_slot(&self) -> Option<&BufferSlot> {
        self.buffers.get(self.next_buffer_index)
    }

    /// The event the pump waits on before the next frame; signaled while the buffer is free.
    pub fn next_available_event(&self) -> Option<HANDLE> {
        self.next_slot().map(|slot| *slot.available_event)
    }

    /// The composition system dropped this manager; the renderer must be rebuilt.
    pub fn is_lost(&self) -> bool {
        let waited = unsafe { WaitForSingleObjectEx(*self.lost_event, 0, false) };
        waited == WAIT_OBJECT_0
    }

    pub fn present_next(&mut self, d3d_context: &ID3D11DeviceContext) -> Result<()> {
        let slot = self.next_slot().ok_or_else(windows::core::Error::empty)?;
        unsafe {
            self.surface.SetBuffer(&slot.buffer)?;
            // The manager tracks submitted work; make sure the frame is submitted.
            d3d_context.Flush();
            self.manager.Present()?;
        }
        self.next_buffer_index = (self.next_buffer_index + 1) % self.buffers.len();
        Ok(())
    }
}
