pub mod about;
pub mod options;
pub mod rename;
pub mod resource;
pub mod shortcut_capture;

use windows::Win32::Foundation::HWND;

/// 소유 창 중앙 배치 (SPEC §6.4 — Rename·About 공용, 2026-07-11)
pub(crate) fn center_on_owner(dialog: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetParent, GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    let Ok(owner) = (unsafe { GetParent(dialog) }) else {
        return;
    };
    let mut owner_bounds = windows::Win32::Foundation::RECT::default();
    let mut dialog_bounds = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(owner, &mut owner_bounds) }.is_err()
        || unsafe { GetWindowRect(dialog, &mut dialog_bounds) }.is_err()
    {
        return;
    }
    let x = owner_bounds.left
        + (owner_bounds.right - owner_bounds.left - (dialog_bounds.right - dialog_bounds.left)) / 2;
    let y = owner_bounds.top
        + (owner_bounds.bottom - owner_bounds.top - (dialog_bounds.bottom - dialog_bounds.top)) / 2;
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
