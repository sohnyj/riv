//! 옵션 다이얼로그 (SPEC §8.3, PORTING_PLAN §4) — .rc 템플릿 + comctl32 v6.
//!
//! - 탭 5: Window / Image / Miscellaneous / Shortcuts / File Association.
//! - transient 편집 모델: Apply 활성 = 저장값과 diff, Restore Defaults 활성 =
//!   기본값과 diff(파일 연결은 기본값 개념 없음 — 현 레지스트리 상태가 기준).
//! - Apply·OK: 파일 연결 레지스트리 동기화(다이얼로그가 직접) + 옵션·바인딩 저장을
//!   `WM_APP_OPTIONS_APPLIED`로 메인 창에 위임(저장 + 전 컴포넌트 브로드캐스트).
//! - Shortcuts: 3컬럼 ListView, 더블클릭 편집(키/마우스 캡처 — shortcut_capture.rs).
//! - File Association: 디코더 레지스트리 기반 동적 그룹핑(decode::format_groups),
//!   tri-state 트리(상태 이미지 3종), 단일 확장자 포맷은 헤더 생략, 알파벳 정렬.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DFC_BUTTON, DFCS_BUTTON3STATE,
    DFCS_BUTTONCHECK, DFCS_CHECKED, DeleteDC, DeleteObject, DrawFrameControl, FillRect, FrameRect,
    GetDC, GetSysColorBrush, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, CheckRadioButton, DRAWITEMSTRUCT, HIMAGELIST,
    HTREEITEM, ILC_COLOR32, ILC_MASK, ImageList_Add, ImageList_Create, IsDlgButtonChecked,
    LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVITEMW, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVS_EX_FULLROWSELECT, NM_CLICK, NM_DBLCLK,
    NMHDR, NMITEMACTIVATE, NMTVKEYDOWN, TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT, TCM_GETCURSEL,
    TCM_INSERTITEMW, TCN_SELCHANGE, TVGN_CARET, TVHITTESTINFO, TVHT_ONITEMSTATEICON, TVI_LAST,
    TVI_ROOT, TVIF_PARAM, TVIF_STATE, TVIF_TEXT, TVINSERTSTRUCTW, TVIS_STATEIMAGEMASK, TVITEMEXW,
    TVM_GETITEMW, TVM_GETNEXTITEM, TVM_HITTEST, TVM_INSERTITEMW, TVM_SETIMAGELIST, TVM_SETITEMW,
    TVN_KEYDOWN, TVSIL_STATE, UDM_SETRANGE32,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::{
    CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CreateDialogParamW, DestroyWindow, DialogBoxParamW,
    EndDialog, GetDlgItem, GetDlgItemInt, GetDlgItemTextW, GetMessagePos, GetWindowLongPtrW,
    GetWindowRect, SW_HIDE, SW_SHOW, SendMessageW, SetDlgItemTextW, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WINDOW_LONG_PTR_INDEX, WM_APP, WM_COMMAND, WM_DESTROY, WM_DRAWITEM,
    WM_INITDIALOG, WM_NOTIFY,
};
use windows::core::PCWSTR;

use crate::actions::Action;
use crate::bindings;
use crate::dialogs::resource::*;
use crate::dialogs::shortcut_capture;
use crate::image::decode;
use crate::settings::{Options, SettingsFile};
use crate::shell::file_association;

/// Apply·OK 통지 — lparam = `*const AppliedOptions` (수신 측이 저장 + 브로드캐스트)
pub const WM_APP_OPTIONS_APPLIED: u32 = WM_APP + 5;
/// 다이얼로그 위치 통지 (SPEC §8.1 optionsgeometry) — lparam = x(하위 32비트)·y(상위)
pub const WM_APP_OPTIONS_GEOMETRY: u32 = WM_APP + 6;

/// Apply 페이로드 — 옵션 전 항목 + 액션별 확정 바인딩 목록
pub struct AppliedOptions {
    pub options: Options,
    pub keyboard: Vec<(String, Vec<String>)>,
    pub mouse: Vec<(String, Vec<String>)>,
}

const IDOK: usize = 1;
const IDCANCEL: usize = 2;
/// DWLP_USER (x64) — rename.rs와 동일 파생
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(16);
const BN_CLICKED: usize = 0;
const CBN_SELCHANGE: usize = 1;
const EN_CHANGE: usize = 0x0300;

/// 트리 lparam 인코딩 — 그룹은 상위 비트, 확장자는 인덱스
const GROUP_FLAG: isize = 0x1000_0000;

/// tri-state 상태 이미지 인덱스 (1-기반 — 0은 "이미지 없음")
const STATE_UNCHECKED: isize = 1;
const STATE_CHECKED: isize = 2;
const STATE_PARTIAL: isize = 3;

#[derive(Clone, PartialEq)]
struct ShortcutRow {
    action: Action,
    keyboard: Vec<String>,
    mouse: Vec<String>,
}

struct AssociationExtension {
    /// ".png" 형태 (레지스트리 표기)
    extension: String,
    checked: bool,
    item: HTREEITEM,
}

struct AssociationGroup {
    item: HTREEITEM,
    members: Vec<usize>,
}

