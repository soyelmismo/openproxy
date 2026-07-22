// views/proxies.ts — Free proxies management view.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import {
  syncProxies,
  testProxy,
  testAllProxies,
  deleteProxy,
  showAddCustomProxy,
  reloadProxies,
} from "../handlers/proxy-handlers.js";
import { t } from "../i18n/index.js";

interface FreeProxyRow {
  id: string;
  source: string;
  host: string;
  port: number;
  type: string;
  country_code: string | null;
  status: string;
  latency_ms: number | null;
  last_validated: string | null;
  created_at: string;
  updated_at: string;
}

// Module-local filters state
let filterSearch = "";
let filterSource = "";
let filterStatus = "";
let filterProtocol = "";
let isSyncing = false;
let loadError: string | null = null;
let currentPage = 1;
let searchDebounceTimer: ReturnType<typeof setTimeout> | null = null;

// ...
function fetchFilteredProxies(): void {
  const queryParams: Record<string, string | number> = {
    limit: 50,
    offset: (currentPage - 1) * 50,
  };
  if (filterSearch) queryParams["search"] = filterSearch;
  if (filterSource) queryParams["source"] = filterSource;
  if (filterStatus) queryParams["status"] = filterStatus;
  if (filterProtocol) queryParams["protocol"] = filterProtocol;

  void reloadProxies(queryParams);
}

function onSearchInput(e: Event): void {
  const target = e.target as HTMLInputElement;
  filterSearch = target.value.trim();
  currentPage = 1;
  if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
  searchDebounceTimer = setTimeout(() => {
    fetchFilteredProxies();
  }, 300);
}

function onSourceChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterSource = target.value;
  currentPage = 1;
  fetchFilteredProxies();
}

function onStatusChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterStatus = target.value;
  currentPage = 1;
  fetchFilteredProxies();
}

function onProtocolChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterProtocol = target.value;
  currentPage = 1;
  fetchFilteredProxies();
}

async function triggerSync(): Promise<void> {
  isSyncing = true;
  requestUpdate();
  try {
    await syncProxies();
  } finally {
    isSyncing = false;
    fetchFilteredProxies();
  }
}

function formatTimeAgo(isoString: string | null): string {
  if (!isoString) return t("proxies.table.never_validated");
  const date = new Date(isoString);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  if (diffMs < 0) return "Just now";
  const diffSecs = Math.floor(diffMs / 1000);
  if (diffSecs < 60) return `${diffSecs}s ago`;
  const diffMins = Math.floor(diffSecs / 60);
  if (diffMins < 60) return `${diffMins}m ago`;
  const diffHours = Math.floor(diffMins / 60);
  if (diffHours < 24) return `${diffHours}h ago`;
  const diffDays = Math.floor(diffHours / 24);
  return `${diffDays}d ago`;
}

function renderProxyRow(p: FreeProxyRow): TemplateResult {
  let statusClass = "unknown";
  let statusLabel = t("proxies.status.unknown");

  if (p.status === "alive") {
    statusClass = "on";
    statusLabel = t("proxies.status.alive");
  } else if (p.status === "dead") {
    statusClass = "off";
    statusLabel = t("proxies.status.dead");
  }

  let latencyText = html`—`;
  let latencyClass = "";
  if (p.latency_ms !== null && p.latency_ms !== undefined) {
    latencyText = html`${p.latency_ms} ms`;
    if (p.latency_ms < 300) {
      latencyClass = "latency-low"; // green
    } else if (p.latency_ms < 800) {
      latencyClass = "latency-medium"; // amber
    } else {
      latencyClass = "latency-high"; // red
    }
  }

  const country = p.country_code || "—";
  const lastVal = formatTimeAgo(p.last_validated);

  return html`
    <tr>
      <td><strong>${p.host}</strong></td>
      <td><code>${p.port}</code></td>
      <td><span class="chip chip-protocol">${p.type.toUpperCase()}</span></td>
      <td><span class="badge badge-source">${p.source}</span></td>
      <td>
        <span class="status-pill ${statusClass}">
          <span class="status-dot"></span>
          ${statusLabel}
        </span>
      </td>
      <td><code>${country}</code></td>
      <td class=${latencyClass}>${latencyText}</td>
      <td><small>${lastVal}</small></td>
      <td>
        <button class="small" @click=${() => void testProxy(p.id)}>${t("common.retry")}</button>
        <button class="small danger" @click=${() => void deleteProxy(p.id)}>${t("common.delete")}</button>
      </td>
    </tr>
  `;
}

