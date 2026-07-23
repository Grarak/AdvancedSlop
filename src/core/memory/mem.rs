use crate::core::emu::Emu;
use crate::core::memory::mmu::{MmuArm7, MMU_PAGE_SHIFT, MMU_PAGE_SIZE};
use crate::core::memory::regions;
use crate::core::memory::regions::{OAM_SIZE, PALETTES_SIZE};

use crate::debug_inst_log::{self, MemLogKind};
use crate::mmap::Shm;
use crate::utils;
use crate::utils::Convert;
use std::intrinsics::unlikely;
use std::marker::PhantomData;
use std::mem;

impl crate::savestate::Savestate for Memory {
    fn savestate(&mut self, state: &mut crate::savestate::SavestateContext) {
        state.pod_slice(&mut self.shm[regions::SAVESTATE_SHM_RANGE]);
        // mmu tables are host mappings; Emu::savestate_post_load runs mmu_update_all
    }
}

pub struct Memory {
    pub shm: Shm,
    pub mmu_arm7: MmuArm7,
    // Renderer dirty bits (gpu_mem_buf::DIRTY_*), set by the vram/palette/oam write
    // handlers (all their stores take the slow path), consumed at the vblank snapshot
    pub gpu_mem_dirty: u8,
}

// Indexed by (addr >> 24) & 0xF — all 16 region nibbles must be covered. The mapped
// regions (ewram/iwram/rom) are normally served by the mmu fast path; these handlers
// are the slow path for everything else and for faults out of fastmem.
macro_rules! create_io_read_lut {
    () => {
        [
            Self::read_bios, // 0x0: bios open-bus constants (HLE)
            Self::read_open_bus, // 0x1
            Self::read_ewram,
            Self::read_iwram,
            Self::read_io_ports,
            Self::read_palettes,
            Self::read_vram,
            Self::read_oam,
            Self::read_rom, // 0x8
            Self::read_rom, // 0x9
            Self::read_rom, // 0xA
            Self::read_rom, // 0xB
            Self::read_rom, // 0xC
            Self::read_rom_eeprom, // 0xD
            Self::read_sram, // 0xE
            Self::read_sram, // 0xF: sram mirror
        ]
    };
}

macro_rules! create_io_write_lut {
    () => {
        [
            Self::write_none, // 0x0: bios
            Self::write_none, // 0x1
            Self::write_ewram,
            Self::write_iwram,
            Self::write_io_ports,
            Self::write_palettes,
            Self::write_vram,
            Self::write_oam,
            Self::write_rom, // 0x8: gpio window when rtc enabled, otherwise ignored
            Self::write_rom, // 0x9
            Self::write_rom, // 0xA
            Self::write_rom, // 0xB
            Self::write_rom, // 0xC
            Self::write_rom_eeprom, // 0xD
            Self::write_sram, // 0xE
            Self::write_sram, // 0xF
        ]
    };
}

// 96K vram: 64K + 32K, upper 32K mirrored once per 128K block, blocks repeat
pub fn vram_offset(addr: u32) -> u32 {
    let addr = addr & 0x1FFFF;
    if addr & 0x10000 != 0 {
        addr & 0x17FFF
    } else {
        addr
    }
}

// Out-of-bounds rom reads return the (addr/2) prefetch pattern
fn rom_oob_pattern(addr: u32) -> u32 {
    let lo = (addr >> 1) & 0xFFFF;
    let hi = ((addr + 2) >> 1) & 0xFFFF;
    lo | (hi << 16)
}

impl Emu {
    fn is_gpio_addr(&self, addr: u32) -> bool {
        self.cartridge.io.has_rtc && (0x080000C4..0x080000CA).contains(&addr)
    }

    // BG/OBJ vram byte-write boundary: byte writes to BG vram duplicate to the
    // halfword, byte writes to OBJ vram are ignored
    fn vram_obj_start(&self) -> u32 {
        if self.gpu.disp_cnt.bg_mode().value() >= 3 {
            0x14000
        } else {
            0x10000
        }
    }
}

