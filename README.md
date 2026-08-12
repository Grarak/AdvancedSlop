# AdvancedSlop

[![Rust](https://github.com/Grarak/AdvancedSlop/actions/workflows/rust.yml/badge.svg)](https://github.com/Grarak/AdvancedSlop/actions/workflows/rust.yml)

Fast GBA Emulator for ARM32/PSVita

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
- Custom control profiles can be created in the settings, but are not applied in game yet
- The PS Vita build is still being tested on hardware, Linux armhf is the most tested target

## Installation/Setup

### Vita

- Grab the latest vpk from [releases](https://github.com/Grarak/AdvancedSlop/releases)
- Install `libshacccg.suprx`, follow
  this [guide](https://cimmerian.gitbook.io/vita-troubleshooting-guide/shader-compiler/extract-libshacccg.suprx) or just install and open VitaDB
- Install `kubridge.skprx` version >= 0.3.1 from https://github.com/bythos14/kubridge/releases
    - Make sure this plugin is in the `*KERNEL` section, otherwise the app might crash upon opening
    - If you have the wrong version installed, the app will either crash or will not be able to launch any games
- It's strongly recommended to overclock your Vita to 500MHz
- Create the folder `ux0:data/advancedslop` and put your roms there
    - They must have the `.gba` file extension

### Linux

```bash
$ advancedslop [-f <framelimit>] [-a] [-s <savestate>] <rom.gba | rom directory>
```

- `-f` sets the framelimit: `0` uncapped, `1`-`9` for 100%, 125%, 150%, 175%, 200%, 250%, 300%, 400%, 500%
- `-a` enables audio
- `-s` continues from a savestate file
- Passing a directory opens the game browser

### Controls

| | Vita | Linux |
|---|---|---|
| D-Pad | D-Pad / left stick | W A S D |
| A / B | Circle / Cross | K / J |
| L / R | L / R | 8 / 9 |
| Start / Select | Start / Select | B / V |
| Pause menu | Triangle | Escape |
| Cycle screen layout | Square | F12 |
| Quick save / load | Right stick up / down | F11 / Shift+F11 |
| Framelimit | | F1-F9 (100%-500%), F10 uncapped |

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
`.env` is gitignored — it's your machine-specific setup. Development background (JIT/interpreter
invariants, debugging playbook, renderer design) lives in `DEVELOPMENT.md`.

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
