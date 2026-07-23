use crate::core::emu::Emu;
use crate::core::memory::regions;
use crate::core::memory::regions::{EWRAM_REGION, IWRAM_REGION, ROM_REGION, V_MEM_ARM7_RANGE};
use crate::core::mmu_tcm_addr;
use crate::mmap::{MemRegion, VirtualMem};
use crate::utils::HeapArrayUsize;
use regions::{EWRAM_OFFSET, IO_PORTS_OFFSET, IWRAM_OFFSET, ROM_END, ROM_OFFSET};

pub const MMU_PAGE_SHIFT: usize = 12;
pub const MMU_PAGE_SIZE: usize = 1 << MMU_PAGE_SHIFT;

const FAST_MEM_PAGE_SHIFT: usize = 14;
pub const FAST_MEM_PAGE_SIZE: usize = 1 << FAST_MEM_PAGE_SHIFT;

fn remove_mmu_write_entry(addr: u32, region: &MemRegion, mmu: &mut [usize], vmem: Option<&mut VirtualMem>) {
    if mmu[(addr >> MMU_PAGE_SHIFT) as usize] == 0 {
        return;
    }
    let base_offset = addr - region.start as u32;
    let base_offset = base_offset & (region.size as u32 - 1);
    for addr_offset in (region.start as u32 + base_offset..region.end as u32).step_by(region.size) {
        mmu[(addr_offset >> MMU_PAGE_SHIFT) as usize] = 0;
    }
    if let Some(vmem) = vmem {
        vmem.set_region_protection(addr as usize & !(MMU_PAGE_SIZE - 1), MMU_PAGE_SIZE, region, true, false, false);
    }
}

// Reverse of remove_mmu_write_entry: restore fastmem writes to a page across all its
// region mirrors. Called when invalidation empties a page of all compiled code — the
// write protection only exists to catch stores into live jit code, so a dead page has
// nothing to detect and its stores should go direct (recompilation re-protects it).
fn restore_mmu_write_entry(addr: u32, region: &MemRegion, mmu_write: &mut [usize], vmem: &mut VirtualMem) {
    let page_base = addr & !(MMU_PAGE_SIZE as u32 - 1);
    let base_offset = (page_base - region.start as u32) & (region.size as u32 - 1);
    for addr_offset in (region.start as u32 + base_offset..region.end as u32).step_by(region.size) {
        mmu_write[(addr_offset >> MMU_PAGE_SHIFT) as usize] = region.shm_offset + (addr_offset as usize & (region.size - 1));
    }
    vmem.set_region_protection(page_base as usize, MMU_PAGE_SIZE, region, true, true, false);
}

// Single GBA mmu on the ARM7 slot. Fastmem serves EWRAM/IWRAM (read+write, mirrored),
// VRAM (read-only) and the three rom images (read-only). VRAM/palette/OAM writes stay
// slow-path for the byte-write quirks; palette and OAM get no read maps either, since
// their regions are smaller than a fastmem page. IO/SRAM/EEPROM are never mapped.
pub struct MmuArm7 {
    vmem: VirtualMem,
    mmu_read: HeapArrayUsize<{ V_MEM_ARM7_RANGE as usize / MMU_PAGE_SIZE }>,
    mmu_write: HeapArrayUsize<{ V_MEM_ARM7_RANGE as usize / MMU_PAGE_SIZE }>,
}

impl MmuArm7 {
    pub fn new() -> Self {
        let vmem = VirtualMem::new(V_MEM_ARM7_RANGE as usize, mmu_tcm_addr()).unwrap();
        crate::core::set_mmu_tcm_addr(vmem.as_ptr() as usize);
        MmuArm7 {
            vmem,
            mmu_read: HeapArrayUsize::default(),
            mmu_write: HeapArrayUsize::default(),
        }
    }
}

