# Implementation Progress

Last updated: 2026-08-10 (Europe/London)

## Status summary

Overall status: **in progress**

| Area | Status | Evidence / next action |
|---|---|---|
| Repository audit | Complete | Workspace was empty; Rust 1.94 and `wasm32-unknown-unknown` are installed. |
| Durable plan | Complete | See `IMPLEMENTATION_PLAN.md`. |
| Decoder selection | Controlled staging complete; fleet pending | Pure-Rust AVC/HEVC backends pass native/WASM, remote Baseline/Main/High/Main10, and 4K execution. Private camera compatibility remains. |
| Core parser/pipeline | Complete for defined scope | Bounded `moov`/`moof`, sample locations/flags, delayed decoder output, shared Annex-B walking, planar scale, and JPEG paths implemented. |
| Local CLI | Complete | Ranged local-file source, options, JPEG write, and JSON metrics implemented for both features. |
| Tests/fuzz/benchmarks | Controlled corpus complete; fleet pending | Tests, four fuzz targets, stage benchmarks, RustSec, local/remote SSIM, bounded reads, and 4K spikes pass. |
| Cloudflare Worker | Deployed to staging | Both Queue-only schema-v2 sprite variants process real R2 notifications with ETag-safe idempotency, retries, logs, and DLQs. |
| Build/deploy/CI | Complete | Codec scripts, pinned staging environments, SIMD packaging, helpers, and expanded two-variant CI added. |
| Cloudflare staging | Controlled milestone complete | Account `0def…04f9` has isolated resources; profile, mismatch, idempotency, MediaMTX, and 4K paths pass. Representative P50/P95 remains. |
| Production rollout | Pending external inputs/approval | Requires private camera-fleet evidence, larger percentile run, memory margin decision, and production resource approval. |
| Unified recording processor | Implemented locally | One ranged index pass plus targeted decode now returns a JPEG sprite, schema-v2 physical-part/thumbnail index, and canonical content-addressed init MP4. |
| Interval thumbnail sprites | Implemented locally | `THUMBNAIL_INTERVAL` accepts values such as `10s` and `1m`; frame count is `floor(video_duration / interval)`, cells are grid-packed, and exact target/PTS/sample locations are indexed. |
| Virtual-HLS metadata | Implemented locally | `mtxi`, tracks, exact `moof+mdat` ranges, decode/presentation timelines, sync flags, source identity, and init hash are serialized. |
| Canonical HLS init | Implemented locally | Deterministic `iso6` `ftyp`, zero `mvhd`/`tkhd` durations, stripped recording `udta/mtxi`, SHA-256 key; AVC/HEVC outputs pass FFprobe. |
| Three-artifact R2 commit | Deployed and verified in staging | Worker writes init, sprite thumbnail, then index commit marker and verifies all artifacts for ETag idempotency. Live AVC/HEVC jobs created and reused content-addressed init objects. |
| Pull-based clip discovery and processing | Implemented locally | M1/M2 select exact complete parts, normalize multi-file fMP4 timelines for standalone playback, and emit HLS, provenance, and contact-sheet artifacts. |
| Clip CLI local publication | Implemented and fixture-tested | M3 scans MediaMTX directories, revalidates file stamps, writes the artifact set manifest-last, and reports structured metrics. |
| Clip R2 publication | Implemented; staging playback pending | M4 SigV4 HEAD/PUT, immutable caching, deterministic keys, skip-on-existing idempotency, and manifest-last ordering pass mock HTTP tests. |
| Clip hardening | Implemented locally | The descriptor carries its media cap across phases, paths are rejected before filesystem access, indexing/rendering are split, native file sources are shared, and fragment timing rewrite fuzzing covers the mutation surface. |

## Decisions

### 2026-08-10 — interval thumbnail sprite sheets

The unified processor now derives a monotonic video duration while indexing,
selects presentation-time targets at the configured interval, and decodes each
target from a preceding verified random-access picture. HEVC open-GOP leading
pictures are excluded when resetting a decoder. All scaled frames are packed
into one bounded JPEG grid at the existing `thumbnail.jpg` key, while schema v2
records the grid geometry and every selected target/PTS/sample location. The
artifact identity includes the interval so configuration changes regenerate the
sprite.

