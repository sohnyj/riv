//! Wide-string paths, extension text, and natural-order comparison.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::core::{HSTRING, PWSTR};

/// Copies a CoTaskMem wide string out and frees it.
pub fn take_task_memory_string(text: PWSTR) -> String {
    let owned = String::from_utf16_lossy(unsafe { text.as_wide() });
    unsafe { CoTaskMemFree(Some(text.as_ptr().cast())) };
    owned
}

/// A path from UTF-16 units as Windows handed them, unpaired surrogates included.
pub fn path_from_wide(units: &[u16]) -> PathBuf {
    PathBuf::from(OsString::from_wide(units))
}

/// Leaf name as titles and messages spell it; empty when the path ends in a root.
pub fn file_name_text(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Extension without the dot, folded so lists match regardless of how a file spells it.
pub fn lowercase_extension(path: &std::path::Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_lowercase())
}

/// Explorer's natural order over UTF-16 names.
pub fn natural_order(a: &HSTRING, b: &HSTRING) -> std::cmp::Ordering {
    let result = unsafe { StrCmpLogicalW(a, b) };
    result.cmp(&0)
}

/// The same order over Rust strings, for callers that hold no wide buffer.
pub fn natural_order_text(a: &str, b: &str) -> std::cmp::Ordering {
    natural_order(&HSTRING::from(a), &HSTRING::from(b))
}
