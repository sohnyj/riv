//! Context menu; TPM_RETURNCMD returns the selection for the single dispatcher.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, MENU_ITEM_FLAGS, MF_CHECKED, MF_DISABLED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, SM_CYMENU, TPM_CENTERALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TPM_VCENTERALIGN, TrackPopupMenuEx, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, Result};

use crate::actions::{Action, ActivationGate};

/// What one menu level is meant to hold, whatever the display measures.
const MENU_LEVEL_CAPACITY: usize = 25;

/// Names to show when the display cannot be measured: what a 720p screen holds at the heaviest scaling.
const UNMEASURED_CAPACITY: usize = 9;

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
    pub has_navigation_targets: bool,
    pub has_animation: bool,
    pub file_info_shown: bool,
    pub loop_enabled: bool,
    pub open_url_available: bool,
    pub playlist_names: Vec<String>,
    /// Absolute index of the first shown name; doubles as the count hidden before it.
    pub playlist_first_index: usize,
    pub playlist_current_slot: Option<usize>,
    pub playlist_hidden_after: usize,
    pub animation_paused: bool,
    pub fit_height: bool,
    pub preserve_zoom: bool,
    pub always_on_top: bool,
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
    /// The Playlist submenu, kept so the playlist key can open the menu at it.
    playlist_menu: HMENU,
}

/// Win32 menus read "&" as an access key prefix; double it to render literally.
fn escape_ampersands(label: &str) -> String {
    label.replace('&', "&&")
}

/// Exit alone takes an access key, so its first letter no longer selects it.
fn access_key(action: Action) -> Option<char> {
    match action {
        Action::Exit => Some('x'),
        _ => None,
    }
}

/// Prefixes the access key with "&" in an escaped label, leaving the shortcut column alone.
fn mark_access_key(mut escaped: String, access_key: Option<char>) -> String {
    let position =
        access_key.and_then(|key| escaped.split('\t').next().and_then(|label| label.find(key)));
    if let Some(index) = position {
        escaped.insert(index, '&');
    }
    escaped
}

impl MenuBuilder {
    fn new(state: MenuState) -> Self {
        Self {
            entries: Vec::new(),
            state_snapshot: state,
            playlist_menu: HMENU::default(),
        }
    }

    fn gate_satisfied(&self, gate: ActivationGate) -> bool {
        match gate {
            ActivationGate::Window => true,
            ActivationGate::Image => self.state_snapshot.has_image,
            ActivationGate::FileOnDisk => self.state_snapshot.has_file_on_disk,
            ActivationGate::ContainingFile => self.state_snapshot.has_containing_file,
            ActivationGate::Animation => self.state_snapshot.has_animation,
            ActivationGate::NavigationTargets => self.state_snapshot.has_navigation_targets,
        }
    }

    fn append_action(&mut self, menu: HMENU, action: Action) -> Result<()> {
        self.append_action_labeled(menu, action, action.label())
    }

    /// The one place text reaches the menu, so every label is escaped exactly once.
    fn append_text(
        menu: HMENU,
        flags: MENU_ITEM_FLAGS,
        identifier: usize,
        text: &str,
        access_key: Option<char>,
    ) -> Result<()> {
        let escaped = mark_access_key(escape_ampersands(text), access_key);
        unsafe { AppendMenuW(menu, flags, identifier, &HSTRING::from(escaped.as_str())) }
    }

