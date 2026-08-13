# Virtual HLS archive strategy

## Decision

Extend the existing Queue consumer into one recording processor. A single
completed MediaMTX `.mp4` event produces an index and canonical init, plus a
thumbnail when image generation succeeds:

```text
recordings/camera-12/2026-08-09_22-00.mp4
  -> artifacts/camera-12/2026-08-09_22-00/thumbnail.jpg (optional)
  -> artifacts/camera-12/indexes/2026-08-09_22-00.json
  -> artifacts/camera-12/archive/2026-08-09.json
  -> artifacts/camera-12/archive.json
  -> init/<sha256>.mp4
```

The thumbnail and index are specific to the source object identity. The init
object is content-addressed and shared by every recording with the same MP4
track/configuration header. The timestamp-named index JSON is written last and
acts as the commit marker for the artifact set.

The source-key parent is the stable stream path. Index objects for one stream
therefore share an index-only listing prefix such as
`artifacts/camera-12/indexes/`. MediaMTX's fixed-width timestamp basename is
retained as the JSON filename, so R2's lexical key order is chronological even
when the embedded `mtxi.stream_id` changes after a publisher reconnect.
Recordings without a source-key parent are not streams and are ignored by the
Worker; the legacy root-clip artifact layout is unsupported.
Recordings, indexes, sprites, and canonical init objects all use
`Cache-Control: public, max-age=31536000, immutable`.

This replaces the thumbnail-only output contract. Backwards compatibility with
the existing CLI, object keys, or R2 custom metadata is not a requirement.

The future playlist service is deliberately not a media proxy. It will select
rows/sidecars and format `EXTINF`, `EXT-X-BYTERANGE`, `EXT-X-MAP`,
`EXT-X-PROGRAM-DATE-TIME`, and `EXT-X-DISCONTINUITY`; all MP4 media ranges will
be fetched by the player directly from R2.

The browser-facing archive contract does not list the immutable indexes. It
fetches the predictable per-stream `archive.json`, whose ISO-date keys point to
compact daily manifests. Daily recording rows retain only identity, UTC bounds,
duration, source/init keys, random-access byte-range objects, and optional sprite
geometry and frame presentation times. The worker folds physical parts into one
object per random-access segment before publishing this projection.

The root is deliberately small and keyed by lexically sortable UTC date:

```json
{
  "version": 1,
  "stream": "camera-12",
  "days": {
    "2026-08-09": {
      "key": "artifacts/camera-12/archive/2026-08-09.json",
      "startedAt": "2026-08-09T00:00:00.000Z",
      "endedAt": "2026-08-10T00:05:00.000Z"
    }
  }
}
```

A daily file contains sorted recording rows. Segment durations come from the
next segment's `presentationStartNs` or the recording duration. Thumbnail
metadata is omitted on failure.

```json
{
  "version": 1,
  "stream": "camera-12",
  "date": "2026-08-09",
  "recordings": [{
    "id": "recordings/camera-12/2026-08-09_22-00.mp4",
    "startedAt": "2026-08-09T22:00:00.000Z",
    "endedAt": "2026-08-09T22:10:00.000Z",
    "durationNs": "600000000000",
    "source": "recordings/camera-12/2026-08-09_22-00.mp4",
    "init": "init/abc123.mp4",
    "segments": [{
      "presentationStartNs": "0",
      "byteOffset": 1024,
      "byteLength": 489201
    }]
  }]
}
```

Browser-facing wall-clock values use fixed-width ISO 8601 UTC with millisecond
precision. Media-relative durations, segment positions, and thumbnail frame
positions remain decimal nanoseconds so playlist calculations do not accumulate
floating-point error. A root day entry records the actual coverage of the
recordings in that start-date shard, which may extend into the following UTC day.

## Why this fits this repository

The current Rust workspace already has the important pieces:

- `rolodex-core` has a checked BMFF box walker, bounded async byte ranges,
  MediaMTX `moov`/`moof` handling, codec configuration parsing, random-access
  sample validation, and the image pipeline.
- The Worker already consumes completed R2 object events, performs
  ETag-conditional range reads, and writes immutable derivatives.
- The repository contains real MediaMTX v1.18.2 AVC and HEVC recorder fixtures,
  in addition to the synthetic decoder corpus.
- H.264 and H.265 remain compile-time Worker variants. The container index and
  init generation are codec-neutral, while thumbnail decoding continues to use
  the selected decoder.

The implementation should therefore broaden the existing bounded parse, not
introduce a second parser or runtime remuxer.

## Unified processor

The core entry point becomes conceptually:

