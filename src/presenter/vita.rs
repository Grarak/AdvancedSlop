use crate::core::gpu::GbaRenderer;
use crate::core::input::Keycode;
use crate::logging::info_println;
use crate::global_settings::GlobalSettings;
use crate::presenter::imgui::root::{
    ImDrawData, ImGui, ImGuiCol__ImGuiCol_Text, ImGui_ImplVitaGL_GamepadUsage, ImGui_ImplVitaGL_Init, ImGui_ImplVitaGL_MouseStickUsage, ImGui_ImplVitaGL_NewFrame,
    ImGui_ImplVitaGL_RenderDrawData, ImGui_ImplVitaGL_TouchUsage, ImVec2,
};
use crate::key_bindings::KeyBinding;
use crate::presenter::ui::{ControlsEditContext, RALoginContext};
use crate::ra_context::RaContext;
use crate::screen_layout::CustomLayout;
use crate::presenter::ui::{show_main_menu, UiBackend};
use crate::presenter::{PresentEvent, PRESENTER_AUDIO_OUT_BUF_SIZE, PRESENTER_AUDIO_OUT_SAMPLE_RATE, PRESENTER_SCREEN_HEIGHT, PRESENTER_SCREEN_WIDTH};
use crate::settings::{Settings, SettingsConfig};
use gl::types::GLuint;
use std::ffi::CString;
use std::path::PathBuf;
use std::str::FromStr;
use std::{mem, ptr};
use vita_gl::SharkOpt;
use vitasdk_sys::*;

pub const ROM_PATH: &str = "ux0:data/advancedslop";
pub const LOG_PATH: &str = "ux0:data/advancedslop/log";
pub const LOG_FILE: &str = "ux0:data/advancedslop/log/log.txt";

// taihen/vitashark/SceShaccCgExt: vitaGL's runtime cg shader compiler; mathneon for
// the math-neon C sources
#[link(name = "taihen_stub", kind = "static", modifiers = "+whole-archive")]
#[link(name = "SceShaccCgExt", kind = "static", modifiers = "+whole-archive")]
#[link(name = "mathneon", kind = "static", modifiers = "+whole-archive")]
#[link(name = "vitashark", kind = "static", modifiers = "+whole-archive")]
// #[link(name = "SceRazorHud_stub", kind = "static", modifiers = "+whole-archive")]
// #[link(name = "ScePerf_stub", kind = "static", modifiers = "+whole-archive")]
extern "C" {}

const KEY_CODE_MAPPING: [(SceCtrlButtons, Keycode); 10] = [
    (SCE_CTRL_UP, Keycode::Up),
    (SCE_CTRL_DOWN, Keycode::Down),
    (SCE_CTRL_LEFT, Keycode::Left),
    (SCE_CTRL_RIGHT, Keycode::Right),
    (SCE_CTRL_START, Keycode::Start),
    (SCE_CTRL_SELECT, Keycode::Select),
    (SCE_CTRL_CIRCLE, Keycode::A),
    (SCE_CTRL_CROSS, Keycode::B),
    (SCE_CTRL_LTRIGGER, Keycode::TriggerL),
    (SCE_CTRL_RTRIGGER, Keycode::TriggerR),
];

#[derive(Clone)]
pub struct PresenterAudioOut {
    audio_port: std::os::raw::c_int,
}

impl PresenterAudioOut {
    fn new() -> Self {
        unsafe {
            PresenterAudioOut {
                audio_port: sceAudioOutOpenPort(
                    SCE_AUDIO_OUT_PORT_TYPE_BGM,
                    PRESENTER_AUDIO_OUT_BUF_SIZE as _,
                    PRESENTER_AUDIO_OUT_SAMPLE_RATE as _,
                    SCE_AUDIO_OUT_MODE_STEREO,
                ),
            }
        }
    }

    pub fn play(&self, buffer: &[u32; PRESENTER_AUDIO_OUT_BUF_SIZE]) {
        unsafe { sceAudioOutOutput(self.audio_port, buffer.as_ptr() as _) };
    }
}

unsafe impl Send for PresenterAudioOut {}

pub struct Presenter {
    presenter_audio_out: PresenterAudioOut,
    keymap: u32,
    prev_buttons: u32,
}

