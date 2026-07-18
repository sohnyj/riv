//! Shortcut capture dialogs: raw key sequences and click-to-record mouse bindings.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, CreatePen, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetSysColor, GetSysColorBrush, HFONT, InvalidateRect, LineTo, MoveToEx,
    PAINTSTRUCT, PS_SOLID, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED, TASKDIALOGCONFIG};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CallWindowProcW, DLGC_WANTALLKEYS, DefWindowProcW, DialogBoxParamW, EndDialog,
    GWLP_USERDATA, GWLP_WNDPROC, GetDlgItem, GetParent, GetWindowLongPtrW, RegisterClassExW,
    SendMessageW, SetWindowLongPtrW, SetWindowTextW, WINDOW_LONG_PTR_INDEX, WM_APP, WM_COMMAND,
    WM_DRAWITEM, WM_GETDLGCODE, WM_INITDIALOG, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT,
    WM_SETFOCUS, WM_SETFONT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN,
    WNDCLASSEXW, WNDPROC,
};
use windows::core::{PCWSTR, w};

use crate::bindings::{
    self, MODIFIER_ALT, MODIFIER_CONTROL, MODIFIER_META, MODIFIER_SHIFT, MouseBase,
};
use crate::dialogs::resource::{
    IDC_CAPTURE_KEY_CLEAR, IDC_CAPTURE_KEY_FIELD, IDC_CAPTURE_KEY_LIST, IDC_CAPTURE_MOUSE_CLEAR,
    IDC_CAPTURE_MOUSE_FIELD, IDD_CAPTURE_KEYBOARD, IDD_CAPTURE_MOUSE,
};

use super::{DWLP_USER, IDCANCEL, IDOK, state_mut};

const WM_RIV_KEY_CAPTURED: u32 = WM_APP + 0x40;
const WM_RIV_MOUSE_CAPTURED: u32 = WM_APP + 0x41;
const WM_RIV_KEY_REMOVE: u32 = WM_APP + 0x42;

const REMOVE_ICON_RED: COLORREF = COLORREF(0x001C_2BC4); // BGR of #C42B1C