impl Emu {
    fn update_all_arm7(&mut self) {
        let rom_len = self.cartridge.io.rom_len;
        let rom_mask = self.cartridge.io.rom_mask;
        let is_eeprom = self.cartridge.io.save_type == crate::cartridge_io::SaveType::Eeprom;
        let eeprom_window_start = if rom_len > 16 * 1024 * 1024 { 0x0DFFFF00 } else { 0x0D000000 };

        for addr in (0..V_MEM_ARM7_RANGE).step_by(MMU_PAGE_SIZE) {
            let mmu_read = &mut self.mem.mmu_arm7.mmu_read[(addr as usize) >> MMU_PAGE_SHIFT];
            let mmu_write = &mut self.mem.mmu_arm7.mmu_write[(addr as usize) >> MMU_PAGE_SHIFT];
            *mmu_read = 0;
            *mmu_write = 0;

            match addr & 0x0F000000 {
                EWRAM_OFFSET => {
                    let addr_offset = (addr as usize) & (EWRAM_REGION.size - 1);
                    *mmu_read = EWRAM_REGION.shm_offset + addr_offset;
                    *mmu_write = EWRAM_REGION.shm_offset + addr_offset;
                }
                IWRAM_OFFSET => {
                    let addr_offset = (addr as usize) & (IWRAM_REGION.size - 1);
                    *mmu_read = IWRAM_REGION.shm_offset + addr_offset;
                    *mmu_write = IWRAM_REGION.shm_offset + addr_offset;
                }
                regions::VRAM_OFFSET => {
                    // Read-only fastmem: byte stores have merge/ignore quirks, so writes
                    // stay on the slow path (write table entry stays 0).
                    *mmu_read = regions::VRAM_REGION.shm_offset + crate::core::memory::mem::vram_offset(addr) as usize;
                }
                ROM_OFFSET..ROM_END => {
                    let rom_offset = (addr & 0x01FFFFFF) & rom_mask;
                    let in_eeprom_window = is_eeprom && addr >= eeprom_window_start && addr < 0x0E000000;
                    if rom_offset < rom_len && !in_eeprom_window {
                        *mmu_read = ROM_REGION.shm_offset + rom_offset as usize;
                    }
                }
                _ => {}
            }
        }

        // mprotect is the only way in kubridge to merge pages
        // Because with jit we are setting protection in 4kb pages
        // iwram is the only region with smc detection
        for addr in (IWRAM_OFFSET..IO_PORTS_OFFSET).step_by(FAST_MEM_PAGE_SIZE) {
            self.mem.mmu_arm7.vmem.set_protection(addr as usize, FAST_MEM_PAGE_SIZE, false, false, false);
        }

        for addr in (EWRAM_OFFSET..IWRAM_OFFSET).step_by(FAST_MEM_PAGE_SIZE) {
            let base_addr = addr & !0xFF000000;
            self.mem
                .mmu_arm7
                .vmem
                .create_page_map(&self.mem.shm, EWRAM_REGION.shm_offset, base_addr as usize, EWRAM_REGION.size, addr as usize, FAST_MEM_PAGE_SIZE, true)
                .unwrap();
        }
        for addr in (IWRAM_OFFSET..IO_PORTS_OFFSET).step_by(FAST_MEM_PAGE_SIZE) {
            let base_addr = addr & !0xFF000000;
            self.mem
                .mmu_arm7
                .vmem
                .create_page_map(&self.mem.shm, IWRAM_REGION.shm_offset, base_addr as usize, IWRAM_REGION.size, addr as usize, FAST_MEM_PAGE_SIZE, true)
                .unwrap();
        }

        // VRAM: read-only fastmem (loads + slice reads go direct; stores fault into the
        // slow path for the byte-write quirks). 96K = 64K bg + 32K obj with the obj half
        // mirrored once per 128K block, blocks repeating over the 16M region.
        //
        // One FAST_MEM_PAGE_SIZE page per map, like every other fastmem region rather
        // than one map per 64K/32K half: on the Vita each map is a kubridge commit, and
        // kubridge only ever decommits or reprotects *whole* committed pages — it refuses
        // to split one ("attempted to partially decommit a page. Ignoring...") — so the
        // granularity committed here is the granularity everything downstream is stuck
        // with. 16K divides both halves and the mirror seam at +0x18000, so no page
        // straddles the seam and vram_offset alone gives each page's shm source. This
        // runs once per game load (plus savestate restores), not per frame.
        for addr in (regions::VRAM_OFFSET..regions::OAM_OFFSET).step_by(FAST_MEM_PAGE_SIZE) {
            self.mem.mmu_arm7.vmem.destroy_map(addr as _, FAST_MEM_PAGE_SIZE);
            let shm_offset = regions::VRAM_REGION.shm_offset + crate::core::memory::mem::vram_offset(addr) as usize;
            self.mem
                .mmu_arm7
                .vmem
                .create_map(&self.mem.shm, shm_offset, addr as usize, FAST_MEM_PAGE_SIZE, true, false, false)
                .unwrap();
        }

        // Rom images: read-only, truncated to the rom size (mirror reads past the rom
        // fall to the slow path, which serves the open-bus pattern). For EEPROM carts the
        // 0x0D range (or the top page for >16M roms) stays unmapped so accesses fault into
        // the cartridge handler. Mapped one FAST_MEM_PAGE_SIZE page at a time so
        // mmu_unmap_rom_head can decommit the first page (for the GPIO/RTC fault): the
        // Vita's kubridge can only decommit whole committed pages, so that page must be
        // its own commit.
        if rom_len > 0 {
            let mapped_len = (rom_len.min(regions::ROM_SIZE) as usize).next_multiple_of(FAST_MEM_PAGE_SIZE);
            for image in (ROM_OFFSET..ROM_END).step_by(regions::ROM_SIZE as usize) {
                let mut image_len = mapped_len;
                if is_eeprom {
                    let eeprom_start = if rom_len > 16 * 1024 * 1024 { 0x01FFC000 } else { 0x01000000 };
                    image_len = image_len.min(eeprom_start);
                }
                for off in (0..regions::ROM_SIZE).step_by(FAST_MEM_PAGE_SIZE) {
                    self.mem
                        .mmu_arm7
                        .vmem
                        .create_map(&self.mem.shm, ROM_REGION.shm_offset + off as usize, image as usize + off as usize, FAST_MEM_PAGE_SIZE, (off as usize) < image_len, false, false)
                        .unwrap();
                }
            }
        }
    }
}

