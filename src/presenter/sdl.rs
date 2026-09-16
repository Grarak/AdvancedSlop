use crate::core::gpu::GbaRenderer;
use crate::core::input;
use crate::global_settings::GlobalSettings;
use crate::presenter::imgui::root::{
    ImDrawData, ImGui, ImGuiCol__ImGuiCol_Text, ImGuiConfigFlags__ImGuiConfigFlags_NavEnableKeyboard, ImGuiInputTextFlags__ImGuiInputTextFlags_Password, ImGui_ImplSdlGL3_Init,
    ImGui_ImplSdlGL3_NewFrame, ImGui_ImplSdlGL3_ProcessEvent, ImGui_ImplSdlGL3_RenderDrawData, ImVec2,
};
use crate::key_bindings::{Hotkey, KeyBinding, KEY_CODES, NUM_HOTKEYS, NUM_KEYS};
use crate::presenter::ui::{ControlsEditContext, RALoginContext};
use crate::ra_context::RaContext;
use crate::screen_layout::CustomLayout;
use crate::presenter::ui::{show_main_menu, MenuLaunch, UiBackend};
use crate::presenter::{PresentEvent, PRESENTER_AUDIO_OUT_BUF_SIZE, PRESENTER_AUDIO_OUT_SAMPLE_RATE, PRESENTER_SCREEN_HEIGHT, PRESENTER_SCREEN_WIDTH};
use crate::settings::{Settings, SettingsConfig};
use clap::{arg, command, value_parser, ArgAction, ArgMatches, Command};
use gl::types::GLuint;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::event::{Event, EventType};
use sdl2::video::{GLContext, GLProfile, Window};
use sdl2::{keyboard, EventPump};
use std::ffi::{CStr, CString};
use std::mem;
use std::ops::BitOrAssign;
use std::ptr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::slice;
#[cfg(debug_assertions)]
use std::sync::atomic::Ordering;
use std::thread;

#[derive(Clone)]
pub struct PresenterAudioOut {
    audio_queue: Rc<Option<AudioQueue<i16>>>,
}

unsafe impl Send for PresenterAudioOut {}

impl PresenterAudioOut {
    fn new(audio_queue: Option<AudioQueue<i16>>) -> Self {
        PresenterAudioOut { audio_queue: Rc::new(audio_queue) }
    }

    pub fn play(&self, buffer: &[u32; PRESENTER_AUDIO_OUT_BUF_SIZE]) {
        let raw = unsafe { slice::from_raw_parts(buffer.as_ptr() as *const i16, PRESENTER_AUDIO_OUT_BUF_SIZE * 2) };
        if let Some(audio_queue) = self.audio_queue.as_ref() {
            audio_queue.queue_audio(raw).unwrap();
            while audio_queue.size() != 0 {
                thread::yield_now();
            }
        }
    }
}

pub struct Presenter {
    arg_matches: ArgMatches,
    presenter_audio_out: PresenterAudioOut,
    window: Window,
    _gl_ctx: GLContext,
    // The active controls profile's SDL keycodes, in KEY_NAMES / Hotkey order.
    key_mapping: [u32; NUM_KEYS],
    hotkey_mapping: [u32; NUM_HOTKEYS],
    event_pump: EventPump,
    // ADVANCEDSLOP_DBG_PORT: a localhost TCP command port for headless control (buttons,
    // framelimit, savestate, inst-log, quit) without a wayland virtual keyboard or signals.
    #[cfg(debug_assertions)]
    debug_state: Option<std::sync::Arc<DebugState>>,
    keymap: u32,
}