pub type TakenBindings = Vec<(String, &'static str)>;

pub fn capture_keyboard_sequences(
    parent: HWND,
    current: &[String],
    taken: TakenBindings,
) -> Option<Vec<String>> {
    ensure_capture_classes();
    let mut state = KeyboardCaptureState {
        sequences: current.to_vec(),
        taken,
        accepted: false,
    };
    show_capture_dialog(
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
    taken: TakenBindings,
) -> Option<Vec<String>> {
    ensure_capture_classes();
    let mut state = MouseCaptureState {
        binding: current.map(str::to_string),
        taken,
        accepted: false,
    };
    show_capture_dialog(
        parent,
        IDD_CAPTURE_MOUSE,
        mouse_procedure,
        &raw mut state as isize,
    );
    state.accepted.then(|| state.binding.into_iter().collect())
}

type DialogProcedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

fn show_capture_dialog(
    parent: HWND,
    template: u16,
    procedure: DialogProcedure,
    state_pointer: isize,
) {
    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    unsafe {
        DialogBoxParamW(
            Some(instance.into()),
            PCWSTR(template as usize as *const u16),
            Some(parent),
            Some(procedure),
            LPARAM(state_pointer),
        )
    };
}

fn warn_conflict(dialog: HWND, encoding: &str, owner_label: &str) {
    let content: Vec<u16> = format!("\"{encoding}\" is already bound to \"{owner_label}\"")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let configuration = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: dialog,
        pszWindowTitle: w!("Shortcut Already Used"),
        pszContent: PCWSTR(content.as_ptr()),
        ..Default::default()
    };
    let _ = unsafe {
        windows::Win32::UI::Controls::TaskDialogIndirect(&raw const configuration, None, None, None)
    };
}

fn held_modifiers() -> u8 {
    let pressed = |key: i32| (unsafe { GetKeyState(key) } as u16 & 0x8000) != 0;
    let mut modifiers = 0u8;
    if pressed(VK_CONTROL.0 as i32) {
        modifiers |= MODIFIER_CONTROL;
    }
    if pressed(VK_SHIFT.0 as i32) {
        modifiers |= MODIFIER_SHIFT;
    }
    if pressed(VK_MENU.0 as i32) {
        modifiers |= MODIFIER_ALT;
    }
    if pressed(VK_LWIN.0 as i32) || pressed(VK_RWIN.0 as i32) {
        modifiers |= MODIFIER_META;
    }
    modifiers
}

struct KeyboardCaptureState {
    sequences: Vec<String>,
    taken: TakenBindings,
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
            let state = unsafe { &*(lparam.0 as *const KeyboardCaptureState) };
            for sequence in &state.sequences {
                listbox_add(dialog, IDC_CAPTURE_KEY_LIST, sequence);
            }
            if let Ok(listbox) = unsafe { GetDlgItem(Some(dialog), IDC_CAPTURE_KEY_LIST) } {
                let procedure = key_list_procedure as *const core::ffi::c_void;
                let original =
                    unsafe { SetWindowLongPtrW(listbox, GWLP_WNDPROC, procedure as isize) };
                unsafe { SetWindowLongPtrW(listbox, GWLP_USERDATA, original) };
            }
            if let Ok(field) = unsafe { GetDlgItem(Some(dialog), IDC_CAPTURE_KEY_FIELD) } {
                let _ = unsafe { SetFocus(Some(field)) };
            }
            0
        }
        WM_RIV_KEY_CAPTURED => {
            let modifiers = (wparam.0 >> 16) as u8;
            let virtual_key = (wparam.0 & 0xFFFF) as u16;
            if let Some(sequence) = bindings::format_key_sequence(modifiers, virtual_key)
                && let Some(state) = state_mut::<KeyboardCaptureState>(dialog)
                && !state.sequences.contains(&sequence)
            {
                listbox_add(dialog, IDC_CAPTURE_KEY_LIST, &sequence);
                state.sequences.push(sequence);
            }
            1
        }
        WM_RIV_KEY_REMOVE => {
            if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                let index = wparam.0;
                if index < state.sequences.len() {
                    state.sequences.remove(index);
                    listbox_remove(dialog, IDC_CAPTURE_KEY_LIST, index);
                }
            }
            1
        }
        WM_DRAWITEM => {
            let draw = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
            if draw.CtlID == IDC_CAPTURE_KEY_LIST as u32 {
                draw_sequence_item(draw);
                return 1;
            }
            0
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command as i32 {
                IDC_CAPTURE_KEY_CLEAR => {
                    if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                        state.sequences.clear();
                        listbox_clear(dialog, IDC_CAPTURE_KEY_LIST);
                    }
                    1
                }
                command if command == IDOK as i32 => {
                    if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                        for sequence in &state.sequences {
                            if let Some((encoding, owner)) = state
                                .taken
                                .iter()
                                .find(|(encoding, _)| encoding == sequence)
                            {
                                warn_conflict(dialog, encoding, owner);
                                return 1; // conflict: block confirmation
                            }
                        }
                        state.accepted = true;
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

struct MouseCaptureState {
    binding: Option<String>,
    taken: TakenBindings,
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
            let state = unsafe { &*(lparam.0 as *const MouseCaptureState) };
            set_mouse_field_text(dialog, state.binding.as_deref());
            1
        }
        WM_RIV_MOUSE_CAPTURED => {
            let modifiers = (wparam.0 >> 8) as u8;
            let double_click = wparam.0 & 0x80 != 0;
            let base = match wparam.0 & 0x7F {
                0 => MouseBase::Left,
                1 => MouseBase::Middle,
                2 => MouseBase::Back,
                3 => MouseBase::Forward,
                4 => MouseBase::WheelUp,
                _ => MouseBase::WheelDown,
            };
            if let Some(state) = state_mut::<MouseCaptureState>(dialog) {
                let encoding = bindings::format_mouse_encoding(modifiers, double_click, base);
                set_mouse_field_text(dialog, Some(&encoding));
                state.binding = Some(encoding);
            }
            1
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command as i32 {
                IDC_CAPTURE_MOUSE_CLEAR => {
                    if let Some(state) = state_mut::<MouseCaptureState>(dialog) {
                        state.binding = None;
                        set_mouse_field_text(dialog, None);
                    }
                    1
                }
                command if command == IDOK as i32 => {
                    if let Some(state) = state_mut::<MouseCaptureState>(dialog) {
                        if let Some(binding) = &state.binding
                            && let Some((encoding, owner)) =
                                state.taken.iter().find(|(encoding, _)| encoding == binding)
                        {
                            warn_conflict(dialog, encoding, owner);
                            return 1;
                        }
                        state.accepted = true;
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
        let text: Vec<u16> = binding
            .unwrap_or("None")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = unsafe { SetWindowTextW(field, PCWSTR(text.as_ptr())) };
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

fn draw_sequence_item(draw: &DRAWITEMSTRUCT) {
    let selected = draw.itemState.0 & ODS_SELECTED.0 != 0;
    unsafe {
        FillRect(
            draw.hDC,
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
    const LB_GETTEXT: u32 = 0x0189;
    let mut text = [0u16; 128];
    let length = unsafe {
        SendMessageW(
            draw.hwndItem,
            LB_GETTEXT,
            Some(WPARAM(draw.itemID as usize)),
            Some(LPARAM(text.as_mut_ptr() as isize)),
        )
    };
    let length = usize::try_from(length.0).unwrap_or(0).min(text.len());
    unsafe {
        SetBkMode(draw.hDC, TRANSPARENT);
        SetTextColor(
            draw.hDC,
            COLORREF(GetSysColor(if selected {
                COLOR_HIGHLIGHTTEXT
            } else {
                COLOR_WINDOWTEXT
            })),
        );
        let mut bounds = draw.rcItem;
        bounds.left += 4;
        DrawTextW(
            draw.hDC,
            &mut text[..length],
            &raw mut bounds,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
    if selected {
        let zone = remove_icon_bounds(&draw.rcItem);
        let side = zone.bottom - zone.top;
        let inset = side / 4;
        let stroke = (side / 10).max(1);
        unsafe {
            let pen = CreatePen(PS_SOLID, stroke, REMOVE_ICON_RED);
            let previous = SelectObject(draw.hDC, pen.into());
            let _ = MoveToEx(draw.hDC, zone.left + inset, zone.top + inset, None);
            let _ = LineTo(draw.hDC, zone.right - inset, zone.bottom - inset);
            let _ = MoveToEx(draw.hDC, zone.right - inset, zone.top + inset, None);
            let _ = LineTo(draw.hDC, zone.left + inset, zone.bottom - inset);
            SelectObject(draw.hDC, previous);
            let _ = DeleteObject(pen.into());
        }
    }
}

unsafe extern "system" fn key_list_procedure(
    listbox: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original: WNDPROC = unsafe {
        std::mem::transmute(GetWindowLongPtrW(listbox, GWLP_USERDATA) as *const core::ffi::c_void)
    };
    if message == WM_LBUTTONDOWN {
        const LB_GETCURSEL: u32 = 0x0188;
        const LB_GETITEMRECT: u32 = 0x0198;
        let x = (lparam.0 & 0xFFFF) as i16 as i32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
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
                            WM_RIV_KEY_REMOVE,
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

fn listbox_add(dialog: HWND, control: i32, text: &str) {
    const LB_ADDSTRING: u32 = 0x0180;
    if let Ok(listbox) = unsafe { GetDlgItem(Some(dialog), control) } {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            SendMessageW(
                listbox,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(wide.as_ptr() as isize)),
            )
        };
    }
}

fn listbox_remove(dialog: HWND, control: i32, index: usize) {
    const LB_DELETESTRING: u32 = 0x0182;
    if let Ok(listbox) = unsafe { GetDlgItem(Some(dialog), control) } {
        unsafe { SendMessageW(listbox, LB_DELETESTRING, Some(WPARAM(index)), None) };
    }
}

fn listbox_clear(dialog: HWND, control: i32) {
    const LB_RESETCONTENT: u32 = 0x0184;
    if let Ok(listbox) = unsafe { GetDlgItem(Some(dialog), control) } {
        unsafe { SendMessageW(listbox, LB_RESETCONTENT, None, None) };
    }
}

pub fn ensure_capture_classes() {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
        for (class_name, procedure, style) in [
            (
                w!("RivKeyCapture"),
                key_field_procedure as unsafe extern "system" fn(_, _, _, _) -> _,
                Default::default(),
            ),
            (w!("RivMouseCapture"), mouse_field_procedure, CS_DBLCLKS),
        ] {
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style,
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
                lpszClassName: class_name,
                ..Default::default()
            };
            let atom = unsafe { RegisterClassExW(&raw const class) };
            assert!(atom != 0, "capture field class registration failed");
        }
    });
}

fn field_font(field: HWND) -> HFONT {
    HFONT(unsafe { GetWindowLongPtrW(field, WINDOW_LONG_PTR_INDEX(-21)) } as *mut _)
}

fn field_paint(field: HWND, text: &str, hint: bool) {
    let mut paint = PAINTSTRUCT::default();
    let device = unsafe { BeginPaint(field, &raw mut paint) };
    unsafe {
        FillRect(
            device,
            &raw const paint.rcPaint,
            GetSysColorBrush(COLOR_WINDOW),
        );
        let font = field_font(field);
        if !font.is_invalid() {
            SelectObject(device, font.into());
        }
        SetBkMode(device, TRANSPARENT);
        SetTextColor(
            device,
            COLORREF(GetSysColor(if hint {
                COLOR_GRAYTEXT
            } else {
                COLOR_WINDOWTEXT
            })),
        );
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut bounds = paint.rcPaint;
        bounds.left += 4;
        DrawTextW(
            device,
            &mut wide,
            &raw mut bounds,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        let _ = EndPaint(field, &raw const paint);
    }
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

unsafe extern "system" fn key_field_procedure(
    field: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_GETDLGCODE => LRESULT(DLGC_WANTALLKEYS as isize),
        WM_SETFONT => {
            unsafe { SetWindowLongPtrW(field, WINDOW_LONG_PTR_INDEX(-21), wparam.0 as isize) };
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let virtual_key = wparam.0 as u16;
            if is_modifier_key(virtual_key) {
                let _ = unsafe { InvalidateRect(Some(field), None, true) };
            } else {
                let packed = ((held_modifiers() as usize) << 16) | virtual_key as usize;
                if let Ok(parent) = unsafe { GetParent(field) } {
                    unsafe {
                        SendMessageW(parent, WM_RIV_KEY_CAPTURED, Some(WPARAM(packed)), None)
                    };
                }
            }
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let _ = unsafe { InvalidateRect(Some(field), None, true) };
            LRESULT(0)
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = unsafe { InvalidateRect(Some(field), None, true) };
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
                let prefix = bindings::modifier_prefix(held_modifiers());
                if prefix.is_empty() {
                    field_paint(field, "Press a key combination\u{2026}", true);
                } else {
                    field_paint(field, &format!("{prefix}\u{2026}"), false);
                }
            } else {
                field_paint(field, "Click here to capture", true);
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
    fn notify(field: HWND, double_click: bool, base_index: usize) -> LRESULT {
        let packed =
            ((held_modifiers() as usize) << 8) | (usize::from(double_click) << 7) | base_index;
        if let Ok(parent) = unsafe { GetParent(field) } {
            unsafe { SendMessageW(parent, WM_RIV_MOUSE_CAPTURED, Some(WPARAM(packed)), None) };
        }
        LRESULT(0)
    }
    match message {
        WM_SETFONT => {
            unsafe { SetWindowLongPtrW(field, WINDOW_LONG_PTR_INDEX(-21), wparam.0 as isize) };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(field)) };
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => notify(field, true, 0),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => notify(field, false, 1),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
            let base_index = if (wparam.0 >> 16) & 0x2 != 0 { 3 } else { 2 };
            notify(field, false, base_index)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
            notify(field, false, if delta > 0 { 4 } else { 5 })
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = unsafe { InvalidateRect(Some(field), None, true) };
            LRESULT(0)
        }
        WM_PAINT => {
            let mut text = [0u16; 128];
            let length = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(field, &mut text)
            };
            let current = String::from_utf16_lossy(&text[..length as usize]);
            let hint = current.is_empty() || current == "None";
            field_paint(
                field,
                if current.is_empty() { "None" } else { &current },
                hint,
            );
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(field, message, wparam, lparam) },
    }
}
