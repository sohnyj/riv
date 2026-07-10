//! 단축키 캡처 다이얼로그 (SPEC §8.3) — 키 시퀀스 캡처 / 마우스 클릭-투-레코드.
//!
//! 캡처 필드는 등록 클래스(RivKeyCapture·RivMouseCapture)로 raw Win32 메시지를
//! 직접 받는다: 수정자는 `GetKeyState`, 더블클릭은 `CS_DBLCLKS`(마우스), 우클릭은
//! 컨텍스트 메뉴 예약이라 무시. qView의 Qualifier 콤보 + Double 체크박스는 Qt 캡처
//! 제약의 우회 UI라 이식하지 않음(2026-07-10 결정).
//! 충돌 검사는 qView와 동일하게 OK 시점 경고 + 차단 (qvshortcutdialog.cpp).

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DrawTextW, EndPaint, FillRect, GetSysColor, GetSysColorBrush, HFONT, InvalidateRect,
    PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::TASKDIALOGCONFIG;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, DLGC_WANTALLKEYS, DefWindowProcW, DialogBoxParamW, EndDialog, GetDlgItem,
    GetParent, GetWindowLongPtrW, RegisterClassExW, SendMessageW, SetWindowLongPtrW,
    SetWindowTextW, WINDOW_LONG_PTR_INDEX, WM_APP, WM_COMMAND, WM_GETDLGCODE, WM_INITDIALOG,
    WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MBUTTONDBLCLK,
    WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT, WM_SETFOCUS, WM_SETFONT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDBLCLK, WM_XBUTTONDOWN, WNDCLASSEXW,
};
use windows::core::{PCWSTR, w};

use crate::bindings::{
    self, MODIFIER_ALT, MODIFIER_CONTROL, MODIFIER_META, MODIFIER_SHIFT, MouseBase,
};
use crate::dialogs::resource::{
    IDC_CAPTURE_KEY_CLEAR, IDC_CAPTURE_KEY_FIELD, IDC_CAPTURE_KEY_LIST, IDC_CAPTURE_KEY_REMOVE,
    IDC_CAPTURE_MOUSE_CLEAR, IDC_CAPTURE_MOUSE_FIELD, IDD_CAPTURE_KEYBOARD, IDD_CAPTURE_MOUSE,
};

const IDOK: usize = 1;
const IDCANCEL: usize = 2;
/// DWLP_USER (x64) — rename.rs와 동일 파생
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

/// 캡처 필드 → 부모 다이얼로그 통지. wparam = (수정자 << 16) | 가상 키
const WM_RIV_KEY_CAPTURED: u32 = WM_APP + 0x40;
/// wparam = (수정자 << 8) | (Double << 7) | 베이스 인덱스
const WM_RIV_MOUSE_CAPTURED: u32 = WM_APP + 0x41;

/// 다른 액션이 이미 소유한 인코딩 목록 — (인코딩 문자열, 액션 라벨). 충돌 검사용.
pub type TakenBindings = Vec<(String, &'static str)>;

/// 키 시퀀스 편집 — 확정 시 새 목록(빈 목록 = 바인딩 제거), 취소 시 None
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

/// 마우스 바인딩 편집 — 확정 시 새 목록(0 또는 1개 — qView 패리티), 취소 시 None
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

/// "X is already bound to Y" 경고 (qvshortcutdialog.cpp 패리티 — OK 차단)
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
        windows::Win32::UI::Controls::TaskDialogIndirect(&configuration, None, None, None)
    };
}

/// 현재 눌린 수정자 (main.rs current_modifiers와 동일 규칙)
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

// ── 키보드 캡처 다이얼로그 ──────────────────────────────────────────────────

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
            // 초기 포커스 = 캡처 필드 (바로 누르면 캡처)
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
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command as i32 {
                IDC_CAPTURE_KEY_REMOVE => {
                    if let Some(state) = state_mut::<KeyboardCaptureState>(dialog) {
                        let selected = listbox_selection(dialog, IDC_CAPTURE_KEY_LIST);
                        if let Some(index) = selected
                            && index < state.sequences.len()
                        {
                            state.sequences.remove(index);
                            listbox_remove(dialog, IDC_CAPTURE_KEY_LIST, index);
                        }
                    }
                    1
                }
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
                                return 1; // 충돌 — 확정 차단 (qView 패리티)
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

// ── 마우스 캡처 다이얼로그 ──────────────────────────────────────────────────

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

// ── 다이얼로그 상태·리스트박스 헬퍼 ─────────────────────────────────────────

fn state_mut<State>(dialog: HWND) -> Option<&'static mut State> {
    let pointer = unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut State;
    unsafe { pointer.as_mut() }
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

fn listbox_selection(dialog: HWND, control: i32) -> Option<usize> {
    const LB_GETCURSEL: u32 = 0x0188;
    const LB_ERR: isize = -1;
    let listbox = unsafe { GetDlgItem(Some(dialog), control) }.ok()?;
    let selected = unsafe { SendMessageW(listbox, LB_GETCURSEL, None, None) };
    (selected.0 != LB_ERR).then_some(selected.0 as usize)
}

// ── 캡처 필드 클래스 ────────────────────────────────────────────────────────

/// 캡처 필드 클래스 등록 — 다이얼로그 생성 전 1회 (템플릿이 클래스명 참조)
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
            let atom = unsafe { RegisterClassExW(&class) };
            assert!(atom != 0, "capture field class registration failed");
        }
    });
}

/// GWLP_USERDATA = WM_SETFONT로 받은 HFONT (캡처 필드 공용)
fn field_font(field: HWND) -> HFONT {
    HFONT(unsafe { GetWindowLongPtrW(field, WINDOW_LONG_PTR_INDEX(-21)) } as *mut _)
}

fn field_paint(field: HWND, text: &str, hint: bool) {
    let mut paint = PAINTSTRUCT::default();
    let device = unsafe { BeginPaint(field, &mut paint) };
    unsafe {
        FillRect(device, &paint.rcPaint, GetSysColorBrush(COLOR_WINDOW));
        let font = field_font(field);
        if !font.is_invalid() {
            SelectObject(device, font.into());
        }
        SetBkMode(device, TRANSPARENT);
        SetTextColor(
            device,
            windows::Win32::Foundation::COLORREF(GetSysColor(if hint {
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
            &mut bounds,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        let _ = EndPaint(field, &paint);
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
    /// (Double << 7) | 베이스 인덱스 — WM_RIV_MOUSE_CAPTURED 포장
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
        // Left 단일 프레스는 팬 예약(SPEC §5.3) — 포커스 이동만, Double만 기록
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(field)) };
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => notify(field, true, 0),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => notify(field, false, 1),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
            // HIWORD(wParam): XBUTTON1 = Back, XBUTTON2 = Forward
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