impl Presenter {
    fn module_installed(name: &str) -> bool {
        let name = CString::from_str(name).unwrap();
        let search_unk = [0u32; 2];
        unsafe { _vshKernelSearchModuleByName(name.as_ptr(), search_unk.as_ptr() as _) >= 0 }
    }

    #[cold]
    pub fn new() -> Option<Self> {
        unsafe {
            info_println!("Set clocks");
            scePowerSetArmClockFrequency(444);
            scePowerSetGpuClockFrequency(222);
            scePowerSetBusClockFrequency(222);
            scePowerSetGpuXbarClockFrequency(166);

            info_println!("Set shader compiler arguments");
            vita_gl::vglSetupRuntimeShaderCompiler(SharkOpt::Fast as _, 1, 0, 1);
            info_println!("Initialize vitaGL");
            vita_gl::vglInitExtended(0, PRESENTER_SCREEN_WIDTH as _, PRESENTER_SCREEN_HEIGHT as _, 128 * 1024 * 1024, SCE_GXM_MULTISAMPLE_NONE);

            info_println!("Checking for kubridge");
            if !Self::module_installed("kubridge") {
                Self::show_message(c"Kubridge not installed, get version 0.3.1 from https://github.com/bythos14/kubridge/releases and put it under the *KERNEL section in config.txt!");
                return None;
            }

            gl::load_with(|name| {
                let name = CString::new(name).unwrap();
                vita_gl::vglGetProcAddress(name.as_ptr() as _) as _
            });

            sceTouchSetSamplingState(SCE_TOUCH_PORT_FRONT, SCE_TOUCH_SAMPLING_STATE_STOP);

            // Make sure the rom directory exists so the user knows where to drop roms
            let _ = std::fs::create_dir_all(ROM_PATH);

            let mut instance = Presenter {
                presenter_audio_out: PresenterAudioOut::new(),
                keymap: 0xFFFFFFFF,
                prev_buttons: 0,
            };
            crate::presenter::ui::init_ui(&mut instance);
            Some(instance)
        }
    }

    // Blocking OK dialog, rendered over the vitaGL surface (needs GL swap to pump it)
    unsafe fn show_message(msg: &core::ffi::CStr) {
        let mut msg_param: SceMsgDialogUserMessageParam = mem::zeroed();
        msg_param.buttonType = SCE_MSG_DIALOG_BUTTON_TYPE_OK as _;
        msg_param.msg = msg.as_ptr();

        let mut param: SceMsgDialogParam = mem::zeroed();
        param.commonParam.magic = SCE_COMMON_DIALOG_MAGIC_NUMBER + ptr::addr_of_mut!(param.commonParam) as usize as u32;
        param.sdkVersion = PSP2_SDK_VERSION;
        param.mode = SCE_MSG_DIALOG_MODE_USER_MSG as _;
        param.userMsgParam = ptr::addr_of_mut!(msg_param);

        sceMsgDialogInit(&param);
        while sceMsgDialogGetStatus() != SCE_COMMON_DIALOG_STATUS_FINISHED {
            vita_gl::vglSwapBuffers(gl::TRUE);
        }
        sceMsgDialogTerm();
    }

