#!/usr/bin/env bash
# Guard internal Make targets that must run under the lifecycle wrapper.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
# shellcheck source=../lib/api_lifecycle_lock.sh
. "$REPO_ROOT/scripts/lib/api_lifecycle_lock.sh"

ENV_FILE="${ASTRA_ENV_FILE:-$REPO_ROOT/.env}"
if [ -f "$ENV_FILE" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    set +a
fi

API_PORT="$(api_lifecycle_effective_port)"
if ! api_lifecycle_lock_is_held "$API_PORT"; then
    echo "❌ This internal target requires the API lifecycle lock for port $API_PORT" >&2
    exit 1
fi
