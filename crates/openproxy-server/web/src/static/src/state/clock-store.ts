

class ClockStore {
  public nowMs: number = Date.now();
  private intervalHandle: ReturnType<typeof setInterval> | null = null;
  private subscribers = new Set<() => void>();

  public start() {
    if (this.intervalHandle) return;
    this.intervalHandle = setInterval(() => {
      this.nowMs = Date.now();
      for (const sub of this.subscribers) {
        sub();
      }
    }, 250);
  }

  public stop() {
    if (this.intervalHandle) {
      clearInterval(this.intervalHandle);
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