    // First .gba under ux0:data/advancedslop; present_ui uses it as the empty-dir check
    // (and shows the "no rom" dialog), direct boots use it as the pick.
    pub fn get_rom_path(&self) -> Option<PathBuf> {
        let dir = std::fs::read_dir(ROM_PATH).ok()?;
        let mut roms: Vec<PathBuf> = dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext.eq_ignore_ascii_case("gba")).unwrap_or(false))
            .collect();
        roms.sort();
        let first = roms.into_iter().next();
        if first.is_none() {
            unsafe { Self::show_message(c"No .gba rom found. Put one under ux0:data/advancedslop/ and relaunch.") };
        }
        first
    }

    /// Show the imgui game browser rooted at ux0:data/advancedslop and return the chosen rom.
    pub fn present_ui(&mut self, settings_config: &mut SettingsConfig, global_settings: &mut GlobalSettings, ra_context: &mut RaContext) -> Option<PathBuf> {
        // get_rom_path shows the "no rom" dialog and returns None when the folder is empty
        self.get_rom_path()?;
        show_main_menu(std::path::Path::new(ROM_PATH), settings_config, global_settings, ra_context, self)
    }

    /// Where the emulator keeps its own files (settings, texture dumps): ux0:data/advancedslop.
    pub fn data_path(&self) -> PathBuf {
        PathBuf::from(ROM_PATH)
    }

    pub fn settings_path(&self) -> PathBuf {
        self.data_path().join("settings.ini")
    }

    pub fn present_pause(&mut self, renderer: &GbaRenderer, settings_config: &mut SettingsConfig) -> crate::presenter::UiPauseMenuReturn {
        crate::presenter::ui::show_pause_menu(self, renderer, settings_config)
    }

    pub fn present_progress(&mut self, title: &str, progress: usize, total: usize) {
        crate::presenter::ui::show_progress(title, progress, total, self);
    }

    pub fn get_framelimit_arg(&self) -> u8 {
        1
    }

    pub fn get_audio_arg(&self) -> bool {
        true
    }

    pub fn get_savestate_path(&self) -> Option<PathBuf> {
        None
    }

    pub fn poll_event(&mut self, _: &Settings) -> PresentEvent {
        let mut pressed: SceCtrlData = unsafe { mem::zeroed() };
        unsafe { sceCtrlPeekBufferPositive(0, &mut pressed, 1) };

        // Triangle and Square are unmapped as GBA keys, so they serve as hotkeys,
        // edge-triggered: Triangle opens the pause menu, Square cycles the screen layout.
        let edge = |button: SceCtrlButtons| pressed.buttons & button != 0 && self.prev_buttons & button == 0;
        let triangle_edge = edge(SCE_CTRL_TRIANGLE);
        let square_edge = edge(SCE_CTRL_SQUARE);
        self.prev_buttons = pressed.buttons;
        if triangle_edge {
            return PresentEvent::Pause;
        }
        if square_edge {
            return PresentEvent::CycleScreenLayout;
        }

        self.keymap = 0xFFFFFFFF;
        for (button, keycode) in KEY_CODE_MAPPING {
            if pressed.buttons & button != 0 {
                self.keymap &= !(1 << keycode as u8);
            }
        }

        // Left stick as dpad
        let (lx, ly) = (pressed.lx as i32 - 128, pressed.ly as i32 - 128);
        if lx < -64 {
            self.keymap &= !(1 << Keycode::Left as u8);
        } else if lx > 64 {
            self.keymap &= !(1 << Keycode::Right as u8);
        }
        if ly < -64 {
            self.keymap &= !(1 << Keycode::Up as u8);
        } else if ly > 64 {
            self.keymap &= !(1 << Keycode::Down as u8);
        }

        PresentEvent::Inputs { keymap: self.keymap }
    }

    pub fn gl_swap_window(&self) {
        unsafe { vita_gl::vglSwapBuffers(gl::FALSE) };
    }

    pub fn get_presenter_audio_out(&self) -> PresenterAudioOut {
        self.presenter_audio_out.clone()
    }

    pub unsafe fn gl_create_depth_tex() -> GLuint {
        let mut tex = 0;
        gl::GenTextures(1, &mut tex);
        gl::BindTexture(gl::TEXTURE_2D, tex);
        gl::TexImage2D(gl::TEXTURE_2D, 0, gl::RGBA as _, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, ptr::null());
        vita_gl::vglFree(vita_gl::vglGetTexDataPointer(gl::TEXTURE_2D));
        vita_gl::vglTexImageDepthBuffer(gl::TEXTURE_2D);
        gl::BindTexture(gl::TEXTURE_2D, 0);
        tex
    }

    pub unsafe fn gl_get_tex_ptr() -> *mut u8 {
        vita_gl::vglGetTexDataPointer(gl::TEXTURE_2D) as _
    }

    pub unsafe fn gl_remap_tex() -> *mut u8 {
        vita_gl::vglRemapTexPtr() as _
    }

    pub fn gl_version_suffix() -> &'static str {
        vita_gl::VITA_GL_VERSION
    }

}

