use crate::core::emu::Emu;

use crate::get_jit_asm_ptr;
use crate::jit::assembler::{GuestInstMetadata, GUEST_REG_POOL_SIZE};
use crate::jit::inst_branch_handler::breakout_imm;
use crate::jit::reg::{Reg, RegReserve};
use crate::jit::MemoryAmount;
use crate::logging::debug_println;
use bilge::prelude::*;
use handler::*;
#[cfg(target_arch = "arm")]
use std::arch::naked_asm;
use std::hint::{assert_unchecked, unreachable_unchecked};
use std::intrinsics::{likely, unlikely};
use std::{mem, ptr};

mod handler {
    use crate::core::emu::Emu;
        use crate::jit::assembler::GuestInstMetadata;
    use crate::jit::reg::{Reg, RegReserve};
    use crate::jit::MemoryAmount;
    use crate::logging::debug_println;
    use std::hint::{assert_unchecked, unreachable_unchecked};
    use std::intrinsics::{likely, unlikely};
    use std::mem::MaybeUninit;
    use std::{mem, slice};

    pub fn handle_request_write<const AMOUNT: MemoryAmount>(value0: u32, value1: u32, addr: u32, emu: &mut Emu) {
        match AMOUNT {
            MemoryAmount::Byte => emu.mem_write::<_>(addr, value0 as u8),
            MemoryAmount::Half => emu.mem_write::<_>(addr, value0 as u16),
            MemoryAmount::Word => emu.mem_write::<_>(addr, value0),
            MemoryAmount::Double => {
                emu.mem_write::<_>(addr, value0);
                emu.mem_write::<_>(addr + 4, value1);
            }
        }
    }

    pub fn handle_request_read<const AMOUNT: MemoryAmount, const SIGNED: bool>(addr: u32, emu: &mut Emu) -> u32 {
        match AMOUNT {
            MemoryAmount::Byte => {
                if SIGNED {
                    emu.mem_read_with_options::<true, u8>(addr) as i8 as i32 as u32
                } else {
                    emu.mem_read_with_options::<true, u8>(addr) as u32
                }
            }
            MemoryAmount::Half => {
                if SIGNED {
                    // ARM7 quirk: misaligned LDRSH loads a sign-extended byte
                    if addr & 1 != 0 {
                        emu.mem_read_with_options::<true, u8>(addr) as i8 as i32 as u32
                    } else {
                        emu.mem_read_with_options::<true, u16>(addr) as i16 as i32 as u32
                    }
                } else {
                    // ARM7 quirk: misaligned LDRH rotates the aligned halfword
                    (emu.mem_read_with_options::<true, u16>(addr) as u32).rotate_right((addr & 1) << 3)
                }
            }
            MemoryAmount::Word => {
                if SIGNED {
                    unsafe { unreachable_unchecked() };
                }
                let value = emu.mem_read_with_options::<true, u32>(addr);
                let shift = (addr & 0x3) << 3;
                value.rotate_right(shift)
            }
            MemoryAmount::Double => unsafe { unreachable_unchecked() },
        }
    }

    pub fn handle_request_read64(addr: u32, emu: &mut Emu) -> (u32, u32) {
        let mut values: [u32; 2] = unsafe { MaybeUninit::uninit().assume_init() };
        emu.mem_read_multiple_slice::<true, true, u32>(addr, &mut values);
        (values[0], values[1])
    }

    fn get_reg_usr_mut<const FIQ_MODE: bool>(emu: &mut Emu, reg: Reg) -> &mut u32 {
        if FIQ_MODE || reg == Reg::SP || reg == Reg::LR {
            emu.thread_get_reg_usr_mut(reg)
        } else {
            emu.thread_get_reg_mut(reg)
        }
    }

