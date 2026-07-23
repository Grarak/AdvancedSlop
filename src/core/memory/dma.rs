use crate::core::cpu_regs::InterruptFlag;
use crate::core::cycle_manager::ImmEventType;
use crate::core::emu::Emu;
use crate::logging::debug_println;
use crate::savestate::Savestate;
use crate::utils;
use bilge::prelude::*;
use std::cmp::min;
use std::hint::assert_unchecked;
use std::{mem, slice};

const CHANNEL_COUNT: usize = 4;

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DmaAddrCtrl {
    Increment = 0,
    Decrement = 1,
    Fixed = 2,
    IncrementReload = 3,
}

impl From<u8> for DmaAddrCtrl {
    fn from(value: u8) -> Self {
        debug_assert!(value <= DmaAddrCtrl::IncrementReload as u8);
        unsafe { mem::transmute(value) }
    }
}

#[bitsize(32)]
#[derive(FromBits)]
struct DmaCnt {
    word_count: u21,
    dest_addr_ctrl: u2,
    src_addr_ctrl: u2,
    repeat: bool,
    transfer_type: bool,
    transfer_mode: u3,
    irq_at_end: bool,
    enable: bool,
}

#[bitsize(32)]
#[derive(FromBits)]
struct DmaCntArm7 {
    word_count: u16,
    not_used: u5,
    dest_addr_ctrl: u2,
    src_addr_ctrl: u2,
    repeat: bool,
    transfer_type: bool,
    not_used1: u1,
    transfer_mode: u2,
    irq_at_end: bool,
    enable: u1,
}

// GBA timing bits 28-29: immediate / vblank / hblank / special (special = sound FIFO
// on ch1/2, video capture on ch3)
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DmaTransferMode {
    StartImm = 0,
    StartAtVBlank = 1,
    StartAtHBlank = 2,
    Special = 3,
}

impl DmaTransferMode {
    fn from_cnt(cnt: u32, _: usize) -> Self {
        DmaTransferMode::from(u8::from(DmaCntArm7::from(cnt).transfer_mode()))
    }
}

impl From<u8> for DmaTransferMode {
    fn from(value: u8) -> Self {
        debug_assert!(value <= DmaTransferMode::Special as u8);
        unsafe { mem::transmute(value) }
    }
}

// Word count 0 means max (0x4000 on ch0-2, 0x10000 on ch3)
fn dma_word_count(cnt: u32, channel_num: usize) -> u32 {
    let count = u32::from(DmaCntArm7::from(cnt).word_count());
    let max = if channel_num == 3 { 0x10000 } else { 0x4000 };
    if count == 0 {
        max
    } else {
        count
    }
}

#[derive(Copy, Clone, Default, Savestate)]
struct DmaChannel {
    cnt: u32,
    sad: u32,
    dad: u32,
    current_src: u32,
    current_dest: u32,
    current_count: u32,
}

#[derive(Savestate)]
pub struct Dma {
    channels: [DmaChannel; CHANNEL_COUNT],
    // Scratch refilled inside a single transfer, never live across events
    #[savestate(skip)]
    src_buf: Vec<u8>,
}

impl Dma {
    pub fn new() -> Self {
        Dma {
            channels: [DmaChannel::default(); CHANNEL_COUNT],
            src_buf: Vec::new(),
        }
    }
}

impl Emu {
    pub fn dma_get_cnt(&self, channel_num: usize) -> u32 {
        self.dma.channels[channel_num].cnt
    }

    pub fn dma_set_sad(&mut self, channel_num: usize, mut mask: u32, value: u32) {
        mask &= if channel_num != 0 { 0x0FFFFFFF } else { 0x07FFFFFF };
        self.dma.channels[channel_num].sad = (self.dma.channels[channel_num].sad & !mask) | (value & mask);
    }

    pub fn dma_set_dad(&mut self, channel_num: usize, mut mask: u32, value: u32) {
        mask &= if channel_num != 0 { 0x0FFFFFFF } else { 0x07FFFFFF };
        self.dma.channels[channel_num].dad = (self.dma.channels[channel_num].dad & !mask) | (value & mask);
    }

