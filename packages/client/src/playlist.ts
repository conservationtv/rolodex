import type { ArchiveManifestClient } from "./manifest-client.js";
import type { ArchiveRecording, ArchivePiece, ISODateString } from "./manifest.js";
import type { MediaPiece } from "./media-pieces.js";
import type { Source } from "./storage.js";
import {
  firstSegmentAtOrAfter,
  normalizeRecording,
  precedingSegmentIndex,
} from "./timeline.js";
import {
  NS_PER_SECOND,
  decimalSeconds,
  isoFromNs,
  nsFromSeconds,
  secondsFromNs,
  utcNs,
} from "./time.js";

/* 2. HLS compilation and Blob URL ownership */

export interface PlaylistWindow {
  before: number;
  after: number;
}

export interface PlaylistWindowRequest {
  at: ISODateString;
  window?: PlaylistWindow;
}

export interface PlaylistLease extends CompiledPlaylist {
  readonly url: string;
  revoke(): void;
  [Symbol.dispose](): void;
}

export interface CompiledPlaylist {
  text: string;
  mediaSegments?: ReadonlyMap<string, readonly MediaPiece[]>;
  startedAt: ISODateString;
  endedAt: ISODateString;
  initial: number;

  utcAt(mediaTime: number): ISODateString;
  mediaTimeAt(utc: ISODateString): number;
}

export interface PlaylistFactoryOptions {
  defaultBeforeSeconds?: number;
  defaultAfterSeconds?: number;
  targetSegmentSeconds?: number;
}

/** One virtual HLS media segment: byte-contiguous decode-ordered parts. */
export interface VirtualSegment {
  startNs: bigint;
  endNs: bigint;
  pieces?: readonly ArchivePiece[];
  byteOffset: bigint;
  byteLength: bigint;
}

export interface CompiledRecording {
  recording: ArchiveRecording;
  startNs: bigint;
  groups: VirtualSegment[];
}

export interface ResolvedRecording extends CompiledRecording {
  init: Source;
  source: Source;
  sources: ReadonlyMap<string, Source>;
}

/** Maps the playlist's contiguous media time onto discontinuous UTC. */
export interface PlaylistPiece {
  mediaStartNs: bigint;
  durationNs: bigint;
  utcStartNs: bigint;
}

export class PlaylistFactory {
  private manifests: ArchiveManifestClient;
  private defaultBeforeSeconds: number;
  private defaultAfterSeconds: number;
  private targetSegmentSeconds: number;
  private revocations = new Set<() => void>();
  private disposed = false;

  constructor(
    manifests: ArchiveManifestClient,
    options?: PlaylistFactoryOptions,
  ) {
    this.manifests = manifests;
    this.defaultBeforeSeconds = options?.defaultBeforeSeconds ?? 0;
    this.defaultAfterSeconds = options?.defaultAfterSeconds ?? 90;
    this.targetSegmentSeconds = options?.targetSegmentSeconds ?? 4;
  }

