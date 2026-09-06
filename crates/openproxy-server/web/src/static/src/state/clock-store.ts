// state/clock-store.ts — global 250ms clock tick feeding relative
// timestamps. Visibility-aware: the tick pauses while the tab is
// hidden and runs once immediately on resume (no backlog), so
// background tabs don't burn CPU re-rendering elapsed-time labels.

import {
  createVisibilityAwareInterval,
  type VisibilityAwareHandle,
} from "../lib/visibility-aware-interval.js";

const CLOCK_TICK_MS = 250;

class ClockStore {
  public nowMs: number = Date.now();
  private intervalHandle: VisibilityAwareHandle | null = null;
  private subscribers = new Set<() => void>();

  public start() {
    if (this.intervalHandle) return;
    this.intervalHandle = createVisibilityAwareInterval(() => {
      this.nowMs = Date.now();
      for (const sub of this.subscribers) {
        sub();
      }
    }, CLOCK_TICK_MS);
  }

  public stop() {
    if (this.intervalHandle) {
      this.intervalHandle.stop();
      this.intervalHandle = null;
    }
  }

  public subscribe(cb: () => void) {
    this.subscribers.add(cb);
    if (this.subscribers.size > 0) {
      this.start();
    }
  }

  public unsubscribe(cb: () => void) {
    this.subscribers.delete(cb);
    if (this.subscribers.size === 0) {
      this.stop();
    }
  }
}

export const clockStore = new ClockStore();
