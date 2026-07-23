// Frame snapshots of the guest's video memory for the rasterizers: the cpu thread copies
// palette/oam/vram out of shm at vblank and goes straight back to the guest, so nothing
// downstream can be disturbed by what the guest does next and the cpu thread never has to
// wait for a frame to finish. GBA layout: 96K vram (64K bg + 32K obj at 0x10000), 1K
// palettes (256 bg + 256 obj entries), 1K oam.
use crate::core::memory::regions;
use crate::utils::HeapArrayU8;

pub const BG_VRAM_SIZE: usize = 0x10000;
pub const OBJ_VRAM_SIZE: usize = 0x8000;

// Dirty bits: set by the guest write handlers, drive both the vblank snapshot copies
// and the render-thread texture uploads
pub const DIRTY_BG: u8 = 1;
pub const DIRTY_OBJ: u8 = 2;
pub const DIRTY_PAL: u8 = 4;
pub const DIRTY_OAM: u8 = 8;

// One contiguous 96K vram image: the rasterizer indexes it by guest offset.
pub struct GpuMemBuf {
    pub vram: HeapArrayU8<{ BG_VRAM_SIZE + OBJ_VRAM_SIZE }>,
    pub pal: HeapArrayU8<{ regions::PALETTES_SIZE as usize }>,
    pub oam: HeapArrayU8<{ regions::OAM_SIZE as usize }>,
}

impl GpuMemBuf {
    pub fn new() -> Self {
        GpuMemBuf {
            vram: HeapArrayU8::default(),
            pal: HeapArrayU8::default(),
            oam: HeapArrayU8::default(),
        }
    }

    pub fn snapshot(&mut self, shm: &crate::mmap::Shm, dirty: u8) {
        if dirty & (DIRTY_BG | DIRTY_OBJ) != 0 {
            let vram = &shm[regions::VRAM_REGION.shm_offset..regions::VRAM_REGION.shm_offset + regions::VRAM_SIZE as usize];
            if dirty & DIRTY_BG != 0 {
                self.vram[..BG_VRAM_SIZE].copy_from_slice(&vram[..BG_VRAM_SIZE]);
            }
            if dirty & DIRTY_OBJ != 0 {
                self.vram[BG_VRAM_SIZE..].copy_from_slice(&vram[BG_VRAM_SIZE..BG_VRAM_SIZE + OBJ_VRAM_SIZE]);
            }
        }
        if dirty & DIRTY_PAL != 0 {
            let pal = &shm[regions::PALETTES_REGION.shm_offset..regions::PALETTES_REGION.shm_offset + regions::PALETTES_SIZE as usize];
            self.pal.copy_from_slice(pal);
        }
        if dirty & DIRTY_OAM != 0 {
            let oam = &shm[regions::OAM_REGION.shm_offset..regions::OAM_REGION.shm_offset + regions::OAM_SIZE as usize];
            self.oam.copy_from_slice(oam);
        }
    }
}
