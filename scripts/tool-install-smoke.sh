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

data_root=$(mktemp -d "${TMPDIR:-/tmp}/inanna-tools.XXXXXX")
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

request() {
    printf '%s\n' "$1" >&3
    if ! IFS= read -r -t "$2" response <&4; then
        echo "timed out waiting for a backend response" >&2
        exit 1
    fi
    if [[ "$response" != *'"ok":true'* ]]; then
        echo "backend request failed: $response" >&2
        exit 1
    fi
}

request '{"protocolVersion":1,"kind":"request","requestId":"smoke-init","method":"initialize","params":{}}' 5
if [[ "$response" != *'"platform":"macos-arm64"'* ]]; then
    echo "backend did not report macOS arm64: $response" >&2
    exit 1
fi

request '{"protocolVersion":1,"kind":"request","requestId":"smoke-check","method":"checkTools","params":{}}' 60
if [[ "$response" != *'"state":"setupRequired"'* ]]; then
    echo "fresh tool check did not require setup: $response" >&2
    exit 1
fi

request '{"protocolVersion":1,"kind":"request","requestId":"smoke-install","method":"installTools","params":{}}' 5
operation_id=$(printf '%s\n' "$response" | sed -n 's/.*"operationId":"\([^"]*\)".*/\1/p')
if [[ -z "$operation_id" ]]; then
    echo "tool install did not return an operation ID: $response" >&2
    exit 1
fi

while IFS= read -r -t 600 event <&4; do
    if [[ "$event" == *'"event":"operationCompleted"'* && "$event" == *"\"operationId\":\"$operation_id\""* ]]; then
        break
    fi
    if [[ "$event" == *'"event":"operationFailed"'* && "$event" == *"\"operationId\":\"$operation_id\""* ]]; then
        echo "tool installation failed: $event" >&2
        exit 1
    fi
done
if [[ ${event:-} != *'"event":"operationCompleted"'* ]]; then
    echo "timed out waiting for tool installation" >&2
    exit 1
fi

tool_directory=$(find "$data_root/data/tools" -mindepth 1 -maxdepth 1 -type d -print -quit)
for executable in yt-dlp ffmpeg ffprobe deno; do
    if [[ ! -x "$tool_directory/$executable" ]]; then
        echo "installed tool is missing or not executable: $executable" >&2
        exit 1
    fi
done

test "$(lipo -archs "$tool_directory/ffmpeg")" = "arm64"
test "$(lipo -archs "$tool_directory/ffprobe")" = "arm64"
"$tool_directory/yt-dlp" --version
"$tool_directory/ffmpeg" -hide_banner -encoders 2>/dev/null | grep -q 'h264_videotoolbox'
"$tool_directory/ffmpeg" \
    -hide_banner -loglevel error \
    -f lavfi -i 'color=c=black:s=1280x720:r=1' \
    -frames:v 1 -an \
    -c:v h264_videotoolbox -profile:v high -pix_fmt yuv420p -allow_sw 0 \
    -b:v 5000k -maxrate 5000k -bufsize 10000k \
    -f null -
"$tool_directory/ffprobe" -version | head -1
"$tool_directory/deno" --version | head -1

request '{"protocolVersion":1,"kind":"request","requestId":"smoke-stop","method":"shutdown","params":{}}' 5
exec 3>&-
exec 4>&-
wait "$backend_pid"
backend_pid=

echo "macOS arm64 managed-tool installation passed"
