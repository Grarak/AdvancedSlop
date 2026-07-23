use crate::core::thread_regs;
use crate::get_jit_asm_ptr;

pub unsafe extern "C" fn register_set_cpsr_checked(value: u32, flags: u8) -> u32 {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_set_cpsr_with_flags(value, flags);
    thread_regs().cpsr
}

pub unsafe extern "C" fn register_set_spsr_checked(value: u32, flags: u8) -> u32 {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_set_spsr_with_flags(value, flags);
    thread_regs().cpsr
}

// ARMv4 quirk: TST/TEQ/CMP/CMN with Rd=15 restore CPSR from SPSR mid-block; returns
// the new cpsr so the emitted `msr CPSR_f` keeps host flags coherent (msr shape)
pub unsafe extern "C" fn register_restore_spsr_checked() -> u32 {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_restore_spsr();
    thread_regs().cpsr
}

pub unsafe extern "C" fn register_restore_spsr() {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_restore_spsr();
}

pub unsafe extern "C" fn restore_thumb_after_restore_spsr() {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_restore_thumb_mode();
}

pub unsafe extern "C" fn set_pc_arm_mode() {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_force_pc_arm_mode()
}

pub unsafe extern "C" fn set_pc_thumb_mode() {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    asm.emu.thread_force_pc_thumb_mode()
}
