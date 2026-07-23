---
name: run-on-testbox
description: Deploy and run AdvancedSlop on the remote ARM test box (raspberry pi class, native armhf speed) — use when asked to test a build on the pi/test box, run a game at real speed, take remote screenshots, or inject button presses remotely.
---

# Run AdvancedSlop on the remote ARM test box

Prerequisite: `.env` in the repo root with `ADVANCEDSLOP_PI_HOST` (ssh, key auth). Optional:
`ADVANCEDSLOP_PI_BIN` (default `~/claude/advancedslop/advancedslop`). Never use sudo on the box without asking.

**Working directory & roms.** The dev machine is x86 and can't run the armhf binary natively,
so the pi5 is where all roms/games run. Roms live on the box at `~/gba`. Do every box-side
thing — deployed binaries, logs, traces, savestates — under a `~/claude/advancedslop/` working
directory so you don't pollute the home dir; keep `ADVANCEDSLOP_PI_BIN` pointed into it.

## Deploy

```bash
cargo build --profile release-debug --target thumbv7neon-unknown-linux-gnueabihf
ssh "$ADVANCEDSLOP_PI_HOST" 'mkdir -p ~/claude/advancedslop'
scp target/thumbv7neon-unknown-linux-gnueabihf/release-debug/advancedslop "$ADVANCEDSLOP_PI_HOST:~/claude/advancedslop/advancedslop"
```

When testing multiple build variants, give each binary a DISTINCT name on the box and
`md5sum` them locally first to prove they differ (a chained `sed && cargo build` can
silently skip the recompile — see DEVELOPMENT.md §4 pitfalls).

## Run / observe / drive

| action | command |
|---|---|
| Kill + relaunch with args | `tools/pi_run.sh '-f 1 ~/gba/<rom>.gba'` |
| Screenshot (wayland grim) | `tools/pi_shot.sh <local.png>` |
| Press a key | `tools/pi_key.sh <xkb-key> [hold_ms]` |
| Check alive | `ssh $ADVANCEDSLOP_PI_HOST 'pgrep -x <binary-name>'` |

Keyboard map: WASD = dpad, K = A, J = B, B = Start, V = Select, 8/9 = L/R.

**Debug command port** (debug/release-debug builds) — the headless control channel; needs no
wayland virtual keyboard (so it works even where `wtype`/`pi_key.sh` is absent). Launch with
`ADVANCEDSLOP_DBG_PORT=<port>` and send newline-delimited commands to `127.0.0.1:<port>` on the box,
e.g. `printf 'buttons a\n' | nc -q0 127.0.0.1 5555`:
- `press/release <btn>` | `buttons [<btn>...]` (exact held set) — btn: `a b up down left
  right start select l r`. A held button = held on the GBA.
- `framelimit <0..9>` (0 = uncapped) | `pause` (open the pause menu) | `savestate` (quick-save
  at vblank) | `inst-log` (arm a `--inst-log-lazy` capture) | `quit`.

**The port also drives the imgui menus** (rom browser, settings tabs, RetroAchievements
overlay), so menu flows are testable headlessly — this matters because the box has no
`wtype`/`xdotool`, it is a wayland session (clients cannot synthesise input), and
`/dev/uinput` is root-only. Held buttons are injected into imgui's keyboard state:
`up/down/left/right` move nav, `a` = activate (Return), `b` = back/cancel (Escape).
So `buttons a` then `buttons` (release) is one activation — nav is edge-triggered, so
every press needs a matching release.

The browser is imgui-native nav (no hand-driven selection). What is
verified over the port: the focus ring walks the rom list, crosses up to the Global
settings button, `a` activates buttons and rows, and the overlay pages (Global settings
and its sub-pages, the game detail page) navigate and activate. What is NOT reliable:
`b` (Escape) closing an overlay sometimes does not take over the injection path —
verified working with real input; when driving headlessly, prefer navigating forward or
relaunching over depending on Back.

Sequence timing in ONE ssh session (loop the `nc` sends with short sleeps); a command per ssh
round-trip is too slow and the game "heals" between them.

**Input-free verify loop**: the port's `savestate` (or the F11 key) quick-saves at vblank;
relaunch with `-s <savestate>` to resume — lets you A/B or re-test a scene deterministically
without re-driving inputs.

Rules that bite:
- **Do NOT force `LIBGL_ALWAYS_SOFTWARE=1` for anything timing-related.** It was mandatory
  while the GL 2D pipeline existed (the box's V3D driver garbled its layers), but that
  pipeline is gone: the gpu now only uploads one texture and blits it, which V3D renders
  correctly. Under llvmpipe that blit costs ~9ms a frame and dominates every measurement —
  it made the present thread, and through it the emulation thread, look 3x slower than it
  is. `tools/pi_run.sh` still exports it; run the binary by hand when measuring.
- ALWAYS pass a framelimit: `-f 1` for interaction, `-f 5`/`-f 9` to fast-forward through
  boot/loading. Uncapped (`-f 0`) is uninteractable.
- `pkill -f` / `pgrep -f` over ssh match the remote shell's own command line and kill your
  session — use `-x <exact-binary-name>` and name binaries distinctly.
- The box's `/tmp` is a small tmpfs: redirect big logs (DEBUG_LOG stdout, traces) to the
  home directory, never `/tmp`. A full `/tmp` kills the cpu thread with a StorageFull panic
  while the UI keeps rendering — looks exactly like an emulation hang.
- The box's V3D GL driver has a known cosmetic tile/glyph rendering offset; verify suspected
  rendering bugs against a pure-jit build or another renderer before blaming emulation.
