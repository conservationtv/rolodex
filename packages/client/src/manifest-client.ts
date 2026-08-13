import type {
  DailyArchiveManifest,
  DailyEntry,
  ISODateRange,
  ISODateString,
  StreamArchiveManifest,
} from "./manifest.js";
import type { ArchiveStorage, RequestOptions } from "./storage.js";
import {
  buildSpans,
  locateWithin,
  normalizeRecording,
  type ArchiveTimeline,
  type NormalizedRecording,
  type ThumbnailCue,
  type TimelineLocation,
} from "./timeline.js";
import {
  LOCATE_PADDING_NS,
  SIGNED_URL_LIFETIME_SECONDS,
  isoFromNs,
  utcNs,
} from "./time.js";

/* Same-module backdoor so PlaylistFactory and ArchivePlayerStore can reach
 * storage and the raw stream manifest without widening the public class. */
export interface ArchiveManifestClientInternals {
  storage: ArchiveStorage;
  getManifest(options?: RequestOptions): Promise<StreamArchiveManifest>;
}

export const manifestClientInternals = new WeakMap<
  ArchiveManifestClient,
  ArchiveManifestClientInternals
>();

export function internalsOf(manifests: ArchiveManifestClient): ArchiveManifestClientInternals {
  const internals = manifestClientInternals.get(manifests);
  if (!internals) throw new Error("Unknown ArchiveManifestClient instance");
  return internals;
}

/* 1. Manifest discovery, normalization, caching and authentication */

export interface ArchiveManifestClientOptions {
  maxAge?: number; // ms
  maximumDailyManifests?: number;
  /** Maximum number of presigned thumbnail sprite URLs retained in memory. */
  maximumThumbnailUrls?: number;
}

export interface CachedFetch<T> {
  value: Promise<T>;
  fetchedAt: number;
}

interface CachedThumbnailUrl {
  value?: Promise<ThumbnailCue["image"]>;
  expiresAtMs: number;
}

const THUMBNAIL_URL_REFRESH_MARGIN_MS = 30_000;

export interface RecordingsAroundResult {
  recordings: NormalizedRecording[];
  availableRange?: ISODateRange;
}

export class ArchiveManifestClient {
  private storage: ArchiveStorage;
  private maxAge: number;
  private maximumDailyManifests: number;
  private maximumThumbnailUrls: number;
  private manifest: CachedFetch<StreamArchiveManifest> | undefined;
  private daily = new Map<string, CachedFetch<DailyArchiveManifest>>();
  private thumbnailUrls = new Map<string, CachedThumbnailUrl>();
  private disposed = false;

  constructor(storage: ArchiveStorage, options: ArchiveManifestClientOptions) {
    this.storage = storage;
    this.maxAge = options.maxAge ?? 30_000;
    this.maximumDailyManifests = options.maximumDailyManifests ?? 32;
    this.maximumThumbnailUrls = Math.max(0, Math.floor(options.maximumThumbnailUrls ?? 64));
    manifestClientInternals.set(this, {
      storage,
      getManifest: (requestOptions) => this.getManifest(requestOptions),
    });
  }

  async load(options?: RequestOptions): Promise<DailyEntry> {
    this.assertUsable();
    const manifest = await this.getManifest(options);
    const latestDate = Object.keys(manifest.days).sort().at(-1);
    const latest = latestDate === undefined ? undefined : manifest.days[latestDate];
    if (!latest) throw new Error("The stream archive contains no days");
    return latest;
  }

  async refresh(signal?: AbortSignal): Promise<void> {
    this.assertUsable();
    this.manifest = undefined;
    this.daily.clear();
    await this.getManifest(signal ? { signal } : undefined);
  }

