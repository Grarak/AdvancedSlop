use crate::core::MAX_LOOP_CYCLE_COUNT;
use crate::core::thread_regs;
use crate::jit::jit_asm::{align_guest_pc, JitAsm};
use crate::jit::jit_asm_common_funs::{exit_guest_context, JitAsmCommonFuns};
use crate::logging::debug_println;
use crate::{get_jit_asm_ptr, BRANCH_LOG, IS_DEBUG};
use std::hint::assert_unchecked;
use std::intrinsics::{likely, unlikely};
use std::mem;

fn flush_cycles(asm: &mut JitAsm, total_cycles: u16, current_pc: u32) {
    let mut cycles = asm.runtime_data.accumulated_cycles as u32;
    cycles += total_cycles as u32 + 2 - asm.runtime_data.pre_cycle_count_sum as u32;
    unsafe { assert_unchecked(cycles <= u16::MAX as u32) };
    asm.runtime_data.accumulated_cycles = cycles as u16;
    // The flush consumes the pre-charge; leaving the field set poisons flows that regain
    // control without rewriting it (a compiled callee returning into the interpreter's
    // hand_to_next_entry call: its bx-lr flushed here, nobody re-set the field, and the
    // interpreter's own next branch flushes with total_cycles 0 — sub overflow).
    asm.runtime_data.pre_cycle_count_sum = 0;
    debug_println!("flush cycles {} at {current_pc:x}", asm.runtime_data.accumulated_cycles);
}

// The quantum is up: exit the guest context; the main loop runs the scheduler and
// re-enters at regs.pc.
#[cold]
#[inline(never)]
fn exe_scheduler(asm: &mut JitAsm, current_pc: u32) {
    debug_println!("exit guest flush cycles");
    if IS_DEBUG {
        asm.runtime_data.set_branch_out_pc(current_pc);
    }
    unsafe { exit_guest_context!(asm) };
}

#[cold]
pub unsafe extern "C" fn exe_scheduler_external(current_pc: u32) {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    exe_scheduler(asm, current_pc);
}

#[inline(always)]
pub fn check_scheduler(asm: &mut JitAsm, current_pc: u32) {
    if unlikely(asm.runtime_data.accumulated_cycles >= MAX_LOOP_CYCLE_COUNT as u16) {
        exe_scheduler(asm, current_pc);
    }
}

#[inline(always)]
pub unsafe fn call_jit_fun(asm: &mut JitAsm, target_pc: u32) {
    (*(&raw mut crate::logging::DISPATCH_RING)).push(target_pc, 0, 0);
    let thumb = target_pc & 1 == 1;
    let target_pc = align_guest_pc(target_pc);
    asm.emu.thread_set_thumb(thumb);

    let jit_entry = asm.emu.jit.get_jit_start_addr(target_pc);
    let jit_entry: extern "C" fn(u32) = mem::transmute(jit_entry);
    jit_entry(target_pc | (thumb as u32));
}

#[inline(always)]
pub extern "C" fn pre_branch<const HAS_LR_RETURN: bool>(asm: &mut JitAsm, total_cycles: u16, lr: u32, current_pc: u32) {
    flush_cycles(asm, total_cycles, current_pc);

    check_scheduler(asm, current_pc);

    asm.runtime_data.pre_cycle_count_sum = 0;
    if HAS_LR_RETURN {
        asm.runtime_data.push_return_stack(lr);
        if BRANCH_LOG {
            JitAsmCommonFuns::debug_push_return_stack(current_pc, lr, asm.runtime_data.get_return_stack_ptr());
        }
    }
}

pub unsafe extern "C" fn branch_reg<const HAS_LR_RETURN: bool>(total_cycles: u16, target_pc: u32, lr: u32, current_pc: u32) {
    let asm = get_jit_asm_ptr().as_mut_unchecked();

    pre_branch::<HAS_LR_RETURN>(asm, total_cycles, lr, current_pc);

    if BRANCH_LOG {
        JitAsmCommonFuns::debug_branch_reg(current_pc, target_pc);
    }

    call_jit_fun(asm, target_pc);
    if HAS_LR_RETURN {
        asm.runtime_data.pre_cycle_count_sum = total_cycles;
    }
}

// The return-stack miss: leave the guest context (execution resumes from regs.pc).
// Outlined — the match path above is the one every guest return executes.
#[cold]
#[inline(never)]
unsafe fn branch_lr_mismatch(asm: &mut JitAsm, target_pc: u32, desired_lr: u32, current_pc: u32) -> ! {
    if BRANCH_LOG {
        JitAsmCommonFuns::debug_branch_lr_failed(current_pc, target_pc, desired_lr);
    }
    let _ = (target_pc, desired_lr);
    exit_guest_context!(asm);
}

pub unsafe extern "C" fn branch_lr(total_cycles: u16, target_pc: u32, current_pc: u32) {
    let asm = get_jit_asm_ptr().as_mut_unchecked();

    flush_cycles(asm, total_cycles, current_pc);
    check_scheduler(asm, current_pc);

    if IS_DEBUG {
        asm.runtime_data.set_branch_out_pc(current_pc);
    }

    let desired_lr = asm.runtime_data.pop_return_stack();
    if likely(desired_lr == target_pc) {
        asm.emu.thread_set_thumb(target_pc & 1 == 1);
        if BRANCH_LOG {
            JitAsmCommonFuns::debug_branch_lr(current_pc, target_pc);
        }
    } else {
        branch_lr_mismatch(asm, target_pc, desired_lr, current_pc);
    }
}

pub unsafe fn breakout_imm(asm: &mut JitAsm, total_cycles: u16, current_pc: u32) {
    asm.runtime_data.accumulated_cycles += total_cycles - asm.runtime_data.pre_cycle_count_sum;
    let is_thumb = current_pc & 1 == 1;
    let pc = current_pc & !1;
    if IS_DEBUG {
        asm.runtime_data.set_branch_out_pc(pc);
    }
    let next_pc_offset = (1 << (!is_thumb as u8)) + 2;
    thread_regs().pc = pc + next_pc_offset;
    (*(&raw mut crate::logging::BREAKOUT_RING)).push(current_pc, pc + next_pc_offset, thread_regs().cpsr);
    asm.emu.breakout_imm = false;
    asm.runtime_data.pre_cycle_count_sum = 0;

    exit_guest_context!(asm);
}
