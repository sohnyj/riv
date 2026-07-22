//! DirectWrite overlays: info panel, status pill, centered error text.

use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ROUNDED_RECT, ID2D1DeviceContext, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_LINE_SPACING_METHOD_UNIFORM,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::core::{Result, w};
use windows_numerics::Vector2;

use crate::image::color;
use crate::image::decode::{DecodedImage, PixelStorage};
use crate::view::renderer::ToneMapInfo;

/// Lucida Console design metrics: ascent 1616/2048 em, natural line exactly 1 em (R12).
const FONT_ASCENT_RATIO: f32 = 1616.0 / 2048.0;
/// Roomier than the font's tight single-em natural line.
const LINE_SPACING_RATIO: f32 = 1.3;

const PANEL_MARGIN: f32 = 12.0;
const PANEL_PADDING_X: f32 = 12.0;
const PANEL_PADDING_Y: f32 = 8.0;
const PANEL_CORNER_RADIUS: f32 = 8.0;
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

pub struct OverlayContent {
    pub error_text: Option<String>,
    /// Centered like an error while a URL downloads (no image is up then).
    pub download_text: Option<String>,
    pub info_text: Option<String>,
    pub status_text: Option<String>,
    /// Centered "riv" wordmark for the empty-window state.
    pub show_wordmark: bool,
    pub background_is_bright: bool,
    pub output_color_target: color::OutputColorTarget,
}

/// Horizontal placement of a top overlay panel.
enum PanelPlacement {
    TopLeft,
    TopCenter,
}

/// A solid brush for `color` mapped to the output color target.
fn solid_brush(
    context: &ID2D1DeviceContext,
    color: D2D1_COLOR_F,
    target: color::OutputColorTarget,
) -> Result<ID2D1SolidColorBrush> {
    unsafe { context.CreateSolidColorBrush(&color::output_color(color, target), None) }
}

