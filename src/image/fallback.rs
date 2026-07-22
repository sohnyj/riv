//! Static C codec adapters: animated WebP, EXR, and HEIF fallback.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use super::decode::{
    DecodeError, DecodedImage, Frame, PixelStorage, blend_over, clear_rectangle, copy_rectangle,
    peak_luminance_from_half_pixels, uncoded_error,
};

/// Must match the built libwebpdemux ABI or WebPDemuxInternal returns null.
const WEBP_DEMUX_ABI_VERSION: c_int = 0x0107;
const WEBP_FF_CANVAS_WIDTH: c_int = 1;
const WEBP_FF_CANVAS_HEIGHT: c_int = 2;
const WEBP_MUX_DISPOSE_BACKGROUND: c_int = 1;
const WEBP_MUX_NO_BLEND: c_int = 1;

#[repr(C)]
struct WebPData {
    bytes: *const u8,
    size: usize,
}

enum WebPDemuxer {}

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

pub fn decode_webp_animation(
    data: &[u8],
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    let webp_data = WebPData {
        bytes: data.as_ptr(),
        size: data.len(),
    };
    let demuxer = unsafe {
        WebPDemuxInternal(
            &raw const webp_data,
            0,
            std::ptr::null_mut(),
            WEBP_DEMUX_ABI_VERSION,
        )
    };
    if demuxer.is_null() {
        return Err(uncoded_error("WebP demux failed"));
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
        return Err(uncoded_error("WebP canvas has no size"));
    }
    let mut iterator: WebPIterator = unsafe { std::mem::zeroed() };
    if unsafe { WebPDemuxGetFrame(demuxer, 1, &raw mut iterator) } == 0 {
        return Err(uncoded_error("WebP has no frames"));
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
            unsafe { WebPDemuxReleaseIterator(&raw mut iterator) };
            return Err(uncoded_error("WebP frame decode failed"));
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
        if unsafe { WebPDemuxNextFrame(&raw mut iterator) } == 0 {
            break;
        }
    }
    unsafe { WebPDemuxReleaseIterator(&raw mut iterator) };
    Ok(DecodedImage {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        format_name,
        icc_profile: None,
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: 8,
        peak_luminance_nits: None,
        bright_coverage: None,
        frames,
    })
}

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

unsafe extern "C" {
    fn riv_exr_decode(
        path: *const u16,
        out_width: *mut c_int,
        out_height: *mut c_int,
        out_pixels: *mut *mut u16,
        error_message: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn riv_exr_decode_memory(
        data: *const u8,
        size: usize,
        out_width: *mut c_int,
        out_height: *mut c_int,
        out_pixels: *mut *mut u16,
        error_message: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn riv_exr_free(pixels: *mut u16);
}

pub fn decode_exr(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    decode_exr_with(
        format_name,
        |width, height, pixels, message, capacity| unsafe {
            riv_exr_decode(wide_path.as_ptr(), width, height, pixels, message, capacity)
        },
    )
}

pub fn decode_exr_bytes(
    data: &[u8],
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    decode_exr_with(
        format_name,
        |width, height, pixels, message, capacity| unsafe {
            riv_exr_decode_memory(
                data.as_ptr(),
                data.len(),
                width,
                height,
                pixels,
                message,
                capacity,
            )
        },
    )
}

fn decode_exr_with(
    format_name: &'static str,
    decode: impl FnOnce(*mut c_int, *mut c_int, *mut *mut u16, *mut c_char, usize) -> c_int,
) -> Result<DecodedImage, DecodeError> {
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let mut half_pixels: *mut u16 = std::ptr::null_mut();
    let mut error_message = [0u8; 256];
    let status = decode(
        &raw mut width,
        &raw mut height,
        &raw mut half_pixels,
        error_message.as_mut_ptr().cast(),
        error_message.len(),
    );
    if status != 0 {
        let text = CStr::from_bytes_until_nul(&error_message)
            .map_or("EXR decode failed", |message| {
                message.to_str().unwrap_or("EXR decode failed")
            });
        return Err(uncoded_error(text));
    }
    let byte_count = width as usize * height as usize * 8;
    // Fallible copy: to_vec aborts on OOM; a huge EXR should error, not crash.
    // The shim hands over associated-alpha linear RGBA halves - the FP16 storage layout.
    let mut pixels = Vec::new();
    let reserved = pixels.try_reserve_exact(byte_count).is_ok();
    if reserved {
        pixels.extend_from_slice(unsafe {
            std::slice::from_raw_parts(half_pixels.cast::<u8>(), byte_count)
        });
    }
    unsafe { riv_exr_free(half_pixels) };
    if !reserved {
        return Err(uncoded_error("EXR is too large to fit in memory"));
    }
    let peak_stats = peak_luminance_from_half_pixels(&pixels);
    let peak_luminance_nits = peak_stats.as_ref().map(|stats| stats.peak_nits);
    let bright_coverage = peak_stats.as_ref().map(|stats| stats.bright_coverage);
    Ok(DecodedImage {
        width: width as u32,
        height: height as u32,
        pixel_width: width as u32,
        pixel_height: height as u32,
        format_name,
        icc_profile: None,
        exif: None,
        storage: PixelStorage::RgbaHalf,
        source_bits_per_channel: 16,
        peak_luminance_nits,
        bright_coverage,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}

const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;

enum HeifContext {}
enum HeifImageHandle {}
enum HeifImage {}

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
        Err(uncoded_error(text))
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

pub fn decode_heif(data: &[u8], format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return Err(uncoded_error("HEIF context allocation failed"));
    }
    let result = decode_heif_primary_image(context, data, format_name);
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
    unsafe { heif_context_get_primary_image_handle(context, &raw mut handle) }.into_result()?;

    let mut image: *mut HeifImage = std::ptr::null_mut();
    let decode_result = unsafe {
        heif_decode_image(
            handle,
            &raw mut image,
            HEIF_COLORSPACE_RGB,
            HEIF_CHROMA_INTERLEAVED_RGBA,
            std::ptr::null(),
        )
    }
    .into_result();
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
        unsafe { heif_image_get_plane_readonly(image, HEIF_CHANNEL_INTERLEAVED, &raw mut stride) };
    if plane.is_null() || width <= 0 || height <= 0 || i64::from(stride) < i64::from(width) * 4 {
        unsafe { heif_image_release(image) };
        return Err(uncoded_error("HEIF image plane unavailable"));
    }
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let row_pointer = unsafe { plane.add(row * stride as usize) };
        let row_pixels = unsafe { std::slice::from_raw_parts(row_pointer, width as usize * 4) };
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
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: 8,
        peak_luminance_nits: None,
        bright_coverage: None,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}

/// A crafted near-SIZE_MAX scanline offset must error, not read out of bounds (fixtures: SECURITY_AUDIT.md).
#[cfg(test)]
mod exr_robustness_tests {
    use super::*;

    #[test]
    #[ignore = "needs test/exr_base.exr"]
    fn a_valid_exr_decodes() {
        let data = std::fs::read("test/exr_base.exr").expect("fixture");
        assert!(decode_exr_bytes(&data, "EXR").is_ok());
    }

    #[test]
    #[ignore = "needs test/exr_bad_offset.exr"]
    fn a_corrupt_offset_table_errors_without_reading_out_of_bounds() {
        let data = std::fs::read("test/exr_bad_offset.exr").expect("fixture");
        // The subtraction bound check must catch it, not wrap and read before the buffer.
        assert!(decode_exr_bytes(&data, "EXR").is_err());
    }
}
