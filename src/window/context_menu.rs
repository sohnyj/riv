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
    pub slideshow_active: bool,
    /// 최근 파일 표시명 (부재 감사 완료 목록 — SPEC §6.4)
    pub recent_names: Vec<String>,
}

struct MenuBuilder {
    /// 명령 ID = actions 인덱스 + 1 (0 = 선택 없음/취소)
    actions: Vec<Action>,
    state_snapshot: MenuState,
}

impl MenuBuilder {
    /// 아직 배선되지 않은 액션(R4 잔여 Open With·R5 애니·R6 다이얼로그·R7 멀티윈도우)
    /// — 항상 비활성 표시
    fn is_wired(action: Action) -> bool {
        !matches!(
            action,
            Action::OpenWith
                | Action::OpenWithOther
                | Action::NewWindow
                | Action::CloseAllWindows
                | Action::Pause
                | Action::NextFrame
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
        let clear_without_recents =
            action == Action::ClearRecents && self.state_snapshot.recent_names.is_empty();
        if !Self::is_wired(action) || !self.gate_satisfied(action.gate()) || clear_without_recents {
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

        // Open Recent — 최대 10개 + Clear Menu (SPEC §6.4, 아이콘 없음 — R10)
        let recent = unsafe { CreatePopupMenu()? };
        for index in 0..self.state_snapshot.recent_names.len().min(10) {
            let name = self.state_snapshot.recent_names[index].clone();
            self.append_action_labeled(recent, Action::Recent(index as u8), &name)?;
        }
        if !self.state_snapshot.recent_names.is_empty() {
            self.append_separator(recent)?;
        }
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
        let slideshow_label = if self.state_snapshot.slideshow_active {
            "Stop Slideshow"
        } else {
            "Start Slideshow"
        };
        self.append_action_labeled(tools, Action::Slideshow, slideshow_label)?;
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
