#![windows_subsystem = "windows"]

mod actions;
mod archive;
mod bindings;
mod dialogs;
mod image;
mod network;
mod settings;
mod shell;
mod text;
mod view;
mod window;

use std::path::Path;
use std::sync::Arc;

use actions::{Action, ActivationGate};
use bindings::{Bindings, MODIFIER_CONTROL, MODIFIER_SHIFT, MouseBase, current_modifiers};
use dialogs::options::{WM_APP_OPTIONS_APPLIED, WM_APP_OPTIONS_GEOMETRY};
use image::animation::Animation;
use image::color;
use image::core::{
    CoreOptions, DecodeCompletion, DownloadProgress, ImageCore, ItemLocation, NavigationCommand,
    SortMode, WM_APP_DECODE_COMPLETE, WM_APP_DOWNLOAD_PROGRESS,
};
use image::decode::DecodedImage;
use network::curl;
use settings::{Options, SettingsFile};
use shell::drag_drop::{self, WM_APP_DROP_PATHS};
use shell::open_with::{self, OpenWithList, WM_APP_OPEN_WITH_LIST};
use shell::{clipboard, file_ops, open_dialog};
use view::dither::DitherMode;
use view::renderer::Renderer;
use view::transform::{FitMode, Size, ViewTransform};
use window::context_menu::{self, MenuSelection, MenuState};
use window::dwm;
use window::overlay::{self, Overlay, OverlayContent};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::D2D1_INTERPOLATION_MODE;
use windows::Win32::Graphics::Direct2D::{
    D2D1_INTERPOLATION_MODE_CUBIC, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, GetMonitorInfoW, GetSysColor, GetSysColorBrush, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromWindow, ScreenToClient, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IDropTarget, OleInitialize, RevokeDragDrop};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GWL_STYLE, GWLP_USERDATA, GetClientRect, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, HCURSOR, HTCAPTION, HTCLIENT,
    HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, IsZoomed, KillTimer,
    LoadCursorW, LoadIconW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOW,
    SW_SHOWMAXIMIZED, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageW, SetCursor, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, WINDOWPLACEMENT, WM_ACTIVATEAPP, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_GESTURE, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_MOVE, WM_NCDESTROY, WM_NCLBUTTONDOWN, WM_PAINT, WM_SETCURSOR,
    WM_SETTINGCHANGE, WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN, WM_TIMER, WM_XBUTTONDOWN, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW, WindowFromPoint,
};
use windows::core::{PCWSTR, Result, w};

/// Icon resource id 1 in riv.rc (MAKEINTRESOURCE).
const APPLICATION_ICON_ID: PCWSTR = PCWSTR(std::ptr::without_provenance(1));

const WM_APP_SHOW_WINDOW: u32 = WM_APP + 2;

const STATUS_TEXT_TIMER: usize = 2;
const SLIDESHOW_TIMER: usize = 3;
const SETTINGS_SAVE_TIMER: usize = 4;
const OPEN_WITH_TIMER: usize = 5;
const ANIMATION_TIMER: usize = 6;
const CURSOR_HIDE_TIMER: usize = 7;

const PAN_STEP: f32 = 64.0;

struct Application {
    /// None between a device loss and the next successful rebuild.
    renderer: Option<Renderer>,
    /// A failed output mode switch retries on the next paint.
    output_reconfigure_pending: bool,
    view_transform: ViewTransform,
    image_core: ImageCore,
    display: Option<Arc<DecodedImage>>,
    displayed_location: Option<ItemLocation>,
    settings: SettingsFile,
    bindings: Bindings,
    preserve_zoom: bool,
    always_on_top: bool,
    fullscreen_restore: Option<WINDOWPLACEMENT>,
    sdr_white_boost: f32,
    pan_drag_position: Option<(i32, i32)>,
    pan_cursor: HCURSOR,
    arrow_cursor: HCURSOR,
    cursor_hidden: bool,
    /// Last pointer position, to ignore synthetic non-moving WM_MOUSEMOVE.
    last_pointer_position: Option<(i32, i32)>,
    wheel_notch_accumulator: i32,
    window_shown: bool,
    show_maximized: bool,
    gesture_zoom_distance: Option<f32>,
    gesture_pan_point: Option<(i32, i32)>,
    overlay: Overlay,
    show_file_info: bool,
    status_text: Option<StatusText>,
    /// Memoized info panel text, rebuilt only when a display input changes.
    info_text_cache: Option<InfoTextCache>,
    /// Received bytes of the pending URL download the view reports on.
    download_progress: Option<(ItemLocation, u64)>,
    slideshow_active: bool,
    animation: Option<Animation>,
    drop_target: Option<IDropTarget>,
    open_with_list: Option<Box<OpenWithList>>,
}

/// A status pill: Timed auto-expires, Sticky holds until the image or playback changes.
enum StatusText {
    Timed(String),
    Sticky(String),
}

impl StatusText {
    fn text(&self) -> &str {
        match self {
            StatusText::Timed(text) | StatusText::Sticky(text) => text,
        }
    }
}

/// Memoized info panel text and the display inputs it was built from.
struct InfoTextCache {
    location: ItemLocation,
    image: usize,
    file_size: u64,
    modified: Option<std::time::SystemTime>,
    output_description: &'static str,
    scaling_description: &'static str,
    dither_description: &'static str,
    text: String,
}

impl Application {
    fn new(window: HWND, initial_path: Option<&Path>) -> Result<Self> {
        let (width, height) = client_size(window);
        let capabilities = color::display_capabilities(window);
        let renderer = Renderer::new(
            window,
            width.max(1),
            height.max(1),
            capabilities.hdr,
            capabilities.bits_per_color,
            tone_map_target_luminance(capabilities.hdr, capabilities.max_luminance),
        )?;
        let device_pixel_ratio = unsafe { GetDpiForWindow(window) } as f32 / 96.0;
        let settings = SettingsFile::load();
        let bindings =
            Bindings::from_settings(settings.keyboard_bindings(), settings.mouse_bindings());
        let mut view_transform = ViewTransform::new();
        view_transform.fit_mode = FitMode::from_setting(settings.options.fit_mode);
        let mut application = Self {
            renderer: Some(renderer),
            output_reconfigure_pending: false,
            view_transform,
            image_core: ImageCore::new(window, core_options(&settings.options)),
            display: None,
            displayed_location: None,
            settings,
            bindings,
            preserve_zoom: false,
            always_on_top: false,
            fullscreen_restore: None,
            sdr_white_boost: color::sdr_white_boost_for(window, capabilities.hdr),
            pan_drag_position: None,
            pan_cursor: unsafe { LoadCursorW(None, IDC_SIZEALL)? },
            arrow_cursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            cursor_hidden: false,
            last_pointer_position: None,
            wheel_notch_accumulator: 0,
            window_shown: false,
            show_maximized: false,
            gesture_zoom_distance: None,
            gesture_pan_point: None,
            overlay: Overlay::new()?,
            show_file_info: false,
            status_text: None,
            info_text_cache: None,
            download_progress: None,
            slideshow_active: false,
            animation: None,
            drop_target: None,
            open_with_list: None,
        };
        if let Some(renderer) = &mut application.renderer {
            renderer.set_sdr_white_boost(application.sdr_white_boost);
            renderer.set_dither_mode(DitherMode::from_setting(
                application.settings.options.dither,
            ));
        }
        application.overlay.set_scale(device_pixel_ratio);
        if let Some(path) = initial_path {
            application.image_core.load_path(path);
        }
        Ok(application)
    }

