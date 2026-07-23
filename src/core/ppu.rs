// PpuRegs: the live display registers (BGxCNT/ofs/affine refs/window/blend), captured
// per scanline so the render thread can replay mid-frame register changes. The software
// scanline PPU below rasterizes the frame from them; it is the only renderer.

use crate::core::gpu::{DispCnt, DISPLAY_HEIGHT, DISPLAY_WIDTH};
use crate::core::graphics::gpu_mem_buf::GpuMemBuf;
use crate::core::memory::mem::vram_offset;
use crate::savestate::Savestate;
use std::arch::arm::{
    uint16x8_t, uint32x4_t, vandq_u16, vandq_u32, vbslq_u16, vceqq_u16, vcltq_u16, vdupq_n_u16, vdupq_n_u32, vget_high_u16, vget_low_u16, vld1_u8, vld1q_u16, vmovl_u16, vmovl_u8, vorrq_u32,
    vshlq_n_u16, vshlq_n_u32, vst1q_u16, vst1q_u32, vtstq_u16,
};
use std::ops::Range;

/// ADVANCEDSLOP_CHECK_SIMD=1 runs the scalar layer selection alongside the vector one and
/// asserts they agree, for validating the reformulation against real frames.
fn check_simd() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("ADVANCEDSLOP_CHECK_SIMD").is_ok_and(|v| v == "1"))
}

// Per-line object pixels, split into parallel arrays rather than an array of structs so
// the compositor can load a run of them as one vector
#[derive(Clone)]
pub struct ObjLine {
    color: [u16; DISPLAY_WIDTH], // 15bpp, bit15 = valid
    prio: [u8; DISPLAY_WIDTH],
    semi: [u8; DISPLAY_WIDTH],
}

impl ObjLine {
    fn new() -> Self {
        ObjLine {
            color: [0; DISPLAY_WIDTH],
            prio: [0; DISPLAY_WIDTH],
            semi: [0; DISPLAY_WIDTH],
        }
    }
}

/// One scanline's objects, window mask and the handful of registers compose needs.
/// Per-worker scratch, reused line to line: each rasterizing core draws and composes its
/// own share of the scanlines, so this never crosses a thread.
#[derive(Clone)]
pub struct ObjLayer {
    obj: ObjLine,
    win_mask: [u8; DISPLAY_WIDTH],
    backdrop: u16,
    bld_cnt: u16,
    bld_alpha: u16,
    bld_y: u16,
    forced_blank: bool,
    // Whether any semi-transparent obj pixel landed on the line. False lets compose take
    // the no-blend fast path without scanning the semi array; a stale true only costs
    // speed, never correctness.
    any_semi: bool,
}

impl Default for ObjLayer {
    fn default() -> Self {
        ObjLayer {
            obj: ObjLine::new(),
            win_mask: [0x3F; DISPLAY_WIDTH],
            backdrop: 0,
            bld_cnt: 0,
            bld_alpha: 0,
            bld_y: 0,
            forced_blank: false,
            any_semi: false,
        }
    }
}

/// Two of a scanline's background layers (bg0/bg1 or bg2/bg3). The pairing is a leftover
/// of the old cross-core layer split, kept because compose's candidate walk is written
/// against two halves; both are per-worker scratch on the same core now.
#[derive(Clone)]
pub struct BgHalf {
    bg: [[u16; DISPLAY_WIDTH]; 2],
    active: [bool; 2],
    prio: [u16; 2],
}

impl Default for BgHalf {
    fn default() -> Self {
        BgHalf {
            bg: [[0; DISPLAY_WIDTH]; 2],
            active: [false; 2],
            prio: [0; 2],
        }
    }
}

/// A background the compositor has to consider, resolved to the line it lives on. The
/// layers arrive in two halves, so the compositor cannot index them by bg number; it
/// builds this list per line instead, ordered by (priority, index) exactly as the
/// hardware walks them.
struct BgCandidate {
    line: *const u16,
    bg: usize,
    prio: u16,
}

// Layer selection for one scanline: the winning and runner-up layer per pixel, as a
// colour plus the sort key that picked it (see select_layers_neon). KEY_NONE = nothing
// there, i.e. the backdrop.
const KEY_NONE: u16 = 0xFFFF;

struct Selection {
    top_color: [u16; DISPLAY_WIDTH],
    top_key: [u16; DISPLAY_WIDTH],
    bottom_color: [u16; DISPLAY_WIDTH],
    bottom_key: [u16; DISPLAY_WIDTH],
}

impl Selection {
    fn new() -> Self {
        Selection {
            top_color: [0; DISPLAY_WIDTH],
            top_key: [KEY_NONE; DISPLAY_WIDTH],
            bottom_color: [0; DISPLAY_WIDTH],
            bottom_key: [KEY_NONE; DISPLAY_WIDTH],
        }
    }
}

// Sort key for a candidate layer: priority in the high bits, tiebreak in the low. The
// tiebreak reproduces the hardware order within one priority — objects first, then
// backgrounds by index — so "smallest key wins" is the same answer the ordered walk gives.
#[inline]
fn obj_key(prio: u8) -> u16 {
    (prio as u16) << 4
}

#[inline]
fn bg_key(prio: u16, bg: usize) -> u16 {
    (prio << 4) | (bg as u16 + 1)
}

/// Layer id for BLDCNT target tests, from a sort key.
#[inline]
fn key_layer(key: u16) -> u8 {
    if key == KEY_NONE {
        LAYER_BACKDROP
    } else if key & 0xF == 0 {
        LAYER_OBJ
    } else {
        (key & 0xF) as u8 - 1
    }
}

// Layer ids for blend target selection (BLDCNT bit order)
#[allow(dead_code)]
const LAYER_OBJ: u8 = 4;
#[allow(dead_code)]
const LAYER_BACKDROP: u8 = 5;