    /// An action's label with its shortcut in a tab-separated column.
    fn menu_text(&self, action: Action, label: &str) -> String {
        match self.state_snapshot.shortcuts.get(action.name()) {
            Some(shortcut) => format!("{label}\t{shortcut}"),
            None => label.to_string(),
        }
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
            Action::ShowFileInfo => self.state_snapshot.file_info_shown,
            Action::Loop => self.state_snapshot.loop_enabled,
            Action::PreserveZoom => self.state_snapshot.preserve_zoom,
            Action::AlwaysOnTop => self.state_snapshot.always_on_top,
            Action::Mirror => self.state_snapshot.mirrored,
            Action::Flip => self.state_snapshot.flipped,
            _ => false,
        };
        if checked {
            flags |= MF_CHECKED;
        }
        let text = self.menu_text(action, label);
        Self::append_text(menu, flags, identifier, &text, access_key(action))
    }

    fn append_open_with_entry(&mut self, menu: HMENU, index: usize, label: &str) -> Result<()> {
        self.entries.push(MenuSelection::OpenWithEntry(index));
        let identifier = self.entries.len();
        Self::append_text(menu, MF_STRING, identifier, label, None)
    }

    /// A disabled line counting the names hidden on that side; zero appends nothing.
    fn append_playlist_overflow(menu: HMENU, hidden: usize) -> Result<()> {
        if hidden == 0 {
            return Ok(());
        }
        let label = format!("... {hidden} more");
        Self::append_text(menu, MF_STRING | MF_GRAYED | MF_DISABLED, 0, &label, None)
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
        Self::append_text(menu, flags, identifier, label, None)
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
        Self::append_text(menu, flags, submenu.0 as usize, label, None)
    }

    fn build(&mut self) -> Result<HMENU> {
        let menu = unsafe { CreatePopupMenu()? };
        self.append_action(menu, Action::Open)?;
        self.append_action(menu, Action::OpenUrl)?;

        let recent = unsafe { CreatePopupMenu()? };
        for index in 0..self.state_snapshot.recent_names.len() {
            let name = self.state_snapshot.recent_names[index].clone();
            self.append_action_labeled(recent, Action::Recent(index as u8), &name)?;
        }
        if !self.state_snapshot.recent_names.is_empty() {
            self.append_separator(recent)?;
        }
        self.append_action_labeled(recent, Action::ClearRecents, "Clear recents")?;
        self.append_submenu(menu, recent, "Open recent", true)?;
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
        self.append_action_labeled(open_with, Action::OtherApplication, "Other application...")?;
        // No on-disk file (archive member or URL) means nothing to hand off.
        self.append_submenu(
            menu,
            open_with,
            "Open with",
            self.state_snapshot.has_file_on_disk,
        )?;
        self.append_separator(menu)?;

        let playlist = unsafe { CreatePopupMenu()? };
        Self::append_playlist_overflow(playlist, self.state_snapshot.playlist_first_index)?;
        let playlist_names = self.state_snapshot.playlist_names.clone();
        for (slot, name) in playlist_names.iter().enumerate() {
            self.append_playlist_entry(playlist, slot, name)?;
        }
        Self::append_playlist_overflow(playlist, self.state_snapshot.playlist_hidden_after)?;
        self.playlist_menu = playlist;
        // The playlist key opens this same submenu, so the label carries that shortcut.
        let playlist_label = self.menu_text(Action::Playlist, Action::Playlist.label());
        // No folder listing means nothing to jump to.
        self.append_submenu(
            menu,
            playlist,
            &playlist_label,
            self.gate_satisfied(Action::Playlist.gate()),
        )?;
        self.append_action(menu, Action::Loop)?;
        self.append_separator(menu)?;
        self.append_action(menu, Action::PreviousFile)?;
        self.append_action(menu, Action::NextFile)?;
        let playback = unsafe { CreatePopupMenu()? };
        let pause_label = if self.state_snapshot.animation_paused {
            "Resume"
        } else {
            "Pause"
        };
        self.append_action_labeled(playback, Action::Pause, pause_label)?;
        self.append_action(playback, Action::PreviousFrame)?;
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

        self.append_action(menu, Action::ShowFileInfo)?;
        self.append_action(menu, Action::Reload)?;
        self.append_separator(menu)?;

        let view = unsafe { CreatePopupMenu()? };
        // The label names the axis a click switches to (slideshow convention).
        let fit_label = if self.state_snapshot.fit_height {
            "Fit width"
        } else {
            "Fit height"
        };
        self.append_action_labeled(view, Action::ToggleFitMode, fit_label)?;
        self.append_action(view, Action::PreserveZoom)?;
        self.append_separator(view)?;
        self.append_action(view, Action::ZoomIn)?;
        self.append_action(view, Action::ZoomOut)?;
        self.append_action(view, Action::ToggleZoom)?;
        self.append_separator(view)?;
        self.append_action(view, Action::RotateLeft)?;
        self.append_action(view, Action::RotateRight)?;
        self.append_separator(view)?;
        self.append_action(view, Action::Mirror)?;
        self.append_action(view, Action::Flip)?;
        self.append_submenu(menu, view, "View", true)?;

        let tools = unsafe { CreatePopupMenu()? };
        self.append_action(tools, Action::ShowInExplorer)?;
        self.append_action(tools, Action::Rename)?;
        self.append_action(tools, Action::Delete)?;
        self.append_separator(tools)?;
        let slideshow_label = if self.state_snapshot.slideshow_active {
            "Stop slideshow"
        } else {
            "Start slideshow"
        };
        self.append_action_labeled(tools, Action::ToggleSlideshow, slideshow_label)?;
        self.append_separator(tools)?;
        self.append_action(tools, Action::Settings)?;
        self.append_submenu(menu, tools, "Tools", true)?;

        let window = unsafe { CreatePopupMenu()? };
        self.append_action(window, Action::AlwaysOnTop)?;
        let fullscreen_label = if self.state_snapshot.fullscreen {
            "Exit fullscreen"
        } else {
            "Enter fullscreen"
        };
        self.append_action_labeled(window, Action::Fullscreen, fullscreen_label)?;
        self.append_submenu(menu, window, "Window", true)?;
        self.append_separator(menu)?;
        self.append_action(menu, Action::Exit)?;
        Ok(menu)
    }
}

