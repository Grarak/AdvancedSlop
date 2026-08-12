use crate::core::cpu_regs::InterruptFlag;
use crate::core::cycle_manager::{CycleManager, EventType};
use crate::core::emu::Emu;
use crate::core::graphics::gl_glyph::GlGlyph;
use crate::core::graphics::gl_utils::GpuFbo;
use crate::core::graphics::gpu_mem_buf::GpuMemBuf;
use crate::core::memory::dma::DmaTransferMode;
use crate::core::ppu::soft_ppu::RasterScratch;
use crate::core::ppu::PpuRegs;
use crate::mmap::Shm;
use crate::presenter::Presenter;
use crate::savestate::Savestate;
use crate::utils::{HeapArray, HeapMem, PtrWrapper};
use bilge::prelude::*;
use std::intrinsics::unlikely;
use std::ptr::NonNull;
use std::cell::UnsafeCell;
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

pub const DISPLAY_WIDTH: usize = 240;
pub const DISPLAY_HEIGHT: usize = 160;
pub const DISPLAY_PIXEL_COUNT: usize = DISPLAY_WIDTH * DISPLAY_HEIGHT;

// GBA timing: 4 cycles/dot, 308 dots/line (240 draw + 68 hblank), 228 lines.
pub const CYCLES_PER_LINE: u32 = 308 * 4;
pub const HDRAW_CYCLES: u32 = 240 * 4;
pub const HBLANK_CYCLES: u32 = CYCLES_PER_LINE - HDRAW_CYCLES;
pub const TOTAL_LINES: u16 = 228;
pub const VISIBLE_LINES: u16 = 160;

struct FrameRateCounter {
    frame_counter: u16,
    fps: Arc<AtomicU16>,
    last_update: Instant,
}

impl FrameRateCounter {
    fn new(fps: Arc<AtomicU16>) -> Self {
        FrameRateCounter {
            frame_counter: 0,
            fps,
            last_update: Instant::now(),
        }
    }

    fn on_frame_ready(&mut self, stats: &RenderStats) {
        self.frame_counter += 1;
        let now = Instant::now();
        if unlikely(now.duration_since(self.last_update).as_millis() >= 1000) {
            self.fps.store(self.frame_counter, Ordering::Relaxed);
            match stats.take() {
                Some(([snapshot, core0, core1, present], skips)) => {
                    eprintln!("{} snapshot {snapshot}us core0 {core0}us core1 {core1}us present {present}us skipped {skips}", self.frame_counter)
                }
                None => eprintln!("{}", self.frame_counter),
            }
            self.frame_counter = 0;
            self.last_update = now;
        }
    }
}

#[bitsize(16)]
#[derive(Copy, Clone, FromBits)]
pub struct DispStat {
    v_blank_flag: bool,
    h_blank_flag: bool,
    v_counter_flag: bool,
    v_blank_irq_enable: bool,
    h_blank_irq_enable: bool,
    v_counter_irq_enable: bool,
    not_used: u2,
    v_count_setting: u8,
}

#[bitsize(16)]
#[derive(Copy, Clone, Default, FromBits)]
pub struct DispCnt {
    pub bg_mode: u3,
    pub cgb_mode: bool,
    pub display_frame_select: bool,
    pub hblank_interval_free: bool,
    pub obj_mapping_1d: bool,
    pub forced_blank: bool,
    pub screen_display_bg0: bool,
    pub screen_display_bg1: bool,
    pub screen_display_bg2: bool,
    pub screen_display_bg3: bool,
    pub screen_display_obj: bool,
    pub window0_display: bool,
    pub window1_display: bool,
    pub obj_window_display: bool,
}

crate::savestate::impl_savestate_bytes!(DispStat, DispCnt);

#[derive(Savestate)]
pub struct Gpu {
    pub disp_stat: DispStat,
    pub disp_cnt: DispCnt,
    pub green_swap: u16,
    pub v_count: u16,
    pub ppu: PpuRegs,
    #[savestate(skip)]
    frame_rate_counter: FrameRateCounter,
    #[savestate(skip)]
    pub renderer: PtrWrapper<GbaRenderer>,
}

impl Gpu {
    pub fn new(fps: Arc<AtomicU16>) -> Gpu {
        Gpu {
            disp_stat: DispStat::from(0),
            disp_cnt: DispCnt::from(0),
            green_swap: 0,
            v_count: 0,
            ppu: PpuRegs::new(),
            frame_rate_counter: FrameRateCounter::new(fps),
            renderer: PtrWrapper::null(),
        }
    }

    pub fn init(&mut self) {
        self.disp_stat = DispStat::from(0);
        self.disp_cnt = DispCnt::from(0);
        self.green_swap = 0;
        self.v_count = 0;
        self.ppu = PpuRegs::new();
    }

