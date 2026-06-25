// state/debug-logs-store.ts — module-local store for the count of
// WARN+ERROR entries in the server's debug-log ring buffer that the
// user has NOT yet viewed. Powers the sidebar badge on the
// "Debug Logs" link so discovery failures (and other WARN-level
// events) are surfaced to the operator without them having to
// navigate to the Debug Logs view to check.
//
// B1 (Bug 3): the user complained they had no way to see trace
// logs of errors like the Cloudflare 404. The Debug Logs view
// already existed and the sidebar already linked to it, but the
// link was easy to miss because there was no visual indicator
// that new errors had arrived. This store closes that gap: every
// 30s it polls `GET /admin/api/debug/logs?level=WARN,ERROR&since=N`
// for entries that arrived since the user's last visit, and the
// sidebar renders a red badge with the unviewed count.
//
// The store is process-global and never tears down. The 30s poll
// handles the case where the user navigates away from the Debug
// Logs view (the view's own 2s poll stops on unmount; this store's
// poll keeps running so the badge reflects new errors even when
// the view is closed). When the user navigates to #/debug-logs,
// `markDebugLogsViewed()` advances the cursor to the latest known
// `latest_seq` and clears the badge.

import { fetchDebugLogs } from "../lib/api.js";

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

type CountListener = (count: number) => void;

// ----------------------------------------------------------------------------
// Module-local state
// ----------------------------------------------------------------------------

/** The `seq` of the most recent entry the user has "seen" (either by
 *  visiting the Debug Logs view or by this store's poll picking it up
 *  and folding it into the badge count). Entries with `seq > viewedSeq`
 *  count towards `unviewedCount`. 0 means "nothing viewed yet — count
 *  everything in the buffer". */
let viewedSeq: number = 0;

/** The latest `latest_seq` reported by the server. Used by
 *  `markDebugLogsViewed()` to advance `viewedSeq` without an extra
 *  network round-trip. */
let latestSeq: number = 0;

/** Current unviewed WARN+ERROR count. 0 hides the sidebar badge. */
let unviewedCount: number = 0;

/** 30s poll handle. Cleared on every tick and rescheduled inside the
 *  tick's `finally` so a slow request can't stack up two concurrent
 *  ticks. */
let pollHandle: ReturnType<typeof setTimeout> | null = null;

let initialized: boolean = false;

const countListeners: Set<CountListener> = new Set();

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/** Current unviewed WARN+ERROR count. 0 hides the badge. */
export function getUnviewedWarnErrorCount(): number {
  return unviewedCount;
}

/** Replace the unviewed count and notify every subscriber (the
 *  sidebar's badge). Clamped at 0; cap display at 99+ in the
 *  sidebar (not here — the count stays accurate for the view's
 *  own header indicator). */
export function setUnviewedWarnErrorCount(n: number): void {
  const next: number = Math.max(0, n | 0);
  if (next === unviewedCount) return;
  unviewedCount = next;
  for (const fn of countListeners) {
    try { fn(unviewedCount); } catch (e: unknown) {
      console.error("[debug-logs-store] count listener threw", e);
    }
  }
}

/** Subscribe to unviewed-count changes. Returns an unsubscribe fn. */
export function onUnviewedWarnErrorCountChange(fn: CountListener): () => void {
  countListeners.add(fn);
  return () => { countListeners.delete(fn); };
}

/** Mark all current entries as viewed. Called by the router when
 *  the user navigates to `#/debug-logs` — advances `viewedSeq` to
 *  `latestSeq` (the highest seq we've seen from the server) and
 *  clears the badge. New errors arriving after this call will
 *  re-trigger the badge on the next 30s poll. */
export function markDebugLogsViewed(): void {
  if (latestSeq > viewedSeq) viewedSeq = latestSeq;
  setUnviewedWarnErrorCount(0);
}

/** Initialise the store at app boot. Idempotent — safe to call more
 *  than once. Starts the 30s poll; the first tick fires immediately
 *  (no 30s delay on boot). Mirrors the design of
 *  `notifications-store.ts::initNotificationsStore` but with a 30s
 *  cadence instead of 30s + a 5s keepalive WS. */
export function initDebugLogsStore(): void {
  if (initialized) return;
  initialized = true;
  // Kick off the first poll immediately so the badge reflects any
  // errors that fired before the dashboard finished booting.
  void refreshUnviewedCount();
  schedulePoll();
}

/** Force a refetch of the unviewed count from the server. Used by
 *  `initDebugLogsStore` (boot) and by the 30s poll. Swallows
 *  network errors — the badge just stays at its last-known value
 *  rather than flickering to 0. */
export async function refreshUnviewedCount(): Promise<void> {
  try {
    // `limit=1` minimizes the payload (we only need the count, but
    // the server's `total_in_buffer` is the count after filtering
    // — we still get an accurate count even with limit=1 because
    // `total_in_buffer` is computed BEFORE the limit truncation).
    // `level=WARN,ERROR` filters to the levels we want to surface.
    // `since=viewedSeq` returns only entries newer than the user's
    // last view (0 = everything in the buffer).
    const resp = await fetchDebugLogs({
      since: viewedSeq,
      level: "WARN,ERROR",
      limit: 1,
    });
    latestSeq = resp.latest_seq;
    // `total_in_buffer` is the count of WARN+ERROR entries with
    // `seq > viewedSeq` (after the level filter, before the limit
    // truncation). Exactly what we want for the badge.
    setUnviewedWarnErrorCount(resp.total_in_buffer);
  } catch (_e: unknown) {
    // Swallow — the 30s poll will try again. The badge stays at
    // its last-known value rather than flickering to 0.
  }
}

// ----------------------------------------------------------------------------
// Boot + lifecycle
// ----------------------------------------------------------------------------

/** Schedule the next 30s poll tick. We use `setTimeout` (not
 *  `setInterval`) so a slow request can't stack up two concurrent
 *  ticks — the next tick is scheduled inside the previous tick's
 *  `finally` AFTER the await resolves. */
function schedulePoll(): void {
  if (pollHandle !== null) return;
  pollHandle = setTimeout(() => {
    pollHandle = null;
    void (async () => {
      try { await refreshUnviewedCount(); }
      finally { schedulePoll(); }
    })();
  }, 30_000);
}
