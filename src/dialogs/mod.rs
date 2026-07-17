pub mod about;
pub mod open_url;
pub mod options;
pub mod rename;
pub mod resource;
pub mod shortcut_capture;
pub mod text_input;

use windows::Win32::Foundation::HWND;

/// Center a dialog within its owner window.
pub(crate) fn center_on_owner(dialog: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetParent, GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    let Ok(owner) = (unsafe { GetParent(dialog) }) else {
        return;
    };
    let mut owner_bounds = windows::Win32::Foundation::RECT::default();
    let mut dialog_bounds = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(owner, &raw mut owner_bounds) }.is_err()
        || unsafe { GetWindowRect(dialog, &raw mut dialog_bounds) }.is_err()
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
