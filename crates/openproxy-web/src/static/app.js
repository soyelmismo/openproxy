// Client-side mirror of the server's built-in provider list. Kept in
// sync with `seed::builtin_provider_ids()` in `openproxy-core`; the
// server is the source of truth (the delete endpoint will reject any
// id in this list with a 400) and this list is the optimistic UI
// hint that hides the "Delete" button for those rows.
const BUILTIN_PROVIDER_IDS = ['openrouter', 'minimax', 'opencode-zen'];

// Client-side mirror of the server's `quota::quota_capable_providers`
// list. The server is the source of truth — calling
// `POST /v1/admin/accounts/:id/refresh-quota` for a non-capable
// provider still returns 200 with `{"supported": false}` — but
// keeping this list here lets the UI hide the quota controls
// entirely (button, column content) instead of showing them greyed
// out only to click through to a no-op.
const QUOTA_CAPABLE_PROVIDERS = ['minimax', 'minimax-cn', 'openrouter', 'antigravity', 'antigravity-cli', 'agy'];

// Predicate used by the provider-detail view: does this provider
// have a quota fetcher on the server?
const providerHasQuota = (providerId) => QUOTA_CAPABLE_PROVIDERS.includes(providerId);

// App state. Holds the latest snapshot of every dashboard resource
// plus the bookkeeping the background-refresh and view-rerender
// machinery needs.
const state = {
  providers: [],
  accounts: [],
  models: [],
  combos: [],
  apiKeys: [],
  health: null,
  logs: {
    rows: [],
    rowById: new Map(),
    lastSeenId: 0,
    ws: null,
    reconnectAttempt: 0,
    reconnectTimer: null,
    status: 'disconnected',
    selectedRow: null,
    liveTokens: new Map(),
    /// Per-request stage timeline for the live-log "millisecond
    /// debug" view. Key is `request_id`, value is the most recent
    /// `StageEvent` payload received for that request. The
    /// dashboard renders `state.logs.stagesByRequestId.get(rid)` in
    /// the row's "phase" cell so the operator can watch a request
    /// transition through `started → connecting → waiting_ttft →
    /// streaming → completed` in real time.
    ///
    /// This is intentionally *append-only* within a request's
    /// lifetime: the latest event wins (it carries the cumulative
    /// timing info — `elapsed_ms`, `connect_ms`, `ttft_ms`). We
    /// do NOT keep the full history; the user only cares about
    /// "what phase is this row in right now, and how long has it
    /// been there". The next click on the row opens the full
    /// request detail (bodies, headers, raw status).
    stagesByRequestId: new Map(),
    /// In-flight placeholder rows for requests that have started
    /// emitting stage events but whose final `RecentUsageRow`
    /// has not yet arrived. The dashboard renders these
    /// *above* the historical table so the operator sees the
    /// request as soon as the pipeline picks it up, not only
    /// after `cost::record` runs. The map is keyed by
    /// `request_id`; when the final row lands, the entry is
    /// removed and the row re-renders as a normal historical
    /// row.
    ///
    /// Bounded by `state.logs.maxRows / 2` so a flood of
    /// requests doesn't OOM the page; older placeholders are
    /// discarded before newer ones.
    inflightByRequestId: new Map(),
    /// Whether the server is currently recording full request/response
    /// bodies and headers in the `usage` table. Toggled from the
    /// Live Logs header; fetched on first render so the button
    /// reflects the real state.
    recording: false,
    /// Loading flag for the recording toggle so the user sees
    /// "Saving…" while the POST is in flight.
    recordingLoading: false,
    // Pagination
    page: 1,
    rowsPerPage: 50,
    maxRows: 500,
    /// When true, every new row snaps the view to page 1 (the
    /// newest end of the DESC-sorted list). When false, the user
    /// keeps their place when new rows arrive — useful when
    /// scrolling back to read older entries.
    followTail: true,
    /// setInterval handle for the 100ms latency ticker that
    /// refreshes the `.log-phase-sub` sublabel of in-flight live-log
    /// rows in place. Lives only while the dashboard is on the
    /// `#/logs` view AND the WS is connected; cleared on view-leave
    /// or WS disconnect. See `tickLogLatency` / `startLogLatencyTicker`
    /// / `stopLogLatencyTicker`.
    latencyTickerHandle: null,
  },
  // Background-refresh / view machinery. `currentView.handler` is the
  // last render fn that wrote to `#main`; `currentView.context` is the
  // route parameter (provider id, combo id) so a re-render can call it
  // back without re-parsing the hash. `bgPollHandle` is the setInterval
  // id; `viewCache` is reserved for the future "stale-while-revalidate"
  // path (we still always re-render today).
  currentView: null,
  currentViewKey: '',
  bgPollHandle: null,
  viewCache: new Map(),
  // Per-provider UI state for the search input + filter tabs. Lives
  // outside the route context so a hashchange away and back keeps
  // the user's filter intact.
  providerDetail: {},
  // Set of model row_ids currently selected via the per-row checkboxes
  // in the provider-detail models table. Cleared on provider change
  // (see `renderProviderDetail`) so a user navigating between two
  // providers never bulk-toggles the wrong list.
  selectedModels: new Set(),
  // Tracks which provider the current `selectedModels` set belongs to,
  // so a re-render of the *same* provider (triggered by a checkbox click,
  // a filter change, or the background poll) keeps the in-progress
  // selection. A real provider switch still wipes the set, because the
  // visible row_ids would otherwise belong to the previous provider.
  selectedModelsProvider: null,
  // In-progress selection for the combo-detail targets table. Cleared when
  // the user navigates to a different combo, kept across re-renders of the
  // same combo (checkbox clicks, bulk delete, background polls).
  selectedTargets: new Set(),
  selectedTargetsCombo: null,
  // Latest `POST /combos/:id/test-all` results by combo id.
  comboTestResults: {},
  // In-progress selection for the model picker (the search modal
  // inside the create/edit-key form). Lives on global state because
  // the picker is a single, shared modal DOM node; closing and
  // re-opening the picker re-seeds it from the hidden input.
  modelPickerSelection: new Set(),
};

