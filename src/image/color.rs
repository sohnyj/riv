//! scRGB conversion, perceptual quantizer curves, and display capability queries.

use std::sync::Arc;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Display::{AdvancedColorInfo, AdvancedColorKind, DisplayInformation};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{IInspectable, Interface};

/// DisplayInformation's desktop interop, absent from the crate; IInspectable slots keep the vtable.
#[allow(non_snake_case)]
mod interop {
    use windows::Win32::Foundation::HWND;
    use windows::core::{GUID, HRESULT};

    #[windows::core::interface("7449121C-382B-4705-8DA7-A795BA482013")]
    pub unsafe trait IDisplayInformationStaticsInterop: windows::core::IUnknown {
        pub unsafe fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
        pub unsafe fn GetRuntimeClassName(&self, name: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
        pub unsafe fn GetForWindow(
            &self,
            window: HWND,
            riid: *const GUID,
            information: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetForMonitor(
            &self,
            monitor: *mut core::ffi::c_void,
            riid: *const GUID,
            information: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
    }
}
use interop::IDisplayInformationStaticsInterop;

/// Encoding of app-drawn colors (overlay, clear) for the current backbuffer.
#[derive(Clone, Copy, PartialEq)]
pub enum OutputColorTarget {
    Srgb,
    ScrgbLinear { sdr_white_boost: f32 },
}

pub fn output_color(color: D2D1_COLOR_F, target: OutputColorTarget) -> D2D1_COLOR_F {
    match target {
        OutputColorTarget::Srgb => color,
        OutputColorTarget::ScrgbLinear { sdr_white_boost } => {
            srgb_color_to_scrgb(color, sdr_white_boost)
        }
    }
}

/// The sRGB electro-optical transfer function: encoded value to linear light.
pub(crate) fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB-encoded color to linear scRGB, times the SDR white boost.
fn srgb_color_to_scrgb(color: D2D1_COLOR_F, sdr_white_boost: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: srgb_to_linear(color.r) * sdr_white_boost,
        g: srgb_to_linear(color.g) * sdr_white_boost,
        b: srgb_to_linear(color.b) * sdr_white_boost,
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

/// Output capabilities from one advanced-color snapshot; unknown state falls back to SDR.
#[derive(Clone, Copy)]
pub struct DisplayCapabilities {
    pub hdr: bool,
    /// Wire depth of the display path, shown in the info panel; never picks formats.
    pub bits_per_color: u32,
    pub maximum_luminance_nits: Option<f32>,
    pub maximum_full_frame_luminance_nits: Option<f32>,
    /// Advanced color (HDR or SDR auto color management) is on for this output.
    pub advanced_color: bool,
    /// SDR white over the 80-nit reference; meaningful on HDR outputs only.
    pub sdr_white_boost: f32,
}

/// The BT.2100 reference white, the SDR tone-map target.
const SDR_TONE_MAP_TARGET_NITS: f32 = 203.0;
/// HDR tone-map target when the monitor reports no peak luminance.
const HDR_PEAK_FALLBACK_NITS: f32 = 600.0;

impl DisplayCapabilities {
    /// Matches DISPLAYCONFIG_ADVANCED_COLOR_MODE (SDR/WCG/HDR) from the existing signals.
    pub fn color_mode_label(&self) -> &'static str {
        if self.hdr {
            "HDR"
        } else if self.advanced_color {
            "WCG"
        } else {
            "SDR"
        }
    }

    /// The tone-map target and paired full-frame limit for this display.
    pub fn tone_map_targets(&self) -> (f32, f32) {
        let target = if self.hdr {
            self.maximum_luminance_nits
                .unwrap_or(HDR_PEAK_FALLBACK_NITS)
        } else {
            SDR_TONE_MAP_TARGET_NITS
        };
        let full_frame = if self.hdr {
            self.maximum_full_frame_luminance_nits.unwrap_or(target)
        } else {
            target
        };
        (target, full_frame)
    }

    /// Display headroom over current SDR white for gain map weighting, capped by the sustained full-frame limit; None without a peak report.
    pub fn gain_map_headroom(&self) -> Option<f32> {
        if !self.hdr {
            return None;
        }
        let peak = self.maximum_luminance_nits?;
        let ceiling = match self.maximum_full_frame_luminance_nits {
            Some(full_frame) => peak.min(full_frame),
            None => peak,
        };
        Some(ceiling / (SDR_REFERENCE_WHITE_NITS * self.sdr_white_boost.max(0.01)))
    }

    /// SDR white boost for the given output; 1.0 outside HDR (ACM output is display-referred).
    pub fn sdr_white_boost_for(&self, hdr_output: bool) -> f32 {
        if hdr_output {
            self.sdr_white_boost
        } else {
            1.0
        }
    }
}

/// A display's color capabilities, native gamut, and installed profile, from one snapshot.
pub struct DisplayColor {
    pub capabilities: DisplayCapabilities,
    pub gamut: Option<DisplayGamut>,
    /// The display's ICC profile, the destination when the OS color-manages nothing.
    pub display_profile: Option<Arc<Vec<u8>>>,
}

/// The window's display information hook: advanced-color snapshots plus a change message.
pub struct DisplayWatcher {
    display_information: DisplayInformation,
    change_token: i64,
}

impl DisplayWatcher {
    /// Hooks `window`'s display information; advanced-color changes post `message` to it.
    pub fn new(window: HWND, message: u32) -> Option<Self> {
        let interop =
            windows::core::factory::<DisplayInformation, IDisplayInformationStaticsInterop>()
                .ok()?;
        let mut pointer: *mut core::ffi::c_void = core::ptr::null_mut();
        unsafe { interop.GetForWindow(window, &DisplayInformation::IID, &raw mut pointer) }
            .ok()
            .ok()?;
        let display_information = unsafe { DisplayInformation::from_raw(pointer) };
        let window_value = window.0 as isize;
        let handler = TypedEventHandler::<DisplayInformation, IInspectable>::new(move |_, _| {
            let window = HWND(window_value as *mut core::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(window), message, WPARAM(0), LPARAM(0)) };
            Ok(())
        });
        let change_token = display_information
            .AdvancedColorInfoChanged(&handler)
            .ok()?;
        Some(Self {
            display_information,
            change_token,
        })
    }

    fn advanced_color_info(&self) -> Option<AdvancedColorInfo> {
        self.display_information.GetAdvancedColorInfo().ok()
    }
}

impl Drop for DisplayWatcher {
    fn drop(&mut self) {
        let _ = self
            .display_information
            .RemoveAdvancedColorInfoChanged(self.change_token);
    }
}

/// Queries the display's color capabilities, gamut, and profile from the watcher's snapshot.
pub fn display_color(watcher: Option<&DisplayWatcher>, window: HWND) -> DisplayColor {
    let information = watcher.and_then(DisplayWatcher::advanced_color_info);
    let capabilities = capabilities_from(information.as_ref(), window);
    // Only ACM-off SDR consumes the profile; skip the disk read in the delegated modes.
    let display_profile = (!capabilities.hdr && !capabilities.advanced_color)
        .then(|| monitor_device_profile(window))
        .flatten()
        .map(Arc::new);
    DisplayColor {
        capabilities,
        gamut: information.as_ref().and_then(gamut_from),
        display_profile,
    }
}

/// The ICC profile Windows associates with the window's monitor.
fn monitor_device_profile(window: HWND) -> Option<Vec<u8>> {
    for_window_display_path(window, display_path_profile)
}

/// The display path's default ICC profile, in the scope the display currently uses.
fn display_path_profile(path: &DISPLAYCONFIG_PATH_INFO) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::UI::ColorSystem::{
        CPST_NONE, CPT_ICC, ColorProfileGetDisplayDefault, ColorProfileGetDisplayUserScope,
        WCS_PROFILE_MANAGEMENT_SCOPE_SYSTEM_WIDE,
    };

