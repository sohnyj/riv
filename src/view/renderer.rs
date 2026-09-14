//! D3D11 + D2D draw path; frames present through composed presentation buffers.

use windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
    D2D1_COMPOSITE_MODE_SOURCE_OVER, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1ColorManagement, CLSID_D2D1HdrToneMap, CLSID_D2D1WhiteLevelAdjustment,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_BUFFER_PRECISION_16BPC_FLOAT, D2D1_COLOR_SPACE_CUSTOM,
    D2D1_COLOR_SPACE_SCRGB, D2D1_COLOR_SPACE_SRGB,
    D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_PROP_DESTINATION_RENDERING_INTENT, D2D1_COLORMANAGEMENT_PROP_QUALITY,
    D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_PROP_SOURCE_RENDERING_INTENT, D2D1_COLORMANAGEMENT_QUALITY_BEST,
    D2D1_COLORMANAGEMENT_RENDERING_INTENT_RELATIVE_COLORIMETRIC, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA1_G10, D2D1_HDRTONEMAP_DISPLAY_MODE_HDR,
    D2D1_HDRTONEMAP_PROP_DISPLAY_MODE, D2D1_HDRTONEMAP_PROP_INPUT_MAX_LUMINANCE,
    D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE, D2D1_INTERPOLATION_MODE,
    D2D1_INTERPOLATION_MODE_CUBIC, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_PROPERTY_TYPE_COLOR_CONTEXT, D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT,
    D2D1_SIMPLE_COLOR_PROFILE, D2D1_WHITELEVELADJUSTMENT_PROP_INPUT_WHITE_LEVEL,
    D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL, D2D1CreateFactory, ID2D1Bitmap1,
    ID2D1ColorContext, ID2D1DeviceContext, ID2D1DeviceContext5, ID2D1Effect, ID2D1Factory1,
    ID2D1Image,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_12_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R16G16B16A16_UNORM,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::{Interface, Result};
use windows_numerics::{Matrix3x2, Vector2};

use std::sync::Arc;
use std::thread;

use crate::image::color::{DisplayLuminances, SDR_REFERENCE_WHITE_NITS, nearest_gamut_label};
use crate::image::decode::{
    DecodedImage, PixelStorage, UploadDevice, UploadedTexture, maximum_resource_bytes,
    try_zeroed_buffer, upload_still_texture,
};
use crate::image::gain_map::GainMapMetadata;
use crate::image::icc;
use crate::view::dither::DitherMode;
use crate::view::gain::GainMapPass;
use crate::view::presentation::{self, CompositionPresenter};
use crate::view::quantize::QuantizePass;
use crate::view::transform::is_unit_scale;

/// Frame-slot wait ceiling: a stalled present queue must not freeze the caller.
pub const FRAME_SLOT_TIMEOUT_MILLISECONDS: u32 = 1000;

/// Two presentation buffers: draw one while the last presented one retires.
const PRESENTATION_BUFFER_COUNT: usize = 2;

/// The FP16 scRGB backbuffer of HDR and ACM-on wide gamut output; DWM quantizes it.
const SCRGB_BACKBUFFER_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

/// The 8-bit backbuffer of plain SDR output; the app quantizes and dithers it.
const SDR_BACKBUFFER_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

/// Bits per channel of that backbuffer: the dither step and the output label read it.
const SDR_BACKBUFFER_BITS: u32 = 8;

/// The color space composition reads the SDR backbuffer in.
const SDR_BACKBUFFER_COLOR_SPACE: DXGI_COLOR_SPACE_TYPE = DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

/// The UNORM16 intermediate scene the quantize pass reads.
const SCENE_TEXTURE_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_UNORM;

/// The tone map for HDR content on SDR output, with the stages that follow it.
struct ToneMapStage {
    tone_map: ID2D1Effect,
    /// Reinterprets scene-referred white as display-referred.
    normalize: ID2D1Effect,
    /// Re-encodes to the sRGB backbuffer; None on the FP16 scRGB backbuffer of ACM-on SDR.
    output_encoding: Option<ID2D1Effect>,
}

struct ModeEffects {
    color_management: ID2D1Effect,
    /// SDR output only: HDR displays pass content through.
    tone_map: Option<ToneMapStage>,
    /// HDR output only: the SDR white-level boost.
    white_level: Option<ID2D1Effect>,
}

#[derive(Clone, PartialEq)]
pub struct OutputMode {
    pub hdr: bool,
    pub advanced_color: bool,
    /// The display's ICC profile, the ACM-off SDR destination; None when unavailable.
    pub display_profile: Option<Arc<[u8]>>,
}

impl OutputMode {
    pub fn is_sdr_wide_gamut(&self) -> bool {
        !self.hdr && self.advanced_color
    }
}

/// The scaling-filter setting: the D2D interpolation and its information-panel label.
#[derive(Clone, Copy)]
pub enum ScalingFilter {
    Nearest,
    Bilinear,
    Bicubic,
    HighQuality,
}

impl ScalingFilter {
    /// Stored order: the settings value is a position here, and so is the combo row.
    pub const IN_SETTING_ORDER: [Self; 4] = [
        Self::Nearest,
        Self::Bilinear,
        Self::Bicubic,
        Self::HighQuality,
    ];

    pub fn from_setting(value: u32) -> Self {
        Self::IN_SETTING_ORDER
            .get(value as usize)
            .copied()
            .unwrap_or(Self::Bilinear)
    }

    pub fn interpolation(self) -> D2D1_INTERPOLATION_MODE {
        match self {
            Self::Nearest => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            Self::Bilinear => D2D1_INTERPOLATION_MODE_LINEAR,
            Self::Bicubic => D2D1_INTERPOLATION_MODE_CUBIC,
            Self::HighQuality => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Nearest => "Nearest",
            Self::Bilinear => "Bilinear",
            Self::Bicubic => "Bicubic",
            Self::HighQuality => "High quality",
        }
    }
}

/// Tone-map luminances for the information overlay (nits).
#[derive(Clone, Copy, PartialEq)]
pub struct ToneMapLuminances {
    pub hdr_display: bool,
    /// None where the display reported nothing; the overlay drops the line rather than guess.
    pub display_peak_nits: Option<f32>,
    pub display_full_frame_nits: Option<f32>,
    pub output_target_nits: f32,
}

/// The display conditions a bake answers to; a change means a re-bake.
#[derive(Clone, Copy, PartialEq)]
struct BakeConditions {
    display_headroom: f32,
    sdr_white_boost: f32,
}

/// The current image's gain map inputs and its baked rendition.
struct GainMapState {
    metadata: GainMapMetadata,
    base_bitmap: ID2D1Bitmap1,
    /// Slim copy of the decoded image: the wiring parameters for both renditions.
    base_image: DecodedImage,
    /// The base's stated primaries, resolved once for the baked rendition's wiring.
    base_primaries: Option<[[f32; 2]; 3]>,
    base_view: ID3D11ShaderResourceView,
    gain_map_view: ID3D11ShaderResourceView,
    baked: Option<BakedGainMap>,
    /// Conditions of the adopted bake; None while the base rendition shows.
    adopted_conditions: Option<BakeConditions>,
}

