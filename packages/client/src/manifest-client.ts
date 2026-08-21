import type {
  DailyArchiveManifest,
  DailyEntry,
  ISODateRange,
  ISODateString,
  StreamArchiveManifest,
} from "./manifest.js";
import type { ArchiveStorage, RequestOptions, Source } from "./storage.js";
import {
  LOCATE_PADDING_NS,
  SIGNED_URL_LIFETIME_SECONDS,
  isoFromNs,
  utcNs,
} from "./time.js";
import {
  buildSpans,
  locateWithin,
  normalizeRecording,
  type ArchiveTimeline,
  type NormalizedRecording,
  type ThumbnailCue,
  type TimelineLocation,
} from "./timeline.js";

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

export interface ArchiveDescription {
  /** Stream identity, e.g. "h265/cam". */
  stream: string;
  /** Whole-archive coverage: earliest to latest instant across every day. */
  coverage?: ISODateRange;
}

export class ArchiveManifestClient {
  private storage: ArchiveStorage;
  private maxAge: number;
  private maximumDailyManifests: number;
  private maximumThumbnailUrls: number;
  private manifest: CachedFetch<StreamArchiveManifest> | undefined;
  private daily = new Map<string, CachedFetch<DailyArchiveManifest>>();
  private thumbnails = new Map<string, CachedThumbnailUrl>();

  constructor(storage: ArchiveStorage, options?: ArchiveManifestClientOptions) {
    this.storage = storage;
    this.maxAge = options?.maxAge ?? 30_000;
    this.maximumDailyManifests = options?.maximumDailyManifests ?? 32;
    this.maximumThumbnailUrls = Math.max(0, Math.floor(options?.maximumThumbnailUrls ?? 64));
  }

  async load(options?: RequestOptions): Promise<DailyEntry> {
    const manifest = await this.getManifest(options);
    const latestDate = Object.keys(manifest.days).sort().at(-1);
    const latest = latestDate === undefined ? undefined : manifest.days[latestDate];
    if (!latest) throw new Error("The stream archive contains no days");
    return latest;
  }

  async refresh(signal?: AbortSignal): Promise<void> {
    this.manifest = undefined;
    this.daily.clear();
    await this.getManifest({ signal });
  }

  async getTimeline(
    range: ISODateRange,
    options?: RequestOptions,
  ): Promise<ArchiveTimeline> {
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

  /** Stream identity and whole-archive coverage, served from the cached
   * stream manifest (so repeated calls within `maxAge` cost nothing). */
  async describe(options?: RequestOptions): Promise<ArchiveDescription> {
    const manifest = await this.getManifest(options);
    let coverage: ISODateRange | undefined;
    for (const day of Object.values(manifest.days)) {
      coverage = coverage === undefined
        ? { startedAt: day.startedAt, endedAt: day.endedAt }
        : {
            startedAt: utcNs(day.startedAt) < utcNs(coverage.startedAt) ? day.startedAt : coverage.startedAt,
            endedAt: utcNs(day.endedAt) > utcNs(coverage.endedAt) ? day.endedAt : coverage.endedAt,
          };
    }
    const description: ArchiveDescription = { stream: manifest.stream };
    if (coverage) description.coverage = coverage;
    return description;
  }

  /** Resolve a playback object key (media or init MP4) to a fetchable URL.
   * Byte ranges live in the compiled playlist, so URLs are whole-object. */
  async getVideo(key: string, options?: RequestOptions): Promise<Source> {
    return this.storage.getSource(key, options);
  }

  dispose(): void {
    this.manifest = undefined;
    this.daily.clear();
    this.thumbnails.clear();
  }


  private getManifest(options?: RequestOptions): Promise<StreamArchiveManifest> {
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
    const cached = this.thumbnails.get(key);
    if (
      cached?.value !== undefined
      && cached.expiresAtMs - now > THUMBNAIL_URL_REFRESH_MARGIN_MS
    ) {
      /* Re-insert for least-recently-used ordering. */
      this.thumbnails.delete(key);
      this.thumbnails.set(key, cached);
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

    this.thumbnails.delete(key);
    if (this.maximumThumbnailUrls > 0) this.thumbnails.set(key, entry);
    value.catch(() => {
      if (this.thumbnails.get(key) === entry) this.thumbnails.delete(key);
    });
    while (this.thumbnails.size > this.maximumThumbnailUrls) {
      const oldest = this.thumbnails.keys().next().value;
      if (oldest === undefined) break;
      this.thumbnails.delete(oldest);
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
