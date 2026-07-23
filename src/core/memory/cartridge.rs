use crate::cartridge_io::{CartridgeIo, SaveType};
use crate::core::emu::Emu;
use crate::core::memory::regions;
use crate::logging::debug_println;
use crate::savestate::{Savestate, SavestateContext};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Rom-load progress, published by the cpu thread while streaming the rom into shm and
// polled by the main thread to draw the loading bar.
pub static ROM_LOAD_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static ROM_LOAD_TOTAL: AtomicU32 = AtomicU32::new(0);
pub static ROM_LOAD_DONE: AtomicU32 = AtomicU32::new(0);

// Flash command state machine (Macronix ids like NooDS: 0xC2, device 0x1C 64K / 0x09
// 128K). Commands arrive as byte writes at 0x0E005555/0x0E002AAA.
#[derive(Default)]
struct FlashState {
    cmd_stage: u8,
    id_mode: bool,
    erase_mode: bool,
    write_mode: bool,
    bank_mode: bool,
    bank: u8,
}

// EEPROM bit-serial protocol state (NooDS CartridgeGba::eepromRead/eepromWrite):
// commands and data arrive one bit at a time through DMA3 to the top of the rom space.
// size stays 0 until the first transfer reveals whether commands are 8-bit (0.5KB,
// 6-bit block address) or 16-bit (8KB, 10-bit block address).
#[derive(Default)]
struct EepromState {
    count: u16,
    cmd: u16,
    data: u64,
    done: bool,
    size: u32,
}

// GBA cartridge: memory-mapped rom (copied into shm at boot, served by fastmem/slow
// path), plus the save backends behind the 0x0E bus window and the EEPROM window at
// the top of the rom space.
pub struct Cartridge {
    pub io: CartridgeIo,
    flash: FlashState,
    eeprom: EepromState,
}

impl Cartridge {
    pub fn new() -> Self {
        Cartridge {
            io: CartridgeIo::empty(),
            flash: FlashState::default(),
            eeprom: EepromState::default(),
        }
    }

    pub fn set_cartridge_io(&mut self, cartridge_io: CartridgeIo) {
        self.io = cartridge_io;
        self.flash = FlashState::default();
        self.eeprom = EepromState::default();
    }
}

impl Savestate for Cartridge {
    fn savestate(&mut self, _state: &mut SavestateContext) {
        // TODO(P6): serialize eeprom/flash protocol state + save_buf
    }
}

impl Emu {
    // Copy the rom into its shm slot so the fastmem images at 0x08/0x0A/0x0C serve it
    pub fn cartridge_load_rom_into_shm(&mut self) {
        use std::io::Read;
        let size = (self.cartridge.io.rom_len as usize).min(regions::ROM_SIZE as usize);
        let shm_offset = regions::ROM_REGION.shm_offset;
        // Read the rom file straight into the already-allocated fastmem shm region in
        // chunks — no resident Vec — publishing progress for the loading bar. DS pages
        // its cart because it never executes from it; GBA runs code from rom, so the
        // bytes must live in shm, but only there.
        let mut file = std::fs::File::open(&self.cartridge.io.file_path).expect("failed to reopen rom");
        ROM_LOAD_TOTAL.store(size as u32, Ordering::Relaxed);
        ROM_LOAD_DONE.store(0, Ordering::Relaxed);
        const CHUNK: usize = 512 * 1024;
        let mut off = 0;
        while off < size {
            let end = (off + CHUNK).min(size);
            file.read_exact(&mut self.mem.shm[shm_offset + off..shm_offset + end]).expect("failed to read rom into shm");
            off = end;
            ROM_LOAD_DONE.store(off as u32, Ordering::Relaxed);
        }
        // Detect save type / RTC and load the save now that the rom is resident in shm.
        let rom = &self.mem.shm[shm_offset..shm_offset + size];
        self.cartridge.io.detect_and_load_save(rom);
        ROM_LOAD_ACTIVE.store(false, Ordering::Release);
    }

