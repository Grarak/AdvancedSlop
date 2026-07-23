#!/bin/bash
# armv7-sacred byte-identity gate (port plan S3/S5): capture the per-block
# (cpu, guest_pc, thumb, len, xxh32(code)) stream over deterministic qemu boots and
# compare against a baseline. Any refactor of shared jit code must keep the stream
# byte-identical.
#
# Usage:
#   armv7_gate.sh capture <outdir> [dur=75]   # build threshold-0 + boot 3 roms, save streams
#   armv7_gate.sh compare <dir_a> <dir_b>     # prefix-compare the streams (min 500 blocks)
set -e
. "$(dirname "$0")/env.sh"

MODE="$1"

roms() {
    # rom path | arm7 mode | log name. Changing this list invalidates stored baselines —
    # re-capture them after editing.
    echo "$ADVANCEDSLOP_TEST_ROM|0|hello_world"
    echo "$ADVANCEDSLOP_ROMS_DIR/Pokemon - Emerald Version (USA, Europe).gba|0|emerald"
    echo "$ADVANCEDSLOP_ROMS_DIR/Pokemon Unbound (v2.1.1.1).gba|2|unbound"
}

case "$MODE" in
capture)
    OUT="$2"
    DUR="${3:-75}"
    [ -n "$OUT" ] || { echo "usage: armv7_gate.sh capture <outdir> [dur]" >&2; exit 2; }
    require_env ADVANCEDSLOP_SYSROOT ADVANCEDSLOP_ROMS_DIR ADVANCEDSLOP_TEST_ROM
    mkdir -p "$OUT"
    cd "$ADVANCEDSLOP_ROOT"

    INTERP_RS=src/jit/interpreter/mod.rs
    trap 'git checkout -- '"$INTERP_RS"' 2>/dev/null' EXIT
    sed -i 's/^pub const INTERP_THRESHOLD: u8 = .*/pub const INTERP_THRESHOLD: u8 = 0;/' $INTERP_RS
    # The whole tree gets touched: gate runs interleave with checkouts/cherry-picks in
    # sibling worktrees, and a same-mtime-tick edit silently skips the recompile (the
    # documented sed+build race) — a stale gate binary produced hours of false FAILs.
    sleep 1.1; find src -name '*.rs' -exec touch {} +
    out=$(cargo build --profile release-debug --target thumbv7neon-unknown-linux-gnueabihf 2>&1) || { echo "$out" | tail -20; exit 1; }
    echo "$out" | grep -q "Compiling advancedslop" || { echo "STALE BUILD (no 'Compiling advancedslop')" >&2; exit 1; }
    md5sum target/thumbv7neon-unknown-linux-gnueabihf/release-debug/advancedslop | tee "$OUT/binary.md5"

    BIN=target/thumbv7neon-unknown-linux-gnueabihf/release-debug/advancedslop
    roms | while IFS='|' read -r rom emu name; do
        echo "== $name (${DUR}s) =="
        ADVANCEDSLOP_BLOCK_HASH_LOG="$OUT/$name.blocks" qemu-arm -L "$ADVANCEDSLOP_SYSROOT" "$BIN" "$rom" -e "$emu" -f 0 >"$OUT/$name.log" 2>&1 &
        PID=$!
        sleep "$DUR"
        kill $PID 2>/dev/null || true
        sleep 1
        wc -l "$OUT/$name.blocks"
    done
    ;;
compare)
    # Two baseline captures of the same build calibrate away the blocks that bake
    # runtime heap pointers (HLE substitutions hash differently every run).
    B1="$2"; B2="$3"; CAND="$4"
    [ -n "$CAND" ] || { echo "usage: armv7_gate.sh compare <base1> <base2> <candidate>" >&2; exit 2; }
    RC=0
    for name in hello_world hg pd; do
        python3 "$ADVANCEDSLOP_TOOLS_DIR/armv7_gate_compare.py" "$name" "$B1/$name.blocks" "$B2/$name.blocks" "$CAND/$name.blocks" || RC=1
    done
    exit $RC
    ;;
*)
    echo "usage: armv7_gate.sh capture|compare ..." >&2
    exit 2
    ;;
esac
