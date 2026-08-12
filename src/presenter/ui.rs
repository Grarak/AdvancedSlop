// imgui frontend for AdvancedSlop: a rom list on the left with the Global settings page
// above it, a passive preview on the right, a fullscreen per-game page (launch button +
// settings tabs) behind each rom, and a modal pause menu over the frozen frame.
use crate::core::gpu::GbaRenderer;
use crate::presenter::imgui::root::{
    ImDrawData, ImDrawList_AddImage, ImDrawList_AddRect, ImDrawList_AddRectFilled, ImDrawList_AddText, ImGuiCol__ImGuiCol_Text, ImFontAtlas_AddFontFromMemoryTTF, ImFontAtlas_GetGlyphRangesDefault, ImFontConfig, ImFontConfig_ImFontConfig, ImGui, ImGuiCol__ImGuiCol_Button,
    ImGuiCond__ImGuiSetCond_Always, ImGuiItemFlags__ImGuiItemFlags_Disabled, ImGuiNavInput__ImGuiNavInput_Cancel, ImGuiNavInput__ImGuiNavInput_FocusNext,
    ImGuiNavInput__ImGuiNavInput_FocusPrev, ImGuiStyleVar__ImGuiStyleVar_Alpha, ImGuiWindowFlags__ImGuiWindowFlags_AlwaysAutoResize, ImGuiWindowFlags__ImGuiWindowFlags_NoBringToFrontOnFocus,
    ImGuiWindowFlags__ImGuiWindowFlags_NoCollapse, ImGuiWindowFlags__ImGuiWindowFlags_NoFocusOnAppearing, ImGuiWindowFlags__ImGuiWindowFlags_NoMove, ImGuiWindowFlags__ImGuiWindowFlags_NoResize,
    ImGuiWindowFlags__ImGuiWindowFlags_NoTitleBar, ImVec2,
};
use crate::global_settings::GlobalSettings;
use crate::key_bindings::KeyBinding;
use crate::presenter::{default_key_binding, show_controls_create_settings, show_layout_create_settings, show_retroachievements_settings, text_input_field, PRESENTER_SCREEN_HEIGHT, PRESENTER_SCREEN_WIDTH};
use crate::ra_context::RaContext;
use crate::screen_layout::CustomLayout;
use crate::settings::{Setting, SettingGroup, SettingValue, SettingsConfig};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::{fs, ptr};
use strum::IntoEnumIterator;

pub trait UiBackend {
    fn init(&mut self);
    // false = the window/app wants to close
    fn new_frame(&mut self) -> bool;
    fn render_draw_data(&mut self, draw_data: *mut ImDrawData);
    fn swap_window(&mut self);
}

const SCREEN_WIDTH: f32 = PRESENTER_SCREEN_WIDTH as f32;
const SCREEN_HEIGHT: f32 = PRESENTER_SCREEN_HEIGHT as f32;

const WINDOW_FLAGS: u32 =
    (ImGuiWindowFlags__ImGuiWindowFlags_NoTitleBar | ImGuiWindowFlags__ImGuiWindowFlags_NoResize | ImGuiWindowFlags__ImGuiWindowFlags_NoMove | ImGuiWindowFlags__ImGuiWindowFlags_NoCollapse) as u32;


const MODAL_FLAGS: u32 = WINDOW_FLAGS | ImGuiWindowFlags__ImGuiWindowFlags_AlwaysAutoResize as u32;

// NoFocusOnAppearing/NoBringToFrontOnFocus: the two
// browse panes must never steal focus or draw over an overlay they appear behind —
// without these, whichever pane is submitted last would grab nav focus at boot, and a
// pane re-appearing under an open overlay would wedge it (Back can't close an
// unfocused overlay).
const PANEL_FLAGS: u32 = WINDOW_FLAGS | ImGuiWindowFlags__ImGuiWindowFlags_NoBringToFrontOnFocus as u32 | ImGuiWindowFlags__ImGuiWindowFlags_NoFocusOnAppearing as u32;

// Back/cancel is Circle on the Vita gamepad, Escape on the keyboard.
#[cfg(target_os = "vita")]
const BACK_HINT: &std::ffi::CStr = c"Press Circle to go back";
#[cfg(not(target_os = "vita"))]
const BACK_HINT: &std::ffi::CStr = c"Press Escape to go back";

/// One-time global style tweaks: rounded frames, and padding/spacing tightened well
/// below the imgui defaults so more fits on the 960x544 screen.
unsafe fn setup_style() {
    let style = &mut *ImGui::GetStyle();
    style.WindowRounding = 0.0; // fullscreen panels look better square
    style.ChildRounding = 6.0;
    style.FrameRounding = 6.0;
    style.PopupRounding = 8.0;
    style.GrabRounding = 6.0;
    style.ScrollbarRounding = 8.0;
    style.WindowBorderSize = 0.0;
    style.FrameBorderSize = 0.0;
    style.PopupBorderSize = 0.0;
    style.WindowPadding = ImVec2 { x: 10.0, y: 8.0 };
    style.FramePadding = ImVec2 { x: 8.0, y: 4.0 };
    style.ItemSpacing = ImVec2 { x: 7.0, y: 5.0 };
    style.ItemInnerSpacing = ImVec2 { x: 6.0, y: 4.0 };
    style.ScrollbarSize = 12.0;
    style.GrabMinSize = 10.0;
    style.ButtonTextAlign = ImVec2 { x: 0.5, y: 0.5 };
}

/// One-time imgui setup: create the context, dark theme, OpenSans (the built-in font is
/// tiny on the 960x544 screen), the style, then the platform backend. Call once in
/// Presenter::new.
pub fn init_ui(ui_backend: &mut impl UiBackend) {
    unsafe {
        ImGui::CreateContext(ptr::null_mut());
        ImGui::StyleColorsDark(ptr::null_mut());
        setup_style();

        let font = include_bytes!("../../font/OpenSans-Regular.ttf");
        let mut config: ImFontConfig = std::mem::zeroed();
        ImFontConfig_ImFontConfig(&mut config);
        config.FontDataOwnedByAtlas = false;
        ImFontAtlas_AddFontFromMemoryTTF(
            (*ImGui::GetIO()).Fonts,
            font.as_ptr() as _,
            font.len() as _,
            22.0,
            &config,
            ImFontAtlas_GetGlyphRangesDefault((*ImGui::GetIO()).Fonts),
        );

        ui_backend.init();
    }
}

unsafe fn begin_window_with(id: &std::ffi::CStr, x: f32, y: f32, w: f32, h: f32, flags: u32) -> bool {
    let pos = ImVec2 { x, y };
    let pivot = ImVec2 { x: 0.0, y: 0.0 };
    ImGui::SetNextWindowPos(&pos, ImGuiCond__ImGuiSetCond_Always as _, &pivot);
    let sz = ImVec2 { x: w, y: h };
    ImGui::SetNextWindowSize(&sz, ImGuiCond__ImGuiSetCond_Always as _);
    ImGui::Begin(id.as_ptr() as _, ptr::null_mut(), flags as _)
}

unsafe fn begin_window(id: &std::ffi::CStr, x: f32, y: f32, w: f32, h: f32) -> bool {
    begin_window_with(id, x, y, w, h, WINDOW_FLAGS)
}

unsafe fn begin_fullscreen_overlay(id: &std::ffi::CStr) -> bool {
    // Opaque background: these overlays cover the frozen game frame, which would show
    // through imgui's translucent default window background. SetNextWindowBgAlpha rather
    // than a style push: imgui compares the style stack depth at Begin against the depth
    // at End, so a push/pop straddling Begin trips its PushStyleColor/PopStyleColor
    // mismatch assert.
    ImGui::SetNextWindowBgAlpha(1.0);
    begin_window(id, 0.0, 0.0, SCREEN_WIDTH, SCREEN_HEIGHT)
}

unsafe fn full_width_button(label: &std::ffi::CStr) -> bool {
    let sz = ImVec2 { x: -1.0, y: 0.0 };
    ImGui::Button(label.as_ptr() as _, &sz)
}

/// Horizontally centers `text` within the content region and draws it.
unsafe fn centered_text(text: &std::ffi::CStr) {
    let w = ImGui::CalcTextSize(text.as_ptr(), ptr::null(), false, 0.0).x;
    let avail = ImGui::GetContentRegionAvail().x;
    if avail > w {
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - w) * 0.5);
    }
    ImGui::Text(text.as_ptr() as _);
}