    pub fn set_gpu_renderer(&mut self, renderer: NonNull<GbaRenderer>) {
        self.renderer = PtrWrapper::new(renderer.as_ptr());
    }

    pub fn initialize_schedule(cm: &mut CycleManager) {
        cm.schedule(HDRAW_CYCLES, EventType::GpuScanline240);
    }

    pub fn get_disp_stat(&self) -> u16 {
        self.disp_stat.into()
    }

    pub fn set_disp_stat(&mut self, mut mask: u16, value: u16) {
        // Flag bits 0-2 are read-only; NooDS parity mask
        mask &= 0xFFB8;
        self.disp_stat = ((u16::from(self.disp_stat) & !mask) | (value & mask)).into();
    }

    pub fn get_disp_cnt(&self) -> u16 {
        self.disp_cnt.into()
    }

    pub fn set_disp_cnt(&mut self, mask: u16, value: u16) {
        self.disp_cnt = ((u16::from(self.disp_cnt) & !mask) | (value & mask)).into();
    }
}

impl Emu {
    // Start of hblank, 960 cycles into the line
    pub fn gpu_on_scanline240_event(&mut self) {
        if self.gpu.v_count < VISIBLE_LINES {
            let disp_cnt = u16::from(self.gpu.disp_cnt);
            let line = self.gpu.v_count as u8;
            self.gpu.renderer.capture_scanline(&self.gpu.ppu, disp_cnt, line);
            self.gpu.ppu.step_internal_refs();
            self.dma_trigger_all(DmaTransferMode::StartAtHBlank);
        }

        self.gpu.disp_stat.set_h_blank_flag(true);
        if self.gpu.disp_stat.h_blank_irq_enable() {
            self.cpu_send_interrupt(InterruptFlag::LcdHBlank);
        }

        // From the due cycle, not the overshot dispatch cycle: the jit overshoots events
        // by up to a scheduler quantum, and rescheduling from the overshot count stretches
        // every scanline — the display grid drifts against the timer/apu grids (m4a then
        // under-produces pcm per frame and the sound DMA overruns its ring: audible pops).
        self.cm.schedule_from_due(HBLANK_CYCLES, EventType::GpuScanline308);
    }

    // End of line
    pub fn gpu_on_scanline308_event(&mut self) {
        self.gpu.v_count += 1;
        match self.gpu.v_count {
            VISIBLE_LINES => {
                self.gpu.disp_stat.set_v_blank_flag(true);
                if self.gpu.disp_stat.v_blank_irq_enable() {
                    self.cpu_send_interrupt(InterruptFlag::LcdVBlank);
                }
                self.dma_trigger_all(DmaTransferMode::StartAtVBlank);

                self.gpu.renderer.on_frame_finish(&mut self.mem.gpu_mem_dirty);
                self.gpu.frame_rate_counter.on_frame_ready(&self.gpu.renderer.stats);
            }
            227 => self.gpu.disp_stat.set_v_blank_flag(false),
            TOTAL_LINES => {
                self.gpu.v_count = 0;
                self.gpu.ppu.reload_internal_refs();
            }
            _ => {}
        }

        let v_match = u16::from(self.gpu.disp_stat) >> 8;
        if self.gpu.v_count == v_match {
            self.gpu.disp_stat.set_v_counter_flag(true);
            if self.gpu.disp_stat.v_counter_irq_enable() {
                self.cpu_send_interrupt(InterruptFlag::LcdVCounterMatch);
            }
        } else {
            self.gpu.disp_stat.set_v_counter_flag(false);
        }
        self.gpu.disp_stat.set_h_blank_flag(false);

        self.cm.schedule_from_due(HDRAW_CYCLES, EventType::GpuScanline240);

        // Vblank entry: savestate hook + cheats + hotkeys, same placement as upstream
        if unlikely(self.gpu.v_count == VISIBLE_LINES) {
            if let Some(request) = crate::savestate::take_request() {
                match request {
                    crate::savestate::SavestateRequest::Save { target, screenshot } => self.savestate_to_file(target, &screenshot),
                    crate::savestate::SavestateRequest::Load { data } => {
                        if self.load_state(data) {
                            crate::logging::info_println!("Savestate loaded");
                        } else {
                            crate::logging::info_println!("Savestate load failed (corrupt)");
                        }
                    }
                    crate::savestate::SavestateRequest::LoadQuick => self.loadstate_from_quick_slot(),
                }
            }
            self.input_process_hotkeys();
        }
    }
}

/// The software framebuffer's first row is guest line 0. Desktop GL's texture row 0 is the
/// bottom, so presenting it has to flip; GXM is top-down and must not.
const SOFT_BLIT_FLIP: bool = !cfg!(target_os = "vita");

