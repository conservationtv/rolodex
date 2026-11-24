# @conservationtv/rolodex

Browser client for playing back Rolodex archives: completed MediaMTX
fragmented MP4 recordings that live in a private S3-compatible bucket
(R2). It reads the manifests written by the Rolodex worker, compiles
virtual HLS playlists over the original MP4 bytes (`EXT-X-BYTERANGE`,
no remuxing), and drives an ordinary `<video>` element through hls.js.

This package is headless. It ships no UI — just a small set of
composable pieces plus a state store designed to sit behind whatever
player interface you want to build.

> **Pairs with the worker.** This library consumes the artifacts
> produced by the Rolodex Rust worker (`crates/worker`): the stream
> directory (`archive.json`), daily heads
> (`archive/YYYY-MM-DD/head.json`), immutable metadata pages, canonical init
> segments, and thumbnail sprite sheets. Point it at a bucket the
> worker has populated and everything below works; without those
> artifacts there is nothing to play.

## What you get

- **`S3ArchiveStorage`** — authenticated access to archive manifests.
  Media, init segments, and thumbnails can use stable URLs on a public
  cache domain, or presigned S3 URLs when the bucket is fully private.
- **`ArchiveManifestClient`** — manifest discovery and caching, plus
  the queries a UI needs: timelines, point-in-time lookup, and
  thumbnail cues for hover previews.
- **`PlaylistFactory`** — compiles a window of the archive into an
  in-memory HLS playlist (a blob URL) on demand. Usually you never
  touch this directly.
- **`ArchivePlayerStore`** — the piece you build UI against. A single
  external store that owns playback, seeking across recordings and
  gaps, viewport pan/zoom, hover thumbnails, and error recovery. Its
  API is deliberately shaped for `React.useSyncExternalStore`, but it
  has no React dependency.

## Installation

```sh
npm install @conservationtv/rolodex
```

Requires a browser with Media Source Extensions (hls.js is used on
every browser, including Safari, so playback behaves identically
everywhere).

## Initialisation

Everything starts from an `ArchiveSource` describing the bucket the
worker writes into:

```ts
import {
  S3ArchiveStorage,
  ArchiveManifestClient,
  ArchivePlayerStore,
} from "@conservationtv/rolodex";

const storage = new S3ArchiveStorage({
  bucket: "my-archive-bucket",
  root: "streams/gate-camera",
  manifest: "streams/gate-camera/archive.json",
  region: "auto",
  endpoint: "https://<account>.r2.cloudflarestorage.com",
  publicBaseUrl: "https://media.example.com", // optional R2 custom domain
  auth: {
    type: "temporary_credentials", // or "credentials"
    accessKeyId: "…",
    secretAccessKey: "…",
    sessionToken: "…",
    expiresAt: "2026-08-14T12:00:00Z",
  },
});

const manifests = new ArchiveManifestClient(storage, {});

// Find the most recent day so we know where to open the player.
const latest = await manifests.load();

const store = new ArchivePlayerStore({
  manifests,
  initial: {
    playhead: latest.startedAt,
    viewport: { startedAt: latest.startedAt, endedAt: latest.endedAt },
  },
});

await store.initialize({ range: latest });
```

Credentials are always used client-side to read manifests, so use short-lived
temporary credentials scoped to read-only access in production. When
`publicBaseUrl` is set, immutable playback objects use that domain without
query parameters so Cloudflare can cache them. Otherwise those URLs are
presigned, with their lifetime automatically clamped to the credential expiry.

### Vanilla JS

```ts
const video = document.querySelector("video");
const detach = store.attach(video);

store.subscribe(() => {
  const snapshot = store.getSnapshot();
  render(snapshot); // your rendering code
});

// Jump to the first recording and start playing.
const first = store
  .getSnapshot()
  .timeline?.spans.find((span) => span.kind === "recording");
if (first) await store.seek(first.startedAt, { autoplay: true });

// Later:
detach();
store.dispose();
```

### React

The store's `subscribe`/`getSnapshot`/`getServerSnapshot` have stable
identities and plug straight into `useSyncExternalStore`:

```tsx
import { useEffect, useRef, useState, useSyncExternalStore } from "react";

function Player({ manifests, latest }) {
  const [store] = useState(() => new ArchivePlayerStore({
    manifests,
    initial: { playhead: latest.startedAt, viewport: latest },
  }));
  const video = useRef(null);

  const snapshot = useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getServerSnapshot,
  );

  useEffect(() => {
    if (!video.current) return undefined;
    return store.attach(video.current);
  }, [store]);
  useEffect(() => () => store.dispose(), [store]);

  return (
    <div>
      <video ref={video} playsInline muted />
      <button onClick={() => store.toggle()}>
        {snapshot.playback.playing ? "Pause" : "Play"}
      </button>
      <span>{snapshot.playhead}</span>
    </div>
  );
}
```

