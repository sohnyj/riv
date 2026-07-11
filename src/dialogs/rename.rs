//! 이름 변경 다이얼로그 — 인메모리 DLGTEMPLATE + Edit 컨트롤 (SPEC §6.4,
//! PORTING_PLAN §3 매핑: QInputDialog → DialogBoxIndirectParamW).
//! 확장자 제외 부분 프리셀렉트, 폰트는 Segoe UI 9 (R12 — MS Shell Dlg 금지).

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DLGTEMPLATE, DialogBoxIndirectParamW, EndDialog, GetDlgItem, GetDlgItemTextW, SendMessageW,
    SetDlgItemTextW, WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG,
};

const EDIT_IDENTIFIER: i32 = 100;
const IDOK: usize = 1;
const IDCANCEL: usize = 2;
/// DWLP_USER = DWLP_DLGPROC(8) + 포인터 크기(8) — winuser.h 파생 상수 (x64)
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);

/// WM_INITDIALOG ↔ 다이얼로그 프로시저 공유 상태
struct RenameState {
    initial_name: Vec<u16>,
    /// 확장자 제외 프리셀렉트 길이 (UTF-16 단위)
    stem_length: usize,
    accepted_name: Option<String>,
}

/// 모달 표시 — 확정 시 새 파일명 반환 (SPEC §6.4)
pub fn show(window: HWND, current_name: &str) -> Option<String> {
    let stem_length = current_name.rfind('.').filter(|dot| *dot > 0).map_or_else(
        || current_name.encode_utf16().count(),
        |dot| current_name[..dot].encode_utf16().count(),
    );
    let mut state = RenameState {
        initial_name: current_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect(),
        stem_length,
        accepted_name: None,
    };
    let template = build_template();
    let confirmed = unsafe {
        DialogBoxIndirectParamW(
            None,
            template.as_ptr().cast::<DLGTEMPLATE>(),
            Some(window),
            Some(dialog_procedure),
            LPARAM(&raw mut state as isize),
        )
    };
    (confirmed == IDOK as isize)
        .then(|| state.accepted_name.take())
        .flatten()
        .filter(|name| !name.trim().is_empty())
}

/// 다이얼로그 프로시저 — DWLP_USER 대신 초기화 시점에 정적 슬롯 없이 lparam 전달을
/// GWLP_USERDATA로 보관하기엔 과하므로, 상태 포인터를 창 프로퍼티 없이 WM_INITDIALOG
/// lparam → SetWindowLongPtrW(DWLP_USER) 경로로 유지한다.
extern "system" fn dialog_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            crate::dialogs::center_on_owner(dialog);
            let state = unsafe { &*(lparam.0 as *const RenameState) };
            unsafe {
                let _ = SetDlgItemTextW(
                    dialog,
                    EDIT_IDENTIFIER,
                    windows::core::PCWSTR(state.initial_name.as_ptr()),
                );
                if let Ok(edit) = GetDlgItem(Some(dialog), EDIT_IDENTIFIER) {
                    // 확장자 제외 프리셀렉트 (SPEC §6.4)
                    SendMessageW(
                        edit,
                        EM_SETSEL,
                        Some(WPARAM(0)),
                        Some(LPARAM(state.stem_length as isize)),
                    );
                    let _ = SetFocus(Some(edit));
                }
            }
            0 // 포커스를 직접 지정했으므로 FALSE
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command {
                IDOK => {
                    let pointer =
                        unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut RenameState;
                    if let Some(state) = unsafe { pointer.as_mut() } {
                        let mut buffer = [0u16; 1024];
                        let length =
                            unsafe { GetDlgItemTextW(dialog, EDIT_IDENTIFIER, &mut buffer) };
                        state.accepted_name =
                            Some(String::from_utf16_lossy(&buffer[..length as usize]));
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                IDCANCEL => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// 인메모리 DLGTEMPLATE — Edit + OK/Cancel, FONT 9 "Segoe UI" (R12)
fn build_template() -> Vec<u16> {
    const DS_SETFONT: u32 = 0x40;
    const DS_MODALFRAME: u32 = 0x80;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_SYSMENU: u32 = 0x0008_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_TABSTOP: u32 = 0x0001_0000;
    const WS_BORDER: u32 = 0x0080_0000;
    const ES_AUTOHSCROLL: u32 = 0x80;
    const BS_DEFPUSHBUTTON: u32 = 0x1;

    let mut template: Vec<u16> = Vec::new();
    let push_u32 = |buffer: &mut Vec<u16>, value: u32| {
        buffer.push((value & 0xFFFF) as u16);
        buffer.push((value >> 16) as u16);
    };

    // DLGTEMPLATE 헤더
    push_u32(
        &mut template,
        DS_SETFONT | DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
    );
    push_u32(&mut template, 0); // dwExtendedStyle
    template.push(3); // cdit
    template.extend_from_slice(&[0, 0, 220, 54]); // x, y, cx, cy (다이얼로그 단위)
    template.push(0); // 메뉴 없음
    template.push(0); // 기본 클래스
    template.extend("Rename".encode_utf16().chain(std::iter::once(0))); // 타이틀
    template.push(9); // FONT 9pt
    template.extend("Segoe UI".encode_utf16().chain(std::iter::once(0)));

    let push_item = |buffer: &mut Vec<u16>,
                     style: u32,
                     bounds: [i16; 4],
                     identifier: u16,
                     class_atom: u16,
                     text: &str| {
        // DLGITEMTEMPLATE는 DWORD 정렬 — u16 버퍼 길이가 짝수여야 함
        if !buffer.len().is_multiple_of(2) {
            buffer.push(0);
        }
        let push_u32_inner = |buffer: &mut Vec<u16>, value: u32| {
            buffer.push((value & 0xFFFF) as u16);
            buffer.push((value >> 16) as u16);
        };
        push_u32_inner(buffer, style);
        push_u32_inner(buffer, 0); // exstyle
        buffer.extend(bounds.iter().map(|value| *value as u16));
        buffer.push(identifier);
        buffer.extend_from_slice(&[0xFFFF, class_atom]);
        buffer.extend(text.encode_utf16().chain(std::iter::once(0)));
        buffer.push(0); // 생성 데이터 없음
    };

    // Edit (클래스 atom 0x0081)
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
        [7, 7, 206, 13],
        EDIT_IDENTIFIER as u16,
        0x0081,
        "",
    );
    // OK / Cancel (버튼 atom 0x0080)
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON,
        [106, 30, 50, 14],
        IDOK as u16,
        0x0080,
        "OK",
    );
    push_item(
        &mut template,
        WS_VISIBLE | WS_TABSTOP,
        [163, 30, 50, 14],
        IDCANCEL as u16,
        0x0080,
        "Cancel",
    );
    template
}
