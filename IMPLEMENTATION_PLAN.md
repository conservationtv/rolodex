# Rolodex — Implementation Plan

## Goal

Build one Rust codebase that produces two mutually exclusive Cloudflare Worker
variants. Each variant consumes MediaMTX recording notifications from a
Cloudflare Queue, uses bounded R2 range reads to locate the first independently
decodable video picture in an fMP4 recording, decodes it, scales it, encodes a
JPEG thumbnail, and writes that thumbnail to R2.

```text
MediaMTX recording in R2
          |
          | object-create event
          v
Cloudflare Queue
          |
          v
Rust Queue consumer Worker
          |
          +-- bounded R2 range reads
          +-- narrow ISO BMFF/fMP4 parser
          +-- first random-access access unit
          +-- compile-time H.264 OR H.265 decoder
          +-- YUV downscale
          +-- JPEG encode
          v
Thumbnail in R2
```

The primary design rule is: **read and decode only the bytes needed to produce
one image**. The implementation is not intended to become a general media
framework.

## Scope

Supported:

- MediaMTX-generated fragmented MP4 recordings.
- Exactly one video track; non-video tracks are ignored.
- `avc1`/`avc3` H.264 or `hvc1`/`hev1` H.265, selected at compile time.
- AVC configuration from `avcC`; HEVC configuration from `hvcC`.
- Fragment sample layout resolved from `trex`, `tfhd`, and `trun`.
- The first IDR/IRAP random-access picture that the selected decoder can emit.
- Aspect-preserving downscaling without upscaling.
- JPEG output with bounded dimensions and configurable quality.
- R2 Event Notification Queue messages and idempotent thumbnail writes.

Deliberately unsupported:

- General MP4 demuxing, progressive/non-fragmented MP4 sample tables, audio
  processing, arbitrary seeking, transcoding, filters, multiple output frames,
  runtime codec switching, or dynamically loading another decoder.

## Repository layout

```text
.
├── Cargo.toml
├── wrangler.jsonc
├── IMPLEMENTATION_PLAN.md
├── PROGRESS.md
├── crates
│   ├── rolodex-core
│   │   ├── src
│   │   │   ├── codec
│   │   │   ├── mp4
│   │   │   ├── frame.rs
│   │   │   ├── jpeg.rs
│   │   │   ├── lib.rs
│   │   │   ├── pipeline.rs
│   │   │   ├── scale.rs
│   │   │   └── source.rs
│   │   └── tests
│   ├── cli
│   └── worker
├── fixtures
│   ├── h264
│   └── h265
├── fuzz
├── benches
└── scripts
```

`rolodex-core` has no Cloudflare dependencies. Its input boundary is a
bounded async byte-range source and its successful output is JPEG bytes plus
metrics. The CLI and Worker are adapters around that boundary.

## Compile-time codec selection

The core defines `codec-h264` and `codec-h265` Cargo features and fails the
build unless exactly one is selected. Each executable forwards those features.
The selected feature controls a single `SelectedDecoder` alias, so the unused
codec backend is absent from the final WASM module.

CI builds and tests both variants independently for native and
`wasm32-unknown-unknown`. The deployment script accepts only `CODEC=h264` or
`CODEC=h265` and converts the value to the corresponding Cargo feature.

## Decoder gate (Milestone 0/1)

Before treating the integration as production-ready:

1. Collect representative raw Annex-B access units and MediaMTX recordings
   from the actual cameras.
2. Prove each decoder compiles for `wasm32-unknown-unknown` without threads,
   filesystem, or network requirements.
3. Decode real Baseline/Main/High AVC and Main/Main10 HEVC random-access
   pictures to planar YUV.
4. Record native and WASM size/time/memory results.
5. Confirm licence and distribution terms.

Initial candidates are pure-Rust decoders to avoid C/C++ toolchain and static
linking problems in Workers:

- H.264: `rusty_h264-decoder` (BSD-2-Clause).
- H.265: `rust_h265` (MIT OR Apache-2.0).

Candidate selection remains conditional on tests against the real camera
corpus. Unsupported real-world profiles or tools are a go/no-go failure, not a
reason to silently return a misleading frame.

## Core interfaces

The Cloudflare-independent byte boundary is conceptually:

```rust
trait ByteSource {
    fn len(&self) -> u64;
    async fn range(&self, offset: u64, length: usize) -> Result<Vec<u8>>;
}
```

Every implementation must reject out-of-bounds reads and short responses.
Instrumentation records the number of requests and bytes returned.

