//! Sending work to the window's message queue, and reading what a message packs.

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

/// Low 16 bits of a packed message parameter.
pub fn low_word(value: usize) -> u32 {
    (value & 0xFFFF) as u32
}

/// High 16 bits of a packed message parameter.
pub fn high_word(value: usize) -> u32 {
    ((value >> 16) & 0xFFFF) as u32
}

/// High 16 bits read as the signed value a wheel delta carries.
pub fn high_word_signed(value: usize) -> i16 {
    high_word(value) as u16 as i16
}

/// The x and y a message packs into one parameter; both halves are signed.
pub fn point_from_packed(value: usize) -> (i32, i32) {
    (
        i32::from(low_word(value) as u16 as i16),
        i32::from(high_word_signed(value)),
    )
}
