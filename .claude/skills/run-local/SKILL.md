---
name: run-local
description: Run AdvancedSlop locally on the dev machine under qemu-arm and take screenshots — use when asked to launch the emulator locally, boot a rom for a quick correctness check, open the menu UI, or capture what the local emulator window shows.
---

# Run AdvancedSlop locally (qemu-arm)

Prerequisite: a `.env` in the repo root (copy `.env.example`, fill in `ADVANCEDSLOP_SYSROOT`,
`ADVANCEDSLOP_ROMS_DIR`, `ADVANCEDSLOP_TEST_ROM`). All tools/ scripts source it automatically.

Build first (dev profile has overflow-checks + debug-assertions on):

```bash
cargo build --profile release-debug --target thumbv7neon-unknown-linux-gnueabihf
```

| action | command |
|---|---|
| Boot the test rom (`$ADVANCEDSLOP_TEST_ROM`) | `tools/launch_hw.sh` |
| Open the menu UI on `$ADVANCEDSLOP_ROMS_DIR` | `tools/launch.sh` |
| Screenshot the emulator window | `tools/shot.sh <out.png>` |
| Stop | `pkill -9 -f "release-debug/advancedslop"` |

Runtime log: `/tmp/advancedslop.log`. The emulator runs detached; give it a few seconds before
screenshotting.

Constraints:
- The dev machine is x86, so the armhf binary only runs here under qemu-arm — ~100x slower
  than native, fine for a quick correctness check, useless for perf. Default to running
  roms/games on the pi5 test box (run-on-testbox skill; roms there live at `~/gba`); reach for
  qemu locally only for a fast boot/correctness sanity check.
- Under Xwayland, mouse injection reaches SDL but keyboard does NOT — touch-navigable flows
  can be driven (mouse click on the bottom-screen area = touch), button-only flows cannot.
- CLI: `-f 0|1|2..` framelimit (0 = uncapped, 1 = 100%, 5 = 200%, 9 = 500%), `-e 0|1|2` =
  ARM7 mode (AccurateLle / SoundHle / Hle). Menu mode: pass a roms *directory* as the
  positional rom argument (there is no `--ui` flag; passing one misparses).
