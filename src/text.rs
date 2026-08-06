//! Win32 string helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::core::PCWSTR;

/// A null-terminated UTF-16 buffer for Win32 wide-string APIs.
pub fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
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

/// Explorer's natural order over null-terminated UTF-16 names.
pub fn natural_order(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    let result = unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) };
    result.cmp(&0)
}

/// The same order over Rust strings, for callers that hold no wide buffer.
pub fn natural_order_text(a: &str, b: &str) -> std::cmp::Ordering {
    natural_order(&wide(a), &wide(b))
}