struct OptionsState {
    parent: HWND,
    dialog: HWND,
    pages: [HWND; 5],
    saved_options: Options,
    transient_options: Options,
    saved_shortcuts: Vec<ShortcutRow>,
    transient_shortcuts: Vec<ShortcutRow>,
    saved_associations: Vec<String>,
    extensions: Vec<AssociationExtension>,
    groups: Vec<AssociationGroup>,
    /// 프로그램적 UI 갱신 중 컨트롤 알림 무시
    syncing: bool,
    state_images: HIMAGELIST,
    custom_colors: [COLORREF; 16],
    /// 저장된 다이얼로그 위치 (optionsgeometry) — WM_INITDIALOG에서 적용
    initial_position: Option<(i32, i32)>,
}

impl OptionsState {
    fn desired_associations(&self) -> Vec<String> {
        self.extensions
            .iter()
            .filter(|entry| entry.checked)
            .map(|entry| entry.extension.clone())
            .collect()
    }

    /// Apply 활성 조건 = 저장 상태와 diff (SPEC §8.3)
    fn dirty(&self) -> bool {
        self.transient_options != self.saved_options
            || self.transient_shortcuts != self.saved_shortcuts
            || self.desired_associations() != self.saved_associations
    }

    /// Restore Defaults 활성 조건 = 기본값과 diff — 파일 연결 제외 (SPEC §8.3)
    fn differs_from_defaults(&self) -> bool {
        self.transient_options != Options::default()
            || self.transient_shortcuts != default_shortcut_rows()
    }
}

fn default_shortcut_rows() -> Vec<ShortcutRow> {
    Action::all_bindable()
        .map(|action| ShortcutRow {
            action,
            keyboard: bindings::default_keyboard_sequences(action.name())
                .iter()
                .map(|sequence| (*sequence).to_string())
                .collect(),
            mouse: bindings::default_mouse_encodings(action.name())
                .iter()
                .map(|encoding| (*encoding).to_string())
                .collect(),
        })
        .collect()
}

/// 모달 표시 (Action::Options) — Apply·OK 시 WM_APP_OPTIONS_APPLIED가 parent로 간다
pub fn show(parent: HWND, settings: &SettingsFile) {
    shortcut_capture::ensure_capture_classes();
    let shortcuts: Vec<ShortcutRow> = Action::all_bindable()
        .map(|action| ShortcutRow {
            action,
            keyboard: bindings::resolved_keyboard_sequences(
                settings.keyboard_bindings(),
                action.name(),
            ),
            mouse: bindings::resolved_mouse_encodings(settings.mouse_bindings(), action.name()),
        })
        .collect();
    let mut saved_associations = file_association::registered_extensions();
    saved_associations.sort();
    let mut state = OptionsState {
        parent,
        dialog: HWND::default(),
        pages: [HWND::default(); 5],
        saved_options: settings.options.clone(),
        transient_options: settings.options.clone(),
        saved_shortcuts: shortcuts.clone(),
        transient_shortcuts: shortcuts,
        saved_associations,
        extensions: Vec::new(),
        groups: Vec::new(),
        syncing: false,
        state_images: HIMAGELIST::default(),
        custom_colors: [COLORREF(0x00FF_FFFF); 16],
        initial_position: settings.options_geometry(),
    };
    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    unsafe {
        DialogBoxParamW(
            Some(instance.into()),
            PCWSTR(IDD_OPTIONS as usize as *const u16),
            Some(parent),
            Some(frame_procedure),
            LPARAM(&raw mut state as isize),
        )
    };
}

fn state_mut(dialog: HWND) -> Option<&'static mut OptionsState> {
    let pointer = unsafe { GetWindowLongPtrW(dialog, DWLP_USER) } as *mut OptionsState;
    unsafe { pointer.as_mut() }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── 프레임 ──────────────────────────────────────────────────────────────────

unsafe extern "system" fn frame_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            let state = unsafe { &mut *(lparam.0 as *mut OptionsState) };
            state.dialog = dialog;
            initialize_frame(state);
            1
        }
        WM_NOTIFY => {
            let header = unsafe { &*(lparam.0 as *const NMHDR) };
            if header.idFrom == IDC_OPTIONS_TAB as usize
                && header.code == TCN_SELCHANGE
                && let Some(state) = state_mut(dialog)
            {
                let selected =
                    unsafe { SendMessageW(header.hwndFrom, TCM_GETCURSEL, None, None).0 };
                for (index, page) in state.pages.iter().enumerate() {
                    let _ = unsafe {
                        ShowWindow(
                            *page,
                            if index as isize == selected {
                                SW_SHOW
                            } else {
                                SW_HIDE
                            },
                        )
                    };
                }
            }
            0
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xFFFF;
            match command as i32 {
                command if command == IDOK as i32 => {
                    if let Some(state) = state_mut(dialog) {
                        apply(state);
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                command if command == IDCANCEL as i32 => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                IDC_APPLY => {
                    if let Some(state) = state_mut(dialog) {
                        apply(state);
                    }
                    1
                }
                IDC_RESTORE_DEFAULTS => {
                    if let Some(state) = state_mut(dialog) {
                        state.transient_options = Options::default();
                        state.transient_shortcuts = default_shortcut_rows();
                        sync_all_pages(state);
                        update_buttons(state);
                    }
                    1
                }
                _ => 0,
            }
        }
        WM_DESTROY => {
            if let Some(state) = state_mut(dialog) {
                // 위치 저장 (optionsgeometry) — OK/Cancel 무관
                let mut bounds = RECT::default();
                if unsafe { GetWindowRect(dialog, &mut bounds) }.is_ok() {
                    let packed = (bounds.left as u32 as isize) | ((bounds.top as isize) << 32);
                    unsafe {
                        SendMessageW(
                            state.parent,
                            WM_APP_OPTIONS_GEOMETRY,
                            None,
                            Some(LPARAM(packed)),
                        )
                    };
                }
                for page in state.pages {
                    if !page.is_invalid() {
                        let _ = unsafe { DestroyWindow(page) };
                    }
                }
                // TVSIL_STATE 이미지 리스트는 트리가 소유하지 않음 — 직접 해제
                if !state.state_images.is_invalid() {
                    let _ = unsafe {
                        windows::Win32::UI::Controls::ImageList_Destroy(Some(state.state_images))
                    };
                }
            }
            0
        }
        _ => 0,
    }
}

