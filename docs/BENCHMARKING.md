# Benchmarking and performance gates

## Native regression benchmark

```bash
./scripts/benchmark-native.sh
```

Criterion measures `moov`, `moof`, NAL conversion, first-frame decode, planar
scale, JPEG, and the complete in-memory pipeline for both codec variants. This
is useful for regression detection but is not a Cloudflare CPU cost
measurement.

## WASM packaging and size

```bash
npm run build:worker
ls -lh crates/worker/build/worker/index.wasm

npm run build:worker:h265
ls -lh crates/worker/build/worker/index.wasm
```

The build output is optimized by `wasm-opt`. CI packages both variants on every
change. To compare `opt-level=3`, `s`, and `z`, use the workspace release
profiles and retain CPU/decode measurements alongside module size; do not choose
the smallest module without measuring decode cost.

## Cloudflare staging matrix

Upload an approved corpus:

```bash
R2_BUCKET=rolodex-recordings-staging \
FIXTURE_DIR=/absolute/path/to/approved-corpus \
SOURCE_PREFIX=recordings/benchmark/ \
./scripts/benchmark-cloudflare.sh
```

For every source, join the structured Worker log with Cloudflare invocation CPU
and wall time:

| Codec | Profile | Resolution | Source bytes | R2 bytes | Requests | Samples decoded | CPU ms | Wall ms | Peak memory | WASM bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| H.264 | High | 1080p | | | | | | | | |
| H.264 | High | 4K | | | | | | | | |
| H.265 | Main | 1080p | | | | | | | | |
| H.265 | Main/Main10 | 4K | | | | | | | | |

Use Cloudflare's recorded CPU time for economics. Worker timers are not a CPU
profiler and local native/WASM wall time is only comparative.

The controlled `0def` staging run, resource IDs, SIMD A/B, 4K results, and
remaining evidence gaps are recorded in `docs/STAGING_RESULTS.md`.

## Initial gates

- P50 CPU below 250 ms and P95 below 750 ms.
- Typical R2 download below 2 MiB.
- One decoded sample normally and no more than three at P95.
- Target peak memory below 64 MiB; hard reconsideration near 128 MiB.
- Typical JPEG below 50 KiB.
- Prefer Worker WASM below 5 MiB.
- H.265 4K below roughly two seconds and 96 MiB may remain acceptable after
  cost modelling.

If a gate fails, optimize in this order: decoded sample count, bytes fetched,
decoder hot paths, measured WASM SIMD opportunities, planar scaling, JPEG, then
bundle size.
