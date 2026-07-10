//! 컨텍스트 메뉴 (SPEC §6.1) — 유일한 메뉴 진입점. OS 기본 스타일 그대로(P14,
//! 아이콘 데코 없음 — Open With 앱 아이콘만 예외, R4). `TPM_RETURNCMD`로 선택
//! 액션을 반환하고 디스패치는 호출자(단일 디스패처)가 수행한다.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::{HSTRING, Result};

use crate::actions::{Action, ActivationGate};

/// 메뉴 enable·라벨 토글에 필요한 상태 스냅샷
pub struct MenuState {
    pub has_image: bool,
    pub has_folder: bool,
    pub has_animation: bool,
    pub preserve_zoom: bool,
    pub fullscreen: bool,
}

struct MenuBuilder {
    /// 명령 ID = actions 인덱스 + 1 (0 = 선택 없음/취소)
    actions: Vec<Action>,
    state_snapshot: MenuState,
}

impl MenuBuilder {
    /// 아직 배선되지 않은 액션(R4 셸 통합·R5 애니·R6 다이얼로그) — 항상 비활성 표시
    fn is_wired(action: Action) -> bool {
        !matches!(
            action,
            Action::Open
                | Action::OpenWith
                | Action::OpenWithOther
                | Action::OpenContainingFolder
                | Action::ShowFileInfo
                | Action::Rename
                | Action::Delete
                | Action::Copy
                | Action::Paste
                | Action::Recent(_)
                | Action::ClearRecents
                | Action::NewWindow
                | Action::CloseAllWindows
                | Action::Pause
                | Action::NextFrame
                | Action::Slideshow
                | Action::Options
        )
    }

    fn gate_satisfied(&self, gate: ActivationGate) -> bool {
        match gate {
            ActivationGate::Window => true,
            ActivationGate::Image => self.state_snapshot.has_image,
            ActivationGate::Animation => self.state_snapshot.has_animation,
            ActivationGate::Folder => self.state_snapshot.has_folder,
        }
    }

    fn append_action(&mut self, menu: HMENU, action: Action) -> Result<()> {
        self.append_action_labeled(menu, action, action.label())
    }

    fn append_action_labeled(&mut self, menu: HMENU, action: Action, label: &str) -> Result<()> {
        self.actions.push(action);
        let identifier = self.actions.len();
        let mut flags = MF_STRING;
        if !Self::is_wired(action) || !self.gate_satisfied(action.gate()) {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        if action == Action::PreserveZoom && self.state_snapshot.preserve_zoom {
            flags |= MF_CHECKED;
        }
        unsafe { AppendMenuW(menu, flags, identifier, &HSTRING::from(label)) }
    }

    fn append_separator(&self, menu: HMENU) -> Result<()> {
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }
    }

    fn append_submenu(&self, menu: HMENU, submenu: HMENU, label: &str) -> Result<()> {
        unsafe { AppendMenuW(menu, MF_POPUP, submenu.0 as usize, &HSTRING::from(label)) }
    }

    /// SPEC §6.1 메뉴 구조
    fn build(&mut self) -> Result<HMENU> {
        let menu = unsafe { CreatePopupMenu()? };
        self.append_action(menu, Action::Open)?;

        // Open Recent / Open With — 목록 채움은 R4 (빈 서브메뉴, 비활성 자리 표시)
        let recent = unsafe { CreatePopupMenu()? };
        self.append_action_labeled(recent, Action::ClearRecents, "Clear Menu")?;
        self.append_submenu(menu, recent, "Open Recent")?;
        self.append_action(menu, Action::ReloadFile)?;
        let open_with = unsafe { CreatePopupMenu()? };
        self.append_action_labeled(open_with, Action::OpenWithOther, "Other Application...")?;
        self.append_submenu(menu, open_with, "Open With")?;
        self.append_action(menu, Action::OpenContainingFolder)?;
        self.append_action(menu, Action::ShowFileInfo)?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::Rename)?;
        self.append_action(menu, Action::Delete)?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::FirstFile)?;
        self.append_action(menu, Action::PreviousFile)?;
        self.append_action(menu, Action::NextFile)?;
        self.append_action(menu, Action::LastFile)?;
        self.append_separator(menu)?;

        let view = unsafe { CreatePopupMenu()? };
        self.append_action(view, Action::ZoomIn)?;
        self.append_action(view, Action::ZoomOut)?;
        self.append_action(view, Action::ResetZoom)?;
        self.append_action(view, Action::PreserveZoom)?;
        self.append_separator(view)?;
        self.append_action(view, Action::RotateRight)?;
        self.append_action(view, Action::RotateLeft)?;
        self.append_separator(view)?;
        self.append_action(view, Action::Mirror)?;
        self.append_action(view, Action::Flip)?;
        self.append_submenu(menu, view, "View")?;

        let tools = unsafe { CreatePopupMenu()? };
        self.append_action(tools, Action::Pause)?;
        self.append_action(tools, Action::NextFrame)?;
        self.append_separator(tools)?;
        self.append_action(tools, Action::Slideshow)?;
        self.append_separator(tools)?;
        self.append_action(tools, Action::Options)?;
        self.append_submenu(menu, tools, "Tools")?;

        let fullscreen_label = if self.state_snapshot.fullscreen {
            "Exit Fullscreen"
        } else {
            "Enter Fullscreen"
        };
        self.append_action_labeled(menu, Action::Fullscreen, fullscreen_label)?;
        self.append_separator(menu)?;
        self.append_action(menu, Action::Quit)?;
        Ok(menu)
    }
}

/// 메뉴 표시 → 선택 액션 반환 (취소 시 None). (x, y) = 화면 좌표.
pub fn show(window: HWND, state: MenuState, x: i32, y: i32) -> Option<Action> {
    let mut builder = MenuBuilder {
        actions: Vec::new(),
        state_snapshot: state,
    };
    let menu = builder.build().ok()?;
    let selected = unsafe {
        TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            x,
            y,
            window,
            None,
        )
    };
    let _ = unsafe { DestroyMenu(menu) };
    let identifier = selected.0 as usize;
    (identifier > 0)
        .then(|| builder.actions.get(identifier - 1).copied())
        .flatten()
}