/// Large centered heading followed by a separator. Top of a dialog/overlay.
unsafe fn dialog_title(text: &std::ffi::CStr) {
    ImGui::SetWindowFontScale(1.4);
    centered_text(text);
    ImGui::SetWindowFontScale(1.0);
    ImGui::Spacing();
    ImGui::Separator();
    ImGui::Spacing();
}

/// Centered, fixed-width button for vertical menus.
unsafe fn menu_button(label: &std::ffi::CStr, width: f32) -> bool {
    let avail = ImGui::GetContentRegionAvail().x;
    if avail > width {
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - width) * 0.5);
    }
    let sz = ImVec2 { x: width, y: 42.0 };
    ImGui::Button(label.as_ptr() as _, &sz)
}

/// Centers the next window on screen — call before `BeginPopupModal`.
unsafe fn center_next_window() {
    let center = ImVec2 {
        x: SCREEN_WIDTH * 0.5,
        y: SCREEN_HEIGHT * 0.5,
    };
    let pivot = ImVec2 { x: 0.5, y: 0.5 };
    ImGui::SetNextWindowPos(&center, ImGuiCond__ImGuiSetCond_Always as _, &pivot);
}

/// Dim, centered "go back" hint, drawn at the current cursor.
unsafe fn back_hint() {
    let w = ImGui::CalcTextSize(BACK_HINT.as_ptr(), ptr::null(), false, 0.0).x;
    let avail = ImGui::GetContentRegionAvail().x;
    if avail > w {
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - w) * 0.5);
    }
    ImGui::TextDisabled(BACK_HINT.as_ptr() as _);
}

unsafe fn nav_input_pressed(input: u32) -> bool {
    // Edge-triggered: imgui sets DownDuration to 0.0 only on the frame the input is
    // first pressed (-1.0 when up, increasing while held). Using the raw NavInputs value
    // instead would fire every frame the input is held.
    (*ImGui::GetIO()).NavInputsDownDuration[input as usize] == 0.0
}

unsafe fn cancel_pressed() -> bool {
    nav_input_pressed(ImGuiNavInput__ImGuiNavInput_Cancel)
}

/// True if the cancel/back press should leave the current overlay rather than step back
/// a level. imgui processes Cancel in NewFrame *before* our code: if nav was inside a
/// child/popup it already popped one level this frame, so the overlay must only close
/// when it was itself the focused window last frame. Callers pass the previous frame's
/// `IsWindowFocused` measurement.
unsafe fn back_closes_overlay(prev_overlay_focused: bool) -> bool {
    cancel_pressed() && prev_overlay_focused
}

/// A zero-height, full-width invisible button that gamepad nav can focus, used to let
/// nav scroll past the first/last real control to the list's edge.
unsafe fn nav_scroll_stop(id: &std::ffi::CStr) {
    let sz = ImVec2 {
        x: ImGui::GetContentRegionAvail().x.max(1.0),
        y: 1.0,
    };
    ImGui::InvisibleButton(id.as_ptr() as _, &sz);
}

/// Renders one setting: the title, its description wrapped on the left, and the control
/// bottom-aligned to the right of the description, then a separator.
///
/// The control sits at the bottom of the description (rather than the description being
/// pinned in a footer) so gamepad nav, which only scrolls far enough to reveal the
/// focused control, always brings the whole description into view too.
unsafe fn render_setting(setting: &mut Setting, id: usize, dirty: &mut bool, buttons_width: f32) {
    const COMBO_WIDTH: f32 = 200.0;

    let title = CString::new(setting.title).unwrap_or_default();
    ImGui::Text(title.as_ptr() as _);

    let style = &*ImGui::GetStyle();
    let control_w = match setting.value {
        SettingValue::Bool(_) => buttons_width,
        _ => COMBO_WIDTH,
    };
    let region_x = ImGui::GetCursorPosX();
    let avail = ImGui::GetContentRegionAvail().x;
    let desc_top = ImGui::GetCursorPosY();

    // Description on the left, wrapped so it never runs under the control column.
    if !setting.description.is_empty() {
        let wrap = region_x + (avail - control_w - style.ItemSpacing.x).max(1.0);
        ImGui::PushTextWrapPos(wrap);
        let description = CString::new(setting.description).unwrap_or_default();
        ImGui::TextDisabled(description.as_ptr() as _);
        ImGui::PopTextWrapPos();
    }
    let desc_bottom = ImGui::GetCursorPosY();

    // Bottom-align the control to the last line of the description.
    let control_h = ImGui::GetFrameHeight();
    let control_y = (desc_bottom - control_h).max(desc_top);
    ImGui::SetCursorPosX(region_x + avail - control_w);
    ImGui::SetCursorPosY(control_y);

    ImGui::PushID3(id as _);
    match &mut setting.value {
        SettingValue::Bool(_) => {
            let value = CString::new(setting.value.to_string()).unwrap_or_default();
            let sz = ImVec2 { x: control_w, y: 0.0 };
            if ImGui::Button(value.as_ptr() as _, &sz) {
                setting.value.next();
                *dirty = true;
            }
        }
        SettingValue::List(inner) => {
            if inner.selection >= inner.values.len() {
                inner.selection = 0;
            }
            let value = CString::new(inner.values[inner.selection].as_str()).unwrap_or_default();
            let combo_id = CString::new(format!("##{id}_list")).unwrap_or_default();
            // Constrain the combo to control_w so it doesn't overrun the window edge
            // (which would make the window horizontally scrollable).
            ImGui::PushItemWidth(control_w);
            if ImGui::BeginCombo(combo_id.as_ptr() as _, value.as_ptr() as _, 0) {
                for (j, val) in inner.values.iter().enumerate() {
                    let is_selected = j == inner.selection;
                    let val_cstr = CString::new(val.as_str()).unwrap_or_default();
                    let sz = ImVec2 { x: 0.0, y: 0.0 };
                    if ImGui::Selectable(val_cstr.as_ptr() as _, is_selected, 0, &sz) {
                        inner.selection = j;
                        *dirty = true;
                    }
                    if is_selected {
                        ImGui::SetItemDefaultFocus();
                    }
                }
                ImGui::EndCombo();
            }
            ImGui::PopItemWidth();
        }
        SettingValue::Int(_) => {}
        SettingValue::Slider(inner) => {
            let slider_id = CString::new(format!("##{id}_slider")).unwrap_or_default();
            ImGui::PushItemWidth(control_w);
            let mut value = inner.value;
            if ImGui::SliderInt(slider_id.as_ptr() as _, &mut value, inner.min, inner.max, c"%d".as_ptr()) {
                inner.value = value.clamp(inner.min, inner.max);
                *dirty = true;
            }
            ImGui::PopItemWidth();
        }
    }
    ImGui::PopID();

    // Drop below whichever of description / control reaches lower, then divide.
    ImGui::SetCursorPosY(desc_bottom.max(control_y + control_h));
    ImGui::Spacing();
    ImGui::Separator();
    ImGui::Spacing();
}

/// Renders the active tab's settings.
unsafe fn render_tab_settings(settings_config: &mut SettingsConfig, group: SettingGroup, only_runtime: bool) {
    let all = settings_config.settings.get_all_mut();
    for (i, setting) in all.iter_mut().enumerate() {
        if setting.group != group || (only_runtime && !setting.runtime) {
            continue;
        }
        // A list nothing has populated yet (Controls, until there's a profile ui) has no
        // value to show in the combo
        if matches!(&setting.value, SettingValue::List(inner) if inner.values.is_empty()) {
            continue;
        }
        render_setting(setting, i, &mut settings_config.dirty, 50.0);
    }
}

