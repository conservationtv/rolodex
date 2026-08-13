# Production dependency review

Recorded 2026-08-09 from Cargo metadata and both feature-resolved dependency
graphs.

| Dependency | Version | Purpose | Declared licence |
|---|---:|---|---|
| `rusty_h264-decoder` | 0.8.0 | AVC decode | BSD-2-Clause |
| `rusty_h264-common` | 0.8.0 | AVC primitives | BSD-2-Clause |
| `rust_h265` | 0.1.0 | HEVC Main/Main10 decode | MIT OR Apache-2.0 |
| `jpeg-encoder` | 0.7.1 | JPEG output | (MIT OR Apache-2.0) AND IJG |
| `workers-rs` (`worker`) | 0.8.5 | Queue/R2 Worker adapter | Apache-2.0 |
| `async-trait` | 0.1.92 | async byte-source boundary | MIT OR Apache-2.0 |
| `serde` / `serde_json` | 1.0.x | events and metrics | MIT OR Apache-2.0 |
| `sha2` | 0.10.9 | content-addressed canonical init keys | MIT OR Apache-2.0 |
| `thiserror` | 2.0.20 | checked errors | MIT OR Apache-2.0 |

Neither selected decoder introduces the LGPL/static-linking issue identified
for the initial libde265 candidate. The build links exactly one decoder.

`cargo audit` 0.22.2 scanned the root lockfile (140 crate dependencies) and fuzz
lockfile (36 packages) against 1,198 RustSec advisories with no findings. CI
repeats both audits. This is point-in-time evidence, not a substitute for
ongoing dependency updates or an organization's formal legal review.

Wrangler is pinned to 4.120.0 in `pnpm-lock.yaml`; `pnpm audit` reported no
known vulnerabilities in its 37-package development dependency graph.
