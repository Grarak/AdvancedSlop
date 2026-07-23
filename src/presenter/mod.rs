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
    // Steps the screen-layout setting to the next entry, live. Square on the Vita
    // (unmapped as a GBA key), F12 on the keyboard.
    CycleScreenLayout,
    Pause,
    Quit,
}

pub const PRESENTER_AUDIO_OUT_SAMPLE_RATE: usize = 48000;
pub const PRESENTER_AUDIO_OUT_BUF_SIZE: usize = 1024;