/// Manual tab bar for the settings categories (imgui 1.61 has no TabBar API): a row of
/// equal-width buttons, the active one highlighted. Sets `*active` to the clicked tab.
unsafe fn settings_tab_bar(active: &mut usize) {
    let spacing = (*ImGui::GetStyle()).ItemSpacing.x;
    let n = SettingGroup::iter().len() as f32;
    let total = ImGui::GetContentRegionAvail().x;
    let w = (total - spacing * (n - 1.0)) / n;
    for (i, group) in SettingGroup::iter().enumerate() {
        if i > 0 {
            ImGui::SameLine(0.0, -1.0);
        }
        let is_active = *active == i;
        if is_active {
            ImGui::PushStyleColor(ImGuiCol__ImGuiCol_Button as _, 0xFFCC8844u32);
        }
        let label = CString::new(<&str>::from(group)).unwrap_or_default();
        let sz = ImVec2 { x: w, y: 0.0 };
        if ImGui::Button(label.as_ptr() as _, &sz) {
            *active = i;
        }
        if is_active {
            ImGui::PopStyleColor(1);
        }
    }
}

/// Renders the tabbed settings body: the category tab bar and the active tab's settings
/// (each with its own inline description) in a scroll child. `button_reserve` is extra
/// height to keep free below for a caller button (e.g. Save); 0 if none.
unsafe fn render_settings_tabs(settings_config: &mut SettingsConfig, active_tab: &mut usize, only_runtime: bool, button_reserve: f32) {
    // L / R shoulder buttons cycle through the category tabs. imgui's gamepad backend
    // maps the shoulders to FocusPrev/FocusNext; nothing else consumes them here, so
    // read them directly (edge-triggered).
    let n = SettingGroup::iter().len();
    if nav_input_pressed(ImGuiNavInput__ImGuiNavInput_FocusPrev) {
        *active_tab = (*active_tab + n - 1) % n;
    }
    if nav_input_pressed(ImGuiNavInput__ImGuiNavInput_FocusNext) {
        *active_tab = (*active_tab + 1) % n;
    }

    settings_tab_bar(active_tab);
    ImGui::Separator();

    let child_sz = ImVec2 { x: 0.0, y: -button_reserve };
    if ImGui::BeginChild(c"##settings_scroll".as_ptr() as _, &child_sz, false, 0) {
        // Invisible nav-stops above the first / below the last setting. Gamepad nav only
        // scrolls far enough to reveal the focused item; since a setting's control sits
        // at the bottom of its (taller) description block, focusing the first/last
        // control alone never exposes the very top/bottom of the list. These give nav
        // something to land on past either end.
        nav_scroll_stop(c"##top_stop");
        render_tab_settings(settings_config, SettingGroup::iter().nth(*active_tab).unwrap(), only_runtime);
        nav_scroll_stop(c"##bottom_stop");
    }
    ImGui::EndChild();
}

/// Full-width "Save settings", dimmed and unclickable until something changed.
unsafe fn save_settings_button(settings_config: &mut SettingsConfig) {
    let dirty = settings_config.dirty;
    if !dirty {
        ImGui::PushItemFlag(ImGuiItemFlags__ImGuiItemFlags_Disabled as _, true);
        ImGui::PushStyleVar(ImGuiStyleVar__ImGuiStyleVar_Alpha as _, (*ImGui::GetStyle()).Alpha * 0.5);
    }
    if full_width_button(c"Save settings") {
        settings_config.flush();
    }
    if !dirty {
        ImGui::PopItemFlag();
        ImGui::PopStyleVar(1);
    }
}

/// Clears the framebuffer and draws imgui's frame into it.
unsafe fn present_frame(ui_backend: &mut impl UiBackend) {
    gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
    gl::Viewport(0, 0, PRESENTER_SCREEN_WIDTH as _, PRESENTER_SCREEN_HEIGHT as _);
    ImGui::Render();
    ui_backend.render_draw_data(ImGui::GetDrawData());
    ui_backend.swap_window();
}

/// One frame of a centered progress dialog (used for the rom load). `progress`/`total`
/// drive the bar; a total of 0 shows an empty bar.
pub fn show_progress(title: &str, progress: usize, total: usize, ui_backend: &mut impl UiBackend) {
    unsafe {
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        gl::Viewport(0, 0, PRESENTER_SCREEN_WIDTH as _, PRESENTER_SCREEN_HEIGHT as _);
        gl::ClearColor(0.0, 0.0, 0.0, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);

        ui_backend.new_frame();

        center_next_window();
        if ImGui::BeginPopupModal(c"ProgressPopup".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
            let title = CString::new(title).unwrap_or_default();
            dialog_title(&title);
            const BAR_WIDTH: f32 = 440.0;
            let avail = ImGui::GetContentRegionAvail().x;
            if avail > BAR_WIDTH {
                ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - BAR_WIDTH) * 0.5);
            }
            let fraction = if total == 0 { 0.0 } else { progress as f32 / total as f32 };
            let sz = ImVec2 { x: BAR_WIDTH, y: 28.0 };
            ImGui::ProgressBar(fraction, &sz, ptr::null());
            ImGui::EndPopup();
        }
        ImGui::OpenPopup(c"ProgressPopup".as_ptr());

        present_frame(ui_backend);
    }
}

/// One frame of the savestate progress dialog, drawn over the frozen game frame while
/// the main loop waits for the cpu thread to finish a menu-requested save or load.
pub fn show_savestate_progress(ui_backend: &mut impl UiBackend, renderer: &GbaRenderer, text: impl AsRef<str>, progress: usize) {
    unsafe {
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        gl::Viewport(0, 0, PRESENTER_SCREEN_WIDTH as _, PRESENTER_SCREEN_HEIGHT as _);
        gl::ClearColor(0.0, 0.0, 0.0, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);
        renderer.blit_main_framebuffer();

        ui_backend.new_frame();

        center_next_window();
        if ImGui::BeginPopupModal(c"SavestateProgressPopup".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
            dialog_title(c"Savestate");
            let text = CString::new(text.as_ref()).unwrap_or_default();
            centered_text(&text);
            ImGui::Spacing();
            const BAR_WIDTH: f32 = 440.0;
            let avail = ImGui::GetContentRegionAvail().x;
            if avail > BAR_WIDTH {
                ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - BAR_WIDTH) * 0.5);
            }
            let sz = ImVec2 { x: BAR_WIDTH, y: 28.0 };
            ImGui::ProgressBar(progress as f32 / 100.0, &sz, ptr::null());
            ImGui::EndPopup();
        }
        ImGui::OpenPopup(c"SavestateProgressPopup".as_ptr());

        present_frame(ui_backend);
    }
}

/// A savestate file as the list shows it: the decoded thumbnail plus the pre-built
/// C strings, so nothing is formatted per frame.
struct SavestateUiEntry {
    path: PathBuf,
    label: CString,
    detail: CString,
    // Row-spanning selectable id; the visible content is drawlist-drawn on top
    sel_id: CString,
    texture: u32,
    // False for states written by another build: shown so they can be seen and deleted,
    // but Load is disabled because the field walk would decode into the wrong layout.
    loadable: bool,
    // User-given name, empty when never renamed; seeds the rename field.
    label_text: String,
}

fn rgb_to_rgba(rgb: &[u8], pixels: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; pixels * 4];
    for i in 0..pixels {
        rgba[i * 4..i * 4 + 3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
        rgba[i * 4 + 3] = 0xFF;
    }
    rgba
}

fn decode_screenshot_jpeg(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let data = decoder.decode().ok()?;
    let info = decoder.info()?;
    if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
        return None;
    }
    let pixels = info.width as usize * info.height as usize;
    Some((info.width as u32, info.height as u32, rgb_to_rgba(&data, pixels)))
}

/// Decode an embedded screenshot into a GL texture; 0 on any failure (the entry then
/// renders without a thumbnail).
unsafe fn create_screenshot_texture(bytes: &[u8]) -> u32 {
    let Some((width, height, data)) = (match bytes {
        [0xFF, 0xD8, ..] => decode_screenshot_jpeg(bytes),
        _ => None,
    }) else {
        return 0;
    };
    let mut tex = 0;
    gl::GenTextures(1, &mut tex);
    gl::BindTexture(gl::TEXTURE_2D, tex);
    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as _);
    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as _);
    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as _);
    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as _);
    gl::TexImage2D(gl::TEXTURE_2D, 0, gl::RGBA as _, width as _, height as _, 0, gl::RGBA, gl::UNSIGNED_BYTE, data.as_ptr() as _);
    gl::BindTexture(gl::TEXTURE_2D, 0);
    tex
}