    let adapter = path.targetInfo.adapterId;
    let source = path.sourceInfo.id;
    let scope = unsafe { ColorProfileGetDisplayUserScope(adapter, source) }
        .unwrap_or(WCS_PROFILE_MANAGEMENT_SCOPE_SYSTEM_WIDE);
    let name = unsafe { ColorProfileGetDisplayDefault(scope, adapter, source, CPT_ICC, CPST_NONE) }
        .ok()?;
    let profile = unsafe { name.to_string() }.ok();
    let _ = unsafe { LocalFree(Some(HLOCAL(name.0.cast()))) };
    let profile = profile.filter(|name| !name.is_empty())?;
    // A bare file name lives in the system color directory; a full path stands alone.
    let profile = std::path::Path::new(&profile);
    if profile.is_absolute() {
        std::fs::read(profile).ok()
    } else {
        std::fs::read(color_directory()?.join(profile)).ok()
    }
}

/// The system color directory, where installed ICC profiles live.
fn color_directory() -> Option<std::path::PathBuf> {
    use windows::Win32::UI::ColorSystem::GetColorDirectoryW;
    use windows::core::{PCWSTR, PWSTR};

    let mut length = 0u32;
    let _ = unsafe { GetColorDirectoryW(PCWSTR::null(), None, &raw mut length) };
    let mut buffer = vec![0u16; (length as usize).div_ceil(2)];
    unsafe {
        GetColorDirectoryW(
            PCWSTR::null(),
            Some(PWSTR(buffer.as_mut_ptr())),
            &raw mut length,
        )
    }
    .as_bool()
    .then_some(())?;
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(crate::text::path_from_wide(&buffer[..end]))
}

fn capabilities_from(information: Option<&AdvancedColorInfo>, window: HWND) -> DisplayCapabilities {
    let kind = information.and_then(|information| information.CurrentAdvancedColorKind().ok());
    let hdr = kind == Some(AdvancedColorKind::HighDynamicRange);
    let nits = |value: Option<f32>| value.filter(|nits| *nits > 0.0);
    DisplayCapabilities {
        hdr,
        bits_per_color: bits_per_color(window).unwrap_or(8),
        maximum_luminance_nits: nits(
            information.and_then(|information| information.MaxLuminanceInNits().ok()),
        ),
        maximum_full_frame_luminance_nits: nits(
            information
                .and_then(|information| information.MaxAverageFullFrameLuminanceInNits().ok()),
        ),
        advanced_color: hdr || kind == Some(AdvancedColorKind::WideColorGamut),
        sdr_white_boost: nits(
            information.and_then(|information| information.SdrWhiteLevelInNits().ok()),
        )
        .map_or(1.0, |white| white / SDR_REFERENCE_WHITE_NITS),
    }
}

/// Advanced-color mode, EDID gamut label, and wire depth of the display, for the info overlay.
#[derive(Clone, Copy, PartialEq)]
pub struct DisplayLabels {
    pub color_mode: &'static str,
    pub gamut: &'static str,
    pub bits_per_color: u32,
}

impl DisplayLabels {
    pub fn new(capabilities: &DisplayCapabilities, gamut: Option<DisplayGamut>) -> Self {
        Self {
            color_mode: capabilities.color_mode_label(),
            gamut: gamut.map_or("unknown", |gamut| gamut.label()),
            bits_per_color: capabilities.bits_per_color,
        }
    }

