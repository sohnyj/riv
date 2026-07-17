//! Open URL dialog built from an in-memory DLGTEMPLATE.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DLGTEMPLATE, DialogBoxIndirectParamW, EndDialog, GetDlgItem, GetDlgItemTextW,
    WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG,
};

pub(crate) const EDIT_IDENTIFIER: i32 = 100;
const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

pub fn show(window: HWND) -> Option<String> {
    let mut accepted_url: Option<String> = None;
    let template = build_template();
    let confirmed = unsafe {
        DialogBoxIndirectParamW(
            None,
            template.as_ptr().cast::<DLGTEMPLATE>(),
            Some(window),
            Some(dialog_procedure),
            LPARAM(&raw mut accepted_url as isize),
        )
    };
    (confirmed == IDOK as isize)
        .then(|| accepted_url.take())
        .flatten()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
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
            if let Ok(edit) = unsafe { GetDlgItem(Some(dialog), EDIT_IDENTIFIER) } {
                let _ = unsafe { SetFocus(Some(edit)) };
            }
            0 // FALSE: focus set explicitly
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command {
                IDOK => {
                    let pointer =
                        unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut Option<String>;
                    if let Some(accepted_url) = unsafe { pointer.as_mut() } {
                        let mut buffer = [0u16; 2048];
                        let length =
                            unsafe { GetDlgItemTextW(dialog, EDIT_IDENTIFIER, &mut buffer) };
                        *accepted_url = Some(String::from_utf16_lossy(&buffer[..length as usize]));
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

fn build_template() -> Vec<u16> {
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
    template.extend_from_slice(&[0, 0, 300, 54]); // x, y, cx, cy in dialog units
    template.push(0); // no menu
    template.push(0); // default class
    template.extend("Open URL".encode_utf16().chain(std::iter::once(0))); // title
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
        [7, 7, 286, 13],
        EDIT_IDENTIFIER as u16,
        0x0081,
        "",
    );
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON,
        [186, 30, 50, 14],
        IDOK as u16,
        0x0080,
        "OK",
    );
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP,
        [243, 30, 50, 14],
        IDCANCEL as u16,
        0x0080,
        "Cancel",
    );
    template
}

/// Needs an interactive session (creates a real dialog window).
#[cfg(test)]
mod dialog_tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::SetDlgItemTextW;
    use windows::core::w;

    #[test]
    #[ignore = "creates a real dialog window"]
    fn dialog_round_trips_the_entered_url() {
        let driver = std::thread::spawn(|| {
            use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
            for _ in 0..100 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let Ok(dialog) = (unsafe { FindWindowW(None, w!("Open URL")) }) else {
                    continue;
                };
                unsafe {
                    SetDlgItemTextW(dialog, EDIT_IDENTIFIER, w!("  http://127.0.0.1/test.png  "))
                        .expect("set edit text");
                    PostMessageW(Some(dialog), WM_COMMAND, WPARAM(IDOK), LPARAM(0))
                        .expect("post IDOK");
                }
                return;
            }
            panic!("the dialog never appeared");
        });
        let url = show(HWND::default());
        driver.join().expect("driver thread");
        assert_eq!(url.as_deref(), Some("http://127.0.0.1/test.png")); // trimmed
    }
}
