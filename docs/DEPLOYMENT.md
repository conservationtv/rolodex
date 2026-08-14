# Cloudflare deployment

## Prerequisites

- A Cloudflare account with Workers, R2, and Queues enabled.
- An existing R2 bucket holding recordings and generated artifacts.
- A Queue for object-create notifications and a dead-letter Queue.
- Node/pnpm for Wrangler and Rust with `wasm32-unknown-unknown` installed.

The checked-in `wrangler.jsonc` is an account-independent H.264 template. Its
default resource names are:

```text
R2 bucket: rolodex-recordings
Queue: rolodex-jobs
DLQ: rolodex-dead-letter
R2 binding: MEDIA_BUCKET
```

Keep `max_batch_size` and `max_concurrency` at 1. The mutable daily and stream
archive manifests also use ETag preconditions, but serial Queue consumption is
the normal single-writer path.

## Deploy to Cloudflare

Deploy to Cloudflare reads `wrangler.jsonc`, copies the public GitHub or GitLab
repository into the user's account, and deploys it with Workers Builds. The R2
bucket, consumer Queue, and dead-letter Queue are provisioned and bound during
that flow. Resource names can be changed on the setup page.

The repository does not currently have a public Git remote. Once it does, add
this badge to the README with that URL:

```md
[![Deploy to Cloudflare](https://deploy.workers.cloudflare.com/button)](https://deploy.workers.cloudflare.com/?url=<PUBLIC_GIT_REPOSITORY_URL>)
```

The template defaults to H.264. `scripts/build-worker.sh` installs a minimal
Rust toolchain when Workers Builds does not provide one, adds the
`wasm32-unknown-unknown` target, installs the pinned `worker-build`, and builds
the Worker.

### Required one-time notification

The deployment creates and connects the Worker resources, but R2 event
notifications are not part of the Wrangler configuration schema. Run this
command exactly once after the first deployment:

```bash
npx wrangler r2 bucket notification create rolodex-recordings \
  --event-type object-create \
  --queue rolodex-jobs \
  --prefix "recordings/" \
  --suffix ".mp4"
```

Replace `rolodex-recordings`, `rolodex-jobs`, or `recordings/` if you changed
their defaults during deployment. The suffix should remain lowercase `.mp4`.
Before running the command again, inspect the bucket's existing notification
rules in the Cloudflare dashboard; overlapping object-create rules are rejected.

## Build and deploy one codec

Fork and clone the repository, then create the resources and deploy directly
with Wrangler:

```bash
git clone https://github.com/<you>/<repo>.git
cd <repo>
pnpm install
npx wrangler login
npx wrangler r2 bucket create rolodex-recordings
npx wrangler queues create rolodex-jobs
npx wrangler queues create rolodex-dead-letter
npm run deploy # or: npm run deploy:h265
npx wrangler r2 bucket notification create rolodex-recordings \
  --event-type object-create --queue rolodex-jobs \
  --prefix "recordings/" --suffix ".mp4"
```

Edit `wrangler.jsonc` before running these commands if you want different
resource names or prefixes. Create the notification only once.

The portable configuration defaults to H.264 and the `rolodex-h264` Worker
name. `deploy:h265` selects the H.265 Cargo feature and deploys as
`rolodex-h265`. For another name, run `CODEC=h264 npx wrangler deploy --name
<name>` (or use `CODEC=h265`).

The build uses `worker-build --no-panic-recovery`: the release profile is
`panic=abort`, and the panic-recovery module path currently requires a WASM
externref table that the decoder modules do not emit. Panics therefore fail the
invocation and follow Queue retry behavior; input parsing itself returns checked
errors and is fuzzable.

The WASM target enables `simd128`; the Cloudflare 4K AVC A/B result reduced CPU
from 868 ms to 473 ms. See `docs/STAGING_RESULTS.md`.

## Worker variables

| Variable | Default | Meaning |
|---|---:|---|
| `SOURCE_PREFIX` | `recordings/` | Only keys beneath this prefix are accepted. |
| `ARTIFACT_PREFIX` | `artifacts/` | Root for per-recording sprites, immutable indexes, and per-stream archive manifests. |
| `INIT_PREFIX` | `init/` | Content-addressed canonical init root. |
| `SOURCE_BUCKET` | unset | Optional event bucket-name validation. |
| `MAX_WIDTH` | `480` | Maximum width of each sprite cell. |
| `MAX_HEIGHT` | `270` | Maximum height of each sprite cell. |
| `JPEG_QUALITY` | `78` | JPEG quality from 1 through 100. |
| `THUMBNAIL_INTERVAL` | `10s` | Gap between sprite frames; accepts integer `ns`, `us`, `ms`, `s`, `m`, or `h` values. |
| `MAX_THUMBNAIL_FRAMES` | `1000` | Hard cell-count cap for one recording sprite. |
| `MAX_SPRITE_PIXELS` | `20000000` | Hard total sprite pixel cap; cells scale down together to fit it. |
| `INITIAL_READ_BYTES` | `262144` | Cached R2 prefix bytes. |
| `FULL_READ_THRESHOLD_BYTES` | `16777216` | Files at or below this size are fetched once and shared from memory by indexing and thumbnail extraction. |
| `FRAGMENT_READ_AHEAD_BYTES` | `1048576` | Part metadata probe/read-ahead; 1 MiB amortizes serial R2 latency across many MediaMTX parts. |
| `MAX_R2_BYTES_PER_JOB` | `134217728` | Hard total range-read cap. |
| `MAX_SAMPLES_DECODED` | `100000` | Hard cumulative access-unit decode cap across all sprite cells. |
| `MAX_SAMPLES_EXAMINED` | `100000` | Hard cumulative video-sample read cap across all sprite cells. |
| `MAX_SAMPLES_INDEXED` | `1000000` | Hard whole-recording sample metadata cap. |
| `MAX_PARTS_PER_RECORDING` | `5000` | Hard MediaMTX part cap. |
| `MAX_TOP_LEVEL_BOXES` | `20000` | Hard top-level BMFF box cap. |

