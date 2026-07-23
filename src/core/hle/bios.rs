use crate::core::thread_regs;
use crate::core::cpu_regs::InterruptFlag;
use crate::core::emu::Emu;
use crate::core::hle::bios_lookup_table::GBA_SWI_LOOKUP_TABLE;
use crate::core::memory::regions;
use crate::core::thread_regs::Cpsr;
use crate::jit::reg::Reg;
use crate::logging::debug_println;
use bilge::prelude::*;
use std::cmp::min;
use std::intrinsics::likely;
use std::mem;

pub fn swi(comment: u8, emu: &mut Emu) {
    let (name, func) = GBA_SWI_LOOKUP_TABLE[min(comment as usize, GBA_SWI_LOOKUP_TABLE.len() - 1)];
    debug_println!("swi call {:x} {}", comment, name);
    emu.cpu.bios_open_bus = 0xE3A02004;
    func(emu)
}

pub fn interrupt(emu: &mut Emu) {
    debug_println!("interrupt");

    emu.cpu.bios_open_bus = 0xE25EF004;

    let regs = thread_regs();
    let mut cpsr = Cpsr::from(regs.cpsr);

    cpsr.set_irq_disable(true);
    cpsr.set_thumb(false);
    cpsr.set_mode(u5::new(0x12));
    emu.thread_set_cpsr(u32::from(cpsr), true);

    let regs = thread_regs();
    let is_thumb = (regs.pc & 1) == 1;
    let mut spsr = Cpsr::from(regs.spsr);
    spsr.set_thumb(is_thumb);
    regs.spsr = u32::from(spsr);

    let regs_to_push = [regs.gp_regs[0], regs.gp_regs[1], regs.gp_regs[2], regs.gp_regs[3], regs.gp_regs[12], regs.pc + 4];
    regs.sp -= regs_to_push.len() as u32 * 4;
    let sp = regs.sp;
    emu.mem_write_multiple_slice::<true, _>(sp, &regs_to_push);

    // User irq handler pointer at 0x03007FFC (0x3FFFFFC = iwram top mirror);
    // the magic lr routes the handler's return into hle_bios_uninterrupt
    thread_regs().lr = 0xFFF00000;
    thread_regs().pc = emu.mem_read::<_>(0x3FFFFFC);
}

pub fn uninterrupt(emu: &mut Emu) {
    debug_println!("uninterrupt");

    emu.cpu.bios_open_bus = 0xE55EC002;

    if emu.cpu.bios_wait_flags != 0 {
        check_wait_flags(emu);
    }

    let mut reg_values = &mut [0u32; 6];
    let regs = thread_regs();
    let sp = regs.sp;
    let aligned_addr = sp & !0x3;
    let aligned_addr = aligned_addr & 0x0FFFFFFF;
    let shm_offset = emu.get_shm_offset::<true, false>(aligned_addr);
    if likely(shm_offset != 0) {
        reg_values = unsafe { mem::transmute(emu.mem.shm.as_ptr().add(shm_offset)) };
    } else {
        emu.mem_read_multiple_slice::<true, false, _>(thread_regs().sp, reg_values);
    }
    regs.gp_regs[0] = reg_values[0];
    regs.gp_regs[1] = reg_values[1];
    regs.gp_regs[2] = reg_values[2];
    regs.gp_regs[3] = reg_values[3];
    regs.gp_regs[12] = reg_values[4];
    regs.sp += reg_values.len() as u32 * 4;
    regs.lr = reg_values[5];
    regs.pc = regs.lr - 4;

    let spsr = regs.spsr;
    regs.pc = (regs.pc & !1) | Cpsr::from(spsr).thumb() as u32;
    emu.thread_set_cpsr(spsr, false);
}

pub fn bit_unpack(emu: &mut Emu) {
    todo!()
}

pub fn cpu_fast_set(emu: &mut Emu) {
    let (src, dest, length_mode) = { (*emu.thread_get_reg(Reg::R0), *emu.thread_get_reg(Reg::R1), *emu.thread_get_reg(Reg::R2)) };

    let fixed = length_mode & (1 << 24) != 0;
    let size = (length_mode & 0x1FFFFF) << 2;

    for i in (0..size).step_by(4) {
        let addr = if fixed { src } else { src + i };
        let value = emu.mem_read::<u32>(addr);
        emu.mem_write::<u32>(dest + i, value);
    }
}

