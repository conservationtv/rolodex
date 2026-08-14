# Completion audit

This audit maps the original 21-part plan to authoritative evidence. `Complete`
means the implementation and available-environment evidence satisfy the item.
`Partial` identifies evidence that still requires the private camera fleet or a
larger production-like run.

| Plan area | Status | Evidence |
|---|---|---|
| 1. Narrow scope | Complete | `README.md` and checked unsupported-layout errors constrain the input/output surface. |
| 2. Repository structure | Complete | Cargo workspace separates Cloudflare-independent core, CLI, and Worker adapters. |
| 3. Compile-time codec | Complete | Exact-one feature compile gates pass; both final WASM modules package independently. |
| 4. Decoder interface | Complete | `VideoDecoder`, `CodecConfig`, and planar `DecodedFrame` isolate both backends. |
| 5. Decoder spike | Partial | AVC Baseline/Main/High and HEVC Main/Main10 pass native, WASM, and staging—including 4K—but the private camera fleet is unavailable. |
| 6. Minimal fMP4 parser | Complete for scope | Bounded `moov`/`moof`, `avcC`/`hvcC`, `trex`/`tfhd`/`trun`, sample flags, and checked offsets are implemented and fuzzed; unused `tfdt` timing is optional. |
| 7. Ranged reader | Complete | R2 conditional ranges, partial-prefix reuse, total-byte caps, 100 MiB sparse-tail and 2 MiB skipped-box tests pass. |
| 8. Annex-B conversion | Complete | Length sizes, NAL counts/lengths, allocation checks, conversion tests, and fuzz target pass. |
| 9. First usable picture | Complete | Flags plus IDR/IRAP inspection; decoder continues across bounded follow-up AUs until its first output and stops immediately. |
| 10. Image path | Complete | Planar 4:2:0 area downscale precedes YCbCr JPEG; 8/10-bit paths and FFmpeg SSIM oracles pass. |
| 11. Memory discipline | Partial | Files up to the configured 16 MiB threshold are buffered once and shared; larger files use bounded ranges. Counts/bytes are bounded, native 4K RSS is recorded, and 4K Workers succeed; exact peak Worker memory is unavailable. |
| 12. Queue Worker | Complete in staging | Actual R2 create events produced remote JPEGs; retries, permanent failures, metadata, and acknowledgements are observed. |
| 13. Tiny batches | Complete | Both staging consumers use batch size 1 and concurrency 2. |
| 14. Correctness corpus | Partial | Seven synthetic/MediaMTX fixtures and all named profiles pass locally/remotely; vendor camera corpus is unavailable. |
| 15. Parser fuzzing | Complete as smoke gate | Four sanitizer-backed targets complete 10,000 runs each after final parser changes. Long-running CI fuzzing remains an ongoing practice. |
| 16. Three performance levels | Partial | Native stage benchmarks, optimized WASM sizes, and actual Cloudflare CPU/wall evidence exist; representative percentile load remains. |
| 17. Benchmark matrix | Partial | Controlled 180p/360p/4K rows exist in `STAGING_RESULTS.md`; camera/vendor rows await the private corpus. |
| 18. Performance gates | Partial | All controlled byte/frame/size gates pass; final 4K CPU passes after SIMD. P50/P95 and exact Worker memory need larger evidence. |
| 19. Optimization order | Complete for observed failure | One-frame/one-range behavior was verified first; measured AVC 4K CPU motivated SIMD, reducing CPU from 868 to 473 ms. |
| 20. Deployment interface | Complete | `CODEC`, matching Wrangler environments, build codec checks, account pinning, and codec-specific Workers are live. |
| 21. Milestones | Partial | M1–M4 and a controlled M5/M6 are achieved. Full M0/M5 and production M7 require camera data and rollout authority. |

## Verification inventory

- Native unit/integration tests: both codec matrices pass.
- Host Worker tests: event shape, key mapping, and content/settings identity.
- Native and WASM Clippy: both codec variants with warnings denied.
- Compile gates: zero or two codec features fail as required.
- Local FFmpeg oracle: seven fixtures pass.
- Remote FFmpeg oracle: seven R2 thumbnails pass.
- Fuzzing: four targets × 10,000 sanitizer-backed cases.
- RustSec: root and fuzz lockfiles contain no known advisories at audit time.
- Packaging: H.264 679,063-byte WASM; H.265 561,082-byte WASM.
- Staging: actual MediaMTX create, idempotency, codec mismatch, Main10, and 4K
  paths observed in account `0def…04f9`.

The active goal is therefore not honestly complete: private fleet compatibility,
representative P50/P95, exact Worker memory evidence, and production rollout
remain outside the current workspace/account evidence.
