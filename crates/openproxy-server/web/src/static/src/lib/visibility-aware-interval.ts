// lib/visibility-aware-interval.ts — a setInterval that pauses while the tab is hidden and
// resumes without a backlog of missed ticks.
//
// Motivation (refactor spec §3.Q11 / §4.3): background polling and clock ticks keep firing
// (throttled) in the background, wasting CPU and — for async polls — piling up requests that all
// resolve at once on return. This wrapper:
//   - stops scheduling ticks while `document.hidden === true`;
//   - on becoming visible again runs the callback ONCE immediately (default
//     `suppressMissedTicks: true`) then resumes the normal cadence — never N catch-up ticks;
//   - is async-aware: when the callback returns a Promise the next tick is scheduled only AFTER
//     it settles, so a slow request can't stack two concurrent ticks (mirrors the deliberate
//     chained-setTimeout pattern in the notification/debug stores).
//
// The `visibilitychange` listener is registered per-instance and removed on `stop()` — there is
// no global registry to leak.

export interface VisibilityAwareOptions {
  /**
   * When the tab becomes visible again, run the callback once immediately instead of waiting a
   * full interval, dropping every tick that "should" have fired while hidden. Default: true.
   * When false, resuming just continues the cadence with no immediate catch-up run.
   */
  suppressMissedTicks?: boolean;
}

export interface VisibilityAwareHandle {
  /** Permanently stop the interval and remove the listener. */
  stop(): void;
}

type TickCallback = () => void | Promise<void>;

function isHidden(): boolean {
  return typeof document !== "undefined" && document.hidden === true;
}

/** Run one tick, swallowing callback errors (a throwing poll must not kill the interval or
 *  produce an unhandled rejection), then re-arm unless we were stopped or the tab went hidden
 *  mid-tick. */
function runTickAndRearm(cb: TickCallback, state: { stopped: boolean }, rearm: () => void): void {
  void (async () => {
    try {
      await cb();
    } catch {
      // Swallow: the next tick retries, matching the chained-setTimeout polls
      // this replaces.
    } finally {
      if (!state.stopped && !isHidden()) rearm();
    }
  })();
}

/**
 * Start a visibility-aware interval. Returns a handle whose `stop()` cancels the timer and
 * detaches the `visibilitychange` listener.
 *
 * The first tick fires after `intervalMs` (setInterval semantics — it does NOT run immediately on
 * creation), so callers wanting an eager first fetch prime it themselves before calling this.
 */
export function createVisibilityAwareInterval(
  cb: TickCallback,
  intervalMs: number,
  opts: VisibilityAwareOptions = {},
): VisibilityAwareHandle {
  const suppressMissedTicks: boolean = opts.suppressMissedTicks ?? true;

  let timer: ReturnType<typeof setTimeout> | null = null;
  const state = { stopped: false };

  const clearTimer = (): void => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };

  /** Schedule the next tick after `intervalMs`. */
  const arm = (): void => {
    clearTimer();
    if (state.stopped) return;
    timer = setTimeout(() => {
      timer = null;
      if (state.stopped) return;
      runTickAndRearm(cb, state, arm);
    }, intervalMs);
  };

  const onVisibilityChange = (): void => {
    if (state.stopped) return;
    if (isHidden()) {
      // Pause: drop the pending tick. An in-flight async callback finishes but
      // does not re-arm.
      clearTimer();
      return;
    }
    // Visible again: one immediate catch-up run when suppressMissedTicks is set,
    // then the normal cadence resumes.
    if (suppressMissedTicks) {
      runTickAndRearm(cb, state, arm);
    } else {
      arm();
    }
  };

  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", onVisibilityChange);
  }

  // The cadence starts only when the tab is visible now; a hidden tab is armed
  // by the visibilitychange handler on resume.
  if (!isHidden()) arm();

  return {
    stop(): void {
      state.stopped = true;
      clearTimer();
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibilityChange);
      }
    },
  };
}