The codec-independent decoder boundary accepts configuration parameter sets
and Annex-B access units and returns at most one decoded planar frame per call.
Frames preserve dimensions and bit depth; 10-bit HEVC samples are reduced to
8-bit only in the image path.

## Minimal ISO BMFF/fMP4 parser

Only these structures are interpreted:

- Top level: `ftyp`, `moov`, `moof`, `mdat`.
- Initialization: `trak`, `tkhd`, `mdia`, `hdlr`, `minf`, `stbl`, `stsd`,
  `mvex`, `trex`.
- AVC sample entries: `avc1`/`avc3` and `avcC`.
- HEVC sample entries: `hvc1`/`hev1` and `hvcC`.
- Fragments: `traf`, `tfhd`, and `trun`; `tfdt` may be present but is not
  required because the thumbnail path does not consume decode timing.

The parser supports 32-bit, 64-bit, and container-extent box sizes; all offset
arithmetic is checked. It imposes explicit limits before allocating or walking
attacker-controlled counts. It extracts:

- video track ID and actual sample entry codec;
- NAL length size and VPS/SPS/PPS as applicable;
- `trex` sample defaults;
- `tfhd` base/default overrides;
- `trun` data offsets, sizes, and flags.

The fragment walker scans top-level box headers without reading `mdat`. It
reads a bounded `moov`, a bounded `moof`, and only the candidate sample bytes.
It uses both ISO sample flags and codec NAL types to require AVC IDR or HEVC
IRAP content before starting a fresh decoder.

## Ranged access strategy

The pipeline starts with a moderate prefix and services incomplete-box needs
with exact ranges. A request crossing the cached-prefix boundary reuses the
prefix and fetches only the missing tail. It must not assume `moov` fits in the
initial prefix. Hard
limits protect Worker memory:

- maximum initialization box bytes;
- maximum fragment metadata bytes;
- maximum individual sample bytes;
- maximum top-level boxes/fragments/samples examined;
- maximum total bytes fetched per job.

Typical expected reads are a prefix/header range, one exact `moov` range, one
or more small `moof` ranges, and the bytes for one video sample.

## Sample conversion and random-access selection

MP4 length-prefixed NAL units are validated and rewritten into one reusable
Annex-B buffer. Codec configuration parameter sets are prefixed to the
candidate access unit. Invalid NAL lengths are rejected before allocation.

The fragment algorithm is:

```text
scan moof
  -> select the video traf by track ID
  -> resolve trex/tfhd/trun defaults
  -> walk bounded sample metadata
  -> identify a sync candidate
  -> read only that sample
  -> confirm AVC IDR or HEVC IRAP NAL
  -> feed selected decoder
  -> if output is delayed, feed bounded subsequent access units
  -> stop immediately on first complete frame
```

If the first fragment has no usable sample, scanning continues within the
configured limits. The pipeline never decodes beyond the first returned frame.

## Image path

The first implementation downsizes the planar 4:2:0 planes directly using an
area filter, then interleaves the reduced YCbCr data for JPEG encoding. This
avoids allocating a full-resolution RGB frame. Output defaults:

- maximum dimensions: 480 x 270;
- JPEG quality: 78;
- no upscaling;
- even output dimensions for 4:2:0.

The target is less than 50 KiB per thumbnail for typical surveillance frames,
but quality and size remain measurable staging parameters rather than assumed
guarantees.

## Worker behavior

The Queue consumer processes messages sequentially with an initial
`max_batch_size` of 1. For each R2 event it:

1. accepts only supported object-create actions and `.mp4` keys;
2. ignores configured thumbnail prefixes;
3. derives a deterministic `.jpg` key;
4. heads the source object and derives an identity from its ETag, codec,
   pipeline version, and output settings;
5. acknowledges an existing thumbnail only when its stored identity matches;
6. runs the core pipeline through ranges conditional on that source ETag;
7. writes JPEG bytes with `image/jpeg` and identity metadata; conditional
   ranged reads prevent mixed-version decoding, while metadata makes a
   last-moment source overwrite converge on the later create event;
8. logs structured job metrics and acknowledges the message.

Retryable failures call Queue retry. Permanent failures such as codec mismatch,
unsupported media, or invalid input are logged distinctly so a poison message
does not retry forever. The initial implementation exposes conservative
configuration for retry classification and can route exhausted messages to a
dead-letter queue through Wrangler configuration.

Example mapping:

```text
recordings/camera-12/2026-08-09_184501.mp4
  -> thumbnails/camera-12/2026-08-09_184501.jpg
```

## Metrics and memory discipline

