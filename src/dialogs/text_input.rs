//! Shared single-edit input dialog; both templates live in riv.rc with the rest.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    EndDialog, GetDlgItem, GetDlgItemTextW, SendMessageW, SetDlgItemTextW, SetWindowLongPtrW,
    WM_COMMAND, WM_INITDIALOG,
};
use windows::core::PCWSTR;

use super::resource::IDC_TEXT_INPUT;
use super::{DWLP_USER, IDCANCEL, IDOK};

/// One edit line with OK/Cancel; the template carries the title and the width.
pub struct TextInputRequest<'a> {
    pub template: u16,
    pub initial_text: &'a str,
    /// UTF-16 range to preselect; None leaves the caret at the start.
    pub selection: Option<(usize, usize)>,
}

struct TextInputState {
    initial_text: Vec<u16>,
    selection: Option<(usize, usize)>,
    accepted_text: Option<String>,
}

/// Runs the modal dialog; Some(text as entered) on OK.
pub fn show(window: HWND, request: &TextInputRequest) -> Option<String> {
    let mut state = TextInputState {
        initial_text: request
            .initial_text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect(),
        selection: request.selection,
        accepted_text: None,
    };
    let dialog_result = super::run_modal(
        window,
        request.template,
        dialog_procedure,
        &raw mut state as isize,
    );
    (dialog_result == IDOK as isize)
        .then(|| state.accepted_text.take())
        .flatten()
}

extern "system" fn dialog_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            crate::dialogs::center_on_owner(dialog);
            let state = unsafe { &*(lparam.0 as *const TextInputState) };
            unsafe {
                let _ =
                    SetDlgItemTextW(dialog, IDC_TEXT_INPUT, PCWSTR(state.initial_text.as_ptr()));
                if let Ok(edit) = GetDlgItem(Some(dialog), IDC_TEXT_INPUT) {
                    if let Some((start, end)) = state.selection {
                        SendMessageW(
                            edit,
                            EM_SETSEL,
                            Some(WPARAM(start)),
                            Some(LPARAM(end as isize)),
                        );
                    }
                    let _ = SetFocus(Some(edit));
                }
            }
            0 // FALSE: focus set explicitly
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command {
                IDOK => {
                    if let Some(state) = super::state_mut::<TextInputState>(dialog) {
                        let mut buffer = [0u16; 2048];
                        let length =
                            unsafe { GetDlgItemTextW(dialog, IDC_TEXT_INPUT, &mut buffer) };
                        state.accepted_text =
                            Some(String::from_utf16_lossy(&buffer[..length as usize]));
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                IDCANCEL => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}