// Copy: the render regs keep a per-scanline snapshot of these for the software renderer
#[derive(Clone, Copy, Savestate)]
pub struct PpuRegs {
    pub bg_cnt: [u16; 4],
    pub bg_h_ofs: [u16; 4],
    pub bg_v_ofs: [u16; 4],
    // BG2/BG3 affine parameters and 28-bit reference points
    pub bg_pa: [i16; 2],
    pub bg_pb: [i16; 2],
    pub bg_pc: [i16; 2],
    pub bg_pd: [i16; 2],
    pub bg_x: [i32; 2],
    pub bg_y: [i32; 2],
    // Internal reference registers: reloaded at frame start and on BGxX/Y writes,
    // stepped by pb/pd per scanline
    pub internal_x: [i32; 2],
    pub internal_y: [i32; 2],
    pub win_h: [u16; 2],
    pub win_v: [u16; 2],
    pub win_in: u16,
    pub win_out: u16,
    pub mosaic: u16,
    pub bld_cnt: u16,
    pub bld_alpha: u16,
    pub bld_y: u16,
}

impl PpuRegs {
    pub fn new() -> Self {
        PpuRegs {
            bg_cnt: [0; 4],
            bg_h_ofs: [0; 4],
            bg_v_ofs: [0; 4],
            bg_pa: [0x100; 2],
            bg_pb: [0; 2],
            bg_pc: [0; 2],
            bg_pd: [0x100; 2],
            bg_x: [0; 2],
            bg_y: [0; 2],
            internal_x: [0; 2],
            internal_y: [0; 2],
            win_h: [0; 2],
            win_v: [0; 2],
            win_in: 0,
            win_out: 0,
            mosaic: 0,
            bld_cnt: 0,
            bld_alpha: 0,
            bld_y: 0,
        }
    }

    pub fn set_bg_x(&mut self, i: usize, mask: u32, value: u32) {
        let masked = ((self.bg_x[i] as u32) & !mask) | (value & mask);
        // 28-bit sign extend; a mid-frame write reloads the internal reference
        self.bg_x[i] = ((masked << 4) as i32) >> 4;
        self.internal_x[i] = self.bg_x[i];
    }

    pub fn set_bg_y(&mut self, i: usize, mask: u32, value: u32) {
        let masked = ((self.bg_y[i] as u32) & !mask) | (value & mask);
        self.bg_y[i] = ((masked << 4) as i32) >> 4;
        self.internal_y[i] = self.bg_y[i];
    }

    pub fn reload_internal_refs(&mut self) {
        self.internal_x = self.bg_x;
        self.internal_y = self.bg_y;
    }

    pub fn step_internal_refs(&mut self) {
        for i in 0..2 {
            self.internal_x[i] = self.internal_x[i].wrapping_add(self.bg_pb[i] as i32);
            self.internal_y[i] = self.internal_y[i].wrapping_add(self.bg_pd[i] as i32);
        }
    }
}

// Software scanline PPU (NooDS gpu_2d.cpp GBA paths as the behavioral reference).
//
// Split across the two rasterizing cores by scanline: each core draws objects, all four
// bg layers *and* the composed pixels for its half of the frame's lines — balanced by
// construction, whatever the bg mode. Neither reads live guest memory — the emulation
// thread snapshots vram, palettes and oam at vblank and carries straight on — so nothing
// here can stall the guest. Everything runs a frame behind, straight into the frame
// buffer the present thread will blit.
pub mod soft_ppu {
use super::*;

#[inline]
fn read_vram16(vram: &[u8], offset: u32) -> u16 {
    let offset = vram_offset(offset) as usize;
    u16::from_le_bytes([vram[offset], vram[offset + 1]])
}

#[inline]
fn read_pal16(pal: &[u8], index: usize) -> u16 {
    u16::from_le_bytes([pal[index << 1], pal[(index << 1) + 1]])
}

struct LineContext<'a> {
    vram: &'a [u8],
    pal: &'a [u8],
    disp_cnt: DispCnt,
    line: u32,
}

/// Objects, the window mask and the compose registers for one scanline, walking the
/// frame's pre-decoded OAM (see `OamScratch`). Split across the two rasterizing cores by
/// scanline range, not by sprite: a sprite's pixels are resolved against whatever is
/// already in the line buffer, ordered by oam index, so splitting the sprite walk would
/// change which sprite wins a tie. Lines are independent.
fn draw_objs(mem: &GpuMemBuf, regs: &PpuRegs, disp_cnt: DispCnt, line: u32, out: &mut ObjLayer, oam: &OamScratch) {
    out.forced_blank = disp_cnt.forced_blank();
    out.backdrop = read_pal16(&mem.pal[..], 0) & 0x7FFF;
    out.bld_cnt = regs.bld_cnt;
    out.bld_alpha = regs.bld_alpha;
    out.bld_y = regs.bld_y;
    out.any_semi = false;
    if out.forced_blank {
        return;
    }

    let ctx = LineContext {
        vram: &mem.vram[..],
        pal: &mem.pal[..],
        disp_cnt,
        line,
    };

    // Objects (which also produce the obj-window mask). The buffers persist across
    // frames and every draw writes only the pixels it covers, so each has to be cleared
    // first — transparency here is the absence of a write.
    let mut obj_window = [false; DISPLAY_WIDTH];
    out.obj.color.fill(0);
    if disp_cnt.screen_display_obj() {
        let mut any_semi = false;
        draw_objects(&ctx, oam, &mut out.obj, &mut obj_window, &mut any_semi);
        out.any_semi = any_semi;
    }

    // Per-pixel window enable mask: bits 0-3 bg, 4 obj, 5 effects. It depends on the
    // obj window, which is why it belongs here rather than with the bgs.
    let win_mask = &mut out.win_mask;
    let windows_active = disp_cnt.window0_display() || disp_cnt.window1_display() || disp_cnt.obj_window_display();
    if !windows_active {
        win_mask.fill(0x3F);
        return;
    }

    let win_out = regs.win_out as u8 & 0x3F;
    let obj_win = (regs.win_out >> 8) as u8 & 0x3F;
    win_mask.fill(win_out);
    if disp_cnt.obj_window_display() {
        for x in 0..DISPLAY_WIDTH {
            if obj_window[x] {
                win_mask[x] = obj_win;
            }
        }
    }
    // Win1 below win0 in priority: apply 1 first, then 0 overrides
    for w in [1usize, 0usize] {
        let enabled = if w == 0 { disp_cnt.window0_display() } else { disp_cnt.window1_display() };
        if !enabled {
            continue;
        }
        let y1 = (regs.win_v[w] >> 8) as u32;
        let y2 = (regs.win_v[w] & 0xFF) as u32;
        let y_in = if y1 <= y2 { line >= y1 && line < y2 } else { line >= y1 || line < y2 };
        if !y_in {
            continue;
        }
        let x1 = (regs.win_h[w] >> 8) as usize;
        let x2 = (regs.win_h[w] & 0xFF) as usize;
        let win_in = (regs.win_in >> (w * 8)) as u8 & 0x3F;
        if x1 <= x2 {
            for x in x1..x2.min(DISPLAY_WIDTH) {
                win_mask[x] = win_in;
            }
        } else {
            for x in 0..x2.min(DISPLAY_WIDTH) {
                win_mask[x] = win_in;
            }
            for x in x1..DISPLAY_WIDTH {
                win_mask[x] = win_in;
            }
        }
    }
}

