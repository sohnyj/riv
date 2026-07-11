//! Color helpers: scRGB conversion and display capability queries.

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

pub fn output_color(color: D2D1_COLOR_F, scrgb_boost: Option<f32>) -> D2D1_COLOR_F {
    match scrgb_boost {
        Some(boost) => srgb_color_to_scrgb(color, boost),
        None => color,
    }
}

/// sRGB-encoded color to linear scRGB, times the SDR white boost.
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

/// SDR white boost for HDR mode; 1.0 elsewhere (ACM output is display-referred).
pub fn sdr_white_boost(window: HWND) -> f32 {
    if !monitor_is_hdr(window) {
        return 1.0;
    }
    query_sdr_white_boost(window).unwrap_or(1.0)
}

/// True when the window's output reports the G2084 (HDR) color space.
pub fn monitor_is_hdr(window: HWND) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    window_output_description(window).is_some_and(|description| {
        description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
    })
}

/// Maximum luminance (nits) of the window's output, when reported.
pub fn display_maximum_luminance(window: HWND) -> Option<f32> {
    window_output_description(window)
        .map(|description| description.MaxLuminance)
        .filter(|luminance| *luminance > 0.0)
}

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
        // Bit 0x2 = advancedColorEnabled.
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
