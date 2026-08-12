# Development Notes

Lessons, invariants, and debugging strategies accumulated while building the cold-block
interpreter and hunting compatibility bugs (boot hangs, save-resume corruption, interrupt
starvation). Written for anyone continuing this work — human or AI session. File/line
references may drift; module-level statements are stable.

Reference implementation for guest semantics: **NooDS** (`interpreter_*.cpp`) — a complete,
known-good interpreter. When this repo's JIT and NooDS disagree on a quirk, the interpreter
must match **this repo's JIT** (the two engines must be trace-identical); the deviations are
listed below.

---

## 1. Core invariants (violating any of these caused a real bug)

### Execution model
- The guest CPU runs on ONE host thread. It runs until its cycle budget
  (`MAX_BRANCH_LOOP_CYCLE_COUNT`: 128) trips a scheduler check at a **taken branch**;
  `run_scheduler` then runs cycle-manager events. There is no preemption between
  branches — anything that must interrupt execution either waits for the next
  branch-point check or uses the `breakout_imm` mechanism (set by halt/immediate-DMA io
  writes, checked after stores).
- **Immediate events (`schedule_imm`) only fire when the scheduler actually runs.** A pending
  interrupt is dispatched by an imm event; if the guest controls when the scheduler runs
  (fixed-cycle loops), the event can be systematically starved (see case study §5.3
  — its counter-case warns equally against forcing prompt delivery).

### Cycle-manager event grids
- **Periodic events reschedule from their due cycle (`schedule_from_due`), never from the
  dispatch-time `cycle_count`.** The jit overshoots each due cycle by the slice remainder;
  rescheduling from the overshot count made every periodic grid drift independently — the SPU
  sample grid slipped ~2 samples per 512-sample alarm period against the ARM7 timer grid,
  which desynced the capture-ring surround loop games run on the ARM7 sound driver (§5.7).
  A due cycle left in the past by a catch-up is legal: it fires on the next check, so a late
  slice catches up instead of stretching the period. Consequences that must hold together:
  `jump_to_next_event` never moves the clock backwards, and the 2^31 overflow rebase uses
  saturating subtraction (a past due underflowed it — abort on release-debug, a never-firing
  event on release). GPU scanline events still reschedule dispatch-relative; convert them the
  same way if a vblank-vs-timer sync bug ever surfaces.
- **The dispatch loop reads `cycle_count` per round, never hoisted.** A handler can replace
  the whole scheduler under the scan: the vblank hook is where savestate loads and rewind
  restores are applied, and `cycle_count` is a free-running absolute counter, so the restored
  clock bears no relation to the running session's. Judged against a hoisted (pre-load) count,
  every restored event reads as due, and the re-scan grinds the entire grid forward to catch
  up — with the apu sample event parking the cpu thread on a full queue while it does, so it
  never ends. A state taken half a rebase period earlier is enough to hang the emulator.
- **A forced scheduler run must not invent guest time.** `cpu_check_for_interrupt`'s
  saturation of `accumulated_cycles` (the §5.3 fix) saves the real count in
  `pre_force_cycles`; every consumer restores it through `take_real_accumulated`. Without the
  restore, ARM7-HLE mode converted the phantom quantum straight into guest time on every irq
  re-enable (`run_scheduler::<true>` has no min() against a real ARM7 slice) and inflated the
  clock enough to trip SDK timeouts.

### Savestate / rewind
- **Guest state is only ever restored at one point in the frame: the vblank hook at
  `v_count == VISIBLE_LINES`** (gpu.rs, `gpu_on_scanline308_event`). Three things make that
  the only safe point, and a restore moved anywhere else breaks at least one of them: cm
  events are dispatched between jit execute calls (`execute_jit`), so no compiled frame is on
  the stack when the jit is invalidated; every gpu event has just been rescheduled, so the
  restored scheduler is consistent; and it sits right after `on_frame_finish` handed the
  frame over, so the renderer pipeline lines up. The pause menu parks the cpu thread at this
  same hook, which is why a menu-driven save/load needs only a single-frame ticket
  (`step_paused_frame`) rather than resuming the game.
- **One field walk drives both directions** (`SavestateContext`), so save and load can never
  drift apart; `is_load_successful` requires the load to consume the file exactly. Anything
  host-derived — mmu tables, compiled blocks, renderer dirty bits — is *not* in the walk and
  must be re-derived after it (`savestate_post_load`), or it describes memory that has
  already changed. See §5.10 for how much of that re-derivation rewind can skip, and why.

### Guest state conventions
- **`regs.pc` and every branch target carry the thumb bit in bit 0** at all
  handback/scheduler/exception boundaries. `call_jit_fun` consumes it; the HLE BIOS irq entry
  reads `pc & 1` for the SPSR T bit. A thumb dispatch loop must write `addr | 1` on
  breakout/fallback.
- **CPSR cross-block contract**: blocks are emitted assuming guest CPSR memory is clean at
  entry; keeping flags in a host register across block edges is unsound.
- **Every linking branch must write the guest LR itself** (jit emitters and interpreter
  handlers both). The return stack is emulator bookkeeping, not the architecture: passing
  the return address only to the return-stack push leaves the guest r14 stale, and the
  callee's `bx lr` then jumps wherever the previous call went (5.6).
