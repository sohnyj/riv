//! Where a dialog is placed and where its controls sit.

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MapWindowPoints, MonitorFromRect,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

/// Holds a placement inside the work area of the nearest monitor.
fn clamp_to_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let target = RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    };
    let monitor = unsafe { MonitorFromRect(&raw const target, MONITOR_DEFAULTTONEAREST) };
    let mut information = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &raw mut information) }.as_bool() {
        return (x, y);
    }
    let work_area = information.rcWork;
    // Clamped low last, so a dialog taller than the work area keeps its top left corner.
    (
        x.min(work_area.right - width).max(work_area.left),
        y.min(work_area.bottom - height).max(work_area.top),
    )
}

pub fn center_on_owner(dialog: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GW_OWNER, GetWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    // GetParent is documented to fail for an owner with WS_POPUP, which the settings dialog is.
    let Ok(owner) = (unsafe { GetWindow(dialog, GW_OWNER) }) else {
        return;
    };
    let mut owner_bounds = RECT::default();
    let mut dialog_bounds = RECT::default();
    if unsafe { GetWindowRect(owner, &raw mut owner_bounds) }.is_err()
        || unsafe { GetWindowRect(dialog, &raw mut dialog_bounds) }.is_err()
    {
        return;
    }
    let width = dialog_bounds.right - dialog_bounds.left;
    let height = dialog_bounds.bottom - dialog_bounds.top;
    let x = owner_bounds.left + (owner_bounds.right - owner_bounds.left - width) / 2;
    let y = owner_bounds.top + (owner_bounds.bottom - owner_bounds.top - height) / 2;
    // An owner at a screen edge centers the dialog past it.
    let (x, y) = clamp_to_work_area(x, y, width, height);
    let _ = unsafe {
        SetWindowPos(
            dialog,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

/// Bounds of a child control in its parent's client coordinates.
pub fn control_bounds(parent: HWND, control: HWND) -> Option<RECT> {
    let mut bounds = RECT::default();
    if unsafe { GetWindowRect(control, &raw mut bounds) }.is_err() {
        return None;
    }
    let mut corners = [
        POINT {
            x: bounds.left,
            y: bounds.top,
        },
        POINT {
            x: bounds.right,
            y: bounds.bottom,
        },
    ];
    unsafe { MapWindowPoints(None, Some(parent), &mut corners) };
    Some(RECT {
        left: corners[0].x,
        top: corners[0].y,
        right: corners[1].x,
        bottom: corners[1].y,
    })
}
