#!/usr/bin/env bash
set -euo pipefail
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CODEC=h264 "$repo_dir/scripts/build-worker.sh"
