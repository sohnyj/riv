//! Win32 string helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// A null-terminated UTF-16 buffer for Win32 wide-string APIs; OS strings
/// (paths, file names) keep their unpaired surrogates.
pub fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
