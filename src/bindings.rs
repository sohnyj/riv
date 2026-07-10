//! 키보드/마우스 바인딩 인코딩·기본값·역참조 (SPEC §5.2~5.3)
//!
//! 인코딩은 `[Ctrl+][Shift+][Alt+][Meta+]<Base>`(마우스는 `Double+` 추가).
//! 설정에 있는 액션은 그 목록으로 **대체**(빈 배열 = 바인딩 제거), 없는 액션은
//! 기본값. 충돌(중복 배정) 경고 UI는 R6 단축키 편집에서 — 조회는 선착 매치.

use serde_json::{Map, Value};

use crate::actions::Action;

pub const MODIFIER_CONTROL: u8 = 1 << 0;
pub const MODIFIER_SHIFT: u8 = 1 << 1;
pub const MODIFIER_ALT: u8 = 1 << 2;
pub const MODIFIER_META: u8 = 1 << 3;

/// 마우스 바인딩 베이스 (SPEC §5.3) — Left는 Double 전용(단일 프레스는 팬 예약),
/// 우클릭은 컨텍스트 메뉴 예약이라 베이스에 없음.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseBase {
    Left,
    Middle,
    Back,
    Forward,
    WheelUp,
    WheelDown,
}

struct KeyBinding {
    modifiers: u8,
    virtual_key: u16,
    action: Action,
}

struct MouseBinding {
    modifiers: u8,
    double_click: bool,
    base: MouseBase,
    action: Action,
}

pub struct Bindings {
    keyboard: Vec<KeyBinding>,
    mouse: Vec<MouseBinding>,
}

/// 기본 키보드 바인딩 (SPEC §5.2 — Q4: qView 기본값 계승)
const DEFAULT_KEYBOARD: &[(&str, &[&str])] = &[
    ("open", &["Ctrl+O"]),
    ("reloadfile", &["F5", "R"]),
    ("opencontainingfolder", &["Ctrl+E"]),
    ("showfileinfo", &["I"]),
    ("copy", &["Ctrl+C"]),
    ("paste", &["Ctrl+V"]),
    ("rename", &["F2", "Ctrl+R"]),
    ("delete", &["Delete"]),
    ("deletepermanent", &["Shift+Delete"]),
    ("firstfile", &["Home"]),
    ("lastfile", &["End"]),
    ("previousfile", &["Left"]),
    ("nextfile", &["Right"]),
    ("zoomin", &["="]),
    ("zoomout", &["-"]),
    ("resetzoom", &["Backspace"]),
    ("preservezoom", &["Z"]),
    ("panup", &["Ctrl+Up"]),
    ("pandown", &["Ctrl+Down"]),
    ("panleft", &["Ctrl+Left"]),
    ("panright", &["Ctrl+Right"]),
    ("rotateright", &["Alt+Right"]),
    ("rotateleft", &["Alt+Left"]),
    ("mirror", &["Alt+M"]),
    ("flip", &["Alt+F"]),
    ("pause", &["Space"]),
    ("nextframe", &["N"]),
    ("decreasespeed", &["["]),
    ("resetspeed", &["\\"]),
    ("increasespeed", &["]"]),
    ("fullscreen", &["F11", "F"]),
    ("slideshow", &["S"]),
    ("options", &["Ctrl+,"]),
    ("quit", &["Escape", "Ctrl+W"]),
];

/// 기본 마우스 바인딩 (SPEC §5.3)
const DEFAULT_MOUSE: &[(&str, &[&str])] = &[
    ("previousfile", &["WheelUp"]),
    ("nextfile", &["WheelDown"]),
    ("zoomin", &["Ctrl+WheelUp"]),
    ("zoomout", &["Ctrl+WheelDown"]),
    ("resetzoom", &["Double+Left"]),
    ("fullscreen", &["Middle"]),
];

