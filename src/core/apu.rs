use crate::core::cycle_manager::EventType;
use crate::core::memory::dma::DmaTransferMode;
use crate::core::emu::Emu;
use crate::presenter::{PRESENTER_AUDIO_OUT_BUF_SIZE, PRESENTER_AUDIO_OUT_SAMPLE_RATE};
use crate::savestate::Savestate;
use crate::soundtouch::SoundTouch;
use crate::utils::{array_init, HeapArrayU32};
use std::cmp::min;
use std::hint::assert_unchecked;
use std::intrinsics::unlikely;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::Thread;
use std::time::Duration;
use std::{slice, thread};

pub const SAMPLE_RATE: usize = 32768;
pub const SAMPLE_BUFFER_SIZE: usize = SAMPLE_RATE * PRESENTER_AUDIO_OUT_BUF_SIZE / PRESENTER_AUDIO_OUT_SAMPLE_RATE;

// The GBA system clock is 2^24 Hz; one output sample every 512 cycles = 32768 Hz.
// This event cadence doubles as the frame limiter (SoundSampler::push parks the cpu
// thread when the queues are full at the current framelimit) — it must run from boot.
pub const CYCLES_PER_SAMPLE: u32 = 512;

pub struct SoundSampler {
    queues: [(HeapArrayU32<SAMPLE_BUFFER_SIZE>, u16); 2],
    busy_queue: usize,
    ready_queue: usize,
    waiting: bool,
    busy: AtomicBool,
    sound_touch: SoundTouch,
    last_sample: u32,
    stretch_ratio: f32,
    average_size: f32,
    size_count: f32,
    cond_mutex: Mutex<bool>,
    condvar: Condvar,
}