struct MemoryIo<const TCM: bool, T: Convert> {
    _data: PhantomData<T>,
}

impl<const TCM: bool, T: Convert> MemoryIo<TCM, T> {
    const READ_LUT: [fn(u32, &mut Emu) -> T; 16] = create_io_read_lut!();
    const WRITE_LUT: [fn(u32, T, &mut Emu); 16] = create_io_write_lut!();

    fn read(addr: u32, emu: &mut Emu) -> T {
        unsafe { Self::READ_LUT.get_unchecked(((addr >> 24) & 0xF) as usize)(addr, emu) }
    }

    fn write(addr: u32, value: T, emu: &mut Emu) {
        let func = unsafe { Self::WRITE_LUT.get_unchecked(((addr >> 24) & 0xF) as usize) };
        func(addr, value, emu);
    }

    // TODO: open bus returns the last prefetched opcode; 0 until then
    fn read_open_bus(_: u32, _: &mut Emu) -> T {
        T::from(0)
    }

    // BIOS region (HLE): reads return the last bios fetch constant, shifted for the
    // access width/alignment like a real open-bus word
    fn read_bios(addr: u32, emu: &mut Emu) -> T {
        if addr < 0x4000 {
            let value = emu.cpu.bios_open_bus;
            <T as Convert>::from(value >> ((addr as usize & (4 - size_of::<T>())) << 3))
        } else {
            T::from(0)
        }
    }

    fn write_none(_: u32, _: T, _: &mut Emu) {}

    fn read_ewram(addr: u32, emu: &mut Emu) -> T {
        let shm_offset = regions::EWRAM_REGION.shm_offset as u32 + (addr & (regions::EWRAM_SIZE - 1));
        utils::read_from_mem(&emu.mem.shm, shm_offset)
    }

    fn write_ewram(addr: u32, value: T, emu: &mut Emu) {
        // EWRAM code is never write-protected (SMC is IWRAM-only), so this slow path is
        // only reached for the non-fastmem access shapes — a plain store, no jit
        // invalidation.
        let shm_offset = regions::EWRAM_REGION.shm_offset as u32 + (addr & (regions::EWRAM_SIZE - 1));
        utils::write_to_mem(&mut emu.mem.shm, shm_offset, value);
    }

    fn read_iwram(addr: u32, emu: &mut Emu) -> T {
        let shm_offset = regions::IWRAM_REGION.shm_offset as u32 + (addr & (regions::IWRAM_SIZE - 1));
        utils::read_from_mem(&emu.mem.shm, shm_offset)
    }

    fn write_iwram(addr: u32, value: T, emu: &mut Emu) {
        let shm_offset = regions::IWRAM_REGION.shm_offset as u32 + (addr & (regions::IWRAM_SIZE - 1));
        // This slow path only serves writes into write-protected (jit-holding) pages.
        // Rewriting identical bytes keeps compiled code valid — the m4a engine
        // re-patches its mixer loops with the same instructions most passes — so skip
        // the write and the invalidation entirely.
        let old: T = utils::read_from_mem(&emu.mem.shm, shm_offset);
        if old.into() == value.into() {
            return;
        }
        utils::write_to_mem(&mut emu.mem.shm, shm_offset, value);
        if unlikely(emu.jit.invalidate_block(addr, size_of::<T>())) {
            unsafe { (*(&raw mut crate::logging::INVALIDATE_RING)).push(addr, size_of::<T>() as u32, 0) };
            emu.breakout_imm = true;
            if emu.jit.is_sticky_smc(addr) {
                emu.jit.invalidate_mmu_page(addr);
                emu.mmu_restore_write(addr, &regions::IWRAM_REGION);
            }
        }
    }

    fn read_io_ports(addr: u32, emu: &mut Emu) -> T {
        emu.io_gba_read(addr & 0x00FFFFFF)
    }

    fn write_io_ports(addr: u32, value: T, emu: &mut Emu) {
        emu.io_gba_write(addr & 0x00FFFFFF, value);
    }

