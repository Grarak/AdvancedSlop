#![allow(incomplete_features)]
#![allow(internal_features)]
#![feature(adt_const_params)]
#![feature(allocator_api)]
#![cfg_attr(target_arch = "arm", feature(arm_target_feature))]
#![feature(const_trait_impl)]
#![feature(core_intrinsics)]
#![feature(downcast_unchecked)]
#![feature(generic_const_exprs)]
#![feature(naked_functions_rustic_abi)]
#![feature(ptr_as_ref_unchecked)]
#![feature(seek_stream_len)]
#![feature(slice_swap_unchecked)]
#![cfg_attr(target_arch = "arm", feature(stdarch_arm_neon_intrinsics))]
#![feature(stmt_expr_attributes)]
#![feature(thread_id_value)]
#![feature(vec_push_within_capacity)]

use crate::core::guest_regs_addr;
use crate::core::jit_asm_addr;
use crate::core::mmu_tcm_addr;
use crate::core::thread_regs;
use crate::cartridge_io::CartridgeIo;
use crate::core::apu::{SoundSampler, SAMPLE_BUFFER_SIZE};
use crate::core::emu::Emu;
use crate::core::gpu::{GbaRenderer, Gpu};
use crate::core::memory::regions;
use crate::core::thread_regs::ThreadRegs;
use crate::global_settings::GlobalSettings;
use crate::ra_context::RaContext;
use crate::jit::jit_asm::{JitAsm, MAX_STACK_DEPTH_SIZE};
use crate::jit::jit_memory::JitMemory;
use crate::logging::{debug_println, info_println};
use crate::mmap::{register_abort_handler, ArmContext, Mmap, PAGE_SIZE};
use crate::presenter::{default_key_binding, PresentEvent, Presenter, UiPauseMenuReturn, PRESENTER_AUDIO_OUT_BUF_SIZE};
use crate::utils::{set_thread_prio_affinity, start_profiling, stop_profiling, HeapArrayU32, ThreadAffinity, ThreadPriority};
use std::cell::UnsafeCell;
use std::intrinsics::unlikely;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;
use std::{mem, thread};

mod bitset;
mod cartridge_io;
mod core;
#[cfg(debug_assertions)]
mod debug_inst_log;
#[cfg(not(debug_assertions))]
#[path = "release_inst_log.rs"]
mod debug_inst_log;
mod fast_fixed_fifo;
mod fixed_fifo;
mod global_settings;
mod inst_log_format;
mod jit;
mod key_bindings;
mod logging;
mod math;
mod mmap;
mod presenter;
mod ra_context;
mod savestate;
mod screen_layout;
mod settings;
mod soundtouch;
pub mod utils;

const BUILD_PROFILE_NAME: &str = include_str!(concat!(env!("OUT_DIR"), "/build_profile_name"));
pub const DEBUG_LOG: bool = const_str_equal(BUILD_PROFILE_NAME, "debug");
pub const IS_DEBUG: bool = DEBUG_LOG || const_str_equal(BUILD_PROFILE_NAME, "release-debug");
pub const BRANCH_LOG: bool = DEBUG_LOG;
// Per-instruction trace recording (--inst-log): available in release-debug too — the dev
// profile is too slow to reach late-boot divergences within a practical wall-clock budget.
// Trace hooks only in the dev profile: the emitted per-instruction call costs ~37% of
// release-debug cycles (measured, Emerald uncapped on the pi) even with no trace armed,
// which taxed every profile and A/B run taken on that build. Capture is documented as
// dev-profile-only; release-debug keeps the rest of IS_DEBUG (asserts, forensics, port).
pub const INST_TRACE: bool = DEBUG_LOG;

use crate::utils::const_str_equal;