    pub fn dma_set_cnt(&mut self, channel_num: usize, mut mask: u32, value: u32) {
        let dma = &mut self.dma;

        let channel = &mut dma.channels[channel_num];
        let was_enabled = DmaCnt::from(channel.cnt).enable();

        mask &= if channel_num == 3 { 0xF7E0FFFF } else { 0xF7E03FFF };

        channel.cnt = (channel.cnt & !mask) | value & mask;

        let transfer_type = DmaTransferMode::from_cnt(channel.cnt, channel_num);

        let dma_cnt = DmaCnt::from(channel.cnt);
        if !was_enabled && dma_cnt.enable() {
            channel.current_src = channel.sad;
            channel.current_dest = channel.dad;
            channel.current_count = dma_word_count(channel.cnt, channel_num);

            if transfer_type == DmaTransferMode::StartImm {
                debug_println!(
                    "dma schedule imm {:x} {:x} {:x} {:x}",
                    channel.cnt,
                    channel.current_dest,
                    channel.current_src,
                    channel.current_count
                );

                self.breakout_imm = true;

                self.cm.schedule_imm(ImmEventType::dma(channel_num as u8));
            }
        }
    }

    #[inline(never)]
    pub fn dma_trigger_all(&mut self, mode: DmaTransferMode) {
        self.dma_trigger(mode, 0xF);
    }

    pub fn dma_trigger(&mut self, mode: DmaTransferMode, channels: u8) {
        for (index, channel) in self.dma.channels.iter().enumerate() {
            if channels & (1 << index) != 0 && DmaCnt::from(channel.cnt).enable() && DmaTransferMode::from_cnt(channel.cnt, index) == mode {
                debug_println!(
                    "dma trigger {:?} {:x} {:x} {:x} {:x}",
                    mode,
                    channel.cnt,
                    channel.current_dest,
                    channel.current_src,
                    channel.current_count
                );
                self.cm.schedule_imm(ImmEventType::dma(index as u8));
            }
        }
    }

    pub fn dma_trigger_imm(&mut self, mode: DmaTransferMode, channels: u8) {
        for i in 0..CHANNEL_COUNT {
            let channel = &self.dma.channels[i];
            if channels & (1 << i) != 0 && DmaCnt::from(channel.cnt).enable() && DmaTransferMode::from_cnt(channel.cnt, i) == mode {
                self.dma_on_event(i as u16);
            }
        }
    }

    pub fn dma_is_scheduled(&self, mode: DmaTransferMode, channels: u8) -> bool {
        for (index, channel) in self.dma.channels.iter().enumerate() {
            if channels & (1 << index) != 0 && DmaCnt::from(channel.cnt).enable() && DmaTransferMode::from_cnt(channel.cnt, index) == mode {
                return true;
            }
        }
        false
    }

    fn dma_do_transfer<T: utils::Convert>(&mut self, dest_addr: &mut u32, src_addr: &mut u32, count: u32, cnt: &DmaCnt, mode: DmaTransferMode) {
        let dest_addr_ctrl = DmaAddrCtrl::from(u8::from(cnt.dest_addr_ctrl()));
        let src_addr_ctrl = DmaAddrCtrl::from(u8::from(cnt.src_addr_ctrl()));

        let step_size = size_of::<T>() as u32;
        debug_println!("dma transfer {mode:?} from {src_addr:x} {src_addr_ctrl:?} to {dest_addr:x} {dest_addr_ctrl:?} with size {count}");

        let dma = &mut self.dma;
        let total_size = count << (step_size >> 1);
        if dma.src_buf.len() < total_size as usize {
            dma.src_buf.reserve(total_size as usize - dma.src_buf.len());
            unsafe { dma.src_buf.set_len(total_size as usize) };
        }

        match (src_addr_ctrl, dest_addr_ctrl) {
            (DmaAddrCtrl::Increment, DmaAddrCtrl::Fixed) => {
                let mut slice = unsafe { slice::from_raw_parts_mut(dma.src_buf.as_mut_ptr() as *mut T, count as usize) };
                let aligned_addr = *src_addr & !(size_of::<T>() as u32 - 1);
                let aligned_addr = aligned_addr & 0x0FFFFFFF;
                let shm_offset = self.get_shm_offset::<false, false>(aligned_addr);
                if shm_offset != 0 {
                    slice = unsafe { slice::from_raw_parts_mut(self.mem.shm.as_ptr().add(shm_offset) as *mut T, count as usize) };
                } else {
                    self.mem_read_multiple_slice::<false, false, T>(aligned_addr, slice);
                }
                self.mem_write_fixed_slice::<false, T>(*dest_addr, slice);
                *src_addr += total_size;
            }
            (DmaAddrCtrl::Increment, DmaAddrCtrl::Increment | DmaAddrCtrl::IncrementReload) => {
                let mut slice = unsafe { slice::from_raw_parts_mut(dma.src_buf.as_mut_ptr() as *mut T, count as usize) };
                let aligned_addr = *src_addr & !(size_of::<T>() as u32 - 1);
                let aligned_addr = aligned_addr & 0x0FFFFFFF;
                let shm_offset = self.get_shm_offset::<false, false>(aligned_addr);
                if shm_offset != 0 {
                    slice = unsafe { slice::from_raw_parts_mut(self.mem.shm.as_ptr().add(shm_offset) as *mut T, count as usize) };
                } else {
                    self.mem_read_multiple_slice::<false, false, T>(aligned_addr, slice);
                }
                self.mem_write_multiple_slice::<false, T>(*dest_addr, slice);
                *src_addr += total_size;
                *dest_addr += total_size;
            }
            (DmaAddrCtrl::Fixed, DmaAddrCtrl::Increment | DmaAddrCtrl::IncrementReload) => {
                let slice = unsafe { slice::from_raw_parts_mut(dma.src_buf.as_mut_ptr() as *mut T, count as usize) };
                self.mem_read_fixed_slice::<false, T>(*src_addr, slice);
                self.mem_write_multiple_slice::<false, T>(*dest_addr, slice);
                *dest_addr += total_size;
            }
            _ => {
                for _ in 0..count {
                    let src = self.mem_read_no_tcm::<T>(*src_addr);
                    self.mem_write_no_tcm::<T>(*dest_addr, src);

                    match src_addr_ctrl {
                        DmaAddrCtrl::Increment => *src_addr += step_size,
                        DmaAddrCtrl::Decrement => *src_addr -= step_size,
                        _ => {}
                    }

                    match dest_addr_ctrl {
                        DmaAddrCtrl::Increment | DmaAddrCtrl::IncrementReload => *dest_addr += step_size,
                        DmaAddrCtrl::Decrement => *dest_addr -= step_size,
                        DmaAddrCtrl::Fixed => {}
                    }
                }
            }
        }
    }

