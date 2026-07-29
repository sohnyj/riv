//! Decoder registry, format dispatch, and the WIC adapter (decode workers only).

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    E_ABORT, GENERIC_READ, WINCODEC_ERR_COMPONENTINITIALIZEFAILURE, WINCODEC_ERR_COMPONENTNOTFOUND,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_IMMUTABLE, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, GUID_WICPixelFormat64bppPRGBAHalf,
    GUID_WICPixelFormat64bppRGBA, IWICBitmapDecoder, IWICBitmapFrameDecode, IWICBitmapSource,
    IWICColorContext, IWICImagingFactory, IWICMetadataQueryReader, IWICPixelFormatInfo2,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICBitmapTransformFlipHorizontal, WICBitmapTransformFlipVertical, WICBitmapTransformOptions,
    WICBitmapTransformRotate90, WICBitmapTransformRotate180, WICBitmapTransformRotate270,
    WICColorContextProfile, WICDecodeMetadataCacheOnDemand,
    WICPixelFormatNumericRepresentationFloat, WICRect,
};
use windows::Win32::Media::MediaFoundation::MF_E_TOPO_CODEC_NOT_FOUND;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToDouble, PropVariantToFileTime,
    PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{HSTRING, Interface, PCWSTR, Result as WindowsResult, w};

use super::color::{
    self, SDR_REFERENCE_WHITE_NITS, perceptual_quantizer_code, perceptual_quantizer_nits,
};

pub struct Frame {
    pub pixels: Vec<u8>,
    pub delay_milliseconds: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PixelStorage {
    Bgra8,
    RgbaHalf,
}

impl PixelStorage {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Bgra8 => 4,
            Self::RgbaHalf => 8,
        }
    }

    /// The DXGI format both the worker upload and the D2D wrap must agree on.
    pub fn dxgi_format(self) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
        match self {
            Self::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            Self::RgbaHalf => DXGI_FORMAT_R16G16B16A16_FLOAT,
        }
    }
}

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub format_name: &'static str,
    pub icc_profile: Option<Vec<u8>>,
    pub exif: Option<ExifInfo>,
    pub storage: PixelStorage,
    /// Meaningful bits per channel of the decoded pixels (8 for Bgra8 storage).
    pub source_bits_per_channel: u32,
    /// Content peak (nits) of FP16 sources; 1.0 = 80 nits in the source primaries.
    pub peak_luminance_nits: Option<f32>,
    /// CIE xy of the source R, G, B primaries when the file states them; None means unknown.
    pub source_primaries: Option<[[f32; 2]; 3]>,
    pub frames: Vec<Frame>,
    /// The animation would expand past the byte limit; only the first frame is kept.
    pub frames_truncated: bool,
}

#[derive(Clone)]
pub struct ExifInfo {
    pub date_taken: Option<std::time::SystemTime>,
    pub rating: Option<u32>,
    pub camera_maker: Option<String>,
    pub camera_model: Option<String>,
    pub f_stop: Option<f64>,
    pub exposure_time_seconds: Option<f64>,
    pub iso_speed: Option<u32>,
    pub exposure_bias: Option<f64>,
    pub focal_length_millimeters: Option<f64>,
    pub max_aperture: Option<f64>,
    pub metering_mode: Option<u32>,
    pub flash: Option<u32>,
}

impl ExifInfo {
    fn any_present(&self) -> bool {
        self.date_taken.is_some()
            || self.rating.is_some()
            || self.camera_maker.is_some()
            || self.camera_model.is_some()
            || self.f_stop.is_some()
            || self.exposure_time_seconds.is_some()
            || self.iso_speed.is_some()
            || self.exposure_bias.is_some()
            || self.focal_length_millimeters.is_some()
            || self.max_aperture.is_some()
            || self.metering_mode.is_some()
            || self.flash.is_some()
    }
}

impl DecodedImage {
    pub fn pixel_bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.pixels.len()).sum()
    }

    /// Bytes per row of a frame; the D3D pitch and the buffer stride alike.
    pub fn row_pitch(&self) -> u32 {
        self.pixel_width * self.storage.bytes_per_pixel()
    }

    /// Exact byte length of one full frame at this geometry.
    pub fn frame_byte_length(&self) -> usize {
        self.row_pitch() as usize * self.pixel_height as usize
    }

    /// The geometry half of a texture description; usage and binding stay caller-side.
    pub fn texture_description(&self) -> D3D11_TEXTURE2D_DESC {
        D3D11_TEXTURE2D_DESC {
            Width: self.pixel_width,
            Height: self.pixel_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: self.storage.dxgi_format(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    /// The metadata with pixels released; a texture is the only copy afterward.
    pub fn without_pixels(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            format_name: self.format_name,
            icc_profile: self.icc_profile.clone(),
            exif: self.exif.clone(),
            storage: self.storage,
            source_bits_per_channel: self.source_bits_per_channel,
            peak_luminance_nits: self.peak_luminance_nits,
            source_primaries: self.source_primaries,
            frames: self
                .frames
                .iter()
                .map(|frame| Frame {
                    pixels: Vec::new(),
                    delay_milliseconds: frame.delay_milliseconds,
                })
                .collect(),
            frames_truncated: self.frames_truncated,
        }
    }
}

#[derive(Clone)]
pub struct DecodeError {
    pub code: i32,
    pub message: String,
    pub store_extensions: &'static [&'static str],
}

impl DecodeError {
    pub fn cancelled() -> Self {
        Self {
            code: E_ABORT.0,
            message: "cancelled".to_string(),
            store_extensions: &[],
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.code == E_ABORT.0
    }

    /// True when no decoder recognized the data and no Store codec is named.
    pub fn is_unrecognized_format(&self) -> bool {
        self.code == WINCODEC_ERR_COMPONENTNOTFOUND.0 && self.store_extensions.is_empty()
    }
}

impl From<windows::core::Error> for DecodeError {
    fn from(error: windows::core::Error) -> Self {
        Self {
            code: error.code().0,
            message: error.message(),
            store_extensions: &[],
        }
    }
}

enum FrameSemantics {
    Single,
    Animation,
    SizeVariants,
}

/// D3D11 FL11 texture limit; larger sources are downscaled before upload.
const MAXIMUM_TEXTURE_DIMENSION: u32 = 16384;

/// Cap on an animation's expanded frames; past it only the first frame is kept.
const ANIMATION_FRAMES_BYTE_LIMIT: u64 = 1 << 30;

/// Whether `frame_count` canvas-sized frames would expand past the byte limit.
pub(crate) fn animation_budget_exceeded(frame_count: u64, canvas_bytes: usize) -> bool {
    frame_count * canvas_bytes as u64 > ANIMATION_FRAMES_BYTE_LIMIT
}

/// 100 ns intervals from 1601-01-01 (FILETIME zero) to the UNIX epoch.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

type MagicSignature = &'static [(usize, &'static [u8])];

enum Adapter {
    Wic,
    WicRawTwoStage,
    Apng,
    Svg,
    WebPAnimation,
    Exr,
    HeifWithWicPreferred,
}

pub struct FormatDescriptor {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    magic: &'static [MagicSignature],
    semantics: FrameSemantics,
    adapter: Adapter,
    store_extensions: &'static [&'static str],
}

