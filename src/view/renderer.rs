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
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
    D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT, D2D1_PROPERTY_TYPE_MATRIX_3X2,
    D2D1_WHITELEVELADJUSTMENT_PROP_INPUT_WHITE_LEVEL,
    D2D1_WHITELEVELADJUSTMENT_PROP_OUTPUT_WHITE_LEVEL, D2D1CreateFactory, ID2D1Bitmap1,
    ID2D1ColorContext, ID2D1DeviceContext, ID2D1Effect, ID2D1Factory1, ID2D1Image,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGISurface, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::core::{Interface, Result};
use windows_numerics::Matrix3x2;

use crate::image::decode::PixelStorage;
use crate::view::dither::{self, DitherMode};

pub struct Renderer {
    hdr_mode: bool,
    tone_map_target_nits: f32,
    swap_chain: IDXGISwapChain1,
    d2d_context: ID2D1DeviceContext,
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    effect_output: Option<ID2D1Image>,
    color_management_effect: Option<ID2D1Effect>,
    white_level_effect: Option<ID2D1Effect>,
    hdr_tone_map_effect: Option<ID2D1Effect>,
    tone_map_normalize_effect: Option<ID2D1Effect>,
    output_color_management_effect: Option<ID2D1Effect>,
    affine_transform_effect: Option<ID2D1Effect>,
    dither_ordered_effect: Option<ID2D1Effect>,
    dither_fruit_effect: Option<ID2D1Effect>,
    dither_mode: DitherMode,
    image_storage: PixelStorage,
    scrgb_color_context: Option<ID2D1ColorContext>,
    srgb_color_context: Option<ID2D1ColorContext>,
    source_icc_profile: Option<Vec<u8>>,
    source_color_context: Option<ID2D1ColorContext>,
    image_display_size: (f32, f32),
    image_pixel_size: (f32, f32),
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
            Some(&raw mut device),
            None,
            None,
        )?;
    }
    Ok(device.expect("D3D11CreateDevice succeeded without device"))
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

/// scRGB 1.0 (D2D scene-referred SDR white).
const SDR_REFERENCE_WHITE_NITS: f32 = 80.0;

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
        tone_map_target_nits: f32,
    ) -> Result<Self> {
        let d3d_device = create_d3d_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_d3d_device(D3D_DRIVER_TYPE_WARP))?;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;

        let swap_chain = unsafe {
            let adapter = dxgi_device.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let description = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: if hdr_mode {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
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
            )?
        };
        // Declare scRGB only in HDR mode; declaring it on SDR flashes DWM composition.
        if hdr_mode && let Ok(swap_chain3) = swap_chain.cast::<IDXGISwapChain3>() {
            let _ = unsafe { swap_chain3.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709) };
        }

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

        let scrgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SCRGB, None) }.ok();
        let srgb_color_context =
            unsafe { d2d_context.CreateColorContext(D2D1_COLOR_SPACE_SRGB, None) }.ok();
        // BEST quality is required for float precision and scRGB conversions.
        let create_color_management = || {
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
        };
        let color_management_effect = create_color_management();
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
                let effect = create_color_management()?;
                unsafe {
                    effect.SetValue(
                        D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
                        D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                        &interface_property_bytes(scrgb_color_context.as_ref()?),
                    )
                }
                .ok()?;
                unsafe {
                    effect.SetValue(
                        D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
                        D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                        &interface_property_bytes(srgb_color_context.as_ref()?),
                    )
                }
                .ok()?;
                Some(effect)
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
        // Output dither is SDR-only; failure here leaves rendering undithered.
        let (affine_transform_effect, dither_ordered_effect, dither_fruit_effect) =
            if !hdr_mode && dither::register_dither_effects(&d2d_factory).is_ok() {
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
            } else {
                (None, None, None)
            };

        let mut renderer = Self {
            hdr_mode,
            tone_map_target_nits,
            swap_chain,
            d2d_context,
            target: None,
            image: None,
            effect_output: None,
            color_management_effect,
            white_level_effect,
            hdr_tone_map_effect,
            tone_map_normalize_effect,
            output_color_management_effect,
            affine_transform_effect,
            dither_ordered_effect,
            dither_fruit_effect,
            dither_mode: DitherMode::None,
            image_storage: PixelStorage::Bgra8,
            scrgb_color_context,
            srgb_color_context,
            source_icc_profile: None,
            source_color_context: None,
            image_display_size: (0.0, 0.0),
            image_pixel_size: (0.0, 0.0),
        };
        renderer.create_target()?;
        Ok(renderer)
    }

    pub fn hdr_mode(&self) -> bool {
        self.hdr_mode
    }

    fn create_target(&mut self) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: if self.hdr_mode {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
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

    pub fn set_sdr_white_boost(&mut self, boost: f32) {
        if let Some(effect) = &self.white_level_effect {
            let _ = set_white_level_input(effect, SDR_REFERENCE_WHITE_NITS * boost.max(0.01));
        }
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
        self.rewire_effect_chain(&bitmap, icc_profile, storage, peak_luminance_nits);
        self.image = Some(bitmap);
        Ok(())
    }

    pub fn set_dither_mode(&mut self, mode: DitherMode) {
        self.dither_mode = mode;
    }

    fn rewire_effect_chain(
        &mut self,
        bitmap: &ID2D1Bitmap1,
        icc_profile: Option<&[u8]>,
        storage: PixelStorage,
        peak_luminance_nits: Option<f32>,
    ) {
        self.effect_output = None;
        let Some(color_management) = &self.color_management_effect else {
            return;
        };
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
        // Content within SDR white skips the tone map but keeps the white boost.
        let tone_map = self
            .hdr_tone_map_effect
            .as_ref()
            .zip(peak_luminance_nits.filter(|peak| *peak > SDR_REFERENCE_WHITE_NITS));
        let destination_context = if self.hdr_mode || tone_map.is_some() {
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
        self.effect_output = match tone_map {
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
    }

    pub fn clear_image(&mut self) {
        self.image = None;
        self.effect_output = None;
    }

    pub fn render(
        &mut self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
        clear_color: D2D1_COLOR_F,
        draw_overlay: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            self.d2d_context.BeginDraw();
            self.d2d_context.Clear(Some(&raw const clear_color));
            if self.image.is_some() {
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
                match (&self.effect_output, &self.image) {
                    (Some(output), _) => {
                        // Dither runs in destination pixel space: the transform moves
                        // into the graph and the context transform stays identity.
                        if !self.draw_image_dithered(output, &transform, interpolation) {
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
                    // No effect support (e.g. wine): draw the bitmap directly.
                    (None, Some(image)) => {
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
                    _ => {}
                }
            }
            // Overlay failure must not block presenting the frame.
            let overlay_result = draw_overlay(&self.d2d_context);
            self.d2d_context.EndDraw(None, None)?;
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            overlay_result
        }
    }

    /// SDR high-depth output path: scene -> 2DAffineTransform -> dither -> target.
    /// Returns false when unavailable so the caller draws the undithered path.
    fn draw_image_dithered(
        &self,
        output: &ID2D1Image,
        transform: &Matrix3x2,
        interpolation: D2D1_INTERPOLATION_MODE,
    ) -> bool {
        if self.hdr_mode || self.image_storage != PixelStorage::RgbaHalf {
            return false;
        }
        let dither_effect = match self.dither_mode {
            DitherMode::None => return false,
            DitherMode::Ordered => &self.dither_ordered_effect,
            DitherMode::Fruit => &self.dither_fruit_effect,
        };
        let (Some(dither_effect), Some(affine_transform)) =
            (dither_effect, &self.affine_transform_effect)
        else {
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