    pub fn dma_on_event0(&mut self) {
        self.dma_on_event(0);
    }

    pub fn dma_on_event1(&mut self) {
        self.dma_on_event(1);
    }

    pub fn dma_on_event2(&mut self) {
        self.dma_on_event(2);
    }

    pub fn dma_on_event3(&mut self) {
        self.dma_on_event(3);
    }

    fn dma_on_event(&mut self, channel_num: u16) {
        let channel_num = channel_num as usize;
        unsafe { assert_unchecked(channel_num < CHANNEL_COUNT) };

        let channel = &mut self.dma.channels[channel_num];

        let (cnt, mode, mut dest, mut src, count) = {
            (
                DmaCnt::from(channel.cnt),
                DmaTransferMode::from_cnt(channel.cnt, channel_num),
                channel.current_dest,
                channel.current_src,
                channel.current_count,
            )
        };

        // Sound DMA (channels 1/2 in special mode): always 4 words to the fixed FIFO
        // address, the word count and destination control are ignored (NooDS parity).
        if mode == DmaTransferMode::Special && (channel_num == 1 || channel_num == 2) {
            let mut values = [0u32; 4];
            for value in &mut values {
                *value = self.mem_read_no_tcm::<u32>(src);
                match DmaAddrCtrl::from(u8::from(cnt.src_addr_ctrl())) {
                    DmaAddrCtrl::Increment => src = src.wrapping_add(4),
                    DmaAddrCtrl::Decrement => src = src.wrapping_sub(4),
                    _ => {}
                }
            }
            self.mem_write_fixed_slice::<false, u32>(dest, &values);
        } else if cnt.transfer_type() {
            self.dma_do_transfer::<u32>(&mut dest, &mut src, count, &cnt, mode)
        } else {
            self.dma_do_transfer::<u16>(&mut dest, &mut src, count, &cnt, mode)
        };

        let channel = &mut self.dma.channels[channel_num];
        channel.current_dest = dest;
        channel.current_src = src;

        if cnt.repeat() && mode != DmaTransferMode::StartImm {
            channel.current_count = dma_word_count(channel.cnt, channel_num);
            if DmaAddrCtrl::from(u8::from(cnt.dest_addr_ctrl())) == DmaAddrCtrl::IncrementReload {
                channel.current_dest = channel.dad;
            }
        } else {
            channel.cnt &= !(1 << 31);
        }

        if cnt.irq_at_end() {
            self.cpu_send_interrupt(InterruptFlag::from(InterruptFlag::Dma0 as u8 + channel_num as u8));
        }
    }
}
