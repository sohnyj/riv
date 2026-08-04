//! Action definitions; every input path converges on one dispatcher.

/// Enablement gate shared by menu items and the dispatcher.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActivationGate {
    Window,
    Image,
    /// Image whose backing file can take file operations (not an archive member).
    FileOnDisk,
    /// Image carried by some file on disk (the archive for members, never a URL).
    ContainingFile,
    Animation,
    /// Somewhere to go besides the anchor itself (single-entry folders stay inert).
    NavigationTargets,
}

/// Variant and table order track the context menu, flattened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    OpenUrl,
    PasteUrl,
    Recent(u8),
    ClearRecents,
    OtherApplication,
    Playlist,
    Loop,
    FirstFile,
    PreviousFile,
    NextFile,
    LastFile,
    Pause,
    PreviousFrame,
    NextFrame,
    DecreaseSpeed,
    IncreaseSpeed,
    ResetSpeed,
    ShowFileInfo,
    Reload,
    ToggleFitMode,
    PreserveZoom,
    ZoomIn,
    ZoomOut,
    ToggleZoom,
    PanUp,
    PanDown,
    PanLeft,
    PanRight,
    RotateLeft,
    RotateRight,
    Mirror,
    Flip,
    ShowInExplorer,
    Rename,
    Delete,
    DeletePermanently,
    ToggleSlideshow,
    Settings,
    AlwaysOnTop,
    Fullscreen,
    Exit,
}

/// (action, name, label, gate); the name is the binding and dispatch key.
const ACTION_TABLE: &[(Action, &str, &str, ActivationGate)] = &[
    (Action::Open, "open", "Open...", ActivationGate::Window),
    (
        Action::OpenUrl,
        "openurl",
        "Open URL...",
        ActivationGate::Window,
    ),
    (
        Action::PasteUrl,
        "pasteurl",
        "Paste URL",
        ActivationGate::Window,
    ),
    (
        Action::ClearRecents,
        "clearrecents",
        "Clear recents",
        ActivationGate::Window,
    ),
    (
        Action::OtherApplication,
        "otherapplication",
        "Other application...",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::Playlist,
        "playlist",
        "Playlist",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::Loop,
        "loop",
        "Loop",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::FirstFile,
        "firstfile",
        "First file",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::PreviousFile,
        "previousfile",
        "Previous",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::NextFile,
        "nextfile",
        "Next",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::LastFile,
        "lastfile",
        "Last file",
        ActivationGate::NavigationTargets,
    ),
    (Action::Pause, "pause", "Pause", ActivationGate::Animation),
    (
        Action::PreviousFrame,
        "previousframe",
        "Previous frame",
        ActivationGate::Animation,
    ),
    (
        Action::NextFrame,
        "nextframe",
        "Next frame",
        ActivationGate::Animation,
    ),
    (
        Action::DecreaseSpeed,
        "decreasespeed",
        "Decrease speed",
        ActivationGate::Animation,
    ),
    (
        Action::IncreaseSpeed,
        "increasespeed",
        "Increase speed",
        ActivationGate::Animation,
    ),
    (
        Action::ResetSpeed,
        "resetspeed",
        "Reset speed",
        ActivationGate::Animation,
    ),
    (
        Action::ShowFileInfo,
        "showfileinfo",
        "Show file info",
        ActivationGate::Image,
    ),
    (Action::Reload, "reload", "Reload", ActivationGate::Image),
    (
        Action::ToggleFitMode,
        "togglefitmode",
        "Toggle fit mode",
        ActivationGate::Image,
    ),
    (
        Action::PreserveZoom,
        "preservezoom",
        "Preserve zoom",
        ActivationGate::Image,
    ),
    (Action::ZoomIn, "zoomin", "Zoom in", ActivationGate::Image),
    (
        Action::ZoomOut,
        "zoomout",
        "Zoom out",
        ActivationGate::Image,
    ),
    (
        Action::ToggleZoom,
        "togglezoom",
        "Toggle zoom",
        ActivationGate::Image,
    ),
    (Action::PanUp, "panup", "Pan up", ActivationGate::Image),
    (
        Action::PanDown,
        "pandown",
        "Pan down",
        ActivationGate::Image,
    ),
    (
        Action::PanLeft,
        "panleft",
        "Pan left",
        ActivationGate::Image,
    ),
    (
        Action::PanRight,
        "panright",
        "Pan right",
        ActivationGate::Image,
    ),
    (
        Action::RotateLeft,
        "rotateleft",
        "Rotate left",
        ActivationGate::Image,
    ),
    (
        Action::RotateRight,
        "rotateright",
        "Rotate right",
        ActivationGate::Image,
    ),
    (Action::Mirror, "mirror", "Mirror", ActivationGate::Image),
    (Action::Flip, "flip", "Flip", ActivationGate::Image),
    (
        Action::ShowInExplorer,
        "showinexplorer",
        "Show in Explorer",
        ActivationGate::ContainingFile,
    ),
    (
        Action::Rename,
        "rename",
        "Rename...",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::Delete,
        "delete",
        "Delete",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::DeletePermanently,
        "deletepermanently",
        "Delete permanently",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::ToggleSlideshow,
        "toggleslideshow",
        "Toggle slideshow",
        ActivationGate::NavigationTargets,
    ),
    (
        Action::Settings,
        "settings",
        "Settings",
        ActivationGate::Window,
    ),
    (
        Action::AlwaysOnTop,
        "alwaysontop",
        "Always on top",
        ActivationGate::Window,
    ),
    (
        Action::Fullscreen,
        "fullscreen",
        "Enter fullscreen",
        ActivationGate::Window,
    ),
    (Action::Exit, "exit", "Exit", ActivationGate::Window),
];

const RECENT_NAMES: [&str; crate::settings::RECENT_FILES_LIMIT] = [
    "recent0", "recent1", "recent2", "recent3", "recent4", "recent5", "recent6", "recent7",
    "recent8", "recent9",
];

impl Action {
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(index) = RECENT_NAMES.iter().position(|recent| *recent == name) {
            return Some(Self::Recent(index as u8));
        }
        ACTION_TABLE
            .iter()
            .find(|(_, action_name, _, _)| *action_name == name)
            .map(|(action, _, _, _)| *action)
    }

    pub fn all_bindable() -> impl Iterator<Item = Self> {
        ACTION_TABLE.iter().map(|(action, _, _, _)| *action)
    }

    /// The table row; Recent is dynamic and answers before any caller reaches here.
    fn entry(self) -> &'static (Self, &'static str, &'static str, ActivationGate) {
        ACTION_TABLE
            .iter()
            .find(|(action, _, _, _)| *action == self)
            .expect("action in table")
    }

    pub fn name(self) -> &'static str {
        if let Self::Recent(index) = self {
            return RECENT_NAMES[usize::from(index).min(RECENT_NAMES.len() - 1)];
        }
        self.entry().1
    }

    pub fn label(self) -> &'static str {
        if matches!(self, Self::Recent(_)) {
            return "";
        }
        self.entry().2
    }

    pub fn gate(self) -> ActivationGate {
        if matches!(self, Self::Recent(_)) {
            return ActivationGate::Window;
        }
        self.entry().3
    }
}