impl Presenter {
    #[cold]
    pub fn new() -> Option<Self> {
        let mut arg_matches = command!()
            .arg(
                arg!(-f <framelimit> "0: No 1: 100%, 2: 200%, 3: 300%")
                    .num_args(1)
                    .required(false)
                    .default_value("1")
                    .value_parser(value_parser!(u8)),
            )
            .arg(arg!(audio: -a "Enable audio").required(false).action(ArgAction::SetTrue))
            .arg(arg!(-s <savestate> "Continue from a savestate file").num_args(1).required(false).value_parser(value_parser!(String)));
        // Available in dev and release-debug builds (debug_assertions); a no-op stub in release
        if cfg!(debug_assertions) {
            arg_matches = arg_matches
                .arg(
                    arg!(--"inst-log" <path> "Write a per-instruction binary log (debug builds only)")
                        .num_args(1)
                        .required(false)
                        .value_parser(value_parser!(String)),
                )
                .arg(
                    arg!(--"inst-log-lazy" <path> "Like --inst-log, but recording only starts on the debug port 'inst-log' command")
                        .num_args(1)
                        .required(false)
                        .value_parser(value_parser!(String)),
                )
        }
        let arg_matches = arg_matches
            .arg(arg!([gba_rom] "GBA rom to run").num_args(1).required(true).value_parser(value_parser!(String)))
            .subcommand(
                Command::new("decode-inst-log")
                    .about("Decode a binary instruction log to text")
                    .arg(arg!(<path> "Log file to decode").value_parser(value_parser!(String))),
            )
            .subcommand_negates_reqs(true)
            .get_matches();

        // Offline deserializer for the binary instruction log; runs and exits without starting the emulator.
        if let Some(sub) = arg_matches.subcommand_matches("decode-inst-log") {
            crate::debug_inst_log::decode_file(sub.get_one::<String>("path").unwrap());
            return None;
        }

        let wants_inst_log = arg_matches.get_one::<String>("inst-log").is_some() || arg_matches.get_one::<String>("inst-log-lazy").is_some();
        if crate::INST_TRACE {
            if let Some(path) = arg_matches.get_one::<String>("inst-log") {
                crate::debug_inst_log::init(path);
            }
            if let Some(path) = arg_matches.get_one::<String>("inst-log-lazy") {
                crate::debug_inst_log::init_lazy(path);
            }
        } else if wants_inst_log {
            // The trace hooks are only compiled into the dev profile; failing loudly beats
            // recording an empty log.
            eprintln!("--inst-log needs a dev-profile build (cargo build); this build has no trace hooks");
            std::process::exit(2);
        }

        sdl2::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
        let sdl = sdl2::init().unwrap();
        let sdl_video = sdl.video().unwrap();
        let audio_queue = sdl
            .audio()
            .and_then(|sdl_audio| {
                sdl_audio
                    .open_queue(
                        None,
                        &AudioSpecDesired {
                            freq: Some(PRESENTER_AUDIO_OUT_SAMPLE_RATE as i32),
                            channels: Some(2),
                            samples: Some(PRESENTER_AUDIO_OUT_BUF_SIZE as u16),
                        },
                    )
                    .and_then(|audio_queue| {
                        audio_queue.resume();
                        Ok(audio_queue)
                    })
            })
            .ok();

        let gl_attr = sdl_video.gl_attr();
        gl_attr.set_context_profile(GLProfile::GLES);
        gl_attr.set_context_version(3, 0);

        let window = sdl_video.window("AdvancedSlop", PRESENTER_SCREEN_WIDTH, PRESENTER_SCREEN_HEIGHT).opengl().build().unwrap();

        let gl_ctx = window.gl_create_context().unwrap();
        gl::load_with(|name| sdl_video.gl_get_proc_address(name) as *const _);

        // The framelimiter paces the emulator; the host refresh rate must not also. It
        // matters more than it looks: the emulation thread waits at each vblank for the
        // previous frame to have been composed, so a blocking SwapBuffers would reach all
        // the way back and cap emulation at the display's rate.
        let _ = sdl_video.gl_set_swap_interval(sdl2::video::SwapInterval::Immediate);

        debug_assert_eq!(gl_attr.context_profile(), GLProfile::GLES);
        debug_assert_eq!(gl_attr.context_version(), (3, 0));

        let event_pump = sdl.event_pump().unwrap();

        let mut instance = Presenter {
            arg_matches,
            presenter_audio_out: PresenterAudioOut::new(audio_queue),
            window,
            _gl_ctx: gl_ctx,
            key_mapping: DEFAULT_KEY_MAPPING,
            hotkey_mapping: DEFAULT_HOTKEY_MAPPING,
            event_pump,
            #[cfg(debug_assertions)]
            debug_state: std::env::var("ADVANCEDSLOP_DBG_PORT").ok().and_then(|p| p.parse::<u16>().ok()).map(spawn_debug_port),
            keymap: 0xFFFFFFFF,
        };
        crate::presenter::ui::init_ui(&mut instance);
        Some(instance)
    }