    #[inline(always)]
    pub fn handle_multiple_request<
        const WRITE: bool,
        const WRITE_BACK: bool,
        const DECREMENT: bool,
        const VALID: bool,
        const USER: bool,
        const NEEDS_PC: bool,
        const GX_FIFO: bool,
    >(
        rlist: RegReserve,
        rlist_len: usize,
        op0_reg: Reg,
        pre: bool,
        emu: &mut Emu,
        metadata: &GuestInstMetadata,
    ) {
        let is_thumb = metadata.pc & 1 == 1;
        let pc = metadata.pc & !1;

        let op0 = emu.thread_get_reg_mut(op0_reg);
        debug_println!("handle multiple request at {pc:x} addr {op0:x} thumb: {is_thumb} write: {WRITE} rlist: {rlist:?}");
        debug_assert_ne!(rlist_len, 0);

        let start_addr = if DECREMENT { op0.wrapping_sub((rlist_len as u32) << 2) } else { *op0 };
        let addr = start_addr;

        if WRITE_BACK && (!WRITE || VALID || unlikely((rlist.0 & ((1 << (op0_reg as u8 + 1)) - 1)) > (1 << op0_reg as u8))) {
            if DECREMENT {
                *op0 = addr;
            } else {
                *op0 = addr.wrapping_add((rlist_len as u32) << 2);
            }
        }

        let mem_addr = addr.wrapping_add((pre as u32) << 2);

        let get_reg_mut = if USER && !emu.thread_is_user_mode() {
            if unlikely(emu.thread_is_fiq_mode()) {
                get_reg_usr_mut::<true>
            } else {
                get_reg_usr_mut::<false>
            }
        } else {
            Emu::thread_get_reg_mut
        };

        let mut values: [u32; Reg::CPSR as usize] = unsafe { MaybeUninit::uninit().assume_init() };
        unsafe { assert_unchecked(rlist_len <= values.len()) };
        if WRITE {
            let mut rlist = rlist.0.reverse_bits();
            for i in 0..if NEEDS_PC { rlist_len - 1 } else { rlist_len } {
                let zeros = rlist.leading_zeros();
                let reg = Reg::from(zeros as u8);
                rlist &= !(0x80000000 >> zeros);
                unsafe { *values.get_unchecked_mut(i) = *get_reg_mut(emu, reg) };
            }
            if NEEDS_PC {
                let pc_offset = 4 << (!is_thumb as u8);
                unsafe { *values.get_unchecked_mut(rlist_len - 1) = pc + pc_offset };
            }

            let slice = unsafe { slice::from_raw_parts(values.as_ptr(), rlist_len) };
            emu.mem_write_multiple_slice::<true, _>(mem_addr, slice);
        } else {
            let mut slice = &mut values;
            let aligned_addr = mem_addr & !0x3;
            let aligned_addr = aligned_addr & 0x0FFFFFFF;
            let shm_offset = emu.get_shm_offset::<true, false>(aligned_addr);
            if unlikely(shm_offset != 0) {
                slice = unsafe { mem::transmute(emu.mem.shm.as_ptr().add(shm_offset)) };
            } else {
                emu.mem_read_multiple_slice::<true, false, _>(aligned_addr, &mut slice[..rlist_len]);
            }
            let mut rlist = rlist.0.reverse_bits();
            for i in 0..rlist_len {
                let zeros = rlist.leading_zeros();
                let reg = Reg::from(zeros as u8);
                rlist &= !(0x80000000 >> zeros);
                unsafe { *get_reg_mut(emu, reg) = *slice.get_unchecked(i) };
            }
        }

        if WRITE_BACK && (WRITE || VALID) {
            *emu.thread_get_reg_mut(op0_reg) = if DECREMENT { start_addr } else { addr.wrapping_add((rlist_len << 2) as u32) };
        }
    }
}

unsafe extern "C" fn breakout_after_write(metadata: *const GuestInstMetadata, host_regs: &[usize; GUEST_REG_POOL_SIZE]) {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    debug_println!("breakout after write");

    let metadata = metadata.as_ref_unchecked();

    for dirty_guest_reg in metadata.dirty_guest_regs - Reg::CPSR {
        let mapped_reg = *metadata.mapped_guest_regs.get_unchecked(dirty_guest_reg as usize);
        let value = *host_regs.get_unchecked(crate::jit::assembler::host_pool_index(mapped_reg)) as u32;
        debug_println!("save {dirty_guest_reg:?} as {value:x} from host {mapped_reg:?}");
        *asm.emu.thread_get_reg_mut(dirty_guest_reg) = value;
    }
    breakout_imm(asm, metadata.total_cycle_count, metadata.pc);
}