    /// Reconfigure the output on HDR mode or bit depth change; else refresh boost and tone map target.
    fn refresh_display_color_state(&mut self, window: HWND) {
        if self.reconfigure_display_output(window, false) {
            self.render(window);
            return;
        }
        let mut stale = false;
        let hdr_mode = self.renderer.as_ref().is_some_and(Renderer::hdr_mode);
        let boost = color::sdr_white_boost_for(window, hdr_mode);
        if (boost - self.sdr_white_boost).abs() > f32::EPSILON {
            self.sdr_white_boost = boost;
            if let Some(renderer) = &mut self.renderer {
                renderer.set_sdr_white_boost(boost);
            }
            stale = true;
        }
        let max_luminance = if hdr_mode {
            color::display_maximum_luminance(window)
        } else {
            None
        };
        let target_nits = tone_map_target_luminance(hdr_mode, max_luminance);
        if self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.set_tone_map_target_nits(target_nits))
        {
            stale = true;
        }
        if stale {
            self.render(window);
        }
    }

    /// True when the output mode changed (repaint due); arms the retry on failure.
    fn reconfigure_display_output(&mut self, window: HWND, force: bool) -> bool {
        let capabilities = color::display_capabilities(window);
        let mismatch = self.renderer.as_ref().is_some_and(|renderer| {
            capabilities.hdr != renderer.hdr_mode()
                || capabilities.bits_per_color != renderer.bits_per_color()
        });
        if !mismatch && !force {
            return false;
        }
        self.sdr_white_boost = color::sdr_white_boost_for(window, capabilities.hdr);
        let target_nits = tone_map_target_luminance(capabilities.hdr, capabilities.max_luminance);
        let reconfigured = self.renderer.as_mut().is_some_and(|renderer| {
            renderer
                .reconfigure_output(capabilities.hdr, capabilities.bits_per_color, target_nits)
                .is_ok()
        });
        self.output_reconfigure_pending = !reconfigured;
        if reconfigured {
            let _ = self.apply_renderer_state();
        }
        true
    }

    fn output_color_target(&self) -> color::OutputColorTarget {
        let Some(renderer) = &self.renderer else {
            return color::OutputColorTarget::Srgb;
        };
        if !renderer.hdr_mode() {
            return color::OutputColorTarget::Srgb;
        }
        if renderer.pq_output() {
            color::OutputColorTarget::Pq {
                sdr_white_boost: self.sdr_white_boost,
            }
        } else {
            color::OutputColorTarget::ScrgbLinear {
                sdr_white_boost: self.sdr_white_boost,
            }
        }
    }

    fn image_size(&self) -> Size {
        let (width, height) = self
            .display
            .as_ref()
            .map_or((1, 1), |image| (image.width, image.height));
        Size {
            width: width as f32,
            height: height as f32,
        }
    }

    fn viewport(&self, window: HWND) -> Size {
        let (width, height) = client_size(window);
        Size {
            width: width as f32,
            height: height as f32,
        }
    }

    fn interpolation_mode(&self) -> D2D1_INTERPOLATION_MODE {
        match self.settings.options.scaling_filter {
            0 => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            2 => D2D1_INTERPOLATION_MODE_CUBIC,
            3 => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            _ => D2D1_INTERPOLATION_MODE_LINEAR,
        }
    }

    fn scaling_description(&self) -> &'static str {
        // A 1:1 placement resamples nothing, whatever the filter.
        if self
            .renderer
            .as_ref()
            .is_some_and(Renderer::is_identity_draw)
        {
            return "None (1:1)";
        }
        match self.settings.options.scaling_filter {
            0 => "Nearest",
            2 => "Bicubic",
            3 => "High Quality",
            _ => "Bilinear",
        }
    }

    fn background_color(&self) -> D2D1_COLOR_F {
        let (red, green, blue) = if self.settings.options.background_color_enabled {
            self.settings.options.background_color
        } else {
            let colorref = unsafe { GetSysColor(COLOR_WINDOW) };
            (
                (colorref & 0xFF) as u8,
                ((colorref >> 8) & 0xFF) as u8,
                ((colorref >> 16) & 0xFF) as u8,
            )
        };
        D2D1_COLOR_F {
            r: f32::from(red) / 255.0,
            g: f32::from(green) / 255.0,
            b: f32::from(blue) / 255.0,
            a: 1.0,
        }
    }

    fn ensure_window_shown(&mut self, window: HWND) {
        if self.window_shown {
            return;
        }
        self.window_shown = true;
        let _ = unsafe {
            ShowWindow(
                window,
                if self.show_maximized {
                    SW_SHOWMAXIMIZED
                } else {
                    SW_SHOW
                },
            )
        };
    }

    fn restore_window_geometry(&mut self, window: HWND) {
        if !self.settings.options.save_window_position {
            return;
        }
        let Some((x, y, width, height, maximized)) = self.settings.window_geometry() else {
            return;
        };
        self.show_maximized = maximized;
        let placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            showCmd: SW_HIDE.0 as u32,
            rcNormalPosition: RECT {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            },
            ..Default::default()
        };
        let _ = unsafe { SetWindowPlacement(window, &raw const placement) };
    }

    fn save_window_geometry(&mut self, window: HWND) {
        if !self.settings.options.save_window_position {
            return;
        }
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if let Some(saved) = &self.fullscreen_restore {
            placement = *saved;
        } else if unsafe { GetWindowPlacement(window, &raw mut placement) }.is_err() {
            return;
        }
        let bounds = placement.rcNormalPosition;
        self.settings.set_window_geometry(
            bounds.left,
            bounds.top,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
            placement.showCmd == SW_SHOWMAXIMIZED.0 as u32,
        );
        let _ = self.settings.save();
    }

    fn update_window_title(&self, window: HWND) {
        let file_name = self
            .image_core
            .current
            .as_ref()
            .map(|current| current.location.display_name())
            .filter(|name| !name.is_empty());
        let title = match (self.settings.options.title_bar_mode, file_name) {
            (0, _) | (_, None) => "riv".to_string(),
            (2, Some(name)) => self.prefix_with_position(name),
            (3, Some(name)) => {
                let body = self
                    .image_core
                    .current
                    .as_ref()
                    .and_then(|current| current.location.folder_name())
                    .map_or_else(|| name.clone(), |folder| format!("{folder}\\{name}"));
                self.prefix_with_position(body)
            }
            (_, Some(name)) => name,
        };
        let wide = crate::text::wide(&title);
        let _ = unsafe { SetWindowTextW(window, PCWSTR(wide.as_ptr())) };
    }

    /// Prefixes "[index/total]" when the folder listing gives a position.
    fn prefix_with_position(&self, body: String) -> String {
        match self.image_core.listing_position() {
            Some((index, total)) => format!("[{index}/{total}] {body}"),
            None => body,
        }
    }

    fn apply_current_image(&mut self, window: HWND) {
        self.download_progress = None;
        self.dismiss_frame_counter();
        let Some(current) = &self.image_core.current else {
            return;
        };
        let image = current.image.clone();
        let location = current.location.clone();
        // Same item at the same logical size (RAW preview swap, reload): keep the view.
        let same_view = self
            .displayed_location
            .as_ref()
            .is_some_and(|displayed| *displayed == location)
            && self.display.as_ref().is_some_and(|previous| {
                previous.width == image.width && previous.height == image.height
            });
        let frame = &image.frames[0];
        let upload = match &mut self.renderer {
            Some(renderer) => renderer.set_image(&frame.pixels, &image),
            None => Err(windows::core::Error::empty()),
        };
        self.display = Some(image);
        self.displayed_location = Some(location.clone());
        if !same_view {
            let transform = &mut self.view_transform;
            transform.rotation_quadrant = 0;
            transform.mirrored = false;
            transform.flipped = false;
            transform.pan_offset_x = 0.0;
            transform.pan_offset_y = 0.0;
            transform.fit_tracking = !self.preserve_zoom;
        }
        if upload.is_err() {
            let _ = self.rebuild_renderer(window);
        }
        let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
        self.animation = self
            .display
            .as_ref()
            .and_then(|image| Animation::new(image));
        if let Some(animation) = &self.animation {
            unsafe {
                SetTimer(
                    Some(window),
                    ANIMATION_TIMER,
                    animation.current_delay_milliseconds(),
                    None,
                )
            };
        }
        if !same_view {
            // Members list the archive itself; URL items stay out of recents.
            if let Some(file) = location.containing_file()
                && self.settings.add_recent_file(file)
            {
                unsafe { SetTimer(Some(window), SETTINGS_SAVE_TIMER, 500, None) };
            }
            self.open_with_list = None;
            if location.as_file().is_some() {
                unsafe { SetTimer(Some(window), OPEN_WITH_TIMER, 250, None) };
            }
        }
        self.update_window_title(window);
        self.render(window);
    }

    fn apply_load_error(&mut self, window: HWND) {
        self.download_progress = None;
        self.clear_displayed_image(window);
    }

    /// Drop the image so only centered overlay text (error, download) shows.
    fn clear_displayed_image(&mut self, window: HWND) {
        let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
        self.dismiss_frame_counter();
        self.animation = None;
        self.display = None;
        self.displayed_location = None;
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_image();
        }
        self.update_window_title(window);
        self.render(window);
    }

    fn apply_download_progress(&mut self, window: HWND, progress: DownloadProgress) {
        if !self.image_core.is_pending(&progress.location) {
            return; // a stale download the view moved away from
        }
        let first_report = self.download_progress.is_none();
        self.download_progress = Some((progress.location, progress.received_bytes));
        if first_report {
            // The download screen stands alone; the previous image goes away.
            self.clear_displayed_image(window);
        } else {
            self.render(window);
        }
    }

    /// Freeze a playing animation while a new load is in flight.
    fn freeze_animation_for_load(&mut self, window: HWND) {
        if self.animation.is_some() {
            let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
        }
    }

    fn play_animation_frame(&mut self, window: HWND) {
        let Some(animation) = self.animation.as_mut() else {
            let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
            return;
        };
        let frame_index = animation.next_frame();
        let delay = animation.current_delay_milliseconds();
        let paused = animation.paused;
        self.render_animation_frame(window, frame_index);
        if !paused {
            unsafe { SetTimer(Some(window), ANIMATION_TIMER, delay, None) };
        }
    }

    /// A manual step pauses playback; resume is left to the user.
    fn step_animation_frame(&mut self, window: HWND, forward: bool) {
        let Some(animation) = self.animation.as_mut() else {
            return;
        };
        animation.paused = true;
        let frame_index = if forward {
            animation.next_frame()
        } else {
            animation.previous_frame()
        };
        let frame_count = animation.frame_count();
        let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
        // The frame-step pill holds until resume or an image change, so no timer.
        let _ = unsafe { KillTimer(Some(window), STATUS_TEXT_TIMER) };
        self.status_text = Some(StatusText::Sticky(format!(
            "Frame: {} / {}",
            frame_index + 1,
            frame_count
        )));
        self.render_animation_frame(window, frame_index);
    }

    fn render_animation_frame(&mut self, window: HWND, frame_index: usize) {
        let Some(image) = self.display.clone() else {
            return;
        };
        let frame = &image.frames[frame_index];
        if let Some(renderer) = &mut self.renderer
            && renderer.update_frame_pixels(&frame.pixels).is_err()
        {
            let _ = renderer.set_image(&frame.pixels, &image);
        }
        self.render(window);
    }

    fn show_status_text(&mut self, window: HWND, text: String) {
        self.status_text = Some(StatusText::Timed(text));
        unsafe { SetTimer(Some(window), STATUS_TEXT_TIMER, 1000, None) };
    }

    /// Drops the frame-step pill; returns whether one was showing.
    fn dismiss_frame_counter(&mut self) -> bool {
        let showing = matches!(self.status_text, Some(StatusText::Sticky(_)));
        if showing {
            self.status_text = None;
        }
        showing
    }

    fn toggle_slideshow(&mut self, window: HWND) {
        if self.slideshow_active {
            self.cancel_slideshow(window);
        } else {
            let interval = self.settings.options.slideshow_interval_seconds * 1000;
            unsafe { SetTimer(Some(window), SLIDESHOW_TIMER, interval, None) };
            // The declared direction aims the preload before the first tick.
            self.image_core
                .set_travel_direction(self.settings.options.slideshow_reversed);
            self.slideshow_active = true;
            self.show_status_text(window, "Slideshow: Start".to_string());
            self.render(window);
        }
    }

    fn cancel_slideshow(&mut self, window: HWND) {
        if self.slideshow_active {
            let _ = unsafe { KillTimer(Some(window), SLIDESHOW_TIMER) };
            self.slideshow_active = false;
            self.show_status_text(window, "Slideshow: Stop".to_string());
            self.render(window);
        }
    }

    fn rebuild_renderer(&mut self, window: HWND) -> Result<()> {
        // The old swapchain must release the window first: DXGI allows one per window.
        self.renderer = None;
        let (width, height) = client_size(window);
        let capabilities = color::display_capabilities(window);
        self.renderer = Some(Renderer::new(
            window,
            width.max(1),
            height.max(1),
            capabilities.hdr,
            capabilities.bits_per_color,
            tone_map_target_luminance(capabilities.hdr, capabilities.max_luminance),
        )?);
        self.apply_renderer_state()
    }

    /// Reapplies the application-held state after a renderer rebuild or reconfigure.
    fn apply_renderer_state(&mut self) -> Result<()> {
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        renderer.set_sdr_white_boost(self.sdr_white_boost);
        renderer.set_dither_mode(DitherMode::from_setting(self.settings.options.dither));
        if let Some(image) = &self.display {
            let frame_index = self
                .animation
                .as_ref()
                .map_or(0, |animation| animation.frame_index);
            renderer.set_image(&image.frames[frame_index].pixels, image)?;
        }
        Ok(())
    }

    fn overlay_content(&mut self, background: D2D1_COLOR_F) -> OverlayContent {
        let error_text = self
            .image_core
            .load_error
            .as_ref()
            .map(|(location, error)| {
                overlay::build_error_text(
                    &location.display_name(),
                    &error.message,
                    error.code,
                    error.store_extension,
                )
            });
        // The pill borrows the top edge: the info panel yields while one shows.
        let info_text = if self.show_file_info && self.status_text.is_none() {
            self.cached_info_text()
        } else {
            None
        };
        let download_text = self
            .download_progress
            .as_ref()
            .filter(|(location, _)| self.image_core.is_pending(location))
            .map(|(location, received_bytes)| {
                overlay::build_download_text(&location.display_name(), *received_bytes)
            });
        let brightness = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
        // The wordmark marks a truly empty window, never a load in flight.
        let show_wordmark = error_text.is_none()
            && download_text.is_none()
            && self.display.is_none()
            && self.image_core.current.is_none()
            && !self.image_core.has_pending_display();
        OverlayContent {
            error_text,
            download_text,
            info_text,
            status_text: self
                .status_text
                .as_ref()
                .map(|status| status.text().to_owned()),
            show_wordmark,
            background_is_bright: brightness > 0.5,
            output_color_target: self.output_color_target(),
        }
    }

    /// Info panel text, rebuilt only when a display input changes (else the cached copy).
    fn cached_info_text(&mut self) -> Option<String> {
        let output_description = self
            .renderer
            .as_ref()
            .map_or("", |renderer| renderer.output_description());
        let scaling_description = self.scaling_description();
        let dither_description = self
            .renderer
            .as_ref()
            .map_or("None", |renderer| renderer.dither_description());
        let (file_size, modified) = self.image_core.current_item_metadata().unwrap_or((0, None));
        let current = self.image_core.current.as_ref()?;
        let image_id = Arc::as_ptr(&current.image) as usize;
        let reuse = self.info_text_cache.as_ref().is_some_and(|cache| {
            cache.location == current.location
                && cache.image == image_id
                && cache.file_size == file_size
                && cache.modified == modified
                && cache.output_description == output_description
                && cache.scaling_description == scaling_description
                && cache.dither_description == dither_description
        });
        if !reuse {
            let text = overlay::build_info_text(
                &current.location.display_name(),
                &current.location.display_text(),
                &current.image,
                file_size,
                modified,
                output_description,
                scaling_description,
                dither_description,
            );
            let location = current.location.clone();
            self.info_text_cache = Some(InfoTextCache {
                location,
                image: image_id,
                file_size,
                modified,
                output_description,
                scaling_description,
                dither_description,
                text,
            });
        }
        self.info_text_cache
            .as_ref()
            .map(|cache| cache.text.clone())
    }

    fn render(&mut self, window: HWND) {
        let (width, height) = client_size(window);
        if width == 0 || height == 0 {
            return;
        }
        // A lost renderer or a failed output switch retries once per paint.
        if self.renderer.is_none() {
            let _ = self.rebuild_renderer(window);
        }
        if self.output_reconfigure_pending {
            self.output_reconfigure_pending = false;
            let _ = self.reconfigure_display_output(window, true);
        }
        let viewport = self.viewport(window);
        let image = self.image_size();
        self.view_transform.synchronize(viewport, image);
        let matrix = self.view_transform.matrix(viewport, image);
        let interpolation = self.interpolation_mode();
        let background = self.background_color();
        let content = self.overlay_content(background);
        let clear_color = color::output_color(background, self.output_color_target());
        let overlay = &self.overlay;
        let draw = |context: &_| overlay.draw(context, viewport.width, viewport.height, &content);
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        if renderer
            .render(matrix, interpolation, clear_color, draw)
            .is_err()
        {
            // Device lost: drop the swapchain, rebuild once and retry.
            self.renderer = None;
            if self.rebuild_renderer(window).is_ok()
                && let Some(renderer) = &mut self.renderer
            {
                let overlay = &self.overlay;
                let _ = renderer.render(matrix, interpolation, clear_color, |context| {
                    overlay.draw(context, viewport.width, viewport.height, &content)
                });
            }
        }
    }

    /// Writes the current options back to disk and rebroadcasts them.
    fn persist_options(&mut self, window: HWND) {
        let options = self.settings.options.clone();
        self.settings.set_options(&options);
        let _ = self.settings.save();
        self.apply_options(window);
    }

    fn apply_options(&mut self, window: HWND) {
        self.bindings = Bindings::from_settings(
            self.settings.keyboard_bindings(),
            self.settings.mouse_bindings(),
        );
        self.view_transform.fit_mode = FitMode::from_setting(self.settings.options.fit_mode);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_dither_mode(DitherMode::from_setting(self.settings.options.dither));
        }
        self.image_core
            .update_options(core_options(&self.settings.options));
        self.update_cursor_autohide(window);
        self.update_window_title(window);
        self.render(window);
    }

    fn gate_satisfied(&self, gate: ActivationGate) -> bool {
        match gate {
            ActivationGate::Window => true,
            ActivationGate::Image => self.image_core.current.is_some(),
            ActivationGate::FileOnDisk => self
                .image_core
                .current
                .as_ref()
                .is_some_and(|current| current.location.as_file().is_some()),
            ActivationGate::ContainingFile => self
                .image_core
                .current
                .as_ref()
                .is_some_and(|current| current.location.containing_file().is_some()),
            ActivationGate::Animation => self
                .image_core
                .current
                .as_ref()
                .is_some_and(|current| current.image.frames.len() > 1),
            ActivationGate::NavigationTargets => self.image_core.has_navigation_targets(),
        }
    }

    /// Cursor anchor for zoom operations, when enabled and over the client area.
    fn zoom_anchor(&self, window: HWND) -> Option<(f32, f32)> {
        if self.settings.options.cursor_zoom {
            cursor_from_center(window)
        } else {
            None
        }
    }

    fn zoom_by(&mut self, window: HWND, factor: f32) {
        let anchor = self.zoom_anchor(window);
        self.zoom_at(window, factor, anchor);
    }

    fn zoom_at(&mut self, window: HWND, factor: f32, anchor: Option<(f32, f32)>) {
        let viewport = self.viewport(window);
        let image = self.image_size();
        let previous_scale = self.view_transform.scale;
        self.view_transform.zoom(factor, anchor, viewport, image);
        if (self.view_transform.scale - previous_scale).abs() > f32::EPSILON {
            let percent = (self.view_transform.scale * 100.0).round();
            self.show_status_text(window, format!("Zoom: {percent}%"));
        }
        self.render(window);
    }

    fn wheel_zoom(&mut self, window: HWND, wheel_delta: i16) {
        let step = 1.0 + self.settings.options.zoom_step_percent as f32 / 100.0;
        let exponent = if self.settings.options.fractional_zoom {
            f32::from(wheel_delta) / 120.0
        } else {
            let notches = self.accumulate_wheel_notches(wheel_delta);
            if notches == 0 {
                return;
            }
            notches as f32
        };
        self.zoom_by(window, step.powf(exponent));
    }

    fn accumulate_wheel_notches(&mut self, wheel_delta: i16) -> i32 {
        self.wheel_notch_accumulator += i32::from(wheel_delta);
        let notches = self.wheel_notch_accumulator / 120;
        self.wheel_notch_accumulator %= 120;
        notches
    }

    fn pan_by(&mut self, window: HWND, delta_x: f32, delta_y: f32) {
        let viewport = self.viewport(window);
        let image = self.image_size();
        self.view_transform
            .pan_by(delta_x, delta_y, viewport, image);
        self.render(window);
    }

    fn cursor_autohide_active(&self) -> bool {
        self.settings.options.hide_cursor_fullscreen && self.fullscreen_restore.is_some()
    }

    /// Re-arm or tear down the idle timer after a fullscreen or option change.
    fn update_cursor_autohide(&mut self, window: HWND) {
        if self.cursor_autohide_active() {
            self.last_pointer_position = None;
            start_cursor_hide_timer(window);
        } else {
            let _ = unsafe { KillTimer(Some(window), CURSOR_HIDE_TIMER) };
            self.reveal_cursor();
        }
    }

    fn reveal_cursor(&mut self) {
        if self.cursor_hidden {
            self.cursor_hidden = false;
            let cursor = if self.pan_drag_position.is_some() {
                self.pan_cursor
            } else {
                self.arrow_cursor
            };
            unsafe { SetCursor(Some(cursor)) };
        }
    }

    /// Real pointer motion reveals the cursor and restarts the idle countdown.
    fn record_pointer_activity(&mut self, window: HWND, position: (i32, i32)) {
        if !self.cursor_autohide_active() || self.last_pointer_position == Some(position) {
            return;
        }
        self.last_pointer_position = Some(position);
        self.reveal_cursor();
        start_cursor_hide_timer(window);
    }

    fn hide_cursor_if_idle(&mut self, window: HWND) {
        if !self.cursor_autohide_active() {
            return;
        }
        // A held drag or a pointer over a menu or another window: re-arm, don't hide.
        if self.pan_drag_position.is_some() || !cursor_over_window(window) {
            start_cursor_hide_timer(window);
            return;
        }
        self.cursor_hidden = true;
        unsafe { SetCursor(None) };
    }
}

