pub use self::platform::*;
pub use self::ui::UiPauseMenuReturn;

// Generated Dear ImGui bindings (imgui 1.61). Linux builds imgui + the SDL GL3 backend
// from the vendored sources; the Vita links imgui + imgui_impl_vitagl from the vitasdk.
pub(crate) mod imgui {
    #![allow(warnings, unused)]
    include!(concat!(env!("OUT_DIR"), "/imgui_bindings.rs"));
}

mod ui;

// Runtime debug commands (buttons/framelimit/savestate/…) for the Linux TCP debug port.
#[cfg(all(debug_assertions, not(target_os = "vita")))]
pub(crate) mod dbg_cmds;

#[cfg(target_os = "linux")]
#[path = "sdl.rs"]
mod platform;

#[cfg(target_os = "vita")]
#[path = "vita.rs"]
mod platform;

pub const PRESENTER_SCREEN_WIDTH: u32 = 960;
pub const PRESENTER_SCREEN_HEIGHT: u32 = 544;

pub enum PresentEvent {
    Inputs { keymap: u32 },
    SetFramelimit(u8),
    // Steps the screen-layout setting to the next (or previous) entry, live. The Next
    // and Previous layout hotkeys of the active controls profile.
    CycleScreenLayout { forward: bool },
    // Savestate hotkeys and the screenshot key come back as events rather than poking
    // the savestate module from inside poll_event: capturing the frame needs the
    // renderer and naming the file needs the loaded rom, and the main loop is the only
    // place that has both (and is the thread that owns the presented pixel buffer).
    QuickSave,
    QuickLoad,
    Screenshot,
    Pause,
    Quit,
}

pub const PRESENTER_AUDIO_OUT_SAMPLE_RATE: usize = 48000;
pub const PRESENTER_AUDIO_OUT_BUF_SIZE: usize = 1024;