impl UiBackend for Presenter {
    fn init(&mut self) {
        unsafe {
            info_println!("Initialize ImGui for vitaGL");
            ImGui_ImplVitaGL_Init();
            (*ImGui::GetIO()).MouseDrawCursor = false;
            ImGui_ImplVitaGL_TouchUsage(true);
            ImGui_ImplVitaGL_GamepadUsage(true);
            ImGui_ImplVitaGL_MouseStickUsage(false);
            ImGui::StyleColorsDark(ptr::null_mut());
        }
    }

    fn new_frame(&mut self) -> bool {
        unsafe { ImGui_ImplVitaGL_NewFrame() };
        true
    }

    fn render_draw_data(&mut self, draw_data: *mut ImDrawData) {
        unsafe { ImGui_ImplVitaGL_RenderDrawData(draw_data) };
    }

    fn swap_window(&mut self) {
        self.gl_swap_window();
    }
}

fn to_cstr_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Text entry through the system IME. The Vita has no keyboard, so a field is a button
/// that opens the on-screen one and returns whatever it was left holding.
unsafe fn dialog_input(title: &str, value: &str, input_type: u32, text_box_mode: u32, max_len: u32) -> String {
    let mut params: SceImeDialogParam = mem::zeroed();
    params.commonParam.magic = SCE_COMMON_DIALOG_MAGIC_NUMBER + ptr::addr_of_mut!(params.commonParam) as usize as u32;
    params.sdkVersion = PSP2_SDK_VERSION;
    params.type_ = input_type;
    params.textBoxMode = text_box_mode;

    let title = to_cstr_utf16(title);
    params.title = title.as_ptr() as _;

    let mut input_buf = [0u16; SCE_IME_DIALOG_MAX_TEXT_LENGTH as usize + 1];
    debug_assert!(max_len < SCE_IME_DIALOG_MAX_TEXT_LENGTH);

    let value = to_cstr_utf16(value);
    params.initialText = value.as_ptr() as _;
    params.inputTextBuffer = input_buf.as_mut_ptr() as _;
    params.maxTextLength = max_len;

    sceImeDialogInit(&params);
    while sceImeDialogGetStatus() != SCE_COMMON_DIALOG_STATUS_FINISHED {
        vita_gl::vglSwapBuffers(gl::TRUE);
    }
    sceImeDialogTerm();

    let len = input_buf.iter().position(|c| *c == 0).unwrap_or(input_buf.len());
    String::from_utf16(&input_buf[..len]).unwrap_or_default().trim().to_string()
}

/// The RetroAchievements login form. Each field is a button that opens the IME, since
/// there is no keyboard to type into an imgui text input with.
pub fn show_retroachievements_settings(global_settings: &mut GlobalSettings, login_context: &mut RALoginContext, context: &mut RaContext) {
    unsafe {
        if !global_settings.ra_username.is_empty() && !global_settings.ra_token.is_empty() {
            let msg = CString::new(format!("Currently logged in as {}", global_settings.ra_username)).unwrap();
            ImGui::Text(msg.as_ptr());
        }

        let field_width = ImVec2 { x: 500.0, y: 0.0 };

        ImGui::PushID(c"ra_username".as_ptr());
        ImGui::Text(c"Username".as_ptr());
        ImGui::SameLine(0f32, -1f32);
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + ImGui::GetContentRegionAvail().x - field_width.x);
        let shown = CString::new(login_context.username.as_str()).unwrap_or_default();
        if ImGui::Button(shown.as_ptr(), &field_width) {
            login_context.username = dialog_input("Username", &login_context.username, SCE_IME_TYPE_BASIC_LATIN, SCE_IME_DIALOG_TEXTBOX_MODE_DEFAULT, 128);
        }
        ImGui::PopID();

        ImGui::PushID(c"ra_password".as_ptr());
        ImGui::Text(c"Password".as_ptr());
        ImGui::SameLine(0f32, -1f32);
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + ImGui::GetContentRegionAvail().x - field_width.x);
        let masked = CString::new("*".repeat(login_context.password.len())).unwrap_or_default();
        if ImGui::Button(masked.as_ptr(), &field_width) {
            login_context.password = dialog_input("Password", &login_context.password, SCE_IME_TYPE_BASIC_LATIN, SCE_IME_DIALOG_TEXTBOX_MODE_PASSWORD, 128);
        }
        ImGui::PopID();

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