fn core_options(options: &Options) -> CoreOptions {
    CoreOptions {
        sort_mode: SortMode::from_setting(options.sort_mode),
        sort_descending: options.sort_descending,
        preloading_mode: options.preloading_mode as usize,
        loop_within_folder: options.loop_within_folder,
        skip_hidden: options.skip_hidden,
        detect_format_by_content: options.detect_format_by_content,
    }
}

fn client_size(window: HWND) -> (u32, u32) {
    let mut bounds = RECT::default();
    let _ = unsafe { GetClientRect(window, &raw mut bounds) };
    (
        (bounds.right - bounds.left).max(0) as u32,
        (bounds.bottom - bounds.top).max(0) as u32,
    )
}

/// HDR: monitor peak (600 fallback); SDR: the 203-nit BT.2100 reference white.
fn tone_map_target_luminance(hdr_mode: bool, max_luminance: Option<f32>) -> f32 {
    if hdr_mode {
        max_luminance.unwrap_or(600.0)
    } else {
        203.0
    }
}

/// Cursor offset from the client center while over the client area.
fn cursor_from_center(window: HWND) -> Option<(f32, f32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&raw mut point) }.ok()?;
    let _ = unsafe { ScreenToClient(window, &raw mut point) };
    let (width, height) = client_size(window);
    if point.x < 0 || point.y < 0 || point.x >= width as i32 || point.y >= height as i32 {
        return None;
    }
    Some((
        point.x as f32 - width as f32 / 2.0,
        point.y as f32 - height as f32 / 2.0,
    ))
}

