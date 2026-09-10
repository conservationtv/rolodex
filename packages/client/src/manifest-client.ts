import { ArchiveObjectNotFoundError, documentFreshUntil } from "./document-cache.js";
import type { ArchiveDocument, ArchiveHead, ArchivePageEntry } from "./manifest.js";
import type {
  DailyArchiveManifest,
  DailyEntry,
  ISODateRange,
  ISODateString,
  StreamArchiveManifest,
} from "./manifest.js";
import type { ArchiveStorage, RequestOptions, Source } from "./storage.js";
import { LOCATE_PADDING_NS, SIGNED_URL_LIFETIME_SECONDS, isoFromNs, utcNs } from "./time.js";
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
  /** Optional application retention ceiling; archive policy can narrow it further. */
  retentionSeconds?: number;
  maximumDailyManifests?: number;
  /** Maximum number of presigned thumbnail sprite URLs retained in memory. */
  maximumThumbnailUrls?: number;
}

export interface CachedFetch<T> {
  value: Promise<T>;
  fetchedAt: number;
}

interface CachedThumbnailUrl {
  recording?: NormalizedRecording;
  image?: ThumbnailCue["image"];
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
  private missingHeads = new Map<string, number>();
  private discovered = new Map<string, DailyEntry>();
  private daily = new Map<string, CachedFetch<DailyArchiveManifest>>();
  private documents = new Map<string, CachedFetch<ArchiveDocument>>();
  private retentionSeconds: number | undefined = undefined;
  private readonly retentionLimit: number | undefined;
  private thumbnails = new Map<string, CachedThumbnailUrl>();

  constructor(storage: ArchiveStorage, options?: ArchiveManifestClientOptions) {
    this.storage = storage;
    this.retentionLimit = options?.retentionSeconds;
    if (
      this.retentionLimit !== undefined &&
      (!Number.isSafeInteger(this.retentionLimit) || this.retentionLimit <= 0)
    )
      throw new RangeError("retentionSeconds must be a positive integer");
    this.retentionSeconds = this.retentionLimit;
    this.maxAge = options?.maxAge ?? 30_000;
    this.maximumDailyManifests = options?.maximumDailyManifests ?? 32;
    this.maximumThumbnailUrls = Math.max(0, Math.floor(options?.maximumThumbnailUrls ?? 64));
  }

  async load(options?: RequestOptions): Promise<DailyEntry> {
    const manifest = await this.getManifest(options);
    const days = await this.coverageDays(manifest, options);
    const latest = days.sort((left, right) => left.endedAt.localeCompare(right.endedAt)).at(-1);
    if (!latest) throw new Error("The stream archive contains no days");
    return latest;
  }

  async refresh(signal?: AbortSignal): Promise<void> {
    // Legacy custom storage has no freshness contract; preserve its explicit refresh semantics.
    const current = this.manifest ? await this.manifest.value : undefined;
    if (current && documentFreshUntil(current, -1) === -1) {
      this.manifest = undefined;
      this.daily.clear();
      this.documents.clear();
    }
    // Refresh obeys HTTP freshness and preserves immutable page identities.
    const manifest = await this.getManifest({ signal });
    await this.discoverToday(manifest, { signal });
  }

  async getTimeline(range: ISODateRange, options?: RequestOptions): Promise<ArchiveTimeline> {
    await this.getManifest(options);
    const startNs = this.retainedStart(utcNs(range.startedAt));
    const endNs = utcNs(range.endedAt);
    if (utcNs(range.endedAt) < utcNs(range.startedAt))
      throw new Error("range.endedAt precedes range.startedAt");
    if (endNs <= startNs) return { requestedRange: range, spans: [] };
    const { recordings, availableRange } = await this.recordingsAround(startNs, endNs, options);
    const timeline: ArchiveTimeline = {
      requestedRange: { startedAt: isoFromNs(startNs), endedAt: range.endedAt },
      spans: buildSpans(recordings, startNs, endNs)
        .map((span) => ({
          ...span,
          startedAt: isoFromNs(this.retainedStart(utcNs(span.startedAt))),
        }))
        .filter((span) => span.endedAt > span.startedAt),
    };
    if (availableRange) timeline.availableRange = availableRange;
    return timeline;
  }