/// Extensions, file filters, and association groups all derive from this registry.
static REGISTRY: &[FormatDescriptor] = &[
    FormatDescriptor {
        name: "PNG",
        extensions: &["png"],
        magic: &[&[(0, b"\x89PNG\r\n\x1a\n")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "APNG",
        extensions: &["apng"],
        magic: &[],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Apng,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "SVG",
        extensions: &["svg", "svgz"],
        magic: &[&[(0, b"<svg")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Svg,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "JPEG",
        extensions: &["jpg", "jpeg", "jpe"],
        magic: &[&[(0, b"\xFF\xD8\xFF")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "GIF",
        extensions: &["gif"],
        magic: &[&[(0, b"GIF8")]],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "WebP",
        extensions: &["webp"],
        magic: &[&[(0, b"RIFF"), (8, b"WEBP")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &["WebP Image Extensions"],
    },
    FormatDescriptor {
        name: "BMP",
        extensions: &["bmp", "dib"],
        // The two reserved fields at offset 6 must be zero.
        magic: &[&[(0, b"BM"), (6, &[0, 0, 0, 0])]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "ICO",
        extensions: &["ico"],
        magic: &[&[(0, &[0x00, 0x00, 0x01, 0x00])]],
        semantics: FrameSemantics::SizeVariants,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "TIFF",
        extensions: &["tif", "tiff"],
        magic: &[&[(0, b"II*\x00")], &[(0, b"MM\x00*")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "DDS",
        extensions: &["dds"],
        magic: &[&[(0, b"DDS ")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "HEIF",
        extensions: &["heic", "heif", "hif"],
        magic: &[
            &[(4, b"ftypheic")],
            &[(4, b"ftypheix")],
            &[(4, b"ftypmif1")],
            &[(4, b"ftypmsf1")],
            &[(4, b"ftyphevc")],
        ],
        semantics: FrameSemantics::Single,
        adapter: Adapter::HeifWithWicPreferred,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "EXR",
        extensions: &["exr"],
        magic: &[&[(0, b"\x76\x2F\x31\x01")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Exr,
        store_extensions: &[],
    },
    FormatDescriptor {
        name: "AVIF",
        extensions: &["avif"],
        magic: &[&[(4, b"ftypavif")], &[(4, b"ftypavis")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &["HEIF Image Extension", "AV1 Video Extension"],
    },
    FormatDescriptor {
        name: "JPEG XL",
        extensions: &["jxl"],
        magic: &[
            &[(0, b"\x00\x00\x00\x0CJXL \r\n\x87\n")],
            &[(0, b"\xFF\x0A")],
        ],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extensions: &["JPEG XL Image Extension"],
    },
    FormatDescriptor {
        name: "RAW",
        extensions: &[
            "arw", "cr2", "cr3", "crw", "dng", "erf", "kdc", "mrw", "nef", "nrw", "orf", "pef",
            "raf", "raw", "rw2", "rwl", "sr2", "srw", "x3f",
        ],
        magic: &[],
        semantics: FrameSemantics::Single,
        adapter: Adapter::WicRawTwoStage,
        store_extensions: &["Raw Image Extension"],
    },
];

pub fn format_groups() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    REGISTRY
        .iter()
        .map(|descriptor| (descriptor.name, descriptor.extensions))
}

pub fn supported_extensions() -> impl Iterator<Item = &'static str> {
    REGISTRY
        .iter()
        .flat_map(|descriptor| descriptor.extensions.iter().copied())
}

pub fn is_supported_extension(extension: &str) -> bool {
    REGISTRY
        .iter()
        .any(|descriptor| descriptor.extensions.contains(&extension))
}

pub fn format_name_for_extension(extension: &str) -> Option<&'static str> {
    descriptor_for_extension(extension).map(|descriptor| descriptor.name)
}

fn descriptor_for_extension(extension: &str) -> Option<&'static FormatDescriptor> {
    REGISTRY
        .iter()
        .find(|descriptor| descriptor.extensions.contains(&extension))
}

pub fn probe_file(path: &Path) -> Option<&'static FormatDescriptor> {
    let header = read_header(path)?;
    probe_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
}

fn probe_magic(header: &[u8]) -> Option<&'static FormatDescriptor> {
    REGISTRY
        .iter()
        .find(|descriptor| {
            descriptor.magic.iter().any(|signature| {
                signature.iter().all(|(offset, bytes)| {
                    header
                        .get(*offset..offset + bytes.len())
                        .is_some_and(|slice| slice == *bytes)
                })
            })
        })
        .or_else(|| xml_svg_probe(header))
}

/// An XML prologue counts as SVG only when an <svg tag follows in the header.
fn xml_svg_probe(header: &[u8]) -> Option<&'static FormatDescriptor> {
    if header.starts_with(b"<?xml") && header.windows(4).any(|window| window == b"<svg") {
        descriptor_for_extension("svg")
    } else {
        None
    }
}

static ANIMATED_WEBP: FormatDescriptor = FormatDescriptor {
    name: "WebP",
    extensions: &[],
    magic: &[],
    semantics: FrameSemantics::Animation,
    adapter: Adapter::WebPAnimation,
    store_extensions: &[],
};

/// PNG + acTL = APNG; WebP + VP8X ANIM flag = animated WebP; HEIF + avif brand = AVIF.
fn refine_by_content(
    descriptor: &'static FormatDescriptor,
    header: &[u8],
) -> &'static FormatDescriptor {
    if descriptor.name == "PNG" && png_has_animation_control(header) {
        return descriptor_for_extension("apng").unwrap_or(descriptor);
    }
    if descriptor.name == "WebP" && webp_has_animation_flag(header) {
        return &ANIMATED_WEBP;
    }
    if descriptor.name == "HEIF"
        && (ftyp_has_brand(header, b"avif") || ftyp_has_brand(header, b"avis"))
    {
        return descriptor_for_extension("avif").unwrap_or(descriptor);
    }
    descriptor
}

/// True when the ftyp box lists a brand as the major brand or a compatible one.
fn ftyp_has_brand(header: &[u8], brand: &[u8; 4]) -> bool {
    if header.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let box_size = read_u32_be(header, 0).unwrap_or(0) as usize;
    header.get(8..12) == Some(brand)
        || header
            .get(16..box_size.min(header.len()))
            .unwrap_or_default()
            .chunks_exact(4)
            .any(|compatible| compatible == brand)
}

fn webp_has_animation_flag(header: &[u8]) -> bool {
    header.get(12..16) == Some(b"VP8X") && header.get(20).is_some_and(|flags| flags & 0x02 != 0)
}

fn png_has_animation_control(header: &[u8]) -> bool {
    let mut offset = 8; // past the PNG signature
    while let Some(chunk_header) = header.get(offset..offset + 8) {
        let length = u32::from_be_bytes(chunk_header[..4].try_into().unwrap()) as usize;
        let chunk_type = &chunk_header[4..8];
        match chunk_type {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => offset += 8 + length + 4, // header + data + CRC
        }
    }
    false
}

fn descriptor_for_path(path: &Path) -> Option<&'static FormatDescriptor> {
    let header = read_header(path);
    let by_extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .and_then(|extension| descriptor_for_extension(&extension));
    match (by_extension, header) {
        (Some(descriptor), Some(header)) => Some(refine_by_content(descriptor, &header)),
        (Some(descriptor), None) => Some(descriptor),
        (None, Some(header)) => {
            probe_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
        }
        (None, None) => None,
    }
}

fn descriptor_for_bytes(data: &[u8], extension: Option<&str>) -> Option<&'static FormatDescriptor> {
    let header = &data[..data.len().min(4096)];
    match extension
        .map(str::to_lowercase)
        .and_then(|extension| descriptor_for_extension(&extension))
    {
        Some(descriptor) => Some(refine_by_content(descriptor, header)),
        None => probe_magic(header).map(|descriptor| refine_by_content(descriptor, header)),
    }
}

fn read_header(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0u8; 4096];
    let read_bytes = file.read(&mut buffer).ok()?;
    buffer.truncate(read_bytes);
    Some(buffer)
}

/// Decode entry point; runs on an MTA decode worker.
pub fn decode_file(path: &Path, cancellation: &AtomicBool) -> Result<DecodedImage, DecodeError> {
    decode_input(&DecodeInput::File(path), cancellation)
}

/// Decodes an in-memory image (an extracted archive member).
pub fn decode_bytes(
    data: &[u8],
    extension: Option<&str>,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    decode_input(&DecodeInput::Memory { data, extension }, cancellation)
}

enum DecodeInput<'a> {
    File(&'a Path),
    Memory {
        data: &'a [u8],
        extension: Option<&'a str>,
    },
}

impl DecodeInput<'_> {
    fn descriptor(&self) -> Option<&'static FormatDescriptor> {
        match self {
            DecodeInput::File(path) => descriptor_for_path(path),
            DecodeInput::Memory { data, extension } => descriptor_for_bytes(data, *extension),
        }
    }

    /// Whole input bytes for the adapters that decode from memory.
    fn read_all(&self) -> Result<std::borrow::Cow<'_, [u8]>, DecodeError> {
        match self {
            DecodeInput::File(path) => std::fs::read(path)
                .map(std::borrow::Cow::Owned)
                .map_err(uncoded_error),
            DecodeInput::Memory { data, .. } => Ok(std::borrow::Cow::Borrowed(*data)),
        }
    }
}

/// Failures meaning the codec is absent or broken, which the Store hint can remedy:
/// unregistered, registered but failing to start, and a missing video decoder underneath.
fn is_missing_codec_error(code: i32) -> bool {
    code == WINCODEC_ERR_COMPONENTNOTFOUND.0
        || code == WINCODEC_ERR_COMPONENTINITIALIZEFAILURE.0
        || code == MF_E_TOPO_CODEC_NOT_FOUND.0
}

fn decode_input(
    input: &DecodeInput<'_>,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    if cancellation.load(Ordering::Relaxed) {
        return Err(DecodeError::cancelled());
    }
    let descriptor = input.descriptor();
    let format_name = descriptor.map_or("Unknown", |descriptor| descriptor.name);
    let semantics = descriptor.map_or(&FrameSemantics::Single, |descriptor| &descriptor.semantics);
    let adapter = descriptor.map_or(&Adapter::Wic, |descriptor| &descriptor.adapter);
    match adapter {
        Adapter::Wic | Adapter::WicRawTwoStage => {
            decode_with_wic(input, format_name, semantics, cancellation).map_err(|mut error| {
                if is_missing_codec_error(error.code)
                    && let Some(descriptor) = descriptor
                {
                    error.store_extensions = descriptor.store_extensions;
                }
                error
            })
        }
        Adapter::Apng => match input {
            DecodeInput::File(path) => {
                let file = File::open(path).map_err(uncoded_error)?;
                decode_apng(BufReader::new(file), format_name, cancellation)
            }
            DecodeInput::Memory { data, .. } => {
                decode_apng(Cursor::new(*data), format_name, cancellation)
            }
        },
        Adapter::Svg => decode_svg(&input.read_all()?, format_name),
        Adapter::WebPAnimation => {
            super::fallback::decode_webp_animation(&input.read_all()?, format_name, usize::MAX)
        }
        Adapter::Exr => match input {
            DecodeInput::File(path) => super::fallback::decode_exr(path, format_name),
            DecodeInput::Memory { data, .. } => {
                super::fallback::decode_exr_bytes(data, format_name)
            }
        }
        .and_then(|decoded| enforce_device_limit(decoded, cancellation)),
        Adapter::HeifWithWicPreferred => {
            decode_with_wic(input, format_name, semantics, cancellation).or_else(|error| {
                // Any WIC failure tries the bundled decoder; a registered codec can
                // still fail to initialize when its video decoder dependency is broken.
                if error.is_cancelled() {
                    Err(error)
                } else {
                    super::fallback::decode_heif(&input.read_all()?, format_name)
                        .and_then(|decoded| enforce_device_limit(decoded, cancellation))
                }
            })
        }
    }
}

/// Decodes only the opening frame; None unless the file really holds several.
pub fn decode_animation_first_frame(
    path: &Path,
    cancellation: &AtomicBool,
) -> Option<DecodedImage> {
    let input = DecodeInput::File(path);
    let descriptor = input.descriptor()?;
    if !matches!(descriptor.semantics, FrameSemantics::Animation) {
        return None;
    }
    match descriptor.adapter {
        // A GIF frame can be a placed sub-rectangle, so even one goes through the compositor.
        Adapter::Wic => with_wic_factory(|factory| {
            let decoder = create_wic_decoder(factory, &input)?;
            if unsafe { decoder.GetFrameCount()? } <= 1 {
                return Ok(None);
            }
            decode_animation(factory, &decoder, 1, cancellation).map(Some)
        })
        .ok()
        .flatten()
        .map(|frames| frames.into_image(descriptor.name)),
        // The acTL chunk already proved the animation; WIC hands back the default image.
        Adapter::Apng => decode_with_wic(
            &input,
            descriptor.name,
            &FrameSemantics::Single,
            cancellation,
        )
        .ok(),
        Adapter::WebPAnimation => {
            super::fallback::decode_webp_animation(&input.read_all().ok()?, descriptor.name, 1)
                .and_then(|decoded| enforce_device_limit(decoded, cancellation))
                .ok()
        }
        _ => None,
    }
}

/// Bitmap bytes a full decode would produce; submission and eviction budget this number.
pub fn decoded_weight(width: u32, height: u32, bytes_per_pixel: u32, frame_count: u64) -> u64 {
    let frame_count = frame_count.max(1);
    if frame_count > 1 {
        // Animations are never downscaled; the compositor works at canvas size.
        let frame_bytes = u64::from(width) * u64::from(height) * u64::from(bytes_per_pixel);
        if animation_budget_exceeded(frame_count, frame_bytes as usize) {
            frame_bytes
        } else {
            frame_count * frame_bytes
        }
    } else {
        let (width, height) = device_limited_size(width, height);
        u64::from(width) * u64::from(height) * u64::from(bytes_per_pixel)
    }
}

/// KeepAspectRatio fit under the texture limit; identity when already within it.
fn device_limited_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAXIMUM_TEXTURE_DIMENSION {
        return (width, height);
    }
    let limit = u64::from(MAXIMUM_TEXTURE_DIMENSION);
    (
        (u64::from(width) * limit / u64::from(longest)).max(1) as u32,
        (u64::from(height) * limit / u64::from(longest)).max(1) as u32,
    )
}

/// Header-only weight probe; None when the header cannot be read or parsed.
pub fn probe_file_weight(path: &Path) -> Option<u64> {
    probe_weight(&DecodeInput::File(path))
}

/// Weight probe over extracted bytes (an archive member).
pub fn probe_bytes_weight(data: &[u8], extension: Option<&str>) -> Option<u64> {
    probe_weight(&DecodeInput::Memory { data, extension })
}

fn probe_weight(input: &DecodeInput<'_>) -> Option<u64> {
    let descriptor = input.descriptor()?;
    match descriptor.adapter {
        Adapter::Wic | Adapter::WicRawTwoStage => probe_wic_weight(input, &descriptor.semantics),
        Adapter::Apng => match input {
            DecodeInput::File(path) => probe_apng_weight(BufReader::new(File::open(path).ok()?)),
            DecodeInput::Memory { data, .. } => probe_apng_weight(Cursor::new(*data)),
        },
        Adapter::Svg => probe_svg_weight(&input.read_all().ok()?),
        Adapter::WebPAnimation => probe_webp_weight(input),
        Adapter::Exr => {
            let (width, height) = match input {
                DecodeInput::File(path) => super::fallback::probe_exr(path),
                DecodeInput::Memory { data, .. } => super::fallback::probe_exr_bytes(data),
            }?;
            Some(decoded_weight(width, height, 8, 1))
        }
        Adapter::HeifWithWicPreferred => {
            // Mirrors the decode dispatch: WIC first, the bundled decoder on failure.
            probe_wic_weight(input, &descriptor.semantics).or_else(|| {
                let (width, height, storage) =
                    super::fallback::probe_heif(&input.read_all().ok()?)?;
                Some(decoded_weight(width, height, storage.bytes_per_pixel(), 1))
            })
        }
    }
}

fn probe_wic_weight(input: &DecodeInput<'_>, semantics: &FrameSemantics) -> Option<u64> {
    with_wic_factory(|factory| {
        let decoder = create_wic_decoder(factory, input)?;
        let index = match semantics {
            FrameSemantics::SizeVariants => {
                let frame_count = unsafe { decoder.GetFrameCount()? }.max(1);
                largest_frame_index(&decoder, frame_count)?
            }
            // Counting animation frames walks the whole file; the first frame is the weight.
            FrameSemantics::Animation | FrameSemantics::Single => 0,
        };
        let frame = unsafe { decoder.GetFrame(index)? };
        let (width, height) = source_size(&frame.cast()?)?;
        let (bits_per_channel, _) = frame_pixel_format_info(factory, &frame);
        let bytes_per_pixel = if bits_per_channel > 8 { 8 } else { 4 };
        Ok(decoded_weight(width, height, bytes_per_pixel, 1))
    })
    .ok()
}

/// IHDR and acTL sit before the image data, so this reads only the file head.
fn probe_apng_weight<Input: BufRead + Seek>(input: Input) -> Option<u64> {
    let decoder = png::Decoder::new(input);
    let reader = decoder.read_info().ok()?;
    let information = reader.info();
    let frame_count = information
        .animation_control
        .map_or(1, |control| u64::from(control.num_frames));
    Some(decoded_weight(
        information.width,
        information.height,
        4,
        frame_count,
    ))
}

/// The raster size tracks the largest monitor, like decode_svg.
fn probe_svg_weight(data: &[u8]) -> Option<u64> {
    let tree = parse_svg_tree(data).ok()?;
    let (pixel_width, pixel_height, _) = svg_raster_geometry(&tree)?;
    Some(decoded_weight(pixel_width, pixel_height, 4, 1))
}

/// The VP8X canvas sits in the header; counting frames would walk the file.
fn probe_webp_weight(input: &DecodeInput<'_>) -> Option<u64> {
    let owned;
    let header: &[u8] = match input {
        DecodeInput::File(path) => {
            owned = read_header(path)?;
            &owned
        }
        DecodeInput::Memory { data, .. } => data,
    };
    let dimension = |offset: usize| -> Option<u32> {
        let bytes = header.get(offset..offset + 3)?;
        Some(1 + (u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16))
    };
    Some(decoded_weight(dimension(24)?, dimension(27)?, 4, 1))
}

/// Extension-only: magic probing never yields RAW, and this runs on the UI thread.
pub fn is_raw_two_stage(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .and_then(|extension| descriptor_for_extension(&extension))
        .is_some_and(|descriptor| matches!(descriptor.adapter, Adapter::WicRawTwoStage))
}

pub fn decode_raw_preview(path: &Path, cancellation: &AtomicBool) -> Option<DecodedImage> {
    let decoded = with_wic_factory(|factory| {
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?
        };
        let preview =
            unsafe { decoder.GetPreview() }.or_else(|_| unsafe { decoder.GetThumbnail() })?;
        let frame = unsafe { decoder.GetFrame(0) }.ok();
        let orientation = frame.as_ref().map_or(1, exif_orientation);
        let icc_profile = frame
            .as_ref()
            .and_then(|frame| icc_profile_bytes(factory, frame));
        let exif = frame.as_ref().and_then(read_exif);
        let source = convert_to_pbgra(factory, &preview)?;
        let source = apply_orientation(factory, source, orientation)?;
        let (width, height) = source_size(&source)?;
        let (source, pixel_width, pixel_height) =
            downscale_to_device_limit(factory, source, width, height)?;
        let pixels = copy_pixels(&source, pixel_width, pixel_height, 4, cancellation)?;
        Ok(DecodedFrames {
            width,
            height,
            pixel_width,
            pixel_height,
            icc_profile,
            exif,
            storage: PixelStorage::Bgra8,
            source_bits_per_channel: 8,
            peak_luminance_nits: None,
            source_primaries: None,
            frames: vec![Frame {
                pixels,
                delay_milliseconds: 0,
            }],
            frames_truncated: false,
        })
    })
    .ok()?;
    Some(decoded.into_image("RAW"))
}

thread_local! {
    static WIC_FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

fn with_wic_factory<T>(
    operation: impl FnOnce(&IWICImagingFactory) -> WindowsResult<T>,
) -> WindowsResult<T> {
    WIC_FACTORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(unsafe {
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?
            });
        }
        operation(slot.as_ref().expect("WIC factory initialized"))
    })
}

struct DecodedFrames {
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    icc_profile: Option<Vec<u8>>,
    exif: Option<ExifInfo>,
    storage: PixelStorage,
    source_bits_per_channel: u32,
    peak_luminance_nits: Option<f32>,
    source_primaries: Option<[[f32; 2]; 3]>,
    frames: Vec<Frame>,
    frames_truncated: bool,
}