    fn read_palettes(addr: u32, emu: &mut Emu) -> T {
        let shm_offset = regions::PALETTES_REGION.shm_offset as u32 + (addr & (regions::PALETTES_SIZE - 1));
        utils::read_from_mem(&emu.mem.shm, shm_offset)
    }

    fn write_palettes(addr: u32, value: T, emu: &mut Emu) {
        emu.mem.gpu_mem_dirty |= crate::core::graphics::gpu_mem_buf::DIRTY_PAL;
        let offset = addr & (regions::PALETTES_SIZE - 1);
        if size_of::<T>() == 1 {
            // Byte writes duplicate to the halfword
            let value = value.into() as u8;
            let shm_offset = regions::PALETTES_REGION.shm_offset as u32 + (offset & !1);
            utils::write_to_mem(&mut emu.mem.shm, shm_offset, u16::from_le_bytes([value, value]));
        } else {
            let shm_offset = regions::PALETTES_REGION.shm_offset as u32 + offset;
            utils::write_to_mem(&mut emu.mem.shm, shm_offset, value);
        }
    }

    fn read_vram(addr: u32, emu: &mut Emu) -> T {
        let shm_offset = regions::VRAM_REGION.shm_offset as u32 + vram_offset(addr);
        utils::read_from_mem(&emu.mem.shm, shm_offset)
    }

    fn write_vram(addr: u32, value: T, emu: &mut Emu) {
        let offset = vram_offset(addr);
        emu.mem.gpu_mem_dirty |= if offset < crate::core::graphics::gpu_mem_buf::BG_VRAM_SIZE as u32 {
            crate::core::graphics::gpu_mem_buf::DIRTY_BG
        } else {
            crate::core::graphics::gpu_mem_buf::DIRTY_OBJ
        };
        if size_of::<T>() == 1 {
            if offset >= emu.vram_obj_start() {
                return; // Byte writes to OBJ vram are ignored
            }
            let value = value.into() as u8;
            let shm_offset = regions::VRAM_REGION.shm_offset as u32 + (offset & !1);
            utils::write_to_mem(&mut emu.mem.shm, shm_offset, u16::from_le_bytes([value, value]));
        } else {
            let shm_offset = regions::VRAM_REGION.shm_offset as u32 + offset;
            utils::write_to_mem(&mut emu.mem.shm, shm_offset, value);
        }
        // No jit invalidation: vram has no jit entries/live ranges (executing from
        // vram is unsupported for now — jit_insert_block would panic on a vram pc)
    }

    fn read_oam(addr: u32, emu: &mut Emu) -> T {
        let shm_offset = regions::OAM_REGION.shm_offset as u32 + (addr & (regions::OAM_SIZE - 1));
        utils::read_from_mem(&emu.mem.shm, shm_offset)
    }

    fn write_oam(addr: u32, value: T, emu: &mut Emu) {
        if size_of::<T>() == 1 {
            return; // Byte writes to OAM are ignored
        }
        emu.mem.gpu_mem_dirty |= crate::core::graphics::gpu_mem_buf::DIRTY_OAM;
        let shm_offset = regions::OAM_REGION.shm_offset as u32 + (addr & (regions::OAM_SIZE - 1));
        utils::write_to_mem(&mut emu.mem.shm, shm_offset, value);
    }

    fn read_rom(addr: u32, emu: &mut Emu) -> T {
        if emu.is_gpio_addr(addr) {
            return <T as Convert>::from(emu.cartridge_read_gpio(addr) as u32);
        }
        let offset = (addr & 0x01FFFFFF) & emu.cartridge.io.rom_mask;
        if offset < emu.cartridge.io.rom_len {
            let shm_offset = regions::ROM_REGION.shm_offset as u32 + offset;
            utils::read_from_mem(&emu.mem.shm, shm_offset)
        } else {
            <T as Convert>::from(rom_oob_pattern(addr))
        }
    }