pub unsafe extern "C" fn _inst_write_mem_handler<const AMOUNT: MemoryAmount>(value0: u32, value1: u32, addr: u32, metadata: *const GuestInstMetadata) -> *const GuestInstMetadata {
    let metadata = metadata.as_ref_unchecked();
    debug_println!("handle write request addr {addr:x} at {:x}", metadata.pc);

    let asm = get_jit_asm_ptr().as_mut_unchecked();
    handle_request_write::<AMOUNT>(value0, value1, addr, asm.emu);
    if unlikely(asm.emu.breakout_imm) {
        metadata
    } else {
        ptr::null()
    }
}

pub unsafe extern "C" fn _inst_write_io_mem_handler<const AMOUNT: MemoryAmount>(
    value0: u32,
    value1: u32,
    addr: u32,
    metadata_ptr: *const GuestInstMetadata,
) -> *const GuestInstMetadata {
    if AMOUNT == MemoryAmount::Double {
        unreachable_unchecked();
    }

    let asm = get_jit_asm_ptr().as_mut_unchecked();
    let metadata = metadata_ptr.as_ref_unchecked();

    debug_println!("handle write io request addr {addr:x} at {:x}", metadata.pc);

    if likely(addr == metadata.s.slow.initial_patch_addr) {
        let func: fn(&mut Emu, u32) = mem::transmute(metadata.s.slow.io_func);
        func(asm.emu, value0);
        ptr::null()
    } else {
        handle_request_write::<AMOUNT>(value0, value1, addr, asm.emu);
        if unlikely(asm.emu.breakout_imm) {
            metadata_ptr
        } else {
            ptr::null()
        }
    }
}

macro_rules! write_mem_handler_cpsr {
    ($name:ident, $inst_fun:ident) => {
        #[cfg(target_arch = "arm")]
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name<const AMOUNT: MemoryAmount>(_value0: u32, _value1: u32, _addr: u32) {
            #[rustfmt::skip]
            naked_asm!(
                "push {{r3, lr}}",
                "mrs lr, cpsr",
                "lsrs lr, lr, 24",
                "strb lr, [r3, {cpsr_bits}]",
                "mov r3, r12",
                "bl {handler}",
                "cbnz r0, 1f",
                "2:",
                "pop {{r3, lr}}",
                "ldr r2, [r3, {cpsr}]",
                "msr cpsr_f, r2",
                "bx lr",
                "1:",
                "push {{r4-r11}}",
                "mov r1, sp",
                "bl {breakout}",
                "pop {{r4-r11}}",
                "b 2b",
                cpsr_bits = const Reg::CPSR as usize * 4 + 3,
                handler = sym $inst_fun::<AMOUNT>,
                cpsr = const Reg::CPSR as usize * 4,
                breakout = sym breakout_after_write,
            );
        }
    };
}

macro_rules! write_mem_handler {
    ($name:ident, $inst_fun:ident) => {
        #[cfg(target_arch = "arm")]
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name<const AMOUNT: MemoryAmount>(_value0: u32, _value1: u32, _addr: u32, _metadata: *const GuestInstMetadata) {
            #[rustfmt::skip]
            naked_asm!(
                "push {{r3, lr}}",
                "mov r3, r12",
                "bl {}",
                "cbnz r0, 1f",
                "2:",
                "pop {{r3, pc}}",
                "1:",
                "push {{r4-r11}}",
                "mov r1, sp",
                "bl {}",
                "pop {{r4-r11}}",
                "b 2b",
                sym $inst_fun::<AMOUNT>,
                sym breakout_after_write,
            );
        }
    };
}

write_mem_handler_cpsr!(inst_write_mem_handler_with_cpsr, _inst_write_mem_handler);
write_mem_handler!(inst_write_mem_handler, _inst_write_mem_handler);
write_mem_handler_cpsr!(inst_write_io_mem_handler_with_cpsr, _inst_write_io_mem_handler);
write_mem_handler!(inst_write_io_mem_handler, _inst_write_io_mem_handler);

