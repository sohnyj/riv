//! Context menu; TPM_RETURNCMD returns the selection for the single dispatcher.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};
use windows::core::{HSTRING, Result};

use crate::actions::{Action, ActivationGate};

/// Playlist submenu size; names beyond the window collapse into a "... nnn more" line.
pub const PLAYLIST_CAPACITY: usize = 25;

#[derive(Clone, Copy)]
pub enum MenuSelection {
    Action(Action),
    OpenWithEntry(usize),
    /// Index into the folder listing snapshot the menu was built from.
    PlaylistEntry(usize),
}

pub struct MenuState {
    pub has_image: bool,
    pub has_file_on_disk: bool,
    pub has_containing_file: bool,
    pub has_folder: bool,
    pub has_animation: bool,
    pub loop_enabled: bool,
    pub open_url_available: bool,
    pub playlist_names: Vec<String>,
    pub playlist_first_index: usize,
    pub playlist_current_slot: Option<usize>,
    pub playlist_hidden_count: usize,
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
            ActivationGate::ContainingFile => self.state_snapshot.has_containing_file,
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
        let open_url_without_curl =
            action == Action::OpenUrl && !self.state_snapshot.open_url_available;
        if !self.gate_satisfied(action.gate()) || clear_without_recents || open_url_without_curl {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        let checked = match action {
            Action::Loop => self.state_snapshot.loop_enabled,
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

    fn append_playlist_entry(&mut self, menu: HMENU, slot: usize, label: &str) -> Result<()> {
        self.entries.push(MenuSelection::PlaylistEntry(
            self.state_snapshot.playlist_first_index + slot,
        ));
        let identifier = self.entries.len();
        let mut flags = MF_STRING;
        if self.state_snapshot.playlist_current_slot == Some(slot) {
            flags |= MF_CHECKED;
        }
        unsafe { AppendMenuW(menu, flags, identifier, &HSTRING::from(label)) }
    }

    fn append_separator(&self, menu: HMENU) -> Result<()> {
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }
    }

    fn append_submenu(
        &self,
        menu: HMENU,
        submenu: HMENU,
        label: &str,
        enabled: bool,
    ) -> Result<()> {
        let mut flags = MF_POPUP;
        if !enabled {
            flags |= MF_GRAYED | MF_DISABLED;
        }
        unsafe { AppendMenuW(menu, flags, submenu.0 as usize, &HSTRING::from(label)) }
    }

    fn build(&mut self) -> Result<HMENU> {
        let menu = unsafe { CreatePopupMenu()? };
        self.append_action(menu, Action::Open)?;
        self.append_action(menu, Action::OpenUrl)?;

        let recent = unsafe { CreatePopupMenu()? };
        for index in 0..self.state_snapshot.recent_names.len().min(10) {
            let name = self.state_snapshot.recent_names[index].clone();
            self.append_action_labeled(recent, Action::Recent(index as u8), &name)?;
        }
        if !self.state_snapshot.recent_names.is_empty() {
            self.append_separator(recent)?;
        }
        self.append_action_labeled(recent, Action::ClearRecents, "Clear Recents")?;
        self.append_submenu(menu, recent, "Open Recent", true)?;
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
        // No on-disk file (archive member or URL) means nothing to hand off.
        self.append_submenu(
            menu,
            open_with,
            "Open With",
            self.state_snapshot.has_file_on_disk,
        )?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::ShowFileInfo)?;
        self.append_action(menu, Action::OpenContainingFolder)?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::PreviousFile)?;
        self.append_action(menu, Action::NextFile)?;
        self.append_action(menu, Action::Loop)?;
        let playlist = unsafe { CreatePopupMenu()? };
        let playlist_names = self.state_snapshot.playlist_names.clone();
        for (slot, name) in playlist_names.iter().enumerate() {
            self.append_playlist_entry(playlist, slot, name)?;
        }
        if self.state_snapshot.playlist_hidden_count > 0 {
            let more = format!("... {} more", self.state_snapshot.playlist_hidden_count);
            unsafe {
                AppendMenuW(
                    playlist,
                    MF_STRING | MF_GRAYED | MF_DISABLED,
                    0,
                    &HSTRING::from(more.as_str()),
                )?;
            }
        }
        // No folder listing means nothing to jump to.
        self.append_submenu(menu, playlist, "Playlist", self.state_snapshot.has_folder)?;
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
        // A still image has nothing to play.
        self.append_submenu(
            menu,
            playback,
            "Playback",
            self.state_snapshot.has_animation,
        )?;
        self.append_separator(menu)?;

        self.append_action(menu, Action::Reload)?;
        self.append_action(menu, Action::Rename)?;
        self.append_action(menu, Action::Delete)?;
        self.append_separator(menu)?;

