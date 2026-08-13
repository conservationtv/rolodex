import { describe, expect, it, vi } from "vitest";
import { z } from "zod";

import {
  ArchiveManifestClient,
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
  version: 1,
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

  async getInit(init: string): Promise<Source> {
    return { url: `https://media.test/${init}` };
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
  it("presigns media URLs against the private bucket", async () => {
    const startedAt = Date.now();
    const storage = new S3ArchiveStorage(createArchiveSource({
      type: "credentials",
      accessKeyId: "AKIA_TEST",
      secretAccessKey: "test-secret",
    }));
    const { url, expiresAt } = await storage.getSource(
      "recordings/h265/cam/2026-08-13_12-00-00.mp4",
    );
    const parsed = new URL(url);
    expect(parsed.host).toBe("account.r2.cloudflarestorage.com");
    expect(parsed.pathname).toBe(
      "/archive/recordings/h265/cam/2026-08-13_12-00-00.mp4",
    );
    expect(parsed.searchParams.get("X-Amz-Signature")).toBeTruthy();
    expect(parsed.searchParams.get("X-Amz-Expires")).toBe("3600");
    expect(Date.parse(expiresAt ?? "")).toBeGreaterThanOrEqual(startedAt + 3_599_000);
  });

  it("clamps the signing lifetime to temporary credential expiry", async () => {
    const credentialExpiresAt = new Date(Date.now() + 600_000).toISOString();
    const storage = new S3ArchiveStorage(createArchiveSource({
      type: "temporary_credentials",
      accessKeyId: "AKIA_TEST",
      secretAccessKey: "test-secret",
      sessionToken: "session-token",
      expiresAt: credentialExpiresAt,
    }));
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

  it("emits a discontinuity and a new map at recording boundaries", async () => {
    const { client } = createClient();
    const factory = new PlaylistFactory(client);
    const playlist = await factory.compile({
      at: "2026-08-13T12:09:00.000Z",
      window: { before: 0, after: 43_000 },
    });

    const lines = playlist.text.split("\n");
    expect(lines.filter((line) => line === "#EXT-X-DISCONTINUITY")).toHaveLength(1);
    expect(lines.filter((line) => line.startsWith("#EXT-X-MAP:"))).toHaveLength(2);
    /* The window anchors at the preceding random-access segment (8s). */
    expect(playlist.startedAt).toBe("2026-08-13T12:00:08.000Z");
    expect(playlist.endedAt).toBe("2026-08-14T00:05:00.000Z");
    /* Media time is contiguous across the 11h45m archive gap. */
    expect(playlist.mediaTimeAt("2026-08-13T23:55:00.000Z")).toBe(592);
    expect(playlist.utcAt(592)).toBe("2026-08-13T23:55:00.000Z");
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
