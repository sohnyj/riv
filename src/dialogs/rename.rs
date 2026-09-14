//! Rename dialog: the shared text input preselecting the name's stem.

use windows::Win32::Foundation::HWND;

use crate::dialogs::text_input::{self, TextInputRequest};

pub fn show(owner: HWND, current_name: &str) -> Option<String> {
    let stem_length = stem_utf16_length(current_name);
    text_input::show(
        owner,
        &TextInputRequest {
            template: crate::dialogs::resource::IDD_RENAME,
            initial_text: current_name,
            selection: Some((0, stem_length)),
        },
    )
    .filter(|name| !name.trim().is_empty())
}

/// UTF-16 units of the stem, the edit control's unit; a name that is only an extension is all stem.
fn stem_utf16_length(name: &str) -> usize {
    name.rfind('.').filter(|dot| *dot > 0).map_or_else(
        || name.encode_utf16().count(),
        |dot| name[..dot].encode_utf16().count(),
    )
}

#[cfg(test)]
mod stem_tests {
    use super::stem_utf16_length;

    #[test]
    fn the_stem_stops_before_the_last_dot_in_utf16_units() {
        assert_eq!(stem_utf16_length("photo.jpg"), 5);
        assert_eq!(stem_utf16_length("archive.tar.gz"), 11);
        assert_eq!(stem_utf16_length("한글 이미지.png"), 6);
        assert_eq!(stem_utf16_length("😀.png"), 2);
    }

    #[test]
    fn a_dotfile_and_a_bare_name_select_everything() {
        assert_eq!(stem_utf16_length(".hidden"), 7);
        assert_eq!(stem_utf16_length("README"), 6);
    }
}
