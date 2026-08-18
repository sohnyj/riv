//! JSON settings in riv.json next to the exe; defaults are never written.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::image::core::SortMode;
use crate::view::dither::DitherMode;
use crate::view::renderer::ScalingFilter;
use crate::view::transform::FitMode;

pub const MAXIMUM_RECENT_FILES: usize = 10;

/// The default window background; used when the custom color is off.
pub const DEFAULT_BACKGROUND_COLOR: (u8, u8, u8) = (0x21, 0x21, 0x21);

/// riv.json option keys; the read and the write name the same constant.
const KEY_BACKGROUND_COLOR_ENABLED: &str = "backgroundcolorenabled";
const KEY_BACKGROUND_COLOR: &str = "backgroundcolor";
const KEY_TITLE_BAR_TEXT: &str = "titlebartext";
const KEY_CONTROL_DRAG_WINDOW: &str = "ctrldragwindow";
const KEY_REMEMBER_WINDOW_SIZE_AND_POSITION: &str = "rememberwindowsizeandposition";
const KEY_HIDE_CURSOR_FULLSCREEN: &str = "hidecursorfullscreen";
const KEY_SCALING_FILTER: &str = "scaling";
const KEY_FIT_MODE: &str = "fitmode";
const KEY_ZOOM_STEP_PERCENT: &str = "zoomstep";
const KEY_DITHER_MODE: &str = "dither";
const KEY_FRACTIONAL_WHEEL_ZOOM: &str = "fractionalwheelzoom";
const KEY_CURSOR_ZOOM: &str = "cursorzoom";
const KEY_SORT_FILES_BY: &str = "sortfilesby";
const KEY_SORT_DESCENDING: &str = "sortdescending";
const KEY_PRELOADING: &str = "preloading";
const KEY_LOOP_WITHIN_FOLDER: &str = "loopwithinfolder";
const KEY_SLIDESHOW_DIRECTION: &str = "slideshowdirection";
const KEY_SLIDESHOW_INTERVAL_SECONDS: &str = "slideshowinterval";
const KEY_AFTER_DELETION: &str = "afterdeletion";
const KEY_ASK_DELETE: &str = "askdelete";
const KEY_DETECT_FORMAT_BY_CONTENT: &str = "detectformatbycontent";
const KEY_REMEMBER_RECENTS: &str = "rememberrecents";
const KEY_SKIP_HIDDEN: &str = "skiphidden";

/// Combo rows in stored order; the index is read back by `Application::window_title`.
pub const TITLE_BAR_TEXT_CHOICES: [&str; 4] = [
    "App name",
    "File name",
    "[N/N] File name",
    "[N/N] Folder\\File name",
];
/// The index is a row of `PRELOAD_SPECIFICATIONS`.
pub const PRELOADING_CHOICES: [&str; 3] = ["Disabled", "Nearby", "Extended"];
/// The index is read back by `Options::slideshow_backward`.
pub const SLIDESHOW_DIRECTION_CHOICES: [&str; 2] = ["Backward", "Forward"];
/// The index picks the navigation command `delete_current_file` steps with.
pub const AFTER_DELETION_CHOICES: [&str; 2] = ["Move back", "Move forward"];

#[derive(Clone, PartialEq)]
pub struct Options {
    pub background_color_enabled: bool,
    pub background_color: (u8, u8, u8),
    pub title_bar_text: u32,
    pub control_drag_window: bool,
    pub remember_window_size_and_position: bool,
    pub hide_cursor_fullscreen: bool,
    pub scaling_filter: u32,
    pub fit_mode: u32,
    pub zoom_step_percent: u32,
    pub dither_mode: u32,
    pub fractional_wheel_zoom: bool,
    pub cursor_zoom: bool,
    pub sort_files_by: u32,
    pub sort_descending: bool,
    pub preloading: u32,
    pub loop_within_folder: bool,
    pub slideshow_direction: u32,
    pub slideshow_interval_seconds: u32,
    pub after_deletion: u32,
    pub ask_delete: bool,
    pub detect_format_by_content: bool,
    pub remember_recents: bool,
    pub skip_hidden: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            background_color_enabled: false,
            background_color: DEFAULT_BACKGROUND_COLOR,
            title_bar_text: 1,
            control_drag_window: true,
            remember_window_size_and_position: true,
            hide_cursor_fullscreen: true,
            scaling_filter: 1,
            fit_mode: 0,
            zoom_step_percent: 25,
            dither_mode: 2,
            fractional_wheel_zoom: true,
            cursor_zoom: true,
            sort_files_by: 0,
            sort_descending: false,
            preloading: 1,
            loop_within_folder: true,
            slideshow_direction: 1,
            slideshow_interval_seconds: 5,
            after_deletion: 1,
            ask_delete: true,
            detect_format_by_content: false,
            remember_recents: true,
            skip_hidden: true,
        }
    }
}

