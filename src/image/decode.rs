//! Decoder registry, format dispatch, and the WIC adapter (decode workers only).

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{
    E_ABORT, E_OUTOFMEMORY, GENERIC_READ, WINCODEC_ERR_COMPONENTINITIALIZEFAILURE,
    WINCODEC_ERR_COMPONENTNOTFOUND,
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
    IWICBitmapSourceTransform, IWICColorContext, IWICImagingFactory, IWICMetadataQueryReader,
    IWICPixelFormatInfo2, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICBitmapTransformFlipHorizontal, WICBitmapTransformFlipVertical,
    WICBitmapTransformOptions, WICBitmapTransformRotate0, WICBitmapTransformRotate90,
    WICBitmapTransformRotate180, WICBitmapTransformRotate270, WICColorContextProfile,
    WICDecodeMetadataCacheOnDemand, WICPixelFormatNumericRepresentationFloat, WICRect,
};
use windows::Win32::Media::MediaFoundation::MF_E_TOPO_CODEC_NOT_FOUND;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToDouble, PropVariantToFileTime,
    PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{HSTRING, Interface, PCWSTR, Result as WindowsResult, w};

use crate::image::color::{
    self, SDR_REFERENCE_WHITE_NITS, perceptual_quantizer_code, perceptual_quantizer_nits,
};
use crate::image::icc;

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
    pub icc_profile: Option<Arc<[u8]>>,
    pub exif: Option<ExifMetadata>,
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
    /// Ultra HDR gain map parameters, when the file carries one.
    pub gain_map: Option<crate::image::gain_map::GainMapMetadata>,
    /// The decoded gain map pixels; released with the frames once a texture holds them.
    pub gain_map_plane: Option<crate::image::gain_map::GainMapPlane>,
}

#[derive(Clone)]
pub struct ExifMetadata {
    pub date_taken: Option<std::time::SystemTime>,
    pub rating: Option<u32>,
    pub camera_maker: Option<String>,
    pub camera_model: Option<String>,
    pub f_stop: Option<f64>,
    pub exposure_time_seconds: Option<f64>,
    pub iso_speed: Option<u32>,
    pub exposure_bias: Option<f64>,
    pub focal_length_millimeters: Option<f64>,
    pub maximum_aperture: Option<f64>,
    pub metering_mode: Option<u32>,
    pub flash: Option<u32>,
}

impl ExifMetadata {
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
            || self.maximum_aperture.is_some()
            || self.metering_mode.is_some()
            || self.flash.is_some()
    }
}

impl DecodedImage {
    pub fn pixel_bytes(&self) -> usize {
        let frames: usize = self.frames.iter().map(|frame| frame.pixels.len()).sum();
        let plane = self
            .gain_map_plane
            .as_ref()
            .map_or(0, |plane| plane.pixels.len());
        frames + plane
    }

    /// Bytes per row of a frame; the D3D pitch and the buffer stride alike.
    pub fn row_pitch(&self) -> u32 {
        self.pixel_width * self.storage.bytes_per_pixel()
    }

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
            gain_map: self.gain_map,
            gain_map_plane: None,
        }
    }
}

#[derive(Clone)]
pub struct DecodeError {
    pub code: i32,
    pub message: String,
    pub store_codec_names: &'static [&'static str],
}

impl DecodeError {
    pub fn cancelled() -> Self {
        Self {
            code: E_ABORT.0,
            message: "cancelled".to_string(),
            store_codec_names: &[],
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.code == E_ABORT.0
    }

    /// True when no decoder recognized the data and no Store codec is named.
    pub fn is_unrecognized_format(&self) -> bool {
        self.code == WINCODEC_ERR_COMPONENTNOTFOUND.0 && self.store_codec_names.is_empty()
    }
}

impl From<windows::core::Error> for DecodeError {
    fn from(error: windows::core::Error) -> Self {
        Self {
            code: error.code().0,
            message: error.message(),
            store_codec_names: &[],
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
const MAXIMUM_ANIMATION_FRAMES_BYTES: u64 = 1 << 30;

/// Whether `frame_count` canvas-sized frames would expand past the byte limit.
pub(crate) fn animation_budget_exceeded(frame_count: u64, canvas_bytes: u64) -> bool {
    frame_count * canvas_bytes > MAXIMUM_ANIMATION_FRAMES_BYTES
}

/// A zeroed buffer of `bytes`, or None when memory runs short; vec! would abort on OOM.
pub(crate) fn try_zeroed_buffer(bytes: usize) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(bytes).ok()?;
    buffer.resize(bytes, 0);
    Some(buffer)
}

#[derive(Clone, Copy, PartialEq)]
pub enum FrameBlend {
    Over,
    Replace,
}

/// What the canvas keeps once the frame has been shown.
#[derive(Clone, Copy, PartialEq)]
pub enum FrameDisposal {
    Keep,
    Background,
    Previous,
}

/// One frame as its container places it on the canvas.
pub struct FrameRegion<'pixels> {
    pub pixels: &'pixels [u8],
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub blend: FrameBlend,
    pub disposal: FrameDisposal,
    pub delay_milliseconds: u32,
}

/// The canvas an animation composes onto; GIF, APNG, and WebP share these rules.
pub struct FrameCompositor {
    canvas: Vec<u8>,
    width: u32,
    height: u32,
    frames: Vec<Frame>,
    truncated: bool,
}

impl FrameCompositor {
    /// None when the canvas alone exceeds the frame budget or cannot be reserved.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let bytes = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)
            .filter(|bytes| !animation_budget_exceeded(1, *bytes as u64))?;
        let canvas = try_zeroed_buffer(bytes)?;
        Some(Self {
            canvas,
            width,
            height,
            frames: Vec::new(),
            truncated: false,
        })
    }

    pub fn frames_so_far(&self) -> usize {
        self.frames.len()
    }

    pub fn accepts_one_more(&mut self) -> bool {
        self.accepts_another(self.frames.len() as u64 + 1)
    }

    /// Whether `declared_frames` still fit the budget; a refusal marks the animation truncated.
    pub fn accepts_another(&mut self, declared_frames: u64) -> bool {
        if !self.frames.is_empty()
            && animation_budget_exceeded(declared_frames, self.canvas.len() as u64)
        {
            self.truncated = true;
            return false;
        }
        true
    }

    /// Composes the region, keeps the result as a frame, then applies the disposal.
    pub fn add_frame(&mut self, region: FrameRegion) {
        let restored = (region.disposal == FrameDisposal::Previous).then(|| self.canvas.clone());
        let compose = match region.blend {
            FrameBlend::Over => blend_over,
            FrameBlend::Replace => copy_rectangle,
        };
        compose(
            &mut self.canvas,
            self.width,
            self.height,
            region.pixels,
            region.width,
            region.height,
            region.left,
            region.top,
        );
        // Restoring swaps the snapshot back in, so the composed canvas moves into the frame.
        let pixels = match restored {
            Some(previous) => std::mem::replace(&mut self.canvas, previous),
            None => self.canvas.clone(),
        };
        self.frames.push(Frame {
            pixels,
            delay_milliseconds: region.delay_milliseconds,
        });
        if region.disposal == FrameDisposal::Background {
            clear_rectangle(
                &mut self.canvas,
                self.width,
                self.height,
                region.left,
                region.top,
                region.width,
                region.height,
            );
        }
    }

    /// Over the budget the animation collapses to its first frame, the same for every format.
    pub fn finish(self) -> (Vec<Frame>, bool) {
        let mut frames = self.frames;
        if self.truncated {
            frames.truncate(1);
        }
        (frames, self.truncated)
    }
}

/// 100 ns intervals from 1601-01-01 (FILETIME zero) to the UNIX epoch.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

type MagicSignature = &'static [(usize, &'static [u8])];

enum Adapter {
    Wic,
    WicRawTwoStage,
    WicSubresolutionTwoStage,
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
    store_codec_names: &'static [&'static str],
    /// The container can carry an Ultra HDR gain map worth probing for.
    carries_gain_map: bool,
}

