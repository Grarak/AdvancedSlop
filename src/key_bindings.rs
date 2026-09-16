use crate::core::input::Keycode;
use ini::{Properties, SectionSetter};
use std::ffi::CString;

pub const NUM_KEYS: usize = 10;

/// GBA button names in editor row order. Used as the ini keys and the editor row
/// labels. Missing ini keys parse to 0 (unbound), so profiles saved before a key
/// existed stay valid.
///
/// Ten, not the DS's twelve: the GBA has no X or Y.
pub const KEY_NAMES: [&str; NUM_KEYS] = ["A", "B", "Right", "Left", "Up", "Down", "R", "L", "Select", "Start"];

/// The GBA key behind each `KEY_NAMES` row, so `KeyBinding::buttons[i]` drives `KEY_CODES[i]`.
pub const KEY_CODES: [Keycode; NUM_KEYS] = [
    Keycode::A,
    Keycode::B,
    Keycode::Right,
    Keycode::Left,
    Keycode::Up,
    Keycode::Down,
    Keycode::TriggerR,
    Keycode::TriggerL,
    Keycode::Select,
    Keycode::Start,
];

pub const NUM_HOTKEYS: usize = 3;

/// Actions triggered by pressing the bound button. Values index `KeyBinding::hotkeys`
/// and `HOTKEY_NAMES`.
///
/// Only the ones a single-screen console can do: the DS's swap-screens, per-screen
/// scaling, blow-mic and toggle-lid have no GBA meaning.
#[repr(usize)]
#[derive(Copy, Clone)]
pub enum Hotkey {
    PreviousLayout = 0,
    NextLayout = 1,
    Pause = 2,
}

/// Hotkey editor row labels and ini keys, indexed by `Hotkey`. Missing ini keys
/// parse to the default binding, so profiles saved before hotkeys were
/// customizable keep the built-in shortcuts.
pub const HOTKEY_NAMES: [&str; NUM_HOTKEYS] = ["Previous layout", "Next layout", "Pause menu"];

/// A named custom controls profile: for each GBA key and each hotkey, the host input
/// that triggers it, 0 when unbound. Platform-specific in meaning — `SCE_CTRL_*` bits
/// on the Vita, an SDL keycode on Linux — but stored as plain `u32`, so this module
/// stays platform-agnostic. A profile saved on one platform is meaningless on the other.
#[derive(Clone)]
pub struct KeyBinding {
    pub name: String,
    pub buttons: [u32; NUM_KEYS],
    pub hotkeys: [u32; NUM_HOTKEYS],
}

impl KeyBinding {
    pub fn new(name: String, buttons: [u32; NUM_KEYS], hotkeys: [u32; NUM_HOTKEYS]) -> Self {
        KeyBinding { name, buttons, hotkeys }
    }

    pub fn from_ini(name: &str, props: &Properties, default_hotkeys: [u32; NUM_HOTKEYS]) -> Self {
        let mut buttons = [0u32; NUM_KEYS];
        for (i, key_name) in KEY_NAMES.iter().enumerate() {
            buttons[i] = props.get(*key_name).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        }
        let mut hotkeys = [0u32; NUM_HOTKEYS];
        for (i, hotkey_name) in HOTKEY_NAMES.iter().enumerate() {
            hotkeys[i] = props.get(*hotkey_name).and_then(|v| v.parse::<u32>().ok()).unwrap_or(default_hotkeys[i]);
        }
        KeyBinding {
            name: name.to_string(),
            buttons,
            hotkeys,
        }
    }

    pub fn to_ini(&self, section_setter: &mut SectionSetter) {
        for (i, key_name) in KEY_NAMES.iter().enumerate() {
            section_setter.set(*key_name, self.buttons[i].to_string());
        }
        for (i, hotkey_name) in HOTKEY_NAMES.iter().enumerate() {
            section_setter.set(*hotkey_name, self.hotkeys[i].to_string());
        }
    }

    pub fn name_c_str(&self) -> CString {
        CString::new(self.name.clone()).unwrap_or_default()
    }
}

impl Default for KeyBinding {
    fn default() -> Self {
        KeyBinding {
            name: String::new(),
            buttons: [0; NUM_KEYS],
            hotkeys: [0; NUM_HOTKEYS],
        }
    }
}
