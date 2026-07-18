//! D3D11 + DXGI flip swapchain + D2D draw path; the swapchain format matches the monitor mode.

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
    D2D1_COMPOSITE_MODE_SOURCE_OVER, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1ColorManagement, CLSID_D2D1HdrToneMap, CLSID_D2D1WhiteLevelAdjustment,
    CLSID_D2D12DAffineTransform, D2D1_2DAFFINETRANSFORM_PROP_INTERPOLATION_MODE,
    D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_BUFFER_PRECISION_16BPC_FLOAT, D2D1_COLOR_SPACE_CUSTOM, D2D1_COLOR_SPACE_SCRGB,
    D2D1_COLOR_SPACE_SRGB, D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_PROP_QUALITY, D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_QUALITY_BEST, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HDRTONEMAP_DISPLAY_MODE_HDR,
    D2D1_HDRTONEMAP_PROP_DISPLAY_MODE, D2D1_HDRTONEMAP_PROP_INPUT_MAX_LUMINANCE,
    D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE, D2D1_INTERPOLATION_MODE,
    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_PROPERTY_TYPE_COLOR_CONTEXT, D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT,
    D2D1_PROPERTY_TYPE_MATRIX_3X2, D2D1_WHITELEVELADJUSTMENT_PROP_INPUT_WHITE_LEVEL,
    D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL, D2D1CreateFactory, ID2D1Bitmap1,
    ID2D1ColorContext, ID2D1DeviceContext, ID2D1DeviceContext5, ID2D1Effect, ID2D1Factory,
    ID2D1Factory1, ID2D1Image,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_12_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
    IDXGISwapChain3,
};
use windows::core::{Interface, Result};
use windows_numerics::Matrix3x2;

use crate::image::color::SDR_REFERENCE_WHITE_NITS;
use crate::image::decode::PixelStorage;
use crate::view::dither::{self, DitherMode};
use crate::view::quantize::QuantizePass;
use crate::view::sampling::{AxisMapping, SamplingPass};

struct FlattenScene {
    shader_resource_view: ID3D11ShaderResourceView,
    target: ID2D1Bitmap1,
    size: (u32, u32),
    /// Scene version and quadrant the held pixels were drawn from.
    version: u64,
    rotated: bool,
}

struct ScaledScene {
    render_target_view: ID3D11RenderTargetView,
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
}

struct ModeEffects {
    color_management_effect: Option<ID2D1Effect>,
    hdr_tone_map_effect: Option<ID2D1Effect>,
    tone_map_normalize_effect: Option<ID2D1Effect>,
    output_color_management_effect: Option<ID2D1Effect>,
    white_level_effect: Option<ID2D1Effect>,
    /// SDR only: sRGB <-> scRGB pair so the scaler convolves linear light.
    linearize_effect: Option<ID2D1Effect>,
    delinearize_effect: Option<ID2D1Effect>,
}

pub struct Renderer {
    hdr_mode: bool,
    bits_per_color: u32,
    swap_chain_format: DXGI_FORMAT,
    tone_map_target_nits: f32,
    swap_chain: IDXGISwapChain1,
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    d2d_context: ID2D1DeviceContext,
    /// Fullscreen quantizing copy for the 10-bit backbuffers D2D cannot target.
    quantize_pass: Option<QuantizePass>,
    /// Separable Lanczos/Hermite scaling; None falls back to D2D interpolation.
    sampling_pass: Option<SamplingPass>,
    flatten_scene: Option<FlattenScene>,
    scaled_scene: Option<ScaledScene>,
    scene_shader_resource_view: Option<ID3D11ShaderResourceView>,
    backbuffer_render_target_view: Option<ID3D11RenderTargetView>,
    backbuffer_size: (u32, u32),
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    effect_output: Option<ID2D1Image>,
    /// scRGB scene ahead of the PQ output encode; present only when that encode is wired.
    linear_scene_output: Option<ID2D1Image>,
    color_management_effect: Option<ID2D1Effect>,
    white_level_effect: Option<ID2D1Effect>,
    hdr_tone_map_effect: Option<ID2D1Effect>,
    tone_map_normalize_effect: Option<ID2D1Effect>,
    output_color_management_effect: Option<ID2D1Effect>,
    /// scRGB -> PQ BT.2020 for the HDR10 backbuffer; None on the FP16 fallback.
    hdr_output_color_management_effect: Option<ID2D1Effect>,
    /// Second scRGB -> PQ instance for the scaled scene; the primary stays on the 1:1 chain.
    hdr_scaled_color_management_effect: Option<ID2D1Effect>,
    linearize_effect: Option<ID2D1Effect>,
    delinearize_effect: Option<ID2D1Effect>,
    affine_transform_effect: Option<ID2D1Effect>,
    dither_ordered_effect: Option<ID2D1Effect>,
    dither_fruit_effect: Option<ID2D1Effect>,
    dither_mode: DitherMode,
    image_storage: PixelStorage,
    image_source_bits_per_channel: u32,
    scrgb_color_context: Option<ID2D1ColorContext>,
    srgb_color_context: Option<ID2D1ColorContext>,
    source_icc_profile: Option<Vec<u8>>,
    source_color_context: Option<ID2D1ColorContext>,
    image_display_size: (f32, f32),
    image_pixel_size: (f32, f32),
    /// What the last frame actually scaled with, for the info panel.
    scaler_description: &'static str,
    /// What the last frame actually dithered with, for the info panel.
    dither_description: &'static str,
    /// Bumped on any change that alters the flattened scene content.
    scene_version: u64,
    /// Guaranteed per-resource size; the flatten reduces itself to fit.
    maximum_resource_bytes: u64,
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { self.d2d_context.SetTarget(None) };
        self.effect_output = None;
        self.linear_scene_output = None;
        self.image = None;
        self.target = None;
        self.flatten_scene = None;
        self.scaled_scene = None;
        self.scene_shader_resource_view = None;
        self.backbuffer_render_target_view = None;
        self.color_management_effect = None;
        self.white_level_effect = None;
        self.hdr_tone_map_effect = None;
        self.tone_map_normalize_effect = None;
        self.output_color_management_effect = None;
        self.hdr_output_color_management_effect = None;
        self.hdr_scaled_color_management_effect = None;
        self.linearize_effect = None;
        self.delinearize_effect = None;
        self.affine_transform_effect = None;
        self.dither_ordered_effect = None;
        self.dither_fruit_effect = None;
    }
}

