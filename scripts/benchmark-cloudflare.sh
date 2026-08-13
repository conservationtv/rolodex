#!/usr/bin/env bash
set -euo pipefail

: "${R2_BUCKET:?set R2_BUCKET to the staging recording bucket}"
: "${FIXTURE_DIR:?set FIXTURE_DIR to a directory of approved benchmark MP4 files}"
: "${SOURCE_PREFIX:=recordings/benchmark/}"

find "$FIXTURE_DIR" -type f -name '*.mp4' -print0 | while IFS= read -r -d '' fixture; do
  key="$SOURCE_PREFIX$(basename "$fixture")"
  echo "uploading $fixture to r2://$R2_BUCKET/$key"
  npx wrangler r2 object put "$R2_BUCKET/$key" \
    --file "$fixture" \
    --content-type "video/mp4" \
    --cache-control "public, max-age=31536000, immutable" \
    --remote
done

echo "Uploads complete. Query Workers Logs for structured metrics and invocation CPU/wall time."