impl DecodedFrames {
    fn into_image(self, format_name: &'static str) -> DecodedImage {
        DecodedImage {
            width: self.width,
            height: self.height,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            format_name,
            icc_profile: self.icc_profile,
            exif: self.exif,
            storage: self.storage,
            source_bits_per_channel: self.source_bits_per_channel,
            peak_luminance_nits: self.peak_luminance_nits,
            source_primaries: self.source_primaries,
            frames: self.frames,
            frames_truncated: self.frames_truncated,
        }
    }
}

fn decode_with_wic(
    input: &DecodeInput<'_>,
    format_name: &'static str,
    semantics: &FrameSemantics,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let decoded = with_wic_factory(|factory| {
        let decoder = create_wic_decoder(factory, input)?;
        let frame_count = unsafe { decoder.GetFrameCount()? }.max(1);
        match semantics {
            FrameSemantics::Animation if frame_count > 1 => {
                decode_animation(factory, &decoder, frame_count, cancellation)
            }
            FrameSemantics::SizeVariants if frame_count > 1 => {
                decode_largest_frame(factory, &decoder, frame_count, cancellation)
            }
            _ => decode_single_frame(factory, &decoder, 0, cancellation),
        }
    })?;
    Ok(decoded.into_image(format_name))
}

fn create_wic_decoder(
    factory: &IWICImagingFactory,
    input: &DecodeInput<'_>,
) -> WindowsResult<IWICBitmapDecoder> {
    match input {
        DecodeInput::File(path) => unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
        },
        // The stream borrows the buffer; decoder and stream stay within this call.
        DecodeInput::Memory { data, .. } => unsafe {
            let stream = factory.CreateStream()?;
            stream.InitializeFromMemory(data)?;
            factory.CreateDecoderFromStream(
                &stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )
        },
    }
}

fn downscale_to_device_limit(
    factory: &IWICImagingFactory,
    source: IWICBitmapSource,
    width: u32,
    height: u32,
) -> WindowsResult<(IWICBitmapSource, u32, u32)> {
    let (scaled_width, scaled_height) = device_limited_size(width, height);
    if (scaled_width, scaled_height) == (width, height) {
        return Ok((source, width, height));
    }
    let scaler = unsafe { factory.CreateBitmapScaler()? };
    unsafe {
        scaler.Initialize(
            &source,
            scaled_width,
            scaled_height,
            WICBitmapInterpolationModeFant,
        )?
    };
    Ok((scaler.cast()?, scaled_width, scaled_height))
}

/// DP3 for fallback decoders: downscale oversized frames before upload; failure is a decode error.
fn enforce_device_limit(
    mut decoded: DecodedImage,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let (width, height) = (decoded.pixel_width, decoded.pixel_height);
    if width.max(height) <= MAXIMUM_TEXTURE_DIMENSION {
        return Ok(decoded);
    }
    let pixel_format = match decoded.storage {
        PixelStorage::Bgra8 => &GUID_WICPixelFormat32bppPBGRA,
        PixelStorage::RgbaHalf => &GUID_WICPixelFormat64bppPRGBAHalf,
    };
    let bytes_per_pixel = decoded.storage.bytes_per_pixel();
    // u64 stride: a native EXR/HEIF width near u32::MAX would overflow width*bpp.
    let stride = u32::try_from(u64::from(width) * u64::from(bytes_per_pixel))
        .map_err(|_| uncoded_error("image stride exceeds the addressable range"))?;
    let frame = decoded
        .frames
        .first_mut()
        .ok_or_else(|| uncoded_error("image has no frames"))?;
    let (pixels, scaled_width, scaled_height) = with_wic_factory(|factory| {
        let bitmap = unsafe {
            factory.CreateBitmapFromMemory(width, height, pixel_format, stride, &frame.pixels)?
        };
        let (source, scaled_width, scaled_height) =
            downscale_to_device_limit(factory, bitmap.cast()?, width, height)?;
        let pixels = copy_pixels(
            &source,
            scaled_width,
            scaled_height,
            bytes_per_pixel,
            cancellation,
        )?;
        Ok((pixels, scaled_width, scaled_height))
    })?;
    frame.pixels = pixels;
    decoded.pixel_width = scaled_width;
    decoded.pixel_height = scaled_height;
    Ok(decoded)
}

fn decode_single_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    index: u32,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedFrames> {
    let frame = unsafe { decoder.GetFrame(index)? };
    let orientation = exif_orientation(&frame);
    let icc_profile = icc_profile_bytes(factory, &frame);
    let exif = read_exif(&frame);
    let (native_bits_per_channel, float_native) = frame_pixel_format_info(factory, &frame);
    let high_depth = native_bits_per_channel > 8;
    // PQ/HLG integers bypass WIC's sRGB-assuming float conversion.
    let hdr_encoding = if float_native {
        None
    } else {
        icc_profile.as_deref().and_then(icc_hdr_encoding)
    };
    let (frame_width, frame_height) = source_size(&frame.cast()?)?;
    let oversized = frame_width.max(frame_height) > MAXIMUM_TEXTURE_DIMENSION;
    // The Fant scaler rejects half floats; oversized integers scale first, convert after.
    let deferred_half = high_depth && !float_native && hdr_encoding.is_none() && oversized;
    let (source, storage) = if high_depth {
        // PQ/HLG and deferred sources stay integer at this stage; the rest go straight to half.
        let target = if hdr_encoding.is_some() || deferred_half {
            &GUID_WICPixelFormat64bppRGBA
        } else {
            &GUID_WICPixelFormat64bppPRGBAHalf
        };
        convert_half_or_pbgra(factory, &frame.cast()?, target)?
    } else {
        (
            convert_to_pbgra(factory, &frame.cast()?)?,
            PixelStorage::Bgra8,
        )
    };
    // The 8bpc retreat loses the PQ/HLG code values along with the depth.
    let hdr_encoding = hdr_encoding.filter(|_| storage == PixelStorage::RgbaHalf);
    let source = apply_orientation(factory, source, orientation)?;
    let (width, height) = source_size(&source)?;
    let (source, pixel_width, pixel_height, storage) =
        match downscale_to_device_limit(factory, source.clone(), width, height) {
            Ok((scaled, scaled_width, scaled_height)) => {
                (scaled, scaled_width, scaled_height, storage)
            }
            // Refused formats retreat to 8-bit; PQ/HLG keeps the error to avoid false colors.
            Err(_) if storage == PixelStorage::RgbaHalf && hdr_encoding.is_none() => {
                let fallback = convert_to_pbgra(factory, &source)?;
                let (scaled, scaled_width, scaled_height) =
                    downscale_to_device_limit(factory, fallback, width, height)?;
                (scaled, scaled_width, scaled_height, PixelStorage::Bgra8)
            }
            Err(error) => return Err(error),
        };
    let (source, storage) = if deferred_half && storage == PixelStorage::RgbaHalf {
        convert_half_or_pbgra(factory, &source, &GUID_WICPixelFormat64bppPRGBAHalf)?
    } else {
        (source, storage)
    };
    let mut pixels = copy_pixels(
        &source,
        pixel_width,
        pixel_height,
        storage.bytes_per_pixel(),
        cancellation,
    )?;
    let linearized_maximum_bits =
        hdr_encoding.map(|encoding| linearize_hdr_pixels(&mut pixels, encoding));
    // Half-stored pixels are linear light regardless of the conversion route.
    let peak_luminance_nits = (storage == PixelStorage::RgbaHalf)
        .then(|| match linearized_maximum_bits {
            Some(maximum_bits) => peak_luminance_with_maximum_bits(&pixels, maximum_bits),
            None => peak_luminance_from_half_pixels(&pixels),
        })
        .flatten();
    // The 8bpc fallback conversion truncates whatever the native format held.
    let source_bits_per_channel = if storage == PixelStorage::RgbaHalf {
        native_bits_per_channel
    } else {
        8
    };
    // Only a conversion riv drove keeps the source primaries; a float native is scRGB already.
    let source_primaries = match hdr_encoding {
        Some(encoding) => Some(encoding.source_primaries()),
        None if storage == PixelStorage::RgbaHalf && !float_native => {
            icc_profile.as_deref().and_then(icc_primaries)
        }
        None => None,
    };
    Ok(DecodedFrames {
        width,
        height,
        pixel_width,
        pixel_height,
        icc_profile,
        exif,
        storage,
        source_bits_per_channel,
        peak_luminance_nits,
        source_primaries,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
        frames_truncated: false,
    })
}

/// Native format traits: (bits per channel, float representation).
fn frame_pixel_format_info(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> (u32, bool) {
    (|| -> WindowsResult<(u32, bool)> {
        let format = unsafe { frame.GetPixelFormat()? };
        let information: IWICPixelFormatInfo2 =
            unsafe { factory.CreateComponentInfo(&raw const format)? }.cast()?;
        let bits_per_pixel = unsafe { information.GetBitsPerPixel()? };
        let channel_count = unsafe { information.GetChannelCount()? };
        let float_native = unsafe { information.GetNumericRepresentation()? }
            == WICPixelFormatNumericRepresentationFloat;
        if channel_count == 0 {
            return Ok((8, float_native));
        }
        Ok((bits_per_pixel / channel_count, float_native))
    })()
    .unwrap_or((8, false))
}

#[derive(Clone, Copy)]
enum HdrTransfer {
    PerceptualQuantizer,
    HybridLogGamma,
}

#[derive(Clone, Copy)]
enum HdrPrimaries {
    Bt709,
    Bt2020,
    DisplayP3,
}

#[derive(Clone, Copy)]
pub(crate) struct HdrEncoding {
    transfer: HdrTransfer,
    primaries: HdrPrimaries,
}

impl HdrEncoding {
    /// CIE xy of the primaries the linearized pixels keep; all three are D65.
    pub(crate) fn source_primaries(self) -> [[f32; 2]; 3] {
        match self.primaries {
            HdrPrimaries::Bt709 => color::BT709_PRIMARIES,
            HdrPrimaries::Bt2020 => color::BT2020_PRIMARIES,
            HdrPrimaries::DisplayP3 => color::DISPLAY_P3_PRIMARIES,
        }
    }
}

/// Big-endian u32 at the offset, when in bounds.
fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Data offset of the first tag-table entry with this signature.
fn icc_tag_offset(icc: &[u8], signature: &[u8; 4]) -> Option<usize> {
    let tag_count = read_u32_be(icc, 128)? as usize;
    for index in 0..tag_count {
        let entry = 132 + index * 12;
        if icc.get(entry..entry + 4)? == signature {
            return read_u32_be(icc, entry + 4).map(|offset| offset as usize);
        }
    }
    None
}

/// Big-endian u16 at the offset, when in bounds.
fn read_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// The signed 16.16 fixed point every ICC numeric tag is written in.
fn read_s15_fixed16(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(read_u32_be(bytes, offset)? as i32 as f32 / 65536.0)
}

/// True when two runs of floats agree everywhere within the tolerance.
fn all_within<'a>(
    one: impl IntoIterator<Item = &'a f32>,
    other: impl IntoIterator<Item = &'a f32>,
    tolerance: f32,
) -> bool {
    one.into_iter()
        .zip(other)
        .all(|(one, other)| (one - other).abs() < tolerance)
}

/// Points where tone curves are compared; close enough to separate the sRGB toe from a gamma.
const TONE_CURVE_SAMPLES: usize = 33;
/// Curves this close describe one response; a pure gamma 2.2 leaves the sRGB curve by 4e-3.
const TONE_CURVE_TOLERANCE: f32 = 1e-3;
/// Primaries this close in xy describe one gamut; the nearest named gamuts sit 0.04 apart.
const PRIMARY_TOLERANCE: f32 = 2e-3;
/// Colorants this close describe one space, scale included.
const COLORANT_TOLERANCE: f32 = 1e-3;

fn tone_curve_input(index: usize) -> f32 {
    index as f32 / (TONE_CURVE_SAMPLES - 1) as f32
}

/// True when the profile describes the space sRGB does.
pub fn icc_is_srgb(icc: &[u8]) -> bool {
    // Chromaticity, not the stored colorants: two vendors round their sRGB differently.
    let Some(primaries) = icc_primaries(icc) else {
        return false;
    };
    if !all_within(
        primaries.iter().flatten(),
        color::BT709_PRIMARIES.iter().flatten(),
        PRIMARY_TOLERANCE,
    ) {
        return false;
    }
    let reference: [f32; TONE_CURVE_SAMPLES] =
        std::array::from_fn(|index| color::srgb_to_linear(tone_curve_input(index)));
    icc_tone_curves(icc).is_some_and(|curves| {
        curves
            .iter()
            .all(|curve| all_within(curve, &reference, TONE_CURVE_TOLERANCE))
    })
}

/// True when both profiles describe one space, so converting between them changes nothing.
pub fn icc_same_space(one: &[u8], other: &[u8]) -> bool {
    // Both sides sit in the D50 connection space, and their scale carries the medium white.
    let (Some(one_colorants), Some(other_colorants)) = (icc_colorants(one), icc_colorants(other))
    else {
        return false;
    };
    if !all_within(
        one_colorants.iter().flatten(),
        other_colorants.iter().flatten(),
        COLORANT_TOLERANCE,
    ) {
        return false;
    }
    let (Some(one), Some(other)) = (icc_tone_curves(one), icc_tone_curves(other)) else {
        return false;
    };
    all_within(
        one.iter().flatten(),
        other.iter().flatten(),
        TONE_CURVE_TOLERANCE,
    )
}

/// R, G, B tone curves sampled over [0, 1]; None when one is missing or of a form riv cannot read.
fn icc_tone_curves(icc: &[u8]) -> Option<[[f32; TONE_CURVE_SAMPLES]; 3]> {
    Some([
        icc_tone_curve(icc, b"rTRC")?,
        icc_tone_curve(icc, b"gTRC")?,
        icc_tone_curve(icc, b"bTRC")?,
    ])
}