/// Scan `<rom_dir>/savestates/` for this rom's states: the exit-resume slot first, then
/// the quick slot (the one the hotkey keeps rewriting), then the numbered ones newest
/// first — most-likely-wanted at the top, which is also where nav lands. Files that don't parse as a savestate of this build are
/// skipped rather than listed as broken.
unsafe fn load_savestate_entries(rom_path: &Path) -> Vec<SavestateUiEntry> {
    use crate::savestate::SlotKind;
    let mut entries = Vec::new();
    let Ok(read_dir) = fs::read_dir(crate::savestate::savestates_dir(rom_path)) else {
        return entries;
    };

    let mut found = Vec::new();
    for dir_entry in read_dir.flatten() {
        let name = dir_entry.file_name().to_string_lossy().into_owned();
        let Some(kind) = crate::savestate::classify_slot(rom_path, &name) else { continue };
        found.push((kind, dir_entry.path()));
    }
    // Quick slot pinned to the top; numbered descending below it.
    // Resume-on-launch first, then the hotkey slot, then numbered descending. Grouped by
    // a tuple rather than folding the rank into the number: any `MAX - num + rank` form
    // overflows once rank exceeds the smallest slot number, and slots start at 1.
    found.sort_by_key(|(kind, _)| match kind {
        SlotKind::Auto => (0u8, 0u32),
        SlotKind::Quick => (1, 0),
        SlotKind::Numbered(num) => (2, u32::MAX - num),
    });

    for (kind, path) in found {
        let (texture, loadable, note, label_text) = match crate::savestate::peek(&path) {
            crate::savestate::PeekResult::Ok(meta) => (create_screenshot_texture(&meta.screenshot), true, String::new(), meta.label),
            crate::savestate::PeekResult::VersionMismatch(version) => (0, false, format!(" - made by a different version (v{version})"), String::new()),
            crate::savestate::PeekResult::Unreadable => continue,
        };
        let metadata = fs::metadata(&path).ok();
        let modified = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|time| chrono::DateTime::<chrono::Local>::from(time).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let size = metadata.map(|m| format!(" - {}", crate::savestate::format_size(m.len()))).unwrap_or_default();
        let (label, id) = match kind {
            SlotKind::Auto => ("Resume (saved on exit)".to_string(), "auto".to_string()),
            SlotKind::Quick => ("Quick savestate".to_string(), "quick".to_string()),
            SlotKind::Numbered(num) => (format!("Savestate {num}"), num.to_string()),
        };
        // A renamed state shows its name; the slot it lives in moves into the detail line
        let (shown, detail_prefix) = if label_text.is_empty() {
            (label, String::new())
        } else {
            (label_text.clone(), format!("{label} - "))
        };
        entries.push(SavestateUiEntry {
            texture,
            path,
            label: CString::new(shown).unwrap(),
            detail: CString::new(format!("{detail_prefix}{modified}{size}{note}")).unwrap(),
            sel_id: CString::new(format!("##savestate{id}")).unwrap(),
            loadable,
            label_text,
        });
    }
    entries
}

unsafe fn free_savestate_entries(entries: &mut Vec<SavestateUiEntry>) {
    for entry in entries.iter() {
        if entry.texture != 0 {
            gl::DeleteTextures(1, &entry.texture);
        }
    }
    entries.clear();
}

enum SavestateUiAction {
    Create,
    Overwrite(PathBuf),
    Load(PathBuf),
    Delete(PathBuf),
    Rename(PathBuf, String),
}

/// Which actions the list can offer. The rom browser has no running emulator behind it,
/// so it can only start a game from a state — creating or overwriting one needs a
/// machine to snapshot.
#[derive(Copy, Clone, Eq, PartialEq)]
enum SavestateUiMode {
    InGame,
    Browser,
}

