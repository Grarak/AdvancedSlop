use crate::core::thread_regs;
use crate::core::apu::{Apu, SoundSampler};
use crate::core::cpu_regs::CpuRegs;
use crate::core::cycle_manager::CycleManager;
use crate::core::gpu::Gpu;
use crate::core::graphics::gpu_mem_buf::{DIRTY_BG, DIRTY_OAM, DIRTY_OBJ, DIRTY_PAL};
use crate::core::input::Input;
use crate::core::memory::cartridge::Cartridge;
use crate::core::memory::dma::Dma;
use crate::core::memory::mem::Memory;
use crate::core::rtc::Rtc;
use crate::core::thread_regs::ThreadRegs;
use crate::core::timers::Timers;
use crate::jit::jit_memory::JitMemory;
use crate::savestate::{Savestate, SavestateContext};
use crate::settings::{Settings, DEFAULT_SETTINGS};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU16, AtomicU32};
use std::sync::Arc;

pub struct Emu {
    pub cartridge: Cartridge,
    pub gpu: Gpu,
    pub cm: CycleManager,
    pub cpu: CpuRegs,
    pub input: Input,
    pub mem: Memory,
    pub apu: Apu,
    pub rtc: Rtc,
    pub dma: Dma,
    pub timers: Timers,
    pub jit: JitMemory,
    pub settings: Settings,
    pub breakout_imm: bool,
    initialized: bool,
}

impl Emu {
    pub fn new(fps: Arc<AtomicU16>, key_map: Arc<AtomicU32>, sound_sampler: NonNull<SoundSampler>, jit: JitMemory) -> Self {
        Emu {
            cartridge: Cartridge::new(),
            gpu: Gpu::new(fps),
            cm: CycleManager::new(),
            cpu: CpuRegs::new(),
            input: Input::new(key_map),
            mem: Memory::new(),
            apu: Apu::new(sound_sampler),
            rtc: Rtc::new(),
            dma: Dma::new(),
            timers: Timers::new(),
            jit,
            settings: DEFAULT_SETTINGS.clone(),
            breakout_imm: false,
            initialized: true,
        }
    }

    pub fn reset(&mut self) {
        self.jit.init(&self.settings);
        if !self.initialized {
            *crate::core::thread_regs() = ThreadRegs::default();
            self.gpu.init();
            self.cm.init();
            self.cpu = CpuRegs::new();
            self.mem.init();
            self.apu.init();
            self.rtc = Rtc::new();
            self.dma = Dma::new();
            self.timers = Timers::new();
        }
        self.initialized = false;
    }

    // Guest state only: jit, settings and host resources are absent by construction.
    // breakout_imm/initialized are runtime control flow, not guest state.
    pub(crate) fn savestate(&mut self, state: &mut SavestateContext) {
        self.cartridge.savestate(state);
        self.gpu.savestate(state);
        self.cm.savestate(state);
        self.cpu.savestate(state);
        self.input.savestate(state);
        self.mem.savestate(state);
        self.apu.savestate(state);
        self.rtc.savestate(state);
        self.dma.savestate(state);
        self.timers.savestate(state);
        thread_regs().savestate(state);
    }

    pub fn save_state(&mut self, label: &str, screenshot: &[u8]) -> Option<Vec<u8>> {
        // Progress split: serialize is a fast field walk, compression dominates
        crate::savestate::op_report(crate::savestate::OpPhase::Serialize, 0);
        let mut state = SavestateContext::new_save();
        self.savestate(&mut state);
        let raw = state.into_data()?;
        Some(crate::savestate::encode_savestate_file(0, label, screenshot, &raw, |done, total| {
            crate::savestate::op_report(crate::savestate::OpPhase::Compress, (5 + done * 90 / total.max(1)) as u8);
        }))
    }

    // Reports load progress/result for the pause-menu dialog; reports are dropped
    // when no dialog armed the op (boot -s loads)
    pub fn load_state(&mut self, data: Vec<u8>) -> bool {
        crate::savestate::op_report(crate::savestate::OpPhase::Decompress, 10);
        // Header and payload are validated before a byte of emu state is touched, so a
        // file from another build/version is rejected with the session intact.
        let Some(raw) = crate::savestate::decode_savestate_file(&data, 0) else {
            crate::savestate::op_fail();
            return false;
        };
        crate::savestate::op_report(crate::savestate::OpPhase::Apply, 60);
        let mut state = SavestateContext::new_load(raw);
        self.savestate(&mut state);
        // Past this point the field walk has already overwritten guest state, whether or
        // not it consumed the file exactly. The fixups have to run either way: the mmu
        // tables, the compiled blocks and the renderer's dirty bits all describe memory
        // that has changed, and leaving them describing the old memory is a host-level
        // fault, not a wrong frame.
        let success = state.is_load_successful();
        crate::savestate::op_report(crate::savestate::OpPhase::Apply, 85);
        self.savestate_post_load();
        if success {
            crate::savestate::op_finish_load(data.len());
        } else {
            crate::savestate::op_fail();
        }
        success
    }

