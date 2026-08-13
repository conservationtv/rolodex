# Real MediaMTX corpus procedure

The checked-in FFmpeg fixtures are smoke tests. Decoder/backend approval needs
actual MediaMTX recordings from every supported camera family.

Collect at least 10–20 recordings per codec spanning:

- 720p, 1080p, 1440p, and 4K;
- short and long recordings;
- each camera manufacturer/model and firmware family;
- observed GOP lengths and bitrates;
- AVC Baseline/Main/High as present;
- HEVC Main and Main10 as present.

For each file, retain a private manifest with a stable fixture ID, SHA-256,
source device, codec/profile/level, bit depth, dimensions, duration, frame rate,
GOP behavior, first random-access sample offset, and whether the local and
staging Worker validations pass. Do not commit sensitive footage.

Characterize with `ffprobe`, run both the expected codec CLI and the opposite
codec mismatch test, then run `scripts/verify-fixtures.sh`. Review generated
contact sheets or individual thumbnails for geometry, color, corruption, and
camera overlays. A decoder `Unsupported` error is a backend selection blocker;
do not broaden the parser or conceal the error until the exact coding tool is
understood.
