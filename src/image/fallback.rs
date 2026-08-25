//! Static C codec adapters: animated WebP, animated AVIF, EXR, and HEIF fallback.

use std::borrow::Cow;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::Arc;
use windows::core::HSTRING;

use crate::image::decode::{
    BGRA8_SOURCE_BITS, DEFAULT_FRAME_DELAY_MILLISECONDS, DecodeError, DecodedImage, Frame,
    FrameBlend, FrameCompositor, FrameDisposal, FrameRegion, HdrEncoding, MAXIMUM_HDR_SOURCE_BITS,
    PixelStorage, cicp_hdr_encoding, linearize_hdr_pixels, peak_luminance_from_half_pixels,
    peak_luminance_with_maximum_bits, premultiplied_bgra_from_rgba, try_zeroed_buffer,
    uncoded_error,
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

/// Composes only the first frame when maximum_frames is 1, for the animation two-stage path.
pub fn decode_webp_animation(
    bytes: &[u8],
    format_name: &'static str,
    maximum_frames: usize,
) -> Result<DecodedImage, DecodeError> {
    let webp_data = WebPData {
        bytes: bytes.as_ptr(),
        size: bytes.len(),
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
        return Err(uncoded_error("WebP animation couldn't be read"));
    }
    let decoded = compose_webp_frames(demuxer, format_name, maximum_frames);
    unsafe { WebPDemuxDelete(demuxer) };
    decoded
}

fn compose_webp_frames(
    demuxer: *mut WebPDemuxer,
    format_name: &'static str,
    maximum_frames: usize,
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
    let Some(mut compositor) = FrameCompositor::new(canvas_width, canvas_height) else {
        unsafe { WebPDemuxReleaseIterator(&raw mut iterator) };
        return Err(uncoded_error("WebP canvas is too large to decode"));
    };
    // Reused across frames; the decoder writes every byte it is given.
    let mut frame_pixels: Vec<u8> = Vec::new();
    loop {
        // The demuxer's frame count is real, so the budget is known after frame one.
        if !compositor.accepts_another(u64::from(iterator.frame_count.max(1) as u32)) {
            break;
        }
        let frame_width = iterator.width.max(0) as u32;
        let frame_height = iterator.height.max(0) as u32;
        let frame_bytes = frame_width as usize * frame_height as usize * 4;
        // Grown fallibly in place: vec-style growth would abort where an error must show.
        if frame_pixels
            .try_reserve_exact(frame_bytes.saturating_sub(frame_pixels.len()))
            .is_err()
        {
            unsafe { WebPDemuxReleaseIterator(&raw mut iterator) };
            return Err(uncoded_error("WebP is too large to fit in memory"));
        }
        frame_pixels.resize(frame_bytes, 0);
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
        let duration_milliseconds = iterator.duration_milliseconds;
        compositor.add_frame(FrameRegion {
            pixels: &frame_pixels,
            left: iterator.x_offset.max(0) as u32,
            top: iterator.y_offset.max(0) as u32,
            width: frame_width,
            height: frame_height,
            blend: if iterator.blend_method == WEBP_MUX_NO_BLEND {
                FrameBlend::Replace
            } else {
                FrameBlend::Over
            },
            // WebP disposes to the background or keeps; it has no restore-previous.
            disposal: if iterator.dispose_method == WEBP_MUX_DISPOSE_BACKGROUND {
                FrameDisposal::Background
            } else {
                FrameDisposal::Keep
            },
            delay_milliseconds: if duration_milliseconds > 0 {
                duration_milliseconds as u32
            } else {
                DEFAULT_FRAME_DELAY_MILLISECONDS
            },
        });
        if compositor.frames_so_far() >= maximum_frames
            || unsafe { WebPDemuxNextFrame(&raw mut iterator) } == 0
        {
            break;
        }
    }
    let (frames, frames_truncated) = compositor.finish();
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
        source_bits_per_channel: BGRA8_SOURCE_BITS,
        peak_luminance_nits: None,
        source_primaries: None,
        frames,
        frames_truncated,
        gain_map: None,
        gain_map_plane: None,
    })
}

fn premultiply_bgra_in_place(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        // Uniform four-lane multiply; the alpha lane's 255 factor leaves it unchanged.
        let multipliers = [pixel[3], pixel[3], pixel[3], 255];
        for (channel, multiplier) in pixel.iter_mut().zip(multipliers) {
            *channel = (u16::from(*channel) * u16::from(multiplier) / 255) as u8;
        }
    }
}