    pub fn get_rom_path(&self) -> Option<PathBuf> {
        self.arg_matches.get_one::<String>("gba_rom").map(PathBuf::from)
    }

    /// If the gba_rom arg is a file, launch it directly (CLI/testing path, applying the
    /// CLI framelimit/audio overrides). If it's a directory, show the imgui game browser
    /// rooted there (which edits the persisted settings in place) and return the rom.
    pub fn present_ui(&mut self, settings_config: &mut SettingsConfig, global_settings: &mut GlobalSettings, ra_context: &mut RaContext) -> Option<MenuLaunch> {
        let arg = PathBuf::from(self.arg_matches.get_one::<String>("gba_rom")?);
        if arg.is_dir() {
            show_main_menu(&arg, settings_config, global_settings, ra_context, self)
        } else {
            settings_config.settings.set_framelimit(self.get_framelimit_arg());
            settings_config.settings.set_audio(self.get_audio_arg());
            Some(arg.into())
        }
    }

    /// Where the emulator keeps its own files (settings, texture dumps) — alongside the
    /// browsed roms.
    pub fn data_path(&self) -> PathBuf {
        let arg = PathBuf::from(self.arg_matches.get_one::<String>("gba_rom").cloned().unwrap_or_default());
        if arg.is_dir() {
            arg
        } else {
            arg.parent().map(|p| p.to_path_buf()).unwrap_or_default()
        }
    }

    pub fn settings_path(&self) -> PathBuf {
        self.data_path().join("advancedslop_settings.ini")
    }

    pub fn present_pause(&mut self, renderer: &GbaRenderer, settings_config: &mut SettingsConfig, rom_path: &Path) -> crate::presenter::UiPauseMenuReturn {
        crate::presenter::ui::show_pause_menu(self, renderer, settings_config, rom_path)
    }

    pub fn present_savestate_progress(&mut self, renderer: &GbaRenderer, text: impl AsRef<str>, progress: usize) {
        crate::presenter::ui::show_savestate_progress(self, renderer, text, progress)
    }

    pub fn present_progress(&mut self, title: &str, progress: usize, total: usize) {
        crate::presenter::ui::show_progress(title, progress, total, self);
    }

    pub fn get_framelimit_arg(&self) -> u8 {
        *self.arg_matches.get_one::<u8>("framelimit").unwrap_or(&1)
    }

    pub fn get_audio_arg(&self) -> bool {
        self.arg_matches.get_flag("audio")
    }

    pub fn get_savestate_path(&self) -> Option<PathBuf> {
        self.arg_matches.get_one::<String>("savestate").map(PathBuf::from)
    }

    /// Switch to a controls profile; takes effect from the next poll.
    pub fn set_key_mapping(&mut self, binding: &KeyBinding) {
        self.key_mapping = binding.buttons;
        self.hotkey_mapping = binding.hotkeys;
        // Keys held under the old profile would otherwise never see their release.
        self.keymap = 0xFFFFFFFF;
    }