/// Two of a scanline's background layers: `base` picks bg0/bg1 or bg2/bg3.
///
/// Which layers exist at all is a function of the bg mode, so a pair can come back with
/// nothing: in the bitmap modes only bg2 is drawn, and the bg0/bg1 pair has no work.
fn draw_bgs(mem: &GpuMemBuf, regs: &PpuRegs, disp_cnt: DispCnt, line: u32, out: &mut BgHalf, base: usize) {
    out.active = [false; 2];
    if disp_cnt.forced_blank() {
        return;
    }

    let ctx = LineContext {
        vram: &mem.vram[..],
        pal: &mem.pal[..],
        disp_cnt,
        line,
    };

    let mode = disp_cnt.bg_mode().value();
    let bg_enabled = [
        disp_cnt.screen_display_bg0(),
        disp_cnt.screen_display_bg1(),
        disp_cnt.screen_display_bg2(),
        disp_cnt.screen_display_bg3(),
    ];

    for i in 0..2 {
        let bg = base + i;
        if !bg_enabled[bg] {
            continue;
        }
        let dst = &mut out.bg[i];
        // Same rule as the objects: clear before drawing, since every draw writes only
        // the pixels it covers and transparency is a missing write
        let drawn = match (mode, bg) {
            (0, _) | (1, 0 | 1) => {
                dst.fill(0);
                draw_text_bg(&ctx, regs, bg, dst);
                true
            }
            (1, 2) | (2, 2 | 3) => {
                dst.fill(0);
                draw_affine_bg(&ctx, regs, bg, dst);
                true
            }
            (3..=5, 2) => {
                dst.fill(0);
                draw_bitmap_bg(&ctx, regs, mode, dst);
                true
            }
            _ => false,
        };
        if drawn {
            out.active[i] = true;
            out.prio[i] = regs.bg_cnt[bg] & 3;
        }
    }
}

/// Composes one rasterized scanline into `fb_line` (0xAABBGGRR): layer selection, window
/// effects gating and BLDCNT blending. Runs on the same core that drew the line, right
/// after it, while the layer buffers are still in L1.
fn compose_scanline(objs: &ObjLayer, bg01: &BgHalf, bg23: &BgHalf, line: u32, fb_line: &mut [u32], sel: &mut Selection) {
    if objs.forced_blank {
        fb_line.fill(0xFFFFFFFF);
        return;
    }

    let backdrop = objs.backdrop;
    let bld_mode = (objs.bld_cnt >> 6) & 3;
    let eva = (objs.bld_alpha & 0x1F).min(16) as u32;
    let evb = ((objs.bld_alpha >> 8) & 0x1F).min(16) as u32;
    let evy = (objs.bld_y & 0x1F).min(16) as u32;

    // Which bgs can contribute, and in what order, is a per-line constant: active ones
    // sorted by (priority, index). Deciding that per pixel meant re-testing and
    // re-comparing priorities 16 times a pixel. Objects can't join the list — their
    // priority is per pixel — so the selection splices them in.
    let mut bg_order = [const { None }; 4];
    let mut bg_count = 0;
    for prio in 0..4u16 {
        for bg in 0..4usize {
            let (half, i) = if bg < 2 { (bg01, bg) } else { (bg23, bg - 2) };
            if half.active[i] && half.prio[i] == prio {
                bg_order[bg_count] = Some(BgCandidate {
                    line: half.bg[i].as_ptr(),
                    bg,
                    prio,
                });
                bg_count += 1;
            }
        }
    }
    let bg_order: [BgCandidate; 4] = bg_order.map(|c| c.unwrap_or(BgCandidate { line: std::ptr::null(), bg: 0, prio: 0 }));
    let bg_order = &bg_order[..bg_count];
    unsafe {
        select_layers_neon(bg_order, &objs.obj, &objs.win_mask, sel);
        // Cross-check the vector reformulation against the ordered walk. cfg! rather
        // than only the env valve so release drops the second selection pass entirely.
        if cfg!(debug_assertions) && check_simd() {
            let mut reference = Selection::new();
            select_layers_scalar(bg_order, &objs.obj, &objs.win_mask, &mut reference);
            for x in 0..DISPLAY_WIDTH {
                debug_assert_eq!(
                    (sel.top_key[x], sel.top_color[x], sel.bottom_key[x], sel.bottom_color[x]),
                    (reference.top_key[x], reference.top_color[x], reference.bottom_key[x], reference.bottom_color[x]),
                    "simd layer selection differs at line {line} x {x}"
                );
            }
        }
    }

    // Fast path: nothing on this line can blend — the BLDCNT mode is off, and either no
    // semi-transparent obj pixel landed or no second target is armed (a semi obj forces
    // alpha regardless of the mode, but still needs a second target). Every pixel is
    // then just the top layer, or the backdrop where there is none.
    if bld_mode == 0 && (!objs.any_semi || objs.bld_cnt & 0x3F00 == 0) {
        top_to_abgr(sel, backdrop, fb_line);
        return;
    }

    let mut line555 = [0u16; DISPLAY_WIDTH];
    for x in 0..DISPLAY_WIDTH {
        let mask = objs.win_mask[x];
        let top_key = sel.top_key[x];
        let bottom_key = sel.bottom_key[x];
        let top_color = if top_key == KEY_NONE { backdrop } else { sel.top_color[x] };
        let bottom_color = if bottom_key == KEY_NONE { backdrop } else { sel.bottom_color[x] };
        let top_layer = key_layer(top_key);
        let bottom_layer = key_layer(bottom_key);
        let top_semi = top_layer == LAYER_OBJ && objs.obj.semi[x] != 0;

        // Blending: semi-transparent objects force alpha onto a 2nd-target bottom;
        // otherwise BLDCNT mode with 1st/2nd target checks, gated by the window
        // effects bit
        let effects_ok = mask & 0x20 != 0;
        let first_target = objs.bld_cnt & (1 << top_layer) != 0;
        let second_target = objs.bld_cnt & (1 << (8 + bottom_layer)) != 0;

        let mut color = top_color;
        if top_semi && second_target && effects_ok {
            color = alpha_blend(top_color, bottom_color, eva, evb);
        } else if effects_ok && first_target {
            match bld_mode {
                1 if second_target => color = alpha_blend(top_color, bottom_color, eva, evb),
                2 => color = brightness(top_color, evy, true),
                3 => color = brightness(top_color, evy, false),
                _ => {}
            }
        }

        line555[x] = color;
    }
    convert_555_line(&line555, fb_line);
}

