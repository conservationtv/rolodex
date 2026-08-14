# Rolodex

Rolodex is an archive playback and thumbnail system for completed MediaMTX
fragmented MP4 recordings. One Queue invocation walks the recording metadata,
extracts presentation-time thumbnail frames into one JPEG sprite sheet when
decoding succeeds, and creates
the archive control-plane artifacts independently. The
same source builds as an H.264 Worker or an H.265 Worker; the unselected decoder
is not linked.

```text
R2 recording -> Queue -> Rust Worker -> ETag-conditional ranged fMP4 walk
                                      +-> <stream>/<timestamp>/thumbnail.jpg (optional)
                                      +-> <stream>/indexes/<timestamp>.json
                                      +-> <stream>/archive/YYYY-MM-DD.json
                                      +-> <stream>/archive.json
                                      +-> init/<sha256>.mp4
```

The index and canonical HLS init allow a later playlist compiler to expose the
unchanged source MP4 media as `EXT-X-BYTERANGE` segments. Video bytes remain on
the direct R2-to-browser path. See
[docs/VIRTUAL_HLS_STRATEGY.md](docs/VIRTUAL_HLS_STRATEGY.md) for the artifact
contract and rollout strategy.

The detailed design and live status are in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md),
[PROGRESS.md](PROGRESS.md), and [docs/COMPLETION_AUDIT.md](docs/COMPLETION_AUDIT.md).
Actual Cloudflare measurements are in
[docs/STAGING_RESULTS.md](docs/STAGING_RESULTS.md); decoder/dependency licences
and the advisory audit are in [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md).

## What is implemented

- Checked, bounded parsing for the required `moov`/`moof` hierarchy,
  `avcC`/`hvcC`, MediaMTX `mtxi`, `trex`, `tfhd`, `tfdt`, and timing-aware
  `trun` data.
- Exact `[moof, end-of-mdat)` byte ranges, decode and presentation timelines,
  resolved sample flags, track descriptors, and wall-clock metadata in a
  versioned JSON sidecar.
- Canonical zero-duration, `iso6`-compatible init MP4 generation with inactive
  tracks and recording-specific `mtxi` removed, plus SHA-256 content addressing.
- Codec/sample-entry mismatch rejection before decoder work.
- Length-prefixed NAL validation, AVC IDR / HEVC IRAP confirmation, and Annex-B
  conversion.
- Pure-Rust AVC Baseline/Main/most-High and HEVC Main/Main10 decoder adapters.
- Direct planar 4:2:0 downscale and YCbCr JPEG encoding, without a
  full-resolution RGB allocation.
- Duration-configured thumbnail sampling (`10s`, `1m`, and similar values),
  random-access-aware decode, bounded sprite layout, and per-cell timing in the
  archive index.
- Best-effort thumbnail generation: decoder, scaling, or JPEG failures leave a
  schema-v4 index with `thumbnail.frame_count = 0` and `thumbnail.error`, while
  the canonical init and complete part metadata are still committed.
- Local ranged-file CLI and structured per-job metrics.
- `workers-rs` Queue consumer, R2 ranged source, index commit-marker
  idempotency, deterministic output keys, per-stream timestamp-sortable indexes,
  ETag-conditional daily playback manifests plus a listing-free stream directory,
  retry classification, and structured logs.
- Synthetic profile fixtures, FFmpeg SSIM verification, fuzz targets,
  Criterion benchmark, codec-specific Worker packaging, and CI for both builds.
- Deployed, Queue-only H.264/H.265 staging Workers in the pinned `0def` account,
  with real R2 Event Notifications, content-aware idempotency, and 4K evidence.

## Local CLI

```bash
cargo run --release -p rolodex \
  --no-default-features --features codec-h264 -- \
  input.mp4 output-directory

cargo run --release -p rolodex \
  --no-default-features --features codec-h265 -- \
  input.mp4 output-directory
```

Optional arguments are `--max-width`, `--max-height`, `--quality`, and
`--thumbnail-interval DURATION`. Cell bounds default to 480 x 270 and JPEG
quality 78. An interval produces
`max(1, floor(video_duration / interval))` cells at elapsed times
`0, interval, 2 × interval, …`; without the option the CLI emits one cell. The
directory always receives `index.json` and `init/<sha256>.mp4`, plus
`thumbnail.jpg` when thumbnail generation succeeds. MediaMTX `mtxi` metadata is
extracted when present but is not required.

## Test and verify

```bash
cargo test -p rolodex-core -p rolodex -p rolodex-worker \
  --no-default-features --features codec-h264
cargo test -p rolodex-core -p rolodex -p rolodex-worker \
  --no-default-features --features codec-h265

./scripts/verify-fixtures.sh
```

`verify-fixtures.sh` compares every checked-in fixture thumbnail against the
first FFmpeg-decoded frame using SSIM. `scripts/generate-fixtures.sh` recreates
the non-sensitive synthetic corpus.

The checked-in fuzz harnesses require `cargo-fuzz` and a nightly compiler. A
bounded smoke pass is:

```bash
for target in box_parser codec_config fragment nal_conversion; do
  cargo +nightly fuzz run "$target" -- -runs=10000 -max_len=4096
done
```

## Build Worker variants

Install requirements: the Rust WASM target and `worker-build` 0.8.5. The build
script installs the latter when absent.

```bash
./scripts/build-h264.sh
./scripts/build-h265.sh
```

For a custom/production configuration, codec selection remains one input:

```bash
CODEC=h264 ./scripts/deploy.sh
CODEC=h265 ./scripts/deploy.sh
```

The root R2/Queue names are templates and must be reconciled before using those
commands. Deployment and notification setup are described in
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

The checked-in `staging_h264` and `staging_h265` environments are already
pinned to the `0def…04f9` account and its isolated staging resources:

```bash
CODEC=h264 WRANGLER_ENV=staging_h264 ./scripts/deploy.sh
CODEC=h265 WRANGLER_ENV=staging_h265 ./scripts/deploy.sh
./scripts/verify-cloudflare-staging.sh
```

## Supported input assumptions

The recording must have one AVC or HEVC video track, a `moov` before its media
fragments, codec parameter sets in `avcC`/`hvcC`, and resolvable sample sizes and
data offsets from `trex`/`tfhd`/`trun`. A candidate must be marked sync and also
contain an AVC IDR or HEVC BLA/IDR/CRA NAL. Other tracks are ignored.

For archive indexing, every `traf` must contain `tfdt`, use
`default-base-is-moof`, and resolve all sample duration/size/flag fields. Each
MediaMTX part must be a byte-contiguous `moof` followed by `mdat`, and every
resolved sample must fall inside that `mdat`. The implementation intentionally
rejects ambiguous layouts instead of guessing. It does not support progressive
MP4 sample tables, encrypted tracks, multiple video tracks, or general
transcoding.

## Real-world completion boundary

Synthetic AVC Baseline/Main/High and HEVC Main/Main10 fixtures pass locally and
in Cloudflare staging, actual MediaMTX recorder smoke files pass, and both
optimized Worker packages build below 1 MiB. Production readiness still
requires representative recordings from the actual camera fleet, percentile
load evidence, and production rollout approval. Those items are tracked
explicitly in `PROGRESS.md`; they are not silently treated as complete.