  async compile(request: PlaylistWindowRequest): Promise<CompiledPlaylist> {
    if (this.disposed) throw new Error("PlaylistFactory has been disposed");
    const atNs = utcNs(request.at);
    const windowStartNs = atNs - nsFromSeconds(Math.max(0, request.window?.before ?? this.defaultBeforeSeconds));
    const windowEndNs = atNs + nsFromSeconds(Math.max(0, request.window?.after ?? this.defaultAfterSeconds));
    const timeline = await this.manifests.getTimeline({
      startedAt: isoFromNs(windowStartNs),
      endedAt: isoFromNs(windowEndNs),
    });
    const targetNs = nsFromSeconds(Math.max(0.001, this.targetSegmentSeconds));

    /* A playlist may cross file boundaries only while UTC remains continuous.
     * A real gap must terminate media so the player store can expose it rather
     * than collapsing missing wall-clock time. */
    const targetSpanIndex = timeline.spans.findIndex((span) =>
      span.kind === "recording"
      && atNs >= utcNs(span.startedAt)
      && atNs < utcNs(span.endedAt),
    );
    if (targetSpanIndex < 0) throw new Error(`No media is available around ${request.at}`);
    let firstSpanIndex = targetSpanIndex;
    while (firstSpanIndex > 0 && timeline.spans[firstSpanIndex - 1]?.kind === "recording") {
      firstSpanIndex -= 1;
    }
    let lastSpanIndex = targetSpanIndex;
    while (
      lastSpanIndex + 1 < timeline.spans.length
      && timeline.spans[lastSpanIndex + 1]?.kind === "recording"
    ) {
      lastSpanIndex += 1;
    }
    const uninterruptedSpans = timeline.spans.slice(firstSpanIndex, lastSpanIndex + 1);

    const compiled: CompiledRecording[] = [];
    for (const span of uninterruptedSpans) {
      if (span.kind !== "recording") continue;
      const item = normalizeRecording(span.recording);
      const segments = item.recording.segments;
      if (!segments.length) continue;
      const localStartNs = windowStartNs > item.startNs ? windowStartNs - item.startNs : 0n;
      const localEndNs = windowEndNs - item.startNs < item.durationNs ? windowEndNs - item.startNs : item.durationNs;
      if (localEndNs <= 0n || localStartNs >= item.durationNs) continue;
      /* Anchor at the random-access segment preceding the window start so a
       * hidden preroll can decode up to the requested instant. */
      const anchor = precedingSegmentIndex(segments, localStartNs);
      const end = localEndNs >= item.durationNs
        ? segments.length
        : Math.max(anchor + 1, firstSegmentAtOrAfter(segments, localEndNs));
      const groups: VirtualSegment[] = [];
      for (let index = anchor; index < end; index++) {
        const segment = segments[index];
        if (!segment) break;
        const next = segments[index + 1];
        const startNs = segment.presentationStartNs;
        const endNs = next ? next.presentationStartNs : item.durationNs;
        const previous = groups[groups.length - 1];
        const contiguous = previous !== undefined && !previous.pieces && !segment.pieces
          && previous.byteOffset + previous.byteLength === BigInt(segment.byteOffset);
        if (previous && contiguous && previous.endNs - previous.startNs < targetNs && endNs - previous.startNs <= 6n * NS_PER_SECOND && previous.byteLength + BigInt(segment.byteLength) <= 4_000_000n) {
          previous.endNs = endNs;
          previous.byteLength += BigInt(segment.byteLength);
        } else {
          groups.push({
            pieces: segment.pieces,
            startNs,
            endNs,
            byteOffset: BigInt(segment.byteOffset),
            byteLength: BigInt(segment.byteLength),
          });
        }
      }
      if (groups.length) compiled.push({ recording: item.recording, startNs: item.startNs, groups });
    }
    if (!compiled.length) throw new Error(`No media is available around ${request.at}`);

    const resolved: ResolvedRecording[] = await Promise.all(compiled.map(async (item) => {
      const [init, source] = await Promise.all([
        this.manifests.getVideo(item.recording.init),
        this.manifests.getVideo(item.recording.source),
      ]);
      const keys = new Set(item.groups.flatMap(group => group.pieces?.map(piece => piece.source) ?? []));
      const sources = new Map(await Promise.all([...keys].map(async key => [key, await this.manifests.getVideo(key)] as const)));
      return { ...item, init, source, sources };
    }));

    let maximumNs = 0n;
    for (const item of resolved) {
      for (const group of item.groups) {
        const durationNs = group.endNs - group.startNs;
        if (durationNs > maximumNs) maximumNs = durationNs;
      }
    }

    const mediaSegments = new Map<string, readonly MediaPiece[]>();
    const segmentPrefix = `https://rolodex.invalid/${crypto.randomUUID()}/`;
    const lines = [
      "#EXTM3U",
      "#EXT-X-VERSION:7",
      `#EXT-X-TARGETDURATION:${(maximumNs + NS_PER_SECOND - 1n) / NS_PER_SECOND}`,
      "#EXT-X-MEDIA-SEQUENCE:0",
      "#EXT-X-PLAYLIST-TYPE:VOD",
      "#EXT-X-INDEPENDENT-SEGMENTS",
    ];
    const pieces: PlaylistPiece[] = [];
    let mediaCursorNs = 0n;
    resolved.forEach((item, recordingIndex) => {
      /* Every file boundary resets tfdt, so it is always a discontinuity. */
      if (recordingIndex > 0) lines.push("#EXT-X-DISCONTINUITY");
      lines.push(`#EXT-X-MAP:URI="${item.init.url}"`);
      for (const group of item.groups) {
        const durationNs = group.endNs - group.startNs;
        pieces.push({ mediaStartNs: mediaCursorNs, durationNs, utcStartNs: item.startNs + group.startNs });
        const media = segmentMedia(group, item, mediaSegments, segmentPrefix);
        lines.push(
          `#EXT-X-PROGRAM-DATE-TIME:${isoFromNs(item.startNs + group.startNs)}`,
          `#EXTINF:${decimalSeconds(durationNs)},`,
          ...media,
        );
        mediaCursorNs += durationNs;
      }
    });
    lines.push("#EXT-X-ENDLIST", "");

    const totalNs = mediaCursorNs;
    const first = pieces[0];
    const last = pieces[pieces.length - 1];
    if (!first || !last) throw new Error(`No media is available around ${request.at}`);

    const mediaTimeAt = (utc: ISODateString): number => {
      const target = utcNs(utc);
      for (const piece of pieces) {
        if (target < piece.utcStartNs) return secondsFromNs(piece.mediaStartNs);
        if (target < piece.utcStartNs + piece.durationNs) {
          return secondsFromNs(piece.mediaStartNs + (target - piece.utcStartNs));
        }
      }
      return secondsFromNs(totalNs);
    };

    const utcAt = (mediaTime: number): ISODateString => {
      let clamped = nsFromSeconds(mediaTime);
      if (clamped < 0n) clamped = 0n;
      if (clamped > totalNs) clamped = totalNs;
      let selected = first;
      for (const piece of pieces) {
        if (piece.mediaStartNs <= clamped) selected = piece;
        else break;
      }
      const within = clamped - selected.mediaStartNs;
      return isoFromNs(selected.utcStartNs + (within < selected.durationNs ? within : selected.durationNs));
    };

    return {
      text: lines.join("\n"),
      mediaSegments,
      startedAt: isoFromNs(first.utcStartNs),
      endedAt: isoFromNs(last.utcStartNs + last.durationNs),
      initial: mediaTimeAt(request.at),
      utcAt,
      mediaTimeAt,
    };
  }