impl Options {
    /// The slideshow steps backward; 0 = "Backward" in the direction combo.
    pub fn slideshow_backward(&self) -> bool {
        self.slideshow_direction == 0
    }

    fn from_document(document: &Value) -> Self {
        let default = Self::default();
        let Some(options) = document.get("options").and_then(Value::as_object) else {
            return default;
        };
        let boolean = |key: &str, fallback: bool| {
            options
                .get(key)
                .and_then(Value::as_bool)
                .unwrap_or(fallback)
        };
        // Narrowing before the range checks would wrap a large stored value into them.
        let unsigned = |key: &str, fallback: u32| {
            options
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(fallback)
        };
        // A stored value outside the list of choices falls back to the default.
        let choice = |key: &str, choices: usize, fallback: u32| {
            let value = unsigned(key, fallback);
            if (value as usize) < choices {
                value
            } else {
                fallback
            }
        };
        Self {
            background_color_enabled: boolean(
                KEY_BACKGROUND_COLOR_ENABLED,
                default.background_color_enabled,
            ),
            background_color: options
                .get(KEY_BACKGROUND_COLOR)
                .and_then(Value::as_str)
                .and_then(parse_hex_color)
                .unwrap_or(default.background_color),
            title_bar_text: choice(
                KEY_TITLE_BAR_TEXT,
                TITLE_BAR_TEXT_CHOICES.len(),
                default.title_bar_text,
            ),
            control_drag_window: boolean(KEY_CONTROL_DRAG_WINDOW, default.control_drag_window),
            remember_window_size_and_position: boolean(
                KEY_REMEMBER_WINDOW_SIZE_AND_POSITION,
                default.remember_window_size_and_position,
            ),
            hide_cursor_fullscreen: boolean(
                KEY_HIDE_CURSOR_FULLSCREEN,
                default.hide_cursor_fullscreen,
            ),
            scaling_filter: choice(
                KEY_SCALING_FILTER,
                ScalingFilter::IN_SETTING_ORDER.len(),
                default.scaling_filter,
            ),
            fit_mode: choice(
                KEY_FIT_MODE,
                FitMode::IN_SETTING_ORDER.len(),
                default.fit_mode,
            ),
            zoom_step_percent: unsigned(KEY_ZOOM_STEP_PERCENT, default.zoom_step_percent)
                .clamp(1, 200),
            dither_mode: choice(
                KEY_DITHER_MODE,
                DitherMode::IN_SETTING_ORDER.len(),
                default.dither_mode,
            ),
            fractional_wheel_zoom: boolean(
                KEY_FRACTIONAL_WHEEL_ZOOM,
                default.fractional_wheel_zoom,
            ),
            cursor_zoom: boolean(KEY_CURSOR_ZOOM, default.cursor_zoom),
            sort_files_by: choice(
                KEY_SORT_FILES_BY,
                SortMode::IN_SETTING_ORDER.len(),
                default.sort_files_by,
            ),
            sort_descending: boolean(KEY_SORT_DESCENDING, default.sort_descending),
            preloading: choice(KEY_PRELOADING, PRELOADING_CHOICES.len(), default.preloading),
            loop_within_folder: boolean(KEY_LOOP_WITHIN_FOLDER, default.loop_within_folder),
            slideshow_direction: choice(
                KEY_SLIDESHOW_DIRECTION,
                SLIDESHOW_DIRECTION_CHOICES.len(),
                default.slideshow_direction,
            ),
            slideshow_interval_seconds: unsigned(
                KEY_SLIDESHOW_INTERVAL_SECONDS,
                default.slideshow_interval_seconds,
            )
            .clamp(1, 3600),
            after_deletion: choice(
                KEY_AFTER_DELETION,
                AFTER_DELETION_CHOICES.len(),
                default.after_deletion,
            ),
            ask_delete: boolean(KEY_ASK_DELETE, default.ask_delete),
            detect_format_by_content: boolean(
                KEY_DETECT_FORMAT_BY_CONTENT,
                default.detect_format_by_content,
            ),
            remember_recents: boolean(KEY_REMEMBER_RECENTS, default.remember_recents),
            skip_hidden: boolean(KEY_SKIP_HIDDEN, default.skip_hidden),
        }
    }
}

