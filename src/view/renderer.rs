//! D3D11 + DXGI flip swapchain + D2D draw path; the swapchain format matches the monitor mode.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
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
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
    D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT, D2D1_SIMPLE_COLOR_PROFILE,
    D2D1_WHITELEVELADJUSTMENT_PROP_INPUT_WHITE_LEVEL,
    D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL, D2D1CreateFactory, ID2D1Bitmap1,
    ID2D1ColorContext, ID2D1DeviceContext, ID2D1DeviceContext5, ID2D1Effect, ID2D1Factory1,
    ID2D1Image,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_12_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
    IDXGISwapChain2, IDXGISwapChain3,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::{Interface, Result};
use windows_numerics::{Matrix3x2, Vector2};

use std::sync::Arc;
use std::thread;

use crate::image::color::{SDR_REFERENCE_WHITE_NITS, nearest_gamut_label};
use crate::image::decode::{
    DecodedImage, PixelStorage, UploadDevice, UploadedTexture, icc_gamut_label, icc_is_srgb,
    icc_same_space, maximum_resource_bytes,
};
use crate::view::dither::DitherMode;
use crate::view::quantize::QuantizePass;

/// Creation and every ResizeBuffers must pass the same swap-chain flags.
const SWAP_CHAIN_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

struct ModeEffects {
    color_management_effect: Option<ID2D1Effect>,
    hdr_tone_map_effect: Option<ID2D1Effect>,
    tone_map_normalize_effect: Option<ID2D1Effect>,
    output_color_management_effect: Option<ID2D1Effect>,
    white_level_effect: Option<ID2D1Effect>,
}

/// Display luminances: the tone-map peak, and the full-frame limit shown in the overlay.
#[derive(Clone, Copy)]
struct ToneMapTarget {
    peak_nits: f32,
    full_frame_nits: f32,
}

/// The output-affecting display state: HDR, wire depth, and advanced-color.
#[derive(Clone, PartialEq)]
pub struct OutputMode {
    pub hdr: bool,
    pub bits_per_color: u32,
    pub advanced_color: bool,
    /// The display's ICC profile, the ACM-off SDR destination; None when unavailable.
    pub display_profile: Option<Arc<Vec<u8>>>,
}

impl OutputMode {
    /// True for SDR output with advanced color on (the wide-gamut path).
    pub fn sdr_wide_gamut(&self) -> bool {
        !self.hdr && self.advanced_color
    }
}

/// Tone-map luminances for the info overlay (nits).
#[derive(Clone, Copy, PartialEq)]
pub struct ToneMapInfo {
    pub hdr_display: bool,
    pub display_peak_nits: f32,
    pub display_full_frame_nits: f32,
    pub output_target_nits: f32,
}

pub struct Renderer {
    /// The mode as built, so a caller can compare against a fresh display query.
    output_mode: OutputMode,
    hdr_mode: bool,
    /// Advanced-color SDR: FP16 scRGB output so DWM color-manages the wide gamut.
    sdr_wide_gamut: bool,
    bits_per_color: u32,
    swap_chain_format: DXGI_FORMAT,
    tone_map_target_nits: f32,
    /// Display's sustained full-frame luminance, shown in the overlay diagnostics.
    display_full_frame_nits: f32,
    swap_chain: IDXGISwapChain1,
    /// Signals when the present queue (depth 1) has room; a render waits here first.
    frame_latency_waitable: Option<HANDLE>,
    d3d_device: ID3D11Device,
    /// Minted per build; worker textures from other generations never wrap here.
    upload_device_generation: u64,
    /// The adapter's per-resource ceiling, fixed per device; read once at build.
    upload_maximum_frame_bytes: u64,
    d3d_context: ID3D11DeviceContext,
    d2d_context: ID2D1DeviceContext,
    /// Fullscreen quantizing copy for the 10-bit backbuffers D2D cannot target.
    quantize_pass: Option<QuantizePass>,
    scene_shader_resource_view: Option<ID3D11ShaderResourceView>,
    backbuffer_render_target_view: Option<ID3D11RenderTargetView>,
    backbuffer_size: (u32, u32),
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    effect_output: Option<ID2D1Image>,
    color_management_effect: Option<ID2D1Effect>,
    white_level_effect: Option<ID2D1Effect>,
    hdr_tone_map_effect: Option<ID2D1Effect>,
    tone_map_normalize_effect: Option<ID2D1Effect>,
    output_color_management_effect: Option<ID2D1Effect>,
    /// scRGB -> PQ BT.2020 for the HDR10 backbuffer; None on the FP16 fallback.
    hdr_output_color_management_effect: Option<ID2D1Effect>,
    pq_color_context: Option<ID2D1ColorContext>,
    dither_setting: DitherMode,
    image_storage: PixelStorage,
    image_source_bits_per_channel: u32,
    scrgb_color_context: Option<ID2D1ColorContext>,
    srgb_color_context: Option<ID2D1ColorContext>,
    /// ACM-off SDR destination = the display's own profile; None outside that mode.
    display_color_context: Option<ID2D1ColorContext>,
    /// Display gamut label shown as the output space when profile mapping is active.
    destination_gamut_label: Option<&'static str>,
    /// Nearest gamut label of a tagged source; None when untagged (output names the gamut only).
    source_gamut_label: Option<&'static str>,
    /// Cached backbuffer label for the info overlay, refreshed on format/mode/gamut change.
    output_description: String,
    source_icc_profile: Option<Vec<u8>>,
    source_color_context: Option<ID2D1ColorContext>,
    linear_source_primaries: Option<[[f32; 2]; 3]>,
    linear_source_context: Option<ID2D1ColorContext>,
    image_display_size: (f32, f32),
    image_pixel_size: (f32, f32),
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
    /// Whether the frame draws at a 1:1 placement, for the info panel.
    pub fn is_identity_draw(self) -> bool {
        self.identity_placement
    }