fn start_cursor_hide_timer(window: HWND) {
    unsafe { SetTimer(Some(window), CURSOR_HIDE_TIMER, 1000, None) };
}

/// True when our window is the topmost one under the pointer.
fn cursor_over_window(window: HWND) -> bool {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&raw mut point) }.is_err() {
        return false;
    }
    let hovered = unsafe { WindowFromPoint(point) };
    hovered == window
}

fn execute_navigation(
    application: &mut Application,
    window: HWND,
    command: NavigationCommand,
) -> bool {
    let result = application.image_core.navigate(command);
    apply_navigation_result(application, window, result)
}

fn apply_navigation_result(
    application: &mut Application,
    window: HWND,
    result: Option<bool>,
) -> bool {
    match result {
        Some(true) => application.apply_current_image(window),
        Some(false) => {
            if application.image_core.load_error.is_some() {
                application.apply_load_error(window);
            } else {
                application.freeze_animation_for_load(window);
            }
        }
        None => return false,
    }
    true
}

fn open_external(
    application: &mut Application,
    window: HWND,
    load: impl FnOnce(&mut ImageCore) -> bool,
) {
    application.cancel_slideshow(window);
    application.freeze_animation_for_load(window);
    if load(&mut application.image_core) {
        application.apply_current_image(window);
    } else if application.image_core.load_error.is_some() {
        application.apply_load_error(window);
    }
}

fn open_external_path(application: &mut Application, window: HWND, path: &Path) {
    open_external(application, window, |core| core.load_path(path));
}

fn open_external_url(application: &mut Application, window: HWND, url: &str) {
    open_external(application, window, |core| core.load_url(url));
}

/// Opens clipboard text as a URL; empty or non-text clipboard surfaces an error too.
fn paste_open_url(application: &mut Application, window: HWND) -> bool {
    let text = clipboard::read_text(window).unwrap_or_default();
    open_external_url(application, window, &text);
    true
}