/// The selection's top layer (backdrop where empty) to 0xAABBGGRR. The no-blend fast
/// path is exactly this and nothing else.
fn top_to_abgr(sel: &Selection, backdrop: u16, fb_line: &mut [u32]) {
    unsafe {
        let none = vdupq_n_u16(KEY_NONE);
        let bd = vdupq_n_u16(backdrop);
        let mut x = 0;
        while x < DISPLAY_WIDTH {
            let key = vld1q_u16(sel.top_key.as_ptr().add(x));
            let color = vld1q_u16(sel.top_color.as_ptr().add(x));
            store_abgr8(fb_line.as_mut_ptr().add(x), vbslq_u16(vceqq_u16(key, none), bd, color));
            x += 8;
        }
    }
}

/// A 15bpp line to 0xAABBGGRR, eight pixels at a time.
fn convert_555_line(src: &[u16; DISPLAY_WIDTH], fb_line: &mut [u32]) {
    unsafe {
        let mut x = 0;
        while x < DISPLAY_WIDTH {
            store_abgr8(fb_line.as_mut_ptr().add(x), vld1q_u16(src.as_ptr().add(x)));
            x += 8;
        }
    }
}

/// Four 15bpp pixels widened to 32 bits, each 5-bit field shifted into its 8-bit slot:
/// r<<3 (from bit 0), g<<6 (from bit 5), b<<9 (from bit 10), alpha forced opaque.
#[target_feature(enable = "neon")]
unsafe fn abgr8_from_555(c: uint32x4_t) -> uint32x4_t {
    let r = vshlq_n_u32::<3>(vandq_u32(c, vdupq_n_u32(0x1F)));
    let g = vshlq_n_u32::<6>(vandq_u32(c, vdupq_n_u32(0x3E0)));
    let b = vshlq_n_u32::<9>(vandq_u32(c, vdupq_n_u32(0x7C00)));
    vorrq_u32(vorrq_u32(r, g), vorrq_u32(b, vdupq_n_u32(0xFF00_0000)))
}

/// Eight 15bpp pixels to eight 0xAABBGGRR words at `dst`.
#[target_feature(enable = "neon")]
unsafe fn store_abgr8(dst: *mut u32, c: uint16x8_t) {
    vst1q_u32(dst, abgr8_from_555(vmovl_u16(vget_low_u16(c))));
    vst1q_u32(dst.add(4), abgr8_from_555(vmovl_u16(vget_high_u16(c))));
}

fn select_layers_scalar(bg_order: &[BgCandidate], obj: &ObjLine, win_mask: &[u8; DISPLAY_WIDTH], sel: &mut Selection) {
    for x in 0..DISPLAY_WIDTH {
        let mask = win_mask[x];
        let mut top_key = KEY_NONE;
        let mut top_color = 0;
        let mut bottom_key = KEY_NONE;
        let mut bottom_color = 0;
        let mut filled = 0;

        let mut obj_pending = mask & 0x10 != 0 && obj.color[x] & 0x8000 != 0;
        let obj_prio = obj.prio[x] as u16;

        'candidates: {
            macro_rules! take {
                ($color:expr, $key:expr) => {
                    if filled == 0 {
                        top_color = $color;
                        top_key = $key;
                        filled = 1;
                    } else {
                        bottom_color = $color;
                        bottom_key = $key;
                        break 'candidates;
                    }
                };
            }

            for candidate in bg_order {
                let (bg, bg_prio) = (candidate.bg, candidate.prio);
                // Objects sit above bgs of the same priority
                if obj_pending && obj_prio <= bg_prio {
                    obj_pending = false;
                    take!(obj.color[x] & 0x7FFF, obj_key(obj.prio[x]));
                }
                let px = unsafe { *candidate.line.add(x) };
                if mask & (1 << bg) != 0 && px & 0x8000 != 0 {
                    take!(px & 0x7FFF, bg_key(bg_prio, bg));
                }
            }
            // Below every active bg, or there were none
            if obj_pending {
                take!(obj.color[x] & 0x7FFF, obj_key(obj.prio[x]));
            }
        }

        sel.top_color[x] = top_color;
        sel.top_key[x] = top_key;
        sel.bottom_color[x] = bottom_color;
        sel.bottom_key[x] = bottom_key;
    }
}

