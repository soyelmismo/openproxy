// Global state singleton, mutated in place by handlers and read by views.
// Non-trivial shapes line up with the manual types in `lib/types/api.ts`;
// looser runtime values get a narrow local union.

import type {
  Provider,
  Account,
  Model,
  Combo,
  FreeProxy,
  ProxySource,
  ApiKey,
} from "../lib/types/api.js";
import type { FormatsMetadata } from "../lib/types/common.js";

// Defined here so the `state` shape can name them without a circular import
// (router.ts and ws.ts import from state/, not the reverse).

/** Hash-routed view names. Keep in sync with `ROUTES` in `state/router.ts`. */
export type RouteName =
  | "home"
  | "providers"
  | "provider-detail"
  | "combos"
  | "combo-detail"
  | "keys"
  | "key-detail"
  | "key-usage"
  | "analytics"
  | "logs"
  | "debug-logs"
  | "config"
  | "notifications"
  | "login"
  | "proxies"
  | "proxy-sources"
  | "playground"
  | "models";

/** Live-logs WebSocket connection status. Mirrors the `setLogsStatus`
 *  labels in `state/ws.ts`. */
export type LogsStatus = "connected" | "connecting" | "reconnecting" | "disconnected";

/** Row id in the logs map: a string `request_id` from the WebSocket, a
 *  numeric `UsageId` from the long-poll feed. */
export type LogsRequestId = string;

/** One row of `POST /combos/:id/test-all`. Field-compatible with
 *  `POST /models/:id/test` so both render through `statusPillClass()`.
 *  `target_id` matches a row in the targets table; `row_id` is the upstream
 *  model row id and is informational only. */
export interface ComboTestResult {
  target_id: number;
  /** Set (and only set) for sub-combo targets — the fan-out skips them. */
  sub_combo_id?: number | null;
  sub_combo_name?: string | null;
  provider_id: string;
  account_id?: number | null;
  model_row_id?: number | null;
  model_id?: string;
  model_display_name?: string | null;
  /** HTTP status from the upstream probe (0 = network failure / skipped). */
  status: number;
  /** Round-trip wall clock in ms; `null` when skipped or never sent. */
  elapsed_ms: number | null;
  /** Error message from the upstream probe; `null` on success. */
  error_msg: string | null;
  /** `true` when the target was skipped (sub-combo, in cooldown, etc.). */
  skipped: boolean;
  /** Upstream model row id. Informational only — same as `model_row_id`. */
  row_id?: number;
}

/** Latest test-all results per combo id. Written by `testAllTargets` and
 *  never refetched on bg-poll, so values change only when the user re-runs
 *  the test. */
export type ComboTestResults = Record<number, ComboTestResult[]>;

export interface ProviderDetailUiState {
  filter?: string;
  search?: string;
  sort?: unknown | null;
  page?: number;
  pageSize?: number;
  [key: string]: unknown;
}
export type ProviderDetailUi = Record<string, ProviderDetailUiState>;

/** Live-logs sub-state.
 *
 *  The `stagesBy*` maps key by `trace_id`, not `request_id`: one client
 *  request fans out into several attempts (per-target retry, fallback, race
 *  losers), each with its own `trace_id` (`UsageInput.trace_id` in
 *  crates/openproxy-core/src/usage.rs). Keying by `request_id` bleeds the
 *  latest attempt's phase over historical rows of the same request.
 *  `stagesByRequestId` only catches `StageEvent`s that arrive with an empty
 *  `trace_id`. */
export interface LogsState {
  page: number;
  rowsPerPage: number;
  maxRows: number;
  followTail: boolean;
  status: LogsStatus;
  ws: WebSocket | null;
  reconnectAttempt: number;
  reconnectTimer: ReturnType<typeof setTimeout> | null;
  recording: boolean;
  recordingLoading: boolean;
  /** Visible LOG_COLUMNS keys. Mutated in place by the toggleColumn handler,
   *  so readers keep the same reference. Seeded from localStorage. */
  visibleColumns: Set<string> | null;

  selectedIdentity: { kind: "row_id", id: number } | { kind: "attempt", attemptKey: string } | null;
}

export interface ProxySummary {
  total: number;
  alive: number;
  dead: number;
  unknown: number;
  avg_latency_ms: number | null;
  sources: string[];
  protocols: string[];
}

/** Per-provider UI map (selection, test results), keyed by provider id. */
export interface DashboardState {
  // Cached server data, refreshed on navigate() and on bgPoll.
  providers: Provider[];
  accounts: Account[];
  models: Model[];
  modelsComplete: boolean;
  combos: Combo[];
  proxies: FreeProxy[];
  proxySources: ProxySource[];
  proxySummary: ProxySummary | null;
  /** Cached API key rows. @see crates/openproxy-core/src/api_keys.rs */
  apiKeys: ApiKey[];
  /** Health payload from /admin/health. `null` until the first
   *  tick resolves, or if the request fails. The bg-poll only
   *  reads `.status` (and `.message` for tooltips). */
  health: { status: string; message?: string } | null;

