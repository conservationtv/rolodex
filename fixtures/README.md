# Fixture corpus

The checked-in fixtures are deterministic, synthetic fragmented MP4 smoke
tests generated with FFmpeg. They prove container/sample extraction and decoder
integration without depending on private camera footage.

| File | Codec/profile | Pixel format | Resolution | Purpose |
|---|---|---|---:|---|
| `h264/mediamtx-v1.18.2-640x360.mp4` | AVC High | yuv420p | 640x360 | Actual MediaMTX v1.18.2 recorder layout |
| `h264/synthetic-baseline-320x180.mp4` | AVC Baseline | yuv420p | 320x180 | Baseline decoder smoke test |
| `h264/synthetic-main-320x180.mp4` | AVC Main | yuv420p | 320x180 | Main decoder smoke test |
| `h264/synthetic-320x180.mp4` | AVC High | yuv420p | 320x180 | Multi-frame fMP4 integration fixture |
| `h265/synthetic-320x180.mp4` | HEVC Main | yuv420p | 320x180 | Multi-frame fMP4 integration fixture |
| `h265/synthetic-main10-320x180.mp4` | HEVC Main 10 | yuv420p10le | 320x180 | High-bit-depth decode/image-path test |
| `h265/mediamtx-v1.18.2-640x360.mp4` | HEVC Main | yuv420p | 640x360 | Actual MediaMTX v1.18.2 recorder layout |

Regenerate these with `scripts/generate-fixtures.sh`. The script requires an
FFmpeg build with `libx264` and `libx265`.

The two `mediamtx-v1.18.2` files were produced by publishing the same synthetic
source over RTSP to MediaMTX's recorder. Regenerate them with
`MEDIAMTX_BIN=/path/to/mediamtx ./scripts/generate-mediamtx-fixtures.sh`; the
script deliberately requires MediaMTX v1.18.2 so fixture provenance does not
change silently.

These are not a substitute for the real MediaMTX camera corpus. Add real files
under codec-specific subdirectories without committing sensitive footage, and
record them in a private manifest containing camera vendor/model, codec,
profile, bit depth, resolution, GOP length, bitrate, and recording duration.
The real-corpus validation command is `scripts/verify-fixtures.sh`.