fn initialize_frame(state: &mut OptionsState) {
    let dialog = state.dialog;
    // 저장된 위치 또는 작업 영역 중앙 (SPEC §8.3 — 2026-07-11: 기본 위치 = 스크린 중앙)
    let position = state.initial_position.or_else(|| {
        let mut bounds = RECT::default();
        let _ = unsafe { GetWindowRect(dialog, &mut bounds) };
        crate::window::work_area_centered_origin(
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
        )
    });
    if let Some((x, y)) = position {
        let _ = unsafe {
            SetWindowPos(
                dialog,
                None,
                x,
                y,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
            )
        };
    }
    let Ok(tab) = (unsafe { GetDlgItem(Some(dialog), IDC_OPTIONS_TAB) }) else {
        return;
    };
    for (index, title) in [
        "Window",
        "Image",
        "Miscellaneous",
        "Shortcuts",
        "File Association",
    ]
    .iter()
    .enumerate()
    {
        let text = wide(title);
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: windows::core::PWSTR(text.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                tab,
                TCM_INSERTITEMW,
                Some(WPARAM(index)),
                Some(LPARAM(&raw const item as isize)),
            )
        };
    }

    // 탭 표시 영역(다이얼로그 클라이언트 좌표) — 페이지 배치 기준
    let mut display = RECT::default();
    let _ = unsafe { GetWindowRect(tab, &mut display) };
    unsafe {
        SendMessageW(
            tab,
            TCM_ADJUSTRECT,
            Some(WPARAM(0)),
            Some(LPARAM(&raw mut display as isize)),
        )
    };
    let mut corners = [
        POINT {
            x: display.left,
            y: display.top,
        },
        POINT {
            x: display.right,
            y: display.bottom,
        },
    ];
    unsafe { windows::Win32::Graphics::Gdi::MapWindowPoints(None, Some(dialog), &mut corners) };

    let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    let state_pointer = state as *mut OptionsState as isize;
    for (index, template) in [
        IDD_PAGE_WINDOW,
        IDD_PAGE_IMAGE,
        IDD_PAGE_MISC,
        IDD_PAGE_SHORTCUTS,
        IDD_PAGE_ASSOCIATION,
    ]
    .iter()
    .enumerate()
    {
        let page = unsafe {
            CreateDialogParamW(
                Some(instance.into()),
                PCWSTR(*template as usize as *const u16),
                Some(dialog),
                Some(page_procedure),
                LPARAM(state_pointer),
            )
        }
        .unwrap_or_default();
        // Z-order를 탭 컨트롤 바로 뒤에 — 포커스 순서 = 탭 → 페이지 → 하단 버튼
        let _ = unsafe {
            SetWindowPos(
                page,
                Some(tab),
                corners[0].x,
                corners[0].y,
                corners[1].x - corners[0].x,
                corners[1].y - corners[0].y,
                windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(0),
            )
        };
        state.pages[index] = page;
    }

    initialize_image_page(state);
    initialize_window_page(state);
    initialize_misc_page(state);
    initialize_shortcuts_page(state);
    initialize_association_page(state);
    sync_all_pages(state);
    update_buttons(state);
    let _ = unsafe { ShowWindow(state.pages[0], SW_SHOW) };
}

fn update_buttons(state: &OptionsState) {
    let enable = |control: i32, enabled: bool| {
        if let Ok(button) = unsafe { GetDlgItem(Some(state.dialog), control) } {
            let _ = unsafe { EnableWindow(button, enabled) };
        }
    };
    enable(IDC_APPLY, state.dirty());
    enable(IDC_RESTORE_DEFAULTS, state.differs_from_defaults());
}

/// Apply — 연결 레지스트리는 직접, 옵션·바인딩 저장은 메인 창 위임 (SPEC §8.3)
fn apply(state: &mut OptionsState) {
    if !state.dirty() {
        return;
    }
    let desired = state.desired_associations();
    if desired != state.saved_associations {
        file_association::set_file_associations(&desired);
        state.saved_associations = desired;
    }
    let payload = AppliedOptions {
        options: state.transient_options.clone(),
        keyboard: state
            .transient_shortcuts
            .iter()
            .map(|row| (row.action.name().to_string(), row.keyboard.clone()))
            .collect(),
        mouse: state
            .transient_shortcuts
            .iter()
            .map(|row| (row.action.name().to_string(), row.mouse.clone()))
            .collect(),
    };
    unsafe {
        SendMessageW(
            state.parent,
            WM_APP_OPTIONS_APPLIED,
            None,
            Some(LPARAM(&raw const payload as isize)),
        )
    };
    state.saved_options = state.transient_options.clone();
    state.saved_shortcuts = state.transient_shortcuts.clone();
    update_buttons(state);
}

