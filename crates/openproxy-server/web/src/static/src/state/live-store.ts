// state/live-store.ts
// ============================================================================
// F5: Live-store for real-time aggregations.
//
// Consumes `row` and `stage` WebSocket events (via the F2 ws-bus pub/sub)
// and maintains in-memory rolling aggregations: throughput, status
// distribution, latency percentiles, race outcomes. Exposes reactive
// snapshots that lit-html components (home view, future live widgets)
// render.
//
// Lifecycle
// ---------
// - `mountLiveStore()` is called by views that need live data (home, logs).
//   The first consumer subscribes to ws-bus events and (if no other view is
//   already driving the WS) opens the WS connection via
//   `connectLogsWebSocket()`. The store does NOT own the WS — it observes
//   events via the bus. But it does trigger connect/disconnect so the home
//   view can get live data without the logs view being mounted.
// - `unmountLiveStore()` decrements the mount count. When the count drops
//   to 0 (last consumer), unsubscribes from ws-bus and (if the store opened
//   the WS) closes it via `disconnectLogsWebSocket()`. Data is preserved so
//   a quick remount shows existing aggregations immediately — the store
//   only re-subscribes + re-rehydrates the gap.
//
// Rehydration
// -----------
// - On first mount (lastSeenRowId === 0), fetches the recent 100 rows from
//   `GET /admin/api/usage/recent?limit=100` to seed the activity-feed ring
//   buffer. NOTE: the server's `recent()` returns rows oldest-first when
//   `since_id=0`, so the initial seed is actually the OLDEST 100 rows in
//   the DB (a pre-existing API quirk — the home view's `?limit=5` fetch
//   has the same issue). The activity feed shows whatever the API returns;
//   `lastSeenRowId` is set to the highest id so subsequent resync fetches
//   the right gap.
// - On `lag_warning` (channel != "notifications") / `resync` envelopes,
//   fetches `GET /admin/api/usage/recent?since_id=${lastSeenRowId}&limit=500`
//   to fill the gap and prepends the new rows to the ring buffer.
// - Rehydrated rows seed the ring buffer ONLY, NOT the buckets. The
//   buckets are aggregations of LIVE events (so KPIs reflect "what's
//   happening now"). Writing old rows to buckets would skew the current
//   bucket; writing recent resync rows to buckets would require per-bucket
//   staleness tracking (added complexity not warranted for MVP). KPIs
//   therefore undercount slightly after a resync — the missed rows are
//   visible in the activity feed but don't count toward throughput. See
//   concerns.
//
// Reactivity
// ----------
// - Subscribers register via `subscribe(fn)`; the store calls all
//   subscribers on a throttled cadence (max 1 update per 250ms) so a
//   1000 events/sec burst triggers at most 4 re-renders/sec.
// - When the document is hidden (visibilitychange), the store continues
//   processing events (keeps buckets + ring buffer current) but does NOT
//   fire subscribers — avoids layout work in a background tab. On
//   visibility regain, a single update is fired so the UI catches up.
//
// Field name mapping (Rust `RecentUsageRow` → task-spec terminology)
// -----------------------------------------------------------------
// The task spec describes the row with placeholder field names that don't
// match the actual Rust struct / TS interface. The mapping:
//   - status_code        → "status"
//   - upstream_model_id  → "model"
//   - prompt_tokens      → "tokens_in"
//   - completion_tokens  → "tokens_out"
//   - total_ms           → "latency_ms"
//   - race_total         → "race_size"
//   - race_lost (bool)   → race_winner = !race_lost
//   - error_message      → "error"
//   - created_at         → "started_at" / "completed_at" (no separate
//                          fields; one timestamp per row)
// The store uses the actual Rust field names (status_code, prompt_tokens,
// etc.) because that's what the WS delivers — the mapping above is just
// documentation.
// ============================================================================

import type { WsEnvelope } from "../views/logs.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";
import { subscribeWs } from "./ws-bus.js";
import { connectLogsWebSocket, disconnectLogsWebSocket, subscribeLogsStatus, type LogsStatus } from "./ws.js";
import { state } from "./index.js";
import { api } from "../lib/api.js";

// ----------------------------------------------------------------------------
// Bucket math (F5.2)
// ----------------------------------------------------------------------------

/** A single time-bucket aggregation. Three windows are maintained:
 *  1-second buckets (300 = 5 min), 5-second buckets (360 = 30 min),
 *  1-minute buckets (1440 = 24h). Each `row` event increments the
 *  current bucket in each window using modulo indexing — old data is
 *  overwritten when the index cycles back. */
