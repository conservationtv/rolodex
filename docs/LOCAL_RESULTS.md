# Local verification results

Recorded 2026-08-09 on an Apple Silicon macOS host with Rust 1.94. These values
are implementation evidence and regression baselines, not Cloudflare billing or
production-camera results.

## Checked-in synthetic corpus

Every fixture decoded exactly one sample and produced a valid JPEG at its
bounded output dimensions. FFmpeg first-frame comparison used
`scripts/verify-fixtures.sh`.

| Codec/profile | SSIM |
|---|---:|
| AVC High through MediaMTX v1.18.2 | 0.916665 |
| AVC High, multi-frame | 0.931624 |
| AVC Baseline | 0.931761 |
| AVC Main | 0.931456 |
| HEVC Main, multi-frame | 0.932568 |
| HEVC Main10 | 0.932331 |
| HEVC Main through MediaMTX v1.18.2 | 0.916361 |

## Native Criterion smoke benchmark

Stage-level quick regression benchmark at 320 x 180:

| Stage | H.264 | H.265 |
|---|---:|---:|
| Parse `moov` | 434–442 ns | 449–471 ns |
| Parse `moof` | 206–212 ns | 347–350 ns |
| NAL conversion | 142 ns | 60–62 ns |
| Decode first frame | 618–652 µs | 863–869 µs |
| Scale YUV | 264–267 µs | 266–274 µs |
| Encode JPEG | 319–335 µs | 331–336 µs |
| Complete pipeline | 1.244–1.288 ms | 1.416–1.473 ms |

The benchmark identifiers were subsequently made codec-specific so Criterion
does not incorrectly label the two decoder variants as a regression against
one another.

## Worker package size

Optimized `worker-build` + `wasm-opt` output:

| Variant | `index.wasm` bytes |
|---|---:|
| H.264 | 679,063 |
| H.265 | 561,082 |

Both packages expose the Queue handler. Packaging uses the documented
no-panic-recovery path and `simd128`; see `docs/DEPLOYMENT.md`.

## 4K single-picture spike

Deterministic FFmpeg `testsrc2`, 3840 x 2160, one independently decodable
picture, medium encoder preset. `/usr/bin/time -l` measured the optimized native
CLI. All variants read one sample and produced a 480 x 270 JPEG.

| Variant | Source sample | JPEG | User CPU | Peak RSS |
|---|---:|---:|---:|---:|
| AVC High 8-bit | 230,173 B | 6,867 B | 0.07 s | 62,717,952 B (59.8 MiB) |
| HEVC Main 8-bit | 219,610 B | 6,853 B | 0.07 s | 41,648,128 B (39.7 MiB) |
| HEVC Main10 | 219,424 B | 6,884 B | 0.06 s | 68,091,904 B (64.9 MiB) |

The Main10 result is slightly above the aspirational 64 MiB native target but
well below the 96 MiB “still acceptable” threshold. Worker isolate memory and
CPU remain staging measurements because native RSS includes a different runtime
and allocator.

## Bounded-read proof

An integration test presents a valid first fMP4 fragment followed by a sparse
logical tail, for a total logical object size of 100 MiB. The complete pipeline
returns after one 256 KiB source read and one decoded sample. This verifies early
termination and range accounting without allocating the logical recording.
