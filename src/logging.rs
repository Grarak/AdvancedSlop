#[cfg(all(target_os = "vita", debug_assertions))]
lazy_static::lazy_static! {
    pub static ref LOG_FILE: std::sync::Mutex<std::fs::File> = {
        let _ = std::fs::create_dir(crate::presenter::LOG_PATH);
        std::sync::Mutex::new(std::fs::File::create(crate::presenter::LOG_FILE).unwrap())
    };
}

macro_rules! debug_println {
    ($($args:tt)*) => {
        if crate::DEBUG_LOG {
            let log = format!($($args)*);
            // Interleave into the binary instruction log (when enabled) so debug lines line up with
            // the per-instruction register snapshots; otherwise print.
            if crate::debug_inst_log::is_logging() {
                crate::debug_inst_log::log_text(&log);
            } else {
                let current_thread = std::thread::current();
                let thread_name = current_thread.name().unwrap();
                println!("[{}] {}", thread_name, log);
            }
        }
    };
}

pub(crate) use debug_println;

macro_rules! info_println {
    ($($args:tt)*) => {
        let log = format!($($args)*);
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap();
        let value = format!("[{}] {}", thread_name, log);
        println!("{value}");
        #[cfg(all(target_os = "vita", debug_assertions))]
        {
            let mut log_file = crate::logging::LOG_FILE.lock().unwrap();
            std::io::Write::write(&mut *log_file, value.as_bytes()).unwrap();
            std::io::Write::write_all(&mut *log_file, "\n".as_bytes()).unwrap();
        }
    };
}
pub(crate) use info_println;

macro_rules! branch_println {
    ($($args:tt)*) => {
        if crate::BRANCH_LOG {
            let log = format!($($args)*);
            // Interleave branch decisions into the binary instruction log (when enabled) so control
            // flow can be diffed alongside the per-instruction register snapshots; otherwise print.
            if crate::debug_inst_log::is_logging() {
                crate::debug_inst_log::log_text(&log);
            } else {
                let current_thread = std::thread::current();
                let thread_name = current_thread.name().unwrap();
                println!("[{}] {}", thread_name, log);
            }
        }
    };
}
pub(crate) use branch_println;

macro_rules! block_asm_print {
    ($($args:tt)*) => {
        if crate::DEBUG_LOG {
            if crate::debug_inst_log::is_logging() {
                crate::debug_inst_log::log_text_no_newline(&format!($($args)*));
            } else {
                print!($($args)*);
            }
        }
    };
}
pub(crate) use block_asm_print;

macro_rules! block_asm_println {
    ($($args:tt)*) => {
        if crate::DEBUG_LOG {
            if crate::debug_inst_log::is_logging() {
                crate::debug_inst_log::log_text(&format!($($args)*));
            } else {
                println!($($args)*);
            }
        }
    };
}
pub(crate) use block_asm_println;

macro_rules! debug_panic {
    ($($args:tt)*) => {
        if crate::IS_DEBUG {
            panic!($($args)*)
        } else {
            unsafe { std::hint::unreachable_unchecked() }
        }
    };
}
pub(crate) use debug_panic;

/// Crash-forensics ring buffers (IS_DEBUG builds; single guest thread). Each records the
/// last N events of one kind; `dump_rings` prints them oldest-first from the panic hook.
pub struct EventRing<const N: usize> {
    pub buf: [[u32; 3]; N],
    pub head: usize,
}

impl<const N: usize> EventRing<N> {
    pub const fn new() -> Self {
        EventRing { buf: [[0; 3]; N], head: 0 }
    }

    #[inline]
    pub fn push(&mut self, a: u32, b: u32, c: u32) {
        if crate::IS_DEBUG {
            self.buf[self.head % N] = [a, b, c];
            self.head += 1;
        }
    }

    pub fn dump(&self, name: &str, labels: [&str; 3]) {
        println!("--- last {} {name} (oldest first) ---", self.buf.len().min(self.head));
        let n = self.head.min(N);
        for i in 0..n {
            let [a, b, c] = self.buf[(self.head - n + i) % N];
            println!("  {}={a:x} {}={b:x} {}={c:x}", labels[0], labels[1], labels[2]);
        }
    }
}

// Dispatch targets (call_jit_fun), breakout resumes, invalidate hits, interpreted block starts.
pub static mut DISPATCH_RING: EventRing<32> = EventRing::new();
pub static mut BREAKOUT_RING: EventRing<16> = EventRing::new();
pub static mut INVALIDATE_RING: EventRing<16> = EventRing::new();
pub static mut INTERP_BLOCK_RING: EventRing<32> = EventRing::new();
pub static mut IRQ_RING: EventRing<16> = EventRing::new();
pub static mut SLICE_RING: EventRing<32> = EventRing::new();

pub fn dump_rings() {
    unsafe {
        (*(&raw const DISPATCH_RING)).dump("dispatch targets", ["target", "", ""]);
        (*(&raw const BREAKOUT_RING)).dump("breakouts", ["store_pc", "resume_pc", "cpsr"]);
        (*(&raw const INVALIDATE_RING)).dump("invalidate hits", ["addr", "size", ""]);
        (*(&raw const INTERP_BLOCK_RING)).dump("interp blocks", ["start_pc", "thumb", ""]);
        (*(&raw const IRQ_RING)).dump("irq deliveries", ["pc", "cpsr", "ie_and_irf"]);
        (*(&raw const SLICE_RING)).dump("exec slices", ["entry", "exit_at", "resume"]);
    }
}
