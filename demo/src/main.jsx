import {
  QueryClient,
  QueryClientProvider,
  useMutation,
  useQuery,
} from "@tanstack/react-query";
import * as React from "react";
import { createRoot } from "react-dom/client";

import {
  ArchiveManifestClient,
  ArchivePlayerStore,
  S3ArchiveStorage,
} from "@conservationtv/filofax";

import { archiveSource } from "./config.js";

const {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} = React;

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: Infinity } },
});

function message(error) {
  return error instanceof Error ? error.message : String(error);
}

function assertReady(store) {
  const snapshot = store.getSnapshot();
  if (snapshot.phase === "error") throw snapshot.error;
}

function formatTime(iso) {
  return iso ? new Date(iso).toLocaleString() : "—";
}

function useHoverThrottle(store) {
  const lastRun = useRef(-Infinity);
  const pending = useRef();
  const timer = useRef();

  const flush = useCallback(() => {
    timer.current = undefined;
    const utc = pending.current;
    pending.current = undefined;
    if (!utc) return;
    lastRun.current = performance.now();
    store.setHover(utc, 0);
  }, [store]);

  const show = useCallback((utc) => {
    pending.current = utc;
    const wait = 60 - (performance.now() - lastRun.current);
    if (wait <= 0) flush();
    else if (timer.current === undefined) timer.current = setTimeout(flush, wait);
  }, [flush]);

  const clear = useCallback(() => {
    pending.current = undefined;
    clearTimeout(timer.current);
    timer.current = undefined;
    store.clearHover();
  }, [store]);

  useEffect(() => clear, [clear]);
  return { show, clear };
}

function Thumbnail({ cue }) {
  const ref = useRef(null);

  useEffect(() => {
    if (!cue || !ref.current) return undefined;
    const canvas = ref.current;
    const image = new Image();
    let cancelled = false;
    image.crossOrigin = "anonymous";
    image.onload = () => {
      if (cancelled) return;
      const { x, y, width, height } = cue.crop;
      canvas.width = width;
      canvas.height = height;
      canvas.getContext("2d").drawImage(image, x, y, width, height, 0, 0, width, height);
    };
    image.src = cue.image.url;
    return () => {
      cancelled = true;
    };
  }, [cue]);

  return cue ? <canvas ref={ref} /> : null;
}

function clamp(value, minimum, maximum) {
  return Math.max(minimum, Math.min(maximum, value));
}

function percentAt(iso, startedAtMs, endedAtMs) {
  const durationMs = endedAtMs - startedAtMs;
  if (durationMs <= 0) return 0;
  return clamp((Date.parse(iso) - startedAtMs) / durationMs, 0, 1) * 100;
}

function rangeStyle(range, startedAtMs, endedAtMs) {
  const left = percentAt(range.startedAt, startedAtMs, endedAtMs);
  const right = percentAt(range.endedAt, startedAtMs, endedAtMs);
  return { left: `${left}%`, width: `${Math.max(0, right - left)}%` };
}

function utcFromPointer(event, startedAtMs, endedAtMs) {
  const rect = event.currentTarget.getBoundingClientRect();
  const ratio = clamp((event.clientX - rect.left) / rect.width, 0, 1);
  return new Date(startedAtMs + ratio * (endedAtMs - startedAtMs)).toISOString();
}