    pub fn poll_event(&mut self, _: &Settings) -> PresentEvent {
        for event in self.event_pump.poll_iter() {
            match event {
                Event::KeyDown { keycode: Some(code), keymod, repeat, .. } => {
                    // The profile's hotkeys come first, so a profile can take over any of
                    // the fixed keys below. Key repeat would cycle layouts on a held key.
                    let key = code as i32 as u32;
                    if !repeat {
                        const HOTKEY_EVENTS: [(Hotkey, PresentEvent); NUM_HOTKEYS] = [
                            (Hotkey::Pause, PresentEvent::Pause),
                            (Hotkey::NextLayout, PresentEvent::CycleScreenLayout { forward: true }),
                            (Hotkey::PreviousLayout, PresentEvent::CycleScreenLayout { forward: false }),
                        ];
                        for (hotkey, event) in HOTKEY_EVENTS {
                            if self.hotkey_mapping[hotkey as usize] == key {
                                return event;
                            }
                        }
                    }
                    // F1-F9 set the framelimit to 1-9 (100%..500%), F10 uncaps it.
                    let function_keys = [
                        keyboard::Keycode::F1,
                        keyboard::Keycode::F2,
                        keyboard::Keycode::F3,
                        keyboard::Keycode::F4,
                        keyboard::Keycode::F5,
                        keyboard::Keycode::F6,
                        keyboard::Keycode::F7,
                        keyboard::Keycode::F8,
                        keyboard::Keycode::F9,
                        keyboard::Keycode::F10,
                    ];
                    if let Some(index) = function_keys.iter().position(|&key| key == code) {
                        return PresentEvent::SetFramelimit(if index == 9 { 0 } else { index as u8 + 1 });
                    }
                    // F11 quick-saves into the overwrite-in-place quick slot, Shift+F11
                    // loads it back. F1-F10/F12 are taken, so the load side rides a
                    // modifier rather than displacing an existing binding.
                    if code == keyboard::Keycode::F11 {
                        return if keymod.intersects(keyboard::Mod::LSHIFTMOD | keyboard::Mod::RSHIFTMOD) {
                            PresentEvent::QuickLoad
                        } else {
                            PresentEvent::QuickSave
                        };
                    }
                    if code == keyboard::Keycode::PrintScreen {
                        return PresentEvent::Screenshot;
                    }
                    // Rewind is a hold, not a press, so it drives an atomic the emulation
                    // thread samples at its vblank hook rather than a one-shot event.
                    if code == keyboard::Keycode::Backspace {
                        crate::core::rewind::set_rewind_held(true);
                    }
                    for (mapped, gba_key) in self.key_mapping.into_iter().zip(KEY_CODES) {
                        if mapped == key {
                            self.keymap &= !(1 << gba_key as u8);
                        }
                    }
                }
                Event::KeyUp { keycode: Some(code), .. } => {
                    if code == keyboard::Keycode::Backspace {
                        crate::core::rewind::set_rewind_held(false);
                    }
                    let key = code as i32 as u32;
                    for (mapped, gba_key) in self.key_mapping.into_iter().zip(KEY_CODES) {
                        if mapped == key {
                            self.keymap |= 1 << gba_key as u8;
                        }
                    }
                }
                Event::Quit { .. } => return PresentEvent::Quit,
                _ => {}
            }
        }
        // Debug command port (ADVANCEDSLOP_DBG_PORT): service one-shot runtime commands (quit,
        // framelimit) as PresentEvents, and fold held buttons into this frame's inputs.
        #[cfg(debug_assertions)]
        if let Some(ref st) = self.debug_state {
            if st.quit.swap(false, Ordering::Relaxed) {
                return PresentEvent::Quit;
            }
            if st.pause.swap(false, Ordering::Relaxed) {
                return PresentEvent::Pause;
            }
            if st.quick_save.swap(false, Ordering::Relaxed) {
                return PresentEvent::QuickSave;
            }
            if st.quick_load.swap(false, Ordering::Relaxed) {
                return PresentEvent::QuickLoad;
            }
            let fl = st.pending_framelimit.swap(-1, Ordering::Relaxed);
            if fl >= 0 {
                return PresentEvent::SetFramelimit(fl as u8);
            }
        }

        let keymap;
        #[cfg(debug_assertions)]
        {
            let mut km = self.keymap;
            if let Some(ref st) = self.debug_state {
                km &= !st.held_buttons.load(Ordering::Relaxed);
            }
            keymap = km;
        }
        #[cfg(not(debug_assertions))]
        {
            keymap = self.keymap;
        }

        PresentEvent::Inputs { keymap }
    }

    pub fn gl_swap_window(&self) {
        self.window.gl_swap_window();
    }

    pub fn get_presenter_audio_out(&self) -> PresenterAudioOut {
        self.presenter_audio_out.clone()
    }

    pub fn gl_create_depth_tex() -> GLuint {
        0
    }