impl SoundSampler {
    pub fn new() -> SoundSampler {
        let mut sound_touch = SoundTouch::new();
        sound_touch.set_channels(2);
        sound_touch.set_sample_rate(SAMPLE_RATE);
        sound_touch.set_pitch(1.0);
        sound_touch.set_tempo(1.0);
        SoundSampler {
            queues: array_init!({(HeapArrayU32::default(), 0)}; 2),
            busy_queue: 0,
            ready_queue: 0,
            waiting: false,
            busy: AtomicBool::new(false),
            sound_touch,
            last_sample: 0,
            stretch_ratio: 1.0,
            average_size: 0.0,
            size_count: 0.0,
            cond_mutex: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    pub fn init(&mut self) {
        self.busy_queue = 0;
        self.ready_queue = 0;
        self.waiting = false;
        self.busy.store(false, Ordering::SeqCst);
        self.sound_touch.clear();
        self.last_sample = 0;
        self.stretch_ratio = 1.0;
        self.average_size = 0.0;
        self.size_count = 0.0;
        *self.cond_mutex.lock().unwrap() = false;
    }

    #[inline(always)]
    fn push(&mut self, sample: u32, framelimit: u8, audio_stretching: bool) {
        while self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::Acquire).is_err() {}

        unsafe { assert_unchecked(self.busy_queue <= 1) };
        let (queue, size) = &mut self.queues[self.busy_queue];
        if *size < SAMPLE_BUFFER_SIZE as u16 {
            queue[*size as usize] = sample;
        }
        *size += 1;

        const SAMPLE_LIMITS: [u16; 10] = [
            SAMPLE_BUFFER_SIZE as u16,
            SAMPLE_BUFFER_SIZE as u16,
            (SAMPLE_BUFFER_SIZE * 125 / 100) as u16,
            (SAMPLE_BUFFER_SIZE * 150 / 100) as u16,
            (SAMPLE_BUFFER_SIZE * 175 / 100) as u16,
            (SAMPLE_BUFFER_SIZE * 200 / 100) as u16,
            (SAMPLE_BUFFER_SIZE * 250 / 100) as u16,
            (SAMPLE_BUFFER_SIZE * 3) as u16,
            (SAMPLE_BUFFER_SIZE * 4) as u16,
            (SAMPLE_BUFFER_SIZE * 5) as u16,
        ];

        let sample_limit = unsafe { *SAMPLE_LIMITS.get_unchecked(framelimit as usize) };

        if unlikely(*size >= sample_limit) {
            let (_, other_size) = unsafe { self.queues.get_unchecked_mut(self.busy_queue ^ 1) };

            if !audio_stretching {
                let mut can_sample = self.cond_mutex.lock().unwrap();
                *can_sample = true;
                self.condvar.notify_one();
            }

            if framelimit != 0 && *other_size >= sample_limit {
                self.waiting = true;
                self.busy.store(false, Ordering::SeqCst);
                thread::park();
                return;
            } else {
                *other_size = 0;
                self.ready_queue = self.busy_queue;
                self.busy_queue ^= 1;
            }
        }

        self.busy.store(false, Ordering::SeqCst);
    }

    pub fn consume(&mut self, cpu_thread: &Thread, buf: &mut [u32; SAMPLE_BUFFER_SIZE], ret: &mut [u32; PRESENTER_AUDIO_OUT_BUF_SIZE], audio_stretching: bool) {
        if !audio_stretching {
            let can_sample = self.cond_mutex.lock().unwrap();
            let (mut can_sample, timeout_result) = self.condvar.wait_timeout_while(can_sample, Duration::from_millis(500), |can_sample| !*can_sample).unwrap();
            if timeout_result.timed_out() {
                ret.fill(0);
                return;
            }
            *can_sample = false;
        }

        while self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::Acquire).is_err() {}

        let ready_queue = self.ready_queue;
        let (queue, queue_size) = &mut self.queues[ready_queue];
        let mut size = *queue_size as usize;
        size = min(size, SAMPLE_BUFFER_SIZE);
        *queue_size = 0;
        buf[..size].copy_from_slice(&queue[..size]);
        self.ready_queue = self.busy_queue;

        if self.waiting {
            self.waiting = false;
            self.busy_queue = ready_queue;
            cpu_thread.unpark();
        }

        self.busy.store(false, Ordering::SeqCst);

        if audio_stretching {
            // Taken from https://github.com/dolphin-emu/dolphin/blob/b5be399fd4175eb6c4ba83201bd4866b357b3200/Source/Core/AudioCommon/AudioStretcher.cpp#L28-L65
            // Take an average ratio so tempo doesn't change abruptly
            self.average_size += size as f32;
            self.size_count += 1.0;
            let ratio = self.average_size as f32 / self.size_count as f32 / SAMPLE_BUFFER_SIZE as f32;
            if self.size_count >= 15.0 {
                self.size_count = 0.0;
                self.average_size = 0.0;
            }
            // 80ms latency
            let max_backlog = SAMPLE_RATE as f32 * 80.0 / 1000.0;
            let backlog_fullness = self.sound_touch.num_of_samples() as f32 / max_backlog;
            if backlog_fullness > 5.0 {
                size = 0;
            }

            // Plot the function for understanding
            // In a nutshell backlog is not at 50% => slow down
            // More aggressive slow down when sample size is small
            let tweak = 1.0 + 2.0 * (backlog_fullness - 0.5) * (1.0 - ratio);
            let current_ratio = ratio * tweak;

            // The fewer samples, the smaller the lpf gain
            // Most likely the next audio frame will have more samples
            // Thus don't let it influence the ratio too much
            const LPF_TIME_SCALE: f32 = 1.0;
            let lpf_gain = 1.0 - (-ratio / LPF_TIME_SCALE).exp();
            self.stretch_ratio += lpf_gain * (current_ratio - self.stretch_ratio);

            if self.stretch_ratio < 0.05 {
                self.stretch_ratio = 0.05;
            }
            self.sound_touch.set_tempo(self.stretch_ratio as f64);

            let sound_touch_buf = unsafe { slice::from_raw_parts(buf.as_ptr() as *const i16, size << 1) };
            self.sound_touch.put_samples(sound_touch_buf, size);
            let sound_touch_buf = unsafe { slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut i16, SAMPLE_BUFFER_SIZE << 1) };
            let num_samples = self.sound_touch.receive_samples(sound_touch_buf, SAMPLE_BUFFER_SIZE);
            if num_samples == 0 {
                buf.fill(self.last_sample);
            } else if num_samples < SAMPLE_BUFFER_SIZE {
                self.last_sample = buf[num_samples - 1];
                buf[num_samples..].fill(self.last_sample);
            }
        }

