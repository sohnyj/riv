//! 오버레이 — 정보 패널·줌 필·에러 텍스트 (SPEC §3.6, P8: D2D/DirectWrite).
//!
//! 이미지와 **같은 D2D 패스**에서 그려 qView의 "오버레이가 RHI에 가려짐" 문제를
//! 구조적으로 제거한다(렌더러가 DrawBitmap 뒤에 호출). 폰트는 Segoe UI(R12).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Globalization::{DATE_SHORTDATE, GetDateFormatEx, GetTimeFormatEx};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ROUNDED_RECT, ID2D1DeviceContext,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_METRICS, DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::core::{Result, w};
use windows_numerics::Vector2;

use crate::image::color;
use crate::image::decode::DecodedImage;

const PANEL_MARGIN: f32 = 12.0;
const PANEL_PADDING_X: f32 = 12.0;
const PANEL_PADDING_Y: f32 = 8.0;
const PANEL_CORNER_RADIUS: f32 = 8.0;
/// 반투명 검정 rgba(0,0,0,165) (SPEC §3.6)
const PANEL_BACKGROUND: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 165.0 / 255.0,
};
const WHITE: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};
const BLACK: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

/// 렌더 패스에 넘기는 오버레이 내용 스냅샷
pub struct OverlayContent {
    /// 에러 텍스트 — "Error occurred opening\n<파일명>\n<사유> (Error <코드>)"
    pub error_text: Option<String>,
    /// 정보 패널 본문 (필드 8종 조립 완료 — SPEC §3.6)
    pub info_text: Option<String>,
    /// 줌 필 텍스트 ("Zoom: N%" 등, 1초 자동 숨김은 창 타이머가 관리)
    pub zoom_pill_text: Option<String>,
    /// 배경 perceived brightness > 0.5 → 에러 글자 검정 (SPEC §3.6)
    pub background_is_bright: bool,
    /// 브러시 색 보정 (SPEC §7 A안) — Some(boost)=HDR FP16 scRGB(선형화×백레벨),
    /// None=SDR/ACM B8G8R8A8(sRGB 원값)
    pub scrgb_boost: Option<f32>,
}

pub struct Overlay {
    text_format: IDWriteTextFormat,
    error_format: IDWriteTextFormat,
    dwrite_factory: IDWriteFactory,
    /// 창 DPI 배율 — D2D 타깃이 96 DPI 고정이라 치수·폰트를 물리 픽셀로 보정
    /// (R7 — 150% DPI 과소 표시 수정, SPEC R12·§3.6)
    scale: f32,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let (text_format, error_format) = create_text_formats(&dwrite_factory, 1.0)?;
        Ok(Self {
            text_format,
            error_format,
            dwrite_factory,
            scale: 1.0,
        })
    }

    /// 창 DPI 변경 반영 — 텍스트 포맷 재생성 (WM_DPICHANGED·시작 시)
    pub fn set_scale(&mut self, scale: f32) {
        let scale = scale.max(0.5);
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        if let Ok((text_format, error_format)) = create_text_formats(&self.dwrite_factory, scale) {
            self.text_format = text_format;
            self.error_format = error_format;
            self.scale = scale;
        }
    }

    /// 렌더러의 D2D 패스 내부에서 호출 (BeginDraw~EndDraw 사이, 변환 identity)
    pub fn draw(
        &self,
        context: &ID2D1DeviceContext,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
    ) -> Result<()> {
        let boost = content.scrgb_boost;
        let margin = PANEL_MARGIN * self.scale;
        let panel_gap = 8.0 * self.scale;
        let info_rect = if let Some(info_text) = &content.info_text {
            Some(self.draw_panel(context, info_text, margin, margin, viewport_width, boost)?)
        } else {
            None
        };
        if let Some(pill_text) = &content.zoom_pill_text {
            // 줌 필 = 상단 중앙 (2026-07-10 — qView의 좌측 배치는 Qt 중앙 정렬 제약의
            // 우회였음. DWrite 메트릭 기반 정밀 중앙 배치). 정보 패널과 겹치면 그 아래로.
            let pill_width = self.measure_panel_width(pill_text, viewport_width)?;
            let centered_left = ((viewport_width - pill_width) / 2.0).max(margin);
            let top = match &info_rect {
                Some(info) if centered_left < info.right + panel_gap => info.bottom + panel_gap,
                _ => margin,
            };
            self.draw_panel(
                context,
                pill_text,
                centered_left,
                top,
                viewport_width,
                boost,
            )?;
        }
        if let Some(error_text) = &content.error_text {
            self.draw_error_text(
                context,
                error_text,
                viewport_width,
                viewport_height,
                content,
            )?;
        }
        Ok(())
    }

    fn panel_layout(&self, text: &str, viewport_width: f32) -> Result<IDWriteTextLayout> {
        self.create_layout(
            text,
            &self.text_format,
            (viewport_width - (PANEL_MARGIN * 2.0 + PANEL_PADDING_X * 2.0) * self.scale).max(0.0),
        )
    }

    /// 패널 전체 폭(패딩 포함) 측정 — 중앙 배치 계산용
    fn measure_panel_width(&self, text: &str, viewport_width: f32) -> Result<f32> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics)? };
        Ok(metrics.width + PANEL_PADDING_X * 2.0 * self.scale)
    }

    /// 반투명 라운드 패널 + 흰 글자. 반환 = 패널 사각형.
    fn draw_panel(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        left: f32,
        top: f32,
        viewport_width: f32,
        scrgb_boost: Option<f32>,
    ) -> Result<D2D_RECT_F> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics)? };
        let padding_x = PANEL_PADDING_X * self.scale;
        let padding_y = PANEL_PADDING_Y * self.scale;
        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top,
                right: left + metrics.width + padding_x * 2.0,
                bottom: top + metrics.height + padding_y * 2.0,
            },
            radiusX: PANEL_CORNER_RADIUS * self.scale,
            radiusY: PANEL_CORNER_RADIUS * self.scale,
        };
        unsafe {
            let background = context
                .CreateSolidColorBrush(&color::output_color(PANEL_BACKGROUND, scrgb_boost), None)?;
            context.FillRoundedRectangle(&panel, &background);
            let foreground =
                context.CreateSolidColorBrush(&color::output_color(WHITE, scrgb_boost), None)?;
            context.DrawTextLayout(
                Vector2 {
                    X: left + padding_x,
                    Y: top + padding_y,
                },
                &layout,
                &foreground,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        Ok(panel.rect)
    }

    /// 뷰포트 중앙 에러 텍스트 — 배경 밝기에 따라 검정/흰색 (SPEC §3.6)
    fn draw_error_text(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
    ) -> Result<()> {
        let layout = self.create_layout(text, &self.error_format, viewport_width)?;
        unsafe {
            layout.SetMaxHeight(viewport_height)?;
            let text_color = if content.background_is_bright {
                BLACK
            } else {
                WHITE
            };
            let brush = context.CreateSolidColorBrush(
                &color::output_color(text_color, content.scrgb_boost),
                None,
            )?;
            context.DrawTextLayout(
                Vector2 { X: 0.0, Y: 0.0 },
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        Ok(())
    }

    fn create_layout(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        max_width: f32,
    ) -> Result<IDWriteTextLayout> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            self.dwrite_factory
                .CreateTextLayout(&utf16, format, max_width.max(1.0), f32::MAX)
        }
    }
}

