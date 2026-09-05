//! JSON settings in riv.json next to the exe; defaults are never written.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::image::core::SortMode;
use crate::view::dither::DitherMode;
use crate::view::renderer::ScalingFilter;
use crate::view::transform::FitMode;

pub const MAXIMUM_RECENT_FILES: usize = 10;

/// Numeric option bounds; the loader clamp and the dialog controls share them.
pub const MINIMUM_ZOOM_STEP_PERCENT: u32 = 1;
pub const MAXIMUM_ZOOM_STEP_PERCENT: u32 = 200;
pub const MINIMUM_SLIDESHOW_INTERVAL_SECONDS: u32 = 1;
pub const MAXIMUM_SLIDESHOW_INTERVAL_SECONDS: u32 = 600;

/// The default window background; used when the custom color is off.
pub const DEFAULT_BACKGROUND_COLOR: (u8, u8, u8) = (0x21, 0x21, 0x21);

/// riv.json option keys; the read and the write name the same constant.
const KEY_BACKGROUND_COLOR_ENABLED: &str = "backgroundcolorenabled";
const KEY_BACKGROUND_COLOR: &str = "backgroundcolor";
const KEY_TITLE_BAR_TEXT: &str = "titlebartext";
const KEY_CONTROL_DRAG_WINDOW: &str = "ctrldragwindow";
const KEY_REMEMBER_WINDOW_PLACEMENT: &str = "rememberwindowplacement";
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

/// riv.json sections and their inner keys; the read and the write name the same constant.
const SECTION_OPTIONS: &str = "options";
const SECTION_RECENTS: &str = "recents";
const SECTION_KEYBOARD_BINDINGS: &str = "keyboardbindings";
const SECTION_MOUSE_BINDINGS: &str = "mousebindings";
const SECTION_WINDOW_PLACEMENT: &str = "windowplacement";
const KEY_RECENT_FILES: &str = "recentfiles";
const KEY_RECENT_FILE_NAME: &str = "name";
const KEY_RECENT_FILE_PATH: &str = "path";
const KEY_LAST_FILE_DIALOG_DIRECTORY: &str = "lastfiledialogdirectory";
const KEY_PLACEMENT_X: &str = "x";
const KEY_PLACEMENT_Y: &str = "y";
const KEY_PLACEMENT_WIDTH: &str = "width";
const KEY_PLACEMENT_HEIGHT: &str = "height";
const KEY_PLACEMENT_MAXIMIZED: &str = "maximized";

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
    pub remember_window_placement: bool,
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
            remember_window_placement: true,
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
        let Some(options) = document.get(SECTION_OPTIONS).and_then(Value::as_object) else {
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
                crate::TitleBarText::IN_SETTING_ORDER.len(),
                default.title_bar_text,
            ),
            control_drag_window: boolean(KEY_CONTROL_DRAG_WINDOW, default.control_drag_window),
            remember_window_placement: boolean(
                KEY_REMEMBER_WINDOW_PLACEMENT,
                default.remember_window_placement,
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
                .clamp(MINIMUM_ZOOM_STEP_PERCENT, MAXIMUM_ZOOM_STEP_PERCENT),
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
            .clamp(
                MINIMUM_SLIDESHOW_INTERVAL_SECONDS,
                MAXIMUM_SLIDESHOW_INTERVAL_SECONDS,
            ),
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
    /// ASCII-lowercased path keys dropped this session; the exit merge must not bring them back.
    removed_recent_keys: HashSet<String>,
}

