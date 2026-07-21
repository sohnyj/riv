//! Win32 string helpers.

/// A null-terminated UTF-16 buffer for Win32 wide-string APIs.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