  async create(request: PlaylistWindowRequest): Promise<PlaylistLease> {
    const compiled = await this.compile(request);
    const url = URL.createObjectURL(
      new Blob([compiled.text], { type: "application/vnd.apple.mpegurl" }),
    );
    let revoked = false;
    const revoke = (): void => {
      if (revoked) return;
      revoked = true;
      this.revocations.delete(revoke);
      URL.revokeObjectURL(url);
    };
    this.revocations.add(revoke);
    return { ...compiled, url, revoke, [Symbol.dispose]: revoke };
  }

  dispose(): void {
    this.disposed = true;
    for (const revoke of Array.from(this.revocations)) revoke();
  }
}

function segmentMedia(group: VirtualSegment, item: ResolvedRecording, segments: Map<string, readonly MediaPiece[]>, prefix: string): string[] {
  if (!group.pieces) return [`#EXT-X-BYTERANGE:${group.byteLength}@${group.byteOffset}`, item.source.url];
  const pieces = group.pieces.map(piece => {
    const source = item.sources.get(piece.source);
    if (!source) throw new Error("Archive piece URL was not resolved");
    return { url: source.url, byteOffset: piece.byteOffset, byteLength: piece.byteLength };
  });
  const first = pieces[0];
  if (pieces.length === 1 && first) return [`#EXT-X-BYTERANGE:${first.byteLength}@${first.byteOffset}`, first.url];
  const url = `${prefix}${segments.size}.m4s`;
  segments.set(url, pieces);
  return [url];
}
