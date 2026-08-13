#!/usr/bin/env bash
set -euo pipefail

: "${R2_BUCKET:?set R2_BUCKET to the existing recording bucket name}"
: "${QUEUE_NAME:=media-thumbnail-jobs}"
: "${SOURCE_PREFIX:=recordings/}"

npx wrangler r2 bucket notification create "$R2_BUCKET" \
  --event-type object-create \
  --queue "$QUEUE_NAME" \
  --prefix "$SOURCE_PREFIX" \
  --suffix ".mp4"
