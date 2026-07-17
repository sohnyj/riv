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
    Folder,
}

/// Variant and table order track the context menu, flattened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    OpenUrl,
    Recent(u8),
    ClearRecents,
    OpenWith,
    OpenWithOther,
    ShowFileInfo,
    OpenContainingFolder,
    FirstFile,
    PreviousFile,
    NextFile,
    LastFile,
    Pause,
    NextFrame,
    DecreaseSpeed,
    IncreaseSpeed,
    ResetSpeed,
    ReloadFile,
    Rename,
    Delete,
    DeletePermanent,
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
    Slideshow,
    Options,
    About,
    Fullscreen,
    Quit,
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
        Action::ClearRecents,
        "clearrecents",
        "Clear Recents",
        ActivationGate::Window,
    ),
    (
        Action::OpenWith,
        "openwith",
        "Open With",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::OpenWithOther,
        "openwithother",
        "Other Application...",
        ActivationGate::FileOnDisk,
    ),
    (
        Action::ShowFileInfo,
        "showfileinfo",
        "Show File Info",
        ActivationGate::Image,
    ),
    (
        Action::OpenContainingFolder,
        "opencontainingfolder",
        "Show in Explorer",
        ActivationGate::ContainingFile,
    ),
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
        Action::IncreaseSpeed,
        "increasespeed",
        "Increase Speed",
        ActivationGate::Animation,
    ),
    (
        Action::ResetSpeed,
        "resetspeed",
        "Reset Speed",
        ActivationGate::Animation,
    ),
    (
        Action::ReloadFile,
        "reloadfile",
        "Reload",
        ActivationGate::Image,
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
        Action::DeletePermanent,
        "deletepermanent",
        "Delete Permanently",
        ActivationGate::FileOnDisk,
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
        Action::Slideshow,
        "slideshow",
        "Toggle Slideshow",
        ActivationGate::Folder,
    ),
    (
        Action::Options,
        "options",
        "Settings...",
        ActivationGate::Window,
    ),
    (Action::About, "about", "About", ActivationGate::Window),
    (
        Action::Fullscreen,
        "fullscreen",
        "Enter Fullscreen",
        ActivationGate::Window,
    ),
    (Action::Quit, "quit", "Exit", ActivationGate::Window),
];

const RECENT_NAMES: [&str; 10] = [
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
        ACTION_TABLE
            .iter()
            .map(|(action, _, _, _)| *action)
            .filter(|action| !matches!(action, Self::OpenWith | Self::About))
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