fn format_hex_color((red, green, blue): (u8, u8, u8)) -> String {
    format!("#{red:02X}{green:02X}{blue:02X}")
}

fn parse_hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let digits = text.strip_prefix('#')?;
    // is_ascii keeps the byte slices on char boundaries (no panic).
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some((red, green, blue))
}

pub fn save_directory_is_writable() -> bool {
    let probe = settings_path().with_extension("json.probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub struct SettingsFile {
    path: PathBuf,
    document: Value,
    pub options: Options,
    /// ASCII-lowercased path keys dropped this session; the exit merge must not resurrect them.
    removed_recent_keys: HashSet<String>,
}

fn recent_files_of(document: &Value) -> Vec<(String, String)> {
    document
        .get("recents")
        .and_then(|recents| recents.get("recentfiles"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("name")?.as_str()?.to_string(),
                        entry.get("path")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl SettingsFile {
    pub fn load() -> Self {
        let path = settings_path();
        let document = read_document(&path);
        let options = Options::from_document(&document);
        Self {
            path,
            document,
            options,
            removed_recent_keys: HashSet::new(),
        }
    }

    /// Atomic save: write a temp file, then rename over.
    fn save(&self) -> std::io::Result<()> {
        let serialized =
            serde_json::to_string_pretty(&self.document).map_err(std::io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serialized)?;
        std::fs::rename(&temporary, &self.path)
    }

    pub fn keyboard_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get("keyboardbindings")?.as_object()
    }

    pub fn mouse_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get("mousebindings")?.as_object()
    }

    /// Writes the in-memory options into the settings document (persisted at exit/Apply).
    pub fn store_options(&mut self) {
        write_options(&mut self.document, &self.options);
        self.options = Options::from_document(&self.document);
    }

    pub fn set_options(&mut self, options: &Options) {
        write_options(&mut self.document, options);
        self.options = Options::from_document(&self.document);
    }

    /// Lists equal to the defaults are removed; unknown keys are preserved.
    pub fn set_binding_overrides(
        &mut self,
        keyboard: &[(String, Vec<String>)],
        mouse: &[(String, Vec<String>)],
    ) {
        let document = self
            .document
            .as_object_mut()
            .expect("settings document is object");
        for (section, resolved, defaults_of) in [
            (
                "keyboardbindings",
                keyboard,
                crate::bindings::default_keyboard_sequences as fn(&str) -> &'static [&'static str],
            ),
            (
                "mousebindings",
                mouse,
                crate::bindings::default_mouse_encodings as fn(&str) -> &'static [&'static str],
            ),
        ] {
            let object = object_section(document, section);
            for (action_name, sequences) in resolved {
                let defaults = defaults_of(action_name);
                if defaults.len() == sequences.len()
                    && defaults
                        .iter()
                        .zip(sequences.iter())
                        .all(|(default, sequence)| default == sequence)
                {
                    object.remove(action_name);
                } else {
                    object.insert(
                        action_name.clone(),
                        Value::Array(
                            sequences
                                .iter()
                                .map(|sequence| Value::String(sequence.clone()))
                                .collect(),
                        ),
                    );
                }
            }
            if object.is_empty() {
                document.remove(section);
            }
        }
    }

    pub fn window_geometry(&self) -> Option<(i32, i32, i32, i32, bool)> {
        let geometry = self.document.get("windowgeometry")?;
        let read = |key: &str| geometry.get(key)?.as_i64().map(|value| value as i32);
        Some((
            read("x")?,
            read("y")?,
            read("width").filter(|width| *width > 0)?,
            read("height").filter(|height| *height > 0)?,
            geometry
                .get("maximized")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ))
    }

    pub fn set_window_geometry(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        maximized: bool,
    ) {
        self.document
            .as_object_mut()
            .expect("settings document is object")
            .insert(
                "windowgeometry".to_string(),
                serde_json::json!({ "x": x, "y": y, "width": width, "height": height, "maximized": maximized }),
            );
    }

    pub fn last_file_dialog_directory(&self) -> Option<&str> {
        self.document
            .get("recents")?
            .get("lastfiledialogdirectory")?
            .as_str()
    }

    pub fn set_last_file_dialog_directory(&mut self, directory: &str) {
        self.document
            .as_object_mut()
            .expect("settings document is object")
            .entry("recents")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("recents is object")
            .insert(
                "lastfiledialogdirectory".to_string(),
                Value::String(directory.to_string()),
            );
    }

    pub fn recent_files(&self) -> Vec<(String, String)> {
        let mut files = recent_files_of(&self.document);
        // A hand-edited document can exceed the cap; every reader sees at most the limit.
        files.truncate(MAXIMUM_RECENT_FILES);
        files
    }

    /// Fold other instances' recents back in (union, this session first) before writing.
    pub fn save_merging_recents(&mut self) -> std::io::Result<()> {
        if self.options.remember_recents {
            let disk = read_document(&self.path);
            let mut files = self.recent_files();
            let mut seen: HashSet<String> = files
                .iter()
                .map(|(_, path)| path.to_ascii_lowercase())
                .collect();
            for (name, path) in recent_files_of(&disk) {
                let key = path.to_ascii_lowercase();
                if self.removed_recent_keys.contains(&key) {
                    continue; // dropped as missing this session
                }
                if seen.insert(key) {
                    files.push((name, path));
                }
            }
            files.truncate(MAXIMUM_RECENT_FILES);
            self.set_recent_files(&files);
        }
        self.save()
    }

    fn set_recent_files(&mut self, files: &[(String, String)]) {
        let list: Vec<Value> = files
            .iter()
            .map(|(name, path)| serde_json::json!({ "name": name, "path": path }))
            .collect();
        let document = self
            .document
            .as_object_mut()
            .expect("settings document is object");
        document
            .entry("recents")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("recents is object")
            .insert("recentfiles".to_string(), Value::Array(list));
    }

    pub fn add_recent_file(&mut self, path: &std::path::Path) {
        if !self.options.remember_recents {
            self.clear_recent_files();
            return;
        }
        let path_text = path.to_string_lossy().into_owned();
        let name = path.file_name().map_or_else(
            || path_text.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let mut files = self.recent_files();
        if files
            .first()
            .is_some_and(|(_, existing)| existing.eq_ignore_ascii_case(&path_text))
        {
            return;
        }
        files.retain(|(_, existing)| !existing.eq_ignore_ascii_case(&path_text));
        files.insert(0, (name, path_text));
        files.truncate(MAXIMUM_RECENT_FILES);
        self.set_recent_files(&files);
    }

    /// Drops the entry and tombstones it so the exit merge cannot restore it.
    pub fn remove_recent_file(&mut self, path: &std::path::Path) {
        let text = path.to_string_lossy();
        let mut files = self.recent_files();
        let count = files.len();
        files.retain(|(_, stored)| !stored.eq_ignore_ascii_case(&text));
        if files.len() != count {
            self.set_recent_files(&files);
        }
        self.removed_recent_keys.insert(text.to_ascii_lowercase());
    }

    pub fn clear_recent_files(&mut self) {
        if !self.recent_files().is_empty() {
            self.set_recent_files(&[]);
        }
    }
}

fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.to_path_buf()))
        .unwrap_or_default()
        .join("riv.json")
}

