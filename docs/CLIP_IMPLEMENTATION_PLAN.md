# Clip implementation plan

A pull-based clip publisher for MediaMTX fMP4 recording directories. Given a
UTC time range, it locates the covering recordings on local disk, splices the
selected fragments into one new fMP4 object with zero re-encoding, uploads the
artifact set to R2, and returns a player-compatible HLS manifest.

This is the inverse route of the Queue worker: on-demand, local-first, and it
uploads only the bytes a clip needs. Spliced output is the only mode; whole
source recordings are never uploaded by this tool.

```text
recordings/camera-12/*.mp4 (local disk)
  -> find(range)            ClipDescriptor (pure metadata, serializable)
  -> process(descriptor)    clip.mp4 + index.m3u8 + descriptor.json
  -> upload(name, artifact) clips/camera-12/<from>_<duration>/…
```

## Why splicing works

MediaMTX writes `default_base_moof` fragments: every `[moof, end-of-mdat)`
range is self-contained, with data offsets relative to the moof start. A
canonical init followed by any ordered subset of whole fragments is therefore a
valid fMP4 byte stream. No sample data is rewritten and nothing is re-encoded.
Non-zero `tfdt` base times are acceptable to players behind `EXT-X-MAP`.

## Crate layout

```text
crates/
  clip-core/    # find/process/upload API; no HTTP, no filesystem policy,
                # no CLI concerns; wasm-compatible for a future API/Worker
  clip-cli/     # argument parsing, filesystem opener, R2 store, output
```

Both join the existing workspace and reuse `rolodex-core` as a path dependency
for `ByteSource`, box walking (`parse_archive_moov`, `parse_indexed_fragment`),
canonical init generation (`build_canonical_init`), sync detection, and
decimal-nanosecond serialization. No decoder feature is enabled: clipping is
codec-neutral, exactly like the worker's index path.

`FileSource` (currently private to `crates/cli`) is lifted into a shared
location — either `rolodex-core` behind a `std-fs` feature or duplicated into
`clip-cli` — so `clip-core` itself never touches the filesystem directly.

## clip-core API

Three phases, deliberately separable so a future API service can run them on
different machines or at different times. `ClipDescriptor` is plain serde data
and acts as the contract between phases.

### 1. find

```rust
pub struct ClipRequest {
    pub stream: String,        // recording directory name, e.g. "camera-12"
    pub from_utc_ns: u64,
    pub to_utc_ns: u64,
}

pub async fn find(
    opener: &dyn SourceOpener,
    listing: &[CandidateFile],   // pre-scanned by the caller (CLI walks the dir)
    request: &ClipRequest,
) -> Result<ClipDescriptor>;
```

- The caller lists candidate files; MediaMTX basenames
  (`%Y-%m-%d_%H-%M-%S-%f`) are fixed-width and lexically chronological, so
  candidate selection is: the last file starting at or before `from` through
  the last file starting before `to`.
- Each candidate is walked with the existing bounded ranged parse to produce
  exact `[moof, end-of-mdat)` part ranges, decode/presentation timelines,
  wall-clock times, and sync flags. Files whose actual coverage misses the
  range are dropped.
- The clip start snaps to the part containing the latest video sync point at
  or before `from`; the end snaps to the first part boundary at or after `to`.
  `requested` and `actual` ranges are both recorded.

```rust
pub struct ClipDescriptor {
    pub version: u32,
    pub stream: String,
    pub requested: TimeRangeNs,
    pub actual: TimeRangeNs,          // after sync/part snapping
    pub sources: Vec<SourceSlice>,    // ordered by wall clock
}

pub struct SourceSlice {
    pub id: String,                   // relative path / object key of the recording
    pub stamp: SourceStamp,           // byte length + mtime guard, checked in process()
    pub init_sha256: String,          // canonical init identity for this file
    pub init_len: u64,
    pub discontinuity_before: bool,   // new file or stream_id change
    pub parts: Vec<PartSlice>,
}

pub struct PartSlice {
    pub offset: u64,                  // into the source recording
    pub length: u64,
    pub duration_ns: u64,
    pub start_utc_ns: u64,
    pub starts_with_sync: bool,
}
```

### 2. process

```rust
pub async fn process(
    opener: &dyn SourceOpener,
    descriptor: &ClipDescriptor,
) -> Result<ClipArtifacts>;

#[async_trait(?Send)]
pub trait SourceOpener {
    async fn open(&self, id: &str) -> Result<Box<dyn ByteSource>>;
}

pub struct ClipArtifacts {
    pub media: Artifact,        // clip.mp4  — canonical init + selected fragments
    pub manifest: Artifact,     // index.m3u8 — VOD, byteranges into clip.mp4
    pub descriptor: Artifact,   // descriptor.json — provenance sidecar
}

pub struct Artifact {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}
```

- `process` re-checks each `SourceStamp` before reading; a mismatch is a
  permanent error (the recording changed since `find`).
- The media artifact is the canonical init (`build_canonical_init`, inactive
  tracks and `mtxi` removed, same code path as the worker) followed by each
  selected fragment range copied verbatim.