function Timeline({
  viewport,
  coverage,
  spans,
  playhead,
  hoverUtc,
  disabled,
  onSeek,
  onHover,
  onClearHover,
  onZoom,
  onRecenter,
}) {
  const trackRef = useRef(null);
  const viewportStartMs = Date.parse(viewport.startedAt);
  const viewportEndMs = Date.parse(viewport.endedAt);
  const coverageStartMs = Date.parse(coverage.startedAt);
  const coverageEndMs = Date.parse(coverage.endedAt);
  const viewportDurationSeconds = (viewportEndMs - viewportStartMs) / 1_000;
  const playheadSeconds = playhead
    ? clamp((Date.parse(playhead) - viewportStartMs) / 1_000, 0, viewportDurationSeconds)
    : 0;
  const playheadPosition = playhead
    ? percentAt(playhead, viewportStartMs, viewportEndMs)
    : undefined;
  const hoverPosition = hoverUtc
    ? percentAt(hoverUtc, viewportStartMs, viewportEndMs)
    : undefined;

  useEffect(() => {
    const track = trackRef.current;
    if (!track) return undefined;
    const handleWheel = (event) => {
      if (disabled) return;
      event.preventDefault();
      event.stopPropagation();
      const utc = utcFromPointer(event, viewportStartMs, viewportEndMs);
      onHover(utc);
      onZoom(utc, event.deltaY);
    };
    track.addEventListener("wheel", handleWheel, { passive: false });
    return () => track.removeEventListener("wheel", handleWheel);
  }, [disabled, onHover, onZoom, viewportEndMs, viewportStartMs]);

  const seekByKeyboard = (event) => {
    if (disabled) return;
    const stepMs = Math.max(1_000, (viewportEndMs - viewportStartMs) / 100);
    const currentMs = playhead ? Date.parse(playhead) : viewportStartMs;
    let targetMs;
    if (event.key === "ArrowLeft") targetMs = currentMs - stepMs;
    else if (event.key === "ArrowRight") targetMs = currentMs + stepMs;
    else if (event.key === "Home") targetMs = viewportStartMs;
    else if (event.key === "End") targetMs = viewportEndMs;
    else return;
    event.preventDefault();
    onSeek(new Date(clamp(targetMs, viewportStartMs, viewportEndMs)).toISOString());
  };

  return (
    <div className="timeline">
      <div className="timeline-heading">
        <strong>Timeline</strong>
        <span>Click to seek · scroll to zoom</span>
      </div>
      <div className="timeline-readout">
        <span>{formatTime(playhead)}</span>
        <span>{Math.round(playheadSeconds)} / {Math.round(viewportDurationSeconds)} seconds</span>
      </div>
      <div
        ref={trackRef}
        className="timeline-track"
        role="slider"
        tabIndex={disabled ? -1 : 0}
        aria-label="Archive time"
        aria-valuemin="0"
        aria-valuemax={viewportDurationSeconds}
        aria-valuenow={playheadSeconds}
        aria-valuetext={formatTime(playhead)}
        aria-disabled={disabled}
        onKeyDown={seekByKeyboard}
        onClick={(event) => {
          if (!disabled) onSeek(utcFromPointer(event, viewportStartMs, viewportEndMs));
        }}
        onPointerMove={(event) => {
          if (!disabled) onHover(utcFromPointer(event, viewportStartMs, viewportEndMs));
        }}
        onPointerLeave={onClearHover}
      >
        {spans.map((span, index) => (
          <span
            className={`timeline-segment ${span.kind}`}
            key={`${span.kind}-${span.startedAt}-${index}`}
            style={rangeStyle(span, viewportStartMs, viewportEndMs)}
          />
        ))}
        {hoverPosition !== undefined && (
          <span className="timeline-marker hover" style={{ left: `${hoverPosition}%` }} />
        )}
        {playheadPosition !== undefined && (
          <span className="timeline-marker playhead" style={{ left: `${playheadPosition}%` }} />
        )}
      </div>
      <div className="timeline-boundaries">
        <span>{formatTime(viewport.startedAt)}</span>
        <span>{formatTime(viewport.endedAt)}</span>
      </div>

      <div className="minimap-heading">
        <span>Full archive</span>
        <span>Click to recenter</span>
      </div>
      <div
        className="timeline-minimap"
        role="button"
        tabIndex={disabled ? -1 : 0}
        aria-label="Recenter timeline within full archive"
        aria-disabled={disabled}
        onClick={(event) => {
          if (!disabled) onRecenter(utcFromPointer(event, coverageStartMs, coverageEndMs));
        }}
        onKeyDown={(event) => {
          if (disabled || (event.key !== "Enter" && event.key !== " ")) return;
          event.preventDefault();
          onRecenter(new Date(coverageStartMs + (coverageEndMs - coverageStartMs) / 2).toISOString());
        }}
      >
        {spans.filter((span) => span.kind === "recording").map((span, index) => (
          <span
            className="minimap-recording"
            key={`${span.startedAt}-${index}`}
            style={rangeStyle(span, coverageStartMs, coverageEndMs)}
          />
        ))}
        <span
          className="minimap-viewport"
          style={rangeStyle(viewport, coverageStartMs, coverageEndMs)}
        />
        {playhead && (
          <span
            className="minimap-playhead"
            style={{ left: `${percentAt(playhead, coverageStartMs, coverageEndMs)}%` }}
          />
        )}
      </div>
      <div className="timeline-boundaries minimap-boundaries">
        <span>{formatTime(coverage.startedAt)}</span>
        <span>{formatTime(coverage.endedAt)}</span>
      </div>
    </div>
  );
}

