//! C 정적 fallback 코덱 어댑터 (PORTING_PLAN §5) — 애니 WebP(libwebp+libwebpdemux)·
//! EXR(OpenEXR C++ 심)·HEIF(libheif+libde265). 정적 라이브러리는
//! `deps/build_deps.sh` 산출물이며 build.rs가 링크한다.
//!
//! FFI 선언은 각 라이브러리의 공개 C 헤더와 1:1 — 구조체 레이아웃·enum 값·ABI 버전은
//! 빌드 시점 소스(deps/sources)에서 확인해 고정했다. 출력은 전부 premultiplied
//! BGRA8 (SPEC §3.1).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use super::decode::{
    DecodeError, DecodedImage, Frame, blend_over, clear_rectangle, copy_rectangle, fallback_error,
};

// ── 애니메이션 WebP — libwebp + libwebpdemux (SPEC §10, WIC 애니 미지원 확인) ──

/// demux.h `WEBP_DEMUX_ABI_VERSION` — 빌드된 라이브러리와 불일치 시 WebPDemuxInternal이
/// NULL을 반환한다 (런타임 안전 검증)
const WEBP_DEMUX_ABI_VERSION: c_int = 0x0107;
/// demux.h `WebPFormatFeature`
const WEBP_FF_CANVAS_WIDTH: c_int = 1;
const WEBP_FF_CANVAS_HEIGHT: c_int = 2;
/// mux_types.h `WebPMuxAnimDispose`
const WEBP_MUX_DISPOSE_BACKGROUND: c_int = 1;
/// mux_types.h `WebPMuxAnimBlend`
const WEBP_MUX_NO_BLEND: c_int = 1;

#[repr(C)]
struct WebPData {
    bytes: *const u8,
    size: usize,
}

enum WebPDemuxer {}

/// demux.h `struct WebPIterator` (ABI 0x0107)
#[repr(C)]
struct WebPIterator {
    frame_number: c_int,
    frame_count: c_int,
    x_offset: c_int,
    y_offset: c_int,
    width: c_int,
    height: c_int,
    duration_milliseconds: c_int,
    dispose_method: c_int,
    complete: c_int,
    fragment: WebPData,
    has_alpha: c_int,
    blend_method: c_int,
    padding: [u32; 2],
    private_data: *mut c_void,
}

unsafe extern "C" {
    fn WebPDemuxInternal(
        data: *const WebPData,
        allow_partial: c_int,
        state: *mut c_void,
        version: c_int,
    ) -> *mut WebPDemuxer;
    fn WebPDemuxDelete(demuxer: *mut WebPDemuxer);
    fn WebPDemuxGetI(demuxer: *const WebPDemuxer, feature: c_int) -> u32;
    fn WebPDemuxGetFrame(
        demuxer: *const WebPDemuxer,
        frame_number: c_int,
        iterator: *mut WebPIterator,
    ) -> c_int;
    fn WebPDemuxNextFrame(iterator: *mut WebPIterator) -> c_int;
    fn WebPDemuxReleaseIterator(iterator: *mut WebPIterator);
    fn WebPDecodeBGRAInto(
        data: *const u8,
        data_size: usize,
        output_buffer: *mut u8,
        output_buffer_size: usize,
        output_stride: c_int,
    ) -> *mut u8;
}

/// 애니메이션 WebP 디코드 — ANMF 프레임을 blend/dispose 규칙으로 캔버스에 합성
/// (GIF·APNG와 동일 모델). 호출 전 VP8X ANIM 플래그 프로빙으로 분기된다.
pub fn decode_webp_animation(
    path: &Path,
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(fallback_error)?;
    let webp_data = WebPData {
        bytes: data.as_ptr(),
        size: data.len(),
    };
    let demuxer =
        unsafe { WebPDemuxInternal(&webp_data, 0, std::ptr::null_mut(), WEBP_DEMUX_ABI_VERSION) };
    if demuxer.is_null() {
        return Err(fallback_error("WebP demux failed"));
    }
    let result = compose_webp_frames(demuxer, format_name);
    unsafe { WebPDemuxDelete(demuxer) };
    result
}