```rust
async fn process_recording(
    source: &impl ByteSource,
    identity: SourceIdentity,
    options: ProcessorOptions,
) -> Result<ProcessorOutput>;

struct ProcessorOutput {
    thumbnail_jpeg: Option<Vec<u8>>,
    metadata: RecordingMetadata,
    init_mp4: Vec<u8>,
    metrics: ProcessorMetrics,
}
```

It makes one top-level walk through the source:

1. Read and parse `ftyp`/`moov` once.
2. Extract tracks, codec configuration, timescales, `trex` defaults, and
   MediaMTX `mtxi` identity/time data.
3. Walk every `moof` followed by its `mdat`, retaining only compact part and
   track-fragment summaries.
4. Build and hash a canonical init containing the tracks referenced by those
   fragments.
5. Derive the video duration and presentation-time thumbnail targets from the
   indexed samples.
6. For each target, decode from its preceding usable random-access point,
   skipping open-GOP leading pictures when necessary.
7. Scale each selected frame into a bounded grid and encode the complete sprite
   as `thumbnail.jpg`. Any thumbnail-only failure is recorded in the index and
   does not discard the already-built archive metadata or init.

The index walk never reads an `mdat` wholesale. A small read-ahead window
amortizes the `moof` header, `moof` body, and adjacent `mdat` header into one R2
range request for normal MediaMTX parts. Large or unusual `moof` boxes fall back
to exact bounded reads.

## Metadata contract

`index.json` is schema version 4 and contains only integer/string values whose
meaning is stable across languages. Unix nanoseconds and other values that can
exceed JavaScript's exact integer range are serialized as decimal strings.

```text
schema_version
source
  object_key
  etag
  byte_length
recording
  video_duration_ns          monotonic video decode span
  stream_id                 UUID/hex from mtxi
  segment_number            decimal string
  start_dts_ns              decimal string
  start_utc_ns              decimal string (mtxi.NTP)
init
  sha256
  byte_length
tracks[]                      tracks referenced by at least one part
  id
  kind                      video/audio/other
  timescale
  codec                     sample-entry FourCC
  width/height              when video
parts[]
  offset
  length                    exact [moof, end-of-mdat) range
  sequence_number           mfhd
  start_ns                  monotonic decode time from recording start
  start_utc_ns              mtxi.NTP + start_ns, when mtxi is present
  duration_ns               decode interval
  presentation_start_ns
  presentation_duration_ns
  video_starts_with_sync
  first_video_sync_ns       decode time
  first_video_sync_presentation_ns
  track_fragments[]
    track_id
    base_decode_time
    sample_count
    decode_start_ns
    decode_duration_ns
    presentation_start_ns
    presentation_duration_ns
    first_sync_decode_ns     video only
    first_sync_presentation_ns
thumbnail
  interval_ns                decimal string when interval sampling is enabled
  error                      omitted on success; failure reason otherwise
  frame_count                floor(video_duration / interval)
  columns/rows               sprite grid
  frame_width/frame_height   one cell
  sprite_width/sprite_height complete JPEG dimensions
  frames[]
    index
    target_ns                elapsed target from video start
    presentation_time_ns     selected sample PTS
    sample_offset
    sample_length
```

`start_ns` and `duration_ns` use `tfdt` plus resolved sample durations, so they
remain monotonic in decode order even when a camera emits B-frames. Presentation
intervals separately apply signed or unsigned `trun` composition offsets. This
distinction is required by the checked-in HEVC MediaMTX fixture: its first few
one-sample parts are decode-ordered but have deliberately non-monotonic PTS.
The video sync fields come from resolved sample flags; the thumbnail path
retains its stronger check that a flagged sync sample actually contains an AVC
IDR or HEVC IRAP NAL.

Sprite cells use row-major addressing: `x = (index % columns) * frame_width`
and `y = (index / columns) * frame_height`. The configured cell dimensions are
maxima. If all requested cells would exceed `MAX_SPRITE_PIXELS`, the processor
reduces every cell proportionally and records the resolved geometry above.

This is a physical-part index, not a preselected HLS segmentation policy. A
playlist compiler must group enough consecutive decode-ordered parts to cover
frame reordering; it must not sort physical parts by presentation time. The
one-part HLS entries remain a useful AVC/no-reordering proof-of-concept, but
production segmentation should combine consecutive byte-contiguous parts into
4-6 second decode-ordered ranges after Safari and hls.js validation. No re-index
is needed to change that policy.

## Canonical init MP4

The init object is not a copy of source media and contains no samples. It is
constructed as follows:

1. Emit a deterministic `ftyp` with `iso6` compatibility.
2. Copy the source `moov` track/sample-description and `mvex` structures for
   track IDs that occur in the recording's fragments; omit declared but unused
   tracks and their `trex` defaults.
