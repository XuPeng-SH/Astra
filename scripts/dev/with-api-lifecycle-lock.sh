#!/usr/bin/env bash
# Serialize one complete API lifecycle transition across terminals and worktrees.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
# shellcheck source=../lib/api_lifecycle_lock.sh
. "$REPO_ROOT/scripts/lib/api_lifecycle_lock.sh"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 command [args ...]" >&2
    exit 2
fi

ENV_FILE="${ASTRA_ENV_FILE:-$REPO_ROOT/.env}"
if [ -f "$ENV_FILE" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    set +a
fi

API_PORT="$(api_lifecycle_effective_port)"
api_lifecycle_prepare_root
LOCK_DIR="$(api_lifecycle_lock_dir "$API_PORT")"
LOCK_TIMEOUT="${ASTRA_API_LIFECYCLE_LOCK_TIMEOUT_SECONDS:-900}"
case "$LOCK_TIMEOUT" in ''|*[!0-9]*) echo "❌ ASTRA_API_LIFECYCLE_LOCK_TIMEOUT_SECONDS must be a positive integer" >&2; exit 2 ;; esac
[ "$LOCK_TIMEOUT" -gt 0 ] || { echo "❌ ASTRA_API_LIFECYCLE_LOCK_TIMEOUT_SECONDS must be a positive integer" >&2; exit 2; }

TOKEN="$(api_lifecycle_new_token)"
START_SECONDS=$SECONDS
WAIT_NOTICE=0
LOCK_OWNED=0

release_lock() {
    if [ "$LOCK_OWNED" -eq 1 ] && [ -d "$LOCK_DIR" ] && [ ! -L "$LOCK_DIR" ]; then
        rm -f "$LOCK_DIR/owner_pid" "$LOCK_DIR/token"
        rmdir "$LOCK_DIR" 2>/dev/null || true
    fi
}
trap release_lock EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    if [ -L "$LOCK_DIR" ] || { [ -e "$LOCK_DIR" ] && [ ! -d "$LOCK_DIR" ]; }; then
        echo "❌ Unsafe API lifecycle lock path: $LOCK_DIR" >&2
        exit 1
    fi
    OWNER_PID="$(cat "$LOCK_DIR/owner_pid" 2>/dev/null || true)"
    case "$OWNER_PID" in
        ''|*[!0-9]*) OWNER_ALIVE=2 ;;
        *) if kill -0 "$OWNER_PID" 2>/dev/null; then OWNER_ALIVE=1; else OWNER_ALIVE=0; fi ;;
    esac
    if [ "$OWNER_ALIVE" -eq 0 ]; then
        echo "❌ Stale API lifecycle lock on port $API_PORT (owner PID: ${OWNER_PID:-unknown})" >&2
        echo "   Verify no lifecycle command is running, then remove: $LOCK_DIR" >&2
        exit 1
    fi
    if [ "$WAIT_NOTICE" -eq 0 ]; then
        echo "⏳ Another API lifecycle operation owns port $API_PORT; waiting (owner PID: ${OWNER_PID:-publishing})..."
        WAIT_NOTICE=1
    fi
    if [ $((SECONDS - START_SECONDS)) -ge "$LOCK_TIMEOUT" ]; then
        echo "❌ Timed out waiting ${LOCK_TIMEOUT}s for API lifecycle lock on port $API_PORT" >&2
        exit 1
    fi
    sleep 1
done

LOCK_OWNED=1
chmod 700 "$LOCK_DIR"
printf '%s\n' "$$" > "$LOCK_DIR/owner_pid"
printf '%s\n' "$TOKEN" > "$LOCK_DIR/token"

export ASTRA_API_LIFECYCLE_LOCK_DIR="$LOCK_DIR"
export ASTRA_API_LIFECYCLE_LOCK_TOKEN="$TOKEN"
"$@"
