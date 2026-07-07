// lib/constants.ts — app-wide constants. Kept here so the views
// and handlers do not litter the codebase with magic strings/numbers.

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
export const QUOTA_CAPABLE_PROVIDERS: readonly string[] = ["minimax", "minimax-cn", "openrouter", "antigravity", "agy", "kiro", "codex"];
export const providerHasQuota = (providerId: string): boolean => QUOTA_CAPABLE_PROVIDERS.includes(providerId);
