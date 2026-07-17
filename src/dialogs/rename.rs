//! Rename dialog: the shared text input preselecting the name's stem.

use windows::Win32::Foundation::HWND;

use super::text_input::{self, TextInputRequest};

pub fn show(window: HWND, current_name: &str) -> Option<String> {
    let stem_length = current_name.rfind('.').filter(|dot| *dot > 0).map_or_else(
        || current_name.encode_utf16().count(),
        |dot| current_name[..dot].encode_utf16().count(),
    );
    text_input::show(
        window,
        &TextInputRequest {
            title: "Rename",
            width: 220,
            initial_text: current_name,
            selection: Some((0, stem_length)),
        },
    )
    .filter(|name| !name.trim().is_empty())
}
