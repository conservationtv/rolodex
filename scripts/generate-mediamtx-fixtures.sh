#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mediamtx_bin="${MEDIAMTX_BIN:-$(command -v mediamtx || true)}"
if [[ -z "$mediamtx_bin" || ! -x "$mediamtx_bin" ]]; then
  echo "set MEDIAMTX_BIN to an executable MediaMTX v1.18.2 binary" >&2
  exit 2
fi
if [[ "$("$mediamtx_bin" --version)" != "v1.18.2" ]]; then
  echo "this reproducibility script requires MediaMTX v1.18.2" >&2
  exit 2
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/media-thumb-mediamtx.XXXXXX")"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill -INT "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

(
  cd "$work_dir"
  "$mediamtx_bin" "$repo_dir/fixtures/mediamtx-smoke.yml"
) &
server_pid="$!"
sleep 1

ffmpeg -hide_banner -loglevel error -re -f lavfi \
  -i testsrc2=size=640x360:rate=5 -t 2 -an -pix_fmt yuv420p \
  -c:v libx264 -profile:v high -g 5 -keyint_min 5 -sc_threshold 0 \
  -preset veryfast -rtsp_transport tcp -f rtsp \
  rtsp://127.0.0.1:18554/smoke-h264

ffmpeg -hide_banner -loglevel error -re -f lavfi \
  -i testsrc2=size=640x360:rate=5 -t 2 -an -pix_fmt yuv420p \
  -c:v libx265 -preset veryfast \
  -x265-params keyint=5:min-keyint=5:scenecut=0:log-level=error:pools=1 \
  -rtsp_transport tcp -f rtsp rtsp://127.0.0.1:18554/smoke-h265

kill -INT "$server_pid"
wait "$server_pid" || true
server_pid=""

h264_fixture="$(find "$work_dir/recordings/smoke-h264" -type f -name '*.mp4' | sort | head -n 1)"
h265_fixture="$(find "$work_dir/recordings/smoke-h265" -type f -name '*.mp4' | sort | head -n 1)"
if [[ -z "$h264_fixture" || -z "$h265_fixture" ]]; then
  echo "MediaMTX did not produce both codec fixtures" >&2
  exit 1
fi
cp "$h264_fixture" "$repo_dir/fixtures/h264/mediamtx-v1.18.2-640x360.mp4"
cp "$h265_fixture" "$repo_dir/fixtures/h265/mediamtx-v1.18.2-640x360.mp4"