export interface Bucket {
  count: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd: number;
  status_2xx: number;
  status_4xx: number;
  status_5xx: number;
  /** Latency samples (ms) for percentile calculation. Capped at
   *  `MAX_LATENCIES_PER_BUCKET` (1000) per bucket — at 1000 events/sec
   *  in a 1-second bucket, this is exact; in a 5-second bucket it's a
   *  uniform sample of the first 1000 (good enough for p50/p95/p99). */
  latencies: number[];
  race_wins: number;
  race_total: number;
}

function emptyBucket(): Bucket {
  return {
    count: 0,
    tokens_in: 0,
    tokens_out: 0,
    cost_usd: 0,
    status_2xx: 0,
    status_4xx: 0,
    status_5xx: 0,
    latencies: [],
    race_wins: 0,
    race_total: 0,
  };
}

/** Reset a bucket in place (preserves object reference, zeros all
 *  fields). Used when the bucket index cycles back and we're about to
 *  write fresh data. */
function resetBucketInPlace(b: Bucket): void {
  b.count = 0;
  b.tokens_in = 0;
  b.tokens_out = 0;
  b.cost_usd = 0;
  b.status_2xx = 0;
  b.status_4xx = 0;
  b.status_5xx = 0;
  b.latencies.length = 0;
  b.race_wins = 0;
  b.race_total = 0;
}

const WINDOW_1S = 300;   // 5 min
const WINDOW_5S = 360;   // 30 min
const WINDOW_1M = 1440;  // 24h
const MAX_LATENCIES_PER_BUCKET = 1000;
const MAX_RECENT_ROWS = 1000;

// Bucket arrays. Pre-allocated once at module load; entries are reset in
// place via `resetBucketInPlace` when the index cycles back. We never
// grow or shrink these arrays.
const buckets1s: Bucket[] = [];
const buckets5s: Bucket[] = [];
const buckets1m: Bucket[] = [];
for (let i = 0; i < WINDOW_1S; i++) buckets1s.push(emptyBucket());
for (let i = 0; i < WINDOW_5S; i++) buckets5s.push(emptyBucket());
for (let i = 0; i < WINDOW_1M; i++) buckets1m.push(emptyBucket());

// Per-window "last bucket index written" trackers. When the current
// index (derived from Date.now()) differs from the tracker, the bucket
// at that index is stale (it was last written in a previous cycle) and
// is reset before the new write. This is the spec's approach — it works
// for LIVE events (monotonically-advancing index). For REHYDRATED events
// (which we don't write to buckets — see module docstring), the trackers
// are not consulted.
let lastBucket1s = -1;
let lastBucket5s = -1;
let lastBucket1m = -1;

function bucketIndexFromNow(windowSecs: number, totalBuckets: number): number {
  const nowSec = Math.floor(Date.now() / 1000);
  // Math.floor(nowSec / windowSecs) can in theory be negative for dates
  // before epoch; the `(... % N + N) % N` idiom keeps the index in
  // [0, N). Defensive — Date.now() is always positive in practice.
  return ((Math.floor(nowSec / windowSecs) % totalBuckets) + totalBuckets) % totalBuckets;
}

/** Write a single row's contribution to one bucket. The bucket is
 *  passed by reference; the caller is responsible for staleness checks
 *  (resetting stale buckets before calling this). */
function incrementBucket(b: Bucket, row: RecentUsageRow): void {
  b.count++;
  b.tokens_in += row.prompt_tokens ?? 0;
  b.tokens_out += row.completion_tokens ?? 0;
  b.cost_usd += row.cost_usd ?? 0;
  if (row.status_code >= 200 && row.status_code < 300) b.status_2xx++;
  else if (row.status_code >= 400 && row.status_code < 500) b.status_4xx++;
  else if (row.status_code >= 500) b.status_5xx++;
  if (b.latencies.length < MAX_LATENCIES_PER_BUCKET) {
    b.latencies.push(row.total_ms || 0);
  }
  const raceSize: number = row.race_total ?? 0;
  if (raceSize > 1) {
    b.race_total++;
    // `race_lost === true` means this row is a race loser. The winner
    // is the row with `race_lost === false` (and `race_total > 1`).
    if (!row.race_lost) b.race_wins++;
  }
}

/** Write a LIVE row event (just arrived from the WS) to all 3 bucket
 *  windows. Uses Date.now() to derive the current bucket index in each
 *  window, resets the bucket if the index advanced since the last
 *  write, then increments. */