/// Layer selection, eight pixels at a time.
///
/// The ordered walk above cannot vectorize directly: objects carry a per-pixel priority,
/// so lanes disagree about where the object falls in the order. Reformulated as a running
/// minimum instead — every candidate gets a sort key, masked to KEY_NONE where it is
/// transparent or window-disabled, and the two smallest keys are the top and bottom
/// layers. Keys are unique per candidate, so the total order is the same one the walk
/// follows and the result is identical.
#[target_feature(enable = "neon")]
unsafe fn select_layers_neon(bg_order: &[BgCandidate], obj: &ObjLine, win_mask: &[u8; DISPLAY_WIDTH], sel: &mut Selection) {
    let none = vdupq_n_u16(KEY_NONE);
    let opaque_bit = vdupq_n_u16(0x8000);
    let color_mask = vdupq_n_u16(0x7FFF);

    let mut x = 0;
    while x < DISPLAY_WIDTH {
        let win = vmovl_u8(vld1_u8(win_mask.as_ptr().add(x)));

        let mut best = none;
        let mut best_color = vdupq_n_u16(0);
        let mut second = none;
        let mut second_color = vdupq_n_u16(0);

        // Objects: key is per pixel, from this pixel's sprite priority
        {
            let color = vld1q_u16(obj.color.as_ptr().add(x));
            let prio = vmovl_u8(vld1_u8(obj.prio.as_ptr().add(x)));
            let valid = vandq_u16(vtstq_u16(color, opaque_bit), vtstq_u16(win, vdupq_n_u16(0x10)));
            let key = vbslq_u16(valid, vshlq_n_u16::<4>(prio), none);
            let color = vandq_u16(color, color_mask);

            let lt_best = vcltq_u16(key, best);
            let lt_second = vcltq_u16(key, second);
            second = vbslq_u16(lt_best, best, vbslq_u16(lt_second, key, second));
            second_color = vbslq_u16(lt_best, best_color, vbslq_u16(lt_second, color, second_color));
            best = vbslq_u16(lt_best, key, best);
            best_color = vbslq_u16(lt_best, color, best_color);
        }

        // Backgrounds: key is a per-line constant, only validity varies per pixel
        for candidate in bg_order {
            let (bg, bg_prio) = (candidate.bg, candidate.prio);
            let color = vld1q_u16(candidate.line.add(x));
            let valid = vandq_u16(vtstq_u16(color, opaque_bit), vtstq_u16(win, vdupq_n_u16(1 << bg)));
            let key = vbslq_u16(valid, vdupq_n_u16(bg_key(bg_prio, bg)), none);
            let color = vandq_u16(color, color_mask);

            let lt_best = vcltq_u16(key, best);
            let lt_second = vcltq_u16(key, second);
            second = vbslq_u16(lt_best, best, vbslq_u16(lt_second, key, second));
            second_color = vbslq_u16(lt_best, best_color, vbslq_u16(lt_second, color, second_color));
            best = vbslq_u16(lt_best, key, best);
            best_color = vbslq_u16(lt_best, color, best_color);
        }

        vst1q_u16(sel.top_color.as_mut_ptr().add(x), best_color);
        vst1q_u16(sel.top_key.as_mut_ptr().add(x), best);
        vst1q_u16(sel.bottom_color.as_mut_ptr().add(x), second_color);
        vst1q_u16(sel.bottom_key.as_mut_ptr().add(x), second);
        x += 8;
    }
}

fn alpha_blend(top: u16, bottom: u16, eva: u32, evb: u32) -> u16 {
    let (r1, g1, b1) = ((top & 0x1F) as u32, ((top >> 5) & 0x1F) as u32, ((top >> 10) & 0x1F) as u32);
    let (r2, g2, b2) = ((bottom & 0x1F) as u32, ((bottom >> 5) & 0x1F) as u32, ((bottom >> 10) & 0x1F) as u32);
    let r = ((r1 * eva + r2 * evb) >> 4).min(31);
    let g = ((g1 * eva + g2 * evb) >> 4).min(31);
    let b = ((b1 * eva + b2 * evb) >> 4).min(31);
    (r | (g << 5) | (b << 10)) as u16
}

fn brightness(color: u16, evy: u32, up: bool) -> u16 {
    let (r, g, b) = ((color & 0x1F) as u32, ((color >> 5) & 0x1F) as u32, ((color >> 10) & 0x1F) as u32);
    let f = |c: u32| -> u32 {
        if up {
            c + (((31 - c) * evy) >> 4)
        } else {
            c - ((c * evy) >> 4)
        }
    };
    (f(r) | (f(g) << 5) | (f(b) << 10)) as u16
}

fn draw_text_bg(ctx: &LineContext, regs: &PpuRegs, bg: usize, out: &mut [u16; DISPLAY_WIDTH]) {
    let cnt = regs.bg_cnt[bg];
    let char_base = (((cnt >> 2) & 3) as u32) << 14;
    let screen_base = (((cnt >> 8) & 0x1F) as u32) << 11;
    let is_8bpp = cnt & (1 << 7) != 0;
    let size = (cnt >> 14) & 3; // 0: 256x256, 1: 512x256, 2: 256x512, 3: 512x512

    let y = (ctx.line + regs.bg_v_ofs[bg] as u32) & if size & 2 != 0 { 511 } else { 255 };
    let x_ofs = regs.bg_h_ofs[bg] as u32;
    let x_mask = if size & 1 != 0 { 511 } else { 255 };

    // Row-constant part of the screenblock address. Screenblock layout: +0x800 per extra
    // 32x32 map, width first.
    let mut row_block = screen_base;
    if y >= 256 {
        row_block += if size & 1 != 0 { 0x1000 } else { 0x800 };
    }
    let row_tile = ((y & 255) / 8) * 32;

    // One tile run at a time: the screenblock address, the map entry fetch, the flip
    // flags and the character row address are all per-tile constants, so doing them per
    // pixel repeated each of them eight times over.
    let mut px = 0usize;
    while px < DISPLAY_WIDTH {
        let x = (px as u32 + x_ofs) & x_mask;
        let block = row_block + if x >= 256 { 0x800 } else { 0 };
        let entry = read_vram16(ctx.vram, block + (row_tile + ((x & 255) / 8)) * 2);
        let tile = (entry & 0x3FF) as u32;
        let h_flip = entry & (1 << 10) != 0;
        let v_flip = entry & (1 << 11) != 0;
        let ty = if v_flip { 7 - (y & 7) } else { y & 7 };

        // Tiles are 8 wide and both the 256px screenblock split and the 512px wrap are
        // 8-aligned, so a run never straddles a tile or a block boundary
        let first_tx = x & 7;
        let run = (8 - first_tx as usize).min(DISPLAY_WIDTH - px);

        // One load per tile row instead of one per pixel: a row is 4 (4bpp) or 8 (8bpp)
        // contiguous bytes — the vram mirror folds at 8-aligned boundaries, so a row
        // never straddles one — and an all-zero row (very common: transparent margins of
        // text and hud layers) skips the whole run against the cleared buffer.
        if is_8bpp {
            let row = vram_offset(char_base + tile * 64 + ty * 8) as usize;
            let row64 = u64::from_le_bytes(ctx.vram[row..row + 8].try_into().unwrap());
            if row64 != 0 {
                for i in 0..run {
                    let tx = first_tx + i as u32;
                    let tx = if h_flip { 7 - tx } else { tx };
                    let index = (row64 >> (tx * 8)) as u8;
                    if index != 0 {
                        out[px + i] = read_pal16(ctx.pal, index as usize) | 0x8000;
                    }
                }
            }
        } else {
            let row = vram_offset(char_base + tile * 32 + ty * 4) as usize;
            let row32 = u32::from_le_bytes(ctx.vram[row..row + 4].try_into().unwrap());
            if row32 != 0 {
                let pal_bank = ((entry >> 12) & 0xF) as usize;
                for i in 0..run {
                    let tx = first_tx + i as u32;
                    let tx = if h_flip { 7 - tx } else { tx };
                    let index = (row32 >> (tx * 4)) & 0xF;
                    if index != 0 {
                        out[px + i] = read_pal16(ctx.pal, pal_bank * 16 + index as usize) | 0x8000;
                    }
                }
            }
        }
        px += run;
    }
}