The core additionally limits initialization metadata to 8 MiB, fragment
metadata to 8 MiB and an individual thumbnail sample to 32 MiB. The Wrangler
configuration requests 10,000 paid-plan subrequests because a default one-hour
MediaMTX recording can contain roughly 3,600 one-second parts.

## Event and retry behavior

Accepted actions are `PutObject`, `CopyObject`, and
`CompleteMultipartUpload`. Out-of-scope events are acknowledged. Before work,
the consumer heads the source and deterministic index key. The per-stream
`<timestamp>.json` index is the commit marker only when its custom metadata
matches the current source ETag/processor version and its content-addressed init
object still exists. The metadata also records whether a thumbnail was created;
when it was, that object must still exist. A failed thumbnail attempt is a valid
committed state and does not suppress archive metadata.

A source key must include a nonempty stream path below `SOURCE_PREFIX`, for
example `recordings/camera-12/clip.mp4`. Legacy root-level clips such as
`recordings/clip.mp4` are out of scope and are acknowledged without artifacts.

After an index is committed, the consumer projects its essential playback data
into `<stream>/archive/YYYY-MM-DD.json`, then adds that object to
`<stream>/archive.json`. Both mutable writes use the ETag read by the merge as an
`If-Match` precondition; first creation uses `If-None-Match: *`. A conflict
re-reads and merges rather than overwriting another writer. Each stream update
also HEADs the dated objects already in `archive.json` and removes entries whose
objects have expired under R2 lifecycle policy. Dated manifests may briefly
reference earlier recordings that have already expired; consumers should treat
404/410 media responses as retention gaps. Root entries expose ISO UTC
`startedAt`/`endedAt` coverage, so a recording that crosses midnight remains in
its start-date manifest while a UI can still select it by time-window overlap.

Recordings and all derived objects are immutable by storage contract. Uploaders
must set `Cache-Control: public, max-age=31536000, immutable` on recording MP4s;
the Worker sets the same policy on index JSON, sprite JPEG, and canonical init
MP4 objects. Existing objects retain their prior HTTP metadata until they are
rewritten or copied with replacement metadata.

Every source range is conditional on the headed ETag, so an overwrite during
decoding is retried instead of producing a mixed-version image. If the source
changes after the final range, stored identity metadata makes the later object
create event regenerate the deterministic artifact set. The Worker writes the
init first, the thumbnail second when available, and the immutable index commit
marker before merging the mutable daily and stream manifests. A retry that finds
an already-committed index still repairs those manifests before acknowledging
the Queue message.

R2 transport failures are retried and eventually reach the configured DLQ.
Invalid/unsupported media and codec mismatches are permanent and acknowledged
after an error log, preventing an unprocessable recording from looping forever.
Alert on `permanent_failure`, inspect its source key, and keep the original
recording for offline diagnosis.
Thumbnail decode, scale, and JPEG failures are instead recorded in schema-v4
index metadata and do not fail the Queue job.

## Staging rollout

1. Deploy to a staging bucket and Queue.
2. Upload the approved fixture corpus with `scripts/benchmark-cloudflare.sh`.
3. Query Workers Logs for `outcome=created` and the nested `metrics` object.
4. Record actual invocation CPU/wall time alongside bytes read, resolution,
   decoder count, output size, and Worker version.
5. Check failures and the DLQ, then hold load long enough to observe P95.
6. Keep Queue concurrency and batch size at one while the Worker owns mutable
   stream manifests.
7. Create the production notification only after staging gates pass.

The current controlled staging evidence is in `docs/STAGING_RESULTS.md`.
Synthetic files remain native decoder tests; the processor accepts them without
MediaMTX metadata, while archive-manifest publication still needs a UTC start.

Cloudflare references:

- <https://developers.cloudflare.com/workers/platform/limits/#subrequests>
- <https://developers.cloudflare.com/r2/buckets/event-notifications/>
- <https://developers.cloudflare.com/queues/configuration/batching-retries/>
- <https://developers.cloudflare.com/queues/configuration/consumer-concurrency/>
- <https://developers.cloudflare.com/workers/observability/logs/workers-logs/>
