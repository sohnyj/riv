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

/// sRGB 인코딩 색 → linear scRGB(× SDR 백레벨 배율) — FP16 타깃의 클리어·브러시 색 공용.
/// DWM은 scRGB 1.0을 SDR 화이트(80 nits)로 매핑하므로 HDR 모드에서만 boost > 1 (SPEC §7).
pub fn srgb_color_to_scrgb(color: D2D1_COLOR_F, sdr_white_boost: f32) -> D2D1_COLOR_F {
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

/// 창이 있는 모니터의 SDR 백레벨 배율 (SPEC §7) — advanced color(HDR) 활성일 때만
/// `DISPLAYCONFIG_SDR_WHITE_LEVEL`(1000 = 80 nits) 기반, 그 외·조회 실패는 1.0.
pub fn sdr_white_boost(window: HWND) -> f32 {
    query_sdr_white_boost(window).unwrap_or(1.0)
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
        let advanced_color_enabled = unsafe { advanced_color.Anonymous.value } & 0x2 != 0;
        if !advanced_color_enabled {
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
