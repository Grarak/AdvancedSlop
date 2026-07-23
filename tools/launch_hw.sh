#!/bin/bash
# Boot ADVANCEDSLOP_TEST_ROM locally under qemu-arm, detached, log to /tmp/advancedslop.log.
. "$(dirname "$0")/env.sh"
require_env ADVANCEDSLOP_SYSROOT ADVANCEDSLOP_TEST_ROM
export DISPLAY="$ADVANCEDSLOP_DISPLAY"
[ -n "$ADVANCEDSLOP_XAUTHORITY" ] && export XAUTHORITY="$ADVANCEDSLOP_XAUTHORITY"
export LIBGL_ALWAYS_SOFTWARE=1
cd "$ADVANCEDSLOP_ROOT"
pkill -9 -f "release-debug/advancedslop" 2>/dev/null
sleep 1
rm -f /tmp/advancedslop.log
setsid qemu-arm -L "$ADVANCEDSLOP_SYSROOT" target/thumbv7neon-unknown-linux-gnueabihf/release-debug/advancedslop "$ADVANCEDSLOP_TEST_ROM" >/tmp/advancedslop.log 2>&1 &
disown
