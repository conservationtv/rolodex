export type GapClockTick = (playheadMs: number) => void;
export type GapClockEnded = () => void;

/** Advances wall-clock time while no media exists. The clock owns all timing,
 * rate changes, animation scheduling, and end detection; consumers only mirror
 * ticks into their state and handle the single ended callback. */
export class GapClock {
  private onTick: GapClockTick;
  private onEnded: GapClockEnded;
  private playheadMs = 0;
  private endedAtMs = 0;
  private rate = 1;
  private anchorPlayheadMs = 0;
  private anchorTime = 0;
  private animationFrame: number | undefined;
  private running = false;

  constructor(onTick: GapClockTick, onEnded: GapClockEnded) {
    this.onTick = onTick;
    this.onEnded = onEnded;
  }

  configure(playheadMs: number, endedAtMs: number, rate: number): void {
    this.stop();
    this.playheadMs = Math.min(playheadMs, endedAtMs);
    this.endedAtMs = endedAtMs;
    this.rate = rate;
  }

  play(): void {
    if (this.running) return;
    if (this.playheadMs >= this.endedAtMs) {
      this.finish();
      return;
    }
    this.running = true;
    this.anchorPlayheadMs = this.playheadMs;
    this.anchorTime = performance.now();
    this.schedule();
  }

  pause(): void {
    if (!this.running) return;
    this.flush(performance.now());
    if (!this.running) return;
    this.cancelFrame();
    this.running = false;
  }

  setRate(rate: number): void {
    if (this.running) this.flush(performance.now());
    this.rate = rate;
  }

  stop(): void {
    this.cancelFrame();
    this.running = false;
  }

  private schedule(): void {
    this.animationFrame = requestAnimationFrame((now) => {
      this.animationFrame = undefined;
      if (!this.running) return;
      this.flush(now);
      if (this.running) this.schedule();
    });
  }

  private flush(now: number): void {
    const elapsedMs = Math.max(0, now - this.anchorTime) * this.rate;
    this.playheadMs = Math.min(this.endedAtMs, this.anchorPlayheadMs + elapsedMs);
    this.anchorPlayheadMs = this.playheadMs;
    this.anchorTime = now;
    this.onTick(this.playheadMs);
    if (this.playheadMs >= this.endedAtMs) this.finish();
  }

  private finish(): void {
    this.cancelFrame();
    this.running = false;
    this.onEnded();
  }

  private cancelFrame(): void {
    if (this.animationFrame === undefined) return;
    cancelAnimationFrame(this.animationFrame);
    this.animationFrame = undefined;
  }
}
