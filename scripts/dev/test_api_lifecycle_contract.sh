#!/usr/bin/env bash
# Exercise API lifecycle serialization and fail-closed ownership behavior.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
test_root="$(mktemp -d /tmp/astra-api-lifecycle-test.XXXXXX)"
test_port=$((30000 + ($$ % 20000)))
lock_root="/tmp/astra-api-lifecycle-$(id -u)"
lock_dir="$lock_root/port-$test_port.lock"
pids=()

cleanup() {
    local pid
    for pid in "${pids[@]:-}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$test_root"
    if [ -d "$lock_dir" ]; then
        rm -f "$lock_dir/owner_pid" "$lock_dir/token"
        rmdir "$lock_dir" 2>/dev/null || true
    fi
}
trap cleanup EXIT

empty_env="$test_root/empty.env"
: > "$empty_env"
wrapper="$repo_root/scripts/dev/with-api-lifecycle-lock.sh"

# A second lifecycle operation for the same port must wait until the complete
# first operation exits, rather than racing its start/stop substeps.
ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    "$wrapper" bash -c 'touch "$1/first-entered"; while [ ! -f "$1/release" ]; do sleep 0.05; done' \
    bash "$test_root" &
first_pid=$!
pids+=("$first_pid")
for _ in {1..100}; do
    [ -f "$test_root/first-entered" ] && break
    sleep 0.05
done
[ -f "$test_root/first-entered" ] || { echo "lifecycle contract failed: first holder did not enter" >&2; exit 1; }

ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    "$wrapper" bash -c 'touch "$1/second-entered"' bash "$test_root" &
second_pid=$!
pids+=("$second_pid")
sleep 0.2
if [ -f "$test_root/second-entered" ]; then
    echo "lifecycle contract failed: concurrent operation bypassed the port lock" >&2
    exit 1
fi
touch "$test_root/release"
wait "$first_pid"
wait "$second_pid"
pids=()
[ -f "$test_root/second-entered" ] || { echo "lifecycle contract failed: waiter never entered" >&2; exit 1; }

# A caller cannot skip serialization with a copied or invented environment flag.
if ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    ASTRA_API_LIFECYCLE_LOCK_DIR="$lock_dir" ASTRA_API_LIFECYCLE_LOCK_TOKEN=forged \
    "$repo_root/scripts/dev/require-api-lifecycle-lock.sh" 2>/dev/null; then
    echo "lifecycle contract failed: forged held-lock environment was accepted" >&2
    exit 1
fi

# A normal signal must release the lock, allowing the next operation through.
set +e
ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    "$wrapper" bash -c 'kill -TERM "$PPID"; sleep 1' >/dev/null 2>&1
signal_status=$?
set -e
[ "$signal_status" -ne 0 ] || { echo "lifecycle contract failed: terminated wrapper returned success" >&2; exit 1; }
ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" "$wrapper" true

# A just-created lock publishes metadata after mkdir. Waiters must conservatively
# wait through that window instead of misclassifying a live acquisition as stale.
mkdir "$lock_dir"
ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    ASTRA_API_LIFECYCLE_LOCK_TIMEOUT_SECONDS=3 \
    "$wrapper" bash -c 'touch "$1/published-window-passed"' bash "$test_root" &
publishing_waiter=$!
pids+=("$publishing_waiter")
sleep 0.2
if [ -f "$test_root/published-window-passed" ] || ! kill -0 "$publishing_waiter" 2>/dev/null; then
    echo "lifecycle contract failed: unpublished owner was not conservatively waited on" >&2
    exit 1
fi
rmdir "$lock_dir"
wait "$publishing_waiter"
pids=()
[ -f "$test_root/published-window-passed" ] || { echo "lifecycle contract failed: metadata-window waiter never entered" >&2; exit 1; }

# A lock whose recorded owner is no longer alive is rejected. Automatically
# moving it would let two waiters race and move a newly acquired lock.
mkdir -p "$lock_root"
chmod 700 "$lock_root"
mkdir "$lock_dir"
printf '99999999\n' > "$lock_dir/owner_pid"
printf 'stale\n' > "$lock_dir/token"
if ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" "$wrapper" true 2>/dev/null; then
    echo "lifecycle contract failed: stale lock was unsafely recovered" >&2
    exit 1
fi
rm -f "$lock_dir/owner_pid" "$lock_dir/token"
rmdir "$lock_dir"

# Equivalent numeric spellings cannot create separate locks for one port.
if ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="0$test_port" "$wrapper" true 2>/dev/null; then
    echo "lifecycle contract failed: noncanonical port bypassed lock identity" >&2
    exit 1
fi

# An unrelated listener must make stop fail and must remain alive. This keeps
# destructive callers such as dev-seed from proceeding after an unsafe stop.
fixture_root="$test_root/checkout"
mkdir -p "$fixture_root/scripts/dev" "$fixture_root/scripts/lib"
cp "$repo_root/scripts/dev/stop-api.sh" \
    "$repo_root/scripts/dev/with-api-lifecycle-lock.sh" \
    "$fixture_root/scripts/dev/"
cp "$repo_root/scripts/lib/api_process_identity.sh" \
    "$repo_root/scripts/lib/api_lifecycle_lock.sh" \
    "$fixture_root/scripts/lib/"

sleep 30 &
foreign_pid=$!
pids+=("$foreign_pid")
fake_bin="$test_root/bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/lsof" <<EOF
#!/usr/bin/env bash
if [ "\${FAKE_LSOF_SILENT_FAILURE:-}" = 1 ]; then
    exit 2
fi
case "\$*" in
    *-tiTCP:*) printf '%s\\n' '$foreign_pid' ;;
    *) printf 'n/bin/sleep\\n' ;;
esac
EOF
chmod +x "$fake_bin/lsof"
set +e
(cd "$fixture_root" && PATH="$fake_bin:$PATH" ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    ./scripts/dev/stop-api.sh >/dev/null 2>&1)
stop_status=$?
set -e
if [ "$stop_status" -eq 0 ] || ! kill -0 "$foreign_pid" 2>/dev/null; then
    echo "lifecycle contract failed: stop accepted or killed a foreign listener" >&2
    exit 1
fi

set +e
silent_failure_output="$(cd "$fixture_root" && PATH="$fake_bin:$PATH" \
    FAKE_LSOF_SILENT_FAILURE=1 ASTRA_ENV_FILE="$empty_env" ASTRA_API_PORT="$test_port" \
    ./scripts/dev/stop-api.sh 2>&1)"
silent_failure_status=$?
set -e
if [ "$silent_failure_status" -eq 0 ] || [[ "$silent_failure_output" != *"Could not verify listeners"* ]]; then
    echo "lifecycle contract failed: silent lsof failure was treated as no listener" >&2
    exit 1
fi

# Internal compound targets must not be directly callable without a verified lock.
if make --no-print-directory -s -C "$repo_root" dev-api-restart-debug-locked >/dev/null 2>&1; then
    echo "lifecycle contract failed: internal restart bypassed the lifecycle lock" >&2
    exit 1
fi

echo "api lifecycle contract passed"