fn icc_tone_curve(icc: &[u8], tag: &[u8; 4]) -> Option<[f32; TONE_CURVE_SAMPLES]> {
    let offset = icc_tag_offset(icc, tag)?;
    match icc.get(offset..offset + 4)? {
        b"curv" => {
            let count = read_u32_be(icc, offset + 8)? as usize;
            match count {
                // No entries is the identity; one entry is a u8Fixed8 gamma.
                0 => Some(std::array::from_fn(tone_curve_input)),
                1 => {
                    let gamma = f32::from(read_u16_be(icc, offset + 12)?) / 256.0;
                    Some(std::array::from_fn(|index| {
                        tone_curve_input(index).powf(gamma)
                    }))
                }
                _ => {
                    let entry = |index: usize| -> Option<f32> {
                        Some(f32::from(read_u16_be(icc, offset + 12 + index * 2)?) / 65535.0)
                    };
                    let mut samples = [0.0f32; TONE_CURVE_SAMPLES];
                    for (index, sample) in samples.iter_mut().enumerate() {
                        let position = tone_curve_input(index) * (count - 1) as f32;
                        let low = position.floor() as usize;
                        let high = (low + 1).min(count - 1);
                        let fraction = position - low as f32;
                        *sample = entry(low)? * (1.0 - fraction) + entry(high)? * fraction;
                    }
                    Some(samples)
                }
            }
        }
        b"para" => {
            let parameter = |index: usize| read_s15_fixed16(icc, offset + 12 + index * 4);
            let gamma = parameter(0)?;
            let function = read_u16_be(icc, offset + 8)?;
            // Every function is (a*x + b)^g + e above d, and c*x + f below it.
            let (a, b, slope, split, above, below) = match function {
                0 => (1.0, 0.0, 0.0, f32::MIN, 0.0, 0.0),
                1 | 2 => {
                    let (a, b) = (parameter(1)?, parameter(2)?);
                    if a == 0.0 {
                        return None;
                    }
                    let constant = if function == 2 { parameter(3)? } else { 0.0 };
                    (a, b, 0.0, -b / a, constant, constant)
                }
                3 => (
                    parameter(1)?,
                    parameter(2)?,
                    parameter(3)?,
                    parameter(4)?,
                    0.0,
                    0.0,
                ),
                4 => (
                    parameter(1)?,
                    parameter(2)?,
                    parameter(3)?,
                    parameter(4)?,
                    parameter(5)?,
                    parameter(6)?,
                ),
                _ => return None,
            };
            Some(std::array::from_fn(|index| {
                let input = tone_curve_input(index);
                if input >= split {
                    (a * input + b).max(0.0).powf(gamma) + above
                } else {
                    slope * input + below
                }
            }))
        }
        _ => None,
    }
}

/// Nearest gamut label from an ICC's matrix primaries; None for non-matrix profiles.
pub fn icc_gamut_label(icc: &[u8]) -> Option<&'static str> {
    Some(crate::image::color::nearest_gamut_label(icc_primaries(
        icc,
    )?))
}

/// Bradford adaptation out of the ICC PCS white (D50) into the D65 named gamuts use.
const D50_TO_D65: [[f32; 3]; 3] = [
    [0.955_577, -0.023_039, 0.063_164],
    [-0.028_290, 1.009_942, 0.021_008],
    [0.012_298, -0.020_483, 1.329_91],
];

/// R, G, B colorants as the profile stores them, in the D50 PCS; None for non-matrix profiles.
fn icc_colorants(icc: &[u8]) -> Option<[[f32; 3]; 3]> {
    let column = |tag: &[u8; 4]| -> Option<[f32; 3]> {
        let offset = icc_tag_offset(icc, tag)?;
        if icc.get(offset..offset + 4)? != b"XYZ " {
            return None;
        }
        Some([
            read_s15_fixed16(icc, offset + 8)?,
            read_s15_fixed16(icc, offset + 12)?,
            read_s15_fixed16(icc, offset + 16)?,
        ])
    };
    Some([column(b"rXYZ")?, column(b"gXYZ")?, column(b"bXYZ")?])
}

/// CIE xy of an ICC's matrix primaries, D65 referred; None for non-matrix profiles.
pub fn icc_primaries(icc: &[u8]) -> Option<[[f32; 2]; 3]> {
    let mut primaries = [[0.0f32; 2]; 3];
    for (stored, primary) in icc_colorants(icc)?.iter().zip(&mut primaries) {
        // The colorants are stored against the D50 PCS; adapt out of it to name the gamut.
        let mut adapted = [0.0f32; 3];
        for (row, channel) in D50_TO_D65.iter().zip(&mut adapted) {
            *channel = row[0] * stored[0] + row[1] * stored[1] + row[2] * stored[2];
        }
        let sum = adapted[0] + adapted[1] + adapted[2];
        if sum <= 0.0 {
            return None;
        }
        *primary = [adapted[0] / sum, adapted[1] / sum];
    }
    Some(primaries)
}

/// Human-readable profile name from the ICC 'desc' tag (v2 text or v4 mluc).
pub fn icc_profile_description(icc: &[u8]) -> Option<String> {
    let offset = icc_tag_offset(icc, b"desc")?;
    let description = match icc.get(offset..offset + 4)? {
        b"desc" => {
            let length = read_u32_be(icc, offset + 8)? as usize;
            let bytes = icc.get(offset + 12..offset + 12 + length)?;
            let end = bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(bytes.len());
            String::from_utf8(bytes[..end].to_vec()).ok()?
        }
        b"mluc" => {
            // First record: length at +20, offset at +24, UTF-16BE.
            let length = read_u32_be(icc, offset + 20)? as usize;
            let start = offset + read_u32_be(icc, offset + 24)? as usize;
            let bytes = icc.get(start..start + length)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16(&units).ok()?
        }
        _ => return None,
    };
    let trimmed = description.trim_matches(['\0', ' ', '\t', '\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// CICP code points; Some for an HDR transfer (16 = PQ, 18 = HLG) with convertible primaries.
pub(crate) fn cicp_hdr_encoding(primaries: u8, transfer: u8) -> Option<HdrEncoding> {
    const TRANSFER_PQ: u8 = 16;
    const TRANSFER_HLG: u8 = 18;
    const PRIMARIES_BT709: u8 = 1;
    const PRIMARIES_BT2020: u8 = 9;
    const PRIMARIES_P3_D65: u8 = 12;
    let transfer = match transfer {
        TRANSFER_PQ => HdrTransfer::PerceptualQuantizer,
        TRANSFER_HLG => HdrTransfer::HybridLogGamma,
        _ => return None,
    };
    let primaries = match primaries {
        PRIMARIES_BT709 => HdrPrimaries::Bt709,
        PRIMARIES_BT2020 => HdrPrimaries::Bt2020,
        PRIMARIES_P3_D65 => HdrPrimaries::DisplayP3,
        _ => return None,
    };
    Some(HdrEncoding {
        transfer,
        primaries,
    })
}

/// ICC 'cicp' tag as an HDR encoding.
fn icc_hdr_encoding(icc: &[u8]) -> Option<HdrEncoding> {
    let offset = icc_tag_offset(icc, b"cicp")?;
    if icc.get(offset..offset + 4)? != b"cicp" {
        return None;
    }
    cicp_hdr_encoding(*icc.get(offset + 8)?, *icc.get(offset + 9)?)
}

/// Rec. 2100 nominal peak for the HLG OOTF (display-referred mapping).
const HLG_PEAK_NITS: f32 = 1000.0;

/// Exact 16-bit code lookup; the transfer functions cost two powf per call.
fn hdr_transfer_lookup_table(transfer: HdrTransfer) -> &'static [f32; 65536] {
    static PERCEPTUAL_QUANTIZER_TABLE: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    static HYBRID_LOG_GAMMA_TABLE: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    let (slot, function): (_, fn(f32) -> f32) = match transfer {
        HdrTransfer::PerceptualQuantizer => {
            (&PERCEPTUAL_QUANTIZER_TABLE, perceptual_quantizer_nits as _)
        }
        HdrTransfer::HybridLogGamma => {
            (&HYBRID_LOG_GAMMA_TABLE, hybrid_log_gamma_scene_linear as _)
        }
    };
    slot.get_or_init(|| {
        let mut table = Box::new([0.0f32; 65536]);
        for (code, value) in table.iter_mut().enumerate() {
            *value = function(code as f32 / f32::from(u16::MAX));
        }
        table
    })
}

/// The renderer's D3D11 device shared with the workers; free-threaded for creation.
#[derive(Clone)]
pub struct UploadDevice {
    pub device: ID3D11Device,
    pub generation: u64,
    /// This adapter's per-resource ceiling; larger frames stay on the UI-thread path.
    pub maximum_frame_bytes: u64,
}

/// A texture uploaded off the UI thread, usable only while its generation is current.
#[derive(Clone)]
pub struct UploadedTexture {
    pub texture: ID3D11Texture2D,
    pub generation: u64,
}

/// The documented D3D11 resource limit: min(max(128, 0.25 x dedicated VRAM), 2048) MB.
pub fn maximum_resource_bytes(dedicated_video_memory: u64) -> u64 {
    const MEGABYTE: u64 = 1024 * 1024;
    (dedicated_video_memory / 4).clamp(128 * MEGABYTE, 2048 * MEGABYTE)
}

/// Uploads a still frame on the worker; None leaves the UI-thread upload path.
pub fn upload_still_texture(
    upload_device: &UploadDevice,
    image: &DecodedImage,
) -> Option<UploadedTexture> {
    let frame = match image.frames.as_slice() {
        [frame] => frame,
        _ => return None,
    };
    if frame.pixels.len() != image.frame_byte_length()
        || frame.pixels.len() as u64 > upload_device.maximum_frame_bytes
    {
        return None;
    }
    let description = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..image.texture_description()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: frame.pixels.as_ptr().cast(),
        SysMemPitch: image.row_pitch(),
        ..Default::default()
    };
    let mut texture = None;
    unsafe {
        upload_device.device.CreateTexture2D(
            &raw const description,
            Some(&raw const data),
            Some(&raw mut texture),
        )
    }
    .ok()?;
    texture.map(|texture| UploadedTexture {
        texture,
        generation: upload_device.generation,
    })
}

/// Pixels per worker block; smaller buffers stay on one thread.
const PARALLEL_BLOCK_MINIMUM_PIXELS: usize = 262_144;

/// Block size in bytes: up to one block per core, each a whole number of pixels.
fn parallel_block_bytes(total_bytes: usize, bytes_per_pixel: usize) -> usize {
    let pixel_count = total_bytes / bytes_per_pixel;
    let cores = std::thread::available_parallelism().map_or(1, |count| count.get());
    let blocks = cores
        .min(pixel_count / PARALLEL_BLOCK_MINIMUM_PIXELS)
        .max(1);
    pixel_count.div_ceil(blocks) * bytes_per_pixel
}

/// Runs `work` over disjoint pixel blocks on all cores, collecting results in order.
fn map_pixel_blocks<Output: Send>(
    pixels: &mut [u8],
    bytes_per_pixel: usize,
    work: impl Fn(&mut [u8]) -> Output + Sync,
) -> Vec<Output> {
    let block_bytes = parallel_block_bytes(pixels.len(), bytes_per_pixel);
    if block_bytes >= pixels.len() {
        return vec![work(pixels)];
    }
    std::thread::scope(|scope| {
        let work = &work;
        let mut blocks: Vec<&mut [u8]> = pixels.chunks_mut(block_bytes).collect();
        let last = blocks.pop();
        let handles: Vec<_> = blocks
            .into_iter()
            .map(|block| scope.spawn(move || work(block)))
            .collect();
        // The calling thread takes the last block instead of idling in join.
        let last_output = last.map(&work);
        let mut outputs: Vec<Output> = handles
            .into_iter()
            .map(|handle| handle.join().expect("pixel block worker panicked"))
            .collect();
        outputs.extend(last_output);
        outputs
    })
}

/// PQ/HLG codes to premultiplied linear halves; returns the maximum color half written.
pub(crate) fn linearize_hdr_pixels(pixels: &mut [u8], encoding: HdrEncoding) -> u16 {
    let transfer_table = hdr_transfer_lookup_table(encoding.transfer);
    map_pixel_blocks(pixels, 8, |block| {
        linearize_block(block, transfer_table, encoding)
    })
    .into_iter()
    .max()
    .unwrap_or(0)
}

fn linearize_block(pixels: &mut [u8], transfer_table: &[f32; 65536], encoding: HdrEncoding) -> u16 {
    let mut maximum_bits = 0u16;
    for pixel in pixels.chunks_exact_mut(8) {
        let mut channel_nits = [0.0f32; 3];
        for (channel, nits) in channel_nits.iter_mut().enumerate() {
            let code = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
            *nits = transfer_table[usize::from(code)];
        }
        if matches!(encoding.transfer, HdrTransfer::HybridLogGamma) {
            // BT.2100 OOTF: display = peak * scene_luminance^0.2 * scene.
            let scene_luminance =
                0.2627 * channel_nits[0] + 0.6780 * channel_nits[1] + 0.0593 * channel_nits[2];
            let display_scale = HLG_PEAK_NITS * scene_luminance.max(0.0).powf(0.2);
            for nits in &mut channel_nits {
                *nits *= display_scale;
            }
        }
        let alpha = f32::from(u16::from_le_bytes([pixel[6], pixel[7]])) / f32::from(u16::MAX);
        for (channel, nits) in channel_nits.iter().enumerate() {
            let premultiplied = nits / SDR_REFERENCE_WHITE_NITS * alpha;
            let half = f32_to_half(premultiplied);
            maximum_bits = maximum_bits.max(positive_normal_half_bits(half));
            pixel[channel * 2..channel * 2 + 2].copy_from_slice(&half.to_le_bytes());
        }
        pixel[6..8].copy_from_slice(&f32_to_half(alpha).to_le_bytes());
    }
    maximum_bits
}

/// BT.2100 HLG inverse OETF (code -> scene linear, 1.0 at nominal peak).
fn hybrid_log_gamma_scene_linear(code: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let code = code.max(0.0);
    if code <= 0.5 {
        (code * code) / 3.0
    } else {
        (((code - C) / A).exp() + B) / 12.0
    }
}

/// Peak-scan histogram resolution in PQ code space.
const PEAK_HISTOGRAM_BINS: usize = 4096;

/// Histogram bin per half bit pattern; built once, two powf per entry.
fn peak_histogram_bin_table() -> &'static [u16; 65536] {
    static TABLE: OnceLock<Box<[u16; 65536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Box::new([0u16; 65536]);
        for (bits, bin) in table.iter_mut().enumerate() {
            let code =
                perceptual_quantizer_code(half_to_f32(bits as u16) * SDR_REFERENCE_WHITE_NITS);
            *bin = ((code.clamp(0.0, 1.0) * (PEAK_HISTOGRAM_BINS - 1) as f32) as usize)
                .min(PEAK_HISTOGRAM_BINS - 1) as u16;
        }
        table
    })
}