pub fn cpu_set(emu: &mut Emu) {
    let src_addr = *emu.thread_get_reg(Reg::R0);
    let dst_addr = *emu.thread_get_reg(Reg::R1);
    let len_mode = *emu.thread_get_reg(Reg::R2);

    let count = len_mode & 0x1FFFFF;
    let fill = (len_mode & (1 << 24)) != 0;
    let is_32_bit = (len_mode & (1 << 26)) != 0;

    if is_32_bit {
        for i in 0..count {
            let addr = src_addr + if fill { 0 } else { i << 2 };
            let value = emu.mem_read::<u32>(addr);
            emu.mem_write::<_>(dst_addr + (i << 2), value);
        }
    } else {
        for i in 0..count {
            let addr = src_addr + if fill { 0 } else { i << 1 };
            let value = emu.mem_read::<u16>(addr);
            emu.mem_write::<_>(dst_addr + (i << 1), value);
        }
    }
}

pub fn diff_unfilt16(emu: &mut Emu) {
    todo!()
}

pub fn diff_unfilt8(emu: &mut Emu) {
    todo!()
}

pub fn divide(emu: &mut Emu) {
    let dividend = *emu.thread_get_reg(Reg::R0) as i32;
    let divisor = *emu.thread_get_reg(Reg::R1) as i32;
    let quotient = dividend / divisor;
    *emu.thread_get_reg_mut(Reg::R0) = quotient as u32;
    *emu.thread_get_reg_mut(Reg::R1) = (dividend % divisor) as u32;
    *emu.thread_get_reg_mut(Reg::R3) = quotient.unsigned_abs();
}

pub fn halt(emu: &mut Emu) {
    emu.cpu_halt(0);
}

pub fn huff_uncomp(emu: &mut Emu) {
    todo!()
}

pub fn check_wait_flags(emu: &mut Emu) {
    // BIOS IntrWait flags word at 0x03007FF8 (iwram top mirror)
    let addr = 0x3FFFFF8;
    let flags = emu.mem_read::<u32>(addr);
    let wait_flags = emu.cpu.bios_wait_flags;

    if flags & wait_flags != 0 {
        emu.mem_write::<_>(addr, flags & !wait_flags);
        emu.cpu.bios_wait_flags = 0;
    } else {
        emu.cpu_halt(0);
    }
}

pub fn interrupt_wait(emu: &mut Emu) {
    let (discard_old, wait_flags) = { (*emu.thread_get_reg(Reg::R0) != 0, *emu.thread_get_reg(Reg::R1)) };
    emu.cpu.bios_wait_flags = wait_flags;

    if discard_old {
        check_wait_flags(emu);
        emu.cpu.bios_wait_flags = wait_flags;
        emu.cpu_halt(0);
    } else {
        check_wait_flags(emu);
    }
}

// Emit one decompressed byte via a 16-bit read-modify-write. The Vram decompression
// SWIs (LZ77UnCompVram 0x12, RLUnCompVram 0x15) must write 16-bit units: VRAM/palette
// turn 8-bit stores into "write the byte to both halves of the halfword", so byte-wise
// output would corrupt every entry (0x3000 tilemap -> 0x3030). A 16-bit rmw keeps the
// destination byte-exact and current, so LZ77 back-references still read correct data.
// Works for the Wram variants too (identical byte output, just via halfword stores).
fn uncomp_write_byte(emu: &mut Emu, addr: u32, value: u8) {
    let aligned = addr & !1;
    let cur = emu.mem_read::<u16>(aligned);
    let shift = (addr & 1) * 8;
    let new = (cur & !(0xFFu16 << shift)) | ((value as u16) << shift);
    emu.mem_write::<u16>(aligned, new);
}

pub fn lz77_uncomp(emu: &mut Emu) {
    let src_addr = *emu.thread_get_reg(Reg::R0);
    let dst_addr = *emu.thread_get_reg(Reg::R1);

    let size = emu.mem_read::<u32>(src_addr) >> 8;
    let mut src = 4;
    let mut dst = 0;

    loop {
        let mut flags = emu.mem_read::<u8>(src_addr + src) as u16;
        src += 1;
        for _ in 0..8 {
            if dst >= size {
                return;
            }

            flags <<= 1;
            if flags & (1 << 8) != 0 {
                let val1 = emu.mem_read::<u8>(src_addr + src);
                src += 1;
                let val2 = emu.mem_read::<u8>(src_addr + src);
                src += 1;
                let size = 3 + ((val1 >> 4) & 0xF);
                let offset = 1 + ((val1 as u32 & 0xF) << 8) + val2 as u32;

                for _ in 0..size {
                    let value = emu.mem_read::<u8>(dst_addr + dst - offset);
                    uncomp_write_byte(emu, dst_addr + dst, value);
                    dst += 1;
                }
            } else {
                let value = emu.mem_read::<u8>(src_addr + src);
                src += 1;
                uncomp_write_byte(emu, dst_addr + dst, value);
                dst += 1;
            }
        }
    }
}

