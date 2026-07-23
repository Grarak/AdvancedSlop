use crate::logging::info_println;
use std::fs::File;
use std::io;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SaveType {
    None,
    // Size (512 bytes vs 8 KB) is detected at runtime from the first DMA command length
    Eeprom,
    Sram,
    Flash64k,
    Flash128k,
}

impl SaveType {
    pub fn initial_size(self) -> usize {
        match self {
            SaveType::None => 0,
            SaveType::Eeprom => 8 * 1024,
            SaveType::Sram => 32 * 1024,
            SaveType::Flash64k => 64 * 1024,
            SaveType::Flash128k => 128 * 1024,
        }
    }
}

// GBA rom string scan, 4-byte stride (NooDS cartridge.cpp findString)
fn find_string(rom: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    if rom.len() < needle.len() {
        return false;
    }
    (0..rom.len() - needle.len()).step_by(4).any(|i| &rom[i..i + needle.len()] == needle)
}

pub struct CartridgeIo {
    pub file_path: PathBuf,
    pub file_name: String,
    pub rom_len: u32,
    pub rom_mask: u32,
    pub title: String,
    pub game_code: u32,
    pub save_type: SaveType,
    pub has_rtc: bool,
    save_file_path: PathBuf,
    pub save_buf: RwLock<Vec<u8>>,
    save_dirty: AtomicBool,
}

impl CartridgeIo {
    // Placeholder until a rom is loaded (Emu construction precedes rom selection)
    pub fn empty() -> Self {
        CartridgeIo {
            file_path: PathBuf::new(),
            file_name: String::new(),
            rom_len: 0,
            rom_mask: 0,
            title: String::new(),
            game_code: 0,
            save_type: SaveType::None,
            has_rtc: false,
            save_file_path: PathBuf::new(),
            save_buf: RwLock::new(Vec::new()),
            save_dirty: AtomicBool::new(false),
        }
    }

    pub fn new(file_path: PathBuf, save_file_path: PathBuf) -> io::Result<Self> {
        // Only the header is read here (title/code/mirror mask). The rom body never
        // lives in a Vec: it is streamed straight into the fastmem shm at load time
        // (cartridge_load_rom_into_shm), and save-type/RTC detection scans it there.
        let mut file = File::open(&file_path)?;
        let rom_len = file.metadata()?.len() as u32;
        if rom_len < 0xC0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "rom smaller than gba header"));
        }
        let mut header = [0u8; 0xC0];
        file.read_exact(&mut header)?;

        let file_name = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let title = String::from_utf8_lossy(&header[0xA0..0xAC]).trim_end_matches('\0').to_string();
        let game_code = u32::from_le_bytes([header[0xAC], header[0xAD], header[0xAE], header[0xAF]]);

        // The rom address space repeats every 32 MB; NES classics ('F' game code) mirror
        // at their (power of two) rom size instead (NooDS cartridge.cpp loadRom)
        let rom_mask = if header[0xAC] == b'F' {
            ((rom_len as usize).next_power_of_two() - 1) as u32
        } else {
            crate::core::memory::regions::ROM_MASK
        };

        Ok(CartridgeIo {
            file_path,
            file_name,
            rom_len,
            rom_mask,
            title,
            game_code,
            // Filled by detect_and_load_save once the rom is in shm.
            save_type: SaveType::None,
            has_rtc: false,
            save_file_path,
            save_buf: RwLock::new(Vec::new()),
            save_dirty: AtomicBool::new(false),
        })
    }

    /// Scan the loaded rom (already in shm) for the save-type / RTC signatures and load
    /// the save file. Called once, right after the rom is streamed in.
    pub fn detect_and_load_save(&mut self, rom: &[u8]) {
        self.save_type = if find_string(rom, "EEPROM_V") {
            SaveType::Eeprom
        } else if find_string(rom, "SRAM_V") {
            SaveType::Sram
        } else if find_string(rom, "FLASH1M_V") {
            SaveType::Flash128k
        } else if find_string(rom, "FLASH_V") || find_string(rom, "FLASH512_V") {
            SaveType::Flash64k
        } else {
            SaveType::None
        };
        self.has_rtc = find_string(rom, "SIIRTC_V");

        let mut save_buf = vec![0xFF; self.save_type.initial_size()];
        match File::open(&self.save_file_path) {
            Ok(mut file) => {
                let mut content = Vec::new();
                if file.read_to_end(&mut content).is_ok() && !content.is_empty() {
                    if content.len() > save_buf.len() {
                        save_buf = content;
                    } else {
                        save_buf[..content.len()].copy_from_slice(&content);
                    }
                }
            }
            Err(_) => {
                info_println!("No save file found at {:?}", self.save_file_path);
            }
        }
        *self.save_buf.write().unwrap() = save_buf;

        info_println!("Loaded {}: title {}, save {:?}, rtc {}, rom size {}", self.file_name, self.title, self.save_type, self.has_rtc, self.rom_len);
    }

    pub fn read_save_buf(&self, addr: u32) -> u8 {
        let save_buf = self.save_buf.read().unwrap();
        if save_buf.is_empty() {
            return 0xFF;
        }
        save_buf[addr as usize & (save_buf.len() - 1)]
    }

    pub fn write_save_buf(&self, addr: u32, value: u8) {
        let mut save_buf = self.save_buf.write().unwrap();
        if save_buf.is_empty() {
            return;
        }
        let len = save_buf.len();
        save_buf[addr as usize & (len - 1)] = value;
        self.save_dirty.store(true, Ordering::Release);
    }

    pub fn resize_save_buf(&self, new_size: usize) {
        let mut save_buf = self.save_buf.write().unwrap();
        save_buf.resize(new_size, 0xFF);
        self.save_dirty.store(true, Ordering::Release);
    }

    pub fn flush_save_buf(&mut self, last_save_time: &Arc<Mutex<Option<(Instant, bool)>>>) {
        if self.save_dirty.swap(false, Ordering::AcqRel) {
            let save_buf = self.save_buf.read().unwrap();
            let result = std::fs::write(&self.save_file_path, save_buf.as_slice());
            let mut last_save_time = last_save_time.lock().unwrap();
            *last_save_time = Some((Instant::now(), result.is_err()));
            if let Err(err) = result {
                info_println!("Failed to write save file {:?}: {err}", self.save_file_path);
            }
        }
    }
}
