# Shared host-local serialization for API lifecycle operations.

api_lifecycle_effective_port() {
    local port="${ASTRA_API_PORT:-17001}"
    case "$port" in
        ''|*[!0-9]*)
            echo "❌ ASTRA_API_PORT must be an integer between 1 and 65535" >&2
            return 1
            ;;
    esac
    case "$port" in
        0*)
            echo "❌ ASTRA_API_PORT must use canonical decimal form without leading zeroes" >&2
            return 1
            ;;
    esac
    if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        echo "❌ ASTRA_API_PORT must be an integer between 1 and 65535" >&2
        return 1
    fi
    printf '%s\n' "$port"
}

api_lifecycle_lock_root() {
    printf '/tmp/astra-api-lifecycle-%s\n' "$(id -u)"
}

api_lifecycle_lock_dir() {
    local port="$1"
    printf '%s/port-%s.lock\n' "$(api_lifecycle_lock_root)" "$port"
}

api_lifecycle_prepare_root() {
    local root
    root="$(api_lifecycle_lock_root)"
    if [ -L "$root" ]; then
        echo "❌ Refusing symbolic-link API lifecycle lock root: $root" >&2
        return 1
    fi
    if [ ! -d "$root" ]; then
        (umask 077 && mkdir "$root") 2>/dev/null || true
    fi
    if [ ! -d "$root" ] || [ -L "$root" ] || [ ! -O "$root" ]; then
        echo "❌ API lifecycle lock root is not a private directory owned by this user: $root" >&2
        return 1
    fi
    chmod 700 "$root" 2>/dev/null || {
        echo "❌ Cannot secure API lifecycle lock root: $root" >&2
        return 1
    }
}

api_lifecycle_new_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 16
    elif command -v uuidgen >/dev/null 2>&1; then
        uuidgen | tr -d '-'
    else
        printf '%s-%s-%s-%s\n' "$$" "$PPID" "$RANDOM" "$(date +%s)"
    fi
}

api_lifecycle_lock_is_held() {
    local port="$1"
    local lock_dir expected_token owner_pid
    lock_dir="$(api_lifecycle_lock_dir "$port")"
    expected_token="${ASTRA_API_LIFECYCLE_LOCK_TOKEN:-}"
    [ -n "$expected_token" ] || return 1
    [ "${ASTRA_API_LIFECYCLE_LOCK_DIR:-}" = "$lock_dir" ] || return 1
    [ -d "$lock_dir" ] && [ ! -L "$lock_dir" ] || return 1
    [ -O "$lock_dir" ] || return 1
    [ "$(cat "$lock_dir/token" 2>/dev/null || true)" = "$expected_token" ] || return 1
    owner_pid="$(cat "$lock_dir/owner_pid" 2>/dev/null || true)"
    case "$owner_pid" in ''|*[!0-9]*) return 1 ;; esac
    kill -0 "$owner_pid" 2>/dev/null
}