/// 정보 패널 본문 조립 — 필드 8종 (SPEC §3.6)
/// Segoe UI 텍스트 포맷 2종(패널 14pt·에러 16pt) × DPI 배율 (R12)
fn create_text_formats(
    dwrite_factory: &IDWriteFactory,
    scale: f32,
) -> Result<(IDWriteTextFormat, IDWriteTextFormat)> {
    let create_format = |size: f32| unsafe {
        dwrite_factory.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size * scale,
            w!("en-us"),
        )
    };
    let text_format = create_format(14.0)?;
    let error_format = create_format(16.0)?;
    unsafe {
        error_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        error_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }
    Ok((text_format, error_format))
}

pub fn build_info_text(
    path: &Path,
    image: &DecodedImage,
    file_size: u64,
    modified: Option<SystemTime>,
) -> String {
    let file_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let mut lines = vec![
        file_name,
        format!("Format: {}", image.format_name),
        format!("Path: {}", path.display()),
        format!("Size: {}", format_file_size(file_size)),
    ];
    if let Some(modified) = modified {
        lines.push(format!("Modified: {}", format_locale_datetime(modified)));
    }
    let megapixels = f64::from(image.width) * f64::from(image.height) / 1_000_000.0;
    lines.push(format!(
        "Resolution: {} x {} ({megapixels:.1} MP)",
        image.width, image.height
    ));
    let divisor = greatest_common_divisor(image.width.max(1), image.height.max(1));
    lines.push(format!(
        "Ratio: {}:{}",
        image.width.max(1) / divisor,
        image.height.max(1) / divisor
    ));
    if image.frames.len() > 1 {
        lines.push(format!("Frames: {}", image.frames.len()));
    }
    if let Some(exif) = &image.exif {
        append_exif_lines(&mut lines, exif);
    }
    lines.join("\n")
}

