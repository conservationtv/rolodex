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
> directory (`archive.json`), the daily manifests
> (`archive/YYYY-MM-DD.json`), per-recording indexes, canonical init
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
call `store.refresh()` periodically; it re-reads the manifests and
updates the timeline in place.

## Other controls

```ts
await store.play();
store.pause();
await store.toggle();
store.setPlaybackRate(2);   // clamped to 0.25–4
await store.seek(utc, { autoplay: true, signal });
```

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
