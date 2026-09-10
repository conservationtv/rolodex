import { createServer } from "node:http";
import Hls from "hls.js";
import { assemblePieces, createPieceLoader } from "./media-pieces.js";
import { describe, expect, it, vi } from "vitest";
import { z } from "zod";

import {
  ArchiveManifestClient,
  ArchiveDocumentCache,
  HttpArchiveStorage,
  ArchivePlayerStore,
  PlaylistFactory,
  S3ArchiveStorage,
  type ArchiveManifestClientOptions,
  type ArchiveSource,
  type ArchiveStorage,
  type DailyArchiveManifest,
  type Source,
  type StreamArchiveManifest,
} from "./index.js";
import { DailyArchiveManifestSchema } from "./manifest.js";

const streamManifest: StreamArchiveManifest = {
  version: 2,
  stream: "h265/cam",
  days: {
    "2026-08-01": {
      key: "artifacts/h265/cam/archive/2026-08-01.json",
      startedAt: "2026-08-01T00:00:00.000Z",
      endedAt: "2026-08-02T00:00:00.000Z",
    },
    "2026-08-13": {
      key: "artifacts/h265/cam/archive/2026-08-13.json",
      startedAt: "2026-08-13T00:00:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
    },
  },
};

const dailyManifest: DailyArchiveManifest = {
  version: 1,
  stream: "h265/cam",
  date: "2026-08-13",
  recordings: [
    {
      id: "recordings/h265/cam/2026-08-13_12-00-00.mp4",
      startedAt: "2026-08-13T12:00:00.000Z",
      endedAt: "2026-08-13T12:10:00.000Z",
      durationNs: 600000000000n,
      source: "recordings/h265/cam/2026-08-13_12-00-00.mp4",
      init: "init/7ad44f2357b9a8230df09e829a4a34676b289b8d7d320ca998e0c65c1aa0ef31.mp4",
      segments: [
        { presentationStartNs: 0n, byteOffset: 1042, byteLength: 489201 },
        { presentationStartNs: 4000000000n, byteOffset: 490243, byteLength: 501882 },
        { presentationStartNs: 8000000000n, byteOffset: 992125, byteLength: 476113 },
      ],
      thumbnail: {
        key: "artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg",
        intervalNs: 60000000000n,
        cell: { width: 240, height: 136 },
        columns: 4,
        presentationNs: [
          0n,
          60033333333n,
          120033333333n,
          180000000000n,
          240033333333n,
          300033333333n,
          360000000000n,
          420033333333n,
          480000000000n,
          540033333333n,
        ],
      },
    },
    {
      id: "recordings/h265/cam/2026-08-13_23-55-00.mp4",
      startedAt: "2026-08-13T23:55:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
      durationNs: 600000000000n,
      source: "recordings/h265/cam/2026-08-13_23-55-00.mp4",
      init: "init/7ad44f2357b9a8230df09e829a4a34676b289b8d7d320ca998e0c65c1aa0ef31.mp4",
      segments: [
        { presentationStartNs: 0n, byteOffset: 1038, byteLength: 495104 },
        { presentationStartNs: 4000000000n, byteOffset: 496142, byteLength: 487991 },
      ],
    },
  ],
};

describe("archive manifest codecs", () => {
  it("round-trips nanoseconds as decimal JSON strings", () => {
    const wire = z.encode(DailyArchiveManifestSchema, dailyManifest);
    expect(wire.recordings[0]?.durationNs).toBe("600000000000");
    expect(wire.recordings[0]?.segments[1]?.presentationStartNs).toBe("4000000000");
    expect(wire.recordings[0]?.thumbnail?.presentationNs[1]).toBe("60033333333");

    const decoded = z.decode(DailyArchiveManifestSchema, wire);
    expect(decoded.recordings[0]?.durationNs).toBe(600000000000n);
    expect(decoded.recordings[0]?.segments[1]?.presentationStartNs).toBe(4000000000n);
    expect(decoded.recordings[0]?.thumbnail?.presentationNs[1]).toBe(60033333333n);
  });
});

class MemoryArchiveStorage implements ArchiveStorage {
  manifestFetches = 0;
  dailyFetches: string[] = [];
  thumbnailFetches: string[] = [];
  thumbnailExpiresAt: string | undefined;
  daily = structuredClone(dailyManifest);

  async getManifest(): Promise<StreamArchiveManifest> {
    this.manifestFetches += 1;
    return structuredClone(streamManifest);
  }

