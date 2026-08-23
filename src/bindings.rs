//! Keyboard/mouse binding encoding, defaults, lookup, and the live input readers.

use serde_json::{Map, Value};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT,
    VK_LEFT, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
    VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SPACE, VK_TAB, VK_UP,
};

use crate::actions::Action;

pub const MODIFIER_CONTROL: u8 = 1 << 0;
pub const MODIFIER_SHIFT: u8 = 1 << 1;
pub const MODIFIER_ALT: u8 = 1 << 2;
pub const MODIFIER_META: u8 = 1 << 3;

/// Modifier tokens in prefix order; the parsers and the formatter read this one list.
const MODIFIER_NAMES: [(u8, &str); 4] = [
    (MODIFIER_CONTROL, "Ctrl"),
    (MODIFIER_SHIFT, "Shift"),
    (MODIFIER_ALT, "Alt"),
    (MODIFIER_META, "Meta"),
];

fn modifier_from_token(token: &str) -> Option<u8> {
    MODIFIER_NAMES
        .iter()
        .find(|(_, name)| *name == token)
        .map(|(modifier, _)| *modifier)
}

pub fn current_modifiers() -> u8 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    let pressed = |key: VIRTUAL_KEY| unsafe { GetKeyState(i32::from(key.0)) } < 0;
    let mut modifiers = 0u8;
    if pressed(VK_CONTROL) {
        modifiers |= MODIFIER_CONTROL;
    }
    if pressed(VK_SHIFT) {
        modifiers |= MODIFIER_SHIFT;
    }
    if pressed(VK_MENU) {
        modifiers |= MODIFIER_ALT;
    }
    if pressed(VK_LWIN) || pressed(VK_RWIN) {
        modifiers |= MODIFIER_META;
    }
    modifiers
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseBase {
    DoubleClick,
    WheelButton,
    Back,
    Forward,
    WheelUp,
    WheelDown,
}

impl MouseBase {
    /// Recovers a base sent as its discriminant; the capture dialog packs one into a WPARAM.
    pub fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::DoubleClick),
            1 => Some(Self::WheelButton),
            2 => Some(Self::Back),
            3 => Some(Self::Forward),
            4 => Some(Self::WheelUp),
            5 => Some(Self::WheelDown),
            _ => None,
        }
    }

    pub fn index(self) -> u8 {
        self as u8
    }

    /// Every base, walked through the discriminant mapping so the two cannot drift.
    fn all() -> impl Iterator<Item = Self> {
        (0u8..).map_while(Self::from_index)
    }

    /// The encoding token; the match is exhaustive, so a new base cannot go unnamed.
    fn name(self) -> &'static str {
        match self {
            Self::DoubleClick => "Double-click",
            Self::WheelButton => "WheelButton",
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::WheelUp => "WheelUp",
            Self::WheelDown => "WheelDown",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::all().find(|base| base.name() == name)
    }

    /// Wheel messages carry the direction in the sign of their delta.
    pub fn from_wheel_delta(delta: i16) -> Self {
        if delta > 0 {
            Self::WheelUp
        } else {
            Self::WheelDown
        }
    }

    /// The high word of an X button message names which of the two was pressed.
    pub fn from_xbutton_flags(flags: u16) -> Self {
        use windows::Win32::UI::WindowsAndMessaging::XBUTTON2;
        if flags & XBUTTON2 != 0 {
            Self::Forward
        } else {
            Self::Back
        }
    }
}

struct KeyBinding {
    modifiers: u8,
    virtual_key: u16,
    action: Action,
}

struct MouseBinding {
    modifiers: u8,
    base: MouseBase,
    action: Action,
}

pub struct Bindings {
    keyboard: Vec<KeyBinding>,
    mouse: Vec<MouseBinding>,
}

/// How many bindings one action keeps. Reading drops the rest; capturing drops the oldest.
pub const MAXIMUM_KEYBOARD_SEQUENCES: usize = 3;
pub const MAXIMUM_MOUSE_ENCODINGS: usize = 1;

