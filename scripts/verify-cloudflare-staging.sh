#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
account_id="${CLOUDFLARE_ACCOUNT_ID:-0def86f295b15a1a2d012774b38b04f9}"
bucket="${R2_BUCKET:-mediamtx-thumbnail-staging}"
minimum_ssim="${MINIMUM_SSIM:-0.90}"
verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/media-thumb-staging.XXXXXX")"

cleanup() {
  rm -rf -- "$verify_dir"
}
trap cleanup EXIT

entries=(
  "artifacts/h264/mediamtx-v1.18.2-640x360|fixtures/h264/mediamtx-v1.18.2-640x360.mp4"
  "artifacts/h265/mediamtx-v1.18.2-640x360|fixtures/h265/mediamtx-v1.18.2-640x360.mp4"
)

cd "$repo_dir"
for entry in "${entries[@]}"; do
  IFS='|' read -r artifact_root fixture <<<"$entry"
  thumbnail_key="$artifact_root/thumbnail.jpg"
  artifact_stream="${artifact_root%/*}"
  recording_name="${artifact_root##*/}"
  index_key="$artifact_stream/indexes/$recording_name.json"
  local_name="${artifact_root//\//_}"
  thumbnail="$verify_dir/$local_name"
  index="$verify_dir/$local_name.json"
  reference="$verify_dir/$local_name.png"
  first_cell="$verify_dir/$local_name-cell.png"

  CLOUDFLARE_ACCOUNT_ID="$account_id" npx wrangler r2 object get \
    "$bucket/$thumbnail_key" --file "$thumbnail" --remote >/dev/null
  CLOUDFLARE_ACCOUNT_ID="$account_id" npx wrangler r2 object get \
    "$bucket/$index_key" --file "$index" --remote >/dev/null
  init_hash="$(node -e 'const fs=require("fs"); process.stdout.write(JSON.parse(fs.readFileSync(process.argv[1])).init.sha256)' "$index")"
  init="$verify_dir/$init_hash.mp4"
  CLOUDFLARE_ACCOUNT_ID="$account_id" npx wrangler r2 object get \
    "$bucket/init/$init_hash.mp4" --file "$init" --remote >/dev/null
  init_duration="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=duration -of csv=p=0 "$init")"
  if [[ "$init_duration" != "0.000000" ]]; then
    echo "init/$init_hash.mp4 has non-zero track duration $init_duration" >&2
    exit 1
  fi
  read -r cell_width cell_height frame_count expected_frames < <(node -e '
    const fs = require("fs");
    const value = JSON.parse(fs.readFileSync(process.argv[1]));
    const thumbnail = value.thumbnail;
    const expected = thumbnail.interval_ns === undefined
      ? 1
      : Math.max(1, Math.floor(Number(BigInt(value.recording.video_duration_ns) / BigInt(thumbnail.interval_ns))));
    process.stdout.write(`${thumbnail.frame_width} ${thumbnail.frame_height} ${thumbnail.frame_count} ${expected}\n`);
  ' "$index")
  if [[ "$frame_count" != "$expected_frames" ]]; then
    echo "$index_key contains $frame_count frames; expected $expected_frames" >&2
    exit 1
  fi
  ffmpeg -hide_banner -loglevel error -y -i "$thumbnail" \
    -vf "crop=$cell_width:$cell_height:0:0" -frames:v 1 "$first_cell"
  ffmpeg -hide_banner -loglevel error -y -i "$fixture" \
    -vf "select=eq(n\,0),scale=$cell_width:$cell_height" \
    -frames:v 1 "$reference"
  ssim_line="$(ffmpeg -hide_banner -i "$reference" -i "$first_cell" \
    -lavfi ssim -f null - 2>&1 | sed -n '/SSIM Y:/p' | tail -n 1)"
  ssim="$(sed -n 's/.* All:\([0-9.]*\).*/\1/p' <<<"$ssim_line")"
  if [[ -z "$ssim" ]]; then
    echo "could not parse SSIM for $thumbnail_key" >&2
    exit 1
  fi
  if ! awk -v value="$ssim" -v minimum="$minimum_ssim" \
    'BEGIN { exit !(value >= minimum) }'; then
    echo "$thumbnail_key SSIM $ssim is below $minimum_ssim" >&2
    exit 1
  fi
  printf '%s\tSSIM=%s\tinit=%s\n' "$artifact_root" "$ssim" "$init_hash"
done