pub unsafe extern "C" fn _inst_read_mem_handler<const AMOUNT: MemoryAmount, const SIGNED: bool>(_: u8, _: u32, addr: u32) -> u32 {
    if AMOUNT == MemoryAmount::Double || (AMOUNT == MemoryAmount::Word && SIGNED) {
        unreachable_unchecked();
    }

    debug_println!("handle read request addr {addr:x}");

    let asm = get_jit_asm_ptr();
    handle_request_read::<AMOUNT, SIGNED>(addr, (*asm).emu)
}

pub unsafe extern "C" fn _inst_read64_mem_handler(_: u8, _: u32, addr: u32) -> u64 {
    debug_println!("handle read64 request addr {addr:x}");

    let asm = get_jit_asm_ptr();
    let (value0, value1) = handle_request_read64(addr, (*asm).emu);
    (value0 as u64) | ((value1 as u64) << 32)
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_mem_handler<const AMOUNT: MemoryAmount, const SIGNED: bool>(_: u8, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "bl {}",
        "pop {{r3, pc}}",
        sym _inst_read_mem_handler::<AMOUNT, SIGNED>,
    );
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_mem_handler_with_cpsr<const AMOUNT: MemoryAmount, const SIGNED: bool>(_: u8, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "mrs lr, cpsr",
        "lsrs lr, lr, 24",
        "strb lr, [r3, {cpsr_bits}]",
        "bl {handler}",
        "pop {{r3, lr}}",
        "ldr r2, [r3, {cpsr}]",
        "msr cpsr_f, r2",
        "bx lr",
        cpsr_bits = const Reg::CPSR as usize * 4 + 3,
        handler = sym _inst_read_mem_handler::<AMOUNT, SIGNED>,
        cpsr = const Reg::CPSR as usize * 4,
    );
}

pub unsafe extern "C" fn _inst_read_io_mem_handler<const AMOUNT: MemoryAmount, const SIGNED: bool>(metadata: *const GuestInstMetadata, _: u32, addr: u32) -> u32 {
    if AMOUNT == MemoryAmount::Double || (AMOUNT == MemoryAmount::Word && SIGNED) {
        unreachable_unchecked();
    }

    debug_println!("handle read request addr {addr:x}");

    let asm = get_jit_asm_ptr().as_mut_unchecked();
    let metadata = metadata.as_ref_unchecked();

    if likely(addr == metadata.s.slow.initial_patch_addr) {
        let func: fn(&mut Emu) -> u32 = mem::transmute(metadata.s.slow.io_func);
        let value = func(asm.emu);
        // The io table fns return u8/u16/u32 behind a transmute; AAPCS leaves the upper
        // bits of a narrow return UNSPECIFIED, so mask to the access width (a leaked
        // u16 intermediate broke timer-polling loops). Signed halves/bytes re-extend.
        match AMOUNT {
            MemoryAmount::Byte => {
                if SIGNED {
                    value as u8 as i8 as i32 as u32
                } else {
                    value as u8 as u32
                }
            }
            MemoryAmount::Half => {
                if SIGNED {
                    value as u16 as i16 as i32 as u32
                } else {
                    value as u16 as u32
                }
            }
            _ => value,
        }
    } else {
        handle_request_read::<AMOUNT, SIGNED>(addr, asm.emu)
    }
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_io_mem_handler<const AMOUNT: MemoryAmount, const SIGNED: bool>(_: *const GuestInstMetadata, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "mov r3, lr",
        "bl {}",
        "pop {{r3, pc}}",
        sym _inst_read_io_mem_handler::<AMOUNT, SIGNED>,
    );
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_io_mem_handler_with_cpsr<const AMOUNT: MemoryAmount, const SIGNED: bool>(_: *const GuestInstMetadata, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "mrs r12, cpsr",
        "lsrs r12, r12, 24",
        "strb r12, [r3, {cpsr_bits}]",
        "mov r3, lr",
        "bl {handler}",
        "pop {{r3, lr}}",
        "ldr r2, [r3, {cpsr}]",
        "msr cpsr_f, r2",
        "bx lr",
        cpsr_bits = const Reg::CPSR as usize * 4 + 3,
        handler = sym _inst_read_io_mem_handler::<AMOUNT, SIGNED>,
        cpsr = const Reg::CPSR as usize * 4,
    );
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read64_mem_handler(_: u8, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "bl {}",
        "pop {{r3, pc}}",
        sym _inst_read64_mem_handler,
    );
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read64_mem_handler_with_cpsr(_: u8, _: u32, _: u32) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3, lr}}",
        "mrs lr, cpsr",
        "lsrs lr, lr, 24",
        "strb lr, [r3, {cpsr_bits}]",
        "bl {handler}",
        "pop {{r3, lr}}",
        "ldr r2, [r3, {cpsr}]",
        "msr cpsr_f, r2",
        "bx lr",
        cpsr_bits = const Reg::CPSR as usize * 4 + 3,
        handler = sym _inst_read64_mem_handler,
        cpsr = const Reg::CPSR as usize * 4,
    );
}

