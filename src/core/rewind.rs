// Rewind: a ring of recent guest states that the player can step backwards through.
//
// Snapshots ride the same vblank hook savestates use (`v_count == VISIBLE_LINES`), which
// is the only point in a frame where restoring is safe — it sits between jit execute
// calls, so no compiled frame is on the stack, and every gpu event has just been
// rescheduled, so the scheduler is consistent (see gpu.rs).
//
// Unlike a savestate the ring never touches the file format: it stores the raw field-walk
// buffer, so a snapshot is a memcpy of guest ram rather than a deflate. That is what makes
// it cheap enough to run several times a second on the emulation thread.

use crate::core::emu::Emu;
use crate::savestate::SavestateContext;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the presenter while the rewind input is held, read at the vblank hook. A held
/// input rather than an edge, so it cannot use the PresentEvent path (which delivers one
/// event per poll).
static REWIND_HELD: AtomicBool = AtomicBool::new(false);

pub fn set_rewind_held(held: bool) {
    REWIND_HELD.store(held, Ordering::Relaxed);
}

pub fn rewind_held() -> bool {
    REWIND_HELD.load(Ordering::Relaxed)
}

/// Frames between snapshots. Rewinding replays one snapshot per frame, so this is also
/// the speed-up factor going backwards: 12 gives a ~5x reverse scrub, fast enough to feel
/// like rewinding and coarse enough that snapshots cost little.
const SNAPSHOT_INTERVAL: u32 = 12;

/// How much guest state the ring may hold. A GBA state measures 416 KB raw, so this is
/// ~58 snapshots (~11 s) on desktop. The Vita gets half: it has far less spare memory once
/// the rom shm, the jit pool and vitaGL's 128 MB pool are accounted for, and ~7 s of
/// history is still worth having. (Untested on hardware — lower it if the Vita runs tight.)
#[cfg(target_os = "vita")]
const MEMORY_BUDGET: usize = 16 << 20;
#[cfg(not(target_os = "vita"))]
const MEMORY_BUDGET: usize = 24 << 20;

/// Never fewer than this many, however big a state turns out to be
const MIN_SLOTS: usize = 4;

#[derive(Default)]
pub struct Rewind {
    /// Ring of raw savestate buffers, oldest..newest. Slots keep their allocation when
    /// popped so the steady state does no allocation at all.
    slots: Vec<Vec<u8>>,
    /// Spare allocations popped off the ring, ready to be filled again
    spare: Vec<Vec<u8>>,
    frames_since_snapshot: u32,
    /// Derived from the first snapshot's size, once we know it
    max_slots: usize,
}

impl Rewind {
    pub fn new() -> Self {
        Rewind::default()
    }

    /// Drop everything — a different game, or a savestate load, makes the ring's contents
    /// unrelated to what is now running.
    pub fn clear(&mut self) {
        self.spare.append(&mut self.slots);
        self.frames_since_snapshot = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl Emu {
    /// One rewind tick, at the vblank hook. Either steps the guest back one snapshot (the
    /// input is held) or records one (every SNAPSHOT_INTERVAL frames).
    pub fn rewind_on_frame(&mut self) {
        if !self.settings.rewind() {
            // Setting turned off mid-session: give the memory back
            if !self.rewind.is_empty() {
                self.rewind.clear();
                self.rewind.spare.clear();
            }
            return;
        }

        if rewind_held() {
            self.rewind_step_back();
        } else {
            self.rewind.frames_since_snapshot += 1;
            if self.rewind.frames_since_snapshot >= SNAPSHOT_INTERVAL {
                self.rewind.frames_since_snapshot = 0;
                self.rewind_snapshot();
            }
        }
    }

    fn rewind_snapshot(&mut self) {
        let buf = self.rewind.spare.pop().unwrap_or_default();
        let mut state = SavestateContext::new_save_reusing(buf);
        self.savestate(&mut state);
        let (buf, ok) = state.into_buf();
        if !ok {
            // Serialization cannot fail on a save (nothing to run out of), but never
            // leave a half-written buffer in the ring if it somehow does.
            debug_assert!(false, "rewind snapshot serialization failed");
            self.rewind.spare.push(buf);
            return;
        }

        // Sized off the first real snapshot rather than a guess at the state layout
        if self.rewind.max_slots == 0 {
            self.rewind.max_slots = (MEMORY_BUDGET / buf.len().max(1)).max(MIN_SLOTS);
            crate::logging::info_println!("Rewind: {} KB per snapshot, {} slots (~{}s)", buf.len() / 1024, self.rewind.max_slots, self.rewind.max_slots as u32 * SNAPSHOT_INTERVAL / 60);
        }

        self.rewind.slots.push(buf);
        if self.rewind.slots.len() > self.rewind.max_slots {
            // Oldest out, allocation kept
            let dropped = self.rewind.slots.remove(0);
            self.rewind.spare.push(dropped);
        }
    }

    fn rewind_step_back(&mut self) {
        // Hold past the end of the ring: sit on the oldest frame rather than resuming
        let Some(buf) = self.rewind.slots.pop() else { return };
        let mut state = SavestateContext::new_load(buf);
        self.savestate(&mut state);
        debug_assert!(state.is_load_successful(), "rewind buffer did not round-trip");
        let (buf, _) = state.into_buf();
        self.rewind.spare.push(buf);

        self.rewind_post_restore();
        // Land on a snapshot boundary so releasing the input resumes cleanly
        self.rewind.frames_since_snapshot = 0;
    }

    /// The in-session half of `savestate_post_load`. The mmu is deliberately not rebuilt:
    /// what shapes it — rom length and mask, save type, the RTC head-page unmap — is fixed
    /// for the life of a game, so the tables are already right, and `mmu_update_all`
    /// destroys and recreates every fastmem mapping (a kubridge commit per page on the
    /// Vita), which is far too expensive to do several times a second.
    fn rewind_post_restore(&mut self) {
        use crate::core::graphics::gpu_mem_buf::{DIRTY_BG, DIRTY_OAM, DIRTY_OBJ, DIRTY_PAL};
        self.mem.gpu_mem_dirty = DIRTY_BG | DIRTY_OBJ | DIRTY_PAL | DIRTY_OAM;

        // Compiled blocks may hold code that the restored ram no longer contains — the
        // guest could have overwritten it since (dma'ing an overlay in, say), and the
        // jit's smc tracking only watches guest writes, not our restores. §5.1 is what
        // stale blocks across a code reload cost.
        self.jit.init(&self.settings);
    }
}