const DEFAULT_KEYBOARD: &[(&str, &[&str])] = &[
    ("open", &["Ctrl+O"]),
    ("pasteurl", &["Ctrl+V"]),
    ("playlist", &["E"]),
    ("loop", &["L"]),
    ("firstfile", &["Home"]),
    ("previousfile", &["Left"]),
    ("nextfile", &["Right"]),
    ("lastfile", &["End"]),
    ("pause", &["spacebar"]),
    ("previousframe", &["B"]),
    ("nextframe", &["N"]),
    ("decreasespeed", &["["]),
    ("increasespeed", &["]"]),
    ("resetspeed", &["\\"]),
    ("showfileinfo", &["I", "Tab"]),
    ("reload", &["Ctrl+R", "F5"]),
    ("togglefitmode", &["V"]),
    ("preservezoom", &["Z"]),
    ("zoomin", &["Up"]),
    ("zoomout", &["Down"]),
    ("togglezoom", &["Enter"]),
    ("panup", &["Ctrl+Up"]),
    ("pandown", &["Ctrl+Down"]),
    ("panleft", &["Ctrl+Left"]),
    ("panright", &["Ctrl+Right"]),
    ("rotateleft", &["Shift+Left"]),
    ("rotateright", &["Shift+Right"]),
    ("mirror", &["Shift+M"]),
    ("flip", &["Shift+F"]),
    ("showinexplorer", &["Ctrl+E"]),
    ("rename", &["R", "F2"]),
    ("delete", &["Delete", "Ctrl+D"]),
    ("deletepermanently", &["Shift+Delete", "Ctrl+Shift+D"]),
    ("toggleslideshow", &["S"]),
    ("settings", &["Ctrl+,"]),
    ("fullscreen", &["F", "F11"]),
    ("alwaysontop", &["T"]),
    ("exit", &["Ctrl+W"]),
];

const DEFAULT_MOUSE: &[(&str, &[&str])] = &[
    ("previousfile", &["WheelUp"]),
    ("nextfile", &["WheelDown"]),
    ("zoomin", &["Ctrl+WheelUp"]),
    ("zoomout", &["Ctrl+WheelDown"]),
    ("togglezoom", &["Double-click"]),
    ("fullscreen", &["WheelButton"]),
];

impl Bindings {
    pub fn from_settings(
        keyboard_overrides: Option<&Map<String, Value>>,
        mouse_overrides: Option<&Map<String, Value>>,
    ) -> Self {
        let keyboard = collect_bindings(
            DEFAULT_KEYBOARD,
            keyboard_overrides,
            MAXIMUM_KEYBOARD_SEQUENCES,
            parse_key_sequence,
        )
        .into_iter()
        .map(|((modifiers, virtual_key), action)| KeyBinding {
            modifiers,
            virtual_key,
            action,
        })
        .collect();
        let mouse = collect_bindings(
            DEFAULT_MOUSE,
            mouse_overrides,
            MAXIMUM_MOUSE_ENCODINGS,
            parse_mouse_encoding,
        )
        .into_iter()
        .map(|((modifiers, base), action)| MouseBinding {
            modifiers,
            base,
            action,
        })
        .collect();
        Self { keyboard, mouse }
    }

    pub fn lookup_key(&self, modifiers: u8, virtual_key: u16) -> Option<Action> {
        self.keyboard
            .iter()
            .find(|binding| binding.modifiers == modifiers && binding.virtual_key == virtual_key)
            .map(|binding| binding.action)
    }

    pub fn lookup_mouse(&self, modifiers: u8, base: MouseBase) -> Option<Action> {
        self.mouse
            .iter()
            .find(|binding| binding.modifiers == modifiers && binding.base == base)
            .map(|binding| binding.action)
    }