unsafe extern "C" {
    fn riv_exr_decode_into(
        path: *const u16,
        out_pixels: *mut u16,
        capacity_pixels: usize,
        out_width: *mut c_int,
        out_height: *mut c_int,
        error_message: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn riv_exr_decode_memory_into(
        data: *const u8,
        size: usize,
        out_pixels: *mut u16,
        capacity_pixels: usize,
        out_width: *mut c_int,
        out_height: *mut c_int,
        error_message: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn riv_exr_probe(path: *const u16, out_width: *mut c_int, out_height: *mut c_int) -> c_int;
    fn riv_exr_probe_memory(
        data: *const u8,
        size: usize,
        out_width: *mut c_int,
        out_height: *mut c_int,
    ) -> c_int;
}

/// Header-only data window size; the decode is always RGBA half.
pub fn probe_exr_dimensions(path: &Path) -> Option<(u32, u32)> {
    let wide_path = HSTRING::from(path);
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let status = unsafe { riv_exr_probe(wide_path.as_ptr(), &raw mut width, &raw mut height) };
    (status == 0).then_some((width as u32, height as u32))
}

pub fn probe_exr_bytes_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let status = unsafe {
        riv_exr_probe_memory(bytes.as_ptr(), bytes.len(), &raw mut width, &raw mut height)
    };
    (status == 0).then_some((width as u32, height as u32))
}

pub fn decode_exr(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let wide_path = HSTRING::from(path);
    decode_exr_with(
        format_name,
        probe_exr_dimensions(path),
        |pixels, capacity, width, height, message, error_capacity| unsafe {
            riv_exr_decode_into(
                wide_path.as_ptr(),
                pixels,
                capacity,
                width,
                height,
                message,
                error_capacity,
            )
        },
    )
}

pub fn decode_exr_bytes(
    bytes: &[u8],
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    decode_exr_with(
        format_name,
        probe_exr_bytes_dimensions(bytes),
        |pixels, capacity, width, height, message, error_capacity| unsafe {
            riv_exr_decode_memory_into(
                bytes.as_ptr(),
                bytes.len(),
                pixels,
                capacity,
                width,
                height,
                message,
                error_capacity,
            )
        },
    )
}

/// Size policy for the bundled decoders: past this the decode is refused instead of reserving for it.
const MAXIMUM_FALLBACK_PIXELS: usize = 1 << 30;

/// The probe sizes the buffer the shim decodes into, so no pixels are copied across the boundary.
fn decode_exr_with(
    format_name: &'static str,
    dimensions: Option<(u32, u32)>,
    decode: impl FnOnce(*mut u16, usize, *mut c_int, *mut c_int, *mut c_char, usize) -> c_int,
) -> Result<DecodedImage, DecodeError> {
    let Some((probed_width, probed_height)) = dimensions else {
        return Err(uncoded_error("EXR header is unreadable"));
    };
    let pixel_count = probed_width as usize * probed_height as usize;
    if pixel_count > MAXIMUM_FALLBACK_PIXELS {
        return Err(uncoded_error("EXR has too many pixels to decode"));
    }
    // The shim writes associated-alpha linear RGBA halves (the FP16 storage layout).
    let Some(mut pixels) = try_zeroed_buffer(pixel_count * 8) else {
        return Err(uncoded_error("EXR is too large to fit in memory"));
    };
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let mut error_message = [0u8; 256];
    let status = decode(
        pixels.as_mut_ptr().cast::<u16>(),
        pixel_count,
        &raw mut width,
        &raw mut height,
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
    // A file that shrank between the probe and the read leaves the tail untouched.
    pixels.truncate(width as usize * height as usize * 8);
    let peak_luminance_nits = peak_luminance_from_half_pixels(&pixels);
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
        // EXR chromaticities are ignored, so nothing states the primaries.
        source_primaries: None,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
        frames_truncated: false,
        gain_map: None,
        gain_map_plane: None,
    })
}

const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHROMA_INTERLEAVED_RRGGBBAA_LE: c_int = 15;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;
const HEIF_ERROR_CODE_END_OF_SEQUENCE: c_int = 13;

enum HeifContext {}
enum HeifImageHandle {}
enum HeifImage {}
enum HeifTrack {}
enum HeifDecodingOptions {}

#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

/// The colr box NCLX; the enum fields are C ints, so the layout has to match exactly.
#[repr(C)]
struct HeifColorProfileNclx {
    version: u8,
    color_primaries: c_int,
    transfer_characteristics: c_int,
    matrix_coefficients: c_int,
    full_range_flag: u8,
    color_primary_red_x: f32,
    color_primary_red_y: f32,
    color_primary_green_x: f32,
    color_primary_green_y: f32,
    color_primary_blue_x: f32,
    color_primary_blue_y: f32,
    color_primary_white_x: f32,
    color_primary_white_y: f32,
}