function Player({ latest, manifests, connectionId }) {
  const [store] = useState(() => new ArchivePlayerStore({
    manifests,
    initial: { playhead: latest.startedAt, viewport: latest },
  }));
  const video = useRef(null);
  const refreshTimer = useRef();
  const [muted, setMuted] = useState(true);
  const snapshot = useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getServerSnapshot,
  );
  const hover = useHoverThrottle(store);

  useEffect(() => {
    if (!video.current) return undefined;
    return store.attach(video.current);
  }, [store]);
  useEffect(() => () => {
    clearTimeout(refreshTimer.current);
    store.dispose();
  }, [store]);

  const bootstrap = useQuery({
    queryKey: ["player", connectionId, latest.startedAt],
    queryFn: async () => {
      await store.initialize({ range: latest });
      assertReady(store);
      const first = store.getSnapshot().timeline?.spans.find((span) => span.kind === "recording");
      if (first) await store.seek(first.startedAt);
      assertReady(store);
      return true;
    },
  });

  const seek = useMutation({
    mutationFn: async (utc) => {
      await store.seek(utc, { autoplay: snapshot.playback.playing });
      assertReady(store);
    },
  });
  const playback = useMutation({
    mutationFn: async () => {
      await store.toggle();
      assertReady(store);
    },
  });

  const viewport = {
    startedAt: snapshot.viewport.startedAt ?? latest.startedAt,
    endedAt: snapshot.viewport.endedAt ?? latest.endedAt,
  };
  const coverage = snapshot.coverage ?? latest;
  const timeline = snapshot.timeline;
  const recordings = timeline?.spans.filter((span) => span.kind === "recording").length ?? 0;
  const gaps = timeline?.spans.filter((span) => span.kind === "gap").length ?? 0;
  const error = bootstrap.error
    ?? seek.error
    ?? playback.error
    ?? (snapshot.phase === "error" ? snapshot.error : null);

  const constrainViewport = useCallback(() => {
    const current = store.getSnapshot();
    const visible = current.viewport;
    const available = current.coverage ?? latest;
    if (!visible.startedAt || !visible.endedAt) return;
    const visibleStartMs = Date.parse(visible.startedAt);
    const visibleEndMs = Date.parse(visible.endedAt);
    const coverageStartMs = Date.parse(available.startedAt);
    const coverageEndMs = Date.parse(available.endedAt);
    if (visibleEndMs - visibleStartMs >= coverageEndMs - coverageStartMs) {
      store.fit(available);
    } else if (visibleStartMs < coverageStartMs) {
      store.panBy((coverageStartMs - visibleStartMs) / 1_000);
    } else if (visibleEndMs > coverageEndMs) {
      store.panBy((coverageEndMs - visibleEndMs) / 1_000);
    }
  }, [latest, store]);

  const scheduleTimelineRefresh = useCallback((delay = 120) => {
    clearTimeout(refreshTimer.current);
    refreshTimer.current = setTimeout(() => {
      refreshTimer.current = undefined;
      void store.refresh();
    }, delay);
  }, [store]);

  const zoomAt = useCallback((utc, deltaY) => {
    const current = store.getSnapshot();
    const visible = current.viewport;
    const available = current.coverage ?? latest;
    if (!visible.startedAt || !visible.endedAt) return;
    const spanMs = Date.parse(visible.endedAt) - Date.parse(visible.startedAt);
    const maximumSpanMs = Date.parse(available.endedAt) - Date.parse(available.startedAt);
    const wheelFactor = Math.exp(-clamp(deltaY, -240, 240) * 0.0025);
    const targetSpanMs = clamp(spanMs / wheelFactor, 1_000, maximumSpanMs);
    if (Math.abs(targetSpanMs - spanMs) < 1) return;
    store.zoomAt(utc, spanMs / targetSpanMs);
    constrainViewport();
    scheduleTimelineRefresh();
  }, [constrainViewport, latest, scheduleTimelineRefresh, store]);

  const recenterAt = useCallback((utc) => {
    const current = store.getSnapshot();
    if (!current.viewport.startedAt || !current.viewport.endedAt) return;
    const centerMs = (
      Date.parse(current.viewport.startedAt) + Date.parse(current.viewport.endedAt)
    ) / 2;
    store.panBy((Date.parse(utc) - centerMs) / 1_000);
    constrainViewport();
    scheduleTimelineRefresh(0);
  }, [constrainViewport, scheduleTimelineRefresh, store]);

  return (
    <section className="player" aria-labelledby="playback-heading">
      <h2 id="playback-heading">Playback</h2>
      <p className={error ? "error" : ""}>
        {error ? message(error) : `${snapshot.phase} · ${recordings} recordings · ${gaps} gaps`}
      </p>

      <video ref={video} muted={muted} playsInline />
      <div className="controls">
        <button type="button" onClick={() => playback.mutate()} disabled={playback.isPending}>
          {snapshot.playback.playing ? "Pause" : "Play"}
        </button>
        <button type="button" onClick={() => setMuted((value) => !value)}>
          {muted ? "Unmute" : "Mute"}
        </button>
        <button
          type="button"
          onClick={() => store.setPlaybackRate(
            snapshot.playback.rate === 0.5 ? 1 : snapshot.playback.rate === 1 ? 2 : 0.5,
          )}
        >
          {snapshot.playback.rate}×
        </button>
      </div>

      <Timeline
        viewport={viewport}
        coverage={coverage}
        spans={timeline?.spans ?? []}
        playhead={snapshot.playhead}
        hoverUtc={snapshot.hover?.playhead}
        disabled={!timeline || seek.isPending}
        onSeek={(utc) => seek.mutate(utc)}
        onHover={hover.show}
        onClearHover={hover.clear}
        onZoom={zoomAt}
        onRecenter={recenterAt}
      />

      {snapshot.phase === "gap" && (
        <button type="button" onClick={() => seek.mutate(snapshot.location.span.endedAt)}>
          Skip gap
        </button>
      )}

      <div className="thumbnail-window" aria-live="polite">
        <p>
          {snapshot.hover
            ? `${formatTime(snapshot.hover.playhead)}${snapshot.hover.loading ? " · loading…" : ""}`
            : "Hover over the timeline to preview a thumbnail"}
        </p>
        <div className="thumbnail-stage">
          {snapshot.hover?.thumbnail
            ? <Thumbnail cue={snapshot.hover.thumbnail} />
            : <span>No thumbnail selected</span>}
        </div>
      </div>
    </section>
  );
}

