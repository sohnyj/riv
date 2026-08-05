pub mod about;
pub mod open_url;
pub mod options;
pub mod rename;
pub mod resource;
pub mod shortcut_capture;
pub mod text_input;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOGCONFIG, TDF_POSITION_RELATIVE_TO_WINDOW,
    TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, WINDOW_LONG_PTR_INDEX};
use windows::core::PCWSTR;

pub const IDOK: usize = 1;
pub const IDCANCEL: usize = 2;
/// DWLP_DLGPROC (8) + 8 on x64; windows-rs does not export it.
pub const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

/// Dialog state stored at DWLP_USER by WM_INITDIALOG.
pub fn state_mut<State>(dialog: HWND) -> Option<&'static mut State> {
    let pointer = unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut State;
    unsafe { pointer.as_mut() }
}

/// Center a dialog within its owner window.
pub fn center_on_owner(dialog: HWND) {
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

/// One-message task dialog titled after the action; `text` is the whole message.
pub fn show_message(owner: Option<HWND>, title: &str, text: &str, button: &str) {
    let title = crate::text::wide(title);
    // A main instruction would draw the first line larger and in color.
    let text = crate::text::wide(text);
    // Labeled here, not by the system: the settings dialog writes its own buttons too.
    let button_text = crate::text::wide(button);
    let buttons = [TASKDIALOG_BUTTON {
        nButtonID: IDOK as i32,
        pszButtonText: PCWSTR(button_text.as_ptr()),
    }];
    // Without the flag a task dialog centers on the monitor, owner or not.
    let placement = match owner {
        Some(_) => TDF_POSITION_RELATIVE_TO_WINDOW,
        None => TASKDIALOG_FLAGS(0),
    };
    let configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: placement,
        hwndParent: owner.unwrap_or_default(),
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        ..Default::default()
    };
    let _ = unsafe { TaskDialogIndirect(&raw const configuration, None, None, None) };
}
