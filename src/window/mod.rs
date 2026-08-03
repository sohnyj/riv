pub mod context_menu;
pub mod dwm;
pub mod menu_theme;
pub mod overlay;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongPtrW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// Centered origin within the primary work area, when available.
pub fn work_area_centered_origin(width: i32, height: i32) -> Option<(i32, i32)> {
    let mut work_area = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut work_area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    Some((
        work_area.left + (work_area.right - work_area.left - width).max(0) / 2,
        work_area.top + (work_area.bottom - work_area.top - height).max(0) / 2,
    ))
}

/// The window's dots per inch, falling back to the screen default when the query fails.
pub fn dpi_for_window(window: HWND) -> u32 {
    match unsafe { GetDpiForWindow(window) } {
        0 => 96,
        dpi => dpi,
    }
}

/// Window size holding a logical client size, framed and scaled at the window's own DPI.
pub fn window_size_for_client(window: HWND, width: i32, height: i32) -> Option<(i32, i32)> {
    let dpi = dpi_for_window(window);
    let scale = |logical: i32| logical * dpi as i32 / 96;
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

/// Posts an owned payload; reclaims it when the message cannot be delivered.
pub fn post_boxed<T>(window: isize, message: u32, payload: Box<T>) {
    let pointer = Box::into_raw(payload);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(window as *mut core::ffi::c_void)),
            message,
            WPARAM(0),
            LPARAM(pointer as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(pointer) });
    }
}
