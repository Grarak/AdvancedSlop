use crate::core::exception_handler::ExceptionVector;
use crate::core::exception_handler;
use crate::get_jit_asm_ptr;
use crate::jit::inst_branch_handler::breakout_imm;

pub unsafe extern "C" fn software_interrupt_handler(opcode: u8, pc: u32, total_cycles: u16) {
    let asm = get_jit_asm_ptr().as_mut_unchecked();
    exception_handler::handle(asm.emu, opcode, ExceptionVector::SoftwareInterrupt);
    if asm.emu.cpu_is_halted() {
        breakout_imm(asm, total_cycles, pc);
    }
}
