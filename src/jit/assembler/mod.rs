// The arm32 assembler lives in its own directory; everything below the module
// declarations is backend-neutral (guest-file shapes, block metadata).
pub mod arm32;
// Compatibility re-exports: call sites keep addressing crate::jit::assembler::{arm, ...}.
pub use arm32::{arm, block_asm, reg_alloc, thumb, vixl};

use crate::jit::inst_info::Operands;
use crate::jit::op::Op;
use crate::jit::reg::{Reg, RegReserve};
use std::ptr;

// Host-register pool shape: 8 pool registers (r4-r11), guest regs id'd by the 16-entry
// guest file.
pub const GUEST_REGS_LENGTH: usize = 16;
pub const GUEST_REG_POOL_SIZE: usize = 8;

/// The host register type the backend allocates from — also the element type of the
/// shared metadata's mapped_guest_regs (slow-mem patching routes values between pool
/// registers and the handlers through it). Host registers are modeled with the same
/// vixl aarch32 `Reg` as the guest file.
pub type HostReg = Reg;
pub const HOST_REG_NONE: HostReg = Reg::None;

/// Index of a mapped host register within the pool dump the breakout/writeback paths
/// capture (r4-r11 pushed in order).
pub fn host_pool_index(reg: HostReg) -> usize {
    reg as usize - 4
}

#[derive(Copy, Clone)]
pub struct GuestInstMetadataFastMem {
    pub start_offset: u16,
    pub size: u16,
    pub op: Op,
    pub operands: Operands,
    pub op0: HostReg,
    pub opcode_offset: usize,
    pub is_os_irq_handler: bool,
}

impl GuestInstMetadataFastMem {
    fn new(start_offset: u16, size: u16, op: Op, operands: Operands, op0: HostReg, opcode_offset: usize, is_os_irq_handler: bool) -> Self {
        GuestInstMetadataFastMem {
            start_offset,
            size,
            op,
            operands,
            op0,
            opcode_offset,
            is_os_irq_handler,
        }
    }
}

#[derive(Copy, Clone)]
pub struct GuestInstMetadataSlowMem {
    pub initial_patch_addr: u32,
    pub io_func: *const (),
}

#[derive(Copy, Clone)]
pub union GuestInstMetadataShared {
    pub fast: GuestInstMetadataFastMem,
    pub slow: GuestInstMetadataSlowMem,
}

impl GuestInstMetadataShared {
    fn new(fast: GuestInstMetadataFastMem) -> Self {
        GuestInstMetadataShared { fast }
    }
}

#[derive(Clone)]
pub struct GuestInstMetadata {
    pub s: GuestInstMetadataShared,
    pub pc: u32,
    pub total_cycle_count: u16,
    pub dirty_guest_regs: RegReserve,
    pub mapped_guest_regs: [HostReg; GUEST_REGS_LENGTH],
}

impl GuestInstMetadata {
    pub fn new(
        fast_mem_start_offset: u16,
        fast_mem_size: u16,
        opcode_offset: usize,
        is_os_irq_handler: bool,
        pc: u32,
        total_cycle_count: u16,
        op: Op,
        operands: Operands,
        op0: HostReg,
        dirty_guest_regs: RegReserve,
        mapped_guest_regs: [HostReg; GUEST_REGS_LENGTH],
    ) -> Self {
        GuestInstMetadata {
            s: GuestInstMetadataShared::new(GuestInstMetadataFastMem::new(fast_mem_start_offset, fast_mem_size, op, operands, op0, opcode_offset, is_os_irq_handler)),
            pc,
            total_cycle_count,
            dirty_guest_regs,
            mapped_guest_regs,
        }
    }
}

#[repr(C)]
pub struct GuestInstOffset {
    pub offset: u16,
    pub pre_cycle_count_sum: u16,
    pub mapping: [*const u32; GUEST_REG_POOL_SIZE],
    pub pc: u32,
}

impl GuestInstOffset {
    fn new(offset: u16, pre_cycle_count_sum: u16, pc: u32) -> Self {
        GuestInstOffset {
            offset,
            mapping: [ptr::null(); GUEST_REG_POOL_SIZE],
            pre_cycle_count_sum,
            pc,
        }
    }
}