    pub fn savestate_to_file(&mut self, target: crate::savestate::SaveTarget, screenshot: &[u8]) {
        use crate::savestate::SaveTarget;
        let rom_path = self.cartridge.io.file_path.clone();
        let dir = crate::savestate::savestates_dir(&rom_path);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            crate::logging::info_println!("Failed to create savestate dir {dir:?}: {err}");
            crate::savestate::op_fail();
            return;
        }
        // Resolved after create_dir_all: next_numbered_path probes for existing files.
        let path = match target {
            SaveTarget::NewSlot => crate::savestate::next_numbered_path(&rom_path),
            SaveTarget::Quick => crate::savestate::quick_slot_path(&rom_path),
            SaveTarget::Auto => crate::savestate::auto_slot_path(&rom_path),
            SaveTarget::Path(path) => path,
        };
        // Overwriting keeps whatever the state was named; every other target starts unnamed.
        let label = match crate::savestate::peek(&path) {
            crate::savestate::PeekResult::Ok(meta) => meta.label,
            _ => String::new(),
        };
        match self.save_state(&label, screenshot) {
            Some(data) => {
                crate::savestate::op_report(crate::savestate::OpPhase::Write, 95);
                match std::fs::write(&path, &data) {
                    Ok(()) => {
                        crate::logging::info_println!("Savestate ({} bytes) written to {path:?}", data.len());
                        crate::savestate::op_finish_save(data.len());
                    }
                    Err(err) => {
                        crate::logging::info_println!("Failed to write savestate {path:?}: {err}");
                        crate::savestate::op_fail();
                    }
                }
            }
            None => {
                crate::logging::info_println!("Savestate serialization failed");
                crate::savestate::op_fail();
            }
        }
    }

    /// Quick-load hotkey: the trigger carries no path, so resolve the quick slot here
    /// against the running rom and load it. A missing slot is the normal case before the
    /// first quick-save — say so and carry on rather than failing loudly.
    pub fn loadstate_from_quick_slot(&mut self) {
        let path = crate::savestate::quick_slot_path(&self.cartridge.io.file_path);
        match std::fs::read(&path) {
            Ok(data) => {
                if self.load_state(data) {
                    crate::logging::info_println!("Quick savestate loaded from {path:?}");
                } else {
                    crate::logging::info_println!("Quick savestate load failed (corrupt or from another version)");
                }
            }
            Err(err) => {
                crate::logging::info_println!("No quick savestate at {path:?}: {err}");
                crate::savestate::op_fail();
            }
        }
    }

    fn savestate_post_load(&mut self) {
        // Host mappings derive from the restored cartridge/mmu state
        self.mmu_update_all();

        // mmu_update_all maps every rom page, including the head page that GPIO/RTC
        // reads have to fault out of. cartridge_auto_enable_rtc only unmaps it on the
        // first GPIO write and early-returns once has_rtc is set, so nothing would put
        // it back — RTC reads would silently return rom bytes for the rest of the
        // session. Unmapping is always safe (it only forces that page's reads onto the
        // slow path), so key it on has_rtc alone.
        if self.cartridge.io.has_rtc {
            self.mmu_unmap_rom_head();
        }

        self.cartridge_savestate_post_load();

        // The restored vram/palette/oam bear no relation to what the renderer last
        // snapshotted; mark all of it dirty so the next vblank re-snapshots everything
        // instead of waiting for the guest to write it again.
        self.mem.gpu_mem_dirty = DIRTY_BG | DIRTY_OBJ | DIRTY_PAL | DIRTY_OAM;

        // Every compiled block may mismatch the restored memory; full jit reset. Safe
        // here and nowhere else in a frame: cm events are dispatched between jit execute
        // calls (execute_jit), so no compiled frame is on the stack.
        self.jit.init(&self.settings);
    }
}
