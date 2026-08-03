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

/// Extension without the dot, folded so lists match regardless of how a file spells it.
pub fn lowercase_extension(path: &std::path::Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_lowercase())
}

/// Explorer's natural order over null-terminated UTF-16 names.
pub fn natural_order(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    let result = unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) };
    result.cmp(&0)
}
