//! Context menu; TPM_RETURNCMD returns the selection for the single dispatcher.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::{HSTRING, Result};

use crate::actions::{Action, ActivationGate};

#[derive(Clone, Copy)]
pub enum MenuSelection {
    Action(Action),
    OpenWithEntry(usize),
}

pub struct MenuState {
    pub has_image: bool,
    pub has_file_on_disk: bool,
    pub has_folder: bool,
    pub has_animation: bool,
    pub animation_paused: bool,
    pub preserve_zoom: bool,
    pub mirrored: bool,
    pub flipped: bool,
    pub fullscreen: bool,
    pub slideshow_active: bool,
    pub recent_names: Vec<String>,
    pub open_with_items: Vec<String>,
    pub open_with_has_default: bool,
    pub shortcuts: HashMap<&'static str, String>,
}

struct MenuBuilder {
    /// Command IDs are entries index + 1; 0 means dismissed.
    entries: Vec<MenuSelection>,
    state_snapshot: MenuState,
}

impl MenuBuilder {
    fn gate_satisfied(&self, gate: ActivationGate) -> bool {
        match gate {
            ActivationGate::Window => true,
            ActivationGate::Image => self.state_snapshot.has_image,
            ActivationGate::FileOnDisk => self.state_snapshot.has_file_on_disk,
            ActivationGate::Animation => self.state_snapshot.has_animation,
            ActivationGate::Folder => self.state_snapshot.has_folder,
        }
    }

    fn append_action(&mut self, menu: HMENU, action: Action) -> Result<()> {
        self.append_action_labeled(menu, action, action.label())
    }

    fn append_action_labeled(&mut self, menu: HMENU, action: Action, label: &str) -> Result<()> {
        self.entries.push(MenuSelection::Action(action));
        let identifier = self.entries.len();
        let mut flags = MF_STRING;
        let clear_without_recents =
            action == Action::ClearRecents && self.state_snapshot.recent_names.is_empty();
        if !self.gate_satisfied(action.gate()) || clear_without_recents {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        let checked = match action {
            Action::PreserveZoom => self.state_snapshot.preserve_zoom,
            Action::Mirror => self.state_snapshot.mirrored,
            Action::Flip => self.state_snapshot.flipped,
            _ => false,
        };
        if checked {
            flags |= MF_CHECKED;
        }
        // Text after a tab renders as the right-aligned shortcut column.
        let text = match self.state_snapshot.shortcuts.get(action.name()) {
            Some(shortcut) => format!("{label}\t{shortcut}"),
            None => label.to_string(),
        };
        unsafe { AppendMenuW(menu, flags, identifier, &HSTRING::from(text.as_str())) }
    }

    fn append_open_with_entry(&mut self, menu: HMENU, index: usize, label: &str) -> Result<()> {
        self.entries.push(MenuSelection::OpenWithEntry(index));
        let identifier = self.entries.len();
        unsafe { AppendMenuW(menu, MF_STRING, identifier, &HSTRING::from(label)) }
    }

    fn append_separator(&self, menu: HMENU) -> Result<()> {
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }
    }

    fn append_submenu(&self, menu: HMENU, submenu: HMENU, label: &str) -> Result<()> {
        unsafe { AppendMenuW(menu, MF_POPUP, submenu.0 as usize, &HSTRING::from(label)) }
    }

    fn build(&mut self) -> Result<HMENU> {
        let menu = unsafe { CreatePopupMenu()? };
        self.append_action(menu, Action::Open)?;

        let recent = unsafe { CreatePopupMenu()? };
        for index in 0..self.state_snapshot.recent_names.len().min(10) {
            let name = self.state_snapshot.recent_names[index].clone();
            self.append_action_labeled(recent, Action::Recent(index as u8), &name)?;
        }
        if !self.state_snapshot.recent_names.is_empty() {
            self.append_separator(recent)?;
        }
        self.append_action_labeled(recent, Action::ClearRecents, "Clear Recents")?;
        self.append_submenu(menu, recent, "Open Recent")?;
        let open_with = unsafe { CreatePopupMenu()? };
        let open_with_items = self.state_snapshot.open_with_items.clone();
        for (index, label) in open_with_items.iter().enumerate() {
            self.append_open_with_entry(open_with, index, label)?;
            if index == 0 && self.state_snapshot.open_with_has_default {
                self.append_separator(open_with)?;
            }
        }
        if !open_with_items.is_empty() {
            self.append_separator(open_with)?;
        }
        self.append_action_labeled(open_with, Action::OpenWithOther, "Other Application...")?;
        self.append_submenu(menu, open_with, "Open With")?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::ShowFileInfo)?;
        self.append_action(menu, Action::OpenContainingFolder)?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::PreviousFile)?;
        self.append_action(menu, Action::NextFile)?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::ReloadFile)?;
        self.append_action(menu, Action::Rename)?;
        self.append_action(menu, Action::Delete)?;
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

        let playback = unsafe { CreatePopupMenu()? };
        let pause_label = if self.state_snapshot.animation_paused {
            "Resume"
        } else {
            "Pause"
        };
        self.append_action_labeled(playback, Action::Pause, pause_label)?;
        self.append_action(playback, Action::NextFrame)?;
        self.append_separator(playback)?;
        self.append_action(playback, Action::DecreaseSpeed)?;
        self.append_action(playback, Action::IncreaseSpeed)?;
        self.append_action(playback, Action::ResetSpeed)?;
        self.append_submenu(menu, playback, "Playback")?;

        let tools = unsafe { CreatePopupMenu()? };
        let slideshow_label = if self.state_snapshot.slideshow_active {
            "Stop Slideshow"
        } else {
            "Start Slideshow"
        };
        self.append_action_labeled(tools, Action::Slideshow, slideshow_label)?;
        self.append_separator(tools)?;
        self.append_action(tools, Action::Options)?;
        self.append_action(tools, Action::About)?;
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

pub fn show(window: HWND, state: MenuState, x: i32, y: i32) -> Option<MenuSelection> {
    let mut builder = MenuBuilder {
        entries: Vec::new(),
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
        .then(|| builder.entries.get(identifier - 1).copied())
        .flatten()
}