impl HeifError {
    fn into_result(self) -> Result<(), DecodeError> {
        if self.code == 0 {
            return Ok(());
        }
        let text = if self.message.is_null() {
            Cow::Borrowed("HEIF decode failed")
        } else {
            unsafe { CStr::from_ptr(self.message) }.to_string_lossy()
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
        options: *const HeifDecodingOptions,
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
        options: *const HeifDecodingOptions,
    ) -> HeifError;
    fn heif_image_release(image: *const HeifImage);
    fn heif_image_get_width(image: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_height(image: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_get_plane_readonly(
        image: *const HeifImage,
        channel: c_int,
        stride: *mut c_int,
    ) -> *const u8;
    fn heif_image_get_bits_per_pixel_range(image: *const HeifImage, channel: c_int) -> c_int;
    fn heif_image_handle_get_raw_color_profile_size(handle: *const HeifImageHandle) -> usize;
    fn heif_image_handle_get_raw_color_profile(
        handle: *const HeifImageHandle,
        out_data: *mut c_void,
    ) -> HeifError;
    fn heif_image_handle_get_nclx_color_profile(
        handle: *const HeifImageHandle,
        out_profile: *mut *mut HeifColorProfileNclx,
    ) -> HeifError;
    fn heif_nclx_color_profile_free(profile: *mut HeifColorProfileNclx);
    fn heif_image_handle_get_width(handle: *const HeifImageHandle) -> c_int;
    fn heif_image_handle_get_height(handle: *const HeifImageHandle) -> c_int;
    fn heif_context_get_track(context: *const HeifContext, id: u32) -> *mut HeifTrack;
    fn heif_track_release(track: *mut HeifTrack);
    fn heif_track_get_image_resolution(
        track: *const HeifTrack,
        out_width: *mut u16,
        out_height: *mut u16,
    ) -> HeifError;
    fn heif_track_get_timescale(track: *const HeifTrack) -> u32;
    fn heif_track_decode_next_image(
        track: *mut HeifTrack,
        out_image: *mut *mut HeifImage,
        colorspace: c_int,
        chroma: c_int,
        options: *const HeifDecodingOptions,
    ) -> HeifError;
    fn heif_image_get_duration(image: *const HeifImage) -> u32;
    fn heif_decoding_options_free(options: *mut HeifDecodingOptions);
    // deps/shim/heif_shim.c: default options with ignore_sequence_editlist set.
    fn riv_heif_sequence_decoding_options_alloc() -> *mut HeifDecodingOptions;
}

/// The primary image's NCLX as an HDR encoding; None for SDR transfers or no colr box.
fn heif_hdr_encoding(handle: *const HeifImageHandle) -> Option<HdrEncoding> {
    let mut profile: *mut HeifColorProfileNclx = std::ptr::null_mut();
    unsafe { heif_image_handle_get_nclx_color_profile(handle, &raw mut profile) }
        .into_result()
        .ok()?;
    if profile.is_null() {
        return None;
    }
    let primaries = unsafe { (*profile).color_primaries };
    let transfer = unsafe { (*profile).transfer_characteristics };
    unsafe { heif_nclx_color_profile_free(profile) };
    cicp_hdr_encoding(u8::try_from(primaries).ok()?, u8::try_from(transfer).ok()?)
}

/// The chroma to ask libheif for and the storage it produces; the two must stay paired.
fn heif_pixel_target(encoding: Option<HdrEncoding>) -> (c_int, PixelStorage) {
    match encoding {
        Some(_) => (HEIF_CHROMA_INTERLEAVED_RRGGBBAA_LE, PixelStorage::RgbaHalf),
        None => (HEIF_CHROMA_INTERLEAVED_RGBA, PixelStorage::Bgra8),
    }
}

/// Container-parse size and storage of the primary image; no pixel decode.
pub fn probe_heif_dimensions_and_storage(bytes: &[u8]) -> Option<(u32, u32, PixelStorage)> {
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return None;
    }
    let dimensions_and_storage = probe_heif_primary_image(context, bytes);
    unsafe { heif_context_free(context) };
    dimensions_and_storage
}

/// Reads the buffer into the context and takes the primary image handle.
fn read_heif_from_memory(context: *mut HeifContext, bytes: &[u8]) -> Result<(), DecodeError> {
    unsafe {
        heif_context_read_from_memory_without_copy(
            context,
            bytes.as_ptr().cast(),
            bytes.len(),
            std::ptr::null(),
        )
    }
    .into_result()
}

fn heif_primary_handle(
    context: *mut HeifContext,
    bytes: &[u8],
) -> Result<*mut HeifImageHandle, DecodeError> {
    read_heif_from_memory(context, bytes)?;
    let mut handle: *mut HeifImageHandle = std::ptr::null_mut();
    unsafe { heif_context_get_primary_image_handle(context, &raw mut handle) }.into_result()?;
    Ok(handle)
}

fn probe_heif_primary_image(
    context: *mut HeifContext,
    bytes: &[u8],
) -> Option<(u32, u32, PixelStorage)> {
    let handle = heif_primary_handle(context, bytes).ok()?;
    let width = unsafe { heif_image_handle_get_width(handle) };
    let height = unsafe { heif_image_handle_get_height(handle) };
    // The same check the decode makes, so the budget matches the storage it will produce.
    let (_, storage) = heif_pixel_target(heif_hdr_encoding(handle));
    unsafe { heif_image_handle_release(handle) };
    (width > 0 && height > 0).then_some((width as u32, height as u32, storage))
}

pub fn decode_heif(bytes: &[u8], format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return Err(uncoded_error("HEIF context allocation failed"));
    }
    let decoded = decode_heif_primary_image(context, bytes, format_name);
    unsafe { heif_context_free(context) };
    decoded
}

fn decode_heif_primary_image(
    context: *mut HeifContext,
    bytes: &[u8],
    format_name: &'static str,
) -> Result<DecodedImage, DecodeError> {
    let handle = heif_primary_handle(context, bytes)?;

    // PQ/HLG survives only a 16-bit request; libheif converts no transfer function.
    let hdr_encoding = heif_hdr_encoding(handle);
    let (chroma, storage) = heif_pixel_target(hdr_encoding);
    let mut image: *mut HeifImage = std::ptr::null_mut();
    let decode_result = unsafe {
        heif_decode_image(
            handle,
            &raw mut image,
            HEIF_COLORSPACE_RGB,
            chroma,
            std::ptr::null(),
        )
    }
    .into_result();
    let icc_profile = {
        let profile_bytes = unsafe { heif_image_handle_get_raw_color_profile_size(handle) };
        if profile_bytes > 0 {
            try_zeroed_buffer(profile_bytes).and_then(|mut buffer| {
                unsafe {
                    heif_image_handle_get_raw_color_profile(handle, buffer.as_mut_ptr().cast())
                }
                .into_result()
                .ok()
                .map(|()| Arc::from(buffer))
            })
        } else {
            None
        }
    };
    unsafe { heif_image_handle_release(handle) };
    decode_result?;

    let width = unsafe { heif_image_get_width(image, HEIF_CHANNEL_INTERLEAVED) };
    let height = unsafe { heif_image_get_height(image, HEIF_CHANNEL_INTERLEAVED) };
    // The 16-bit words hold codes of the source depth, not of the full range.
    let source_bits_per_channel = match hdr_encoding {
        Some(_) => unsafe { heif_image_get_bits_per_pixel_range(image, HEIF_CHANNEL_INTERLEAVED) },
        None => 8,
    };
    let mut stride: c_int = 0;
    let plane =
        unsafe { heif_image_get_plane_readonly(image, HEIF_CHANNEL_INTERLEAVED, &raw mut stride) };
    let row_bytes = i64::from(width) * i64::from(storage.bytes_per_pixel());
    if plane.is_null()
        || width <= 0
        || height <= 0
        || !(1..=MAXIMUM_HDR_SOURCE_BITS as c_int).contains(&source_bits_per_channel)
        || i64::from(stride) < row_bytes
    {
        unsafe { heif_image_release(image) };
        return Err(uncoded_error("HEIF image plane unavailable"));
    }
    let row_bytes = row_bytes as usize;
    let pixel_count = width as usize * height as usize;
    if pixel_count > MAXIMUM_FALLBACK_PIXELS {
        unsafe { heif_image_release(image) };
        return Err(uncoded_error("HEIF has too many pixels to decode"));
    }
    // The cap bounds pixel_count, so row_bytes * height cannot overflow usize here.
    let total_bytes = row_bytes * height as usize;
    let Some(mut pixels) = try_zeroed_buffer(total_bytes) else {
        unsafe { heif_image_release(image) };
        return Err(uncoded_error("HEIF is too large to fit in memory"));
    };
    for (row, output_row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
        let row_pointer = unsafe { plane.add(row * stride as usize) };
        let row_pixels = unsafe { std::slice::from_raw_parts(row_pointer, row_bytes) };
        match hdr_encoding {
            // The codes stay as libheif wrote them; the transfer table expands them.
            Some(_) => output_row.copy_from_slice(row_pixels),
            None => premultiplied_bgra_from_rgba(row_pixels, output_row),
        }
    }
    unsafe { heif_image_release(image) };
    // The copied codes are still PQ/HLG; the shared pass makes them premultiplied linear.
    let peak_luminance_nits = hdr_encoding.and_then(|encoding| {
        let maximum_bits =
            linearize_hdr_pixels(&mut pixels, encoding, source_bits_per_channel as u32);
        peak_luminance_with_maximum_bits(&pixels, maximum_bits)
    });
    Ok(DecodedImage {
        width: width as u32,
        height: height as u32,
        pixel_width: width as u32,
        pixel_height: height as u32,
        format_name,
        icc_profile,
        exif: None,
        storage,
        source_bits_per_channel: source_bits_per_channel as u32,
        peak_luminance_nits,
        source_primaries: hdr_encoding.map(HdrEncoding::source_primaries),
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
        frames_truncated: false,
        gain_map: None,
        gain_map_plane: None,
    })
}

/// Composes only the first frame when maximum_frames is 1, for the animation two-stage path.
pub fn decode_avif_animation(
    bytes: &[u8],
    format_name: &'static str,
    maximum_frames: usize,
) -> Result<DecodedImage, DecodeError> {
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return Err(uncoded_error("HEIF context allocation failed"));
    }
    let decoded = decode_avif_sequence(context, bytes, format_name, maximum_frames);
    unsafe { heif_context_free(context) };
    decoded
}