fn draw_affine_bg(ctx: &LineContext, regs: &PpuRegs, bg: usize, out: &mut [u16; DISPLAY_WIDTH]) {
    let cnt = regs.bg_cnt[bg];
    let char_base = (((cnt >> 2) & 3) as u32) << 14;
    let screen_base = (((cnt >> 8) & 0x1F) as u32) << 11;
    let wrap = cnt & (1 << 13) != 0;
    let size = 128u32 << ((cnt >> 14) & 3);

    let i = bg - 2;
    let mut cx = regs.internal_x[i];
    let mut cy = regs.internal_y[i];
    let pa = regs.bg_pa[i] as i32;
    let pc = regs.bg_pc[i] as i32;

    for px in 0..DISPLAY_WIDTH {
        let mut x = cx >> 8;
        let mut y = cy >> 8;
        cx = cx.wrapping_add(pa);
        cy = cy.wrapping_add(pc);

        if wrap {
            // size is a power of two, so the euclidean wrap is a mask — spelled out
            // because the Vita's Cortex-A9 has no integer divide and rem_euclid on a
            // runtime divisor is a libcall per pixel
            x &= size as i32 - 1;
            y &= size as i32 - 1;
        } else if x < 0 || x >= size as i32 || y < 0 || y >= size as i32 {
            continue;
        }
        let (x, y) = (x as u32, y as u32);

        let tile = ctx.vram[vram_offset(screen_base + (y / 8) * (size / 8) + (x / 8)) as usize] as u32;
        let index = ctx.vram[vram_offset(char_base + tile * 64 + (y & 7) * 8 + (x & 7)) as usize];
        if index == 0 {
            continue;
        }
        out[px] = read_pal16(ctx.pal, index as usize) | 0x8000;
    }
}

fn draw_bitmap_bg(ctx: &LineContext, regs: &PpuRegs, mode: u8, out: &mut [u16; DISPLAY_WIDTH]) {
    // Bitmap BGs sample through the BG2 affine matrix like NooDS drawExtendedGba
    let (width, height) = if mode == 5 { (160i32, 128i32) } else { (240, 160) };
    let page = if mode != 3 && ctx.disp_cnt.display_frame_select() { 0xA000u32 } else { 0 };

    let mut cx = regs.internal_x[0];
    let mut cy = regs.internal_y[0];
    let pa = regs.bg_pa[0] as i32;
    let pc = regs.bg_pc[0] as i32;

    // Identity fast path: pa == 1.0 and pc == 0 mean x advances exactly one texel per
    // pixel and y is constant, whatever the fractional start — the common case, since
    // bitmap-mode games rarely rotate. Bitmap pages all end below the vram mirror, so
    // rows are read straight out of the snapshot.
    if pa == 0x100 && pc == 0 {
        let y = cy >> 8;
        if y < 0 || y >= height {
            return;
        }
        let x0 = cx >> 8;
        let px_start = (-x0).max(0).min(DISPLAY_WIDTH as i32) as usize;
        let px_end = (width - x0).clamp(0, DISPLAY_WIDTH as i32) as usize;
        if px_start >= px_end {
            return;
        }
        if mode == 4 {
            let base = (page as usize + (y * width) as usize) + (x0 + px_start as i32) as usize;
            let src = &ctx.vram[base..base + (px_end - px_start)];
            for (dst, &index) in out[px_start..px_end].iter_mut().zip(src) {
                if index != 0 {
                    *dst = read_pal16(ctx.pal, index as usize) | 0x8000;
                }
            }
        } else {
            let base = (page as usize + (y * width) as usize * 2) + (x0 + px_start as i32) as usize * 2;
            let src = &ctx.vram[base..base + (px_end - px_start) * 2];
            for (dst, s) in out[px_start..px_end].iter_mut().zip(src.chunks_exact(2)) {
                *dst = u16::from_le_bytes([s[0], s[1]]) | 0x8000;
            }
        }
        return;
    }

    for px in 0..DISPLAY_WIDTH {
        let x = cx >> 8;
        let y = cy >> 8;
        cx = cx.wrapping_add(pa);
        cy = cy.wrapping_add(pc);

        if x < 0 || x >= width || y < 0 || y >= height {
            continue;
        }
        let pixel_index = (y * width + x) as u32;

        if mode == 4 {
            let index = ctx.vram[vram_offset(page + pixel_index) as usize];
            if index == 0 {
                continue;
            }
            out[px] = read_pal16(ctx.pal, index as usize) | 0x8000;
        } else {
            let color = read_vram16(ctx.vram, page + pixel_index * 2);
            out[px] = color | 0x8000;
        }
    }
}