    pub fn gl_version_suffix() -> &'static str {
        ""
    }

}

impl UiBackend for Presenter {
    fn init(&mut self) {
        unsafe {
            (*ImGui::GetIO()).ConfigFlags.bitor_assign(ImGuiConfigFlags__ImGuiConfigFlags_NavEnableKeyboard as i32);
            ImGui_ImplSdlGL3_Init(self.window.raw() as _, ptr::null());
        }
    }

    fn new_frame(&mut self) -> bool {
        unsafe {
            let mut event: sdl2::sys::SDL_Event = mem::zeroed();
            while sdl2::sys::SDL_PollEvent(&mut event) != 0 {
                if let Ok(event_type) = EventType::try_from(event.type_) {
                    if event_type == EventType::Quit {
                        return false;
                    }
                }
                ImGui_ImplSdlGL3_ProcessEvent(ptr::addr_of_mut!(event) as _);
            }
            // Before NewFrame: it latches KeysDown into the KeysDownDuration edges the
            // menus read, and the SDL backend only writes KeysDown from events, so an
            // injected key survives to be latched.
            #[cfg(debug_assertions)]
            self.apply_debug_nav_inputs();
            ImGui_ImplSdlGL3_NewFrame(self.window.raw() as _);
            true
        }
    }

    fn render_draw_data(&mut self, draw_data: *mut ImDrawData) {
        unsafe { ImGui_ImplSdlGL3_RenderDrawData(draw_data) };
    }

    fn swap_window(&mut self) {
        self.gl_swap_window();
    }
}

impl Presenter {
    /// Drive the imgui menus from the debug command port.
    ///
    /// The port's buttons otherwise only reach the emulated GBA, which leaves the rom
    /// browser and the settings/RetroAchievements overlays unreachable on a headless box:
    /// they read real input, and a wayland session will not let a client synthesise any.
    ///
    /// Injection goes through imgui's *keyboard* state rather than NavInputs: imgui only
    /// acts on NavInputs when NavEnableGamepad is set, and this build enables
    /// NavEnableKeyboard, whose keys imgui maps onto its nav itself.
    #[cfg(debug_assertions)]
    fn apply_debug_nav_inputs(&mut self) {
        // SDL scancodes (io.KeysDown is indexed by scancode in the SDL backend).
        const SC_RETURN: usize = 40;
        const SC_ESCAPE: usize = 41;
        const SC_SPACE: usize = 44;
        const SC_RIGHT: usize = 79;
        const SC_LEFT: usize = 80;
        const SC_DOWN: usize = 81;
        const SC_UP: usize = 82;

        // A presses Space *and* Return: imgui's keyboard nav activates a focused widget
        // on Space; Return doubles as Input for widgets that distinguish the two.
        const KEY_MAPPING: [(input::Keycode, usize); 7] = [
            (input::Keycode::Up, SC_UP),
            (input::Keycode::Down, SC_DOWN),
            (input::Keycode::Left, SC_LEFT),
            (input::Keycode::Right, SC_RIGHT),
            (input::Keycode::A, SC_SPACE),
            (input::Keycode::A, SC_RETURN),
            (input::Keycode::B, SC_ESCAPE),
        ];

        let Some(ref st) = self.debug_state else { return };
        let held = st.held_buttons.load(Ordering::Relaxed);

        // Assign, never just set: leaving a key down once pressed gives imgui no release,
        // so there is no second press edge to activate anything and held arrows key-repeat
        // the nav cursor away on their own. The port owns these keys outright while it is
        // enabled, which is fine — a box being driven over it has no real keyboard.
        unsafe {
            let io = &mut *ImGui::GetIO();
            for (key, scancode) in KEY_MAPPING {
                io.KeysDown[scancode] = held & (1 << key as u32) != 0;
            }
        }
    }
}


// State + command parser live in presenter::dbg_cmds.
#[cfg(debug_assertions)]
use super::dbg_cmds::{handle_debug_cmd, DebugState};

