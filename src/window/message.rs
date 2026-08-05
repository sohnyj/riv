//! Sending work to the window's message queue.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Posts an owned payload; reclaims it when the message cannot be delivered.
pub fn post_boxed<T>(window: isize, message: u32, payload: Box<T>) {
    let pointer = Box::into_raw(payload);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(window as *mut core::ffi::c_void)),
            message,
            WPARAM(0),
            LPARAM(pointer as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(pointer) });
    }
}