function writeRowToBuckets(row: RecentUsageRow): void {
  const idx1s = bucketIndexFromNow(1, WINDOW_1S);
  if (idx1s !== lastBucket1s) {
    resetBucketInPlace(buckets1s[idx1s]!);
    lastBucket1s = idx1s;
  }
  const idx5s = bucketIndexFromNow(5, WINDOW_5S);
  if (idx5s !== lastBucket5s) {
    resetBucketInPlace(buckets5s[idx5s]!);
    lastBucket5s = idx5s;
  }
  const idx1m = bucketIndexFromNow(60, WINDOW_1M);
  if (idx1m !== lastBucket1m) {
    resetBucketInPlace(buckets1m[idx1m]!);
    lastBucket1m = idx1m;
  }
  // The non-null assertions on `buckets*[idx]` are safe because the
  // arrays are pre-allocated to exactly `WINDOW_*` entries and the
  // modulo index is always in [0, WINDOW_*).
  incrementBucket(buckets1s[idx1s]!, row);
  incrementBucket(buckets5s[idx5s]!, row);
  incrementBucket(buckets1m[idx1m]!, row);
}

// ----------------------------------------------------------------------------
// In-memory state
// ----------------------------------------------------------------------------

/** Ring buffer of the last 1000 terminal rows, NEWEST FIRST. Live `row`
 *  events are prepended; rehydrated rows are prepended in oldest-first
 *  order so the buffer ends up newest-first. Capped at MAX_RECENT_ROWS
 *  — when full, the oldest entry (last element) is dropped. */
const recentRows: RecentUsageRow[] = [];
/** Set of row ids currently in `recentRows`. O(1) dedup check on
 *  prepend — if a row arrives twice (e.g. a rehydrate fetch overlaps
 *  with a live event), the duplicate is silently dropped. */
const recentRowIds: Set<number> = new Set<number>();

/** In-flight requests keyed by `request_id`. Populated by non-terminal
 *  `stage` events ("started", "connecting", "waiting_ttft",
 *  "streaming"); cleared by terminal stage events ("completed",
 *  "failed", "cancelled") or by the matching `row` event. Exposed in
 *  the snapshot as `activeRequests: number` (the size of this map). */
const activeRequests: Map<string, StageEvent> = new Map<string, StageEvent>();

/** Highest `usage.id` we've seen (from rehydration or live `row`
 *  events). Used as the `since_id` for resync rehydration. 0 means
 *  "never hydrated" — the first mount will fetch the initial seed. */
let lastSeenRowId = 0;

/** Connection state observed from WS events. The store does NOT own
 *  the WS — it just observes. Transitions:
 *  - initial: "disconnected"
 *  - on mount (first subscriber): "connecting" (we're about to
 *    rehydrate + subscribe)
 *  - on first row/stage event: "connected"
 *  - on lag_warning/resync: "connecting" (catching up)
 *  - on next row/stage after a lag: "connected" */
export type LiveConnectionState = "disconnected" | "connecting" | "connected";
let connectionState: LiveConnectionState = "disconnected";

// ----------------------------------------------------------------------------
// Reactivity (F5.6) + visibility pause (F5.7)
// ----------------------------------------------------------------------------

const subscribers: Set<() => void> = new Set<() => void>();
let updateScheduled = false;
let paused = false;
let visibilityListenerInstalled = false;
const UPDATE_THROTTLE_MS = 250;

/** Register a re-render callback. The store calls all subscribers on a
 *  throttled cadence (max 1 update per 250ms) after each `row` / `stage`
 *  event. Returns an unsubscribe function — call it when the view
 *  unmounts to avoid leaks. */
export function subscribe(fn: () => void): () => void {
  subscribers.add(fn);
  return () => {
    subscribers.delete(fn);
  };
}

function scheduleUpdate(): void {
  if (paused) return;
  if (updateScheduled) return;
  updateScheduled = true;
  setTimeout(() => {
    updateScheduled = false;
    if (paused) return;
    for (const fn of subscribers) {
      try {
        fn();
      } catch (e: unknown) {
        // Same defensive pattern as ws-bus: a single broken subscriber
        // must not break the store for the rest.
        console.error("[openproxy] live-store subscriber threw:", e);
      }
    }
  }, UPDATE_THROTTLE_MS);
}

function installVisibilityListener(): void {
  if (visibilityListenerInstalled) return;
  if (typeof document === "undefined") return;
  visibilityListenerInstalled = true;
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      // Pause subscriber notifications — the store still processes
      // events (keeps buckets + ring buffer current) but doesn't fire
      // re-renders while the tab is in the background. Saves layout
      // work the user can't see.
      paused = true;
    } else {
      paused = false;
      // If we were in the background for a while, force an update now
      // so the UI catches up to the current state.
      scheduleUpdate();
    }
  });
}

