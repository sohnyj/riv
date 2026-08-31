//! Shortcut capture dialogs: raw keyboard sequences and click-to-record mouse bindings.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, CreatePen, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetSysColor, GetSysColorBrush, HBRUSH, HDC, HFONT, InvalidateRect, LineTo,
    MoveToEx, PAINTSTRUCT, PS_SOLID, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CallWindowProcW, DLGC_WANTALLKEYS, DefWindowProcW, EndDialog, GWLP_USERDATA,
    GWLP_WNDPROC, GetClientRect, GetDlgItem, GetParent, GetWindowLongPtrW, LB_ADDSTRING,
    LB_DELETESTRING, LB_GETCOUNT, LB_GETCURSEL, LB_GETITEMHEIGHT, LB_GETITEMRECT, LB_GETTEXT,
    LB_GETTEXTLEN, LB_GETTOPINDEX, LB_RESETCONTENT, RegisterClassExW, SendDlgItemMessageW,
    SendMessageW, SetWindowLongPtrW, SetWindowTextW, WM_APP, WM_COMMAND, WM_DRAWITEM,
    WM_ERASEBKGND, WM_GETDLGCODE, WM_INITDIALOG, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT,
    WM_SETFOCUS, WM_SETFONT, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK,
    WM_XBUTTONDOWN, WNDCLASSEXW, WNDPROC,
};
use windows::core::{HSTRING, w};

use crate::bindings::{self, MouseBase, current_modifiers};
use crate::dialogs::resource::{
    IDC_CAPTURE_KEYBOARD_CLEAR, IDC_CAPTURE_KEYBOARD_FIELD, IDC_CAPTURE_KEYBOARD_LIST,
    IDC_CAPTURE_MOUSE_CLEAR, IDC_CAPTURE_MOUSE_FIELD, IDD_CAPTURE_KEYBOARD, IDD_CAPTURE_MOUSE,
};

use crate::dialogs::modal::{DWLP_USER, IDCANCEL, IDOK, state_mut};
use crate::window::message::{high_word, high_word_signed, low_word, point_from_packed};

const WM_RIV_KEYBOARD_CAPTURED: u32 = WM_APP + 0x40;
const WM_RIV_MOUSE_CAPTURED: u32 = WM_APP + 0x41;
const WM_RIV_KEYBOARD_REMOVE: u32 = WM_APP + 0x42;

const REMOVE_ICON_RED: COLORREF = COLORREF(0x001C_2BC4); // BGR of #C42B1C

/// Unbound-field placeholder; the paint handler recovers it by string comparison.
const NO_BINDING_TEXT: &str = "None";

/// WM_RIV_MOUSE_CAPTURED wparam layout: modifiers above this shift, the base index below.
const MOUSE_CAPTURE_MODIFIER_SHIFT: usize = 8;

pub fn capture_keyboard_sequences(
    parent: HWND,
    current: &[String],
    taken: &[(&str, &str)],
) -> Option<Vec<String>> {
    ensure_capture_classes();
    let mut state = KeyboardCaptureState {
        sequences: current.to_vec(),
        taken,
        accepted: false,
    };
    crate::dialogs::modal::run_modal(
        parent,
        IDD_CAPTURE_KEYBOARD,
        keyboard_procedure,
        &raw mut state as isize,
    );
    state.accepted.then_some(state.sequences)
}

pub fn capture_mouse_binding(
    parent: HWND,
    current: Option<&str>,
    taken: &[(&str, &str)],
) -> Option<Vec<String>> {
    ensure_capture_classes();
    let mut state = MouseCaptureState {
        binding: current.map(str::to_string),
        taken,
        accepted: false,
    };
    crate::dialogs::modal::run_modal(
        parent,
        IDD_CAPTURE_MOUSE,
        mouse_procedure,
        &raw mut state as isize,
    );
    state.accepted.then(|| state.binding.into_iter().collect())
}

fn warn_conflict(dialog: HWND, encoding: &str, owner_label: &str) {
    crate::dialogs::message::show_message(
        Some(dialog),
        "Shortcut",
        "Shortcut already used.",
        &format!("\"{encoding}\" is already bound to \"{owner_label}\""),
        "OK",
    );
}

struct KeyboardCaptureState<'a> {
    sequences: Vec<String>,
    taken: &'a [(&'a str, &'a str)],
    accepted: bool,
}

