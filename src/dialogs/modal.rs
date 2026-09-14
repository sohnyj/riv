use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, GetWindowLongPtrW, WINDOW_LONG_PTR_INDEX,
};

use crate::dialogs::resource;

/// DWLP_DLGPROC (8) + 8 on x64; windows-rs does not export it.
pub const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

pub type DialogProcedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

/// The executable's own module: the dialog templates and window classes come from it.
pub fn module_handle() -> HMODULE {
    unsafe { GetModuleHandleW(None) }.expect("the module handle of the running module")
}

/// Runs a dialog template from the executable's own resources; the DialogBox result.
pub fn run_modal(
    owner: HWND,
    template: u16,
    procedure: DialogProcedure,
    state_pointer: isize,
) -> isize {
    unsafe {
        DialogBoxParamW(
            Some(module_handle().into()),
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
