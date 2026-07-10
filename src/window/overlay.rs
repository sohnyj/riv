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
}

pub struct Overlay {
    text_format: IDWriteTextFormat,
    error_format: IDWriteTextFormat,
    dwrite_factory: IDWriteFactory,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let create_format = |size: f32| unsafe {
            dwrite_factory.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )
        };
        let text_format = create_format(14.0)?;
        let error_format = create_format(16.0)?;
        unsafe {
            error_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            error_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        }
        Ok(Self {
            text_format,
            error_format,
            dwrite_factory,
        })
    }

    /// 렌더러의 D2D 패스 내부에서 호출 (BeginDraw~EndDraw 사이, 변환 identity)
    pub fn draw(
        &self,
        context: &ID2D1DeviceContext,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
    ) -> Result<()> {
        let info_rect = if let Some(info_text) = &content.info_text {
            Some(self.draw_panel(
                context,
                info_text,
                PANEL_MARGIN,
                PANEL_MARGIN,
                viewport_width,
            )?)
        } else {
            None
        };
        if let Some(pill_text) = &content.zoom_pill_text {
            // 줌 필 = 상단 중앙 (2026-07-10 — qView의 좌측 배치는 Qt 중앙 정렬 제약의
            // 우회였음. DWrite 메트릭 기반 정밀 중앙 배치). 정보 패널과 겹치면 그 아래로.
            let pill_width = self.measure_panel_width(pill_text, viewport_width)?;
            let centered_left = ((viewport_width - pill_width) / 2.0).max(PANEL_MARGIN);
            let top = match &info_rect {
                Some(info) if centered_left < info.right + 8.0 => info.bottom + 8.0,
                _ => PANEL_MARGIN,
            };
            self.draw_panel(context, pill_text, centered_left, top, viewport_width)?;
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
            (viewport_width - PANEL_MARGIN * 2.0 - PANEL_PADDING_X * 2.0).max(0.0),
        )
    }

    /// 패널 전체 폭(패딩 포함) 측정 — 중앙 배치 계산용
    fn measure_panel_width(&self, text: &str, viewport_width: f32) -> Result<f32> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics)? };
        Ok(metrics.width + PANEL_PADDING_X * 2.0)
    }

    /// 반투명 라운드 패널 + 흰 글자. 반환 = 패널 사각형.
    fn draw_panel(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        left: f32,
        top: f32,
        viewport_width: f32,
    ) -> Result<D2D_RECT_F> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics)? };
        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top,
                right: left + metrics.width + PANEL_PADDING_X * 2.0,
                bottom: top + metrics.height + PANEL_PADDING_Y * 2.0,
            },
            radiusX: PANEL_CORNER_RADIUS,
            radiusY: PANEL_CORNER_RADIUS,
        };
        unsafe {
            let background = context.CreateSolidColorBrush(&PANEL_BACKGROUND, None)?;
            context.FillRoundedRectangle(&panel, &background);
            let foreground = context.CreateSolidColorBrush(&WHITE, None)?;
            context.DrawTextLayout(
                Vector2 {
                    X: left + PANEL_PADDING_X,
                    Y: top + PANEL_PADDING_Y,
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
            let color = if content.background_is_bright {
                BLACK
            } else {
                WHITE
            };
            let brush = context.CreateSolidColorBrush(&color, None)?;
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
    lines.join("\n")
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