// ── 페이지 공용 프로시저 ────────────────────────────────────────────────────

unsafe extern "system" fn page_procedure(
    page: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(page, DWLP_USER, lparam.0) };
            1
        }
        WM_COMMAND => {
            let Some(state) = state_mut(page) else {
                return 0;
            };
            if state.syncing {
                return 0;
            }
            let control = (wparam.0 & 0xFFFF) as i32;
            let notification = wparam.0 >> 16;
            handle_page_command(state, page, control, notification)
        }
        WM_NOTIFY => {
            let Some(state) = state_mut(page) else {
                return 0;
            };
            let header = unsafe { &*(lparam.0 as *const NMHDR) };
            match header.idFrom as i32 {
                IDC_SHORTCUTS_LIST if header.code == NM_DBLCLK => {
                    let activate = unsafe { &*(lparam.0 as *const NMITEMACTIVATE) };
                    if activate.iItem >= 0 {
                        edit_shortcut(state, activate.iItem as usize, activate.iSubItem == 2);
                    }
                    1
                }
                IDC_ASSOC_TREE if header.code == NM_CLICK => {
                    toggle_association_at_cursor(state, header.hwndFrom);
                    1
                }
                IDC_ASSOC_TREE if header.code == TVN_KEYDOWN => {
                    let key_down = unsafe { &*(lparam.0 as *const NMTVKEYDOWN) };
                    if key_down.wVKey == VK_SPACE.0 {
                        let selected = unsafe {
                            SendMessageW(
                                header.hwndFrom,
                                TVM_GETNEXTITEM,
                                Some(WPARAM(TVGN_CARET as usize)),
                                None,
                            )
                        };
                        if selected.0 != 0 {
                            toggle_association_item(state, header.hwndFrom, HTREEITEM(selected.0));
                        }
                    }
                    1
                }
                _ => 0,
            }
        }
        WM_DRAWITEM => {
            let Some(state) = state_mut(page) else {
                return 0;
            };
            let draw = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
            if draw.CtlID == IDC_WINDOW_BGCOLOR_BUTTON as u32 {
                let (red, green, blue) = state.transient_options.background_color;
                let brush = unsafe {
                    CreateSolidBrush(COLORREF(
                        u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16),
                    ))
                };
                unsafe {
                    FillRect(draw.hDC, &draw.rcItem, brush);
                    FrameRect(
                        draw.hDC,
                        &draw.rcItem,
                        GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_BTNSHADOW),
                    );
                    let _ = DeleteObject(brush.into());
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

/// 컨트롤 → transient 반영. 반환 = 처리 여부
fn handle_page_command(
    state: &mut OptionsState,
    page: HWND,
    control: i32,
    notification: usize,
) -> isize {
    let options = &mut state.transient_options;
    let mut handled = true;
    match (control, notification) {
        // ── Window ──
        (IDC_WINDOW_BGCOLOR_ENABLED, BN_CLICKED) => {
            options.background_color_enabled = is_checked(page, control);
            sync_bgcolor_button(state, page);
        }
        (IDC_WINDOW_BGCOLOR_BUTTON, BN_CLICKED) => {
            choose_background_color(state, page);
        }
        (IDC_WINDOW_TITLEBAR_BASIC, BN_CLICKED) => options.title_bar_mode = 0,
        (IDC_WINDOW_TITLEBAR_MINIMAL, BN_CLICKED) => options.title_bar_mode = 1,
        (IDC_WINDOW_TITLEBAR_PRACTICAL, BN_CLICKED) => options.title_bar_mode = 2,
        (IDC_WINDOW_FITMODE, CBN_SELCHANGE) => {
            options.fit_mode = combo_selection(page, control);
        }
        (IDC_WINDOW_SAVE_POSITION, BN_CLICKED) => {
            options.save_window_position = is_checked(page, control);
        }
        (IDC_WINDOW_CTRL_DRAG, BN_CLICKED) => {
            options.control_drag_window = is_checked(page, control);
        }
        // ── Image ──
        (IDC_IMAGE_FILTERING, CBN_SELCHANGE) => {
            options.scaling_filter = combo_selection(page, control);
        }
        (IDC_IMAGE_SCALEFACTOR_EDIT, EN_CHANGE) => {
            let value = unsafe { GetDlgItemInt(page, control, None, false) };
            options.scale_factor_percent = value.clamp(1, 500);
        }
        (IDC_IMAGE_CURSOR_ZOOM, BN_CLICKED) => {
            options.cursor_zoom = is_checked(page, control);
        }
        (IDC_IMAGE_FRACTIONAL_ZOOM, BN_CLICKED) => {
            options.fractional_zoom = is_checked(page, control);
        }
        // ── Miscellaneous ──
        (IDC_MISC_SORT, CBN_SELCHANGE) => options.sort_mode = combo_selection(page, control),
        (IDC_MISC_ASCENDING, BN_CLICKED) => options.sort_descending = false,
        (IDC_MISC_DESCENDING, BN_CLICKED) => options.sort_descending = true,
        (IDC_MISC_PRELOADING, CBN_SELCHANGE) => {
            options.preloading_mode = combo_selection(page, control);
        }
        (IDC_MISC_LOOP_FOLDERS, BN_CLICKED) => {
            options.loop_folders_enabled = is_checked(page, control);
        }
        (IDC_MISC_SLIDESHOW_DIRECTION, CBN_SELCHANGE) => {
            options.slideshow_reversed = combo_selection(page, control) == 1;
        }
        (IDC_MISC_SLIDESHOW_TIMER_EDIT, EN_CHANGE) => {
            if let Ok(seconds) = dialog_item_text(page, control).trim().parse::<f64>() {
                options.slideshow_timer_seconds = seconds.max(0.1);
            }
        }
        (IDC_MISC_AFTER_DELETE, CBN_SELCHANGE) => {
            options.after_delete = combo_selection(page, control);
        }
        (IDC_MISC_ASK_DELETE, BN_CLICKED) => options.ask_delete = is_checked(page, control),
        (IDC_MISC_MIME_DETECTION, BN_CLICKED) => {
            options.allow_mime_content_detection = is_checked(page, control);
        }
        (IDC_MISC_SAVE_RECENTS, BN_CLICKED) => options.save_recents = is_checked(page, control),
        (IDC_MISC_SKIP_HIDDEN, BN_CLICKED) => options.skip_hidden = is_checked(page, control),
        // ── Shortcuts ──
        (IDC_SHORTCUTS_RESET, BN_CLICKED) => {
            state.transient_shortcuts = default_shortcut_rows();
            refresh_shortcut_rows(state);
        }
        (IDC_SHORTCUTS_CLEAR_ALL, BN_CLICKED) => {
            for row in &mut state.transient_shortcuts {
                row.keyboard.clear();
                row.mouse.clear();
            }
            refresh_shortcut_rows(state);
        }
        // ── File Association ──
        (IDC_ASSOC_SELECT_ALL, BN_CLICKED) => set_all_associations(state, true),
        (IDC_ASSOC_SELECT_NONE, BN_CLICKED) => set_all_associations(state, false),
        _ => handled = false,
    }
    if handled {
        update_buttons(state);
        1
    } else {
        0
    }
}

// ── Window·Image·Miscellaneous 페이지 ───────────────────────────────────────

fn initialize_window_page(state: &OptionsState) {
    let page = state.pages[0];
    combo_fill(page, IDC_WINDOW_FITMODE, &["Width", "Height"]);
}

fn initialize_image_page(state: &OptionsState) {
    let page = state.pages[1];
    combo_fill(
        page,
        IDC_IMAGE_FILTERING,
        &["Nearest", "Bilinear", "Bicubic", "High Quality"],
    );
    if let Ok(spin) = unsafe { GetDlgItem(Some(page), IDC_IMAGE_SCALEFACTOR_SPIN) } {
        unsafe { SendMessageW(spin, UDM_SETRANGE32, Some(WPARAM(1)), Some(LPARAM(500))) };
    }
}

fn initialize_misc_page(state: &OptionsState) {
    let page = state.pages[2];
    combo_fill(
        page,
        IDC_MISC_SORT,
        &["Name", "Date Modified", "Date Created", "Size", "Type"],
    );
    combo_fill(
        page,
        IDC_MISC_PRELOADING,
        &["Disabled", "Adjacent", "Extended"],
    );
    combo_fill(page, IDC_MISC_SLIDESHOW_DIRECTION, &["Forward", "Backward"]);
    combo_fill(
        page,
        IDC_MISC_AFTER_DELETE,
        &["Move Back", "Do Nothing", "Move Forward"],
    );
}

/// transient → 전 컨트롤 반영 (초기화·Restore Defaults)
fn sync_all_pages(state: &mut OptionsState) {
    state.syncing = true;
    let options = state.transient_options.clone();
    let window_page = state.pages[0];
    set_check(
        window_page,
        IDC_WINDOW_BGCOLOR_ENABLED,
        options.background_color_enabled,
    );
    let _ = unsafe {
        CheckRadioButton(
            window_page,
            IDC_WINDOW_TITLEBAR_BASIC,
            IDC_WINDOW_TITLEBAR_PRACTICAL,
            IDC_WINDOW_TITLEBAR_BASIC + options.title_bar_mode.min(2) as i32,
        )
    };
    combo_select(window_page, IDC_WINDOW_FITMODE, options.fit_mode);
    set_check(
        window_page,
        IDC_WINDOW_SAVE_POSITION,
        options.save_window_position,
    );
    set_check(
        window_page,
        IDC_WINDOW_CTRL_DRAG,
        options.control_drag_window,
    );
    sync_bgcolor_button(state, window_page);

    let image_page = state.pages[1];
    combo_select(image_page, IDC_IMAGE_FILTERING, options.scaling_filter);
    set_dialog_item_text(
        image_page,
        IDC_IMAGE_SCALEFACTOR_EDIT,
        &options.scale_factor_percent.to_string(),
    );
    set_check(image_page, IDC_IMAGE_CURSOR_ZOOM, options.cursor_zoom);
    set_check(
        image_page,
        IDC_IMAGE_FRACTIONAL_ZOOM,
        options.fractional_zoom,
    );

    let misc_page = state.pages[2];
    combo_select(misc_page, IDC_MISC_SORT, options.sort_mode);
    let _ = unsafe {
        CheckRadioButton(
            misc_page,
            IDC_MISC_ASCENDING,
            IDC_MISC_DESCENDING,
            if options.sort_descending {
                IDC_MISC_DESCENDING
            } else {
                IDC_MISC_ASCENDING
            },
        )
    };
    combo_select(misc_page, IDC_MISC_PRELOADING, options.preloading_mode);
    set_check(
        misc_page,
        IDC_MISC_LOOP_FOLDERS,
        options.loop_folders_enabled,
    );
    combo_select(
        misc_page,
        IDC_MISC_SLIDESHOW_DIRECTION,
        u32::from(options.slideshow_reversed),
    );
    set_dialog_item_text(
        misc_page,
        IDC_MISC_SLIDESHOW_TIMER_EDIT,
        &format_seconds(options.slideshow_timer_seconds),
    );
    combo_select(misc_page, IDC_MISC_AFTER_DELETE, options.after_delete);
    set_check(misc_page, IDC_MISC_ASK_DELETE, options.ask_delete);
    set_check(
        misc_page,
        IDC_MISC_MIME_DETECTION,
        options.allow_mime_content_detection,
    );
    set_check(misc_page, IDC_MISC_SAVE_RECENTS, options.save_recents);
    set_check(misc_page, IDC_MISC_SKIP_HIDDEN, options.skip_hidden);

    state.syncing = false;
    refresh_shortcut_rows(state);
}

/// 배경색 버튼 — 체크 연동 활성화 + 색 견본 다시 그리기
fn sync_bgcolor_button(state: &OptionsState, page: HWND) {
    if let Ok(button) = unsafe { GetDlgItem(Some(page), IDC_WINDOW_BGCOLOR_BUTTON) } {
        let _ = unsafe { EnableWindow(button, state.transient_options.background_color_enabled) };
        let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(button), None, true) };
    }
}

