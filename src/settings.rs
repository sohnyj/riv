//! JSON settings in riv.json next to the exe; defaults are never written.

use std::path::PathBuf;

use serde_json::{Map, Value};

#[derive(Clone, PartialEq)]
pub struct Options {
    pub background_color_enabled: bool,
    pub background_color: (u8, u8, u8),
    pub title_bar_mode: u32,
    pub control_drag_window: bool,
    pub save_window_position: bool,
    pub scaling_filter: u32,
    pub fit_mode: u32,
    pub scale_factor_percent: u32,
    pub dither: u32,
    pub fractional_zoom: bool,
    pub cursor_zoom: bool,
    pub sort_mode: u32,
    pub sort_descending: bool,
    pub preloading_mode: u32,
    pub loop_folders_enabled: bool,
    pub slideshow_reversed: bool,
    pub slideshow_timer_seconds: u32,
    pub after_delete: u32,
    pub ask_delete: bool,
    pub allow_mime_content_detection: bool,
    pub save_recents: bool,
    pub skip_hidden: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            background_color_enabled: true,
            background_color: (0x21, 0x21, 0x21),
            title_bar_mode: 1,
            control_drag_window: true,
            save_window_position: true,
            scaling_filter: 1,
            fit_mode: 0,
            scale_factor_percent: 25,
            dither: 2,
            fractional_zoom: true,
            cursor_zoom: true,
            sort_mode: 0,
            sort_descending: false,
            preloading_mode: 1,
            loop_folders_enabled: true,
            slideshow_reversed: false,
            slideshow_timer_seconds: 5,
            after_delete: 1,
            ask_delete: true,
            allow_mime_content_detection: false,
            save_recents: true,
            skip_hidden: true,
        }
    }
}

impl Options {
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
        let unsigned = |key: &str, fallback: u32| {
            options
                .get(key)
                .and_then(Value::as_u64)
                .map_or(fallback, |value| value as u32)
        };
        // Out-of-range stored values fall back to the default.
        let bounded = |key: &str, maximum: u32, fallback: u32| {
            let value = unsigned(key, fallback);
            if value <= maximum { value } else { fallback }
        };
        Self {
            background_color_enabled: boolean("bgcolorenabled", default.background_color_enabled),
            background_color: options
                .get("bgcolor")
                .and_then(Value::as_str)
                .and_then(parse_hex_color)
                .unwrap_or(default.background_color),
            title_bar_mode: unsigned("titlebarmode", default.title_bar_mode),
            control_drag_window: boolean("ctrldragwindow", default.control_drag_window),
            save_window_position: boolean("savewindowposition", default.save_window_position),
            scaling_filter: unsigned("filteringenabled", default.scaling_filter),
            fit_mode: unsigned("fitmode", default.fit_mode),
            scale_factor_percent: unsigned("scalefactor", default.scale_factor_percent),
            dither: bounded("dither", 2, default.dither),
            fractional_zoom: boolean("fractionalzoom", default.fractional_zoom),
            cursor_zoom: boolean("cursorzoom", default.cursor_zoom),
            sort_mode: bounded("sortmode", 4, default.sort_mode),
            sort_descending: boolean("sortdescending", default.sort_descending),
            preloading_mode: unsigned("preloadingmode", default.preloading_mode),
            loop_folders_enabled: boolean("loopfoldersenabled", default.loop_folders_enabled),
            slideshow_reversed: boolean("slideshowreversed", default.slideshow_reversed),
            slideshow_timer_seconds: unsigned("slideshowtimer", default.slideshow_timer_seconds)
                .clamp(1, 3600),
            after_delete: bounded("afterdelete", 1, default.after_delete),
            ask_delete: boolean("askdelete", default.ask_delete),
            allow_mime_content_detection: boolean(
                "allowmimecontentdetection",
                default.allow_mime_content_detection,
            ),
            save_recents: boolean("saverecents", default.save_recents),
            skip_hidden: boolean("skiphidden", default.skip_hidden),
        }
    }
}

fn format_hex_color((red, green, blue): (u8, u8, u8)) -> String {
    format!("#{red:02X}{green:02X}{blue:02X}")
}

fn parse_hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some((red, green, blue))
}

