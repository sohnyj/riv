//! Dialog resource IDs; keep in sync with res/resource.h.

use windows::core::PCWSTR;

/// A numeric resource name is the identifier itself, not a pointer to one.
pub fn template_name(identifier: u16) -> PCWSTR {
    PCWSTR(identifier as usize as *const u16)
}

/// Authored size of the seven settings pages, as riv.rc writes it.
pub const PAGE_TEMPLATE_WIDTH_DIALOG_UNITS: i32 = 292;
pub const PAGE_TEMPLATE_HEIGHT_DIALOG_UNITS: i32 = 194;

pub const IDD_OPTIONS: u16 = 100;
pub const IDD_PAGE_WINDOW: u16 = 110;
pub const IDD_PAGE_IMAGE: u16 = 120;
pub const IDD_PAGE_MISCELLANEOUS: u16 = 130;
pub const IDD_PAGE_SHORTCUTS: u16 = 140;
pub const IDD_PAGE_ASSOCIATION: u16 = 150;
pub const IDD_PAGE_START_MENU: u16 = 190;
pub const IDD_PAGE_ABOUT: u16 = 200;
pub const IDD_CAPTURE_KEYBOARD: u16 = 160;
pub const IDD_CAPTURE_MOUSE: u16 = 170;
pub const IDD_RENAME: u16 = 180;
pub const IDD_OPEN_URL: u16 = 185;

/// The single edit field both text input dialogs carry.
pub const IDC_TEXT_INPUT: i32 = 100;

pub const IDC_OPTIONS_TAB: i32 = 1001;
pub const IDC_APPLY: i32 = 1002;
pub const IDC_RESTORE_DEFAULTS: i32 = 1003;

pub const IDC_WINDOW_BACKGROUND_COLOR_ENABLED: i32 = 1101;
pub const IDC_WINDOW_BACKGROUND_COLOR_BUTTON: i32 = 1102;
pub const IDC_WINDOW_TITLE_BAR_TEXT: i32 = 1103;
pub const IDC_WINDOW_REMEMBER_WINDOW_PLACEMENT: i32 = 1107;
pub const IDC_WINDOW_CONTROL_DRAG: i32 = 1108;
pub const IDC_WINDOW_HIDE_CURSOR_FULLSCREEN: i32 = 1109;

pub const IDC_IMAGE_SCALING: i32 = 1201;
pub const IDC_IMAGE_ZOOM_STEP_EDIT: i32 = 1202;
pub const IDC_IMAGE_ZOOM_STEP_SPIN: i32 = 1203;
pub const IDC_IMAGE_CURSOR_ZOOM: i32 = 1204;
pub const IDC_IMAGE_FRACTIONAL_WHEEL_ZOOM: i32 = 1205;
pub const IDC_IMAGE_DITHER: i32 = 1206;
pub const IDC_IMAGE_FIT_MODE: i32 = 1207;
pub const IDC_IMAGE_PRELOADING: i32 = 1208;

pub const IDC_MISCELLANEOUS_SORT: i32 = 1301;
pub const IDC_MISCELLANEOUS_ASCENDING: i32 = 1302;
pub const IDC_MISCELLANEOUS_DESCENDING: i32 = 1303;
pub const IDC_MISCELLANEOUS_LOOP_WITHIN_FOLDER: i32 = 1305;
pub const IDC_MISCELLANEOUS_SLIDESHOW_DIRECTION: i32 = 1306;
pub const IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_EDIT: i32 = 1307;
pub const IDC_MISCELLANEOUS_AFTER_DELETION: i32 = 1308;
pub const IDC_MISCELLANEOUS_ASK_DELETE: i32 = 1309;
pub const IDC_MISCELLANEOUS_DETECT_FORMAT_BY_CONTENT: i32 = 1310;
pub const IDC_MISCELLANEOUS_REMEMBER_RECENTS: i32 = 1311;
pub const IDC_MISCELLANEOUS_SKIP_HIDDEN: i32 = 1312;
pub const IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_SPIN: i32 = 1313;

pub const IDC_SHORTCUTS_LIST: i32 = 1401;
pub const IDC_SHORTCUTS_RESET: i32 = 1402;
pub const IDC_SHORTCUTS_CLEAR_ALL: i32 = 1403;

pub const IDC_ASSOCIATION_TREE: i32 = 1501;
pub const IDC_ASSOCIATION_SELECT_ALL: i32 = 1502;
pub const IDC_ASSOCIATION_SELECT_NONE: i32 = 1503;
pub const IDC_START_MENU_SHORTCUT: i32 = 1901;

pub const IDC_CAPTURE_KEYBOARD_FIELD: i32 = 1601;
pub const IDC_CAPTURE_KEYBOARD_LIST: i32 = 1602;
pub const IDC_CAPTURE_KEYBOARD_CLEAR: i32 = 1604;
pub const IDC_CAPTURE_MOUSE_FIELD: i32 = 1701;
pub const IDC_CAPTURE_MOUSE_CLEAR: i32 = 1702;

pub const IDC_ABOUT_TITLE: i32 = 1801;
pub const IDC_ABOUT_VERSION: i32 = 1802;
pub const IDC_ABOUT_BUILD: i32 = 1803;
pub const IDC_ABOUT_LINK: i32 = 1804;

#[cfg(test)]
mod header_mirror_tests {
    use windows::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDOK};

    /// A number only one file has costs a compile error; two different numbers cost a control.
    #[test]
    fn every_identifier_carries_the_same_number_in_both_files() {
        // Windows owns these two; the header spells them out because llvm-rc has no windows.h.
        let os_owned = [
            ("IDOK", IDOK.0.to_string()),
            ("IDCANCEL", IDCANCEL.0.to_string()),
        ];
        let header: Vec<(&str, &str)> = include_str!("../../res/resource.h")
            .lines()
            .filter_map(|line| line.strip_prefix("#define ")?.split_once(' '))
            .filter(|(name, _)| name.starts_with("ID") || name.starts_with("PAGE_TEMPLATE_"))
            .collect();
        let declared: Vec<(&str, &str)> = include_str!("resource.rs")
            .lines()
            .filter_map(|line| {
                let (name, typed) = line.strip_prefix("pub const ")?.split_once(": ")?;
                Some((name, typed.split_once(" = ")?.1.strip_suffix(';')?))
            })
            .collect();
        let header_only: Vec<_> = header
            .iter()
            .filter(|entry| !declared.contains(entry))
            .filter(|(name, value)| !os_owned.iter().any(|(os, v)| os == name && v == value))
            .collect();
        let rust_only: Vec<_> = declared
            .iter()
            .filter(|entry| !header.contains(entry))
            .collect();
        assert!(
            header_only.is_empty() && rust_only.is_empty(),
            "resource.h only: {header_only:?}, resource.rs only: {rust_only:?}"
        );
    }
}