fn recent_files_of(document: &Value) -> Vec<(String, String)> {
    document
        .get(SECTION_RECENTS)
        .and_then(|recents| recents.get(KEY_RECENT_FILES))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    Some((
                        entry.get(KEY_RECENT_FILE_NAME)?.as_str()?.to_string(),
                        entry.get(KEY_RECENT_FILE_PATH)?.as_str()?.to_string(),
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

    /// Atomic save: write a temporary file, then rename over.
    fn save(&self) -> std::io::Result<()> {
        let serialized =
            serde_json::to_string_pretty(&self.document).map_err(std::io::Error::other)?;
        // Named per process: windows saving at once must not share one half-written file.
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temporary, serialized)?;
        std::fs::rename(&temporary, &self.path)
    }

    pub fn keyboard_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get(SECTION_KEYBOARD_BINDINGS)?.as_object()
    }

    pub fn mouse_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get(SECTION_MOUSE_BINDINGS)?.as_object()
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
        let document = document_object(&mut self.document);
        for (section, resolved, defaults_of) in [
            (
                SECTION_KEYBOARD_BINDINGS,
                keyboard,
                crate::bindings::default_keyboard_sequences as fn(&str) -> &'static [&'static str],
            ),
            (
                SECTION_MOUSE_BINDINGS,
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

    pub fn window_placement(&self) -> Option<(i32, i32, i32, i32, bool)> {
        let placement = self.document.get(SECTION_WINDOW_PLACEMENT)?;
        // A stored value past i32 drops the restore instead of wrapping into it.
        let read = |key: &str| {
            placement
                .get(key)?
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
        };
        let x = read(KEY_PLACEMENT_X)?;
        let y = read(KEY_PLACEMENT_Y)?;
        let width = read(KEY_PLACEMENT_WIDTH).filter(|width| *width > 0)?;
        let height = read(KEY_PLACEMENT_HEIGHT).filter(|height| *height > 0)?;
        // The consumer builds x + width edges, so a sum past i32 drops the restore too.
        x.checked_add(width)?;
        y.checked_add(height)?;
        Some((
            x,
            y,
            width,
            height,
            placement
                .get(KEY_PLACEMENT_MAXIMIZED)
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ))
    }

    pub fn set_window_placement(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        maximized: bool,
    ) {
        document_object(&mut self.document).insert(
            SECTION_WINDOW_PLACEMENT.to_string(),
            serde_json::json!({
                KEY_PLACEMENT_X: x,
                KEY_PLACEMENT_Y: y,
                KEY_PLACEMENT_WIDTH: width,
                KEY_PLACEMENT_HEIGHT: height,
                KEY_PLACEMENT_MAXIMIZED: maximized,
            }),
        );
    }

    pub fn last_file_dialog_directory(&self) -> Option<&str> {
        self.document
            .get(SECTION_RECENTS)?
            .get(KEY_LAST_FILE_DIALOG_DIRECTORY)?
            .as_str()
    }

    pub fn set_last_file_dialog_directory(&mut self, directory: &str) {
        object_section(document_object(&mut self.document), SECTION_RECENTS).insert(
            KEY_LAST_FILE_DIALOG_DIRECTORY.to_string(),
            Value::String(directory.to_string()),
        );
    }

    pub fn recent_files(&self) -> Vec<(String, String)> {
        let mut files = recent_files_of(&self.document);
        // A hand-edited document can exceed the maximum; every reader sees at most that many.
        files.truncate(MAXIMUM_RECENT_FILES);
        files
    }

    /// Whether the list holds anything, for callers that would drop the list they built.
    pub fn has_recent_files(&self) -> bool {
        self.document
            .get(SECTION_RECENTS)
            .and_then(|recents| recents.get(KEY_RECENT_FILES))
            .and_then(Value::as_array)
            .is_some_and(|list| !list.is_empty())
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
        } else {
            // set_last_file_dialog_directory can re-insert the section after write_options dropped it.
            document_object(&mut self.document).remove(SECTION_RECENTS);
        }
        self.save()
    }

    fn set_recent_files(&mut self, files: &[(String, String)]) {
        let list: Vec<Value> = files
            .iter()
            .map(|(name, path)| {
                serde_json::json!({ KEY_RECENT_FILE_NAME: name, KEY_RECENT_FILE_PATH: path })
            })
            .collect();
        object_section(document_object(&mut self.document), SECTION_RECENTS)
            .insert(KEY_RECENT_FILES.to_string(), Value::Array(list));
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

    /// Drops the entry and records its key so the exit merge cannot restore it.
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
        if self.has_recent_files() {
            self.set_recent_files(&[]);
        }
    }
}

fn settings_path() -> PathBuf {
    std::env::current_exe()
        .expect("the running module always has a path")
        .parent()
        .expect("the executable path always names a directory")
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
            KEY_REMEMBER_WINDOW_PLACEMENT,
            Value::Bool(options.remember_window_placement),
            Value::Bool(default.remember_window_placement),
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
    let document = document_object(document);
    let options_object = object_section(document, SECTION_OPTIONS);
    for (key, value, default_value) in entries {
        if value == default_value {
            options_object.remove(key);
        } else {
            options_object.insert(key.to_string(), value);
        }
    }
    if !options.remember_recents {
        document.remove(SECTION_RECENTS);
    }
}

