//! The one-message dialog the rest of the program reports failures through.

use windows::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows::Win32::UI::Controls::{
    PFTASKDIALOGCALLBACK, TASKDIALOG_BUTTON, TASKDIALOG_NOTIFICATIONS, TASKDIALOGCONFIG,
    TDF_ALLOW_DIALOG_CANCELLATION, TDN_CREATED, TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{IDNO, IDOK, IDYES};
use windows::core::{HRESULT, HSTRING, PCWSTR, w};

/// The dismiss label every plain failure dialog passes.
pub const CLOSE_BUTTON: &str = "Close";

/// The confirmation pair; the delete confirmation shares this vocabulary.
pub const YES_BUTTON: PCWSTR = w!("Yes");
pub const NO_BUTTON: PCWSTR = w!("No");

/// TDF_POSITION_RELATIVE_TO_WINDOW puts the dialog at a dialog owner's corner, so riv centers it.
pub fn centering_callback(owner: Option<HWND>) -> PFTASKDIALOGCALLBACK {
    owner.is_some().then_some(center_when_created as _)
}

unsafe extern "system" fn center_when_created(
    dialog: HWND,
    notification: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    _lparam: LPARAM,
    _reference_data: isize,
) -> HRESULT {
    if notification == TDN_CREATED {
        crate::dialogs::placement::center_on_owner(dialog);
    }
    S_OK
}

/// The body every dialog shows: the lead sentence, a blank line, the detail.
pub fn body_text(lead: &str, detail: &str) -> HSTRING {
    // A main instruction would draw the lead larger and in color, so it goes in the content.
    HSTRING::from(format!("{lead}\n\n{detail}"))
}

/// The configuration every dialog starts from; the strings and buttons outlive the call.
pub fn configuration(
    owner: Option<HWND>,
    title: &HSTRING,
    content: &HSTRING,
    buttons: &[TASKDIALOG_BUTTON],
) -> TASKDIALOGCONFIG {
    TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner.unwrap_or_default(),
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        pfCallback: centering_callback(owner),
        ..Default::default()
    }
}

/// One-message task dialog titled after the action; the headline leads the message.
pub fn show_message(owner: Option<HWND>, title: &str, headline: &str, detail: &str, button: &str) {
    let title = HSTRING::from(title);
    let text = body_text(headline, detail);
    // Labeled here, not by the system: the settings dialog writes its own buttons too.
    let button_text = HSTRING::from(button);
    let buttons = [TASKDIALOG_BUTTON {
        nButtonID: IDOK.0,
        pszButtonText: PCWSTR(button_text.as_ptr()),
    }];
    let configuration = configuration(owner, &title, &text, &buttons);
    let _ = unsafe { TaskDialogIndirect(&raw const configuration, None, None, None) };
}

/// Yes/No question defaulting to No, which the dismissals answer with; true when Yes was pressed.
pub fn confirm_message(owner: Option<HWND>, title: &str, question: &str, detail: &str) -> bool {
    let title = HSTRING::from(title);
    let text = body_text(question, detail);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: IDYES.0,
            pszButtonText: YES_BUTTON,
        },
        TASKDIALOG_BUTTON {
            nButtonID: IDNO.0,
            pszButtonText: NO_BUTTON,
        },
    ];
    let configuration = TASKDIALOGCONFIG {
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        nDefaultButton: IDNO.0,
        ..configuration(owner, &title, &text, &buttons)
    };
    let mut pressed = IDNO.0;
    let shown =
        unsafe { TaskDialogIndirect(&raw const configuration, Some(&raw mut pressed), None, None) };
    shown.is_ok() && pressed == IDYES.0
}
