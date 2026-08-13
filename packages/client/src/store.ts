import Hls, { type ErrorData } from "hls.js";

import { GapClock } from "./gap-clock.js";
import { ArchiveManifestClient, internalsOf } from "./manifest-client.js";
import type { ISODateRange, ISODateString } from "./manifest.js";
import { PlaylistFactory, type PlaylistLease } from "./playlist.js";
import {
  firstSegmentAtOrAfter,
  locateWithin,
  type ArchiveTimeline,
  type ThumbnailCue,
  type TimelineLocation,
} from "./timeline.js";
import {
  MEDIA_RECOVERY_COOLDOWN_MS,
  MEDIA_RECOVERY_SKIP_NS,
  isoFromNs,
  utcNs,
} from "./time.js";

/* 3. UI/player state — directly compatible with useSyncExternalStore */

export interface PlaybackState {
  playing: boolean;
  rate: number;
  mediaTime: number;
  bufferedSeconds: number;
}

export interface ViewportState {
  startedAt?: ISODateString;
  endedAt?: ISODateString;
}

export interface HoverState {
  playhead: ISODateString;
  thumbnail?: ThumbnailCue;
  loading: boolean;
}

export interface ActivePlaylist {
  url: string;
  startedAt: ISODateString;
  endedAt: ISODateString;
}

/** Fields present in every phase. */
export interface ArchivePlayerSnapshotBase {
  revision: number;
  playback: PlaybackState;
  viewport: ViewportState;
  playhead?: ISODateString;
  hover?: HoverState;
}

/** Fields that exist once the stream manifest has loaded. */
export interface LoadedArchivePlayerFields {
  stream: string;
  coverage?: ISODateRange;
  timeline?: ArchiveTimeline;
  location?: TimelineLocation;
  active?: ActivePlaylist;
}

export interface LoadedArchivePlayerSnapshotBase extends ArchivePlayerSnapshotBase, LoadedArchivePlayerFields {}

export interface IdleArchivePlayerSnapshot extends ArchivePlayerSnapshotBase {
  phase: "idle";
}

export interface LoadingManifestArchivePlayerSnapshot extends ArchivePlayerSnapshotBase {
  phase: "loading-manifest";
}

export interface ReadyArchivePlayerSnapshot extends LoadedArchivePlayerSnapshotBase {
  phase: "ready";
}

export interface LoadingMediaArchivePlayerSnapshot extends LoadedArchivePlayerSnapshotBase {
  phase: "loading-media";
}

export interface PlayingArchivePlayerSnapshot extends LoadedArchivePlayerSnapshotBase {
  phase: "playing";
}

export interface PausedArchivePlayerSnapshot extends LoadedArchivePlayerSnapshotBase {
  phase: "paused";
}

export interface GapArchivePlayerSnapshot extends LoadedArchivePlayerSnapshotBase {
  phase: "gap";
  location: TimelineLocation;
}

/** Errors can strike before or after the manifest loads, so the loaded
 * fields stay optional here. */
export interface ErrorArchivePlayerSnapshot extends ArchivePlayerSnapshotBase {
  phase: "error";
  error: Error;
  stream?: string;
  coverage?: ISODateRange;
  timeline?: ArchiveTimeline;
  location?: TimelineLocation;
  active?: ActivePlaylist;
}

export type ArchivePlayerSnapshot =
  | IdleArchivePlayerSnapshot
  | LoadingManifestArchivePlayerSnapshot
  | ReadyArchivePlayerSnapshot
  | LoadingMediaArchivePlayerSnapshot
  | PlayingArchivePlayerSnapshot
  | PausedArchivePlayerSnapshot
  | GapArchivePlayerSnapshot
  | ErrorArchivePlayerSnapshot;

export type ArchivePlayerPhase = ArchivePlayerSnapshot["phase"];

/** Phases in which compiled media can be bound to the element. */
export type MediaBoundArchivePlayerSnapshot =
  | ReadyArchivePlayerSnapshot
  | LoadingMediaArchivePlayerSnapshot
  | PlayingArchivePlayerSnapshot
  | PausedArchivePlayerSnapshot;

export function isMediaBound(
  snapshot: ArchivePlayerSnapshot,
): snapshot is MediaBoundArchivePlayerSnapshot {
  return snapshot.phase === "ready"
    || snapshot.phase === "loading-media"
    || snapshot.phase === "playing"
    || snapshot.phase === "paused";
}