pub fn runlen_uncomp(emu: &mut Emu) {
    let src_addr = *emu.thread_get_reg(Reg::R0);
    let dst_addr = *emu.thread_get_reg(Reg::R1);
    let size = emu.mem_read::<u32>(src_addr) >> 8;

    let mut src = 4;
    let mut dst = 0;

    while dst < size {
        let flags = emu.mem_read::<u8>(src_addr + src);
        src += 1;

        if flags & (1 << 7) != 0 {
            let value = emu.mem_read::<u8>(src_addr + src);
            src += 1;
            let length = (flags & 0x7F) + 3;
            for i in 0..length {
                uncomp_write_byte(emu, dst_addr + dst + i as u32, value);
            }
            dst += length as u32;
        } else {
            let length = (flags & 0x7F) + 1;
            for i in 0..length {
                let value = emu.mem_read::<u8>(src_addr + src + i as u32);
                uncomp_write_byte(emu, dst_addr + dst + i as u32, value);
            }
            src += length as u32;
            dst += length as u32;
        }
    }
}

pub fn square_root(emu: &mut Emu) {
    let reg0 = emu.thread_get_reg_mut(Reg::R0);
    *reg0 = reg0.isqrt();
}

pub fn unknown(_: &mut Emu) {}

pub fn v_blank_intr_wait(emu: &mut Emu) {
    {
        *emu.thread_get_reg_mut(Reg::R0) = 1;
        *emu.thread_get_reg_mut(Reg::R1) = 1 << InterruptFlag::LcdVBlank as u8;
    }
    interrupt_wait(emu);
}

pub fn sleep(emu: &mut Emu) {
    emu.cpu_set_halt_cnt(0xC0);
}

pub fn sound_bias(emu: &mut Emu) {
    let bias_level = if *emu.thread_get_reg(Reg::R0) != 0 { 0x200u16 } else { 0u16 };
    emu.mem_write::<_>(0x4000088, bias_level);
}

pub fn soft_reset(emu: &mut Emu) {
    // Clear the bios work area at the top of iwram
    emu.mem_write_multiple_memset::<true, u8>(0x3007E00, 0, 0x200);

    let return_flag = emu.mem_read::<u8>(0x3007FFA);
    let entry = if return_flag == 0 { 0x08000000 } else { 0x02000000 };

    let regs = thread_regs();
    regs.user.sp = 0x03007F00;
    regs.irq.sp = 0x03007FA0;
    regs.svc.sp = 0x03007FE0;
    regs.irq.lr = 0;
    regs.irq.spsr = 0;
    regs.svc.lr = 0;
    regs.svc.spsr = 0;
    regs.gp_regs.fill(0);
    regs.user.lr = entry;
    regs.pc = entry;
    emu.thread_set_cpsr(0x000000DF, false);
}

pub fn register_ram_reset(emu: &mut Emu) {
    // The bios always drops into forced blank, regardless of the flags (GBATEK).
    // FireRed-engine games depend on it: their io-reg shadow system only writes
    // DISPSTAT through immediately while the screen is blanked.
    emu.mem_write::<u16>(0x4000000, 0x0080);

    let flags = *emu.thread_get_reg(Reg::R0);
    if flags & 0x01 != 0 {
        emu.mem_write_multiple_memset::<true, u32>(regions::EWRAM_OFFSET, 0, (regions::EWRAM_SIZE >> 2) as usize);
    }
    if flags & 0x02 != 0 {
        // IWRAM except the last 0x200 bytes (stacks/bios area)
        emu.mem_write_multiple_memset::<true, u32>(regions::IWRAM_OFFSET, 0, ((regions::IWRAM_SIZE - 0x200) >> 2) as usize);
    }
    if flags & 0x04 != 0 {
        emu.mem_write_multiple_memset::<true, u32>(regions::PALETTES_OFFSET, 0, (regions::PALETTES_SIZE >> 2) as usize);
    }
    if flags & 0x08 != 0 {
        emu.mem_write_multiple_memset::<true, u32>(regions::VRAM_OFFSET, 0, (regions::VRAM_SIZE >> 2) as usize);
    }
    if flags & 0x10 != 0 {
        emu.mem_write_multiple_memset::<true, u32>(regions::OAM_OFFSET, 0, (regions::OAM_SIZE >> 2) as usize);
    }
    // TODO: flag bits 5-7 (sio/sound/other io registers)
}

pub fn divide_arm(emu: &mut Emu) {
    let regs = thread_regs();
    regs.gp_regs.swap(0, 1);
    divide(emu);
}