fn run_cpu(emu: &mut Emu, savestate: Option<Vec<u8>>) {
    emu.reset();

    info_println!("Loading rom into memory");
    emu.cartridge_load_rom_into_shm();

    info_println!("Initialize mmu");
    emu.mmu_update_all();

    // Direct boot state (HLE bios): System mode with irqs enabled (hardware hands off
    // with cpsr = 0x1F; jsmolka bios.gba test 3 depends on it), entry at the rom base
    {
        let regs = thread_regs();
        regs.user.sp = 0x03007F00;
        regs.irq.sp = 0x03007FA0;
        regs.svc.sp = 0x03007FE0;
        regs.user.lr = 0x08000000;
        regs.gp_regs[12] = 0x08000000;
        regs.pc = 0x08000000;
        emu.thread_set_cpsr(0x0000001F, false);
    }

    {
        // I/O boot state
        emu.mem_write::<_>(0x4000300, 0x01u8); // POSTFLG
        emu.mem_write::<_>(0x4000088, 0x0200u16); // SOUNDBIAS
    }

    Gpu::initialize_schedule(&mut emu.cm);
    emu.apu_initialize_schedule();

    unsafe { register_abort_handler(fault_handler).unwrap() };

    let jit_asm_arm7 = unsafe { (jit_asm_addr() as *mut JitAsm).as_mut_unchecked() };

    if let Some(data) = savestate {
        if emu.load_state(data) {
            info_println!("Savestate loaded, continuing");
        } else {
            eprintln!("Failed to load savestate");
        }
    }

    execute_jit(jit_asm_arm7);
}

pub unsafe fn get_jit_asm_ptr<'a>() -> *mut JitAsm<'a> {
    jit_asm_addr() as *mut JitAsm<'a>
}

unsafe fn process_fault(mem_addr: usize, host_pc: &mut usize, arm_context: &ArmContext) -> bool {
    let asm = unsafe { get_jit_asm_ptr().as_mut_unchecked() };

    debug_println!("fault at {mem_addr:x}");
    // Guest addresses cover the full 32 bits (wild pointers, mirror probes), so the
    // emitted base+addr can land outside the fastmem reservation in either direction
    // (it wraps on 32-bit hosts). Any fault from inside jit code is a guest access:
    // recover the guest address modularly and patch the site to the slow path.
    if !asm.emu.jit.is_in_jit_mem(*host_pc) {
        eprintln!("fault {host_pc:x} {mem_addr:x} outside of jit code");
        return false;
    }

    let guest_mem_addr = (mem_addr as u64).wrapping_sub(mmu_tcm_addr() as u64) as u32;
    debug_println!("guest fault at {mem_addr:x} to guest {guest_mem_addr:x}");
    asm.emu.jit.patch_slow_mem(host_pc, guest_mem_addr, arm_context)
}

#[cold]
fn fault_handler(mem_addr: usize, host_pc: &mut usize, arm_context: &ArmContext) -> bool {
    unsafe { process_fault(mem_addr, host_pc, arm_context) }
}

#[inline(never)]
fn execute_jit(jit_asm_arm7: &mut JitAsm) {
    loop {
        let cycles = if !jit_asm_arm7.emu.cpu_is_halted() && !jit_asm_arm7.runtime_data.is_idle_loop() {
            jit_asm_arm7.execute()
        } else {
            0
        };

        if unlikely(cycles == 0) {
            jit_asm_arm7.emu.cm.jump_to_next_event();
        } else {
            jit_asm_arm7.emu.cm.add_cycles(cycles);
        }

        if jit_asm_arm7.emu.cm_check_events() {
            jit_asm_arm7.runtime_data.set_idle_loop(false);
            // Events may have scheduled imm events (interrupt dispatch): run them now,
            // before handing the guest another quantum — hardware takes an irq within
            // cycles of the raise, and games can poll the flag and disable IME faster
            // than a quantum otherwise
            jit_asm_arm7.emu.cm_check_events();
        }
        // Outside guest code the flag's purpose is served (imm events just drained);
        // a stale flag would force a spurious breakout on the next slice's first store
        jit_asm_arm7.emu.breakout_imm = false;

        if unlikely(jit_asm_arm7.emu.gpu.renderer.is_quit()) {
            break;
        }
    }
}