/// EXIF 확장 표시 (SPEC §3.6, 2026-07-11) — 존재 필드만, 탐색기 세부 정보 순서.
/// 날짜는 기존 로캘 포맷 재사용, 라벨은 영어 고정.
fn append_exif_lines(lines: &mut Vec<String>, exif: &crate::image::decode::ExifInfo) {
    if let Some(taken) = exif.date_taken {
        lines.push(format!("Date taken: {}", format_locale_datetime(taken)));
    }
    if let Some(rating) = exif.rating {
        let stars = match rating {
            1..=12 => 1,
            13..=37 => 2,
            38..=62 => 3,
            63..=87 => 4,
            _ => 5,
        };
        lines.push(format!(
            "Rating: {stars} star{}",
            if stars == 1 { "" } else { "s" }
        ));
    }
    if let Some(maker) = &exif.camera_maker {
        lines.push(format!("Camera maker: {maker}"));
    }
    if let Some(model) = &exif.camera_model {
        lines.push(format!("Camera model: {model}"));
    }
    if let Some(f_stop) = exif.f_stop {
        lines.push(format!("F-stop: F/{}", trim_number(f_stop, 1)));
    }
    if let Some(seconds) = exif.exposure_time_seconds {
        let text = if seconds > 0.0 && seconds < 1.0 {
            format!("1/{} s", (1.0 / seconds).round() as u64)
        } else {
            format!("{} s", trim_number(seconds, 1))
        };
        lines.push(format!("Exposure time: {text}"));
    }
    if let Some(iso) = exif.iso_speed {
        lines.push(format!("ISO speed: ISO-{iso}"));
    }
    if let Some(bias) = exif.exposure_bias {
        lines.push(format!("Exposure bias: {} step", trim_number(bias, 1)));
    }
    if let Some(focal) = exif.focal_length_millimeters {
        lines.push(format!("Focal length: {} mm", trim_number(focal, 1)));
    }
    if let Some(aperture) = exif.max_aperture {
        lines.push(format!("Max aperture: {}", trim_number(aperture, 2)));
    }
    if let Some(mode) = exif.metering_mode {
        let text = match mode {
            1 => "Average",
            2 => "Center weighted average",
            3 => "Spot",
            4 => "Multi-spot",
            5 => "Pattern",
            6 => "Partial",
            _ => "Unknown",
        };
        lines.push(format!("Metering mode: {text}"));
    }
    if let Some(flash) = exif.flash {
        // EXIF Flash 비트필드: bit0 = 발광, bits3-4 = 모드(1/2=강제, 3=자동)
        let fired = flash & 0x1 != 0;
        let mode = (flash >> 3) & 0x3;
        let mut text = String::from(if fired { "Flash" } else { "No flash" });
        match mode {
            1 | 2 => text.push_str(", compulsory"),
            3 => text.push_str(", auto"),
            _ => {}
        }
        lines.push(format!("Flash mode: {text}"));
    }
    if let Some(focal_35mm) = exif.focal_length_35mm {
        lines.push(format!("Focal length (35mm): {focal_35mm}"));
    }
}

/// 소수 표시 — 지정 자릿수로 반올림 후 후행 0·소수점 제거 ("13.0" → "13")
fn trim_number(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 에러 텍스트 조립 (SPEC §3.6)
pub fn build_error_text(
    path: &Path,
    message: &str,
    code: i32,
    store_extension: Option<&str>,
) -> String {
    let file_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let reason = if message.is_empty() {
        "Decode failed".to_string()
    } else {
        message.trim().to_string()
    };
    let mut text = format!("Error occurred opening\n{file_name}\n{reason} (Error 0x{code:08X})");
    // WIC 확장 부재 — 설치 안내 문구만, 링크 없음 (SPEC §10)
    if let Some(extension_name) = store_extension {
        text.push_str(&format!(
            "\nInstall \"{extension_name}\" from the Microsoft Store to view this file."
        ));
    }
    text
}

/// "1.2 MiB (1,234,567 bytes)" (SPEC §3.6)
fn format_file_size(bytes: u64) -> String {
    let units: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    let scaled = units.iter().find(|(_, unit)| bytes >= *unit).map_or_else(
        || format!("{bytes} B"),
        |(name, unit)| format!("{:.1} {name}", bytes as f64 / *unit as f64),
    );
    format!("{scaled} ({} bytes)", group_thousands(bytes))
}

fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn greatest_common_divisor(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// 수정일시 로캘 포맷 (SPEC §3.6) — OS 로캘 API 위임 (P15)
fn format_locale_datetime(time: SystemTime) -> String {
    let Ok(elapsed) = time.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    // UNIX epoch → FILETIME(1601 기준, 100ns 단위)
    let intervals = elapsed.as_nanos() / 100 + 116_444_736_000_000_000;
    let file_time = FILETIME {
        dwLowDateTime: intervals as u32,
        dwHighDateTime: (intervals >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&file_time, &mut utc) }.is_err()
        || unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_err()
    {
        return String::new();
    }
    let mut date_buffer = [0u16; 64];
    let date_length = unsafe {
        GetDateFormatEx(
            None,
            DATE_SHORTDATE,
            Some(&local),
            None,
            Some(&mut date_buffer),
            None,
        )
    };
    let mut time_buffer = [0u16; 64];
    let time_length = unsafe {
        GetTimeFormatEx(
            None,
            windows::Win32::Globalization::TIME_FORMAT_FLAGS(0),
            Some(&local),
            None,
            Some(&mut time_buffer),
        )
    };
    let date = String::from_utf16_lossy(&date_buffer[..date_length.max(1) as usize - 1]);
    let time = String::from_utf16_lossy(&time_buffer[..time_length.max(1) as usize - 1]);
    format!("{date} {time}")
}
