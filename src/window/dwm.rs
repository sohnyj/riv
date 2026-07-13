//! DWM window attributes; failures (e.g. wine) are ignored.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};

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

/// Dark title bar follows the system app theme (AppsUseLightTheme).
pub fn apply_title_bar_theme(window: HWND) {
    let dark: i32 = i32::from(system_apps_use_dark_theme());
    set_attribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark);
}

fn system_apps_use_dark_theme() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;

    let mut value = 1u32;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    result == windows::Win32::Foundation::ERROR_SUCCESS && value == 0
}
