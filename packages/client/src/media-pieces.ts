import Hls, {
  type FragmentLoaderContext,
  type LoaderContext,
  type FragmentLoaderConstructor,
  type LoaderConfiguration,
  type LoaderCallbacks,
} from "hls.js";

export interface MediaPiece {
  readonly url: string;
  readonly byteOffset: number;
  readonly byteLength: number;
}
const maxSegmentBytes = 8 * 1024 * 1024;

class RangeResponseError extends Error {
  readonly status: number;

  constructor(status: number) {
    super(`Archive range unavailable: HTTP ${status}`);
    this.status = status;
  }
}

/** Bounded assembly of immutable R2 ranges. Normal HLS URLs retain the default loader. */
export function createPieceLoader(
  segments: ReadonlyMap<string, readonly MediaPiece[]>,
): FragmentLoaderConstructor {
  return class PieceLoader extends Hls.DefaultConfig.loader {
    declare context: FragmentLoaderContext | null;
    private cancel: (() => void) | undefined = undefined;
    private controller: AbortController | undefined = undefined;
    private timer: ReturnType<typeof setTimeout> | undefined = undefined;

    override load(
      context: FragmentLoaderContext,
      config: LoaderConfiguration,
      callbacks: LoaderCallbacks<FragmentLoaderContext>,
    ): void {
      const pieces = segments.get(context.url);
      if (!pieces) {
        super.load(context, config, {
          onSuccess: (response, stats, _context, details) =>
            callbacks.onSuccess(response, stats, context, details),
          onError: (error, _context, details, stats) =>
            callbacks.onError(error, context, details, stats),
          onTimeout: (stats, _context, details) => callbacks.onTimeout(stats, context, details),
          onAbort: (stats, _context, details) => callbacks.onAbort?.(stats, context, details),
          onProgress: (stats, _context, data, details) =>
            callbacks.onProgress?.(stats, context, data, details),
        } satisfies LoaderCallbacks<LoaderContext>);
        return;
      }
      this.context = context;
      const controller = new AbortController();
      this.controller = controller;
      this.cancel = () => {
        this.stats.aborted = true;
        callbacks.onAbort?.(this.stats, context, null);
      };
      this.stats.loading.start = performance.now();
      this.timer = setTimeout(() => {
        controller.abort();
        this.cancel = undefined;
        callbacks.onTimeout(this.stats, context, null);
      }, config.loadPolicy.maxLoadTimeMs);
      void assemblePieces(pieces, controller.signal)
        .then((data) => {
          if (controller.signal.aborted) return;
          clearTimeout(this.timer);
          this.stats.loading.first = this.stats.loading.start;
          this.stats.loading.end = performance.now();
          this.stats.loaded = data.byteLength;
          this.stats.total = data.byteLength;
          this.cancel = undefined;
          callbacks.onSuccess({ url: context.url, data }, this.stats, context, null);
        })
        .catch((cause: Error) => {
          if (controller.signal.aborted) return;
          clearTimeout(this.timer);
          controller.abort();
          this.cancel = undefined;
          callbacks.onError(
            { code: cause instanceof RangeResponseError ? cause.status : 0, text: cause.message },
            context,
            null,
            this.stats,
          );
        });
    }
    override abort(): void {
      const cancel = this.cancel;
      this.stopPieces();
      super.abort();
      cancel?.();
    }
    override destroy(): void {
      // hls.js destroys loaders inside terminal callbacks; destruction must not emit another callback.
      this.stopPieces();
      super.destroy();
    }

    private stopPieces(): void {
      this.cancel = undefined;
      this.controller?.abort();
      this.controller = undefined;
      clearTimeout(this.timer);
      this.timer = undefined;
    }
  };
}

/** Fetch at most three coalesced pieces concurrently and never buffer an unbounded response. */
export async function assemblePieces(
  pieces: readonly MediaPiece[],
  signal: AbortSignal,
): Promise<ArrayBuffer> {
  const length = pieces.reduce((total, piece) => total + piece.byteLength, 0);
  if (!pieces.length || pieces.length > 32 || length > maxSegmentBytes)
    throw new Error("Archive range exceeds assembly limits");
  const output = new Uint8Array(length);
  let offset = 0;
  for (let index = 0; index < pieces.length; index += 3) {
    const batch = pieces.slice(index, index + 3);
    const starts = batch.map((piece) => {
      const start = offset;
      offset += piece.byteLength;
      return start;
    });
    await Promise.all(
      batch.map((piece, i) =>
        readPiece(piece, output.subarray(starts[i], starts[i]! + piece.byteLength), signal),
      ),
    );
  }
  return output.buffer;
}

async function readPiece(
  piece: MediaPiece,
  output: Uint8Array,
  signal: AbortSignal,
): Promise<void> {
  const end = piece.byteOffset + piece.byteLength - 1;
  const response = await fetch(piece.url, {
    signal,
    headers: { Range: `bytes=${piece.byteOffset}-${end}` },
  });
  const expected = `bytes ${piece.byteOffset}-${end}/`;
  if (response.status !== 206 || !response.headers.get("Content-Range")?.startsWith(expected)) {
    await response.body?.cancel();
    throw new RangeResponseError(response.status);
  }
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Archive range has no body");
  let received = 0;
  try {
    let next = await reader.read();
    while (!next.done) {
      if (received + next.value.byteLength > output.byteLength)
        throw new Error("Archive range returned too many bytes");
      output.set(next.value, received);
      received += next.value.byteLength;
      next = await reader.read();
    }
    if (received !== output.byteLength) throw new Error("Archive range was truncated");
  } finally {
    await reader.cancel();
    reader.releaseLock();
  }
}
