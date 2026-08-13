#!/usr/bin/env bash
set -euo pipefail

case "${CODEC:-}" in
  h264) codec_feature="codec-h264" ;;
  h265) codec_feature="codec-h265" ;;
  *)
    echo "CODEC must be h264 or h265" >&2
    exit 2
    ;;
esac

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if ! command -v worker-build >/dev/null 2>&1; then
  cargo install --locked worker-build --version 0.8.5
fi

cd "$repo_dir/crates/worker"
worker-build --release --no-panic-recovery --no-default-features --features "$codec_feature"
