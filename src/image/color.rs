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

/// Encoding of app-drawn colors (overlay, clear) for the current backbuffer.
#[derive(Clone, Copy)]
pub enum OutputColorTarget {
    Srgb,
    ScrgbLinear { sdr_white_boost: f32 },
    Pq { sdr_white_boost: f32 },
}

pub fn output_color(color: D2D1_COLOR_F, target: OutputColorTarget) -> D2D1_COLOR_F {
    match target {
        OutputColorTarget::Srgb => color,
        OutputColorTarget::ScrgbLinear { sdr_white_boost } => {
            srgb_color_to_scrgb(color, sdr_white_boost)
        }
        OutputColorTarget::Pq { sdr_white_boost } => {
            scrgb_color_to_pq(srgb_color_to_scrgb(color, sdr_white_boost))
        }
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

/// scRGB 1.0, the luminance DWM assigns to sRGB white.
pub const SDR_REFERENCE_WHITE_NITS: f32 = 80.0;
/// PQ code 1.0.
const PQ_PEAK_NITS: f32 = 10000.0;

/// SMPTE ST 2084 constants, shared by the encode and decode directions.
const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// SMPTE ST 2084 inverse EOTF (nits -> code).
pub fn perceptual_quantizer_code(nits: f32) -> f32 {
    let normalized = (nits.max(0.0) / PQ_PEAK_NITS).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * normalized) / (1.0 + PQ_C3 * normalized)).powf(PQ_M2)
}

/// SMPTE ST 2084 EOTF (code -> nits).
pub fn perceptual_quantizer_nits(code: f32) -> f32 {
    let power = code.max(0.0).powf(1.0 / PQ_M2);
    let numerator = (power - PQ_C1).max(0.0);
    let denominator = PQ_C2 - PQ_C3 * power;
    PQ_PEAK_NITS * (numerator / denominator).powf(1.0 / PQ_M1)
}

/// BT.709 -> BT.2020 primaries (rows sum to 1).
const BT709_TO_BT2020: [[f32; 3]; 3] = [
    [0.627_404, 0.329_283, 0.043_313],
    [0.069_097, 0.919_540, 0.011_362],
    [0.016_391, 0.088_013, 0.895_595],
];

/// Linear scRGB (BT.709, 1.0 = 80 nits) to PQ-encoded BT.2020 (HDR10 backbuffer).
fn scrgb_color_to_pq(color: D2D1_COLOR_F) -> D2D1_COLOR_F {
    let source = [color.r, color.g, color.b];
    let mut encoded = [0.0f32; 3];
    for (row, channel) in BT709_TO_BT2020.iter().zip(&mut encoded) {
        let linear = row[0] * source[0] + row[1] * source[1] + row[2] * source[2];
        *channel = perceptual_quantizer_code((linear * SDR_REFERENCE_WHITE_NITS).min(PQ_PEAK_NITS));
    }
    D2D1_COLOR_F {
        r: encoded[0],
        g: encoded[1],
        b: encoded[2],
        a: color.a,
    }
}

/// Output capabilities from a single enumeration; unknown output falls back to SDR 8-bit.
#[derive(Clone, Copy)]
pub struct DisplayCapabilities {
    pub hdr: bool,
    pub bits_per_color: u32,
    pub max_luminance: Option<f32>,
    pub max_full_frame_luminance: Option<f32>,
    /// Advanced color (HDR or SDR auto color management) is on for this output.
    pub advanced_color: bool,
}

/// The window output's HDR mode, bit depth and peak luminance in one query.
pub fn display_capabilities(window: HWND) -> DisplayCapabilities {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let Some(description) = window_output_description(window) else {
        return DisplayCapabilities {
            hdr: false,
            bits_per_color: 8,
            max_luminance: None,
            max_full_frame_luminance: None,
            advanced_color: false,
        };
    };
    DisplayCapabilities {
        hdr: description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        bits_per_color: description.BitsPerColor,
        max_luminance: (description.MaxLuminance > 0.0).then_some(description.MaxLuminance),
        max_full_frame_luminance: (description.MaxFullFrameLuminance > 0.0)
            .then_some(description.MaxFullFrameLuminance),
        advanced_color: advanced_color_enabled(window),
    }
}

/// The display's color primaries (CIE xy), from EDID; for the WCG diagnostic overlay.
#[derive(Clone, Copy)]
pub struct DisplayGamut {
    pub red: [f32; 2],
    pub green: [f32; 2],
    pub blue: [f32; 2],
    pub white: [f32; 2],
}

impl DisplayGamut {
    /// Nearest known gamut by primary distance; the tell is whether it is wider than sRGB.
    pub fn label(&self) -> &'static str {
        // R, G, B primaries (xy) of the reference gamuts.
        const REFERENCES: [(&str, [[f32; 2]; 3]); 4] = [
            ("sRGB", [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060]]),
            (
                "Adobe RGB",
                [[0.640, 0.330], [0.210, 0.710], [0.150, 0.060]],
            ),
            ("DCI-P3", [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]]),
            ("BT.2020", [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]]),
        ];
        let measured = [self.red, self.green, self.blue];
        let mut best = ("unknown", f32::MAX);
        for (name, reference) in REFERENCES {
            let distance: f32 = reference
                .iter()
                .zip(measured)
                .map(|(target, actual)| {
                    (target[0] - actual[0]).powi(2) + (target[1] - actual[1]).powi(2)
                })
                .sum();
            if distance < best.1 {
                best = (name, distance);
            }
        }
        best.0
    }

    /// True when EDID carried real chromaticities rather than zeros.
    pub fn is_known(&self) -> bool {
        [self.red, self.green, self.blue, self.white]
            .iter()
            .flatten()
            .any(|coordinate| *coordinate > 0.0)
    }
}

