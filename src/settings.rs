use crate::key_bindings::KeyBinding;
use ini::Ini;
use lazy_static::lazy_static;
use std::fmt::{Debug, Display, Formatter};
use std::hint::unreachable_unchecked;
use std::path::PathBuf;
use std::str::FromStr;
use strum::EnumCount;
use strum_macros::{EnumCount, EnumIter, FromRepr, IntoStaticStr};

fn framelimit_value() -> SettingValue {
    const VALUES: [&str; 10] = ["off", "100%", "125%", "150%", "175%", "200%", "250%", "300%", "400%", "500%"];
    SettingValue::List(ListInner::new(1, VALUES.into_iter().map(|value| value.to_string()).collect()))
}

fn screen_layout_value() -> SettingValue {
    SettingValue::List(ListInner::new(0, crate::screen_layout::NAMES.into_iter().map(|value| value.to_string()).collect()))
}

#[derive(Clone)]
pub struct ListInner {
    pub selection: usize,
    pub values: Vec<String>,
    initial_selection: String,
}

impl ListInner {
    pub fn new(selection: usize, values: Vec<String>) -> Self {
        ListInner {
            initial_selection: if selection >= values.len() { "".to_string() } else { values[selection].clone() },
            selection,
            values,
        }
    }

    fn reset_to_initial_selection(&mut self) {
        self.selection = self.values.iter().position(|value| self.initial_selection == *value).unwrap_or(0)
    }
}

#[derive(Clone)]
pub struct SliderInner {
    pub value: i32,
    pub min: i32,
    pub max: i32,
}

impl SliderInner {
    pub const fn new(value: i32, min: i32, max: i32) -> Self {
        SliderInner { value, min, max }
    }
}

#[derive(Clone)]
pub enum SettingValue {
    Bool(bool),
    List(ListInner),
    Int(usize),
    Slider(SliderInner),
}

