//! Sending work to the window's message queue, and reading what a message packs.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SendMessageW};

/// Payload pointers handed to the window, each with the message and type it went out as.
static SENT_PAYLOADS: LazyLock<Mutex<HashMap<usize, (u32, TypeId)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn record_sent<T: 'static>(pointer: usize, message: u32) {
    lock_sent().insert(pointer, (message, TypeId::of::<T>()));
}

fn remove_sent(pointer: usize) {
    lock_sent().remove(&pointer);
}

/// Whether this pointer went out with this message carrying a `T`.
fn was_sent<T: 'static>(pointer: usize, message: u32) -> bool {
    lock_sent().get(&pointer) == Some(&(message, TypeId::of::<T>()))
}

fn lock_sent() -> std::sync::MutexGuard<'static, HashMap<usize, (u32, TypeId)>> {
    SENT_PAYLOADS.lock().expect("payload registry poisoned")
}

/// Posts an owned payload; reclaims it when the message cannot be delivered.
pub fn post_boxed<T: 'static>(window: isize, message: u32, payload: Box<T>) {
    let pointer = Box::into_raw(payload);
    record_sent::<T>(pointer as usize, message);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(window as *mut core::ffi::c_void)),
            message,
            WPARAM(0),
            LPARAM(pointer as isize),
        )
    };
    if posted.is_err() {
        remove_sent(pointer as usize);
        drop(unsafe { Box::from_raw(pointer) });
    }
}

/// The posted payload, or None when that pointer never went out with this message as a `T`.
pub unsafe fn take_boxed<T: 'static>(message: u32, lparam: LPARAM) -> Option<Box<T>> {
    let pointer = lparam.0 as usize;
    {
        // One locked check-and-remove, so the pointer is reclaimed exactly once.
        let mut payloads = lock_sent();
        if payloads.get(&pointer) != Some(&(message, TypeId::of::<T>())) {
            return None;
        }
        payloads.remove(&pointer);
    }
    Some(unsafe { Box::from_raw(pointer as *mut T) })
}

/// Sends a borrowed payload, readable by the window procedure for the length of the call.
pub fn send_borrowed<T: 'static>(window: HWND, message: u32, payload: &T) -> LRESULT {
    let pointer = std::ptr::from_ref(payload) as usize;
    record_sent::<T>(pointer, message);
    let message_result =
        unsafe { SendMessageW(window, message, None, Some(LPARAM(pointer as isize))) };
    remove_sent(pointer);
    message_result
}

/// The sent payload, or None when that pointer never went out with this message as a `T`.
pub unsafe fn borrowed_payload<'payload, T: 'static>(
    message: u32,
    lparam: LPARAM,
) -> Option<&'payload T> {
    let pointer = lparam.0 as usize;
    // The sender's stack owns the value only during the send; the reference must not outlive that call.
    was_sent::<T>(pointer, message).then(|| unsafe { &*(pointer as *const T) })
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

#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn a_pointer_that_never_went_out_is_refused() {
        // What a stray PostMessage from another process delivers; nothing may be read from it.
        let stray = LPARAM(0x1234_5678);
        assert!(unsafe { take_boxed::<u64>(0x8001, stray) }.is_none());
        assert!(unsafe { borrowed_payload::<u64>(0x8001, stray) }.is_none());
    }

    #[test]
    fn a_payload_belongs_to_the_message_it_went_out_with() {
        let pointer = Box::into_raw(Box::new(7u64));
        record_sent::<u64>(pointer as usize, 0x8001);
        let lparam = LPARAM(pointer as isize);
        assert!(unsafe { take_boxed::<u64>(0x8002, lparam) }.is_none());
        let taken = unsafe { take_boxed::<u64>(0x8001, lparam) };
        assert_eq!(taken.as_deref(), Some(&7));
    }

    #[test]
    fn a_payload_taken_as_the_wrong_type_is_refused() {
        let pointer = Box::into_raw(Box::new(7u64));
        record_sent::<u64>(pointer as usize, 0x8003);
        let lparam = LPARAM(pointer as isize);
        assert!(unsafe { take_boxed::<u32>(0x8003, lparam) }.is_none());
        // The refusal leaves the registration, so the right type still reclaims the payload.
        assert_eq!(
            unsafe { take_boxed::<u64>(0x8003, lparam) }.as_deref(),
            Some(&7)
        );
    }
}

#[cfg(test)]
mod wm_app_number_tests {
    /// New files declaring WM_APP messages must be listed here; the scan cannot discover them.
    const SOURCES: [(&str, &str); 6] = [
        ("main.rs", include_str!("../main.rs")),
        ("image/core.rs", include_str!("../image/core.rs")),
        ("shell/drag_drop.rs", include_str!("../shell/drag_drop.rs")),
        ("shell/open_with.rs", include_str!("../shell/open_with.rs")),
        ("dialogs/options.rs", include_str!("../dialogs/options.rs")),
        (
            "dialogs/shortcut_capture.rs",
            include_str!("../dialogs/shortcut_capture.rs"),
        ),
    ];

    fn declared_numbers() -> Vec<(String, String, u32)> {
        let mut declarations = Vec::new();
        for (file, source) in SOURCES {
            for line in source.lines() {
                let Some((head, tail)) = line.split_once("= WM_APP + ") else {
                    continue;
                };
                let name = head
                    .split(':')
                    .next()
                    .and_then(|declared| declared.split_whitespace().last())
                    .expect("a constant name precedes the value")
                    .to_string();
                let value = tail.trim().trim_end_matches(';');
                let number = match value.strip_prefix("0x") {
                    Some(hex) => u32::from_str_radix(hex, 16),
                    None => value.parse(),
                }
                .expect("a WM_APP offset is a number literal");
                declarations.push((file.to_string(), name, number));
            }
        }
        declarations
    }

    #[test]
    fn every_message_number_is_distinct() {
        let declarations = declared_numbers();
        for (file, _) in SOURCES {
            assert!(
                declarations.iter().any(|(source, _, _)| source == file),
                "the scan found nothing in {file}; drop it from SOURCES or fix the parse"
            );
        }
        for (index, (file, name, number)) in declarations.iter().enumerate() {
            for (other_file, other_name, other_number) in &declarations[index + 1..] {
                assert!(
                    number != other_number,
                    "{name} ({file}) and {other_name} ({other_file}) share WM_APP + {number}"
                );
            }
        }
    }
}
