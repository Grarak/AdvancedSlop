#!/bin/bash
# Usage: pi_key.sh <xkb key name> [hold_ms] — press a key on the remote test box via wayland virtual keyboard.
. "$(dirname "$0")/env.sh"
require_env ADVANCEDSLOP_PI_HOST
HOLD=${2:-150}
ssh -o BatchMode=yes "$ADVANCEDSLOP_PI_HOST" "XDG_RUNTIME_DIR=$ADVANCEDSLOP_PI_RUNTIME_DIR WAYLAND_DISPLAY=$ADVANCEDSLOP_PI_WAYLAND_DISPLAY $ADVANCEDSLOP_PI_WTYPE -P $1 -s $HOLD -p $1" 2>&1 | grep -v setlocale