- The manifest is a VOD media playlist:
  - `EXT-X-MAP:URI="clip.mp4",BYTERANGE="<init_len>@0"`;
  - one `EXTINF` + `EXT-X-BYTERANGE` per part (offsets recomputed into the
    spliced object);
  - `EXT-X-PROGRAM-DATE-TIME` from part wall-clock times;
  - `EXT-X-DISCONTINUITY` where `discontinuity_before` is set;
  - relative URI `clip.mp4` so the artifact set is location-independent.
- Sources with differing canonical inits are rejected in v1
  (`Error::InitMismatch`). The HLS-correct fix — inline each init in the
  spliced object and switch `EXT-X-MAP` after a discontinuity — is a planned
  extension, not v1 scope.
- v1 buffers artifacts in memory. Clip size is naturally bounded
  (range duration × bitrate); a size cap in `find` (default 512 MiB estimated)
  fails fast instead of building an oversized clip. Streaming assembly is a
  later extension behind the same API.

### 3. upload

```rust
#[async_trait(?Send)]
pub trait ArtifactStore {
    async fn upload(&self, name: &str, artifact: &Artifact) -> Result<UploadOutcome>;
}

pub enum UploadOutcome { Created, AlreadyExists }
```

- clip-core defines the trait and the deterministic key scheme:

  ```text
  clips/<stream>/<from-iso-basic>_<duration-s>/clip.mp4
  clips/<stream>/<from-iso-basic>_<duration-s>/descriptor.json
  clips/<stream>/<from-iso-basic>_<duration-s>/index.m3u8   (written last)
  ```

- Keys derive from `(stream, actual range)`, so re-runs are idempotent.
- Upload order is media, descriptor, manifest; the manifest is the commit
  marker, mirroring the worker's index-last convention.
- Implementations HEAD before PUT and return `AlreadyExists` to skip work.
- All objects use `Cache-Control: public, max-age=31536000, immutable`.

## clip-cli

```bash
clip \
  --recordings ./recordings --stream camera-12 \
  --from 2026-08-13T22:19:00Z --to 2026-08-13T22:21:30Z \
  --bucket rolodex --endpoint https://<account>.r2.cloudflarestorage.com \
  [--out ./clip-output]        # local-only mode, skips upload
  [--base-url https://media.example.com]  # printed manifest URL prefix
```

- Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
  environment variables only; never flags.
- `--out` writes the three artifacts to a directory and skips upload entirely,
  which keeps the end-to-end path testable without R2.
- On success the CLI prints the manifest URL (or local path) plus a JSON
  metrics line consistent with the existing CLI: files scanned, parts
  selected, bytes read, bytes uploaded, snap deltas.
- The R2 `ArtifactStore` uses a minimal SigV4 signer (`sha2` + `hmac` +
  a small HTTP client) rather than `aws-sdk-s3`, keeping the dependency
  audit in `docs/DEPENDENCIES.md` short. Single PUT only; the descriptor size
  cap keeps objects far below R2's single-request limit. Multipart is out of
  scope.

## Decisions and risks

| Item | Position |
|---|---|
| Snap-start pre-roll | The first spliced fragment may contain samples that decode-precede its sync sample. MSE pipelines drop leading undecodable frames; acceptable for v1. Recorder configs where part duration equals the GOP make part boundaries sync-aligned and eliminate this entirely. |
| HEVC RASL leading pictures | After splicing at a CRA, RASL pictures are undecodable and are dropped by decoders. Cosmetic at worst; documented, not mitigated. |
| Non-zero `tfdt` at clip start | Permitted by HLS with `EXT-X-MAP`. Verified against hls.js and Safari native HLS in M4. Rebasing timestamps would require moof rewrites and is explicitly out of scope. |
| Mixed inits across files | Rejected in v1 with a clear error. Multi-`EXT-X-MAP` splicing is the designed extension. |
| Concurrent recording | The final segment file of a live stream is still being written. `find` uses the walk's own bounds; a truncated trailing fragment is excluded, and the `SourceStamp` check in `process` catches growth between phases. |
| Sharing | Objects land under an unguessable-enough deterministic key on a public-read prefix. Presigned playlists require signing every URI in the manifest; out of scope. |

## Milestones

### M1 — clip-core find
Directory-listing contract, candidate selection, ranged walk reuse,
sync/part snapping, `ClipDescriptor` serde round-trip. Tests against the
MediaMTX v1.18.2 fixtures plus synthetic multi-file listings covering: range
inside one file, range spanning files, range in a gap, range past the end,
start before the first sync.

### M2 — clip-core process
Splice assembly, manifest generation, stamp verification, init-mismatch
rejection. Golden-file tests for the manifest; `ffprobe` validation that the
spliced fixture output is a conforming fMP4 whose stream parameters match the
source (extend `scripts/verify-fixtures.sh`).

### M3 — clip-cli local mode
Argument parsing, `FileSource` opener, `--out` directory output, metrics
line. End-to-end test: fixture directory in, three artifacts out, manifest
byteranges verified against the written `clip.mp4`.

### M4 — upload and playback
SigV4 `ArtifactStore`, HEAD-before-PUT idempotency, manifest-last commit
order. Manual verification: clip from a real recording directory, uploaded to
the staging bucket, played via hls.js (reuse the demo page) and Safari native
HLS. Record results in `docs/LOCAL_RESULTS.md`.

### M5 — hardening
Fuzz target for descriptor deserialization, size-cap behavior, README and
`docs/DEPENDENCIES.md` updates, CI wiring for the new crates.