impl Bindings {
    /// 기본값 + 설정 재정의 병합 (SPEC §8.1 keyboardbindings/mousebindings)
    pub fn from_settings(
        keyboard_overrides: Option<&Map<String, Value>>,
        mouse_overrides: Option<&Map<String, Value>>,
    ) -> Self {
        let mut keyboard = Vec::new();
        for (name, default_sequences) in DEFAULT_KEYBOARD {
            let Some(action) = Action::from_name(name) else {
                continue;
            };
            for sequence in override_or_default(keyboard_overrides, name, default_sequences) {
                if let Some((modifiers, virtual_key)) = parse_key_sequence(&sequence) {
                    keyboard.push(KeyBinding {
                        modifiers,
                        virtual_key,
                        action,
                    });
                }
            }
        }
        // 기본값에 없는 액션(recent0..9 등)의 사용자 바인딩
        if let Some(overrides) = keyboard_overrides {
            for (name, sequences) in overrides {
                if DEFAULT_KEYBOARD.iter().any(|(default, _)| default == name) {
                    continue;
                }
                let Some(action) = Action::from_name(name) else {
                    continue;
                };
                for sequence in string_list(sequences) {
                    if let Some((modifiers, virtual_key)) = parse_key_sequence(&sequence) {
                        keyboard.push(KeyBinding {
                            modifiers,
                            virtual_key,
                            action,
                        });
                    }
                }
            }
        }

        let mut mouse = Vec::new();
        for (name, default_sequences) in DEFAULT_MOUSE {
            let Some(action) = Action::from_name(name) else {
                continue;
            };
            for encoding in override_or_default(mouse_overrides, name, default_sequences) {
                if let Some((modifiers, double_click, base)) = parse_mouse_encoding(&encoding) {
                    mouse.push(MouseBinding {
                        modifiers,
                        double_click,
                        base,
                        action,
                    });
                }
            }
        }
        if let Some(overrides) = mouse_overrides {
            for (name, encodings) in overrides {
                if DEFAULT_MOUSE.iter().any(|(default, _)| default == name) {
                    continue;
                }
                let Some(action) = Action::from_name(name) else {
                    continue;
                };
                for encoding in string_list(encodings) {
                    if let Some((modifiers, double_click, base)) = parse_mouse_encoding(&encoding) {
                        mouse.push(MouseBinding {
                            modifiers,
                            double_click,
                            base,
                            action,
                        });
                    }
                }
            }
        }
        Self { keyboard, mouse }
    }

    pub fn lookup_key(&self, modifiers: u8, virtual_key: u16) -> Option<Action> {
        self.keyboard
            .iter()
            .find(|binding| binding.modifiers == modifiers && binding.virtual_key == virtual_key)
            .map(|binding| binding.action)
    }

    pub fn lookup_mouse(
        &self,
        modifiers: u8,
        double_click: bool,
        base: MouseBase,
    ) -> Option<Action> {
        self.mouse
            .iter()
            .find(|binding| {
                binding.modifiers == modifiers
                    && binding.double_click == double_click
                    && binding.base == base
            })
            .map(|binding| binding.action)
    }

    /// Escape 특례 (SPEC §5.2) — Escape가 어떤 액션에도 안 묶였을 때만
    /// "전체화면 나가기" 전용 키로 동작
    pub fn escape_is_unbound(&self) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
        !self
            .keyboard
            .iter()
            .any(|binding| binding.virtual_key == VK_ESCAPE.0)
    }
}

/// 기본 키 시퀀스 (SPEC §5.2) — 단축키 편집의 Reset to Default·저장 생략 기준
pub fn default_keyboard_sequences(action_name: &str) -> &'static [&'static str] {
    DEFAULT_KEYBOARD
        .iter()
        .find(|(name, _)| *name == action_name)
        .map_or(&[], |(_, sequences)| sequences)
}

/// 기본 마우스 인코딩 (SPEC §5.3) — 동상
pub fn default_mouse_encodings(action_name: &str) -> &'static [&'static str] {
    DEFAULT_MOUSE
        .iter()
        .find(|(name, _)| *name == action_name)
        .map_or(&[], |(_, encodings)| encodings)
}

/// 캡처 결과 → 인코딩 문자열. 이름 없는 가상 키(한/영 전환 등)는 None —
/// `parse_key_sequence`와 왕복 정합이 보장되는 키만 바인딩 허용 (R6 캡처)
pub fn format_key_sequence(modifiers: u8, virtual_key: u16) -> Option<String> {
    let base = key_name_from_virtual_key(virtual_key)?;
    Some(format!("{}{base}", modifier_prefix(modifiers)))
}

