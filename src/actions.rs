//! Action definitions; every input path converges on one dispatcher.

/// What an action needs before it can act; the menu and the dispatcher share it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActionRequirement {
    Window,
    Image,
    /// Image whose backing file can take file operations (not an archive member).
    FileOnDisk,
    /// Image carried by some file on disk (the archive for members, never a URL).
    ContainingFile,
    Animation,
    /// Somewhere to go besides the anchor itself (single-entry folders stay inert).
    NavigationTargets,
    /// At least one recent file, so there is something to clear.
    RecentFiles,
}

impl ActionRequirement {
    /// Declaration order is the index a snapshot stores each answer at.
    pub const ALL: [Self; 7] = [
        Self::Window,
        Self::Image,
        Self::FileOnDisk,
        Self::ContainingFile,
        Self::Animation,
        Self::NavigationTargets,
        Self::RecentFiles,
    ];
}

/// The requirements a menu found satisfied as it opened; the builder reads this, never live state.
#[derive(Clone, Copy, Default)]
pub struct SatisfiedRequirements([bool; ActionRequirement::ALL.len()]);

impl SatisfiedRequirements {
    /// Answers every requirement once, at the moment the snapshot is taken.
    pub fn evaluate(satisfied: impl Fn(ActionRequirement) -> bool) -> Self {
        Self(ActionRequirement::ALL.map(satisfied))
    }

    pub fn satisfied(self, requirement: ActionRequirement) -> bool {
        self.0[requirement as usize]
    }

    #[cfg(test)]
    pub fn set(&mut self, requirement: ActionRequirement, satisfied: bool) {
        self.0[requirement as usize] = satisfied;
    }
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

/// (action, name, label, requirement); the name is the binding and dispatch key.
const ACTION_TABLE: &[(Action, &str, &str, ActionRequirement)] = &[
    (Action::Open, "open", "Open...", ActionRequirement::Window),
    (
        Action::OpenUrl,
        "openurl",
        "Open URL...",
        ActionRequirement::Window,
    ),
    (
        Action::PasteUrl,
        "pasteurl",
        "Paste URL",
        ActionRequirement::Window,
    ),
    (
        Action::ClearRecents,
        "clearrecents",
        "Clear recents",
        ActionRequirement::RecentFiles,
    ),
    (
        Action::OtherApplication,
        "otherapplication",
        "Other application...",
        ActionRequirement::FileOnDisk,
    ),
    (
        Action::Playlist,
        "playlist",
        "Playlist",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::Loop,
        "loop",
        "Loop",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::FirstFile,
        "firstfile",
        "First file",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::PreviousFile,
        "previousfile",
        "Previous",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::NextFile,
        "nextfile",
        "Next",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::LastFile,
        "lastfile",
        "Last file",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::Pause,
        "pause",
        "Pause",
        ActionRequirement::Animation,
    ),
    (
        Action::PreviousFrame,
        "previousframe",
        "Previous frame",
        ActionRequirement::Animation,
    ),
    (
        Action::NextFrame,
        "nextframe",
        "Next frame",
        ActionRequirement::Animation,
    ),
    (
        Action::DecreaseSpeed,
        "decreasespeed",
        "Decrease speed",
        ActionRequirement::Animation,
    ),
    (
        Action::IncreaseSpeed,
        "increasespeed",
        "Increase speed",
        ActionRequirement::Animation,
    ),
    (
        Action::ResetSpeed,
        "resetspeed",
        "Reset speed",
        ActionRequirement::Animation,
    ),
    (
        Action::ShowFileInfo,
        "showfileinfo",
        "Show file info",
        ActionRequirement::Image,
    ),
    (Action::Reload, "reload", "Reload", ActionRequirement::Image),
    (
        Action::ToggleFitMode,
        "togglefitmode",
        "Toggle fit mode",
        ActionRequirement::Image,
    ),
    (
        Action::PreserveZoom,
        "preservezoom",
        "Preserve zoom",
        ActionRequirement::Image,
    ),
    (
        Action::ZoomIn,
        "zoomin",
        "Zoom in",
        ActionRequirement::Image,
    ),
    (
        Action::ZoomOut,
        "zoomout",
        "Zoom out",
        ActionRequirement::Image,
    ),
    (
        Action::ToggleZoom,
        "togglezoom",
        "Toggle zoom",
        ActionRequirement::Image,
    ),
    (Action::PanUp, "panup", "Pan up", ActionRequirement::Image),
    (
        Action::PanDown,
        "pandown",
        "Pan down",
        ActionRequirement::Image,
    ),
    (
        Action::PanLeft,
        "panleft",
        "Pan left",
        ActionRequirement::Image,
    ),
    (
        Action::PanRight,
        "panright",
        "Pan right",
        ActionRequirement::Image,
    ),
    (
        Action::RotateLeft,
        "rotateleft",
        "Rotate left",
        ActionRequirement::Image,
    ),
    (
        Action::RotateRight,
        "rotateright",
        "Rotate right",
        ActionRequirement::Image,
    ),
    (Action::Mirror, "mirror", "Mirror", ActionRequirement::Image),
    (Action::Flip, "flip", "Flip", ActionRequirement::Image),
    (
        Action::ShowInExplorer,
        "showinexplorer",
        "Show in Explorer",
        ActionRequirement::ContainingFile,
    ),
    (
        Action::Rename,
        "rename",
        "Rename...",
        ActionRequirement::FileOnDisk,
    ),
    (
        Action::Delete,
        "delete",
        "Delete",
        ActionRequirement::FileOnDisk,
    ),
    (
        Action::DeletePermanently,
        "deletepermanently",
        "Delete permanently",
        ActionRequirement::FileOnDisk,
    ),
    (
        Action::ToggleSlideshow,
        "toggleslideshow",
        "Toggle slideshow",
        ActionRequirement::NavigationTargets,
    ),
    (
        Action::Settings,
        "settings",
        "Settings",
        ActionRequirement::Window,
    ),
    (
        Action::AlwaysOnTop,
        "alwaysontop",
        "Always on top",
        ActionRequirement::Window,
    ),
    (
        Action::Fullscreen,
        "fullscreen",
        "Toggle fullscreen",
        ActionRequirement::Window,
    ),
    (Action::Exit, "exit", "Exit", ActionRequirement::Window),
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
    fn entry(self) -> &'static (Self, &'static str, &'static str, ActionRequirement) {
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

    pub fn requirement(self) -> ActionRequirement {
        if matches!(self, Self::Recent(_)) {
            return ActionRequirement::Window;
        }
        self.entry().3
    }

    /// Which way the four pan actions move the image; the caller sets how far.
    pub fn pan_direction(self) -> Option<(f32, f32)> {
        match self {
            Self::PanUp => Some((0.0, 1.0)),
            Self::PanDown => Some((0.0, -1.0)),
            Self::PanLeft => Some((1.0, 0.0)),
            Self::PanRight => Some((-1.0, 0.0)),
            _ => None,
        }
    }
}
