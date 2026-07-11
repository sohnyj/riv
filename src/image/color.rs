//! 색 관리 보조 (SPEC §7, PORTING_PLAN §5 색 관리) — sRGB → linear scRGB 변환과
//! SDR 백레벨(HDR 모드) 조회. 소스 프로파일 → ColorManagement 이펙트 배선은 렌더러가,
//! ICC 바이트 추출은 각 디코드 어댑터가 담당한다.

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};

/// 타깃 모드별 클리어·브러시 색 (SPEC §7 A안 — 스왑체인 모드 매칭):
/// `scrgb_boost = Some(boost)` = HDR의 FP16 scRGB 타깃(선형화 × SDR 백레벨),
/// `None` = SDR/ACM의 B8G8R8A8 타깃(sRGB 인코딩 원값 그대로).
pub fn output_color(color: D2D1_COLOR_F, scrgb_boost: Option<f32>) -> D2D1_COLOR_F {
    match scrgb_boost {
        Some(boost) => srgb_color_to_scrgb(color, boost),
        None => color,
    }
}

/// sRGB 인코딩 색 → linear scRGB(× SDR 백레벨 배율) — FP16 타깃의 클리어·브러시 색 공용.
/// DWM은 scRGB 1.0을 SDR 화이트(80 nits)로 매핑하므로 HDR 모드에서만 boost > 1 (SPEC §7).
fn srgb_color_to_scrgb(color: D2D1_COLOR_F, sdr_white_boost: f32) -> D2D1_COLOR_F {
    let linearize = |encoded: f32| {
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    };
    D2D1_COLOR_F {
        r: linearize(color.r) * sdr_white_boost,
        g: linearize(color.g) * sdr_white_boost,
        b: linearize(color.b) * sdr_white_boost,
        a: color.a,
    }
}

/// 창이 있는 모니터의 SDR 백레벨 배율 (SPEC §7) — **HDR 모드(scene-referred)에서만**
/// `DISPLAYCONFIG_SDR_WHITE_LEVEL`(1000 = 80 nits) 기반. SDR advanced color(ACM)는
/// display-referred(1.0 = 디스플레이 참조 백)라 부스트 비대상, 조회 실패도 1.0.
pub fn sdr_white_boost(window: HWND) -> f32 {
    if !monitor_is_hdr(window) {
        return 1.0;
    }
    query_sdr_white_boost(window).unwrap_or(1.0)
}

/// HDR 활성 판별 — `IDXGIOutput6::GetDesc1().ColorSpace == G2084` (문서 권장 Win32 경로.
/// SDR advanced color 디스플레이는 G22_P709로 보고되어 자연히 제외된다).
/// 스왑체인 모드 매칭(A안)의 분기 기준 — 렌더러 구축·재구축 시 조회 (SPEC §7).
pub fn monitor_is_hdr(window: HWND) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    window_output_description(window).is_some_and(|description| {
        description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
    })
}

/// 창 모니터의 최대 휘도(nits) — HdrToneMap의 OutputMaxLuminance (SPEC §7 Q6).
/// 조회 실패(wine 등)는 None — 호출자가 모드별 기본값을 정한다.
pub fn display_maximum_luminance(window: HWND) -> Option<f32> {
    window_output_description(window)
        .map(|description| description.MaxLuminance)
        .filter(|luminance| *luminance > 0.0)
}

/// 창이 있는 모니터의 DXGI 출력 정보 (IDXGIOutput6::GetDesc1)
fn window_output_description(
    window: HWND,
) -> Option<windows::Win32::Graphics::Dxgi::DXGI_OUTPUT_DESC1> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::core::Interface;

    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.ok()?;
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        adapter_index += 1;
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            let Ok(description) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if description.Monitor != monitor {
                continue;
            }
            return output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|output6| unsafe { output6.GetDesc1() }.ok());
        }
    }
    None
}

fn query_sdr_white_boost(window: HWND) -> Option<f32> {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut monitor_information = MONITORINFOEXW::default();
    monitor_information.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(monitor, &mut monitor_information.monitorInfo) }
        .as_bool()
        .then_some(())?;
    let device_name = monitor_information.szDevice;

    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    } != ERROR_SUCCESS
    {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }

    for path in &paths[..path_count as usize] {
        // 경로의 GDI 소스 이름 ↔ 창 모니터 매칭
        let mut source_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source_name.header) } != 0
            || source_name.viewGdiDeviceName != device_name
        {
            continue;
        }
        // advanced color(HDR) 활성 여부 — 비활성이면 boost 없음
        let mut advanced_color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut advanced_color.header) } != 0 {
            return None;
        }
        // 비트 0x2 = advancedColorEnabled
        if unsafe { advanced_color.Anonymous.value } & 0x2 == 0 {
            return None;
        }
        let mut white_level = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut white_level.header) } != 0
            || white_level.SDRWhiteLevel == 0
        {
            return None;
        }
        return Some(white_level.SDRWhiteLevel as f32 / 1000.0);
    }
    None
}