/// The file's first visual track with its resolution; the caller releases the track.
fn avif_sequence_track(
    context: *mut HeifContext,
    bytes: &[u8],
) -> Result<(*mut HeifTrack, u32, u32), DecodeError> {
    read_heif_from_memory(context, bytes)?;
    let track = unsafe { heif_context_get_track(context, 0) };
    if track.is_null() {
        return Err(uncoded_error("AVIF has no image sequence track"));
    }
    let mut width: u16 = 0;
    let mut height: u16 = 0;
    unsafe { heif_track_get_image_resolution(track, &raw mut width, &raw mut height) }
        .into_result()
        .inspect_err(|_| {
            unsafe { heif_track_release(track) };
        })?;
    Ok((track, u32::from(width), u32::from(height)))
}

fn decode_avif_sequence(
    context: *mut HeifContext,
    bytes: &[u8],
    format_name: &'static str,
    maximum_frames: usize,
) -> Result<DecodedImage, DecodeError> {
    let (track, canvas_width, canvas_height) = avif_sequence_track(context, bytes)?;
    let options = unsafe { riv_heif_sequence_decoding_options_alloc() };
    let decoded = if options.is_null() {
        Err(uncoded_error("HEIF options allocation failed"))
    } else {
        compose_avif_frames(
            track,
            options,
            canvas_width,
            canvas_height,
            format_name,
            maximum_frames,
        )
    };
    if !options.is_null() {
        unsafe { heif_decoding_options_free(options) };
    }
    unsafe { heif_track_release(track) };
    decoded
}