- Guest regs map to host R4–R11 contiguously; `GUEST_REGS_PTR_REG = R3`, `CPSR_TMP_REG = R0`
  — R0–R3/R12/LR are scratch, so a helper call emitted mid-block clobbers them. (A hand-rolled
  emitter change that parked a value in R0 across a call faulted instantly; even R8 parking
  conflicted with block-internal allocation. Don't insert calls into emitted blocks without
  understanding the allocator's live state at that point.)
- Host code is emitted in the guest's mode (ARM guest block → ARM host code, thumb → thumb);
  entry pointers carry the thumb bit. This is also why frame-pointer profiling is impossible
  on this target (r7/r11 fp split).
- `exit_guest_context` long-jumps out of arbitrarily deep guest call chains by restoring the
  saved host sp — it abandons Rust frames. Nothing on that path may own anything with a `Drop`.

### Cycle accounting (interpreter must mirror the JIT exactly)
- Per-inst cycles = the disassembler's `InstInfo::cycle`, prefix-summed per block.
  Condition-failed ARM instructions still charge their full static cycle (the JIT's per-block
  prefix sums include skipped instructions).
- Every taken branch adds **+2** (the JIT's `emit_count_cycles` epilogue: `total + 2 -
  pre_sum`) — but NOT on fallback handback (the compiled block adds its own +2) and NOT on
  `breakout_imm` (the JIT's breakout flush has no +2 either).
- **Scheduler checks happen only at taken branches.** Adding a mid-straight-line budget check
  to "improve responsiveness" skews irq timing between the engines and breaks trace parity.

### Address arithmetic policy
- release-debug builds run with overflow-checks + debug-assertions ON. Computed guest
  addresses use plain `+`/`-` so a genuine guest bug panics loudly — but **negative register
  offsets are routine guest behavior** (`ldrh r1, [r2, r1]` with r1 = −4 is legal). Load/store
  *address* computations must use wrapping arithmetic; only treat *data* overflow as suspect.
  A "plain + catches guest bugs" convention applied to addresses produced false-positive
  panics in real games.
- Corollary: a bug that asserts on release-debug **silently corrupts** on release. "Game gets
  stuck on hardware" is often an assert-class bug — reproduce on release-debug first.
- Audit `assert_unchecked` bounds for off-by-one at the boundary: `start + LEN < buf.len()`
  rejects the last valid slot when `start + LEN` is an exclusive slice end (must be `<=`).
  This aborted long game sessions only after a cache filled — invisible in short tests.

---

## 2. Interpreter design rules (deliberate; weaker designs were rejected)

- **Flat literal fn-pointer tables**, exactly like the disassembler's own lookup tables:
  `ARM_TABLE: [ArmInterpFn; 4096]` indexed `((op>>16)&0xFF0)|((op>>4)&0xF)`, `THUMB_TABLE:
  [ThumbInterpFn; 1024]` indexed `op >> 6`. Every entry is a fully specialized handler built
  with const generics + `paste` (mirroring `disassembler/delegations.rs`). **Zero matching on
  the execution path** — no `match op`, no in-handler addressing/shift classification.
- The tables are **generated from the disassembler's lookup tables**
  (`tools/gen_thumb_table.py`; same approach for ARM) so JIT-decode and interpreter-decode
  can never disagree. Regenerate after adding handlers; unknown names → `inst_fallback`.
- ARM condition check: literal `CONDITION[((op>>24)&0xF0)|(cpsr>>28)]` table (0=skip, 1=run,
  2=reserved→fallback). Unconditional fast path: `opcode >= 0xE0000000` skips the cpsr load.
- **Handback model**: interpret straight-line until a branch/pc-write, then hand to the next
  guest address's jit entry ("one block then call next entry"). Anything unimplemented →
  `inst_fallback` → that block compiles. Correctness holds at ANY coverage level.
- **Return-stack parity**: `bl`-shapes route through the JIT's own `branch_reg` (push +
  native call + resume interpreting the tail) and `bx lr`-shapes through `branch_lr`
  (pop-compare + native return). Without this, every interpreted call/return produced a
  return-stack mismatch → `exit_guest_context` → quantum reset — a massive scheduler-timing
  divergence vs the JIT.
- **Flat loop for cold branch targets**: a taken branch to another cold same-mode block
  continues inside the dispatch loop instead of recursing a host frame per block (compiled
  branches tail-call; naive recursion overflowed the host stack in interpreted spin loops —
  and the SIGSEGV was eaten by the fastmem fault handler, appearing as a silent death).
  A stack-depth guard covers the remaining forward-call chains.
- **Hotness counters live in `jit_memory`**, one u8 per halfword of executable memory, laid
  out per region exactly like the jit entries with the same mirror collapse. Direct-mapped
  per-cpu arrays keyed on raw pc were rejected: they count the same physical code separately
  per mirror address and duplicate storage. A null counter pointer = non-executable region
  (e.g. HLE BIOS trampoline addresses) — such targets must reach their special jit entries,
  never the interpreter.
- Semantics quirks aligned to this JIT (deviating from NooDS where they differ): word loads
  rotate misaligned reads; halfword loads do NOT model the ARM7 misaligned-rotate quirk;
  thumb `stmia` models the ARM7 "written-back base is stored unless lowest listed reg" quirk;
  `NEG` sets V. (The ARM-side `ldm_stm` does not model the stm quirk — known divergence
  suspect if an ARM7-heavy title misbehaves.)
- Block transfers gather/scatter through a stack buffer with a single multiple-slice memory
  request (one region/mmu resolve per transfer, mirroring the JIT's slow path). The register
  count stays runtime — rlist bits aren't in the table index, so it can't be const.
- `breakout_imm` is checked **only after stores** (only stores can set it) — matching the
  JIT's write handlers and avoiding a per-inst load+branch.

## 3. Blocks that must NEVER be interpreted

Currently none: the dual-CPU-era mechanisms this section policed (the HLE-substituted os irq
handler, the overlay-reload invalidation hook, the sync-microcode window) are long gone,
and the GBA's HLE bios runs outside guest pc space entirely. The invariant
class still matters — if a compile-time pattern substitution that carries *semantics*
(not just speed) is ever added, its pcs must bypass the interpreter, because
interpret-then-compile does not self-heal when the pattern only matches from the block's
first pc (see §5.4). Decide the class when adding a substitution: accelerator
substitutions (functionally equivalent code) may interpret first; semantic replacements
and side-effect hooks may not.

---

## 4. Debugging playbook

Ordered by cost. Every technique below cracked at least one real bug.

1. **Read the panic.** release-debug has overflow-checks and debug-asserts; the panic hook
   prints a full backtrace plus the last interpreted instruction with all guest regs,
   and the memory bounds asserts print the offending address.
2. **A/B the engine.** Flip `INTERP_THRESHOLD` (0 = pure jit, 255 = always interpret, 100 =
   production) and rebuild. Pure jit reproducing it rules the interpreter out.
3. **A/B a suspect feature** with a temporary `const DISABLE_X: bool` knob. Minutes to
   exonerate a subsystem.
4. **Count events in the debug log before tracing instructions.** With `DEBUG_LOG = true`,
   grep-counting `send interrupt` / `interrupt {` / `can't interrupt` / `hle ipc send` lines
   over a time window characterizes a hang instantly: irqs flowing but no progress = game
   loop alive (look elsewhere); requests sent but zero dispatches = starvation; no irq
   traffic at all = spinning before irq setup (very early boot). This is how both a "GPU
   never starts" hang and an interrupt-starvation hang were localized in minutes each.
5. **Instruction-trace diff** (the heavy hammer). `--inst-log <path>` records from boot;
   `--inst-log-lazy <path>` arms on the debug port's `inst-log` command (capture the final
   stretch); SIGINT and the panic hook flush. Decode with `tools/trace_decode.sh <path>` (x86-native — a detached crate that
   reuses the emulator's real disassembler but links no C/C++, so it decodes on the dev box that has
   no emulator binary; `advancedslop decode-inst-log <path>` is the equivalent on an arm box).
   Diff two engines (threshold 0 vs 100/255 builds) or two arm7-emulation modes.
   - **Parity caveats (each one cost real time):** neither engine logs taken branches, but
     the jit logs extra records the interpreter doesn't — "enter block" markers, records
     re-emitted on return-stack resume, same-block `bl`s. Filter to per-cpu `Executed`
     records and drop records whose InstInfo writes PC before comparing.
   - First divergence with same PC but different registers = real bug. Different PC sequence
     with same registers = usually benign timing skew; io-poll loops legitimately differ in
     iteration count between engines.
   - **Boot with no input is deterministic** — full-boot traces diff cleanly. Traces that
     start from an interactive point are polluted by input-frame differences.
   - `tools/trace_diff.py` diffs the binary logs directly (no decode) and resyncs across
     HLE gaps. Its three resync pitfalls are documented in the header; the important one:
     pick the resync with the SMALLEST total skip, and include offset 0 in the common-pc
     search, or it aligns far-future visits and reports bogus divergences.
   - Interleaved `memory read/write at X with value Y` text records show io handshakes,
     overlay/file loads, and callback-pointer provenance for free.
   - **Memory-watch a region across a whole trace** by scanning the raw ilog and
     reconstructing each store/load's effective address from the post-execution registers
     (invert writeback; ~15 s per 24 GB with a small standalone scanner). Histogram the
     hits by (pc, sp): a one-off sp variant among thousands of identical executions IS the
     anomaly. In the text records, `failed to branch lr ... desired: ffffffff` is routine
     (empty return stack); a failed branch-lr with a *real* desired address means a callee
     returned somewhere its caller never expected — follow that first.
6. **Static disassembly of guest code.** armhf binutils via qemu:
   `qemu-arm -L <sysroot> .../arm-linux-gnueabihf-objdump -D -b binary -m armv5te
   [-M force-thumb] --adjust-vma=<ram_addr> <bin>`. The .gba rom maps flat at
   0x08000000 (entry at its start), so `--adjust-vma=0x8000000` over the rom file
   disassembles it in place; code copied to iwram/ewram is found by searching its raw
   bytes in the rom.
7. **Use a third reference to break interp-vs-jit ambiguity.** An interp-vs-jit diff cannot
   distinguish "interp = hardware, jit's HLE ≠ hardware" (benign) from "interp ≠ hardware"
   (the bug) — both look identical. Compare against NooDS at the divergence pcs. When a
   memory corruption needs a known-good write sequence, **instrument NooDS's `Memory::write`
   with an address watch** and capture the reference stream — this pinpointed an
   event-ordering bug that pure trace diffing could not. For anything longer than a spot
   check, build NooDS headless: a ~30-line main that heap-allocates `Core`, sets
   `Settings::fpsLimiter = 0` / `threaded2D = threaded3D = 0`, and loops
   `running.store(true); runCore();` (the core stops itself every frame) runs a rom at
   several times real speed on the dev box with no window — tap `Spu::pushSample` or
   `Memory::write` behind an env var and the reference stream is free. Boot with no input
   is deterministic there too, so segment-level comparisons line up.
8. **Bisect hangs by screenshot, not by exit code.** A boot hang has no crash to detect;
   a screenshot at a fixed wall-time plus a non-black-pixel percentage (fps overlay text
   stays under a few percent; any real frame is far above) is a reliable GOOD/BAD oracle
   for `git checkout` bisecting. Two traps: "fixed the crash" commits are not "boots"
   commits (AC's a64 crash fix still left a hang — bisect each symptom separately), and
   old revisions may not build the current frontend — ask the maintainer for a known-good
   release anchor before walking the whole history.
9. **A frozen scheduler has a signature.** When fps shows 0/60 with the cpu thread busy:
   drop a wall-clock-throttled (1 Hz) state print inside `cm_check_events` — `cycle_count`,
   `next_event_cycle`, the active-event set with due cycles, halt bits, both pcs. A live
   guest with `cycle_count` frozen short of `next_event_cycle` means nobody is adding
   cycles or jumping: some execution path runs guest code accounted at 0 cycles in a loop
   (§5.8). The same dump distinguishes that instantly from "events pending but handler
   never rescheduled" and "all events gone". For the steady-state loop itself,
   `--inst-log-lazy` + the debug port's `inst-log` command after the hang settles captures
   exactly the spin without the boot prefix.

### Audio triage

Two env valves (compiled in, off by default, cpu-thread only):

- `ADVANCEDSLOP_AUDIO_DUMP=<path>` writes every SPU sample event as an 8-byte frame — final
  output L/R plus the pre-capture mixer L/R (s16le, 32768 Hz). It taps *before* the
  transport queues, so the stream is guest-time deterministic: identical runs produce
  identical dumps, and a no-input boot can be compared sample-by-sample across builds or
  against a NooDS `pushSample` dump of the same segment.
- `ADVANCEDSLOP_SPU_LOG=1` prints channel start/stop, capture control writes and main-cnt writes
  to stderr with cycle timestamps (the per-second fps counter also prints to stderr —
  filter it out).

Reading a dump: count discontinuities (|Δsample| above ~8000) per channel and histogram
their spacing and position mod the suspected ring size. Sustained dozens per second with
full-scale clipping = emulation; NooDS-level counts (tens per minutes, at scene changes) =
content. Final-output glitches with no counterpart in the mixer stream (neither at t nor
one ring-length earlier) prove the fault is in the capture-ring dynamics rather than the
source channels — that separation is what cracked §5.7. Games that route ALL audio through
the capture units (main-cnt output-from = channels 1/3) turn any capture bug into global
crackle; the SPU log shows that configuration in the first seconds of boot.

### JIT-block annotation (jitdump)

Flat perf-map profiles name hot jit blocks but can't see inside them. The jitdump path can:

1. Run with `ADVANCEDSLOP_JITDUMP=1` (Linux builds): blocks are also written to
   `/tmp/jit-<pid>.dump` with code bytes and the thumb bit on the code address.
2. Record with a monotonic clock (required for inject): `perf record -k CLOCK_MONOTONIC -p <pid>`.
3. `perf inject --jit -i perf.data -o jitted.data` writes one small ELF per block
   (`/tmp/jitted-<pid>-N.so`); `perf report`/`perf annotate -i jitted.data` then gives
   per-instruction sample counts inside guest blocks, correctly disassembled as arm or thumb.

This needs a patched perf (kernel 7.0 sources work): genelf must set the thumb bit on the
function symbol, emit a `$t` mapping symbol when the jitdump address has bit0 set, and — when
building on an aarch64 host — be forced to emit EM_ARM/ELFCLASS32 ELFs instead of native
ones, or thumb disassembly comes out as garbage. Aggregate the per-block annotations across
the top N blocks to spot systematic emitter overhead (that's how the cross-block cpsr
round-trip and the accounting load pair were found).

Two attribution caveats: each debug-info sub-block becomes its own jitdump record, so a
branch between records looks cross-block when it's local; and a high sample count on a cheap
instruction right after a load is usually the load's latency (skid), not that instruction.

### Environment / tooling pitfalls

- **The dev machine is x86; run all roms/games on the pi5 test box.** The dev box builds the
  armhf binary but cannot run it natively — qemu-arm is ~100x slower and correctness-only
  (run-local skill), so for anything interactive or timed use the pi5 (run-on-testbox skill).
  Roms live on the pi5 at `~/gba`. Confine every box-side artifact — deployed binaries, logs,
  traces, savestates — to a `~/claude/advancedslop/` working directory so you don't pollute the home
  dir; point `ADVANCEDSLOP_PI_BIN` into it.
- **`sed -i file && cargo build` chained in one command can produce a STALE binary**: the
  edit lands in the same mtime tick as the previous build's fingerprint and cargo skips the
  recompile. This faked an A/B result and sent a whole investigation the wrong way. Run edits
  and builds as separate commands, watch for the "Compiling" line, and `md5sum` A/B binaries
  to prove they differ.
- **DEBUG_LOG stdout is huge** (GBs in minutes). Never point it at a small tmpfs: a full
  /tmp made the perf-map writer panic with StorageFull and the cpu thread died while the UI
  kept rendering — indistinguishable from an emulation hang. Redirect to a real disk, or
  pipe through `grep --line-buffered` and keep only the lines you need.
- **`pkill -f` / `pgrep -f` over ssh match the remote shell's own command line** (it contains
  the binary name) and kill your own session. Use exact-name matching (`pkill -x`) and name
  deployed test binaries distinctly.
- Test binaries built from different configs must be checksummed, not trusted by filename.
- **vitaGL has no `glReadPixels`** (it crashes) and no `glGetTexImage` — the only way to get
  texel bytes back on the Vita is `vglGetTexDataPointer`, which is also how the zero-copy
  framebuffer upload works. The read is bounded by the byte size you pass, not by the
  texture's own allocation, so a `w * h * 4` guess overruns any format that isn't rgba8
  (the palette is rgba5551 there). Raw texture memory is top-down; desktop GL is not.
- On the pi test box: Wayland — screenshot with `grim` (scrot sees black), key injection with
  `wtype`; always run with a framelimit (`-f 1` interactive, `-f 5`/`-f 9` to fast-forward
  boot); the V3D driver has a known cosmetic glyph/tile rendering offset — verify a suspected
  rendering bug against a pure-jit build and another renderer before blaming emulation.
  Under qemu+Xwayland, mouse injection works but keyboard does not reach SDL.
- Throughput benchmarking: navigate to the test scene at `-f 1` (input choreography timed
  against log-line counts breaks at uncapped speeds), then uncap live with F10 (F1-F9 set
  framelimit 1-9) and average ~40 seconds of the per-second vblank log. Same build type,
  same scene, back-to-back runs — the box drifts thermally between sessions.

---

## 5. Case studies (root causes worth remembering)

### 5.1 Stale JIT blocks across code reload
A title crashed/hung on save-resume, interpreter-only, deterministic. The causal chain ran
backwards from a wild read through a stale callback pointer into reloaded code that flowed
off its end into data: the jit cache kept compiled blocks of the *previous* code at the
same base while guest memory correctly held the new one, because the invalidation hook
covering that reload path existed only in compiled code and the pcs ran interpreted.
Guest memory was right; the code cache was stale. Lesson: **when an invalidation hook
exists only in compiled code, interpretation is a correctness hazard, not a perf choice**
(§3). Also: games legitimately load different code at one base address — "identical memory
content, different executed code" means code-cache staleness, not memory corruption.

### 5.2 HLE side-effect divergence poisons trace diffs
The same hunt was nearly derailed: the first "divergence" (a +0x28 heap shift) was the HLE
cpu-copy function not replicating the real function's r1 post-increment. The interpreter
matched hardware; the JIT's HLE didn't; the game tolerates both. Real bugs hide among such
benign divergences — hence playbook §4.7 (third reference). If HLE functions ever get
side-effect-exact (r1/r12/flags), interp-vs-jit diffs become directly trustworthy again.

### 5.3 Interrupt starvation by quantum resonance
A title hung forever on a black screen at boot, cpu busy. Event counting (§4.4) showed
thousands of irq requests, zero dispatches, and every dispatch attempt logging
"can't interrupt" with irqs disabled. The guest's send loop toggles cpsr.I with a fixed
per-iteration cycle count; the scheduler quantum expiry **phase-locked into the
irq-disabled window**, so the imm event that dispatches interrupts always found I set and
the pending irq starved — while hardware takes the irq the moment I clears. Fix: on an
irq-enable that finds a request pending, saturate the cpu's cycle budget so the existing
taken-branch scheduler check fires at the next branch (coherent pc, ≤1 quantum of skew).
Lesson: **fixed-cadence guest loops can resonate with any fixed scheduling quantum**; when
an event "never fires", check whether the guest controls the phase at which the event
processor runs. Also: prefer routing fixes through the existing branch-point checks over
adding new mid-block breakout paths — a first attempt that emitted a breakout call inside
`msr` blocks faulted on register-allocation contract violations (§1) and was abandoned for
the quantum-saturation design, which needed no emitter changes at all.

**The counter-case (found by bisecting a boot wedge back to this very fix):** an HLE
routine that synthesizes an irq synchronously inside the guest's own request leaves it
pending the moment the guest re-enables interrupts — a timing no hardware produces.
Forcing prompt delivery then interrupts a title inside a critical-section exit its SDK
never expected an irq in, and it wedges. Neither extreme works: never forcing starves one
title, always forcing wedges another — and making the forced quantum time-honest is NOT
enough, the delivery point itself is the poison. Resolution shape: defer the forced
dispatch only for the HLE's synthetic irqs (they deliver at quantum expiry, the latency
the mode always had); physical irqs keep the forcing. **Any change to irq-delivery timing
needs canary titles sensitive in both directions.** The clean long-term fix is giving
synthetic irqs real latency (a scheduled event instead of a synchronous push).

### 5.4 Mid-pattern hotness defeats interpret-then-compile
The hotness threshold normally guarantees "interpreted now, compiled soon, substitution
eventually" — but a loop whose back-edge target is mid-pattern compiles from the wrong
start pc and the pattern match never fires. Any compile-time pattern substitution is only
reachable from the pattern's FIRST pc; interpret-then-compile does not self-heal there
(the reason §3's class of pcs must bypass the interpreter outright).

### 5.5 Diagnosing a black screen: no-irq spin vs starved irq vs dead thread
Three distinct black screens seen, distinguishable in minutes with §4.4: (a) zero irq
traffic after boot lines = spinning before irq setup; (b) irq requests
without dispatches = starvation (5.3); (c) healthy irq traffic but a dead render = look at
the frontend/GL or a crashed helper thread (the StorageFull panic killed the cpu thread
while the UI kept running).

### 5.6 Stale guest LR: the return-stack safety net hides the wrong turn
A deep, deterministic corruption crash (a callback table zeroed mid-frame → null call →
garbage walk → unmapped-dispatch panic) traced back to one interpreter path (cond-0xF
`blx imm`) that fed the return address to the return-stack push but never wrote the guest
r14. The callee's `bx lr` then returned to the PREVIOUS call's lr — into the middle of an
outer function. The return-stack mismatch was absorbed by the exit-guest-context safety
net (execution "resumed" at the stale lr looking almost legitimate), the outer function
re-ran half its body with sp still one frame low, and its epilogue stores landed on its
caller's locals — the callback table. Lessons: the safety net converts a hard wrong-turn
into subtle downstream corruption, so treat real-address `failed to branch lr` records as
primary evidence (§4.5); and a function's own pcs executing at a never-before-seen sp is
the fingerprint of a broken call/return upstream, not of the function itself.

### 5.7 Audio event-grid drift desyncs a driver's ring processing (crackle)
Crackling in real games; the pre-transport audio dump reproduced it (thousands of
full-scale 4-6-sample bursts, positions drifting slowly mod a ring size) while the
reference emulator on identical content had a few dozen legitimate transients. The chain:
the game's sound driver processed a ring buffer in place on a timer alarm, trusting that
its alarm and the sample-consumer's write head stay phase-locked (on hardware both derive
from one crystal). Every periodic event rescheduled itself from the overshot
dispatch-time clock, so the sample grid slipped a few cycles per event while timers
slipped a few per *overflow* — a net drift of ~2 samples per alarm period, and the
driver's in-place pass re-processed seam samples that hadn't been refreshed yet:
double-applied transform ≈ 2× amplitude, clipping — exactly the observed bursts. Fix:
`schedule_from_due` (§1). Diagnosis chain worth keeping: deterministic dump →
discontinuity histogram → mixer-vs-final separation → a reference-emulator ring-watch
(log CPU stores + sample writes + reads with positions) revealed the driver's exact
chunking → the drift was then one interval-histogram away (56% of sample events fired
1-10+ cycles late, compounding). Lesson: **any guest feedback loop closed through
emulated hardware (ring buffers, CPU-streamed audio) turns relative timing drift between
event grids into data corruption** — grid exactness matters even where "a few cycles
late" looks harmless.

### 5.8 One stale byte offset in emitted code = engine-wide livelock
A title never booted in one engine configuration: 0 fps forever, cpu thread spinning, all
scheduler events pending, clock frozen a few hundred cycles short of the next event (§4.9
signature). The emitted idle-loop exit "set" the idle flag by writing bit 7 of
`data_packed+3` — the layout of an old 32-bit packed struct. A repack to one byte moved
`idle_loop` to bit 1 of byte 0; the emitter kept emitting the stale offset, which now
landed in struct padding, so the idle check never became true. In one configuration the
damage was invisible (its idle exit flushes real cycles, so the scheduler kept advancing
and only the fast-forward optimization silently died); in the other the idle exit reports
0 accumulated cycles — the scheduler executed the "not idle" block for 0 cycles, added 0
to the clock, and never reached any event. Fixes and lessons: the flag mask is now a
named constant beside the struct with a debug assert tying it to the bitfield, so a
future repack can't strand the emitter again — **hand-computed field offsets in emitted
code are invisible to the type system; pin them to the struct declaration**. And:
identical dead code can be fatal in one configuration and asymptomatic in another —
"works over here" never exonerates shared emitted-code patterns.

### 5.9 HLE LZ77UnCompVram byte-writes corrupt every decompressed background
Pokémon Emerald's Game Freak logo, intro cutscene, and title screen all rendered as garbled
horizontal green bands — coherent-but-wrong, on fully static frames. The renderer looked
guilty, but every stage (text/affine/obj tile addressing, the multi-screenblock jump, the
priority blend) matched NooDS's GBA paths line for line, and instrumentation proved the
frames were 100% static: **zero mid-frame VRAM/palette/OAM writes and one distinct scroll
value across all 160 lines**, so "no scanline rendering" was ruled out as the cause.

The bug was in the *data*, not the renderer. Isolation method worth reusing: dump the frame
state (BG VRAM + palette + bg regs) to a file, re-render it offline with a from-scratch
reimplementation of the shader algorithm, and diff against NooDS. Feeding *NooDS's* VRAM
into that reimplementation produced the correct scene; feeding *AdvancedSlop's* VRAM produced the
same garble as the GL renderer — proving the algorithm was right and AdvancedSlop's VRAM contents
were wrong. A byte-level diff of the two tilemaps showed AdvancedSlop had `0x3030` (tile 0x30)
where NooDS had `0x3000` (tile 0), i.e. the low byte of every 16-bit entry was clobbered by
the high byte's value — the fingerprint of the **VRAM/palette 8-bit-write-duplicates-to-the-
halfword** quirk.

Root cause: the HLE `LZ77UnCompVram` (SWI 0x12) and `RLUnCompVram` (SWI 0x15) emitted their
decompressed output byte-by-byte (`mem_write::<u8>`), which for a VRAM destination duplicates
each byte across the halfword. On real hardware the *Vram* decompression SWIs buffer bytes
and store **16-bit units** precisely because VRAM can't be byte-written; only the *Wram*
variants (0x11 / 0x14) write bytes. Both variants shared one HLE function that always
byte-wrote. Fix: emit each output byte through a 16-bit read-modify-write, so the store is a
halfword (no duplication) while the destination stays byte-exact for LZ77 back-references.
Lesson: **any HLE routine whose real-hardware counterpart writes VRAM/palette must store
16-bit units** — a byte store there is silently corrupting, and the corruption only surfaces
downstream as a rendering artifact, which sends you hunting in the wrong subsystem. The
"re-render the dumped state offline and compare to the reference" trick is the fastest way to
partition a rendering bug into *data* vs *renderer*.

### 5.10 `jit.init()` costs 11 ms, and almost all of it is the rom tables
Rewind restores a state every frame while the input is held, and each restore has to drop
compiled blocks (§5.1: the restored RAM may no longer contain the code a block was compiled
from, and the jit's SMC tracking only watches *guest* writes, not our restores). Reusing
`jit.init()` for that measured **11.1 ms per restore on the pi** — 66% of a frame budget —
against 123 µs for the state walk itself. It still held 60 fps, which is exactly why it
needed measuring rather than eyeballing: the headroom was gone and the Vita's A9 would have
fallen off a cliff.

`init` refills tables sized for the whole address space, and the rom-keyed ones dominate:
`JitEntries.rom` is `ROM_SIZE / 2` pointers (**64 MB**) and `JitExecCounts.rom` is
`ROM_SIZE / 2` bytes (**16 MB**). ~80 MB of memset per call. The ewram/iwram tables together
are under 1 MB.

The fix is `JitMemory::invalidate_ram_blocks()`: the same shape as the `invalidate_block`
SMC path (clear entries, live ranges, hotness counts) applied to all of ewram/iwram, and
**nothing else**. 11.1 ms → 120 µs.

Two invariants make it sound, and both must hold for any future caller:
- **The rom cannot change under a restore.** It lies outside `SAVESTATE_SHM_RANGE`, is
  written once by `cartridge_load_rom_into_shm`, and the rewind ring is cleared on game
  change and on a savestate load. So rom-derived blocks stay valid and their tables need no
  refill. (`create_jit_blocks!` already documents rom as "immutable and large".)
- **Do not touch the code allocator.** `init` also resets `arm7_data`, which lets future
  compiles overwrite existing code — fine when every entry is being cleared, fatal if rom
  entries survive and keep pointing at it. Orphaned RAM blocks are reclaimed by
  `reset_blocks` exactly as after a normal SMC invalidation. For the same reason the
  per-page `guest_inst_*` metadata is left alone: it is keyed by *host* code page, not guest
  address, and the allocator owns its lifetime.

`JitMemoryMap` holds pointers *into* the entry/count arrays, so filling them in place needs
no rebuild. The savestate file-load path deliberately still uses the full `init` — 11 ms once
is invisible, and it rebuilds the mmu anyway.

---

## 6. Performance lessons — measured on hardware, do NOT retry

All tried and reverted for zero or negative gain:

1. Back-edge cycle-accounting merge (paired u16 ldr/str).
2. Inlining `pre_branch`.
3. SPU sample batching, both variants (one broke frame pacing — the 1024-cycle SpuSample
   event cadence IS the frame limiter; do not batch it).
4. NEON-vectorizing scalar hot-loop math — much slower: Cortex-A9 punishes NEON↔integer
   register transfers in scalar loops; the crossing stall dominates.
5. Bulk struct copy + field patch in the vertex path — slower.
6. perf frame-pointer callgraph infrastructure — fundamentally unsound on this target
   (r7-thumb/r11-arm fp split; JIT emits host code in guest mode). Flat profiles work via
   the perf map (`/tmp/perf-<pid>.map`, `ARM7_<guest_pc>` symbols); callgraphs need dwarf.

Second round (JIT/dispatch/SPU, block-annotation-driven), also tried and dropped after
hardware A/B showed no gain on the target:

7. Weakening the SPU sample-queue lock from seqcst to acquire/release. On the Linux test
   box with audio enabled this was a 5.4x throughput unlock (the seqcst spinlock ran two
   fenced atomics per pushed sample against a busy-waiting consumer core) — on the target
   it measured flat. The contention was an artifact of the test box's audio-thread design
   and core count, not of the emulator.
8. Merging the per-branch cycle accounting into one 32-bit load/store pair (the two u16
   counters share a word). Fewer memory ops, no measurable gain anywhere.
9. Caching decoded per-channel SPU cnt fields (volume/divider/panning/format) and direct
   format dispatch instead of a fn pointer.
10. Doing the lighting dot products scalar (smull/smlal) instead of neon-with-lane-extract
    on the target build. Annotation showed a big stall on the neon-to-core transfer, but
    the hardware disagreed with the fix.
11. The div peripheral's 64-bit modes taking a 32-bit library division when operands fit,
    and a lookup table for the sound driver's bounded rate curve.
12. Removing the unaligned-rotate emission after fastmem loads (the lsl/ror pair) — the
    hot annotate samples on those lines are load-latency skid, not the rotate itself.

What DID survive hardware measurement from that round: building through skip-one branches
of de-conditionalized SDK functions plus HLE of their nocond sends; HLE of the thumb
lcg-keystream crypt family (pattern substitution now covers thumb bodies; read constants
from the guest literal pool, never assume them); carrying the guest cpsr in host lr across
the local-branch accounting instead of a memory round-trip; flags-only cpsr restores in the
naked-asm helpers.

Meta-lessons:

- The geometry/JIT/SPU core is tuned out; instruction-count reasoning has repeatedly been
  wrong about it. Don't micro-optimize the core without a profile showing the target
  dominating. The interpreter (skipping compilation of code executed < threshold times)
  is the one structural perf avenue that survived.
- Fence and contention costs are nearly invisible to leaf sampling: the seqcst lock cost
  ~1% of samples while capping throughput at a fifth. Only A/B throughput runs catch this
  class. Conversely, a big win on the dev box can be worth exactly nothing on the target —
  different core count, memory model cost, and thread architecture. No perf change counts
  until the target hardware measured it; keep each one in its own commit so they can be
  accepted or dropped independently.
- Audio-on and audio-off are different benchmark baselines (they exercise different SPU
  paths and thread interactions). Compare like with like.

Interpreter-specific perf that DID land: sequential opcode fetch (resolve the code's shm
offset once per straight-line run, re-resolve on 4KB page cross or jump), cached ThreadRegs
pointer in the dispatch context, the flat loop for cold branch targets, the AL-condition
fast path, batched ldm/stm, store-only breakout checks.

### Link-time hot/cold symbol ordering (July 2026 — pi-verified mechanism, vita verdict pending)

`build.rs` passes `-Wl,--symbol-ordering-file=tools/symbol_order/<target-triple>.txt` to lld
whenever that file exists. Listed (profiled-hot) functions are laid out contiguously in list
order; everything unlisted — the cold bulk — lands grouped together, so the hot working set
spans the minimum number of icache lines and iTLB pages.

Workflow:

1. Flat profile on the box, then
   `perf report --no-children --no-demangle -s dso,symbol -g none --stdio --percent-limit 0.01`
2. `tools/gen_symbol_order.sh <report.txt> <dso-name> tools/symbol_order/<triple>.txt`
3. Port to the other target (hash suffixes differ per target):
   `tools/match_symbol_order.sh <src-order.txt> <other-target.elf> <out.txt>` — matches by
   demangled name with `::h<hash>` stripped; a key with several monomorphizations emits all.

Gotchas (each cost a debug loop):

- rustc does NOT surface linker warnings on successful links, so a stale ordering file
  (symbol hashes churn with any code change) silently degrades to a no-op. After
  regenerating, verify with `llvm-nm --numeric-sort` that the list entries sit contiguous.
- Match against a FRESHLY built elf — hashes in a stale `target/` binary may already be
  wrong for the current tree.
- The vita `ldscript.ld` (SECTIONS script) is compatible: lld still applies the ordering,
  but places the ordered group mid-`.text` (branch-distance heuristic) rather than at the
  front, and `.text.startup` functions (e.g. `actual_main`) stay in their own script group.
  Grouping is what matters; both are fine.
- Vita link needs `OPENSSL_DIR=/usr/local/vitasdk/arm-vita-eabi OPENSSL_STATIC=1`.

Measured on the pi (HG town savestate, uncapped, warm, per-instruction rates): iTLB misses
-29% (0.68→0.49 MPKI), L1-icache misses -6% (4.83→4.59 MPKI), throughput +0.6% — inside
noise on the out-of-order A76, as expected when the front-end stall isn't the bottleneck.
The in-order A9 should benefit more; per the verdict rule this ships only if vita hardware
measures a win.

### Dispatch-spine follow-ups from the miss profile (July 2026, vita verdicts pending)

The 5-event miss profile put ~25% of all branch mispredicts and ~25% of all L1I misses in
the jit↔native dispatch funnel (branch_lr alone owned 7.2% of every icache miss). Two
changes came out of it:

- Cold-tail outlining of the handlers (#[cold] mismatch/exceed/interrupt paths): pi-neutral
  — the icache pressure is jit-code capacity eviction, not handler size. Kept as the
  structural base for the next item; drop if vita also measures nothing.
- **Emitted BX-LR return fast path** (both backends): the match case runs inline in
  compiled code, slow cases tail into branch_lr_slow. pi5: **+4.0% throughput**
  (interleaved A/B, well above noise) even though emitted code volume grows (L1I misses
  rose 4.6→5.1 MPKI) — the win is the removed native round trip per guest return, not the
  front end. Gate = jit-vs-jit determinism pairs (byte-identical HG-boot ilogs both
  arches; hello_world residuals are host-address bookkeeping records only — "Enter jit
  addr" and fastmem fault logs embed per-process host pointers, benign across builds).

---

## 7. Known open items / divergence suspects

- ARM-side interpreter `ldm_stm` doesn't model the ARM7 stm-base-writeback quirk (the JIT
  and thumb `stmia_t` do) — suspect if an ARM7-heavy title misbehaves interpreter-only.
- HLE cpu-copy/fill functions don't replicate the real functions' r1/r12/flags side effects
  — benign vs hardware-tolerant games, but pollutes trace diffs (5.2).
- GPU scanline events still reschedule from the dispatch-time clock (see §1 event grids) —
  the frame grid drifts a hair slow. Harmless so far; convert to `schedule_from_due` if a
  vblank-vs-timer sync bug ever shows.
- The interpreter now covers the full instruction set (the former fallback list — SWI,
  MCR/MRC, LDRD/STRD, SWP, DSP muls, cond=0xF space, user-banked ldm/stm, empty rlists —
  is implemented from the NooDS reference). Open disagreement: the disassembler's cycle
  values differ from NooDS for SWP (4 vs 2), LDRD (3 vs 2) and the ldm/stm formula;
  the jit charges the disassembler, the interpreter follows NooDS for the new ops.
  Needs one source of truth (§1 cycle accounting says: mirror the jit).

---

## 8. The renderer (software PPU across three cores)

There is no hardware 2D path any more. The PPU is a scanline software rasterizer, split
across every core the Vita gives an application, and the emulation thread does not take
part in it beyond producing a snapshot. Everything below was arrived at by measurement on
the pi5 and by two failures on Vita hardware; the invariants are load-bearing.

(An earlier revision split the work by *layer* — bg0/bg1 on one core, bg2/bg3 on the
other, compose on the present thread. The scanline split replaced it: balanced by
construction in every bg mode, compose off the present thread, and the ~500K/frame of
cross-core layer buffers gone. Measured on the pi5 in the Emerald intro-speech scene
(savestate `Pokemon - Emerald Version (USA, Europe)-1.sav`, uncapped, back-to-back
sandwich): per-frame core shares 461/111us + 428us compose on the present thread before,
287/295us and no compose after — render critical path -36% — and 386 -> 404 fps (+4.5%)
even though the renderer never blocks the emulation thread, from bus traffic alone.)

### Core and thread map

| core | thread | job |
|---|---|---|
| 0 | `raster0` worker | lines 0-79: objects, all four bgs, compose |
| 1 | `raster1` worker | lines 80-159: same |
| 1 | main (owns the GL context) | upload, blit, swap |
| 2 | `cpu` | emulation; at vblank, snapshot and hand over |
| 0 | `audio_out`, `save` | mostly blocked on their devices |

One thread per core, single-core affinity masks, never a mask the scheduler can choose
within — the Vita's scheduler makes a mess of anything left to it. The two threads on
core 1 do not really contend: the main thread is parked inside `vglSwapBuffers` for most
of a frame, which is exactly why the rasterizing half must not be on it (see D3 below).

### Frame flow

```
cpu thread (core 2)              rasterizers (cores 0,1)         present thread (core 1)
------------------               -----------------------         -----------------------
hblank of each line:
  capture_regs[line] = PpuRegs

vblank:
  slot = write_slot
  if slot not free -> skip frame
  swap capture_regs <-> slot.regs
  slot.mem.snapshot(shm, dirty)
  slot.state = QUEUED  ------->  wake
  (returns to the guest)         decode slot's oam, then per
                                 line of own half: objs, four
                                 bgs, compose -> pixels[i]
                                 slot.state |= HALF[n]
                                            -------->  wake, when state == RASTERED
                                                       slot.state = 0  (frees it)
                                                       upload, blit, swap pixels[i]
```

Nothing on the left waits on anything to its right. The emulation thread's entire cost is
the snapshot: 0-2 us a frame in practice, because of the dirty tracking.

### The swapchain

`SWAPCHAIN = 2` slots. A slot owns a frame's inputs:

| per slot | size | written by | read by |
|---|---|---|---|
| `GpuMemBuf` (vram/pal/oam snapshot) | 98K | cpu thread | rasterizers |
| `GbaRenderRegs` (PpuRegs x 160 + dispcnt) | ~15K | cpu thread | rasterizers |

A slot has no outputs any more: each rasterizer composes a line right after drawing it,
straight into the pixel buffer, and everything in between — the line's layers, the
selection, the frame's pre-decoded oam — is per-worker scratch (`RasterScratch`, ~19K)
that never leaves its core, or its L1 for the layer parts.

Slot state is one `AtomicU8`, `RenderSync::slots[slot]`:

- `0` — free. Only the cpu thread may write the slot's inputs.
- `S_QUEUED` — inputs are complete; the rasterizers own them.
- `| S_HALF[0]`, `| S_HALF[1]` — each rasterizer ors its own bit when its half of the
  scanlines is drawn *and composed*.
- `S_RASTERED` (all three) — the present thread owns it. It zeroes the state when done,
  which is the only transition back to free.

Every bit has exactly one setter and one clearer. Slots are produced and consumed strictly
in order, so each thread walks its own cursor (`write_slot`, per-worker `slot`,
`read_slot`) round the ring rather than searching for work.

The pixel targets are separate and five deep (`FRAME_BUFS`), round-robined in lockstep by
every consumer's own cursor (each advances exactly once per slot, from the same start).
Five, because the producers moved two frames ahead of the present: the workers write
frame n's buffer as soon as frame n-2's slot frees, which happens right after frame n-3's
swap returned. A buffer's own swap was frame n-5, so two later swap-returns always sit
between a swap and that buffer's rewrite — the same driver-still-reading slack the old
three-deep chain gave a compositor that wrote only after presenting the previous frame.

### Invariants

1. **Nothing downstream may read live guest memory.** The rasterizers read the slot's
   snapshot. This is what lets the cpu thread hand over and leave.
2. **A slot's inputs are written only while its state is 0.** The frameskip check is that
   test; there is no other guard.
3. **The present thread frees the slot before presenting, not after.** The present may
   park on vblank for most of a frame; holding the slot across it would cost frames.
4. **Objects are split by scanline, never by sprite.** `draw_objects` resolves a sprite's
   pixels against whatever is already in the line buffer, ordered by OAM index. Splitting
   the sprite walk would change which sprite wins a tie. Lines are independent.
5. **Layer buffers persist across lines and frames, and every draw writes only the pixels
   it covers,** so each one is cleared before it is drawn — transparency is the *absence*
   of a write. Missing a `fill(0)` leaves the previous line's pixels showing through.
   (The tile-row zero-skips in the draws depend on this: an all-transparent row is
   *skipped*, not written as zeros.)
6. **Dirty bits are per slot.** A slot two frames old needs everything that has changed
   since *it* was last filled, not just the last frame's writes. `on_frame_finish` ors the
   frame's dirty bits into every slot, then consumes and clears only its own.
7. **Per-scanline register capture cannot be dropped.** Games rewrite scroll offsets,
   affine references, windows and blend settings mid-frame from the hblank IRQ. The
   snapshot is taken at vblank, so the registers must be captured per line at their own
   hblank or every such effect is lost.
8. **`set_quit` must wake the sleepers.** All three consumers wait with a 20ms timeout and
   re-check `quit`, so teardown cannot hang, but the wake makes it prompt.

### The per-line pipeline

`ppu.rs` exposes one entry point per line, `draw_scanline`, which runs three stages on
the worker's own `RasterScratch`:

- `draw_objs` — objects, the window mask, and compose's registers (backdrop,
  BLDCNT/BLDALPHA/BLDY, forced blank). The window mask belongs here because it depends on
  the object window. Sprites come from the frame's pre-decoded oam (`OamScratch`,
  filled once per frame by `begin_frame`): per-line lists of only the sprites that cover
  the line, attributes and affine parameters already unpacked. Only the two things
  DISPCNT can change mid-frame — the 1D-mapping stride and the bitmap-mode charblock
  rule — are still resolved per line.
- `draw_bgs(base)` — two background layers, `base` selecting bg0/bg1 or bg2/bg3, plus
  which of them are active and at what priority.
- `compose_scanline` — per pixel: layer selection, window effects gating, BLDCNT
  blending, straight into the frame buffer row. Reads nothing the guest owns; it runs
  right after the draws on the same core, while the layer lines are still in L1.

The layers arrive as two bg pairs plus the obj line, so compose resolves them per line
into a priority-ordered `BgCandidate` list and the selection walks that instead of
indexing a four-layer array.

`select_layers_neon` is the vector form of the selection; `ADVANCEDSLOP_CHECK_SIMD=1`
cross-checks it against `select_layers_scalar` every scanline. That check is
`cfg!(debug_assertions) && check_simd()`, so release drops the second pass entirely.

Hot-path shortcuts, all behavior-preserving:

- **Tile rows fetch as one load** (u32 for 4bpp, u64 for 8bpp) in the text-bg and
  regular-sprite draws, and an all-zero row skips its whole run — transparent margins
  dominate real text/hud layers and sprites. Rows are 4/8-aligned and the vram mirror
  folds at 8-aligned boundaries, so a row never straddles the fold.
- **Compose has a no-blend fast path**: when the BLDCNT mode is off and no
  semi-transparent obj pixel landed on the line (a per-line flag from `draw_objs`; semi
  objs force alpha regardless of mode but still need a second target), every pixel is
  just the top selection or backdrop, converted 555→8888 eight at a time in NEON. Blend
  lines still resolve per pixel, into a 555 temp that takes the same NEON conversion.
- **Affine-bg wrap is a mask**, not `rem_euclid`: the size is a power of two, and the
  Cortex-A9 has no integer divide — rem_euclid on a runtime divisor is a libcall per
  pixel, twice.
- **Bitmap modes have an identity fast path** (pa == 1.0, pc == 0, the common case):
  straight run decode instead of stepping the matrix per pixel.

### Reading the stats

Debug-assertion builds (dev, release-debug) print one line a second; in release the
stats struct is empty and every recording call a no-op:

```
60 snapshot 3us core0 301us core1 308us present 502us skipped 0
```

Means per frame, and the only numbers that matter for headroom:

- `snapshot` — the emulation thread's entire share. If this is large, the dirty tracking
  is being defeated (something is rewriting all of vram every frame).
- `core0` / `core1` — each worker's raster+compose share of its half of the lines. The
  scanline split makes these mode-independent; what imbalance remains is content
  (sprites or blending clustered in one half of the screen).
- `present` — the present thread's. On the Vita this is mostly the vblank park, not CPU.
- `skipped` — **the health metric.** The emulation thread runs free regardless, so skips
  are the only symptom of the render side not keeping up. Non-zero at 60fps means a
  rasterizer or the present is over budget.

To measure: run on the pi5 with the real GPU driver (do *not* force
`LIBGL_ALWAYS_SOFTWARE`, see section 4), reach a steady scene at `-f 1`, then
`framelimit 0` over the debug port and average ~30 seconds. Two traps, both hit in July
2026: deployed binary names must stay ≤15 chars or `pkill -x` silently misses them (the
kernel truncates comm) and instances pile up, all pinning the same cores; and absolute
uncapped fps in this profile is roughly halved by the per-instruction jit debug valves
(`debug_after_exec_op` alone was 34% of uncapped pi5 cycles) — compare A/B within one
profile, never a release-debug number against a release number.

### Dead ends — measured, do not retry

- **D1. Spinning while holding the sync mutex.** The emulation thread reacquired it
  microseconds at a time and the compositor could barely get in to publish its result.
  95 -> 40 fps. The handshake is atomics for this reason; the mutex exists only to close
  the lost-wakeup race for threads that genuinely sleep.
- **D2. Spinning on a read-modify-write** (`swap` in the loop) instead of loading first.
  Takes the cache line exclusive every iteration so the setter cannot get it back: another
  100 -> 40 fps. Test-and-test-and-set, or better, no spin at all.
- **D3. Rasterizing on the present thread.** `vglSwapBuffers` parks its caller on vblank
  (`sceGxmDisplayQueueAddEntry`'s callback calls `sceDisplayWaitVblankStartMulti`, and
  `vsync_interval` is 1), so that thread is asleep for most of a frame. With bg2/bg3 on
  it, the emulation thread — which back then waited for every bg layer — spun on the
  display: 100% cpu and exactly 60fps with the framelimiter off. **Nothing the emulation
  thread waits for may sit behind a present.** It now waits for nothing at all, but the
  rule stands for whatever comes next.
- **D4. Not setting the SDL swap interval.** Same shape on Linux: the host compositor's
  vsync reached back through the frame handshake and capped emulation at the display rate.
  The framelimiter is the only pacer; `SwapInterval::Immediate` is set at context creation.
- **D5. A chunked layer ring** (hand the compositor 8 lines at a time, 4 chunks in flight)
  to keep the layer buffers cache-resident. The compositor shares its thread with the
  present, so it drains in a burst and then sleeps in the swap; the ring filled and the
  cpu thread stalled on ring space for the rest of the frame. 247 -> 94 fps. Sizing the
  ring to a whole frame worked but is just the swapchain with more bookkeeping.
- **D6. Moving compositing off the emulation thread, on its own.** Traded ~0.5ms of
  compose for ~0.5ms of extra memory traffic writing the layer buffers out: 242 -> 247
  fps, i.e. nothing. The win only appears once the *rasterizing* is parallel too.
- **D7. Frame numbers in the handshake.** While only one frame is in flight there is
  nothing to compare, and per-thread frame counters were pure overhead. The swapchain
  reintroduced slots, but they are indexed by a per-thread cursor, not compared.

### Open items

- **Vita verdict pending on the scanline split.** The rewrite (scanline split, per-line
  compose, the hot-path shortcuts, `FRAME_BUFS` 3 → 5) is qemu-verified for correctness
  and pi5-measured (+4.5% uncapped, render critical path -36%; see the intro note); the
  Vita numbers and the +300K CDRAM for the two extra pixel textures have not been
  measured on hardware.
- The pi5 cannot reproduce the Vita's present cost: its `present` is ~0.5ms of real GPU
  work, not a vblank park. Anything about the present thread's scheduling has to be
  confirmed on hardware.

### File map

| file | contents |
|---|---|
| `src/core/gpu.rs` | `GbaRenderer`: the handshake, the swapchain, the three loops, `GbaRenderRegs`, `SoftRenderer` (pixel buffers, GL upload/blit) |
| `src/core/ppu.rs` | `PpuRegs`, `draw_scanline` (objs → bgs → compose), `RasterScratch`/`OamScratch`, the layer selection (scalar + NEON), the individual layer draws |
| `src/core/graphics/gpu_mem_buf.rs` | the vram/palette/oam snapshot and its dirty bits |
| `src/core/graphics/gl_glyph.rs` | the debug-stats text overlay |
| `src/core/graphics/gl_utils.rs` | `GpuFbo` and shader helpers — **always** create fbos through these; vitaGL has no `glTexStorage2D` |
| `src/lib.rs` | thread creation, affinities, per-game start/teardown |