  async getDailyManifest(key: string): Promise<DailyArchiveManifest> {
    this.dailyFetches.push(key);
    if (key === "artifacts/h265/cam/archive/2026-08-13.json") return structuredClone(this.daily);
    throw new Error(`Unexpected daily manifest request: ${key}`);
  }

  async getSource(source: string): Promise<Source> {
    return { url: `https://media.test/${source}` };
  }

  async getThumbnail(thumbnail: string): Promise<Source> {
    this.thumbnailFetches.push(thumbnail);
    const source: Source = { url: `https://media.test/${thumbnail}` };
    if (this.thumbnailExpiresAt !== undefined) source.expiresAt = this.thumbnailExpiresAt;
    return source;
  }
}

function createClient(options: ArchiveManifestClientOptions = {}) {
  const storage = new MemoryArchiveStorage();
  return { storage, client: new ArchiveManifestClient(storage, options) };
}

function createArchiveSource(auth: ArchiveSource["auth"]): ArchiveSource {
  return {
    bucket: "archive",
    root: "https://media.example.com/",
    manifest: "/artifacts/h265/cam/archive.json",
    region: "auto",
    endpoint: "https://account.r2.cloudflarestorage.com",
    auth,
  };
}

describe("S3ArchiveStorage", () => {
  it("resolves immutable objects through a public cache domain", async () => {
    const source = {
      ...createArchiveSource({
        type: "credentials" as const,
        accessKeyId: "AKIA_TEST",
        secretAccessKey: "test-secret",
      }),
      publicBaseUrl: "https://r2.archive.experiments.jameswilliams.fyi/cache/",
    };
    const storage = new S3ArchiveStorage(source);

    await expect(storage.getSource("/recordings/cam one/clip #1.mp4")).resolves.toEqual({
      url: "https://r2.archive.experiments.jameswilliams.fyi/cache/recordings/cam%20one/clip%20%231.mp4",
    });
    await expect(storage.getSource("init/abc.mp4")).resolves.toEqual({
      url: "https://r2.archive.experiments.jameswilliams.fyi/cache/init/abc.mp4",
    });
    await expect(storage.getThumbnail("artifacts/cam/thumbnail.jpg")).resolves.toEqual({
      url: "https://r2.archive.experiments.jameswilliams.fyi/cache/artifacts/cam/thumbnail.jpg",
    });
  });

  it("presigns media URLs against the private bucket", async () => {
    const startedAt = Date.now();
    const storage = new S3ArchiveStorage(
      createArchiveSource({
        type: "credentials",
        accessKeyId: "AKIA_TEST",
        secretAccessKey: "test-secret",
      }),
    );
    const { url, expiresAt } = await storage.getSource(
      "recordings/h265/cam/2026-08-13_12-00-00.mp4",
    );
    const parsed = new URL(url);
    expect(parsed.host).toBe("account.r2.cloudflarestorage.com");
    expect(parsed.pathname).toBe("/archive/recordings/h265/cam/2026-08-13_12-00-00.mp4");
    expect(parsed.searchParams.get("X-Amz-Signature")).toBeTruthy();
    expect(parsed.searchParams.get("X-Amz-Expires")).toBe("3600");
    expect(Date.parse(expiresAt ?? "")).toBeGreaterThanOrEqual(startedAt + 3_599_000);
  });

  it("clamps the signing lifetime to temporary credential expiry", async () => {
    const credentialExpiresAt = new Date(Date.now() + 600_000).toISOString();
    const storage = new S3ArchiveStorage(
      createArchiveSource({
        type: "temporary_credentials",
        accessKeyId: "AKIA_TEST",
        secretAccessKey: "test-secret",
        sessionToken: "session-token",
        expiresAt: credentialExpiresAt,
      }),
    );
    const { url, expiresAt } = await storage.getThumbnail(
      "artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg",
    );
    const parsed = new URL(url);
    expect(parsed.searchParams.get("X-Amz-Security-Token")).toBe("session-token");
    const expiresSeconds = Number(parsed.searchParams.get("X-Amz-Expires"));
    expect(expiresSeconds).toBeGreaterThan(0);
    expect(expiresSeconds).toBeLessThanOrEqual(600);
    expect(Date.parse(expiresAt ?? "")).toBeLessThanOrEqual(Date.parse(credentialExpiresAt));
  });
});

