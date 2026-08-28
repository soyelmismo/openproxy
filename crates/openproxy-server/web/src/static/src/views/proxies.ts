// views/proxies.ts — Free proxies management view.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { icons } from "../lib/icons.js";
import {
  syncProxies,
  testProxy,
  testAllProxies,
  getProxyTestUrl,
  updateProxyTestUrl,
  deleteProxy,
  showAddCustomProxy,
  reloadProxies,
} from "../handlers/proxy-handlers.js";
import { showToast } from "../components/toast.js";
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
let proxyTestUrl = "https://cloudflare.com/cdn-cgi/trace";
let isSavingTestUrl = false;

interface ProxyQueryParams {
  limit: number;
  offset: number;
  search?: string;
  source?: string;
  status?: string;
  protocol?: string;
  [key: string]: string | number | undefined;
}

function fetchFilteredProxies(): void {
  const queryParams: ProxyQueryParams = {
    limit: 50,
    offset: (currentPage - 1) * 50,
  };
  if (filterSearch) queryParams.search = filterSearch;
  if (filterSource) queryParams.source = filterSource;
  if (filterStatus) queryParams.status = filterStatus;
  if (filterProtocol) queryParams.protocol = filterProtocol;

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

async function onCopyProxy(text: string, e: Event): Promise<void> {
  e.preventDefault();
  e.stopPropagation();
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      showToast("Proxy copied!", "success");
      return;
    }
    const el = document.createElement("textarea");
    el.value = text;
    document.body.appendChild(el);
    el.select();
    document.execCommand("copy");
    document.body.removeChild(el);
    showToast("Proxy copied!", "success");
  } catch (_err: unknown) {
    showToast("Failed to copy", "error");
  }
}

function renderProxyRow(p: FreeProxyRow): TemplateResult {
  const isAlive = p.status === "alive";
  const fullHost = p.port ? `${p.host}:${p.port}` : p.host;
  const statusLabel = isAlive ? "ALIVE" : (p.status === "dead" ? "DEAD" : p.status.toUpperCase());
  const statusClass = isAlive ? "alive on" : (p.status === "dead" ? "dead off" : "unknown");

  let latencyText = html`—`;
  const latencyNum = p.latency_ms ?? 0;
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

  const country = p.country_code || "XX";
  const lastVal = formatTimeAgo(p.last_validated);

  return html`
    <tr class="proxy-card-row ${isAlive ? 'alive' : 'dead'}">
      <!-- Desktop Table Cells -->
      <td class="col-proxy-host" data-label="Host"><strong>${p.host}</strong></td>
      <td class="col-proxy-port" data-label="Port"><code class="proxy-port-code">${p.port}</code></td>
      <td class="col-proxy-protocol" data-label="Type"><span class="chip chip-protocol">${p.type.toUpperCase()}</span></td>
      <td class="col-proxy-source" data-label="Source"><span class="badge badge-source">${p.source}</span></td>
      <td class="col-proxy-status" data-label="Status">
        <span class="status-pill ${statusClass}">
          <span class="status-dot"></span>
          ${statusLabel}
        </span>
      </td>
      <td class="col-proxy-country" data-label="Country"><code class="proxy-country-code">${country}</code></td>
      <td class="col-proxy-latency ${latencyClass}" data-label="Latency">${latencyText}</td>
      <td class="col-proxy-validated" data-label="Checked"><small class="proxy-time-text">${lastVal}</small></td>
      <td class="col-proxy-actions" data-label="Actions">
        <div class="proxy-actions-wrap">
          <button class="small" @click=${() => void testProxy(p.id)}>${icons.flask()} ${t("common.retry")}</button>
          <button class="small danger" @click=${() => void deleteProxy(p.id)}>${icons.trash()} ${t("common.delete")}</button>
        </div>
      </td>

      <!-- Mobile Card Structure -->
      <td class="mobile-proxy-card-cell">
        <!-- Línea 1: Host:Port + Protocolo + País + Status -->
        <div class="p-card-line-1">
          <div class="p-card-host-group">
            <span 
              class="p-card-host" 
              title="Click to copy host:port"
              @click=${(e: Event) => onCopyProxy(fullHost, e)}
            >
              ${fullHost}
            </span>
            <span class="p-card-chip">${p.type.toUpperCase()}</span>
            <span class="p-card-country">${country}</span>
          </div>
          <span class="p-card-status ${isAlive ? 'alive' : 'dead'}">
            ${statusLabel}
          </span>
        </div>

        <!-- Línea 2: Latencia + Fuente + Antigüedad + Botones -->
        <div class="p-card-line-2">
          <div class="p-card-metrics">
            <span class="p-card-latency ${latencyNum > 1000 ? 'high' : ''}">${p.latency_ms !== null && p.latency_ms !== undefined ? `${p.latency_ms} ms` : '—'}</span>
            <span>·</span>
            <span class="p-card-source" title="${p.source}">${p.source}</span>
            <span>·</span>
            <span class="p-card-time">${lastVal}</span>
          </div>
          <div class="p-card-actions">
            <button @click=${() => void testProxy(p.id)}>🔄 Retry</button>
            <button class="danger" @click=${() => void deleteProxy(p.id)} title="Delete">✕</button>
          </div>
        </div>
      </td>
    </tr>
  `;
}