        for i in 0..PRESENTER_AUDIO_OUT_BUF_SIZE {
            ret[i] = unsafe { *buf.get_unchecked(i * SAMPLE_BUFFER_SIZE / PRESENTER_AUDIO_OUT_BUF_SIZE) };
        }
    }
}

// GBA APU, ported from NooDS (spu.cpp runGbaSample/gbaFifoTimer/writeGba* as the spec):
// four GB-style PSG channels (two tones with sweep/envelope, wave RAM, noise) plus the
// two 8-bit DirectSound FIFOs fed by timer-paced DMA. One stereo sample every 512 cycles.
#[derive(Savestate)]
pub struct Apu {
    pub sound_bias: u16,
    // Per-channel PSG registers: cnt_l exists only for tone 1 (sweep) and wave (bank/enable)
    cnt_l: [u8; 2],
    cnt_h: [u16; 4],
    cnt_x: [u16; 4],
    main_cnt_l: u16,
    main_cnt_h: u16,
    // Master enable in bit 7, per-channel active flags in bits 0-3
    main_cnt_x: u8,
    sound_timers: [i32; 4],
    // Envelope state: [tone1, tone2, noise] (the wave channel has no envelope)
    envelopes: [u8; 3],
    env_timers: [i32; 3],
    sweep_timer: i32,
    wave_digit: u8,
    noise_value: u16,
    wave_ram: [[u8; 16]; 2],
    // 512Hz frame sequencer phase, incremented per 32768Hz sample (see runGbaSample)
    frame_sequencer: u16,
    // DirectSound FIFOs: 32-byte rings + the sample currently latched at the DAC
    fifo: [[i8; 32]; 2],
    fifo_head: [u8; 2],
    fifo_len: [u8; 2],
    sample_ab: [i8; 2],
    #[savestate(skip)]
    sound_sampler: NonNull<SoundSampler>,
}

impl Apu {
    pub fn new(sound_sampler: NonNull<SoundSampler>) -> Self {
        Apu {
            sound_bias: 0,
            cnt_l: [0; 2],
            cnt_h: [0; 4],
            cnt_x: [0; 4],
            main_cnt_l: 0,
            main_cnt_h: 0,
            main_cnt_x: 0,
            sound_timers: [0; 4],
            envelopes: [0; 3],
            env_timers: [0; 3],
            sweep_timer: 0,
            wave_digit: 0,
            noise_value: 0,
            wave_ram: [[0; 16]; 2],
            frame_sequencer: 0,
            fifo: [[0; 32]; 2],
            fifo_head: [0; 2],
            fifo_len: [0; 2],
            sample_ab: [0; 2],
            sound_sampler,
        }
    }

    pub fn init(&mut self) {
        let sampler = self.sound_sampler;
        *self = Apu::new(sampler);
    }

    fn master_enabled(&self) -> bool {
        self.main_cnt_x & 0x80 != 0
    }

    pub fn read_cnt_l(&self, channel: usize) -> u8 {
        // Only two of these exist, on the tone-sweep and wave channels
        self.cnt_l[channel / 2]
    }

    pub fn read_cnt_h(&self, channel: usize) -> u16 {
        // The sound length is write-only
        self.cnt_h[channel] & !(if channel == 2 { 0x00FF } else { 0x003F })
    }

    pub fn read_cnt_x(&self, channel: usize) -> u16 {
        // The frequency is write-only (the noise divisor bits read back)
        self.cnt_x[channel] & !(if channel == 3 { 0x0000 } else { 0x07FF })
    }

    pub fn read_main_cnt_l(&self) -> u16 {
        self.main_cnt_l
    }

    pub fn read_main_cnt_h(&self) -> u16 {
        self.main_cnt_h
    }

    pub fn read_main_cnt_x(&self) -> u8 {
        self.main_cnt_x
    }