// ----------------------------------------------------------------------------
// Snapshot computation (F5.3)
// ----------------------------------------------------------------------------

/** Per-bucket throughput chart point. `t` is the bucket start time in
 *  ms since epoch. `rps` / `tps` / `cps` are per-second rates within
 *  the bucket (count / bucketSecs, etc.). */
export interface ThroughputPoint {
  t: number;
  rps: number;
  tps: number;
  cps: number;
}

/** Per-bucket status-code chart point. Counts (not rates) — the chart
 *  stacks 2xx / 4xx / 5xx per bucket. */
export interface StatusCodePoint {
  t: number;
  s2xx: number;
  s4xx: number;
  s5xx: number;
}

/** Per-bucket latency percentile chart point. `p50` / `p95` / `p99`
 *  are computed from this bucket's latencies (sort-and-index). */
export interface LatencyPoint {
  t: number;
  p50: number;
  p95: number;
  p99: number;
}

/** Aggregated race outcomes over the window. `won` is race winners
 *  (`race_lost === false` && `race_total > 1`); `lost` is race losers
 *  (`race_lost === true`); `single` is non-race rows (`race_total <= 1`). */
export interface RaceOutcomes {
  won: number;
  lost: number;
  single: number;
}

/** Reactive snapshot returned by `getSnapshot`. All time-series arrays
 *  are ordered oldest → newest. KPIs are computed over the whole
 *  window. */
export interface Snapshot {
  // KPIs
  activeRequests: number;
  requestsPerSec: number;
  tokensPerSec: number;
  costPerSec: number;
  successRate: number;  // 0..1
  avgLatencyMs: number;
  p50LatencyMs: number;
  p95LatencyMs: number;
  p99LatencyMs: number;
  raceWinRate: number;  // 0..1
  // Time series for charts (oldest → newest)
  throughput: ThroughputPoint[];
  statusCodes: StatusCodePoint[];
  latency: LatencyPoint[];
  raceOutcomes: RaceOutcomes;
  // Activity feed (most recent 20, newest first)
  recentRows: RecentUsageRow[];
}

/** Supported snapshot windows. 60s and 300s use the 1-second buckets
 *  (60 / 300 buckets respectively); 1800s uses the 5-second buckets
 *  (360 buckets). The 24h 1-minute buckets are not exposed via
 *  `getSnapshot` (no UI consumer yet) but are maintained for future
 *  use. */
export type SnapshotWindow = 60 | 300 | 1800;

function getWindowBuckets(windowSecs: SnapshotWindow): {
  buckets: Bucket[];
  bucketSecs: number;
  count: number;
} {
  if (windowSecs === 1800) {
    return { buckets: buckets5s, bucketSecs: 5, count: WINDOW_5S };
  }
  // 60 or 300 both use buckets1s
  const count = windowSecs === 60 ? 60 : WINDOW_1S;
  return { buckets: buckets1s, bucketSecs: 1, count };
}

/** Collect the last `count` buckets (oldest → newest) from a modulo-
 *  indexed ring buffer. The "current" bucket is derived from
 *  Date.now(); older buckets are `count - 1` indices behind it (with
 *  modulo wraparound). */
function collectWindow(windowSecs: SnapshotWindow): {
  buckets: Bucket[];
  bucketSecs: number;
  startMs: number;
} {
  const { buckets, bucketSecs, count } = getWindowBuckets(windowSecs);
  const totalBuckets = buckets.length;
  const nowSec = Math.floor(Date.now() / 1000);
  const currentIdx = ((Math.floor(nowSec / bucketSecs) % totalBuckets) + totalBuckets) % totalBuckets;
  // The current bucket's start time (ms since epoch).
  const currentBucketStartSec = Math.floor(nowSec / bucketSecs) * bucketSecs;
  const out: Bucket[] = [];
  for (let i = count - 1; i >= 0; i--) {
    const idx = (((currentIdx - i) % totalBuckets) + totalBuckets) % totalBuckets;
    // The non-null assertion is safe — the arrays are pre-allocated
    // and the modulo index is always in range.
    out.push(buckets[idx]!);
  }
  // The oldest bucket in `out` starts at `currentBucketStartSec - (count - 1) * bucketSecs`.
  const startMs = (currentBucketStartSec - (count - 1) * bucketSecs) * 1000;
  return { buckets: out, bucketSecs, startMs };
}

/** Percentile of a SORTED (ascending) array. `p` in [0, 1]. Uses the
 *  "nearest rank" method: idx = floor(p * n), clamped to [0, n-1].
 *  For n=0 returns 0. */