// Pixel targets, round-robined in lockstep by every consumer's own cursor. The GL driver
// may still be reading a texture when its SwapBuffers returns, so a buffer must not come
// around again too soon. The workers write frame n's buffer as soon as frame n-2 is
// freed, i.e. right after frame n-3's swap returned — five deep therefore keeps two
// later swap-returns between a buffer's own swap (frame n-5) and its rewrite, the same
// slack the old three-deep chain gave a compositor that only wrote after presenting the
// previous frame.
const FRAME_BUFS: usize = 5;

/// The display registers as they stood at each scanline's hblank. Guest code rewrites
/// scroll offsets, affine references, windows and blend settings mid-frame (that is what
/// hblank irqs are for), so a whole-frame rasterizer cannot read the live registers.
pub struct GbaRenderRegs {
    pub disp_cnt: [u16; DISPLAY_HEIGHT],
    pub ppu: [PpuRegs; DISPLAY_HEIGHT],
}

impl Default for GbaRenderRegs {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl GbaRenderRegs {
    fn on_scanline(&mut self, regs: &PpuRegs, disp_cnt: u16, line: u8) {
        let line = line as usize;
        unsafe { std::hint::assert_unchecked(line < DISPLAY_HEIGHT) };
        self.disp_cnt[line] = disp_cnt;
        self.ppu[line] = *regs;
    }
}

// Swapchain depth. Two, so the emulation thread can snapshot the next frame into one
// slot while the render side is still working through the other: it only has to drop a
// frame when both are occupied, rather than whenever rendering has not caught up.
const SWAPCHAIN: usize = 2;

// Software rasterizer state.
struct SoftRenderer {
    // Per slot: the frame's inputs — a snapshot of guest video memory and the
    // per-scanline register capture. Everything a frame needs is in its own slot, which
    // is what lets the emulation thread hand one over and walk away. There are no
    // per-slot outputs: each worker composes its half of the scanlines as it draws them,
    // straight into a pixel buffer.
    mem: [UnsafeCell<GpuMemBuf>; SWAPCHAIN],
    regs: [UnsafeCell<HeapMem<GbaRenderRegs>>; SWAPCHAIN],
    // Pixel targets the workers compose into before uploading. Unused on the Vita, where
    // vitaGL hands out the texture's own memory and the workers write straight into it —
    // no scratch, no upload, no copy.
    #[cfg(not(target_os = "vita"))]
    pixels: [HeapArray<u32, DISPLAY_PIXEL_COUNT>; FRAME_BUFS],
    fbs: [GpuFbo; FRAME_BUFS],
    #[cfg(target_os = "vita")]
    tex_ptrs: [*mut u32; FRAME_BUFS],
}

// Shared between the three threads. Which of them may touch what, and when, is the whole
// job of RenderSync below.
unsafe impl Send for SoftRenderer {}
unsafe impl Sync for SoftRenderer {}

impl SoftRenderer {
    fn new() -> Self {
        let fbs = std::array::from_fn(|_| GpuFbo::new(DISPLAY_WIDTH as u32, DISPLAY_HEIGHT as u32, false, false).unwrap());
        // vitaGL lays a texture out at VGL_ALIGN(width, 8) * bpp stride; 240 is 8-aligned,
        // so rows are tight and a scanline write lands where the sampler expects it.
        #[cfg(target_os = "vita")]
        let tex_ptrs = unsafe {
            let mut ptrs = [std::ptr::null_mut(); FRAME_BUFS];
            for (i, fbo) in fbs.iter().enumerate() {
                gl::BindTexture(gl::TEXTURE_2D, fbo.color);
                ptrs[i] = crate::presenter::Presenter::gl_get_tex_ptr() as *mut u32;
            }
            gl::BindTexture(gl::TEXTURE_2D, 0);
            ptrs
        };

        SoftRenderer {
            mem: std::array::from_fn(|_| UnsafeCell::new(GpuMemBuf::new())),
            regs: std::array::from_fn(|_| UnsafeCell::new(HeapMem::default())),
            #[cfg(not(target_os = "vita"))]
            pixels: std::array::from_fn(|_| HeapArray::default()),
            fbs,
            #[cfg(target_os = "vita")]
            tex_ptrs,
        }
    }

    /// One rasterizing core's half of a pixel buffer: rows 0-79 or 80-159. The two
    /// workers hold both halves of one buffer at the same time, which is why each gets a
    /// slice of only its own rows; aliasing between whole buffers is prevented by
    /// RenderSync and the lockstep cursors, not the borrow checker.
    #[allow(clippy::mut_from_ref)]
    unsafe fn buf_half(&self, index: usize, half: usize) -> &mut [u32] {
        #[cfg(target_os = "vita")]
        let base = self.tex_ptrs[index];
        #[cfg(not(target_os = "vita"))]
        let base = self.pixels[index].as_ptr() as *mut u32;
        std::slice::from_raw_parts_mut(base.add(half * (DISPLAY_PIXEL_COUNT / 2)), DISPLAY_PIXEL_COUNT / 2)
    }

