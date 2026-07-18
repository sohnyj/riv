//! DirectWrite overlays: info panel, status pill, centered error text.

use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ROUNDED_RECT, ID2D1DeviceContext,
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

pub struct Overlay {
    text_format: IDWriteTextFormat,
    error_format: IDWriteTextFormat,
    wordmark_format: IDWriteTextFormat,
    dwrite_factory: IDWriteFactory,
    scale: f32,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let (text_format, error_format, wordmark_format) =
            create_text_formats(&dwrite_factory, 1.0)?;
        Ok(Self {
            text_format,
            error_format,
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
        if let Ok((text_format, error_format, wordmark_format)) =
            create_text_formats(&self.dwrite_factory, scale)
        {
            self.text_format = text_format;
            self.error_format = error_format;
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
        let margin = PANEL_MARGIN * self.scale;
        if let Some(info_text) = &content.info_text {
            self.draw_panel(
                context,
                info_text,
                margin,
                margin,
                viewport_width,
                output_color_target,
            )?;
        }
        if let Some(status_text) = &content.status_text {
            let status_width = self.measure_panel_width(status_text, viewport_width)?;
            let centered_left = ((viewport_width - status_width) / 2.0).max(margin);
            self.draw_panel(
                context,
                status_text,
                centered_left,
                margin,
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
                &self.error_format,
                viewport_width,
                viewport_height,
                content,
            )?;
        } else if content.show_wordmark {
            self.draw_centered_text(
                context,
                "riv",
                &self.wordmark_format,
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

    fn measure_panel_width(&self, text: &str, viewport_width: f32) -> Result<f32> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&raw mut metrics)? };
        Ok(metrics.width + PANEL_PADDING_X * 2.0 * self.scale)
    }

    fn draw_panel(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        left: f32,
        top: f32,
        viewport_width: f32,
        output_color_target: color::OutputColorTarget,
    ) -> Result<D2D_RECT_F> {
        let layout = self.panel_layout(text, viewport_width)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&raw mut metrics)? };
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
            let background = context.CreateSolidColorBrush(
                &color::output_color(PANEL_BACKGROUND, output_color_target),
                None,
            )?;
            context.FillRoundedRectangle(&raw const panel, &background);
            let foreground = context
                .CreateSolidColorBrush(&color::output_color(WHITE, output_color_target), None)?;
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

    fn draw_centered_text(
        &self,
        context: &ID2D1DeviceContext,
        text: &str,
        format: &IDWriteTextFormat,
        viewport_width: f32,
        viewport_height: f32,
        content: &OverlayContent,
    ) -> Result<()> {
        let layout = self.create_layout(text, format, viewport_width)?;
        unsafe {
            layout.SetMaxHeight(viewport_height)?;
            let text_color = if content.background_is_bright {
                BLACK
            } else {
                WHITE
            };
            let brush = context.CreateSolidColorBrush(
                &color::output_color(text_color, content.output_color_target),
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
    let error_format = create_format(16.0)?;
    // The wordmark matches the About title: 40pt, size is the only variation.
    let wordmark_format = create_format(40.0 * 96.0 / 72.0)?;
    for format in [&error_format, &wordmark_format] {
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
    set_line_spacing(&error_format, 16.0 * scale)?;
    Ok((text_format, error_format, wordmark_format))
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
) -> String {
    let megapixels = f64::from(image.width) * f64::from(image.height) / 1_000_000.0;
    // Content and render facts first, file-system facts second; Path wraps, so it closes the block.
    let mut lines = vec![
        file_name.to_string(),
        format!("Format: {}", image.format_name),
        format!(
            "Resolution: {} x {} ({megapixels:.1} MP)",
            image.width, image.height
        ),
    ];
    if image.frames.len() > 1 {
        lines.push(format!("Frames: {}", image.frames.len()));
    }
    lines.push(format!("Scaling: {scaling_description}"));
    let color_profile = match &image.icc_profile {
        Some(profile) => crate::image::decode::icc_profile_description(profile)
            .unwrap_or_else(|| "Embedded".to_string()),
        None => "None".to_string(),
    };
    lines.push(format!("Color profile: {color_profile}"));
    match image.storage {
        PixelStorage::RgbaHalf => match image.peak_luminance_nits {
            Some(peak) => lines.push(format!("Bit depth: FP16 linear, peak {peak:.0} nits")),
            None => lines.push("Bit depth: high (FP16)".to_string()),
        },
        PixelStorage::Bgra8 => {
            lines.push(format!("Bit depth: {}-bit", image.source_bits_per_channel));
        }
    }
    lines.push(format!("Output: {output_description}"));
    lines.push(format!("Dither: {dither_description}"));
    lines.push(format!("Size: {}", format_file_size(file_size)));
    if let Some(modified) = modified {
        lines.push(format!("Modified: {}", format_local_datetime(modified)));
    }
    lines.push(format!("Path: {location_text}"));
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
    let intervals = elapsed.as_nanos() / 100 + 116_444_736_000_000_000;
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
        );
        assert!(unparsable.contains("Color profile: Embedded"));
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
