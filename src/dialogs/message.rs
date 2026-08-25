//! The one-message dialog the rest of the program reports failures through.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_POSITION_RELATIVE_TO_WINDOW, TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{IDNO, IDYES};
use windows::core::{HSTRING, PCWSTR, w};

use crate::dialogs::modal::IDOK;

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

/// Yes/No question defaulting to No, which the dismissals answer with; true when Yes was pressed.
pub fn confirm_message(owner: Option<HWND>, title: &str, question: &str, detail: &str) -> bool {
    let title = HSTRING::from(title);
    let text = HSTRING::from(format!("{question}\n\n{detail}"));
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: IDYES.0,
            pszButtonText: w!("Yes"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDNO.0,
            pszButtonText: w!("No"),
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
        nDefaultButton: IDNO.0,
        ..Default::default()
    };
    let mut pressed = IDNO.0;
    let shown =
        unsafe { TaskDialogIndirect(&raw const configuration, Some(&raw mut pressed), None, None) };
    shown.is_ok() && pressed == IDYES.0
}
