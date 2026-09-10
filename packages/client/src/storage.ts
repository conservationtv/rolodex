import { GetObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import { z } from "zod";
import { ArchiveCredentials, type ArchiveCredentialProvider } from "./credentials.js";
import { ArchiveDocumentCache } from "./document-cache.js";
import type { DocumentResponse } from "./document-cache.js";
import type { ArchiveDocument } from "./manifest.js";

import {
  DailyArchiveManifestSchema,
  StreamArchiveManifestSchema,
  type DailyArchiveManifest,
  type ISODateString,
  type StreamArchiveManifest,
} from "./manifest.js";
import { SIGNED_URL_LIFETIME_SECONDS } from "./time.js";

const TransportResponseSchema = z.object({ headers: z.record(z.string(), z.string()) });
export type TransportResponse = z.output<typeof TransportResponseSchema>;
const TransportErrorSchema = z.object({
  $metadata: z.object({ httpStatusCode: z.number() }),
  $response: TransportResponseSchema.optional(),
});

export type TransportError = z.output<typeof TransportErrorSchema>;

/* Authentication and object access */

export interface CredentialsAuthentication {
  type: "credentials";
  accessKeyId: string;
  secretAccessKey: string;
}

export interface TemporaryCredentialsAuthentication {
  type: "temporary_credentials";
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken: string;
  expiresAt?: ISODateString;
}

export type ArchiveAuthentication = CredentialsAuthentication | TemporaryCredentialsAuthentication;

export interface ArchiveSource {
  bucket: string;
  root: string;
  manifest: string;
  region: string;
  endpoint: string;
  /**
   * Public base URL for immutable media, init, and thumbnail objects.
   * When omitted, object URLs are presigned against `endpoint`.
   */
  publicBaseUrl?: string;
  auth: ArchiveAuthentication | ArchiveCredentialProvider;
}

export interface Source {
  url: string;
  /** Present when the URL is temporary and must be resolved again after this instant. */
  expiresAt?: ISODateString;
}

export interface RequestOptions {
  signal?: AbortSignal;
}

export interface ArchiveStorage {
  getArchiveObject?(key: string, options?: RequestOptions): Promise<ArchiveDocument>;
  getManifest(options?: RequestOptions): Promise<StreamArchiveManifest>;
  getDailyManifest(key: string, options?: RequestOptions): Promise<DailyArchiveManifest>;

  getSource(source: string, options?: RequestOptions): Promise<Source>;
  getThumbnail(thumbnail: string, options?: RequestOptions): Promise<Source>;
}

export function normalizeKey(key: string): string {
  return key.replace(/^\/+/, "");
}

export function publicObjectUrl(baseUrl: string, key: string): string {
  const root = new URL(baseUrl);
  root.search = "";
  root.hash = "";
  root.pathname = `${root.pathname.replace(/\/?$/, "/")}${normalizeKey(key)
    .split("/")
    .map(encodeURIComponent)
    .join("/")}`;
  return root.href;
}

export interface SigningCredentials {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
  expiration?: Date;
}

export function signingCredentials(auth: ArchiveAuthentication): SigningCredentials {
  const credentials: SigningCredentials = {
    accessKeyId: auth.accessKeyId,
    secretAccessKey: auth.secretAccessKey,
  };
  if (auth.type === "temporary_credentials") {
    credentials.sessionToken = auth.sessionToken;
    if (auth.expiresAt) credentials.expiration = new Date(auth.expiresAt);
  }
  return credentials;
}

export class S3ArchiveStorage implements ArchiveStorage {
  private client: S3Client;
  private source: ArchiveSource;
  private cache: ArchiveDocumentCache;
  private credentials: ArchiveCredentials;

  constructor(source: ArchiveSource) {
    this.source = source;
    this.credentials = new ArchiveCredentials(source.auth);
    this.cache = new ArchiveDocumentCache((key, etag, signal) =>
      this.loadDocument(key, etag, signal),
    );
    this.client = new S3Client({
      region: this.source.region,
      endpoint: this.source.endpoint,
      forcePathStyle: true,
      credentials: async () => signingCredentials(await this.credentials.get()),
    });
  }

  private async loadDocument(
    key: string,
    etag: string | undefined,
    signal?: AbortSignal,
  ): Promise<DocumentResponse> {
    const command = new GetObjectCommand({
      Bucket: this.source.bucket,
      Key: normalizeKey(key),
      IfNoneMatch: etag,
    });
    let headers = new Headers();
    // Preserve the actual Date/Age headers, including endpoints fronted by an HTTP cache.
    command.middlewareStack.add(
      (next) => async (args) => {
        const result = await next(args);
        headers = new Headers(TransportResponseSchema.parse(result.response).headers);
        return result;
      },
      { step: "deserialize", name: "archiveCacheMetadata" },
    );
    try {
      const response = await this.client.send(command, { abortSignal: signal });
      return { status: 200, headers, body: await response.Body?.transformToString() };
    } catch (cause) {
      const error = TransportErrorSchema.safeParse(cause);
      if (error.success && [304, 404].includes(error.data.$metadata.httpStatusCode ?? 0)) {
        return {
          status: error.data.$metadata.httpStatusCode ?? 404,
          headers: new Headers(error.data.$response?.headers),
        };
      }
      throw cause;
    }
  }

  /* Manifests are read through the authenticated S3 API. Immutable playback
   * objects can use a public R2 custom domain so their stable URLs are eligible
   * for edge caching; private buckets fall back to SigV4 presigned GET URLs.
   * Range requests work with either form. */
  private async resolve(key: string): Promise<Source> {
    if (this.source.publicBaseUrl) {
      return { url: publicObjectUrl(this.source.publicBaseUrl, key) };
    }
    const command = new GetObjectCommand({
      Bucket: this.source.bucket,
      Key: normalizeKey(key),
    });
    const auth = await this.credentials.get();
    const lifetimeSeconds = this.signedUrlLifetimeSeconds(auth);
    const expiresAt = new Date(Date.now() + lifetimeSeconds * 1000).toISOString();
    const url = await getSignedUrl(this.client, command, {
      expiresIn: lifetimeSeconds,
    });
    return { url, expiresAt };
  }

  /* A presigned URL cannot outlive temporary credentials. */
  private signedUrlLifetimeSeconds(auth: ArchiveAuthentication): number {
    if (auth.type !== "temporary_credentials" || !auth.expiresAt) {
      return SIGNED_URL_LIFETIME_SECONDS;
    }
    const remainingSeconds = Math.floor((Date.parse(auth.expiresAt) - Date.now()) / 1000);
    if (!Number.isFinite(remainingSeconds) || remainingSeconds < 1)
      throw new Error("Archive credentials have expired");
    return Math.min(SIGNED_URL_LIFETIME_SECONDS, remainingSeconds);
  }

  async getManifest(options?: RequestOptions): Promise<StreamArchiveManifest> {
    const value = await this.cache.read(this.source.manifest, options?.signal);
    return "days" in value ? value : StreamArchiveManifestSchema.parse(value);
  }

  async getDailyManifest(key: string, options?: RequestOptions): Promise<DailyArchiveManifest> {
    const value = await this.cache.read(key, options?.signal);
    return "recordings" in value ? value : DailyArchiveManifestSchema.parse(value);
  }

  getArchiveObject(key: string, options?: RequestOptions): Promise<ArchiveDocument> {
    return this.cache.read(key, options?.signal);
  }

  dispose(): void {
    this.credentials.dispose();
    this.client.destroy();
    this.cache.clear();
  }

  async getSource(source: string, _options?: RequestOptions): Promise<Source> {
    return this.resolve(source);
  }

  async getThumbnail(thumbnail: string, _options?: RequestOptions): Promise<Source> {
    return this.resolve(thumbnail);
  }
}