### 2026-08-09 — unified virtual-HLS artifact processor

Thumbnail generation, whole-recording metadata indexing, and canonical init
creation now run in one processor and one Queue invocation. Backwards
compatibility with the thumbnail-only CLI/key contract was intentionally
dropped. Sprites retain per-recording roots under `artifacts/`, indexes use
per-stream timestamp-sortable `indexes/` prefixes, and init MP4s are
content-addressed under `init/`. The index object is written last as the commit
marker.

### 2026-08-10 — per-stream sortable index keys

Worker indexes are stored as
`artifacts/<stream>/indexes/<MediaMTX timestamp>.json`. The source-key parent,
not the reconnect-scoped `mtxi.stream_id`, defines the stable stream prefix.
This makes a prefix listing index-only and preserves chronological lexical
ordering. Sprite and content-addressed init keys are unchanged.

MediaMTX's `mtxi` must be removed from canonical init bytes before hashing,
otherwise stream ID, segment number, DTS, and NTP would defeat sharing. Closed
`mvhd` and all `tkhd` durations are zeroed. The checked-in HEVC fixture also
proved that B-frame presentation timestamps can be non-monotonic across tiny
physical parts, so schema v2 stores monotonic decode intervals separately from
presentation intervals and never recommends reordering byte ranges by PTS.

### 2026-08-09 — greenfield workspace

The target directory contained no project files and was not an initialized Git
worktree. The implementation can therefore follow the proposed workspace
boundary without compatibility work. No unrelated user files were changed.

### 2026-08-09 — initial decoder candidates

Current crates.io inspection found two promising pure-Rust backends:

- `rusty_h264-decoder` 0.8.0, BSD-2-Clause, describing Baseline, B-slice, and
  most High-profile support with CAVLC/CABAC.
- `rust_h265` 0.1.0, MIT OR Apache-2.0, describing Main/Main10 4:2:0 support.

Pure Rust avoids C/C++ standard-library, threading, and static-linking problems
for `wasm32-unknown-unknown`. Both candidates subsequently compiled for WASM
and decoded the checked-in synthetic profiles. They remain provisional until
the real MediaMTX camera corpus passes. The HEVC crate is young and actual
camera compatibility remains a material risk.

### 2026-08-09 — Worker packaging mode

Raw WASM checks succeeded, but `worker-build`'s panic-recovery wrapper failed
because the decoder module had no externref table. The production Cargo profile
already uses `panic=abort`, so packaging now uses
`worker-build --no-panic-recovery`. Both final packages build successfully and
expose the Queue handler.

### 2026-08-09 — content-aware idempotency

An existing key alone is insufficient when a recording is overwritten or
output settings change. Thumbnails now store source ETag, codec, dimensions,
quality, and pipeline-version metadata. Conditional ranged reads prevent a
mixed-version decode. Identity metadata provides convergence if the source is
replaced after decoding; an extra post-generation HEAD was removed because it
was still racy with the subsequent PUT. Identical content in a new R2 version
remains idempotent.

### 2026-08-09 — review hardening

Zero-length NAL entries are now skipped consistently by random-access
detection and Annex-B conversion through one fallible NAL iterator. The parser
no longer requires unused `tfdt` timing, box header decoding and child walking
are shared/lazy, the range reader is isolated from the pipeline state machine,
and the decode path has explicit find-start/decode-follow-up phases without
impossible-state `expect` calls. Worker configuration is parsed once per batch.

### 2026-08-09 — measured WASM SIMD

The first Cloudflare 4K AVC run used 868 ms CPU, above the desired 750 ms line.
Enabling `simd128` reduced identical work to 473 ms. HEVC Main10 also improved;
HEVC Main remained within its accepted envelope. The target flag is retained
in `.cargo/config.toml`.

### 2026-08-09 — isolated `0def` staging

The user authorized deployment only in the account with ID
`0def86f295b15a1a2d012774b38b04f9`. The config is pinned to that ID. Dedicated
bucket, codec Queues, DLQs, and prefix-filtered notification rules were created;
no pre-existing account resources were reused or modified.

