# Process identity helpers for the local API lifecycle scripts.

# Return success only when PID is an Astra API server executable. On macOS,
# `ps -o comm` can truncate `target/debug/astra-server`; prefer the executable
# descriptor and keep the command-line fallback restricted to its first token.
api_process_is_astra_server() {
    local pid="$1"
    local comm executable command_line executable_token
    [ -n "$pid" ] || return 1

    comm=$(cat "/proc/$pid/comm" 2>/dev/null || true)
    if [[ "$comm" == "astra-server" ]]; then
        return 0
    fi

    if command -v lsof >/dev/null 2>&1; then
        executable=$(lsof -a -p "$pid" -d txt -Fn 2>/dev/null |
            sed -n 's/^n//p' | head -n 1)
        if [[ -n "$executable" && "$(basename "$executable")" == "astra-server" ]]; then
            return 0
        fi
    fi

    command_line=$(ps -p "$pid" -o command= 2>/dev/null || true)
    executable_token=$(printf '%s\n' "$command_line" | awk '{print $1}')
    [[ -n "$executable_token" && "$(basename "$executable_token")" == "astra-server" ]]
}