  async locate(at: ISODateString, options?: RequestOptions): Promise<TimelineLocation> {
    await this.getManifest(options);
    at = isoFromNs(this.retainedStart(utcNs(at)));
    const atNs = utcNs(at);
    const startNs = atNs - LOCATE_PADDING_NS;
    const endNs = atNs + LOCATE_PADDING_NS;
    const { recordings } = await this.recordingsAround(startNs, endNs, options, atNs);
    const location = locateWithin(buildSpans(recordings, startNs, endNs), at);
    if (location) return location;
    /* buildSpans omits sub-minimum slivers, but a player still needs the next
     * exact boundary. A zero-width gap would repeatedly seek the same instant. */
    const next = recordings.find((recording) => recording.startNs > atNs);
    return {
      utc: at,
      span: { kind: "gap", startedAt: at, endedAt: next ? isoFromNs(next.startNs) : at },
    };
  }

  async getThumbnail(at: ISODateString, options?: RequestOptions): Promise<ThumbnailCue | null> {
    await this.getManifest(options);
    if (utcNs(at) < this.retainedStart(utcNs(at))) return null;
    const location = await this.locate(at, options);
    const recording = location.recording;
    const track = recording?.thumbnail;
    if (!recording || !track) return null;
    if (!track.presentationNs.length || track.columns < 1) return null;
    if (track.cell.width < 1 || track.cell.height < 1) return null;

    const image = await this.getThumbnailUrl(track.key, options);
    const cached = this.thumbnails.get(track.key);
    if (cached) {
      cached.recording = normalizeRecording(recording);
      cached.image = image;
    }
    return thumbnailCue(normalizeRecording(recording), utcNs(at), image) ?? null;
  }

  /** Resolve any crop in a known sprite without fetching manifests or renewing its URL. */
  getCachedThumbnail(at: ISODateString): ThumbnailCue | undefined {
    const atNs = utcNs(at);
    if (atNs < this.retainedStart(atNs)) return undefined;
    const now = Date.now();
    for (const [key, cached] of this.thumbnails) {
      // Only resolved, unexpired sprites whose recording owns this UTC can satisfy a cache-only lookup.
      const recording = cached.recording;
      if (!recording || !cached.image) continue;
      if (cached.expiresAtMs - now <= THUMBNAIL_URL_REFRESH_MARGIN_MS) continue;
      if (atNs < recording.startNs || atNs >= recording.endNs) continue;
      this.thumbnails.delete(key);
      this.thumbnails.set(key, cached);
      return thumbnailCue(recording, atNs, cached.image);
    }
    return undefined;
  }