    /// What dithering the frame gets, for the info panel.
    pub fn dither_description(self) -> &'static str {
        self.dither.description()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(handle) = self.frame_latency_waitable.take() {
            let _ = unsafe { CloseHandle(handle) };
        }
        unsafe { self.d2d_context.SetTarget(None) };
        self.effect_output = None;
        self.image = None;
        self.target = None;
        self.scene_shader_resource_view = None;
        self.backbuffer_render_target_view = None;
        self.color_management_effect = None;
        self.white_level_effect = None;
        self.hdr_tone_map_effect = None;
        self.tone_map_normalize_effect = None;
        self.output_color_management_effect = None;
        self.hdr_output_color_management_effect = None;
    }
}

/// A D3D11 device with its immediate context.
#[derive(Clone)]
pub struct GraphicsDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

/// Hardware when available, WARP otherwise.
pub fn create_device() -> Result<GraphicsDevice> {
    // D3D11 WARP is documented only through 11_1; shader model 5.0 needs no more.
    create_d3d_device(D3D_DRIVER_TYPE_HARDWARE, &[D3D_FEATURE_LEVEL_12_0])
        .or_else(|_| create_d3d_device(D3D_DRIVER_TYPE_WARP, &[D3D_FEATURE_LEVEL_11_0]))
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

fn create_d3d_device(
    driver_type: D3D_DRIVER_TYPE,
    feature_levels: &[D3D_FEATURE_LEVEL],
) -> Result<GraphicsDevice> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(feature_levels),
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

/// Declares only with reported PRESENT support; an undeclared surface stays sRGB.
fn declare_color_space(
    swap_chain: &IDXGISwapChain3,
    color_space: DXGI_COLOR_SPACE_TYPE,
) -> Result<()> {
    let support = unsafe { swap_chain.CheckColorSpaceSupport(color_space) }?;
    if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 == 0 {
        return Err(windows::core::Error::empty());
    }
    unsafe { swap_chain.SetColorSpace1(color_space) }
}

fn source_pixel_format(storage: PixelStorage) -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: storage.dxgi_format(),
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
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
    srgb_color_context: Option<&'a ID2D1ColorContext>,
) -> Option<&'a ID2D1ColorContext> {
    display_color_context.or(srgb_color_context)
}