/// The bake target and its D2D wrap; the bitmap keeps the texture alive.
struct BakedGainMap {
    render_target_view: ID3D11RenderTargetView,
    bitmap: ID2D1Bitmap1,
}

impl BakedGainMap {
    /// An FP16 render target of the base's size, wrapped as a D2D bitmap; None when D3D refuses.
    fn create(
        d3d_device: &ID3D11Device,
        d2d_context: &ID2D1DeviceContext,
        size: (u32, u32),
    ) -> Option<Self> {
        let texture = crate::view::texture::create_render_texture(
            d3d_device,
            size,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_RESOURCE_MISC_FLAG(0),
        )
        .ok()?;
        let render_target_view =
            crate::view::texture::create_render_target_view(d3d_device, &texture).ok()?;
        let properties = image_bitmap_properties(PixelStorage::RgbaHalf);
        let bitmap = Renderer::bitmap_over_texture(d2d_context, &texture, &properties).ok()?;
        Some(Self {
            render_target_view,
            bitmap,
        })
    }
}

pub struct Renderer {
    /// The mode as built, so a caller can compare against a fresh display query.
    output_mode: OutputMode,
    backbuffer_format: DXGI_FORMAT,
    luminances: DisplayLuminances,
    presenter: CompositionPresenter,
    d3d_device: ID3D11Device,
    /// Incremented per build; worker textures from other generations never wrap here.
    upload_device_generation: u64,
    /// The adapter's per-resource ceiling, fixed per device; read once at build.
    upload_maximum_frame_bytes: u64,
    d3d_context: ID3D11DeviceContext,
    /// Declared before d2d_context: effects must release while their context lives.
    mode_effects: ModeEffects,
    d2d_context: ID2D1DeviceContext,
    /// Fullscreen quantizing copy from the UNORM16 scene to the 8-bit backbuffer.
    quantize_pass: Option<QuantizePass>,
    scene_shader_resource_view: Option<ID3D11ShaderResourceView>,
    backbuffer_size: (u32, u32),
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    effect_output: Option<ID2D1Image>,
    dither_setting: DitherMode,
    image_storage: PixelStorage,
    image_source_bits_per_channel: u32,
    scrgb_color_context: ID2D1ColorContext,
    srgb_color_context: ID2D1ColorContext,
    /// ACM-off SDR destination = the display's own profile; None outside that mode.
    display_color_context: Option<ID2D1ColorContext>,
    /// Display gamut label shown as the output space when profile mapping is active.
    destination_gamut_label: Option<&'static str>,
    /// Nearest gamut label of a tagged source; None when untagged (output names the gamut only).
    source_gamut_label: Option<&'static str>,
    /// Cached backbuffer label for the information overlay, refreshed on format/mode/gamut change.
    output_label: String,
    source_icc_profile: Option<Arc<[u8]>>,
    source_color_context: Option<ID2D1ColorContext>,
    linear_source_primaries: Option<[[f32; 2]; 3]>,
    linear_source_context: Option<ID2D1ColorContext>,
    image_display_size: (f32, f32),
    image_pixel_size: (u32, u32),
    /// Set when the pump already waited for the next frame's present-queue room.
    frame_slot_held: bool,
    /// Created on the first gain map bake, so images without one allocate nothing.
    gain_pass: Option<GainMapPass>,
    gain_state: Option<GainMapState>,
    /// Display peak over current SDR white; None (failed peak query) keeps the base rendition.
    display_headroom: Option<f32>,
    /// SDR white over 80 nits, folded into the bake so its output is absolute.
    sdr_white_boost: f32,
}

/// What `render` draws the frame with, decided before the overlay reports it.
#[derive(Clone, Copy)]
pub struct FrameDecision {
    transform: Matrix3x2,
    /// The requested filter, or NEAREST where the placement resamples nothing.
    draw_interpolation: D2D1_INTERPOLATION_MODE,
    dither: DitherMode,
    /// None when the backbuffer is not one the app quantizes.
    quantization_steps: Option<u32>,
    identity_placement: bool,
}

impl FrameDecision {
    pub fn is_identity_draw(self) -> bool {
        self.identity_placement
    }

    pub fn dither_description(self) -> &'static str {
        self.dither.description()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Release the per-buffer D2D wrappers while the device is alive.
        for slot in self.presenter.buffers_mut() {
            slot.d2d_target = None;
            slot.render_target_view = None;
        }
        unsafe { self.d2d_context.SetTarget(None) };
        self.effect_output = None;
        self.image = None;
        self.gain_state = None;
        self.target = None;
        self.scene_shader_resource_view = None;
    }
}

pub struct GraphicsDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

/// D2D interop needs BGRA support on every device riv creates.
const REQUIRED_DEVICE_FLAGS: D3D11_CREATE_DEVICE_FLAG = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

const REQUIRED_FEATURE_LEVELS: [D3D_FEATURE_LEVEL; 1] = [D3D_FEATURE_LEVEL_12_0];

/// A hardware device at FL 12_0 or nothing: no software rasterizer takes its place.
pub fn create_device() -> Result<GraphicsDevice> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            REQUIRED_DEVICE_FLAGS | presentation::REQUIRED_DEVICE_FLAG,
            Some(&REQUIRED_FEATURE_LEVELS),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )?;
    }
    Ok(GraphicsDevice {
        device: device.expect("D3D11CreateDevice succeeded without device"),
        context: context.expect("D3D11CreateDevice succeeded without context"),
    })
}

/// A device being built off the UI thread, waited for where the renderer needs it.
pub struct PendingDevice(thread::JoinHandle<Result<GraphicsDevice>>);

impl PendingDevice {
    pub fn start() -> Self {
        Self(thread::spawn(create_device))
    }

    pub fn wait(self) -> Result<GraphicsDevice> {
        self.0.join().expect("device thread panicked")
    }
}

/// Every bitmap riv draws or targets is premultiplied; only the DXGI format varies.
fn premultiplied_pixel_format(format: DXGI_FORMAT) -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
}

fn source_pixel_format(storage: PixelStorage) -> D2D1_PIXEL_FORMAT {
    premultiplied_pixel_format(storage.dxgi_format())
}

/// Shared by the CPU upload and the worker-texture wrap; both must describe alike.
fn image_bitmap_properties(storage: PixelStorage) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: source_pixel_format(storage),
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        ..Default::default()
    }
}

/// Effect property payload for interface values: the raw pointer bytes.
fn interface_property_bytes<T: Interface>(interface: &T) -> [u8; size_of::<usize>()] {
    (interface.as_raw() as usize).to_ne_bytes()
}

/// ACM-off SDR maps into the display's own profile; sRGB otherwise.
fn sdr_destination<'a>(
    display_color_context: Option<&'a ID2D1ColorContext>,
    srgb_color_context: &'a ID2D1ColorContext,
) -> &'a ID2D1ColorContext {
    display_color_context.unwrap_or(srgb_color_context)
}

