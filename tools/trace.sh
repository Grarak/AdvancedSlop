#!/bin/bash
# Usage: trace.sh <inst-log-path>
# Boot ADVANCEDSLOP_TEST_ROM locally under qemu-arm with --inst-log, detached.
. "$(dirname "$0")/env.sh"
require_env ADVANCEDSLOP_SYSROOT ADVANCEDSLOP_TEST_ROM
export DISPLAY="$ADVANCEDSLOP_DISPLAY"
[ -n "$ADVANCEDSLOP_XAUTHORITY" ] && export XAUTHORITY="$ADVANCEDSLOP_XAUTHORITY"
export LIBGL_ALWAYS_SOFTWARE=1
cd "$ADVANCEDSLOP_ROOT"
pkill -9 -f "debug/advancedslop" 2>/dev/null
sleep 1
rm -f /tmp/advancedslop_trace.log
# The debug profile IS the tracing build (DEBUG_LOG keys off the profile name; opt-level 3).
setsid qemu-arm -L "$ADVANCEDSLOP_SYSROOT" target/thumbv7neon-unknown-linux-gnueabihf/debug/advancedslop --inst-log "$1" "$ADVANCEDSLOP_TEST_ROM" >/tmp/advancedslop_trace.log 2>&1 &
disown