/// Content peak of linear halves: 99.9th-percentile max channel, binned in PQ codes.
pub fn peak_luminance_from_half_pixels(pixels: &[u8]) -> Option<f32> {
    let channel_maxima = channel_maxima_from_half_pixels(pixels);
    peak_luminance_with_maximum_bits(
        pixels,
        channel_maxima[0]
            .max(channel_maxima[1])
            .max(channel_maxima[2]),
    )
}

/// Peak from a known channel maximum, skipping the scan that would recompute it.
pub(crate) fn peak_luminance_with_maximum_bits(pixels: &[u8], maximum_bits: u16) -> Option<f32> {
    if pixels.len() < 8 {
        return None;
    }
    let maximum_linear = half_to_f32(maximum_bits);
    if maximum_linear <= 1.0 {
        // Entirely within SDR white: the tone map is skipped, so skip the histogram.
        return Some(maximum_linear * SDR_REFERENCE_WHITE_NITS);
    }
    // Jittered subsampling: a fixed stride aliases with periodic image structure.
    const SUBSAMPLE_MINIMUM_PIXELS: usize = 4_000_000;
    let bin_table = peak_histogram_bin_table();
    let pixel_count = pixels.len() / 8;
    let subsample = pixel_count >= SUBSAMPLE_MINIMUM_PIXELS;
    let mut histogram = [0u32; PEAK_HISTOGRAM_BINS];
    let mut sample_count = 0u32;
    let mut jitter_state = 0x9E37_79B9u32;
    let mut index = 0usize;
    while index < pixel_count {
        let pixel = &pixels[index * 8..index * 8 + 8];
        let mut maximum_bits = 0u16;
        for channel in 0..3 {
            let bits = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
            maximum_bits = maximum_bits.max(positive_normal_half_bits(bits));
        }
        histogram[usize::from(bin_table[usize::from(maximum_bits)])] += 1;
        sample_count += 1;
        if subsample {
            jitter_state = jitter_state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            index += 1 + (jitter_state >> 29) as usize;
        } else {
            index += 1;
        }
    }
    let threshold = (u64::from(sample_count) * 999 / 1000) as u32;
    let mut accumulated = 0u32;
    let mut percentile_bin = PEAK_HISTOGRAM_BINS - 1;
    for (bin, count) in histogram.iter().enumerate() {
        accumulated += count;
        if accumulated >= threshold {
            percentile_bin = bin;
            break;
        }
    }
    let code = (percentile_bin as f32 + 1.0) / PEAK_HISTOGRAM_BINS as f32;
    Some(perceptual_quantizer_nits(code.min(1.0)))
}

/// Per-channel maxima. Four uniform lanes; the discarded alpha keeps the
/// stride pattern regular.
fn channel_maxima_from_half_pixels(pixels: &[u8]) -> [u16; 4] {
    let mut channel_maxima = [0u16; 4];
    for pixel in pixels.chunks_exact(8) {
        for (channel, maximum) in channel_maxima.iter_mut().enumerate() {
            let bits = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
            *maximum = (*maximum).max(positive_normal_half_bits(bits));
        }
    }
    channel_maxima
}

/// Bits of a positive normal half (others map to 0); valid bits order like their values.
fn positive_normal_half_bits(bits: u16) -> u16 {
    if bits.wrapping_sub(0x0400) < 0x7800 {
        bits
    } else {
        0
    }
}

/// Peak-scan only: negatives, subnormals, and non-finite values map to 0.
fn half_to_f32(bits: u16) -> f32 {
    let exponent = (bits >> 10) & 0x1F;
    if bits & 0x8000 != 0 || exponent == 0 || exponent == 31 {
        return 0.0;
    }
    let mantissa = u32::from(bits & 0x03FF);
    f32::from_bits(((u32::from(exponent) + 112) << 23) | (mantissa << 13))
}

/// Round-to-nearest-even; overflow clamps to the half maximum, NaN maps to 0.
fn f32_to_half(value: f32) -> u16 {
    const HALF_MAXIMUM: f32 = 65504.0;
    const MINIMUM_NORMAL: f32 = 6.103_515_6e-5; // 2^-14
    const SUBNORMAL_UNIT: f32 = 5.960_464_5e-8; // 2^-24
    if value.is_nan() {
        return 0;
    }
    let sign = if value.is_sign_negative() { 0x8000 } else { 0 };
    let magnitude = value.abs();
    if magnitude < MINIMUM_NORMAL {
        let units = (magnitude / SUBNORMAL_UNIT).round() as u16;
        return sign | units.min(0x03FF);
    }
    if magnitude > HALF_MAXIMUM {
        return sign | 0x7BFF;
    }
    let bits = magnitude.to_bits();
    let exponent = ((bits >> 23) & 0xFF) - 112;
    let mantissa = bits & 0x007F_FFFF;
    let mut half = (exponent << 10) | (mantissa >> 13);
    let remainder = mantissa & 0x1FFF;
    if remainder > 0x1000 || (remainder == 0x1000 && half & 1 == 1) {
        half += 1;
    }
    sign | half as u16
}

fn icc_profile_bytes(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> Option<Vec<u8>> {
    let mut count = 0u32;
    unsafe { frame.GetColorContexts(&mut [], &raw mut count) }.ok()?;
    if count == 0 {
        return None;
    }
    let mut contexts: Vec<Option<IWICColorContext>> = (0..count)
        .map(|_| unsafe { factory.CreateColorContext() }.ok())
        .collect();
    if contexts.iter().any(Option::is_none) {
        return None;
    }
    let mut actual_count = 0u32;
    unsafe { frame.GetColorContexts(&mut contexts, &raw mut actual_count) }.ok()?;
    for context in contexts.into_iter().flatten() {
        if unsafe { context.GetType() } != Ok(WICColorContextProfile) {
            continue;
        }
        let mut size = 0u32;
        let _ = unsafe { context.GetProfileBytes(&mut [], &raw mut size) };
        if size == 0 {
            continue;
        }
        let mut buffer = vec![0u8; size as usize];
        let mut written = 0u32;
        unsafe { context.GetProfileBytes(&mut buffer, &raw mut written) }.ok()?;
        buffer.truncate(written as usize);
        return Some(buffer);
    }
    None
}

fn decode_largest_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frame_count: u32,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedFrames> {
    let largest_index = largest_frame_index(decoder, frame_count)?;
    decode_single_frame(factory, decoder, largest_index, cancellation)
}

/// Index of the frame with the most pixels; the size-variant display rule.
fn largest_frame_index(decoder: &IWICBitmapDecoder, frame_count: u32) -> WindowsResult<u32> {
    let mut largest_index = 0;
    let mut largest_pixels = 0u64;
    for index in 0..frame_count {
        let frame = unsafe { decoder.GetFrame(index)? };
        let (width, height) = source_size(&frame.cast()?)?;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > largest_pixels {
            largest_pixels = pixels;
            largest_index = index;
        }
    }
    Ok(largest_index)
}

struct FrameMetadata {
    left: u32,
    top: u32,
    delay_milliseconds: u32,
    disposal: u32,
}

fn frame_metadata(frame: &IWICBitmapFrameDecode) -> FrameMetadata {
    let reader = unsafe { frame.GetMetadataQueryReader() }.ok();
    let query = |name: PCWSTR| reader.as_ref().and_then(|reader| query_u32(reader, name));

    let delay_milliseconds = query(w!("/grctlext/Delay"))
        .map(|centiseconds| centiseconds * 10)
        .filter(|milliseconds| *milliseconds >= 20)
        .unwrap_or(100);
    FrameMetadata {
        left: query(w!("/imgdesc/Left")).unwrap_or(0),
        top: query(w!("/imgdesc/Top")).unwrap_or(0),
        delay_milliseconds,
        disposal: query(w!("/grctlext/Disposal")).unwrap_or(0),
    }
}

fn decode_animation(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frame_count: u32,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedFrames> {
    let container_reader = unsafe { decoder.GetMetadataQueryReader() }.ok();
    let container_query = |name: PCWSTR| {
        container_reader
            .as_ref()
            .and_then(|reader| query_u32(reader, name))
    };
    let mut canvas_width = container_query(w!("/logscrdesc/Width")).unwrap_or(0);
    let mut canvas_height = container_query(w!("/logscrdesc/Height")).unwrap_or(0);

    let mut canvas: Vec<u8> = Vec::new();
    let mut frames = Vec::with_capacity(frame_count as usize);
    let mut frames_truncated = false;
    let mut icc_profile = None;
    for index in 0..frame_count {
        if cancellation.load(Ordering::Relaxed) {
            return Err(E_ABORT.into());
        }
        // The container's frame count is real, so the budget is known after frame one.
        if !frames.is_empty() && animation_budget_exceeded(u64::from(frame_count), canvas.len()) {
            frames_truncated = true;
            break;
        }
        let frame = unsafe { decoder.GetFrame(index)? };
        if index == 0 {
            icc_profile = icc_profile_bytes(factory, &frame);
        }
        let metadata = frame_metadata(&frame);
        let source = convert_to_pbgra(factory, &frame.cast()?)?;
        let (frame_width, frame_height) = source_size(&source)?;
        if canvas_width == 0 || canvas_height == 0 {
            canvas_width = frame_width;
            canvas_height = frame_height;
        }
        if canvas.is_empty() {
            canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
        }
        let frame_pixels = copy_pixels(&source, frame_width, frame_height, 4, cancellation)?;

        let restore_previous = (metadata.disposal == 3).then(|| canvas.clone());
        blend_over(
            &mut canvas,
            canvas_width,
            canvas_height,
            &frame_pixels,
            frame_width,
            frame_height,
            metadata.left,
            metadata.top,
        );
        frames.push(Frame {
            pixels: canvas.clone(),
            delay_milliseconds: metadata.delay_milliseconds,
        });

        match (metadata.disposal, restore_previous) {
            (2, _) => clear_rectangle(
                &mut canvas,
                canvas_width,
                metadata.left,
                metadata.top,
                frame_width,
                frame_height,
            ),
            (3, Some(previous)) => canvas = previous,
            _ => {}
        }
    }
    Ok(DecodedFrames {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        icc_profile,
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: 8,
        peak_luminance_nits: None,
        source_primaries: None,
        frames,
        frames_truncated,
    })
}

/// Premultiplied source-over blend, clipped to the canvas.
#[expect(clippy::too_many_arguments)]
pub fn blend_over(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let visible_width = source_width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = source_height.min(canvas_height.saturating_sub(top)) as usize;
    for row in 0..visible_height {
        let source_start = row * source_width as usize * 4;
        let canvas_start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        let source_row = &source[source_start..source_start + visible_width * 4];
        let canvas_row = &mut canvas[canvas_start..canvas_start + visible_width * 4];
        // Branch-free over-composite; premultiplied sources make the alpha 0/255 shortcuts redundant.
        for (canvas_pixel, source_pixel) in canvas_row
            .chunks_exact_mut(4)
            .zip(source_row.chunks_exact(4))
        {
            let inverse_alpha = 255 - u32::from(source_pixel[3]);
            for (canvas_channel, source_channel) in canvas_pixel.iter_mut().zip(source_pixel) {
                let blended = u32::from(*source_channel)
                    + (u32::from(*canvas_channel) * inverse_alpha + 127) / 255;
                *canvas_channel = blended.min(255) as u8;
            }
        }
    }
}

pub fn clear_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
) {
    if canvas_width == 0 {
        return; // a zero-width canvas has nothing to clear and would divide by zero
    }
    let canvas_height = canvas.len() / (canvas_width as usize * 4);
    let visible_width = width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = (height as usize).min(canvas_height.saturating_sub(top as usize));
    for row in 0..visible_height {
        let start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        canvas[start..start + visible_width * 4].fill(0);
    }
}

fn convert_to_pbgra(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
) -> WindowsResult<IWICBitmapSource> {
    convert_pixel_format(factory, source, &GUID_WICPixelFormat32bppPBGRA)
}

/// Converts to the requested half-domain format, retreating to 8-bit PBGRA on refusal.
fn convert_half_or_pbgra(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
    target: &windows::core::GUID,
) -> WindowsResult<(IWICBitmapSource, PixelStorage)> {
    match convert_pixel_format(factory, source, target) {
        Ok(converted) => Ok((converted, PixelStorage::RgbaHalf)),
        Err(_) => Ok((convert_to_pbgra(factory, source)?, PixelStorage::Bgra8)),
    }
}

fn convert_pixel_format(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
    target: &windows::core::GUID,
) -> WindowsResult<IWICBitmapSource> {
    let converter = unsafe { factory.CreateFormatConverter()? };
    unsafe {
        converter.Initialize(
            source,
            target,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
    }
    converter.cast()
}

/// Applies EXIF orientation via the WIC flip/rotator.
fn apply_orientation(
    factory: &IWICImagingFactory,
    source: IWICBitmapSource,
    orientation: u32,
) -> WindowsResult<IWICBitmapSource> {
    let options = match orientation {
        2 => WICBitmapTransformFlipHorizontal,
        3 => WICBitmapTransformRotate180,
        4 => WICBitmapTransformFlipVertical,
        5 => WICBitmapTransformOptions(
            WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0,
        ),
        6 => WICBitmapTransformRotate90,
        7 => WICBitmapTransformOptions(
            WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0,
        ),
        8 => WICBitmapTransformRotate270,
        _ => return Ok(source),
    };
    let rotator = unsafe { factory.CreateBitmapFlipRotator()? };
    unsafe { rotator.Initialize(&source, options)? };
    rotator.cast()
}

fn exif_orientation(frame: &IWICBitmapFrameDecode) -> u32 {
    let Ok(reader) = (unsafe { frame.GetMetadataQueryReader() }) else {
        return 1;
    };
    query_u32(&reader, w!("System.Photo.Orientation")).unwrap_or(1)
}

fn read_exif(frame: &IWICBitmapFrameDecode) -> Option<ExifInfo> {
    let reader = unsafe { frame.GetMetadataQueryReader() }.ok()?;
    let information = ExifInfo {
        date_taken: query_filetime(&reader, w!("System.Photo.DateTaken")),
        rating: query_u32(&reader, w!("System.Rating")),
        camera_maker: query_string(&reader, w!("System.Photo.CameraManufacturer")),
        camera_model: query_string(&reader, w!("System.Photo.CameraModel")),
        f_stop: query_f64(&reader, w!("System.Photo.FNumber")),
        exposure_time_seconds: query_f64(&reader, w!("System.Photo.ExposureTime")),
        iso_speed: query_u32(&reader, w!("System.Photo.ISOSpeed")),
        exposure_bias: query_f64(&reader, w!("System.Photo.ExposureBias")),
        focal_length_millimeters: query_f64(&reader, w!("System.Photo.FocalLength")),
        max_aperture: query_f64(&reader, w!("System.Photo.MaxAperture")),
        metering_mode: query_u32(&reader, w!("System.Photo.MeteringMode")),
        flash: query_u32(&reader, w!("System.Photo.Flash")),
    };
    information.any_present().then_some(information)
}

fn query_f64(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<f64> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &raw mut value) }.ok()?;
    let result = unsafe { PropVariantToDouble(&raw const value) }.ok();
    let _ = unsafe { PropVariantClear(&raw mut value) };
    result.filter(|number| number.is_finite())
}

