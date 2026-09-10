#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 /path/to/inanna-backend" >&2
    exit 2
fi

backend_path=$1
if [[ ! -x "$backend_path" ]]; then
    echo "backend is missing or not executable: $backend_path" >&2
    exit 1
fi

# A launch test alone can pass on a build host with Homebrew libraries installed.
# The standalone macOS backend must depend only on libraries shipped with macOS.
if [[ $(uname -s) == Darwin ]]; then
    linked_libraries=$(otool -L "$backend_path")
    while IFS= read -r library; do
        case "$library" in
            /usr/lib/*|/System/Library/*) ;;
            *)
                echo "backend depends on a non-system library: $library" >&2
                exit 1
                ;;
        esac
    done < <(printf '%s\n' "$linked_libraries" | sed -n '2,$s/^[[:space:]]*\(.*\) (compatibility version.*$/\1/p')
fi

data_root=$(mktemp -d "${TMPDIR:-/tmp}/inanna-smoke.XXXXXX")
request_pipe="$data_root/requests"
response_pipe="$data_root/responses"
mkfifo "$request_pipe" "$response_pipe"
backend_pid=

cleanup() {
    if [[ -n "$backend_pid" ]] && kill -0 "$backend_pid" 2>/dev/null; then
        kill "$backend_pid" 2>/dev/null || true
        wait "$backend_pid" 2>/dev/null || true
    fi
    rm -rf "$data_root"
}
trap cleanup EXIT

"$backend_path" --data-root "$data_root/data" <"$request_pipe" >"$response_pipe" &
backend_pid=$!
exec 3<>"$request_pipe"
exec 4<>"$response_pipe"

printf '%s\n' '{"protocolVersion":1,"kind":"request","requestId":"smoke-init","method":"initialize","params":{}}' >&3
if ! IFS= read -r -t 5 response <&4; then
    echo "timed out waiting for the backend handshake" >&2
    exit 1
fi
if [[ "$response" != *'"requestId":"smoke-init"'* || "$response" != *'"ok":true'* ]]; then
    echo "backend handshake failed: $response" >&2
    exit 1
fi

printf '%s\n' '{"protocolVersion":1,"kind":"request","requestId":"smoke-stop","method":"shutdown","params":{}}' >&3
if ! IFS= read -r -t 5 response <&4; then
    echo "timed out waiting for the backend shutdown response" >&2
    exit 1
fi
if [[ "$response" != *'"requestId":"smoke-stop"'* || "$response" != *'"ok":true'* ]]; then
    echo "backend shutdown failed: $response" >&2
    exit 1
fi

exec 3>&-
exec 4>&-
for _ in {1..50}; do
    if ! kill -0 "$backend_pid" 2>/dev/null; then
        wait "$backend_pid"
        backend_pid=
        exit 0
    fi
    sleep 0.1
done

echo "backend did not exit after shutdown" >&2
exit 1
