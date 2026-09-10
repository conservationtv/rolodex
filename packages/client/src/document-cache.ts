import { ArchiveDocumentSchema, type ArchiveDocument } from "./manifest.js";

export interface DocumentResponse {
  status: number;
  headers: Headers;
  body?: string;
}
export type DocumentLoader = (
  key: string,
  etag: string | undefined,
  signal?: AbortSignal,
) => Promise<DocumentResponse>;
export class ArchiveObjectNotFoundError extends Error {
  readonly key: string;
  constructor(key: string) {
    super(`${key}: HTTP 404`);
    this.name = "ArchiveObjectNotFoundError";
    this.key = key;
  }
}

interface Entry {
  document: ArchiveDocument;
  headers: Headers;
  expiresAt: number;
}
const freshness = new WeakMap<ArchiveDocument, number>();

/** Metadata follows the parsed value without adding fields to the wire schema. */
export function documentFreshUntil(document: ArchiveDocument, fallback: number): number {
  return freshness.get(document) ?? fallback;
}

/** RFC 9111 private-cache lifetime: s-maxage is deliberately not used. */
function lifetime(headers: Headers, fallback: number): number {
  const control = headers.get("cache-control") ?? "";
  if (/(?:^|,)\s*(?:no-store|no-cache)(?:\s|,|=|$)/i.test(control)) return 0;
  const maxAge = /(?:^|,)\s*max-age\s*=\s*"?(\d+)"?(?:\s*,|\s*$)/i.exec(control);
  if (maxAge) return Number(maxAge[1]) * 1000;
  if (/max-age/i.test(control)) return 0;
  const expires = Date.parse(headers.get("expires") ?? "");
  const date = Date.parse(headers.get("date") ?? "");
  return Number.isFinite(expires)
    ? Math.max(0, expires - (Number.isFinite(date) ? date : Date.now()))
    : fallback;
}
function expiresAt(
  headers: Headers,
  requestedAt: number,
  receivedAt: number,
  fallback: number,
): number {
  const date = Date.parse(headers.get("date") ?? "");
  const apparentAge = Number.isFinite(date) ? Math.max(0, receivedAt - date) : 0;
  const age = Number(headers.get("age") ?? "0");
  const correctedAge =
    Number.isFinite(age) && age >= 0 ? age * 1000 + receivedAt - requestedAt : Infinity;
  return (
    receivedAt + Math.max(0, lifetime(headers, fallback) - Math.max(apparentAge, correctedAge))
  );
}

/** Bounded, validator-aware document cache, shared by HTTP and authenticated S3 readers. */
export class ArchiveDocumentCache {
  private entries = new Map<string, Entry>();
  private loader: DocumentLoader;
  private maximumEntries: number;
  private fallbackMaxAge: number;
  constructor(loader: DocumentLoader, maximumEntries = 128, fallbackMaxAge = 30_000) {
    this.loader = loader;
    this.maximumEntries = Math.max(1, maximumEntries);
    this.fallbackMaxAge = fallbackMaxAge;
  }

  async read(key: string, signal?: AbortSignal): Promise<ArchiveDocument> {
    signal?.throwIfAborted();
    const cached = this.entries.get(key);
    if (cached && cached.expiresAt > Date.now()) {
      this.entries.delete(key);
      this.entries.set(key, cached);
      return cached.document;
    }
    const requestedAt = Date.now();
    const response = await this.loader(key, cached?.headers.get("etag") ?? undefined, signal);
    const receivedAt = Date.now();
    const entry = this.decode(key, response, cached);
    entry.expiresAt = expiresAt(entry.headers, requestedAt, receivedAt, this.fallbackMaxAge);
    freshness.set(entry.document, entry.expiresAt);
    this.entries.delete(key);
    if (!/(?:^|,)\s*no-store(?:\s|,|$)/i.test(entry.headers.get("cache-control") ?? ""))
      this.entries.set(key, entry);
    while (this.entries.size > this.maximumEntries) {
      const oldest = this.entries.keys().next().value;
      if (oldest === undefined) break;
      this.entries.delete(oldest);
    }
    return entry.document;
  }

  clear(): void {
    this.entries.clear();
  }

  private decode(key: string, response: DocumentResponse, cached: Entry | undefined): Entry {
    if (response.status === 304) {
      if (!cached) throw new Error(`${key}: 304 without a cached representation`);
      const headers = new Headers(cached.headers);
      // Age and Date describe this validation, never the prior response.
      headers.delete("age");
      headers.delete("date");
      response.headers.forEach((value, name) => headers.set(name, value));
      return { document: cached.document, headers, expiresAt: 0 };
    }
    if (response.status === 404) throw new ArchiveObjectNotFoundError(key);
    if (response.status !== 200 || response.body === undefined)
      throw new Error(`${key}: HTTP ${response.status}`);
    return {
      document: ArchiveDocumentSchema.parse(JSON.parse(response.body)),
      headers: response.headers,
      expiresAt: 0,
    };
  }
}
