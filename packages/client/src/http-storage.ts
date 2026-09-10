import { ArchiveDocumentCache } from "./document-cache.js";
import { DailyArchiveManifestSchema, StreamArchiveManifestSchema } from "./manifest.js";
import type { ArchiveStorage, RequestOptions } from "./storage.js";

export interface HttpArchiveStorageOptions {
  baseUrl: string;
  manifestKey?: string;
  fetch?: typeof globalThis.fetch;
  maximumCachedDocuments?: number;
}

/** HTTP object access with bounded parsed-document caching and conditional revalidation. */
export class HttpArchiveStorage implements ArchiveStorage {
  readonly base: string;
  private manifestKey: string;
  private cache: ArchiveDocumentCache;

  constructor(config: HttpArchiveStorageOptions) {
    const base = new URL(config.baseUrl);
    if (!["http:", "https:"].includes(base.protocol) || base.search || base.hash)
      throw new Error("Archive base URL must be HTTP(S), without a query or fragment");
    this.base = base.href.replace(/\/$/, "");
    this.manifestKey = config.manifestKey ?? "archive.json";
    const fetcher = config.fetch ?? globalThis.fetch.bind(globalThis);
    this.cache = new ArchiveDocumentCache(async (key, etag, signal) => {
      const headers = new Headers();
      if (etag) headers.set("If-None-Match", etag);
      // Any browser/shared-cache hit still passes through Date/Age freshness accounting.
      const response = await fetcher(this.url(key), { signal, headers, cache: "default" });
      return {
        status: response.status,
        headers: response.headers,
        body: response.status === 200 ? await response.text() : undefined,
      };
    }, config.maximumCachedDocuments);
  }
  url(key: string): string {
    if (key.split("/").some((part) => part === "." || part === ".."))
      throw new Error("Archive keys must not contain dot path segments");
    return `${this.base}/${key.split("/").map(encodeURIComponent).join("/")}`;
  }
  async getManifest(options?: RequestOptions) {
    const document = await this.cache.read(this.manifestKey, options?.signal);
    // Validate the kind without cloning the cached value or its freshness metadata.
    if (!("days" in document)) return StreamArchiveManifestSchema.parse(document);
    return document;
  }
  async getDailyManifest(key: string, options?: RequestOptions) {
    const document = await this.cache.read(key, options?.signal);
    if (!("recordings" in document)) return DailyArchiveManifestSchema.parse(document);
    return document;
  }
  getArchiveObject(key: string, options?: RequestOptions) {
    return this.cache.read(key, options?.signal);
  }
  async getSource(key: string) {
    return { url: this.url(key) };
  }
  async getThumbnail(key: string) {
    return this.getSource(key);
  }
  dispose(): void {
    this.cache.clear();
  }
}