        let view = unsafe { CreatePopupMenu()? };
        self.append_action(view, Action::ZoomIn)?;
        self.append_action(view, Action::ZoomOut)?;
        self.append_action(view, Action::ResetZoom)?;
        self.append_action(view, Action::PreserveZoom)?;
        self.append_separator(view)?;
        self.append_action(view, Action::RotateLeft)?;
        self.append_action(view, Action::RotateRight)?;
        self.append_separator(view)?;
        self.append_action(view, Action::Mirror)?;
        self.append_action(view, Action::Flip)?;
        self.append_submenu(menu, view, "View", true)?;

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
        self.append_submenu(menu, tools, "Tools", true)?;

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

#[cfg(test)]
mod menu_structure_tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuState, GetMenuStringW, GetSubMenu, MENU_ITEM_FLAGS, MF_BYPOSITION,
    };

    fn state() -> MenuState {
        MenuState {
            has_image: true,
            has_file_on_disk: true,
            has_containing_file: true,
            has_folder: false,
            has_animation: true,
            loop_enabled: true,
            open_url_available: true,
            playlist_names: Vec::new(),
            playlist_first_index: 0,
            playlist_current_slot: None,
            playlist_hidden_count: 0,
            animation_paused: false,
            preserve_zoom: false,
            mirrored: false,
            flipped: false,
            fullscreen: false,
            slideshow_active: false,
            recent_names: Vec::new(),
            open_with_items: Vec::new(),
            open_with_has_default: false,
            shortcuts: HashMap::new(),
        }
    }

    fn submenu_is_grayed(state: MenuState, label: &str) -> bool {
        let mut builder = MenuBuilder {
            entries: Vec::new(),
            state_snapshot: state,
        };
        let menu = builder.build().expect("menu builds");
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        let mut grayed = None;
        for position in 0..count {
            let mut text = [0u16; 64];
            let length =
                unsafe { GetMenuStringW(menu, position as u32, Some(&mut text), MF_BYPOSITION) };
            if String::from_utf16_lossy(&text[..length as usize]) == label {
                let flags = unsafe { GetMenuState(menu, position as u32, MF_BYPOSITION) };
                grayed = Some(MENU_ITEM_FLAGS(flags) & MF_GRAYED == MF_GRAYED);
                break;
            }
        }
        let _ = unsafe { DestroyMenu(menu) };
        grayed.expect("submenu present")
    }

    #[test]
    fn open_with_follows_the_on_disk_file() {
        assert!(!submenu_is_grayed(state(), "Open With")); // a plain file can hand off
        let mut without_file = state();
        without_file.has_file_on_disk = false;
        assert!(submenu_is_grayed(without_file, "Open With")); // URL or archive member cannot
    }

    #[test]
    fn playback_follows_the_animation() {
        assert!(!submenu_is_grayed(state(), "Playback"));
        let mut still = state();
        still.has_animation = false;
        assert!(submenu_is_grayed(still, "Playback"));
    }

    #[test]
    fn playlist_follows_the_folder_listing() {
        assert!(submenu_is_grayed(state(), "Playlist")); // no listing to jump to
        let mut with_folder = state();
        with_folder.has_folder = true;
        with_folder.playlist_names = vec!["a.png".to_string()];
        assert!(!submenu_is_grayed(with_folder, "Playlist"));
    }

    #[test]
    fn playlist_lists_the_window_and_collapses_the_rest() {
        let mut with_folder = state();
        with_folder.has_folder = true;
        with_folder.playlist_names = (0..25).map(|index| format!("{index:03}.png")).collect();
        with_folder.playlist_first_index = 38;
        with_folder.playlist_current_slot = Some(12);
        with_folder.playlist_hidden_count = 75;
        let mut builder = MenuBuilder {
            entries: Vec::new(),
            state_snapshot: with_folder,
        };
        let menu = builder.build().expect("menu builds");
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        let mut submenu = None;
        for position in 0..count {
            let mut text = [0u16; 64];
            let length =
                unsafe { GetMenuStringW(menu, position as u32, Some(&mut text), MF_BYPOSITION) };
            if String::from_utf16_lossy(&text[..length as usize]) == "Playlist" {
                submenu = Some(unsafe { GetSubMenu(menu, position) });
                break;
            }
        }
        let submenu = submenu.expect("Playlist submenu present");
        assert_eq!(unsafe { GetMenuItemCount(Some(submenu)) }, 26);
        // The overflow line shows the count and takes no selection.
        let mut text = [0u16; 64];
        let length = unsafe { GetMenuStringW(submenu, 25, Some(&mut text), MF_BYPOSITION) };
        assert_eq!(
            String::from_utf16_lossy(&text[..length as usize]),
            "... 75 more"
        );
        let more_flags = unsafe { GetMenuState(submenu, 25, MF_BYPOSITION) };
        assert!(MENU_ITEM_FLAGS(more_flags) & MF_GRAYED == MF_GRAYED);
        // The current file carries the check marker.
        let current_flags = unsafe { GetMenuState(submenu, 12, MF_BYPOSITION) };
        assert!(MENU_ITEM_FLAGS(current_flags) & MF_CHECKED == MF_CHECKED);
        // Selections map back to absolute listing indices.
        assert!(
            builder
                .entries
                .iter()
                .any(|entry| matches!(entry, MenuSelection::PlaylistEntry(50)))
        );
        let _ = unsafe { DestroyMenu(menu) };
    }

    #[test]
    fn top_level_items_follow_the_menu_order() {
        let mut builder = MenuBuilder {
            entries: Vec::new(),
            state_snapshot: state(),
        };
        let menu = builder.build().expect("menu builds");
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        let mut labels = Vec::new();
        for position in 0..count {
            let mut label = [0u16; 64];
            let length =
                unsafe { GetMenuStringW(menu, position as u32, Some(&mut label), MF_BYPOSITION) };
            labels.push(String::from_utf16_lossy(&label[..length as usize]));
        }
        let _ = unsafe { DestroyMenu(menu) };
        let expected: Vec<&str> = vec![
            "Open...",
            "Open URL...",
            "Open Recent",
            "Open With",
            "", // separator
            "Show File Info",
            "Show in Explorer",
            "", // separator
            "Previous",
            "Next",
            "Loop",
            "Playlist",
            "Playback",
            "", // separator
            "Reload",
            "Rename...",
            "Delete",
            "", // separator
            "View",
            "Tools",
            "Enter Fullscreen",
            "", // separator
            "Exit",
        ];
        assert_eq!(labels, expected);
    }
}