fn choose_background_color(state: &mut OptionsState, page: HWND) {
    let (red, green, blue) = state.transient_options.background_color;
    let mut configuration = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: state.dialog,
        rgbResult: COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16)),
        lpCustColors: state.custom_colors.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if unsafe { ChooseColorW(&mut configuration) }.as_bool() {
        let chosen = configuration.rgbResult.0;
        state.transient_options.background_color = (
            (chosen & 0xFF) as u8,
            ((chosen >> 8) & 0xFF) as u8,
            ((chosen >> 16) & 0xFF) as u8,
        );
        sync_bgcolor_button(state, page);
    }
}

fn format_seconds(seconds: f64) -> String {
    if seconds.fract() == 0.0 {
        format!("{}", seconds as u64)
    } else {
        format!("{seconds}")
    }
}

// ── Shortcuts 페이지 ────────────────────────────────────────────────────────

fn initialize_shortcuts_page(state: &OptionsState) {
    let page = state.pages[3];
    let Ok(list) = (unsafe { GetDlgItem(Some(page), IDC_SHORTCUTS_LIST) }) else {
        return;
    };
    unsafe {
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),
            Some(LPARAM(LVS_EX_FULLROWSELECT as isize)),
        )
    };
    for (index, (title, width)) in [("Action", 160), ("Keyboard", 140), ("Mouse", 110)]
        .iter()
        .enumerate()
    {
        let text = wide(title);
        let column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            cx: *width,
            pszText: windows::core::PWSTR(text.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index)),
                Some(LPARAM(&raw const column as isize)),
            )
        };
    }
    for (index, row) in state.transient_shortcuts.iter().enumerate() {
        let label = wide(row.action.label());
        let item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: index as i32,
            pszText: windows::core::PWSTR(label.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTITEMW,
                None,
                Some(LPARAM(&raw const item as isize)),
            )
        };
    }
}