/// Popup rows stand taller than the menu bar row this metric names; the quarter is measured margin.
fn menu_row_height(dpi: u32) -> i32 {
    let bar_row = unsafe { GetSystemMetricsForDpi(SM_CYMENU, dpi) }.max(1);
    bar_row + bar_row / 4
}

/// The title bar and top frame a normal window wears, which the menu leaves clear.
fn title_bar_height(dpi: u32) -> i32 {
    let mut frame = RECT::default();
    if unsafe {
        AdjustWindowRectExForDpi(
            &raw mut frame,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    }
    .is_err()
    {
        return 0;
    }
    -frame.top
}

/// Names that leave both "..." lines room; odd, so the current file sits in the middle.
fn capacity_for_height(usable_height: i32, row_height: i32) -> usize {
    let name_count = usable_height / row_height.max(1) - 2;
    let odd_name_count = if name_count % 2 == 0 {
        name_count - 1
    } else {
        name_count
    };
    // Too short to center a list still offers the current file; too tall stops at a menu level.
    (odd_name_count.max(1) as usize).min(MENU_LEVEL_CAPACITY)
}

/// Names the display shows with the taskbar and the title bar left clear.
pub fn playlist_capacity(window: HWND) -> usize {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut monitor_info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // Without a measurement, guess low so the menu still fits a small display.
    if !unsafe { GetMonitorInfoW(monitor, &raw mut monitor_info) }.as_bool() {
        return UNMEASURED_CAPACITY;
    }
    let dpi = crate::window::geometry::dpi_for_window(window);
    // The work area already excludes the taskbar.
    let work_height = monitor_info.rcWork.bottom - monitor_info.rcWork.top;
    capacity_for_height(work_height - title_bar_height(dpi), menu_row_height(dpi))
}

/// Which menu goes on screen; one build serves both.
#[derive(Clone, Copy)]
pub enum MenuTarget {
    /// The whole context menu, at the point.
    Full,
    /// Only the Playlist submenu, centered on the point.
    Playlist,
}

pub fn show(
    window: HWND,
    state: MenuState,
    x: i32,
    y: i32,
    target: MenuTarget,
) -> Option<MenuSelection> {
    let mut builder = MenuBuilder::new(state);
    let menu = builder.build().ok()?;
    let (tracked, flags) = match target {
        MenuTarget::Full => (menu, TPM_RETURNCMD | TPM_RIGHTBUTTON),
        MenuTarget::Playlist => (
            builder.playlist_menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_CENTERALIGN | TPM_VCENTERALIGN,
        ),
    };
    let selected = unsafe { TrackPopupMenuEx(tracked, flags.0, x, y, window, None) };
    // Destroying the menu takes its submenus with it.
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
            has_navigation_targets: false,
            has_animation: true,
            file_info_shown: false,
            loop_enabled: true,
            open_url_available: true,
            playlist_names: Vec::new(),
            playlist_first_index: 0,
            playlist_current_slot: None,
            playlist_hidden_after: 0,
            animation_paused: false,
            fit_height: false,
            preserve_zoom: false,
            always_on_top: false,
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
        let mut builder = MenuBuilder::new(state);
        let menu = builder.build().expect("menu builds");
        let position = position_of_label(menu, label).expect("submenu present");
        let grayed = is_grayed(menu, position);
        let _ = unsafe { DestroyMenu(menu) };
        grayed
    }

    #[test]
    fn open_with_follows_the_on_disk_file() {
        assert!(!submenu_is_grayed(state(), "Open with")); // a plain file can hand off
        let mut without_file = state();
        without_file.has_file_on_disk = false;
        assert!(submenu_is_grayed(without_file, "Open with")); // URL or archive member cannot
    }

    #[test]
    fn playback_follows_the_animation() {
        assert!(!submenu_is_grayed(state(), "Playback"));
        let mut still = state();
        still.has_animation = false;
        assert!(submenu_is_grayed(still, "Playback"));
    }

    fn submenu_by_label(menu: HMENU, label: &str) -> HMENU {
        let position =
            position_of_label(menu, label).unwrap_or_else(|| panic!("{label} submenu present"));
        unsafe { GetSubMenu(menu, position as i32) }
    }

    fn item_label(menu: HMENU, position: u32) -> String {
        let mut text = [0u16; 64];
        let length = unsafe { GetMenuStringW(menu, position, Some(&mut text), MF_BYPOSITION) };
        String::from_utf16_lossy(&text[..length as usize])
    }

    fn item_labels(menu: HMENU) -> Vec<String> {
        (0..unsafe { GetMenuItemCount(Some(menu)) })
            .map(|position| item_label(menu, position as u32))
            .collect()
    }

    fn position_of_label(menu: HMENU, label: &str) -> Option<u32> {
        item_labels(menu)
            .iter()
            .position(|candidate| candidate == label)
            .map(|position| position as u32)
    }

    fn is_grayed(menu: HMENU, position: u32) -> bool {
        let flags = unsafe { GetMenuState(menu, position, MF_BYPOSITION) };
        MENU_ITEM_FLAGS(flags) & MF_GRAYED == MF_GRAYED
    }

    /// Label without the shortcut column.
    fn bare_label(menu: HMENU, position: u32) -> String {
        let label = item_label(menu, position);
        label.split('\t').next().unwrap_or_default().to_string()
    }

    #[test]
    fn view_leads_with_the_fit_toggle() {
        let mut builder = MenuBuilder::new(state());
        let menu = builder.build().expect("menu builds");
        let view = submenu_by_label(menu, "View");
        // The fit label names the other axis: width is current here.
        assert_eq!(bare_label(view, 0), "Fit height");
        assert_eq!(bare_label(view, 1), "Preserve zoom");
        assert_eq!(bare_label(view, 3), "Zoom in");
        assert_eq!(bare_label(view, 4), "Zoom out");
        assert_eq!(bare_label(view, 5), "Toggle zoom");

        let mut height_state = state();
        height_state.fit_height = true;
        let mut builder = MenuBuilder::new(height_state);
        let menu = builder.build().expect("menu builds");
        let view = submenu_by_label(menu, "View");
        assert_eq!(bare_label(view, 0), "Fit width");
    }

    #[test]
    fn ampersands_in_names_render_literally() {
        let mut with_names = state();
        with_names.has_navigation_targets = true;
        with_names.playlist_names = vec!["a&b.png".to_string()];
        with_names.recent_names = vec!["c&d.png".to_string()];
        with_names.open_with_items = vec!["E & F".to_string()];
        let mut builder = MenuBuilder::new(with_names);
        let menu = builder.build().expect("menu builds");
        // GetMenuString returns the stored text; "&&" draws as a literal "&".
        assert_eq!(
            item_label(submenu_by_label(menu, "Open recent"), 0),
            "c&&d.png"
        );
        assert_eq!(item_label(submenu_by_label(menu, "Open with"), 0), "E && F");
        assert_eq!(
            item_label(submenu_by_label(menu, "Playlist"), 0),
            "a&&b.png"
        );
        let _ = unsafe { DestroyMenu(menu) };
    }

    #[test]
    fn playlist_follows_the_folder_listing() {
        assert!(submenu_is_grayed(state(), "Playlist")); // no listing to jump to
        let mut with_folder = state();
        with_folder.has_navigation_targets = true;
        with_folder.playlist_names = vec!["a.png".to_string()];
        assert!(!submenu_is_grayed(with_folder, "Playlist"));
    }

    #[test]
    fn playlist_lists_the_window_and_collapses_the_rest() {
        let mut with_folder = state();
        with_folder.has_navigation_targets = true;
        with_folder.playlist_names = (0..20).map(|index| format!("{index:03}.png")).collect();
        with_folder.playlist_first_index = 38;
        with_folder.playlist_current_slot = Some(12);
        with_folder.playlist_hidden_after = 42;
        let mut builder = MenuBuilder::new(with_folder);
        let menu = builder.build().expect("menu builds");
        let submenu = submenu_by_label(menu, "Playlist");
        assert_eq!(unsafe { GetMenuItemCount(Some(submenu)) }, 22);
        // Overflow lines at both ends show each side's count and take no selection.
        let overflow_line = |position: u32| {
            assert!(is_grayed(submenu, position));
            item_label(submenu, position)
        };
        assert_eq!(overflow_line(0), "... 38 more");
        assert_eq!(overflow_line(21), "... 42 more");
        // The current file carries the check marker, shifted past the leading overflow line.
        let current_flags = unsafe { GetMenuState(submenu, 13, MF_BYPOSITION) };
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
    fn the_name_count_stays_odd_below_the_display() {
        // Ten rows: two go to the "..." lines, and the odd count centers the current file.
        assert_eq!(capacity_for_height(350, 35), 7);
        assert_eq!(capacity_for_height(385, 35), 9);
        // A display too short for a centered list still offers the current file.
        assert_eq!(capacity_for_height(100, 35), 1);
        // 1080p at 200%: work area 984 less the title bar, rows of about 47.
        assert_eq!(capacity_for_height(920, 47), 17);
        // A tall display stops at the cap one menu level is meant to hold.
        assert_eq!(capacity_for_height(2000, 35), MENU_LEVEL_CAPACITY);
        for height in 0..2000 {
            let names = capacity_for_height(height, 35);
            assert!(names % 2 == 1, "{names} names for {height} pixels");
        }
        // Where the unmeasured count comes from: a 720p work area at 200% scaling.
        assert_eq!(capacity_for_height(624 - 62, 47), UNMEASURED_CAPACITY);
    }

    #[test]
    fn the_playlist_submenu_shows_the_playlist_shortcut() {
        let mut with_shortcut = state();
        with_shortcut
            .shortcuts
            .insert(Action::Playlist.name(), "E".to_string());
        let mut builder = MenuBuilder::new(with_shortcut);
        let menu = builder.build().expect("menu builds");
        let labels = item_labels(menu);
        assert!(labels.contains(&"Playlist\tE".to_string()));
        // The key opens this submenu itself, so the builder hands its handle back.
        assert_eq!(builder.playlist_menu, submenu_by_label(menu, "Playlist\tE"));
        let _ = unsafe { DestroyMenu(menu) };
    }

    #[test]
    fn top_level_items_follow_the_menu_order() {
        let mut builder = MenuBuilder::new(state());
        let menu = builder.build().expect("menu builds");
        let labels = item_labels(menu);
        let _ = unsafe { DestroyMenu(menu) };
        let expected: Vec<&str> = vec![
            "Open...",
            "Open URL...",
            "Open recent",
            "Open with",
            "", // separator
            "Playlist",
            "Loop",
            "", // separator
            "Previous",
            "Next",
            "Playback",
            "", // separator
            "Show file info",
            "Reload",
            "", // separator
            "View",
            "Tools",
            "Window",
            "", // separator
            "E&xit",
        ];
        assert_eq!(labels, expected);
    }

    #[test]
    fn window_and_tools_carry_their_sections() {
        let mut builder = MenuBuilder::new(state());
        let menu = builder.build().expect("menu builds");
        let window = submenu_by_label(menu, "Window");
        assert_eq!(bare_label(window, 0), "Always on top");
        assert_eq!(bare_label(window, 1), "Enter fullscreen");
        let tools = submenu_by_label(menu, "Tools");
        let tools_labels: Vec<String> = (0..unsafe { GetMenuItemCount(Some(tools)) })
            .map(|position| bare_label(tools, position as u32))
            .collect();
        assert_eq!(
            tools_labels,
            vec![
                "Show in Explorer",
                "Rename...",
                "Delete",
                "", // separator
                "Start slideshow",
                "", // separator
                "Settings",
            ]
        );
        let _ = unsafe { DestroyMenu(menu) };
    }
}
