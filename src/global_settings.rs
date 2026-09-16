use crate::key_bindings::KeyBinding;
use crate::screen_layout::CustomLayout;
use ini::Ini;
use std::path::PathBuf;
use std::{fs, io};

pub struct GlobalSettings {
    dir: PathBuf,
    pub default_control: KeyBinding,
    pub custom_controls: Vec<KeyBinding>,
    pub custom_layouts: Vec<CustomLayout>,
    pub ra_username: String,
    pub ra_token: String,
}

impl GlobalSettings {
    pub fn new(dir: PathBuf, default_keybinding: KeyBinding) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;

        let mut custom_controls = Vec::new();

        let custom_controls_ini_path = dir.join("custom_controls.ini");
        if let Ok(ini) = Ini::load_from_file(custom_controls_ini_path) {
            for binding_name in ini.sections() {
                if let Some(binding_name) = binding_name {
                    if let Some(props) = ini.section(Some(binding_name)) {
                        custom_controls.push(KeyBinding::from_ini(binding_name, props, default_keybinding.hotkeys));
                    }
                }
            }
        }

        let mut custom_layouts = Vec::new();
        let custom_layouts_ini_path = dir.join("custom_layouts.ini");
        if let Ok(ini) = Ini::load_from_file(custom_layouts_ini_path) {
            for layout_name in ini.sections() {
                if let Some(layout_name) = layout_name {
                    if let Some(props) = ini.section(Some(layout_name)) {
                        custom_layouts.push(CustomLayout::from_ini(layout_name, props));
                    }
                }
            }
        }

        let mut ra_username = "".to_string();
        let mut ra_token = "".to_string();
        let settings_path = dir.join("settings.ini");
        if let Ok(ini) = Ini::load_from_file(settings_path) {
            if let Some(props) = ini.section(Some("ra")) {
                if let Some(username) = props.get("username") {
                    ra_username = username.to_string();
                }
                if let Some(token) = props.get("token") {
                    ra_token = token.to_string();
                }
            }
        }

        Ok(GlobalSettings {
            dir,
            default_control: default_keybinding,
            custom_controls,
            custom_layouts,
            ra_username,
            ra_token,
        })
    }

    /// The profile at `index` of the Controls setting: 0 is the built-in default, then the
    /// custom profiles in order. An index past the end (a profile deleted since the
    /// setting was chosen) falls back to the default rather than leaving the pad dead.
    pub fn get_control(&self, index: usize) -> &KeyBinding {
        match index.checked_sub(1) {
            Some(i) => self.custom_controls.get(i).unwrap_or(&self.default_control),
            None => &self.default_control,
        }
    }

    pub fn add_custom_controls(&mut self, binding: KeyBinding) -> bool {
        if binding.name == self.default_control.name {
            return false;
        }
        match self.custom_controls.iter().find(|b| b.name == binding.name) {
            None => {
                self.custom_controls.push(binding);
                self.flush_custom_controls();
                true
            }
            Some(_) => false,
        }
    }

    pub fn delete_custom_controls(&mut self, index: usize) {
        self.custom_controls.remove(index);
        self.flush_custom_controls();
    }

    fn flush_custom_controls(&self) {
        let custom_controls_ini_path = self.dir.join("custom_controls.ini");
        let mut ini = Ini::new();
        for binding in &self.custom_controls {
            let mut section_setter = ini.with_section(Some(&binding.name));
            binding.to_ini(&mut section_setter);
        }
        ini.write_to_file(custom_controls_ini_path).unwrap();
    }

    pub fn add_custom_layout(&mut self, layout: CustomLayout) -> bool {
        match self.custom_layouts.iter().find(|l| l.name == layout.name) {
            None => {
                self.custom_layouts.push(layout);
                self.flush_custom_layouts();
                true
            }
            Some(_) => false,
        }
    }

    pub fn delete_custom_layout(&mut self, index: usize) {
        self.custom_layouts.remove(index);
        self.flush_custom_layouts();
    }

    fn flush_custom_layouts(&self) {
        let custom_layouts_ini_path = self.dir.join("custom_layouts.ini");
        let mut ini = Ini::new();
        for layout in &self.custom_layouts {
            let mut section_setter = ini.with_section(Some(&layout.name));
            layout.to_ini(&mut section_setter);
        }
        ini.write_to_file(custom_layouts_ini_path).unwrap();
    }

    pub fn set_ra_data(&mut self, username: String, token: String) {
        self.ra_username = username;
        self.ra_token = token;
        self.flush_settings();
    }

    fn flush_settings(&self) {
        let settings_path = self.dir.join("settings.ini");
        let mut ini = Ini::new();
        ini.with_section(Some("ra")).set("username", &self.ra_username).set("token", &self.ra_token);
        ini.write_to_file(settings_path).unwrap();
    }
}