/// The window output's EDID primaries, when the driver reports them.
pub fn display_gamut(window: HWND) -> Option<DisplayGamut> {
    let description = window_output_description(window)?;
    let gamut = DisplayGamut {
        red: description.RedPrimary,
        green: description.GreenPrimary,
        blue: description.BluePrimary,
        white: description.WhitePoint,
    };
    gamut.is_known().then_some(gamut)
}

/// SDR white boost given the known HDR state; 1.0 outside HDR (ACM output is display-referred).
pub fn sdr_white_boost_for(window: HWND, hdr: bool) -> f32 {
    if !hdr {
        return 1.0;
    }
    query_sdr_white_boost(window).unwrap_or(1.0)
}

/// Peak and sustained full-frame luminance (nits) of the window's output, from one enumeration.
pub fn display_luminance_limits(window: HWND) -> (Option<f32>, Option<f32>) {
    window_output_description(window).map_or((None, None), |description| {
        (
            (description.MaxLuminance > 0.0).then_some(description.MaxLuminance),
            (description.MaxFullFrameLuminance > 0.0).then_some(description.MaxFullFrameLuminance),
        )
    })
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

/// Applies `read` to the active display path driving `window`'s monitor.
fn for_window_display_path<T>(
    window: HWND,
    read: impl Fn(&DISPLAYCONFIG_PATH_INFO) -> Option<T>,
) -> Option<T> {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut monitor_information = MONITORINFOEXW::default();
    monitor_information.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe { GetMonitorInfoW(monitor, &raw mut monitor_information.monitorInfo) }
        .as_bool()
        .then_some(())?;
    let device_name = monitor_information.szDevice;

    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if unsafe {
        GetDisplayConfigBufferSizes(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            &raw mut mode_count,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            paths.as_mut_ptr(),
            &raw mut mode_count,
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
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut source_name.header) } != 0
            || source_name.viewGdiDeviceName != device_name
        {
            continue;
        }
        return read(path);
    }
    None
}

/// The advanced-color flags for `path`'s target (bit 0x2 = advancedColorEnabled).
fn advanced_color_flags(path: &DISPLAYCONFIG_PATH_INFO) -> Option<u32> {
    let mut advanced_color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            size: size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    (unsafe { DisplayConfigGetDeviceInfo(&raw mut advanced_color.header) } == 0)
        .then_some(unsafe { advanced_color.Anonymous.value })
}

/// True when advanced color (HDR, or SDR auto color management) is on for the window's display.
pub fn advanced_color_enabled(window: HWND) -> bool {
    for_window_display_path(window, advanced_color_flags).is_some_and(|flags| flags & 0x2 != 0)
}

fn query_sdr_white_boost(window: HWND) -> Option<f32> {
    for_window_display_path(window, |path| {
        // Bit 0x2 = advancedColorEnabled; the SDR white level is meaningful only then.
        if advanced_color_flags(path)? & 0x2 == 0 {
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
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut white_level.header) } != 0
            || white_level.SDRWhiteLevel == 0
        {
            return None;
        }
        Some(white_level.SDRWhiteLevel as f32 / 1000.0)
    })
}

#[cfg(test)]
mod output_color_tests {
    use super::*;

    const TOLERANCE: f32 = 1e-3;

    fn gray(value: f32) -> D2D1_COLOR_F {
        D2D1_COLOR_F {
            r: value,
            g: value,
            b: value,
            a: 1.0,
        }
    }

    #[test]
    fn pq_code_matches_reference_points() {
        assert!(perceptual_quantizer_code(0.0).abs() < TOLERANCE);
        assert!((perceptual_quantizer_code(10000.0) - 1.0).abs() < TOLERANCE);
        // 100 nits, the HDR reference white anchor of ST 2084.
        assert!((perceptual_quantizer_code(100.0) - 0.5081).abs() < TOLERANCE);
        // The directions invert each other.
        assert!((perceptual_quantizer_nits(0.5081) - 100.0).abs() < 0.1);
    }

    #[test]
    fn bt2020_matrix_preserves_white() {
        for row in BT709_TO_BT2020 {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < TOLERANCE);
        }
    }

    #[test]
    fn output_color_encodes_srgb_white_per_target() {
        let white = gray(1.0);
        let srgb = output_color(white, OutputColorTarget::Srgb);
        assert!((srgb.r - 1.0).abs() < TOLERANCE);
        let scrgb = output_color(
            white,
            OutputColorTarget::ScrgbLinear {
                sdr_white_boost: 2.5,
            },
        );
        assert!((scrgb.g - 2.5).abs() < TOLERANCE);
        // 80 nits in PQ; equal channels stay equal through the matrix.
        let pq = output_color(
            white,
            OutputColorTarget::Pq {
                sdr_white_boost: 1.0,
            },
        );
        assert!((pq.r - 0.4859).abs() < TOLERANCE);
        assert!((pq.r - pq.b).abs() < TOLERANCE);
    }
}