function percentileOfSorted(sortedAsc: number[], p: number): number {
  const n = sortedAsc.length;
  if (n === 0) return 0;
  const idx = Math.min(n - 1, Math.max(0, Math.floor(p * n)));
  // noUncheckedIndexedAccess: idx is clamped to [0, n-1], so the
  // access is safe. The `?? 0` is a defensive fallback.
  return sortedAsc[idx] ?? 0;
}

/** Concatenate all latencies from the window's buckets and compute the
 *  given percentile. O(n log n) per call — n is at most
 *  `count * MAX_LATENCIES_PER_BUCKET` (e.g. 300 * 1000 = 300k for a
 *  5-min window). Acceptable for the 4Hz snapshot cadence; see
 *  concerns for the t-digest alternative. */
function windowPercentile(windowBuckets: Bucket[], p: number): number {
  let total = 0;
  for (const b of windowBuckets) total += b.latencies.length;
  if (total === 0) return 0;
  const all: number[] = new Array<number>(total);
  let i = 0;
  for (const b of windowBuckets) {
    for (const lat of b.latencies) {
      all[i] = lat;
      i++;
    }
  }
  all.sort((a, c) => a - c);
  return percentileOfSorted(all, p);
}

/** Average latency across the window (mean of all samples). */
function windowAvgLatency(windowBuckets: Bucket[]): number {
  let sum = 0;
  let n = 0;
  for (const b of windowBuckets) {
    for (const lat of b.latencies) {
      sum += lat;
      n++;
    }
  }
  return n === 0 ? 0 : sum / n;
}

/** Compute and return a snapshot for the given window. Called by lit-html
 *  components on each throttled re-render. The snapshot is a fresh object
 *  graph each call (no caching) — callers can mutate freely. */
export function getSnapshot(windowSecs: SnapshotWindow): Snapshot {
  const { buckets: windowBuckets, bucketSecs, startMs } = collectWindow(windowSecs);

  // KPI aggregates over the whole window.
  let count = 0;
  let tokensIn = 0;
  let tokensOut = 0;
  let costUsd = 0;
  let s2xx = 0;
  let s4xx = 0;
  let s5xx = 0;
  let raceWins = 0;
  let raceTotal = 0;
  for (const b of windowBuckets) {
    count += b.count;
    tokensIn += b.tokens_in;
    tokensOut += b.tokens_out;
    costUsd += b.cost_usd;
    s2xx += b.status_2xx;
    s4xx += b.status_4xx;
    s5xx += b.status_5xx;
    raceWins += b.race_wins;
    raceTotal += b.race_total;
  }

  const windowSecsNum: number = windowSecs;
  const requestsPerSec = windowSecsNum > 0 ? count / windowSecsNum : 0;
  const tokensPerSec = windowSecsNum > 0 ? (tokensIn + tokensOut) / windowSecsNum : 0;
  const costPerSec = windowSecsNum > 0 ? costUsd / windowSecsNum : 0;
  const totalStatus = s2xx + s4xx + s5xx;
  const successRate = totalStatus > 0 ? s2xx / totalStatus : 0;
  const raceWinRate = raceTotal > 0 ? raceWins / raceTotal : 0;

  // Time series (oldest → newest). The first bucket in `windowBuckets`
  // is the oldest; its start time is `startMs`. Each subsequent bucket
  // is `bucketSecs` later.
  const throughput: ThroughputPoint[] = [];
  const statusCodes: StatusCodePoint[] = [];
  const latency: LatencyPoint[] = [];
  for (let i = 0; i < windowBuckets.length; i++) {
    const b = windowBuckets[i]!;
    const t = startMs + i * bucketSecs * 1000;
    const rps = bucketSecs > 0 ? b.count / bucketSecs : 0;
    const tps = bucketSecs > 0 ? (b.tokens_in + b.tokens_out) / bucketSecs : 0;
    const cps = bucketSecs > 0 ? b.cost_usd / bucketSecs : 0;
    throughput.push({ t, rps, tps, cps });
    statusCodes.push({ t, s2xx: b.status_2xx, s4xx: b.status_4xx, s5xx: b.status_5xx });
    // Per-bucket percentile: copy + sort this bucket's latencies.
    // Don't mutate the bucket's array (it's the live store's).
    const sortedLat = [...b.latencies].sort((a, c) => a - c);
    latency.push({
      t,
      p50: percentileOfSorted(sortedLat, 0.5),
      p95: percentileOfSorted(sortedLat, 0.95),
      p99: percentileOfSorted(sortedLat, 0.99),
    });
  }

  const raceOutcomes: RaceOutcomes = {
    won: raceWins,
    lost: raceTotal - raceWins,
    single: count - raceTotal,
  };

  // Activity feed: most recent 20, newest first. `recentRows` is
  // already newest-first; slice(0, 20) returns a fresh array.
  const recentRowsSlice: RecentUsageRow[] = recentRows.slice(0, 20);

  return {
    activeRequests: activeRequests.size,
    requestsPerSec,
    tokensPerSec,
    costPerSec,
    successRate,
    avgLatencyMs: windowAvgLatency(windowBuckets),
    p50LatencyMs: windowPercentile(windowBuckets, 0.5),
    p95LatencyMs: windowPercentile(windowBuckets, 0.95),
    p99LatencyMs: windowPercentile(windowBuckets, 0.99),
    raceWinRate,
    throughput,
    statusCodes,
    latency,
    raceOutcomes,
    recentRows: recentRowsSlice,
  };
}

