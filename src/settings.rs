//! JSON 설정 모듈 — exe와 같은 디렉토리의 `riv.json` (SPEC §8.1~8.2)
//!
//! 미설정 키는 기본값을 쓰고 **기본값은 파일에 쓰지 않는다**. recents·지오메트리·
//! 바인딩 등 다른 절은 문서(Value)를 그대로 보존한다. 저장은 임시 파일 쓰기 후
//! 원자 교체. 앱 재활성화 시 재로드해 외부 편집을 반영한다.

use std::path::PathBuf;

use serde_json::{Map, Value};

/// 옵션 스냅샷 — SPEC §8.2 전 항목. 로드 시 기본값으로 채워진다.
/// 일부 필드는 R4~R7에서 소비 예정(항목별 주석 참조).
#[derive(Clone, PartialEq)]
pub struct Options {
    pub background_color_enabled: bool,
    /// (R, G, B) — JSON에는 "#RRGGBB"
    pub background_color: (u8, u8, u8),
    /// 0="riv" 고정 / 1=파일명 / 2="i/n - 파일명" (SPEC §6.1 — 타이틀 반영은 R7)
    pub title_bar_mode: u32,
    pub control_drag_window: bool,
    /// 창 지오메트리 저장/복원 (R7)
    pub save_window_position: bool,
    /// 단일 인스턴스 모드 (SPEC §6.5 — R7)
    pub single_instance: bool,
    /// Scaling: 0=Nearest/1=Bilinear/2=Cubic/3=High Quality (SPEC §3.3)
    pub scaling_filter: u32,
    /// fit 축: 0=Width/1=Height (SPEC §3.2)
    pub fit_mode: u32,
    /// 줌 스텝(%) (SPEC §3.2)
    pub scale_factor_percent: u32,
    pub fractional_zoom: bool,
    pub cursor_zoom: bool,
    pub sort_mode: u32,
    pub sort_descending: bool,
    pub preloading_mode: u32,
    pub loop_folders_enabled: bool,
    /// 슬라이드쇼 (R4)
    pub slideshow_reversed: bool,
    pub slideshow_timer_seconds: f64,
    /// 삭제 후 이동 (R4)
    pub after_delete: u32,
    pub ask_delete: bool,
    pub allow_mime_content_detection: bool,
    /// 최근 파일 (R4)
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
            single_instance: false,
            scaling_filter: 1,
            fit_mode: 0,
            scale_factor_percent: 25,
            fractional_zoom: true,
            cursor_zoom: true,
            sort_mode: 0,
            sort_descending: false,
            preloading_mode: 1,
            loop_folders_enabled: true,
            slideshow_reversed: false,
            slideshow_timer_seconds: 5.0,
            after_delete: 2,
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
            single_instance: boolean("singleinstance", default.single_instance),
            scaling_filter: unsigned("filteringenabled", default.scaling_filter),
            fit_mode: unsigned("fitmode", default.fit_mode),
            scale_factor_percent: unsigned("scalefactor", default.scale_factor_percent),
            fractional_zoom: boolean("fractionalzoom", default.fractional_zoom),
            cursor_zoom: boolean("cursorzoom", default.cursor_zoom),
            sort_mode: unsigned("sortmode", default.sort_mode),
            sort_descending: boolean("sortdescending", default.sort_descending),
            preloading_mode: unsigned("preloadingmode", default.preloading_mode),
            loop_folders_enabled: boolean("loopfoldersenabled", default.loop_folders_enabled),
            slideshow_reversed: boolean("slideshowreversed", default.slideshow_reversed),
            slideshow_timer_seconds: options
                .get("slideshowtimer")
                .and_then(Value::as_f64)
                .unwrap_or(default.slideshow_timer_seconds),
            after_delete: unsigned("afterdelete", default.after_delete),
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

/// (R, G, B) → "#RRGGBB"
fn format_hex_color((red, green, blue): (u8, u8, u8)) -> String {
    format!("#{red:02X}{green:02X}{blue:02X}")
}

/// "#RRGGBB" → (R, G, B)
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

pub struct SettingsFile {
    path: PathBuf,
    /// 파일 문서 전체 — options 외 절(recents·지오메트리·바인딩) 보존용
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

    /// 앱 재활성화 시 재로드 — 외부 편집 반영 (SPEC §8.1).
    /// 반환 = 옵션 변경 여부(변경 시 호출자가 브로드캐스트).
    pub fn reload(&mut self) -> bool {
        let mut document = read_document(&self.path);
        // recents는 앱 소유 상태(외부 편집 대상 아님) — 디바운스 저장 전 유실 방지
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

    /// 원자 저장 — 임시 파일 쓰기 후 교체(std::fs::rename = MoveFileExW REPLACE_EXISTING)
    pub fn save(&self) -> std::io::Result<()> {
        let serialized =
            serde_json::to_string_pretty(&self.document).map_err(std::io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serialized)?;
        std::fs::rename(&temporary, &self.path)
    }

    /// 사용자 재정의 키보드 바인딩: 액션명 → 키 시퀀스 문자열 목록 (SPEC §8.1)
    pub fn keyboard_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get("keyboardbindings")?.as_object()
    }

    /// 사용자 재정의 마우스 바인딩: 액션명 → 마우스 인코딩 문자열 목록 (SPEC §8.1)
    pub fn mouse_bindings(&self) -> Option<&Map<String, Value>> {
        self.document.get("mousebindings")?.as_object()
    }

    /// 옵션 값 기록 + 스냅샷 갱신 — 삭제 확인 "다시 묻지 않기" 등 (SPEC §6.4·§8.2)
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

    /// 옵션 전 항목 기록 (R6 Apply) — 기본값과 같은 키는 제거해 "기본값은 파일에
    /// 쓰지 않음"(SPEC §8.1)을 유지하고, 스냅샷을 갱신한다.
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
                "singleinstance",
                Value::Bool(options.single_instance),
                Value::Bool(default.single_instance),
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
        self.options = Options::from_document(&self.document);
    }

    /// 바인딩 재정의 기록 (R6 Apply) — 각 액션의 확정 목록이 기본값과 같으면 키 제거,
    /// 다르면 대체 목록 기록(빈 배열 = 바인딩 제거). 목록에 없는 키(recent0..9 등)는 보존.
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

    /// 창 지오메트리 (SPEC §6.1·§8.1 루트 windowgeometry) — (x, y, 너비, 높이, 최대화)
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

    /// 옵션 다이얼로그 위치 (SPEC §8.1 루트 optionsgeometry) — (x, y)
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

    /// 파일 열기 다이얼로그 마지막 디렉터리 (SPEC §6.4·§8.1 recents)
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

    // ── 최근 파일 (SPEC §6.4 — 최대 10, 중복 제거, 부재 감사) ────────────────

    /// (표시명, 경로) 목록 — recents.recentFiles
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

    /// 표시 성공 시 호출 — 반환 = 변경 여부(디바운스 저장 트리거).
    /// `saverecents` off면 수집하지 않고 목록을 비운다 (SPEC §6.4).
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

    /// 존재하지 않는 파일 자동 제거 — 메뉴 구성 시 감사 (SPEC §6.4)
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

/// exe와 같은 디렉토리의 riv.json (R4 — 별도 설정 디렉토리 없음).
/// exe 경로 취득 실패는 현실적으로 불가 — 그 경우 작업 디렉토리 상대 경로.
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
