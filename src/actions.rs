//! 액션 정의·메타 (SPEC §5.1) — 메뉴·키보드·마우스 입력이 전부 액션 하나로
//! 수렴하고, 디스패치는 main의 단일 지점에서 분기한다 (§2 핵심 계약).

/// 활성화 게이트 (SPEC §5.1) — 메뉴 enable·디스패치 가드 공용
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActivationGate {
    /// 창만 필요 (windowdisable)
    Window,
    /// 이미지 로드 필요 (disable)
    Image,
    /// 애니메이션 필요 (gifdisable)
    Animation,
    /// 폴더 목록 필요 (folderdisable)
    Folder,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    // 파일
    Open,
    OpenWith,
    OpenWithOther,
    OpenContainingFolder,
    ReloadFile,
    ShowFileInfo,
    Rename,
    Delete,
    DeletePermanent,
    Recent(u8),
    ClearRecents,
    Quit,
    // 탐색
    FirstFile,
    PreviousFile,
    NextFile,
    LastFile,
    // 보기
    ZoomIn,
    ZoomOut,
    ResetZoom,
    PreserveZoom,
    PanUp,
    PanDown,
    PanLeft,
    PanRight,
    RotateRight,
    RotateLeft,
    Mirror,
    Flip,
    Fullscreen,
    // 애니메이션
    Pause,
    NextFrame,
    DecreaseSpeed,
    ResetSpeed,
    IncreaseSpeed,
    // 기타
    Slideshow,
    Options,
    About,
}

/// (액션, 이름, 라벨, 게이트) — 이름 = 바인딩·디스패치 키 (SPEC §5.1).
/// recent0..9는 이름이 동적이라 표 밖에서 처리.
const ACTION_TABLE: &[(Action, &str, &str, ActivationGate)] = &[
    (Action::Open, "open", "Open...", ActivationGate::Window),
    (
        Action::OpenWith,
        "openwith",
        "Open With",
        ActivationGate::Image,
    ),
    (
        Action::OpenWithOther,
        "openwithother",
        "Other Application...",
        ActivationGate::Image,
    ),
    (
        Action::OpenContainingFolder,
        "opencontainingfolder",
        "Show in Explorer",
        ActivationGate::Image,
    ),
    (
        Action::ReloadFile,
        "reloadfile",
        "Reload File",
        ActivationGate::Image,
    ),
    (
        Action::ShowFileInfo,
        "showfileinfo",
        "Show File Info",
        ActivationGate::Image,
    ),
    (Action::Rename, "rename", "Rename...", ActivationGate::Image),
    (Action::Delete, "delete", "Delete", ActivationGate::Image),
    (
        Action::DeletePermanent,
        "deletepermanent",
        "Delete Permanently",
        ActivationGate::Image,
    ),
    (
        Action::ClearRecents,
        "clearrecents",
        "Clear Menu",
        ActivationGate::Window,
    ),
    (Action::Quit, "quit", "Exit", ActivationGate::Window),
    (
        Action::FirstFile,
        "firstfile",
        "First File",
        ActivationGate::Folder,
    ),
    (
        Action::PreviousFile,
        "previousfile",
        "Previous",
        ActivationGate::Folder,
    ),
    (Action::NextFile, "nextfile", "Next", ActivationGate::Folder),
    (
        Action::LastFile,
        "lastfile",
        "Last File",
        ActivationGate::Folder,
    ),
    (Action::ZoomIn, "zoomin", "Zoom In", ActivationGate::Image),
    (
        Action::ZoomOut,
        "zoomout",
        "Zoom Out",
        ActivationGate::Image,
    ),
    (
        Action::ResetZoom,
        "resetzoom",
        "Toggle Zoom",
        ActivationGate::Image,
    ),
    (
        Action::PreserveZoom,
        "preservezoom",
        "Preserve Zoom",
        ActivationGate::Image,
    ),
    (Action::PanUp, "panup", "Pan Up", ActivationGate::Image),
    (
        Action::PanDown,
        "pandown",
        "Pan Down",
        ActivationGate::Image,
    ),
    (
        Action::PanLeft,
        "panleft",
        "Pan Left",
        ActivationGate::Image,
    ),
    (
        Action::PanRight,
        "panright",
        "Pan Right",
        ActivationGate::Image,
    ),
    (
        Action::RotateRight,
        "rotateright",
        "Rotate Right",
        ActivationGate::Image,
    ),
    (
        Action::RotateLeft,
        "rotateleft",
        "Rotate Left",
        ActivationGate::Image,
    ),
    (Action::Mirror, "mirror", "Mirror", ActivationGate::Image),
    (Action::Flip, "flip", "Flip", ActivationGate::Image),
    (
        Action::Fullscreen,
        "fullscreen",
        "Enter Fullscreen",
        ActivationGate::Window,
    ),
    (Action::Pause, "pause", "Pause", ActivationGate::Animation),
    (
        Action::NextFrame,
        "nextframe",
        "Next Frame",
        ActivationGate::Animation,
    ),
    (
        Action::DecreaseSpeed,
        "decreasespeed",
        "Decrease Speed",
        ActivationGate::Animation,
    ),
    (
        Action::ResetSpeed,
        "resetspeed",
        "Reset Speed",
        ActivationGate::Animation,
    ),
    (
        Action::IncreaseSpeed,
        "increasespeed",
        "Increase Speed",
        ActivationGate::Animation,
    ),
    (
        Action::Slideshow,
        "slideshow",
        "Start Slideshow",
        ActivationGate::Folder,
    ),
    (
        Action::Options,
        "options",
        "Settings...",
        ActivationGate::Window,
    ),
    (Action::About, "about", "About", ActivationGate::Window),
];

