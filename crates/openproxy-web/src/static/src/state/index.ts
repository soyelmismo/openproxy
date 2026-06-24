// state/index.ts — global state singleton. Mutated in place by the
// handlers; views read it on render. Mirrors the original
// `state` object in app.js.
//
// All non-trivial shapes here line up with the manual types in
// `lib/types/api.ts`. Where the runtime state holds something
// looser than the server-side type (e.g. `currentView.name` can
// be `null` on first paint, `modelPickerSelection` is a Set of
// strings not a model row id), we use a narrow local union.

import type {
  Provider,
  Account,
  Model,
  Combo,
  StageEvent,
  RecentUsageRow,
} from "../lib/types/api.js";

// ----------------------------------------------------------------------------
// Shared route + connection status unions. Defined here so the
// `state` shape can reference them without a circular import
// (router.ts and ws.ts import from state/, not the other way
// round). They are re-exported below for ergonomics.
// ----------------------------------------------------------------------------

/** Hash-routed view names. Mirrors the `ROUTES` array in
 *  `state/router.ts` — keep them in sync. */
export type RouteName =
  | "home"
  | "providers"
  | "provider-detail"
  | "combos"
  | "combo-detail"
  | "keys"
  | "key-usage"
  | "analytics"
  | "logs"
  | "debug-logs"
  | "config";

/** Live-logs WebSocket connection status. Mirrors the `setLogsStatus`
 *  labels in `state/ws.ts`. */
export type LogsStatus = "connected" | "connecting" | "reconnecting" | "disconnected";

/** A row id used in the logs map. The WebSocket hands us a string
 *  request_id; the long-poll feed gives us numeric `UsageId`. The
 *  maps in `state.logs` key by string for the in-flight WS feed
 *  and by `RecentUsageRow.id` for the persisted rows. We keep
 *  the maps narrowly typed where we know the shape. */
export type LogsRequestId = string;

/** The latest test-all results per combo id. Populated by
 *  handlers/* (out of G3 scope) when the user clicks Test all.
 *  The shape is the same as `state.logs.testResults` upstream —
 *  we type it as `unknown` here and let the consumers narrow. */
export type ComboTestResults = Record<number, unknown>;

/** Per-provider UI state for the detail view: search box, filter
 *  tab (all/active/inactive). Keyed by provider id so navigating
 *  away and back preserves the user's filter. The shape is open
 *  (views/* set whatever they need); we keep it loose on purpose. */
export type ProviderDetailUi = Record<string, Record<string, unknown>>;

/** Shape of the live-logs sub-state. Mirrors the `state.logs`
 *  literal in the original `state/index.js`.
 *
 *  Note on the `stagesBy*` maps: a single client request can fan
 *  out into multiple pipeline attempts (per-target retry, fallback
 *  to the next combo target, race losers still get a row). Each
 *  attempt has its own `trace_id` (per the `UsageInput.trace_id`
 *  column in `crates/openproxy-core/src/usage.rs:758`), so we key
 *  the live stage map by `trace_id` to keep per-attempt phase
 *  labels isolated. Keying by `request_id` — as the original code
 *  did — bleeds the latest attempt's phase over every historical
 *  row of the same `request_id`, which is the user-visible bug
 *  "retries duplicate counters on the failed entries".
 *  `stagesByRequestId` is kept (and only written) as a
 *  compatibility fallback for the rare case where a `StageEvent`
 *  arrives with an empty `trace_id` (it then keys by `request_id`
 *  to avoid losing the signal entirely). */
