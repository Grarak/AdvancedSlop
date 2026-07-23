# Shared environment loader for the tools/ scripts. Not executable — source it:
#   . "$(dirname "$0")/env.sh"
# Resolves the repo root, sources <repo>/.env if present (copy .env.example there
# and fill in your paths), and provides require_env for mandatory variables.

ADVANCEDSLOP_TOOLS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ADVANCEDSLOP_ROOT="$(dirname "$ADVANCEDSLOP_TOOLS_DIR")"

if [ -f "$ADVANCEDSLOP_ROOT/.env" ]; then
    set -a
    . "$ADVANCEDSLOP_ROOT/.env"
    set +a
fi

require_env() {
    for var in "$@"; do
        if [ -z "$(eval echo "\$$var")" ]; then
            echo "error: $var is not set — copy .env.example to .env in the repo root and fill it in" >&2
            exit 1
        fi
    done
}

# Defaults that work on most setups; override in .env if needed.
: "${ADVANCEDSLOP_DISPLAY:=:0}"
: "${ADVANCEDSLOP_PI_BIN:=~/claude/advancedslop/advancedslop}"
: "${ADVANCEDSLOP_PI_RUNTIME_DIR:=/run/user/1000}"
: "${ADVANCEDSLOP_PI_WAYLAND_DISPLAY:=wayland-0}"
: "${ADVANCEDSLOP_PI_WTYPE:=wtype}"
