import { z } from "zod";

import {
  IsoDateStringSchema,
} from "./time.js";

/* Manifest validation */

export const DecimalStringSchema = z.string().regex(/^\d+$/, "expected a decimal integer string");

export const DecimalNanosecondsSchema = z.codec(
  DecimalStringSchema,
  z.bigint().nonnegative(),
  {
    decode: BigInt,
    encode: (value) => value.toString(),
  },
);

export const DailyEntrySchema = z.object({
  key: z.string().min(1),
  startedAt: IsoDateStringSchema,
  endedAt: IsoDateStringSchema,
});

export const StreamArchiveManifestSchema = z.object({
  version: z.number().int().refine((value) => value === 1, "unsupported stream archive version"),
  stream: z.string().min(1),
  days: z.record(z.string(), DailyEntrySchema),
});

export const ArchiveSegmentSchema = z.object({
  presentationStartNs: DecimalNanosecondsSchema,
  byteOffset: z.number().int().nonnegative(),
  byteLength: z.number().int().positive(),
});

export const ThumbnailCellSchema = z.object({
  width: z.number().int().positive(),
  height: z.number().int().positive(),
});

export const ThumbnailTrackSchema = z.object({
  key: z.string().min(1),
  intervalNs: DecimalNanosecondsSchema.optional(),
  cell: ThumbnailCellSchema,
  columns: z.number().int().positive(),
  presentationNs: z.array(DecimalNanosecondsSchema),
});

export const ArchiveRecordingSchema = z.object({
  id: z.string().min(1),
  startedAt: IsoDateStringSchema,
  endedAt: IsoDateStringSchema,
  durationNs: DecimalNanosecondsSchema,
  source: z.string().min(1),
  init: z.string().min(1),
  segments: z.array(ArchiveSegmentSchema),
  thumbnail: ThumbnailTrackSchema.optional(),
});

export const DailyArchiveManifestSchema = z.object({
  version: z.number().int().refine((value) => value === 1, "unsupported daily archive version"),
  stream: z.string().min(1),
  date: z.string().regex(/^\d{4}-\d{2}-\d{2}$/, "expected a YYYY-MM-DD date"),
  recordings: z.array(ArchiveRecordingSchema),
});

export type DecimalString = z.output<typeof DecimalStringSchema>;
export type DecimalNanoseconds = z.output<typeof DecimalNanosecondsSchema>;
export type DailyEntry = z.output<typeof DailyEntrySchema>;
export type StreamArchiveManifest = z.output<typeof StreamArchiveManifestSchema>;
export type ArchiveSegment = z.output<typeof ArchiveSegmentSchema>;
export type ThumbnailCell = z.output<typeof ThumbnailCellSchema>;
export type ThumbnailTrack = z.output<typeof ThumbnailTrackSchema>;
export type ArchiveRecording = z.output<typeof ArchiveRecordingSchema>;
export type DailyArchiveManifest = z.output<typeof DailyArchiveManifestSchema>;

export { IsoDateStringSchema } from "./time.js";
export type { ISODateRange, ISODateString } from "./time.js";
