// lib/constants.ts — app-wide constants. Kept here so the views
// and handlers do not litter the codebase with magic strings/numbers.

import type { StageEvent } from "./types/api.js";

// OAuth-capable provider ids. The provider-detail view uses these
// to decide when to show the login section.
const OAUTH_ALL = ["antigravity", "antigravity-cli", "kiro"] as const;
const OAUTH_PKCE = ["antigravity", "antigravity-cli"] as const;
const OAUTH_DEVICE = ["kiro"] as const;

export const OAUTH_PROVIDER_IDS: readonly string[] = OAUTH_ALL;
export const OAUTH_PKCE_PROVIDERS: readonly string[] = OAUTH_PKCE;
export const OAUTH_DEVICE_CODE_PROVIDERS: readonly string[] = OAUTH_DEVICE;

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
};

// Live logs WS reconnect backoff in ms.
export const LOGS_WS_RECONNECT_DELAYS: readonly number[] = [1000, 2000, 4000, 8000, 16000, 30000];

// Compute the small "sublabel" string shown next to the stage label
// in the live-log row (e.g. `connect 412ms`, `ttft 871ms`,
// `total 4231ms`, `12500ms stale`, or the live `elapsed_ms`).
//
// Priority:
//  1. Terminal stages (`completed` / `failed`) — show the row's
//     `total_ms` if known, otherwise the stage's `elapsed_ms`.
//  2. Non-terminal with a `total_ms` — the row is finalized but the
//     stage hasn't caught up; surface that as `${total}ms stale`
//     (matches §4.1 in the spec).
//  3. `streaming` with `ttft_ms` — show `ttft`.
//  4. `waiting_ttft` / `streaming` with `connect_ms` — show `connect`.
//  5. Fallback: live `elapsed_ms`.
//
// Used by both `components/log-row.ts` (the row HTML renderer) and
// `state/ticker.ts` (the live counter) so the two sublabels stay in
// sync as the row moves through its lifecycle.
export function phaseSublabel(stage: StageEvent, totalMs?: number | null): string {
  if (stage.stage === "completed" || stage.stage === "failed") {
    return (totalMs != null && totalMs > 0)
      ? `total ${totalMs}ms`
      : `${stage.elapsed_ms || 0}ms`;
  }
  if (totalMs != null && totalMs > 0) {
    return `${totalMs}ms stale`;
  }
  if (stage.stage === "streaming" && stage.ttft_ms != null) {
    return `ttft ${stage.ttft_ms}ms`;
  }
  if ((stage.stage === "waiting_ttft" || stage.stage === "streaming") && stage.connect_ms != null) {
    return `connect ${stage.connect_ms}ms`;
  }
  return `${stage.elapsed_ms || 0}ms`;
}

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
  { key: "status",   label: "Status"   },
  { key: "provider", label: "Provider" },
  { key: "model",    label: "Model"    },
  { key: "tokens",   label: "Tokens"   },
  { key: "latency",  label: "Latency"  },
  { key: "cost",     label: "Cost"     },
];

// Localised status -> CSS class for the status-pill component.
export function statusPillClass(code: number | null): string {
  if (code == null) return "lost";
  if (code >= 500) return "err";
  if (code >= 400) return "warn";
  if (code >= 200 && code < 300) return "ok";
  if (code === 0) return "lost";
  return "lost";
}

// Built-in provider ids — these cannot be deleted from the UI
// (the cascade would lose the server-side adapter). Mirrors the
// old app.js BUILTIN_PROVIDER_IDS list.
export const BUILTIN_PROVIDER_IDS: readonly string[] = ["openrouter", "minimax", "opencode-zen"];

// Providers that expose a quota fetcher (POST .../refresh-quota).
// The server is the source of truth via `quota_capable_providers`,
// but we mirror the list client-side so the confirm dialog and the
// "not supported by this provider" hint only appear when there is
// actually something to refresh.
export const QUOTA_CAPABLE_PROVIDERS: readonly string[] = ["minimax", "minimax-cn", "openrouter", "antigravity", "antigravity-cli", "agy"];
export const providerHasQuota = (providerId: string): boolean => QUOTA_CAPABLE_PROVIDERS.includes(providerId);
