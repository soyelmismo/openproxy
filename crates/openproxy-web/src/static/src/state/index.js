// state/index.js — global state singleton. Mutated in place by the
// handlers; views read it on render. Mirrors the original
// `state` object in app.js.

export const state = {
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
  selectedTargets: new Set(),
  selectedTargetsCombo: null,
  // Provider-detail model selection (multi-select bulk actions in
  // the models table on the provider detail view). The set is
  // cleared whenever the user navigates to a different provider
  // (see views/provider-detail.js).
  selectedModels: new Set(),
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
  modelPickerSelection: new Set(),
  // Live-logs state. Heavy enough to warrant a sub-object.
  logs: {
    rows: [],
    rowById: new Map(),
    lastSeenId: 0,
    stagesByRequestId: new Map(),
    inflightByRequestId: new Map(),
    liveTokens: new Map(),
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
    recording: false,
    recordingLoading: false,
    // Set of column keys (matching LOG_COLUMNS[].key) that the user
    // wants to see in the table. Defaults to all keys; overwritten
    // from localStorage at startup by views/logs.js. The set is
    // mutated in place by the toggleColumn handler so the rest of
    // the code can keep reading the same reference.
    visibleColumns: null,
  },
  // Latency tracker for the last `api()` call (used by the health
  // pill in the sidebar).
  lastApiLatencyMs: 0,
};

// Bg-poll interval handle. We re-use the same window flag so the
// router / shell can call `startBgPoll()` / `stopBgPoll()` safely.
let pollHandle = null;

export function setPollHandle(h) { pollHandle = h; }
export function getPollHandle() { return pollHandle; }