// HLE-only accuracy: f64 trig instead of the bios' fixed-point series. Swap for the
// exact algorithms if test roms demand bit-exactness.
pub fn arc_tan(emu: &mut Emu) {
    let reg0 = emu.thread_get_reg_mut(Reg::R0);
    let tan = (*reg0 as i16 as f64) / 16384.0;
    let angle = tan.atan() / (2.0 * std::f64::consts::PI) * 65536.0;
    *reg0 = (angle as i32 as u32) & 0xFFFF;
}

pub fn arc_tan2(emu: &mut Emu) {
    let x = *emu.thread_get_reg(Reg::R0) as i16 as f64;
    let y = *emu.thread_get_reg(Reg::R1) as i16 as f64;
    let angle = y.atan2(x) / (2.0 * std::f64::consts::PI) * 65536.0;
    *emu.thread_get_reg_mut(Reg::R0) = (angle as i32 as u32) & 0xFFFF;
}

pub fn bios_checksum(emu: &mut Emu) {
    *emu.thread_get_reg_mut(Reg::R0) = 0xBAAE187F;
}

// TODO: port exact implementations from NooDS hle_bios.cpp
pub fn bg_affine_set(emu: &mut Emu) {
    let mut src = *emu.thread_get_reg(Reg::R0);
    let mut dst = *emu.thread_get_reg(Reg::R1);
    let count = *emu.thread_get_reg(Reg::R2);

    for _ in 0..count {
        let orig_x = emu.mem_read::<u32>(src) as i32 as f64 / 256.0;
        let orig_y = emu.mem_read::<u32>(src + 4) as i32 as f64 / 256.0;
        let disp_x = emu.mem_read::<u16>(src + 8) as i16 as f64;
        let disp_y = emu.mem_read::<u16>(src + 10) as i16 as f64;
        let scale_x = emu.mem_read::<u16>(src + 12) as i16 as f64 / 256.0;
        let scale_y = emu.mem_read::<u16>(src + 14) as i16 as f64 / 256.0;
        let theta = (emu.mem_read::<u16>(src + 16) >> 8) as f64 / 128.0 * std::f64::consts::PI;
        src += 20;

        let (sin, cos) = theta.sin_cos();
        let pa = scale_x * cos;
        let pb = -scale_x * sin;
        let pc = scale_y * sin;
        let pd = scale_y * cos;

        emu.mem_write::<u16>(dst, ((pa * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + 2, ((pb * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + 4, ((pc * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + 6, ((pd * 256.0) as i32 as u32 & 0xFFFF) as u16);
        let ref_x = orig_x - pa * disp_x - pb * disp_y;
        let ref_y = orig_y - pc * disp_x - pd * disp_y;
        emu.mem_write::<u32>(dst + 8, (ref_x * 256.0) as i32 as u32);
        emu.mem_write::<u32>(dst + 12, (ref_y * 256.0) as i32 as u32);
        dst += 16;
    }
}

pub fn obj_affine_set(emu: &mut Emu) {
    let mut src = *emu.thread_get_reg(Reg::R0);
    let mut dst = *emu.thread_get_reg(Reg::R1);
    let count = *emu.thread_get_reg(Reg::R2);
    let stride = *emu.thread_get_reg(Reg::R3);

    for _ in 0..count {
        let scale_x = emu.mem_read::<u16>(src) as i16 as f64 / 256.0;
        let scale_y = emu.mem_read::<u16>(src + 2) as i16 as f64 / 256.0;
        let theta = (emu.mem_read::<u16>(src + 4) >> 8) as f64 / 128.0 * std::f64::consts::PI;
        src += 8;

        let (sin, cos) = theta.sin_cos();
        emu.mem_write::<u16>(dst, ((scale_x * cos * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + stride, ((-scale_x * sin * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + stride * 2, ((scale_y * sin * 256.0) as i32 as u32 & 0xFFFF) as u16);
        emu.mem_write::<u16>(dst + stride * 3, ((scale_y * cos * 256.0) as i32 as u32 & 0xFFFF) as u16);
        dst += stride * 4;
    }
}

pub fn midi_key_2_freq(emu: &mut Emu) {
    let wave_data = *emu.thread_get_reg(Reg::R0);
    let key = *emu.thread_get_reg(Reg::R1) as f64;
    let fine = *emu.thread_get_reg(Reg::R2) as f64;
    let freq = emu.mem_read::<u32>(wave_data + 4) as f64;
    let result = freq / 2f64.powf((180.0 - key - fine / 256.0) / 12.0);
    *emu.thread_get_reg_mut(Reg::R0) = result as u32;
}