/// transient 바인딩 → Keyboard·Mouse 컬럼 텍스트 갱신
fn refresh_shortcut_rows(state: &OptionsState) {
    let Ok(list) = (unsafe { GetDlgItem(Some(state.pages[3]), IDC_SHORTCUTS_LIST) }) else {
        return;
    };
    for (index, row) in state.transient_shortcuts.iter().enumerate() {
        for (subitem, text) in [(1, row.keyboard.join(", ")), (2, row.mouse.join(", "))] {
            let wide_text = wide(&text);
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iSubItem: subitem,
                pszText: windows::core::PWSTR(wide_text.as_ptr().cast_mut()),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETITEMTEXTW,
                    Some(WPARAM(index)),
                    Some(LPARAM(&raw const item as isize)),
                )
            };
        }
    }
}

/// 더블클릭 편집 — Mouse 컬럼이면 마우스 캡처, 그 외 키 캡처 (SPEC §8.3)
fn edit_shortcut(state: &mut OptionsState, row_index: usize, mouse_column: bool) {
    if row_index >= state.transient_shortcuts.len() {
        return;
    }
    let taken: shortcut_capture::TakenBindings = state
        .transient_shortcuts
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != row_index)
        .flat_map(|(_, row)| {
            let encodings = if mouse_column {
                &row.mouse
            } else {
                &row.keyboard
            };
            encodings
                .iter()
                .map(|encoding| (encoding.clone(), row.action.label()))
        })
        .collect();
    let row = &state.transient_shortcuts[row_index];
    let updated = if mouse_column {
        shortcut_capture::capture_mouse_binding(
            state.dialog,
            row.mouse.first().map(String::as_str),
            taken,
        )
    } else {
        shortcut_capture::capture_keyboard_sequences(state.dialog, &row.keyboard, taken)
    };
    if let Some(encodings) = updated {
        let row = &mut state.transient_shortcuts[row_index];
        if mouse_column {
            row.mouse = encodings;
        } else {
            row.keyboard = encodings;
        }
        refresh_shortcut_rows(state);
        update_buttons(state);
    }
}

