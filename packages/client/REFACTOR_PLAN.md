# Plan: split `src/index.ts` into modules

## Goal

`packages/client/src/index.ts` (~1,570 lines) contains the whole library. Split it
into cohesive modules with `index.ts` reduced to a barrel that re-exports the
public API. This is a **move-only refactor**: no behaviour changes, no renames,
no signature changes, no new dependencies. The published API (everything
currently exported from `index.ts`) must remain byte-for-byte compatible at the
type level.

## Non-negotiable constraints

1. **Public API is frozen.** After the split, `src/index.ts` must re-export
   exactly the same value exports (`S3ArchiveStorage`, `ArchiveManifestClient`,
   `PlaylistFactory`, `ArchivePlayerStore`) and the same type exports as today
   (see the `export type { ... }` block at the bottom of the current file).
   Consumers and `src/index.test.ts` import only from `./index.js`; the test
   file must not need changes beyond nothing at all.
2. **House lint rules apply to every new file** (`pnpm lint` runs oxlint with
   the `house` and `anti-slop` plugins). Two rules constrain the split:
   - `require-zod-output-type` pairs each zod schema with its
     `z.output<typeof …Schema>` type **in the same file**. So the schemas and
     their derived types (`DecimalString`, `ISODateString`, `DailyEntry`,
     `StreamArchiveManifest`, `DailyArchiveManifest`, `ArchiveRecording`,
     `ArchiveSegment`, `ThumbnailCell`, `ThumbnailTrack`) must move together.
   - `no-inline-object-type` / `no-conditional-empty-object-spread`: the code
     already complies; do not "simplify" while moving or you may reintroduce
     violations.
3. **No internals backdoor.** (Superseded: the original WeakMap backdoor —
   `manifestClientInternals` / `internalsOf` — was removed after this refactor.)
   `PlaylistFactory` and `ArchivePlayerStore` now use public
   `ArchiveManifestClient` methods instead: `describe()` for the stream
   identity and coverage, and `getVideo(key)` for media/init object URLs. Do
   not reintroduce shared mutable module state to bridge the classes.
   `package.json` only exposes the `"."` entry and tsdown bundles from the
   single `src/index.ts` entry, so non-barrel exports stay internal to the
   bundle — keep it that way (do not add entries to `exports` or
   `tsdown.config.ts`).
4. **TS config quirks to respect** (`tsconfig.json`): `verbatimModuleSyntax`
   (use `import type { … }` for type-only imports), `erasableSyntaxOnly` (no
   parameter properties/enums), `exactOptionalPropertyTypes`,
   `noUncheckedIndexedAccess`, `noUnusedLocals`. Every moved declaration must
   be imported where used or it will fail the build.

## Target layout

Dependency direction is strictly top-to-bottom; no cycles:

```
src/
  time.ts             ← nothing (leaf)
  manifest.ts         ← time
  storage.ts          ← manifest, time
  timeline.ts         ← manifest, time
  manifest-client.ts  ← storage, manifest, timeline, time
  playlist.ts         ← manifest-client, storage, timeline, manifest, time
  store.ts            ← playlist, manifest-client, timeline, manifest, time  (+ hls.js)
  index.ts            ← barrel only
```

### `src/time.ts` — time and duration helpers

Move (all currently near the top of `index.ts`):

- Constants: `NS_PER_MS`, `NS_PER_SECOND`, `MINIMUM_GAP_NS`,
  `LOCATE_PADDING_NS`, `SIGNED_URL_LIFETIME_SECONDS`,
  `MEDIA_RECOVERY_SKIP_NS`, `MEDIA_RECOVERY_COOLDOWN_MS`.
- Functions: `utcNs`, `isoFromNs`, `secondsFromNs`, `nsFromSeconds`,
  `decimalSeconds`.
- Types: `ISODateRange` (handwritten interface). Note `ISODateString` is
  zod-derived and must live in `manifest.ts` (see constraint 2); `time.ts` can
  accept plain `string` parameters or import the type from `manifest.ts` —
  **prefer plain `string` in signatures here to keep `time.ts` a leaf**, since
  `ISODateString` is just an alias of `string`. Alternatively move
  `IsoDateStringSchema` into `time.ts` with its type; either is acceptable as
  long as the schema/type pairing stays in one file and no cycle forms.
  `ISODateRange` references `ISODateString`; if `ISODateString` stays in
  `manifest.ts`, put `ISODateRange` there too rather than importing backwards.

### `src/manifest.ts` — wire format: zod schemas and derived types

Move:

- Schemas: `IsoDateStringSchema`, `DecimalStringSchema`, `DailyEntrySchema`,
  `StreamArchiveManifestSchema`, `ArchiveSegmentSchema`, `ThumbnailCellSchema`,
  `ThumbnailTrackSchema`, `ArchiveRecordingSchema`,
  `DailyArchiveManifestSchema`.
- Derived types (same file, per lint rule): `ISODateString`, `DecimalString`,
  `DailyEntry`, `StreamArchiveManifest`, `DailyArchiveManifest`,
  `ArchiveRecording`, `ArchiveSegment`, `ThumbnailCell`, `ThumbnailTrack`.
- `ISODateRange` (see note above).

Preserve schema declaration order (they reference each other at module
evaluation time).

### `src/storage.ts` — object access and authentication

Move:

- Types: `CredentialsAuthentication`, `TemporaryCredentialsAuthentication`,
  `ArchiveAuthentication`, `ArchiveSource`, `Source`, `ArchiveStorage`,
  `RequestOptions`, `SigningCredentials`.
- Functions: `normalizeKey`, `signingCredentials`.
- Class: `S3ArchiveStorage` (imports `GetObjectCommand`/`S3Client`,
  `getSignedUrl`, the two manifest schemas, `SIGNED_URL_LIFETIME_SECONDS`).