fn compose_webp_frames(
    demuxer: *mut WebPDemuxer,
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    let canvas_width = unsafe { WebPDemuxGetI(demuxer, WEBP_FF_CANVAS_WIDTH) };
    let canvas_height = unsafe { WebPDemuxGetI(demuxer, WEBP_FF_CANVAS_HEIGHT) };
    if canvas_width == 0 || canvas_height == 0 {
        return Err(fallback_error("WebP canvas has no size"));
    }
    let mut iterator: WebPIterator = unsafe { std::mem::zeroed() };
    if unsafe { WebPDemuxGetFrame(demuxer, 1, &mut iterator) } == 0 {
        return Err(fallback_error("WebP has no frames"));
    }
    let mut canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
    let mut frames = Vec::with_capacity(iterator.frame_count.max(1) as usize);
    loop {
        let frame_width = iterator.width.max(0) as u32;
        let frame_height = iterator.height.max(0) as u32;
        let mut frame_pixels = vec![0u8; frame_width as usize * frame_height as usize * 4];
        let decoded = unsafe {
            WebPDecodeBGRAInto(
                iterator.fragment.bytes,
                iterator.fragment.size,
                frame_pixels.as_mut_ptr(),
                frame_pixels.len(),
                frame_width as c_int * 4,
            )
        };
        if decoded.is_null() {
            unsafe { WebPDemuxReleaseIterator(&mut iterator) };
            return Err(fallback_error("WebP frame decode failed"));
        }
        premultiply_bgra_in_place(&mut frame_pixels);
        if iterator.blend_method == WEBP_MUX_NO_BLEND {
            copy_rectangle(
                &mut canvas,
                canvas_width,
                canvas_height,
                &frame_pixels,
                frame_width,
                frame_height,
                iterator.x_offset.max(0) as u32,
                iterator.y_offset.max(0) as u32,
            );
        } else {
            blend_over(
                &mut canvas,
                canvas_width,
                canvas_height,
                &frame_pixels,
                frame_width,
                frame_height,
                iterator.x_offset.max(0) as u32,
                iterator.y_offset.max(0) as u32,
            );
        }
        let duration = iterator.duration_milliseconds;
        frames.push(Frame {
            pixels: canvas.clone(),
            delay_milliseconds: if duration > 0 { duration as u32 } else { 100 },
        });
        if iterator.dispose_method == WEBP_MUX_DISPOSE_BACKGROUND {
            clear_rectangle(
                &mut canvas,
                canvas_width,
                iterator.x_offset.max(0) as u32,
                iterator.y_offset.max(0) as u32,
                frame_width,
                frame_height,
            );
        }
        if unsafe { WebPDemuxNextFrame(&mut iterator) } == 0 {
            break;
        }
    }
    unsafe { WebPDemuxReleaseIterator(&mut iterator) };
    Ok(DecodedImage {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        format_name,
        icc_profile: None,
        frames,
    })
}

/// 스트레이트 BGRA → premultiplied (SPEC §3.1)
fn premultiply_bgra_in_place(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        let (color_channels, _) = pixel.split_at_mut(3);
        for channel in color_channels {
            *channel = (u16::from(*channel) * alpha / 255) as u8;
        }
    }
}

// ── EXR — OpenEXR C++ 심 (deps/shim, SPEC §10 성능 우선 선택) ────────────