/// Points a ColorManagement effect at its source and destination color contexts.
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
    pub fn new(
        window: HWND,
        width: u32,
        height: u32,
        mode: OutputMode,
        tone_map_target_nits: f32,
        full_frame_nits: f32,
        device: GraphicsDevice,
    ) -> Result<Self> {
        let target = ToneMapTarget {
            peak_nits: tone_map_target_nits,
            full_frame_nits,
        };
        // A deep-color failure downgrades to the proven formats, never blocks launch.
        Self::build(window, width, height, mode.clone(), target, true, &device).or_else(|_| {
            // A fresh device for the retry: DXGI allows one flip swapchain per window.
            Self::build(
                window,
                width,
                height,
                mode,
                target,
                false,
                &create_device()?,
            )
        })
    }

    fn create_color_management_effect(d2d_context: &ID2D1DeviceContext) -> Option<ID2D1Effect> {
        // BEST quality is required for float precision and scRGB conversions.
        let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1ColorManagement) }.ok()?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_QUALITY.0 as u32,
                D2D1_PROPERTY_TYPE_ENUM,
                &D2D1_COLORMANAGEMENT_QUALITY_BEST.0.to_ne_bytes(),
            )
        }
        .ok()?;
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
            }
            .ok()?;
        }
        Some(effect)
    }

    /// The HDR10 backbuffer's own color space, as a context sources can convert into.
    fn create_pq_color_context(d2d_context: &ID2D1DeviceContext) -> Option<ID2D1ColorContext> {
        unsafe {
            d2d_context
                .cast::<ID2D1DeviceContext5>()
                .ok()?
                .CreateColorContextFromDxgiColorSpace(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
        }
        .ok()
        .map(Into::into)
    }

    /// Display-profile color context and its gamut label for ACM-off SDR; both None otherwise.
    fn display_context_and_label(
        d2d_context: &ID2D1DeviceContext,
        hdr_mode: bool,
        sdr_wide_gamut: bool,
        display_profile: Option<&Arc<Vec<u8>>>,
    ) -> (Option<ID2D1ColorContext>, Option<&'static str>) {
        let Some(display_profile) = display_profile.filter(|_| !hdr_mode && !sdr_wide_gamut) else {
            return (None, None);
        };
        let Ok(context) = (unsafe {
            d2d_context.CreateColorContext(D2D1_COLOR_SPACE_CUSTOM, Some(display_profile))
        }) else {
            return (None, None);
        };
        (Some(context), icc_gamut_label(display_profile))
    }

    fn create_conversion_effect(
        d2d_context: &ID2D1DeviceContext,
        source: Option<&ID2D1ColorContext>,
        destination: Option<&ID2D1ColorContext>,
    ) -> Option<ID2D1Effect> {
        let (source, destination) = (source?, destination?);
        let effect = Self::create_color_management_effect(d2d_context)?;
        wire_color_management(&effect, source, destination).ok()?;
        Some(effect)
    }

    fn create_mode_effects(
        d2d_context: &ID2D1DeviceContext,
        hdr_mode: bool,
        sdr_wide_gamut: bool,
        tone_map_target_nits: f32,
        scrgb_color_context: Option<&ID2D1ColorContext>,
        sdr_destination_context: Option<&ID2D1ColorContext>,
    ) -> ModeEffects {
        let color_management_effect = Self::create_color_management_effect(d2d_context);
        // SDR only: HDR displays pass content through with no tone map.
        let hdr_tone_map_effect = (!hdr_mode)
            .then_some(())
            .and(color_management_effect.as_ref())
            .and_then(|_| {
                let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1HdrToneMap) }.ok()?;
                unsafe {
                    effect.SetValue(
                        D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &tone_map_target_nits.to_ne_bytes(),
                    )
                }
                .ok()?;
                // The SDR curve mode raises midtones; always use the HDR curve.
                unsafe {
                    effect.SetValue(
                        D2D1_HDRTONEMAP_PROP_DISPLAY_MODE.0 as u32,
                        D2D1_PROPERTY_TYPE_ENUM,
                        &D2D1_HDRTONEMAP_DISPLAY_MODE_HDR.0.to_ne_bytes(),
                    )
                }
                .ok()?;
                Some(effect)
            });
        let tone_map_normalize_effect = (!hdr_mode)
            .then_some(())
            .and(hdr_tone_map_effect.as_ref())
            .and_then(|_| {
                let effect =
                    unsafe { d2d_context.CreateEffect(&CLSID_D2D1WhiteLevelAdjustment) }.ok()?;
                set_white_level_input(&effect, SDR_REFERENCE_WHITE_NITS).ok()?;
                Some(effect)
            });
        // The FP16 scRGB backbuffer of ACM-on SDR takes the tone-mapped scRGB with no re-encode.
        let output_color_management_effect = (!hdr_mode && !sdr_wide_gamut)
            .then_some(())
            .and(hdr_tone_map_effect.as_ref())
            .and_then(|_| {
                Self::create_conversion_effect(
                    d2d_context,
                    scrgb_color_context,
                    sdr_destination_context,
                )
            });
        let white_level_effect = hdr_mode
            .then_some(())
            .and(color_management_effect.as_ref())
            .and_then(|_| {
                let effect =
                    unsafe { d2d_context.CreateEffect(&CLSID_D2D1WhiteLevelAdjustment) }.ok()?;
                set_white_level_input(&effect, SDR_REFERENCE_WHITE_NITS).ok()?;
                unsafe {
                    effect.SetValue(
                        D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &SDR_REFERENCE_WHITE_NITS.to_ne_bytes(),
                    )
                }
                .ok()?;
                Some(effect)
            });
        ModeEffects {
            color_management_effect,
            hdr_tone_map_effect,
            tone_map_normalize_effect,
            output_color_management_effect,
            white_level_effect,
        }
    }

    /// Dither only the UNORM backbuffers the app quantizes; FP16 leaves quantization to DWM.
    fn backbuffer_bits_for(format: DXGI_FORMAT) -> Option<u32> {
        if format == DXGI_FORMAT_B8G8R8A8_UNORM {
            Some(8)
        } else if format == DXGI_FORMAT_R10G10B10A2_UNORM {
            Some(10)
        } else {
            None
        }
    }

    /// The backbuffer format the mode prefers before any swapchain refusal.
    fn preferred_swap_chain_format(
        hdr_mode: bool,
        pq_output: bool,
        sdr_wide_gamut: bool,
        ten_bit_target: bool,
        bits_per_color: u32,
    ) -> DXGI_FORMAT {
        if hdr_mode {
            if pq_output {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            }
        } else if sdr_wide_gamut {
            // Advanced color is on: hand wide-gamut scRGB to DWM's composition.
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else if ten_bit_target && bits_per_color >= 10 {
            // Only the format widens; no declaration, so DWM keeps the sRGB reading.
            DXGI_FORMAT_R10G10B10A2_UNORM
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }

    /// The format each mode is known to accept when a 10-bit swapchain is refused.
    fn mode_fallback_format(hdr_mode: bool) -> DXGI_FORMAT {
        if hdr_mode {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }

    fn build(
        window: HWND,
        width: u32,
        height: u32,
        mode: OutputMode,
        target: ToneMapTarget,
        deep_color: bool,
        device: &GraphicsDevice,
    ) -> Result<Self> {
        let output_mode = mode.clone();
        let sdr_wide_gamut = mode.sdr_wide_gamut();
        let OutputMode {
            hdr: hdr_mode,
            bits_per_color,
            ..
        } = mode;
        let display_profile = mode.display_profile;
        let tone_map_target_nits = target.peak_nits;
        let full_frame_nits = target.full_frame_nits;
        let GraphicsDevice {
            device: d3d_device,
            context: d3d_context,
        } = device.clone();
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let upload_maximum_frame_bytes = maximum_resource_bytes(
            unsafe { dxgi_device.GetAdapter() }
                .and_then(|adapter| unsafe { adapter.GetDesc() })
                .map_or(0, |description| description.DedicatedVideoMemory as u64),
        );

        // D2D precedes the swapchain: the PQ pipeline decides the backbuffer format.
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
        // D2D draws the UNORM16 scene; the pass dithers and its UNORM write quantizes.
        let mut quantize_pass = (deep_color
            && unsafe { d2d_context.IsDxgiFormatSupported(DXGI_FORMAT_R16G16B16A16_UNORM) }
                .as_bool())
        .then(|| QuantizePass::new(&d3d_device).ok())
        .flatten();
        let ten_bit_target = quantize_pass.is_some();

        let scrgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SCRGB, None) }.ok();
        let srgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SRGB, None) }.ok();
        let (display_color_context, destination_gamut_label) = Self::display_context_and_label(
            &d2d_context,
            hdr_mode,
            sdr_wide_gamut,
            display_profile.as_ref(),
        );

        // HDR encodes to PQ in the app so its 10-bit write is the only quantizer.
        let pq_color_context = (hdr_mode && ten_bit_target)
            .then(|| Self::create_pq_color_context(&d2d_context))
            .flatten();
        let mut hdr_output_color_management_effect = Self::create_conversion_effect(
            &d2d_context,
            scrgb_color_context.as_ref(),
            pq_color_context.as_ref(),
        );

        let mut swap_chain_format = Self::preferred_swap_chain_format(
            hdr_mode,
            hdr_output_color_management_effect.is_some(),
            sdr_wide_gamut,
            ten_bit_target,
            bits_per_color,
        );
        let create_swap_chain = |format: DXGI_FORMAT| -> Result<IDXGISwapChain1> {
            unsafe {
                let adapter = dxgi_device.GetAdapter()?;
                let factory: IDXGIFactory2 = adapter.GetParent()?;
                let description = DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: SWAP_CHAIN_FLAGS.0 as u32,
                    ..Default::default()
                };
                factory.CreateSwapChainForHwnd(
                    &d3d_device,
                    window,
                    &raw const description,
                    None,
                    None,
                )
            }
        };
        let swap_chain = match create_swap_chain(swap_chain_format) {
            Ok(swap_chain) => swap_chain,
            // A 10-bit refusal falls back to the mode's proven format.
            Err(_) if swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM => {
                hdr_output_color_management_effect = None;
                swap_chain_format = Self::mode_fallback_format(hdr_mode);
                create_swap_chain(swap_chain_format)?
            }
            Err(error) => return Err(error),
        };
        // Only HDR and ACM-on wide gamut declare; scRGB on plain SDR flashes DWM composition.
        if hdr_mode {
            let swap_chain3 = swap_chain.cast::<IDXGISwapChain3>().ok();
            let pq_declared = hdr_output_color_management_effect.is_some()
                && swap_chain3.as_ref().is_some_and(|swap_chain3| {
                    declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                        .is_ok()
                });
            if !pq_declared {
                hdr_output_color_management_effect = None;
                if swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM {
                    swap_chain_format = DXGI_FORMAT_R16G16B16A16_FLOAT;
                    unsafe {
                        swap_chain.ResizeBuffers(
                            0,
                            width,
                            height,
                            swap_chain_format,
                            SWAP_CHAIN_FLAGS,
                        )?;
                    }
                }
                if let Some(swap_chain3) = &swap_chain3 {
                    let _ =
                        declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
                }
            }
        } else if sdr_wide_gamut && let Ok(swap_chain3) = swap_chain.cast::<IDXGISwapChain3>() {
            // The FP16 default is already scRGB; declare it so the intent is explicit.
            let _ = declare_color_space(&swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
        }
        // FP16 leaves quantization to DWM; the UNORM backbuffers keep the pass.
        if swap_chain_format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            quantize_pass = None;
        }
        // Waitable swapchains default to a present-queue depth of 1.
        let frame_latency_waitable = swap_chain
            .cast::<IDXGISwapChain2>()
            .ok()
            .map(|swap_chain2| unsafe { swap_chain2.GetFrameLatencyWaitableObject() });
        let mode_effects = Self::create_mode_effects(
            &d2d_context,
            hdr_mode,
            sdr_wide_gamut,
            tone_map_target_nits,
            scrgb_color_context.as_ref(),
            sdr_destination(display_color_context.as_ref(), srgb_color_context.as_ref()),
        );
        static UPLOAD_DEVICE_GENERATIONS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let mut renderer = Self {
            output_mode,
            upload_device_generation: UPLOAD_DEVICE_GENERATIONS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            upload_maximum_frame_bytes,
            hdr_mode,
            sdr_wide_gamut,
            bits_per_color,
            swap_chain_format,
            tone_map_target_nits,
            display_full_frame_nits: full_frame_nits,
            swap_chain,
            frame_latency_waitable,
            d3d_device,
            d3d_context,
            d2d_context,
            quantize_pass,
            scene_shader_resource_view: None,
            backbuffer_render_target_view: None,
            backbuffer_size: (0, 0),
            target: None,
            image: None,
            effect_output: None,
            color_management_effect: mode_effects.color_management_effect,
            white_level_effect: mode_effects.white_level_effect,
            hdr_tone_map_effect: mode_effects.hdr_tone_map_effect,
            tone_map_normalize_effect: mode_effects.tone_map_normalize_effect,
            output_color_management_effect: mode_effects.output_color_management_effect,
            hdr_output_color_management_effect,
            pq_color_context,
            dither_setting: DitherMode::None,
            image_storage: PixelStorage::Bgra8,
            image_source_bits_per_channel: 8,
            scrgb_color_context,
            srgb_color_context,
            display_color_context,
            destination_gamut_label,
            source_gamut_label: None,
            output_description: String::new(),
            source_icc_profile: None,
            source_color_context: None,
            linear_source_primaries: None,
            linear_source_context: None,
            image_display_size: (0.0, 0.0),
            image_pixel_size: (0.0, 0.0),
        };
        renderer.refresh_output_description();
        renderer.create_target()?;
        Ok(renderer)
    }

    pub fn hdr_mode(&self) -> bool {
        self.hdr_mode
    }

    /// The mode this renderer was built or reconfigured with.
    pub fn output_mode(&self) -> &OutputMode {
        &self.output_mode
    }

    /// Tone-map luminances for the info overlay: display caps and the output target.
    pub fn tone_map_info(&self) -> ToneMapInfo {
        ToneMapInfo {
            hdr_display: self.hdr_mode,
            display_peak_nits: self.tone_map_target_nits,
            display_full_frame_nits: self.display_full_frame_nits,
            output_target_nits: self.tone_map_target_nits,
        }
    }

    /// True when the backbuffer is HDR10 (PQ) rather than the scRGB FP16 fallback.
    pub fn pq_output(&self) -> bool {
        self.hdr_output_color_management_effect.is_some()
    }

    /// True when the SDR output is advanced-color FP16 scRGB (wide gamut handed to DWM).
    pub fn sdr_wide_gamut(&self) -> bool {
        self.sdr_wide_gamut
    }

    /// Active backbuffer, for the info overlay. ACM-off SDR names the gamut it maps into.
    pub fn output_description(&self) -> &str {
        &self.output_description
    }

    /// Recomputes the cached output label after a format, mode, or gamut change.
    fn refresh_output_description(&mut self) {
        self.output_description = if self.swap_chain_format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            "FP16 scRGB".to_string()
        } else if self.swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM {
            if self.hdr_mode {
                "10-bit HDR10 (PQ)".to_string()
            } else {
                self.sdr_output_description("10-bit")
            }
        } else {
            self.sdr_output_description("8-bit")
        };
    }

    /// Linear light in the given primaries; D65, which every gamut riv can state uses.
    fn create_linear_color_context(&self, primaries: [[f32; 2]; 3]) -> Option<ID2D1ColorContext> {
        // D65 tristimulus, normalized to Y = 1, which is what whitePointXZ wants.
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
        unsafe {
            self.d2d_context
                .cast::<ID2D1DeviceContext5>()
                .ok()?
                .CreateColorContextFromSimpleColorProfile(&raw const profile)
        }
        .ok()
        .map(Into::into)
    }

    /// Whether this source profile, or untagged sRGB, is already the SDR destination space.
    fn is_destination_space(&self, icc_profile: Option<&[u8]>) -> bool {
        // Only a context riv managed to build stands in as the destination; the rest read sRGB.
        let destination = self
            .display_color_context
            .as_ref()
            .and(self.output_mode.display_profile.as_deref())
            .map(Vec::as_slice);
        match (icc_profile, destination) {
            // Untagged stands for sRGB, so one side present means that side must be sRGB.
            (None, None) => true,
            (None, Some(profile)) | (Some(profile), None) => icc_is_srgb(profile),
            (Some(source), Some(display)) => icc_same_space(source, display),
        }
    }

    /// SDR destination color context: the display's own profile when mapping, else sRGB.
    fn sdr_destination_context(&self) -> Option<&ID2D1ColorContext> {
        sdr_destination(
            self.display_color_context.as_ref(),
            self.srgb_color_context.as_ref(),
        )
    }

    /// SDR output label; ACM-off profile mapping appends the destination gamut.
    fn sdr_output_description(&self, bits: &str) -> String {
        let destination = self.destination_gamut_label.unwrap_or("sRGB");
        match self.source_gamut_label {
            Some(source) if source != destination => format!("{bits} {source} in {destination}"),
            _ => format!("{bits} {destination}"),
        }
    }

    fn create_target(&mut self) -> Result<()> {
        let scene_format = if self.quantize_pass.is_some() {
            // D2D draws the UNORM16 scene; the pass dithers and quantizes to the backbuffer.
            DXGI_FORMAT_R16G16B16A16_UNORM
        } else {
            self.swap_chain_format
        };
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: scene_format,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        unsafe {
            let buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            let mut buffer_description = D3D11_TEXTURE2D_DESC::default();
            buffer.GetDesc(&raw mut buffer_description);
            self.backbuffer_size = (buffer_description.Width, buffer_description.Height);
            let surface: IDXGISurface = if self.quantize_pass.is_some() {
                let scene_texture = crate::view::create_scene_texture(
                    &self.d3d_device,
                    self.backbuffer_size,
                    scene_format,
                )?;
                let mut scene_view = None;
                self.d3d_device.CreateShaderResourceView(
                    &scene_texture,
                    None,
                    Some(&raw mut scene_view),
                )?;
                self.scene_shader_resource_view = scene_view;
                let mut backbuffer_view = None;
                self.d3d_device.CreateRenderTargetView(
                    &buffer,
                    None,
                    Some(&raw mut backbuffer_view),
                )?;
                self.backbuffer_render_target_view = backbuffer_view;
                scene_texture.cast()?
            } else {
                buffer.cast()?
            };
            let target = self
                .d2d_context
                .CreateBitmapFromDxgiSurface(&surface, Some(&raw const properties))?;
            self.d2d_context.SetTarget(&target);
            self.target = Some(target);
        }
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe {
            self.d2d_context.SetTarget(None);
            self.target = None;
            self.scene_shader_resource_view = None;
            self.backbuffer_render_target_view = None;
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                SWAP_CHAIN_FLAGS,
            )?;
        }
        self.create_target()
    }

    /// Switches the output mode in place: DXGI allows one flip-model swapchain per window.
    pub fn reconfigure_output(
        &mut self,
        mode: OutputMode,
        tone_map_target_nits: f32,
        full_frame_nits: f32,
    ) -> Result<()> {
        let sdr_wide_gamut = mode.sdr_wide_gamut();
        let output_mode = mode.clone();
        let OutputMode {
            hdr: hdr_mode,
            bits_per_color,
            ..
        } = mode;
        let display_profile = mode.display_profile;
        // Adopt the target state first so a partial failure cannot retry every WM_MOVE.
        self.output_mode = output_mode;
        self.hdr_mode = hdr_mode;
        self.sdr_wide_gamut = sdr_wide_gamut;
        (self.display_color_context, self.destination_gamut_label) =
            Self::display_context_and_label(
                &self.d2d_context,
                hdr_mode,
                sdr_wide_gamut,
                display_profile.as_ref(),
            );
        self.bits_per_color = bits_per_color;
        self.tone_map_target_nits = tone_map_target_nits;
        self.display_full_frame_nits = full_frame_nits;

        // Release every backbuffer reference ahead of ResizeBuffers.
        unsafe { self.d2d_context.SetTarget(None) };
        self.target = None;
        self.effect_output = None;
        self.scene_shader_resource_view = None;
        self.backbuffer_render_target_view = None;

        // The pass depends only on the (unchanged) device, so keep it across reconfigures.
        if self.quantize_pass.is_none()
            && unsafe {
                self.d2d_context
                    .IsDxgiFormatSupported(DXGI_FORMAT_R16G16B16A16_UNORM)
            }
            .as_bool()
        {
            self.quantize_pass = QuantizePass::new(&self.d3d_device).ok();
        }
        let ten_bit_target = self.quantize_pass.is_some();

        let pq_color_context = (hdr_mode && ten_bit_target)
            .then(|| Self::create_pq_color_context(&self.d2d_context))
            .flatten();
        let mut hdr_output_color_management_effect = Self::create_conversion_effect(
            &self.d2d_context,
            self.scrgb_color_context.as_ref(),
            pq_color_context.as_ref(),
        );
        let mut swap_chain_format = Self::preferred_swap_chain_format(
            hdr_mode,
            hdr_output_color_management_effect.is_some(),
            sdr_wide_gamut,
            ten_bit_target,
            bits_per_color,
        );
        let resize_to = |swap_chain: &IDXGISwapChain1, format| unsafe {
            swap_chain.ResizeBuffers(0, 0, 0, format, SWAP_CHAIN_FLAGS)
        };
        if let Err(error) = resize_to(&self.swap_chain, swap_chain_format) {
            if swap_chain_format != DXGI_FORMAT_R10G10B10A2_UNORM {
                return Err(error);
            }
            // A 10-bit refusal falls back to the mode's proven format.
            hdr_output_color_management_effect = None;
            swap_chain_format = Self::mode_fallback_format(hdr_mode);
            resize_to(&self.swap_chain, swap_chain_format)?;
        }
        let swap_chain3 = self.swap_chain.cast::<IDXGISwapChain3>().ok();
        if hdr_mode {
            let pq_declared = hdr_output_color_management_effect.is_some()
                && swap_chain3.as_ref().is_some_and(|swap_chain3| {
                    declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                        .is_ok()
                });
            if !pq_declared {
                hdr_output_color_management_effect = None;
                if swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM {
                    swap_chain_format = DXGI_FORMAT_R16G16B16A16_FLOAT;
                    resize_to(&self.swap_chain, swap_chain_format)?;
                }
                if let Some(swap_chain3) = &swap_chain3 {
                    let _ =
                        declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
                }
            }
        } else if let Some(swap_chain3) = &swap_chain3 {
            if sdr_wide_gamut {
                // Advanced-color SDR: extended-range scRGB for DWM's composition.
                let _ = declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
            } else {
                // Undo any HDR declaration; SDR composition reads sRGB.
                let _ = declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            }
        }
        if swap_chain_format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            self.quantize_pass = None;
        }

        let mode_effects = Self::create_mode_effects(
            &self.d2d_context,
            hdr_mode,
            sdr_wide_gamut,
            tone_map_target_nits,
            self.scrgb_color_context.as_ref(),
            self.sdr_destination_context(),
        );
        self.color_management_effect = mode_effects.color_management_effect;
        self.white_level_effect = mode_effects.white_level_effect;
        self.hdr_tone_map_effect = mode_effects.hdr_tone_map_effect;
        self.tone_map_normalize_effect = mode_effects.tone_map_normalize_effect;
        self.output_color_management_effect = mode_effects.output_color_management_effect;
        self.hdr_output_color_management_effect = hdr_output_color_management_effect;
        self.pq_color_context = pq_color_context;
        self.swap_chain_format = swap_chain_format;
        self.refresh_output_description();
        self.create_target()
    }

    pub fn set_sdr_white_boost(&mut self, boost: f32) {
        if let Some(effect) = &self.white_level_effect {
            let _ = set_white_level_input(effect, SDR_REFERENCE_WHITE_NITS * boost.max(0.01));
        }
    }

    /// Updates the stored display luminances (overlay, next rewire); true when they changed.
    pub fn set_tone_map_target(&mut self, nits: f32, full_frame_nits: f32) -> bool {
        if (nits - self.tone_map_target_nits).abs() < f32::EPSILON
            && (full_frame_nits - self.display_full_frame_nits).abs() < f32::EPSILON
        {
            return false;
        }
        self.tone_map_target_nits = nits;
        self.display_full_frame_nits = full_frame_nits;
        true
    }

    /// A matching worker texture wraps without an upload; anything else re-uploads.
    pub fn set_image(
        &mut self,
        frame_pixels: &[u8],
        texture: Option<&UploadedTexture>,
        image: &DecodedImage,
    ) -> Result<()> {
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
        self.adopt_image_bitmap(bitmap, image);
        Ok(())
    }

    fn wrap_uploaded_texture(
        &mut self,
        uploaded: &UploadedTexture,
        image: &DecodedImage,
    ) -> Result<()> {
        let surface: IDXGISurface = uploaded.texture.cast()?;
        let properties = image_bitmap_properties(image.storage);
        let bitmap = unsafe {
            self.d2d_context
                .CreateBitmapFromDxgiSurface(&surface, Some(&raw const properties))?
        };
        self.adopt_image_bitmap(bitmap, image);
        Ok(())
    }

    /// Copies a worker texture back to CPU pixels; only this build's textures qualify.
    /// Used before a planned rebuild, while this device is still alive.
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
        let staging = staging.ok_or_else(windows::core::Error::empty)?;
        let pitch = image.row_pitch() as usize;
        let mut pixels = vec![0u8; image.frame_byte_length()];
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

    fn adopt_image_bitmap(&mut self, bitmap: ID2D1Bitmap1, image: &DecodedImage) {
        self.image_display_size = (image.width as f32, image.height as f32);
        self.image_pixel_size = (image.pixel_width as f32, image.pixel_height as f32);
        self.image_storage = image.storage;
        self.image_source_bits_per_channel = image.source_bits_per_channel;
        self.rewire_effect_chain(
            &bitmap,
            image.icc_profile.as_deref(),
            image.storage,
            image.peak_luminance_nits,
            image.source_primaries,
        );
        self.image = Some(bitmap);
    }

    pub fn set_dither_setting(&mut self, mode: DitherMode) {
        self.dither_setting = mode;
    }

    /// Reuses the current bitmap wiring; callers fall back to set_image when there is none.
    pub fn update_frame_pixels(&mut self, pixels: &[u8]) -> Result<()> {
        let Some(bitmap) = &self.image else {
            return Err(windows::core::Error::empty());
        };
        let pitch = self.image_pixel_size.0 as u32 * self.image_storage.bytes_per_pixel();
        unsafe { bitmap.CopyFromMemory(None, pixels.as_ptr().cast(), pitch) }?;
        Ok(())
    }

    fn rewire_effect_chain(
        &mut self,
        bitmap: &ID2D1Bitmap1,
        icc_profile: Option<&[u8]>,
        storage: PixelStorage,
        peak_luminance_nits: Option<f32>,
        source_primaries: Option<[[f32; 2]; 3]>,
    ) {
        self.effect_output = None;
        self.source_gamut_label = source_primaries
            .map(nearest_gamut_label)
            .or_else(|| icc_profile.and_then(icc_gamut_label));
        self.refresh_output_description();
        let Some(color_management) = &self.color_management_effect else {
            return;
        };
        // HDR passes through; SDR maps content above SDR white to the target.
        let hdr_content = peak_luminance_nits.is_some_and(|peak| peak > SDR_REFERENCE_WHITE_NITS);
        let tone_map = self
            .hdr_tone_map_effect
            .as_ref()
            .zip(peak_luminance_nits.filter(|_| hdr_content));
        let scrgb_destination = self.hdr_mode || self.sdr_wide_gamut || tone_map.is_some();
        // A source already in the destination space skips CM; the conversion would change nothing.
        if storage == PixelStorage::Bgra8
            && !scrgb_destination
            && self.is_destination_space(icc_profile)
        {
            // Unwire the previous bitmap so the effect does not keep it alive.
            unsafe { color_management.SetInput(0, None, true) };
            return;
        }
        // FP16 pixels are linear light in the stated primaries; scRGB covers unknown ones.
        let dedicated_context = match storage {
            PixelStorage::RgbaHalf => {
                // A source that states nothing leaves the cached context alone.
                if let Some(primaries) = source_primaries
                    && self.linear_source_primaries != Some(primaries)
                {
                    self.linear_source_context = self.create_linear_color_context(primaries);
                    self.linear_source_primaries = Some(primaries);
                }
                source_primaries
                    .and(self.linear_source_context.as_ref())
                    .or(self.scrgb_color_context.as_ref())
            }
            PixelStorage::Bgra8 => None,
        };
        let source_context = match dedicated_context {
            Some(context) => context,
            None => {
                if self.source_color_context.is_none()
                    || self.source_icc_profile.as_deref() != icc_profile
                {
                    self.source_color_context = match icc_profile {
                        Some(icc_profile) => unsafe {
                            self.d2d_context
                                .CreateColorContext(D2D1_COLOR_SPACE_CUSTOM, Some(icc_profile))
                        }
                        .ok(),
                        None => None,
                    }
                    .or_else(|| {
                        unsafe {
                            self.d2d_context
                                .CreateColorContext(D2D1_COLOR_SPACE_SRGB, None)
                        }
                        .ok()
                    });
                    self.source_icc_profile = icc_profile.map(<[u8]>::to_vec);
                }
                let Some(source_context) = &self.source_color_context else {
                    return;
                };
                source_context
            }
        };
        // HDR content needs no linear stage before the backbuffer, so it converts once.
        let direct_to_backbuffer = hdr_content && self.pq_output();
        let destination_context = if direct_to_backbuffer {
            self.pq_color_context.as_ref()
        } else if scrgb_destination {
            self.scrgb_color_context.as_ref()
        } else {
            self.sdr_destination_context()
        };
        let Some(destination_context) = destination_context else {
            return;
        };
        if wire_color_management(color_management, source_context, destination_context).is_err() {
            return;
        }
        unsafe { color_management.SetInput(0, bitmap, true) };
        let Ok(converted) = (unsafe { color_management.GetOutput() }) else {
            return;
        };
        let scene = match tone_map {
            Some((tone_map_effect, peak)) => {
                // Very low input maxima misbehave; floor at the SDR reference white.
                let input_maximum = peak.max(SDR_REFERENCE_WHITE_NITS);
                let input_set = unsafe {
                    tone_map_effect.SetValue(
                        D2D1_HDRTONEMAP_PROP_INPUT_MAX_LUMINANCE.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &input_maximum.to_ne_bytes(),
                    )
                }
                .is_ok();
                if !input_set {
                    return;
                }
                let output_maximum = self.tone_map_target_nits;
                let _ = unsafe {
                    tone_map_effect.SetValue(
                        D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
                        D2D1_PROPERTY_TYPE_FLOAT,
                        &output_maximum.to_ne_bytes(),
                    )
                };
                unsafe { tone_map_effect.SetInput(0, &converted, true) };
                let tone_mapped = unsafe { tone_map_effect.GetOutput() }.ok();
                // Reinterpret scene-referred white as display-referred, then re-encode to sRGB.
                tone_mapped.and_then(|tone_mapped| {
                    let normalize = self.tone_map_normalize_effect.as_ref()?;
                    let display_white = self.tone_map_target_nits.min(input_maximum);
                    unsafe {
                        normalize.SetValue(
                            D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL.0 as u32,
                            D2D1_PROPERTY_TYPE_FLOAT,
                            &display_white.to_ne_bytes(),
                        )
                    }
                    .ok()?;
                    unsafe { normalize.SetInput(0, &tone_mapped, true) };
                    let normalized = unsafe { normalize.GetOutput() }.ok()?;
                    if self.sdr_wide_gamut {
                        // FP16 scRGB backbuffer: keep the tone-mapped scRGB, no sRGB re-encode.
                        return Some(normalized);
                    }
                    let output_encoding = self.output_color_management_effect.as_ref()?;
                    unsafe { output_encoding.SetInput(0, &normalized, true) };
                    unsafe { output_encoding.GetOutput() }.ok()
                })
            }
            None => {
                // SDR content takes the white-level boost; HDR content passes through.
                match &self.white_level_effect {
                    Some(white_level) if !hdr_content => {
                        unsafe { white_level.SetInput(0, &converted, true) };
                        unsafe { white_level.GetOutput() }.ok()
                    }
                    _ => Some(converted),
                }
            }
        };
        // The HDR10 backbuffer quantizes PQ; encode after every linear stage.
        let output_encoding = self
            .hdr_output_color_management_effect
            .as_ref()
            .filter(|_| !direct_to_backbuffer);
        self.effect_output = match (output_encoding, scene) {
            (Some(output_encoding), Some(scene)) => {
                unsafe { output_encoding.SetInput(0, &scene, true) };
                unsafe { output_encoding.GetOutput() }.ok()
            }
            (None, scene) => scene,
            (Some(_), None) => None,
        };
    }

    pub fn clear_image(&mut self) {
        self.image = None;
        self.effect_output = None;
    }

    /// Decides the frame's placement and quantization; the info panel reads it.
    pub fn decide_frame(
        &self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
    ) -> FrameDecision {
        // DrawImage has no destination rect; fold the display scale into the matrix.
        let scale_x = self.image_display_size.0 / self.image_pixel_size.0.max(1.0);
        let scale_y = self.image_display_size.1 / self.image_pixel_size.1.max(1.0);
        let transform = Matrix3x2 {
            M11: matrix[0] * scale_x,
            M12: matrix[1] * scale_x,
            M21: matrix[2] * scale_y,
            M22: matrix[3] * scale_y,
            M31: matrix[4],
            M32: matrix[5],
        };
        // Fold a 90/270 rotation onto the axes; a 1:1 placement on whole pixels resamples nothing.
        let identity_placement = if matrix[1] == 0.0 && matrix[2] == 0.0 {
            Self::is_pixel_identity(transform.M11, transform.M22, transform.M31, transform.M32)
        } else if matrix[0] == 0.0 && matrix[3] == 0.0 {
            let source_height = self.image_pixel_size.1.round();
            Self::is_pixel_identity(
                -transform.M21,
                transform.M12,
                source_height * transform.M21 + transform.M31,
                transform.M32,
            )
        } else {
            false
        };
        // Force NEAREST so a 1:1 placement stays pixel-exact, whatever the filter.
        let draw_interpolation = if identity_placement {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        } else {
            interpolation
        };
        let backbuffer_bits = Self::backbuffer_bits_for(self.swap_chain_format)
            .filter(|_| self.quantize_pass.is_some());
        let quantization_steps = backbuffer_bits.map(|bits| (1 << bits) - 1);
        let pass_dither = match backbuffer_bits {
            Some(bits) if self.image.is_some() => self.active_dither_mode(draw_interpolation, bits),
            _ => DitherMode::None,
        };
        FrameDecision {
            transform,
            draw_interpolation,
            dither: pass_dither,
            quantization_steps,
            identity_placement,
        }
    }

    pub fn render(
        &mut self,
        decision: FrameDecision,
        clear_color: D2D1_COLOR_F,
        draw_overlay: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        // Wait for present-queue room first, so the frame draws the freshest state.
        if let Some(handle) = self.frame_latency_waitable {
            let _ = unsafe { WaitForSingleObjectEx(handle, 1000, false) };
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
                            right: self.image_pixel_size.0,
                            bottom: self.image_pixel_size.1,
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
            // Overlay failure must not block presenting the frame.
            let overlay_result = draw_overlay(&self.d2d_context);
            self.d2d_context.EndDraw(None, None)?;
            if let (Some(quantize_pass), Some(quantization_steps), Some(scene), Some(backbuffer)) = (
                &self.quantize_pass,
                decision.quantization_steps,
                &self.scene_shader_resource_view,
                &self.backbuffer_render_target_view,
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
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            overlay_result
        }
    }

    /// A whole-pixel 1:1 placement (unit scale, integer offset) that resamples nothing.
    fn is_pixel_identity(scale_x: f32, scale_y: f32, offset_x: f32, offset_y: f32) -> bool {
        (scale_x.abs() - 1.0).abs() < 1e-6
            && (scale_y.abs() - 1.0).abs() < 1e-6
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