`RequestOptions` is used by nearly every module; it lives here because
`ArchiveStorage` defines the contract. (If a cycle appears, `RequestOptions`
may move to `time.ts` instead — it has no dependencies.)

### `src/timeline.ts` — pure timeline math

Move:

- Types: `RecordingTimelineSpan`, `GapTimelineSpan`, `TimelineSpan`,
  `ArchiveTimeline`, `TimelineLocation`, `ThumbnailCrop`, `ThumbnailCue`,
  `ResolvedObjectUrl`, `NormalizedRecording`.
- Functions: `normalizeRecording`, `buildSpans`, `precedingSegmentIndex`,
  `firstSegmentAtOrAfter`, `locateWithin`.

Everything here is pure and synchronous — this is also the natural place to
add unit tests later.

### `src/manifest-client.ts` — caching client

Move:

- Types: `ArchiveManifestClientOptions`, `CachedFetch`,
  `RecordingsAroundResult`.
- Class: `ArchiveManifestClient`.

(The `internalsOf` WeakMap backdoor that once lived here was later removed;
`playlist.ts` and `store.ts` use the public `describe()` and `getVideo()`
methods instead.)

### `src/playlist.ts` — virtual HLS compilation

Move:

- Types: `PlaylistWindow`, `PlaylistWindowRequest`, `PlaylistLease`,
  `CompiledPlaylist`, `PlaylistFactoryOptions`, `VirtualSegment`,
  `CompiledRecording`, `ResolvedRecording`, `PlaylistPiece`.
- Class: `PlaylistFactory`.

### `src/store.ts` — player state

Move:

- Types: `PlaybackState`, `ViewportState`, `HoverState`, `ActivePlaylist`,
  `ArchivePlayerSnapshotBase`, `LoadedArchivePlayerFields`,
  `LoadedArchivePlayerSnapshotBase`, the eight phase snapshot interfaces
  (`IdleArchivePlayerSnapshot` … `ErrorArchivePlayerSnapshot`),
  `ArchivePlayerSnapshot`, `ArchivePlayerPhase`,
  `MediaBoundArchivePlayerSnapshot`, `ArchivePlayerInitialState`,
  `ArchivePlayerStoreOptions`, `ArchivePlayerInitializeOptions`,
  `ArchivePlayerSeekOptions`.
- Functions: `isMediaBound`, `carryBase`, `carryLoaded`.
- Class: `ArchivePlayerStore` (imports `Hls` and `type ErrorData` from
  `hls.js`).

### `src/index.ts` — barrel

Only re-exports; no logic:

```ts
export { S3ArchiveStorage } from "./storage.js";
export { ArchiveManifestClient } from "./manifest-client.js";
export { PlaylistFactory } from "./playlist.js";
export { ArchivePlayerStore } from "./store.js";
export type { /* identical list to today's export type block */ } from "./…";
```

Use explicit `.js` extensions in relative imports throughout (ESM,
`moduleResolution: Bundler` tolerates either, but match the existing test-file
convention of `./index.js`). Do **not** re-export `CachedFetch`,
`RecordingsAroundResult`, `NormalizedRecording`, `VirtualSegment`,
`CompiledRecording`, `ResolvedRecording`, `PlaylistPiece`, `SigningCredentials`,
`carryBase`, `carryLoaded`, `isMediaBound`, or any schema constants — these are
internal. (Exception: if you find a schema constant is useful publicly, leave
that decision out of this refactor.)

## Execution order

1. Create `time.ts` and `manifest.ts` first (leaves), moving declarations
   verbatim; fix imports in `index.ts` temporarily so the build stays green.
2. Extract `storage.ts`, then `timeline.ts`, then `manifest-client.ts`, then
   `playlist.ts`, then `store.ts`, running `pnpm typecheck` after each step.
3. Reduce `index.ts` to the barrel last.
4. Delete nothing until its new home compiles; prefer `git mv`-style single
   commits per module if committing incrementally (only commit if asked).

## Verification (acceptance criteria)

Run from `packages/client`, all must pass:

1. `pnpm lint` — zero errors/warnings (watch for `require-zod-output-type`
   firing if a schema and its type were separated).
2. `pnpm typecheck`.
3. `pnpm test` — all 16 tests pass **without modifying the test file**.
4. `pnpm build` — then diff the public surface: `git diff --no-index` the
   previous `dist/index.d.ts` against the new one (build once on the base
   commit and stash the file first). Ordering may shift; the set of exported
   declarations and their shapes must not.
5. Grep check that no internal symbol leaks: `rg "internalsOf|carryBase|carryLoaded" dist/index.d.ts` returns nothing.

## Known traps

- `S3ArchiveStorage.getObject` uses `z.ZodType<Output>` — needs `z` imported in
  `storage.ts` even though schemas live in `manifest.ts`.
- `ArchiveManifestClient`'s constructor registers itself in the WeakMap; keep
  registration and lookup in the same module to avoid an import cycle between
  `manifest-client.ts` and `playlist.ts`/`store.ts` (the dependency must point
  playlist/store → manifest-client only).
- `store.ts` reads `MEDIA_RECOVERY_SKIP_NS`/`MEDIA_RECOVERY_COOLDOWN_MS` and
  `LOCATE_PADDING_NS` lives in `manifest-client.ts`'s logic — both are
  currently top-of-file constants; put them in `time.ts` and import.
- `PlaylistLease` uses `[Symbol.dispose]`; `ESNext.Disposable` is already in
  tsconfig `lib` — no change needed, just don't drop it.
- Comments matter: the block comments explaining the presign strategy, the
  internals backdoor, the discontinuity rule, and damaged-media recovery should
  move with their code.