// ── File Association 페이지 ─────────────────────────────────────────────────

fn initialize_association_page(state: &mut OptionsState) {
    let page = state.pages[4];
    let Ok(tree) = (unsafe { GetDlgItem(Some(page), IDC_ASSOC_TREE) }) else {
        return;
    };
    state.state_images = create_tristate_images();
    unsafe {
        SendMessageW(
            tree,
            TVM_SETIMAGELIST,
            Some(WPARAM(TVSIL_STATE as usize)),
            Some(LPARAM(state.state_images.0)),
        )
    };

    // 디코더 레지스트리 → 알파벳 정렬, 다확장자 포맷만 그룹 헤더 (SPEC §8.3)
    let mut formats: Vec<(&'static str, &'static [&'static str])> =
        decode::format_groups().collect();
    formats.sort_by_key(|(name, _)| *name);
    for (name, extension_list) in formats {
        if extension_list.len() == 1 {
            let extension = format!(".{}", extension_list[0]);
            let checked = state.saved_associations.contains(&extension);
            let label = format!("{name} ({extension})");
            let item = tree_insert(
                tree,
                TVI_ROOT,
                &label,
                state.extensions.len() as isize,
                if checked {
                    STATE_CHECKED
                } else {
                    STATE_UNCHECKED
                },
            );
            state.extensions.push(AssociationExtension {
                extension,
                checked,
                item,
            });
        } else {
            let group_index = state.groups.len();
            let header = tree_insert(
                tree,
                TVI_ROOT,
                name,
                GROUP_FLAG | group_index as isize,
                STATE_UNCHECKED,
            );
            let mut members = Vec::new();
            for extension_name in extension_list {
                let extension = format!(".{extension_name}");
                let checked = state.saved_associations.contains(&extension);
                let item = tree_insert(
                    tree,
                    header,
                    &extension,
                    state.extensions.len() as isize,
                    if checked {
                        STATE_CHECKED
                    } else {
                        STATE_UNCHECKED
                    },
                );
                members.push(state.extensions.len());
                state.extensions.push(AssociationExtension {
                    extension,
                    checked,
                    item,
                });
            }
            state.groups.push(AssociationGroup {
                item: header,
                members,
            });
        }
    }
    for group_index in 0..state.groups.len() {
        refresh_group_state(state, tree, group_index);
    }
}

/// 16×16 체크박스 3종(미확인은 인덱스 0 자리 채움) — DrawFrameControl
fn create_tristate_images() -> HIMAGELIST {
    let images = unsafe { ImageList_Create(16, 16, ILC_COLOR32 | ILC_MASK, 4, 0) };
    let screen = unsafe { GetDC(None) };
    for style in [
        DFCS_BUTTONCHECK, // 0: 자리 채움 (상태 인덱스 0 = 이미지 없음)
        DFCS_BUTTONCHECK,
        DFCS_BUTTONCHECK | DFCS_CHECKED,
        DFCS_BUTTON3STATE | DFCS_CHECKED,
    ] {
        unsafe {
            let memory = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 16, 16);
            let previous = SelectObject(memory, bitmap.into());
            let mut bounds = RECT {
                left: 1,
                top: 1,
                right: 15,
                bottom: 15,
            };
            FillRect(
                memory,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 16,
                    bottom: 16,
                },
                GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_WINDOW),
            );
            let _ = DrawFrameControl(memory, &mut bounds, DFC_BUTTON, style);
            SelectObject(memory, previous);
            ImageList_Add(images, bitmap, None);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
        }
    }
    unsafe { ReleaseDC(None, screen) };
    images
}

fn tree_insert(
    tree: HWND,
    parent: HTREEITEM,
    text: &str,
    parameter: isize,
    state_image: isize,
) -> HTREEITEM {
    let label = wide(text);
    let insert = TVINSERTSTRUCTW {
        hParent: parent,
        hInsertAfter: TVI_LAST,
        Anonymous: windows::Win32::UI::Controls::TVINSERTSTRUCTW_0 {
            itemex: TVITEMEXW {
                mask: TVIF_TEXT | TVIF_PARAM | TVIF_STATE,
                pszText: windows::core::PWSTR(label.as_ptr().cast_mut()),
                lParam: LPARAM(parameter),
                state: (state_image as u32) << 12,
                stateMask: TVIS_STATEIMAGEMASK.0,
                ..Default::default()
            },
        },
    };
    let item = unsafe {
        SendMessageW(
            tree,
            TVM_INSERTITEMW,
            None,
            Some(LPARAM(&raw const insert as isize)),
        )
    };
    HTREEITEM(item.0)
}

fn tree_set_state_image(tree: HWND, item: HTREEITEM, state_image: isize) {
    let update = TVITEMEXW {
        mask: TVIF_STATE,
        hItem: item,
        state: (state_image as u32) << 12,
        stateMask: TVIS_STATEIMAGEMASK.0,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            tree,
            TVM_SETITEMW,
            None,
            Some(LPARAM(&raw const update as isize)),
        )
    };
}

