import { z } from "zod";

/* Time helpers. Wall-clock values are fixed-width ISO 8601 UTC with
 * millisecond precision; media-relative values stay decimal nanoseconds so
 * playlist calculations do not accumulate floating-point error. */

export const NS_PER_MS = 1_000_000n;
export const NS_PER_SECOND = 1_000_000_000n;
/* Coverage slivers below this threshold are treated as continuous. */
export const MINIMUM_GAP_NS = 250_000_000n;
/* Recordings never exceed one UTC day shard on either side of an instant. */
export const LOCATE_PADDING_NS = 24n * 60n * 60n * NS_PER_SECOND;
/* Damaged-media recovery skips just past the failed sample and is
 * rate-limited so an error storm cannot trigger a seek loop. */
export const MEDIA_RECOVERY_SKIP_NS = 250_000_000n;
export const MEDIA_RECOVERY_COOLDOWN_MS = 250;
/* Lifetime of presigned object URLs handed to the browser. */
export const SIGNED_URL_LIFETIME_SECONDS = 60 * 60;

export const IsoDateStringSchema = z.string().refine(
  (value) => Number.isFinite(Date.parse(value)),
  { message: "expected an ISO 8601 UTC timestamp" },
);

export type ISODateString = z.output<typeof IsoDateStringSchema>;

export const UtcNanosecondsSchema = z.codec(
  IsoDateStringSchema,
  z.bigint(),
  {
    decode: (value) => BigInt(Date.parse(value)) * NS_PER_MS,
    encode: (value) => new Date(Number(value / NS_PER_MS)).toISOString(),
  },
);

export type UtcNanoseconds = z.output<typeof UtcNanosecondsSchema>;

export interface ISODateRange {
  startedAt: ISODateString;
  endedAt: ISODateString;
}

export function utcNs(value: ISODateString): UtcNanoseconds {
  return z.decode(UtcNanosecondsSchema, value);
}

export function isoFromNs(value: UtcNanoseconds): ISODateString {
  return z.encode(UtcNanosecondsSchema, value);
}

export function secondsFromNs(value: bigint): number {
  return Number(value) / 1e9;
}

export function nsFromSeconds(value: number): bigint {
  if (!Number.isFinite(value)) throw new Error(`Invalid duration in seconds: ${value}`);
  return BigInt(Math.round(value * 1e9));
}

export function decimalSeconds(value: bigint): string {
  const seconds = value / NS_PER_SECOND;
  const fraction = value % NS_PER_SECOND;
  if (fraction === 0n) return `${seconds}.0`;
  return `${seconds}.${fraction.toString().padStart(9, "0").replace(/0+$/, "")}`;
}
