//! Clipboard text lookup for the paste-to-open-URL key.

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Trimmed clipboard text; None when the clipboard holds none.
pub fn read_text(window: HWND) -> Option<String> {
    if unsafe { OpenClipboard(Some(window)) }.is_err() {
        return None;
    }
    let text = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }
        .ok()
        .and_then(read_wide);
    let _ = unsafe { CloseClipboard() };
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn read_wide(handle: HANDLE) -> Option<String> {
    let global = HGLOBAL(handle.0);
    let pointer = unsafe { GlobalLock(global) }.cast::<u16>();
    if pointer.is_null() {
        return None;
    }
    // The allocation size bounds the scan against a missing terminator.
    let capacity = unsafe { GlobalSize(global) } / size_of::<u16>();
    let mut length = 0usize;
    while length < capacity && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) });
    let _ = unsafe { GlobalUnlock(global) };
    Some(text)
}

/// Needs a session clipboard; overwrites its text content.
#[cfg(test)]
mod round_trip_tests {
    use super::*;
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};

    fn write_text(text: &str) {
        let wide = crate::text::wide(text);
        unsafe {
            OpenClipboard(None).expect("open clipboard");
            EmptyClipboard().expect("empty clipboard");
            let global = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).expect("clipboard alloc");
            let pointer = GlobalLock(global).cast::<u16>();
            std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
            let _ = GlobalUnlock(global);
            SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0)))
                .expect("set clipboard");
            CloseClipboard().expect("close clipboard");
        }
    }

    #[test]
    #[ignore = "overwrites the session clipboard"]
    fn clipboard_text_round_trips_trimmed() {
        write_text("  https://a.com/한글 경로/b.png \r\n");
        assert_eq!(
            read_text(HWND::default()).as_deref(),
            Some("https://a.com/한글 경로/b.png")
        );
        write_text("   \r\n");
        assert_eq!(read_text(HWND::default()), None); // whitespace-only reads as none
    }
}