impl SettingValue {
    pub fn next(&mut self) {
        match self {
            SettingValue::Bool(value) => *value ^= true,
            SettingValue::List(inner) => inner.selection = (inner.selection + 1) % inner.values.len(),
            SettingValue::Int(_) => {}
            SettingValue::Slider(_) => {}
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SettingValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool_mut(&mut self) -> Option<&mut bool> {
        match self {
            SettingValue::Bool(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<(usize, &Vec<String>)> {
        match self {
            SettingValue::List(inner) => Some((inner.selection, &inner.values)),
            _ => None,
        }
    }

    pub fn as_list_mut(&mut self) -> Option<(&mut usize, &mut Vec<String>)> {
        match self {
            SettingValue::List(inner) => Some((&mut inner.selection, &mut inner.values)),
            _ => None,
        }
    }

    pub fn as_slider(&self) -> Option<&SliderInner> {
        match self {
            SettingValue::Slider(inner) => Some(inner),
            _ => None,
        }
    }

    fn parse_str(&mut self, str: &str) {
        match self {
            SettingValue::Bool(value) => *value = bool::from_str(str).unwrap_or(false),
            SettingValue::List(inner) => {
                inner.initial_selection = str.to_string();
                inner.reset_to_initial_selection();
            }
            SettingValue::Int(_) => {}
            SettingValue::Slider(inner) => {
                if let Ok(value) = i32::from_str(str) {
                    inner.value = value.clamp(inner.min, inner.max);
                }
            }
        }
    }

    fn to_parse_string(&self) -> String {
        match self {
            SettingValue::Bool(value) => value.to_string(),
            // Unpopulated lists (e.g. Controls before profiles load) have no values
            SettingValue::List(inner) => inner.values.get(inner.selection).cloned().unwrap_or_else(|| inner.initial_selection.clone()),
            SettingValue::Int(value) => value.to_string(),
            SettingValue::Slider(inner) => inner.value.to_string(),
        }
    }
}

impl Display for SettingValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                SettingValue::Bool(value) => if *value { "on" } else { "off" }.to_string(),
                SettingValue::List(inner) => inner.values[inner.selection].clone(),
                SettingValue::Int(value) => value.to_string(),
                SettingValue::Slider(inner) => inner.value.to_string(),
            }
        )
    }
}

// One tab per group in the settings ui, in declaration order.
#[derive(Copy, Clone, EnumIter, Eq, IntoStaticStr, PartialEq)]
pub enum SettingGroup {
    Emulation,
    Screen,
    System,
}

#[derive(Clone)]
pub struct Setting {
    pub title: &'static str,
    pub description: &'static str,
    pub value: SettingValue,
    pub runtime: bool,
    pub group: SettingGroup,
}

impl Setting {
    const fn new(title: &'static str, description: &'static str, value: SettingValue, runtime: bool, group: SettingGroup) -> Self {
        Setting {
            title,
            description,
            value,
            runtime,
            group,
        }
    }
}

lazy_static! {
    // Built straight from SettingId, so the array order always matches the index
    // enum: element `i` is `SettingId::from_repr(i)`. Add/reorder a setting in one
    // place (the enum + its `definition()` arm) and everything stays in sync.
    pub static ref DEFAULT_SETTINGS: Settings = Settings(std::array::from_fn(|i| SettingId::from_repr(i).unwrap().definition()));
}

#[derive(Clone)]
pub struct Settings([Setting; SettingId::COUNT]);

#[repr(usize)]
#[derive(Copy, Clone, EnumCount, FromRepr)]
pub(crate) enum SettingId {
    Framelimit,
    Audio,
    AudioStretching,
    ScreenLayout,
    Controls,
    JoystickAsDpad,
    ShowDebugStatistics,
    Retroachievements,
    SavestateOnExit,
    Rewind,
}

impl SettingId {
    pub(crate) fn definition(self) -> Setting {
        match self {
            SettingId::Framelimit => Setting::new("Framelimit", "Caps the emulation speed relative to real hardware. Set to 'off' to run as fast as possible.", framelimit_value(), true, SettingGroup::Emulation),
            SettingId::Audio => Setting::new("Audio", "Turn audio off for a small performance boost.", SettingValue::Bool(true), true, SettingGroup::Emulation),
            SettingId::AudioStretching => Setting::new("Audio stretching", "Stretches audio to prevent crackling when a game runs below full speed. Adds a little latency.", SettingValue::Bool(true), true, SettingGroup::Emulation),
            SettingId::ScreenLayout => Setting::new(
                "Screen layout",
                {
                    #[cfg(target_os = "vita")]
                    {
                        "How the game screen fits the display. Fit keeps the aspect ratio, Integer scales by whole pixels for the sharpest image, Stretch fills the whole screen. In-game: Square cycles layouts."
                    }
                    #[cfg(not(target_os = "vita"))]
                    {
                        "How the game screen fits the display. Fit keeps the aspect ratio, Integer scales by whole pixels for the sharpest image, Stretch fills the whole screen. In-game: F12 cycles layouts."
                    }
                },
                screen_layout_value(),
                true,
                SettingGroup::Screen,
            ),
            SettingId::Controls => Setting::new("Controls", "Custom button mapping to use. Create profiles under Global settings.", SettingValue::List(ListInner::new(0, vec![])), true, SettingGroup::System),
            SettingId::JoystickAsDpad => Setting::new("Joystick as D-Pad", "Use the left analog stick as the D-Pad.", SettingValue::Bool(true), true, SettingGroup::System),
            SettingId::SavestateOnExit => Setting::new(
                "Savestate on exit",
                "Write a savestate when you quit a game, so it can be resumed from the game's page in the browser.",
                SettingValue::Bool(true),
                true,
                SettingGroup::System,
            ),
            SettingId::ShowDebugStatistics => Setting::new("Show debug statistics", "Show FPS and other debug information while playing.", SettingValue::Bool(true), true, SettingGroup::System),
            SettingId::Rewind => Setting::new(
                "Rewind",
                "Hold the rewind button to step back through the last few seconds. Costs memory and a little speed while enabled.",
                SettingValue::Bool(false),
                true,
                SettingGroup::Emulation,
            ),
            SettingId::Retroachievements => Setting::new(
                "RetroAchievements",
                "Unlock achievements in supported games. Needs an account and an internet connection; log in from the settings menu.",
                SettingValue::Bool(false),
                true,
                SettingGroup::System,
            ),
        }
    }
}

impl Settings {
    pub fn joystick_as_dpad(&self) -> bool {
        unsafe { self.0[SettingId::JoystickAsDpad as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn controls_index(&self) -> usize {
        unsafe { self.0[SettingId::Controls as usize].value.as_list().unwrap_unchecked().0 }
    }

    pub fn populate_controls(&mut self, default_binding: &KeyBinding, bindings: &[KeyBinding]) {
        let (_, values) = unsafe { self.0[SettingId::Controls as usize].value.as_list_mut().unwrap_unchecked() };
        let first_population = values.is_empty();
        values.clear();
        values.push(default_binding.name.clone());
        for binding in bindings {
            values.push(binding.name.clone());
        }
        if first_population {
            match &mut self.0[SettingId::Controls as usize].value {
                SettingValue::List(inner) => inner.reset_to_initial_selection(),
                _ => unsafe { unreachable_unchecked() },
            }
        }
    }

    /// Rebuild the screen-layout list as the built-in names followed by the saved
    /// custom ones, so an index past NAMES selects a custom layout.
    pub fn populate_screen_layouts(&mut self, customs: &[crate::screen_layout::CustomLayout]) {
        let (_, values) = unsafe { self.0[SettingId::ScreenLayout as usize].value.as_list_mut().unwrap_unchecked() };
        values.truncate(crate::screen_layout::NAMES.len());
        for layout in customs {
            values.push(layout.name.clone());
        }
    }

    pub fn framelimit(&self) -> u8 {
        unsafe { self.0[SettingId::Framelimit as usize].value.as_list().unwrap_unchecked().0 as u8 }
    }

    pub fn audio(&self) -> bool {
        unsafe { self.0[SettingId::Audio as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn audio_stretching(&self) -> bool {
        unsafe { self.0[SettingId::AudioStretching as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn show_debug_stats(&self) -> bool {
        unsafe { self.0[SettingId::ShowDebugStatistics as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn savestate_on_exit(&self) -> bool {
        unsafe { self.0[SettingId::SavestateOnExit as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn rewind(&self) -> bool {
        unsafe { self.0[SettingId::Rewind as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn retroachievements(&self) -> bool {
        unsafe { self.0[SettingId::Retroachievements as usize].value.as_bool().unwrap_unchecked() }
    }

    pub fn screen_layout(&self) -> usize {
        unsafe { self.0[SettingId::ScreenLayout as usize].value.as_list().unwrap_unchecked().0 }
    }

    /// Steps to the next layout, wrapping — the in-game hotkey.
    pub fn cycle_screen_layout(&mut self) {
        self.0[SettingId::ScreenLayout as usize].value.next();
    }

    pub fn set_framelimit(&mut self, value: u8) {
        *self.0[SettingId::Framelimit as usize].value.as_list_mut().unwrap().0 = value as usize;
    }

    pub fn set_audio(&mut self, value: bool) {
        *self.0[SettingId::Audio as usize].value.as_bool_mut().unwrap() = value;
    }

    pub fn get_all_mut(&mut self) -> &mut [Setting] {
        &mut self.0
    }
}

impl Debug for Settings {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_map();
        for setting in &self.0 {
            list.key(&setting.title).value(&setting.value.to_string());
        }
        list.finish()
    }
}

pub struct SettingsConfig {
    pub settings: Settings,
    pub settings_file_path: PathBuf,
    pub dirty: bool,
}

impl From<Settings> for SettingsConfig {
    fn from(value: Settings) -> Self {
        SettingsConfig {
            settings: value,
            settings_file_path: PathBuf::new(),
            dirty: false,
        }
    }
}

impl SettingsConfig {
    pub fn new(path: PathBuf) -> Self {
        let mut settings = DEFAULT_SETTINGS.clone();

        if let Ok(ini) = Ini::load_from_file(&path) {
            if let Some(section) = ini.section(None::<String>) {
                for setting in settings.get_all_mut() {
                    if let Some(value) = section.get(setting.title) {
                        setting.value.parse_str(value);
                    }
                }
            }
        }

        SettingsConfig {
            settings,
            settings_file_path: path,
            dirty: false,
        }
    }

    pub fn flush(&mut self) {
        if self.dirty {
            let mut ini = Ini::new();
            let mut section = ini.with_section(None::<String>);
            for setting in self.settings.get_all_mut() {
                section.set(setting.title, setting.value.to_parse_string());
            }
            ini.write_to_file(&self.settings_file_path).unwrap();
            self.dirty = false;
        }
    }
}