#[bitsize(32)]
#[derive(FromBits)]
pub struct InstMemMultipleParams {
    pub rlist: u16,
    pub rlist_len: u4,
    pub op0: u4,
    pub pre: bool,
    pub user: bool,
    unused: u6,
}

pub unsafe extern "C" fn _inst_mem_handler_multiple<
    const WRITE: bool,
    const WRITE_BACK: bool,
    const DECREMENT: bool,
    const VALID: bool,
    const USER: bool,
    const NEEDS_PC: bool,
    const GX_FIFO: bool,
>(
    params: u32,
    metadata: *const GuestInstMetadata,
    host_regs: &mut [usize; GUEST_REG_POOL_SIZE],
) {
    if (!WRITE_BACK && !VALID) || (!WRITE && NEEDS_PC) || (!WRITE && GX_FIFO) || (USER && NEEDS_PC) {
        unreachable_unchecked()
    }

    let asm = get_jit_asm_ptr().as_mut_unchecked();
    let metadata = metadata.as_ref_unchecked();
    let params = InstMemMultipleParams::from(params);
    let op0_reg = Reg::from(u8::from(params.op0()));
    let rlist = RegReserve::from(params.rlist() as u32);
    let rlist_len = u8::from(params.rlist_len()) as usize;

    if WRITE {
        for dirty_guest_reg in metadata.dirty_guest_regs - Reg::CPSR {
            let mapped_reg = *metadata.mapped_guest_regs.get_unchecked(dirty_guest_reg as usize);
            if mapped_reg != crate::jit::assembler::HOST_REG_NONE {
                let value = *host_regs.get_unchecked(crate::jit::assembler::host_pool_index(mapped_reg)) as u32;
                *asm.emu.thread_get_reg_mut(dirty_guest_reg) = value;
            }
        }
    } else {
        let op0 = Reg::from(u8::from(params.op0()));
        if metadata.dirty_guest_regs.is_reserved(op0) {
            let mapped_reg = *metadata.mapped_guest_regs.get_unchecked(op0 as usize);
            if mapped_reg != crate::jit::assembler::HOST_REG_NONE {
                let value = *host_regs.get_unchecked(crate::jit::assembler::host_pool_index(mapped_reg)) as u32;
                *asm.emu.thread_get_reg_mut(op0) = value;
            }
        }
    }

    handle_multiple_request::<WRITE, WRITE_BACK, DECREMENT, VALID, USER, NEEDS_PC, GX_FIFO>(rlist, rlist_len, op0_reg, params.pre(), asm.emu, metadata);

    if WRITE && unlikely(asm.emu.breakout_imm) {
        breakout_imm(asm, metadata.total_cycle_count, metadata.pc);
    }

    if WRITE_BACK {
        let mapped_reg = *metadata.mapped_guest_regs.get_unchecked(op0_reg as usize);
        if mapped_reg != crate::jit::assembler::HOST_REG_NONE {
            *host_regs.get_unchecked_mut(crate::jit::assembler::host_pool_index(mapped_reg)) = *asm.emu.thread_get_reg(op0_reg) as usize;
        }
    }

    if !WRITE {
        let mut rlist = rlist.0.reverse_bits();
        for _ in 0..rlist_len {
            let zeros = rlist.leading_zeros();
            let reg = Reg::from(zeros as u8);
            rlist &= !(0x80000000 >> zeros);
            let mapped_reg = *metadata.mapped_guest_regs.get_unchecked(reg as usize);
            if mapped_reg != crate::jit::assembler::HOST_REG_NONE {
                *host_regs.get_unchecked_mut(crate::jit::assembler::host_pool_index(mapped_reg)) = *asm.emu.thread_get_reg(reg) as usize;
            }
        }
    }
}