    /// Escape acts as exit-fullscreen only while unbound.
    pub fn escape_is_unbound(&self) -> bool {
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

pub fn format_mouse_encoding(modifiers: u8, base: MouseBase) -> String {
    format!("{}{}", modifier_prefix(modifiers), base.name())
}

pub fn modifier_prefix(modifiers: u8) -> String {
    let mut prefix = String::new();
    for (modifier, name) in MODIFIER_NAMES {
        if modifiers & modifier != 0 {
            prefix.push_str(name);
            prefix.push('+');
        }
    }
    prefix
}

pub fn resolved_keyboard_sequences(
    overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Vec<String> {
    // Parser round-trip discards unparseable riv.json strings.
    override_or_default(
        overrides,
        action_name,
        default_keyboard_sequences(action_name),
        MAXIMUM_KEYBOARD_SEQUENCES,
    )
    .into_iter()
    .filter_map(|sequence| {
        let (modifiers, virtual_key) = parse_key_sequence(sequence)?;
        format_key_sequence(modifiers, virtual_key)
    })
    .collect()
}

pub fn menu_shortcut_text(
    keyboard_overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Option<String> {
    resolved_keyboard_sequences(keyboard_overrides, action_name)
        .into_iter()
        .next()
}

pub fn resolved_mouse_encodings(
    overrides: Option<&Map<String, Value>>,
    action_name: &str,
) -> Vec<String> {
    // Parser round-trip discards unparseable riv.json strings.
    override_or_default(
        overrides,
        action_name,
        default_mouse_encodings(action_name),
        MAXIMUM_MOUSE_ENCODINGS,
    )
    .into_iter()
    .filter_map(|encoding| {
        let (modifiers, base) = parse_mouse_encoding(encoding)?;
        Some(format_mouse_encoding(modifiers, base))
    })
    .collect()
}

/// Parsed bindings for one input kind: defaults (overridable) then override-only actions.
fn collect_bindings<T>(
    defaults: &[(&str, &[&str])],
    overrides: Option<&Map<String, Value>>,
    maximum: usize,
    mut parse: impl FnMut(&str) -> Option<T>,
) -> Vec<(T, Action)> {
    let mut collected = Vec::new();
    for (name, default_sequences) in defaults {
        if let Some(action) = Action::from_name(name) {
            for sequence in override_or_default(overrides, name, default_sequences, maximum) {
                if let Some(parsed) = parse(sequence) {
                    collected.push((parsed, action));
                }
            }
        }
    }
    if let Some(overrides) = overrides {
        for (name, sequences) in overrides {
            if defaults.iter().any(|(default, _)| default == name) {
                continue;
            }
            if let Some(action) = Action::from_name(name) {
                for sequence in string_list(sequences).into_iter().take(maximum) {
                    if let Some(parsed) = parse(sequence) {
                        collected.push((parsed, action));
                    }
                }
            }
        }
    }
    collected
}

fn override_or_default<'a>(
    overrides: Option<&'a Map<String, Value>>,
    name: &str,
    defaults: &[&'a str],
    maximum: usize,
) -> Vec<&'a str> {
    let mut resolved = match overrides.and_then(|map| map.get(name)) {
        Some(value) => string_list(value),
        None => defaults.to_vec(),
    };
    resolved.truncate(maximum);
    resolved
}

fn string_list(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn parse_key_sequence(sequence: &str) -> Option<(u8, u16)> {
    let mut modifiers = 0u8;
    let mut virtual_key = None;
    for token in sequence.split('+') {
        match modifier_from_token(token) {
            Some(modifier) => modifiers |= modifier,
            None => virtual_key = virtual_key_from_name(token),
        }
    }
    virtual_key.map(|key| (modifiers, key))
}

fn parse_mouse_encoding(encoding: &str) -> Option<(u8, MouseBase)> {
    let mut modifiers = 0u8;
    let mut base = None;
    for token in encoding.split('+') {
        match modifier_from_token(token) {
            Some(modifier) => modifiers |= modifier,
            None => base = Some(MouseBase::from_name(token)?),
        }
    }
    Some((modifiers, base?))
}

/// Named keys, both directions; a one-sided edit would break the parser round trip.
const KEY_NAMES: [(&str, VIRTUAL_KEY); 26] = [
    ("Left", VK_LEFT),
    ("Right", VK_RIGHT),
    ("Up", VK_UP),
    ("Down", VK_DOWN),
    ("Home", VK_HOME),
    ("End", VK_END),
    ("Page Up", VK_PRIOR),
    ("Page Down", VK_NEXT),
    ("spacebar", VK_SPACE),
    ("Backspace", VK_BACK),
    ("Delete", VK_DELETE),
    ("Insert", VK_INSERT),
    ("Esc", VK_ESCAPE),
    ("Enter", VK_RETURN),
    ("Tab", VK_TAB),
    ("=", VK_OEM_PLUS),
    ("-", VK_OEM_MINUS),
    (",", VK_OEM_COMMA),
    (".", VK_OEM_PERIOD),
    (";", VK_OEM_1),
    ("/", VK_OEM_2),
    ("`", VK_OEM_3),
    ("[", VK_OEM_4),
    ("\\", VK_OEM_5),
    ("]", VK_OEM_6),
    ("'", VK_OEM_7),
];

fn virtual_key_from_name(name: &str) -> Option<u16> {
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
    KEY_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, key)| key.0)
}

fn key_name_from_virtual_key(virtual_key: u16) -> Option<String> {
    if (u16::from(b'0')..=u16::from(b'9')).contains(&virtual_key)
        || (u16::from(b'A')..=u16::from(b'Z')).contains(&virtual_key)
    {
        return Some(char::from(virtual_key as u8).to_string());
    }
    if (VK_F1.0..=VK_F24.0).contains(&virtual_key) {
        return Some(format!("F{}", virtual_key - VK_F1.0 + 1));
    }
    KEY_NAMES
        .iter()
        .find(|(_, key)| key.0 == virtual_key)
        .map(|(name, _)| (*name).to_string())
}

#[cfg(test)]
mod normalization_tests {
    use super::*;

    #[test]
    fn resolved_bindings_round_trip_and_discard_junk() {
        let overrides = serde_json::json!({
            "nextfile": ["Right", "Ctrl+Ctrl+X", "A".repeat(300)],
            "fullscreen": ["WheelButton", "Nope"],
        });
        let map = overrides.as_object().expect("object");
        assert_eq!(
            resolved_keyboard_sequences(Some(map), "nextfile"),
            ["Right", "Ctrl+X"]
        );
        assert_eq!(
            resolved_mouse_encodings(Some(map), "fullscreen"),
            ["WheelButton"]
        );
    }

    #[test]
    fn default_tables_name_actions_in_the_table_order() {
        // collect_bindings skips names from_name cannot resolve, so a typo here silently loses a default.
        let action_names: Vec<&str> = crate::actions::Action::all_bindable()
            .map(|action| action.name())
            .collect();
        for defaults in [DEFAULT_KEYBOARD, DEFAULT_MOUSE] {
            let mut unmatched = action_names.iter();
            for (name, _) in defaults {
                assert!(
                    unmatched.any(|action_name| action_name == name),
                    "{name} is missing from the action table or out of its order"
                );
            }
        }
    }

    #[test]
    fn default_tables_spell_the_parser_round_trip() {
        // An unparseable default drops silently; a non-canonical one becomes a stored override.
        for (name, sequences) in DEFAULT_KEYBOARD {
            for sequence in *sequences {
                let (modifiers, virtual_key) =
                    parse_key_sequence(sequence).unwrap_or_else(|| panic!("{name}: {sequence}"));
                assert_eq!(
                    format_key_sequence(modifiers, virtual_key).as_deref(),
                    Some(*sequence),
                    "{name}"
                );
            }
        }
        for (name, encodings) in DEFAULT_MOUSE {
            for encoding in *encodings {
                let (modifiers, base) =
                    parse_mouse_encoding(encoding).unwrap_or_else(|| panic!("{name}: {encoding}"));
                assert_eq!(format_mouse_encoding(modifiers, base), *encoding, "{name}");
            }
        }
    }

    #[test]
    fn a_hand_written_list_stops_at_the_maximum() {
        let overrides = serde_json::json!({
            "nextfile": ["Right", "Ctrl+A", "Ctrl+B", "Ctrl+C"],
            "fullscreen": ["WheelButton", "Ctrl+WheelUp"],
        });
        let map = overrides.as_object().expect("object");
        assert_eq!(
            resolved_keyboard_sequences(Some(map), "nextfile"),
            ["Right", "Ctrl+A", "Ctrl+B"]
        );
        assert_eq!(
            resolved_mouse_encodings(Some(map), "fullscreen"),
            ["WheelButton"]
        );
        let bindings = Bindings::from_settings(Some(map), Some(map));
        assert!(
            bindings
                .lookup_key(MODIFIER_CONTROL, u16::from(b'B'))
                .is_some()
        );
        assert!(
            bindings
                .lookup_key(MODIFIER_CONTROL, u16::from(b'C'))
                .is_none()
        );
    }
}
