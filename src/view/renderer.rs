//! D3D11 디바이스 + DXGI 플립 스왑체인 + D2D 드로우 경로
//! (SPEC §3.1·§3.3·§7, PORTING_PLAN §3 렌더러 세부 — 커스텀 셰이더 0)
//!
//! 백버퍼 = **스왑체인 모드 매칭(A안, 2026-07-11 확정)** — 상시 FP16 선언(구 B안)이
//! SDR/ACM에서 DWM 컴포지션 전환(전역 Mica 변화·플래시)을 유발해 모드별로 나눈다:
//! - **HDR(G2084)**: `R16G16B16A16_FLOAT` + `SetColorSpace1`(RGB_FULL_G10 scRGB).
//!   SDR 백레벨은 `WhiteLevelAdjustment` 이펙트(Input=SDRWhite, Output=80 —
//!   ×SdrWhite/80 부스트), 클리어·브러시 색은 CPU 선형화×부스트.
//! - **SDR/ACM**: `B8G8R8A8_UNORM` + 색공간 무선언(DWM이 sRGB로 간주 — ACM 매핑 포함).
//!   백레벨 이펙트 없음, 클리어·브러시 색은 sRGB 원값.
//!
//! 소스 비트맵(sRGB 인코딩 PBGRA8)은 draw 시점에 `ColorManagement` 이펙트로 대상
//! 색공간(HDR=scRGB, SDR=sRGB)으로 변환한다 (SPEC §7). 이펙트 중간 버퍼는 FP16 강제 —
//! 기본 정밀도(입력 기준 8bpc)면 백레벨 부스트(>1.0)가 클램프된다(실기 확인 2026-07-11,
//! 사설 텍스처라 컴포지션 플래시와 무관 — 상시 유지). 모니터 이동·HDR 토글로 모드가
//! 바뀌면 호출자(main)가 렌더러를 재구축한다.
//! 구 UNORM 비트 심도 감지(모니터별 R10G10B10A2 매칭)는 제거.

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
    D2D1_COMPOSITE_MODE_SOURCE_OVER, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1ColorManagement, CLSID_D2D1WhiteLevelAdjustment, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_BUFFER_PRECISION_16BPC_FLOAT, D2D1_COLOR_SPACE_CUSTOM, D2D1_COLOR_SPACE_SCRGB,
    D2D1_COLOR_SPACE_SRGB, D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_PROP_QUALITY, D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT,
    D2D1_COLORMANAGEMENT_QUALITY_BEST, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE, D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
    D2D1_PROPERTY_TYPE_ENUM, D2D1_PROPERTY_TYPE_FLOAT,
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

pub struct Renderer {
    /// 구축 시점의 스왑체인 모드 (A안) — 현재 모니터와 불일치하면 재구축 대상
    hdr_mode: bool,
    swap_chain: IDXGISwapChain1,
    d2d_context: ID2D1DeviceContext,
    target: Option<ID2D1Bitmap1>,
    image: Option<ID2D1Bitmap1>,
    /// 이펙트 체인 최종 출력 — ColorManagement → 백레벨 스케일
    effect_output: Option<ID2D1Image>,
    /// sRGB → scRGB 변환 이펙트 (SPEC §7) — wine 등 미지원 환경은 None(DrawBitmap 직행)
    color_management_effect: Option<ID2D1Effect>,
    /// HDR 모드 SDR 백레벨 보정(WhiteLevelAdjustment) (SPEC §7) — 미지원 환경은 None
    white_level_effect: Option<ID2D1Effect>,
    /// 현재 소스 프로파일 캐시 — 애니메이션 프레임 재업로드 시 재생성 회피
    source_icc_profile: Option<Vec<u8>>,
    source_color_context: Option<ID2D1ColorContext>,
    /// 드로우 논리 크기(원본 픽셀) — DP3 다운스케일 시 비트맵보다 크다 (SPEC §3.4)
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
            Some(&mut device),
            None,
            None,
        )?;
    }
    Ok(device.expect("D3D11CreateDevice succeeded without device"))
}

/// 소스 비트맵 픽셀 포맷 — sRGB 인코딩 premultiplied BGRA8 (SPEC §3.1)
fn source_pixel_format() -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    }
}

/// IUnknown 계열 이펙트 프로퍼티 값 — raw 인터페이스 포인터 바이트
fn interface_property_bytes<T: Interface>(interface: &T) -> [u8; size_of::<usize>()] {
    (interface.as_raw() as usize).to_ne_bytes()
}

/// SDR 참조 화이트 = 80 nits (D2D1_SCENE_REFERRED_SDR_WHITE_LEVEL)
const SDR_REFERENCE_WHITE_NITS: f32 = 80.0;

