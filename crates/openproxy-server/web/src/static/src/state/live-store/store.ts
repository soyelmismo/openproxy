import type { WsEnvelope } from "../../views/logs.js";
import type { RecentUsageRow, StageEvent } from "../../lib/types/api.js";
import { subscribeWs } from "../ws-bus.js";
import {
  connectLogsWebSocket,
  disconnectLogsWebSocket,
  subscribeLogsStatus,
  type LogsStatus,
} from "../ws.js";
import { state } from "../index.js";
import { api } from "../../lib/api.js";
import type {
  LatencyPoint,
  LiveConnectionState,
  RaceOutcomes,
  Snapshot,
  SnapshotWindow,
  StatusCodePoint,
  ThroughputPoint,
} from "./types.js";
import {
  collectWindow,
  percentileOfSorted,
  windowAvgLatency,
  windowPercentile,
  writeRowToBuckets,
  MAX_RECENT_ROWS,
} from "./buckets.js";

const recentRows: RecentUsageRow[] = [];
const recentRowIds: Set<number> = new Set<number>();
const activeRequests: Map<string, StageEvent> = new Map<string, StageEvent>();
let lastSeenRowId = 0;
let connectionState: LiveConnectionState = "disconnected";

const subscribers: Set<() => void> = new Set<() => void>();
let updateScheduled = false;
let paused = false;
let visibilityListenerInstalled = false;
const UPDATE_THROTTLE_MS = 250;

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
      paused = true;
    } else {
      paused = false;
      scheduleUpdate();
    }
  });
}

export function getSnapshot(windowSecs: SnapshotWindow): Snapshot {
  const { buckets: windowBuckets, bucketSecs, startMs } = collectWindow(windowSecs);

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

function isRecentUsageRowShape(x: unknown): x is RecentUsageRow {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  return typeof o["request_id"] === "string" && typeof o["created_at"] === "string";
}

function isStageEventShape(x: unknown): x is StageEvent {
  if (!x || typeof x !== "object") return false;
  const o = x as Record<string, unknown>;
  return typeof o["request_id"] === "string" && typeof o["stage"] === "string";
}

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

function handleRow(msg: WsEnvelope): void {
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

function handleStage(msg: WsEnvelope): void {
  const candidate: unknown = msg.data ?? msg;
  if (!isStageEventShape(candidate)) return;
  const event = candidate;
  if (!event.request_id) return;
  const stage: string = event.stage;
  if (stage === "completed" || stage === "failed" || stage === "cancelled") {
    activeRequests.delete(event.request_id);
  } else {
    activeRequests.set(event.request_id, event);
  }
  connectionState = "connected";
  scheduleUpdate();
}

function handleLagWarning(msg: WsEnvelope): void {
  if (msg.channel === "notifications") return;
  connectionState = "connecting";
  scheduleUpdate();
}

function handleResync(msg: WsEnvelope): void {
  connectionState = "connecting";
  const sinceId: number = typeof msg.since_id === "number"
    ? msg.since_id
    : lastSeenRowId;
  void rehydrateGap(sinceId);
  scheduleUpdate();
}

async function rehydrateInitial(): Promise<void> {
  try {
    const rows = (await api("/usage/recent?limit=100")) as RecentUsageRow[] | null;
    if (!Array.isArray(rows) || rows.length === 0) return;
    let maxId = 0;
    const validRows: RecentUsageRow[] = [];
    for (const row of rows) {
      if (!isRecentUsageRowShape(row)) continue;
      validRows.push(row);
      if (typeof row.id === "number" && row.id > maxId) maxId = row.id;
    }
    if (validRows.length > 0) {
      recentRows.push(...validRows);
      recentRows.sort((a, b) => {
        const ta = new Date(a.created_at!).getTime();
        const tb = new Date(b.created_at!).getTime();
        return tb - ta;
      });
      if (recentRows.length > MAX_RECENT_ROWS) {
        recentRows.length = MAX_RECENT_ROWS;
      }
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
    console.error("[openproxy] live-store initial rehydrate failed:", e);
  }
}

async function rehydrateGap(sinceId: number): Promise<void> {
  try {
    const since = Math.max(0, sinceId);
    const path = since > 0
      ? `/usage/recent?since_id=${encodeURIComponent(String(since))}&limit=500`
      : "/usage/recent?limit=500";
    const rows = (await api(path)) as RecentUsageRow[] | null;
    if (!Array.isArray(rows) || rows.length === 0) {
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
    connectionState = "connected";
    scheduleUpdate();
  } catch (e: unknown) {
    console.error("[openproxy] live-store gap rehydrate failed:", e);
  }
}

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

function isWsActive(): boolean {
  const ws = state.logs.ws;
  if (!ws) return false;
  return ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING;
}

export function mountLiveStore(): () => void {
  mountCount++;
  if (mountCount === 1) {
    unsubRow = subscribeWs("row", handleRow);
    unsubStage = subscribeWs("stage", handleStage);
    unsubLag = subscribeWs("lag_warning", handleLagWarning);
    unsubResync = subscribeWs("resync", handleResync);
    unsubConnection = subscribeLogsStatus(handleConnectionStatus);
    installVisibilityListener();
    if (!isWsActive()) {
      connectLogsWebSocket();
      storeOpenedWs = true;
    } else {
      storeOpenedWs = false;
    }
    connectionState = "connecting";
    if (lastSeenRowId === 0) {
      void rehydrateInitial();
    }
  }
  return () => unmountLiveStore();
}

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
  }
}

export function getConnectionState(): LiveConnectionState {
  return connectionState;
}

export function getLastSeenRowId(): number {
  return lastSeenRowId;
}