## Verification log

| Date | Command/check | Result |
|---|---|---|
| 2026-08-09 | `rustc --version` | `rustc 1.94.0 (2026-03-02)` |
| 2026-08-09 | `cargo --version` | `cargo 1.94.0 (2026-01-15)` |
| 2026-08-09 | `rustup target list --installed` | Native ARM/x86 targets and `wasm32-unknown-unknown` installed. |
| 2026-08-09 | Workspace file audit | Empty project directory; no applicable `AGENTS.md`. |
| 2026-08-09 | `cargo info` and local source inspection | Candidate decoder APIs and licenses identified; `workers-rs` Queue and R2 range APIs confirmed. |
| 2026-08-09 | Native tests, both feature variants | AVC: 20 unit + 5 integration tests; HEVC: 20 unit + 6 integration tests; 7 Worker event/key/identity tests pass. |
| 2026-08-09 | `cargo clippy ... -D warnings`, both variants and Worker WASM | Passes. |
| 2026-08-09 | Final formatting and script audit | Workspace and fuzz manifests format cleanly; every shell helper passes `bash -n` and the MediaMTX generator is executable. |
| 2026-08-09 | Core and Worker `wasm32-unknown-unknown` checks | Both codec variants pass. |
| 2026-08-09 | Final SIMD Worker packages | H.264 679,063 bytes; H.265 561,082 bytes. |
| 2026-08-09 | `scripts/verify-fixtures.sh` | All seven synthetic/profile and MediaMTX-layout fixtures pass; SSIM 0.916–0.933. |
| 2026-08-09 | Sparse 100 MiB logical-object integration test | Stops after one 256 KiB read and one decoded sample. |
| 2026-08-09 | Native 4K AVC/HEVC/Main10 spikes | One sample each; ~0.06–0.07 s user CPU; 39.7–64.9 MiB peak RSS. |
| 2026-08-09 | `cargo +nightly fuzz run ... -runs=10000 -max_len=4096` | Box, codec-config, fragment, and NAL-conversion targets each complete 10,000 sanitizer-backed runs without a crash. |
| 2026-08-09 | MediaMTX v1.18.2 recorder smoke test | Synthetic RTSP AVC and HEVC recordings decode from the first sample; representative outputs are checked into the fixture corpus. |
| 2026-08-09 | `cargo audit` on root and fuzz lockfiles | No known RustSec advisories. |
| 2026-08-09 | Locked Wrangler 4.120.0 and `npm audit` | 37 development packages; no known npm vulnerabilities. |
| 2026-08-09 | Post-review Cloudflare R2 → Queue → Worker → R2 | Versions `ac6af22d…` / `c878b940…` process MediaMTX AVC/HEVC in one range/sample at 87/75 ms CPU; remote SSIM is 0.916665/0.916361. |
| 2026-08-09 | Remote profile and image oracle | AVC Baseline/Main/High and HEVC Main/Main10 pass; seven remote JPEGs score SSIM 0.920–0.934. |
| 2026-08-09 | Cloudflare 4K SIMD spike | AVC 473 ms CPU; HEVC Main 459 ms; Main10 309 ms. All read one range and emit one frame. |
| 2026-08-09 | Remote retry/idempotency behavior | Same-content events return `already_exists`; codec mismatch logs permanent failure and is acknowledged. |
| 2026-08-10 | Interval sprite tests, both codec variants | 28 core tests plus 5 AVC/6 HEVC integration tests and 7 Worker tests pass; MediaMTX fixtures produce 5-cell AVC and 9-cell HEVC sheets at `200ms`. |
| 2026-08-10 | Sprite FFmpeg oracle | All 14 generated cells score SSIM 0.914–0.917 against the selected FFmpeg frame. |
| 2026-08-10 | Strict Clippy, WASM checks, fuzz check | Both workspace codec matrices pass with warnings denied; both Worker WASM variants and the fuzz manifest compile. |
| 2026-08-10 | Wrangler staging dry-runs | AVC uploads at 786.27 KiB (291.09 KiB gzip); HEVC at 671.19 KiB (252.95 KiB gzip); interval/sprite bindings are present. No deployment performed. |
| 2026-08-10 | Live schema-v2 sprite deployment | Versions `74479e69…` / `9ab0cc2b…` deployed. Four R2-triggered jobs per codec produced valid 5-cell AVC and 9-cell HEVC sprites plus index/init artifacts. |
| 2026-08-10 | Live sprite timing | First AVC/HEVC jobs: 423/644 ms CPU and 3,144/5,197 ms wall. Warm medians: 160/255 ms CPU and 2,243/2,429 ms wall. Wrangler tail exposes no peak-memory measurement. |
| 2026-08-21 | Clip review implementation matrices | H.264: 81 tests; H.265: 82 tests. Strict Clippy, both rolodex-clip and Worker WASM builds, formatting, all five fuzz binaries, and end-to-end FFmpeg/SSIM fixture checks pass. |
| 2026-08-21 | Standalone repeated-source clip oracle | AVC 2.000011 s / SSIM 0.916665; HEVC 3.600011 s / SSIM 0.916361. FFprobe and full FFmpeg framehash decode pass outside HLS. |
| 2026-08-21 | Descriptor fuzz smoke (superseded) | 10,000 sanitizer-backed runs at 4 KiB maximum input completed; the serde-only target was subsequently replaced by `fragment_rewrite`. |
| 2026-08-21 | Root and fuzz dependency audits after clip CLI additions | No known RustSec advisories or warnings. |