// (shape, size) -> (width, height) in pixels
const OBJ_SIZES: [[(u32, u32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
];

// DecodedObj flags
const F_AFFINE: u8 = 1;
const F_SEMI: u8 = 1 << 1;
const F_WINDOW: u8 = 1 << 2;
const F_8BPP: u8 = 1 << 3;
const F_HFLIP: u8 = 1 << 4;
const F_VFLIP: u8 = 1 << 5;

/// One OAM entry with everything the per-line draw needs already unpacked. Only the
/// mapping-dependent tile stride and the bitmap-mode charblock rule stay per line —
/// DISPCNT is captured per scanline and those two are its to change.
#[derive(Clone, Copy)]
struct DecodedObj {
    ox: i16,
    oy: i16,
    w: u16,
    h: u16,
    bound_w: u16,
    bound_h: u16,
    base_tile: u16,
    // Tile row stride under 1D mapping: the sprite's tiles packed linearly, 8bpp tiles
    // counting double. 2D mapping is a 32-tile-wide char grid instead.
    stride_1d: u16,
    // Affine parameters (identity for regular sprites)
    pa: i16,
    pb: i16,
    pc: i16,
    pd: i16,
    prio: u8,
    pal_bank: u8,
    flags: u8,
}

const HALF_LINES: usize = DISPLAY_HEIGHT / 2;

/// A frame-half's OAM, decoded once per frame: the snapshot freezes OAM, so instead of
/// re-decoding all 128 entries on every line, each line walks a pre-built list (in oam
/// order) of just the sprites that cover it.
pub struct OamScratch {
    objs: [DecodedObj; 128],
    counts: [u8; HALF_LINES],
    lists: [[u8; 128]; HALF_LINES],
    base_line: u16,
}

impl OamScratch {
    fn new() -> Self {
        // All-integer plain data; zeroing is valid and skips a 14K stack template
        unsafe { std::mem::zeroed() }
    }

    fn decode(&mut self, oam: &[u8], lines: Range<u32>) {
        let line_count = (lines.end - lines.start) as usize;
        debug_assert!(line_count <= HALF_LINES);
        self.base_line = lines.start as u16;
        self.counts[..line_count].fill(0);
        let mut decoded = 0u8;

        for obj in 0..128 {
            let attr0 = u16::from_le_bytes([oam[obj * 8], oam[obj * 8 + 1]]);
            let attr1 = u16::from_le_bytes([oam[obj * 8 + 2], oam[obj * 8 + 3]]);
            let attr2 = u16::from_le_bytes([oam[obj * 8 + 4], oam[obj * 8 + 5]]);

            let affine = attr0 & (1 << 8) != 0;
            if !affine && attr0 & (1 << 9) != 0 {
                continue; // disabled
            }
            let gfx_mode = (attr0 >> 10) & 3;
            if gfx_mode == 3 {
                continue; // prohibited (no bitmap objs on gba)
            }
            let shape = ((attr0 >> 14) & 3) as usize;
            if shape == 3 {
                continue;
            }
            let size_idx = ((attr1 >> 14) & 3) as usize;
            let (w, h) = OBJ_SIZES[shape][size_idx];
            let double = affine && attr0 & (1 << 9) != 0;
            let (bound_w, bound_h) = if double { (w * 2, h * 2) } else { (w, h) };

            let mut oy = (attr0 & 0xFF) as i32;
            if oy + bound_h as i32 > 256 {
                oy -= 256;
            }
            let y0 = oy.max(lines.start as i32);
            let y1 = (oy + bound_h as i32).min(lines.end as i32);
            if y0 >= y1 {
                continue; // no line of this half
            }
            let mut ox = (attr1 & 0x1FF) as i32;
            if ox >= DISPLAY_WIDTH as i32 {
                ox -= 512;
            }
            if ox + bound_w as i32 <= 0 {
                continue; // fully off-screen
            }

            let is_8bpp = attr0 & (1 << 13) != 0;
            let (pa, pb, pc, pd) = if affine {
                let group = (((attr1 >> 9) & 0x1F) as usize) * 32;
                (
                    i16::from_le_bytes([oam[group + 6], oam[group + 7]]),
                    i16::from_le_bytes([oam[group + 14], oam[group + 15]]),
                    i16::from_le_bytes([oam[group + 22], oam[group + 23]]),
                    i16::from_le_bytes([oam[group + 30], oam[group + 31]]),
                )
            } else {
                (0x100, 0, 0, 0x100)
            };

            let mut flags = 0u8;
            flags |= if affine { F_AFFINE } else { 0 };
            flags |= if gfx_mode == 1 { F_SEMI } else { 0 };
            flags |= if gfx_mode == 2 { F_WINDOW } else { 0 };
            flags |= if is_8bpp { F_8BPP } else { 0 };
            flags |= if !affine && attr1 & (1 << 12) != 0 { F_HFLIP } else { 0 };
            flags |= if !affine && attr1 & (1 << 13) != 0 { F_VFLIP } else { 0 };

            let slot = decoded;
            decoded += 1;
            self.objs[slot as usize] = DecodedObj {
                ox: ox as i16,
                oy: oy as i16,
                w: w as u16,
                h: h as u16,
                bound_w: bound_w as u16,
                bound_h: bound_h as u16,
                base_tile: attr2 & 0x3FF,
                stride_1d: ((w / 8) * if is_8bpp { 2 } else { 1 }) as u16,
                pa,
                pb,
                pc,
                pd,
                prio: ((attr2 >> 10) & 3) as u8,
                pal_bank: ((attr2 >> 12) & 0xF) as u8,
                flags,
            };
            for line in y0..y1 {
                let li = (line - lines.start as i32) as usize;
                self.lists[li][self.counts[li] as usize] = slot;
                self.counts[li] += 1;
            }
        }
    }
}

fn draw_objects(ctx: &LineContext, oam: &OamScratch, out: &mut ObjLine, obj_window: &mut [bool; DISPLAY_WIDTH], any_semi: &mut bool) {
    let bitmap_mode = ctx.disp_cnt.bg_mode().value() >= 3;
    let mapping_1d = ctx.disp_cnt.obj_mapping_1d();
    let li = (ctx.line - oam.base_line as u32) as usize;
    let list = &oam.lists[li][..oam.counts[li] as usize];

    for &slot in list {
        let o = &oam.objs[slot as usize];
        // In bitmap modes the low charblock is bg vram; those tiles don't render
        if bitmap_mode && o.base_tile < 512 {
            continue;
        }
        let is_8bpp = o.flags & F_8BPP != 0;
        let window = o.flags & F_WINDOW != 0;
        let semi = (o.flags & F_SEMI != 0) as u8;
        let prio = o.prio;
        let pal_bank = o.pal_bank as usize;
        let tile_step = if is_8bpp { 2 } else { 1 };
        let row_stride = if mapping_1d { o.stride_1d as u32 } else { 32 };
        let ox = o.ox as i32;
        let local_y = ctx.line as i32 - o.oy as i32;
        let x_start = (-ox).max(0);

        // Resolve one texel against the line buffer. Priority ties go to the earlier
        // (lower oam index) sprite, which drew first.
        macro_rules! plot {
            ($sx:expr, $index:expr) => {
                let index = $index;
                if index != 0 {
                    let sx = $sx;
                    if window {
                        obj_window[sx] = true;
                    } else if out.color[sx] & 0x8000 == 0 || out.prio[sx] > prio {
                        let color = if is_8bpp {
                            read_pal16(ctx.pal, 256 + index as usize)
                        } else {
                            read_pal16(ctx.pal, 256 + pal_bank * 16 + index as usize)
                        };
                        out.color[sx] = color | 0x8000;
                        out.prio[sx] = prio;
                        out.semi[sx] = semi;
                        *any_semi |= semi != 0;
                    }
                }
            };
        }

        if o.flags & F_AFFINE != 0 {
            // Affine: per pixel, texture coordinates transformed around the bound-box
            // center. The transform can land anywhere in the tile, so there is no tile
            // row to batch.
            let (pa, pb, pc, pd) = (o.pa as i32, o.pb as i32, o.pc as i32, o.pd as i32);
            let (w, h) = (o.w as i32, o.h as i32);
            let (bound_w, bound_h) = (o.bound_w as i32, o.bound_h as i32);
            let x_end = bound_w.min(DISPLAY_WIDTH as i32 - ox);
            for local_x in x_start..x_end {
                let cx = local_x - bound_w / 2;
                let cy = local_y - bound_h / 2;
                let tx = ((pa * cx + pb * cy) >> 8) + w / 2;
                let ty = ((pc * cx + pd * cy) >> 8) + h / 2;
                if !(0..w).contains(&tx) || !(0..h).contains(&ty) {
                    continue;
                }
                let (tx, ty) = (tx as u32, ty as u32);
                let tile = o.base_tile as u32 + (ty / 8) * row_stride + (tx / 8) * tile_step;
                let index = if is_8bpp {
                    ctx.vram[vram_offset(0x10000 + (tile & 0x3FF) * 32 + (ty & 7) * 8 + (tx & 7)) as usize]
                } else {
                    let byte = ctx.vram[vram_offset(0x10000 + (tile & 0x3FF) * 32 + (ty & 7) * 4 + (tx & 7) / 2) as usize];
                    (byte >> ((tx & 1) * 4)) & 0xF
                };
                plot!((ox + local_x) as usize, index);
            }
        } else {
            // Regular sprite: fetch each 8-texel tile row once — 4 or 8 contiguous
            // bytes; the vram mirror folds at 8-aligned boundaries, so a row never
            // straddles one — and skip it whole when fully transparent, instead of
            // re-deriving the tile address per pixel.
            let w = o.w as i32;
            let h_flip = o.flags & F_HFLIP != 0;
            let ty = (if o.flags & F_VFLIP != 0 { o.h as i32 - 1 - local_y } else { local_y }) as u32;
            let row_tile = o.base_tile as u32 + (ty / 8) * row_stride;
            let x_end = w.min(DISPLAY_WIDTH as i32 - ox);
            let mut local_x = x_start;
            while local_x < x_end {
                let tx0 = (if h_flip { w - 1 - local_x } else { local_x }) as u32;
                let in_tile = if h_flip { (tx0 & 7) + 1 } else { 8 - (tx0 & 7) };
                let run = (in_tile as i32).min(x_end - local_x);
                let tile = row_tile + (tx0 / 8) * tile_step;
                if is_8bpp {
                    let row = vram_offset(0x10000 + (tile & 0x3FF) * 32 + (ty & 7) * 8) as usize;
                    let row64 = u64::from_le_bytes(ctx.vram[row..row + 8].try_into().unwrap());
                    if row64 != 0 {
                        for i in 0..run {
                            let tx = if h_flip { tx0 - i as u32 } else { tx0 + i as u32 };
                            plot!((ox + local_x + i) as usize, (row64 >> ((tx & 7) * 8)) as u8);
                        }
                    }
                } else {
                    let row = vram_offset(0x10000 + (tile & 0x3FF) * 32 + (ty & 7) * 4) as usize;
                    let row32 = u32::from_le_bytes(ctx.vram[row..row + 4].try_into().unwrap());
                    if row32 != 0 {
                        for i in 0..run {
                            let tx = if h_flip { tx0 - i as u32 } else { tx0 + i as u32 };
                            plot!((ox + local_x + i) as usize, ((row32 >> ((tx & 7) * 4)) & 0xF) as u8);
                        }
                    }
                }
                local_x += run;
            }
        }
    }
}

/// Everything one rasterizing core reuses line to line: the per-line layer buffers, the
/// selection scratch and the frame's pre-decoded OAM. One per worker, nothing shared.
/// The layer buffers persist across lines and frames — every draw clears exactly what it
/// covers (transparency is the absence of a write), so reuse never shows.
pub struct RasterScratch {
    obj: ObjLayer,
    bg01: BgHalf,
    bg23: BgHalf,
    sel: Selection,
    oam: OamScratch,
}

impl RasterScratch {
    pub fn new() -> Box<RasterScratch> {
        Box::new(RasterScratch {
            obj: ObjLayer::default(),
            bg01: BgHalf::default(),
            bg23: BgHalf::default(),
            sel: Selection::new(),
            oam: OamScratch::new(),
        })
    }

    /// Once per frame, before the line loop: pre-decode the slot's (frozen) OAM into
    /// per-line sprite lists for this core's share of the scanlines.
    pub fn begin_frame(&mut self, mem: &GpuMemBuf, lines: Range<u32>) {
        self.oam.decode(&mem.oam[..], lines);
    }
}

/// One scanline end to end on one core: objects and the window mask, all four bg layers,
/// compose — straight into the frame buffer row. `line` must be inside the range this
/// scratch's `begin_frame` decoded.
pub fn draw_scanline(mem: &GpuMemBuf, regs: &PpuRegs, disp_cnt: DispCnt, line: u32, scratch: &mut RasterScratch, fb_line: &mut [u32]) {
    draw_objs(mem, regs, disp_cnt, line, &mut scratch.obj, &scratch.oam);
    draw_bgs(mem, regs, disp_cnt, line, &mut scratch.bg01, 0);
    draw_bgs(mem, regs, disp_cnt, line, &mut scratch.bg23, 2);
    compose_scanline(&scratch.obj, &scratch.bg01, &scratch.bg23, line, fb_line, &mut scratch.sel);
}

}