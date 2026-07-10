//! D3D11 디바이스 + DXGI 플립 스왑체인 + D2D 드로우 경로
//! (SPEC §3.1·§3.3, PORTING_PLAN §3 렌더러 세부 — 커스텀 셰이더 0)

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_INTERPOLATION_MODE, D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice,
    IDXGIFactory2, IDXGIOutput6, IDXGISurface, IDXGISwapChain1,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::core::{Interface, Result};
use windows_numerics::Matrix3x2;

pub struct Renderer {
    swap_chain: IDXGISwapChain1,
    d2d_context: ID2D1DeviceContext,
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    backbuffer_format: DXGI_FORMAT,
}

/// 창이 있는 모니터의 비트 심도를 감지해 백버퍼 포맷을 결정 —
/// 10-bit 디스플레이면 R10G10B10A2, 그 외·감지 실패는 8-bit(B8G8R8A8).
/// 10-bit 경로는 실기 검증 대상(wine은 IDXGIOutput6 미지원 가능).
fn detect_backbuffer_format(adapter: &IDXGIAdapter, window: HWND) -> DXGI_FORMAT {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut output_index = 0;
    while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
        output_index += 1;
        let Ok(description) = (unsafe { output.GetDesc() }) else {
            continue;
        };
        if description.Monitor != monitor {
            continue;
        }
        if let Ok(output6) = output.cast::<IDXGIOutput6>()
            && let Ok(description1) = unsafe { output6.GetDesc1() }
            && description1.BitsPerColor >= 10
        {
            return DXGI_FORMAT_R10G10B10A2_UNORM;
        }
        break;
    }
    DXGI_FORMAT_B8G8R8A8_UNORM
}

fn create_d3d_device(driver_type: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    Ok(device.expect("D3D11CreateDevice succeeded without device"))
}

fn pixel_format() -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
}

impl Renderer {
    pub fn new(window: HWND, width: u32, height: u32) -> Result<Self> {
        // WARP 폴백은 런타임 위임(P7) — 하드웨어 실패 시 1회 재시도만
        let d3d_device = create_d3d_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_d3d_device(D3D_DRIVER_TYPE_WARP))?;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;

        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter()? };
        let backbuffer_format = detect_backbuffer_format(&adapter, window);
        let swap_chain = unsafe {
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let description = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: backbuffer_format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            factory.CreateSwapChainForHwnd(&d3d_device, window, &description, None, None)?
        };

        let d2d_context = unsafe {
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };

        let mut renderer = Self {
            swap_chain,
            d2d_context,
            target: None,
            image: None,
            backbuffer_format,
        };
        renderer.create_target()?;
        Ok(renderer)
    }

    fn create_target(&mut self) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: self.backbuffer_format,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        unsafe {
            let surface: IDXGISurface = self.swap_chain.GetBuffer(0)?;
            let target = self
                .d2d_context
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))?;
            self.d2d_context.SetTarget(&target);
            self.target = Some(target);
        }
        Ok(())
    }

    /// WM_SIZE에서 동기 호출 — 백버퍼 재생성 (호출자가 즉시 재렌더)
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe {
            self.d2d_context.SetTarget(None);
            self.target = None;
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.create_target()
    }

    /// premultiplied BGRA8 픽셀 업로드 (SPEC §3.1)
    pub fn set_image(&mut self, pixels: &[u8], width: u32, height: u32) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: pixel_format(),
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            ..Default::default()
        };
        let bitmap = unsafe {
            self.d2d_context.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr().cast()),
                width * 4,
                &properties,
            )?
        };
        self.image = Some(bitmap);
        Ok(())
    }

    /// Clear → SetTransform → DrawBitmap → Present (SPEC §3.1)
    pub fn render(
        &mut self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
        clear_color: D2D1_COLOR_F,
    ) -> Result<()> {
        unsafe {
            self.d2d_context.BeginDraw();
            self.d2d_context.Clear(Some(&clear_color));
            if let Some(image) = &self.image {
                let transform = Matrix3x2 {
                    M11: matrix[0],
                    M12: matrix[1],
                    M21: matrix[2],
                    M22: matrix[3],
                    M31: matrix[4],
                    M32: matrix[5],
                };
                self.d2d_context.SetTransform(&transform);
                self.d2d_context
                    .DrawBitmap(image, None, 1.0, interpolation, None, None);
                self.d2d_context.SetTransform(&Matrix3x2::identity());
            }
            self.d2d_context.EndDraw(None, None)?;
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()
        }
    }
}
