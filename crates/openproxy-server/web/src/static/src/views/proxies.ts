// views/proxies.ts — Free proxies management view.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { mountView, requestUpdate } from "../state/reactive.js";
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
let loadError: string | null = null;
let isSyncing = false;

function onSearchInput(e: Event): void {
  const target = e.target as HTMLInputElement;
  filterSearch = target.value.trim().toLowerCase();
  requestUpdate();
}

function onSourceChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterSource = target.value;
  requestUpdate();
}

function onStatusChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterStatus = target.value;
  requestUpdate();
}

function onProtocolChange(e: Event): void {
  const target = e.target as HTMLSelectElement;
  filterProtocol = target.value;
  requestUpdate();
}

async function triggerSync(): Promise<void> {
  isSyncing = true;
  requestUpdate();
  try {
    await syncProxies();
  } finally {
    isSyncing = false;
    requestUpdate();
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
  
  // Calculate stats
  const total = proxies.length;
  const aliveProxies = proxies.filter(p => p.status === "alive");
  const alive = aliveProxies.length;
  const dead = proxies.filter(p => p.status === "dead").length;
  const avgLatency = alive > 0 
    ? Math.round(aliveProxies.reduce((sum, p) => sum + (p.latency_ms || 0), 0) / alive) 
    : 0;

  // Apply filters
  const filtered = proxies.filter(p => {
    if (filterSearch && !p.host.toLowerCase().includes(filterSearch)) return false;
    if (filterSource && p.source !== filterSource) return false;
    if (filterStatus && p.status !== filterStatus) return false;
    if (filterProtocol && p.type !== filterProtocol) return false;
    return true;
  });

  const syncBtnLabel = isSyncing ? "Syncing..." : t("proxies.btn.sync");

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
        <div class="kpi-value">${alive > 0 ? html`${avgLatency} <small>ms</small>` : "—"}</div>
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
          <option value="http">HTTP</option>
          <option value="https">HTTPS</option>
          <option value="socks4">SOCKS4</option>
          <option value="socks5">SOCKS5</option>
        </select>
        <select @change=${onSourceChange} .value=${filterSource}>
          <option value="">${t("proxies.filter.all_sources")}</option>
          <option value="proxifly">proxifly</option>
          <option value="iplocate">iplocate</option>
          <option value="1proxy">1proxy</option>
          <option value="custom">custom</option>
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
      : filtered.length === 0
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
              ${filtered.map(renderProxyRow)}
            </tbody>
          </table>
        `
    }
  `;
}

export async function mountProxies(): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;
  loadError = null;
  const cleanup = mountView(main, renderProxies);
  try {
    await reloadProxies();
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e);
    requestUpdate();
  }
  return cleanup;
}