/// Full-screen savestate list: thumbnail and name/date per entry, create button on top.
/// Clicking an entry opens a modal offering Load, Overwrite or Delete.
unsafe fn render_savestate_overlay(entries: &[SavestateUiEntry], selected: &mut Option<usize>, confirming_delete: &mut bool, renaming: &mut Option<String>, mode: SavestateUiMode) -> Option<SavestateUiAction> {
    let mut action = None;

    if mode == SavestateUiMode::InGame {
        if full_width_button(c"Create new savestate") {
            action = Some(SavestateUiAction::Create);
        }
        ImGui::Spacing();
        ImGui::Separator();
        ImGui::Spacing();
    }

    if entries.is_empty() {
        ImGui::Spacing();
        centered_text(c"No savestates yet");
    }

    let child_sz = ImVec2 { x: 0.0, y: -ImGui::GetTextLineHeightWithSpacing() };
    if ImGui::BeginChild(c"##savestate_scroll".as_ptr() as _, &child_sz, false, 0) {
        // Thumbnails keep the 3:2 GBA aspect
        const THUMB_W: f32 = 192.0;
        const THUMB_H: f32 = 128.0;
        for (i, entry) in entries.iter().enumerate() {
            // One row-spanning Selectable so the entry is reachable by gamepad/keyboard
            // nav (a plain widget group never receives nav focus); the thumbnail and
            // texts are drawlist-drawn over it so no other item competes for
            // hover/clicks.
            let origin = ImGui::GetCursorScreenPos();
            let sel_sz = ImVec2 {
                x: ImGui::GetContentRegionAvail().x,
                y: THUMB_H,
            };
            if ImGui::Selectable(entry.sel_id.as_ptr(), false, 0, &sel_sz) {
                *selected = Some(i);
                // Each dialog opens on its first page, never mid-confirmation or mid-rename
                *confirming_delete = false;
                *renaming = None;
            }

            let dl = ImGui::GetWindowDrawList();
            if entry.texture != 0 {
                let thumb_min = origin;
                let thumb_max = ImVec2 {
                    x: origin.x + THUMB_W,
                    y: origin.y + THUMB_H,
                };
                let uv0 = ImVec2 { x: 0.0, y: 0.0 };
                let uv1 = ImVec2 { x: 1.0, y: 1.0 };
                ImDrawList_AddImage(dl, entry.texture as _, &thumb_min, &thumb_max, &uv0, &uv1, 0xFFFFFFFF);
                ImDrawList_AddRect(dl, &thumb_min, &thumb_max, 0xFF4D4D4D, 0.0, 0, 1.0);
            }
            let line_height = ImGui::GetTextLineHeightWithSpacing();
            let label_pos = ImVec2 {
                x: origin.x + THUMB_W + 14.0,
                y: origin.y + 4.0,
            };
            let detail_pos = ImVec2 {
                x: label_pos.x,
                y: label_pos.y + line_height * 1.3,
            };
            ImDrawList_AddText(dl, &label_pos, 0xFFFFFFFF, entry.label.as_ptr(), ptr::null());
            ImDrawList_AddText(dl, &detail_pos, 0xFFCCCCCC, entry.detail.as_ptr(), ptr::null());

            ImGui::Spacing();
            ImGui::Separator();
            ImGui::Spacing();
        }
        nav_scroll_stop(c"##savestate_bottom_stop");
    }
    ImGui::EndChild();
    back_hint();

    // Load-or-delete dialog for the clicked entry
    if let Some(i) = *selected {
        let entry = &entries[i];
        ImGui::OpenPopup(c"SavestateActionPopup".as_ptr());
        center_next_window();
        if ImGui::BeginPopupModal(c"SavestateActionPopup".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
            const BUTTON_WIDTH: f32 = 260.0;
            if let Some(buf) = renaming {
                dialog_title(c"Rename savestate");
                text_input_field(c"Name", buf, crate::savestate::MAX_LABEL_LEN);
                ImGui::Spacing();
                if menu_button(c"Save name", BUTTON_WIDTH) {
                    action = Some(SavestateUiAction::Rename(entry.path.clone(), buf.clone()));
                    *selected = None;
                    *renaming = None;
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Cancel", BUTTON_WIDTH) || cancel_pressed() {
                    *renaming = None;
                }
                ImGui::EndPopup();
                return action;
            }
            if *confirming_delete {
                // Deleting a state is unrecoverable and the button sits in a pad-navigated
                // list, so it takes a second, explicit press.
                dialog_title(c"Delete savestate?");
                centered_text(&entry.label);
                centered_text(c"This cannot be undone.");
                ImGui::Spacing();
                if menu_button(c"Delete", BUTTON_WIDTH) {
                    action = Some(SavestateUiAction::Delete(entry.path.clone()));
                    *selected = None;
                    *confirming_delete = false;
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Cancel", BUTTON_WIDTH) || cancel_pressed() {
                    *confirming_delete = false;
                }
                ImGui::EndPopup();
                return action;
            }

            dialog_title(&entry.label);
            centered_text(&entry.detail);
            ImGui::Spacing();
            // A state from another build can be listed and deleted but never loaded
            if !entry.loadable {
                ImGui::PushItemFlag(ImGuiItemFlags__ImGuiItemFlags_Disabled as _, true);
                ImGui::PushStyleVar(ImGuiStyleVar__ImGuiStyleVar_Alpha as _, (*ImGui::GetStyle()).Alpha * 0.5f32);
            }
            if menu_button(if mode == SavestateUiMode::Browser { c"Launch from here" } else { c"Load" }, BUTTON_WIDTH) && entry.loadable {
                action = Some(SavestateUiAction::Load(entry.path.clone()));
                *selected = None;
                ImGui::CloseCurrentPopup();
            }
            if !entry.loadable {
                ImGui::PopItemFlag();
                ImGui::PopStyleVar(1);
            }
            // Re-save in place, so a slot can be reused instead of every save adding a
            // file. Between Load and Delete on purpose: it is the one you reach for after
            // Load, and it keeps a misnav off Delete's neighbour.
            if mode == SavestateUiMode::InGame && menu_button(c"Overwrite", BUTTON_WIDTH) {
                action = Some(SavestateUiAction::Overwrite(entry.path.clone()));
                *selected = None;
                ImGui::CloseCurrentPopup();
            }
            if entry.loadable && menu_button(c"Rename", BUTTON_WIDTH) {
                *renaming = Some(entry.label_text.clone());
            }
            if menu_button(c"Delete", BUTTON_WIDTH) {
                *confirming_delete = true;
            }
            if menu_button(c"Cancel", BUTTON_WIDTH) || cancel_pressed() {
                *selected = None;
                ImGui::CloseCurrentPopup();
            }
            ImGui::EndPopup();
        }
    }

    action
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum UiPauseMenuReturn {
    Resume,
    QuitToMenu,
    QuitApp,
}

/// Blocking pause menu, drawn over the frozen game frame: a modal button list, with the
/// settings on their own fullscreen overlay and a confirmation before quitting.
/// `settings_config` is edited in place; the caller copies it into the live emulator.
pub fn show_pause_menu(ui_backend: &mut impl UiBackend, renderer: &GbaRenderer, settings_config: &mut SettingsConfig, rom_path: &Path) -> UiPauseMenuReturn {
    let mut pressed_settings = false;
    let mut pressed_savestates = false;
    let mut savestate_entries: Vec<SavestateUiEntry> = Vec::new();
    let mut savestate_selected: Option<usize> = None;
    let mut savestate_confirm_delete = false;
    let mut savestate_renaming: Option<String> = None;
    let mut pressed_quit = false;
    let mut pressed_exit = false;
    let mut return_value = None;
    let mut active_tab: usize = 0;
    let mut overlay_focused = true;
    loop {
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::Viewport(0, 0, PRESENTER_SCREEN_WIDTH as _, PRESENTER_SCREEN_HEIGHT as _);
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            renderer.blit_main_framebuffer();

            if !ui_backend.new_frame() {
                return UiPauseMenuReturn::QuitApp;
            }

            center_next_window();
            if ImGui::BeginPopupModal(c"PausePopup".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
                dialog_title(c"Paused");
                const BUTTON_WIDTH: f32 = 260.0;
                if menu_button(c"Resume", BUTTON_WIDTH) {
                    return_value = Some(UiPauseMenuReturn::Resume);
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Settings", BUTTON_WIDTH) {
                    pressed_settings = true;
                    active_tab = 0;
                    overlay_focused = true;
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Savestates", BUTTON_WIDTH) {
                    pressed_savestates = true;
                    overlay_focused = true;
                    savestate_selected = None;
                    savestate_entries = load_savestate_entries(rom_path);
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Quit game", BUTTON_WIDTH) {
                    pressed_quit = true;
                    ImGui::CloseCurrentPopup();
                }
                if menu_button(c"Exit emulator", BUTTON_WIDTH) {
                    pressed_exit = true;
                    ImGui::CloseCurrentPopup();
                }
                ImGui::EndPopup();
            }

            center_next_window();
            if ImGui::BeginPopupModal(c"QuitPopup".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
                dialog_title(if pressed_exit { c"Exit emulator?" } else { c"Quit game?" });
                if settings_config.settings.savestate_on_exit() {
                    centered_text(c"A savestate will be written so you can resume.");
                } else {
                    centered_text(c"Unsaved progress will be lost.");
                }
                ImGui::Spacing();
                ImGui::Spacing();

                const BUTTON_WIDTH: f32 = 120.0;
                let spacing = (*ImGui::GetStyle()).ItemSpacing.x;
                let total = BUTTON_WIDTH * 2.0 + spacing;
                let avail = ImGui::GetContentRegionAvail().x;
                if avail > total {
                    ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - total) * 0.5);
                }
                let bsz = ImVec2 { x: BUTTON_WIDTH, y: 44.0 };
                if ImGui::Button(c"No".as_ptr(), &bsz) {
                    pressed_quit = false;
                    pressed_exit = false;
                    ImGui::CloseCurrentPopup();
                }
                ImGui::SameLine(0.0, spacing);
                if ImGui::Button(c"Yes".as_ptr(), &bsz) {
                    // Queued before returning: the cpu thread is still parked at its
                    // vblank hook, and the main loop performs any armed op (showing the
                    // progress dialog) before it tears the game down.
                    if settings_config.settings.savestate_on_exit() {
                        let screenshot = renderer.capture_frame_jpeg();
                        crate::savestate::op_begin();
                        crate::savestate::request_save_with_screenshot(crate::savestate::SaveTarget::Auto, screenshot);
                    }
                    return_value = Some(if pressed_exit { UiPauseMenuReturn::QuitApp } else { UiPauseMenuReturn::QuitToMenu });
                    ImGui::CloseCurrentPopup();
                }
                ImGui::EndPopup();
            }

            if return_value.is_none() {
                if pressed_settings {
                    if begin_fullscreen_overlay(c"##pausesettings") {
                        let padding_y = (*ImGui::GetStyle()).WindowPadding.y;
                        let footer = ImGui::GetTextLineHeightWithSpacing() + ImGui::GetFrameHeight();
                        // Only the runtime settings: the rest can't take effect until the
                        // next launch.
                        render_settings_tabs(settings_config, &mut active_tab, true, footer);

                        // Hint + save pinned to the bottom, so they never float up with
                        // the scroll list above them.
                        ImGui::SetCursorPosY(ImGui::GetWindowHeight() - padding_y - footer);
                        back_hint();
                        save_settings_button(settings_config);

                        // Back/Esc steps out of the settings child first; only leaves the
                        // settings overlay when at the tab level.
                        if back_closes_overlay(overlay_focused) {
                            pressed_settings = false;
                        }
                        overlay_focused = ImGui::IsWindowFocused(0);
                    }
                    ImGui::End();
                } else if pressed_savestates {
                    if begin_fullscreen_overlay(c"##savestates") {
                        match render_savestate_overlay(&savestate_entries, &mut savestate_selected, &mut savestate_confirm_delete, &mut savestate_renaming, SavestateUiMode::InGame) {
                            Some(action @ (SavestateUiAction::Create | SavestateUiAction::Overwrite(_))) => {
                                // Screenshot of the frozen frame behind the menu; the state
                                // itself is written by the cpu thread at the vblank it is
                                // parked in. op_begin arms the progress dialog the main
                                // loop shows meanwhile.
                                let target = match action {
                                    SavestateUiAction::Overwrite(path) => crate::savestate::SaveTarget::Path(path),
                                    _ => crate::savestate::SaveTarget::NewSlot,
                                };
                                let screenshot = renderer.capture_frame_jpeg();
                                crate::savestate::op_begin();
                                crate::savestate::request_save_with_screenshot(target, screenshot);
                                return_value = Some(UiPauseMenuReturn::Resume);
                            }
                            Some(SavestateUiAction::Load(path)) => match fs::read(&path) {
                                Ok(data) => {
                                    crate::savestate::op_begin();
                                    crate::savestate::request_load(data);
                                    return_value = Some(UiPauseMenuReturn::Resume);
                                }
                                Err(err) => eprintln!("Failed to read savestate {path:?}: {err}"),
                            },
                            Some(SavestateUiAction::Delete(path)) => {
                                if let Err(err) = fs::remove_file(&path) {
                                    eprintln!("Failed to delete savestate {path:?}: {err}");
                                }
                                free_savestate_entries(&mut savestate_entries);
                                savestate_entries = load_savestate_entries(rom_path);
                            }
                            // Header-only rewrite on this thread: no emulator involvement,
                            // so the list can just be rescanned right away.
                            Some(SavestateUiAction::Rename(path, name)) => {
                                if let Err(err) = crate::savestate::relabel_file(&path, &name) {
                                    eprintln!("Failed to rename savestate {path:?}: {err}");
                                }
                                free_savestate_entries(&mut savestate_entries);
                                savestate_entries = load_savestate_entries(rom_path);
                            }
                            None => {}
                        }
                        // Close the overlay only when no entry dialog is open
                        if savestate_selected.is_none() && back_closes_overlay(overlay_focused) {
                            pressed_savestates = false;
                            free_savestate_entries(&mut savestate_entries);
                        }
                        overlay_focused = ImGui::IsWindowFocused(0);
                    }
                    ImGui::End();
                } else if pressed_quit || pressed_exit {
                    ImGui::OpenPopup(c"QuitPopup".as_ptr());
                } else {
                    ImGui::OpenPopup(c"PausePopup".as_ptr());
                }
            }

            present_frame(ui_backend);

            if let Some(ret) = return_value {
                free_savestate_entries(&mut savestate_entries);
                return ret;
            }
        }
    }
}

