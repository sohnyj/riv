//! DWM window attributes and the system theme hook; failures (e.g. wine) are ignored.

use windows::Foundation::TypedEventHandler;
use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::IInspectable;

fn set_attribute<T>(
    window: HWND,
    attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE,
    value: &T,
) {
    let _ = unsafe {
        DwmSetWindowAttribute(
            window,
            attribute,
            (value as *const T).cast(),
            size_of::<T>() as u32,
        )
    };
}

/// Draws the title bar in dark or light mode; the caller gates on the value changing.
pub fn apply_title_bar_theme(window: HWND, dark: bool) {
    let dark: i32 = i32::from(dark);
    set_attribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark);
}

/// The system color hook: dark-mode reads plus a posted change message.
pub struct ThemeWatcher {
    settings: UISettings,
    change_token: i64,
}

impl ThemeWatcher {
    /// Hooks the system color settings; color changes post `message` to `window`.
    pub fn new(window: HWND, message: u32) -> Option<Self> {
        let settings = UISettings::new().ok()?;
        let window_value = window.0 as isize;
        let handler = TypedEventHandler::<UISettings, IInspectable>::new(move |_, _| {
            let window = HWND(window_value as *mut core::ffi::c_void);
            let _ = unsafe { PostMessageW(Some(window), message, WPARAM(0), LPARAM(0)) };
            Ok(())
        });
        let change_token = settings.ColorValuesChanged(&handler).ok()?;
        Some(Self {
            settings,
            change_token,
        })
    }

    /// Dark mode shows a light foreground; brightness per the perceived-luminance weights.
    pub fn apps_use_dark_theme(&self) -> bool {
        self.settings
            .GetColorValue(UIColorType::Foreground)
            .is_ok_and(|color| {
                5 * u32::from(color.G) + 2 * u32::from(color.R) + u32::from(color.B) > 8 * 128
            })
    }

    /// Unhooks the change handler; the settings object dies with the watcher.
    pub fn close(&self) {
        let _ = self.settings.RemoveColorValuesChanged(self.change_token);
    }
}
