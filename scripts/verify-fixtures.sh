#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/rolodex-verify.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
minimum_ssim="${MINIMUM_SSIM:-0.85}"

verify_codec() {
  local codec="$1"
  local feature="codec-$codec"
  local fixture
  cargo build --quiet --release -p rolodex --no-default-features --features "$feature" \
    --manifest-path "$repo_dir/Cargo.toml"

  while IFS= read -r -d '' fixture; do
    local base output_dir thumb dimensions log score
    base="$(basename "$fixture" .mp4)"
    output_dir="$work_dir/$codec-$base"
    "$repo_dir/target/release/rolodex" \
      "$fixture" "$output_dir" >"$work_dir/$codec-$base.json"
    thumb="$output_dir/thumbnail.jpg"
    test -s "$output_dir/index.json"
    find "$output_dir/init" -type f -name '*.mp4' -size +0 -print -quit | grep -q .
    dimensions="$(ffprobe -v error -select_streams v:0 \
      -show_entries stream=width,height -of csv=s=x:p=0 "$thumb")"
    log="$work_dir/$codec-$base-ssim.log"
    ffmpeg -hide_banner -i "$fixture" -i "$thumb" \
      -filter_complex "[0:v:0]select='eq(n,0)',scale=$dimensions,setpts=PTS-STARTPTS[reference];[1:v:0]setpts=PTS-STARTPTS[actual];[reference][actual]ssim" \
      -frames:v 1 -f null - >"$log" 2>&1
    score="$(sed -n 's/.* All:\([0-9.]*\).*/\1/p' "$log" | tail -n 1)"
    if [[ -z "$score" ]] || ! awk -v score="$score" -v minimum="$minimum_ssim" 'BEGIN { exit !(score >= minimum) }'; then
      echo "$fixture: SSIM ${score:-missing} is below $minimum_ssim" >&2
      sed -n '/SSIM/p' "$log" >&2
      exit 1
    fi
    echo "$fixture: SSIM $score"
  done < <(find "$repo_dir/fixtures/$codec" -type f -name '*.mp4' -print0 | sort -z)
}

verify_codec h264
verify_codec h265