describe("ArchiveManifestClient", () => {
  it("loads the latest day entry", async () => {
    const { client } = createClient();
    await expect(client.load()).resolves.toEqual({
      key: "artifacts/h265/cam/archive/2026-08-13.json",
      startedAt: "2026-08-13T00:00:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
    });
  });

  it("builds a timeline with recording and gap spans", async () => {
    const { client, storage } = createClient();
    const timeline = await client.getTimeline({
      startedAt: "2026-08-13T12:00:00.000Z",
      endedAt: "2026-08-13T12:20:00.000Z",
    });
    expect(timeline.availableRange).toEqual({
      startedAt: "2026-08-01T00:00:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
    });
    expect(timeline.spans.map((span) => span.kind)).toEqual(["recording", "gap"]);
    expect(timeline.spans[1]).toMatchObject({
      startedAt: "2026-08-13T12:10:00.000Z",
      endedAt: "2026-08-13T12:20:00.000Z",
    });
    /* Only the overlapping day shard is fetched. */
    expect(storage.dailyFetches).toEqual(["artifacts/h265/cam/archive/2026-08-13.json"]);
  });

  it("clips retention coverage and seeks as UTC advances without new recordings", async () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(new Date("2026-08-13T13:05:00.000Z"));
      const { client } = createClient({ retentionSeconds: 3600 });
      const range = { startedAt: "2026-08-13T12:00:00.000Z", endedAt: "2026-08-13T12:10:00.000Z" };
      const timeline = await client.getTimeline(range);
      expect(timeline.requestedRange.startedAt).toBe("2026-08-13T12:05:00.000Z");
      expect(timeline.spans[0]?.startedAt).toBe("2026-08-13T12:05:00.000Z");
      expect((await client.locate(range.startedAt)).utc).toBe("2026-08-13T12:05:00.000Z");
      expect(await client.getThumbnail(range.startedAt)).toBeNull();
      vi.setSystemTime(new Date("2026-08-13T13:11:00.000Z"));
      expect((await client.getTimeline(range)).spans).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("caches the root manifest between calls", async () => {
    const { client, storage } = createClient();
    await client.load();
    await client.getTimeline({
      startedAt: "2026-08-13T12:00:00.000Z",
      endedAt: "2026-08-13T12:20:00.000Z",
    });
    expect(storage.manifestFetches).toBe(1);
    await client.refresh();
    expect(storage.manifestFetches).toBe(2);
  });

  it("locates an instant inside a recording", async () => {
    const { client } = createClient();
    const location = await client.locate("2026-08-13T12:05:30.000Z");
    expect(location.span.kind).toBe("recording");
    expect(location.recording?.id).toBe("recordings/h265/cam/2026-08-13_12-00-00.mp4");
    expect(location.recordingOffsetNs).toBe(330000000000n);
    expect(location.segmentIndex).toBe(2);
  });

  it("locates an instant inside a gap", async () => {
    const { client } = createClient();
    const location = await client.locate("2026-08-13T15:00:00.000Z");
    expect(location.span).toEqual({
      kind: "gap",
      startedAt: "2026-08-13T12:10:00.000Z",
      endedAt: "2026-08-13T23:55:00.000Z",
    });
    expect(location.recording).toBeUndefined();
  });

  it("produces a thumbnail cue with sprite crop geometry", async () => {
    const { client } = createClient();
    const cue = await client.getThumbnail("2026-08-13T12:05:30.000Z");
    /* Frame 5 samples at 300.033s → row-major cell (1, 1) in a 4-column sprite. */
    expect(cue).toMatchObject({
      sampledAt: "2026-08-13T12:05:00.033Z",
      crop: { x: 240, y: 136, width: 240, height: 136 },
    });
    expect(cue?.image.url).toBe(
      "https://media.test/artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg",
    );
  });

  it("reuses a cached thumbnail sprite URL across crop positions", async () => {
    const { client, storage } = createClient();
    const first = await client.getThumbnail("2026-08-13T12:01:30.000Z");
    const second = await client.getThumbnail("2026-08-13T12:05:30.000Z");

    expect(first?.crop).not.toEqual(second?.crop);
    expect(first?.image.url).toBe(second?.image.url);
    expect(storage.thumbnailFetches).toEqual([
      "artifacts/h265/cam/2026-08-13_12-00-00/thumbnail.jpg",
    ]);
  });

  it("resolves a new thumbnail URL when the cached URL is near expiry", async () => {
    const { client, storage } = createClient();
    storage.thumbnailExpiresAt = new Date(Date.now() + 1_000).toISOString();

    await client.getThumbnail("2026-08-13T12:01:30.000Z");
    await client.getThumbnail("2026-08-13T12:05:30.000Z");

    expect(storage.thumbnailFetches).toHaveLength(2);
  });

  it("evicts the least-recently-used thumbnail URL at the configured bound", async () => {
    const { client, storage } = createClient({ maximumThumbnailUrls: 1 });
    const firstRecording = storage.daily.recordings[0];
    const firstTrack = firstRecording?.thumbnail;
    if (!firstTrack) throw new Error("expected the fixture to contain a thumbnail track");
    const firstKey = firstTrack.key;

    await client.getThumbnail("2026-08-13T12:01:30.000Z");
    firstTrack.key = "artifacts/h265/cam/second-thumbnail.jpg";
    await client.refresh();
    await client.getThumbnail("2026-08-13T12:01:30.000Z");
    firstTrack.key = firstKey;
    await client.refresh();
    await client.getThumbnail("2026-08-13T12:01:30.000Z");

    expect(storage.thumbnailFetches).toEqual([
      firstKey,
      "artifacts/h265/cam/second-thumbnail.jpg",
      firstKey,
    ]);
  });

  it("returns null for recordings without a thumbnail track", async () => {
    const { client } = createClient();
    await expect(client.getThumbnail("2026-08-13T23:56:00.000Z")).resolves.toBeNull();
  });
});

describe("PlaylistFactory", () => {
  it("compiles a byte-range playlist for one recording", async () => {
    const { client } = createClient();
    const factory = new PlaylistFactory(client);
    const playlist = await factory.compile({
      at: "2026-08-13T12:00:02.000Z",
      window: { before: 0, after: 598 },
    });

    expect(playlist.startedAt).toBe("2026-08-13T12:00:00.000Z");
    expect(playlist.endedAt).toBe("2026-08-13T12:10:00.000Z");
    expect(playlist.initial).toBe(2);

    const lines = playlist.text.split("\n");
    expect(lines).toContain("#EXT-X-VERSION:7");
    expect(lines).toContain("#EXT-X-TARGETDURATION:592");
    expect(lines).toContain(
      '#EXT-X-MAP:URI="https://media.test/init/7ad44f2357b9a8230df09e829a4a34676b289b8d7d320ca998e0c65c1aa0ef31.mp4"',
    );
    expect(lines).toContain("#EXT-X-PROGRAM-DATE-TIME:2026-08-13T12:00:00.000Z");
    expect(lines.filter((line) => line.startsWith("#EXT-X-BYTERANGE:"))).toEqual([
      "#EXT-X-BYTERANGE:489201@1042",
      "#EXT-X-BYTERANGE:501882@490243",
      "#EXT-X-BYTERANGE:476113@992125",
    ]);
    expect(lines.filter((line) => line.startsWith("#EXTINF:"))).toEqual([
      "#EXTINF:4.0,",
      "#EXTINF:4.0,",
      "#EXTINF:592.0,",
    ]);
    expect(lines).not.toContain("#EXT-X-DISCONTINUITY");
    expect(lines.at(-2)).toBe("#EXT-X-ENDLIST");

    expect(playlist.utcAt(0)).toBe("2026-08-13T12:00:00.000Z");
    expect(playlist.utcAt(9)).toBe("2026-08-13T12:00:09.000Z");
    expect(playlist.mediaTimeAt("2026-08-13T12:05:00.000Z")).toBe(300);
  });

  it("stops the playlist before a real archive gap", async () => {
    const { client } = createClient();
    const factory = new PlaylistFactory(client);
    const playlist = await factory.compile({
      at: "2026-08-13T12:09:00.000Z",
      window: { before: 0, after: 43_000 },
    });

    const lines = playlist.text.split("\n");
    expect(lines).not.toContain("#EXT-X-DISCONTINUITY");
    expect(lines.filter((line) => line.startsWith("#EXT-X-MAP:"))).toHaveLength(1);
    /* The window anchors at the preceding random-access segment (8s). */
    expect(playlist.startedAt).toBe("2026-08-13T12:00:08.000Z");
    expect(playlist.endedAt).toBe("2026-08-13T12:10:00.000Z");
    expect(playlist.mediaTimeAt("2026-08-13T23:55:00.000Z")).toBe(592);
    expect(playlist.utcAt(592)).toBe("2026-08-13T12:10:00.000Z");
    expect(lines).not.toContain("https://media.test/recordings/h265/cam/2026-08-13_23-55-00.mp4");
  });

  it("hands out revocable blob URL leases", async () => {
    const { client } = createClient();
    const factory = new PlaylistFactory(client);
    const lease = await factory.create({ at: "2026-08-13T12:00:02.000Z" });
    expect(lease.url).toMatch(/^blob:/);
    expect(lease[Symbol.dispose]).toBe(lease.revoke);
    lease.revoke();
    lease.revoke(); // idempotent
    factory.dispose();
  });

  it("rejects a window with no media", async () => {
    const { client } = createClient();
    const factory = new PlaylistFactory(client);
    await expect(
      factory.compile({ at: "2026-08-13T15:00:00.000Z", window: { before: 0, after: 60 } }),
    ).rejects.toThrow(/No media is available/);
  });
});

describe("ArchivePlayerStore", () => {
  function createStore() {
    const { client } = createClient();
    const store = new ArchivePlayerStore({
      manifests: client,
      initial: {
        playhead: "2026-08-13T12:05:30.000Z",
        viewport: {
          startedAt: "2026-08-13T12:00:00.000Z",
          endedAt: "2026-08-13T12:20:00.000Z",
        },
      },
    });
    return { client, store };
  }

  it("initializes into a ready snapshot", async () => {
    const { store } = createStore();
    const notifications: number[] = [];
    store.subscribe(() => notifications.push(store.getSnapshot().revision));

    expect(store.getSnapshot().phase).toBe("idle");
    await store.initialize();

    const snapshot = store.getSnapshot();
    expect(snapshot.phase).toBe("ready");
    if (snapshot.phase !== "ready") throw new Error("expected a ready snapshot");
    expect(snapshot.stream).toBe("h265/cam");
    expect(snapshot.coverage).toEqual({
      startedAt: "2026-08-01T00:00:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
    });
    expect(snapshot.timeline?.spans.map((span) => span.kind)).toEqual(["recording", "gap"]);
    expect(snapshot.location?.recording?.id).toBe("recordings/h265/cam/2026-08-13_12-00-00.mp4");
    expect(notifications.length).toBeGreaterThan(0);
    /* The server snapshot keeps a stable identity. */
    expect(store.getServerSnapshot().revision).toBe(0);
  });

  it("resolves hover thumbnails asynchronously", async () => {
    const { store } = createStore();
    await store.initialize();

    store.setHover("2026-08-13T12:05:30.000Z", 120);
    expect(store.getSnapshot().hover).toMatchObject({
      playhead: "2026-08-13T12:05:30.000Z",
      loading: true,
    });

    await vi.waitFor(() => {
      expect(store.getSnapshot().hover?.loading).toBe(false);
    });
    expect(store.getSnapshot().hover?.thumbnail?.crop).toEqual({
      x: 240,
      y: 136,
      width: 240,
      height: 136,
    });

    store.clearHover();
    expect(store.getSnapshot().hover).toBeUndefined();
    store.dispose();
  });

  it("reuses a playlist for seeks inside its UTC window", async () => {
    const { client } = createClient();
    const playlists = new PlaylistFactory(client);
    const create = vi.spyOn(playlists, "create");
    const store = new ArchivePlayerStore({
      manifests: client,
      playlists,
      initial: {
        playhead: "2026-08-13T12:05:30.000Z",
        viewport: {
          startedAt: "2026-08-13T12:00:00.000Z",
          endedAt: "2026-08-13T12:20:00.000Z",
        },
      },
    });

    await store.seek("2026-08-13T12:05:30.000Z");
    const firstSnapshot = store.getSnapshot();
    expect(firstSnapshot.phase).toBe("ready");
    if (firstSnapshot.phase !== "ready") throw new Error("expected a ready snapshot");
    const firstUrl = firstSnapshot.active?.url;
    expect(create).toHaveBeenCalledTimes(1);

    const phases: string[] = [];
    store.subscribe(() => phases.push(store.getSnapshot().phase));
    await store.seek("2026-08-13T12:05:40.000Z");

    expect(create).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot()).toMatchObject({
      phase: "ready",
      playhead: "2026-08-13T12:05:40.000Z",
      active: { url: firstUrl },
    });
    expect(phases).not.toContain("loading-media");

    await store.seek("2026-08-13T23:56:00.000Z");
    expect(create).toHaveBeenCalledTimes(2);

    store.dispose();
    playlists.dispose();
  });

  it("enters the real gap when playback reaches a recording boundary", async () => {
    const { store } = createStore();
    await store.seek("2026-08-13T12:09:00.000Z");
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    await store.play();

    /* Browsers dispatch pause before ended. That event may update the public
     * snapshot, but it must not erase the intent to continue across a gap. */
    // @ts-expect-error Simulate the pause listener without coupling this test to an HLS media element.
    store.mutate((current) => ({
      ...current,
      playback: { ...current.playback, playing: false },
    }));

    // @ts-expect-error The private handler is exercised as the media element's ended event boundary.
    await store.handleEnded();

    const snapshot = store.getSnapshot();
    expect(snapshot.phase).toBe("gap");
    if (snapshot.phase !== "gap") throw new Error("expected a gap snapshot");
    expect(snapshot).toMatchObject({
      phase: "gap",
      playhead: "2026-08-13T12:10:00.000Z",
      playback: { playing: true },
      location: {
        span: {
          kind: "gap",
          startedAt: "2026-08-13T12:10:00.000Z",
          endedAt: "2026-08-13T23:55:00.000Z",
        },
      },
    });
    expect(snapshot.active).toBeUndefined();
    store.dispose();
    vi.unstubAllGlobals();
  });

  it("adjusts the viewport with zoom, pan, and fit", async () => {
    const { store } = createStore();
    await store.initialize();

    store.zoomAt("2026-08-13T12:10:00.000Z", 2);
    expect(store.getSnapshot().viewport).toEqual({
      startedAt: "2026-08-13T12:05:00.000Z",
      endedAt: "2026-08-13T12:15:00.000Z",
    });

    store.panBy(60);
    expect(store.getSnapshot().viewport).toEqual({
      startedAt: "2026-08-13T12:06:00.000Z",
      endedAt: "2026-08-13T12:16:00.000Z",
    });

    store.fit();
    expect(store.getSnapshot().viewport).toEqual({
      startedAt: "2026-08-01T00:00:00.000Z",
      endedAt: "2026-08-14T00:05:00.000Z",
    });
    store.dispose();
  });
});