/// What the login form is holding between frames. The password is kept only until the
/// login callback comes back — a successful login stores the server's token instead, so
/// the password never reaches the ini.
#[derive(Default)]
pub struct RALoginContext {
    pub username: String,
    pub password: String,
    pub error: String,
    pub logging_in: bool,
}

/// Errors surfaced by the profile editor while a name is being entered.
#[derive(Default)]
pub struct ControlsEditContext {
    pub empty_name: bool,
    pub duplicated_name: bool,
}

/// The saved-profile list: pick one to delete, or add another. Editing an existing
/// profile is not offered — a profile is small enough to just re-add.
unsafe fn render_controls_settings_overlay(
    show: &mut bool,
    global_settings: &mut GlobalSettings,
    creating: &mut bool,
    edit_context: &mut ControlsEditContext,
    new_binding: &mut KeyBinding,
    selected: &mut Option<usize>,
    overlay_focused: &mut bool,
) {
    if !begin_fullscreen_overlay(c"##controlssettings") {
        ImGui::End();
        return;
    }
    dialog_title(c"Custom controls");

    center_next_window();
    if ImGui::BeginPopupModal(c"customcontrolsmenu".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
        let bsz = ImVec2 { x: 120.0, y: 44.0 };
        if ImGui::Button(c"Delete".as_ptr(), &bsz) {
            if let Some(i) = *selected {
                global_settings.delete_custom_controls(i);
            }
            *selected = None;
            ImGui::CloseCurrentPopup();
        }
        ImGui::SameLine(0.0, (*ImGui::GetStyle()).ItemSpacing.x);
        if ImGui::Button(c"Back".as_ptr(), &bsz) {
            *selected = None;
            ImGui::CloseCurrentPopup();
        }
        ImGui::EndPopup();
    }
    if selected.is_some() {
        ImGui::OpenPopup(c"customcontrolsmenu".as_ptr());
    }

    const BUTTON_WIDTH: f32 = 380.0;
    if global_settings.custom_controls.is_empty() {
        centered_text(c"No custom controls yet.");
        ImGui::Spacing();
    }
    for (i, binding) in global_settings.custom_controls.iter().enumerate() {
        if menu_button(&binding.name_c_str(), BUTTON_WIDTH) {
            *selected = Some(i);
        }
    }

    if menu_button(c"Add custom controls", BUTTON_WIDTH) {
        *creating = true;
        *edit_context = ControlsEditContext::default();
        *new_binding = default_key_binding();
    }

    back_hint();
    if back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// The editor for a new profile. The rows themselves are per-platform
/// (`show_controls_create_settings`) because capturing a button press is.
unsafe fn render_custom_controls_overlay(
    show: &mut bool,
    global_settings: &mut GlobalSettings,
    edit_context: &mut ControlsEditContext,
    new_binding: &mut KeyBinding,
    overlay_focused: &mut bool,
) {
    if !begin_fullscreen_overlay(c"##customcontrols") {
        ImGui::End();
        return;
    }
    dialog_title(c"New controls profile");

    if show_controls_create_settings(global_settings, edit_context, new_binding) {
        *show = false;
    }

    if back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// Preview, error line and Save button — the half of the layout editor that is the same
/// on both platforms. Returns true once the layout has been saved.
pub unsafe fn finish_layout_edit(global_settings: &mut GlobalSettings, edit_context: &mut ControlsEditContext, layout: &mut CustomLayout) -> bool {
    ImGui::Spacing();
    draw_layout_preview(layout);
    ImGui::Spacing();

    if edit_context.empty_name || edit_context.duplicated_name {
        ImGui::PushStyleColor(ImGuiCol__ImGuiCol_Text as _, 0xFF0000FF);
        if edit_context.empty_name {
            ImGui::Text(c"Layout name can't be empty".as_ptr());
        } else {
            ImGui::Text(c"A layout with that name already exists".as_ptr());
        }
        ImGui::PopStyleColor(1);
    }

    if full_width_button(c"Save layout") {
        *edit_context = ControlsEditContext::default();
        if layout.name.is_empty() {
            edit_context.empty_name = true;
        } else if global_settings.add_custom_layout(layout.clone()) {
            return true;
        } else {
            edit_context.duplicated_name = true;
        }
    }
    false
}

/// Scaled outline of where a custom layout puts the screen, so the numbers being typed
/// mean something before the game is launched.
pub(crate) unsafe fn draw_layout_preview(layout: &CustomLayout) {
    const PREVIEW_W: f32 = 320.0;
    let scale = PREVIEW_W / SCREEN_WIDTH;
    let preview_h = SCREEN_HEIGHT * scale;

    let avail = ImGui::GetContentRegionAvail().x;
    if avail > PREVIEW_W {
        ImGui::SetCursorPosX(ImGui::GetCursorPosX() + (avail - PREVIEW_W) * 0.5);
    }
    let origin = ImGui::GetCursorScreenPos();
    let draw_list = ImGui::GetWindowDrawList();

    let bg_max = ImVec2 { x: origin.x + PREVIEW_W, y: origin.y + preview_h };
    ImDrawList_AddRectFilled(draw_list, &origin, &bg_max, 0xFF202020, 0.0, 0);

    let (x, y, w, h) = layout.rect();
    let scr_min = ImVec2 { x: origin.x + x as f32 * scale, y: origin.y + y as f32 * scale };
    let scr_max = ImVec2 { x: scr_min.x + w as f32 * scale, y: scr_min.y + h as f32 * scale };
    ImDrawList_AddRectFilled(draw_list, &scr_min, &scr_max, 0xFFCC8844, 0.0, 0);

    let sz = ImVec2 { x: PREVIEW_W, y: preview_h };
    ImGui::Dummy(&sz);
}

/// The saved-layout list: pick one to delete, or add another.
unsafe fn render_layouts_settings_overlay(
    show: &mut bool,
    global_settings: &mut GlobalSettings,
    creating: &mut bool,
    edit_context: &mut ControlsEditContext,
    new_layout: &mut CustomLayout,
    selected: &mut Option<usize>,
    overlay_focused: &mut bool,
) {
    if !begin_fullscreen_overlay(c"##layoutssettings") {
        ImGui::End();
        return;
    }
    dialog_title(c"Custom layouts");

    center_next_window();
    if ImGui::BeginPopupModal(c"customlayoutsmenu".as_ptr(), ptr::null_mut(), MODAL_FLAGS as _) {
        let bsz = ImVec2 { x: 120.0, y: 44.0 };
        if ImGui::Button(c"Delete".as_ptr(), &bsz) {
            if let Some(i) = *selected {
                global_settings.delete_custom_layout(i);
            }
            *selected = None;
            ImGui::CloseCurrentPopup();
        }
        ImGui::SameLine(0.0, (*ImGui::GetStyle()).ItemSpacing.x);
        if ImGui::Button(c"Back".as_ptr(), &bsz) {
            *selected = None;
            ImGui::CloseCurrentPopup();
        }
        ImGui::EndPopup();
    }
    if selected.is_some() {
        ImGui::OpenPopup(c"customlayoutsmenu".as_ptr());
    }

    const BUTTON_WIDTH: f32 = 380.0;
    if global_settings.custom_layouts.is_empty() {
        centered_text(c"No custom layouts yet.");
        ImGui::Spacing();
    }
    for (i, layout) in global_settings.custom_layouts.iter().enumerate() {
        if menu_button(&layout.name_c_str(), BUTTON_WIDTH) {
            *selected = Some(i);
        }
    }

    if menu_button(c"Add custom layout", BUTTON_WIDTH) {
        *creating = true;
        *edit_context = ControlsEditContext::default();
        *new_layout = CustomLayout::fit_default();
    }

    back_hint();
    if back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// The editor for a new layout. Fields are per-platform (the Vita needs the IME);
/// the preview and the save/validate half are shared.
unsafe fn render_custom_layout_overlay(
    show: &mut bool,
    global_settings: &mut GlobalSettings,
    edit_context: &mut ControlsEditContext,
    new_layout: &mut CustomLayout,
    overlay_focused: &mut bool,
) {
    if !begin_fullscreen_overlay(c"##customlayout") {
        ImGui::End();
        return;
    }
    dialog_title(c"New screen layout");

    if show_layout_create_settings(global_settings, edit_context, new_layout) {
        *show = false;
    }

    if back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// Drain a finished login attempt into the form. On success the server's token — never
/// the password — is what gets persisted, so a later session can log back in silently.
pub fn poll_ra_login(global_settings: &mut GlobalSettings, login_context: &mut RALoginContext, context: &mut RaContext) {
    if !login_context.logging_in {
        return;
    }
    let Some(data) = context.get_login_callback_data() else {
        return;
    };
    login_context.logging_in = false;
    if data.result == rcheevos::RC_OK {
        *login_context = RALoginContext::default();
        if let Some((username, token)) = context.get_user_info() {
            global_settings.set_ra_data(username, token);
        }
    } else {
        login_context.error = data.error_message.unwrap_or_else(|| "Login failed".to_string());
    }
}

/// The settings that belong to the emulator rather than to a game: they live in
/// settings.ini and the custom_*.ini files under the presenter's data path, not in the
/// per-game settings the tabs edit, which is why they are a page of their own rather
/// than another settings tab.
unsafe fn render_global_settings_overlay(show: &mut bool, open_layouts: &mut bool, open_controls: &mut bool, open_ra: &mut bool, overlay_focused: &mut bool) {
    if !begin_fullscreen_overlay(c"##globalsettings") {
        ImGui::End();
        return;
    }
    dialog_title(c"Global settings");

    const BUTTON_WIDTH: f32 = 380.0;
    if menu_button(c"Custom screen layouts", BUTTON_WIDTH) {
        *open_layouts = true;
    }
    if menu_button(c"Custom controls", BUTTON_WIDTH) {
        *open_controls = true;
    }
    if menu_button(c"RetroAchievements", BUTTON_WIDTH) {
        *open_ra = true;
    }

    back_hint();
    if back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// Fullscreen RetroAchievements login/status overlay. The form fields themselves are
/// per-platform (`show_retroachievements_settings`): Linux types into them directly,
/// the Vita has no keyboard and has to bounce through the system IME dialog.
unsafe fn render_ra_settings_overlay(show: &mut bool, global_settings: &mut GlobalSettings, ra_context: &mut RaContext, login_context: &mut RALoginContext, overlay_focused: &mut bool) {
    if !begin_fullscreen_overlay(c"##retroachievements") {
        ImGui::End();
        return;
    }

    dialog_title(c"RetroAchievements");
    show_retroachievements_settings(global_settings, login_context, ra_context);

    let padding_y = (*ImGui::GetStyle()).WindowPadding.y;
    let footer = ImGui::GetTextLineHeightWithSpacing();
    ImGui::SetCursorPosY(ImGui::GetWindowHeight() - padding_y - footer);
    back_hint();

    // Don't let Back close the overlay mid-request: the login callback writes the token
    // into global_settings, and leaving early would drop the result on the floor.
    if !login_context.logging_in && back_closes_overlay(*overlay_focused) {
        *show = false;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

// A .gba in the browse directory, with the display name derived from the file stem.
struct GameEntry {
    path: PathBuf,
    label: CString,
}

fn scan_games(dir: &Path) -> Vec<GameEntry> {
    let Ok(read_dir) = fs::read_dir(dir) else { return Vec::new() };
    let mut games: Vec<GameEntry> = read_dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|ext| ext.eq_ignore_ascii_case("gba")).unwrap_or(false))
        .map(|path| {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("game").to_string();
            GameEntry {
                label: CString::new(stem).unwrap_or_default(),
                path,
            }
        })
        .collect();
    games.sort_by(|a, b| a.path.cmp(&b.path));
    games
}

const LIST_WIDTH: f32 = SCREEN_WIDTH;

/// The rom list: navigation is imgui's own (a Selectable holds
/// the NavId — the vitaGL backend's NavInputs drive it on the pad), the Global settings
/// button sits above the list, and picking an entry opens the fullscreen game detail
/// overlay. Nothing here moves focus by hand: the hand-driven selection this list once
/// had was justified by a qemu observation (no sceCtrl, so the pad looked dead locally)
/// and broke the real Vita — bisected on hardware to the commit that introduced it.
unsafe fn render_game_list(games: &[GameEntry], rom_dir: &Path, hovered: &mut Option<usize>, detail_game: &mut Option<usize>, active_tab: &mut usize, open_global: &mut bool) {
    if !begin_window_with(c"##left", 0.0, 0.0, LIST_WIDTH, SCREEN_HEIGHT, PANEL_FLAGS) {
        ImGui::End();
        return;
    }

    if full_width_button(c"Global settings") {
        *open_global = true;
    }
    if ImGui::IsItemHovered(0) {
        *hovered = None;
    }
    ImGui::Spacing();
    ImGui::Separator();

    let header = if games.is_empty() {
        format!("No .gba roms in {}", rom_dir.display())
    } else {
        format!("{} roms in {}", games.len(), rom_dir.display())
    };
    let header = CString::new(header).unwrap_or_default();
    ImGui::PushTextWrapPos(0.0);
    ImGui::TextDisabled(header.as_ptr() as _);
    ImGui::PopTextWrapPos();
    ImGui::Separator();

    let child_sz = ImVec2 { x: 0.0, y: 0.0 };
    if ImGui::BeginChild(c"##gamelist".as_ptr() as _, &child_sz, false, 0) {
        // Clamp the selectable to the available width so long file names clip instead of
        // widening the panel (which would make it horizontally scrollable).
        let sel_width = ImGui::GetContentRegionAvail().x;
        for (i, game) in games.iter().enumerate() {
            let sel_sz = ImVec2 { x: sel_width, y: 0.0 };
            if ImGui::Selectable(game.label.as_ptr() as _, false, 0, &sel_sz) {
                *detail_game = Some(i);
                *active_tab = 0;
            }
            if ImGui::IsItemHovered(0) {
                *hovered = Some(i);
            }
        }
    }
    ImGui::EndChild();
    ImGui::End();
}

/// Fullscreen per-game page: title, Launch, and the game's settings tabs. Back closes
/// it and drops nav back into the list.
unsafe fn render_game_detail_overlay(
    games: &[GameEntry],
    settings_config: &mut SettingsConfig,
    detail_game: &mut Option<usize>,
    active_tab: &mut usize,
    overlay_focused: &mut bool,
    launch: &mut Option<PathBuf>,
    open_savestates: &mut bool,
) {
    let Some(i) = *detail_game else {
        *overlay_focused = true;
        return;
    };
    if !begin_fullscreen_overlay(c"##gamedetail") {
        ImGui::End();
        return;
    }

    ImGui::SetWindowFontScale(1.3);
    ImGui::PushTextWrapPos(0.0);
    ImGui::TextUnformatted(games[i].label.as_ptr() as _, ptr::null());
    ImGui::PopTextWrapPos();
    ImGui::SetWindowFontScale(1.0);
    ImGui::Spacing();

    if full_width_button(c"Launch game") {
        *launch = Some(games[i].path.clone());
    }
    // Resume straight into a state without launching, pausing and navigating first.
    if full_width_button(c"Launch from savestate") {
        *open_savestates = true;
    }
    ImGui::Spacing();

    // Nav will not step straight from the tab bar up to the buttons above it — the tabs
    // sit on one SameLine row and imgui looks for a nav target overlapping that row's
    // span. A full-width invisible stop in between gives it one, the same trick the
    // settings child uses at its own ends.
    nav_scroll_stop(c"##detail_buttons_stop");

    render_settings_tabs(settings_config, active_tab, false, 0.0);

    // Back/Esc: only close at the top level; otherwise imgui has already stepped out of
    // the settings child or a combo this frame.
    if back_closes_overlay(*overlay_focused) {
        *detail_game = None;
    }
    *overlay_focused = ImGui::IsWindowFocused(0);
    ImGui::End();
}

/// Blocking game browser. Returns the chosen rom path, or None if the window closed.
/// `settings_config` is edited in place so the launch reflects the on-screen choices.
/// What the browser hands back: the rom to run, and optionally a savestate to resume it
/// from (chosen on the game's detail page).
pub struct MenuLaunch {
    pub rom: PathBuf,
    pub savestate: Option<PathBuf>,
}

impl From<PathBuf> for MenuLaunch {
    fn from(rom: PathBuf) -> Self {
        MenuLaunch { rom, savestate: None }
    }
}

pub fn show_main_menu(
    rom_dir: &Path,
    settings_config: &mut SettingsConfig,
    global_settings: &mut GlobalSettings,
    ra_context: &mut RaContext,
    ui_backend: &mut impl UiBackend,
) -> Option<MenuLaunch> {
    unsafe {
        let games = scan_games(rom_dir);
        let mut hovered: Option<usize> = None;
        let mut detail_game: Option<usize> = None;
        let mut detail_overlay_focused = true;
        let mut active_tab: usize = 0;
        let mut launch: Option<PathBuf> = None;
        let mut launch_savestate: Option<PathBuf> = None;
        let mut open_savestates = false;
        let mut show_savestates = false;
        let mut savestate_entries: Vec<SavestateUiEntry> = Vec::new();
        let mut savestate_selected: Option<usize> = None;
        let mut savestate_confirm_delete = false;
        let mut savestate_renaming: Option<String> = None;
        let mut savestate_overlay_focused = true;
        let mut show_global = false;
        let mut global_overlay_focused = false;
        let mut show_ra = false;
        let mut ra_overlay_focused = false;
        let mut ra_login_context = RALoginContext::default();
        let mut show_controls = false;
        let mut controls_overlay_focused = false;
        let mut creating_controls = false;
        let mut create_overlay_focused = false;
        let mut controls_edit_context = ControlsEditContext::default();
        let mut new_binding = KeyBinding::default();
        let mut selected_control: Option<usize> = None;
        let mut show_layouts = false;
        let mut layouts_overlay_focused = false;
        let mut creating_layout = false;
        let mut layout_create_focused = false;
        let mut layout_edit_context = ControlsEditContext::default();
        let mut new_layout = CustomLayout::default();
        let mut selected_layout: Option<usize> = None;

        while launch.is_none() {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);

            if !ui_backend.new_frame() {
                free_savestate_entries(&mut savestate_entries);
                return None;
            }

            render_game_list(&games, rom_dir, &mut hovered, &mut detail_game, &mut active_tab, &mut show_global);
            render_game_detail_overlay(&games, settings_config, &mut detail_game, &mut active_tab, &mut detail_overlay_focused, &mut launch, &mut open_savestates);

            // Edge-triggered: the thumbnails are GL textures, so the list is scanned when
            // the page opens, not every frame.
            if open_savestates {
                open_savestates = false;
                if let Some(i) = detail_game {
                    free_savestate_entries(&mut savestate_entries);
                    savestate_entries = load_savestate_entries(&games[i].path);
                    savestate_selected = None;
                    savestate_overlay_focused = true;
                    show_savestates = true;
                }
            }

            if show_savestates {
                // detail_game is what names the rom; if it closed underneath, so does this
                match detail_game {
                    Some(i) => {
                        if begin_fullscreen_overlay(c"##browsersavestates") {
                            match render_savestate_overlay(&savestate_entries, &mut savestate_selected, &mut savestate_confirm_delete, &mut savestate_renaming, SavestateUiMode::Browser) {
                                Some(SavestateUiAction::Load(path)) => {
                                    launch_savestate = Some(path);
                                    launch = Some(games[i].path.clone());
                                }
                                Some(SavestateUiAction::Delete(path)) => {
                                    if let Err(err) = fs::remove_file(&path) {
                                        eprintln!("Failed to delete savestate {path:?}: {err}");
                                    }
                                    free_savestate_entries(&mut savestate_entries);
                                    savestate_entries = load_savestate_entries(&games[i].path);
                                }
                                Some(SavestateUiAction::Rename(path, name)) => {
                                    if let Err(err) = crate::savestate::relabel_file(&path, &name) {
                                        eprintln!("Failed to rename savestate {path:?}: {err}");
                                    }
                                    free_savestate_entries(&mut savestate_entries);
                                    savestate_entries = load_savestate_entries(&games[i].path);
                                }
                                // Create/Overwrite are not offered in Browser mode
                                _ => {}
                            }
                            if savestate_selected.is_none() && back_closes_overlay(savestate_overlay_focused) {
                                show_savestates = false;
                                free_savestate_entries(&mut savestate_entries);
                            }
                            savestate_overlay_focused = ImGui::IsWindowFocused(0);
                        }
                        ImGui::End();
                    }
                    None => {
                        show_savestates = false;
                        free_savestate_entries(&mut savestate_entries);
                    }
                }
            }

            // Only one page is up at a time: a sub-page covers the menu it was opened
            // from, and closing it (Back) returns there rather than all the way out.
            if show_global && !show_ra && !show_controls && !show_layouts {
                render_global_settings_overlay(&mut show_global, &mut show_layouts, &mut show_controls, &mut show_ra, &mut global_overlay_focused);
            }

            if show_ra {
                render_ra_settings_overlay(&mut show_ra, global_settings, ra_context, &mut ra_login_context, &mut ra_overlay_focused);
            }

            if show_controls {
                if creating_controls {
                    render_custom_controls_overlay(&mut creating_controls, global_settings, &mut controls_edit_context, &mut new_binding, &mut create_overlay_focused);
                } else {
                    render_controls_settings_overlay(
                        &mut show_controls,
                        global_settings,
                        &mut creating_controls,
                        &mut controls_edit_context,
                        &mut new_binding,
                        &mut selected_control,
                        &mut controls_overlay_focused,
                    );
                }
                // Keep the Controls setting's list in step with the saved profiles, so a
                // profile added here is selectable without leaving the menu.
                settings_config.settings.populate_controls(&global_settings.default_control, &global_settings.custom_controls);
            }

            if show_layouts {
                if creating_layout {
                    render_custom_layout_overlay(&mut creating_layout, global_settings, &mut layout_edit_context, &mut new_layout, &mut layout_create_focused);
                } else {
                    render_layouts_settings_overlay(
                        &mut show_layouts,
                        global_settings,
                        &mut creating_layout,
                        &mut layout_edit_context,
                        &mut new_layout,
                        &mut selected_layout,
                        &mut layouts_overlay_focused,
                    );
                }
                settings_config.settings.populate_screen_layouts(&global_settings.custom_layouts);
            }

            present_frame(ui_backend);
        }

        free_savestate_entries(&mut savestate_entries);
        launch.map(|rom| MenuLaunch { rom, savestate: launch_savestate })
    }
}