pub fn probe_writable() -> bool {
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
        }
    }

    pub fn reload(&mut self) -> bool {
        let mut document = read_document(&self.path);
        if let Some(recents) = self.document.get("recents").cloned()
            && let Some(object) = document.as_object_mut()
        {
            object.insert("recents".to_string(), recents);
        }
        let options = Options::from_document(&document);
        let options_changed = options != self.options;
        let bindings_changed = document.get("keyboardbindings")
            != self.document.get("keyboardbindings")
            || document.get("mousebindings") != self.document.get("mousebindings");
        self.document = document;
        self.options = options;
        options_changed || bindings_changed
    }

    /// Atomic save: write a temp file, then rename over.
    pub fn save(&self) -> std::io::Result<()> {
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

    pub fn set_option_boolean(&mut self, key: &str, value: bool) {
        self.document
            .as_object_mut()
            .expect("settings document is object")
            .entry("options")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("options is object")
            .insert(key.to_string(), Value::Bool(value));
        self.options = Options::from_document(&self.document);
    }

    pub fn set_options(&mut self, options: &Options) {
        let default = Options::default();
        let entries: [(&str, Value, Value); 22] = [
            (
                "bgcolorenabled",
                Value::Bool(options.background_color_enabled),
                Value::Bool(default.background_color_enabled),
            ),
            (
                "bgcolor",
                Value::String(format_hex_color(options.background_color)),
                Value::String(format_hex_color(default.background_color)),
            ),
            (
                "titlebarmode",
                Value::from(options.title_bar_mode),
                Value::from(default.title_bar_mode),
            ),
            (
                "ctrldragwindow",
                Value::Bool(options.control_drag_window),
                Value::Bool(default.control_drag_window),
            ),
            (
                "savewindowposition",
                Value::Bool(options.save_window_position),
                Value::Bool(default.save_window_position),
            ),
            (
                "filteringenabled",
                Value::from(options.scaling_filter),
                Value::from(default.scaling_filter),
            ),
            (
                "fitmode",
                Value::from(options.fit_mode),
                Value::from(default.fit_mode),
            ),
            (
                "scalefactor",
                Value::from(options.scale_factor_percent),
                Value::from(default.scale_factor_percent),
            ),
            (
                "dither",
                Value::from(options.dither),
                Value::from(default.dither),
            ),
            (
                "fractionalzoom",
                Value::Bool(options.fractional_zoom),
                Value::Bool(default.fractional_zoom),
            ),
            (
                "cursorzoom",
                Value::Bool(options.cursor_zoom),
                Value::Bool(default.cursor_zoom),
            ),
            (
                "sortmode",
                Value::from(options.sort_mode),
                Value::from(default.sort_mode),
            ),
            (
                "sortdescending",
                Value::Bool(options.sort_descending),
                Value::Bool(default.sort_descending),
            ),
            (
                "preloadingmode",
                Value::from(options.preloading_mode),
                Value::from(default.preloading_mode),
            ),
            (
                "loopfoldersenabled",
                Value::Bool(options.loop_folders_enabled),
                Value::Bool(default.loop_folders_enabled),
            ),
            (
                "slideshowreversed",
                Value::Bool(options.slideshow_reversed),
                Value::Bool(default.slideshow_reversed),
            ),
            (
                "slideshowtimer",
                Value::from(options.slideshow_timer_seconds),
                Value::from(default.slideshow_timer_seconds),
            ),
            (
                "afterdelete",
                Value::from(options.after_delete),
                Value::from(default.after_delete),
            ),
            (
                "askdelete",
                Value::Bool(options.ask_delete),
                Value::Bool(default.ask_delete),
            ),
            (
                "allowmimecontentdetection",
                Value::Bool(options.allow_mime_content_detection),
                Value::Bool(default.allow_mime_content_detection),
            ),
            (
                "saverecents",
                Value::Bool(options.save_recents),
                Value::Bool(default.save_recents),
            ),
            (
                "skiphidden",
                Value::Bool(options.skip_hidden),
                Value::Bool(default.skip_hidden),
            ),
        ];
        let options_object = self
            .document
            .as_object_mut()
            .expect("settings document is object")
            .entry("options")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("options is object");
        for (key, value, default_value) in entries {
            if value == default_value {
                options_object.remove(key);
            } else {
                options_object.insert(key.to_string(), value);
            }
        }
        if !options.save_recents
            && let Some(document) = self.document.as_object_mut()
        {
            document.remove("recents");
        }
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
            let object = document
                .entry(section)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("bindings section is object");
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

    pub fn options_geometry(&self) -> Option<(i32, i32)> {
        let geometry = self.document.get("optionsgeometry")?;
        let read = |key: &str| geometry.get(key)?.as_i64().map(|value| value as i32);
        Some((read("x")?, read("y")?))
    }

    pub fn set_options_geometry(&mut self, x: i32, y: i32) {
        self.document
            .as_object_mut()
            .expect("settings document is object")
            .insert(
                "optionsgeometry".to_string(),
                serde_json::json!({ "x": x, "y": y }),
            );
    }

    pub fn last_file_dialog_directory(&self) -> Option<String> {
        self.document
            .get("recents")?
            .get("lastFileDialogDir")?
            .as_str()
            .map(str::to_string)
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
                "lastFileDialogDir".to_string(),
                Value::String(directory.to_string()),
            );
    }

    pub fn recent_files(&self) -> Vec<(String, String)> {
        self.document
            .get("recents")
            .and_then(|recents| recents.get("recentFiles"))
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
            .insert("recentFiles".to_string(), Value::Array(list));
    }

    pub fn add_recent_file(&mut self, path: &std::path::Path) -> bool {
        if !self.options.save_recents {
            return self.clear_recent_files();
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
            return false;
        }
        files.retain(|(_, existing)| !existing.eq_ignore_ascii_case(&path_text));
        files.insert(0, (name, path_text));
        files.truncate(10);
        self.set_recent_files(&files);
        true
    }

    pub fn prune_recent_files(&mut self) -> bool {
        let files = self.recent_files();
        let pruned: Vec<(String, String)> = files
            .iter()
            .filter(|(_, path)| std::path::Path::new(path).is_file())
            .cloned()
            .collect();
        if pruned.len() == files.len() {
            return false;
        }
        self.set_recent_files(&pruned);
        true
    }

    pub fn clear_recent_files(&mut self) -> bool {
        if self.recent_files().is_empty() {
            return false;
        }
        self.set_recent_files(&[]);
        true
    }
}

fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.to_path_buf()))
        .unwrap_or_default()
        .join("riv.json")
}

fn read_document(path: &PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}
