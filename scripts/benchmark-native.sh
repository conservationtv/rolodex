#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
cargo bench -p thumbnail-core --no-default-features --features codec-h264 --bench pipeline
cargo bench -p thumbnail-core --no-default-features --features codec-h265 --bench pipeline