const RECENT_NAMES: [&str; 10] = [
    "recent0", "recent1", "recent2", "recent3", "recent4", "recent5", "recent6", "recent7",
    "recent8", "recent9",
];

impl Action {
    /// 바인딩·디스패치 키 → 액션 (SPEC §5.1)
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(index) = RECENT_NAMES.iter().position(|recent| *recent == name) {
            return Some(Self::Recent(index as u8));
        }
        ACTION_TABLE
            .iter()
            .find(|(_, action_name, _, _)| *action_name == name)
            .map(|(action, _, _, _)| *action)
    }

    /// 단축키 편집 테이블의 행 목록 (SPEC §8.3) — Recent(동적 이름)와 OpenWith
    /// (서브메뉴 컨테이너 — 디스패치 무동작이라 바인딩 무의미)는 제외
    pub fn all_bindable() -> impl Iterator<Item = Self> {
        ACTION_TABLE
            .iter()
            .map(|(action, _, _, _)| *action)
            .filter(|action| *action != Self::OpenWith)
    }

    pub fn name(self) -> &'static str {
        if let Self::Recent(index) = self {
            return RECENT_NAMES[usize::from(index).min(9)];
        }
        ACTION_TABLE
            .iter()
            .find(|(action, _, _, _)| *action == self)
            .map(|(_, name, _, _)| *name)
            .expect("action in table")
    }

    /// 메뉴 라벨 (상태 토글 라벨은 메뉴 구성부에서 처리)
    pub fn label(self) -> &'static str {
        if matches!(self, Self::Recent(_)) {
            return "";
        }
        ACTION_TABLE
            .iter()
            .find(|(action, _, _, _)| *action == self)
            .map(|(_, _, label, _)| *label)
            .expect("action in table")
    }

    pub fn gate(self) -> ActivationGate {
        if matches!(self, Self::Recent(_)) {
            return ActivationGate::Window;
        }
        ACTION_TABLE
            .iter()
            .find(|(action, _, _, _)| *action == self)
            .map(|(_, _, _, gate)| *gate)
            .expect("action in table")
    }
}