    pub fn read_wave_ram(&self, index: usize) -> u8 {
        // The bank currently selected for playback is inaccessible
        self.wave_ram[(self.cnt_l[1] as usize >> 6) & 1 ^ 1][index]
    }

    pub fn write_cnt_l(&mut self, channel: usize, value: u8) {
        if !self.master_enabled() {
            return;
        }
        let mask = if channel == 0 { 0x7F } else { 0xE0 };
        let slot = channel / 2;
        self.cnt_l[slot] = (self.cnt_l[slot] & !mask) | (value & mask);
    }

    pub fn write_cnt_h(&mut self, channel: usize, mut mask: u16, value: u16) {
        if !self.master_enabled() {
            return;
        }
        match channel {
            2 => mask &= 0xE0FF,
            3 => mask &= 0xFF3F,
            _ => {}
        }
        self.cnt_h[channel] = (self.cnt_h[channel] & !mask) | (value & mask);
    }

    pub fn write_cnt_x(&mut self, channel: usize, mut mask: u16, value: u16) {
        if !self.master_enabled() {
            return;
        }
        mask &= if channel == 3 { 0x40FF } else { 0x47FF };
        self.cnt_x[channel] = (self.cnt_x[channel] & !mask) | (value & mask);

        // Restart the channel
        if value & (1 << 15) == 0 {
            return;
        }
        self.main_cnt_x |= 1 << channel;
        match channel {
            0 | 1 => {
                if channel == 0 {
                    self.sweep_timer = ((self.cnt_l[0] & 0x70) >> 4) as i32;
                }
                self.envelopes[channel] = ((self.cnt_h[channel] & 0xF000) >> 12) as u8;
                self.env_timers[channel] = ((self.cnt_h[channel] & 0x0700) >> 8) as i32;
                self.sound_timers[channel] = 2048 - (self.cnt_x[channel] & 0x07FF) as i32;
            }
            2 => {
                self.wave_digit = 0;
                self.sound_timers[2] = 2048 - (self.cnt_x[2] & 0x07FF) as i32;
            }
            _ => {
                self.noise_value = if self.cnt_h[3] & (1 << 3) != 0 { 0x40 } else { 0x4000 };
                self.envelopes[2] = ((self.cnt_h[3] & 0xF000) >> 12) as u8;
                self.env_timers[2] = ((self.cnt_h[3] & 0x0700) >> 8) as i32;
                let mut divisor = (self.cnt_x[3] & 0x0007) as i32 * 16;
                if divisor == 0 {
                    divisor = 8;
                }
                self.sound_timers[3] = divisor << ((self.cnt_x[3] & 0x00F0) >> 4);
            }
        }
    }

    pub fn write_main_cnt_l(&mut self, mut mask: u16, value: u16) {
        if !self.master_enabled() {
            return;
        }
        mask &= 0xFF77;
        self.main_cnt_l = (self.main_cnt_l & !mask) | (value & mask);
    }

    pub fn write_main_cnt_h(&mut self, mut mask: u16, value: u16) {
        mask &= 0x770F;
        self.main_cnt_h = (self.main_cnt_h & !mask) | (value & mask);

        // FIFO reset bits
        if value & (1 << 11) != 0 {
            self.fifo_len[0] = 0;
            self.fifo_head[0] = 0;
        }
        if value & (1 << 15) != 0 {
            self.fifo_len[1] = 0;
            self.fifo_head[1] = 0;
        }
    }

    pub fn write_main_cnt_x(&mut self, value: u8) {
        self.main_cnt_x = (self.main_cnt_x & !0x80) | (value & 0x80);

        // Reset the PSG channels when disabled
        if !self.master_enabled() {
            self.cnt_l = [0; 2];
            self.cnt_h = [0; 4];
            self.cnt_x = [0; 4];
            self.main_cnt_l = 0;
            self.main_cnt_x &= !0x0F;
            self.frame_sequencer = 0;
        }
    }

    pub fn write_sound_bias(&mut self, mut mask: u16, value: u16) {
        mask &= 0xC3FE;
        self.sound_bias = (self.sound_bias & !mask) | (value & mask);
    }