fn write_options(document: &mut Value, options: &Options) {
    let default = Options::default();
    let entries: [(&str, Value, Value); 23] = [
        (
            KEY_BACKGROUND_COLOR_ENABLED,
            Value::Bool(options.background_color_enabled),
            Value::Bool(default.background_color_enabled),
        ),
        (
            KEY_BACKGROUND_COLOR,
            Value::String(format_hex_color(options.background_color)),
            Value::String(format_hex_color(default.background_color)),
        ),
        (
            KEY_TITLE_BAR_TEXT,
            Value::from(options.title_bar_text),
            Value::from(default.title_bar_text),
        ),
        (
            KEY_CONTROL_DRAG_WINDOW,
            Value::Bool(options.control_drag_window),
            Value::Bool(default.control_drag_window),
        ),
        (
            KEY_REMEMBER_WINDOW_SIZE_AND_POSITION,
            Value::Bool(options.remember_window_size_and_position),
            Value::Bool(default.remember_window_size_and_position),
        ),
        (
            KEY_HIDE_CURSOR_FULLSCREEN,
            Value::Bool(options.hide_cursor_fullscreen),
            Value::Bool(default.hide_cursor_fullscreen),
        ),
        (
            KEY_SCALING_FILTER,
            Value::from(options.scaling_filter),
            Value::from(default.scaling_filter),
        ),
        (
            KEY_FIT_MODE,
            Value::from(options.fit_mode),
            Value::from(default.fit_mode),
        ),
        (
            KEY_ZOOM_STEP_PERCENT,
            Value::from(options.zoom_step_percent),
            Value::from(default.zoom_step_percent),
        ),
        (
            KEY_DITHER_MODE,
            Value::from(options.dither_mode),
            Value::from(default.dither_mode),
        ),
        (
            KEY_FRACTIONAL_WHEEL_ZOOM,
            Value::Bool(options.fractional_wheel_zoom),
            Value::Bool(default.fractional_wheel_zoom),
        ),
        (
            KEY_CURSOR_ZOOM,
            Value::Bool(options.cursor_zoom),
            Value::Bool(default.cursor_zoom),
        ),
        (
            KEY_SORT_FILES_BY,
            Value::from(options.sort_files_by),
            Value::from(default.sort_files_by),
        ),
        (
            KEY_SORT_DESCENDING,
            Value::Bool(options.sort_descending),
            Value::Bool(default.sort_descending),
        ),
        (
            KEY_PRELOADING,
            Value::from(options.preloading),
            Value::from(default.preloading),
        ),
        (
            KEY_LOOP_WITHIN_FOLDER,
            Value::Bool(options.loop_within_folder),
            Value::Bool(default.loop_within_folder),
        ),
        (
            KEY_SLIDESHOW_DIRECTION,
            Value::from(options.slideshow_direction),
            Value::from(default.slideshow_direction),
        ),
        (
            KEY_SLIDESHOW_INTERVAL_SECONDS,
            Value::from(options.slideshow_interval_seconds),
            Value::from(default.slideshow_interval_seconds),
        ),
        (
            KEY_AFTER_DELETION,
            Value::from(options.after_deletion),
            Value::from(default.after_deletion),
        ),
        (
            KEY_ASK_DELETE,
            Value::Bool(options.ask_delete),
            Value::Bool(default.ask_delete),
        ),
        (
            KEY_DETECT_FORMAT_BY_CONTENT,
            Value::Bool(options.detect_format_by_content),
            Value::Bool(default.detect_format_by_content),
        ),
        (
            KEY_REMEMBER_RECENTS,
            Value::Bool(options.remember_recents),
            Value::Bool(default.remember_recents),
        ),
        (
            KEY_SKIP_HIDDEN,
            Value::Bool(options.skip_hidden),
            Value::Bool(default.skip_hidden),
        ),
    ];
    let document = document
        .as_object_mut()
        .expect("settings document is object");
    let options_object = object_section(document, "options");
    for (key, value, default_value) in entries {
        if value == default_value {
            options_object.remove(key);
        } else {
            options_object.insert(key.to_string(), value);
        }
    }
    if !options.remember_recents {
        document.remove("recents");
    }
}