// Spawn the debug command port on 127.0.0.1:<port>; see dbg_cmds.rs for the command
// grammar. Replies "ok" or "err: ...".
#[cfg(debug_assertions)]
fn spawn_debug_port(port: u16) -> std::sync::Arc<DebugState> {
    use std::io::{BufRead, BufReader, Write};
    let state = std::sync::Arc::new(DebugState::new());
    let srv = state.clone();
    thread::Builder::new()
        .name("dbg_port".to_owned())
        .spawn(move || {
            let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[dbg_port] bind 127.0.0.1:{port} failed: {e}");
                    return;
                }
            };
            eprintln!("[dbg_port] listening on 127.0.0.1:{port}");
            for stream in listener.incoming().flatten() {
                let mut writer = match stream.try_clone() {
                    Ok(w) => w,
                    Err(_) => continue,
                };
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let reply = handle_debug_cmd(&srv, line.trim());
                    if writeln!(writer, "{reply}").is_err() {
                        break;
                    }
                }
            }
        })
        .unwrap();
    state
}

/// The RetroAchievements login form. Linux has a real keyboard, so the fields are plain
/// imgui text inputs; the Vita build bounces through the system IME instead.
pub fn show_retroachievements_settings(global_settings: &mut GlobalSettings, login_context: &mut RALoginContext, context: &mut RaContext) {
    unsafe {
        if !global_settings.ra_username.is_empty() && !global_settings.ra_token.is_empty() {
            let msg = CString::new(format!("Currently logged in as {}", global_settings.ra_username)).unwrap();
            ImGui::Text(msg.as_ptr());
        }

        let mut username = [0u8; 128];
        let len = login_context.username.len().min(username.len() - 1);
        username[..len].copy_from_slice(&login_context.username.as_bytes()[..len]);
        if ImGui::InputText(c"Username".as_ptr(), username.as_mut_ptr(), username.len(), 0, None, ptr::null_mut()) {
            login_context.username = CStr::from_ptr(username.as_ptr() as _).to_str().unwrap_or("").to_string();
        }

        let mut password = [0u8; 128];
        let len = login_context.password.len().min(password.len() - 1);
        password[..len].copy_from_slice(&login_context.password.as_bytes()[..len]);
        if ImGui::InputText(
            c"Password".as_ptr(),
            password.as_mut_ptr(),
            password.len(),
            ImGuiInputTextFlags__ImGuiInputTextFlags_Password as _,
            None,
            ptr::null_mut(),
        ) {
            login_context.password = CStr::from_ptr(password.as_ptr() as _).to_str().unwrap_or("").to_string();
        }

        if !login_context.error.is_empty() {
            ImGui::PushStyleColor(ImGuiCol__ImGuiCol_Text as _, 0xFF0000FF);
            let err = CString::new(login_context.error.as_str()).unwrap_or_default();
            ImGui::Text(err.as_ptr());
            ImGui::PopStyleColor(1);
        }

        crate::presenter::ui::poll_ra_login(global_settings, login_context, context);

        let sz = ImVec2 { x: 0.0, y: 0.0 };
        if login_context.logging_in {
            ImGui::Text(c"Logging in...".as_ptr());
        } else if ImGui::Button(c"Login".as_ptr(), &sz) {
            login_context.error.clear();
            login_context.logging_in = true;
            context.login_with_password(&login_context.username, &login_context.password);
        }
    }
}

lazy_static::lazy_static! {
    /// Every key a profile can bind, with SDL's name for it: letters, digits, the arrows,
    /// modifiers and the common punctuation. Escape and F12 are the default pause and
    /// layout hotkeys, so they are offered too.
    static ref BINDABLE_KEYS: Vec<(CString, u32)> = {
        use keyboard::Keycode::*;
        let keys = [
            A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
            Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,
            Up, Down, Left, Right, Return, Space, Tab, Escape, F12,
            LShift, RShift, LCtrl, RCtrl, LAlt, RAlt,
            Comma, Period, Slash, Semicolon, Quote, LeftBracket, RightBracket, Minus, Equals, Backslash,
        ];
        keys.into_iter().map(|key| (CString::new(key.name()).unwrap_or_default(), key as i32 as u32)).collect()
    };
}