describe("paged archive caching", () => {
  it("subtracts Age, revalidates with ETag, and retains the parsed value after 304", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-06T12:00:00Z"));
    try {
      const loader = vi.fn(async (_key: string, etag: string | undefined) => ({
        status: etag ? 304 : 200,
        headers: new Headers({
          "cache-control": "public, max-age=10",
          age: etag ? "0" : "8",
          date: new Date().toUTCString(),
          etag: '"one"',
        }),
        body: etag ? undefined : JSON.stringify(streamManifest),
      }));
      const cache = new ArchiveDocumentCache(loader);
      const first = await cache.read("archive.json");
      vi.advanceTimersByTime(1_999);
      expect(await cache.read("archive.json")).toBe(first);
      expect(loader).toHaveBeenCalledTimes(1);
      vi.advanceTimersByTime(1);
      expect(await cache.read("archive.json")).toBe(first);
      expect(loader.mock.calls[1]?.[1]).toBe('"one"');
      vi.advanceTimersByTime(9_999);
      expect(await cache.read("archive.json")).toBe(first);
      expect(loader).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not retain no-store responses or reuse no-cache without validation", async () => {
    const loader = vi.fn(async () => ({
      status: 200,
      headers: new Headers({ "cache-control": "no-store, max-age=999", etag: '"one"' }),
      body: JSON.stringify(streamManifest),
    }));
    const cache = new ArchiveDocumentCache(loader);
    await cache.read("archive.json");
    await cache.read("archive.json");
    expect(loader).toHaveBeenCalledTimes(2);
    expect(loader).toHaveBeenLastCalledWith("archive.json", undefined, undefined);
    loader.mockImplementation(async () => ({
      status: 200,
      headers: new Headers({ "cache-control": "no-cache, max-age=999", etag: '"one"' }),
      body: JSON.stringify(streamManifest),
    }));
    await cache.read("archive.json");
    await cache.read("archive.json");
    expect(loader).toHaveBeenLastCalledWith("archive.json", '"one"', undefined);
  });

  it("keeps immutable pages across refreshes and only loads pages intersecting a seek", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-06T12:00:00Z"));
    try {
      const page = z.encode(DailyArchiveManifestSchema, dailyManifest);
      const first = page.recordings[0]!;
      const last = page.recordings[1]!;
      const firstEntry = {
        key: "index.first.json",
        bucket: "12:00",
        startedAt: first.startedAt,
        endedAt: first.endedAt,
      };
      const tail = {
        key: "index.tail.json",
        bucket: "23:55",
        startedAt: last.startedAt,
        endedAt: last.endedAt,
      };
      const head = {
        version: 2,
        stream: page.stream,
        date: page.date,
        startedAt: first.startedAt,
        endedAt: last.endedAt,
        pages: "pages.json",
        tail,
      };
      const root = {
        version: 2,
        stream: page.stream,
        days: {
          [page.date]: { key: "head.json", startedAt: head.startedAt, endedAt: head.endedAt },
        },
      };
      const bodies = new Map([
        ["archive.json", JSON.stringify(root)],
        ["head.json", JSON.stringify(head)],
        [
          "pages.json",
          JSON.stringify({ version: 2, stream: page.stream, date: page.date, pages: [firstEntry] }),
        ],
        ["index.first.json", JSON.stringify({ ...page, recordings: [first] })],
        ["index.tail.json", JSON.stringify({ ...page, recordings: [last] })],
      ]);
      const requests: string[] = [];
      const revisions = new Map<string, string>();
      const storage = new HttpArchiveStorage({
        baseUrl: "https://archive.test",
        fetch: async (input, init) => {
          const key = String(input).split("/").at(-1)!;
          requests.push(key);
          const mutable = key === "archive.json" || key === "head.json";
          const headers = {
            "cache-control": key === "archive.json" ? "max-age=300" : mutable ? "max-age=5" : "max-age=31536000, immutable",
            date: new Date().toUTCString(),
            age: "0",
            etag: revisions.get(key) ?? '"one"',
          };
          const unchanged = new Headers(init?.headers).get("if-none-match") === headers.etag;
          return new Response(unchanged ? null : bodies.get(key), {
            status: unchanged ? 304 : 200,
            headers,
          });
        },
      });
      const client = new ArchiveManifestClient(storage);
      const location = await client.locate(first.startedAt);
      expect(location.recording?.id).toBe(first.id);
      expect(requests).not.toContain("index.tail.json");
      vi.advanceTimersByTime(5_000);
      await client.refresh();
      expect((await client.locate(first.startedAt)).recording).toBe(location.recording);
      expect(requests.filter((key) => key === "index.first.json")).toHaveLength(1);
      expect(requests.filter((key) => key === "pages.json")).toHaveLength(1);
      expect(requests.filter((key) => key === "head.json")).toHaveLength(2);
      expect(requests.filter((key) => key === "archive.json")).toHaveLength(1);
      await client.locate(last.startedAt);
      expect(requests.filter((key) => key === "index.tail.json")).toHaveLength(1);
      bodies.set("head.json", JSON.stringify({ ...head, endedAt: "2026-08-14T01:00:00.000Z" }));
      revisions.set("head.json", '"growth"');
      vi.advanceTimersByTime(5_000);
      expect((await client.describe()).coverage?.endedAt).toBe("2026-08-14T01:00:00.000Z");
      expect(requests.filter((key) => key === "archive.json")).toHaveLength(1);
      // Removing a page publishes a new directory; cached bytes no longer confer coverage.
      bodies.set(
        "pages.revised.json",
        JSON.stringify({ version: 2, stream: page.stream, date: page.date, pages: [] }),
      );
      bodies.set("head.json", JSON.stringify({ ...head, pages: "pages.revised.json" }));
      revisions.set("head.json", '"two"');
      vi.advanceTimersByTime(5_000);
      await client.refresh();
      const removed = await client.locate(first.startedAt);
      expect(removed.span.kind).toBe("gap");
      expect(removed.span.endedAt).toBe(last.startedAt);
      expect(requests.filter((key) => key === "index.first.json")).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
  it("discovers midnight heads without rereading a fresh root and bounds missing-head probes", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-08-13T23:59:59Z"));
    try {
      const oldHead = {
        version: 2, stream: "camera", date: "2026-08-13",
        startedAt: "2026-08-13T12:00:00Z", endedAt: "2026-08-14T00:00:00Z",
        pages: "pages.old.json",
        tail: { key: "tail.old.json", bucket: "23:55", startedAt: "2026-08-13T23:55:00Z", endedAt: "2026-08-14T00:00:00Z" },
      };
      const root = {
        version: 2, stream: "camera", headPrefix: "archive/",
        days: { "2026-08-13": { key: "old-head.json", startedAt: oldHead.startedAt, endedAt: oldHead.endedAt } },
      };
      const bodies = new Map([
        ["archive.json", JSON.stringify(root)], ["old-head.json", JSON.stringify(oldHead)],
      ]);
      const requests: string[] = [];
      const storage = new HttpArchiveStorage({ baseUrl: "https://archive.test", fetch: async input => {
        const key = new URL(String(input)).pathname.slice(1);
        requests.push(key);
        const body = bodies.get(key);
        return new Response(body ?? "Not indexed", { status: body ? 200 : 404, headers: {
          "cache-control": key === "archive.json" ? "max-age=300" : "max-age=5",
          date: new Date().toUTCString(), age: "0",
        } });
      } });
      const client = new ArchiveManifestClient(storage);
      await client.describe();
      vi.advanceTimersByTime(2000);
      await client.describe();
      await client.describe();
      expect(requests.filter(key => key === "archive/2026-08-14/head.json")).toHaveLength(1);
      bodies.set("archive/2026-08-14/head.json", JSON.stringify({ ...oldHead,
        date: "2026-08-14", startedAt: "2026-08-14T00:00:00Z", endedAt: "2026-08-14T00:00:30Z",
      }));
      vi.advanceTimersByTime(5000);
      await client.refresh();
      expect((await client.describe()).coverage?.endedAt).toBe("2026-08-14T00:00:30Z");
      expect((await client.load()).key).toBe("archive/2026-08-14/head.json");
      expect(requests.filter(key => key === "archive.json")).toHaveLength(1);
      expect(requests.filter(key => key === "archive/2026-08-14/head.json")).toHaveLength(2);
    } finally { vi.useRealTimers(); }
  });

});


describe("repaired media ranges", () => {
  it.each(["destroy", "abort"] as const)("%s respects hls.js callback-driven cleanup", (operation) => {
    // Use the real default loader's lifecycle without starting browser networking.
    vi.stubGlobal("self", globalThis);
    try {
      const Loader = createPieceLoader(new Map());
      const loader = new Loader(Hls.DefaultConfig);
      const onAbort = vi.fn(() => loader.destroy());
      Object.assign(loader, { callbacks: { onAbort } });

      expect(() => loader[operation]()).not.toThrow();
      expect(onAbort).toHaveBeenCalledTimes(operation === "abort" ? 1 : 0);
      expect(() => loader.destroy()).not.toThrow();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("compiles mixed sources as one independently playable segment", async () => {
    const {storage,client}=createClient();
    const recording=storage.daily.recordings[0]!;
    recording.segments[0]!.pieces=[
      {source:"repairs/start.mp4",byteOffset:0,byteLength:100},
      {source:recording.source,byteOffset:1142,byteLength:489101},
    ];
    storage.daily.version=3;
    const playlist=await new PlaylistFactory(client).compile({at:"2026-08-13T12:00:02.000Z",window:{before:0,after:1}});
    const entries=[...playlist.mediaSegments!.entries()];
    expect(entries).toHaveLength(1);
    expect(entries[0]![1]).toEqual([
      {url:"https://media.test/repairs/start.mp4",byteOffset:0,byteLength:100},
      {url:`https://media.test/${recording.source}`,byteOffset:1142,byteLength:489101},
    ]);
    expect(playlist.text).toContain(entries[0]![0]);
    expect(playlist.initial).toBe(2);
    expect(playlist.utcAt(3)).toBe("2026-08-13T12:00:03.000Z");
    client.dispose();
  });

  it("assembles exact HTTP ranges and rejects ignored ranges and aborted work", async () => {
    const bytes=Buffer.from("0123456789abcdef");
    const server=createServer((request,response)=>{
      if(request.url==="/ignored"){response.writeHead(200);response.end(bytes);return;}
      const match=/bytes=(\d+)-(\d+)/.exec(request.headers.range??"");
      const start=Number(match?.[1]);const end=Number(match?.[2]);
      response.writeHead(206,{"Content-Range":`bytes ${start}-${end}/${bytes.length}`});
      response.end(bytes.subarray(start,end+1));
    });
    await new Promise<void>(resolve=>server.listen(0,"127.0.0.1",resolve));
    const address=z.object({port:z.number()}).parse(server.address());
    const url=`http://127.0.0.1:${address.port}`;
    try {
      const result=await assemblePieces([{url,byteOffset:8,byteLength:4},{url,byteOffset:0,byteLength:3}],new AbortController().signal);
      expect(new TextDecoder().decode(result)).toBe("89ab012");
      await expect(assemblePieces([{url:`${url}/ignored`,byteOffset:0,byteLength:3}],new AbortController().signal)).rejects.toThrow("HTTP 200");
      const controller=new AbortController();controller.abort();
      await expect(assemblePieces([{url,byteOffset:0,byteLength:3}],controller.signal)).rejects.toThrow();
    } finally {server.closeAllConnections();await new Promise<void>(resolve=>server.close(()=>resolve()));}
  });
});