    pub fn cartridge_read_sram(&mut self, addr: u32) -> u8 {
        let offset = addr & 0xFFFF;
        match self.cartridge.io.save_type {
            SaveType::None | SaveType::Eeprom => 0xFF,
            SaveType::Sram => self.cartridge.io.read_save_buf(offset & 0x7FFF),
            SaveType::Flash64k | SaveType::Flash128k => {
                let flash = &self.cartridge.flash;
                if flash.id_mode {
                    match offset {
                        0 => 0xC2, // Macronix
                        1 => {
                            if self.cartridge.io.save_type == SaveType::Flash128k {
                                0x09
                            } else {
                                0x1C
                            }
                        }
                        _ => 0xFF,
                    }
                } else {
                    self.cartridge.io.read_save_buf((flash.bank as u32) * 0x10000 + offset)
                }
            }
        }
    }

    pub fn cartridge_write_sram(&mut self, addr: u32, value: u8) {
        let offset = addr & 0xFFFF;
        match self.cartridge.io.save_type {
            SaveType::None | SaveType::Eeprom => {}
            SaveType::Sram => self.cartridge.io.write_save_buf(offset & 0x7FFF, value),
            SaveType::Flash64k | SaveType::Flash128k => self.cartridge_write_flash(offset, value),
        }
    }

    fn cartridge_write_flash(&mut self, offset: u32, value: u8) {
        let is_128k = self.cartridge.io.save_type == SaveType::Flash128k;

        if self.cartridge.flash.write_mode {
            self.cartridge.flash.write_mode = false;
            let bank = self.cartridge.flash.bank as u32;
            self.cartridge.io.write_save_buf(bank * 0x10000 + offset, value);
            return;
        }
        if self.cartridge.flash.bank_mode && offset == 0 {
            self.cartridge.flash.bank = value & (is_128k as u8);
            self.cartridge.flash.bank_mode = false;
            return;
        }

        match self.cartridge.flash.cmd_stage {
            0 => {
                if offset == 0x5555 && value == 0xAA {
                    self.cartridge.flash.cmd_stage = 1;
                }
            }
            1 => {
                self.cartridge.flash.cmd_stage = if offset == 0x2AAA && value == 0x55 { 2 } else { 0 };
            }
            _ => {
                self.cartridge.flash.cmd_stage = 0;
                if self.cartridge.flash.erase_mode {
                    self.cartridge.flash.erase_mode = false;
                    if offset == 0x5555 && value == 0x10 {
                        // Chip erase
                        let len = self.cartridge.io.save_buf.read().unwrap().len();
                        for i in 0..len {
                            self.cartridge.io.write_save_buf(i as u32, 0xFF);
                        }
                        return;
                    }
                    if value == 0x30 {
                        // 4K sector erase
                        let base = (self.cartridge.flash.bank as u32) * 0x10000 + (offset & 0xF000);
                        for i in 0..0x1000 {
                            self.cartridge.io.write_save_buf(base + i, 0xFF);
                        }
                        return;
                    }
                }
                match value {
                    0x90 => self.cartridge.flash.id_mode = true,
                    0xF0 => self.cartridge.flash.id_mode = false,
                    0xA0 => self.cartridge.flash.write_mode = true,
                    0x80 => self.cartridge.flash.erase_mode = true,
                    0xB0 if is_128k => self.cartridge.flash.bank_mode = true,
                    _ => debug_println!("flash: unhandled command {value:x} at {offset:x}"),
                }
            }
        }
    }

    pub fn cartridge_is_eeprom(&self, addr: u32) -> bool {
        if self.cartridge.io.save_type != SaveType::Eeprom {
            return false;
        }
        // Small roms: the whole 0x0D000000 range; >16MB roms: only the top 256 bytes
        if self.cartridge.io.rom_len > 16 * 1024 * 1024 {
            addr >= 0x0DFFFF00
        } else {
            (0x0D000000..0x0E000000).contains(&addr)
        }
    }

    pub fn cartridge_read_eeprom(&mut self) -> u16 {
        let eeprom = &mut self.cartridge.eeprom;
        if eeprom.size == 0 {
            // Detect the save size from how many command bits were sent before reading
            eeprom.size = if eeprom.count == 9 { 0x200 } else { 0x2000 };
            self.cartridge.io.resize_save_buf(eeprom.size as usize);
        }

        // 0.5KB uses 8-bit commands, 8KB uses 16-bit commands
        let length = if eeprom.size == 0x200 { 8 } else { 16 };

        if (eeprom.cmd & 0xC000) >> 14 == 0x3 && eeprom.count >= length + 1 {
            // Read: 4 junk bits, then the 64 data bits MSB first
            eeprom.count += 1;
            if eeprom.count >= length + 6 {
                let bit = (63 - (eeprom.count as i32 - (length as i32 + 6))) as u32;
                let addr = if eeprom.size == 0x200 { (eeprom.cmd & 0x3F00) >> 8 } else { eeprom.cmd & 0x03FF } as u32;
                let value = (self.cartridge.io.read_save_buf(addr * 8 + bit / 8) >> (bit % 8)) & 1;

                if eeprom.count >= length + 69 {
                    *eeprom = EepromState { size: eeprom.size, ..EepromState::default() };
                }
                return value as u16;
            }
        } else if eeprom.done {
            // Signal that a write has finished
            return 1;
        }

        0
    }

