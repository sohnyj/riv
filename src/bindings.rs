//! Keyboard/mouse binding encoding, defaults, and lookup.

use serde_json::{Map, Value};

use crate::actions::Action;

pub const MODIFIER_CONTROL: u8 = 1 << 0;
pub const MODIFIER_SHIFT: u8 = 1 << 1;
pub const MODIFIER_ALT: u8 = 1 << 2;
pub const MODIFIER_META: u8 = 1 << 3;

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

const DEFAULT_KEYBOARD: &[(&str, &[&str])] = &[
    ("open", &["Ctrl+O"]),
    ("reloadfile", &["Ctrl+R", "F5"]),
    ("opencontainingfolder", &["Ctrl+E"]),
    ("showfileinfo", &["I"]),
    ("rename", &["R", "F2"]),
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
    ("fullscreen", &["F", "F11"]),
    ("slideshow", &["S"]),
    ("options", &["Ctrl+,"]),
    ("quit", &["Ctrl+W", "Escape"]),
];

const DEFAULT_MOUSE: &[(&str, &[&str])] = &[
    ("previousfile", &["WheelUp"]),
    ("nextfile", &["WheelDown"]),
    ("zoomin", &["Ctrl+WheelUp"]),
    ("zoomout", &["Ctrl+WheelDown"]),
    ("resetzoom", &["Double+Left"]),
    ("fullscreen", &["Middle"]),
];

impl Bindings {
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

    /// Escape acts as exit-fullscreen only while unbound.
    pub fn escape_is_unbound(&self) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
        !self
            .keyboard
            .iter()
            .any(|binding| binding.virtual_key == VK_ESCAPE.0)
    }
}

pub fn default_keyboard_sequences(action_name: &str) -> &'static [&'static str] {
    DEFAULT_KEYBOARD
        .iter()
        .find(|(name, _)| *name == action_name)
        .map_or(&[], |(_, sequences)| sequences)
}

pub fn default_mouse_encodings(action_name: &str) -> &'static [&'static str] {
    DEFAULT_MOUSE
        .iter()
        .find(|(name, _)| *name == action_name)
        .map_or(&[], |(_, encodings)| encodings)
}

/// None for keys that cannot round-trip through the parser.
pub fn format_key_sequence(modifiers: u8, virtual_key: u16) -> Option<String> {
    let base = key_name_from_virtual_key(virtual_key)?;
    Some(format!("{}{base}", modifier_prefix(modifiers)))
}

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

/// First keyboard sequence plus the mouse binding, comma-joined.
pub fn menu_shortcut_text(
    keyboard_overrides: Option<&Map<String, Value>>,
    mouse_overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Option<String> {
    let keyboard_sequences = resolved_keyboard_sequences(keyboard_overrides, action_name);
    let mouse_encodings = resolved_mouse_encodings(mouse_overrides, action_name);
    let parts: Vec<&str> = keyboard_sequences
        .first()
        .into_iter()
        .chain(mouse_encodings.first())
        .map(String::as_str)
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

pub fn resolved_mouse_encodings(
    overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Vec<String> {
    override_or_default(overrides, action_name, default_mouse_encodings(action_name))
}

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

fn parse_key_sequence(sequence: &str) -> Option<(u8, u16)> {
    let mut modifiers = 0u8;
    let mut virtual_key = None;
    for token in sequence.split('+') {
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
    let mut characters = name.chars();
    if let (Some(character), None) = (characters.next(), characters.next())
        && character.is_ascii_alphanumeric()
    {
        return Some(character.to_ascii_uppercase() as u16);
    }
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

fn key_name_from_virtual_key(virtual_key: u16) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT,
        VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
        VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
        VK_SPACE, VK_TAB, VK_UP,
    };
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