macro_rules! write_mem_handler_multiple_cpsr {
    ($name:ident, $inst_func:ident, $gx_fifo:expr) => {
        #[cfg(target_arch = "arm")]
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name<const WRITE_BACK: bool, const DECREMENT: bool, const VALID: bool, const USER: bool, const NEEDS_PC: bool>(
            _: u32,
            _: *const GuestInstMetadata,
        ) {
            #[rustfmt::skip]
            naked_asm!(
                "push {{r3-r11,lr}}",
                "mrs r2, cpsr",
                "lsrs r2, r2, 24",
                "strb r2, [r3, {cpsr_bits}]",
                "add r2, sp, 4",
                "bl {handler}",
                "pop {{r3-r11,lr}}",
                "ldr r2, [r3, {cpsr}]",
                "msr cpsr_f, r2",
                "bx lr",
                cpsr_bits = const Reg::CPSR as usize * 4 + 3,
                handler = sym $inst_func::<true, WRITE_BACK, DECREMENT, VALID, USER, NEEDS_PC, $gx_fifo>,
                cpsr = const Reg::CPSR as usize * 4,
            );
        }
    };
}

macro_rules! write_mem_handler_multiple {
    ($name:ident, $inst_func:ident, $gx_fifo:expr) => {
        #[cfg(target_arch = "arm")]
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name<const WRITE_BACK: bool, const DECREMENT: bool, const VALID: bool, const USER: bool, const NEEDS_PC: bool>(
            _: u32,
            _: *const GuestInstMetadata,
        ) {
            #[rustfmt::skip]
            naked_asm!(
                "push {{r3-r11,lr}}",
                "add r2, sp, 4",
                "bl {handler}",
                "pop {{r3-r11,pc}}",
                handler = sym $inst_func::<true, WRITE_BACK, DECREMENT, VALID, USER, NEEDS_PC, $gx_fifo>,
            );
        }
    };
}

write_mem_handler_multiple_cpsr!(inst_write_mem_handler_multiple_with_cpsr, _inst_mem_handler_multiple, false);
write_mem_handler_multiple!(inst_write_mem_handler_multiple, _inst_mem_handler_multiple, false);

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_mem_handler_multiple_with_cpsr<const WRITE_BACK: bool, const DECREMENT: bool, const VALID: bool, const USER: bool, const NEEDS_PC: bool>(
    _: u32,
    _: *const GuestInstMetadata,
) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3-r11,lr}}",
        "mrs r2, cpsr",
        "lsrs r2, r2, 24",
        "strb r2, [r3, {cpsr_bits}]",
        "add r2, sp, 4",
        "bl {handler}",
        "pop {{r3-r11,lr}}",
        "ldr r2, [r3, {cpsr}]",
        "msr cpsr_f, r2",
        "bx lr",
        cpsr_bits = const Reg::CPSR as usize * 4 + 3,
        handler = sym _inst_mem_handler_multiple::<false, WRITE_BACK, DECREMENT, VALID, USER, NEEDS_PC, false>,
        cpsr = const Reg::CPSR as usize * 4,
    );
}

#[cfg(target_arch = "arm")]
#[unsafe(naked)]
pub unsafe extern "C" fn inst_read_mem_handler_multiple<const WRITE_BACK: bool, const DECREMENT: bool, const VALID: bool, const USER: bool, const NEEDS_PC: bool>(
    _: u32,
    _: *const GuestInstMetadata,
) {
    #[rustfmt::skip]
    naked_asm!(
        "push {{r3-r11,lr}}",
        "add r2, sp, 4",
        "bl {handler}",
        "pop {{r3-r11,pc}}",
        handler = sym _inst_mem_handler_multiple::<false, WRITE_BACK, DECREMENT, VALID, USER, NEEDS_PC, false>,
    );
}