    pub fn write_wave_ram(&mut self, index: usize, value: u8) {
        // Writes land in the currently inactive bank
        self.wave_ram[(self.cnt_l[1] as usize >> 6) & 1 ^ 1][index] = value;
    }

    pub fn write_fifo(&mut self, fifo: usize, mask: u32, value: u32) {
        for i in (0..32).step_by(8) {
            if self.fifo_len[fifo] < 32 && mask & (0xFF << i) != 0 {
                let tail = (self.fifo_head[fifo] + self.fifo_len[fifo]) % 32;
                self.fifo[fifo][tail as usize] = (value >> i) as i8;
                self.fifo_len[fifo] += 1;
            }
        }
    }

    pub fn fifo_len(&self, fifo: usize) -> u8 {
        self.fifo_len[fifo]
    }

    fn fifo_pop(&mut self, fifo: usize) -> Option<i8> {
        if self.fifo_len[fifo] == 0 {
            return None;
        }
        let value = self.fifo[fifo][self.fifo_head[fifo] as usize];
        self.fifo_head[fifo] = (self.fifo_head[fifo] + 1) % 32;
        self.fifo_len[fifo] -= 1;
        Some(value)
    }

    /// One 32768Hz stereo sample; the NooDS runGbaSample logic verbatim.
    fn run_sample(&mut self) -> u32 {
        let mut sample_left = 0i32;
        let mut sample_right = 0i32;

        if self.master_enabled() {
            let mut data = [0i32; 4];

            // Tone channels
            for i in 0..2 {
                if self.main_cnt_x & (1 << i) == 0 {
                    continue;
                }

                // Frequency sweeper at 128Hz (first channel only)
                if i == 0 && self.frame_sequencer % 256 == 128 && self.cnt_l[0] & 0x70 != 0 {
                    self.sweep_timer -= 1;
                    if self.sweep_timer <= 0 {
                        let frequency = (self.cnt_x[0] & 0x07FF) as i32;
                        let mut sweep = frequency >> (self.cnt_l[0] & 0x07);
                        if self.cnt_l[0] & (1 << 3) != 0 {
                            sweep = -sweep;
                        }
                        let frequency = frequency + sweep;
                        if frequency < 0x800 {
                            self.cnt_x[0] = (self.cnt_x[0] & !0x07FF) | frequency as u16;
                            self.sweep_timer = ((self.cnt_l[0] & 0x70) >> 4) as i32;
                        } else {
                            // Disable the channel if the frequency overflows
                            self.main_cnt_x &= !(1 << i);
                            continue;
                        }
                    }
                }

                // Decrement and reload the sound timer
                self.sound_timers[i] -= 4;
                while self.sound_timers[i] <= 0 {
                    self.sound_timers[i] += 2048 - (self.cnt_x[i] & 0x07FF) as i32;
                }

                // Duty cycle switch point
                let period = 2048 - (self.cnt_x[i] & 0x07FF) as i32;
                let duty = match (self.cnt_h[i] & 0x00C0) >> 6 {
                    0 => period * 7 / 8,
                    1 => period * 6 / 8,
                    2 => period * 4 / 8,
                    _ => period * 2 / 8,
                };
                data[i] = if self.sound_timers[i] < duty { -0x80 } else { 0x80 };

                // Length counter at 256Hz
                if self.frame_sequencer % 128 == 0 && self.cnt_x[i] & (1 << 14) != 0 && self.cnt_h[i] & 0x003F != 0 {
                    self.cnt_h[i] = (self.cnt_h[i] & !0x003F) | ((self.cnt_h[i] & 0x003F) - 1);
                    if self.cnt_h[i] & 0x003F == 0 {
                        self.main_cnt_x &= !(1 << i);
                    }
                }

                // Envelope timer at 64Hz
                if self.frame_sequencer == 448 {
                    self.env_timers[i] -= 1;
                    if self.env_timers[i] <= 0 {
                        if self.env_timers[i] == 0 {
                            if self.cnt_h[i] & (1 << 11) != 0 && self.envelopes[i] < 15 {
                                self.envelopes[i] += 1;
                            } else if self.cnt_h[i] & (1 << 11) == 0 && self.envelopes[i] > 0 {
                                self.envelopes[i] -= 1;
                            }
                        } else {
                            // The envelope seems to reset with a period of zero
                            self.envelopes[i] = ((self.cnt_h[i] & 0xF000) >> 12) as u8;
                        }
                        self.env_timers[i] = ((self.cnt_h[i] & 0x0700) >> 8) as i32;
                    }
                }

                data[i] = data[i] * self.envelopes[i] as i32 / 15;
            }

            // Wave channel
            if self.main_cnt_x & (1 << 2) != 0 && self.cnt_l[1] & (1 << 7) != 0 {
                // Each timer reload advances the wave digit
                self.sound_timers[2] -= 64;
                while self.sound_timers[2] <= 0 {
                    self.sound_timers[2] += 2048 - (self.cnt_x[2] & 0x07FF) as i32;
                    self.wave_digit = (self.wave_digit + 1) % 64;
                }

                // Bank select; in 64-digit dimension the second bank plays after the first 32
                let mut bank = ((self.cnt_l[1] >> 6) & 1) as usize;
                if self.cnt_l[1] & (1 << 5) != 0 && self.wave_digit >= 32 {
                    bank ^= 1;
                }

                let byte = self.wave_ram[bank][(self.wave_digit as usize % 32) / 2] as i32;
                data[2] = if self.wave_digit & 1 != 0 { byte & 0x0F } else { byte >> 4 };

                // Length counter at 256Hz
                if self.frame_sequencer % 128 == 0 && self.cnt_x[2] & (1 << 14) != 0 && self.cnt_h[2] & 0x00FF != 0 {
                    self.cnt_h[2] = (self.cnt_h[2] & !0x00FF) | ((self.cnt_h[2] & 0x00FF) - 1);
                    if self.cnt_h[2] & 0x00FF == 0 {
                        self.main_cnt_x &= !(1 << 2);
                    }
                }

                // Volume shift; bit 15 forces 75%
                data[2] = match (self.cnt_h[2] & 0xE000) >> 13 {
                    0 => data[2] >> 4,
                    1 => data[2],
                    2 => data[2] >> 1,
                    3 => data[2] >> 2,
                    _ => data[2] * 3 / 4,
                };

                // Expand the 4-bit sample to 8-bit range
                data[2] = data[2] * 0x100 / 0xF;
            }

            // Noise channel
            if self.main_cnt_x & (1 << 3) != 0 {
                // Each timer reload advances the LFSR
                self.sound_timers[3] -= 16;
                while self.sound_timers[3] <= 0 {
                    let mut divisor = (self.cnt_x[3] & 0x0007) as i32 * 16;
                    if divisor == 0 {
                        divisor = 8;
                    }
                    self.sound_timers[3] += divisor << ((self.cnt_x[3] & 0x00F0) >> 4);

                    // Advance and save the carry bit in bit 15
                    self.noise_value &= !(1 << 15);
                    if self.noise_value & 1 != 0 {
                        self.noise_value = (1 << 15) | ((self.noise_value >> 1) ^ (if self.cnt_h[3] & (1 << 3) != 0 { 0x60 } else { 0x6000 }));
                    } else {
                        self.noise_value >>= 1;
                    }
                }

                data[3] = if self.noise_value & (1 << 15) != 0 { 0x80 } else { -0x80 };

                // Length counter at 256Hz
                if self.frame_sequencer % 128 == 0 && self.cnt_x[3] & (1 << 14) != 0 && self.cnt_h[3] & 0x003F != 0 {
                    self.cnt_h[3] = (self.cnt_h[3] & !0x003F) | ((self.cnt_h[3] & 0x003F) - 1);
                    if self.cnt_h[3] & 0x003F == 0 {
                        self.main_cnt_x &= !(1 << 3);
                    }
                }

                // Envelope timer at 64Hz (shares slot 2 with nothing — wave has no envelope)
                if self.frame_sequencer == 448 {
                    self.env_timers[2] -= 1;
                    if self.env_timers[2] <= 0 {
                        if self.env_timers[2] == 0 {
                            if self.cnt_h[3] & (1 << 11) != 0 && self.envelopes[2] < 15 {
                                self.envelopes[2] += 1;
                            } else if self.cnt_h[3] & (1 << 11) == 0 && self.envelopes[2] > 0 {
                                self.envelopes[2] -= 1;
                            }
                        } else {
                            self.envelopes[2] = ((self.cnt_h[3] & 0xF000) >> 12) as u8;
                        }
                        self.env_timers[2] = ((self.cnt_h[3] & 0x0700) >> 8) as i32;
                    }
                }

                data[3] = data[3] * self.envelopes[2] as i32 / 15;
            }

            // Mix the PSG channels (max +/-0x80 each)
            for (i, mut value) in data.into_iter().enumerate() {
                value >>= match self.main_cnt_h & 0x0003 {
                    0 => 2,
                    1 => 1,
                    _ => 0,
                };
                if self.main_cnt_l & (1 << (12 + i)) != 0 {
                    sample_left += value * ((self.main_cnt_l & 0x0070) >> 4) as i32 / 7;
                }
                if self.main_cnt_l & (1 << (8 + i)) != 0 {
                    sample_right += value * (self.main_cnt_l & 0x0007) as i32 / 7;
                }
            }

            // Mix the FIFO channels (max +/-0x200 at full volume)
            let shift_a = if self.main_cnt_h & (1 << 2) != 0 { 2 } else { 1 };
            if self.main_cnt_h & (1 << 9) != 0 {
                sample_left += (self.sample_ab[0] as i32) << shift_a;
            }
            if self.main_cnt_h & (1 << 8) != 0 {
                sample_right += (self.sample_ab[0] as i32) << shift_a;
            }
            let shift_b = if self.main_cnt_h & (1 << 3) != 0 { 2 } else { 1 };
            if self.main_cnt_h & (1 << 13) != 0 {
                sample_left += (self.sample_ab[1] as i32) << shift_b;
            }
            if self.main_cnt_h & (1 << 12) != 0 {
                sample_right += (self.sample_ab[1] as i32) << shift_b;
            }

            // 512Hz frame sequencer, stepped once per 32768Hz sample
            self.frame_sequencer = (self.frame_sequencer + 1) % 512;
        }

        // Apply the sound bias, clip to the 10-bit DAC range and expand to signed 16-bit
        let left = ((sample_left + (self.sound_bias & 0x3FF) as i32).clamp(0, 0x3FF) - 0x200) << 6;
        let right = ((sample_right + (self.sound_bias & 0x3FF) as i32).clamp(0, 0x3FF) - 0x200) << 6;
        (left as u16 as u32) | ((right as u16 as u32) << 16)
    }
}

