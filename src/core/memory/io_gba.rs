use crate::core::emu::Emu;
use crate::core::memory::io_gba_lut::*;
use crate::utils::Convert;
use std::intrinsics::likely;

pub mod io_gba {
    use std::hint::unreachable_unchecked;
    use std::mem;

    use crate::core::{emu::Emu, memory::io_gba_lut::*};
    use crate::utils::Convert;

    pub fn get_read<T: Convert>(addr_offset: u32) -> Option<fn(&mut Emu) -> T> {
        if io_gba_read::is_in_range(addr_offset) {
            Some(io_gba_read::get_read(addr_offset))
        } else {
            None
        }
    }

    pub fn get_write<T: Convert>(addr_offset: u32) -> Option<fn(&mut Emu, T)> {
        if io_gba_write::is_in_range(addr_offset) {
            Some(io_gba_write::get_write(addr_offset))
        } else {
            None
        }
    }

    pub fn get_read_with_size(addr_offset: u32, size: usize) -> Option<fn(&mut Emu) -> u32> {
        unsafe {
            match size {
                1 => mem::transmute(get_read::<u8>(addr_offset)),
                2 => mem::transmute(get_read::<u16>(addr_offset)),
                4 => mem::transmute(get_read::<u32>(addr_offset)),
                _ => unreachable_unchecked(),
            }
        }
    }

    pub fn get_write_with_size(addr_offset: u32, size: usize) -> Option<fn(&mut Emu, u32)> {
        unsafe {
            match size {
                1 => mem::transmute(get_write::<u8>(addr_offset)),
                2 => mem::transmute(get_write::<u16>(addr_offset)),
                4 => mem::transmute(get_write::<u32>(addr_offset)),
                _ => unreachable_unchecked(),
            }
        }
    }
}

impl Emu {
    pub fn io_gba_read<T: Convert>(&mut self, addr_offset: u32) -> T {
        if likely(io_gba_read::is_in_range(addr_offset)) {
            io_gba_read::read(self, addr_offset)
        } else {
            T::from(0)
        }
    }

    pub fn io_gba_write<T: Convert>(&mut self, addr_offset: u32, value: T) {
        if likely(io_gba_write::is_in_range(addr_offset)) {
            io_gba_write::write(self, value, addr_offset);
        }
    }

    pub fn io_gba_write_fixed_slice<T: Convert>(&mut self, addr_offset: u32, slice: &[T]) {
        if likely(io_gba_write::is_in_range(addr_offset)) {
            let func = io_gba_write::get_write(addr_offset);
            for value in slice {
                func(self, *value);
            }
        }
    }
}