function renderPageHeader(isSyncing: boolean, syncBtnLabel: string): TemplateResult {
  return html`
    <div class="page-header">
      <div>
        <h2>${t("proxies.title")}</h2>
        <p class="subtitle">${t("proxies.subtitle")}</p>
        <div style="margin-top: 0.5rem; display: flex; align-items: center; gap: 0.5rem;">
          <input 
            type="text" 
            .value=${proxyTestUrl} 
            @change=${async (e: Event) => {
              proxyTestUrl = (e.target as HTMLInputElement).value;
              isSavingTestUrl = true;
              requestUpdate();
              try {
                await updateProxyTestUrl(proxyTestUrl);
              } finally {
                isSavingTestUrl = false;
                requestUpdate();
              }
            }}
            placeholder="Proxy test URL..."
            style="width: 100%; max-width: 320px; min-width: 0; padding: 0.35rem 0.5rem; border-radius: var(--radius-sm); border: var(--border-w) var(--border-style) var(--color-border);"
          />
          ${isSavingTestUrl ? html`<span class="spinner" style="width: 14px; height: 14px;"></span>` : ""}
        </div>
      </div>
      <div class="actions">
        <button class="primary" ?disabled=${isSyncing} @click=${triggerSync}>
          ${isSyncing ? html`<span class="spinner"></span>` : icons.refresh()}
          ${syncBtnLabel}
        </button>
        <button class="secondary" @click=${() => void testAllProxies()}>
          ${icons.flask()} ${t("proxies.btn.test_all")}
        </button>
        <button class="secondary" @click=${showAddCustomProxy}>
          ${icons.plus()} ${t("proxies.btn.add")}
        </button>
      </div>
    </div>
  `;
}

function renderKpisDashboard(total: number, alive: number, dead: number, avgLatency: number | null): TemplateResult {
  return html`
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
  `;
}

function renderFilterBar(search: string, protocol: string, source: string, status: string, uniqueProtocols: string[], uniqueSources: string[]): TemplateResult {
  return html`
    <div class="filter-bar">
      <div class="filter-search">
        <input 
          type="text" 
          .value=${search}
          placeholder=${t("proxies.filter.search_placeholder")} 
          @input=${onSearchInput}
        />
      </div>
      <div class="filter-selects">
        <select @change=${onProtocolChange} .value=${protocol}>
          <option value="">${t("proxies.filter.all_protocols")}</option>
          ${uniqueProtocols.map((p: string) => html`<option value=${p}>${p.toUpperCase()}</option>`)}
        </select>
        <select @change=${onSourceChange} .value=${source}>
          <option value="">${t("proxies.filter.all_sources")}</option>
          ${uniqueSources.map((s: string) => html`<option value=${s}>${s}</option>`)}
        </select>
        <select @change=${onStatusChange} .value=${status}>
          <option value="">${t("proxies.filter.all_statuses")}</option>
          <option value="unknown">${t("proxies.status.unknown")}</option>
          <option value="alive">${t("proxies.status.alive")}</option>
          <option value="dead">${t("proxies.status.dead")}</option>
        </select>
      </div>
    </div>
  `;
}

function renderProxiesList(proxies: FreeProxyRow[], error: string | null, page: number, hasPrevPage: boolean, hasNextPage: boolean): TemplateResult {
  return html`
    ${error
      ? html`<div class="banner banner-error">${error}</div>`
      : proxies.length === 0
        ? html`<p class="empty">${t("common.empty")}</p>`
        : html`
          <div class="table-wrap">
            <table class="proxies-table responsive-card-table">
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
          </div>
          <div class="pagination" style="display: flex; justify-content: space-between; align-items: center; margin: 1.5rem 0;">
            <span>Page ${page}</span>
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

function renderProxies(): TemplateResult {
  const proxies = (state.proxies as FreeProxyRow[]) || [];
  const summary = state.proxySummary || {
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
    ${renderPageHeader(isSyncing, syncBtnLabel)}
    <!-- KPIs dashboard -->
    ${renderKpisDashboard(total, alive, dead, avgLatency)}
    <!-- Filter toolbar -->
    ${renderFilterBar(filterSearch, filterProtocol, filterSource, filterStatus, uniqueProtocols, uniqueSources)}
    <!-- Proxies list -->
    ${renderProxiesList(proxies, loadError, currentPage, hasPrevPage, hasNextPage)}
  `;
}

export async function mountProxies(): Promise<(() => void) | void> {
  loadError = null;
  currentPage = 1;
  proxyTestUrl = await getProxyTestUrl();
  return createView(
    renderProxies,
    async () => { fetchFilteredProxies(); },
    (msg) => { loadError = msg; },
  );
}
