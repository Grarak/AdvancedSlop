use crate::core::thread_regs;
use crate::core::cycle_manager::ImmEventType;
use crate::core::emu::Emu;
use crate::core::exception_handler::ExceptionVector;
use crate::core::thread_regs::Cpsr;
use crate::core::exception_handler;
use crate::jit::jit_asm::JitAsm;
use crate::logging::debug_println;
use crate::savestate::Savestate;
use std::fmt::{Debug, Formatter};
use std::mem;


#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum InterruptFlag {
    LcdVBlank = 0,
    LcdHBlank = 1,
    LcdVCounterMatch = 2,
    Timer0Overflow = 3,
    Timer1Overflow = 4,
    Timer2Overflow = 5,
    Timer3Overflow = 6,
    Rtc = 7,
    Dma0 = 8,
    Dma1 = 9,
    Dma2 = 10,
    Dma3 = 11,
    Keypad = 12,
    GbaSlot = 13,
}

impl From<u8> for InterruptFlag {
    fn from(value: u8) -> Self {
        debug_assert!(value <= InterruptFlag::GbaSlot as u8);
        unsafe { mem::transmute(value) }
    }
}

pub struct InterruptFlags(pub u32);

impl Debug for InterruptFlags {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug_set = f.debug_set();
        for i in 0..16 {
            if self.0 & (1 << i) != 0 {
                // Bits 14/15 are reserved and have no enum variant, but guests can still set them.
                if matches!(i, 14 | 15) {
                    debug_set.entry(&format_args!("Reserved{i}"));
                } else {
                    debug_set.entry(&InterruptFlag::from(i));
                }
            }
        }
        debug_set.finish()
    }
}

#[repr(C)]
#[derive(Savestate)]
pub struct CpuRegs {
    pub post_flg: u8,
    pub halt_cnt: u8,
    halt: u8,
    pub bios_wait_flags: u32,
    // WAITCNT (0x4000204): stored and readable, not modeled in timing (NooDS parity)
    pub wait_cnt: u16,
    // BIOS-region reads return the last bios fetch; with the HLE bios these are the
    // four documented constants (after boot / after swi / during irq / after irq)
    pub bios_open_bus: u32,
}

impl CpuRegs {
    pub fn new() -> Self {
        CpuRegs {
            post_flg: 0,
            halt_cnt: 0,
            halt: 0,
            bios_wait_flags: 0,
            wait_cnt: 0,
            bios_open_bus: 0xE129F000,
        }
    }
}

impl Emu {
    pub fn cpu_set_ime(&mut self, value: u8) {
        thread_regs().ime = value & 0x1;
        self.cpu_check_for_interrupt();
    }

    pub fn cpu_set_ie(&mut self, mut mask: u32, value: u32) {
        // GBA: 14 interrupt sources
        mask &= 0x3FFF;
        let regs = thread_regs();
        regs.ie = (regs.ie & !mask) | (value & mask);
        debug_println!("set ie {:x} {:?}", regs.ie, InterruptFlags(regs.ie));
        self.cpu_check_for_interrupt();
    }

    pub fn cpu_check_for_interrupt(&mut self) {
        let regs = thread_regs();
        if regs.ime != 0 && (regs.ie & regs.irf) != 0 && !Cpsr::from(regs.cpsr).irq_disable() {
            self.cpu_schedule_interrupt();
            // Make sure to run the interrupt as soon as possible
            let asm = unsafe { (crate::core::JIT_ASM_ADDR as *mut JitAsm).as_mut_unchecked() };
            if asm.runtime_data.accumulated_cycles < crate::core::MAX_BRANCH_LOOP_CYCLE_COUNT as u16 {
                asm.runtime_data.accumulated_cycles = crate::core::MAX_BRANCH_LOOP_CYCLE_COUNT as u16;
            }
        }
    }

    fn cpu_schedule_interrupt(&mut self) {
        self.cm.schedule_imm(ImmEventType::cpu_interrupt());
    }

    pub fn cpu_set_irf(&mut self, mask: u32, value: u32) {
        debug_println!("set irf {:?}", InterruptFlags(value & mask));
        let regs = thread_regs();
        regs.irf &= !(value & mask);
    }

    pub fn cpu_set_post_flg(&mut self, value: u8) {
        let cpu_regs = &mut self.cpu;
        cpu_regs.post_flg |= value & 0x1;
    }

    pub fn cpu_halt(&mut self, bit: u8) {
        debug_println!("halt with bit {bit}");
        self.cpu.halt |= 1 << bit;
    }

    pub fn cpu_unhalt(&mut self, bit: u8) {
        debug_println!("unhalt with bit {bit}");
        self.cpu.halt &= !(1 << bit);
    }

    pub fn cpu_is_halted(&self) -> bool {
        self.cpu.halt != 0
    }

    #[inline(never)]
    pub fn cpu_send_interrupt(&mut self, flag: InterruptFlag) {
        let regs = thread_regs();
        regs.irf |= 1 << flag as u8;
        debug_println!(
            "send interrupt {flag:?} {:?} {:?} {:x} {}",
            InterruptFlags(regs.ie),
            InterruptFlags(regs.irf),
            regs.ime,
            !Cpsr::from(regs.cpsr).irq_disable()
        );
        if (regs.ie & regs.irf) != 0 {
            if regs.ime != 0 && !Cpsr::from(regs.cpsr).irq_disable() {
                debug_println!("schedule send interrupt {flag:?}");
                // Prompt delivery (quantum saturation): without it the guest can see the
                // triggering flag, poll out and disable IME before the imm event runs
                // (hardware takes the irq within cycles). No HLE-IPC counter-case here.
                self.cpu_check_for_interrupt();
            } else {
                debug_println!("unhalt send interrupt {flag:?}");
                self.cpu_unhalt(0);
            }
        }
    }

    pub fn cpu_set_halt_cnt(&mut self, value: u8) {
        self.cpu.halt_cnt = value & 0xC0;
        // GBA HALTCNT: any write halts until an enabled interrupt arrives; bit 7 (stop)
        // is treated as a plain halt (NooDS parity)
        self.cpu_halt(0);
    }

    pub fn cpu_on_interrupt_event(&mut self) {
        let regs = thread_regs();
        let interrupted = {
            let interrupt = regs.ime != 0 && (regs.ie & regs.irf) != 0 && !Cpsr::from(regs.cpsr).irq_disable();
            if interrupt {
                debug_println!("interrupt {:?}", InterruptFlags(regs.ie & regs.irf));
            } else {
                debug_println!(
                    "can't interrupt {:x} {:?} {}",
                    regs.ime,
                    InterruptFlags(regs.ie & regs.irf),
                    !Cpsr::from(regs.cpsr).irq_disable()
                );
            }
            interrupt
        };
        if interrupted {
            if regs.ie & regs.irf & 1 != 0 {
                crate::debug_inst_log::count_vblank_delivery();
            }
            unsafe { (*(&raw mut crate::logging::IRQ_RING)).push(regs.pc, regs.cpsr, regs.ie & regs.irf) };
            exception_handler::handle(self, 0, ExceptionVector::NormalInterrupt);
            self.cpu_unhalt(0);
        }
    }
}
