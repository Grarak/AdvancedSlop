# AdvancedSlop

[![Rust](https://github.com/Grarak/AdvancedSlop/actions/workflows/rust.yml/badge.svg)](https://github.com/Grarak/AdvancedSlop/actions/workflows/rust.yml)

Fast GBA Emulator for PSVita

## Status

Most games run, with these caveats:

- The BIOS is HLE only, no BIOS file is needed or supported
    - A few BIOS calls (`BgAffineSet`, `ObjAffineSet`, `ArcTan`) are approximations
      rather than bit-exact
- 2D rendering is mostly complete
    - Mosaic and GREENSWAP are unimplemented
- No scanline rendering of VRAM, palette and OAM. Registers are captured per scanline,
  but the memory is snapshotted once per frame
    - Games that rewrite VRAM or the palette mid-frame (e.g. for gradients) will not
      render correctly

## Installation/Setup

- Grab the latest vpk from [releases](https://github.com/Grarak/AdvancedSlop/releases)
- Install `libshacccg.suprx`, follow
  this [guide](https://cimmerian.gitbook.io/vita-troubleshooting-guide/shader-compiler/extract-libshacccg.suprx) or just install and open VitaDB
- Install `kubridge.skprx` version >= 0.3.1 from https://github.com/bythos14/kubridge/releases
    - Make sure this plugin is in the `*KERNEL` section, otherwise the app might crash upon opening
    - If you have the wrong version installed, the app will either crash or will not be able to launch any games
- Create the folder `ux0:data/advancedslop` and put your roms there
    - They must have the `.gba` file extension

## Controls

| | Button |
|---|---|
| D-Pad | D-Pad / left stick |
| A / B | Circle / Cross |
| L / R | L / R |
| Start / Select | Start / Select |
| Pause menu | Triangle |
| Cycle screen layout | Square |
| Quick save / load | Right stick up / down |
| Rewind (hold, enable in settings) | Right stick left |

The buttons, the pause menu and the screen layout hotkeys can be remapped: create a
profile under Global settings → Custom controls, then pick it in the Controls setting.
Quick save/load and rewind stay on the right stick.

Savestates can also be created, loaded and managed from the pause menu, and a game can be
resumed from a savestate on its page in the browser.

## Bug reporting

Feel free to create an issue if you run into problems. Please include the game, where it
happens and, if possible, a savestate from just before the issue.

## Building

Install [cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html)

You need to have both llvm-18 and llvm-21 installed.
- llvm-18 is required for bindgen, newer versions struggle to parse class sizes correctly
- llvm-21 is used for building C/C++ libraries

Clone the repo
```bash
$ git clone --recurse-submodules https://github.com/Grarak/AdvancedSlop.git
```

### Linux
Get an armhf sysroot with libsdl2 development packages installed
- On ubuntu >= 22.04
    ```bash
    $ sudo apt install debootstrap qemu-user-static
    $ sudo debootstrap --foreign --variant=buildd --arch=armhf jammy ./ubuntu-armhf https://ports.ubuntu.com
    $ sudo cp /usr/bin/qemu-arm-static ubuntu-armhf/usr/bin/
    $ sudo chroot ubuntu-armhf /debootstrap/debootstrap --second-stage
    $ apt update && apt install libsdl2-dev # If apt can't find the package make sure the source has "main restricted universe multiverse" defined in /etc/apt
    ```
```bash
$ LIBCLANG_PATH=<path to llvm-18 library> ADVANCEDSLOP_SYSROOT=<path to armhf sysroot> cargo build --target thumbv7neon-unknown-linux-gnueabihf --release
```

For iteration use `--profile release-debug`, it's optimized but keeps debug assertions and
the headless debug command port (`ADVANCEDSLOP_DBG_PORT`).

### Development environment file
The build variables above and the helper scripts in `tools/` (launching under qemu, remote
test box runs, screenshots, instruction traces — see `tools/README.md`) read their paths from
environment variables. Copy the example file and fill in your own setup:
```bash
$ cp .env.example .env   # then edit .env with your paths
```
The `tools/` scripts source `.env` automatically. To use it for cargo builds:
```bash
$ set -a; . ./.env; set +a
$ cargo build --target thumbv7neon-unknown-linux-gnueabihf --release
```
`.env` is gitignored — it's your machine-specific setup.

### Vita
- Install [Vitasdk](https://vitasdk.org/)
- Install [cargo vita](https://github.com/vita-rust/cargo-vita)
```bash
$ LIBCLANG_PATH=<path to llvm-18 library> cargo vita build vpk -- --release
```

### Final optimized release build
To obtain the most optimized build you need to use a [patched rust compiler](https://github.com/Grarak/rust) due to LTO incompatibility.
The upstream rust compiler doesn't set the target cpu and target features in the callsite attributes
which prevents the clang linker from inlining cross language functions.
```bash
$ RUSTC=<path to compiled rustc> LIBCLANG_PATH=<path to llvm-18 library> RUSTFLAGS="-Zlocation-detail=none -Zfmt-debug=none -Zub-checks=no -Zsaturating-float-casts=no -Ztrap-unreachable=no -Zmir-opt-level=4 -Clinker-plugin-lto -Clto=fat -Zunstable-options -Cpanic=immediate-abort" cargo vita build vpk -- --release
```

The toolchain is also pinned to an older rust nightly (see `rust-toolchain.toml`), the last one built against llvm-21 — newer llvm versions cause performance regressions.

Development background (JIT/interpreter invariants, debugging playbook, renderer design) lives in `DEVELOPMENT.md`.

## Credits

- [DSVita](https://github.com/Grarak/DSVita) AdvancedSlop is built on its JIT, frontend and Vita port.
- [NooDS](https://github.com/Hydr8gon/NooDS) was used as reference. A lot of code was taken from there.
- [DSHBA](https://github.com/DenSinH/DSHBA) PPU implementation reference
- [vitaGL](https://github.com/Rinnegatamante/vitaGL) Vita rendering wouldn't be possible without it
- [Tonc](https://www.coranac.com/tonc/text/toc.htm) GBA PPU documentation
- [GBATEK](http://problemkaputt.de/gbatek-index.htm) GBA documentation
- [kubridge](https://github.com/bythos14/kubridge) For fastmem implementation
- [rcheevos](https://github.com/RetroAchievements/rcheevos) RetroAchievements support
- [dolphin](https://github.com/dolphin-emu/dolphin) For audio stretching code