/// The single dispatch point; every input path converges here.
fn dispatch_action(application: &mut Application, window: HWND, action: Action) {
    if !application.gate_satisfied(action.gate()) {
        return;
    }
    let zoom_step = 1.0 + application.settings.options.zoom_step_percent as f32 / 100.0;
    match action {
        Action::Quit => {
            let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        Action::FirstFile => {
            execute_navigation(application, window, NavigationCommand::First);
        }
        Action::PreviousFile => {
            execute_navigation(application, window, NavigationCommand::Previous);
        }
        Action::NextFile => {
            execute_navigation(application, window, NavigationCommand::Next);
        }
        Action::LastFile => {
            execute_navigation(application, window, NavigationCommand::Last);
        }
        Action::Reload => {
            if application.image_core.reload_current() {
                application.apply_current_image(window);
            }
        }
        Action::ZoomIn => application.zoom_by(window, zoom_step),
        Action::ZoomOut => application.zoom_by(window, 1.0 / zoom_step),
        Action::ToggleZoom => {
            let viewport = application.viewport(window);
            let image = application.image_size();
            let anchor = application.zoom_anchor(window);
            application
                .view_transform
                .toggle_zoom(anchor, viewport, image);
            let text = if application.view_transform.fit_tracking {
                "Fit"
            } else {
                "1:1"
            };
            application.show_status_text(window, text.to_string());
            application.render(window);
        }
        Action::FitMode => {
            application.settings.options.fit_mode ^= 1;
            let axis = if application.settings.options.fit_mode == 1 {
                "Height"
            } else {
                "Width"
            };
            application.show_status_text(window, format!("Fit: {axis}"));
            application.persist_options(window);
        }
        Action::PreserveZoom => {
            application.preserve_zoom = !application.preserve_zoom;
            let state = if application.preserve_zoom {
                "On"
            } else {
                "Off"
            };
            application.show_status_text(window, format!("Preserve Zoom: {state}"));
            application.render(window);
        }
        Action::ShowFileInfo => {
            // Any pill masks the panel; one press reveals it rather than toggling off unseen.
            if application.status_text.take().is_some() {
                application.show_file_info = true;
            } else {
                application.show_file_info = !application.show_file_info;
            }
            application.render(window);
        }
        Action::Loop => {
            application.settings.options.loop_within_folder ^= true;
            let state = if application.settings.options.loop_within_folder {
                "On"
            } else {
                "Off"
            };
            // The pill spells out the full option name the short menu label elides.
            application.show_status_text(window, format!("Loop within Folder: {state}"));
            application.persist_options(window);
        }
        Action::Slideshow => application.toggle_slideshow(window),
        Action::Recent(index) => {
            let path = application
                .settings
                .recent_files()
                .get(usize::from(index))
                .map(|(_, path)| std::path::PathBuf::from(path));
            if let Some(path) = path {
                open_external_path(application, window, &path);
            }
        }
        Action::ClearRecents => {
            if application.settings.clear_recent_files() {
                unsafe { SetTimer(Some(window), SETTINGS_SAVE_TIMER, 500, None) };
            }
        }
        Action::PanUp => application.pan_by(window, 0.0, PAN_STEP),
        Action::PanDown => application.pan_by(window, 0.0, -PAN_STEP),
        Action::PanLeft => application.pan_by(window, PAN_STEP, 0.0),
        Action::PanRight => application.pan_by(window, -PAN_STEP, 0.0),
        Action::RotateRight | Action::RotateLeft => {
            let step = if action == Action::RotateRight { 1 } else { -1 };
            let viewport = application.viewport(window);
            let image = application.image_size();
            application.view_transform.rotate(step, viewport, image);
            let text = match application.view_transform.rotation_quadrant {
                1 => "Rotate: R90\u{b0}",
                2 => "Rotate: 180\u{b0}",
                3 => "Rotate: L90\u{b0}",
                _ => "Rotate: 0\u{b0}",
            };
            application.show_status_text(window, text.to_string());
            application.render(window);
        }
        Action::Mirror => {
            application.view_transform.mirror();
            let text = if application.view_transform.mirrored {
                "Mirror: On"
            } else {
                "Mirror: Off"
            };
            application.show_status_text(window, text.to_string());
            application.render(window);
        }
        Action::Flip => {
            application.view_transform.flip();
            let text = if application.view_transform.flipped {
                "Flip: On"
            } else {
                "Flip: Off"
            };
            application.show_status_text(window, text.to_string());
            application.render(window);
        }
        Action::AlwaysOnTop => {
            application.always_on_top = !application.always_on_top;
            let (order, state) = if application.always_on_top {
                (HWND_TOPMOST, "On")
            } else {
                (HWND_NOTOPMOST, "Off")
            };
            let _ = unsafe {
                SetWindowPos(
                    window,
                    Some(order),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            };
            application.show_status_text(window, format!("Always on Top: {state}"));
            application.render(window);
        }
        Action::Fullscreen => {
            toggle_fullscreen(application, window);
            application.render(window);
        }
        Action::Open => {
            let last_directory = application.settings.last_file_dialog_directory();
            let paths = open_dialog::show(window, last_directory.as_deref());
            for rest in paths.iter().skip(1) {
                open_in_new_window(rest);
            }
            if let Some(first) = paths.first() {
                if let Some(parent) = first.parent() {
                    application
                        .settings
                        .set_last_file_dialog_directory(&parent.to_string_lossy());
                    unsafe { SetTimer(Some(window), SETTINGS_SAVE_TIMER, 500, None) };
                }
                let first = first.clone();
                open_external_path(application, window, &first);
            }
        }
        Action::OpenUrl => {
            if let Some(url) = dialogs::open_url::show(window) {
                open_external_url(application, window, &url);
            }
        }
        Action::OpenContainingFolder => {
            // The ContainingFile gate keeps URL items out of here.
            if let Some(file) = application
                .image_core
                .current
                .as_ref()
                .and_then(|current| current.location.containing_file())
            {
                file_ops::show_in_explorer(file);
            }
        }
        Action::Delete | Action::DeletePermanent => {
            delete_current_file(application, window, action == Action::DeletePermanent);
        }
        Action::Rename => {
            rename_current_file(application, window);
        }
        Action::OpenWithOther => {
            if let Some(path) = application
                .image_core
                .current
                .as_ref()
                .and_then(|current| current.location.as_file())
            {
                let path = path.to_path_buf();
                open_with::show_open_with_dialog(window, &path);
            }
        }
        Action::OpenWith => {}
        Action::Pause => {
            if let Some(animation) = application.animation.as_mut() {
                animation.paused = !animation.paused;
                let paused = animation.paused;
                if paused {
                    let _ = unsafe { KillTimer(Some(window), ANIMATION_TIMER) };
                } else {
                    let delay = animation.current_delay_milliseconds();
                    unsafe { SetTimer(Some(window), ANIMATION_TIMER, delay, None) };
                }
                // Resuming drops the frame-step pill left by a manual step.
                if !paused && application.dismiss_frame_counter() {
                    application.render(window);
                }
            }
        }
        Action::PreviousFrame => application.step_animation_frame(window, false),
        Action::NextFrame => application.step_animation_frame(window, true),
        Action::DecreaseSpeed | Action::IncreaseSpeed => {
            if let Some(animation) = application.animation.as_mut() {
                animation.adjust_speed(action == Action::IncreaseSpeed);
                let text = format!("Speed: {}%", animation.speed_percent());
                application.show_status_text(window, text);
                application.render(window);
            }
        }
        Action::ResetSpeed => {
            if let Some(animation) = application.animation.as_mut() {
                animation.reset_speed();
                let text = format!("Speed: {}%", animation.speed_percent());
                application.show_status_text(window, text);
                application.render(window);
            }
        }
        Action::Options => {
            dialogs::options::show(window, &application.settings);
        }
    }
}

fn delete_current_file(application: &mut Application, window: HWND, permanent: bool) {
    // The FileOnDisk gate keeps archive members out of here.
    let Some(path) = application
        .image_core
        .current
        .as_ref()
        .and_then(|current| current.location.as_file())
        .map(Path::to_path_buf)
    else {
        return;
    };
    if permanent || application.settings.options.ask_delete {
        let confirmation = file_ops::confirm_delete(window, &path, permanent);
        if !confirmation.confirmed {
            return;
        }
        if !permanent && confirmation.do_not_ask_again {
            application.settings.set_option_boolean("askdelete", false);
            unsafe { SetTimer(Some(window), SETTINGS_SAVE_TIMER, 500, None) };
        }
    }
    let (command, opposite) = if application.settings.options.after_delete == 0 {
        (NavigationCommand::Previous, NavigationCommand::Next)
    } else {
        (NavigationCommand::Next, NavigationCommand::Previous)
    };
    let deleted = ItemLocation::File(path.clone());
    // Compute the after-delete target before the file disappears; folder ends fall back.
    let target = [command, opposite]
        .into_iter()
        .find_map(|direction| {
            application
                .image_core
                .peek_navigation_target(direction)
                .filter(|candidate| *candidate != deleted)
        })
        .and_then(|candidate| candidate.as_file().map(Path::to_path_buf));
    match file_ops::delete_file(&path, permanent) {
        Ok(()) => {
            application.image_core.rescan_listing();
            match target {
                Some(target) => open_external_path(application, window, &target),
                None => {
                    application.image_core.clear_current_item();
                    application.clear_displayed_image(window);
                }
            }
        }
        Err(_) => {
            if application.image_core.reload_current() {
                application.apply_current_image(window);
            }
        }
    }
}

fn rename_current_file(application: &mut Application, window: HWND) {
    let Some(path) = application
        .image_core
        .current
        .as_ref()
        .and_then(|current| current.location.as_file())
        .map(Path::to_path_buf)
    else {
        return;
    };
    let current_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let Some(new_name) = dialogs::rename::show(window, &current_name) else {
        return;
    };
    match file_ops::rename_file(&path, &new_name) {
        Ok(new_path) => {
            application.image_core.rescan_listing();
            open_external_path(application, window, &new_path);
        }
        Err(error) => file_ops::show_rename_error(window, &error),
    }
}

fn toggle_fullscreen(application: &mut Application, window: HWND) {
    unsafe {
        if let Some(placement) = application.fullscreen_restore.take() {
            let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
            SetWindowLongPtrW(window, GWL_STYLE, (style | WS_OVERLAPPEDWINDOW.0) as isize);
            let _ = SetWindowPlacement(window, &raw const placement);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        } else {
            let mut placement = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            let _ = GetWindowPlacement(window, &raw mut placement);
            application.fullscreen_restore = Some(placement);

            let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
            let mut monitor_info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &raw mut monitor_info);
            let bounds = monitor_info.rcMonitor;

            let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
            SetWindowLongPtrW(window, GWL_STYLE, (style & !WS_OVERLAPPEDWINDOW.0) as isize);
            let _ = SetWindowPos(
                window,
                Some(HWND_TOP),
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_FRAMECHANGED,
            );
        }
    }
    application.update_cursor_autohide(window);
}

fn handle_key(application: &mut Application, window: HWND, virtual_key: u16) -> bool {
    if [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|modifier| modifier.0 == virtual_key)
    {
        return false;
    }
    let modifiers = current_modifiers();
    if let Some(action) = application.bindings.lookup_key(modifiers, virtual_key) {
        dispatch_action(application, window, action);
        return true;
    }
    if virtual_key == VK_ESCAPE.0
        && application.bindings.escape_is_unbound()
        && application.fullscreen_restore.is_some()
    {
        toggle_fullscreen(application, window);
        application.render(window);
        return true;
    }
    // Fixed paste-to-open key; user bindings on Ctrl+V take precedence above.
    if modifiers == MODIFIER_CONTROL && virtual_key == u16::from(b'V') {
        return paste_open_url(application, window);
    }
    false
}

fn handle_wheel(application: &mut Application, window: HWND, wheel_delta: i16) {
    let modifiers = current_modifiers();
    // Fine-grained deltas with no modifiers read as touchpad panning.
    if wheel_delta % 120 != 0 && modifiers & !MODIFIER_SHIFT == 0 {
        let amount = f32::from(wheel_delta) / 2.0;
        if modifiers & MODIFIER_SHIFT != 0 {
            application.pan_by(window, amount, 0.0);
        } else {
            application.pan_by(window, 0.0, amount);
        }
        return;
    }
    let base = if wheel_delta > 0 {
        MouseBase::WheelUp
    } else {
        MouseBase::WheelDown
    };
    let Some(action) = application.bindings.lookup_mouse(modifiers, false, base) else {
        return;
    };
    if !application.gate_satisfied(action.gate()) {
        return;
    }
    let pan_amount = f32::from(wheel_delta.abs()) / 2.0;
    match action {
        Action::ZoomIn | Action::ZoomOut => application.wheel_zoom(window, wheel_delta),
        Action::PanUp => application.pan_by(window, 0.0, pan_amount),
        Action::PanDown => application.pan_by(window, 0.0, -pan_amount),
        Action::PanLeft => application.pan_by(window, pan_amount, 0.0),
        Action::PanRight => application.pan_by(window, -pan_amount, 0.0),
        action => {
            let notches = application.accumulate_wheel_notches(wheel_delta);
            for _ in 0..notches.abs() {
                dispatch_action(application, window, action);
            }
        }
    }
}

fn handle_gesture(application: &mut Application, window: HWND, lparam: LPARAM) -> bool {
    use windows::Win32::UI::Input::Touch::{
        CloseGestureInfoHandle, GESTUREINFO, GID_PAN, GID_ZOOM, GetGestureInfo, HGESTUREINFO,
    };
    const GF_BEGIN: u32 = 0x1;

    let handle = HGESTUREINFO(lparam.0 as *mut _);
    let mut information = GESTUREINFO {
        cbSize: size_of::<GESTUREINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGestureInfo(handle, &raw mut information) }.is_err() {
        return false;
    }
    let began = information.dwFlags & GF_BEGIN != 0;
    let handled = match information.dwID {
        identifier if identifier == GID_ZOOM.0 => {
            let distance = (information.ullArguments & 0xFFFF_FFFF) as f32;
            if began {
                application.gesture_zoom_distance = Some(distance);
            } else if let Some(previous) = application.gesture_zoom_distance.replace(distance)
                && previous > 0.0
            {
                let mut hotpoint = POINT {
                    x: i32::from(information.ptsLocation.x),
                    y: i32::from(information.ptsLocation.y),
                };
                let _ = unsafe { ScreenToClient(window, &raw mut hotpoint) };
                let (width, height) = client_size(window);
                let anchor = (
                    hotpoint.x as f32 - width as f32 / 2.0,
                    hotpoint.y as f32 - height as f32 / 2.0,
                );
                application.zoom_at(window, distance / previous, Some(anchor));
            }
            true
        }
        identifier if identifier == GID_PAN.0 => {
            let position = (
                i32::from(information.ptsLocation.x),
                i32::from(information.ptsLocation.y),
            );
            if began {
                application.gesture_pan_point = Some(position);
            } else if let Some(previous) = application.gesture_pan_point.replace(position) {
                application.pan_by(
                    window,
                    (position.0 - previous.0) as f32,
                    (position.1 - previous.1) as f32,
                );
            }
            true
        }
        _ => false,
    };
    if handled {
        let _ = unsafe { CloseGestureInfoHandle(handle) };
    }
    handled
}

fn main() -> Result<()> {
    unsafe { OleInitialize(None) }?;

    if process_is_elevated() {
        fail_fast_dialog(
            "riv does not run elevated",
            "Start riv from a normal user session instead.",
        );
        return Ok(());
    }
    if !settings::probe_writable() {
        fail_fast_dialog(
            "Settings cannot be saved here",
            "riv stores riv.json next to the executable, but this folder is not writable. \
             Move riv to a writable folder and run it again.",
        );
        return Ok(());
    }

    window::menu_theme::enable_dark_menus();

    let argument_path = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    let instance = unsafe { GetModuleHandleW(None)? };
    let class_name = w!("riv");

    let application_icon = unsafe { LoadIconW(Some(instance.into()), APPLICATION_ICON_ID)? };
    let window_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(window_procedure),
        hInstance: instance.into(),
        hIcon: application_icon,
        hIconSm: application_icon,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
        lpszClassName: class_name,
        ..Default::default()
    };
    let class_atom = unsafe { RegisterClassExW(&raw const window_class) };
    assert!(class_atom != 0, "RegisterClassExW failed");

    create_main_window(argument_path.as_deref())?;

    let mut message = MSG::default();
    loop {
        // -1 is an error, not a message; as_bool would dispatch a stale MSG.
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        let _ = unsafe { TranslateMessage(&raw const message) };
        unsafe { DispatchMessageW(&raw const message) };
    }
    Ok(())
}