fn compose_avif_frames(
    track: *mut HeifTrack,
    options: *const HeifDecodingOptions,
    canvas_width: u32,
    canvas_height: u32,
    format_name: &'static str,
    maximum_frames: usize,
) -> Result<DecodedImage, DecodeError> {
    if canvas_width == 0 || canvas_height == 0 {
        return Err(uncoded_error("AVIF canvas has no size"));
    }
    let Some(mut compositor) = FrameCompositor::new(canvas_width, canvas_height) else {
        return Err(uncoded_error("AVIF canvas is too large to decode"));
    };
    let frame_bytes = canvas_width as usize * canvas_height as usize * 4;
    let Some(mut frame_pixels) = try_zeroed_buffer(frame_bytes) else {
        return Err(uncoded_error("AVIF is too large to fit in memory"));
    };
    let timescale = unsafe { heif_track_get_timescale(track) };
    loop {
        // The container declares no sample count, so the budget is checked per frame.
        if !compositor.accepts_one_more() {
            break;
        }
        let mut image: *mut HeifImage = std::ptr::null_mut();
        let status = unsafe {
            heif_track_decode_next_image(
                track,
                &raw mut image,
                HEIF_COLORSPACE_RGB,
                HEIF_CHROMA_INTERLEAVED_RGBA,
                options,
            )
        };
        if status.code == HEIF_ERROR_CODE_END_OF_SEQUENCE {
            break;
        }
        status.into_result()?;
        let copied = premultiplied_bgra_from_sequence_image(
            image,
            canvas_width,
            canvas_height,
            &mut frame_pixels,
        );
        let duration_ticks = unsafe { heif_image_get_duration(image) };
        unsafe { heif_image_release(image) };
        copied?;
        compositor.add_frame(FrameRegion {
            pixels: &frame_pixels,
            left: 0,
            top: 0,
            width: canvas_width,
            height: canvas_height,
            // Sequence frames are whole canvases; there is nothing to blend or dispose.
            blend: FrameBlend::Replace,
            disposal: FrameDisposal::Keep,
            delay_milliseconds: sequence_delay_milliseconds(duration_ticks, timescale),
        });
        if compositor.frames_so_far() >= maximum_frames {
            break;
        }
    }
    let (frames, frames_truncated) = compositor.finish();
    if frames.is_empty() {
        return Err(uncoded_error("AVIF sequence has no frames"));
    }
    Ok(DecodedImage {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        format_name,
        icc_profile: None,
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: BGRA8_SOURCE_BITS,
        peak_luminance_nits: None,
        source_primaries: None,
        frames,
        frames_truncated,
        gain_map: None,
        gain_map_plane: None,
    })
}

