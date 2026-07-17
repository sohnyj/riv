//! Shared single-edit input dialog built from an in-memory DLGTEMPLATE.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DLGTEMPLATE, DialogBoxIndirectParamW, EndDialog, GetDlgItem, GetDlgItemTextW, SendMessageW,
    SetDlgItemTextW, WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG,
};

pub(crate) const EDIT_IDENTIFIER: i32 = 100;
pub(crate) const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

/// One edit line with OK/Cancel; width in dialog units.
pub struct TextInputRequest<'a> {
    pub title: &'a str,
    pub width: i16,
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
    let template = build_template(request.title, request.width);
    let confirmed = unsafe {
        DialogBoxIndirectParamW(
            None,
            template.as_ptr().cast::<DLGTEMPLATE>(),
            Some(window),
            Some(dialog_procedure),
            LPARAM(&raw mut state as isize),
        )
    };
    (confirmed == IDOK as isize)
        .then(|| state.accepted_text.take())
        .flatten()
}

extern "system" fn dialog_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            crate::dialogs::center_on_owner(dialog);
            let state = unsafe { &*(lparam.0 as *const TextInputState) };
            unsafe {
                let _ = SetDlgItemTextW(
                    dialog,
                    EDIT_IDENTIFIER,
                    windows::core::PCWSTR(state.initial_text.as_ptr()),
                );
                if let Ok(edit) = GetDlgItem(Some(dialog), EDIT_IDENTIFIER) {
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
                    let pointer =
                        unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut TextInputState;
                    if let Some(state) = unsafe { pointer.as_mut() } {
                        let mut buffer = [0u16; 2048];
                        let length =
                            unsafe { GetDlgItemTextW(dialog, EDIT_IDENTIFIER, &mut buffer) };
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

fn build_template(title: &str, width: i16) -> Vec<u16> {
    const DS_SETFONT: u32 = 0x40;
    const DS_MODALFRAME: u32 = 0x80;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_SYSMENU: u32 = 0x0008_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_TABSTOP: u32 = 0x0001_0000;
    const WS_BORDER: u32 = 0x0080_0000;
    const ES_AUTOHSCROLL: u32 = 0x80;
    const BS_DEFPUSHBUTTON: u32 = 0x1;

    let mut template: Vec<u16> = Vec::new();
    let push_u32 = |buffer: &mut Vec<u16>, value: u32| {
        buffer.push((value & 0xFFFF) as u16);
        buffer.push((value >> 16) as u16);
    };

    push_u32(
        &mut template,
        DS_SETFONT | DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
    );
    push_u32(&mut template, 0); // dwExtendedStyle
    template.push(3); // cdit
    template.extend_from_slice(&[0, 0, width as u16, 54]); // x, y, cx, cy in dialog units
    template.push(0); // no menu
    template.push(0); // default class
    template.extend(title.encode_utf16().chain(std::iter::once(0)));
    template.push(9); // FONT 9pt
    template.extend("Segoe UI".encode_utf16().chain(std::iter::once(0)));

    let push_item = |buffer: &mut Vec<u16>,
                     style: u32,
                     bounds: [i16; 4],
                     identifier: u16,
                     class_atom: u16,
                     text: &str| {
        if !buffer.len().is_multiple_of(2) {
            buffer.push(0);
        }
        let push_u32_inner = |buffer: &mut Vec<u16>, value: u32| {
            buffer.push((value & 0xFFFF) as u16);
            buffer.push((value >> 16) as u16);
        };
        push_u32_inner(buffer, style);
        push_u32_inner(buffer, 0); // exstyle
        buffer.extend(bounds.iter().map(|value| *value as u16));
        buffer.push(identifier);
        buffer.extend_from_slice(&[0xFFFF, class_atom]);
        buffer.extend(text.encode_utf16().chain(std::iter::once(0)));
        buffer.push(0); // no creation data
    };

    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
        [7, 7, width - 14, 13],
        EDIT_IDENTIFIER as u16,
        0x0081,
        "",
    );
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON,
        [width - 114, 30, 50, 14],
        IDOK as u16,
        0x0080,
        "OK",
    );
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP,
        [width - 57, 30, 50, 14],
        IDCANCEL as u16,
        0x0080,
        "Cancel",
    );
    template
}