    fn write_rom(addr: u32, value: T, emu: &mut Emu) {
        if (0x080000C4..0x080000CA).contains(&addr) {
            emu.cartridge_auto_enable_rtc();
            emu.cartridge_write_gpio(addr, value.into() as u16);
        }
    }

    fn read_rom_eeprom(addr: u32, emu: &mut Emu) -> T {
        if emu.cartridge_is_eeprom(addr) {
            <T as Convert>::from(emu.cartridge_read_eeprom() as u32)
        } else {
            Self::read_rom(addr, emu)
        }
    }

    fn write_rom_eeprom(addr: u32, value: T, emu: &mut Emu) {
        if emu.cartridge_is_eeprom(addr) {
            emu.cartridge_write_eeprom(value.into() as u16);
        }
    }

    // 8-bit bus: wide reads splat the byte, wide writes store the lane byte
    fn read_sram(addr: u32, emu: &mut Emu) -> T {
        let byte = emu.cartridge_read_sram(addr) as u32;
        <T as Convert>::from(byte * 0x01010101)
    }

    fn write_sram(addr: u32, value: T, emu: &mut Emu) {
        let rotated = (value.into() as u32) >> ((addr as usize & (size_of::<T>() - 1)) << 3);
        emu.cartridge_write_sram(addr, rotated as u8);
    }
}

struct MemoryMultipleSliceIo<const TCM: bool, T: Convert> {
    _data: PhantomData<T>,
}

impl<const TCM: bool, T: Convert> MemoryMultipleSliceIo<TCM, T> {
    fn read(addr: u32, slice: &mut [T], emu: &mut Emu) {
        let read_shift = size_of::<T>() >> 1;
        for i in 0..slice.len() {
            slice[i] = MemoryIo::<TCM, T>::read(addr + (i << read_shift) as u32, emu);
        }
    }

    fn write(addr: u32, slice: &[T], emu: &mut Emu) {
        let write_shift = size_of::<T>() >> 1;
        for i in 0..slice.len() {
            MemoryIo::<TCM, T>::write(addr + (i << write_shift) as u32, slice[i], emu);
        }
    }
}

struct MemoryFixedSliceIo<const TCM: bool, T: Convert> {
    _data: PhantomData<T>,
}

impl<const TCM: bool, T: Convert> MemoryFixedSliceIo<TCM, T> {
    fn read(addr: u32, slice: &mut [T], emu: &mut Emu) {
        slice.fill(MemoryIo::<TCM, T>::read(addr, emu));
    }

    fn write(addr: u32, slice: &[T], emu: &mut Emu) {
        // Fixed destination: every element lands on the same address
        if (addr >> 24) & 0xF == 4 {
            emu.io_gba_write_fixed_slice(addr & 0x00FFFFFF, slice);
        } else {
            for value in slice {
                MemoryIo::<TCM, T>::write(addr, *value, emu);
            }
        }
    }
}

struct MemoryMultipleMemsetIo<const TCM: bool, T: Convert> {
    _data: PhantomData<T>,
}

impl<const TCM: bool, T: Convert> MemoryMultipleMemsetIo<TCM, T> {
    fn write(addr: u32, value: T, size: usize, emu: &mut Emu) {
        let write_shift = size_of::<T>() >> 1;
        for i in 0..size {
            MemoryIo::<TCM, T>::write(addr + (i << write_shift) as u32, value, emu);
        }
    }
}

impl Memory {
    pub fn new() -> Self {
        Memory {
            shm: Shm::new("physical", regions::TOTAL_MEM_SIZE as usize).unwrap(),
            mmu_arm7: MmuArm7::new(),
            gpu_mem_dirty: 0xF,
        }
    }

    pub fn init(&mut self) {
        // The rom slot is reloaded by cartridge_load_rom_into_shm after a reset
        self.shm.fill(0);
        self.gpu_mem_dirty = 0xF;
    }
}

