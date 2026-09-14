//! DPI queries and DPI-scaled window sizing.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, USER_DEFAULT_SCREEN_DPI, WINDOW_EX_STYLE, WINDOW_STYLE,
};

pub fn dpi_for_window(window: HWND) -> u32 {
    unsafe { GetDpiForWindow(window) }
}

/// Window size holding a logical client size, framed and scaled at the window's own DPI.
pub fn window_size_for_client(window: HWND, width: i32, height: i32) -> (i32, i32) {
    let dpi = dpi_for_window(window);
    let scale = |logical: i32| logical * dpi as i32 / USER_DEFAULT_SCREEN_DPI as i32;
    let mut window_bounds = RECT {
        left: 0,
        top: 0,
        right: scale(width),
        bottom: scale(height),
    };
    let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32);
    unsafe {
        AdjustWindowRectExForDpi(
            &raw mut window_bounds,
            style,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    }
    .expect("the frame size of an existing window at its own DPI");
    (
        window_bounds.right - window_bounds.left,
        window_bounds.bottom - window_bounds.top,
    )
}