fn premultiplied_bgra_from_sequence_image(
    image: *const HeifImage,
    canvas_width: u32,
    canvas_height: u32,
    frame_pixels: &mut [u8],
) -> Result<(), DecodeError> {
    let width = unsafe { heif_image_get_width(image, HEIF_CHANNEL_INTERLEAVED) };
    let height = unsafe { heif_image_get_height(image, HEIF_CHANNEL_INTERLEAVED) };
    let mut stride: c_int = 0;
    let plane =
        unsafe { heif_image_get_plane_readonly(image, HEIF_CHANNEL_INTERLEAVED, &raw mut stride) };
    let row_bytes = i64::from(canvas_width) * 4;
    if plane.is_null()
        || i64::from(width) != i64::from(canvas_width)
        || i64::from(height) != i64::from(canvas_height)
        || i64::from(stride) < row_bytes
    {
        return Err(uncoded_error("AVIF frame differs from its sequence header"));
    }
    let row_bytes = row_bytes as usize;
    for (row, output_row) in frame_pixels.chunks_exact_mut(row_bytes).enumerate() {
        let row_pixels =
            unsafe { std::slice::from_raw_parts(plane.add(row * stride as usize), row_bytes) };
        premultiplied_bgra_from_rgba(row_pixels, output_row);
    }
    Ok(())
}

fn sequence_delay_milliseconds(duration_ticks: u32, timescale: u32) -> u32 {
    match (u64::from(duration_ticks) * 1000).checked_div(u64::from(timescale)) {
        None | Some(0) => DEFAULT_FRAME_DELAY_MILLISECONDS,
        Some(milliseconds) => milliseconds.min(u64::from(u32::MAX)) as u32,
    }
}

/// Parse-only, for the weight probe; no frame is decoded.
pub fn probe_avif_sequence_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let context = unsafe { heif_context_alloc() };
    if context.is_null() {
        return None;
    }
    let geometry = avif_sequence_track(context, bytes)
        .ok()
        .map(|(track, width, height)| {
            unsafe { heif_track_release(track) };
            (width, height)
        });
    unsafe { heif_context_free(context) };
    geometry
}

#[cfg(test)]
mod heif_range_tests {
    use super::*;

    /// Full-range 16-bit code per source code; the reference the fused table is checked against.
    fn full_range_expansion(source_bits: u32) -> Box<[u16; 65536]> {
        let maximum = (1u32 << source_bits) - 1;
        let mut table = crate::image::decode::boxed_lookup_table::<u16>();
        let declared = maximum as usize + 1;
        for (code, expanded) in table[..declared].iter_mut().enumerate() {
            *expanded = ((code as u32 * u32::from(u16::MAX) + maximum / 2) / maximum) as u16;
        }
        // A broken decoder writing past the declared depth clamps rather than wrapping.
        table[declared..].fill(u16::MAX);
        table
    }

    /// Rewrites 16-bit little-endian codes through the expansion table, code for code.
    fn expand_to_full_range(source: &[u8], output: &mut [u8], table: &[u16; 65536]) {
        for (code, expanded) in source
            .as_chunks::<2>()
            .0
            .iter()
            .zip(output.as_chunks_mut::<2>().0)
        {
            let value = table[usize::from(u16::from_le_bytes(*code))];
            expanded.copy_from_slice(&value.to_le_bytes());
        }
    }

