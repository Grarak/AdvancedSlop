use crate::core::memory::mmu::FAST_MEM_PAGE_SIZE;
use crate::mmap::MemRegion;

pub const EWRAM_OFFSET: u32 = 0x02000000;
pub const EWRAM_SIZE: u32 = 256 * 1024;

pub const IWRAM_OFFSET: u32 = 0x03000000;
pub const IWRAM_SIZE: u32 = 32 * 1024;

pub const IO_PORTS_OFFSET: u32 = 0x04000000;

pub const PALETTES_OFFSET: u32 = 0x05000000;
pub const PALETTES_SIZE: u32 = 1024;

pub const VRAM_OFFSET: u32 = 0x06000000;
pub const VRAM_SIZE: u32 = 96 * 1024;

pub const OAM_OFFSET: u32 = 0x07000000;
pub const OAM_SIZE: u32 = 1024;

pub const ROM_OFFSET: u32 = 0x08000000;
pub const ROM_SIZE: u32 = 32 * 1024 * 1024;
pub const ROM_END: u32 = 0x0E000000;
// The GBA rom address space repeats every 32 MB: images at 0x08/0x0A/0x0C.
pub const ROM_MASK: u32 = ROM_SIZE - 1;

pub const SRAM_OFFSET: u32 = 0x0E000000;
// 64 KB bus window; the actual save (sram/flash) lives on the cartridge, not in shm.
pub const SRAM_SIZE: u32 = 64 * 1024;

// No BIOS region: the BIOS is HLE-only (SWI traps + HLE irq dispatch), reads of
// 0x00000000-0x01FFFFFF are open bus.

pub const TOTAL_MEM_SIZE: u32 = 16 * 1024 /* Some padding for mmu */
        + EWRAM_SIZE + IWRAM_SIZE
        + FAST_MEM_PAGE_SIZE as u32 /* Palettes */
        + VRAM_SIZE
        + FAST_MEM_PAGE_SIZE as u32 /* OAM */
        + ROM_SIZE;

const P_EWRAM_OFFSET: usize = 16 * 1024;
const P_IWRAM_OFFSET: usize = P_EWRAM_OFFSET + EWRAM_SIZE as usize;
const P_PALETTES_OFFSET: usize = P_IWRAM_OFFSET + IWRAM_SIZE as usize;
const P_VRAM_OFFSET: usize = P_PALETTES_OFFSET + FAST_MEM_PAGE_SIZE;
const P_OAM_OFFSET: usize = P_VRAM_OFFSET + VRAM_SIZE as usize;
const P_ROM_OFFSET: usize = P_OAM_OFFSET + FAST_MEM_PAGE_SIZE;

// Mutable guest memory span inside the shm: ewram, iwram, palettes, vram, oam.
// Excludes the mmu padding before and the immutable rom after.
pub const SAVESTATE_SHM_RANGE: std::ops::Range<usize> = P_EWRAM_OFFSET..P_ROM_OFFSET;

pub const EWRAM_REGION: MemRegion = MemRegion::new(EWRAM_OFFSET as usize, IWRAM_OFFSET as usize, EWRAM_SIZE as usize, P_EWRAM_OFFSET, true);
pub const IWRAM_REGION: MemRegion = MemRegion::new(IWRAM_OFFSET as usize, IO_PORTS_OFFSET as usize, IWRAM_SIZE as usize, P_IWRAM_OFFSET, true);
pub const PALETTES_REGION: MemRegion = MemRegion::new(PALETTES_OFFSET as usize, VRAM_OFFSET as usize, FAST_MEM_PAGE_SIZE, P_PALETTES_OFFSET, true);
// VRAM is 96 KB (not a power of two; 64K + 32K with the upper 32K mirrored once per
// 128K block) — it never goes through the pow2 MemRegion mirror math. Slow-path
// handlers apply the exact mirror formula; this region exists for shm bookkeeping.
pub const VRAM_REGION: MemRegion = MemRegion::new(VRAM_OFFSET as usize, OAM_OFFSET as usize, VRAM_SIZE as usize, P_VRAM_OFFSET, true);
pub const OAM_REGION: MemRegion = MemRegion::new(OAM_OFFSET as usize, ROM_OFFSET as usize, FAST_MEM_PAGE_SIZE, P_OAM_OFFSET, true);
pub const ROM_REGION: MemRegion = MemRegion::new(ROM_OFFSET as usize, ROM_END as usize, ROM_SIZE as usize, P_ROM_OFFSET, false);

// The fastmem reservation must reach past SRAM at 0x0E000000 (the DS ARM7 build used
// 0x0B000000; the name is kept to minimize jit-side churn).
pub const V_MEM_ARM7_RANGE: u32 = 0x10000000;