unsafe extern "system" fn keyboard_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            crate::dialogs::geometry::center_on_owner(dialog);
            let state = unsafe { &*(lparam.0 as *const KeyboardCaptureState) };
            for sequence in &state.sequences {
                listbox_add(dialog, sequence);
            }
            if let Ok(listbox) = unsafe { GetDlgItem(Some(dialog), IDC_CAPTURE_KEYBOARD_LIST) } {
                let procedure = keyboard_list_procedure as *const core::ffi::c_void;
                // The original procedure is stored before the swap, so the subclass never reads it unset.
                let original = unsafe { GetWindowLongPtrW(listbox, GWLP_WNDPROC) };
                unsafe { SetWindowLongPtrW(listbox, GWLP_USERDATA, original) };
                unsafe { SetWindowLongPtrW(listbox, GWLP_WNDPROC, procedure as isize) };
            }
            if let Ok(field) = unsafe { GetDlgItem(Some(dialog), IDC_CAPTURE_KEYBOARD_FIELD) } {
                let _ = unsafe { SetFocus(Some(field)) };
            }
            0
        }
        WM_RIV_KEYBOARD_CAPTURED => {
            let modifiers = high_word(wparam.0) as u8;
            let virtual_key = low_word(wparam.0) as u16;
            if let Some(sequence) = bindings::format_keyboard_sequence(modifiers, virtual_key)
                && let Some(state) = state_mut::<KeyboardCaptureState>(dialog)
                && !state.sequences.contains(&sequence)
            {
                // The list is full at the limit, so the oldest is dropped to make room.
                if state.sequences.len() == bindings::MAXIMUM_KEYBOARD_SEQUENCES {
                    state.sequences.remove(0);
                    listbox_remove(dialog, 0);
                }
                listbox_add(dialog, &sequence);
                state.sequences.push(sequence);
            }
            1
        }
        WM_RIV_KEYBOARD_REMOVE => {
            if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                let index = wparam.0;
                if index < state.sequences.len() {
                    state.sequences.remove(index);
                    listbox_remove(dialog, index);
                }
            }
            1
        }
        WM_DRAWITEM => {
            let draw = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
            if draw.CtlID == IDC_CAPTURE_KEYBOARD_LIST as u32 {
                draw_sequence_item(draw);
                return 1;
            }
            0
        }
        WM_COMMAND => {
            let command = low_word(wparam.0) as i32;
            match command {
                IDC_CAPTURE_KEYBOARD_CLEAR => {
                    if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                        state.sequences.clear();
                        listbox_clear(dialog);
                    }
                    1
                }
                command if command == IDOK as i32 => {
                    let conflict = state_mut::<KeyboardCaptureState>(dialog).and_then(|state| {
                        for sequence in &state.sequences {
                            if let Some((encoding, owner)) = state
                                .taken
                                .iter()
                                .find(|(encoding, _)| encoding == sequence)
                            {
                                return Some((encoding.to_string(), owner.to_string()));
                            }
                        }
                        state.accepted = true;
                        None
                    });
                    // The warning runs a modal loop that re-enters this procedure; the borrow ended above.
                    if let Some((encoding, owner)) = conflict {
                        warn_conflict(dialog, &encoding, &owner);
                        return 1;
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                command if command == IDCANCEL as i32 => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

struct MouseCaptureState<'a> {
    binding: Option<String>,
    taken: &'a [(&'a str, &'a str)],
    accepted: bool,
}

unsafe extern "system" fn mouse_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            crate::dialogs::geometry::center_on_owner(dialog);
            let state = unsafe { &*(lparam.0 as *const MouseCaptureState) };
            set_mouse_field_text(dialog, state.binding.as_deref());
            1
        }
        WM_RIV_MOUSE_CAPTURED => {
            let modifiers = (wparam.0 >> MOUSE_CAPTURE_MODIFIER_SHIFT) as u8;
            if let Some(base) = MouseBase::from_index(wparam.0 as u8)
                && let Some(state) = state_mut::<MouseCaptureState>(dialog)
            {
                let encoding = bindings::format_mouse_encoding(modifiers, base);
                set_mouse_field_text(dialog, Some(&encoding));
                state.binding = Some(encoding);
            }
            1
        }
        WM_COMMAND => {
            let command = low_word(wparam.0) as i32;
            match command {
                IDC_CAPTURE_MOUSE_CLEAR => {
                    if let Some(state) = state_mut::<MouseCaptureState>(dialog) {
                        state.binding = None;
                        set_mouse_field_text(dialog, None);
                    }
                    1
                }
                command if command == IDOK as i32 => {
                    let conflict = state_mut::<MouseCaptureState>(dialog).and_then(|state| {
                        if let Some(binding) = &state.binding
                            && let Some((encoding, owner)) =
                                state.taken.iter().find(|(encoding, _)| encoding == binding)
                        {
                            return Some((encoding.to_string(), owner.to_string()));
                        }
                        state.accepted = true;
                        None
                    });
                    // The warning runs a modal loop that re-enters this procedure; the borrow ended above.
                    if let Some((encoding, owner)) = conflict {
                        warn_conflict(dialog, &encoding, &owner);
                        return 1;
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                command if command == IDCANCEL as i32 => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

fn set_mouse_field_text(dialog: HWND, binding: Option<&str>) {
    if let Ok(field) = unsafe { GetDlgItem(Some(dialog), IDC_CAPTURE_MOUSE_FIELD) } {
        let _ =
            unsafe { SetWindowTextW(field, &HSTRING::from(binding.unwrap_or(NO_BINDING_TEXT))) };
        let _ = unsafe { InvalidateRect(Some(field), None, false) };
    }
}

fn remove_icon_bounds(item: &RECT) -> RECT {
    let side = item.bottom - item.top;
    RECT {
        left: item.right - side,
        top: item.top,
        right: item.right,
        bottom: item.bottom,
    }
}

/// Reads a list item into a buffer sized from LB_GETTEXTLEN (LB_GETTEXT has no bound).
fn listbox_item_text(listbox: HWND, item_index: u32) -> Vec<u16> {
    let length = unsafe {
        SendMessageW(
            listbox,
            LB_GETTEXTLEN,
            Some(WPARAM(item_index as usize)),
            None,
        )
    };
    let length = usize::try_from(length.0).unwrap_or(0);
    let mut text = vec![0u16; length + 1]; // + 1 for the NUL
    let copied = unsafe {
        SendMessageW(
            listbox,
            LB_GETTEXT,
            Some(WPARAM(item_index as usize)),
            Some(LPARAM(text.as_mut_ptr() as isize)),
        )
    };
    text.truncate(usize::try_from(copied.0).unwrap_or(0));
    text
}

/// Left-aligned, vertically-centered text one field indent in from `rect`.
fn draw_field_text(device: HDC, rect: RECT, text: &mut [u16], color: COLORREF) {
    unsafe {
        SetBkMode(device, TRANSPARENT);
        SetTextColor(device, color);
        let mut bounds = rect;
        bounds.left += 4;
        DrawTextW(
            device,
            text,
            &raw mut bounds,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_sequence_item(draw: &DRAWITEMSTRUCT) {
    crate::dialogs::paint::draw_buffered(draw.hDC, draw.rcItem, |device| {
        paint_sequence_item(draw, device)
    });
}

fn paint_sequence_item(draw: &DRAWITEMSTRUCT, device: HDC) {
    let selected = draw.itemState.0 & ODS_SELECTED.0 != 0;
    unsafe {
        FillRect(
            device,
            &raw const draw.rcItem,
            GetSysColorBrush(if selected {
                COLOR_HIGHLIGHT
            } else {
                COLOR_WINDOW
            }),
        );
    }
    if draw.itemID == u32::MAX {
        return; // empty list: background only
    }
    let mut text = listbox_item_text(draw.hwndItem, draw.itemID);
    let color = COLORREF(unsafe {
        GetSysColor(if selected {
            COLOR_HIGHLIGHTTEXT
        } else {
            COLOR_WINDOWTEXT
        })
    });
    draw_field_text(device, draw.rcItem, &mut text, color);
    if selected {
        let zone = remove_icon_bounds(&draw.rcItem);
        let side = zone.bottom - zone.top;
        let inset = side / 4;
        let stroke = (side / 10).max(1);
        unsafe {
            let pen = CreatePen(PS_SOLID, stroke, REMOVE_ICON_RED);
            let previous = SelectObject(device, pen.into());
            let _ = MoveToEx(device, zone.left + inset, zone.top + inset, None);
            let _ = LineTo(device, zone.right - inset, zone.bottom - inset);
            let _ = MoveToEx(device, zone.right - inset, zone.top + inset, None);
            let _ = LineTo(device, zone.left + inset, zone.bottom - inset);
            SelectObject(device, previous);
            let _ = DeleteObject(pen.into());
        }
    }
}

fn erase_below_last_item(listbox: HWND, device: HDC) {
    let mut client = RECT::default();
    if unsafe { GetClientRect(listbox, &raw mut client) }.is_err() {
        return;
    }
    let count = unsafe { SendMessageW(listbox, LB_GETCOUNT, None, None) }.0;
    let top = unsafe { SendMessageW(listbox, LB_GETTOPINDEX, None, None) }.0;
    let height = unsafe { SendMessageW(listbox, LB_GETITEMHEIGHT, None, None) }.0;
    client.top = ((count - top) * height) as i32;
    if client.top < client.bottom {
        unsafe { FillRect(device, &raw const client, GetSysColorBrush(COLOR_WINDOW)) };
    }
}

unsafe extern "system" fn keyboard_list_procedure(
    listbox: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original: WNDPROC = unsafe {
        std::mem::transmute(GetWindowLongPtrW(listbox, GWLP_USERDATA) as *const core::ffi::c_void)
    };
    if message == WM_ERASEBKGND {
        // Each row fills its own background, so only the space under the last one is left.
        erase_below_last_item(listbox, HDC(wparam.0 as *mut _));
        return LRESULT(1);
    }
    if message == WM_LBUTTONDOWN {
        let (x, y) = point_from_packed(lparam.0 as usize);
        let selected = unsafe { SendMessageW(listbox, LB_GETCURSEL, None, None) }.0;
        if selected >= 0 {
            let mut item = RECT::default();
            unsafe {
                SendMessageW(
                    listbox,
                    LB_GETITEMRECT,
                    Some(WPARAM(selected as usize)),
                    Some(LPARAM(&raw mut item as isize)),
                )
            };
            let zone = remove_icon_bounds(&item);
            if x >= zone.left && x < zone.right && y >= zone.top && y < zone.bottom {
                if let Ok(dialog) = unsafe { GetParent(listbox) } {
                    unsafe {
                        SendMessageW(
                            dialog,
                            WM_RIV_KEYBOARD_REMOVE,
                            Some(WPARAM(selected as usize)),
                            None,
                        )
                    };
                }
                return LRESULT(0); // consume so the selection does not move
            }
        }
    }
    unsafe { CallWindowProcW(original, listbox, message, wparam, lparam) }
}

fn listbox_add(dialog: HWND, text: &str) {
    let wide = HSTRING::from(text);
    unsafe {
        SendDlgItemMessageW(
            dialog,
            IDC_CAPTURE_KEYBOARD_LIST,
            LB_ADDSTRING,
            WPARAM(0),
            LPARAM(wide.as_ptr() as isize),
        )
    };
}

fn listbox_remove(dialog: HWND, index: usize) {
    unsafe {
        SendDlgItemMessageW(
            dialog,
            IDC_CAPTURE_KEYBOARD_LIST,
            LB_DELETESTRING,
            WPARAM(index),
            LPARAM(0),
        )
    };
}

fn listbox_clear(dialog: HWND) {
    unsafe {
        SendDlgItemMessageW(
            dialog,
            IDC_CAPTURE_KEYBOARD_LIST,
            LB_RESETCONTENT,
            WPARAM(0),
            LPARAM(0),
        )
    };
}

fn ensure_capture_classes() {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        let instance =
            unsafe { GetModuleHandleW(None) }.expect("the module handle of the running module");
        for (class_name, procedure, style) in [
            (
                w!("RivKeyboardCapture"),
                keyboard_field_procedure as unsafe extern "system" fn(_, _, _, _) -> _,
                Default::default(),
            ),
            (w!("RivMouseCapture"), mouse_field_procedure, CS_DBLCLKS),
        ] {
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style,
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                // No class brush: the field fills what it repaints, and a system erase flashes.
                hbrBackground: HBRUSH::default(),
                lpszClassName: class_name,
                ..Default::default()
            };
            let atom = unsafe { RegisterClassExW(&raw const class) };
            assert!(atom != 0, "capture field class registration failed");
        }
    });
}

fn field_font(field: HWND) -> HFONT {
    HFONT(unsafe { GetWindowLongPtrW(field, GWLP_USERDATA) } as *mut _)
}

fn paint_field(field: HWND, text: &str, hint: bool) {
    let mut paint = PAINTSTRUCT::default();
    let target = unsafe { BeginPaint(field, &raw mut paint) };
    let bounds = paint.rcPaint;
    crate::dialogs::paint::draw_buffered(target, bounds, |device| unsafe {
        FillRect(device, &raw const bounds, GetSysColorBrush(COLOR_WINDOW));
        let font = field_font(field);
        if !font.is_invalid() {
            SelectObject(device, font.into());
        }
        let color = COLORREF(GetSysColor(if hint {
            COLOR_GRAYTEXT
        } else {
            COLOR_WINDOWTEXT
        }));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        draw_field_text(device, bounds, &mut wide, color);
    });
    let _ = unsafe { EndPaint(field, &raw const paint) };
}

fn is_modifier_key(virtual_key: u16) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    };
    [
        VK_SHIFT.0,
        VK_CONTROL.0,
        VK_MENU.0,
        VK_LWIN.0,
        VK_RWIN.0,
        VK_LSHIFT.0,
        VK_RSHIFT.0,
        VK_LCONTROL.0,
        VK_RCONTROL.0,
        VK_LMENU.0,
        VK_RMENU.0,
    ]
    .contains(&virtual_key)
}

unsafe extern "system" fn keyboard_field_procedure(
    field: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_GETDLGCODE => LRESULT(DLGC_WANTALLKEYS as isize),
        WM_SETFONT => {
            unsafe { SetWindowLongPtrW(field, GWLP_USERDATA, wparam.0 as isize) };
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let virtual_key = wparam.0 as u16;
            if is_modifier_key(virtual_key) {
                let _ = unsafe { InvalidateRect(Some(field), None, false) };
            } else {
                let packed = ((current_modifiers() as usize) << 16) | virtual_key as usize;
                if let Ok(parent) = unsafe { GetParent(field) } {
                    unsafe {
                        SendMessageW(parent, WM_RIV_KEYBOARD_CAPTURED, Some(WPARAM(packed)), None)
                    };
                }
            }
            LRESULT(0)
        }
        // Translation runs in the loop, so an Alt combination reaches here after being captured.
        WM_SYSCHAR => LRESULT(0),
        WM_KEYUP | WM_SYSKEYUP | WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = unsafe { InvalidateRect(Some(field), None, false) };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(field)) };
            LRESULT(0)
        }
        WM_PAINT => {
            let focused =
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } == field;
            if focused {
                let prefix = bindings::modifier_prefix(current_modifiers());
                if prefix.is_empty() {
                    paint_field(field, "Press a key combination...", true);
                } else {
                    paint_field(field, &format!("{prefix}..."), false);
                }
            } else {
                paint_field(field, "Click here to capture", true);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(field, message, wparam, lparam) },
    }
}