unsafe extern "C" {
    fn riv_exr_decode(
        path: *const u16,
        out_width: *mut c_int,
        out_height: *mut c_int,
        out_pixels: *mut *mut u16,
        error_message: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn riv_exr_free(pixels: *mut u16);
}

/// EXR 디코드 — 심이 준 linear RGBA half를 SDR로 톤 다운(클램프 + sRGB 인코딩,
/// SPEC §10 "HDR→SDR 톤 다운" 1차 구현) 후 premultiplied BGRA8로 변환.
/// EXR 관례상 픽셀은 associated(premultiplied linear) — 인코딩 전 un-premultiply.
pub fn decode_exr(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let mut half_pixels: *mut u16 = std::ptr::null_mut();
    let mut error_message = [0u8; 256];
    let status = unsafe {
        riv_exr_decode(
            wide_path.as_ptr(),
            &mut width,
            &mut height,
            &mut half_pixels,
            error_message.as_mut_ptr().cast(),
            error_message.len(),
        )
    };
    if status != 0 {
        let text = CStr::from_bytes_until_nul(&error_message)
            .map_or("EXR decode failed", |message| {
                message.to_str().unwrap_or("EXR decode failed")
            });
        return Err(fallback_error(text));
    }
    let pixel_count = width as usize * height as usize;
    let halves = unsafe { std::slice::from_raw_parts(half_pixels, pixel_count * 4) };
    let mut pixels = Vec::with_capacity(pixel_count * 4);
    for pixel in halves.chunks_exact(4) {
        let alpha = half_to_float(pixel[3]).clamp(0.0, 1.0);
        let encode = |value: u16| {
            let mut linear = half_to_float(value);
            if alpha > 0.0 && alpha < 1.0 {
                linear /= alpha; // associated → straight
            }
            let encoded = linear_to_srgb(linear.clamp(0.0, 1.0));
            (encoded * alpha * 255.0 + 0.5) as u8 // 인코딩 후 재-premultiply
        };
        pixels.push(encode(pixel[2]));
        pixels.push(encode(pixel[1]));
        pixels.push(encode(pixel[0]));
        pixels.push((alpha * 255.0 + 0.5) as u8);
    }
    unsafe { riv_exr_free(half_pixels) };
    // 톤 다운 결과는 sRGB 인코딩 — 프로파일 없음(sRGB 가정 경로)
    Ok(DecodedImage {
        width: width as u32,
        height: height as u32,
        pixel_width: width as u32,
        pixel_height: height as u32,
        format_name,
        icc_profile: None,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}

/// IEEE 754 half → f32 (의존성 없는 비트 변환 — P3)
fn half_to_float(half: u16) -> f32 {
    let sign = u32::from(half >> 15);
    let exponent = u32::from((half >> 10) & 0x1F);
    let mantissa = u32::from(half & 0x3FF);
    let bits = if exponent == 0 {
        if mantissa == 0 {
            sign << 31
        } else {
            // 서브노멀 — 정규화
            let mut exponent = 113u32;
            let mut mantissa = mantissa;
            while mantissa & 0x400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            (sign << 31) | (exponent << 23) | ((mantissa & 0x3FF) << 13)
        }
    } else if exponent == 31 {
        (sign << 31) | (0xFF << 23) | (mantissa << 13)
    } else {
        (sign << 31) | ((exponent + 112) << 23) | (mantissa << 13)
    };
    f32::from_bits(bits)
}

fn linear_to_srgb(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

// ── HEIF — libheif + libde265 (WIC 부재 시 런타임 fallback, PORTING_PLAN §5) ───

/// heif_image.h enum 값
const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;

enum HeifContext {}
enum HeifImageHandle {}
enum HeifImage {}

/// heif_error.h `struct heif_error` — code 0 = 성공
#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

impl HeifError {
    fn into_result(self) -> Result<(), DecodeError> {
        if self.code == 0 {
            return Ok(());
        }
        let text = if self.message.is_null() {
            "HEIF decode failed".to_string()
        } else {
            unsafe { CStr::from_ptr(self.message) }
                .to_string_lossy()
                .into_owned()
        };
        Err(fallback_error(text))
    }
}

unsafe extern "C" {
    fn heif_context_alloc() -> *mut HeifContext;
    fn heif_context_free(context: *mut HeifContext);
    fn heif_context_read_from_memory_without_copy(
        context: *mut HeifContext,
        memory: *const c_void,
        size: usize,
        options: *const c_void,
    ) -> HeifError;
    fn heif_context_get_primary_image_handle(
        context: *mut HeifContext,
        handle: *mut *mut HeifImageHandle,
    ) -> HeifError;
    fn heif_image_handle_release(handle: *const HeifImageHandle);
    fn heif_decode_image(
        handle: *const HeifImageHandle,
        image: *mut *mut HeifImage,
        colorspace: c_int,
        chroma: c_int,
        options: *const c_void,
    ) -> HeifError;
    fn heif_image_release(image: *const HeifImage);
    fn heif_image_get_width(image: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_height(image: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_plane_readonly(
        image: *const HeifImage,
        channel: c_int,
        stride: *mut c_int,
    ) -> *const u8;
    fn heif_image_handle_get_raw_color_profile_size(handle: *const HeifImageHandle) -> usize;
    fn heif_image_handle_get_raw_color_profile(
        handle: *const HeifImageHandle,
        out_data: *mut c_void,
    ) -> HeifError;
}

/// HEIF 디코드 — irot/imir 등 변환은 libheif 기본 적용(옵션 NULL) (PORTING_PLAN §5)
pub fn decode_heif(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(fallback_error)?;
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return Err(fallback_error("HEIF context allocation failed"));
    }
    let result = decode_heif_primary_image(context, &data, format_name);
    unsafe { heif_context_free(context) };
    result
}

fn decode_heif_primary_image(
    context: *mut HeifContext,
    data: &[u8],
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    unsafe {
        heif_context_read_from_memory_without_copy(
            context,
            data.as_ptr().cast(),
            data.len(),
            std::ptr::null(),
        )
    }
    .into_result()?;
    let mut handle: *mut HeifImageHandle = std::ptr::null_mut();
    unsafe { heif_context_get_primary_image_handle(context, &mut handle) }.into_result()?;

    let mut image: *mut HeifImage = std::ptr::null_mut();
    let decode_result = unsafe {
        heif_decode_image(
            handle,
            &mut image,
            HEIF_COLORSPACE_RGB,
            HEIF_CHROMA_INTERLEAVED_RGBA,
            std::ptr::null(),
        )
    }
    .into_result();
    // ICC 바이트 — ColorManagement 이펙트 소스 (SPEC §7 "fallback 디코더는 ICC 바이트")
    let icc_profile = {
        let size = unsafe { heif_image_handle_get_raw_color_profile_size(handle) };
        if size > 0 {
            let mut buffer = vec![0u8; size];
            unsafe { heif_image_handle_get_raw_color_profile(handle, buffer.as_mut_ptr().cast()) }
                .into_result()
                .ok()
                .map(|()| buffer)
        } else {
            None
        }
    };
    unsafe { heif_image_handle_release(handle) };
    decode_result?;

    let width = unsafe { heif_image_get_width(image, HEIF_CHANNEL_INTERLEAVED) };
    let height = unsafe { heif_image_get_height(image, HEIF_CHANNEL_INTERLEAVED) };
    let mut stride: c_int = 0;
    let plane =
        unsafe { heif_image_get_plane_readonly(image, HEIF_CHANNEL_INTERLEAVED, &mut stride) };
    if plane.is_null() || width <= 0 || height <= 0 || stride < width * 4 {
        unsafe { heif_image_release(image) };
        return Err(fallback_error("HEIF image plane unavailable"));
    }
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let row_pointer = unsafe { plane.add(row * stride as usize) };
        let row_pixels = unsafe { std::slice::from_raw_parts(row_pointer, width as usize * 4) };
        // 스트레이트 RGBA → premultiplied BGRA
        for pixel in row_pixels.chunks_exact(4) {
            let alpha = u16::from(pixel[3]);
            pixels.push((u16::from(pixel[2]) * alpha / 255) as u8);
            pixels.push((u16::from(pixel[1]) * alpha / 255) as u8);
            pixels.push((u16::from(pixel[0]) * alpha / 255) as u8);
            pixels.push(pixel[3]);
        }
    }
    unsafe { heif_image_release(image) };
    Ok(DecodedImage {
        width: width as u32,
        height: height as u32,
        pixel_width: width as u32,
        pixel_height: height as u32,
        format_name,
        icc_profile,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}
