import { ArchiveManifestClient, internalsOf } from "./manifest-client.js";
import type { ArchiveRecording, ISODateString } from "./manifest.js";
import type { ArchiveStorage, Source } from "./storage.js";
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
}

/** Maps the playlist's contiguous media time onto discontinuous UTC. */
export interface PlaylistPiece {
  mediaStartNs: bigint;
  durationNs: bigint;
  utcStartNs: bigint;
}

export class PlaylistFactory {
  private manifests: ArchiveManifestClient;
  private storage: ArchiveStorage;
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
    this.storage = internalsOf(manifests).storage;
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

    const compiled: CompiledRecording[] = [];
    for (const span of timeline.spans) {
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
        const contiguous = previous !== undefined
          && previous.byteOffset + previous.byteLength === BigInt(segment.byteOffset);
        if (previous && contiguous && previous.endNs - previous.startNs < targetNs) {
          previous.endNs = endNs;
          previous.byteLength += BigInt(segment.byteLength);
        } else {
          groups.push({
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
        this.storage.getInit(item.recording.init),
        this.storage.getSource(item.recording.source),
      ]);
      return { ...item, init, source };
    }));

    let maximumNs = 0n;
    for (const item of resolved) {
      for (const group of item.groups) {
        const durationNs = group.endNs - group.startNs;
        if (durationNs > maximumNs) maximumNs = durationNs;
      }
    }

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
        lines.push(
          `#EXT-X-PROGRAM-DATE-TIME:${isoFromNs(item.startNs + group.startNs)}`,
          `#EXTINF:${decimalSeconds(durationNs)},`,
          `#EXT-X-BYTERANGE:${group.byteLength}@${group.byteOffset}`,
          item.source.url,
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