  // The view currently displayed. Used by `rerenderCurrentView`
  // so background polls can re-paint in place. `name` is null on
  // first paint before any hashchange fires.
  currentView: { name: RouteName | null; context: string | null };

  // Combo-target selection (multi-select delete in the targets
  // table). Lives here so it survives across the bgPoll re-render.
  selectedTargets: Set<unknown>;
  selectedTargetsCombo: number | null;

  // Provider-detail model selection (multi-select bulk actions in
  // the models table on the provider detail view). The set is
  // cleared whenever the user navigates to a different provider
  // (see views/provider-detail.js).
  selectedModels: Set<unknown>;
  selectedModelsProvider: string | null;

  // Per-provider UI state for the detail view: search box, filter
  // tab (all/active/inactive). Keyed by provider id so navigating
  // away and back preserves the user's filter.
  providerDetail: ProviderDetailUi;

  // The latest test-all results per combo id. We don't refetch on
  // poll — they only update when the user clicks Test all.
  comboTestResults: ComboTestResults;

  // In-flight model picker selection (used by the Keys view). The
  // "committed" set is encoded into the hidden input value; the
  // picker working set is rebuilt on open.
  modelPickerSelection: Set<string>;

  // Live-logs state. Heavy enough to warrant a sub-object.
  logs: LogsState;

  // Introspected API wire formats and provider formats from GET /admin/api/formats
  formats: FormatsMetadata | null;

  // Latency tracker for the last `api()` call (used by the health
  // pill in the sidebar).
  lastApiLatencyMs: number;

  // UI preferences (e.g. sidebar collapse state).
  ui?: { sidebarCollapsed?: boolean };

  // Internal bg-poll state. Mutated in place by bg-poll.ts; the
  // `__` prefix marks it as out-of-band. `__healthPollHandle` is
  // a `setTimeout` handle, so we type it as `ReturnType<typeof
  // setTimeout>` (number in browsers, Timeout in Node).
  __healthPollHandle: ReturnType<typeof setTimeout> | null;
  __healthPollActive: boolean;
  __healthPollRunning: boolean;
}

export const state: DashboardState = {
  ui: { sidebarCollapsed: false },
  formats: null,
  // Cached server data, refreshed on navigate() and on bgPoll.
  providers: [],
  accounts: [],
  models: [],
  modelsComplete: false,
  combos: [],
  proxies: [],
  proxySources: [],
  proxySummary: null,
  apiKeys: [],
  health: null,
  currentView: { name: null, context: null },
  // Combo-target selection (multi-select delete in the targets
  // table). Lives here so it survives across the bgPoll re-render.
  selectedTargets: new Set<unknown>(),
  selectedTargetsCombo: null,
  // Provider-detail model selection (multi-select bulk actions in
  // the models table on the provider detail view). The set is
  // cleared whenever the user navigates to a different provider
  // (see views/provider-detail.js).
  selectedModels: new Set<unknown>(),
  selectedModelsProvider: null,
  // Per-provider UI state for the detail view: search box, filter
  // tab (all/active/inactive). Keyed by provider id so navigating
  // away and back preserves the user's filter.
  providerDetail: {},
  // The latest test-all results per combo id. We don't refetch on
  // poll — they only update when the user clicks Test all.
  comboTestResults: {},
  // In-flight model picker selection (used by the Keys view). The
  // "committed" set is encoded into the hidden input value; the
  // picker working set is rebuilt on open.
  modelPickerSelection: new Set<string>(),
  // Live-logs state. Heavy enough to warrant a sub-object.
  logs: {
    page: 1,
    rowsPerPage: 50,
    maxRows: 500,
    followTail: true,
    status: "disconnected",
    ws: null,
    reconnectAttempt: 0,
    reconnectTimer: null,
    recording: false,
    recordingLoading: false,
    visibleColumns: null,
    selectedIdentity: null,
  },
  lastApiLatencyMs: 0,

  // Bg-poll internal state. `__healthPollHandle` is null on boot.
  __healthPollHandle: null,
  __healthPollActive: false,
  __healthPollRunning: false,
};

// Bg-poll interval handle, shared by the router and shell.
let pollHandle: ReturnType<typeof setTimeout> | null = null;

export function setPollHandle(h: ReturnType<typeof setTimeout> | null): void {
  pollHandle = h;
}
export function getPollHandle(): ReturnType<typeof setTimeout> | null {
  return pollHandle;
}