fn wire_color_management(
    effect: &ID2D1Effect,
    source: &ID2D1ColorContext,
    destination: &ID2D1ColorContext,
) -> Result<()> {
    unsafe {
        effect.SetValue(
            D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
            D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
            &interface_property_bytes(source),
        )?;
        effect.SetValue(
            D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
            D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
            &interface_property_bytes(destination),
        )?;
    }
    Ok(())
}

fn build_when<T>(condition: bool, build: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    condition.then(build).transpose()
}

/// A WhiteLevelAdjustment with its input at SDR reference white; the output defaults to 80.
fn create_white_level_effect(d2d_context: &ID2D1DeviceContext) -> Result<ID2D1Effect> {
    let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1WhiteLevelAdjustment) }?;
    set_white_level_input(&effect, SDR_REFERENCE_WHITE_NITS)?;
    Ok(effect)
}

/// WhiteLevelAdjustment multiplies by input/output white level.
fn set_white_level_input(effect: &ID2D1Effect, input_white_nits: f32) -> Result<()> {
    unsafe {
        effect.SetValue(
            D2D1_WHITELEVELADJUSTMENT_PROP_INPUT_WHITE_LEVEL.0 as u32,
            D2D1_PROPERTY_TYPE_FLOAT,
            &input_white_nits.to_ne_bytes(),
        )
    }
}

