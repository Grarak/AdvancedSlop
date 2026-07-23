// S3511 GPIO RTC (NooDS rtc.cpp GBA paths as the reference). Wired at
// 0x080000C4 (data: SCK bit0, SIO bit1, CS bit2), 0x080000C6 (direction),
// 0x080000C8 (control: bit0 enables register reads).

use crate::savestate::Savestate;

#[derive(Savestate)]
pub struct Rtc {
    cs: u8,
    sck: u8,
    sio: u8,
    write_count: u8,
    command: u8,
    control: u8,
    date_time: [u8; 7],
    pub gp_direction: u16,
    pub gp_control: u16,
}

impl Rtc {
    pub fn new() -> Self {
        Rtc {
            cs: 0,
            sck: 0,
            sio: 0,
            write_count: 0,
            command: 0,
            control: 0,
            date_time: [0; 7],
            gp_direction: 0,
            gp_control: 0,
        }
    }

    fn update_date_time(&mut self) {
        use chrono::{Datelike, Timelike};
        // Deterministic A/B trace captures: freeze the clock so two runs launched at
        // different wall times still execute identically.
        if std::env::var_os("ADVANCEDSLOP_RTC_FREEZE").is_some() {
            self.date_time = [0x26, 0x07, 0x23, 0x04, 0x12, 0x00, 0x00];
            return;
        }
        let now = chrono::Local::now();
        let year = (now.year() % 100) as u8;
        let month = now.month() as u8;
        let day = now.day() as u8;
        let weekday = now.weekday().num_days_from_sunday() as u8;
        let mut hour = now.hour() as u8;
        let pm = hour >= 12;
        // 12-hour format unless the 24-hour control bit (bit 6) is set
        if self.control & (1 << 6) == 0 {
            hour %= 12;
        }
        let minute = now.minute() as u8;
        let second = now.second() as u8;

        let bcd = |v: u8| ((v / 10) << 4) | (v % 10);
        self.date_time = [bcd(year), bcd(month), bcd(day), bcd(weekday), bcd(hour), bcd(minute), bcd(second)];
        if pm {
            self.date_time[4] |= 1 << 6;
        }
    }

    fn read_register(&mut self, index: u8) -> u8 {
        match index {
            0 => {
                // Reset
                self.control = 0;
                0
            }
            1 => (self.control >> (self.write_count & 7)) & 1,
            2 => {
                // Date and time (7 bytes)
                if self.write_count == 8 {
                    self.update_date_time();
                }
                (self.date_time[(self.write_count / 8) as usize - 1] >> (self.write_count % 8)) & 1
            }
            3 => {
                // Time only (3 bytes)
                if self.write_count == 8 {
                    self.update_date_time();
                }
                (self.date_time[(self.write_count / 8) as usize + 4 - 1] >> (self.write_count % 8)) & 1
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, index: u8, value: u8) {
        if index == 1 && (1u8 << (self.write_count & 7)) & 0x6A != 0 {
            // Control r/w bits
            self.control = (self.control & !(1 << (self.write_count & 7))) | (value << (self.write_count & 7));
        }
    }

    fn update_rtc(&mut self, cs: u8, sck: u8, mut sio: u8) {
        if cs != 0 {
            // Transfer a bit on the SCK rising edge
            if self.sck == 0 && sck != 0 {
                if self.write_count < 8 {
                    // First 8 bits form the command, MSB first; reverse if the fixed
                    // 0110 pattern arrived in the other bit order
                    self.command |= sio << (7 - self.write_count);
                    if self.write_count == 7 && (self.command & 0xF0) != 0x60 {
                        self.command = self.command.reverse_bits();
                    }
                } else if self.command & 1 != 0 {
                    sio = self.read_register((self.command >> 1) & 0x7);
                } else {
                    self.write_register((self.command >> 1) & 0x7, sio);
                }
                self.write_count = self.write_count.wrapping_add(1);
            }
        } else {
            // CS low resets the transfer
            self.write_count = 0;
            self.command = 0;
        }

        self.cs = cs;
        self.sck = sck;
        self.sio = sio;
    }

    pub fn write_gp_data(&mut self, mask: u16, value: u16) {
        if mask & 0xFF != 0 {
            let cs = if self.gp_direction & (1 << 2) != 0 { ((value >> 2) & 1) as u8 } else { self.cs };
            let sio = if self.gp_direction & (1 << 1) != 0 { ((value >> 1) & 1) as u8 } else { self.sio };
            let sck = if self.gp_direction & 1 != 0 { (value & 1) as u8 } else { self.sck };
            self.update_rtc(cs, sck, sio);
        }
    }

    pub fn read_gp_data(&self) -> u16 {
        // Only pins configured as inputs read back
        let cs = if self.gp_direction & (1 << 2) != 0 { 0 } else { self.cs };
        let sio = if self.gp_direction & (1 << 1) != 0 { 0 } else { self.sio };
        let sck = if self.gp_direction & 1 != 0 { 0 } else { self.sck };
        ((cs as u16) << 2) | ((sio as u16) << 1) | (sck as u16)
    }

    pub fn write_gp_direction(&mut self, mask: u16, value: u16) {
        let mask = mask & 0xF;
        self.gp_direction = (self.gp_direction & !mask) | (value & mask);
    }

    pub fn write_gp_control(&mut self, mask: u16, value: u16) {
        let mask = mask & 1;
        self.gp_control = (self.gp_control & !mask) | (value & mask);
    }
}