/** Phase-independent fields, carried across every transition. */
export function carryBase(snapshot: ArchivePlayerSnapshot): ArchivePlayerSnapshotBase {
  const base: ArchivePlayerSnapshotBase = {
    revision: snapshot.revision,
    playback: snapshot.playback,
    viewport: snapshot.viewport,
  };
  if (snapshot.playhead !== undefined) base.playhead = snapshot.playhead;
  if (snapshot.hover !== undefined) base.hover = snapshot.hover;
  return base;
}

/** Loaded fields when the snapshot has them; spreading the undefined result
 * into a transition is a no-op for pre-manifest phases. */
export function carryLoaded(
  snapshot: ArchivePlayerSnapshot,
): LoadedArchivePlayerFields | undefined {
  if (snapshot.phase === "idle" || snapshot.phase === "loading-manifest") return undefined;
  const stream = snapshot.stream;
  if (stream === undefined) return undefined;
  const fields: LoadedArchivePlayerFields = { stream };
  if (snapshot.coverage !== undefined) fields.coverage = snapshot.coverage;
  if (snapshot.timeline !== undefined) fields.timeline = snapshot.timeline;
  if (snapshot.location !== undefined) fields.location = snapshot.location;
  if (snapshot.active !== undefined) fields.active = snapshot.active;
  return fields;
}

export interface ArchivePlayerInitialState {
  playhead: ISODateString;
  viewport: ISODateRange;
}

export interface ArchivePlayerStoreOptions {
  manifests: ArchiveManifestClient;
  playlists?: PlaylistFactory;
  initial: ArchivePlayerInitialState;
}

export interface ArchivePlayerInitializeOptions {
  range?: ISODateRange;
  signal?: AbortSignal;
}

export interface ArchivePlayerSeekOptions {
  autoplay?: boolean;
  signal?: AbortSignal;
}

export class ArchivePlayerStore {
  private manifests: ArchiveManifestClient;
  private playlists: PlaylistFactory;
  private ownsPlaylists: boolean;
  private listeners = new Set<() => void>();
  private snapshot: ArchivePlayerSnapshot;
  private serverSnapshot: ArchivePlayerSnapshot;
  private element: HTMLMediaElement | undefined;
  private detachElement: (() => void) | undefined;
  private lease: PlaylistLease | undefined;
  private pendingMediaTime: number | undefined;
  private engine: Hls | undefined;
  private gapClock: GapClock;
  private playbackIntent = false;
  private lastMediaRecoveryAt = Number.NEGATIVE_INFINITY;
  private seekToken = 0;
  private hoverToken = 0;
  private disposed = false;

