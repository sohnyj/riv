//! The one-message dialog the rest of the program reports failures through.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_POSITION_RELATIVE_TO_WINDOW, TaskDialogIndirect,
};
use windows::core::{HSTRING, PCWSTR};

use crate::dialogs::modal::{IDCANCEL, IDOK};

/// The dismiss label every plain failure dialog passes.
pub const CLOSE_BUTTON: &str = "Close";

/// One-message task dialog titled after the action; the headline leads the message.
pub fn show_message(owner: Option<HWND>, title: &str, headline: &str, detail: &str, button: &str) {
    let title = HSTRING::from(title);
    // A main instruction would draw the headline larger and in color.
    let text = HSTRING::from(format!("{headline}\n\n{detail}"));
    // Labeled here, not by the system: the settings dialog writes its own buttons too.
    let button_text = HSTRING::from(button);
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

/// Two-button dialog defaulting to the rejecting answer; true when the accepting one was pressed.
pub fn confirm_message(
    owner: Option<HWND>,
    title: &str,
    headline: &str,
    detail: &str,
    accept_button: &str,
    reject_button: &str,
) -> bool {
    let title = HSTRING::from(title);
    let text = HSTRING::from(format!("{headline}\n\n{detail}"));
    let accept_text = HSTRING::from(accept_button);
    let reject_text = HSTRING::from(reject_button);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: IDOK as i32,
            pszButtonText: PCWSTR(accept_text.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDCANCEL as i32,
            pszButtonText: PCWSTR(reject_text.as_ptr()),
        },
    ];
    let placement = match owner {
        Some(_) => TDF_POSITION_RELATIVE_TO_WINDOW,
        None => TASKDIALOG_FLAGS(0),
    };
    let configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: placement | TDF_ALLOW_DIALOG_CANCELLATION,
        hwndParent: owner.unwrap_or_default(),
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDCANCEL as i32,
        ..Default::default()
    };
    let mut pressed = IDCANCEL as i32;
    let shown =
        unsafe { TaskDialogIndirect(&raw const configuration, Some(&raw mut pressed), None, None) };
    shown.is_ok() && pressed == IDOK as i32
}