function renderProxies(): TemplateResult {
  const proxies = (state.proxies as FreeProxyRow[]) || [];
  const summary = (state as any).proxySummary || {
    total: 0,
    alive: 0,
    dead: 0,
    unknown: 0,
    avg_latency_ms: null,
    sources: [],
    protocols: [],
  };

  const total = summary.total;
  const alive = summary.alive;
  const dead = summary.dead;
  const avgLatency = summary.avg_latency_ms;
  const uniqueSources = summary.sources || [];
  const uniqueProtocols = summary.protocols || [];

  const syncBtnLabel = isSyncing ? "Syncing..." : t("proxies.btn.sync");
  const hasPrevPage = currentPage > 1;
  const hasNextPage = proxies.length === 50;

  return html`
    <div class="page-header">
      <div>
        <h2>${t("proxies.title")}</h2>
        <p class="subtitle">${t("proxies.subtitle")}</p>
      </div>
      <div class="actions">
        <button class="primary" ?disabled=${isSyncing} @click=${triggerSync}>
          ${isSyncing ? html`<span class="spinner"></span>` : html``}
          ${syncBtnLabel}
        </button>
        <button class="secondary" @click=${() => void testAllProxies()}>
          ${t("proxies.btn.test_all")}
        </button>
        <button class="secondary" @click=${showAddCustomProxy}>
          + ${t("proxies.btn.add")}
        </button>
      </div>
    </div>

    <!-- KPIs dashboard -->
    <div class="kpi-grid">
      <div class="kpi-card">
        <div class="kpi-title">${t("proxies.kpi.total")}</div>
        <div class="kpi-value">${total}</div>
      </div>
      <div class="kpi-card kpi-success">
        <div class="kpi-title">${t("proxies.kpi.alive")}</div>
        <div class="kpi-value glow-green">${alive}</div>
      </div>
      <div class="kpi-card kpi-error">
        <div class="kpi-title">${t("proxies.kpi.dead")}</div>
        <div class="kpi-value">${dead}</div>
      </div>
      <div class="kpi-card kpi-latency">
        <div class="kpi-title">${t("proxies.kpi.avg_latency")}</div>
        <div class="kpi-value">${avgLatency !== null ? html`${avgLatency} <small>ms</small>` : "—"}</div>
      </div>
    </div>

    <!-- Filter toolbar -->
    <div class="filter-bar">
      <div class="filter-search">
        <input 
          type="text" 
          .value=${filterSearch} 
          placeholder=${t("proxies.filter.search_placeholder")} 
          @input=${onSearchInput}
        />
      </div>
      <div class="filter-selects">
        <select @change=${onProtocolChange} .value=${filterProtocol}>
          <option value="">${t("proxies.filter.all_protocols")}</option>
          ${uniqueProtocols.map((p: string) => html`<option value=${p}>${p.toUpperCase()}</option>`)}
        </select>
        <select @change=${onSourceChange} .value=${filterSource}>
          <option value="">${t("proxies.filter.all_sources")}</option>
          ${uniqueSources.map((s: string) => html`<option value=${s}>${s}</option>`)}
        </select>
        <select @change=${onStatusChange} .value=${filterStatus}>
          <option value="">${t("proxies.filter.all_statuses")}</option>
          <option value="unknown">${t("proxies.status.unknown")}</option>
          <option value="alive">${t("proxies.status.alive")}</option>
          <option value="dead">${t("proxies.status.dead")}</option>
        </select>
      </div>
    </div>

    <!-- Proxies list -->
    ${loadError
      ? html`<div class="banner banner-error">${loadError}</div>`
      : proxies.length === 0
        ? html`<p class="empty">${t("common.empty")}</p>`
        : html`
          <table>
            <thead>
              <tr>
                <th>${t("proxies.table.col_host")}</th>
                <th>${t("proxies.table.col_port")}</th>
                <th>${t("proxies.table.col_type")}</th>
                <th>${t("proxies.table.col_source")}</th>
                <th>${t("proxies.table.col_status")}</th>
                <th>${t("proxies.table.col_country")}</th>
                <th>${t("proxies.table.col_latency")}</th>
                <th>${t("proxies.table.col_last_val")}</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              ${proxies.map(renderProxyRow)}
            </tbody>
          </table>
          <div style="display: flex; justify-content: space-between; align-items: center; margin: 1.5rem 0;">
            <span>Page ${currentPage}</span>
            <div style="display: flex; gap: 0.5rem;">
              <button class="secondary small" ?disabled=${!hasPrevPage} @click=${() => { if (hasPrevPage) { currentPage--; fetchFilteredProxies(); } }}>
                ← Previous
              </button>
              <button class="secondary small" ?disabled=${!hasNextPage} @click=${() => { if (hasNextPage) { currentPage++; fetchFilteredProxies(); } }}>
                Next →
              </button>
            </div>
          </div>
        `
    }
  `;
}

export async function mountProxies(): Promise<(() => void) | void> {
  loadError = null;
  currentPage = 1;
  return createView(
    renderProxies,
    async () => { fetchFilteredProxies(); },
    (msg) => { loadError = msg; },
  );
}
