//! Running a dialog template and reaching the state it was given.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, GetWindowLongPtrW, WINDOW_LONG_PTR_INDEX,
};

use crate::dialogs::resource;

pub const IDOK: usize = 1;
pub const IDCANCEL: usize = 2;
/// DWLP_DLGPROC (8) + 8 on x64; windows-rs does not export it.
pub const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

pub type DialogProcedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

/// Runs a dialog template from the executable's own resources; the DialogBox result.
pub fn run_modal(
    owner: HWND,
    template: u16,
    procedure: DialogProcedure,
    state_pointer: isize,
) -> isize {
    let instance =
        unsafe { GetModuleHandleW(None) }.expect("the module handle of the running module");
    unsafe {
        DialogBoxParamW(
            Some(instance.into()),
            resource::template_name(template),
            Some(owner),
            Some(procedure),
            LPARAM(state_pointer),
        )
    }
}

/// Dialog state stored at DWLP_USER by WM_INITDIALOG.
pub fn state_mut<State>(dialog: HWND) -> Option<&'static mut State> {
    let pointer = unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut State;
    // Synchronous notifications and nested modals re-enter for a second &mut; end the borrow first.
    unsafe { pointer.as_mut() }
}