// ----------------------------------------------------------------------------
// WS event handlers (F5.4, F5.8)
// ----------------------------------------------------------------------------

function isRecentUsageRowShape(x: unknown): x is RecentUsageRow {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  // The recent-usage row always has a `request_id` and a `created_at`.
  // Other fields can be null but these two are stable.
  return typeof o["request_id"] === "string" && typeof o["created_at"] === "string";
}

function isStageEventShape(x: unknown): x is StageEvent {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  return typeof o["request_id"] === "string" && typeof o["stage"] === "string";
}

/** Handle a `row` envelope (terminal usage row from the WS). Prepends
 *  to the activity feed, writes to all 3 bucket windows, removes any
 *  matching active-request entry, advances `lastSeenRowId`, and
 *  schedules a throttled re-render. */
function handleRow(msg: WsEnvelope): void {
  // The WS envelope puts the row in `data`; some legacy paths put it
  // in `row`. Match the logs view's fallback pattern.
  const candidate: unknown = msg.data ?? msg.row ?? msg;
  if (!isRecentUsageRowShape(candidate)) return;
  const row = candidate;
  prependRow(row);
  writeRowToBuckets(row);
  if (row.request_id) activeRequests.delete(row.request_id);
  if (typeof row.id === "number" && row.id > lastSeenRowId) {
    lastSeenRowId = row.id;
  }
  connectionState = "connected";
  scheduleUpdate();
}

/** Handle a `stage` envelope (in-flight stage transition from the WS).
 *  Non-terminal stages upsert into `activeRequests`; terminal stages
 *  ("completed" / "failed" / "cancelled") remove the entry. */
function handleStage(msg: WsEnvelope): void {
  const candidate: unknown = msg.data ?? msg;
  if (!isStageEventShape(candidate)) return;
  const event = candidate;
  if (!event.request_id) return;
  const stage: string = event.stage;
  if (stage === "completed" || stage === "failed" || stage === "cancelled") {
    activeRequests.delete(event.request_id);
  } else {
    // Upsert — overwrites any previous stage event for the same
    // request_id (we only care about the latest stage for the
    // active-requests count, not the history).
    activeRequests.set(event.request_id, event);
  }
  connectionState = "connected";
  scheduleUpdate();
}

/** Handle a `lag_warning` envelope. The server detected a broadcast
 *  `Lagged(_)` on the usage or stage channel and is about to send a
 *  `resync` envelope with `since_id`. We just flip connectionState to
 *  "connecting" here; the resync handler does the actual gap fetch.
 *  Notifications-channel lags are ignored (F4's responsibility). */
function handleLagWarning(msg: WsEnvelope): void {
  if (msg.channel === "notifications") return;
  connectionState = "connecting";
  scheduleUpdate();
}

/** Handle a `resync` envelope. The server is telling us to refetch any
 *  rows newer than `since_id` (the highest id it knows we've seen).
 *  Falls back to our own `lastSeenRowId` if `since_id` is missing. */
function handleResync(msg: WsEnvelope): void {
  connectionState = "connecting";
  const sinceId: number = typeof msg.since_id === "number"
    ? msg.since_id
    : lastSeenRowId;
  void rehydrateGap(sinceId);
  scheduleUpdate();
}

// ----------------------------------------------------------------------------
// Activity feed (F5.9) + rehydration (F5.4)
// ----------------------------------------------------------------------------

/** Prepend a row to the activity-feed ring buffer (newest first).
 *  Dedupes by `id` — if the row is already in the buffer (e.g. a
 *  rehydrate fetch overlaps with a live event), silently skips. Trims
 *  to MAX_RECENT_ROWS by dropping the oldest entry (last element). */
