use crate::core::thread_regs;
use crate::core::apu::{Apu, SoundSampler};
use crate::core::cpu_regs::CpuRegs;
use crate::core::cycle_manager::CycleManager;
use crate::core::gpu::Gpu;
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
    fn savestate(&mut self, state: &mut SavestateContext) {
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

    pub fn save_state(&mut self, screenshot: &[u8]) -> Option<Vec<u8>> {
        // Progress split: serialize is a fast field walk, compression dominates
        crate::savestate::op_report(crate::savestate::OpPhase::Serialize, 0);
        let mut state = SavestateContext::new_save();
        self.savestate(&mut state);
        let raw = state.into_data()?;
        Some(crate::savestate::encode_savestate_file(0, screenshot, &raw, |done, total| {
            crate::savestate::op_report(crate::savestate::OpPhase::Compress, (5 + done * 90 / total.max(1)) as u8);
        }))
    }

    // Reports load progress/result for the pause-menu dialog; reports are dropped
    // when no dialog armed the op (boot -s loads)
    pub fn load_state(&mut self, data: Vec<u8>) -> bool {
        crate::savestate::op_report(crate::savestate::OpPhase::Decompress, 10);
        let Some(raw) = crate::savestate::decode_savestate_file(&data, 0) else {
            crate::savestate::op_fail();
            return false;
        };
        crate::savestate::op_report(crate::savestate::OpPhase::Apply, 60);
        let mut state = SavestateContext::new_load(raw);
        self.savestate(&mut state);
        if !state.is_load_successful() {
            crate::savestate::op_fail();
            return false;
        }
        crate::savestate::op_report(crate::savestate::OpPhase::Apply, 85);
        self.savestate_post_load();
        crate::savestate::op_finish_load(data.len());
        true
    }

    pub fn savestate_to_file(&mut self, screenshot: &[u8]) {
        let rom_path = self.cartridge.io.file_path.clone();
        let dir = rom_path.parent().unwrap_or(std::path::Path::new(".")).join("savestates");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            crate::logging::info_println!("Failed to create savestate dir {dir:?}: {err}");
            crate::savestate::op_fail();
            return;
        }
        let stem = rom_path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut num = 1u32;
        let mut path = dir.join(format!("{stem}-{num}.sav"));
        while path.exists() {
            num += 1;
            path = dir.join(format!("{stem}-{num}.sav"));
        }
        match self.save_state(screenshot) {
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

    fn savestate_post_load(&mut self) {
        // Host mappings derive from the restored cartridge/mmu state
        self.mmu_update_all();

        // Every compiled block may mismatch the restored memory; full jit reset
        self.jit.init(&self.settings);
    }
}
