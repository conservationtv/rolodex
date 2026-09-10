import { z } from "zod";

import { IsoDateStringSchema } from "./time.js";

/* Manifest validation */

export const DecimalStringSchema = z.string().regex(/^\d+$/, "expected a decimal integer string");

export const DecimalNanosecondsSchema = z.codec(DecimalStringSchema, z.bigint().nonnegative(), {
  decode: BigInt,
  encode: (value) => value.toString(),
});

export const DailyEntrySchema = z.object({
  key: z.string().min(1),
  startedAt: IsoDateStringSchema,
  endedAt: IsoDateStringSchema,
});

export const StreamArchiveManifestSchema = z.object({
  version: z.literal(2),
  stream: z.string().min(1),
  days: z.record(z.string(), DailyEntrySchema),
  /** Direct UTC day discovery; root coverage fields are historical hints only. */
  headPrefix: z.string().min(1).optional(),
  /** Rolling UTC retention, independent of document cache freshness. */
  retentionSeconds: z.number().int().positive().optional(),
});

export const ArchivePieceSchema = z.object({
  source: z.string().min(1),
  byteOffset: z.number().int().nonnegative(),
  byteLength: z.number().int().positive(),
});
export type ArchivePiece = z.output<typeof ArchivePieceSchema>;

/** Location discriminant shared with the Worker ArchiveSegment:
 * When pieces is present, read its ranges in order; byteOffset is zero and
 * byteLength is their sum. Otherwise read byteOffset/byteLength from the
 * recording's source. An empty pieces array is invalid on the wire.
 */
export const ArchiveSegmentSchema = z.object({
  presentationStartNs: DecimalNanosecondsSchema,
  pieces: z.array(ArchivePieceSchema).min(1).optional(),
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
  version: z
    .number()
    .int()
    .refine((value) => value === 1 || value === 3, "unsupported daily archive version"),
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

/** Immutable page directory and mutable daily head. Page bodies reuse v1's recording codec. */
export const ArchivePageEntrySchema = DailyEntrySchema.extend({ bucket: z.string() });
export const ArchivePageDirectorySchema = z.object({
  version: z.literal(2),
  stream: z.string(),
  date: z.string(),
  pages: z.array(ArchivePageEntrySchema),
});
export const ArchiveHeadSchema = z.object({
  version: z.literal(2),
  stream: z.string(),
  date: z.string(),
  startedAt: IsoDateStringSchema,
  endedAt: IsoDateStringSchema,
  pages: z.string(),
  tail: ArchivePageEntrySchema,
});
export const ArchiveDocumentSchema = z.union([
  StreamArchiveManifestSchema,
  DailyArchiveManifestSchema,
  ArchiveHeadSchema,
  ArchivePageDirectorySchema,
]);
export type ArchivePageEntry = z.output<typeof ArchivePageEntrySchema>;
export type ArchivePageDirectory = z.output<typeof ArchivePageDirectorySchema>;
export type ArchiveHead = z.output<typeof ArchiveHeadSchema>;
export type ArchiveDocument = z.output<typeof ArchiveDocumentSchema>;
