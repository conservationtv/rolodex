import type {
  ArchiveRecording,
  ArchiveSegment,
  ISODateRange,
  ISODateString,
} from "./manifest.js";
import { MINIMUM_GAP_NS, isoFromNs, utcNs } from "./time.js";

/* Normalized archive model */

export interface ResolvedObjectUrl {
  url: string;
  expiresAt: ISODateString;
}

export interface RecordingTimelineSpan {
  kind: "recording";
  startedAt: ISODateString;
  endedAt: ISODateString;
  recording: ArchiveRecording;
}

export interface GapTimelineSpan {
  kind: "gap";
  startedAt: ISODateString;
  endedAt: ISODateString;
}

export type TimelineSpan = RecordingTimelineSpan | GapTimelineSpan;

export interface ArchiveTimeline {
  requestedRange: ISODateRange;
  availableRange?: ISODateRange;
  spans: TimelineSpan[];
}

export interface TimelineLocation {
  utc: ISODateString;
  span: TimelineSpan;
  recording?: ArchiveRecording;
  recordingOffsetNs?: bigint;
  segmentIndex?: number;
}

export interface ThumbnailCrop {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface ThumbnailCue {
  sampledAt: ISODateString;
  image: ResolvedObjectUrl;
  crop: ThumbnailCrop;
}

/* Timeline internals */

export interface NormalizedRecording {
  recording: ArchiveRecording;
  startNs: bigint;
  endNs: bigint;
  durationNs: bigint;
}

export function normalizeRecording(recording: ArchiveRecording): NormalizedRecording {
  const startNs = utcNs(recording.startedAt);
  const durationNs = recording.durationNs;
  return { recording, startNs, durationNs, endNs: startNs + durationNs };
}

export function buildSpans(
  recordings: NormalizedRecording[],
  rangeStartNs: bigint,
  rangeEndNs: bigint,
): TimelineSpan[] {
  const spans: TimelineSpan[] = [];
  let cursorNs = rangeStartNs;
  for (const item of recordings) {
    if (item.endNs <= rangeStartNs || item.startNs >= rangeEndNs) continue;
    if (item.startNs - cursorNs >= MINIMUM_GAP_NS) {
      spans.push({ kind: "gap", startedAt: isoFromNs(cursorNs), endedAt: isoFromNs(item.startNs) });
    }
    spans.push({
      kind: "recording",
      startedAt: isoFromNs(item.startNs),
      endedAt: isoFromNs(item.endNs),
      recording: item.recording,
    });
    if (item.endNs > cursorNs) cursorNs = item.endNs;
  }
  if (rangeEndNs - cursorNs >= MINIMUM_GAP_NS) {
    spans.push({ kind: "gap", startedAt: isoFromNs(cursorNs), endedAt: isoFromNs(rangeEndNs) });
  }
  return spans;
}

/** Index of the last random-access segment at or before the offset. */
export function precedingSegmentIndex(segments: ArchiveSegment[], offsetNs: bigint): number {
  let candidate = 0;
  for (let index = 0; index < segments.length; index++) {
    const segment = segments[index];
    if (segment && segment.presentationStartNs <= offsetNs) candidate = index;
    else break;
  }
  return candidate;
}

export function firstSegmentAtOrAfter(segments: ArchiveSegment[], offsetNs: bigint): number {
  for (let index = 0; index < segments.length; index++) {
    const segment = segments[index];
    if (segment && segment.presentationStartNs >= offsetNs) return index;
  }
  return segments.length;
}

export function locateWithin(spans: TimelineSpan[], at: ISODateString): TimelineLocation | undefined {
  const atNs = utcNs(at);
  for (const span of spans) {
    if (atNs < utcNs(span.startedAt) || atNs >= utcNs(span.endedAt)) continue;
    if (span.kind === "gap") return { utc: at, span };
    const recording = span.recording;
    const offsetNs = atNs - utcNs(recording.startedAt);
    return {
      utc: at,
      span,
      recording,
      recordingOffsetNs: offsetNs,
      segmentIndex: precedingSegmentIndex(recording.segments, offsetNs),
    };
  }
  return undefined;
}