fn query_string(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<String> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &raw mut value) }.ok()?;
    let text = unsafe { PropVariantToStringAlloc(&raw const value) }
        .ok()
        .map(|out| {
            let result = String::from_utf16_lossy(unsafe { out.as_wide() });
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(out.0.cast())) };
            result
        });
    let _ = unsafe { PropVariantClear(&raw mut value) };
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn query_filetime(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<std::time::SystemTime> {
    use windows::Win32::System::Variant::PSTF_UTC;
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &raw mut value) }.ok()?;
    let file_time = unsafe { PropVariantToFileTime(&raw const value, PSTF_UTC) }.ok();
    let _ = unsafe { PropVariantClear(&raw mut value) };
    let file_time = file_time?;
    let intervals =
        (u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime);
    let unix_intervals = intervals.checked_sub(FILETIME_UNIX_EPOCH)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(unix_intervals * 100))
}

fn query_u32(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u32> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &raw mut value) }.ok()?;
    let result = unsafe { PropVariantToUInt32(&raw const value) }.ok();
    let _ = unsafe { PropVariantClear(&raw mut value) };
    result
}

fn source_size(source: &IWICBitmapSource) -> WindowsResult<(u32, u32)> {
    let (mut width, mut height) = (0u32, 0u32);
    unsafe { source.GetSize(&raw mut width, &raw mut height)? };
    Ok((width, height))
}

/// Code 0 means "no code", not a real HRESULT; the error overlay omits it.
pub fn uncoded_error(message: impl std::fmt::Display) -> DecodeError {
    DecodeError {
        code: 0,
        message: message.to_string(),
        store_extensions: &[],
    }
}

fn decode_apng<Input: BufRead + Seek>(
    input: Input,
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let mut decoder = png::Decoder::new(input);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(uncoded_error)?;

    let (canvas_width, canvas_height) = {
        let information = reader.info();
        (information.width, information.height)
    };
    let icc_profile = reader
        .info()
        .icc_profile
        .as_ref()
        .map(|profile| profile.to_vec());
    let animation_frame_count = reader
        .info()
        .animation_control
        .map_or(1, |control| control.num_frames);
    let default_image_is_first_frame = reader.info().frame_control.is_some();
    let has_animation = reader.info().animation_control.is_some();

    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| uncoded_error("APNG output buffer size overflow"))?;
    let mut buffer = vec![0u8; buffer_size];

    if has_animation && !default_image_is_first_frame {
        reader.next_frame(&mut buffer).map_err(uncoded_error)?;
    }

    let mut canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
    // The png crate accepts acTL num_frames up to i32::MAX; cap the reservation.
    let mut frames = Vec::with_capacity((animation_frame_count as usize).min(4096));
    let mut frames_truncated = false;
    for index in 0..animation_frame_count {
        if cancellation.load(Ordering::Relaxed) {
            return Err(DecodeError::cancelled());
        }
        // acTL's declared count is untrusted, so the budget runs frame by frame.
        if !frames.is_empty() && animation_budget_exceeded(frames.len() as u64 + 1, canvas.len()) {
            frames_truncated = true;
            break;
        }
        if !(index == 0 && (default_image_is_first_frame || !has_animation)) {
            reader.next_frame_info().map_err(uncoded_error)?;
        }
        let frame_control = reader.info().frame_control.unwrap_or(png::FrameControl {
            width: canvas_width,
            height: canvas_height,
            blend_op: png::BlendOp::Source,
            ..Default::default()
        });
        let output = reader.next_frame(&mut buffer).map_err(uncoded_error)?;
        let region_pixels = pixels_to_premultiplied_bgra(
            &buffer[..output.buffer_size()],
            output.color_type,
            frame_control.width,
            frame_control.height,
        )?;

        let restore_previous =
            (frame_control.dispose_op == png::DisposeOp::Previous).then(|| canvas.clone());
        if frame_control.blend_op == png::BlendOp::Source {
            copy_rectangle(
                &mut canvas,
                canvas_width,
                canvas_height,
                &region_pixels,
                frame_control.width,
                frame_control.height,
                frame_control.x_offset,
                frame_control.y_offset,
            );
        } else {
            blend_over(
                &mut canvas,
                canvas_width,
                canvas_height,
                &region_pixels,
                frame_control.width,
                frame_control.height,
                frame_control.x_offset,
                frame_control.y_offset,
            );
        }
        let delay_denominator = if frame_control.delay_den == 0 {
            100
        } else {
            u32::from(frame_control.delay_den)
        };
        frames.push(Frame {
            pixels: canvas.clone(),
            delay_milliseconds: (u32::from(frame_control.delay_num) * 1000 / delay_denominator)
                .max(10),
        });
        match (frame_control.dispose_op, restore_previous) {
            (png::DisposeOp::Background, _) => clear_rectangle(
                &mut canvas,
                canvas_width,
                frame_control.x_offset,
                frame_control.y_offset,
                frame_control.width,
                frame_control.height,
            ),
            (png::DisposeOp::Previous, Some(previous)) => canvas = previous,
            _ => {}
        }
    }
    if frames_truncated {
        frames.truncate(1);
    }
    Ok(DecodedImage {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        format_name,
        icc_profile,
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: 8,
        peak_luminance_nits: None,
        source_primaries: None,
        frames,
        frames_truncated,
    })
}

/// Straight RGBA to premultiplied BGRA; both slices hold the same pixel count.
pub fn premultiplied_bgra_from_rgba(source: &[u8], output: &mut [u8]) {
    for (source_pixel, output_pixel) in source.chunks_exact(4).zip(output.chunks_exact_mut(4)) {
        // Uniform four-lane multiply; the alpha lane's 255 factor leaves it unchanged.
        let alpha = u16::from(source_pixel[3]);
        let swizzled = [
            source_pixel[2],
            source_pixel[1],
            source_pixel[0],
            source_pixel[3],
        ];
        let multipliers = [alpha, alpha, alpha, 255];
        for (output_channel, (value, multiplier)) in output_pixel
            .iter_mut()
            .zip(swizzled.into_iter().zip(multipliers))
        {
            *output_channel = (u16::from(value) * multiplier / 255) as u8;
        }
    }
}

fn pixels_to_premultiplied_bgra(
    pixels: &[u8],
    color_type: png::ColorType,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, DecodeError> {
    let pixel_count = width as usize * height as usize;
    let mut output = vec![0u8; pixel_count * 4];
    match color_type {
        png::ColorType::Rgba => {
            premultiplied_bgra_from_rgba(&pixels[..pixel_count * 4], &mut output);
        }
        png::ColorType::Rgb => {
            for (source_pixel, output_pixel) in pixels[..pixel_count * 3]
                .chunks_exact(3)
                .zip(output.chunks_exact_mut(4))
            {
                output_pixel[0] = source_pixel[2];
                output_pixel[1] = source_pixel[1];
                output_pixel[2] = source_pixel[0];
                output_pixel[3] = 255;
            }
        }
        other => {
            return Err(uncoded_error(format!(
                "unsupported PNG color type after normalization: {other:?}"
            )));
        }
    }
    Ok(output)
}

#[expect(clippy::too_many_arguments)]
pub fn copy_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let visible_width = source_width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = source_height.min(canvas_height.saturating_sub(top)) as usize;
    for row in 0..visible_height {
        let source_start = row * source_width as usize * 4;
        let canvas_start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        canvas[canvas_start..canvas_start + visible_width * 4]
            .copy_from_slice(&source[source_start..source_start + visible_width * 4]);
    }
}

fn decode_svg(data: &[u8], format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let tree = parse_svg_tree(data)?;
    let (pixel_width, pixel_height, scale) =
        svg_raster_geometry(&tree).ok_or_else(|| uncoded_error("SVG has no intrinsic size"))?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pixel_width, pixel_height)
        .ok_or_else(|| uncoded_error("SVG raster target allocation failed"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut pixels = pixmap.take();
    for pixel in pixels.chunks_exact_mut(4) {
        let swapped = [pixel[2], pixel[1], pixel[0], pixel[3]];
        pixel.copy_from_slice(&swapped);
    }
    Ok(DecodedImage {
        width: pixel_width,
        height: pixel_height,
        pixel_width,
        pixel_height,
        format_name,
        icc_profile: None,
        exif: None,
        storage: PixelStorage::Bgra8,
        source_bits_per_channel: 8,
        peak_luminance_nits: None,
        source_primaries: None,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
        frames_truncated: false,
    })
}

fn parse_svg_tree(data: &[u8]) -> Result<resvg::usvg::Tree, DecodeError> {
    let options = resvg::usvg::Options {
        fontdb: font_database().clone(),
        ..Default::default()
    };
    resvg::usvg::Tree::from_data(data, &options).map_err(uncoded_error)
}

/// Raster size and scale at the largest monitor's long side; the probe weight
/// and the decode must agree on this. None when the tree has no intrinsic size.
fn svg_raster_geometry(tree: &resvg::usvg::Tree) -> Option<(u32, u32, f32)> {
    let size = tree.size();
    if !(size.width() > 0.0 && size.height() > 0.0) {
        return None;
    }
    let target = largest_monitor_long_side().min(MAXIMUM_TEXTURE_DIMENSION) as f32;
    let scale = target / size.width().max(size.height());
    let pixel_width = (size.width() * scale).round().max(1.0) as u32;
    let pixel_height = (size.height() * scale).round().max(1.0) as u32;
    Some((pixel_width, pixel_height, scale))
}

fn font_database() -> &'static std::sync::Arc<resvg::usvg::fontdb::Database> {
    static DATABASE: OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = resvg::usvg::fontdb::Database::new();
        database.load_system_fonts();
        std::sync::Arc::new(database)
    })
}

fn largest_monitor_long_side() -> u32 {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::core::BOOL;

    extern "system" fn monitor_callback(
        _monitor: HMONITOR,
        _device_context: HDC,
        bounds: *mut RECT,
        state: LPARAM,
    ) -> BOOL {
        let longest = unsafe { &mut *(state.0 as *mut i32) };
        let bounds = unsafe { &*bounds };
        *longest = (*longest)
            .max(bounds.right - bounds.left)
            .max(bounds.bottom - bounds.top);
        true.into()
    }

    let mut longest = 0i32;
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_callback),
            LPARAM(&raw mut longest as isize),
        )
    };
    if longest > 0 { longest as u32 } else { 1920 }
}

/// Copies in strips so a cancelled decode can stop between them.
fn copy_pixels(
    source: &IWICBitmapSource,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    cancellation: &AtomicBool,
) -> WindowsResult<Vec<u8>> {
    const STRIP_ROWS: u32 = 256;
    let stride = width * bytes_per_pixel;
    let mut pixels = vec![0u8; stride as usize * height as usize];
    let mut row = 0;
    while row < height {
        if cancellation.load(Ordering::Relaxed) {
            return Err(E_ABORT.into());
        }
        let rows = STRIP_ROWS.min(height - row);
        let rectangle = WICRect {
            X: 0,
            Y: row as i32,
            Width: width as i32,
            Height: rows as i32,
        };
        let start = (row * stride) as usize;
        let end = start + (rows * stride) as usize;
        unsafe { source.CopyPixels(&raw const rectangle, stride, &mut pixels[start..end])? };
        row += rows;
    }
    Ok(pixels)
}

#[cfg(test)]
mod svg_tests {
    use super::*;

    #[test]
    fn svg_pixels_come_out_premultiplied_bgra() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#FF8000" fill-opacity="1.0"/></svg>"##;
        let decoded = decode_svg(svg, "SVG").unwrap_or_else(|_| panic!("decode failed"));
        let pixel = &decoded.frames[0].pixels[..4];
        assert_eq!(pixel, [0x00, 0x80, 0xFF, 0xFF], "expected BGRA order");
    }
}

#[cfg(test)]
mod icc_description_tests {
    use super::*;