  async getTimeline(
    range: ISODateRange,
    options?: RequestOptions,
  ): Promise<ArchiveTimeline> {
    this.assertUsable();
    const startNs = utcNs(range.startedAt);
    const endNs = utcNs(range.endedAt);
    if (endNs < startNs) throw new Error("range.endedAt precedes range.startedAt");
    const { recordings, availableRange } = await this.recordingsAround(startNs, endNs, options);
    const timeline: ArchiveTimeline = {
      requestedRange: { startedAt: range.startedAt, endedAt: range.endedAt },
      spans: buildSpans(recordings, startNs, endNs),
    };
    if (availableRange) timeline.availableRange = availableRange;
    return timeline;
  }

  async locate(
    at: ISODateString,
    options?: RequestOptions,
  ): Promise<TimelineLocation> {
    this.assertUsable();
    const atNs = utcNs(at);
    const startNs = atNs - LOCATE_PADDING_NS;
    const endNs = atNs + LOCATE_PADDING_NS;
    const { recordings } = await this.recordingsAround(startNs, endNs, options);
    const location = locateWithin(buildSpans(recordings, startNs, endNs), at);
    if (location) return location;
    /* A sub-minimum sliver between recordings; report it as a gap. */
    return {
      utc: at,
      span: { kind: "gap", startedAt: at, endedAt: at },
    };
  }

  async getThumbnail(
    at: ISODateString,
    options?: RequestOptions,
  ): Promise<ThumbnailCue | null> {
    this.assertUsable();
    const location = await this.locate(at, options);
    const recording = location.recording;
    const track = recording?.thumbnail;
    if (!recording || !track) return null;
    if (!track.presentationNs.length || track.columns < 1) return null;
    if (track.cell.width < 1 || track.cell.height < 1) return null;

    const offsetNs = location.recordingOffsetNs ?? 0n;
    let frameIndex = 0;
    for (let index = 0; index < track.presentationNs.length; index++) {
      const presentation = track.presentationNs[index];
      if (presentation !== undefined && presentation <= offsetNs) frameIndex = index;
      else break;
    }
    const presentationNs = track.presentationNs[frameIndex] ?? 0n;
    const image = await this.getThumbnailUrl(track.key, options);
    return {
      sampledAt: isoFromNs(utcNs(recording.startedAt) + presentationNs),
      image,
      crop: {
        x: (frameIndex % track.columns) * track.cell.width,
        y: Math.floor(frameIndex / track.columns) * track.cell.height,
        width: track.cell.width,
        height: track.cell.height,
      },
    };
  }

  dispose(): void {
    this.disposed = true;
    this.manifest = undefined;
    this.daily.clear();
    this.thumbnailUrls.clear();
  }

  private assertUsable(): void {
    if (this.disposed) throw new Error("ArchiveManifestClient has been disposed");
  }

  private getManifest(options?: RequestOptions): Promise<StreamArchiveManifest> {
    this.assertUsable();
    const cached = this.manifest;
    if (cached && Date.now() - cached.fetchedAt <= this.maxAge) return cached.value;
    const entry: CachedFetch<StreamArchiveManifest> = {
      value: this.storage.getManifest(options),
      fetchedAt: Date.now(),
    };
    this.manifest = entry;
    entry.value.catch(() => {
      if (this.manifest === entry) this.manifest = undefined;
    });
    return entry.value;
  }

  private getDaily(key: string, options?: RequestOptions): Promise<DailyArchiveManifest> {
    const cached = this.daily.get(key);
    if (cached && Date.now() - cached.fetchedAt <= this.maxAge) {
      /* Re-insert for least-recently-used ordering. */
      this.daily.delete(key);
      this.daily.set(key, cached);
      return cached.value;
    }
    const entry: CachedFetch<DailyArchiveManifest> = {
      value: this.storage.getDailyManifest(key, options),
      fetchedAt: Date.now(),
    };
    this.daily.delete(key);
    this.daily.set(key, entry);
    entry.value.catch(() => {
      if (this.daily.get(key) === entry) this.daily.delete(key);
    });
    while (this.daily.size > this.maximumDailyManifests) {
      const oldest = this.daily.keys().next().value;
      if (oldest === undefined) break;
      this.daily.delete(oldest);
    }
    return entry.value;
  }