/// The section as an object, replacing a stored value that is not one (the reader ignores those).
fn object_section<'a>(
    document: &'a mut Map<String, Value>,
    section: &str,
) -> &'a mut Map<String, Value> {
    if !document.get(section).is_some_and(Value::is_object) {
        document.insert(section.to_string(), Value::Object(Map::new()));
    }
    document
        .get_mut(section)
        .and_then(Value::as_object_mut)
        .expect("section inserted as an object above")
}

fn read_document(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn hex_color_parses_and_rejects_without_panicking() {
        assert_eq!(parse_hex_color("#21A0FF"), Some((0x21, 0xA0, 0xFF)));
        assert_eq!(parse_hex_color("21A0FF"), None); // missing '#'
        assert_eq!(parse_hex_color("#12345"), None); // too short
        assert_eq!(parse_hex_color("#GGGGGG"), None); // non-hex
        // Six bytes but a multibyte char crosses a slice boundary; must not panic.
        assert_eq!(parse_hex_color("#a\u{20AC}ab"), None);
    }
}

#[cfg(test)]
mod option_bounds_tests {
    use super::*;

    #[test]
    fn out_of_range_indexes_fall_back_to_defaults() {
        let document = serde_json::json!({ "options": {
            "titlebartext": 9,
            "scaling": 9,
            "fitmode": 9,
            "preloading": 9,
            "dither": 9,
            "sortfilesby": 9,
            "afterdeletion": 9,
        }});
        let options = Options::from_document(&document);
        let default = Options::default();
        assert_eq!(options.title_bar_text, default.title_bar_text);
        assert_eq!(options.scaling_filter, default.scaling_filter);
        assert_eq!(options.fit_mode, default.fit_mode);
        assert_eq!(options.preloading, default.preloading);
        assert_eq!(options.dither_mode, default.dither_mode);
        assert_eq!(options.sort_files_by, default.sort_files_by);
        assert_eq!(options.after_deletion, default.after_deletion);
    }