export interface LogsState {
  rows: RecentUsageRow[];
  rowById: Map<number, RecentUsageRow>;
  lastSeenId: number;
  /** Primary: keyed by `trace_id` (per attempt). */
  stagesByTraceId: Map<string, StageEvent>;
  /** Fallback: keyed by `request_id` for stage events with an
   *  empty `trace_id` (very rare; can happen for synthetic events
   *  emitted from inside the frontend itself). */
  stagesByRequestId: Map<LogsRequestId, StageEvent>;
  /** Per-attempt inflight placeholders, keyed by `trace_id`. The
   *  original code keyed this by `request_id` which again caused
   *  retry attempts to overwrite the in-flight placeholder of the
   *  first attempt. The view renders these mixed with the real
   *  rows (see `renderLogsRows` in `views/logs.ts`). */
  inflightByTraceId: Map<string, RecentUsageRow>;
  /** Fallback: inflight placeholders for trace_id-less rows. */
  inflightByRequestId: Map<LogsRequestId, RecentUsageRow>;
  liveTokens: Map<LogsRequestId, number>;
  selectedRow: RecentUsageRow | null;
  page: number;
  rowsPerPage: number;
  maxRows: number;
  followTail: boolean;
  status: LogsStatus;
  ws: WebSocket | null;
  reconnectAttempt: number;
  reconnectTimer: ReturnType<typeof setTimeout> | null;
  latencyTickerHandle: ReturnType<typeof setInterval> | null;
  /** Interval handle for the stale-inflight reaper (cleans up ghost
   *  placeholders left by lost terminal events / dropped usage rows). */
  staleReaperHandle: ReturnType<typeof setInterval> | null;
  recording: boolean;
  recordingLoading: boolean;
  /** Set of column keys (matching LOG_COLUMNS[].key) that the user
   *  wants to see in the table. Defaults to `null` and is replaced
   *  by a `Set<string>` from localStorage at startup by
   *  views/logs.js. The set is mutated in place by the
   *  toggleColumn handler so the rest of the code can keep
   *  reading the same reference. */
  visibleColumns: Set<string> | null;
}

/** Shape of the cached provider-detail sub-state (per-provider
 *  selection + test results). The per-provider UI map is keyed
 *  by provider id and the test results are keyed by combo id. */
export interface DashboardState {
  // Cached server data, refreshed on navigate() and on bgPoll.
  providers: Provider[];
  accounts: Account[];
  models: Model[];
  combos: Combo[];
  /** Cached API key rows. The shape is provider-specific; the
   *  dashboard views hydrate it from `/admin/api-keys`. Kept
   *  loose here (out of G3 scope — G4 will narrow it). */
  apiKeys: unknown[];
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

  // Latency tracker for the last `api()` call (used by the health
  // pill in the sidebar).
  lastApiLatencyMs: number;

  // Internal bg-poll state. Mutated in place by bg-poll.ts; the
  // `__` prefix marks it as out-of-band. `__healthPollHandle` is
  // a `setTimeout` handle, so we type it as `ReturnType<typeof
  // setTimeout>` (number in browsers, Timeout in Node).
  __healthPollHandle: ReturnType<typeof setTimeout> | null;
  __healthPollActive: boolean;
  __healthPollRunning: boolean;
}

export const state: DashboardState = {
  // Cached server data, refreshed on navigate() and on bgPoll.
  providers: [],
  accounts: [],
  models: [],
  combos: [],
  apiKeys: [],
  health: null,
  // The view currently displayed. Used by `rerenderCurrentView` so
  // background polls can re-paint in place.
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
    rows: [],
    rowById: new Map<number, RecentUsageRow>(),
    lastSeenId: 0,
    stagesByTraceId: new Map<string, StageEvent>(),
    stagesByRequestId: new Map<LogsRequestId, StageEvent>(),
    inflightByTraceId: new Map<string, RecentUsageRow>(),
    inflightByRequestId: new Map<LogsRequestId, RecentUsageRow>(),
    liveTokens: new Map<LogsRequestId, number>(),
    selectedRow: null,
    page: 1,
    rowsPerPage: 50,
    maxRows: 500,
    followTail: true,
    status: "disconnected",
    ws: null,
    reconnectAttempt: 0,
    reconnectTimer: null,
    latencyTickerHandle: null,
    staleReaperHandle: null,
    recording: false,
    recordingLoading: false,
    visibleColumns: null,
  },
  // Latency tracker for the last `api()` call (used by the health
  // pill in the sidebar).
  lastApiLatencyMs: 0,

  // Bg-poll internal state. `__healthPollHandle` is null on boot.
  __healthPollHandle: null,
  __healthPollActive: false,
  __healthPollRunning: false,
};

// Bg-poll interval handle. We re-use the same window flag so the
// router / shell can call `startBgPoll()` / `stopBgPoll()` safely.
let pollHandle: ReturnType<typeof setTimeout> | null = null;

export function setPollHandle(h: ReturnType<typeof setTimeout> | null): void {
  pollHandle = h;
}
export function getPollHandle(): ReturnType<typeof setTimeout> | null {
  return pollHandle;
}
