import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { GapClock } from "./gap-clock.js";

describe("GapClock", () => {
  let scheduled: FrameRequestCallback | undefined;
  let now = 0;

  beforeEach(() => {
    scheduled = undefined;
    now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    vi.stubGlobal("requestAnimationFrame", vi.fn((callback: FrameRequestCallback) => {
      scheduled = callback;
      return 1;
    }));
    vi.stubGlobal("cancelAnimationFrame", vi.fn(() => {
      scheduled = undefined;
    }));
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  const runFrame = (timestamp: number): void => {
    const callback = scheduled;
    if (!callback) throw new Error("expected a scheduled animation frame");
    scheduled = undefined;
    callback(timestamp);
  };

  it("re-anchors rate changes without losing elapsed wall-clock time", () => {
    const ticks: number[] = [];
    const ended = vi.fn();
    const clock = new GapClock((playheadMs) => ticks.push(playheadMs), ended);
    clock.configure(1_000, 4_000, 1);

    clock.play();
    runFrame(500);
    now = 750;
    clock.setRate(2);
    runFrame(1_000);
    now = 1_100;
    clock.pause();

    expect(ticks).toEqual([1_500, 1_750, 2_250, 2_450]);
    expect(ended).not.toHaveBeenCalled();
    expect(scheduled).toBeUndefined();
  });

  it("owns end detection and emits the boundary exactly once", () => {
    const ticks: number[] = [];
    const ended = vi.fn();
    const clock = new GapClock((playheadMs) => ticks.push(playheadMs), ended);
    clock.configure(1_000, 2_000, 1);

    clock.play();
    runFrame(1_000);
    clock.pause();
    clock.setRate(2);

    expect(ticks).toEqual([2_000]);
    expect(ended).toHaveBeenCalledTimes(1);
    expect(scheduled).toBeUndefined();
  });
});