    /// The display line mirrors the output line's "[bits] [gamut]" form.
    pub fn display_description(&self) -> String {
        format!("{}-bit {}", self.bits_per_color, self.gamut)
    }
}

/// The display's color primaries (CIE xy), from EDID; for the WCG diagnostic overlay.
#[derive(Clone, Copy)]
pub struct DisplayGamut {
    pub red: [f32; 2],
    pub green: [f32; 2],
    pub blue: [f32; 2],
}

impl DisplayGamut {
    /// Nearest known gamut by primary distance; the tell is whether it is wider than sRGB.
    pub fn label(&self) -> &'static str {
        nearest_gamut_label([self.red, self.green, self.blue])
    }

    /// True when EDID carried real chromaticities rather than zeros.
    pub fn is_known(&self) -> bool {
        [self.red, self.green, self.blue]
            .iter()
            .flatten()
            .any(|coordinate| *coordinate > 0.0)
    }
}

/// R, G, B primaries (CIE xy) of the gamuts riv can name; sRGB shares BT.709's.
pub(crate) const BT709_PRIMARIES: [[f32; 2]; 3] = [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060]];
pub(crate) const DISPLAY_P3_PRIMARIES: [[f32; 2]; 3] =
    [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]];
pub(crate) const BT2020_PRIMARIES: [[f32; 2]; 3] = [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]];

/// Nearest reference gamut label by R/G/B primary (xy) distance.
pub(crate) fn nearest_gamut_label(measured: [[f32; 2]; 3]) -> &'static str {
    const REFERENCES: [(&str, [[f32; 2]; 3]); 4] = [
        ("sRGB", BT709_PRIMARIES),
        (
            "Adobe RGB",
            [[0.640, 0.330], [0.210, 0.710], [0.150, 0.060]],
        ),
        ("DCI-P3", DISPLAY_P3_PRIMARIES),
        ("BT.2020", BT2020_PRIMARIES),
    ];
    let distance = |reference: &[[f32; 2]; 3]| -> f32 {
        reference
            .iter()
            .zip(measured)
            .map(|(target, actual)| {
                (target[0] - actual[0]).powi(2) + (target[1] - actual[1]).powi(2)
            })
            .sum()
    };
    let mut best = (REFERENCES[0].0, distance(&REFERENCES[0].1));
    for (name, reference) in &REFERENCES[1..] {
        let candidate = distance(reference);
        if candidate < best.1 {
            best = (*name, candidate);
        }
    }
    best.0
}

/// The display's native primaries, when the snapshot carries real chromaticities.
fn gamut_from(information: &AdvancedColorInfo) -> Option<DisplayGamut> {
    let point = |point: windows::Foundation::Point| [point.X, point.Y];
    let gamut = DisplayGamut {
        red: point(information.RedPrimary().ok()?),
        green: point(information.GreenPrimary().ok()?),
        blue: point(information.BluePrimary().ok()?),
    };
    gamut.is_known().then_some(gamut)
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

/// Wire depth of the window's display, from the active DXGI output description.
fn bits_per_color(window: HWND) -> Option<u32> {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    // A fresh factory each query: enumeration snapshots go stale across display changes.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            if let Ok(description) = output
                .cast::<IDXGIOutput6>()
                .and_then(|output6| unsafe { output6.GetDesc1() })
                && description.Monitor == monitor
            {
                return Some(description.BitsPerColor).filter(|bits| *bits > 0);
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    None
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
    }
}
