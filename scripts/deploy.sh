#!/usr/bin/env bash
set -euo pipefail

case "${CODEC:-}" in
  h264|h265) ;;
  *)
    echo "usage: CODEC=h264|h265 ./scripts/deploy.sh" >&2
    exit 2
    ;;
esac

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export CODEC
worker_name="${WORKER_NAME:-rolodex-$CODEC}"
cd "$repo_dir"
args=(deploy)
if [[ -n "${WRANGLER_ENV:-}" ]]; then
  case "$WRANGLER_ENV:$CODEC" in
    staging_h264:h264|staging_h265:h265) ;;
    staging_h264:*|staging_h265:*)
      echo "WRANGLER_ENV=$WRANGLER_ENV does not match CODEC=$CODEC" >&2
      exit 2
      ;;
  esac
  args+=(--env "$WRANGLER_ENV")
else
  args+=(--name "$worker_name")
fi
npx wrangler "${args[@]}"
