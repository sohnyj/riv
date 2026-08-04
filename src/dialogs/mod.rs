pub mod about;
pub mod open_url;
pub mod options;
pub mod rename;
pub mod resource;
pub mod shortcut_capture;
pub mod text_input;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MapWindowPoints, MonitorFromRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_FLAGS, TASKDIALOGCONFIG, TDF_POSITION_RELATIVE_TO_WINDOW,
    TaskDialogIndirect,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, GetWindowLongPtrW, GetWindowRect, WINDOW_LONG_PTR_INDEX,
};
use windows::core::PCWSTR;

pub const IDOK: usize = 1;
pub const IDCANCEL: usize = 2;
/// DWLP_DLGPROC (8) + 8 on x64; windows-rs does not export it.
pub const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

pub type DialogProcedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

/// Runs a dialog template from the executable's own resources; the DialogBox result.
pub fn run_modal(
    parent: HWND,
    template: u16,
    procedure: DialogProcedure,
    state_pointer: isize,
) -> isize {
    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    unsafe {
        DialogBoxParamW(
            Some(instance.into()),
            resource::template_name(template),
            Some(parent),
            Some(procedure),
            LPARAM(state_pointer),
        )
    }
}

/// Dialog state stored at DWLP_USER by WM_INITDIALOG.
pub fn state_mut<State>(dialog: HWND) -> Option<&'static mut State> {
    let pointer = unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut State;
    unsafe { pointer.as_mut() }
}

/// Holds a placement inside the work area of the monitor it lands on.
pub fn clamp_to_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let target = RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    };
    let monitor = unsafe { MonitorFromRect(&raw const target, MONITOR_DEFAULTTONEAREST) };
    let mut information = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &raw mut information) }.as_bool() {
        return (x, y);
    }
    let work_area = information.rcWork;
    // Clamped low last, so a dialog taller than the work area keeps its top left corner.
    (
        x.min(work_area.right - width).max(work_area.left),
        y.min(work_area.bottom - height).max(work_area.top),
    )
}

/// Center a dialog within its owner window.
pub fn center_on_owner(dialog: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetParent, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    let Ok(owner) = (unsafe { GetParent(dialog) }) else {
        return;
    };
    let mut owner_bounds = RECT::default();
    let mut dialog_bounds = RECT::default();
    if unsafe { GetWindowRect(owner, &raw mut owner_bounds) }.is_err()
        || unsafe { GetWindowRect(dialog, &raw mut dialog_bounds) }.is_err()
    {
        return;
    }
    let width = dialog_bounds.right - dialog_bounds.left;
    let height = dialog_bounds.bottom - dialog_bounds.top;
    let x = owner_bounds.left + (owner_bounds.right - owner_bounds.left - width) / 2;
    let y = owner_bounds.top + (owner_bounds.bottom - owner_bounds.top - height) / 2;
    // An owner at a screen edge centers the dialog past it.
    let (x, y) = clamp_to_work_area(x, y, width, height);
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

/// Bounds of a child control in its parent's client coordinates.
pub fn control_bounds(parent: HWND, control: HWND) -> Option<RECT> {
    let mut bounds = RECT::default();
    if unsafe { GetWindowRect(control, &raw mut bounds) }.is_err() {
        return None;
    }
    let mut corners = [
        POINT {
            x: bounds.left,
            y: bounds.top,
        },
        POINT {
            x: bounds.right,
            y: bounds.bottom,
        },
    ];
    unsafe { MapWindowPoints(None, Some(parent), &mut corners) };
    Some(RECT {
        left: corners[0].x,
        top: corners[0].y,
        right: corners[1].x,
        bottom: corners[1].y,
    })
}

/// One-message task dialog titled after the action; the headline leads the message.
pub fn show_message(owner: Option<HWND>, title: &str, headline: &str, detail: &str, button: &str) {
    let title = crate::text::wide(title);
    // A main instruction would draw the headline larger and in color.
    let text = crate::text::wide(format!("{headline}\n\n{detail}"));
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