Per-job metrics include:

- codec and source dimensions;
- samples/fragments examined and samples decoded;
- R2 range requests and bytes fetched;
- source and thumbnail byte sizes;
- selected sample offset/size and random-access kind.

Phase-level timings are maintained by the native stage benchmarks rather than
instrumenting Worker hot paths with unsuitable timers.

Cloudflare's invocation CPU time is the authoritative production cost metric.
The design target is below 64 MiB peak memory and it must stay below the Worker
isolate limit. Whole-recording buffering and unbounded counts are prohibited.

## Correctness and security verification

Tests cover box header edge cases, `avcC`, `hvcC`, `trex`/`tfhd`/`trun`
resolution, NAL conversion, sync detection, range limits, scaling, JPEG output,
key mapping, event parsing, and idempotency decisions.

The real fixture corpus should include AVC Baseline/Main/High and HEVC
Main/Main10 recordings across 720p, 1080p, 1440p, and 4K, multiple vendors,
bitrates, GOP lengths, and short/long recordings. FFmpeg produces the offline
oracle. Output comparison uses dimensions plus pixel/perceptual thresholds,
not JPEG byte equality.

Fuzz targets cover box parsing, AVC/HEVC configuration records, fragment
metadata, and MP4-to-Annex-B conversion. All parser limits are tested with
malicious sizes and counts.

## Performance verification

Three layers are maintained:

1. Native microbenchmarks for init parsing, fragment parsing, NAL conversion,
   decoding, scaling/JPEG, and the full pipeline.
2. Local release WASM builds recording module size and, where a suitable WASM
   runner is available, initialization/decode time and peak linear memory. The
   production target enables `simd128` after the measured 4K AVC A/B result.
3. Staging Workers fed a controlled R2/Queue fixture matrix, using Workers Logs
   for actual CPU and wall time.

Initial gates:

| Metric | Desired | Reconsider |
|---|---:|---:|
| P50 CPU | < 250 ms | ordinary jobs are multi-second |
| P95 CPU | < 750 ms | H.265 1080p is multi-second |
| Typical R2 bytes | < 2 MiB | tens of MiB |
| Peak memory | < 64 MiB | approaches 128 MiB |
| Frames decoded | 1 typical, <= 3 P95 | frequent full GOP |
| Thumbnail | < 50 KiB typical | consistently much larger |
| WASM | preferably < 5 MiB | deployment/platform limit |

H.265 4K CPU up to roughly two seconds and memory below 96 MiB may still be
acceptable after cost modelling.

## Optimization order

1. Minimize frames decoded.
2. Minimize R2 bytes fetched.
3. Profile decoder reconstruction/DSP work.
4. Apply WASM SIMD only to measured hot paths.
5. Optimize planar scaling and bit-depth reduction.
6. Optimize JPEG encoding.
7. Compare `opt-level=3`, `s`, and `z`, then tune LTO, symbol stripping, and
   `wasm-opt` based on CPU cost and bundle constraints.

## Milestones and completion criteria

### M0 — corpus characterization

- Fixture manifest and collection instructions exist.
- Actual camera profile, bit depth, resolution, GOP behavior, and first random
  access position are recorded for each supplied fixture.

### M1 — decoder proof

- Both candidates compile to Worker WASM.
- Representative Annex-B AVC and HEVC access units decode to valid YUV.
- Native/WASM size and performance results are recorded.

### M2 — local CLI

- `rolodex input.mp4 output.jpg` works for both feature variants.
- Outputs validate against FFmpeg across the available corpus.

### M3 — ranged core

- The CLI pipeline uses only `ByteSource::range`.
- Tests prove bounded reads and report bytes fetched.

### M4 — Worker

- Queue/R2 integration builds to WASM.
- Key validation, codec mismatch, idempotency, writes, retries, and metrics are
  implemented and tested where possible off-platform.

### M5 — staging benchmark

- H.264 and H.265 benchmark Workers process the fixture matrix.
- Actual Cloudflare CPU/wall/R2/memory observations are captured.

### M6 — measured optimization

- Any failing gate has a profile-backed decision or optimization.

### M7 — rollout

- Production bindings, notifications, queue/dead-letter settings, dashboards,
  and alarms are configured.
- Concurrency increases only after stable P95 results.

Local implementation is complete when M1–M4 code and verification pass for
available fixtures. Full project completion additionally requires the user's
real MediaMTX corpus and authorized Cloudflare staging/production environments;
those external dependencies are recorded explicitly in `PROGRESS.md` rather
than represented as completed without evidence.