// Router: regex pattern -> handler. The first matching pattern wins, and
// the match groups are passed to the handler. Hash-based routes support
// params (e.g. #/providers/<id>, #/combos/<id>) which the flat object
// router we used before couldn't express cleanly.
const routes = [
  { pattern: /^#?\/?providers$/, handler: renderProviders, context: null },
  { pattern: /^#?\/?providers\/([^/]+)$/, handler: renderProviderDetail, context: 'provider' },
  { pattern: /^#?\/?combos$/, handler: renderCombos, context: null },
  { pattern: /^#?\/?combos\/(\d+)$/, handler: renderComboDetail, context: 'combo' },
  { pattern: /^#?\/?keys$/, handler: renderKeys, context: null },
  { pattern: /^#?\/?keys\/(\d+)\/usage$/, handler: renderKeyUsage, context: 'key' },
  { pattern: /^#?\/?analytics$/, handler: renderAnalytics, context: null },
  { pattern: /^#?\/?logs$/, handler: renderLogs, context: null },
  { pattern: /^#?\/?config$/, handler: renderConfig, context: null },
];

function currentRoute() {
  return location.hash || '#/providers';
}

// Resolve the current route against the table, find the matching
// pattern, and invoke its handler. Handlers write directly to
// `#main.innerHTML` — they don't return HTML — so the re-render path
// (`rerenderCurrentView`) is a plain function call. We never block on
// a spinner when we already have a cached snapshot of the view.
function navigate() {
  const route = currentRoute();
  for (const r of routes) {
    const m = route.match(r.pattern);
    if (!m) continue;

    // Highlight the parent section in the sidebar. /providers/<id> and
    // /combos/<id> both map to their parent section so the sidebar
    // keeps its "where am I" affordance when the user drills in.
    const mainRoute = route.replace(/^#?\//, '').split('/')[0];
    document.querySelectorAll('nav a').forEach(a => {
      a.classList.toggle('active', a.dataset.route === mainRoute);
    });

    // Decode the route param. Provider ids are strings (e.g.
    // "openrouter"); combo ids are integers. The `context: 'provider'`
    // / `context: 'combo'` discriminator on the route definition is
    // what makes the cast safe.
    let context = null;
    if (m[1] != null) {
      if (r.context === 'provider') {
        context = decodeURIComponent(m[1]);
      } else if (r.context === 'combo') {
        context = parseInt(m[1], 10);
      }
    }

    state.currentView = { handler: r.handler, context };
    // View-leave hook: if we just navigated *away* from the logs
    // view, stop the latency ticker. `navigate()` is the only
    // entry point that mutates `state.currentViewKey`, so this
    // check reliably fires on every cross-view transition.
    // `startLogLatencyTicker` is called from `renderLogs` (which
    // is the handler for the logs route) so re-entering the view
    // re-mounts the ticker.
    if (state.currentViewKey !== route) {
      const wasLogs = /^\/?logs$/.test(state.currentViewKey.replace(/^#?/, ''));
      const isLogs = r.handler === renderLogs;
      if (wasLogs && !isLogs) {
        stopLogLatencyTicker();
      }
    }
    state.currentViewKey = route;

    // Only show the loading placeholder on the first paint of this
    // route. Background re-renders (via rerenderCurrentView) skip the
    // placeholder so the view never flashes to "Loading...".
    if (!state.viewCache.has(route)) {
      document.getElementById('main').innerHTML = '<div class="loading">Loading...</div>';
    }
    r.handler(context).then(() => {
      state.viewCache.set(route, true);
    }).catch(e => {
      document.getElementById('main').innerHTML = `<div class="error">Error: ${escapeHtml(e.message)}</div>`;
    });
    return;
  }
  // Fallback: send the user to the providers landing page.
  location.hash = '#/providers';
}

// ===== Theme switcher =====
//
// State lives in localStorage so a reload keeps the user's choice. We
// apply the theme *before* `load` fires by reading the storage value
// at script-eval time and stamping `data-theme` on `<html>` — that
// way the very first paint already uses the right token set, with no
// dark→light flash for users who picked light last session.
function getStoredTheme() {
  const stored = localStorage.getItem('openproxy-theme');
  return stored === 'light' ? 'light' : 'dark';
}

function applyTheme(theme) {
  // Setting `data-theme` on the document root makes the
  // `:root[data-theme="light"]` block win against the default `:root`
  // block (same specificity, but the attribute selector + the more
  // specific selector order matter here). For dark we explicitly set
  // "dark" so removing the attribute via DevTools doesn't accidentally
  // re-fall-back to the wrong cascade.
  document.documentElement.setAttribute('data-theme', theme);
}

let currentTheme = getStoredTheme();
applyTheme(currentTheme);

function toggleTheme() {
  currentTheme = currentTheme === 'dark' ? 'light' : 'dark';
  localStorage.setItem('openproxy-theme', currentTheme);
  applyTheme(currentTheme);
  const btn = document.getElementById('theme-toggle-btn');
  if (btn) btn.textContent = currentTheme === 'light' ? '☀' : '☾';
}

// Render the theme toggle into the sidebar. Idempotent: if a button
// with the same id already exists we drop it first. We re-run this
// after a navigate() in case the sidebar was rebuilt (it isn't, but
// the safety is cheap).
function renderThemeToggle() {
  const sidebar = document.querySelector('.sidebar');
  if (!sidebar) return;
  let btn = document.getElementById('theme-toggle-btn');
  if (btn) btn.remove();
  btn = document.createElement('button');
  btn.id = 'theme-toggle-btn';
  btn.className = 'theme-toggle';
  btn.type = 'button';
  btn.onclick = toggleTheme;
  btn.title = 'Toggle theme';
  btn.setAttribute('aria-label', 'Toggle color theme');
  btn.textContent = currentTheme === 'light' ? '☀' : '☾';
  sidebar.appendChild(btn);
}

window.addEventListener('hashchange', navigate);
window.addEventListener('pagehide', () => {
  // Defensive cleanup: if the user closes the tab while the
  // ticker is running, the JS context is going away anyway, but
  // calling `clearInterval` here avoids any chance of a stray
  // callback firing during teardown (e.g. on slow browsers where
  // `pagehide` fires before navigation finalises).
  stopLogLatencyTicker();
});
window.addEventListener('load', () => {
  renderThemeToggle();
  navigate();
  checkHealth();
  setInterval(checkHealth, 5000);
  startBackgroundPolling();
});

// ===== API helper =====
async function api(path, opts = {}) {
  const r = await fetch('/web/api' + path, {
    ...opts,
    headers: { 'Content-Type': 'application/json', ...(opts.headers || {}) },
  });
  if (!r.ok) {
    const txt = await r.text();
    throw new Error(`${r.status}: ${txt}`);
  }
  if (r.status === 204) return null;
  return r.json();
}

// ===== Health =====
async function checkHealth() {
  const el = document.getElementById('health-status');
  try {
    const h = await api('/health');
    el.textContent = h.status === 'ok' ? '✓ healthy' : '! degraded';
    el.className = h.status === 'ok' ? 'healthy' : 'degraded';
  } catch (e) {
    el.textContent = '! offline';
    el.className = 'degraded';
  }
}

// ===== Background polling =====
//
// The polling loop's job is to keep `state.{providers,accounts,models}`
// fresh without forcing the user to click "Refresh". When the snapshot
// changes, we re-render the *current* view in place — no spinner, no
// scroll jump, no lost input focus (we only mutate focus when an
// explicit action like `updateProviderFilter` does it on its own).
//
// `JSON.stringify` is a fine change detector here: the data sets are
// small (hundreds of rows at most) and the alternative is a deep diff.
// 3-second cadence keeps the UI feeling live without hammering the
// server; the spec asks for 3-5s and we pick the lower bound.
let bgPollInFlight = false;
function startBackgroundPolling() {
  if (state.bgPollHandle) clearInterval(state.bgPollHandle);
  state.bgPollHandle = setInterval(async () => {
    if (bgPollInFlight) return;  // skip overlap if a previous tick is slow
    bgPollInFlight = true;
    try {
      const [providers, accounts, models, apiKeys, health] = await Promise.all([
        api('/providers').catch(() => null),
        api('/accounts').catch(() => null),
        api('/models').catch(() => null),
        api('/keys').catch(() => null),
        api('/health').catch(() => null),
      ]);
      let changed = false;
      // The stringified compare intentionally treats whitespace /
      // property order as a change. JSON returned by axum with serde
      // is stable enough that this is rare, and when it happens the
      // extra re-render is cheap.
      if (providers && JSON.stringify(providers) !== JSON.stringify(state.providers)) {
        state.providers = providers; changed = true;
      }
      if (accounts && JSON.stringify(accounts) !== JSON.stringify(state.accounts)) {
        state.accounts = accounts; changed = true;
      }
      if (models && JSON.stringify(models) !== JSON.stringify(state.models)) {
        state.models = models; changed = true;
      }
      if (apiKeys && JSON.stringify(apiKeys) !== JSON.stringify(state.apiKeys)) {
        state.apiKeys = apiKeys; changed = true;
      }
      if (health && JSON.stringify(health) !== JSON.stringify(state.health)) {
        state.health = health; changed = true;
      }
      if (changed && state.currentView) {
        rerenderCurrentView();
      }
    } catch (e) {
      // Silent: a single failed poll shouldn't take the dashboard down.
      console.warn('background poll failed', e);
    } finally {
      bgPollInFlight = false;
    }
  }, 3000);
}

function rerenderCurrentView() {
  if (!state.currentView) return;
  const main = document.getElementById('main');
  // Background-poll re-renders replace `#main`'s innerHTML, which
  // would wipe any open modal the user has on screen (create/edit
  // key, custom model, etc.). Pull the modal nodes out of `#main`
  // first and stash them on a body-level container, then re-attach
  // them after the re-render. The container is itself hidden so
  // moving the modals out and back is invisible to the user.
  const openModals = Array.from(main.querySelectorAll('.modal-bg'));
  let modalStash = null;
  if (openModals.length > 0) {
    modalStash = document.createElement('div');
    modalStash.id = '__modal_stash__';
    modalStash.style.display = 'none';
    document.body.appendChild(modalStash);
    openModals.forEach(m => modalStash.appendChild(m));
  }
  // Stash elements marked .persist-on-rerender (OAuth manual/device
  // sections) so they survive the innerHTML wipe triggered by a
  // background poll.
  const persistElements = Array.from(main.querySelectorAll('.persist-on-rerender'));
  const persistStates = persistElements.map(el => ({
    id: el.id,
    display: el.style.display,
    html: el.innerHTML,
  }));
  // No spinner on re-render: if the handler errors, we replace the view
  // with the error message so the user can see what went wrong without
  // a stale page hanging around.
  state.currentView.handler(state.currentView.context).then(() => {
    if (modalStash) {
      // Re-attach the stashed modals on top of the fresh view HTML.
      openModals.forEach(m => main.appendChild(m));
      modalStash.remove();
    }
    // Restore persisted elements with their previous state.
    persistStates.forEach(s => {
      const el = document.getElementById(s.id);
      if (el && s.display !== 'none') {
        el.innerHTML = s.html;
        el.style.display = s.display;
      }
    });
  }).catch(e => {
    main.innerHTML = `<div class="error">Error: ${escapeHtml(e.message)}</div>`;
    if (modalStash) {
      openModals.forEach(m => main.appendChild(m));
      modalStash.remove();
    }
  });
}

// ===== Providers (grid) =====
async function renderProviders() {
  // The grid view reads from `state` directly when it can so the
  // background poll can keep the cards fresh without a full re-fetch
  // here. On a cold first paint (state is empty) we have to fetch
  // before we can render.
  if (state.providers.length === 0) {
    const [providers, accounts, models] = await Promise.all([
      api('/providers'),
      api('/accounts'),
      api('/models'),  // admin endpoint: returns { row_id, active, ... }
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.models = models;
  }

  // Per-provider rollups used by the card grid.
  const stats = {};
  for (const p of state.providers) {
    const providerAccounts = state.accounts.filter(a => a.provider_id === p.id);
    const providerModels = state.models.filter(m => m.provider_id === p.id);
    stats[p.id] = {
      accounts: providerAccounts,
      models: providerModels,
      active_models: providerModels.filter(m => m.active).length,
    };
  }

  let html = `
    <div class="page-header">
      <h2>Providers</h2>
      <div>
        <button onclick="refreshAllProviders()">Refresh all</button>
        <button class="primary" onclick="showCreateProvider()">+ Add provider</button>
      </div>
    </div>
    <div class="provider-grid">
  `;

  if (state.providers.length === 0) {
    html += `
      <div class="empty-state">
        <h3>No providers configured</h3>
        <p>Add a provider to get started.</p>
        <button class="primary" onclick="showCreateProvider()">+ Add provider</button>
      </div>
    `;
  }

  for (const p of state.providers) {
    const s = stats[p.id];
    const unhealthyAccs = s.accounts.filter(a => a.health_status === 'unhealthy').length;
    // Card classes encode the visual state:
    // - `has-errors`: red left stripe when at least one account is unhealthy.
    // - `inactive`:   dimmed card when the provider has been deactivated
    //                 (its name picks up a small "(inactive)" suffix).
    // The two flags are independent — an inactive provider with healthy
    // accounts is just dimmed, while an active provider with unhealthy
    // accounts gets the red stripe.
    const cardClasses = [
      'provider-card',
      unhealthyAccs > 0 ? 'has-errors' : '',
      p.active ? '' : 'inactive',
    ].filter(Boolean).join(' ');
    html += `
      <a href="#/providers/${encodeURIComponent(p.id)}" class="${cardClasses}">
        <div class="provider-card-header">
          <div class="provider-icon" data-format="${escapeAttr(p.format)}">${getProviderIconHtml(p.id, p.format)}</div>
          <div class="provider-info">
            <h3>${escapeHtml(p.name)}${p.active ? '' : ' <small class="inactive-suffix">(inactive)</small>'}</h3>
            <code>${escapeHtml(p.id)}</code>
          </div>
        </div>
        <div class="provider-card-body">
          <div class="capabilities">
            <span class="chip" data-format="${escapeAttr(p.format)}">${escapeHtml(p.format)}</span>
            <span class="chip">${escapeHtml(p.auth_type)}</span>
          </div>
        </div>
        <div class="provider-card-footer">
          <div class="stat">
            <label>Accounts</label>
            <value>${s.accounts.length}</value>
            ${unhealthyAccs > 0 ? `<span class="badge error">${unhealthyAccs} down</span>` : ''}
          </div>
          <div class="stat">
            <label>Models</label>
            <value>${s.active_models}/${s.models.length}</value>
          </div>
        </div>
      </a>
    `;
  }
  html += '</div>';
  document.getElementById('main').innerHTML = html;
}

function getProviderIconHtml(providerId, format) {
  // Three built-in providers get distinct visual markers so the user
  // can scan the grid quickly. Custom providers fall back to the first
  // letter of their id (uppercased), which keeps the icon area from
  // looking broken while still being informative.
  const knownLogos = {
    'openrouter': '🟢',
    'minimax': '🟡',
    'opencode-zen': '🟣',
  };
  const glyph = knownLogos[providerId] || ((providerId[0] || '?').toUpperCase());
  return `<span class="provider-emoji">${glyph}</span>`;
}

window.refreshProvider = async function(providerId, btn) {
  // Same UX as before: disable the button + relabel while in flight.
  // The button param is optional so refreshAllProviders can call us
  // without a DOM element.
  const original = btn ? btn.textContent : 'Refresh models';
  if (btn) {
    btn.disabled = true;
    btn.textContent = 'Refreshing...';
  }
  try {
    const result = await api(
      '/providers/' + encodeURIComponent(providerId) + '/refresh',
      { method: 'POST' },
    );
    const n = (result && typeof result.models_refreshed === 'number')
      ? result.models_refreshed
      : 0;
    const note = result && result.note ? ' (' + result.note + ')' : '';
    alert(`Refreshed ${n} models for ${providerId}${note}.`);
    // Force a refetch instead of relying on the polling interval —
    // the user explicitly asked for fresh data.
    state.providers = await api('/providers');
    state.models = await api('/models');
    rerenderCurrentView();
  } catch (e) {
    alert('Error: ' + e.message);
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = original;
    }
  }
};

window.refreshAllProviders = async function() {
  // Bulk refresh: walk every provider and call its refresh endpoint.
  // Per-provider failures are logged but don't abort the loop — a
  // single misbehaving upstream shouldn't block the rest.
  const providers = await api('/providers');
  for (const p of providers) {
    try {
      await api('/providers/' + encodeURIComponent(p.id) + '/refresh', { method: 'POST' });
    } catch (e) {
      console.error('Failed to refresh', p.id, e);
    }
  }
  state.providers = await api('/providers');
  state.models = await api('/models');
  rerenderCurrentView();
};

window.showCreateProvider = function() {
  const html = `
    <div class="modal-bg" id="create-provider-modal" onclick="if(event.target===this) closeCreateProvider()">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>New provider</h2>
          <button type="button" class="close-btn" onclick="closeCreateProvider()" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="createProvider(event)">
          <div class="modal-body">
            <div class="field">
              <label for="provider-id">ID</label>
              <input id="provider-id" name="id" type="text" required placeholder="openrouter">
            </div>
            <div class="field">
              <label for="provider-name">Name</label>
              <input id="provider-name" name="name" type="text" required placeholder="OpenRouter">
            </div>
            <div class="field">
              <label for="provider-base-url">Base URL</label>
              <input id="provider-base-url" name="base_url" type="text" required placeholder="https://openrouter.ai/api/v1">
            </div>
            <div class="field">
              <label for="provider-auth">Auth</label>
              <select id="provider-auth" name="auth_type">
                <option value="bearer">bearer</option>
                <option value="x-api-key">x-api-key</option>
              </select>
            </div>
            <div class="field">
              <label for="provider-format">Format</label>
              <select id="provider-format" name="format">
                <option value="openai">openai</option>
                <option value="anthropic">anthropic</option>
                <option value="mixed">mixed</option>
              </select>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" onclick="closeCreateProvider()">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
};

window.closeCreateProvider = function() {
  const modal = document.getElementById('create-provider-modal');
  if (modal) modal.remove();
};

window.createProvider = async function(e) {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    await api('/providers', {
      method: 'POST',
      body: JSON.stringify(Object.fromEntries(f)),
    });
    state.providers = await api('/providers');
    navigate();
  } catch (err) {
    alert('Error: ' + err.message);
  }
};

window.deleteProvider = async function(id) {
  // Soft-confirm path: kept for any callers (and the old "Delete
  // provider" button in case it's still wired up somewhere). The
  // dashboard's "Delete" button now uses `confirmDeleteProvider`,
  // which adds a typed-id step to make the destructive cascade harder
  // to trigger by accident.
  if (!confirm(`Delete provider ${id}? This will cascade-delete its accounts and models.`)) return;
  try {
    await api('/providers/' + encodeURIComponent(id), { method: 'DELETE' });
    // The provider is gone; bump the caches so the next render is
    // consistent without waiting for the poll.
    state.providers = state.providers.filter(p => p.id !== id);
    state.models = state.models.filter(m => m.provider_id !== id);
    state.accounts = state.accounts.filter(a => a.provider_id !== id);
    navigate();
  } catch (e) { alert('Error: ' + e.message); }
};

window.toggleProviderActive = async function(providerId, newActive) {
  // Deactivating a provider is the soft, reversible alternative to
  // deleting it: the row stays in the DB (accounts and models
  // preserved), and reactivation brings everything back. The button
  // just flips `active` via the dedicated endpoint.
  //
  // Reactivation skips the confirm — going from "off" to "on" is
  // safe and the user clearly intended it by clicking "Activate".
  if (!newActive) {
    const ok = confirm(
      `Deactivate provider "${providerId}"?\n\n` +
      `Its accounts and models will be preserved, but it won't be ` +
      `usable in combos until you reactivate it.`
    );
    if (!ok) return;
  }
  try {
    await api('/providers/' + encodeURIComponent(providerId) + '/active', {
      method: 'POST',
      body: JSON.stringify({ active: newActive }),
    });
    // Refetch providers so the card / detail reflects the new state.
    state.providers = await api('/providers');
    navigate();
  } catch (e) {
    alert('Error: ' + e.message);
  }
};

window.confirmDeleteProvider = async function(providerId) {
  // Two-step confirmation to make the cascade-delete harder to
  // trigger by accident. The first step asks the user to type the
  // provider id verbatim — a string the user has to look at and
  // re-type is enough friction to catch most misclicks. The second
  // step is a plain "Really?" for the final go-ahead.
  const typed = prompt(`Type the provider ID to confirm deletion: ${providerId}`);
  if (typed !== providerId) {
    if (typed != null) {
      alert(`Provider id "${typed}" does not match. Nothing was deleted.`);
    }
    return;
  }
  if (!confirm(`Really delete ${providerId}? This cascades to all its accounts and models.`)) return;
  try {
    await api('/providers/' + encodeURIComponent(providerId), { method: 'DELETE' });
    // Bump caches: the provider, its accounts, and its models are all gone.
    state.providers = state.providers.filter(p => p.id !== providerId);
    state.models = state.models.filter(m => m.provider_id !== providerId);
    state.accounts = state.accounts.filter(a => a.provider_id !== providerId);
    // The user just deleted the provider they're looking at: send
    // them back to the providers grid.
    location.hash = '#/providers';
  } catch (e) {
    // The server returns `{"error": {"code", "message"}}` for a
    // 4xx. The most common rejection on this path is a built-in
    // (which the UI normally hides via the "🔒 Delete (built-in)"
    // button, but the server is the source of truth and might
    // reject for any other validation reason). Show the message
    // verbatim so the operator sees "cannot be deleted. Use
    // POST .../active to deactivate it" instead of a generic
    // "Error: 400: ...".
    const friendly = extractApiErrorMessage(e) || e.message;
    alert('Cannot delete: ' + friendly);
  }
};

window.renameProviderPrompt = async function(providerId, currentName) {
  // The `name` field is a *display* label — the `id` is the slug
  // used in URLs and FKs, so the rename only touches `name`. PATCH
  // `/v1/admin/providers/:id` already exists in the backend, this is
  // just the UX.
  const newName = prompt(`Rename provider "${providerId}":`, currentName);
  if (newName == null) return; // cancel
  const trimmed = newName.trim();
  if (trimmed === '') {
    alert('Name cannot be empty');
    return;
  }
  if (trimmed === currentName) return; // no-op

  // Names are not unique in the schema (only ids are), so a name
  // collision is allowed — we just warn so the operator can notice.
  const collision = state.providers.find(
    p => p.id !== providerId && p.name === trimmed,
  );
  if (collision) {
    const ok = confirm(
      `A provider with this name already exists (${collision.id}). ` +
      `Use this name anyway?`
    );
    if (!ok) return;
  }

  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'PATCH',
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = await api('/providers');
    navigate();
  } catch (e) {
    alert('Error: ' + e.message);
  }
};

// ===== Provider detail (Connections + Models) =====
//
// The detail view is the most feature-dense screen in the dashboard,
// so it has its own UI state object (search box, filter tab) stored on
// `state.providerDetail[providerId]`. That state is *not* part of the
// route key: navigating away and back keeps the user's filter intact,
// which matches user expectation for "I drilled in, I drilled out, I
// drilled in again — show me what I had selected before."
async function renderProviderDetail(providerId) {
  // Switching providers always starts with an empty selection — the
  // visible row_ids live in the previous provider's table, and a
  // bulk-action on those would silently hit the wrong models.
  // But re-renders triggered by the user interacting with checkboxes,
  // the filter, the search, or the background poll must NOT clear
  // the in-progress selection.
  if (state.selectedModelsProvider !== providerId) {
    state.selectedModels.clear();
    state.selectedModelsProvider = providerId;
  }
  // On a cold paint we need to fetch; on a background re-render the
  // poll has already populated `state`, so we skip the network round
  // trip and re-render straight from the cache.
  if (state.providers.length === 0) {
    const [providers, accounts, models] = await Promise.all([
      api('/providers'),
      api('/accounts'),
      api('/models'),
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.models = models;
  }
  const provider = state.providers.find(p => p.id === providerId);
  if (!provider) {
    document.getElementById('main').innerHTML = `<div class="error">Provider "${escapeHtml(providerId)}" not found. <a href="#/providers">← Back</a></div>`;
    return;
  }
  const accounts = state.accounts.filter(a => a.provider_id === providerId);
  const providerModels = state.models.filter(m => m.provider_id === providerId);
  const activeModels = providerModels.filter(m => m.active).length;

  // Per-provider UI state. Default to "all" / empty search on first
  // visit; keep the user's previous selection on subsequent visits.
  if (!state.providerDetail[providerId]) {
    state.providerDetail[providerId] = { filter: 'all', search: '' };
  }
  const ui = state.providerDetail[providerId];
  const searchLower = ui.search.toLowerCase();
  const filtered = providerModels.filter(m => {
    if (ui.filter === 'active' && !m.active) return false;
    if (ui.filter === 'inactive' && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });

  let html = `
    <div class="page-header">
      <a href="#/providers" class="back-link">← All providers</a>
    </div>
    <div class="provider-detail-header${provider.active ? '' : ' inactive'}">
      <div class="provider-icon icon-large" data-format="${escapeAttr(provider.format)}">${getProviderIconHtml(provider.id, provider.format)}</div>
      <div>
        <h2 class="editable" onclick="renameProviderPrompt('${escapeAttr(provider.id)}', '${escapeAttr(provider.name)}')" title="Click to rename">${escapeHtml(provider.name)} <small>✎</small></h2>
        <code>${escapeHtml(provider.id)}</code>
        <div class="meta">
          <span class="chip" data-format="${escapeAttr(provider.format)}">${escapeHtml(provider.format)}</span>
          <span class="chip">${escapeHtml(provider.auth_type)}</span>
          <a href="${escapeAttr(provider.base_url)}" target="_blank" rel="noopener" class="meta-link">${escapeHtml(provider.base_url)}</a>
          ${provider.active ? '' : '<span class="chip inactive-chip">inactive</span>'}
        </div>
      </div>
      <div class="actions">
        <button onclick="refreshProvider('${escapeAttr(provider.id)}')">↻ Refresh models</button>
        <button class="primary" onclick="toggleProviderActive('${escapeAttr(provider.id)}', ${!provider.active})">
          ${provider.active ? 'Deactivate' : 'Activate'}
        </button>
        ${BUILTIN_PROVIDER_IDS.includes(provider.id)
          ? '<button class="locked" disabled title="Built-in providers cannot be deleted. Deactivate them instead.">🔒 Delete (built-in)</button>'
          : `<button class="danger small" onclick="confirmDeleteProvider('${escapeAttr(provider.id)}')">Delete</button>`}
      </div>
    </div>

    ${OAUTH_PROVIDER_IDS.includes(provider.id) ? `
    <div class="detail-section oauth-login-section">
      <h3>OAuth Login</h3>
      <div class="oauth-buttons">
        ${OAUTH_PKCE_PROVIDERS.includes(provider.id) ? `<button class="btn primary" onclick="OAuthLogin.startPKCE('${escapeAttr(provider.id)}')">Login with ${escapeHtml(provider.name || provider.id)}</button>` : ''}
        ${OAUTH_DEVICE_CODE_PROVIDERS.includes(provider.id) ? `<button class="btn primary" onclick="OAuthLogin.startDeviceCode('${escapeAttr(provider.id)}')">Login with ${escapeHtml(provider.name || provider.id)}</button>` : ''}
      </div>
      <div id="oauth-device-info" class="persist-on-rerender" style="display:none;"></div>
      <div id="oauth-manual-section" class="persist-on-rerender" style="display:none;">
        <div class="oauth-manual-card">
          <h4>OAuth Login — Manual Mode</h4>

          <div id="oauth-manual-step1">
            <p>1. Open this URL in your browser and authenticate:</p>
            <div class="oauth-manual-url">
              <input type="text" id="oauth-auth-url" readonly class="mono">
              <button onclick="navigator.clipboard.writeText(document.getElementById('oauth-auth-url').value); showToast('Copied!', 'success')">📋 Copy</button>
            </div>
          </div>

          <div id="oauth-manual-step2" style="display:none;">
            <p>2. After authentication, paste the callback URL here:</p>
            <div class="oauth-manual-input">
              <input type="text" id="oauth-callback-input"
                     placeholder="http://your-server:8788/callback.html?code=...">
              <button onclick="OAuthLogin.submitManualCallback()" class="btn-primary">Connect</button>
            </div>
            <p class="hint">Paste the full URL from your browser's address bar</p>
          </div>

          <button onclick="document.getElementById('oauth-manual-section').style.display='none'" class="btn-secondary">
            Cancel
          </button>
        </div>
      </div>
    </div>
    ` : ''}
    <section class="detail-section">
      <div class="section-header">
        <h3>Connections (${accounts.length})</h3>
        <div>
          ${providerHasQuota(provider.id) ? `<button onclick="refreshAllQuotas('${escapeAttr(provider.id)}')">↻ Refresh all quotas</button>` : ''}
          <button class="primary" onclick="showCreateAccount('${escapeAttr(provider.id)}')">+ Add account</button>
        </div>
      </div>
      <table>
        <thead><tr><th>Label</th><th>Priority</th><th>Health</th><th>Quota</th><th>Created</th><th>Actions</th></tr></thead>
        <tbody>
  `;
  if (accounts.length === 0) {
    html += `<tr><td colspan="6" class="empty-row">No accounts. Add an API key to start using this provider.</td></tr>`;
  } else {
    for (const a of accounts) {
      // Per-row quota cell: providers without a fetcher show a muted
      // "not supported" hint instead of an empty cell, so the operator
      // knows the column is intentionally blank rather than missing
      // data. The fetch button follows the same gate.
      const quotaCell = providerHasQuota(provider.id)
        ? renderQuotaCell(a)
        : '<div class="quota-cell muted"><small>not supported by this provider</small></div>';
      html += `
        <tr>
          <td>${escapeHtml(a.label || '—')}</td>
          <td>${a.priority}</td>
          <td>
            <select onchange="setHealth(${a.id}, this.value)" class="health-select ${escapeAttr(a.health_status)}">
              <option value="healthy" ${a.health_status === 'healthy' ? 'selected' : ''}>healthy</option>
              <option value="degraded" ${a.health_status === 'degraded' ? 'selected' : ''}>degraded</option>
              <option value="unhealthy" ${a.health_status === 'unhealthy' ? 'selected' : ''}>unhealthy</option>
            </select>
          </td>
          <td>${quotaCell}</td>
          <td>${escapeHtml(a.created_at || '—')}</td>
          <td>
            ${providerHasQuota(provider.id) ? `<button class="small" onclick="refreshAccountQuota(${a.id})">↻ Quota</button>` : ''}
            <button class="small danger" onclick="deleteAccount(${a.id})">Delete</button>
          </td>
        </tr>
      `;
    }
  }
  html += `</tbody></table></section>`;

  // Models section. The "active/total" header gives the user a quick
  // health read; bulk actions are useful when a provider turns on a
  // fleet of new models and the user wants to flip them all at once.
  html += `
    <section class="detail-section">
      <div class="section-header">
        <h3>Models (${activeModels}/${providerModels.length} active)</h3>
        <div>
          <button onclick="bulkToggleModels('${escapeAttr(provider.id)}', true)">Enable all</button>
          <button onclick="bulkToggleModels('${escapeAttr(provider.id)}', false)">Disable all</button>
          <button class="primary" onclick="showCustomModelForm('${escapeAttr(provider.id)}')">+ Custom model</button>
        </div>
      </div>

      <div class="auto-activate-bar">
        <label>
          Auto-activate on refresh:
          <input type="text"
                 id="auto-activate-input-${escapeAttr(provider.id)}"
                 placeholder="(empty = enable all)"
                 value="${escapeAttr(provider.auto_activate_keyword || '')}"
                 onblur="updateAutoActivate('${escapeAttr(provider.id)}', this.value)">
        </label>
        <small>Models whose ID contains this string are auto-enabled on refresh. Empty = enable all new models.</small>
      </div>

      <div class="filter-bar">
        <input type="text" id="search-input-${escapeAttr(provider.id)}" placeholder="Search models..." value="${escapeAttr(ui.search)}"
               oninput="updateProviderFilter('${escapeAttr(provider.id)}', 'search', this.value)">
        <div class="filter-tabs">
          <button id="filter-tab-${escapeAttr(provider.id)}-all" class="filter-tab ${ui.filter === 'all' ? 'active' : ''}" onclick="updateProviderFilter('${escapeAttr(provider.id)}', 'filter', 'all')">All (${providerModels.length})</button>
          <button id="filter-tab-${escapeAttr(provider.id)}-active" class="filter-tab ${ui.filter === 'active' ? 'active' : ''}" onclick="updateProviderFilter('${escapeAttr(provider.id)}', 'filter', 'active')">Active (${activeModels})</button>
          <button id="filter-tab-${escapeAttr(provider.id)}-inactive" class="filter-tab ${ui.filter === 'inactive' ? 'active' : ''}" onclick="updateProviderFilter('${escapeAttr(provider.id)}', 'filter', 'inactive')">Inactive (${providerModels.length - activeModels})</button>
        </div>
      </div>

      ${state.selectedModels.size > 0 ? `
      <div class="bulk-actions-bar">
        <span><strong>${state.selectedModels.size}</strong> selected</span>
        <button onclick="bulkEnableSelected('${escapeAttr(provider.id)}')">Enable selected</button>
        <button onclick="bulkDisableSelected('${escapeAttr(provider.id)}')">Disable selected</button>
        <button onclick="bulkTestSelected('${escapeAttr(provider.id)}')">Test selected</button>
        <button class="danger" onclick="bulkDeleteSelected('${escapeAttr(provider.id)}')">Delete selected</button>
        <button class="link" onclick="clearModelSelection()">Clear selection</button>
      </div>
      ` : ''}

      <table>
        <thead><tr><th><input type="checkbox" id="model-select-all" onchange="toggleSelectAllModels(this.checked)"></th><th>Model ID</th><th>Display</th><th>Format</th><th>Context</th><th>Out</th><th>Capabilities</th><th>Status</th><th>Last test</th><th>Actions</th></tr></thead>
        <tbody id="models-tbody">
  `;
  // After the table is in the DOM, sync the master "select all"
  // checkbox state with reality. We can't rely on the static
  // `checked` attribute because (a) the master checkbox's
  // onchange re-renders the page and drops its `checked` state, and
  // (b) we want an indeterminate visual when only some visible rows
  // are selected. The DOM lookup runs after the innerHTML write
  // below, in a `queueMicrotask`.
  queueMicrotask(() => {
    const master = document.getElementById('model-select-all');
    if (!master) return;
    const visible = getVisibleModelRowIds();
    if (visible.length === 0) {
      master.checked = false;
      master.indeterminate = false;
      return;
    }
    const selectedVisible = visible.filter(id => state.selectedModels.has(id)).length;
    if (selectedVisible === 0) {
      master.checked = false;
      master.indeterminate = false;
    } else if (selectedVisible === visible.length) {
      master.checked = true;
      master.indeterminate = false;
    } else {
      master.checked = false;
      master.indeterminate = true;
    }
  });

  if (filtered.length === 0) {
    html += `<tr><td colspan="9" class="empty-row">No models match the filter.</td></tr>`;
  } else {
    html += renderModelRows(filtered);
  }
  html += `</tbody></table></section>`;

  document.getElementById('main').innerHTML = html;
}

// Map an HTTP status code to a status-pill color. `0` is the sentinel
// the server stamps when the request never reached the upstream
// (DNS / connect / TLS / timeout); treat it as the red "off" pill so
// it reads as a network error at a glance. Skip rows are handled at
// the call site (the result carries a sibling `skipped` boolean
// that the renderer can switch on directly — `status=0` is also
// emitted for skips so the pill alone can't distinguish them).
function statusPillClass(status) {
  if (status === 0) return 'off';
  if (status >= 200 && status < 300) return 'on';
  if (status >= 400 && status < 500) return 'warn';
  if (status >= 500) return 'off';
  return '';
}

// Format a token count for compact display. `null`/`undefined` render
// as an em-dash so the column stays the same width across rows.
// Anything above 1k uses `k`; above 1M uses `M` with one decimal.
function formatContext(tokens) {
  if (tokens == null) return '<span class="muted">—</span>';
  if (tokens >= 1000000) return (tokens / 1000000).toFixed(1) + 'M';
  if (tokens >= 1000) return Math.round(tokens / 1000) + 'k';
  return String(tokens);
}

// Render the per-model capability badges (vision/tools/reasoning/…).
// Accepts either a JSON string (the wire shape from `/v1/admin/models`)
// or a plain object (in case a caller pre-parsed it). Bad input renders
// as an em-dash rather than throwing — the admin list should never
// blow up because of a single bad row.
function renderCapabilityBadges(json) {
  if (json == null) return '<span class="muted">—</span>';
  let caps;
  if (typeof json === 'string') {
    try { caps = JSON.parse(json); } catch (e) { return '<span class="muted">—</span>'; }
  } else {
    caps = json;
  }
  const badges = [];
  if (caps.vision) badges.push('<span class="cap-badge">vision</span>');
  if (caps.tool_calling) badges.push('<span class="cap-badge">tools</span>');
  if (caps.reasoning) badges.push('<span class="cap-badge">reasoning</span>');
  if (caps.thinking) badges.push('<span class="cap-badge">thinking</span>');
  if (caps.structured_output) badges.push('<span class="cap-badge">json</span>');
  if (caps.attachment) badges.push('<span class="cap-badge">attach</span>');
  return badges.length > 0 ? badges.join(' ') : '<span class="muted">—</span>';
}

window.bulkToggleModels = async function(providerId, active) {
  const models = state.models.filter(m => m.provider_id === providerId);
  const customCount = models.filter(m => m.custom).length;
  const toggleableCount = models.length - customCount;
  const toToggleCount = models.filter(m => !m.custom && m.active !== active).length;

  if (toToggleCount === 0) { alert('Nothing to toggle.'); return; }

  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (!confirm(msg)) return;

  try {
    const result = await api('/models/bulk-toggle', {
      method: 'POST',
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    // Refetch models
    state.models = await api('/models');
    rerenderCurrentView();
  } catch (e) {
    alert('Error: ' + e.message);
  }
};

// ===== Accounts =====
window.showCreateAccount = function(preselectedProvider = null) {
  const providers = state.providers || [];
  // When a provider is pre-selected (e.g. the "Add account" button on
  // the provider detail view), the user is already inside that
  // provider's context — showing a dropdown to pick it is redundant
  // and confusing. Render the ID as plain text and submit it via a
  // hidden field instead. When no provider is pre-selected, fall back
  // to the dropdown so this modal can be reused from other contexts.
  const providerField = preselectedProvider
    ? `<div class="field">
         <label>Provider</label>
         <div class="readonly-field"><code>${escapeHtml(preselectedProvider)}</code></div>
         <input type="hidden" name="provider_id" value="${escapeAttr(preselectedProvider)}">
       </div>`
    : `<div class="field">
         <label for="account-provider">Provider</label>
         <select id="account-provider" name="provider_id" required>
           ${providers.map(p => `<option value="${escapeAttr(p.id)}">${escapeHtml(p.id)}</option>`).join('')}
         </select>
       </div>`;
  const html = `
    <div class="modal-bg" id="create-account-modal" onclick="if(event.target===this) closeCreateAccount()">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>New account</h2>
          <button type="button" class="close-btn" onclick="closeCreateAccount()" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="createAccount(event)">
          <div class="modal-body">
            ${providerField}
            <div class="field">
              <label for="account-api-key">API key</label>
              <input id="account-api-key" name="api_key" type="password" required placeholder="sk-...">
            </div>
            <div class="field">
              <label for="account-label">Label</label>
              <input id="account-label" name="label" type="text" placeholder="primary">
            </div>
            <div class="field">
              <label for="account-priority">Priority</label>
              <input id="account-priority" name="priority" type="number" value="100">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" onclick="closeCreateAccount()">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  // Modal is appended to #main rather than replacing it so the
  // backdrop click handler can be a simple identity check on the
  // target. The underlying page stays in the DOM (inert).
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
};

window.closeCreateAccount = function() {
  const modal = document.getElementById('create-account-modal');
  if (modal) modal.remove();
};

window.createAccount = async function(e) {
  e.preventDefault();
  const f = new FormData(e.target);
  const body = Object.fromEntries(f);
  body.priority = parseInt(body.priority);
  try {
    await api('/accounts', { method: 'POST', body: JSON.stringify(body) });
    // The modal sits on top of the detail view; dropping the modal
    // first prevents a re-render of the parent from clobbering the
    // modal mid-close animation.
    const modal = e.target.closest('.modal-bg');
    if (modal) modal.remove();
    state.accounts = await api('/accounts');
    rerenderCurrentView();
  } catch (err) { alert('Error: ' + err.message); }
};

window.deleteAccount = async function(id) {
  if (!confirm('Delete account ' + id + '?')) return;
  try {
    await api('/accounts/' + id, { method: 'DELETE' });
    state.accounts = state.accounts.filter(a => a.id !== id);
    rerenderCurrentView();
  } catch (e) { alert('Error: ' + e.message); }
};

window.setHealth = async function(id, health) {
  try {
    await api('/accounts/' + id + '/health', { method: 'POST', body: JSON.stringify({ health }) });
    // Update the cached account so the background poll's diff is a
    // no-op and the next render is correct.
    const a = state.accounts.find(x => x.id === id);
    if (a) a.health_status = health;
  } catch (e) { alert('Error: ' + e.message); rerenderCurrentView(); }
};

// ===== Account quota (MiniMax Coding Plan) =====
//
// Each Connections row in the provider-detail view shows a "Quota"
// cell with a small bar chart of the session/weekly usage plus a
// refresh button. The data lives on the `Account` struct (the server
// stamps it via `POST /v1/admin/accounts/:id/refresh-quota`), so
// rendering is just a read of `state.accounts[i].quota_*` — there's no
// per-cell network call. The refresh button is the only place that
// triggers a write back to the server.
function renderQuotaCell(a) {
  // Error path: a previous fetch failed. The message is bounded by
  // the server (it puts the upstream error text in `quota_fetch_error`),
  // but we still escape it before injecting into the DOM.
  if (a.quota_fetch_error) {
    return `<div class="quota-cell error"><small>✗ ${escapeHtml(a.quota_fetch_error)}</small></div>`;
  }
  // No usable data: distinguish "we tried, the upstream said
  // nothing" from "we never tried". The former shows
  // `quota_last_fetched_at`, the latter does not. We treat the
  // quota as "absent" only when BOTH the session and the weekly
  // USED values are missing — an OpenRouter key with no configured
  // limit (limit=null) but a real usage of 0 still has a used
  // counter, so it should fall through to the bar renderer with a
  // "—" limit rather than being hidden behind "no quota data".
  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    if (a.quota_last_fetched_at) {
      return `<div class="quota-cell muted"><small>no quota data</small></div>`;
    }
    return `<div class="quota-cell muted"><small>quota: not fetched</small></div>`;
  }
  // Render the two bars. We render even when only one of the two
  // quotas is present (the server may know session but not weekly,
  // or vice versa) — the missing side is dashed and shows "—".
  const sessionPct = (a.quota_session_limit && a.quota_session_limit > 0 && a.quota_session_used != null)
    ? Math.round(a.quota_session_used / a.quota_session_limit * 100)
    : null;
  const weeklyPct = (a.quota_weekly_limit && a.quota_weekly_limit > 0 && a.quota_weekly_used != null)
    ? Math.round(a.quota_weekly_used / a.quota_weekly_limit * 100)
    : null;
  const sessionColor = sessionPct == null ? 'unknown'
    : sessionPct > 80 ? 'danger'
    : sessionPct > 50 ? 'warn' : 'ok';
  const weeklyColor = weeklyPct == null ? 'unknown'
    : weeklyPct > 80 ? 'danger'
    : weeklyPct > 50 ? 'warn' : 'ok';

  // When the limit is exactly 100 the parser is in percent-fallback
  // mode (MiniMax shipped only the remaining-percent field). The bar
  // math is identical, but the label should make it clear we're
  // showing an estimate rather than a raw "X / N" call count.
  const isPct = (used, limit) => limit === 100 && used != null;
  const sessionText = a.quota_session_used == null ? '—'
    : isPct(a.quota_session_used, a.quota_session_limit)
      ? `${a.quota_session_used}% used`
      : `${a.quota_session_used} / ${a.quota_session_limit ?? '—'}`;
  const weeklyText = a.quota_weekly_used == null ? '—'
    : isPct(a.quota_weekly_used, a.quota_weekly_limit)
      ? `${a.quota_weekly_used}% used`
      : `${a.quota_weekly_used} / ${a.quota_weekly_limit ?? '—'}`;

  return `
    <div class="quota-cell">
      ${a.quota_plan_name ? `<small class="quota-plan">${escapeHtml(a.quota_plan_name)}</small>` : ''}
      <div class="quota-bar ${sessionColor}">
        <div class="quota-bar-fill" style="width: ${sessionPct == null ? 0 : Math.min(100, sessionPct)}%"></div>
        <span>session: ${sessionText}</span>
      </div>
      <div class="quota-bar ${weeklyColor}">
        <div class="quota-bar-fill" style="width: ${weeklyPct == null ? 0 : Math.min(100, weeklyPct)}%"></div>
        <span>weekly: ${weeklyText}</span>
      </div>
    </div>
  `;
}

window.refreshAccountQuota = async function(accountId) {
  // The button param is implicit through the event target — we
  // capture it via `event.target`. (Not all callers are DOM handlers;
  // refreshAllQuotas calls the API directly without re-rendering
  // mid-loop.)
  const btn = window.event && window.event.target ? window.event.target : null;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = '...';
  }
  try {
    const result = await api(`/accounts/${accountId}/refresh-quota`, { method: 'POST' });
    if (result.supported === false) {
      if (btn) flashButton(btn, 'n/a', '#9399b2');
    } else if (result.error) {
      if (btn) flashButton(btn, '✗ err', '#f38ba8');
    } else {
      if (btn) flashButton(btn, '✓', '#a6e3a1');
    }
    // Refetch accounts to update the rendered table.
    const accounts = await api('/accounts');
    state.accounts = accounts;
    rerenderCurrentView();
  } catch (e) {
    if (btn) flashButton(btn, '✗', '#f38ba8');
    setTimeout(() => alert('Error: ' + e.message), 100);
  } finally {
    if (btn) {
      setTimeout(() => { btn.disabled = false; btn.textContent = oldText; }, 1500);
    }
  }
};

window.refreshAllQuotas = async function(providerId) {
  const accounts = (state.accounts || []).filter(a => a.provider_id === providerId);
  // Only the providers listed in `quota_capable_providers` on the
  // server have a fetcher; we mirror that list client-side so the
  // confirmation dialog only appears when there's actually something
  // to refresh.
  const supported = accounts.filter(a => QUOTA_CAPABLE_PROVIDERS.includes(a.provider_id));
  if (supported.length === 0) {
    alert('No accounts with quota support (only ' + QUOTA_CAPABLE_PROVIDERS.join(', ') + ').');
    return;
  }
  if (!confirm(`Refresh quota for ${supported.length} accounts?`)) return;
  for (const a of supported) {
    try {
      await api(`/accounts/${a.id}/refresh-quota`, { method: 'POST' });
    } catch (e) {
      console.error('Failed to refresh quota for', a.id, e);
    }
  }
  // Refetch and re-render so the new quota columns show up.
  state.accounts = await api('/accounts');
  rerenderCurrentView();
  alert('Quotas refreshed.');
};

// ===== Models (toggle / test / delete / custom) =====
window.toggleModel = async function(rowId, newActive) {
  // The toggle endpoint takes the row's numeric primary key (not the
  // upstream model id) and a body of `{"active": bool}`. The caller
  // passes the *desired* new state; we forward it verbatim and update
  // the cache so the next background poll is a no-op.
  try {
    await api('/models/' + rowId + '/toggle', {
      method: 'POST',
      body: JSON.stringify({ active: !!newActive }),
    });
    const m = state.models.find(x => x.row_id === rowId);
    if (m) m.active = !!newActive;
    rerenderCurrentView();
  } catch (e) {
    alert('Error: ' + e.message);
  }
};

// Fire a single test request against the upstream for one model. We
// only re-render the affected row's "last test" cell — there's no need
// to redraw the whole table for a 50ms latency stamp. The button
// itself gets a coloured flash so the click feels acknowledged even
// when the request takes a few seconds.
window.testModel = async function(rowId, modelId) {
  const btn = document.getElementById(`test-btn-${rowId}`);
  if (!btn) return;
  const oldText = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Testing...';
  try {
    const result = await api(`/models/${rowId}/test`, { method: 'POST' });
    // Update only the "last test" cell so we don't lose the user's
    // scroll / focus on a 200-row table. The row id is set in the
    // server response; fall back to the request rowId if the server
    // omits it (older builds).
    const rid = result.row_id ?? rowId;
    const row = document.getElementById(`model-row-${rid}`);
    if (row) {
      // Column 5 in the table is "Last test" (the leading checkbox
      // column shifted every other index by +1). Using children[] is
      // brittle to column reorders, but it's also free of book-keeping
      // (no per-cell id); the table shape is owned by this file.
      const cell = row.children[5];
      if (cell) {
        cell.innerHTML = `<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`;
      }
    }
    if (result.status >= 200 && result.status < 300) {
      flashButton(btn, '✓', '#a6e3a1');
    } else if (result.status === 0) {
      flashButton(btn, '✗ net', '#f38ba8');
    } else {
      flashButton(btn, '✗ ' + result.status, '#f38ba8');
    }
  } catch (e) {
    flashButton(btn, '✗', '#f38ba8');
    setTimeout(() => alert('Test failed: ' + e.message), 100);
  } finally {
    setTimeout(() => {
      btn.disabled = false;
      btn.textContent = oldText;
    }, 1500);
  }
};

// Briefly paint the button a colour to confirm a click landed. 1.5s
// is enough for the user to see the result before the label reverts.
function flashButton(btn, text, color) {
  btn.textContent = text;
  btn.style.background = color;
  setTimeout(() => { btn.style.background = ''; }, 1500);
}

// Update the per-provider search/filter state and re-render only the
// affected parts (the model tbody + the filter-tab counts). A full
// re-render of `renderProviderDetail` would replace the search input
// itself and steal focus mid-keystroke, so we keep the surrounding
// DOM stable and patch the tbody in place. The search input keeps
// focus because we never remove it from the document.
window.updateProviderFilter = function(providerId, key, value) {
  if (!state.providerDetail[providerId]) {
    state.providerDetail[providerId] = { filter: 'all', search: '' };
  }
  state.providerDetail[providerId][key] = value;
  const ui = state.providerDetail[providerId];

  // Recompute the visible models from the same rules used by
  // `renderProviderDetail`. Keeping the logic in one place would
  // require a `filterModels(providerId)` helper, but it's three
  // conditions and the duplication is clearer than the indirection.
  const searchLower = (ui.search || '').toLowerCase();
  const allProviderModels = state.models.filter(m => m.provider_id === providerId);
  const filtered = allProviderModels.filter(m => {
    if (ui.filter === 'active' && !m.active) return false;
    if (ui.filter === 'inactive' && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });

  // Re-paint the tbody (and its empty-state row) without touching the
  // surrounding page chrome. The search input lives outside the
  // tbody, so its focus survives.
  const tbody = document.getElementById('models-tbody');
  if (tbody) {
    tbody.innerHTML = filtered.length === 0
      ? `<tr><td colspan="9" class="empty-row">No models match the filter.</td></tr>`
      : renderModelRows(filtered);
  }

  // Refresh the (All / Active / Inactive) counts on the filter tabs.
  // The numbers don't change as the user types, but keeping them in
  // sync via a single updater means we don't have to remember to also
  // update them when the data shape evolves.
  updateFilterTabCounts(providerId, allProviderModels);

  // The master "select all" checkbox state depends on which rows are
  // currently visible (see the note in `renderProviderDetail`). The
  // full re-render ran this in a `queueMicrotask`; we run it now
  // because the microtask queue won't be flushed on a partial paint.
  const master = document.getElementById('model-select-all');
  if (master) {
    const visible = filtered.map(m => m.row_id);
    if (visible.length === 0) {
      master.checked = false;
      master.indeterminate = false;
    } else {
      const selectedVisible = visible.filter(id => state.selectedModels.has(id)).length;
      if (selectedVisible === 0) {
        master.checked = false;
        master.indeterminate = false;
      } else if (selectedVisible === visible.length) {
        master.checked = true;
        master.indeterminate = false;
      } else {
        master.checked = false;
        master.indeterminate = true;
      }
    }
  }
};

// Render the inner HTML of the models table tbody for a list of
// already-filtered models. Pulled out of `renderProviderDetail` so
// `updateProviderFilter` can re-paint just the rows without rebuilding
// the whole page (and dropping the search input's focus).
function renderModelRows(rows) {
  let html = '';
  for (const m of rows) {
    const lastTest = m.last_test_status != null
      ? `<span class="status-pill ${statusPillClass(m.last_test_status)}">${m.last_test_status}</span> <small>${escapeHtml(m.last_test_at || '')}</small>`
      : '<span class="muted">never</span>';
    const isSelected = state.selectedModels.has(m.row_id);
    html += `
      <tr id="model-row-${m.row_id}" class="${m.active ? '' : 'inactive'} ${isSelected ? 'selected' : ''}">
        <td><input type="checkbox" ${isSelected ? 'checked' : ''} onchange="toggleModelSelection(${m.row_id}, this.checked)"></td>
        <td><code>${escapeHtml(m.model_id)}</code>${m.custom ? '<span class="badge custom">custom</span>' : ''}</td>
        <td>${escapeHtml(m.display_name || '—')}</td>
        <td>${escapeHtml(m.target_format || '—')}</td>
        <td>${formatContext(m.context_length)}</td>
        <td>${formatContext(m.max_output_tokens)}</td>
        <td>${renderCapabilityBadges(m.capabilities_json)}${m.family ? ` <small class="muted">${escapeHtml(m.family)}</small>` : ''}</td>
        <td><span class="status-pill ${m.active ? 'on' : 'off'}">${m.active ? 'active' : 'inactive'}</span></td>
        <td>${lastTest}</td>
        <td>
          <button class="small" id="test-btn-${m.row_id}" onclick="testModel(${m.row_id}, '${escapeAttr(m.model_id)}')">Test</button>
          <button class="small" onclick="toggleModel(${m.row_id}, ${!m.active})">${m.active ? 'Disable' : 'Enable'}</button>
          <button class="small danger" onclick="deleteModel(${m.row_id})">×</button>
        </td>
      </tr>
    `;
  }
  return html;
}

// Rewrite the (All / Active / Inactive) counts on the filter tabs so
// the user sees the totals for the provider, not for the current
// filter. Cheaper than a full re-render of `renderProviderDetail`.
function updateFilterTabCounts(providerId, allProviderModels) {
  const active = allProviderModels.filter(m => m.active).length;
  const inactive = allProviderModels.length - active;
  const allBtn = document.getElementById(`filter-tab-${providerId}-all`);
  const activeBtn = document.getElementById(`filter-tab-${providerId}-active`);
  const inactiveBtn = document.getElementById(`filter-tab-${providerId}-inactive`);
  if (allBtn) allBtn.textContent = `All (${allProviderModels.length})`;
  if (activeBtn) activeBtn.textContent = `Active (${active})`;
  if (inactiveBtn) inactiveBtn.textContent = `Inactive (${inactive})`;
}

// Persist the provider's auto-activate keyword. We don't debounce:
// the user types and tabs out (or clicks away), and `onblur` fires
// once. The endpoint takes a three-state `null` / string — we send
// `null` for an empty input to clear the column back to NULL so a
// future refresh re-enables *all* non-custom models.
window.updateAutoActivate = async function(providerId, value) {
  const body = { auto_activate_keyword: value && value.trim() ? value.trim() : null };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    // Refresh the providers cache so the next background-poll diff
    // is a no-op and the input value (in case the server normalized
    // the string) reflects the truth.
    state.providers = await api('/providers');
  } catch (e) {
    alert('Error: ' + e.message);
    rerenderCurrentView();
  }
};

// Open the "add a custom model" modal. Defaults the format selector
// to whatever the provider already speaks (Anthropic for Anthropic
// providers, OpenAI for everything else) so the user only has to
// override it when the model speaks a different protocol.
window.showCustomModelForm = function(providerId) {
  const provider = state.providers.find(p => p.id === providerId);
  const defaultFormat = provider && provider.format === 'anthropic' ? 'anthropic' : 'openai';
  const html = `
    <div class="modal-bg" id="custom-model-modal" onclick="if(event.target===this) closeCustomModelForm()">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>Custom model for ${escapeHtml(providerId)}</h2>
          <button type="button" class="close-btn" onclick="closeCustomModelForm()" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="createCustomModel(event, '${escapeAttr(providerId)}')">
          <div class="modal-body">
            <div class="field">
              <label for="custom-model-id">Model ID</label>
              <input id="custom-model-id" name="model_id" type="text" required placeholder="my-custom-model">
            </div>
            <div class="field">
              <label for="custom-model-display">Display name</label>
              <input id="custom-model-display" name="display_name" type="text" placeholder="My custom model">
            </div>
            <div class="field">
              <label for="custom-model-format">Target format</label>
              <select id="custom-model-format" name="target_format">
                <option value="openai" ${defaultFormat === 'openai' ? 'selected' : ''}>openai</option>
                <option value="anthropic" ${defaultFormat === 'anthropic' ? 'selected' : ''}>anthropic</option>
              </select>
            </div>
            <div class="field">
              <label for="custom-model-ttl">TTL (seconds, 0 = never expires)</label>
              <input id="custom-model-ttl" name="ttl_seconds" type="number" value="0">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" onclick="closeCustomModelForm()">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
};

window.closeCustomModelForm = function() {
  const modal = document.getElementById('custom-model-modal');
  if (modal) modal.remove();
};

window.createCustomModel = async function(e, providerId) {
  e.preventDefault();
  const f = new FormData(e.target);
  const body = {
    provider_id: providerId,
    model_id: f.get('model_id'),
    display_name: f.get('display_name') || null,
    target_format: f.get('target_format'),
    ttl_seconds: parseInt(f.get('ttl_seconds')) || 0,
  };
  try {
    await api('/models/custom', { method: 'POST', body: JSON.stringify(body) });
    e.target.closest('.modal-bg').remove();
    state.models = await api('/models');
    rerenderCurrentView();
  } catch (err) {
    alert('Error: ' + err.message);
  }
};

window.deleteModel = async function(rowId) {
  if (!confirm('Delete this model? Combo targets referencing it will be removed too.')) return;
  try {
    await api(`/models/${rowId}`, { method: 'DELETE' });
    state.models = state.models.filter(m => m.row_id !== rowId);
    rerenderCurrentView();
  } catch (e) { alert('Error: ' + e.message); }
};

// ===== Multi-select on the provider-detail models table =====
//
// The selection is a Set of model row_ids (the numeric primary key the
// server uses for /models/:id endpoints). It is cleared at the top of
// `renderProviderDetail` so a navigation between providers never leaks
// selections across providers. The bulk-actions bar and the per-row
// `tr.selected` class both re-derive from the Set on every render, so
// the only mutation points are these four functions.

window.toggleModelSelection = function(rowId, checked) {
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  rerenderCurrentView();
};

window.toggleSelectAllModels = function(checked) {
  // Only toggle the rows currently passing the active/inactive filter
  // + search box, not every model of the provider. This is what the
  // "select all" affordance promises: a 200-row provider where 3 rows
  // match the user's search shouldn't surprise them by selecting 197
  // extra rows.
  const visible = getVisibleModelRowIds();
  if (checked) {
    for (const id of visible) state.selectedModels.add(id);
  } else {
    for (const id of visible) state.selectedModels.delete(id);
  }
  rerenderCurrentView();
};

window.clearModelSelection = function() {
  state.selectedModels.clear();
  rerenderCurrentView();
};

// Helper: read the per-provider filter+search state and return the
// row_ids of the models that would currently be rendered in the
// models table. Used by `toggleSelectAllModels` so the "select all"
// checkbox only catches the visible rows.
function getVisibleModelRowIds() {
  if (!state.currentView || state.currentView.context === null) return [];
  const providerId = state.currentView.context;
  const ui = state.providerDetail[providerId];
  if (!ui) return [];
  const searchLower = (ui.search || '').toLowerCase();
  return state.models
    .filter(m => m.provider_id === providerId)
    .filter(m => {
      if (ui.filter === 'active' && !m.active) return false;
      if (ui.filter === 'inactive' && m.active) return false;
      if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
      return true;
    })
    .map(m => m.row_id);
}

window.bulkEnableSelected = function(providerId) {
  return bulkSetSelected(providerId, true);
};

window.bulkDisableSelected = function(providerId) {
  return bulkSetSelected(providerId, false);
};

// Bulk enable/disable by calling the existing single-row toggle in
// parallel. Each toggle is its own atomic UPDATE on the server; the
// previous bulk-toggle endpoint applied to *all* non-custom rows of
// the provider, which is exactly the over-broad behavior the per-row
// selection is meant to escape. A refresh is fired at the end so the
// cache matches what the server now has.
async function bulkSetSelected(providerId, active) {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`${active ? 'Enable' : 'Disable'} ${ids.length} models?`)) return;
  await Promise.all(ids.map(rowId =>
    api('/models/' + rowId + '/toggle', {
      method: 'POST',
      body: JSON.stringify({ active }),
    }).catch(e => console.error('Failed toggle', rowId, e))
  ));
  state.models = await api('/models');
  state.selectedModels.clear();
  rerenderCurrentView();
}

window.bulkTestSelected = async function(providerId) {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`Test ${ids.length} models sequentially?`)) return;
  for (const rowId of ids) {
    try {
      const btn = document.getElementById(`test-btn-${rowId}`);
      if (btn) {
        btn.disabled = true;
        btn.textContent = 'Testing...';
      }
      const result = await api(`/models/${rowId}/test`, { method: 'POST' });
      // Patch only the "last test" cell of the affected row (col index
      // 5 in the new 7-column table: checkbox, Model ID, Display,
      // Format, Status, Last test, Actions).
      const row = document.getElementById(`model-row-${rowId}`);
      if (row) {
        const cell = row.children[5];
        if (cell) {
          cell.innerHTML = `<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`;
        }
      }
      if (btn) {
        if (result.status >= 200 && result.status < 300) {
          btn.textContent = '✓';
          btn.style.background = '#a6e3a1';
        } else {
          btn.textContent = '✗ ' + result.status;
          btn.style.background = '#f38ba8';
        }
        setTimeout(() => {
          btn.textContent = 'Test';
          btn.style.background = '';
          btn.disabled = false;
        }, 1500);
      }
    } catch (e) {
      console.error('Test failed', rowId, e);
    }
  }
  // Refresh the models cache so the background poll is a no-op and
  // the next render shows the up-to-date last_test_* columns.
  state.models = await api('/models');
};

window.bulkDeleteSelected = async function(providerId) {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!confirm(`Delete ${ids.length} models? This cannot be undone.`)) return;
  await Promise.all(ids.map(rowId =>
    api('/models/' + rowId, { method: 'DELETE' })
      .catch(e => console.error('Failed delete', rowId, e))
  ));
  state.models = await api('/models');
  state.selectedModels.clear();
  rerenderCurrentView();
};

// ===== Combos (grid) =====
async function renderCombos() {
  const combos = await api('/combos');
  state.combos = combos;

  // Fetch each combo's targets in parallel so the card can show
  // per-target chips. Failed fetches degrade gracefully to an empty
  // target list (we still render the card).
  const targetsByCombo = {};
  await Promise.all(combos.map(async c => {
    try {
      targetsByCombo[c.id] = await api('/combos/' + c.id + '/targets');
    } catch (e) {
      targetsByCombo[c.id] = [];
    }
  }));

  let html = `
    <div class="page-header">
      <h2>Combos</h2>
      <button class="primary" onclick="showCreateCombo()">+ Add combo</button>
    </div>
  `;
  if (combos.length === 0) {
    html += `<p class="empty">No combos. <button class="primary" onclick="showCreateCombo()">+ Add combo</button></p>`;
  } else {
    html += `<div class="combo-grid">`;
    for (const c of combos) {
      const targets = targetsByCombo[c.id] || [];
      const sorted = [...targets].sort((a, b) => a.priority_order - b.priority_order);
      const visible = sorted.slice(0, 4);
      const remaining = sorted.length - visible.length;
      html += `
        <a href="#/combos/${c.id}" class="combo-card">
          <div class="combo-card-header">
            <h3>${escapeHtml(c.name)}</h3>
            <span class="chip">${escapeHtml(c.strategy)}</span>
            <span class="chip">race=${c.race_size}</span>
          </div>
          <div class="combo-card-body">
            <div class="target-chips">
              ${visible.map(t => `<span class="target-chip">${escapeHtml(t.provider_id)}</span>`).join('')}
              ${remaining > 0 ? `<span class="target-chip">+${remaining} more</span>` : ''}
            </div>
            <small>${sorted.length} target${sorted.length !== 1 ? 's' : ''}</small>
          </div>
        </a>
      `;
    }
    html += `</div>`;
  }
  document.getElementById('main').innerHTML = html;
}

async function renderComboDetail(comboId) {
  if (state.selectedTargetsCombo !== comboId) {
    state.selectedTargets.clear();
    state.selectedTargetsCombo = comboId;
  }
  // Both calls are independent; running them in parallel halves the
  // perceived latency on slow networks.
  const [combo, targets] = await Promise.all([
    api('/combos/' + comboId).catch(() => null),
    api('/combos/' + comboId + '/targets'),
  ]);
  if (!combo) {
    document.getElementById('main').innerHTML = `<div class="error">Combo ${comboId} not found. <a href="#/combos">← Back</a></div>`;
    return;
  }
  let html = `
    <div class="page-header">
      <a href="#/combos" class="back-link">← All combos</a>
    </div>
    <div class="combo-detail-header">
      <h2>${escapeHtml(combo.name)}</h2>
      <div class="meta">
        <span class="chip">${escapeHtml(combo.strategy)}</span>
        <label>Race size: <input type="number" min="1" max="8" value="${combo.race_size}" onchange="updateRaceSize(${comboId}, this.value)" class="race-input"></label>
        <button onclick="testAllTargets(${comboId})">🧪 Test all</button>
        <button class="danger" onclick="deleteCombo(${comboId})">Delete</button>
      </div>
    </div>
    ${state.comboTestResults[comboId] ? renderComboTestResults(state.comboTestResults[comboId]) : ''}
    <section class="detail-section">
      <div class="section-header">
        <h3>Targets (${targets.length})</h3>
        <button class="primary" onclick="showAddTarget(${comboId})">+ Add target</button>
      </div>
      ${(() => {
        // Inline summary when at least one target is parked in
        // cooldown. Renders just below the section header so the
        // operator gets a quick "X of Y targets are cooling down"
        // glance. The per-row badge (in the table below) carries
        // the per-target reason.
        const cooldowns = targets.filter(t => t.in_cooldown);
        if (cooldowns.length === 0) return '';
        return `<div class="cooldown-banner">⏸ ${cooldowns.length} of ${targets.length} target(s) in cooldown — engine will skip them for now.</div>`;
      })()}
      ${state.selectedTargets.size > 0 ? `
      <div class="bulk-actions-bar">
        <span><strong>${state.selectedTargets.size}</strong> selected</span>
        <button class="danger" onclick="bulkDeleteSelectedTargets(${comboId})">Delete selected</button>
        <button class="link" onclick="clearTargetSelection()">Clear selection</button>
      </div>
      ` : ''}
  `;
  if (targets.length === 0) {
    html += `<p class="empty">No targets. Add a target to start routing.</p>`;
  } else {
      html += `<table>
      <thead><tr><th><input type="checkbox" id="target-select-all" onchange="toggleSelectAllTargets(this.checked)"></th><th>#</th><th>Provider</th><th>Account</th><th>Model</th><th>Actions</th></tr></thead>
      <tbody id="targets-tbody">`;
    for (const t of [...targets].sort((a, b) => a.priority_order - b.priority_order)) {
      // Sub-combo targets are rendered with a "→ combo: <name>" chip
      // in the Model column. The provider column still shows the
      // virtual "combo" id so the row looks consistent with a flat
      // target; the chip is what conveys the indirection.
      const isSubCombo = t.sub_combo_id != null;
      // Cooldown badge: the engine parks a target in `target_cooldowns`
      // for `cooldown_secs` after a retryable failure (5xx, 429,
      // timeout, connection error). The /targets response now includes
      // `in_cooldown` / `cooldown_until` / `cooldown_reason`; we render
      // a small inline badge so the operator can spot parked rows
      // without opening the test-all panel. The next background poll
      // re-fetches /targets and the badge disappears automatically
      // once the cooldown expires (or the operator hits
      // "Reset cooldown").
      let cooldownBadge = '';
      if (t.in_cooldown) {
        const until = t.cooldown_until ? ` until ${escapeHtml(t.cooldown_until)}` : '';
        const reason = t.cooldown_reason ? ` — ${escapeHtml(t.cooldown_reason)}` : '';
        const title = `Cooldown${reason}${until}`;
        cooldownBadge = ` <span class="badge badge-cooldown" title="${escapeHtml(title)}">⏸ cooldown</span>`;
      }
      // The reset button only makes sense for *flat* (non-sub-combo)
      // rows — sub-combo cooldowns are an internal mechanism we don't
      // expose here.
      const resetCooldownBtn = (t.in_cooldown && !isSubCombo)
        ? `<button class="small" title="Force-clear the cooldown for this target" onclick="resetCooldown(${comboId}, ${t.id})">🔄</button>`
        : '';
      const modelCell = isSubCombo
        ? `<span class="chip combo-chip">→ combo: ${escapeHtml(t.sub_combo_name || ('#' + t.sub_combo_id))}</span>`
        : escapeHtml(t.model_display_name || t.model_id || `row #${t.model_row_id}`) + cooldownBadge;
      const providerCell = isSubCombo
        ? `<span class="virtual-provider">${escapeHtml(t.provider_id)}</span>`
        : `<a href="#/providers/${encodeURIComponent(t.provider_id)}">${escapeHtml(t.provider_id)}</a>`;
      const accountCell = isSubCombo
        ? '<em>n/a</em>'
        : (t.account_id ? '#' + t.account_id : '<em>rotate</em>');
      const isSelected = state.selectedTargets.has(t.id);
      html += `
        <tr class="${isSelected ? 'selected' : ''}">
          <td><input type="checkbox" ${isSelected ? 'checked' : ''} data-target-id="${t.id}" onchange="toggleTargetSelection(${t.id}, this.checked)"></td>
          <td>${t.priority_order}</td>
          <td>${providerCell}</td>
          <td>${accountCell}</td>
          <td>${modelCell}</td>
          <td>
            <button class="small" onclick="changePriority(${comboId}, ${t.id}, -1)">↑</button>
            <button class="small" onclick="changePriority(${comboId}, ${t.id}, 1)">↓</button>
            ${resetCooldownBtn}
            <button class="small danger" onclick="deleteTarget(${comboId}, ${t.id})">×</button>
          </td>
        </tr>
      `;
    }
    html += `</tbody></table>`;
  }
  html += `</section>`;
  document.getElementById('main').innerHTML = html;
  // After the table is in the DOM, sync the master "select all"
  // checkbox state with the in-flight selection. The combo-detail
  // table has no search / filter, so "visible" = "all loaded targets";
  // we still compute it from state so the checkbox stays in sync
  // across background-poll re-renders.
  queueMicrotask(() => {
    const master = document.getElementById('target-select-all');
    if (!master) return;
    const visibleIds = targets.map(t => t.id);
    if (visibleIds.length === 0) {
      master.checked = false;
      master.indeterminate = false;
      return;
    }
    const selectedVisible = visibleIds.filter(id => state.selectedTargets.has(id)).length;
    if (selectedVisible === 0) {
      master.checked = false;
      master.indeterminate = false;
    } else if (selectedVisible === visibleIds.length) {
      master.checked = true;
      master.indeterminate = false;
    } else {
      master.checked = false;
      master.indeterminate = true;
    }
  });
}

window.showCreateCombo = function() {
  const html = `
    <div class="modal-bg" id="create-combo-modal" onclick="if(event.target===this) closeCreateCombo()">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>New combo</h2>
          <button type="button" class="close-btn" onclick="closeCreateCombo()" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="createCombo(event)">
          <div class="modal-body">
            <div class="field">
              <label for="combo-name">Name</label>
              <input id="combo-name" name="name" type="text" required>
            </div>
            <div class="field">
              <label for="combo-strategy">Strategy</label>
              <select id="combo-strategy" name="strategy">
                <option value="priority">priority</option>
                <option value="round_robin">round_robin</option>
                <option value="shuffle">shuffle</option>
              </select>
            </div>
            <div class="field">
              <label for="combo-race-size">Race size</label>
              <input id="combo-race-size" name="race_size" type="number" min="1" max="8" value="1">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" onclick="closeCreateCombo()">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
};

window.closeCreateCombo = function() {
  const modal = document.getElementById('create-combo-modal');
  if (modal) modal.remove();
};

window.createCombo = async function(e) {
  e.preventDefault();
  const f = new FormData(e.target);
  const body = Object.fromEntries(f);
  body.race_size = parseInt(body.race_size);
  try {
    await api('/combos', { method: 'POST', body: JSON.stringify(body) });
    navigate();
  } catch (err) { alert('Error: ' + err.message); }
};

window.testAllTargets = async function(comboId) {
  // Test-all fires a single request to the server, which returns a
  // list of per-target results (see `test_combo_targets` in
  // admin.rs). The handler returns a `skipped` result per flat
  // target and a `skipped` result per sub-combo target, so the
  // dashboard always sees *something* on screen after the click.
  // The button itself gets a short "Testing..." label so the click
  // feels acknowledged even on a 15s timeout.
  const btn = window.event && window.event.target ? window.event.target : null;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = '🧪 Testing...';
  }
  try {
    const results = await api('/combos/' + comboId + '/test-all', { method: 'POST' });
    state.comboTestResults[comboId] = Array.isArray(results) ? results : [];
    rerenderCurrentView();
  } catch (e) {
    alert('Test all failed: ' + (e.message || e));
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = oldText || '🧪 Test all';
    }
  }
};

// Render the per-target results table that the `test-all` endpoint
// returns. Lives just below the combo detail header so the user can
// scan the per-row status without scrolling back to the targets list.
// `results` is the JSON array returned by `POST /v1/admin/combos/:id/test-all`.
//
// Each result row carries:
//   - `status`: numeric HTTP status (0 for "never reached upstream").
//   - `elapsed_ms`: how long the upstream call took. Skips are 0.
//   - `error_msg`: short error description for `status != 2xx`.
//   - `skipped` (bool) / `skip_reason` (string): the row was not
//     actually hit (sub-combo, in cooldown, model inactive, …).
//     `status` is `0` in that case and `statusPillClass` would
//     paint it red; we want the warning pill instead.
function renderComboTestResults(results) {
  if (!Array.isArray(results) || results.length === 0) {
    return `<div class="detail-section"><h3>Test all — results</h3><p class="empty">No targets to test.</p></div>`;
  }
  const rows = results.map(r => {
    const isSubCombo = r.sub_combo_id != null;
    const targetLabel = isSubCombo
      ? `<span class="chip combo-chip">→ combo: ${escapeHtml(r.sub_combo_name || ('#' + r.sub_combo_id))}</span>`
      : escapeHtml(r.model_display_name || r.model_id || `row #${r.model_row_id}`);
    const providerLabel = r.provider_id ? escapeHtml(r.provider_id) : '—';
    // Skipped rows: paint the pill warning (not red) and show
    // the explicit `skip_reason` in the detail column. The
    // `skipped` field is a sibling of `status`; we don't have
    // to overload `status=0` to mean "skipped".
    const statusClass = r.skipped ? 'warn' : statusPillClass(r.status);
    const statusText = r.skipped ? 'skipped' : String(r.status);
    const detail = r.skipped
      ? (r.skip_reason || r.error_msg || 'skipped')
      : (r.error_msg || '');
    const detailHtml = detail ? `<small>${escapeHtml(detail)}</small>` : '';
    const elapsed = (r.elapsed_ms != null && r.elapsed_ms > 0)
      ? `${r.elapsed_ms} ms`
      : '—';
    return `
      <tr>
        <td>#${r.target_id}</td>
        <td>${providerLabel}</td>
        <td>${targetLabel}</td>
        <td><span class="status-pill ${statusClass}">${statusText}</span></td>
        <td>${elapsed}</td>
        <td>${detailHtml}</td>
      </tr>
    `;
  }).join('');
  return `
    <div class="detail-section">
      <h3>Test all — results (${results.length})</h3>
      <table>
        <thead><tr><th>Target</th><th>Provider</th><th>Model / Sub-combo</th><th>Status</th><th>Latency</th><th>Detail</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
  `;
}

window.toggleTargetSelection = function(targetId, checked) {
  if (checked) state.selectedTargets.add(targetId);
  else state.selectedTargets.delete(targetId);
  rerenderCurrentView();
};

window.toggleSelectAllTargets = function(checked) {
  if (!state.currentView || state.currentView.context == null) return;
  // The combo-detail table has no search / filter, so "visible" ==
  // "every target currently rendered". We grab the rendered rows
  // from the DOM so we don't have to keep a duplicate copy in state.
  const visible = Array.from(document.querySelectorAll('#targets-tbody input[type="checkbox"]'))
    .map(cb => parseInt(cb.getAttribute('data-target-id'), 10))
    .filter(id => !Number.isNaN(id));
  if (checked) {
    for (const id of visible) state.selectedTargets.add(id);
  } else {
    for (const id of visible) state.selectedTargets.delete(id);
  }
  rerenderCurrentView();
};

window.clearTargetSelection = function() {
  state.selectedTargets.clear();
  rerenderCurrentView();
};

window.bulkDeleteSelectedTargets = async function(comboId) {
  const ids = Array.from(state.selectedTargets);
  if (ids.length === 0) return;
  if (!confirm(`Delete ${ids.length} targets? This cannot be undone.`)) return;
  // Fire every DELETE in parallel. Per-target failures are logged
  // but don't abort the loop — a single bad row shouldn't block the
  // rest, matching the providers' bulk-delete UX.
  await Promise.all(ids.map(targetId =>
    api('/combos/' + comboId + '/targets/' + targetId, { method: 'DELETE' })
      .catch(e => console.error('Failed delete target', targetId, e))
  ));
  state.selectedTargets.clear();
  rerenderCurrentView();
};

window.deleteCombo = async function(id) {
  if (!confirm('Delete combo ' + id + '?')) return;
  try {
    await api('/combos/' + id, { method: 'DELETE' });
    navigate();
  } catch (e) { alert('Error: ' + e.message); }
};

window.updateRaceSize = async function(id, val) {
  val = parseInt(val);
  if (val < 1 || val > 8) { alert('Must be 1-8'); navigate(); return; }
  try {
    await api('/combos/' + id, { method: 'PATCH', body: JSON.stringify({ race_size: val }) });
  } catch (e) { alert('Error: ' + e.message); navigate(); }
};

window.showAddTarget = async function(comboId) {
  // Seed `state.models` from the cache if the user has been to any
  // view that already fetched it (Providers, Models, etc.). Falling
  // back to a fresh `api('/models')` keeps the modal correct on
  // cold load too.
  if (!state.models || state.models.length === 0) {
    state.models = await api('/models');
  }
  const [providers, accounts, validSubCombos] = await Promise.all([
    api('/providers'),
    api('/accounts'),
    api('/combos/' + comboId + '/targets/valid-sub-combos').catch(() => []),
  ]);
  const modelOpts = state.models.map(m => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    const owner = m.provider_id || m.owned_by || '?';
    if (rowId == null) return '';
    return `<option value="${escapeAttr(String(rowId))}">#${rowId} — ${escapeHtml(upstreamId)} (${escapeHtml(owner)})</option>`;
  }).filter(Boolean).join('');
  // Note the `onchange="onTargetProviderChange()"` on the provider
  // select. The initial population of the model <select> is filtered
  // by the first provider after the modal is mounted (see the
  // `onTargetProviderChange()` call at the bottom of this function),
  // so the user never sees a "Model" dropdown full of rows owned
  // by a different provider.
  //
  // The "Target type" radio at the top toggles between the flat
  // (Model) form and the sub-combo (Combo) form. When the user
  // picks "Combo" the Provider / Account / Model fields are
  // hidden and the sub-combo <select> takes over. The backend
  // enforces the XOR (model_row_id vs sub_combo_id); the form
  // just makes the mutually exclusive shape explicit.
  const subComboOpts = (validSubCombos || [])
    .map(c => `<option value="${c.id}">${escapeHtml(c.name)} (id ${c.id})</option>`)
    .join('');
  const subComboEmpty = subComboOpts
    ? ''
    : '<option disabled>No other combos exist (or every other combo would create a cycle).</option>';
  const html = `
    <div class="modal-bg" id="add-target-modal" onclick="if(event.target===this) closeAddTarget()">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>Add target to combo ${comboId}</h2>
          <button type="button" class="close-btn" onclick="closeAddTarget()" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="addTarget(event, ${comboId})">
          <div class="modal-body">
            <div class="field">
              <label>Target type</label>
              <div class="radio-group">
                <label><input type="radio" name="target_kind" value="model" checked onchange="onTargetKindChange()"> Model</label>
                <label><input type="radio" name="target_kind" value="combo" onchange="onTargetKindChange()"> Sub-combo</label>
              </div>
            </div>
            <div id="model-fields">
              <div class="field">
                <label for="target-provider">Provider</label>
                <select id="target-provider" name="provider_id" onchange="onTargetProviderChange()" required>
                  <option value="">Select provider...</option>
                  ${providers.map(p => `<option value="${escapeAttr(p.id)}">${escapeHtml(p.name || p.id)}</option>`).join('')}
                </select>
              </div>
              <div class="field">
                <label for="target-account">Account (optional, leave blank to rotate)</label>
                <select id="target-account" name="account_id">
                  <option value="">— rotate —</option>
                  ${accounts.map(a => `<option value="${a.id}">${escapeHtml(a.provider_id)}/${escapeHtml(a.label || String(a.id))}</option>`).join('')}
                </select>
              </div>
              <div class="field">
                <label for="target-model">Model</label>
                <select id="target-model" name="model_row_id" required>
                  ${modelOpts || '<option disabled>No models discovered yet — click "Refresh models" on the Providers tab first.</option>'}
                </select>
              </div>
            </div>
            <div id="combo-fields" style="display: none">
              <div class="field">
                <label for="target-sub-combo">Sub-combo</label>
                <select id="target-sub-combo" name="sub_combo_id" disabled>
                  ${subComboOpts || subComboEmpty}
                </select>
                <small>Only combos that won't close a cycle with combo ${comboId} are listed.</small>
              </div>
            </div>
            <div class="field">
              <label for="target-priority">Priority</label>
              <input id="target-priority" name="priority_order" type="number" value="100" required>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" onclick="closeAddTarget()">Cancel</button>
            <button type="submit" class="primary">Add</button>
          </div>
        </form>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
  // Filter the model dropdown to match the first provider so the
  // initial render is consistent with the cascade behavior the
  // user will get when they pick a different provider.
  onTargetProviderChange();
};

// Toggle the Model / Sub-combo blocks in the Add-Target modal.
// Wired via `onchange="onTargetKindChange()"` on the target_kind
// radio; the `disabled` attribute on the unused fields prevents
// browser-native form validation from rejecting a "blank" combo
// select (we only POST the one that is visible).
window.onTargetKindChange = function() {
  const kind = (document.querySelector('input[name="target_kind"]:checked') || {}).value;
  const modelFields = document.getElementById('model-fields');
  const comboFields = document.getElementById('combo-fields');
  const modelSel = document.getElementById('target-model');
  const comboSel = document.getElementById('target-sub-combo');
  if (!modelFields || !comboFields) return;
  if (kind === 'combo') {
    modelFields.style.display = 'none';
    comboFields.style.display = '';
    if (modelSel) modelSel.disabled = true;
    if (comboSel) comboSel.disabled = false;
  } else {
    modelFields.style.display = '';
    comboFields.style.display = 'none';
    if (modelSel) modelSel.disabled = false;
    if (comboSel) comboSel.disabled = true;
  }
};

window.closeAddTarget = function() {
  const modal = document.getElementById('add-target-modal');
  if (modal) modal.remove();
};

// Filter the "Model" <select> in the Add-Target modal to rows that
// belong to the currently selected provider. Wired via
// `onchange="onTargetProviderChange()"` on the provider <select>;
// also called once after the modal is mounted so the initial
// dropdown is consistent with the first provider rather than
// showing every model in the cache.
window.onTargetProviderChange = function() {
  const providerSel = document.getElementById('target-provider');
  const modelSel = document.getElementById('target-model');
  if (!providerSel || !modelSel) return;
  const provider = providerSel.value;
  const filtered = (state.models || []).filter(m => m.provider_id === provider && m.active);
  if (!provider) {
    // No provider picked yet — show the placeholder, disable submit
    // implicitly because the form's `required` on the model <select>
    // is satisfied by the disabled option below.
    modelSel.innerHTML = '<option disabled selected>Select a provider first</option>';
    return;
  }
  const opts = filtered.map(m => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    if (rowId == null) return '';
    return `<option value="${escapeAttr(String(rowId))}">${escapeHtml(upstreamId)}${m.display_name ? ' — ' + escapeHtml(m.display_name) : ''}</option>`;
  }).filter(Boolean).join('');
  modelSel.innerHTML = opts || '<option disabled>No active models for this provider</option>';
};

window.addTarget = async function(e, comboId) {
  e.preventDefault();
  const f = new FormData(e.target);
  const kind = (document.querySelector('input[name="target_kind"]:checked') || {}).value;
  // The two target kinds share the same wire shape on the server
  // side (`AddTargetInput`): exactly one of `model_row_id` /
  // `sub_combo_id` is set. We populate the right one based on the
  // radio and leave the other as `null` (which JSON serialises to
  // `null` and the server's `?` on `Option<i64>` accepts).
  let body;
  if (kind === 'combo') {
    const subComboId = parseInt(f.get('sub_combo_id'));
    if (!subComboId) {
      alert('Select a sub-combo first.');
      return;
    }
    body = {
      provider_id: 'combo',
      account_id: null,
      model_row_id: null,
      sub_combo_id: subComboId,
      priority_order: parseInt(f.get('priority_order')),
    };
  } else {
    body = {
      provider_id: f.get('provider_id'),
      account_id: f.get('account_id') ? parseInt(f.get('account_id')) : null,
      model_row_id: parseInt(f.get('model_row_id')),
      sub_combo_id: null,
      priority_order: parseInt(f.get('priority_order')),
    };
  }
  try {
    await api('/combos/' + comboId + '/targets', { method: 'POST', body: JSON.stringify(body) });
    navigate();
  } catch (err) { alert('Error: ' + err.message); }
};

window.deleteTarget = async function(comboId, targetId) {
  if (!confirm('Delete target ' + targetId + '?')) return;
  try {
    await api('/combos/' + comboId + '/targets/' + targetId, { method: 'DELETE' });
    navigate();
  } catch (e) { alert('Error: ' + e.message); }
};

window.resetCooldown = async function(comboId, targetId) {
  // Force-clear the persistent cooldown row for a single target.
  // The handler is `POST /v1/admin/combos/:id/targets/:target_id/clear-cooldown`
  // (literal segment, registered before the `:target_id` DELETE/PATCH
  // route in `router.rs`). On success we re-render the current view
  // so the badge disappears; on failure we surface the backend
  // message verbatim (typically `CoreError::Validation` for a
  // cross-combo id, or a 404 if the target was deleted out from
  // under us by a parallel operator).
  try {
    await api('/combos/' + comboId + '/targets/' + targetId + '/clear-cooldown', {
      method: 'POST',
    });
    rerenderCurrentView();
  } catch (e) {
    alert('Could not clear cooldown: ' + (e.message || e));
  }
};

window.changePriority = async function(comboId, targetId, delta) {
  // Swap-based reorder: pull the current ordered list, swap the
  // moved target with its neighbor in-place, and POST the full new
  // order to `/targets/reorder`. The backend renumbers every row
  // atomically in a single transaction, so two targets can never
  // briefly share a `priority_order` (the old `old + delta` PATCH
  // would collide whenever a row's `priority_order` matched the
  // computed `new_order`).
  try {
    const targets = await api('/combos/' + comboId + '/targets');
    const sorted = [...targets].sort((a, b) => a.priority_order - b.priority_order);
    const idx = sorted.findIndex(t => t.id === targetId);
    const swapIdx = idx + delta;
    if (swapIdx < 0 || swapIdx >= sorted.length) return;
    // Swap in place
    [sorted[idx], sorted[swapIdx]] = [sorted[swapIdx], sorted[idx]];
    await api('/combos/' + comboId + '/targets/reorder', {
      method: 'POST',
      body: JSON.stringify({ target_ids: sorted.map(t => t.id) }),
    });
    navigate();
  } catch (e) {
    alert('Error reordering: ' + (e.message || e));
  }
};

// ===== Analytics =====
async function renderAnalytics() {
  document.getElementById('main').innerHTML = '<h2>Analytics</h2><div class="loading">Loading...</div>';
  try {
    const [summary, byModel, byAccount, byStatus, latency, races] = await Promise.all([
      api('/usage/summary'),
      api('/usage/by-model'),
      api('/usage/by-account'),
      api('/usage/by-status'),
      api('/usage/latency'),
      api('/usage/races'),
    ]);
    let html = `
      <h2>Analytics</h2>
      <div class="card">
        <h3>Summary</h3>
        <div class="metrics">
          <div><label>Unique requests</label><value>${summary.unique_requests}</value></div>
          <div><label>Total rows</label><value>${summary.total_rows}</value></div>
          <div><label>Winners</label><value>${summary.winners}</value></div>
          <div><label>Losers</label><value>${summary.losers}</value></div>
          <div><label>Errors</label><value>${summary.errors}</value></div>
          <div><label>Total prompt tokens</label><value>${summary.total_prompt_tokens}</value></div>
          <div><label>Total completion tokens</label><value>${summary.total_completion_tokens}</value></div>
          <div><label>Total cost USD</label><value>$${summary.total_cost_usd.toFixed(4)}</value></div>
          <div><label>Avg TTFT ms</label><value>${summary.avg_ttft_ms ? summary.avg_ttft_ms.toFixed(1) : '—'}</value></div>
        </div>
      </div>
      <div class="card">
        <h3>Latency percentiles (winners only)</h3>
        <div class="metrics">
          <div><label>Samples</label><value>${latency.samples}</value></div>
          <div><label>p50 connect ms</label><value>${latency.p50_connect_ms?.toFixed(0) ?? '—'}</value></div>
          <div><label>p95 connect ms</label><value>${latency.p95_connect_ms?.toFixed(0) ?? '—'}</value></div>
          <div><label>p50 TTFT ms</label><value>${latency.p50_ttft_ms?.toFixed(0) ?? '—'}</value></div>
          <div><label>p95 TTFT ms</label><value>${latency.p95_ttft_ms?.toFixed(0) ?? '—'}</value></div>
          <div><label>p50 total ms</label><value>${latency.p50_total_ms?.toFixed(0) ?? '—'}</value></div>
          <div><label>p95 total ms</label><value>${latency.p95_total_ms?.toFixed(0) ?? '—'}</value></div>
        </div>
      </div>
      <div class="card">
        <h3>Race stats</h3>
        <div class="metrics">
          <div><label>Total races</label><value>${races.total_races}</value></div>
          <div><label>Winners</label><value>${races.winners}</value></div>
          <div><label>Losers</label><value>${races.losers}</value></div>
        </div>
      </div>
      <div class="card">
        <h3>By model</h3>
        <table>
          <thead><tr><th>Provider</th><th>Model</th><th>Unique</th><th>Total</th><th>Cost USD</th></tr></thead>
          <tbody>${byModel.map(r => `<tr><td>${escapeHtml(r.provider_id)}</td><td>${escapeHtml(r.upstream_model_id)}</td><td>${r.unique_requests}</td><td>${r.total_rows}</td><td>$${r.total_cost_usd.toFixed(4)}</td></tr>`).join('')}</tbody>
        </table>
      </div>
    `;
    document.getElementById('main').innerHTML = html;
  } catch (e) {
    document.getElementById('main').innerHTML = `<div class="error">${escapeHtml(e.message)}</div>`;
  }
}

// ===== Live Logs =====
const LOGS_WS_RECONNECT_DELAYS = [1000, 2000, 4000, 8000, 16000, 30000];

function logsWsUrl() {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const baseUrl = `${scheme}//${location.host}/web/api/usage/stream`;
  return baseUrl;
}

function setLogsStatus(status) {
  state.logs.status = status;
  const badge = document.getElementById('logs-connection-status');
  if (!badge) return;
  const labels = {
    connected: '🟢 connected',
    connecting: '🟡 connecting',
    reconnecting: '🟡 reconnecting',
    disconnected: '🔴 disconnected',
  };
  badge.className = `logs-connection-badge ${status}`;
  badge.textContent = labels[status] || '🔴 disconnected';
}

function renderLogsRows() {
  const logsEl = document.getElementById('logs');
  if (!logsEl) return;
  // Build a single unified list: historical rows + in-flight
  // placeholders. We sort by `id` DESC, but the in-flight
  // placeholders have *string* ids like `"inflight-<uuid>"`;
  // we use a synthetic numeric id of `Number.MAX_SAFE_INTEGER`
  // minus the request's `created_at` epoch millis so newer
  // in-flight placeholders sit at the top of page 1.
  //
  // `created_at` from the server is RFC-3339 with millisecond
  // precision; `Date.parse(...)` handles it. We don't do
  // timezone math — we just need a monotonic *within this
  // session* ordering, so any monotonicity-preserving transform
  // would do. We pick this one because it survives an
  // `appendChild` of a page-1 row that the operator is staring
  // at (the row keeps its position relative to other in-flight
  // rows while the in-flight set churns).
  const inflightRows = Array.from(state.logs.inflightByRequestId.values()).map(p => {
    const t = Date.parse(p.created_at);
    const syntheticId = isFinite(t) ? (Number.MAX_SAFE_INTEGER - t) : Number.MAX_SAFE_INTEGER;
    return Object.assign({}, p, { id: syntheticId, __inflight: true });
  });
  const rows = state.logs.rows.concat(inflightRows)
    .sort((a, b) => (b.id || 0) - (a.id || 0));
  const totalRows = rows.length;
  const rpp = state.logs.rowsPerPage;
  const totalPages = Math.max(1, Math.ceil(totalRows / rpp));

  // Clamp current page
  if (state.logs.page > totalPages) state.logs.page = totalPages;
  if (state.logs.page < 1) state.logs.page = 1;

  const start = (state.logs.page - 1) * rpp;
  const end = Math.min(start + rpp, totalRows);
  const pageRows = rows.slice(start, end);

  // Column header
  const headerHtml = `
    <div class="log-row" style="cursor:default;border-bottom:1px solid var(--border-strong);font-weight:600;font-size:0.72rem;text-transform:uppercase;color:var(--text-muted);background:#0f0d0b;position:sticky;top:0;z-index:1;">
      <span>Time</span>
      <span>Phase</span>
      <span>Status</span>
      <span>Provider</span>
      <span>Model</span>
      <span>Tokens</span>
      <span>Latency</span>
      <span>Cost</span>
    </div>`;

  const bodyHtml = pageRows.length
    ? pageRows.map(renderLogRowHtml).join('')
    : '<div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>';

  // Pagination controls
  const isFirst = state.logs.page <= 1;
  const isLast = state.logs.page >= totalPages;
  const paginationHtml = totalRows > 0 ? `
    <div class="logs-pagination">
      <span class="rows-info">${totalRows} row${totalRows !== 1 ? 's' : ''}</span>
      <button onclick="logsGoPage(1)" ${isFirst ? 'disabled' : ''}>⟨⟨</button>
      <button onclick="logsPrevPage()" ${isFirst ? 'disabled' : ''}>‹ Prev</button>
      <span class="page-info">Page ${state.logs.page} of ${totalPages}</span>
      <button onclick="logsNextPage()" ${isLast ? 'disabled' : ''}>Next ›</button>
      <button onclick="logsGoPage(${totalPages})" ${isLast ? 'disabled' : ''}>⟩⟩</button>
      <label class="logs-follow-toggle" title="When ON, new rows automatically scroll the view to the most recent page. When OFF, the view stays on the page you're reading.">
        <input type="checkbox" id="logs-follow-input" ${state.logs.followTail ? 'checked' : ''} onchange="logsSetFollow(this.checked)">
        <span>Follow</span>
      </label>
    </div>` : '';

  logsEl.innerHTML = headerHtml + bodyHtml + paginationHtml;
  attachLogRowHandlers();
}

window.logsPrevPage = function() {
  if (state.logs.page > 1) {
    state.logs.page--;
    // Reaching page 1 (the newest end) re-enables follow-tail.
    if (state.logs.page === 1) state.logs.followTail = true;
    renderLogsRows();
  }
};

window.logsNextPage = function() {
  // Rows are sorted DESCENDING by id (newest first), so the FIRST
  // page contains the freshest rows. "Next" therefore moves toward
  // older rows (page N). Auto-scroll/follow-tail stays on page 1
  // by design.
  if (state.logs.page < totalPages()) {
    state.logs.page++;
    // If the user manually walked all the way to the last page,
    // they've explicitly chosen to read the older end — leave
    // followTail off so future broadcasts don't yank them back.
    if (state.logs.page >= totalPages()) state.logs.followTail = false;
    renderLogsRows();
  }
};

window.logsGoPage = function(p) {
  const total = totalPages();
  state.logs.page = Math.max(1, Math.min(p, total));
  // "Following the tail" means sitting on page 1 (newest end).
  state.logs.followTail = (state.logs.page === 1);
  renderLogsRows();
};

function totalPages() {
  return Math.max(1, Math.ceil(state.logs.rows.length / state.logs.rowsPerPage));
}

window.logsSetFollow = function(enabled) {
  state.logs.followTail = !!enabled;
  if (enabled) {
    // Jump to page 1 (the newest end) so the user sees the tail.
    state.logs.page = 1;
    renderLogsRows();
  }
};

function mergeLogsByDescId(existing, incoming) {
  // Build the map FROM existing first, so the merge is independent
  // of any stale `state.logs.rowById` from previous renders. The
  // prior code started with `new Map(state.logs.rowById)` and then
  // re-added every `existing` row on top of it, which meant the
  // request_id alias entries from a previous history batch kept
  // accumulating (Map sizes grew from 50 to 200+ entries even when
  // the visible row count stayed at 1 or 2). That stale alias
  // baggage also confused the dedup filter below.
  const merged = new Map();
  // Bug #1 fix: also key rows by `request_id` so the in-flight
  // "started" event (which arrives with id=0) and the terminal
  // "row" event (which arrives with a real DB id) are merged into
  // one entry instead of two. Without this, the dashboard would
  // show the in-flight row, then duplicate it as a completed row.
  for (const row of existing) {
    const k = Number(row.id) || row.id;
    merged.set(k, row);
    if (row.request_id) merged.set('req:' + row.request_id, row);
  }
  for (const row of incoming) {
    if (row == null || row.id == null) continue;
    const k = Number(row.id) || row.id;
    // Replace the entry keyed by `id`; if the incoming row has no
    // real id yet (in-flight), prefer any existing row with the
    // same request_id as the base for the merge so the user sees
    // continuous updates of the same logical request.
    let base = merged.get(k);
    if ((!k || k === 0) && row.request_id) {
      const reqKey = 'req:' + row.request_id;
      if (merged.has(reqKey)) base = merged.get(reqKey);
    }
    merged.set(k, { ...(base || {}), ...row });
    if (row.request_id) merged.set('req:' + row.request_id, merged.get(k));
    state.logs.lastSeenId = Math.max(state.logs.lastSeenId, row.id);
  }
  // Dedupe correctly: build a Set of seen identity keys in a single
  // pass, then filter. A row can be indexed under both `id` and
  // `req:<request_id>`, so `Array.from(merged.values())` may yield
  // the same row twice. We must keep the first occurrence and drop
  // the rest, *globally* — the previous per-element slice-based
  // filter accidentally dropped the incoming row when the existing
  // map had a request_id-aliased entry of the same id (see bug
  // captured in E2E: row arrived id=141, map had 197 stale
  // request_id aliases, filter saw newRow twice and rejected it).
  const seenKeys = new Set();
  let result = Array.from(merged.values()).filter((r) => {
    const key = r.id != null ? Number(r.id) : (r.request_id ? 'r:' + r.request_id : Symbol());
    if (key === Symbol() || seenKeys.has(key)) return false;
    seenKeys.add(key);
    return true;
  }).sort((a, b) => (b.id || 0) - (a.id || 0));

  // Enforce max row limit – discard oldest (highest id = newest, so remove from end)
  const limit = state.logs.maxRows;
  if (result.length > limit) {
    const removed = result.slice(limit);
    result = result.slice(0, limit);
    for (const r of removed) {
      const k = Number(r.id) || r.id;
      merged.delete(k);
    }
    state.logs.rowById = merged;
  } else {
    state.logs.rowById = merged;
  }
  return result;
}

function renderLogRowHtml(row) {
  const streaming = row.is_streaming && !row.stream_complete;
  const cls = [
    'log-row',
    row.status_code >= 400 || row.status_code === 0 ? 'error' : 'ok',
    row.race_lost ? 'loser' : '',
    streaming ? 'streaming' : '',
  ].filter(Boolean).join(' ');
  // Look up the latest stage event for this request, if any.
  // For rows that arrived via `history` (i.e. the request
  // finished before the dashboard connected) there is no
  // stage — fall back to the historical terminal state.
  const stage = state.logs.stagesByRequestId.get(row.request_id);
  const phaseCell = renderLogPhaseHtml(stage, row);
  return `
    <button class="${cls}" data-id="${escapeAttr(row.id)}" data-request-id="${escapeAttr(row.request_id || '')}" aria-label="Open usage detail for ${escapeAttr(row.request_id || row.id || '')}">
      <span class="log-time">${escapeHtml(row.created_at || '')}</span>
      ${phaseCell}
      <span class="log-status">${row.status_code ?? '—'}</span>
      <span class="log-provider">${escapeHtml(row.provider_id || '')}</span>
      <span class="log-model">${escapeHtml(row.upstream_model_id || '')}</span>
      <span class="log-tokens">${formatContext(row.prompt_tokens)}↓ ${formatContext(row.completion_tokens)}↑</span>
      <span class="log-latency">${row.total_ms || 0}ms</span>
      <span class="log-cost">$${(row.cost_usd || 0).toFixed(4)}</span>
    </button>
  `;
}

/// Live-log latency ticker. While the dashboard is on `#/logs`
/// and the WS is connected, this runs every 100ms and rewrites
/// the `.log-phase-sub` sublabel of any in-flight row in place
/// — without re-rendering the whole row, so the operator sees a
/// smooth millisecond counter instead of a stale number that
/// only updates when the next `stage` event lands.
///
/// The wall-clock is computed from `state.logs.stagesByRequestId`'s
/// `stage.timestamp` field (RFC-3339) so the displayed number
/// matches the server's clock, modulo whatever skew the WS
/// delivery added.
///
/// We intentionally do NOT re-render rows whose stage is
/// `completed` or `failed` (the number is final), nor rows that
/// aren't on the current page (they aren't in the DOM, and the
/// next `renderLogsRows` will repaint them anyway).
function tickLogLatency() {
  const stages = state.logs.stagesByRequestId;
  if (!stages || stages.size === 0) return;
  const now = Date.now();
  // Build a per-request lookup of `data-request-id` -> row element
  // for the currently rendered page only. We restrict to the live
  // DOM (cheap, O(rows_on_page)) instead of scanning the full
  // state; paged-out rows are handled by the next re-render.
  const rowEls = document.querySelectorAll('#logs .log-row[data-request-id]');
  if (rowEls.length === 0) return;
  for (const rowEl of rowEls) {
    const requestId = rowEl.dataset.requestId;
    if (!requestId) continue;
    const stage = stages.get(requestId);
    if (!stage) continue;
    // Terminal stages: the row already shows the final value in
    // its sublabel (and `total_ms` in the latency cell). No-op.
    if (stage.stage === 'completed' || stage.stage === 'failed') continue;
    // Parse the server's RFC-3339 timestamp. If it's missing or
    // unparseable, fall back to the stage's `elapsed_ms` (which
    // the server already stamped for exactly this purpose).
    const t = Date.parse(stage.timestamp);
    let live;
    if (isFinite(t)) {
      live = Math.max(0, now - t);
    } else {
      live = stage.elapsed_ms || 0;
    }
    const sub = rowEl.querySelector('.log-phase-sub');
    if (!sub) continue;
    // Mirror `renderLogPhaseHtml`'s label rules so the visual
    // meaning of the number is consistent between initial render
    // and ticker-driven updates.
    let label;
    if (stage.stage === 'streaming' && stage.ttft_ms != null) {
      label = `ttft ${stage.ttft_ms}ms`;
    } else if ((stage.stage === 'waiting_ttft' || stage.stage === 'streaming') && stage.connect_ms != null) {
      label = `connect ${stage.connect_ms}ms`;
    } else {
      label = `${live}ms`;
    }
    if (sub.textContent !== label) {
      sub.textContent = label;
      // Mark the row as "ticking" so the (optional) CSS dot can
      // pulse. The class is harmless if no `.log-phase-sub--ticking`
      // rule is defined — it's just a hook for future styling.
      sub.classList.add('log-phase-sub--ticking');
    }
    // Bonus: also tick the row's `log-latency` cell (the corner
    // total). Only when the stage is non-terminal — historical
    // rows already show the final `total_ms` and should not be
    // overwritten. We read `row.total_ms` from the row's data
    // attribute fallback (none today) and otherwise just show
    // `live` as a "running total" so the operator sees the
    // cumulative time grow alongside the phase sublabel.
    const latencyEl = rowEl.querySelector('.log-latency');
    if (latencyEl) {
      const newLatency = `${live}ms`;
      if (latencyEl.textContent !== newLatency) {
        latencyEl.textContent = newLatency;
      }
    }
  }
}

/// Start the 100ms latency ticker. Idempotent: if a handle is
/// already registered, this is a no-op so re-entering the view
/// (e.g. via the bgPoll re-render path that calls `renderLogs`
/// with the idempotency guard) doesn't double-up the interval.
function startLogLatencyTicker() {
  if (state.logs.latencyTickerHandle) return;
  state.logs.latencyTickerHandle = setInterval(tickLogLatency, 100);
}

/// Stop the 100ms latency ticker. Idempotent and safe to call
/// from the WS `disconnected` handler, the view-leave path, and
/// page unload.
function stopLogLatencyTicker() {
  if (state.logs.latencyTickerHandle) {
    clearInterval(state.logs.latencyTickerHandle);
    state.logs.latencyTickerHandle = null;
  }
}

/// Render the "phase" cell for a log row. Pulls the latest
/// `StageEvent` for the request if any, else renders an empty
/// placeholder (for historical rows).
///
/// The cell value is the human-readable phase label
/// (e.g. `waiting_ttft`, `streaming`) with a colour-coded
/// badge and a small "Xms" sublabel for the time spent in
/// this phase. The exact `elapsed_ms` is already on the row's
/// `total_ms` after the request finishes, so we show the
/// *current* ms-from-start as the sublabel to give the
/// operator a live feel even mid-request.
function renderLogPhaseHtml(stage, row) {
  if (!stage) {
    return `<span class="log-phase log-phase--idle" title="No live phase info (request finished before live-log opened)">—</span>`;
  }
  const phase = stage.stage || 'started';
  const elapsed = stage.elapsed_ms || 0;
  const label = STAGE_LABELS[phase] || phase;
  const cls = `log-phase log-phase--${phase}`;
  // Sublabel: the *latest* per-stage value the operator cares
  // about. We pick the most relevant of the available timings:
  //   - `connecting` shows `elapsed_ms` (time to first byte)
  //   - `waiting_ttft` shows `connect_ms` (time to first byte)
  //   - `streaming` shows `ttft_ms` (time to first body byte)
  //   - `started` shows `elapsed_ms` (always 0 in practice)
  //   - `failed` shows `elapsed_ms` (total time-to-fail)
  let sublabel;
  if (phase === 'streaming' && stage.ttft_ms != null) {
    sublabel = `ttft ${stage.ttft_ms}ms`;
  } else if ((phase === 'waiting_ttft' || phase === 'streaming') && stage.connect_ms != null) {
    sublabel = `connect ${stage.connect_ms}ms`;
  } else {
    sublabel = `${elapsed}ms`;
  }
  return `<span class="${cls}" title="${escapeAttr(label)} (${escapeAttr(sublabel)})">${escapeHtml(label)}<span class="log-phase-sub">${escapeHtml(sublabel)}</span></span>`;
}

/// Maps the server's coarse stage labels to human-friendly
/// strings. The server publishes
/// `started`/`connecting`/`waiting_ttft`/`streaming`/`failed` —
/// we keep those exact keys in the `data-stage` attribute (and
/// CSS) so the styling can target them directly, but render a
/// friendlier label in the cell body.
const STAGE_LABELS = {
  started: 'procesando payload',
  connecting: 'conectando a upstream',
  waiting_ttft: 'esperando ttft',
  streaming: 'recibiendo streaming',
  completed: 'completado',
  failed: 'falló',
};

/// CSS-escape a string for use inside an attribute selector.
/// Browsers expose `CSS.escape(...)` natively; we use it
/// directly and fall back to a no-op for environments that
/// don't (older test runners). The dashboard's request_ids
/// are random hex, so the risk of selector injection is
/// nil, but a stray `-` or `\` would still break the lookup.
function cssEscape(s) {
  if (typeof CSS !== 'undefined' && CSS.escape) return CSS.escape(s);
  return String(s).replace(/[^\w-]/g, (c) => '\\' + c);
}

function attachLogRowHandlers() {
  document.querySelectorAll('#logs .log-row').forEach(row => {
    row.addEventListener('click', () => openLogDetail(row.dataset.id, row.dataset.requestId));
  });
}

function handleLogsMessage(raw) {
  let msg;
  try {
    msg = JSON.parse(raw.data);
  } catch (_) {
    showToast('Live Logs received an invalid WebSocket message.', 'error');
    return;
  }
  window.__logMsgTrace = window.__logMsgTrace || [];
  window.__logMsgTrace.push({ t: Date.now(), type: msg.type, hasData: !!msg.data, hasRow: !!msg.row, keys: Object.keys(msg || {}).slice(0, 10) });
  if (msg.type === 'history') {
    const rows = Array.isArray(msg.rows) ? msg.rows : [];
    state.logs.rows = mergeLogsByDescId(state.logs.rows, rows);
    // History: go to page 1 by default (most recent data)
    state.logs.page = 1;
    state.logs.followTail = true;
    renderLogsRows();
  } else if (msg.type === 'row') {
    const row = msg.data || msg.row || msg;
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
    // When the final `RecentUsageRow` arrives, drop the
    // in-flight placeholder for the same `request_id` (if any)
    // so the row seamlessly becomes a historical row, not a
    // duplicate.
    if (row.request_id && state.logs.inflightByRequestId.has(row.request_id)) {
      state.logs.inflightByRequestId.delete(row.request_id);
    }
    if (row.is_streaming && !row.stream_complete) {
      state.logs.liveTokens.set(row.request_id, state.logs.liveTokens.get(row.request_id) || '');
    }
    // Bug #2 fix: only snap to page 1 (the newest end) if the user
    // is following the tail. If they scrolled away to read older
    // rows (page > 1 with followTail off), leave them alone.
    if (state.logs.followTail) {
      state.logs.page = 1;
    }
    renderLogsRows();
    updateOpenLogDetail(row);
  } else if (msg.type === 'stage') {
    handleStageEvent(msg.data || msg);
  } else if (msg.type === 'stream_tokens') {
    handleStreamTokens(msg);
  } else if (msg.type === 'error') {
    showToast(msg.message || 'Live Logs WebSocket error', 'error');
  }
}

/// Live-log stage event: an in-flight request transitioned
/// through a phase. We update the row's "phase" cell in place
/// (no row re-render — the operator wants to see the timeline
/// blink, not the row jump around the DOM).
///
/// The match for the row is by `request_id` because stages are
/// broadcast *before* the row's `cost::record` fires (and therefore
/// before the `RecentUsageRow` reaches the client). In that
/// window the request has no `usage_id` to anchor on, so
/// `request_id` is the only stable key.
function handleStageEvent(event) {
  if (!event || !event.request_id) return;
  const requestId = event.request_id;
  // Stash the latest event for this request. The render fn reads
  // this map to produce the "phase" cell value.
  state.logs.stagesByRequestId.set(requestId, event);

  // Three cases for the matching row in `state.logs.rows`:
  //
  //  A) Row exists in memory and is in the rendered page.
  //     → Update the phase cell in place. Cheap, no flicker.
  //
  //  B) Row exists in memory but is paged out (page > 1).
  //     → Force a re-render. The pagination logic will put the
  //       user back on the page they're on; the phase cell is
  //       included in the new HTML.
  //
  //  C) No row yet (this stage is the first signal of the
  //     request). The operator still wants to see *something*
  //     — a row showing only the live phase info, anchored by
  //     `request_id`. We add an "in-flight" row to a separate
  //     list and render that above the historical table.
  //     When the final `RecentUsageRow` lands, we remove the
  //     placeholder and the row seamlessly becomes the
  //     historical one.
  const existingRow = state.logs.rows.find(r => r.request_id === requestId);
  if (existingRow) {
    // Ensure `state.logs.rowById` knows about it too so
    // pagination / detail panel lookups work after a re-render.
    if (existingRow.id != null) {
      state.logs.rowById.set(existingRow.id, existingRow);
    }
    if (state.logs.followTail) {
      state.logs.page = 1;
    }
    renderLogsRows();
    updateOpenLogDetail(existingRow);
    return;
  }

  // Case C: in-flight placeholder. The placeholder is keyed by
  // the negative form of the *latest* stage event's `elapsed_ms`
  // timestamp so the DESC sort puts the most recent in-flight
  // request at the top of page 1. The id is unique per
  // `request_id` because the placeholders are stored in a
  // separate map (see `state.logs.inflightByRequestId`).
  if (!state.logs.inflightByRequestId.has(requestId)) {
    state.logs.inflightByRequestId.set(requestId, {
      id: `inflight-${requestId}`,
      request_id: requestId,
      // Copy the model/provider fields from the first stage
      // event so the row already shows the right combo target.
      provider_id: event.provider_id || '',
      upstream_model_id: event.upstream_model_id || '',
      // Synthetic timestamp from the event so the row's `log-time`
      // cell is meaningful.
      created_at: event.timestamp || new Date().toISOString(),
      // Zero/dummy values for the rest — the row's render fn
      // falls back to the phase cell for the operator's primary
      // information.
      status_code: 0,
      prompt_tokens: null,
      completion_tokens: null,
      total_ms: 0,
      cost_usd: 0,
      is_streaming: false,
      stream_complete: false,
      race_lost: false,
    });
  }
  if (state.logs.followTail) {
    state.logs.page = 1;
  }
  renderLogsRows();
}

function handleStreamTokens(msg) {
  const requestId = msg.request_id;
  if (!requestId) return;
  const prev = state.logs.liveTokens.get(requestId) || '';
  const next = prev + (msg.delta || '');
  state.logs.liveTokens.set(requestId, next);
  if (msg.complete) {
    const row = state.logs.rowById.get(msg.id) || state.logs.rows.find(r => r.request_id === requestId);
    if (row) {
      row.stream_complete = true;
      renderLogsRows();
    }
  }
  const panel = document.querySelector(`[data-token-panel="${CSS.escape(requestId)}"]`);
  const body = document.getElementById('stream-token-body');
  if (panel) panel.textContent = next;
  if (body) {
    body.textContent = next;
    body.scrollTop = body.scrollHeight;
  }
}

function updateOpenLogDetail(row) {
  const selected = state.logs.selectedRow;
  if (!selected || selected.request_id !== row.request_id) return;
  state.logs.selectedRow = { ...(selected || {}), ...row };
  const title = document.getElementById('log-detail-title');
  if (title) title.textContent = `Usage #${row.id || row.request_id}`;
  updateLogDetailSummary();
}

function updateLogDetailSummary() {
  const row = state.logs.selectedRow;
  if (!row) return;
  const summary = document.getElementById('log-detail-summary');
  if (!summary) return;
  summary.innerHTML = `
    <div><strong>Request</strong><code>${escapeHtml(row.request_id || '—')}</code></div>
    <div><strong>Trace</strong><code>${escapeHtml(row.trace_id || '—')}</code></div>
    <div><strong>Provider</strong>${escapeHtml(row.provider_id || '—')}</div>
    <div><strong>Model</strong>${escapeHtml(row.upstream_model_id || '—')}</div>
    <div><strong>Status</strong><span class="status-pill ${statusPillClass(row.status_code)}">${row.status_code ?? '—'}</span></div>
    <div><strong>Total</strong>${row.total_ms || 0}ms</div>
    <div><strong>Tokens</strong>${row.prompt_tokens || 0}↓ ${row.completion_tokens || 0}↑</div>
    <div><strong>Cost</strong>$${(row.cost_usd || 0).toFixed(4)}</div>
  `;
}

async function openLogDetail(id, requestId) {
  let row = state.logs.rowById.get(Number(id)) || state.logs.rows.find(r => r.request_id === requestId);
  if (!row) {
    row = { id, request_id: requestId };
    state.logs.rows = mergeLogsByDescId(state.logs.rows, [row]);
  }
  state.logs.selectedRow = row;
  renderLogDetailModal();

  if (!hasCompleteLogDetail(row)) {
    const detailEl = document.getElementById('log-detail-loading');
    if (detailEl) detailEl.textContent = 'Loading detail…';
    try {
      const detail = await api(`/usage/detail?id=${encodeURIComponent(id)}`);
      const fetched = detail?.row || detail?.detail || detail;
      if (fetched) {
        row = { ...row, ...fetched };
        state.logs.rowById.set(Number(row.id || id), row);
        state.logs.selectedRow = row;
        renderLogDetailModal();
      }
    } catch (e) {
      if (detailEl) detailEl.textContent = `Detail unavailable: ${e.message || e}`;
      showToast(`Request detail unavailable: ${e.message || e}`, 'error');
    }
  }
}

function hasCompleteLogDetail(row) {
  return !!(
    row &&
    (
      row.request_body_json !== undefined ||
      row.response_body_json !== undefined ||
      row.request_headers !== undefined ||
      row.response_headers !== undefined ||
      row.timing_ms !== undefined ||
      row.error_message !== undefined ||
      row.race_total !== undefined ||
      row.race_attempts !== undefined
    )
  );
}

function renderLogDetailModal() {
  const row = state.logs.selectedRow;
  if (!row) return;
  const requestId = row.request_id || '';
  const html = `
    <div class="modal-bg log-detail-modal" onclick="if(event.target===this) closeLogDetailModal()">
      <div class="modal log-detail-modal-card" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2 id="log-detail-title">Usage #${escapeHtml(row.id || requestId)}</h2>
          <button type="button" class="close-btn" onclick="closeLogDetailModal()" aria-label="Close">&times;</button>
        </div>
        <div class="detail-tabs" role="tablist" aria-label="Log detail tabs">
          <button class="detail-tab active" data-tab="request" role="tab">Request</button>
          <button class="detail-tab" data-tab="response" role="tab">Response</button>
          <button class="detail-tab" data-tab="headers" role="tab">Headers</button>
          <button class="detail-tab" data-tab="timing" role="tab">Timing</button>
          <button class="detail-tab" data-tab="race" role="tab">Race</button>
          <button class="detail-tab" data-tab="error" role="tab">Error</button>
        </div>
        <div class="modal-body">
          <section id="log-detail-summary" class="log-detail-summary"></section>
          <section id="log-detail-loading" class="muted"></section>
          <section class="detail-tab-panel active" data-panel="request">
            <div class="detail-panel-header">
              <h3>Request JSON</h3>
              <button type="button" class="copy-btn" data-copy-target="request-json">Copy</button>
            </div>
            <pre class="json-viewer" id="request-json">${escapeHtml(prettyJson(row.request_body_json))}</pre>
          </section>
          <section class="detail-tab-panel" data-panel="response">
            <div class="detail-panel-header">
              <h3>Response JSON</h3>
              <button type="button" class="copy-btn" data-copy-target="response-json">Copy</button>
            </div>
            <pre class="json-viewer" id="response-json">${escapeHtml(prettyJson(row.response_body_json))}</pre>
            ${row.is_streaming ? `<div class="streaming-token"><strong>${row.stream_complete ? 'Stream complete' : 'Streaming'}:</strong><span id="stream-token-body">${escapeHtml(state.logs.liveTokens.get(requestId) || '')}</span></div>` : ''}
          </section>
          <section class="detail-tab-panel" data-panel="headers">
            <div class="detail-panel-header">
              <h3>Headers</h3>
              <button type="button" class="copy-btn" data-copy-target="headers-json">Copy</button>
            </div>
            <pre class="json-viewer" id="headers-json">${escapeHtml(prettyJson({ request: row.request_headers || {}, response: row.response_headers || {} }))}</pre>
          </section>
          <section class="detail-tab-panel" data-panel="timing">
            <div class="detail-panel-header">
              <h3>Timing</h3>
              <button type="button" class="copy-btn" data-copy-target="timing-json">Copy</button>
            </div>
            <pre class="json-viewer" id="timing-json">${escapeHtml(prettyJson(row.timing_ms || { total_ms: row.total_ms, connect_ms: row.connect_ms, ttft_ms: row.ttft_ms }))}</pre>
          </section>
          <section class="detail-tab-panel" data-panel="race">
            <div class="detail-panel-header">
              <h3>Race</h3>
              <button type="button" class="copy-btn" data-copy-target="race-json">Copy</button>
            </div>
            <pre class="json-viewer" id="race-json">${escapeHtml(prettyJson({ total: row.race_total, attempts: row.race_attempts, lost: row.race_lost }))}</pre>
          </section>
          <section class="detail-tab-panel" data-panel="error">
            <div class="detail-panel-header">
              <h3>Error</h3>
              <button type="button" class="copy-btn" data-copy-target="error-json">Copy</button>
            </div>
            <pre class="json-viewer" id="error-json">${escapeHtml(prettyJson(row.error_message || null))}</pre>
          </section>
        </div>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
  updateLogDetailSummary();
  attachDetailTabHandlers();
  attachCopyButtonHandlers();
}

function attachDetailTabHandlers() {
  document.querySelectorAll('.detail-tab').forEach(tab => {
    tab.addEventListener('click', () => {
      const target = tab.dataset.tab;
      tab.closest('.log-detail-modal-card').querySelectorAll('.detail-tab').forEach(t => t.classList.toggle('active', t === tab));
      tab.closest('.log-detail-modal-card').querySelectorAll('.detail-tab-panel').forEach(panel => {
        panel.classList.toggle('active', panel.dataset.panel === target);
      });
    });
  });
}

function attachCopyButtonHandlers() {
  document.querySelectorAll('.copy-btn').forEach(btn => {
    btn.addEventListener('click', async () => {
      const target = document.getElementById(btn.dataset.copyTarget);
      if (!target) return;
      try {
        await navigator.clipboard.writeText(target.textContent || '');
        btn.textContent = 'Copied!';
        setTimeout(() => { btn.textContent = 'Copy'; }, 1500);
      } catch (_) {
        showToast('Copy failed', 'error');
      }
    });
  });
}

function closeLogDetailModal() {
  const modal = document.querySelector('.log-detail-modal');
  if (modal) modal.remove();
  state.logs.selectedRow = null;
}

function connectLogsWebSocket() {
  clearLogsReconnectTimer();

  // Bug #3 fix: singleton guard. If we already have an open or
  // connecting WebSocket, don't create a duplicate. A previous
  // instance may also be in the process of being torn down (its
  // close handshake not yet complete) — let that finish naturally
  // rather than racing a new one.
  if (state.logs.ws) {
    const ready = state.logs.ws.readyState;
    if (ready === WebSocket.OPEN) {
      // Already open. The badge in the DOM may have been reset to
      // "disconnected" by a fresh innerHTML render (e.g. user
      // navigated away to another view and back). Sync the badge to
      // reflect the real socket state.
      setLogsStatus('connected');
      return;
    }
    if (ready === WebSocket.CONNECTING) {
      // Still handshaking; the open handler will update the badge.
      return;
    }
    // CLOSING or CLOSED: fall through and create a fresh socket. The
    // old one's listeners are still attached but they all check
    // `state.logs.ws !== ws` so they're inert.
  }

  setLogsStatus(state.logs.reconnectAttempt === 0 ? 'connecting' : 'reconnecting');
  const ws = new WebSocket(logsWsUrl());
  ws.addEventListener('open', () => {
    if (state.logs.ws !== ws) return;
    state.logs.reconnectAttempt = 0;
    setLogsStatus('connected');
    if (state.logs.lastSeenId > 0) {
      ws.send(JSON.stringify({ type: 'subscribe', since_id: state.logs.lastSeenId }));
    }
  });
  ws.addEventListener('message', event => {
    if (state.logs.ws !== ws) return;
    handleLogsMessage(event);
  });
  ws.addEventListener('close', () => {
    if (state.logs.ws !== ws) return;
    setLogsStatus('disconnected');
    // Stop the latency ticker while we're disconnected: the map
    // keeps growing stale (and the operator's view is no longer
    // updating anyway), so the work would be wasted. Re-enabled
    // on the next successful `open` via `startLogLatencyTicker` —
    // see the `onopen` branch + the `renderLogs` re-entry path.
    stopLogLatencyTicker();
    scheduleLogsReconnect();
  });
  ws.addEventListener('error', () => {
    if (state.logs.ws !== ws) return;
    showToast('Live Logs disconnected. Reconnecting…', 'error');
    ws.close();
  });
  state.logs.ws = ws;
}

function scheduleLogsReconnect() {
  clearLogsReconnectTimer();
  const delay = LOGS_WS_RECONNECT_DELAYS[Math.min(state.logs.reconnectAttempt, LOGS_WS_RECONNECT_DELAYS.length - 1)];
  state.logs.reconnectAttempt += 1;
  state.logs.reconnectTimer = setTimeout(connectLogsWebSocket, delay);
}

function clearLogsReconnectTimer() {
  if (state.logs.reconnectTimer) {
    clearTimeout(state.logs.reconnectTimer);
    state.logs.reconnectTimer = null;
  }
}

async function renderLogs() {
  // Idempotency guard: bgPoll calls rerenderCurrentView() every 3s when
  // /providers, /accounts, /models, /keys, or /health change. If we
  // re-render the logs view on every poll, we wipe the rows in memory,
  // break the WS subscriptions, and thrash the DOM — the WS appears to
  // drop shortly after opening. So: if the logs view is already in the
  // DOM, just make sure the WS is connected and the recording toggle
  // is in sync, then return.
  const main = document.getElementById('main');
  const alreadyRendered = main && main.querySelector('.logs-header') && main.querySelector('#logs');
  if (alreadyRendered) {
    // The WS is the source of truth for rows. We do not re-create it
    // here; the singleton guard inside connectLogsWebSocket will do
    // the right thing (skip if already open/connecting, sync the
    // badge if the DOM was just rebuilt).
    connectLogsWebSocket();
    fetchRecordingState();
    // (Re)start the 100ms latency ticker. The start fn is
    // idempotent so it's safe to call on every re-render of the
    // logs view (bgPoll re-renders, hashchange-into-the-same-route,
    // etc.) without piling up intervals.
    startLogLatencyTicker();
    return;
  }

  clearLogsReconnectTimer();
  state.logs.rows = [];
  state.logs.rowById = new Map();
  state.logs.lastSeenId = 0;
  state.logs.liveTokens = new Map();
  state.logs.reconnectAttempt = 0;
  state.logs.page = 1;
  state.logs.followTail = true;
  document.getElementById('main').innerHTML = `
    <div class="logs-header">
      <h2>Live Logs</h2>
      <div class="logs-header-actions">
        <span id="logs-connection-status" class="logs-connection-badge disconnected">🔴 disconnected</span>
        <button
          id="logs-recording-toggle"
          class="logs-recording-toggle"
          type="button"
          aria-pressed="false"
          title="When ON, the server saves full request/response bodies and headers for every request (disk). When OFF, only metadata is kept."
        >
          <span class="logs-recording-dot" aria-hidden="true"></span>
          <span class="logs-recording-label">⏺ Record: <strong>OFF</strong></span>
        </button>
      </div>
    </div>
    <div class="logs" id="logs">
      <div class="empty" style="padding:2rem;">No recent requests yet. Use the API to see logs appear here in real time.</div>
    </div>
  `;
  // Wire the recording toggle and sync its state with the server.
  const recBtn = document.getElementById('logs-recording-toggle');
  if (recBtn) {
    recBtn.addEventListener('click', () => toggleRecording());
  }
  fetchRecordingState();
  connectLogsWebSocket();
  // Start the 100ms latency ticker. Idempotent — safe to call
  // even if the ticker was somehow left running from a previous
  // view-mount.
  startLogLatencyTicker();
}

/// Fetch the current recording flag from the server and update the
/// toggle button. Safe to call multiple times.
async function fetchRecordingState() {
  try {
    const data = await api('/recording');
    state.logs.recording = !!data.recording;
  } catch (err) {
    console.warn('fetchRecordingState failed', err);
    // Leave state.logs.recording at whatever it was; the button is
    // a soft indicator and the server is the source of truth.
  } finally {
    renderRecordingToggle();
  }
}

/// Flip the recording flag on the server, then re-sync local state.
async function toggleRecording() {
  if (state.logs.recordingLoading) return;
  state.logs.recordingLoading = true;
  renderRecordingToggle();
  const desired = !state.logs.recording;
  try {
    const data = await api('/recording', {
      method: 'POST',
      body: JSON.stringify({ enabled: desired }),
    });
    state.logs.recording = !!data.recording;
  } catch (err) {
    console.error('toggleRecording failed', err);
    alert('Failed to toggle recording: ' + err.message);
  } finally {
    state.logs.recordingLoading = false;
    renderRecordingToggle();
  }
}

/// Re-render the Record button to reflect state.logs.recording.
function renderRecordingToggle() {
  const btn = document.getElementById('logs-recording-toggle');
  if (!btn) return;
  const on = !!state.logs.recording;
  const loading = !!state.logs.recordingLoading;
  btn.classList.toggle('on', on);
  btn.classList.toggle('off', !on);
  btn.classList.toggle('loading', loading);
  btn.setAttribute('aria-pressed', on ? 'true' : 'false');
  btn.disabled = loading;
  const label = btn.querySelector('.logs-recording-label');
  if (label) {
    label.innerHTML = `⏺ Record: <strong>${on ? 'ON' : 'OFF'}</strong>`;
  }
  btn.title = on
    ? 'Recording is ON — full bodies and headers are being saved. Click to stop.'
    : 'Recording is OFF — only metadata is being saved. Click to start recording full bodies and headers.';
}

function prettyJson(value) {
  if (value == null) return 'null';
  if (typeof value === 'string') {
    try { return JSON.stringify(JSON.parse(value), null, 2); }
    catch (_) { return value; }
  }
  return JSON.stringify(value, null, 2);
}

// ===== OAuth Login Flow =====

//
// Two flows are supported:
//   - PKCE (Antigravity, Antigravity CLI): popup-based authorization
//   - Device code (Kiro): user enters a code on a verification URI
//
// The OAuth-capable provider ids are kept in this list so the
// provider-detail view knows when to show the login section.
const OAUTH_PROVIDER_IDS = ['antigravity', 'antigravity-cli', 'kiro'];
const OAUTH_PKCE_PROVIDERS = ['antigravity', 'antigravity-cli'];
const OAUTH_DEVICE_CODE_PROVIDERS = ['kiro'];

const OAuthLogin = {
  async startPKCE(provider) {
    try {
      const resp = await api(`/oauth/${provider}/authorize`);
      if (resp.error) throw new Error(resp.error);

      const isLocal = window.location.hostname === 'localhost' ||
                      window.location.hostname === '127.0.0.1';

      if (isLocal) {
        await this.pkcePopup(provider, resp);
      } else {
        this.showManualPasteForm(provider, resp);
      }
    } catch (err) {
      showToast(`OAuth failed: ${err.message}`, 'error');
    }
  },

  async pkcePopup(provider, authData) {
    // Open popup for OAuth consent
    const popup = window.open(authData.authorization_url, 'oauth popup',
      'width=600,height=700,top=100,left=100');

    // Listen for the code from the popup
    const code = await new Promise((resolve, reject) => {
      const handler = (event) => {
        if (event.origin !== window.location.origin) return;
        if (event.data && event.data.type === 'oauth_code') {
          window.removeEventListener('message', handler);
          popup.close();
          resolve(event.data.code);
        }
      };
      window.addEventListener('message', handler);

      // Timeout after 5 minutes
      setTimeout(() => {
        window.removeEventListener('message', handler);
        reject(new Error('OAuth timeout'));
      }, 300000);
    });

    // Exchange the code — pass redirect_uri so the core server uses
    // the same URI Google expects (exact-match per OAuth spec).
    const exchangeResp = await api(`/oauth/${provider}/exchange`, {
      method: 'POST',
      body: JSON.stringify({
        code,
        redirect_uri: authData.redirect_uri,
        code_verifier: authData.code_verifier,
      }),
    });

    if (exchangeResp.error) throw new Error(exchangeResp.error);

    showToast(`Logged in with ${provider}`, 'success');
    state.accounts = await api('/accounts');
    rerenderCurrentView();
  },

  showManualPasteForm(provider, authData) {
    const section = document.getElementById('oauth-manual-section');
    if (!section) return;
    section.style.display = 'block';

    this._currentAuth = { provider, ...authData };

    const authUrlInput = document.getElementById('oauth-auth-url');
    if (authUrlInput) authUrlInput.value = authData.authorization_url;

    const callbackInput = document.getElementById('oauth-callback-input');
    if (callbackInput) callbackInput.value = '';

    const step1 = document.getElementById('oauth-manual-step1');
    const step2 = document.getElementById('oauth-manual-step2');
    if (step1) step1.style.display = 'block';
    if (step2) step2.style.display = 'none';

    window.open(authData.authorization_url, '_blank');

    setTimeout(() => {
      if (step1) step1.style.display = 'none';
      if (step2) step2.style.display = 'block';
    }, 2000);
  },

  async submitManualCallback() {
    const input = document.getElementById('oauth-callback-input').value.trim();
    const authData = this._currentAuth;

    if (!input) {
      showToast('Please paste the callback URL', 'error');
      return;
    }

    let code = null;
    let callbackState = null;

    try {
      const url = new URL(input);
      code = url.searchParams.get('code');
      callbackState = url.searchParams.get('state') || url.hash.replace(/^#/, '') || null;
    } catch {
      const [rawCode, rawState] = input.split('#', 2);
      code = rawCode || null;
      callbackState = rawState || null;
    }

    if (!code) {
      showToast('No authorization code found. Paste the full callback URL.', 'error');
      return;
    }

    const exchangeResp = await api(`/oauth/${authData.provider}/exchange`, {
      method: 'POST',
      body: JSON.stringify({
        code,
        redirect_uri: authData.redirect_uri,
        code_verifier: authData.code_verifier,
        state: callbackState || authData.state,
      }),
    });

    if (exchangeResp.error) throw new Error(exchangeResp.error);

    showToast(`Logged in with ${authData.provider}`, 'success');
    document.getElementById('oauth-manual-section').style.display = 'none';
    state.accounts = await api('/accounts');
    rerenderCurrentView();
  },

  async startDeviceCode(provider) {
    try {
      const resp = await api(`/oauth/${provider}/device-code`, {
        method: 'POST',
      });
      if (resp.error) throw new Error(resp.error);

      // Show device code UI
      const deviceInfo = document.getElementById('oauth-device-info');
      if (deviceInfo) {
        deviceInfo.innerHTML = `
          <div class="device-code-flow">
            <p>To log in with ${escapeHtml(provider)}:</p>
            <ol>
              <li>Open <a href="${escapeAttr(resp.verification_uri)}" target="_blank" rel="noopener">${escapeHtml(resp.verification_uri)}</a></li>
              <li>Enter code: <strong class="copy-text">${escapeHtml(resp.user_code)}</strong></li>
            </ol>
            <p class="polling-status">Waiting for authorization...</p>
          </div>
        `;
        deviceInfo.style.display = 'block';
      }

      // Poll for completion
      const pollInterval = setInterval(async () => {
        try {
          const pollResp = await api(`/oauth/${provider}/device-poll`, {
            method: 'POST',
            body: JSON.stringify({ device_code: resp.device_code }),
          });

          if (pollResp.status === 'complete') {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = 'none';
            showToast(`Logged in with ${provider}`, 'success');
            state.accounts = await api('/accounts');
            rerenderCurrentView();
          } else if (pollResp.status === 'expired') {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = 'none';
            showToast('Device code expired', 'error');
          }
          // else: still polling, continue
        } catch (err) {
          // Poll error, continue
        }
      }, 5000);

    } catch (err) {
      showToast(`Device code failed: ${err.message}`, 'error');
    }
  },
};

function showToast(message, type) {
  const toast = document.createElement('div');
  toast.className = `toast toast-${type}`;
  toast.textContent = message;
  document.body.appendChild(toast);
  setTimeout(() => toast.classList.add('show'), 10);
  setTimeout(() => {
    toast.classList.remove('show');
    setTimeout(() => toast.remove(), 300);
  }, 3000);
}

// ===== Utilities =====
function escapeHtml(s) {
  if (s == null) return '';
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function escapeAttr(s) {
  return escapeHtml(s);
}

// Pull the human-readable `message` field out of a `{"error":{"code",
// "message"}}` envelope produced by the server's `ApiError` impl.
// Returns `null` if the body isn't in that shape (e.g. it was a
// network failure with no body), so the caller can fall back to
// the generic Error message. The thrower is `api()` above, which
// raises `new Error(`${r.status}: ${txt}`)`; the JSON body is
// therefore embedded as a string suffix in `e.message`, and we
// re-parse it here. Doing it as a regex on the message (instead
// of routing the raw body around) keeps the call sites of `api()`
// unchanged.
function extractApiErrorMessage(e) {
  if (!e || typeof e.message !== 'string') return null;
  // The body lives after the first colon in the thrower's
  // "<status>: <body>" message. The body itself is a JSON object,
  // so we look for `"message":"..."` and pull the inner string.
  const m = e.message.match(/"error"\s*:\s*\{[\s\S]*?"message"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (!m) return null;
  // Unescape the JSON string just enough to handle the common
  // cases (the server's error messages contain `\"` and `\\`).
  try {
    return JSON.parse('"' + m[1] + '"');
  } catch (_) {
    return m[1];
  }
}

// ===== Config =====
//
// **Variant A of the "Config" menu (honest stub).** The dashboard
// displays the timeouts/retries/circuit-breaker/racing values that
// were loaded from `config.toml` at server startup. The values are
// read-only here — there is no PUT endpoint, no DB table, and no
// in-memory mutation path. The intent is to keep the user informed
// about the active configuration without lying about being able to
// edit it.
//
// The "Save" button is intentionally non-functional: clicking it
// shows a toast that explains *why* it can't do anything. Anyone who
// needs to change a value must edit `config.toml` (or set the
// `OPENPROXY_*` env override) and restart the process. When/if we
// add hot-reload (variant B), the inputs flip from `disabled` to
// editable and a `PUT /v1/admin/config` is wired up — but that's a
// follow-up with its own scoping decision (see the spec notes).
//
// The view is *not* subscribed to the 3 s background poll: there's
// nothing on the page that can change without a restart, and polling
// would needlessly burn a request every 3 s for zero value.
async function renderConfig() {
  document.getElementById('main').innerHTML = `
    <div class="page-header">
      <h2>Config</h2>
    </div>
    <p class="muted">
      These values are read from <code>config.toml</code> at server
      startup (with <code>OPENPROXY_*</code> env-var overrides). Timeouts
      are editable below; the other sections reflect the loaded TOML.
    </p>
    <div class="loading">Loading...</div>
  `;
  let cfg;
  try {
    cfg = await api('/config');
  } catch (e) {
    document.getElementById('main').innerHTML =
      `<div class="error">Error: ${escapeHtml(e.message)}</div>`;
    return;
  }

  // The wire shape is { timeouts, retries, circuit_breaker, racing }.
  // Each sub-object's keys mirror the matching `[section]` in
  // config.example.toml, so the operator can copy the values back
  // into the file verbatim.
  const t = cfg.timeouts || {};
  const r = cfg.retries || {};
  const cb = cfg.circuit_breaker || {};
  const rc = cfg.racing || {};

  const fmtMs = (v) => (v == null ? '—' : `${v} ms`);

  // Renderer for a number input. `editable=false` keeps the legacy
  // read-only look (`disabled` + muted background); `editable=true`
  // omits `disabled` so the operator can type. The CSS
  // `.config-field input[type="number"]:disabled` only matches
  // when the attribute is present, so dropping it is enough to
  // flip the visual.
  const field = (label, name, value, help, opts = {}) => `
    <label class="config-field">
      <span class="config-label">${escapeHtml(label)}</span>
      <input type="number" name="${escapeHtml(name)}"
             value="${escapeHtml(String(value ?? ''))}"
             min="0" step="100"
             ${opts.editable ? '' : 'disabled'}
             aria-label="${escapeHtml(label)}${opts.editable ? '' : ' (read-only)'}">
      <span class="config-help">${escapeHtml(help)}</span>
    </label>
  `;

  document.getElementById('main').innerHTML = `
    <div class="page-header">
      <h2>Config</h2>
    </div>

    <div id="config-banner" class="banner banner-info">
      <strong>Live values.</strong>
      The values below are the ones the server is currently using.
      Timeouts are editable; the other sections reflect the loaded
      <code>config.toml</code>. Changes to timeouts are persisted in
      the database and apply to the next request.
    </div>

    <section class="card">
      <h3>Timeouts <small>(ms)</small></h3>
      <p class="muted">
        Precedence (highest wins):
        <code>model&nbsp;overrides</code> →
        <code>provider_timeouts</code> →
        <code>system&nbsp;default</code>. The system default is what
        you see here. Editing these values takes effect on the next
        request; in-flight requests keep the previous value.
      </p>
      <div class="config-grid">
        ${field('connect_ms', 'timeouts.connect_ms', t.connect_ms, 'TCP connect to the upstream.', {editable: true})}
        ${field('request_send_ms', 'timeouts.request_send_ms', t.request_send_ms, 'Max time to flush the request body.', {editable: true})}
        ${field('ttft_ms', 'timeouts.ttft_ms', t.ttft_ms, 'Time-to-first-token for streaming responses.', {editable: true})}
        ${field('idle_chunk_ms', 'timeouts.idle_chunk_ms', t.idle_chunk_ms, 'Max gap between SSE chunks.', {editable: true})}
        ${field('total_ms', 'timeouts.total_ms', t.total_ms, 'Hard ceiling for the whole request.', {editable: true})}
      </div>
    </section>

    <section class="card">
      <h3>Retries</h3>
      <div class="config-grid">
        ${field('max_attempts', 'retries.max_attempts', r.max_attempts, 'Including the first try.')}
        ${field('backoff_base_ms', 'retries.backoff_base_ms', r.backoff_base_ms, 'Initial backoff.')}
        ${field('backoff_factor', 'retries.backoff_factor', r.backoff_factor, 'Exponential factor.')}
        ${field('backoff_jitter_pct', 'retries.backoff_jitter_pct', r.backoff_jitter_pct, 'Random jitter % to avoid thundering herds.')}
      </div>
    </section>

    <section class="card">
      <h3>Circuit Breaker</h3>
      <div class="config-grid">
        ${field('failure_threshold', 'circuit_breaker.failure_threshold', cb.failure_threshold, 'Consecutive failures before a target is parked.')}
        ${field('unhealthy_duration_ms', 'circuit_breaker.unhealthy_duration_ms', cb.unhealthy_duration_ms, 'How long a parked target stays out of rotation.')}
      </div>
    </section>

    <section class="card">
      <h3>Racing</h3>
      <div class="config-grid">
        ${field('default_race_size', 'racing.default_race_size', rc.default_race_size, 'Default number of parallel targets per combo.')}
        ${field('max_race_size', 'racing.max_race_size', rc.max_race_size, 'Upper bound the dashboard can set.')}
        ${field('abort_grace_ms', 'racing.abort_grace_ms', rc.abort_grace_ms, 'Grace period before aborting losing branches.')}
      </div>
    </section>

    <div class="config-actions">
      <button class="primary" type="button"
              onclick="window.configSaveTimeouts(event)">Save</button>
      <span class="muted">
        Saves the timeouts above. The other sections are
        read-only here; edit <code>config.toml</code> and restart
        to change them.
      </span>
    </div>

    <details class="config-details">
      <summary>What does the precedence chain look like?</summary>
      <p>
        The pipeline resolves the effective timeouts on every
        request via
        <code>openproxy_core::timeouts::resolve</code>:
      </p>
      <ol>
        <li>Start with the system defaults shown above (this view).</li>
        <li>Override <code>connect</code>, <code>request_send</code>,
            and <code>total</code> from
            <code>provider_timeouts</code> if a row exists for the
            selected provider.</li>
        <li>Override <code>ttft</code> and <code>idle_chunk</code>
            from <code>models.timeout_overrides_json</code> if the
            target model sets them.</li>
      </ol>
      <p>
        Provider/model overrides live in the database (not in
        <code>config.toml</code>), so they <em>can</em> change
        without a restart — but they are not exposed in this view.
        Use the Providers / Combos detail screens for those.
      </p>
    </details>
  `;
}

// Swap the banner at the top of the Config view to a new
// "type"/"title"/"body" triple. `type` is one of 'info' / 'success'
// (drives the CSS class). Used by `configSaveTimeouts` to flip from
// the initial blue/amber `.banner-info` to the green `.banner--success`
// after a successful PUT.
function swapBanner(bannerType, title, body) {
  const el = document.getElementById('config-banner');
  if (!el) return;
  // Strip the previous type-modifier class (banner-info / banner--success).
  el.classList.remove('banner-info', 'banner--success');
  el.classList.add('banner-' + bannerType);
  el.innerHTML =
    `<strong>${escapeHtml(title)}</strong> ${escapeHtml(body)}`;
}

// Read the five timeouts inputs and return a TimeoutsConfig-shaped
// object. Returns `null` and toasts an error on any missing/empty/
// non-numeric field, so the caller can short-circuit before hitting
// the network.
function readTimeoutsFromInputs() {
  const fields = [
    'connect_ms', 'request_send_ms', 'ttft_ms', 'idle_chunk_ms', 'total_ms',
  ];
  const out = {};
  for (const f of fields) {
    const el = document.querySelector(
      `input[name="timeouts.${f}"]`
    );
    if (!el) {
      showToast(`timeouts.${f} input is missing from the DOM`, 'error');
      return null;
    }
    const raw = (el.value || '').trim();
    if (raw === '') {
      showToast(`timeouts.${f} is required`, 'error');
      return null;
    }
    // Accept only non-negative integers; reject decimals / signs /
    // scientific notation by going through Number and re-checking
    // that it's a safe u64.
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n) || !/^\d+$/.test(raw)) {
      showToast(`timeouts.${f} must be a non-negative integer`, 'error');
      return null;
    }
    out[f] = n;
  }
  return out;
}

// Click handler for the "Save" button on the Config view. PUTs the
// edited timeouts to the server, then flips the banner to a green
// `.banner--success`. On failure, toasts the server's error message.
window.configSaveTimeouts = async function(ev) {
  if (ev) ev.preventDefault();
  const t = readTimeoutsFromInputs();
  if (!t) return; // readTimeoutsFromInputs already toasted.
  try {
    await api('/config/timeouts', {
      method: 'PUT',
      body: JSON.stringify(t),
    });
    showToast('Config updated — applies to next requests', 'success');
    swapBanner(
      'success',
      'Live — applies to next requests',
      'The values below are persisted in the database and will ' +
      'take effect on the next request. Requests already in flight ' +
      'continue with the previous values.'
    );
  } catch (e) {
    // e.message is "<status>: <body>" from the api() helper. Pull
    // the server's human-readable message out of the JSON envelope
    // (it lives in {"error":{"message":"..."}}); fall back to the
    // raw message if the envelope isn't there.
    showToast(extractApiErrorMessage(e) || e.message, 'error');
  }
};

// ===== API Keys =====
//
// Each key has a metadata row (id, label, prefix, scopes, etc.) and
// an opaque *plaintext* that is shown exactly once — on creation
// and on regeneration. The plaintext is never re-derivable from the
// DB (only the SHA-256 hash is stored), so the modal that displays
// it must also remind the user to copy it.
//
// Status pills reflect the soft-disable state: a revoked key shows
// the "revoked" pill even though `is_active` is also 0; the
// `revoked_at` stamp is the authoritative "this happened" signal.

async function renderKeys() {
  // Always refetch the keys list (the user may have hit "Revoke" /
  // "Delete" elsewhere, or the background poll has updated the row),
  // and bring models along so the create/edit modals don't have to
  // do a second round-trip when the user clicks "+ Create" or "Edit".
  const [keys, models] = await Promise.all([
    api('/keys'),
    api('/models'),
  ]);
  state.apiKeys = keys;
  state.models = models;

  let html = `
    <div class="page-header">
      <h2>API Keys</h2>
      <button class="primary" onclick="showCreateKey()">+ Create key</button>
    </div>
  `;

  if (keys.length === 0) {
    html += `<p class="empty">No API keys yet. Create one to authenticate clients.</p>`;
  } else {
    html += `<table>
      <thead><tr><th>Label</th><th>Prefix</th><th>Scopes</th><th>Allowed models</th><th>Status</th><th>Last used</th><th>Created</th><th>Actions</th></tr></thead>
      <tbody>`;
    for (const k of keys) {
      const scopes = (k.scopes || []).join(', ') || '—';
      let allowedModels = 'all';
      if (k.allowed_models === null || k.allowed_models === undefined) {
        allowedModels = 'all';
      } else if (Array.isArray(k.allowed_models) && k.allowed_models.length === 0) {
        allowedModels = '(empty)';
      } else if (Array.isArray(k.allowed_models)) {
        allowedModels = k.allowed_models.length + ' models';
      }
      const isActive = k.is_active && !k.revoked_at;
      const statusClass = isActive ? 'on' : 'off';
      const statusText = k.revoked_at ? 'revoked' : (k.is_active ? 'active' : 'inactive');
      const createdBy = k.created_by ? ` <small>(${escapeHtml(k.created_by)})</small>` : '';
      html += `
        <tr>
          <td>${escapeHtml(k.label || '—')}${createdBy}</td>
          <td><code>${escapeHtml(k.key_prefix || '—')}</code></td>
          <td>${escapeHtml(scopes)}</td>
          <td>${escapeHtml(allowedModels)}</td>
          <td><span class="status-pill ${statusClass}">${statusText}</span></td>
          <td>${escapeHtml(k.last_used_at || 'never')}</td>
          <td>${escapeHtml(k.created_at || '—')}</td>
          <td>
            <button class="small" onclick="showEditKey(${k.id})">Edit</button>
            <button class="small" onclick="regenerateKey(${k.id}, '${escapeAttr(k.label || '')}')">Regenerate</button>
            <button class="small" onclick="viewKeyUsage(${k.id})">Usage</button>
            ${k.is_active && !k.revoked_at
              ? `<button class="small" onclick="revokeKey(${k.id}, '${escapeAttr(k.label || '')}')">Revoke</button>`
              : ''}
            <button class="small danger" onclick="deleteKey(${k.id}, '${escapeAttr(k.label || '')}')">Delete</button>
          </td>
        </tr>
      `;
    }
    html += '</tbody></table>';
  }

  document.getElementById('main').innerHTML = html;
}

window.showCreateKey = async function() {
  // The model picker needs the model catalog. `renderKeys` already
  // refreshes `state.models` on entry, but the user can land here
  // from a stale background poll (no keys yet, no models cached),
  // so a defensive refetch is cheap and avoids an empty picker.
  if (!state.models || state.models.length === 0) {
    state.models = await api('/models');
  }
  const html = renderKeyFormHtml({ mode: 'create' });
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
  // The form template ships with a "all models" placeholder chip
  // for simplicity; the actual chips (when editing) are derived
  // from the hidden input by re-rendering after the form mounts.
  renderAllowedModelsChips();
};

window.showEditKey = async function(id) {
  if (!state.models || state.models.length === 0) {
    state.models = await api('/models');
  }
  // Pull the *current* key row so we prefill scopes / models / expiry.
  // The cache may be a tick behind, so a dedicated GET is safer than
  // patching the cached copy.
  let key;
  try {
    key = await api('/keys/' + id);
  } catch (e) {
    alert('Error: ' + e.message);
    return;
  }
  const html = renderKeyFormHtml({ mode: 'edit', key });
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
  // Same reasoning as `showCreateKey`: paint the real chips now
  // that the hidden input has the row's `allowed_models` and the
  // chip container is in the DOM.
  renderAllowedModelsChips();
};

// Shared form-rendering for create + edit. The two flows share the
// same DOM, the same `state.models` / `state.modelPickerSelection`
// set, and the same submit-pipeline helpers — only the submit
// callback and the prefilled values differ.
function renderKeyFormHtml({ mode, key }) {
  const isEdit = mode === 'edit';
  const labelVal = isEdit ? (key.label || '') : '';
  const scopes = isEdit ? (key.scopes || []) : ['chat'];
  // The three allowed_models states — `null` (all), `[]` (none),
  // `[a,b,c]` (specific list) — are all distinguishable, but the
  // hidden input is a single string, so we encode them as:
  //   ""            → null  (all models)
  //   " "           → []    (empty list = no models allowed)
  //   "a,b,c"       → ["a","b","c"]
  // The single space sentinel for "[]" is necessary because an
  // empty string is overloaded with "all". The string is parsed
  // back in `buildKeyBodyFromForm`.
  let allowedModelsValue = '';
  if (isEdit && Array.isArray(key.allowed_models)) {
    allowedModelsValue = key.allowed_models.length === 0
      ? ' '
      : key.allowed_models.join(',');
  }
  // Seed the picker selection from the existing row (edit) or empty
  // (create). We have to do this *before* rendering the chips, but
  // `state.modelPickerSelection` lives in module scope and the
  // initial `renderAllowedModelsChips` reads from the hidden input.
  // The form's hidden input is prefilled below, then the chip area
  // is built off that same value via `getCurrentAllowedModels()`.
  const expiry = isEdit ? formatExpiryForInput(key.expires_at) : { amount: '', unit: 'never' };
  const title = isEdit ? `Edit API key #${key.id}` : 'Create API key';

  // The picker modal is rendered once, at the bottom of <main>, the
  // first time the form opens. Subsequent opens just toggle the
  // `display` style. This avoids duplicate `#model-picker-modal`
  // nodes when the user opens Create, closes it, opens Edit.
  ensureModelPickerModal();

  return `
    <div class="modal-bg" onclick="if(event.target===this) closeKeyForm(this)">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>${escapeHtml(title)}</h2>
          <button type="button" class="close-btn" onclick="closeKeyForm(this.closest('.modal-bg'))" aria-label="Close">&times;</button>
        </div>
        <form onsubmit="${isEdit ? `updateKey(event, ${key.id})` : 'createKey(event)'}">
          <div class="modal-body">
            <div class="field">
              <label for="key-label">Label</label>
              <input id="key-label" name="label" type="text" placeholder="my-app" value="${escapeAttr(labelVal)}" required>
            </div>

            <div class="field">
              <span class="field-label">Scopes</span>
              <div class="scopes-list">
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="chat" ${scopes.includes('chat') ? 'checked' : ''}>
                  <div class="scope-info">
                    <strong>chat</strong>
                    <small>Can use /v1/chat/completions</small>
                  </div>
                </label>
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="manage" ${scopes.includes('manage') ? 'checked' : ''}>
                  <div class="scope-info">
                    <strong>manage</strong>
                    <small>Can use /v1/admin/* (CRUD providers, accounts, etc.)</small>
                  </div>
                </label>
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="read" ${scopes.includes('read') ? 'checked' : ''}>
                  <div class="scope-info">
                    <strong>read</strong>
                    <small>Can use analytics endpoints (GET only)</small>
                  </div>
                </label>
              </div>
            </div>

            <div class="field">
              <span class="field-label">Allowed models (empty = all)</span>
              <div class="model-picker-display" id="model-picker-display">
                <span class="muted">all models</span>
                <button type="button" class="link-btn" onclick="openModelPickerModal()">Edit</button>
              </div>
              <input type="hidden" name="allowed_models" value="${escapeAttr(allowedModelsValue)}">
            </div>

            <div class="field">
              <label for="key-expires-amount">Expires in</label>
              <div class="expiry-row">
                <input id="key-expires-amount" type="number" name="expires_amount" min="1" max="999" placeholder="30" value="${escapeAttr(String(expiry.amount))}" ${expiry.unit === 'never' ? 'disabled' : ''}>
                <select name="expires_unit" onchange="toggleExpiryAmount(this)">
                  <option value="days" ${expiry.unit === 'days' ? 'selected' : ''}>days</option>
                  <option value="months" ${expiry.unit === 'months' ? 'selected' : ''}>months</option>
                  <option value="years" ${expiry.unit === 'years' ? 'selected' : ''}>years</option>
                  <option value="never" ${expiry.unit === 'never' ? 'selected' : ''}>never</option>
                </select>
              </div>
            </div>
          </div>

          <div class="modal-footer">
            <button type="button" onclick="closeKeyForm(this.closest('.modal-bg'))">Cancel</button>
            <button type="submit" class="primary">${isEdit ? 'Save' : 'Create key'}</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

// Click-on-backdrop / Cancel button. We have to be careful: in the
// edit case the model picker modal can sit on top, and closing the
// parent modal should *also* close the picker so the user doesn't
// see a dangling overlay. Single-argument variant for the
// `onclick="closeKeyForm(this)"` path is supported because the
// Cancel button calls `this.closest('.modal-bg')` itself.
function closeKeyForm(modalBg) {
  if (!modalBg || !modalBg.parentElement) return;
  modalBg.remove();
  const picker = document.getElementById('model-picker-modal');
  if (picker) picker.style.display = 'none';
}

// ===== Expiry helpers =====
//
// The dashboard never edits an absolute timestamp; it edits a delta
// (amount + unit) and `calculateExpiry` resolves it on submit.
// `formatExpiryForInput` is the inverse: given an ISO string, pick
// the best-fitting (amount, unit) approximation so the edit form
// pre-fills something close to the truth.

window.toggleExpiryAmount = function(select) {
  const row = select.parentElement;
  const amount = row.querySelector('input[name="expires_amount"]');
  if (!amount) return;
  amount.disabled = select.value === 'never';
  if (select.value === 'never') amount.value = '';
};

function calculateExpiry(amount, unit) {
  if (unit === 'never' || !amount) return null;
  const n = parseInt(amount, 10);
  if (!Number.isFinite(n) || n <= 0) return null;
  const now = new Date();
  if (unit === 'days')   now.setDate(now.getDate() + n);
  else if (unit === 'months') now.setMonth(now.getMonth() + n);
  else if (unit === 'years')  now.setFullYear(now.getFullYear() + n);
  else return null;
  return now.toISOString();
}

function formatExpiryForInput(isoString) {
  if (!isoString) return { amount: '', unit: 'never' };
  const expiry = new Date(isoString);
  const now = new Date();
  const diffMs = expiry - now;
  if (!Number.isFinite(diffMs) || diffMs < 0) return { amount: '', unit: 'never' };

  const MS_PER_DAY = 1000 * 60 * 60 * 24;
  // Approximate: prefer the *largest* unit that yields a value
  // >= 1, so an expiry of 90 days shows as "3 months" and an
  // expiry of 400 days shows as "1 year". The 30/365 day constants
  // are deliberate — DST, leap years, and varying month lengths
  // would over-fit a calendar-accurate conversion.
  const diffYears  = Math.floor(diffMs / (MS_PER_DAY * 365));
  if (diffYears >= 1) return { amount: diffYears, unit: 'years' };
  const diffMonths = Math.floor(diffMs / (MS_PER_DAY * 30));
  if (diffMonths >= 1) return { amount: diffMonths, unit: 'months' };
  const diffDays   = Math.floor(diffMs / MS_PER_DAY);
  return { amount: Math.max(1, diffDays), unit: 'days' };
}

window.createKey = async function(e) {
  e.preventDefault();
  const body = buildKeyBodyFromForm(e.target);
  if (!body) return; // buildKeyBodyFromForm already alerted
  try {
    const result = await api('/keys', {
      method: 'POST',
      body: JSON.stringify(body),
    });
    // Drop the create-form modal *before* showing the plaintext modal
    // so we don't end up with two stacked overlays. Also close the
    // picker in case the user opened it then submitted.
    closeKeyForm(e.target.closest('.modal-bg'));
    showPlaintextKey(result.plaintext, result.key);
  } catch (err) { alert('Error: ' + err.message); }
};

window.updateKey = async function(e, id) {
  e.preventDefault();
  const body = buildKeyBodyFromForm(e.target);
  if (!body) return;
  try {
    await api('/keys/' + id, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    closeKeyForm(e.target.closest('.modal-bg'));
    // Refresh the cache so the row in the table reflects the new
    // label/scopes/allowed_models on the next render.
    state.apiKeys = await api('/keys');
    navigate();
  } catch (err) { alert('Error: ' + err.message); }
};

// Pull the editable fields out of the create/edit form and shape
// them for the API body. Returns null and alerts the user if a
// required field is missing (currently: at least one scope).
function buildKeyBodyFromForm(form) {
  // `FormData.getAll` works for the old `<select multiple>` pattern
  // but not for the new checkbox group; the explicit DOM scan is
  // more honest about the field's actual shape.
  const scopes = Array.from(form.querySelectorAll('input[name="scopes"]:checked'))
    .map(input => input.value);
  if (scopes.length === 0) {
    alert('Pick at least one scope.');
    return null;
  }
  const allowedModelsStr = (form.querySelector('input[name="allowed_models"]').value || '');
  // Three-state encoding on the hidden input:
  //   ""   → null  (no key / all models)
  //   " "  → []    (empty list, no models allowed)
  //   "x,y"→ ["x","y"]
  let allowedModels;
  if (allowedModelsStr === '') {
    allowedModels = null;
  } else if (allowedModelsStr === ' ') {
    allowedModels = [];
  } else {
    allowedModels = allowedModelsStr.split(',').map(s => s.trim()).filter(Boolean);
  }
  const amount = form.querySelector('input[name="expires_amount"]').value;
  const unit = form.querySelector('select[name="expires_unit"]').value;
  const expiresAt = calculateExpiry(amount, unit);
  const label = (form.querySelector('input[name="label"]').value || '').trim() || null;
  return {
    label,
    scopes,
    allowed_models: allowedModels,
    expires_at: expiresAt,
  };
}

// ===== Model picker (search + multi-select) =====
//
// Lives as a single modal node in the DOM (`#model-picker-modal`),
// opened by `openModelPickerModal` and closed by
// `closeModelPickerModal`. The selection is a Set of `model_id`
// strings; the chips on the parent form are derived from the
// hidden input value on every close.

function ensureModelPickerModal() {
  if (document.getElementById('model-picker-modal')) return;
  const html = `
    <div class="modal-bg modal-picker-bg" id="model-picker-modal" style="display: none;" onclick="if(event.target===this) closeModelPickerModal()">
      <div class="modal modal-picker" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>Select models</h2>
          <button type="button" class="close-btn" onclick="closeModelPickerModal()" aria-label="Close">&times;</button>
        </div>
        <div class="picker-search">
          <input type="text" id="model-picker-search" placeholder="Search models..." oninput="filterModelPicker()">
        </div>
        <div class="modal-body">
          <div class="model-picker-list" id="model-picker-list"></div>
        </div>
        <div class="modal-footer">
          <button type="button" onclick="clearModelPicker()">Clear all</button>
          <button type="button" class="primary" onclick="closeModelPickerModal()">Done</button>
        </div>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
}

window.openModelPickerModal = function() {
  // Re-seed the working set from the committed hidden input so
  // cancel + reopen is non-destructive: a user who adds 5 models,
  // closes by clicking the backdrop, then re-opens, sees the same
  // 5 selected (the hidden input was untouched). The empty-array
  // case (" " sentinel) becomes an empty working set; the all-models
  // case ("") also becomes empty because the user has not picked
  // any *restrictions* yet — adding any checkbox will switch the
  // hidden input from "" to "model_id" on commit.
  const current = getCurrentAllowedModels();
  state.modelPickerSelection = new Set(current || []);
  document.getElementById('model-picker-modal').style.display = 'flex';
  const search = document.getElementById('model-picker-search');
  if (search) { search.value = ''; search.focus(); }
  renderModelPickerList();
};

window.closeModelPickerModal = function() {
  // Commit the working set back to the hidden input. We *only*
  // commit on explicit close (Done button or backdrop click), so
  // an accidental Escape via the parent form's Cancel doesn't lose
  // work — it just leaves the picker state alone, and the chips
  // reflect the last committed selection.
  //
  // Three-state encoding rule for an empty working set on close:
  // if the user actively *removed* the last selected model
  // (previous hidden value was a non-empty list), keep " " so the
  // empty-list semantics ("no models allowed") survive — that's
  // the natural intent of "uncheck the last item". If the previous
  // state was already "all models" (hidden == "") or "no models"
  // (hidden == " "), leave it alone.
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    if (state.modelPickerSelection.size === 0) {
      const hadModels = hidden.value !== '' && hidden.value !== ' ';
      if (hadModels) hidden.value = ' ';
    } else {
      hidden.value = Array.from(state.modelPickerSelection).join(',');
    }
  }
  renderAllowedModelsChips();
  const modal = document.getElementById('model-picker-modal');
  if (modal) modal.style.display = 'none';
};

window.clearModelPicker = function() {
  // "Clear all" means "this key may not use any model" — the
  // empty-list semantics. Set the sentinel directly so the
  // three-state encoding is preserved; closeModelPickerModal will
  // see size==0 and write " " back.
  state.modelPickerSelection = new Set();
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) hidden.value = ' ';
  renderModelPickerList();
};

function renderModelPickerList() {
  const list = document.getElementById('model-picker-list');
  if (!list) return;
  const allModels = state.models || [];
  const search = ((document.getElementById('model-picker-search') || {}).value || '').toLowerCase();
  const filtered = allModels.filter(m => {
    if (!search) return true;
    return m.model_id.toLowerCase().includes(search);
  });
  if (filtered.length === 0) {
    list.innerHTML = `<div class="model-picker-row"><span class="muted">No models match.</span></div>`;
    return;
  }
  list.innerHTML = filtered.map(m => {
    const checked = state.modelPickerSelection.has(m.model_id);
    return `
      <label class="model-picker-row">
        <input type="checkbox" ${checked ? 'checked' : ''} onchange="toggleModelPicker('${escapeAttr(m.model_id)}', this.checked)">
        <span class="model-id">${escapeHtml(m.model_id)}</span>
        <span class="model-provider">${escapeHtml(m.provider_id)}</span>
      </label>
    `;
  }).join('');
}

window.toggleModelPicker = function(modelId, checked) {
  if (checked) state.modelPickerSelection.add(modelId);
  else state.modelPickerSelection.delete(modelId);
};

window.filterModelPicker = function() {
  renderModelPickerList();
};

window.removeModelFromKey = function(modelId) {
  // The chip X operates on the *committed* state (the hidden input)
  // and then syncs the picker working set. Doing it in this order
  // means: if the user just removed chips via the X without ever
  // opening the picker, the picker's working set still reflects
  // the latest committed state on the next open (see
  // `openModelPickerModal`, which re-seeds from the hidden input).
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (hidden) {
    // Preserve the "no models" sentinel (" ") when removing the
    // last chip from a list-of-one — otherwise an empty join would
    // be parsed back as "all models" in buildKeyBodyFromForm, which
    // is a different (and unintended) state.
    const wasNoModels = hidden.value === ' ';
    const current = (wasNoModels ? [] : hidden.value.split(',').map(s => s.trim()).filter(Boolean));
    const next = current.filter(m => m !== modelId);
    if (next.length === 0) {
      hidden.value = ' ';
    } else {
      hidden.value = next.join(',');
    }
  }
  // Mirror the removal into the picker working set if the picker
  // is open, so the checkbox visually unchecks too.
  const pickerOpen = document.getElementById('model-picker-modal').style.display !== 'none';
  if (pickerOpen) {
    state.modelPickerSelection.delete(modelId);
    renderModelPickerList();
  }
  renderAllowedModelsChips();
};

function getCurrentAllowedModels() {
  // Returns the *committed* model list (or null for "all models",
  // or [] for "no models"). The hidden input uses the three-state
  // encoding: "" → null, " " → [], "a,b" → ["a","b"]. The picker
  // working set (state.modelPickerSelection) is a parallel
  // representation used only while the picker modal is open.
  const hidden = document.querySelector('input[name="allowed_models"]');
  if (!hidden) return null;
  const v = hidden.value;
  if (v === '') return null;
  if (v === ' ') return [];
  return v.split(',').map(s => s.trim()).filter(Boolean);
}

function renderAllowedModelsChips() {
  const display = document.getElementById('model-picker-display');
  if (!display) return;
  const models = getCurrentAllowedModels();
  if (models === null) {
    display.innerHTML = '<span class="muted">all models</span> <button type="button" onclick="openModelPickerModal()">Edit</button>';
  } else if (models.length === 0) {
    display.innerHTML = '<span class="muted">no models</span> <button type="button" onclick="openModelPickerModal()">Edit</button>';
  } else {
    const chips = models.map(m =>
      `<span class="model-chip">${escapeHtml(m)} <button type="button" onclick="removeModelFromKey('${escapeAttr(m)}')">&times;</button></span>`
    ).join('');
    display.innerHTML = `${chips} <button type="button" onclick="openModelPickerModal()">Edit</button>`;
  }
}

// Modal that displays the one-shot plaintext. The user must copy
// it; "I've saved it" closes the modal AND refetches the key list
// so the new row is visible. The `navigate()` call re-enters the
// current route, which re-runs `renderKeys`.
function showPlaintextKey(plaintext, metadata) {
  const html = `
    <div class="modal-bg">
      <div class="modal" onclick="event.stopPropagation()">
        <div class="modal-header">
          <h2>Save this key now</h2>
          <button type="button" class="close-btn" onclick="this.closest('.modal-bg').remove(); navigate();" aria-label="Close">&times;</button>
        </div>
        <div class="modal-body">
          <p>This is the <strong>only time</strong> you'll see this key. Copy it now and store it securely.</p>
          <div class="key-display">
            <code id="plaintext-key">${escapeHtml(plaintext)}</code>
            <button id="copy-key-btn" type="button">Copy</button>
          </div>
          <p><small>Label: ${escapeHtml(metadata && metadata.label ? metadata.label : '—')} · Prefix: <code>${escapeHtml(metadata && metadata.key_prefix ? metadata.key_prefix : '—')}</code></small></p>
        </div>
        <div class="modal-footer">
          <button type="button" class="primary" onclick="this.closest('.modal-bg').remove(); navigate();">I've saved it</button>
        </div>
      </div>
    </div>
  `;
  document.getElementById('main').insertAdjacentHTML('beforeend', html);
  // Wire the copy button after the HTML is in the DOM. We avoid
  // putting the secret in the inline onclick string to keep it
  // out of the DOM attribute.
  const copyBtn = document.getElementById('copy-key-btn');
  if (copyBtn) {
    copyBtn.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(plaintext);
        copyBtn.textContent = 'Copied!';
      } catch (e) {
        // Clipboard API blocked (e.g. non-secure context): fall
        // back to selecting the text in a temporary textarea.
        const ta = document.createElement('textarea');
        ta.value = plaintext;
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand('copy'); copyBtn.textContent = 'Copied!'; }
        catch (_) { copyBtn.textContent = 'Copy failed'; }
        finally { document.body.removeChild(ta); }
      }
    });
  }
}

window.regenerateKey = async function(id, label) {
  const display = label || ('#' + id);
  if (!confirm(`Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`)) return;
  try {
    const result = await api(`/keys/${id}/regenerate`, { method: 'POST' });
    showPlaintextKey(result.plaintext, result.key);
  } catch (e) { alert('Error: ' + e.message); }
};

window.revokeKey = async function(id, label) {
  const display = label || ('#' + id);
  if (!confirm(`Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`)) return;
  try {
    await api(`/keys/${id}/revoke`, { method: 'POST' });
    state.apiKeys = await api('/keys');
    navigate();
  } catch (e) { alert('Error: ' + e.message); }
};

window.viewKeyUsage = function(id) {
  location.hash = `#/keys/${id}/usage`;
};

window.deleteKey = async function(id, label) {
  const display = label || ('#' + id);
  if (!confirm(`Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`)) return;
  try {
    await api(`/keys/${id}`, { method: 'DELETE' });
    state.apiKeys = state.apiKeys.filter(k => k.id !== id);
    navigate();
  } catch (e) { alert('Error: ' + e.message); }
};

// Per-key usage recap. Reuses the analytics endpoints by adding
// `api_key_id` to the query string, which the server maps to the
// `usage.api_key_id` column.
async function renderKeyUsage(keyId) {
  const [head, summary] = await Promise.all([
    api(`/keys/${keyId}/usage`),
    api(`/usage/summary?api_key_id=${keyId}`),
  ]);
  const k = head.key || {};
  const s = head.summary || {};
  const unique = s.unique_requests ?? 0;
  const total = s.total_rows ?? 0;
  const errors = s.errors ?? 0;
  const cost = (s.total_cost_usd ?? 0).toFixed(4);
  const last = k.last_used_at || 'never';
  const html = `
    <div class="page-header">
      <a href="#/keys" class="back-link">← All keys</a>
      <h2>API key #${keyId} usage</h2>
    </div>
    <section class="detail-section">
      <div class="section-header">
        <h3>Headline metrics</h3>
      </div>
      <table>
        <tbody>
          <tr><th>Total rows</th><td>${total}</td></tr>
          <tr><th>Unique requests</th><td>${unique}</td></tr>
          <tr><th>Errors (4xx/5xx)</th><td>${errors}</td></tr>
          <tr><th>Total cost (USD)</th><td>$${cost}</td></tr>
          <tr><th>Last used</th><td>${escapeHtml(last)}</td></tr>
        </tbody>
      </table>
    </section>
    <p class="empty"><small>Filter the global Analytics page with <code>?api_key_id=${keyId}</code> for per-(provider, model) breakdown.</small></p>
  `;
  document.getElementById('main').innerHTML = html;
}
