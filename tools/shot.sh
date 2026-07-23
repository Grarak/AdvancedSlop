#!/bin/bash
# Usage: shot.sh <out.png> — screenshot the local advancedslop window (xwd, no ImageMagick needed).
. "$(dirname "$0")/env.sh"
export DISPLAY="$ADVANCEDSLOP_DISPLAY"
[ -n "$ADVANCEDSLOP_XAUTHORITY" ] && export XAUTHORITY="$ADVANCEDSLOP_XAUTHORITY"
WID=$(xwininfo -root -tree 2>/dev/null | grep '"advancedslop" "advancedslop"' | grep -o '0x[0-9a-f]*' | head -1)
xwd -id "$WID" -out /tmp/s.xwd 2>/dev/null && python3 "$ADVANCEDSLOP_TOOLS_DIR/xwd2png.py" /tmp/s.xwd "$1" >/dev/null
