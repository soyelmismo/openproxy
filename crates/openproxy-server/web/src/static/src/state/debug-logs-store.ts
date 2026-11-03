// Unviewed WARN+ERROR count from the server's debug-log ring buffer, powering
// the sidebar badge on the Debug Logs link.
//
// Every 30s the store polls
// `GET /admin/api/debug/logs?level=WARN,ERROR&since=<viewedSeq>`. The poll is
// process-global and never tears down, so the badge keeps reflecting new errors
// while the view is closed and its own 2s poll is stopped.

import { fetchDebugLogs } from "../lib/api.js";
import {
  createVisibilityAwareInterval,
  type VisibilityAwareHandle,
} from "../lib/visibility-aware-interval.js";

type CountListener = (count: number) => void;

// Module-local state


/** `seq` of the newest entry the user has seen. Entries above it count towards
 *  the badge. 0 counts everything in the buffer. */
let viewedSeq: number = 0;

/** Newest `latest_seq` from the server, so `markDebugLogsViewed()` can advance
 *  `viewedSeq` without a round-trip. */
let latestSeq: number = 0;

/** Current unviewed WARN+ERROR count. 0 hides the sidebar badge. */
let unviewedCount: number = 0;

/** Visibility-aware poll: one tick at a time, paused while the tab is hidden. */
let pollHandle: VisibilityAwareHandle | null = null;

let initialized: boolean = false;

const countListeners: Set<CountListener> = new Set();

/** Current unviewed WARN+ERROR count. 0 hides the badge. */
export function getUnviewedWarnErrorCount(): number {
  return unviewedCount;
}

/** Replace the unviewed count and notify every subscriber (the sidebar
 *  badge). The count stays exact; the sidebar caps its display at 99+. */
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

export function onUnviewedWarnErrorCountChange(fn: CountListener): () => void {
  countListeners.add(fn);
  return () => { countListeners.delete(fn); };
}

/** Mark everything seen so far as viewed and clear the badge. Called when the
 *  user navigates to `#/debug-logs`; later errors re-trigger the badge on the
 *  next poll. */
export function markDebugLogsViewed(): void {
  if (latestSeq > viewedSeq) viewedSeq = latestSeq;
  setUnviewedWarnErrorCount(0);
}

/** Start the poll at boot. Idempotent; the first tick fires immediately so
 *  errors raised during boot show up. */
export function initDebugLogsStore(): void {
  if (initialized) return;
  initialized = true;
  void refreshUnviewedCount();
  schedulePoll();
}

/** Refetch the unviewed count. Swallows network errors: the badge holds its
 *  last-known value rather than flickering to 0. */
export async function refreshUnviewedCount(): Promise<void> {
  try {
    // `total_in_buffer` is computed before the limit truncation, so limit=1
    // still yields the full count of matching entries above `viewedSeq`.
    const resp = await fetchDebugLogs({
      since: viewedSeq,
      level: "WARN,ERROR",
      limit: 1,
    });
    latestSeq = resp.latest_seq;
    setUnviewedWarnErrorCount(resp.total_in_buffer);
  } catch {
    // Next tick retries; the badge keeps its last-known value.
  }
}

/** Start the 30s visibility-aware poll. */
function schedulePoll(): void {
  if (pollHandle !== null) return;
  pollHandle = createVisibilityAwareInterval(() => refreshUnviewedCount(), 30_000);
}