/// Extensions, file filters, and association groups all derive from this registry.
static REGISTRY: &[FormatDescriptor] = &[
    FormatDescriptor {
        name: "PNG",
        extensions: &["png"],
        magic: &[&[(0, b"\x89PNG\r\n\x1a\n")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "APNG",
        extensions: &["apng"],
        magic: &[],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Apng,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "SVG",
        extensions: &["svg", "svgz"],
        magic: &[&[(0, b"<svg")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Svg,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "JPEG",
        extensions: &["jpe", "jpeg", "jpg"],
        magic: &[&[(0, b"\xFF\xD8\xFF")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: true,
    },
    FormatDescriptor {
        name: "GIF",
        extensions: &["gif"],
        magic: &[&[(0, b"GIF8")]],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "WebP",
        extensions: &["webp"],
        magic: &[&[(0, b"RIFF"), (8, b"WEBP")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &["WebP Image Extensions"],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "BMP",
        extensions: &["bmp", "dib"],
        // The two reserved fields at offset 6 must be zero.
        magic: &[&[(0, b"BM"), (6, &[0, 0, 0, 0])]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "ICO",
        extensions: &["ico"],
        magic: &[&[(0, &[0x00, 0x00, 0x01, 0x00])]],
        semantics: FrameSemantics::SizeVariants,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "TIFF",
        extensions: &["tiff", "tif"],
        magic: &[&[(0, b"II*\x00")], &[(0, b"MM\x00*")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "DDS",
        extensions: &["dds"],
        magic: &[&[(0, b"DDS ")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &[],
        carries_gain_map: false,
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
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "EXR",
        extensions: &["exr"],
        magic: &[&[(0, b"\x76\x2F\x31\x01")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Exr,
        store_codec_names: &[],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "AVIF",
        extensions: &["avif"],
        magic: &[&[(4, b"ftypavif")], &[(4, b"ftypavis")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_codec_names: &["HEIF Image Extension", "AV1 Video Extension"],
        carries_gain_map: false,
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
        store_codec_names: &["JPEG XL Image Extension"],
        carries_gain_map: false,
    },
    FormatDescriptor {
        name: "JPEG XR",
        extensions: &["jxr", "wdp"],
        magic: &[&[(0, b"II\xBC\x01")], &[(0, b"II\xBC\x00")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::WicSubresolutionTwoStage,
        store_codec_names: &[],
        carries_gain_map: false,
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
        store_codec_names: &["Raw Image Extension"],
        carries_gain_map: false,
    },
];

pub fn format_groups() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    REGISTRY
        .iter()
        .map(|descriptor| (descriptor.name, descriptor.extensions))
}

pub fn format_name_for_extension(extension: &str) -> Option<&'static str> {
    descriptor_for_extension(extension).map(|descriptor| descriptor.name)
}

fn descriptor_for_extension(extension: &str) -> Option<&'static FormatDescriptor> {
    REGISTRY
        .iter()
        .find(|descriptor| descriptor.extensions.contains(&extension))
}

pub fn descriptor_for_content(path: &Path) -> Option<&'static FormatDescriptor> {
    let header = read_header(path)?;
    descriptor_for_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
}

fn descriptor_for_magic(header: &[u8]) -> Option<&'static FormatDescriptor> {
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
    store_codec_names: &[],
    carries_gain_map: false,
};

/// The names refine_by_content can reclassify; keep the two in step.
fn refines_by_content(descriptor: &FormatDescriptor) -> bool {
    matches!(descriptor.name, "PNG" | "WebP" | "HEIF")
}

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

/// Formats rastered at the largest monitor's size, so cached weights expire with it.
pub fn weight_depends_on_display(extension: &str) -> bool {
    descriptor_for_extension(extension)
        .is_some_and(|descriptor| matches!(descriptor.adapter, Adapter::Svg))
}

fn descriptor_for_path(path: &Path) -> Option<&'static FormatDescriptor> {
    let by_extension = crate::text::lowercase_extension(path)
        .and_then(|extension| descriptor_for_extension(&extension));
    if let Some(descriptor) = by_extension {
        // The header is read only for the names it can reclassify.
        if !refines_by_content(descriptor) {
            return Some(descriptor);
        }
        return Some(match read_header(path) {
            Some(header) => refine_by_content(descriptor, &header),
            None => descriptor,
        });
    }
    let header = read_header(path)?;
    descriptor_for_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
}

fn descriptor_for_bytes(
    bytes: &[u8],
    extension: Option<&str>,
) -> Option<&'static FormatDescriptor> {
    let header = &bytes[..bytes.len().min(4096)];
    match extension
        .map(str::to_lowercase)
        .and_then(|extension| descriptor_for_extension(&extension))
    {
        Some(descriptor) => Some(refine_by_content(descriptor, header)),
        None => {
            descriptor_for_magic(header).map(|descriptor| refine_by_content(descriptor, header))
        }
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
    bytes: &[u8],
    extension: Option<&str>,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    decode_input(&DecodeInput::Memory { bytes, extension }, cancellation)
}

enum DecodeInput<'a> {
    File(&'a Path),
    Memory {
        bytes: &'a [u8],
        extension: Option<&'a str>,
    },
}

impl DecodeInput<'_> {
    fn descriptor(&self) -> Option<&'static FormatDescriptor> {
        match self {
            DecodeInput::File(path) => descriptor_for_path(path),
            DecodeInput::Memory { bytes, extension } => descriptor_for_bytes(bytes, *extension),
        }
    }

    /// Whole input bytes for the adapters that decode from memory.
    fn read_all(&self) -> Result<std::borrow::Cow<'_, [u8]>, DecodeError> {
        match self {
            DecodeInput::File(path) => std::fs::read(path)
                .map(std::borrow::Cow::Owned)
                .map_err(uncoded_error),
            DecodeInput::Memory { bytes, .. } => Ok(std::borrow::Cow::Borrowed(*bytes)),
        }
    }
}

/// Failures the Store hint can remedy: the codec is absent, or registered but broken.
fn is_missing_codec_error(code: i32) -> bool {
    code == WINCODEC_ERR_COMPONENTNOTFOUND.0
        || code == WINCODEC_ERR_COMPONENTINITIALIZEFAILURE.0
        || code == MF_E_TOPO_CODEC_NOT_FOUND.0
}

/// Attaches the gain map, or leaves both fields absent so damage reads as a plain file.
fn attach_gain_map(
    mut decoded: DecodedImage,
    input: &DecodeInput<'_>,
    cancellation: &AtomicBool,
) -> DecodedImage {
    if let Some((metadata, plane)) =
        find_and_decode_gain_map(input, cancellation, decoded.width, decoded.height)
    {
        decoded.gain_map = Some(metadata);
        decoded.gain_map_plane = Some(plane);
    }
    decoded
}

/// The parsed parameters and decoded plane, when the file carries a usable gain map.
fn find_and_decode_gain_map(
    input: &DecodeInput<'_>,
    cancellation: &AtomicBool,
    base_width: u32,
    base_height: u32,
) -> Option<(
    crate::image::gain_map::GainMapMetadata,
    crate::image::gain_map::GainMapPlane,
)> {
    // The header probe skips the whole-file read below when the JPEG carries no MPF.
    if let DecodeInput::File(path) = input {
        let file = File::open(path).ok()?;
        if !crate::image::gain_map::jpeg_carries_mpf(BufReader::new(file)) {
            return None;
        }
    }
    let bytes = input.read_all().ok()?;
    let found = crate::image::gain_map::find_ultra_hdr(&bytes)?;
    let plane = decode_gain_map_plane(bytes.get(found.gain_map_range.clone())?, cancellation)?;
    plane
        .fits_within(base_width, base_height)
        .then_some((found.metadata, plane))
}

/// Decodes the gain map JPEG to BGRA, the storage every base frame already uses.
fn decode_gain_map_plane(
    gain_map_bytes: &[u8],
    cancellation: &AtomicBool,
) -> Option<crate::image::gain_map::GainMapPlane> {
    with_wic_factory(|factory| {
        let decoder = create_wic_decoder(
            factory,
            &DecodeInput::Memory {
                bytes: gain_map_bytes,
                extension: None,
            },
        )?;
        let frame = unsafe { decoder.GetFrame(0)? };
        let source = convert_to_pbgra(factory, &frame.cast()?)?;
        let (width, height) = source_size(&source)?;
        let pixels = copy_pixels(&source, width, height, 4, cancellation)?;
        Ok(crate::image::gain_map::GainMapPlane {
            width,
            height,
            pixels,
        })
    })
    .ok()
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
        Adapter::Wic | Adapter::WicRawTwoStage | Adapter::WicSubresolutionTwoStage => {
            let carries_gain_map = descriptor.is_some_and(|descriptor| descriptor.carries_gain_map);
            decode_with_wic(input, format_name, semantics, cancellation)
                .map(|decoded| {
                    if carries_gain_map {
                        attach_gain_map(decoded, input, cancellation)
                    } else {
                        decoded
                    }
                })
                .map_err(|mut error| {
                    if is_missing_codec_error(error.code)
                        && let Some(descriptor) = descriptor
                    {
                        error.store_codec_names = descriptor.store_codec_names;
                    }
                    error
                })
        }
        Adapter::Apng => match input {
            DecodeInput::File(path) => {
                let file = File::open(path).map_err(uncoded_error)?;
                decode_apng(BufReader::new(file), format_name, cancellation)
            }
            DecodeInput::Memory { bytes, .. } => {
                decode_apng(Cursor::new(*bytes), format_name, cancellation)
            }
        },
        Adapter::Svg => decode_svg(&input.read_all()?, format_name),
        Adapter::WebPAnimation => crate::image::fallback::decode_webp_animation(
            &input.read_all()?,
            format_name,
            usize::MAX,
        ),
        Adapter::Exr => match input {
            DecodeInput::File(path) => crate::image::fallback::decode_exr(path, format_name),
            DecodeInput::Memory { bytes, .. } => {
                crate::image::fallback::decode_exr_bytes(bytes, format_name)
            }
        }
        .and_then(|decoded| enforce_device_limit(decoded, cancellation)),
        Adapter::HeifWithWicPreferred => {
            decode_with_wic(input, format_name, semantics, cancellation).or_else(|error| {
                // Any WIC failure tries the bundled decoder, not just an unregistered codec.
                if error.is_cancelled() {
                    Err(error)
                } else {
                    crate::image::fallback::decode_heif(&input.read_all()?, format_name)
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
            decode_animation(factory, &decoder, 1, descriptor.name, cancellation).map(Some)
        })
        .ok()
        .flatten(),
        // The acTL chunk already identified the animation; WIC returns the default image.
        Adapter::Apng => decode_with_wic(
            &input,
            descriptor.name,
            &FrameSemantics::Single,
            cancellation,
        )
        .ok(),
        Adapter::WebPAnimation => crate::image::fallback::decode_webp_animation(
            &input.read_all().ok()?,
            descriptor.name,
            1,
        )
        .and_then(|decoded| enforce_device_limit(decoded, cancellation))
        .ok(),
        _ => None,
    }
}

/// Bitmap bytes a full decode would produce; submission and eviction budget this number.
pub fn decoded_weight(width: u32, height: u32, bytes_per_pixel: u32, frame_count: u64) -> u64 {
    let frame_count = frame_count.max(1);
    if frame_count > 1 {
        // Animations are never downscaled; the compositor works at canvas size.
        let frame_bytes = u64::from(width) * u64::from(height) * u64::from(bytes_per_pixel);
        if animation_budget_exceeded(frame_count, frame_bytes) {
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

pub fn probe_bytes_weight(bytes: &[u8], extension: Option<&str>) -> Option<u64> {
    probe_weight(&DecodeInput::Memory { bytes, extension })
}

fn probe_weight(input: &DecodeInput<'_>) -> Option<u64> {
    let descriptor = input.descriptor()?;
    match descriptor.adapter {
        Adapter::Wic | Adapter::WicRawTwoStage | Adapter::WicSubresolutionTwoStage => {
            probe_wic_weight(input, &descriptor.semantics)
        }
        Adapter::Apng => match input {
            DecodeInput::File(path) => probe_apng_weight(BufReader::new(File::open(path).ok()?)),
            DecodeInput::Memory { bytes, .. } => probe_apng_weight(Cursor::new(*bytes)),
        },
        Adapter::Svg => probe_svg_weight(&input.read_all().ok()?),
        Adapter::WebPAnimation => probe_webp_weight(input),
        Adapter::Exr => {
            let (width, height) = match input {
                DecodeInput::File(path) => crate::image::fallback::probe_exr_dimensions(path),
                DecodeInput::Memory { bytes, .. } => {
                    crate::image::fallback::probe_exr_bytes_dimensions(bytes)
                }
            }?;
            Some(decoded_weight(width, height, 8, 1))
        }
        Adapter::HeifWithWicPreferred => {
            // Mirrors the decode dispatch: WIC first, the bundled decoder on failure.
            probe_wic_weight(input, &descriptor.semantics).or_else(|| {
                let (width, height, storage) =
                    crate::image::fallback::probe_heif_dimensions_and_storage(
                        &input.read_all().ok()?,
                    )?;
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
        let (bits_per_channel, _) = frame_pixel_format_traits(factory, &frame);
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
fn probe_svg_weight(bytes: &[u8]) -> Option<u64> {
    let tree = parse_svg_tree(bytes).ok()?;
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
        DecodeInput::Memory { bytes, .. } => bytes,
    };
    let dimension = |offset: usize| -> Option<u32> {
        let bytes = header.get(offset..offset + 3)?;
        Some(1 + (u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16))
    };
    Some(decoded_weight(dimension(24)?, dimension(27)?, 4, 1))
}

/// Extension-only descriptor lookup, so the UI-thread checks do no I/O.
fn descriptor_for_path_extension(path: &Path) -> Option<&'static FormatDescriptor> {
    descriptor_for_extension(&crate::text::lowercase_extension(path)?)
}

/// A file whose decode runs preview first; magic probing never yields these formats.
pub fn is_two_stage_preview(path: &Path) -> bool {
    descriptor_for_path_extension(path).is_some_and(|descriptor| {
        matches!(
            descriptor.adapter,
            Adapter::WicRawTwoStage | Adapter::WicSubresolutionTwoStage
        )
    })
}

/// The preview stage of a two-stage file: a RAW embedded preview or a sub-resolution pass.
pub fn decode_two_stage_preview(path: &Path, cancellation: &AtomicBool) -> Option<DecodedImage> {
    let descriptor = descriptor_for_path_extension(path)?;
    match descriptor.adapter {
        Adapter::WicRawTwoStage => decode_raw_preview(path, descriptor.name, cancellation),
        Adapter::WicSubresolutionTwoStage => {
            decode_subresolution_preview(path, descriptor.name, cancellation)
        }
        _ => None,
    }
}

fn decode_raw_preview(
    path: &Path,
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> Option<DecodedImage> {
    let decoded = with_wic_factory(|factory| {
        let decoder = create_wic_decoder(factory, &DecodeInput::File(path))?;
        let preview =
            unsafe { decoder.GetPreview() }.or_else(|_| unsafe { decoder.GetThumbnail() })?;
        let frame = unsafe { decoder.GetFrame(0) }.ok();
        // One reader for both readers of it: building it parses the whole metadata tree.
        let metadata = frame
            .as_ref()
            .and_then(|frame| unsafe { frame.GetMetadataQueryReader() }.ok());
        let orientation = exif_orientation(metadata.as_ref());
        let icc_profile = frame
            .as_ref()
            .and_then(|frame| icc_profile_bytes(factory, frame));
        let exif = metadata.as_ref().and_then(read_exif);
        let source = convert_to_pbgra(factory, &preview)?;
        let source = apply_orientation(factory, source, orientation)?;
        let (width, height) = source_size(&source)?;
        let (source, pixel_width, pixel_height) =
            downscale_to_device_limit(factory, source, width, height)?;
        let pixels = copy_pixels(&source, pixel_width, pixel_height, 4, cancellation)?;
        Ok(DecodedImage {
            width,
            height,
            pixel_width,
            pixel_height,
            format_name,
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
            gain_map: None,
            gain_map_plane: None,
        })
    })
    .ok()?;
    Some(decoded)
}

/// Preview request: a quarter for float sources, half for the rest, capped near the monitor.
fn subresolution_target_size(width: u32, height: u32, float_native: bool) -> (u32, u32) {
    let target = largest_monitor_long_side().min(MAXIMUM_TEXTURE_DIMENSION);
    let longest = width.max(height).max(1);
    let class_divisor: u32 = if float_native { 4 } else { 2 };
    let divisor = class_divisor.max(longest.div_ceil(target));
    ((width / divisor).max(1), (height / divisor).max(1))
}

/// Component information for a pixel format GUID, the one WIC route to its traits.
fn pixel_format_information(
    factory: &IWICImagingFactory,
    format: &windows::core::GUID,
) -> WindowsResult<IWICPixelFormatInfo2> {
    unsafe { factory.CreateComponentInfo(format) }?.cast()
}

fn pixel_format_bits_per_pixel(
    factory: &IWICImagingFactory,
    format: &windows::core::GUID,
) -> WindowsResult<u32> {
    unsafe { pixel_format_information(factory, format)?.GetBitsPerPixel() }
}

/// Decodes a display-sized preview through the decoder's native scaler; None keeps one stage.
fn decode_subresolution_preview(
    path: &Path,
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> Option<DecodedImage> {
    let decoded = with_wic_factory(|factory| {
        let decoder = create_wic_decoder(factory, &DecodeInput::File(path))?;
        let frame = unsafe { decoder.GetFrame(0)? };
        let Some(scaled) = subresolution_source(factory, &frame, cancellation)? else {
            return Ok(None);
        };
        decode_frame_source(factory, &frame, scaled, format_name, cancellation).map(Some)
    })
    .ok()
    .flatten()?;
    Some(decoded)
}

/// Sub-resolution copy through the decoder's native scaler; None when the decoder cannot scale.
fn subresolution_source(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    cancellation: &AtomicBool,
) -> WindowsResult<Option<IWICBitmapSource>> {
    let Ok(transform) = frame.cast::<IWICBitmapSourceTransform>() else {
        return Ok(None);
    };
    let (full_width, full_height) = source_size(&frame.cast()?)?;
    let (_, float_native) = frame_pixel_format_traits(factory, frame);
    let (mut width, mut height) = subresolution_target_size(full_width, full_height, float_native);
    unsafe { transform.GetClosestSize(&mut width, &mut height)? };
    // A decoder that cannot scale returns the full size; stay single-stage then.
    if (width, height) == (full_width, full_height) {
        return Ok(None);
    }
    let mut format = unsafe { frame.GetPixelFormat()? };
    unsafe { transform.GetClosestPixelFormat(&mut format)? };
    let bits_per_pixel = pixel_format_bits_per_pixel(factory, &format)?;
    let stride = (width * bits_per_pixel).div_ceil(8);
    if cancellation.load(Ordering::Relaxed) {
        return Err(E_ABORT.into());
    }
    let mut pixels = vec![0u8; stride as usize * height as usize];
    unsafe {
        transform.CopyPixels(
            std::ptr::null(),
            width,
            height,
            &format,
            WICBitmapTransformRotate0,
            stride,
            &mut pixels,
        )?
    };
    let bitmap =
        unsafe { factory.CreateBitmapFromMemory(width, height, &format, stride, &pixels)? };
    Ok(Some(bitmap.cast()?))
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

fn decode_with_wic(
    input: &DecodeInput<'_>,
    format_name: &'static str,
    semantics: &FrameSemantics,
    cancellation: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    with_wic_factory(|factory| {
        let decoder = create_wic_decoder(factory, input)?;
        let frame_count = unsafe { decoder.GetFrameCount()? }.max(1);
        match semantics {
            FrameSemantics::Animation if frame_count > 1 => {
                decode_animation(factory, &decoder, frame_count, format_name, cancellation)
            }
            FrameSemantics::SizeVariants if frame_count > 1 => {
                decode_largest_frame(factory, &decoder, frame_count, format_name, cancellation)
            }
            _ => decode_single_frame(factory, &decoder, 0, format_name, cancellation),
        }
    })
    .map_err(DecodeError::from)
}

fn create_wic_decoder(
    factory: &IWICImagingFactory,
    input: &DecodeInput<'_>,
) -> WindowsResult<IWICBitmapDecoder> {
    match input {
        DecodeInput::File(path) => unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(*path),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
        },
        // The stream borrows the buffer; decoder and stream stay within this call.
        DecodeInput::Memory { bytes, .. } => unsafe {
            let stream = factory.CreateStream()?;
            stream.InitializeFromMemory(bytes)?;
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

/// Downscales fallback-decoded frames past the device limit; failure is a decode error.
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
    // u64 stride: a native EXR/HEIF width near u32::MAX would overflow width*bytes_per_pixel.
    let stride = u32::try_from(u64::from(width) * u64::from(bytes_per_pixel))
        .map_err(|_| uncoded_error("Image stride exceeds the addressable range"))?;
    let frame = decoded
        .frames
        .first_mut()
        .ok_or_else(|| uncoded_error("Image has no frames"))?;
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
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedImage> {
    let frame = unsafe { decoder.GetFrame(index)? };
    let pixel_source = frame.cast()?;
    decode_frame_source(factory, &frame, pixel_source, format_name, cancellation)
}

/// The single-frame pipeline over the frame itself or a sub-resolution copy of it.
fn decode_frame_source(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    pixel_source: IWICBitmapSource,
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedImage> {
    // One reader for both readers of it: building it parses the whole metadata tree.
    let metadata = unsafe { frame.GetMetadataQueryReader() }.ok();
    let orientation = exif_orientation(metadata.as_ref());
    let icc_profile = icc_profile_bytes(factory, frame);
    let exif = metadata.as_ref().and_then(read_exif);
    let (native_bits_per_channel, float_native) = frame_pixel_format_traits(factory, frame);
    let high_depth = native_bits_per_channel > 8;
    // PQ/HLG integers bypass WIC's sRGB-assuming float conversion.
    let hdr_encoding = if float_native {
        None
    } else {
        icc_profile.as_deref().and_then(icc_hdr_encoding)
    };
    let (frame_width, frame_height) = source_size(&pixel_source)?;
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
        convert_half_or_pbgra(factory, &pixel_source, target)?
    } else {
        (
            convert_to_pbgra(factory, &pixel_source)?,
            PixelStorage::Bgra8,
        )
    };
    // The 8bpc fallback loses the PQ/HLG code values along with the depth.
    let hdr_encoding = hdr_encoding.filter(|_| storage == PixelStorage::RgbaHalf);
    let source = apply_orientation(factory, source, orientation)?;
    let (width, height) = source_size(&source)?;
    let (source, pixel_width, pixel_height, storage) =
        match downscale_to_device_limit(factory, source.clone(), width, height) {
            Ok((scaled, scaled_width, scaled_height)) => {
                (scaled, scaled_width, scaled_height, storage)
            }
            // Refused formats fall back to 8-bit; PQ/HLG keeps the error to avoid false colors.
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
        hdr_encoding.map(|encoding| linearize_hdr_pixels(&mut pixels, encoding, 16));
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
            icc_profile.as_deref().and_then(icc::primaries)
        }
        None => None,
    };
    Ok(DecodedImage {
        width,
        height,
        pixel_width,
        pixel_height,
        format_name,
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
        gain_map: None,
        gain_map_plane: None,
    })
}

/// Native format traits: (bits per channel, float representation).
fn frame_pixel_format_traits(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> (u32, bool) {
    (|| -> WindowsResult<(u32, bool)> {
        let format = unsafe { frame.GetPixelFormat()? };
        let information = pixel_format_information(factory, &format)?;
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

fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

pub(crate) fn cicp_hdr_encoding(primaries: u8, transfer: u8) -> Option<HdrEncoding> {
    const CICP_TRANSFER_PQ: u8 = 16;
    const CICP_TRANSFER_HLG: u8 = 18;
    const CICP_PRIMARIES_BT709: u8 = 1;
    const CICP_PRIMARIES_BT2020: u8 = 9;
    const CICP_PRIMARIES_P3_D65: u8 = 12;
    let transfer = match transfer {
        CICP_TRANSFER_PQ => HdrTransfer::PerceptualQuantizer,
        CICP_TRANSFER_HLG => HdrTransfer::HybridLogGamma,
        _ => return None,
    };
    let primaries = match primaries {
        CICP_PRIMARIES_BT709 => HdrPrimaries::Bt709,
        CICP_PRIMARIES_BT2020 => HdrPrimaries::Bt2020,
        CICP_PRIMARIES_P3_D65 => HdrPrimaries::DisplayP3,
        _ => return None,
    };
    Some(HdrEncoding {
        transfer,
        primaries,
    })
}

/// ICC 'cicp' tag as an HDR encoding.
fn icc_hdr_encoding(icc: &[u8]) -> Option<HdrEncoding> {
    let offset = icc::tag_offset(icc, b"cicp")?;
    if icc.get(offset..offset + 4)? != b"cicp" {
        return None;
    }
    cicp_hdr_encoding(*icc.get(offset + 8)?, *icc.get(offset + 9)?)
}

/// Rec. 2100 nominal peak for the HLG OOTF (display-referred mapping).
const HLG_PEAK_NITS: f32 = 1000.0;

/// One entry per 16-bit code, allocated on the heap instead of moved from the stack.
pub(crate) fn boxed_lookup_table<T: Copy + Default + std::fmt::Debug>() -> Box<[T; 65536]> {
    vec![T::default(); 65536]
        .into_boxed_slice()
        .try_into()
        .expect("65536 entries")
}

/// Exact code lookup per source depth, with the full-range expansion folded in.
fn hdr_transfer_lookup_table(transfer: HdrTransfer, source_bits: u32) -> &'static [f32; 65536] {
    // One table per source depth, 1 through 16.
    type TablesByDepth = [OnceLock<Box<[f32; 65536]>>; 17];
    static PERCEPTUAL_QUANTIZER_TABLES: TablesByDepth = [const { OnceLock::new() }; 17];
    static HYBRID_LOG_GAMMA_TABLES: TablesByDepth = [const { OnceLock::new() }; 17];
    let (tables, function): (&TablesByDepth, fn(f32) -> f32) = match transfer {
        HdrTransfer::PerceptualQuantizer => {
            (&PERCEPTUAL_QUANTIZER_TABLES, perceptual_quantizer_nits)
        }
        HdrTransfer::HybridLogGamma => (&HYBRID_LOG_GAMMA_TABLES, hybrid_log_gamma_scene_linear),
    };
    tables[source_bits as usize].get_or_init(|| {
        let maximum = (1u32 << source_bits) - 1;
        let mut table = boxed_lookup_table::<f32>();
        let declared = maximum as usize + 1;
        for (code, value) in table[..declared].iter_mut().enumerate() {
            let expanded = (code as u32 * u32::from(u16::MAX) + maximum / 2) / maximum;
            *value = function(expanded as f32 / f32::from(u16::MAX));
        }
        // A broken decoder writing past the declared depth clamps rather than wrapping.
        table[declared..].fill(function(1.0));
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
    /// The Ultra HDR gain map uploaded beside the base, when the image carries one.
    pub gain_map: Option<ID3D11Texture2D>,
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
    let texture = upload_immutable_texture(
        upload_device,
        &description,
        &frame.pixels,
        image.row_pitch(),
    )?;
    Some(UploadedTexture {
        texture,
        gain_map: image
            .gain_map_plane
            .as_ref()
            .and_then(|plane| upload_gain_map_texture(upload_device, plane)),
        generation: upload_device.generation,
    })
}

/// Uploads the gain map plane; None quietly keeps the base SDR rendition.
fn upload_gain_map_texture(
    upload_device: &UploadDevice,
    plane: &crate::image::gain_map::GainMapPlane,
) -> Option<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: plane.width,
        Height: plane.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    upload_immutable_texture(upload_device, &description, &plane.pixels, plane.width * 4)
}

/// One immutable shader-resource texture from tightly packed pixels.
fn upload_immutable_texture(
    upload_device: &UploadDevice,
    description: &D3D11_TEXTURE2D_DESC,
    pixels: &[u8],
    row_pitch: u32,
) -> Option<ID3D11Texture2D> {
    let subresource = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: row_pitch,
        ..Default::default()
    };
    let mut texture = None;
    unsafe {
        upload_device.device.CreateTexture2D(
            &raw const *description,
            Some(&raw const subresource),
            Some(&raw mut texture),
        )
    }
    .ok()?;
    texture
}

/// Pixels per worker block; smaller buffers stay on one thread.
const PARALLEL_BLOCK_MINIMUM_PIXELS: usize = 262_144;

/// Block size in bytes: up to one block per core, each a whole number of pixels.
fn parallel_block_bytes(total_bytes: usize, bytes_per_pixel: usize) -> usize {
    let pixel_count = total_bytes / bytes_per_pixel;
    static CORES: OnceLock<usize> = OnceLock::new();
    let cores =
        *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |count| count.get()));
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
pub(crate) fn linearize_hdr_pixels(
    pixels: &mut [u8],
    encoding: HdrEncoding,
    source_bits: u32,
) -> u16 {
    let source_bits = source_bits.clamp(1, 16);
    let transfer_table = hdr_transfer_lookup_table(encoding.transfer, source_bits);
    let source_maximum = ((1u32 << source_bits) - 1) as f32;
    map_pixel_blocks(pixels, 8, |block| {
        linearize_block(block, transfer_table, encoding, source_maximum)
    })
    .into_iter()
    .max()
    .unwrap_or(0)
}

fn linearize_block(
    pixels: &mut [u8],
    transfer_table: &[f32; 65536],
    encoding: HdrEncoding,
    source_maximum: f32,
) -> u16 {
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
        let alpha = f32::from(u16::from_le_bytes([pixel[6], pixel[7]])) / source_maximum;
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
        let mut table = boxed_lookup_table::<u16>();
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

/// Per-channel maxima; the discarded alpha lane keeps the stride regular.
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
) -> Option<Arc<[u8]>> {
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
        let mut profile_byte_count = 0u32;
        let _ = unsafe { context.GetProfileBytes(&mut [], &raw mut profile_byte_count) };
        if profile_byte_count == 0 {
            continue;
        }
        let mut buffer = vec![0u8; profile_byte_count as usize];
        let mut written = 0u32;
        unsafe { context.GetProfileBytes(&mut buffer, &raw mut written) }.ok()?;
        buffer.truncate(written as usize);
        return Some(Arc::from(buffer));
    }
    None
}

fn decode_largest_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frame_count: u32,
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedImage> {
    let largest_index = largest_frame_index(decoder, frame_count)?;
    decode_single_frame(factory, decoder, largest_index, format_name, cancellation)
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
    format_name: &'static str,
    cancellation: &AtomicBool,
) -> WindowsResult<DecodedImage> {
    let container_reader = unsafe { decoder.GetMetadataQueryReader() }.ok();
    let container_query = |name: PCWSTR| {
        container_reader
            .as_ref()
            .and_then(|reader| query_u32(reader, name))
    };
    let canvas_width = container_query(w!("/logscrdesc/Width")).unwrap_or(0);
    let canvas_height = container_query(w!("/logscrdesc/Height")).unwrap_or(0);
    // A container without a logical screen leaves the first frame to set the canvas.
    let (canvas_width, canvas_height) = if canvas_width == 0 || canvas_height == 0 {
        source_size(&unsafe { decoder.GetFrame(0)? }.cast()?)?
    } else {
        (canvas_width, canvas_height)
    };
    let mut compositor = FrameCompositor::new(canvas_width, canvas_height).ok_or(E_OUTOFMEMORY)?;
    let mut icc_profile = None;
    // Reused across frames; the copy writes every byte it is given.
    let mut frame_pixels: Vec<u8> = Vec::new();
    for index in 0..frame_count {
        if cancellation.load(Ordering::Relaxed) {
            return Err(E_ABORT.into());
        }
        // The container's frame count is real, so the budget is known after frame one.
        if !compositor.accepts_another(u64::from(frame_count)) {
            break;
        }
        let frame = unsafe { decoder.GetFrame(index)? };
        if index == 0 {
            icc_profile = icc_profile_bytes(factory, &frame);
        }
        let metadata = frame_metadata(&frame);
        let source = convert_to_pbgra(factory, &frame.cast()?)?;
        let (frame_width, frame_height) = source_size(&source)?;
        copy_pixels_into(
            &source,
            frame_width,
            frame_height,
            4,
            cancellation,
            &mut frame_pixels,
        )?;
        compositor.add_frame(FrameRegion {
            pixels: &frame_pixels,
            left: metadata.left,
            top: metadata.top,
            width: frame_width,
            height: frame_height,
            // GIF frames are placed sub-rectangles that always composite over the canvas.
            blend: FrameBlend::Over,
            disposal: match metadata.disposal {
                2 => FrameDisposal::Background,
                3 => FrameDisposal::Previous,
                _ => FrameDisposal::Keep,
            },
            delay_milliseconds: metadata.delay_milliseconds,
        });
    }
    let (frames, frames_truncated) = compositor.finish();
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
        gain_map: None,
        gain_map_plane: None,
    })
}

/// The part of a `width` x `height` rectangle at (`left`, `top`) that lies on the canvas.
fn visible_rectangle(
    canvas_width: u32,
    canvas_height: u32,
    width: u32,
    height: u32,
    left: u32,
    top: u32,
) -> Option<(usize, usize)> {
    let visible_width = width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = height.min(canvas_height.saturating_sub(top)) as usize;
    // An off-canvas offset clips everything; the row start would still index past the end.
    (visible_width > 0 && visible_height > 0).then_some((visible_width, visible_height))
}

/// Premultiplied source-over blend, clipped to the canvas.
#[expect(clippy::too_many_arguments)]
fn blend_over(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let Some((visible_width, visible_height)) = visible_rectangle(
        canvas_width,
        canvas_height,
        source_width,
        source_height,
        left,
        top,
    ) else {
        return;
    };
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

fn clear_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
) {
    let Some((visible_width, visible_height)) =
        visible_rectangle(canvas_width, canvas_height, width, height, left, top)
    else {
        return;
    };
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

/// Converts to the requested half-domain format, falling back to 8-bit PBGRA on refusal.
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

fn exif_orientation(reader: Option<&IWICMetadataQueryReader>) -> u32 {
    reader
        .and_then(|reader| query_u32(reader, w!("System.Photo.Orientation")))
        .unwrap_or(1)
}

fn read_exif(reader: &IWICMetadataQueryReader) -> Option<ExifMetadata> {
    let metadata = ExifMetadata {
        date_taken: query_filetime(reader, w!("System.Photo.DateTaken")),
        rating: query_u32(reader, w!("System.Rating")),
        camera_maker: query_string(reader, w!("System.Photo.CameraManufacturer")),
        camera_model: query_string(reader, w!("System.Photo.CameraModel")),
        f_stop: query_f64(reader, w!("System.Photo.FNumber")),
        exposure_time_seconds: query_f64(reader, w!("System.Photo.ExposureTime")),
        iso_speed: query_u32(reader, w!("System.Photo.ISOSpeed")),
        exposure_bias: query_f64(reader, w!("System.Photo.ExposureBias")),
        focal_length_millimeters: query_f64(reader, w!("System.Photo.FocalLength")),
        maximum_aperture: query_f64(reader, w!("System.Photo.MaxAperture")),
        metering_mode: query_u32(reader, w!("System.Photo.MeteringMode")),
        flash: query_u32(reader, w!("System.Photo.Flash")),
    };
    metadata.any_present().then_some(metadata)
}

/// Runs `convert` on the named metadata PROPVARIANT, clearing it afterwards.
fn query_propvariant<T>(
    reader: &IWICMetadataQueryReader,
    name: PCWSTR,
    convert: impl FnOnce(&PROPVARIANT) -> Option<T>,
) -> Option<T> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &raw mut value) }.ok()?;
    let converted = convert(&value);
    let _ = unsafe { PropVariantClear(&raw mut value) };
    converted
}

fn query_f64(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<f64> {
    query_propvariant(reader, name, |value| {
        unsafe { PropVariantToDouble(std::ptr::from_ref(value)) }.ok()
    })
    .filter(|number| number.is_finite())
}

fn query_string(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<String> {
    query_propvariant(reader, name, |value| {
        unsafe { PropVariantToStringAlloc(std::ptr::from_ref(value)) }
            .ok()
            .map(crate::text::take_task_memory_string)
    })
    .and_then(|text| {
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn query_filetime(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<std::time::SystemTime> {
    use windows::Win32::System::Variant::PSTF_UTC;
    let file_time = query_propvariant(reader, name, |value| {
        unsafe { PropVariantToFileTime(std::ptr::from_ref(value), PSTF_UTC) }.ok()
    })?;
    let intervals =
        (u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime);
    let unix_intervals = intervals.checked_sub(FILETIME_UNIX_EPOCH)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(unix_intervals * 100))
}

fn query_u32(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u32> {
    query_propvariant(reader, name, |value| {
        unsafe { PropVariantToUInt32(std::ptr::from_ref(value)) }.ok()
    })
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
        store_codec_names: &[],
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
    // Untrusted IHDR: the compositor refuses an over-budget canvas, which bounds the frame buffer.
    let Some(mut compositor) = FrameCompositor::new(canvas_width, canvas_height) else {
        return Err(uncoded_error("APNG canvas is too large to decode"));
    };
    let icc_profile = reader
        .info()
        .icc_profile
        .as_ref()
        .map(|profile| Arc::from(&profile[..]));
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

    // Reused across frames; the conversion writes every byte it is given.
    let mut region_pixels: Vec<u8> = Vec::new();
    for index in 0..animation_frame_count {
        if cancellation.load(Ordering::Relaxed) {
            return Err(DecodeError::cancelled());
        }
        // acTL's declared count is untrusted, so the budget runs frame by frame.
        if !compositor.accepts_one_more() {
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
        pixels_to_premultiplied_bgra_into(
            &buffer[..output.buffer_size()],
            output.color_type,
            frame_control.width,
            frame_control.height,
            &mut region_pixels,
        )?;

        let delay_denominator = if frame_control.delay_den == 0 {
            100
        } else {
            u32::from(frame_control.delay_den)
        };
        compositor.add_frame(FrameRegion {
            pixels: &region_pixels,
            left: frame_control.x_offset,
            top: frame_control.y_offset,
            width: frame_control.width,
            height: frame_control.height,
            blend: match frame_control.blend_op {
                png::BlendOp::Source => FrameBlend::Replace,
                png::BlendOp::Over => FrameBlend::Over,
            },
            disposal: match frame_control.dispose_op {
                png::DisposeOp::Background => FrameDisposal::Background,
                png::DisposeOp::Previous => FrameDisposal::Previous,
                png::DisposeOp::None => FrameDisposal::Keep,
            },
            delay_milliseconds: (u32::from(frame_control.delay_num) * 1000 / delay_denominator)
                .max(10),
        });
    }
    let (frames, frames_truncated) = compositor.finish();
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
        gain_map: None,
        gain_map_plane: None,
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

/// Fills `output` with the converted region, so an animation reuses one buffer across frames.
fn pixels_to_premultiplied_bgra_into(
    pixels: &[u8],
    color_type: png::ColorType,
    width: u32,
    height: u32,
    output: &mut Vec<u8>,
) -> Result<(), DecodeError> {
    let pixel_count = width as usize * height as usize;
    output.resize(pixel_count * 4, 0);
    match color_type {
        png::ColorType::Rgba => {
            premultiplied_bgra_from_rgba(&pixels[..pixel_count * 4], output);
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
                "Unsupported PNG color type after normalization: {other:?}"
            )));
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn copy_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let Some((visible_width, visible_height)) = visible_rectangle(
        canvas_width,
        canvas_height,
        source_width,
        source_height,
        left,
        top,
    ) else {
        return;
    };
    for row in 0..visible_height {
        let source_start = row * source_width as usize * 4;
        let canvas_start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        canvas[canvas_start..canvas_start + visible_width * 4]
            .copy_from_slice(&source[source_start..source_start + visible_width * 4]);
    }
}

fn decode_svg(bytes: &[u8], format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let tree = parse_svg_tree(bytes)?;
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
        gain_map: None,
        gain_map_plane: None,
    })
}

fn parse_svg_tree(bytes: &[u8]) -> Result<resvg::usvg::Tree, DecodeError> {
    let options = resvg::usvg::Options {
        fontdb: font_database().clone(),
        ..Default::default()
    };
    resvg::usvg::Tree::from_data(bytes, &options).map_err(uncoded_error)
}

/// Raster size and scale at the largest monitor's long side; probe and decode must agree.
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

/// Cleared on a display reconfigure; 0 means the system is queried again.
static LARGEST_MONITOR_LONG_SIDE: AtomicU32 = AtomicU32::new(0);

/// Clears the cached monitor size; the listing invalidates display-sized weights alongside.
pub fn invalidate_monitor_size() {
    LARGEST_MONITOR_LONG_SIDE.store(0, Ordering::Relaxed);
}

fn largest_monitor_long_side() -> u32 {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::core::BOOL;

    extern "system" fn monitor_callback(
        _monitor: HMONITOR,
        _device_context: HDC,
        bounds: *mut RECT,
        longest_side_pointer: LPARAM,
    ) -> BOOL {
        let longest = unsafe { &mut *(longest_side_pointer.0 as *mut i32) };
        let bounds = unsafe { &*bounds };
        *longest = (*longest)
            .max(bounds.right - bounds.left)
            .max(bounds.bottom - bounds.top);
        true.into()
    }

    let cached = LARGEST_MONITOR_LONG_SIDE.load(Ordering::Relaxed);
    if cached > 0 {
        return cached;
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
    let longest = if longest > 0 { longest as u32 } else { 1920 };
    LARGEST_MONITOR_LONG_SIDE.store(longest, Ordering::Relaxed);
    longest
}

/// Four-channel test pixels from a linear congruential sequence; alpha hits 0 and 255 too.
#[cfg(test)]
pub(crate) fn random_pixels(count: usize, mut state: u32) -> Vec<u8> {
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

/// Copies in strips so a cancelled decode can stop between them.
fn copy_pixels(
    source: &IWICBitmapSource,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    cancellation: &AtomicBool,
) -> WindowsResult<Vec<u8>> {
    let mut pixels = Vec::new();
    copy_pixels_into(
        source,
        width,
        height,
        bytes_per_pixel,
        cancellation,
        &mut pixels,
    )?;
    Ok(pixels)
}

/// Fills `pixels` with the frame, so an animation reuses one buffer across frames.
fn copy_pixels_into(
    source: &IWICBitmapSource,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    cancellation: &AtomicBool,
    pixels: &mut Vec<u8>,
) -> WindowsResult<()> {
    const STRIP_ROWS: u32 = 256;
    let stride = width * bytes_per_pixel;
    pixels.resize(stride as usize * height as usize, 0);
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
    Ok(())
}

#[cfg(test)]
mod compositor_tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;

    /// CRC-32 over an APNG/PNG chunk's type and data (polynomial 0xEDB88320).
    fn png_chunk_crc(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc ^ 0xFFFF_FFFF
    }

    #[test]
    fn an_oversized_apng_header_is_refused_without_aborting() {
        // A real 1x1 PNG with its IHDR rewritten to an oversized canvas; the decode must refuse it.
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("header");
            writer.write_image_data(&[0, 0, 0, 0]).expect("pixels");
        }
        // IHDR data begins after the 8-byte signature and the length+type fields.
        png[16..20].copy_from_slice(&60000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60000u32.to_be_bytes());
        let crc = png_chunk_crc(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let decoded = decode_apng(Cursor::new(png), "PNG", &AtomicBool::new(false));
        assert!(decoded.is_err(), "oversized canvas must fail closed");
    }

    #[test]
    fn frames_placed_past_the_canvas_edge_are_clipped_without_panicking() {
        // left past the width with a bottom row still visible used to index past the end.
        let mut canvas = vec![0u8; 10 * 10 * 4];
        let source = vec![255u8; 4];
        blend_over(&mut canvas, 10, 10, &source, 1, 1, 15, 9);
        copy_rectangle(&mut canvas, 10, 10, &source, 1, 1, 15, 9);
        clear_rectangle(&mut canvas, 10, 10, 15, 9, 1, 1);
        assert!(canvas.iter().all(|&byte| byte == 0), "nothing is visible");
    }

    /// One opaque pixel of the given blue, in the premultiplied BGRA the compositor takes.
    fn blue_pixel(blue: u8) -> Vec<u8> {
        vec![blue, 0, 0, 255]
    }

    fn region(pixels: &[u8], disposal: FrameDisposal) -> FrameRegion<'_> {
        FrameRegion {
            pixels,
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            blend: FrameBlend::Replace,
            disposal,
            delay_milliseconds: 40,
        }
    }

    #[test]
    fn keeping_the_canvas_carries_a_frame_into_the_next() {
        let mut compositor = FrameCompositor::new(2, 1).expect("canvas");
        compositor.add_frame(region(&blue_pixel(10), FrameDisposal::Keep));
        compositor.add_frame(FrameRegion {
            left: 1,
            ..region(&blue_pixel(20), FrameDisposal::Keep)
        });
        let (frames, truncated) = compositor.finish();
        assert!(!truncated);
        assert_eq!(frames[0].pixels[0], 10);
        // The second frame still shows the first, which nothing disposed of.
        assert_eq!(frames[1].pixels[0], 10);
        assert_eq!(frames[1].pixels[4], 20);
    }

    #[test]
    fn disposing_to_the_background_clears_only_that_region() {
        let mut compositor = FrameCompositor::new(2, 1).expect("canvas");
        compositor.add_frame(FrameRegion {
            left: 1,
            ..region(&blue_pixel(20), FrameDisposal::Keep)
        });
        compositor.add_frame(region(&blue_pixel(10), FrameDisposal::Background));
        compositor.add_frame(FrameRegion {
            width: 0,
            ..region(&[], FrameDisposal::Keep)
        });
        let (frames, _) = compositor.finish();
        assert_eq!(frames[1].pixels[0], 10, "the frame is kept as it was shown");
        assert_eq!(frames[2].pixels[0], 0, "its own region is cleared");
        assert_eq!(frames[2].pixels[4], 20, "the rest of the canvas stands");
    }

    #[test]
    fn disposing_to_previous_restores_what_the_frame_covered() {
        let mut compositor = FrameCompositor::new(1, 1).expect("canvas");
        compositor.add_frame(region(&blue_pixel(10), FrameDisposal::Keep));
        compositor.add_frame(region(&blue_pixel(20), FrameDisposal::Previous));
        compositor.add_frame(FrameRegion {
            width: 0,
            ..region(&[], FrameDisposal::Keep)
        });
        let (frames, _) = compositor.finish();
        assert_eq!(frames[1].pixels[0], 20, "the frame is kept as it was shown");
        assert_eq!(frames[2].pixels[0], 10, "the canvas goes back to before it");
    }

    #[test]
    fn blending_over_keeps_what_a_transparent_frame_does_not_cover() {
        let mut compositor = FrameCompositor::new(1, 1).expect("canvas");
        compositor.add_frame(region(&blue_pixel(10), FrameDisposal::Keep));
        compositor.add_frame(FrameRegion {
            blend: FrameBlend::Over,
            ..region(&[0, 0, 0, 0], FrameDisposal::Keep)
        });
        let (frames, _) = compositor.finish();
        assert_eq!(frames[1].pixels[0], 10, "a clear frame leaves the canvas");
    }

    #[test]
    fn frames_past_the_byte_budget_collapse_to_the_first() {
        // One frame of this canvas is a quarter of the budget, so the fifth is refused.
        let side = 8192;
        let mut compositor = FrameCompositor::new(side, side).expect("canvas");
        for _ in 0..4 {
            assert!(compositor.accepts_one_more());
            compositor.add_frame(region(&blue_pixel(10), FrameDisposal::Keep));
        }
        assert!(!compositor.accepts_one_more());
        let (frames, truncated) = compositor.finish();
        assert_eq!(frames.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn a_canvas_over_the_budget_is_refused() {
        assert!(FrameCompositor::new(65535, 65535).is_none());
    }

    /// The disposal fixture's four frames: keep, background, previous, then an empty one.
    #[test]
    #[ignore = "needs test/ fixtures"]
    fn the_apng_disposal_fixture_composes_as_its_frames_declare() {
        let file = std::fs::File::open("test/disposal_apng.png").expect("fixture");
        let decoded = decode_apng(
            std::io::BufReader::new(file),
            "PNG",
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(decoded.frames.len(), 4);
        let pixel = |frame: usize, x: usize, y: usize| {
            let start = (y * decoded.width as usize + x) * 4;
            decoded.frames[frame].pixels[start..start + 4].to_vec()
        };
        // The last frame keeps the square nothing disposed of, and neither of the other two.
        assert_eq!(pixel(3, 8, 12)[2], 220, "the kept square stays");
        assert_eq!(
            pixel(3, 30, 12),
            vec![0, 0, 0, 0],
            "both disposals undid theirs"
        );
        assert_eq!(
            pixel(1, 30, 12)[1],
            180,
            "the background frame showed its square"
        );
        assert_eq!(
            pixel(2, 30, 12)[0],
            230,
            "the previous frame showed its square"
        );
        // Both disposals undid their frame, so the canvas is the opening one again.
        assert_eq!(decoded.frames[3].pixels, decoded.frames[0].pixels);
    }

    /// The same four frames as the APNG fixture, read through WIC.
    #[test]
    #[ignore = "needs test/ fixtures and a WIC GIF decoder"]
    fn the_gif_disposal_fixture_composes_as_its_frames_declare() {
        // WIC needs COM; the worker initializes it at thread start, tests do it here.
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        let decoded = with_wic_factory(|factory| {
            let decoder = create_wic_decoder(
                factory,
                &DecodeInput::File(Path::new("test/disposal_gif.gif")),
            )?;
            let frame_count = unsafe { decoder.GetFrameCount()? };
            decode_animation(
                factory,
                &decoder,
                frame_count,
                "GIF",
                &AtomicBool::new(false),
            )
        })
        .expect("decode");
        assert_eq!(decoded.frames.len(), 4);
        let pixel = |frame: usize, x: usize, y: usize| {
            let start = (y * decoded.width as usize + x) * 4;
            decoded.frames[frame].pixels[start..start + 4].to_vec()
        };
        assert_eq!(pixel(3, 8, 12)[2], 220, "the kept square stays");
        assert_eq!(
            pixel(3, 30, 12),
            vec![0, 0, 0, 0],
            "both disposals undid theirs"
        );
        // Both disposals undid their frame, so the canvas is the opening one again.
        assert_eq!(decoded.frames[3].pixels, decoded.frames[0].pixels);
    }
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
mod descriptor_probe_tests {
    use super::*;

    #[test]
    fn two_stage_detection_is_extension_only() {
        assert!(is_two_stage_preview(Path::new("photo.dng")));
        assert!(is_two_stage_preview(Path::new("PHOTO.DNG")));
        assert!(is_two_stage_preview(Path::new("capture.jxr")));
        assert!(!is_two_stage_preview(Path::new("photo.png")));
        assert!(!is_two_stage_preview(Path::new("photo")));
    }

    #[test]
    fn xml_probes_as_svg_only_with_an_svg_tag() {
        let svg_document = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(
            descriptor_for_magic(svg_document).map(|d| d.name),
            Some("SVG")
        );
        let plain_xml = b"<?xml version=\"1.0\"?>\n<note><to>reader</to></note>";
        assert!(descriptor_for_magic(plain_xml).is_none());
        let bare_svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(descriptor_for_magic(bare_svg).map(|d| d.name), Some("SVG"));
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
            let table = hdr_transfer_lookup_table(transfer, 16);
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
        let transfer_table = hdr_transfer_lookup_table(encoding.transfer, 16);
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
            linearize_hdr_pixels(&mut pixels, encoding, 16);
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
        linearize_hdr_pixels(&mut pixels, encoding, 16);
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
            let maximum_bits = linearize_hdr_pixels(&mut pixels, encoding, 16);
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
            let maximum_bits = linearize_hdr_pixels(&mut scratch, encoding, 16);
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
                linearize_hdr_pixels(&mut scratch, encoding, 16);
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

    #[test]
    fn rgba_conversion_matches_the_scalar_reference() {
        let rgba = random_pixels(64 * 64, 3);
        let mut converted = Vec::new();
        let converted_ok =
            pixels_to_premultiplied_bgra_into(&rgba, png::ColorType::Rgba, 64, 64, &mut converted);
        assert!(converted_ok.is_ok(), "conversion failed");
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
        let rgb: Vec<u8> = random_pixels(64 * 64, 11)
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        let mut converted = Vec::new();
        let converted_ok =
            pixels_to_premultiplied_bgra_into(&rgb, png::ColorType::Rgb, 64, 64, &mut converted);
        assert!(converted_ok.is_ok(), "conversion failed");
        let mut expected = Vec::new();
        for pixel in rgb.chunks_exact(3) {
            expected.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        assert_eq!(converted, expected);
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn rgba_conversion_timing() {
        let rgba = random_pixels(1920 * 1080, 17);
        let mut reused = Vec::new();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            for _ in 0..50 {
                let mut fresh = Vec::new();
                let _ = pixels_to_premultiplied_bgra_into(
                    &rgba,
                    png::ColorType::Rgba,
                    1920,
                    1080,
                    &mut fresh,
                );
                std::hint::black_box(&fresh);
            }
            println!(
                "rgba conversion 50 frames, new buffer each={:?}",
                start.elapsed()
            );
            let start = std::time::Instant::now();
            for _ in 0..50 {
                let _ = pixels_to_premultiplied_bgra_into(
                    &rgba,
                    png::ColorType::Rgba,
                    1920,
                    1080,
                    &mut reused,
                );
                std::hint::black_box(&reused);
            }
            println!(
                "rgba conversion 50 frames, one buffer={:?}",
                start.elapsed()
            );
        }
    }

    #[test]
    #[ignore = "manual timing comparison (--nocapture)"]
    fn rgb_conversion_timing() {
        let rgb: Vec<u8> = random_pixels(1920 * 1080, 23)
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        let mut reused = Vec::new();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            for _ in 0..50 {
                let mut fresh = Vec::new();
                let _ = pixels_to_premultiplied_bgra_into(
                    &rgb,
                    png::ColorType::Rgb,
                    1920,
                    1080,
                    &mut fresh,
                );
                std::hint::black_box(&fresh);
            }
            println!(
                "rgb conversion 50 frames, new buffer each={:?}",
                start.elapsed()
            );
            let start = std::time::Instant::now();
            for _ in 0..50 {
                let _ = pixels_to_premultiplied_bgra_into(
                    &rgb,
                    png::ColorType::Rgb,
                    1920,
                    1080,
                    &mut reused,
                );
                std::hint::black_box(&reused);
            }
            println!("rgb conversion 50 frames, one buffer={:?}", start.elapsed());
        }
    }
}

/// A huge declared acTL num_frames must not drive the reservation.
#[cfg(test)]
mod apng_tests {
    use super::*;

    #[test]
    #[ignore = "needs test/apng_huge_frames.png"]
    fn a_huge_declared_frame_count_does_not_over_reserve() {
        let png_bytes = std::fs::read("test/apng_huge_frames.png").expect("fixture");
        let cancellation = AtomicBool::new(false);
        // Frames run out after the first; decode errors without the huge reservation.
        assert!(decode_bytes(&png_bytes, Some("png"), &cancellation).is_err());
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

#[cfg(test)]
mod gain_map_decode_tests {
    use super::*;

    /// WIC needs COM; the worker initializes it at thread start, tests do it here.
    fn initialize_com() {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    #[test]
    #[ignore = "needs test/test_uhdr fixtures"]
    fn an_ultra_hdr_jpeg_decodes_with_its_gain_plane() {
        initialize_com();
        let cancellation = AtomicBool::new(false);
        let decoded = decode_file(
            Path::new("test/test_uhdr/Originals/Ultra_HDR_Samples_Originals_01.jpg"),
            &cancellation,
        )
        .unwrap_or_else(|error| panic!("decode: {}", error.message));
        let metadata = decoded.gain_map.expect("gain map parameters");
        assert!(metadata.hdr_capacity_maximum > 0.0);
        let plane = decoded.gain_map_plane.as_ref().expect("gain plane");
        assert!(plane.fits_within(decoded.width, decoded.height));
        assert_eq!(
            plane.pixels.len(),
            plane.width as usize * plane.height as usize * 4
        );
        // The plane joins the cache weight on top of the base frame bytes.
        assert_eq!(
            decoded.pixel_bytes(),
            decoded.frame_byte_length() + plane.pixels.len()
        );
    }

    #[test]
    #[ignore = "needs test/test_uhdr fixtures"]
    fn a_plain_jpeg_carries_no_gain_map() {
        initialize_com();
        let cancellation = AtomicBool::new(false);
        let decoded = decode_file(
            Path::new("test/test_uhdr/SDR Emulation/Ultra_HDR_Samples_Emulated_01_base.jpg"),
            &cancellation,
        )
        .unwrap_or_else(|error| panic!("decode: {}", error.message));
        assert!(decoded.gain_map.is_none());
        assert!(decoded.gain_map_plane.is_none());
        assert_eq!(decoded.pixel_bytes(), decoded.frame_byte_length());
    }
}