/// The new-profile editor over the keyboard; the name is a plain text field.
pub fn show_controls_create_settings(global_settings: &mut GlobalSettings, edit_context: &mut ControlsEditContext, binding: &mut KeyBinding) -> bool {
    unsafe {
        crate::presenter::ui::show_controls_editor(global_settings, edit_context, binding, &BINDABLE_KEYS, |name| {
            let mut buf = [0u8; 33];
            let len = name.len().min(buf.len() - 1);
            buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            if ImGui::InputText(c"Profile name".as_ptr(), buf.as_mut_ptr() as _, buf.len(), 0, None, ptr::null_mut()) {
                *name = CStr::from_ptr(buf.as_ptr() as _).to_str().unwrap_or("").to_string();
            }
        })
    }
}

/// Editable text field for the savestate rename dialog. There is a real keyboard here,
/// so the field is edited in place.
pub fn text_input_field(label: &CStr, value: &mut String, max_len: usize) {
    unsafe {
        let mut buf = vec![0u8; max_len + 1];
        let len = value.len().min(max_len);
        buf[..len].copy_from_slice(&value.as_bytes()[..len]);
        ImGui::PushItemWidth(-1.0);
        if ImGui::InputText(label.as_ptr(), buf.as_mut_ptr(), buf.len(), 0, None, ptr::null_mut()) {
            *value = CStr::from_ptr(buf.as_ptr() as _).to_str().unwrap_or("").to_string();
        }
        ImGui::PopItemWidth();
    }
}

/// Default mapping in KEY_NAMES order: K/J for A/B, WASD for the d-pad, 8/9 for L/R,
/// V/B for Select/Start.
const DEFAULT_KEY_MAPPING: [u32; NUM_KEYS] = [
    keyboard::Keycode::K as i32 as u32,    // A
    keyboard::Keycode::J as i32 as u32,    // B
    keyboard::Keycode::D as i32 as u32,    // Right
    keyboard::Keycode::A as i32 as u32,    // Left
    keyboard::Keycode::W as i32 as u32,    // Up
    keyboard::Keycode::S as i32 as u32,    // Down
    keyboard::Keycode::Num9 as i32 as u32, // R
    keyboard::Keycode::Num8 as i32 as u32, // L
    keyboard::Keycode::V as i32 as u32,    // Select
    keyboard::Keycode::B as i32 as u32,    // Start
];

/// Defaults in Hotkey order: F12 cycles the layout forward and Escape opens the pause
/// menu; stepping backwards has no default key.
const DEFAULT_HOTKEY_MAPPING: [u32; NUM_HOTKEYS] = [0, keyboard::Keycode::F12 as i32 as u32, keyboard::Keycode::Escape as i32 as u32];

/// The binding a new profile starts from: the default mapping.
pub fn default_key_binding() -> KeyBinding {
    KeyBinding {
        name: String::new(),
        buttons: DEFAULT_KEY_MAPPING,
        hotkeys: DEFAULT_HOTKEY_MAPPING,
    }
}

/// Layout fields on Linux: plain int inputs, since there is a keyboard.
pub fn show_layout_create_settings(global_settings: &mut GlobalSettings, edit_context: &mut ControlsEditContext, layout: &mut CustomLayout) -> bool {
    unsafe {
        let mut name = [0u8; 32];
        let len = layout.name.len().min(name.len() - 1);
        name[..len].copy_from_slice(&layout.name.as_bytes()[..len]);
        if ImGui::InputText(c"Name".as_ptr(), name.as_mut_ptr(), name.len(), 0, None, ptr::null_mut()) {
            layout.name = CStr::from_ptr(name.as_ptr() as _).to_str().unwrap_or("").to_string();
        }

        let mut field = |label: &CStr, value: &mut u16| {
            let mut v = *value as i32;
            if ImGui::InputInt(label.as_ptr(), &mut v, 1, 10, 0) {
                *value = v.clamp(0, u16::MAX as i32) as u16;
            }
        };
        field(c"X", &mut layout.pos.0);
        field(c"Y", &mut layout.pos.1);
        field(c"Width", &mut layout.size.0);
        field(c"Height", &mut layout.size.1);

        crate::presenter::ui::finish_layout_edit(global_settings, edit_context, layout)
    }
}

