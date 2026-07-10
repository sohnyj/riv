//! 클립보드 — 복사(CF_HDROP + "PNG" + CF_DIBV5)·붙여넣기(CF_HDROP만)
//! (SPEC §6.4, PORTING_PLAN §3 매핑). 회전/미러/플립은 GPU 전용 상태이므로
//! 복사 시 픽셀에 현재 방향을 bake한다(화면과 동일).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPV5HEADER};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::core::{Result, w};

/// 화면 방향 bake용 파라미터 (SPEC §6.4 복사 규칙)
pub struct BakedOrientation {
    pub rotation_quadrant: u32,
    pub mirrored: bool,
    pub flipped: bool,
}

/// 복사: 파일 경로(CF_HDROP) + "PNG" + CF_DIBV5 (SPEC §6.4).
/// `pixels` = premultiplied BGRA(프레임 0), bake·언프리멀티플라이는 내부 수행.
pub fn copy_image(
    window: HWND,
    path: &Path,
    pixels: &[u8],
    width: u32,
    height: u32,
    orientation: &BakedOrientation,
) -> Result<()> {
    let (baked, baked_width, baked_height) = bake_orientation(pixels, width, height, orientation);
    let straight = unpremultiply(&baked);

    unsafe {
        OpenClipboard(Some(window))?;
    }
    let result = (|| -> Result<()> {
        unsafe { EmptyClipboard()? };
        set_drop_paths(path)?;
        set_png(&straight, baked_width, baked_height)?;
        set_dib_v5(&straight, baked_width, baked_height)?;
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}

/// 붙여넣기: CF_HDROP 경로 목록만 — URL 텍스트 무시 (SPEC §6.4)
pub fn paste_paths(window: HWND) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if unsafe { OpenClipboard(Some(window)) }.is_err() {
        return paths;
    }
    if let Ok(handle) = unsafe { GetClipboardData(CF_HDROP.0.into()) } {
        let drop_handle = HDROP(handle.0);
        let count = unsafe { DragQueryFileW(drop_handle, u32::MAX, None) };
        for index in 0..count {
            let mut buffer = [0u16; 32768];
            let length = unsafe { DragQueryFileW(drop_handle, index, Some(&mut buffer)) };
            if length > 0 {
                paths.push(PathBuf::from(String::from_utf16_lossy(
                    &buffer[..length as usize],
                )));
            }
        }
    }
    let _ = unsafe { CloseClipboard() };
    paths
}

/// CF_HDROP: DROPFILES 헤더 + 이중 널 종단 와이드 경로 목록
fn set_drop_paths(path: &Path) -> Result<()> {
    // DROPFILES: pFiles(오프셋)=20, pt, fNC, fWide=1 (shellapi 레이아웃)
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(&20u32.to_le_bytes());
    payload.extend_from_slice(&[0u8; 8]); // POINT pt
    payload.extend_from_slice(&0u32.to_le_bytes()); // fNC
    payload.extend_from_slice(&1u32.to_le_bytes()); // fWide
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0u16, 0u16]).collect();
    payload.extend_from_slice(unsafe {
        std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2)
    });
    let global = copy_to_global(&payload)?;
    unsafe { SetClipboardData(CF_HDROP.0.into(), Some(HANDLE(global.0)))? };
    Ok(())
}

/// "PNG" 포맷 — WIC PNG 인코더로 메모리 스트림에 인코딩 (클립보드는 뷰 동작, R7 예외)
fn set_png(straight_bgra: &[u8], width: u32, height: u32) -> Result<()> {
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = factory.CreateBitmapFromMemory(
            width,
            height,
            &GUID_WICPixelFormat32bppBGRA,
            width * 4,
            straight_bgra,
        )?;
        // 소유권을 클립보드로 넘길 HGLOBAL 기반 스트림 (fdeleteonrelease=false)
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), false)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.expect("encoder frame");
        frame.Initialize(None)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
        let global = GetHGlobalFromStream(&stream)?;
        let format = RegisterClipboardFormatW(w!("PNG"));
        SetClipboardData(format, Some(HANDLE(global.0)))?;
    }
    Ok(())
}

/// CF_DIBV5 — 32bpp straight 알파, bottom-up (SPEC §6.4)
fn set_dib_v5(straight_bgra: &[u8], width: u32, height: u32) -> Result<()> {
    let mut header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: width as i32,
        bV5Height: height as i32, // 양수 = bottom-up
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_RGB,
        bV5SizeImage: width * height * 4,
        bV5AlphaMask: 0xFF00_0000,
        ..Default::default()
    };
    header.bV5CSType = 0x7352_4742; // 'sRGB' (LCS_sRGB)
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const header).cast::<u8>(),
            size_of::<BITMAPV5HEADER>(),
        )
    };
    let mut payload = Vec::with_capacity(header_bytes.len() + straight_bgra.len());
    payload.extend_from_slice(header_bytes);
    // bottom-up 행 순서
    let stride = width as usize * 4;
    for row in (0..height as usize).rev() {
        payload.extend_from_slice(&straight_bgra[row * stride..(row + 1) * stride]);
    }
    let global = copy_to_global(&payload)?;
    // CF_DIBV5 = 17
    unsafe { SetClipboardData(17, Some(HANDLE(global.0)))? };
    Ok(())
}

fn copy_to_global(payload: &[u8]) -> Result<HGLOBAL> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, payload.len())?;
        let destination = GlobalLock(global);
        std::ptr::copy_nonoverlapping(payload.as_ptr(), destination.cast(), payload.len());
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

/// 현재 방향 bake — 사분면 회전 → mirror/flip (화면 행렬과 동일 순서, SPEC §6.4)
fn bake_orientation(
    pixels: &[u8],
    width: u32,
    height: u32,
    orientation: &BakedOrientation,
) -> (Vec<u8>, u32, u32) {
    let (w, h) = (width as usize, height as usize);
    let swapped = !orientation.rotation_quadrant.is_multiple_of(2);
    let (out_w, out_h) = if swapped { (h, w) } else { (w, h) };
    let mut output = vec![0u8; pixels.len()];
    for y in 0..h {
        for x in 0..w {
            // 화면 행렬과 동일 순서: mirror/flip(소스 축) → 사분면 회전
            let mirrored_x = if orientation.mirrored { w - 1 - x } else { x };
            let mirrored_y = if orientation.flipped { h - 1 - y } else { y };
            let (tx, ty) = match orientation.rotation_quadrant {
                1 => (h - 1 - mirrored_y, mirrored_x), // 90° CW
                2 => (w - 1 - mirrored_x, h - 1 - mirrored_y),
                3 => (mirrored_y, w - 1 - mirrored_x), // 270° CW
                _ => (mirrored_x, mirrored_y),
            };
            let source = (y * w + x) * 4;
            let destination = (ty * out_w + tx) * 4;
            output[destination..destination + 4].copy_from_slice(&pixels[source..source + 4]);
        }
    }
    (output, out_w as u32, out_h as u32)
}

/// premultiplied → straight 알파 (클립보드 포맷 관례)
fn unpremultiply(pixels: &[u8]) -> Vec<u8> {
    let mut output = pixels.to_vec();
    for pixel in output.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha > 0 && alpha < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    output
}
