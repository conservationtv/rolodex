import type { ArchiveAuthentication, RequestOptions } from "./storage.js";

/** Borrowed credential issuer. Requests must return new credentials before the old session expires. */
export interface ArchiveCredentialProvider {
  get(options?: RequestOptions): Promise<ArchiveAuthentication>;
}

/** Credential renewal is shared by signing and document reads, independently of document freshness. */
export class ArchiveCredentials {
  private value: ArchiveAuthentication | undefined = undefined;
  private pending: Promise<ArchiveAuthentication> | undefined = undefined;
  private refreshAt = 0;
  private lifetime = new AbortController();
  private readonly source: ArchiveAuthentication | ArchiveCredentialProvider;
  constructor(source: ArchiveAuthentication | ArchiveCredentialProvider) {
    this.source = source;
  }

  get(): Promise<ArchiveAuthentication> {
    this.lifetime.signal.throwIfAborted();
    if (!("get" in this.source)) return Promise.resolve(this.source);
    if (this.value && Date.now() < this.refreshAt) return Promise.resolve(this.value);
    if (this.pending) return this.pending;
    this.pending = this.source
      .get({ signal: this.lifetime.signal })
      .then((auth) => {
        this.lifetime.signal.throwIfAborted();
        this.refreshAt = renewalTime(auth);
        this.value = auth;
        return auth;
      })
      .finally(() => {
        this.pending = undefined;
      });
    return this.pending;
  }

  dispose(): void {
    this.lifetime.abort();
    this.value = undefined;
  }
}

function renewalTime(auth: ArchiveAuthentication): number {
  if (auth.type !== "temporary_credentials") return Infinity;
  const expiresAt = Date.parse(auth.expiresAt ?? "");
  const now = Date.now();
  if (!Number.isFinite(expiresAt) || expiresAt <= now)
    throw new Error("Credential provider returned expired credentials or no expiry");
  return expiresAt - Math.min(120_000, (expiresAt - now) * 0.2);
}