fn create_main_window(initial_path: Option<&Path>) -> Result<HWND> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let (default_x, default_y) =
        window::work_area_centered_origin(640, 480).unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("riv"),
            w!("riv"),
            WS_OVERLAPPEDWINDOW,
            default_x,
            default_y,
            640,
            480,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };

    let application = Box::new(Application::new(window, initial_path)?);
    unsafe {
        SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(application) as isize);
    }
    {
        use windows::Win32::System::SystemServices::{GC_PAN, GC_ZOOM};
        use windows::Win32::UI::Input::Touch::{
            GESTURECONFIG, GID_PAN, GID_ZOOM, SetGestureConfig,
        };
        let configurations = [
            GESTURECONFIG {
                dwID: GID_ZOOM,
                dwWant: GC_ZOOM.0,
                dwBlock: 0,
            },
            GESTURECONFIG {
                dwID: GID_PAN,
                dwWant: GC_PAN.0,
                dwBlock: 0,
            },
        ];
        let _ = unsafe {
            SetGestureConfig(
                window,
                0,
                &configurations,
                size_of::<GESTURECONFIG>() as u32,
            )
        };
    }
    if let Some(application) = application_from_window(window) {
        application.restore_window_geometry(window);
        dwm::apply_title_bar_theme(window);
        application.drop_target = drag_drop::register(window).ok();
        application.render(window);
        // Presented before the first show, so the class brush never flashes.
        let _ = unsafe { PostMessageW(Some(window), WM_APP_SHOW_WINDOW, WPARAM(0), LPARAM(0)) };
    }
    Ok(window)
}

fn open_in_new_window(path: &Path) {
    if let Ok(executable) = std::env::current_exe() {
        let _ = std::process::Command::new(executable).arg(path).spawn();
    }
}

/// TokenElevation misreports admin accounts with UAC off; check the elevation type.
fn process_is_elevated() -> bool {
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TokenElevationType,
        TokenElevationTypeFull,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = windows::Win32::Foundation::HANDLE::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.is_err() {
        return false;
    }
    let mut elevation_type = TOKEN_ELEVATION_TYPE::default();
    let mut returned = 0u32;
    let elevated = unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            Some((&raw mut elevation_type).cast()),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &raw mut returned,
        )
    }
    .is_ok()
        && elevation_type == TokenElevationTypeFull;
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(token) };
    elevated
}