## The snapshot

The store emits a single immutable snapshot per change. It is a
discriminated union on `phase`:

| Phase | Meaning |
| --- | --- |
| `idle` | Constructed, `initialize()` not yet called |
| `loading-manifest` | Fetching the stream manifest |
| `ready` | Manifest (and possibly media) loaded, not playing |
| `loading-media` | Compiling and loading a playlist after a seek |
| `playing` / `paused` | Media bound to the element |
| `gap` | Playhead is in a period with no recording; a wall-clock simulation advances the playhead until the gap ends, then playback resumes automatically |
| `error` | Unrecoverable failure (`snapshot.error`) |

Every snapshot carries `playback` (playing, rate, mediaTime, buffered
seconds), `viewport`, and the current `playhead` as an ISO UTC string.
Once loaded it also carries `coverage` (full archive extent),
`timeline`, and `location`.

## Timeline

`snapshot.timeline` is the model for drawing a scrubber. It contains an
ordered list of spans covering the viewport, each either a recording or
a gap:

```ts
for (const span of snapshot.timeline?.spans ?? []) {
  // span.kind: "recording" | "gap"
  // span.startedAt / span.endedAt: ISO UTC strings
}
```

Render each span proportionally between `viewport.startedAt` and
`viewport.endedAt`, then wire pointer events back into the store:

```ts
// Click to seek (utc computed from the pointer position):
await store.seek(utc, { autoplay: snapshot.playback.playing });

// Hover previews — the store fetches the right thumbnail sprite cell:
store.setHover(utc, pointerX);
store.clearHover();
```

`snapshot.hover` then carries a `ThumbnailCue`: a presigned sprite
sheet URL plus a pixel `crop` (`x`, `y`, `width`, `height`) identifying
the cell to draw, e.g. via `background-position` or
`canvas.drawImage`.

Viewport navigation is built in:

```ts
store.zoomAt(utc, 2);   // zoom in around a timestamp
store.panBy(-3600);     // pan one hour earlier
store.fit();            // fit the full archive (or fit(range))
```

To pick up recordings the worker has finished since the page loaded,
call `store.refresh()` periodically; it revalidates expired manifests and
updates the timeline in place while retaining fresh immutable pages.

## Other controls

```ts
await store.play();
store.pause();
await store.toggle();
store.setPlaybackRate(2);   // clamped to 0.25–4
await store.seek(utc, { autoplay: true, signal });
```

The async methods reject when they fail, so `await` is enough to know
whether they worked:

- `initialize()` and `seek()` reject with the failure, which is also
  published as `phase: "error"`. When their `signal` aborts they reject
  with its reason and do not enter `error`: an aborted `initialize()`
  restores the previous snapshot, and a seek aborted before media
  loading starts leaves the snapshot unchanged. Once media loading has
  started the seek completes, and the signal only cancels the wait.
- With a media element attached, `seek()` resolves once the element can
  show a frame at the target, not merely once the playlist is bound. A
  seek into a gap resolves once the gap clock is positioned. A seek that
  a later seek supersedes resolves without effect.
- `play()` (and `toggle()`) rejects when the browser refuses playback,
  for example with `NotAllowedError` before a user gesture, and leaves
  the store paused. Blocked autoplay requested through
  `seek(utc, { autoplay: true })` is not a seek failure; the seek
  resolves paused.
- `refresh()` rejects on failure and publishes the error as
  `snapshot.warning`, keeping the current timeline.

`recordingRanges(timeline)` returns the recorded parts of a timeline as
ordered ISO ranges, merging touching recordings, for drawing coverage
bars or combining with other sources.

Seeks that land inside the currently compiled playlist window are
instant; others compile a new window transparently. Damaged or missing
media is skipped automatically to the next random-access point, and
playback rolls across recording boundaries and gaps without user
intervention.

## Lower-level use

You can skip `ArchivePlayerStore` entirely and build on the primitives:

- `manifests.getTimeline(range)` — spans for any range.
- `manifests.locate(utc)` — which recording/gap contains an instant,
  with recording offset and segment index.
- `manifests.getThumbnail(utc)` — a thumbnail cue for an instant.
- `manifests.describe()` — the stream identity and whole-archive coverage.
- `manifests.getVideo(key)` — a fetchable URL for a media or init object.
- `new PlaylistFactory(manifests).create({ at })` — a `PlaylistLease`
  with a blob playlist `url`, `mediaTimeAt(utc)` / `utcAt(mediaTime)`
  converters, and `revoke()`.

All public types are exported; `S3ArchiveStorage` can also be replaced
with your own `ArchiveStorage` implementation (for example, one that
resolves URLs through a signing service instead of embedding
credentials in the page).

### Paged archive format and refresh

The client now requires version-2 stream discovery. This is a hard cutover: there
is no conversion or automatic reindexing of version-1 history. The Rust worker
starts a new discovery index when the next recording completes. Old recording
objects remain stored, but are not included in the new index.