    #[test]
    fn values_past_u32_fall_back_instead_of_wrapping() {
        // 2^32 + 2 truncated to 2, which passed the choice check as a valid index.
        let document = serde_json::json!({ "options": {
            "titlebartext": 4_294_967_298u64,
            "zoomstep": 4_294_967_296u64,
        }});
        let options = Options::from_document(&document);
        let default = Options::default();
        assert_eq!(options.title_bar_text, default.title_bar_text);
        assert_eq!(options.zoom_step_percent, default.zoom_step_percent);
    }

    #[test]
    fn writing_replaces_sections_that_are_not_objects() {
        // The reader already ignores these; the writer used to panic on them.
        let mut document = serde_json::json!({ "options": 3, "keyboardbindings": "x" });
        let sections = document.as_object_mut().expect("document is an object");
        object_section(sections, "options");
        object_section(sections, "keyboardbindings");
        assert!(sections["options"].is_object());
        assert!(sections["keyboardbindings"].is_object());
    }

    #[test]
    fn numeric_values_clamp_to_their_ranges() {
        let document = serde_json::json!({ "options": {
            "zoomstep": 0,
            "slideshowinterval": 100_000,
        }});
        let options = Options::from_document(&document);
        assert_eq!(options.zoom_step_percent, 1);
        assert_eq!(options.slideshow_interval_seconds, 3600);
    }

    #[test]
    fn in_range_values_are_kept() {
        let document = serde_json::json!({ "options": {
            "titlebartext": 2,
            "scaling": 3,
            "fitmode": 1,
            "preloading": 2,
            "zoomstep": 200,
        }});
        let options = Options::from_document(&document);
        assert_eq!(options.title_bar_text, 2);
        assert_eq!(options.scaling_filter, 3);
        assert_eq!(options.fit_mode, 1);
        assert_eq!(options.preloading, 2);
        assert_eq!(options.zoom_step_percent, 200);
    }
}