unsafe extern "system" fn mouse_field_procedure(
    field: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    fn notify(field: HWND, base: MouseBase) -> LRESULT {
        let packed = ((current_modifiers() as usize) << MOUSE_CAPTURE_MODIFIER_SHIFT)
            | base.index() as usize;
        if let Ok(parent) = unsafe { GetParent(field) } {
            unsafe { SendMessageW(parent, WM_RIV_MOUSE_CAPTURED, Some(WPARAM(packed)), None) };
        }
        LRESULT(0)
    }
    match message {
        WM_SETFONT => {
            unsafe { SetWindowLongPtrW(field, GWLP_USERDATA, wparam.0 as isize) };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(field)) };
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => notify(field, MouseBase::DoubleClick),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => notify(field, MouseBase::WheelButton),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => notify(
            field,
            MouseBase::from_xbutton_flags(high_word(wparam.0) as u16),
        ),
        WM_MOUSEWHEEL => notify(
            field,
            MouseBase::from_wheel_delta(high_word_signed(wparam.0)),
        ),
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = unsafe { InvalidateRect(Some(field), None, false) };
            LRESULT(0)
        }
        WM_PAINT => {
            let mut text = [0u16; 128];
            let length = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(field, &mut text)
            };
            let current = String::from_utf16_lossy(&text[..length as usize]);
            let hint = current.is_empty() || current == NO_BINDING_TEXT;
            paint_field(
                field,
                if current.is_empty() {
                    NO_BINDING_TEXT
                } else {
                    &current
                },
                hint,
            );
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(field, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod listbox_item_text_tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::core::PCWSTR;

    const WS_POPUP: u32 = 0x8000_0000;
    const LBS_HASSTRINGS: u32 = 0x0040;

    #[test]
    #[ignore = "creates a LISTBOX; runs under wine"]
    fn an_item_longer_than_the_old_fixed_buffer_reads_back_fully() {
        // > the old [u16; 128] buffer that overflowed
        let item = HSTRING::from("A".repeat(300));
        let listbox = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("LISTBOX"),
                PCWSTR::null(),
                WINDOW_STYLE(WS_POPUP | LBS_HASSTRINGS),
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create listbox");
        unsafe {
            SendMessageW(
                listbox,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(item.as_ptr() as isize)),
            )
        };
        let text = listbox_item_text(listbox, 0);
        let _ = unsafe { DestroyWindow(listbox) };
        assert_eq!(text.len(), 300);
    }
}
