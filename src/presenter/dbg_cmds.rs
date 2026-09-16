// Debug-only runtime command surface for the Linux TCP port (ADVANCEDSLOP_DBG_PORT in
// sdl.rs). Commands mutate a DebugState; the platform poll_event folds it into each
// frame's inputs.
//
// Newline/broadcast-delimited text commands:
//   press/release <btn> | buttons [<btn>...] |
//   key <sdl key name> <down|up> | text <string> |
//   framelimit <0..9> | pause | savestate | loadstate | rewind <on|off> | inst-log | quit
// btn: a b up down left right start select l r. inst-log arms --inst-log-lazy
// capture. Replies "ok" or "err: ...".
//
// press/release/buttons set GBA keys directly, past the controls profile. key/text
// instead push real SDL keyboard events, so they go through the profile mapping in
// poll_event (and reach imgui text fields) exactly like a physical keyboard would.

use crate::core::input;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

// Held-button bits are input::Keycode positions.
pub struct DebugState {
    pub held_buttons: AtomicU32,
    pub pending_framelimit: AtomicI32, // -1 = none, else 0..=9
    pub pause: AtomicBool,
    pub quit: AtomicBool,
    pub quick_save: AtomicBool,
    pub quick_load: AtomicBool,
}

impl DebugState {
    pub const fn new() -> Self {
        DebugState {
            held_buttons: AtomicU32::new(0),
            pending_framelimit: AtomicI32::new(-1),
            pause: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            quick_save: AtomicBool::new(false),
            quick_load: AtomicBool::new(false),
        }
    }
}

pub fn handle_debug_cmd(state: &DebugState, line: &str) -> String {
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    match cmd {
        "" => "ok".to_owned(),
        "press" | "release" => {
            let Some(code) = it.next().and_then(debug_input_key) else {
                return "err: unknown button".to_owned();
            };
            let bit = 1u32 << code as u8;
            if cmd == "press" {
                state.held_buttons.fetch_or(bit, Ordering::Relaxed);
            } else {
                state.held_buttons.fetch_and(!bit, Ordering::Relaxed);
            }
            "ok".to_owned()
        }
        "buttons" => {
            let mut mask = 0u32;
            for tok in it {
                match debug_input_key(tok) {
                    Some(code) => mask |= 1 << code as u8,
                    None => return format!("err: unknown button '{tok}'"),
                }
            }
            state.held_buttons.store(mask, Ordering::Relaxed);
            "ok".to_owned()
        }
        "key" => {
            let Some(keycode) = it.next().and_then(sdl2::keyboard::Keycode::from_name) else {
                return "err: key <sdl key name> <down|up>".to_owned();
            };
            let event_type = match it.next() {
                Some("down") => sdl2::sys::SDL_EventType::SDL_KEYDOWN,
                Some("up") => sdl2::sys::SDL_EventType::SDL_KEYUP,
                _ => return "err: key <sdl key name> <down|up>".to_owned(),
            };
            let scancode = unsafe { sdl2::sys::SDL_GetScancodeFromKey(keycode as i32) };
            let mut event = sdl2::sys::SDL_Event {
                key: sdl2::sys::SDL_KeyboardEvent {
                    type_: event_type as u32,
                    timestamp: 0,
                    windowID: 0,
                    state: (event_type == sdl2::sys::SDL_EventType::SDL_KEYDOWN) as u8,
                    repeat: 0,
                    padding2: 0,
                    padding3: 0,
                    keysym: sdl2::sys::SDL_Keysym {
                        scancode,
                        sym: keycode as i32,
                        mod_: 0,
                        unused: 0,
                    },
                },
            };
            // SDL_PushEvent is thread-safe; the event joins the main thread's queue.
            push_sdl_event(&mut event)
        }
        "text" => {
            let text = line.trim_start().strip_prefix("text").unwrap_or("").trim_start();
            let mut event = sdl2::sys::SDL_Event {
                text: sdl2::sys::SDL_TextInputEvent {
                    type_: sdl2::sys::SDL_EventType::SDL_TEXTINPUT as u32,
                    timestamp: 0,
                    windowID: 0,
                    text: [0; 32],
                },
            };
            // One event holds up to 31 bytes plus the terminator.
            let bytes = &text.as_bytes()[..text.len().min(31)];
            unsafe { event.text.text[..bytes.len()].copy_from_slice(&*(bytes as *const [u8] as *const [std::ffi::c_char])) };
            push_sdl_event(&mut event)
        }
        "framelimit" => match it.next().and_then(|s| s.parse::<i32>().ok()) {
            Some(n @ 0..=9) => {
                state.pending_framelimit.store(n, Ordering::Relaxed);
                "ok".to_owned()
            }
            _ => "err: framelimit <0..9>".to_owned(),
        },
        // Opens the pause menu, which the keyboard would otherwise be the only way in
        // (the menu itself still needs real key/gamepad/mouse input to drive).
        "pause" => {
            state.pause.store(true, Ordering::Relaxed);
            "ok".to_owned()
        }
        // Handed to poll_event rather than straight to the savestate module: the save
        // needs a screenshot of the presented frame, which only the render thread may read.
        "savestate" => {
            state.quick_save.store(true, Ordering::Relaxed);
            "ok".to_owned()
        }
        "loadstate" => {
            state.quick_load.store(true, Ordering::Relaxed);
            "ok".to_owned()
        }
        // Rewind is a held input, so it is set/cleared rather than pulsed. Straight to the
        // module: it is an atomic the emulation thread samples, with no frame to capture.
        "rewind" => match it.next() {
            Some("on") => {
                crate::core::rewind::set_rewind_held(true);
                "ok".to_owned()
            }
            Some("off") => {
                crate::core::rewind::set_rewind_held(false);
                "ok".to_owned()
            }
            _ => "err: rewind <on|off>".to_owned(),
        },
        "inst-log" => {
            crate::debug_inst_log::arm_lazy();
            "ok".to_owned()
        }
        "quit" => {
            state.quit.store(true, Ordering::Relaxed);
            "ok".to_owned()
        }
        other => format!("err: unknown cmd '{other}'"),
    }
}

fn push_sdl_event(event: &mut sdl2::sys::SDL_Event) -> String {
    if unsafe { sdl2::sys::SDL_PushEvent(event) } == 1 {
        "ok".to_owned()
    } else {
        "err: SDL_PushEvent failed".to_owned()
    }
}

// Debug-only button names → input::Keycode.
fn debug_input_key(name: &str) -> Option<input::Keycode> {
    use input::Keycode::*;
    Some(match name {
        "a" => A,
        "b" => B,
        "up" => Up,
        "down" => Down,
        "left" => Left,
        "right" => Right,
        "start" => Start,
        "select" => Select,
        "l" => TriggerL,
        "r" => TriggerR,
        _ => return None,
    })
}
