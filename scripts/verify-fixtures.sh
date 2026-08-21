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
      index "$fixture" "$output_dir" >"$work_dir/$codec-$base.json"
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

  local clip_fixture clip_dir source_stream clip_stream source_duration clip_duration clip_thumb clip_dimensions clip_log clip_score
  clip_fixture="$repo_dir/fixtures/$codec/mediamtx-v1.18.2-640x360.mp4"
  clip_dir="$work_dir/$codec-clip"
  cargo run --quiet --release -p rolodex-clip --example process_fixture \
    --no-default-features --features "$feature,test-util" --manifest-path "$repo_dir/Cargo.toml" \
    -- "$clip_fixture" "$clip_dir" --repeat
  test -s "$clip_dir/clip.mp4"
  test -s "$clip_dir/index.m3u8"
  test -s "$clip_dir/descriptor.json"
  test -s "$clip_dir/thumbnail.jpg"
  source_stream="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name,width,height -of csv=s=x:p=0 "$clip_fixture")"
  clip_stream="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name,width,height -of csv=s=x:p=0 "$clip_dir/clip.mp4")"
  if [[ "$clip_stream" != "$source_stream" ]]; then
    echo "$clip_fixture: spliced stream $clip_stream does not match source $source_stream" >&2
    exit 1
  fi
  source_duration="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$clip_fixture")"
  clip_duration="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$clip_dir/clip.mp4")"
  if ! awk -v source="$source_duration" -v clip="$clip_duration" \
    'BEGIN { expected = source * 2; delta = clip - expected; if (delta < 0) delta = -delta; exit !(delta <= 0.05) }'; then
    echo "$clip_fixture: standalone clip duration $clip_duration does not match repeated source $source_duration" >&2
    exit 1
  fi
  ffmpeg -v error -i "$clip_dir/clip.mp4" -map 0:v:0 \
    -f framehash "$work_dir/$codec-clip-frames.sha256"
  clip_thumb="$clip_dir/thumbnail.jpg"
  clip_dimensions="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=width,height -of csv=s=x:p=0 "$clip_thumb")"
  clip_log="$work_dir/$codec-clip-ssim.log"
  ffmpeg -hide_banner -i "$clip_fixture" -i "$clip_thumb" \
    -filter_complex "[0:v:0]select='eq(n,0)',scale=$clip_dimensions,setpts=PTS-STARTPTS[reference];[1:v:0]setpts=PTS-STARTPTS[actual];[reference][actual]ssim" \
    -frames:v 1 -f null - >"$clip_log" 2>&1
  clip_score="$(sed -n 's/.* All:\([0-9.]*\).*/\1/p' "$clip_log" | tail -n 1)"
  if [[ -z "$clip_score" ]] || ! awk -v score="$clip_score" -v minimum="$minimum_ssim" 'BEGIN { exit !(score >= minimum) }'; then
    echo "$clip_fixture: clip SSIM ${clip_score:-missing} is below $minimum_ssim" >&2
    sed -n '/SSIM/p' "$clip_log" >&2
    exit 1
  fi
  echo "$clip_fixture: standalone repeated clip duration is $clip_duration and SSIM is $clip_score"
}

verify_codec h264
verify_codec h265