function prependRow(row: RecentUsageRow): void {
  const id: number = typeof row.id === "number" ? row.id : 0;
  if (id > 0 && recentRowIds.has(id)) return;
  recentRows.unshift(row);
  if (id > 0) recentRowIds.add(id);
  while (recentRows.length > MAX_RECENT_ROWS) {
    const dropped = recentRows.pop();
    if (dropped && typeof dropped.id === "number" && dropped.id > 0) {
      recentRowIds.delete(dropped.id);
    }
  }
}

/** Initial seed on first mount (lastSeenRowId === 0). Fetches the
 *  recent 100 rows from `GET /admin/api/usage/recent?limit=100` and
 *  prepends them to the ring buffer (in oldest-first order, so the
 *  buffer ends up newest-first). Sets `lastSeenRowId` to the highest
 *  id fetched.
 *
 *  NOTE: the server's `recent()` returns rows oldest-first when
 *  `since_id=0`, so this actually fetches the OLDEST 100 rows in the
 *  DB — a pre-existing API quirk. The activity feed shows whatever the
 *  API returns; the user sees stale data until new WS events arrive.
 *  See module docstring + concerns. */
async function rehydrateInitial(): Promise<void> {
  try {
    const rows = await api("/usage/recent?limit=100") as RecentUsageRow[] | null;
    if (!Array.isArray(rows) || rows.length === 0) return;
    // Server returns oldest-first; prepend in that order so the
    // buffer ends up newest-first (last prepended = front).
    let maxId = 0;
    const validRows: RecentUsageRow[] = [];
    for (const row of rows) {
      if (!isRecentUsageRowShape(row)) continue;
      validRows.push(row);
      if (typeof row.id === "number" && row.id > maxId) maxId = row.id;
    }
    if (validRows.length > 0) {
      // Merge with any WS rows that may have already arrived
      recentRows.push(...validRows);
      // Sort newest-first (descending by created_at)
      recentRows.sort((a, b) => {
        const ta = new Date(a.created_at!).getTime();
        const tb = new Date(b.created_at!).getTime();
        return tb - ta;
      });
      // Trim to max length
      if (recentRows.length > MAX_RECENT_ROWS) {
        recentRows.length = MAX_RECENT_ROWS;
      }
      // Rebuild the IDs set
      recentRowIds.clear();
      for (const row of recentRows) {
        if (typeof row.id === "number") {
          recentRowIds.add(row.id);
        }
      }
    }
    if (maxId > lastSeenRowId) lastSeenRowId = maxId;
    scheduleUpdate();
  } catch (e: unknown) {
    // Non-fatal: the store still works, just without an initial seed.
    // The activity feed starts empty and fills from live WS events.
    console.error("[openproxy] live-store initial rehydrate failed:", e);
  }
}

/** Gap-fill rehydration on `lag_warning` / `resync`. Fetches rows
 *  newer than `sinceId` from `GET /admin/api/usage/recent?since_id=…`
 *  and prepends them to the ring buffer. Updates `lastSeenRowId`.
 *
 *  Rehydrated rows seed the ring buffer ONLY, not the buckets — see
 *  module docstring for rationale. */
async function rehydrateGap(sinceId: number): Promise<void> {
  try {
    const since = Math.max(0, sinceId);
    const path = since > 0
      ? `/usage/recent?since_id=${encodeURIComponent(String(since))}&limit=500`
      : "/usage/recent?limit=500";
    const rows = await api(path) as RecentUsageRow[] | null;
    if (!Array.isArray(rows) || rows.length === 0) {
      // No gap to fill — we're already up to date. Flip back to
      // "connected" so the UI doesn't show a perpetual "connecting"
      // banner.
      connectionState = "connected";
      return;
    }
    let maxId = 0;
    for (const row of rows) {
      if (!isRecentUsageRowShape(row)) continue;
      prependRow(row);
      if (typeof row.id === "number" && row.id > maxId) maxId = row.id;
    }
    if (maxId > lastSeenRowId) lastSeenRowId = maxId;
    // After the gap fill, we're caught up — flip to "connected" on
    // the next live event (or immediately if we have one).
    connectionState = "connected";
    scheduleUpdate();
  } catch (e: unknown) {
    console.error("[openproxy] live-store gap rehydrate failed:", e);
    // Stay in "connecting" — the next live event will flip us to
    // "connected". The user sees a persistent "connecting" banner,
    // which is truthful (we couldn't confirm we're caught up).
  }
}