3. Set every `mvhd` and `tkhd` duration to zero, for version 0 and version 1
   boxes.
4. Remove MediaMTX `udta/mtxi` recording identity/time data before hashing.
5. Preserve empty fragmented-MP4 sample tables and track IDs exactly.
6. SHA-256 the final bytes and store them at `init/<sha256>.mp4`.

Dropping `mtxi` is essential for deduplication: its stream ID, segment number,
DTS, and NTP change between recordings even when decoder configuration does
not. A codec/sample-entry, parameter-set, resolution, channel-layout, or
timescale change remains in `moov`, changes the hash naturally, and tells the
playlist compiler to emit a discontinuity plus a new `EXT-X-MAP`.

## Validation and rejection rules

The processor targets MediaMTX fragmented MP4 rather than arbitrary MP4. It
rejects a recording for virtual-HLS indexing when any required invariant is
ambiguous:

- `moov` must precede media fragments and describe exactly one supported video
  track for thumbnail generation.
- Every indexed `traf` must contain `tfdt` and use movie-fragment-relative
  addressing (`default-base-is-moof` with resolvable `trun.data_offset`).
- Each part must be one `moof` immediately followed by one `mdat`; every sample
  range must fall within that `mdat`.
- Sample duration, size, flags, and composition time must resolve from
  `trex`/`tfhd`/`trun` without guessing.
- Counts, box sizes, total metadata bytes, parts, tracks, samples per part, and
  total R2 reads remain explicitly bounded.

Synthetic fMP4 without `mtxi` can remain available for decoder tests, but the
production Worker requires `mtxi` so it never publishes archive metadata with
invented wall-clock time.

## R2 commit and retry semantics

For a source ETag, the Worker performs:

1. Process the source through ETag-conditional range reads.
2. HEAD/PUT the content-addressed init object.
3. PUT the thumbnail with source identity metadata when one was generated.
4. PUT `<stream>/indexes/<timestamp>.json` with source identity, processor
   version, init hash, and artifact keys as custom metadata.
5. ETag-conditionally merge the row into the UTC daily manifest, then merge its
   date/key into `archive.json` while pruning dated objects that no longer exist.

On retry, a matching index commit marker is accepted only if the referenced init
exists and, when the commit records an available thumbnail, that thumbnail also
exists. The retry still updates the mutable manifests before acknowledging. An
unavailable thumbnail is an explicit, idempotent commit state.
Partial previous attempts are overwritten or completed safely. Concurrent
writers can write the same init key because the key is the SHA-256 of identical
bytes.

## Playlist strategy after this goal

The first playlist proof of concept should use the AVC/no-reordering fixture,
one physical part per HLS entry, a canonical init URI, and a bounded time
window. It should test, in order:

1. One raw `moof+mdat` byte range in native Safari and hls.js.
2. A recording-file boundary with `EXT-X-DISCONTINUITY`, a repeated/new
   `EXT-X-MAP`, and program date time.
3. Seeking with a hidden preroll beginning at the part containing the preceding
   video sync sample.
4. Multi-`moof` byte ranges for 4-6 second virtual segments.

Across files, the compiler compares MediaMTX stream ID and sequential segment
number for source continuity, but still emits a discontinuity at every file
boundary because each file resets `tfdt`. A changed init hash is independently
a discontinuity. The UI continues to own the large archive timeline and asks
the playlist compiler for a small window around a requested UTC time.

## Operational gates

Before production rollout:

- Verify R2 custom-domain `206` and CORS behavior from the real browser origin.
- Measure index R2 requests/bytes and CPU for full one-hour, multi-track camera
  files. With the default one-second MediaMTX part duration, expect roughly
  3,600 part probes per recording; the deployment therefore requires the paid
  Worker subrequest allowance and an explicit configured safety limit.
- Validate generated init files with FFprobe and both target players.
- Run boundary/seek tests on representative AVC, HEVC, B-frame, audio, 4K, and
  codec-change recordings from the private fleet corpus.
- Keep the existing 128 MiB Worker memory ceiling visible in metrics and reject
  rather than partially publish an index when a limit is exceeded.

## Delivery sequence

1. Add movie/track/`mtxi` and timing-aware fragment models to
   `rolodex-core`.
2. Add canonical init construction and content hashing.
3. Replace the thumbnail-only pipeline with the unified processor and schema-v4
   metadata output.
4. Replace the CLI with a three-artifact local harness.
5. Replace Worker keying/idempotency/writes and update Wrangler limits.
6. Prove both codec feature builds against the MediaMTX fixtures, FFprobe the
   generated init objects, and retain the existing decoder/SSIM verification.