    fn profile(tag_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut icc = vec![0u8; 128];
        icc.extend_from_slice(&1u32.to_be_bytes());
        icc.extend_from_slice(b"desc");
        icc.extend_from_slice(&144u32.to_be_bytes());
        icc.extend_from_slice(&(8 + payload.len() as u32).to_be_bytes());
        icc.extend_from_slice(tag_type);
        icc.extend_from_slice(&[0u8; 4]);
        icc.extend_from_slice(payload);
        icc
    }

    #[test]
    fn reads_a_version2_text_description() {
        let name = b"Adobe RGB (1998)\0";
        let mut payload = (name.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(name);
        let icc = profile(b"desc", &payload);
        assert_eq!(
            icc_profile_description(&icc).as_deref(),
            Some("Adobe RGB (1998)")
        );
    }

    #[test]
    fn reads_a_version4_mluc_description() {
        let text: Vec<u8> = "Display P3"
            .encode_utf16()
            .flat_map(|unit| unit.to_be_bytes())
            .collect();
        let mut payload = 1u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&12u32.to_be_bytes());
        payload.extend_from_slice(b"enUS");
        payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
        payload.extend_from_slice(&28u32.to_be_bytes());
        payload.extend_from_slice(&text);
        let icc = profile(b"mluc", &payload);
        assert_eq!(icc_profile_description(&icc).as_deref(), Some("Display P3"));
    }

    #[test]
    fn garbage_profiles_yield_none() {
        assert_eq!(icc_profile_description(&[0u8; 16]), None);
        assert_eq!(icc_profile_description(b"not an icc profile"), None);
    }
}

#[cfg(test)]
mod icc_space_tests {
    use super::*;

    /// sRGB colorants as a profile stores them, in the D50 connection space.
    const SRGB_COLORANTS: [[f32; 3]; 3] = [
        [0.4360, 0.2225, 0.0139],
        [0.3851, 0.7169, 0.0971],
        [0.1431, 0.0606, 0.7141],
    ];
    /// Display P3 colorants, likewise D50 referred.
    const DISPLAY_P3_COLORANTS: [[f32; 3]; 3] = [
        [0.5151, 0.2412, -0.0011],
        [0.2920, 0.6922, 0.0419],
        [0.1571, 0.0666, 0.7841],
    ];

    fn fixed(value: f32) -> [u8; 4] {
        ((value * 65536.0).round() as i32).to_be_bytes()
    }

    fn profile(colorants: [[f32; 3]; 3], curve: &[u8]) -> Vec<u8> {
        let names: [&[u8; 4]; 6] = [b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"];
        let payloads: Vec<Vec<u8>> = colorants
            .iter()
            .map(|values| {
                let mut tag = b"XYZ \0\0\0\0".to_vec();
                values.iter().for_each(|value| tag.extend(fixed(*value)));
                tag
            })
            .chain(std::iter::repeat_n(curve.to_vec(), 3))
            .collect();
        let mut icc = vec![0u8; 128];
        icc.extend_from_slice(&(names.len() as u32).to_be_bytes());
        let mut offset = 132 + names.len() * 12;
        for (name, payload) in names.iter().zip(&payloads) {
            icc.extend_from_slice(*name);
            icc.extend_from_slice(&(offset as u32).to_be_bytes());
            icc.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            offset += payload.len();
        }
        payloads
            .iter()
            .for_each(|payload| icc.extend_from_slice(payload));
        icc
    }

    /// Parametric curve type 3, which is how a version 4 profile writes sRGB.
    fn parametric_srgb_curve() -> Vec<u8> {
        let mut tag = b"para\0\0\0\0".to_vec();
        tag.extend_from_slice(&3u16.to_be_bytes());
        tag.extend_from_slice(&[0u8; 2]);
        for value in [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045] {
            tag.extend(fixed(value));
        }
        tag
    }

    /// Sampled curve, which is how a version 2 profile writes the same response.
    fn sampled_curve(shape: impl Fn(f32) -> f32) -> Vec<u8> {
        const ENTRIES: usize = 1024;
        let mut tag = b"curv\0\0\0\0".to_vec();
        tag.extend_from_slice(&(ENTRIES as u32).to_be_bytes());
        for index in 0..ENTRIES {
            let value = shape(index as f32 / (ENTRIES - 1) as f32);
            tag.extend_from_slice(&((value * 65535.0).round() as u16).to_be_bytes());
        }
        tag
    }

    fn gamma_curve(gamma: f32) -> Vec<u8> {
        let mut tag = b"curv\0\0\0\0".to_vec();
        tag.extend_from_slice(&1u32.to_be_bytes());
        tag.extend_from_slice(&((gamma * 256.0).round() as u16).to_be_bytes());
        tag
    }

    #[test]
    fn both_ways_of_writing_srgb_read_as_srgb() {
        assert!(icc_is_srgb(&profile(
            SRGB_COLORANTS,
            &parametric_srgb_curve()
        )));
        assert!(icc_is_srgb(&profile(
            SRGB_COLORANTS,
            &sampled_curve(color::srgb_to_linear)
        )));
    }

    #[test]
    fn a_pure_gamma_is_not_the_srgb_curve() {
        // The two part company below the toe.
        assert!(!icc_is_srgb(&profile(SRGB_COLORANTS, &gamma_curve(2.2))));
        assert!(!icc_is_srgb(&profile(
            SRGB_COLORANTS,
            &sampled_curve(|value| value.powf(2.2))
        )));
    }

    #[test]
    fn a_wider_gamut_is_not_srgb_whatever_its_curve() {
        assert!(!icc_is_srgb(&profile(
            DISPLAY_P3_COLORANTS,
            &parametric_srgb_curve()
        )));
    }

    #[test]
    fn a_profile_riv_cannot_read_is_never_srgb() {
        assert!(!icc_is_srgb(&[0u8; 16]));
        assert!(!icc_is_srgb(b"not an icc profile"));
        // Matrix primaries but no tone curves.
        let mut without_curves = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        without_curves[128..132].copy_from_slice(&3u32.to_be_bytes());
        assert!(!icc_is_srgb(&without_curves));
    }

    #[test]
    fn one_space_written_two_ways_compares_equal() {
        let version2 = profile(SRGB_COLORANTS, &sampled_curve(color::srgb_to_linear));
        let version4 = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        assert!(icc_same_space(&version2, &version4));
        assert!(icc_same_space(&version4, &version2));
    }

    #[test]
    fn one_gamut_at_two_white_points_compares_unequal() {
        // Scaling a colorant keeps its chromaticity and moves the white the three add up to.
        let scaled = |column: [f32; 3], by: f32| column.map(|value| value * by);
        let shifted = [
            scaled(SRGB_COLORANTS[0], 0.94),
            scaled(SRGB_COLORANTS[1], 1.05),
            SRGB_COLORANTS[2],
        ];
        let one = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        let other = profile(shifted, &parametric_srgb_curve());
        let (Some(one_xy), Some(other_xy)) = (icc_primaries(&one), icc_primaries(&other)) else {
            panic!("both profiles carry matrix primaries");
        };
        assert!(
            one_xy
                .iter()
                .flatten()
                .zip(other_xy.iter().flatten())
                .all(|(one, other)| (one - other).abs() < 1e-4),
            "the chromaticities are the same, so they alone cannot separate the two"
        );
        assert!(!icc_same_space(&one, &other));
    }

    #[test]
    fn a_different_gamut_or_curve_compares_unequal() {
        let srgb = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        let wide = profile(DISPLAY_P3_COLORANTS, &parametric_srgb_curve());
        let gamma = profile(SRGB_COLORANTS, &gamma_curve(2.2));
        assert!(!icc_same_space(&srgb, &wide));
        assert!(!icc_same_space(&srgb, &gamma));
        assert!(icc_same_space(&wide, &wide));
    }
}

#[cfg(test)]
mod descriptor_probe_tests {
    use super::*;

    #[test]
    fn raw_two_stage_detection_is_extension_only() {
        assert!(is_raw_two_stage(Path::new("photo.dng")));
        assert!(is_raw_two_stage(Path::new("PHOTO.DNG")));
        assert!(!is_raw_two_stage(Path::new("photo.png")));
        assert!(!is_raw_two_stage(Path::new("photo")));
    }

    #[test]
    fn xml_probes_as_svg_only_with_an_svg_tag() {
        let svg_document = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(probe_magic(svg_document).map(|d| d.name), Some("SVG"));
        let plain_xml = b"<?xml version=\"1.0\"?>\n<note><to>reader</to></note>";
        assert!(probe_magic(plain_xml).is_none());
        let bare_svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(probe_magic(bare_svg).map(|d| d.name), Some("SVG"));
    }

    /// PNG signature + IHDR(13 bytes) + an acTL chunk header.
    fn animated_png_header() -> Vec<u8> {
        let mut header = b"\x89PNG\r\n\x1a\n".to_vec();
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&[0u8; 13 + 4]); // data + CRC
        header.extend_from_slice(&8u32.to_be_bytes());
        header.extend_from_slice(b"acTL");
        header
    }

    #[test]
    fn magic_wins_without_an_extension_hint() {
        let descriptor = descriptor_for_bytes(b"\x89PNG\r\n\x1a\nrest", None).expect("descriptor");
        assert_eq!(descriptor.name, "PNG");
    }

    #[test]
    fn extension_hint_selects_the_descriptor() {
        let descriptor =
            descriptor_for_bytes(b"\xFF\xD8\xFFdata", Some("JPG")).expect("descriptor");
        assert_eq!(descriptor.name, "JPEG");
    }

    #[test]
    fn content_refinement_promotes_apng() {
        let descriptor =
            descriptor_for_bytes(&animated_png_header(), Some("png")).expect("descriptor");
        assert_eq!(descriptor.name, "APNG");
    }

    #[test]
    fn unknown_bytes_yield_none() {
        assert!(descriptor_for_bytes(b"plain text", None).is_none());
        assert!(descriptor_for_bytes(&[], None).is_none());
    }

    #[test]
    fn reserved_bytes_tell_a_bitmap_from_text() {
        assert!(descriptor_for_bytes(b"BMW service manual, page 6", None).is_none());
        let mut bitmap = b"BM".to_vec();
        bitmap.extend_from_slice(&64u32.to_le_bytes());
        bitmap.extend_from_slice(&[0u8; 4]); // the two reserved fields
        bitmap.extend_from_slice(&54u32.to_le_bytes());
        let descriptor = descriptor_for_bytes(&bitmap, None).expect("descriptor");
        assert_eq!(descriptor.name, "BMP");
    }

    /// Box size, "ftyp", the major brand, the minor version, the compatible brands.
    fn ftyp_header(major_brand: &[u8; 4], compatible_brands: &[&[u8; 4]]) -> Vec<u8> {
        let box_size = 16 + 4 * compatible_brands.len();
        let mut header = (box_size as u32).to_be_bytes().to_vec();
        header.extend_from_slice(b"ftyp");
        header.extend_from_slice(major_brand);
        header.extend_from_slice(&[0u8; 4]);
        for brand in compatible_brands {
            header.extend_from_slice(*brand);
        }
        header
    }

    #[test]
    fn content_refinement_promotes_avif() {
        let header = ftyp_header(b"mif1", &[b"mif1", b"avif"]);
        let descriptor = descriptor_for_bytes(&header, None).expect("descriptor");
        assert_eq!(descriptor.name, "AVIF");
    }

    #[test]
    fn a_heif_without_an_avif_brand_stays_heif() {
        let header = ftyp_header(b"heic", &[b"heic"]);
        let descriptor = descriptor_for_bytes(&header, None).expect("descriptor");
        assert_eq!(descriptor.name, "HEIF");
    }
}

#[cfg(test)]
mod hdr_linearization_tests {
    use super::*;

    type TransferFunction = fn(f32) -> f32;

    #[test]
    fn transfer_lookup_tables_match_direct_evaluation() {
        let cases: [(HdrTransfer, TransferFunction); 2] = [
            (HdrTransfer::PerceptualQuantizer, perceptual_quantizer_nits),
            (HdrTransfer::HybridLogGamma, hybrid_log_gamma_scene_linear),
        ];
        for (transfer, function) in cases {
            let table = hdr_transfer_lookup_table(transfer);
            for code in [0u16, 1, 255, 12345, 32767, 32768, 65534, 65535] {
                let direct = function(f32::from(code) / f32::from(u16::MAX));
                assert_eq!(table[usize::from(code)], direct, "code={code}");
            }
        }
    }

