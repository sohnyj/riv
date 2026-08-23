//! The window's position and size at its own DPI.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, USER_DEFAULT_SCREEN_DPI, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// The window's dots per inch, falling back to the screen default when the query fails.
pub fn dpi_for_window(window: HWND) -> u32 {
    match unsafe { GetDpiForWindow(window) } {
        0 => USER_DEFAULT_SCREEN_DPI,
        dpi => dpi,
    }
}

/// Window size holding a logical client size, framed and scaled at the window's own DPI.
pub fn window_size_for_client(window: HWND, width: i32, height: i32) -> Option<(i32, i32)> {
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
    .ok()?;
    Some((
        window_bounds.right - window_bounds.left,
        window_bounds.bottom - window_bounds.top,
    ))
}
