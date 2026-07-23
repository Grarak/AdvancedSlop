// Software interrupts and (no-op) coprocessor transfers. Semantics mirror the jit's
// runtime handlers (software_interrupt_handler): the hle bios executes the swi inline.
// A halt requests an immediate breakout through the same flag the io write handlers use.

use super::{Ctx, InstResult, T_BIT};
use crate::core::exception_handler::{self, ExceptionVector};

fn swi_common(ctx: &mut Ctx, comment: u8, step: u32) -> InstResult {
    let thumb = ctx.cpsr() & T_BIT != 0;
    let resume = (ctx.inst_addr + step) | thumb as u32;
    unsafe { (*ctx.regs).pc = resume };
    exception_handler::handle(ctx.asm.emu, comment, ExceptionVector::SoftwareInterrupt);
    if ctx.asm.emu.cpu_is_halted() {
        // Break out through the store-breakout path; breakout_imm re-derives the resume pc.
        ctx.asm.emu.breakout_imm = true;
        return InstResult::ContinueStore(3);
    }
    let pc = unsafe { (*ctx.regs).pc };
    if pc != resume {
        // The swi redirected execution (soft reset style); tag with the new mode.
        let thumb = ctx.cpsr() & T_BIT != 0;
        return InstResult::Branch(3, (pc & !1) | thumb as u32);
    }
    InstResult::Continue(3)
}

pub(super) fn swi(ctx: &mut Ctx, opcode: u32) -> InstResult {
    swi_common(ctx, (opcode >> 16) as u8, 4)
}

pub(super) fn swi_t(ctx: &mut Ctx, opcode: u16) -> InstResult {
    swi_common(ctx, opcode as u8, 2)
}

pub(super) fn mcr(ctx: &mut Ctx, opcode: u32) -> InstResult {
    // No coprocessors on the GBA: a no-op (like NooDS).
    let _ = (ctx, opcode);
    InstResult::Continue(1)
}

pub(super) fn mrc(ctx: &mut Ctx, opcode: u32) -> InstResult {
    // No coprocessors on the GBA: a no-op (like NooDS).
    let _ = (ctx, opcode);
    InstResult::Continue(1)
}
