import type { RecentUsageRow } from "../../lib/types/api.js";
import type { Bucket, SnapshotWindow } from "./types.js";

export function emptyBucket(): Bucket {
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

export function resetBucketInPlace(b: Bucket): void {
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

export const WINDOW_1S = 300;   // 5 min
export const WINDOW_5S = 360;   // 30 min
export const WINDOW_1M = 1440;  // 24h
export const MAX_LATENCIES_PER_BUCKET = 1000;
export const MAX_RECENT_ROWS = 1000;

export const buckets1s: Bucket[] = [];
export const buckets5s: Bucket[] = [];
export const buckets1m: Bucket[] = [];
for (let i = 0; i < WINDOW_1S; i++) buckets1s.push(emptyBucket());
for (let i = 0; i < WINDOW_5S; i++) buckets5s.push(emptyBucket());
for (let i = 0; i < WINDOW_1M; i++) buckets1m.push(emptyBucket());

let lastBucket1s = -1;
let lastBucket5s = -1;
let lastBucket1m = -1;

function bucketIndexFromNow(windowSecs: number, totalBuckets: number): number {
  const nowSec = Math.floor(Date.now() / 1000);
  return ((Math.floor(nowSec / windowSecs) % totalBuckets) + totalBuckets) % totalBuckets;
}

function incrementBucket(b: Bucket, row: RecentUsageRow): void {
  b.count++;
  const isSuccess = row.status_code >= 200 && row.status_code < 400;
  if (isSuccess) {
    b.tokens_in += row.prompt_tokens ?? 0;
    b.tokens_out += row.completion_tokens ?? 0;
    b.cost_usd += row.cost_usd ?? 0;
    b.status_2xx++;
  } else if (row.status_code >= 400 && row.status_code < 500) {
    b.status_4xx++;
  } else if (row.status_code >= 500) {
    b.status_5xx++;
  }
  if (b.latencies.length < MAX_LATENCIES_PER_BUCKET) {
    b.latencies.push(row.total_ms || 0);
  }
  const raceSize: number = row.race_total ?? 0;
  if (raceSize > 1) {
    b.race_total++;
    if (!row.race_lost) b.race_wins++;
  }
}

export function writeRowToBuckets(row: RecentUsageRow): void {
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
  incrementBucket(buckets1s[idx1s]!, row);
  incrementBucket(buckets5s[idx5s]!, row);
  incrementBucket(buckets1m[idx1m]!, row);
}

export interface WindowBuckets {
  buckets: Bucket[];
  bucketSecs: number;
  count: number;
}

export interface CollectedWindow {
  buckets: Bucket[];
  bucketSecs: number;
  startMs: number;
}

export function getWindowBuckets(windowSecs: SnapshotWindow): WindowBuckets {
  if (windowSecs === 1800) {
    return { buckets: buckets5s, bucketSecs: 5, count: WINDOW_5S };
  }
  const count = windowSecs === 60 ? 60 : WINDOW_1S;
  return { buckets: buckets1s, bucketSecs: 1, count };
}

export function collectWindow(windowSecs: SnapshotWindow): CollectedWindow {
  const { buckets, bucketSecs, count } = getWindowBuckets(windowSecs);
  const totalBuckets = buckets.length;
  const nowSec = Math.floor(Date.now() / 1000);
  const currentIdx = ((Math.floor(nowSec / bucketSecs) % totalBuckets) + totalBuckets) % totalBuckets;
  const currentBucketStartSec = Math.floor(nowSec / bucketSecs) * bucketSecs;
  const out: Bucket[] = [];
  for (let i = count - 1; i >= 0; i--) {
    const idx = (((currentIdx - i) % totalBuckets) + totalBuckets) % totalBuckets;
    out.push(buckets[idx]!);
  }
  const startMs = (currentBucketStartSec - (count - 1) * bucketSecs) * 1000;
  return { buckets: out, bucketSecs, startMs };
}

export function percentileOfSorted(sortedAsc: number[], p: number): number {
  const n = sortedAsc.length;
  if (n === 0) return 0;
  const idx = Math.min(n - 1, Math.max(0, Math.floor(p * n)));
  return sortedAsc[idx] ?? 0;
}

export function windowPercentile(windowBuckets: Bucket[], p: number): number {
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

export function windowAvgLatency(windowBuckets: Bucket[]): number {
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