/// 캡처 결과 → 마우스 인코딩 문자열 (SPEC §5.3)
pub fn format_mouse_encoding(modifiers: u8, double_click: bool, base: MouseBase) -> String {
    let base_name = match base {
        MouseBase::Left => "Left",
        MouseBase::Middle => "Middle",
        MouseBase::Back => "Back",
        MouseBase::Forward => "Forward",
        MouseBase::WheelUp => "WheelUp",
        MouseBase::WheelDown => "WheelDown",
    };
    format!(
        "{}{}{base_name}",
        modifier_prefix(modifiers),
        if double_click { "Double+" } else { "" }
    )
}

/// 수정자 접두 — 파서·기본값과 같은 순서(Ctrl, Shift, Alt, Meta).
/// 캡처 필드의 진행 중 표시(R6)에도 쓰인다.
pub fn modifier_prefix(modifiers: u8) -> String {
    let mut prefix = String::new();
    if modifiers & MODIFIER_CONTROL != 0 {
        prefix.push_str("Ctrl+");
    }
    if modifiers & MODIFIER_SHIFT != 0 {
        prefix.push_str("Shift+");
    }
    if modifiers & MODIFIER_ALT != 0 {
        prefix.push_str("Alt+");
    }
    if modifiers & MODIFIER_META != 0 {
        prefix.push_str("Meta+");
    }
    prefix
}