impl Emu {
    pub fn apu_initialize_schedule(&mut self) {
        self.cm.schedule(CYCLES_PER_SAMPLE, EventType::ApuSample);
    }

    pub fn apu_on_sample_event(&mut self) {
        let sample = self.apu.run_sample();
        let framelimit = self.settings.framelimit();
        let audio_stretching = self.settings.audio_stretching();
        unsafe { self.apu.sound_sampler.as_mut().push(sample, framelimit, audio_stretching) };
        self.cm.schedule_from_due(CYCLES_PER_SAMPLE, EventType::ApuSample);
    }

    /// Timer 0/1 overflow: latch the next FIFO byte for each DirectSound channel driven by
    /// this timer and request a sound DMA refill when a FIFO runs half empty (NooDS
    /// gbaFifoTimer).
    pub fn apu_on_fifo_timer(&mut self, timer: usize) {
        for fifo in 0..2 {
            let timer_select_bit = if fifo == 0 { 10 } else { 14 };
            if ((self.apu.main_cnt_h >> timer_select_bit) & 1) as usize != timer {
                continue;
            }
            if let Some(value) = self.apu.fifo_pop(fifo) {
                self.apu.sample_ab[fifo] = value;
            }
            if self.apu.fifo_len[fifo] <= 16 {
                self.dma_trigger(DmaTransferMode::Special, 1 << (fifo + 1));
            }
        }
    }
}