const BINDABLE_BUTTONS: [(&core::ffi::CStr, u32); 12] = [
    (c"Circle", SCE_CTRL_CIRCLE),
    (c"Cross", SCE_CTRL_CROSS),
    (c"Triangle", SCE_CTRL_TRIANGLE),
    (c"Square", SCE_CTRL_SQUARE),
    (c"L", SCE_CTRL_LTRIGGER),
    (c"R", SCE_CTRL_RTRIGGER),
    (c"Up", SCE_CTRL_UP),
    (c"Down", SCE_CTRL_DOWN),
    (c"Left", SCE_CTRL_LEFT),
    (c"Right", SCE_CTRL_RIGHT),
    (c"Start", SCE_CTRL_START),
    (c"Select", SCE_CTRL_SELECT),
];

/// Default mapping in KEY_NAMES order, matching KEY_CODE_MAPPING above.
const DEFAULT_KEY_MAPPING: [u32; crate::key_bindings::NUM_KEYS] = [
    SCE_CTRL_CIRCLE,   // A
    SCE_CTRL_CROSS,    // B
    SCE_CTRL_RIGHT,    // Right
    SCE_CTRL_LEFT,     // Left
    SCE_CTRL_UP,       // Up
    SCE_CTRL_DOWN,     // Down
    SCE_CTRL_RTRIGGER, // R
    SCE_CTRL_LTRIGGER, // L
    SCE_CTRL_SELECT,   // Select
    SCE_CTRL_START,    // Start
];

/// Defaults in Hotkey order. Square already cycles the layout forward and Triangle
/// opens the pause menu (see poll_event); stepping backwards has no built-in button.
const DEFAULT_HOTKEY_MAPPING: [u32; crate::key_bindings::NUM_HOTKEYS] = [0, SCE_CTRL_SQUARE, SCE_CTRL_TRIANGLE];

/// A fresh controls profile seeded with the default mapping.
pub fn default_key_binding() -> KeyBinding {
    KeyBinding {
        name: String::new(),
        buttons: DEFAULT_KEY_MAPPING,
        hotkeys: DEFAULT_HOTKEY_MAPPING,
    }
}

/// One settings row: a fixed-width button showing `value` (tapping it opens the
/// on-screen keyboard) with `label` to its right.
unsafe fn layout_field_button(label: &str, value: &core::ffi::CStr) -> bool {
    let c_label = CString::from_str(label).unwrap();
    ImGui::PushID(c_label.as_ptr());
    let sz = ImVec2 { x: 150.0, y: 0.0 };
    let clicked = ImGui::Button(value.as_ptr(), &sz);
    ImGui::SameLine(0f32, -1f32);
    ImGui::Text(c_label.as_ptr());
    ImGui::PopID();
    clicked
}

/// One binding row: the key's name, and a combo of every bindable Vita button.
unsafe fn binding_button_row(id: i32, label: &str, value: &mut u32) {
    ImGui::PushID3(id);
    let key_label = CString::from_str(label).unwrap();
    ImGui::Text(key_label.as_ptr());
    ImGui::SameLine(0f32, -1f32);
    ImGui::SetCursorPosX(ImGui::GetCursorPosX() + ImGui::GetContentRegionAvail().x - 200f32);
    ImGui::PushItemWidth(200f32);

    let current = BINDABLE_BUTTONS.iter().position(|(_, bit)| *bit == *value);
    let preview = current.map(|c| BINDABLE_BUTTONS[c].0).unwrap_or(c"None");
    if ImGui::BeginCombo(c"##btn".as_ptr(), preview.as_ptr(), 0) {
        let sz = ImVec2 { x: 0f32, y: 0f32 };
        if ImGui::Selectable(c"None".as_ptr(), current.is_none(), 0, &sz) {
            *value = 0;
        }
        for (j, (name, bit)) in BINDABLE_BUTTONS.iter().enumerate() {
            let is_selected = current == Some(j);
            if ImGui::Selectable(name.as_ptr(), is_selected, 0, &sz) {
                *value = *bit;
            }
            if is_selected {
                ImGui::SetItemDefaultFocus();
            }
        }
        ImGui::EndCombo();
    }
    ImGui::PopItemWidth();
    ImGui::PopID();
}