## External inputs needed for full validation

These do not prevent implementation or synthetic testing, but they prevent
honest completion of the corresponding real-world milestones:

- 10–20 representative MediaMTX H.264 recordings and 10–20 H.265 recordings,
  or an approved path from which to copy them into `fixtures/`.
- Expected camera profiles, including whether HEVC Main10 is required.
- Enough representative staging jobs to calculate P50/P95 and validate a
  conservative Worker peak-memory margin.
- Production bucket/queue naming plus explicit approval for production rollout.

## Work log

### 2026-08-09

- Set the complete implementation as the active Codex goal.
- Audited the workspace and toolchain.
- Converted the proposed architecture into the durable implementation plan.
- Began the decoder viability gate by inspecting current pure-Rust AVC/HEVC
  decoder candidates and the current `workers-rs` Queue/R2 APIs.
- Implemented the Cargo workspace, core parser/pipeline, decoder adapters,
  image path, ranged CLI, and Cloudflare Queue/R2 Worker.
- Added deterministic fixtures covering AVC Baseline/Main/High and HEVC
  Main/Main10, then verified every image against FFmpeg.
- Added checked allocation/count limits, malformed-input tests, four fuzz
  targets, native benchmarking, Worker packaging, deploy helpers, and CI.
- Installed `cargo-fuzz` plus a minimal nightly toolchain without changing the
  default stable toolchain, then executed 10,000 short-run cases against each
  of the four fuzz targets with AddressSanitizer enabled.
- Packaged and measured both optimized Worker variants and ran native 4K
  single-picture CPU/memory spikes.
- Ran MediaMTX v1.18.2 locally, published synthetic AVC and HEVC over RTSP, and
  added the recorder's actual fMP4 output to the regression corpus.
- Hardened delayed decoder output, fragment/sample locations, partial-prefix reads,
  content-aware idempotency, overwrite races, configuration retries, and
  structured failure logs.
- Expanded Criterion into stage benchmarks, CI into feature/oracle/security
  gates, and added a requirement-by-requirement completion audit.
- Created isolated resources in account `0def…04f9`, deployed both Queue-only
  Workers, configured R2 notifications, and verified actual remote JPEGs.
- Measured scalar versus SIMD 4K runs and retained SIMD after AVC CPU fell from
  868 ms to 473 ms.
- Resolved the zero-length NAL permanent-failure path, unified NAL and box
  walking, removed unused timing requirements, split the pipeline into explicit
  discovery/decode phases, extracted the range reader, and simplified Worker
  configuration/source handling and logging.
- Re-ran both codec test/Clippy/WASM gates, all four fuzz targets for 10,000
  cases, the local FFmpeg oracle, and post-review MediaMTX staging smoke jobs;
  deployed only to the pinned `0def…04f9` account.
