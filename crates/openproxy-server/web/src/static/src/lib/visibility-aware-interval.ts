// lib/visibility-aware-interval.ts — a setInterval that pauses while
// the tab is hidden and resumes without a backlog of missed ticks.
//
// Motivation (refactor spec §3.Q11 / §4.3): background polling and
// clock ticks keep firing (throttled) while the tab is in the
// background, wasting CPU and — for async polls — piling up requests
// that all resolve at once when the user returns. This wrapper:
//   - stops scheduling ticks while `document.hidden === true`;
//   - on becoming visible again, runs the callback ONCE immediately
//     (when `suppressMissedTicks` is true, the default) and then
//     resumes the normal cadence — never N catch-up ticks;
//   - is async-aware: if the callback returns a Promise, the next
//     tick is scheduled only AFTER it settles, so a slow request
//     can't stack up two concurrent ticks (mirrors the deliberate
//     chained-setTimeout pattern in the notification/debug stores).
//
// The `visibilitychange` listener is registered per-instance and
// removed on `stop()` — there is no global registry to leak.

export interface VisibilityAwareOptions {
  /**
   * When the tab becomes visible again, run the callback once
   * immediately instead of waiting a full interval, and drop every
   * tick that "should" have fired while hidden. Default: true.
   * When false, resuming simply continues the cadence with no
   * immediate catch-up run.
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

/** Run one tick, swallowing callback errors (a throwing poll must
 *  not kill the interval or produce an unhandled rejection), then
 *  re-arm unless we were stopped or the tab went hidden mid-tick. */
function runTickAndRearm(cb: TickCallback, state: { stopped: boolean }, rearm: () => void): void {
  void (async () => {
    try {
      await cb();
    } catch (_e: unknown) {
      // Swallow — same contract as the chained-setTimeout polls this
      // replaces: the next tick retries.
    } finally {
      if (!state.stopped && !isHidden()) rearm();
    }
  })();
}

/**
 * Start a visibility-aware interval. Returns a handle whose `stop()`
 * cancels the timer and detaches the `visibilitychange` listener.
 *
 * The first tick fires after `intervalMs` (setInterval semantics —
 * it does NOT run immediately on creation), so callers that want an
 * eager first fetch prime it themselves before calling this.
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
      // Pause: drop the pending tick. Any in-flight async callback
      // will finish but won't re-arm (guarded by `state.stopped`).
      clearTimer();
      return;
    }
    // Visible again — one immediate catch-up run (if suppressMissedTicks),
    // then resume the cadence.
    if (suppressMissedTicks) {
      runTickAndRearm(cb, state, arm);
    } else {
      arm();
    }
  };

  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", onVisibilityChange);
  }

  // Only start the cadence if we're visible now; if hidden at
  // creation, the visibilitychange handler will arm it on resume.
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
