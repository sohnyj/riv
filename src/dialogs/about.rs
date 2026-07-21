//! Settings About page: large title, version, build info, and a repository link.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DeleteObject, FW_NORMAL,
    HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDlgItem, SendMessageW, SetDlgItemTextW, WM_SETFONT,
};
use windows::core::{PCWSTR, w};

use crate::dialogs::resource::{
    IDC_ABOUT_BUILD, IDC_ABOUT_LINK, IDC_ABOUT_TITLE, IDC_ABOUT_VERSION,
};

const TITLE_POINT_SIZE: i32 = 40;
const VERSION_POINT_SIZE: i32 = 14;

/// The two fonts the page owns; the Settings dialog frees them when it closes.
#[derive(Default)]
pub struct AboutFonts {
    title: HFONT,
    version: HFONT,
}

impl AboutFonts {
    pub fn destroy(&self) {
        for font in [self.title, self.version] {
            if !font.is_invalid() {
                let _ = unsafe { DeleteObject(font.into()) };
            }
        }
    }
}

/// Fill in the version text and the title/version fonts; returns the fonts to free.
pub fn initialize_page(page: HWND) -> AboutFonts {
    set_text(
        page,
        IDC_ABOUT_VERSION,
        concat!("version ", env!("CARGO_PKG_VERSION")),
    );
    let dpi = unsafe { GetDpiForWindow(page) }.max(96) as i32;
    let fonts = AboutFonts {
        title: create_font(TITLE_POINT_SIZE, dpi),
        version: create_font(VERSION_POINT_SIZE, dpi),
    };
    apply_font(page, IDC_ABOUT_TITLE, fonts.title);
    apply_font(page, IDC_ABOUT_VERSION, fonts.version);
    layout_centered(page);
    fonts
}

/// Open the repository URL carried by the link's notification.
pub fn handle_link(lparam: LPARAM) {
    use windows::Win32::UI::Controls::NMLINK;
    let link = unsafe { &*(lparam.0 as *const NMLINK) };
    open_link(&link.item.szUrl);
}

/// Center the rows as one block in the stretched page; the .rc rows only fix spacing.
fn layout_centered(page: HWND) {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::MapWindowPoints;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };
    let mut client = RECT::default();
    if unsafe { GetClientRect(page, &raw mut client) }.is_err() {
        return;
    }
    let width = client.right - client.left;
    let height = client.bottom - client.top;

    let rows = [
        IDC_ABOUT_TITLE,
        IDC_ABOUT_VERSION,
        IDC_ABOUT_BUILD,
        IDC_ABOUT_LINK,
    ];
    let mut placements = Vec::with_capacity(rows.len());
    for id in rows {
        let Ok(control) = (unsafe { GetDlgItem(Some(page), id) }) else {
            return;
        };
        let mut bounds = RECT::default();
        if unsafe { GetWindowRect(control, &raw mut bounds) }.is_err() {
            return;
        }
        let mut corners = [
            POINT {
                x: bounds.left,
                y: bounds.top,
            },
            POINT {
                x: bounds.right,
                y: bounds.bottom,
            },
        ];
        unsafe { MapWindowPoints(None, Some(page), &mut corners) };
        placements.push((id, control, corners[0].y, corners[1].y - corners[0].y));
    }

    let block_top = placements[0].2;
    let block_bottom = placements.last().map_or(block_top, |row| row.2 + row.3);
    let offset = (height - (block_bottom - block_top)) / 2 - block_top;

    for (id, control, top, row_height) in placements {
        let y = top + offset;
        if id == IDC_ABOUT_LINK {
            place_link(control, width, y, row_height);
        } else {
            let _ = unsafe {
                SetWindowPos(
                    control,
                    None,
                    0,
                    y,
                    width,
                    row_height,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )
            };
        }
    }
}

/// SysLink is left-aligned; shrink to its ideal size and center it horizontally.
fn place_link(link: HWND, width: i32, top: i32, fallback_height: i32) {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Controls::LM_GETIDEALSIZE;
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos};
    let mut ideal = SIZE::default();
    let measured = unsafe {
        SendMessageW(
            link,
            LM_GETIDEALSIZE,
            Some(WPARAM(width as usize)),
            Some(LPARAM(&raw mut ideal as isize)),
        )
    }
    .0 as i32;
    let (x, link_width, link_height) = if ideal.cx > 0 {
        ((width - ideal.cx) / 2, ideal.cx, measured.max(ideal.cy))
    } else {
        (0, width, fallback_height)
    };
    let _ = unsafe {
        SetWindowPos(
            link,
            None,
            x,
            top,
            link_width,
            link_height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
    };
}

fn open_link(url_wide: &[u16]) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let length = url_wide
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(url_wide.len());
    if length == 0 {
        return;
    }
    let url: Vec<u16> = url_wide[..length]
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
}

fn set_text(page: HWND, control: i32, text: &str) {
    let wide = crate::text::wide(text);
    let _ = unsafe { SetDlgItemTextW(page, control, PCWSTR(wide.as_ptr())) };
}

fn apply_font(page: HWND, control: i32, font: HFONT) {
    if font.is_invalid() {
        return;
    }
    if let Ok(label) = unsafe { GetDlgItem(Some(page), control) } {
        unsafe {
            SendMessageW(
                label,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            )
        };
    }
}

/// Point size is the only variation; weight and style stay untouched.
fn create_font(point_size: i32, dpi: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -(point_size * dpi / 72),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            Default::default(),
            w!("Lucida Console"),
        )
    }
}
