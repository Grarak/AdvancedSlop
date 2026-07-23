// Custom screen layouts. The GBA has one 240x160 screen, so a layout is just a
// destination rectangle on the presenter surface, fed to the renderer's blit. The
// setting is a list persisted by name and applied both at game launch and live from the
// pause menu.
use crate::core::gpu::{DISPLAY_HEIGHT, DISPLAY_WIDTH};
use crate::presenter::{PRESENTER_SCREEN_HEIGHT, PRESENTER_SCREEN_WIDTH};
use ini::{Properties, SectionSetter};
use std::ffi::CString;

/// Layout names, in setting-list order. The ini stores the selected name, so renaming
/// one resets users of it to the default.
pub const NAMES: [&str; 3] = ["Fit", "Integer", "Stretch"];

/// A user-defined placement of the single GBA screen: where it sits on the presenter
/// surface and how big it is, both in presenter pixels.
#[derive(Clone, Default)]
pub struct CustomLayout {
    pub name: String,
    pub pos: (u16, u16),
    pub size: (u16, u16),
}

impl CustomLayout {
    pub fn name_c_str(&self) -> CString {
        CString::new(self.name.clone()).unwrap_or_default()
    }

    pub fn from_ini(name: &str, props: &Properties) -> Self {
        let get = |k: &str| props.get(k).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
        CustomLayout {
            name: name.to_string(),
            pos: (get("x"), get("y")),
            size: (get("width"), get("height")),
        }
    }

    pub fn to_ini(&self, section_setter: &mut SectionSetter) {
        section_setter
            .set("x", self.pos.0.to_string())
            .set("y", self.pos.1.to_string())
            .set("width", self.size.0.to_string())
            .set("height", self.size.1.to_string());
    }

    /// Seed for the editor: the Fit rectangle, so a new layout starts somewhere sane
    /// rather than as a zero-sized screen in the corner.
    pub fn fit_default() -> Self {
        let (x, y, w, h) = rect(0);
        CustomLayout {
            name: String::new(),
            pos: (x.max(0) as u16, y.max(0) as u16),
            size: (w.max(1) as u16, h.max(1) as u16),
        }
    }

    pub fn rect(&self) -> (i32, i32, i32, i32) {
        (self.pos.0 as i32, self.pos.1 as i32, self.size.0.max(1) as i32, self.size.1.max(1) as i32)
    }
}

/// The rect for a layout index, where indices past the built-in NAMES select a custom
/// layout. Kept separate from `rect` so the hot render path does not need the settings.
pub fn rect_with_custom(layout: usize, customs: &[CustomLayout]) -> (i32, i32, i32, i32) {
    match layout.checked_sub(NAMES.len()).and_then(|i| customs.get(i)) {
        Some(custom) => custom.rect(),
        None => rect(layout),
    }
}

/// The presented frame's (x, y, w, h) on the default framebuffer for a layout index.
/// Out-of-range indices (an ini edited by hand) fall back to Fit.
pub fn rect(layout: usize) -> (i32, i32, i32, i32) {
    let (screen_w, screen_h) = (PRESENTER_SCREEN_WIDTH as f32, PRESENTER_SCREEN_HEIGHT as f32);
    let (guest_w, guest_h) = (DISPLAY_WIDTH as f32, DISPLAY_HEIGHT as f32);
    let fit_scale = (screen_w / guest_w).min(screen_h / guest_h);
    let scale = match NAMES.get(layout).copied() {
        // Whole-pixel scaling: every guest pixel is the same integer size, the sharpest
        // the panel can show (960x544 -> 3x = 720x480, centered)
        Some("Integer") => fit_scale.floor().max(1.0),
        // Fill the whole surface, giving up the aspect ratio
        Some("Stretch") => return (0, 0, screen_w as i32, screen_h as i32),
        // Fit: aspect-preserving maximum (960x544 -> 816x544, centered)
        _ => fit_scale,
    };
    let width = (guest_w * scale) as i32;
    let height = (guest_h * scale) as i32;
    ((screen_w as i32 - width) / 2, (screen_h as i32 - height) / 2, width, height)
}