  private getThumbnailUrl(
    key: string,
    options?: RequestOptions,
  ): Promise<ThumbnailCue["image"]> {
    const now = Date.now();
    const cached = this.thumbnailUrls.get(key);
    if (
      cached?.value !== undefined
      && cached.expiresAtMs - now > THUMBNAIL_URL_REFRESH_MARGIN_MS
    ) {
      /* Re-insert for least-recently-used ordering. */
      this.thumbnailUrls.delete(key);
      this.thumbnailUrls.set(key, cached);
      return cached.value;
    }

    const entry: CachedThumbnailUrl = {
      expiresAtMs: now + SIGNED_URL_LIFETIME_SECONDS * 1000,
    };
    const value = this.storage.getThumbnail(key, options).then((source) => {
      const fallbackExpiresAtMs = now + SIGNED_URL_LIFETIME_SECONDS * 1000;
      const sourceExpiresAtMs = source.expiresAt === undefined
        ? fallbackExpiresAtMs
        : Date.parse(source.expiresAt);
      entry.expiresAtMs = Number.isFinite(sourceExpiresAtMs)
        ? sourceExpiresAtMs
        : fallbackExpiresAtMs;
      return {
        url: source.url,
        expiresAt: new Date(entry.expiresAtMs).toISOString(),
      };
    });
    entry.value = value;

    this.thumbnailUrls.delete(key);
    if (this.maximumThumbnailUrls > 0) this.thumbnailUrls.set(key, entry);
    value.catch(() => {
      if (this.thumbnailUrls.get(key) === entry) this.thumbnailUrls.delete(key);
    });
    while (this.thumbnailUrls.size > this.maximumThumbnailUrls) {
      const oldest = this.thumbnailUrls.keys().next().value;
      if (oldest === undefined) break;
      this.thumbnailUrls.delete(oldest);
    }
    return value;
  }

  /** Recordings from every day shard whose coverage overlaps the range,
   * deduplicated by id and sorted by start time, plus whole-archive coverage. */
  private async recordingsAround(
    startNs: bigint,
    endNs: bigint,
    options?: RequestOptions,
  ): Promise<RecordingsAroundResult> {
    const manifest = await this.getManifest(options);
    const days = Object.entries(manifest.days).sort(([left], [right]) => left.localeCompare(right));
    let coverageStartNs: bigint | undefined;
    let coverageEndNs: bigint | undefined;
    const wanted: DailyEntry[] = [];
    for (const [, day] of days) {
      const dayStartNs = utcNs(day.startedAt);
      const dayEndNs = utcNs(day.endedAt);
      if (coverageStartNs === undefined || dayStartNs < coverageStartNs) coverageStartNs = dayStartNs;
      if (coverageEndNs === undefined || dayEndNs > coverageEndNs) coverageEndNs = dayEndNs;
      if (dayEndNs > startNs && dayStartNs < endNs) wanted.push(day);
    }
    const dailies = await Promise.all(wanted.map((day) => this.getDaily(day.key, options)));
    const byId = new Map<string, NormalizedRecording>();
    for (const daily of dailies) {
      for (const recording of daily.recordings) {
        if (!byId.has(recording.id)) byId.set(recording.id, normalizeRecording(recording));
      }
    }
    const recordings = [...byId.values()].sort((left, right) =>
      left.startNs < right.startNs ? -1 : left.startNs > right.startNs ? 1 : 0,
    );
    const result: RecordingsAroundResult = { recordings };
    if (coverageStartNs !== undefined && coverageEndNs !== undefined) {
      result.availableRange = {
        startedAt: isoFromNs(coverageStartNs),
        endedAt: isoFromNs(coverageEndNs),
      };
    }
    return result;
  }
}