fn fail_fast_dialog(instruction: &str, content: &str) {
    use windows::Win32::UI::Controls::{TASKDIALOGCONFIG, TDCBF_CLOSE_BUTTON, TaskDialogIndirect};

    let instruction_wide = crate::text::wide(instruction);
    let content_wide = crate::text::wide(content);
    let configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        pszWindowTitle: w!("riv"),
        pszMainInstruction: PCWSTR(instruction_wide.as_ptr()),
        pszContent: PCWSTR(content_wide.as_ptr()),
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        ..Default::default()
    };
    let _ = unsafe { TaskDialogIndirect(&raw const configuration, None, None, None) };
}

fn application_from_window(window: HWND) -> Option<&'static mut Application> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut Application;
    unsafe { pointer.as_mut() }
}

extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => {
            if let Some(application) = application_from_window(window) {
                let width = (lparam.0 & 0xFFFF) as u32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as u32;
                if width > 0 && height > 0 {
                    let resized = application
                        .renderer
                        .as_mut()
                        .is_some_and(|renderer| renderer.resize(width, height).is_ok());
                    if !resized {
                        let _ = application.rebuild_renderer(window);
                    }
                    application.render(window);
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let _ = unsafe { ValidateRect(Some(window), None) };
            LRESULT(0)
        }
        WM_APP_DECODE_COMPLETE => {
            let completion = unsafe { Box::from_raw(lparam.0 as *mut DecodeCompletion) };
            if let Some(application) = application_from_window(window)
                && application.image_core.on_decode_complete(*completion)
            {
                if application.image_core.load_error.is_some() {
                    application.apply_load_error(window);
                } else {
                    application.apply_current_image(window);
                }
            }
            LRESULT(0)
        }
        WM_APP_DOWNLOAD_PROGRESS => {
            let progress = unsafe { Box::from_raw(lparam.0 as *mut DownloadProgress) };
            if let Some(application) = application_from_window(window) {
                application.apply_download_progress(window, *progress);
            }
            LRESULT(0)
        }
        WM_APP_DROP_PATHS => {
            let paths = unsafe { Box::from_raw(lparam.0 as *mut Vec<std::path::PathBuf>) };
            for rest in paths.iter().skip(1) {
                open_in_new_window(rest);
            }
            if let Some(application) = application_from_window(window)
                && let Some(first) = paths.first()
            {
                open_external_path(application, window, first);
            }
            LRESULT(0)
        }
        WM_APP_OPTIONS_APPLIED => {
            let payload = unsafe { &*(lparam.0 as *const dialogs::options::AppliedOptions) };
            if let Some(application) = application_from_window(window) {
                application.settings.set_options(&payload.options);
                application
                    .settings
                    .set_binding_overrides(&payload.keyboard, &payload.mouse);
                let _ = application.settings.save();
                application.apply_options(window);
                application.render(window);
            }
            LRESULT(0)
        }
        WM_APP_OPTIONS_GEOMETRY => {
            if let Some(application) = application_from_window(window) {
                let x = (lparam.0 & 0xFFFF_FFFF) as u32 as i32;
                let y = (lparam.0 >> 32) as i32;
                application.settings.set_options_geometry(x, y);
                let _ = application.settings.save();
            }
            LRESULT(0)
        }
        WM_APP_SHOW_WINDOW => {
            if let Some(application) = application_from_window(window) {
                application.ensure_window_shown(window);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ANIMATION_TIMER => {
            if let Some(application) = application_from_window(window) {
                application.play_animation_frame(window);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == CURSOR_HIDE_TIMER => {
            let _ = unsafe { KillTimer(Some(window), CURSOR_HIDE_TIMER) };
            if let Some(application) = application_from_window(window) {
                application.hide_cursor_if_idle(window);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == STATUS_TEXT_TIMER => {
            let _ = unsafe { KillTimer(Some(window), STATUS_TEXT_TIMER) };
            if let Some(application) = application_from_window(window)
                && matches!(application.status_text, Some(StatusText::Timed(_)))
            {
                application.status_text = None;
                application.render(window);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == SLIDESHOW_TIMER => {
            if let Some(application) = application_from_window(window) {
                let command = if application.settings.options.slideshow_reversed {
                    NavigationCommand::Previous
                } else {
                    NavigationCommand::Next
                };
                if !execute_navigation(application, window, command) {
                    application.cancel_slideshow(window);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == SETTINGS_SAVE_TIMER => {
            let _ = unsafe { KillTimer(Some(window), SETTINGS_SAVE_TIMER) };
            if let Some(application) = application_from_window(window) {
                let _ = application.settings.save();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == OPEN_WITH_TIMER => {
            let _ = unsafe { KillTimer(Some(window), OPEN_WITH_TIMER) };
            if let Some(application) = application_from_window(window)
                && let Some(path) = application
                    .image_core
                    .current
                    .as_ref()
                    .and_then(|current| current.location.as_file())
            {
                open_with::enumerate_in_background(window, path.to_path_buf());
            }
            LRESULT(0)
        }
        WM_APP_OPEN_WITH_LIST => {
            let list = unsafe { Box::from_raw(lparam.0 as *mut OpenWithList) };
            if let Some(application) = application_from_window(window) {
                let is_current = application
                    .image_core
                    .current
                    .as_ref()
                    .and_then(|current| current.location.as_file())
                    .is_some_and(|path| {
                        path.to_string_lossy()
                            .eq_ignore_ascii_case(&list.path.to_string_lossy())
                    });
                if is_current {
                    application.open_with_list = Some(list);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let handled = application_from_window(window)
                .is_some_and(|application| handle_key(application, window, wparam.0 as u16));
            if !handled && message == WM_SYSKEYDOWN {
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            } else {
                LRESULT(0)
            }
        }
        // Swallow Alt+chars consumed by bindings; DefWindowProc would beep.
        WM_SYSCHAR => {
            let character = char::from_u32(wparam.0 as u32).unwrap_or('\0');
            let bound = character.is_ascii_alphanumeric()
                && application_from_window(window).is_some_and(|application| {
                    application
                        .bindings
                        .lookup_key(current_modifiers(), character.to_ascii_uppercase() as u16)
                        .is_some()
                });
            if bound {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
        }
        WM_GESTURE => {
            if let Some(application) = application_from_window(window)
                && handle_gesture(application, window, lparam)
            {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_MOUSEHWHEEL => {
            if let Some(application) = application_from_window(window) {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                application.pan_by(window, f32::from(delta) / -2.0, 0.0);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(application) = application_from_window(window) {
                let wheel_delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                handle_wheel(application, window, wheel_delta);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(application) = application_from_window(window) {
                let move_window = current_modifiers() == MODIFIER_CONTROL
                    && application.settings.options.control_drag_window
                    && application.fullscreen_restore.is_none()
                    && !unsafe { IsZoomed(window) }.as_bool();
                if move_window {
                    let _ = unsafe { ReleaseCapture() };
                    unsafe {
                        SendMessageW(
                            window,
                            WM_NCLBUTTONDOWN,
                            Some(WPARAM(HTCAPTION as usize)),
                            Some(LPARAM(0)),
                        )
                    };
                } else {
                    unsafe { SetCapture(window) };
                    if !application.cursor_hidden {
                        unsafe { SetCursor(Some(application.pan_cursor)) };
                    }
                    application.pan_drag_position = Some((
                        (lparam.0 & 0xFFFF) as u16 as i16 as i32,
                        ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
                    ));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(application) = application_from_window(window) {
                let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                application.record_pointer_activity(window, (x, y));
                if let Some((last_x, last_y)) = application.pan_drag_position {
                    application.pan_drag_position = Some((x, y));
                    application.pan_by(window, (x - last_x) as f32, (y - last_y) as f32);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(application) = application_from_window(window)
                && application.pan_drag_position.take().is_some()
            {
                let _ = unsafe { ReleaseCapture() };
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(application) = application_from_window(window) {
                let over_client = (lparam.0 & 0xFFFF) as u32 == HTCLIENT;
                if application.pan_drag_position.is_some() {
                    unsafe { SetCursor(Some(application.pan_cursor)) };
                    return LRESULT(1);
                }
                if application.cursor_hidden && over_client {
                    unsafe { SetCursor(None) };
                    return LRESULT(1);
                }
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_LBUTTONDBLCLK => {
            if let Some(application) = application_from_window(window)
                && let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), true, MouseBase::Left)
            {
                dispatch_action(application, window, action);
            }
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            if let Some(application) = application_from_window(window)
                && let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), false, MouseBase::Middle)
            {
                dispatch_action(application, window, action);
            }
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            if let Some(application) = application_from_window(window) {
                let base = if (wparam.0 >> 16) & 0xFFFF == 1 {
                    MouseBase::Back
                } else {
                    MouseBase::Forward
                };
                if let Some(action) =
                    application
                        .bindings
                        .lookup_mouse(current_modifiers(), false, base)
                {
                    dispatch_action(application, window, action);
                }
            }
            LRESULT(1) // handled: prevent default app-command translation
        }
        WM_CONTEXTMENU => {
            if let Some(application) = application_from_window(window) {
                let mut x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
                let mut y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                if x == -1 && y == -1 {
                    let mut bounds = RECT::default();
                    let _ = unsafe { GetWindowRect(window, &raw mut bounds) };
                    x = (bounds.left + bounds.right) / 2;
                    y = (bounds.top + bounds.bottom) / 2;
                }
                if application.settings.prune_recent_files() {
                    unsafe { SetTimer(Some(window), SETTINGS_SAVE_TIMER, 500, None) };
                }
                let playlist = application
                    .image_core
                    .playlist_window(context_menu::PLAYLIST_CAPACITY);
                let state = MenuState {
                    has_image: application.image_core.current.is_some(),
                    has_file_on_disk: application
                        .image_core
                        .current
                        .as_ref()
                        .is_some_and(|current| current.location.as_file().is_some()),
                    has_containing_file: application
                        .image_core
                        .current
                        .as_ref()
                        .is_some_and(|current| current.location.containing_file().is_some()),
                    has_navigation_targets: application.image_core.has_navigation_targets(),
                    file_info_shown: application.show_file_info,
                    loop_enabled: application.settings.options.loop_within_folder,
                    open_url_available: curl::available(),
                    playlist_names: playlist.names,
                    playlist_first_index: playlist.first_index,
                    playlist_current_slot: playlist.current_slot,
                    playlist_hidden_count: playlist.hidden_count,
                    has_animation: application
                        .image_core
                        .current
                        .as_ref()
                        .is_some_and(|current| current.image.frames.len() > 1),
                    animation_paused: application
                        .animation
                        .as_ref()
                        .is_some_and(|animation| animation.paused),
                    fit_height: application.settings.options.fit_mode == 1,
                    preserve_zoom: application.preserve_zoom,
                    always_on_top: application.always_on_top,
                    mirrored: application.view_transform.mirrored,
                    flipped: application.view_transform.flipped,
                    fullscreen: application.fullscreen_restore.is_some(),
                    slideshow_active: application.slideshow_active,
                    recent_names: application
                        .settings
                        .recent_files()
                        .into_iter()
                        .map(|(name, _)| name)
                        .collect(),
                    open_with_items: application.open_with_list.as_ref().map_or_else(
                        Vec::new,
                        |list| {
                            list.items
                                .iter()
                                .map(|item| item.display_name.clone())
                                .collect()
                        },
                    ),
                    open_with_has_default: application
                        .open_with_list
                        .as_ref()
                        .is_some_and(|list| list.has_default),
                    shortcuts: Action::all_bindable()
                        .filter_map(|action| {
                            bindings::menu_shortcut_text(
                                application.settings.keyboard_bindings(),
                                application.settings.mouse_bindings(),
                                action.name(),
                            )
                            .map(|text| (action.name(), text))
                        })
                        .collect(),
                };
                let selection = context_menu::show(window, state, x, y);
                // The menu pumped messages; re-fetch in case the window was destroyed.
                if let Some(selection) = selection
                    && let Some(application) = application_from_window(window)
                {
                    match selection {
                        MenuSelection::Action(action) => {
                            dispatch_action(application, window, action);
                        }
                        MenuSelection::OpenWithEntry(index) => {
                            if let (Some(path), Some(list)) = (
                                application
                                    .image_core
                                    .current
                                    .as_ref()
                                    .and_then(|current| current.location.as_file()),
                                application.open_with_list.as_ref(),
                            ) && let Some(item) = list.items.get(index)
                            {
                                let _ = open_with::invoke(path, &item.executable_path);
                            }
                        }
                        MenuSelection::PlaylistEntry(index) => {
                            let result = application.image_core.navigate_to_entry(index);
                            apply_navigation_result(application, window, result);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATEAPP => {
            if wparam.0 != 0
                && let Some(application) = application_from_window(window)
                && application.settings.reload()
            {
                application.apply_options(window);
            }
            LRESULT(0)
        }
        WM_MOVE | WM_DISPLAYCHANGE => {
            if let Some(application) = application_from_window(window) {
                application.refresh_display_color_state(window);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if let Some(application) = application_from_window(window) {
                let ratio = (wparam.0 & 0xFFFF) as f32 / 96.0;
                application.overlay.set_scale(ratio);
            }
            let suggested_bounds = unsafe { &*(lparam.0 as *const RECT) };
            let _ = unsafe {
                SetWindowPos(
                    window,
                    None,
                    suggested_bounds.left,
                    suggested_bounds.top,
                    suggested_bounds.right - suggested_bounds.left,
                    suggested_bounds.bottom - suggested_bounds.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            LRESULT(0)
        }
        WM_CLOSE => {
            // A modal disables its owner; dropping WM_CLOSE avoids freeing it mid-pump.
            if !unsafe { windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(window) }
                .as_bool()
            {
                return LRESULT(0);
            }
            if let Some(application) = application_from_window(window) {
                application.save_window_geometry(window);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_SETTINGCHANGE => {
            dwm::apply_title_bar_theme(window);
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_NCDESTROY => {
            let _ = unsafe { RevokeDragDrop(window) };
            let pointer =
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) } as *mut Application;
            if !pointer.is_null() {
                drop(unsafe { Box::from_raw(pointer) });
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Needs riv.exe and System32 curl.exe; posted messages only — wine focus is nondeterministic.
#[cfg(test)]
mod open_url_smoke_tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowTextW, PostMessageW, SetDlgItemTextW, WM_CLOSE, WM_COMMAND,
        WM_KEYDOWN,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use crate::dialogs;
    use crate::network::curl;

    const EXECUTABLE: &str = "target/x86_64-pc-windows-msvc/debug/riv.exe";
    const SETTINGS: &str = "target/x86_64-pc-windows-msvc/debug/riv.json";

    fn png_bytes() -> Vec<u8> {
        let mut data = Vec::new();
        let mut encoder = png::Encoder::new(&mut data, 4, 4);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer
            .write_image_data(&[0, 255, 0, 255].repeat(16))
            .expect("png data");
        writer.finish().expect("png finish");
        data
    }

    fn serve(mut stream: TcpStream, body: &[u8]) {
        let mut request = [0u8; 2048];
        let length = stream.read(&mut request).unwrap_or(0);
        let target_found = request[..length]
            .windows("GET /test.png".len())
            .any(|window| window == b"GET /test.png");
        let response = if target_found {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            [header.as_bytes(), body].concat()
        } else {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        };
        stream.write_all(&response).expect("respond");
    }

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>, seconds: u64) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(value) = probe() {
                return Some(value);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn window_title(window: HWND) -> String {
        let mut buffer = [0u16; 256];
        let length = unsafe { GetWindowTextW(window, &mut buffer) } as usize;
        String::from_utf16_lossy(&buffer[..length])
    }

    #[test]
    #[ignore = "needs built riv.exe and System32 curl.exe"]
    fn an_entered_url_downloads_and_displays() {
        assert!(curl::available(), "curl.exe unavailable");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local address").port();
        let body = png_bytes();
        // Detached on purpose: retried entries may fetch more than once.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &body);
            }
        });
        // Bind the dialog to a plain key so a posted WM_KEYDOWN can open it.
        std::fs::write(SETTINGS, r#"{"keyboardbindings":{"openurl":["U"]}}"#)
            .expect("write riv.json");
        let mut app = std::process::Command::new(EXECUTABLE)
            .spawn()
            .expect("riv.exe spawn (build first, run from the repo root)");
        let window =
            wait_for(|| unsafe { FindWindowW(None, w!("riv")) }.ok(), 15).expect("riv window");
        // One keypress only: each opens a modal dialog, and stacking them deadlocks.
        let key = WPARAM(usize::from(u16::from(b'U')));
        let _ = unsafe { PostMessageW(Some(window), WM_KEYDOWN, key, LPARAM(0)) };
        let dialog = wait_for(|| unsafe { FindWindowW(None, w!("Open URL")) }.ok(), 15)
            .expect("Open URL dialog");
        let url = HSTRING::from(format!("http://127.0.0.1:{port}/test.png"));
        unsafe {
            SetDlgItemTextW(
                dialog,
                dialogs::text_input::EDIT_IDENTIFIER,
                PCWSTR(url.as_ptr()),
            )
            .expect("set dialog text");
            PostMessageW(Some(dialog), WM_COMMAND, WPARAM(1), LPARAM(0)).expect("post IDOK");
        }
        let title_became_file =
            wait_for(|| (window_title(window) == "test.png").then_some(()), 20).is_some();
        let final_title = window_title(window);
        let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let _ = app.wait();
        let _ = std::fs::remove_file(SETTINGS);
        assert!(
            title_became_file,
            "title never became test.png (was {final_title:?})"
        );
    }
}
