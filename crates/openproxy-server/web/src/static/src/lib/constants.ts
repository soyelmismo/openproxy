// lib/constants.ts — app-wide constants. Kept here so the views
// and handlers do not litter the codebase with magic strings/numbers.

import type { PriorityMode, CooldownMode } from "./types/api.js";

// Human-readable label for each server-side stage. The server keys
// are kept in the data-stage attribute (and CSS) so styling can
// target them directly; the cell body shows the friendlier label.
export const STAGE_LABELS: Readonly<Record<string, string>> = {
  started: "procesando payload",
  connecting: "conectando a upstream",
  waiting_ttft: "esperando ttft",
  streaming: "recibiendo streaming",
  completed: "completado",
  failed: "falló",
  cancelled: "cancelado",
};

// Live logs WS reconnect backoff in ms.
//
// TRIPLE-FIX (Bug 1): the first reconnect delay was 1000ms, which
// made the live dashboard show "⚠ Disconnected from real-time
// stream" for ~1s after every transient failure (e.g. the first
// attempt hitting a 401 because the token wasn't yet attached to
// the WS upgrade URL, or a network blip). Reduced the first delay
// to 250ms so the dashboard recovers within a quarter-second on
// transient failures, then back off progressively: 250 → 500 →
// 1s → 2s → 5s → 10s → 30s. The 30s cap is preserved so a
// permanently-down server doesn't trigger a tight retry loop.
//
// `connectLogsWebSocket()` is itself invoked synchronously from
// `initNotificationsStore()` (which is called by the sidebar's
// `maybeBootstrapNotifications()` gate on the first render after
// login), so the very first connection attempt happens immediately
// on login — these delays only govern retries AFTER a failure.
export const LOGS_WS_RECONNECT_DELAYS: readonly number[] = [250, 500, 1000, 2000, 5000, 10000, 30000];

// Local-storage key for the user theme choice.
export const THEME_STORAGE_KEY = "openproxy-theme";

// Local-storage key for the visible-columns choice on the /logs
// view. Value is a JSON array of column keys (e.g. ["time","phase"]).
export const LOGS_VISIBLE_COLUMNS_STORAGE_KEY = "openproxy:logs:visibleColumns";

// Definition of every log-row column, in the order they appear in
// the table. The `key` matches the existing CSS class `.log-{key}`
// on the span (e.g. "time" → `.log-time`), and the `label` is the
// header text. Adding a new column = add an entry here and the
// matching span in components/log-row.js.
export interface LogColumn {
  readonly key: string;
  readonly label: string;
}

export const LOG_COLUMNS: readonly LogColumn[] = [
  { key: "time",     label: "Time"     },
  { key: "phase",    label: "Phase"    },
  { key: "client",   label: "Client"   },
  { key: "status",   label: "Status"   },
  { key: "provider", label: "Provider" },
  { key: "model",    label: "Model"    },
  { key: "tokens",   label: "Tokens"   },
  { key: "latency",  label: "Latency"  },
  { key: "cost",     label: "Cost"     },
  { key: "compression", label: "Compress" },
];

export const PRIORITY_MODE_LABELS: Record<PriorityMode, string> = {
  strict: "Strict", lkgp: "LKGP", weighted: "Weighted",
  least_used: "Least Used", p2c: "P2C",
};

export const PRIORITY_MODE_TOOLTIPS: Record<PriorityMode, string> = {
  strict: "Walk targets in manual priority order. The first healthy target is always tried first.",
  lkgp: "Least Known Good Provider — prefer the target with the most recent successful request. Falls back to priority order for never-tried targets. An exploration rate adds priority-weighted randomness: earlier targets (which the operator positioned first for speed/intelligence) are more likely to be explored than later fallback targets.",
  weighted: "Weighted random selection — each target's probability is proportional to its weight. Set weights in the targets table below.",
  least_used: "Prefer the target with the fewest total requests in the selection window. Useful for distributing load evenly.",
  p2c: "Power of Two Choices — pick two random targets, choose the one with fewer recent failures. Good balance of simplicity and load distribution.",
};

export const COOLDOWN_MODE_TOOLTIPS: Record<CooldownMode, string> = {
  flat: "Fixed cooldown duration after each failure. The target is parked for the same amount of time regardless of how many times it has failed.",
  exponential: "Cooldown grows with each failure: base × factor^(failures-1), capped at max. A flapping target gets progressively longer cooldowns, giving it time to recover.",
};

// Localised status -> CSS class for the status-pill component.
export function statusPillClass(code: number | null): string {
  if (code == null) return "lost";
  if (code >= 500) return "err";
  if (code >= 400) return "warn";
  if (code >= 200 && code < 300) return "ok";
  if (code === 0) return "lost";
  return "lost";
}

// Built-in provider ids and quota-capable lists have been removed.
// The UI now uses `provider.metadata` to determine built-in, deletable, and quota support.