// ----------------------------------------------------------------------------
// Mount / unmount lifecycle (F5.5)
// ----------------------------------------------------------------------------

let mountCount = 0;
let storeOpenedWs = false;
let unsubRow: (() => void) | null = null;
let unsubStage: (() => void) | null = null;
let unsubLag: (() => void) | null = null;
let unsubResync: (() => void) | null = null;
let unsubConnection: (() => void) | null = null;

function handleConnectionStatus(status: LogsStatus): void {
  connectionState = status === "connected"
    ? "connected"
    : status === "connecting" || status === "reconnecting"
      ? "connecting"
      : "disconnected";
  scheduleUpdate();
}

/** Returns true if the live-logs WS is currently OPEN or CONNECTING.
 *  Used to decide whether the store should open the WS itself (when
 *  no other view is driving it) or rely on the existing connection. */
function isWsActive(): boolean {
  const ws = state.logs.ws;
  if (!ws) return false;
  return ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING;
}

/** Mount the live-store. The first consumer subscribes to ws-bus
 *  events and (if no other view is already driving the WS) opens the
 *  WS connection. Returns a disposer — call it when the view unmounts.
 *
 *  Idempotent: calling `mountLiveStore()` N times requires N
 *  `unmountLiveStore()` calls to fully tear down. The store stays
 *  mounted across quick navigations (home → logs → home) to keep the
 *  data warm. */
export function mountLiveStore(): () => void {
  mountCount++;
  if (mountCount === 1) {
    // First consumer — subscribe to WS events.
    unsubRow = subscribeWs("row", handleRow);
    unsubStage = subscribeWs("stage", handleStage);
    unsubLag = subscribeWs("lag_warning", handleLagWarning);
    unsubResync = subscribeWs("resync", handleResync);
    unsubConnection = subscribeLogsStatus(handleConnectionStatus);
    installVisibilityListener();
    // If no other view is driving the WS, open it ourselves. The
    // `storeOpenedWs` flag lets us close it on unmount only if we
    // were the one who opened it (avoids closing a WS the logs view
    // is still using).
    if (!isWsActive()) {
      connectLogsWebSocket();
      storeOpenedWs = true;
    } else {
      storeOpenedWs = false;
    }
    // Connection state: we're subscribed + (maybe) opening the WS.
    connectionState = "connecting";
    // Rehydrate if this is the first mount in the session.
    if (lastSeenRowId === 0) {
      void rehydrateInitial();
    }
  }
  return () => unmountLiveStore();
}

/** Unmount the live-store. Decrements the mount count; when it drops
 *  to 0, unsubscribes from ws-bus and (if the store opened the WS)
 *  closes it. Data is preserved (recentRows, buckets*, lastSeenRowId)
 *  so a quick remount shows existing aggregations immediately. */
export function unmountLiveStore(): void {
  mountCount = Math.max(0, mountCount - 1);
  if (mountCount === 0) {
    unsubRow?.();
    unsubRow = null;
    unsubStage?.();
    unsubStage = null;
    unsubLag?.();
    unsubLag = null;
    unsubResync?.();
    unsubResync = null;
    unsubConnection?.();
    unsubConnection = null;
    if (storeOpenedWs) {
      disconnectLogsWebSocket();
      storeOpenedWs = false;
    }
    // Don't clear the data — keep it warm for the next mount within
    // the session. `connectionState` stays as-is so a quick remount
    // shows the last-known state.
  }
}

// ----------------------------------------------------------------------------
// Public getters
// ----------------------------------------------------------------------------

export function getConnectionState(): LiveConnectionState {
  return connectionState;
}

export function getLastSeenRowId(): number {
  return lastSeenRowId;
}

// ----------------------------------------------------------------------------
// Tests / introspection
// ----------------------------------------------------------------------------
//
// TODO: unit tests once we have vitest. The existing test setup is
// Playwright e2e (no vitest config in package.json); adding a unit
// test runner is out of scope for F5. The e2e coverage from F6 (home
// view) will exercise the store end-to-end. In the meantime, the
// functions above are pure (no side effects beyond the in-memory
// state) and easily testable: feed `handleRow` / `handleStage`
// synthetic events via `dispatchWs`, then assert on `getSnapshot()`.
//
// The internal state (`recentRows`, `activeRequests`, `buckets*`,
// `lastSeenRowId`, `connectionState`) is intentionally NOT exported —
// consumers should read snapshots via `getSnapshot()` and connection
// state via `getConnectionState()`. Exposing internals would let
// callers mutate the store's invariants (e.g. pushing to `recentRows`
// without updating `recentRowIds`).