/// WhiteLevelAdjustment의 입력 백레벨(nits) 설정 — 문서 표(SDR 콘텐츠·FP16·HDR):
/// Input = SDRWhite, Output = 80 고정. 이펙트는 Input/Output 비율로 곱한다.
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
    /// `hdr_mode` = 창이 있는 모니터의 HDR(G2084) 여부 (A안 — 호출자가
    /// `color::monitor_is_hdr`로 조회, 모드 변경 시 재구축)
    pub fn new(window: HWND, width: u32, height: u32, hdr_mode: bool) -> Result<Self> {
        // WARP 폴백은 런타임 위임(P7) — 하드웨어 실패 시 1회 재시도만
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
            factory.CreateSwapChainForHwnd(&d3d_device, window, &description, None, None)?
        };
        // HDR만 scRGB 색공간 선언 — SDR/ACM은 무선언(DWM이 sRGB로 간주·ACM 매핑 포함,
        // FP16+scRGB 상시 선언은 컴포지션 전환 플래시 유발 — A안 2026-07-11).
        // wine 등 IDXGISwapChain3 미지원 환경은 무시(P16 — 형상 확인만)
        if hdr_mode && let Ok(swap_chain3) = swap_chain.cast::<IDXGISwapChain3>() {
            let _ = unsafe { swap_chain3.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709) };
        }

        let d2d_context = unsafe {
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };
        // 이펙트 중간 버퍼를 FP16으로 — 기본(입력 비트맵 기준 8bpc UNORM)이면 백레벨
        // 부스트(>1.0)가 중간 버퍼에서 [0,1] 클램프된다 (실기 HDR 어두움 원인 후보,
        // 2026-07-11 — 오버레이(이펙트 미경유)만 밝던 관찰과 부합)
        unsafe {
            let mut rendering_controls = d2d_context.GetRenderingControls();
            rendering_controls.bufferPrecision = D2D1_BUFFER_PRECISION_16BPC_FLOAT;
            d2d_context.SetRenderingControls(&rendering_controls);
        }

        // 이펙트·색 컨텍스트 — 미지원 환경(wine)은 None으로 DrawBitmap 직행 (P16).
        // 변환 대상 = 타깃 색공간 (A안: HDR=scRGB, SDR/ACM=sRGB)
        let destination_color_context = unsafe {
            d2d_context.CreateColorContext(
                if hdr_mode {
                    D2D1_COLOR_SPACE_SCRGB
                } else {
                    D2D1_COLOR_SPACE_SRGB
                },
                None,
            )
        }
        .ok();
        let color_management_effect = destination_color_context.as_ref().and_then(|destination| {
            let effect = unsafe { d2d_context.CreateEffect(&CLSID_D2D1ColorManagement) }.ok()?;
            // 품질 BEST 필수 — 부동소수점 정밀도·scRGB 색공간 지원 조건. 기본(NORMAL)이면
            // scRGB 변환이 적용되지 않아 이중 감마 인코딩(실기 washed-out, 2026-07-11 확인)
            unsafe {
                effect.SetValue(
                    D2D1_COLORMANAGEMENT_PROP_QUALITY.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &D2D1_COLORMANAGEMENT_QUALITY_BEST.0.to_ne_bytes(),
                )
            }
            .ok()?;
            // 색 컨텍스트 프로퍼티 타입은 COLOR_CONTEXT — IUNKNOWN으로 지정하면 타입
            // 불일치로 SetValue가 실패해 체인 전체가 폴백된다 (실기 washed-out의 원인,
            // 2026-07-11 확인)
            unsafe {
                effect.SetValue(
                    D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
                    D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                    &interface_property_bytes(destination),
                )
            }
            .ok()?;
            Some(effect)
        });
        // SDR 백레벨 보정 — HDR 모드 전용(Input=SDRWhite(부스트 반영), Output=80 고정,
        // SPEC §7). SDR/ACM은 display-referred라 비대상 (A안 — 이펙트 자체를 생성 안 함)
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

        let mut renderer = Self {
            hdr_mode,
            swap_chain,
            d2d_context,
            target: None,
            image: None,
            effect_output: None,
            color_management_effect,
            white_level_effect,
            source_icc_profile: None,
            source_color_context: None,
            image_display_size: (0.0, 0.0),
            image_pixel_size: (0.0, 0.0),
        };
        renderer.create_target()?;
        Ok(renderer)
    }

    /// 구축 모드 — 현재 모니터의 HDR 여부와 다르면 호출자가 재구축 (A안)
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
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))?;
            self.d2d_context.SetTarget(&target);
            self.target = Some(target);
        }
        Ok(())
    }

    /// WM_SIZE에서 동기 호출 — 백버퍼 재생성 (호출자가 즉시 재렌더)
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

    /// HDR 모드의 SDR 백레벨 반영 (SPEC §7) — boost = SDRWhiteLevel/1000, 1.0 = SDR/ACM
    /// (스케일 항등). 이펙트 프로퍼티는 라이브 — 다음 draw부터 반영된다.
    pub fn set_sdr_white_boost(&mut self, boost: f32) {
        if let Some(effect) = &self.white_level_effect {
            let _ = set_white_level_input(effect, SDR_REFERENCE_WHITE_NITS * boost.max(0.01));
        }
    }

    /// premultiplied BGRA8(sRGB 인코딩) 픽셀 업로드 (SPEC §3.1) + 이펙트 체인 재배선.
    /// `display_size` = 논리(원본) 크기, `icc_profile` = 소스 프로파일(없으면 sRGB 가정)
    pub fn set_image(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        display_size: (u32, u32),
        icc_profile: Option<&[u8]>,
    ) -> Result<()> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: source_pixel_format(),
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            ..Default::default()
        };
        let bitmap = unsafe {
            self.d2d_context.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(pixels.as_ptr().cast()),
                width * 4,
                &properties,
            )?
        };
        self.image_display_size = (display_size.0 as f32, display_size.1 as f32);
        self.image_pixel_size = (width as f32, height as f32);
        self.rewire_effect_chain(&bitmap, icc_profile);
        self.image = Some(bitmap);
        Ok(())
    }

    /// 소스 프로파일 → ID2D1ColorContext → ColorManagement → 백레벨 스케일 (SPEC §7)
    fn rewire_effect_chain(&mut self, bitmap: &ID2D1Bitmap1, icc_profile: Option<&[u8]>) {
        self.effect_output = None;
        let Some(color_management) = &self.color_management_effect else {
            return;
        };
        // 프로파일이 바뀔 때만 소스 색 컨텍스트 재생성 (애니메이션 프레임 재업로드 대비)
        if self.source_color_context.is_none() || self.source_icc_profile.as_deref() != icc_profile
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
        let wired = unsafe {
            color_management.SetValue(
                D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &interface_property_bytes(source_context),
            )
        }
        .is_ok();
        if !wired {
            return;
        }
        unsafe { color_management.SetInput(0, bitmap, true) };
        self.effect_output = match &self.white_level_effect {
            Some(white_level) => unsafe {
                color_management.GetOutput().ok().and_then(|converted| {
                    white_level.SetInput(0, &converted, true);
                    white_level.GetOutput().ok()
                })
            },
            None => unsafe { color_management.GetOutput().ok() },
        };
    }

    /// 디코드 실패 등으로 표시 이미지 제거 (에러 텍스트만 남김 — SPEC §3.6)
    pub fn clear_image(&mut self) {
        self.image = None;
        self.effect_output = None;
    }

    /// Clear → SetTransform → DrawImage(이펙트 체인) → 오버레이(같은 패스) → Present.
    /// `clear_color`는 타깃 모드 색 — HDR=linear scRGB, SDR=sRGB 원값
    /// (변환은 호출자 — image/color.rs `output_color`)
    pub fn render(
        &mut self,
        matrix: [f32; 6],
        interpolation: D2D1_INTERPOLATION_MODE,
        clear_color: D2D1_COLOR_F,
        draw_overlay: impl FnOnce(&ID2D1DeviceContext) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            self.d2d_context.BeginDraw();
            self.d2d_context.Clear(Some(&clear_color));
            if self.image.is_some() {
                // 비트맵 픽셀 → 논리 크기 스케일을 변환에 합성 (DP3 — DrawImage는
                // 대상 사각형이 없어 행렬로 처리)
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
                self.d2d_context.SetTransform(&transform);
                match (&self.effect_output, &self.image) {
                    (Some(output), _) => {
                        self.d2d_context.DrawImage(
                            output,
                            None,
                            None,
                            interpolation,
                            D2D1_COMPOSITE_MODE_SOURCE_OVER,
                        );
                    }
                    (None, Some(image)) => {
                        // 이펙트 미지원 환경(wine) — 변환 없이 직접 드로우 (P16)
                        let destination = D2D_RECT_F {
                            left: 0.0,
                            top: 0.0,
                            right: self.image_pixel_size.0,
                            bottom: self.image_pixel_size.1,
                        };
                        self.d2d_context.DrawBitmap(
                            image,
                            Some(&destination),
                            1.0,
                            interpolation,
                            None,
                            None,
                        );
                    }
                    _ => {}
                }
                self.d2d_context.SetTransform(&Matrix3x2::identity());
            }
            // 오버레이 실패는 프레임 제시를 막지 않는다 — EndDraw·Present 후 전파
            let overlay_result = draw_overlay(&self.d2d_context);
            self.d2d_context.EndDraw(None, None)?;
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            overlay_result
        }
    }
}
