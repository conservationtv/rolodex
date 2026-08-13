#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$repo_dir/fixtures/h264" "$repo_dir/fixtures/h265"

common_input=(
  -hide_banner -loglevel error -y
  -f lavfi -i testsrc2=size=320x180:rate=1
  -t 1 -an
)
fragment_flags=(
  -movflags +empty_moov+default_base_moof+frag_keyframe
  -f mp4
)

ffmpeg "${common_input[@]}" -pix_fmt yuv420p -c:v libx264 -profile:v baseline -g 1 \
  "${fragment_flags[@]}" "$repo_dir/fixtures/h264/synthetic-baseline-320x180.mp4"

ffmpeg "${common_input[@]}" -pix_fmt yuv420p -c:v libx264 -profile:v main -g 1 \
  "${fragment_flags[@]}" "$repo_dir/fixtures/h264/synthetic-main-320x180.mp4"

ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=size=320x180:rate=5 \
  -t 1 -an -pix_fmt yuv420p -c:v libx264 -profile:v high -g 5 -keyint_min 5 \
  -sc_threshold 0 "${fragment_flags[@]}" \
  "$repo_dir/fixtures/h264/synthetic-320x180.mp4"

ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=size=320x180:rate=5 \
  -t 1 -an -pix_fmt yuv420p -c:v libx265 \
  -x265-params keyint=5:min-keyint=5:scenecut=0:log-level=error:pools=1 \
  "${fragment_flags[@]}" "$repo_dir/fixtures/h265/synthetic-320x180.mp4"

ffmpeg "${common_input[@]}" -pix_fmt yuv420p10le -c:v libx265 -profile:v main10 \
  -x265-params keyint=1:min-keyint=1:scenecut=0:log-level=error:pools=1 \
  "${fragment_flags[@]}" "$repo_dir/fixtures/h265/synthetic-main10-320x180.mp4"