    pub fn cartridge_write_eeprom(&mut self, value: u16) {
        let eeprom = &mut self.cartridge.eeprom;
        eeprom.done = false;

        let length = if eeprom.size == 0x200 { 8u16 } else { 16 };

        // Command bits arrive MSB first; with the size undetected the command counts as
        // 16-bit (like NooDS) — a read transfer resolves the real size before the
        // command is interpreted, and a write-first transfer assumes 8KB below.
        if eeprom.count < length {
            eeprom.count += 1;
            eeprom.cmd |= (value & 1) << (16 - eeprom.count);
        } else {
            match (eeprom.cmd & 0xC000) >> 14 {
                0x3 => {
                    // Accept the final bit that terminates a read command
                    if eeprom.count < length + 1 {
                        eeprom.count += 1;
                    }
                }
                0x2 => {
                    // Write: collect the 64 data bits MSB first, then a terminating bit
                    eeprom.count += 1;
                    if eeprom.count <= length + 64 {
                        eeprom.data |= ((value & 1) as u64) << (length + 64 - eeprom.count);
                    }
                    if eeprom.count >= length + 65 {
                        if eeprom.size == 0 {
                            // Games usually read first; a write-first transfer can't
                            // reveal the size, so assume 8KB (NooDS behavior)
                            eeprom.size = 0x2000;
                            self.cartridge.io.resize_save_buf(0x2000);
                        }
                        let eeprom = &mut self.cartridge.eeprom;
                        let addr = if eeprom.size == 0x200 { (eeprom.cmd & 0x3F00) >> 8 } else { eeprom.cmd & 0x03FF } as u32;
                        let data = eeprom.data;
                        let size = eeprom.size;
                        for i in 0..8 {
                            self.cartridge.io.write_save_buf(addr * 8 + i, (data >> (i * 8)) as u8);
                        }
                        self.cartridge.eeprom = EepromState {
                            size,
                            done: true,
                            ..EepromState::default()
                        };
                    }
                }
                _ => {}
            }
        }
    }

    // GPIO (RTC) at 0x080000C4-0x080000C9. When reads aren't enabled (GP_CONTROL
    // bit 0 clear) the rom bytes show through, like hardware.
    pub fn cartridge_read_gpio(&mut self, addr: u32) -> u16 {
        if self.rtc.gp_control & 1 == 0 {
            let offset = (addr & 0x01FFFFFF) & self.cartridge.io.rom_mask;
            if offset + 1 < self.cartridge.io.rom_len {
                let shm_offset = regions::ROM_REGION.shm_offset as u32 + offset;
                return crate::utils::read_from_mem::<u16>(&self.mem.shm, shm_offset);
            }
            return 0;
        }
        match addr & 0xF {
            0x4 => self.rtc.read_gp_data(),
            0x6 => self.rtc.gp_direction,
            0x8 => self.rtc.gp_control,
            _ => 0,
        }
    }

    pub fn cartridge_write_gpio(&mut self, addr: u32, value: u16) {
        match addr & 0xF {
            0x4 => self.rtc.write_gp_data(0xFFFF, value),
            0x6 => self.rtc.write_gp_direction(0xFFFF, value),
            0x8 => self.rtc.write_gp_control(0xFFFF, value),
            _ => {}
        }
    }

    // mGBA-style auto-detection: the first write into the GPIO window enables the rtc
    // (CFRU-based hacks lack the SIIRTC_V string) and unmaps the rom head page from
    // fastmem so GPIO reads reach the handler
    pub fn cartridge_auto_enable_rtc(&mut self) {
        if self.cartridge.io.has_rtc {
            return;
        }
        self.cartridge.io.has_rtc = true;
        crate::logging::info_println!("GPIO write detected: enabling cartridge RTC");
        self.mmu_unmap_rom_head();
    }
}
