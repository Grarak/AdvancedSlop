use crate::core::memory::regions;
use crate::jit::jit_memory::{JitEntries, JitEntry, JitExecCounts, JitLiveRanges, BIOS_UNINTERRUPT_ENTRY_ARM7, JIT_LIVE_RANGE_PAGE_SIZE_SHIFT};
use crate::utils;
use crate::utils::HeapArrayUsize;
use std::cmp::min;
use std::{ptr, slice};

// The whole GBA bus (0x00000000-0x0FFFFFFF) fits in 256MB of guest address space.
const MEMORY_RANGE: u32 = 0x10000000;

pub const BLOCK_SHIFT: usize = 13;
pub const BLOCK_SIZE: usize = 1 << BLOCK_SHIFT;
const SIZE: usize = (MEMORY_RANGE >> 1) as usize / BLOCK_SIZE;
const LIVE_RANGES_SIZE: usize = (MEMORY_RANGE >> (JIT_LIVE_RANGE_PAGE_SIZE_SHIFT + 3)) as usize;

pub struct JitMemoryMap {
    map: HeapArrayUsize<SIZE>,
    exec_counts_map: HeapArrayUsize<SIZE>,
    live_ranges_map: HeapArrayUsize<LIVE_RANGES_SIZE>,
}

impl JitMemoryMap {
    pub fn new(entries: &JitEntries, live_ranges: &JitLiveRanges, exec_counts: &JitExecCounts) -> Self {
        let mut instance = JitMemoryMap {
            map: HeapArrayUsize::default(),
            exec_counts_map: HeapArrayUsize::default(),
            live_ranges_map: HeapArrayUsize::default(),
        };

        macro_rules! get_ptr {
            ($addr:expr, $entries:expr) => {{
                (unsafe { $entries.as_ptr().add(($addr >> 1) % $entries.len()) } as usize)
            }};
        }

        instance.map[(0xFF00000) >> BLOCK_SHIFT >> 1] = ptr::addr_of!(BIOS_UNINTERRUPT_ENTRY_ARM7) as usize;

        for i in 0..SIZE {
            let addr = (i << BLOCK_SHIFT) << 1;
            let map_ptr = &mut instance.map[i];
            let counts_ptr = &mut instance.exec_counts_map[i];

            match (addr as u32) & 0x0F000000 {
                regions::EWRAM_OFFSET => {
                    *map_ptr = get_ptr!(addr, entries.ewram);
                    *counts_ptr = get_ptr!(addr, exec_counts.ewram);
                }
                regions::IWRAM_OFFSET => {
                    *map_ptr = get_ptr!(addr, entries.iwram);
                    *counts_ptr = get_ptr!(addr, exec_counts.iwram);
                }
                regions::ROM_OFFSET..regions::ROM_END => {
                    // Rom entries span one 32M image; the images at 0x0A/0x0C collapse
                    // onto it via the % in get_ptr
                    *map_ptr = get_ptr!(addr, entries.rom);
                    *counts_ptr = get_ptr!(addr, exec_counts.rom);
                }
                _ => {}
            }
        }

        macro_rules! get_ptr {
            ($index:expr, $live_ranges:expr) => {{
                (unsafe { $live_ranges.as_ptr().add($index % $live_ranges.len()) } as usize)
            }};
        }

        for i in 0..LIVE_RANGES_SIZE {
            let addr = i << (JIT_LIVE_RANGE_PAGE_SIZE_SHIFT + 3);
            let map_ptr = &mut instance.live_ranges_map[i];

            match (addr as u32) & 0xFF000000 {
                regions::EWRAM_OFFSET => *map_ptr = get_ptr!(i, live_ranges.ewram),
                regions::IWRAM_OFFSET => *map_ptr = get_ptr!(i, live_ranges.iwram),
                _ => {}
            }
        }

        instance
    }

    pub fn get_jit_entry(&self, addr: u32) -> *mut JitEntry {
        let addr = (addr & 0x0FFFFFFF) >> 1;
        unsafe { ((*self.map.get_unchecked((addr >> BLOCK_SHIFT) as usize)) as *mut JitEntry).add((addr as usize) & (BLOCK_SIZE - 1)) }
    }

    pub fn write_jit_entries(&mut self, addr: u32, size: usize, value: JitEntry) {
        let mut addr = (addr & 0x0FFFFFFF) >> 1;
        let mut size = size >> 1;
        while size > 0 {
            let block = self.map[(addr >> BLOCK_SHIFT) as usize] as *mut JitEntry;
            let block_offset = (addr as usize) & (BLOCK_SIZE - 1);
            let block_remaining = BLOCK_SIZE - block_offset;
            let write_size = min(block_remaining, size);
            unsafe { slice::from_raw_parts_mut(block.add(block_offset), write_size).fill(value) };
            addr = utils::align_up(addr as usize, BLOCK_SIZE) as u32;
            size -= write_size;
        }
    }

    /// The interpreter's hotness counter for a guest pc: one u8 per halfword slot,
    /// mirror-collapsed like the jit entries and shared between the cpus. Null for addresses
    /// outside the executable regions (the bios trampoline blocks, unmapped space) — callers
    /// skip the interpreter there and let the pc's jit entry deal with it.
    pub fn get_exec_count(&self, addr: u32) -> *mut u8 {
        let addr = (addr & 0x0FFFFFFF) >> 1;
        let block = unsafe { *self.exec_counts_map.get_unchecked((addr >> BLOCK_SHIFT) as usize) } as *mut u8;
        if block.is_null() {
            return block;
        }
        unsafe { block.add((addr as usize) & (BLOCK_SIZE - 1)) }
    }

    pub fn get_live_range(&self, addr: u32) -> *mut u8 {
        unsafe { (*self.live_ranges_map.get_unchecked((addr >> (JIT_LIVE_RANGE_PAGE_SIZE_SHIFT + 3)) as usize)) as _ }
    }

    pub fn has_jit_block(&self, addr: u32) -> bool {
        let live_range = self.get_live_range(addr);
        if live_range.is_null() {
            return false;
        }
        let bit = (addr >> JIT_LIVE_RANGE_PAGE_SIZE_SHIFT) & 0x7;
        unsafe { *live_range & (1 << bit) != 0 }
    }

    pub fn get_map_ptr(&self) -> *const JitEntry {
        self.map.as_ptr() as _
    }
}
