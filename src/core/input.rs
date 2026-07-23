use crate::core::cpu_regs::InterruptFlag;
use crate::core::emu::Emu;
use crate::savestate::Savestate;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum Keycode {
    A = 0,
    B = 1,
    Select = 2,
    Start = 3,
    Right = 4,
    Left = 5,
    Up = 6,
    Down = 7,
    TriggerR = 8,
    TriggerL = 9,
}

#[derive(Savestate)]
pub struct Input {
    key_input: u16,
    pub key_cnt: u16,
    #[savestate(skip)]
    key_map: Arc<AtomicU32>,
    #[savestate(skip)]
    prev_keypad_irq: bool,
}

impl Input {
    pub fn new(key_map: Arc<AtomicU32>) -> Self {
        Input {
            key_input: 0x3FF,
            key_cnt: 0,
            key_map,
            prev_keypad_irq: false,
        }
    }

    pub fn get_key_input(&self) -> u16 {
        let key_map = self.key_map.load(Ordering::Relaxed);
        (self.key_input & !0x3FF) | (key_map & 0x3FF) as u16
    }

    pub fn set_key_cnt(&mut self, mask: u16, value: u16) {
        let mask = mask & 0xC3FF;
        self.key_cnt = (self.key_cnt & !mask) | (value & mask);
    }
}

impl Emu {
    // Runs once per frame at the vblank hook on the cpu thread. KEYCNT irq (NooDS skips
    // this; cheap and some games use it to wake from stop): edge-triggered on the
    // selected-buttons condition becoming true.
    pub fn input_process_hotkeys(&mut self) {
        let key_cnt = self.input.key_cnt;
        if key_cnt & 0x4000 != 0 {
            let pressed = !self.input.get_key_input() & 0x3FF;
            let selected = key_cnt & 0x3FF;
            let hit = if key_cnt & 0x8000 != 0 {
                selected != 0 && (pressed & selected) == selected
            } else {
                (pressed & selected) != 0
            };
            if hit && !self.input.prev_keypad_irq {
                self.cpu_send_interrupt(InterruptFlag::Keypad);
            }
            self.input.prev_keypad_irq = hit;
        } else {
            self.input.prev_keypad_irq = false;
        }
    }
}