`archive.json` contains small day entries. Each entry points to
`archive/YYYY-MM-DD/head.json`, which references a content-addressed page
directory and the current tail page. Recording metadata is partitioned by its
UTC start into five-minute buckets. The newest bucket is the tail; its metadata
uses the same immutable page format as sealed buckets. Page bodies retain the
version-1 recording codec (including decimal nanoseconds and byte ranges).

A new recording changes the tail hash, head and small discovery object. The
sealed-page directory changes only on rollover or a correction to a sealed
page. The writer publishes referenced objects before conditionally committing
the head, retries competing writes and rereads the head before updating discovery.
Late arrivals update their UTC partition. Immutable objects from failed writes
or superseded revisions can remain unreferenced; garbage collection is a
separate maintenance concern. Recording identity and its UTC partition must be
stable across replacement uploads.

`locate()` and thumbnail lookup load only the page covering the UTC target (or
the next page for a gap). `getTimeline(range)` loads the pages intersecting the
requested range, preserving full recording metadata for playlist construction.
A whole-history timeline therefore still needs all its pages on a cold load.
Parsed caches are bounded; refreshing does not clear sealed pages.

`HttpArchiveStorage`, now exported directly by Rolodex, and `S3ArchiveStorage`
share `ArchiveDocumentCache`. Both use response freshness and ETag revalidation.
`refresh()` checks expired discovery data; it does not bypass a fresh HTTP TTL.
`maxAge` is a fallback for storage without response freshness, not an override
of server directives. `no-store` responses are not retained by the document
cache; `no-cache` responses require validation on every access. `Age`, `Date`
and request transit time reduce the remaining `max-age`. A `304` retains the
parsed document while updating its freshness. `s-maxage` applies only to shared
caches, not the client cache.

| Object | Cache-Control |
| --- | --- |
| Archive root and heads older than two UTC days | `public, max-age=300, must-revalidate` |
| Current and previous UTC day heads | `public, max-age=5, must-revalidate` |
| Content-addressed directory, page and init | `public, max-age=31536000, immutable` |
| Per-recording generated index and thumbnail | `public, max-age=31536000, immutable` |

Generated thumbnails and per-recording indexes retain their existing filenames. Reprocessing can overwrite these objects; existing cached copies may remain stale for their cache lifetime. This is an accepted trade-off.

The Rust writer sets R2 HTTP metadata on PUT. `Age` is not upload metadata:
it measures response cache residence, not object age. HTTP gateways must emit
`Date`, report the response's `Age`, preserve ETag and expose those headers to
cross-origin clients. The Rewind demo reads through an R2 custom domain, where Cloudflare supplies `Date` and cache-residence `Age`; its CORS policy permits `If-None-Match`.

Custom storage implementations can expose `getArchiveObject(key, options)` and
compose `ArchiveDocumentCache` with their own `DocumentLoader`, preserving the
same freshness contract without depending on HTTP or S3 authentication code.

### Renewing temporary credentials

`ArchiveSource.auth` also accepts an `ArchiveCredentialProvider`:

```ts
const storage = new S3ArchiveStorage({
  bucket, root: '', manifest, region: 'auto', endpoint,
  auth: { get: ({ signal } = {}) => fetchViewerCredentials(signal) }
});
```

Return `TemporaryCredentialsAuthentication` including `expiresAt`. The storage
shares one pending credential acquisition across signing and document reads,
renews up to two minutes before expiry, and preserves manifest caches across
renewal. A provider returning expired credentials or no expiry is rejected.
Call `storage.dispose()` when its owning session ends; this aborts credential
acquisition and closes the S3 client. Static authentication remains supported.

### Discovery versus live coverage

`archive.json` is a day directory, cached for five minutes. The indexer writes it
only when registering a day (or changing its key), not for each recording. Its
existing per-day `startedAt`/`endedAt` fields are discovery-time hints retained in
the v2 wire format; current coverage comes from each day's head. Consumers must
use `describe()` rather than reading those root bounds as the live edge.

The root's `headPrefix` identifies deterministic `YYYY-MM-DD/head.json` paths.
The client probes today's head when that date is absent from cached discovery,
so UTC midnight and delayed first recordings do not wait for root expiry. A
missing current head is retried at most every five seconds. Registered day paths
still come from the directory; newly probed heads must match the stream and date.
`load()` includes newly discovered days, and live coverage can grow without any
root read or root rewrite.

Current and previous UTC day heads have five-second freshness. Older heads use
five-minute freshness when published by the worker; the demo gateway also ages
previously published heads into this policy. Late historical edits therefore
remain discoverable. Hashed pages and page directories remain immutable.
Refresh and thumbnail lookups respect Cache-Control, Date and Age independently
for each resource. Root changes such as historical backfills appear within its
five-minute discovery interval.