impl Emu {
    pub fn mem_get_palettes(&self) -> &'static [u8; PALETTES_SIZE as usize] {
        unsafe { mem::transmute(self.mem.shm.as_ptr().add(regions::PALETTES_REGION.shm_offset)) }
    }

    pub fn mem_get_oam(&self) -> &'static [u8; OAM_SIZE as usize] {
        unsafe { mem::transmute(self.mem.shm.as_ptr().add(regions::OAM_REGION.shm_offset)) }
    }

    pub fn get_shm_offset<const TCM: bool, const WRITE: bool>(&self, addr: u32) -> usize {
        let mmu = {
            if WRITE {
                self.mmu_get_write()
            } else {
                self.mmu_get_read()
            }
        };

        let shm_offset = unsafe { *mmu.get_unchecked((addr as usize) >> MMU_PAGE_SHIFT) };
        if shm_offset != 0 {
            let offset = (addr as usize) & (MMU_PAGE_SIZE - 1);
            shm_offset + offset
        } else {
            0
        }
    }

    pub fn mem_read<T: Convert>(&mut self, addr: u32) -> T {
        self.mem_read_with_options::<true, T>(addr)
    }

    pub fn mem_read_no_tcm<T: Convert>(&mut self, addr: u32) -> T {
        self.mem_read_with_options::<false, T>(addr)
    }

    pub fn mem_read_with_options<const TCM: bool, T: Convert>(&mut self, addr: u32) -> T {
        debug_inst_log::log_mem(MemLogKind::Read, addr, 0);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;

        let shm_offset = self.get_shm_offset::<TCM, false>(aligned_addr) as u32;
        if shm_offset != 0 {
            let ret: T = utils::read_from_mem(&self.mem.shm, shm_offset);
            debug_inst_log::log_mem(MemLogKind::ReadValue, addr, ret.into());
            return ret;
        }

        let ret: T = MemoryIo::<TCM, T>::read(aligned_addr, self);
        debug_inst_log::log_mem(MemLogKind::ReadValue, addr, ret.into());
        ret
    }

    pub fn mem_read_multiple_slice<const TCM: bool, const SHM_MEMORY: bool, T: Convert>(&mut self, addr: u32, slice: &mut [T]) {
        debug_inst_log::log_mem(MemLogKind::SliceReadSize, addr, size_of_val(slice) as u32);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;

        if SHM_MEMORY {
            let shm_offset = self.get_shm_offset::<TCM, false>(aligned_addr) as u32;
            if shm_offset != 0 {
                utils::read_from_mem_slice(&self.mem.shm, shm_offset, slice);
                debug_inst_log::log_mem_slice(MemLogKind::SliceReadValue, aligned_addr, slice);
                return;
            }
        }

        MemoryMultipleSliceIo::<TCM, T>::read(aligned_addr, slice, self);

        debug_inst_log::log_mem_slice(MemLogKind::SliceReadValue, aligned_addr, slice);
    }

    pub fn mem_read_fixed_slice<const TCM: bool, T: Convert>(&mut self, addr: u32, slice: &mut [T]) {
        debug_inst_log::log_mem(MemLogKind::FixedReadSize, addr, size_of_val(slice) as u32);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;

        let shm_offset = self.get_shm_offset::<TCM, false>(aligned_addr) as u32;
        if shm_offset != 0 {
            slice.fill(utils::read_from_mem(&self.mem.shm, shm_offset));
        } else {
            MemoryFixedSliceIo::<TCM, T>::read(aligned_addr, slice, self);
        }

        debug_inst_log::log_mem_slice(MemLogKind::FixedReadValue, aligned_addr, slice);
    }

    pub fn mem_write<T: Convert>(&mut self, addr: u32, value: T) {
        self.mem_write_internal::<true, T>(addr, value)
    }

    pub fn mem_write_no_tcm<T: Convert>(&mut self, addr: u32, value: T) {
        self.mem_write_internal::<false, T>(addr, value)
    }

    fn mem_write_internal<const TCM: bool, T: Convert>(&mut self, addr: u32, value: T) {
        debug_inst_log::log_mem(MemLogKind::WriteValue, addr, value.into());
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;

        let shm_offset = self.get_shm_offset::<TCM, true>(aligned_addr);
        if shm_offset != 0 {
            utils::write_to_mem(&mut self.mem.shm, shm_offset as u32, value);
            return;
        }

        MemoryIo::<TCM, T>::write(aligned_addr, value, self);
    }

    pub fn mem_write_multiple_slice<const TCM: bool, T: Convert>(&mut self, addr: u32, slice: &[T]) {
        debug_inst_log::log_mem(MemLogKind::FixedWriteSize, addr, size_of_val(slice) as u32);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;
        debug_inst_log::log_mem_slice(MemLogKind::SliceWriteValue, aligned_addr, slice);

        // The fast path may only be taken when EVERY mmu page the slice touches is
        // writable: checking just the first page let multi-page copies (CpuFastSet, dma,
        // stm) that start on an unprotected page silently overwrite live jit code on a
        // protected one — no fault, no invalidation, stale blocks kept executing (the
        // m4a frequency switch copies its rewritten mixer exactly like this).
        let shm_offset = self.get_shm_offset::<TCM, true>(aligned_addr) as u32;
        if shm_offset != 0 {
            let end = aligned_addr + size_of_val(slice) as u32 - 1;
            let mut page = (aligned_addr | (MMU_PAGE_SIZE as u32 - 1)) + 1;
            let mut all_writable = true;
            while page <= end {
                if self.get_shm_offset::<TCM, true>(page) == 0 {
                    all_writable = false;
                    break;
                }
                page += MMU_PAGE_SIZE as u32;
            }
            if all_writable {
                utils::write_to_mem_slice(&mut self.mem.shm, shm_offset as usize, slice);
                return;
            }
        }

        MemoryMultipleSliceIo::<TCM, T>::write(aligned_addr, slice, self);
    }

    pub fn mem_write_fixed_slice<const TCM: bool, T: Convert>(&mut self, addr: u32, slice: &[T]) {
        debug_inst_log::log_mem(MemLogKind::FixedWriteSize, addr, size_of_val(slice) as u32);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;
        debug_inst_log::log_mem_slice(MemLogKind::FixedWriteValue, aligned_addr, slice);

        let shm_offset = self.get_shm_offset::<TCM, true>(aligned_addr) as u32;
        if shm_offset != 0 {
            utils::write_to_mem(&mut self.mem.shm, shm_offset, unsafe { slice.last().unwrap_unchecked() });
            return;
        }

        MemoryFixedSliceIo::<TCM, T>::write(aligned_addr, slice, self);
    }

    pub fn mem_write_multiple_memset<const TCM: bool, T: Convert>(&mut self, addr: u32, value: T, size: usize) {
        debug_inst_log::log_mem(MemLogKind::MemsetWriteSize, addr, (size_of::<T>() * size) as u32);
        let aligned_addr = addr & !(size_of::<T>() as u32 - 1);
        let aligned_addr = aligned_addr & 0x0FFFFFFF;

        let shm_offset = self.get_shm_offset::<TCM, true>(aligned_addr) as u32;
        if shm_offset != 0 {
            utils::write_memset(&mut self.mem.shm, shm_offset as usize, value, size);
            return;
        }

        MemoryMultipleMemsetIo::<TCM, T>::write(aligned_addr, value, size, self);
    }

    pub fn mem_read_struct<const TCM: bool, T>(&mut self, addr: u32) -> T
    where
        [(); size_of::<T>()]:,
    {
        let mut mem = [0; size_of::<T>()];
        self.mem_read_multiple_slice::<TCM, true, u8>(addr, &mut mem);
        unsafe { mem::transmute_copy(&mem) }
    }

    pub fn mem_write_struct<const TCM: bool, T>(&mut self, addr: u32, value: &T) {
        let slice = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
        self.mem_write_multiple_slice::<TCM, u8>(addr, slice);
    }
}