impl Emu {
    // Unmap the first fastmem page of each rom image so GPIO reads (0x080000C4+)
    // fault into the slow path; the mmu entries are cleared too
    pub fn mmu_unmap_rom_head(&mut self) {
        for image in (ROM_OFFSET..ROM_END).step_by(regions::ROM_SIZE as usize) {
            self.mem.mmu_arm7.vmem.set_protection(image as usize, FAST_MEM_PAGE_SIZE, false, false, false);
            for addr in (image..image + FAST_MEM_PAGE_SIZE as u32).step_by(MMU_PAGE_SIZE) {
                self.mem.mmu_arm7.mmu_read[(addr as usize) >> MMU_PAGE_SHIFT] = 0;
            }
        }
    }

    #[inline(never)]
    pub fn mmu_update_all(&mut self) {
        self.update_all_arm7()
    }

    pub fn mmu_get_read(&self) -> &[usize] {
        self.mem.mmu_arm7.mmu_read.as_ref()
    }

    pub fn mmu_get_read_tcm(&self) -> &[usize] {
        self.mem.mmu_arm7.mmu_read.as_ref()
    }

    pub fn mmu_get_write(&self) -> &[usize] {
        self.mem.mmu_arm7.mmu_write.as_ref()
    }

    pub fn mmu_get_write_tcm(&self) -> &[usize] {
        self.mem.mmu_arm7.mmu_write.as_ref()
    }

    pub fn mmu_remove_write(&mut self, addr: u32, region: &MemRegion) {
        remove_mmu_write_entry(addr, region, self.mem.mmu_arm7.mmu_write.as_mut(), Some(&mut self.mem.mmu_arm7.vmem))
    }

    pub fn mmu_restore_write(&mut self, addr: u32, region: &MemRegion) {
        restore_mmu_write_entry(addr, region, self.mem.mmu_arm7.mmu_write.as_mut(), &mut self.mem.mmu_arm7.vmem)
    }
}