/// 상태 아이콘 클릭 위치의 항목 토글 (NM_CLICK)
fn toggle_association_at_cursor(state: &mut OptionsState, tree: HWND) {
    let position = unsafe { GetMessagePos() };
    let mut hit = TVHITTESTINFO {
        pt: POINT {
            x: (position & 0xFFFF) as i16 as i32,
            y: (position >> 16) as i16 as i32,
        },
        ..Default::default()
    };
    let mut corner = [hit.pt];
    unsafe { windows::Win32::Graphics::Gdi::MapWindowPoints(None, Some(tree), &mut corner) };
    hit.pt = corner[0];
    let item =
        unsafe { SendMessageW(tree, TVM_HITTEST, None, Some(LPARAM(&raw mut hit as isize))) };
    if item.0 != 0 && hit.flags & TVHT_ONITEMSTATEICON != Default::default() {
        toggle_association_item(state, tree, HTREEITEM(item.0));
    }
}

fn toggle_association_item(state: &mut OptionsState, tree: HWND, item: HTREEITEM) {
    let mut query = TVITEMEXW {
        mask: TVIF_PARAM,
        hItem: item,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            tree,
            TVM_GETITEMW,
            None,
            Some(LPARAM(&raw mut query as isize)),
        )
    };
    let parameter = query.lParam.0;
    if parameter & GROUP_FLAG != 0 {
        // 그룹 헤더 — 전체 체크 상태면 해제, 아니면 일괄 체크 (tri-state 하위 일괄 토글)
        let group_index = (parameter & !GROUP_FLAG) as usize;
        let Some(group) = state.groups.get(group_index) else {
            return;
        };
        let members = group.members.clone();
        let all_checked = members
            .iter()
            .all(|member| state.extensions[*member].checked);
        for member in &members {
            let entry = &mut state.extensions[*member];
            entry.checked = !all_checked;
            tree_set_state_image(
                tree,
                entry.item,
                if entry.checked {
                    STATE_CHECKED
                } else {
                    STATE_UNCHECKED
                },
            );
        }
        refresh_group_state(state, tree, group_index);
    } else {
        let extension_index = parameter as usize;
        let Some(entry) = state.extensions.get_mut(extension_index) else {
            return;
        };
        entry.checked = !entry.checked;
        tree_set_state_image(
            tree,
            entry.item,
            if entry.checked {
                STATE_CHECKED
            } else {
                STATE_UNCHECKED
            },
        );
        if let Some(group_index) = state
            .groups
            .iter()
            .position(|group| group.members.contains(&extension_index))
        {
            refresh_group_state(state, tree, group_index);
        }
    }
    update_buttons(state);
}

/// 그룹 헤더 tri-state 재계산 — 전체/일부/없음
fn refresh_group_state(state: &OptionsState, tree: HWND, group_index: usize) {
    let group = &state.groups[group_index];
    let checked_count = group
        .members
        .iter()
        .filter(|member| state.extensions[**member].checked)
        .count();
    let image = if checked_count == 0 {
        STATE_UNCHECKED
    } else if checked_count == group.members.len() {
        STATE_CHECKED
    } else {
        STATE_PARTIAL
    };
    tree_set_state_image(tree, group.item, image);
}

fn set_all_associations(state: &mut OptionsState, checked: bool) {
    let Ok(tree) = (unsafe { GetDlgItem(Some(state.pages[4]), IDC_ASSOC_TREE) }) else {
        return;
    };
    for entry in &mut state.extensions {
        entry.checked = checked;
        tree_set_state_image(
            tree,
            entry.item,
            if checked {
                STATE_CHECKED
            } else {
                STATE_UNCHECKED
            },
        );
    }
    for group_index in 0..state.groups.len() {
        refresh_group_state(state, tree, group_index);
    }
}

// ── 컨트롤 헬퍼 ─────────────────────────────────────────────────────────────

fn combo_fill(page: HWND, control: i32, entries: &[&str]) {
    let Ok(combo) = (unsafe { GetDlgItem(Some(page), control) }) else {
        return;
    };
    for entry in entries {
        let text = wide(entry);
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(text.as_ptr() as isize)),
            )
        };
    }
}

fn combo_select(page: HWND, control: i32, index: u32) {
    if let Ok(combo) = unsafe { GetDlgItem(Some(page), control) } {
        unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index as usize)), None) };
    }
}

fn combo_selection(page: HWND, control: i32) -> u32 {
    unsafe { GetDlgItem(Some(page), control) }
        .map(|combo| unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0.max(0) as u32 })
        .unwrap_or(0)
}

fn set_check(page: HWND, control: i32, checked: bool) {
    let _ = unsafe {
        CheckDlgButton(
            page,
            control,
            if checked { BST_CHECKED } else { BST_UNCHECKED },
        )
    };
}

fn is_checked(page: HWND, control: i32) -> bool {
    unsafe { IsDlgButtonChecked(page, control) == BST_CHECKED.0 }
}

fn set_dialog_item_text(page: HWND, control: i32, text: &str) {
    let wide_text = wide(text);
    let _ = unsafe { SetDlgItemTextW(page, control, PCWSTR(wide_text.as_ptr())) };
}

fn dialog_item_text(page: HWND, control: i32) -> String {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetDlgItemTextW(page, control, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length as usize])
}