function sourceFields(source) {
  return {
    bucket: source.bucket,
    root: source.root,
    manifest: source.manifest,
    region: source.region,
    endpoint: source.endpoint,
    accessKeyId: source.auth.accessKeyId,
    secretAccessKey: source.auth.secretAccessKey,
  };
}

function Configuration({ initialSource, onConnect }) {
  const [fields, setFields] = useState(() => sourceFields(initialSource));
  const update = (event) => {
    const { name, value } = event.currentTarget;
    setFields((current) => ({ ...current, [name]: value }));
  };
  const submit = (event) => {
    event.preventDefault();
    onConnect({
      bucket: fields.bucket.trim(),
      root: fields.root.trim(),
      manifest: fields.manifest.trim(),
      region: fields.region.trim(),
      endpoint: fields.endpoint.trim(),
      auth: {
        type: "credentials",
        accessKeyId: fields.accessKeyId.trim(),
        secretAccessKey: fields.secretAccessKey,
      },
    });
  };

  return (
    <section className="configuration" aria-labelledby="configuration-heading">
      <div>
        <h2 id="configuration-heading">Archive connection</h2>
        <p>Credentials stay in this browser tab and are used to sign requests directly.</p>
      </div>
      <form onSubmit={submit}>
        <div className="config-grid">
          <label>
            Bucket
            <input name="bucket" value={fields.bucket} onChange={update} required />
          </label>
          <label>
            Region
            <input name="region" value={fields.region} onChange={update} required />
          </label>
          <label className="wide">
            Endpoint
            <input
              name="endpoint"
              type="url"
              value={fields.endpoint}
              onChange={update}
              placeholder="https://example.r2.cloudflarestorage.com"
              spellCheck="false"
              required
            />
          </label>
          <label>
            Root
            <input name="root" value={fields.root} onChange={update} spellCheck="false" required />
          </label>
          <label>
            Manifest
            <input
              name="manifest"
              value={fields.manifest}
              onChange={update}
              spellCheck="false"
              required
            />
          </label>
          <label>
            Access key ID
            <input
              name="accessKeyId"
              value={fields.accessKeyId}
              onChange={update}
              autoComplete="off"
              spellCheck="false"
              required
            />
          </label>
          <label>
            Secret access key
            <input
              name="secretAccessKey"
              type="password"
              value={fields.secretAccessKey}
              onChange={update}
              autoComplete="off"
              required
            />
          </label>
        </div>
        <button className="primary" type="submit">Load archive</button>
      </form>
    </section>
  );
}

function Archive({ source, connectionId }) {
  const resources = useMemo(() => {
    const storage = new S3ArchiveStorage(source);
    return { manifests: new ArchiveManifestClient(storage, {}) };
  }, [source]);
  useEffect(() => () => resources.manifests.dispose(), [resources]);

  const archive = useQuery({
    queryKey: ["archive", connectionId],
    queryFn: () => resources.manifests.load(),
  });

  if (archive.isPending) return <section className="status"><p>Loading archive…</p></section>;
  if (archive.isError) {
    return <section className="status"><p className="error">{message(archive.error)}</p></section>;
  }
  return (
    <Player
      latest={archive.data}
      manifests={resources.manifests}
      connectionId={connectionId}
    />
  );
}

function App() {
  const [connection, setConnection] = useState({
    id: 0,
    source: archiveSource,
  });

  return (
    <main>
      <h1>Filofax demo</h1>
      <Configuration
        initialSource={archiveSource}
        onConnect={(source) => setConnection((current) => ({
          id: current.id + 1,
          source,
        }))}
      />
      <Archive
        key={connection.id}
        source={connection.source}
        connectionId={connection.id}
      />
    </main>
  );
}

createRoot(document.getElementById("root")).render(
  <QueryClientProvider client={queryClient}>
    <App />
  </QueryClientProvider>,
);