/// The new-profile editor: a name plus one combo per GBA key and hotkey. Returns true
/// once the profile has been saved, which is the caller's cue to close the overlay.
pub fn show_controls_create_settings(global_settings: &mut GlobalSettings, edit_context: &mut ControlsEditContext, binding: &mut KeyBinding) -> bool {
    use crate::key_bindings::{HOTKEY_NAMES, KEY_NAMES, NUM_HOTKEYS, NUM_KEYS};
    unsafe {
        let has_error = edit_context.empty_name || edit_context.duplicated_name;
        let mut footer = ImGui::GetFrameHeightWithSpacing();
        if has_error {
            footer += ImGui::GetTextLineHeightWithSpacing();
        }
        let body_height = (ImGui::GetContentRegionAvail().y - footer).max(0.0);

        let fields_sz = ImVec2 { x: 0.0, y: body_height };
        ImGui::BeginChild(c"##controls_fields".as_ptr(), &fields_sz, false, 0);

        if layout_field_button("Profile name", &binding.name_c_str()) {
            binding.name = dialog_input("Profile name", &binding.name, SCE_IME_TYPE_BASIC_LATIN, SCE_IME_DIALOG_TEXTBOX_MODE_DEFAULT, 32);
        }
        ImGui::Spacing();
        ImGui::Separator();

        for i in 0..NUM_KEYS {
            binding_button_row(i as _, KEY_NAMES[i], &mut binding.buttons[i]);
        }

        ImGui::Spacing();
        ImGui::Separator();
        ImGui::TextDisabled(c"Hotkeys".as_ptr());
        for i in 0..NUM_HOTKEYS {
            binding_button_row((NUM_KEYS + i) as _, HOTKEY_NAMES[i], &mut binding.hotkeys[i]);
        }

        ImGui::EndChild();

        if has_error {
            ImGui::PushStyleColor(ImGuiCol__ImGuiCol_Text as _, 0xFF0000FF);
            if edit_context.empty_name {
                ImGui::Text(c"Profile name can't be empty".as_ptr());
            } else {
                ImGui::Text(c"A profile with that name already exists".as_ptr());
            }
            ImGui::PopStyleColor(1);
        }

        let vec = ImVec2 { x: -1.0, y: 0.0 };
        if ImGui::Button(c"Save profile".as_ptr(), &vec) {
            *edit_context = ControlsEditContext::default();
            if binding.name.is_empty() {
                edit_context.empty_name = true;
            } else if global_settings.add_custom_controls(binding.clone()) {
                return true;
            } else {
                edit_context.duplicated_name = true;
            }
        }
        false
    }
}

/// Layout fields on the Vita: each is a button opening the numeric IME, since there is
/// no keyboard to type an int input into.
pub fn show_layout_create_settings(global_settings: &mut GlobalSettings, edit_context: &mut ControlsEditContext, layout: &mut CustomLayout) -> bool {
    unsafe {
        if layout_field_button("Name", &layout.name_c_str()) {
            layout.name = dialog_input("Name", &layout.name, SCE_IME_TYPE_BASIC_LATIN, SCE_IME_DIALOG_TEXTBOX_MODE_DEFAULT, 32);
        }

        let mut number_field = |label: &str, value: &mut u16| {
            let shown = CString::new(value.to_string()).unwrap_or_default();
            if layout_field_button(label, &shown) {
                let entered = dialog_input(label, &value.to_string(), SCE_IME_TYPE_NUMBER, SCE_IME_DIALOG_TEXTBOX_MODE_DEFAULT, 5);
                // A cancelled or non-numeric entry leaves the field as it was.
                if let Ok(parsed) = entered.trim().parse::<u32>() {
                    *value = parsed.min(u16::MAX as u32) as u16;
                }
            }
        };
        number_field("X", &mut layout.pos.0);
        number_field("Y", &mut layout.pos.1);
        number_field("Width", &mut layout.size.0);
        number_field("Height", &mut layout.size.1);

        crate::presenter::ui::finish_layout_edit(global_settings, edit_context, layout)
    }
}

