//! About dialog: large title, version, build info, and a repository link.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DeleteObject, FW_NORMAL,
    HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, EndDialog, GetDlgItem, SendMessageW, SetDlgItemTextW, SetWindowLongPtrW,
    WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_DESTROY, WM_INITDIALOG, WM_NOTIFY, WM_SETFONT,
};
use windows::core::{PCWSTR, w};

use crate::dialogs::resource::{IDC_ABOUT_LINK, IDC_ABOUT_TITLE, IDC_ABOUT_VERSION, IDD_ABOUT};

const IDOK: usize = 1;
const IDCANCEL: usize = 2;
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

const TITLE_POINT_SIZE: i32 = 40;
const VERSION_POINT_SIZE: i32 = 14;

struct AboutState {
    title_font: HFONT,
    version_font: HFONT,
}

pub fn show(window: HWND) {
    let mut state = AboutState {
        title_font: HFONT::default(),
        version_font: HFONT::default(),
    };
    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    unsafe {
        DialogBoxParamW(
            Some(instance.into()),
            PCWSTR(IDD_ABOUT as usize as *const u16),
            Some(window),
            Some(dialog_procedure),
            LPARAM(&raw mut state as isize),
        )
    };
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
            initialize(dialog, unsafe { &mut *(lparam.0 as *mut AboutState) });
            1
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            if command == IDOK || command == IDCANCEL {
                let _ = unsafe { EndDialog(dialog, command as isize) };
                return 1;
            }
            0
        }
        WM_NOTIFY => {
            use windows::Win32::UI::Controls::{NM_CLICK, NM_RETURN, NMLINK};
            let header = unsafe { &*(lparam.0 as *const windows::Win32::UI::Controls::NMHDR) };
            if header.idFrom == IDC_ABOUT_LINK as usize
                && (header.code == NM_CLICK || header.code == NM_RETURN)
            {
                let link = unsafe { &*(lparam.0 as *const NMLINK) };
                open_link(&link.item.szUrl);
                return 1;
            }
            0
        }
        WM_DESTROY => {
            let pointer = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(dialog, DWLP_USER)
            } as *mut AboutState;
            if let Some(state) = unsafe { pointer.as_ref() } {
                for font in [state.title_font, state.version_font] {
                    if !font.is_invalid() {
                        let _ = unsafe { DeleteObject(font.into()) };
                    }
                }
            }
            0
        }
        _ => 0,
    }
}

fn initialize(dialog: HWND, state: &mut AboutState) {
    set_text(
        dialog,
        IDC_ABOUT_VERSION,
        concat!("version ", env!("CARGO_PKG_VERSION")),
    );

    let dpi = unsafe { GetDpiForWindow(dialog) }.max(96) as i32;
    state.title_font = create_dialog_font(TITLE_POINT_SIZE, dpi);
    state.version_font = create_dialog_font(VERSION_POINT_SIZE, dpi);
    apply_font(dialog, IDC_ABOUT_TITLE, state.title_font);
    apply_font(dialog, IDC_ABOUT_VERSION, state.version_font);
    center_link(dialog);
}

/// SysLink is left-aligned; shrink to its ideal size to center it.
fn center_link(dialog: HWND) {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Controls::LM_GETIDEALSIZE;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };
    let Ok(link) = (unsafe { GetDlgItem(Some(dialog), IDC_ABOUT_LINK) }) else {
        return;
    };
    let mut client = windows::Win32::Foundation::RECT::default();
    if unsafe { GetClientRect(dialog, &raw mut client) }.is_err() {
        return;
    }
    let mut ideal = SIZE::default();
    let height = unsafe {
        SendMessageW(
            link,
            LM_GETIDEALSIZE,
            Some(WPARAM((client.right - client.left) as usize)),
            Some(LPARAM(&raw mut ideal as isize)),
        )
    }
    .0 as i32;
    if ideal.cx <= 0 {
        return;
    }
    let mut bounds = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(link, &raw mut bounds) }.is_err() {
        return;
    }
    let mut corner = [windows::Win32::Foundation::POINT {
        x: bounds.left,
        y: bounds.top,
    }];
    unsafe { windows::Win32::Graphics::Gdi::MapWindowPoints(None, Some(dialog), &mut corner) };
    let _ = unsafe {
        SetWindowPos(
            link,
            None,
            (client.right - client.left - ideal.cx) / 2,
            corner[0].y,
            ideal.cx,
            height.max(ideal.cy),
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
    };
}

fn open_link(url_wide: &[u16]) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let length = url_wide
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(url_wide.len());
    if length == 0 {
        return;
    }
    let url: Vec<u16> = url_wide[..length]
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
}

fn set_text(dialog: HWND, control: i32, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe { SetDlgItemTextW(dialog, control, PCWSTR(wide.as_ptr())) };
}

fn apply_font(dialog: HWND, control: i32, font: HFONT) {
    if font.is_invalid() {
        return;
    }
    if let Ok(label) = unsafe { GetDlgItem(Some(dialog), control) } {
        unsafe {
            SendMessageW(
                label,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            )
        };
    }
}

/// Point size is the only variation; weight and style stay untouched.
fn create_dialog_font(point_size: i32, dpi: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -(point_size * dpi / 72),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            Default::default(),
            w!("Lucida Console"),
        )
    }
}
