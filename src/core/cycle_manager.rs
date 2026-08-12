use crate::core::cycle_manager::EventType::{Last, Timer0};
use crate::core::cycle_manager::ImmEventType::{CpuInterrupt, Dma0, Dma3};
use crate::core::emu::Emu;
use crate::savestate::Savestate;
use std::cmp::max;
use std::intrinsics::{likely, unlikely};
use std::mem;

#[repr(u8)]
#[derive(Copy, Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum ImmEventType {
    CpuInterrupt = 0,
    Dma0 = 1,
    Dma1 = 2,
    Dma2 = 3,
    Dma3 = 4,
}

impl ImmEventType {
    pub fn cpu_interrupt() -> Self {
        CpuInterrupt
    }

    pub fn dma(channel_num: u8) -> Self {
        ImmEventType::from(Dma0 as u8 + channel_num)
    }
}

impl From<u8> for ImmEventType {
    fn from(value: u8) -> Self {
        debug_assert!(value <= Dma3 as u8);
        unsafe { mem::transmute(value) }
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum EventType {
    GpuScanline240 = 0,
    GpuScanline308 = 1,
    // Timers outrank the apu sample event on due-cycle ties: a timer overflow clocks its
    // FIFO byte onto the DAC on the edge, and a sample taken at that same cycle must see
    // the new byte (matches NooDS' dispatch order).
    Timer0 = 2,
    Timer1 = 3,
    Timer2 = 4,
    Timer3 = 5,
    ApuSample = 6,
    Last = 7,
}

impl EventType {
    pub fn timer(channel_num: u8) -> Self {
        EventType::from(Timer0 as u8 + channel_num)
    }
}

impl From<u8> for EventType {
    fn from(value: u8) -> Self {
        debug_assert!(value < Last as u8);
        unsafe { mem::transmute(value) }
    }
}

#[derive(Savestate)]
pub struct CycleManager {
    cycle_count: u32,
    events: [u32; Last as usize],
    next_event_cycle: u32,
    active_events: u32,
    active_imm_events: u32,
    // Due cycle of the event currently being dispatched, valid only inside a handler.
    // Periodic handlers reschedule via schedule_from_due instead of the dispatch-time
    // cycle_count: the jit overshoots the due cycle by the slice remainder, and
    // rescheduling from the overshot count made every periodic grid drift (SPU sample
    // cadence vs ARM7 timers), desyncing the capture-based surround loop games run on
    // the ARM7 sound driver (audio crackle in e.g. Animal Crossing, Star Wars Ep3).
    #[savestate(skip)]
    current_event_due: u32,
}

impl CycleManager {
    pub fn new() -> Self {
        CycleManager {
            cycle_count: 0,
            events: [0; Last as usize],
            next_event_cycle: u32::MAX,
            active_events: 0,
            active_imm_events: 0,
            current_event_due: 0,
        }
    }

    pub fn init(&mut self) {
        self.cycle_count = 0;
        self.events = [0; Last as usize];
        self.next_event_cycle = u32::MAX;
        self.active_events = 0;
        self.active_imm_events = 0;
        self.current_event_due = 0;
    }

    pub fn add_cycles(&mut self, cycle_count: u16) {
        self.cycle_count += cycle_count as u32;
    }

    pub fn get_cycles(&self) -> u32 {
        self.cycle_count
    }

    pub fn schedule_imm(&mut self, event_type: ImmEventType) {
        self.active_imm_events |= 1 << (31 - event_type as u8);
    }

    pub fn schedule(&mut self, in_cycles: u32, event_type: EventType) {
        let mut in_cycles = max(in_cycles, 1);
        if unlikely(u32::MAX - in_cycles < self.cycle_count) {
            in_cycles = u32::MAX - self.cycle_count;
        }
        let event_cycle = self.cycle_count + in_cycles;
        self.events[event_type as usize] = event_cycle;
        self.active_events |= 1 << (31 - event_type as u8);
        if event_cycle < self.next_event_cycle {
            self.next_event_cycle = event_cycle;
        }
    }

    // Reschedule a periodic event relative to its due cycle instead of the (overshot)
    // dispatch-time cycle_count, keeping the event grid drift-free. Only valid while
    // dispatching that event. A due cycle already in the past fires on the next check,
    // so a late slice catches up instead of stretching the period.
    pub fn schedule_from_due(&mut self, in_cycles: u32, event_type: EventType) {
        debug_assert!(in_cycles >= 1);
        let event_cycle = self.current_event_due.saturating_add(in_cycles);
        self.events[event_type as usize] = event_cycle;
        self.active_events |= 1 << (31 - event_type as u8);
        if event_cycle < self.next_event_cycle {
            self.next_event_cycle = event_cycle;
        }
    }

    pub fn current_event_due(&self) -> u32 {
        self.current_event_due
    }

    pub fn jump_to_next_event(&mut self) {
        // A catch-up reschedule (schedule_from_due after a late slice) can leave
        // next_event_cycle in the past — never move the clock backwards.
        if self.next_event_cycle > self.cycle_count {
            self.cycle_count = self.next_event_cycle;
        }
    }
}

impl Emu {
    pub fn cm_check_events(&mut self) -> bool {
        const IMM_LUT: [fn(&mut Emu); Dma3 as usize + 1] = [
            Emu::cpu_on_interrupt_event,
            Emu::dma_on_event0,
            Emu::dma_on_event1,
            Emu::dma_on_event2,
            Emu::dma_on_event3,
        ];

        const LUT: [fn(&mut Emu); Last as usize] = [
            Emu::gpu_on_scanline240_event,
            Emu::gpu_on_scanline308_event,
            Emu::timers_on_overflow_event::<0>,
            Emu::timers_on_overflow_event::<1>,
            Emu::timers_on_overflow_event::<2>,
            Emu::timers_on_overflow_event::<3>,
            Emu::apu_on_sample_event,
        ];

        let mut active_imm_events = self.cm.active_imm_events;
        self.cm.active_imm_events = 0;
        let mut offset = 0;
        while active_imm_events != 0 {
            let zeros = active_imm_events.leading_zeros();
            let event_index = (zeros + offset) as usize;

            let func = unsafe { IMM_LUT.get_unchecked(event_index) };
            func(self);

            active_imm_events <<= zeros + 1;
            offset += zeros + 1;
        }

        if likely(self.cm.cycle_count < self.cm.next_event_cycle) {
            return false;
        }

        // Dispatch every due event in DUE-CYCLE order, not table order: the jit drains
        // events in up-to-quantum batches, and within a batch a timer overflow due before
        // the apu sample event must pop its FIFO byte before the sample reads the latch —
        // index-ordered dispatch displaced DirectSound transitions by one output sample
        // (audible wideband jitter on pcm audio). Ties break by table index. Handlers may
        // reschedule themselves into the same batch (catch-up), so re-scan until nothing
        // is due.
        self.cm.next_event_cycle = u32::MAX;
        loop {
            // Read per round instead of hoisting out of the loop: a handler can replace
            // the whole scheduler under this scan — the vblank hook is where savestate
            // loads are applied, and cycle_count is a free-running absolute counter, so
            // the restored clock bears no relation to this session's. Judged against a
            // hoisted (pre-load) count, every restored event reads as due and the
            // re-scan grinds the entire grid forward to catch up; the apu sample event
            // parks the cpu thread on a full queue while doing it, so it never ends.
            let cycle_count = self.cm.cycle_count;
            let mut best_index = usize::MAX;
            let mut best_cycle = u32::MAX;
            let mut active_events = self.cm.active_events;
            let mut offset = 0;
            while active_events != 0 {
                let zeros = active_events.leading_zeros();
                let event_index = (zeros + offset) as usize;
                let event_cycle = unsafe { *self.cm.events.get_unchecked(event_index) };
                if event_cycle <= cycle_count && event_cycle < best_cycle {
                    best_cycle = event_cycle;
                    best_index = event_index;
                }
                active_events <<= zeros + 1;
                offset += zeros + 1;
            }
            if best_index == usize::MAX {
                break;
            }
            self.cm.active_events &= !(1 << (31 - best_index));
            // The due slot is only overwritten once its handler reschedules, so it still
            // holds this event's due cycle here; schedule_from_due anchors to it.
            self.cm.current_event_due = best_cycle;
            let func = unsafe { LUT.get_unchecked(best_index) };
            func(self);
        }

        // Handlers rescheduled their events; recompute the next due cycle.
        {
            let mut active_events = self.cm.active_events;
            let mut offset = 0;
            while active_events != 0 {
                let zeros = active_events.leading_zeros();
                let event_index = (zeros + offset) as usize;
                let event_cycle = unsafe { *self.cm.events.get_unchecked(event_index) };
                if event_cycle < self.cm.next_event_cycle {
                    self.cm.next_event_cycle = event_cycle;
                }
                active_events <<= zeros + 1;
                offset += zeros + 1;
            }
        }

        // Re-read rather than reusing the local: a savestate load in a handler above
        // swapped the clock out.
        if unlikely(self.cm.cycle_count > 0x7FFFFFFF) {
            self.cm_on_overflow_event();
        }
        true
    }

    #[inline(never)]
    fn cm_on_overflow_event(&mut self) {
        for i in 0..self.cm.events.len() {
            if self.cm.active_events & (1 << (31 - i)) != 0 {
                self.cm.events[i] -= self.cm.cycle_count;
            }
        }
        self.cm.next_event_cycle -= self.cm.cycle_count;
        {
            for channel in &mut self.timers.channels {
                if channel.scheduled_cycle < self.cm.cycle_count {
                    channel.scheduled_cycle = 0;
                } else {
                    channel.scheduled_cycle -= self.cm.cycle_count;
                }
            }
        }
        self.cm.cycle_count = 0;
    }
}