/// The document root; read_document only ever produces an object.
fn document_object(document: &mut Value) -> &mut Map<String, Value> {
    document
        .as_object_mut()
        .expect("settings document is object")
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

/// True when the settings file is there but unusable; loading it starts from defaults instead.
pub fn settings_document_is_unreadable() -> bool {
    document_is_unreadable(&settings_path())
}

fn document_is_unreadable(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            !serde_json::from_str::<Value>(&text).is_ok_and(|document| document.is_object())
        }
        // A missing file is the ordinary first run; any other error is a file that will not open.
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
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
        assert_eq!(options.slideshow_interval_seconds, 600);
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

#[cfg(test)]
mod document_readability_tests {
    use super::*;

    fn document_at(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes).expect("document file");
        path
    }

    #[test]
    fn a_present_document_is_unreadable_unless_it_is_a_json_object() {
        let cases: [(&str, &[u8], bool); 5] = [
            ("riv-readability-object.json", br#"{"options": {}}"#, false),
            (
                "riv-readability-truncated.json",
                br#"{"options": {"zoomstep": "#,
                true,
            ),
            ("riv-readability-array.json", b"[]", true),
            ("riv-readability-empty.json", b"", true),
            // UTF-16 with a byte order mark, which PowerShell 5's Out-File writes by default.
            ("riv-readability-utf16.json", b"\xff\xfe{\x00}\x00", true),
        ];
        for (name, bytes, unreadable) in cases {
            let path = document_at(name, bytes);
            assert_eq!(document_is_unreadable(&path), unreadable, "{name}");
            let _ = std::fs::remove_file(&path);
        }
        // Absent is the ordinary first run, not a broken file.
        let missing = std::env::temp_dir().join("riv-readability-missing.json");
        assert!(!document_is_unreadable(&missing));
    }

    #[test]
    fn mistyped_values_read_as_defaults() {
        // Each value differs from its default and has a JSON type the reader does not take.
        let document = serde_json::json!({ "options": {
            "zoomstep": "50",
            "cursorzoom": 0,
            "slideshowinterval": -5,
            "slideshowdirection": 0.0,
            "backgroundcolor": 0x112233,
            "sortdescending": "true",
        }});
        assert!(Options::from_document(&document) == Options::default());
    }
}

#[cfg(test)]
mod option_round_trip_tests {
    use super::*;

    #[test]
    fn every_field_is_read_back_as_written() {
        // A full literal: a new field fails here at compile time, a missing write row at run time.
        let stored = Options {
            background_color_enabled: true,
            background_color: (0x11, 0x22, 0x33),
            title_bar_text: 2,
            control_drag_window: false,
            remember_window_placement: false,
            hide_cursor_fullscreen: false,
            scaling_filter: 2,
            fit_mode: 1,
            zoom_step_percent: 50,
            dither_mode: 0,
            fractional_wheel_zoom: false,
            cursor_zoom: false,
            sort_files_by: 2,
            sort_descending: true,
            preloading: 2,
            loop_within_folder: false,
            slideshow_direction: 0,
            slideshow_interval_seconds: 10,
            after_deletion: 0,
            ask_delete: false,
            detect_format_by_content: true,
            remember_recents: false,
            skip_hidden: false,
        };
        let mut document = serde_json::json!({});
        write_options(&mut document, &stored);
        let read = Options::from_document(&document);
        assert_eq!(
            read.background_color_enabled,
            stored.background_color_enabled
        );
        assert_eq!(read.background_color, stored.background_color);
        assert_eq!(read.title_bar_text, stored.title_bar_text);
        assert_eq!(read.control_drag_window, stored.control_drag_window);
        assert_eq!(
            read.remember_window_placement,
            stored.remember_window_placement
        );
        assert_eq!(read.hide_cursor_fullscreen, stored.hide_cursor_fullscreen);
        assert_eq!(read.scaling_filter, stored.scaling_filter);
        assert_eq!(read.fit_mode, stored.fit_mode);
        assert_eq!(read.zoom_step_percent, stored.zoom_step_percent);
        assert_eq!(read.dither_mode, stored.dither_mode);
        assert_eq!(read.fractional_wheel_zoom, stored.fractional_wheel_zoom);
        assert_eq!(read.cursor_zoom, stored.cursor_zoom);
        assert_eq!(read.sort_files_by, stored.sort_files_by);
        assert_eq!(read.sort_descending, stored.sort_descending);
        assert_eq!(read.preloading, stored.preloading);
        assert_eq!(read.loop_within_folder, stored.loop_within_folder);
        assert_eq!(read.slideshow_direction, stored.slideshow_direction);
        assert_eq!(
            read.slideshow_interval_seconds,
            stored.slideshow_interval_seconds
        );
        assert_eq!(read.after_deletion, stored.after_deletion);
        assert_eq!(read.ask_delete, stored.ask_delete);
        assert_eq!(
            read.detect_format_by_content,
            stored.detect_format_by_content
        );
        assert_eq!(read.remember_recents, stored.remember_recents);
        assert_eq!(read.skip_hidden, stored.skip_hidden);
    }

    #[test]
    fn default_values_write_no_keys() {
        let mut document = serde_json::json!({});
        write_options(&mut document, &Options::default());
        let options = document
            .get("options")
            .and_then(Value::as_object)
            .expect("options object");
        assert!(options.is_empty());
    }
}

#[cfg(test)]
mod window_placement_bounds_tests {
    use super::*;

    fn settings_with(document: serde_json::Value) -> SettingsFile {
        SettingsFile {
            path: PathBuf::new(),
            document,
            options: Options::default(),
            removed_recent_keys: HashSet::new(),
        }
    }

    #[test]
    fn a_placement_past_i32_drops_the_restore_instead_of_wrapping() {
        let stored = settings_with(serde_json::json!({ "windowplacement": {
            "x": 100, "y": -200, "width": 640, "height": 480, "maximized": true } }));
        assert_eq!(stored.window_placement(), Some((100, -200, 640, 480, true)));
        // 2^32 used to fold to x = 0 and restore a place never saved.
        let wrapped = settings_with(serde_json::json!({ "windowplacement": {
            "x": 4_294_967_296i64, "y": 0, "width": 640, "height": 480 } }));
        assert_eq!(wrapped.window_placement(), None);
        // 2^32 + 1 used to fold to width 1 and pass the positive filter.
        let folded = settings_with(serde_json::json!({ "windowplacement": {
            "x": 0, "y": 0, "width": 4_294_967_297i64, "height": 480 } }));
        assert_eq!(folded.window_placement(), None);
    }

    #[test]
    fn a_placement_sum_past_i32_drops_the_restore() {
        // Each field fits i32, but the right edge x + width the consumer builds does not.
        let summed = settings_with(serde_json::json!({ "windowplacement": {
            "x": 2_000_000_000, "y": 0, "width": 2_000_000_000, "height": 480 } }));
        assert_eq!(summed.window_placement(), None);
        let summed_vertical = settings_with(serde_json::json!({ "windowplacement": {
            "x": 0, "y": 2_000_000_000, "width": 640, "height": 2_000_000_000 } }));
        assert_eq!(summed_vertical.window_placement(), None);
    }
}

#[cfg(test)]
mod recents_save_tests {
    use super::*;

    #[test]
    fn saving_with_recents_off_drops_the_section() {
        let path = std::env::temp_dir().join("riv-recents-off.json");
        let _ = std::fs::remove_file(&path);
        let mut settings = SettingsFile {
            path: path.clone(),
            document: serde_json::json!({}),
            options: Options {
                remember_recents: false,
                ..Options::default()
            },
            removed_recent_keys: HashSet::new(),
        };
        // The open dialog records its directory without consulting the option.
        settings.set_last_file_dialog_directory("C:\\pictures");
        settings.save_merging_recents().expect("save");
        let saved = read_document(&path);
        let _ = std::fs::remove_file(&path);
        assert!(saved.get("recents").is_none());
    }
}

#[cfg(test)]
mod stored_key_tests {
    use super::*;

    #[test]
    fn a_hand_written_document_reads_every_documented_key() {
        // Literal spellings: the round-trip tests share constants, so only this catches a rename.
        let document = serde_json::json!({ "options": {
            "backgroundcolorenabled": true,
            "backgroundcolor": "#112233",
            "titlebartext": 2,
            "ctrldragwindow": false,
            "rememberwindowplacement": false,
            "hidecursorfullscreen": false,
            "scaling": 2,
            "fitmode": 1,
            "zoomstep": 50,
            "dither": 0,
            "fractionalwheelzoom": false,
            "cursorzoom": false,
            "sortfilesby": 2,
            "sortdescending": true,
            "preloading": 2,
            "loopwithinfolder": false,
            "slideshowdirection": 0,
            "slideshowinterval": 10,
            "afterdeletion": 0,
            "askdelete": false,
            "detectformatbycontent": true,
            "rememberrecents": false,
            "skiphidden": false,
        }});
        let expected = Options {
            background_color_enabled: true,
            background_color: (0x11, 0x22, 0x33),
            title_bar_text: 2,
            control_drag_window: false,
            remember_window_placement: false,
            hide_cursor_fullscreen: false,
            scaling_filter: 2,
            fit_mode: 1,
            zoom_step_percent: 50,
            dither_mode: 0,
            fractional_wheel_zoom: false,
            cursor_zoom: false,
            sort_files_by: 2,
            sort_descending: true,
            preloading: 2,
            loop_within_folder: false,
            slideshow_direction: 0,
            slideshow_interval_seconds: 10,
            after_deletion: 0,
            ask_delete: false,
            detect_format_by_content: true,
            remember_recents: false,
            skip_hidden: false,
        };
        assert!(Options::from_document(&document) == expected);
    }

    #[test]
    fn defaults_match_the_documented_values() {
        let expected = Options {
            background_color_enabled: false,
            background_color: (0x21, 0x21, 0x21),
            title_bar_text: 1,
            control_drag_window: true,
            remember_window_placement: true,
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
        };
        assert!(Options::default() == expected);
    }

    #[test]
    fn renamed_keys_read_as_defaults() {
        let document = serde_json::json!({ "options": {
            "scalefactor": 3,
            "slideshowtimer": 9,
            "allowmimecontentdetection": true,
            "saverecents": false,
            "filteringenabled": 3,
            "loopfoldersenabled": false,
            "fractionalzoom": false,
            "slideshowreversed": true,
        }});
        assert!(Options::from_document(&document) == Options::default());
    }

    #[test]
    fn binding_sections_read_by_their_stored_names() {
        let settings = SettingsFile {
            path: PathBuf::new(),
            document: serde_json::json!({
                "keyboardbindings": { "reload": ["F5"] },
                "mousebindings": { "togglezoom": ["Double-click"] },
            }),
            options: Options::default(),
            removed_recent_keys: HashSet::new(),
        };
        assert!(
            settings
                .keyboard_bindings()
                .is_some_and(|map| map.contains_key("reload"))
        );
        assert!(
            settings
                .mouse_bindings()
                .is_some_and(|map| map.contains_key("togglezoom"))
        );
    }
}

#[cfg(test)]
mod recent_files_tests {
    use super::*;

    fn in_memory() -> SettingsFile {
        SettingsFile {
            path: PathBuf::new(),
            document: serde_json::json!({}),
            options: Options::default(),
            removed_recent_keys: HashSet::new(),
        }
    }

    #[test]
    fn recents_keep_ten_newest_without_duplicates() {
        let mut settings = in_memory();
        for index in 0..12 {
            settings.add_recent_file(Path::new(&format!("C:\\p\\{index}.png")));
        }
        let files = settings.recent_files();
        assert_eq!(files.len(), MAXIMUM_RECENT_FILES);
        assert_eq!(files[0].1, "C:\\p\\11.png");
        // A re-added path moves to the front once, compared without case.
        settings.add_recent_file(Path::new("C:\\P\\9.PNG"));
        let files = settings.recent_files();
        assert_eq!(files.len(), MAXIMUM_RECENT_FILES);
        assert_eq!(files[0].1, "C:\\P\\9.PNG");
        let copies = files
            .iter()
            .filter(|(_, path)| path.eq_ignore_ascii_case("C:\\p\\9.png"))
            .count();
        assert_eq!(copies, 1);
        // Re-adding the head is a no-op that keeps the stored spelling.
        settings.add_recent_file(Path::new("C:\\p\\9.png"));
        assert_eq!(settings.recent_files()[0].1, "C:\\P\\9.PNG");
    }

    #[test]
    fn a_hand_edited_list_truncates_on_read() {
        let entries: Vec<Value> = (0..12)
            .map(|index| serde_json::json!({ "name": format!("{index}.png"), "path": format!("C:\\p\\{index}.png") }))
            .collect();
        let mut settings = in_memory();
        settings.document = serde_json::json!({ "recents": { "recentfiles": entries } });
        assert_eq!(settings.recent_files().len(), MAXIMUM_RECENT_FILES);
    }

    #[test]
    fn the_exit_merge_unions_without_bringing_back_removals() {
        let path = std::env::temp_dir().join("riv-recents-merge.json");
        let disk = serde_json::json!({ "recents": { "recentfiles": [
            { "name": "a.png", "path": "C:\\d\\a.png" },
            { "name": "b.png", "path": "C:\\d\\b.png" },
        ]}});
        std::fs::write(&path, serde_json::to_string(&disk).expect("serialize")).expect("write");
        let mut settings = in_memory();
        settings.path = path.clone();
        settings.add_recent_file(Path::new("C:\\d\\c.png"));
        settings.add_recent_file(Path::new("C:\\d\\B.png"));
        settings.remove_recent_file(Path::new("C:\\d\\a.png"));
        settings.save_merging_recents().expect("save");
        let saved = read_document(&path);
        let _ = std::fs::remove_file(&path);
        let paths: Vec<String> = recent_files_of(&saved)
            .into_iter()
            .map(|(_, stored)| stored)
            .collect();
        // This session first, the disk's b folded into the session's B, the removed a gone.
        assert_eq!(paths, ["C:\\d\\B.png", "C:\\d\\c.png"]);
    }
}
