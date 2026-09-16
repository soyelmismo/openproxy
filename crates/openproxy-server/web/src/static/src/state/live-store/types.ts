import type { RecentUsageRow } from "../../lib/types/api.js";

export interface Bucket {
  count: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd: number;
  status_2xx: number;
  status_4xx: number;
  status_5xx: number;
  latencies: number[];
  race_wins: number;
  race_total: number;
}

export interface ThroughputPoint {
  t: number;
  rps: number;
  tps: number;
  cps: number;
}

export interface StatusCodePoint {
  t: number;
  s2xx: number;
  s4xx: number;
  s5xx: number;
}

export interface LatencyPoint {
  t: number;
  p50: number;
  p95: number;
  p99: number;
}

export interface RaceOutcomes {
  won: number;
  lost: number;
  single: number;
}

export interface Snapshot {
  activeRequests: number;
  requestsPerSec: number;
  tokensPerSec: number;
  costPerSec: number;
  successRate: number;
  avgLatencyMs: number;
  p50LatencyMs: number;
  p95LatencyMs: number;
  p99LatencyMs: number;
  raceWinRate: number;
  throughput: ThroughputPoint[];
  statusCodes: StatusCodePoint[];
  latency: LatencyPoint[];
  raceOutcomes: RaceOutcomes;
  recentRows: RecentUsageRow[];
}

export type SnapshotWindow = 60 | 300 | 1800;

export type LiveConnectionState = "disconnected" | "connecting" | "connected";