  /* Stable function identities for React.useSyncExternalStore */
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): ArchivePlayerSnapshot => this.snapshot;
  getServerSnapshot = (): ArchivePlayerSnapshot => this.serverSnapshot;

  constructor(options: ArchivePlayerStoreOptions) {
    this.manifests = options.manifests;
    this.playlists = options.playlists ?? new PlaylistFactory(options.manifests);
    this.ownsPlaylists = options.playlists === undefined;
    this.snapshot = {
      revision: 0,
      phase: "idle",
      playhead: options.initial.playhead,
      playback: { playing: false, rate: 1, mediaTime: 0, bufferedSeconds: 0 },
      viewport: {
        startedAt: options.initial.viewport.startedAt,
        endedAt: options.initial.viewport.endedAt,
      },
    };
    this.serverSnapshot = this.snapshot;
    this.gapClock = new GapClock(
      (playheadMs) => this.syncGapPlayhead(playheadMs),
      () => this.handleGapEnded(),
    );
  }

  async initialize(options?: ArchivePlayerInitializeOptions): Promise<void> {
    const requestOptions = options?.signal ? { signal: options.signal } : undefined;
    this.mutate((snapshot) => ({ ...carryBase(snapshot), phase: "loading-manifest" }));
    try {
      const manifest = await internalsOf(this.manifests).getManifest(requestOptions);
      let coverage: ISODateRange | undefined;
      for (const day of Object.values(manifest.days)) {
        coverage = coverage === undefined
          ? { startedAt: day.startedAt, endedAt: day.endedAt }
          : {
              startedAt: utcNs(day.startedAt) < utcNs(coverage.startedAt) ? day.startedAt : coverage.startedAt,
              endedAt: utcNs(day.endedAt) > utcNs(coverage.endedAt) ? day.endedAt : coverage.endedAt,
            };
      }
      const viewport = this.snapshot.viewport;
      const range = options?.range
        ?? (viewport.startedAt && viewport.endedAt
          ? { startedAt: viewport.startedAt, endedAt: viewport.endedAt }
          : coverage);
      const timeline = range ? await this.manifests.getTimeline(range, requestOptions) : undefined;
      const playhead = this.snapshot.playhead;
      const location = playhead
        ? await this.manifests.locate(playhead, requestOptions)
        : undefined;
      this.mutate((snapshot) => {
        const next: ReadyArchivePlayerSnapshot = {
          ...carryBase(snapshot),
          phase: "ready",
          stream: manifest.stream,
          viewport: range
            ? { startedAt: range.startedAt, endedAt: range.endedAt }
            : snapshot.viewport,
        };
        if (coverage) next.coverage = coverage;
        if (timeline) next.timeline = timeline;
        if (location) next.location = location;
        return next;
      });
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)));
    }
  }

  attach(element: HTMLMediaElement): () => void {
    this.detachElement?.();
    this.element = element;
    element.playbackRate = this.snapshot.playback.rate;

    const onTimeUpdate = (): void => this.syncFromElement(element);
    const onPlay = (): void => {
      this.playbackIntent = true;
      this.mutate((snapshot) => {
        const playback = { ...snapshot.playback, playing: true };
        if (isMediaBound(snapshot)) return { ...snapshot, phase: "playing", playback };
        return { ...snapshot, playback };
      });
    };
    const onPause = (): void => this.mutate((snapshot) => {
      const playback = { ...snapshot.playback, playing: false };
      if (snapshot.phase === "playing") return { ...snapshot, phase: "paused", playback };
      return { ...snapshot, playback };
    });
    const onRateChange = (): void => this.mutate((snapshot) => ({
      ...snapshot,
      playback: { ...snapshot.playback, rate: element.playbackRate },
    }));
    const onError = (): void => {
      const mediaError = element.error;
      const cause = new Error(mediaError?.message || `Media error ${mediaError?.code ?? "unknown"}`);
      void this.recoverFromDamagedMedia(false).then((recovered) => {
        if (!recovered) this.fail(cause);
      });
    };
    const onEnded = (): void => {
      void this.handleEnded();
    };

    element.addEventListener("timeupdate", onTimeUpdate);
    element.addEventListener("progress", onTimeUpdate);
    element.addEventListener("play", onPlay);
    element.addEventListener("pause", onPause);
    element.addEventListener("ratechange", onRateChange);
    element.addEventListener("error", onError);
    element.addEventListener("ended", onEnded);

    /* Late attach: bind the playlist that an earlier seek already compiled. */
    if (this.lease) this.bindLease(element, this.lease);

    const detach = (): void => {
      this.destroyEngine();
      element.removeEventListener("timeupdate", onTimeUpdate);
      element.removeEventListener("progress", onTimeUpdate);
      element.removeEventListener("play", onPlay);
      element.removeEventListener("pause", onPause);
      element.removeEventListener("ratechange", onRateChange);
      element.removeEventListener("error", onError);
      element.removeEventListener("ended", onEnded);
      if (this.element === element) this.element = undefined;
      if (this.detachElement === detach) this.detachElement = undefined;
    };
    this.detachElement = detach;
    return detach;
  }

  async seek(
    to: ISODateString,
    options?: ArchivePlayerSeekOptions,
  ): Promise<void> {
    return this.seekTo(to, options, true);
  }

  private async seekTo(
    to: ISODateString,
    options: ArchivePlayerSeekOptions | undefined,
    allowReuse: boolean,
  ): Promise<void> {
    this.gapClock.stop();
    if (options?.autoplay !== undefined) this.playbackIntent = options.autoplay;
    const token = ++this.seekToken;
    const requestOptions = options?.signal ? { signal: options.signal } : undefined;
    try {
      /* The manifest is required (and cached) so every post-manifest phase
       * can carry the stream identity. */
      const manifest = await internalsOf(this.manifests).getManifest(requestOptions);
      if (token !== this.seekToken) return;
      const location = await this.manifests.locate(to, requestOptions);
      if (token !== this.seekToken) return;
      if (location.span.kind === "gap") {
        const autoplay = options?.autoplay ?? this.playbackIntent;
        this.enterGap(to, location, manifest.stream, autoplay);
        return;
      }

      const currentLease = this.lease;
      const targetNs = utcNs(to);
      if (
        allowReuse
        && currentLease
        && targetNs >= utcNs(currentLease.startedAt)
        && targetNs < utcNs(currentLease.endedAt)
      ) {
        const mediaTime = currentLease.mediaTimeAt(to);
        this.pendingMediaTime = mediaTime;
        const element = this.element;
        const autoplay = options?.autoplay ?? this.playbackIntent;
        this.playbackIntent = autoplay;
        if (element) element.currentTime = mediaTime;
        this.mutate((snapshot) => ({
          ...carryBase(snapshot),
          ...carryLoaded(snapshot),
          phase: element ? (autoplay ? "playing" : "paused") : "ready",
          stream: manifest.stream,
          playhead: to,
          location,
          playback: {
            ...snapshot.playback,
            playing: element ? autoplay : snapshot.playback.playing,
            mediaTime,
          },
        }));
        if (!element) return;
        if (autoplay) {
          await element.play().catch(() => {
            if (token !== this.seekToken) return;
            this.playbackIntent = false;
            this.mutate((snapshot) => ({
              ...carryBase(snapshot),
              ...carryLoaded(snapshot),
              phase: "paused",
              stream: manifest.stream,
              playhead: to,
              location,
              playback: { ...snapshot.playback, playing: false },
            }));
          });
        } else {
          element.pause();
        }
        return;
      }

      this.mutate((snapshot) => ({
        ...carryBase(snapshot),
        ...carryLoaded(snapshot),
        phase: "loading-media",
        stream: manifest.stream,
        playhead: to,
      }));
      const lease = await this.playlists.create({ at: to });
      if (token !== this.seekToken) {
        lease.revoke();
        return;
      }
      /* Tear down the previous engine before revoking the blob URL it loaded. */
      this.destroyEngine();
      this.lease?.revoke();
      this.lease = lease;
      this.pendingMediaTime = lease.initial;
      this.mutate((snapshot) => ({
        ...carryBase(snapshot),
        ...carryLoaded(snapshot),
        phase: "ready",
        stream: manifest.stream,
        location,
        active: { url: lease.url, startedAt: lease.startedAt, endedAt: lease.endedAt },
      }));

      const element = this.element;
      if (!element) return;
      const autoplay = options?.autoplay ?? this.playbackIntent;
      this.playbackIntent = autoplay;
      this.bindLease(element, lease);
      if (autoplay) {
        await element.play().catch(() => {
          /* Blocked autoplay is not a seek failure. */
        });
      }
    } catch (error) {
      if (token === this.seekToken) {
        this.fail(error instanceof Error ? error : new Error(String(error)));
      }
    }
  }

  async play(): Promise<void> {
    this.playbackIntent = true;
    if (this.snapshot.phase === "gap") {
      this.mutate((snapshot) => ({
        ...snapshot,
        playback: { ...snapshot.playback, playing: true },
      }));
      this.gapClock.play();
      return;
    }
    this.mutate((snapshot) => {
      const playback = { ...snapshot.playback, playing: true };
      if (isMediaBound(snapshot)) return { ...snapshot, phase: "playing", playback };
      return { ...snapshot, playback };
    });
    const element = this.element;
    if (!element) return;
    try {
      await element.play();
    } catch {
      this.playbackIntent = false;
      this.mutate((snapshot) => {
        const playback = { ...snapshot.playback, playing: false };
        if (snapshot.phase === "playing") return { ...snapshot, phase: "paused", playback };
        return { ...snapshot, playback };
      });
    }
  }

  pause(): void {
    this.playbackIntent = false;
    if (this.snapshot.phase === "gap") {
      this.gapClock.pause();
      this.mutate((snapshot) => ({
        ...snapshot,
        playback: { ...snapshot.playback, playing: false },
      }));
      return;
    }
    this.element?.pause();
    this.mutate((snapshot) => {
      const playback = { ...snapshot.playback, playing: false };
      if (snapshot.phase === "playing") return { ...snapshot, phase: "paused", playback };
      return { ...snapshot, playback };
    });
  }

  async toggle(): Promise<void> {
    if (this.snapshot.playback.playing) this.pause();
    else await this.play();
  }

  setPlaybackRate(rate: number): void {
    const bounded = Math.min(4, Math.max(0.25, rate));
    this.gapClock.setRate(bounded);
    if (this.element) this.element.playbackRate = bounded;
    this.mutate((snapshot) => ({
      ...snapshot,
      playback: { ...snapshot.playback, rate: bounded },
    }));
  }

  zoomAt(utc: ISODateString, factor: number): void {
    const viewport = this.viewportRange();
    if (!viewport || !(factor > 0)) return;
    const anchorMs = Date.parse(utc);
    const startMs = Date.parse(viewport.startedAt);
    const endMs = Date.parse(viewport.endedAt);
    if (![anchorMs, startMs, endMs].every(Number.isFinite)) return;
    const nextStartMs = anchorMs - (anchorMs - startMs) / factor;
    const nextEndMs = anchorMs + (endMs - anchorMs) / factor;
    if (nextEndMs <= nextStartMs) return;
    this.mutate((snapshot) => ({
      ...snapshot,
      viewport: {
        startedAt: new Date(nextStartMs).toISOString(),
        endedAt: new Date(nextEndMs).toISOString(),
      },
    }));
  }

  panBy(seconds: number): void {
    const viewport = this.viewportRange();
    if (!viewport || !Number.isFinite(seconds)) return;
    const deltaMs = seconds * 1000;
    this.mutate((snapshot) => ({
      ...snapshot,
      viewport: {
        startedAt: new Date(Date.parse(viewport.startedAt) + deltaMs).toISOString(),
        endedAt: new Date(Date.parse(viewport.endedAt) + deltaMs).toISOString(),
      },
    }));
  }

  fit(range?: ISODateRange): void {
    const target = range ?? carryLoaded(this.snapshot)?.coverage;
    if (!target) return;
    this.mutate((snapshot) => ({
      ...snapshot,
      viewport: { startedAt: target.startedAt, endedAt: target.endedAt },
    }));
  }

  setHover(utc: ISODateString, _x: number): void {
    const token = ++this.hoverToken;
    this.mutate((snapshot) => {
      const hover: HoverState = { playhead: utc, loading: true };
      /* Keep the previous sprite visible while the next cue loads. */
      if (snapshot.hover?.thumbnail) hover.thumbnail = snapshot.hover.thumbnail;
      return { ...snapshot, hover };
    });
    void this.manifests.getThumbnail(utc)
      .then((thumbnail) => {
        if (token !== this.hoverToken) return;
        this.mutate((snapshot) => {
          const hover: HoverState = { playhead: utc, loading: false };
          if (thumbnail) hover.thumbnail = thumbnail;
          return { ...snapshot, hover };
        });
      })
      .catch(() => {
        if (token !== this.hoverToken) return;
        this.mutate((snapshot) => ({
          ...snapshot,
          hover: { playhead: utc, loading: false },
        }));
      });
  }

  clearHover(): void {
    this.hoverToken += 1;
    if (!this.snapshot.hover) return;
    this.mutate(({ hover: _hover, ...snapshot }) => snapshot);
  }

  async refresh(): Promise<void> {
    try {
      await this.manifests.refresh();
      const range = this.viewportRange();
      const timeline = range ? await this.manifests.getTimeline(range) : undefined;
      const playhead = this.snapshot.playhead;
      const location = playhead ? await this.manifests.locate(playhead) : undefined;
      this.mutate((snapshot) => {
        /* Pre-manifest phases have nowhere to store timeline data. */
        if (snapshot.phase === "idle" || snapshot.phase === "loading-manifest") return snapshot;
        const next = { ...snapshot };
        if (timeline) next.timeline = timeline;
        if (location) next.location = location;
        return next;
      });
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)));
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.seekToken += 1;
    this.hoverToken += 1;
    this.gapClock.stop();
    this.detachElement?.();
    this.destroyEngine();
    this.lease?.revoke();
    this.lease = undefined;
    if (this.ownsPlaylists) this.playlists.dispose();
    this.listeners.clear();
  }

  /* hls.js is used unconditionally — even where native HLS exists — so every
   * browser exercises the same MSE code path and the blob playlist, byte
   * ranges, and discontinuities behave identically everywhere. */
  private bindLease(element: HTMLMediaElement, lease: PlaylistLease): void {
    this.destroyEngine();
    if (!Hls.isSupported()) {
      this.fail(new Error("hls.js requires Media Source Extensions, which this environment does not provide"));
      return;
    }
    const engine = new Hls({ startPosition: this.pendingMediaTime ?? lease.initial });
    this.engine = engine;
    engine.on(Hls.Events.ERROR, (_event, data: ErrorData) => {
      if (this.engine !== engine || !data.fatal) return;
      const cause = data.error instanceof Error ? data.error : new Error(data.details);
      const missingSource = data.response?.code === 404 || data.response?.code === 410;
      if (data.type !== Hls.ErrorTypes.MEDIA_ERROR && !missingSource) {
        this.fail(cause);
        return;
      }
      void this.recoverFromDamagedMedia(missingSource).then((recovered) => {
        if (!recovered) this.fail(cause);
      });
    });
    engine.loadSource(lease.url);
    engine.attachMedia(element);
    element.playbackRate = this.snapshot.playback.rate;
  }

  private destroyEngine(): void {
    const engine = this.engine;
    this.engine = undefined;
    engine?.destroy();
  }

  private enterGap(
    playhead: ISODateString,
    location: TimelineLocation,
    stream: string,
    autoplay: boolean,
  ): void {
    this.gapClock.stop();
    this.playbackIntent = autoplay;
    this.element?.pause();
    this.destroyEngine();
    this.lease?.revoke();
    this.lease = undefined;
    this.pendingMediaTime = undefined;
    this.mutate((snapshot) => {
      const loaded = carryLoaded(snapshot);
      const next: GapArchivePlayerSnapshot = {
        ...carryBase(snapshot),
        phase: "gap",
        stream,
        playhead,
        location,
        playback: {
          ...snapshot.playback,
          playing: false,
          mediaTime: 0,
          bufferedSeconds: 0,
        },
      };
      if (loaded?.coverage !== undefined) next.coverage = loaded.coverage;
      if (loaded?.timeline !== undefined) next.timeline = loaded.timeline;
      return next;
    });
    this.gapClock.configure(
      Date.parse(playhead),
      Date.parse(location.span.endedAt),
      this.snapshot.playback.rate,
    );
    if (autoplay) {
      this.mutate((snapshot) => ({
        ...snapshot,
        playback: { ...snapshot.playback, playing: true },
      }));
      this.gapClock.play();
    }
  }

  private syncGapPlayhead(playheadMs: number): void {
    if (this.snapshot.phase !== "gap") return;
    const playhead = new Date(playheadMs).toISOString();
    this.mutate((snapshot) => {
      if (snapshot.phase !== "gap") return snapshot;
      return {
        ...snapshot,
        playhead,
        location: { ...snapshot.location, utc: playhead },
      };
    });
  }

  private handleGapEnded(): void {
    if (this.snapshot.phase !== "gap") return;
    const boundary = this.snapshot.location.span.endedAt;
    void this.seekTo(boundary, { autoplay: this.playbackIntent }, false);
  }

  private async handleEnded(): Promise<void> {
    const lease = this.lease;
    if (!lease) {
      this.playbackIntent = false;
      this.mutate((snapshot) => {
        const playback = { ...snapshot.playback, playing: false };
        if (isMediaBound(snapshot)) return { ...snapshot, phase: "paused", playback };
        return { ...snapshot, playback };
      });
      return;
    }

    const token = ++this.seekToken;
    const boundary = lease.endedAt;
    const autoplay = this.playbackIntent;
    const loaded = carryLoaded(this.snapshot);
    if (
      loaded !== undefined
      && loaded.coverage !== undefined
      && utcNs(boundary) >= utcNs(loaded.coverage.endedAt)
    ) {
      this.mutate((snapshot) => {
        const next: PausedArchivePlayerSnapshot = {
          ...carryBase(snapshot),
          ...loaded,
          phase: "paused",
          playhead: boundary,
          playback: { ...snapshot.playback, playing: false },
        };
        return next;
      });
      this.playbackIntent = false;
      return;
    }

    try {
      const location = await this.manifests.locate(boundary);
      if (this.disposed || token !== this.seekToken) return;
      if (location.span.kind === "gap") {
        const stream = carryLoaded(this.snapshot)?.stream;
        if (stream !== undefined) this.enterGap(boundary, location, stream, autoplay);
        return;
      }
      await this.seekTo(boundary, { autoplay }, false);
    } catch (error) {
      if (token === this.seekToken) {
        this.fail(error instanceof Error ? error : new Error(String(error)));
      }
    }
  }

  /** Skips damaged or missing media to the next random-access point: the
   * next segment of the failed recording, or the following recording. The
   * returned promise resolves false when there is nowhere left to seek. */
  private async recoverFromDamagedMedia(missingSource: boolean): Promise<boolean> {
    if (this.disposed) return false;
    const now = Date.now();
    /* Swallow the duplicate errors that accompany one damaged part. */
    if (now - this.lastMediaRecoveryAt < MEDIA_RECOVERY_COOLDOWN_MS) return true;

    const lease = this.lease;
    const element = this.element;
    const failedUtc = lease && element ? lease.utcAt(element.currentTime) : this.snapshot.playhead;
    if (!failedUtc) return false;

    const recovery = await this.nextRecoveryPoint(failedUtc, missingSource);
    if (recovery === undefined) return false;
    this.lastMediaRecoveryAt = now;
    void this.seekTo(recovery, { autoplay: this.snapshot.playback.playing }, false);
    return true;
  }

  private async nextRecoveryPoint(
    failedUtc: ISODateString,
    missingSource: boolean,
  ): Promise<ISODateString | undefined> {
    const location = await this.manifests.locate(failedUtc);
    if (location.recording && !missingSource) {
      /* The next random-access segment strictly after the failed sample. */
      const afterNs = (location.recordingOffsetNs ?? 0n) + MEDIA_RECOVERY_SKIP_NS;
      const segments = location.recording.segments;
      const nextIndex = firstSegmentAtOrAfter(segments, afterNs + 1n);
      const next = segments[nextIndex];
      if (next) {
        return isoFromNs(utcNs(location.recording.startedAt) + next.presentationStartNs);
      }
    }
    /* Otherwise step across span boundaries until a recording begins. Two
     * hops cover "end of recording, then gap, then next recording". */
    let boundary = location.span.endedAt;
    for (let hop = 0; hop < 2; hop++) {
      const candidate = await this.manifests.locate(boundary);
      if (candidate.span.kind === "recording") return boundary;
      if (candidate.span.endedAt === boundary) break;
      boundary = candidate.span.endedAt;
    }
    return undefined;
  }

  private mutate(update: (previous: ArchivePlayerSnapshot) => ArchivePlayerSnapshot): void {
    const previous = this.snapshot;
    this.snapshot = { ...update(previous), revision: previous.revision + 1 };
    for (const listener of this.listeners) listener();
  }

  private fail(error: Error): void {
    this.mutate((snapshot) => ({
      ...carryBase(snapshot),
      ...carryLoaded(snapshot),
      phase: "error",
      error,
    }));
  }

  private viewportRange(): ISODateRange | undefined {
    const { startedAt, endedAt } = this.snapshot.viewport;
    if (startedAt && endedAt) return { startedAt, endedAt };
    return carryLoaded(this.snapshot)?.coverage;
  }

  private syncFromElement(element: HTMLMediaElement): void {
    const lease = this.lease;
    const mediaTime = element.currentTime;
    let bufferedSeconds = 0;
    for (let index = 0; index < element.buffered.length; index++) {
      if (mediaTime >= element.buffered.start(index) && mediaTime <= element.buffered.end(index)) {
        bufferedSeconds = element.buffered.end(index) - mediaTime;
        break;
      }
    }
    const timeline = carryLoaded(this.snapshot)?.timeline;
    const playhead = lease ? lease.utcAt(mediaTime) : this.snapshot.playhead;
    const location = playhead && timeline ? locateWithin(timeline.spans, playhead) : undefined;
    this.mutate((snapshot) => {
      const playback = { ...snapshot.playback, mediaTime, bufferedSeconds };
      if (isMediaBound(snapshot)) {
        const next = { ...snapshot, playback };
        if (playhead) next.playhead = playhead;
        if (location) next.location = location;
        return next;
      }
      const next = { ...snapshot, playback };
      if (playhead) next.playhead = playhead;
      return next;
    });
  }
}