    #[test]
    fn the_declared_depth_maps_onto_the_whole_16_bit_range() {
        let table = full_range_expansion(10);
        assert_eq!(table[0], 0);
        assert_eq!(table[1023], u16::MAX);
        // Scaling, not a shift: half the source range lands on half the full range.
        assert_eq!(table[512], 32800);
        assert!(table.windows(2).take(1024).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn sixteen_bit_sources_pass_through_untouched() {
        let table = full_range_expansion(16);
        assert!(
            table
                .iter()
                .enumerate()
                .all(|(code, expanded)| usize::from(*expanded) == code)
        );
    }

    #[test]
    fn codes_above_the_declared_depth_clamp_to_white() {
        // A broken decoder writing past the range must not wrap around to black.
        let table = full_range_expansion(10);
        assert_eq!(table[1024], u16::MAX);
        assert_eq!(table[65535], u16::MAX);
    }

    #[test]
    fn expansion_rewrites_little_endian_lanes_in_place_order() {
        let table = full_range_expansion(10);
        let source = [0x00, 0x00, 0xFF, 0x03, 0x00, 0x02, 0xFF, 0x03];
        let mut output = [0u8; 8];
        expand_to_full_range(&source, &mut output, &table);
        assert_eq!(output, [0x00, 0x00, 0xFF, 0xFF, 0x20, 0x80, 0xFF, 0xFF]);
    }

    /// Deterministic RGBA codes of the given depth, as libheif writes them (low bits).
    fn coded_pixels(count: usize, bits: u32, mut state: u32) -> Vec<u8> {
        let maximum = (1u32 << bits) - 1;
        let mut pixels = Vec::with_capacity(count * 8);
        for _ in 0..count {
            for channel in 0..4 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Opaque alpha is the common case in this path; keep some partial ones.
                let code = match (channel, state >> 30) {
                    (3, 0..=2) => maximum,
                    _ => (state >> 8) % (maximum + 1),
                };
                pixels.extend_from_slice(&(code as u16).to_le_bytes());
            }
        }
        pixels
    }

    /// Two-pass shape: expand every code, then linearize with the full-range table.
    fn expand_then_linearize(source: &[u8], bits: u32, encoding: HdrEncoding) -> (Vec<u8>, u16) {
        let table = full_range_expansion(bits);
        let mut pixels = vec![0u8; source.len()];
        expand_to_full_range(source, &mut pixels, &table);
        let maximum_bits = linearize_hdr_pixels(&mut pixels, encoding, 16);
        (pixels, maximum_bits)
    }

    /// Fused shape: copy the codes, then linearize with the depth's transfer table.
    fn copy_then_linearize(source: &[u8], bits: u32, encoding: HdrEncoding) -> (Vec<u8>, u16) {
        let mut pixels = vec![0u8; source.len()];
        pixels.copy_from_slice(source);
        let maximum_bits = linearize_hdr_pixels(&mut pixels, encoding, bits);
        (pixels, maximum_bits)
    }

    #[test]
    fn the_fused_table_matches_the_separate_expansion_pass() {
        for bits in [8u32, 10, 12, 16] {
            for transfer in [16u8, 18] {
                let encoding = cicp_hdr_encoding(9, transfer).expect("cicp encoding");
                let source = coded_pixels(600_000, bits, 97);
                let (expanded, expanded_maximum) = expand_then_linearize(&source, bits, encoding);
                let (fused, fused_maximum) = copy_then_linearize(&source, bits, encoding);
                let mut differing = 0usize;
                let mut widest_gap = 0u16;
                for (left, right) in expanded
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(fused.as_chunks::<2>().0)
                {
                    let left = u16::from_le_bytes(*left);
                    let right = u16::from_le_bytes(*right);
                    if left != right {
                        differing += 1;
                        widest_gap = widest_gap.max(left.abs_diff(right));
                    }
                }
                println!(
                    "bits={bits} transfer={transfer} differing={differing} of {} widest_gap={widest_gap} ulp",
                    expanded.len() / 2
                );
                // 255 and 65535 divide the full range exactly, so those depths must not move.
                if u32::from(u16::MAX) % ((1u32 << bits) - 1) == 0 {
                    assert_eq!(expanded, fused, "bits={bits}");
                }
                // Elsewhere only alpha moves, and the fused divisor is the more exact of the two.
                assert!(widest_gap <= 2, "bits={bits} widest_gap={widest_gap}");
                assert_eq!(expanded_maximum, fused_maximum, "bits={bits}");
            }
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn heif_expansion_timing() {
        const BITS: u32 = 10;
        let encoding = cicp_hdr_encoding(9, 16).expect("cicp encoding");
        let source = coded_pixels(12_000_000, BITS, 41);
        let table = full_range_expansion(BITS);
        // Both destinations are allocated and touched once, so neither pays for first-touch.
        let mut expanded = vec![0u8; source.len()];
        let mut copied = vec![0u8; source.len()];
        expand_to_full_range(&source, &mut expanded, &table);
        copied.copy_from_slice(&source);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            expand_to_full_range(&source, &mut expanded, &table);
            let expand_elapsed = start.elapsed();
            let start = std::time::Instant::now();
            copied.copy_from_slice(&source);
            let copy_elapsed = start.elapsed();
            let mut scratch = expanded.clone();
            let start = std::time::Instant::now();
            let full_range_maximum = linearize_hdr_pixels(&mut scratch, encoding, 16);
            let full_range_elapsed = start.elapsed();
            let mut scratch = copied.clone();
            let start = std::time::Instant::now();
            let coded_maximum = linearize_hdr_pixels(&mut scratch, encoding, BITS);
            let coded_elapsed = start.elapsed();
            assert_eq!(full_range_maximum, coded_maximum);
            println!(
                "expand={expand_elapsed:?} copy={copy_elapsed:?} \
                 linearize(16)={full_range_elapsed:?} linearize({BITS})={coded_elapsed:?}"
            );
        }
    }
}

#[cfg(test)]
mod premultiply_tests {
    use super::*;

    #[test]
    fn premultiply_matches_the_scalar_reference() {
        let mut pixels = crate::image::decode::random_pixels(64 * 64, 13);
        let mut expected = pixels.clone();
        for pixel in expected.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            if alpha == 255 {
                continue;
            }
            for channel in &mut pixel[..3] {
                *channel = (u16::from(*channel) * alpha / 255) as u8;
            }
        }
        premultiply_bgra_in_place(&mut pixels);
        assert_eq!(pixels, expected);
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn premultiply_timing() {
        let pixels = crate::image::decode::random_pixels(1920 * 1080, 31);
        for _ in 0..3 {
            let mut scratch = pixels.clone();
            let start = std::time::Instant::now();
            for _ in 0..50 {
                premultiply_bgra_in_place(&mut scratch);
                std::hint::black_box(&scratch);
            }
            println!("premultiply 50 frames elapsed={:?}", start.elapsed());
        }
    }
}

#[cfg(test)]
mod exr_robustness_tests {
    use super::*;

    #[test]
    #[ignore = "needs test/exr_base.exr"]
    fn a_valid_exr_decodes() {
        let file_bytes = std::fs::read("test/exr_base.exr").expect("fixture");
        assert!(decode_exr_bytes(&file_bytes, "EXR").is_ok());
    }

    #[test]
    #[ignore = "needs test/exr_bad_offset.exr"]
    fn a_corrupt_offset_table_errors_without_reading_out_of_bounds() {
        let file_bytes = std::fs::read("test/exr_bad_offset.exr").expect("fixture");
        // The subtraction bound check must catch it, not wrap and read before the buffer.
        assert!(decode_exr_bytes(&file_bytes, "EXR").is_err());
    }
}

#[cfg(test)]
mod avif_sequence_tests {
    use super::*;

    fn fixture_bytes() -> Vec<u8> {
        std::fs::read("test/animated_avif.avif").expect("run test/make_animation_avif.py first")
    }

    /// The fixture is high-quality YUV, not lossless, so flat colors land within a few codes.
    fn assert_pixel_near(actual: [u8; 4], expected: [u8; 4]) {
        for (channel, (actual_code, expected_code)) in actual.iter().zip(expected).enumerate() {
            assert!(
                (i16::from(*actual_code) - i16::from(expected_code)).abs() <= 6,
                "channel {channel}: {actual:?} vs {expected:?}"
            );
        }
    }

    /// The fixture marks infinite repetition, so finishing at all proves the editlist is ignored.
    #[test]
    #[ignore = "needs test/ fixtures"]
    fn the_avif_fixture_decodes_frames_and_timing() {
        let decoded = decode_avif_animation(&fixture_bytes(), "AVIF", usize::MAX)
            .map_err(|error| error.message)
            .expect("decodes");
        assert_eq!((decoded.width, decoded.height), (64, 32));
        assert_eq!(decoded.frames.len(), 4);
        assert!(!decoded.frames_truncated);
        assert!(
            decoded
                .frames
                .iter()
                .all(|frame| frame.delay_milliseconds == 100)
        );
        let pixel = |frame: usize| -> [u8; 4] {
            decoded.frames[frame].pixels[..4].try_into().expect("pixel")
        };
        assert_pixel_near(pixel(0), [40, 40, 220, 255]); // opaque red, BGRA order
        assert_pixel_near(pixel(1), [60, 180, 40, 255]);
        // Blue at alpha 128 arrives premultiplied: channel x 128 / 255.
        assert_pixel_near(pixel(2), [115, 45, 30, 128]);
        assert_pixel_near(pixel(3), [255, 255, 255, 255]);
    }

    #[test]
    #[ignore = "needs test/ fixtures"]
    fn the_first_frame_stops_a_bounded_decode() {
        let decoded = decode_avif_animation(&fixture_bytes(), "AVIF", 1)
            .map_err(|error| error.message)
            .expect("decodes");
        assert_eq!(decoded.frames.len(), 1);
        let pixel: [u8; 4] = decoded.frames[0].pixels[..4].try_into().expect("pixel");
        assert_pixel_near(pixel, [40, 40, 220, 255]);
    }

    #[test]
    #[ignore = "needs test/ fixtures"]
    fn the_probe_reports_the_track_geometry() {
        assert_eq!(
            probe_avif_sequence_dimensions(&fixture_bytes()),
            Some((64, 32))
        );
    }
}