  /** Stream identity from discovery, with authoritative coverage from cached daily heads. */
  async describe(options?: RequestOptions): Promise<ArchiveDescription> {
    const manifest = await this.getManifest(options);
    let coverage: ISODateRange | undefined;
    for (const day of await this.coverageDays(manifest, options)) {
      coverage =
        coverage === undefined
          ? { startedAt: day.startedAt, endedAt: day.endedAt }
          : {
              startedAt:
                utcNs(day.startedAt) < utcNs(coverage.startedAt)
                  ? day.startedAt
                  : coverage.startedAt,
              endedAt:
                utcNs(day.endedAt) > utcNs(coverage.endedAt) ? day.endedAt : coverage.endedAt,
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
    this.retentionSeconds = undefined;
    this.daily.clear();
    this.thumbnails.clear();
    this.documents.clear();
    this.missingHeads.clear();
    this.discovered.clear();
  }

  /** Coverage lives in heads; root entries only locate days. */
  private async coverageDays(
    manifest: StreamArchiveManifest,
    options?: RequestOptions,
  ): Promise<DailyEntry[]> {
    await this.discoverToday(manifest, options);
    const days = new Map(Object.entries(manifest.days));
    for (const [date, day] of this.discovered) days.set(date, day);
    const cutoff =
      this.retentionSeconds === undefined ? -8.64e15 : Date.now() - this.retentionSeconds * 1000;
    const candidates = [...days].filter(
      ([date]) => Date.parse(`${date}T00:00:00Z`) + 86_400_000 > cutoff,
    );
    const resolved = await Promise.all(candidates.map(([, day]) => this.dayCoverage(day, options)));
    return resolved
      .filter((day) => Date.parse(day.endedAt) > Math.max(cutoff, Date.parse(day.startedAt)))
      .map((day) => ({
        ...day,
        startedAt: new Date(Math.max(cutoff, Date.parse(day.startedAt))).toISOString(),
      }));
  }

  private async dayCoverage(day: DailyEntry, options?: RequestOptions): Promise<DailyEntry> {
    if (!this.storage.getArchiveObject) return day;
    const head = await this.getDocument(day.key, options);
    // Custom stores may still expose complete daily documents.
    if (!("tail" in head)) return day;
    return { key: day.key, startedAt: head.startedAt, endedAt: head.endedAt };
  }

  /** Probe today's deterministic head independently of the slowly refreshed root, including empty streams. */
  private async discoverToday(
    manifest: StreamArchiveManifest,
    options?: RequestOptions,
  ): Promise<void> {
    if (!manifest.headPrefix || !this.storage.getArchiveObject) return;
    const date = new Date().toISOString().slice(0, 10);
    if (manifest.days[date] || this.discovered.has(date)) return;
    const key = `${manifest.headPrefix}${date}/head.json`;
    if ((this.missingHeads.get(key) ?? 0) > Date.now()) return;
    try {
      const head = await this.getDocument(key, options);
      if (!("tail" in head) || head.date !== date || head.stream !== manifest.stream)
        throw new Error("Discovered archive head identity mismatch");
      this.discovered.set(date, { key, startedAt: head.startedAt, endedAt: head.endedAt });
      this.missingHeads.clear();
    } catch (cause) {
      if (!(cause instanceof ArchiveObjectNotFoundError)) throw cause;
      this.missingHeads.clear();
      this.missingHeads.set(key, Date.now() + 5000);
    }
  }

  private retainedStart(at: bigint): bigint {
    if (this.retentionSeconds === undefined) return at;
    const cutoff = BigInt(Date.now() - this.retentionSeconds * 1000) * 1_000_000n;
    return at < cutoff ? cutoff : at;
  }

  private getManifest(options?: RequestOptions): Promise<StreamArchiveManifest> {
    const cached = this.manifest;
    if (cached && Date.now() < cached.fetchedAt) return cached.value;
    const entry: CachedFetch<StreamArchiveManifest> = {
      value: this.storage.getManifest(options),
      fetchedAt: Infinity,
    };
    entry.value = entry.value.then((value) => {
      entry.fetchedAt = documentFreshUntil(value, Date.now() + this.maxAge);
      const seconds = Math.min(this.retentionLimit ?? Infinity, value.retentionSeconds ?? Infinity);
      this.retentionSeconds = Number.isFinite(seconds) ? seconds : undefined;
      for (const date of this.discovered.keys()) {
        if (value.days[date]) this.discovered.delete(date);
      }
      return value;
    });
    this.manifest = entry;
    entry.value.catch(() => {
      if (this.manifest === entry) this.manifest = undefined;
    });
    return entry.value;
  }

  private getDaily(key: string, options?: RequestOptions): Promise<DailyArchiveManifest> {
    const cached = this.daily.get(key);
    if (cached && Date.now() < cached.fetchedAt) {
      /* Re-insert for least-recently-used ordering. */
      this.daily.delete(key);
      this.daily.set(key, cached);
      return cached.value;
    }
    const entry: CachedFetch<DailyArchiveManifest> = {
      value: this.storage.getDailyManifest(key, options),
      fetchedAt: Infinity,
    };
    entry.value = entry.value.then((value) => {
      entry.fetchedAt = documentFreshUntil(value, Date.now() + this.maxAge);
      return value;
    });
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

  private getDocument(key: string, options?: RequestOptions): Promise<ArchiveDocument> {
    const cached = this.documents.get(key);
    if (cached && cached.fetchedAt > Date.now()) return cached.value;
    const reader = this.storage.getArchiveObject;
    if (!reader) throw new Error("Paged archives require ArchiveStorage.getArchiveObject");
    const entry: CachedFetch<ArchiveDocument> = {
      value: reader.call(this.storage, key, options),
      fetchedAt: Infinity,
    };
    entry.value = entry.value.then((value) => {
      entry.fetchedAt = documentFreshUntil(value, Date.now() + this.maxAge);
      return value;
    });
    this.documents.delete(key);
    this.documents.set(key, entry);
    entry.value.catch(() => {
      if (this.documents.get(key) === entry) this.documents.delete(key);
    });
    while (this.documents.size > this.maximumDailyManifests * 16) {
      const oldest = this.documents.keys().next().value;
      if (oldest === undefined) break;
      this.documents.delete(oldest);
    }
    return entry.value;
  }

  private async readDay(
    key: string,
    startNs: bigint,
    endNs: bigint,
    options?: RequestOptions,
    pointNs?: bigint,
  ): Promise<DailyArchiveManifest> {
    if (!this.storage.getArchiveObject) return this.getDaily(key, options);
    const document = await this.getDocument(key, options);
    if ("recordings" in document) return document;
    if (!("tail" in document)) throw new Error(`${key}: expected a daily head`);
    return this.readPages(document, startNs, endNs, options, pointNs);
  }

  private async readPages(
    head: ArchiveHead,
    startNs: bigint,
    endNs: bigint,
    options?: RequestOptions,
    pointNs?: bigint,
  ): Promise<DailyArchiveManifest> {
    const directory = await this.getDocument(head.pages, options);
    if (
      !("pages" in directory) ||
      !Array.isArray(directory.pages) ||
      directory.stream !== head.stream ||
      directory.date !== head.date
    )
      throw new Error("Archive page directory identity mismatch");
    const entries: ArchivePageEntry[] = [...directory.pages, head.tail];
    const wanted =
      pointNs === undefined
        ? entries.filter((page) => utcNs(page.endedAt) > startNs && utcNs(page.startedAt) < endNs)
        : this.pagesAt(entries, pointNs);
    const documents = await Promise.all(wanted.map((page) => this.getDocument(page.key, options)));
    const recordings = documents.flatMap((document) => {
      if (
        !("recordings" in document) ||
        document.stream !== head.stream ||
        document.date !== head.date
      )
        throw new Error("Archive page identity mismatch");
      return document.recordings;
    });
    return { version: 1, stream: head.stream, date: head.date, recordings };
  }

  private pagesAt(entries: ArchivePageEntry[], atNs: bigint): ArchivePageEntry[] {
    const covering = entries.filter(
      (page) => utcNs(page.startedAt) <= atNs && utcNs(page.endedAt) > atNs,
    );
    if (covering.length) return covering;
    // A seek in a gap needs only the next page to find the precise forward boundary.
    const next = entries
      .filter((page) => utcNs(page.startedAt) > atNs)
      .sort((left, right) => left.startedAt.localeCompare(right.startedAt))[0];
    return next ? [next] : [];
  }

  private getThumbnailUrl(key: string, options?: RequestOptions): Promise<ThumbnailCue["image"]> {
    const now = Date.now();
    const cached = this.thumbnails.get(key);
    if (cached?.value !== undefined && cached.expiresAtMs - now > THUMBNAIL_URL_REFRESH_MARGIN_MS) {
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
      const sourceExpiresAtMs =
        source.expiresAt === undefined ? fallbackExpiresAtMs : Date.parse(source.expiresAt);
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
    pointNs?: bigint,
  ): Promise<RecordingsAroundResult> {
    const manifest = await this.getManifest(options);
    const days = await this.coverageDays(manifest, options);
    let coverageStartNs: bigint | undefined;
    let coverageEndNs: bigint | undefined;
    const wanted: DailyEntry[] = [];
    for (const day of days) {
      const dayStartNs = utcNs(day.startedAt);
      const dayEndNs = utcNs(day.endedAt);
      if (coverageStartNs === undefined || dayStartNs < coverageStartNs)
        coverageStartNs = dayStartNs;
      if (coverageEndNs === undefined || dayEndNs > coverageEndNs) coverageEndNs = dayEndNs;
      if (dayEndNs > startNs && dayStartNs < endNs) wanted.push(day);
    }
    const dailies = await Promise.all(
      wanted.map((day) => this.readDay(day.key, startNs, endNs, options, pointNs)),
    );
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

/** Select the preceding sample within a recording; every crop shares the same sprite URL. */
function thumbnailCue(
  recording: NormalizedRecording,
  atNs: bigint,
  image: ThumbnailCue["image"],
): ThumbnailCue | undefined {
  const track = recording.recording.thumbnail;
  if (!track) return undefined;
  const offsetNs = atNs - recording.startNs;
  let frameIndex = 0;
  for (let index = 0; index < track.presentationNs.length; index++) {
    const presentation = track.presentationNs[index];
    if (presentation !== undefined && presentation <= offsetNs) frameIndex = index;
    else break;
  }
  return {
    sampledAt: isoFromNs(recording.startNs + (track.presentationNs[frameIndex] ?? 0n)),
    image,
    crop: {
      x: (frameIndex % track.columns) * track.cell.width,
      y: Math.floor(frameIndex / track.columns) * track.cell.height,
      width: track.cell.width,
      height: track.cell.height,
    },
  };
}