/// 액션의 확정 키 시퀀스 목록 — 재정의가 있으면 그 목록, 없으면 기본값.
/// 옵션 다이얼로그 Shortcuts 탭의 초기 상태 (SPEC §8.3)
pub fn resolved_keyboard_sequences(
    overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Vec<String> {
    override_or_default(
        overrides,
        action_name,
        default_keyboard_sequences(action_name),
    )
}

/// 액션의 확정 마우스 인코딩 목록 — 동상
pub fn resolved_mouse_encodings(
    overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Vec<String> {
    override_or_default(overrides, action_name, default_mouse_encodings(action_name))
}

/// 설정 재정의가 있으면 그 목록(빈 배열 = 제거), 없으면 기본값
fn override_or_default(
    overrides: Option<&Map<String, Value>>,
    name: &str,
    defaults: &[&str],
) -> Vec<String> {
    match overrides.and_then(|map| map.get(name)) {
        Some(value) => string_list(value),
        None => defaults.iter().map(|text| (*text).to_string()).collect(),
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// "Ctrl+Shift+F5" → (수정자, 가상 키). 미지원 토큰이 있으면 None.
fn parse_key_sequence(sequence: &str) -> Option<(u8, u16)> {
    let mut modifiers = 0u8;
    let mut virtual_key = None;
    for token in sequence.split('+') {
        // "Ctrl++"(= '+' 키) 같은 빈 토큰은 미지원 — 기본값에 없음
        match token {
            "Ctrl" => modifiers |= MODIFIER_CONTROL,
            "Shift" => modifiers |= MODIFIER_SHIFT,
            "Alt" => modifiers |= MODIFIER_ALT,
            "Meta" => modifiers |= MODIFIER_META,
            base => virtual_key = virtual_key_from_name(base),
        }
    }
    virtual_key.map(|key| (modifiers, key))
}

/// "[수정자+][Double+]<Base>" → (수정자, Double, 베이스) (SPEC §5.3)
fn parse_mouse_encoding(encoding: &str) -> Option<(u8, bool, MouseBase)> {
    let mut modifiers = 0u8;
    let mut double_click = false;
    let mut base = None;
    for token in encoding.split('+') {
        match token {
            "Ctrl" => modifiers |= MODIFIER_CONTROL,
            "Shift" => modifiers |= MODIFIER_SHIFT,
            "Alt" => modifiers |= MODIFIER_ALT,
            "Meta" => modifiers |= MODIFIER_META,
            "Double" => double_click = true,
            "Left" => base = Some(MouseBase::Left),
            "Middle" => base = Some(MouseBase::Middle),
            "Back" => base = Some(MouseBase::Back),
            "Forward" => base = Some(MouseBase::Forward),
            "WheelUp" => base = Some(MouseBase::WheelUp),
            "WheelDown" => base = Some(MouseBase::WheelDown),
            _ => return None,
        }
    }
    let base = base?;
    // Double은 Left 전용, Left는 Double 전용 (단일 프레스 = 팬 드래그 예약)
    if (base == MouseBase::Left) != double_click {
        return None;
    }
    Some((modifiers, double_click, base))
}

fn virtual_key_from_name(name: &str) -> Option<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT,
        VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
        VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
        VK_SPACE, VK_TAB, VK_UP,
    };
    // 단일 영숫자: 가상 키 = ASCII 대문자
    let mut characters = name.chars();
    if let (Some(character), None) = (characters.next(), characters.next())
        && character.is_ascii_alphanumeric()
    {
        return Some(character.to_ascii_uppercase() as u16);
    }
    // F1~F24
    if let Some(number) = name.strip_prefix('F')
        && let Ok(index) = number.parse::<u16>()
        && (1..=24).contains(&index)
    {
        return Some(VK_F1.0 + index - 1);
    }
    let key = match name {
        "Left" => VK_LEFT,
        "Right" => VK_RIGHT,
        "Up" => VK_UP,
        "Down" => VK_DOWN,
        "Home" => VK_HOME,
        "End" => VK_END,
        "PgUp" => VK_PRIOR,
        "PgDown" => VK_NEXT,
        "Space" => VK_SPACE,
        "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "Insert" => VK_INSERT,
        "Escape" => VK_ESCAPE,
        "Enter" | "Return" => VK_RETURN,
        "Tab" => VK_TAB,
        "=" => VK_OEM_PLUS,
        "-" => VK_OEM_MINUS,
        "," => VK_OEM_COMMA,
        "." => VK_OEM_PERIOD,
        ";" => VK_OEM_1,
        "/" => VK_OEM_2,
        "`" => VK_OEM_3,
        "[" => VK_OEM_4,
        "\\" => VK_OEM_5,
        "]" => VK_OEM_6,
        "'" => VK_OEM_7,
        _ => return None,
    };
    Some(key.0)
}

/// 가상 키 → 시퀀스 베이스 이름 — `virtual_key_from_name`의 역 (R6 캡처).
/// 왕복 정합: 여기서 나온 이름은 반드시 같은 가상 키로 파싱된다.
fn key_name_from_virtual_key(virtual_key: u16) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT,
        VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
        VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
        VK_SPACE, VK_TAB, VK_UP,
    };
    // 영숫자: 가상 키 = ASCII 대문자
    if (u16::from(b'0')..=u16::from(b'9')).contains(&virtual_key)
        || (u16::from(b'A')..=u16::from(b'Z')).contains(&virtual_key)
    {
        return Some(char::from(virtual_key as u8).to_string());
    }
    if (VK_F1.0..=VK_F24.0).contains(&virtual_key) {
        return Some(format!("F{}", virtual_key - VK_F1.0 + 1));
    }
    let name = match windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(virtual_key) {
        VK_LEFT => "Left",
        VK_RIGHT => "Right",
        VK_UP => "Up",
        VK_DOWN => "Down",
        VK_HOME => "Home",
        VK_END => "End",
        VK_PRIOR => "PgUp",
        VK_NEXT => "PgDown",
        VK_SPACE => "Space",
        VK_BACK => "Backspace",
        VK_DELETE => "Delete",
        VK_INSERT => "Insert",
        VK_ESCAPE => "Escape",
        VK_RETURN => "Enter",
        VK_TAB => "Tab",
        VK_OEM_PLUS => "=",
        VK_OEM_MINUS => "-",
        VK_OEM_COMMA => ",",
        VK_OEM_PERIOD => ".",
        VK_OEM_1 => ";",
        VK_OEM_2 => "/",
        VK_OEM_3 => "`",
        VK_OEM_4 => "[",
        VK_OEM_5 => "\\",
        VK_OEM_6 => "]",
        VK_OEM_7 => "'",
        _ => return None,
    };
    Some(name.to_string())
}