    /// Deterministic 16-bit RGBA codes covering the full range.
    fn coded_pixels(count: usize, mut state: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(count * 8);
        for _ in 0..count {
            for _ in 0..4 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                pixels.extend_from_slice(&((state >> 8) as u16).to_le_bytes());
            }
        }
        pixels
    }

    fn linearize_hdr_pixels_reference(pixels: &mut [u8], encoding: HdrEncoding) {
        let transfer_table = hdr_transfer_lookup_table(encoding.transfer);
        for pixel in pixels.chunks_exact_mut(8) {
            let mut channel_nits = [0.0f32; 3];
            for (channel, nits) in channel_nits.iter_mut().enumerate() {
                let code = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
                *nits = transfer_table[usize::from(code)];
            }
            if matches!(encoding.transfer, HdrTransfer::HybridLogGamma) {
                let scene_luminance =
                    0.2627 * channel_nits[0] + 0.6780 * channel_nits[1] + 0.0593 * channel_nits[2];
                let display_scale = HLG_PEAK_NITS * scene_luminance.max(0.0).powf(0.2);
                for nits in &mut channel_nits {
                    *nits *= display_scale;
                }
            }
            let alpha = f32::from(u16::from_le_bytes([pixel[6], pixel[7]])) / f32::from(u16::MAX);
            for (channel, nits) in channel_nits.iter().enumerate() {
                let premultiplied = nits / SDR_REFERENCE_WHITE_NITS * alpha;
                pixel[channel * 2..channel * 2 + 2]
                    .copy_from_slice(&f32_to_half(premultiplied).to_le_bytes());
            }
            pixel[6..8].copy_from_slice(&f32_to_half(alpha).to_le_bytes());
        }
    }

    #[test]
    fn linearization_matches_the_scalar_reference() {
        for transfer in [
            HdrTransfer::PerceptualQuantizer,
            HdrTransfer::HybridLogGamma,
        ] {
            let encoding = HdrEncoding {
                transfer,
                primaries: HdrPrimaries::Bt2020,
            };
            let mut pixels = coded_pixels(4096, 21);
            let mut expected = pixels.clone();
            linearize_hdr_pixels_reference(&mut expected, encoding);
            linearize_hdr_pixels(&mut pixels, encoding);
            assert_eq!(pixels, expected);
        }
    }

    #[test]
    fn parallel_linearization_matches_the_sequential_reference() {
        // Above the block threshold the buffer splits across worker threads.
        let encoding = HdrEncoding {
            transfer: HdrTransfer::PerceptualQuantizer,
            primaries: HdrPrimaries::Bt2020,
        };
        let mut pixels = coded_pixels(600_000, 5);
        let mut expected = pixels.clone();
        linearize_hdr_pixels_reference(&mut expected, encoding);
        linearize_hdr_pixels(&mut pixels, encoding);
        assert_eq!(pixels, expected);
    }

    #[test]
    fn linearization_maximum_matches_a_rescan() {
        // Below and above the parallel block threshold, for both transfers.
        for (count, transfer) in [
            (4096usize, HdrTransfer::PerceptualQuantizer),
            (4096, HdrTransfer::HybridLogGamma),
            (600_000, HdrTransfer::PerceptualQuantizer),
        ] {
            let encoding = HdrEncoding {
                transfer,
                primaries: HdrPrimaries::Bt2020,
            };
            let mut pixels = coded_pixels(count, 31);
            let maximum_bits = linearize_hdr_pixels(&mut pixels, encoding);
            assert_eq!(
                peak_luminance_with_maximum_bits(&pixels, maximum_bits),
                peak_luminance_from_half_pixels(&pixels),
                "count={count}"
            );
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn linearized_peak_timing() {
        let encoding = HdrEncoding {
            transfer: HdrTransfer::PerceptualQuantizer,
            primaries: HdrPrimaries::Bt2020,
        };
        let pixels = coded_pixels(16_000_000, 77);
        for _ in 0..3 {
            let mut scratch = pixels.clone();
            let start = std::time::Instant::now();
            let maximum_bits = linearize_hdr_pixels(&mut scratch, encoding);
            let fused = peak_luminance_with_maximum_bits(&scratch, maximum_bits);
            let fused_elapsed = start.elapsed();
            let start = std::time::Instant::now();
            let rescanned = peak_luminance_from_half_pixels(&scratch);
            let rescan_elapsed = start.elapsed();
            assert_eq!(fused, rescanned);
            println!(
                "fused={fused_elapsed:?} versus linearize+rescan={:?}",
                fused_elapsed + rescan_elapsed
            );
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn half_encode_timing() {
        let mut state = 1u32;
        let values: Vec<f32> = (0..64_000_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state as f32 / u32::MAX as f32) * 30.0 - 2.0
            })
            .collect();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let mut accumulator = 0u16;
            for value in &values {
                accumulator ^= f32_to_half(*value);
            }
            println!(
                "half encode 64M elapsed={:?} accumulator={accumulator}",
                start.elapsed()
            );
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn linearization_timing() {
        for (label, transfer) in [
            ("pq", HdrTransfer::PerceptualQuantizer),
            ("hlg", HdrTransfer::HybridLogGamma),
        ] {
            let encoding = HdrEncoding {
                transfer,
                primaries: HdrPrimaries::Bt2020,
            };
            let pixels = coded_pixels(16_000_000, 77);
            for _ in 0..3 {
                let mut scratch = pixels.clone();
                let start = std::time::Instant::now();
                linearize_hdr_pixels(&mut scratch, encoding);
                println!("{label} 16M pixels elapsed={:?}", start.elapsed());
            }
        }
    }
}

#[cfg(test)]
mod peak_scan_tests {
    use super::*;

    fn half_pixels(values: &[(f32, f32, f32)]) -> Vec<u8> {
        let mut pixels = Vec::new();
        for (red, green, blue) in values {
            for value in [*red, *green, *blue, 1.0] {
                pixels.extend_from_slice(&f32_to_half(value).to_le_bytes());
            }
        }
        pixels
    }

    #[test]
    fn sdr_content_short_circuits_to_maximum() {
        let pixels = half_pixels(&[(0.25, 0.5, 0.125); 64]);
        let peak = peak_luminance_from_half_pixels(&pixels).unwrap();
        assert!(
            (peak - 0.5 * SDR_REFERENCE_WHITE_NITS).abs() < 0.1,
            "peak={peak}"
        );
    }

    #[test]
    fn hdr_percentile_rejects_outliers_full_scan() {
        // Below the subsample floor the scan is exhaustive: aligned outliers still get rejected.
        let mut values = vec![(2.5f32, 2.5f32, 2.5f32); 4000];
        values[100] = (60.0, 60.0, 60.0);
        values[2000] = (60.0, 60.0, 60.0);
        let pixels = half_pixels(&values);
        let peak = peak_luminance_from_half_pixels(&pixels).unwrap();
        assert!((peak - 200.0).abs() < 5.0, "peak={peak}");
    }

    #[test]
    fn hdr_percentile_rejects_aligned_outliers_when_subsampled() {
        // Bright pixels on a fixed 8000 period: jittered subsampling must not alias past 0.1%.
        let mut values = vec![(2.5f32, 2.5f32, 2.5f32); 4_000_000];
        for index in (0..values.len()).step_by(8000) {
            values[index] = (60.0, 60.0, 60.0);
        }
        let pixels = half_pixels(&values);
        let peak = peak_luminance_from_half_pixels(&pixels).unwrap();
        assert!((peak - 200.0).abs() < 5.0, "peak={peak}");
    }

    #[test]
    fn empty_input_yields_none() {
        assert!(peak_luminance_from_half_pixels(&[]).is_none());
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn maximum_scan_timing() {
        // All-SDR content returns right after the maximum pass, timing it alone.
        let pixels = half_pixels(&vec![(0.25f32, 0.5, 0.75); 16_000_000]);
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let peak = peak_luminance_from_half_pixels(&pixels).unwrap();
            println!("maximum scan peak={peak} elapsed={:?}", start.elapsed());
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn peak_scan_timing() {
        let mut values = vec![(2.5f32, 0.5, 1.5); 16_000_000];
        for index in (0..values.len()).step_by(97) {
            values[index] = (30.0, 10.0, 5.0);
        }
        let pixels = half_pixels(&values);
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let peak = peak_luminance_from_half_pixels(&pixels).unwrap();
            println!("peak={peak} elapsed={:?}", start.elapsed());
        }
    }
}

#[cfg(test)]
mod compositing_tests {
    use super::*;

    /// Deterministic premultiplied BGRA with frequent fully transparent and opaque pixels.
    fn premultiplied_pixels(count: usize, mut state: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(count * 4);
        for _ in 0..count {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let alpha = match state >> 30 {
                0 => 0,
                1 => 255,
                _ => (state >> 8) as u8,
            };
            for shift in [0u32, 8, 16] {
                let straight = (state >> shift) as u8;
                pixels.push((u16::from(straight) * u16::from(alpha) / 255) as u8);
            }
            pixels.push(alpha);
        }
        pixels
    }

    fn blend_over_reference(canvas: &mut [u8], source: &[u8]) {
        for (canvas_pixel, source_pixel) in canvas.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
            let alpha = u32::from(source_pixel[3]);
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                canvas_pixel.copy_from_slice(source_pixel);
                continue;
            }
            for channel in 0..4 {
                let blended = u32::from(source_pixel[channel])
                    + (u32::from(canvas_pixel[channel]) * (255 - alpha) + 127) / 255;
                canvas_pixel[channel] = blended.min(255) as u8;
            }
        }
    }

    #[test]
    fn blend_over_matches_the_scalar_reference() {
        let source = premultiplied_pixels(64 * 64, 7);
        let mut canvas = premultiplied_pixels(64 * 64, 1234);
        let mut expected = canvas.clone();
        blend_over_reference(&mut expected, &source);
        blend_over(&mut canvas, 64, 64, &source, 64, 64, 0, 0);
        assert_eq!(canvas, expected);
    }

    #[test]
    fn blend_over_clips_an_offset_frame_to_the_canvas() {
        let source = premultiplied_pixels(8 * 8, 42);
        let mut canvas = premultiplied_pixels(16 * 16, 9);
        let mut expected = canvas.clone();
        // Rows 0..4 of the visible 4x4 window, blended one pixel at a time.
        for row in 0..4usize {
            for column in 0..4usize {
                let canvas_start = ((12 + row) * 16 + 12 + column) * 4;
                let source_start = (row * 8 + column) * 4;
                blend_over_reference(
                    &mut expected[canvas_start..canvas_start + 4],
                    &source[source_start..source_start + 4],
                );
            }
        }
        blend_over(&mut canvas, 16, 16, &source, 8, 8, 12, 12);
        assert_eq!(canvas, expected);
    }

    #[test]
    fn clear_rectangle_ignores_a_zero_width_canvas() {
        let mut canvas = Vec::new();
        clear_rectangle(&mut canvas, 0, 0, 0, 4, 4); // must not divide by zero
        assert!(canvas.is_empty());
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn blend_over_timing() {
        const WIDTH: u32 = 1920;
        const HEIGHT: u32 = 1080;
        let source = premultiplied_pixels((WIDTH * HEIGHT) as usize, 99);
        let mut canvas = premultiplied_pixels((WIDTH * HEIGHT) as usize, 5);
        for _ in 0..3 {
            let start = std::time::Instant::now();
            for _ in 0..50 {
                blend_over(&mut canvas, WIDTH, HEIGHT, &source, WIDTH, HEIGHT, 0, 0);
            }
            println!("blend_over 50 frames elapsed={:?}", start.elapsed());
        }
    }
}

#[cfg(test)]
mod premultiplied_conversion_tests {
    use super::*;

    /// Deterministic straight-alpha RGBA with frequent fully transparent and opaque pixels.
    fn rgba_pixels(count: usize, mut state: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(count * 4);
        for _ in 0..count {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            for shift in [0u32, 8, 16] {
                pixels.push((state >> shift) as u8);
            }
            pixels.push(match state >> 30 {
                0 => 0,
                1 => 255,
                _ => (state >> 8) as u8,
            });
        }
        pixels
    }

    #[test]
    fn rgba_conversion_matches_the_scalar_reference() {
        let rgba = rgba_pixels(64 * 64, 3);
        let Ok(converted) = pixels_to_premultiplied_bgra(&rgba, png::ColorType::Rgba, 64, 64)
        else {
            panic!("conversion failed");
        };
        let mut expected = Vec::new();
        for pixel in rgba.chunks_exact(4) {
            let alpha = u16::from(pixel[3]);
            expected.push((u16::from(pixel[2]) * alpha / 255) as u8);
            expected.push((u16::from(pixel[1]) * alpha / 255) as u8);
            expected.push((u16::from(pixel[0]) * alpha / 255) as u8);
            expected.push(pixel[3]);
        }
        assert_eq!(converted, expected);
    }

    #[test]
    fn rgb_conversion_matches_the_scalar_reference() {
        let rgb: Vec<u8> = rgba_pixels(64 * 64, 11)
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        let Ok(converted) = pixels_to_premultiplied_bgra(&rgb, png::ColorType::Rgb, 64, 64) else {
            panic!("conversion failed");
        };
        let mut expected = Vec::new();
        for pixel in rgb.chunks_exact(3) {
            expected.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        assert_eq!(converted, expected);
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn rgba_conversion_timing() {
        let rgba = rgba_pixels(1920 * 1080, 17);
        for _ in 0..3 {
            let start = std::time::Instant::now();
            for _ in 0..50 {
                let Ok(converted) =
                    pixels_to_premultiplied_bgra(&rgba, png::ColorType::Rgba, 1920, 1080)
                else {
                    panic!("conversion failed");
                };
                std::hint::black_box(&converted);
            }
            println!("rgba conversion 50 frames elapsed={:?}", start.elapsed());
        }
    }
}

/// A huge declared acTL num_frames must not drive the reservation (fixture: SECURITY_AUDIT.md).
#[cfg(test)]
mod apng_tests {
    use super::*;

    #[test]
    #[ignore = "needs test/apng_huge_frames.png"]
    fn a_huge_declared_frame_count_does_not_over_reserve() {
        let data = std::fs::read("test/apng_huge_frames.png").expect("fixture");
        let cancellation = AtomicBool::new(false);
        // Frames run out after the first; decode errors without the huge reservation.
        assert!(decode_bytes(&data, Some("png"), &cancellation).is_err());
    }
}

#[cfg(test)]
mod decoded_weight_tests {
    use super::*;

    #[test]
    fn a_single_frame_weighs_its_pixel_bytes() {
        assert_eq!(decoded_weight(6000, 4000, 4, 1), 96_000_000);
        assert_eq!(decoded_weight(6000, 4000, 8, 1), 192_000_000);
    }

    #[test]
    fn an_oversized_single_downscales_before_weighing() {
        // 17000x6000 exceeds the texture limit; the decode lands at 16384x5782.
        let expected = 16384u64 * (6000 * 16384 / 17000) * 8;
        assert_eq!(decoded_weight(17000, 6000, 8, 1), expected);
    }

    #[test]
    fn an_animation_within_the_cap_counts_every_frame() {
        let frame_bytes = 748u64 * 418 * 4;
        assert_eq!(decoded_weight(748, 418, 4, 400), frame_bytes * 400);
    }

    #[test]
    fn an_animation_past_the_cap_weighs_its_first_frame() {
        let frame_bytes = 1920u64 * 1080 * 4;
        assert_eq!(decoded_weight(1920, 1080, 4, 2000), frame_bytes);
    }

    #[test]
    fn a_claimed_first_frame_past_the_cap_keeps_its_claimed_size() {
        // A forged canvas is never capped: the weight blocks speculation instead.
        let frame_bytes = 60000u64 * 60000 * 4;
        assert_eq!(decoded_weight(60000, 60000, 4, 10), frame_bytes);
    }
}
