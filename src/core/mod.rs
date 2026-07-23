use crate::core::thread_regs::ThreadRegs;

pub mod apu;
pub mod cpu_regs;
pub mod cycle_manager;
pub mod emu;
pub mod exception_handler;
pub mod gpu;
pub mod graphics;
pub mod hle;
pub mod input;
pub mod memory;
pub mod ppu;
pub mod rtc;
pub mod thread_regs;
pub mod timers;

// The guest-facing regions (ThreadRegs, JitAsm, the fastmem reservation) need
// process-lifetime-stable bases. On 32-bit hosts they must also be *compile-time*
// constants below 4 GiB: the arm32 backend bakes them as u32 asm/emitter immediates.

pub const GUEST_REGS_ADDR: usize = if cfg!(target_os = "vita") { 0xA8000000 } else { 0xA1000000 };

pub const JIT_ASM_ADDR: usize = if cfg!(target_os = "vita") { 0xA2000000 } else { 0x71000000 };

pub const MMU_TCM_ADDR: usize = if cfg!(target_os = "vita") { 0xC0000000 } else { 0x90000000 };

// Scheduler quantum: the jit checks at taken branches, so a block back-edge can overshoot
// by up to one straight-line run.
pub const MAX_LOOP_CYCLE_COUNT: u32 = 128;
pub const MAX_BRANCH_LOOP_CYCLE_COUNT: u32 = 128;

pub const fn guest_regs_addr() -> usize {
    GUEST_REGS_ADDR
}

pub const fn jit_asm_addr() -> usize {
    JIT_ASM_ADDR
}

pub const fn mmu_tcm_addr() -> usize {
    MMU_TCM_ADDR
}

pub fn thread_regs() -> &'static mut ThreadRegs {
    unsafe { (GUEST_REGS_ADDR as *mut ThreadRegs).as_mut_unchecked() }
}

// The set_*_addr calls sit right after the region mmaps in actual_main/Mmu::new,
// before any thread spawns — pure sanity checks against the baked constants.
pub fn set_guest_regs_addr(addr: usize) {
    debug_assert_eq!(addr, GUEST_REGS_ADDR);
}

pub fn set_jit_asm_addr(addr: usize) {
    debug_assert_eq!(addr, JIT_ASM_ADDR);
}

pub fn set_mmu_tcm_addr(addr: usize) {
    debug_assert_eq!(addr, MMU_TCM_ADDR);
}