    /// One slot's frame inputs: written by the emulation thread while the slot is free,
    /// read by everyone else while it is queued.
    #[allow(clippy::mut_from_ref)]
    unsafe fn mem(&self, slot: usize) -> &mut GpuMemBuf {
        &mut *self.mem[slot].get()
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn regs(&self, slot: usize) -> &mut HeapMem<GbaRenderRegs> {
        &mut *self.regs[slot].get()
    }

    /// A whole composed pixel buffer, as the workers left it. Read-only, and only
    /// sound for a buffer no worker currently owns (the savestate screenshot reads the
    /// last presented one while the game is paused).
    unsafe fn buf(&self, index: usize) -> &[u32] {
        #[cfg(target_os = "vita")]
        let base = self.tex_ptrs[index] as *const u32;
        #[cfg(not(target_os = "vita"))]
        let base = self.pixels[index].as_ptr();
        std::slice::from_raw_parts(base, DISPLAY_PIXEL_COUNT)
    }

    /// Publishes a composed buffer and returns the fbo holding it.
    unsafe fn upload(&self, index: usize) -> gl::types::GLuint {
        // Nothing to upload on the Vita: those scanlines landed in the texture itself
        #[cfg(not(target_os = "vita"))]
        {
            gl::BindTexture(gl::TEXTURE_2D, self.fbs[index].color);
            gl::TexSubImage2D(gl::TEXTURE_2D, 0, 0, 0, DISPLAY_WIDTH as i32, DISPLAY_HEIGHT as i32, gl::RGBA, gl::UNSIGNED_BYTE, self.pixels[index].as_ptr() as _);
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        self.fbs[index].fbo
    }
}

/// Per-stage timing, accumulated in microseconds and reported with the fps line. The
/// scanline split is balanced by construction, but confirming that — and that the
/// present thread keeps up — is not visible from the frame rate alone. `snapshot` is the
/// only one of these the emulation thread pays for; core0/core1 are each worker's
/// raster+compose share of its half of the lines.
///
/// Debug-assertion builds only: in release the struct is empty and every method a no-op,
/// so GbaRenderer carries nothing for it.
#[derive(Default)]
pub struct RenderStats {
    // Stage accumulators, indexed by RenderStage; the order is the stats line's.
    #[cfg(debug_assertions)]
    stages: [AtomicU64; 4],
    #[cfg(debug_assertions)]
    skips: AtomicU64,
    #[cfg(debug_assertions)]
    frames: AtomicU64,
}

#[derive(Clone, Copy)]
enum RenderStage {
    Snapshot,
    Core0,
    Core1,
    Present,
}

#[cfg_attr(not(debug_assertions), allow(unused_variables))]
impl RenderStats {
    fn add(&self, stage: RenderStage, start: std::time::Instant) {
        #[cfg(debug_assertions)]
        self.stages[stage as usize].fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    fn count_skip(&self) {
        #[cfg(debug_assertions)]
        self.skips.fetch_add(1, Ordering::Relaxed);
    }

    fn count_frame(&self) {
        #[cfg(debug_assertions)]
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Mean microseconds per frame for each stage plus the skip count, and resets.
    fn take(&self) -> Option<([u64; 4], u64)> {
        #[cfg(debug_assertions)]
        {
            let frames = self.frames.swap(0, Ordering::Relaxed).max(1);
            let skips = self.skips.swap(0, Ordering::Relaxed);
            Some((std::array::from_fn(|i| self.stages[i].swap(0, Ordering::Relaxed) / frames), skips))
        }
        #[cfg(not(debug_assertions))]
        None
    }
}

// Per-slot state. QUEUED means the emulation thread has filled in the slot's inputs;
// HALF0/HALF1 are the two rasterizing cores reporting their share done. A slot is free
// again — and reusable by the emulation thread — when the compositor zeroes it.
const S_QUEUED: u8 = 1 << 0;
const S_HALF: [u8; 2] = [1 << 1, 1 << 2];
const S_RASTERED: u8 = S_QUEUED | S_HALF[0] | S_HALF[1];

/// The frame handshake: one atomic per swapchain slot, plus one condvar to wake the
/// three threads that sleep on it.
///
/// The emulation thread never waits for rendering. At vblank it snapshots guest video
/// memory into a free slot, marks it QUEUED and carries straight on; if no slot is free
/// it drops the frame instead. Everything downstream reads that snapshot, so nothing it
/// does can be disturbed by the guest and nothing the guest does has to wait for it.
///
/// Each rasterizing core ors its own HALF bit in when it has drawn *and composed* its
/// half of the slot's scanlines into the current pixel buffer; the present thread takes
/// the slot once both are in, zeroes the state to release it, and presents that buffer.
///
/// Slots are produced and consumed strictly in order, so each thread just walks its own
/// cursor round the ring rather than searching for work.
#[derive(Default)]
struct RenderSync {
    slots: [AtomicU8; SWAPCHAIN],
}

impl RenderSync {
    fn state(&self, slot: usize) -> u8 {
        self.slots[slot].load(Ordering::Acquire)
    }
}

// The renderer. One frame per vblank, produced by the emulation thread as a snapshot and
// consumed by three others: two rasterizing cores that each draw and compose half of the
// scanlines straight into a pixel buffer, and a present thread that uploads, blits and
// swaps. See DEVELOPMENT.md section 8 for the whole picture; the invariants there are
// load-bearing.
pub struct GbaRenderer {
    quit: AtomicBool,
    // Set by the main thread to freeze the game for the pause menu; the cpu thread
    // parks itself at the next vblank handoff until it clears.
    pause: AtomicBool,
    // One-shot ticket that lets the parked cpu thread past the pause loop without
    // resuming the game: it then reaches the savestate hook a few lines below the park,
    // performs the queued save/load, runs one frame and parks again.
    pause_step: AtomicBool,
    present_rect: (i32, i32, i32, i32),
    sync: RenderSync,
    // The emulation thread never sleeps on this; it is for the three that do.
    wake_mutex: Mutex<()>,
    wake_condvar: Condvar,
    // cpu-thread only, written per scanline, swapped into the slot at vblank
    capture_regs: HeapMem<GbaRenderRegs>,
    // What has changed in guest video memory since each slot was last snapshotted. Per
    // slot, because a slot two frames old needs everything that has happened since, not
    // just what changed in the last frame.
    slot_dirty: [u8; SWAPCHAIN],
    // Guest memory. Set once per game, before the other threads start; the shm mapping
    // is made at startup and never moves.
    shm: *const Shm,
    // Created by reset_for_new_game on the GL thread before the other threads start;
    // shared by all three from then on (see SoftRenderer's Send/Sync note).
    soft_renderer: Option<SoftRenderer>,
    pub stats: RenderStats,
    // Emulation-thread only: the swapchain slot it hands over next
    write_slot: usize,
    // GL-thread only
    gl_glyph: Option<GlGlyph>,
    // The swapchain slot the present thread consumes next
    read_slot: usize,
    // Pixel buffer the present thread consumes next. The workers run their own cursors
    // over the same ring; every consumer advances once per slot, so they stay in step.
    present_pixels: usize,
    // Source of the last presented frame, so the pause menu can re-blit it
    last_blit: Option<gl::types::GLuint>,
    // GL-thread only: which RA event the notification is currently showing, so a repeat
    // of the same one does not restart its fade.
    ra_last_event_instant: Option<std::time::Instant>,
}

impl GbaRenderer {
    pub fn new() -> Self {
        GbaRenderer {
            quit: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            pause_step: AtomicBool::new(false),
            present_rect: (0, 0, crate::presenter::PRESENTER_SCREEN_WIDTH as i32, crate::presenter::PRESENTER_SCREEN_HEIGHT as i32),
            sync: RenderSync::default(),
            wake_mutex: Mutex::new(()),
            wake_condvar: Condvar::new(),
            capture_regs: HeapMem::default(),
            slot_dirty: [0xF; SWAPCHAIN],
            shm: std::ptr::null(),
            soft_renderer: None,
            stats: RenderStats::default(),
            write_slot: 0,
            gl_glyph: None,
            read_slot: 0,
            present_pixels: 0,
            last_blit: None,
            ra_last_event_instant: None,
        }
    }

    /// Draw the most recent RetroAchievements unlock over the frame, fading in over the
    /// first half of its lifetime and back out over the second.
    ///
    /// Text only: blitting the achievement badge image would need a dedicated shader
    /// program, and this renderer has no equivalent to hang it off yet.
    unsafe fn draw_ra_event(&mut self, ra_context: &crate::ra_context::RaContext) {
        use crate::presenter::{PRESENTER_SCREEN_HEIGHT, PRESENTER_SCREEN_WIDTH};
        use glyph_brush::{HorizontalAlign, Layout, VerticalAlign};

        const DURATION: std::time::Duration = std::time::Duration::from_secs(5);

        let (title, description, elapsed) = {
            let event = ra_context.event.lock().unwrap();
            let (event, instant) = &*event;
            let elapsed = instant.elapsed();
            if event.title.is_empty() || elapsed > DURATION {
                // Let a later event with the same text fade in again from the start.
                self.ra_last_event_instant = None;
                return;
            }
            self.ra_last_event_instant = Some(*instant);
            (event.title.clone(), event.description.clone(), elapsed)
        };

        // Triangle over the lifetime: 0 at both ends, 1 in the middle.
        let half = DURATION.as_secs_f32() / 2.0;
        let alpha = 1.0 - ((elapsed.as_secs_f32() - half) / half).abs();

        const OFFSET_Y: u32 = 416;
        const WIDTH: u32 = 500;
        const HEIGHT: u32 = PRESENTER_SCREEN_HEIGHT - OFFSET_Y;
        let _ = PRESENTER_SCREEN_WIDTH;
        gl::Viewport(0, OFFSET_Y as _, WIDTH as _, HEIGHT as _);

        gl::Enable(gl::BLEND);
        gl::BlendEquation(gl::FUNC_ADD);
        gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);

        self.gl_glyph.get_or_insert_with(GlGlyph::new).draw(
            format!("{title}\n{description}"),
            (WIDTH as f32, HEIGHT as f32),
            (0.0, 0.0),
            32.0,
            Layout::default().h_align(HorizontalAlign::Left).v_align(VerticalAlign::Center),
            alpha.clamp(0.0, 1.0),
        );

        gl::Disable(gl::BLEND);
    }

    // Reset the per-game handoff state so the same renderer drives another game after a
    // return-to-menu. Must run on the GL thread before the cpu thread starts: it is what
    // creates the frame buffers the cpu thread rasterizes into.
    pub fn reset_for_new_game(&mut self, shm: &Shm) {
        self.quit.store(false, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
        self.pause_step.store(false, Ordering::Relaxed);
        self.sync = RenderSync::default();
        self.write_slot = 0;
        self.read_slot = 0;
        self.slot_dirty = [0xF; SWAPCHAIN];
        self.present_pixels = 0;
        self.shm = shm;
        self.soft_renderer.get_or_insert_with(SoftRenderer::new);
    }

    pub fn set_pause(&self, pause: bool) {
        self.pause.store(pause, Ordering::Release);
    }

    /// Wake the paused cpu thread for exactly one frame, leaving the pause in place.
    /// Used to let it consume a savestate request queued from the pause menu: the
    /// request is handled at the vblank hook it parks in, then it parks again.
    pub fn step_paused_frame(&self, cpu_thread: &std::thread::Thread) {
        self.pause_step.store(true, Ordering::Release);
        cpu_thread.unpark();
    }

    /// Rasterizes one visible scanline into the buffer the cpu thread currently owns.
    /// Called from the line's hblank so the registers are the ones the hardware would
    /// have latched — mid-frame writes (hblank irq effects) land on the right lines
    /// without anything being captured.
    pub fn capture_scanline(&mut self, regs: &PpuRegs, disp_cnt: u16, line: u8) {
        self.capture_regs.on_scanline(regs, disp_cnt, line);
    }

    /// Signals the sleeping consumers that a counter moved. Taking the wake mutex is
    /// what closes the race against a waiter that has tested its predicate but not yet
    /// parked; it is never held while doing work.
    fn wake(&self) {
        let _guard = self.wake_mutex.lock().unwrap();
        self.wake_condvar.notify_all();
    }

    /// Vblank: snapshot the frame into a free swapchain slot and go straight back to the
    /// guest. Nothing here waits for rendering — the rasterizers work from the snapshot,
    /// so the guest may overwrite vram the moment this returns.
    pub fn on_frame_finish(&mut self, gpu_mem_dirty: &mut u8) {
        // Every slot needs to know about this frame's writes, not just the one being
        // filled: a slot skipped now must still pick them up when it is next used.
        for dirty in &mut self.slot_dirty {
            *dirty |= *gpu_mem_dirty;
        }
        *gpu_mem_dirty = 0;

        let slot = self.write_slot;
        if self.sync.state(slot) == 0 {
            self.queue_frame(slot);
            self.write_slot = (slot + 1) % SWAPCHAIN;
        } else {
            // Both slots still in flight: drop the frame rather than wait for one.
            self.stats.count_skip();
        }
        self.stats.count_frame();

        // Pause: freeze the game between frames (the last frame is already handed over,
        // so the pause menu draws over it). The main thread unparks on resume/quit, or
        // hands over a single-frame ticket (step_paused_frame) so a savestate request
        // queued from the menu gets consumed at the hook a few lines below without the
        // game actually resuming. Testing the ticket before parking is what makes the
        // set-then-unpark pair race-free in both orders.
        while self.pause.load(Ordering::Acquire) && !self.quit.load(Ordering::Relaxed) {
            if self.pause_step.swap(false, Ordering::AcqRel) {
                break;
            }
            std::thread::park();
        }
    }

    fn queue_frame(&mut self, slot: usize) {
        let Some(soft) = &self.soft_renderer else { return };
        let snapshot_start = std::time::Instant::now();
        unsafe {
            // Double-buffer swap: the slot gets this frame's captures, the cpu thread
            // keeps capturing into the other buffer (every line is rewritten each frame,
            // so stale contents never show)
            std::mem::swap(soft.regs(slot), &mut self.capture_regs);
            soft.mem(slot).snapshot(&*self.shm, self.slot_dirty[slot]);
        }
        self.slot_dirty[slot] = 0;
        self.stats.add(RenderStage::Snapshot, snapshot_start);

        self.sync.slots[slot].store(S_QUEUED, Ordering::Release);
        self.wake();
    }

    /// One rasterizing core's share of a slot: objects, all four bg layers and the
    /// composed pixels for its half of the scanlines, written straight into pixel buffer
    /// `pix`. The other core does the other half; they touch disjoint rows. A scanline
    /// split is balanced by construction whatever the bg mode, and composing a line
    /// right after drawing it keeps the layer buffers in L1 — they never leave the core.
    fn rasterize_half<const HALF: usize>(&self, slot: usize, pix: usize, scratch: &mut RasterScratch) {
        let Some(soft) = &self.soft_renderer else { return };
        let start = HALF * (DISPLAY_HEIGHT / 2);
        let lines = start as u32..(start + DISPLAY_HEIGHT / 2) as u32;
        unsafe {
            let regs = &**soft.regs(slot);
            let mem = &*soft.mem(slot);
            let fb = soft.buf_half(pix, HALF);
            scratch.begin_frame(mem, lines.clone());
            for line in lines {
                let disp_cnt = DispCnt::from(regs.disp_cnt[line as usize]);
                let row = (line as usize - start) * DISPLAY_WIDTH;
                crate::core::ppu::soft_ppu::draw_scanline(mem, &regs.ppu[line as usize], disp_cnt, line, scratch, &mut fb[row..row + DISPLAY_WIDTH]);
            }
        }
    }

    /// Sleeps until `ready` holds. Returns false if the wait gave up (quit, or the
    /// deadline passed).
    fn wait_until(&self, ready: impl Fn() -> bool, deadline: Option<std::time::Instant>) -> bool {
        let mut guard = self.wake_mutex.lock().unwrap();
        loop {
            if ready() {
                return true;
            }
            if self.quit.load(Ordering::Relaxed) {
                return false;
            }
            let timeout = match deadline {
                Some(deadline) => match deadline.checked_duration_since(std::time::Instant::now()) {
                    Some(left) => left,
                    None => return false,
                },
                None => std::time::Duration::from_millis(20),
            };
            guard = self.wake_condvar.wait_timeout(guard, timeout).unwrap().0;
        }
    }

    /// A rasterizing core's whole life: take the next queued slot, draw and compose this
    /// core's half of its scanlines, report done. Pinned for the lifetime of a game, and
    /// asleep between frames — one wakeup a frame is cheap next to the frame it then
    /// spends working.
    /// `HALF` is const so each worker monomorphizes: its sync bit, line range, buffer
    /// slice and stats lane all fold to constants.
    pub fn raster_worker_loop<const HALF: usize>(&self) {
        let bit = S_HALF[HALF];
        let mut slot = 0;
        // This worker's pixel-buffer cursor: in lockstep with the present thread's,
        // since every consumer advances exactly once per slot, from the same start.
        let mut pix = 0;
        let mut scratch = RasterScratch::new();
        // "Queued, and my own share not drawn yet"
        while self.wait_until(|| self.sync.state(slot) & (S_QUEUED | bit) == S_QUEUED, None) {
            let start = std::time::Instant::now();
            self.rasterize_half::<HALF>(slot, pix, &mut scratch);
            self.stats.add(if HALF == 0 { RenderStage::Core0 } else { RenderStage::Core1 }, start);
            self.sync.slots[slot].fetch_or(bit, Ordering::Release);
            self.wake();
            slot = (slot + 1) % SWAPCHAIN;
            pix = (pix + 1) % FRAME_BUFS;
        }
    }

    pub fn set_present_rect(&mut self, rect: (i32, i32, i32, i32)) {
        self.present_rect = rect;
    }

    /// The last presented frame as a jpeg, for savestate thumbnails. Reads the composed
    /// pixels out of host memory — the buffer the present thread last uploaded — rather
    /// than off the GPU: vitaGL has no working glReadPixels, and at 240x160 the frame is
    /// already thumbnail-sized, so nothing has to be scaled either.
    ///
    /// Empty before the first frame or if encoding fails; the list UI then draws the
    /// entry without a thumbnail. Call only while the game is paused, so no worker owns
    /// the buffer being read.
    pub fn capture_frame_jpeg(&self) -> Vec<u8> {
        let (Some(_), Some(soft)) = (self.last_blit, self.soft_renderer.as_ref()) else {
            return Vec::new();
        };
        // present_pixels has already advanced past the buffer that was presented
        let index = (self.present_pixels + FRAME_BUFS - 1) % FRAME_BUFS;
        let pixels = unsafe { soft.buf(index) };

        // Pixels are 0xAABBGGRR words, i.e. r,g,b,a bytes in memory
        let mut rgb = vec![0u8; DISPLAY_PIXEL_COUNT * 3];
        for (dst, pixel) in rgb.chunks_exact_mut(3).zip(pixels) {
            dst.copy_from_slice(&pixel.to_le_bytes()[..3]);
        }

        let mut jpeg = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut jpeg, 80);
        if encoder.encode(&rgb, DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16, jpeg_encoder::ColorType::Rgb).is_err() {
            jpeg.clear();
        }
        jpeg
    }

    /// Re-blit the last rendered frame onto the default framebuffer, so the pause menu
    /// draws over the frozen game instead of a black screen. Render-thread only; a no-op
    /// before the first frame (the texture is created lazily in render_loop).
    pub fn blit_main_framebuffer(&self) {
        let Some(src_fbo) = self.last_blit else { return };
        unsafe { self.blit(src_fbo) };
    }

    /// Blits the finished frame onto the default framebuffer, flipping where the host
    /// disagrees with the rasterizer's row order (SOFT_BLIT_FLIP).
    unsafe fn blit(&self, src_fbo: gl::types::GLuint) {
        let (x, y, width, height) = self.present_rect;
        gl::BindFramebuffer(gl::READ_FRAMEBUFFER, src_fbo);
        let (src_y0, src_y1) = if SOFT_BLIT_FLIP { (DISPLAY_HEIGHT as i32, 0) } else { (0, DISPLAY_HEIGHT as i32) };
        gl::BlitFramebuffer(0, src_y0, DISPLAY_WIDTH as i32, src_y1, x, y, x + width, y + height, gl::COLOR_BUFFER_BIT, gl::NEAREST);
        gl::BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
    }

    pub fn is_quit(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    pub fn set_quit(&mut self, quit: bool) {
        self.quit.store(quit, Ordering::Relaxed);
        if quit {
            self.wake();
        }
    }

    // Runs on the render/main thread with the GL context current
    // `debug_fps` = Some(fps) draws the debug stats overlay over the frame.
    pub fn render_loop(&mut self, presenter: &mut Presenter, debug_fps: Option<u16>, ra_context: Option<&crate::ra_context::RaContext>) {
        // Wait for both rasterizing halves of the next slot in the ring — by then its
        // pixels are fully composed. Nothing upstream waits on this thread — it only
        // presents, which may park on vblank without holding the guest or the
        // rasterizers up — so the deadline exists only to let the main loop keep polling
        // events if the cpu thread stops producing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        let slot = self.read_slot;
        if !self.wait_until(|| self.sync.state(slot) == S_RASTERED, Some(deadline)) {
            return;
        }

        // Free the slot before presenting, not after: it is what the emulation thread
        // needs to avoid dropping the next frame, and the present below may park on
        // vblank for most of one. The pixels are already out of the slot — the workers
        // composed them into the buffer this thread now presents.
        self.sync.slots[slot].store(0, Ordering::Release);
        self.wake();
        self.read_slot = (slot + 1) % SWAPCHAIN;
        let present_start = std::time::Instant::now();

        let index = self.present_pixels;
        let soft = self.soft_renderer.as_ref().unwrap();
        let src_fbo = unsafe { soft.upload(index) };
        self.last_blit = Some(src_fbo);

        unsafe {
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
            gl::ClearColor(0f32, 0f32, 0f32, 1f32);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            self.blit(src_fbo);

            if let Some(fps) = debug_fps {
                let per = fps as u32 * 100 / 60;
                self.gl_glyph.get_or_insert_with(GlGlyph::new).draw_debug_stats(format!("{per}% ({fps}/60)\n{fps} fps"));
            }

            if let Some(ra_context) = ra_context {
                self.draw_ra_event(ra_context);
            }
        }

        presenter.gl_swap_window();

        self.stats.add(RenderStage::Present, present_start);

        // Round-robin in lockstep with the workers' cursors: SwapBuffers can return
        // while the driver is still reading this texture, so the ring must come all the
        // way around before the buffer is rewritten (see FRAME_BUFS).
        self.present_pixels = (index + 1) % FRAME_BUFS;
    }
}