pub fn actual_main() {
    // Static core partition, one thread per core and no mask wider than one core: the
    // Vita's scheduler makes a mess of anything it is left to decide. Core 0 rasterizes
    // and composes the top half of every frame's scanlines (and is shared with the OS),
    // core 1 does the bottom half and is also this thread — presenting — and core 2 is
    // the emulation thread, which only snapshots. The two on core 1 do not really
    // contend: the present is parked on vblank for most of a frame, which is exactly why
    // the rasterizing half must not be behind it.
    if cfg!(target_os = "vita") {
        set_thread_prio_affinity(ThreadPriority::High, &[ThreadAffinity::Core1]);
    }

    info_println!("Starting AdvancedSlop");

    if IS_DEBUG {
        std::env::set_var("RUST_BACKTRACE", "1");
        #[cfg(target_os = "linux")]
        std::panic::set_hook(Box::new(|panic_info| {
            debug_inst_log::flush();
            jit::interpreter::print_last_interpreted();
            logging::dump_rings();
            let mut count = 0;
            let cwd = std::env::current_dir();
            backtrace::trace(|frame| {
                backtrace::resolve_frame(frame, |symbols| {
                    eprint!("{count}: {:4x} - ", frame.ip() as usize);
                    match symbols.name() {
                        None => eprint!("<unknown>"),
                        Some(name) => {
                            eprint!("{name}");
                            if name.to_string().starts_with("advancedslop") {
                                eprint!(" <----------");
                            }
                        }
                    }
                    eprintln!();
                    if let (Some(file), Some(line)) = (symbols.filename(), symbols.lineno()) {
                        eprint!("{:4}", "");
                        if let Ok(cwd) = &cwd {
                            if let Ok(suffix) = file.strip_prefix(cwd) {
                                eprint!("          at {suffix:?}:{line}");
                            } else {
                                eprint!("          at {file:?}:{line}");
                            }
                        } else {
                            eprint!("          at {file:?}:{line}");
                        }
                        if let Some(colno) = symbols.colno() {
                            eprint!(":{colno}")
                        }
                        eprintln!();
                    }
                });

                count += 1;
                count < 25
            });

            eprintln!();
            eprintln!(
                "{}: {} <----------",
                panic_info.payload_as_str().unwrap_or("No payload"),
                panic_info
                    .location()
                    .map_or("No location".to_string(), |location| { format!("{}:{}:{}", location.file(), location.line(), location.column()) })
            );
            eprintln!();
        }));

        #[cfg(target_os = "vita")]
        {
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let location = info.location().unwrap();

                let msg = match info.payload().downcast_ref::<&'static str>() {
                    Some(s) => *s,
                    None => match info.payload().downcast_ref::<String>() {
                        Some(s) => &s[..],
                        None => "Box<Any>",
                    },
                };
                info_println!("panicked at {location}: '{msg}'");

                default_hook(info);
            }));
        }
    }

    let presenter = Presenter::new();
    let mut presenter = match presenter {
        None => return,
        Some(presenter) => presenter,
    };

    let fps = Arc::new(AtomicU16::new(0));
    let key_map = Arc::new(AtomicU32::new(0xFFFFFFFF));
    let mut sound_sampler = UnsafeCell::new(SoundSampler::new());

    let fps_clone = fps.clone();
    let key_map_clone = key_map.clone();
    let sound_sampler_ptr = sound_sampler.get() as usize;

    // Both targets map the register file at their fixed guest_regs_addr() constant;
    // set_guest_regs_addr just feeds the debug-only sanity checks in core/mod.rs.
    let mut arm7_thread_regs = Mmap::rw("arm7_thread_regs", guest_regs_addr(), utils::align_up(size_of::<ThreadRegs>(), PAGE_SIZE)).unwrap();
    core::set_guest_regs_addr(arm7_thread_regs.as_ptr() as usize);
    let arm7_thread_regs = arm7_thread_regs.as_mut_ptr() as *mut ThreadRegs;
    unsafe { *arm7_thread_regs = ThreadRegs::default() };

    // Initializing jit mem inside of emu, breaks kubridge for some reason
    // Might be caused by initialize shared mem? Initialize here and pass it to emu
    let jit_mem = JitMemory::new();
    let mut emu_unsafe = UnsafeCell::new(Emu::new(fps_clone, key_map_clone, NonNull::from(sound_sampler.get_mut()), jit_mem));
    let emu_ptr = emu_unsafe.get() as usize;

    let mut jit_asm_arm7 = Mmap::rw("arm7_jit_asm", jit_asm_addr(), utils::align_up(size_of::<JitAsm>(), PAGE_SIZE)).unwrap();
    core::set_jit_asm_addr(jit_asm_arm7.as_ptr() as usize);
    let jit_asm_arm7: &'static mut JitAsm = unsafe { mem::transmute(jit_asm_arm7.as_mut_ptr()) };
    *jit_asm_arm7 = JitAsm::new(unsafe { emu_unsafe.get().as_mut().unwrap() });

    let cpu_active = Arc::new(AtomicBool::new(true));

    // The renderer lives for the whole app; each game resets its handoff state and
    // reuses it (see reset_for_new_game). The present rect comes from the screen-layout
    // setting, applied at each game launch and again on pause-menu changes.
    let mut gba_renderer = GbaRenderer::new();
    emu_unsafe.get_mut().gpu.set_gpu_renderer(NonNull::from(&mut gba_renderer));
    let gba_renderer_ptr = &gba_renderer as *const GbaRenderer as usize;

    // The -s savestate arg only seeds the first launched game.
    let mut pending_savestate = presenter.get_savestate_path().and_then(|path| match std::fs::read(&path) {
        Ok(data) => Some(data),
        Err(err) => {
            eprintln!("Failed to read savestate {path:?}: {err}");
            None
        }
    });

    // Persistent settings: loaded from the ini next to the roms, edited on the menu /
    // pause screens, written back after each.
    let mut settings_config = crate::settings::SettingsConfig::new(presenter.settings_path());

    // RetroAchievements. The guard must be bound AFTER the context: drop order runs it
    // first, joining the request thread before the context it holds a raw pointer to goes
    // away (see RaThreadGuard).
    // The default profile is index 0 of the Controls setting and the fallback every
    // custom profile inherits hotkeys from, so it has to be the real built-in mapping,
    // not an all-unbound KeyBinding::default().
    let mut global_settings = GlobalSettings::new(presenter.data_path(), default_key_binding()).unwrap();
    let mut ra_context = RaContext::new();
    ra_context.set_cache_dir(presenter.data_path().join("ra"));
    let _ra_context_thread_guard = ra_context.start_server_request_receive_thread();
    // A stored token gets the session logged back in without asking again.
    settings_config.settings.populate_screen_layouts(&global_settings.custom_layouts);
    if !global_settings.ra_username.is_empty() && !global_settings.ra_token.is_empty() {
        ra_context.login_with_token(&global_settings.ra_username, &global_settings.ra_token);
    }

    // Frontend: the game browser + settings menu. Returns the chosen rom; None
    // means the user closed the app. After a game quits back here, the loop repeats.
    'games: loop {
        let rom_path = presenter.present_ui(&mut settings_config, &mut global_settings, &mut ra_context);
        // Persist whatever the menu changed, whether it launched a game or closed the app.
        settings_config.flush();
        let Some(rom_path) = rom_path else { break 'games };
        let settings = settings_config.settings.clone();
        let save_path = PathBuf::from(format!("{}.sav", rom_path.to_string_lossy().trim_end_matches(".gba").trim_end_matches(".GBA")));
        let cartridge_io = match CartridgeIo::new(rom_path, save_path) {
            Ok(io) => io,
            Err(err) => {
                eprintln!("Failed to load rom: {err}");
                continue 'games;
            }
        };

        gba_renderer.set_present_rect(screen_layout::rect_with_custom(settings.screen_layout(), &global_settings.custom_layouts));
        gba_renderer.reset_for_new_game(&emu_unsafe.get_mut().mem.shm);
        emu_unsafe.get_mut().cartridge.set_cartridge_io(cartridge_io);
        emu_unsafe.get_mut().settings = settings;

        // Identify the rom with the server and arm the achievement runtime. rc_read_mem
        // resolves RA's flat addresses against the shm, so hand it that base.
        let ra_enabled = settings_config.settings.retroachievements();
        if ra_enabled {
            let shm_ptr = emu_unsafe.get_mut().mem.shm.as_ptr();
            ra_context.load_game(shm_ptr, &emu_unsafe.get_mut().cartridge.io);
        }

        sound_sampler.get_mut().init();

        let presenter_audio_out = presenter.get_presenter_audio_out();
        let last_save_time: Arc<Mutex<Option<(std::time::Instant, bool)>>> = Arc::new(Mutex::new(None));
        let last_save_time_clone = last_save_time.clone();
        let savestate = pending_savestate.take();

        cpu_active.store(true, Ordering::SeqCst);
        // Armed before the cpu thread starts so the loading bar shows from the first
        // frame; cleared by cartridge_load_rom_into_shm once the rom is in shm.
        crate::core::memory::cartridge::ROM_LOAD_ACTIVE.store(true, Ordering::Relaxed);
        crate::core::memory::cartridge::ROM_LOAD_TOTAL.store(0, Ordering::Relaxed);
        crate::core::memory::cartridge::ROM_LOAD_DONE.store(0, Ordering::Relaxed);

        let cpu_thread = thread::Builder::new()
            .name("cpu".to_owned())
            .stack_size(MAX_STACK_DEPTH_SIZE + 1024 * 1024) // Add 1MB headroom to stack
            .spawn(move || {
                set_thread_prio_affinity(ThreadPriority::High, &[ThreadAffinity::Core2]);
                info_println!("Start cpu {:?}", thread::current().id());
                let emu = emu_ptr as *mut Emu;
                start_profiling();
                run_cpu(unsafe { emu.as_mut_unchecked() }, savestate);
                stop_profiling();
                info_println!("Stopped cpu {:?}", thread::current().id());
            })
            .unwrap();

        let cpu_thread_ptr = cpu_thread.thread() as *const _ as usize;

        // Rasterizers: woken by the emulation thread at each vblank, each draws and
        // composes its half of the frame's scanlines straight into the pixel buffer.
        // The bottom half gets its own thread on core 1 rather than running on the
        // present thread, which spends most of a frame parked on vblank inside
        // vglSwapBuffers.
        let raster_threads: Vec<_> = [(0usize, ThreadAffinity::Core0), (1, ThreadAffinity::Core1)]
            .into_iter()
            .map(|(half, core)| {
                thread::Builder::new()
                    .name(format!("raster{half}"))
                    .spawn(move || {
                        set_thread_prio_affinity(ThreadPriority::High, &[core]);
                        info_println!("Start raster worker {half} {:?}", thread::current().id());
                        let renderer = unsafe { (gba_renderer_ptr as *const GbaRenderer).as_ref_unchecked() };
                        match half {
                            0 => renderer.raster_worker_loop::<0>(),
                            _ => renderer.raster_worker_loop::<1>(),
                        }
                        info_println!("Stopped raster worker {half} {:?}", thread::current().id());
                    })
                    .unwrap()
            })
            .collect();

        let cpu_active_clone = cpu_active.clone();
        let audio_out_thread = thread::Builder::new()
            .name("audio_out".to_owned())
            .spawn(move || {
                // Core 0: mostly blocked on the audio device
                set_thread_prio_affinity(ThreadPriority::Default, &[ThreadAffinity::Core0]);
                let mut guest_buffer = HeapArrayU32::<{ SAMPLE_BUFFER_SIZE }>::default();
                let mut audio_buffer = HeapArrayU32::<{ PRESENTER_AUDIO_OUT_BUF_SIZE }>::default();
                let emu = unsafe { (emu_ptr as *mut Emu).as_mut_unchecked() };
                let sound_sampler = unsafe { (sound_sampler_ptr as *mut SoundSampler).as_mut_unchecked() };
                let cpu_thread = unsafe { (cpu_thread_ptr as *const Thread).as_ref_unchecked() };
                let cpu_active = cpu_active_clone;
                while cpu_active.load(Ordering::Relaxed) {
                    sound_sampler.consume(cpu_thread, &mut guest_buffer, &mut audio_buffer, emu.settings.audio_stretching());
                    presenter_audio_out.play(&audio_buffer);
                }
            })
            .unwrap();

        let cpu_active_clone = cpu_active.clone();
        let save_thread = thread::Builder::new()
            .name("save".to_owned())
            .spawn(move || {
                set_thread_prio_affinity(ThreadPriority::Low, &[ThreadAffinity::Core0]);
                let last_save_time = last_save_time_clone;
                let emu = unsafe { (emu_ptr as *mut Emu).as_mut().unwrap_unchecked() };
                let cpu_active = cpu_active_clone;
                'outer: loop {
                    for _ in 0..6 {
                        if !cpu_active.load(Ordering::Relaxed) {
                            break 'outer;
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                    emu.cartridge.io.flush_save_buf(&last_save_time);
                }
            })
            .unwrap();

        // Draw the loading bar while the cpu thread streams the rom into shm.
        while crate::core::memory::cartridge::ROM_LOAD_ACTIVE.load(Ordering::Acquire) {
            let total = crate::core::memory::cartridge::ROM_LOAD_TOTAL.load(Ordering::Relaxed);
            let done = crate::core::memory::cartridge::ROM_LOAD_DONE.load(Ordering::Relaxed);
            presenter.present_progress("Loading rom", done as usize, total.max(1) as usize);
        }

        // What to do once the inner game loop ends.
        enum GameEnd {
            ToMenu,
            QuitApp,
        }
        let mut game_end = GameEnd::ToMenu;

        'game: loop {
            match presenter.poll_event(unsafe { &emu_unsafe.get().as_ref_unchecked().settings }) {
                PresentEvent::Inputs { keymap, .. } => {
                    key_map.store(keymap, Ordering::Relaxed);
                }
                PresentEvent::SetFramelimit(value) => {
                    emu_unsafe.get_mut().settings.set_framelimit(value);
                    info_println!("Framelimit set to {value}");
                }
                PresentEvent::CycleScreenLayout => {
                    // Live: the rect is only read by this thread's blit. Both settings
                    // copies step together — the emu's is copied back over the config at
                    // game end, which would otherwise revert the cycle. Dirty so the
                    // choice shows in the pause menu and persists like a menu edit.
                    settings_config.settings.cycle_screen_layout();
                    emu_unsafe.get_mut().settings.cycle_screen_layout();
                    settings_config.dirty = true;
                    gba_renderer.set_present_rect(screen_layout::rect_with_custom(settings_config.settings.screen_layout(), &global_settings.custom_layouts));
                }
                PresentEvent::Quit => {
                    game_end = GameEnd::QuitApp;
                    break 'game;
                }
                PresentEvent::Pause => {
                    // Freeze the game (cpu thread parks at the next vblank handoff), then
                    // draw the pause menu over the last frame.
                    let renderer = unsafe { (gba_renderer_ptr as *const GbaRenderer).as_ref_unchecked() };
                    renderer.set_pause(true);
                    let ret = presenter.present_pause(renderer, &mut settings_config);
                    // The cpu thread is still parked, so handing it the menu's settings is
                    // safe up until the unpark below.
                    emu_unsafe.get_mut().settings = settings_config.settings.clone();
                    gba_renderer.set_present_rect(screen_layout::rect_with_custom(settings_config.settings.screen_layout(), &global_settings.custom_layouts));
                    renderer.set_pause(false);
                    cpu_thread.thread().unpark();
                    match ret {
                        UiPauseMenuReturn::Resume => {}
                        UiPauseMenuReturn::QuitToMenu => break 'game,
                        UiPauseMenuReturn::QuitApp => {
                            game_end = GameEnd::QuitApp;
                            break 'game;
                        }
                    }
                    continue 'game;
                }
            }

            let debug_fps = if emu_unsafe.get_mut().settings.show_debug_stats() {
                Some(fps.load(Ordering::Relaxed))
            } else {
                None
            };
            gba_renderer.render_loop(&mut presenter, debug_fps, ra_enabled.then_some(&ra_context));

            if ra_enabled {
                ra_context.on_frame();
            }
        }

        // Tear the game down: stop the cpu loop (also wake it if parked on pause or a
        // full sample queue) and join the per-game threads before returning to the menu.
        gba_renderer.set_quit(true);
        gba_renderer.set_pause(false);
        cpu_thread.thread().unpark();
        cpu_thread.join().unwrap();
        for raster_thread in raster_threads {
            raster_thread.join().unwrap();
        }
        cpu_active.store(false, Ordering::SeqCst);
        audio_out_thread.join().unwrap();
        save_thread.join().unwrap();

        if ra_enabled {
            ra_context.unload_game();
            ra_context.on_idle();
        }

        // Persist any settings the pause menu changed during the session.
        settings_config.settings = emu_unsafe.get_mut().settings.clone();
        settings_config.dirty = true;
        settings_config.flush();

        if let GameEnd::QuitApp = game_end {
            break 'games;
        }
    }
}