impl Renderer {
    fn create_color_management_effect(d2d_context: &ID2D1DeviceContext) -> Result<ID2D1Effect> {
        // BEST quality is required for float precision and scRGB conversions.
        let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1ColorManagement) }?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_QUALITY.0 as u32,
                D2D1_PROPERTY_TYPE_ENUM,
                &D2D1_COLORMANAGEMENT_QUALITY_BEST.0.to_ne_bytes(),
            )
        }?;
        // Colorimetric intent, not the perceptual default.
        for intent in [
            D2D1_COLORMANAGEMENT_PROP_SOURCE_RENDERING_INTENT,
            D2D1_COLORMANAGEMENT_PROP_DESTINATION_RENDERING_INTENT,
        ] {
            unsafe {
                effect.SetValue(
                    intent.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &D2D1_COLORMANAGEMENT_RENDERING_INTENT_RELATIVE_COLORIMETRIC
                        .0
                        .to_ne_bytes(),
                )
            }?;
        }
        Ok(effect)
    }

    /// Display-profile color context and its gamut label for ACM-off SDR; both None otherwise.
    fn display_context_and_label(
        d2d_context: &ID2D1DeviceContext,
        is_hdr_output: bool,
        is_sdr_wide_gamut: bool,
        display_profile: Option<&[u8]>,
    ) -> (Option<ID2D1ColorContext>, Option<&'static str>) {
        let Some(display_profile) =
            display_profile.filter(|_| !is_hdr_output && !is_sdr_wide_gamut)
        else {
            return (None, None);
        };
        let Ok(context) = (unsafe {
            d2d_context.CreateColorContext(D2D1_COLOR_SPACE_CUSTOM, Some(display_profile))
        }) else {
            return (None, None);
        };
        (Some(context), icc::gamut_label(display_profile))
    }

    fn create_conversion_effect(
        d2d_context: &ID2D1DeviceContext,
        source: &ID2D1ColorContext,
        destination: &ID2D1ColorContext,
    ) -> Result<ID2D1Effect> {
        let effect = Self::create_color_management_effect(d2d_context)?;
        wire_color_management(&effect, source, destination)?;
        Ok(effect)
    }

    fn create_mode_effects(
        d2d_context: &ID2D1DeviceContext,
        is_hdr_output: bool,
        is_sdr_wide_gamut: bool,
        tone_map_target_nits: f32,
        scrgb_color_context: &ID2D1ColorContext,
        sdr_destination_context: &ID2D1ColorContext,
    ) -> Result<ModeEffects> {
        let color_management = Self::create_color_management_effect(d2d_context)?;
        // SDR only: HDR displays pass content through with no tone map.
        let tone_map = build_when(!is_hdr_output, || {
            let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1HdrToneMap) }?;
            unsafe {
                effect.SetValue(
                    D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &tone_map_target_nits.to_ne_bytes(),
                )?;
                // The SDR curve mode raises midtones; always use the HDR curve.
                effect.SetValue(
                    D2D1_HDRTONEMAP_PROP_DISPLAY_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &D2D1_HDRTONEMAP_DISPLAY_MODE_HDR.0.to_ne_bytes(),
                )?;
            }
            Ok(ToneMapStage {
                tone_map: effect,
                normalize: create_white_level_effect(d2d_context)?,
                // The FP16 scRGB backbuffer of ACM-on SDR takes the tone-mapped scRGB with no re-encode.
                output_encoding: build_when(!is_sdr_wide_gamut, || {
                    Self::create_conversion_effect(
                        d2d_context,
                        scrgb_color_context,
                        sdr_destination_context,
                    )
                })?,
            })
        })?;
        let white_level = build_when(is_hdr_output, || {
            let effect = create_white_level_effect(d2d_context)?;
            unsafe {
                effect.SetValue(
                    D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &SDR_REFERENCE_WHITE_NITS.to_ne_bytes(),
                )?;
            }
            Ok(effect)
        })?;
        Ok(ModeEffects {
            color_management,
            tone_map,
            white_level,
        })
    }

    /// Dither only the 8-bit backbuffer the app quantizes; FP16 leaves quantization to DWM.
    fn backbuffer_bits_for(format: DXGI_FORMAT) -> Option<u32> {
        (format == SDR_BACKBUFFER_FORMAT).then_some(SDR_BACKBUFFER_BITS)
    }

    /// FP16 scRGB for HDR and ACM-on wide gamut, 8-bit sRGB otherwise; both paths share it.
    fn mode_format_and_color_space(
        is_hdr_output: bool,
        is_sdr_wide_gamut: bool,
    ) -> (DXGI_FORMAT, DXGI_COLOR_SPACE_TYPE) {
        if is_hdr_output || is_sdr_wide_gamut {
            (
                SCRGB_BACKBUFFER_FORMAT,
                DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
            )
        } else {
            (SDR_BACKBUFFER_FORMAT, SDR_BACKBUFFER_COLOR_SPACE)
        }
    }

    pub fn new(
        window: HWND,
        width: u32,
        height: u32,
        mode: OutputMode,
        luminances: DisplayLuminances,
        device: GraphicsDevice,
    ) -> Result<Self> {
        let is_sdr_wide_gamut = mode.is_sdr_wide_gamut();
        let is_hdr_output = mode.hdr;
        let tone_map_target_nits = luminances.target_nits;
        let GraphicsDevice {
            device: d3d_device,
            context: d3d_context,
        } = device;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        // An adapter that will not describe itself gets the floor; larger frames upload from the CPU.
        let upload_maximum_frame_bytes = maximum_resource_bytes(
            unsafe { dxgi_device.GetAdapter() }
                .and_then(|adapter| unsafe { adapter.GetDesc() })
                .map_or(0, |description| description.DedicatedVideoMemory as u64),
        );

        // D2D precedes the present target: the pass decides the scene format.
        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let d2d_context = unsafe {
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };
        // Default effect precision is the input's 8bpc UNORM, which clamps >1.0 boosts.
        unsafe {
            let mut rendering_controls = d2d_context.GetRenderingControls();
            rendering_controls.bufferPrecision = D2D1_BUFFER_PRECISION_16BPC_FLOAT;
            d2d_context.SetRenderingControls(&raw const rendering_controls);
        }
        let (backbuffer_format, color_space) =
            Self::mode_format_and_color_space(is_hdr_output, is_sdr_wide_gamut);
        // D2D draws the UNORM16 scene and the pass quantizes it; FP16 leaves that to DWM.
        let quantize_pass = (backbuffer_format != SCRGB_BACKBUFFER_FORMAT)
            .then(|| QuantizePass::new(&d3d_device))
            .transpose()?;

        let scrgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SCRGB, None) }?;
        let srgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SRGB, None) }?;
        let (display_color_context, destination_gamut_label) = Self::display_context_and_label(
            &d2d_context,
            is_hdr_output,
            is_sdr_wide_gamut,
            mode.display_profile.as_deref(),
        );

        let mut presenter = CompositionPresenter::new(&d3d_device, window)?;
        presenter.set_color_space(color_space)?;
        presenter.ensure_buffers(
            &d3d_device,
            backbuffer_format,
            (width, height),
            PRESENTATION_BUFFER_COUNT,
        )?;
        let mode_effects = Self::create_mode_effects(
            &d2d_context,
            is_hdr_output,
            is_sdr_wide_gamut,
            tone_map_target_nits,
            &scrgb_color_context,
            sdr_destination(display_color_context.as_ref(), &srgb_color_context),
        )?;
        static UPLOAD_DEVICE_GENERATIONS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let mut renderer = Self {
            output_mode: mode,
            upload_device_generation: UPLOAD_DEVICE_GENERATIONS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            upload_maximum_frame_bytes,
            backbuffer_format,
            luminances,
            presenter,
            d3d_device,
            d3d_context,
            d2d_context,
            quantize_pass,
            scene_shader_resource_view: None,
            backbuffer_size: (width, height),
            target: None,
            image: None,
            effect_output: None,
            mode_effects,
            dither_setting: DitherMode::None,
            image_storage: PixelStorage::Bgra8,
            image_source_bits_per_channel: 8,
            scrgb_color_context,
            srgb_color_context,
            display_color_context,
            destination_gamut_label,
            source_gamut_label: None,
            output_label: String::new(),
            source_icc_profile: None,
            source_color_context: None,
            linear_source_primaries: None,
            linear_source_context: None,
            image_display_size: (0.0, 0.0),
            image_pixel_size: (0, 0),
            frame_slot_held: false,
            gain_pass: None,
            gain_state: None,
            display_headroom: None,
            sdr_white_boost: 1.0,
        };
        renderer.refresh_output_label();
        renderer.create_target()?;
        Ok(renderer)
    }

    fn is_hdr_output(&self) -> bool {
        self.output_mode.hdr
    }

    /// The mode this renderer was built or reconfigured with.
    pub fn output_mode(&self) -> &OutputMode {
        &self.output_mode
    }

    /// Tone-map luminances for the information overlay: display caps and the output target.
    pub fn tone_map_luminances(&self) -> ToneMapLuminances {
        ToneMapLuminances {
            hdr_display: self.is_hdr_output(),
            display_peak_nits: self.luminances.reported_peak_nits,
            display_full_frame_nits: self.luminances.reported_full_frame_nits,
            output_target_nits: self.luminances.target_nits,
        }
    }

    /// The SDR output is advanced-color FP16 scRGB, wide gamut handed to DWM.
    fn is_sdr_wide_gamut(&self) -> bool {
        self.output_mode.is_sdr_wide_gamut()
    }

    /// The backbuffer is FP16 scRGB; app-drawn colors encode linearly for it.
    pub fn is_scrgb_output(&self) -> bool {
        self.backbuffer_format == SCRGB_BACKBUFFER_FORMAT
    }

    /// Active backbuffer, for the information overlay. ACM-off SDR names the gamut it maps into.
    pub fn output_label(&self) -> &str {
        &self.output_label
    }

    /// Recomputes the cached output label after a format, mode, or gamut change.
    fn refresh_output_label(&mut self) {
        self.output_label = if self.backbuffer_format == SCRGB_BACKBUFFER_FORMAT {
            "FP16 scRGB".to_string()
        } else {
            self.sdr_output_label()
        };
    }

    /// Linear light in the given primaries; D65, which every gamut riv can state uses.
    fn create_linear_color_context(&self, primaries: [[f32; 2]; 3]) -> Result<ID2D1ColorContext> {
        // D65 tristimulus normalized to Y = 1, the form whitePointXZ takes.
        const D65_WHITE_POINT_XZ: Vector2 = Vector2 {
            X: 0.9505,
            Y: 1.0891,
        };
        let point = |xy: [f32; 2]| Vector2 { X: xy[0], Y: xy[1] };
        let profile = D2D1_SIMPLE_COLOR_PROFILE {
            redPrimary: point(primaries[0]),
            greenPrimary: point(primaries[1]),
            bluePrimary: point(primaries[2]),
            whitePointXZ: D65_WHITE_POINT_XZ,
            gamma: D2D1_GAMMA1_G10,
        };
        let context: ID2D1DeviceContext5 = self.d2d_context.cast()?;
        Ok(unsafe { context.CreateColorContextFromSimpleColorProfile(&raw const profile) }?.into())
    }

    /// Whether this source profile, or untagged sRGB, is already the SDR destination space.
    fn is_destination_space(&self, icc_profile: Option<&[u8]>) -> bool {
        // Only a context riv managed to build is used as the destination; the rest read sRGB.
        let destination = self
            .display_color_context
            .as_ref()
            .and(self.output_mode.display_profile.as_deref());
        match (icc_profile, destination) {
            // Untagged means sRGB, so one side present means that side must be sRGB.
            (None, None) => true,
            (None, Some(profile)) | (Some(profile), None) => icc::is_srgb(profile),
            (Some(source), Some(display)) => icc::same_space(source, display),
        }
    }

    fn sdr_destination_context(&self) -> &ID2D1ColorContext {
        sdr_destination(
            self.display_color_context.as_ref(),
            &self.srgb_color_context,
        )
    }

    /// SDR output label; ACM-off profile mapping appends the destination gamut.
    fn sdr_output_label(&self) -> String {
        let destination = self.destination_gamut_label.unwrap_or("sRGB");
        match self.source_gamut_label {
            Some(source) if source != destination => {
                format!("{SDR_BACKBUFFER_BITS}-bit {source} in {destination}")
            }
            _ => format!("{SDR_BACKBUFFER_BITS}-bit {destination}"),
        }
    }

    fn target_bitmap_properties(format: DXGI_FORMAT) -> D2D1_BITMAP_PROPERTIES1 {
        D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: premultiplied_pixel_format(format),
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        }
    }

    fn bitmap_over_texture(
        d2d_context: &ID2D1DeviceContext,
        texture: &ID3D11Texture2D,
        properties: &D2D1_BITMAP_PROPERTIES1,
    ) -> Result<ID2D1Bitmap1> {
        let surface: IDXGISurface = texture.cast()?;
        unsafe {
            d2d_context.CreateBitmapFromDxgiSurface(&surface, Some(std::ptr::from_ref(properties)))
        }
    }

    /// UNORM16 scene the quantize pass reads, as the D2D target.
    fn create_scene_target(&mut self) -> Result<()> {
        let scene_texture = crate::view::texture::create_render_texture(
            &self.d3d_device,
            self.backbuffer_size,
            SCENE_TEXTURE_FORMAT,
            D3D11_RESOURCE_MISC_FLAG(0),
        )?;
        self.scene_shader_resource_view = Some(crate::view::texture::create_shader_resource_view(
            &self.d3d_device,
            &scene_texture,
        )?);
        let properties = Self::target_bitmap_properties(SCENE_TEXTURE_FORMAT);
        let target = Self::bitmap_over_texture(&self.d2d_context, &scene_texture, &properties)?;
        unsafe { self.d2d_context.SetTarget(&target) };
        self.target = Some(target);
        Ok(())
    }

    fn create_target(&mut self) -> Result<()> {
        let quantizing = self.quantize_pass.is_some();
        let format = self.backbuffer_format;
        let size = self.backbuffer_size;
        let first_target = {
            let presenter = &mut self.presenter;
            presenter.ensure_buffers(&self.d3d_device, format, size, PRESENTATION_BUFFER_COUNT)?;
            let properties = Self::target_bitmap_properties(format);
            let mut first_target = None;
            for slot in presenter.buffers_mut() {
                if quantizing {
                    // The pass writes each presentation buffer; D2D draws the shared scene.
                    slot.d2d_target = None;
                    slot.render_target_view =
                        Some(crate::view::texture::create_render_target_view(
                            &self.d3d_device,
                            &slot.texture,
                        )?);
                } else {
                    // D2D draws each presentation buffer directly; render retargets per frame.
                    slot.render_target_view = None;
                    let target =
                        Self::bitmap_over_texture(&self.d2d_context, &slot.texture, &properties)?;
                    if first_target.is_none() {
                        first_target = Some(target.clone());
                    }
                    slot.d2d_target = Some(target);
                }
            }
            first_target
        };
        if quantizing {
            self.create_scene_target()
        } else {
            let target = first_target.expect("the ring holds at least one buffer");
            unsafe { self.d2d_context.SetTarget(&target) };
            self.target = Some(target);
            Ok(())
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe { self.d2d_context.SetTarget(None) };
        self.target = None;
        self.scene_shader_resource_view = None;
        self.backbuffer_size = (width, height);
        self.create_target()
    }

    /// Switches the output mode in place; the window keeps its presentation surface.
    pub fn reconfigure_output(
        &mut self,
        mode: OutputMode,
        luminances: DisplayLuminances,
    ) -> Result<()> {
        let is_sdr_wide_gamut = mode.is_sdr_wide_gamut();
        let is_hdr_output = mode.hdr;
        // Adopt the target state first so a partial failure cannot retry every WM_MOVE.
        self.output_mode = mode;
        (self.display_color_context, self.destination_gamut_label) =
            Self::display_context_and_label(
                &self.d2d_context,
                is_hdr_output,
                is_sdr_wide_gamut,
                self.output_mode.display_profile.as_deref(),
            );
        self.luminances = luminances;

        // Release every buffer reference before the ring is retargeted.
        unsafe { self.d2d_context.SetTarget(None) };
        self.target = None;
        self.effect_output = None;
        self.scene_shader_resource_view = None;

        let (format, color_space) =
            Self::mode_format_and_color_space(is_hdr_output, is_sdr_wide_gamut);
        if format == SCRGB_BACKBUFFER_FORMAT {
            // FP16 leaves quantization to DWM.
            self.quantize_pass = None;
        } else if self.quantize_pass.is_none() {
            // The pass depends only on the (unchanged) device, so keep it across reconfigures.
            self.quantize_pass = Some(QuantizePass::new(&self.d3d_device)?);
        }
        self.presenter.set_color_space(color_space)?;

        let mode_effects = Self::create_mode_effects(
            &self.d2d_context,
            is_hdr_output,
            is_sdr_wide_gamut,
            luminances.target_nits,
            &self.scrgb_color_context,
            self.sdr_destination_context(),
        )?;
        self.mode_effects = mode_effects;
        self.backbuffer_format = format;
        self.refresh_output_label();
        self.create_target()
    }

    pub fn set_sdr_white_boost(&mut self, boost: f32) -> Result<()> {
        self.sdr_white_boost = boost;
        if let Some(effect) = &self.mode_effects.white_level {
            set_white_level_input(effect, SDR_REFERENCE_WHITE_NITS * self.sdr_white_boost)?;
        }
        Ok(())
    }

    /// Display peak over current SDR white; None (failed peak query) keeps the base rendition.
    pub fn set_display_headroom(&mut self, headroom: Option<f32>) {
        self.display_headroom = headroom;
    }

    /// True when the gain-applied rendition is the one on screen.
    pub fn ultra_hdr_applied(&self) -> bool {
        self.gain_state
            .as_ref()
            .is_some_and(|state| state.adopted_conditions.is_some())
    }

    /// Bakes or retires the gain rendition when its inputs moved; part of each frame decision.
    fn refresh_gain_bake(&mut self) -> Result<()> {
        let Some(mut state) = self.gain_state.take() else {
            return Ok(());
        };
        let conditions =
            self.display_headroom
                .filter(|_| self.is_hdr_output())
                .map(|display_headroom| BakeConditions {
                    display_headroom,
                    sdr_white_boost: self.sdr_white_boost,
                });
        if state.adopted_conditions == conditions {
            self.gain_state = Some(state);
            return Ok(());
        }
        match conditions {
            Some(conditions) => {
                if let Some(bitmap) = self.bake_gain_map(&mut state, conditions.display_headroom) {
                    let wiring = Self::baked_wiring(&state);
                    self.adopt_image_bitmap(bitmap, &wiring)?;
                    state.adopted_conditions = Some(conditions);
                } else {
                    // The bake fell through; show the base and stop retrying this image.
                    if state.adopted_conditions.is_some() {
                        self.adopt_image_bitmap(state.base_bitmap.clone(), &state.base_image)?;
                    }
                    return Ok(());
                }
            }
            None => {
                self.adopt_image_bitmap(state.base_bitmap.clone(), &state.base_image)?;
                state.adopted_conditions = None;
            }
        }
        self.gain_state = Some(state);
        Ok(())
    }

    fn bake_gain_map(
        &mut self,
        state: &mut GainMapState,
        display_headroom: f32,
    ) -> Option<ID2D1Bitmap1> {
        self.ensure_gain_pass();
        let pass = self.gain_pass.as_ref()?;
        let size = (state.base_image.pixel_width, state.base_image.pixel_height);
        let baked = match &mut state.baked {
            Some(baked) => baked,
            empty @ None => empty.insert(BakedGainMap::create(
                &self.d3d_device,
                &self.d2d_context,
                size,
            )?),
        };
        let weight = state.metadata.weight(display_headroom);
        pass.bake(
            &self.d3d_context,
            crate::view::gain::BakeInputs {
                base: &state.base_view,
                gain_map: &state.gain_map_view,
                target: &baked.render_target_view,
                target_size: size,
                metadata: &state.metadata,
                weight,
                sdr_white_boost: self.sdr_white_boost,
            },
        )
        .ok()?;
        Some(baked.bitmap.clone())
    }

    /// The gain pass is built on first use; a failed build leaves the base rendition.
    fn ensure_gain_pass(&mut self) {
        if self.gain_pass.is_none() {
            self.gain_pass = GainMapPass::new(&self.d3d_device).ok();
        }
    }

    /// The baked rendition wires like a linear FP16 source in the base's primaries.
    fn baked_wiring(state: &GainMapState) -> DecodedImage {
        // A whole image for nine fields: adopt_image_bitmap takes the same type from the display path.
        let mut wiring = state.base_image.without_pixels();
        wiring.storage = PixelStorage::RgbaHalf;
        wiring.source_primaries = state.base_primaries;
        wiring.icc_profile = None;
        wiring.peak_luminance_nits = Some(state.metadata.capacity_peak_nits());
        // Half floats carry full precision; the base's stored depth no longer applies.
        wiring.source_bits_per_channel = 16;
        wiring
    }

    /// True when the stored display luminances changed (overlay, next rewire).
    pub fn set_tone_map_target(&mut self, luminances: DisplayLuminances) -> bool {
        if self.luminances == luminances {
            return false;
        }
        self.luminances = luminances;
        true
    }

    /// A matching worker texture wraps without an upload; anything else re-uploads.
    pub fn set_image(
        &mut self,
        frame_pixels: &[u8],
        texture: Option<&UploadedTexture>,
        image: &DecodedImage,
    ) -> Result<()> {
        self.gain_state = None;
        if let Some(uploaded) = texture
            && uploaded.generation == self.upload_device_generation
            && self.wrap_uploaded_texture(uploaded, image).is_ok()
        {
            return Ok(());
        }
        if frame_pixels.len() != image.frame_byte_length() {
            // A slimmed image has no pixels to upload; the caller recovers elsewhere.
            return Err(windows::core::Error::empty());
        }
        // The first display can arrive before any worker upload device; upload here.
        if image.gain_map.is_some()
            && let Some(uploaded) = upload_still_texture(&self.upload_device(), image)
            && self.wrap_uploaded_texture(&uploaded, image).is_ok()
        {
            return Ok(());
        }
        let properties = image_bitmap_properties(image.storage);
        let bitmap = unsafe {
            self.d2d_context.CreateBitmap(
                D2D_SIZE_U {
                    width: image.pixel_width,
                    height: image.pixel_height,
                },
                Some(frame_pixels.as_ptr().cast()),
                image.row_pitch(),
                &raw const properties,
            )?
        };
        self.adopt_image_bitmap(bitmap, image)?;
        Ok(())
    }

    fn wrap_uploaded_texture(
        &mut self,
        uploaded: &UploadedTexture,
        image: &DecodedImage,
    ) -> Result<()> {
        let properties = image_bitmap_properties(image.storage);
        let bitmap = Self::bitmap_over_texture(&self.d2d_context, &uploaded.texture, &properties)?;
        self.gain_state = self.create_gain_state(uploaded, image, bitmap.clone());
        self.adopt_image_bitmap(bitmap, image)?;
        Ok(())
    }

    /// Gain inputs for an Ultra HDR image whose base and gain map both have textures.
    fn create_gain_state(
        &self,
        uploaded: &UploadedTexture,
        image: &DecodedImage,
        base_bitmap: ID2D1Bitmap1,
    ) -> Option<GainMapState> {
        let metadata = image.gain_map?;
        let gain_map_texture = uploaded.gain_map.as_ref()?;
        let base_view =
            crate::view::texture::create_shader_resource_view(&self.d3d_device, &uploaded.texture)
                .ok()?;
        let gain_map_view =
            crate::view::texture::create_shader_resource_view(&self.d3d_device, gain_map_texture)
                .ok()?;
        Some(GainMapState {
            metadata,
            base_bitmap,
            base_image: image.without_pixels(),
            base_primaries: image.icc_profile.as_deref().and_then(icc::primaries),
            base_view,
            gain_map_view,
            baked: None,
            adopted_conditions: None,
        })
    }

    /// Copies a worker texture back to CPU pixels; only this build's textures qualify.
    pub fn read_back_texture(
        &self,
        uploaded: &UploadedTexture,
        image: &DecodedImage,
    ) -> Result<Vec<u8>> {
        if uploaded.generation != self.upload_device_generation {
            return Err(windows::core::Error::empty());
        }
        let description = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..image.texture_description()
        };
        let mut staging = None;
        unsafe {
            self.d3d_device
                .CreateTexture2D(&raw const description, None, Some(&raw mut staging))?
        };
        let staging = staging.expect("CreateTexture2D succeeded without texture");
        let pitch = image.row_pitch() as usize;
        let mut pixels =
            try_zeroed_buffer(image.frame_byte_length()).ok_or_else(windows::core::Error::empty)?;
        unsafe {
            self.d3d_context.CopyResource(&staging, &uploaded.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.d3d_context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))?;
            let source = mapped.pData.cast::<u8>();
            for (row_index, row) in pixels.chunks_exact_mut(pitch).enumerate() {
                let offset = row_index * mapped.RowPitch as usize;
                row.copy_from_slice(std::slice::from_raw_parts(source.add(offset), pitch));
            }
            self.d3d_context.Unmap(&staging, 0);
        }
        Ok(pixels)
    }

    /// What the workers upload with; the generation ties their textures to this build.
    pub fn upload_device(&self) -> UploadDevice {
        UploadDevice {
            device: self.d3d_device.clone(),
            generation: self.upload_device_generation,
            maximum_frame_bytes: self.upload_maximum_frame_bytes,
        }
    }

    fn adopt_image_bitmap(&mut self, bitmap: ID2D1Bitmap1, image: &DecodedImage) -> Result<()> {
        self.image_display_size = (image.width as f32, image.height as f32);
        self.image_pixel_size = (image.pixel_width, image.pixel_height);
        self.image_storage = image.storage;
        self.image_source_bits_per_channel = image.source_bits_per_channel;
        self.rewire_effect_chain(
            &bitmap,
            image.icc_profile.as_ref(),
            image.storage,
            image.peak_luminance_nits,
            image.source_primaries,
        )?;
        self.image = Some(bitmap);
        Ok(())
    }

    pub fn set_dither_setting(&mut self, mode: DitherMode) {
        self.dither_setting = mode;
    }

    /// Reuses the current bitmap wiring; callers fall back to set_image when there is none.
    pub fn update_frame_pixels(&mut self, pixels: &[u8]) -> Result<()> {
        let Some(bitmap) = &self.image else {
            return Err(windows::core::Error::empty());
        };
        let pitch = self.image_pixel_size.0 * self.image_storage.bytes_per_pixel();
        // CopyFromMemory reads the whole bitmap, so a short frame buffer must not reach it.
        if pixels.len() != pitch as usize * self.image_pixel_size.1 as usize {
            return Err(windows::core::Error::empty());
        }
        unsafe { bitmap.CopyFromMemory(None, pixels.as_ptr().cast(), pitch) }?;
        Ok(())
    }

    fn rewire_effect_chain(
        &mut self,
        bitmap: &ID2D1Bitmap1,
        icc_profile: Option<&Arc<[u8]>>,
        storage: PixelStorage,
        peak_luminance_nits: Option<f32>,
        source_primaries: Option<[[f32; 2]; 3]>,
    ) -> Result<()> {
        let icc_bytes = icc_profile.map(|profile| &**profile);
        self.effect_output = None;
        self.note_source_gamut(source_primaries, icc_bytes);
        // Unwire the previous bitmap now, so a failure return does not keep it alive.
        unsafe { self.mode_effects.color_management.SetInput(0, None, true) };
        // HDR passes through; SDR maps content above SDR white to the target.
        let hdr_content = peak_luminance_nits.is_some_and(|peak| peak > SDR_REFERENCE_WHITE_NITS);
        let tone_map_peak = peak_luminance_nits
            .filter(|_| hdr_content)
            .filter(|_| self.mode_effects.tone_map.is_some());
        let scrgb_destination =
            self.is_hdr_output() || self.is_sdr_wide_gamut() || tone_map_peak.is_some();
        // A source already in the destination space skips CM; the conversion would change nothing.
        if storage == PixelStorage::Bgra8
            && !scrgb_destination
            && self.is_destination_space(icc_bytes)
        {
            return Ok(());
        }
        let destination_context = if scrgb_destination {
            self.scrgb_color_context.clone()
        } else {
            sdr_destination(
                self.display_color_context.as_ref(),
                &self.srgb_color_context,
            )
            .clone()
        };
        let source_context = self.resolve_source_context(storage, source_primaries, icc_profile)?;
        let color_management = &self.mode_effects.color_management;
        wire_color_management(color_management, &source_context, &destination_context)?;
        unsafe { color_management.SetInput(0, bitmap, true) };
        let converted = unsafe { color_management.GetOutput() }?;
        self.effect_output = Some(self.wire_scene(converted, tone_map_peak, hdr_content)?);
        Ok(())
    }

    /// The gamut the information panel names for the source: its primaries, else its profile.
    fn note_source_gamut(
        &mut self,
        source_primaries: Option<[[f32; 2]; 3]>,
        icc_bytes: Option<&[u8]>,
    ) {
        self.source_gamut_label = source_primaries
            .map(nearest_gamut_label)
            .or_else(|| icc_bytes.and_then(icc::gamut_label));
        self.refresh_output_label();
    }

    /// The color context the source pixels are in; cached per primaries and per ICC profile.
    fn resolve_source_context(
        &mut self,
        storage: PixelStorage,
        source_primaries: Option<[[f32; 2]; 3]>,
        icc_profile: Option<&Arc<[u8]>>,
    ) -> Result<ID2D1ColorContext> {
        let icc_bytes = icc_profile.map(|profile| &**profile);
        // FP16 pixels are linear light in the stated primaries; scRGB covers unknown ones.
        match storage {
            PixelStorage::RgbaHalf => {
                // A source that states nothing leaves the cached context alone.
                if let Some(primaries) = source_primaries
                    && self.linear_source_primaries != Some(primaries)
                {
                    self.linear_source_context = Some(self.create_linear_color_context(primaries)?);
                    self.linear_source_primaries = Some(primaries);
                }
                Ok(source_primaries
                    .and(self.linear_source_context.as_ref())
                    .unwrap_or(&self.scrgb_color_context)
                    .clone())
            }
            PixelStorage::Bgra8 => {
                if self.source_icc_profile.as_deref() != icc_bytes {
                    self.source_color_context = None;
                    self.source_icc_profile = icc_profile.cloned();
                }
                let d2d_context = &self.d2d_context;
                let srgb_color_context = &self.srgb_color_context;
                let context = self
                    .source_color_context
                    .get_or_insert_with(|| {
                        // A profile D2D rejects reads as untagged: sRGB.
                        icc_bytes
                            .and_then(|icc_profile| {
                                unsafe {
                                    d2d_context.CreateColorContext(
                                        D2D1_COLOR_SPACE_CUSTOM,
                                        Some(icc_profile),
                                    )
                                }
                                .ok()
                            })
                            .unwrap_or_else(|| srgb_color_context.clone())
                    })
                    .clone();
                Ok(context)
            }
        }
    }

    /// The scene after color management: tone mapped for SDR output, boosted, or passed through.
    fn wire_scene(
        &self,
        converted: ID2D1Image,
        tone_map_peak: Option<f32>,
        hdr_content: bool,
    ) -> Result<ID2D1Image> {
        let tone_map = self.mode_effects.tone_map.as_ref().zip(tone_map_peak);
        let scene = match tone_map {
            Some((stage, peak)) => {
                // Very low input maxima misbehave; floor at the SDR reference white.
                let input_maximum = peak.max(SDR_REFERENCE_WHITE_NITS);
                unsafe {
                    stage.tone_map.SetValue(
                        D2D1_HDRTONEMAP_PROP_INPUT_MAX_LUMINANCE.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &input_maximum.to_ne_bytes(),
                    )?;
                    stage.tone_map.SetValue(
                        D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &self.luminances.target_nits.to_ne_bytes(),
                    )?;
                    stage.tone_map.SetInput(0, &converted, true);
                }
                let tone_mapped = unsafe { stage.tone_map.GetOutput() }?;
                // Reinterpret scene-referred white as display-referred, then re-encode to sRGB.
                let display_white = self.luminances.target_nits.min(input_maximum);
                unsafe {
                    stage.normalize.SetValue(
                        D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &display_white.to_ne_bytes(),
                    )?;
                    stage.normalize.SetInput(0, &tone_mapped, true);
                }
                let normalized = unsafe { stage.normalize.GetOutput() }?;
                match &stage.output_encoding {
                    None => normalized,
                    Some(output_encoding) => {
                        unsafe { output_encoding.SetInput(0, &normalized, true) };
                        unsafe { output_encoding.GetOutput() }?
                    }
                }
            }
            None => match &self.mode_effects.white_level {
                // SDR content takes the white-level boost; HDR content passes through.
                Some(white_level) if !hdr_content => {
                    unsafe { white_level.SetInput(0, &converted, true) };
                    unsafe { white_level.GetOutput() }?
                }
                _ => converted,
            },
        };
        Ok(scene)
    }

    pub fn clear_image(&mut self) {
        self.gain_state = None;
        self.image = None;
        self.effect_output = None;
        // Unwire the previous bitmap so the effect does not keep it alive.
        unsafe { self.mode_effects.color_management.SetInput(0, None, true) };
    }

    /// Decides the frame's placement and quantization; the information panel reads it.
    pub fn decide_frame(
        &mut self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
    ) -> Result<FrameDecision> {
        // The gain rendition settles first, so the decision and the panel see it.
        self.refresh_gain_bake()?;
        let transform = self.frame_transform(matrix);
        let identity_placement = self.placement_is_identity(matrix, &transform);
        // Force NEAREST so a 1:1 placement stays pixel-exact, whatever the filter.
        let draw_interpolation = if identity_placement {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            interpolation
        };
        let (quantization_steps, dither) = self.quantization_policy(draw_interpolation);
        Ok(FrameDecision {
            transform,
            draw_interpolation,
            dither,
            quantization_steps,
            identity_placement,
        })
    }

    /// The view matrix with the display scale folded in: DrawImage has no destination rect.
    fn frame_transform(&self, matrix: [f32; 6]) -> Matrix3x2 {
        let scale_x = self.image_display_size.0 / (self.image_pixel_size.0.max(1) as f32);
        let scale_y = self.image_display_size.1 / (self.image_pixel_size.1.max(1) as f32);
        Matrix3x2 {
            M11: matrix[0] * scale_x,
            M12: matrix[1] * scale_x,
            M21: matrix[2] * scale_y,
            M22: matrix[3] * scale_y,
            M31: matrix[4],
            M32: matrix[5],
        }
    }

    /// A 1:1 placement on whole pixels, with a 90/270 rotation folded onto the axes.
    fn placement_is_identity(&self, matrix: [f32; 6], transform: &Matrix3x2) -> bool {
        if matrix[1] == 0.0 && matrix[2] == 0.0 {
            Self::is_pixel_identity(transform.M11, transform.M22, transform.M31, transform.M32)
        } else if matrix[0] == 0.0 && matrix[3] == 0.0 {
            let source_height = self.image_pixel_size.1 as f32;
            Self::is_pixel_identity(
                -transform.M21,
                transform.M12,
                source_height * transform.M21 + transform.M31,
                transform.M32,
            )
        } else {
            false
        }
    }

    /// The quantization steps of the back buffer, when a quantize pass exists, and the dither.
    fn quantization_policy(
        &self,
        draw_interpolation: D2D1_INTERPOLATION_MODE,
    ) -> (Option<u32>, DitherMode) {
        let backbuffer_bits = Self::backbuffer_bits_for(self.backbuffer_format);
        let quantization_steps = backbuffer_bits.map(|bits| (1 << bits) - 1);
        let dither = match backbuffer_bits {
            Some(bits) if self.image.is_some() => self.active_dither_mode(draw_interpolation, bits),
            _ => DitherMode::None,
        };
        (quantization_steps, dither)
    }

    /// What the pump waits on; None when the slot is already held or no buffer exists yet.
    pub fn pending_frame_slot(&self) -> Option<HANDLE> {
        if self.frame_slot_held {
            return None;
        }
        self.presenter.next_available_event()
    }

    pub fn hold_frame_slot(&mut self) {
        self.frame_slot_held = true;
    }

    /// Waits for present-queue room unless the pump already did.
    fn consume_frame_slot(&mut self) {
        if let Some(handle) = self.pending_frame_slot() {
            let _ =
                unsafe { WaitForSingleObjectEx(handle, FRAME_SLOT_TIMEOUT_MILLISECONDS, false) };
        }
        self.frame_slot_held = false;
    }

    pub fn render(
        &mut self,
        decision: FrameDecision,
        clear_color: D2D1_COLOR_F,
        draw_overlay: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        // The composition system dropped the manager; the caller rebuilds.
        if self.presenter.is_lost() {
            return Err(windows::core::Error::empty());
        }
        self.consume_frame_slot();
        // Without the pass, D2D draws into this frame's own buffer.
        if self.quantize_pass.is_none()
            && let Some(target) = self
                .presenter
                .next_slot()
                .and_then(|slot| slot.d2d_target.as_ref())
        {
            unsafe { self.d2d_context.SetTarget(target) };
        }
        unsafe {
            self.d2d_context.BeginDraw();
            self.d2d_context.Clear(Some(&raw const clear_color));
            if let Some(image) = &self.image {
                self.d2d_context.SetTransform(&raw const decision.transform);
                match &self.effect_output {
                    Some(output) => self.d2d_context.DrawImage(
                        output,
                        None,
                        None,
                        decision.draw_interpolation,
                        D2D1_COMPOSITE_MODE_SOURCE_OVER,
                    ),
                    // Untouched pixels, or no effect support.
                    None => {
                        let destination = D2D_RECT_F {
                            left: 0.0,
                            top: 0.0,
                            right: self.image_pixel_size.0 as f32,
                            bottom: self.image_pixel_size.1 as f32,
                        };
                        self.d2d_context.DrawBitmap(
                            image,
                            Some(&raw const destination),
                            1.0,
                            decision.draw_interpolation,
                            None,
                            None,
                        );
                    }
                }
                self.d2d_context.SetTransform(&Matrix3x2::identity());
            }
        }
        // Overlay failure must not block presenting the frame.
        let overlay_result = draw_overlay(&self.d2d_context);
        unsafe { self.d2d_context.EndDraw(None, None) }?;
        let quantize_target = self
            .presenter
            .next_slot()
            .and_then(|slot| slot.render_target_view.as_ref());
        if let (Some(quantize_pass), Some(quantization_steps), Some(scene), Some(backbuffer)) = (
            &self.quantize_pass,
            decision.quantization_steps,
            &self.scene_shader_resource_view,
            quantize_target,
        ) {
            quantize_pass.draw(
                &self.d3d_context,
                scene,
                backbuffer,
                self.backbuffer_size,
                decision.dither,
                quantization_steps,
            );
        }
        self.presenter.present_next(&self.d3d_context)?;
        overlay_result
    }

    /// A whole-pixel 1:1 placement (unit scale, integer offset) that resamples nothing.
    fn is_pixel_identity(scale_x: f32, scale_y: f32, offset_x: f32, offset_y: f32) -> bool {
        is_unit_scale(scale_x.abs())
            && is_unit_scale(scale_y.abs())
            && (offset_x - offset_x.round()).abs() < 1e-4
            && (offset_y - offset_y.round()).abs() < 1e-4
    }

    /// The frame's output dither; a draw that makes no new values has nothing to dither.
    fn active_dither_mode(
        &self,
        draw_interpolation: D2D1_INTERPOLATION_MODE,
        backbuffer_bits: u32,
    ) -> DitherMode {
        // Nearest copies source texels, so it makes no value the source did not already hold.
        let resamples = draw_interpolation != D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR;
        let source_bits = self.image_source_bits_per_channel;
        // Pass-through is exact at equal depth; a color transform can band there.
        let within_depth = if self.effect_output.is_none() {
            source_bits <= backbuffer_bits
        } else {
            source_bits < backbuffer_bits
        };
        if !resamples && within_depth {
            return DitherMode::None;
        }
        self.dither_setting
    }
}