fn create_d3d_device(
    driver_type: D3D_DRIVER_TYPE,
    feature_levels: &[D3D_FEATURE_LEVEL],
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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
    Ok((
        device.expect("D3D11CreateDevice succeeded without device"),
        context.expect("D3D11CreateDevice succeeded without context"),
    ))
}

/// D3D11 guaranteed per-resource limit: min(max(128 MiB, adapter memory / 4), 2 GiB - 1).
fn maximum_resource_bytes(dxgi_device: &IDXGIDevice) -> u64 {
    const FLOOR_BYTES: u64 = 128 << 20;
    const CEILING_BYTES: u64 = (2 << 30) - 1;
    let memory = unsafe {
        dxgi_device
            .GetAdapter()
            .and_then(|adapter| adapter.GetDesc())
    }
    .map(|description| {
        // UMA adapters under-report dedicated memory; take the larger pool.
        (description.DedicatedVideoMemory as u64).max(description.SharedSystemMemory as u64)
    })
    .unwrap_or(0);
    (memory / 4).clamp(FLOOR_BYTES, CEILING_BYTES)
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
        format: match storage {
            PixelStorage::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            PixelStorage::RgbaHalf => DXGI_FORMAT_R16G16B16A16_FLOAT,
        },
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
}

/// Effect property payload for interface values: the raw pointer bytes.
fn interface_property_bytes<T: Interface>(interface: &T) -> [u8; size_of::<usize>()] {
    (interface.as_raw() as usize).to_ne_bytes()
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
        hdr_mode: bool,
        bits_per_color: u32,
        tone_map_target_nits: f32,
    ) -> Result<Self> {
        // A deep-color failure downgrades to the proven formats, never blocks launch.
        Self::build(
            window,
            width,
            height,
            hdr_mode,
            bits_per_color,
            tone_map_target_nits,
            true,
        )
        .or_else(|_| {
            Self::build(
                window,
                width,
                height,
                hdr_mode,
                bits_per_color,
                tone_map_target_nits,
                false,
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
        Some(effect)
    }

    /// scRGB -> PQ BT.2020 for the HDR10 backbuffer.
    fn create_pq_output_effect(
        d2d_context: &ID2D1DeviceContext,
        scrgb_color_context: Option<&ID2D1ColorContext>,
    ) -> Option<ID2D1Effect> {
        let pq_color_context = unsafe {
            d2d_context
                .cast::<ID2D1DeviceContext5>()
                .ok()?
                .CreateColorContextFromDxgiColorSpace(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                .ok()?
        };
        let effect = Self::create_color_management_effect(d2d_context)?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(scrgb_color_context?),
            )
        }
        .ok()?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(&pq_color_context),
            )
        }
        .ok()?;
        Some(effect)
    }

    fn create_transfer_effect(
        d2d_context: &ID2D1DeviceContext,
        source: Option<&ID2D1ColorContext>,
        destination: Option<&ID2D1ColorContext>,
    ) -> Option<ID2D1Effect> {
        let effect = Self::create_color_management_effect(d2d_context)?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(source?),
            )
        }
        .ok()?;
        unsafe {
            effect.SetValue(
                D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(destination?),
            )
        }
        .ok()?;
        Some(effect)
    }

    fn create_mode_effects(
        d2d_context: &ID2D1DeviceContext,
        hdr_mode: bool,
        tone_map_target_nits: f32,
        scrgb_color_context: Option<&ID2D1ColorContext>,
        srgb_color_context: Option<&ID2D1ColorContext>,
    ) -> ModeEffects {
        let color_management_effect = Self::create_color_management_effect(d2d_context);
        let hdr_tone_map_effect = color_management_effect.as_ref().and_then(|_| {
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
        let output_color_management_effect = (!hdr_mode)
            .then_some(())
            .and(hdr_tone_map_effect.as_ref())
            .and_then(|_| {
                Self::create_transfer_effect(d2d_context, scrgb_color_context, srgb_color_context)
            });
        let linearize_effect = (!hdr_mode)
            .then(|| {
                Self::create_transfer_effect(d2d_context, srgb_color_context, scrgb_color_context)
            })
            .flatten();
        let delinearize_effect = (!hdr_mode)
            .then(|| {
                Self::create_transfer_effect(d2d_context, scrgb_color_context, srgb_color_context)
            })
            .flatten();
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
            linearize_effect,
            delinearize_effect,
        }
    }

    /// Dither only the UNORM backbuffers the app quantizes; FP16 leaves quantization to DWM.
    fn quantization_steps_for(format: DXGI_FORMAT) -> Option<u32> {
        if format == DXGI_FORMAT_B8G8R8A8_UNORM {
            Some(255)
        } else if format == DXGI_FORMAT_R10G10B10A2_UNORM {
            Some(1023)
        } else {
            None
        }
    }

    fn create_dither_effects(
        d2d_context: &ID2D1DeviceContext,
        d2d_factory: &ID2D1Factory1,
        quantization_steps: u32,
    ) -> (
        Option<ID2D1Effect>,
        Option<ID2D1Effect>,
        Option<ID2D1Effect>,
    ) {
        if dither::prepare_dither_effects(quantization_steps).is_err() {
            return (None, None, None);
        }
        // A repeat registration is harmless; creation fails if none ever succeeded.
        let _ = dither::register_dither_effects(d2d_factory);
        unsafe {
            (
                d2d_context.CreateEffect(&CLSID_D2D12DAffineTransform).ok(),
                d2d_context
                    .CreateEffect(&dither::CLSID_RIV_DITHER_ORDERED)
                    .ok(),
                d2d_context
                    .CreateEffect(&dither::CLSID_RIV_DITHER_FRUIT)
                    .ok(),
            )
        }
    }

    fn build(
        window: HWND,
        width: u32,
        height: u32,
        hdr_mode: bool,
        bits_per_color: u32,
        tone_map_target_nits: f32,
        deep_color: bool,
    ) -> Result<Self> {
        // D3D11 WARP is documented only through 11_1; shader model 5.0 needs no more.
        let (d3d_device, d3d_context) =
            create_d3d_device(D3D_DRIVER_TYPE_HARDWARE, &[D3D_FEATURE_LEVEL_12_0])
                .or_else(|_| create_d3d_device(D3D_DRIVER_TYPE_WARP, &[D3D_FEATURE_LEVEL_11_0]))?;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let maximum_resource_bytes = maximum_resource_bytes(&dxgi_device);

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
        // D2D cannot target 10-bit UNORM: draw on UNORM16, then a fullscreen pass quantizes.
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

        // HDR encodes to PQ in the app so its 10-bit write is the only quantizer.
        let mut hdr_output_color_management_effect = (hdr_mode && ten_bit_target)
            .then(|| Self::create_pq_output_effect(&d2d_context, scrgb_color_context.as_ref()))
            .flatten();

        let mut swap_chain_format = if hdr_mode {
            if hdr_output_color_management_effect.is_some() {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            }
        } else if ten_bit_target && bits_per_color >= 10 {
            // Only the format widens; no declaration, so DWM keeps the sRGB reading.
            DXGI_FORMAT_R10G10B10A2_UNORM
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        };
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
                    Scaling: DXGI_SCALING_NONE,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
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
                swap_chain_format = if hdr_mode {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                };
                create_swap_chain(swap_chain_format)?
            }
            Err(error) => return Err(error),
        };
        // Declare a color space only in HDR mode; declaring on SDR flashes DWM composition.
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
                            DXGI_SWAP_CHAIN_FLAG(0),
                        )?;
                    }
                }
                if let Some(swap_chain3) = &swap_chain3 {
                    let _ =
                        declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
                }
            }
        }
        // The pass exists only for the 10-bit backbuffer it feeds.
        if swap_chain_format != DXGI_FORMAT_R10G10B10A2_UNORM {
            quantize_pass = None;
        }
        let hdr_scaled_color_management_effect = hdr_output_color_management_effect
            .is_some()
            .then(|| Self::create_pq_output_effect(&d2d_context, scrgb_color_context.as_ref()))
            .flatten();

        let mode_effects = Self::create_mode_effects(
            &d2d_context,
            hdr_mode,
            tone_map_target_nits,
            scrgb_color_context.as_ref(),
            srgb_color_context.as_ref(),
        );
        let (affine_transform_effect, dither_ordered_effect, dither_fruit_effect) =
            match Self::quantization_steps_for(swap_chain_format) {
                Some(quantization_steps) => {
                    Self::create_dither_effects(&d2d_context, &d2d_factory, quantization_steps)
                }
                None => (None, None, None),
            };

        let sampling_pass = SamplingPass::new(&d3d_device).ok();
        let mut renderer = Self {
            hdr_mode,
            bits_per_color,
            swap_chain_format,
            tone_map_target_nits,
            swap_chain,
            d3d_device,
            d3d_context,
            d2d_context,
            quantize_pass,
            sampling_pass,
            flatten_scene: None,
            scaled_scene: None,
            scene_shader_resource_view: None,
            backbuffer_render_target_view: None,
            backbuffer_size: (0, 0),
            target: None,
            image: None,
            effect_output: None,
            linear_scene_output: None,
            color_management_effect: mode_effects.color_management_effect,
            white_level_effect: mode_effects.white_level_effect,
            hdr_tone_map_effect: mode_effects.hdr_tone_map_effect,
            tone_map_normalize_effect: mode_effects.tone_map_normalize_effect,
            output_color_management_effect: mode_effects.output_color_management_effect,
            hdr_output_color_management_effect,
            hdr_scaled_color_management_effect,
            linearize_effect: mode_effects.linearize_effect,
            delinearize_effect: mode_effects.delinearize_effect,
            affine_transform_effect,
            dither_ordered_effect,
            dither_fruit_effect,
            dither_mode: DitherMode::None,
            image_storage: PixelStorage::Bgra8,
            image_source_bits_per_channel: 8,
            scrgb_color_context,
            srgb_color_context,
            source_icc_profile: None,
            source_color_context: None,
            image_display_size: (0.0, 0.0),
            image_pixel_size: (0.0, 0.0),
            scaler_description: "Lanczos / Hermite",
            dither_description: "None",
            scene_version: 0,
            maximum_resource_bytes,
        };
        renderer.create_target()?;
        Ok(renderer)
    }

    pub fn hdr_mode(&self) -> bool {
        self.hdr_mode
    }

    pub fn bits_per_color(&self) -> u32 {
        self.bits_per_color
    }

    /// True when the backbuffer is HDR10 (PQ) rather than the scRGB FP16 fallback.
    pub fn pq_output(&self) -> bool {
        self.hdr_output_color_management_effect.is_some()
    }

    /// Active backbuffer, for the info overlay.
    pub fn output_description(&self) -> &'static str {
        if self.swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM {
            if self.hdr_mode {
                "10-bit HDR10 (PQ)"
            } else {
                "10-bit sRGB"
            }
        } else if self.hdr_mode {
            "FP16 scRGB"
        } else {
            "8-bit sRGB"
        }
    }

    fn create_target(&mut self) -> Result<()> {
        let scene_format = if self.quantize_pass.is_some() {
            // D2D cannot target the 10-bit backbuffer; it draws the UNORM16 scene.
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
            self.scaled_scene = None;
            if let Some(sampling_pass) = &mut self.sampling_pass {
                sampling_pass.invalidate();
            }
            self.scene_shader_resource_view = None;
            self.backbuffer_render_target_view = None;
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

    /// Switches the output mode in place: DXGI allows one flip-model swapchain per window.
    pub fn reconfigure_output(
        &mut self,
        hdr_mode: bool,
        bits_per_color: u32,
        tone_map_target_nits: f32,
    ) -> Result<()> {
        // Adopt the target state first so a partial failure cannot retry every WM_MOVE.
        self.hdr_mode = hdr_mode;
        self.bits_per_color = bits_per_color;
        self.tone_map_target_nits = tone_map_target_nits;

        // Release every backbuffer reference ahead of ResizeBuffers.
        unsafe { self.d2d_context.SetTarget(None) };
        self.target = None;
        self.effect_output = None;
        self.linear_scene_output = None;
        self.flatten_scene = None;
        self.scaled_scene = None;
        if let Some(sampling_pass) = &mut self.sampling_pass {
            sampling_pass.invalidate();
        }
        self.scene_shader_resource_view = None;
        self.backbuffer_render_target_view = None;

        self.quantize_pass = unsafe {
            self.d2d_context
                .IsDxgiFormatSupported(DXGI_FORMAT_R16G16B16A16_UNORM)
        }
        .as_bool()
        .then(|| QuantizePass::new(&self.d3d_device).ok())
        .flatten();
        let ten_bit_target = self.quantize_pass.is_some();

        let mut hdr_output_color_management_effect = (hdr_mode && ten_bit_target)
            .then(|| {
                Self::create_pq_output_effect(&self.d2d_context, self.scrgb_color_context.as_ref())
            })
            .flatten();
        let mut swap_chain_format = if hdr_mode {
            if hdr_output_color_management_effect.is_some() {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            }
        } else if ten_bit_target && bits_per_color >= 10 {
            DXGI_FORMAT_R10G10B10A2_UNORM
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        };
        let resize_to = |swap_chain: &IDXGISwapChain1, format| unsafe {
            swap_chain.ResizeBuffers(0, 0, 0, format, DXGI_SWAP_CHAIN_FLAG(0))
        };
        if let Err(error) = resize_to(&self.swap_chain, swap_chain_format) {
            if swap_chain_format != DXGI_FORMAT_R10G10B10A2_UNORM {
                return Err(error);
            }
            // A 10-bit refusal falls back to the mode's proven format.
            hdr_output_color_management_effect = None;
            swap_chain_format = if hdr_mode {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            };
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
            // Undo any HDR declaration; SDR composition reads sRGB.
            let _ = declare_color_space(swap_chain3, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        }
        if swap_chain_format != DXGI_FORMAT_R10G10B10A2_UNORM {
            self.quantize_pass = None;
        }

        let mode_effects = Self::create_mode_effects(
            &self.d2d_context,
            hdr_mode,
            tone_map_target_nits,
            self.scrgb_color_context.as_ref(),
            self.srgb_color_context.as_ref(),
        );
        self.color_management_effect = mode_effects.color_management_effect;
        self.white_level_effect = mode_effects.white_level_effect;
        self.hdr_tone_map_effect = mode_effects.hdr_tone_map_effect;
        self.tone_map_normalize_effect = mode_effects.tone_map_normalize_effect;
        self.output_color_management_effect = mode_effects.output_color_management_effect;
        self.linearize_effect = mode_effects.linearize_effect;
        self.delinearize_effect = mode_effects.delinearize_effect;
        self.hdr_scaled_color_management_effect = hdr_output_color_management_effect
            .is_some()
            .then(|| {
                Self::create_pq_output_effect(&self.d2d_context, self.scrgb_color_context.as_ref())
            })
            .flatten();
        self.hdr_output_color_management_effect = hdr_output_color_management_effect;

        let d2d_factory: Option<ID2D1Factory1> = unsafe { self.d2d_context.GetFactory() }
            .ok()
            .and_then(|factory: ID2D1Factory| factory.cast().ok());
        (
            self.affine_transform_effect,
            self.dither_ordered_effect,
            self.dither_fruit_effect,
        ) = match (
            Self::quantization_steps_for(swap_chain_format),
            &d2d_factory,
        ) {
            (Some(quantization_steps), Some(d2d_factory)) => {
                Self::create_dither_effects(&self.d2d_context, d2d_factory, quantization_steps)
            }
            _ => (None, None, None),
        };
        self.swap_chain_format = swap_chain_format;
        self.bump_scene_version();
        self.create_target()
    }

    /// Any change that alters the flattened scene content.
    fn bump_scene_version(&mut self) {
        self.scene_version = self.scene_version.wrapping_add(1);
    }

    pub fn set_sdr_white_boost(&mut self, boost: f32) {
        if let Some(effect) = &self.white_level_effect {
            let _ = set_white_level_input(effect, SDR_REFERENCE_WHITE_NITS * boost.max(0.01));
            self.bump_scene_version();
        }
    }

    /// Updates the tone map target in place; true when it changed (monitor move).
    pub fn set_tone_map_target_nits(&mut self, nits: f32) -> bool {
        if (nits - self.tone_map_target_nits).abs() < f32::EPSILON {
            return false;
        }
        self.tone_map_target_nits = nits;
        if let Some(effect) = &self.hdr_tone_map_effect {
            let _ = unsafe {
                effect.SetValue(
                    D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &nits.to_ne_bytes(),
                )
            };
        }
        self.bump_scene_version();
        true
    }

    #[expect(clippy::too_many_arguments)]
    pub fn set_image(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        display_size: (u32, u32),
        icc_profile: Option<&[u8]>,
        storage: PixelStorage,
        source_bits_per_channel: u32,
        peak_luminance_nits: Option<f32>,
    ) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: source_pixel_format(storage),
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            ..Default::default()
        };
        let bitmap = unsafe {
            self.d2d_context.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr().cast()),
                width * storage.bytes_per_pixel(),
                &raw const properties,
            )?
        };
        self.image_display_size = (display_size.0 as f32, display_size.1 as f32);
        self.image_pixel_size = (width as f32, height as f32);
        self.image_storage = storage;
        self.image_source_bits_per_channel = source_bits_per_channel;
        self.rewire_effect_chain(&bitmap, icc_profile, storage, peak_luminance_nits);
        self.image = Some(bitmap);
        self.bump_scene_version();
        Ok(())
    }

    pub fn set_dither_mode(&mut self, mode: DitherMode) {
        self.dither_mode = mode;
    }

    /// Reuses the current bitmap wiring; callers fall back to set_image when there is none.
    pub fn update_frame_pixels(&mut self, pixels: &[u8]) -> Result<()> {
        let Some(bitmap) = &self.image else {
            return Err(windows::core::Error::empty());
        };
        let pitch = self.image_pixel_size.0 as u32 * self.image_storage.bytes_per_pixel();
        unsafe { bitmap.CopyFromMemory(None, pixels.as_ptr().cast(), pitch) }?;
        self.bump_scene_version();
        Ok(())
    }

    fn rewire_effect_chain(
        &mut self,
        bitmap: &ID2D1Bitmap1,
        icc_profile: Option<&[u8]>,
        storage: PixelStorage,
        peak_luminance_nits: Option<f32>,
    ) {
        self.effect_output = None;
        self.linear_scene_output = None;
        let Some(color_management) = &self.color_management_effect else {
            return;
        };
        // Content within SDR white skips the tone map but keeps the white boost.
        let tone_map = self
            .hdr_tone_map_effect
            .as_ref()
            .zip(peak_luminance_nits.filter(|peak| *peak > SDR_REFERENCE_WHITE_NITS));
        let scrgb_destination = self.hdr_mode || tone_map.is_some();
        // Untagged SDR already matches the undeclared sRGB swapchain.
        if storage == PixelStorage::Bgra8 && icc_profile.is_none() && !scrgb_destination {
            // Unwire the previous bitmap so the effect does not keep it alive.
            unsafe { color_management.SetInput(0, None, true) };
            return;
        }
        // FP16 pixels are linear scRGB; the embedded ICC does not describe them.
        let dedicated_context = match storage {
            PixelStorage::RgbaHalf => self.scrgb_color_context.as_ref(),
            PixelStorage::Bgra8 => None,
        };
        let source_context = match dedicated_context {
            Some(context) => context,
            None => {
                if self.source_color_context.is_none()
                    || self.source_icc_profile.as_deref() != icc_profile
                {
                    self.source_color_context = match icc_profile {
                        Some(profile) => unsafe {
                            self.d2d_context
                                .CreateColorContext(D2D1_COLOR_SPACE_CUSTOM, Some(profile))
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
        let destination_context = if scrgb_destination {
            &self.scrgb_color_context
        } else {
            &self.srgb_color_context
        };
        let Some(destination_context) = destination_context else {
            return;
        };
        let wired = unsafe {
            color_management.SetValue(
                D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(source_context),
            )
        }
        .is_ok()
            && unsafe {
                color_management.SetValue(
                    D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
                    D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                    &interface_property_bytes(destination_context),
                )
            }
            .is_ok();
        if !wired {
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
                unsafe { tone_map_effect.SetInput(0, &converted, true) };
                let tone_mapped = unsafe { tone_map_effect.GetOutput() }.ok();
                if self.hdr_mode {
                    // Absolute luminance: no SDR white boost after tone mapping.
                    tone_mapped
                } else {
                    tone_mapped.and_then(|tone_mapped| {
                        let normalize = self.tone_map_normalize_effect.as_ref()?;
                        let output_encoding = self.output_color_management_effect.as_ref()?;
                        // Reinterpret scene-referred (80 nits) as display-referred white.
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
                        unsafe { output_encoding.SetInput(0, &normalized, true) };
                        unsafe { output_encoding.GetOutput() }.ok()
                    })
                }
            }
            None => match &self.white_level_effect {
                Some(white_level) => {
                    unsafe { white_level.SetInput(0, &converted, true) };
                    unsafe { white_level.GetOutput() }.ok()
                }
                None => Some(converted),
            },
        };
        // The HDR10 backbuffer quantizes PQ; encode after every linear stage.
        self.effect_output = match (&self.hdr_output_color_management_effect, scene) {
            (Some(output_encoding), Some(scene)) => {
                unsafe { output_encoding.SetInput(0, &scene, true) };
                let encoded = unsafe { output_encoding.GetOutput() }.ok();
                if encoded.is_some() {
                    // Kept so the scaler convolves linear light and encodes after.
                    self.linear_scene_output = Some(scene);
                }
                encoded
            }
            (None, scene) => scene,
            (Some(_), None) => None,
        };
    }

    pub fn clear_image(&mut self) {
        self.image = None;
        self.effect_output = None;
        self.linear_scene_output = None;
    }

    pub fn render(
        &mut self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
        custom_scaling: bool,
        clear_color: D2D1_COLOR_F,
        draw_overlay: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
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
        // 90/270 fold into the flatten as a lossless permutation; the pass then
        // sees the axis-aligned remainder in rotated-image space.
        let separable = if matrix[1] == 0.0 && matrix[2] == 0.0 {
            Some((transform, false))
        } else if matrix[0] == 0.0 && matrix[3] == 0.0 {
            let source_height = self.image_pixel_size.1.round();
            Some((
                Matrix3x2 {
                    M11: -transform.M21,
                    M12: 0.0,
                    M21: 0.0,
                    M22: transform.M12,
                    M31: source_height * transform.M21 + transform.M31,
                    M32: transform.M32,
                },
                true,
            ))
        } else {
            None
        };
        // 1:1 on whole pixels resamples nothing; leave the pixels untouched.
        let identity_placement = separable.as_ref().is_some_and(|(effective, _)| {
            (effective.M11.abs() - 1.0).abs() < 1e-6
                && (effective.M22.abs() - 1.0).abs() < 1e-6
                && (effective.M31 - effective.M31.round()).abs() < 1e-4
                && (effective.M32 - effective.M32.round()).abs() < 1e-4
        });
        let prescaled = if custom_scaling
            && !identity_placement
            && self.image.is_some()
            && let Some((effective, rotated)) = &separable
        {
            self.prepare_scaled_scene(effective, *rotated)
        } else {
            None
        };
        if custom_scaling && self.image.is_some() && prescaled.is_none() {
            self.scaler_description = if identity_placement {
                "None (1:1)"
            } else {
                "High Quality (fallback)"
            };
        }
        // An active effect chain leaves fractional pixels even at 1:1.
        let pixels_transformed = !identity_placement || self.effect_output.is_some();
        self.dither_description =
            if self.image.is_some() && self.active_dither_effect(pixels_transformed).is_some() {
                match self.dither_mode {
                    DitherMode::None => "None",
                    DitherMode::Ordered => "Ordered",
                    DitherMode::Fruit => "Fruit",
                }
            } else {
                "None"
            };
        unsafe {
            self.d2d_context.BeginDraw();
            self.d2d_context.Clear(Some(&raw const clear_color));
            if self.image.is_some() {
                match (&prescaled, &self.effect_output, &self.image) {
                    (Some(scaled), _, _) => {
                        if let Some(composite) = self.prescaled_composite_image(scaled)
                            && !self.draw_prescaled_dithered(&composite)
                        {
                            self.d2d_context.DrawImage(
                                &composite,
                                None,
                                None,
                                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                                D2D1_COMPOSITE_MODE_SOURCE_OVER,
                            );
                        }
                    }
                    (None, Some(output), _) => {
                        // Dither must run in destination pixel space (identity context transform).
                        if !self.draw_image_dithered(
                            output,
                            &transform,
                            interpolation,
                            pixels_transformed,
                        ) {
                            self.d2d_context.SetTransform(&raw const transform);
                            self.d2d_context.DrawImage(
                                output,
                                None,
                                None,
                                interpolation,
                                D2D1_COMPOSITE_MODE_SOURCE_OVER,
                            );
                            self.d2d_context.SetTransform(&Matrix3x2::identity());
                        }
                    }
                    // Untouched pixels, or no effect support.
                    (None, None, Some(image)) => {
                        let dithered = image.cast::<ID2D1Image>().is_ok_and(|bitmap_image| {
                            self.draw_image_dithered(
                                &bitmap_image,
                                &transform,
                                interpolation,
                                pixels_transformed,
                            )
                        });
                        if !dithered {
                            let destination = D2D_RECT_F {
                                left: 0.0,
                                top: 0.0,
                                right: self.image_pixel_size.0,
                                bottom: self.image_pixel_size.1,
                            };
                            self.d2d_context.SetTransform(&raw const transform);
                            self.d2d_context.DrawBitmap(
                                image,
                                Some(&raw const destination),
                                1.0,
                                interpolation,
                                None,
                                None,
                            );
                            self.d2d_context.SetTransform(&Matrix3x2::identity());
                        }
                    }
                    _ => {}
                }
            }
            // Overlay failure must not block presenting the frame.
            let overlay_result = draw_overlay(&self.d2d_context);
            self.d2d_context.EndDraw(None, None)?;
            if let (Some(quantize_pass), Some(scene), Some(backbuffer)) = (
                &self.quantize_pass,
                &self.scene_shader_resource_view,
                &self.backbuffer_render_target_view,
            ) {
                quantize_pass.draw(
                    &self.d3d_context,
                    scene,
                    backbuffer,
                    self.backbuffer_size.0,
                    self.backbuffer_size.1,
                );
            }
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            overlay_result
        }
    }

    fn ensure_flatten_scene(&mut self, size: (u32, u32)) -> Result<()> {
        if self
            .flatten_scene
            .as_ref()
            .is_some_and(|held| held.size == size)
        {
            return Ok(());
        }
        self.flatten_scene = None;
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        let texture = crate::view::create_scene_texture(
            &self.d3d_device,
            size,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        )?;
        unsafe {
            let mut shader_resource_view = None;
            self.d3d_device.CreateShaderResourceView(
                &texture,
                None,
                Some(&raw mut shader_resource_view),
            )?;
            let surface: IDXGISurface = texture.cast()?;
            let target = self
                .d2d_context
                .CreateBitmapFromDxgiSurface(&surface, Some(&raw const properties))?;
            self.flatten_scene = Some(FlattenScene {
                shader_resource_view: shader_resource_view
                    .ok_or_else(windows::core::Error::empty)?,
                target,
                size,
                version: self.scene_version.wrapping_sub(1),
                rotated: false,
            });
        }
        Ok(())
    }

    fn ensure_scaled_scene(&mut self, size: (u32, u32)) -> Result<()> {
        if self
            .scaled_scene
            .as_ref()
            .is_some_and(|held| held.size == size)
        {
            return Ok(());
        }
        self.scaled_scene = None;
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let texture = crate::view::create_scene_texture(
            &self.d3d_device,
            size,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        )?;
        unsafe {
            let mut render_target_view = None;
            self.d3d_device.CreateRenderTargetView(
                &texture,
                None,
                Some(&raw mut render_target_view),
            )?;
            let surface: IDXGISurface = texture.cast()?;
            let bitmap = self
                .d2d_context
                .CreateBitmapFromDxgiSurface(&surface, Some(&raw const properties))?;
            self.scaled_scene = Some(ScaledScene {
                render_target_view: render_target_view.ok_or_else(windows::core::Error::empty)?,
                bitmap,
                size,
            });
        }
        Ok(())
    }

    /// Flattens the effect chain within the budget, then convolves it to backbuffer space.
    fn prepare_scaled_scene(
        &mut self,
        transform: &Matrix3x2,
        rotated: bool,
    ) -> Option<ID2D1Bitmap1> {
        self.sampling_pass.as_ref()?;
        if transform.M11 == 0.0 || transform.M22 == 0.0 {
            return None;
        }
        let pixel_size = (
            self.image_pixel_size.0.round() as u32,
            self.image_pixel_size.1.round() as u32,
        );
        let source_size = if rotated {
            (pixel_size.1, pixel_size.0)
        } else {
            pixel_size
        };
        let target_size = self.backbuffer_size;
        if source_size.0 == 0 || source_size.1 == 0 || target_size.0 == 0 || target_size.1 == 0 {
            return None;
        }
        // Sources past the budget flatten reduced; the convolution restores them.
        const FLATTEN_BYTES_PER_PIXEL: u64 = 8;
        let budget_pixels = self.maximum_resource_bytes / FLATTEN_BYTES_PER_PIXEL;
        let source_pixels = u64::from(source_size.0) * u64::from(source_size.1);
        let flatten_size = if source_pixels <= budget_pixels {
            source_size
        } else {
            let ratio = (budget_pixels as f64 / source_pixels as f64).sqrt();
            (
                ((f64::from(source_size.0) * ratio) as u32).max(1),
                ((f64::from(source_size.1) * ratio) as u32).max(1),
            )
        };
        let reduction = (
            flatten_size.0 as f32 / source_size.0 as f32,
            flatten_size.1 as f32 / source_size.1 as f32,
        );
        let reduced = flatten_size != source_size;
        self.ensure_flatten_scene(flatten_size).ok()?;
        self.ensure_scaled_scene(target_size).ok()?;
        // The flatten only depends on scene content and quadrant; pan and zoom reuse it.
        let flatten_current = self.flatten_scene.as_ref().is_some_and(|flatten| {
            flatten.version == self.scene_version && flatten.rotated == rotated
        });
        if !flatten_current {
            let flatten = self.flatten_scene.as_ref()?;
            unsafe {
                self.d2d_context.SetTarget(&flatten.target);
                self.d2d_context.BeginDraw();
                self.d2d_context.Clear(None);
                if rotated || reduced {
                    let placement = if rotated {
                        Matrix3x2 {
                            M11: 0.0,
                            M12: reduction.1,
                            M21: -reduction.0,
                            M22: 0.0,
                            M31: flatten_size.0 as f32,
                            M32: 0.0,
                        }
                    } else {
                        Matrix3x2 {
                            M11: reduction.0,
                            M12: 0.0,
                            M21: 0.0,
                            M22: reduction.1,
                            M31: 0.0,
                            M32: 0.0,
                        }
                    };
                    self.d2d_context.SetTransform(&raw const placement);
                }
                // The HDR10 chain ends in PQ; flatten the linear scene, encode after scaling.
                let scene: Option<ID2D1Image> = if self.hdr_scaled_color_management_effect.is_some()
                    && let Some(linear_scene) = &self.linear_scene_output
                {
                    Some(linear_scene.clone())
                } else {
                    match (&self.effect_output, &self.image) {
                        (Some(output), _) => Some(output.clone()),
                        (None, Some(image)) => image.cast().ok(),
                        _ => None,
                    }
                };
                if let Some(scene) = scene {
                    // SDR scenes linearize so the convolution runs in linear light.
                    let source = if !self.hdr_mode
                        && self.delinearize_effect.is_some()
                        && let Some(linearize) = &self.linearize_effect
                    {
                        linearize.SetInput(0, &scene, true);
                        linearize.GetOutput().ok().unwrap_or(scene)
                    } else {
                        scene
                    };
                    self.d2d_context.DrawImage(
                        &source,
                        None,
                        None,
                        if reduced {
                            D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
                        } else {
                            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                        },
                        D2D1_COMPOSITE_MODE_SOURCE_OVER,
                    );
                }
                if rotated || reduced {
                    self.d2d_context.SetTransform(&Matrix3x2::identity());
                }
                let finished = self.d2d_context.EndDraw(None, None);
                match &self.target {
                    Some(target) => self.d2d_context.SetTarget(target),
                    None => self.d2d_context.SetTarget(None),
                }
                finished.ok()?;
            }
            if let Some(flatten) = &mut self.flatten_scene {
                flatten.version = self.scene_version;
                flatten.rotated = rotated;
            }
        }
        // Output pixel -> flatten texel; the reduction folds into the mapping.
        let horizontal = AxisMapping {
            position_scale: reduction.0 / transform.M11,
            position_offset: -transform.M31 * reduction.0 / transform.M11,
        };
        let vertical = AxisMapping {
            position_scale: reduction.1 / transform.M22,
            position_offset: -transform.M32 * reduction.1 / transform.M22,
        };
        let source_view = self.flatten_scene.as_ref()?.shader_resource_view.clone();
        let scaled = self.scaled_scene.as_ref()?;
        let render_target_view = scaled.render_target_view.clone();
        let bitmap = scaled.bitmap.clone();
        self.sampling_pass
            .as_mut()?
            .scale(
                &self.d3d_device,
                &self.d3d_context,
                &source_view,
                flatten_size,
                &render_target_view,
                target_size,
                horizontal,
                vertical,
            )
            .ok()?;
        self.scaler_description = if horizontal.filter_name() == vertical.filter_name() {
            horizontal.filter_name()
        } else {
            "Lanczos + Hermite"
        };
        Some(bitmap)
    }

    pub fn scaler_description(&self) -> &'static str {
        self.scaler_description
    }

    /// What dithering the last frame actually got, for the info panel.
    pub fn dither_description(&self) -> &'static str {
        self.dither_description
    }

    /// The registered dither effect; untouched 1:1 draws within the backbuffer depth skip it.
    fn active_dither_effect(&self, pixels_transformed: bool) -> Option<&ID2D1Effect> {
        let backbuffer_bits = if self.swap_chain_format == DXGI_FORMAT_R10G10B10A2_UNORM {
            10
        } else {
            8
        };
        if !pixels_transformed && self.image_source_bits_per_channel <= backbuffer_bits {
            return None;
        }
        match self.dither_mode {
            DitherMode::None => None,
            DitherMode::Ordered => self.dither_ordered_effect.as_ref(),
            DitherMode::Fruit => self.dither_fruit_effect.as_ref(),
        }
    }

    /// Applies the deferred PQ encode or the SDR delinearization before compositing.
    fn prescaled_composite_image(&self, scaled: &ID2D1Bitmap1) -> Option<ID2D1Image> {
        if self.linear_scene_output.is_some()
            && let Some(output_encoding) = &self.hdr_scaled_color_management_effect
        {
            unsafe { output_encoding.SetInput(0, scaled, true) };
            if let Ok(output) = unsafe { output_encoding.GetOutput() } {
                return Some(output);
            }
        }
        if !self.hdr_mode
            && self.linearize_effect.is_some()
            && let Some(delinearize) = &self.delinearize_effect
        {
            unsafe { delinearize.SetInput(0, scaled, true) };
            if let Ok(output) = unsafe { delinearize.GetOutput() } {
                return Some(output);
            }
        }
        scaled.cast().ok()
    }

    /// Prescaled scene -> dither -> target; false when the caller draws plain.
    fn draw_prescaled_dithered(&self, composite: &ID2D1Image) -> bool {
        let Some(dither_effect) = self.active_dither_effect(true) else {
            return false;
        };
        unsafe { dither_effect.SetInput(0, composite, true) };
        let Ok(dithered) = (unsafe { dither_effect.GetOutput() }) else {
            return false;
        };
        unsafe {
            self.d2d_context.DrawImage(
                &dithered,
                None,
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
        }
        true
    }

    /// Scene -> affine -> dither -> target; false when unavailable, the caller draws undithered.
    fn draw_image_dithered(
        &self,
        output: &ID2D1Image,
        transform: &Matrix3x2,
        interpolation: D2D1_INTERPOLATION_MODE,
        pixels_transformed: bool,
    ) -> bool {
        let (Some(dither_effect), Some(affine_transform)) = (
            self.active_dither_effect(pixels_transformed),
            &self.affine_transform_effect,
        ) else {
            return false;
        };
        let wired = unsafe {
            affine_transform.SetValue(
                D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0 as u32,
                D2D1_PROPERTY_TYPE_MATRIX_3X2,
                &matrix_property_bytes(transform),
            )
        }
        .is_ok()
            && unsafe {
                // The affine interpolation enum shares the interpolation mode values.
                affine_transform.SetValue(
                    D2D1_2DAFFINETRANSFORM_PROP_INTERPOLATION_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &interpolation.0.to_ne_bytes(),
                )
            }
            .is_ok();
        if !wired {
            return false;
        }
        unsafe { affine_transform.SetInput(0, output, true) };
        let Ok(scaled) = (unsafe { affine_transform.GetOutput() }) else {
            return false;
        };
        unsafe { dither_effect.SetInput(0, &scaled, true) };
        let Ok(dithered) = (unsafe { dither_effect.GetOutput() }) else {
            return false;
        };
        unsafe {
            self.d2d_context.DrawImage(
                &dithered,
                None,
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
        }
        true
    }
}

fn matrix_property_bytes(matrix: &Matrix3x2) -> [u8; 24] {
    let elements = [
        matrix.M11, matrix.M12, matrix.M21, matrix.M22, matrix.M31, matrix.M32,
    ];
    let mut bytes = [0u8; 24];
    for (index, element) in elements.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&element.to_ne_bytes());
    }
    bytes
}