pub struct Overlay {
    text_format: IDWriteTextFormat,
    centered_format: IDWriteTextFormat,
    wordmark_format: IDWriteTextFormat,
    dwrite_factory: IDWriteFactory,
    scale: f32,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let (text_format, centered_format, wordmark_format) =
            create_text_formats(&dwrite_factory, 1.0)?;
        Ok(Self {
            text_format,
            centered_format,
            wordmark_format,
            dwrite_factory,
            scale: 1.0,
        })
    }

    pub fn set_scale(&mut self, scale: f32) {
        let scale = scale.max(0.5);
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        if let Ok((text_format, centered_format, wordmark_format)) =
            create_text_formats(&self.dwrite_factory, scale)
        {
            self.text_format = text_format;
            self.centered_format = centered_format;
            self.wordmark_format = wordmark_format;
            self.scale = scale;
        }
    }

    pub fn draw(
        &self,
        context: &ID2D1DeviceContext,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
    ) -> Result<()> {
        let output_color_target = content.output_color_target;
        if let Some(info_text) = &content.info_text {
            self.draw_panel(
                context,
                info_text,
                PanelPlacement::TopLeft,
                viewport_width,
                output_color_target,
            )?;
        }
        if let Some(status_text) = &content.status_text {
            self.draw_panel(
                context,
                status_text,
                PanelPlacement::TopCenter,
                viewport_width,
                output_color_target,
            )?;
        }
        if let Some(centered_text) = content
            .error_text
            .as_ref()
            .or(content.download_text.as_ref())
        {
            self.draw_centered_text(
                context,
                centered_text,
                &self.centered_format,
                viewport_width,
                viewport_height,
                content,
                true,
            )?;
        } else if content.show_wordmark {
            // The wordmark is a mark, not a message; it stays unboxed.
            self.draw_centered_text(
                context,
                "riv",
                &self.wordmark_format,
                viewport_width,
                viewport_height,
                content,
                false,
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

    fn draw_panel(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        placement: PanelPlacement,
        viewport_width: f32,
        output_color_target: color::OutputColorTarget,
    ) -> Result<()> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&raw mut metrics)? };
        let padding_x = PANEL_PADDING_X * self.scale;
        let padding_y = PANEL_PADDING_Y * self.scale;
        let margin = PANEL_MARGIN * self.scale;
        let width = metrics.width + padding_x * 2.0;
        let left = match placement {
            PanelPlacement::TopLeft => margin,
            PanelPlacement::TopCenter => ((viewport_width - width) / 2.0).max(margin),
        };
        let top = margin;
        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top,
                right: left + width,
                bottom: top + metrics.height + padding_y * 2.0,
            },
            radiusX: PANEL_CORNER_RADIUS * self.scale,
            radiusY: PANEL_CORNER_RADIUS * self.scale,
        };
        unsafe {
            let background = solid_brush(context, PANEL_BACKGROUND, output_color_target)?;
            context.FillRoundedRectangle(&raw const panel, &background);
            let foreground = solid_brush(context, WHITE, output_color_target)?;
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
        Ok(())
    }

    /// Boxed messages take the panel styling; the unboxed wordmark follows the background.
    #[expect(clippy::too_many_arguments)]
    fn draw_centered_text(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        format: &IDWriteTextFormat,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
        boxed: bool,
    ) -> Result<()> {
        let padding_x = PANEL_PADDING_X * self.scale;
        let padding_y = PANEL_PADDING_Y * self.scale;
        // The box must fit the viewport, so boxed text wraps inside the margins.
        let inset = if boxed {
            (PANEL_MARGIN * self.scale + padding_x).min(viewport_width / 2.0)
        } else {
            0.0
        };
        let layout = self.create_layout(text, format, viewport_width - inset * 2.0)?;
        unsafe {
            layout.SetMaxHeight(viewport_height)?;
            if boxed {
                let mut metrics = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&raw mut metrics)?;
                let panel = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: inset + metrics.left - padding_x,
                        top: metrics.top - padding_y,
                        right: inset + metrics.left + metrics.width + padding_x,
                        bottom: metrics.top + metrics.height + padding_y,
                    },
                    radiusX: PANEL_CORNER_RADIUS * self.scale,
                    radiusY: PANEL_CORNER_RADIUS * self.scale,
                };
                let background =
                    solid_brush(context, PANEL_BACKGROUND, content.output_color_target)?;
                context.FillRoundedRectangle(&raw const panel, &background);
            }
            let text_color = if boxed || !content.background_is_bright {
                WHITE
            } else {
                BLACK
            };
            let brush = solid_brush(context, text_color, content.output_color_target)?;
            context.DrawTextLayout(
                Vector2 { X: inset, Y: 0.0 },
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

fn create_text_formats(
    dwrite_factory: &IDWriteFactory,
    scale: f32,
) -> Result<(IDWriteTextFormat, IDWriteTextFormat, IDWriteTextFormat)> {
    let create_format = |size: f32| unsafe {
        dwrite_factory.CreateTextFormat(
            w!("Lucida Console"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size * scale,
            w!("en-us"),
        )
    };
    let text_format = create_format(14.0)?;
    let centered_format = create_format(16.0)?;
    // The wordmark matches the About title: 40pt, size is the only variation.
    let wordmark_format = create_format(40.0 * 96.0 / 72.0)?;
    for format in [&centered_format, &wordmark_format] {
        unsafe {
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        }
    }
    // Uniform spacing keeps fallback glyphs (CJK names) from bloating single lines.
    let set_line_spacing = |format: &IDWriteTextFormat, size: f32| {
        let line = size * LINE_SPACING_RATIO;
        let baseline = size * FONT_ASCENT_RATIO + (line - size) / 2.0;
        unsafe { format.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line, baseline) }
    };
    set_line_spacing(&text_format, 14.0 * scale)?;
    set_line_spacing(&centered_format, 16.0 * scale)?;
    Ok((text_format, centered_format, wordmark_format))
}

#[expect(clippy::too_many_arguments)]
pub fn build_info_text(
    file_name: &str,
    location_text: &str,
    image: &DecodedImage,
    file_size: u64,
    modified: Option<SystemTime>,
    output_description: &str,
    scaling_description: &str,
    dither_description: &str,
    tone_map: Option<ToneMapInfo>,
) -> String {
    let megapixels = f64::from(image.width) * f64::from(image.height) / 1_000_000.0;
    // Grouped: image, then color/HDR, then render, then file (Date modified last, by EXIF Date taken).
    let mut lines = vec![
        file_name.to_string(),
        format!("Format: {}", image.format_name),
        format!(
            "Resolution: {} x {} ({megapixels:.1} MP)",
            image.width, image.height
        ),
    ];
    lines.push(format!(
        "Ratio: {}",
        format_aspect_ratio(image.width, image.height)
    ));
    if image.frames.len() > 1 {
        lines.push(format!("Frames: {}", image.frames.len()));
    }
    let color_profile = match &image.icc_profile {
        Some(profile) => crate::image::decode::icc_profile_description(profile)
            .unwrap_or_else(|| "Embedded".to_string()),
        None => "None".to_string(),
    };
    lines.push(format!("Color profile: {color_profile}"));
    match image.storage {
        PixelStorage::RgbaHalf => lines.push("Bit depth: FP16 linear".to_string()),
        PixelStorage::Bgra8 => {
            lines.push(format!("Bit depth: {}-bit", image.source_bits_per_channel));
        }
    }
    if let Some(peak) = image.peak_luminance_nits {
        lines.push(format!("Content peak: {peak:.0} nits"));
        if let Some(tone_map) = tone_map {
            if tone_map.hdr_display {
                lines.push(format!(
                    "Display peak: {:.0} nits",
                    tone_map.display_peak_nits
                ));
                lines.push(format!(
                    "Display full: {:.0} nits",
                    tone_map.display_full_frame_nits
                ));
            }
            if peak > color::SDR_REFERENCE_WHITE_NITS {
                lines.push(format!(
                    "Tone map: {:.0} nits",
                    tone_map.output_target_nits
                ));
            }
        }
    }
    lines.push(format!("Scaling: {scaling_description}"));
    lines.push(format!("Output: {output_description}"));
    lines.push(format!("Dither: {dither_description}"));
    lines.push(format!("Size: {}", format_file_size(file_size)));
    lines.push(format!("Path: {location_text}"));
    if let Some(modified) = modified {
        lines.push(format!(
            "Date modified: {}",
            format_local_datetime(modified)
        ));
    }
    if let Some(exif) = &image.exif {
        append_exif_lines(&mut lines, exif);
    }
    lines.join("\n")
}

/// Photography notation, shooting-settings order; Rating is Windows metadata and goes last.
fn append_exif_lines(lines: &mut Vec<String>, exif: &crate::image::decode::ExifInfo) {
    if let Some(taken) = exif.date_taken {
        lines.push(format!("Date taken: {}", format_local_datetime(taken)));
    }
    if let Some(maker) = &exif.camera_maker {
        lines.push(format!("Camera maker: {maker}"));
    }
    if let Some(model) = &exif.camera_model {
        lines.push(format!("Camera model: {model}"));
    }
    if let Some(focal) = exif.focal_length_millimeters {
        lines.push(format!("Focal length: {}mm", trim_number(focal, 1)));
    }
    if let Some(f_stop) = exif.f_stop {
        lines.push(format!("Aperture: f/{}", trim_number(f_stop, 1)));
    }
    if let Some(seconds) = exif.exposure_time_seconds {
        let text = if seconds > 0.0 && seconds < 1.0 {
            format!("1/{}s", (1.0 / seconds).round() as u64)
        } else {
            format!("{}s", trim_number(seconds, 1))
        };
        lines.push(format!("Exposure time: {text}"));
    }
    if let Some(iso) = exif.iso_speed {
        lines.push(format!("ISO: {iso}"));
    }
    if let Some(bias) = exif.exposure_bias {
        let value = trim_number(bias, 1);
        let signed = if value.starts_with('-') || value == "0" {
            value
        } else {
            format!("+{value}")
        };
        lines.push(format!("Exposure bias: {signed} EV"));
    }
    if let Some(aperture) = exif.max_aperture {
        lines.push(format!("Max aperture: f/{}", trim_number(aperture, 2)));
    }
    if let Some(mode) = exif.metering_mode {
        let text = match mode {
            1 => "Average",
            2 => "Center-weighted average",
            3 => "Spot",
            4 => "Multi-spot",
            5 => "Pattern",
            6 => "Partial",
            _ => "Unknown",
        };
        lines.push(format!("Metering mode: {text}"));
    }
    if let Some(flash) = exif.flash {
        let fired = flash & 0x1 != 0;
        let mode = (flash >> 3) & 0x3;
        let mut text = String::from(if fired { "Fired" } else { "Did not fire" });
        match mode {
            1 | 2 => text.push_str(", compulsory"),
            3 => text.push_str(", auto"),
            _ => {}
        }
        lines.push(format!("Flash: {text}"));
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
}

fn trim_number(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn build_error_text(
    file_name: &str,
    message: &str,
    code: i32,
    store_extension: Option<&str>,
) -> String {
    let reason = if message.is_empty() {
        "Decode failed".to_string()
    } else {
        message.trim().to_string()
    };
    // Code 0 means "no code" (validation, curl, fallback decoders), not a real HRESULT.
    let reason = if code == 0 {
        reason
    } else {
        format!("{reason} (Error 0x{code:08X})")
    };
    let mut text = if file_name.is_empty() {
        format!("Error occurred opening\n{reason}")
    } else {
        format!("Error occurred opening\n{file_name}\n{reason}")
    };
    if let Some(extension_name) = store_extension {
        text.push_str(&format!(
            "\nInstall \"{extension_name}\" (Microsoft Corporation)\nfrom the Microsoft Store to view this file."
        ));
    }
    text
}

/// Centered status while a URL downloads; received bytes only, no total is known.
pub fn build_download_text(file_name: &str, received_bytes: u64) -> String {
    if received_bytes == 0 {
        return format!("Connecting...\n{file_name}");
    }
    format!(
        "Downloading...\n{file_name}\n{}",
        scaled_size(received_bytes)
    )
}

fn scaled_size(bytes: u64) -> String {
    let units: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    units.iter().find(|(_, unit)| bytes >= *unit).map_or_else(
        || format!("{bytes} B"),
        |(name, unit)| format!("{:.1} {name}", bytes as f64 / *unit as f64),
    )
}

/// A recognized aspect ratio; the name is present when it adds to the display form.
struct NamedRatio {
    value: f64,
    display: &'static str,
    name: Option<&'static str>,
}

/// Absolute match window for a ratio value.
const RATIO_MATCH_THRESHOLD: f64 = 0.025;

const NAMED_RATIOS: &[NamedRatio] = &[
    NamedRatio {
        value: 1.0,
        display: "1:1",
        name: Some("Square"),
    },
    NamedRatio {
        value: 5.0 / 4.0,
        display: "5:4",
        name: None,
    },
    NamedRatio {
        value: 4.0 / 3.0,
        display: "4:3",
        name: None,
    },
    NamedRatio {
        value: 11.0 / 8.0,
        display: "11:8",
        name: Some("Academy"),
    },
    NamedRatio {
        value: 1.43,
        display: "1.43:1",
        name: Some("IMAX"),
    },
    NamedRatio {
        value: 3.0 / 2.0,
        display: "3:2",
        name: Some("35mm"),
    },
    NamedRatio {
        value: 16.0 / 10.0,
        display: "16:10",
        name: None,
    },
    NamedRatio {
        value: 5.0 / 3.0,
        display: "5:3",
        name: Some("35mm Widescreen"),
    },
    NamedRatio {
        value: 16.0 / 9.0,
        display: "16:9",
        name: None,
    },
    NamedRatio {
        value: 1.85,
        display: "1.85:1",
        name: Some("Academy Flat"),
    },
    NamedRatio {
        value: 256.0 / 135.0,
        display: "1.90:1",
        name: Some("SMPTE/DCI"),
    },
    NamedRatio {
        value: 2.0,
        display: "2:1",
        name: Some("Univisium"),
    },
    NamedRatio {
        value: 2.208,
        display: "2.20:1",
        name: Some("70mm"),
    },
    NamedRatio {
        value: 2.35,
        display: "2.35:1",
        name: Some("Scope"),
    },
    NamedRatio {
        value: 2.39,
        display: "2.39:1",
        name: Some("Panavision"),
    },
];

/// Reduced ratio terms up to this stay an integer ratio; larger reductions become a decimal.
const RATIO_INTEGER_LIMIT: u32 = 32;

fn matched_ratio(value: f64) -> Option<&'static NamedRatio> {
    NAMED_RATIOS
        .iter()
        .find(|entry| (value - entry.value).abs() < RATIO_MATCH_THRESHOLD)
}

fn greatest_common_divisor(mut first: u32, mut second: u32) -> u32 {
    while second != 0 {
        (first, second) = (second, first % second);
    }
    first.max(1)
}

/// The image's own ratio: an integer "width:height" when small, else a cinema-style decimal.
fn ratio_notation(width: u32, height: u32) -> String {
    let divisor = greatest_common_divisor(width, height);
    let (reduced_width, reduced_height) = (width / divisor, height / divisor);
    if reduced_width <= RATIO_INTEGER_LIMIT && reduced_height <= RATIO_INTEGER_LIMIT {
        format!("{reduced_width}:{reduced_height}")
    } else if width >= height {
        format!("{:.2}:1", f64::from(width) / f64::from(height))
    } else {
        format!("1:{:.2}", f64::from(height) / f64::from(width))
    }
}

fn reversed_ratio(display: &str) -> String {
    match display.split_once(':') {
        Some((width, height)) => format!("{height}:{width}"),
        None => display.to_string(),
    }
}

/// The ratio's label: the name in parentheses and a "Vertical" tag when either applies.
fn ratio_label(display: &str, name: Option<&str>, vertical: bool) -> String {
    match (name, vertical) {
        (Some(name), true) => format!("{display} ({name}, Vertical)"),
        (Some(name), false) => format!("{display} ({name})"),
        (None, true) => format!("{display} (Vertical)"),
        (None, false) => display.to_string(),
    }
}

/// Folds a matched ratio to its label; portrait framings reverse it and tag "Vertical".
fn format_aspect_ratio(width: u32, height: u32) -> String {
    let width = width.max(1);
    let height = height.max(1);
    let vertical = width < height;
    let (long, short) = (width.max(height), width.min(height));
    match matched_ratio(f64::from(long) / f64::from(short)) {
        Some(entry) => {
            let display = if vertical {
                reversed_ratio(entry.display)
            } else {
                entry.display.to_string()
            };
            ratio_label(&display, entry.name, vertical)
        }
        None => ratio_label(&ratio_notation(width, height), None, vertical),
    }
}

fn format_file_size(bytes: u64) -> String {
    format!("{} ({} bytes)", scaled_size(bytes), group_thousands(bytes))
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

/// Fixed 24-hour local time; locale forms can pull fallback glyphs into the panel.
fn format_local_datetime(time: SystemTime) -> String {
    let Ok(elapsed) = time.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let intervals =
        elapsed.as_nanos() / 100 + u128::from(crate::image::decode::FILETIME_UNIX_EPOCH);
    let file_time = FILETIME {
        dwLowDateTime: intervals as u32,
        dwHighDateTime: (intervals >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&raw const file_time, &raw mut utc) }.is_err()
        || unsafe { SystemTimeToTzSpecificLocalTime(None, &raw const utc, &raw mut local) }.is_err()
    {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

#[cfg(test)]
mod info_text_tests {
    use super::*;
    use crate::image::decode::Frame;

    #[test]
    fn bit_depth_always_appears() {
        let image = DecodedImage {
            width: 2,
            height: 1,
            pixel_width: 2,
            pixel_height: 1,
            format_name: "PNG",
            icc_profile: None,
            exif: None,
            storage: PixelStorage::Bgra8,
            source_bits_per_channel: 8,
            peak_luminance_nits: None,
            bright_coverage: None,
            frames: vec![Frame {
                pixels: vec![0; 8],
                delay_milliseconds: 0,
            }],
        };
        let text = build_info_text(
            "a.png",
            "C:\\a.png",
            &image,
            100,
            None,
            "8-bit sRGB",
            "Bilinear",
            "None",
            None,
        );
        assert!(text.contains("Bit depth: 8-bit"));
    }

    #[test]
    fn color_profile_line_reports_the_tag_state() {
        let mut image = DecodedImage {
            width: 2,
            height: 1,
            pixel_width: 2,
            pixel_height: 1,
            format_name: "PNG",
            icc_profile: None,
            exif: None,
            storage: PixelStorage::Bgra8,
            source_bits_per_channel: 8,
            peak_luminance_nits: None,
            bright_coverage: None,
            frames: vec![Frame {
                pixels: vec![0; 8],
                delay_milliseconds: 0,
            }],
        };
        let untagged = build_info_text(
            "a.png",
            "C:\\a.png",
            &image,
            100,
            None,
            "8-bit sRGB",
            "Bilinear",
            "None",
            None,
        );
        assert!(untagged.contains("Color profile: None"));
        image.icc_profile = Some(vec![0; 4]);
        let unparsable = build_info_text(
            "a.png",
            "C:\\a.png",
            &image,
            100,
            None,
            "8-bit sRGB",
            "Bilinear",
            "None",
            None,
        );
        assert!(unparsable.contains("Color profile: Embedded"));
    }

    #[test]
    fn hdr_lines_show_content_display_caps_and_the_tone_map() {
        let image = DecodedImage {
            width: 2,
            height: 1,
            pixel_width: 2,
            pixel_height: 1,
            format_name: "EXR",
            icc_profile: None,
            exif: None,
            storage: PixelStorage::RgbaHalf,
            source_bits_per_channel: 16,
            peak_luminance_nits: Some(1000.0),
            bright_coverage: Some(0.5),
            frames: vec![Frame {
                pixels: vec![0; 16],
                delay_milliseconds: 0,
            }],
        };
        let tone_map = ToneMapInfo {
            hdr_display: true,
            display_peak_nits: 600.0,
            display_full_frame_nits: 400.0,
            output_target_nits: 500.0,
        };
        let text = build_info_text(
            "a.exr",
            "C:\\a.exr",
            &image,
            100,
            None,
            "HDR10",
            "Bilinear",
            "None",
            Some(tone_map),
        );
        assert!(text.contains("Content peak: 1000 nits"), "{text}");
        assert!(text.contains("Display peak: 600 nits"));
        assert!(text.contains("Display full: 400 nits"));
        // Content peak (1000) exceeds the target (500): tone mapping is active.
        assert!(text.contains("Tone map: 500 nits"), "{text}");
        assert!(!text.contains("clipping"), "{text}");
    }
}

#[cfg(test)]
mod exif_line_tests {
    use super::*;
    use crate::image::decode::ExifInfo;

    #[test]
    fn exif_lines_follow_the_standard_notation_and_order() {
        let exif = ExifInfo {
            date_taken: None,
            rating: Some(80),
            camera_maker: Some("NIKON CORPORATION".to_string()),
            camera_model: Some("NIKON Z 8".to_string()),
            f_stop: Some(6.3),
            exposure_time_seconds: Some(0.004),
            iso_speed: Some(64),
            exposure_bias: Some(-0.7),
            focal_length_millimeters: Some(20.0),
            max_aperture: Some(4.0),
            metering_mode: Some(5),
            flash: Some(0),
        };
        let mut lines = Vec::new();
        append_exif_lines(&mut lines, &exif);
        assert_eq!(
            lines,
            vec![
                "Camera maker: NIKON CORPORATION",
                "Camera model: NIKON Z 8",
                "Focal length: 20mm",
                "Aperture: f/6.3",
                "Exposure time: 1/250s",
                "ISO: 64",
                "Exposure bias: -0.7 EV",
                "Max aperture: f/4",
                "Metering mode: Pattern",
                "Flash: Did not fire",
                "Rating: 4 stars",
            ]
        );
    }

    #[test]
    fn positive_bias_carries_its_sign() {
        let exif = ExifInfo {
            date_taken: None,
            rating: None,
            camera_maker: None,
            camera_model: None,
            f_stop: None,
            exposure_time_seconds: Some(2.0),
            iso_speed: None,
            exposure_bias: Some(0.7),
            focal_length_millimeters: None,
            max_aperture: None,
            metering_mode: None,
            flash: None,
        };
        let mut lines = Vec::new();
        append_exif_lines(&mut lines, &exif);
        assert_eq!(lines, vec!["Exposure time: 2s", "Exposure bias: +0.7 EV"]);
    }
}

#[cfg(test)]
mod aspect_ratio_tests {
    use super::*;

    #[test]
    fn named_ratios_label_unless_it_repeats_the_ratio() {
        assert_eq!(format_aspect_ratio(6000, 4000), "3:2 (35mm)");
        assert_eq!(format_aspect_ratio(1024, 1024), "1:1 (Square)");
        // The name repeats the ratio for these, so it is omitted.
        assert_eq!(format_aspect_ratio(1920, 1080), "16:9");
        assert_eq!(format_aspect_ratio(4032, 3024), "4:3");
        assert_eq!(format_aspect_ratio(2000, 1600), "5:4");
    }

    #[test]
    fn a_named_ratio_folds_to_its_canonical_label() {
        // 2467:1648 = 1.4969, within the match window, so it reads as 3:2 (not 1.50:1).
        assert_eq!(format_aspect_ratio(2467, 1648), "3:2 (35mm)");
    }

    #[test]
    fn cinema_ratios_read_as_decimals_with_their_name() {
        assert_eq!(format_aspect_ratio(1998, 1080), "1.85:1 (Academy Flat)");
        assert_eq!(format_aspect_ratio(2048, 858), "2.39:1 (Panavision)");
        assert_eq!(format_aspect_ratio(2048, 931), "2.20:1 (70mm)");
        // 21:9 has no entry; 2.370 folds to Scope by the table's first-match order.
        assert_eq!(format_aspect_ratio(2560, 1080), "2.35:1 (Scope)");
    }

    #[test]
    fn portrait_shows_the_reversed_ratio_tagged_vertical() {
        // A distinct landscape name rides along inside the tag.
        assert_eq!(format_aspect_ratio(4000, 6000), "2:3 (35mm, Vertical)");
        // When the name just repeats the ratio, only Vertical shows.
        assert_eq!(format_aspect_ratio(1080, 1920), "9:16 (Vertical)");
        assert_eq!(format_aspect_ratio(3000, 4000), "3:4 (Vertical)");
        // An unnamed portrait still carries the Vertical tag on its true ratio.
        assert_eq!(format_aspect_ratio(1000, 1301), "1:1.30 (Vertical)");
    }

    #[test]
    fn an_unnamed_ratio_shows_its_true_value() {
        // 1301:1000 = 1.301, outside every match window.
        assert_eq!(format_aspect_ratio(1301, 1000), "1.30:1");
        // 32:9 was trimmed from the table; it still reads as its reduced integer ratio.
        assert_eq!(format_aspect_ratio(3840, 1080), "32:9");
    }

    #[test]
    fn a_zero_dimension_stays_finite() {
        assert_eq!(format_aspect_ratio(0, 0), "1:1 (Square)");
    }
}

#[cfg(test)]
mod error_text_tests {
    use super::*;

    #[test]
    fn an_empty_name_drops_its_line() {
        let text = build_error_text("", "no URL in the clipboard", 0, None);
        assert_eq!(text, "Error occurred opening\nno URL in the clipboard");
    }

    #[test]
    fn code_zero_drops_the_error_suffix() {
        let uncoded = build_error_text("a.png", "unsupported URL protocol", 0, None);
        assert_eq!(
            uncoded,
            "Error occurred opening\na.png\nunsupported URL protocol"
        );
        let coded = build_error_text("a.png", "no image at this URL", 0x88982F50u32 as i32, None);
        assert_eq!(
            coded,
            "Error occurred opening\na.png\nno image at this URL (Error 0x88982F50)"
        );
    }
}
