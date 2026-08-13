import { GetObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import { z } from "zod";

import {
  DailyArchiveManifestSchema,
  StreamArchiveManifestSchema,
  type DailyArchiveManifest,
  type ISODateString,
  type StreamArchiveManifest,
} from "./manifest.js";
import { SIGNED_URL_LIFETIME_SECONDS } from "./time.js";

/* Authentication and object access */

export interface CredentialsAuthentication {
  type: 'credentials';
  accessKeyId: string;
  secretAccessKey: string;
}

export interface TemporaryCredentialsAuthentication {
  type: 'temporary_credentials';
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
  auth: ArchiveAuthentication;
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
  getManifest(options?: RequestOptions): Promise<StreamArchiveManifest>;
  getDailyManifest(key: string, options?: RequestOptions): Promise<DailyArchiveManifest>;

  getSource(source: string, options?: RequestOptions): Promise<Source>;
  getInit(init: string, options?: RequestOptions): Promise<Source>;
  getThumbnail(thumbnail: string, options?: RequestOptions): Promise<Source>;
}

export function normalizeKey(key: string): string {
  return key.replace(/^\/+/, "");
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

  constructor(source: ArchiveSource) {
    this.source = source;
    this.client = new S3Client({
      region: this.source.region,
      endpoint: this.source.endpoint,
      forcePathStyle: true,
      credentials: signingCredentials(this.source.auth),
    });
  }

  private async getObject<Output>(
    key: string,
    schema: z.ZodType<Output>,
    options?: RequestOptions,
  ): Promise<Output> {
    const command = new GetObjectCommand({
      Bucket: this.source.bucket,
      Key: normalizeKey(key),
    });
    const response = await this.client.send(
      command,
      options?.signal ? { abortSignal: options.signal } : {},
    );
    if (!response.Body) throw new Error(`${key}: object has no body`);
    return schema.parse(JSON.parse(await response.Body.transformToString()));
  }

  /* The bucket is fully private. Manifests are read through the S3 API with
   * SDK credentials; media, init, and thumbnail objects are exposed to the
   * browser as SigV4 presigned GET URLs. Range requests remain possible
   * because presigned GETs only sign the host header, so the player can
   * still fetch playlist byte ranges directly. */
  private async resolve(key: string): Promise<Source> {
    const command = new GetObjectCommand({
      Bucket: this.source.bucket,
      Key: normalizeKey(key),
    });
    const lifetimeSeconds = this.signedUrlLifetimeSeconds();
    const expiresAt = new Date(Date.now() + lifetimeSeconds * 1000).toISOString();
    const url = await getSignedUrl(this.client, command, {
      expiresIn: lifetimeSeconds,
    });
    return { url, expiresAt };
  }

  /* A presigned URL cannot outlive temporary credentials. */
  private signedUrlLifetimeSeconds(): number {
    const auth = this.source.auth;
    if (auth.type !== "temporary_credentials" || !auth.expiresAt) {
      return SIGNED_URL_LIFETIME_SECONDS;
    }
    const remainingSeconds = Math.floor((Date.parse(auth.expiresAt) - Date.now()) / 1000);
    return Math.max(1, Math.min(SIGNED_URL_LIFETIME_SECONDS, remainingSeconds));
  }

  async getManifest(options?: RequestOptions): Promise<StreamArchiveManifest> {
    return this.getObject(this.source.manifest, StreamArchiveManifestSchema, options);
  }

  async getDailyManifest(key: string, options?: RequestOptions): Promise<DailyArchiveManifest> {
    return this.getObject(key, DailyArchiveManifestSchema, options);
  }

  async getSource(source: string, _options?: RequestOptions): Promise<Source> {
    return this.resolve(source);
  }

  async getInit(init: string, _options?: RequestOptions): Promise<Source> {
    return this.resolve(init);
  }

  async getThumbnail(thumbnail: string, _options?: RequestOptions): Promise<Source> {
    return this.resolve(thumbnail);
  }
}
