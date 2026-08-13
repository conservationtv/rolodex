# Cloudflare staging results

Recorded on 2026-08-09 and 2026-08-10 in the Cloudflare account whose ID begins `0def`.
These are real R2 Event Notification → Queue → Rust Worker → R2 executions,
not local timing estimates.

## Deployed staging topology

| Resource | Value |
|---|---|
| Account ID | `0def86f295b15a1a2d012774b38b04f9` |
| R2 bucket | `rolodex-staging` |
| H.264 Queue | `rolodex-h264-staging` |
| H.264 DLQ | `rolodex-h264-staging-dlq` |
| H.265 Queue | `rolodex-h265-staging` |
| H.265 DLQ | `rolodex-h265-staging-dlq` |
| H.264 notification rule | `28b8052c-7359-4a42-8e41-7c0b5a202669` |
| H.265 notification rule | `4aefde36-89d7-4bd7-80d5-96dc852b9416` |
| Thumbnail-only H.264 baseline version | `ac6af22d-8b9e-480b-bc97-33e80d407b4e` |
| Thumbnail-only H.265 baseline version | `c878b940-0327-4b70-95b3-7941ab3d61db` |
| Current sprite H.264 version | `74479e69-1004-473d-85cb-4362ae9c1d68` |
| Current sprite H.265 version | `9ab0cc2b-3dec-449e-81ba-15b19ae41d6c` |

The Workers have Queue consumers only. `workers_dev` and preview URLs are
disabled; both former public URLs return HTTP 404.

## Final-version MediaMTX smoke jobs

Both sources were produced by a local MediaMTX v1.18.2 recorder. Cloudflare's
tail event supplied the CPU and wall measurements.

| Codec | Resolution | Source/R2 bytes | Requests | Samples decoded | CPU | Wall | JPEG bytes |
|---|---:|---:|---:|---:|---:|---:|---:|
| H.264 High | 640×360 | 35,385 | 1 | 1 | 87 ms | 1,444 ms | 11,563 |
| H.265 Main | 640×360 | 55,701 | 1 | 1 | 75 ms | 1,956 ms | 11,539 |

The post-review remote JPEGs scored 0.916665 SSIM for AVC and 0.916361 for
HEVC against FFmpeg. Earlier SIMD versions also processed 320×180 smoke
objects at 55 ms CPU for AVC Main and 36 ms for HEVC Main.

## Interval-sprite deployment — 2026-08-10

The unified schema-v2 processor was deployed to both staging Workers with a
`200ms` interval so the short MediaMTX fixtures exercise multiple cells. Four
unique R2 source keys per codec triggered independent Queue jobs. The first job
created its content-addressed init; the following three reused it.

| Codec | Cells | Decoded samples | Source/R2 bytes | Requests | First CPU / wall | Warm CPU median (range) | Warm wall median (range) | JPEG |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| H.264 High | 5 | 15 | 35,385 | 1 | 423 ms / 3,144 ms | 160 ms (133–409) | 2,243 ms (2,202–2,436) | 60,991 B |
| H.265 Main | 9 | 30 | 55,701 | 1 | 644 ms / 5,197 ms | 255 ms (207–401) | 2,429 ms (2,198–2,582) | 105,350 B |

Against the thumbnail-only smoke rows above, the first AVC sprite invocation
used 4.86× CPU and 2.18× wall time to emit five cells; the first HEVC invocation
used 8.59× CPU and 2.66× wall time to emit nine cells. Using the three warm jobs,
the median multipliers were 1.84× CPU / 1.55× wall for AVC and 3.40× CPU /
1.24× wall for HEVC. Per emitted cell, first-run CPU was 84.6 ms for AVC and
71.6 ms for HEVC, close to the old one-cell 87/75 ms results.

The live JPEGs are 960×810 (2×3 cells) for AVC and 960×1350 (2×5 cells) for
HEVC. Their first cells score 0.916663 and 0.916361 SSIM respectively against
FFmpeg. Both jobs still fetched each small source in one R2 request; the extra
cost is decoder resets, scaling, sprite assembly, JPEG encoding, and the new
index/init writes rather than additional source transfer.

Wrangler tail reports invocation CPU and wall time but does not include a
peak-memory field. These successful runs prove only that the jobs remained
within the Worker isolate limit; they do not provide an exact memory comparison
with the former thumbnail-only Worker.

## Profile coverage in the Worker runtime

The staging pipeline emitted valid JPEGs for AVC Baseline/Main/High and HEVC
Main/Main10. Every job examined one fragment and decoded one sample. A deliberate
HEVC object delivered to the AVC Worker produced `permanent_failure` after 2 ms
CPU and was acknowledged; the test object was then removed.

Re-uploading unchanged bytes exercises content-aware idempotency. The Workers
returned `already_exists` without decoding or writing, at 15–16 ms CPU. The
thumbnail identity includes source ETag, codec, output settings, and pipeline
version; R2 version changes with identical content remain idempotent.

## 4K isolate spike and SIMD decision

Each source contained one 3840×2160 random-access picture. The first scalar AVC
run crossed the desired 750 ms line, so an otherwise identical `simd128` build
was deployed and measured. The checked-in `.cargo/config.toml` retains SIMD.

| Variant | Build | R2 bytes | Samples | CPU | Wall | JPEG bytes |
|---|---|---:|---:|---:|---:|---:|
| AVC High 8-bit | scalar | 231,128 | 1 | 868 ms | 2,444 ms | 6,867 |
| AVC High 8-bit | SIMD | 231,128 | 1 | 473 ms | 1,916 ms | 6,867 |
| HEVC Main 8-bit | scalar | 222,967 | 1 | 357 ms | 1,928 ms | 6,853 |
| HEVC Main 8-bit | SIMD | 222,967 | 1 | 459 ms | 2,040 ms | 6,853 |
| HEVC Main10 | scalar | 222,772 | 1 | 371 ms | 1,909 ms | 6,884 |
| HEVC Main10 | SIMD | 222,772 | 1 | 309 ms | 1,824 ms | 6,884 |

Single-event CPU values contain runtime variance, so this is an engineering
spike rather than a percentile distribution. SIMD is retained because the
large AVC improvement clears the gate, Main10 improves, and HEVC Main remains
well inside the two-second acceptable CPU threshold.

Successful 4K execution proves the jobs remained below the platform's hard
isolate limit, but Workers tail does not report peak isolate memory. It does not
prove the aspirational 64 MiB target.

## Image oracle

`scripts/verify-cloudflare-staging.sh` downloaded all seven stored staging
thumbnails and compared each with FFmpeg's first decoded frame.

| Fixture | SSIM |
|---|---:|
| MediaMTX AVC | 0.920605 |
| AVC High | 0.932768 |
| AVC Baseline | 0.933020 |
| AVC Main | 0.932762 |
| MediaMTX HEVC | 0.920250 |
| HEVC Main | 0.933767 |
| HEVC Main10 | 0.932960 |

The three 4K remote JPEGs scored 0.923184 (AVC), 0.923237 (HEVC Main), and
0.919790 (HEVC Main10).

## What remains before production

- Run the private camera-fleet corpus across models, firmware, resolutions,
  bitrates, GOP lengths, and long recordings.
- Run enough representative jobs to calculate P50/P95 rather than treating
  these controlled samples as percentiles.
- Obtain a credible peak-memory measurement or maintain a conservative
  operational margin from successful 4K execution.
- Configure production resources, alerts, and rollout only after those gates.
